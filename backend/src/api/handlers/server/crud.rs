// MCPMate Proxy API handlers for MCP server CRUD operations
// Contains handler functions for creating, updating, and importing servers

#![expect(
    clippy::result_large_err,
    reason = "Propagates CapabilityReadError from capability discovery."
)]

use super::{basic::load_server_transport_validity, common, shared::*};
use crate::api::models::server::{
    ServerCapabilityDiscoveryData, ServerCreateReq, ServerDeleteReq, ServerDetailsData, ServerDetailsResp,
    ServerMetaPayload, ServerNamespaceRemediationReq, ServerOperationData, ServerOperationResp, ServerUpdateReq,
    ServersImportData, ServersImportReq, ServersImportResp, SkippedServerData,
};
use crate::{
    api::handlers::{
        ApiError,
        common::{internal_error, map_anyhow_error, map_database_error},
    },
    config::server::{
        ImportOptions, ImportOutcome, SkippedServer, headers::has_non_empty_authorization_header,
        import::server_meta_from_payload, import_batch,
    },
    config::{
        database::Database,
        models::{
            ConfigValue, ServerTransportDraft, ValidatedTransport, is_redacted_display_value,
            restore_redacted_env_values, restore_redacted_http_header_values,
        },
        server::{self},
    },
    core::secrets::{mcp_config_from_server, sync_server_secret_usages},
};
use axum::{Json, extract::State};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

async fn discover_server_capabilities(
    state: &Arc<AppState>,
    database: Arc<Database>,
    server_id: &str,
) -> Result<(), crate::core::capability::read_service::CapabilityReadError> {
    let lists = crate::core::capability::read_service::CapabilityReadService::from_runtime(
        database,
        state.connection_pool.clone(),
    )
    .list_all_kinds(
        server_id,
        Some(crate::core::capability::runtime::RefreshStrategy::CacheFirst),
    )
    .await?;
    if let Some(error) = lists.into_first_error() {
        return Err(error);
    }
    Ok(())
}

fn validate_server_transport(draft: &ServerTransportDraft) -> Result<ValidatedTransport, ApiError> {
    draft.validate().map_err(|diagnostics| {
        let details = diagnostics
            .into_iter()
            .map(|diagnostic| format!("{} ({})", diagnostic.code, diagnostic.field))
            .collect::<Vec<_>>()
            .join(", ");
        ApiError::BadRequest(format!("Invalid server transport: {details}"))
    })
}

async fn restore_redacted_transport_values_for_update(
    db: &Database,
    server_id: &str,
    draft: &mut ServerTransportDraft,
) -> Result<(), ApiError> {
    let contains_redacted_value = match draft {
        ServerTransportDraft::Stdio { env, .. } => env
            .values()
            .any(|value| matches!(value, ConfigValue::Literal { value } if is_redacted_display_value(value))),
        ServerTransportDraft::Http { headers, .. } => headers
            .values()
            .any(|value| matches!(value, ConfigValue::Literal { value } if is_redacted_display_value(value))),
        ServerTransportDraft::Unrecognized { .. } => false,
    };
    if !contains_redacted_value {
        return Ok(());
    }

    let existing = crate::config::server::get_server_transport_draft(&db.pool, server_id)
        .await
        .map_err(map_anyhow_error)?
        .ok_or_else(|| {
            ApiError::BadRequest(
                "Cannot preserve redacted transport values because the persisted transport draft is missing"
                    .to_string(),
            )
        })?;
    match draft {
        ServerTransportDraft::Stdio { env, .. } => {
            let existing_env = match existing {
                ServerTransportDraft::Stdio { env, .. } => env,
                _ => BTreeMap::new(),
            };
            restore_redacted_env_values(env, &existing_env);
            Ok(())
        }
        ServerTransportDraft::Http { headers, .. } => {
            let existing_headers = match existing {
                ServerTransportDraft::Http { headers, .. } => headers,
                _ => BTreeMap::new(),
            };
            restore_redacted_http_header_values(headers, &existing_headers);
            Ok(())
        }
        ServerTransportDraft::Unrecognized { .. } => Ok(()),
    }
}

pub async fn remediate_server_namespace(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ServerNamespaceRemediationReq>,
) -> Result<Json<ServerOperationResp>, ApiError> {
    let db = common::get_database_from_state(&state)?;
    crate::config::server::validate_server_namespace(&payload.namespace)
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;

    let issue = common::load_namespace_issue(&db.pool, &payload.id)
        .await
        .map_err(map_anyhow_error)?;
    if issue.is_none() {
        return Err(ApiError::Conflict(
            "Namespace remediation is available only for servers with an unresolved namespace issue".to_string(),
        ));
    }
    if let Some(owner) = crate::config::server::get_server(&db.pool, &payload.namespace)
        .await
        .map_err(map_anyhow_error)?
        .filter(|server| server.id.as_deref() != Some(payload.id.as_str()))
    {
        return Err(ApiError::Conflict(format!(
            "Namespace '{}' is already used by server '{}'",
            payload.namespace,
            owner.id.unwrap_or(owner.name)
        )));
    }

    let secret_store = state.secret_store.read().await.clone();
    let (server, server_config) =
        crate::core::foundation::loader::load_server_config_strict(db.as_ref(), &payload.id, secret_store)
            .await
            .map_err(map_anyhow_error)?;
    let mut snapshot =
        crate::config::server::capabilities::discover_from_config(&payload.id, &server_config, server.server_type)
            .await
            .map_err(map_anyhow_error)?;

    if let Err(error) = crate::config::server::namespace_repair::remediate_namespace_with_snapshot(
        &db.pool,
        &payload.id,
        &payload.namespace,
        &mut snapshot,
    )
    .await
    {
        if error
            .downcast_ref::<crate::core::capability::naming::ExternalIdentifierCollision>()
            .is_some()
        {
            crate::config::server::namespace_repair::record_capability_collision_from_error(&db.pool, &error)
                .await
                .map_err(map_anyhow_error)?;
            return Err(ApiError::Conflict(error.to_string()));
        }
        return Err(map_anyhow_error(error));
    }

    let mut pending_steps = Vec::new();
    if let Err(error) = crate::config::server::capabilities::apply_discovered_snapshot(
        &db.pool,
        db.capability_cache.as_ref(),
        &payload.id,
        &payload.namespace,
        &snapshot,
        true,
    )
    .await
    {
        if let Err(record_error) =
            crate::config::server::namespace_repair::record_capability_collision_from_error(&db.pool, &error).await
        {
            tracing::warn!(
                server_id = %payload.id,
                error = %record_error,
                "Failed to record post-commit capability collision"
            );
        }
        pending_steps.push(format!("capability cache refresh: {error}"));
    }

    let pool_sync_result = {
        let mut pool = state.connection_pool.lock().await;
        pool.sync_servers_from_active_profile().await
    };
    if let Err(error) = pool_sync_result {
        pending_steps.push(format!("connection pool refresh: {error}"));
    }

    if !pending_steps.is_empty() {
        tracing::warn!(
            server_id = %payload.id,
            pending_steps = %pending_steps.join("; "),
            "Namespace remediation committed, but runtime convergence is pending"
        );
    }

    let (result, status) = if pending_steps.is_empty() {
        ("Namespace remediated", "remediated")
    } else {
        (
            "Namespace remediated; runtime convergence pending",
            "remediated_pending",
        )
    };

    Ok(Json(ServerOperationResp::success(ServerOperationData {
        id: payload.id,
        name: payload.namespace,
        result: result.to_string(),
        status: status.to_string(),
        allowed_operations: Vec::new(),
    })))
}

