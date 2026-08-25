use std::{
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
    path::{Component, Path, PathBuf},
    str::FromStr,
    sync::{Arc, OnceLock},
};

use dashmap::DashMap;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Pool, Sqlite, Transaction};
use url::Url;
use uuid::Uuid;
use zip::ZipArchive;

use super::workflow::{WorkflowSpecificationError, verify_workflow_profile};

pub const MAX_UPLOAD_BYTES: usize = 5 * 1024 * 1024;

static SKILL_PACKAGE_LOCKS: OnceLock<DashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowMaterialKind {
    ExternalUrl,
    UploadedFile,
    MarkdownFile,
}

impl WorkflowMaterialKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::ExternalUrl => "external_url",
            Self::UploadedFile => "uploaded_file",
            Self::MarkdownFile => "markdown_file",
        }
    }
}

impl FromStr for WorkflowMaterialKind {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "external_url" => Ok(Self::ExternalUrl),
            "uploaded_file" => Ok(Self::UploadedFile),
            "markdown_file" => Ok(Self::MarkdownFile),
            _ => Err("invalid workflow material kind"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
pub struct WorkflowMaterial {
    pub material_id: String,
    pub profile_id: String,
    pub material_revision: i64,
    pub ordinal: i64,
    pub title: String,
    pub kind: WorkflowMaterialKind,
    pub external_url: Option<String>,
    pub relative_path: Option<String>,
    pub original_filename: Option<String>,
    pub file_size: Option<i64>,
    pub checksum: Option<String>,
    pub markdown_content: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub reference_step_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
pub struct WorkflowMaterialsView {
    pub profile_id: String,
    pub skill_name: String,
    pub materials_revision: i64,
    pub materials: Vec<WorkflowMaterial>,
    pub step_material_ids: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
pub struct WorkflowMaterialSaveCommand {
    pub profile_id: String,
    pub material_id: Option<String>,
    pub expected_material_revision: Option<i64>,
    pub expected_materials_revision: i64,
    pub title: String,
    pub kind: WorkflowMaterialKind,
    pub external_url: Option<String>,
    pub markdown_content: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
pub struct WorkflowStepMaterialsSaveCommand {
    pub profile_id: String,
    pub step_id: String,
    pub material_ids: Vec<String>,
    pub expected_materials_revision: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
pub struct WorkflowMaterialsReorderCommand {
    pub profile_id: String,
    pub material_ids: Vec<String>,
    pub expected_materials_revision: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkflowMaterialsError {
    #[error(transparent)]
    Workflow(#[from] WorkflowSpecificationError),
    #[error("invalid workflow material request: {0}")]
    InvalidRequest(String),
    #[error("workflow material was changed by another author")]
    MaterialChanged { current_material_revision: i64 },
    #[error("workflow Materials library was changed by another author")]
    MaterialsChanged { current_materials_revision: i64 },
    #[error("workflow material was not found")]
    MaterialNotFound,
    #[error("workflow material database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("workflow material file operation failed")]
    File(#[from] std::io::Error),
    #[error("workflow Skill directory recovery failed after {failed_operation}: {recovery_error}")]
    SkillDirectoryRecovery {
        failed_operation: String,
        recovery_error: String,
    },
}

#[derive(Clone)]
pub struct WorkflowMaterialsService {
    pool: Pool<Sqlite>,
    skills_root: PathBuf,
}

struct StagedMaterialFile {
    original: PathBuf,
    staged: PathBuf,
}

pub(crate) enum SkillDirectoryRename {
    Unchanged,
    Moved {
        source: PathBuf,
        destination: PathBuf,
        skill_file_created: bool,
    },
    Created {
        destination: PathBuf,
    },
}

pub(crate) struct StagedSkillDefinition {
    destination: PathBuf,
    staged_previous: Option<PathBuf>,
}

#[doc(hidden)]
pub struct StagedPackageFile {
    destination: PathBuf,
    staged_previous: Option<PathBuf>,
}

#[doc(hidden)]
pub struct PackageFileDeletionLease {
    destination: PathBuf,
    leased: Option<PathBuf>,
}

impl PackageFileDeletionLease {
    pub(crate) async fn move_to_trash(&mut self) -> Result<(), WorkflowMaterialsError> {
        let Some(path) = self.leased.as_ref() else {
            return Ok(());
        };
        move_managed_path_to_trash(path.clone()).await?;
        self.leased = None;
        Ok(())
    }

    pub(crate) async fn rollback(mut self) -> Result<(), WorkflowMaterialsError> {
        let Some(path) = self.leased.take() else {
            return Ok(());
        };
        match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(WorkflowMaterialsError::InvalidRequest(
                    "package-file deletion lease source is unsafe".to_string(),
                ));
            }
            Ok(_) => {}
            Err(error) => return Err(error.into()),
        }
        match tokio::fs::symlink_metadata(&self.destination).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(WorkflowMaterialsError::InvalidRequest(
                    "package-file deletion lease destination is occupied".to_string(),
                ));
            }
            Err(error) => return Err(error.into()),
        }
        tokio::fs::rename(path, self.destination).await?;
        Ok(())
    }
}

impl StagedPackageFile {
    pub(crate) async fn commit(self) {
        if let Some(path) = self.staged_previous {
            if let Err(error) = tokio::fs::remove_file(path).await {
                tracing::warn!(%error, "Failed to remove committed Workflow package-file backup");
            }
        }
    }

    pub(crate) async fn rollback(self) -> Result<(), WorkflowMaterialsError> {
        match tokio::fs::symlink_metadata(&self.destination).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(WorkflowMaterialsError::InvalidRequest(
                    "package-file path is unsafe during rollback".to_string(),
                ));
            }
            Ok(_) => tokio::fs::remove_file(&self.destination).await?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(path) = self.staged_previous {
            tokio::fs::rename(path, self.destination).await?;
        }
        Ok(())
    }
}

impl StagedSkillDefinition {
    pub(crate) async fn commit(self) {
        if let Some(path) = self.staged_previous {
            if let Err(error) = tokio::fs::remove_file(path).await {
                tracing::warn!(%error, "Failed to remove committed Workflow Skill definition backup");
            }
        }
    }

    pub(crate) async fn rollback(self) -> Result<(), WorkflowMaterialsError> {
        match tokio::fs::symlink_metadata(&self.destination).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(WorkflowMaterialsError::InvalidRequest(
                    "SKILL.md path is unsafe during rollback".to_string(),
                ));
            }
            Ok(_) => tokio::fs::remove_file(&self.destination).await?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(path) = self.staged_previous {
            tokio::fs::rename(path, self.destination).await?;
        }
        Ok(())
    }
}

impl SkillDirectoryRename {
    pub(crate) async fn rollback(self) -> Result<(), WorkflowMaterialsError> {
        match self {
            Self::Unchanged => {}
            Self::Moved {
                source,
                destination,
                skill_file_created,
            } => {
                if skill_file_created {
                    remove_skill_definition(&destination).await?;
                } else {
                    let source_name = source.file_name().and_then(|name| name.to_str()).ok_or_else(|| {
                        WorkflowMaterialsError::InvalidRequest("Skill directory name is invalid".to_string())
                    })?;
                    rewrite_skill_definition_name(&destination, source_name).await?;
                }
                tokio::fs::rename(destination, source).await?;
            }
            Self::Created { destination } => {
                let skill_file = destination.join("SKILL.md");
                let references = destination.join("references");
                let scripts = destination.join("scripts");
                let assets = destination.join("assets");
                tokio::fs::remove_file(skill_file).await?;
                tokio::fs::remove_dir(references).await?;
                tokio::fs::remove_dir(scripts).await?;
                tokio::fs::remove_dir(assets).await?;
                tokio::fs::remove_dir(destination).await?;
            }
        }
        Ok(())
    }
}

pub(crate) async fn rollback_skill_directory_rename(
    rename: SkillDirectoryRename,
    failed_operation: String,
) -> Result<(), WorkflowMaterialsError> {
    rename
        .rollback()
        .await
        .map_err(|recovery_error| WorkflowMaterialsError::SkillDirectoryRecovery {
            failed_operation,
            recovery_error: recovery_error.to_string(),
        })
}

impl StagedMaterialFile {
    async fn restore(self) -> Result<(), WorkflowMaterialsError> {
        tokio::fs::rename(self.staged, self.original).await?;
        Ok(())
    }

    async fn move_to_trash(self) -> Result<(), WorkflowMaterialsError> {
        move_managed_path_to_trash(self.staged).await
    }
}

pub(crate) async fn move_managed_path_to_trash(path: PathBuf) -> Result<(), WorkflowMaterialsError> {
    tokio::task::spawn_blocking(move || {
        #[cfg(target_os = "macos")]
        {
            use trash::macos::{DeleteMethod, TrashContextExtMacos};

            let mut context = trash::TrashContext::new();
            context.set_delete_method(DeleteMethod::NsFileManager);
            context.delete(&path)
        }
        #[cfg(not(target_os = "macos"))]
        {
            trash::delete(&path)
        }
    })
    .await
    .map_err(|error| WorkflowMaterialsError::InvalidRequest(format!("failed to move managed path to trash: {error}")))?
    .map_err(|error| WorkflowMaterialsError::InvalidRequest(format!("failed to move managed path to trash: {error}")))
}

async fn trash_staged_file(
    staged_file: Option<StagedMaterialFile>,
    profile_id: &str,
    material_id: &str,
) {
    if let Some(staged_file) = staged_file {
        if let Err(error) = staged_file.move_to_trash().await {
            tracing::warn!(
                %error,
                %profile_id,
                %material_id,
                "Failed to move committed workflow material file to trash"
            );
        }
    }
}

async fn restore_staged_file(staged_file: Option<StagedMaterialFile>) -> Result<(), WorkflowMaterialsError> {
    if let Some(staged_file) = staged_file {
        staged_file.restore().await?;
    }
    Ok(())
}

impl WorkflowMaterialsService {
    pub fn new(
        pool: Pool<Sqlite>,
        skills_root: PathBuf,
    ) -> Self {
        Self { pool, skills_root }
    }

    pub fn validate_configured_skill_name(skill_name: &str) -> Result<(), WorkflowMaterialsError> {
        validate_skill_name(skill_name)
    }

    #[doc(hidden)]
    pub async fn lock_skill_package(
        &self,
        skill_name: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, WorkflowMaterialsError> {
        validate_skill_name(skill_name)?;
        let key = self.skills_root.join(skill_name);
        let lock = SKILL_PACKAGE_LOCKS
            .get_or_init(DashMap::new)
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        Ok(lock.lock_owned().await)
    }

    pub async fn trash_managed_skill_directory(
        skills_root: PathBuf,
        skill_name: String,
    ) -> Result<(), WorkflowMaterialsError> {
        validate_skill_name(&skill_name)?;
        ensure_existing_directory_without_symlink(&skills_root).await?;
        let directory = skills_root.join(skill_name);
        ensure_existing_directory_without_symlink(&directory).await?;
        move_managed_path_to_trash(directory).await
    }

    pub async fn view(
        &self,
        profile_id: &str,
    ) -> Result<WorkflowMaterialsView, WorkflowMaterialsError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        verify_workflow_profile(&mut transaction, profile_id).await?;
        ensure_skill_name(&mut transaction, profile_id).await?;
        ensure_material_library(&mut transaction, profile_id).await?;
        transaction.commit().await?;

        let mut read_transaction = self.pool.begin().await?;
        let skill_name: String =
            sqlx::query_scalar("SELECT skill_name FROM workflow_profile_skills WHERE profile_id = ?")
                .bind(profile_id)
                .fetch_one(&mut *read_transaction)
                .await?;
        self.ensure_skill_directory(&skill_name).await?;
        let view = self.load_view_in_transaction(&mut read_transaction, profile_id).await?;
        read_transaction.commit().await?;
        Ok(view)
    }

    pub async fn set_skill_name(
        &self,
        profile_id: &str,
        requested_skill_name: &str,
    ) -> Result<(), WorkflowMaterialsError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rename = self
            .set_skill_name_in_transaction(&mut transaction, profile_id, requested_skill_name)
            .await?;
        if let Err(error) = transaction.commit().await {
            rollback_skill_directory_rename(rename, error.to_string()).await?;
            return Err(error.into());
        }
        Ok(())
    }

    pub(crate) async fn set_skill_name_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        profile_id: &str,
        requested_skill_name: &str,
    ) -> Result<SkillDirectoryRename, WorkflowMaterialsError> {
        validate_skill_name(requested_skill_name)?;
        ensure_directory_without_symlink(&self.skills_root).await?;
        verify_workflow_profile(transaction, profile_id).await?;
        let existing_skill_name: Option<String> =
            sqlx::query_scalar("SELECT skill_name FROM workflow_profile_skills WHERE profile_id = ?")
                .bind(profile_id)
                .fetch_optional(&mut **transaction)
                .await?;
        let is_taken: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workflow_profile_skills WHERE skill_name = ? AND profile_id != ?)",
        )
        .bind(requested_skill_name)
        .bind(profile_id)
        .fetch_one(&mut **transaction)
        .await?;
        if is_taken {
            return Err(WorkflowMaterialsError::InvalidRequest(
                "Skill name is already used by another Workflow Profile".to_string(),
            ));
        }

