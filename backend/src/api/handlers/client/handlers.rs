// HTTP handlers for client management API (template-driven)

use super::backups::parse_policy_payload;
use super::inspection::{
    ClientFileGuardRejection, attachment_state_drift_reason, build_config_file_parse_inspect_data,
    build_merge_strategy_metadata, build_parse_metadata, configured_server_entries_data, derive_attachment_state,
    detect_mcpmate_in_client_config, evaluate_client_attach_file_guard, evaluate_client_detach_file_guard,
    inspect_client_config_lenient, mark_configured_server_managed_status, parse_api_transports,
    parse_rule_from_api_data, transports_data_from_state,
};
use crate::api::models::client::{
    ClientAttachData, ClientAttachReq, ClientAttachResp, ClientBackupActionData, ClientBackupActionResp,
    ClientCapabilityConfigData, ClientCapabilityConfigReq, ClientCapabilityConfigResp, ClientCheckData, ClientCheckReq,
    ClientCheckResp, ClientConfigData, ClientConfigFileParseInspectExistingReq,
    ClientConfigFileParseInspectExistingResp, ClientConfigFileParseInspectReq, ClientConfigFileParseInspectResp,
    ClientConfigMode, ClientConfigReq, ClientConfigResp, ClientConfigRestoreReq, ClientConfigSelected,
    ClientConfigUpdateData, ClientConfigUpdateReq, ClientConfigUpdateResp, ClientDetachData, ClientDetachReq,
    ClientDetachResp, ClientDetectReq, ClientInfo, ClientTemplateMetadata, ClientTemplateStorageMetadata,
    ClientUnifyDirectExposureData,
};
use crate::api::routes::AppState;
use crate::audit::{AuditAction, AuditEvent, AuditStatus};
use crate::clients::document::{get_config_last_modified, infer_format_from_path, parse_config_fallback};
use crate::clients::models::{AttachmentState, CapabilitySource, ClientCapabilityConfigState, ClientGovernanceKind};
use crate::clients::service::core::{ClientStateRow, RuntimeClientMetadata};
use crate::clients::service::settings::ActiveClientSettingsUpdate;
use crate::clients::{
    ClientConfigService, ClientDescriptor, ClientRenderOptions, ConfigError, ConfigMode, TemplateExecutionResult,
};
use crate::common::ClientCategory;
use axum::{
    extract::{Json, Query, State},
    http::StatusCode,
};
use chrono::Utc;
use serde_json::{Value, json};
use std::sync::Arc;

type ClientSettingsErrorResponse = Box<(StatusCode, Json<crate::api::models::client::ClientSettingsUpdateResp>)>;

fn client_settings_error(
    status: StatusCode,
    message: impl Into<String>,
) -> ClientSettingsErrorResponse {
    Box::new((
        status,
        Json(crate::api::models::client::ClientSettingsUpdateResp::error_simple(
            "client_settings_invalid",
            &message.into(),
        )),
    ))
}

fn build_client_capability_config_data(
    identifier: String,
    state: ClientCapabilityConfigState,
    source_revision_set: crate::api::models::CatalogRevisionSet,
) -> ClientCapabilityConfigData {
    ClientCapabilityConfigData {
        identifier,
        capability_source: state.capability_config.capability_source,
        selected_profile_ids: state.capability_config.selected_profile_ids,
        custom_profile_id: state.capability_config.custom_profile_id,
        custom_profile_missing: state.custom_profile_missing,
        unify_direct_exposure: ClientUnifyDirectExposureData {
            intent: state.unify_direct_exposure_intent,
            diagnostics: state.unify_direct_exposure_diagnostics,
            resolved_capabilities: state.unify_direct_exposure,
        },
        source_revision_set,
    }
}

fn parse_inspect_existing_error(
    err: ConfigError,
) -> Box<(StatusCode, Json<ClientConfigFileParseInspectExistingResp>)> {
    let status = map_config_error_status(&err);
    let response = match err {
        ConfigError::ClientNotFound { .. } => ClientConfigFileParseInspectExistingResp::error_simple(
            "client_config_parse_missing_client",
            "Client record not found.",
        ),
        _ => ClientConfigFileParseInspectExistingResp::error_simple("client_config_parse_invalid", &err.to_string()),
    };
    Box::new((status, Json(response)))
}

fn should_include_default_client(descriptor: &ClientDescriptor) -> bool {
    if !descriptor.persisted {
        return false;
    }

    let state = &descriptor.state;
    match state.governance_kind() {
        ClientGovernanceKind::Active => true,
        ClientGovernanceKind::Passive => state.runtime_observed(),
    }
}

pub async fn config_file_parse_inspect(
    State(app_state): State<Arc<AppState>>,
    Json(request): Json<ClientConfigFileParseInspectReq>,
) -> Result<Json<ClientConfigFileParseInspectResp>, Box<(StatusCode, Json<ClientConfigFileParseInspectResp>)>> {
    let service = get_client_service(&app_state).map_err(|status| {
        Box::new((
            status,
            Json(ClientConfigFileParseInspectResp::error_simple(
                "client_config_parse_unavailable",
                "Client service unavailable",
            )),
        ))
    })?;
    let draft = request.config_file_parse.as_ref().map(parse_rule_from_api_data);
    let inspection = service
        .inspect_config_file_parse(&request.config_path, draft.as_ref())
        .await
        .map_err(|err| {
            let status = map_config_error_status(&err);
            Box::new((
                status,
                Json(ClientConfigFileParseInspectResp::error_simple(
                    "client_config_parse_invalid",
                    &err.to_string(),
                )),
            ))
        })?;

    Ok(Json(ClientConfigFileParseInspectResp::success(
        build_config_file_parse_inspect_data(inspection),
    )))
}

pub async fn config_file_parse_inspect_existing(
    State(app_state): State<Arc<AppState>>,
    Json(request): Json<ClientConfigFileParseInspectExistingReq>,
) -> Result<Json<ClientConfigFileParseInspectExistingResp>, Box<(StatusCode, Json<ClientConfigFileParseInspectExistingResp>)>>
{
    let service = get_client_service(&app_state).map_err(|status| {
        Box::new((
            status,
            Json(ClientConfigFileParseInspectExistingResp::error_simple(
                "client_config_parse_unavailable",
                "Client service unavailable",
            )),
        ))
    })?;
    let draft = request.config_file_parse.as_ref().map(parse_rule_from_api_data);
    let inspection = service
        .inspect_existing_client_config_file_parse(&request.identifier, draft.as_ref())
        .await
        .map_err(parse_inspect_existing_error)?;

    Ok(Json(ClientConfigFileParseInspectExistingResp::success(
        build_config_file_parse_inspect_data(inspection),
    )))
}

