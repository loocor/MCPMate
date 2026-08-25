// Common utilities for server API handlers
// Provides shared functions for server identification, validation, and response formatting

use sqlx::{Pool, Sqlite};
use std::{collections::HashMap, convert::TryInto, sync::Arc};

use crate::{
    api::{
        handlers::ApiError,
        models::server::{
            CapabilityKindSummary, InstanceSummary, ServerCapabilitySummary, ServerIcon, ServerMetaInfo,
            ServerNamespaceConflict, ServerNamespaceIssue, StandardServerInfo,
        },
        routes::AppState,
    },
    config::{database::Database, server},
};
use axum::http::StatusCode;

#[derive(Debug, Default)]
pub(crate) struct ServerOAuthResponseSummary {
    pub oauth_status: Option<crate::core::oauth::OAuthConnectionState>,
    pub oauth_custody_state: Option<crate::core::oauth::OAuthCustodyState>,
    pub oauth_requires_reconnect: Option<bool>,
    pub oauth_issue: Option<crate::core::oauth::OAuthStatusIssue>,
}

pub(crate) fn parse_server_icons(raw: &str) -> Result<Option<Vec<ServerIcon>>, serde_json::Error> {
    if let Ok(list) = serde_json::from_str::<Vec<ServerIcon>>(raw) {
        return Ok((!list.is_empty()).then_some(list));
    }

    let legacy = serde_json::from_str::<Vec<rmcp::model::Icon>>(raw)?;
    Ok((!legacy.is_empty()).then_some(legacy.into_iter().map(ServerIcon::from).collect()))
}

/// Connection pool access manager
///
/// Provides standardized access to the connection pool with timeout handling,
/// following the project's Manager pattern and error handling conventions.
pub struct ConnectionPoolManager;

impl ConnectionPoolManager {
    /// Get connection pool with timeout and proper error handling
    ///
    /// This method provides a standardized way to access the connection pool with:
    /// - Configurable timeout based on operation type
    /// - Consistent error messages using the unified error handling system
    /// - Proper logging and monitoring
    /// - Integration with the project's error handling patterns
    pub async fn get_pool_with_timeout<'a>(
        state: &'a Arc<AppState>,
        timeout_secs: u64,
        operation_context: &str,
    ) -> Result<tokio::sync::MutexGuard<'a, crate::core::pool::UpstreamConnectionPool>, ApiError> {
        match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            state.connection_pool.lock(),
        )
        .await
        {
            Ok(pool) => {
                tracing::debug!("Successfully acquired connection pool lock for {}", operation_context);
                Ok(pool)
            }
            Err(_) => {
                tracing::warn!(
                    "Connection pool lock timeout for {} ({}s timeout)",
                    operation_context,
                    timeout_secs
                );
                Err(crate::api::handlers::common::errors::internal_error(&format!(
                    "Connection pool access timeout for {} ({}s)",
                    operation_context, timeout_secs
                )))
            }
        }
    }

    /// Get connection pool for API operations (5s timeout)
    pub async fn get_pool_for_api<'a>(
        state: &'a Arc<AppState>
    ) -> Result<tokio::sync::MutexGuard<'a, crate::core::pool::UpstreamConnectionPool>, ApiError> {
        Self::get_pool_with_timeout(state, 5, "API operation").await
    }

    /// Get connection pool for health checks (1s timeout)
    pub async fn get_pool_for_health_check<'a>(
        state: &'a Arc<AppState>
    ) -> Result<tokio::sync::MutexGuard<'a, crate::core::pool::UpstreamConnectionPool>, ApiError> {
        Self::get_pool_with_timeout(state, 1, "health check").await
    }

    /// Get connection pool for capability operations (10s timeout)
    pub async fn get_pool_for_capability<'a>(
        state: &'a Arc<AppState>
    ) -> Result<tokio::sync::MutexGuard<'a, crate::core::pool::UpstreamConnectionPool>, ApiError> {
        Self::get_pool_with_timeout(state, 10, "capability operation").await
    }

    // Removed heavy snapshot getter; prefer pool.get_server_status_summary() or get_snapshot() in read paths
}

/// Server identification result
#[derive(Debug, Clone)]
pub struct ServerIdentification {
    /// Server ID (guaranteed to exist)
    pub server_id: String,
    /// Server name (guaranteed to exist)
    pub server_name: String,
}