        if existing_skill_name.as_deref() == Some(requested_skill_name) {
            self.ensure_skill_directory(requested_skill_name).await?;
            return Ok(SkillDirectoryRename::Unchanged);
        }

        let rename = self
            .rename_skill_directory(existing_skill_name.as_deref(), requested_skill_name)
            .await?;
        let update_result = sqlx::query(
            "INSERT INTO workflow_profile_skills (profile_id, skill_name) VALUES (?, ?) \
             ON CONFLICT(profile_id) DO UPDATE SET skill_name = excluded.skill_name",
        )
        .bind(profile_id)
        .bind(requested_skill_name)
        .execute(&mut **transaction)
        .await;
        if let Err(error) = update_result {
            rollback_skill_directory_rename(rename, error.to_string()).await?;
            return Err(error.into());
        }
        Ok(rename)
    }

    pub async fn save(
        &self,
        command: WorkflowMaterialSaveCommand,
    ) -> Result<WorkflowMaterial, WorkflowMaterialsError> {
        validate_save_command(&command)?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        verify_workflow_profile(&mut transaction, &command.profile_id).await?;
        let skill_name = ensure_skill_name(&mut transaction, &command.profile_id).await?;
        ensure_material_library(&mut transaction, &command.profile_id).await?;
        verify_materials_revision(
            &mut transaction,
            &command.profile_id,
            command.expected_materials_revision,
        )
        .await?;
        let material_id = command
            .material_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let previous_relative_path = if command.material_id.is_some() {
            load_material_row(&mut transaction, &material_id)
                .await?
                .and_then(|material| material.relative_path)
        } else {
            None
        };
        let material = match command.kind {
            WorkflowMaterialKind::ExternalUrl => save_external_url(&mut transaction, &command, &material_id).await?,
            WorkflowMaterialKind::MarkdownFile => save_markdown(&mut transaction, &command, &material_id).await?,
            WorkflowMaterialKind::UploadedFile => {
                save_uploaded_file_metadata(&mut transaction, &command, &material_id).await?
            }
        };
        self.ensure_skill_directory(&skill_name).await?;
        let replaces_previous_file = previous_relative_path.as_deref() != material.relative_path.as_deref();
        let staged_previous = if material.kind == WorkflowMaterialKind::MarkdownFile || replaces_previous_file {
            if let Some(path) = previous_relative_path.as_deref() {
                self.stage_material_file(&skill_name, path).await?
            } else {
                None
            }
        } else {
            None
        };
        if material.kind == WorkflowMaterialKind::MarkdownFile {
            if let Err(error) = self
                .write_material_file(
                    &skill_name,
                    material.relative_path.as_deref().expect("markdown path"),
                    material.markdown_content.as_deref().unwrap_or_default().as_bytes(),
                )
                .await
            {
                if let Some(staged_file) = staged_previous {
                    staged_file.restore().await?;
                }
                return Err(error);
            }
        }
        if let Err(error) = bump_materials_revision(&mut transaction, &command.profile_id).await {
            if material.kind == WorkflowMaterialKind::MarkdownFile {
                self.rollback_written_file(
                    &skill_name,
                    material.relative_path.as_deref().expect("markdown path"),
                    staged_previous,
                )
                .await?;
            } else {
                restore_staged_file(staged_previous).await?;
            }
            return Err(error);
        }
        if let Err(error) = transaction.commit().await {
            if material.kind == WorkflowMaterialKind::MarkdownFile {
                self.rollback_written_file(
                    &skill_name,
                    material.relative_path.as_deref().expect("markdown path"),
                    staged_previous,
                )
                .await?;
            } else {
                restore_staged_file(staged_previous).await?;
            }
            return Err(error.into());
        }
        trash_staged_file(staged_previous, &command.profile_id, &material_id).await;
        self.load_material(&material_id).await
    }

    pub async fn upload(
        &self,
        profile_id: &str,
        title: String,
        original_filename: String,
        bytes: Vec<u8>,
        replace_material_id: Option<&str>,
        expected_material_revision: Option<i64>,
        expected_materials_revision: i64,
    ) -> Result<WorkflowMaterial, WorkflowMaterialsError> {
        validate_uploaded_file(&title, &original_filename, &bytes)?;
        let extension = allowed_extension(&original_filename).expect("validated extension");
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        verify_workflow_profile(&mut transaction, profile_id).await?;
        let skill_name = ensure_skill_name(&mut transaction, profile_id).await?;
        ensure_material_library(&mut transaction, profile_id).await?;
        verify_materials_revision(&mut transaction, profile_id, expected_materials_revision).await?;
        let material_id = replace_material_id
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let checksum = checksum(&bytes);
        let previous_relative_path = if replace_material_id.is_some() {
            let current = load_material_row(&mut transaction, &material_id)
                .await?
                .ok_or(WorkflowMaterialsError::MaterialNotFound)?;
            if current.profile_id != profile_id || current.material_revision != expected_material_revision.unwrap_or(-1)
            {
                return Err(WorkflowMaterialsError::MaterialChanged {
                    current_material_revision: current.material_revision,
                });
            }
            if WorkflowMaterialKind::from_str(&current.kind).ok() == Some(WorkflowMaterialKind::ExternalUrl) {
                return Err(WorkflowMaterialsError::InvalidRequest(
                    "external URLs cannot be replaced as files".to_string(),
                ));
            }
            current.relative_path
        } else {
            None
        };
        let relative_path = if replace_material_id.is_some() {
            resolve_replace_material_path(
                &mut transaction,
                profile_id,
                &material_id,
                &extension,
                previous_relative_path.as_deref(),
                &original_filename,
                &title,
            )
            .await?
        } else {
            allocate_created_material_path(
                &mut transaction,
                profile_id,
                &material_id,
                &extension,
                filename_stem(&original_filename).as_deref(),
                Some(title.as_str()),
            )
            .await?
        };
        if replace_material_id.is_some() {
            sqlx::query("UPDATE workflow_profile_materials SET material_revision = material_revision + 1, title = ?, kind = 'uploaded_file', external_url = NULL, relative_path = ?, original_filename = ?, file_size = ?, checksum = ?, markdown_content = NULL, updated_at = CURRENT_TIMESTAMP WHERE material_id = ?")
                .bind(&title).bind(&relative_path).bind(&original_filename).bind(bytes.len() as i64).bind(&checksum).bind(&material_id)
                .execute(&mut *transaction).await?;
        } else {
            let ordinal = next_material_ordinal(&mut transaction, profile_id).await?;
            sqlx::query("INSERT INTO workflow_profile_materials (material_id, profile_id, ordinal, title, kind, relative_path, original_filename, file_size, checksum) VALUES (?, ?, ?, ?, 'uploaded_file', ?, ?, ?, ?)")
                .bind(&material_id).bind(profile_id).bind(ordinal).bind(&title).bind(&relative_path).bind(&original_filename).bind(bytes.len() as i64).bind(&checksum)
                .execute(&mut *transaction).await?;
        };
        self.ensure_skill_directory(&skill_name).await?;
        let staged_previous = if let Some(path) = previous_relative_path.as_deref() {
            self.stage_material_file(&skill_name, path).await?
        } else {
            None
        };
        if let Err(error) = self.write_material_file(&skill_name, &relative_path, &bytes).await {
            if let Some(staged_file) = staged_previous {
                staged_file.restore().await?;
            }
            return Err(error);
        }
        if let Err(error) = bump_materials_revision(&mut transaction, profile_id).await {
            self.rollback_written_file(&skill_name, &relative_path, staged_previous)
                .await?;
            return Err(error);
        }
        if let Err(error) = transaction.commit().await {
            self.rollback_written_file(&skill_name, &relative_path, staged_previous)
                .await?;
            return Err(error.into());
        }
        trash_staged_file(staged_previous, profile_id, &material_id).await;
        self.load_material(&material_id).await
    }

    pub async fn delete(
        &self,
        profile_id: &str,
        material_id: &str,
        expected_material_revision: i64,
        expected_materials_revision: i64,
    ) -> Result<(), WorkflowMaterialsError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        verify_workflow_profile(&mut transaction, profile_id).await?;
        ensure_material_library(&mut transaction, profile_id).await?;
        verify_materials_revision(&mut transaction, profile_id, expected_materials_revision).await?;
        let current = load_material_row(&mut transaction, material_id)
            .await?
            .ok_or(WorkflowMaterialsError::MaterialNotFound)?;
        if current.profile_id != profile_id || current.material_revision != expected_material_revision {
            return Err(WorkflowMaterialsError::MaterialChanged {
                current_material_revision: current.material_revision,
            });
        }
        let skill_name = ensure_skill_name(&mut transaction, profile_id).await?;
        sqlx::query("DELETE FROM workflow_profile_materials WHERE material_id = ?")
            .bind(material_id)
            .execute(&mut *transaction)
            .await?;
        bump_materials_revision(&mut transaction, profile_id).await?;
        let staged_file = if let Some(path) = current.relative_path.as_deref() {
            self.ensure_skill_directory(&skill_name).await?;
            self.stage_material_file(&skill_name, path).await?
        } else {
            None
        };
        if let Err(error) = transaction.commit().await {
            if let Some(staged_file) = staged_file {
                staged_file.restore().await?;
            }
            return Err(error.into());
        }
        trash_staged_file(staged_file, profile_id, material_id).await;
        Ok(())
    }

    pub async fn save_step_materials(
        &self,
        command: WorkflowStepMaterialsSaveCommand,
    ) -> Result<Vec<String>, WorkflowMaterialsError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        verify_workflow_profile(&mut transaction, &command.profile_id).await?;
        ensure_material_library(&mut transaction, &command.profile_id).await?;
        verify_materials_revision(
            &mut transaction,
            &command.profile_id,
            command.expected_materials_revision,
        )
        .await?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workflow_profile_steps WHERE profile_id = ? AND step_id = ?)",
        )
        .bind(&command.profile_id)
        .bind(&command.step_id)
        .fetch_one(&mut *transaction)
        .await?;
        if !exists {
            return Err(WorkflowMaterialsError::InvalidRequest(
                "workflow step does not exist".to_string(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for material_id in &command.material_ids {
            if !seen.insert(material_id) {
                return Err(WorkflowMaterialsError::InvalidRequest(
                    "workflow material IDs must be unique per step".to_string(),
                ));
            }
            let belongs: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM workflow_profile_materials WHERE material_id = ? AND profile_id = ?)",
            )
            .bind(material_id)
            .bind(&command.profile_id)
            .fetch_one(&mut *transaction)
            .await?;
            if !belongs {
                return Err(WorkflowMaterialsError::MaterialNotFound);
            }
        }
        sqlx::query("DELETE FROM workflow_profile_step_materials WHERE profile_id = ? AND step_id = ?")
            .bind(&command.profile_id)
            .bind(&command.step_id)
            .execute(&mut *transaction)
            .await?;
        for (ordinal, material_id) in command.material_ids.iter().enumerate() {
            sqlx::query("INSERT INTO workflow_profile_step_materials (profile_id, step_id, material_id, ordinal) VALUES (?, ?, ?, ?)")
                .bind(&command.profile_id).bind(&command.step_id).bind(material_id).bind(ordinal as i64).execute(&mut *transaction).await?;
        }
        bump_materials_revision(&mut transaction, &command.profile_id).await?;
        transaction.commit().await?;
        Ok(command.material_ids)
    }

    pub async fn reorder(
        &self,
        command: WorkflowMaterialsReorderCommand,
    ) -> Result<Vec<String>, WorkflowMaterialsError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        verify_workflow_profile(&mut transaction, &command.profile_id).await?;
        ensure_material_library(&mut transaction, &command.profile_id).await?;
        verify_materials_revision(
            &mut transaction,
            &command.profile_id,
            command.expected_materials_revision,
        )
        .await?;
        let material_ids: Vec<String> = sqlx::query_scalar(
            "SELECT material_id FROM workflow_profile_materials WHERE profile_id = ? ORDER BY ordinal",
        )
        .bind(&command.profile_id)
        .fetch_all(&mut *transaction)
        .await?;
        let expected_ids = material_ids.iter().collect::<std::collections::BTreeSet<_>>();
        let received_ids = command.material_ids.iter().collect::<std::collections::BTreeSet<_>>();
        if material_ids.len() != command.material_ids.len() || expected_ids != received_ids {
            return Err(WorkflowMaterialsError::InvalidRequest(
                "material ordering must contain every profile Material exactly once".to_string(),
            ));
        }
        let temporary_ordinal_offset: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM workflow_profile_materials WHERE profile_id = ?",
        )
        .bind(&command.profile_id)
        .fetch_one(&mut *transaction)
        .await?;
        sqlx::query("UPDATE workflow_profile_materials SET ordinal = ordinal + ? WHERE profile_id = ?")
            .bind(temporary_ordinal_offset)
            .bind(&command.profile_id)
            .execute(&mut *transaction)
            .await?;
        for (ordinal, material_id) in command.material_ids.iter().enumerate() {
            sqlx::query("UPDATE workflow_profile_materials SET ordinal = ?, updated_at = CURRENT_TIMESTAMP WHERE profile_id = ? AND material_id = ?")
                .bind(ordinal as i64)
                .bind(&command.profile_id)
                .bind(material_id)
                .execute(&mut *transaction)
                .await?;
        }
        bump_materials_revision(&mut transaction, &command.profile_id).await?;
        transaction.commit().await?;
        Ok(command.material_ids)
    }

    pub async fn read_preview(
        &self,
        profile_id: &str,
        material_id: &str,
    ) -> Result<String, WorkflowMaterialsError> {
        let material = self.load_material(material_id).await?;
        if material.profile_id != profile_id {
            return Err(WorkflowMaterialsError::MaterialNotFound);
        }
        if material.kind == WorkflowMaterialKind::ExternalUrl {
            return Err(WorkflowMaterialsError::InvalidRequest(
                "external URLs do not have local previews".to_string(),
            ));
        }
        if let Some(markdown) = material.markdown_content {
            return Ok(markdown);
        }
        let extension = material
            .relative_path
            .as_deref()
            .and_then(allowed_extension)
            .ok_or_else(|| WorkflowMaterialsError::InvalidRequest("material path is invalid".to_string()))?;
        if !is_text_extension(&extension) {
            return Err(WorkflowMaterialsError::InvalidRequest(
                "this file type does not support previews".to_string(),
            ));
        }
        let skill_name = self.skill_name(profile_id).await?;
        self.ensure_skill_directory(&skill_name).await?;
        let path = self.material_path(&skill_name, material.relative_path.as_deref().expect("file path"))?;
        ensure_regular_file_without_symlink(&path).await?;
        Ok(tokio::fs::read_to_string(path).await?)
    }

    pub async fn resolve_local_file(
        &self,
        profile_id: &str,
        material_id: &str,
    ) -> Result<PathBuf, WorkflowMaterialsError> {
        let material = self.load_material(material_id).await?;
        if material.profile_id != profile_id {
            return Err(WorkflowMaterialsError::MaterialNotFound);
        }
        if material.kind == WorkflowMaterialKind::ExternalUrl {
            return Err(WorkflowMaterialsError::InvalidRequest(
                "external URLs do not have local files".to_string(),
            ));
        }

        let relative_path = material
            .relative_path
            .as_deref()
            .ok_or_else(|| WorkflowMaterialsError::InvalidRequest("material path is invalid".to_string()))?;
        let skill_name = self.skill_name(profile_id).await?;
        let path = self.material_path(&skill_name, relative_path)?;
        ensure_existing_directory_without_symlink(&self.skills_root).await?;
        ensure_existing_directory_without_symlink(&self.skills_root.join(&skill_name)).await?;
        let parent = path
            .parent()
            .ok_or_else(|| WorkflowMaterialsError::InvalidRequest("material path is invalid".to_string()))?;
        ensure_existing_directory_without_symlink(parent).await?;
        ensure_regular_file_without_symlink(&path).await?;
        Ok(path)
    }

    async fn load_view_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        profile_id: &str,
    ) -> Result<WorkflowMaterialsView, WorkflowMaterialsError> {
        let skill_name: String =
            sqlx::query_scalar("SELECT skill_name FROM workflow_profile_skills WHERE profile_id = ?")
                .bind(profile_id)
                .fetch_one(&mut **transaction)
                .await?;
        let rows = sqlx::query_as::<_, MaterialRow>("SELECT material_id, profile_id, material_revision, ordinal, title, kind, external_url, relative_path, original_filename, file_size, checksum, markdown_content, created_at, updated_at FROM workflow_profile_materials WHERE profile_id = ? ORDER BY ordinal")
            .bind(profile_id).fetch_all(&mut **transaction).await?;
        let materials = self.hydrate_materials_in_transaction(transaction, rows).await?;
        let materials_revision: i64 = sqlx::query_scalar(
            "SELECT materials_revision FROM workflow_profile_material_libraries WHERE profile_id = ?",
        )
        .bind(profile_id)
        .fetch_one(&mut **transaction)
        .await?;
        let step_rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT step_id, material_id FROM workflow_profile_step_materials WHERE profile_id = ? ORDER BY step_id, ordinal",
        )
        .bind(profile_id)
        .fetch_all(&mut **transaction)
        .await?;
        let mut step_material_ids = BTreeMap::new();
        for (step_id, material_id) in step_rows {
            step_material_ids
                .entry(step_id)
                .or_insert_with(Vec::new)
                .push(material_id);
        }
        Ok(WorkflowMaterialsView {
            profile_id: profile_id.to_string(),
            skill_name,
            materials_revision,
            materials,
            step_material_ids,
        })
    }

    async fn load_material(
        &self,
        material_id: &str,
    ) -> Result<WorkflowMaterial, WorkflowMaterialsError> {
        let row = sqlx::query_as::<_, MaterialRow>("SELECT material_id, profile_id, material_revision, ordinal, title, kind, external_url, relative_path, original_filename, file_size, checksum, markdown_content, created_at, updated_at FROM workflow_profile_materials WHERE material_id = ?")
            .bind(material_id).fetch_optional(&self.pool).await?.ok_or(WorkflowMaterialsError::MaterialNotFound)?;
        Ok(self.hydrate_materials(vec![row]).await?.pop().expect("one material"))
    }

    async fn hydrate_materials(
        &self,
        rows: Vec<MaterialRow>,
    ) -> Result<Vec<WorkflowMaterial>, WorkflowMaterialsError> {
        let ids: Vec<String> = rows.iter().map(|row| row.material_id.clone()).collect();
        let refs: Vec<(String, String)> = if ids.is_empty() {
            Vec::new()
        } else {
            let mut query = sqlx::QueryBuilder::<Sqlite>::new(
                "SELECT material_id, step_id FROM workflow_profile_step_materials WHERE material_id IN (",
            );
            let mut separated = query.separated(", ");
            for id in &ids {
                separated.push_bind(id);
            }
            separated.push_unseparated(") ORDER BY ordinal");
            query.build_query_as().fetch_all(&self.pool).await?
        };
        let mut references = BTreeMap::<String, Vec<String>>::new();
        for (material_id, step_id) in refs {
            references.entry(material_id).or_default().push(step_id);
        }
        rows.into_iter()
            .map(|row| {
                let material_id = row.material_id.clone();
                row.into_material(references.remove(&material_id).unwrap_or_default())
            })
            .collect()
    }

    async fn hydrate_materials_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        rows: Vec<MaterialRow>,
    ) -> Result<Vec<WorkflowMaterial>, WorkflowMaterialsError> {
        let ids: Vec<String> = rows.iter().map(|row| row.material_id.clone()).collect();
        let refs: Vec<(String, String)> = if ids.is_empty() {
            Vec::new()
        } else {
            let mut query = sqlx::QueryBuilder::<Sqlite>::new(
                "SELECT material_id, step_id FROM workflow_profile_step_materials WHERE material_id IN (",
            );
            let mut separated = query.separated(", ");
            for id in &ids {
                separated.push_bind(id);
            }
            separated.push_unseparated(") ORDER BY ordinal");
            query.build_query_as().fetch_all(&mut **transaction).await?
        };
        let mut references = BTreeMap::<String, Vec<String>>::new();
        for (material_id, step_id) in refs {
            references.entry(material_id).or_default().push(step_id);
        }
        rows.into_iter()
            .map(|row| {
                let material_id = row.material_id.clone();
                row.into_material(references.remove(&material_id).unwrap_or_default())
            })
            .collect()
    }

    async fn skill_name(
        &self,
        profile_id: &str,
    ) -> Result<String, WorkflowMaterialsError> {
        sqlx::query_scalar("SELECT skill_name FROM workflow_profile_skills WHERE profile_id = ?")
            .bind(profile_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| {
                WorkflowMaterialsError::InvalidRequest("workflow skill has not been initialized".to_string())
            })
    }

    async fn ensure_skill_directory(
        &self,
        skill_name: &str,
    ) -> Result<(), WorkflowMaterialsError> {
        validate_skill_name(skill_name)?;
        let root = self.skills_root.join(skill_name);
        ensure_directory_without_symlink(&self.skills_root).await?;
        ensure_directory_without_symlink(&root).await?;
        let references = root.join("references");
        let scripts = root.join("scripts");
        let assets = root.join("assets");
        ensure_directory_without_symlink(&references).await?;
        ensure_directory_without_symlink(&scripts).await?;
        ensure_directory_without_symlink(&assets).await?;
        let skill_file = root.join("SKILL.md");
        match tokio::fs::symlink_metadata(&skill_file).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(WorkflowMaterialsError::InvalidRequest(
                    "SKILL.md path is unsafe".to_string(),
                ));
            }
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let scaffold = format!(
            "---\nname: {skill_name}\ndescription: Managed Workflow Profile material library.\n---\n\n# Materials\n"
        );
        write_atomic(&skill_file, scaffold.as_bytes()).await
    }

    pub(crate) async fn stage_skill_definition(
        &self,
        skill_name: &str,
        content: &str,
    ) -> Result<StagedSkillDefinition, WorkflowMaterialsError> {
        self.ensure_skill_directory(skill_name).await?;
        let destination = self.skills_root.join(skill_name).join("SKILL.md");
        let staged_previous = match tokio::fs::symlink_metadata(&destination).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(WorkflowMaterialsError::InvalidRequest(
                    "SKILL.md path is unsafe".to_string(),
                ));
            }
            Ok(_) => {
                let staged = destination.with_file_name(format!(".SKILL.md.{}.previous", Uuid::new_v4()));
                tokio::fs::rename(&destination, &staged).await?;
                Some(staged)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = write_atomic(&destination, content.as_bytes()).await {
            if let Some(path) = &staged_previous {
                tokio::fs::rename(path, &destination).await?;
            }
            return Err(error);
        }
        Ok(StagedSkillDefinition {
            destination,
            staged_previous,
        })
    }

    #[doc(hidden)]
    pub async fn stage_package_file_bytes(
        &self,
        skill_name: &str,
        relative_path: &str,
        content: &[u8],
    ) -> Result<StagedPackageFile, WorkflowMaterialsError> {
        self.ensure_skill_directory(skill_name).await?;
        let destination = self.material_path(skill_name, relative_path)?;
        let staged_previous = match tokio::fs::symlink_metadata(&destination).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(WorkflowMaterialsError::InvalidRequest(
                    "package-file path is unsafe".to_string(),
                ));
            }
            Ok(_) => {
                let name = destination
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("package-file");
                let staged = destination.with_file_name(format!(".{name}.{}.previous", Uuid::new_v4()));
                tokio::fs::rename(&destination, &staged).await?;
                Some(staged)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = write_atomic(&destination, content).await {
            if let Some(path) = &staged_previous {
                tokio::fs::rename(path, &destination).await?;
            }
            return Err(error);
        }
        Ok(StagedPackageFile {
            destination,
            staged_previous,
        })
    }

    #[doc(hidden)]
    pub async fn stage_package_file_deletion_lease(
        &self,
        skill_name: &str,
        relative_path: &str,
    ) -> Result<PackageFileDeletionLease, WorkflowMaterialsError> {
        self.ensure_skill_directory(skill_name).await?;
        let destination = self.material_path(skill_name, relative_path)?;
        let leased = match tokio::fs::symlink_metadata(&destination).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(WorkflowMaterialsError::InvalidRequest(
                    "package-file path is unsafe".to_string(),
                ));
            }
            Ok(_) => {
                let name = destination
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("package-file");
                let leased = destination.with_file_name(format!(".{name}.{}.deletion-lease", Uuid::new_v4()));
                tokio::fs::rename(&destination, &leased).await?;
                Some(leased)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        Ok(PackageFileDeletionLease { destination, leased })
    }

    pub(crate) async fn trash_unregistered_package_files(
        &self,
        skill_name: &str,
        registered_paths: &BTreeSet<String>,
    ) -> Result<(), WorkflowMaterialsError> {
        self.ensure_skill_directory(skill_name).await?;
        for directory_name in ["references", "scripts", "assets"] {
            let directory = self.skills_root.join(skill_name).join(directory_name);
            ensure_existing_directory_without_symlink(&directory).await?;
            let mut entries = tokio::fs::read_dir(&directory).await?;
            while let Some(entry) = entries.next_entry().await? {
                let metadata = entry.file_type().await?;
                if metadata.is_symlink() || !metadata.is_file() {
                    return Err(WorkflowMaterialsError::InvalidRequest(
                        "managed package directory contains an unsafe entry".to_string(),
                    ));
                }
                let file_name = entry.file_name().into_string().map_err(|_| {
                    WorkflowMaterialsError::InvalidRequest(
                        "managed package directory contains a non-UTF-8 file name".to_string(),
                    )
                })?;
                let relative_path = format!("{directory_name}/{file_name}");
                if let Some(original_name) = deletion_lease_original_name(&file_name) {
                    let original_relative_path = format!("{directory_name}/{original_name}");
                    if registered_paths.contains(&original_relative_path) {
                        let original_path = directory.join(original_name);
                        match tokio::fs::symlink_metadata(&original_path).await {
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                tokio::fs::rename(entry.path(), original_path).await?;
                            }
                            Ok(_) => move_managed_path_to_trash(entry.path()).await?,
                            Err(error) => return Err(error.into()),
                        }
                    } else {
                        move_managed_path_to_trash(entry.path()).await?;
                    }
                    continue;
                }
                if !registered_paths.contains(&relative_path) {
                    move_managed_path_to_trash(entry.path()).await?;
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn recover_registered_package_file_leases(
        &self,
        skill_name: &str,
        registered_paths: &BTreeSet<String>,
    ) -> Result<(), WorkflowMaterialsError> {
        self.ensure_skill_directory(skill_name).await?;
        for directory_name in ["references", "scripts", "assets"] {
            let directory = self.skills_root.join(skill_name).join(directory_name);
            ensure_existing_directory_without_symlink(&directory).await?;
            let mut entries = tokio::fs::read_dir(&directory).await?;
            while let Some(entry) = entries.next_entry().await? {
                let metadata = entry.file_type().await?;
                if metadata.is_symlink() || !metadata.is_file() {
                    return Err(WorkflowMaterialsError::InvalidRequest(
                        "managed package directory contains an unsafe entry".to_string(),
                    ));
                }
                let file_name = entry.file_name().into_string().map_err(|_| {
                    WorkflowMaterialsError::InvalidRequest(
                        "managed package directory contains a non-UTF-8 file name".to_string(),
                    )
                })?;
                let Some(original_name) = deletion_lease_original_name(&file_name) else {
                    continue;
                };
                let original_relative_path = format!("{directory_name}/{original_name}");
                if !registered_paths.contains(&original_relative_path) {
                    continue;
                }
                let original_path = directory.join(original_name);
                match tokio::fs::symlink_metadata(&original_path).await {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        tokio::fs::rename(entry.path(), original_path).await?;
                    }
                    Ok(_) => move_managed_path_to_trash(entry.path()).await?,
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn recover_registered_package_file_backups(
        &self,
        skill_name: &str,
        registered_checksums: &BTreeMap<String, String>,
    ) -> Result<(), WorkflowMaterialsError> {
        self.ensure_skill_directory(skill_name).await?;
        for directory_name in ["references", "scripts", "assets"] {
            let directory = self.skills_root.join(skill_name).join(directory_name);
            ensure_existing_directory_without_symlink(&directory).await?;
            let mut entries = tokio::fs::read_dir(&directory).await?;
            while let Some(entry) = entries.next_entry().await? {
                let metadata = entry.file_type().await?;
                if metadata.is_symlink() || !metadata.is_file() {
                    return Err(WorkflowMaterialsError::InvalidRequest(
                        "managed package directory contains an unsafe entry".to_string(),
                    ));
                }
                let file_name = entry.file_name().into_string().map_err(|_| {
                    WorkflowMaterialsError::InvalidRequest(
                        "managed package directory contains a non-UTF-8 file name".to_string(),
                    )
                })?;
                let Some(original_name) = staged_previous_original_name(&file_name) else {
                    continue;
                };
                let relative_path = format!("{directory_name}/{original_name}");
                let Some(expected_checksum) = registered_checksums.get(&relative_path) else {
                    continue;
                };
                let backup_checksum = checksum(&tokio::fs::read(entry.path()).await?);
                let destination = directory.join(original_name);
                let destination_checksum = managed_regular_file_checksum(&destination).await?;
                if destination_checksum.as_deref() == Some(expected_checksum) {
                    move_managed_path_to_trash(entry.path()).await?;
                } else if backup_checksum == *expected_checksum {
                    let displaced = if destination_checksum.is_some() {
                        let displaced = destination.with_file_name(format!(
                            ".{}.{}.recovery-discard",
                            original_name,
                            Uuid::new_v4()
                        ));
                        tokio::fs::rename(&destination, &displaced).await?;
                        Some(displaced)
                    } else {
                        None
                    };
                    if let Err(error) = tokio::fs::rename(entry.path(), &destination).await {
                        if let Some(displaced) = displaced {
                            tokio::fs::rename(displaced, destination).await?;
                        }
                        return Err(error.into());
                    }
                    if let Some(displaced) = displaced {
                        move_managed_path_to_trash(displaced).await?;
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn read_package_file_text(
        &self,
        skill_name: &str,
        relative_path: &str,
        expected_checksum: &str,
    ) -> Result<String, WorkflowMaterialsError> {
        validate_skill_name(skill_name)?;
        validate_relative_path(relative_path)?;
        ensure_existing_directory_without_symlink(&self.skills_root).await?;
        let root = self.skills_root.join(skill_name);
        ensure_existing_directory_without_symlink(&root).await?;
        let path = self.material_path(skill_name, relative_path)?;
        let parent = path
            .parent()
            .ok_or_else(|| WorkflowMaterialsError::InvalidRequest("package-file path is invalid".to_string()))?;
        ensure_existing_directory_without_symlink(parent).await?;
        ensure_regular_file_without_symlink(&path).await?;
        let content = tokio::fs::read_to_string(path).await?;
        let checksum = format!("{:x}", Sha256::digest(content.as_bytes()));
        if checksum != expected_checksum {
            return Err(WorkflowMaterialsError::InvalidRequest(
                "package-file content does not match its registered checksum".to_string(),
            ));
        }
        Ok(content)
    }

    pub(crate) async fn verify_package_file_bytes(
        &self,
        skill_name: &str,
        relative_path: &str,
        expected_checksum: &str,
    ) -> Result<(), WorkflowMaterialsError> {
        validate_skill_name(skill_name)?;
        validate_relative_path(relative_path)?;
        ensure_existing_directory_without_symlink(&self.skills_root).await?;
        let root = self.skills_root.join(skill_name);
        ensure_existing_directory_without_symlink(&root).await?;
        let path = self.material_path(skill_name, relative_path)?;
        let parent = path
            .parent()
            .ok_or_else(|| WorkflowMaterialsError::InvalidRequest("package-file path is invalid".to_string()))?;
        ensure_existing_directory_without_symlink(parent).await?;
        ensure_regular_file_without_symlink(&path).await?;
        let bytes = tokio::fs::read(path).await?;
        let checksum = format!("{:x}", Sha256::digest(&bytes));
        if checksum != expected_checksum {
            return Err(WorkflowMaterialsError::InvalidRequest(
                "package-file content does not match its registered checksum".to_string(),
            ));
        }
        Ok(())
    }

    async fn rename_skill_directory(
        &self,
        existing_skill_name: Option<&str>,
        requested_skill_name: &str,
    ) -> Result<SkillDirectoryRename, WorkflowMaterialsError> {
        let destination = self.skills_root.join(requested_skill_name);
        match existing_skill_name {
            Some(existing_skill_name) => {
                validate_skill_name(existing_skill_name)?;
                let source = self.skills_root.join(existing_skill_name);
                match tokio::fs::symlink_metadata(&source).await {
                    Ok(metadata) => {
                        if metadata.file_type().is_symlink() || !metadata.is_dir() {
                            return Err(WorkflowMaterialsError::InvalidRequest(
                                "existing Skill directory is unsafe".to_string(),
                            ));
                        }
                        if tokio::fs::try_exists(&destination).await? {
                            return Err(WorkflowMaterialsError::InvalidRequest(
                                "requested Skill directory already exists".to_string(),
                            ));
                        }
                        tokio::fs::rename(&source, &destination).await?;
                        let skill_file_created = matches!(
                            tokio::fs::symlink_metadata(destination.join("SKILL.md")).await,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound
                        );
                        if let Err(error) = rewrite_skill_definition_name(&destination, requested_skill_name).await {
                            let _ = tokio::fs::rename(&destination, &source).await;
                            return Err(error);
                        }
                        Ok(SkillDirectoryRename::Moved {
                            source,
                            destination,
                            skill_file_created,
                        })
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        self.create_skill_directory(requested_skill_name).await?;
                        Ok(SkillDirectoryRename::Created { destination })
                    }
                    Err(error) => Err(error.into()),
                }
            }
            None => {
                self.create_skill_directory(requested_skill_name).await?;
                Ok(SkillDirectoryRename::Created { destination })
            }
        }
    }

    async fn create_skill_directory(
        &self,
        skill_name: &str,
    ) -> Result<(), WorkflowMaterialsError> {
        let destination = self.skills_root.join(skill_name);
        match tokio::fs::symlink_metadata(&destination).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.ensure_skill_directory(skill_name).await,
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                Err(WorkflowMaterialsError::InvalidRequest(
                    "requested Skill directory is not a regular directory".to_string(),
                ))
            }
            Ok(_) => self.ensure_skill_directory(skill_name).await,
            Err(error) => Err(error.into()),
        }
    }

    async fn write_material_file(
        &self,
        skill_name: &str,
        relative_path: &str,
        bytes: &[u8],
    ) -> Result<(), WorkflowMaterialsError> {
        let path = self.material_path(skill_name, relative_path)?;
        write_atomic(&path, bytes).await
    }

    async fn remove_material_file(
        &self,
        skill_name: &str,
        relative_path: &str,
    ) -> Result<(), WorkflowMaterialsError> {
        let path = self.material_path(skill_name, relative_path)?;
        match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(WorkflowMaterialsError::InvalidRequest(
                "material file must not be a symbolic link".to_string(),
            )),
            Ok(metadata) if !metadata.is_file() => Err(WorkflowMaterialsError::InvalidRequest(
                "material path is not a file".to_string(),
            )),
            Ok(_) => {
                tokio::fs::remove_file(path).await?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    async fn stage_material_file(
        &self,
        skill_name: &str,
        relative_path: &str,
    ) -> Result<Option<StagedMaterialFile>, WorkflowMaterialsError> {
        let original = self.material_path(skill_name, relative_path)?;
        let metadata = match tokio::fs::symlink_metadata(&original).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(WorkflowMaterialsError::InvalidRequest(
                "material path is unsafe".to_string(),
            ));
        }
        let staged = original.with_file_name(format!(
            ".{}.{}.delete",
            original
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("material"),
            Uuid::new_v4()
        ));
        tokio::fs::rename(&original, &staged).await?;
        Ok(Some(StagedMaterialFile { original, staged }))
    }

    async fn rollback_written_file(
        &self,
        skill_name: &str,
        relative_path: &str,
        staged_previous: Option<StagedMaterialFile>,
    ) -> Result<(), WorkflowMaterialsError> {
        let removal = self.remove_material_file(skill_name, relative_path).await;
        if let Some(staged_file) = staged_previous {
            staged_file.restore().await?;
        }
        removal
    }

    fn material_path(
        &self,
        skill_name: &str,
        relative_path: &str,
    ) -> Result<PathBuf, WorkflowMaterialsError> {
        validate_skill_name(skill_name)?;
        validate_relative_path(relative_path)?;
        let root = self.skills_root.join(skill_name);
        let target = root.join(relative_path);
        if !target.starts_with(&root) {
            return Err(WorkflowMaterialsError::InvalidRequest(
                "material path escapes its Skill directory".to_string(),
            ));
        }
        Ok(target)
    }
}

#[derive(FromRow)]
struct MaterialRow {
    material_id: String,
    profile_id: String,
    material_revision: i64,
    ordinal: i64,
    title: String,
    kind: String,
    external_url: Option<String>,
    relative_path: Option<String>,
    original_filename: Option<String>,
    file_size: Option<i64>,
    checksum: Option<String>,
    markdown_content: Option<String>,
    created_at: String,
    updated_at: String,
}

impl MaterialRow {
    fn into_material(
        self,
        reference_step_ids: Vec<String>,
    ) -> Result<WorkflowMaterial, WorkflowMaterialsError> {
        Ok(WorkflowMaterial {
            material_id: self.material_id,
            profile_id: self.profile_id,
            material_revision: self.material_revision,
            ordinal: self.ordinal,
            title: self.title,
            kind: WorkflowMaterialKind::from_str(&self.kind).map_err(|_| {
                WorkflowMaterialsError::InvalidRequest("invalid stored workflow material kind".to_string())
            })?,
            external_url: self.external_url,
            relative_path: self.relative_path,
            original_filename: self.original_filename,
            file_size: self.file_size,
            checksum: self.checksum,
            markdown_content: self.markdown_content,
            created_at: self.created_at,
            updated_at: self.updated_at,
            reference_step_ids,
        })
    }
}

pub(crate) async fn ensure_skill_name(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
) -> Result<String, WorkflowMaterialsError> {
    if let Some(skill_name) = sqlx::query_scalar("SELECT skill_name FROM workflow_profile_skills WHERE profile_id = ?")
        .bind(profile_id)
        .fetch_optional(&mut **transaction)
        .await?
    {
        return Ok(skill_name);
    }
    let normalized_id = profile_id
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() || character.is_ascii_digit() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let normalized_id = normalized_id
        .split('-')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let skill_name = if normalized_id.is_empty() {
        "workflow-profile".to_string()
    } else {
        format!("workflow-{normalized_id}")
    };
    validate_skill_name(&skill_name)?;
    sqlx::query("INSERT INTO workflow_profile_skills (profile_id, skill_name) VALUES (?, ?)")
        .bind(profile_id)
        .bind(&skill_name)
        .execute(&mut **transaction)
        .await?;
    Ok(skill_name)
}

fn deletion_lease_original_name(file_name: &str) -> Option<&str> {
    let lease_name = file_name.strip_prefix('.')?.strip_suffix(".deletion-lease")?;
    let (original_name, lease_id) = lease_name.rsplit_once('.')?;
    (!original_name.is_empty() && Uuid::parse_str(lease_id).is_ok()).then_some(original_name)
}

fn staged_previous_original_name(file_name: &str) -> Option<&str> {
    let staged_name = file_name.strip_prefix('.')?.strip_suffix(".previous")?;
    let (original_name, staged_id) = staged_name.rsplit_once('.')?;
    (!original_name.is_empty() && Uuid::parse_str(staged_id).is_ok()).then_some(original_name)
}

async fn managed_regular_file_checksum(path: &Path) -> Result<Option<String>, WorkflowMaterialsError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(
            WorkflowMaterialsError::InvalidRequest("managed package path contains an unsafe entry".to_string()),
        ),
        Ok(_) => Ok(Some(checksum(&tokio::fs::read(path).await?))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn ensure_material_library(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
) -> Result<(), WorkflowMaterialsError> {
    sqlx::query("INSERT OR IGNORE INTO workflow_profile_material_libraries (profile_id) VALUES (?)")
        .bind(profile_id)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn bump_materials_revision(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
) -> Result<(), WorkflowMaterialsError> {
    sqlx::query("UPDATE workflow_profile_material_libraries SET materials_revision = materials_revision + 1, updated_at = CURRENT_TIMESTAMP WHERE profile_id = ?")
        .bind(profile_id)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn verify_materials_revision(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    expected_materials_revision: i64,
) -> Result<(), WorkflowMaterialsError> {
    let current_materials_revision: i64 =
        sqlx::query_scalar("SELECT materials_revision FROM workflow_profile_material_libraries WHERE profile_id = ?")
            .bind(profile_id)
            .fetch_one(&mut **transaction)
            .await?;
    if current_materials_revision == expected_materials_revision {
        Ok(())
    } else {
        Err(WorkflowMaterialsError::MaterialsChanged {
            current_materials_revision,
        })
    }
}

async fn load_material_row(
    transaction: &mut Transaction<'_, Sqlite>,
    material_id: &str,
) -> Result<Option<MaterialRow>, WorkflowMaterialsError> {
    Ok(sqlx::query_as("SELECT material_id, profile_id, material_revision, ordinal, title, kind, external_url, relative_path, original_filename, file_size, checksum, markdown_content, created_at, updated_at FROM workflow_profile_materials WHERE material_id = ?").bind(material_id).fetch_optional(&mut **transaction).await?)
}

async fn next_material_ordinal(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
) -> Result<i64, WorkflowMaterialsError> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(MAX(ordinal) + 1, 0) FROM workflow_profile_materials WHERE profile_id = ?",
    )
    .bind(profile_id)
    .fetch_one(&mut **transaction)
    .await?)
}

async fn save_external_url(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &WorkflowMaterialSaveCommand,
    material_id: &str,
) -> Result<WorkflowMaterial, WorkflowMaterialsError> {
    let url = validate_external_url(command.external_url.as_deref())?;
    save_inline(
        transaction,
        command,
        material_id,
        WorkflowMaterialKind::ExternalUrl,
        Some(url),
        None,
        None,
    )
    .await
}

async fn save_markdown(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &WorkflowMaterialSaveCommand,
    material_id: &str,
) -> Result<WorkflowMaterial, WorkflowMaterialsError> {
    let content = command
        .markdown_content
        .as_deref()
        .ok_or_else(|| WorkflowMaterialsError::InvalidRequest("markdown content is required".to_string()))?;
    if content.len() > MAX_UPLOAD_BYTES {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "markdown content exceeds 5 MiB".to_string(),
        ));
    }
    let path = if command.material_id.is_some() {
        let current = load_material_row(transaction, material_id)
            .await?
            .ok_or(WorkflowMaterialsError::MaterialNotFound)?;
        if current.profile_id != command.profile_id {
            return Err(WorkflowMaterialsError::MaterialNotFound);
        }
        match current.relative_path {
            Some(existing) => existing,
            None => {
                allocate_created_material_path(
                    transaction,
                    &command.profile_id,
                    material_id,
                    "md",
                    Some(command.title.as_str()),
                    None,
                )
                .await?
            }
        }
    } else {
        allocate_created_material_path(
            transaction,
            &command.profile_id,
            material_id,
            "md",
            Some(command.title.as_str()),
            None,
        )
        .await?
    };
    save_inline(
        transaction,
        command,
        material_id,
        WorkflowMaterialKind::MarkdownFile,
        None,
        Some(path),
        Some(content.to_string()),
    )
    .await
}

async fn save_uploaded_file_metadata(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &WorkflowMaterialSaveCommand,
    material_id: &str,
) -> Result<WorkflowMaterial, WorkflowMaterialsError> {
    let current = load_material_row(transaction, material_id)
        .await?
        .ok_or(WorkflowMaterialsError::MaterialNotFound)?;
    if current.profile_id != command.profile_id
        || current.material_revision != command.expected_material_revision.unwrap_or(-1)
    {
        return Err(WorkflowMaterialsError::MaterialChanged {
            current_material_revision: current.material_revision,
        });
    }
    if WorkflowMaterialKind::from_str(&current.kind).ok() != Some(WorkflowMaterialKind::UploadedFile) {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "uploaded file metadata must retain its file type".to_string(),
        ));
    }
    if command.external_url.is_some() || command.markdown_content.is_some() {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "uploaded file metadata cannot include inline content".to_string(),
        ));
    }

    sqlx::query("UPDATE workflow_profile_materials SET material_revision = material_revision + 1, title = ?, updated_at = CURRENT_TIMESTAMP WHERE material_id = ?")
        .bind(&command.title)
        .bind(material_id)
        .execute(&mut **transaction)
        .await?;
    load_material_row(transaction, material_id)
        .await?
        .ok_or(WorkflowMaterialsError::MaterialNotFound)?
        .into_material(Vec::new())
}

async fn save_inline(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &WorkflowMaterialSaveCommand,
    material_id: &str,
    kind: WorkflowMaterialKind,
    external_url: Option<String>,
    relative_path: Option<String>,
    markdown_content: Option<String>,
) -> Result<WorkflowMaterial, WorkflowMaterialsError> {
    if command.material_id.is_some() {
        let current = load_material_row(transaction, material_id)
            .await?
            .ok_or(WorkflowMaterialsError::MaterialNotFound)?;
        if current.profile_id != command.profile_id
            || current.material_revision != command.expected_material_revision.unwrap_or(-1)
        {
            return Err(WorkflowMaterialsError::MaterialChanged {
                current_material_revision: current.material_revision,
            });
        }
        if WorkflowMaterialKind::from_str(&current.kind).ok() == Some(WorkflowMaterialKind::UploadedFile) {
            return Err(WorkflowMaterialsError::InvalidRequest(
                "uploaded files must use the replace endpoint".to_string(),
            ));
        }
        sqlx::query("UPDATE workflow_profile_materials SET material_revision = material_revision + 1, title = ?, kind = ?, external_url = ?, relative_path = ?, original_filename = NULL, file_size = ?, checksum = ?, markdown_content = ?, updated_at = CURRENT_TIMESTAMP WHERE material_id = ?")
            .bind(&command.title).bind(kind.as_str()).bind(&external_url).bind(&relative_path).bind(markdown_content.as_ref().map(|content| content.len() as i64)).bind(markdown_content.as_ref().map(|content| checksum(content.as_bytes()))).bind(&markdown_content).bind(material_id).execute(&mut **transaction).await?;
    } else {
        let ordinal = next_material_ordinal(transaction, &command.profile_id).await?;
        sqlx::query("INSERT INTO workflow_profile_materials (material_id, profile_id, ordinal, title, kind, external_url, relative_path, file_size, checksum, markdown_content) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(material_id).bind(&command.profile_id).bind(ordinal).bind(&command.title).bind(kind.as_str()).bind(&external_url).bind(&relative_path).bind(markdown_content.as_ref().map(|content| content.len() as i64)).bind(markdown_content.as_ref().map(|content| checksum(content.as_bytes()))).bind(&markdown_content).execute(&mut **transaction).await?;
    }
    load_material_row(transaction, material_id)
        .await?
        .ok_or(WorkflowMaterialsError::MaterialNotFound)?
        .into_material(Vec::new())
}

fn validate_save_command(command: &WorkflowMaterialSaveCommand) -> Result<(), WorkflowMaterialsError> {
    if command.profile_id.trim().is_empty() || command.title.trim().is_empty() {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "profile ID and material title are required".to_string(),
        ));
    }
    if command
        .material_id
        .as_deref()
        .is_some_and(|id| Uuid::parse_str(id).is_err())
    {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "material ID must be a UUID".to_string(),
        ));
    }
    if command.kind == WorkflowMaterialKind::UploadedFile && command.material_id.is_none() {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "uploaded files must use the upload endpoint when created".to_string(),
        ));
    }
    Ok(())
}

