use std::sync::Arc;

use axum::extract::Multipart;

use super::common::*;
use crate::{
    api::models::profile::{
        ProfileIdReq, WorkflowGuideExternalDocumentData, WorkflowGuideExternalDocumentReq,
        WorkflowGuideExternalDocumentResp, WorkflowGuidePackageFileDeleteReq, WorkflowGuidePackageFileSaveData,
        WorkflowGuidePackageFileSaveResp, WorkflowGuidePreviewData, WorkflowGuidePreviewReq, WorkflowGuidePreviewResp,
        WorkflowGuideRepairReq, WorkflowGuideSaveData, WorkflowGuideSaveReq, WorkflowGuideSaveResp,
        WorkflowGuideViewData, WorkflowGuideViewResp,
    },
    core::profile::guide::{
        WorkflowGuideError, WorkflowGuidePackageCategory, WorkflowGuidePackageFileSaveCommand,
        WorkflowGuidePreviewCommand, WorkflowGuideReclamationConfirmation, WorkflowGuideSaveCommand,
        WorkflowGuideService,
    },
};

pub async fn workflow_guide_view(
    State(state): State<Arc<AppState>>,
    Query(request): Query<ProfileIdReq>,
) -> Result<Json<WorkflowGuideViewResp>, ApiError> {
    let db = get_database(&state).await?;
    let guide = WorkflowGuideService::new(db.pool.clone())
        .view(&request.id)
        .await
        .map_err(workflow_guide_error)?;
    Ok(Json(WorkflowGuideViewResp::success(WorkflowGuideViewData { guide })))
}

pub async fn workflow_guide_save(
    State(state): State<Arc<AppState>>,
    Json(request): Json<WorkflowGuideSaveReq>,
) -> Result<Json<WorkflowGuideSaveResp>, ApiError> {
    let profile_id = request.profile_id.clone();
    let db = get_database(&state).await?;
    let skills_root = db.path.parent().unwrap_or(std::path::Path::new(".")).join("skills");
    let saved = WorkflowGuideService::new(db.pool.clone())
        .save_and_project(
            WorkflowGuideSaveCommand {
                profile_id: request.profile_id,
                expected_guide_revision: request.expected_guide_revision,
                markdown: request.markdown,
                reclamation_confirmation: request.reclamation_confirmation.map(Into::into),
            },
            skills_root,
        )
        .await
        .map_err(workflow_guide_error)?;
    audit_workflow_guide_change(&state, profile_id, "POST", "/api/mcp/profile/workflow/guide/save").await;
    Ok(Json(WorkflowGuideSaveResp::success(WorkflowGuideSaveData {
        guide: saved.guide,
        projected_skill: saved.projected_skill,
    })))
}

pub async fn workflow_guide_preview(
    State(state): State<Arc<AppState>>,
    Json(request): Json<WorkflowGuidePreviewReq>,
) -> Result<Json<WorkflowGuidePreviewResp>, ApiError> {
    let db = get_database(&state).await?;
    let preview = WorkflowGuideService::new(db.pool.clone())
        .preview(WorkflowGuidePreviewCommand {
            profile_id: request.profile_id,
            relative_path: request.relative_path,
            markdown: request.markdown,
        })
        .await
        .map_err(workflow_guide_error)?;
    Ok(Json(WorkflowGuidePreviewResp::success(WorkflowGuidePreviewData {
        projected_skill: preview.projected_skill,
        active_document: preview.active_document,
        orphaned_package_files: preview.orphaned_package_files,
        orphaned_capabilities: preview.orphaned_capabilities,
    })))
}

pub async fn workflow_guide_external_document_view(
    State(state): State<Arc<AppState>>,
    Query(request): Query<WorkflowGuideExternalDocumentReq>,
) -> Result<Json<WorkflowGuideExternalDocumentResp>, ApiError> {
    let db = get_database(&state).await?;
    let skills_root = db.path.parent().unwrap_or(std::path::Path::new(".")).join("skills");
    let document = WorkflowGuideService::new(db.pool.clone())
        .read_external_document(&request.profile_id, &request.package_file_id, skills_root)
        .await
        .map_err(workflow_guide_error)?;
    Ok(Json(WorkflowGuideExternalDocumentResp::success(
        WorkflowGuideExternalDocumentData { document },
    )))
}

