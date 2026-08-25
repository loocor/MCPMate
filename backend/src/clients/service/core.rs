use crate::clients::TemplateEngine;
use crate::clients::detector::{ClientDetector, DetectedClient};
use crate::clients::discovery::{admin_discovery_base_url, fetch_admin_discovery_client_templates_strict};
use crate::clients::document::{map_config_file_error, parse_config, persist_config_document};
use crate::clients::engine::TemplateExecutionResult;
use crate::clients::error::{ConfigError, ConfigResult};
use crate::clients::models::{
    AttachmentState, BackupPolicySetting, CONFIG_TRANSPORT_PRIORITY, ClientCapabilityConfig, ClientConfigFileParse,
    ClientConfigFileState, ClientConnectionMode, ClientGovernanceKind, ClientRegistrationOrigin,
    ClientRenderDefinition, ClientTemplate, ConfigMapping, ConfigMode, FormatRule, ManagedEndpointConfig,
    MergeStrategy, ServerTemplateInput, StorageConfig, StorageKind, TemplateFormat,
};
use crate::clients::mutate::remove_managed_entries;
#[cfg(test)]
use crate::clients::source::FileTemplateSource;
use crate::clients::source::{ClientConfigSource, DbTemplateSource};
use crate::system::paths::{PathService, get_path_service};
use crate::system::settings::{
    DEFAULT_CLIENT_DISCOVERY_SNAPSHOT_TTL_SECONDS, get_settings, record_client_discovery_snapshot_failure,
    set_client_discovery_snapshot_last_success_at,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs::OpenOptions;
use tokio::sync::{Mutex, watch};

#[derive(Debug, Clone, sqlx::FromRow, Default)]
pub struct ClientStateRow {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) id: String,
    pub(super) identifier: String,
    pub(super) name: String,
    pub(super) display_name: Option<String>,
    pub(super) config_path: Option<String>,
    pub(super) config_mode: Option<String>,
    pub(super) transport: Option<String>,
    pub(super) client_version: Option<String>,
    pub(super) backup_policy: Option<String>,
    pub(super) backup_limit: Option<i64>,
    pub(super) capability_source: Option<String>,
    pub(super) governance_kind: Option<String>,
    pub(super) connection_mode: Option<String>,
    pub(super) registration_origin: Option<String>,
    pub(super) runtime_observed: Option<i64>,
    pub(super) template_identifier: Option<String>,
    pub(super) selected_profile_ids: Option<String>,
    pub(super) custom_profile_id: Option<String>,
    pub(super) approval_status: Option<String>,
    pub(super) attachment_state: Option<String>,
    #[allow(dead_code)]
    pub(super) template_id: Option<String>,
    #[allow(dead_code)]
    pub(super) template_version: Option<String>,
    #[allow(dead_code)]
    pub(super) approval_metadata: Option<String>,
    // Template configuration fields (persisted from template at initialization)
    pub(super) config_format: Option<String>,
    pub(super) protocol_revision: Option<String>,
    pub(super) container_type: Option<String>,
    pub(super) container_keys: Option<String>,
    pub(super) storage_kind: Option<String>,
    pub(super) storage_adapter: Option<String>,
    pub(super) storage_path_strategy: Option<String>,
    pub(super) merge_strategy: Option<String>,
    pub(super) merge_strategy_override: Option<String>,
    pub(super) keep_original_config: Option<i64>,
    pub(super) managed_source: Option<String>,
    pub(super) transports: Option<String>,
    pub(super) config_file_parse: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeClientMetadata {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage_url: Option<String>,
    #[serde(default)]
    pub docs_url: Option<String>,
    #[serde(default)]
    pub support_url: Option<String>,
    #[serde(default)]
    pub logo_url: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
}

impl RuntimeClientMetadata {
    pub fn from_template(template: &ClientTemplate) -> Self {
        Self {
            description: template
                .metadata
                .get("description")
                .and_then(|value| value.as_str())
                .map(ToString::to_string),
            homepage_url: template
                .metadata
                .get("homepage_url")
                .and_then(|value| value.as_str())
                .map(ToString::to_string),
            docs_url: template
                .metadata
                .get("docs_url")
                .and_then(|value| value.as_str())
                .map(ToString::to_string),
            support_url: template
                .metadata
                .get("support_url")
                .and_then(|value| value.as_str())
                .map(ToString::to_string),
            logo_url: template
                .metadata
                .get("logo_url")
                .and_then(|value| value.as_str())
                .map(ToString::to_string),
            category: template
                .metadata
                .get("category")
                .and_then(|value| value.as_str())
                .map(ToString::to_string),
        }
    }

    pub fn resolve_for_template(
        stored: &Self,
        template: Option<&ClientTemplate>,
    ) -> Self {
        let Some(template) = template else {
            return stored.clone();
        };
        Self::resolve_with_template_metadata(stored, Some(Self::from_template(template)))
    }

    pub fn resolve_with_template_metadata(
        stored: &Self,
        template: Option<Self>,
    ) -> Self {
        let Some(template) = template else {
            return stored.clone();
        };
        Self {
            // Keep user-saved metadata authoritative; template metadata is fallback only.
            description: stored.description.clone().or(template.description),
            homepage_url: stored.homepage_url.clone().or(template.homepage_url),
            docs_url: stored.docs_url.clone().or(template.docs_url),
            support_url: stored.support_url.clone().or(template.support_url),
            logo_url: stored.logo_url.clone().or(template.logo_url),
            category: stored.category.clone().or(template.category),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct PersistedTemplateConfig {
    pub(super) config_format: Option<String>,
    pub(super) protocol_revision: Option<String>,
    pub(super) container_type: Option<String>,
    pub(super) container_keys: Option<String>,
    pub(super) storage_kind: Option<String>,
    pub(super) storage_adapter: Option<String>,
    pub(super) storage_path_strategy: Option<String>,
    pub(super) merge_strategy: Option<String>,
    pub(super) keep_original_config: Option<i64>,
    pub(super) managed_source: Option<String>,
    pub(super) transports: Option<String>,
    pub(super) config_file_parse: Option<String>,
}

impl PersistedTemplateConfig {
    pub(super) fn from_template(template: &ClientTemplate) -> Self {
        Self {
            config_format: Some(template.format.as_str().to_string()),
            protocol_revision: template.protocol_revision.clone(),
            container_type: Some(
                match template.config_mapping.container_type {
                    crate::clients::models::ContainerType::ObjectMap => "object",
                    crate::clients::models::ContainerType::Array => "array",
                }
                .to_string(),
            ),
            container_keys: serde_json::to_string(&template.config_mapping.container_keys).ok(),
            storage_kind: Some(
                match template.storage.kind {
                    crate::clients::models::StorageKind::File => "file",
                    crate::clients::models::StorageKind::Kv => "kv",
                    crate::clients::models::StorageKind::Custom => "custom",
                }
                .to_string(),
            ),
            storage_adapter: template.storage.adapter.clone(),
            storage_path_strategy: template.storage.path_strategy.clone(),
            merge_strategy: Some(
                match template.config_mapping.merge_strategy {
                    crate::clients::models::MergeStrategy::Replace => "replace",
                    crate::clients::models::MergeStrategy::DeepMerge => "deep_merge",
                }
                .to_string(),
            ),
            keep_original_config: Some(template.config_mapping.keep_original_config as i64),
            managed_source: template.config_mapping.managed_source.clone(),
            transports: if template.config_mapping.format_rules.is_empty() {
                None
            } else {
                serde_json::to_string(&template.config_mapping.format_rules).ok()
            },
            config_file_parse: None,
        }
    }
}

impl ClientStateRow {
    #[cfg(test)]
    pub(crate) fn test_attachment_fixture(
        connection_mode: &str,
        config_path: Option<&str>,
        attachment_state: Option<&str>,
    ) -> Self {
        Self {
            identifier: "test.client".to_string(),
            config_path: config_path.map(str::to_string),
            connection_mode: Some(connection_mode.to_string()),
            attachment_state: attachment_state.map(str::to_string),
            ..Self::default()
        }
    }

    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    pub(super) fn to_setting(&self) -> BackupPolicySetting {
        BackupPolicySetting::from_pair(
            self.backup_policy.as_deref(),
            self.backup_limit.map(|value| value.max(0) as u32),
        )
    }

    #[allow(dead_code)]
    pub fn is_approved(&self) -> bool {
        matches!(self.approval_status.as_deref(), Some("approved") | None)
    }

    pub fn approval_status(&self) -> &str {
        self.approval_status.as_deref().unwrap_or("approved")
    }

    pub fn attachment_state(&self) -> AttachmentState {
        if !self.has_local_config_target() {
            return AttachmentState::NotApplicable;
        }

        self.attachment_state
            .as_deref()
            .and_then(|value| value.parse::<AttachmentState>().ok())
            .unwrap_or_default()
    }

    pub fn connection_mode(&self) -> ClientConnectionMode {
        self.connection_mode
            .as_deref()
            .and_then(|value| value.parse::<ClientConnectionMode>().ok())
            .unwrap_or_default()
    }

    pub fn governance_kind(&self) -> ClientGovernanceKind {
        self.governance_kind
            .as_deref()
            .and_then(|value| value.parse::<ClientGovernanceKind>().ok())
            .unwrap_or_default()
    }

    pub fn registration_origin(&self) -> ClientRegistrationOrigin {
        self.registration_origin
            .as_deref()
            .and_then(|value| value.parse::<ClientRegistrationOrigin>().ok())
            .unwrap_or_else(|| {
                if self.runtime_observed() {
                    ClientRegistrationOrigin::RuntimeInitialize
                } else if self.has_config_file_target() {
                    ClientRegistrationOrigin::ConfigDetection
                } else {
                    ClientRegistrationOrigin::Manual
                }
            })
    }

    pub fn display_name(&self) -> &str {
        self.display_name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&self.name)
    }

    pub fn config_path(&self) -> Option<&str> {
        self.config_path.as_deref().filter(|value| !value.trim().is_empty())
    }

    pub fn governed_by_default_policy(&self) -> bool {
        self.governance_kind() == ClientGovernanceKind::Passive
    }

    pub fn has_local_config_target(&self) -> bool {
        self.connection_mode() == ClientConnectionMode::LocalConfigDetected && self.config_path().is_some()
    }

    pub fn has_config_file_target(&self) -> bool {
        self.config_path().is_some()
    }

    pub fn config_file_state(&self) -> ClientConfigFileState {
        if self.has_config_file_target() {
            ClientConfigFileState::WithConfigFile
        } else {
            ClientConfigFileState::WithoutConfigFile
        }
    }

    pub fn runtime_observed(&self) -> bool {
        self.runtime_observed.unwrap_or_default() != 0
    }

    #[allow(dead_code)]
    pub fn is_pending_approval(&self) -> bool {
        self.approval_status.as_deref() == Some("pending")
    }

    #[allow(dead_code)]
    pub fn template_id(&self) -> Option<&str> {
        self.template_id.as_deref()
    }

    /// Returns the template identifier (init-time seed; NOT for runtime inference).
    pub fn template_identifier(&self) -> Option<&str> {
        self.template_identifier
            .as_deref()
            .filter(|value| !value.trim().is_empty())
    }

    pub(super) fn capability_config(&self) -> ConfigResult<ClientCapabilityConfig> {
        ClientCapabilityConfig::from_parts(
            self.capability_source.as_deref(),
            self.selected_profile_ids.as_deref(),
            self.custom_profile_id.clone(),
        )
        .map_err(ConfigError::DataAccessError)
    }

    pub fn runtime_client_metadata(&self) -> RuntimeClientMetadata {
        let Some(raw) = self.approval_metadata.as_deref() else {
            return RuntimeClientMetadata::default();
        };

        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            return RuntimeClientMetadata::default();
        };

        value
            .get("runtime_client")
            .cloned()
            .and_then(|entry| serde_json::from_value::<RuntimeClientMetadata>(entry).ok())
            .unwrap_or_default()
    }

    // Template configuration accessors (persisted from template at initialization)

    pub fn config_format(&self) -> Option<&str> {
        self.config_format.as_deref().filter(|v| !v.trim().is_empty())
    }

    pub fn protocol_revision(&self) -> Option<&str> {
        self.protocol_revision.as_deref().filter(|v| !v.trim().is_empty())
    }

    pub fn container_type(&self) -> Option<&str> {
        self.container_type.as_deref().filter(|v| !v.trim().is_empty())
    }

    pub fn container_keys(&self) -> ConfigResult<Vec<String>> {
        let Some(raw) = self.container_keys.as_deref() else {
            return Ok(Vec::new());
        };

        if raw.trim().is_empty() {
            return Ok(Vec::new());
        }

        serde_json::from_str::<Vec<String>>(raw)
            .map_err(|e| ConfigError::DataAccessError(format!("Failed to parse container_keys: {}", e)))
    }

    pub fn storage_kind(&self) -> Option<&str> {
        self.storage_kind.as_deref().filter(|v| !v.trim().is_empty())
    }

    pub fn storage_adapter(&self) -> Option<&str> {
        self.storage_adapter.as_deref().filter(|v| !v.trim().is_empty())
    }

    pub fn storage_path_strategy(&self) -> Option<&str> {
        self.storage_path_strategy.as_deref().filter(|v| !v.trim().is_empty())
    }

    pub fn merge_strategy(&self) -> Option<&str> {
        self.merge_strategy.as_deref().filter(|v| !v.trim().is_empty())
    }

    pub fn merge_strategy_override(&self) -> Option<&str> {
        self.merge_strategy_override
            .as_deref()
            .filter(|value| !value.trim().is_empty())
    }

    fn parse_persisted_merge_strategy(
        &self,
        value: Option<&str>,
        field: &str,
    ) -> ConfigResult<MergeStrategy> {
        let value = value.ok_or_else(|| {
            ConfigError::DataAccessError(format!(
                "Client '{}' is missing persisted {field}; cannot resolve writeback behavior",
                self.identifier()
            ))
        })?;

        match value {
            "replace" => Ok(MergeStrategy::Replace),
            "deep_merge" => Ok(MergeStrategy::DeepMerge),
            value => Err(ConfigError::DataAccessError(format!(
                "Client '{}' has unsupported persisted {field} '{value}'",
                self.identifier()
            ))),
        }
    }

    pub fn template_merge_strategy(&self) -> ConfigResult<MergeStrategy> {
        match self.merge_strategy() {
            Some(value) => self.parse_persisted_merge_strategy(Some(value), "merge_strategy"),
            None => Ok(MergeStrategy::default()),
        }
    }

    pub fn merge_strategy_override_value(&self) -> ConfigResult<Option<MergeStrategy>> {
        self.merge_strategy_override()
            .map(|value| self.parse_persisted_merge_strategy(Some(value), "merge_strategy_override"))
            .transpose()
    }

    pub fn effective_merge_strategy_value(
        &self,
        system_override: Option<MergeStrategy>,
    ) -> ConfigResult<MergeStrategy> {
        match self.merge_strategy_override_value()? {
            Some(strategy) => Ok(strategy),
            None => system_override
                .map(Ok)
                .unwrap_or_else(|| self.template_merge_strategy()),
        }
    }

    pub fn keep_original_config(&self) -> bool {
        self.keep_original_config.map(|v| v != 0).unwrap_or(false)
    }

    pub fn managed_source(&self) -> Option<&str> {
        self.managed_source.as_deref().filter(|v| !v.trim().is_empty())
    }

    pub fn transports(&self) -> ConfigResult<Option<serde_json::Value>> {
        let Some(raw) = self.transports.as_deref() else {
            return Ok(None);
        };

        if raw.trim().is_empty() {
            return Ok(None);
        }

        serde_json::from_str::<serde_json::Value>(raw)
            .map(Some)
            .map_err(|e| ConfigError::DataAccessError(format!("Failed to parse transports: {}", e)))
    }

    pub fn parsed_transports(&self) -> ConfigResult<std::collections::HashMap<String, FormatRule>> {
        let Some(value) = self.transports()? else {
            return Ok(std::collections::HashMap::new());
        };

        serde_json::from_value::<std::collections::HashMap<String, FormatRule>>(value)
            .map_err(|e| ConfigError::DataAccessError(format!("Failed to decode transports: {}", e)))
    }

    pub fn config_file_parse_override(&self) -> ConfigResult<Option<ClientConfigFileParse>> {
        let Some(raw) = self.config_file_parse.as_deref() else {
            return Ok(None);
        };

        if raw.trim().is_empty() {
            return Ok(None);
        }

        serde_json::from_str::<ClientConfigFileParse>(raw)
            .map(Some)
            .map_err(|e| ConfigError::DataAccessError(format!("Failed to parse config_file_parse: {}", e)))
    }

    pub fn effective_config_file_parse(&self) -> ConfigResult<Option<ClientConfigFileParse>> {
        if let Some(override_parse) = self.config_file_parse_override()? {
            return Ok(Some(override_parse));
        }

        self.legacy_config_file_parse()
    }

    pub fn effective_config_file_parse_with(
        &self,
        next_override: Option<&ClientConfigFileParse>,
        clear_override: bool,
    ) -> ConfigResult<Option<ClientConfigFileParse>> {
        match (next_override, clear_override) {
            (Some(parse), _) => Ok(Some(parse.clone())),
            (None, true) => self.legacy_config_file_parse(),
            (None, false) => self.effective_config_file_parse(),
        }
    }

    pub fn legacy_config_file_parse(&self) -> ConfigResult<Option<ClientConfigFileParse>> {
        let format = match self.config_format() {
            Some("json") => TemplateFormat::Json,
            Some("json5") => TemplateFormat::Json5,
            Some("toml") => TemplateFormat::Toml,
            Some("yaml") => TemplateFormat::Yaml,
            Some(_) | None => return Ok(None),
        };

        let container_keys = self.container_keys()?;
        if container_keys.is_empty() {
            return Ok(None);
        }

        let container_type = match self.container_type() {
            Some("array") => crate::clients::models::ContainerType::Array,
            _ => crate::clients::models::ContainerType::ObjectMap,
        };

        Ok(Some(ClientConfigFileParse {
            format,
            container_type,
            container_keys,
        }))
    }
}

fn effective_transports_for_state(state: &ClientStateRow) -> ConfigResult<HashMap<String, FormatRule>> {
    Ok(state
        .parsed_transports()?
        .into_iter()
        .map(|(transport, rule)| (transport, rule.normalized()))
        .collect())
}

pub(crate) fn supported_transports_from_transports(transports: &HashMap<String, FormatRule>) -> Vec<String> {
    CONFIG_TRANSPORT_PRIORITY
        .into_iter()
        .filter(|transport| transports.contains_key(*transport))
        .map(str::to_string)
        .collect()
}

/// Summarized view of a client template combined with detection and filesystem state
#[derive(Debug, Clone)]
pub struct ClientDescriptor {
    pub template: Option<ClientTemplate>,
    pub state: ClientStateRow,
    pub detection: Option<DetectedClient>,
    pub config_path: Option<String>,
    pub config_exists: bool,
    pub detected_at: Option<DateTime<Utc>>,
    pub persisted: bool,
}

#[derive(Debug, Clone)]
pub struct ClientBackupRecord {
    pub identifier: String,
    pub backup: String,
    pub path: String,
    pub size: u64,
    pub created_at: Option<DateTime<Utc>>,
}

/// Parameters for rendering/applying a client configuration
#[derive(Debug, Clone)]
pub struct ClientRenderOptions {
    pub client_id: String,
    pub mode: ConfigMode,
    pub profile_id: Option<String>,
    pub server_ids: Option<Vec<String>>,
    pub dry_run: bool,
}

/// Result of a configuration execution
#[derive(Debug)]
pub struct ClientRenderResult {
    pub execution: TemplateExecutionResult,
    pub target_path: Option<String>,
    pub servers: Vec<ServerTemplateInput>,
    pub warnings: Vec<String>,
    pub chosen_transport: Option<String>,
    pub auto_selected: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PreviewOutcome {
    pub format: TemplateFormat,
    pub before: Option<String>,
    pub after: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ApplyOutcome {
    pub preview: PreviewOutcome,
    pub applied: bool,
    pub backup_path: Option<String>,
    pub scheduled: bool,
    pub scheduled_reason: Option<String>,
    pub warnings: Vec<String>,
}

/// High-level service wiring templates, detection, and storage backends
pub struct ClientConfigService {
    pub(super) template_source: Arc<dyn ClientConfigSource>,
    pub(super) template_engine: Arc<TemplateEngine>,
    pub(super) detector: Arc<ClientDetector>,
    pub(super) db_pool: Arc<SqlitePool>,
    pub(super) admin_discovery_refresh_enabled: bool,
    runtime_template_snapshot_gate: Arc<Mutex<RuntimeTemplateSnapshotRefreshState>>,
}

#[derive(Default)]
struct RuntimeTemplateSnapshotRefreshState {
    completion: Option<watch::Receiver<bool>>,
}

struct RuntimeTemplateSnapshotRefreshAttempt {
    completion: watch::Sender<bool>,
}

impl Drop for RuntimeTemplateSnapshotRefreshAttempt {
    fn drop(&mut self) {
        self.completion.send_replace(true);
    }
}

impl ClientConfigService {
    /// Bootstrap service with default template root resolution
    pub async fn bootstrap(db_pool: Arc<SqlitePool>) -> crate::clients::error::ConfigResult<Self> {
        let runtime_source: Arc<dyn ClientConfigSource> = Arc::new(DbTemplateSource::new(db_pool.clone())?);
        let service = Self::with_source_and_admin_discovery_refresh(db_pool, runtime_source, true).await?;
        if let Err(err) = service.ensure_runtime_template_snapshot().await {
            tracing::warn!(
                error = %err,
                "Admin discovery refresh failed during client config bootstrap; retaining runtime templates"
            );
        }
        Ok(service)
    }

    /// Initialize service with pre-built template source (primarily for tests)
    pub async fn with_source(
        db_pool: Arc<SqlitePool>,
        template_source: Arc<dyn ClientConfigSource>,
    ) -> crate::clients::error::ConfigResult<Self> {
        Self::with_source_and_admin_discovery_refresh(db_pool, template_source, false).await
    }

    async fn with_source_and_admin_discovery_refresh(
        db_pool: Arc<SqlitePool>,
        template_source: Arc<dyn ClientConfigSource>,
        admin_discovery_refresh_enabled: bool,
    ) -> crate::clients::error::ConfigResult<Self> {
        let engine = TemplateEngine::with_defaults(template_source.clone());
        let detector = ClientDetector::new(template_source.clone())?;

        Ok(Self {
            template_source,
            template_engine: Arc::new(engine),
            detector: Arc::new(detector),
            db_pool,
            admin_discovery_refresh_enabled,
            runtime_template_snapshot_gate: Arc::new(Mutex::new(RuntimeTemplateSnapshotRefreshState::default())),
        })
    }

    /// Reload templates from disk, keeping previous index if reloading fails
    pub async fn reload_templates(&self) -> crate::clients::error::ConfigResult<()> {
        Ok(())
    }

    pub async fn ensure_runtime_template_snapshot(&self) -> crate::clients::error::ConfigResult<()> {
        if !self.admin_discovery_refresh_enabled {
            return Ok(());
        }

        let mut refresh_state = self.runtime_template_snapshot_gate.lock().await;
        if let Some(completion) = refresh_state.completion.as_ref() {
            if *completion.borrow() {
                refresh_state.completion = None;
            } else {
                let mut completion = completion.clone();
                drop(refresh_state);
                let _ = completion.changed().await;
                return Ok(());
            }
        }
        if !Self::runtime_template_snapshot_needs_refresh(self.db_pool.as_ref()).await? {
            return Ok(());
        }

        let (completion, receiver) = watch::channel(false);
        refresh_state.completion = Some(receiver);
        drop(refresh_state);
        let _attempt = RuntimeTemplateSnapshotRefreshAttempt { completion };

        let base_url = admin_discovery_base_url();
        match Self::refresh_runtime_templates_from_admin_discovery(self.db_pool.as_ref(), &base_url).await {
            Ok(()) => Ok(()),
            Err(err) => {
                if let Err(record_err) = record_client_discovery_snapshot_failure(self.db_pool.as_ref()).await {
                    tracing::warn!(
                        error = %record_err,
                        "Failed to persist Admin discovery refresh failure state"
                    );
                }
                Err(err)
            }
        }
    }

    async fn runtime_template_snapshot_needs_refresh(
        db_pool: &SqlitePool
    ) -> crate::clients::error::ConfigResult<bool> {
        let settings = get_settings(db_pool).await?;
        let runtime_template_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_template_runtime")
            .fetch_one(db_pool)
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        if runtime_template_count == 0 {
            return Ok(true);
        }
        let refresh_needed = match settings.client_discovery_snapshot_last_success_at.as_deref() {
            Some(last_success_at) if settings.client_discovery_snapshot_ttl_seconds > 0 => {
                let last_success_at = DateTime::parse_from_rfc3339(last_success_at).map_err(|err| {
                    ConfigError::DataAccessError(format!("Invalid client discovery snapshot success time: {err}"))
                })?;
                Utc::now().signed_duration_since(last_success_at.with_timezone(&Utc))
                    >= chrono::Duration::seconds(settings.client_discovery_snapshot_ttl_seconds)
            }
            _ => true,
        };
        if !refresh_needed || settings.client_discovery_snapshot_consecutive_failures < 3 {
            return Ok(refresh_needed);
        }

        let Some(failure_window_started_at) = settings.client_discovery_snapshot_failure_window_started_at else {
            return Ok(true);
        };
        let failure_window_started_at = DateTime::parse_from_rfc3339(&failure_window_started_at).map_err(|err| {
            ConfigError::DataAccessError(format!("Invalid client discovery snapshot failure window time: {err}"))
        })?;
        Ok(
            Utc::now().signed_duration_since(failure_window_started_at.with_timezone(&Utc))
                >= chrono::Duration::seconds(DEFAULT_CLIENT_DISCOVERY_SNAPSHOT_TTL_SECONDS),
        )
    }

    /// Get template for client identifier on current platform
    pub async fn get_client_template(
        &self,
        client_id: &str,
    ) -> crate::clients::error::ConfigResult<ClientTemplate> {
        self.template_source
            .get_template(client_id, PathService::get_current_platform())
            .await?
            .ok_or_else(|| {
                crate::clients::error::ConfigError::TemplateIndexError(format!(
                    "Client template not found for {}",
                    client_id
                ))
            })
    }

    pub async fn default_merge_strategy_override(&self) -> ConfigResult<Option<MergeStrategy>> {
        Ok(
            crate::config::client::runtime_settings::get_client_runtime_defaults(self.db_pool.as_ref())
                .await?
                .default_merge_strategy_override,
        )
    }

    pub fn build_render_definition_from_state(
        state: &ClientStateRow,
        system_override: Option<MergeStrategy>,
    ) -> ConfigResult<ClientRenderDefinition> {
        let parse = state.effective_config_file_parse()?.ok_or_else(|| {
            ConfigError::DataAccessError(format!(
                "Client '{}' is missing persisted config_file_parse; cannot render configuration",
                state.identifier()
            ))
        })?;

        if parse.container_keys.is_empty() {
            return Err(ConfigError::DataAccessError(format!(
                "Client '{}' is missing config_file_parse.container_keys; cannot render configuration",
                state.identifier()
            )));
        }

        let transports = effective_transports_for_state(state)?;
        let supported_transports = supported_transports_from_transports(&transports);
        if supported_transports.is_empty() {
            return Err(ConfigError::DataAccessError(format!(
                "Client '{}' is missing persisted transports; cannot render configuration",
                state.identifier()
            )));
        }

        for transport in supported_transports {
            if let Some(rule) = transports.get(&transport) {
                rule.validate_for_transport(&transport)
                    .map_err(ConfigError::DataAccessError)?;
            } else {
                return Err(ConfigError::DataAccessError(format!(
                    "Client '{}' is missing persisted format rule for supported transport '{}'",
                    state.identifier(),
                    transport
                )));
            }
        }

        let config_mapping = ConfigMapping {
            container_keys: parse.container_keys.clone(),
            container_type: parse.container_type,
            merge_strategy: state.effective_merge_strategy_value(system_override)?,
            keep_original_config: state.keep_original_config(),
            managed_endpoint: Some(ManagedEndpointConfig {
                source: state.managed_source().map(str::to_string),
            }),
            managed_source: state.managed_source().map(str::to_string),
            parse: Some(parse.clone()),
            format_rules: transports,
        };

        let storage = StorageConfig {
            kind: match state.storage_kind() {
                Some("kv") => StorageKind::Kv,
                Some("custom") => StorageKind::Custom,
                _ => StorageKind::File,
            },
            path_strategy: state.storage_path_strategy().map(str::to_string),
            adapter: state.storage_adapter().map(str::to_string),
        };

        Ok(ClientRenderDefinition {
            identifier: state.identifier().to_string(),
            format: parse.format,
            storage,
            config_mapping,
        })
    }

    /// Read current configuration file content for a client
    pub async fn read_current_config(
        &self,
        client_id: &str,
    ) -> crate::clients::error::ConfigResult<Option<String>> {
        let state = self
            .fetch_state(client_id)
            .await?
            .ok_or_else(|| ConfigError::DataAccessError(format!("Client {} not found", client_id)))?;
        let config_path = state
            .config_path()
            .ok_or_else(|| ConfigError::PathResolutionError(format!("No config_path for client {}", client_id)))?;
        let storage = self.template_engine.storage_for_client(&state)?;
        storage.read(config_path).await
    }

    /// Detach MCPMate from a client's external configuration while preserving MCPMate-side settings.
    pub async fn detach_client(
        &self,
        client_id: &str,
    ) -> ConfigResult<bool> {
        let state = self
            .fetch_state(client_id)
            .await?
            .ok_or_else(|| ConfigError::DataAccessError(format!("Client {} not found", client_id)))?;
        let config_path = state
            .config_path()
            .ok_or_else(|| ConfigError::PathResolutionError(format!("No config_path for client {}", client_id)))?;
        if !state.has_local_config_target() {
            return Err(ConfigError::DataAccessError(format!(
                "Client {} does not have an attachable local config target",
                client_id
            )));
        }
        let raw_content = self.read_current_config(client_id).await?.ok_or_else(|| {
            ConfigError::FileOperationError(format!("Config file not found for client {}", client_id))
        })?;

        let parse_rule = state.effective_config_file_parse()?.ok_or_else(|| {
            ConfigError::DataAccessError(format!(
                "Client '{}' is missing persisted config_file_parse; cannot detach configuration",
                client_id
            ))
        })?;
        let document = parse_config(&raw_content, &parse_rule)
            .map_err(|err| map_config_file_error("Failed to parse config for detach", err))?;
        let (updated, changed) = remove_managed_entries(document, &parse_rule);

        if changed {
            let storage = self.template_engine.storage_for_client(&state)?;
            persist_config_document(
                &storage,
                client_id,
                config_path,
                &updated,
                &BackupPolicySetting::default(),
            )
            .await
            .map_err(|err| map_config_file_error("Failed to persist detached config", err))?;
        }

        self.mark_client_detached(client_id).await?;
        Ok(changed)
    }

    /// Get resolved configuration path for a client on current platform
    pub async fn config_path(
        &self,
        client_id: &str,
    ) -> crate::clients::error::ConfigResult<Option<String>> {
        self.resolved_config_path(client_id).await
    }

    pub(super) fn resolved_config_path_from_state(state: &ClientStateRow) -> ConfigResult<Option<String>> {
        let Some(config_path) = state.config_path() else {
            return Ok(None);
        };

        let resolved = get_path_service()
            .resolve_user_path(config_path)
            .map_err(|err| ConfigError::PathResolutionError(err.to_string()))?;
        Ok(Some(resolved.to_string_lossy().to_string()))
    }

    pub(super) async fn resolved_config_path(
        &self,
        client_id: &str,
    ) -> crate::clients::error::ConfigResult<Option<String>> {
        let Some(state) = self.fetch_state(client_id).await? else {
            return Ok(None);
        };

        Self::resolved_config_path_from_state(&state)
    }

    pub(super) async fn verified_local_config_target(
        &self,
        client_id: &str,
    ) -> ConfigResult<Option<String>> {
        let Some(config_path) = self.resolved_config_path(client_id).await? else {
            return Ok(None);
        };

        let resolved_path = std::path::PathBuf::from(&config_path);
        let metadata = tokio::fs::metadata(&resolved_path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                ConfigError::DataAccessError(format!("Client config target does not exist: {}", config_path))
            } else {
                ConfigError::FileOperationError(format!(
                    "Failed to inspect client config target {}: {}",
                    config_path, err
                ))
            }
        })?;

        if metadata.is_file() {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&resolved_path)
                .await
                .map_err(|_| ConfigError::PathNotWritable {
                    path: resolved_path.clone(),
                })?;
        } else if metadata.is_dir() {
            Self::validate_directory_target_writable(&resolved_path).await?;
        } else {
            return Err(ConfigError::DataAccessError(format!(
                "Client config target is neither a file nor a directory: {}",
                config_path
            )));
        }

        Ok(Some(config_path))
    }

    pub(super) async fn validate_directory_target_writable(directory_path: &std::path::Path) -> ConfigResult<()> {
        let probe_name = format!(
            ".mcpmate-write-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        );
        let probe_path = directory_path.join(probe_name);

        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe_path)
            .await
            .map_err(|_| ConfigError::PathNotWritable {
                path: directory_path.to_path_buf(),
            })?;

        tokio::fs::remove_file(&probe_path)
            .await
            .map_err(|_| ConfigError::PathNotWritable {
                path: directory_path.to_path_buf(),
            })?;

        Ok(())
    }

    pub async fn has_verified_local_config_target(
        &self,
        client_id: &str,
    ) -> ConfigResult<bool> {
        Ok(self.verified_local_config_target(client_id).await?.is_some())
    }

    #[cfg(test)]
    pub(crate) async fn seed_runtime_template_snapshots(
        db_pool: &SqlitePool,
        file_source: &FileTemplateSource,
    ) -> crate::clients::error::ConfigResult<()> {
        let templates = file_source.list_client().await?;
        Self::seed_runtime_template_snapshots_from_templates(db_pool, &templates).await
    }

    pub async fn refresh_runtime_templates_from_admin_discovery(
        db_pool: &SqlitePool,
        base_url: &str,
    ) -> crate::clients::error::ConfigResult<()> {
        let templates = fetch_admin_discovery_client_templates_strict(base_url).await?;
        Self::replace_runtime_template_snapshots_from_templates(db_pool, &templates).await?;
        set_client_discovery_snapshot_last_success_at(db_pool, Utc::now().to_rfc3339()).await
    }

    async fn replace_runtime_template_snapshots_from_templates(
        db_pool: &SqlitePool,
        templates: &[ClientTemplate],
    ) -> crate::clients::error::ConfigResult<()> {
        let mut tx = db_pool
            .begin()
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        sqlx::query("DELETE FROM client_template_runtime")
            .execute(&mut *tx)
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        for template in templates {
            let payload_json = serde_json::to_string(&template).map_err(|err| {
                ConfigError::TemplateParseError(format!("Failed to serialize runtime template payload: {}", err))
            })?;
            sqlx::query(
                r#"
                INSERT INTO client_template_runtime (identifier, payload_json)
                VALUES (?, ?)
                "#,
            )
            .bind(&template.identifier)
            .bind(payload_json)
            .execute(&mut *tx)
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))
    }

    #[cfg(test)]
    async fn seed_runtime_template_snapshots_from_templates(
        db_pool: &SqlitePool,
        templates: &[ClientTemplate],
    ) -> crate::clients::error::ConfigResult<()> {
        for template in templates {
            let payload_json = serde_json::to_string(&template).map_err(|err| {
                ConfigError::TemplateParseError(format!("Failed to serialize runtime template payload: {}", err))
            })?;
            sqlx::query(
                r#"
                INSERT INTO client_template_runtime (identifier, payload_json)
                VALUES (?, ?)
                ON CONFLICT(identifier) DO UPDATE SET
                    payload_json = excluded.payload_json,
                    updated_at = CURRENT_TIMESTAMP
                "#,
            )
            .bind(&template.identifier)
            .bind(payload_json)
            .execute(db_pool)
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        }
        Ok(())
    }

    pub async fn runtime_template_metadata(
        &self,
        identifier: &str,
    ) -> ConfigResult<Option<RuntimeClientMetadata>> {
        let payload: Option<String> = sqlx::query_scalar(&format!(
            "SELECT payload_json FROM {} WHERE identifier = ?",
            crate::common::constants::database::tables::CLIENT_TEMPLATE_RUNTIME
        ))
        .bind(identifier)
        .fetch_optional(&*self.db_pool)
        .await
        .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;

        let Some(payload) = payload else {
            return Ok(None);
        };
        let template: ClientTemplate = serde_json::from_str(&payload).map_err(|err| {
            ConfigError::DataAccessError(format!("Failed to parse runtime client template metadata: {err}"))
        })?;

        Ok(Some(RuntimeClientMetadata::from_template(&template)))
    }

    #[cfg(test)]
    pub(crate) async fn seed_client_runtime_rows(
        db_pool: &SqlitePool,
        file_source: &FileTemplateSource,
    ) -> crate::clients::error::ConfigResult<()> {
        let templates = file_source.list_client().await?;
        Self::seed_client_runtime_rows_from_templates(db_pool, &templates).await
    }

    pub(crate) fn preview_state_from_detected_template(
        identifier: &str,
        display_name: &str,
        config_path: Option<&str>,
        template: &ClientTemplate,
    ) -> ClientStateRow {
        let persisted_config = PersistedTemplateConfig::from_template(template);
        let runtime_metadata = serde_json::json!({
            "runtime_client": RuntimeClientMetadata::from_template(template)
        });

        ClientStateRow {
            id: crate::generate_id!("clnt"),
            identifier: identifier.to_string(),
            name: display_name.to_string(),
            display_name: Some(display_name.to_string()),
            config_path: config_path.map(ToString::to_string),
            backup_policy: Some("keep_n".to_string()),
            backup_limit: Some(5),
            capability_source: Some("activated".to_string()),
            governance_kind: Some("passive".to_string()),
            connection_mode: Some("local_config_detected".to_string()),
            registration_origin: Some("config_detection".to_string()),
            runtime_observed: Some(0),
            approval_status: Some("pending".to_string()),
            attachment_state: Some("detached".to_string()),
            template_identifier: Some(template.identifier.clone()),
            config_format: persisted_config.config_format,
            protocol_revision: persisted_config.protocol_revision,
            container_type: persisted_config.container_type,
            container_keys: persisted_config.container_keys,
            storage_kind: persisted_config.storage_kind,
            storage_adapter: persisted_config.storage_adapter,
            storage_path_strategy: persisted_config.storage_path_strategy,
            merge_strategy: persisted_config.merge_strategy,
            keep_original_config: persisted_config.keep_original_config,
            managed_source: persisted_config.managed_source,
            transports: persisted_config.transports,
            config_file_parse: persisted_config.config_file_parse,
            approval_metadata: serde_json::to_string(&runtime_metadata).ok(),
            ..ClientStateRow::default()
        }
    }

    #[cfg(test)]
    async fn seed_client_runtime_rows_from_templates(
        db_pool: &SqlitePool,
        templates: &[ClientTemplate],
    ) -> crate::clients::error::ConfigResult<()> {
        for template in templates {
            let display_name = template.display_name.as_deref().unwrap_or(&template.identifier);
            let config_path = Self::extract_runtime_config_path_from_template(template);
            let id = crate::generate_id!("clnt");
            let persisted_config = PersistedTemplateConfig::from_template(template);

            let runtime_metadata = serde_json::json!({
                "runtime_client": RuntimeClientMetadata::from_template(template)
            });
            let approval_metadata = serde_json::to_string(&runtime_metadata).ok();

            sqlx::query(
                r#"
                INSERT INTO client (
                    id, name, display_name, identifier, config_path, backup_policy, backup_limit,
                    capability_source, governance_kind, connection_mode, approval_status, template_identifier,
                    config_format, protocol_revision, container_type, container_keys,
                    storage_kind, storage_adapter, storage_path_strategy,
                    merge_strategy, keep_original_config, managed_source, transports, config_file_parse,
                    approval_metadata, attachment_state
                )
                VALUES (?, ?, ?, ?, ?, 'keep_n', 5, 'activated', 'passive', 'local_config_detected', 'approved', ?,
                        ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'attached')
                ON CONFLICT(identifier) DO UPDATE SET
                    display_name = COALESCE(NULLIF(client.display_name, ''), excluded.display_name),
                    config_path = COALESCE(NULLIF(client.config_path, ''), excluded.config_path),
                    template_identifier = COALESCE(NULLIF(client.template_identifier, ''), excluded.template_identifier),
                    config_format = excluded.config_format,
                    protocol_revision = excluded.protocol_revision,
                    container_type = excluded.container_type,
                    container_keys = excluded.container_keys,
                    storage_kind = excluded.storage_kind,
                    storage_adapter = excluded.storage_adapter,
                    storage_path_strategy = excluded.storage_path_strategy,
                    merge_strategy = excluded.merge_strategy,
                    keep_original_config = excluded.keep_original_config,
                    managed_source = excluded.managed_source,
                    transports = COALESCE(client.transports, excluded.transports),
                    config_file_parse = COALESCE(client.config_file_parse, excluded.config_file_parse),
                    approval_metadata = COALESCE(client.approval_metadata, excluded.approval_metadata),
                    updated_at = CURRENT_TIMESTAMP
                "#,
            )
            .bind(&id)
            .bind(display_name)
            .bind(display_name)
            .bind(&template.identifier)
            .bind(config_path)
            .bind(&template.identifier)
            .bind(persisted_config.config_format)
            .bind(persisted_config.protocol_revision)
            .bind(persisted_config.container_type)
            .bind(persisted_config.container_keys)
            .bind(persisted_config.storage_kind)
            .bind(persisted_config.storage_adapter)
            .bind(persisted_config.storage_path_strategy)
            .bind(persisted_config.merge_strategy)
            .bind(persisted_config.keep_original_config)
            .bind(persisted_config.managed_source)
            .bind(persisted_config.transports)
            .bind(persisted_config.config_file_parse)
            .bind(approval_metadata)
            .execute(db_pool)
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        }

        Ok(())
    }

    pub(crate) fn extract_runtime_config_path_from_template(template: &ClientTemplate) -> Option<String> {
        let platform = PathService::get_current_platform();
        let rules = template.platform_rules(platform)?;
        let rule = rules.first()?;
        let candidate = rule.config_path.as_ref().or(Some(&rule.value))?;
        PathService::new()
            .ok()?
            .resolve_user_path(candidate)
            .ok()
            .map(|value| value.to_string_lossy().to_string())
    }
}