async fn clear_oauth_auth_source_for_manual_authorization(
    state: &Arc<AppState>,
    db: &Database,
    server_id: &str,
    headers: &HashMap<String, String>,
) -> Result<(), ApiError> {
    if !has_non_empty_authorization_header(headers) {
        return Ok(());
    }

    delete_oauth_secrets_for_server_best_effort(state, db, server_id).await?;
    crate::config::server::delete_server_oauth_config(&db.pool, server_id)
        .await
        .map_err(map_anyhow_error)?;
    crate::config::server::delete_server_oauth_token(&db.pool, server_id)
        .await
        .map_err(map_anyhow_error)?;
    Ok(())
}

async fn upsert_meta_payload(
    db: &Database,
    server_id: &str,
    payload: &ServerMetaPayload,
) -> Result<(), ApiError> {
    let mut meta = server_meta_from_payload(server_id, payload).map_err(|err| ApiError::BadRequest(err.to_string()))?;
    meta.server_version = None;
    meta.protocol_version = None;

    server::upsert_server_meta(&db.pool, &meta)
        .await
        .map_err(map_anyhow_error)
        .map(|_| ())
}

async fn sync_secret_usages_for_server(
    state: &Arc<AppState>,
    db: &Database,
    server_id: &str,
    server: &Server,
) -> Result<(), ApiError> {
    let Some(secret_store) = state.secret_store.read().await.clone() else {
        return Ok(());
    };

    let config = mcp_config_from_server(&db.pool, server_id, server)
        .await
        .map_err(map_anyhow_error)?;

    sync_server_secret_usages(secret_store.as_ref(), server_id, &config)
        .await
        .map_err(map_anyhow_error)
}

async fn delete_oauth_secrets_for_server(
    state: &Arc<AppState>,
    db: &Database,
    server_id: &str,
) -> anyhow::Result<()> {
    let manager =
        crate::core::oauth::OAuthManager::new_optional_store(db.pool.clone(), state.secret_store.read().await.clone());

    manager.delete_all_oauth_secrets(server_id).await
}

async fn delete_oauth_secret_rows_for_server(
    db: &Database,
    server_id: &str,
) -> anyhow::Result<()> {
    use crate::core::oauth::manager::{OAuthSecretSlot, oauth_secret_alias};

    for slot in [
        OAuthSecretSlot::ClientSecret,
        OAuthSecretSlot::AccessToken,
        OAuthSecretSlot::RefreshToken,
    ] {
        let alias = oauth_secret_alias(server_id, slot);
        sqlx::query("DELETE FROM secure_store_secrets WHERE alias = ?")
            .bind(alias)
            .execute(&db.pool)
            .await?;
    }
    Ok(())
}

fn is_oauth_secret_cleanup_unavailable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::core::oauth::manager::OAuthSecretCleanupUnavailable>()
        .is_some()
}

async fn delete_oauth_secrets_for_server_best_effort(
    state: &Arc<AppState>,
    db: &Database,
    server_id: &str,
) -> Result<(), ApiError> {
    match delete_oauth_secrets_for_server(state, db, server_id).await {
        Ok(()) => Ok(()),
        Err(error) if is_oauth_secret_cleanup_unavailable(&error) => {
            tracing::warn!(
                server_id,
                error = %error,
                "Deleting OAuth secret metadata without decrypting because Secure Store is unavailable during server deletion"
            );
            delete_oauth_secret_rows_for_server(db, server_id)
                .await
                .map_err(map_anyhow_error)?;
            Ok(())
        }
        Err(error) => Err(map_anyhow_error(error)),
    }
}

/// Create a new MCP server configuration
///
/// This endpoint creates a new MCP server configuration from a required tagged
/// transport definition. The definition determines the applicable fields for
/// stdio, legacy SSE, and Streamable HTTP transports.
///
/// **Endpoint**: `POST /mcp/servers/create`
///
/// # Parameters
/// - `payload`: Server creation request containing a name and tagged transport.
///
/// # Returns
/// - Success: Returns detailed information of the created server
/// - Failure: Returns specific error information and correction suggestions
///
/// # Error Handling
/// - 400 Bad Request: Transport definition is invalid
/// - 409 Conflict: Server name already exists
/// - 500 Internal Server Error: Database operation failed
///
pub async fn create_server(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ServerCreateReq>,
) -> Result<Json<ServerDetailsResp>, ApiError> {
    let started_at = std::time::Instant::now();
    let db = common::get_database_from_state(&state)?;
    let validated_transport = validate_server_transport(&payload.transport)?;
    let is_pending_import = payload.pending_import.unwrap_or(false);

    crate::config::server::validate_server_namespace(&payload.name)
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;

    let existing_server = crate::config::server::get_server(&db.pool, &payload.name)
        .await
        .map_err(map_anyhow_error)?;
    let reusable_pending_server = existing_server
        .as_ref()
        .filter(|server| is_pending_import && server.pending_import);
    let reusable_pending_server_id = reusable_pending_server.and_then(|server| server.id.clone());
    if existing_server.is_some() && reusable_pending_server.is_none() {
        return Err(ApiError::Conflict(format!(
            "Server with name '{}' already exists. Please choose a different name for your server.",
            payload.name
        )));
    }

    if let Some(server_id) = reusable_pending_server_id.as_deref() {
        delete_oauth_secrets_for_server_best_effort(&state, &db, server_id).await?;
        crate::config::server::delete_server_oauth_config(&db.pool, server_id)
            .await
            .map_err(map_anyhow_error)?;
        crate::config::server::delete_server_oauth_token(&db.pool, server_id)
            .await
            .map_err(map_anyhow_error)?;
    }

    let mut server = Server::new(payload.name.clone(), validated_transport.server_type());
    if let Some(existing) = reusable_pending_server {
        server.id = existing.id.clone();
    }
    server.source = payload.source.clone();
    server.pending_import = is_pending_import;
    server.unify_direct_exposure_eligible = payload.unify_direct_exposure_eligible.unwrap_or(false);
    if server.pending_import {
        server.enabled = crate::common::status::EnabledStatus::Disabled;
    }

    let runtime_headers = validated_transport.runtime_headers();
    let server_id = crate::config::server::upsert_server_definition(&db.pool, &server, &payload.transport)
        .await
        .map_err(map_anyhow_error)?;
    crate::core::capability::resolver::upsert(&server_id, &payload.name).await;

    clear_oauth_auth_source_for_manual_authorization(&state, &db, &server_id, &runtime_headers).await?;

    let server_row = crate::config::server::get_server_by_id(&db.pool, &server_id)
        .await
        .map_err(map_anyhow_error)?
        .ok_or_else(|| internal_error("Server record missing after creation"))?;

    sync_secret_usages_for_server(&state, &db, &server_id, &server_row).await?;

    // Apply optional metadata payload
    if let Some(meta_payload) = payload.meta.as_ref() {
        upsert_meta_payload(&db, &server_id, meta_payload).await?;
    }

    if !server_row.pending_import {
        let mut pool = state.connection_pool.lock().await;
        pool.sync_servers_from_active_profile()
            .await
            .map_err(map_anyhow_error)?;
    }

    // Initial capability discovery persists the transactional SQLite catalog.
    let capability_discovery = if !server_row.pending_import {
        match discover_server_capabilities(&state, db.clone(), &server_id).await {
            Ok(()) => Some(ServerCapabilityDiscoveryData::succeeded()),
            Err(error) => {
                tracing::warn!(server_id = %server_id, error = %error, "Initial capability sync failed after server creation");
                Some(ServerCapabilityDiscoveryData::failed())
            }
        }
    } else {
        None
    };

    let server_name = server_row.name.clone();
    let source = server_row.source.clone();
    let command = server_row.command.clone();
    let url = server_row.url.clone();
    let server_type = server_row.server_type;
    let created_at = server_row.created_at.map(|dt| dt.to_rfc3339());
    let updated_at = server_row.updated_at.map(|dt| dt.to_rfc3339());

    let details = common::get_complete_server_details(&db.pool, &server_id, &server_name, &state).await;
    let effective_enabled = details.globally_enabled && details.enabled_in_profile;
    let oauth_summary = common::load_server_oauth_response_summary(
        &db.pool,
        &state,
        &server_id,
        server_row.server_type.is_http_transport(),
    )
    .await?;
    let transport_validity = load_server_transport_validity(&db.pool, &server_id).await;

    let audit_server_id = server_id.clone();
    let response = Json(ServerDetailsResp::success(ServerDetailsData {
        id: Some(server_id.clone()),
        name: server_name,
        source,
        enabled: effective_enabled,
        globally_enabled: details.globally_enabled,
        enabled_in_profile: details.enabled_in_profile,
        unify_direct_exposure_eligible: server_row.unify_direct_exposure_eligible,
        server_type,
        transport_validity,
        command,
        url,
        args: details.args,
        env: details.env,
        headers: None,
        meta: details.meta,
        server_info: details.server_info,
        capability: details.capability,
        protocol_version: details.protocol_version,
        created_at,
        updated_at,
        instances: details.instances,
        auth_mode: None,
        oauth_status: oauth_summary.oauth_status,
        oauth_custody_state: oauth_summary.oauth_custody_state,
        oauth_requires_reconnect: oauth_summary.oauth_requires_reconnect,
        oauth_issue: oauth_summary.oauth_issue,
        namespace_issue: common::load_namespace_issue(&db.pool, &server_id)
            .await
            .map_err(map_anyhow_error)?,
        source_revision_set: crate::core::capability::materializer::SurfaceAuthoringLoader::load_catalog_revision_set(
            &db.pool,
        )
        .await
        .map_err(|error| ApiError::InternalError(error.to_string()))?
        .into_iter()
        .collect(),
        capability_discovery,
    }));

    let mut data = Map::new();
    data.insert("server_name".to_string(), Value::String(payload.name));
    crate::audit::interceptor::emit_event(
        state.audit_service.as_ref(),
        crate::audit::interceptor::build_rest_event(
            crate::audit::AuditAction::ServerCreate,
            crate::audit::AuditStatus::Success,
            "POST",
            "/api/mcp/servers/create",
            Some(started_at.elapsed().as_millis() as u64),
            Some(audit_server_id),
            None,
            Some(data),
            None,
        ),
    )
    .await;

    Ok(response)
}

