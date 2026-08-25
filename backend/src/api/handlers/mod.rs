// MCP Proxy API handlers module
// Contains handler functions for API endpoints

pub mod audit;
pub mod client;
pub mod common;
pub mod inspector;
pub mod llm;
pub mod onboarding;
pub mod profile;
pub mod runtime;
pub mod secrets;
pub mod secrets_password;
pub mod server;
pub mod system;
use std::fmt;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Clone, Debug)]
pub struct ProfileConflict {
    code: &'static str,
    message: &'static str,
    current_authoring_generation: Option<i64>,
    dependency_server_ids: Vec<String>,
}

impl ProfileConflict {
    pub fn profile_authoring_changed(current_authoring_generation: i64) -> Self {
        Self {
            code: "profile_authoring_changed",
            message: "Profile was changed by another author",
            current_authoring_generation: Some(current_authoring_generation),
            dependency_server_ids: Vec::new(),
        }
    }

    pub fn catalog_dependency_changed(dependency_server_ids: Vec<String>) -> Self {
        Self {
            code: "catalog_dependency_changed",
            message: "Profile capability dependencies changed",
            current_authoring_generation: None,
            dependency_server_ids,
        }
    }

    pub fn consumer_binding_changed(dependency_server_ids: Vec<String>) -> Self {
        Self {
            code: "consumer_binding_changed",
            message: "Consumer binding changed during Profile authoring",
            current_authoring_generation: None,
            dependency_server_ids,
        }
    }
}

/// API error type
#[derive(Debug)]
pub enum ApiError {
    /// Not found error
    NotFound(String),
    /// Bad request error
    BadRequest(String),
    /// Internal server error
    InternalError(String),
    /// Service unavailable error
    ServiceUnavailable(String),
    /// Conflict error
    Conflict(String),
    /// Profile-scoped coded conflict.
    ProfileConflict(ProfileConflict),
    /// Workflow Guide save needs explicit reclamation confirmation.
    WorkflowGuideReclamationRequired(crate::core::profile::guide::WorkflowGuideReclamationPlan),
    /// Workflow Guide save committed but managed Trash cleanup needs Repair.
    WorkflowGuideTrashCleanupPending {
        relative_paths: Vec<String>,
        message: String,
    },
    /// Profile target validation failure.
    InvalidProfileTarget(Vec<String>),
    /// Forbidden error
    Forbidden(String),
    /// Timeout error
    Timeout(String),
    /// Upstream gateway timeout error
    GatewayTimeout(String),
    /// Upstream authentication error
    Unauthorized(String),
    /// Upstream/bad gateway error
    BadGateway(String),
}

impl fmt::Display for ApiError {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            ApiError::NotFound(msg) => write!(f, "Not found: {msg}"),
            ApiError::BadRequest(msg) => write!(f, "Bad request: {msg}"),
            ApiError::InternalError(msg) => write!(f, "Internal error: {msg}"),
            ApiError::ServiceUnavailable(msg) => write!(f, "Service unavailable: {msg}"),
            ApiError::Conflict(msg) => write!(f, "Conflict: {msg}"),
            ApiError::ProfileConflict(conflict) => write!(f, "Conflict: {}", conflict.message),
            ApiError::WorkflowGuideReclamationRequired(_) => {
                write!(f, "Conflict: Workflow Guide save requires reclamation confirmation")
            }
            ApiError::WorkflowGuideTrashCleanupPending { message, .. } => {
                write!(f, "Internal error: Workflow Guide Trash cleanup is pending: {message}")
            }
            ApiError::InvalidProfileTarget(_) => write!(f, "Bad request: invalid Profile target"),
            ApiError::Forbidden(msg) => write!(f, "Forbidden: {msg}"),
            ApiError::Timeout(msg) => write!(f, "Timeout: {msg}"),
            ApiError::GatewayTimeout(msg) => write!(f, "Gateway timeout: {msg}"),
            ApiError::Unauthorized(msg) => write!(f, "Unauthorized: {msg}"),
            ApiError::BadGateway(msg) => write!(f, "Bad gateway: {msg}"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let error = match self {
            ApiError::ProfileConflict(conflict) => {
                let mut details = serde_json::Map::new();
                if let Some(generation) = conflict.current_authoring_generation {
                    details.insert("currentAuthoringGeneration".to_string(), json!(generation));
                }
                if !conflict.dependency_server_ids.is_empty() {
                    details.insert("dependencyServerIds".to_string(), json!(conflict.dependency_server_ids));
                }
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": {
                            "message": conflict.message,
                            "status": StatusCode::CONFLICT.as_u16(),
                            "code": conflict.code,
                            "details": details,
                        }
                    })),
                )
                    .into_response();
            }
            ApiError::WorkflowGuideReclamationRequired(plan) => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": {
                            "message": "Workflow Guide save requires reclamation confirmation",
                            "status": StatusCode::CONFLICT.as_u16(),
                            "code": "workflow_guide_reclamation_required",
                            "details": {
                                "packageFiles": plan.package_files,
                                "capabilities": plan.capabilities,
                            },
                        }
                    })),
                )
                    .into_response();
            }
            ApiError::WorkflowGuideTrashCleanupPending {
                relative_paths,
                message,
            } => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": {
                            "message": format!(
                                "Workflow Guide save committed, but Trash cleanup is pending: {message}"
                            ),
                            "status": StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                            "code": "workflow_guide_trash_cleanup_pending",
                            "details": { "relativePaths": relative_paths },
                        }
                    })),
                )
                    .into_response();
            }
            ApiError::InvalidProfileTarget(dependency_server_ids) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": {
                            "message": "Profile target does not exist",
                            "status": StatusCode::BAD_REQUEST.as_u16(),
                            "code": "invalid_target",
                            "details": { "dependencyServerIds": dependency_server_ids },
                        }
                    })),
                )
                    .into_response();
            }
            error => error,
        };
        let (status, message) = match error {
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::InternalError(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            ApiError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, msg),
            ApiError::ProfileConflict(_)
            | ApiError::WorkflowGuideReclamationRequired(_)
            | ApiError::WorkflowGuideTrashCleanupPending { .. }
            | ApiError::InvalidProfileTarget(_) => unreachable!(),
            ApiError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg),
            ApiError::Timeout(msg) => (StatusCode::REQUEST_TIMEOUT, msg),
            ApiError::GatewayTimeout(msg) => (StatusCode::GATEWAY_TIMEOUT, msg),
            ApiError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg),
            ApiError::BadGateway(msg) => (StatusCode::BAD_GATEWAY, msg),
        };

        let body = Json(json!({
            "error": {
                "message": message,
                "status": status.as_u16()
            }
        }));

        (status, body).into_response()
    }
}