#[cfg(test)]
mod runtime_template_snapshot_fixtures {
    use super::*;

    pub(super) fn runtime_template(identifier: &str) -> ClientTemplate {
        ClientTemplate {
            identifier: identifier.to_string(),
            display_name: Some(identifier.to_string()),
            ..Default::default()
        }
    }

    pub(super) struct AdminDiscoveryBaseUrlOverride {
        previous: Option<String>,
    }

    impl AdminDiscoveryBaseUrlOverride {
        pub(super) fn set(value: String) -> Self {
            let previous = std::env::var(crate::clients::discovery::ADMIN_DISCOVERY_BASE_URL_ENV).ok();
            unsafe {
                std::env::set_var(crate::clients::discovery::ADMIN_DISCOVERY_BASE_URL_ENV, value);
            }
            Self { previous }
        }
    }

    pub(super) struct TempDirOverride {
        previous: Option<std::ffi::OsString>,
    }

    impl TempDirOverride {
        pub(super) fn set(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("TMPDIR");
            unsafe {
                std::env::set_var("TMPDIR", path);
            }
            Self { previous }
        }
    }

    impl Drop for TempDirOverride {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe {
                    std::env::set_var("TMPDIR", value);
                },
                None => unsafe {
                    std::env::remove_var("TMPDIR");
                },
            }
        }
    }

    impl Drop for AdminDiscoveryBaseUrlOverride {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe {
                    std::env::set_var(crate::clients::discovery::ADMIN_DISCOVERY_BASE_URL_ENV, value);
                },
                None => unsafe {
                    std::env::remove_var(crate::clients::discovery::ADMIN_DISCOVERY_BASE_URL_ENV);
                },
            }
        }
    }

    pub(super) async fn configure_runtime_snapshot(
        pool: &SqlitePool,
        ttl_seconds: i64,
        last_success_at: Option<String>,
    ) {
        let mut settings = crate::system::settings::get_settings(pool)
            .await
            .expect("read system settings");
        settings.client_discovery_snapshot_ttl_seconds = ttl_seconds;
        settings.client_discovery_snapshot_last_success_at = last_success_at;
        crate::system::settings::set_settings(pool, &settings)
            .await
            .expect("write system settings");
    }

    pub(super) fn admin_discovery_file_client_response(identifier: &str) -> serde_json::Value {
        serde_json::json!({
            "clients": [{
                "identifier": identifier,
                "config": {
                    "kind": "file",
                    "file": {
                        "paths": { "macos": "~/.recovered/mcp.json" },
                        "container": { "keys": ["mcpServers"] }
                    },
                    "transports": { "stdio": { "command_field": "command" } }
                }
            }],
            "page": { "limit": 100, "offset": 0, "total": 1 }
        })
    }
}

