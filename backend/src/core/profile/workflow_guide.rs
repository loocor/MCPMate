use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
};

use once_cell::sync::Lazy;
use regex::Regex;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Pool, Sqlite, Transaction};
use uuid::Uuid;

use super::materials::{
    MAX_UPLOAD_BYTES, PackageFileDeletionLease, StagedPackageFile, StagedSkillDefinition, WorkflowMaterialsService,
    ensure_skill_name, validate_relative_path,
};
use super::projection::{
    capability_item_body, capability_section_intro, external_reference_body, format_projected_skill_markdown,
    projection_config, skill_compatibility,
};
use super::workflow::{
    WorkflowBindingCommand, WorkflowBindingPolicy, WorkflowSpecificationError, WorkflowSpecificationSaveCommand,
    WorkflowSpecificationService, WorkflowStepCommand, verify_workflow_profile,
};

static CAPABILITY_START: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^:::capability\s+(\{.*\})\s*$").expect("valid Workflow Guide capability directive regex")
});
static EXTERNAL_START: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^:::external\s+(\{.*\})\s*$").expect("valid Workflow Guide external directive regex"));
static STANDALONE_EXTERNAL_REFERENCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^\s*\[([^\]\n]+)\]\((references/[^\s)#]+\.md)(?:#[^\s)]+)?\)\s*$")
        .expect("valid standalone external Markdown reference regex")
});
static DIRECTIVE_END: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^:::\s*$").expect("valid Workflow Guide directive end regex"));
static PACKAGE_FILE_REFERENCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\[[^\]\n]+\]\(((?:references|scripts|assets)/[^\s)#]+)(?:#[^\s)]+)?\)")
        .expect("valid workflow Guide package file reference regex")
});
static SIBLING_MARKDOWN_REFERENCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^\s*\[[^\]\n]+\]\(((?:\./)?[^/\s)#]+\.md)(?:#[^\s)]+)?\)\s*$")
        .expect("valid external Guide sibling Markdown reference regex")
});
static UUID_REFERENCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
        .expect("valid UUID reference regex")
});
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

#[derive(Clone, FromRow)]
struct ExternalGuideRow {
    package_file_id: String,
    file_revision: i64,
    title: String,
    relative_path: String,
    markdown: String,
}

#[derive(Clone)]
struct GuideDocument {
    package_file_id: Option<String>,
    file_revision: Option<i64>,
    title: String,
    relative_path: String,
    markdown: String,
    guide: WorkflowGuide,
}

struct GuideDocumentGraph {
    documents: Vec<GuideDocument>,
    combined: WorkflowGuide,
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