/// Update an existing MCP server configuration
///
/// This endpoint replaces an existing server's transport with a required tagged
/// transport definition.
///
/// **Endpoint**: `POST /mcp/servers/update`
///
/// # Parameters
/// - `payload`: Server update request containing the replacement transport.
///
/// # Returns
/// - Success: Returns detailed information of the updated server
/// - Failure: Returns specific error information and correction suggestions
///
/// # Error Handling
/// - 400 Bad Request: Transport definition is invalid
/// - 404 Not Found: The specified server does not exist
/// - 500 Internal Server Error: Database operation failed
pub async fn update_server(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ServerUpdateReq>,
) -> Result<Json<ServerDetailsResp>, ApiError> {
    let started_at = std::time::Instant::now();
    validate_server_update_management_contract(&payload)?;
    let mut transport = payload.transport;
    validate_server_transport(&transport)?;
    let db = common::get_database_from_state(&state)?;

    let id = payload.id.clone();
    // Get existing server by ID
    let existing_server = crate::config::server::get_server_by_id(&db.pool, &id)
        .await
        .map_err(map_anyhow_error)?
        .ok_or_else(|| ApiError::NotFound(format!("Server with ID '{id}' not found")))?;
    let server_id = existing_server
        .id
        .clone()
        .ok_or_else(|| internal_error("Server ID not found"))?;
    restore_redacted_transport_values_for_update(&db, &server_id, &mut transport).await?;
    let validated_transport = validate_server_transport(&transport)?;
    let previous_config_fingerprint =
        crate::config::server::capabilities::previous_valid_config_fingerprint(&db.pool, &server_id)
            .await
            .map_err(map_anyhow_error)?;

    // Create updated server model
    let mut updated_server = existing_server.clone();

    if let Some(s) = payload.source {
        updated_server.source = Some(s);
    }

    if let Some(pending_import) = payload.pending_import {
        updated_server.pending_import = pending_import;
        if pending_import {
            updated_server.enabled = crate::common::status::EnabledStatus::Disabled;
        }
    }

    let runtime_headers = validated_transport.runtime_headers();
    crate::config::server::upsert_server_definition(&db.pool, &updated_server, &transport)
        .await
        .map_err(map_anyhow_error)?;

    clear_oauth_auth_source_for_manual_authorization(&state, &db, &server_id, &runtime_headers).await?;

    let updated_server = crate::config::server::get_server_by_id(&db.pool, &server_id)
        .await
        .map_err(map_anyhow_error)?
        .ok_or_else(|| internal_error("Server record missing after update"))?;

    sync_secret_usages_for_server(&state, &db, &server_id, &updated_server).await?;

    if let Some(meta_payload) = payload.meta.as_ref() {
        upsert_meta_payload(&db, &server_id, meta_payload).await?;
    }

    let current_config_fingerprint =
        crate::config::server::capabilities::current_config_fingerprint(&db.pool, &server_id)
            .await
            .map_err(map_anyhow_error)?;
    let configuration_changed = previous_config_fingerprint.as_deref() != Some(current_config_fingerprint.as_str());

    if !updated_server.pending_import && configuration_changed {
        let mut pool = state.connection_pool.lock().await;
        pool.invalidate_server_runtime(&server_id).await;
        pool.sync_servers_from_active_profile()
            .await
            .map_err(map_anyhow_error)?;
        drop(pool);
    }

    let capability_discovery = if !updated_server.pending_import
        && (configuration_changed || existing_server.pending_import)
    {
        match discover_server_capabilities(&state, db.clone(), &server_id).await {
            Ok(()) => Some(ServerCapabilityDiscoveryData::succeeded()),
            Err(error) => {
                tracing::warn!(server_id = %server_id, error = %error, "Capability sync failed after server configuration update");
                Some(ServerCapabilityDiscoveryData::failed())
            }
        }
    } else {
        None
    };

    // Get server details via shared helper
    let details = common::get_complete_server_details(&db.pool, &server_id, &updated_server.name, &state).await;
    let oauth_summary = common::load_server_oauth_response_summary(
        &db.pool,
        &state,
        &server_id,
        updated_server.server_type.is_http_transport(),
    )
    .await?;
    let transport_validity = load_server_transport_validity(&db.pool, &server_id).await;

    // Return success response
    let audit_server_id = server_id.clone();
    let audit_server_name = updated_server.name.clone();
    let response = Json(ServerDetailsResp::success(ServerDetailsData {
        id: Some(server_id),
        name: updated_server.name.clone(),
        source: updated_server.source.clone(),
        enabled: details.globally_enabled && details.enabled_in_profile,
        globally_enabled: details.globally_enabled,
        enabled_in_profile: details.enabled_in_profile,
        unify_direct_exposure_eligible: updated_server.unify_direct_exposure_eligible,
        server_type: updated_server.server_type,
        transport_validity,
        command: updated_server.command.clone(),
        url: updated_server.url.clone(),
        args: details.args,
        env: details.env,
        headers: None,
        meta: details.meta,
        server_info: details.server_info,
        capability: details.capability,
        protocol_version: details.protocol_version,
        created_at: updated_server.created_at.map(|dt| dt.to_rfc3339()),
        updated_at: updated_server.updated_at.map(|dt| dt.to_rfc3339()),
        instances: details.instances,
        auth_mode: None,
        oauth_status: oauth_summary.oauth_status,
        oauth_custody_state: oauth_summary.oauth_custody_state,
        oauth_requires_reconnect: oauth_summary.oauth_requires_reconnect,
        oauth_issue: oauth_summary.oauth_issue,
        namespace_issue: None,
        source_revision_set: crate::core::capability::materializer::SurfaceAuthoringLoader::load_catalog_revision_set(
            &db.pool,
        )
        .await
        .map_err(|error| ApiError::InternalError(error.to_string()))?
        .into_iter()
        .collect(),
        capability_discovery,
    }));

    let mut data = Map::new();
    data.insert("server_name".to_string(), Value::String(audit_server_name));
    crate::audit::interceptor::emit_event(
        state.audit_service.as_ref(),
        crate::audit::interceptor::build_rest_event(
            crate::audit::AuditAction::ServerUpdate,
            crate::audit::AuditStatus::Success,
            "POST",
            "/api/mcp/servers/update",
            Some(started_at.elapsed().as_millis() as u64),
            Some(audit_server_id),
            None,
            Some(data),
            None,
        ),
    )
    .await;

    Ok(response)
}