#[cfg(test)]
mod render_definition_tests {
    use super::*;

    #[test]
    fn build_render_definition_ignores_metadata_supported_transports() {
        let state = ClientStateRow {
            identifier: "zed".to_string(),
            config_path: Some("~/.config/zed/settings.json".to_string()),
            connection_mode: Some("local_config_detected".to_string()),
            template_identifier: Some("zed".to_string()),
            config_format: Some("json".to_string()),
            container_type: Some("object".to_string()),
            container_keys: Some("[\"context_servers\"]".to_string()),
            transports: Some(
                serde_json::json!({
                    "stdio": {
                        "template": {
                            "type": "stdio",
                            "command": "{{{command}}}"
                        },
                    "include_type": false
                    }
                })
                .to_string(),
            ),
            approval_metadata: Some(
                serde_json::json!({
                    "runtime_client": {
                        "supported_transports": ["streamable_http"]
                    }
                })
                .to_string(),
            ),
            ..ClientStateRow::default()
        };

        let definition = ClientConfigService::build_render_definition_from_state(&state, None)
            .expect("metadata transports should not affect render definition");

        assert!(definition.config_mapping.format_rules.contains_key("stdio"));
        assert!(!definition.config_mapping.format_rules.contains_key("streamable_http"));
    }

    #[test]
    fn build_render_definition_derives_supported_transports_from_transports() {
        let state = ClientStateRow {
            identifier: "cursor".to_string(),
            config_path: Some("~/.cursor/mcp.json".to_string()),
            connection_mode: Some("local_config_detected".to_string()),
            template_identifier: Some("cursor".to_string()),
            config_format: Some("json".to_string()),
            container_type: Some("object".to_string()),
            container_keys: Some("[\"mcpServers\"]".to_string()),
            storage_kind: Some("file".to_string()),
            storage_path_strategy: Some("config_path".to_string()),
            merge_strategy: Some("replace".to_string()),
            managed_source: Some("profile".to_string()),
            transports: Some(
                serde_json::json!({
                    "streamable_http": {
                        "template": {
                            "type": "streamable_http",
                            "url": "{{{url}}}"
                        },
                        "include_type": false
                    }
                })
                .to_string(),
            ),
            approval_metadata: None,
            ..ClientStateRow::default()
        };

        let definition = ClientConfigService::build_render_definition_from_state(&state, None)
            .expect("render definition should derive transports from format rules");

        assert!(definition.config_mapping.format_rules.contains_key("streamable_http"));
    }