/// Helper function to create ServerIdentification from a server record
#[inline]
fn create_server_identification(server: crate::config::models::Server) -> Result<ServerIdentification, ApiError> {
    let server_id = server
        .id
        .ok_or_else(|| ApiError::InternalError("Server found but missing ID".to_string()))?;

    Ok(ServerIdentification {
        server_id,
        server_name: server.name,
    })
}

/// Complete server details for response building
#[derive(Debug, Default)]
pub struct ServerDetails {
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub meta: Option<ServerMetaInfo>,
    pub server_info: Option<StandardServerInfo>,
    pub globally_enabled: bool,
    pub enabled_in_profile: bool,
    pub instances: Vec<InstanceSummary>,
    pub capability: Option<ServerCapabilitySummary>,
    pub protocol_version: Option<String>,
}

pub(super) fn standard_server_info(server_meta: &crate::config::models::ServerMeta) -> Option<StandardServerInfo> {
    if server_meta.upstream_name.is_none()
        && server_meta.upstream_title.is_none()
        && server_meta.server_version.is_none()
    {
        return None;
    }

    Some(StandardServerInfo {
        name: server_meta.upstream_name.clone(),
        title: server_meta.upstream_title.clone(),
        version: server_meta.server_version.clone(),
    })
}

pub(super) async fn load_namespace_issue(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> anyhow::Result<Option<ServerNamespaceIssue>> {
    let issue = crate::config::server::namespace_repair::inspect_namespace_issue(pool, server_id).await?;
    Ok(issue.map(|issue| {
        let code = match issue.kind {
            crate::config::server::namespace_repair::NamespaceIssueKind::CapabilityCollision => "capability_collision",
            crate::config::server::namespace_repair::NamespaceIssueKind::InvalidNamespace
                if !issue.conflicts.is_empty() =>
            {
                "namespace_conflict"
            }
            crate::config::server::namespace_repair::NamespaceIssueKind::InvalidNamespace
                if issue.suggested_namespace.is_none() =>
            {
                "manual_remediation_required"
            }
            crate::config::server::namespace_repair::NamespaceIssueKind::InvalidNamespace => "pending_repair",
        };
        ServerNamespaceIssue {
            code: code.to_string(),
            current_namespace: issue.current_namespace,
            remediation_allowed: true,
            suggested_namespace: issue.suggested_namespace,
            conflicts: issue
                .conflicts
                .into_iter()
                .map(|conflict| ServerNamespaceConflict {
                    server_id: conflict.server_id,
                    namespace: conflict.namespace,
                })
                .collect(),
        }
    }))
}

/// Resolve server identifier (name or ID) to complete server information
///
/// This function provides intelligent resolution of server identifiers:
/// - Accepts both server_name and server_id as input
/// - Returns complete server identification information
/// - Handles edge cases and provides clear error messages
pub async fn resolve_server_identifier(
    pool: &Pool<Sqlite>,
    identifier: &str,
) -> Result<ServerIdentification, ApiError> {
    // Validate input
    if identifier.trim().is_empty() {
        return Err(ApiError::BadRequest("Server identifier cannot be empty".to_string()));
    }

    // Try to find server by ID first (more efficient for ID-based lookups)
    if let Some(server) = server::get_server_by_id(pool, identifier)
        .await
        .map_err(|e| ApiError::InternalError(format!("Database error: {e}")))?
    {
        return create_server_identification(server);
    }

    // Try to find server by name
    if let Some(server) = server::get_server(pool, identifier)
        .await
        .map_err(|e| ApiError::InternalError(format!("Database error: {e}")))?
    {
        return create_server_identification(server);
    }

    // Server not found
    Err(ApiError::NotFound(format!(
        "Server '{}' not found. Please check the server name or ID.",
        identifier
    )))
}

/// Get complete server details including args, env, meta, and status information
///
/// This function consolidates all server detail retrieval logic that was
/// previously duplicated across multiple handler functions.
pub async fn get_complete_server_details(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    state: &Arc<AppState>,
) -> ServerDetails {
    let mut details = ServerDetails::default();

    // Get server arguments
    if !server_id.is_empty() {
        match server::get_server_args(pool, server_id).await {
            Ok(server_args) if !server_args.is_empty() => {
                let mut sorted_args: Vec<_> = server_args.into_iter().collect();
                sorted_args.sort_by_key(|arg| arg.arg_index);
                details.args = Some(sorted_args.into_iter().map(|arg| arg.arg_value).collect());
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("Failed to get arguments for server '{}': {}", server_name, e),
        }
    }

    // Get server environment variables
    if !server_id.is_empty() {
        match server::get_server_env(pool, server_id).await {
            Ok(env_map) if !env_map.is_empty() => details.env = Some(env_map),
            Ok(_) => {}
            Err(e) => tracing::warn!(
                "Failed to get environment variables for server '{}': {}",
                server_name,
                e
            ),
        }
    }

    // Get server metadata
    if !server_id.is_empty() {
        match server::get_server_meta(pool, server_id).await {
            Ok(Some(server_meta)) => {
                details.server_info = standard_server_info(&server_meta);
                let icons = match server_meta.icons_json.as_deref() {
                    Some(raw) => match parse_server_icons(raw) {
                        Ok(list) => list,
                        Err(err) => {
                            tracing::warn!("Failed to parse icons for server '{}': {}", server_name, err);
                            None
                        }
                    },
                    None => None,
                };
                let repository = server_meta
                    .repository
                    .as_deref()
                    .and_then(|raw| match serde_json::from_str(raw) {
                        Ok(repo) => Some(repo),
                        Err(err) => {
                            tracing::warn!(
                                "Failed to parse repository metadata for server '{}': {}",
                                server_name,
                                err
                            );
                            None
                        }
                    });
                let registry_meta =
                    server_meta
                        .registry_meta_json
                        .as_deref()
                        .and_then(|raw| match serde_json::from_str(raw) {
                            Ok(meta) => Some(meta),
                            Err(err) => {
                                tracing::warn!(
                                    "Failed to parse registry meta block for server '{}': {}",
                                    server_name,
                                    err
                                );
                                None
                            }
                        });
                let mut extras: Option<serde_json::Value> =
                    server_meta
                        .extras_json
                        .as_deref()
                        .and_then(|raw| match serde_json::from_str(raw) {
                            Ok(val) => Some(val),
                            Err(err) => {
                                tracing::warn!("Failed to parse extras metadata for server '{}': {}", server_name, err);
                                None
                            }
                        });

                // Fold legacy fields into extras. Keeps backward compatibility for pre-registry metadata.
                if extras.is_none()
                    && (server_meta.author.is_some()
                        || server_meta.category.is_some()
                        || server_meta.recommended_scenario.is_some()
                        || server_meta.rating.is_some())
                {
                    let mut legacy = serde_json::Map::new();
                    if let Some(author) = server_meta.author.clone() {
                        legacy.insert("author".to_string(), serde_json::Value::String(author));
                    }
                    if let Some(category) = server_meta.category.clone() {
                        legacy.insert("category".to_string(), serde_json::Value::String(category));
                    }
                    if let Some(scene) = server_meta.recommended_scenario.clone() {
                        legacy.insert("recommended_scenario".to_string(), serde_json::Value::String(scene));
                    }
                    if let Some(rating) = server_meta.rating {
                        legacy.insert("rating".to_string(), serde_json::Value::Number(rating.into()));
                    }
                    if !legacy.is_empty() {
                        let mut wrapper = serde_json::Map::new();
                        wrapper.insert("legacy".to_string(), serde_json::Value::Object(legacy));
                        extras = Some(serde_json::Value::Object(wrapper));
                    }
                }

                details.meta = Some(ServerMetaInfo {
                    description: server_meta.description,
                    version: server_meta.registry_version,
                    website_url: server_meta.website,
                    repository,
                    meta: registry_meta,
                    extras,
                    icons,
                });
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("Failed to get metadata for server '{}': {}", server_name, e),
        }
    }

    // Get server global enabled status
    details.globally_enabled = server::get_server_global_status(pool, server_id)
        .await
        .ok()
        .flatten()
        .unwrap_or(true);

    // Get server enabled status in profile
    details.enabled_in_profile = server::is_server_enabled_in_any_profile(pool, server_id)
        .await
        .unwrap_or(false);

    // Get instance information from connection pool
    details.instances = get_server_instances(state, server_id).await;

    let live_protocol_version = get_live_server_protocol_version(state, server_id).await;
    let management = load_server_capability_management(state, server_id).await;
    details.capability = management.as_ref().map(|projection| projection.summary.clone());
    details.protocol_version = resolve_management_protocol_version(
        live_protocol_version,
        management.and_then(|projection| projection.cached_protocol_version),
    );

    details
}

pub(crate) async fn load_server_oauth_response_summary(
    pool: &Pool<Sqlite>,
    state: &Arc<AppState>,
    server_id: &str,
    is_http_transport: bool,
) -> Result<ServerOAuthResponseSummary, ApiError> {
    if !is_http_transport {
        return Ok(ServerOAuthResponseSummary::default());
    }

    let manager =
        crate::core::oauth::OAuthManager::new_optional_store(pool.clone(), state.secret_store.read().await.clone());
    let status = manager.get_status(server_id).await.map_err(|err| {
        ApiError::InternalError(format!("Failed to load OAuth status for server '{server_id}': {err}"))
    })?;

    if !status.configured {
        return Ok(ServerOAuthResponseSummary::default());
    }

    Ok(ServerOAuthResponseSummary {
        oauth_status: Some(status.state),
        oauth_custody_state: Some(status.custody_state),
        oauth_requires_reconnect: Some(status.requires_reconnect),
        oauth_issue: status.issue,
    })
}

/// Get server instances with timeout protection
///
/// Consolidates the connection pool access and instance retrieval logic
/// that was duplicated across multiple handlers.
pub async fn get_server_instances(
    state: &Arc<AppState>,
    server_id: &str,
) -> Vec<InstanceSummary> {
    let pool = match ConnectionPoolManager::get_pool_for_api(state).await {
        Ok(pool) => pool,
        Err(_) => {
            tracing::warn!("Failed to get connection pool for server '{}'", server_id);
            return Vec::new();
        }
    };

    let instances = match pool.connections.get(server_id) {
        Some(instances) => instances,
        None => return Vec::new(),
    };

    let now = std::time::SystemTime::now();
    instances
        .iter()
        .map(|(id, conn)| {
            let connected_at = if conn.is_connected() {
                Some(chrono::DateTime::<chrono::Utc>::from(now - conn.time_since_last_connection()).to_rfc3339())
            } else {
                None
            };

            let started_at = chrono::DateTime::<chrono::Utc>::from(now - conn.time_since_creation()).to_rfc3339();

            InstanceSummary {
                id: id.clone(),
                status: conn.status_string(),
                started_at: Some(started_at),
                connected_at,
            }
        })
        .collect()
}

pub(crate) struct CapabilityManagementProjection {
    pub(crate) summary: ServerCapabilitySummary,
    pub(crate) cached_protocol_version: Option<String>,
}

pub(crate) fn resolve_management_protocol_version(
    live_protocol_version: Option<String>,
    cached_protocol_version: Option<String>,
) -> Option<String> {
    live_protocol_version.or(cached_protocol_version)
}

async fn load_server_capability_management(
    state: &Arc<AppState>,
    server_id: &str,
) -> Option<CapabilityManagementProjection> {
    if server_id.is_empty() {
        return None;
    }

    let database = match get_database_from_state(state) {
        Ok(database) => database,
        Err(error) => {
            tracing::warn!(server_id = %server_id, error = %error, "Capability catalog is unavailable");
            return None;
        }
    };
    match database.load_capability_snapshot(server_id).await {
        Ok((Some(snapshot), _)) => {
            build_capability_management_projection_if_current(&database.pool, server_id, &snapshot).await
        }
        Ok((None, _)) => None,
        Err(error) => {
            tracing::warn!(server_id = %server_id, error = %error, "Failed to load capability catalog summary");
            None
        }
    }
}

/// Guards against showing a management summary or cached protocol version derived from a
/// stale catalog snapshot. `/api/mcp/servers/update` can change a server's command, URL,
/// headers, env, or enabled state without immediately invalidating the persisted snapshot; the
/// old snapshot otherwise remains readable until the next capability read invalidates it,
/// which would let the management UI keep showing a `ready` summary and protocol version from
/// the previous configuration (Codex review, PR #160).
pub(crate) async fn build_capability_management_projection_if_current(
    pool: &sqlx::SqlitePool,
    server_id: &str,
    snapshot: &mcpmate_capability_store::CatalogSnapshot,
) -> Option<CapabilityManagementProjection> {
    match crate::config::server::capabilities::current_config_fingerprint(pool, server_id).await {
        Ok(current_fingerprint) if current_fingerprint == snapshot.config_fingerprint => {
            Some(build_capability_management_projection(snapshot))
        }
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(
                server_id = %server_id,
                error = %error,
                "Failed to validate capability catalog fingerprint for management summary"
            );
            None
        }
    }
}

fn build_capability_management_projection(
    snapshot: &mcpmate_capability_store::CatalogSnapshot
) -> CapabilityManagementProjection {
    CapabilityManagementProjection {
        summary: build_server_capability_summary(snapshot),
        cached_protocol_version: snapshot
            .initialize
            .as_ref()
            .map(|initialize| initialize.protocol_version.to_string()),
    }
}

pub(crate) fn build_server_capability_summary(
    snapshot: &mcpmate_capability_store::CatalogSnapshot
) -> ServerCapabilitySummary {
    let kind = |kind| {
        let observation = snapshot.kind_states.iter().find(|state| state.kind == kind);
        let declaration = observation
            .map(|state| state.declaration)
            .unwrap_or(mcpmate_capability_store::DeclarationState::Unknown);
        let inventory = observation
            .map(|state| state.inventory)
            .unwrap_or(mcpmate_capability_store::InventoryState::Unknown);
        let current_count = snapshot
            .records
            .iter()
            .filter(|record| record.kind() == kind)
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        let current_available = snapshot.state == mcpmate_capability_store::SnapshotState::Ready
            && declaration == mcpmate_capability_store::DeclarationState::Supported
            && inventory == mcpmate_capability_store::InventoryState::Complete;
        let last_error = observation.and_then(|state| state.error.clone()).or_else(|| {
            (snapshot.state != mcpmate_capability_store::SnapshotState::Ready)
                .then(|| snapshot.last_error.clone())
                .flatten()
        });

        CapabilityKindSummary {
            declaration,
            inventory,
            current_count,
            current_available,
            last_error,
            failure_kind: observation.and_then(|state| state.failure_kind.clone()),
        }
    };

    ServerCapabilitySummary {
        snapshot_state: snapshot.state,
        revision: snapshot.revision,
        observed_at: snapshot.observed_at.to_rfc3339(),
        tools: kind(mcpmate_capability_store::CapabilityKind::Tools),
        prompts: kind(mcpmate_capability_store::CapabilityKind::Prompts),
        resources: kind(mcpmate_capability_store::CapabilityKind::Resources),
        resource_templates: kind(mcpmate_capability_store::CapabilityKind::ResourceTemplates),
    }
}

async fn get_live_server_protocol_version(
    state: &Arc<AppState>,
    server_id: &str,
) -> Option<String> {
    if server_id.is_empty() {
        return None;
    }

    ConnectionPoolManager::get_pool_for_api(state)
        .await
        .ok()
        .and_then(|pool_guard| {
            pool_guard.connections.get(server_id).and_then(|instances| {
                instances.values().find_map(|conn| {
                    conn.service
                        .as_ref()
                        .and_then(|service| service.peer_info().map(|info| info.protocol_version.to_string()))
                })
            })
        })
}

/// Get server by name or ID with validation
///
/// Consolidates server lookup logic with proper error handling.
pub async fn get_server_by_identifier(
    pool: &Pool<Sqlite>,
    identifier: &str,
) -> Result<(crate::config::models::Server, String), ApiError> {
    let server = server::get_server(pool, identifier)
        .await
        .map_err(|e| ApiError::InternalError(format!("Failed to get server: {e}")))?;

    let server = server.ok_or_else(|| ApiError::NotFound(format!("Server '{identifier}' not found")))?;

    let server_id = server
        .id
        .clone()
        .ok_or_else(|| ApiError::InternalError(format!("Server '{identifier}' has no ID")))?;

    Ok((server, server_id))
}

/// Get server by ID or return error (unified interface)
///
/// Replaces profile/helpers.rs::get_server_or_error with standardized interface.
/// This function provides a clean, consistent way to get a server by ID.
pub async fn get_server_or_error(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<crate::config::models::Server, ApiError> {
    let server = server::get_server_by_id(pool, server_id)
        .await
        .map_err(|e| ApiError::InternalError(format!("Failed to get server: {e}")))?;

    server.ok_or_else(|| ApiError::NotFound(format!("Server with ID '{server_id}' not found")))
}

/// Get server info by ID (unified interface)
///
/// Replaces server/mgmt.rs::get_server_info_by_id with standardized interface.
/// Returns (server_id, server_name) tuple for lightweight operations.
pub async fn get_server_info_by_id(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<(String, String), ApiError> {
    let server = get_server_or_error(pool, server_id).await?;

    let actual_server_id = server
        .id
        .ok_or_else(|| ApiError::InternalError("Server found but missing ID".to_string()))?;

    Ok((actual_server_id, server.name))
}

/// Get server info for inspect endpoints (unified interface)
///
/// Replaces the repeated validation pattern in resources.rs, prompts.rs, and tools.rs.
/// This function consolidates database access, server validation, and info creation.
pub async fn get_server_info_for_inspect(
    app_state: &Arc<AppState>,
    server_id: &str,
    query: &InspectQuery,
) -> Result<
    (
        Arc<crate::config::database::Database>,
        ServerIdentification,
        InspectParams,
    ),
    StatusCode,
> {
    // Get database from app state
    let db = get_database_from_state(app_state).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Get server by ID
    let server_row = crate::config::server::get_server_by_id(&db.pool, server_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Create server identification
    let server_info = ServerIdentification {
        server_id: server_id.to_string(),
        server_name: server_row.name.clone(),
    };

    // Validate server ID format
    validate_server_id(&server_info.server_id).map_err(|_| StatusCode::BAD_REQUEST)?;

    // Parse query parameters
    let params = query.to_params().map_err(|_| StatusCode::BAD_REQUEST)?;

    Ok((db, server_info, params))
}

/// Get database from application state
///
/// Helper function to extract database connection from AppState
/// with proper error handling.
#[inline]
pub fn get_database_from_state(state: &Arc<AppState>) -> Result<Arc<Database>, ApiError> {
    state
        .database
        .clone()
        .or_else(|| state.http_proxy.as_ref().and_then(|p| p.database.clone()))
        .ok_or_else(|| ApiError::InternalError("Database not available".to_string()))
}

/// Get inspect service from application state
///
/// Helper function to extract inspect service from AppState
/// with proper error handling.
// Refresh strategy for backward compatibility
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub enum RefreshStrategy {
    #[default]
    CacheFirst,
    RefreshIfStale,
    Force,
}

impl From<crate::api::models::server::ServerRefreshStrategy> for RefreshStrategy {
    fn from(value: crate::api::models::server::ServerRefreshStrategy) -> Self {
        match value {
            crate::api::models::server::ServerRefreshStrategy::Auto => RefreshStrategy::CacheFirst,
            crate::api::models::server::ServerRefreshStrategy::Force => RefreshStrategy::Force,
            crate::api::models::server::ServerRefreshStrategy::Cache => RefreshStrategy::CacheFirst,
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct InspectParams {
    pub refresh: Option<RefreshStrategy>,
    pub format: Option<String>,
    pub include_meta: Option<bool>,
    pub timeout: Option<u64>,
}

/// Unified query parameters for inspect endpoints
/// Replaces duplicate ToolsQuery, PromptsQuery, and ResourcesQuery structures
#[derive(Debug, serde::Deserialize)]
pub struct InspectQuery {
    /// Refresh strategy for queries
    pub refresh: Option<RefreshStrategy>,
    /// Response format
    pub format: Option<String>,
    /// Whether to include metadata
    pub include_meta: Option<bool>,
    /// Timeout in seconds
    pub timeout: Option<u64>,
}

impl InspectQuery {
    /// Convert to InspectParams
    pub fn to_params(&self) -> Result<InspectParams, crate::api::handlers::ApiError> {
        Ok(InspectParams {
            refresh: Some(self.refresh.unwrap_or_default()),
            format: self.format.clone(),
            include_meta: self.include_meta,
            timeout: self.timeout,
        })
    }
}

/// Validate server ID format
///
/// Ensures server ID follows expected format patterns
#[inline]
pub fn validate_server_id(server_id: &str) -> Result<(), ApiError> {
    if server_id.trim().is_empty() {
        return Err(ApiError::BadRequest("Server ID cannot be empty".to_string()));
    }

    // Allow both generated IDs (serv_*) and custom names
    // This is flexible to support different ID formats
    if server_id.len() > 255 {
        return Err(ApiError::BadRequest(
            "Server ID too long (max 255 characters)".to_string(),
        ));
    }

    Ok(())
}

/// Handle inspect service errors with appropriate HTTP status codes
///
/// Converts inspect service errors to appropriate API errors
#[inline]
pub fn handle_inspect_error(error: String) -> ApiError {
    ApiError::InternalError(error)
}

/// Create a standardized JSON response for inspect endpoints
///
/// This helper function creates a consistent response format for all inspect endpoints.
pub fn create_inspect_response(
    data: Vec<serde_json::Value>,
    cache_hit: bool,
    refresh_strategy: Option<RefreshStrategy>,
    source: &str,
) -> axum::Json<serde_json::Value> {
    // Determine high-level state for client simplicity
    let state = if source.starts_with("capability-") {
        "unsupported"
    } else {
        "ok"
    };
    axum::Json(serde_json::json!({
        "data": data,
        "state": state,
        "meta": {
            "cache_hit": cache_hit,
            "strategy": refresh_strategy.unwrap_or_default(),
            "source": source
        }
    }))
}

/// Enrich capability items with their authoritative external identifiers.
#[inline]
pub async fn enrich_capability_response(
    state: &Arc<AppState>,
    capability_type: super::capability::CapabilityType,
    server_id: &str,
    processed: Vec<serde_json::Value>,
    cache_hit: bool,
    refresh: Option<RefreshStrategy>,
    strategy: &str,
) -> Result<axum::Json<serde_json::Value>, ApiError> {
    let db = get_database_from_state(state)?;
    let enriched = super::capability::enrich_capability_items(capability_type, &db.pool, server_id, processed).await?;
    Ok(super::capability::respond_with_enriched(
        enriched, cache_hit, refresh, strategy,
    ))
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    async fn test_pool() -> Pool<Sqlite> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        let mut server = crate::config::models::Server::new_stdio("docs".to_string(), Some("node".to_string()));
        server.id = Some("server-a".to_string());
        crate::config::server::upsert_server_definition(
            &pool,
            &server,
            &crate::config::models::ServerTransportDraft::Stdio {
                command: Some("node".to_string()),
                args: Vec::new(),
                env: Default::default(),
            },
        )
        .await
        .expect("insert typed server definition");
        pool
    }

    fn snapshot_with_fingerprint(fingerprint: &str) -> mcpmate_capability_store::CatalogSnapshot {
        mcpmate_capability_store::CatalogSnapshot {
            server_id: "server-a".to_string(),
            server_name: "docs".to_string(),
            config_fingerprint: fingerprint.to_string(),
            revision: 1,
            state: mcpmate_capability_store::SnapshotState::Ready,
            initialize: None,
            kind_states: Vec::new(),
            records: Vec::new(),
            observed_at: chrono::Utc::now(),
            committed_at: chrono::Utc::now(),
            last_error: None,
        }
    }

    #[tokio::test]
    async fn management_projection_is_served_when_fingerprint_matches_the_live_config() {
        let pool = test_pool().await;
        let fingerprint = crate::config::server::capabilities::current_config_fingerprint(&pool, "server-a")
            .await
            .expect("compute fingerprint");
        let snapshot = snapshot_with_fingerprint(&fingerprint);

        let projection = build_capability_management_projection_if_current(&pool, "server-a", &snapshot).await;
        assert!(projection.is_some());
    }

    #[tokio::test]
    async fn stale_management_projection_is_suppressed_after_config_changes() {
        // The persisted catalog snapshot survives an `/api/mcp/servers/update` config edit
        // until a capability read invalidates it. Management summaries must not present that
        // stale `ready` snapshot as if it reflected the new configuration (Codex review,
        // PR #160).
        let pool = test_pool().await;
        let stale_fingerprint = crate::config::server::capabilities::current_config_fingerprint(&pool, "server-a")
            .await
            .expect("compute fingerprint before the config change");
        let snapshot = snapshot_with_fingerprint(&stale_fingerprint);

        let mut server = crate::config::models::Server::new_stdio("docs".to_string(), Some("python".to_string()));
        server.id = Some("server-a".to_string());
        crate::config::server::upsert_server_definition(
            &pool,
            &server,
            &crate::config::models::ServerTransportDraft::Stdio {
                command: Some("python".to_string()),
                args: Vec::new(),
                env: Default::default(),
            },
        )
        .await
        .expect("simulate a typed server configuration update");

        let projection = build_capability_management_projection_if_current(&pool, "server-a", &snapshot).await;
        assert!(
            projection.is_none(),
            "a fingerprint mismatch must suppress the stale summary"
        );
    }
}
