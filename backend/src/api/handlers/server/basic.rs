// MCPMate Proxy API handlers for basic MCP server operations
// Contains handler functions for listing and getting servers

use super::{common, shared::*};
use crate::api::models::server::{
    InstanceListData, InstanceListReq, InstanceListResp, InstanceSummary, ServerDetailsData, ServerDetailsReq,
    ServerDetailsResp, ServerListData, ServerListReq, ServerListResp, ServerMetaInfo, ServerTransportValidity,
    StandardServerInfo,
};
use axum::http::StatusCode;
use sqlx::{Pool, Row, Sqlite};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

const SERVER_LIST_RUNTIME_SNAPSHOT_TIMEOUT: Duration = Duration::from_millis(200);

/// Macro to extract database pool from app state with early return on error
macro_rules! get_db_pool {
    ($app_state:expr) => {
        match &$app_state.database {
            Some(db) => db.pool.clone(),
            None => return Err(StatusCode::SERVICE_UNAVAILABLE),
        }
    };
}

/// Whether API should include default HTTP headers in responses (redacted).
/// Defaults to true — redaction-safe round-trip is ensured by frontend
/// resolveRecordUpdatePayload + backend merge_headers_for_update.
fn should_expose_headers() -> bool {
    matches!(
        std::env::var("MCPMATE_API_EXPOSE_HEADERS")
            .unwrap_or_else(|_| "true".to_string())
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "on" | "yes"
    )
}