    #[test]
    fn build_render_definition_requires_canonical_transport_keys() {
        let state = ClientStateRow {
            identifier: "cursor".to_string(),
            config_path: Some("~/.cursor/mcp.json".to_string()),
            connection_mode: Some("local_config_detected".to_string()),
            template_identifier: Some("cursor".to_string()),
            config_format: Some("json".to_string()),
            container_type: Some("object".to_string()),
            container_keys: Some("[\"mcpServers\"]".to_string()),
            storage_kind: Some("file".to_string()),
            storage_path_strategy: Some("config_path".to_string()),
            merge_strategy: Some("replace".to_string()),
            managed_source: Some("profile".to_string()),
            transports: Some(
                serde_json::json!({
                    "http": {
                        "template": {
                            "type": "streamable_http",
                            "url": "{{{url}}}"
                        },
                        "include_type": false
                    }
                })
                .to_string(),
            ),
            approval_metadata: None,
            ..ClientStateRow::default()
        };

        let error = ClientConfigService::build_render_definition_from_state(&state, None)
            .expect_err("alias transport keys should be rejected");

        assert!(error.to_string().contains("missing persisted transports"));
    }

    #[tokio::test]
    async fn detach_client_rewrites_json5_style_declared_json_config() {
        use crate::clients::models::AttachmentState;
        use crate::clients::source::{ClientConfigSource, DbTemplateSource, FileTemplateSource, TemplateRoot};
        use crate::config::client::init::initialize_client_table;
        use sqlx::sqlite::SqlitePoolOptions;
        use std::sync::Arc;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().expect("temp dir");
        let config_path = temp_dir.path().join("zed-settings.json");
        tokio::fs::write(
            &config_path,
            r#"{
                "context_servers": {
                    "MCPMate": {
                        "command": "bridge",
                    },
                    "other": {
                        "command": "node",
                    },
                },
            }"#,
        )
        .await
        .expect("write json5-style config");