fn validate_server_update_management_contract(payload: &ServerUpdateReq) -> Result<(), ApiError> {
    if payload.enabled.is_some() {
        return Err(ApiError::BadRequest(
            "Update global server status through /api/mcp/servers/manage".to_string(),
        ));
    }
    if payload.profile_ids.is_some() {
        return Err(ApiError::BadRequest(
            "Update profile server relationships through /api/mcp/profile/servers/manage".to_string(),
        ));
    }
    if payload.unify_direct_exposure_eligible.is_some() {
        return Err(ApiError::BadRequest(
            "Update direct exposure eligibility through /api/mcp/servers/manage".to_string(),
        ));
    }
    Ok(())
}

/// Import servers from JSON configuration (now uses unified core)
///
/// **Endpoint:** `POST /mcp/servers/import`
pub async fn import_servers(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ServersImportReq>,
) -> Result<Json<ServersImportResp>, ApiError> {
    let started_at = std::time::Instant::now();
    let db = common::get_database_from_state(&state)?;

    // Validate: require either client_identifier or mcp_servers
    if payload.client_identifier.is_none() && payload.mcp_servers.is_empty() {
        return Err(ApiError::BadRequest(
            "Either 'client_identifier' or 'mcp_servers' must be provided".to_string(),
        ));
    }

    let mcp_servers = if let Some(ref client_id) = payload.client_identifier {
        let service = state
            .client_service
            .as_ref()
            .ok_or_else(|| ApiError::InternalError("Client service unavailable".into()))?;

        service
            .fetch_state(client_id)
            .await
            .map_err(|err| ApiError::InternalError(err.to_string()))?
            .ok_or_else(|| ApiError::NotFound(format!("Client '{}' not found", client_id)))?;

        let plan = crate::config::server::plan_import_from_client_inspection(
            service,
            client_id,
            None,
            None,
            payload.selected_server_names.as_slice(),
        )
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;

        plan.items
    } else {
        payload.mcp_servers
    };

    let import_options = ImportOptions::dashboard_import(payload.dry_run);
    let outcome = import_batch(db.clone(), Some(&state.connection_pool), mcp_servers, import_options)
        .await
        .map_err(map_anyhow_error)?;

    let ImportOutcome {
        imported,
        skipped,
        failed,
        scheduled: _,
        runtime_sync_error,
    } = outcome;

    let imported_servers: Vec<String> = imported.into_iter().map(|s| s.name).collect();
    let skipped_servers: Vec<SkippedServerData> = skipped.into_iter().map(skipped_server_to_api).collect();
    let failed_servers: Vec<String> = failed.keys().cloned().collect();
    let error_details = if failed.is_empty() { None } else { Some(failed) };

    let import_data = ServersImportData {
        imported_count: imported_servers.len(),
        imported_servers,
        skipped_count: skipped_servers.len(),
        skipped_servers,
        failed_count: failed_servers.len(),
        failed_servers,
        error_details,
        runtime_sync_error,
    };

    let mut data = Map::new();
    data.insert(
        "imported_count".to_string(),
        Value::from(import_data.imported_count as u64),
    );
    data.insert(
        "skipped_count".to_string(),
        Value::from(import_data.skipped_count as u64),
    );
    data.insert("failed_count".to_string(), Value::from(import_data.failed_count as u64));
    if let Some(error) = import_data.runtime_sync_error.as_ref() {
        data.insert("runtime_sync_error".to_string(), Value::from(error.clone()));
    }
    let audit_error = import_data.runtime_sync_error.clone();
    crate::audit::interceptor::emit_event(
        state.audit_service.as_ref(),
        crate::audit::interceptor::build_rest_event(
            crate::audit::AuditAction::ServerImport,
            if import_data.failed_count > 0 || audit_error.is_some() {
                crate::audit::AuditStatus::Failed
            } else {
                crate::audit::AuditStatus::Success
            },
            "POST",
            "/api/mcp/servers/import",
            Some(started_at.elapsed().as_millis() as u64),
            None,
            None,
            Some(data),
            audit_error,
        ),
    )
    .await;

    Ok(Json(ServersImportResp::success(import_data)))
}

fn skipped_server_to_api(source: SkippedServer) -> SkippedServerData {
    SkippedServerData::from(source)
}

/// Disconnect server instances from connection pool
async fn disconnect_server_instances(
    state: &Arc<AppState>,
    name: &str,
) {
    let mut pool =
        match crate::api::handlers::server::common::ConnectionPoolManager::get_pool_for_health_check(state).await {
            Ok(pool) => pool,
            Err(_) => {
                tracing::warn!("Failed to get connection pool, proceeding with server deletion anyway");
                return;
            }
        };

    let Some(instances) = pool.connections.get(name) else {
        return;
    };

    let instance_ids: Vec<String> = instances.keys().cloned().collect();
    for instance_id in instance_ids {
        if let Err(e) = pool.disconnect(name, &instance_id).await {
            tracing::warn!(
                "Failed to disconnect instance '{}' of server '{}': {}",
                instance_id,
                name,
                e
            );
        }
    }
}

/// Delete server-related records from database
async fn delete_server_records(
    state: &Arc<AppState>,
    db: &Database,
    server_id: &str,
) -> Result<(), ApiError> {
    delete_oauth_secrets_for_server_best_effort(state, db, server_id).await?;

    let mut tx = db.pool.begin().await.map_err(map_database_error)?;

    // Purge secret usages for this server first (no CASCADE FK to server_config).
    sqlx::query("DELETE FROM secure_store_usages WHERE server_id = ?")
        .bind(server_id)
        .execute(&mut *tx)
        .await
        .map_err(map_database_error)?;

    crate::core::capability::reconciliation::CatalogSurfaceReconciler::new(db.pool.clone())
        .retire_server_in_transaction(&mut tx, server_id)
        .await
        .map_err(|error| ApiError::InternalError(error.to_string()))?;

    // Use CASCADE DELETE for server_config and its FK-linked tables.
    sqlx::query("DELETE FROM server_config WHERE id = ?")
        .bind(server_id)
        .execute(&mut *tx)
        .await
        .map_err(map_database_error)?;

    // The following tables will be automatically cleaned up by CASCADE DELETE:
    // - server_tools (has FK to server_config.id)
    // - server_args (has FK to server_config.id)
    // - server_env (has FK to server_config.id)
    // - server_meta (has FK to server_config.id)
    // - server_oauth_config (has FK to server_config.id)
    // - server_oauth_tokens (has FK to server_config.id)
    // Capability refs, versions, and authoring intent remain as retired history.

    tx.commit().await.map_err(map_database_error)?;
    db.capability_cache.invalidate_server(server_id).await;
    Ok(())
}