/// Convert anyhow errors to API errors
impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError::InternalError(err.to_string())
    }
}

/// Convert a bare status code (from shared helpers that predate typed errors) into an
/// ApiError, preserving a best-effort reason so the response body is never empty.
impl From<StatusCode> for ApiError {
    fn from(status: StatusCode) -> Self {
        let message = status.canonical_reason().unwrap_or("Request failed").to_string();
        match status {
            StatusCode::NOT_FOUND => ApiError::NotFound(message),
            StatusCode::BAD_REQUEST => ApiError::BadRequest(message),
            StatusCode::CONFLICT => ApiError::Conflict(message),
            StatusCode::FORBIDDEN => ApiError::Forbidden(message),
            StatusCode::UNAUTHORIZED => ApiError::Unauthorized(message),
            StatusCode::SERVICE_UNAVAILABLE => ApiError::ServiceUnavailable(message),
            StatusCode::REQUEST_TIMEOUT => ApiError::Timeout(message),
            StatusCode::GATEWAY_TIMEOUT => ApiError::GatewayTimeout(message),
            StatusCode::BAD_GATEWAY => ApiError::BadGateway(message),
            _ => ApiError::InternalError(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_unavailable_errors_return_503() {
        let response = ApiError::ServiceUnavailable("secure store unavailable".to_string()).into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn ordinary_timeout_errors_remain_request_timeout() {
        let response = ApiError::Timeout("operation timed out".to_string()).into_response();

        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    }

    #[test]
    fn unauthorized_errors_return_401() {
        let response = ApiError::Unauthorized("upstream auth failed".to_string()).into_response();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn bad_gateway_errors_return_502() {
        let response = ApiError::BadGateway("upstream discovery failed".to_string()).into_response();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn status_code_conversion_preserves_a_reason_in_the_body() {
        let error: ApiError = StatusCode::NOT_FOUND.into();
        match error {
            ApiError::NotFound(msg) => assert_eq!(msg, "Not Found"),
            other => panic!("Expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn profile_authoring_conflict_returns_stable_code_and_generation_details() {
        let response = ApiError::ProfileConflict(ProfileConflict::profile_authoring_changed(13)).into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "profile_authoring_changed");
        assert_eq!(json["error"]["details"]["currentAuthoringGeneration"], 13);
    }

    #[tokio::test]
    async fn workflow_guide_reclamation_conflict_returns_structured_candidates() {
        use crate::core::profile::guide::{
            WorkflowGuideCapability, WorkflowGuidePackageCategory, WorkflowGuidePackageFile,
            WorkflowGuideReclamationPlan,
        };

        let response = ApiError::WorkflowGuideReclamationRequired(WorkflowGuideReclamationPlan {
            package_files: vec![WorkflowGuidePackageFile {
                package_file_id: "file-1".to_string(),
                file_revision: 3,
                title: "Policy".to_string(),
                category: WorkflowGuidePackageCategory::Reference,
                relative_path: "references/policy.md".to_string(),
                mime_type: Some("text/markdown".to_string()),
                extension: Some("md".to_string()),
                file_size: Some(10),
            }],
            capabilities: vec![WorkflowGuideCapability {
                name: "lookup".to_string(),
                exposure: crate::core::profile::workflow::WorkflowBindingPolicy::Direct,
                guide: "Use the lookup capability.".to_string(),
                start_line: 1,
                end_line: 3,
            }],
        })
        .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "workflow_guide_reclamation_required");
        assert_eq!(json["error"]["details"]["packageFiles"][0]["file_revision"], 3);
        assert_eq!(json["error"]["details"]["capabilities"][0]["name"], "lookup");
    }

    #[tokio::test]
    async fn workflow_guide_trash_cleanup_pending_returns_stable_repair_contract() {
        let response = ApiError::WorkflowGuideTrashCleanupPending {
            relative_paths: vec!["references/policy.md".to_string()],
            message: "native Trash is unavailable".to_string(),
        }
        .into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "workflow_guide_trash_cleanup_pending");
        assert_eq!(
            json["error"]["details"]["relativePaths"],
            json!(["references/policy.md"])
        );
        assert!(json["error"]["message"].as_str().unwrap().contains("save committed"));
    }

    #[tokio::test]
    async fn invalid_profile_target_returns_coded_bad_request_without_internal_details() {
        let response = ApiError::InvalidProfileTarget(vec!["missing".to_string()]).into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "invalid_target");
        assert_eq!(json["error"]["details"]["dependencyServerIds"], json!(["missing"]));
        assert!(json.to_string().find("sql").is_none());
    }
}