fn validate_uploaded_file(
    title: &str,
    filename: &str,
    bytes: &[u8],
) -> Result<(), WorkflowMaterialsError> {
    if title.trim().is_empty() {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "material title is required".to_string(),
        ));
    }
    if bytes.is_empty() {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "uploaded file must not be empty".to_string(),
        ));
    }
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "uploaded file exceeds 5 MiB".to_string(),
        ));
    }
    let Some(extension) = allowed_extension(filename) else {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "uploaded file extension is not allowed".to_string(),
        ));
    };
    validate_uploaded_content(&extension, bytes)
}

fn validate_uploaded_content(
    extension: &str,
    bytes: &[u8],
) -> Result<(), WorkflowMaterialsError> {
    if is_text_extension(extension) {
        std::str::from_utf8(bytes).map_err(|_| {
            WorkflowMaterialsError::InvalidRequest("uploaded text file must be valid UTF-8".to_string())
        })?;
        return Ok(());
    }

    if extension == "pdf" {
        return if bytes.starts_with(b"%PDF-") {
            Ok(())
        } else {
            Err(WorkflowMaterialsError::InvalidRequest(
                "uploaded PDF file is invalid".to_string(),
            ))
        };
    }

    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| WorkflowMaterialsError::InvalidRequest("uploaded Office file is invalid".to_string()))?;
    let has_content_types = archive.by_name("[Content_Types].xml").is_ok();
    let expected_document = match extension {
        "docx" => "word/document.xml",
        "xlsx" => "xl/workbook.xml",
        _ => unreachable!("only Office extensions reach this branch"),
    };
    if has_content_types && archive.by_name(expected_document).is_ok() {
        Ok(())
    } else {
        Err(WorkflowMaterialsError::InvalidRequest(
            "uploaded Office file does not match its extension".to_string(),
        ))
    }
}

