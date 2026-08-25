use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
};

use sha2::{Digest, Sha256};
use sqlx::{FromRow, Pool, Sqlite, Transaction};
use uuid::Uuid;

use super::materials::{
    MAX_UPLOAD_BYTES, PackageFileDeletionLease, StagedPackageFile, StagedSkillDefinition, WorkflowMaterialsService,
    ensure_skill_name, validate_relative_path,
};
use super::projection::skill_compatibility;
use super::workflow::{
    WorkflowBindingCommand, WorkflowBindingPolicy, WorkflowSpecificationError, WorkflowSpecificationSaveCommand,
    WorkflowSpecificationService, WorkflowStepCommand, verify_workflow_profile,
};

#[path = "syntax.rs"]
mod syntax;
pub use syntax::{effective_exposure_by_name, parse_workflow_guide, render_workflow_skill};

#[doc(hidden)]
pub mod test_support {
    pub use super::syntax::{format_skill_definition, normalize_main_guide_markdown};
    pub use super::{
        ExternalGuideRow, GuideDocumentGraph, allocate_package_path, build_guide_document_graph,
        validate_package_file_command, verify_reclamation_confirmation,
    };
}

use syntax::{
    PACKAGE_FILE_REFERENCE, SIBLING_MARKDOWN_REFERENCE, UUID_REFERENCE, display_skill_name, format_parse_errors,
    format_skill_definition, normalize_main_guide_markdown,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowGuide {
    pub headings: Vec<WorkflowGuideHeading>,
    pub capabilities: Vec<WorkflowGuideCapability>,
    pub external_references: Vec<WorkflowGuideExternalReference>,
    pub package_paths: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowGuideHeading {
    pub level: u8,
    pub text: String,
    pub line: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
pub struct WorkflowGuideCapability {
    pub name: String,
    pub exposure: WorkflowBindingPolicy,
    pub guide: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
pub struct WorkflowGuideExternalReference {
    pub title: String,
    pub path: String,
    pub guide: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowGuideParseError {
    pub line: usize,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
pub struct RenderedWorkflowSkill {
    pub markdown: String,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
pub struct WorkflowGuideView {
    pub profile_id: String,
    pub guide_revision: i64,
    pub markdown: String,
    pub capabilities: Vec<WorkflowGuideCapability>,
    pub package_files: Vec<WorkflowGuidePackageFile>,
    pub documents: Vec<WorkflowGuideExternalDocument>,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
pub struct WorkflowGuidePackageFile {
    pub package_file_id: String,
    pub file_revision: i64,
    pub title: String,
    pub category: WorkflowGuidePackageCategory,
    pub relative_path: String,
    pub mime_type: Option<String>,
    pub extension: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
pub struct WorkflowGuideExternalDocument {
    pub package_file_id: String,
    pub file_revision: i64,
    pub title: String,
    pub relative_path: String,
    pub markdown: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
pub struct WorkflowGuideSaveCommand {
    pub profile_id: String,
    pub expected_guide_revision: i64,
    pub markdown: String,
    pub reclamation_confirmation: Option<WorkflowGuideReclamationConfirmation>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
pub struct WorkflowGuidePreviewCommand {
    pub profile_id: String,
    pub relative_path: Option<String>,
    pub markdown: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowGuideSaveResult {
    pub guide: WorkflowGuideView,
    pub projected_skill: RenderedWorkflowSkill,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowGuidePackageFileSaveResult {
    pub guide: WorkflowGuideView,
    pub projected_skill: RenderedWorkflowSkill,
    pub package_file: WorkflowGuidePackageFile,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowGuidePreviewResult {
    pub projected_skill: RenderedWorkflowSkill,
    pub active_document: RenderedWorkflowSkill,
    pub orphaned_package_files: Vec<WorkflowGuidePackageFile>,
    pub orphaned_capabilities: Vec<WorkflowGuideCapability>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
pub struct WorkflowGuidePackageFileSaveCommand {
    pub profile_id: String,
    pub package_file_id: Option<String>,
    pub expected_file_revision: Option<i64>,
    pub expected_guide_revision: Option<i64>,
    pub title: String,
    pub category: WorkflowGuidePackageCategory,
    pub original_filename: String,
    pub bytes: Vec<u8>,
    pub reclamation_confirmation: Option<WorkflowGuideReclamationConfirmation>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
pub struct WorkflowGuideReclamationConfirmation {
    pub package_files: Vec<WorkflowGuidePackageFileRevision>,
    pub capability_names: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
pub struct WorkflowGuidePackageFileRevision {
    pub package_file_id: String,
    pub file_revision: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Serialize)]
pub struct WorkflowGuideReclamationPlan {
    pub package_files: Vec<WorkflowGuidePackageFile>,
    pub capabilities: Vec<WorkflowGuideCapability>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowGuidePackageCategory {
    Reference,
    Script,
    Asset,
}

impl std::str::FromStr for WorkflowGuidePackageCategory {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "reference" => Ok(Self::Reference),
            "script" => Ok(Self::Script),
            "asset" => Ok(Self::Asset),
            _ => Err("invalid Workflow Guide package-file category"),
        }
    }
}

impl WorkflowGuidePackageCategory {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reference => "reference",
            Self::Script => "script",
            Self::Asset => "asset",
        }
    }

    fn directory(self) -> &'static str {
        match self {
            Self::Reference => "references",
            Self::Script => "scripts",
            Self::Asset => "assets",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkflowGuideError {
    #[error(transparent)]
    Workflow(#[from] WorkflowSpecificationError),
    #[error("invalid Workflow Guide storage: {0}")]
    InvalidStorage(String),
    #[error("Workflow Guide database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("Workflow Guide was changed by another author")]
    GuideChanged { current_guide_revision: i64 },
    #[error("Workflow Guide package file was changed by another author")]
    PackageFileChanged { current_file_revision: i64 },
    #[error("Workflow Guide save requires reclamation confirmation")]
    ReclamationConfirmationRequired(WorkflowGuideReclamationPlan),
    #[error("Workflow Guide reclamation candidates changed before confirmation")]
    ReclamationConfirmationChanged,
    #[error("Workflow Guide save committed, but Trash cleanup is pending for {relative_paths:?}: {message}")]
    TrashCleanupPending {
        relative_paths: Vec<String>,
        message: String,
    },
    #[error("Workflow Guide projection failed: {0}")]
    Projection(String),
}

#[derive(Clone)]
pub struct WorkflowGuideService {
    pool: Pool<Sqlite>,
}

impl WorkflowGuideService {
    pub fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }

    pub async fn view(
        &self,
        profile_id: &str,
    ) -> Result<WorkflowGuideView, WorkflowGuideError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        verify_workflow_profile(&mut transaction, profile_id).await?;
        ensure_guide(&mut transaction, profile_id).await?;
        let view = load_guide_view(&mut transaction, profile_id).await?;
        transaction.commit().await?;
        Ok(view)
    }

    pub async fn project(
        &self,
        profile_id: &str,
        skills_root: PathBuf,
    ) -> Result<RenderedWorkflowSkill, WorkflowGuideError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        verify_workflow_profile(&mut transaction, profile_id).await?;
        ensure_guide(&mut transaction, profile_id).await?;
        let skill_name = ensure_skill_name(&mut transaction, profile_id)
            .await
            .map_err(|error| WorkflowGuideError::Projection(error.to_string()))?;
        let registered_paths = sqlx::query_scalar::<_, String>(
            "SELECT relative_path
             FROM workflow_profile_package_files
             WHERE profile_id = ?
             UNION
             SELECT relative_path
             FROM workflow_profile_materials
             WHERE profile_id = ? AND relative_path IS NOT NULL",
        )
        .bind(profile_id)
        .bind(profile_id)
        .fetch_all(&mut *transaction)
        .await?
        .into_iter()
        .collect::<BTreeSet<_>>();
        let material_service = WorkflowMaterialsService::new(self.pool.clone(), skills_root.clone());
        let _package_guard = material_service
            .lock_skill_package(&skill_name)
            .await
            .map_err(|error| WorkflowGuideError::Projection(error.to_string()))?;
        material_service
            .recover_registered_package_file_leases(&skill_name, &registered_paths)
            .await
            .map_err(|error| WorkflowGuideError::Projection(error.to_string()))?;
        let registered_files: Vec<(String, Option<String>, bool)> = sqlx::query_as(
            "SELECT files.relative_path, files.checksum,
                    EXISTS(
                        SELECT 1 FROM workflow_profile_external_guides external
                        WHERE external.package_file_id = files.package_file_id
                          AND external.profile_id = files.profile_id
                    )
             FROM workflow_profile_package_files files WHERE files.profile_id = ?",
        )
        .bind(profile_id)
        .fetch_all(&mut *transaction)
        .await?;
        let registered_checksums = registered_files
            .iter()
            .filter_map(|(relative_path, checksum, _)| {
                checksum
                    .as_ref()
                    .map(|checksum| (relative_path.clone(), checksum.clone()))
            })
            .collect::<BTreeMap<_, _>>();
        material_service
            .recover_registered_package_file_backups(&skill_name, &registered_checksums)
            .await
            .map_err(|error| WorkflowGuideError::Projection(error.to_string()))?;
        for (relative_path, checksum, is_external_guide) in registered_files {
            if is_external_guide {
                continue;
            }
            let checksum = checksum.ok_or_else(|| {
                WorkflowGuideError::InvalidStorage(format!(
                    "registered package file '{relative_path}' has no checksum and cannot be repaired"
                ))
            })?;
            material_service
                .verify_package_file_bytes(&skill_name, &relative_path, &checksum)
                .await
                .map_err(|error| {
                    WorkflowGuideError::InvalidStorage(format!(
                        "registered package file '{relative_path}' cannot be repaired: {error}"
                    ))
                })?;
        }
        let (rendered, staged) =
            stage_projection_in_transaction(&mut transaction, profile_id, self.pool.clone(), skills_root.clone())
                .await?;
        if let Err(error) = transaction.commit().await {
            staged
                .rollback()
                .await
                .map_err(|rollback| WorkflowGuideError::Projection(rollback.to_string()))?;
            return Err(error.into());
        }
        staged.commit().await;
        if let Err(error) = material_service
            .trash_unregistered_package_files(&skill_name, &registered_paths)
            .await
        {
            return Err(WorkflowGuideError::TrashCleanupPending {
                relative_paths: vec![
                    "references/*".to_string(),
                    "scripts/*".to_string(),
                    "assets/*".to_string(),
                ],
                message: error.to_string(),
            });
        }
        Ok(rendered)
    }

    pub async fn save(
        &self,
        mut command: WorkflowGuideSaveCommand,
    ) -> Result<WorkflowGuideView, WorkflowGuideError> {
        command.markdown = normalize_main_guide_markdown(&command.markdown)?;
        let parsed = parse_workflow_guide(&command.markdown)
            .map_err(|errors| WorkflowGuideError::InvalidStorage(format_parse_errors(&errors)))?;
        validate_save_command(&command, &parsed)?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let (view, reclamation_plan) = save_in_transaction(&mut transaction, &command).await?;
        if !reclamation_plan.is_empty() {
            return Err(WorkflowGuideError::InvalidStorage(
                "confirmed reclamation requires coordinated save and projection".to_string(),
            ));
        }
        transaction.commit().await?;
        Ok(view)
    }

    /// Render the current author draft through the production projector without
    /// changing the database or managed Skill package.
    pub async fn preview(
        &self,
        mut command: WorkflowGuidePreviewCommand,
    ) -> Result<WorkflowGuidePreviewResult, WorkflowGuideError> {
        if command.relative_path.is_none() {
            command.markdown = normalize_main_guide_markdown(&command.markdown)?;
        }
        let parsed = parse_workflow_guide(&command.markdown)
            .map_err(|errors| WorkflowGuideError::InvalidStorage(format_parse_errors(&errors)))?;
        let save_shape = WorkflowGuideSaveCommand {
            profile_id: command.profile_id.clone(),
            expected_guide_revision: 0,
            markdown: command.markdown.clone(),
            reclamation_confirmation: None,
        };
        validate_save_command(&save_shape, &parsed)?;

        let mut transaction = self.pool.begin().await?;
        verify_workflow_profile(&mut transaction, &command.profile_id).await?;
        let skill_name: String =
            sqlx::query_scalar("SELECT skill_name FROM workflow_profile_skills WHERE profile_id = ?")
                .bind(&command.profile_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(|_| {
                    WorkflowGuideError::InvalidStorage("Workflow Profile has no configured skill_name".to_string())
                })?;
        let persisted = load_guide_view(&mut transaction, &command.profile_id).await?;
        let persisted_graph = load_guide_document_graph(
            &mut transaction,
            &command.profile_id,
            &persisted.markdown,
            &BTreeMap::new(),
        )
        .await?;
        let package_paths = persisted
            .package_files
            .iter()
            .map(|file| file.relative_path.as_str())
            .collect::<BTreeSet<_>>();
        for path in &parsed.package_paths {
            if !package_paths.contains(path.as_str()) {
                return Err(WorkflowGuideError::InvalidStorage(format!(
                    "Guide references unavailable package file '{path}'"
                )));
            }
        }
        let main_markdown = if command.relative_path.is_some() {
            persisted.markdown.clone()
        } else {
            command.markdown.clone()
        };
        let external_overrides = command
            .relative_path
            .as_ref()
            .map(|relative_path| BTreeMap::from([(relative_path.clone(), command.markdown.clone())]))
            .unwrap_or_default();
        let candidate_graph = load_guide_document_graph(
            &mut transaction,
            &command.profile_id,
            &main_markdown,
            &external_overrides,
        )
        .await?;
        let active_document = if let Some(relative_path) = &command.relative_path {
            let row: Option<ExternalGuideRow> = sqlx::query_as(
                "SELECT external.package_file_id, files.file_revision, files.title, files.relative_path, external.markdown
                 FROM workflow_profile_external_guides external
                 JOIN workflow_profile_package_files files ON files.package_file_id = external.package_file_id
                 WHERE external.profile_id = ? AND files.relative_path = ?",
            )
            .bind(&command.profile_id)
            .bind(relative_path)
            .fetch_optional(&mut *transaction)
            .await?;
            let Some(row) = row else {
                return Err(WorkflowGuideError::InvalidStorage(
                    "preview target is not an external Markdown document".to_string(),
                ));
            };
            candidate_graph
                .document(relative_path)
                .cloned()
                .unwrap_or(GuideDocument {
                    package_file_id: Some(row.package_file_id),
                    file_revision: Some(row.file_revision),
                    title: row.title,
                    relative_path: row.relative_path,
                    markdown: command.markdown.clone(),
                    guide: parsed.clone(),
                })
        } else {
            candidate_graph.root()?.clone()
        };
        let reclamation_plan = persisted_graph.reclamation_plan(&candidate_graph, &persisted);
        for path in &candidate_graph.combined.package_paths {
            if !package_paths.contains(path.as_str()) {
                return Err(WorkflowGuideError::InvalidStorage(format!(
                    "Guide references unavailable package file '{path}'"
                )));
            }
        }
        verify_capability_names(&mut transaction, &candidate_graph.combined.capabilities).await?;
        let profile: (String, String) = sqlx::query_as("SELECT name, description FROM profile WHERE id = ?")
            .bind(&command.profile_id)
            .fetch_one(&mut *transaction)
            .await?;
        let root = candidate_graph.root()?;
        let effective_exposure = effective_exposure_by_name(&candidate_graph.combined.capabilities);
        let server_names = load_bound_server_display_names(&mut transaction, &command.profile_id).await?;
        let compatibility = skill_compatibility(&server_names);
        let projected_skill = format_skill_definition(
            &skill_name,
            &profile.0,
            &profile.1,
            compatibility.as_deref(),
            &render_workflow_skill(&root.markdown, &root.guide, &effective_exposure).markdown,
        );
        let active_document =
            render_workflow_skill(&active_document.markdown, &active_document.guide, &effective_exposure);
        if UUID_REFERENCE.is_match(&projected_skill)
            || projected_skill.contains("skill://")
            || UUID_REFERENCE.is_match(&active_document.markdown)
            || active_document.markdown.contains("skill://")
        {
            return Err(WorkflowGuideError::InvalidStorage(
                "projected Skill contains an opaque identifier".to_string(),
            ));
        }
        transaction.commit().await?;
        Ok(WorkflowGuidePreviewResult {
            projected_skill: RenderedWorkflowSkill {
                markdown: projected_skill,
            },
            active_document,
            orphaned_package_files: reclamation_plan.package_files,
            orphaned_capabilities: reclamation_plan.capabilities,
        })
    }

    pub async fn read_external_document(
        &self,
        profile_id: &str,
        package_file_id: &str,
        skills_root: PathBuf,
    ) -> Result<WorkflowGuideExternalDocument, WorkflowGuideError> {
        loop {
            let candidate_skill_name: String =
                sqlx::query_scalar("SELECT skill_name FROM workflow_profile_skills WHERE profile_id = ?")
                    .bind(profile_id)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(|_| {
                        WorkflowGuideError::InvalidStorage("Workflow Profile has no configured skill_name".to_string())
                    })?;
            let material_service = WorkflowMaterialsService::new(self.pool.clone(), skills_root.clone());
            let package_guard = material_service
                .lock_skill_package(&candidate_skill_name)
                .await
                .map_err(|error| WorkflowGuideError::Projection(error.to_string()))?;
            let mut transaction = self.pool.begin().await?;
            verify_workflow_profile(&mut transaction, profile_id).await?;
            let locked_skill_name: String =
                sqlx::query_scalar("SELECT skill_name FROM workflow_profile_skills WHERE profile_id = ?")
                    .bind(profile_id)
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(|_| {
                        WorkflowGuideError::InvalidStorage("Workflow Profile has no configured skill_name".to_string())
                    })?;
            if locked_skill_name != candidate_skill_name {
                transaction.rollback().await?;
                drop(package_guard);
                continue;
            }
            let file: Option<PackageFileRow> = sqlx::query_as(
                "SELECT package_file_id, file_revision, title, category, relative_path, mime_type, extension, file_size, checksum
                 FROM workflow_profile_package_files
                 WHERE profile_id = ? AND package_file_id = ?",
            )
            .bind(profile_id)
            .bind(package_file_id)
            .fetch_optional(&mut *transaction)
            .await?;
            let Some(file) = file else {
                return Err(WorkflowGuideError::InvalidStorage(
                    "external Markdown document was not found".to_string(),
                ));
            };
            if file.category != WorkflowGuidePackageCategory::Reference.as_str()
                || file.extension.as_deref() != Some("md")
                || !file.relative_path.starts_with("references/")
            {
                return Err(WorkflowGuideError::InvalidStorage(
                    "package file is not an external Markdown document".to_string(),
                ));
            }
            let markdown: Option<String> = sqlx::query_scalar(
                "SELECT markdown FROM workflow_profile_external_guides
                 WHERE package_file_id = ? AND profile_id = ?",
            )
            .bind(package_file_id)
            .bind(profile_id)
            .fetch_optional(&mut *transaction)
            .await?;
            let Some(markdown) = markdown else {
                return Err(WorkflowGuideError::InvalidStorage(
                    "external Markdown document has no managed Guide source".to_string(),
                ));
            };
            let checksum = file.checksum.as_deref().ok_or_else(|| {
                WorkflowGuideError::InvalidStorage("external Markdown document has no registered checksum".to_string())
            })?;
            transaction.commit().await?;
            let _ = material_service
                .read_package_file_text(&locked_skill_name, &file.relative_path, checksum)
                .await
                .map_err(|error| WorkflowGuideError::Projection(error.to_string()))?;
            drop(package_guard);
            return Ok(WorkflowGuideExternalDocument {
                package_file_id: file.package_file_id,
                file_revision: file.file_revision,
                title: file.title,
                relative_path: file.relative_path,
                markdown,
            });
        }
    }

    pub async fn save_and_project(
        &self,
        mut command: WorkflowGuideSaveCommand,
        skills_root: PathBuf,
    ) -> Result<WorkflowGuideSaveResult, WorkflowGuideError> {
        command.markdown = normalize_main_guide_markdown(&command.markdown)?;
        let parsed = parse_workflow_guide(&command.markdown)
            .map_err(|errors| WorkflowGuideError::InvalidStorage(format_parse_errors(&errors)))?;
        validate_save_command(&command, &parsed)?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let (guide, reclamation_plan) = save_in_transaction(&mut transaction, &command).await?;
        let skill_name = ensure_skill_name(&mut transaction, &command.profile_id)
            .await
            .map_err(|error| WorkflowGuideError::Projection(error.to_string()))?;
        let material_service = WorkflowMaterialsService::new(self.pool.clone(), skills_root.clone());
        let _package_guard = material_service
            .lock_skill_package(&skill_name)
            .await
            .map_err(|error| WorkflowGuideError::Projection(error.to_string()))?;
        let reclamation = stage_reclamation_leases(&material_service, &skill_name, &reclamation_plan).await?;
        let (projected_skill, staged) = match stage_projection_in_transaction(
            &mut transaction,
            &command.profile_id,
            self.pool.clone(),
            skills_root,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                rollback_save_artifacts(None, None, reclamation).await?;
                return Err(error);
            }
        };
        if let Err(error) = transaction.commit().await {
            rollback_save_artifacts(Some(staged), None, reclamation).await?;
            return Err(error.into());
        }
        staged.commit().await;
        move_reclamation_leases_to_trash(reclamation).await?;
        Ok(WorkflowGuideSaveResult { guide, projected_skill })
    }

    pub async fn save_package_file_and_project(
        &self,
        command: WorkflowGuidePackageFileSaveCommand,
        skills_root: PathBuf,
    ) -> Result<WorkflowGuidePackageFileSaveResult, WorkflowGuideError> {
        validate_package_file_command(&command)?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        verify_workflow_profile(&mut transaction, &command.profile_id).await?;
        ensure_guide(&mut transaction, &command.profile_id).await?;
        let extension = package_extension(&command.original_filename)?;
        let is_external_guide = command.category == WorkflowGuidePackageCategory::Reference && extension == "md";
        let expected_guide_revision = if is_external_guide {
            let expected_guide_revision = command.expected_guide_revision.ok_or_else(|| {
                WorkflowGuideError::InvalidStorage("external Markdown save requires a Guide revision".to_string())
            })?;
            verify_guide_revision(&mut transaction, &command.profile_id, expected_guide_revision).await?;
            Some(expected_guide_revision)
        } else {
            None
        };
        let persisted_state = if is_external_guide {
            let persisted = load_guide_view(&mut transaction, &command.profile_id).await?;
            let graph = load_guide_document_graph(
                &mut transaction,
                &command.profile_id,
                &persisted.markdown,
                &BTreeMap::new(),
            )
            .await?;
            Some((persisted, graph))
        } else {
            None
        };
        let skill_name = ensure_skill_name(&mut transaction, &command.profile_id)
            .await
            .map_err(|error| WorkflowGuideError::Projection(error.to_string()))?;
        let mime_type = package_mime_type(&extension);
        let checksum = format!("{:x}", Sha256::digest(&command.bytes));
        let (package_file_id, relative_path) = match command.package_file_id.as_deref() {
            Some(package_file_id) => {
                let current: Option<(String, i64, String, String)> = sqlx::query_as(
                    "SELECT profile_id, file_revision, relative_path, category
                     FROM workflow_profile_package_files WHERE package_file_id = ?",
                )
                .bind(package_file_id)
                .fetch_optional(&mut *transaction)
                .await?;
                let Some((profile_id, file_revision, relative_path, category)) = current else {
                    return Err(WorkflowGuideError::InvalidStorage(
                        "package file was not found".to_string(),
                    ));
                };
                if profile_id != command.profile_id {
                    return Err(WorkflowGuideError::InvalidStorage(
                        "package file was not found".to_string(),
                    ));
                }
                if command.expected_file_revision != Some(file_revision) {
                    return Err(WorkflowGuideError::PackageFileChanged {
                        current_file_revision: file_revision,
                    });
                }
                if category != command.category.as_str() || !relative_path.ends_with(&format!(".{extension}")) {
                    return Err(WorkflowGuideError::InvalidStorage(
                        "replacing a package file cannot change its category or extension".to_string(),
                    ));
                }
                sqlx::query(
                    "UPDATE workflow_profile_package_files
                     SET file_revision = file_revision + 1, title = ?, category = ?, mime_type = ?,
                         extension = ?, file_size = ?, checksum = ?,
                         updated_at = CURRENT_TIMESTAMP
                     WHERE package_file_id = ?",
                )
                .bind(&command.title)
                .bind(command.category.as_str())
                .bind(mime_type)
                .bind(&extension)
                .bind(command.bytes.len() as i64)
                .bind(&checksum)
                .bind(package_file_id)
                .execute(&mut *transaction)
                .await?;
                (package_file_id.to_string(), relative_path)
            }
            None => {
                let package_file_id = Uuid::new_v4().to_string();
                let relative_path = allocate_package_path(
                    &mut transaction,
                    &command.profile_id,
                    command.category,
                    &command.title,
                    &extension,
                )
                .await?;
                let ordinal: i64 = sqlx::query_scalar(
                    "SELECT COALESCE(MAX(ordinal), -1) + 1
                     FROM workflow_profile_package_files WHERE profile_id = ?",
                )
                .bind(&command.profile_id)
                .fetch_one(&mut *transaction)
                .await?;
                sqlx::query(
                    "INSERT INTO workflow_profile_package_files (
                        package_file_id, profile_id, ordinal, title, category, relative_path,
                        mime_type, extension, file_size, checksum
                     ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&package_file_id)
                .bind(&command.profile_id)
                .bind(ordinal)
                .bind(&command.title)
                .bind(command.category.as_str())
                .bind(&relative_path)
                .bind(mime_type)
                .bind(&extension)
                .bind(command.bytes.len() as i64)
                .bind(&checksum)
                .execute(&mut *transaction)
                .await?;
                (package_file_id, relative_path)
            }
        };
        let mut external_document_is_reachable = false;
        let mut reclamation_plan = WorkflowGuideReclamationPlan {
            package_files: Vec::new(),
            capabilities: Vec::new(),
        };
        if is_external_guide {
            let expected_guide_revision = expected_guide_revision.ok_or_else(|| {
                WorkflowGuideError::InvalidStorage("external Markdown save requires a Guide revision".to_string())
            })?;
            let markdown = String::from_utf8(command.bytes.clone()).map_err(|_| {
                WorkflowGuideError::InvalidStorage("external Markdown document must be valid UTF-8".to_string())
            })?;
            parse_workflow_guide(&markdown)
                .map_err(|errors| WorkflowGuideError::InvalidStorage(format_parse_errors(&errors)))?;
            sqlx::query(
                "INSERT INTO workflow_profile_external_guides (package_file_id, profile_id, markdown)
                 VALUES (?, ?, ?)
                 ON CONFLICT(package_file_id) DO UPDATE SET markdown = excluded.markdown, updated_at = CURRENT_TIMESTAMP",
            )
            .bind(&package_file_id)
            .bind(&command.profile_id)
            .bind(&markdown)
            .execute(&mut *transaction)
            .await?;
            let main_markdown: String =
                sqlx::query_scalar("SELECT markdown FROM workflow_profile_guides WHERE profile_id = ?")
                    .bind(&command.profile_id)
                    .fetch_one(&mut *transaction)
                    .await?;
            let graph =
                load_guide_document_graph(&mut transaction, &command.profile_id, &main_markdown, &BTreeMap::new())
                    .await?;
            let (persisted, persisted_graph) = persisted_state
                .as_ref()
                .expect("external Markdown saves load the persisted document graph");
            reclamation_plan = persisted_graph.reclamation_plan(&graph, persisted);
            verify_reclamation_confirmation(&reclamation_plan, command.reclamation_confirmation.as_ref())?;
            apply_reclamation_metadata(&mut transaction, &command.profile_id, &reclamation_plan).await?;
            external_document_is_reachable = graph.contains_package_file(&package_file_id);
            if external_document_is_reachable {
                verify_capability_names(&mut transaction, &graph.combined.capabilities).await?;
                verify_package_paths(&mut transaction, &command.profile_id, &graph.combined.package_paths).await?;
                synchronize_workflow_specification(&mut transaction, &command.profile_id, &graph.combined.capabilities)
                    .await?;
                bump_guide_revision(&mut transaction, &command.profile_id, expected_guide_revision).await?;
            }
        }
        let material_service = WorkflowMaterialsService::new(self.pool.clone(), skills_root.clone());
        let _package_guard = material_service
            .lock_skill_package(&skill_name)
            .await
            .map_err(|error| WorkflowGuideError::Projection(error.to_string()))?;
        let reclamation = stage_reclamation_leases(&material_service, &skill_name, &reclamation_plan).await?;
        let package_file_is_registered: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM workflow_profile_package_files
                WHERE profile_id = ? AND package_file_id = ?
             )",
        )
        .bind(&command.profile_id)
        .bind(&package_file_id)
        .fetch_one(&mut *transaction)
        .await?;
        let staged_file = if (is_external_guide && external_document_is_reachable) || !package_file_is_registered {
            None
        } else {
            match material_service
                .stage_package_file_bytes(&skill_name, &relative_path, &command.bytes)
                .await
            {
                Ok(staged) => Some(staged),
                Err(error) => {
                    rollback_reclamation_leases(reclamation).await?;
                    return Err(WorkflowGuideError::Projection(error.to_string()));
                }
            }
        };
        let (projected_skill, projection) = match stage_projection_in_transaction(
            &mut transaction,
            &command.profile_id,
            self.pool.clone(),
            skills_root,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                rollback_save_artifacts(None, staged_file, reclamation).await?;
                return Err(error);
            }
        };
        let guide = match load_guide_view(&mut transaction, &command.profile_id).await {
            Ok(guide) => guide,
            Err(error) => {
                rollback_save_artifacts(Some(projection), staged_file, reclamation).await?;
                return Err(error);
            }
        };
        let Some(package_file) = guide
            .package_files
            .iter()
            .find(|file| file.package_file_id == package_file_id)
            .cloned()
        else {
            rollback_save_artifacts(Some(projection), staged_file, reclamation).await?;
            return Err(WorkflowGuideError::InvalidStorage(
                "saved package file was not returned from persistence".to_string(),
            ));
        };
        if let Err(error) = transaction.commit().await {
            rollback_save_artifacts(Some(projection), staged_file, reclamation).await?;
            return Err(error.into());
        }
        projection.commit().await;
        if let Some(staged_file) = staged_file {
            staged_file.commit().await;
        }
        move_reclamation_leases_to_trash(reclamation).await?;
        Ok(WorkflowGuidePackageFileSaveResult {
            guide,
            projected_skill,
            package_file,
        })
    }

    pub async fn delete_package_file_and_project(
        &self,
        profile_id: &str,
        package_file_id: &str,
        expected_file_revision: i64,
        expected_guide_revision: i64,
        skills_root: PathBuf,
    ) -> Result<WorkflowGuideSaveResult, WorkflowGuideError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        verify_workflow_profile(&mut transaction, profile_id).await?;
        ensure_guide(&mut transaction, profile_id).await?;
        verify_guide_revision(&mut transaction, profile_id, expected_guide_revision).await?;
        let skill_name = ensure_skill_name(&mut transaction, profile_id)
            .await
            .map_err(|error| WorkflowGuideError::Projection(error.to_string()))?;
        let current: Option<(String, i64, String)> = sqlx::query_as(
            "SELECT profile_id, file_revision, relative_path
             FROM workflow_profile_package_files WHERE package_file_id = ?",
        )
        .bind(package_file_id)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some((current_profile_id, file_revision, relative_path)) = current else {
            return Err(WorkflowGuideError::InvalidStorage(
                "package file was not found".to_string(),
            ));
        };
        if current_profile_id != profile_id {
            return Err(WorkflowGuideError::InvalidStorage(
                "package file was not found".to_string(),
            ));
        }
        if file_revision != expected_file_revision {
            return Err(WorkflowGuideError::PackageFileChanged {
                current_file_revision: file_revision,
            });
        }
        let markdown: String = sqlx::query_scalar("SELECT markdown FROM workflow_profile_guides WHERE profile_id = ?")
            .bind(profile_id)
            .fetch_one(&mut *transaction)
            .await?;
        let graph = load_guide_document_graph(&mut transaction, profile_id, &markdown, &BTreeMap::new()).await?;
        if graph.documents.iter().any(|document| {
            document.package_file_id.as_deref() != Some(package_file_id)
                && document.guide.package_paths.contains(&relative_path)
        }) {
            return Err(WorkflowGuideError::InvalidStorage(
                "remove the package-file link from every Guide document before deleting the file".to_string(),
            ));
        }
        sqlx::query("DELETE FROM workflow_profile_package_files WHERE package_file_id = ?")
            .bind(package_file_id)
            .execute(&mut *transaction)
            .await?;
        bump_guide_revision(&mut transaction, profile_id, expected_guide_revision).await?;
        let material_service = WorkflowMaterialsService::new(self.pool.clone(), skills_root.clone());
        let _package_guard = material_service
            .lock_skill_package(&skill_name)
            .await
            .map_err(|error| WorkflowGuideError::Projection(error.to_string()))?;
        let mut deletion_lease = material_service
            .stage_package_file_deletion_lease(&skill_name, &relative_path)
            .await
            .map_err(|error| WorkflowGuideError::Projection(error.to_string()))?;
        let (projected_skill, projection) =
            match stage_projection_in_transaction(&mut transaction, profile_id, self.pool.clone(), skills_root).await {
                Ok(result) => result,
                Err(error) => {
                    rollback_projection_and_deletion_lease(None, deletion_lease).await?;
                    return Err(error);
                }
            };
        let guide = match load_guide_view(&mut transaction, profile_id).await {
            Ok(guide) => guide,
            Err(error) => {
                rollback_projection_and_deletion_lease(Some(projection), deletion_lease).await?;
                return Err(error);
            }
        };
        if let Err(error) = transaction.commit().await {
            rollback_projection_and_deletion_lease(Some(projection), deletion_lease).await?;
            return Err(error.into());
        }
        projection.commit().await;
        if let Err(error) = deletion_lease.move_to_trash().await {
            deletion_lease
                .rollback()
                .await
                .map_err(|rollback| WorkflowGuideError::TrashCleanupPending {
                    relative_paths: vec![relative_path.clone()],
                    message: format!("{error}; lease rollback also failed: {rollback}"),
                })?;
            return Err(WorkflowGuideError::TrashCleanupPending {
                relative_paths: vec![relative_path],
                message: error.to_string(),
            });
        }
        Ok(WorkflowGuideSaveResult { guide, projected_skill })
    }
}

#[derive(FromRow)]
struct GuideRow {
    guide_revision: i64,
    markdown: String,
}

#[derive(FromRow)]
struct PackageFileRow {
    package_file_id: String,
    file_revision: i64,
    title: String,
    category: String,
    relative_path: String,
    mime_type: Option<String>,
    extension: Option<String>,
    file_size: Option<i64>,
    checksum: Option<String>,
}

#[derive(FromRow, serde::Serialize)]
struct ProjectionPackageFile {
    relative_path: String,
    file_revision: i64,
    title: String,
    category: String,
    mime_type: Option<String>,
    extension: Option<String>,
    file_size: Option<i64>,
    checksum: Option<String>,
}

#[doc(hidden)]
#[derive(Clone, FromRow)]
pub struct ExternalGuideRow {
    pub package_file_id: String,
    pub file_revision: i64,
    pub title: String,
    pub relative_path: String,
    pub markdown: String,
}

#[doc(hidden)]
#[derive(Clone)]
pub struct GuideDocument {
    pub package_file_id: Option<String>,
    pub file_revision: Option<i64>,
    pub title: String,
    pub relative_path: String,
    pub markdown: String,
    pub guide: WorkflowGuide,
}

#[doc(hidden)]
pub struct GuideDocumentGraph {
    pub documents: Vec<GuideDocument>,
    pub combined: WorkflowGuide,
}

impl GuideDocumentGraph {
    fn root(&self) -> Result<&GuideDocument, WorkflowGuideError> {
        self.documents
            .first()
            .ok_or_else(|| WorkflowGuideError::InvalidStorage("Workflow Guide is missing".to_string()))
    }

    fn document(
        &self,
        relative_path: &str,
    ) -> Option<&GuideDocument> {
        self.documents
            .iter()
            .find(|document| document.relative_path == relative_path)
    }

    fn contains_package_file(
        &self,
        package_file_id: &str,
    ) -> bool {
        self.documents
            .iter()
            .any(|document| document.package_file_id.as_deref() == Some(package_file_id))
    }

    pub fn orphaned_package_files(
        &self,
        candidate: &Self,
        package_files: &[WorkflowGuidePackageFile],
    ) -> Vec<WorkflowGuidePackageFile> {
        package_files
            .iter()
            .filter(|file| {
                self.combined.package_paths.contains(&file.relative_path)
                    && !candidate.combined.package_paths.contains(&file.relative_path)
            })
            .cloned()
            .collect()
    }

    fn reclamation_plan(
        &self,
        candidate: &Self,
        persisted: &WorkflowGuideView,
    ) -> WorkflowGuideReclamationPlan {
        WorkflowGuideReclamationPlan {
            package_files: self.orphaned_package_files(candidate, &persisted.package_files),
            capabilities: persisted
                .capabilities
                .iter()
                .filter(|capability| {
                    self.combined
                        .capabilities
                        .iter()
                        .any(|item| item.name == capability.name)
                        && !candidate
                            .combined
                            .capabilities
                            .iter()
                            .any(|item| item.name == capability.name)
                })
                .fold(
                    BTreeMap::<String, WorkflowGuideCapability>::new(),
                    |mut unique, capability| {
                        unique
                            .entry(capability.name.clone())
                            .or_insert_with(|| capability.clone());
                        unique
                    },
                )
                .into_values()
                .collect(),
        }
    }
}

impl WorkflowGuideReclamationPlan {
    fn is_empty(&self) -> bool {
        self.package_files.is_empty() && self.capabilities.is_empty()
    }
}

pub(crate) struct StagedWorkflowProjection {
    skill_definition: StagedSkillDefinition,
    package_files: Vec<StagedPackageFile>,
}

struct StagedReclamation {
    relative_path: String,
    lease: PackageFileDeletionLease,
}

async fn stage_reclamation_leases(
    material_service: &WorkflowMaterialsService,
    skill_name: &str,
    plan: &WorkflowGuideReclamationPlan,
) -> Result<Vec<StagedReclamation>, WorkflowGuideError> {
    let mut staged = Vec::new();
    for file in &plan.package_files {
        match material_service
            .stage_package_file_deletion_lease(skill_name, &file.relative_path)
            .await
        {
            Ok(lease) => staged.push(StagedReclamation {
                relative_path: file.relative_path.clone(),
                lease,
            }),
            Err(error) => {
                rollback_reclamation_leases(staged).await?;
                return Err(WorkflowGuideError::Projection(error.to_string()));
            }
        }
    }
    Ok(staged)
}

async fn rollback_reclamation_leases(staged: Vec<StagedReclamation>) -> Result<(), WorkflowGuideError> {
    let mut errors = Vec::new();
    for staged_file in staged.into_iter().rev() {
        if let Err(error) = staged_file.lease.rollback().await {
            errors.push(error.to_string());
        }
    }
    if !errors.is_empty() {
        return Err(WorkflowGuideError::Projection(errors.join("; ")));
    }
    Ok(())
}

async fn rollback_save_artifacts(
    projection: Option<StagedWorkflowProjection>,
    staged_file: Option<StagedPackageFile>,
    reclamation: Vec<StagedReclamation>,
) -> Result<(), WorkflowGuideError> {
    let mut errors = Vec::new();
    if let Some(projection) = projection
        && let Err(error) = projection.rollback().await
    {
        errors.push(error.to_string());
    }
    if let Some(staged_file) = staged_file
        && let Err(error) = staged_file.rollback().await
    {
        errors.push(error.to_string());
    }
    if let Err(error) = rollback_reclamation_leases(reclamation).await {
        errors.push(error.to_string());
    }
    if !errors.is_empty() {
        return Err(WorkflowGuideError::Projection(errors.join("; ")));
    }
    Ok(())
}

async fn rollback_projection_and_deletion_lease(
    projection: Option<StagedWorkflowProjection>,
    deletion_lease: PackageFileDeletionLease,
) -> Result<(), WorkflowGuideError> {
    let mut errors = Vec::new();
    if let Some(projection) = projection
        && let Err(error) = projection.rollback().await
    {
        errors.push(error.to_string());
    }
    if let Err(error) = deletion_lease.rollback().await {
        errors.push(error.to_string());
    }
    if !errors.is_empty() {
        return Err(WorkflowGuideError::Projection(errors.join("; ")));
    }
    Ok(())
}

async fn move_reclamation_leases_to_trash(staged: Vec<StagedReclamation>) -> Result<(), WorkflowGuideError> {
    let mut pending = VecDeque::from(staged);
    while let Some(mut staged_file) = pending.pop_front() {
        if let Err(error) = staged_file.lease.move_to_trash().await {
            let mut cleanup_pending = vec![staged_file];
            cleanup_pending.extend(pending);
            let relative_paths = cleanup_pending
                .iter()
                .map(|item| item.relative_path.clone())
                .collect::<Vec<_>>();
            let rollback_error = rollback_reclamation_leases(cleanup_pending).await.err();
            let message = match rollback_error {
                Some(rollback_error) => format!("{error}; lease rollback also failed: {rollback_error}"),
                None => error.to_string(),
            };
            return Err(WorkflowGuideError::TrashCleanupPending {
                relative_paths,
                message,
            });
        }
    }
    Ok(())
}

impl StagedWorkflowProjection {
    pub(crate) async fn commit(self) {
        self.skill_definition.commit().await;
        for package_file in self.package_files {
            package_file.commit().await;
        }
    }

    pub(crate) async fn rollback(self) -> Result<(), WorkflowGuideError> {
        let mut errors = Vec::new();
        if let Err(error) = self.skill_definition.rollback().await {
            errors.push(error.to_string());
        }
        for package_file in self.package_files {
            if let Err(error) = package_file.rollback().await {
                errors.push(error.to_string());
            }
        }
        if !errors.is_empty() {
            return Err(WorkflowGuideError::Projection(errors.join("; ")));
        }
        Ok(())
    }
}

async fn save_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &WorkflowGuideSaveCommand,
) -> Result<(WorkflowGuideView, WorkflowGuideReclamationPlan), WorkflowGuideError> {
    verify_workflow_profile(transaction, &command.profile_id).await?;
    ensure_guide(transaction, &command.profile_id).await?;
    verify_guide_revision(transaction, &command.profile_id, command.expected_guide_revision).await?;
    let persisted = load_guide_view(transaction, &command.profile_id).await?;
    let persisted_graph =
        load_guide_document_graph(transaction, &command.profile_id, &persisted.markdown, &BTreeMap::new()).await?;
    let graph =
        load_guide_document_graph(transaction, &command.profile_id, &command.markdown, &BTreeMap::new()).await?;
    let reclamation_plan = persisted_graph.reclamation_plan(&graph, &persisted);
    verify_reclamation_confirmation(&reclamation_plan, command.reclamation_confirmation.as_ref())?;
    apply_reclamation_metadata(transaction, &command.profile_id, &reclamation_plan).await?;
    verify_capability_names(transaction, &graph.combined.capabilities).await?;
    verify_package_paths(transaction, &command.profile_id, &graph.combined.package_paths).await?;
    synchronize_workflow_specification(transaction, &command.profile_id, &graph.combined.capabilities).await?;
    let changed = sqlx::query(
        "UPDATE workflow_profile_guides
         SET markdown = ?, guide_revision = guide_revision + 1, updated_at = CURRENT_TIMESTAMP
         WHERE profile_id = ? AND guide_revision = ?",
    )
    .bind(&command.markdown)
    .bind(&command.profile_id)
    .bind(command.expected_guide_revision)
    .execute(&mut **transaction)
    .await?;
    if changed.rows_affected() != 1 {
        return Err(load_guide_conflict(transaction, &command.profile_id).await?);
    }
    let view = load_guide_view(transaction, &command.profile_id).await?;
    Ok((view, reclamation_plan))
}

#[doc(hidden)]
pub fn verify_reclamation_confirmation(
    plan: &WorkflowGuideReclamationPlan,
    confirmation: Option<&WorkflowGuideReclamationConfirmation>,
) -> Result<(), WorkflowGuideError> {
    if plan.is_empty() {
        return match confirmation {
            None => Ok(()),
            Some(confirmation) if confirmation.package_files.is_empty() && confirmation.capability_names.is_empty() => {
                Ok(())
            }
            Some(_) => Err(WorkflowGuideError::ReclamationConfirmationChanged),
        };
    }
    let Some(confirmation) = confirmation else {
        return Err(WorkflowGuideError::ReclamationConfirmationRequired(plan.clone()));
    };
    let expected_files = plan
        .package_files
        .iter()
        .map(|file| (file.package_file_id.as_str(), file.file_revision))
        .collect::<BTreeSet<_>>();
    let confirmed_files = confirmation
        .package_files
        .iter()
        .map(|file| (file.package_file_id.as_str(), file.file_revision))
        .collect::<BTreeSet<_>>();
    let expected_names = plan
        .capabilities
        .iter()
        .map(|capability| capability.name.as_str())
        .collect::<BTreeSet<_>>();
    let confirmed_names = confirmation
        .capability_names
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if expected_files != confirmed_files
        || expected_names != confirmed_names
        || confirmed_files.len() != confirmation.package_files.len()
        || confirmed_names.len() != confirmation.capability_names.len()
    {
        return Err(WorkflowGuideError::ReclamationConfirmationChanged);
    }
    Ok(())
}

async fn apply_reclamation_metadata(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    plan: &WorkflowGuideReclamationPlan,
) -> Result<(), WorkflowGuideError> {
    for file in &plan.package_files {
        let deleted = sqlx::query(
            "DELETE FROM workflow_profile_package_files
             WHERE profile_id = ? AND package_file_id = ? AND file_revision = ?",
        )
        .bind(profile_id)
        .bind(&file.package_file_id)
        .bind(file.file_revision)
        .execute(&mut **transaction)
        .await?;
        if deleted.rows_affected() != 1 {
            return Err(WorkflowGuideError::ReclamationConfirmationChanged);
        }
    }
    Ok(())
}

async fn synchronize_workflow_specification(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    capabilities: &[WorkflowGuideCapability],
) -> Result<(), WorkflowGuideError> {
    let capability_refs = load_canonical_capability_refs(transaction).await?;
    let specification: Option<(i64, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT specification_revision, validation_notes, avoid_rules
         FROM workflow_profile_specifications WHERE profile_id = ?",
    )
    .bind(profile_id)
    .fetch_optional(&mut **transaction)
    .await?;
    let (specification_revision, validation_notes, avoid_rules) = match specification {
        Some((revision, validation_notes, avoid_rules)) => (Some(revision), validation_notes, avoid_rules),
        None => (None, None, None),
    };
    let specification = WorkflowSpecificationService::save_in_transaction(
        transaction,
        WorkflowSpecificationSaveCommand {
            profile_id: profile_id.to_string(),
            expected_specification_revision: specification_revision,
            validation_notes,
            avoid_rules,
            steps: capabilities
                .iter()
                .map(|capability| {
                    let ref_id = capability_refs.get(&capability.name).ok_or_else(|| {
                        WorkflowGuideError::InvalidStorage(format!(
                            "Guide references unavailable canonical capability name '{}'",
                            capability.name
                        ))
                    })?;
                    Ok(WorkflowStepCommand {
                        step_id: None,
                        title: capability.name.clone(),
                        description: (!capability.guide.is_empty()).then(|| capability.guide.clone()),
                        bindings: vec![WorkflowBindingCommand {
                            ref_id: ref_id.clone(),
                            binding_policy: capability.exposure,
                        }],
                    })
                })
                .collect::<Result<Vec<_>, WorkflowGuideError>>()?,
        },
    )
    .await?;
    let _ = specification;
    Ok(())
}

async fn load_guide_document_graph(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    main_markdown: &str,
    external_overrides: &BTreeMap<String, String>,
) -> Result<GuideDocumentGraph, WorkflowGuideError> {
    let external_rows = sqlx::query_as::<_, ExternalGuideRow>(
        "SELECT external.package_file_id, files.file_revision, files.title, files.relative_path, external.markdown
         FROM workflow_profile_external_guides external
         JOIN workflow_profile_package_files files ON files.package_file_id = external.package_file_id
         WHERE external.profile_id = ? ORDER BY files.ordinal",
    )
    .bind(profile_id)
    .fetch_all(&mut **transaction)
    .await?;
    build_guide_document_graph(main_markdown, external_rows, external_overrides)
}

#[doc(hidden)]
pub fn build_guide_document_graph(
    main_markdown: &str,
    external_rows: Vec<ExternalGuideRow>,
    external_overrides: &BTreeMap<String, String>,
) -> Result<GuideDocumentGraph, WorkflowGuideError> {
    let main_markdown = normalize_main_guide_markdown(main_markdown)?;
    let root = GuideDocument {
        package_file_id: None,
        file_revision: None,
        title: "SKILL.md".to_string(),
        relative_path: "SKILL.md".to_string(),
        guide: parse_workflow_guide(&main_markdown)
            .map_err(|errors| WorkflowGuideError::InvalidStorage(format_parse_errors(&errors)))?,
        markdown: main_markdown,
    };
    let external_by_path = external_rows
        .into_iter()
        .map(|row| (row.relative_path.clone(), row))
        .collect::<BTreeMap<_, _>>();
    let mut pending_paths = root.guide.package_paths.iter().cloned().collect::<VecDeque<_>>();
    let mut visited_paths = BTreeSet::new();
    let mut documents = vec![root];

    while let Some(path) = pending_paths.pop_front() {
        if !visited_paths.insert(path.clone()) {
            continue;
        }
        let Some(row) = external_by_path.get(&path) else {
            continue;
        };
        let markdown = external_overrides
            .get(&row.relative_path)
            .cloned()
            .unwrap_or_else(|| row.markdown.clone());
        let mut guide = parse_workflow_guide(&markdown).map_err(|errors| {
            WorkflowGuideError::InvalidStorage(format!("{}: {}", row.relative_path, format_parse_errors(&errors)))
        })?;
        guide
            .package_paths
            .extend(resolve_sibling_markdown_paths(&row.relative_path, &markdown)?);
        pending_paths.extend(guide.package_paths.iter().cloned());
        documents.push(GuideDocument {
            package_file_id: Some(row.package_file_id.clone()),
            file_revision: Some(row.file_revision),
            title: row.title.clone(),
            relative_path: row.relative_path.clone(),
            markdown,
            guide,
        });
    }
    let mut combined = WorkflowGuide {
        headings: Vec::new(),
        capabilities: Vec::new(),
        external_references: Vec::new(),
        package_paths: BTreeSet::new(),
    };
    for document in &documents {
        combined.headings.extend(document.guide.headings.clone());
        combined.package_paths.extend(document.guide.package_paths.clone());
    }
    let documents_by_path = documents
        .iter()
        .map(|document| (document.relative_path.as_str(), document))
        .collect::<BTreeMap<_, _>>();
    collect_capabilities_in_recursive_order(
        "SKILL.md",
        &documents_by_path,
        &mut BTreeSet::new(),
        &mut combined.capabilities,
    )?;
    Ok(GuideDocumentGraph { documents, combined })
}

#[derive(Clone)]
enum GuideOrderEvent {
    Capability(WorkflowGuideCapability),
    ExternalDocument(String),
}

fn collect_capabilities_in_recursive_order(
    relative_path: &str,
    documents: &BTreeMap<&str, &GuideDocument>,
    visited: &mut BTreeSet<String>,
    capabilities: &mut Vec<WorkflowGuideCapability>,
) -> Result<(), WorkflowGuideError> {
    if !visited.insert(relative_path.to_string()) {
        return Ok(());
    }
    let Some(document) = documents.get(relative_path) else {
        return Ok(());
    };
    let line_offsets = line_offsets(&document.markdown);
    let mut events = document
        .guide
        .capabilities
        .iter()
        .cloned()
        .map(|capability| {
            (
                line_offsets
                    .get(capability.start_line.saturating_sub(1))
                    .copied()
                    .unwrap_or(usize::MAX),
                GuideOrderEvent::Capability(capability),
            )
        })
        .collect::<Vec<_>>();
    events.extend(document.guide.external_references.iter().cloned().map(|external| {
        (
            line_offsets
                .get(external.start_line.saturating_sub(1))
                .copied()
                .unwrap_or(usize::MAX),
            GuideOrderEvent::ExternalDocument(external.path),
        )
    }));
    events.extend(
        ordered_external_markdown_references(&document.relative_path, &document.markdown)?
            .into_iter()
            .map(|(offset, path)| (offset, GuideOrderEvent::ExternalDocument(path))),
    );
    events.sort_by_key(|(offset, _)| *offset);
    for (_, event) in events {
        match event {
            GuideOrderEvent::Capability(capability) => capabilities.push(capability),
            GuideOrderEvent::ExternalDocument(path) => {
                collect_capabilities_in_recursive_order(&path, documents, visited, capabilities)?;
            }
        }
    }
    Ok(())
}

fn line_offsets(markdown: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (index, byte) in markdown.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(index + 1);
        }
    }
    offsets
}

fn ordered_external_markdown_references(
    source_path: &str,
    markdown: &str,
) -> Result<Vec<(usize, String)>, WorkflowGuideError> {
    let mut references = PACKAGE_FILE_REFERENCE
        .captures_iter(markdown)
        .filter_map(|captures| {
            let path = captures.get(1)?.as_str();
            path.ends_with(".md").then(|| {
                (
                    captures.get(0).map_or(usize::MAX, |value| value.start()),
                    path.to_string(),
                )
            })
        })
        .collect::<Vec<_>>();
    if source_path != "SKILL.md" {
        let parent = std::path::Path::new(source_path)
            .parent()
            .and_then(|path| path.to_str())
            .ok_or_else(|| WorkflowGuideError::InvalidStorage("external Guide path has no parent".to_string()))?;
        references.extend(
            SIBLING_MARKDOWN_REFERENCE
                .captures_iter(markdown)
                .filter_map(|captures| {
                    let file_name = captures
                        .get(1)?
                        .as_str()
                        .strip_prefix("./")
                        .unwrap_or(captures.get(1)?.as_str());
                    Some((
                        captures.get(0).map_or(usize::MAX, |value| value.start()),
                        format!("{parent}/{file_name}"),
                    ))
                }),
        );
    }
    references.sort();
    references.dedup();
    Ok(references)
}

fn resolve_sibling_markdown_paths(
    source_path: &str,
    markdown: &str,
) -> Result<BTreeSet<String>, WorkflowGuideError> {
    let parent = std::path::Path::new(source_path)
        .parent()
        .and_then(|path| path.to_str())
        .ok_or_else(|| WorkflowGuideError::InvalidStorage("external Guide path has no parent".to_string()))?;
    SIBLING_MARKDOWN_REFERENCE
        .captures_iter(markdown)
        .map(|captures| {
            let file_name = captures[1].strip_prefix("./").unwrap_or(&captures[1]);
            let path = format!("{parent}/{file_name}");
            validate_relative_path(&path).map_err(|error| WorkflowGuideError::InvalidStorage(error.to_string()))?;
            Ok(path)
        })
        .collect()
}

pub(crate) async fn stage_projection_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    pool: Pool<Sqlite>,
    skills_root: PathBuf,
) -> Result<(RenderedWorkflowSkill, StagedWorkflowProjection), WorkflowGuideError> {
    let skill_name = ensure_skill_name(transaction, profile_id)
        .await
        .map_err(|error| WorkflowGuideError::Projection(error.to_string()))?;
    let view = load_guide_view(transaction, profile_id).await?;
    let markdown = normalize_main_guide_markdown(&view.markdown)?;
    let graph = load_guide_document_graph(transaction, profile_id, &markdown, &BTreeMap::new()).await?;
    let combined_guide = &graph.combined;
    verify_capability_names(transaction, &combined_guide.capabilities).await?;
    let package_paths = view
        .package_files
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect::<BTreeSet<_>>();
    for path in &combined_guide.package_paths {
        if !package_paths.contains(path.as_str()) {
            return Err(WorkflowGuideError::InvalidStorage(format!(
                "Guide references unavailable package file '{path}'"
            )));
        }
    }
    let profile: (String, String) = sqlx::query_as("SELECT name, description FROM profile WHERE id = ?")
        .bind(profile_id)
        .fetch_one(&mut **transaction)
        .await?;
    let root = graph.root()?;
    let effective_exposure = effective_exposure_by_name(&combined_guide.capabilities);
    let server_names = load_bound_server_display_names(transaction, profile_id).await?;
    let compatibility = skill_compatibility(&server_names);
    let rendered = render_workflow_skill(&markdown, &root.guide, &effective_exposure);
    let skill = format_skill_definition(
        &skill_name,
        &profile.0,
        &profile.1,
        compatibility.as_deref(),
        &rendered.markdown,
    );
    if UUID_REFERENCE.is_match(&skill) || skill.contains("skill://") {
        return Err(WorkflowGuideError::InvalidStorage(
            "projected Skill contains an opaque identifier".to_string(),
        ));
    }
    let material_service = WorkflowMaterialsService::new(pool, skills_root);
    let mut staged_package_files = Vec::<StagedPackageFile>::new();
    for document in graph
        .documents
        .iter()
        .filter(|document| document.package_file_id.is_some())
    {
        let rendered = render_workflow_skill(&document.markdown, &document.guide, &effective_exposure);
        let content = if document.markdown.ends_with('\n') {
            format!("{}\n", rendered.markdown)
        } else {
            rendered.markdown
        };
        let bytes = content.into_bytes();
        let staged = match material_service
            .stage_package_file_bytes(&skill_name, &document.relative_path, &bytes)
            .await
        {
            Ok(staged) => staged,
            Err(error) => {
                rollback_staged_package_files(staged_package_files).await?;
                return Err(WorkflowGuideError::Projection(error.to_string()));
            }
        };
        if let Err(error) = sqlx::query(
            "UPDATE workflow_profile_package_files SET checksum = ?, file_size = ? WHERE package_file_id = ?",
        )
        .bind(format!("{:x}", Sha256::digest(&bytes)))
        .bind(bytes.len() as i64)
        .bind(document.package_file_id.as_deref())
        .execute(&mut **transaction)
        .await
        {
            staged_package_files.push(staged);
            rollback_staged_package_files(staged_package_files).await?;
            return Err(error.into());
        }
        staged_package_files.push(staged);
    }
    let staged_skill_definition = material_service
        .stage_skill_definition(&skill_name, &skill)
        .await
        .map_err(|error| WorkflowGuideError::Projection(error.to_string()));
    let staged_skill_definition = match staged_skill_definition {
        Ok(staged) => staged,
        Err(error) => {
            rollback_staged_package_files(staged_package_files).await?;
            return Err(error);
        }
    };
    let fingerprint = match projection_input_fingerprint(transaction, profile_id, &view.markdown).await {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            StagedWorkflowProjection {
                skill_definition: staged_skill_definition,
                package_files: staged_package_files,
            }
            .rollback()
            .await?;
            return Err(error);
        }
    };
    if let Err(error) = sqlx::query(
        "UPDATE workflow_profile_skill_projections
         SET input_fingerprint = ?, projected_at = CURRENT_TIMESTAMP WHERE profile_id = ?",
    )
    .bind(fingerprint)
    .bind(profile_id)
    .execute(&mut **transaction)
    .await
    {
        StagedWorkflowProjection {
            skill_definition: staged_skill_definition,
            package_files: staged_package_files,
        }
        .rollback()
        .await?;
        return Err(error.into());
    }
    Ok((
        RenderedWorkflowSkill { markdown: skill },
        StagedWorkflowProjection {
            skill_definition: staged_skill_definition,
            package_files: staged_package_files,
        },
    ))
}

async fn rollback_staged_package_files(staged_package_files: Vec<StagedPackageFile>) -> Result<(), WorkflowGuideError> {
    let mut errors = Vec::new();
    for staged in staged_package_files {
        if let Err(error) = staged.rollback().await {
            errors.push(error.to_string());
        }
    }
    if !errors.is_empty() {
        return Err(WorkflowGuideError::Projection(errors.join("; ")));
    }
    Ok(())
}

pub(crate) async fn ensure_guide(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
) -> Result<(), WorkflowGuideError> {
    let name: String = sqlx::query_scalar("SELECT name FROM profile WHERE id = ?")
        .bind(profile_id)
        .fetch_one(&mut **transaction)
        .await?;
    sqlx::query("INSERT OR IGNORE INTO workflow_profile_guides (profile_id, markdown) VALUES (?, ?)")
        .bind(profile_id)
        .bind(format!("# {}", name.trim()))
        .execute(&mut **transaction)
        .await?;
    sqlx::query("INSERT OR IGNORE INTO workflow_profile_skill_projections (profile_id) VALUES (?)")
        .bind(profile_id)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn load_guide_view(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
) -> Result<WorkflowGuideView, WorkflowGuideError> {
    let guide = sqlx::query_as::<_, GuideRow>(
        "SELECT guide_revision, markdown FROM workflow_profile_guides WHERE profile_id = ?",
    )
    .bind(profile_id)
    .fetch_one(&mut **transaction)
    .await?;
    let capabilities = load_guide_document_graph(transaction, profile_id, &guide.markdown, &BTreeMap::new())
        .await?
        .combined
        .capabilities;
    let package_files = sqlx::query_as::<_, PackageFileRow>(
        "SELECT package_file_id, file_revision, title, category, relative_path, mime_type, extension, file_size, checksum
         FROM workflow_profile_package_files WHERE profile_id = ? ORDER BY ordinal",
    )
    .bind(profile_id)
    .fetch_all(&mut **transaction)
    .await?
    .into_iter()
    .map(|row| {
        let category = row.category.parse().map_err(|_| {
            WorkflowGuideError::InvalidStorage("invalid Workflow Guide package-file category".to_string())
        })?;
        Ok(WorkflowGuidePackageFile {
            package_file_id: row.package_file_id,
            file_revision: row.file_revision,
            title: row.title,
            category,
            relative_path: row.relative_path,
            mime_type: row.mime_type,
            extension: row.extension,
            file_size: row.file_size,
        })
    })
    .collect::<Result<Vec<_>, WorkflowGuideError>>()?;
    let documents = load_guide_document_graph(transaction, profile_id, &guide.markdown, &BTreeMap::new())
        .await?
        .documents
        .into_iter()
        .filter_map(|document| match (document.package_file_id, document.file_revision) {
            (Some(package_file_id), Some(file_revision)) => Some(WorkflowGuideExternalDocument {
                package_file_id,
                file_revision,
                title: document.title,
                relative_path: document.relative_path,
                markdown: document.markdown,
            }),
            _ => None,
        })
        .collect();
    Ok(WorkflowGuideView {
        profile_id: profile_id.to_string(),
        guide_revision: guide.guide_revision,
        markdown: guide.markdown,
        capabilities,
        package_files,
        documents,
    })
}

fn validate_save_command(
    command: &WorkflowGuideSaveCommand,
    _guide: &WorkflowGuide,
) -> Result<(), WorkflowGuideError> {
    if command.profile_id.trim().is_empty() {
        return Err(WorkflowGuideError::InvalidStorage(
            "Workflow Profile ID is required".to_string(),
        ));
    }
    Ok(())
}

async fn verify_guide_revision(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    expected_guide_revision: i64,
) -> Result<(), WorkflowGuideError> {
    let actual: i64 = sqlx::query_scalar("SELECT guide_revision FROM workflow_profile_guides WHERE profile_id = ?")
        .bind(profile_id)
        .fetch_one(&mut **transaction)
        .await?;
    if actual == expected_guide_revision {
        Ok(())
    } else {
        Err(WorkflowGuideError::GuideChanged {
            current_guide_revision: actual,
        })
    }
}

async fn bump_guide_revision(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    expected_guide_revision: i64,
) -> Result<(), WorkflowGuideError> {
    let changed = sqlx::query(
        "UPDATE workflow_profile_guides
         SET guide_revision = guide_revision + 1, updated_at = CURRENT_TIMESTAMP
         WHERE profile_id = ? AND guide_revision = ?",
    )
    .bind(profile_id)
    .bind(expected_guide_revision)
    .execute(&mut **transaction)
    .await?;
    if changed.rows_affected() == 1 {
        return Ok(());
    }
    Err(load_guide_conflict(transaction, profile_id).await?)
}

async fn load_guide_conflict(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
) -> Result<WorkflowGuideError, WorkflowGuideError> {
    let current_guide_revision: i64 =
        sqlx::query_scalar("SELECT guide_revision FROM workflow_profile_guides WHERE profile_id = ?")
            .bind(profile_id)
            .fetch_one(&mut **transaction)
            .await?;
    Ok(WorkflowGuideError::GuideChanged { current_guide_revision })
}

async fn load_canonical_capability_refs(
    transaction: &mut Transaction<'_, Sqlite>
) -> Result<BTreeMap<String, String>, WorkflowGuideError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT names.canonical_name, capability.ref_id
        FROM capability_refs capability
        JOIN server_config server ON server.id = capability.server_id
        JOIN (
            SELECT server_id, 'tools' AS kind, tool_name AS origin_key, unique_name AS canonical_name
            FROM server_tools
            UNION ALL
            SELECT server_id, 'resources', resource_uri, unique_uri
            FROM server_resources
            UNION ALL
            SELECT server_id, 'prompts', prompt_name, unique_name
            FROM server_prompts
            UNION ALL
            SELECT server_id, 'resource_templates', uri_template, unique_name
            FROM server_resource_templates
        ) names
          ON names.server_id = capability.server_id
         AND names.kind = capability.kind
         AND names.origin_key = capability.origin_key
        WHERE server.enabled = 1 AND capability.state = 'active'
        ORDER BY names.canonical_name, capability.ref_id
        "#,
    )
    .fetch_all(&mut **transaction)
    .await?;
    let mut resolved = BTreeMap::new();
    for (name, ref_id) in rows {
        if let Some(existing) = resolved.insert(name.clone(), ref_id.clone())
            && existing != ref_id
        {
            return Err(WorkflowGuideError::InvalidStorage(format!(
                "canonical capability name '{name}' is ambiguous"
            )));
        }
    }
    Ok(resolved)
}

async fn verify_capability_names(
    transaction: &mut Transaction<'_, Sqlite>,
    capabilities: &[WorkflowGuideCapability],
) -> Result<(), WorkflowGuideError> {
    let available = load_canonical_capability_refs(transaction).await?;
    for capability in capabilities {
        if !available.contains_key(&capability.name) {
            return Err(WorkflowGuideError::InvalidStorage(format!(
                "Guide references unavailable canonical capability name '{}'",
                capability.name
            )));
        }
    }
    Ok(())
}

async fn verify_package_paths(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    paths: &BTreeSet<String>,
) -> Result<(), WorkflowGuideError> {
    for path in paths {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workflow_profile_package_files
             WHERE profile_id = ? AND relative_path = ?)",
        )
        .bind(profile_id)
        .bind(path)
        .fetch_one(&mut **transaction)
        .await?;
        if !exists {
            return Err(WorkflowGuideError::InvalidStorage(format!(
                "Guide references unavailable package file '{path}'"
            )));
        }
    }
    Ok(())
}

async fn load_bound_server_display_names(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
) -> Result<Vec<String>, WorkflowGuideError> {
    sqlx::query_scalar(
        "SELECT DISTINCT server.name
         FROM workflow_profile_step_bindings binding
         JOIN capability_refs capability ON capability.ref_id = binding.ref_id
         JOIN server_config server ON server.id = capability.server_id
         WHERE binding.profile_id = ?
         ORDER BY server.name",
    )
    .bind(profile_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(WorkflowGuideError::from)
}

async fn projection_input_fingerprint(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    markdown: &str,
) -> Result<String, WorkflowGuideError> {
    let package_files = sqlx::query_as::<_, ProjectionPackageFile>(
        "SELECT relative_path, file_revision, title, category, mime_type, extension, file_size, checksum
             FROM workflow_profile_package_files WHERE profile_id = ? ORDER BY relative_path",
    )
    .bind(profile_id)
    .fetch_all(&mut **transaction)
    .await?;
    let input = serde_json::json!({
        "profile_id": profile_id,
        "markdown": markdown,
        "package_files": package_files,
    });
    Ok(format!("{:x}", Sha256::digest(input.to_string().as_bytes())))
}

#[doc(hidden)]
pub fn validate_package_file_command(command: &WorkflowGuidePackageFileSaveCommand) -> Result<(), WorkflowGuideError> {
    if command.title.trim().is_empty() || command.title.trim().len() > 120 || command.title.contains(['\n', '\r']) {
        return Err(WorkflowGuideError::InvalidStorage(
            "package-file title must be one line with at most 120 characters".to_string(),
        ));
    }
    if command.bytes.is_empty() || command.bytes.len() > MAX_UPLOAD_BYTES {
        return Err(WorkflowGuideError::InvalidStorage(
            "package-file content must be between 1 byte and 5 MiB".to_string(),
        ));
    }
    let extension = package_extension(&command.original_filename)?;
    let valid = match command.category {
        WorkflowGuidePackageCategory::Reference => {
            matches!(extension.as_str(), "md" | "json" | "yaml" | "yml" | "toml")
        }
        WorkflowGuidePackageCategory::Script => {
            matches!(extension.as_str(), "js" | "mjs" | "cjs" | "py" | "sh" | "bat")
        }
        WorkflowGuidePackageCategory::Asset => matches!(
            extension.as_str(),
            "pdf" | "docx" | "xlsx" | "png" | "jpg" | "jpeg" | "webp" | "svg" | "gif" | "ico" | "csv" | "sql"
        ),
    };
    if !valid {
        return Err(WorkflowGuideError::InvalidStorage(format!(
            "'.{extension}' files are not allowed for {} package files",
            command.category.as_str()
        )));
    }
    Ok(())
}

fn package_extension(filename: &str) -> Result<String, WorkflowGuideError> {
    let path = std::path::Path::new(filename);
    if path.file_name().and_then(|name| name.to_str()) != Some(filename) {
        return Err(WorkflowGuideError::InvalidStorage(
            "package-file name must not contain a path".to_string(),
        ));
    }
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .filter(|extension| {
            !extension.is_empty() && extension.chars().all(|character| character.is_ascii_alphanumeric())
        })
        .ok_or_else(|| WorkflowGuideError::InvalidStorage("package-file name must have a valid extension".to_string()))
}

fn package_mime_type(extension: &str) -> Option<&'static str> {
    match extension {
        "md" => Some("text/markdown"),
        "json" => Some("application/json"),
        "yaml" | "yml" => Some("application/yaml"),
        "toml" => Some("application/toml"),
        "js" | "mjs" | "cjs" => Some("text/javascript"),
        "py" => Some("text/x-python"),
        "sh" => Some("text/x-shellscript"),
        "bat" => Some("application/x-bat"),
        "pdf" => Some("application/pdf"),
        "docx" => Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        "xlsx" => Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        "gif" => Some("image/gif"),
        "ico" => Some("image/vnd.microsoft.icon"),
        "csv" => Some("text/csv"),
        "sql" => Some("application/sql"),
        _ => None,
    }
}

#[doc(hidden)]
pub async fn allocate_package_path(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    category: WorkflowGuidePackageCategory,
    title: &str,
    extension: &str,
) -> Result<String, WorkflowGuideError> {
    let stem = display_skill_name(title);
    for ordinal in 1..=999 {
        let suffix = if ordinal == 1 {
            String::new()
        } else {
            format!("-{ordinal}")
        };
        let relative_path = format!("{}/{}{}.{}", category.directory(), stem, suffix, extension);
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM workflow_profile_package_files
                WHERE profile_id = ? AND relative_path = ?
                UNION
                SELECT 1 FROM workflow_profile_materials
                WHERE profile_id = ? AND relative_path = ?
             )",
        )
        .bind(profile_id)
        .bind(&relative_path)
        .bind(profile_id)
        .bind(&relative_path)
        .fetch_one(&mut **transaction)
        .await?;
        if !exists {
            return Ok(relative_path);
        }
    }
    Err(WorkflowGuideError::InvalidStorage(
        "could not allocate a readable package-file path".to_string(),
    ))
}