        let pool = Arc::new(
            SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .expect("sqlite pool"),
        );
        crate::helpers::prepare_config_database(pool.as_ref()).await;
        initialize_client_table(pool.as_ref()).await.expect("init client table");

        let template_root = TemplateRoot::new(temp_dir.path().join("client-templates"));
        let source = Arc::new(
            FileTemplateSource::bootstrap(template_root)
                .await
                .expect("template source"),
        );
        ClientConfigService::seed_runtime_template_snapshots(pool.as_ref(), source.as_ref())
            .await
            .expect("seed runtime templates");

        let client_id = "zed.detach.integration";
        let generated_id = crate::generate_id!("clnt");
        sqlx::query(
            r#"
            INSERT INTO client (
                id, name, display_name, identifier, config_path, backup_policy, backup_limit,
                approval_status, governance_kind, connection_mode, attachment_state,
                config_format, container_type, container_keys, storage_kind, storage_path_strategy
            )
            VALUES (?, 'Zed', 'Zed', ?, ?, 'keep_n', 5, 'approved', 'passive', 'local_config_detected', 'attached',
                    'json', 'object', ?, 'file', 'config_path')
            "#,
        )
        .bind(&generated_id)
        .bind(client_id)
        .bind(config_path.to_string_lossy().to_string())
        .bind(r#"["context_servers"]"#)
        .execute(pool.as_ref())
        .await
        .expect("insert client row");

        let runtime_source: Arc<dyn ClientConfigSource> =
            Arc::new(DbTemplateSource::new(pool.clone()).expect("runtime source"));
        let service = ClientConfigService::with_source(pool, runtime_source)
            .await
            .expect("client config service");

        let changed = service.detach_client(client_id).await.expect("detach client");
        assert!(changed);

        let written = tokio::fs::read_to_string(&config_path)
            .await
            .expect("read detached config");
        let parsed: serde_json::Value = serde_json::from_str(&written).expect("detached file should be strict JSON");
        assert!(parsed["context_servers"].get("MCPMate").is_none());
        assert_eq!(parsed["context_servers"]["other"]["command"], "node");

        let state = service
            .fetch_state(client_id)
            .await
            .expect("fetch state")
            .expect("state exists");
        assert_eq!(state.attachment_state(), AttachmentState::Detached);
    }

