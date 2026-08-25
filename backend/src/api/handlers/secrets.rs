use std::sync::Arc;

use axum::{
    Json,
    extract::{Query, State},
};

use crate::{
    api::{
        handlers::ApiError,
        models::secrets::{
            PassphraseRotateReq, ProviderModePayload, ProviderSwitchData, ProviderSwitchReq, ProviderSwitchResp,
            SecretCreateReq, SecretDeleteData, SecretDeleteReq, SecretDeleteResp, SecretDetailsReq, SecretKindPayload,
            SecretListData, SecretListResp, SecretMetadataData, SecretMetadataResp, SecretOriginData,
            SecretStoreIssueData, SecretStoreProviderData, SecretStoreStatusData, SecretStoreStatusResp,
            SecretStoreUnlockReq, SecretUpdateReq, SecretUsageData, SecretUsageListData, SecretUsageListResp,
            SecretUsageLocationData, SecretUsageReq,
        },
        routes::AppState,
    },
    core::secrets::store::{
        SecretCreateInput, SecretKindInput, SecretMetadataView, SecretOriginInput, SecretStoreReadiness,
        SecretUpdateInput, SecretUsageLocationInput, SecretUsageView,
    },
    core::secrets::{
        discover_active_secret_usages, discover_active_secret_usages_for_alias, preload_mcp_server_configs,
        resolve_secret_usage_status_from_cache,
    },
};
use mcpmate_secrets::{RootKeyProviderMetadata, SecretRootKeyError, SecretStoreDeleteError};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};

const PROVIDER_SWITCH_CONFIRMATION_PHRASE: &str = "ROTATE SECRETS";

pub async fn get_secret_store_status(
    State(state): State<Arc<AppState>>
) -> Result<Json<SecretStoreStatusResp>, ApiError> {
    let readiness = state
        .secret_store_readiness
        .try_read()
        .map(|guard| secret_store_status_data(&guard))
        .unwrap_or_else(|_| SecretStoreStatusData {
            status: "unavailable".to_string(),
            provider: None,
            issue: Some(SecretStoreIssueData {
                reason_code: "read_lock_failed".to_string(),
                message: "Could not read store status".to_string(),
            }),
        });
    Ok(Json(SecretStoreStatusResp::success(readiness)))
}

pub async fn list_secrets(State(state): State<Arc<AppState>>) -> Result<Json<SecretListResp>, ApiError> {
    let store = get_secret_store(&state)?;
    let db = crate::api::handlers::server::common::get_database_from_state(&state)?;

    let secrets = store
        .list_secret_metadata()
        .await
        .map_err(|err| ApiError::InternalError(err.to_string()))?;

    // Count active bindings from persisted server configs and OAuth-owned refs.
    let discovered = discover_active_secret_usages(&db.pool)
        .await
        .map_err(crate::api::handlers::common::errors::map_anyhow_error)?;
    let mut active_count_by_alias: HashMap<String, u64> = HashMap::new();
    let mut active_binding_keys_by_alias: HashMap<String, HashSet<String>> = HashMap::new();
    for usage in discovered {
        let binding_key = usage.location.binding_key(&usage.server_id);
        *active_count_by_alias.entry(usage.alias.clone()).or_insert(0) += 1;
        active_binding_keys_by_alias
            .entry(usage.alias)
            .or_default()
            .insert(binding_key);
    }

    let (all_indexed, mut unknown_count_by_alias) = store
        .list_all_usages_with_unsupported_counts()
        .await
        .map_err(map_secret_store_error)?;
    let mut indexed_by_alias: HashMap<String, Vec<SecretUsageView>> = HashMap::new();
    for usage in all_indexed {
        indexed_by_alias.entry(usage.alias.clone()).or_default().push(usage);
    }

    let indexed_server_ids: Vec<String> = indexed_by_alias
        .values()
        .flat_map(|usages| usages.iter().map(|usage| usage.server_id.clone()))
        .collect();
    let server_config_cache = preload_mcp_server_configs(&db.pool, indexed_server_ids)
        .await
        .map_err(crate::api::handlers::common::errors::map_anyhow_error)?;

    let mut enriched = Vec::with_capacity(secrets.len());
    for metadata in secrets {
        let mut data = secret_metadata_data(metadata);
        data.used_by_count = active_count_by_alias.remove(&data.alias).unwrap_or(0);
        data.unknown_usage_count = unknown_count_by_alias.remove(&data.alias).unwrap_or(0);
        let active_binding_keys = active_binding_keys_by_alias.remove(&data.alias).unwrap_or_default();
        let indexed = indexed_by_alias.remove(&data.alias).unwrap_or_default();
        let mut historical_usage_count = 0;
        for usage in indexed {
            let key = usage.location.binding_key(&usage.server_id);
            if active_binding_keys.contains(&key) {
                continue;
            }
            if resolve_secret_usage_status_from_cache(&usage, &server_config_cache)
                .map_err(crate::api::handlers::common::errors::map_anyhow_error)?
                == "stale"
            {
                historical_usage_count += 1;
            }
        }
        data.historical_usage_count = historical_usage_count;
        enriched.push(data);
    }
    Ok(Json(SecretListResp::success(SecretListData { secrets: enriched })))
}

