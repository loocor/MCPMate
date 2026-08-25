// MCPMate Proxy API handlers for Profile capabilities management
// Contains handler functions for managing tools, resources, and prompts in Profile

use super::{common::*, helpers::get_profile_or_error};
use crate::api::models::profile::{
    ComponentOperationResult, ProfileComponentAction, ProfileComponentListReq, ProfileComponentManageReq,
    ProfilePromptData, ProfilePromptsListData, ProfilePromptsListResp, ProfileResourceData,
    ProfileResourceTemplateData, ProfileResourceTemplatesListData, ProfileResourceTemplatesListResp,
    ProfileResourcesListData, ProfileResourcesListResp, ProfileServerManageData, ProfileServerManageResp,
    ProfileToolData, ProfileToolsListData, ProfileToolsListResp,
};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

type CapabilityAuditDetails = Value;

// Component type enumeration for type-safe operations
#[derive(Debug, Clone, Copy)]
enum ComponentType {
    Tool,
    Resource,
    Prompt,
    ResourceTemplate,
}

impl ComponentType {
    fn from_kind(kind: &str) -> Result<Self, ApiError> {
        match kind {
            "tools" => Ok(Self::Tool),
            "resources" => Ok(Self::Resource),
            "prompts" => Ok(Self::Prompt),
            "resource_templates" => Ok(Self::ResourceTemplate),
            _ => Err(ApiError::NotFound(format!("Unknown Capability kind: {kind}"))),
        }
    }

    /// Get component type name as string
    fn as_str(&self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Resource => "resource",
            Self::Prompt => "prompt",
            Self::ResourceTemplate => "resource_template",
        }
    }
}

/// List prompts in a profile (standardized version)
///
/// **Endpoint:** `GET /mcp/profile/prompts/list?profile_id={profile_id}&enabled_only={bool}`
pub async fn prompts_list(
    State(state): State<Arc<AppState>>,
    Query(request): Query<ProfileComponentListReq>,
) -> Result<Json<ProfilePromptsListResp>, ApiError> {
    let db = get_database(&state).await?;
    let mut transaction = db.pool.begin().await.map_err(snapshot_error)?;
    let profile = load_profile_in_transaction(&mut transaction, &request.profile_id).await?;

    // Get prompts in the profile
    let prompt_configs =
        crate::config::profile::prompt::get_prompts_for_profile_in_transaction(&mut transaction, &request.profile_id)
            .await
            .map_err(|e| ApiError::InternalError(format!("Failed to get profile prompts: {e}")))?;
    let mut prompts = Vec::new();
    for config in prompt_configs {
        let allowed_operations: Vec<String> = allowed_ops(config.enabled);
        prompts.push(ProfilePromptData {
            ref_id: config.id.unwrap_or_default(),
            server_id: config.server_id.clone(),
            server_name: config.server_name.clone(),
            prompt_name: config.prompt_name.clone(),
            unique_name: config.unique_name,
            description: config.description,
            enabled: config.enabled,
            state: config.state,
            state_generation: config.state_generation,
            allowed_operations,
        });
    }

    // Apply enabled filter if requested
    if request.enabled_only.unwrap_or(false) {
        prompts.retain(|p| p.enabled);
    }

    let total = prompts.len();
    let source_revision_set = load_related_revision_set(
        &mut transaction,
        prompts.iter().map(|prompt| prompt.server_id.clone()).collect(),
    )
    .await?;
    transaction.commit().await.map_err(snapshot_error)?;
    let response = ProfilePromptsListData {
        profile_id: request.profile_id,
        profile_name: profile.name,
        prompts,
        total,
        authoring_generation: profile.authoring_generation,
        source_revision_set,
    };

    Ok(Json(ProfilePromptsListResp::success(response)))
}