    #[tokio::test]
    async fn seed_client_runtime_rows_persists_transports_without_transport_metadata() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        let mut transports = HashMap::new();
        transports.insert(
            "streamable_http".to_string(),
            FormatRule {
                template: serde_json::json!({
                    "type": "streamable_http",
                    "url": "{{{url}}}"
                }),
                include_type: false,
                ..Default::default()
            },
        );
        let template = ClientTemplate {
            identifier: "cursor".to_string(),
            display_name: Some("Cursor".to_string()),
            format: TemplateFormat::Json,
            storage: StorageConfig {
                kind: StorageKind::File,
                path_strategy: Some("config_path".to_string()),
                adapter: None,
            },
            config_mapping: ConfigMapping {
                container_keys: vec!["mcpServers".to_string()],
                container_type: crate::clients::models::ContainerType::ObjectMap,
                merge_strategy: MergeStrategy::Replace,
                keep_original_config: false,
                managed_endpoint: None,
                managed_source: Some("profile".to_string()),
                parse: None,
                format_rules: transports,
            },
            ..Default::default()
        };

        ClientConfigService::seed_client_runtime_rows_from_templates(&pool, &[template])
            .await
            .expect("seed client runtime row");
        let (approval_metadata, persisted_transports): (String, String) =
            sqlx::query_as("SELECT approval_metadata, transports FROM client WHERE identifier = ?")
                .bind("cursor")
                .fetch_one(&pool)
                .await
                .expect("load client row");
        let value: serde_json::Value = serde_json::from_str(&approval_metadata).expect("approval metadata json");
        let persisted_rules: HashMap<String, FormatRule> =
            serde_json::from_str(&persisted_transports).expect("persisted transports json");

        assert!(value["runtime_client"].get("supported_transports").is_none());
        assert_eq!(
            supported_transports_from_transports(&persisted_rules),
            vec!["streamable_http".to_string()]
        );
    }
}

#[cfg(test)]
mod runtime_template_snapshot_tests {
    use super::runtime_template_snapshot_fixtures::{
        AdminDiscoveryBaseUrlOverride, TempDirOverride, admin_discovery_file_client_response,
        configure_runtime_snapshot, runtime_template,
    };
    use super::*;