/// Handler for GET /api/client
/// Detects and returns all clients, with optional template reload
pub async fn list(
    State(app_state): State<Arc<AppState>>,
    Query(request): Query<ClientCheckReq>,
) -> Result<Json<ClientCheckResp>, StatusCode> {
    let service = get_client_service(&app_state)?;

    let descriptors = service
        .list_clients(request.refresh, request.persist_detected)
        .await
        .map_err(|err| {
            tracing::error!("Failed to list clients: {}", err);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let mut client_infos = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors
        .into_iter()
        .filter(|descriptor| request.include_detected || should_include_default_client(descriptor))
    {
        match descriptor_to_client_info(service.as_ref(), app_state.as_ref(), descriptor).await {
            Ok(info) => client_infos.push(info),
            Err(status) => return Err(status),
        }
    }

    let response = ClientCheckData {
        total: client_infos.len(),
        client: client_infos,
        last_updated: Utc::now().to_rfc3339(),
    };

    Ok(Json(ClientCheckResp::success(response)))
}

/// Handler for GET /api/client/detect
/// Detects installed clients without persisting candidates as client records.
pub async fn detect(
    State(app_state): State<Arc<AppState>>,
    Query(request): Query<ClientDetectReq>,
) -> Result<Json<ClientCheckResp>, StatusCode> {
    let service = get_client_service(&app_state)?;

    let descriptors = service.list_clients(request.refresh, false).await.map_err(|err| {
        tracing::error!("Failed to detect clients: {}", err);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let mut client_infos = Vec::new();
    for descriptor in descriptors
        .into_iter()
        .filter(|descriptor| descriptor.detection.is_some())
    {
        match descriptor_to_client_info(service.as_ref(), app_state.as_ref(), descriptor).await {
            Ok(info) => client_infos.push(info),
            Err(status) => return Err(status),
        }
    }

    let response = ClientCheckData {
        total: client_infos.len(),
        client: client_infos,
        last_updated: Utc::now().to_rfc3339(),
    };

    Ok(Json(ClientCheckResp::success(response)))
}

/// Handler for GET /api/client/config/details
/// Returns current configuration content
pub async fn config_details(
    State(app_state): State<Arc<AppState>>,
    Query(request): Query<ClientConfigReq>,
) -> Result<Json<ClientConfigResp>, StatusCode> {
    let service = get_client_service(&app_state)?;
    let state = service
        .fetch_state_repairing_local_target(&request.identifier)
        .await
        .map_err(|err| {
            tracing::error!(
                client = %request.identifier,
                error = %err,
                "Failed to load client state for config details"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or_else(|| {
            tracing::error!("Failed to resolve client config details for {}", request.identifier);
            StatusCode::NOT_FOUND
        })?;

    let config_path = service.config_path(&request.identifier).await.map_err(|err| {
        tracing::error!("Failed to resolve config path for {}: {}", request.identifier, err);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let mut warnings: Vec<String> = Vec::new();
    let mut degraded_reasons: Vec<String> = Vec::new();
    let has_local_config_target = state.has_local_config_target();
    let use_runtime_config_read = !has_local_config_target;
    let content = if use_runtime_config_read {
        read_runtime_config(service.as_ref(), &request.identifier)
            .await
            .map_err(|err| {
                tracing::error!(client = %request.identifier, error = %err, "Failed to read runtime config");
                StatusCode::INTERNAL_SERVER_ERROR
            })?
    } else {
        match service.read_current_config(&request.identifier).await {
            Ok(content) => content,
            Err(err) => {
                let message = format!("Unable to read current configuration: {}", err);
                tracing::warn!(
                    client = %request.identifier,
                    error = %err,
                    "Gracefully degrading after configuration read failure"
                );
                warnings.push(message);
                degraded_reasons.push("config_read_failed_fallback_none".to_string());
                None
            }
        }
    };

    let config_exists = content.is_some();
    let inspected_config = inspect_client_config_lenient(content.as_deref(), &state);
    let parsed_content = inspected_config
        .as_ref()
        .map(|inspected| inspected.document.clone())
        .unwrap_or_else(|| match state.effective_config_file_parse() {
            Ok(parse_rule) => parse_config_fallback(content.as_deref(), parse_rule.as_ref(), state.config_path()),
            Err(err) => {
                warnings.push(format!("Persisted config parse metadata is invalid: {err}"));
                degraded_reasons.push("config_file_parse_invalid".to_string());
                parse_config_fallback(content.as_deref(), None, state.config_path())
            }
        });

    let (configured_servers_analysis, mut configured_server_entries) = inspected_config
        .as_ref()
        .map(configured_server_entries_data)
        .unwrap_or_default();
    if let Some(reason) = attachment_state_drift_reason(&state, &configured_servers_analysis) {
        degraded_reasons.push(reason.to_string());
    }
    if let Some(database) = app_state.database.as_ref() {
        mark_configured_server_managed_status(&database.pool, &mut configured_server_entries)
            .await
            .map_err(|err| {
                tracing::error!(
                    client = %request.identifier,
                    error = %err,
                    "Failed to annotate configured server managed status"
                );
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
    }

    let last_modified = config_path.as_deref().and_then(get_config_last_modified);

    let config_type = match state.container_type() {
        Some("array") => Some(crate::api::models::client::ClientConfigType::Array),
        Some("object") => Some(crate::api::models::client::ClientConfigType::Standard),
        _ => None,
    }
    .or_else(|| infer_config_type_from_path(config_path.as_deref()));

    let stored_runtime_metadata = state.runtime_client_metadata();
    let template_metadata = service
        .runtime_template_metadata(state.template_identifier().unwrap_or(state.identifier()))
        .await
        .map_err(|err| {
            tracing::error!(
                client = %request.identifier,
                error = %err,
                "Failed to resolve runtime template metadata for config details"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let runtime_metadata =
        RuntimeClientMetadata::resolve_with_template_metadata(&stored_runtime_metadata, template_metadata);
    let description = runtime_metadata.description.clone();
    let homepage_url = runtime_metadata.homepage_url.clone();
    let docs_url = runtime_metadata.docs_url.clone();
    let support_url = runtime_metadata.support_url.clone();
    let logo_url = runtime_metadata.logo_url.clone();

    let capability_config_state = service
        .get_capability_config_state(&request.identifier)
        .await
        .map_err(|err| {
            tracing::error!(client = %request.identifier, error = %err, "Failed to load capability config state");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let capability_config = capability_config_state
        .as_ref()
        .map(|state| state.capability_config.clone())
        .unwrap_or_default();
    let custom_profile_missing = capability_config_state
        .as_ref()
        .map(|state| state.custom_profile_missing)
        .unwrap_or(false);
    let config_file_state = state.config_file_state();
    let registration_origin = state.registration_origin();
    let runtime_observed = state.runtime_observed();
    let governed_by_default_policy = state.governed_by_default_policy();
    let approval_status = Some(state.approval_status().to_string());
    let attachment_state = derive_attachment_state(&state, &configured_servers_analysis);
    let writable_config = service
        .has_verified_local_config_target(&request.identifier)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(client = %request.identifier, error = %err, "Failed to verify local config target");
            degraded_reasons.push("writable_target_verification_failed_default_false".to_string());
            false
        });
    let (config_file_parse_effective, config_file_parse_override, uses_template_parse_default) =
        build_parse_metadata(&state);
    let system_merge_strategy_override = service.default_merge_strategy_override().await.map_err(|err| {
        tracing::error!(
            client = %request.identifier,
            error = %err,
            "Failed to load the system writeback default"
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let merge_strategy_metadata =
        build_merge_strategy_metadata(&state, system_merge_strategy_override).map_err(|err| {
            tracing::error!(
                client = %request.identifier,
                error = %err,
                "Failed to resolve client writeback behavior"
            );
            map_config_error_status(&err)
        })?;
    let transports = transports_data_from_state(&state);

    let data = ClientConfigData {
        config_path: config_path.unwrap_or_default(),
        config_exists,
        content: parsed_content,
        has_mcp_config: configured_servers_analysis.has_mcp_config,
        mcp_servers_count: configured_servers_analysis.server_count,
        configured_server_entries,
        last_modified,
        config_type,
        template: build_template_metadata_from_state(&state, &runtime_metadata),
        transports,
        description,
        homepage_url,
        docs_url,
        support_url,
        logo_url,
        capability_source: capability_config.capability_source,
        selected_profile_ids: capability_config.selected_profile_ids,
        custom_profile_id: capability_config.custom_profile_id,
        custom_profile_missing,
        approval_status,
        attachment_state: Some(attachment_state),
        config_file_state,
        registration_origin,
        runtime_observed,
        governed_by_default_policy,
        writable_config,
        config_file_parse_effective,
        config_file_parse_override,
        uses_template_parse_default,
        template_merge_strategy: merge_strategy_metadata.template_merge_strategy,
        system_merge_strategy_override: merge_strategy_metadata.system_merge_strategy_override,
        merge_strategy_override: merge_strategy_metadata.merge_strategy_override,
        effective_merge_strategy: merge_strategy_metadata.effective_merge_strategy,
        merge_strategy_source: merge_strategy_metadata.merge_strategy_source,
        supported_merge_strategies: merge_strategy_metadata.supported_merge_strategies,
        warnings,
        degraded_reasons,
    };

    Ok(Json(ClientConfigResp::success(data)))
}

/// Handler for POST /api/client/config/apply
/// Generates and optionally applies configuration
#[tracing::instrument(
    skip(app_state, request),
    level = "debug",
    fields(
        client = %request.identifier,
        mode = ?request.mode,
        preview = %request.preview,
        selected = ?request.selected_config
    )
)]
pub async fn config_apply(
    State(app_state): State<Arc<AppState>>,
    Json(request): Json<ClientConfigUpdateReq>,
) -> Result<Json<ClientConfigUpdateResp>, StatusCode> {
    let service = get_client_service(&app_state)?;
    let mark_attached_on_apply = !matches!(request.mode, ClientConfigMode::Transparent);
    let applied_config =
        apply_client_config_request(&service, &request, "config_apply", mark_attached_on_apply).await?;
    let data = applied_config.data;

    tracing::info!(
        client = %request.identifier,
        applied = data.applied,
        preview = %request.preview,
        scheduled = data.scheduled.unwrap_or(false),
        "config_apply succeeded"
    );

    emit_client_audit_event(
        &app_state,
        AuditAction::ClientConfigApply,
        AuditStatus::Success,
        "/api/client/config/apply",
        &request.identifier,
        Some(request.identifier.clone()),
        Some(json!({
            "mode": request.mode,
            "preview": request.preview,
            "applied": data.applied,
            "scheduled": data.scheduled.unwrap_or(false),
            "selected_config": request.selected_config,
        })),
        None,
    )
    .await;

    Ok(Json(ClientConfigUpdateResp::success(data)))
}

struct AppliedClientConfig {
    data: ClientConfigUpdateData,
}

async fn apply_client_config_request(
    service: &ClientConfigService,
    request: &ClientConfigUpdateReq,
    operation: &'static str,
    mark_attached_on_apply: bool,
) -> Result<AppliedClientConfig, StatusCode> {
    let requested_backup_policy = if request.preview {
        None
    } else {
        request.backup_policy.as_ref().map(parse_policy_payload).transpose()?
    };

    let existing_state = service.fetch_state(&request.identifier).await.map_err(|err| {
        tracing::error!(client = %request.identifier, operation, error = %err, "Failed to load client state before apply");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if let Some(state) = existing_state.as_ref() {
        if state.approval_status() != "approved" {
            tracing::warn!(
                client = %request.identifier,
                operation,
                approval_status = %state.approval_status(),
                "Rejected config apply for non-approved client"
            );
            return Err(StatusCode::FORBIDDEN);
        }
    }

    match service.has_verified_local_config_target(&request.identifier).await {
        Ok(true) => {}
        Ok(false) => {
            if !existing_state
                .as_ref()
                .map(ClientStateRow::has_local_config_target)
                .unwrap_or(false)
            {
                tracing::warn!(
                    client = %request.identifier,
                    operation,
                    "Rejected config apply without runtime local config target"
                );
                return Err(StatusCode::FORBIDDEN);
            }
        }
        Err(err) => {
            tracing::warn!(
                client = %request.identifier,
                operation,
                error = %err,
                "Rejected config apply without a verified local config target"
            );
            return Err(StatusCode::FORBIDDEN);
        }
    }

    let client_name = service.resolve_client_name(&request.identifier).await.map_err(|err| {
        tracing::error!(client = %request.identifier, operation, error = %err, "Failed to resolve client name");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    match existing_state.as_ref() {
        Some(state) => {
            service
                .ensure_active_state_row_with_name(&request.identifier, &client_name, Some(state.approval_status()))
                .await
                .map_err(|err| {
                    tracing::error!(client = %request.identifier, operation, error = %err, "Failed to refresh active client state before apply");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
        }
        None => {
            service
                .ensure_active_state_row_with_name(&request.identifier, &client_name, Some("approved"))
                .await
                .map_err(|err| {
                    tracing::error!(client = %request.identifier, operation, error = %err, "Failed to create active client state before apply");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
        }
    }

    if let Some(policy) = requested_backup_policy {
        service
            .set_backup_policy(&request.identifier, policy)
            .await
            .map_err(|err| {
                tracing::error!(
                    client = %request.identifier,
                    operation,
                    error = %err,
                    "Failed to persist backup policy before apply"
                );
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
    }

    let options = build_render_options(request);
    let outcome = service.apply_with_deferred(options).await.map_err(|err| {
        let status = map_config_error_status(&err);
        tracing::error!(
            client = %request.identifier,
            operation,
            mode = ?request.mode,
            preview = %request.preview,
            selected = ?request.selected_config,
            status = %status.as_u16(),
            error = %err,
            "config apply failed"
        );
        status
    })?;

    let synthetic = TemplateExecutionResult::DryRun {
        diff: crate::clients::mutate::ConfigDiff {
            format: outcome.preview.format,
            before: outcome.preview.before.clone(),
            after: outcome.preview.after.clone(),
            summary: outcome.preview.summary.clone(),
        },
        content: outcome.preview.after.clone().unwrap_or_default(),
    };

    let state = service.fetch_state(&request.identifier).await.map_err(|err| {
        tracing::error!(client = %request.identifier, operation, error = %err, "Failed to load client state for preview");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let preview_content = match &synthetic {
        TemplateExecutionResult::Applied { content, .. } => content.as_str(),
        TemplateExecutionResult::DryRun { content, .. } => content.as_str(),
    };
    let preview = match state.as_ref() {
        Some(state) => {
            let parse_rule = match state.effective_config_file_parse() {
                Ok(rule) => rule,
                Err(err) => {
                    tracing::warn!(
                        client = %request.identifier,
                        error = %err,
                        "Invalid persisted config_file_parse while building apply preview; degrading to autodetect"
                    );
                    None
                }
            };
            parse_config_fallback(Some(preview_content), parse_rule.as_ref(), state.config_path())
        }
        None => Value::String(preview_content.to_string()),
    };
    let mut warnings = outcome.warnings.clone();
    warnings.extend(outcome.preview.summary.clone());
    let applied = outcome.applied && !request.preview;

    if applied && mark_attached_on_apply {
        service.mark_client_attached(&request.identifier).await.map_err(|err| {
            tracing::error!(client = %request.identifier, operation, error = %err, "Failed to persist attached state after config apply");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        service
            .ensure_initial_managed_surface(&request.identifier)
            .await
            .map_err(|err| {
                tracing::error!(
                    client = %request.identifier,
                    operation,
                    error = %err,
                    "Failed to publish the initial managed Consumer Surface after config apply"
                );
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
    }

    Ok(AppliedClientConfig {
        data: ClientConfigUpdateData {
            success: true,
            preview,
            applied,
            backup_path: outcome.backup_path.clone(),
            warnings,
            diff_format: Some(outcome.preview.format.as_str().to_string()),
            diff_before: outcome.preview.before.clone(),
            diff_after: outcome.preview.after.clone(),
            scheduled: Some(outcome.scheduled),
            scheduled_reason: outcome.scheduled_reason,
        },
    })
}

fn map_config_error_status(err: &ConfigError) -> StatusCode {
    match err {
        ConfigError::ClientDisabled { .. } => StatusCode::FORBIDDEN,
        ConfigError::ClientNotFound { .. } => StatusCode::NOT_FOUND,
        ConfigError::ConcurrencyConflict { .. } => StatusCode::CONFLICT,
        ConfigError::DataAccessError(_)
        | ConfigError::PathResolutionError(_)
        | ConfigError::PathNotWritable { .. }
        | ConfigError::FileOperationError(_)
        | ConfigError::IoError(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn client_file_guard_warnings(
    guard: Result<Option<&'static str>, ClientFileGuardRejection>,
    operation: &'static str,
    identifier: &str,
    attachment_state: AttachmentState,
) -> Result<Vec<String>, StatusCode> {
    match guard {
        Ok(Some(warning)) => Ok(vec![warning.to_string()]),
        Ok(None) => Ok(Vec::new()),
        Err(ClientFileGuardRejection::UndetectableFile) => {
            tracing::warn!(
                client = %identifier,
                operation,
                "Rejected client file guard: config file state undetectable"
            );
            Err(StatusCode::BAD_REQUEST)
        }
        Err(ClientFileGuardRejection::NonApplicableDbState) => {
            tracing::warn!(
                client = %identifier,
                operation,
                attachment = ?attachment_state,
                "Rejected client file guard: non-applicable DB state"
            );
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

/// Handler for POST /api/client/config/restore
/// Restores configuration from a named backup snapshot
pub async fn config_restore(
    State(app_state): State<Arc<AppState>>,
    Json(request): Json<ClientConfigRestoreReq>,
) -> Result<Json<ClientBackupActionResp>, StatusCode> {
    let service = get_client_service(&app_state)?;

    if let Ok(Some(state)) = service.fetch_state(&request.identifier).await {
        if state.is_pending_approval() {
            tracing::warn!(
                client = %request.identifier,
                approval_status = %state.approval_status(),
                "Rejected config restore for pending client"
            );
            return Err(StatusCode::FORBIDDEN);
        }
    }

    let client_name = service.resolve_client_name(&request.identifier).await.map_err(|err| {
        tracing::error!(client = %request.identifier, error = %err, "Failed to resolve client name");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    service
        .ensure_active_state_row_with_name(&request.identifier, &client_name, Some("approved"))
        .await
        .map_err(|err| {
            tracing::error!(client = %request.identifier, error = %err, "Failed to activate client state before restore");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let result = service
        .restore_backup(&request.identifier, &request.backup)
        .await
        .map_err(|err| match err {
            ConfigError::TemplateIndexError(_) | ConfigError::FileOperationError(_) => {
                tracing::warn!(
                    "Backup {} for client {} not found or unreadable",
                    request.backup,
                    request.identifier
                );
                StatusCode::NOT_FOUND
            }
            other => {
                tracing::error!(
                    "Failed to restore backup {} for {}: {}",
                    request.backup,
                    request.identifier,
                    other
                );
                StatusCode::INTERNAL_SERVER_ERROR
            }
        })?;

    let message = match result {
        Some(path) => format!("Restored configuration; previous version saved to {}", path),
        None => "Restored configuration without creating a new backup".to_string(),
    };

    let data = ClientBackupActionData {
        identifier: request.identifier,
        backup: request.backup,
        message,
    };

    emit_client_audit_event(
        &app_state,
        AuditAction::ClientConfigRestore,
        AuditStatus::Success,
        "/api/client/config/restore",
        &data.identifier,
        Some(data.backup.clone()),
        Some(json!({
            "backup": data.backup,
            "message": data.message,
        })),
        None,
    )
    .await;

    Ok(Json(ClientBackupActionResp::success(data)))
}

pub(crate) fn get_client_service(state: &AppState) -> Result<Arc<ClientConfigService>, StatusCode> {
    state
        .client_service
        .as_ref()
        .cloned()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

async fn emit_client_audit_event(
    app_state: &AppState,
    action: AuditAction,
    status: AuditStatus,
    route: &str,
    client_id: &str,
    target: Option<String>,
    data: Option<Value>,
    error_message: Option<String>,
) {
    let mut event = AuditEvent::new(action, status)
        .with_http_route("POST", route)
        .with_client_id(client_id.to_string());

    if let Some(target) = target {
        event = event.with_target(target);
    }
    if let Some(data) = data {
        event = event.with_data(data);
    }
    if let Some(error_message) = error_message {
        event = event.with_error(None::<String>, error_message);
    }

    crate::audit::interceptor::emit_event(app_state.audit_service.as_ref(), event.build()).await;
}

/// PATCH/POST /api/client/update - partial update client settings
pub async fn update_settings(
    State(app_state): State<Arc<AppState>>,
    Json(request): Json<crate::api::models::client::ClientSettingsUpdateReq>,
) -> Result<Json<crate::api::models::client::ClientSettingsUpdateResp>, ClientSettingsErrorResponse> {
    let service =
        get_client_service(&app_state).map_err(|status| client_settings_error(status, "Client service unavailable"))?;

    tracing::info!(
        client = %request.identifier,
        transport = ?request.transport,
        client_version = ?request.client_version,
        display_name = ?request.display_name,
        config_file_state = ?request.config_file_state,
        config_path = ?request.config_path,
        clear_config_file_parse = %request.clear_config_file_parse,
        merge_strategy_override = ?request.merge_strategy_override,
        clear_merge_strategy_override = %request.clear_merge_strategy_override,
        "update_settings: received request"
    );

    let parsed_transports = request
        .transports
        .as_ref()
        .map(parse_api_transports)
        .transpose()
        .map_err(|err| client_settings_error(StatusCode::UNPROCESSABLE_ENTITY, err))?;

    let settings_result = service
        .set_active_client_settings(
            &request.identifier,
            ActiveClientSettingsUpdate {
                display_name: request.display_name.clone(),
                transport: request.transport.clone(),
                client_version: request.client_version.clone(),
                config_file_state: request.config_file_state,
                config_path: request.config_path.clone(),
                description: request.description.clone(),
                homepage_url: request.homepage_url.clone(),
                docs_url: request.docs_url.clone(),
                support_url: request.support_url.clone(),
                logo_url: request.logo_url.clone(),
                config_file_parse: request.config_file_parse.as_ref().map(parse_rule_from_api_data),
                clear_config_file_parse: request.clear_config_file_parse,
                merge_strategy_override: request.merge_strategy_override,
                clear_merge_strategy_override: request.clear_merge_strategy_override,
                transports: parsed_transports,
                clear_transports: request.clear_transports,
            },
        )
        .await
        .map_err(|err| {
            let status = map_config_error_status(&err);
            tracing::error!(
                client = %request.identifier,
                error = %err,
                status = %status.as_u16(),
                "Failed to update client settings"
            );
            client_settings_error(status, err.to_string())
        })?;

    let (_, transport, version) = service
        .get_client_settings(&request.identifier)
        .await
        .map_err(|err| {
            tracing::error!(
                client = %request.identifier,
                error = %err,
                "Failed to fetch updated client settings"
            );
            client_settings_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
        })?
        .unwrap_or((None, "auto".into(), None));
    let state = service
        .fetch_state(&request.identifier)
        .await
        .map_err(|err| {
            tracing::error!(client = %request.identifier, error = %err, "Failed to fetch updated client state");
            client_settings_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
        })?
        .ok_or_else(|| client_settings_error(StatusCode::NOT_FOUND, "Client state not found"))?;
    let stored_runtime_metadata = state.runtime_client_metadata();
    let template_metadata = service
        .runtime_template_metadata(state.template_identifier().unwrap_or(state.identifier()))
        .await
        .map_err(|err| {
            tracing::error!(
                client = %state.identifier(),
                error = %err,
                "Failed to resolve runtime template metadata after settings update"
            );
            client_settings_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
        })?;
    let runtime_metadata =
        RuntimeClientMetadata::resolve_with_template_metadata(&stored_runtime_metadata, template_metadata);
    let (config_file_parse_effective, config_file_parse_override, uses_template_parse_default) =
        build_parse_metadata(&state);
    let system_merge_strategy_override = service.default_merge_strategy_override().await.map_err(|err| {
        tracing::error!(
            client = %request.identifier,
            error = %err,
            "Failed to load the system writeback default after settings update"
        );
        client_settings_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
    })?;
    let merge_strategy_metadata =
        build_merge_strategy_metadata(&state, system_merge_strategy_override).map_err(|err| {
            let status = map_config_error_status(&err);
            tracing::error!(
                client = %request.identifier,
                error = %err,
                status = %status.as_u16(),
                "Failed to resolve client writeback behavior after settings update"
            );
            client_settings_error(status, err.to_string())
        })?;
    let transports = transports_data_from_state(&state);

    let data = crate::api::models::client::ClientSettingsUpdateData {
        created: settings_result.created,
        identifier: request.identifier,
        display_name: state.display_name().to_string(),
        transport,
        client_version: version,
        config_file_state: state.config_file_state(),
        registration_origin: state.registration_origin(),
        runtime_observed: state.runtime_observed(),
        config_path: state.config_path().map(str::to_string),
        transports,
        description: runtime_metadata.description.clone(),
        homepage_url: runtime_metadata.homepage_url.clone(),
        docs_url: runtime_metadata.docs_url.clone(),
        support_url: runtime_metadata.support_url.clone(),
        logo_url: runtime_metadata.logo_url.clone(),
        config_file_parse_effective,
        config_file_parse_override,
        uses_template_parse_default,
        template_merge_strategy: merge_strategy_metadata.template_merge_strategy,
        system_merge_strategy_override: merge_strategy_metadata.system_merge_strategy_override,
        merge_strategy_override: merge_strategy_metadata.merge_strategy_override,
        effective_merge_strategy: merge_strategy_metadata.effective_merge_strategy,
        merge_strategy_source: merge_strategy_metadata.merge_strategy_source,
        supported_merge_strategies: merge_strategy_metadata.supported_merge_strategies,
        setting_sources: crate::api::models::client::ClientSettingsSourceData {
            display_name: settings_result.display_name_source.to_string(),
            approval_status: settings_result.approval_status_source.to_string(),
            config_file_state: settings_result.config_file_state_source.to_string(),
        },
    };

    emit_client_audit_event(
        &app_state,
        AuditAction::ClientSettingsUpdate,
        AuditStatus::Success,
        "/api/client/update",
        &data.identifier,
        Some(data.identifier.clone()),
        Some(json!({
            "transport": data.transport,
            "client_version": data.client_version,
            "display_name": data.display_name,
            "config_file_state": data.config_file_state,
            "config_path": data.config_path,
            "transports": data.transports,
            "config_file_parse_effective": data.config_file_parse_effective,
            "config_file_parse_override": data.config_file_parse_override,
            "merge_strategy_override": data.merge_strategy_override,
            "effective_merge_strategy": data.effective_merge_strategy,
            "merge_strategy_source": data.merge_strategy_source,
        })),
        None,
    )
    .await;

    Ok(Json(crate::api::models::client::ClientSettingsUpdateResp::success(
        data,
    )))
}

pub async fn update_capability_config(
    State(app_state): State<Arc<AppState>>,
    Json(request): Json<ClientCapabilityConfigReq>,
) -> Result<Json<ClientCapabilityConfigResp>, StatusCode> {
    let service = get_client_service(&app_state)?;

    let (state, _visible_surface_changed, materialization) = service
        .update_capability_config_state_and_invalidate(
            &request.identifier,
            request.config_mode,
            request.capability_source,
            request.selected_profile_ids,
            request.unify_direct_exposure.map(Into::into),
            request.source_revision_set.clone().into_iter().collect(),
        )
        .await
        .map_err(|err| {
            tracing::error!(
                client = %request.identifier,
                error = %err,
                "Failed to update capability config"
            );
            map_config_error_status(&err)
        })?;

    let data = build_client_capability_config_data(request.identifier, state, request.source_revision_set);

    // Managed capability changes and revocations are notified through the
    // transactional Surface outbox after runtime identity synchronization.
    emit_client_audit_event(
        &app_state,
        AuditAction::ClientCapabilityUpdate,
        AuditStatus::Success,
        "/api/client/capability-config",
        &data.identifier,
        Some(data.identifier.clone()),
        Some(json!({
            "capability_source": data.capability_source,
            "selected_profile_ids": data.selected_profile_ids,
            "custom_profile_id": data.custom_profile_id,
            "unify_direct_exposure": data.unify_direct_exposure,
        })),
        None,
    )
    .await;
    if let Some(commit) = materialization
        && commit.effective_surface_changed
        && let Some(binding) = commit.binding
    {
        crate::audit::interceptor::emit_event(
            app_state.audit_service.as_ref(),
            AuditEvent::new(AuditAction::SurfacePublish, AuditStatus::Success)
                .with_http_route("POST", "/api/client/capability-config")
                .with_actor("client_management")
                .with_client_id(binding.consumer_id.clone())
                .with_target(binding.active_publication_id)
                .with_data(json!({
                    "binding_generation": binding.generation,
                    "proposal_id": commit.proposal_id,
                    "trigger": "management_save",
                }))
                .build(),
        )
        .await;
    }

    Ok(Json(ClientCapabilityConfigResp::success(data)))
}

pub async fn get_capability_config(
    State(app_state): State<Arc<AppState>>,
    Query(request): Query<ClientConfigReq>,
) -> Result<Json<ClientCapabilityConfigResp>, StatusCode> {
    let service = get_client_service(&app_state)?;

    let config = service
        .get_capability_config_state(&request.identifier)
        .await
        .map_err(|err| {
            tracing::error!(
                client = %request.identifier,
                error = %err,
                "Failed to load capability config"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    let source_revision_set = service
        .catalog_revision_set()
        .await
        .map_err(|error| {
            tracing::error!(
                client = %request.identifier,
                error = %error,
                "Failed to load capability catalog revision set"
            );
            map_config_error_status(&error)
        })?
        .into_iter()
        .collect();

    Ok(Json(ClientCapabilityConfigResp::success(
        build_client_capability_config_data(request.identifier, config, source_revision_set),
    )))
}

/// Handler for POST /api/client/detach
pub async fn client_detach(
    State(app_state): State<Arc<AppState>>,
    Json(request): Json<ClientDetachReq>,
) -> Result<Json<ClientDetachResp>, StatusCode> {
    let service = get_client_service(&app_state)?;

    let existing_state = service
        .fetch_state_repairing_local_target(&request.identifier)
        .await
        .map_err(|err| {
            tracing::error!(client = %request.identifier, error = %err, "Failed to load client state before detach");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let Some(state) = existing_state.as_ref() else {
        return Err(StatusCode::NOT_FOUND);
    };

    let attachment_state = state.attachment_state();
    let mcpmate_present = detect_mcpmate_in_client_config(service.as_ref(), state).await;
    let warnings = client_file_guard_warnings(
        evaluate_client_detach_file_guard(mcpmate_present, attachment_state),
        "detach",
        &request.identifier,
        attachment_state,
    )?;

    let changed = service.detach_client(&request.identifier).await.map_err(|err| {
        let status = map_config_error_status(&err);
        tracing::error!(client = %request.identifier, status = %status.as_u16(), error = %err, "Failed to detach client");
        status
    })?;
    super::manage::invalidate_client_runtime_visibility(&request.identifier).await;

    tracing::info!(client = %request.identifier, changed, "Client detached from MCPMate config");

    emit_client_audit_event(
        &app_state,
        AuditAction::ClientConfigDetach,
        AuditStatus::Success,
        "/api/client/detach",
        &request.identifier,
        Some(request.identifier.clone()),
        Some(json!({
            "detach": true,
            "changed": changed,
            "attachment_state": "detached",
        })),
        None,
    )
    .await;

    Ok(Json(ClientDetachResp::success(ClientDetachData {
        identifier: request.identifier,
        attachment_state: "detached".to_string(),
        changed,
        warnings,
    })))
}

fn resolve_persisted_apply_selection(
    mode: ClientConfigMode,
    cap: &ClientCapabilityConfigState,
) -> Result<ClientConfigSelected, ConfigError> {
    match mode {
        ClientConfigMode::Unify => Ok(ClientConfigSelected::Default),
        ClientConfigMode::Hosted | ClientConfigMode::Transparent => {
            if cap.capability_config.capability_source == CapabilitySource::Custom {
                if cap.custom_profile_missing || cap.capability_config.custom_profile_id.is_none() {
                    return Err(ConfigError::DataAccessError(
                        "Custom workspace profile is missing; fix capability configuration before attaching MCPMate."
                            .to_string(),
                    ));
                }
                Ok(ClientConfigSelected::Profile {
                    profile_id: cap.capability_config.custom_profile_id.clone().unwrap(),
                })
            } else {
                Ok(ClientConfigSelected::Default)
            }
        }
    }
}

fn client_config_mode_from_effective(mode: &str) -> Result<ClientConfigMode, StatusCode> {
    match mode.to_ascii_lowercase().as_str() {
        "unify" => Ok(ClientConfigMode::Unify),
        "hosted" => Ok(ClientConfigMode::Hosted),
        "transparent" => Ok(ClientConfigMode::Transparent),
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

/// Handler for POST /api/client/attach
pub async fn client_attach(
    State(app_state): State<Arc<AppState>>,
    Json(request): Json<ClientAttachReq>,
) -> Result<Json<ClientAttachResp>, StatusCode> {
    let service = get_client_service(&app_state)?;

    let existing_state = service
        .fetch_state_repairing_local_target(&request.identifier)
        .await
        .map_err(|err| {
            tracing::error!(client = %request.identifier, error = %err, "Failed to load client state before attach");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let Some(state) = existing_state.as_ref() else {
        return Err(StatusCode::NOT_FOUND);
    };

    let attachment_state = state.attachment_state();
    let mcpmate_present = detect_mcpmate_in_client_config(service.as_ref(), state).await;
    let warnings = client_file_guard_warnings(
        evaluate_client_attach_file_guard(mcpmate_present, attachment_state),
        "attach",
        &request.identifier,
        attachment_state,
    )?;

    let effective_mode = service.get_effective_config_mode(&request.identifier).await.map_err(|err| {
        tracing::error!(client = %request.identifier, error = %err, "Failed to resolve effective config mode for attach");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let api_mode = client_config_mode_from_effective(&effective_mode)?;

    let cap_state = service
        .get_capability_config_state(&request.identifier)
        .await
        .map_err(|err| {
            tracing::error!(client = %request.identifier, error = %err, "Failed to load capability config for attach");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    let selected_config = resolve_persisted_apply_selection(api_mode.clone(), &cap_state).map_err(|err| {
        tracing::warn!(client = %request.identifier, error = %err, "Invalid capability selection for attach");
        StatusCode::BAD_REQUEST
    })?;

    let apply_request = ClientConfigUpdateReq {
        identifier: request.identifier.clone(),
        mode: api_mode,
        preview: false,
        selected_config,
        backup_policy: None,
    };

    let applied_config = apply_client_config_request(&service, &apply_request, "client_attach", true).await?;
    if !applied_config.data.applied {
        tracing::error!(client = %request.identifier, "client_attach produced no applied write");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    tracing::info!(client = %request.identifier, "Client MCPMate entry re-attached to external config");

    emit_client_audit_event(
        &app_state,
        AuditAction::ClientConfigAttach,
        AuditStatus::Success,
        "/api/client/attach",
        &request.identifier,
        Some(request.identifier.clone()),
        Some(json!({
            "attach": true,
            "applied": applied_config.data.applied,
            "mode": apply_request.mode,
            "selected_config": apply_request.selected_config,
            "attachment_state": "attached",
        })),
        None,
    )
    .await;

    Ok(Json(ClientAttachResp::success(ClientAttachData {
        identifier: request.identifier,
        attachment_state: "attached".to_string(),
        changed: applied_config.data.applied,
        warnings,
    })))
}

async fn descriptor_to_client_info(
    service: &ClientConfigService,
    _app_state: &AppState,
    descriptor: ClientDescriptor,
) -> Result<ClientInfo, StatusCode> {
    let state = descriptor.state.clone();
    let stored_runtime_metadata = state.runtime_client_metadata();
    let runtime_metadata =
        RuntimeClientMetadata::resolve_for_template(&stored_runtime_metadata, descriptor.template.as_ref());
    let (config_file_parse_effective, config_file_parse_override, uses_template_parse_default) =
        build_parse_metadata(&state);
    let transports = transports_data_from_state(&state);
    let identifier = state.identifier().to_string();
    let display_name = state.display_name().to_string();
    let logo_url = runtime_metadata.logo_url.clone();
    let category = runtime_metadata
        .category
        .as_deref()
        .and_then(ClientCategory::parse)
        .unwrap_or_default();
    let description = runtime_metadata.description.clone();
    let homepage_url = runtime_metadata.homepage_url.clone();
    let docs_url = runtime_metadata.docs_url.clone();
    let support_url = runtime_metadata.support_url.clone();
    let config_type = match state.container_type() {
        Some("array") => Some(crate::api::models::client::ClientConfigType::Array),
        Some("object") => Some(crate::api::models::client::ClientConfigType::Standard),
        _ => infer_config_type_from_path(descriptor.config_path.as_deref()),
    };
    let capability_config = service
        .get_capability_config(&identifier)
        .await
        .map_err(|err| {
            tracing::error!(
                client = %identifier,
                error = %err,
                "Failed to load client capability config"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .unwrap_or_default();
    let custom_profile_missing = service
        .get_capability_config_state(&identifier)
        .await
        .map_err(|err| {
            tracing::error!(client = %identifier, error = %err, "Failed to load client capability config state");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .map(|state| state.custom_profile_missing)
        .unwrap_or(false);

    let content = read_descriptor_config_content(service, &descriptor, &identifier).await;

    let config_analysis = inspect_client_config_lenient(content.as_deref(), &state)
        .map(|inspected| inspected.analysis)
        .unwrap_or_default();

    let last_modified = descriptor.config_path.as_deref().and_then(get_config_last_modified);

    let approval_status = Some(state.approval_status().to_string());
    let config_file_state = state.config_file_state();
    let registration_origin = state.registration_origin();
    let runtime_observed = state.runtime_observed();
    let governed_by_default_policy = state.governed_by_default_policy();

    let (config_mode, transport, client_version) = service
        .get_client_settings(state.identifier())
        .await
        .ok()
        .flatten()
        .map(|(mode, transport, version)| (mode, Some(transport), version))
        .unwrap_or((None, Some("auto".to_string()), None));

    let writable_config = service
        .has_verified_local_config_target(state.identifier())
        .await
        .unwrap_or(false);
    let pending_approval = approval_status.as_deref() == Some("pending");
    let attachment_state = derive_attachment_state(&state, &config_analysis);

    Ok(ClientInfo {
        identifier,
        display_name,
        logo_url,
        category,
        enabled: state.is_approved(),
        detected: descriptor.detection.is_some(),
        install_path: None,
        config_path: descriptor.config_path.unwrap_or_default(),
        config_exists: descriptor.config_exists,
        has_mcp_config: config_analysis.has_mcp_config,
        transports,
        description,
        homepage_url,
        docs_url,
        support_url,
        config_mode,
        transport,
        client_version,
        capability_source: capability_config.capability_source,
        selected_profile_ids: capability_config.selected_profile_ids,
        custom_profile_id: capability_config.custom_profile_id,
        custom_profile_missing,
        config_type,
        last_detected: descriptor.detected_at.map(|dt| dt.to_rfc3339()),
        last_modified,
        mcp_servers_count: Some(config_analysis.server_count),
        template: build_template_metadata_from_state(&state, &runtime_metadata),
        approval_status,
        attachment_state: Some(attachment_state),
        config_file_state,
        registration_origin,
        runtime_observed,
        governed_by_default_policy,
        writable_config,
        pending_approval,
        config_file_parse_effective,
        config_file_parse_override,
        uses_template_parse_default,
    })
}

async fn read_descriptor_config_content(
    service: &ClientConfigService,
    descriptor: &ClientDescriptor,
    identifier: &str,
) -> Option<String> {
    if !descriptor.config_exists {
        return None;
    }

    if descriptor.persisted {
        match service.read_current_config(identifier).await {
            Ok(content) => content,
            Err(err) => {
                tracing::warn!(
                    client = %identifier,
                    error = %err,
                    "Continuing list operation despite configuration read failure"
                );
                None
            }
        }
    } else {
        let config_path = descriptor.config_path.as_deref()?;
        match tokio::fs::read_to_string(config_path).await {
            Ok(content) => Some(content),
            Err(err) => {
                tracing::warn!(
                    client = %identifier,
                    path = %config_path,
                    error = %err,
                    "Continuing detect operation despite configuration read failure"
                );
                None
            }
        }
    }
}

// moved to POST /api/client/config/import

fn build_template_metadata_from_state(
    state: &ClientStateRow,
    runtime_metadata: &RuntimeClientMetadata,
) -> ClientTemplateMetadata {
    let container_type = match state.container_type() {
        Some("array") => crate::api::models::client::ClientConfigType::Array,
        _ => crate::api::models::client::ClientConfigType::Standard,
    };

    ClientTemplateMetadata {
        format: state.config_format().unwrap_or("json").to_string(),
        protocol_revision: state.protocol_revision().map(|s| s.to_string()),
        storage: ClientTemplateStorageMetadata {
            kind: state.storage_kind().unwrap_or("file").to_string(),
            path_strategy: state.storage_path_strategy().map(|s| s.to_string()),
        },
        container_type,
        merge_strategy: state.merge_strategy().unwrap_or("replace").to_string(),
        keep_original_config: state.keep_original_config(),
        managed_source: state.managed_source().map(|s| s.to_string()),
        description: runtime_metadata.description.clone(),
        homepage_url: runtime_metadata.homepage_url.clone(),
        docs_url: runtime_metadata.docs_url.clone(),
        support_url: runtime_metadata.support_url.clone(),
    }
}

fn infer_config_type_from_path(config_path: Option<&str>) -> Option<crate::api::models::client::ClientConfigType> {
    infer_format_from_path(config_path).map(|_| crate::api::models::client::ClientConfigType::Standard)
}

async fn read_runtime_config(
    service: &ClientConfigService,
    client_id: &str,
) -> crate::clients::error::ConfigResult<Option<String>> {
    let Some(path) = service.config_path(client_id).await? else {
        return Ok(None);
    };

    match tokio::fs::read_to_string(&path).await {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(ConfigError::IoError(err)),
    }
}

fn build_render_options(request: &ClientConfigUpdateReq) -> ClientRenderOptions {
    let mode = map_mode(request.mode.clone());
    let profile_id = match &request.selected_config {
        ClientConfigSelected::Profile { profile_id } => Some(profile_id.clone()),
        ClientConfigSelected::Default => None,
        ClientConfigSelected::Servers { .. } => None,
    };
    let server_ids = match (&request.selected_config, mode.clone()) {
        (ClientConfigSelected::Servers { server_ids }, ConfigMode::Native) => Some(server_ids.clone()),
        _ => None,
    };

    ClientRenderOptions {
        client_id: request.identifier.clone(),
        mode,
        profile_id,
        server_ids,
        dry_run: request.preview,
    }
}

fn map_mode(mode: ClientConfigMode) -> ConfigMode {
    match mode {
        ClientConfigMode::Unify => ConfigMode::Managed,
        ClientConfigMode::Hosted => ConfigMode::Managed,
        ClientConfigMode::Transparent => ConfigMode::Native,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::handlers::client::inspection::{ATTACH_OVERWRITE_WARNING, DETACH_NO_EFFECT_WARNING};
    use crate::api::models::client::{ClientConfigFileParseData, ClientFormatRuleData, ClientSettingsUpdateReq};
    use crate::api::routes::AppState;
    use crate::clients::models::{ClientConfigFileState, ClientRegistrationOrigin};
    use crate::clients::{
        CapabilitySource,
        source::{ClientConfigSource, DbTemplateSource, FileTemplateSource, TemplateRoot},
    };
    use crate::common::profile::{ProfileRole, ProfileType};
    use crate::config::{
        client::init::{initialize_client_table, initialize_system_settings},
        database::Database,
        models::Profile,
        profile::init::initialize_profile_tables,
        server::init::initialize_server_tables,
    };
    use crate::core::{models::Config, pool::UpstreamConnectionPool, profile::ConfigApplicationStateManager};
    use crate::inspector::{calls::InspectorCallRegistry, sessions::InspectorSessionManager};
    use crate::system::metrics::MetricsCollector;
    use axum::Json;
    use axum::extract::{Query, State};
    use mcpmate_capability_store::{
        CapabilityCatalog, CapabilityKind as CatalogCapabilityKind, CapabilityObservation, CapabilityPayload,
        CatalogRecord, DeclarationState, InventoryState, KindObservation, SqliteCapabilityCatalog,
    };
    use sqlx::sqlite::SqlitePoolOptions;
    use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
    use tempfile::TempDir;
    use tokio::sync::{Mutex, RwLock};

    struct TestContext {
        _temp_dir: TempDir,
        app_state: Arc<AppState>,
        client_service: Arc<ClientConfigService>,
        db_pool: sqlx::SqlitePool,
    }

    async fn create_test_context() -> TestContext {
        create_test_context_with_template_setup(true, |_, _| {}).await
    }

    async fn create_test_context_with_template_setup<F>(
        seed_client_rows: bool,
        setup_templates: F,
    ) -> TestContext
    where
        F: FnOnce(&std::path::Path, &TemplateRoot),
    {
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

        crate::test_helpers::prepare_config_database(&db_pool).await;
        initialize_server_tables(&db_pool).await.expect("init server tables");
        initialize_client_table(&db_pool).await.expect("init client table");
        crate::config::database::initialize_capability_catalog(&db_pool)
            .await
            .expect("init capability catalog");
        initialize_profile_tables(&db_pool).await.expect("init profile tables");
        initialize_system_settings(&db_pool)
            .await
            .expect("init system settings store");

        let database = Arc::new(Database {
            pool: db_pool.clone(),
            path: PathBuf::from(":memory:"),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        });

        let template_root = TemplateRoot::new(temp_dir.path().join("client-templates"));
        template_root.ensure_base_dirs().expect("template dirs");
        setup_templates(temp_dir.path(), &template_root);
        let template_source = Arc::new(
            FileTemplateSource::bootstrap(template_root)
                .await
                .expect("template source"),
        );
        ClientConfigService::seed_runtime_template_snapshots(&db_pool, template_source.as_ref())
            .await
            .expect("seed runtime templates");
        if seed_client_rows {
            ClientConfigService::seed_client_runtime_rows(&db_pool, template_source.as_ref())
                .await
                .expect("seed runtime rows");
        }
        let runtime_source: Arc<dyn ClientConfigSource> =
            Arc::new(DbTemplateSource::new(Arc::new(db_pool.clone())).expect("runtime source"));
        let client_service = Arc::new(
            ClientConfigService::with_source(Arc::new(db_pool.clone()), runtime_source)
                .await
                .expect("client service"),
        );
        crate::core::capability::naming::initialize(db_pool.clone());

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
            client_service: Some(client_service.clone()),
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
            client_service,
            db_pool,
        }
    }

    fn settings_update_req(identifier: &str) -> ClientSettingsUpdateReq {
        ClientSettingsUpdateReq {
            identifier: identifier.to_string(),
            transport: None,
            client_version: None,
            display_name: None,
            config_file_state: None,
            config_path: None,
            description: None,
            homepage_url: None,
            docs_url: None,
            support_url: None,
            logo_url: None,
            config_file_parse: None,
            clear_config_file_parse: false,
            merge_strategy_override: None,
            clear_merge_strategy_override: false,
            transports: None,
            clear_transports: false,
        }
    }

    async fn displayed_catalog_revision_set(context: &TestContext) -> crate::api::models::CatalogRevisionSet {
        context
            .client_service
            .catalog_revision_set()
            .await
            .expect("load displayed catalog revisions")
            .into_iter()
            .collect()
    }

    fn json_object_parse(container_key: &str) -> ClientConfigFileParseData {
        ClientConfigFileParseData {
            format: "json".to_string(),
            container_type: crate::api::models::client::ClientConfigType::Standard,
            container_keys: vec![container_key.to_string()],
        }
    }

    fn streamable_http_rule() -> ClientFormatRuleData {
        ClientFormatRuleData {
            include_type: true,
            type_value: Some("streamable_http".to_string()),
            url_field: Some("url".to_string()),
            headers_field: Some("headers".to_string()),
            ..Default::default()
        }
    }

    fn selected_streamable_http_rule() -> ClientFormatRuleData {
        ClientFormatRuleData {
            url_field: Some("url".to_string()),
            selected: Some(true),
            ..Default::default()
        }
    }

    async fn seed_runtime_attachable_client(
        context: &TestContext,
        identifier: &str,
        config_path: &std::path::Path,
    ) -> serde_json::Value {
        use crate::api::models::client::ClientBackupPolicyPayload;

        tokio::fs::write(config_path, "{}")
            .await
            .expect("seed runtime client config file");

        let Json(update_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("streamable_http".to_string()),
                display_name: Some("Runtime Attach Client".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                config_file_parse: Some(json_object_parse("mcpServers")),
                transports: Some(HashMap::from([("streamable_http".to_string(), streamable_http_rule())])),
                ..settings_update_req(identifier)
            }),
        )
        .await
        .expect("update runtime attachable client");

        assert!(update_response.success);

        let Json(apply_response) = config_apply(
            State(context.app_state.clone()),
            Json(ClientConfigUpdateReq {
                identifier: identifier.to_string(),
                mode: ClientConfigMode::Hosted,
                preview: false,
                selected_config: ClientConfigSelected::Default,
                backup_policy: Some(ClientBackupPolicyPayload {
                    policy: "off".to_string(),
                    limit: None,
                }),
            }),
        )
        .await
        .expect("apply runtime attachable client config");

        assert!(apply_response.success);
        assert!(apply_response.data.expect("apply data").applied);

        let written = tokio::fs::read_to_string(config_path)
            .await
            .expect("read applied config");
        serde_json::from_str(&written).expect("parse applied config")
    }

    #[tokio::test]
    async fn detect_returns_detected_clients_without_persisting_records() {
        let context = create_test_context_with_template_setup(false, |temp_path, template_root| {
            let config_path = temp_path.join("detected-client.json");
            std::fs::write(
                &config_path,
                r#"{"mcpServers":{"Detected Server":{"command":"node","args":["server.js"]}}}"#,
            )
            .expect("write detected client config");

            let template = format!(
                r#"{{
                  identifier: "test.detected",
                  display_name: "Detected Test",
                  format: "json",
                  storage: {{ kind: "file", path_strategy: "config_path" }},
                  detection: {{
                    macos: [{{ method: "config_path", value: "{}" }}],
                    linux: [{{ method: "config_path", value: "{}" }}],
                    windows: [{{ method: "config_path", value: "{}" }}],
                  }},
                  config_mapping: {{
                    container_keys: ["mcpServers"],
                    container_type: "object_map",
                    merge_strategy: "replace",
                    format_rules: {{
                      stdio: {{
                        template: {{
                          command: "{{{{command}}}}",
                          args: "{{{{{{json args}}}}}}",
                          env: "{{{{{{json env}}}}}}",
                        }},
                        requires_type_field: false,
                      }},
                    }},
                  }},
                }}"#,
                config_path.display(),
                config_path.display(),
                config_path.display(),
            );
            std::fs::write(template_root.official_dir().join("test_detected.json5"), template)
                .expect("write detected client template");
        })
        .await;

        let Json(response) = detect(
            State(context.app_state.clone()),
            Query(ClientDetectReq { refresh: true }),
        )
        .await
        .expect("detect clients");

        assert!(response.success);
        let clients = response.data.expect("detect data").client;
        let detected = clients
            .iter()
            .find(|client| client.identifier == "test.detected")
            .expect("detected client should be returned");

        assert!(detected.detected);
        assert_eq!(detected.mcp_servers_count, Some(1));
        assert!(
            context
                .client_service
                .fetch_state("test.detected")
                .await
                .expect("fetch state")
                .is_none()
        );
    }

    async fn insert_shared_profile(
        pool: &sqlx::SqlitePool,
        name: &str,
    ) -> String {
        let profile = Profile::new(name.to_string(), ProfileType::Shared);
        crate::test_helpers::insert_profile(pool, &profile).await
    }

    async fn insert_active_shared_profile(
        pool: &sqlx::SqlitePool,
        name: &str,
    ) -> String {
        let mut profile = Profile::new(name.to_string(), ProfileType::Shared);
        profile.is_active = true;
        profile.is_default = true;
        crate::test_helpers::insert_profile(pool, &profile).await
    }

    async fn insert_unify_server(
        pool: &sqlx::SqlitePool,
        id: &str,
        name: &str,
        eligible: bool,
        tool_names: &[&str],
    ) {
        crate::core::capability::naming::initialize(pool.clone());
        sqlx::query(
            r#"
            INSERT INTO server_config (id, name, server_type, command, enabled, unify_direct_exposure_eligible)
            VALUES (?, ?, 'stdio', 'server-binary', 1, ?)
            "#,
        )
        .bind(id)
        .bind(name)
        .bind(if eligible { 1 } else { 0 })
        .execute(pool)
        .await
        .expect("insert unify server");

        for tool_name in tool_names {
            crate::config::server::tools::upsert_server_tool(pool, id, name, tool_name, Some("tool"))
                .await
                .expect("insert server tool");
        }
        commit_tool_catalog(pool, id, name, tool_names).await;
    }

    async fn set_test_server_enabled(
        pool: &sqlx::SqlitePool,
        server_id: &str,
        enabled: bool,
    ) {
        crate::core::capability::management::ServerSurfaceManagement::set_server_enabled(
            pool, server_id, enabled, "test",
        )
        .await
        .expect("set server global status");
    }

    async fn set_test_server_direct_exposure_eligible(
        pool: &sqlx::SqlitePool,
        server_id: &str,
        eligible: bool,
    ) {
        let revisions = crate::core::capability::materializer::SurfaceAuthoringLoader::load_catalog_revision_set(pool)
            .await
            .expect("load catalog revision set");
        crate::core::capability::management::ServerSurfaceManagement::set_direct_exposure_eligible(
            pool, server_id, eligible, revisions, "test",
        )
        .await
        .expect("set server direct exposure eligibility");
    }

    async fn commit_tool_catalog(
        pool: &sqlx::SqlitePool,
        server_id: &str,
        server_name: &str,
        tool_names: &[&str],
    ) {
        let records = tool_names
            .iter()
            .map(|tool_name| {
                let tool: rmcp::model::Tool = serde_json::from_value(serde_json::json!({
                    "name": tool_name,
                    "description": "tool",
                    "inputSchema": {"type": "object"}
                }))
                .expect("build Tool");
                let external_name = format!("{}_{}", server_name, tool_name).replace('-', "_");
                CatalogRecord::materialize(server_id, *tool_name, external_name, CapabilityPayload::Tool(tool))
                    .expect("materialize Tool")
            })
            .collect();
        let initialize_result: rmcp::model::InitializeResult = serde_json::from_value(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": server_name, "version": "1.0.0"}
        }))
        .expect("build InitializeResult");
        SqliteCapabilityCatalog::new(pool.clone())
            .commit_observation(CapabilityObservation::new(
                server_id,
                server_name,
                "test-config",
                initialize_result,
                vec![KindObservation::new(
                    CatalogCapabilityKind::Tools,
                    DeclarationState::Supported,
                    InventoryState::Complete,
                )],
                records,
            ))
            .await
            .expect("commit Tool catalog observation");
    }

    fn capability_ref(
        server_id: &str,
        kind: CatalogCapabilityKind,
        origin_key: &str,
    ) -> String {
        mcpmate_capability_store::CapabilityRefId::derive(&mcpmate_capability_store::CapabilitySourceIdentity::new(
            server_id, kind, origin_key,
        ))
        .expect("derive CapabilityRef")
        .to_string()
    }

    async fn insert_unify_non_tool_capabilities(
        pool: &sqlx::SqlitePool,
        server_id: &str,
        server_name: &str,
        prompt_names: &[&str],
        resource_uris: &[&str],
        template_uris: &[&str],
    ) {
        crate::core::capability::naming::initialize(pool.clone());
        for prompt_name in prompt_names {
            crate::config::server::capabilities::upsert_shadow_prompt(pool, server_id, server_name, prompt_name, None)
                .await
                .expect("insert server prompt");
        }

        for resource_uri in resource_uris {
            crate::config::server::capabilities::upsert_shadow_resource(
                pool,
                server_id,
                server_name,
                resource_uri,
                Some(resource_uri),
                None,
                None,
            )
            .await
            .expect("insert server resource");
        }

        for template_uri in template_uris {
            crate::config::server::capabilities::upsert_shadow_resource_template(
                pool,
                server_id,
                server_name,
                template_uri,
                Some(template_uri),
                None,
            )
            .await
            .expect("insert server resource template");
        }
        let mut records = Vec::new();
        for prompt_name in prompt_names {
            let prompt: rmcp::model::Prompt = serde_json::from_value(serde_json::json!({
                "name": prompt_name,
                "description": "prompt",
                "arguments": []
            }))
            .expect("build Prompt");
            records.push(
                CatalogRecord::materialize(
                    server_id,
                    *prompt_name,
                    format!("{server_name}_{prompt_name}"),
                    CapabilityPayload::Prompt(prompt),
                )
                .expect("materialize Prompt"),
            );
        }
        for resource_uri in resource_uris {
            let resource: rmcp::model::Resource = serde_json::from_value(serde_json::json!({
                "uri": resource_uri,
                "name": resource_uri
            }))
            .expect("build Resource");
            records.push(
                CatalogRecord::materialize(
                    server_id,
                    *resource_uri,
                    canonical_test_resource(server_name),
                    CapabilityPayload::Resource(resource),
                )
                .expect("materialize Resource"),
            );
        }
        for template_uri in template_uris {
            let template: rmcp::model::ResourceTemplate = serde_json::from_value(serde_json::json!({
                "uriTemplate": template_uri,
                "name": template_uri
            }))
            .expect("build Resource Template");
            records.push(
                CatalogRecord::materialize(
                    server_id,
                    *template_uri,
                    canonical_test_template(server_name),
                    CapabilityPayload::ResourceTemplate(template),
                )
                .expect("materialize Resource Template"),
            );
        }
        let initialize_result: rmcp::model::InitializeResult = serde_json::from_value(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {
                "prompts": {},
                "resources": {}
            },
            "serverInfo": {"name": server_name, "version": "1.0.0"}
        }))
        .expect("build InitializeResult");
        SqliteCapabilityCatalog::new(pool.clone())
            .commit_observation(CapabilityObservation::new(
                server_id,
                server_name,
                "test-config",
                initialize_result,
                vec![
                    KindObservation::new(
                        CatalogCapabilityKind::Prompts,
                        DeclarationState::Supported,
                        InventoryState::Complete,
                    ),
                    KindObservation::new(
                        CatalogCapabilityKind::Resources,
                        DeclarationState::Supported,
                        InventoryState::Complete,
                    ),
                    KindObservation::new(
                        CatalogCapabilityKind::ResourceTemplates,
                        DeclarationState::Supported,
                        InventoryState::Complete,
                    ),
                ],
                records,
            ))
            .await
            .expect("commit non-Tool catalog observation");
    }

    fn canonical_test_resource(namespace: &str) -> String {
        crate::core::capability::resource_uri::encode_resource_uri(namespace, "test://resource-a")
            .expect("encode test resource")
    }

    fn canonical_test_template(namespace: &str) -> String {
        crate::core::capability::resource_uri::encode_resource_template(namespace, "test:///{id}")
            .expect("encode test resource template")
    }

    #[tokio::test]
    async fn update_capability_config_returns_updated_payload() {
        let context = create_test_context().await;
        let profile_id = insert_shared_profile(&context.db_pool, "profile-a").await;
        crate::config::client::init::set_default_client_config_mode(&context.db_pool, "hosted")
            .await
            .expect("set inherited managed mode");
        context
            .client_service
            .set_capability_config("client-a", CapabilitySource::Activated, Vec::new())
            .await
            .expect("create client capability state");
        sqlx::query("UPDATE client SET config_mode = NULL, approval_status = 'approved' WHERE identifier = 'client-a'")
            .execute(&context.db_pool)
            .await
            .expect("mark fixture as managed Consumer");

        let Json(response) = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-a".to_string(),
                config_mode: None,
                capability_source: CapabilitySource::Profiles,
                selected_profile_ids: vec![profile_id.clone()],
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: None,
            }),
        )
        .await
        .expect("update capability config");

        assert!(response.success);
        let data = response.data.expect("response data");
        assert_eq!(data.identifier, "client-a");
        assert_eq!(data.capability_source, CapabilitySource::Profiles);
        assert_eq!(data.selected_profile_ids, vec![profile_id.clone()]);
        assert!(data.custom_profile_id.is_none());
        assert_eq!(
            data.unify_direct_exposure.intent.route_mode,
            crate::clients::models::UnifyRouteMode::BrokerOnly
        );

        let stored = context
            .client_service
            .get_capability_config("client-a")
            .await
            .expect("stored config")
            .expect("stored data");
        assert_eq!(stored.capability_source, CapabilitySource::Profiles);
        assert_eq!(stored.selected_profile_ids, vec![profile_id]);
        let consumer_id: String = sqlx::query_scalar("SELECT identifier FROM client WHERE identifier = 'client-a'")
            .fetch_one(&context.db_pool)
            .await
            .expect("load Consumer id");
        assert!(
            mcpmate_capability_store::SqliteSurfaceStore::new(context.db_pool.clone())
                .load_binding(&consumer_id)
                .await
                .expect("load initial binding")
                .is_some(),
            "managed capability save must publish an initial Surface in the same operation"
        );
    }

    #[tokio::test]
    async fn get_capability_config_returns_stored_custom_profile() {
        let context = create_test_context().await;
        let config = context
            .client_service
            .set_capability_config("client-a", CapabilitySource::Custom, Vec::new())
            .await
            .expect("seed custom capability config");

        let Json(response) = get_capability_config(
            State(context.app_state.clone()),
            Query(ClientConfigReq {
                identifier: "client-a".to_string(),
            }),
        )
        .await
        .expect("get capability config");

        assert!(response.success);
        let data = response.data.expect("response data");
        assert_eq!(data.identifier, "client-a");
        assert_eq!(data.capability_source, CapabilitySource::Custom);
        assert!(data.selected_profile_ids.is_empty());
        assert_eq!(data.custom_profile_id, config.custom_profile_id);
        assert_eq!(
            data.unify_direct_exposure.intent.route_mode,
            crate::clients::models::UnifyRouteMode::BrokerOnly
        );
    }

    #[tokio::test]
    async fn capability_config_roundtrips_unify_route_only() {
        let context = create_test_context().await;

        let Json(response) = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-route-only".to_string(),
                config_mode: None,
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: Some(crate::api::models::client::ClientUnifyDirectExposureReq {
                    route_mode: crate::clients::models::UnifyRouteMode::BrokerOnly,
                    server_ids: Vec::new(),
                    capability_refs: Default::default(),
                }),
            }),
        )
        .await
        .expect("update route-only capability config");

        let data = response.data.expect("response data");
        assert_eq!(
            data.unify_direct_exposure.intent.route_mode,
            crate::clients::models::UnifyRouteMode::BrokerOnly
        );
        assert!(data.unify_direct_exposure.intent.server_ids.is_empty());
        assert!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_tool_surfaces
                .is_empty()
        );
        assert!(data.unify_direct_exposure.diagnostics.invalid_server_ids.is_empty());
        assert!(data.unify_direct_exposure.diagnostics.invalid_tool_surfaces.is_empty());
    }

    #[tokio::test]
    async fn capability_config_roundtrips_empty_unify_direct_route_modes() {
        let context = create_test_context().await;

        for (identifier, route_mode) in [
            (
                "client-empty-server-level",
                crate::clients::models::UnifyRouteMode::ServerLevel,
            ),
            (
                "client-empty-capability-level",
                crate::clients::models::UnifyRouteMode::CapabilityLevel,
            ),
        ] {
            let _ = update_capability_config(
                State(context.app_state.clone()),
                Json(ClientCapabilityConfigReq {
                    identifier: identifier.to_string(),
                    config_mode: Some("unify".to_string()),
                    capability_source: CapabilitySource::Activated,
                    selected_profile_ids: Vec::new(),
                    source_revision_set: displayed_catalog_revision_set(&context).await,
                    unify_direct_exposure: Some(crate::api::models::client::ClientUnifyDirectExposureReq {
                        route_mode,
                        server_ids: Vec::new(),
                        capability_refs: Default::default(),
                    }),
                }),
            )
            .await
            .expect("update empty unify direct route mode");

            let Json(response) = get_capability_config(
                State(context.app_state.clone()),
                Query(ClientConfigReq {
                    identifier: identifier.to_string(),
                }),
            )
            .await
            .expect("reload empty unify direct route mode");

            let data = response.data.expect("response data");
            assert_eq!(data.unify_direct_exposure.intent.route_mode, route_mode);
            assert!(data.unify_direct_exposure.intent.server_ids.is_empty());
            assert!(data.unify_direct_exposure.intent.capability_refs.is_empty());
        }
    }

    #[tokio::test]
    async fn capability_config_roundtrips_unify_server_level_selection() {
        let context = create_test_context().await;
        insert_unify_server(
            &context.db_pool,
            "server-eligible",
            "eligible_server",
            true,
            &["tool-a", "tool-b"],
        )
        .await;
        let Json(response) = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-server-level".to_string(),
                config_mode: None,
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: Some(crate::api::models::client::ClientUnifyDirectExposureReq {
                    route_mode: crate::clients::models::UnifyRouteMode::ServerLevel,
                    server_ids: vec!["server-eligible".to_string()],
                    capability_refs: Default::default(),
                }),
            }),
        )
        .await
        .expect("update server-level capability config");

        let data = response.data.expect("response data");
        assert_eq!(
            data.unify_direct_exposure.intent.route_mode,
            crate::clients::models::UnifyRouteMode::ServerLevel
        );
        assert_eq!(
            data.unify_direct_exposure.intent.server_ids,
            vec!["server-eligible".to_string()]
        );
        assert_eq!(
            data.unify_direct_exposure.resolved_capabilities.selected_tool_surfaces,
            vec![
                crate::clients::models::UnifyDirectToolSurface {
                    server_id: "server-eligible".to_string(),
                    tool_name: "tool-a".to_string(),
                },
                crate::clients::models::UnifyDirectToolSurface {
                    server_id: "server-eligible".to_string(),
                    tool_name: "tool-b".to_string(),
                },
            ]
        );
        assert!(data.unify_direct_exposure.diagnostics.invalid_server_ids.is_empty());

        let stored_servers: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT des.server_id
            FROM direct_exposure_servers des
            JOIN client c ON c.identifier = des.consumer_id
            WHERE c.identifier = ?
            ORDER BY des.server_id
            "#,
        )
        .bind("client-server-level")
        .fetch_all(&context.db_pool)
        .await
        .expect("load stored Direct Exposure servers");
        assert_eq!(stored_servers, ["server-eligible"]);

        crate::config::server::tools::upsert_server_tool(
            &context.db_pool,
            "server-eligible",
            "eligible_server",
            "tool-c",
            Some("tool"),
        )
        .await
        .expect("insert refreshed server tool");
        commit_tool_catalog(
            &context.db_pool,
            "server-eligible",
            "eligible_server",
            &["tool-a", "tool-b", "tool-c"],
        )
        .await;

        let Json(response) = get_capability_config(
            State(context.app_state.clone()),
            Query(ClientConfigReq {
                identifier: "client-server-level".to_string(),
            }),
        )
        .await
        .expect("reload server-level capability config after inventory refresh");

        let data = response.data.expect("response data");
        assert_eq!(
            data.unify_direct_exposure.intent.server_ids,
            vec!["server-eligible".to_string()]
        );
        assert!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_tool_surfaces
                .iter()
                .any(|surface| surface.server_id == "server-eligible" && surface.tool_name == "tool-c")
        );
    }

    #[tokio::test]
    async fn initial_unify_server_level_selection_survives_late_server_enable() {
        let context = create_test_context().await;
        insert_unify_server(
            &context.db_pool,
            "server-late-enable",
            "late_enable_server",
            true,
            &["tool-a"],
        )
        .await;
        sqlx::query("UPDATE server_config SET enabled = 0 WHERE id = ?")
            .bind("server-late-enable")
            .execute(&context.db_pool)
            .await
            .expect("disable server before initial selection");

        let stale = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-late-server-level".to_string(),
                config_mode: Some("unify".to_string()),
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: Default::default(),
                unify_direct_exposure: Some(crate::api::models::client::ClientUnifyDirectExposureReq {
                    route_mode: crate::clients::models::UnifyRouteMode::ServerLevel,
                    server_ids: vec!["server-late-enable".to_string()],
                    capability_refs: Default::default(),
                }),
            }),
        )
        .await;
        assert_eq!(stale.unwrap_err(), StatusCode::CONFLICT);
        let stale_relationship_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM direct_exposure_servers WHERE consumer_id = (SELECT identifier FROM client WHERE identifier = 'client-late-server-level')",
        )
        .fetch_one(&context.db_pool)
        .await
        .expect("count stale direct exposure writes");
        assert_eq!(stale_relationship_count, 0);
        let stale_consumer_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM client WHERE identifier = 'client-late-server-level'")
                .fetch_one(&context.db_pool)
                .await
                .expect("count stale Consumer writes");
        assert_eq!(stale_consumer_count, 0);

        let Json(response) = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-late-server-level".to_string(),
                config_mode: None,
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: context
                    .client_service
                    .catalog_revision_set()
                    .await
                    .expect("load displayed catalog revisions")
                    .into_iter()
                    .collect(),
                unify_direct_exposure: Some(crate::api::models::client::ClientUnifyDirectExposureReq {
                    route_mode: crate::clients::models::UnifyRouteMode::ServerLevel,
                    server_ids: vec!["server-late-enable".to_string()],
                    capability_refs: Default::default(),
                }),
            }),
        )
        .await
        .expect("store server-level intent before server is enabled");

        let data = response.data.expect("response data");
        assert!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_tool_surfaces
                .is_empty()
        );

        let stored_servers: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT des.server_id
            FROM direct_exposure_servers des
            JOIN client c ON c.identifier = des.consumer_id
            WHERE c.identifier = ?
            ORDER BY des.server_id
            "#,
        )
        .bind("client-late-server-level")
        .fetch_all(&context.db_pool)
        .await
        .expect("load stored Direct Exposure servers");
        assert_eq!(stored_servers, ["server-late-enable"]);

        sqlx::query("UPDATE server_config SET enabled = 1 WHERE id = ?")
            .bind("server-late-enable")
            .execute(&context.db_pool)
            .await
            .expect("enable server after initial selection");

        let reconciled = context
            .client_service
            .reconcile_unify_direct_exposure_for_server("server-late-enable")
            .await
            .expect("reconcile direct exposure after enable");
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].identifier, "client-late-server-level");
        assert_eq!(
            reconciled[0].unify_direct_exposure.selected_tool_surfaces,
            vec![crate::clients::models::UnifyDirectToolSurface {
                server_id: "server-late-enable".to_string(),
                tool_name: "tool-a".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn stale_custom_save_leaves_no_consumer_or_custom_profile() {
        let context = create_test_context().await;
        insert_unify_server(
            &context.db_pool,
            "server-custom-stale",
            "custom_stale_server",
            true,
            &["tool-a"],
        )
        .await;

        let result = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-custom-stale".to_string(),
                config_mode: Some("hosted".to_string()),
                capability_source: CapabilitySource::Custom,
                selected_profile_ids: Vec::new(),
                source_revision_set: Default::default(),
                unify_direct_exposure: None,
            }),
        )
        .await;

        assert_eq!(result.unwrap_err(), StatusCode::CONFLICT);
        let consumer_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM client WHERE identifier = 'client-custom-stale'")
                .fetch_one(&context.db_pool)
                .await
                .expect("count stale custom Consumer writes");
        let profile_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM profile WHERE name = 'client-custom-stale_custom'")
                .fetch_one(&context.db_pool)
                .await
                .expect("count stale custom Profile writes");
        assert_eq!(consumer_count, 0);
        assert_eq!(profile_count, 0);
    }

    #[tokio::test]
    async fn capability_config_roundtrips_unify_tool_selection() {
        let context = create_test_context().await;
        insert_unify_server(
            &context.db_pool,
            "server-tools",
            "tool_server",
            true,
            &["tool-a", "tool-b"],
        )
        .await;
        let tool_ref = capability_ref("server-tools", CatalogCapabilityKind::Tools, "tool-b");

        let Json(response) = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-tool-live".to_string(),
                config_mode: None,
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: Some(crate::api::models::client::ClientUnifyDirectExposureReq {
                    route_mode: crate::clients::models::UnifyRouteMode::CapabilityLevel,
                    server_ids: Vec::new(),
                    capability_refs: crate::clients::models::UnifyDirectCapabilityRefs {
                        tool_refs: vec![tool_ref.clone()],
                        ..Default::default()
                    },
                }),
            }),
        )
        .await
        .expect("update capability-level config");

        let data = response.data.expect("response data");
        assert_eq!(
            data.unify_direct_exposure.resolved_capabilities.selected_tool_surfaces,
            vec![crate::clients::models::UnifyDirectToolSurface {
                server_id: "server-tools".to_string(),
                tool_name: "tool-b".to_string(),
            }]
        );
        assert!(data.unify_direct_exposure.diagnostics.invalid_tool_surfaces.is_empty());
        assert_eq!(data.unify_direct_exposure.intent.capability_refs.tool_refs, [tool_ref]);
    }

    #[tokio::test]
    async fn capability_level_direct_intent_survives_complete_removal() {
        let context = create_test_context().await;
        insert_unify_server(
            &context.db_pool,
            "server-unresolved",
            "unresolved_server",
            true,
            &["tool-a"],
        )
        .await;
        let tool_ref = capability_ref("server-unresolved", CatalogCapabilityKind::Tools, "tool-a");
        let Json(_) = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-direct-unresolved".to_string(),
                config_mode: None,
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: Some(crate::api::models::client::ClientUnifyDirectExposureReq {
                    route_mode: crate::clients::models::UnifyRouteMode::CapabilityLevel,
                    server_ids: Vec::new(),
                    capability_refs: crate::clients::models::UnifyDirectCapabilityRefs {
                        tool_refs: vec![tool_ref.clone()],
                        ..Default::default()
                    },
                }),
            }),
        )
        .await
        .expect("persist Direct Exposure CapabilityRef");

        commit_tool_catalog(&context.db_pool, "server-unresolved", "unresolved_server", &[]).await;

        let Json(response) = get_capability_config(
            State(context.app_state.clone()),
            Query(ClientConfigReq {
                identifier: "client-direct-unresolved".to_string(),
            }),
        )
        .await
        .expect("reload unresolved Direct Exposure intent");
        let data = response.data.expect("response data");
        assert_eq!(
            data.unify_direct_exposure.intent.capability_refs.tool_refs,
            std::slice::from_ref(&tool_ref)
        );
        assert!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_tool_surfaces
                .is_empty()
        );
        assert_eq!(
            data.unify_direct_exposure.diagnostics.invalid_capability_refs,
            std::slice::from_ref(&tool_ref)
        );
        let stored_ref: String = sqlx::query_scalar(
            r#"
            SELECT der.ref_id
            FROM direct_exposure_refs der
            JOIN client c ON c.identifier = der.consumer_id
            WHERE c.identifier = ?
            "#,
        )
        .bind("client-direct-unresolved")
        .fetch_one(&context.db_pool)
        .await
        .expect("load persisted Direct Exposure ref");
        assert_eq!(stored_ref, tool_ref);
    }

    #[tokio::test]
    async fn capability_config_roundtrips_unify_prompt_resource_and_template_selection() {
        let context = create_test_context().await;
        insert_unify_server(&context.db_pool, "server-mixed", "mixed_server", true, &["tool-a"]).await;
        insert_unify_non_tool_capabilities(
            &context.db_pool,
            "server-mixed",
            "mixed_server",
            &["prompt-a"],
            &["test://resource-a"],
            &["test:///{id}"],
        )
        .await;
        let prompt_ref = capability_ref("server-mixed", CatalogCapabilityKind::Prompts, "prompt-a");
        let resource_ref = capability_ref("server-mixed", CatalogCapabilityKind::Resources, "test://resource-a");
        let template_ref = capability_ref("server-mixed", CatalogCapabilityKind::ResourceTemplates, "test:///{id}");

        let Json(response) = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-mixed-live".to_string(),
                config_mode: None,
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: Some(crate::api::models::client::ClientUnifyDirectExposureReq {
                    route_mode: crate::clients::models::UnifyRouteMode::CapabilityLevel,
                    server_ids: Vec::new(),
                    capability_refs: crate::clients::models::UnifyDirectCapabilityRefs {
                        prompt_refs: vec![prompt_ref],
                        resource_refs: vec![resource_ref],
                        template_refs: vec![template_ref],
                        ..Default::default()
                    },
                }),
            }),
        )
        .await
        .expect("update mixed capability-level config");

        let data = response.data.expect("response data");
        assert_eq!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_prompt_surfaces,
            vec![crate::clients::models::UnifyDirectPromptSurface {
                server_id: "server-mixed".to_string(),
                prompt_name: "prompt-a".to_string(),
            }]
        );
        assert_eq!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_resource_surfaces,
            vec![crate::clients::models::UnifyDirectResourceSurface {
                server_id: "server-mixed".to_string(),
                resource_uri: "test://resource-a".to_string(),
            }]
        );
        assert_eq!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_template_surfaces,
            vec![crate::clients::models::UnifyDirectTemplateSurface {
                server_id: "server-mixed".to_string(),
                uri_template: "test:///{id}".to_string(),
            }]
        );
        assert!(
            data.unify_direct_exposure
                .diagnostics
                .invalid_prompt_surfaces
                .is_empty()
        );
        assert!(
            data.unify_direct_exposure
                .diagnostics
                .invalid_resource_surfaces
                .is_empty()
        );
        assert!(
            data.unify_direct_exposure
                .diagnostics
                .invalid_template_surfaces
                .is_empty()
        );
    }

    #[tokio::test]
    async fn capability_config_unify_tool_selection_uses_globally_enabled_servers_for_activated_mode() {
        let context = create_test_context().await;
        insert_active_shared_profile(&context.db_pool, "active-profile-without-target-server").await;
        insert_unify_server(
            &context.db_pool,
            "server-global-only",
            "global_only_server",
            true,
            &["tool-a", "tool-b"],
        )
        .await;
        let tool_ref = capability_ref("server-global-only", CatalogCapabilityKind::Tools, "tool-b");

        let Json(response) = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-tool-live-global-enabled".to_string(),
                config_mode: None,
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: Some(crate::api::models::client::ClientUnifyDirectExposureReq {
                    route_mode: crate::clients::models::UnifyRouteMode::CapabilityLevel,
                    server_ids: Vec::new(),
                    capability_refs: crate::clients::models::UnifyDirectCapabilityRefs {
                        tool_refs: vec![tool_ref],
                        ..Default::default()
                    },
                }),
            }),
        )
        .await
        .expect("update capability-level config against globally enabled server");

        let data = response.data.expect("response data");
        assert_eq!(
            data.unify_direct_exposure.resolved_capabilities.selected_tool_surfaces,
            vec![crate::clients::models::UnifyDirectToolSurface {
                server_id: "server-global-only".to_string(),
                tool_name: "tool-b".to_string(),
            }]
        );
        assert!(data.unify_direct_exposure.diagnostics.invalid_server_ids.is_empty());
        assert!(data.unify_direct_exposure.diagnostics.invalid_tool_surfaces.is_empty());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn hosted_source_switch_without_surface_delta_does_not_enqueue_notification() {
        let context = create_test_context().await;
        let profile = Profile {
            id: Some("PROFHOSTED001".to_string()),
            name: "Hosted Profile".to_string(),
            description: None,
            profile_type: ProfileType::Shared,
            role: ProfileRole::User,
            priority: 0,
            is_active: false,
            is_default: false,
            authoring_generation: 0,
            profile_mode: crate::config::models::ProfileMode::Capability,
            created_at: None,
            updated_at: None,
        };
        crate::test_helpers::insert_profile(&context.db_pool, &profile).await;
        let _ = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-a".to_string(),
                config_mode: Some("hosted".to_string()),
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: None,
            }),
        )
        .await
        .expect("seed hosted mode through atomic capability configuration");

        let before_outbox: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM surface_outbox_events WHERE aggregate_id = 'client-a' AND delivered_at IS NULL",
        )
        .fetch_one(&context.db_pool)
        .await
        .expect("count pending Surface notifications");
        let Json(response) = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-a".to_string(),
                config_mode: None,
                capability_source: CapabilitySource::Profiles,
                selected_profile_ids: vec!["PROFHOSTED001".to_string()],
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: None,
            }),
        )
        .await
        .expect("switch hosted capability source");

        assert!(response.success);
        let after_outbox: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM surface_outbox_events WHERE aggregate_id = 'client-a' AND delivered_at IS NULL",
        )
        .fetch_one(&context.db_pool)
        .await
        .expect("count pending Surface notifications");
        assert_eq!(after_outbox, before_outbox);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn managed_mode_switch_without_surface_delta_does_not_enqueue_notification() {
        let context = create_test_context().await;
        let _ = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-a".to_string(),
                config_mode: Some("unify".to_string()),
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: None,
            }),
        )
        .await
        .expect("seed unify mode through atomic capability configuration");

        let before_outbox: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM surface_outbox_events WHERE aggregate_id = 'client-a' AND delivered_at IS NULL",
        )
        .fetch_one(&context.db_pool)
        .await
        .expect("count pending Surface notifications");
        let Json(response) = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-a".to_string(),
                config_mode: Some("hosted".to_string()),
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: None,
            }),
        )
        .await
        .expect("switch mode to hosted");

        assert!(response.success);
        let after_outbox: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM surface_outbox_events WHERE aggregate_id = 'client-a' AND delivered_at IS NULL",
        )
        .fetch_one(&context.db_pool)
        .await
        .expect("count pending Surface notifications");
        assert_eq!(after_outbox, before_outbox);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mode_switch_to_transparent_enqueues_durable_revocation() {
        let context = create_test_context().await;
        context
            .client_service
            .set_capability_config("client-a", CapabilitySource::Activated, Vec::new())
            .await
            .expect("create Consumer state");
        sqlx::query("UPDATE client SET approval_status = 'approved' WHERE identifier = 'client-a'")
            .execute(&context.db_pool)
            .await
            .expect("approve Consumer");
        let _ = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-a".to_string(),
                config_mode: Some("hosted".to_string()),
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: None,
            }),
        )
        .await
        .expect("seed hosted mode through atomic capability configuration");
        let binding_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM consumer_surface_bindings WHERE consumer_id = 'client-a'")
                .fetch_one(&context.db_pool)
                .await
                .expect("count initial Consumer binding");
        assert_eq!(binding_count, 1);

        let Json(response) = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-a".to_string(),
                config_mode: Some("transparent".to_string()),
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: None,
            }),
        )
        .await
        .expect("switch mode to transparent");

        assert!(response.success);
        let binding_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM consumer_surface_bindings WHERE consumer_id = 'client-a'")
                .fetch_one(&context.db_pool)
                .await
                .expect("count Consumer binding after leaving managed mode");
        assert_eq!(binding_count, 0);
        let revocation_payload: String = sqlx::query_scalar(
            r#"
            SELECT payload
            FROM surface_outbox_events
            WHERE aggregate_id = 'client-a' AND delivered_at IS NULL
            ORDER BY created_at DESC, event_id DESC
            LIMIT 1
            "#,
        )
        .fetch_one(&context.db_pool)
        .await
        .expect("load pending managed access revocation");
        assert!(revocation_payload.contains("managed_access_revoked"));
    }

    #[tokio::test]
    async fn update_settings_rejects_alias_transport_keys() {
        let context = create_test_context().await;

        let result = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transports: Some(HashMap::from([("http".to_string(), selected_streamable_http_rule())])),
                ..settings_update_req("client-a")
            }),
        )
        .await;

        let (status, Json(response)) = *result.expect_err("alias transport key should be rejected");
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!response.success);
        let error = response.error.expect("error details");
        assert!(error.message.contains("Invalid transport key 'http'"));
    }

    #[test]
    fn client_settings_contract_rejects_non_atomic_config_mode() {
        let result = serde_json::from_value::<ClientSettingsUpdateReq>(json!({
            "identifier": "client-a",
            "config_mode": "hosted"
        }));

        let error = result.expect_err("config mode must use the atomic capability endpoint");
        assert!(error.to_string().contains("unknown field `config_mode`"));
    }

    #[tokio::test]
    async fn capability_config_preserves_registered_refs_that_are_not_currently_eligible() {
        let context = create_test_context().await;
        insert_unify_server(
            &context.db_pool,
            "server-eligible",
            "eligible_server",
            true,
            &["tool-a"],
        )
        .await;
        insert_unify_server(
            &context.db_pool,
            "server-ineligible",
            "ineligible_server",
            false,
            &["tool-x"],
        )
        .await;
        let eligible_ref = capability_ref("server-eligible", CatalogCapabilityKind::Tools, "tool-a");
        let ineligible_ref = capability_ref("server-ineligible", CatalogCapabilityKind::Tools, "tool-x");
        let mut expected_refs = vec![eligible_ref.clone(), ineligible_ref.clone()];
        expected_refs.sort();

        let Json(response) = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-invalid".to_string(),
                config_mode: None,
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: Some(crate::api::models::client::ClientUnifyDirectExposureReq {
                    route_mode: crate::clients::models::UnifyRouteMode::CapabilityLevel,
                    server_ids: Vec::new(),
                    capability_refs: crate::clients::models::UnifyDirectCapabilityRefs {
                        tool_refs: vec![ineligible_ref.clone(), eligible_ref.clone()],
                        ..Default::default()
                    },
                }),
            }),
        )
        .await
        .expect("update invalid unify config");

        let data = response.data.expect("response data");
        assert!(data.unify_direct_exposure.intent.server_ids.is_empty());
        assert_eq!(
            data.unify_direct_exposure.resolved_capabilities.selected_tool_surfaces,
            vec![crate::clients::models::UnifyDirectToolSurface {
                server_id: "server-eligible".to_string(),
                tool_name: "tool-a".to_string(),
            }]
        );
        assert_eq!(
            data.unify_direct_exposure.intent.capability_refs.tool_refs,
            expected_refs
        );
        assert_eq!(
            data.unify_direct_exposure.diagnostics.invalid_server_ids,
            Vec::<String>::new()
        );
        assert_eq!(
            data.unify_direct_exposure.diagnostics.invalid_capability_refs,
            vec![ineligible_ref.clone()]
        );
        let stored_refs = sqlx::query_scalar::<_, String>(
            r#"
            SELECT direct.ref_id
            FROM direct_exposure_refs direct
            JOIN client ON client.identifier = direct.consumer_id
            WHERE client.identifier = ? AND direct.enabled = 1
            ORDER BY direct.ref_id
            "#,
        )
        .bind("client-invalid")
        .fetch_all(&context.db_pool)
        .await
        .expect("load stored Direct Exposure refs");
        assert_eq!(stored_refs, expected_refs);
    }

    #[tokio::test]
    async fn failed_direct_exposure_write_does_not_partially_update_client_capability_source() {
        let context = create_test_context().await;
        let profile_id = insert_shared_profile(&context.db_pool, "atomic-profile").await;
        let unknown_ref = format!("cref_sha256:{}", "0".repeat(64));

        let result = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-atomic-direct".to_string(),
                config_mode: None,
                capability_source: CapabilitySource::Profiles,
                selected_profile_ids: vec![profile_id],
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: Some(crate::api::models::client::ClientUnifyDirectExposureReq {
                    route_mode: crate::clients::models::UnifyRouteMode::CapabilityLevel,
                    server_ids: Vec::new(),
                    capability_refs: crate::clients::models::UnifyDirectCapabilityRefs {
                        tool_refs: vec![unknown_ref],
                        ..Default::default()
                    },
                }),
            }),
        )
        .await;

        assert!(result.is_err());
        let consumer_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM client WHERE identifier = 'client-atomic-direct'")
                .fetch_one(&context.db_pool)
                .await
                .expect("count Consumer rows after failed write");
        assert_eq!(consumer_count, 0);
    }

    #[tokio::test]
    async fn server_eligibility_change_prunes_server_level_direct_surfaces() {
        let context = create_test_context().await;
        insert_unify_server(
            &context.db_pool,
            "server-prune-level",
            "prune_level_server",
            true,
            &["tool-a"],
        )
        .await;

        let _ = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-prune-level".to_string(),
                config_mode: None,
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: Some(crate::api::models::client::ClientUnifyDirectExposureReq {
                    route_mode: crate::clients::models::UnifyRouteMode::ServerLevel,
                    server_ids: vec!["server-prune-level".to_string()],
                    capability_refs: Default::default(),
                }),
            }),
        )
        .await
        .expect("seed server-level capability config");

        set_test_server_direct_exposure_eligible(&context.db_pool, "server-prune-level", false).await;

        let Json(response) = get_capability_config(
            State(context.app_state.clone()),
            Query(ClientConfigReq {
                identifier: "client-prune-level".to_string(),
            }),
        )
        .await
        .expect("load pruned capability config");

        let data = response.data.expect("response data");
        assert!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_tool_surfaces
                .is_empty()
        );
    }

    #[tokio::test]
    async fn server_eligibility_change_hides_surfaces_without_removing_capability_ref_intent() {
        let context = create_test_context().await;
        insert_unify_server(
            &context.db_pool,
            "server-prune-capabilities",
            "prune_capability_server",
            true,
            &["tool-a"],
        )
        .await;
        insert_unify_non_tool_capabilities(
            &context.db_pool,
            "server-prune-capabilities",
            "prune_capability_server",
            &["prompt-a"],
            &["test://resource-a"],
            &["test:///{id}"],
        )
        .await;
        let requested_refs = crate::clients::models::UnifyDirectCapabilityRefs {
            tool_refs: vec![capability_ref(
                "server-prune-capabilities",
                CatalogCapabilityKind::Tools,
                "tool-a",
            )],
            prompt_refs: vec![capability_ref(
                "server-prune-capabilities",
                CatalogCapabilityKind::Prompts,
                "prompt-a",
            )],
            resource_refs: vec![capability_ref(
                "server-prune-capabilities",
                CatalogCapabilityKind::Resources,
                "test://resource-a",
            )],
            template_refs: vec![capability_ref(
                "server-prune-capabilities",
                CatalogCapabilityKind::ResourceTemplates,
                "test:///{id}",
            )],
        };

        let _ = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-prune-capabilities".to_string(),
                config_mode: None,
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: Some(crate::api::models::client::ClientUnifyDirectExposureReq {
                    route_mode: crate::clients::models::UnifyRouteMode::CapabilityLevel,
                    server_ids: Vec::new(),
                    capability_refs: requested_refs.clone(),
                }),
            }),
        )
        .await
        .expect("seed capability-level direct surfaces");

        set_test_server_direct_exposure_eligible(&context.db_pool, "server-prune-capabilities", false).await;

        let Json(response) = get_capability_config(
            State(context.app_state.clone()),
            Query(ClientConfigReq {
                identifier: "client-prune-capabilities".to_string(),
            }),
        )
        .await
        .expect("load pruned capability-level config");

        let data = response.data.expect("response data");
        assert!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_tool_surfaces
                .is_empty()
        );
        assert!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_prompt_surfaces
                .is_empty()
        );
        assert!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_resource_surfaces
                .is_empty()
        );
        assert!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_template_surfaces
                .is_empty()
        );
        assert_eq!(data.unify_direct_exposure.intent.capability_refs, requested_refs);
    }

    #[tokio::test]
    async fn server_disable_prunes_server_level_direct_surfaces() {
        let context = create_test_context().await;
        insert_unify_server(
            &context.db_pool,
            "server-disable-level",
            "disable_level_server",
            true,
            &["tool-a"],
        )
        .await;

        let _ = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-disable-level".to_string(),
                config_mode: None,
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: Some(crate::api::models::client::ClientUnifyDirectExposureReq {
                    route_mode: crate::clients::models::UnifyRouteMode::ServerLevel,
                    server_ids: vec!["server-disable-level".to_string()],
                    capability_refs: Default::default(),
                }),
            }),
        )
        .await
        .expect("seed server-level config before disable");

        set_test_server_enabled(&context.db_pool, "server-disable-level", false).await;

        let Json(response) = get_capability_config(
            State(context.app_state.clone()),
            Query(ClientConfigReq {
                identifier: "client-disable-level".to_string(),
            }),
        )
        .await
        .expect("load server-level config after disable");

        let data = response.data.expect("response data");
        assert!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_tool_surfaces
                .is_empty()
        );
    }

    #[tokio::test]
    async fn server_disable_hides_surfaces_without_removing_capability_ref_intent() {
        let context = create_test_context().await;
        insert_unify_server(
            &context.db_pool,
            "server-disable-capabilities",
            "disable_capability_server",
            true,
            &["tool-a"],
        )
        .await;
        insert_unify_non_tool_capabilities(
            &context.db_pool,
            "server-disable-capabilities",
            "disable_capability_server",
            &["prompt-a"],
            &["test://resource-a"],
            &["test:///{id}"],
        )
        .await;
        let requested_refs = crate::clients::models::UnifyDirectCapabilityRefs {
            tool_refs: vec![capability_ref(
                "server-disable-capabilities",
                CatalogCapabilityKind::Tools,
                "tool-a",
            )],
            prompt_refs: vec![capability_ref(
                "server-disable-capabilities",
                CatalogCapabilityKind::Prompts,
                "prompt-a",
            )],
            resource_refs: vec![capability_ref(
                "server-disable-capabilities",
                CatalogCapabilityKind::Resources,
                "test://resource-a",
            )],
            template_refs: vec![capability_ref(
                "server-disable-capabilities",
                CatalogCapabilityKind::ResourceTemplates,
                "test:///{id}",
            )],
        };

        let _ = update_capability_config(
            State(context.app_state.clone()),
            Json(ClientCapabilityConfigReq {
                identifier: "client-disable-capabilities".to_string(),
                config_mode: None,
                capability_source: CapabilitySource::Activated,
                selected_profile_ids: Vec::new(),
                source_revision_set: displayed_catalog_revision_set(&context).await,
                unify_direct_exposure: Some(crate::api::models::client::ClientUnifyDirectExposureReq {
                    route_mode: crate::clients::models::UnifyRouteMode::CapabilityLevel,
                    server_ids: Vec::new(),
                    capability_refs: requested_refs.clone(),
                }),
            }),
        )
        .await
        .expect("seed capability config before disable");

        set_test_server_enabled(&context.db_pool, "server-disable-capabilities", false).await;

        let Json(response) = get_capability_config(
            State(context.app_state.clone()),
            Query(ClientConfigReq {
                identifier: "client-disable-capabilities".to_string(),
            }),
        )
        .await
        .expect("load capability config after disable");

        let data = response.data.expect("response data");
        assert!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_tool_surfaces
                .is_empty()
        );
        assert!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_prompt_surfaces
                .is_empty()
        );
        assert!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_resource_surfaces
                .is_empty()
        );
        assert!(
            data.unify_direct_exposure
                .resolved_capabilities
                .selected_template_surfaces
                .is_empty()
        );
        assert_eq!(data.unify_direct_exposure.intent.capability_refs, requested_refs);
    }

    #[tokio::test]
    async fn config_details_supports_active_runtime_only_client() {
        let context = create_test_context().await;

        context
            .client_service
            .set_active_client_settings(
                "custom.runtime",
                ActiveClientSettingsUpdate {
                    config_file_state: Some(ClientConfigFileState::WithoutConfigFile),
                    description: Some("Runtime-only client".to_string()),
                    ..ActiveClientSettingsUpdate::default()
                },
            )
            .await
            .expect("create active runtime-only client");
        sqlx::query("UPDATE client SET config_mode = 'hosted' WHERE identifier = 'custom.runtime'")
            .execute(&context.db_pool)
            .await
            .expect("seed hosted mode");

        let Json(response) = config_details(
            State(context.app_state.clone()),
            Query(ClientConfigReq {
                identifier: "custom.runtime".to_string(),
            }),
        )
        .await
        .expect("config details for runtime-only client");

        assert!(response.success);
        let data = response.data.expect("response data");
        assert!(!data.governed_by_default_policy);
        assert!(!data.writable_config);
        assert!(data.warnings.is_empty());
        assert!(data.transports.is_none());
        assert_eq!(data.description.as_deref(), Some("Runtime-only client"));
        assert_eq!(data.template.format, "json");
        assert_eq!(data.template.storage.kind, "file");
    }

    #[tokio::test]
    async fn list_includes_passive_runtime_clients_by_default() {
        let context = create_test_context().await;
        context
            .client_service
            .persist_handshake_observation(
                "test.passive-runtime",
                Some("Test Passive Runtime"),
                Some("1.0.0"),
                Some("streamable_http"),
                None,
                None,
                None,
            )
            .await
            .expect("seed passive runtime client");

        let Json(default_response) = list(
            State(context.app_state.clone()),
            Query(ClientCheckReq {
                refresh: false,
                persist_detected: false,
                include_detected: false,
            }),
        )
        .await
        .expect("list managed clients");

        assert!(default_response.success);
        let runtime_client = default_response
            .data
            .expect("default data")
            .client
            .into_iter()
            .find(|client| client.identifier == "test.passive-runtime")
            .expect("runtime-observed passive client should be visible by default");
        assert_eq!(
            runtime_client.registration_origin,
            ClientRegistrationOrigin::RuntimeInitialize
        );
        assert!(runtime_client.runtime_observed);
        assert_eq!(
            runtime_client.config_file_state,
            ClientConfigFileState::WithoutConfigFile
        );
    }

    #[tokio::test]
    async fn list_excludes_passive_detected_candidates_by_default() {
        let context = create_test_context().await;
        context
            .client_service
            .set_first_contact_behavior(crate::clients::models::FirstContactBehavior::Allow)
            .await
            .expect("set allow policy");
        context
            .client_service
            .ensure_passive_observed_row("test.passive-detected", "Test Passive Detected", Some("/tmp/test.json"))
            .await
            .expect("seed passive detected client");

        let Json(default_response) = list(
            State(context.app_state.clone()),
            Query(ClientCheckReq {
                refresh: false,
                persist_detected: false,
                include_detected: false,
            }),
        )
        .await
        .expect("list managed clients");

        assert!(default_response.success);
        assert!(
            default_response
                .data
                .expect("default data")
                .client
                .into_iter()
                .all(|client| client.identifier != "test.passive-detected")
        );

        let Json(include_response) = list(
            State(context.app_state.clone()),
            Query(ClientCheckReq {
                refresh: false,
                persist_detected: false,
                include_detected: true,
            }),
        )
        .await
        .expect("list detected candidates");

        assert!(include_response.success);
        let detected_client = include_response
            .data
            .expect("include data")
            .client
            .into_iter()
            .find(|client| client.identifier == "test.passive-detected")
            .expect("detected passive client should be visible when requested");
        assert_eq!(
            detected_client.registration_origin,
            ClientRegistrationOrigin::ConfigDetection
        );
        assert!(!detected_client.runtime_observed);
        assert_eq!(detected_client.config_file_state, ClientConfigFileState::WithConfigFile);
    }

    #[tokio::test]
    async fn list_uses_template_metadata_for_known_active_client() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("codex.toml");
        tokio::fs::write(&config_path, "")
            .await
            .expect("seed codex config file");
        let mut template = crate::clients::models::ClientTemplate {
            identifier: "codex".to_string(),
            display_name: Some("Codex".to_string()),
            ..Default::default()
        };
        template.metadata.insert(
            "description".to_string(),
            serde_json::Value::String(
                "One agent for everywhere you code—included in ChatGPT Plus, Pro, Business, Edu, and Enterprise plans."
                    .to_string(),
            ),
        );
        template.metadata.insert(
            "logo_url".to_string(),
            serde_json::Value::String("https://openai.com/favicon.ico".to_string()),
        );
        sqlx::query("INSERT OR REPLACE INTO client_template_runtime (identifier, payload_json) VALUES (?, ?)")
            .bind("codex")
            .bind(serde_json::to_string(&template).expect("serialize codex template"))
            .execute(&context.db_pool)
            .await
            .expect("seed codex runtime template");

        context
            .client_service
            .set_active_client_settings(
                "codex",
                ActiveClientSettingsUpdate {
                    config_file_state: Some(ClientConfigFileState::WithConfigFile),
                    config_path: Some(config_path.to_string_lossy().to_string()),
                    clear_config_file_parse: true,
                    ..ActiveClientSettingsUpdate::default()
                },
            )
            .await
            .expect("promote codex");

        sqlx::query("UPDATE client SET approval_metadata = NULL WHERE identifier = ?")
            .bind("codex")
            .execute(&context.db_pool)
            .await
            .expect("clear stored metadata");

        let Json(response) = list(
            State(context.app_state.clone()),
            Query(ClientCheckReq {
                refresh: false,
                persist_detected: false,
                include_detected: false,
            }),
        )
        .await
        .expect("list clients");

        let codex = response
            .data
            .expect("list data")
            .client
            .into_iter()
            .find(|client| client.identifier == "codex")
            .expect("codex in managed list");
        assert_eq!(
            codex.description.as_deref(),
            Some(
                "One agent for everywhere you code—included in ChatGPT Plus, Pro, Business, Edu, and Enterprise plans."
            )
        );
        assert_eq!(codex.logo_url.as_deref(), Some("https://openai.com/favicon.ico"));
    }

    #[tokio::test]
    async fn update_settings_persists_runtime_only_active_payload_fields() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("custom-runtime-payload.json");
        tokio::fs::write(&config_path, "{}")
            .await
            .expect("seed payload config file");

        let Json(response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("streamable_http".to_string()),
                client_version: Some("1.2.3".to_string()),
                display_name: Some("Custom Runtime".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                description: Some("Custom runtime client".to_string()),
                homepage_url: Some("https://example.com".to_string()),
                docs_url: Some("https://example.com/docs".to_string()),
                support_url: Some("https://example.com/support".to_string()),
                logo_url: Some("https://example.com/logo.png".to_string()),
                ..settings_update_req("custom.runtime.payload")
            }),
        )
        .await
        .expect("update settings");

        assert!(response.success);
        let data = response.data.expect("response data");
        assert_eq!(data.display_name, "Custom Runtime");
        assert_eq!(data.config_file_state, ClientConfigFileState::WithConfigFile);
        assert!(data.transports.is_none());
        assert_eq!(data.description.as_deref(), Some("Custom runtime client"));

        let state = context
            .client_service
            .fetch_state("custom.runtime.payload")
            .await
            .expect("fetch state")
            .expect("state exists");
        assert_eq!(state.governance_kind().as_str(), "active");
        assert_eq!(state.display_name(), "Custom Runtime");
        assert_eq!(state.connection_mode().as_str(), "local_config_detected");
        assert_eq!(state.config_path(), Some(config_path.to_string_lossy().as_ref()));
        assert_eq!(state.template_identifier(), None);
        assert_eq!(
            state.runtime_client_metadata().homepage_url.as_deref(),
            Some("https://example.com")
        );

        assert_eq!(state.config_format(), None);
        assert_eq!(state.container_keys().expect("container keys"), Vec::<String>::new());
        assert_eq!(state.managed_source(), None);
        let transports = state.parsed_transports().expect("transports");
        assert!(transports.is_empty());
    }

    #[tokio::test]
    async fn config_details_returns_transports_as_support_source() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("template-client.json");
        tokio::fs::write(&config_path, "{}")
            .await
            .expect("seed template client config file");

        let Json(update_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("stdio".to_string()),
                display_name: Some("Client A".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                config_file_parse: Some(json_object_parse("mcpServers")),
                transports: Some(HashMap::from([("streamable_http".to_string(), streamable_http_rule())])),
                ..settings_update_req("client-a")
            }),
        )
        .await
        .expect("update template client settings");

        assert!(update_response.success);

        let Json(details_response) = config_details(
            State(context.app_state.clone()),
            Query(ClientConfigReq {
                identifier: "client-a".to_string(),
            }),
        )
        .await
        .expect("load template client details");

        assert!(details_response.success);
        let details = details_response.data.expect("details data");
        assert!(
            details
                .transports
                .as_ref()
                .is_some_and(|transports| transports.contains_key("streamable_http"))
        );

        let Json(list_response) = list(
            State(context.app_state.clone()),
            Query(ClientCheckReq {
                refresh: false,
                persist_detected: true,
                include_detected: false,
            }),
        )
        .await
        .expect("list clients");

        let listed_client = list_response
            .data
            .expect("list data")
            .client
            .into_iter()
            .find(|client| client.identifier == "client-a")
            .expect("client-a in list response");
        assert!(
            listed_client
                .transports
                .as_ref()
                .is_some_and(|transports| transports.contains_key("streamable_http"))
        );
    }

    #[tokio::test]
    async fn config_details_uses_effective_parse_for_configured_server_entries() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("zed-settings.json");
        tokio::fs::write(
            &config_path,
            r#"{
                "context_servers": {
                    "MCPMate": {"url": "http://127.0.0.1:8000/mcp"},
                    "mcp-server-context7": {"baseUrl": "https://context7.com/mcp"}
                },
                "agent_servers": {
                    "claude-acp": {"type": "registry"},
                    "codex-acp": {"type": "registry"}
                }
            }"#,
        )
        .await
        .expect("seed zed config file");

        let Json(update_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("stdio".to_string()),
                display_name: Some("Zed Strict Inspect".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                config_file_parse: Some(json_object_parse("context_servers")),
                transports: Some(HashMap::from([(
                    "streamable_http".to_string(),
                    selected_streamable_http_rule(),
                )])),
                ..settings_update_req("zed.strict.inspect")
            }),
        )
        .await
        .expect("update zed-like client settings");

        assert!(update_response.success);

        let Json(details_response) = config_details(
            State(context.app_state.clone()),
            Query(ClientConfigReq {
                identifier: "zed.strict.inspect".to_string(),
            }),
        )
        .await
        .expect("load zed-like client details");

        assert!(details_response.success);
        let details = details_response.data.expect("details data");
        assert_eq!(details.configured_server_entries.len(), 2);
        assert_eq!(details.configured_server_entries[0].name, "MCPMate");
        assert_eq!(details.configured_server_entries[0].transport, "streamable_http");
        assert_eq!(details.configured_server_entries[0].import_status, "importable");
        assert_eq!(details.configured_server_entries[1].name, "mcp-server-context7");
        assert_eq!(details.configured_server_entries[1].transport, "streamable_http");
        assert_eq!(details.configured_server_entries[1].import_status, "importable");
        assert_eq!(details.configured_server_entries[1].skip_reason, None);
        assert_eq!(details.mcp_servers_count, 2);
    }

    #[tokio::test]
    async fn config_details_marks_managed_entries_by_fingerprint() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("already-imported.json");
        tokio::fs::write(
            &config_path,
            r#"{
                "mcpServers": {
                    "mcp-server-context7": {"url": "https://context7.com/mcp"}
                }
            }"#,
        )
        .await
        .expect("seed client config file");

        sqlx::query(&format!(
            "INSERT INTO {} (id, name, server_type, url) VALUES (?, ?, ?, ?)",
            crate::common::constants::database::tables::SERVER_CONFIG
        ))
        .bind("existing-context7")
        .bind("mcp-server-context7")
        .bind("streamable_http")
        .bind("https://context7.com/mcp")
        .execute(&context.db_pool)
        .await
        .expect("insert existing server");

        let Json(update_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("stdio".to_string()),
                display_name: Some("Already Imported Client".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                config_file_parse: Some(json_object_parse("mcpServers")),
                transports: Some(HashMap::from([(
                    "streamable_http".to_string(),
                    selected_streamable_http_rule(),
                )])),
                ..settings_update_req("already.imported.client")
            }),
        )
        .await
        .expect("update client settings");

        assert!(update_response.success);

        let Json(details_response) = config_details(
            State(context.app_state.clone()),
            Query(ClientConfigReq {
                identifier: "already.imported.client".to_string(),
            }),
        )
        .await
        .expect("load client details");

        assert!(details_response.success);
        let details = details_response.data.expect("details data");
        assert_eq!(details.configured_server_entries.len(), 1);
        let entry = &details.configured_server_entries[0];
        assert_eq!(entry.name, "mcp-server-context7");
        assert_eq!(entry.import_status, "importable");
        assert_eq!(entry.skip_reason, None);
        assert!(entry.managed_by_mcpmate);
    }

    #[tokio::test]
    async fn inspect_existing_prefers_supplied_draft_over_invalid_persisted_parse() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("inspect-existing.json");
        tokio::fs::write(
            &config_path,
            r#"{
                "mcpServers": {
                    "alpha": { "command": "node" }
                }
            }"#,
        )
        .await
        .expect("seed inspect-existing config");

        context
            .client_service
            .set_active_client_settings(
                "client-a",
                ActiveClientSettingsUpdate {
                    display_name: Some("Inspect Existing Client".to_string()),
                    transport: Some("stdio".to_string()),
                    config_file_state: Some(ClientConfigFileState::WithConfigFile),
                    config_path: Some(config_path.to_string_lossy().to_string()),
                    ..ActiveClientSettingsUpdate::default()
                },
            )
            .await
            .expect("seed active client state");

        sqlx::query(
            r#"
            UPDATE client
            SET config_mode = 'hosted',
                config_file_parse = 'not-json',
                approval_status = 'approved'
            WHERE identifier = ?
            "#,
        )
        .bind("client-a")
        .execute(&context.db_pool)
        .await
        .expect("persist invalid parse override");

        let Json(response) = config_file_parse_inspect_existing(
            State(context.app_state.clone()),
            Json(ClientConfigFileParseInspectExistingReq {
                identifier: "client-a".to_string(),
                config_file_parse: Some(json_object_parse("mcpServers")),
            }),
        )
        .await
        .expect("inspect existing with supplied draft");

        assert!(response.success);
        let data = response.data.expect("response data");
        assert_eq!(
            data.validation.as_ref().map(|validation| validation.server_count),
            Some(1)
        );
        assert_eq!(data.preview["alpha"]["command"], "node");
    }

    #[test]
    fn transport_support_derivation_rejects_alias_http_keys() {
        let mut rules: HashMap<String, crate::clients::models::FormatRule> = HashMap::new();
        rules.insert(
            "http".to_string(),
            crate::clients::models::FormatRule {
                template: serde_json::json!({
                    "type": "streamable_http",
                    "url": "{{{url}}}",
                    "headers": "{{{json headers}}}"
                }),
                include_type: false,
                ..Default::default()
            },
        );

        let transports = crate::clients::service::core::supported_transports_from_transports(&rules);
        assert!(transports.is_empty());
    }

    #[tokio::test]
    async fn update_settings_rejects_missing_local_config_target() {
        let context = create_test_context().await;
        let missing_path = context._temp_dir.path().join("missing-client.json");

        let result = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("streamable_http".to_string()),
                display_name: Some("Invalid Runtime".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(missing_path.to_string_lossy().to_string()),
                ..settings_update_req("custom.runtime.invalid")
            }),
        )
        .await;

        let (status, Json(response)) = *result.expect_err("missing local config target should fail");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error = response.error.expect("error payload");
        assert!(error.message.contains("does not exist"));
    }

    #[tokio::test]
    async fn update_settings_clears_config_path_when_switching_to_manual() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("client-manual-clear.json");
        tokio::fs::write(&config_path, "{}").await.expect("seed config file");

        let Json(initial_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("streamable_http".to_string()),
                display_name: Some("Clear Runtime".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                config_file_parse: Some(json_object_parse("mcpServers")),
                transports: Some(HashMap::from([("streamable_http".to_string(), streamable_http_rule())])),
                ..settings_update_req("custom.runtime.clear")
            }),
        )
        .await
        .expect("initial update succeeds");
        assert!(initial_response.success);

        let Json(clear_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("streamable_http".to_string()),
                display_name: Some("Clear Runtime".to_string()),
                config_file_state: Some(ClientConfigFileState::WithoutConfigFile),
                ..settings_update_req("custom.runtime.clear")
            }),
        )
        .await
        .expect("manual update succeeds");
        assert!(clear_response.success);
        let clear_data = clear_response.data.expect("response data");
        assert_eq!(clear_data.config_file_state, ClientConfigFileState::WithoutConfigFile);
        assert!(clear_data.config_path.is_none());
        assert!(clear_data.config_file_parse_effective.is_none());
        assert!(clear_data.config_file_parse_override.is_none());
        assert!(clear_data.transports.is_none());

        let state = context
            .client_service
            .fetch_state("custom.runtime.clear")
            .await
            .expect("fetch state")
            .expect("state exists");
        assert_eq!(state.connection_mode().as_str(), "manual");
        assert_eq!(state.config_path(), None);
        assert!(state.effective_config_file_parse().expect("config parse").is_none());
        assert!(state.parsed_transports().expect("transports").is_empty());
    }

    #[tokio::test]
    async fn update_settings_marks_detached_when_switching_from_manual_to_local_config() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("client-local-attach.json");
        tokio::fs::write(&config_path, r#"{"mcpServers":{}}"#)
            .await
            .expect("seed config file");

        let Json(manual_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                display_name: Some("Manual Runtime".to_string()),
                config_file_state: Some(ClientConfigFileState::WithoutConfigFile),
                ..settings_update_req("custom.runtime.manual-to-local")
            }),
        )
        .await
        .expect("manual update succeeds");
        assert!(manual_response.success);
        assert!(manual_response.data.expect("manual response data").created);

        let manual_state = context
            .client_service
            .fetch_state("custom.runtime.manual-to-local")
            .await
            .expect("fetch manual state")
            .expect("manual state exists");
        assert_eq!(manual_state.attachment_state().as_str(), "not_applicable");

        let Json(local_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("streamable_http".to_string()),
                display_name: Some("Manual Runtime".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                config_file_parse: Some(json_object_parse("mcpServers")),
                transports: Some(HashMap::from([("streamable_http".to_string(), streamable_http_rule())])),
                ..settings_update_req("custom.runtime.manual-to-local")
            }),
        )
        .await
        .expect("local config update succeeds");
        assert!(local_response.success);
        assert!(!local_response.data.expect("local response data").created);

        let local_state = context
            .client_service
            .fetch_state("custom.runtime.manual-to-local")
            .await
            .expect("fetch local state")
            .expect("local state exists");
        assert!(local_state.has_local_config_target());
        assert_eq!(local_state.attachment_state().as_str(), "detached");
    }

    #[tokio::test]
    async fn update_settings_clear_parse_does_not_revive_legacy_metadata() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("zed-settings.json");
        tokio::fs::write(
            &config_path,
            r#"{
                "mcpServers": {
                    "alpha": { "command": "node" }
                }
            }"#,
        )
        .await
        .expect("seed strict json config file");

        let Json(initial_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                display_name: Some("Zed".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                config_file_parse: Some(json_object_parse("mcpServers")),
                ..settings_update_req("zed.parse.clear")
            }),
        )
        .await
        .expect("initial update succeeds");
        assert!(initial_response.success);

        tokio::fs::write(
            &config_path,
            r#"{
                "mcpServers": {
                    "alpha": {
                        "command": "node",
                    },
                },
            }"#,
        )
        .await
        .expect("seed json5-style config file");

        let Json(clear_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                display_name: Some("Zed".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                clear_config_file_parse: true,
                ..settings_update_req("zed.parse.clear")
            }),
        )
        .await
        .expect("clear update succeeds");
        assert!(clear_response.success);
        let clear_data = clear_response.data.expect("clear response data");
        assert!(clear_data.config_file_parse_effective.is_none());
        assert!(clear_data.config_file_parse_override.is_none());

        let cleared_state = context
            .client_service
            .fetch_state("zed.parse.clear")
            .await
            .expect("fetch cleared state")
            .expect("state exists");
        assert_eq!(cleared_state.config_format(), None);
        assert_eq!(cleared_state.container_type(), None);
        assert_eq!(
            cleared_state.container_keys().expect("container keys"),
            Vec::<String>::new()
        );
        assert!(
            cleared_state
                .effective_config_file_parse()
                .expect("config parse")
                .is_none()
        );

        let Json(update_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                display_name: Some("Zed".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                ..settings_update_req("zed.parse.clear")
            }),
        )
        .await
        .expect("subsequent update should not validate with stale json parse metadata");
        assert!(update_response.success);
    }

    #[tokio::test]
    async fn update_settings_clear_parse_with_replacement_keeps_replacement_effective() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("zed-replacement.json5");
        tokio::fs::write(
            &config_path,
            r#"{
                "context_servers": {
                    "alpha": {
                        "command": "node",
                    },
                },
            }"#,
        )
        .await
        .expect("seed json5-style config file");

        let replacement_parse = ClientConfigFileParseData {
            format: "json5".to_string(),
            container_type: crate::api::models::client::ClientConfigType::Standard,
            container_keys: vec!["context_servers".to_string()],
        };

        let Json(update_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                display_name: Some("Zed".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                config_file_parse: Some(replacement_parse.clone()),
                clear_config_file_parse: true,
                ..settings_update_req("zed.parse.replacement")
            }),
        )
        .await
        .expect("replacement update succeeds");
        assert!(update_response.success);
        let update_data = update_response.data.expect("update response data");
        assert_eq!(update_data.config_file_parse_effective, Some(replacement_parse.clone()));
        assert!(update_data.config_file_parse_override.is_none());

        let state = context
            .client_service
            .fetch_state("zed.parse.replacement")
            .await
            .expect("fetch state")
            .expect("state exists");
        assert_eq!(state.config_format(), Some("json5"));
        assert_eq!(state.container_type(), Some("object"));
        assert_eq!(
            state.container_keys().expect("container keys"),
            vec!["context_servers".to_string()]
        );
        assert_eq!(
            state.effective_config_file_parse().expect("config parse"),
            Some(parse_rule_from_api_data(&replacement_parse))
        );
    }

    #[tokio::test]
    async fn update_settings_infers_manual_when_config_path_is_explicitly_cleared() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("client-manual-empty-string.json");
        tokio::fs::write(&config_path, "{}").await.expect("seed config file");

        let Json(initial_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("streamable_http".to_string()),
                display_name: Some("Empty Clear Runtime".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                ..settings_update_req("custom.runtime.empty-clear")
            }),
        )
        .await
        .expect("initial update succeeds");
        assert!(initial_response.success);

        let Json(clear_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("streamable_http".to_string()),
                display_name: Some("Empty Clear Runtime".to_string()),
                config_path: Some("   ".to_string()),
                ..settings_update_req("custom.runtime.empty-clear")
            }),
        )
        .await
        .expect("explicit empty config path update succeeds");
        assert!(clear_response.success);

        let state = context
            .client_service
            .fetch_state("custom.runtime.empty-clear")
            .await
            .expect("fetch state")
            .expect("state exists");
        assert_eq!(state.connection_mode().as_str(), "manual");
        assert_eq!(state.config_path(), None);
    }

    #[tokio::test]
    async fn config_details_and_apply_reject_stale_missing_config_target() {
        let context = create_test_context().await;

        context
            .client_service
            .set_client_settings("client-a", Some("streamable_http".to_string()), None)
            .await
            .expect("seed client settings");

        sqlx::query(
            "UPDATE client SET connection_mode = 'local_config_detected', config_path = ?, approval_status = 'approved' WHERE identifier = ?",
        )
        .bind(context._temp_dir.path().join("stale-missing.json").to_string_lossy().to_string())
        .bind("client-a")
        .execute(&context.db_pool)
        .await
        .expect("seed stale config path");

        let Json(details_response) = config_details(
            State(context.app_state.clone()),
            Query(ClientConfigReq {
                identifier: "client-a".to_string(),
            }),
        )
        .await
        .expect("config details response");

        assert!(details_response.success);
        let details = details_response.data.expect("details data");
        assert!(!details.writable_config);

        let apply_result = config_apply(
            State(context.app_state.clone()),
            Json(ClientConfigUpdateReq {
                identifier: "client-a".to_string(),
                mode: ClientConfigMode::Hosted,
                preview: false,
                selected_config: ClientConfigSelected::Default,
                backup_policy: None,
            }),
        )
        .await;

        assert!(apply_result.is_err());
        assert_eq!(apply_result.unwrap_err(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn update_settings_accepts_directory_target_for_kv_client() {
        let context = create_test_context().await;
        let kv_dir = context._temp_dir.path().join("cherry-kv");
        tokio::fs::create_dir_all(&kv_dir).await.expect("create kv directory");

        let Json(response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("streamable_http".to_string()),
                display_name: Some("Cherry Studio".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(kv_dir.to_string_lossy().to_string()),
                ..settings_update_req("cherry_studio")
            }),
        )
        .await
        .expect("kv directory target should be accepted");

        assert!(response.success);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn update_settings_rejects_non_writable_directory_target() {
        use std::os::unix::fs::PermissionsExt;

        let context = create_test_context().await;
        let kv_dir = context._temp_dir.path().join("read-only-kv");
        tokio::fs::create_dir_all(&kv_dir).await.expect("create kv directory");

        let original_permissions = std::fs::metadata(&kv_dir).expect("directory metadata").permissions();
        let mut read_only_permissions = original_permissions.clone();
        read_only_permissions.set_mode(0o555);
        std::fs::set_permissions(&kv_dir, read_only_permissions).expect("set read-only permissions");

        let result = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("streamable_http".to_string()),
                display_name: Some("Cherry Studio".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(kv_dir.to_string_lossy().to_string()),
                ..settings_update_req("cherry_studio")
            }),
        )
        .await;

        std::fs::set_permissions(&kv_dir, original_permissions).expect("restore permissions");

        let (status, Json(response)) = *result.expect_err("read-only directory target should fail");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error = response.error.expect("error payload");
        assert!(error.message.contains("not writable"));
    }

    #[tokio::test]
    async fn update_settings_preserves_suspended_governance_state() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("suspended-client.json");
        tokio::fs::write(&config_path, "{}")
            .await
            .expect("seed suspended client config file");

        context
            .client_service
            .set_active_client_settings(
                "client-a",
                ActiveClientSettingsUpdate {
                    display_name: Some("Suspended Client".to_string()),
                    transport: Some("streamable_http".to_string()),
                    config_file_state: Some(ClientConfigFileState::WithConfigFile),
                    config_path: Some(config_path.to_string_lossy().to_string()),
                    ..ActiveClientSettingsUpdate::default()
                },
            )
            .await
            .expect("seed active client state");
        sqlx::query("UPDATE client SET config_mode = 'hosted' WHERE identifier = 'client-a'")
            .execute(&context.db_pool)
            .await
            .expect("seed hosted mode");

        context
            .client_service
            .suspend_client("client-a")
            .await
            .expect("suspend client");

        let Json(response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("streamable_http".to_string()),
                client_version: Some("2.0.0".to_string()),
                display_name: Some("Suspended Client".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                description: Some("Still editable while denied".to_string()),
                ..settings_update_req("client-a")
            }),
        )
        .await
        .expect("update denied client settings succeeds");

        assert!(response.success);
        let state = context
            .client_service
            .fetch_state("client-a")
            .await
            .expect("fetch state")
            .expect("state exists");
        assert_eq!(state.approval_status(), "suspended");
        assert_eq!(state.display_name(), "Suspended Client");
    }

    #[tokio::test]
    async fn config_apply_uses_db_runtime_definition_for_runtime_only_client() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("runtime-client.json");
        tokio::fs::write(&config_path, "{}")
            .await
            .expect("seed runtime apply config file");

        let Json(update_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("streamable_http".to_string()),
                client_version: Some("9.9.9".to_string()),
                display_name: Some("Runtime Apply Client".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                description: Some("Runtime apply test client".to_string()),
                config_file_parse: Some(json_object_parse("mcpServers")),
                transports: Some(HashMap::from([("streamable_http".to_string(), streamable_http_rule())])),
                ..settings_update_req("custom.runtime.apply")
            }),
        )
        .await
        .expect("update runtime-only client");

        assert!(update_response.success);

        let Json(apply_response) = config_apply(
            State(context.app_state.clone()),
            Json(ClientConfigUpdateReq {
                identifier: "custom.runtime.apply".to_string(),
                mode: ClientConfigMode::Hosted,
                preview: false,
                selected_config: ClientConfigSelected::Default,
                backup_policy: Some(crate::api::models::client::ClientBackupPolicyPayload {
                    policy: "off".to_string(),
                    limit: None,
                }),
            }),
        )
        .await
        .expect("apply runtime-only client config");

        assert!(apply_response.success);
        let data = apply_response.data.expect("response data");
        assert!(data.applied);
        assert_eq!(data.preview["mcpServers"]["MCPMate"]["type"], "streamable_http");

        let written = tokio::fs::read_to_string(&config_path)
            .await
            .expect("read written config");
        let parsed: Value = serde_json::from_str(&written).expect("parse written config");
        assert_eq!(parsed["mcpServers"]["MCPMate"]["type"], "streamable_http");
        assert_eq!(
            parsed["mcpServers"]["MCPMate"]["headers"][crate::common::constants::client_headers::MCPMATE_CLIENT_ID],
            "custom.runtime.apply"
        );

        let Json(details_response) = config_details(
            State(context.app_state.clone()),
            Query(ClientConfigReq {
                identifier: "custom.runtime.apply".to_string(),
            }),
        )
        .await
        .expect("details for applied runtime-only client");

        assert!(details_response.success);
        let details = details_response.data.expect("details data");
        assert!(details.config_exists);
        assert_eq!(details.attachment_state.as_deref(), Some("attached"));
        assert_eq!(details.template.managed_source.as_deref(), None);
        assert_eq!(details.content["mcpServers"]["MCPMate"]["type"], "streamable_http");
        assert_eq!(details.configured_server_entries.len(), 1);
        assert_eq!(details.configured_server_entries[0].name, "MCPMate");
    }

    #[tokio::test]
    async fn config_apply_rejects_pending_client() {
        use crate::clients::models::OnboardingPolicy;

        let context = create_test_context().await;

        context
            .client_service
            .set_onboarding_policy(OnboardingPolicy::RequireApproval)
            .await
            .expect("set onboarding policy");

        sqlx::query(
            r#"
            INSERT INTO client (
                id, name, identifier, approval_status, connection_mode, template_identifier
            )
            VALUES ('clnt001', 'Pending Client', 'pending.client', 'pending', 'manual', NULL)
            "#,
        )
        .execute(&context.db_pool)
        .await
        .expect("create pending client");

        let result = config_apply(
            State(context.app_state.clone()),
            Json(ClientConfigUpdateReq {
                identifier: "pending.client".to_string(),
                mode: ClientConfigMode::Hosted,
                preview: false,
                selected_config: ClientConfigSelected::Profile {
                    profile_id: "PROF001".to_string(),
                },
                backup_policy: None,
            }),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn config_restore_rejects_pending_client() {
        use crate::clients::models::OnboardingPolicy;

        let context = create_test_context().await;

        context
            .client_service
            .set_onboarding_policy(OnboardingPolicy::RequireApproval)
            .await
            .expect("set onboarding policy");

        sqlx::query(
            r#"
            INSERT INTO client (
                id, name, identifier, approval_status, connection_mode, template_identifier
            )
            VALUES ('clnt002', 'Pending Client', 'pending.restore', 'pending', 'manual', NULL)
            "#,
        )
        .execute(&context.db_pool)
        .await
        .expect("create pending client");

        let result = config_restore(
            State(context.app_state.clone()),
            Json(ClientConfigRestoreReq {
                identifier: "pending.restore".to_string(),
                backup: "backup-001".to_string(),
            }),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn client_attach_allows_reattach_when_file_already_contains_mcpmate() {
        let context = create_test_context().await;
        let identifier = "custom.runtime.attach";
        let config_path = context._temp_dir.path().join("runtime-attach-client.json");
        let parsed = seed_runtime_attachable_client(&context, identifier, &config_path).await;
        assert!(parsed["mcpServers"].get("MCPMate").is_some());
        let surface_store = mcpmate_capability_store::SqliteSurfaceStore::new(context.db_pool.clone());
        let initial_binding = surface_store
            .load_binding(identifier)
            .await
            .expect("load managed Consumer Surface")
            .expect("applying MCPMate to an approved managed Consumer must create its initial Surface publication");

        let Json(attach_response) = client_attach(
            State(context.app_state.clone()),
            Json(ClientAttachReq {
                identifier: identifier.to_string(),
            }),
        )
        .await
        .expect("reattach while file already contains MCPMate");

        assert!(attach_response.success);
        let data = attach_response.data.expect("attach data");
        assert!(
            data.warnings.iter().any(|warning| warning == ATTACH_OVERWRITE_WARNING),
            "expected overwrite warning, got {:?}",
            data.warnings
        );
        assert_eq!(data.attachment_state, "attached");
        let rebound = surface_store
            .load_binding(identifier)
            .await
            .expect("reload managed Consumer Surface")
            .expect("managed Consumer Surface remains published after reattach");
        assert_eq!(rebound.generation, initial_binding.generation);
    }

    #[tokio::test]
    async fn client_detach_removes_mcpmate_when_db_detached_but_file_still_present() {
        let context = create_test_context().await;
        let identifier = "custom.runtime.detach";
        let config_path = context._temp_dir.path().join("runtime-detach-client.json");
        let parsed = seed_runtime_attachable_client(&context, identifier, &config_path).await;
        assert!(parsed["mcpServers"].get("MCPMate").is_some());

        let seeded_state = context
            .client_service
            .fetch_state(identifier)
            .await
            .expect("fetch seeded detach state")
            .expect("seeded detach state exists");
        assert!(seeded_state.has_local_config_target());

        sqlx::query("UPDATE client SET attachment_state = 'detached' WHERE identifier = ?")
            .bind(identifier)
            .execute(&context.db_pool)
            .await
            .expect("force detached DB state while file still contains MCPMate");
        let detached_state = context
            .client_service
            .fetch_state(identifier)
            .await
            .expect("fetch detached state")
            .expect("detached state exists");
        assert_eq!(detached_state.attachment_state().as_str(), "detached");
        assert_eq!(
            detect_mcpmate_in_client_config(context.client_service.as_ref(), &detached_state).await,
            Some(true)
        );

        let Json(detach_response) = client_detach(
            State(context.app_state.clone()),
            Json(ClientDetachReq {
                identifier: identifier.to_string(),
            }),
        )
        .await
        .expect("detach while file still contains MCPMate");

        assert!(detach_response.success);
        let data = detach_response.data.expect("detach data");
        assert!(data.changed);
        assert!(data.warnings.is_empty());
        assert_eq!(data.attachment_state, "detached");

        let written = tokio::fs::read_to_string(&config_path)
            .await
            .expect("read detached config");
        let parsed: serde_json::Value = serde_json::from_str(&written).expect("parse detached config");
        assert!(parsed["mcpServers"].get("MCPMate").is_none());
    }

    #[tokio::test]
    async fn client_detach_warns_when_mcpmate_absent_from_file() {
        let context = create_test_context().await;
        let identifier = "custom.runtime.detach-idempotent";
        let config_path = context._temp_dir.path().join("runtime-detach-idempotent.json");
        tokio::fs::write(&config_path, r#"{"mcpServers":{}}"#)
            .await
            .expect("seed empty runtime config");

        let Json(update_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("streamable_http".to_string()),
                display_name: Some("Runtime Detach Idempotent".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                config_file_parse: Some(json_object_parse("mcpServers")),
                transports: Some(HashMap::from([("streamable_http".to_string(), streamable_http_rule())])),
                ..settings_update_req(identifier)
            }),
        )
        .await
        .expect("update runtime detach idempotent client");
        assert!(update_response.success);

        sqlx::query("UPDATE client SET attachment_state = 'detached' WHERE identifier = ?")
            .bind(identifier)
            .execute(&context.db_pool)
            .await
            .expect("mark client detached");

        let Json(detach_response) = client_detach(
            State(context.app_state.clone()),
            Json(ClientDetachReq {
                identifier: identifier.to_string(),
            }),
        )
        .await
        .expect("idempotent detach");

        assert!(detach_response.success);
        let data = detach_response.data.expect("detach data");
        assert!(!data.changed);
        assert!(
            data.warnings.iter().any(|warning| warning == DETACH_NO_EFFECT_WARNING),
            "expected no-effect warning, got {:?}",
            data.warnings
        );
    }

    #[tokio::test]
    async fn client_detach_rejects_undetectable_config_file() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("undetectable-detach.json");
        tokio::fs::write(
            &config_path,
            r#"{"mcpServers":{"MCPMate":{"url":"http://127.0.0.1:8000/mcp"}}}"#,
        )
        .await
        .expect("seed undetectable detach config");

        context
            .client_service
            .set_active_client_settings(
                "client-a",
                ActiveClientSettingsUpdate {
                    display_name: Some("Undetectable Detach Client".to_string()),
                    transport: Some("streamable_http".to_string()),
                    config_file_state: Some(ClientConfigFileState::WithConfigFile),
                    config_path: Some(config_path.to_string_lossy().to_string()),
                    ..ActiveClientSettingsUpdate::default()
                },
            )
            .await
            .expect("seed active client state");

        sqlx::query(
            r#"
            UPDATE client
            SET config_mode = 'hosted',
                config_file_parse = 'not-json',
                approval_status = 'approved',
                attachment_state = 'attached'
            WHERE identifier = ?
            "#,
        )
        .bind("client-a")
        .execute(&context.db_pool)
        .await
        .expect("persist invalid parse override");

        let result = client_detach(
            State(context.app_state.clone()),
            Json(ClientDetachReq {
                identifier: "client-a".to_string(),
            }),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn config_details_repairs_local_target_before_deriving_attachment_state() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("drift-attachment.json");
        tokio::fs::write(
            &config_path,
            r#"{"mcpServers":{"MCPMate":{"url":"http://127.0.0.1:8000/mcp"}}}"#,
        )
        .await
        .expect("seed drifted attachment config");

        let Json(update_response) = update_settings(
            State(context.app_state.clone()),
            Json(ClientSettingsUpdateReq {
                transport: Some("streamable_http".to_string()),
                display_name: Some("Drift Attachment Client".to_string()),
                config_file_state: Some(ClientConfigFileState::WithConfigFile),
                config_path: Some(config_path.to_string_lossy().to_string()),
                config_file_parse: Some(json_object_parse("mcpServers")),
                transports: Some(HashMap::from([("streamable_http".to_string(), streamable_http_rule())])),
                ..settings_update_req("client-a")
            }),
        )
        .await
        .expect("seed drift attachment client settings");
        assert!(update_response.success);

        sqlx::query(
            r#"
            UPDATE client
            SET connection_mode = 'manual',
                attachment_state = 'not_applicable'
            WHERE identifier = ?
            "#,
        )
        .bind("client-a")
        .execute(&context.db_pool)
        .await
        .expect("persist drifted attachment metadata");

        let Json(details_response) = config_details(
            State(context.app_state.clone()),
            Query(ClientConfigReq {
                identifier: "client-a".to_string(),
            }),
        )
        .await
        .expect("config details response");

        assert!(details_response.success);
        let details = details_response.data.expect("details data");
        assert_eq!(details.attachment_state.as_deref(), Some("attached"));
        assert!(
            !details
                .degraded_reasons
                .iter()
                .any(|reason| reason == "attachment_requires_local_config_target"),
            "repair should remove attachment drift, got {:?}",
            details.degraded_reasons
        );
    }
}