fn validate_external_url(value: Option<&str>) -> Result<String, WorkflowMaterialsError> {
    let value = value.ok_or_else(|| WorkflowMaterialsError::InvalidRequest("external URL is required".to_string()))?;
    let url =
        Url::parse(value).map_err(|_| WorkflowMaterialsError::InvalidRequest("external URL is invalid".to_string()))?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "external URL must use https".to_string(),
        ));
    }
    Ok(url.into())
}

fn validate_skill_name(skill_name: &str) -> Result<(), WorkflowMaterialsError> {
    let valid = !skill_name.is_empty()
        && skill_name.len() <= 64
        && skill_name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !skill_name.starts_with('-')
        && !skill_name.ends_with('-')
        && !skill_name.contains("--");
    if valid {
        Ok(())
    } else {
        Err(WorkflowMaterialsError::InvalidRequest(
            "generated Skill name is invalid".to_string(),
        ))
    }
}

fn storage_directory_for_extension(extension: &str) -> &'static str {
    if matches!(extension, "js" | "mjs" | "cjs" | "py" | "sh" | "bat") {
        "scripts"
    } else if matches!(
        extension,
        "pdf" | "docx" | "xlsx" | "png" | "jpg" | "jpeg" | "webp" | "svg" | "gif" | "ico" | "csv" | "sql"
    ) {
        "assets"
    } else {
        "references"
    }
}