pub async fn create_secret(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<SecretCreateReq>,
) -> Result<Json<SecretMetadataResp>, ApiError> {
    let store_guard = state.secret_store.read().await;
    let store = get_secret_store_from_guard(&store_guard)?;
    if !is_user_creatable_secret_kind(&payload.kind) {
        return Err(ApiError::BadRequest(
            "OAuth secret kinds are managed by the OAuth flow and cannot be created manually".to_string(),
        ));
    }
    let metadata = store
        .create_secret(SecretCreateInput {
            alias: payload.alias,
            kind: secret_kind_input(payload.kind),
            value: payload.value,
            label: payload.label,
            origin: payload.origin.map(secret_origin_input),
        })
        .await
        .map_err(map_secret_store_error)?;
    Ok(Json(SecretMetadataResp::success(
        secret_metadata_data_with_unknown_count(metadata, 0),
    )))
}

pub async fn update_secret(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<SecretUpdateReq>,
) -> Result<Json<SecretMetadataResp>, ApiError> {
    let store_guard = state.secret_store.read().await;
    let store = get_secret_store_from_guard(&store_guard)?;
    let existing = store
        .get_secret_metadata(&payload.alias)
        .await
        .map_err(map_secret_store_error)?;
    if payload.value.is_some() && is_oauth_secret_kind(&existing.kind) {
        return Err(ApiError::BadRequest(
            "OAuth secret values are managed by the OAuth flow; reconnect or revoke OAuth instead".to_string(),
        ));
    }
    let metadata = store
        .update_secret(SecretUpdateInput {
            alias: payload.alias,
            kind: payload.kind.map(secret_kind_input),
            value: payload.value,
            label: payload.label,
            origin: payload.origin.map(secret_origin_input),
        })
        .await
        .map_err(map_secret_store_error)?;
    Ok(Json(SecretMetadataResp::success(
        secret_metadata_data_with_usage_state(&state, &store, metadata).await?,
    )))
}

pub async fn get_secret_details(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SecretDetailsReq>,
) -> Result<Json<SecretMetadataResp>, ApiError> {
    let store = get_secret_store(&state)?;
    let metadata = store
        .get_secret_metadata(&query.alias)
        .await
        .map_err(map_secret_store_error)?;
    Ok(Json(SecretMetadataResp::success(
        secret_metadata_data_with_usage_state(&state, &store, metadata).await?,
    )))
}

pub async fn delete_secret(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<SecretDeleteReq>,
) -> Result<Json<SecretDeleteResp>, ApiError> {
    let store_guard = state.secret_store.read().await;
    let store = get_secret_store_from_guard(&store_guard)?;

    // When not force-deleting, check if any usages are still active
    // (i.e. a server-owned config or OAuth record still references the secret).
    // Stale usages (server removed or owner record changed) should not block deletion.
    if !payload.force {
        let db = crate::api::handlers::server::common::get_database_from_state(&state)?;
        let active_usages = discover_active_secret_usages_for_alias(&db.pool, &payload.alias)
            .await
            .map_err(crate::api::handlers::common::errors::map_anyhow_error)?;
        if let Some(usage) = active_usages.first() {
            return Err(ApiError::Conflict(format!(
                "Secret '{}' is actively used by server '{}' and cannot be deleted",
                payload.alias, usage.server_id
            )));
        }
        let unknown_usage_count = secret_unknown_usage_count(&store, &payload.alias).await?;
        if unknown_usage_count > 0 {
            return Err(ApiError::Conflict(format!(
                "Secret '{}' has {} unsupported usage reference(s) and cannot be deleted without force",
                payload.alias, unknown_usage_count
            )));
        }
    }
    store
        .delete_secret(&payload.alias, true)
        .await
        .map_err(map_secret_store_delete_error)?;
    Ok(Json(SecretDeleteResp::success(SecretDeleteData {
        alias: payload.alias,
        deleted: true,
    })))
}

pub async fn list_secret_usages(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SecretUsageReq>,
) -> Result<Json<SecretUsageListResp>, ApiError> {
    let store = get_secret_store(&state)?;
    let db = crate::api::handlers::server::common::get_database_from_state(&state)?;

    let discovered = discover_active_secret_usages_for_alias(&db.pool, &query.alias)
        .await
        .map_err(crate::api::handlers::common::errors::map_anyhow_error)?;
    let mut active_binding_keys: HashSet<String> = HashSet::with_capacity(discovered.len());
    let mut enriched = Vec::with_capacity(discovered.len());
    for usage in discovered {
        active_binding_keys.insert(usage.location.binding_key(&usage.server_id));
        enriched.push(secret_usage_data(usage, "active".to_string()));
    }

    let indexed = store.list_usages(&query.alias).await.map_err(map_secret_store_error)?;
    let indexed_server_ids: Vec<String> = indexed.iter().map(|usage| usage.server_id.clone()).collect();
    let server_config_cache = preload_mcp_server_configs(&db.pool, indexed_server_ids)
        .await
        .map_err(crate::api::handlers::common::errors::map_anyhow_error)?;
    for usage in indexed {
        let key = usage.location.binding_key(&usage.server_id);
        if active_binding_keys.contains(&key) {
            continue;
        }
        let status = resolve_secret_usage_status_from_cache(&usage, &server_config_cache)
            .map_err(crate::api::handlers::common::errors::map_anyhow_error)?;
        if status == "stale" {
            enriched.push(secret_usage_data(usage, status.to_string()));
        }
    }

    Ok(Json(SecretUsageListResp::success(SecretUsageListData {
        usages: enriched,
    })))
}

