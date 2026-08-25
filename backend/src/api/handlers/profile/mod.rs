// MCPMate Proxy API handlers for Profile management
// Contains handler functions for Profile endpoints

// Re-export all public functions from submodules
pub use self::{
    authoring::{profile_authoring_save, profile_authoring_view},
    capabilities::{component_manage, prompts_list, resource_templates_list, resources_list, tools_list},
    capability_token_ledger::capability_token_ledger,
    guide::{
        workflow_guide_external_document_view, workflow_guide_package_file_delete, workflow_guide_package_file_upload,
        workflow_guide_preview, workflow_guide_repair, workflow_guide_save, workflow_guide_view,
    },
    helpers::{get_profile_or_error, get_tool_or_error, get_tool_with_details_or_error},
    materials::{
        workflow_material_delete, workflow_material_preview, workflow_material_replace, workflow_material_save,
        workflow_material_upload, workflow_materials_reorder, workflow_materials_view, workflow_step_materials_save,
    },
    mgmt::{profile_delete, profile_details, profile_list, profile_manage},
    server::{server_manage, servers_list},
    token_estimate::token_estimate,
    workflow::{workflow_specification_preview, workflow_specification_view},
};

// Submodules
mod authoring;
mod capabilities;
mod capability_token_ledger;
mod guide;
pub mod helpers;
pub(crate) mod materials;
mod mgmt;
mod server;
mod token_estimate;
mod unified_capability_query;
mod workflow;

pub(crate) async fn map_profile_management_error(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    profile_id: &str,
    dependency_server_ids: Vec<String>,
    error: mcpmate_capability_store::CatalogError,
) -> crate::api::handlers::ApiError {
    use crate::api::handlers::{ApiError, ProfileConflict};
    use mcpmate_capability_store::CatalogError;

    match error {
        CatalogError::ConcurrencyConflict {
            entity: "profile catalog dependency revisions",
            ..
        } => ApiError::ProfileConflict(ProfileConflict::catalog_dependency_changed(dependency_server_ids)),
        CatalogError::ConcurrencyConflict {
            entity: "consumer surface binding",
            ..
        } => ApiError::ProfileConflict(ProfileConflict::consumer_binding_changed(dependency_server_ids)),
        CatalogError::ConcurrencyConflict {
            entity: "profile authoring generation",
            id,
        } => {
            let generation = match sqlx::query_scalar::<_, i64>("SELECT authoring_generation FROM profile WHERE id = ?")
                .bind(&id)
                .fetch_optional(pool)
                .await
            {
                Ok(generation) => generation,
                Err(_) => {
                    return ApiError::InternalError("Failed to load Profile authoring generation".to_string());
                }
            };
            match generation {
                Some(generation) => ApiError::ProfileConflict(ProfileConflict::profile_authoring_changed(generation)),
                None => ApiError::NotFound(format!("Profile with ID '{id}' not found")),
            }
        }
        CatalogError::ConcurrencyConflict {
            entity: "profile server relationship",
            id,
        } => ApiError::Conflict(format!("Profile '{id}' server relationship changed before commit")),
        CatalogError::ConcurrencyConflict { .. } => ApiError::Conflict("Profile operation conflict".to_string()),
        CatalogError::InvalidSurfaceValue {
            field: "profile server",
            value,
        } => ApiError::InvalidProfileTarget(vec![value]),
        CatalogError::InvalidSurfaceValue { field, .. } => ApiError::BadRequest(match field {
            "profile catalog dependency revisions" => {
                "Capability dependency Server IDs do not match the selected capabilities".to_string()
            }
            _ => "Invalid Profile operation".to_string(),
        }),
        CatalogError::SurfaceNotFound { .. } => ApiError::NotFound(format!("Profile with ID '{profile_id}' not found")),
        _ => ApiError::InternalError("Profile operation failed".to_string()),
    }
}