/// List resources in a profile (standardized version)
///
/// **Endpoint:** `GET /mcp/profile/resources/list?profile_id={profile_id}&enabled_only={bool}`
pub async fn resources_list(
    State(state): State<Arc<AppState>>,
    Query(request): Query<ProfileComponentListReq>,
) -> Result<Json<ProfileResourcesListResp>, ApiError> {
    let db = get_database(&state).await?;
    let mut transaction = db.pool.begin().await.map_err(snapshot_error)?;
    let profile = load_profile_in_transaction(&mut transaction, &request.profile_id).await?;

    // Get resources in the profile
    let resource_configs = crate::config::profile::resource::get_resources_for_profile_in_transaction(
        &mut transaction,
        &request.profile_id,
    )
    .await
    .map_err(|e| ApiError::InternalError(format!("Failed to get profile resources: {e}")))?;
    let mut resources = Vec::new();
    for config in resource_configs {
        let allowed_operations: Vec<String> = allowed_ops(config.enabled);
        resources.push(ProfileResourceData {
            ref_id: config.id.unwrap_or_default(),
            server_id: config.server_id.clone(),
            server_name: config.server_name.clone(),
            resource_uri: config.resource_uri.clone(),
            unique_uri: config.unique_uri,
            description: config.description,
            enabled: config.enabled,
            state: config.state,
            state_generation: config.state_generation,
            allowed_operations,
        });
    }

    // Apply enabled filter if requested
    if request.enabled_only.unwrap_or(false) {
        resources.retain(|r| r.enabled);
    }

    let total = resources.len();
    let source_revision_set = load_related_revision_set(
        &mut transaction,
        resources.iter().map(|resource| resource.server_id.clone()).collect(),
    )
    .await?;
    transaction.commit().await.map_err(snapshot_error)?;
    let response = ProfileResourcesListData {
        profile_id: request.profile_id,
        profile_name: profile.name,
        resources,
        total,
        authoring_generation: profile.authoring_generation,
        source_revision_set,
    };

    Ok(Json(ProfileResourcesListResp::success(response)))
}

/// List resource templates in a profile (standardized version)
///
/// **Endpoint:** `GET /mcp/profile/resource-templates/list?profile_id={profile_id}&enabled_only={bool}`
pub async fn resource_templates_list(
    State(state): State<Arc<AppState>>,
    Query(request): Query<ProfileComponentListReq>,
) -> Result<Json<ProfileResourceTemplatesListResp>, ApiError> {
    let db = get_database(&state).await?;
    let mut transaction = db.pool.begin().await.map_err(snapshot_error)?;
    let profile = load_profile_in_transaction(&mut transaction, &request.profile_id).await?;

    let template_configs =
        crate::config::profile::resource_template::get_resource_templates_for_profile_in_transaction(
            &mut transaction,
            &request.profile_id,
        )
        .await
        .map_err(|e| ApiError::InternalError(format!("Failed to get profile resource templates: {e}")))?;

    let mut templates = Vec::new();
    for config in template_configs {
        let allowed_operations: Vec<String> = allowed_ops(config.enabled);
        templates.push(ProfileResourceTemplateData {
            ref_id: config.id.unwrap_or_default(),
            server_id: config.server_id.clone(),
            server_name: config.server_name.clone(),
            uri_template: config.resource_uri.clone(),
            unique_uri_template: config.unique_uri,
            description: config.description,
            enabled: config.enabled,
            state: config.state,
            state_generation: config.state_generation,
            allowed_operations,
        });
    }

    if request.enabled_only.unwrap_or(false) {
        templates.retain(|t| t.enabled);
    }

    let total = templates.len();
    let source_revision_set = load_related_revision_set(
        &mut transaction,
        templates.iter().map(|template| template.server_id.clone()).collect(),
    )
    .await?;
    transaction.commit().await.map_err(snapshot_error)?;
    let response = ProfileResourceTemplatesListData {
        profile_id: request.profile_id,
        profile_name: profile.name,
        templates,
        total,
        authoring_generation: profile.authoring_generation,
        source_revision_set,
    };

    Ok(Json(ProfileResourceTemplatesListResp::success(response)))
}