fn get_secret_store(state: &Arc<AppState>) -> Result<Arc<crate::core::secrets::store::LocalSecretStore>, ApiError> {
    // Try read-lock first (non-blocking). If poisoned or empty, return error.
    state
        .secret_store
        .try_read()
        .ok()
        .and_then(|guard| guard.clone())
        .ok_or_else(|| {
            ApiError::ServiceUnavailable(
                "Secret store is unavailable. Unlock or configure the operating-system secure storage provider."
                    .to_string(),
            )
        })
}

fn get_secret_store_from_guard(
    guard: &Option<Arc<crate::core::secrets::store::LocalSecretStore>>
) -> Result<Arc<crate::core::secrets::store::LocalSecretStore>, ApiError> {
    guard.clone().ok_or_else(secret_store_unavailable_error)
}

fn secret_store_unavailable_error() -> ApiError {
    ApiError::ServiceUnavailable(
        "Secret store is unavailable. Unlock or configure the operating-system secure storage provider.".to_string(),
    )
}

fn map_secret_store_error(error: anyhow::Error) -> ApiError {
    let message = error.to_string();
    if message.contains("was not found") {
        ApiError::NotFound(message)
    } else if message.contains("in use") {
        ApiError::Conflict(message)
    } else {
        ApiError::BadRequest(message)
    }
}

fn map_secret_store_delete_error(error: SecretStoreDeleteError) -> ApiError {
    let message = error.to_string();
    match error {
        SecretStoreDeleteError::NotFound { .. } => ApiError::NotFound(message),
        SecretStoreDeleteError::InUse { .. } | SecretStoreDeleteError::UnsupportedUsage { .. } => {
            ApiError::Conflict(message)
        }
        SecretStoreDeleteError::Store(_) => ApiError::BadRequest(message),
    }
}

fn secret_kind_input(kind: SecretKindPayload) -> SecretKindInput {
    match kind {
        SecretKindPayload::Generic => SecretKindInput::Generic,
        SecretKindPayload::Token => SecretKindInput::Token,
        SecretKindPayload::ApiKey => SecretKindInput::ApiKey,
        SecretKindPayload::Password => SecretKindInput::Password,
        SecretKindPayload::OAuthClientSecret => SecretKindInput::OAuthClientSecret,
        SecretKindPayload::OAuthAccessToken => SecretKindInput::OAuthAccessToken,
        SecretKindPayload::OAuthRefreshToken => SecretKindInput::OAuthRefreshToken,
        SecretKindPayload::UrlCredential => SecretKindInput::UrlCredential,
        SecretKindPayload::HeaderValue => SecretKindInput::HeaderValue,
    }
}

fn is_user_creatable_secret_kind(kind: &SecretKindPayload) -> bool {
    !matches!(
        kind,
        SecretKindPayload::OAuthClientSecret
            | SecretKindPayload::OAuthAccessToken
            | SecretKindPayload::OAuthRefreshToken
    )
}

fn is_oauth_secret_kind(kind: &str) -> bool {
    matches!(
        kind,
        "oauth_client_secret" | "oauth_access_token" | "oauth_refresh_token"
    )
}

fn secret_metadata_data(metadata: SecretMetadataView) -> SecretMetadataData {
    secret_metadata_data_with_unknown_count(metadata, 0)
}

fn secret_metadata_data_with_unknown_count(
    metadata: SecretMetadataView,
    unknown_usage_count: u64,
) -> SecretMetadataData {
    SecretMetadataData {
        alias: metadata.alias,
        placeholder: metadata.placeholder,
        kind: metadata.kind,
        label: metadata.label,
        origin: metadata.origin.map(secret_origin_data),
        provider_id: metadata.provider_id,
        provider_kind: metadata.provider_kind,
        version: metadata.version,
        used_by_count: metadata.used_by_count,
        historical_usage_count: 0,
        unknown_usage_count,
        created_at: metadata.created_at,
        updated_at: metadata.updated_at,
    }
}

async fn secret_unknown_usage_count(
    store: &crate::core::secrets::store::LocalSecretStore,
    alias: &str,
) -> Result<u64, ApiError> {
    store
        .count_unsupported_usages_for_alias(alias)
        .await
        .map_err(map_secret_store_error)
}

async fn secret_metadata_data_with_usage_state(
    state: &Arc<AppState>,
    store: &crate::core::secrets::store::LocalSecretStore,
    metadata: SecretMetadataView,
) -> Result<SecretMetadataData, ApiError> {
    let db = crate::api::handlers::server::common::get_database_from_state(state)?;
    let alias = metadata.alias.clone();
    let mut data = secret_metadata_data(metadata);
    data.used_by_count = discover_active_secret_usages_for_alias(&db.pool, &alias)
        .await
        .map_err(crate::api::handlers::common::errors::map_anyhow_error)?
        .len() as u64;
    data.unknown_usage_count = secret_unknown_usage_count(store, &alias).await?;
    Ok(data)
}

fn secret_origin_input(origin: SecretOriginData) -> SecretOriginInput {
    SecretOriginInput {
        server_id: origin.server_id,
        server_name: origin.server_name,
        server_kind: origin.server_kind,
        source: origin.source,
        field_group: origin.field_group,
        field_key: origin.field_key,
        field_index: origin.field_index,
        field_path: origin.field_path,
    }
}

