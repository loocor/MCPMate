// Unified server import core for MCPMate
// Provides a single entrypoint used by: server API import, client config import, and first-run config import.

use anyhow::{Context, Result};
use sqlx::{Pool, Sqlite};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

use crate::api::models::server::{ServerMetaPayload, ServersImportConfig};
use crate::clients::analyzer::{ConfigImportSkipReason, InspectedServerEntry};
use crate::clients::models::ClientConfigFileParse;
use crate::clients::service::ClientConfigService;
use crate::common::constants::profile_keys;
use crate::common::server::ServerType;
use crate::common::types::{ServerSource, ServerSourceType};
use crate::config::database::Database;
use crate::config::models::{ConfigValue, HttpTransportKind, Server, ServerMeta, ServerTransportDraft};
use crate::config::server as server_ops;
use crate::config::server::{args, fingerprint, get_all_servers, upsert_server_definition};

// Capability sync utilities for the transactional SQLite catalog.
use crate::core::capability::read_service::CapabilityReadService;
use crate::core::pool::UpstreamConnectionPool;

const IMPORT_DISCOVERY_CONCURRENCY: usize = 2;
static IMPORT_DISCOVERY_PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn import_discovery_permits() -> Arc<Semaphore> {
    IMPORT_DISCOVERY_PERMITS
        .get_or_init(|| Arc::new(Semaphore::new(IMPORT_DISCOVERY_CONCURRENCY)))
        .clone()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    Skip,
    Update,
    Error,
}

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub by_name: bool,
    pub by_fingerprint: bool,
    pub conflict_policy: ConflictPolicy,
    pub preview: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            by_name: true,
            by_fingerprint: true,
            conflict_policy: ConflictPolicy::Skip,
            preview: false,
        }
    }
}

