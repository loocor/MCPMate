use super::ClientConfigService;
use crate::clients::error::{ConfigError, ConfigResult};
use crate::clients::models::{
    AttachmentState, CapabilitySource, ClientCapabilityConfig, ClientCapabilityConfigState, ClientConfigFileParse,
    ClientConfigFileState, ClientConnectionMode, ContainerType, FirstContactBehavior, FormatRule,
    UnifyDirectCapabilityRefs, UnifyDirectExposureConfig, UnifyDirectExposureDiagnostics, UnifyDirectExposureIntent,
    UnifyDirectPromptSurface, UnifyDirectPromptSurfaceDiagnostic, UnifyDirectResourceSurface,
    UnifyDirectResourceSurfaceDiagnostic, UnifyDirectTemplateSurface, UnifyDirectTemplateSurfaceDiagnostic,
    UnifyDirectToolSurface, UnifyDirectToolSurfaceDiagnostic, UnifyRouteMode, canonical_config_transport_key,
};
use crate::clients::service::core::{ClientStateRow, PersistedTemplateConfig, RuntimeClientMetadata};
use crate::common::profile::ProfileType;
use crate::config::database::Database;
use crate::core::capability::materializer::{
    MaterializationCoordinator, MaterializationTrigger, revoke_managed_surface_in_transaction,
};
use crate::core::proxy::server::{ClientContext, ClientIdentitySource, ClientTransport};
use crate::system::paths::get_path_service;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::OpenOptions;

const VALID_TRANSPORTS: &[&str] = &["auto", "sse", "stdio", "streamable_http"];

fn sanitize_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|trimmed| !trimmed.is_empty())
        .map(str::to_string)
}

fn validate_observed_transport(value: Option<&str>) -> ConfigResult<()> {
    let Some(transport) = value else {
        return Ok(());
    };

    if canonical_config_transport_key(transport).is_some() {
        return Ok(());
    }

    Err(ConfigError::DataAccessError(format!(
        "Invalid observed transport '{}'; expected canonical transport key",
        transport.trim()
    )))
}

#[derive(Debug, Clone, Default)]
struct ResolvedUnifyDirectExposureState {
    intent: UnifyDirectExposureIntent,
    config: UnifyDirectExposureConfig,
    diagnostics: UnifyDirectExposureDiagnostics,
}

#[derive(Debug, Clone, Default)]
struct UnifyDirectExposureInventory {
    tools: HashMap<String, HashSet<String>>,
    prompts: HashMap<String, HashSet<String>>,
    resources: HashMap<String, HashSet<String>>,
    templates: HashMap<String, HashSet<String>>,
    tool_refs: HashMap<String, UnifyDirectToolSurface>,
    prompt_refs: HashMap<String, UnifyDirectPromptSurface>,
    resource_refs: HashMap<String, UnifyDirectResourceSurface>,
    template_refs: HashMap<String, UnifyDirectTemplateSurface>,
}

struct PreparedCapabilityClientInsert {
    id: String,
    display_name: String,
    config_path: Option<String>,
    approval_status: &'static str,
    connection_mode: &'static str,
    attachment_state: &'static str,
    template_identifier: Option<String>,
    persisted_config: PersistedTemplateConfig,
}

struct PreparedCustomProfile {
    id: String,
    name: String,
}

#[derive(Debug, Clone, Default)]
pub struct ActiveClientSettingsUpdate {
    pub display_name: Option<String>,
    pub transport: Option<String>,
    pub client_version: Option<String>,
    pub config_file_state: Option<ClientConfigFileState>,
    pub config_path: Option<String>,
    pub description: Option<String>,
    pub homepage_url: Option<String>,
    pub docs_url: Option<String>,
    pub support_url: Option<String>,
    pub logo_url: Option<String>,
    pub config_file_parse: Option<ClientConfigFileParse>,
    pub clear_config_file_parse: bool,
    pub merge_strategy_override: Option<crate::clients::models::MergeStrategy>,
    pub clear_merge_strategy_override: bool,
    pub transports: Option<HashMap<String, FormatRule>>,
    pub clear_transports: bool,
}