pub async fn workflow_guide_package_file_upload(
    State(state): State<Arc<AppState>>,
    multipart: Multipart,
) -> Result<Json<WorkflowGuidePackageFileSaveResp>, ApiError> {
    let upload = parse_package_file_upload(multipart).await?;
    let profile_id = upload.profile_id.clone();
    let db = get_database(&state).await?;
    let skills_root = db.path.parent().unwrap_or(std::path::Path::new(".")).join("skills");
    let saved = WorkflowGuideService::new(db.pool.clone())
        .save_package_file_and_project(
            WorkflowGuidePackageFileSaveCommand {
                profile_id: upload.profile_id,
                package_file_id: upload.package_file_id,
                expected_file_revision: upload.expected_file_revision,
                expected_guide_revision: upload.expected_guide_revision,
                title: upload.title,
                category: upload.category,
                original_filename: upload.filename,
                bytes: upload.bytes,
                reclamation_confirmation: upload.reclamation_confirmation,
            },
            skills_root,
        )
        .await
        .map_err(workflow_guide_error)?;
    audit_workflow_guide_change(
        &state,
        profile_id,
        "POST",
        "/api/mcp/profile/workflow/guide/package-files/upload",
    )
    .await;
    Ok(Json(WorkflowGuidePackageFileSaveResp::success(
        WorkflowGuidePackageFileSaveData {
            guide: saved.guide,
            projected_skill: saved.projected_skill,
            package_file: saved.package_file,
        },
    )))
}

pub async fn workflow_guide_package_file_delete(
    State(state): State<Arc<AppState>>,
    Json(request): Json<WorkflowGuidePackageFileDeleteReq>,
) -> Result<Json<WorkflowGuideSaveResp>, ApiError> {
    let profile_id = request.profile_id.clone();
    let db = get_database(&state).await?;
    let skills_root = db.path.parent().unwrap_or(std::path::Path::new(".")).join("skills");
    let saved = WorkflowGuideService::new(db.pool.clone())
        .delete_package_file_and_project(
            &request.profile_id,
            &request.package_file_id,
            request.expected_file_revision,
            request.expected_guide_revision,
            skills_root,
        )
        .await
        .map_err(workflow_guide_error)?;
    audit_workflow_guide_change(
        &state,
        profile_id,
        "DELETE",
        "/api/mcp/profile/workflow/guide/package-files/delete",
    )
    .await;
    Ok(Json(WorkflowGuideSaveResp::success(WorkflowGuideSaveData {
        guide: saved.guide,
        projected_skill: saved.projected_skill,
    })))
}

pub async fn workflow_guide_repair(
    State(state): State<Arc<AppState>>,
    Json(request): Json<WorkflowGuideRepairReq>,
) -> Result<Json<WorkflowGuideSaveResp>, ApiError> {
    let profile_id = request.profile_id;
    let db = get_database(&state).await?;
    let skills_root = db.path.parent().unwrap_or(std::path::Path::new(".")).join("skills");
    let service = WorkflowGuideService::new(db.pool.clone());
    let projected_skill = service
        .project(&profile_id, skills_root)
        .await
        .map_err(workflow_guide_error)?;
    let guide = service.view(&profile_id).await.map_err(workflow_guide_error)?;
    audit_workflow_guide_change(&state, profile_id, "POST", "/api/mcp/profile/workflow/guide/repair").await;
    Ok(Json(WorkflowGuideSaveResp::success(WorkflowGuideSaveData {
        guide,
        projected_skill,
    })))
}

impl From<crate::api::models::profile::WorkflowGuideReclamationConfirmationReq>
    for WorkflowGuideReclamationConfirmation
{
    fn from(value: crate::api::models::profile::WorkflowGuideReclamationConfirmationReq) -> Self {
        Self {
            package_files: value
                .package_files
                .into_iter()
                .map(|file| crate::core::profile::guide::WorkflowGuidePackageFileRevision {
                    package_file_id: file.package_file_id,
                    file_revision: file.file_revision,
                })
                .collect(),
            capability_names: value.capability_names,
        }
    }
}

async fn audit_workflow_guide_change(
    state: &Arc<AppState>,
    profile_id: String,
    method: &'static str,
    route: &'static str,
) {
    crate::audit::interceptor::emit_event(
        state.audit_service.as_ref(),
        crate::audit::interceptor::build_rest_event(
            crate::audit::AuditAction::ProfileUpdate,
            crate::audit::AuditStatus::Success,
            method,
            route,
            None,
            None,
            Some(profile_id),
            None,
            None,
        ),
    )
    .await;
}