impl ImportOptions {
    /// Default options for dashboard and first-run imports (skip on conflict; optional preview).
    pub fn dashboard_import(preview: bool) -> Self {
        Self {
            by_name: true,
            by_fingerprint: true,
            conflict_policy: ConflictPolicy::Skip,
            preview,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ImportOutcome {
    pub imported: Vec<ImportedServer>,
    pub skipped: Vec<SkippedServer>,
    pub failed: HashMap<String, String>,
    pub scheduled: bool,
    pub runtime_sync_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImportedServer {
    pub name: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub server_type: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SkippedServer {
    pub name: String,
    pub reason: SkipReason,
}

#[derive(Debug, Clone)]
pub enum SkipReason {
    DuplicateName,
    DuplicateFingerprint,
    ConfigInvalidEntry,
    ConfigMissingCommand,
    ConfigMissingUrl,
    ConfigUnrecognized,
    UrlQueryMismatch {
        existing_query: Option<String>,
        incoming_query: Option<String>,
    },
}

impl From<ConfigImportSkipReason> for SkipReason {
    fn from(reason: ConfigImportSkipReason) -> Self {
        match reason {
            ConfigImportSkipReason::InvalidEntry => Self::ConfigInvalidEntry,
            ConfigImportSkipReason::MissingCommand => Self::ConfigMissingCommand,
            ConfigImportSkipReason::MissingUrl => Self::ConfigMissingUrl,
            ConfigImportSkipReason::Unrecognized => Self::ConfigUnrecognized,
        }
    }
}

impl SkipReason {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::DuplicateName => "duplicate_name",
            Self::DuplicateFingerprint => "duplicate_fingerprint",
            Self::ConfigInvalidEntry => "config_invalid_entry",
            Self::ConfigMissingCommand => "config_missing_command",
            Self::ConfigMissingUrl => "config_missing_url",
            Self::ConfigUnrecognized => "config_unrecognized",
            Self::UrlQueryMismatch { .. } => "url_query_mismatch",
        }
    }

    pub(crate) fn is_duplicate_fingerprint(&self) -> bool {
        matches!(self, Self::DuplicateFingerprint)
    }
}

pub struct ClientImportPlan {
    pub items: HashMap<String, ServersImportConfig>,
    pub skipped_servers: Vec<SkippedServer>,
}

fn record_conflict(
    outcome: &mut ImportOutcome,
    name: &str,
    reason: SkipReason,
    policy: ConflictPolicy,
) -> bool {
    match policy {
        ConflictPolicy::Skip => {
            outcome.skipped.push(SkippedServer {
                name: name.to_string(),
                reason,
            });
            true
        }
        ConflictPolicy::Error => {
            outcome.failed.insert(name.to_string(), "duplicate".to_string());
            true
        }
        ConflictPolicy::Update => false,
    }
}

struct ImportCandidate {
    server_type: ServerType,
    persisted_kind: &'static str,
    fingerprint: String,
    url_signature: Option<fingerprint::UrlSignature>,
}

fn prepare_import_candidate(cfg: &ServersImportConfig) -> Result<ImportCandidate> {
    let lc = cfg.kind.trim().to_ascii_lowercase();
    let server_type = ServerType::from_client_format(&lc)
        .map_err(|_| anyhow::anyhow!(format!("Invalid server type '{}'", cfg.kind)))?;
    let persisted_kind = server_type.client_format();
    validate_server_config(persisted_kind, &cfg.command, &cfg.url).map_err(|e| anyhow::anyhow!(e.to_string()))?;

    let dedup = fingerprint::server_dedup_fingerprint(
        server_type,
        cfg.command.as_deref(),
        cfg.url.as_deref(),
        cfg.args.as_deref().unwrap_or_default(),
    );

    Ok(ImportCandidate {
        server_type,
        persisted_kind,
        fingerprint: dedup.value,
        url_signature: dedup.url_signature,
    })
}

fn import_conflict_reason(
    existing: &ExistingIndex,
    name: &str,
    candidate: &ImportCandidate,
    opts: &ImportOptions,
) -> Option<SkipReason> {
    if opts.by_fingerprint
        && !candidate.fingerprint.is_empty()
        && existing.fingerprints.contains(&candidate.fingerprint)
    {
        return Some(SkipReason::DuplicateFingerprint);
    }

    if opts.by_fingerprint {
        if let Some(sig) = candidate.url_signature.as_ref() {
            if existing.url_bases.contains(&sig.base) {
                let existing_sig = existing.url_signatures.get(&sig.base);
                return Some(SkipReason::UrlQueryMismatch {
                    existing_query: existing_sig.and_then(|s| s.display_query()),
                    incoming_query: sig.display_query(),
                });
            }
        }
    }

    if opts.by_name && existing.names.contains(name) {
        return Some(SkipReason::DuplicateName);
    }

    None
}

pub(crate) async fn find_import_conflicts(
    db_pool: &Pool<Sqlite>,
    items: &HashMap<String, ServersImportConfig>,
    opts: &ImportOptions,
) -> Result<HashMap<String, SkipReason>> {
    let existing = ExistingIndex::build(db_pool).await?;
    let mut conflicts = HashMap::new();

    for (name, cfg) in items {
        let candidate = prepare_import_candidate(cfg)?;
        if let Some(reason) = import_conflict_reason(&existing, name, &candidate, opts) {
            conflicts.insert(name.clone(), reason);
        }
    }

    Ok(conflicts)
}

fn build_imported_server(
    name: String,
    cfg: &ServersImportConfig,
    args: Vec<String>,
    env: HashMap<String, String>,
    server_type: &str,
) -> ImportedServer {
    ImportedServer {
        name,
        command: cfg.command.clone(),
        args,
        env,
        server_type: server_type.to_string(),
        url: cfg.url.clone(),
    }
}

fn build_transport_draft(
    server_type: ServerType,
    cfg: &ServersImportConfig,
    args: Vec<String>,
    env: HashMap<String, String>,
) -> ServerTransportDraft {
    match server_type {
        ServerType::Stdio => ServerTransportDraft::Stdio {
            command: cfg.command.clone(),
            args,
            env: env.into_iter().map(|(key, value)| (key, config_value(value))).collect(),
        },
        ServerType::Sse => ServerTransportDraft::Http {
            protocol: HttpTransportKind::Sse,
            endpoint: cfg.url.clone(),
            headers: cfg
                .headers
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|(key, value)| (key, config_value(value)))
                .collect(),
        },
        ServerType::StreamableHttp => ServerTransportDraft::Http {
            protocol: HttpTransportKind::StreamableHttp,
            endpoint: cfg.url.clone(),
            headers: cfg
                .headers
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|(key, value)| (key, config_value(value)))
                .collect(),
        },
    }
}

fn config_value(value: String) -> ConfigValue {
    match value
        .strip_prefix("[[secret:")
        .and_then(|value| value.strip_suffix("]]"))
        .filter(|alias| !alias.is_empty())
    {
        Some(alias) => ConfigValue::SecretRef {
            alias: alias.to_string(),
        },
        None => ConfigValue::Literal { value },
    }
}

fn is_mcpmate_import_entry(entry: &InspectedServerEntry) -> bool {
    entry.name.eq_ignore_ascii_case(profile_keys::MCPMATE)
}

pub(crate) fn build_import_plan_from_entries(
    entries: impl IntoIterator<Item = InspectedServerEntry>,
    client_identifier: &str,
) -> ClientImportPlan {
    let mut items = HashMap::new();
    let mut skipped_servers = Vec::new();
    for entry in entries {
        if is_mcpmate_import_entry(&entry) {
            continue;
        }

        match import_config_from_inspected_entry(entry, client_identifier) {
            Ok((name, config)) => {
                items.insert(name, config);
            }
            Err(skipped) => skipped_servers.push(skipped),
        }
    }

    ClientImportPlan { items, skipped_servers }
}

fn import_config_from_inspected_entry(
    entry: InspectedServerEntry,
    client_identifier: &str,
) -> std::result::Result<(String, ServersImportConfig), SkippedServer> {
    let (kind, command, url) = match entry.resolved_import_transport() {
        Ok(target) => (
            target.kind.to_string(),
            target.command.map(str::to_string),
            target.url.map(str::to_string),
        ),
        Err(reason) => {
            return Err(SkippedServer {
                name: entry.name,
                reason: reason.into(),
            });
        }
    };

    let InspectedServerEntry {
        name,
        args,
        env,
        headers,
        ..
    } = entry;
    let headers = if headers.is_empty() { None } else { Some(headers) };

    Ok((
        name,
        ServersImportConfig {
            kind,
            command,
            args: Some(args),
            url,
            env: Some(env),
            headers,
            source: Some(ServerSource::new(
                ServerSourceType::Local,
                Some(client_identifier.to_string()),
            )),
            meta: None,
        },
    ))
}

pub async fn plan_import_from_client_inspection(
    service: &ClientConfigService,
    identifier: &str,
    config_path_override: Option<&str>,
    parse_rule: Option<&ClientConfigFileParse>,
    selected_server_names: &[String],
) -> Result<ClientImportPlan> {
    let trimmed_override = config_path_override.map(str::trim).filter(|path| !path.is_empty());
    let inspected = if let Some(path) = trimmed_override {
        let state = service
            .fetch_state(identifier)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Client '{}' not found", identifier))?;
        service
            .inspect_config_path_for_import(&state, path, parse_rule)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
    } else {
        service
            .inspect_current_config_for_import(identifier)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
    };

    let selected: HashSet<String> = selected_server_names
        .iter()
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect();

    let entries: Vec<InspectedServerEntry> = inspected
        .inspection
        .entries
        .into_iter()
        .filter(|entry| selected.is_empty() || selected.contains(&entry.name.trim().to_ascii_lowercase()))
        .collect();

    Ok(build_import_plan_from_entries(entries, identifier))
}

/// Import a batch of servers with consistent deduplication and capability sync.
/// - `items`: map of server name -> ServersImportConfig (kind/command/url/args/env)
pub async fn import_batch(
    database: Arc<Database>,
    connection_pool: Option<&Arc<tokio::sync::Mutex<UpstreamConnectionPool>>>,
    items: HashMap<String, ServersImportConfig>,
    opts: ImportOptions,
) -> Result<ImportOutcome> {
    let db_pool = &database.pool;
    let mut outcome = ImportOutcome::default();
    let mut pending_discoveries = Vec::new();
    tracing::info!(
        target: "mcpmate::config::server::import",
        count = items.len(),
        preview = %opts.preview,
        "Starting server import batch"
    );
    let existing = ExistingIndex::build(db_pool).await?;

    for (name, cfg) in items.into_iter() {
        if let Err(error) = crate::config::server::validate_server_namespace(&name) {
            outcome.failed.insert(name, error.to_string());
            continue;
        }
        let candidate = match prepare_import_candidate(&cfg) {
            Ok(candidate) => candidate,
            Err(error) => {
                outcome.failed.insert(name, error.to_string());
                continue;
            }
        };

        // Normalize args/env once for both preview and apply.
        let (args_norm, env_norm) = normalize_args_env(
            cfg.args.clone().unwrap_or_default(),
            cfg.env.clone().unwrap_or_default(),
        );
        let transport = build_transport_draft(candidate.server_type, &cfg, args_norm.clone(), env_norm.clone());
        if let Err(diagnostics) = transport.validate() {
            outcome
                .failed
                .insert(name, format!("server transport draft is invalid: {diagnostics:?}"));
            continue;
        }

        if let Some(reason) = import_conflict_reason(&existing, &name, &candidate, &opts) {
            if record_conflict(&mut outcome, &name, reason, opts.conflict_policy) {
                continue;
            }
        }

        // Preview: report would-be imported without DB side-effects
        if opts.preview {
            outcome.imported.push(build_imported_server(
                name,
                &cfg,
                args_norm,
                env_norm,
                candidate.persisted_kind,
            ));
            continue;
        }

        // Apply: persist the typed definition and its legacy projections atomically.
        let mut server = match candidate.server_type {
            ServerType::Stdio => Server::new_stdio(name.clone(), cfg.command.clone()),
            ServerType::Sse => Server::new_sse(name.clone(), cfg.url.clone()),
            ServerType::StreamableHttp => Server::new_streamable_http(name.clone(), cfg.url.clone()),
        };
        server.id = existing.ids_by_name.get(&name).cloned();
        server.source = cfg.source.clone();
        // Persist transport_type consistent with server_type to aid validation/preview paths
        // (DB accepts lowercase client-format values per Type/Encode implementation)
        // Stdio/Sse/StreamableHttp map 1:1 here via Server::new_* constructors; keep as-is.

        let server_id = upsert_server_definition(db_pool, &server, &transport)
            .await
            .with_context(|| format!("Failed to upsert server definition '{}'", name))?;

        if let Some(meta_payload) = cfg.meta.as_ref() {
            if let Err(err) = upsert_import_meta(db_pool, &server_id, meta_payload).await {
                tracing::warn!(
                    target: "mcpmate::config::server::import",
                    server_id = %server_id,
                    server_name = %name,
                    error = %err,
                    "Failed to persist metadata for imported server"
                );
            }
        }

        // Update resolver cache (id <-> name) so capability service can map server_id to server_name immediately
        crate::core::capability::resolver::upsert(&server_id, &name).await;

        if connection_pool.is_some() {
            pending_discoveries.push((server_id.clone(), name.clone()));
        }

        outcome.imported.push(build_imported_server(
            name,
            &cfg,
            args_norm,
            env_norm,
            candidate.persisted_kind,
        ));
    }

    if !opts.preview
        && !pending_discoveries.is_empty()
        && let Some(connection_pool) = connection_pool
    {
        let sync_result = {
            let mut pool = connection_pool.lock().await;
            pool.sync_servers_from_active_profile().await
        };
        if let Err(error) = sync_result {
            let reason = format!("Failed to synchronize the production pool after server import: {error}");
            tracing::error!(
                target: "mcpmate::config::server::import",
                error = %error,
                "Imported servers were persisted, but production pool synchronization failed"
            );
            outcome.runtime_sync_error = Some(reason);
        } else {
            for (sid, sname) in pending_discoveries {
                let cp = connection_pool.clone();
                let database = database.clone();
                tokio::spawn(async move {
                    let _permit = import_discovery_permits()
                        .acquire_owned()
                        .await
                        .expect("import discovery semaphore is never closed");
                    tracing::info!(
                        target: "mcpmate::config::server::import",
                        server_id = %sid,
                        server_name = %sname,
                        "Starting coordinated capability discovery"
                    );

                    let service = CapabilityReadService::from_runtime(database, cp);
                    match service.list_all_kinds(&sid, None).await {
                        Ok(lists) if !lists.has_failures() => {
                            tracing::info!(
                                target: "mcpmate::config::server::import",
                                server_id = %sid,
                                server_name = %sname,
                                "Coordinated capability discovery finished"
                            );
                        }
                        Ok(_) => {
                            tracing::warn!(
                                target: "mcpmate::config::server::import",
                                server_id = %sid,
                                server_name = %sname,
                                "Coordinated capability discovery completed with one or more failed kinds"
                            );
                        }
                        Err(error) => {
                            tracing::warn!(
                                target: "mcpmate::config::server::import",
                                server_id = %sid,
                                server_name = %sname,
                                error = %error,
                                "Coordinated capability discovery failed"
                            );
                        }
                    }
                });
            }
            outcome.scheduled = true;
        }
    }

    Ok(outcome)
}

pub(crate) async fn upsert_import_meta(
    db_pool: &Pool<Sqlite>,
    server_id: &str,
    payload: &ServerMetaPayload,
) -> Result<()> {
    let meta = server_meta_from_payload(server_id, payload)?;

    server_ops::upsert_server_meta(db_pool, &meta)
        .await
        .context("Failed to persist server metadata during import")?;

    Ok(())
}

pub(crate) fn server_meta_from_payload(
    server_id: &str,
    payload: &ServerMetaPayload,
) -> Result<ServerMeta> {
    let mut meta = ServerMeta::new(server_id.to_owned());
    meta.description = payload.description.clone();
    meta.website = payload.website_url.clone();
    meta.registry_version = payload.version.clone();
    meta.repository = payload
        .repository
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .context("Failed to serialize repository metadata for import")?;
    meta.registry_meta_json = payload
        .meta
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .context("Failed to serialize registry meta block for import")?;
    meta.extras_json = payload
        .extras
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .context("Failed to serialize extras metadata for import")?;
    meta.icons_json = payload
        .icons
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .context("Failed to serialize server icons for import")?;

    Ok(meta)
}

pub(crate) fn normalize_args_env(
    args: Vec<String>,
    env: std::collections::HashMap<String, String>,
) -> (Vec<String>, std::collections::HashMap<String, String>) {
    let mut env_map = env;
    let mut filtered_args = Vec::with_capacity(args.len());
    for a in args.into_iter() {
        if let Some((k, v)) = parse_env_assignment(&a).or_else(|| parse_env_assignment_fallback(&a)) {
            env_map.entry(k).or_insert(v);
        } else {
            filtered_args.push(a);
        }
    }
    (filtered_args, env_map)
}

// Less strict fallback for assignments like KEY="VALUE" with spaces trimmed
fn parse_env_assignment_fallback(s: &str) -> Option<(String, String)> {
    if s.starts_with('-') {
        return None;
    }
    let eq = s.find('=')?;
    let (k, v) = s.split_at(eq);
    if k.is_empty() {
        return None;
    }
    let mut value = v[1..].trim().to_string();
    if ((value.starts_with('"') && value.ends_with('"')) || (value.starts_with('\'') && value.ends_with('\'')))
        && value.len() >= 2
    {
        value = value[1..value.len() - 1].to_string();
    }
    Some((k.to_string(), value))
}

// Strict env assignment parser: KEY=VALUE with KEY matching [A-Za-z_][A-Za-z0-9_]* and not starting with '-'
fn parse_env_assignment(s: &str) -> Option<(String, String)> {
    if s.starts_with('-') {
        return None;
    }
    let eq = s.find('=')?;
    let (k, v) = s.split_at(eq);
    if k.is_empty() {
        return None;
    }
    let mut chars = k.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => (),
        _ => return None,
    };
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let mut value = v[1..].trim().to_string();
    if ((value.starts_with('"') && value.ends_with('"')) || (value.starts_with('\'') && value.ends_with('\'')))
        && value.len() >= 2
    {
        value = value[1..value.len() - 1].to_string();
    }
    Some((k.to_string(), value))
}