    #[tokio::test]
    async fn refresh_admin_discovery_does_not_seed_embedded_static_templates() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/discovery/clients"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "clients": [],
                "page": {
                    "limit": 50,
                    "offset": 0,
                    "total": 0
                }
            })))
            .mount(&server)
            .await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");

        ClientConfigService::refresh_runtime_templates_from_admin_discovery(&pool, &server.uri())
            .await
            .expect("refresh Admin discovery");

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_template_runtime")
            .fetch_one(&pool)
            .await
            .expect("runtime template count");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn refresh_admin_discovery_rejects_invalid_entry_without_replacing_snapshot_or_timestamp() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/discovery/clients"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "clients": [
                    admin_discovery_file_client_response("valid-client")["clients"][0].clone(),
                    {
                        "identifier": "invalid-client",
                        "config": { "kind": "file" }
                    }
                ],
                "page": { "limit": 100, "offset": 0, "total": 2 }
            })))
            .mount(&server)
            .await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        ClientConfigService::seed_runtime_template_snapshots_from_templates(
            &pool,
            &[runtime_template("cached-client")],
        )
        .await
        .expect("seed cached runtime template");
        let old_success_at = (Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        configure_runtime_snapshot(&pool, 60, Some(old_success_at.clone())).await;

        let result = ClientConfigService::refresh_runtime_templates_from_admin_discovery(&pool, &server.uri()).await;

        assert!(result.is_err(), "invalid mapped entry must reject the whole refresh");
        let cached: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM client_template_runtime WHERE identifier = 'cached-client'")
                .fetch_one(&pool)
                .await
                .expect("cached runtime template count");
        assert_eq!(cached, 1);
        assert_eq!(
            crate::system::settings::get_settings(&pool)
                .await
                .expect("read settings after rejected refresh")
                .client_discovery_snapshot_last_success_at,
            Some(old_success_at)
        );
    }

    #[tokio::test]
    async fn refresh_admin_discovery_rejects_nonobject_file_entry_without_replacing_snapshot_or_timestamp() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/discovery/clients"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "clients": [
                    admin_discovery_file_client_response("valid-client")["clients"][0].clone(),
                    {
                        "identifier": "non-object-file-client",
                        "config": { "kind": "file", "file": [] }
                    }
                ],
                "page": { "limit": 100, "offset": 0, "total": 3 }
            })))
            .mount(&server)
            .await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        ClientConfigService::seed_runtime_template_snapshots_from_templates(
            &pool,
            &[runtime_template("cached-client")],
        )
        .await
        .expect("seed cached runtime template");
        let old_success_at = (Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        configure_runtime_snapshot(&pool, 60, Some(old_success_at.clone())).await;

        let result = ClientConfigService::refresh_runtime_templates_from_admin_discovery(&pool, &server.uri()).await;

        assert!(
            result.is_err(),
            "a declared file entry must contain an object file mapping"
        );
        let cached: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM client_template_runtime WHERE identifier = 'cached-client'")
                .fetch_one(&pool)
                .await
                .expect("cached runtime template count");
        assert_eq!(cached, 1);
        assert_eq!(
            crate::system::settings::get_settings(&pool)
                .await
                .expect("read settings after rejected refresh")
                .client_discovery_snapshot_last_success_at,
            Some(old_success_at)
        );
    }

    #[tokio::test]
    async fn refresh_admin_discovery_skips_nonfile_entry_without_identifier_and_replaces_snapshot() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/discovery/clients"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "clients": [
                    { "config": { "kind": "none" } },
                    admin_discovery_file_client_response("valid-client")["clients"][0].clone()
                ],
                "page": { "limit": 100, "offset": 0, "total": 2 }
            })))
            .mount(&server)
            .await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        ClientConfigService::seed_runtime_template_snapshots_from_templates(
            &pool,
            &[runtime_template("cached-client")],
        )
        .await
        .expect("seed cached runtime template");

        ClientConfigService::refresh_runtime_templates_from_admin_discovery(&pool, &server.uri())
            .await
            .expect("non-file entries must not make strict runtime discovery fail");

        let cached: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM client_template_runtime WHERE identifier = 'cached-client'")
                .fetch_one(&pool)
                .await
                .expect("cached runtime template count");
        let valid: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM client_template_runtime WHERE identifier = 'valid-client'")
                .fetch_one(&pool)
                .await
                .expect("valid runtime template count");
        assert_eq!(cached, 0, "successful strict refresh must replace the old snapshot");
        assert_eq!(valid, 1, "a valid file entry must survive non-file catalog entries");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn bootstrap_continues_when_admin_discovery_is_unavailable() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/discovery/clients"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        ClientConfigService::seed_runtime_template_snapshots_from_templates(
            &pool,
            &[runtime_template("cached-client")],
        )
        .await
        .expect("seed cached runtime template");
        let _base_url = AdminDiscoveryBaseUrlOverride::set(server.uri());

        let result = ClientConfigService::bootstrap(Arc::new(pool.clone())).await;

        let service = result.expect("bootstrap should continue without Admin discovery data");
        let templates = service
            .template_source
            .list_client()
            .await
            .expect("list runtime templates");
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].identifier, "cached-client");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn fresh_runtime_snapshot_skips_discovery_during_bootstrap_and_forced_list() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/discovery/clients"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(admin_discovery_file_client_response("unexpected-client")),
            )
            .mount(&server)
            .await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        ClientConfigService::seed_runtime_template_snapshots_from_templates(
            &pool,
            &[runtime_template("cached-client")],
        )
        .await
        .expect("seed cached runtime template");
        configure_runtime_snapshot(&pool, 3_600, Some(Utc::now().to_rfc3339())).await;
        let _base_url = AdminDiscoveryBaseUrlOverride::set(server.uri());

        let service = ClientConfigService::bootstrap(Arc::new(pool.clone()))
            .await
            .expect("bootstrap with a fresh snapshot");
        service
            .list_clients(true, false)
            .await
            .expect("forced detection with a fresh snapshot");

        assert!(
            server
                .received_requests()
                .await
                .expect("read captured requests")
                .is_empty(),
            "fresh snapshot must not call Admin discovery"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn list_clients_refreshes_runtime_templates_after_snapshot_becomes_stale() {
        let server = wiremock::MockServer::start().await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        ClientConfigService::seed_runtime_template_snapshots_from_templates(
            &pool,
            &[runtime_template("cached-client")],
        )
        .await
        .expect("seed cached runtime template");
        configure_runtime_snapshot(&pool, 3_600, Some(Utc::now().to_rfc3339())).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/discovery/clients"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(admin_discovery_file_client_response("recovered-client")),
            )
            .mount(&server)
            .await;
        let _base_url = AdminDiscoveryBaseUrlOverride::set(server.uri());

        let service = ClientConfigService::bootstrap(Arc::new(pool.clone()))
            .await
            .expect("bootstrap should retain a fresh snapshot");
        configure_runtime_snapshot(&pool, 60, Some((Utc::now() - chrono::Duration::hours(2)).to_rfc3339())).await;
        service
            .list_clients(true, false)
            .await
            .expect("list should refresh a stale runtime snapshot");

        assert_eq!(
            server.received_requests().await.expect("read captured requests").len(),
            1,
            "list must refresh exactly once after the snapshot becomes stale"
        );
        let settings = crate::system::settings::get_settings(&pool)
            .await
            .expect("read settings after refresh");
        assert!(settings.client_discovery_snapshot_last_success_at.is_some());

        let recovered: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM client_template_runtime WHERE identifier = 'recovered-client'")
                .fetch_one(&pool)
                .await
                .expect("runtime template count");
        assert_eq!(recovered, 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn list_clients_retains_stale_templates_when_runtime_snapshot_refresh_fails() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/discovery/clients"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        ClientConfigService::seed_runtime_template_snapshots_from_templates(
            &pool,
            &[runtime_template("cached-client")],
        )
        .await
        .expect("seed cached runtime template");
        ClientConfigService::seed_client_runtime_rows_from_templates(&pool, &[runtime_template("cached-client")])
            .await
            .expect("seed cached client row");
        let old_success_at = (Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        configure_runtime_snapshot(&pool, 3_600, Some(Utc::now().to_rfc3339())).await;
        let _base_url = AdminDiscoveryBaseUrlOverride::set(server.uri());

        let service = ClientConfigService::bootstrap(Arc::new(pool.clone()))
            .await
            .expect("bootstrap should retain cached snapshot");
        configure_runtime_snapshot(&pool, 60, Some(old_success_at.clone())).await;
        let descriptors = service
            .list_clients(true, false)
            .await
            .expect("list should retain cached snapshot when refresh fails");

        assert!(descriptors.iter().any(|descriptor| {
            descriptor.state.identifier() == "cached-client"
                && descriptor
                    .template
                    .as_ref()
                    .map(|template| template.identifier.as_str())
                    == Some("cached-client")
        }));

        let cached: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM client_template_runtime WHERE identifier = 'cached-client'")
                .fetch_one(&pool)
                .await
                .expect("cached runtime template count");
        assert_eq!(cached, 1);
        assert_eq!(
            crate::system::settings::get_settings(&pool)
                .await
                .expect("read settings after failed refresh")
                .client_discovery_snapshot_last_success_at,
            Some(old_success_at)
        );
        assert_eq!(
            server.received_requests().await.expect("read captured requests").len(),
            1,
            "list must attempt the stale snapshot refresh exactly once"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn snapshot_metadata_write_bypasses_settings_backup_directory() {
        let temp_dir = tempfile::TempDir::new().expect("create isolated temp root");
        let _tmpdir = TempDirOverride::set(temp_dir.path());
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/discovery/clients"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(admin_discovery_file_client_response("recovered-client")),
            )
            .mount(&server)
            .await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        ClientConfigService::seed_runtime_template_snapshots_from_templates(
            &pool,
            &[runtime_template("cached-client")],
        )
        .await
        .expect("seed cached runtime template");
        let old_success_at = (Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        configure_runtime_snapshot(&pool, 60, Some(old_success_at.clone())).await;

        let settings_path = std::fs::read_dir(temp_dir.path())
            .expect("read isolated temp root")
            .map(|entry| entry.expect("read temp root entry").path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("mcpmate-system-settings-test-"))
            })
            .expect("find test settings config");
        let backup_root = settings_path.parent().expect("settings config parent").join("backups");
        std::fs::create_dir(&backup_root).expect("create backup root parent");
        std::fs::write(backup_root.join("client"), b"block backup directory creation")
            .expect("create backup root blocker");
        let _base_url = AdminDiscoveryBaseUrlOverride::set(server.uri());
        let source: Arc<dyn ClientConfigSource> =
            Arc::new(DbTemplateSource::new(Arc::new(pool.clone())).expect("runtime source"));
        let service =
            ClientConfigService::with_source_and_admin_discovery_refresh(Arc::new(pool.clone()), source, true)
                .await
                .expect("create runtime service");

        service
            .ensure_runtime_template_snapshot()
            .await
            .expect("metadata update must not require the settings backup directory");
        let recovered: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM client_template_runtime WHERE identifier = 'recovered-client'")
                .fetch_one(&pool)
                .await
                .expect("recovered runtime template count");
        let cached: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM client_template_runtime WHERE identifier = 'cached-client'")
                .fetch_one(&pool)
                .await
                .expect("cached runtime template count");
        assert_eq!(recovered, 1, "successful discovery must replace the SQLite snapshot");
        assert_eq!(cached, 0, "previous SQLite snapshot entries must be replaced");
        assert!(
            crate::system::settings::get_settings(&pool)
                .await
                .expect("read settings after metadata write")
                .client_discovery_snapshot_last_success_at
                .is_some(),
            "successful refresh must persist its timestamp without backups"
        );

        service
            .list_clients(false, false)
            .await
            .expect("list continues with the replaced SQLite snapshot");

        assert_eq!(
            server.received_requests().await.expect("read captured requests").len(),
            1,
            "fresh timestamp must avoid another Admin discovery request during the next list"
        );
        assert_eq!(
            service
                .get_client_template("recovered-client")
                .await
                .expect("read replaced runtime template")
                .identifier,
            "recovered-client"
        );
        assert!(
            crate::system::settings::get_settings(&pool)
                .await
                .expect("read settings after metadata update")
                .client_discovery_snapshot_last_success_at
                .is_some(),
            "the successful metadata update must remain readable"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn runtime_snapshot_refresh_is_single_flight() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/discovery/clients"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(50))
                    .set_body_json(admin_discovery_file_client_response("discovered-client")),
            )
            .mount(&server)
            .await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        configure_runtime_snapshot(&pool, 3_600, None).await;
        let _base_url = AdminDiscoveryBaseUrlOverride::set(server.uri());
        let source: Arc<dyn ClientConfigSource> =
            Arc::new(DbTemplateSource::new(Arc::new(pool.clone())).expect("create runtime source"));
        let service = Arc::new(
            ClientConfigService::with_source_and_admin_discovery_refresh(Arc::new(pool), source, true)
                .await
                .expect("create runtime service"),
        );

        let (first, second) = tokio::join!(
            service.ensure_runtime_template_snapshot(),
            service.ensure_runtime_template_snapshot(),
        );
        first.expect("first refresh");
        second.expect("second refresh");
        assert_eq!(
            server.received_requests().await.expect("read captured requests").len(),
            1,
            "concurrent refreshes must share one Admin discovery request"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn concurrent_list_clients_shares_failed_runtime_refresh_attempt() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/discovery/clients"))
            .respond_with(wiremock::ResponseTemplate::new(503).set_delay(std::time::Duration::from_millis(50)))
            .mount(&server)
            .await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        ClientConfigService::seed_runtime_template_snapshots_from_templates(
            &pool,
            &[runtime_template("cached-client")],
        )
        .await
        .expect("seed cached runtime template");
        ClientConfigService::seed_client_runtime_rows_from_templates(&pool, &[runtime_template("cached-client")])
            .await
            .expect("seed cached client row");
        configure_runtime_snapshot(&pool, 60, Some((Utc::now() - chrono::Duration::hours(2)).to_rfc3339())).await;
        let _base_url = AdminDiscoveryBaseUrlOverride::set(server.uri());
        let source: Arc<dyn ClientConfigSource> =
            Arc::new(DbTemplateSource::new(Arc::new(pool.clone())).expect("create runtime source"));
        let service = Arc::new(
            ClientConfigService::with_source_and_admin_discovery_refresh(Arc::new(pool), source, true)
                .await
                .expect("create runtime service"),
        );

        let (first, second) = tokio::join!(service.list_clients(false, false), service.list_clients(false, false),);

        for descriptors in [first, second] {
            assert!(
                descriptors
                    .expect("list must retain the local snapshot after discovery failure")
                    .iter()
                    .any(|descriptor| descriptor.state.identifier() == "cached-client")
            );
        }
        assert_eq!(
            server.received_requests().await.expect("read captured requests").len(),
            1,
            "concurrent lists must share the in-flight failed discovery attempt"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn stale_snapshot_stops_refreshing_after_three_failures() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/discovery/clients"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        ClientConfigService::seed_runtime_template_snapshots_from_templates(
            &pool,
            &[runtime_template("cached-client")],
        )
        .await
        .expect("seed cached runtime template");
        ClientConfigService::seed_client_runtime_rows_from_templates(&pool, &[runtime_template("cached-client")])
            .await
            .expect("seed cached client row");
        configure_runtime_snapshot(&pool, 60, Some((Utc::now() - chrono::Duration::hours(2)).to_rfc3339())).await;
        let _base_url = AdminDiscoveryBaseUrlOverride::set(server.uri());
        let source: Arc<dyn ClientConfigSource> =
            Arc::new(DbTemplateSource::new(Arc::new(pool.clone())).expect("create runtime source"));
        let service = ClientConfigService::with_source_and_admin_discovery_refresh(Arc::new(pool), source, true)
            .await
            .expect("create runtime service");

        for attempt in 1..=3 {
            let descriptors = service
                .list_clients(false, false)
                .await
                .expect("list must retain the local snapshot after discovery failure");
            assert!(
                descriptors
                    .iter()
                    .any(|descriptor| descriptor.state.identifier() == "cached-client"),
                "attempt {attempt} must retain the cached template"
            );
        }
        assert_eq!(
            server.received_requests().await.expect("read captured requests").len(),
            3,
            "each of the first three stale refreshes must request Admin discovery"
        );

        let descriptors = service
            .list_clients(false, false)
            .await
            .expect("list must retain the cached snapshot after the retry cap");
        assert!(
            descriptors
                .iter()
                .any(|descriptor| descriptor.state.identifier() == "cached-client")
        );
        assert_eq!(
            server.received_requests().await.expect("read captured requests").len(),
            3,
            "the fourth stale refresh must wait for the next failure window"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn retry_cap_uses_a_fixed_six_hour_window_without_a_success_timestamp() {
        let temp_dir = tempfile::tempdir().expect("create temporary settings directory");
        let _temp_dir = TempDirOverride::set(temp_dir.path());
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        ClientConfigService::seed_runtime_template_snapshots_from_templates(
            &pool,
            &[runtime_template("cached-client")],
        )
        .await
        .expect("seed cached runtime template");
        configure_runtime_snapshot(&pool, 60, None).await;

        let mut settings = get_settings(&pool).await.expect("read settings");
        settings.client_discovery_snapshot_failure_window_started_at =
            Some((Utc::now() - chrono::Duration::minutes(5)).to_rfc3339());
        settings.client_discovery_snapshot_consecutive_failures = 3;
        crate::system::settings::set_settings(&pool, &settings)
            .await
            .expect("write retry cap settings");

        assert!(
            !ClientConfigService::runtime_template_snapshot_needs_refresh(&pool)
                .await
                .expect("evaluate capped retry window"),
            "a missing success timestamp must still honor the retry cap for the fixed six-hour window"
        );

        settings.client_discovery_snapshot_failure_window_started_at =
            Some((Utc::now() - chrono::Duration::hours(7)).to_rfc3339());
        crate::system::settings::set_settings(&pool, &settings)
            .await
            .expect("write expired retry window");

        assert!(
            ClientConfigService::runtime_template_snapshot_needs_refresh(&pool)
                .await
                .expect("evaluate expired retry window"),
            "the retry cap must reset after the fixed six-hour window"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn concurrent_list_clients_shares_nonpositive_ttl_runtime_refresh_attempt() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/discovery/clients"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(50))
                    .set_body_json(admin_discovery_file_client_response("discovered-client")),
            )
            .mount(&server)
            .await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        ClientConfigService::seed_runtime_template_snapshots_from_templates(
            &pool,
            &[runtime_template("cached-client")],
        )
        .await
        .expect("seed cached runtime template");
        ClientConfigService::seed_client_runtime_rows_from_templates(&pool, &[runtime_template("cached-client")])
            .await
            .expect("seed cached client row");
        configure_runtime_snapshot(&pool, -1, Some(Utc::now().to_rfc3339())).await;
        let _base_url = AdminDiscoveryBaseUrlOverride::set(server.uri());
        let source: Arc<dyn ClientConfigSource> =
            Arc::new(DbTemplateSource::new(Arc::new(pool.clone())).expect("create runtime source"));
        let service = Arc::new(
            ClientConfigService::with_source_and_admin_discovery_refresh(Arc::new(pool), source, true)
                .await
                .expect("create runtime service"),
        );

        let (first, second) = tokio::join!(service.list_clients(false, false), service.list_clients(false, false),);

        for descriptors in [first, second] {
            assert!(
                descriptors
                    .expect("list must retain the local snapshot after the shared refresh")
                    .iter()
                    .any(|descriptor| descriptor.state.identifier() == "cached-client")
            );
        }
        assert_eq!(
            server.received_requests().await.expect("read captured requests").len(),
            1,
            "concurrent lists must share the in-flight nonpositive-TTL discovery attempt"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn list_clients_refreshes_fresh_timestamp_when_runtime_snapshot_is_empty() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/discovery/clients"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(admin_discovery_file_client_response("discovered-client")),
            )
            .mount(&server)
            .await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        configure_runtime_snapshot(&pool, 3_600, Some(Utc::now().to_rfc3339())).await;
        let _base_url = AdminDiscoveryBaseUrlOverride::set(server.uri());
        let source: Arc<dyn ClientConfigSource> =
            Arc::new(DbTemplateSource::new(Arc::new(pool.clone())).expect("create runtime source"));
        let service =
            ClientConfigService::with_source_and_admin_discovery_refresh(Arc::new(pool.clone()), source, true)
                .await
                .expect("create runtime service");

        service
            .list_clients(false, false)
            .await
            .expect("list refreshes an empty runtime snapshot");

        assert_eq!(
            server.received_requests().await.expect("read captured requests").len(),
            1,
            "a fresh timestamp without a physical runtime snapshot must refresh"
        );
        let discovered: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM client_template_runtime WHERE identifier = 'discovered-client'")
                .fetch_one(&pool)
                .await
                .expect("discovered runtime template count");
        assert_eq!(discovered, 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn negative_runtime_snapshot_ttl_refreshes_on_every_list_access() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/discovery/clients"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(admin_discovery_file_client_response("discovered-client")),
            )
            .mount(&server)
            .await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("init client table");
        ClientConfigService::seed_runtime_template_snapshots_from_templates(
            &pool,
            &[runtime_template("cached-client")],
        )
        .await
        .expect("seed cached runtime template");
        configure_runtime_snapshot(&pool, 3_600, Some(Utc::now().to_rfc3339())).await;
        let _base_url = AdminDiscoveryBaseUrlOverride::set(server.uri());
        let service = ClientConfigService::bootstrap(Arc::new(pool.clone()))
            .await
            .expect("bootstrap with fresh runtime snapshot");
        configure_runtime_snapshot(&pool, -1, Some(Utc::now().to_rfc3339())).await;

        service.list_clients(false, false).await.expect("first list refresh");
        service.list_clients(false, false).await.expect("second list refresh");

        assert_eq!(
            server.received_requests().await.expect("read captured requests").len(),
            2,
            "negative TTL must refresh on every list access"
        );
    }
}