fn workflow_guide_error(error: WorkflowGuideError) -> ApiError {
    match error {
        WorkflowGuideError::Workflow(error) => super::workflow::workflow_specification_error(error),
        WorkflowGuideError::GuideChanged { current_guide_revision } => ApiError::Conflict(format!(
            "Workflow Guide was changed by another author (current revision {current_guide_revision})"
        )),
        WorkflowGuideError::PackageFileChanged { current_file_revision } => ApiError::Conflict(format!(
            "Workflow Guide package file was changed by another author (current revision {current_file_revision})"
        )),
        WorkflowGuideError::ReclamationConfirmationRequired(plan) => ApiError::WorkflowGuideReclamationRequired(plan),
        WorkflowGuideError::ReclamationConfirmationChanged => {
            ApiError::Conflict("Workflow Guide reclamation candidates changed before confirmation".to_string())
        }
        WorkflowGuideError::TrashCleanupPending {
            relative_paths,
            message,
        } => ApiError::WorkflowGuideTrashCleanupPending {
            relative_paths,
            message,
        },
        WorkflowGuideError::InvalidStorage(message) => ApiError::BadRequest(message),
        WorkflowGuideError::Database(_) | WorkflowGuideError::Projection(_) => {
            ApiError::InternalError("Workflow Guide operation failed".to_string())
        }
    }
}

struct PackageFileUpload {
    profile_id: String,
    package_file_id: Option<String>,
    expected_file_revision: Option<i64>,
    expected_guide_revision: Option<i64>,
    reclamation_confirmation: Option<WorkflowGuideReclamationConfirmation>,
    title: String,
    category: WorkflowGuidePackageCategory,
    filename: String,
    bytes: Vec<u8>,
}

async fn parse_package_file_upload(mut multipart: Multipart) -> Result<PackageFileUpload, ApiError> {
    let (
        mut profile_id,
        mut package_file_id,
        mut expected_file_revision,
        mut expected_guide_revision,
        mut reclamation_confirmation,
        mut title,
        mut category,
        mut filename,
        mut bytes,
    ) = (None, None, None, None, None, None, None, None, None);
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::BadRequest(format!("invalid multipart body: {error}")))?
    {
        match field.name().unwrap_or_default() {
            "file" => {
                filename = field.file_name().map(ToOwned::to_owned);
                let content = field
                    .bytes()
                    .await
                    .map_err(|error| ApiError::BadRequest(format!("failed to read upload: {error}")))?;
                if content.len() > crate::core::profile::materials::MAX_UPLOAD_BYTES {
                    return Err(ApiError::BadRequest("uploaded file exceeds 5 MiB".to_string()));
                }
                bytes = Some(content.to_vec());
            }
            "profile_id" => profile_id = Some(field.text().await.map_err(invalid_upload_field)?),
            "package_file_id" => package_file_id = Some(field.text().await.map_err(invalid_upload_field)?),
            "expected_file_revision" => {
                expected_file_revision =
                    Some(
                        field.text().await.map_err(invalid_upload_field)?.parse().map_err(|_| {
                            ApiError::BadRequest("expected_file_revision must be an integer".to_string())
                        })?,
                    )
            }
            "expected_guide_revision" => {
                expected_guide_revision =
                    Some(
                        field.text().await.map_err(invalid_upload_field)?.parse().map_err(|_| {
                            ApiError::BadRequest("expected_guide_revision must be an integer".to_string())
                        })?,
                    )
            }
            "reclamation_confirmation" => {
                reclamation_confirmation = Some(
                    serde_json::from_str(&field.text().await.map_err(invalid_upload_field)?).map_err(|error| {
                        ApiError::BadRequest(format!("invalid reclamation confirmation payload: {error}"))
                    })?,
                )
            }
            "title" => title = Some(field.text().await.map_err(invalid_upload_field)?),
            "category" => {
                category = Some(
                    field
                        .text()
                        .await
                        .map_err(invalid_upload_field)?
                        .parse()
                        .map_err(|_| ApiError::BadRequest("invalid package-file category".to_string()))?,
                )
            }
            _ => return Err(ApiError::BadRequest("unexpected multipart field".to_string())),
        }
    }
    let package_file_id = package_file_id.filter(|value: &String| !value.trim().is_empty());
    if package_file_id.is_some() != expected_file_revision.is_some() {
        return Err(ApiError::BadRequest(
            "package_file_id and expected_file_revision must be supplied together".to_string(),
        ));
    }
    Ok(PackageFileUpload {
        profile_id: profile_id
            .filter(|value: &String| !value.trim().is_empty())
            .ok_or_else(|| ApiError::BadRequest("profile_id is required".to_string()))?,
        package_file_id,
        expected_file_revision,
        expected_guide_revision,
        reclamation_confirmation,
        title: title
            .filter(|value: &String| !value.trim().is_empty())
            .ok_or_else(|| ApiError::BadRequest("title is required".to_string()))?,
        category: category.ok_or_else(|| ApiError::BadRequest("category is required".to_string()))?,
        filename: filename.ok_or_else(|| ApiError::BadRequest("file is required".to_string()))?,
        bytes: bytes.ok_or_else(|| ApiError::BadRequest("file is required".to_string()))?,
    })
}

fn invalid_upload_field(error: axum::extract::multipart::MultipartError) -> ApiError {
    ApiError::BadRequest(format!("invalid multipart field: {error}"))
}