// ========================= Helpers =========================

#[derive(Debug)]
struct ExistingIndex {
    names: HashSet<String>,
    ids_by_name: HashMap<String, String>,
    fingerprints: HashSet<String>,
    url_bases: HashSet<String>,
    url_signatures: HashMap<String, fingerprint::UrlSignature>,
}

impl ExistingIndex {
    async fn build(db: &Pool<Sqlite>) -> Result<Self> {
        let mut names = HashSet::new();
        let mut ids_by_name = HashMap::new();
        let mut fps = HashSet::new();
        let mut url_bases = HashSet::new();
        let mut url_sigs = HashMap::new();
        let servers = get_all_servers(db).await?;
        for s in servers {
            names.insert(s.name.clone());
            if let Some(id) = s.id.as_ref() {
                ids_by_name.insert(s.name.clone(), id.clone());
            }
            let args_list = match (s.server_type, s.id.as_ref()) {
                (ServerType::Stdio, Some(id)) => args::get_server_args(db, id)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|a| a.arg_value)
                    .collect(),
                _ => Vec::new(),
            };
            let dedup = fingerprint::server_dedup_fingerprint(
                s.server_type,
                s.command.as_deref(),
                s.url.as_deref(),
                &args_list,
            );
            fps.insert(dedup.value);
            if let Some(sig) = dedup.url_signature {
                url_bases.insert(sig.base.clone());
                url_sigs.entry(sig.base.clone()).or_insert(sig);
            }
        }
        Ok(Self {
            names,
            ids_by_name,
            fingerprints: fps,
            url_bases,
            url_signatures: url_sigs,
        })
    }
}

