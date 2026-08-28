/// Unified response converter service to eliminate model conversion duplication
pub struct ResponseConverter;

/// Generic success response
#[derive(serde::Serialize)]
pub struct SuccessResp {
    /// Success message
    pub message: String,
}

/// Generic error response
#[derive(serde::Serialize)]
pub struct ErrorResp {
    /// Error details
    pub error: ErrorDetails,
}

/// Error details
#[derive(serde::Serialize)]
pub struct ErrorDetails {
    /// Error message
    pub message: String,
    /// HTTP status code
    pub status: u16,
}

/// Profile-scoped error response documented for authoring and component contracts.
#[derive(schemars::JsonSchema, serde::Serialize)]
pub struct ProfileApiErrorResp {
    pub error: ProfileApiErrorDetails,
}

#[derive(schemars::JsonSchema, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileApiErrorDetails {
    pub message: String,
    pub status: u16,
    pub code: Option<String>,
    pub details: Option<ProfileApiErrorMetadata>,
}

#[derive(schemars::JsonSchema, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileApiErrorMetadata {
    pub current_authoring_generation: Option<i64>,
    pub dependency_server_ids: Option<Vec<String>>,
}

impl ResponseConverter {
    /// Convert Profile to ProfileResponse with consistent logic
    pub fn profile_to_response(profile: &crate::config::models::Profile) -> crate::api::models::profile::ProfileData {
        let mut allowed_operations = Vec::new();
        let is_default_anchor = profile.role.is_default_anchor();

        // Add allowed operations based on current state
        if profile.is_active && !is_default_anchor {
            allowed_operations.push("deactivate".to_string());
        } else if !profile.is_active {
            allowed_operations.push("activate".to_string());
        }

        // Always allow update and delete
        allowed_operations.push("update".to_string());
        if !is_default_anchor {
            allowed_operations.push("delete".to_string());
        }

        crate::api::models::profile::ProfileData {
            id: profile.id.clone().unwrap_or_default(),
            name: profile.name.clone(),
            description: profile.description.clone(),
            profile_type: profile.profile_type_string(),
            role: profile.role.to_string(),
            priority: profile.priority,
            is_active: profile.is_active,
            is_default: profile.is_default,
            authoring_generation: profile.authoring_generation,
            profile_mode: profile.profile_mode,
            allowed_operations,
        }
    }

    /// Convert Server to ProfileServerResponse with consistent logic
    pub fn server_to_profile_response(
        server: &crate::config::models::Server,
        enabled: bool,
    ) -> crate::api::models::profile::ProfileServerResp {
        let mut allowed_operations = Vec::new();
        if enabled {
            allowed_operations.push("disable".to_string());
        } else {
            allowed_operations.push("enable".to_string());
        }

        crate::api::models::profile::ProfileServerResp {
            id: server.id.clone().unwrap_or_default(),
            name: server.name.clone(),
            enabled,
            allowed_operations,
        }
    }

    /// Format timestamps consistently across all API responses
    pub fn format_timestamp(timestamp: Option<chrono::DateTime<chrono::Utc>>) -> Option<String> {
        timestamp.map(|dt| dt.to_rfc3339())
    }

    /// Create consistent error response
    pub fn create_error_response(
        message: &str,
        status: u16,
    ) -> ErrorResp {
        ErrorResp {
            error: ErrorDetails {
                message: message.to_string(),
                status,
            },
        }
    }
}