fn material_stem_fallback(material_id: &str) -> String {
    let compact = material_id
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .take(6)
        .collect::<String>()
        .to_ascii_lowercase();
    if compact.is_empty() {
        "material".to_string()
    } else {
        format!("material-{compact}")
    }
}

fn filename_stem(filename: &str) -> Option<String> {
    let stem = Path::new(filename).file_stem()?.to_str()?.trim();
    if stem.is_empty() { None } else { Some(stem.to_string()) }
}

fn leaf_stem_from_relative_path(relative_path: &str) -> Option<String> {
    filename_stem(Path::new(relative_path).file_name()?.to_str()?)
}

/// Build a human-readable file stem for Skill directory storage.
/// Keeps Unicode letters/digits (including CJK). Does not use material_id unless fallback.
fn normalize_material_stem(raw: &str) -> String {
    let mut stem = String::new();
    let mut last_was_separator = false;
    for character in raw.trim().chars() {
        if character.is_alphanumeric() {
            for lower in character.to_lowercase() {
                stem.push(lower);
            }
            last_was_separator = false;
            continue;
        }
        if matches!(character, '.' | '-' | '_' | ' ' | '\t' | '\n' | '\r') && !stem.is_empty() && !last_was_separator {
            stem.push('-');
            last_was_separator = true;
        }
    }
    while stem.ends_with('-') || stem.ends_with('.') {
        stem.pop();
    }
    while stem.starts_with('-') || stem.starts_with('.') {
        stem.remove(0);
    }
    // Collapse repeated separators that can appear after trimming edges mid-pass.
    let mut collapsed = String::with_capacity(stem.len());
    let mut previous_separator = false;
    for character in stem.chars() {
        let separator = character == '-' || character == '.';
        if separator && previous_separator {
            // Prefer keeping a single '-' when mixing '.' and '-'.
            if collapsed.ends_with('.') && character == '-' {
                collapsed.pop();
                collapsed.push('-');
            }
            continue;
        }
        collapsed.push(character);
        previous_separator = separator;
    }
    while collapsed.ends_with('-') || collapsed.ends_with('.') {
        collapsed.pop();
    }
    if collapsed.len() > 64 {
        let mut end = 64;
        while end > 0 && !collapsed.is_char_boundary(end) {
            end -= 1;
        }
        collapsed.truncate(end);
        while collapsed.ends_with('-') || collapsed.ends_with('.') {
            collapsed.pop();
        }
    }
    if collapsed.is_empty() || collapsed == "skill" {
        String::new()
    } else {
        collapsed
    }
}

