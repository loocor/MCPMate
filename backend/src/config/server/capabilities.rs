// Server capability discovery, transactional catalog persistence, and identity projection.

use anyhow::{Context, Result};
#[cfg(test)]
use mcpmate_capability_store::CapabilityCatalog;
use mcpmate_capability_store::{
    CapabilityFailureObservation, CapabilityKind as CatalogKind, CapabilityObservation, CapabilityPayload,
    CatalogCommit, CatalogRecord, CatalogSnapshot, DeclarationState, DerivedCapabilityCache, InventoryState,
    KindFailureKind, KindObservation, SnapshotState, SqliteCapabilityCatalog,
};
use sha2::{Digest, Sha256};
use sqlx::{Pool, Sqlite, Transaction};

#[cfg(test)]
use crate::core::capability::naming::reconcile_external_identifier_additions;
use crate::core::capability::naming::{NamingKind, begin_naming_transaction, reconcile_external_identifiers};
use std::collections::{BTreeSet, HashMap};

use crate::core::capability::index::{
    CachedPromptInfo, CachedResourceInfo, CachedResourceTemplateInfo, CachedToolInfo,
};
use tokio::time::{Duration, timeout};

fn is_preview_package_runner(command: &str) -> bool {
    crate::core::transport::timeout_policy::is_package_runner(command)
}

async fn run_preview_operation<T, F>(
    operation: &str,
    operation_timeout: Option<Duration>,
    future: F,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    match operation_timeout {
        Some(duration) => timeout(duration, future)
            .await
            .map_err(|_| anyhow::anyhow!("Timed out during {operation} after {}ms", duration.as_millis()))?,
        None => future.await,
    }
}

/// Internal helpers to deduplicate discovery and application steps
mod discovery_helpers {
    use super::*;
    use rmcp::{model::ErrorCode, service::ServiceError};

    pub async fn collect_all_tools(service: &crate::core::transport::ClientService) -> Result<Vec<rmcp::model::Tool>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let result = service
                .list_tools(
                    cursor
                        .clone()
                        .map(|value| rmcp::model::PaginatedRequestParams::default().with_cursor(Some(value))),
                )
                .await
                .map_err(anyhow::Error::new)
                .map_err(|source| CapabilityInventoryDiscoveryError::new(CatalogKind::Tools, source))?;
            out.extend(result.tools);
            cursor = result.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    pub async fn collect_all_prompts(
        service: &crate::core::transport::ClientService
    ) -> Result<Vec<rmcp::model::Prompt>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let result = service
                .list_prompts(
                    cursor
                        .clone()
                        .map(|c| rmcp::model::PaginatedRequestParams::default().with_cursor(Some(c))),
                )
                .await
                .map_err(anyhow::Error::new)
                .map_err(|source| CapabilityInventoryDiscoveryError::new(CatalogKind::Prompts, source))?;
            out.extend(result.prompts);
            cursor = result.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    pub async fn collect_all_resources(
        service: &crate::core::transport::ClientService
    ) -> Result<Vec<rmcp::model::Resource>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let result = service
                .list_resources(
                    cursor
                        .clone()
                        .map(|c| rmcp::model::PaginatedRequestParams::default().with_cursor(Some(c))),
                )
                .await
                .map_err(anyhow::Error::new)
                .map_err(|source| CapabilityInventoryDiscoveryError::new(CatalogKind::Resources, source))?;
            out.extend(result.resources);
            cursor = result.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    pub async fn collect_all_resource_templates(
        service: &crate::core::transport::ClientService
    ) -> Result<ResourceTemplateDiscovery> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let result = match service
                .list_resource_templates(Some(
                    rmcp::model::PaginatedRequestParams::default().with_cursor(cursor.clone()),
                ))
                .await
            {
                Ok(result) => result,
                Err(ServiceError::McpError(error)) if error.code == ErrorCode::METHOD_NOT_FOUND => {
                    return Ok(ResourceTemplateDiscovery::Unsupported);
                }
                Err(error) => {
                    return Err(CapabilityInventoryDiscoveryError::new(
                        CatalogKind::ResourceTemplates,
                        anyhow::Error::new(error),
                    )
                    .into());
                }
            };
            out.extend(result.resource_templates);
            cursor = result.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(ResourceTemplateDiscovery::Complete(out))
    }

    pub async fn apply_snapshot(
        db_pool: &Pool<Sqlite>,
        capability_cache: &DerivedCapabilityCache,
        server_id: &str,
        server_name: &str,
        snapshot: &super::CapabilitySnapshot,
        _seed_profiles: bool,
    ) -> Result<CatalogCommit> {
        super::commit_capability_observation(
            db_pool,
            capability_cache,
            server_id,
            server_name,
            snapshot.clone(),
            crate::core::pool::CapSyncFlags::ALL,
        )
        .await
    }
}

#[derive(Debug)]
enum ResourceTemplateDiscovery {
    Complete(Vec<rmcp::model::ResourceTemplate>),
    Unsupported,
}

#[derive(Debug, thiserror::Error)]
#[error("{operation} inventory discovery failed for {kind:?}: {source}")]
pub(crate) struct CapabilityInventoryDiscoveryError {
    pub kind: CatalogKind,
    operation: &'static str,
    #[source]
    pub source: anyhow::Error,
}

impl CapabilityInventoryDiscoveryError {
    fn new(
        kind: CatalogKind,
        source: anyhow::Error,
    ) -> Self {
        let operation = match kind {
            CatalogKind::Tools => "tools/list",
            CatalogKind::Prompts => "prompts/list",
            CatalogKind::Resources => "resources/list",
            CatalogKind::ResourceTemplates => "resources/templates/list",
        };
        Self {
            kind,
            operation,
            source,
        }
    }
}

pub(crate) async fn apply_discovered_snapshot(
    db_pool: &Pool<Sqlite>,
    capability_cache: &DerivedCapabilityCache,
    server_id: &str,
    server_name: &str,
    snapshot: &CapabilitySnapshot,
    seed_profiles: bool,
) -> Result<CatalogCommit> {
    discovery_helpers::apply_snapshot(
        db_pool,
        capability_cache,
        server_id,
        server_name,
        snapshot,
        seed_profiles,
    )
    .await
}

/// Unified capability snapshot container
#[derive(Debug, Clone, Default)]
pub struct CapabilitySnapshot {
    pub tools: Vec<CachedToolInfo>,
    pub resources: Vec<CachedResourceInfo>,
    pub prompts: Vec<CachedPromptInfo>,
    pub resource_templates: Vec<CachedResourceTemplateInfo>,
    pub protocol_version: Option<String>,
    pub upstream_name: Option<String>,
    pub upstream_title: Option<String>,
    pub server_version: Option<String>,
    pub initialize: Option<rmcp::model::InitializeResult>,
    pub protocol_tools: Vec<rmcp::model::Tool>,
    pub protocol_resources: Vec<rmcp::model::Resource>,
    pub protocol_prompts: Vec<rmcp::model::Prompt>,
    pub protocol_resource_templates: Vec<rmcp::model::ResourceTemplate>,
    pub kind_states: Vec<KindObservation>,
}

impl CapabilitySnapshot {
    fn set_tools(
        &mut self,
        tools: Vec<rmcp::model::Tool>,
    ) {
        self.tools = tools.iter().map(cached_tool_from_protocol).collect();
        self.protocol_tools = tools;
    }

    fn set_prompts(
        &mut self,
        prompts: Vec<rmcp::model::Prompt>,
    ) {
        self.prompts = prompts.iter().map(cached_prompt_from_protocol).collect();
        self.protocol_prompts = prompts;
    }

    fn set_resources(
        &mut self,
        resources: Vec<rmcp::model::Resource>,
    ) {
        self.resources = resources.iter().map(cached_resource_from_protocol).collect();
        self.protocol_resources = resources;
    }

    fn set_resource_templates(
        &mut self,
        templates: Vec<rmcp::model::ResourceTemplate>,
    ) {
        self.resource_templates = templates.iter().map(cached_resource_template_from_protocol).collect();
        self.protocol_resource_templates = templates;
    }

    fn ensure_protocol_payloads(&mut self) {
        if self.protocol_tools.is_empty() && !self.tools.is_empty() {
            self.protocol_tools = self.tools.iter().filter_map(protocol_tool_from_cached).collect();
        }
        if self.protocol_prompts.is_empty() && !self.prompts.is_empty() {
            self.protocol_prompts = self.prompts.iter().map(protocol_prompt_from_cached).collect();
        }
        if self.protocol_resources.is_empty() && !self.resources.is_empty() {
            self.protocol_resources = self.resources.iter().map(protocol_resource_from_cached).collect();
        }
        if self.protocol_resource_templates.is_empty() && !self.resource_templates.is_empty() {
            self.protocol_resource_templates = self
                .resource_templates
                .iter()
                .map(protocol_resource_template_from_cached)
                .collect();
        }
    }
}

fn cached_tool_from_protocol(tool: &rmcp::model::Tool) -> CachedToolInfo {
    CachedToolInfo {
        name: tool.name.to_string(),
        description: tool.description.clone().map(|value| value.into_owned()),
        input_schema_json: serde_json::to_string(&tool.schema_as_json_value()).unwrap_or_else(|_| "{}".to_string()),
        output_schema_json: tool.output_schema.as_ref().map(|schema| {
            serde_json::to_string(&serde_json::Value::Object((**schema).clone())).unwrap_or_else(|_| "{}".to_string())
        }),
        unique_name: None,
        icons: tool.icons.clone(),
        enabled: true,
        cached_at: chrono::Utc::now(),
    }
}

fn cached_prompt_from_protocol(prompt: &rmcp::model::Prompt) -> CachedPromptInfo {
    CachedPromptInfo {
        name: prompt.name.clone(),
        description: prompt.description.clone(),
        arguments: prompt
            .arguments
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|argument| crate::core::capability::index::PromptArgument {
                name: argument.name,
                description: argument.description,
                required: argument.required.unwrap_or(false),
            })
            .collect(),
        icons: prompt.icons.clone(),
        enabled: true,
        cached_at: chrono::Utc::now(),
    }
}

fn cached_resource_from_protocol(resource: &rmcp::model::Resource) -> CachedResourceInfo {
    CachedResourceInfo {
        uri: resource.uri.clone(),
        name: Some(resource.name.clone()),
        description: resource.description.clone(),
        mime_type: resource.mime_type.clone(),
        icons: resource.icons.clone(),
        enabled: true,
        cached_at: chrono::Utc::now(),
    }
}

fn cached_resource_template_from_protocol(template: &rmcp::model::ResourceTemplate) -> CachedResourceTemplateInfo {
    CachedResourceTemplateInfo {
        uri_template: template.uri_template.clone(),
        name: Some(template.name.clone()),
        description: template.description.clone(),
        mime_type: template.mime_type.clone(),
        enabled: true,
        cached_at: chrono::Utc::now(),
    }
}

fn protocol_tool_from_cached(tool: &CachedToolInfo) -> Option<rmcp::model::Tool> {
    let input_schema = serde_json::from_str::<serde_json::Value>(&tool.input_schema_json).ok()?;
    let output_schema = tool
        .output_schema_json
        .as_deref()
        .map(serde_json::from_str::<serde_json::Value>)
        .transpose()
        .ok()?;
    serde_json::from_value(serde_json::json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": input_schema,
        "outputSchema": output_schema,
        "icons": tool.icons,
    }))
    .ok()
}

fn protocol_prompt_from_cached(prompt: &CachedPromptInfo) -> rmcp::model::Prompt {
    serde_json::from_value(serde_json::json!({
        "name": prompt.name,
        "description": prompt.description,
        "arguments": prompt.arguments.iter().map(|argument| serde_json::json!({
            "name": argument.name,
            "description": argument.description,
            "required": argument.required,
        })).collect::<Vec<_>>(),
        "icons": prompt.icons,
    }))
    .expect("cached prompt projection must be valid RMCP payload")
}

fn protocol_resource_from_cached(resource: &CachedResourceInfo) -> rmcp::model::Resource {
    serde_json::from_value(serde_json::json!({
        "uri": resource.uri,
        "name": resource.name.as_deref().unwrap_or(&resource.uri),
        "description": resource.description,
        "mimeType": resource.mime_type,
        "icons": resource.icons,
    }))
    .expect("cached resource projection must be valid RMCP payload")
}

fn protocol_resource_template_from_cached(template: &CachedResourceTemplateInfo) -> rmcp::model::ResourceTemplate {
    serde_json::from_value(serde_json::json!({
        "uriTemplate": template.uri_template,
        "name": template.name.as_deref().unwrap_or(&template.uri_template),
        "description": template.description,
        "mimeType": template.mime_type,
    }))
    .expect("cached resource template projection must be valid RMCP payload")
}

pub async fn persist_snapshot_server_info(
    pool: &Pool<Sqlite>,
    server_id: &str,
    snapshot: &CapabilitySnapshot,
) -> Result<()> {
    let (Some(upstream_name), Some(protocol_version)) =
        (snapshot.upstream_name.clone(), snapshot.protocol_version.clone())
    else {
        return Ok(());
    };
    crate::config::server::meta::update_server_info(
        pool,
        server_id,
        upstream_name,
        snapshot.upstream_title.clone(),
        snapshot.server_version.clone(),
        protocol_version,
    )
    .await
}

/// Discover capabilities from an existing upstream connection (API temporary instance)
pub async fn discover_from_connection(conn: &crate::core::pool::UpstreamConnection) -> Result<CapabilitySnapshot> {
    let service = conn
        .service
        .as_ref()
        .context("Connected instance has no capability peer")?;
    let tools = if conn
        .capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.tools.as_ref())
        .is_some()
    {
        discovery_helpers::collect_all_tools(service).await?
    } else {
        Vec::new()
    };
    discover_from_service(service, tools, conn.capabilities.clone()).await
}

pub async fn discover_from_service(
    service: &crate::core::transport::ClientService,
    tools: Vec<rmcp::model::Tool>,
    capabilities: Option<rmcp::model::ServerCapabilities>,
) -> Result<CapabilitySnapshot> {
    let peer_info = service.peer_info();
    let initialize = crate::core::transport::client::legacy_initialize_result(peer_info.as_deref())?;
    let mut snap = CapabilitySnapshot {
        protocol_version: initialize.as_ref().map(|info| info.protocol_version.to_string()),
        upstream_name: initialize.as_ref().map(|info| info.server_info.name.clone()),
        upstream_title: initialize.as_ref().and_then(|info| info.server_info.title.clone()),
        server_version: initialize.as_ref().map(|info| info.server_info.version.clone()),
        initialize,
        ..Default::default()
    };

    snap.set_tools(tools);

    // Prompts (paginate defensively)
    if capabilities.as_ref().and_then(|value| value.prompts.as_ref()).is_some() {
        let items = discovery_helpers::collect_all_prompts(service).await?;
        snap.set_prompts(items);
    }

    // Resources and templates (paginate fully)
    if capabilities
        .as_ref()
        .and_then(|value| value.resources.as_ref())
        .is_some()
    {
        let resources = discovery_helpers::collect_all_resources(service).await?;
        let templates = discovery_helpers::collect_all_resource_templates(service).await?;
        snap.set_resources(resources);
        match templates {
            ResourceTemplateDiscovery::Complete(templates) => snap.set_resource_templates(templates),
            ResourceTemplateDiscovery::Unsupported => {
                snap.set_resource_templates(Vec::new());
                snap.kind_states
                    .push(unsupported_complete_observation(CatalogKind::ResourceTemplates));
            }
        }
    }

    Ok(snap)
}