/// Delete an existing MCP server (updated for payload parameters)
///
/// **Endpoint:** `DELETE /mcp/servers/delete`
pub async fn delete_server(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ServerDeleteReq>,
) -> Result<Json<ServerOperationData>, ApiError> {
    let started_at = std::time::Instant::now();
    let db = common::get_database_from_state(&state)?;

    let id = request.id;

    // Get existing server by ID
    let existing_server = crate::config::server::get_server_by_id(&db.pool, &id)
        .await
        .map_err(map_anyhow_error)?
        .ok_or_else(|| ApiError::NotFound(format!("Server with ID '{id}' not found")))?;
    let server_id = existing_server
        .id
        .clone()
        .ok_or_else(|| internal_error("Server ID not found"))?;

    // Disconnect server instances
    disconnect_server_instances(&state, &existing_server.name).await;

    // Delete all server-related records
    delete_server_records(&state, &db, &server_id).await?;

    // Remove resolver mapping to keep id<->name cache consistent
    crate::core::capability::resolver::remove_by_id(&server_id).await;

    tracing::info!("Successfully deleted server '{}'", existing_server.name);

    // Return success response
    let response = Json(ServerOperationData {
        id: server_id,
        name: existing_server.name,
        result: "Successfully deleted server".to_string(),
        status: "Deleted".to_string(),
        allowed_operations: Vec::new(),
    });

    crate::audit::interceptor::emit_event(
        state.audit_service.as_ref(),
        crate::audit::interceptor::build_rest_event(
            crate::audit::AuditAction::ServerDelete,
            crate::audit::AuditStatus::Success,
            "DELETE",
            "/api/mcp/servers/delete",
            Some(started_at.elapsed().as_millis() as u64),
            Some(response.0.id.clone()),
            None,
            None,
            None,
        ),
    )
    .await;

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::validate_server_update_management_contract;
    use crate::api::{handlers::ApiError, models::server::ServerUpdateReq};

    #[test]
    fn server_update_rejects_surface_management_bypasses() {
        for payload in [
            serde_json::json!({
                "id": "server-a",
                "transport": {"kind": "stdio", "command": "server-a", "args": [], "env": {}},
                "enabled": true
            }),
            serde_json::json!({
                "id": "server-a",
                "transport": {"kind": "stdio", "command": "server-a", "args": [], "env": {}},
                "profile_ids": ["profile-a"]
            }),
            serde_json::json!({
                "id": "server-a",
                "transport": {"kind": "stdio", "command": "server-a", "args": [], "env": {}},
                "unify_direct_exposure_eligible": true
            }),
        ] {
            let request: ServerUpdateReq = serde_json::from_value(payload).unwrap();
            assert!(matches!(
                validate_server_update_management_contract(&request),
                Err(ApiError::BadRequest(_))
            ));
        }
    }

    use super::*;
    use crate::{
        config::models::{ConfigValue, HttpTransportKind, ServerOAuthConfig, ServerOAuthToken, ServerTransportDraft},
        core::{
            models::Config, pool::UpstreamConnectionPool, profile::ConfigApplicationStateManager,
            secrets::store::LocalSecretStore,
        },
        inspector::{calls::InspectorCallRegistry, sessions::InspectorSessionManager},
        system::metrics::MetricsCollector,
    };
    use sqlx::sqlite::SqlitePoolOptions;
    use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};
    use tempfile::TempDir;
    use tokio::sync::{Mutex, RwLock};

    struct TestContext {
        _temp_dir: TempDir,
        app_state: Arc<AppState>,
        database: Arc<Database>,
    }

    async fn upsert_typed_streamable_http_server(
        pool: &sqlx::SqlitePool,
        server: &Server,
    ) {
        server::upsert_server_definition(
            pool,
            server,
            &ServerTransportDraft::Http {
                protocol: HttpTransportKind::StreamableHttp,
                endpoint: server.url.clone(),
                headers: Default::default(),
            },
        )
        .await
        .expect("insert typed server definition");
    }

    #[tokio::test]
    async fn delete_server_records_continues_when_oauth_secret_cleanup_needs_secure_store() {
        let context = create_test_context().await;
        let server_id = "serv_delete_without_store";

        let mut server =
            Server::new_streamable_http("oauth server".to_string(), Some("https://example.com/mcp".to_string()));
        server.id = Some(server_id.to_string());
        upsert_typed_streamable_http_server(&context.database.pool, &server).await;

        server::upsert_server_oauth_config(
            &context.database.pool,
            &ServerOAuthConfig {
                id: None,
                server_id: server_id.to_string(),
                authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                token_endpoint: "https://issuer.example.com/token".to_string(),
                client_id: "client-1".to_string(),
                client_secret: Some(format!("[[secret:oauth/{server_id}/client-secret]]")),
                scopes: Some("read write".to_string()),
                redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("insert oauth config");

        server::upsert_server_oauth_token(
            &context.database.pool,
            &ServerOAuthToken {
                id: None,
                server_id: server_id.to_string(),
                access_token: format!("[[secret:oauth/{server_id}/access-token]]"),
                refresh_token: Some(format!("[[secret:oauth/{server_id}/refresh-token]]")),
                token_type: "bearer".to_string(),
                expires_at: None,
                scope: Some("read write".to_string()),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("insert oauth token");
        insert_test_oauth_secret_rows(&context.database.pool, server_id).await;

        delete_server_records(&context.app_state, context.database.as_ref(), server_id)
            .await
            .expect("delete server records");

        assert!(
            server::get_server_by_id(&context.database.pool, server_id)
                .await
                .expect("load server")
                .is_none()
        );
        assert!(
            server::get_server_oauth_config(&context.database.pool, server_id)
                .await
                .expect("load oauth config")
                .is_none()
        );
        assert!(
            server::get_server_oauth_token(&context.database.pool, server_id)
                .await
                .expect("load oauth token")
                .is_none()
        );
        let remaining_secret_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM secure_store_secrets WHERE alias LIKE ?")
                .bind(format!("oauth/{server_id}/%"))
                .fetch_one(&context.database.pool)
                .await
                .expect("count oauth secret rows");
        assert_eq!(remaining_secret_count, 0);
    }

    #[tokio::test]
    async fn update_server_with_manual_authorization_header_clears_oauth_auth_source() {
        let context = create_test_context().await;
        let server_id = "serv_manual_authorization";

        let mut server = Server::new_streamable_http(
            "manual auth server".to_string(),
            Some("https://example.com/mcp".to_string()),
        );
        server.id = Some(server_id.to_string());
        upsert_typed_streamable_http_server(&context.database.pool, &server).await;

        insert_plain_oauth_auth_source(&context.database.pool, server_id).await;

        let _response = update_server(
            State(context.app_state.clone()),
            Json(
                serde_json::from_value(serde_json::json!({
                    "id": server_id,
                    "transport": {
                        "kind": "http",
                        "protocol": "streamable_http",
                        "endpoint": "https://example.com/mcp",
                        "headers": {
                            "Authorization": {"kind": "literal", "value": "Bearer manual-token"},
                            "X-Trace": {"kind": "literal", "value": "trace-1"}
                        }
                    }
                }))
                .expect("decode update request"),
            ),
        )
        .await
        .expect("update server");

        assert!(
            server::get_server_oauth_config(&context.database.pool, server_id)
                .await
                .expect("load oauth config")
                .is_none()
        );
        assert!(
            server::get_server_oauth_token(&context.database.pool, server_id)
                .await
                .expect("load oauth token")
                .is_none()
        );

        let headers = server::get_server_headers(&context.database.pool, server_id)
            .await
            .expect("load headers");
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer manual-token")
        );
        assert_eq!(headers.get("x-trace").map(String::as_str), Some("trace-1"));
    }

    #[tokio::test]
    async fn update_server_preserves_redacted_http_header_config_values() {
        let context = create_test_context().await;

        for (server_id, existing_authorization) in [
            (
                "serv_preserve_literal_header",
                ConfigValue::Literal {
                    value: "Bearer existing-token".to_string(),
                },
            ),
            (
                "serv_preserve_secret_header",
                ConfigValue::SecretRef {
                    alias: "server/header-token".to_string(),
                },
            ),
        ] {
            let mut server = Server::new_streamable_http(
                format!("redacted header {server_id}"),
                Some("https://example.com/mcp".to_string()),
            );
            server.id = Some(server_id.to_string());
            let expected_authorization = existing_authorization.clone();
            server::upsert_server_definition(
                &context.database.pool,
                &server,
                &ServerTransportDraft::Http {
                    protocol: HttpTransportKind::StreamableHttp,
                    endpoint: Some("https://example.com/mcp".to_string()),
                    headers: BTreeMap::from([
                        ("Authorization".to_string(), existing_authorization),
                        (
                            "X-Removed".to_string(),
                            ConfigValue::Literal {
                                value: "removed-on-full-replacement".to_string(),
                            },
                        ),
                    ]),
                },
            )
            .await
            .expect("seed structured HTTP server");

            let _ = update_server(
                State(context.app_state.clone()),
                Json(
                    serde_json::from_value(serde_json::json!({
                        "id": server_id,
                        "transport": {
                            "kind": "http",
                            "protocol": "streamable_http",
                            "endpoint": "https://example.com/mcp",
                            "headers": {
                                "Authorization": {"kind": "literal", "value": "***REDACTED***"},
                                "X-Trace": {"kind": "literal", "value": "trace-1"}
                            }
                        }
                    }))
                    .expect("decode update request"),
                ),
            )
            .await
            .expect("update redacted HTTP headers");

            let headers = server::get_server_headers(&context.database.pool, server_id)
                .await
                .expect("load headers");
            assert_eq!(
                headers.get("authorization"),
                Some(&expected_authorization.runtime_value())
            );
            assert_eq!(headers.get("x-trace").map(String::as_str), Some("trace-1"));
            assert!(!headers.contains_key("x-removed"));
            assert!(!headers.values().any(|value| value == "***REDACTED***"));

            let Some(ServerTransportDraft::Http { headers, .. }) =
                server::get_server_transport_draft(&context.database.pool, server_id)
                    .await
                    .expect("load transport draft")
            else {
                panic!("updated server must retain an HTTP transport draft");
            };
            assert_eq!(headers.get("Authorization"), Some(&expected_authorization));
            assert_eq!(
                headers.get("X-Trace").map(|value| value.runtime_value()),
                Some("trace-1".to_string())
            );
            assert!(!headers.contains_key("X-Removed"));
        }
    }

    #[tokio::test]
    async fn update_server_preserves_redacted_stdio_env_config_values() {
        let context = create_test_context().await;

        for (server_id, existing_token) in [
            (
                "serv_preserve_literal_env",
                ConfigValue::Literal {
                    value: "existing-token".to_string(),
                },
            ),
            (
                "serv_preserve_secret_env",
                ConfigValue::SecretRef {
                    alias: "server/env-token".to_string(),
                },
            ),
        ] {
            let mut server = Server::new_stdio(format!("redacted env {server_id}"), Some("node".to_string()));
            server.id = Some(server_id.to_string());
            let expected_token = existing_token.clone();
            server::upsert_server_definition(
                &context.database.pool,
                &server,
                &ServerTransportDraft::Stdio {
                    command: Some("node".to_string()),
                    args: vec!["old.js".to_string()],
                    env: BTreeMap::from([
                        ("TOKEN".to_string(), existing_token),
                        (
                            "REMOVED".to_string(),
                            ConfigValue::Literal {
                                value: "removed-on-full-replacement".to_string(),
                            },
                        ),
                    ]),
                },
            )
            .await
            .expect("seed structured stdio server");

            let _ = update_server(
                State(context.app_state.clone()),
                Json(
                    serde_json::from_value(serde_json::json!({
                        "id": server_id,
                        "transport": {
                            "kind": "stdio",
                            "command": "node",
                            "args": ["new.js"],
                            "env": {
                                "TOKEN": {"kind": "literal", "value": "***REDACTED***"},
                                "TRACE": {"kind": "literal", "value": "trace-1"}
                            }
                        }
                    }))
                    .expect("decode update request"),
                ),
            )
            .await
            .expect("update redacted stdio env");

            let env = server::get_server_env(&context.database.pool, server_id)
                .await
                .expect("load environment");
            assert_eq!(env.get("TOKEN"), Some(&expected_token.runtime_value()));
            assert_eq!(env.get("TRACE").map(String::as_str), Some("trace-1"));
            assert!(!env.contains_key("REMOVED"));
            assert!(!env.values().any(|value| value == "***REDACTED***"));

            let Some(ServerTransportDraft::Stdio { env, .. }) =
                server::get_server_transport_draft(&context.database.pool, server_id)
                    .await
                    .expect("load transport draft")
            else {
                panic!("updated server must retain a stdio transport draft");
            };
            assert_eq!(env.get("TOKEN"), Some(&expected_token));
            assert_eq!(
                env.get("TRACE").map(|value| value.runtime_value()),
                Some("trace-1".to_string())
            );
            assert!(!env.contains_key("REMOVED"));
        }
    }

    #[tokio::test]
    async fn update_server_repairs_invalid_transport_without_a_previous_fingerprint() {
        let context = create_test_context().await;
        let server_id = "serv_repair_invalid_transport";
        let mut server = Server::new_streamable_http(
            "repair invalid transport".to_string(),
            Some("http://127.0.0.1:9/previous".to_string()),
        );
        server.id = Some(server_id.to_string());
        upsert_typed_streamable_http_server(&context.database.pool, &server).await;
        sqlx::query("UPDATE server_transport SET draft_json = ? WHERE server_id = ?")
            .bind(r#"{"kind":"stdio","command":null,"args":[],"env":{}}"#)
            .bind(server_id)
            .execute(&context.database.pool)
            .await
            .expect("seed invalid historical transport");

        let response = update_server(
            State(context.app_state.clone()),
            Json(
                serde_json::from_value(serde_json::json!({
                    "id": server_id,
                    "transport": {
                        "kind": "http",
                        "protocol": "streamable_http",
                        "endpoint": "http://127.0.0.1:9/repaired",
                        "headers": {}
                    }
                }))
                .expect("decode repair request"),
            ),
        )
        .await
        .expect("a valid definition repairs an invalid historical transport");

        assert!(response.0.success);
        let Some(ServerTransportDraft::Http { endpoint, .. }) =
            server::get_server_transport_draft(&context.database.pool, server_id)
                .await
                .expect("load repaired draft")
        else {
            panic!("repair must persist an HTTP transport draft");
        };
        assert_eq!(endpoint.as_deref(), Some("http://127.0.0.1:9/repaired"));
        assert!(
            crate::config::server::capabilities::current_config_fingerprint(&context.database.pool, server_id)
                .await
                .is_ok(),
            "the repaired transport must establish a valid capability baseline"
        );
    }

    #[tokio::test]
    async fn update_server_reports_failed_capability_discovery_after_persisting_configuration() {
        let context = create_test_context().await;
        let server_id = "serv_discovery_failure";
        let mut server = Server::new_streamable_http(
            "discovery failure server".to_string(),
            Some("http://127.0.0.1:9/previous".to_string()),
        );
        server.id = Some(server_id.to_string());
        upsert_typed_streamable_http_server(&context.database.pool, &server).await;

        let response = update_server(
            State(context.app_state.clone()),
            Json(
                serde_json::from_value(serde_json::json!({
                    "id": server_id,
                    "transport": {
                        "kind": "http",
                        "protocol": "streamable_http",
                        "endpoint": "http://127.0.0.1:9/updated",
                        "headers": {}
                    }
                }))
                .expect("decode update request"),
            ),
        )
        .await
        .expect("discovery failure must not fail a persisted update");

        assert!(response.0.success, "persisted update must return success");
        let persisted = server::get_server_by_id(&context.database.pool, server_id)
            .await
            .expect("load updated server")
            .expect("updated server exists");
        assert_eq!(persisted.url.as_deref(), Some("http://127.0.0.1:9/updated"));

        let response_json = serde_json::to_value(response.0).expect("serialize update response");
        let discovery = response_json
            .pointer("/data/capability_discovery")
            .expect("update response must report attempted discovery");
        assert_eq!(discovery["attempted"], true);
        assert_eq!(discovery["status"], "failed");
        let error = discovery["error"].as_str().expect("safe failure message");
        assert_eq!(
            error,
            "Capability discovery failed. Check the server configuration and upstream availability."
        );
        assert!(!error.contains("127.0.0.1"), "must not reveal transport configuration");
    }

    #[tokio::test]
    async fn create_server_reports_failed_capability_discovery_after_persisting_configuration() {
        let context = create_test_context().await;

        let response = create_server(
            State(context.app_state.clone()),
            Json(
                serde_json::from_value(serde_json::json!({
                    "name": "discovery_failure_create",
                    "transport": {
                        "kind": "http",
                        "protocol": "streamable_http",
                        "endpoint": "http://127.0.0.1:9/mcp",
                        "headers": {}
                    }
                }))
                .expect("decode create request"),
            ),
        )
        .await
        .expect("discovery failure must not fail a persisted create");

        assert!(response.0.success, "persisted create must return success");
        let created = server::get_server(&context.database.pool, "discovery_failure_create")
            .await
            .expect("load created server")
            .expect("created server exists");
        assert_eq!(created.url.as_deref(), Some("http://127.0.0.1:9/mcp"));

        let response_json = serde_json::to_value(response.0).expect("serialize create response");
        let discovery = response_json
            .pointer("/data/capability_discovery")
            .expect("create response must report attempted discovery");
        assert_eq!(discovery["attempted"], true);
        assert_eq!(discovery["status"], "failed");
        let error = discovery["error"].as_str().expect("safe failure message");
        assert_eq!(
            error,
            "Capability discovery failed. Check the server configuration and upstream availability."
        );
        assert!(!error.contains("127.0.0.1"), "must not reveal transport configuration");
    }

    #[tokio::test]
    async fn create_server_returns_transport_diagnostics_without_persisting() {
        let context = create_test_context().await;
        let request = serde_json::from_value(serde_json::json!({
            "name": "invalid_transport",
            "transport": {
                "kind": "stdio",
                "args": [],
                "env": {}
            }
        }))
        .expect("decode tagged create request");

        let error = create_server(State(context.app_state.clone()), Json(request))
            .await
            .expect_err("invalid transport must fail before persistence");

        assert!(matches!(error, ApiError::BadRequest(message) if message.contains("stdio_command_missing (command)")));
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_config")
            .fetch_one(&context.database.pool)
            .await
            .expect("count servers");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn create_server_rejects_non_canonical_namespace_with_suggestion() {
        let context = create_test_context().await;

        let error = create_server(
            State(context.app_state.clone()),
            Json(
                serde_json::from_value(serde_json::json!({
                    "name": "Sequential Thinking-v2",
                    "transport": {
                        "kind": "http",
                        "protocol": "streamable_http",
                        "endpoint": "https://example.com/mcp",
                        "headers": {}
                    }
                }))
                .expect("decode create request"),
            ),
        )
        .await
        .expect_err("non-canonical namespace must fail");

        match error {
            ApiError::BadRequest(message) => {
                assert!(message.contains("Suggested namespace: 'sequential_thinking_v2'"));
            }
            other => panic!("expected bad request, got {other:?}"),
        }
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_config")
            .fetch_one(&context.database.pool)
            .await
            .expect("count servers");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn oauth_pending_import_rejects_namespace_before_creating_record() {
        let context = create_test_context().await;

        let error = create_server(
            State(context.app_state.clone()),
            Json(
                serde_json::from_value(serde_json::json!({
                    "name": "OAuth Pending Server",
                    "transport": {
                        "kind": "http",
                        "protocol": "streamable_http",
                        "endpoint": "https://example.com/mcp",
                        "headers": {}
                    },
                    "pending_import": true
                }))
                .expect("decode pending import request"),
            ),
        )
        .await
        .expect_err("pending OAuth record requires a confirmed namespace");

        assert!(matches!(error, ApiError::BadRequest(_)));
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_config")
            .fetch_one(&context.database.pool)
            .await
            .expect("count servers");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn create_server_rejects_namespace_conflicts_without_renaming() {
        let context = create_test_context().await;
        let mut existing = Server::new_streamable_http(
            "stable_namespace".to_string(),
            Some("https://example.com/existing".to_string()),
        );
        existing.id = Some("server-existing".to_string());
        upsert_typed_streamable_http_server(&context.database.pool, &existing).await;

        let error = create_server(
            State(context.app_state.clone()),
            Json(
                serde_json::from_value(serde_json::json!({
                    "name": "stable_namespace",
                    "transport": {
                        "kind": "http",
                        "protocol": "streamable_http",
                        "endpoint": "https://example.com/new",
                        "headers": {}
                    }
                }))
                .expect("decode create request"),
            ),
        )
        .await
        .expect_err("duplicate namespace must fail");

        assert!(matches!(error, ApiError::Conflict(_)));
        let names = sqlx::query_scalar::<_, String>("SELECT name FROM server_config ORDER BY name")
            .fetch_all(&context.database.pool)
            .await
            .expect("load namespaces");
        assert_eq!(names, vec!["stable_namespace"]);
    }

    #[tokio::test]
    async fn namespace_remediation_is_available_only_for_invalid_legacy_servers() {
        let context = create_test_context().await;
        let fixture = context._temp_dir.path().join("namespace_remediation_fixture.py");
        std::fs::write(
            &fixture,
            format!(
                r#"
import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    request_id = request.get("id")
    if request_id is None:
        continue
    method = request.get("method")
    if method == "initialize":
        result = {{
            "protocolVersion": "{}",
            "capabilities": {{"tools": {{}}}},
            "serverInfo": {{"name": "namespace-remediation-fixture", "version": "1.0.0"}}
        }}
    elif method == "tools/list":
        result = {{"tools": []}}
    else:
        continue
    sys.stdout.write(json.dumps({{"jsonrpc": "2.0", "id": request_id, "result": result}}) + "\n")
    sys.stdout.flush()
"#,
                crate::common::constants::protocol::CURRENT_VERSION
            ),
        )
        .expect("write namespace remediation fixture");
        let mut legacy = Server::new_stdio("Sequential Thinking".to_string(), Some("python3".to_string()));
        legacy.id = Some("server-legacy".to_string());
        server::upsert_server_definition(
            &context.database.pool,
            &legacy,
            &ServerTransportDraft::Stdio {
                command: Some("python3".to_string()),
                args: vec![fixture.to_string_lossy().to_string()],
                env: Default::default(),
            },
        )
        .await
        .expect("insert typed legacy namespace fixture");

        let response = remediate_server_namespace(
            State(context.app_state.clone()),
            Json(ServerNamespaceRemediationReq {
                id: "server-legacy".to_string(),
                namespace: "sequential_reasoning".to_string(),
            }),
        )
        .await
        .expect("remediate legacy namespace");
        assert_eq!(response.0.data.expect("operation data").name, "sequential_reasoning");

        let error = remediate_server_namespace(
            State(context.app_state.clone()),
            Json(ServerNamespaceRemediationReq {
                id: "server-legacy".to_string(),
                namespace: "another_namespace".to_string(),
            }),
        )
        .await
        .expect_err("ordinary rename must remain unavailable");
        assert!(matches!(error, ApiError::Conflict(_)));
    }

    #[tokio::test]
    async fn namespace_remediation_connection_failure_does_not_commit_changes() {
        let context = create_test_context().await;
        let mut legacy =
            Server::new_streamable_http("Legacy Server".to_string(), Some("http://127.0.0.1:9/mcp".to_string()));
        legacy.id = Some("server-unreachable".to_string());
        server::upsert_server(&context.database.pool, &legacy)
            .await
            .expect("insert unreachable legacy server");

        remediate_server_namespace(
            State(context.app_state.clone()),
            Json(ServerNamespaceRemediationReq {
                id: "server-unreachable".to_string(),
                namespace: "legacy_server".to_string(),
            }),
        )
        .await
        .expect_err("upstream discovery failure must abort remediation");

        let namespace: String = sqlx::query_scalar("SELECT name FROM server_config WHERE id = 'server-unreachable'")
            .fetch_one(&context.database.pool)
            .await
            .expect("load unchanged namespace");
        assert_eq!(namespace, "Legacy Server");
    }

    #[tokio::test]
    async fn create_server_with_manual_authorization_header_persists_header() {
        let context = create_test_context().await;

        let response = create_server(
            State(context.app_state.clone()),
            Json(
                serde_json::from_value(serde_json::json!({
                    "name": "manual_auth_create_server",
                    "transport": {
                        "kind": "http",
                        "protocol": "streamable_http",
                        "endpoint": "https://example.com/mcp",
                        "headers": {
                            "Authorization": {"kind": "literal", "value": "Bearer manual-token"},
                            "X-Trace": {"kind": "literal", "value": "trace-1"}
                        }
                    }
                }))
                .expect("decode create request"),
            ),
        )
        .await
        .expect("create server");
        let created = response.0.data.expect("created server data");
        let server_id = created.id.expect("created server id");

        let headers = server::get_server_headers(&context.database.pool, &server_id)
            .await
            .expect("load headers");
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer manual-token")
        );
        assert_eq!(headers.get("x-trace").map(String::as_str), Some("trace-1"));
    }

    #[tokio::test]
    async fn manual_authorization_cleanup_ignores_blank_authorization_and_proxy_authorization() {
        let context = create_test_context().await;
        let server_id = "serv_non_manual_authorization";

        let mut server = Server::new_streamable_http(
            "non manual auth server".to_string(),
            Some("https://example.com/mcp".to_string()),
        );
        server.id = Some(server_id.to_string());
        upsert_typed_streamable_http_server(&context.database.pool, &server).await;
        insert_plain_oauth_auth_source(&context.database.pool, server_id).await;

        clear_oauth_auth_source_for_manual_authorization(
            &context.app_state,
            &context.database,
            server_id,
            &HashMap::from([
                ("Authorization".to_string(), "   ".to_string()),
                ("Proxy-Authorization".to_string(), "Bearer proxy-token".to_string()),
            ]),
        )
        .await
        .expect("manual auth cleanup");

        assert!(
            server::get_server_oauth_config(&context.database.pool, server_id)
                .await
                .expect("load oauth config")
                .is_some()
        );
        assert!(
            server::get_server_oauth_token(&context.database.pool, server_id)
                .await
                .expect("load oauth token")
                .is_some()
        );
    }

    async fn insert_plain_oauth_auth_source(
        pool: &sqlx::SqlitePool,
        server_id: &str,
    ) {
        server::upsert_server_oauth_config(
            pool,
            &ServerOAuthConfig {
                id: None,
                server_id: server_id.to_string(),
                authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                token_endpoint: "https://issuer.example.com/token".to_string(),
                client_id: "client-1".to_string(),
                client_secret: None,
                scopes: Some("read write".to_string()),
                redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("insert oauth config");
        server::upsert_server_oauth_token(
            pool,
            &ServerOAuthToken {
                id: None,
                server_id: server_id.to_string(),
                access_token: "oauth-access-token".to_string(),
                refresh_token: Some("oauth-refresh-token".to_string()),
                token_type: "bearer".to_string(),
                expires_at: None,
                scope: Some("read write".to_string()),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("insert oauth token");
    }

    async fn insert_test_oauth_secret_rows(
        pool: &sqlx::SqlitePool,
        server_id: &str,
    ) {
        for (alias, kind) in [
            (format!("oauth/{server_id}/client-secret"), "oauth_client_secret"),
            (format!("oauth/{server_id}/access-token"), "oauth_access_token"),
            (format!("oauth/{server_id}/refresh-token"), "oauth_refresh_token"),
        ] {
            sqlx::query(
                r#"
                INSERT INTO secure_store_secrets (
                    alias, kind, provider_id, provider_kind, version,
                    key_nonce, encrypted_key, nonce, encrypted_value
                )
                VALUES (?, ?, 'test-provider', 'test', 1, 'key-nonce', 'encrypted-key', 'nonce', 'encrypted-value')
                "#,
            )
            .bind(alias)
            .bind(kind)
            .execute(pool)
            .await
            .expect("insert oauth secret row");
        }
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
        crate::config::initialization::run_initialization(&db_pool)
            .await
            .expect("initialize database");
        LocalSecretStore::initialize_with_development_root_key(
            db_pool.clone(),
            temp_dir.path().join("secrets").join("local-root.key"),
        )
        .await
        .expect("init secret store tables");

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
            database: Some(database.clone()),
            audit_database: None,
            audit_service: None,
            config_application_state: Arc::new(ConfigApplicationStateManager::new()),
            client_service: None,
            inspector_calls: Arc::new(InspectorCallRegistry::new()),
            inspector_sessions: Arc::new(InspectorSessionManager::new()),
            oauth_manager: RwLock::new(Some(Arc::new(crate::core::oauth::OAuthManager::new(db_pool)))),
            secret_store: RwLock::new(None),
            secret_store_readiness: RwLock::new(crate::api::routes::unavailable_secret_store_readiness(
                "test_unavailable",
            )),
        });

        TestContext {
            _temp_dir: temp_dir,
            app_state,
            database,
        }
    }
}