fn preferred_material_stem(
    primary: Option<&str>,
    secondary: Option<&str>,
    material_id: &str,
) -> String {
    for candidate in [primary, secondary].into_iter().flatten() {
        let stem = normalize_material_stem(candidate);
        if !stem.is_empty() {
            return stem;
        }
    }
    material_stem_fallback(material_id)
}

fn storage_relative_path(
    stem: &str,
    extension: &str,
) -> Result<String, WorkflowMaterialsError> {
    if stem.is_empty() || extension.is_empty() {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "material storage path is invalid".to_string(),
        ));
    }
    let directory = storage_directory_for_extension(extension);
    let path = format!("{directory}/{stem}.{extension}");
    validate_relative_path(&path)?;
    // references/skill.md is rejected by validate_relative_path via SKILL.md suffix check
    // (case-sensitive). Also reject case-insensitive skill.md leaf explicitly.
    if path.to_ascii_lowercase().ends_with("/skill.md") {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "material storage path is reserved".to_string(),
        ));
    }
    Ok(path)
}

async fn taken_relative_paths(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    exclude_material_id: Option<&str>,
) -> Result<BTreeSet<String>, WorkflowMaterialsError> {
    let rows: Vec<String> = if let Some(exclude_material_id) = exclude_material_id {
        sqlx::query_scalar(
            "SELECT relative_path FROM workflow_profile_materials \
             WHERE profile_id = ? AND relative_path IS NOT NULL AND material_id != ? \
             UNION \
             SELECT relative_path FROM workflow_profile_package_files \
             WHERE profile_id = ?",
        )
        .bind(profile_id)
        .bind(exclude_material_id)
        .bind(profile_id)
        .fetch_all(&mut **transaction)
        .await?
    } else {
        sqlx::query_scalar(
            "SELECT relative_path FROM workflow_profile_materials \
             WHERE profile_id = ? AND relative_path IS NOT NULL \
             UNION \
             SELECT relative_path FROM workflow_profile_package_files \
             WHERE profile_id = ?",
        )
        .bind(profile_id)
        .bind(profile_id)
        .fetch_all(&mut **transaction)
        .await?
    };
    Ok(rows.into_iter().collect())
}

async fn allocate_storage_relative_path(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    preferred_stem: &str,
    extension: &str,
    exclude_material_id: Option<&str>,
) -> Result<String, WorkflowMaterialsError> {
    let taken = taken_relative_paths(transaction, profile_id, exclude_material_id).await?;
    if preferred_stem.is_empty() {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "material storage stem is empty".to_string(),
        ));
    }
    let base_stem = if storage_relative_path(preferred_stem, extension).is_ok() {
        preferred_stem.to_string()
    } else {
        let fallback = format!("{preferred_stem}-file");
        if storage_relative_path(&fallback, extension).is_err() {
            return Err(WorkflowMaterialsError::InvalidRequest(
                "material storage path is invalid".to_string(),
            ));
        }
        fallback
    };
    for ordinal in 1..10_000 {
        let candidate_stem = if ordinal == 1 {
            base_stem.clone()
        } else {
            format!("{base_stem}-{ordinal}")
        };
        let path = storage_relative_path(&candidate_stem, extension)?;
        if !taken.contains(&path) {
            return Ok(path);
        }
    }
    Err(WorkflowMaterialsError::InvalidRequest(
        "unable to allocate a unique material storage path".to_string(),
    ))
}