/// Discover capabilities by connecting with the given server config (used by migration)
pub async fn discover_from_config(
    server_name: &str,
    server_config: &crate::core::models::MCPServerConfig,
    server_type: crate::common::server::ServerType,
) -> Result<CapabilitySnapshot> {
    use crate::core::transport::{TransportType, connect_http_server, connect_server_simple};

    let (service, tools, capabilities, _pid) = match server_type {
        crate::common::server::ServerType::Stdio => {
            connect_server_simple(server_name, server_config, server_type, TransportType::Stdio).await?
        }
        crate::common::server::ServerType::Sse | crate::common::server::ServerType::StreamableHttp => {
            connect_http_server(server_name, server_config, TransportType::StreamableHttp)
                .await
                .map(|(s, t, c)| (s, t, c, None))?
        }
    };

    let peer_info = service.peer_info();
    let initialize = crate::core::transport::client::legacy_initialize_result(peer_info.as_deref())?;
    let mut snap = CapabilitySnapshot {
        protocol_version: initialize.as_ref().map(|info| info.protocol_version.to_string()),
        upstream_name: initialize.as_ref().map(|info| info.server_info.name.clone()),
        upstream_title: initialize.as_ref().and_then(|info| info.server_info.title.clone()),
        server_version: initialize.as_ref().map(|info| info.server_info.version.clone()),
        initialize,
        ..Default::default()
    };

    snap.set_tools(tools);

    // Prompts (paginate defensively)
    if capabilities.as_ref().and_then(|c| c.prompts.as_ref()).is_some() {
        let items = discovery_helpers::collect_all_prompts(&service).await?;
        snap.set_prompts(items);
    }

    // Resources & templates (paginate fully)
    if capabilities.as_ref().and_then(|c| c.resources.as_ref()).is_some() {
        let resources = discovery_helpers::collect_all_resources(&service).await?;
        let templates = discovery_helpers::collect_all_resource_templates(&service).await?;
        snap.set_resources(resources);
        match templates {
            ResourceTemplateDiscovery::Complete(templates) => snap.set_resource_templates(templates),
            ResourceTemplateDiscovery::Unsupported => {
                snap.set_resource_templates(Vec::new());
                snap.kind_states
                    .push(unsupported_complete_observation(CatalogKind::ResourceTemplates));
            }
        }
    }

    Ok(snap)
}

async fn preview_snapshot_from_service(
    service: &crate::core::transport::ClientService,
    tools: Vec<rmcp::model::Tool>,
    capabilities: Option<rmcp::model::ServerCapabilities>,
    operation_timeout: Option<std::time::Duration>,
) -> Result<CapabilitySnapshot> {
    let peer_info = service.peer_info();
    let initialize = crate::core::transport::client::legacy_initialize_result(peer_info.as_deref())?;
    let mut snap = CapabilitySnapshot {
        protocol_version: initialize.as_ref().map(|info| info.protocol_version.to_string()),
        upstream_name: initialize.as_ref().map(|info| info.server_info.name.clone()),
        upstream_title: initialize.as_ref().and_then(|info| info.server_info.title.clone()),
        server_version: initialize.as_ref().map(|info| info.server_info.version.clone()),
        initialize,
        ..Default::default()
    };
    snap.set_tools(tools);
    if capabilities.as_ref().and_then(|value| value.prompts.as_ref()).is_some() {
        let items = run_preview_operation(
            "prompts/list",
            operation_timeout,
            discovery_helpers::collect_all_prompts(service),
        )
        .await?;
        snap.set_prompts(items);
    }
    if capabilities
        .as_ref()
        .and_then(|value| value.resources.as_ref())
        .is_some()
    {
        let resources = run_preview_operation(
            "resources/list",
            operation_timeout,
            discovery_helpers::collect_all_resources(service),
        )
        .await?;
        let templates = run_preview_operation(
            "resources/templates/list",
            operation_timeout,
            discovery_helpers::collect_all_resource_templates(service),
        )
        .await?;
        snap.set_resources(resources);
        match templates {
            ResourceTemplateDiscovery::Complete(templates) => snap.set_resource_templates(templates),
            ResourceTemplateDiscovery::Unsupported => {
                snap.set_resource_templates(Vec::new());
                snap.kind_states
                    .push(unsupported_complete_observation(CatalogKind::ResourceTemplates));
            }
        }
    }
    Ok(snap)
}

pub(crate) async fn discover_from_preview_connection(
    connection: &crate::core::pool::UpstreamConnection,
    operation_timeout: Option<std::time::Duration>,
) -> Result<CapabilitySnapshot> {
    let service = connection
        .service
        .as_deref()
        .context("Preview owner has no capability peer")?;
    let tools = if connection
        .capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.tools.as_ref())
        .is_some()
    {
        run_preview_operation(
            "tools/list",
            operation_timeout,
            discovery_helpers::collect_all_tools(service),
        )
        .await?
    } else {
        Vec::new()
    };
    preview_snapshot_from_service(service, tools, connection.capabilities.clone(), operation_timeout).await
}

/// Connect and initialize a preview owner for short-term reuse.
pub(crate) async fn connect_preview_owner(
    server_name: &str,
    server_config: &crate::core::models::MCPServerConfig,
    server_type: crate::common::server::ServerType,
    http_client: Option<reqwest::Client>,
    operation_timeout: Option<std::time::Duration>,
) -> Result<(
    crate::core::pool::UpstreamConnection,
    Option<tokio_util::sync::CancellationToken>,
)> {
    use crate::core::transport::{
        TransportType, connect_http_server_with_client_timeouts, stdio::connect_stdio_server_with_timeouts,
    };
    use tokio_util::sync::CancellationToken;

    let timeout_policy = crate::core::transport::timeout_policy::McpTimeoutPolicy::for_server(
        server_type,
        server_config.command.as_deref(),
        operation_timeout,
    );
    let (service, tools, capabilities, pid, cancellation) = match server_type {
        crate::common::server::ServerType::Stdio => {
            let command = server_config.command.as_deref().unwrap_or_default();
            let cancellation = CancellationToken::new();
            let result = connect_stdio_server_with_timeouts(
                server_name,
                server_config,
                cancellation.clone(),
                None,
                timeout_policy.startup,
                timeout_policy.capability_operation,
            )
            .await;

            if is_preview_package_runner(command) {
                let (service, tools, capabilities, pid) = result.with_context(|| {
                    format!(
                        "Package runner preview startup failed for '{server_name}' after allowing up to {}s",
                        timeout_policy.startup.as_secs()
                    )
                })?;
                (service, tools, capabilities, pid, Some(cancellation))
            } else {
                let (service, tools, capabilities, pid) = result?;
                (service, tools, capabilities, pid, Some(cancellation))
            }
        }
        crate::common::server::ServerType::Sse | crate::common::server::ServerType::StreamableHttp => {
            let client = http_client.unwrap_or_default();
            let (service, tools, capabilities) = connect_http_server_with_client_timeouts(
                server_name,
                server_config,
                client,
                TransportType::StreamableHttp,
                timeout_policy.startup,
                timeout_policy.capability_operation,
            )
            .await?;
            (service, tools, capabilities, None, None)
        }
    };

    let mut connection = crate::core::pool::UpstreamConnection::new(server_name.to_string());
    connection.process_id = pid;
    connection.config_fingerprint = server_config.source_fingerprint.clone();
    connection.runtime_fingerprint = Some(crate::config::server::fingerprint::materialized_runtime_fingerprint(
        server_config,
    )?);
    connection.update_connected(service, tools, capabilities);
    Ok((connection, cancellation))
}

/// Discover capabilities with optional custom HTTP client and timeouts (used by migration callers).
pub async fn discover_from_config_preview(
    server_name: &str,
    server_config: &crate::core::models::MCPServerConfig,
    server_type: crate::common::server::ServerType,
    http_client: Option<reqwest::Client>,
    operation_timeout: Option<std::time::Duration>,
) -> Result<CapabilitySnapshot> {
    let (connection, cancellation) =
        connect_preview_owner(server_name, server_config, server_type, http_client, operation_timeout).await?;
    let snapshot = discover_from_preview_connection(&connection, operation_timeout).await;
    crate::core::pool::UpstreamConnectionPool::discard_startup_result(Some(connection), cancellation).await;
    snapshot
}