fn secret_origin_data(origin: SecretOriginInput) -> SecretOriginData {
    SecretOriginData {
        server_id: origin.server_id,
        server_name: origin.server_name,
        server_kind: origin.server_kind,
        source: origin.source,
        field_group: origin.field_group,
        field_key: origin.field_key,
        field_index: origin.field_index,
        field_path: origin.field_path,
    }
}

fn secret_usage_data(
    usage: SecretUsageView,
    status: String,
) -> SecretUsageData {
    SecretUsageData {
        alias: usage.alias,
        server_id: usage.server_id,
        location: secret_usage_location_data(usage.location),
        status,
    }
}

fn secret_store_provider_data(
    snapshot: &crate::core::secrets::store::SecretStoreProviderSnapshot
) -> SecretStoreProviderData {
    SecretStoreProviderData {
        provider_id: snapshot.provider_id.clone(),
        provider_kind: snapshot.provider_kind.clone(),
        provider_mode: snapshot.provider_mode.clone(),
        security_level: snapshot.security_level.clone(),
    }
}

fn secret_store_status_data(readiness: &SecretStoreReadiness) -> SecretStoreStatusData {
    match readiness {
        SecretStoreReadiness::Ready {
            provider_id,
            provider_kind,
            provider_mode,
            security_level,
        } => SecretStoreStatusData {
            status: "ready".to_string(),
            provider: Some(SecretStoreProviderData {
                provider_id: provider_id.clone(),
                provider_kind: provider_kind.clone(),
                provider_mode: provider_mode.clone(),
                security_level: security_level.clone(),
            }),
            issue: None,
        },
        SecretStoreReadiness::Unavailable {
            reason_code,
            message,
            provider,
        } => SecretStoreStatusData {
            status: "unavailable".to_string(),
            provider: provider.as_ref().map(secret_store_provider_data),
            issue: Some(SecretStoreIssueData {
                reason_code: reason_code.clone(),
                message: message.clone(),
            }),
        },
    }
}

fn secret_store_readiness_from_root_key_error(
    error: &SecretRootKeyError,
    metadata: RootKeyProviderMetadata,
) -> SecretStoreReadiness {
    let reason_code = match error {
        SecretRootKeyError::ProviderUnavailable(_) => "provider_unavailable",
        SecretRootKeyError::MissingMaterial(_) => "missing_root_key",
        SecretRootKeyError::InvalidMaterial(_) => "invalid_root_key",
        SecretRootKeyError::LocalStorage(_) => "local_storage_error",
        SecretRootKeyError::DevelopmentStorage(_) => "development_storage_error",
    };
    SecretStoreReadiness::unavailable_with_provider(reason_code, error.to_string(), metadata)
}

async fn update_secret_store_readiness(
    state: &Arc<AppState>,
    readiness: SecretStoreReadiness,
) {
    let mut readiness_guard = state.secret_store_readiness.write().await;
    *readiness_guard = readiness;
}

fn api_error_from_secret_root_key_error(error: SecretRootKeyError) -> ApiError {
    match error {
        SecretRootKeyError::InvalidMaterial(message) => {
            ApiError::BadRequest(format!("Invalid current passphrase: {message}"))
        }
        SecretRootKeyError::MissingMaterial(message) => {
            ApiError::ServiceUnavailable(format!("Current root key material is missing: {message}"))
        }
        other => ApiError::InternalError(format!("Failed to load root key: {other}")),
    }
}

fn provider_for_mode(
    mode: crate::core::secrets::store::RootKeyProviderMode,
    passphrase_path: &std::path::Path,
    local_file_path: &std::path::Path,
    passphrase: &str,
) -> Result<Arc<dyn crate::core::secrets::store::SecretRootKeyProvider>, ApiError> {
    match mode {
        crate::core::secrets::store::RootKeyProviderMode::OperatingSystem => Ok(Arc::new(
            crate::core::secrets::store::OperatingSystemRootKeyProvider::new(),
        )),
        crate::core::secrets::store::RootKeyProviderMode::Passphrase => {
            Ok(Arc::new(crate::core::secrets::store::PassphraseRootKeyProvider::new(
                passphrase_path.to_path_buf(),
                passphrase.to_string(),
            )))
        }
        crate::core::secrets::store::RootKeyProviderMode::LocalFile => Ok(Arc::new(
            crate::core::secrets::store::LocalFileRootKeyProvider::new(local_file_path.to_path_buf()),
        )),
        _ => Err(ApiError::BadRequest("Provider mode is not supported".to_string())),
    }
}