async fn allocate_created_material_path(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    material_id: &str,
    extension: &str,
    primary_stem_source: Option<&str>,
    secondary_stem_source: Option<&str>,
) -> Result<String, WorkflowMaterialsError> {
    let preferred = preferred_material_stem(primary_stem_source, secondary_stem_source, material_id);
    allocate_storage_relative_path(transaction, profile_id, &preferred, extension, None).await
}

async fn resolve_replace_material_path(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    material_id: &str,
    extension: &str,
    previous_relative_path: Option<&str>,
    original_filename: &str,
    title: &str,
) -> Result<String, WorkflowMaterialsError> {
    if let Some(previous_relative_path) = previous_relative_path {
        let previous_extension = Path::new(previous_relative_path)
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| value.to_ascii_lowercase());
        if previous_extension.as_deref() == Some(extension)
            && storage_directory_for_extension(extension)
                == Path::new(previous_relative_path)
                    .components()
                    .next()
                    .and_then(|component| match component {
                        Component::Normal(name) => name.to_str(),
                        _ => None,
                    })
                    .unwrap_or_default()
        {
            validate_relative_path(previous_relative_path)?;
            return Ok(previous_relative_path.to_string());
        }
        let stem = leaf_stem_from_relative_path(previous_relative_path)
            .map(|value| normalize_material_stem(&value))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                preferred_material_stem(filename_stem(original_filename).as_deref(), Some(title), material_id)
            });
        return allocate_storage_relative_path(transaction, profile_id, &stem, extension, Some(material_id)).await;
    }
    allocate_created_material_path(
        transaction,
        profile_id,
        material_id,
        extension,
        filename_stem(original_filename).as_deref(),
        Some(title),
    )
    .await
}

pub(crate) fn validate_relative_path(path: &str) -> Result<(), WorkflowMaterialsError> {
    let value = Path::new(path);
    let mut components = value.components();
    let directory = components.next();
    let filename = components.next();
    let valid_directory = matches!(
        directory,
        Some(Component::Normal(name)) if matches!(name.to_str(), Some("references" | "scripts" | "assets"))
    );
    let valid = value
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
        && valid_directory
        && matches!(filename, Some(Component::Normal(_)))
        && components.next().is_none()
        && !path.ends_with("SKILL.md");
    if valid {
        Ok(())
    } else {
        Err(WorkflowMaterialsError::InvalidRequest(
            "material path is invalid".to_string(),
        ))
    }
}

fn allowed_extension(filename: &str) -> Option<String> {
    let extension = Path::new(filename).extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "md" | "js" | "mjs" | "cjs" | "py" | "sh" | "bat" | "pdf" | "json" | "yaml" | "yml" | "toml" | "docx"
        | "xlsx" | "png" | "jpg" | "jpeg" | "webp" | "svg" | "gif" | "ico" | "csv" | "sql" => Some(extension),
        _ => None,
    }
}

fn is_text_extension(extension: &str) -> bool {
    matches!(
        extension,
        "md" | "js" | "mjs" | "cjs" | "py" | "sh" | "bat" | "json" | "yaml" | "yml" | "toml" | "csv" | "sql" | "svg"
    )
}

fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

async fn write_atomic(
    path: &Path,
    bytes: &[u8],
) -> Result<(), WorkflowMaterialsError> {
    let parent = path
        .parent()
        .ok_or_else(|| WorkflowMaterialsError::InvalidRequest("material target has no parent directory".to_string()))?;
    let metadata = tokio::fs::symlink_metadata(parent).await?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "material target directory is unsafe".to_string(),
        ));
    }
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|name| name.to_str()).unwrap_or("material"),
        Uuid::new_v4()
    ));
    if let Err(error) = tokio::fs::write(&temporary, bytes).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    if let Err(error) = tokio::fs::rename(&temporary, path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    Ok(())
}

async fn rewrite_skill_definition_name(
    skill_directory: &Path,
    skill_name: &str,
) -> Result<(), WorkflowMaterialsError> {
    validate_skill_name(skill_name)?;
    let skill_file = skill_directory.join("SKILL.md");
    match tokio::fs::symlink_metadata(&skill_file).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(WorkflowMaterialsError::InvalidRequest(
                "SKILL.md path is unsafe".to_string(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let scaffold = format!(
                "---\nname: {skill_name}\ndescription: Managed Workflow Profile material library.\n---\n\n# Materials\n"
            );
            return write_atomic(&skill_file, scaffold.as_bytes()).await;
        }
        Err(error) => return Err(error.into()),
    }
    let definition = tokio::fs::read_to_string(&skill_file).await?;
    let mut lines = definition.lines();
    if lines.next() != Some("---") {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "SKILL.md frontmatter is invalid".to_string(),
        ));
    }
    let mut replaced = false;
    let mut rewritten = Vec::new();
    for line in definition.lines() {
        if !replaced && line.starts_with("name:") {
            rewritten.push(format!("name: {skill_name}"));
            replaced = true;
        } else {
            rewritten.push(line.to_string());
        }
    }
    if !replaced {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "SKILL.md frontmatter is missing name".to_string(),
        ));
    }
    let content = format!("{}\n", rewritten.join("\n"));
    write_atomic(&skill_file, content.as_bytes()).await
}