/// Upsert an exact upstream prompt and its external identifier.
async fn upsert_shadow_prompt_row(
    tx: &mut Transaction<'_, Sqlite>,
    server_id: &str,
    server_name: &str,
    prompt_name: &str,
    unique_name: &str,
    description: Option<&str>,
) -> Result<()> {
    let id = crate::generate_id!("sprm");
    sqlx::query(
        r#"
        INSERT INTO server_prompts (id, server_id, server_name, prompt_name, unique_name, description)
        VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT(server_id, prompt_name) DO UPDATE SET
            server_name = excluded.server_name,
            unique_name = excluded.unique_name,
            description = excluded.description,
            updated_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(&id)
    .bind(server_id)
    .bind(server_name)
    .bind(prompt_name)
    .bind(unique_name)
    .bind(description)
    .execute(&mut **tx)
    .await
    .context("Failed to upsert shadow prompt")?;
    Ok(())
}

/// Test-only convenience wrapper that upserts a single shadow prompt row outside the
/// observation-commit transaction. Production code must go through
/// `apply_snapshot_catalog_in_transaction` (via `upsert_shadow_prompts_batch_in_transaction`)
/// so the shadow index never drifts from the committed catalog snapshot.
#[cfg(test)]
pub(crate) async fn upsert_shadow_prompt(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    prompt_name: &str,
    description: Option<&str>,
) -> Result<String> {
    let mut tx = begin_naming_transaction(pool)
        .await
        .context("Failed to begin shadow prompt update")?;
    let reconciliation = reconcile_external_identifier_additions(
        &mut tx,
        NamingKind::Prompt,
        server_id,
        server_name,
        &[prompt_name.to_string()],
    )
    .await?;
    let unique_name = reconciliation.identifier_for(prompt_name)?.to_string();
    upsert_shadow_prompt_row(&mut tx, server_id, server_name, prompt_name, &unique_name, description).await?;
    let catalog_changed = reconciliation.catalog_changed();
    tx.commit().await.context("Failed to commit shadow prompt update")?;
    if catalog_changed {
        crate::core::events::EventBus::global().publish(crate::core::events::Event::CapabilityCatalogChanged {
            server_id: server_id.to_string(),
            server_name: server_name.to_string(),
        });
    }
    Ok(unique_name)
}

/// Upsert an exact upstream resource URI and its external identifier.
async fn upsert_shadow_resource_row(
    tx: &mut Transaction<'_, Sqlite>,
    server_id: &str,
    server_name: &str,
    uri: &str,
    unique_uri: &str,
    name: Option<&str>,
    description: Option<&str>,
    mime_type: Option<&str>,
) -> Result<()> {
    let id = crate::generate_id!("sres");
    sqlx::query(
        r#"
        INSERT INTO server_resources (id, server_id, server_name, resource_uri, unique_uri, name, description, mime_type)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(server_id, resource_uri) DO UPDATE SET
            server_name = excluded.server_name,
            unique_uri = excluded.unique_uri,
            name = excluded.name,
            description = excluded.description,
            mime_type = excluded.mime_type,
            updated_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(&id)
    .bind(server_id)
    .bind(server_name)
    .bind(uri)
    .bind(unique_uri)
    .bind(name)
    .bind(description)
    .bind(mime_type)
    .execute(&mut **tx)
    .await
    .context("Failed to upsert shadow resource")?;
    Ok(())
}

/// Test-only convenience wrapper that upserts a single shadow resource row outside the
/// observation-commit transaction. Production code must go through
/// `apply_snapshot_catalog_in_transaction` (via `upsert_shadow_resources_batch_in_transaction`)
/// so the shadow index never drifts from the committed catalog snapshot.
#[cfg(test)]
pub(crate) async fn upsert_shadow_resource(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    uri: &str,
    name: Option<&str>,
    description: Option<&str>,
    mime_type: Option<&str>,
) -> Result<String> {
    let mut tx = begin_naming_transaction(pool)
        .await
        .context("Failed to begin shadow resource update")?;
    let reconciliation = reconcile_external_identifier_additions(
        &mut tx,
        NamingKind::Resource,
        server_id,
        server_name,
        &[uri.to_string()],
    )
    .await?;
    let unique_uri = reconciliation.identifier_for(uri)?.to_string();
    upsert_shadow_resource_row(
        &mut tx,
        server_id,
        server_name,
        uri,
        &unique_uri,
        name,
        description,
        mime_type,
    )
    .await?;
    let catalog_changed = reconciliation.catalog_changed();
    tx.commit().await.context("Failed to commit shadow resource update")?;
    if catalog_changed {
        crate::core::events::EventBus::global().publish(crate::core::events::Event::CapabilityCatalogChanged {
            server_id: server_id.to_string(),
            server_name: server_name.to_string(),
        });
    }
    Ok(unique_uri)
}

/// Upsert an exact upstream resource template and its external identifier.
async fn upsert_shadow_resource_template_row(
    tx: &mut Transaction<'_, Sqlite>,
    server_id: &str,
    server_name: &str,
    uri_template: &str,
    unique_name: &str,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<()> {
    let id = crate::generate_id!("srst");
    let route_uri = crate::core::capability::resource_uri::template_match_key(unique_name)?;
    sqlx::query(
        r#"
        INSERT INTO server_resource_templates (id, server_id, server_name, uri_template, unique_name, route_uri, name, description)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(server_id, uri_template) DO UPDATE SET
            server_name = excluded.server_name,
            unique_name = excluded.unique_name,
            route_uri = excluded.route_uri,
            name = excluded.name,
            description = excluded.description,
            updated_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(&id)
    .bind(server_id)
    .bind(server_name)
    .bind(uri_template)
    .bind(unique_name)
    .bind(route_uri)
    .bind(name)
    .bind(description)
    .execute(&mut **tx)
    .await
    .context("Failed to upsert shadow resource template")?;
    Ok(())
}

/// Test-only convenience wrapper that upserts a single shadow resource template row outside
/// the observation-commit transaction. Production code must go through
/// `apply_snapshot_catalog_in_transaction` (via
/// `upsert_shadow_resource_templates_batch_in_transaction`) so the shadow index never drifts
/// from the committed catalog snapshot.
#[cfg(test)]
pub(crate) async fn upsert_shadow_resource_template(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    uri_template: &str,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<String> {
    let mut tx = begin_naming_transaction(pool)
        .await
        .context("Failed to begin shadow resource template update")?;
    let reconciliation = reconcile_external_identifier_additions(
        &mut tx,
        NamingKind::ResourceTemplate,
        server_id,
        server_name,
        &[uri_template.to_string()],
    )
    .await?;
    let unique_name = reconciliation.identifier_for(uri_template)?.to_string();
    upsert_shadow_resource_template_row(
        &mut tx,
        server_id,
        server_name,
        uri_template,
        &unique_name,
        name,
        description,
    )
    .await?;
    let catalog_changed = reconciliation.catalog_changed();
    tx.commit()
        .await
        .context("Failed to commit shadow resource template update")?;
    if catalog_changed {
        crate::core::events::EventBus::global().publish(crate::core::events::Event::CapabilityCatalogChanged {
            server_id: server_id.to_string(),
            server_name: server_name.to_string(),
        });
    }
    Ok(unique_name)
}

pub(crate) async fn upsert_shadow_prompts_batch_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    server_id: &str,
    server_name: &str,
    prompts: &[CachedPromptInfo],
) -> Result<bool> {
    let inventory = prompts.iter().map(|prompt| prompt.name.clone()).collect::<Vec<_>>();
    let reconciliation =
        reconcile_external_identifiers(tx, NamingKind::Prompt, server_id, server_name, &inventory).await?;
    for prompt in prompts {
        upsert_shadow_prompt_row(
            tx,
            server_id,
            server_name,
            &prompt.name,
            reconciliation.identifier_for(&prompt.name)?,
            prompt.description.as_deref(),
        )
        .await?;
    }
    let catalog_changed = reconciliation.catalog_changed();
    Ok(catalog_changed)
}

#[cfg(test)]
pub(crate) async fn upsert_shadow_resources_batch(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    resources: &[CachedResourceInfo],
) -> Result<bool> {
    let mut tx = begin_naming_transaction(pool)
        .await
        .context("Failed to begin shadow resource batch")?;
    let catalog_changed =
        upsert_shadow_resources_batch_in_transaction(&mut tx, server_id, server_name, resources).await?;
    tx.commit().await.context("Failed to commit shadow resource batch")?;
    Ok(catalog_changed)
}

pub(crate) async fn upsert_shadow_resources_batch_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    server_id: &str,
    server_name: &str,
    resources: &[CachedResourceInfo],
) -> Result<bool> {
    let inventory = resources
        .iter()
        .map(|resource| resource.uri.clone())
        .collect::<Vec<_>>();
    let reconciliation =
        reconcile_external_identifiers(tx, NamingKind::Resource, server_id, server_name, &inventory).await?;
    for resource in resources {
        upsert_shadow_resource_row(
            tx,
            server_id,
            server_name,
            &resource.uri,
            reconciliation.identifier_for(&resource.uri)?,
            resource.name.as_deref(),
            resource.description.as_deref(),
            resource.mime_type.as_deref(),
        )
        .await?;
    }
    let catalog_changed = reconciliation.catalog_changed();
    Ok(catalog_changed)
}

pub(crate) async fn upsert_shadow_resource_templates_batch_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    server_id: &str,
    server_name: &str,
    templates: &[CachedResourceTemplateInfo],
) -> Result<bool> {
    let mut projectable_templates = Vec::new();
    for template in templates {
        if crate::core::capability::resource_uri::resource_template_is_projectable(server_name, &template.uri_template)?
        {
            projectable_templates.push(template);
        } else {
            tracing::warn!(
                server_id,
                server_name,
                upstream_template = %template.uri_template,
                "Resource Template remains available upstream but cannot enter the canonical address space"
            );
        }
    }
    let inventory = projectable_templates
        .iter()
        .map(|template| template.uri_template.clone())
        .collect::<Vec<_>>();
    let reconciliation =
        reconcile_external_identifiers(tx, NamingKind::ResourceTemplate, server_id, server_name, &inventory).await?;
    for template in projectable_templates {
        upsert_shadow_resource_template_row(
            tx,
            server_id,
            server_name,
            &template.uri_template,
            reconciliation.identifier_for(&template.uri_template)?,
            template.name.as_deref(),
            template.description.as_deref(),
        )
        .await?;
    }
    let catalog_changed = reconciliation.catalog_changed();
    Ok(catalog_changed)
}

/// Fixed, non-inventory-driven initialize result for test-only fixtures that do not care
/// about declaration semantics. Declaring every kind as `Supported` unconditionally (rather
/// than inferring support from whether a list happens to be empty) keeps `store_dual_write`
/// from resurrecting the "empty list == unsupported" inference bug in test fixtures.
#[cfg(test)]
fn dual_write_fixture_initialize(protocol_version: Option<&str>) -> rmcp::model::InitializeResult {
    let protocol_version = protocol_version.unwrap_or(rmcp::model::ProtocolVersion::LATEST.as_str());
    serde_json::from_value(serde_json::json!({
        "protocolVersion": protocol_version,
        "capabilities": {"tools": {}, "prompts": {}, "resources": {}},
        "serverInfo": {"name": "dual-write-fixture", "version": "test"}
    }))
    .expect("dual-write fixture initialize must always decode")
}

/// Test adapter that persists derived index DTOs through the SQLite catalog.
#[cfg(test)]
pub async fn store_dual_write(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    tools: Vec<CachedToolInfo>,
    resources: Vec<CachedResourceInfo>,
    prompts: Vec<CachedPromptInfo>,
    templates: Vec<CachedResourceTemplateInfo>,
    protocol_version: Option<String>,
) -> Result<()> {
    let initialize = dual_write_fixture_initialize(protocol_version.as_deref());
    let mut snapshot = CapabilitySnapshot {
        tools,
        resources,
        prompts,
        resource_templates: templates,
        protocol_version,
        initialize: Some(initialize),
        ..Default::default()
    };
    snapshot.ensure_protocol_payloads();
    commit_capability_observation(
        pool,
        &DerivedCapabilityCache::default(),
        server_id,
        server_name,
        snapshot,
        crate::core::pool::CapSyncFlags::ALL,
    )
    .await
    .map(|_| ())
}

pub(crate) async fn apply_snapshot_catalog_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    server_id: &str,
    server_name: &str,
    snapshot: &mut CapabilitySnapshot,
) -> Result<bool> {
    let mut catalog_changed = crate::config::server::tools::assign_unique_names_to_cached_tools_in_transaction(
        tx,
        server_id,
        server_name,
        &mut snapshot.tools,
    )
    .await?;
    catalog_changed |=
        upsert_shadow_prompts_batch_in_transaction(tx, server_id, server_name, &snapshot.prompts).await?;
    catalog_changed |=
        upsert_shadow_resources_batch_in_transaction(tx, server_id, server_name, &snapshot.resources).await?;
    catalog_changed |=
        upsert_shadow_resource_templates_batch_in_transaction(tx, server_id, server_name, &snapshot.resource_templates)
            .await?;
    Ok(catalog_changed)
}

fn snapshot_from_catalog(snapshot: CatalogSnapshot) -> Result<CapabilitySnapshot> {
    let mut result = CapabilitySnapshot::default();
    if let Some(initialize) = snapshot.initialize {
        result.protocol_version = Some(initialize.protocol_version.to_string());
        result.upstream_name = Some(initialize.server_info.name.clone());
        result.upstream_title = initialize.server_info.title.clone();
        result.server_version = Some(initialize.server_info.version.clone());
        result.initialize = Some(initialize);
    }
    for record in snapshot.records {
        match record.source_payload {
            CapabilityPayload::Tool(tool) => {
                let mut cached = cached_tool_from_protocol(&tool);
                cached.unique_name = Some(record.external_key);
                result.tools.push(cached);
                result.protocol_tools.push(tool);
            }
            CapabilityPayload::Prompt(prompt) => result.protocol_prompts.push(prompt),
            CapabilityPayload::Resource(resource) => result.protocol_resources.push(resource),
            CapabilityPayload::ResourceTemplate(template) => result.protocol_resource_templates.push(template),
        }
    }
    result.prompts = result
        .protocol_prompts
        .iter()
        .map(cached_prompt_from_protocol)
        .collect();
    result.resources = result
        .protocol_resources
        .iter()
        .map(cached_resource_from_protocol)
        .collect();
    result.resource_templates = result
        .protocol_resource_templates
        .iter()
        .map(cached_resource_template_from_protocol)
        .collect();
    Ok(result)
}

fn shadow_table_and_column(kind: CatalogKind) -> (&'static str, &'static str) {
    match kind {
        CatalogKind::Tools => ("server_tools", "tool_name"),
        CatalogKind::Prompts => ("server_prompts", "prompt_name"),
        CatalogKind::Resources => ("server_resources", "resource_uri"),
        CatalogKind::ResourceTemplates => ("server_resource_templates", "uri_template"),
    }
}

/// External-key prefix `catalog_records_in_transaction` assigns to a Resource Template that
/// cannot enter the canonical shadow-index address space (see
/// `resource_template_is_projectable`). These records are intentionally never written to
/// `server_resource_templates`, so the shadow-index integrity check must not expect them there.
const UNPROJECTABLE_TEMPLATE_EXTERNAL_PREFIX: &str = "internal://capability/";

/// Compares one kind's catalog records against its shadow index table by upstream key. The
/// shadow index is derived data committed atomically with the catalog snapshot; if the two
/// disagree (e.g. a prior bug wrote one without the other), the persisted snapshot can no
/// longer be trusted for cache-first serving and the caller must treat it the same as an
/// invalidated baseline rather than silently serving a possibly-inconsistent projection.
pub(crate) async fn shadow_index_matches_catalog_kind(
    pool: &Pool<Sqlite>,
    server_id: &str,
    kind: CatalogKind,
    records: &[CatalogRecord],
) -> Result<bool> {
    let catalog_keys: BTreeSet<&str> = records
        .iter()
        .filter(|record| {
            record.kind() == kind && !record.external_key.starts_with(UNPROJECTABLE_TEMPLATE_EXTERNAL_PREFIX)
        })
        .map(|record| record.upstream_key.as_str())
        .collect();
    let (table, column) = shadow_table_and_column(kind);
    let shadow_keys: Vec<String> = sqlx::query_scalar(&format!("SELECT {column} FROM {table} WHERE server_id = ?"))
        .bind(server_id)
        .fetch_all(pool)
        .await
        .with_context(|| format!("Failed to load {table} shadow index for the catalog integrity check"))?;
    let shadow_keys: BTreeSet<&str> = shadow_keys.iter().map(String::as_str).collect();
    Ok(catalog_keys == shadow_keys)
}

fn declaration_for_kind(
    initialize: &rmcp::model::InitializeResult,
    kind: CatalogKind,
) -> DeclarationState {
    let supported = match kind {
        CatalogKind::Tools => initialize.capabilities.tools.is_some(),
        CatalogKind::Prompts => initialize.capabilities.prompts.is_some(),
        CatalogKind::Resources | CatalogKind::ResourceTemplates => initialize.capabilities.resources.is_some(),
    };
    if supported {
        DeclarationState::Supported
    } else {
        DeclarationState::Unsupported
    }
}

fn merge_selected_kinds(
    existing: Option<CatalogSnapshot>,
    mut discovered: CapabilitySnapshot,
    kinds: crate::core::pool::CapSyncFlags,
    server_name: &str,
    rebuilding_untrusted_catalog: bool,
) -> Result<(CapabilitySnapshot, Vec<KindObservation>)> {
    discovered.ensure_protocol_payloads();
    let discovered_states = discovered.kind_states.clone();
    // Keep historical payloads for unselected kinds whenever a prior snapshot still
    // exists. Corrupt replacement already deleted the prior rows, so there is nothing
    // to retain there. Ready baselines also keep prior Complete kind states; non-Ready
    // baselines force Unknown for unselected kinds so they are not re-exposed until
    // re-listed, while shadow indexes and Profile associations survive reconcile.
    let retain_history =
        existing.is_some() && !rebuilding_untrusted_catalog && kinds != crate::core::pool::CapSyncFlags::ALL;
    let merge_from_ready = existing
        .as_ref()
        .is_some_and(|snapshot| snapshot.state == SnapshotState::Ready && retain_history);
    let previous_states = existing
        .as_ref()
        .filter(|_| retain_history)
        .map(|snapshot| snapshot.kind_states.clone())
        .unwrap_or_default();
    let mut merged = match existing.filter(|_| retain_history) {
        Some(snapshot) => snapshot_from_catalog(snapshot)?,
        None => CapabilitySnapshot::default(),
    };

    if kinds.contains(crate::core::pool::CapSyncFlags::TOOLS) {
        merged.set_tools(discovered.protocol_tools);
    }
    if kinds.contains(crate::core::pool::CapSyncFlags::PROMPTS) {
        merged.set_prompts(discovered.protocol_prompts);
    }
    if kinds.contains(crate::core::pool::CapSyncFlags::RESOURCES) {
        merged.set_resources(discovered.protocol_resources);
    }
    if kinds.contains(crate::core::pool::CapSyncFlags::RESOURCE_TEMPLATES) {
        merged.set_resource_templates(discovered.protocol_resource_templates);
    }
    merged.initialize = discovered.initialize.or(merged.initialize);
    merged.protocol_version = discovered.protocol_version.or(merged.protocol_version);
    merged.upstream_name = discovered.upstream_name.or(merged.upstream_name);
    merged.upstream_title = discovered.upstream_title.or(merged.upstream_title);
    merged.server_version = discovered.server_version.or(merged.server_version);
    let initialize = merged.initialize.clone().with_context(|| {
        format!(
            "Capability discovery for server '{server_name}' did not provide a protocol initialize result; \
             refusing to fabricate a declaration from inventory size"
        )
    })?;

    let states = CatalogKind::ALL
        .into_iter()
        .map(|kind| {
            let selected = match kind {
                CatalogKind::Tools => kinds.contains(crate::core::pool::CapSyncFlags::TOOLS),
                CatalogKind::Prompts => kinds.contains(crate::core::pool::CapSyncFlags::PROMPTS),
                CatalogKind::Resources => kinds.contains(crate::core::pool::CapSyncFlags::RESOURCES),
                CatalogKind::ResourceTemplates => kinds.contains(crate::core::pool::CapSyncFlags::RESOURCE_TEMPLATES),
            };
            if selected {
                discovered_states
                    .iter()
                    .find(|state| state.kind == kind)
                    .cloned()
                    .unwrap_or_else(|| {
                        KindObservation::new(kind, declaration_for_kind(&initialize, kind), InventoryState::Complete)
                    })
            } else if merge_from_ready {
                previous_states
                    .iter()
                    .find(|state| state.kind == kind)
                    .cloned()
                    .unwrap_or_else(|| {
                        KindObservation::new(kind, declaration_for_kind(&initialize, kind), InventoryState::Unknown)
                    })
            } else {
                KindObservation::new(kind, declaration_for_kind(&initialize, kind), InventoryState::Unknown)
            }
        })
        .collect();
    Ok((merged, states))
}

async fn config_fingerprint_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    server_id: &str,
) -> mcpmate_capability_store::Result<String> {
    let draft_json: Option<String> = sqlx::query_scalar("SELECT draft_json FROM server_transport WHERE server_id = ?")
        .bind(server_id)
        .fetch_optional(&mut **tx)
        .await?;
    let draft_json = draft_json.ok_or_else(|| mcpmate_capability_store::CatalogError::InvalidValue {
        field: "server_transport.draft_json",
        value: format!("persisted ServerTransportDraft is missing for server '{server_id}'"),
    })?;
    let draft: crate::config::models::ServerTransportDraft =
        serde_json::from_str(&draft_json).map_err(|error| mcpmate_capability_store::CatalogError::InvalidValue {
            field: "server_transport.draft_json",
            value: format!("persisted ServerTransportDraft could not be decoded for server '{server_id}': {error}"),
        })?;
    let transport = draft
        .validate()
        .map_err(|diagnostics| mcpmate_capability_store::CatalogError::InvalidValue {
            field: "server_transport.draft_json",
            value: format!("persisted ServerTransportDraft is invalid for server '{server_id}': {diagnostics:?}"),
        })?;
    Ok(super::fingerprint::validated_transport_fingerprint(&transport)?)
}