async fn map_secret_store_rotation_error(
    state: &Arc<AppState>,
    error: mcpmate_secrets::SecretStoreRotationError,
    current_provider_metadata: RootKeyProviderMetadata,
) -> ApiError {
    match error {
        mcpmate_secrets::SecretStoreRotationError::CurrentProviderUnavailable(error) => {
            if matches!(
                current_provider_metadata.mode(),
                crate::core::secrets::store::RootKeyProviderMode::OperatingSystem
            ) {
                let readiness = secret_store_readiness_from_root_key_error(&error, current_provider_metadata);
                update_secret_store_readiness(state, readiness).await;
                return ApiError::ServiceUnavailable(
                    concat!(
                        "OS secure storage is unavailable and existing encrypted secrets require ",
                        "the current root key before switching providers."
                    )
                    .to_string(),
                );
            }
            api_error_from_secret_root_key_error(error)
        }
        mcpmate_secrets::SecretStoreRotationError::CurrentRecordUnreadable { alias, message } => {
            let readiness = SecretStoreReadiness::unavailable_with_provider(
                "secret_key_mismatch",
                format!("Secret '{alias}' cannot be decrypted with the current provider: {message}"),
                current_provider_metadata,
            );
            update_secret_store_readiness(state, readiness).await;
            ApiError::Conflict(format!(
                "Existing secure store record '{alias}' cannot be decrypted with the current provider"
            ))
        }
        mcpmate_secrets::SecretStoreRotationError::TargetProviderUnavailable(error) => {
            ApiError::ServiceUnavailable(format!("Target secure store provider is unavailable: {error}"))
        }
        mcpmate_secrets::SecretStoreRotationError::PersistenceFailed { action, message } => {
            ApiError::InternalError(format!("Secure store rotation failed during {action}: {message}"))
        }
        mcpmate_secrets::SecretStoreRotationError::PostRotationVerificationFailed { alias, message } => {
            ApiError::InternalError(format!(
                "Secret '{alias}' failed verification after secure store rotation: {message}"
            ))
        }
    }
}

fn data_dir_from_state(state: &Arc<AppState>) -> Result<std::path::PathBuf, ApiError> {
    let db = state
        .database
        .as_ref()
        .ok_or_else(|| ApiError::ServiceUnavailable("No database configured".to_string()))?;
    Ok(db.path.parent().unwrap_or(std::path::Path::new(".")).to_path_buf())
}

async fn apply_secret_store_bootstrap(
    state: &Arc<AppState>,
    bootstrap: crate::core::secrets::store::SecretStoreBootstrap,
) -> Result<SecretStoreReadiness, ApiError> {
    let mut store_guard = state.secret_store.write().await;
    apply_secret_store_bootstrap_with_locked_store(state, &mut store_guard, bootstrap).await
}

async fn apply_secret_store_bootstrap_with_locked_store(
    state: &Arc<AppState>,
    store_guard: &mut Option<Arc<crate::core::secrets::store::LocalSecretStore>>,
    bootstrap: crate::core::secrets::store::SecretStoreBootstrap,
) -> Result<SecretStoreReadiness, ApiError> {
    let readiness = bootstrap.readiness.clone();
    let store_arc = bootstrap.store.map(Arc::new);

    if let Some(store) = store_arc.clone() {
        state.connection_pool.lock().await.set_secret_resolver(store);
    }

    if let Some(db) = state.database.as_ref() {
        let new_manager = Arc::new(crate::core::oauth::OAuthManager::new_optional_store(
            db.pool.clone(),
            store_arc.clone(),
        ));
        let mut manager_guard = state.oauth_manager.write().await;
        *manager_guard = Some(new_manager);
    }

    *store_guard = store_arc;
    {
        let mut readiness_guard = state.secret_store_readiness.write().await;
        *readiness_guard = readiness.clone();
    }

    Ok(readiness)
}

async fn persist_provider_mode(
    state: &Arc<AppState>,
    mode: crate::core::secrets::store::RootKeyProviderMode,
) -> Result<(), ApiError> {
    let pool = state
        .database
        .as_ref()
        .ok_or_else(|| ApiError::ServiceUnavailable("No database configured".to_string()))?
        .pool
        .clone();
    mcpmate_secrets::database::upsert_provider_config(
        &pool,
        crate::core::secrets::store::provider_mode_to_persisted(mode),
    )
    .await
    .map_err(|err| ApiError::InternalError(format!("Failed to persist provider mode: {err}")))?;
    Ok(())
}

async fn secure_store_secret_count(pool: &SqlitePool) -> Result<i64, ApiError> {
    let table_name: Option<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'secure_store_secrets'")
            .fetch_optional(pool)
            .await
            .map_err(|err| ApiError::InternalError(format!("Failed to inspect secure store schema: {err}")))?;
    if table_name.is_none() {
        return Ok(0);
    }

    sqlx::query_scalar("SELECT COUNT(*) FROM secure_store_secrets")
        .fetch_one(pool)
        .await
        .map_err(|err| ApiError::InternalError(format!("Failed to count secure store secrets: {err}")))
}

async fn initialize_fresh_provider_with_locked_store(
    state: &Arc<AppState>,
    store_guard: &mut Option<Arc<crate::core::secrets::store::LocalSecretStore>>,
    pool: SqlitePool,
    mode: crate::core::secrets::store::RootKeyProviderMode,
    passphrase_path: &std::path::Path,
    local_file_path: &std::path::Path,
    passphrase: &str,
) -> Result<Json<ProviderSwitchResp>, ApiError> {
    let provider = provider_for_mode(mode, passphrase_path, local_file_path, passphrase)?;

    let new_store = crate::core::secrets::store::LocalSecretStore::initialize_with_root_key_provider(pool, provider)
        .await
        .map_err(|err| ApiError::InternalError(format!("Failed to initialize fresh provider: {err}")))?;

    persist_provider_mode(state, mode).await?;

    let bootstrap = crate::core::secrets::store::SecretStoreBootstrap {
        readiness: crate::core::secrets::store::SecretStoreReadiness::ready(new_store.provider_metadata()),
        store: Some(new_store),
    };
    let readiness = apply_secret_store_bootstrap_with_locked_store(state, store_guard, bootstrap).await?;

    let new_status = secret_store_status_data(&readiness);
    Ok(Json(ProviderSwitchResp::success(ProviderSwitchData { new_status })))
}