async fn remove_skill_definition(skill_directory: &Path) -> Result<(), WorkflowMaterialsError> {
    let skill_file = skill_directory.join("SKILL.md");
    match tokio::fs::symlink_metadata(&skill_file).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(
            WorkflowMaterialsError::InvalidRequest("SKILL.md path is unsafe".to_string()),
        ),
        Ok(_) => {
            tokio::fs::remove_file(skill_file).await?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn ensure_directory_without_symlink(path: &Path) -> Result<(), WorkflowMaterialsError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(WorkflowMaterialsError::InvalidRequest(
            "Skill directory must not contain symbolic links".to_string(),
        )),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(WorkflowMaterialsError::InvalidRequest(
            "Skill directory path is not a directory".to_string(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir(path).await?;
            let metadata = tokio::fs::symlink_metadata(path).await?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(WorkflowMaterialsError::InvalidRequest(
                    "Skill directory path is unsafe".to_string(),
                ));
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

async fn ensure_existing_directory_without_symlink(path: &Path) -> Result<(), WorkflowMaterialsError> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "Skill directory path is unsafe".to_string(),
        ));
    }
    Ok(())
}

async fn ensure_regular_file_without_symlink(path: &Path) -> Result<(), WorkflowMaterialsError> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(WorkflowMaterialsError::InvalidRequest(
            "material file is unsafe".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_extensions_match_the_material_contract() {
        for filename in [
            "reference.md",
            "script.js",
            "module.mjs",
            "legacy.cjs",
            "tool.py",
            "file.pdf",
            "data.json",
            "config.yaml",
            "config.yml",
            "config.toml",
            "document.docx",
            "sheet.xlsx",
            "setup.sh",
            "run.bat",
            "icon.png",
            "photo.jpg",
            "photo.jpeg",
            "preview.webp",
            "mark.svg",
            "anim.gif",
            "app.ico",
            "rows.csv",
            "schema.sql",
        ] {
            assert!(allowed_extension(filename).is_some(), "{filename}");
        }
        for filename in ["macro.docm", "legacy.doc", "sheet.xls", "archive.zip", "no-extension"] {
            assert!(allowed_extension(filename).is_none(), "{filename}");
        }
    }

    #[test]
    fn material_paths_are_limited_to_skill_subdirectories() {
        assert!(validate_relative_path("references/brief.md").is_ok());
        assert!(validate_relative_path("scripts/check.py").is_ok());
        assert!(validate_relative_path("assets/report.pdf").is_ok());
        assert!(validate_relative_path("../SKILL.md").is_err());
        assert!(validate_relative_path("references/../SKILL.md").is_err());
        assert!(validate_relative_path("references/SKILL.md").is_err());
        assert!(validate_relative_path("references/nested/brief.md").is_err());
    }

    #[tokio::test]
    async fn package_file_deletion_lease_rolls_back_or_moves_to_native_trash() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        let temporary = tempfile::tempdir().expect("create Skills root");
        let service = WorkflowMaterialsService::new(pool, temporary.path().to_path_buf());
        service
            .ensure_skill_directory("workflow-guide")
            .await
            .expect("create managed Skill directory");
        let path = temporary.path().join("workflow-guide/references/policy.md");
        tokio::fs::write(&path, b"policy")
            .await
            .expect("write managed package file");

        let lease = service
            .stage_package_file_deletion_lease("workflow-guide", "references/policy.md")
            .await
            .expect("stage deletion lease");
        assert!(!path.exists());
        lease.rollback().await.expect("roll back deletion lease");
        assert_eq!(tokio::fs::read(&path).await.expect("read restored file"), b"policy");

        let unsafe_lease = service
            .stage_package_file_deletion_lease("workflow-guide", "references/policy.md")
            .await
            .expect("stage unsafe-source deletion lease");
        let leased_path = unsafe_lease.leased.as_ref().expect("leased source path").clone();
        tokio::fs::remove_file(&leased_path)
            .await
            .expect("remove leased source before substitution");
        std::os::unix::fs::symlink(&path, &leased_path).expect("substitute leased source with symlink");
        let error = unsafe_lease
            .rollback()
            .await
            .expect_err("unsafe leased source must not be restored");
        assert!(error.to_string().contains("source is unsafe"));
        tokio::fs::remove_file(leased_path)
            .await
            .expect("remove unsafe leased source");
        tokio::fs::write(&path, b"policy")
            .await
            .expect("restore managed package file for Trash test");

        let mut lease = service
            .stage_package_file_deletion_lease("workflow-guide", "references/policy.md")
            .await
            .expect("stage second deletion lease");
        lease.move_to_trash().await.expect("move deletion lease to Trash");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn repair_cleanup_trashes_only_unregistered_package_files() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        let temporary = tempfile::tempdir().expect("create Skills root");
        let service = WorkflowMaterialsService::new(pool, temporary.path().to_path_buf());
        service
            .ensure_skill_directory("workflow-guide")
            .await
            .expect("create managed Skill directory");
        let registered = temporary.path().join("workflow-guide/references/registered.md");
        let orphaned = temporary.path().join("workflow-guide/references/orphaned.md");
        let recoverable_lease = temporary
            .path()
            .join("workflow-guide/references/.recover.md.00000000-0000-4000-8000-000000000001.deletion-lease");
        let committed_lease = temporary
            .path()
            .join("workflow-guide/references/.removed.md.00000000-0000-4000-8000-000000000002.deletion-lease");
        tokio::fs::write(&registered, b"registered")
            .await
            .expect("write registered file");
        tokio::fs::write(&orphaned, b"orphaned")
            .await
            .expect("write orphaned file");
        tokio::fs::write(&recoverable_lease, b"recoverable")
            .await
            .expect("write pre-commit residual lease");
        tokio::fs::write(&committed_lease, b"committed")
            .await
            .expect("write post-commit residual lease");

        service
            .trash_unregistered_package_files(
                "workflow-guide",
                &BTreeSet::from([
                    "references/registered.md".to_string(),
                    "references/recover.md".to_string(),
                ]),
            )
            .await
            .expect("clean unregistered package files");

        assert!(registered.exists());
        assert!(!orphaned.exists());
        assert_eq!(
            tokio::fs::read(temporary.path().join("workflow-guide/references/recover.md"))
                .await
                .expect("read restored pre-commit lease"),
            b"recoverable"
        );
        assert!(!recoverable_lease.exists());
        assert!(!committed_lease.exists());
    }

    #[tokio::test]
    async fn skill_package_lock_serializes_projection_and_cleanup() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        let temporary = tempfile::tempdir().expect("create Skills root");
        let first_service = WorkflowMaterialsService::new(pool.clone(), temporary.path().to_path_buf());
        let second_service = WorkflowMaterialsService::new(pool, temporary.path().to_path_buf());
        let first_guard = first_service
            .lock_skill_package("workflow-guide")
            .await
            .expect("acquire first package lock");

        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                second_service.lock_skill_package("workflow-guide"),
            )
            .await
            .is_err(),
            "a second package operation must wait while the first holds the lock"
        );

        drop(first_guard);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            second_service.lock_skill_package("workflow-guide"),
        )
        .await
        .expect("second package operation resumes after unlock")
        .expect("acquire second package lock");
    }

    #[tokio::test]
    async fn material_paths_reserve_guide_package_file_paths() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Workflow', '', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .expect("insert Workflow Profile");
        sqlx::query(
            "INSERT INTO workflow_profile_package_files (
                package_file_id, profile_id, ordinal, title, category, relative_path
             ) VALUES ('guide-file', 'workflow-profile', 0, 'Policy', 'reference', 'references/policy.md')",
        )
        .execute(&pool)
        .await
        .expect("reserve Guide package path");
        let mut transaction = pool.begin().await.expect("begin allocation transaction");

        let allocated = allocate_storage_relative_path(&mut transaction, "workflow-profile", "policy", "md", None)
            .await
            .expect("allocate legacy Material path");

        assert_eq!(allocated, "references/policy-2.md");
    }

    #[test]
    fn external_urls_require_https() {
        assert!(validate_external_url(Some("https://example.com/reference")).is_ok());
        assert!(validate_external_url(Some("http://example.com/reference")).is_err());
        assert!(validate_external_url(Some("file:///tmp/reference.md")).is_err());
    }

    #[test]
    fn uploaded_files_must_match_their_declared_type() {
        assert!(validate_uploaded_file("Notes", "notes.md", b"# Notes\n").is_ok());
        assert!(validate_uploaded_file("Notes", "notes.md", &[0xff]).is_err());
        assert!(validate_uploaded_file("Guide", "guide.pdf", b"%PDF-1.7\n").is_ok());
        assert!(validate_uploaded_file("Guide", "guide.pdf", b"not a PDF").is_err());
        assert!(validate_uploaded_file("Report", "report.docx", b"not a zip").is_err());
    }

    #[test]
    fn material_stems_prefer_readable_names_and_keep_unicode() {
        assert_eq!(
            normalize_material_stem("Research Brief (final)"),
            "research-brief-final"
        );
        assert_eq!(normalize_material_stem("  API_Auth Notes  "), "api-auth-notes");
        assert_eq!(normalize_material_stem("客户调研纪要 v2"), "客户调研纪要-v2");
        assert_eq!(normalize_material_stem("!!!"), "");
        assert_eq!(normalize_material_stem("Skill"), "");
        assert_eq!(
            preferred_material_stem(
                filename_stem("evidence.md").as_deref(),
                Some("Ignored Title"),
                "9e742db7-2d52-4c02-91fb-25c8c86a1495"
            ),
            "evidence"
        );
        assert_eq!(
            preferred_material_stem(
                filename_stem("Research Brief (final).MD").as_deref(),
                Some("Title"),
                "9e742db7-2d52-4c02-91fb-25c8c86a1495"
            ),
            "research-brief-final"
        );
        assert_eq!(
            preferred_material_stem(None, Some("Local evidence"), "9e742db7-2d52-4c02-91fb-25c8c86a1495"),
            "local-evidence"
        );
        assert_eq!(
            preferred_material_stem(None, Some("!!!"), "9e742db7-2d52-4c02-91fb-25c8c86a1495"),
            "material-9e742d"
        );
    }

    #[test]
    fn generated_storage_paths_use_readable_stems_not_material_ids() {
        assert_eq!(
            storage_relative_path("check-repo", "py").unwrap(),
            "scripts/check-repo.py"
        );
        assert_eq!(
            storage_relative_path("quarterly-report", "pdf").unwrap(),
            "assets/quarterly-report.pdf"
        );
        assert_eq!(
            storage_relative_path("local-evidence", "md").unwrap(),
            "references/local-evidence.md"
        );
        assert_eq!(
            storage_relative_path("客户调研纪要", "md").unwrap(),
            "references/客户调研纪要.md"
        );
        assert!(storage_relative_path("skill", "md").is_err());
    }

    #[tokio::test]
    async fn renaming_skill_name_moves_the_managed_directory_without_changing_material_files() {
        let directory = tempfile::tempdir().unwrap();
        let skills_root = directory.path().join("skills");
        let old_directory = skills_root.join("old-skill");
        let references = old_directory.join("references");
        tokio::fs::create_dir_all(&references).await.unwrap();
        tokio::fs::create_dir(old_directory.join("scripts")).await.unwrap();
        tokio::fs::create_dir(old_directory.join("assets")).await.unwrap();
        tokio::fs::write(
            old_directory.join("SKILL.md"),
            "---\nname: old-skill\ndescription: Managed Workflow Profile material library.\n---\n\n# Materials\n",
        )
        .await
        .unwrap();
        tokio::fs::write(references.join("guide.md"), "# Guide\n")
            .await
            .unwrap();

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode) VALUES ('profile-a', 'Profile A', '', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO workflow_profile_skills (profile_id, skill_name) VALUES ('profile-a', 'old-skill')")
            .execute(&pool)
            .await
            .unwrap();

        let service = WorkflowMaterialsService::new(pool.clone(), skills_root.clone());
        service.set_skill_name("profile-a", "new-skill").await.unwrap();

        assert!(!old_directory.exists());
        assert_eq!(
            tokio::fs::read_to_string(skills_root.join("new-skill/references/guide.md"))
                .await
                .unwrap(),
            "# Guide\n"
        );
        assert!(
            tokio::fs::read_to_string(skills_root.join("new-skill/SKILL.md"))
                .await
                .unwrap()
                .contains("name: new-skill")
        );
        let configured: String =
            sqlx::query_scalar("SELECT skill_name FROM workflow_profile_skills WHERE profile_id = 'profile-a'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(configured, "new-skill");
    }

    #[tokio::test]
    async fn failed_skill_name_change_restores_a_directory_without_a_skill_definition() {
        let directory = tempfile::tempdir().unwrap();
        let skills_root = directory.path().join("skills");
        let old_directory = skills_root.join("old-skill");
        tokio::fs::create_dir_all(old_directory.join("references"))
            .await
            .unwrap();
        tokio::fs::create_dir(old_directory.join("scripts")).await.unwrap();
        tokio::fs::create_dir(old_directory.join("assets")).await.unwrap();
        tokio::fs::write(old_directory.join("references/guide.md"), "# Guide\n")
            .await
            .unwrap();

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let service = WorkflowMaterialsService::new(pool, skills_root.clone());
        let rename = service
            .rename_skill_directory(Some("old-skill"), "new-skill")
            .await
            .unwrap();

        rename.rollback().await.unwrap();

        assert!(old_directory.exists());
        assert!(!old_directory.join("SKILL.md").exists());
        assert_eq!(
            tokio::fs::read_to_string(old_directory.join("references/guide.md"))
                .await
                .unwrap(),
            "# Guide\n"
        );
        assert!(!skills_root.join("new-skill").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symbolic_links_are_not_accepted_as_material_files() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.md");
        let link = directory.path().join("link.md");
        tokio::fs::write(&target, "reference").await.unwrap();
        symlink(&target, &link).unwrap();

        assert!(ensure_regular_file_without_symlink(&link).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symbolic_links_are_not_accepted_as_material_directories() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let link = directory.path().join("link");
        tokio::fs::create_dir(&target).await.unwrap();
        symlink(&target, &link).unwrap();

        assert!(ensure_existing_directory_without_symlink(&link).await.is_err());
    }

    #[tokio::test]
    async fn local_file_resolution_requires_the_owning_profile_and_managed_file() {
        let directory = tempfile::tempdir().unwrap();
        let skills_root = directory.path().join("skills");
        let skill_root = skills_root.join("workflow-test");
        let references = skill_root.join("references");
        tokio::fs::create_dir_all(&references).await.unwrap();
        let file = references.join("material.md");
        tokio::fs::write(&file, "# Material").await.unwrap();

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode) VALUES ('profile-a', 'Profile A', '', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO workflow_profile_skills (profile_id, skill_name) VALUES ('profile-a', 'workflow-test')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO workflow_profile_materials (material_id, profile_id, material_revision, ordinal, title, kind, relative_path, created_at, updated_at) VALUES ('material-file', 'profile-a', 1, 0, 'Material', 'uploaded_file', 'references/material.md', 'now', 'now')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO workflow_profile_materials (material_id, profile_id, material_revision, ordinal, title, kind, external_url, created_at, updated_at) VALUES ('material-url', 'profile-a', 1, 1, 'URL', 'external_url', 'https://example.com', 'now', 'now')")
            .execute(&pool)
            .await
            .unwrap();

        let service = WorkflowMaterialsService::new(pool, skills_root);
        assert_eq!(
            service.resolve_local_file("profile-a", "material-file").await.unwrap(),
            file
        );
        assert!(matches!(
            service.resolve_local_file("profile-b", "material-file").await,
            Err(WorkflowMaterialsError::MaterialNotFound)
        ));
        assert!(matches!(
            service.resolve_local_file("profile-a", "material-url").await,
            Err(WorkflowMaterialsError::InvalidRequest(_))
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let outside = directory.path().join("outside");
            tokio::fs::create_dir(&outside).await.unwrap();
            tokio::fs::write(outside.join("material.md"), "outside").await.unwrap();
            tokio::fs::remove_file(&file).await.unwrap();
            tokio::fs::remove_dir(&references).await.unwrap();
            symlink(&outside, &references).unwrap();

            assert!(matches!(
                service.resolve_local_file("profile-a", "material-file").await,
                Err(WorkflowMaterialsError::InvalidRequest(_))
            ));
        }
    }

    #[tokio::test]
    async fn converting_a_markdown_material_requires_its_previous_file_to_be_safe_before_commit() {
        let directory = tempfile::tempdir().unwrap();
        let skills_root = directory.path().join("skills");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode) VALUES ('profile-a', 'Profile A', '', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let service = WorkflowMaterialsService::new(pool.clone(), skills_root.clone());
        let initial = service.view("profile-a").await.unwrap();
        let markdown = service
            .save(WorkflowMaterialSaveCommand {
                profile_id: "profile-a".to_string(),
                material_id: None,
                expected_material_revision: None,
                expected_materials_revision: initial.materials_revision,
                title: "Guide".to_string(),
                kind: WorkflowMaterialKind::MarkdownFile,
                external_url: None,
                markdown_content: Some("# Guide".to_string()),
            })
            .await
            .unwrap();
        let path = skills_root
            .join(initial.skill_name)
            .join(markdown.relative_path.as_deref().expect("Markdown material path"));
        tokio::fs::remove_file(&path).await.unwrap();
        tokio::fs::create_dir(&path).await.unwrap();

        let result = service
            .save(WorkflowMaterialSaveCommand {
                profile_id: "profile-a".to_string(),
                material_id: Some(markdown.material_id.clone()),
                expected_material_revision: Some(markdown.material_revision),
                expected_materials_revision: initial.materials_revision + 1,
                title: "Guide".to_string(),
                kind: WorkflowMaterialKind::ExternalUrl,
                external_url: Some("https://example.com/guide".to_string()),
                markdown_content: None,
            })
            .await;

        assert!(matches!(result, Err(WorkflowMaterialsError::InvalidRequest(_))));
        let stored_kind: String =
            sqlx::query_scalar("SELECT kind FROM workflow_profile_materials WHERE material_id = ?")
                .bind(markdown.material_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored_kind, "markdown_file");
    }
}