/// List tools in a profile (standardized version)
///
/// **Endpoint:** `GET /mcp/profile/tools/list?profile_id={profile_id}&enabled_only={bool}`
pub async fn tools_list(
    State(state): State<Arc<AppState>>,
    Query(request): Query<ProfileComponentListReq>,
) -> Result<Json<ProfileToolsListResp>, ApiError> {
    let db = get_database(&state).await?;
    let response = load_profile_tools_list_data(db.as_ref(), request).await?;
    Ok(Json(ProfileToolsListResp::success(response)))
}

async fn load_profile_tools_list_data(
    db: &crate::config::database::Database,
    request: ProfileComponentListReq,
) -> Result<ProfileToolsListData, ApiError> {
    let mut transaction = db.pool.begin().await.map_err(snapshot_error)?;
    let profile = load_profile_in_transaction(&mut transaction, &request.profile_id).await?;
    let tool_configs =
        crate::config::profile::tool::get_profile_tools_in_transaction(&mut transaction, &request.profile_id)
            .await
            .map_err(|e| ApiError::InternalError(format!("Failed to get profile tools: {e}")))?;
    let mut tools = tool_configs.into_iter().map(profile_tool_data).collect::<Vec<_>>();

    if request.enabled_only.unwrap_or(false) {
        tools.retain(|t| t.enabled);
    }

    let total = tools.len();
    let source_revision_set = load_related_revision_set(
        &mut transaction,
        tools.iter().map(|tool| tool.server_id.clone()).collect(),
    )
    .await?;
    transaction.commit().await.map_err(snapshot_error)?;
    Ok(ProfileToolsListData {
        profile_id: request.profile_id,
        profile_name: profile.name,
        tools,
        total,
        authoring_generation: profile.authoring_generation,
        source_revision_set,
    })
}

