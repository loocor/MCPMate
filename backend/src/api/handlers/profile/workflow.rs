use super::common::*;
use crate::{
    api::models::profile::{
        ProfileIdReq, WorkflowSpecificationPreviewData, WorkflowSpecificationPreviewResp,
        WorkflowSpecificationViewData, WorkflowSpecificationViewResp,
    },
    core::profile::workflow::{WorkflowSpecificationError, WorkflowSpecificationService},
};

pub async fn workflow_specification_view(
    State(state): State<Arc<AppState>>,
    Query(request): Query<ProfileIdReq>,
) -> Result<Json<WorkflowSpecificationViewResp>, ApiError> {
    let db = get_database(&state).await?;
    let specification = WorkflowSpecificationService::new(db.pool.clone())
        .view(&request.id)
        .await
        .map_err(workflow_specification_error)?;
    Ok(Json(WorkflowSpecificationViewResp::success(
        WorkflowSpecificationViewData { specification },
    )))
}

pub async fn workflow_specification_preview(
    State(state): State<Arc<AppState>>,
    Query(request): Query<ProfileIdReq>,
) -> Result<Json<WorkflowSpecificationPreviewResp>, ApiError> {
    let db = get_database(&state).await?;
    let preview = WorkflowSpecificationService::new(db.pool.clone())
        .preview(&request.id)
        .await
        .map_err(workflow_specification_error)?;
    Ok(Json(WorkflowSpecificationPreviewResp::success(
        WorkflowSpecificationPreviewData { preview },
    )))
}

pub(super) fn workflow_specification_error(error: WorkflowSpecificationError) -> ApiError {
    match error {
        WorkflowSpecificationError::NotFound { profile_id } => {
            ApiError::NotFound(format!("Workflow Profile with ID '{profile_id}' not found"))
        }
        WorkflowSpecificationError::InvalidProfileMode { profile_id } => {
            ApiError::BadRequest(format!("Profile '{profile_id}' is not a workflow Profile"))
        }
        WorkflowSpecificationError::InvalidRequest(message) => ApiError::BadRequest(message),
        WorkflowSpecificationError::InvalidBinding { ref_id } => {
            ApiError::BadRequest(format!("Workflow capability binding '{ref_id}' is unavailable"))
        }
        WorkflowSpecificationError::SpecificationChanged {
            current_specification_revision,
        } => ApiError::Conflict(format!(
            "Workflow specification was changed by another author (current revision {current_specification_revision})"
        )),
        WorkflowSpecificationError::Database(_) | WorkflowSpecificationError::Capability(_) => {
            ApiError::InternalError("Workflow specification failed".to_string())
        }
        WorkflowSpecificationError::SurfacePublication(message) => ApiError::InternalError(message),
    }
}