pub async fn current_config_fingerprint(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<String> {
    let mut transaction = pool.begin().await?;
    let fingerprint = config_fingerprint_in_transaction(&mut transaction, server_id).await?;
    transaction.commit().await?;
    Ok(fingerprint)
}

/// Reads the previous fingerprint when the persisted transport is valid.
///
/// An invalid historical draft has no trustworthy baseline. Callers repairing it
/// should treat that as a configuration change, while runtime consumers keep
/// using `current_config_fingerprint` and remain fail-closed.
pub async fn previous_valid_config_fingerprint(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<Option<String>> {
    match current_config_fingerprint(pool, server_id).await {
        Ok(fingerprint) => Ok(Some(fingerprint)),
        Err(error)
            if matches!(
                error.downcast_ref::<mcpmate_capability_store::CatalogError>(),
                Some(mcpmate_capability_store::CatalogError::InvalidValue { .. })
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

async fn persist_server_info_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    server_id: &str,
    server_name: &str,
    initialize: &rmcp::model::InitializeResult,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO server_meta (
            id, server_id, server_name, upstream_name, upstream_title, server_version, protocol_version
        ) VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(server_id) DO UPDATE SET
            server_name = excluded.server_name,
            upstream_name = excluded.upstream_name,
            upstream_title = excluded.upstream_title,
            server_version = excluded.server_version,
            protocol_version = excluded.protocol_version,
            updated_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(crate::generate_id!("smet"))
    .bind(server_id)
    .bind(server_name)
    .bind(&initialize.server_info.name)
    .bind(&initialize.server_info.title)
    .bind(&initialize.server_info.version)
    .bind(initialize.protocol_version.to_string())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn catalog_records_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    snapshot: &CapabilitySnapshot,
    server_id: &str,
) -> Result<Vec<CatalogRecord>> {
    let tool_identity = sqlx::query_as::<_, (String, String, String)>(
        "SELECT tool_name, id, unique_name FROM server_tools WHERE server_id = ?",
    )
    .bind(server_id)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|(key, id, external)| (key, (id, external)))
    .collect::<HashMap<_, _>>();
    let prompt_identity = sqlx::query_as::<_, (String, String, String)>(
        "SELECT prompt_name, id, unique_name FROM server_prompts WHERE server_id = ?",
    )
    .bind(server_id)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|(key, id, external)| (key, (id, external)))
    .collect::<HashMap<_, _>>();
    let resource_identity = sqlx::query_as::<_, (String, String, String)>(
        "SELECT resource_uri, id, unique_uri FROM server_resources WHERE server_id = ?",
    )
    .bind(server_id)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|(key, id, external)| (key, (id, external)))
    .collect::<HashMap<_, _>>();
    let template_identity = sqlx::query_as::<_, (String, String, String)>(
        "SELECT uri_template, id, unique_name FROM server_resource_templates WHERE server_id = ?",
    )
    .bind(server_id)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|(key, id, external)| (key, (id, external)))
    .collect::<HashMap<_, _>>();
    let mut records = Vec::new();
    for tool in &snapshot.protocol_tools {
        let (_, external) = tool_identity
            .get(tool.name.as_ref())
            .with_context(|| format!("Missing Tool identity for '{}'", tool.name))?;
        records.push(CatalogRecord::materialize(
            server_id,
            tool.name.as_ref(),
            external,
            CapabilityPayload::Tool(tool.clone()),
        )?);
    }
    for prompt in &snapshot.protocol_prompts {
        let (_, external) = prompt_identity
            .get(&prompt.name)
            .with_context(|| format!("Missing Prompt identity for '{}'", prompt.name))?;
        records.push(CatalogRecord::materialize(
            server_id,
            &prompt.name,
            external,
            CapabilityPayload::Prompt(prompt.clone()),
        )?);
    }
    for resource in &snapshot.protocol_resources {
        let (_, external) = resource_identity
            .get(&resource.uri)
            .with_context(|| format!("Missing Resource identity for '{}'", resource.uri))?;
        records.push(CatalogRecord::materialize(
            server_id,
            &resource.uri,
            external,
            CapabilityPayload::Resource(resource.clone()),
        )?);
    }
    for template in &snapshot.protocol_resource_templates {
        if let Some((_, external)) = template_identity.get(&template.uri_template) {
            records.push(CatalogRecord::materialize(
                server_id,
                &template.uri_template,
                external,
                CapabilityPayload::ResourceTemplate(template.clone()),
            )?);
        } else {
            let digest = format!("{:x}", Sha256::digest(format!("{server_id}:{}", template.uri_template)));
            records.push(CatalogRecord::materialize(
                server_id,
                &template.uri_template,
                format!("internal://capability/{server_id}/resource-template/{digest}"),
                CapabilityPayload::ResourceTemplate(template.clone()),
            )?);
        }
    }
    Ok(records)
}

async fn commit_snapshot_for_kinds_checked(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    snapshot: CapabilitySnapshot,
    kinds: crate::core::pool::CapSyncFlags,
    expected_config_fingerprint: Option<&str>,
) -> Result<CatalogCommit> {
    let catalog = SqliteCapabilityCatalog::new(pool.clone());
    #[cfg(test)]
    catalog.ensure_schema().await?;
    let mut tx = begin_naming_transaction(pool)
        .await
        .context("Failed to begin transactional capability catalog update")?;
    let config_fingerprint = config_fingerprint_in_transaction(&mut tx, server_id).await?;
    if let Some(expected) = expected_config_fingerprint
        && config_fingerprint != expected
    {
        return Err(CapabilityConfigurationChanged {
            server_id: server_id.to_string(),
            expected: expected.to_string(),
            actual: config_fingerprint,
        }
        .into());
    }
    let (existing, rebuilding_untrusted_catalog, previous_revision) =
        match catalog.load_snapshot_in_transaction(&mut tx, server_id).await {
            Ok(existing) => (existing, false, None),
            Err(
                mcpmate_capability_store::CatalogError::Json(_)
                | mcpmate_capability_store::CatalogError::UnsupportedRecordVersion { .. }
                | mcpmate_capability_store::CatalogError::InvalidValue { .. }
                | mcpmate_capability_store::CatalogError::InvalidTimestamp { .. },
            ) => {
                let previous_revision = catalog
                    .load_revision_in_transaction(&mut tx, server_id)
                    .await?
                    .context("Corrupt capability snapshot disappeared before replacement")?;
                catalog.remove_server_in_transaction(&mut tx, server_id).await?;
                (None, true, Some(previous_revision))
            }
            Err(error) => return Err(error.into()),
        };
    let (mut merged, states) =
        merge_selected_kinds(existing, snapshot, kinds, server_name, rebuilding_untrusted_catalog)?;
    apply_snapshot_catalog_in_transaction(&mut tx, server_id, server_name, &mut merged).await?;
    let initialize = merged
        .initialize
        .clone()
        .context("Capability discovery did not provide initialize data")?;
    persist_server_info_in_transaction(&mut tx, server_id, server_name, &initialize).await?;
    let records = catalog_records_in_transaction(&mut tx, &merged, server_id).await?;
    let observed_kinds = CatalogKind::ALL.into_iter().filter(|kind| match kind {
        CatalogKind::Tools => kinds.contains(crate::core::pool::CapSyncFlags::TOOLS),
        CatalogKind::Prompts => kinds.contains(crate::core::pool::CapSyncFlags::PROMPTS),
        CatalogKind::Resources => kinds.contains(crate::core::pool::CapSyncFlags::RESOURCES),
        CatalogKind::ResourceTemplates => kinds.contains(crate::core::pool::CapSyncFlags::RESOURCE_TEMPLATES),
    });
    let observation =
        CapabilityObservation::new(server_id, server_name, config_fingerprint, initialize, states, records)
            .with_observed_kinds(observed_kinds);
    let reconciliation = crate::core::capability::reconciliation::CatalogSurfaceReconciler::new(pool.clone())
        .reconcile_after_revision_in_transaction(
            &mut tx,
            observation,
            previous_revision,
            crate::core::capability::reconciliation::ReconciliationFault::None,
        )
        .await?;
    tx.commit()
        .await
        .context("Failed to commit capability catalog transaction")?;
    Ok(reconciliation.commit)
}

pub(crate) async fn commit_snapshot_for_kinds(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    snapshot: CapabilitySnapshot,
    kinds: crate::core::pool::CapSyncFlags,
) -> Result<CatalogCommit> {
    commit_snapshot_for_kinds_checked(pool, server_id, server_name, snapshot, kinds, None).await
}

#[derive(Clone, Debug, thiserror::Error)]
#[error(
    "server configuration changed during capability discovery for '{server_id}': expected {expected}, found {actual}"
)]
pub(crate) struct CapabilityConfigurationChanged {
    pub server_id: String,
    pub expected: String,
    pub actual: String,
}

#[derive(Clone, Debug)]
pub(crate) struct CapabilityFailureEvidence {
    pub server_id: String,
    pub kind: CatalogKind,
    pub instance_id: Option<String>,
    pub connection_generation: Option<u64>,
    pub failure_kind: Option<KindFailureKind>,
    pub timeout_ms: Option<u64>,
    pub reason: String,
}

pub(crate) struct CapabilityProtocolObservation {
    pub initialize: Option<rmcp::model::InitializeResult>,
    pub tools: Vec<rmcp::model::Tool>,
    pub resources: Vec<rmcp::model::Resource>,
    pub prompts: Vec<rmcp::model::Prompt>,
    pub templates: Vec<rmcp::model::ResourceTemplate>,
    pub kinds: crate::core::pool::CapSyncFlags,
    pub kind_states: Vec<KindObservation>,
}

fn catalog_kind_name(kind: CatalogKind) -> &'static str {
    match kind {
        CatalogKind::Tools => "tools",
        CatalogKind::Prompts => "prompts",
        CatalogKind::Resources => "resources",
        CatalogKind::ResourceTemplates => "resource_templates",
    }
}

pub(crate) fn unsupported_complete_observation(kind: CatalogKind) -> KindObservation {
    KindObservation::new(kind, DeclarationState::Unsupported, InventoryState::Complete)
}

pub(crate) async fn commit_capability_observation(
    pool: &Pool<Sqlite>,
    cache: &DerivedCapabilityCache,
    server_id: &str,
    server_name: &str,
    snapshot: CapabilitySnapshot,
    kinds: crate::core::pool::CapSyncFlags,
) -> Result<CatalogCommit> {
    let commit = commit_snapshot_for_kinds(pool, server_id, server_name, snapshot, kinds).await?;
    if commit.changed {
        cache.invalidate_server(server_id).await;
        publish_catalog_commit(server_id, server_name, commit.revision);
    }
    Ok(commit)
}

pub(crate) async fn record_capability_failure(
    pool: &Pool<Sqlite>,
    cache: &DerivedCapabilityCache,
    evidence: CapabilityFailureEvidence,
) -> mcpmate_capability_store::Result<CatalogCommit> {
    let kind = evidence.kind;
    record_capability_failures(pool, cache, &[kind], evidence).await
}

pub(crate) async fn record_capability_failures(
    pool: &Pool<Sqlite>,
    cache: &DerivedCapabilityCache,
    kinds: &[CatalogKind],
    evidence: CapabilityFailureEvidence,
) -> mcpmate_capability_store::Result<CatalogCommit> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let server_name = sqlx::query_scalar::<_, String>("SELECT name FROM server_config WHERE id = ?")
        .bind(&evidence.server_id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or_else(|| mcpmate_capability_store::CatalogError::ServerNotFound {
            server_id: evidence.server_id.clone(),
        })?;
    let config_fingerprint = config_fingerprint_in_transaction(&mut transaction, &evidence.server_id).await?;
    let kind_names = kinds
        .iter()
        .map(|kind| catalog_kind_name(*kind))
        .collect::<Vec<_>>()
        .join(",");
    let reason = format!(
        "server_id={} server_name={} kinds=[{}] instance={:?} generation={:?} reason={}",
        evidence.server_id,
        server_name,
        kind_names,
        evidence.instance_id,
        evidence.connection_generation,
        evidence.reason
    );
    let mut observation = CapabilityFailureObservation::for_kinds(
        &evidence.server_id,
        &server_name,
        config_fingerprint,
        kinds.iter().copied(),
        reason,
    );
    if let Some(failure_kind) = evidence.failure_kind {
        observation = observation.with_failure(failure_kind, evidence.timeout_ms);
    }
    let commit = SqliteCapabilityCatalog::new(pool.clone())
        .record_failure_in_transaction(&mut transaction, observation)
        .await?;
    transaction.commit().await?;
    if commit.changed {
        cache.invalidate_server(&evidence.server_id).await;
        publish_catalog_commit(&evidence.server_id, &server_name, commit.revision);
    }
    Ok(commit)
}

pub(crate) fn publish_catalog_commit(
    server_id: &str,
    server_name: &str,
    revision: i64,
) {
    crate::core::events::EventBus::global().publish(crate::core::events::Event::CapabilityCatalogCommitted {
        server_id: server_id.to_string(),
        server_name: server_name.to_string(),
        revision,
    });
    crate::core::events::EventBus::global().publish(crate::core::events::Event::CapabilityCatalogChanged {
        server_id: server_id.to_string(),
        server_name: server_name.to_string(),
    });
}

pub async fn commit_protocol_items_for_kinds(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    initialize: Option<rmcp::model::InitializeResult>,
    tools: Vec<rmcp::model::Tool>,
    resources: Vec<rmcp::model::Resource>,
    prompts: Vec<rmcp::model::Prompt>,
    templates: Vec<rmcp::model::ResourceTemplate>,
    kinds: crate::core::pool::CapSyncFlags,
) -> Result<CatalogCommit> {
    commit_protocol_observation_for_kinds(
        pool,
        server_id,
        server_name,
        initialize,
        tools,
        resources,
        prompts,
        templates,
        kinds,
        Vec::new(),
    )
    .await
}

pub async fn commit_protocol_observation_for_kinds(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    initialize: Option<rmcp::model::InitializeResult>,
    tools: Vec<rmcp::model::Tool>,
    resources: Vec<rmcp::model::Resource>,
    prompts: Vec<rmcp::model::Prompt>,
    templates: Vec<rmcp::model::ResourceTemplate>,
    kinds: crate::core::pool::CapSyncFlags,
    kind_states: Vec<KindObservation>,
) -> Result<CatalogCommit> {
    let mut snapshot = CapabilitySnapshot {
        protocol_version: initialize.as_ref().map(|result| result.protocol_version.to_string()),
        upstream_name: initialize.as_ref().map(|result| result.server_info.name.clone()),
        upstream_title: initialize.as_ref().and_then(|result| result.server_info.title.clone()),
        server_version: initialize.as_ref().map(|result| result.server_info.version.clone()),
        initialize,
        kind_states,
        ..Default::default()
    };
    snapshot.set_tools(tools);
    snapshot.set_resources(resources);
    snapshot.set_prompts(prompts);
    snapshot.set_resource_templates(templates);
    commit_snapshot_for_kinds(pool, server_id, server_name, snapshot, kinds).await
}