/// Redact sensitive header values while keeping general visibility
fn redact_headers(input: &std::collections::HashMap<String, String>) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for (k, v) in input.iter() {
        let lower = k.to_ascii_lowercase();
        if [
            "authorization",
            "proxy-authorization",
            "x-api-key",
            "api-key",
            "apikey",
            "cookie",
            "set-cookie",
            "x-auth-token",
            "authentication",
        ]
        .iter()
        .any(|s| *s == lower)
        {
            // Show short masked preview for long tokens, else fully masked
            if v.len() > 12 {
                let (head, tail) = (&v[..6], &v[v.len() - 2..]);
                out.insert(k.clone(), format!("{}***{}", head, tail));
            } else {
                out.insert(k.clone(), "***REDACTED***".to_string());
            }
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

fn is_auth_header_key(key: &str) -> bool {
    matches!(
        key.trim().to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "x-api-key"
            | "api-key"
            | "apikey"
            | "x-auth-token"
            | "authentication"
    )
}

fn detect_auth_mode_from_headers_and_oauth(
    headers: Option<&HashMap<String, String>>,
    oauth_configured: bool,
) -> Option<String> {
    let has_header_auth = headers
        .map(|headers| headers.keys().any(|key| is_auth_header_key(key)))
        .unwrap_or(false);

    if has_header_auth {
        Some("header".to_string())
    } else if oauth_configured {
        Some("oauth".to_string())
    } else {
        None
    }
}

/// Get details for a specific MCP server
///
/// **Endpoint:** `GET /mcp/servers/details?id={server_id}`
pub async fn server_details(
    State(app_state): State<Arc<AppState>>,
    Query(request): Query<ServerDetailsReq>,
) -> Result<Json<ServerDetailsResp>, StatusCode> {
    let db_pool = get_db_pool!(app_state);
    let result = server_details_core(&request, &db_pool, &app_state).await?;
    Ok(Json(result))
}

/// List all MCP servers with optional filtering
///
/// **Endpoint:** `GET /mcp/servers/list?enabled={bool}&server_type={type}&limit={limit}&offset={offset}`
pub async fn server_list(
    State(app_state): State<Arc<AppState>>,
    Query(request): Query<ServerListReq>,
) -> Result<Json<ServerListResp>, StatusCode> {
    let db_pool = get_db_pool!(app_state);
    let result = server_list_core(&request, &db_pool, &app_state).await?;
    Ok(Json(result))
}

/// List instances for servers
///
/// **Endpoint:** `GET /mcp/servers/instances/list?id={server_id}`
pub async fn instance_list(
    State(app_state): State<Arc<AppState>>,
    Query(request): Query<InstanceListReq>,
) -> Result<Json<InstanceListResp>, StatusCode> {
    let db_pool = get_db_pool!(app_state);
    let result = instance_list_core(&request, &db_pool, &app_state).await?;
    Ok(Json(result))
}

// ==================== Core Business Functions ====================

/// Core business logic for server details operation
async fn server_details_core(
    request: &ServerDetailsReq,
    db_pool: &sqlx::SqlitePool,
    state: &Arc<AppState>,
) -> Result<ServerDetailsResp, StatusCode> {
    // Get the server by ID
    let server = crate::config::server::get_server_by_id(db_pool, &request.id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get server: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    let id_opt = server.id.clone();
    let server_id = id_opt.as_deref().unwrap_or_default();
    let name = server.name.clone();

    // Get complete server details using unified function
    let details = common::get_complete_server_details(db_pool, server_id, &name, state).await;
    let enabled = details.globally_enabled;
    let created_at = server.created_at.map(|dt| dt.to_rfc3339());
    let updated_at = server.updated_at.map(|dt| dt.to_rfc3339());

    let mut oauth_status = None;
    let mut oauth_custody_state = None;
    let mut oauth_requires_reconnect = None;
    let mut oauth_issue = None;
    let mut oauth_configured = false;
    if server.server_type.is_http_transport() {
        let manager = crate::core::oauth::manager::OAuthManager::new_optional_store(
            db_pool.clone(),
            state.secret_store.read().await.clone(),
        );
        if let Ok(status) = manager.get_status(server_id).await {
            oauth_configured = status.configured;
            if status.configured {
                oauth_status = Some(status.state);
                oauth_custody_state = Some(status.custody_state);
                oauth_requires_reconnect = Some(status.requires_reconnect);
                oauth_issue = status.issue;
            }
        }
    }
    let raw_headers = if let Some(ref id) = id_opt {
        match crate::config::server::get_server_headers(db_pool, id).await {
            Ok(headers) => Some(headers),
            Err(error) => {
                tracing::warn!(server_id = %id, error = %error, "Failed to load server headers for server details");
                None
            }
        }
    } else {
        None
    };
    let auth_mode = detect_auth_mode_from_headers_and_oauth(raw_headers.as_ref(), oauth_configured);

    // Optionally expose default headers (redacted)
    let headers = if should_expose_headers() {
        raw_headers
            .as_ref()
            .filter(|headers| !headers.is_empty())
            .map(redact_headers)
    } else {
        None
    };
    let namespace_issue = match id_opt.as_deref() {
        Some(server_id) => common::load_namespace_issue(db_pool, server_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
        None => None,
    };
    let transport_validity = load_server_transport_validity(db_pool, &request.id).await;

    let server_details = ServerDetailsData {
        id: id_opt,
        name,
        source: server.source.clone(),
        enabled,
        globally_enabled: details.globally_enabled,
        enabled_in_profile: details.enabled_in_profile,
        unify_direct_exposure_eligible: server.unify_direct_exposure_eligible,
        server_type: server.server_type,
        transport_validity,
        command: server.command.clone(),
        url: server.url.clone(),
        args: details.args,
        env: details.env,
        headers,
        meta: details.meta,
        server_info: details.server_info,
        capability: details.capability.clone(),
        protocol_version: details.protocol_version.clone(),
        created_at,
        updated_at,
        instances: details.instances,
        auth_mode,
        oauth_status,
        oauth_custody_state,
        oauth_requires_reconnect,
        oauth_issue,
        namespace_issue,
        source_revision_set: crate::core::capability::materializer::SurfaceAuthoringLoader::load_catalog_revision_set(
            db_pool,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .into_iter()
        .collect(),
        capability_discovery: None,
    };

    Ok(ServerDetailsResp::success(server_details))
}

/// Core business logic for server list operation
async fn server_list_core(
    request: &ServerListReq,
    db_pool: &sqlx::SqlitePool,
    state: &Arc<AppState>,
) -> Result<ServerListResp, StatusCode> {
    let source_revision_set =
        crate::core::capability::materializer::SurfaceAuthoringLoader::load_catalog_revision_set(db_pool)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .into_iter()
            .collect::<crate::api::models::CatalogRevisionSet>();
    let all_servers = crate::config::server::get_all_servers(db_pool).await.map_err(|e| {
        tracing::error!("Failed to get servers: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let server_ids: Vec<String> = all_servers.iter().filter_map(|server| server.id.clone()).collect();
    let mut transport_validity_map = load_server_transport_validities(db_pool, &server_ids).await;
    let (instance_map, live_protocol_versions) = snapshot_runtime_state(state).await;
    let management_map = load_server_capability_management(state, &server_ids).await;
    let (meta_map, server_info_map) = load_server_meta_maps(db_pool, &server_ids).await;
    let enabled_in_profile = load_enabled_server_ids(db_pool).await;
    let streamable_http_server_ids: Vec<String> = all_servers
        .iter()
        .filter(|server| server.server_type.is_http_transport())
        .filter_map(|server| server.id.clone())
        .collect();
    let secure_store_available = state.secret_store.read().await.is_some();
    let oauth_status_map =
        match crate::core::oauth::load_oauth_states(db_pool, &streamable_http_server_ids, secure_store_available).await
        {
            Ok(map) => map,
            Err(error) => {
                tracing::warn!(error = %error, "Failed to batch load OAuth states for server list");
                HashMap::new()
            }
        };
    let mut raw_headers_map = load_server_header_maps(db_pool, &server_ids).await;
    let headers_map = if should_expose_headers() {
        raw_headers_map
            .iter()
            .filter(|(_, headers)| !headers.is_empty())
            .map(|(server_id, headers)| (server_id.clone(), redact_headers(headers)))
            .collect()
    } else {
        HashMap::new()
    };

    let offset = request.offset.unwrap_or(0) as usize;
    let limit = request.limit.unwrap_or(100) as usize;
    let mut filtered_servers = Vec::new();
    let mut total_count = 0;
    let mut instance_map = instance_map;
    let mut management_map = management_map;
    let mut meta_map = meta_map;
    let mut server_info_map = server_info_map;
    let mut live_protocol_versions = live_protocol_versions;
    let mut headers_map = headers_map;

    for server in all_servers {
        if let Some(ref type_filter) = request.server_type {
            if server.server_type.as_str() != type_filter {
                continue;
            }
        }

        let server_id = server.id.clone().unwrap_or_default();
        let globally_enabled = server.enabled.as_bool();
        if let Some(enabled_filter) = request.enabled {
            if globally_enabled != enabled_filter {
                continue;
            }
        }

        total_count += 1;
        if total_count <= offset || filtered_servers.len() >= limit {
            continue;
        }

        let enabled = globally_enabled;
        let created_at = server.created_at.map(|dt| dt.to_rfc3339());
        let updated_at = server.updated_at.map(|dt| dt.to_rfc3339());
        let enabled_in_profile_flag = globally_enabled && enabled_in_profile.contains(&server_id);
        let instances = instance_map.remove(&server_id).unwrap_or_default();
        let management = management_map.remove(&server_id);
        let capability = management.as_ref().map(|projection| projection.summary.clone());
        let meta = meta_map.remove(&server_id).flatten();
        let server_info = server_info_map.remove(&server_id).flatten();
        let protocol_version = common::resolve_management_protocol_version(
            live_protocol_versions.remove(&server_id).flatten(),
            management.and_then(|projection| projection.cached_protocol_version),
        );
        let raw_headers = raw_headers_map.remove(&server_id);
        let headers = headers_map.remove(&server_id);

        let oauth_summary = if server.server_type.is_http_transport() {
            oauth_status_map.get(&server_id).cloned()
        } else {
            None
        };
        let oauth_configured = oauth_summary.is_some();
        let auth_mode = detect_auth_mode_from_headers_and_oauth(raw_headers.as_ref(), oauth_configured);
        let (oauth_status, oauth_custody_state, oauth_requires_reconnect, oauth_issue) = oauth_summary
            .map(|summary| {
                (
                    Some(summary.state),
                    Some(summary.custody_state),
                    Some(summary.requires_reconnect),
                    summary.issue,
                )
            })
            .unwrap_or_default();
        let namespace_issue = common::load_namespace_issue(db_pool, &server_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        filtered_servers.push(ServerDetailsData {
            id: server.id.clone(),
            name: server.name,
            source: server.source,
            enabled,
            globally_enabled,
            enabled_in_profile: enabled_in_profile_flag,
            unify_direct_exposure_eligible: server.unify_direct_exposure_eligible,
            server_type: server.server_type,
            transport_validity: transport_validity_map
                .remove(&server_id)
                .unwrap_or_else(ServerTransportValidity::missing),
            command: server.command,
            url: server.url,
            args: None,
            env: None,
            headers,
            meta,
            server_info,
            capability,
            protocol_version,
            created_at,
            updated_at,
            instances,
            auth_mode,
            oauth_status,
            oauth_custody_state,
            oauth_requires_reconnect,
            oauth_issue,
            namespace_issue,
            source_revision_set: source_revision_set.clone(),
            capability_discovery: None,
        });
    }

    Ok(ServerListResp::success(ServerListData {
        servers: filtered_servers,
    }))
}

async fn load_server_transport_validities(
    pool: &Pool<Sqlite>,
    server_ids: &[String],
) -> HashMap<String, ServerTransportValidity> {
    match crate::config::server::get_server_transport_drafts(pool, server_ids).await {
        Ok(mut drafts) => server_ids
            .iter()
            .map(|server_id| {
                let validity = match drafts.remove(server_id) {
                    Some(crate::config::server::ServerTransportDraftLoad::Draft(draft)) => {
                        ServerTransportValidity::from_draft(draft)
                    }
                    Some(crate::config::server::ServerTransportDraftLoad::DecodeFailed) => {
                        ServerTransportValidity::draft_decode_failed()
                    }
                    None => ServerTransportValidity::missing(),
                };
                (server_id.clone(), validity)
            })
            .collect(),
        Err(error) => {
            tracing::warn!(error = %error, "Failed to load server transport drafts");
            server_ids
                .iter()
                .map(|server_id| (server_id.clone(), ServerTransportValidity::draft_load_failed()))
                .collect()
        }
    }
}

pub(crate) async fn load_server_transport_validity(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> ServerTransportValidity {
    load_server_transport_validities(pool, &[server_id.to_string()])
        .await
        .remove(server_id)
        .unwrap_or_else(ServerTransportValidity::missing)
}

async fn snapshot_runtime_state(
    state: &Arc<AppState>
) -> (HashMap<String, Vec<InstanceSummary>>, HashMap<String, Option<String>>) {
    let pool = match state.connection_pool.try_lock() {
        Ok(pool) => pool,
        Err(_) => {
            match tokio::time::timeout(SERVER_LIST_RUNTIME_SNAPSHOT_TIMEOUT, state.connection_pool.lock()).await {
                Ok(pool) => pool,
                Err(_) => {
                    tracing::warn!(
                        timeout_ms = SERVER_LIST_RUNTIME_SNAPSHOT_TIMEOUT.as_millis(),
                        "Connection pool busy; skipping runtime snapshot for server list"
                    );
                    return (HashMap::new(), HashMap::new());
                }
            }
        }
    };

    let now = std::time::SystemTime::now();
    let connection_snapshot = pool.get_connection_snapshot();
    let protocol_snapshot = pool.get_snapshot();

    let instance_map = connection_snapshot
        .into_iter()
        .map(|(server_id, instances)| {
            let summaries = instances
                .into_iter()
                .map(|(id, conn)| build_instance_summary_from_snapshot(id, conn, now))
                .collect();
            (server_id, summaries)
        })
        .collect();

    let protocol_versions = protocol_snapshot
        .into_iter()
        .map(|(server_id, instances)| {
            let version = instances.into_iter().find_map(|(_, _, _, _, peer)| {
                peer.and_then(|peer| peer.peer_info().map(|info| info.protocol_version.to_string()))
            });
            (server_id, version)
        })
        .collect();

    (instance_map, protocol_versions)
}

fn build_instance_summary_from_snapshot(
    id: String,
    conn: crate::core::pool::UpstreamConnection,
    now: std::time::SystemTime,
) -> InstanceSummary {
    let started_at = chrono::DateTime::<chrono::Utc>::from(now - conn.time_since_creation()).to_rfc3339();
    let connected_at = conn
        .is_connected()
        .then(|| chrono::DateTime::<chrono::Utc>::from(now - conn.time_since_last_connection()).to_rfc3339());

    InstanceSummary {
        id,
        status: conn.status_string(),
        started_at: Some(started_at),
        connected_at,
    }
}

async fn load_enabled_server_ids(pool: &Pool<Sqlite>) -> HashSet<String> {
    let active_profiles = match crate::config::profile::get_active_profile(pool).await {
        Ok(profiles) if !profiles.is_empty() => profiles,
        Ok(_) => match crate::config::profile::get_default_profiles(pool).await {
            Ok(defaults) => defaults.into_iter().filter(|profile| profile.is_active).collect(),
            Err(error) => {
                tracing::warn!(error = %error, "Failed to load default profiles for server list");
                Vec::new()
            }
        },
        Err(error) => {
            tracing::warn!(error = %error, "Failed to load active profiles for server list");
            Vec::new()
        }
    };

    let mut enabled = HashSet::new();
    for profile in active_profiles {
        let Some(profile_id) = profile.id else {
            continue;
        };

        match crate::config::profile::get_profile_servers(pool, &profile_id).await {
            Ok(servers) => {
                for server in servers.into_iter().filter(|server| server.enabled) {
                    enabled.insert(server.server_id);
                }
            }
            Err(error) => {
                tracing::warn!(profile_id = %profile_id, error = %error, "Failed to load profile servers for server list");
            }
        }
    }

    if enabled.is_empty() {
        if let Ok(profile_id) = crate::config::profile::ensure_default_anchor_profile_id(pool).await {
            match crate::config::profile::get_profile_servers(pool, &profile_id).await {
                Ok(servers) => {
                    for server in servers.into_iter().filter(|server| server.enabled) {
                        enabled.insert(server.server_id);
                    }
                }
                Err(error) => {
                    tracing::warn!(profile_id = %profile_id, error = %error, "Failed to load anchor profile servers for server list");
                }
            }
        }
    }

    enabled
}

async fn load_server_capability_management(
    state: &Arc<AppState>,
    server_ids: &[String],
) -> HashMap<String, common::CapabilityManagementProjection> {
    let database = match common::get_database_from_state(state) {
        Ok(database) => database,
        Err(error) => {
            tracing::warn!(error = %error, "Capability catalog is unavailable for server list");
            return HashMap::new();
        }
    };

    let mut projections = HashMap::new();
    for server_id in server_ids {
        match database.load_capability_snapshot(server_id).await {
            Ok((Some(snapshot), _)) => {
                if let Some(projection) =
                    common::build_capability_management_projection_if_current(&database.pool, server_id, &snapshot)
                        .await
                {
                    projections.insert(server_id.clone(), projection);
                }
            }
            Ok((None, _)) => {}
            Err(error) => {
                tracing::warn!(
                    server_id = %server_id,
                    error = %error,
                    "Failed to load capability catalog summary for server list"
                );
            }
        }
    }

    projections
}

async fn load_server_meta_maps(
    pool: &Pool<Sqlite>,
    server_ids: &[String],
) -> (
    HashMap<String, Option<ServerMetaInfo>>,
    HashMap<String, Option<StandardServerInfo>>,
) {
    if server_ids.is_empty() {
        return (HashMap::new(), HashMap::new());
    }

    let rows = match sqlx::query(
        r#"
        SELECT *
        FROM server_meta
        "#,
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(error = %error, "Failed to load server metadata map for server list");
            return (HashMap::new(), HashMap::new());
        }
    };

    let mut meta_map = HashMap::new();
    let mut server_info_map = HashMap::new();
    for row in rows {
        let server_meta: crate::config::models::ServerMeta = match sqlx::FromRow::from_row(&row) {
            Ok(meta) => meta,
            Err(error) => {
                tracing::warn!(error = %error, "Failed to decode server metadata row for server list");
                continue;
            }
        };
        let server_id = server_meta.server_id.clone();
        let server_info = common::standard_server_info(&server_meta);
        server_info_map.insert(server_id.clone(), server_info);
        meta_map.insert(server_id, build_server_meta_info(server_meta));
    }
    (meta_map, server_info_map)
}

fn build_server_meta_info(server_meta: crate::config::models::ServerMeta) -> Option<ServerMetaInfo> {
    let icons = match server_meta.icons_json.as_deref() {
        Some(raw) => match crate::api::handlers::server::common::parse_server_icons(raw) {
            Ok(list) => list,
            Err(error) => {
                tracing::warn!(error = %error, server_id = %server_meta.server_id, "Failed to parse icons for server list");
                None
            }
        },
        None => None,
    };

    let repository = server_meta.repository.as_deref().and_then(|raw| match serde_json::from_str(raw) {
        Ok(repo) => Some(repo),
        Err(error) => {
            tracing::warn!(error = %error, server_id = %server_meta.server_id, "Failed to parse repository metadata for server list");
            None
        }
    });

    let registry_meta = server_meta
        .registry_meta_json
        .as_deref()
        .and_then(|raw| match serde_json::from_str(raw) {
            Ok(meta) => Some(meta),
            Err(error) => {
                tracing::warn!(error = %error, server_id = %server_meta.server_id, "Failed to parse registry meta block for server list");
                None
            }
        });

    let mut extras: Option<serde_json::Value> =
        server_meta
            .extras_json
            .as_deref()
            .and_then(|raw| match serde_json::from_str(raw) {
                Ok(value) => Some(value),
                Err(error) => {
                    tracing::warn!(error = %error, server_id = %server_meta.server_id, "Failed to parse extras metadata for server list");
                    None
                }
            });

    if extras.is_none()
        && (server_meta.author.is_some()
            || server_meta.category.is_some()
            || server_meta.recommended_scenario.is_some()
            || server_meta.rating.is_some())
    {
        let mut legacy = serde_json::Map::new();
        if let Some(author) = server_meta.author {
            legacy.insert("author".to_string(), serde_json::Value::String(author));
        }
        if let Some(category) = server_meta.category {
            legacy.insert("category".to_string(), serde_json::Value::String(category));
        }
        if let Some(scene) = server_meta.recommended_scenario {
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

    Some(ServerMetaInfo {
        description: server_meta.description,
        version: server_meta.registry_version,
        website_url: server_meta.website,
        repository,
        meta: registry_meta,
        extras,
        icons,
    })
}

async fn load_server_header_maps(
    pool: &Pool<Sqlite>,
    server_ids: &[String],
) -> HashMap<String, HashMap<String, String>> {
    if server_ids.is_empty() {
        return HashMap::new();
    }

    let query = format!(
        "SELECT server_id, header_key, header_value FROM server_headers WHERE server_id IN ({}) ORDER BY server_id, header_key",
        server_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ")
    );

    let mut statement = sqlx::query(&query);
    for server_id in server_ids {
        statement = statement.bind(server_id);
    }

    let rows = match statement.fetch_all(pool).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(error = %error, "Failed to batch load server headers for server list");
            return HashMap::new();
        }
    };

    let mut headers_map = HashMap::new();
    for row in rows {
        let server_id: String = row.get("server_id");
        let header_key: String = row.get("header_key");
        let header_value: String = row.get("header_value");
        headers_map
            .entry(server_id)
            .or_insert_with(HashMap::new)
            .insert(header_key, header_value);
    }

    headers_map
}

/// Core business logic for instance list operation
async fn instance_list_core(
    request: &InstanceListReq,
    db_pool: &sqlx::SqlitePool,
    state: &Arc<AppState>,
) -> Result<InstanceListResp, StatusCode> {
    if let Some(ref server_id) = request.id {
        // List instances for specific server
        let server = crate::config::server::get_server_by_id(db_pool, server_id)
            .await
            .map_err(|e| {
                tracing::error!("Failed to get server: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?
            .ok_or(StatusCode::NOT_FOUND)?;

        let name = server.name;
        let instance_summaries = common::get_server_instances(state, server_id).await;

        Ok(InstanceListResp::success(InstanceListData {
            name,
            instances: instance_summaries,
        }))
    } else {
        // List all instances for all servers
        use crate::api::handlers::server::common::ConnectionPoolManager;
        let pool = match ConnectionPoolManager::get_pool_for_health_check(state).await {
            Ok(pool) => pool,
            Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
        };

        let mut all_instances = Vec::new();
        for instances in pool.connections.values() {
            for (instance_id, conn) in instances {
                let now = std::time::SystemTime::now();
                all_instances.push(build_instance_summary_from_live_connection(
                    instance_id.clone(),
                    conn,
                    now,
                ));
            }
        }

        Ok(InstanceListResp::success(InstanceListData {
            name: "all".to_string(),
            instances: all_instances,
        }))
    }
}

fn build_instance_summary_from_live_connection(
    id: String,
    conn: &crate::core::pool::UpstreamConnection,
    now: std::time::SystemTime,
) -> InstanceSummary {
    let started_at = chrono::DateTime::<chrono::Utc>::from(now - conn.created_at.elapsed()).to_rfc3339();
    let connected_at = conn
        .is_connected()
        .then(|| chrono::DateTime::<chrono::Utc>::from(now - conn.last_connected.elapsed()).to_rfc3339());

    InstanceSummary {
        id,
        status: conn.status_string(),
        started_at: Some(started_at),
        connected_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::server::TransportValidityState;
    use crate::{
        common::{profile::ProfileType, server::ServerType},
        config::{
            database::Database,
            models::{HttpTransportKind, Profile, Server, ServerOAuthConfig, ServerOAuthToken, ServerTransportDraft},
            profile, server,
        },
        core::{
            models::Config,
            pool::{UpstreamConnection, UpstreamConnectionPool},
            profile::ConfigApplicationStateManager,
        },
        inspector::{calls::InspectorCallRegistry, sessions::InspectorSessionManager},
        system::metrics::MetricsCollector,
    };
    use chrono::Utc;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::{path::PathBuf, sync::Arc, time::Duration};
    use tempfile::TempDir;
    use tokio::{
        sync::{Mutex, RwLock},
        time::Instant,
    };

    struct TestContext {
        _temp_dir: TempDir,
        app_state: Arc<AppState>,
        db_pool: sqlx::SqlitePool,
    }

    #[test]
    fn live_protocol_version_has_priority_over_cached_management_projection() {
        assert_eq!(
            common::resolve_management_protocol_version(
                Some("live-version".to_string()),
                Some("cached-version".to_string()),
            )
            .as_deref(),
            Some("live-version")
        );
    }

    #[tokio::test]
    async fn server_list_returns_quickly_when_pool_lock_is_held() {
        let context = create_test_context().await;
        seed_servers(&context.db_pool, 24).await;

        let request = ServerListReq {
            enabled: None,
            server_type: None,
            limit: Some(100),
            offset: Some(0),
        };

        let lock_guard = context.app_state.connection_pool.lock().await;
        let started = Instant::now();
        let response = server_list_core(&request, &context.db_pool, &context.app_state)
            .await
            .expect("server list should degrade gracefully");
        let elapsed = started.elapsed();
        drop(lock_guard);

        assert!(response.success);
        let data = response.data.expect("server list data");
        assert_eq!(data.servers.len(), 24);
        assert!(
            elapsed < Duration::from_secs(2),
            "server list should not wait for API lock timeout: {elapsed:?}"
        );
        assert!(data.servers.iter().all(|server| server.instances.is_empty()));
        assert!(data.servers.iter().all(|server| server.protocol_version.is_none()));
        assert!(data.servers.iter().all(|server| server.capability.is_none()));
    }

    #[tokio::test]
    async fn server_read_models_report_invalid_drafts_without_exposing_literal_config_values() {
        let context = create_test_context().await;
        let server_id = seed_servers(&context.db_pool, 1)
            .await
            .into_iter()
            .next()
            .expect("seed server");
        sqlx::query("UPDATE server_transport SET draft_json = ? WHERE server_id = ?")
            .bind(
                serde_json::json!({
                    "kind": "stdio",
                    "command": null,
                    "args": [],
                    "env": {
                        "ACCESS_TOKEN": { "kind": "literal", "value": "literal-secret" },
                        "SECRET_ALIAS": { "kind": "secret_ref", "alias": "upstream-token" }
                    }
                })
                .to_string(),
            )
            .bind(&server_id)
            .execute(&context.db_pool)
            .await
            .expect("store invalid transport draft");

        let detail = server_details_core(
            &ServerDetailsReq { id: server_id.clone() },
            &context.db_pool,
            &context.app_state,
        )
        .await
        .expect("server details")
        .data
        .expect("server details data");
        let listed = server_list_core(
            &ServerListReq {
                enabled: None,
                server_type: None,
                limit: Some(10),
                offset: Some(0),
            },
            &context.db_pool,
            &context.app_state,
        )
        .await
        .expect("server list")
        .data
        .expect("server list data")
        .servers
        .into_iter()
        .find(|server| server.id.as_deref() == Some(server_id.as_str()))
        .expect("listed server");

        for server in [detail, listed] {
            assert_eq!(server.transport_validity.state, TransportValidityState::Invalid);
            assert!(
                server
                    .transport_validity
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.code == "stdio_command_missing" && diagnostic.field == "command")
            );

            let payload = serde_json::to_string(&server).expect("serialize API response");
            assert!(!payload.contains("literal-secret"));
            assert!(payload.contains("***REDACTED***"));
            assert!(payload.contains("upstream-token"));
        }
    }

    #[tokio::test]
    async fn server_read_models_surface_transport_draft_decode_failures() {
        let context = create_test_context().await;
        let server_id = seed_servers(&context.db_pool, 1)
            .await
            .into_iter()
            .next()
            .expect("seed server");
        sqlx::query("UPDATE server_transport SET draft_json = ? WHERE server_id = ?")
            .bind(r#"{"kind":"stdio","command":42,"args":[],"env":{}}"#)
            .bind(&server_id)
            .execute(&context.db_pool)
            .await
            .expect("store undecodable transport draft");

        let detail = server_details_core(
            &ServerDetailsReq { id: server_id.clone() },
            &context.db_pool,
            &context.app_state,
        )
        .await
        .expect("server details")
        .data
        .expect("server details data");
        let listed = server_list_core(
            &ServerListReq {
                enabled: None,
                server_type: None,
                limit: Some(10),
                offset: Some(0),
            },
            &context.db_pool,
            &context.app_state,
        )
        .await
        .expect("server list")
        .data
        .expect("server list data")
        .servers
        .into_iter()
        .find(|server| server.id.as_deref() == Some(server_id.as_str()))
        .expect("listed server");

        for server in [detail, listed] {
            assert_eq!(server.transport_validity.state, TransportValidityState::Invalid);
            assert_eq!(
                server.transport_validity.diagnostics[0].code,
                "transport_draft_decode_failed"
            );
            assert_eq!(server.transport_validity.diagnostics[0].field, "transport");
            assert!(server.transport_validity.draft.is_none());
        }
    }

    #[tokio::test]
    async fn server_list_waits_briefly_for_runtime_snapshot_before_degrading() {
        let context = create_test_context().await;
        let server_id = seed_servers(&context.db_pool, 1)
            .await
            .into_iter()
            .next()
            .expect("server id");
        seed_runtime_instance(&context.app_state, &server_id).await;

        let request = ServerListReq {
            enabled: None,
            server_type: None,
            limit: Some(10),
            offset: Some(0),
        };

        let app_state = context.app_state.clone();
        let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
        let holder = tokio::spawn(async move {
            let _guard = app_state.connection_pool.lock().await;
            let _ = locked_tx.send(());
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        locked_rx.await.expect("lock acquired");

        let started = Instant::now();
        let response = server_list_core(&request, &context.db_pool, &context.app_state)
            .await
            .expect("server list should wait briefly for runtime snapshot");
        let elapsed = started.elapsed();
        holder.await.expect("holder task");

        assert!(response.success);
        let data = response.data.expect("server list data");
        assert_eq!(data.servers.len(), 1);
        assert!(
            elapsed >= Duration::from_millis(40),
            "server list should wait briefly for runtime snapshot: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "server list should not wait for full API lock timeout: {elapsed:?}"
        );
        assert_eq!(data.servers[0].instances.len(), 1);
    }

    #[tokio::test]
    async fn server_list_preserves_list_metadata_without_full_details() {
        let context = create_test_context().await;
        let server_id = seed_servers(&context.db_pool, 1)
            .await
            .into_iter()
            .next()
            .expect("server id");
        seed_server_meta(&context.db_pool, &server_id).await;
        seed_server_capabilities(&context.db_pool, &server_id).await;
        seed_profile_enablement(&context.db_pool, &server_id).await;
        seed_runtime_instance(&context.app_state, &server_id).await;

        let request = ServerListReq {
            enabled: Some(true),
            server_type: Some("stdio".to_string()),
            limit: Some(10),
            offset: Some(0),
        };

        let response = server_list_core(&request, &context.db_pool, &context.app_state)
            .await
            .expect("server list should succeed");
        let data = response.data.expect("server list data");
        let server = data.servers.into_iter().next().expect("server item");

        assert_eq!(server.id.as_deref(), Some(server_id.as_str()));
        assert!(server.enabled);
        assert!(server.globally_enabled);
        assert!(server.enabled_in_profile);
        assert!(!server.unify_direct_exposure_eligible);
        assert_eq!(server.args, None);
        assert_eq!(server.env, None);
        assert_eq!(server.instances.len(), 1);
        assert_eq!(server.instances[0].status, "Ready");
        let capability = server.capability.as_ref().expect("capability snapshot summary");
        assert_eq!(
            capability.snapshot_state,
            mcpmate_capability_store::SnapshotState::Ready
        );
        assert_eq!(capability.tools.current_count, 1);
        assert_eq!(capability.prompts.current_count, 1);
        assert!(capability.tools.current_available);
        assert!(capability.prompts.current_available);
        assert_eq!(
            capability.tools.declaration,
            mcpmate_capability_store::DeclarationState::Supported
        );
        assert_eq!(
            capability.prompts.declaration,
            mcpmate_capability_store::DeclarationState::Supported
        );
        assert_eq!(
            capability.resources.declaration,
            mcpmate_capability_store::DeclarationState::Unsupported
        );
        assert_eq!(
            server.meta.as_ref().and_then(|meta| meta.description.as_deref()),
            Some("Server description")
        );
        assert_eq!(
            server
                .meta
                .as_ref()
                .and_then(|meta| meta.icons.as_ref())
                .map(|icons| icons.len()),
            Some(1)
        );
    }

    #[tokio::test]
    async fn server_list_capability_summary_uses_one_catalog_snapshot_not_shadow_indexes() {
        let context = create_test_context().await;
        let server_id = seed_servers(&context.db_pool, 1)
            .await
            .into_iter()
            .next()
            .expect("server id");
        seed_server_capabilities(&context.db_pool, &server_id).await;

        let database = context.app_state.database.as_ref().expect("database");
        let (snapshot, _) = database
            .load_capability_snapshot(&server_id)
            .await
            .expect("load catalog snapshot");
        let snapshot = snapshot.expect("catalog snapshot");

        sqlx::query("DELETE FROM server_tools WHERE server_id = ?")
            .bind(&server_id)
            .execute(&context.db_pool)
            .await
            .expect("delete shadow tool rows");
        database.capability_cache.clear().await;
        let metrics_before = database.capability_cache.metrics().await;

        let response = server_list_core(
            &ServerListReq {
                enabled: None,
                server_type: None,
                limit: Some(10),
                offset: Some(0),
            },
            &context.db_pool,
            &context.app_state,
        )
        .await
        .expect("server list");
        let server = response
            .data
            .expect("server list data")
            .servers
            .into_iter()
            .next()
            .expect("server item");
        let summary = server.capability.expect("capability summary");
        let metrics_after = database.capability_cache.metrics().await;

        assert_eq!(summary.revision, snapshot.revision);
        assert_eq!(summary.observed_at, snapshot.observed_at.to_rfc3339());
        assert_eq!(server.protocol_version.as_deref(), Some("2025-11-25"));
        assert_eq!(
            (metrics_after.raw_hits + metrics_after.raw_misses) - (metrics_before.raw_hits + metrics_before.raw_misses),
            1,
            "list must derive lifecycle summary and cached protocol from one snapshot load"
        );
        assert_eq!(summary.tools.current_count, 1);
        assert_eq!(
            summary.tools.inventory,
            mcpmate_capability_store::InventoryState::Complete
        );
        assert!(summary.tools.current_available);
    }

    #[tokio::test]
    async fn server_details_derives_capability_and_cached_protocol_from_one_catalog_snapshot() {
        let context = create_test_context().await;
        let server_id = seed_servers(&context.db_pool, 1)
            .await
            .into_iter()
            .next()
            .expect("server id");
        seed_server_capabilities(&context.db_pool, &server_id).await;

        let database = context.app_state.database.as_ref().expect("database");
        let (snapshot, _) = database
            .load_capability_snapshot(&server_id)
            .await
            .expect("load catalog snapshot");
        let snapshot = snapshot.expect("catalog snapshot");
        database.capability_cache.clear().await;
        let metrics_before = database.capability_cache.metrics().await;

        let response = server_details_core(
            &ServerDetailsReq { id: server_id.clone() },
            &context.db_pool,
            &context.app_state,
        )
        .await
        .expect("server details");
        let server = response.data.expect("server details data");
        let summary = server.capability.expect("capability summary");
        let metrics_after = database.capability_cache.metrics().await;

        assert_eq!(summary.revision, snapshot.revision);
        assert_eq!(summary.observed_at, snapshot.observed_at.to_rfc3339());
        assert_eq!(server.protocol_version.as_deref(), Some("2025-11-25"));
        assert_eq!(
            (metrics_after.raw_hits + metrics_after.raw_misses) - (metrics_before.raw_hits + metrics_before.raw_misses),
            1,
            "details must derive lifecycle summary and cached protocol from one snapshot load"
        );
    }

    #[tokio::test]
    async fn server_list_derives_auth_mode_from_headers_and_oauth_state() {
        let context = create_test_context().await;
        let oauth_only_id = seed_streamable_http_server(&context.db_pool, "oauth-only").await;
        let header_only_id = seed_streamable_http_server(&context.db_pool, "header-only").await;
        let both_id = seed_streamable_http_server(&context.db_pool, "both").await;

        crate::config::server::upsert_server_oauth_config(
            &context.db_pool,
            &ServerOAuthConfig {
                id: None,
                server_id: oauth_only_id.clone(),
                authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                token_endpoint: "https://issuer.example.com/token".to_string(),
                client_id: "client-oauth-only".to_string(),
                client_secret: None,
                scopes: Some("read".to_string()),
                redirect_uri: "http://127.0.0.1:5173/oauth/callback".to_string(),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("upsert oauth-only config");
        crate::config::server::upsert_server_oauth_token(
            &context.db_pool,
            &ServerOAuthToken {
                id: None,
                server_id: oauth_only_id.clone(),
                access_token: "token-oauth-only".to_string(),
                refresh_token: None,
                token_type: "bearer".to_string(),
                expires_at: Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
                scope: Some("read".to_string()),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("upsert oauth-only token");

        crate::config::server::upsert_server_headers(
            &context.db_pool,
            &header_only_id,
            &HashMap::from([("Authorization".to_string(), "Bearer header-only".to_string())]),
        )
        .await
        .expect("upsert header-only auth header");

        crate::config::server::upsert_server_oauth_config(
            &context.db_pool,
            &ServerOAuthConfig {
                id: None,
                server_id: both_id.clone(),
                authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                token_endpoint: "https://issuer.example.com/token".to_string(),
                client_id: "client-both".to_string(),
                client_secret: None,
                scopes: Some("read write".to_string()),
                redirect_uri: "http://127.0.0.1:5173/oauth/callback".to_string(),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("upsert both config");
        crate::config::server::upsert_server_oauth_token(
            &context.db_pool,
            &ServerOAuthToken {
                id: None,
                server_id: both_id.clone(),
                access_token: "token-both".to_string(),
                refresh_token: None,
                token_type: "bearer".to_string(),
                expires_at: Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
                scope: Some("read write".to_string()),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("upsert both token");
        crate::config::server::upsert_server_headers(
            &context.db_pool,
            &both_id,
            &HashMap::from([("Authorization".to_string(), "Bearer manual".to_string())]),
        )
        .await
        .expect("upsert both auth header");

        let request = ServerListReq {
            enabled: Some(true),
            server_type: Some("streamable_http".to_string()),
            limit: Some(10),
            offset: Some(0),
        };

        let response = server_list_core(&request, &context.db_pool, &context.app_state)
            .await
            .expect("server list should succeed");
        let data = response.data.expect("server list data");
        let servers_by_id: HashMap<String, ServerDetailsData> = data
            .servers
            .into_iter()
            .filter_map(|server| server.id.clone().map(|id| (id, server)))
            .collect();

        let oauth_only = servers_by_id.get(&oauth_only_id).expect("oauth-only server");
        assert_eq!(oauth_only.auth_mode.as_deref(), Some("oauth"));
        assert!(matches!(
            oauth_only.oauth_status,
            Some(crate::core::oauth::OAuthConnectionState::Connected)
        ));
        assert!(matches!(
            oauth_only.oauth_custody_state,
            Some(crate::core::oauth::OAuthCustodyState::Unavailable)
        ));
        assert_eq!(oauth_only.oauth_requires_reconnect, Some(true));

        let header_only = servers_by_id.get(&header_only_id).expect("header-only server");
        assert_eq!(header_only.auth_mode.as_deref(), Some("header"));
        assert!(header_only.oauth_status.is_none());
        assert!(header_only.oauth_custody_state.is_none());

        let both = servers_by_id.get(&both_id).expect("both server");
        assert_eq!(both.auth_mode.as_deref(), Some("header"));
        assert!(matches!(
            both.oauth_status,
            Some(crate::core::oauth::OAuthConnectionState::Connected)
        ));
        assert!(matches!(
            both.oauth_custody_state,
            Some(crate::core::oauth::OAuthCustodyState::Unavailable)
        ));
        assert_eq!(both.oauth_requires_reconnect, Some(true));
    }

    #[tokio::test]
    async fn unify_direct_exposure_eligibility_round_trips_through_management_list_and_details() {
        let context = create_test_context().await;
        let server_id = seed_streamable_http_server(&context.db_pool, "eligibility-server").await;
        crate::config::server::upsert_server_oauth_config(
            &context.db_pool,
            &ServerOAuthConfig {
                id: None,
                server_id: server_id.clone(),
                authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                token_endpoint: "https://issuer.example.com/token".to_string(),
                client_id: "client-eligibility".to_string(),
                client_secret: None,
                scopes: Some("read".to_string()),
                redirect_uri: "http://127.0.0.1:5173/oauth/callback".to_string(),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("upsert eligibility oauth config");
        crate::config::server::upsert_server_oauth_token(
            &context.db_pool,
            &ServerOAuthToken {
                id: None,
                server_id: server_id.clone(),
                access_token: "token-eligibility".to_string(),
                refresh_token: None,
                token_type: "bearer".to_string(),
                expires_at: Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
                scope: Some("read".to_string()),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("upsert eligibility oauth token");

        let detail_before = server_details_core(
            &ServerDetailsReq { id: server_id.clone() },
            &context.db_pool,
            &context.app_state,
        )
        .await
        .expect("server details before update");
        let detail_before = detail_before.data.expect("server details payload before update");
        assert!(!detail_before.unify_direct_exposure_eligible);
        assert_eq!(detail_before.name, "eligibility-server");
        assert_eq!(
            detail_before.url.as_deref(),
            Some("https://example.com/eligibility-server")
        );

        let revisions =
            crate::core::capability::materializer::SurfaceAuthoringLoader::load_catalog_revision_set(&context.db_pool)
                .await
                .expect("load source revision set");
        crate::core::capability::management::ServerSurfaceManagement::set_direct_exposure_eligible(
            &context.db_pool,
            &server_id,
            true,
            revisions,
            "test",
        )
        .await
        .expect("server management response");

        let list_response = server_list_core(
            &ServerListReq {
                enabled: Some(true),
                server_type: Some("streamable_http".to_string()),
                limit: Some(10),
                offset: Some(0),
            },
            &context.db_pool,
            &context.app_state,
        )
        .await
        .expect("server list after update");
        let listed = list_response
            .data
            .expect("server list payload")
            .servers
            .into_iter()
            .find(|server| server.id.as_deref() == Some(server_id.as_str()))
            .expect("updated server in list");
        assert!(listed.unify_direct_exposure_eligible);
        assert_eq!(listed.name, "eligibility-server");
        assert_eq!(listed.url.as_deref(), Some("https://example.com/eligibility-server"));

        let detail_after = server_details_core(
            &ServerDetailsReq { id: server_id },
            &context.db_pool,
            &context.app_state,
        )
        .await
        .expect("server details after update");
        let detail_after = detail_after.data.expect("server details payload after update");
        assert!(detail_after.unify_direct_exposure_eligible);
        assert_eq!(detail_after.name, "eligibility-server");
        assert_eq!(
            detail_after.url.as_deref(),
            Some("https://example.com/eligibility-server")
        );
    }

    async fn create_test_context() -> TestContext {
        let temp_dir = TempDir::new().expect("temp dir");
        let db_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");

        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&db_pool)
            .await
            .expect("enable foreign keys");

        crate::helpers::prepare_config_database(&db_pool).await;
        crate::config::server::init::initialize_server_tables(&db_pool)
            .await
            .expect("init server tables");
        crate::config::profile::init::initialize_profile_tables(&db_pool)
            .await
            .expect("init profile tables");
        crate::config::client::init::initialize_client_table(&db_pool)
            .await
            .expect("init client tables");
        mcpmate_capability_store::SqliteCapabilityCatalog::new(db_pool.clone())
            .ensure_schema()
            .await
            .expect("init capability catalog tables");
        crate::core::capability::naming::initialize(db_pool.clone());

        let database = Arc::new(Database {
            pool: db_pool.clone(),
            path: PathBuf::from(":memory:"),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        });

        let app_state = Arc::new(AppState {
            connection_pool: Arc::new(Mutex::new(UpstreamConnectionPool::new(
                Arc::new(Config::default()),
                Some(database.clone()),
            ))),
            metrics_collector: Arc::new(MetricsCollector::new(Duration::from_secs(5))),
            http_proxy: None,
            profile_merge_service: None,
            database: Some(database),
            audit_database: None,
            audit_service: None,
            config_application_state: Arc::new(ConfigApplicationStateManager::new()),
            client_service: None,
            inspector_calls: Arc::new(InspectorCallRegistry::new()),
            inspector_sessions: Arc::new(InspectorSessionManager::new()),
            oauth_manager: RwLock::new(Some(Arc::new(crate::core::oauth::OAuthManager::new(db_pool.clone())))),
            secret_store: RwLock::new(None),
            secret_store_readiness: RwLock::new(crate::api::routes::unavailable_secret_store_readiness(
                "test_unavailable",
            )),
        });

        TestContext {
            _temp_dir: temp_dir,
            app_state,
            db_pool,
        }
    }

    async fn seed_servers(
        pool: &sqlx::SqlitePool,
        count: usize,
    ) -> Vec<String> {
        let mut ids = Vec::with_capacity(count);
        for index in 0..count {
            let server = Server {
                id: None,
                name: format!("server_{index}"),
                server_type: ServerType::Stdio,
                command: Some("echo".to_string()),
                url: None,
                source: None,
                enabled: crate::common::status::EnabledStatus::Enabled,
                unify_direct_exposure_eligible: false,
                created_at: None,
                updated_at: None,
                pending_import: false,
            };
            let id = server::upsert_server_definition(
                pool,
                &server,
                &ServerTransportDraft::Stdio {
                    command: Some("echo".to_string()),
                    args: Vec::new(),
                    env: Default::default(),
                },
            )
            .await
            .expect("insert typed server definition");
            ids.push(id);
        }
        ids
    }

    async fn seed_streamable_http_server(
        pool: &sqlx::SqlitePool,
        name: &str,
    ) -> String {
        let server = Server {
            id: None,
            name: name.to_string(),
            server_type: ServerType::StreamableHttp,
            command: None,
            url: Some(format!("https://example.com/{name}")),
            source: None,
            enabled: crate::common::status::EnabledStatus::Enabled,
            unify_direct_exposure_eligible: false,
            created_at: None,
            updated_at: None,
            pending_import: false,
        };

        server::upsert_server_definition(
            pool,
            &server,
            &ServerTransportDraft::Http {
                protocol: HttpTransportKind::StreamableHttp,
                endpoint: server.url.clone(),
                headers: Default::default(),
            },
        )
        .await
        .expect("insert typed streamable http server")
    }

    async fn seed_server_meta(
        pool: &sqlx::SqlitePool,
        server_id: &str,
    ) {
        let meta = crate::config::models::ServerMeta {
            id: None,
            server_id: server_id.to_string(),
            description: Some("Server description".to_string()),
            website: Some("https://example.com".to_string()),
            repository: Some(r#"{"url":"https://example.com/repo","source":"github"}"#.to_string()),
            registry_version: Some("1.2.3".to_string()),
            registry_meta_json: None,
            extras_json: None,
            author: None,
            category: None,
            recommended_scenario: None,
            rating: None,
            icons_json: Some(r#"[{"src":"https://example.com/icon.png","mimeType":"image/png"}]"#.to_string()),
            upstream_name: None,
            upstream_title: None,
            server_version: None,
            protocol_version: None,
            created_at: None,
            updated_at: None,
        };

        server::upsert_server_meta(pool, &meta)
            .await
            .expect("upsert server meta");
    }

    async fn seed_server_capabilities(
        pool: &sqlx::SqlitePool,
        server_id: &str,
    ) {
        let initialize = serde_json::from_value(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}, "prompts": {}},
            "serverInfo": {"name": "server_0", "version": "1.0.0"}
        }))
        .expect("build initialize fixture");
        let tool = rmcp::model::Tool::new("tool-a", "tool", std::sync::Arc::new(serde_json::Map::new()));
        let prompt = rmcp::model::Prompt::new("prompt-a", None::<String>, None);
        server::capabilities::commit_protocol_items_for_kinds(
            pool,
            server_id,
            "server_0",
            Some(initialize),
            vec![tool],
            Vec::new(),
            vec![prompt],
            Vec::new(),
            crate::core::pool::CapSyncFlags::ALL,
        )
        .await
        .expect("commit capability catalog");
    }

    async fn seed_profile_enablement(
        pool: &sqlx::SqlitePool,
        server_id: &str,
    ) {
        let mut profile = Profile::new_with_description(
            "default".to_string(),
            Some("default profile".to_string()),
            ProfileType::Shared,
        );
        profile.is_active = true;
        profile.is_default = true;
        let profile_id = crate::helpers::insert_profile(pool, &profile).await;
        profile::add_server_to_profile(pool, &profile_id, server_id, true)
            .await
            .expect("attach server to profile");
    }

    async fn seed_runtime_instance(
        app_state: &Arc<AppState>,
        server_id: &str,
    ) {
        let mut pool = app_state.connection_pool.lock().await;
        let mut instance = UpstreamConnection::new("server_0".to_string());
        instance.status = crate::core::foundation::types::ConnectionStatus::Ready;
        pool.connections.insert(
            server_id.to_string(),
            HashMap::from([("instance-1".to_string(), instance)]),
        );
    }
}