    fn orphaned_package_files(
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

fn verify_reclamation_confirmation(
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

fn build_guide_document_graph(
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

/// Parses the canonical, document-first Workflow Guide format.
///
/// This parser intentionally owns only document syntax. Database lookups for
/// canonical capability names and package paths are performed by the authoring service
/// so a missing or ambiguous reference fails at the same transaction boundary
/// as persistence and projection.
pub fn parse_workflow_guide(markdown: &str) -> Result<WorkflowGuide, Vec<WorkflowGuideParseError>> {
    let mut headings = Vec::new();
    let mut capabilities = Vec::new();
    let mut external_references = Vec::new();
    let mut package_paths = BTreeSet::new();
    let mut errors = Vec::new();
    let mut fence = None;
    let mut active_capability: Option<ActiveCapability> = None;
    let mut active_external: Option<ActiveExternal> = None;

    for (index, line) in markdown.lines().enumerate() {
        let line_number = index + 1;
        if let Some(active_fence) = fence {
            if closes_fence(line, active_fence) {
                fence = None;
                if let Some(active) = active_capability.as_mut() {
                    active.lines.push(line.to_string());
                } else if let Some(active) = active_external.as_mut() {
                    active.lines.push(line.to_string());
                }
                continue;
            }
            if contains_reserved_workflow_guide_syntax(line) {
                errors.push(WorkflowGuideParseError {
                    line: line_number,
                    message: "Workflow Guide directives and references are not allowed in fenced code".to_string(),
                });
            }
            if let Some(active) = active_capability.as_mut() {
                active.lines.push(line.to_string());
            } else if let Some(active) = active_external.as_mut() {
                active.lines.push(line.to_string());
            }
            continue;
        }
        if let Some(opening_fence) = opening_fence(line) {
            fence = Some(opening_fence);
            if let Some(active) = active_capability.as_mut() {
                active.lines.push(line.to_string());
            } else if let Some(active) = active_external.as_mut() {
                active.lines.push(line.to_string());
            }
            continue;
        }

        if active_capability.is_some() {
            if DIRECTIVE_END.is_match(line) {
                let closed = active_capability
                    .take()
                    .expect("active Workflow Guide capability exists");
                let guide = closed.lines.join("\n").trim().to_string();
                collect_package_references(&guide, &mut package_paths);
                capabilities.push(WorkflowGuideCapability {
                    name: closed.name,
                    exposure: closed.exposure,
                    guide,
                    start_line: closed.start_line,
                    end_line: line_number,
                });
            } else if let Some(open) = active_capability.as_mut() {
                open.lines.push(line.to_string());
            }
            continue;
        }
        if active_external.is_some() {
            if DIRECTIVE_END.is_match(line) {
                let closed = active_external
                    .take()
                    .expect("active Workflow Guide external reference exists");
                let guide = closed.lines.join("\n").trim().to_string();
                collect_package_references(&guide, &mut package_paths);
                package_paths.insert(closed.path.clone());
                external_references.push(WorkflowGuideExternalReference {
                    title: closed.title,
                    path: closed.path,
                    guide,
                    start_line: closed.start_line,
                    end_line: line_number,
                });
            } else if let Some(open) = active_external.as_mut() {
                open.lines.push(line.to_string());
            }
            continue;
        }

        if let Some(captures) = CAPABILITY_START.captures(line) {
            match serde_json::from_str::<CapabilityDirectiveHeader>(&captures[1]) {
                Ok(header) if !header.name.trim().is_empty() && !header.name.contains(['\n', '\r']) => {
                    active_capability = Some(ActiveCapability {
                        name: header.name,
                        exposure: header.exposure,
                        start_line: line_number,
                        lines: Vec::new(),
                    });
                }
                Ok(_) => errors.push(WorkflowGuideParseError {
                    line: line_number,
                    message: "Capability name must not be empty".to_string(),
                }),
                Err(error) => errors.push(WorkflowGuideParseError {
                    line: line_number,
                    message: format!("invalid Capability directive: {error}"),
                }),
            }
            continue;
        }
        if line.trim_start().starts_with(":::capability") {
            errors.push(WorkflowGuideParseError {
                line: line_number,
                message: "invalid Capability directive; expected JSON name and exposure".to_string(),
            });
            continue;
        }
        if let Some(captures) = EXTERNAL_START.captures(line) {
            match serde_json::from_str::<ExternalDirectiveHeader>(&captures[1]) {
                Ok(header)
                    if !header.title.trim().is_empty()
                        && !header.title.contains(['\n', '\r'])
                        && header.path.starts_with("references/")
                        && header.path.ends_with(".md")
                        && !header.path.contains(['\n', '\r']) =>
                {
                    active_external = Some(ActiveExternal {
                        title: header.title,
                        path: header.path,
                        start_line: line_number,
                        lines: Vec::new(),
                    });
                }
                Ok(_) => errors.push(WorkflowGuideParseError {
                    line: line_number,
                    message: "External reference title and references/*.md path must not be empty".to_string(),
                }),
                Err(error) => errors.push(WorkflowGuideParseError {
                    line: line_number,
                    message: format!("invalid External directive: {error}"),
                }),
            }
            continue;
        }
        if line.trim_start().starts_with(":::external") {
            errors.push(WorkflowGuideParseError {
                line: line_number,
                message: "invalid External directive; expected JSON title and path".to_string(),
            });
            continue;
        }
        if DIRECTIVE_END.is_match(line) {
            errors.push(WorkflowGuideParseError {
                line: line_number,
                message: "Workflow Guide directive end has no matching start".to_string(),
            });
            continue;
        }
        if let Some((level, text)) = heading(line) {
            headings.push(WorkflowGuideHeading {
                level,
                text: text.to_string(),
                line: line_number,
            });
        }
        collect_package_references(line, &mut package_paths);
    }

    if let Some(active_capability) = active_capability {
        errors.push(WorkflowGuideParseError {
            line: active_capability.start_line,
            message: format!("Capability '{}' directive is not closed", active_capability.name),
        });
    }
    if let Some(active_external) = active_external {
        errors.push(WorkflowGuideParseError {
            line: active_external.start_line,
            message: format!("External reference '{}' directive is not closed", active_external.title),
        });
    }
    for (index, line) in markdown.lines().enumerate() {
        if UUID_REFERENCE.is_match(line) {
            errors.push(WorkflowGuideParseError {
                line: index + 1,
                message: "opaque identifiers are not allowed in a Workflow Guide".to_string(),
            });
        }
        if line.contains("skill://") {
            errors.push(WorkflowGuideParseError {
                line: index + 1,
                message: "skill:// references are not allowed in a Workflow Guide".to_string(),
            });
        }
    }

    if errors.is_empty() {
        Ok(WorkflowGuide {
            headings,
            capabilities,
            external_references,
            package_paths,
        })
    } else {
        Err(errors)
    }
}

pub fn effective_exposure_by_name(capabilities: &[WorkflowGuideCapability]) -> BTreeMap<String, WorkflowBindingPolicy> {
    let mut effective = BTreeMap::new();
    for capability in capabilities {
        match effective.get(&capability.name) {
            Some(WorkflowBindingPolicy::Direct) => {}
            _ => {
                effective.insert(capability.name.clone(), capability.exposure);
            }
        }
    }
    effective
}

pub fn render_workflow_skill(
    markdown: &str,
    guide: &WorkflowGuide,
    effective_exposure: &BTreeMap<String, WorkflowBindingPolicy>,
) -> RenderedWorkflowSkill {
    let capabilities_by_start = guide
        .capabilities
        .iter()
        .map(|capability| (capability.start_line, capability))
        .collect::<BTreeMap<_, _>>();
    let externals_by_start = guide
        .external_references
        .iter()
        .map(|external| (external.start_line, external))
        .collect::<BTreeMap<_, _>>();
    let mut output: Vec<String> = Vec::new();
    let lines = markdown.lines().collect::<Vec<_>>();
    let mut index = 0;
    let mut capability_section_shown = false;
    let use_capability_list_style = guide.capabilities.len() >= projection_config().workflow.capability.list_threshold;
    let (has_meta_on_demand, has_direct) = capability_exposure_summary(&guide.capabilities, effective_exposure);

    while index < lines.len() {
        let line_number = index + 1;
        if let Some(capability) = capabilities_by_start.get(&line_number) {
            let exposure = *effective_exposure.get(&capability.name).unwrap_or(&capability.exposure);
            if !capability_section_shown {
                push_blank_line_if_needed(&mut output);
                output.push(capability_section_intro(has_meta_on_demand, has_direct));
                capability_section_shown = true;
            }
            let previous_is_list_item = output.last().is_some_and(|line| line.starts_with("- "));
            if !use_capability_list_style || !previous_is_list_item {
                push_blank_line_if_needed(&mut output);
            }
            let rendered = render_capability_occurrence(capability, exposure, use_capability_list_style);
            output.push(rendered);
            let followed_by_capability = next_capability_after(&lines, capability.end_line, &capabilities_by_start);
            if (!use_capability_list_style || !followed_by_capability)
                && lines
                    .get(capability.end_line)
                    .is_some_and(|line| !line.trim().is_empty())
            {
                output.push(String::new());
            }
            index = capability.end_line;
            continue;
        }
        if let Some(external) = externals_by_start.get(&line_number) {
            push_blank_line_if_needed(&mut output);
            output.push(external_reference_body(
                &external.title,
                &external.path,
                &external.guide,
            ));
            if lines.get(external.end_line).is_some_and(|line| !line.trim().is_empty()) {
                output.push(String::new());
            }
            index = external.end_line;
            continue;
        }
        if let Some(captures) = STANDALONE_EXTERNAL_REFERENCE.captures(lines[index]) {
            push_blank_line_if_needed(&mut output);
            output.push(external_reference_body(&captures[1], &captures[2], ""));
            index += 1;
            continue;
        }
        if use_capability_list_style
            && lines[index].trim().is_empty()
            && is_blank_between_consecutive_capabilities(
                line_number,
                &lines,
                &guide.capabilities,
                &capabilities_by_start,
            )
        {
            index += 1;
            continue;
        }
        output.push(lines[index].to_string());
        index += 1;
    }

    RenderedWorkflowSkill {
        markdown: collapse_blank_lines(&output.join("\n")),
    }
}

struct ActiveExternal {
    title: String,
    path: String,
    start_line: usize,
    lines: Vec<String>,
}

struct ActiveCapability {
    name: String,
    exposure: WorkflowBindingPolicy,
    start_line: usize,
    lines: Vec<String>,
}

#[derive(serde::Deserialize)]
struct ExternalDirectiveHeader {
    title: String,
    path: String,
}

#[derive(serde::Deserialize)]
struct CapabilityDirectiveHeader {
    name: String,
    exposure: WorkflowBindingPolicy,
}

#[derive(Clone, Copy)]
struct MarkdownFence {
    delimiter: char,
    length: usize,
}

fn opening_fence(line: &str) -> Option<MarkdownFence> {
    let trimmed = line.trim_start();
    let delimiter = trimmed.chars().next()?;
    if delimiter != '`' && delimiter != '~' {
        return None;
    }
    let length = trimmed.chars().take_while(|character| *character == delimiter).count();
    (length >= 3).then_some(MarkdownFence { delimiter, length })
}

fn closes_fence(
    line: &str,
    fence: MarkdownFence,
) -> bool {
    let trimmed = line.trim_start();
    let length = trimmed
        .chars()
        .take_while(|character| *character == fence.delimiter)
        .count();
    length >= fence.length && trimmed[length..].trim().is_empty()
}

fn contains_reserved_workflow_guide_syntax(line: &str) -> bool {
    line.trim_start().starts_with(":::capability")
        || line.trim_start().starts_with(":::external")
        || DIRECTIVE_END.is_match(line)
        || PACKAGE_FILE_REFERENCE.is_match(line)
}

fn heading(line: &str) -> Option<(u8, &str)> {
    let trimmed = line.trim_start();
    let level = trimmed.chars().take_while(|character| *character == '#').count();
    if !(1..=6).contains(&level) || trimmed.as_bytes().get(level) != Some(&b' ') {
        return None;
    }
    let text = trimmed[level..].trim();
    (!text.is_empty()).then_some((level as u8, text))
}

fn collect_package_references(
    line: &str,
    package_paths: &mut BTreeSet<String>,
) {
    package_paths.extend(
        PACKAGE_FILE_REFERENCE
            .captures_iter(line)
            .map(|captures| captures[1].to_string()),
    );
}

fn capability_exposure_summary(
    capabilities: &[WorkflowGuideCapability],
    effective_exposure: &BTreeMap<String, WorkflowBindingPolicy>,
) -> (bool, bool) {
    let mut has_meta_on_demand = false;
    let mut has_direct = false;
    for capability in capabilities {
        let exposure = *effective_exposure.get(&capability.name).unwrap_or(&capability.exposure);
        match exposure {
            WorkflowBindingPolicy::MetaOnDemand => has_meta_on_demand = true,
            WorkflowBindingPolicy::Direct => has_direct = true,
        }
    }
    (has_meta_on_demand, has_direct)
}

fn push_blank_line_if_needed(output: &mut Vec<String>) {
    if output.last().is_some_and(|line| !line.trim().is_empty()) {
        output.push(String::new());
    }
}

fn next_capability_after(
    lines: &[&str],
    after_line: usize,
    capabilities_by_start: &BTreeMap<usize, &WorkflowGuideCapability>,
) -> bool {
    next_non_blank_line(lines, after_line + 1)
        .is_some_and(|line_number| capabilities_by_start.contains_key(&line_number))
}

fn next_non_blank_line(
    lines: &[&str],
    from_line: usize,
) -> Option<usize> {
    for line_number in from_line..=lines.len() {
        if lines[line_number - 1].trim().is_empty() {
            continue;
        }
        return Some(line_number);
    }
    None
}

fn previous_non_blank_line(
    lines: &[&str],
    before_line: usize,
) -> Option<usize> {
    for line_number in (1..=before_line).rev() {
        if lines[line_number - 1].trim().is_empty() {
            continue;
        }
        return Some(line_number);
    }
    None
}

fn line_in_capability(
    line_number: usize,
    capabilities: &[WorkflowGuideCapability],
) -> bool {
    capabilities
        .iter()
        .any(|capability| (capability.start_line..=capability.end_line).contains(&line_number))
}

fn is_blank_between_consecutive_capabilities(
    blank_line_number: usize,
    lines: &[&str],
    capabilities: &[WorkflowGuideCapability],
    capabilities_by_start: &BTreeMap<usize, &WorkflowGuideCapability>,
) -> bool {
    let Some(previous_line) = previous_non_blank_line(lines, blank_line_number - 1) else {
        return false;
    };
    let Some(next_line) = next_non_blank_line(lines, blank_line_number + 1) else {
        return false;
    };
    line_in_capability(previous_line, capabilities) && capabilities_by_start.contains_key(&next_line)
}

fn format_as_markdown_list_item(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "-".to_string();
    }
    let mut lines = trimmed.lines();
    let first = lines.next().expect("non-empty capability list item");
    let mut output = format!("- {first}");
    for line in lines {
        if line.trim().is_empty() {
            output.push('\n');
        } else {
            output.push_str("\n  ");
            output.push_str(line);
        }
    }
    output
}

fn render_capability_occurrence(
    capability: &WorkflowGuideCapability,
    exposure: WorkflowBindingPolicy,
    list_style: bool,
) -> String {
    let body = capability_item_body(&capability.name, &capability.guide, exposure);
    if list_style {
        format_as_markdown_list_item(&body)
    } else {
        body
    }
}

fn collapse_blank_lines(value: &str) -> String {
    value
        .lines()
        .fold((String::new(), 0_usize), |(mut output, blanks), line| {
            let next_blanks = usize::from(line.trim().is_empty()) * (blanks + 1);
            if next_blanks <= 2 {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(line);
            }
            (output, next_blanks)
        })
        .0
        .trim()
        .to_string()
}

/// The Profile record owns a Skill's identity.  Imported standard Skills may
/// carry their own front matter, but that is source metadata rather than Guide
/// body content and must never become a second front-matter block on projection.
fn normalize_main_guide_markdown(markdown: &str) -> Result<String, WorkflowGuideError> {
    let normalized = markdown.replace("\r\n", "\n");
    let Some(remainder) = normalized.strip_prefix("---\n") else {
        return Ok(normalized);
    };
    let Some(closing_offset) = remainder.find("\n---\n") else {
        return Err(WorkflowGuideError::InvalidStorage(
            "Skill front matter must be closed before the Guide body".to_string(),
        ));
    };
    let front_matter = &remainder[..closing_offset];
    let values: BTreeMap<String, serde_yaml::Value> = serde_yaml::from_str(front_matter)
        .map_err(|error| WorkflowGuideError::InvalidStorage(format!("Skill front matter is invalid YAML: {error}")))?;
    for key in ["name", "description"] {
        if !values.contains_key(key) {
            return Err(WorkflowGuideError::InvalidStorage(format!(
                "Skill front matter must include '{key}'"
            )));
        }
    }
    Ok(remainder[closing_offset + "\n---\n".len()..].to_string())
}

fn format_parse_errors(errors: &[WorkflowGuideParseError]) -> String {
    errors
        .iter()
        .map(|error| format!("line {}: {}", error.line, error.message))
        .collect::<Vec<_>>()
        .join("; ")
}

fn format_skill_definition(
    skill_name: &str,
    profile_name: &str,
    description: &str,
    compatibility: Option<&str>,
    body: &str,
) -> String {
    let description = if description.trim().is_empty() {
        format!("Workflow Guide for {}.", profile_name.trim())
    } else {
        description.trim().to_string()
    };
    format_projected_skill_markdown(skill_name, &description, compatibility, body)
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

fn validate_package_file_command(command: &WorkflowGuidePackageFileSaveCommand) -> Result<(), WorkflowGuideError> {
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
        WorkflowGuidePackageCategory::Script => matches!(extension.as_str(), "js" | "mjs" | "cjs" | "py"),
        WorkflowGuidePackageCategory::Asset => matches!(extension.as_str(), "pdf" | "docx" | "xlsx"),
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
        "pdf" => Some("application/pdf"),
        "docx" => Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        "xlsx" => Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        _ => None,
    }
}

async fn allocate_package_path(
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

fn display_skill_name(profile_name: &str) -> String {
    let mut output = String::new();
    let mut pending_separator = false;
    for character in profile_name.trim().chars() {
        if character.is_ascii_alphanumeric() {
            if pending_separator && !output.is_empty() {
                output.push('-');
            }
            output.push(character.to_ascii_lowercase());
            pending_separator = false;
        } else {
            pending_separator = true;
        }
    }
    if output.is_empty() {
        "workflow-guide".to_string()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn external_row(
        path: &str,
        markdown: &str,
    ) -> ExternalGuideRow {
        ExternalGuideRow {
            package_file_id: format!("file-{path}"),
            file_revision: 1,
            title: path.to_string(),
            relative_path: path.to_string(),
            markdown: markdown.to_string(),
        }
    }

    fn package_file(path: &str) -> WorkflowGuidePackageFile {
        WorkflowGuidePackageFile {
            package_file_id: format!("file-{path}"),
            file_revision: 1,
            title: path.to_string(),
            category: if path.starts_with("references/") {
                WorkflowGuidePackageCategory::Reference
            } else {
                WorkflowGuidePackageCategory::Asset
            },
            relative_path: path.to_string(),
            mime_type: None,
            extension: None,
            file_size: None,
        }
    }

    #[test]
    fn parses_readable_blocks_and_references() {
        let guide = parse_workflow_guide(
            "# Investigate a release\n\n:::capability {\"name\":\"search-release-logs\",\"exposure\":\"direct\"}\nUse it to collect the release evidence.\n:::\n\nRead [policy](references/release-policy.md).\n",
        )
        .expect("valid Guide");

        assert_eq!(guide.headings[0].text, "Investigate a release");
        assert_eq!(guide.capabilities[0].name, "search-release-logs");
        assert_eq!(guide.capabilities[0].exposure, WorkflowBindingPolicy::Direct);
        assert_eq!(guide.capabilities[0].guide, "Use it to collect the release evidence.");
        assert_eq!(
            guide.package_paths,
            BTreeSet::from(["references/release-policy.md".to_string()])
        );
    }

    #[test]
    fn document_graph_traverses_recursive_references_and_terminates_cycles() {
        let graph = build_guide_document_graph(
            "# Root\n\n[A](references/a.md)\n",
            vec![
                external_row("references/a.md", "# A\n\n[B](b.md#details)\n"),
                external_row(
                    "references/b.md",
                    "# B\n\n[A](references/a.md)\n\n[Diagram](assets/diagram.png)\n",
                ),
            ],
            &BTreeMap::new(),
        )
        .expect("build recursive document graph");

        assert_eq!(
            graph
                .documents
                .iter()
                .map(|document| document.relative_path.as_str())
                .collect::<Vec<_>>(),
            vec!["SKILL.md", "references/a.md", "references/b.md"]
        );
        assert_eq!(
            graph.combined.package_paths,
            BTreeSet::from([
                "assets/diagram.png".to_string(),
                "references/a.md".to_string(),
                "references/b.md".to_string(),
            ])
        );
    }

    #[test]
    fn document_graph_orders_capabilities_at_recursive_reference_positions() {
        let graph = build_guide_document_graph(
            "# Root\n\n:::capability {\"name\":\"first\",\"exposure\":\"meta_on_demand\"}\nFirst.\n:::\n\n[External](references/external.md)\n\n:::capability {\"name\":\"last\",\"exposure\":\"direct\"}\nLast.\n:::\n",
            vec![external_row(
                "references/external.md",
                "# External\n\n:::capability {\"name\":\"middle\",\"exposure\":\"direct\"}\nMiddle.\n:::\n",
            )],
            &BTreeMap::new(),
        )
        .expect("build recursively ordered document graph");

        assert_eq!(
            graph
                .combined
                .capabilities
                .iter()
                .map(|capability| capability.name.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "middle", "last"]
        );
    }

    #[test]
    fn document_graph_only_follows_standalone_sibling_markdown_references() {
        let graph = build_guide_document_graph(
            "# Root\n\n[A](references/a.md)\n",
            vec![
                external_row("references/a.md", "# A\n\nSee [B](b.md#details) inline.\n\n[C](c.md)\n"),
                external_row("references/b.md", "# B\n"),
                external_row("references/c.md", "# C\n"),
            ],
            &BTreeMap::new(),
        )
        .expect("build document graph");

        assert_eq!(
            graph
                .documents
                .iter()
                .map(|document| document.relative_path.as_str())
                .collect::<Vec<_>>(),
            vec!["SKILL.md", "references/a.md", "references/c.md"]
        );
    }

    #[test]
    fn document_graph_override_reports_only_newly_unreachable_files() {
        let rows = vec![
            external_row("references/a.md", "# A\n\n[B](references/b.md)\n"),
            external_row("references/b.md", "# B\n\n[Diagram](assets/diagram.png)\n"),
        ];
        let persisted = build_guide_document_graph("# Root\n\n[A](references/a.md)\n", rows.clone(), &BTreeMap::new())
            .expect("build persisted graph");
        let candidate = build_guide_document_graph(
            "# Root\n\n[A](references/a.md)\n",
            rows,
            &BTreeMap::from([("references/a.md".to_string(), "# A\n".to_string())]),
        )
        .expect("build candidate graph");
        let package_files = [
            package_file("references/a.md"),
            package_file("references/b.md"),
            package_file("assets/diagram.png"),
        ];

        assert_eq!(
            persisted
                .orphaned_package_files(&candidate, &package_files)
                .into_iter()
                .map(|file| file.relative_path)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["assets/diagram.png".to_string(), "references/b.md".to_string(),])
        );
    }

    #[test]
    fn document_graph_keeps_shared_and_unlinked_external_references_out_of_orphans() {
        let rows = vec![
            external_row("references/a.md", "# A\n\n[B](references/b.md)\n"),
            external_row("references/c.md", "# C\n\n[B](references/b.md)\n"),
            external_row("references/b.md", "# B\n"),
            external_row("references/unlinked.md", "# Unlinked\n\n[B](references/b.md)\n"),
        ];
        let root = "# Root\n\n[A](references/a.md)\n\n[C](references/c.md)\n";
        let persisted =
            build_guide_document_graph(root, rows.clone(), &BTreeMap::new()).expect("build persisted graph");
        let candidate = build_guide_document_graph(
            root,
            rows.clone(),
            &BTreeMap::from([("references/a.md".to_string(), "# A\n".to_string())]),
        )
        .expect("build shared-reference candidate graph");
        let package_files = [package_file("references/b.md")];
        assert!(persisted.orphaned_package_files(&candidate, &package_files).is_empty());

        let unlinked_persisted = build_guide_document_graph("# Root\n", rows.clone(), &BTreeMap::new())
            .expect("build unlinked persisted graph");
        let unlinked_candidate = build_guide_document_graph(
            "# Root\n",
            rows,
            &BTreeMap::from([("references/unlinked.md".to_string(), "# Changed\n".to_string())]),
        )
        .expect("build unlinked candidate graph");
        assert!(
            unlinked_persisted
                .orphaned_package_files(&unlinked_candidate, &package_files)
                .is_empty()
        );
    }

    #[test]
    fn reclamation_confirmation_rejects_stale_and_duplicate_candidates() {
        let plan = WorkflowGuideReclamationPlan {
            package_files: vec![package_file("references/a.md")],
            capabilities: vec![WorkflowGuideCapability {
                name: "lookup".to_string(),
                exposure: WorkflowBindingPolicy::Direct,
                guide: "Look up the record.".to_string(),
                start_line: 1,
                end_line: 3,
            }],
        };
        let stale = WorkflowGuideReclamationConfirmation {
            package_files: vec![WorkflowGuidePackageFileRevision {
                package_file_id: "file-references/a.md".to_string(),
                file_revision: 2,
            }],
            capability_names: vec!["lookup".to_string()],
        };
        assert!(matches!(
            verify_reclamation_confirmation(&plan, Some(&stale)),
            Err(WorkflowGuideError::ReclamationConfirmationChanged)
        ));

        let duplicate = WorkflowGuideReclamationConfirmation {
            package_files: vec![
                WorkflowGuidePackageFileRevision {
                    package_file_id: "file-references/a.md".to_string(),
                    file_revision: 1,
                },
                WorkflowGuidePackageFileRevision {
                    package_file_id: "file-references/a.md".to_string(),
                    file_revision: 1,
                },
            ],
            capability_names: vec!["lookup".to_string(), "lookup".to_string()],
        };
        assert!(matches!(
            verify_reclamation_confirmation(&plan, Some(&duplicate)),
            Err(WorkflowGuideError::ReclamationConfirmationChanged)
        ));
    }

    #[test]
    fn rejects_reserved_workflow_syntax_in_fenced_code() {
        let errors = parse_workflow_guide(
            "```markdown\n:::capability {\"name\":\"fake\",\"exposure\":\"direct\"}\n[Fake](references/fake.md)\n:::\n```")
        .expect_err("fenced pseudo-references must fail");
        assert_eq!(
            errors
                .iter()
                .map(|error| (error.line, error.message.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (
                    2,
                    "Workflow Guide directives and references are not allowed in fenced code"
                ),
                (
                    3,
                    "Workflow Guide directives and references are not allowed in fenced code"
                ),
                (
                    4,
                    "Workflow Guide directives and references are not allowed in fenced code"
                ),
            ]
        );
    }

    #[test]
    fn rejects_reserved_workflow_syntax_in_tilde_fenced_code() {
        let errors =
            parse_workflow_guide("~~~markdown\n:::capability {\"name\":\"fake\",\"exposure\":\"direct\"}\n~~~")
                .expect_err("tilde fenced pseudo-references must fail");
        assert_eq!(errors[0].line, 2);
        assert_eq!(
            errors[0].message,
            "Workflow Guide directives and references are not allowed in fenced code"
        );
    }

    #[test]
    fn keeps_shorter_fence_markers_inside_a_longer_fence() {
        let errors = parse_workflow_guide(
            "````markdown\n```\n:::capability {\"name\":\"lookup\",\"exposure\":\"direct\"}\n:::\n```\n````",
        )
        .expect_err("reserved syntax inside the outer fence must remain inert");

        assert!(
            errors
                .iter()
                .any(|error| { error.line == 3 && error.message.contains("not allowed in fenced code") })
        );
        assert!(
            errors
                .iter()
                .any(|error| { error.line == 4 && error.message.contains("not allowed in fenced code") })
        );
    }

    #[test]
    fn normalizes_imported_skill_front_matter_before_projection() {
        let body = normalize_main_guide_markdown(
            "---\nname: imported-skill\ndescription: Imported description\n---\n\n# Imported heading\n",
        )
        .expect("valid imported Skill");
        let guide = parse_workflow_guide(&body).expect("normalized Guide");
        let rendered = render_workflow_skill(&body, &guide, &effective_exposure_by_name(&guide.capabilities));
        let skill = format_skill_definition(
            "profile-skill",
            "Profile",
            "Profile description",
            None,
            &rendered.markdown,
        );

        assert_eq!(skill.matches("name:").count(), 1);
        assert_eq!(skill.matches("description:").count(), 1);
        assert!(skill.contains("# Imported heading"));
        assert!(!skill.contains("imported-skill"));
    }

    #[test]
    fn reports_malformed_directives_and_opaque_identifiers() {
        let errors = parse_workflow_guide(
            ":::capability {\"name\":\"lookup\"}\n\n550e8400-e29b-41d4-a716-446655440000\nskill://internal",
        )
        .expect_err("invalid Guide");

        assert_eq!(errors.len(), 3);
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("invalid Capability directive"))
        );
        assert!(errors.iter().any(|error| error.message.contains("opaque identifiers")));
        assert!(errors.iter().any(|error| error.message.contains("skill://")));
    }

    #[test]
    fn projects_external_directives_and_standalone_markdown_references() {
        let directive = concat!(
            "# Investigate\n\n",
            ":::external {\"title\":\"Evidence index\",\"path\":\"references/evidence-index.md\"}\n",
            "Open this when you need the evidence checklist.\n",
            ":::\n"
        );
        let directive_guide = parse_workflow_guide(directive).expect("valid external directive");
        let directive_rendered = render_workflow_skill(
            directive,
            &directive_guide,
            &effective_exposure_by_name(&directive_guide.capabilities),
        );
        assert!(
            directive_rendered
                .markdown
                .contains("Open this when you need the evidence checklist.")
        );
        assert!(
            directive_rendered
                .markdown
                .contains("[Evidence index](references/evidence-index.md)")
        );
        assert!(!directive_rendered.markdown.contains("**External document:"));
        assert!(!directive_rendered.markdown.contains("Consult `"));
        assert!(!directive_rendered.markdown.contains(":::external"));

        let standalone = "# Investigate\n\n[Evidence index](references/evidence-index.md)\n";
        let standalone_guide = parse_workflow_guide(standalone).expect("valid standalone external link");
        let standalone_rendered = render_workflow_skill(
            standalone,
            &standalone_guide,
            &effective_exposure_by_name(&standalone_guide.capabilities),
        );
        assert!(
            standalone_rendered
                .markdown
                .contains("[Evidence index](references/evidence-index.md)")
        );
        assert!(!standalone_rendered.markdown.contains("**External document:"));
        assert!(!standalone_rendered.markdown.contains("Consult `"));
    }

    #[test]
    fn projects_only_standard_markdown_and_readable_names() {
        let markdown = "# Investigate\n\n:::capability {\"name\":\"search-release-logs\",\"exposure\":\"direct\"}\nUse it to search release logs.\n:::\n";
        let guide = parse_workflow_guide(markdown).expect("valid Guide");
        let rendered = render_workflow_skill(markdown, &guide, &effective_exposure_by_name(&guide.capabilities));

        assert_eq!(
            rendered.markdown,
            concat!(
                "# Investigate\n\n",
                "The following steps are MCP server capability invocations. ",
                "Steps marked `(direct)` can be invoked by tool name.\n\n",
                "`search-release-logs` (direct): Use it to search release logs."
            )
        );
        assert!(!rendered.markdown.contains(":::capability"));
        assert!(!rendered.markdown.contains("Exposure:"));
    }

    #[test]
    fn normalizes_mixed_occurrence_exposure_to_direct_invocation() {
        let markdown = concat!(
            "# Investigate\n\n",
            ":::capability {\"name\":\"search-release-logs\",\"exposure\":\"meta_on_demand\"}\n",
            "Inspect first.\n",
            ":::\n\n",
            ":::capability {\"name\":\"search-release-logs\",\"exposure\":\"direct\"}\n",
            "Then capture the screenshot.\n",
            ":::\n"
        );
        let guide = parse_workflow_guide(markdown).expect("valid Guide");
        let rendered = render_workflow_skill(markdown, &guide, &effective_exposure_by_name(&guide.capabilities));

        assert_eq!(
            rendered.markdown,
            concat!(
                "# Investigate\n\n",
                "The following steps are MCP server capability invocations. ",
                "Steps marked `(direct)` can be invoked by tool name.\n\n",
                "- `search-release-logs` (direct): Inspect first.\n",
                "- `search-release-logs` (direct): Then capture the screenshot."
            )
        );
        assert!(!rendered.markdown.contains("mcpmate_ucan_details"));
        assert!(!rendered.markdown.contains("Exposure:"));
    }

    #[test]
    fn projects_meta_on_demand_invocation_without_direct_labels() {
        let markdown = concat!(
            "# Investigate\n\n",
            ":::capability {\"name\":\"search-release-logs\",\"exposure\":\"meta_on_demand\"}\n",
            "Inspect first.\n",
            ":::\n\n",
            ":::capability {\"name\":\"search-release-logs\",\"exposure\":\"meta_on_demand\"}\n",
            "Then summarize.\n",
            ":::\n"
        );
        let guide = parse_workflow_guide(markdown).expect("valid Guide");
        let rendered = render_workflow_skill(markdown, &guide, &effective_exposure_by_name(&guide.capabilities));

        assert!(rendered.markdown.contains("mcpmate_ucan_details"));
        assert!(!rendered.markdown.contains("Use `search-release-logs` directly."));
        assert!(!rendered.markdown.contains("**Capability:"));
        assert_eq!(
            rendered.markdown,
            concat!(
                "# Investigate\n\n",
                "The following steps are MCP server capability invocations. ",
                "Steps marked `(on-demand)` must be inspected with `mcpmate_ucan_details`, ",
                "then invoked with `mcpmate_ucan_call`.\n\n",
                "- `search-release-logs` (on-demand): Inspect first.\n",
                "- `search-release-logs` (on-demand): Then summarize."
            )
        );
    }

    #[test]
    fn projects_multiple_capabilities_as_a_markdown_list() {
        let markdown = concat!(
            "# Investigate\n\n",
            ":::capability {\"name\":\"open-page\",\"exposure\":\"direct\"}\n",
            "Open the target URL.\n",
            ":::\n\n",
            ":::capability {\"name\":\"capture-shot\",\"exposure\":\"direct\"}\n",
            "Capture the screenshot.\n",
            ":::\n"
        );
        let guide = parse_workflow_guide(markdown).expect("valid Guide");
        let rendered = render_workflow_skill(markdown, &guide, &effective_exposure_by_name(&guide.capabilities));

        assert_eq!(
            rendered.markdown,
            concat!(
                "# Investigate\n\n",
                "The following steps are MCP server capability invocations. ",
                "Steps marked `(direct)` can be invoked by tool name.\n\n",
                "- `open-page` (direct): Open the target URL.\n",
                "- `capture-shot` (direct): Capture the screenshot."
            )
        );
    }

    #[tokio::test]
    async fn initializes_a_new_workflow_profile_with_a_readable_guide() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        crate::test_helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', '', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .expect("insert Workflow Profile");

        let view = WorkflowGuideService::new(pool)
            .view("workflow-profile")
            .await
            .expect("view Guide");

        assert_eq!(view.guide_revision, 0);
        assert_eq!(view.markdown, "# Release investigation");
        assert!(view.capabilities.is_empty());
        assert!(view.package_files.is_empty());
    }

    #[tokio::test]
    async fn guide_package_paths_reserve_legacy_material_paths() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        crate::test_helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Workflow', '', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .expect("insert Workflow Profile");
        sqlx::query(
            "INSERT INTO workflow_profile_materials (
                material_id, profile_id, ordinal, title, kind, relative_path
             ) VALUES ('legacy-material', 'workflow-profile', 0, 'Policy', 'uploaded_file', 'references/policy.md')",
        )
        .execute(&pool)
        .await
        .expect("reserve legacy Material path");
        let mut transaction = pool.begin().await.expect("begin allocation transaction");

        let allocated = allocate_package_path(
            &mut transaction,
            "workflow-profile",
            WorkflowGuidePackageCategory::Reference,
            "Policy",
            "md",
        )
        .await
        .expect("allocate Guide package path");

        assert_eq!(allocated, "references/policy-2.md");
    }