pub(crate) async fn commit_capability_protocol_observation(
    pool: &Pool<Sqlite>,
    cache: &DerivedCapabilityCache,
    server_id: &str,
    server_name: &str,
    expected_config_fingerprint: &str,
    observation: CapabilityProtocolObservation,
) -> Result<CatalogCommit> {
    let CapabilityProtocolObservation {
        initialize,
        tools,
        resources,
        prompts,
        templates,
        kinds,
        kind_states,
    } = observation;
    let mut snapshot = CapabilitySnapshot {
        protocol_version: initialize.as_ref().map(|result| result.protocol_version.to_string()),
        upstream_name: initialize.as_ref().map(|result| result.server_info.name.clone()),
        upstream_title: initialize.as_ref().and_then(|result| result.server_info.title.clone()),
        server_version: initialize.as_ref().map(|result| result.server_info.version.clone()),
        initialize,
        kind_states,
        ..Default::default()
    };
    snapshot.set_tools(tools);
    snapshot.set_resources(resources);
    snapshot.set_prompts(prompts);
    snapshot.set_resource_templates(templates);
    let commit = commit_snapshot_for_kinds_checked(
        pool,
        server_id,
        server_name,
        snapshot,
        kinds,
        Some(expected_config_fingerprint),
    )
    .await?;
    if commit.changed {
        cache.invalidate_server(server_id).await;
        publish_catalog_commit(server_id, server_name, commit.revision);
    }
    Ok(commit)
}