pub async fn unlock_secret_store(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<SecretStoreUnlockReq>,
) -> Result<Json<SecretStoreStatusResp>, ApiError> {
    if payload.passphrase.is_empty() {
        return Err(ApiError::BadRequest("Passphrase cannot be empty".to_string()));
    }

    let needs_unlock = {
        let readiness = state
            .secret_store_readiness
            .try_read()
            .map_err(|_| ApiError::ServiceUnavailable("Store lock contention".to_string()))?;
        match &*readiness {
            SecretStoreReadiness::Unavailable { reason_code, .. } => reason_code == "passphrase_unlock_required",
            _ => false,
        }
    };
    if !needs_unlock {
        return Err(ApiError::BadRequest(
            "Secret store does not require passphrase unlock".to_string(),
        ));
    }

    let data_dir = data_dir_from_state(&state)?;
    let pool = state
        .database
        .as_ref()
        .ok_or_else(|| ApiError::ServiceUnavailable("No database configured".to_string()))?
        .pool
        .clone();

    let bootstrap =
        crate::core::secrets::store::initialize_secret_store_with_passphrase(pool, &data_dir, &payload.passphrase)
            .await
            .map_err(|err| ApiError::InternalError(format!("Failed to unlock secret store: {err}")))?;

    if bootstrap.store.is_none() {
        let message = match &bootstrap.readiness {
            SecretStoreReadiness::Unavailable { message, .. } => message.clone(),
            _ => "Failed to unlock secret store".to_string(),
        };
        return Err(ApiError::BadRequest(message));
    }

    let readiness = apply_secret_store_bootstrap(&state, bootstrap).await?;
    Ok(Json(SecretStoreStatusResp::success(secret_store_status_data(
        &readiness,
    ))))
}

pub async fn rotate_passphrase(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<PassphraseRotateReq>,
) -> Result<Json<SecretStoreStatusResp>, ApiError> {
    if payload.new_passphrase.is_empty() {
        return Err(ApiError::BadRequest("New passphrase cannot be empty".to_string()));
    }
    if payload.new_passphrase != payload.confirm {
        return Err(ApiError::BadRequest("New passphrases do not match".to_string()));
    }
    if payload.new_passphrase.len() < 4 {
        return Err(ApiError::BadRequest(
            "New passphrase must be at least 4 characters".to_string(),
        ));
    }

    let store = get_secret_store(&state)?;
    if store.provider_metadata().mode() != crate::core::secrets::store::RootKeyProviderMode::Passphrase {
        return Err(ApiError::BadRequest(
            "Passphrase rotation is only available in passphrase encryption mode".to_string(),
        ));
    }

    let data_dir = data_dir_from_state(&state)?;
    let (passphrase_path, _) = crate::core::secrets::store::secret_store_paths(&data_dir);
    let pool = state
        .database
        .as_ref()
        .ok_or_else(|| ApiError::ServiceUnavailable("No database configured".to_string()))?
        .pool
        .clone();

    let current_provider: Arc<dyn crate::core::secrets::store::SecretRootKeyProvider> =
        Arc::new(crate::core::secrets::store::PassphraseRootKeyProvider::new(
            passphrase_path.clone(),
            payload.current_passphrase.clone(),
        ));
    let current_provider_metadata = current_provider.metadata();
    let target_provider: Arc<dyn crate::core::secrets::store::SecretRootKeyProvider> =
        Arc::new(crate::core::secrets::store::PassphraseRootKeyProvider::new(
            passphrase_path.clone(),
            payload.new_passphrase.clone(),
        ));

    let mut store_guard = state.secret_store.write().await;
    let new_store =
        match crate::core::secrets::store::LocalSecretStore::rotate_provider(pool, current_provider, target_provider)
            .await
        {
            Ok(store) => store,
            Err(err) => {
                return Err(map_secret_store_rotation_error(&state, err, current_provider_metadata).await);
            }
        };

    let bootstrap = crate::core::secrets::store::SecretStoreBootstrap {
        readiness: crate::core::secrets::store::SecretStoreReadiness::ready(new_store.provider_metadata()),
        store: Some(new_store),
    };
    let readiness = apply_secret_store_bootstrap_with_locked_store(&state, &mut store_guard, bootstrap).await?;
    Ok(Json(SecretStoreStatusResp::success(secret_store_status_data(
        &readiness,
    ))))
}

fn secret_usage_location_data(location: SecretUsageLocationInput) -> SecretUsageLocationData {
    match location {
        SecretUsageLocationInput::StdioCommand => SecretUsageLocationData::StdioCommand,
        SecretUsageLocationInput::StdioArgument { index } => SecretUsageLocationData::StdioArgument { index },
        SecretUsageLocationInput::StdioEnv { name } => SecretUsageLocationData::StdioEnv { name },
        SecretUsageLocationInput::StreamableHttpUrl => SecretUsageLocationData::StreamableHttpUrl,
        SecretUsageLocationInput::StreamableHttpHeader { name } => {
            SecretUsageLocationData::StreamableHttpHeader { name }
        }
        SecretUsageLocationInput::OAuthToken => SecretUsageLocationData::OAuthToken,
        SecretUsageLocationInput::LlmProviderApiKey => SecretUsageLocationData::LlmProviderApiKey,
    }
}