fn validate_server_config(
    kind: &str,
    command: &Option<String>,
    url: &Option<String>,
) -> Result<(), &'static str> {
    match kind {
        "stdio" if command.is_none() => Err("Command is required for stdio servers"),
        "sse" | "streamable_http" if url.is_none() => Err("URL is required for HTTP-based servers"),
        "stdio" | "sse" | "streamable_http" => Ok(()),
        _ => Err("Invalid server type"),
    }
}

// Fingerprint helpers for stdio servers live in fingerprint.rs

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        common::server::ServerType,
        config::{
            models::{ConfigValue, HttpTransportKind, Server, ServerTransportDraft},
            server::{
                get_server, get_server_args, get_server_env, get_server_headers, get_server_transport_draft,
                upsert_server_definition,
            },
        },
        core::models::Config,
    };
    use std::collections::BTreeMap;
    use tokio::sync::Mutex;

    fn server_entry(
        name: &str,
        transport: &str,
        command: Option<&str>,
        url: Option<&str>,
        issue: Option<&str>,
    ) -> InspectedServerEntry {
        InspectedServerEntry {
            name: name.to_string(),
            transport: transport.to_string(),
            command: command.map(str::to_string),
            args: Vec::new(),
            env: HashMap::new(),
            headers: HashMap::new(),
            url: url.map(str::to_string),
            issue: issue.map(str::to_string),
        }
    }

    async fn seed_replaceable_stdio_definition(pool: &Pool<Sqlite>) -> String {
        let server = Server::new_stdio("replaceable".to_string(), Some("uvx".to_string()));
        upsert_server_definition(
            pool,
            &server,
            &ServerTransportDraft::Stdio {
                command: Some("uvx".to_string()),
                args: vec!["--from".to_string()],
                env: BTreeMap::from([(
                    "SERVICE_TOKEN".to_string(),
                    ConfigValue::Literal {
                        value: "from-args".to_string(),
                    },
                )]),
            },
        )
        .await
        .expect("seed stdio definition")
    }

    #[test]
    fn client_config_import_plan_filters_out_mcpmate_self_entry() {
        let plan = build_import_plan_from_entries(
            [
                server_entry("MCPMate", "stdio", Some("mcpmate-bridge"), None, None),
                server_entry(
                    "context7",
                    "streamable_http",
                    None,
                    Some("http://127.0.0.1:8123/mcp"),
                    None,
                ),
                server_entry("shadcn-mcp-server", "unclassified", None, None, None),
            ],
            "test-client",
        );

        assert!(!plan.items.contains_key("MCPMate"));
        let context7 = plan.items.get("context7").expect("context7 server entry");
        assert_eq!(context7.kind, "streamable_http");
        assert_eq!(context7.url.as_deref(), Some("http://127.0.0.1:8123/mcp"));
        assert_eq!(plan.skipped_servers.len(), 1);
        assert_eq!(plan.skipped_servers[0].name, "shadcn-mcp-server");
        assert!(matches!(plan.skipped_servers[0].reason, SkipReason::ConfigUnrecognized));
    }

    #[test]
    fn client_config_import_plan_reports_invalid_entries() {
        let plan = build_import_plan_from_entries(
            [
                server_entry("broken", "unclassified", None, None, Some("config_invalid_entry")),
                server_entry("valid", "stdio", Some("uvx"), None, None),
            ],
            "test-client",
        );

        assert!(plan.items.contains_key("valid"));
        assert_eq!(plan.skipped_servers.len(), 1);
        assert_eq!(plan.skipped_servers[0].name, "broken");
        assert!(matches!(plan.skipped_servers[0].reason, SkipReason::ConfigInvalidEntry));
    }

    #[tokio::test]
    async fn import_batch_reports_non_canonical_namespace_without_writes() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        let connection_pool = Arc::new(Mutex::new(UpstreamConnectionPool::new(
            Arc::new(Config::default()),
            None,
        )));
        let items = HashMap::from([(
            "Sequential Thinking-v2".to_string(),
            ServersImportConfig {
                kind: "stdio".to_string(),
                command: Some("server-command".to_string()),
                args: None,
                url: None,
                env: None,
                headers: None,
                source: None,
                meta: None,
            },
        )]);

        let outcome = import_batch(
            Arc::new(Database {
                pool: pool.clone(),
                path: std::path::PathBuf::new(),
                capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
            }),
            Some(&connection_pool),
            items,
            ImportOptions::dashboard_import(true),
        )
        .await
        .expect("preview import");

        assert!(outcome.imported.is_empty());
        assert_eq!(outcome.failed.len(), 1);
        assert!(outcome.failed["Sequential Thinking-v2"].contains("Suggested namespace: 'sequential_thinking_v2'"));
        assert!(get_all_servers(&pool).await.expect("load servers").is_empty());
    }

    #[tokio::test]
    async fn import_batch_reports_runtime_sync_failure_after_persistence() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        let database = Arc::new(Database {
            pool: pool.clone(),
            path: std::path::PathBuf::new(),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        });
        let connection_pool = Arc::new(Mutex::new(UpstreamConnectionPool::new(
            Arc::new(Config::default()),
            None,
        )));
        let items = HashMap::from([(
            "docs".to_string(),
            ServersImportConfig {
                kind: "stdio".to_string(),
                command: Some("server-command".to_string()),
                args: None,
                url: None,
                env: None,
                headers: None,
                source: None,
                meta: None,
            },
        )]);

        let outcome = import_batch(
            database,
            Some(&connection_pool),
            items,
            ImportOptions::dashboard_import(false),
        )
        .await
        .expect("persisted import returns its runtime convergence status");

        assert_eq!(outcome.imported.len(), 1);
        assert!(!outcome.scheduled);
        assert!(
            outcome
                .runtime_sync_error
                .as_deref()
                .is_some_and(|error| error.contains("Database not available for server sync"))
        );
        assert!(
            get_all_servers(&pool)
                .await
                .expect("load imported servers")
                .iter()
                .any(|server| server.name == "docs")
        );
    }

    #[tokio::test]
    async fn import_batch_does_not_mutate_profile_authoring_state() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role, is_active)
             VALUES ('profile-a', 'Profile A', '', 'shared', 'user', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let database = Arc::new(Database {
            pool: pool.clone(),
            path: std::path::PathBuf::new(),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        });
        let items = ["docs", "search"]
            .into_iter()
            .map(|name| {
                (
                    name.to_string(),
                    ServersImportConfig {
                        kind: "stdio".to_string(),
                        command: Some(format!("{name}-server")),
                        args: None,
                        url: None,
                        env: None,
                        headers: None,
                        source: None,
                        meta: None,
                    },
                )
            })
            .collect();
        let outcome = import_batch(database, None, items, ImportOptions::dashboard_import(false))
            .await
            .unwrap();

        assert_eq!(outcome.imported.len(), 2);
        let generation: i64 = sqlx::query_scalar("SELECT authoring_generation FROM profile WHERE id = 'profile-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(generation, 0);
        let relationships: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM profile_server_relationships WHERE profile_id = 'profile-a'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(relationships, 0);
    }

    #[test]
    fn build_transport_draft_preserves_literal_and_secret_config_values() {
        let config = ServersImportConfig {
            kind: "stdio".to_string(),
            command: Some("uvx".to_string()),
            args: Some(vec!["--from".to_string(), "SERVICE_TOKEN=from-args".to_string()]),
            url: None,
            env: Some(HashMap::from([
                ("RUST_LOG".to_string(), "info".to_string()),
                ("API_TOKEN".to_string(), "[[secret:api-token]]".to_string()),
            ])),
            headers: None,
            source: None,
            meta: None,
        };
        let (args, env) = normalize_args_env(config.args.clone().expect("args"), config.env.clone().expect("env"));
        let ServerTransportDraft::Stdio { command, args, env } =
            build_transport_draft(ServerType::Stdio, &config, args, env)
        else {
            panic!("expected stdio draft");
        };

        assert_eq!(command.as_deref(), Some("uvx"));
        assert_eq!(args, ["--from"]);
        assert_eq!(
            env.get("RUST_LOG"),
            Some(&ConfigValue::Literal {
                value: "info".to_string()
            })
        );
        assert_eq!(
            env.get("API_TOKEN"),
            Some(&ConfigValue::SecretRef {
                alias: "api-token".to_string()
            })
        );
        assert_eq!(
            env.get("SERVICE_TOKEN"),
            Some(&ConfigValue::Literal {
                value: "from-args".to_string()
            })
        );
    }

    #[tokio::test]
    async fn import_batch_persists_typed_projection_and_continues_after_invalid_entry() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        let database = Arc::new(Database {
            pool: pool.clone(),
            path: std::path::PathBuf::new(),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        });
        let server_id = seed_replaceable_stdio_definition(&pool).await;

        let update = HashMap::from([
            (
                "replaceable".to_string(),
                ServersImportConfig {
                    kind: "streamable_http".to_string(),
                    command: None,
                    args: None,
                    url: Some("https://example.test/mcp".to_string()),
                    env: None,
                    headers: Some(HashMap::from([(
                        "X-Api-Key".to_string(),
                        "[[secret:http-token]]".to_string(),
                    )])),
                    source: None,
                    meta: None,
                },
            ),
            (
                "broken".to_string(),
                ServersImportConfig {
                    kind: "stdio".to_string(),
                    command: Some("   ".to_string()),
                    args: None,
                    url: None,
                    env: None,
                    headers: None,
                    source: None,
                    meta: None,
                },
            ),
        ]);
        let outcome = import_batch(
            database,
            None,
            update,
            ImportOptions {
                conflict_policy: ConflictPolicy::Update,
                ..ImportOptions::default()
            },
        )
        .await
        .expect("import valid entry despite invalid entry");

        assert_eq!(outcome.imported.len(), 1);
        assert!(outcome.failed["broken"].contains("stdio_command_missing"));
        let updated_server = get_server(&pool, "replaceable")
            .await
            .expect("load updated server")
            .expect("updated server exists");
        assert_eq!(updated_server.id.as_deref(), Some(server_id.as_str()));
        assert_eq!(updated_server.server_type, ServerType::StreamableHttp);
        assert_eq!(updated_server.command, None);
        assert_eq!(updated_server.url.as_deref(), Some("https://example.test/mcp"));
        let Some(ServerTransportDraft::Http {
            protocol,
            endpoint,
            headers,
        }) = get_server_transport_draft(&pool, &server_id)
            .await
            .expect("load HTTP draft")
        else {
            panic!("expected HTTP draft");
        };
        assert_eq!(protocol, HttpTransportKind::StreamableHttp);
        assert_eq!(endpoint.as_deref(), Some("https://example.test/mcp"));
        assert_eq!(
            headers.get("X-Api-Key"),
            Some(&ConfigValue::SecretRef {
                alias: "http-token".to_string()
            })
        );
        assert!(
            get_server_args(&pool, &server_id)
                .await
                .expect("load HTTP args")
                .is_empty()
        );
        assert!(
            get_server_env(&pool, &server_id)
                .await
                .expect("load HTTP env")
                .is_empty()
        );
        assert_eq!(
            get_server_headers(&pool, &server_id).await.expect("load HTTP headers"),
            HashMap::from([("x-api-key".to_string(), "[[secret:http-token]]".to_string())])
        );
    }
}