    #[tokio::test]
    async fn projects_a_guide_atomically_without_internal_identifiers() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        crate::test_helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', 'Investigate production release regressions.', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .expect("insert Workflow Profile");
        sqlx::query(
            "INSERT INTO workflow_profile_skills (profile_id, skill_name) VALUES ('workflow-profile', 'release-investigation-guide')",
        )
        .execute(&pool)
        .await
        .expect("configure friendly Skill name");
        let service = WorkflowGuideService::new(pool.clone());
        service.view("workflow-profile").await.expect("initialize Guide");
        sqlx::query(
            "UPDATE workflow_profile_guides
             SET markdown = '# Release investigation\n\nUse this Guide to investigate a regression.'
             WHERE profile_id = 'workflow-profile'",
        )
        .execute(&pool)
        .await
        .expect("write Guide fixture");
        let temporary = tempfile::tempdir().expect("create skills directory");

        let projected = service
            .project("workflow-profile", temporary.path().to_path_buf())
            .await
            .expect("project Guide");

        assert!(projected.markdown.contains("name: release-investigation-guide"));
        assert!(projected.markdown.contains("# Release investigation"));
        assert!(!projected.markdown.contains("workflow-profile"));
        let skill = std::fs::read_to_string(temporary.path().join("release-investigation-guide/SKILL.md"))
            .expect("read projected Skill");
        assert_eq!(skill, projected.markdown);
        let skill_path = temporary.path().join("release-investigation-guide/SKILL.md");
        std::fs::remove_file(&skill_path).expect("remove stale projected Skill");
        let repaired = service
            .project("workflow-profile", temporary.path().to_path_buf())
            .await
            .expect("repair missing projected Skill");
        assert_eq!(
            std::fs::read_to_string(&skill_path).expect("read repaired Skill"),
            repaired.markdown
        );
        assert_eq!(repaired.markdown, projected.markdown);
        let asset_bytes = b"registered asset";
        let asset_path = temporary.path().join("release-investigation-guide/assets/evidence.bin");
        std::fs::write(&asset_path, asset_bytes).expect("write registered asset");
        sqlx::query(
            "INSERT INTO workflow_profile_package_files (
                package_file_id, profile_id, ordinal, title, category, relative_path,
                extension, file_size, checksum
             ) VALUES ('asset-file', 'workflow-profile', 0, 'Evidence', 'asset',
                'assets/evidence.bin', 'bin', ?, ?)",
        )
        .bind(asset_bytes.len() as i64)
        .bind(format!("{:x}", Sha256::digest(asset_bytes)))
        .execute(&pool)
        .await
        .expect("register asset");
        service
            .project("workflow-profile", temporary.path().to_path_buf())
            .await
            .expect("repair preserves a registered non-Markdown file");
        assert_eq!(std::fs::read(&asset_path).expect("read preserved asset"), asset_bytes);

        let materials = WorkflowMaterialsService::new(pool.clone(), temporary.path().to_path_buf());
        let residual_lease = materials
            .stage_package_file_deletion_lease("release-investigation-guide", "assets/evidence.bin")
            .await
            .expect("simulate an interrupted package-file deletion");
        drop(residual_lease);
        assert!(!asset_path.exists());
        service
            .project("workflow-profile", temporary.path().to_path_buf())
            .await
            .expect("repair restores a registered package-file deletion lease before checksum verification");
        assert_eq!(std::fs::read(&asset_path).expect("read restored asset"), asset_bytes);

        let interrupted_replacement = materials
            .stage_package_file_bytes(
                "release-investigation-guide",
                "assets/evidence.bin",
                b"uncommitted replacement",
            )
            .await
            .expect("simulate an interrupted package-file replacement");
        drop(interrupted_replacement);
        assert_eq!(
            std::fs::read(&asset_path).expect("read uncommitted replacement"),
            b"uncommitted replacement"
        );
        service
            .project("workflow-profile", temporary.path().to_path_buf())
            .await
            .expect("repair restores a registered package-file backup before checksum verification");
        assert_eq!(std::fs::read(&asset_path).expect("read recovered asset"), asset_bytes);

        std::fs::remove_file(&asset_path).expect("remove registered asset");
        let error = service
            .project("workflow-profile", temporary.path().to_path_buf())
            .await
            .expect_err("repair diagnoses a missing registered non-Markdown file");
        assert!(error.to_string().contains("cannot be repaired"));
        let fingerprint: Option<String> = sqlx::query_scalar(
            "SELECT input_fingerprint FROM workflow_profile_skill_projections WHERE profile_id = 'workflow-profile'",
        )
        .fetch_one(&pool)
        .await
        .expect("load projection fingerprint");
        assert!(fingerprint.is_some());
    }

    #[tokio::test]
    async fn repair_preserves_files_owned_by_legacy_material_rows() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        crate::test_helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', '', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .expect("insert Workflow Profile");
        sqlx::query(
            "INSERT INTO workflow_profile_skills (profile_id, skill_name)
             VALUES ('workflow-profile', 'release-investigation-guide')",
        )
        .execute(&pool)
        .await
        .expect("configure friendly Skill name");
        let service = WorkflowGuideService::new(pool.clone());
        service.view("workflow-profile").await.expect("initialize Guide");
        let temporary = tempfile::tempdir().expect("create skills directory");
        service
            .project("workflow-profile", temporary.path().to_path_buf())
            .await
            .expect("create Skill projection");

        let legacy_path = temporary
            .path()
            .join("release-investigation-guide/references/legacy-notes.md");
        std::fs::write(&legacy_path, "# Legacy notes\n").expect("write legacy Material file");
        sqlx::query(
            "INSERT INTO workflow_profile_materials (
                material_id, profile_id, ordinal, title, kind, relative_path
             ) VALUES (
                'legacy-material', 'workflow-profile', 0, 'Legacy notes',
                'uploaded_file', 'references/legacy-notes.md'
             )",
        )
        .execute(&pool)
        .await
        .expect("register legacy Material file");

        service
            .project("workflow-profile", temporary.path().to_path_buf())
            .await
            .expect("repair Skill projection");

        assert_eq!(
            std::fs::read_to_string(legacy_path).expect("read preserved legacy Material file"),
            "# Legacy notes\n"
        );
    }

    #[tokio::test]
    async fn projection_fingerprint_tracks_package_file_revisions() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        crate::test_helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', '', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .expect("insert Workflow Profile");
        sqlx::query(
            "INSERT INTO workflow_profile_skills (profile_id, skill_name) VALUES ('workflow-profile', 'release-investigation-guide')",
        )
        .execute(&pool)
        .await
        .expect("configure friendly Skill name");
        sqlx::query(
            "INSERT INTO workflow_profile_package_files (
                package_file_id, profile_id, ordinal, title, category, relative_path, file_revision, checksum
             ) VALUES (
                'package-file', 'workflow-profile', 0, 'Release policy', 'reference',
                'references/release-policy.md', 0, 'first-checksum'
             )",
        )
        .execute(&pool)
        .await
        .expect("insert package file");
        sqlx::query(
            "INSERT INTO workflow_profile_external_guides (package_file_id, profile_id, markdown)
             VALUES ('package-file', 'workflow-profile', '# Release policy')",
        )
        .execute(&pool)
        .await
        .expect("register reconstructable external Guide source");
        let service = WorkflowGuideService::new(pool.clone());
        service.view("workflow-profile").await.expect("initialize Guide");
        let temporary = tempfile::tempdir().expect("create skills directory");

        service
            .project("workflow-profile", temporary.path().to_path_buf())
            .await
            .expect("project first package revision");
        let first: String = sqlx::query_scalar(
            "SELECT input_fingerprint FROM workflow_profile_skill_projections WHERE profile_id = 'workflow-profile'",
        )
        .fetch_one(&pool)
        .await
        .expect("load first fingerprint");
        sqlx::query(
            "UPDATE workflow_profile_package_files
             SET file_revision = 1, checksum = 'second-checksum'
             WHERE package_file_id = 'package-file'",
        )
        .execute(&pool)
        .await
        .expect("change package file revision");

        service
            .project("workflow-profile", temporary.path().to_path_buf())
            .await
            .expect("project changed package revision");
        let second: String = sqlx::query_scalar(
            "SELECT input_fingerprint FROM workflow_profile_skill_projections WHERE profile_id = 'workflow-profile'",
        )
        .fetch_one(&pool)
        .await
        .expect("load second fingerprint");
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn manages_category_validated_package_files_with_the_skill_projection() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        crate::test_helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', '', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .expect("insert Workflow Profile");
        let service = WorkflowGuideService::new(pool);
        service.view("workflow-profile").await.expect("initialize Guide");
        let temporary = tempfile::tempdir().expect("create skills directory");

        let saved = service
            .save_package_file_and_project(
                WorkflowGuidePackageFileSaveCommand {
                    profile_id: "workflow-profile".to_string(),
                    package_file_id: None,
                    expected_file_revision: None,
                    expected_guide_revision: Some(0),
                    title: "Release policy".to_string(),
                    category: WorkflowGuidePackageCategory::Reference,
                    original_filename: "release-policy.md".to_string(),
                    bytes: b"# Release policy\n".to_vec(),
                    reclamation_confirmation: None,
                },
                temporary.path().to_path_buf(),
            )
            .await
            .expect("save package file");
        let file = saved.guide.package_files.first().expect("saved package file").clone();
        assert_eq!(file.relative_path, "references/release-policy.md");
        assert_eq!(file.mime_type.as_deref(), Some("text/markdown"));
        assert!(!saved.projected_skill.markdown.contains(&file.package_file_id));
        assert_eq!(
            std::fs::read(
                temporary
                    .path()
                    .join("workflow-workflow-profile")
                    .join(&file.relative_path)
            )
            .expect("read package file"),
            b"# Release policy\n"
        );

        let linked = service
            .save_and_project(
                WorkflowGuideSaveCommand {
                    profile_id: "workflow-profile".to_string(),
                    expected_guide_revision: saved.guide.guide_revision,
                    markdown: format!("# Release investigation\n\n[{}]({})\n", file.title, file.relative_path),
                    reclamation_confirmation: None,
                },
                temporary.path().to_path_buf(),
            )
            .await
            .expect("link package file");
        let error = service
            .save_and_project(
                WorkflowGuideSaveCommand {
                    profile_id: "workflow-profile".to_string(),
                    expected_guide_revision: linked.guide.guide_revision,
                    markdown: "# Release investigation\n".to_string(),
                    reclamation_confirmation: None,
                },
                temporary.path().to_path_buf(),
            )
            .await
            .expect_err("newly unreachable package file requires confirmation");
        let WorkflowGuideError::ReclamationConfirmationRequired(plan) = error else {
            panic!("expected reclamation confirmation requirement");
        };
        assert_eq!(plan.package_files, vec![file.clone()]);
        assert_eq!(
            service
                .view("workflow-profile")
                .await
                .expect("reload unchanged Guide")
                .guide_revision,
            linked.guide.guide_revision
        );
        let reclaimed = service
            .save_and_project(
                WorkflowGuideSaveCommand {
                    profile_id: "workflow-profile".to_string(),
                    expected_guide_revision: linked.guide.guide_revision,
                    markdown: "# Release investigation\n".to_string(),
                    reclamation_confirmation: Some(WorkflowGuideReclamationConfirmation {
                        package_files: vec![WorkflowGuidePackageFileRevision {
                            package_file_id: file.package_file_id,
                            file_revision: file.file_revision,
                        }],
                        capability_names: Vec::new(),
                    }),
                },
                temporary.path().to_path_buf(),
            )
            .await
            .expect("save after exact reclamation confirmation");
        assert!(reclaimed.guide.package_files.is_empty());
        assert!(
            !temporary
                .path()
                .join("workflow-workflow-profile")
                .join(file.relative_path)
                .exists(),
            "confirmed reclamation moves the projected package file out of the Skill package"
        );
    }

    #[tokio::test]
    async fn reads_and_previews_external_markdown_without_persisting_the_draft() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        crate::test_helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', 'Investigate releases.', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .expect("insert Workflow Profile");
        sqlx::query(
            "INSERT INTO workflow_profile_skills (profile_id, skill_name)
             VALUES ('workflow-profile', 'release-investigation-guide')",
        )
        .execute(&pool)
        .await
        .expect("configure friendly Skill name");
        let service = WorkflowGuideService::new(pool.clone());
        service.view("workflow-profile").await.expect("initialize Guide");
        let temporary = tempfile::tempdir().expect("create skills directory");
        let saved = service
            .save_package_file_and_project(
                WorkflowGuidePackageFileSaveCommand {
                    profile_id: "workflow-profile".to_string(),
                    package_file_id: None,
                    expected_file_revision: None,
                    expected_guide_revision: Some(0),
                    title: "Release policy".to_string(),
                    category: WorkflowGuidePackageCategory::Reference,
                    original_filename: "release-policy.md".to_string(),
                    bytes: b"# Release policy\nDraft policy body.\n".to_vec(),
                    reclamation_confirmation: None,
                },
                temporary.path().to_path_buf(),
            )
            .await
            .expect("save external Markdown document");
        let file = saved.guide.package_files.first().expect("package file").clone();
        let package_service = WorkflowMaterialsService::new(pool.clone(), temporary.path().to_path_buf());
        let package_guard = package_service
            .lock_skill_package("release-investigation-guide")
            .await
            .expect("hold package lock while opening external document");
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                service.read_external_document(
                    "workflow-profile",
                    &file.package_file_id,
                    temporary.path().to_path_buf(),
                ),
            )
            .await
            .is_err(),
            "external document reads must wait for a package writer"
        );
        drop(package_guard);
        service
            .read_external_document(
                "workflow-profile",
                &file.package_file_id,
                temporary.path().to_path_buf(),
            )
            .await
            .expect("read resumes after package writer releases the lock");
        let document = service
            .read_external_document(
                "workflow-profile",
                &file.package_file_id,
                temporary.path().to_path_buf(),
            )
            .await
            .expect("read external Markdown document");
        assert_eq!(document.relative_path, "references/release-policy.md");
        assert_eq!(document.markdown, "# Release policy\nDraft policy body.\n");

        let with_asset = service
            .save_package_file_and_project(
                WorkflowGuidePackageFileSaveCommand {
                    profile_id: "workflow-profile".to_string(),
                    package_file_id: None,
                    expected_file_revision: None,
                    expected_guide_revision: None,
                    title: "Policy diagram".to_string(),
                    category: WorkflowGuidePackageCategory::Asset,
                    original_filename: "policy-diagram.pdf".to_string(),
                    bytes: b"diagram".to_vec(),
                    reclamation_confirmation: None,
                },
                temporary.path().to_path_buf(),
            )
            .await
            .expect("save asset referenced by external Markdown");
        let asset = with_asset
            .guide
            .package_files
            .iter()
            .find(|candidate| candidate.category == WorkflowGuidePackageCategory::Asset)
            .expect("saved asset")
            .clone();

        let linked = service
            .save_and_project(
                WorkflowGuideSaveCommand {
                    profile_id: "workflow-profile".to_string(),
                    expected_guide_revision: saved.guide.guide_revision,
                    markdown: format!("# Release investigation\n\n[{}]({})\n", file.title, file.relative_path),
                    reclamation_confirmation: None,
                },
                temporary.path().to_path_buf(),
            )
            .await
            .expect("link external Markdown from the root Guide");

        let updated = service
            .save_package_file_and_project(
                WorkflowGuidePackageFileSaveCommand {
                    profile_id: "workflow-profile".to_string(),
                    package_file_id: Some(file.package_file_id.clone()),
                    expected_file_revision: Some(file.file_revision),
                    expected_guide_revision: Some(linked.guide.guide_revision),
                    title: file.title.clone(),
                    category: WorkflowGuidePackageCategory::Reference,
                    original_filename: "release-policy.md".to_string(),
                    bytes: format!(
                        "# Release policy\nCurrent policy body.\n\n[Policy diagram]({})\n",
                        asset.relative_path
                    )
                    .into_bytes(),
                    reclamation_confirmation: None,
                },
                temporary.path().to_path_buf(),
            )
            .await
            .expect("save current external Markdown revision");
        let updated_file = updated
            .guide
            .package_files
            .iter()
            .find(|candidate| candidate.package_file_id == file.package_file_id)
            .expect("updated package file")
            .clone();
        let error = service
            .save_package_file_and_project(
                WorkflowGuidePackageFileSaveCommand {
                    profile_id: "workflow-profile".to_string(),
                    package_file_id: Some(updated_file.package_file_id.clone()),
                    expected_file_revision: Some(updated_file.file_revision),
                    expected_guide_revision: Some(linked.guide.guide_revision),
                    title: updated_file.title.clone(),
                    category: WorkflowGuidePackageCategory::Reference,
                    original_filename: "release-policy.md".to_string(),
                    bytes: b"# Release policy\nStale policy body.\n".to_vec(),
                    reclamation_confirmation: None,
                },
                temporary.path().to_path_buf(),
            )
            .await
            .expect_err("stale external Markdown Guide revision must conflict");
        assert!(matches!(
            error,
            WorkflowGuideError::GuideChanged {
                current_guide_revision: 2
            }
        ));

        let before_revision: i64 = sqlx::query_scalar(
            "SELECT guide_revision FROM workflow_profile_guides WHERE profile_id = 'workflow-profile'",
        )
        .fetch_one(&pool)
        .await
        .expect("read revision before preview");
        let preview = service
            .preview(WorkflowGuidePreviewCommand {
                profile_id: "workflow-profile".to_string(),
                relative_path: Some(document.relative_path.clone()),
                markdown: "# Release policy\nUnsaved preview body.\n".to_string(),
            })
            .await
            .expect("preview external Markdown draft");
        assert_eq!(
            preview.active_document.markdown,
            "# Release policy\nUnsaved preview body."
        );
        assert!(
            preview
                .projected_skill
                .markdown
                .contains("name: release-investigation-guide")
        );
        assert!(preview.projected_skill.markdown.contains("# Release investigation"));
        assert_eq!(
            preview
                .orphaned_package_files
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect::<Vec<_>>(),
            vec![asset.relative_path.as_str()]
        );
        let after_revision: i64 = sqlx::query_scalar(
            "SELECT guide_revision FROM workflow_profile_guides WHERE profile_id = 'workflow-profile'",
        )
        .fetch_one(&pool)
        .await
        .expect("read revision after preview");
        assert_eq!(after_revision, before_revision);
        assert_eq!(
            std::fs::read_to_string(
                temporary
                    .path()
                    .join("release-investigation-guide/references/release-policy.md"),
            )
            .expect("read persisted external Markdown"),
            format!(
                "# Release policy\nCurrent policy body.\n\n[Policy diagram]({})\n",
                asset.relative_path
            )
        );
        std::fs::write(
            temporary
                .path()
                .join("release-investigation-guide/references/release-policy.md"),
            "# Release policy\nUnexpected replacement.\n",
        )
        .expect("replace external Markdown outside the managed save path");
        let error = service
            .read_external_document(
                "workflow-profile",
                &file.package_file_id,
                temporary.path().to_path_buf(),
            )
            .await
            .expect_err("checksum mismatch must reject an unregistered file replacement");
        assert!(error.to_string().contains("registered checksum"));

        let external_path = temporary
            .path()
            .join("release-investigation-guide/references/release-policy.md");
        std::fs::remove_file(&external_path).expect("remove stale external projection");
        service
            .project("workflow-profile", temporary.path().to_path_buf())
            .await
            .expect("repair missing external Markdown projection");
        assert_eq!(
            std::fs::read_to_string(external_path).expect("read repaired external Markdown"),
            format!(
                "# Release policy\nCurrent policy body.\n\n[Policy diagram]({})\n",
                asset.relative_path
            )
        );

        let remove_asset = WorkflowGuidePackageFileSaveCommand {
            profile_id: "workflow-profile".to_string(),
            package_file_id: Some(updated_file.package_file_id.clone()),
            expected_file_revision: Some(updated_file.file_revision),
            expected_guide_revision: Some(updated.guide.guide_revision),
            title: updated_file.title.clone(),
            category: WorkflowGuidePackageCategory::Reference,
            original_filename: "release-policy.md".to_string(),
            bytes: b"# Release policy\nNo diagram is required.\n".to_vec(),
            reclamation_confirmation: None,
        };
        let error = service
            .save_package_file_and_project(remove_asset.clone(), temporary.path().to_path_buf())
            .await
            .expect_err("reachable external Markdown reclamation requires confirmation");
        let WorkflowGuideError::ReclamationConfirmationRequired(plan) = error else {
            panic!("expected external Markdown reclamation confirmation requirement");
        };
        assert_eq!(plan.package_files, vec![asset.clone()]);
        let reclaimed = service
            .save_package_file_and_project(
                WorkflowGuidePackageFileSaveCommand {
                    reclamation_confirmation: Some(WorkflowGuideReclamationConfirmation {
                        package_files: vec![WorkflowGuidePackageFileRevision {
                            package_file_id: asset.package_file_id,
                            file_revision: asset.file_revision,
                        }],
                        capability_names: Vec::new(),
                    }),
                    ..remove_asset
                },
                temporary.path().to_path_buf(),
            )
            .await
            .expect("save external Markdown after exact reclamation confirmation");
        assert!(
            reclaimed
                .guide
                .package_files
                .iter()
                .all(|candidate| candidate.relative_path != asset.relative_path)
        );
        assert!(
            !temporary
                .path()
                .join("release-investigation-guide")
                .join(&asset.relative_path)
                .exists(),
            "external Markdown reclamation moves the orphaned asset out of the Skill package"
        );
        assert_eq!(
            std::fs::read_to_string(
                temporary
                    .path()
                    .join("release-investigation-guide")
                    .join(&updated_file.relative_path),
            )
            .expect("read updated external Markdown after child reclamation"),
            "# Release policy\nNo diagram is required.\n",
            "saving the parent document must not reclaim or skip its replacement"
        );
    }

    #[test]
    fn rejects_package_file_extensions_outside_the_selected_category() {
        let error = validate_package_file_command(&WorkflowGuidePackageFileSaveCommand {
            profile_id: "workflow-profile".to_string(),
            package_file_id: None,
            expected_file_revision: None,
            expected_guide_revision: None,
            title: "Release policy".to_string(),
            category: WorkflowGuidePackageCategory::Script,
            original_filename: "release-policy.md".to_string(),
            bytes: b"# Release policy\n".to_vec(),
            reclamation_confirmation: None,
        })
        .expect_err("markdown is not a script file");
        assert!(error.to_string().contains("not allowed for script"));
    }

    #[tokio::test]
    async fn saves_plain_guide_without_fabricating_workflow_steps() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        crate::test_helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', '', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .expect("insert Workflow Profile");
        sqlx::query(
            "INSERT INTO workflow_profile_skills (profile_id, skill_name) VALUES ('workflow-profile', 'release-investigation-guide')",
        )
        .execute(&pool)
        .await
        .expect("configure friendly Skill name");
        let service = WorkflowGuideService::new(pool.clone());
        service.view("workflow-profile").await.expect("initialize Guide");

        let saved = service
            .save(WorkflowGuideSaveCommand {
                profile_id: "workflow-profile".to_string(),
                expected_guide_revision: 0,
                markdown: "# Release investigation\n\nRead the release logs before making a conclusion.".to_string(),
                reclamation_confirmation: None,
            })
            .await
            .expect("save Guide");

        assert_eq!(saved.guide_revision, 1);
        let step_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM workflow_profile_steps WHERE profile_id = 'workflow-profile'")
                .fetch_one(&pool)
                .await
                .expect("count Workflow steps");
        assert_eq!(step_count, 0);
    }

    #[tokio::test]
    async fn saves_reachable_package_reference_without_fabricating_steps() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        crate::test_helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', '', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .expect("insert Workflow Profile");
        sqlx::query(
            "INSERT INTO workflow_profile_package_files (
                package_file_id, profile_id, ordinal, title, category, relative_path
             ) VALUES ('package-file', 'workflow-profile', 0, 'Release policy', 'reference', 'references/release-policy.md')",
        )
        .execute(&pool)
        .await
        .expect("insert package file");
        let service = WorkflowGuideService::new(pool.clone());
        service.view("workflow-profile").await.expect("initialize Guide");

        service
            .save(WorkflowGuideSaveCommand {
                profile_id: "workflow-profile".to_string(),
                expected_guide_revision: 0,
                markdown: "# Release investigation\n\nRead [Release policy](references/release-policy.md).".to_string(),
                reclamation_confirmation: None,
            })
            .await
            .expect("save Guide");

        let step_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM workflow_profile_steps WHERE profile_id = 'workflow-profile'")
                .fetch_one(&pool)
                .await
                .expect("count Workflow steps");
        assert_eq!(step_count, 0);
    }

    #[tokio::test]
    async fn saves_and_projects_through_one_coordinated_operation() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        crate::test_helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, profile_mode)
             VALUES ('workflow-profile', 'Release investigation', '', 'shared', 'user', 'workflow')",
        )
        .execute(&pool)
        .await
        .expect("insert Workflow Profile");
        sqlx::query(
            "INSERT INTO workflow_profile_skills (profile_id, skill_name) VALUES ('workflow-profile', 'release-investigation-guide')",
        )
        .execute(&pool)
        .await
        .expect("configure friendly Skill name");
        let service = WorkflowGuideService::new(pool.clone());
        service.view("workflow-profile").await.expect("initialize Guide");
        let temporary = tempfile::tempdir().expect("create skills directory");

        let saved = service
            .save_and_project(
                WorkflowGuideSaveCommand {
                    profile_id: "workflow-profile".to_string(),
                    expected_guide_revision: 0,
                    markdown: "# Release investigation\n\nRead the logs.".to_string(),
                    reclamation_confirmation: None,
                },
                temporary.path().to_path_buf(),
            )
            .await
            .expect("save and project Guide");

        assert_eq!(saved.guide.guide_revision, 1);
        assert!(saved.projected_skill.markdown.contains("Read the logs."));
        let skill_path = temporary.path().join("release-investigation-guide/SKILL.md");
        assert_eq!(
            std::fs::read_to_string(skill_path).expect("read Skill"),
            saved.projected_skill.markdown
        );
    }

    #[test]
    fn skill_front_matter_preserves_the_configured_identity_and_validates_yaml() {
        let skill = format_skill_definition(
            "release-investigation-guide",
            "Release investigation",
            "Investigate: \"production\"\nwith care.",
            Some("Requires Playwright via MCPMate."),
            "# Release investigation",
        );

        let front_matter = skill
            .strip_prefix("---\n")
            .and_then(|value| value.split_once("---\n\n"))
            .map(|(front_matter, _)| front_matter)
            .expect("extract front matter");
        assert!(
            front_matter.starts_with("name: release-investigation-guide\n"),
            "name must precede description in front matter"
        );
        let metadata: BTreeMap<String, String> = serde_yaml::from_str(front_matter).expect("parse YAML front matter");

        assert_eq!(metadata.get("name"), Some(&"release-investigation-guide".to_string()));
        assert_eq!(
            metadata.get("description"),
            Some(&"Investigate: \"production\"\nwith care.".to_string())
        );
        assert_eq!(
            metadata.get("compatibility"),
            Some(&"Requires Playwright via MCPMate.".to_string())
        );
    }
}