/// Test adapter for partial SQLite catalog commits.
#[cfg(test)]
pub async fn store_dual_write_for_kinds(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    tools: Vec<CachedToolInfo>,
    resources: Vec<CachedResourceInfo>,
    prompts: Vec<CachedPromptInfo>,
    templates: Vec<CachedResourceTemplateInfo>,
    protocol_version: Option<String>,
    kinds: crate::core::pool::CapSyncFlags,
) -> Result<()> {
    let mut snapshot = CapabilitySnapshot {
        tools,
        resources,
        prompts,
        resource_templates: templates,
        protocol_version,
        ..Default::default()
    };
    snapshot.ensure_protocol_payloads();
    commit_snapshot_for_kinds(pool, server_id, server_name, snapshot, kinds)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    async fn insert_typed_stdio_server(
        pool: &Pool<Sqlite>,
        server_id: &str,
        server_name: &str,
        command: &str,
    ) {
        let mut server = crate::config::models::Server::new_stdio(server_name.to_string(), Some(command.to_string()));
        server.id = Some(server_id.to_string());
        crate::config::server::upsert_server_definition(
            pool,
            &server,
            &crate::config::models::ServerTransportDraft::Stdio {
                command: Some(command.to_string()),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
        )
        .await
        .expect("insert typed server definition");
    }

    async fn capability_store_pool() -> Pool<Sqlite> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        crate::config::profile::init::initialize_profile_tables(&pool)
            .await
            .expect("initialize profile tables");
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("initialize client table");
        SqliteCapabilityCatalog::new(pool.clone())
            .ensure_schema()
            .await
            .expect("initialize capability catalog");
        insert_typed_stdio_server(&pool, "server-a", "docs", "node").await;
        pool
    }

    async fn persist_typed_streamable_http_draft(pool: &Pool<Sqlite>) {
        let draft = crate::config::models::ServerTransportDraft::Http {
            protocol: crate::config::models::HttpTransportKind::StreamableHttp,
            endpoint: Some("https://typed.example.test/mcp".to_string()),
            headers: BTreeMap::from([
                (
                    "Authorization".to_string(),
                    crate::config::models::ConfigValue::SecretRef {
                        alias: "capability-token".to_string(),
                    },
                ),
                (
                    "X-Tenant".to_string(),
                    crate::config::models::ConfigValue::Literal {
                        value: "typed".to_string(),
                    },
                ),
            ]),
        };
        let mut transaction = pool.begin().await.expect("begin typed draft transaction");
        crate::config::server::upsert_server_transport_draft_tx(&mut transaction, "server-a", &draft)
            .await
            .expect("persist typed transport draft");
        transaction.commit().await.expect("commit typed transport draft");
    }

    #[tokio::test]
    async fn current_config_fingerprint_uses_validated_typed_transport_not_legacy_projection() {
        let pool = capability_store_pool().await;
        persist_typed_streamable_http_draft(&pool).await;

        sqlx::query(
            "UPDATE server_config
             SET server_type = 'stdio', command = 'legacy-command', url = NULL, enabled = 1
             WHERE id = 'server-a'",
        )
        .execute(&pool)
        .await
        .expect("corrupt legacy server projection");
        crate::config::server::args::upsert_server_args(&pool, "server-a", &["--legacy-argument".to_string()])
            .await
            .expect("corrupt legacy arguments projection");
        crate::config::server::env::upsert_server_env(
            &pool,
            "server-a",
            &HashMap::from([("LEGACY_ENV".to_string(), "incorrect".to_string())]),
        )
        .await
        .expect("corrupt legacy environment projection");
        crate::config::server::headers::upsert_server_headers(
            &pool,
            "server-a",
            &HashMap::from([("X-Legacy".to_string(), "incorrect".to_string())]),
        )
        .await
        .expect("corrupt legacy headers projection");
        let persisted = current_config_fingerprint(&pool, "server-a")
            .await
            .expect("fingerprint persisted runtime config");

        const EXPECTED: &str = "sha256:da8bd73c163b93db045fafe3be8d80ed4671c98555d36726d1e0819b738e011b";
        assert_eq!(persisted, EXPECTED);
    }

    #[tokio::test]
    async fn current_config_fingerprint_ignores_global_enabled() {
        let pool = capability_store_pool().await;
        persist_typed_streamable_http_draft(&pool).await;

        let enabled = current_config_fingerprint(&pool, "server-a")
            .await
            .expect("fingerprint enabled server");
        sqlx::query("UPDATE server_config SET enabled = 0 WHERE id = 'server-a'")
            .execute(&pool)
            .await
            .expect("disable server globally");
        let disabled = current_config_fingerprint(&pool, "server-a")
            .await
            .expect("fingerprint disabled server");

        assert_eq!(disabled, enabled);
    }

    #[tokio::test]
    async fn current_config_fingerprint_rejects_missing_typed_transport_draft() {
        let pool = capability_store_pool().await;

        sqlx::query("DELETE FROM server_transport WHERE server_id = 'server-a'")
            .execute(&pool)
            .await
            .expect("remove typed transport draft");

        let error = current_config_fingerprint(&pool, "server-a")
            .await
            .expect_err("missing typed transport draft must fail closed");

        assert!(error.to_string().contains("ServerTransportDraft is missing"));
    }

    #[tokio::test]
    async fn current_config_fingerprint_rejects_invalid_typed_transport_draft() {
        let pool = capability_store_pool().await;
        sqlx::query("UPDATE server_transport SET draft_json = ? WHERE server_id = ?")
            .bind(r#"{"kind":"stdio","command":null,"args":[],"env":{}}"#)
            .bind("server-a")
            .execute(&pool)
            .await
            .expect("persist invalid typed transport draft");

        let error = current_config_fingerprint(&pool, "server-a")
            .await
            .expect_err("invalid typed transport draft must fail closed");

        assert!(error.to_string().contains("ServerTransportDraft is invalid"));
    }

    #[tokio::test]
    async fn previous_valid_config_fingerprint_allows_transport_repair_baselines() {
        let pool = capability_store_pool().await;

        sqlx::query("DELETE FROM server_transport WHERE server_id = 'server-a'")
            .execute(&pool)
            .await
            .expect("remove typed transport draft");
        assert_eq!(
            previous_valid_config_fingerprint(&pool, "server-a")
                .await
                .expect("missing draft is an absent baseline"),
            None
        );

        persist_typed_streamable_http_draft(&pool).await;
        sqlx::query("UPDATE server_transport SET draft_json = ? WHERE server_id = ?")
            .bind(r#"{"kind":"stdio","command":null,"args":[],"env":{}}"#)
            .bind("server-a")
            .execute(&pool)
            .await
            .expect("persist invalid typed transport draft");
        assert_eq!(
            previous_valid_config_fingerprint(&pool, "server-a")
                .await
                .expect("invalid draft is an absent baseline"),
            None
        );
    }

    fn decode<T: DeserializeOwned>(value: Value) -> T {
        serde_json::from_value(value).expect("fixture must match RMCP 2.2")
    }

    fn protocol_fixture() -> (
        rmcp::model::InitializeResult,
        rmcp::model::Tool,
        rmcp::model::Resource,
        rmcp::model::Prompt,
        rmcp::model::ResourceTemplate,
    ) {
        let initialize = decode(json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {
                "prompts": {"listChanged": true},
                "resources": {"subscribe": true, "listChanged": true},
                "tools": {"listChanged": true}
            },
            "serverInfo": {"name": "fixture-server", "title": "Fixture Server", "version": "2.2.0"},
            "instructions": "Preserve this initialize result exactly.",
            "_meta": {"fixture": "initialize"}
        }));
        let tool = decode(json!({
            "name": "analyze",
            "title": "Analyze",
            "description": "Analyze a payload",
            "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]},
            "outputSchema": {"type": "object", "properties": {"result": {"type": "string"}}},
            "annotations": {
                "title": "Safe analyzer",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            },
            "execution": {"taskSupport": "optional"},
            "icons": [{"src": "https://icons.example/tool.svg", "mimeType": "image/svg+xml", "sizes": ["any"]}],
            "_meta": {"fixture": "tool"}
        }));
        let resource = decode(json!({
            "uri": "file:///fixture/report.md",
            "name": "report",
            "title": "Fixture Report",
            "description": "A complete resource fixture",
            "mimeType": "text/markdown",
            "size": 4096,
            "icons": [{"src": "https://icons.example/resource.svg", "mimeType": "image/svg+xml"}],
            "_meta": {"fixture": "resource"},
            "annotations": {"audience": ["user", "assistant"], "priority": 0.75, "lastModified": "2026-07-20T00:00:00Z"}
        }));
        let prompt = decode(json!({
            "name": "summarize",
            "title": "Summarize",
            "description": "Summarize a document",
            "arguments": [{"name": "document", "title": "Document", "description": "Input text", "required": true}],
            "icons": [{"src": "https://icons.example/prompt.png", "mimeType": "image/png"}],
            "_meta": {"fixture": "prompt"}
        }));
        let template = decode(json!({
            "uriTemplate": "file:///fixture/{name}.md",
            "name": "fixture-template",
            "title": "Fixture Template",
            "description": "A complete template fixture",
            "mimeType": "text/markdown",
            "icons": [{"src": "https://icons.example/template.svg", "mimeType": "image/svg+xml"}],
            "_meta": {"fixture": "template"},
            "annotations": {"audience": ["assistant"], "priority": 0.5}
        }));
        (initialize, tool, resource, prompt, template)
    }

    async fn insert_active_profile(pool: &Pool<Sqlite>) {
        insert_active_profile_attached_to(pool, "profile-a", "server-a").await;
    }

    /// Inserts an active profile attached to `server_id` through a server-level authoring relationship.
    async fn insert_active_profile_attached_to(
        pool: &Pool<Sqlite>,
        profile_id: &str,
        server_id: &str,
    ) {
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, is_active, is_default, priority) VALUES (?, ?, '', 'shared', 1, 1, 0)",
        )
        .bind(profile_id)
        .bind(format!("Profile {profile_id}"))
        .execute(pool)
        .await
        .expect("insert active profile");
        sqlx::query(
            "INSERT INTO profile_server_relationships (profile_id, server_id, new_ref_policy) VALUES (?, ?, 'follow')",
        )
        .bind(profile_id)
        .bind(server_id)
        .execute(pool)
        .await
        .expect("attach server to active profile");
    }

    #[tokio::test]
    async fn protocol_observation_commits_payload_indexes_profiles_and_metadata_atomically() {
        let pool = capability_store_pool().await;
        insert_active_profile(&pool).await;
        sqlx::query("ALTER TABLE server_config ADD COLUMN capabilities TEXT")
            .execute(&pool)
            .await
            .expect("simulate a pre-C4 database");
        sqlx::query("UPDATE server_config SET capabilities = 'legacy-summary' WHERE id = 'server-a'")
            .execute(&pool)
            .await
            .expect("seed legacy summary");
        let (initialize, tool, resource, prompt, template) = protocol_fixture();

        let commit = commit_protocol_items_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(initialize.clone()),
            vec![tool.clone()],
            vec![resource.clone()],
            vec![prompt.clone()],
            vec![template.clone()],
            crate::core::pool::CapSyncFlags::ALL,
        )
        .await
        .expect("commit protocol observation");

        assert_eq!(commit.revision, 1);
        let catalog = SqliteCapabilityCatalog::new(pool.clone());
        let snapshot = catalog
            .load_snapshot("server-a")
            .await
            .expect("load catalog")
            .expect("catalog snapshot exists");
        assert_eq!(
            serde_json::to_value(snapshot.initialize.as_ref().expect("ready snapshot initialize"))
                .expect("serialize initialize"),
            serde_json::to_value(&initialize).expect("serialize expected initialize")
        );
        let payloads = snapshot
            .records
            .iter()
            .map(|record| serde_json::to_value(&record.source_payload).expect("serialize payload"))
            .collect::<Vec<_>>();
        for expected in [
            CapabilityPayload::Tool(tool),
            CapabilityPayload::Resource(resource),
            CapabilityPayload::Prompt(prompt),
            CapabilityPayload::ResourceTemplate(template),
        ] {
            assert!(payloads.contains(&serde_json::to_value(expected).expect("serialize fixture payload")));
        }

        for table in [
            "server_tools",
            "server_prompts",
            "server_resources",
            "server_resource_templates",
        ] {
            let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap_or_else(|error| panic!("count {table}: {error}"));
            assert_eq!(count, 1, "{table} must be committed with the snapshot");
        }
        let profile_intent_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM profile_capability_refs WHERE profile_id = 'profile-a'")
                .fetch_one(&pool)
                .await
                .expect("count profile capability intent");
        assert_eq!(
            profile_intent_count, 0,
            "inventory observation must not write capability-level authoring intent"
        );
        let server_meta: (String, String, String) = sqlx::query_as(
            "SELECT upstream_name, server_version, protocol_version FROM server_meta WHERE server_id = 'server-a'",
        )
        .fetch_one(&pool)
        .await
        .expect("load server metadata");
        assert_eq!(
            server_meta,
            ("fixture-server".into(), "2.2.0".into(), "2025-11-25".into())
        );
        let legacy_summary: Option<String> =
            sqlx::query_scalar("SELECT capabilities FROM server_config WHERE id = 'server-a'")
                .fetch_one(&pool)
                .await
                .expect("load legacy summary");
        assert_eq!(legacy_summary.as_deref(), Some("legacy-summary"));
    }

    #[tokio::test]
    async fn protocol_observation_preserves_server_level_profile_intent_without_seeding_ref_rows() {
        let pool = capability_store_pool().await;
        insert_active_profile(&pool).await;
        let (initialize, tool, resource, prompt, template) = protocol_fixture();

        commit_protocol_items_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(initialize),
            vec![tool],
            vec![resource],
            vec![prompt],
            vec![template],
            crate::core::pool::CapSyncFlags::ALL,
        )
        .await
        .expect("commit protocol observation");

        let policy: String = sqlx::query_scalar(
            "SELECT new_ref_policy FROM profile_server_relationships WHERE profile_id = 'profile-a' AND server_id = 'server-a'",
        )
        .fetch_one(&pool)
        .await
        .expect("load profile server relationship");
        assert_eq!(policy, "follow");
        let capability_intent_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM profile_capability_refs WHERE profile_id = 'profile-a'")
                .fetch_one(&pool)
                .await
                .expect("count profile capability intent");
        assert_eq!(capability_intent_count, 0);
    }

    #[tokio::test]
    async fn inventory_observation_does_not_create_authoring_intent_for_any_profile() {
        let pool = capability_store_pool().await;
        insert_active_profile_attached_to(&pool, "profile-attached", "server-a").await;
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, is_active, is_default, priority) VALUES ('profile-unrelated', 'Profile Unrelated', '', 'shared', 1, 0, 1)",
        )
        .execute(&pool)
        .await
        .expect("insert unrelated active profile");

        let (initialize, tool, resource, prompt, template) = protocol_fixture();
        commit_protocol_items_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(initialize),
            vec![tool],
            vec![resource],
            vec![prompt],
            vec![template],
            crate::core::pool::CapSyncFlags::ALL,
        )
        .await
        .expect("commit protocol observation");

        let authored_profiles = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT profile_id FROM profile_capability_refs ORDER BY profile_id",
        )
        .fetch_all(&pool)
        .await
        .expect("load authored profiles");
        assert!(authored_profiles.is_empty());
        let relationship_profiles =
            sqlx::query_scalar::<_, String>("SELECT profile_id FROM profile_server_relationships ORDER BY profile_id")
                .fetch_all(&pool)
                .await
                .expect("load server-level profile intent");
        assert_eq!(relationship_profiles, vec!["profile-attached"]);
    }

    #[tokio::test]
    async fn live_observation_atomically_replaces_a_corrupt_catalog_snapshot() {
        let pool = capability_store_pool().await;
        let (initialize, tool, _, _, _) = protocol_fixture();
        let first_commit = commit_protocol_items_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(initialize.clone()),
            vec![tool],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            crate::core::pool::CapSyncFlags::ALL,
        )
        .await
        .expect("commit initial observation");
        let second_commit = commit_protocol_items_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(initialize.clone()),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            crate::core::pool::CapSyncFlags::TOOLS,
        )
        .await
        .expect("advance catalog revision before corruption");
        assert_eq!(first_commit.revision, 1);
        assert_eq!(second_commit.revision, 2);
        sqlx::query(
            "UPDATE capability_server_snapshots SET initialize_payload = '{corrupt-json' WHERE server_id = 'server-a'",
        )
        .execute(&pool)
        .await
        .expect("corrupt stored initialize payload");
        let replacement_tool: rmcp::model::Tool = decode(json!({
            "name": "replacement",
            "description": "Replacement after live recovery",
            "inputSchema": {"type": "object"}
        }));

        let replacement_commit = commit_protocol_items_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(initialize),
            vec![replacement_tool.clone()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            crate::core::pool::CapSyncFlags::TOOLS,
        )
        .await
        .expect("live observation should replace the corrupt snapshot");
        assert_eq!(replacement_commit.revision, 4);

        let snapshot = SqliteCapabilityCatalog::new(pool.clone())
            .load_snapshot("server-a")
            .await
            .expect("replacement snapshot should decode")
            .expect("replacement snapshot should exist");
        assert_eq!(snapshot.revision, 4);
        assert!(snapshot.records.iter().any(
            |record| matches!(&record.source_payload, CapabilityPayload::Tool(tool) if tool.name == replacement_tool.name)
        ));
        let kind_revisions: Vec<i64> = sqlx::query_scalar(
            "SELECT DISTINCT catalog_revision FROM capability_kind_states WHERE server_id = 'server-a'",
        )
        .fetch_all(&pool)
        .await
        .expect("load capability_kind_states revisions");
        assert_eq!(
            kind_revisions,
            vec![4],
            "capability_kind_states must use the replacement revision"
        );
        let current_revisions: Vec<i64> = sqlx::query_scalar(
            r#"
            SELECT DISTINCT c.catalog_revision
            FROM capability_ref_current c
            JOIN capability_refs r ON r.ref_id = c.ref_id
            WHERE r.server_id = 'server-a'
            "#,
        )
        .fetch_all(&pool)
        .await
        .expect("load capability_ref_current revisions");
        assert_eq!(
            current_revisions,
            vec![4],
            "capability_ref_current must use the replacement revision"
        );
    }

    #[tokio::test]
    async fn corrupt_snapshot_replacement_rolls_back_when_a_later_write_fails() {
        let pool = capability_store_pool().await;
        let (initialize, tool, _, _, _) = protocol_fixture();
        commit_protocol_items_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(initialize.clone()),
            vec![tool],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            crate::core::pool::CapSyncFlags::ALL,
        )
        .await
        .expect("commit initial observation");
        let corrupt_payload = "{corrupt-json";
        sqlx::query("UPDATE capability_server_snapshots SET initialize_payload = ? WHERE server_id = 'server-a'")
            .bind(corrupt_payload)
            .execute(&pool)
            .await
            .expect("corrupt stored initialize payload");
        sqlx::query(
            "CREATE TRIGGER fail_replacement_tool BEFORE INSERT ON server_tools BEGIN SELECT RAISE(ABORT, 'replacement fixture failure'); END",
        )
        .execute(&pool)
        .await
        .expect("install replacement failure trigger");
        let replacement_tool: rmcp::model::Tool = decode(json!({
            "name": "replacement",
            "description": "Replacement after live recovery",
            "inputSchema": {"type": "object"}
        }));

        let error = commit_protocol_items_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(initialize),
            vec![replacement_tool],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            crate::core::pool::CapSyncFlags::TOOLS,
        )
        .await
        .expect_err("later write failure should abort corrupt replacement");

        assert!(
            format!("{error:#}").contains("replacement fixture failure"),
            "unexpected replacement error: {error:#}"
        );
        let stored_payload: String = sqlx::query_scalar(
            "SELECT initialize_payload FROM capability_server_snapshots WHERE server_id = 'server-a'",
        )
        .fetch_one(&pool)
        .await
        .expect("original corrupt row should survive rollback");
        assert_eq!(stored_payload, corrupt_payload);
        assert!(matches!(
            SqliteCapabilityCatalog::new(pool).load_snapshot("server-a").await,
            Err(mcpmate_capability_store::CatalogError::Json(_))
        ));
    }

    #[tokio::test]
    async fn partial_protocol_refresh_preserves_untouched_full_payloads() {
        let pool = capability_store_pool().await;
        let (initialize, tool, resource, prompt, template) = protocol_fixture();
        commit_protocol_items_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(initialize.clone()),
            vec![tool],
            vec![resource.clone()],
            vec![prompt.clone()],
            vec![template.clone()],
            crate::core::pool::CapSyncFlags::ALL,
        )
        .await
        .expect("commit initial observation");
        let updated_tool: rmcp::model::Tool = decode(json!({
            "name": "analyze-v2",
            "title": "Analyze v2",
            "description": "Updated tool only",
            "inputSchema": {"type": "object"},
            "outputSchema": {"type": "object"},
            "_meta": {"fixture": "tool-v2"}
        }));

        let commit = commit_protocol_items_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(initialize),
            vec![updated_tool.clone()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            crate::core::pool::CapSyncFlags::TOOLS,
        )
        .await
        .expect("commit partial tools refresh");

        assert_eq!(commit.revision, 2);
        let snapshot = SqliteCapabilityCatalog::new(pool)
            .load_snapshot("server-a")
            .await
            .expect("load catalog")
            .expect("catalog snapshot exists");
        let payloads = snapshot
            .records
            .into_iter()
            .map(|record| record.source_payload)
            .collect::<Vec<_>>();
        assert!(payloads.contains(&CapabilityPayload::Tool(updated_tool)));
        assert!(payloads.contains(&CapabilityPayload::Resource(resource)));
        assert!(payloads.contains(&CapabilityPayload::Prompt(prompt)));
        assert!(payloads.contains(&CapabilityPayload::ResourceTemplate(template)));
        assert_eq!(payloads.len(), 4);
    }

    #[tokio::test]
    async fn raw_snapshot_keeps_templates_that_cannot_enter_external_projection() {
        let pool = capability_store_pool().await;
        store_dual_write(
            &pool,
            "server-a",
            "docs",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![cached_template("file:///{path}"), cached_template("file:///{+path}")],
            Some("2025-11-25".to_string()),
        )
        .await
        .expect("store raw templates independently from external projection");

        let cached = SqliteCapabilityCatalog::new(pool.clone())
            .load_snapshot("server-a")
            .await
            .expect("read raw snapshot")
            .expect("raw snapshot exists");
        assert_eq!(
            cached
                .records
                .iter()
                .filter(|record| matches!(record.source_payload, CapabilityPayload::ResourceTemplate(_)))
                .count(),
            2
        );

        let projected = sqlx::query_scalar::<_, String>(
            "SELECT uri_template FROM server_resource_templates WHERE server_id = ? ORDER BY uri_template",
        )
        .bind("server-a")
        .fetch_all(&pool)
        .await
        .expect("load projected templates");
        assert_eq!(projected, vec!["file:///{path}".to_string()]);
    }

    #[tokio::test]
    async fn shadow_index_integrity_check_ignores_unprojectable_templates() {
        // An unprojectable Resource Template (e.g. `file:///{+path}`) legitimately never gets a
        // `server_resource_templates` shadow row, by design of
        // `upsert_shadow_resource_templates_batch_in_transaction`. The integrity check must not
        // treat that intentional gap as catalog/shadow-index divergence, or every cache-first
        // read for a server with such a template would invalidate a perfectly valid snapshot
        // and fall back to live discovery (Codex review, PR #160).
        let pool = capability_store_pool().await;
        let projectable: rmcp::model::ResourceTemplate = decode(json!({
            "uriTemplate": "file:///{path}",
            "name": "projectable-template"
        }));
        let unprojectable: rmcp::model::ResourceTemplate = decode(json!({
            "uriTemplate": "file:///{+path}",
            "name": "unprojectable-template"
        }));

        commit_protocol_items_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(decode(json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"resources": {"listChanged": true}},
                "serverInfo": {"name": "docs", "version": "1.0.0"}
            }))),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![projectable, unprojectable],
            crate::core::pool::CapSyncFlags::ALL,
        )
        .await
        .expect("commit a snapshot containing an unprojectable template");

        let snapshot = SqliteCapabilityCatalog::new(pool.clone())
            .load_snapshot("server-a")
            .await
            .expect("load catalog")
            .expect("catalog snapshot exists");
        assert_eq!(
            snapshot
                .records
                .iter()
                .filter(|record| record.kind() == CatalogKind::ResourceTemplates)
                .count(),
            2,
            "the catalog must retain both templates even though only one is projectable"
        );

        let trustworthy =
            shadow_index_matches_catalog_kind(&pool, "server-a", CatalogKind::ResourceTemplates, &snapshot.records)
                .await
                .expect("integrity check must run");
        assert!(
            trustworthy,
            "an unprojectable template's intentional absence from the shadow index must not look like corruption"
        );
    }

    fn cached_tool(name: &str) -> CachedToolInfo {
        CachedToolInfo {
            name: name.to_string(),
            description: None,
            input_schema_json: r#"{"type":"object"}"#.to_string(),
            output_schema_json: Some(r#"{"type":"object"}"#.to_string()),
            unique_name: None,
            icons: None,
            enabled: true,
            cached_at: Utc::now(),
        }
    }

    fn cached_resource(uri: &str) -> CachedResourceInfo {
        CachedResourceInfo {
            uri: uri.to_string(),
            name: Some(uri.to_string()),
            description: None,
            mime_type: None,
            icons: None,
            enabled: true,
            cached_at: Utc::now(),
        }
    }

    fn cached_template(uri_template: &str) -> CachedResourceTemplateInfo {
        CachedResourceTemplateInfo {
            uri_template: uri_template.to_string(),
            name: Some(uri_template.to_string()),
            description: None,
            mime_type: None,
            enabled: true,
            cached_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn first_partial_observation_records_only_the_selected_kind_as_complete() {
        let pool = capability_store_pool().await;
        let (_, tool, _, _, _) = protocol_fixture();
        let initialize = decode(json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {
                "tools": {"listChanged": true},
                "prompts": {"listChanged": true}
            },
            "serverInfo": {"name": "docs", "version": "1.0.0"}
        }));

        commit_protocol_observation_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(initialize),
            vec![tool],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            crate::core::pool::CapSyncFlags::TOOLS,
            Vec::new(),
        )
        .await
        .expect("first partial observation must establish a catalog baseline");

        let snapshot = SqliteCapabilityCatalog::new(pool.clone())
            .load_snapshot("server-a")
            .await
            .expect("load first partial snapshot")
            .expect("first partial snapshot exists");
        let state = |kind| {
            snapshot
                .kind_states
                .iter()
                .find(|state| state.kind == kind)
                .expect("kind state exists")
        };
        assert_eq!(state(CatalogKind::Tools).declaration, DeclarationState::Supported);
        assert_eq!(state(CatalogKind::Tools).inventory, InventoryState::Complete);
        assert_eq!(state(CatalogKind::Prompts).declaration, DeclarationState::Supported);
        assert_eq!(state(CatalogKind::Prompts).inventory, InventoryState::Unknown);
        assert_eq!(state(CatalogKind::Resources).declaration, DeclarationState::Unsupported);
        assert_eq!(state(CatalogKind::Resources).inventory, InventoryState::Unknown);
        assert_eq!(
            state(CatalogKind::ResourceTemplates).declaration,
            DeclarationState::Unsupported
        );
        assert_eq!(state(CatalogKind::ResourceTemplates).inventory, InventoryState::Unknown);
        assert_eq!(
            snapshot
                .records
                .iter()
                .filter(|record| record.kind() == CatalogKind::Tools)
                .count(),
            1
        );
        assert!(
            snapshot
                .records
                .iter()
                .all(|record| record.kind() == CatalogKind::Tools)
        );
    }

    #[tokio::test]
    async fn first_partial_unsupported_observation_completes_only_the_selected_kind() {
        let pool = capability_store_pool().await;
        let initialize = decode(json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"resources": {"listChanged": true}},
            "serverInfo": {"name": "docs", "version": "1.0.0"}
        }));

        commit_protocol_observation_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(initialize),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            crate::core::pool::CapSyncFlags::RESOURCE_TEMPLATES,
            vec![KindObservation::new(
                CatalogKind::ResourceTemplates,
                DeclarationState::Unsupported,
                InventoryState::Complete,
            )],
        )
        .await
        .expect("first partial unsupported observation must establish a catalog baseline");

        let snapshot = SqliteCapabilityCatalog::new(pool.clone())
            .load_snapshot("server-a")
            .await
            .expect("load first unsupported partial snapshot")
            .expect("first unsupported partial snapshot exists");
        let state = |kind| {
            snapshot
                .kind_states
                .iter()
                .find(|state| state.kind == kind)
                .expect("kind state exists")
        };
        assert_eq!(state(CatalogKind::Tools).declaration, DeclarationState::Unsupported);
        assert_eq!(state(CatalogKind::Tools).inventory, InventoryState::Unknown);
        assert_eq!(state(CatalogKind::Prompts).declaration, DeclarationState::Unsupported);
        assert_eq!(state(CatalogKind::Prompts).inventory, InventoryState::Unknown);
        assert_eq!(state(CatalogKind::Resources).declaration, DeclarationState::Supported);
        assert_eq!(state(CatalogKind::Resources).inventory, InventoryState::Unknown);
        assert_eq!(
            state(CatalogKind::ResourceTemplates).declaration,
            DeclarationState::Unsupported
        );
        assert_eq!(
            state(CatalogKind::ResourceTemplates).inventory,
            InventoryState::Complete
        );
        assert!(snapshot.records.is_empty());
    }

    #[tokio::test]
    async fn fresh_failure_creates_scoped_unavailable_snapshot_with_canonical_evidence() {
        let pool = capability_store_pool().await;
        let server_id = "server-fresh-failure-event";
        let server_name = "fresh_failure_docs";
        insert_typed_stdio_server(&pool, server_id, server_name, "node").await;
        let cache = DerivedCapabilityCache::default();
        let catalog = SqliteCapabilityCatalog::new(pool.clone());
        assert!(
            catalog
                .load_snapshot(server_id)
                .await
                .expect("load empty catalog")
                .is_none()
        );
        let invalidations_before = cache.metrics().await.invalidations;
        let mut events = crate::core::events::EventBus::global().subscribe_async();

        let commit = record_capability_failure(
            &pool,
            &cache,
            CapabilityFailureEvidence {
                server_id: server_id.to_string(),
                kind: CatalogKind::Resources,
                instance_id: None,
                connection_generation: None,
                failure_kind: None,
                timeout_ms: None,
                reason: "initial resource discovery failed".to_string(),
            },
        )
        .await
        .expect("fresh failure must establish durable catalog evidence");

        assert_eq!(commit.revision, 1);
        let snapshot = catalog
            .load_snapshot(server_id)
            .await
            .expect("load failure snapshot")
            .expect("failure snapshot exists");
        assert_eq!(snapshot.server_name, server_name);
        assert_eq!(snapshot.state, SnapshotState::Unavailable);
        assert!(snapshot.records.is_empty());
        let resources = snapshot
            .kind_states
            .iter()
            .find(|state| state.kind == CatalogKind::Resources)
            .expect("resources failure state exists");
        assert_eq!(resources.declaration, DeclarationState::Unknown);
        assert_eq!(resources.inventory, InventoryState::Failed);
        assert_eq!(
            snapshot.last_error.as_deref(),
            Some(
                "server_id=server-fresh-failure-event server_name=fresh_failure_docs kinds=[resources] instance=None generation=None reason=initial resource discovery failed"
            )
        );
        let initialize_payload: String =
            sqlx::query_scalar("SELECT initialize_payload FROM capability_server_snapshots WHERE server_id = ?")
                .bind(server_id)
                .fetch_one(&pool)
                .await
                .expect("load failure initialize payload");
        assert_eq!(
            initialize_payload, "null",
            "failure evidence must not fabricate initialize data"
        );
        assert_eq!(cache.metrics().await.invalidations, invalidations_before + 1);

        let mut committed = 0;
        let mut changed = 0;
        tokio::time::timeout(Duration::from_secs(1), async {
            while committed == 0 || changed == 0 {
                match events.recv().await.expect("receive catalog event") {
                    crate::core::events::Event::CapabilityCatalogCommitted {
                        server_id,
                        server_name,
                        revision,
                    } if server_id == "server-fresh-failure-event" => {
                        assert_eq!(server_name, "fresh_failure_docs");
                        assert_eq!(revision, 1);
                        committed += 1;
                    }
                    crate::core::events::Event::CapabilityCatalogChanged { server_id, server_name }
                        if server_id == "server-fresh-failure-event" =>
                    {
                        assert_eq!(server_name, "fresh_failure_docs");
                        changed += 1;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("fresh failure must publish one complete catalog transition");
        tokio::time::sleep(Duration::from_millis(25)).await;
        while let Ok(event) = events.try_recv() {
            match event {
                crate::core::events::Event::CapabilityCatalogCommitted { server_id, .. }
                    if server_id == "server-fresh-failure-event" =>
                {
                    committed += 1
                }
                crate::core::events::Event::CapabilityCatalogChanged { server_id, .. }
                    if server_id == "server-fresh-failure-event" =>
                {
                    changed += 1
                }
                _ => {}
            }
        }
        assert_eq!((committed, changed), (1, 1));

        let invalidations_before_repeat = cache.metrics().await.invalidations;
        let repeated = record_capability_failure(
            &pool,
            &cache,
            CapabilityFailureEvidence {
                server_id: server_id.to_string(),
                kind: CatalogKind::Resources,
                instance_id: Some("validation-2".to_string()),
                connection_generation: Some(2),
                failure_kind: None,
                timeout_ms: None,
                reason: "latest resource discovery evidence".to_string(),
            },
        )
        .await
        .expect("repeated failure must refresh diagnostics without a catalog transition");
        assert_eq!(repeated.revision, commit.revision);
        assert!(!repeated.changed);
        assert_eq!(cache.metrics().await.invalidations, invalidations_before_repeat);
        let repeated_snapshot = catalog
            .load_snapshot(server_id)
            .await
            .expect("load repeated failure snapshot")
            .expect("repeated failure snapshot exists");
        assert!(
            repeated_snapshot
                .last_error
                .as_deref()
                .is_some_and(|reason| reason.contains("latest resource discovery evidence"))
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
        let mut repeated_committed = 0;
        let mut repeated_changed = 0;
        while let Ok(event) = events.try_recv() {
            match event {
                crate::core::events::Event::CapabilityCatalogCommitted { server_id, .. }
                    if server_id == "server-fresh-failure-event" =>
                {
                    repeated_committed += 1
                }
                crate::core::events::Event::CapabilityCatalogChanged { server_id, .. }
                    if server_id == "server-fresh-failure-event" =>
                {
                    repeated_changed += 1
                }
                _ => {}
            }
        }
        assert_eq!((repeated_committed, repeated_changed), (0, 0));
    }

    #[tokio::test]
    async fn partial_observation_retains_unselected_history_after_unavailable_baseline() {
        let pool = capability_store_pool().await;
        insert_active_profile(&pool).await;
        let (initial_initialize, initial_tool, initial_resource, initial_prompt, initial_template) = protocol_fixture();
        let full_commit = commit_protocol_items_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(initial_initialize),
            vec![initial_tool],
            vec![initial_resource],
            vec![initial_prompt.clone()],
            vec![initial_template],
            crate::core::pool::CapSyncFlags::ALL,
        )
        .await
        .expect("commit full Ready baseline");
        let unavailable_commit = SqliteCapabilityCatalog::new(pool.clone())
            .record_failure("server-a", None, "upstream unavailable")
            .await
            .expect("mark full baseline unavailable");

        let current_initialize = decode(json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {
                "tools": {"listChanged": true},
                "prompts": {"listChanged": true}
            },
            "serverInfo": {"name": "docs", "version": "2.3.0"}
        }));
        let current_tool = decode(json!({
            "name": "current_tool",
            "description": "Current live tool",
            "inputSchema": {"type": "object"}
        }));
        let partial_commit = commit_protocol_observation_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(current_initialize),
            vec![current_tool],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            crate::core::pool::CapSyncFlags::TOOLS,
            Vec::new(),
        )
        .await
        .expect("commit selected-kind recovery");

        assert_eq!(unavailable_commit.revision, full_commit.revision + 1);
        assert_eq!(partial_commit.revision, unavailable_commit.revision + 1);
        let snapshot = SqliteCapabilityCatalog::new(pool.clone())
            .load_snapshot("server-a")
            .await
            .expect("load rebuilt snapshot")
            .expect("rebuilt snapshot exists");
        assert_eq!(snapshot.state, SnapshotState::Ready);
        let state = |kind| {
            snapshot
                .kind_states
                .iter()
                .find(|state| state.kind == kind)
                .expect("kind state exists")
        };
        assert_eq!(state(CatalogKind::Tools).declaration, DeclarationState::Supported);
        assert_eq!(state(CatalogKind::Tools).inventory, InventoryState::Complete);
        assert_eq!(state(CatalogKind::Prompts).declaration, DeclarationState::Supported);
        assert_eq!(state(CatalogKind::Prompts).inventory, InventoryState::Unknown);
        assert_eq!(state(CatalogKind::Resources).declaration, DeclarationState::Unsupported);
        assert_eq!(state(CatalogKind::Resources).inventory, InventoryState::Unknown);
        assert_eq!(
            state(CatalogKind::ResourceTemplates).declaration,
            DeclarationState::Unsupported
        );
        assert_eq!(state(CatalogKind::ResourceTemplates).inventory, InventoryState::Unknown);
        assert_eq!(snapshot.records.len(), 4);
        assert!(
            snapshot
                .records
                .iter()
                .any(|record| record.kind() == CatalogKind::Tools && record.upstream_key == "current_tool")
        );
        assert!(
            snapshot
                .records
                .iter()
                .any(|record| record.kind() == CatalogKind::Prompts && record.upstream_key == initial_prompt.name)
        );

        let shadow_prompts = sqlx::query_scalar::<_, String>(
            "SELECT prompt_name FROM server_prompts WHERE server_id = 'server-a' ORDER BY prompt_name",
        )
        .fetch_all(&pool)
        .await
        .expect("load prompt shadow index");
        assert_eq!(shadow_prompts, vec![initial_prompt.name.clone()]);

        let profile_ref_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM profile_capability_refs WHERE profile_id = 'profile-a'")
                .fetch_one(&pool)
                .await
                .expect("count profile capability intent");
        assert_eq!(profile_ref_count, 0);
    }

    #[tokio::test]
    async fn missing_initialize_fails_closed_instead_of_inferring_declarations_from_inventory() {
        let pool = capability_store_pool().await;
        insert_active_profile(&pool).await;
        let (_initialize, tool, _resource, _prompt, _template) = protocol_fixture();

        // A brand-new server with no prior baseline and no real protocol initialize result
        // must fail the commit rather than fabricate a "Supported" declaration purely
        // because the discovered tool inventory happens to be non-empty.
        let error = commit_protocol_items_for_kinds(
            &pool,
            "server-a",
            "docs",
            None,
            vec![tool],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            crate::core::pool::CapSyncFlags::ALL,
        )
        .await
        .expect_err("commit without initialize must fail closed");
        assert!(
            error
                .to_string()
                .contains("did not provide a protocol initialize result"),
            "unexpected error: {error}"
        );

        let snapshot = SqliteCapabilityCatalog::new(pool.clone())
            .load_snapshot("server-a")
            .await
            .expect("load catalog after failed commit");
        assert!(
            snapshot.is_none(),
            "a failed commit must not leave a partial catalog snapshot behind"
        );
    }

    #[tokio::test]
    async fn client_visible_tool_metadata_change_emits_catalog_changed() {
        let pool = capability_store_pool().await;
        let mut initial = cached_tool("read");
        initial.description = Some("Initial description".to_string());
        store_dual_write(
            &pool,
            "server-a",
            "docs",
            vec![initial],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Some("2025-11-25".to_string()),
        )
        .await
        .expect("store initial snapshot");

        let mut events = crate::core::events::EventBus::global().subscribe_async();
        let mut updated = cached_tool("read");
        updated.description = Some("Updated description".to_string());
        store_dual_write(
            &pool,
            "server-a",
            "docs",
            vec![updated],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Some("2025-11-25".to_string()),
        )
        .await
        .expect("store metadata-only snapshot change");

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), async move {
            loop {
                match events.recv().await {
                    Ok(crate::core::events::Event::CapabilityCatalogChanged { server_id, .. })
                        if server_id == "server-a" =>
                    {
                        break;
                    }
                    Ok(_) => continue,
                    Err(error) => panic!("event receiver failed: {error}"),
                }
            }
        })
        .await;
        assert!(event.is_ok(), "metadata-only change must notify downstream clients");
    }

    #[tokio::test]
    async fn identical_capability_observation_does_not_invalidate_or_emit_catalog_events() {
        let pool = capability_store_pool().await;
        let server_id = "server-identical-noop";
        let server_name = "identical_noop_docs";
        insert_typed_stdio_server(&pool, server_id, server_name, "node").await;
        let cache = DerivedCapabilityCache::default();
        let (initialize, tool, resource, prompt, template) = protocol_fixture();
        let observation = CapabilityProtocolObservation {
            initialize: Some(initialize),
            tools: vec![tool],
            resources: vec![resource],
            prompts: vec![prompt],
            templates: vec![template],
            kinds: crate::core::pool::CapSyncFlags::ALL,
            kind_states: Vec::new(),
        };
        let config_fingerprint = current_config_fingerprint(&pool, server_id)
            .await
            .expect("load config fingerprint");
        let first = commit_capability_protocol_observation(
            &pool,
            &cache,
            server_id,
            server_name,
            &config_fingerprint,
            CapabilityProtocolObservation {
                initialize: observation.initialize.clone(),
                tools: observation.tools.clone(),
                resources: observation.resources.clone(),
                prompts: observation.prompts.clone(),
                templates: observation.templates.clone(),
                kinds: observation.kinds,
                kind_states: observation.kind_states.clone(),
            },
        )
        .await
        .expect("commit initial observation");
        assert!(first.changed);
        let first_snapshot = SqliteCapabilityCatalog::new(pool.clone())
            .load_snapshot(server_id)
            .await
            .unwrap()
            .unwrap();
        let first_kind_states = first_snapshot.kind_states.clone();
        let first_config_fingerprint = first_snapshot.config_fingerprint.clone();
        let first_state = first_snapshot.state;
        let first_ids = first_snapshot
            .records
            .into_iter()
            .map(|record| (record.ref_id.to_string(), record.capability_id.to_string()))
            .collect::<Vec<_>>();
        let invalidations_after_first = cache.metrics().await.invalidations;
        let mut events = crate::core::events::EventBus::global().subscribe_async();

        let repeated = commit_capability_protocol_observation(
            &pool,
            &cache,
            server_id,
            server_name,
            &config_fingerprint,
            observation,
        )
        .await
        .expect("commit identical observation");
        let repeated_snapshot = SqliteCapabilityCatalog::new(pool.clone())
            .load_snapshot(server_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(repeated_snapshot.kind_states, first_kind_states);
        assert_eq!(repeated_snapshot.config_fingerprint, first_config_fingerprint);
        assert_eq!(repeated_snapshot.state, first_state);
        let repeated_ids = repeated_snapshot
            .records
            .into_iter()
            .map(|record| (record.ref_id.to_string(), record.capability_id.to_string()))
            .collect::<Vec<_>>();
        assert_eq!(repeated_ids, first_ids);

        assert_eq!(repeated.revision, first.revision);
        assert!(!repeated.changed);
        assert_eq!(cache.metrics().await.invalidations, invalidations_after_first);
        let mut committed = 0;
        let mut changed = 0;
        while let Ok(event) = events.try_recv() {
            match event {
                crate::core::events::Event::CapabilityCatalogCommitted { server_id, .. }
                    if server_id == "server-identical-noop" =>
                {
                    committed += 1
                }
                crate::core::events::Event::CapabilityCatalogChanged { server_id, .. }
                    if server_id == "server-identical-noop" =>
                {
                    changed += 1
                }
                _ => {}
            }
        }
        assert_eq!((committed, changed), (0, 0));
    }

    #[tokio::test]
    async fn discovery_commit_rejects_a_changed_server_configuration() {
        let pool = capability_store_pool().await;
        let server_id = "server-config-race";
        let server_name = "config_race_docs";
        insert_typed_stdio_server(&pool, server_id, server_name, "old-command").await;
        let expected_fingerprint = current_config_fingerprint(&pool, server_id)
            .await
            .expect("capture owner fingerprint");
        insert_typed_stdio_server(&pool, server_id, server_name, "new-command").await;

        let cache = DerivedCapabilityCache::default();
        let (initialize, tool, resource, prompt, template) = protocol_fixture();
        let error = commit_capability_protocol_observation(
            &pool,
            &cache,
            server_id,
            server_name,
            &expected_fingerprint,
            CapabilityProtocolObservation {
                initialize: Some(initialize),
                tools: vec![tool],
                resources: vec![resource],
                prompts: vec![prompt],
                templates: vec![template],
                kinds: crate::core::pool::CapSyncFlags::ALL,
                kind_states: Vec::new(),
            },
        )
        .await
        .expect_err("stale discovery must not commit under the new configuration");

        assert!(
            error
                .to_string()
                .contains("server configuration changed during capability discovery")
        );
        assert!(
            SqliteCapabilityCatalog::new(pool)
                .load_snapshot(server_id)
                .await
                .expect("load catalog")
                .is_none()
        );
    }

    #[tokio::test]
    async fn preview_timeout_is_applied_to_each_operation_independently() {
        let timeout = Duration::from_millis(40);

        let first = run_preview_operation("prompts/list", Some(timeout), async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Ok::<_, anyhow::Error>("prompts")
        })
        .await
        .expect("first operation should finish within its own deadline");
        let second = run_preview_operation("resources/list", Some(timeout), async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Ok::<_, anyhow::Error>("resources")
        })
        .await
        .expect("second operation should receive a fresh deadline");

        assert_eq!(first, "prompts");
        assert_eq!(second, "resources");
    }

    #[tokio::test]
    async fn preview_timeout_identifies_the_failed_operation() {
        let error = run_preview_operation("resources/templates/list", Some(Duration::from_millis(5)), async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect_err("slow operation must time out");

        assert!(error.to_string().contains("resources/templates/list"));
    }

    #[tokio::test]
    async fn snapshot_identity_is_persisted_without_overwriting_namespace() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        insert_typed_stdio_server(&pool, "server-a", "everything", "node").await;
        let snapshot = CapabilitySnapshot {
            upstream_name: Some("io.modelcontextprotocol/everything".to_string()),
            upstream_title: Some("Everything Reference Server".to_string()),
            server_version: Some("1.2.3".to_string()),
            protocol_version: Some("2025-11-25".to_string()),
            ..Default::default()
        };

        persist_snapshot_server_info(&pool, "server-a", &snapshot)
            .await
            .expect("persist snapshot identity");

        let namespace: String = sqlx::query_scalar("SELECT name FROM server_config WHERE id = 'server-a'")
            .fetch_one(&pool)
            .await
            .expect("load namespace");
        let upstream_name: String =
            sqlx::query_scalar("SELECT upstream_name FROM server_meta WHERE server_id = 'server-a'")
                .fetch_one(&pool)
                .await
                .expect("load upstream name");
        assert_eq!(namespace, "everything");
        assert_eq!(upstream_name, "io.modelcontextprotocol/everything");
    }

    #[tokio::test]
    async fn raw_snapshot_preserves_exact_values_for_every_capability_kind() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        crate::config::profile::init::initialize_profile_tables(&pool)
            .await
            .expect("initialize profile tables");
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("initialize client table");
        insert_typed_stdio_server(&pool, "server-a", "searxng", "node").await;

        let now = Utc::now();
        store_dual_write(
            &pool,
            "server-a",
            "searxng",
            vec![CachedToolInfo {
                name: "get_searxng_status".to_string(),
                description: None,
                input_schema_json: r#"{"type":"object"}"#.to_string(),
                output_schema_json: None,
                unique_name: None,
                icons: None,
                enabled: true,
                cached_at: now,
            }],
            vec![CachedResourceInfo {
                uri: "file:///searxng/status".to_string(),
                name: Some("Status".to_string()),
                description: None,
                mime_type: None,
                icons: None,
                enabled: true,
                cached_at: now,
            }],
            vec![CachedPromptInfo {
                name: "get_searxng_help".to_string(),
                description: None,
                arguments: Vec::new(),
                icons: None,
                enabled: true,
                cached_at: now,
            }],
            vec![CachedResourceTemplateInfo {
                uri_template: "searxng://status/{id}".to_string(),
                name: Some("Status template".to_string()),
                description: None,
                mime_type: None,
                enabled: true,
                cached_at: now,
            }],
            Some("2025-11-25".to_string()),
        )
        .await
        .expect("store raw snapshot");

        let cached = SqliteCapabilityCatalog::new(pool.clone())
            .load_snapshot("server-a")
            .await
            .expect("read raw snapshot")
            .expect("raw snapshot exists");
        let payloads = cached
            .records
            .into_iter()
            .map(|record| record.source_payload)
            .collect::<Vec<_>>();
        assert!(
            payloads
                .iter()
                .any(|payload| matches!(payload, CapabilityPayload::Tool(tool) if tool.name == "get_searxng_status"))
        );
        assert!(
            payloads.iter().any(
                |payload| matches!(payload, CapabilityPayload::Prompt(prompt) if prompt.name == "get_searxng_help")
            )
        );
        assert!(payloads.iter().any(
            |payload| matches!(payload, CapabilityPayload::Resource(resource) if resource.uri == "file:///searxng/status")
        ));
        assert!(payloads.iter().any(
            |payload| matches!(payload, CapabilityPayload::ResourceTemplate(template) if template.uri_template == "searxng://status/{id}")
        ));
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT unique_name FROM server_tools WHERE server_id = 'server-a'")
                .fetch_one(&pool)
                .await
                .expect("load tool projection"),
            "searxng_get_status"
        );
        let (unique_name, route_uri): (String, String) =
            sqlx::query_as("SELECT unique_name, route_uri FROM server_resource_templates WHERE server_id = 'server-a'")
                .fetch_one(&pool)
                .await
                .expect("load persisted template route");
        assert_eq!(unique_name, "mcpmate://resources/template/searxng/searxng/status/{id}");
        assert_eq!(route_uri, "mcpmate://resources/template/searxng/searxng/status/{}");
    }

    #[tokio::test]
    async fn empty_snapshot_removes_stale_rows_from_every_capability_catalog() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        crate::config::profile::init::initialize_profile_tables(&pool)
            .await
            .expect("initialize profile tables");
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("initialize client table");
        insert_typed_stdio_server(&pool, "server-a", "docs", "node").await;
        crate::config::server::tools::upsert_server_tool(&pool, "server-a", "docs", "read", None)
            .await
            .expect("insert tool");
        upsert_shadow_prompt(&pool, "server-a", "docs", "help", None)
            .await
            .expect("insert prompt");
        upsert_shadow_resource(&pool, "server-a", "docs", "file:///guide", None, None, None)
            .await
            .expect("insert resource");
        upsert_shadow_resource_template(&pool, "server-a", "docs", "file:///{path}", Some("Files"), None)
            .await
            .expect("insert template");
        sqlx::query(
            "INSERT INTO server_issued_resources (id, server_id, server_name, resource_uri, unique_uri) VALUES ('issued-1', 'server-a', 'docs', 'file:///generated', 'mcpmate://resources/docs/file/generated')",
        )
        .execute(&pool)
        .await
        .expect("insert issued resource route");
        store_dual_write(
            &pool,
            "server-a",
            "docs",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Some("2025-11-25".to_string()),
        )
        .await
        .expect("store empty authoritative snapshot");

        for table in [
            "server_tools",
            "server_prompts",
            "server_resources",
            "server_resource_templates",
        ] {
            let query = format!("SELECT COUNT(*) FROM {table} WHERE server_id = 'server-a'");
            let count: i64 = sqlx::query_scalar(&query)
                .fetch_one(&pool)
                .await
                .expect("count catalog rows");
            assert_eq!(count, 0, "{table} retained stale capability rows");
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_issued_resources WHERE server_id = 'server-a'",)
                .fetch_one(&pool)
                .await
                .expect("count issued resource routes"),
            1,
            "authoritative resources/list reconciliation must not delete issued routes"
        );
    }

    #[tokio::test]
    async fn issued_resource_routes_survive_inventory_churn_without_route_churn() {
        let pool = capability_store_pool().await;
        let issued_upstream = "demo://resource/generated";
        let issued_uri = crate::core::capability::resource_registry::issue_resource_route(
            &pool,
            "server-a",
            "docs",
            issued_upstream,
        )
        .await
        .expect("issue dynamic resource route");
        assert_eq!(issued_uri, "mcpmate://resources/docs/demo/generated");

        let listed_upstream = "demo://resources/generated";
        upsert_shadow_resources_batch(&pool, "server-a", "docs", &[cached_resource(listed_upstream)])
            .await
            .expect("persist colliding listed resource");

        let listed_uri: String = sqlx::query_scalar("SELECT unique_uri FROM server_resources WHERE resource_uri = ?")
            .bind(listed_upstream)
            .fetch_one(&pool)
            .await
            .expect("load listed resource URI");
        assert_ne!(listed_uri, issued_uri);
        assert_eq!(
            crate::core::capability::resource_registry::resolve_resource_route(&pool, &issued_uri)
                .await
                .expect("resolve preserved issued route")
                .upstream_uri,
            issued_upstream
        );

        upsert_shadow_resources_batch(
            &pool,
            "server-a",
            "docs",
            &[cached_resource(issued_upstream), cached_resource(listed_upstream)],
        )
        .await
        .expect("promote issued resource into listed inventory");

        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT unique_uri FROM server_resources WHERE resource_uri = ?")
                .bind(issued_upstream)
                .fetch_one(&pool)
                .await
                .expect("load promoted listed resource"),
            issued_uri
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM server_issued_resources WHERE server_id = 'server-a' AND resource_uri = ?",
            )
            .bind(issued_upstream)
            .fetch_one(&pool)
            .await
            .expect("count retained issued routes"),
            1,
            "listing an issued upstream URI must retain its issued route"
        );
        assert_eq!(
            crate::core::capability::resource_registry::resolve_resource_route(&pool, &issued_uri)
                .await
                .expect("resolve listed route before inventory removal")
                .source,
            crate::core::capability::resource_registry::ResourceRouteSource::Listed
        );

        upsert_shadow_resources_batch(&pool, "server-a", "docs", &[cached_resource(listed_upstream)])
            .await
            .expect("remove issued resource from authoritative inventory");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_resources WHERE resource_uri = ?")
                .bind(issued_upstream)
                .fetch_one(&pool)
                .await
                .expect("count removed listed resource"),
            0
        );
        assert_eq!(
            crate::core::capability::resource_registry::resolve_resource_route(&pool, &issued_uri)
                .await
                .expect("resolve preserved issued route after inventory removal")
                .source,
            crate::core::capability::resource_registry::ResourceRouteSource::Issued
        );

        upsert_shadow_resources_batch(
            &pool,
            "server-a",
            "docs",
            &[cached_resource(issued_upstream), cached_resource(listed_upstream)],
        )
        .await
        .expect("relist issued resource");
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT unique_uri FROM server_resources WHERE resource_uri = ?")
                .bind(issued_upstream)
                .fetch_one(&pool)
                .await
                .expect("load relisted resource URI"),
            issued_uri,
            "relisting must reuse the original canonical URI"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM server_issued_resources WHERE server_id = 'server-a' AND resource_uri = ?",
            )
            .bind(issued_upstream)
            .fetch_one(&pool)
            .await
            .expect("count issued routes after relisting"),
            1
        );
    }

    #[tokio::test]
    async fn invalid_full_snapshot_does_not_commit_an_earlier_capability_kind() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        crate::config::profile::init::initialize_profile_tables(&pool)
            .await
            .expect("initialize profile tables");
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("initialize client table");
        insert_typed_stdio_server(&pool, "server-a", "docs", "node").await;
        let now = Utc::now();

        store_dual_write(
            &pool,
            "server-a",
            "docs",
            vec![CachedToolInfo {
                name: "old_tool".to_string(),
                description: None,
                input_schema_json: r#"{"type":"object"}"#.to_string(),
                output_schema_json: None,
                unique_name: None,
                icons: None,
                enabled: true,
                cached_at: now,
            }],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Some("2025-11-25".to_string()),
        )
        .await
        .expect("store initial snapshot");

        let duplicate_prompt = CachedPromptInfo {
            name: "duplicate".to_string(),
            description: None,
            arguments: Vec::new(),
            icons: None,
            enabled: true,
            cached_at: now,
        };
        let error = store_dual_write(
            &pool,
            "server-a",
            "docs",
            vec![CachedToolInfo {
                name: "new_tool".to_string(),
                description: None,
                input_schema_json: r#"{"type":"object"}"#.to_string(),
                output_schema_json: None,
                unique_name: None,
                icons: None,
                enabled: true,
                cached_at: now,
            }],
            Vec::new(),
            vec![duplicate_prompt.clone(), duplicate_prompt],
            Vec::new(),
            Some("2025-11-25".to_string()),
        )
        .await
        .expect_err("invalid later inventory must roll back the full catalog update");

        assert!(error.to_string().contains("duplicate upstream"));
        let tools = sqlx::query_scalar::<_, String>(
            "SELECT tool_name FROM server_tools WHERE server_id = 'server-a' ORDER BY tool_name",
        )
        .fetch_all(&pool)
        .await
        .expect("load committed tools");
        assert_eq!(tools, vec!["old_tool"]);
    }

    #[tokio::test]
    async fn scoped_snapshot_replaces_only_the_requested_capability_kind() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        crate::config::profile::init::initialize_profile_tables(&pool)
            .await
            .expect("initialize profile tables");
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("initialize client table");
        insert_typed_stdio_server(&pool, "server-a", "docs", "node").await;
        let now = Utc::now();
        store_dual_write(
            &pool,
            "server-a",
            "docs",
            vec![CachedToolInfo {
                name: "read".to_string(),
                description: None,
                input_schema_json: r#"{"type":"object"}"#.to_string(),
                output_schema_json: None,
                unique_name: None,
                icons: None,
                enabled: true,
                cached_at: now,
            }],
            vec![CachedResourceInfo {
                uri: "file:///guide".to_string(),
                name: Some("Guide".to_string()),
                description: None,
                mime_type: None,
                icons: None,
                enabled: true,
                cached_at: now,
            }],
            vec![CachedPromptInfo {
                name: "help".to_string(),
                description: None,
                arguments: Vec::new(),
                icons: None,
                enabled: true,
                cached_at: now,
            }],
            vec![CachedResourceTemplateInfo {
                uri_template: "file:///{path}".to_string(),
                name: Some("Files".to_string()),
                description: None,
                mime_type: None,
                enabled: true,
                cached_at: now,
            }],
            Some("2025-11-25".to_string()),
        )
        .await
        .expect("store initial full snapshot");

        store_dual_write_for_kinds(
            &pool,
            "server-a",
            "docs",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Some("2025-11-25".to_string()),
            crate::core::pool::CapSyncFlags::TOOLS,
        )
        .await
        .expect("replace only the tool inventory");

        let cached = SqliteCapabilityCatalog::new(pool)
            .load_snapshot("server-a")
            .await
            .expect("read scoped snapshot")
            .expect("scoped snapshot exists");
        assert_eq!(
            cached
                .records
                .iter()
                .filter(|record| matches!(record.source_payload, CapabilityPayload::Tool(_)))
                .count(),
            0
        );
        assert_eq!(
            cached
                .records
                .iter()
                .filter(|record| matches!(record.source_payload, CapabilityPayload::Prompt(_)))
                .count(),
            1
        );
        assert_eq!(
            cached
                .records
                .iter()
                .filter(|record| matches!(record.source_payload, CapabilityPayload::Resource(_)))
                .count(),
            1
        );
        assert_eq!(
            cached
                .records
                .iter()
                .filter(|record| matches!(record.source_payload, CapabilityPayload::ResourceTemplate(_)))
                .count(),
            1
        );
    }
}