fn config_file_state_to_connection_mode(state: ClientConfigFileState) -> &'static str {
    match state {
        ClientConfigFileState::WithConfigFile => "local_config_detected",
        ClientConfigFileState::WithoutConfigFile => "manual",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveClientSettingsResult {
    pub created: bool,
    pub display_name_source: &'static str,
    pub approval_status_source: &'static str,
    pub config_file_state_source: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciledUnifyDirectExposure {
    pub identifier: String,
    pub unify_direct_exposure: UnifyDirectExposureConfig,
    pub visible_surface_changed: bool,
}

fn unify_direct_exposure_references_server(
    config: &UnifyDirectExposureConfig,
    server_id: &str,
) -> bool {
    config.selected_server_ids.iter().any(|id| id == server_id)
        || config
            .selected_tool_surfaces
            .iter()
            .any(|surface| surface.server_id == server_id)
        || config
            .selected_prompt_surfaces
            .iter()
            .any(|surface| surface.server_id == server_id)
        || config
            .selected_resource_surfaces
            .iter()
            .any(|surface| surface.server_id == server_id)
        || config
            .selected_template_surfaces
            .iter()
            .any(|surface| surface.server_id == server_id)
}

fn serialize_json<T: serde::Serialize>(value: &T) -> ConfigResult<String> {
    serde_json::to_string(value).map_err(|err| ConfigError::DataAccessError(err.to_string()))
}

fn can_apply_first_initialize_observation(state: &ClientStateRow) -> ConfigResult<bool> {
    if state.template_identifier().is_some() || state.governance_kind().as_str() != "passive" {
        return Ok(false);
    }

    if state
        .client_version
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        || state.transport.as_deref().is_some_and(|value| {
            let trimmed = value.trim();
            !trimmed.is_empty() && trimmed != "auto"
        })
        || !state.parsed_transports()?.is_empty()
    {
        return Ok(false);
    }

    let metadata = state.runtime_client_metadata();
    Ok(metadata.description.is_none()
        && metadata.homepage_url.is_none()
        && metadata.docs_url.is_none()
        && metadata.support_url.is_none()
        && metadata.logo_url.is_none()
        && metadata.category.is_none())
}

impl ClientConfigService {
    pub async fn set_merge_strategy_override(
        &self,
        identifier: &str,
        strategy: Option<crate::clients::models::MergeStrategy>,
    ) -> ConfigResult<()> {
        if self.fetch_state(identifier).await?.is_none() {
            return Err(ConfigError::DataAccessError(format!("Client not found: {identifier}")));
        }

        match strategy {
            Some(strategy) => {
                let value = match strategy {
                    crate::clients::models::MergeStrategy::Replace => "replace",
                    crate::clients::models::MergeStrategy::DeepMerge => "deep_merge",
                };
                sqlx::query(
                    r#"
                    INSERT INTO client_writeback_policy (client_identifier, merge_strategy)
                    VALUES (?, ?)
                    ON CONFLICT(client_identifier) DO UPDATE SET
                        merge_strategy = excluded.merge_strategy,
                        updated_at = CURRENT_TIMESTAMP
                    "#,
                )
                .bind(identifier)
                .bind(value)
                .execute(&*self.db_pool)
                .await
                .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
            }
            None => {
                sqlx::query("DELETE FROM client_writeback_policy WHERE client_identifier = ?")
                    .bind(identifier)
                    .execute(&*self.db_pool)
                    .await
                    .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
            }
        }

        Ok(())
    }

    pub async fn persist_handshake_observation(
        &self,
        identifier: &str,
        observed_name: Option<&str>,
        client_version: Option<&str>,
        transport: Option<&str>,
        description: Option<&str>,
        homepage_url: Option<&str>,
        logo_url: Option<&str>,
    ) -> ConfigResult<()> {
        let display_name = sanitize_optional(observed_name);
        let client_version = sanitize_optional(client_version);
        let transport = sanitize_optional(transport);
        let description = sanitize_optional(description);
        let homepage_url = sanitize_optional(homepage_url);
        let logo_url = sanitize_optional(logo_url);

        validate_observed_transport(transport.as_deref())?;

        let observed_name = display_name.as_deref().unwrap_or(identifier);
        let existing_state = if let Some(state) = self.fetch_state(identifier).await? {
            self.mark_runtime_observed(identifier).await?;
            if !can_apply_first_initialize_observation(&state)? {
                return Ok(());
            }
            state
        } else {
            let platform = crate::system::paths::PathService::get_current_platform();
            if self.template_source.get_template(identifier, platform).await?.is_some() {
                return Ok(());
            }
            self.ensure_passive_runtime_observed_row(identifier, observed_name)
                .await?
        };

        if let Some(display_name) = display_name.as_deref() {
            self.update_client_names(identifier, display_name).await?;
        }

        if let Some(client_version) = client_version.as_deref() {
            self.update_client_version(identifier, client_version).await?;
        }

        if let Some(transport) = transport.as_deref() {
            self.update_transport(identifier, transport).await?;
        }

        if description.is_some() || homepage_url.is_some() || logo_url.is_some() {
            let existing_metadata = existing_state.runtime_client_metadata();
            let next_metadata = RuntimeClientMetadata {
                description: description.or(existing_metadata.description),
                homepage_url: homepage_url.or(existing_metadata.homepage_url),
                docs_url: existing_metadata.docs_url,
                support_url: existing_metadata.support_url,
                logo_url: logo_url.or(existing_metadata.logo_url),
                category: existing_metadata.category,
            };
            self.update_runtime_client_metadata(identifier, &next_metadata, false)
                .await?;
        }

        if let Some(observed_transport) = transport.as_deref() {
            self.upsert_observed_transport_support(identifier, observed_transport)
                .await?;
        }

        Ok(())
    }

    async fn upsert_observed_transport_support(
        &self,
        identifier: &str,
        observed_transport: &str,
    ) -> ConfigResult<()> {
        let Some(normalized_transport) = canonical_config_transport_key(observed_transport) else {
            if observed_transport.trim().is_empty() {
                return Ok(());
            }
            return Err(ConfigError::DataAccessError(format!(
                "Invalid observed transport '{}'; expected canonical transport key",
                observed_transport.trim()
            )));
        };

        let existing_raw: Option<String> = sqlx::query_scalar("SELECT transports FROM client WHERE identifier = ?")
            .bind(identifier)
            .fetch_optional(&*self.db_pool)
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?
            .flatten();

        let mut transports = existing_raw
            .as_deref()
            .map(serde_json::from_str::<HashMap<String, FormatRule>>)
            .transpose()
            .map_err(|err| ConfigError::DataAccessError(format!("Failed to parse transports: {}", err)))?
            .unwrap_or_default();

        if transports.contains_key(normalized_transport) {
            return Ok(());
        }

        transports.insert(
            normalized_transport.to_string(),
            FormatRule {
                selected: Some(true),
                ..FormatRule::default()
            },
        );

        self.update_transports(identifier, Some(&transports), false).await
    }

    async fn resolve_effective_mode_from_explicit(
        &self,
        explicit_mode: Option<&str>,
    ) -> ConfigResult<String> {
        let default_mode = crate::config::client::init::resolve_default_client_config_mode(&self.db_pool)
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        Ok(crate::config::client::init::effective_client_config_mode(explicit_mode, &default_mode).to_string())
    }

    pub async fn get_effective_config_mode(
        &self,
        identifier: &str,
    ) -> ConfigResult<String> {
        let explicit = self.fetch_state(identifier).await?.and_then(|state| state.config_mode);
        self.resolve_effective_mode_from_explicit(explicit.as_deref()).await
    }

    async fn validate_runtime_target_input(
        &self,
        connection_mode: Option<&str>,
        config_path: Option<&str>,
    ) -> ConfigResult<()> {
        let normalized_path = config_path.map(str::trim).filter(|value| !value.is_empty());

        match connection_mode {
            Some("local_config_detected") => {
                let raw_path = normalized_path.ok_or_else(|| {
                    ConfigError::DataAccessError(
                        "Clients with a local config target must provide a valid MCP config file path.".to_string(),
                    )
                })?;
                self.validate_existing_config_target(raw_path).await?;
            }
            Some("manual") => {
                if normalized_path.is_some() {
                    return Err(ConfigError::DataAccessError(
                        "Only clients with a local config target may store a config file path.".to_string(),
                    ));
                }
            }
            _ => {
                if let Some(raw_path) = normalized_path {
                    self.validate_existing_config_target(raw_path).await?;
                }
            }
        }

        Ok(())
    }

    async fn validate_existing_config_target(
        &self,
        raw_path: &str,
    ) -> ConfigResult<()> {
        let resolved_path = get_path_service()
            .resolve_user_path(raw_path)
            .map_err(|err| ConfigError::PathResolutionError(err.to_string()))?;
        let metadata = tokio::fs::metadata(&resolved_path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                ConfigError::DataAccessError(format!("Configured MCP file does not exist: {}", raw_path))
            } else {
                ConfigError::FileOperationError(format!("Failed to inspect configured MCP file {}: {}", raw_path, err))
            }
        })?;

        if metadata.is_file() {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&resolved_path)
                .await
                .map_err(|_| ConfigError::PathNotWritable { path: resolved_path })?;
        } else if metadata.is_dir() {
            Self::validate_directory_target_writable(&resolved_path).await?;
        } else {
            return Err(ConfigError::DataAccessError(format!(
                "Configured MCP path is neither a file nor a directory: {}",
                raw_path
            )));
        }

        Ok(())
    }

    /// Update client settings (transport and client_version).
    /// - transport: optional, only update if provided; must be one of: auto, sse, stdio, streamable_http
    /// - client_version: optional, only update if provided
    pub async fn set_client_settings(
        &self,
        identifier: &str,
        transport: Option<String>,
        client_version: Option<String>,
    ) -> ConfigResult<()> {
        self.set_active_client_settings(
            identifier,
            ActiveClientSettingsUpdate {
                transport,
                client_version,
                ..ActiveClientSettingsUpdate::default()
            },
        )
        .await
        .map(|_| ())
    }

    pub async fn set_active_client_settings(
        &self,
        identifier: &str,
        update: ActiveClientSettingsUpdate,
    ) -> ConfigResult<ActiveClientSettingsResult> {
        tracing::info!(
            client = %identifier,
            transport = ?update.transport,
            client_version = ?update.client_version,
            config_file_state = ?update.config_file_state,
            config_path = ?update.config_path,
            clear_config_file_parse = %update.clear_config_file_parse,
            "set_active_client_settings: entry"
        );

        if update.clear_merge_strategy_override && update.merge_strategy_override.is_some() {
            return Err(ConfigError::DataAccessError(
                "merge strategy override cannot be set and cleared in the same request".to_string(),
            ));
        }

        if let Some(ref tr) = update.transport {
            if !VALID_TRANSPORTS.contains(&tr.as_str()) {
                let err = format!(
                    "Invalid transport value '{}', must be one of: {}",
                    tr,
                    VALID_TRANSPORTS.join(", ")
                );
                tracing::error!(client = %identifier, transport = %tr, "{}", err);
                return Err(ConfigError::DataAccessError(err));
            }
        }

        let trimmed_display_name = update
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let (name, display_name_source): (String, &'static str) = match trimmed_display_name {
            Some(value) => (value.to_string(), "provided"),
            None => (self.resolve_client_name(identifier).await?, "stored"),
        };
        let existing_state = self.fetch_state(identifier).await?;
        let raw_config_path = update.config_path.as_deref().map(str::trim);
        let normalized_config_path = raw_config_path.filter(|value| !value.is_empty()).map(str::to_string);
        let clear_config_artifacts = matches!(update.config_file_state, Some(ClientConfigFileState::WithoutConfigFile));
        let clear_config_file_parse = update.clear_config_file_parse || clear_config_artifacts;
        let clear_transports = update.clear_transports || clear_config_artifacts;

        let (resolved_connection_mode, config_file_state_source): (Option<String>, &'static str) =
            if let Some(state) = update.config_file_state {
                (
                    Some(config_file_state_to_connection_mode(state).to_string()),
                    "provided",
                )
            } else {
                match raw_config_path {
                    Some("") => (Some("manual".to_string()), "derived"),
                    Some(_) => (Some("local_config_detected".to_string()), "derived"),
                    None => (None, "stored"),
                }
            };

        self.validate_runtime_target_input(resolved_connection_mode.as_deref(), normalized_config_path.as_deref())
            .await?;

        let effective_parse_for_validation = match existing_state.as_ref() {
            Some(state) => state
                .effective_config_file_parse_with(update.config_file_parse.as_ref(), clear_config_file_parse)?
                .or_else(|| update.config_file_parse.clone()),
            None => update.config_file_parse.clone(),
        };
        let validation_path = normalized_config_path
            .as_deref()
            .or_else(|| existing_state.as_ref().and_then(|state| state.config_path()));

        if !clear_config_file_parse && matches!(resolved_connection_mode.as_deref(), Some("local_config_detected")) {
            if let (Some(path), Some(parse)) = (validation_path, effective_parse_for_validation.as_ref()) {
                self.validate_config_file_parse_rule(path, parse).await?;
            }
        }

        let (approval_status, approval_status_source): (String, &'static str) = existing_state
            .as_ref()
            .map(|state| (state.approval_status().to_string(), "stored"))
            .unwrap_or_else(|| ("approved".to_string(), "default"));

        let (_, created) = self
            .ensure_active_state_row_with_creation(identifier, &name, Some(&approval_status))
            .await?;

        self.update_client_names(identifier, &name).await?;

        if let Some(tr) = update.transport {
            self.update_transport(identifier, &tr).await?;
        }

        if let Some(ver) = update.client_version {
            self.update_client_version(identifier, &ver).await?;
        }

        if update.config_path.is_some() || resolved_connection_mode.is_some() {
            self.update_runtime_target(
                identifier,
                normalized_config_path.as_deref(),
                resolved_connection_mode.as_deref(),
                true,
            )
            .await?;
        }

        if update.description.is_some()
            || update.homepage_url.is_some()
            || update.docs_url.is_some()
            || update.support_url.is_some()
            || update.logo_url.is_some()
        {
            let existing_metadata = existing_state
                .as_ref()
                .map(|state| state.runtime_client_metadata())
                .unwrap_or_default();

            tracing::debug!(
                client = %identifier,
                update_logo_url = ?update.logo_url,
                existing_logo_url = ?existing_metadata.logo_url,
                "Merging runtime metadata"
            );

            let next_metadata = RuntimeClientMetadata {
                description: update.description.or(existing_metadata.description),
                homepage_url: update.homepage_url.or(existing_metadata.homepage_url),
                docs_url: update.docs_url.or(existing_metadata.docs_url),
                support_url: update.support_url.or(existing_metadata.support_url),
                logo_url: update.logo_url.or(existing_metadata.logo_url),
                category: existing_metadata.category,
            };

            tracing::debug!(
                client = %identifier,
                merged_logo_url = ?next_metadata.logo_url,
                "Merged runtime metadata, calling update_runtime_client_metadata"
            );

            self.update_runtime_client_metadata(identifier, &next_metadata, true)
                .await?;
        }

        if clear_config_artifacts {
            self.clear_config_file_artifacts(identifier).await?;
        } else if clear_config_file_parse || update.config_file_parse.is_some() {
            self.update_config_file_parse(identifier, update.config_file_parse.as_ref(), clear_config_file_parse)
                .await?;
        }

        if update.clear_merge_strategy_override || update.merge_strategy_override.is_some() {
            self.set_merge_strategy_override(identifier, update.merge_strategy_override)
                .await?;
        }

        if !clear_config_artifacts && (clear_transports || update.transports.is_some()) {
            self.update_transports(identifier, update.transports.as_ref(), clear_transports)
                .await?;
        }

        if !clear_config_artifacts && matches!(resolved_connection_mode.as_deref(), Some("local_config_detected")) {
            self.ensure_local_config_target_metadata(identifier).await?;
        }

        self.notify_managed_consumer_surface_bootstrap(
            identifier,
            "consumer_registration",
            format!("register:{identifier}"),
        )
        .await?;

        tracing::info!(client = %identifier, "set_active_client_settings: complete");
        Ok(ActiveClientSettingsResult {
            created,
            display_name_source,
            approval_status_source,
            config_file_state_source,
        })
    }

    async fn update_client_names(
        &self,
        identifier: &str,
        name: &str,
    ) -> ConfigResult<()> {
        tracing::debug!(client = %identifier, name = %name, "Updating client name and display name");

        sqlx::query(
            r#"
            UPDATE client
            SET name = ?,
                display_name = ?,
                updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(name)
        .bind(name)
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        Ok(())
    }

    /// Update transport protocol
    async fn update_transport(
        &self,
        identifier: &str,
        transport: &str,
    ) -> ConfigResult<()> {
        tracing::info!(client = %identifier, transport = %transport, "Updating transport");

        let result =
            sqlx::query("UPDATE client SET transport = ?, updated_at = CURRENT_TIMESTAMP WHERE identifier = ?")
                .bind(transport)
                .bind(identifier)
                .execute(&*self.db_pool)
                .await
                .map_err(|e| {
                    tracing::error!(
                        client = %identifier,
                        transport = %transport,
                        error = %e,
                        "Failed to update transport"
                    );
                    ConfigError::DataAccessError(e.to_string())
                })?;

        tracing::info!(
            client = %identifier,
            transport = %transport,
            rows_affected = %result.rows_affected(),
            "transport updated"
        );

        Ok(())
    }

    /// Update client_version
    async fn update_client_version(
        &self,
        identifier: &str,
        version: &str,
    ) -> ConfigResult<()> {
        tracing::info!(client = %identifier, version = %version, "Updating client_version");

        let result =
            sqlx::query("UPDATE client SET client_version = ?, updated_at = CURRENT_TIMESTAMP WHERE identifier = ?")
                .bind(version)
                .bind(identifier)
                .execute(&*self.db_pool)
                .await
                .map_err(|e| {
                    tracing::error!(client = %identifier, error = %e, "Failed to update client_version");
                    ConfigError::DataAccessError(e.to_string())
                })?;

        tracing::info!(
            client = %identifier,
            rows_affected = %result.rows_affected(),
            "client_version updated"
        );

        Ok(())
    }

    pub(super) async fn update_runtime_target(
        &self,
        identifier: &str,
        config_path: Option<&str>,
        connection_mode: Option<&str>,
        promote_active: bool,
    ) -> ConfigResult<()> {
        let governance_kind = if promote_active { Some("active") } else { None };
        sqlx::query(
            r#"
            UPDATE client
            SET config_path = CASE
                    WHEN ? IS NOT NULL THEN NULLIF(?, '')
                    WHEN ? = 'manual' THEN NULL
                    ELSE NULLIF(?, '')
                END,
                connection_mode = CASE
                    WHEN ? IS NULL THEN connection_mode
                    ELSE NULLIF(?, '')
                END,
                governance_kind = COALESCE(?, governance_kind),
                updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(config_path)
        .bind(config_path)
        .bind(connection_mode)
        .bind(config_path)
        .bind(connection_mode)
        .bind(connection_mode)
        .bind(governance_kind)
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|e| ConfigError::DataAccessError(e.to_string()))?;

        Ok(())
    }

    async fn update_runtime_client_metadata(
        &self,
        identifier: &str,
        metadata: &RuntimeClientMetadata,
        promote_active: bool,
    ) -> ConfigResult<()> {
        let existing: Option<String> = sqlx::query_scalar("SELECT approval_metadata FROM client WHERE identifier = ?")
            .bind(identifier)
            .fetch_optional(&*self.db_pool)
            .await
            .map_err(|e| ConfigError::DataAccessError(e.to_string()))?;

        let mut payload = existing
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Map<String, Value>>(raw).ok())
            .unwrap_or_default();
        payload.insert("runtime_client".to_string(), json!(metadata));

        let governance_kind = if promote_active { Some("active") } else { None };

        sqlx::query(
            r#"
            UPDATE client
            SET approval_metadata = ?,
                governance_kind = COALESCE(?, governance_kind),
                updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(serde_json::to_string(&payload).map_err(|e| ConfigError::DataAccessError(e.to_string()))?)
        .bind(governance_kind)
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|e| ConfigError::DataAccessError(e.to_string()))?;

        Ok(())
    }

    async fn update_config_file_parse(
        &self,
        identifier: &str,
        config_file_parse: Option<&ClientConfigFileParse>,
        clear_override: bool,
    ) -> ConfigResult<()> {
        let existing_state = self.fetch_state(identifier).await?;
        let serialized_override = if clear_override {
            None
        } else {
            config_file_parse
                .map(|value| serde_json::to_string(value).map_err(|err| ConfigError::DataAccessError(err.to_string())))
                .transpose()?
        };

        let effective_parse = if clear_override && config_file_parse.is_none() {
            None
        } else {
            match existing_state.as_ref() {
                Some(state) => state
                    .effective_config_file_parse_with(config_file_parse, clear_override)?
                    .or_else(|| config_file_parse.cloned()),
                None => config_file_parse.cloned(),
            }
        };

        let config_format = effective_parse.as_ref().map(|value| value.format.as_str().to_string());
        let container_type = effective_parse.as_ref().map(|value| match value.container_type {
            ContainerType::Array => "array".to_string(),
            ContainerType::ObjectMap => "object".to_string(),
        });
        let container_keys = effective_parse
            .as_ref()
            .map(|value| {
                serde_json::to_string(&value.container_keys)
                    .map_err(|err| ConfigError::DataAccessError(err.to_string()))
            })
            .transpose()?;

        sqlx::query(
            r#"
            UPDATE client
            SET config_file_parse = ?,
                config_format = ?,
                container_type = ?,
                container_keys = ?,
                governance_kind = 'active',
                updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(serialized_override)
        .bind(config_format)
        .bind(container_type)
        .bind(container_keys)
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        Ok(())
    }

    async fn clear_config_file_artifacts(
        &self,
        identifier: &str,
    ) -> ConfigResult<()> {
        sqlx::query(
            r#"
            UPDATE client
            SET config_file_parse = NULL,
                config_format = NULL,
                container_type = NULL,
                container_keys = NULL,
                transports = NULL,
                updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        Ok(())
    }

    async fn update_transports(
        &self,
        identifier: &str,
        transports: Option<&HashMap<String, FormatRule>>,
        clear_override: bool,
    ) -> ConfigResult<()> {
        if clear_override {
            sqlx::query(
                r#"
                UPDATE client
                SET transports = NULL,
                    updated_at = CURRENT_TIMESTAMP
                WHERE identifier = ?
                "#,
            )
            .bind(identifier)
            .execute(&*self.db_pool)
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
            return Ok(());
        }

        let Some(rules) = transports else {
            return Ok(());
        };

        for transport in rules.keys() {
            if canonical_config_transport_key(transport).is_none() {
                return Err(ConfigError::DataAccessError(format!(
                    "Invalid transport key '{}'; expected canonical transport key",
                    transport
                )));
            }
        }

        let serialized = serde_json::to_string(rules).map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        sqlx::query(
            r#"
            UPDATE client
            SET transports = ?,
                updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(serialized)
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        Ok(())
    }

    /// Get client settings (config_mode, transport, client_version)
    /// Returns None if client state not found
    pub async fn get_client_settings(
        &self,
        identifier: &str,
    ) -> ConfigResult<Option<(Option<String>, String, Option<String>)>> {
        let state = self.fetch_state(identifier).await?;

        if state.is_none() {
            tracing::debug!(client = %identifier, "Client state not found");
            return Ok(None);
        }

        let state = state.unwrap();
        let transport = state.transport.unwrap_or_else(|| "auto".to_string());

        Ok(Some((state.config_mode, transport, state.client_version)))
    }

    pub async fn set_capability_config(
        &self,
        identifier: &str,
        capability_source: CapabilitySource,
        selected_profile_ids: Vec<String>,
    ) -> ConfigResult<ClientCapabilityConfig> {
        let name = self.resolve_client_name(identifier).await?;
        self.ensure_state_row_with_name(identifier, &name).await?;

        let selected_profile_ids = self.normalize_selected_profile_ids(capability_source, selected_profile_ids)?;
        self.validate_selected_profile_ids(&selected_profile_ids).await?;

        let custom_profile_id = match capability_source {
            CapabilitySource::Activated | CapabilitySource::Profiles => None,
            CapabilitySource::Custom => Some(self.ensure_custom_profile(identifier).await?),
        };
        let selected_profile_ids_json = if selected_profile_ids.is_empty() {
            None
        } else {
            Some(serialize_json(&selected_profile_ids)?)
        };

        sqlx::query(
            r#"
            UPDATE client
            SET capability_source = ?,
                selected_profile_ids = ?,
                custom_profile_id = ?,
                governance_kind = 'active',
                updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(capability_source.as_str())
        .bind(selected_profile_ids_json)
        .bind(custom_profile_id.as_deref())
        .bind(identifier)
        .execute(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        self.get_capability_config(identifier)
            .await?
            .ok_or_else(|| ConfigError::DataAccessError(format!("Failed to load capability config for {identifier}")))
    }

    pub async fn get_capability_config_state(
        &self,
        identifier: &str,
    ) -> ConfigResult<Option<ClientCapabilityConfigState>> {
        let Some(state) = self.fetch_state(identifier).await? else {
            return Ok(None);
        };

        let capability_config = state.capability_config()?;
        let custom_profile_missing = self
            .resolve_custom_profile_missing(
                capability_config.capability_source,
                capability_config.custom_profile_id.as_deref(),
            )
            .await?;
        let raw_unify_direct_exposure = self.load_unify_direct_exposure_intent(identifier).await?;
        let resolved = self
            .resolve_unify_direct_exposure_intent(identifier, &capability_config, &raw_unify_direct_exposure)
            .await?;

        Ok(Some(ClientCapabilityConfigState {
            capability_config,
            custom_profile_missing,
            unify_direct_exposure_intent: resolved.intent,
            unify_direct_exposure: resolved.config,
            unify_direct_exposure_diagnostics: resolved.diagnostics,
        }))
    }

    pub async fn get_unify_direct_exposure_config(
        &self,
        identifier: &str,
    ) -> ConfigResult<Option<UnifyDirectExposureConfig>> {
        Ok(self
            .get_capability_config_state(identifier)
            .await?
            .map(|state| state.unify_direct_exposure))
    }

    pub async fn catalog_revision_set(&self) -> ConfigResult<HashMap<String, i64>> {
        crate::core::capability::materializer::SurfaceAuthoringLoader::load_catalog_revision_set(&self.db_pool)
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))
    }

    pub async fn update_capability_config_state_and_invalidate(
        &self,
        identifier: &str,
        config_mode_update: Option<String>,
        capability_source: CapabilitySource,
        selected_profile_ids: Vec<String>,
        unify_direct_exposure_update: Option<UnifyDirectExposureIntent>,
        source_revision_set: HashMap<String, i64>,
    ) -> ConfigResult<(
        ClientCapabilityConfigState,
        bool,
        Option<crate::core::capability::materializer::MaterializationCommit>,
    )> {
        let visibility_service = crate::core::profile::visibility::ProfileVisibilityService::new(
            Some(Arc::new(Database {
                pool: self.db_pool.as_ref().clone(),
                path: PathBuf::new(),
                capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
            })),
            None,
        );

        let build_client_context =
            |config_mode: &str, unify_direct_exposure: Option<UnifyDirectExposureConfig>| ClientContext {
                client_id: identifier.to_string(),
                session_id: None,
                profile_id: None,
                config_mode: Some(config_mode.to_string()),
                unify_workspace: unify_direct_exposure,
                surface_fingerprint: None,
                transport: ClientTransport::Other,
                source: ClientIdentitySource::ManagedQuery,
                observed_client_info: None,
            };

        let old_state = self.get_capability_config_state(identifier).await?;
        let old_effective_mode = self.get_effective_config_mode(identifier).await?;
        let new_effective_mode = match config_mode_update.as_deref() {
            Some(mode) => self.resolve_effective_mode_from_explicit(Some(mode)).await?,
            None => old_effective_mode.clone(),
        };
        let resolve_unify_workspace = |mode: &str, state: &ClientCapabilityConfigState| {
            (mode == "unify").then(|| state.unify_direct_exposure.clone())
        };
        let old_fingerprint = if let Some(state) = old_state.as_ref() {
            let context =
                build_client_context(&old_effective_mode, resolve_unify_workspace(&old_effective_mode, state));
            Some(
                visibility_service
                    .resolve_snapshot_for_client(&context)
                    .await
                    .map_err(|err| ConfigError::DataAccessError(err.to_string()))?
                    .surface_fingerprint,
            )
        } else {
            None
        };

        let (state, materialization) = self
            .set_capability_config_state(
                identifier,
                config_mode_update,
                capability_source,
                selected_profile_ids,
                unify_direct_exposure_update,
                source_revision_set,
            )
            .await?;

        let new_context = build_client_context(
            &new_effective_mode,
            resolve_unify_workspace(&new_effective_mode, &state),
        );
        let new_fingerprint = visibility_service
            .resolve_snapshot_for_client(&new_context)
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?
            .surface_fingerprint;

        if let Some(ref fingerprint) = old_fingerprint {
            tracing::debug!(
                client = %identifier,
                old_fingerprint = %fingerprint,
                new_fingerprint = %new_fingerprint,
                "Client capability visibility fingerprint changed"
            );
        }

        let has_visible_direct_surface = !state.unify_direct_exposure.selected_tool_surfaces.is_empty()
            || !state.unify_direct_exposure.selected_prompt_surfaces.is_empty()
            || !state.unify_direct_exposure.selected_resource_surfaces.is_empty()
            || !state.unify_direct_exposure.selected_template_surfaces.is_empty();
        let visible_surface_changed = old_fingerprint
            .as_ref()
            .map(|fingerprint| fingerprint != &new_fingerprint)
            .unwrap_or(has_visible_direct_surface);

        Ok((state, visible_surface_changed, materialization))
    }

    pub async fn update_capability_config_and_invalidate(
        &self,
        identifier: &str,
        capability_source: CapabilitySource,
        selected_profile_ids: Vec<String>,
        source_revision_set: HashMap<String, i64>,
    ) -> ConfigResult<ClientCapabilityConfig> {
        self.update_capability_config_state_and_invalidate(
            identifier,
            None,
            capability_source,
            selected_profile_ids,
            None,
            source_revision_set,
        )
        .await
        .map(|(state, _, _)| state.capability_config)
    }

    pub async fn reconcile_unify_direct_exposure_for_server(
        &self,
        server_id: &str,
    ) -> ConfigResult<Vec<ReconciledUnifyDirectExposure>> {
        let states = self.fetch_client_states().await?;
        let mut reconciled = Vec::new();

        for (identifier, row) in states {
            if self.get_effective_config_mode(&identifier).await? != "unify" {
                continue;
            }

            let capability_config = row.capability_config()?;
            let raw_unify_direct_exposure = self.load_unify_direct_exposure_intent(&identifier).await?;
            let resolved = self
                .resolve_unify_direct_exposure_intent(&identifier, &capability_config, &raw_unify_direct_exposure)
                .await?;
            if !unify_direct_exposure_references_server(&resolved.config, server_id) {
                continue;
            }

            let (state, visible_surface_changed, _) = self
                .update_capability_config_state_and_invalidate(
                    &identifier,
                    None,
                    capability_config.capability_source,
                    capability_config.selected_profile_ids,
                    None,
                    crate::core::capability::materializer::SurfaceAuthoringLoader::load_catalog_revision_set(
                        &self.db_pool,
                    )
                    .await
                    .map_err(|error| ConfigError::DataAccessError(error.to_string()))?,
                )
                .await?;

            reconciled.push(ReconciledUnifyDirectExposure {
                identifier,
                unify_direct_exposure: state.unify_direct_exposure,
                visible_surface_changed,
            });
        }

        Ok(reconciled)
    }

    pub async fn get_capability_config(
        &self,
        identifier: &str,
    ) -> ConfigResult<Option<ClientCapabilityConfig>> {
        Ok(self
            .get_capability_config_state(identifier)
            .await?
            .map(|state| state.capability_config))
    }

    async fn set_capability_config_state(
        &self,
        identifier: &str,
        config_mode_update: Option<String>,
        capability_source: CapabilitySource,
        selected_profile_ids: Vec<String>,
        unify_direct_exposure_update: Option<UnifyDirectExposureIntent>,
        source_revision_set: HashMap<String, i64>,
    ) -> ConfigResult<(
        ClientCapabilityConfigState,
        Option<crate::core::capability::materializer::MaterializationCommit>,
    )> {
        let name = self.resolve_client_name(identifier).await?;
        let prepared_client = self.prepare_capability_client_insert(identifier, &name).await?;

        let selected_profile_ids = self.normalize_selected_profile_ids(capability_source, selected_profile_ids)?;
        self.validate_selected_profile_ids(&selected_profile_ids).await?;

        let prepared_custom_profile = match capability_source {
            CapabilitySource::Activated | CapabilitySource::Profiles => None,
            CapabilitySource::Custom => Some(self.prepare_custom_profile(identifier).await?),
        };
        let mut custom_profile_id = prepared_custom_profile.as_ref().map(|profile| profile.id.clone());
        let custom_profile_missing = false;
        let selected_profile_ids_json = if selected_profile_ids.is_empty() {
            None
        } else {
            Some(
                serde_json::to_string(&selected_profile_ids)
                    .map_err(|err| ConfigError::DataAccessError(err.to_string()))?,
            )
        };

        let existing_unify_direct_exposure = self.load_unify_direct_exposure_intent(identifier).await?;
        let requested_unify_direct_exposure = self.normalize_unify_direct_exposure_intent(
            unify_direct_exposure_update.unwrap_or(existing_unify_direct_exposure),
        );
        let resolved_unify_direct_exposure = self
            .resolve_unify_direct_exposure_intent(
                identifier,
                &ClientCapabilityConfig {
                    capability_source,
                    selected_profile_ids: selected_profile_ids.clone(),
                    custom_profile_id: custom_profile_id.clone(),
                },
                &requested_unify_direct_exposure,
            )
            .await?;
        let default_config_mode = crate::config::client::init::resolve_default_client_config_mode(&self.db_pool)
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        let mut transaction = self
            .db_pool
            .begin()
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        MaterializationCoordinator::new(self.db_pool.as_ref().clone())
            .verify_catalog_revision_set_in_transaction(&mut transaction, &source_revision_set)
            .await
            .map_err(|error| match error {
                mcpmate_capability_store::CatalogError::ConcurrencyConflict { .. } => {
                    ConfigError::ConcurrencyConflict {
                        details: error.to_string(),
                    }
                }
                _ => ConfigError::DataAccessError(error.to_string()),
            })?;
        self.ensure_capability_client_in_transaction(&mut transaction, identifier, &name, &prepared_client)
            .await?;
        if let Some(prepared_profile) = &prepared_custom_profile {
            custom_profile_id = Some(
                self.ensure_custom_profile_in_transaction(&mut transaction, identifier, prepared_profile)
                    .await?,
            );
        }
        let consumer_id = identifier.to_string();
        sqlx::query(
            r#"
            UPDATE client
            SET config_mode = COALESCE(?, config_mode),
                capability_source = ?,
                selected_profile_ids = ?,
                custom_profile_id = ?,
                governance_kind = 'active',
                updated_at = CURRENT_TIMESTAMP
            WHERE identifier = ?
            "#,
        )
        .bind(config_mode_update.as_deref())
        .bind(capability_source.as_str())
        .bind(selected_profile_ids_json)
        .bind(custom_profile_id.as_deref())
        .bind(identifier)
        .execute(&mut *transaction)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        self.persist_unify_direct_exposure_intent(&mut transaction, &consumer_id, &requested_unify_direct_exposure)
            .await?;
        let managed_state: (Option<String>, String) =
            sqlx::query_as("SELECT config_mode, approval_status FROM client WHERE identifier = ?")
                .bind(&consumer_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        let effective_config_mode =
            crate::config::client::init::effective_client_config_mode(managed_state.0.as_deref(), &default_config_mode);
        let materialization = if managed_state.1 == "approved"
            && crate::config::client::init::is_managed_client_config_mode(effective_config_mode)
        {
            self.materialize_managed_surface_in_transaction(
                &mut transaction,
                &consumer_id,
                &default_config_mode,
                &MaterializationTrigger::for_consumer(
                    "management_save",
                    format!("client-capability-config:{consumer_id}"),
                    "client_management",
                ),
            )
            .await
            .map_err(|error| match error {
                mcpmate_capability_store::CatalogError::ConcurrencyConflict { .. } => {
                    ConfigError::ConcurrencyConflict {
                        details: error.to_string(),
                    }
                }
                _ => ConfigError::DataAccessError(error.to_string()),
            })?
        } else {
            revoke_managed_surface_in_transaction(
                self.db_pool.as_ref(),
                &mut transaction,
                &consumer_id,
                "client-capability-config",
            )
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
            None
        };
        transaction
            .commit()
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;

        Ok((
            ClientCapabilityConfigState {
                capability_config: ClientCapabilityConfig {
                    capability_source,
                    selected_profile_ids,
                    custom_profile_id,
                },
                custom_profile_missing,
                unify_direct_exposure_intent: requested_unify_direct_exposure,
                unify_direct_exposure: resolved_unify_direct_exposure.config,
                unify_direct_exposure_diagnostics: resolved_unify_direct_exposure.diagnostics,
            },
            materialization,
        ))
    }

    async fn load_unify_direct_exposure_intent(
        &self,
        identifier: &str,
    ) -> ConfigResult<UnifyDirectExposureIntent> {
        let consumer: Option<(String, String)> =
            sqlx::query_as("SELECT identifier, unify_route_mode FROM client WHERE identifier = ?")
                .bind(identifier)
                .fetch_optional(&*self.db_pool)
                .await
                .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        let Some((consumer_id, route_mode)) = consumer else {
            return Ok(UnifyDirectExposureIntent::default());
        };
        let route_mode = route_mode.parse::<UnifyRouteMode>().map_err(|error| {
            ConfigError::DataAccessError(format!(
                "Invalid unify route mode '{route_mode}' for Consumer '{consumer_id}': {error}"
            ))
        })?;
        let server_ids: Vec<String> = sqlx::query_scalar(
            "SELECT server_id FROM direct_exposure_servers WHERE consumer_id = ? ORDER BY server_id",
        )
        .bind(&consumer_id)
        .fetch_all(&*self.db_pool)
        .await
        .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        let refs: Vec<(String, String)> = sqlx::query_as(
            r#"
            SELECT der.ref_id, cr.kind
            FROM direct_exposure_refs der
            JOIN capability_refs cr ON cr.ref_id = der.ref_id
            WHERE der.consumer_id = ? AND der.enabled = 1
            ORDER BY cr.kind, der.ref_id
            "#,
        )
        .bind(&consumer_id)
        .fetch_all(&*self.db_pool)
        .await
        .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        let mut capability_refs = UnifyDirectCapabilityRefs::default();
        for (ref_id, kind) in refs {
            match kind.as_str() {
                "tools" => capability_refs.tool_refs.push(ref_id),
                "prompts" => capability_refs.prompt_refs.push(ref_id),
                "resources" => capability_refs.resource_refs.push(ref_id),
                "resource_templates" => capability_refs.template_refs.push(ref_id),
                _ => {
                    return Err(ConfigError::DataAccessError(format!(
                        "Unknown Capability kind '{kind}' for Direct Exposure"
                    )));
                }
            }
        }
        Ok(UnifyDirectExposureIntent {
            route_mode,
            server_ids,
            capability_refs,
        })
    }

    async fn persist_unify_direct_exposure_intent(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        consumer_id: &str,
        intent: &UnifyDirectExposureIntent,
    ) -> ConfigResult<()> {
        let capability_refs = if intent.route_mode == crate::clients::models::UnifyRouteMode::CapabilityLevel {
            let refs = self.normalize_unify_direct_capability_refs(intent.capability_refs.clone());
            self.validate_unify_direct_capability_ref_kinds(transaction, &refs)
                .await?;
            Some(refs)
        } else {
            None
        };
        sqlx::query("UPDATE client SET unify_route_mode = ?, updated_at = CURRENT_TIMESTAMP WHERE identifier = ?")
            .bind(intent.route_mode.as_str())
            .bind(consumer_id)
            .execute(&mut **transaction)
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        sqlx::query("DELETE FROM direct_exposure_refs WHERE consumer_id = ?")
            .bind(consumer_id)
            .execute(&mut **transaction)
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        sqlx::query("DELETE FROM direct_exposure_servers WHERE consumer_id = ?")
            .bind(consumer_id)
            .execute(&mut **transaction)
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        match intent.route_mode {
            crate::clients::models::UnifyRouteMode::BrokerOnly => {}
            crate::clients::models::UnifyRouteMode::ServerLevel => {
                for server_id in self.normalize_selected_server_ids_for_unify(intent.server_ids.clone()) {
                    sqlx::query(
                        "INSERT INTO direct_exposure_servers (consumer_id, server_id, new_ref_policy) VALUES (?, ?, 'follow')",
                    )
                    .bind(consumer_id)
                    .bind(server_id)
                    .execute(&mut **transaction)
                    .await
                    .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
                }
            }
            crate::clients::models::UnifyRouteMode::CapabilityLevel => {
                let refs = capability_refs.expect("capability-level Direct Exposure validates typed refs");
                for ref_id in refs
                    .tool_refs
                    .into_iter()
                    .chain(refs.prompt_refs)
                    .chain(refs.resource_refs)
                    .chain(refs.template_refs)
                {
                    sqlx::query("INSERT INTO direct_exposure_refs (consumer_id, ref_id, enabled) VALUES (?, ?, 1)")
                        .bind(consumer_id)
                        .bind(ref_id)
                        .execute(&mut **transaction)
                        .await
                        .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
                }
            }
        }
        Ok(())
    }

    async fn validate_unify_direct_capability_ref_kinds(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        refs: &UnifyDirectCapabilityRefs,
    ) -> ConfigResult<()> {
        for (expected_kind, ref_ids) in [
            ("tools", &refs.tool_refs),
            ("prompts", &refs.prompt_refs),
            ("resources", &refs.resource_refs),
            ("resource_templates", &refs.template_refs),
        ] {
            for ref_id in ref_ids {
                let actual_kind: Option<String> =
                    sqlx::query_scalar("SELECT kind FROM capability_refs WHERE ref_id = ?")
                        .bind(ref_id)
                        .fetch_optional(&mut **transaction)
                        .await
                        .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
                let actual_kind = actual_kind.ok_or_else(|| {
                    ConfigError::DataAccessError(format!("Direct Exposure Capability Ref not found: {ref_id}"))
                })?;
                if actual_kind != expected_kind {
                    return Err(ConfigError::DataAccessError(format!(
                        "Direct Exposure Capability Ref {ref_id} expected {expected_kind} but catalog Ref is {actual_kind}"
                    )));
                }
            }
        }
        Ok(())
    }

    async fn resolve_custom_profile_missing(
        &self,
        capability_source: CapabilitySource,
        custom_profile_id: Option<&str>,
    ) -> ConfigResult<bool> {
        if capability_source != CapabilitySource::Custom {
            return Ok(false);
        }

        let Some(profile_id) = custom_profile_id.filter(|value| !value.trim().is_empty()) else {
            return Ok(true);
        };

        Ok(crate::config::profile::get_profile(self.db_pool.as_ref(), profile_id)
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?
            .is_none())
    }

    async fn resolve_unify_direct_exposure_intent(
        &self,
        identifier: &str,
        capability_config: &ClientCapabilityConfig,
        intent: &UnifyDirectExposureIntent,
    ) -> ConfigResult<ResolvedUnifyDirectExposureState> {
        let inventory = self.load_unify_direct_exposure_inventory().await?;
        let visible_server_ids = self
            .resolve_visible_server_ids_for_unify_direct_exposure(identifier, capability_config)
            .await?;
        let mut diagnostics = UnifyDirectExposureDiagnostics::default();

        let requested_server_ids = self.normalize_selected_server_ids_for_unify(intent.server_ids.clone());
        let selected_server_ids = requested_server_ids
            .iter()
            .cloned()
            .filter_map(|server_id| {
                if !visible_server_ids.contains(&server_id) {
                    diagnostics.invalid_server_ids.push(server_id);
                    None
                } else if inventory.tools.contains_key(&server_id)
                    || inventory.prompts.contains_key(&server_id)
                    || inventory.resources.contains_key(&server_id)
                    || inventory.templates.contains_key(&server_id)
                {
                    Some(server_id)
                } else {
                    diagnostics.invalid_server_ids.push(server_id);
                    None
                }
            })
            .collect::<Vec<_>>();

        let server_level_tool_surfaces = if intent.route_mode == crate::clients::models::UnifyRouteMode::ServerLevel {
            self.materialize_tool_surfaces_for_servers(&selected_server_ids, &inventory)
        } else {
            Vec::new()
        };
        let server_level_prompt_surfaces = if intent.route_mode == crate::clients::models::UnifyRouteMode::ServerLevel {
            self.materialize_prompt_surfaces_for_servers(&selected_server_ids, &inventory)
        } else {
            Vec::new()
        };
        let server_level_resource_surfaces = if intent.route_mode == crate::clients::models::UnifyRouteMode::ServerLevel
        {
            self.materialize_resource_surfaces_for_servers(&selected_server_ids, &inventory)
        } else {
            Vec::new()
        };
        let server_level_template_surfaces = if intent.route_mode == crate::clients::models::UnifyRouteMode::ServerLevel
        {
            self.materialize_template_surfaces_for_servers(&selected_server_ids, &inventory)
        } else {
            Vec::new()
        };

        let capability_level_tool_surfaces =
            if intent.route_mode == crate::clients::models::UnifyRouteMode::CapabilityLevel {
                self.resolve_tool_surfaces_for_capability_refs(
                    &intent.capability_refs.tool_refs,
                    &inventory,
                    &mut diagnostics,
                )
            } else {
                Vec::new()
            };
        let capability_level_prompt_surfaces =
            if intent.route_mode == crate::clients::models::UnifyRouteMode::CapabilityLevel {
                self.resolve_prompt_surfaces_for_capability_refs(
                    &intent.capability_refs.prompt_refs,
                    &inventory,
                    &mut diagnostics,
                )
            } else {
                Vec::new()
            };
        let capability_level_resource_surfaces =
            if intent.route_mode == crate::clients::models::UnifyRouteMode::CapabilityLevel {
                self.resolve_resource_surfaces_for_capability_refs(
                    &intent.capability_refs.resource_refs,
                    &inventory,
                    &mut diagnostics,
                )
            } else {
                Vec::new()
            };
        let capability_level_template_surfaces =
            if intent.route_mode == crate::clients::models::UnifyRouteMode::CapabilityLevel {
                self.resolve_template_surfaces_for_capability_refs(
                    &intent.capability_refs.template_refs,
                    &inventory,
                    &mut diagnostics,
                )
            } else {
                Vec::new()
            };

        let selected_tool_surfaces = self.normalize_selected_tool_surfaces(
            capability_level_tool_surfaces
                .into_iter()
                .chain(server_level_tool_surfaces)
                .collect(),
        );
        let selected_tool_surfaces = selected_tool_surfaces
            .into_iter()
            .filter_map(|surface| {
                if !visible_server_ids.contains(&surface.server_id) {
                    diagnostics
                        .invalid_tool_surfaces
                        .push(UnifyDirectToolSurfaceDiagnostic {
                            server_id: surface.server_id,
                            tool_name: surface.tool_name,
                            reason: "server_not_visible".to_string(),
                        });
                    return None;
                }

                let Some(tool_names) = inventory.tools.get(&surface.server_id) else {
                    diagnostics
                        .invalid_tool_surfaces
                        .push(UnifyDirectToolSurfaceDiagnostic {
                            server_id: surface.server_id,
                            tool_name: surface.tool_name,
                            reason: "server_not_eligible_or_missing".to_string(),
                        });
                    return None;
                };

                if tool_names.contains(&surface.tool_name) {
                    Some(surface)
                } else {
                    diagnostics
                        .invalid_tool_surfaces
                        .push(UnifyDirectToolSurfaceDiagnostic {
                            server_id: surface.server_id,
                            tool_name: surface.tool_name,
                            reason: "tool_not_found".to_string(),
                        });
                    None
                }
            })
            .collect::<Vec<_>>();
        let selected_prompt_surfaces = self.normalize_selected_prompt_surfaces(
            capability_level_prompt_surfaces
                .into_iter()
                .chain(server_level_prompt_surfaces)
                .collect(),
        );
        let selected_prompt_surfaces = selected_prompt_surfaces
            .into_iter()
            .filter_map(|surface| {
                if !visible_server_ids.contains(&surface.server_id) {
                    diagnostics
                        .invalid_prompt_surfaces
                        .push(UnifyDirectPromptSurfaceDiagnostic {
                            server_id: surface.server_id,
                            prompt_name: surface.prompt_name,
                            reason: "server_not_visible".to_string(),
                        });
                    return None;
                }
                let Some(prompt_names) = inventory.prompts.get(&surface.server_id) else {
                    diagnostics
                        .invalid_prompt_surfaces
                        .push(UnifyDirectPromptSurfaceDiagnostic {
                            server_id: surface.server_id,
                            prompt_name: surface.prompt_name,
                            reason: "server_not_eligible_or_missing".to_string(),
                        });
                    return None;
                };
                if prompt_names.contains(&surface.prompt_name) {
                    Some(surface)
                } else {
                    diagnostics
                        .invalid_prompt_surfaces
                        .push(UnifyDirectPromptSurfaceDiagnostic {
                            server_id: surface.server_id,
                            prompt_name: surface.prompt_name,
                            reason: "prompt_not_found".to_string(),
                        });
                    None
                }
            })
            .collect::<Vec<_>>();
        let selected_resource_surfaces = self.normalize_selected_resource_surfaces(
            capability_level_resource_surfaces
                .into_iter()
                .chain(server_level_resource_surfaces)
                .collect(),
        );
        let selected_resource_surfaces = selected_resource_surfaces
            .into_iter()
            .filter_map(|surface| {
                if !visible_server_ids.contains(&surface.server_id) {
                    diagnostics
                        .invalid_resource_surfaces
                        .push(UnifyDirectResourceSurfaceDiagnostic {
                            server_id: surface.server_id,
                            resource_uri: surface.resource_uri,
                            reason: "server_not_visible".to_string(),
                        });
                    return None;
                }
                let Some(resource_uris) = inventory.resources.get(&surface.server_id) else {
                    diagnostics
                        .invalid_resource_surfaces
                        .push(UnifyDirectResourceSurfaceDiagnostic {
                            server_id: surface.server_id,
                            resource_uri: surface.resource_uri,
                            reason: "server_not_eligible_or_missing".to_string(),
                        });
                    return None;
                };
                if resource_uris.contains(&surface.resource_uri) {
                    Some(surface)
                } else {
                    diagnostics
                        .invalid_resource_surfaces
                        .push(UnifyDirectResourceSurfaceDiagnostic {
                            server_id: surface.server_id,
                            resource_uri: surface.resource_uri,
                            reason: "resource_not_found".to_string(),
                        });
                    None
                }
            })
            .collect::<Vec<_>>();
        let selected_template_surfaces = self.normalize_selected_template_surfaces(
            capability_level_template_surfaces
                .into_iter()
                .chain(server_level_template_surfaces)
                .collect(),
        );
        let selected_template_surfaces = selected_template_surfaces
            .into_iter()
            .filter_map(|surface| {
                if !visible_server_ids.contains(&surface.server_id) {
                    diagnostics
                        .invalid_template_surfaces
                        .push(UnifyDirectTemplateSurfaceDiagnostic {
                            server_id: surface.server_id,
                            uri_template: surface.uri_template,
                            reason: "server_not_visible".to_string(),
                        });
                    return None;
                }
                let Some(uri_templates) = inventory.templates.get(&surface.server_id) else {
                    diagnostics
                        .invalid_template_surfaces
                        .push(UnifyDirectTemplateSurfaceDiagnostic {
                            server_id: surface.server_id,
                            uri_template: surface.uri_template,
                            reason: "server_not_eligible_or_missing".to_string(),
                        });
                    return None;
                };
                if uri_templates.contains(&surface.uri_template) {
                    Some(surface)
                } else {
                    diagnostics
                        .invalid_template_surfaces
                        .push(UnifyDirectTemplateSurfaceDiagnostic {
                            server_id: surface.server_id,
                            uri_template: surface.uri_template,
                            reason: "template_not_found".to_string(),
                        });
                    None
                }
            })
            .collect::<Vec<_>>();

        diagnostics.invalid_server_ids.sort();
        diagnostics.invalid_server_ids.dedup();
        diagnostics.invalid_tool_surfaces.sort_by(|left, right| {
            left.server_id
                .cmp(&right.server_id)
                .then(left.tool_name.cmp(&right.tool_name))
                .then(left.reason.cmp(&right.reason))
        });
        diagnostics.invalid_tool_surfaces.dedup();
        diagnostics.invalid_prompt_surfaces.sort_by(|left, right| {
            left.server_id
                .cmp(&right.server_id)
                .then(left.prompt_name.cmp(&right.prompt_name))
                .then(left.reason.cmp(&right.reason))
        });
        diagnostics.invalid_prompt_surfaces.dedup();
        diagnostics.invalid_resource_surfaces.sort_by(|left, right| {
            left.server_id
                .cmp(&right.server_id)
                .then(left.resource_uri.cmp(&right.resource_uri))
                .then(left.reason.cmp(&right.reason))
        });
        diagnostics.invalid_resource_surfaces.dedup();
        diagnostics.invalid_template_surfaces.sort_by(|left, right| {
            left.server_id
                .cmp(&right.server_id)
                .then(left.uri_template.cmp(&right.uri_template))
                .then(left.reason.cmp(&right.reason))
        });
        diagnostics.invalid_template_surfaces.dedup();
        diagnostics.invalid_capability_refs.sort();
        diagnostics.invalid_capability_refs.dedup();

        let resolved_intent = UnifyDirectExposureIntent {
            route_mode: intent.route_mode,
            server_ids: if intent.route_mode == crate::clients::models::UnifyRouteMode::ServerLevel {
                requested_server_ids
            } else {
                Vec::new()
            },
            capability_refs: if intent.route_mode == crate::clients::models::UnifyRouteMode::CapabilityLevel {
                self.normalize_unify_direct_capability_refs(intent.capability_refs.clone())
            } else {
                UnifyDirectCapabilityRefs::default()
            },
        };

        Ok(ResolvedUnifyDirectExposureState {
            intent: resolved_intent,
            config: UnifyDirectExposureConfig {
                route_mode: intent.route_mode,
                selected_server_ids: if intent.route_mode == crate::clients::models::UnifyRouteMode::ServerLevel {
                    selected_server_ids
                } else {
                    Vec::new()
                },
                selected_tool_surfaces,
                selected_prompt_surfaces,
                selected_resource_surfaces,
                selected_template_surfaces,
            },
            diagnostics,
        })
    }

    async fn load_unify_direct_exposure_inventory(&self) -> ConfigResult<UnifyDirectExposureInventory> {
        let tool_rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            r#"
            SELECT sc.id, cr.origin_key, cr.ref_id
            FROM server_config sc
            LEFT JOIN capability_refs cr
              ON cr.server_id = sc.id
             AND cr.kind = 'tools'
             AND cr.state = 'active'
            WHERE sc.enabled = 1 AND sc.unify_direct_exposure_eligible = 1
            ORDER BY sc.id, cr.origin_key
            "#,
        )
        .fetch_all(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        let prompt_rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            r#"
            SELECT sc.id, cr.origin_key, cr.ref_id
            FROM server_config sc
            LEFT JOIN capability_refs cr
              ON cr.server_id = sc.id
             AND cr.kind = 'prompts'
             AND cr.state = 'active'
            WHERE sc.enabled = 1 AND sc.unify_direct_exposure_eligible = 1
            ORDER BY sc.id, cr.origin_key
            "#,
        )
        .fetch_all(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        let resource_rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            r#"
            SELECT sc.id, cr.origin_key, cr.ref_id
            FROM server_config sc
            LEFT JOIN capability_refs cr
              ON cr.server_id = sc.id
             AND cr.kind = 'resources'
             AND cr.state = 'active'
            WHERE sc.enabled = 1 AND sc.unify_direct_exposure_eligible = 1
            ORDER BY sc.id, cr.origin_key
            "#,
        )
        .fetch_all(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        let template_rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            r#"
            SELECT sc.id, cr.origin_key, cr.ref_id
            FROM server_config sc
            LEFT JOIN capability_refs cr
              ON cr.server_id = sc.id
             AND cr.kind = 'resource_templates'
             AND cr.state = 'active'
            WHERE sc.enabled = 1 AND sc.unify_direct_exposure_eligible = 1
            ORDER BY sc.id, cr.origin_key
            "#,
        )
        .fetch_all(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        let mut inventory = UnifyDirectExposureInventory::default();
        for (server_id, tool_name, unique_name) in tool_rows {
            let entry = inventory.tools.entry(server_id.clone()).or_default();
            if let Some(tool_name) = tool_name {
                entry.insert(tool_name.clone());
                if let Some(unique_name) = unique_name {
                    inventory
                        .tool_refs
                        .insert(unique_name, UnifyDirectToolSurface { server_id, tool_name });
                }
            }
        }
        for (server_id, prompt_name, unique_name) in prompt_rows {
            let entry = inventory.prompts.entry(server_id.clone()).or_default();
            if let Some(prompt_name) = prompt_name {
                entry.insert(prompt_name.clone());
                if let Some(unique_name) = unique_name {
                    inventory
                        .prompt_refs
                        .insert(unique_name, UnifyDirectPromptSurface { server_id, prompt_name });
                }
            }
        }
        for (server_id, resource_uri, unique_uri) in resource_rows {
            let entry = inventory.resources.entry(server_id.clone()).or_default();
            if let Some(resource_uri) = resource_uri {
                entry.insert(resource_uri.clone());
                if let Some(unique_uri) = unique_uri {
                    inventory.resource_refs.insert(
                        unique_uri,
                        UnifyDirectResourceSurface {
                            server_id,
                            resource_uri,
                        },
                    );
                }
            }
        }
        for (server_id, uri_template, unique_name) in template_rows {
            let entry = inventory.templates.entry(server_id.clone()).or_default();
            if let Some(uri_template) = uri_template {
                entry.insert(uri_template.clone());
                if let Some(unique_name) = unique_name {
                    inventory.template_refs.insert(
                        unique_name,
                        UnifyDirectTemplateSurface {
                            server_id,
                            uri_template,
                        },
                    );
                }
            }
        }

        Ok(inventory)
    }

    async fn resolve_visible_server_ids_for_capability_config(
        &self,
        identifier: &str,
        capability_config: &ClientCapabilityConfig,
    ) -> ConfigResult<HashSet<String>> {
        let profile_ids = self
            .resolve_profile_ids_for_capability_config(identifier, capability_config)
            .await?;
        let server_ids = if profile_ids.is_empty() {
            if capability_config.capability_source == CapabilitySource::Activated {
                sqlx::query_scalar::<_, String>(
                    r#"
                    SELECT id
                    FROM server_config
                    WHERE enabled = 1
                    ORDER BY name, id
                    "#,
                )
                .fetch_all(&*self.db_pool)
                .await
                .map_err(|err| ConfigError::DataAccessError(err.to_string()))?
            } else {
                Vec::new()
            }
        } else {
            let placeholders = vec!["?"; profile_ids.len()].join(", ");
            let sql = format!(
                r#"
                SELECT DISTINCT sc.id
                FROM server_config sc
                WHERE sc.enabled = 1
                  AND (
                    EXISTS (
                      SELECT 1 FROM profile_server_relationships psr
                      WHERE psr.profile_id IN ({placeholders})
                        AND psr.server_id = sc.id
                        AND psr.enabled = 1
                    )
                    OR EXISTS (
                      SELECT 1
                      FROM profile_capability_refs pcr
                      JOIN capability_refs cr ON cr.ref_id = pcr.ref_id
                      WHERE pcr.profile_id IN ({placeholders})
                        AND cr.server_id = sc.id
                        AND pcr.enabled = 1
                        AND NOT EXISTS (
                          SELECT 1
                          FROM profile_server_relationships gate
                          WHERE gate.profile_id = pcr.profile_id
                            AND gate.server_id = cr.server_id
                            AND gate.enabled = 0
                        )
                    )
                  )
                ORDER BY sc.name, sc.id
                "#,
            );
            let mut query = sqlx::query_scalar::<_, String>(&sql);
            for profile_id in &profile_ids {
                query = query.bind(profile_id);
            }
            for profile_id in &profile_ids {
                query = query.bind(profile_id);
            }
            query
                .fetch_all(&*self.db_pool)
                .await
                .map_err(|err| ConfigError::DataAccessError(err.to_string()))?
        };

        Ok(server_ids.into_iter().collect())
    }

    async fn resolve_visible_server_ids_for_unify_direct_exposure(
        &self,
        identifier: &str,
        capability_config: &ClientCapabilityConfig,
    ) -> ConfigResult<HashSet<String>> {
        match capability_config.capability_source {
            CapabilitySource::Activated => {
                let server_ids = sqlx::query_scalar::<_, String>(
                    r#"
                    SELECT id
                    FROM server_config
                    WHERE enabled = 1
                    ORDER BY name, id
                    "#,
                )
                .fetch_all(&*self.db_pool)
                .await
                .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

                Ok(server_ids.into_iter().collect())
            }
            CapabilitySource::Profiles | CapabilitySource::Custom => {
                self.resolve_visible_server_ids_for_capability_config(identifier, capability_config)
                    .await
            }
        }
    }

    fn resolve_tool_surfaces_for_capability_refs(
        &self,
        capability_refs: &[String],
        inventory: &UnifyDirectExposureInventory,
        diagnostics: &mut UnifyDirectExposureDiagnostics,
    ) -> Vec<UnifyDirectToolSurface> {
        self.normalize_unify_direct_ids(capability_refs.to_vec())
            .into_iter()
            .filter_map(|capability_id| match inventory.tool_refs.get(&capability_id) {
                Some(surface) => Some(surface.clone()),
                None => {
                    diagnostics.invalid_capability_refs.push(capability_id);
                    None
                }
            })
            .collect()
    }

    fn resolve_prompt_surfaces_for_capability_refs(
        &self,
        capability_refs: &[String],
        inventory: &UnifyDirectExposureInventory,
        diagnostics: &mut UnifyDirectExposureDiagnostics,
    ) -> Vec<UnifyDirectPromptSurface> {
        self.normalize_unify_direct_ids(capability_refs.to_vec())
            .into_iter()
            .filter_map(|capability_id| match inventory.prompt_refs.get(&capability_id) {
                Some(surface) => Some(surface.clone()),
                None => {
                    diagnostics.invalid_capability_refs.push(capability_id);
                    None
                }
            })
            .collect()
    }

    fn resolve_resource_surfaces_for_capability_refs(
        &self,
        capability_refs: &[String],
        inventory: &UnifyDirectExposureInventory,
        diagnostics: &mut UnifyDirectExposureDiagnostics,
    ) -> Vec<UnifyDirectResourceSurface> {
        self.normalize_unify_direct_ids(capability_refs.to_vec())
            .into_iter()
            .filter_map(|capability_id| match inventory.resource_refs.get(&capability_id) {
                Some(surface) => Some(surface.clone()),
                None => {
                    diagnostics.invalid_capability_refs.push(capability_id);
                    None
                }
            })
            .collect()
    }

    fn resolve_template_surfaces_for_capability_refs(
        &self,
        capability_refs: &[String],
        inventory: &UnifyDirectExposureInventory,
        diagnostics: &mut UnifyDirectExposureDiagnostics,
    ) -> Vec<UnifyDirectTemplateSurface> {
        self.normalize_unify_direct_ids(capability_refs.to_vec())
            .into_iter()
            .filter_map(|capability_id| match inventory.template_refs.get(&capability_id) {
                Some(surface) => Some(surface.clone()),
                None => {
                    diagnostics.invalid_capability_refs.push(capability_id);
                    None
                }
            })
            .collect()
    }

    fn materialize_tool_surfaces_for_servers(
        &self,
        server_ids: &[String],
        inventory: &UnifyDirectExposureInventory,
    ) -> Vec<UnifyDirectToolSurface> {
        server_ids
            .iter()
            .flat_map(|server_id| {
                inventory.tools.get(server_id).into_iter().flat_map(|tool_names| {
                    tool_names.iter().map(|tool_name| UnifyDirectToolSurface {
                        server_id: server_id.clone(),
                        tool_name: tool_name.clone(),
                    })
                })
            })
            .collect()
    }

    fn materialize_prompt_surfaces_for_servers(
        &self,
        server_ids: &[String],
        inventory: &UnifyDirectExposureInventory,
    ) -> Vec<UnifyDirectPromptSurface> {
        server_ids
            .iter()
            .flat_map(|server_id| {
                inventory.prompts.get(server_id).into_iter().flat_map(|prompt_names| {
                    prompt_names.iter().map(|prompt_name| UnifyDirectPromptSurface {
                        server_id: server_id.clone(),
                        prompt_name: prompt_name.clone(),
                    })
                })
            })
            .collect()
    }

    fn materialize_resource_surfaces_for_servers(
        &self,
        server_ids: &[String],
        inventory: &UnifyDirectExposureInventory,
    ) -> Vec<UnifyDirectResourceSurface> {
        server_ids
            .iter()
            .flat_map(|server_id| {
                inventory
                    .resources
                    .get(server_id)
                    .into_iter()
                    .flat_map(|resource_uris| {
                        resource_uris.iter().map(|resource_uri| UnifyDirectResourceSurface {
                            server_id: server_id.clone(),
                            resource_uri: resource_uri.clone(),
                        })
                    })
            })
            .collect()
    }

    fn materialize_template_surfaces_for_servers(
        &self,
        server_ids: &[String],
        inventory: &UnifyDirectExposureInventory,
    ) -> Vec<UnifyDirectTemplateSurface> {
        server_ids
            .iter()
            .flat_map(|server_id| {
                inventory
                    .templates
                    .get(server_id)
                    .into_iter()
                    .flat_map(|uri_templates| {
                        uri_templates.iter().map(|uri_template| UnifyDirectTemplateSurface {
                            server_id: server_id.clone(),
                            uri_template: uri_template.clone(),
                        })
                    })
            })
            .collect()
    }

    async fn resolve_profile_ids_for_capability_config(
        &self,
        identifier: &str,
        capability_config: &ClientCapabilityConfig,
    ) -> ConfigResult<Vec<String>> {
        let mut profile_ids = match capability_config.capability_source {
            CapabilitySource::Activated => crate::config::profile::basic::get_active_profile(&self.db_pool)
                .await
                .map_err(|err| ConfigError::DataAccessError(err.to_string()))?
                .into_iter()
                .filter_map(|profile| profile.id)
                .collect::<Vec<_>>(),
            CapabilitySource::Profiles => capability_config.selected_profile_ids.clone(),
            CapabilitySource::Custom => vec![capability_config.custom_profile_id.clone().ok_or_else(|| {
                ConfigError::DataAccessError(format!(
                    "Custom capability source requires custom_profile_id for {}",
                    identifier
                ))
            })?],
        };

        profile_ids.sort();
        profile_ids.dedup();
        Ok(profile_ids)
    }

    fn normalize_selected_server_ids_for_unify(
        &self,
        selected_server_ids: Vec<String>,
    ) -> Vec<String> {
        let mut normalized = selected_server_ids
            .into_iter()
            .map(|server_id| server_id.trim().to_string())
            .filter(|server_id| !server_id.is_empty())
            .collect::<Vec<_>>();
        normalized.sort();
        normalized.dedup();
        normalized
    }

    fn normalize_unify_direct_ids(
        &self,
        ids: Vec<String>,
    ) -> Vec<String> {
        let mut normalized = ids
            .into_iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect::<Vec<_>>();
        normalized.sort();
        normalized.dedup();
        normalized
    }

    fn normalize_unify_direct_capability_refs(
        &self,
        capability_refs: UnifyDirectCapabilityRefs,
    ) -> UnifyDirectCapabilityRefs {
        UnifyDirectCapabilityRefs {
            tool_refs: self.normalize_unify_direct_ids(capability_refs.tool_refs),
            prompt_refs: self.normalize_unify_direct_ids(capability_refs.prompt_refs),
            resource_refs: self.normalize_unify_direct_ids(capability_refs.resource_refs),
            template_refs: self.normalize_unify_direct_ids(capability_refs.template_refs),
        }
    }

    fn normalize_unify_direct_exposure_intent(
        &self,
        intent: UnifyDirectExposureIntent,
    ) -> UnifyDirectExposureIntent {
        match intent.route_mode {
            crate::clients::models::UnifyRouteMode::BrokerOnly => UnifyDirectExposureIntent::default(),
            crate::clients::models::UnifyRouteMode::ServerLevel => UnifyDirectExposureIntent {
                route_mode: intent.route_mode,
                server_ids: self.normalize_selected_server_ids_for_unify(intent.server_ids),
                capability_refs: UnifyDirectCapabilityRefs::default(),
            },
            crate::clients::models::UnifyRouteMode::CapabilityLevel => UnifyDirectExposureIntent {
                route_mode: intent.route_mode,
                server_ids: Vec::new(),
                capability_refs: self.normalize_unify_direct_capability_refs(intent.capability_refs),
            },
        }
    }

    fn normalize_selected_tool_surfaces(
        &self,
        selected_tool_surfaces: Vec<UnifyDirectToolSurface>,
    ) -> Vec<UnifyDirectToolSurface> {
        let mut normalized = selected_tool_surfaces
            .into_iter()
            .map(|surface| UnifyDirectToolSurface {
                server_id: surface.server_id.trim().to_string(),
                tool_name: surface.tool_name.trim().to_string(),
            })
            .filter(|surface| !surface.server_id.is_empty() && !surface.tool_name.is_empty())
            .collect::<Vec<_>>();
        normalized.sort_by(|left, right| {
            left.server_id
                .cmp(&right.server_id)
                .then(left.tool_name.cmp(&right.tool_name))
        });
        normalized.dedup();
        normalized
    }

    fn normalize_selected_prompt_surfaces(
        &self,
        selected_prompt_surfaces: Vec<UnifyDirectPromptSurface>,
    ) -> Vec<UnifyDirectPromptSurface> {
        let mut normalized = selected_prompt_surfaces
            .into_iter()
            .map(|surface| UnifyDirectPromptSurface {
                server_id: surface.server_id.trim().to_string(),
                prompt_name: surface.prompt_name.trim().to_string(),
            })
            .filter(|surface| !surface.server_id.is_empty() && !surface.prompt_name.is_empty())
            .collect::<Vec<_>>();
        normalized.sort_by(|left, right| {
            left.server_id
                .cmp(&right.server_id)
                .then(left.prompt_name.cmp(&right.prompt_name))
        });
        normalized.dedup();
        normalized
    }

    fn normalize_selected_resource_surfaces(
        &self,
        selected_resource_surfaces: Vec<UnifyDirectResourceSurface>,
    ) -> Vec<UnifyDirectResourceSurface> {
        let mut normalized = selected_resource_surfaces
            .into_iter()
            .map(|surface| UnifyDirectResourceSurface {
                server_id: surface.server_id.trim().to_string(),
                resource_uri: surface.resource_uri.trim().to_string(),
            })
            .filter(|surface| !surface.server_id.is_empty() && !surface.resource_uri.is_empty())
            .collect::<Vec<_>>();
        normalized.sort_by(|left, right| {
            left.server_id
                .cmp(&right.server_id)
                .then(left.resource_uri.cmp(&right.resource_uri))
        });
        normalized.dedup();
        normalized
    }

    fn normalize_selected_template_surfaces(
        &self,
        selected_template_surfaces: Vec<UnifyDirectTemplateSurface>,
    ) -> Vec<UnifyDirectTemplateSurface> {
        let mut normalized = selected_template_surfaces
            .into_iter()
            .map(|surface| UnifyDirectTemplateSurface {
                server_id: surface.server_id.trim().to_string(),
                uri_template: surface.uri_template.trim().to_string(),
            })
            .filter(|surface| !surface.server_id.is_empty() && !surface.uri_template.is_empty())
            .collect::<Vec<_>>();
        normalized.sort_by(|left, right| {
            left.server_id
                .cmp(&right.server_id)
                .then(left.uri_template.cmp(&right.uri_template))
        });
        normalized.dedup();
        normalized
    }

    async fn prepare_capability_client_insert(
        &self,
        identifier: &str,
        name: &str,
    ) -> ConfigResult<PreparedCapabilityClientInsert> {
        let first_contact_behavior = self.get_first_contact_behavior().await?;
        let approval_status = match first_contact_behavior {
            FirstContactBehavior::Deny => "suspended",
            FirstContactBehavior::Review => "pending",
            FirstContactBehavior::Allow => "approved",
        };
        let platform = crate::system::paths::PathService::get_current_platform();
        let template = self.template_source.get_template(identifier, platform).await?;
        let display_name = template
            .as_ref()
            .and_then(|entry| entry.display_name.clone())
            .unwrap_or_else(|| name.to_string());
        let config_path = template
            .as_ref()
            .and_then(Self::extract_runtime_config_path_from_template);
        let connection_mode = if config_path.is_some() {
            ClientConnectionMode::LocalConfigDetected.as_str()
        } else {
            ClientConnectionMode::Manual.as_str()
        };
        let attachment_state = if config_path.is_some() {
            AttachmentState::Detached.as_str()
        } else {
            AttachmentState::NotApplicable.as_str()
        };
        Ok(PreparedCapabilityClientInsert {
            id: crate::generate_id!("clnt"),
            display_name,
            config_path,
            approval_status,
            connection_mode,
            attachment_state,
            template_identifier: template.as_ref().map(|entry| entry.identifier.clone()),
            persisted_config: template
                .as_ref()
                .map(PersistedTemplateConfig::from_template)
                .unwrap_or_default(),
        })
    }

    async fn ensure_capability_client_in_transaction(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        identifier: &str,
        name: &str,
        prepared: &PreparedCapabilityClientInsert,
    ) -> ConfigResult<()> {
        sqlx::query(
            r#"
            INSERT OR IGNORE INTO client (
                id, name, display_name, identifier, config_path, backup_policy, backup_limit,
                approval_status, governance_kind, connection_mode, registration_origin, runtime_observed,
                template_identifier, config_format, protocol_revision, container_type, container_keys,
                storage_kind, storage_adapter, storage_path_strategy, merge_strategy, keep_original_config,
                managed_source, transports, config_file_parse, attachment_state
            )
            VALUES (?, ?, ?, ?, ?, 'keep_n', 5, ?, 'passive', ?, 'manual', 0, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&prepared.id)
        .bind(name)
        .bind(&prepared.display_name)
        .bind(identifier)
        .bind(prepared.config_path.as_deref())
        .bind(prepared.approval_status)
        .bind(prepared.connection_mode)
        .bind(prepared.template_identifier.as_deref())
        .bind(prepared.persisted_config.config_format.as_deref())
        .bind(prepared.persisted_config.protocol_revision.as_deref())
        .bind(prepared.persisted_config.container_type.as_deref())
        .bind(prepared.persisted_config.container_keys.as_deref())
        .bind(prepared.persisted_config.storage_kind.as_deref())
        .bind(prepared.persisted_config.storage_adapter.as_deref())
        .bind(prepared.persisted_config.storage_path_strategy.as_deref())
        .bind(prepared.persisted_config.merge_strategy.as_deref())
        .bind(prepared.persisted_config.keep_original_config)
        .bind(prepared.persisted_config.managed_source.as_deref())
        .bind(prepared.persisted_config.transports.as_deref())
        .bind(prepared.persisted_config.config_file_parse.as_deref())
        .bind(prepared.attachment_state)
        .execute(&mut **transaction)
        .await
        .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        sqlx::query("UPDATE client SET name = ?, updated_at = CURRENT_TIMESTAMP WHERE identifier = ?")
            .bind(name)
            .bind(identifier)
            .execute(&mut **transaction)
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        Ok(())
    }

    async fn prepare_custom_profile(
        &self,
        identifier: &str,
    ) -> ConfigResult<PreparedCustomProfile> {
        let name = format!("{}_custom", identifier);
        let id = match crate::config::profile::get_profile_by_name(&self.db_pool, &name)
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?
        {
            Some(profile) => {
                if profile.profile_type != ProfileType::HostApp {
                    return Err(ConfigError::DataAccessError(format!(
                        "Profile '{}' already exists but is not host_app",
                        name
                    )));
                }
                profile
                    .id
                    .ok_or_else(|| ConfigError::DataAccessError(format!("Profile '{}' is missing an id", name)))?
            }
            None => crate::generate_id!("prof"),
        };
        Ok(PreparedCustomProfile { id, name })
    }

    async fn ensure_custom_profile_in_transaction(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        identifier: &str,
        prepared: &PreparedCustomProfile,
    ) -> ConfigResult<String> {
        if let Some((id, profile_type)) =
            sqlx::query_as::<_, (String, String)>("SELECT id, type FROM profile WHERE name = ?")
                .bind(&prepared.name)
                .fetch_optional(&mut **transaction)
                .await
                .map_err(|error| ConfigError::DataAccessError(error.to_string()))?
        {
            if profile_type != ProfileType::HostApp.as_str() {
                return Err(ConfigError::DataAccessError(format!(
                    "Profile '{}' already exists but is not host_app",
                    prepared.name
                )));
            }
            return Ok(id);
        }
        sqlx::query(
            r#"
            INSERT INTO profile (
                id, name, description, type, role,
                priority, is_active, is_default
            ) VALUES (?, ?, ?, 'host_app', 'user', 0, 0, 0)
            "#,
        )
        .bind(&prepared.id)
        .bind(&prepared.name)
        .bind(format!("Custom profile for {}", identifier))
        .execute(&mut **transaction)
        .await
        .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        Ok(prepared.id.clone())
    }

    async fn ensure_custom_profile(
        &self,
        identifier: &str,
    ) -> ConfigResult<String> {
        let prepared = self.prepare_custom_profile(identifier).await?;
        let mut transaction = self
            .db_pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        let profile_id = self
            .ensure_custom_profile_in_transaction(&mut transaction, identifier, &prepared)
            .await?;
        transaction
            .commit()
            .await
            .map_err(|error| ConfigError::DataAccessError(error.to_string()))?;
        Ok(profile_id)
    }

    async fn validate_selected_profile_ids(
        &self,
        selected_profile_ids: &[String],
    ) -> ConfigResult<()> {
        for profile_id in selected_profile_ids {
            let profile = crate::config::profile::get_profile(&self.db_pool, profile_id)
                .await
                .map_err(|err| ConfigError::DataAccessError(err.to_string()))?
                .ok_or_else(|| {
                    ConfigError::DataAccessError(format!("Selected profile '{}' does not exist", profile_id))
                })?;

            if profile.profile_type != ProfileType::Shared {
                return Err(ConfigError::DataAccessError(format!(
                    "Selected profile '{}' must be a shared profile",
                    profile_id
                )));
            }
            if profile.profile_mode == crate::config::models::ProfileMode::Workflow {
                return Err(ConfigError::DataAccessError(format!(
                    "Selected profile '{}' is a Workflow and cannot join a Capability working set",
                    profile_id
                )));
            }
        }

        Ok(())
    }

    fn normalize_selected_profile_ids(
        &self,
        capability_source: CapabilitySource,
        selected_profile_ids: Vec<String>,
    ) -> ConfigResult<Vec<String>> {
        match capability_source {
            CapabilitySource::Activated => Ok(Vec::new()),
            CapabilitySource::Profiles => {
                let mut normalized = selected_profile_ids
                    .into_iter()
                    .map(|id| id.trim().to_string())
                    .filter(|id| !id.is_empty())
                    .collect::<Vec<_>>();
                normalized.sort();
                normalized.dedup();

                if normalized.is_empty() {
                    return Err(ConfigError::DataAccessError(
                        "profiles capability source requires at least one selected profile".to_string(),
                    ));
                }

                Ok(normalized)
            }
            CapabilitySource::Custom => Ok(Vec::new()),
        }
    }
}