pub(crate) async fn emit_surface_publication_audits(
    state: &std::sync::Arc<crate::api::routes::AppState>,
    actor: &str,
    profile_id: Option<&str>,
    route: &str,
    materializations: Vec<crate::core::capability::management::ConsumerMaterialization>,
) {
    for materialization in materializations {
        if !materialization.commit.effective_surface_changed {
            continue;
        }
        let Some(binding) = materialization.commit.binding else {
            continue;
        };
        let mut event = crate::audit::AuditEvent::new(
            crate::audit::AuditAction::SurfacePublish,
            crate::audit::AuditStatus::Success,
        )
        .with_http_route("POST", route)
        .with_actor(actor)
        .with_client_id(materialization.consumer_id)
        .with_target(binding.active_publication_id)
        .with_data(serde_json::json!({
            "binding_generation": binding.generation,
            "proposal_id": materialization.commit.proposal_id,
            "trigger": "management_save",
        }));
        if let Some(profile_id) = profile_id {
            event = event.with_profile_id(profile_id);
        }
        crate::audit::interceptor::emit_event(state.audit_service.as_ref(), event.build()).await;
    }
}

#[cfg(test)]
mod tests {
    use axum::{http::StatusCode, response::IntoResponse};
    use mcpmate_capability_store::CatalogError;
    use sqlx::sqlite::SqlitePoolOptions;

    #[tokio::test]
    async fn management_conflict_uses_the_actual_stale_profile_generation() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE profile (id TEXT PRIMARY KEY, authoring_generation INTEGER NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO profile VALUES ('profile-a', 0), ('profile-b', 2)")
            .execute(&pool)
            .await
            .unwrap();

        let response = super::map_profile_management_error(
            &pool,
            "profile-a",
            Vec::new(),
            CatalogError::ConcurrencyConflict {
                entity: "profile authoring generation",
                id: "profile-b".to_string(),
            },
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["details"]["currentAuthoringGeneration"], 2);
    }

    #[tokio::test]
    async fn missing_profile_relationship_maps_to_structured_conflict_without_advancing_generation() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::helpers::prepare_config_database(&pool).await;
        crate::system::settings::initialize_settings_file(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO profile (id, name, type, role, is_active, authoring_generation)
             VALUES ('profile-a', 'Profile A', 'shared', 'user', 1, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO server_config (id, name, server_type, command, enabled)
             VALUES ('server-a', 'Server A', 'stdio', '', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let error = crate::core::capability::management::ProfileSurfaceManagement::mutate_server(
            &pool,
            "profile-a",
            "server-a",
            crate::core::capability::management::ProfileRelationshipAction::Disable,
            0,
            "test",
        )
        .await
        .unwrap_err();
        let response = super::map_profile_management_error(&pool, "profile-a", vec!["server-a".to_string()], error)
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"]["message"].as_str().unwrap().contains("profile-a"));
        let generation: i64 = sqlx::query_scalar("SELECT authoring_generation FROM profile WHERE id = 'profile-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(generation, 0);
    }

    #[tokio::test]
    async fn generation_conflict_lookup_failure_is_internal_error() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        let response = super::map_profile_management_error(
            &pool,
            "profile-a",
            Vec::new(),
            CatalogError::ConcurrencyConflict {
                entity: "profile authoring generation",
                id: "profile-a".to_string(),
            },
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}

// Common imports for all submodules
pub(crate) mod common {
    pub use std::sync::Arc;

    pub use axum::{
        Json,
        extract::{Query, State},
    };

    pub use crate::{
        api::{
            handlers::ApiError,
            models::{ResponseConverter, profile::ProfileData},
            routes::AppState,
        },
        config::models::Profile,
    };

    /// Get database reference from AppState
    pub async fn get_database(state: &Arc<AppState>) -> Result<Arc<crate::config::database::Database>, ApiError> {
        match state.http_proxy.as_ref().and_then(|p| p.database.clone()) {
            Some(db) => Ok(db),
            None => Err(ApiError::InternalError("Database not available".to_string())),
        }
    }

    /// Convert Profile to ProfileResponse using unified converter
    pub fn profile_to_response(profile: &Profile) -> ProfileData {
        ResponseConverter::profile_to_response(profile)
    }
}