async fn load_related_revision_set(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    server_ids: BTreeSet<String>,
) -> Result<crate::api::models::CatalogRevisionSet, ApiError> {
    let mut revisions = crate::api::models::CatalogRevisionSet::new();
    for server_id in server_ids {
        let revision = sqlx::query_scalar::<_, i64>(
            "SELECT catalog_revision FROM capability_server_snapshots WHERE server_id = ?",
        )
        .bind(&server_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(|_| ApiError::InternalError("Failed to load Profile capability dependencies".to_string()))?
        .ok_or_else(|| ApiError::InternalError("Profile capability dependency is unavailable".to_string()))?;
        revisions.insert(server_id, revision);
    }
    Ok(revisions)
}

async fn load_profile_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    profile_id: &str,
) -> Result<crate::config::models::Profile, ApiError> {
    sqlx::query_as("SELECT * FROM profile WHERE id = ?")
        .bind(profile_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(snapshot_error)?
        .ok_or_else(|| ApiError::NotFound(format!("Profile with ID '{profile_id}' not found")))
}

fn snapshot_error(error: sqlx::Error) -> ApiError {
    ApiError::InternalError(format!("Failed to load Profile projection: {error}"))
}

/// Manage capability operations (enable/disable tools, resources, prompts)
/// Supports both single and batch operations for enhanced performance
///
/// **Endpoint:** `POST /mcp/profile/tools/manage`, `POST /mcp/profile/resources/manage`, `POST /mcp/profile/prompts/manage`
pub async fn component_manage(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ProfileComponentManageReq>,
) -> Result<Json<ProfileServerManageResp>, ApiError> {
    let started_at = std::time::Instant::now();
    let db = get_database(&state).await?;

    // Verify profile exists
    let profile = get_profile_or_error(&db, &request.profile_id).await?;

    // Validate component IDs
    validate_component_ids(&request)?;
    let enabled = matches!(request.action, ProfileComponentAction::Enable);

    let capability_details = collect_capability_audit_details(&db, &request.component_ids).await;
    let audit_server_id = extract_single_component_string(&capability_details, "server_id");

    // Execute unified operations (single or batch)
    let result = execute_unified_operations(&state, &request).await;
    let mut data = Map::new();
    data.insert(
        "component_count".to_string(),
        Value::from(request.component_ids.len() as u64),
    );
    data.insert("profile_name".to_string(), Value::String(profile.name.clone()));
    data.insert(
        "component_action".to_string(),
        Value::String(if enabled { "enable" } else { "disable" }.to_string()),
    );
    data.insert("components".to_string(), Value::Array(capability_details));
    crate::audit::interceptor::emit_event(
        state.audit_service.as_ref(),
        crate::audit::interceptor::build_rest_event(
            if enabled {
                crate::audit::AuditAction::CapabilityGrant
            } else {
                crate::audit::AuditAction::CapabilityRevoke
            },
            if result.is_ok() {
                crate::audit::AuditStatus::Success
            } else {
                crate::audit::AuditStatus::Failed
            },
            "POST",
            "/api/mcp/profile/components/manage",
            Some(started_at.elapsed().as_millis() as u64),
            audit_server_id,
            Some(request.profile_id.clone()),
            Some(data),
            result.as_ref().err().map(ToString::to_string),
        ),
    )
    .await;
    result
}

async fn collect_capability_audit_details(
    db: &crate::config::database::Database,
    component_ids: &[String],
) -> Vec<CapabilityAuditDetails> {
    let mut details = Vec::new();

    for component_id in component_ids {
        let row = sqlx::query_as::<_, (String, String, String, String)>(
            r#"
            SELECT cr.kind, cr.server_id, sc.name, cr.origin_key
            FROM capability_refs cr
            JOIN server_config sc ON sc.id = cr.server_id
            WHERE cr.ref_id = ?
            "#,
        )
        .bind(component_id)
        .fetch_optional(&db.pool)
        .await
        .ok()
        .flatten();
        let Some((kind, server_id, server_name, origin_key)) = row else {
            details.push(serde_json::json!({
                "ref_id": component_id,
                "component_type": "unknown",
            }));
            continue;
        };
        let Ok(component_type) = ComponentType::from_kind(&kind) else {
            details.push(serde_json::json!({
                "ref_id": component_id,
                "component_type": "unknown",
            }));
            continue;
        };
        details.push(serde_json::json!({
            "ref_id": component_id,
            "component_type": component_type.as_str(),
            "server_id": server_id,
            "server_name": server_name,
            "origin_key": origin_key,
        }));
    }

    details
}

fn extract_single_component_string(
    details: &[CapabilityAuditDetails],
    key: &str,
) -> Option<String> {
    if details.len() != 1 {
        return None;
    }

    details
        .first()
        .and_then(|detail| detail.as_object())
        .and_then(|detail| detail.get(key))
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
}

/// Validate component IDs from request
fn validate_component_ids(request: &ProfileComponentManageReq) -> Result<(), ApiError> {
    if request.component_ids.is_empty() {
        Err(ApiError::BadRequest("component_ids cannot be empty".to_string()))
    } else {
        Ok(())
    }
}

/// Execute unified operations (single or batch) with transaction support
async fn execute_unified_operations(
    state: &Arc<AppState>,
    request: &ProfileComponentManageReq,
) -> Result<Json<ProfileServerManageResp>, ApiError> {
    let db = get_database(state).await?;
    let action = match request.action {
        ProfileComponentAction::Enable => crate::core::capability::management::ProfileRelationshipAction::Enable,
        ProfileComponentAction::Disable => crate::core::capability::management::ProfileRelationshipAction::Disable,
        ProfileComponentAction::Remove => crate::core::capability::management::ProfileRelationshipAction::Remove,
        ProfileComponentAction::Replace => {
            return Err(ApiError::BadRequest(
                "Replace is only supported for profile server selections".to_string(),
            ));
        }
    };
    let management = crate::core::capability::management::ProfileSurfaceManagement::mutate_capabilities(
        &db.pool,
        &request.profile_id,
        &request.component_ids,
        action,
        request.expected_authoring_generation,
        request.source_revision_set.clone().into_iter().collect(),
        "profile_management",
    )
    .await;
    let management = match management {
        Ok(management) => management,
        Err(error) => {
            return Err(super::map_profile_management_error(
                &db.pool,
                &request.profile_id,
                request.source_revision_set.keys().cloned().collect(),
                error,
            )
            .await);
        }
    };
    invalidate_profile_cache(state).await;
    super::emit_surface_publication_audits(
        state,
        "profile_management",
        Some(&request.profile_id),
        "/api/mcp/profile/capabilities/manage",
        management.materializations,
    )
    .await;

    let result_label = match request.action {
        ProfileComponentAction::Enable => "enabled",
        ProfileComponentAction::Disable => "disabled",
        ProfileComponentAction::Remove => "removed",
        ProfileComponentAction::Replace => unreachable!(),
    };
    let results = request
        .component_ids
        .iter()
        .zip(management.mutations)
        .map(|(component_id, mutation)| ComponentOperationResult {
            component_id: component_id.clone(),
            component_type: ComponentType::from_kind(mutation.kind.as_str())
                .expect("validated capability kind")
                .as_str()
                .to_string(),
            success: true,
            result: result_label.to_string(),
            error: None,
        })
        .collect();
    let response = ProfileServerManageData {
        profile_id: request.profile_id.clone(),
        results,
        summary: format!("{} succeeded, 0 failed", request.component_ids.len()),
        status: "completed".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    };

    Ok(Json(ProfileServerManageResp::success(response)))
}

/// Invalidate profile cache if merge service is available
async fn invalidate_profile_cache(state: &Arc<AppState>) {
    if let Some(merge_service) = &state.profile_merge_service {
        merge_service.invalidate_cache().await;
        tracing::debug!("Invalidated profile service cache to sync capability changes");
    }
}

// Small helpers to reduce duplication
fn allowed_ops(enabled: bool) -> Vec<String> {
    vec![if enabled { "disable" } else { "enable" }.to_string()]
}

fn profile_tool_data(tool: crate::config::models::ProfileToolWithDetails) -> ProfileToolData {
    ProfileToolData {
        ref_id: tool.ref_id,
        server_id: tool.server_id,
        server_name: tool.server_name,
        tool_name: tool.tool_name,
        unique_name: tool.unique_name,
        description: tool.description,
        enabled: tool.enabled,
        state: tool.state,
        state_generation: tool.state_generation,
        allowed_operations: vec!["enable".to_string(), "disable".to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::database::Database;
    use mcpmate_capability_store::{
        CapabilityCatalog, CapabilityKind, CapabilityObservation, CapabilityPayload, CatalogRecord, DeclarationState,
        InventoryState, KindObservation, SqliteCapabilityCatalog,
    };
    use rmcp::model::{InitializeResult, Tool};
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::{path::PathBuf, sync::Arc};

    #[tokio::test]
    async fn profile_tools_list_keeps_retired_refs_without_a_live_server() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::initialization::run_initialization(&pool)
            .await
            .expect("initialize database");
        sqlx::query(
            "INSERT INTO server_config (id, name, server_type, command, enabled) VALUES ('server-retired', 'Retired Server', 'stdio', '', 1)",
        )
        .execute(&pool)
        .await
        .expect("insert server");
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role) VALUES ('profile-a', 'Profile A', '', 'shared', 'user')",
        )
        .execute(&pool)
        .await
        .expect("insert profile");

        let initialize: InitializeResult = serde_json::from_value(json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "Retired Server", "version": "1.0.0"}
        }))
        .expect("initialize payload");
        let tool: Tool = serde_json::from_value(json!({
            "name": "analyze",
            "description": "Analyze input",
            "inputSchema": {"type": "object"}
        }))
        .expect("tool payload");
        let record = CatalogRecord::materialize(
            "server-retired",
            "analyze",
            "retired__analyze",
            CapabilityPayload::Tool(tool),
        )
        .expect("materialize tool");
        let catalog = SqliteCapabilityCatalog::new(pool.clone());
        catalog
            .commit_observation(CapabilityObservation::new(
                "server-retired",
                "Retired Server",
                "config-v1",
                initialize,
                vec![KindObservation::new(
                    CapabilityKind::Tools,
                    DeclarationState::Supported,
                    InventoryState::Complete,
                )],
                vec![record.clone()],
            ))
            .await
            .expect("commit catalog");
        crate::config::profile::capability_ref::upsert_profile_capability_ref(&pool, "profile-a", &record.ref_id, true)
            .await
            .expect("insert profile intent");

        let mut transaction = pool.begin().await.expect("retirement transaction");
        catalog
            .retire_server_in_transaction(&mut transaction, "server-retired")
            .await
            .expect("retire server");
        transaction.commit().await.expect("commit retirement");
        sqlx::query("DELETE FROM server_config WHERE id = 'server-retired'")
            .execute(&pool)
            .await
            .expect("delete live server");

        let database = Database {
            pool,
            path: PathBuf::from(":memory:"),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        };
        let response = load_profile_tools_list_data(
            &database,
            ProfileComponentListReq {
                profile_id: "profile-a".to_string(),
                enabled_only: None,
            },
        )
        .await
        .expect("list profile tools");

        assert_eq!(response.tools.len(), 1);
        assert_eq!(response.tools[0].server_name, "Retired Server");
        assert_eq!(response.tools[0].state, "retired");
    }

    #[tokio::test]
    async fn profile_component_and_revision_projection_share_one_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(directory.path().join("projection.db"))
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(3)
            .connect_with(options)
            .await
            .unwrap();
        crate::helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO server_config (id, name, server_type, command, enabled) VALUES ('server-a', 'Server A', 'stdio', '', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role) VALUES ('profile-a', 'Profile A', '', 'shared', 'user')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let catalog = SqliteCapabilityCatalog::new(pool.clone());
        let observation = |description: &str| {
            let tool: Tool = serde_json::from_value(json!({
                "name": "analyze",
                "description": description,
                "inputSchema": {"type": "object"}
            }))
            .unwrap();
            let record = CatalogRecord::materialize(
                "server-a",
                "analyze",
                "server_a__analyze",
                CapabilityPayload::Tool(tool),
            )
            .unwrap();
            let initialize: InitializeResult = serde_json::from_value(json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "Server A", "version": "1.0.0"}
            }))
            .unwrap();
            (
                record.clone(),
                CapabilityObservation::new(
                    "server-a",
                    "Server A",
                    "config-v1",
                    initialize,
                    vec![KindObservation::new(
                        CapabilityKind::Tools,
                        DeclarationState::Supported,
                        InventoryState::Complete,
                    )],
                    vec![record],
                ),
            )
        };
        let (record, first) = observation("before");
        catalog.commit_observation(first).await.unwrap();
        crate::config::profile::capability_ref::upsert_profile_capability_ref(&pool, "profile-a", &record.ref_id, true)
            .await
            .unwrap();

        let mut snapshot = pool.begin().await.unwrap();
        let generation: i64 = sqlx::query_scalar("SELECT authoring_generation FROM profile WHERE id = 'profile-a'")
            .fetch_one(&mut *snapshot)
            .await
            .unwrap();
        assert_eq!(generation, 0);
        let (_, second) = observation("after");
        catalog.commit_observation(second).await.unwrap();
        sqlx::query("UPDATE profile SET authoring_generation = 1 WHERE id = 'profile-a'")
            .execute(&pool)
            .await
            .unwrap();

        let tools = crate::config::profile::tool::get_profile_tools_in_transaction(&mut snapshot, "profile-a")
            .await
            .unwrap();
        let revisions = load_related_revision_set(&mut snapshot, BTreeSet::from(["server-a".to_string()]))
            .await
            .unwrap();
        assert_eq!(tools[0].description.as_deref(), Some("before"));
        assert_eq!(revisions["server-a"], 1);
    }
}