pub async fn switch_provider(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ProviderSwitchReq>,
) -> Result<Json<ProviderSwitchResp>, ApiError> {
    let db = state
        .database
        .as_ref()
        .ok_or_else(|| ApiError::ServiceUnavailable("No database configured".to_string()))?;
    let data_dir = data_dir_from_state(&state)?;
    let (passphrase_path, local_file_path) = crate::core::secrets::store::secret_store_paths(&data_dir);

    let new_mode = match payload.mode {
        ProviderModePayload::OperatingSystem => crate::core::secrets::store::RootKeyProviderMode::OperatingSystem,
        ProviderModePayload::Passphrase => crate::core::secrets::store::RootKeyProviderMode::Passphrase,
        ProviderModePayload::LocalFile => crate::core::secrets::store::RootKeyProviderMode::LocalFile,
    };

    let current_mode = {
        let store_guard = state
            .secret_store
            .try_read()
            .map_err(|_| ApiError::ServiceUnavailable("Store lock contention".to_string()))?;
        store_guard.as_ref().map(|s| s.provider_metadata().mode())
    };

    let current_mode = match current_mode {
        Some(mode) => mode,
        None => {
            // Store not initialized — read persisted mode from the database.
            let pool = &db.pool;
            mcpmate_secrets::database::get_provider_config(pool)
                .await
                .map_err(|err| ApiError::InternalError(format!("Failed to read provider config: {err}")))?
                .map(|cfg| {
                    crate::core::secrets::store::parse_persisted_provider_mode(&cfg.provider_mode)
                        .map_err(ApiError::BadRequest)
                })
                .transpose()?
                .unwrap_or(crate::core::secrets::store::RootKeyProviderMode::OperatingSystem)
        }
    };

    if current_mode != new_mode && payload.confirmation_phrase.as_deref() != Some(PROVIDER_SWITCH_CONFIRMATION_PHRASE) {
        return Err(ApiError::BadRequest(
            "Provider switch confirmation phrase is required".to_string(),
        ));
    }

    // Already on this mode. If the store is unavailable, treat this as an
    // explicit retry so OS keychain prompts can be raised again after the user
    // fixes the environment or grants access.
    if current_mode == new_mode {
        let readiness = state
            .secret_store_readiness
            .try_read()
            .map(|guard| secret_store_status_data(&guard))
            .unwrap_or_else(|_| SecretStoreStatusData {
                status: "ready".to_string(),
                provider: None,
                issue: None,
            });
        if readiness.status != "ready" {
            let bootstrap = crate::core::secrets::store::bootstrap_secret_store(db.pool.clone(), &data_dir).await;
            let new_readiness = apply_secret_store_bootstrap(&state, bootstrap).await?;
            let new_status = secret_store_status_data(&new_readiness);
            return Ok(Json(ProviderSwitchResp::success(ProviderSwitchData { new_status })));
        }
        return Ok(Json(ProviderSwitchResp::success(ProviderSwitchData {
            new_status: readiness,
        })));
    }

    // Validate passphrases for the switch direction.
    let new_passphrase = payload.passphrase.filter(|p| !p.is_empty());
    let current_passphrase = payload.current_passphrase.filter(|p| !p.is_empty());

    if new_mode == crate::core::secrets::store::RootKeyProviderMode::Passphrase && new_passphrase.is_none() {
        return Err(ApiError::BadRequest(
            "Passphrase is required for passphrase mode".to_string(),
        ));
    }

    let switching_from_passphrase = current_mode == crate::core::secrets::store::RootKeyProviderMode::Passphrase;
    if switching_from_passphrase && current_passphrase.is_none() {
        return Err(ApiError::BadRequest(
            "Current passphrase is required to switch from passphrase mode".to_string(),
        ));
    }

    let load_passphrase = current_passphrase.as_deref().unwrap_or_default();
    let store_passphrase = new_passphrase.as_deref().unwrap_or_default();

    let current_provider = provider_for_mode(current_mode, &passphrase_path, &local_file_path, load_passphrase)?;
    let current_provider_metadata = current_provider.metadata();
    let target_provider = provider_for_mode(new_mode, &passphrase_path, &local_file_path, store_passphrase)?;

    if switching_from_passphrase {
        current_provider
            .load_existing_root_key()
            .map_err(api_error_from_secret_root_key_error)?;
    }

    let mut store_guard = state.secret_store.write().await;
    let secret_count = secure_store_secret_count(&db.pool).await?;
    if secret_count == 0 {
        return initialize_fresh_provider_with_locked_store(
            &state,
            &mut store_guard,
            db.pool.clone(),
            new_mode,
            &passphrase_path,
            &local_file_path,
            store_passphrase,
        )
        .await;
    }

    let new_store = match crate::core::secrets::store::LocalSecretStore::rotate_provider(
        db.pool.clone(),
        current_provider,
        target_provider,
    )
    .await
    {
        Ok(store) => store,
        Err(err) => {
            return Err(map_secret_store_rotation_error(&state, err, current_provider_metadata).await);
        }
    };

    let bootstrap = crate::core::secrets::store::SecretStoreBootstrap {
        readiness: crate::core::secrets::store::SecretStoreReadiness::ready(new_store.provider_metadata()),
        store: Some(new_store),
    };
    let new_readiness = apply_secret_store_bootstrap_with_locked_store(&state, &mut store_guard, bootstrap).await?;

    // Clean up the old provider's key file AFTER bootstrap succeeds.
    match current_mode {
        crate::core::secrets::store::RootKeyProviderMode::LocalFile => {
            let _ = std::fs::remove_file(&local_file_path);
        }
        crate::core::secrets::store::RootKeyProviderMode::Passphrase => {
            let _ = std::fs::remove_file(&passphrase_path);
        }
        _ => {} // OS keyring has no file to clean.
    }

    let new_status = secret_store_status_data(&new_readiness);
    Ok(Json(ProviderSwitchResp::success(ProviderSwitchData { new_status })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, time::Duration};

    use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
    use tempfile::TempDir;
    use tokio::sync::{Mutex, RwLock};

    use crate::{
        config::database::Database,
        core::{models::Config, pool::UpstreamConnectionPool, secrets::store::LocalSecretStore},
        inspector::{calls::InspectorCallRegistry, sessions::InspectorSessionManager},
        system::metrics::MetricsCollector,
    };
    use mcpmate_secrets::SecretRootKeyProvider;

    struct TestContext {
        _temp_dir: TempDir,
        app_state: Arc<AppState>,
        store: Arc<LocalSecretStore>,
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
        crate::config::llm::init::initialize_llm_tables(&db_pool)
            .await
            .expect("init llm tables");
        let store = Arc::new(
            LocalSecretStore::initialize_with_development_root_key(
                db_pool.clone(),
                temp_dir.path().join("secrets").join("local-root.key"),
            )
            .await
            .expect("init secret store"),
        );

        let database = Arc::new(Database {
            pool: db_pool,
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
            config_application_state: Arc::new(crate::core::profile::ConfigApplicationStateManager::new()),
            client_service: None,
            inspector_calls: Arc::new(InspectorCallRegistry::new()),
            inspector_sessions: Arc::new(InspectorSessionManager::new()),
            oauth_manager: RwLock::new(None),
            secret_store: RwLock::new(Some(store.clone())),
            secret_store_readiness: RwLock::new(SecretStoreReadiness::ready(store.provider_metadata())),
        });

        TestContext {
            _temp_dir: temp_dir,
            app_state,
            store,
        }
    }

    async fn create_secret_with_unknown_usage(
        context: &TestContext,
        alias: &str,
    ) {
        context
            .store
            .create_secret(SecretCreateInput {
                alias: alias.to_string(),
                kind: SecretKindInput::ApiKey,
                value: "secret".to_string(),
                label: None,
                origin: None,
            })
            .await
            .expect("create secret");
        insert_unsupported_usage(&context.store.pool(), alias).await;
    }

    async fn insert_unsupported_usage(
        pool: &SqlitePool,
        alias: &str,
    ) {
        sqlx::query(
            r#"
            INSERT INTO secure_store_usages (
                id, alias, server_id, location_kind, location_name, location_index
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
        )
        .bind(format!("{alias}-future-location-row"))
        .bind(alias)
        .bind("LLMPROVFuture")
        .bind("future_runtime_location")
        .bind(Option::<String>::None)
        .bind(Option::<i64>::None)
        .execute(pool)
        .await
        .expect("insert unknown usage row");
    }

    #[tokio::test]
    async fn get_secret_details_reports_unknown_usage_separately() {
        let context = create_test_context().await;
        create_secret_with_unknown_usage(&context, "provider-api-key").await;

        let Json(response) = get_secret_details(
            State(context.app_state.clone()),
            Query(SecretDetailsReq {
                alias: "provider-api-key".to_string(),
            }),
        )
        .await
        .expect("get details");

        assert!(response.success);
        let data = response.data.expect("details data");
        assert_eq!(data.used_by_count, 0);
        assert_eq!(data.unknown_usage_count, 1);
    }

    #[tokio::test]
    async fn delete_secret_rejects_unknown_usage_without_force() {
        let context = create_test_context().await;
        create_secret_with_unknown_usage(&context, "provider-api-key").await;

        let error = delete_secret(
            State(context.app_state.clone()),
            Json(SecretDeleteReq {
                alias: "provider-api-key".to_string(),
                force: false,
            }),
        )
        .await
        .expect_err("delete should reject unknown usage");

        assert!(matches!(error, ApiError::Conflict(_)));
        assert!(context.store.get_secret_metadata("provider-api-key").await.is_ok());
    }

    #[test]
    fn root_key_load_error_readiness_preserves_provider_metadata() {
        let metadata = crate::core::secrets::store::OperatingSystemRootKeyProvider::new().metadata();
        let error = mcpmate_secrets::SecretRootKeyError::ProviderUnavailable("keychain denied".to_string());

        let readiness = secret_store_readiness_from_root_key_error(&error, metadata);

        match readiness {
            SecretStoreReadiness::Unavailable {
                reason_code,
                provider: Some(provider),
                ..
            } => {
                assert_eq!(reason_code, "provider_unavailable");
                assert_eq!(provider.provider_mode, "operating_system");
            }
            other => panic!("expected unavailable readiness with provider metadata, got {other:?}"),
        }
    }
}
