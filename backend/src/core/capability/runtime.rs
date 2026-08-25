use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::future::BoxFuture;
use mcpmate_capability_store::{
    CapabilityCatalog, CapabilityKind as CatalogKind, CapabilityPayload, CatalogCommit, CatalogError, CatalogSnapshot,
    InventoryState, KindFailureKind, KindObservation, ProjectionKey, ProjectionNameDomain, ProjectionPayload,
    SnapshotState, SqliteCapabilityCatalog,
};
use rmcp::service::{Peer, RoleClient};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;

use crate::config::database::Database;
use crate::core::capability::connection_provider::{CapabilityAuthenticationFailureCode, CapabilityOwner};
#[cfg(test)]
use crate::core::capability::index::{CachedResourceInfo, CachedResourceTemplateInfo};
use crate::core::capability::internal::{
    CapabilityFetchFailure, collect_capability_from_instance_peer, is_method_not_supported,
};
use crate::core::capability::naming::NamingKind;
use crate::core::capability::{CapabilityType, ConnectionSelection, RuntimeIdentity};
use crate::core::pool::{CapSyncFlags, FailureKind, UpstreamConnectionPool};

/// Context for capability listing operations.
#[derive(Clone, Debug)]
pub struct ListCtx {
    pub capability: CapabilityType,
    pub server_id: String,
    pub refresh: Option<RefreshStrategy>,
    pub operation_timeout: Duration,
    pub validation_session: Option<String>,
    pub runtime_identity: Option<RuntimeIdentity>,
    pub connection_selection: Option<ConnectionSelection>,
    pub visibility_snapshot: Option<Arc<crate::core::profile::visibility::VisibilitySnapshot>>,
    pub name_domain: NameDomain,
}

/// Selects which capability identifier domain a listing surface exposes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NameDomain {
    /// Exact identifiers reported by the upstream MCP server.
    Upstream,
    /// Stable MCPMate identifiers exposed outside the connection pool.
    External,
}

/// Context for capability call operations.
#[derive(Clone, Debug)]
pub struct CallCtx {
    pub call_id: String,
    pub server_id: String,
    pub tool_name: String,
    pub timeout: Option<Duration>,
    pub arguments: Option<rmcp::model::JsonObject>,
    pub runtime_identity: Option<RuntimeIdentity>,
    pub connection_selection: Option<ConnectionSelection>,
}

/// Refresh strategy for list operations.
#[derive(Clone, Copy, Debug)]
pub enum RefreshStrategy {
    CacheFirst,
    Force,
}

/// Metadata returned alongside capability results.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Meta {
    pub cache_hit: bool,
    pub source: String,
    pub duration_ms: u64,
    pub had_peer: bool,
}

/// Unified capability result container.
#[derive(Clone, Debug)]
pub struct ListResult {
    pub items: CapabilityItems,
    pub meta: Meta,
}

/// Polymorphic capability item collection.
#[derive(Clone, Debug)]
pub enum CapabilityItems {
    Tools(Vec<rmcp::model::Tool>),
    Prompts(Vec<rmcp::model::Prompt>),
    Resources(Vec<rmcp::model::Resource>),
    ResourceTemplates(Vec<rmcp::model::ResourceTemplate>),
}

impl CapabilityItems {
    pub fn empty(kind: CapabilityType) -> Self {
        match kind {
            CapabilityType::Tools => CapabilityItems::Tools(Vec::new()),
            CapabilityType::Prompts => CapabilityItems::Prompts(Vec::new()),
            CapabilityType::Resources => CapabilityItems::Resources(Vec::new()),
            CapabilityType::ResourceTemplates => CapabilityItems::ResourceTemplates(Vec::new()),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            CapabilityItems::Tools(items) => items.is_empty(),
            CapabilityItems::Prompts(items) => items.is_empty(),
            CapabilityItems::Resources(items) => items.is_empty(),
            CapabilityItems::ResourceTemplates(items) => items.is_empty(),
        }
    }

    pub fn into_tools(self) -> Option<Vec<rmcp::model::Tool>> {
        match self {
            CapabilityItems::Tools(items) => Some(items),
            _ => None,
        }
    }

    pub fn into_prompts(self) -> Option<Vec<rmcp::model::Prompt>> {
        match self {
            CapabilityItems::Prompts(items) => Some(items),
            _ => None,
        }
    }

    pub fn into_resources(self) -> Option<Vec<rmcp::model::Resource>> {
        match self {
            CapabilityItems::Resources(items) => Some(items),
            _ => None,
        }
    }

    pub fn into_resource_templates(self) -> Option<Vec<rmcp::model::ResourceTemplate>> {
        match self {
            CapabilityItems::ResourceTemplates(items) => Some(items),
            _ => None,
        }
    }
}

fn catalog_kind(capability: CapabilityType) -> CatalogKind {
    match capability {
        CapabilityType::Tools => CatalogKind::Tools,
        CapabilityType::Prompts => CatalogKind::Prompts,
        CapabilityType::Resources => CatalogKind::Resources,
        CapabilityType::ResourceTemplates => CatalogKind::ResourceTemplates,
    }
}

fn catalog_payloads_for_kind(
    records: Vec<mcpmate_capability_store::CatalogRecord>,
    kind: CatalogKind,
    name_domain: NameDomain,
) -> Vec<CapabilityPayload> {
    records
        .into_iter()
        .filter(|record| record.kind() == kind)
        .map(|record| match name_domain {
            NameDomain::External => record.effective_payload,
            NameDomain::Upstream => record.source_payload,
        })
        .collect()
}

fn items_from_catalog(
    capability: CapabilityType,
    records: Vec<mcpmate_capability_store::CatalogRecord>,
    name_domain: NameDomain,
) -> CapabilityItems {
    let payloads = catalog_payloads_for_kind(records, catalog_kind(capability), name_domain);
    match capability {
        CapabilityType::Tools => CapabilityItems::Tools(
            payloads
                .into_iter()
                .filter_map(|payload| match payload {
                    CapabilityPayload::Tool(tool) => Some(tool),
                    _ => None,
                })
                .collect(),
        ),
        CapabilityType::Prompts => CapabilityItems::Prompts(
            payloads
                .into_iter()
                .filter_map(|payload| match payload {
                    CapabilityPayload::Prompt(prompt) => Some(prompt),
                    _ => None,
                })
                .collect(),
        ),
        CapabilityType::Resources => CapabilityItems::Resources(
            payloads
                .into_iter()
                .filter_map(|payload| match payload {
                    CapabilityPayload::Resource(resource) => Some(resource),
                    _ => None,
                })
                .collect(),
        ),
        CapabilityType::ResourceTemplates => CapabilityItems::ResourceTemplates(
            payloads
                .into_iter()
                .filter_map(|payload| match payload {
                    CapabilityPayload::ResourceTemplate(template) => Some(template),
                    _ => None,
                })
                .collect(),
        ),
    }
}

fn projection_payload(items: CapabilityItems) -> ProjectionPayload {
    match items {
        CapabilityItems::Tools(items) => ProjectionPayload::Tools(items),
        CapabilityItems::Prompts(items) => ProjectionPayload::Prompts(items),
        CapabilityItems::Resources(items) => ProjectionPayload::Resources(items),
        CapabilityItems::ResourceTemplates(items) => ProjectionPayload::ResourceTemplates(items),
    }
}

fn items_from_projection(payload: &ProjectionPayload) -> CapabilityItems {
    match payload {
        ProjectionPayload::Tools(items) => CapabilityItems::Tools(items.clone()),
        ProjectionPayload::Prompts(items) => CapabilityItems::Prompts(items.clone()),
        ProjectionPayload::Resources(items) => CapabilityItems::Resources(items.clone()),
        ProjectionPayload::ResourceTemplates(items) => CapabilityItems::ResourceTemplates(items.clone()),
    }
}

fn projection_key(
    ctx: &ListCtx,
    revision: i64,
) -> ProjectionKey {
    let selection_key = ctx
        .connection_selection
        .as_ref()
        .map(ConnectionSelection::cache_scope_key)
        .unwrap_or_else(|| format!("{}#shared", ctx.server_id));
    // A caller with a `visibility_snapshot` but no `runtime_identity` (builtin/broker call
    // sites that resolve visibility ad hoc instead of through a bound client session) must
    // never fall back to the unscoped "shared_raw" bucket: that bucket is shared with
    // unfiltered management/API reads, and the snapshot's own `surface_fingerprint` already
    // encodes the exact visibility scope (config mode, allowed capability sets, direct
    // exposure) that produced this projection. Reusing it here keeps filtered projections
    // isolated from the unfiltered baseline and from other visibility scopes.
    let surface_fingerprint = ctx
        .runtime_identity
        .as_ref()
        .map(|identity| identity.surface_fingerprint.clone())
        .or_else(|| {
            ctx.visibility_snapshot
                .as_ref()
                .map(|snapshot| snapshot.surface_fingerprint.clone())
        })
        .unwrap_or_else(|| "shared_raw".to_string());
    let revision_set = format!("{}:{revision}", ctx.server_id);
    let catalog_revision_set_hash = format!("{:x}", Sha256::digest(revision_set));
    ProjectionKey::new(
        selection_key,
        surface_fingerprint,
        catalog_kind(ctx.capability),
        match ctx.name_domain {
            NameDomain::Upstream => ProjectionNameDomain::Upstream,
            NameDomain::External => ProjectionNameDomain::External,
        },
        catalog_revision_set_hash,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeFailureKind {
    Timeout,
    SessionGone,
    TransportClosed,
    StaleGeneration,
    Authentication,
    AuthRequired,
    Unauthorized,
    Forbidden,
    InsufficientScope,
    Protocol,
    Application,
    Other,
}

impl RuntimeFailureKind {
    pub(crate) const fn retry_disposition(
        self
    ) -> crate::core::capability::connection_provider::DiscoveryRetryDisposition {
        use crate::core::capability::connection_provider::DiscoveryRetryDisposition;

        match self {
            Self::SessionGone | Self::TransportClosed | Self::StaleGeneration => DiscoveryRetryDisposition::FreshOnce,
            Self::Timeout
            | Self::Authentication
            | Self::AuthRequired
            | Self::Unauthorized
            | Self::Forbidden
            | Self::InsufficientScope
            | Self::Protocol
            | Self::Application
            | Self::Other => DiscoveryRetryDisposition::DoNotRetry,
        }
    }

    pub(crate) const fn from_authentication_code(code: CapabilityAuthenticationFailureCode) -> Self {
        match code {
            CapabilityAuthenticationFailureCode::AuthRequired => Self::AuthRequired,
            CapabilityAuthenticationFailureCode::Unauthorized => Self::Unauthorized,
            CapabilityAuthenticationFailureCode::Forbidden => Self::Forbidden,
            CapabilityAuthenticationFailureCode::InsufficientScope => Self::InsufficientScope,
        }
    }

    pub(crate) const fn authentication_code(self) -> Option<CapabilityAuthenticationFailureCode> {
        match self {
            Self::Authentication | Self::AuthRequired => Some(CapabilityAuthenticationFailureCode::AuthRequired),
            Self::Unauthorized => Some(CapabilityAuthenticationFailureCode::Unauthorized),
            Self::Forbidden => Some(CapabilityAuthenticationFailureCode::Forbidden),
            Self::InsufficientScope => Some(CapabilityAuthenticationFailureCode::InsufficientScope),
            _ => None,
        }
    }

    pub(crate) const fn persisted(self) -> KindFailureKind {
        match self {
            Self::Timeout => KindFailureKind::Timeout,
            Self::SessionGone => KindFailureKind::SessionGone,
            Self::TransportClosed => KindFailureKind::TransportClosed,
            Self::StaleGeneration => KindFailureKind::StaleGeneration,
            Self::Authentication => KindFailureKind::Authentication,
            Self::AuthRequired => KindFailureKind::AuthRequired,
            Self::Unauthorized => KindFailureKind::Unauthorized,
            Self::Forbidden => KindFailureKind::Forbidden,
            Self::InsufficientScope => KindFailureKind::InsufficientScope,
            Self::Protocol => KindFailureKind::Protocol,
            Self::Application => KindFailureKind::Application,
            Self::Other => KindFailureKind::Other,
        }
    }

    const fn from_persisted(kind: KindFailureKind) -> Self {
        match kind {
            KindFailureKind::Timeout => Self::Timeout,
            KindFailureKind::SessionGone => Self::SessionGone,
            KindFailureKind::TransportClosed => Self::TransportClosed,
            KindFailureKind::StaleGeneration => Self::StaleGeneration,
            KindFailureKind::Authentication => Self::Authentication,
            KindFailureKind::AuthRequired => Self::AuthRequired,
            KindFailureKind::Unauthorized => Self::Unauthorized,
            KindFailureKind::Forbidden => Self::Forbidden,
            KindFailureKind::InsufficientScope => Self::InsufficientScope,
            KindFailureKind::Protocol => Self::Protocol,
            KindFailureKind::Application => Self::Application,
            KindFailureKind::Other => Self::Other,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("capability runtime failure ({kind:?}): {message}", message = .message.as_deref().unwrap_or("no detail"))]
pub(crate) struct RuntimeFailure {
    pub kind: RuntimeFailureKind,
    pub message: Option<String>,
    pub timeout_ms: Option<u128>,
}

/// Protocol observation returned by a capability discovery adapter.
#[derive(Debug)]
pub(crate) struct CapabilityDiscoveryObservation {
    pub(crate) items: CapabilityItems,
    pub(crate) flags: CapSyncFlags,
    pub(crate) kind_states: Vec<KindObservation>,
}

/// Full-server protocol observation used by management discovery.
#[derive(Debug)]
pub(crate) struct CapabilityFullDiscoveryObservation {
    pub(crate) tools: Vec<rmcp::model::Tool>,
    pub(crate) resources: Vec<rmcp::model::Resource>,
    pub(crate) prompts: Vec<rmcp::model::Prompt>,
    pub(crate) templates: Vec<rmcp::model::ResourceTemplate>,
    pub(crate) flags: CapSyncFlags,
    pub(crate) kind_states: Vec<KindObservation>,
    pub(crate) failures: Vec<CapabilityKindFailure>,
}

#[derive(Debug, Clone)]
pub(crate) struct CapabilityKindFailure {
    pub(crate) kind: CapabilityType,
    pub(crate) failure: RuntimeFailure,
}

pub(crate) enum CatalogReadFailure {
    Catalog(CatalogError),
    Operation(anyhow::Error),
}

/// Evidence that changes whether a persisted capability snapshot may be exposed.
#[derive(Clone, Debug)]
pub enum CapabilityEvidence {
    RuntimeFailure {
        server_id: String,
        kind: CatalogKind,
        reason: String,
    },
    Invalidated {
        server_id: String,
        reason: String,
    },
}

/// Stable boundary for recording runtime evidence without coupling the store to the pool.
#[async_trait]
pub trait CapabilityEvidenceSink: Send + Sync {
    async fn record(
        &self,
        evidence: CapabilityEvidence,
    ) -> mcpmate_capability_store::Result<CatalogCommit>;
}

#[derive(Clone)]
pub struct SqliteCapabilityEvidenceSink {
    catalog: SqliteCapabilityCatalog,
}

impl SqliteCapabilityEvidenceSink {
    pub fn new(catalog: SqliteCapabilityCatalog) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl CapabilityEvidenceSink for SqliteCapabilityEvidenceSink {
    async fn record(
        &self,
        evidence: CapabilityEvidence,
    ) -> mcpmate_capability_store::Result<CatalogCommit> {
        match evidence {
            CapabilityEvidence::RuntimeFailure {
                server_id,
                kind,
                reason,
            } => self.catalog.record_failure(&server_id, Some(kind), &reason).await,
            CapabilityEvidence::Invalidated { server_id, reason } => {
                self.catalog.invalidate_server(&server_id, &reason).await
            }
        }
    }
}

pub(crate) async fn handle_runtime_failure(
    pool: &Arc<Mutex<UpstreamConnectionPool>>,
    server_id: &str,
    instance_id: &str,
    failure: RuntimeFailure,
) {
    let message = failure.message.clone();
    let failure_kind = match failure.kind {
        RuntimeFailureKind::Timeout => FailureKind::RuntimeTimeout,
        RuntimeFailureKind::SessionGone | RuntimeFailureKind::TransportClosed | RuntimeFailureKind::StaleGeneration => {
            FailureKind::RuntimeGone
        }
        RuntimeFailureKind::Authentication
        | RuntimeFailureKind::AuthRequired
        | RuntimeFailureKind::Unauthorized
        | RuntimeFailureKind::Forbidden
        | RuntimeFailureKind::InsufficientScope
        | RuntimeFailureKind::Protocol
        | RuntimeFailureKind::Application
        | RuntimeFailureKind::Other => FailureKind::RuntimeOther,
    };
    let mut pool_guard = pool.lock().await;
    let _ = pool_guard.register_failure(server_id, failure_kind, message);
    // Only tear down the connection for session-gone errors to avoid penalizing transient timeouts
    if matches!(
        failure.kind,
        RuntimeFailureKind::SessionGone | RuntimeFailureKind::TransportClosed
    ) {
        let _ = pool_guard.disconnect_non_blocking(server_id, instance_id).await;
    }
}

fn runtime_failure_from_capability(failure: Option<CapabilityFetchFailure>) -> Option<RuntimeFailure> {
    failure.map(|f| match f {
        CapabilityFetchFailure::Timeout { timeout_ms } => RuntimeFailure {
            kind: RuntimeFailureKind::Timeout,
            message: None,
            timeout_ms: Some(timeout_ms),
        },
        CapabilityFetchFailure::TransportClosed => RuntimeFailure {
            kind: RuntimeFailureKind::TransportClosed,
            message: Some("transport closed".to_string()),
            timeout_ms: None,
        },
        CapabilityFetchFailure::Unsupported { message } => RuntimeFailure {
            kind: RuntimeFailureKind::Protocol,
            message: Some(message),
            timeout_ms: None,
        },
        CapabilityFetchFailure::AuthRequired { message } => RuntimeFailure {
            kind: RuntimeFailureKind::AuthRequired,
            message: Some(message),
            timeout_ms: None,
        },
        CapabilityFetchFailure::InsufficientScope { message } => RuntimeFailure {
            kind: RuntimeFailureKind::InsufficientScope,
            message: Some(message),
            timeout_ms: None,
        },
        CapabilityFetchFailure::Other { message } => RuntimeFailure {
            kind: RuntimeFailureKind::Other,
            message: Some(message),
            timeout_ms: None,
        },
    })
}

/// Execute a tool call using the shared runtime pipeline.
pub async fn call_tool(
    ctx: &CallCtx,
    pool: &Arc<Mutex<UpstreamConnectionPool>>,
) -> Result<rmcp::model::CallToolResult> {
    call_tool_impl(ctx, pool).await
}

pub(crate) async fn try_catalog_read(
    ctx: &ListCtx,
    database: &Arc<Database>,
) -> std::result::Result<Option<ListResult>, CatalogReadFailure> {
    try_catalog_read_with_hook(ctx, database, || async {}).await
}

pub(crate) async fn persisted_kind_failure(
    ctx: &ListCtx,
    database: &Arc<Database>,
) -> std::result::Result<Option<RuntimeFailure>, CatalogReadFailure> {
    let (snapshot, _) = database
        .load_capability_snapshot_typed(&ctx.server_id)
        .await
        .map_err(CatalogReadFailure::Catalog)?;
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    if snapshot.state == SnapshotState::Invalidated {
        return Ok(None);
    }
    let kind = catalog_kind(ctx.capability);
    let Some(state) = snapshot
        .kind_states
        .iter()
        .find(|state| state.kind == kind && state.inventory == InventoryState::Failed)
    else {
        return Ok(None);
    };
    Ok(Some(RuntimeFailure {
        kind: state
            .failure_kind
            .clone()
            .map(RuntimeFailureKind::from_persisted)
            .unwrap_or(RuntimeFailureKind::Other),
        message: state.error.clone(),
        timeout_ms: state.timeout_ms.map(u128::from),
    }))
}

/// Records a persisted snapshot as untrusted, invalidates the memory cache, and publishes the
/// resulting catalog revision so other readers observe the same fact. Shared by every cache-read
/// integrity check (config drift, shadow index divergence, ...) so each check only needs to
/// decide *whether* the snapshot is untrusted, not how to react once it is.
async fn invalidate_untrusted_snapshot(
    database: &Arc<Database>,
    snapshot: &CatalogSnapshot,
    reason: String,
) -> std::result::Result<(), CatalogReadFailure> {
    let catalog = SqliteCapabilityCatalog::new(database.pool.clone());
    let commit = catalog
        .invalidate_server_if_current(
            &snapshot.server_id,
            snapshot.revision,
            &snapshot.config_fingerprint,
            &reason,
        )
        .await
        .map_err(CatalogReadFailure::Catalog)?;
    if !commit.changed {
        return Ok(());
    }
    database.capability_cache.invalidate_server(&snapshot.server_id).await;
    crate::config::server::capabilities::publish_catalog_commit(
        &snapshot.server_id,
        &snapshot.server_name,
        commit.revision,
    );
    Ok(())
}

async fn try_catalog_read_with_hook<F, Fut>(
    ctx: &ListCtx,
    database: &Arc<Database>,
    after_snapshot_load: F,
) -> std::result::Result<Option<ListResult>, CatalogReadFailure>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let start = std::time::Instant::now();
    let projection_epoch = database.capability_cache.projection_epoch();
    let (snapshot, raw_memory_hit) = database
        .load_capability_snapshot_typed(&ctx.server_id)
        .await
        .map_err(CatalogReadFailure::Catalog)?;
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    after_snapshot_load().await;
    let current_fingerprint =
        crate::config::server::capabilities::current_config_fingerprint(&database.pool, &ctx.server_id)
            .await
            .map_err(CatalogReadFailure::Operation)?;
    if snapshot.config_fingerprint != current_fingerprint {
        invalidate_untrusted_snapshot(
            database,
            &snapshot,
            "server configuration fingerprint changed".to_string(),
        )
        .await?;
        return Ok(None);
    }
    if snapshot.state != SnapshotState::Ready {
        return Ok(None);
    }
    let kind = catalog_kind(ctx.capability);
    let Some(state) = snapshot.kind_states.iter().find(|state| state.kind == kind) else {
        return Ok(None);
    };
    if state.inventory != InventoryState::Complete {
        return Ok(None);
    }
    let shadow_index_trustworthy = crate::config::server::capabilities::shadow_index_matches_catalog_kind(
        &database.pool,
        &ctx.server_id,
        kind,
        &snapshot.records,
    )
    .await
    .map_err(CatalogReadFailure::Operation)?;
    if !shadow_index_trustworthy {
        invalidate_untrusted_snapshot(
            database,
            &snapshot,
            format!("shadow index for {kind:?} diverged from the catalog snapshot"),
        )
        .await?;
        return Ok(None);
    }

    let projected_from_sqlite = Arc::new(AtomicBool::new(false));
    let projector_flag = projected_from_sqlite.clone();
    let visibility_snapshot = ctx.visibility_snapshot.clone();
    let raw_items = items_from_catalog(ctx.capability, snapshot.records.clone(), ctx.name_domain);
    let projected = database
        .capability_cache
        .get_or_project_at_epoch(
            projection_key(ctx, snapshot.revision),
            projection_epoch,
            || async move {
                projector_flag.store(true, Ordering::Relaxed);
                let items = apply_visibility_filter(raw_items, visibility_snapshot.as_deref());
                Ok::<_, anyhow::Error>(projection_payload(items))
            },
        )
        .await
        .map_err(CatalogReadFailure::Operation)?;
    let memory_hit = raw_memory_hit && !projected_from_sqlite.load(Ordering::Relaxed);
    Ok(Some(ListResult {
        items: items_from_projection(&projected),
        meta: Meta {
            cache_hit: true,
            source: if memory_hit { "memory_cache" } else { "sqlite_catalog" }.to_string(),
            duration_ms: start.elapsed().as_millis() as u64,
            had_peer: false,
        },
    }))
}

pub(crate) async fn discover_owner(
    ctx: &ListCtx,
    owner: &CapabilityOwner,
) -> std::result::Result<CapabilityDiscoveryObservation, RuntimeFailure> {
    if owner.server_id != ctx.server_id {
        return Err(RuntimeFailure {
            kind: RuntimeFailureKind::Other,
            message: Some(format!(
                "owner targets server '{}' instead of '{}'",
                owner.server_id, ctx.server_id
            )),
            timeout_ms: None,
        });
    }
    if ctx.capability == CapabilityType::Tools
        && let Some(tools) = owner.startup_tools.as_ref()
    {
        return Ok(CapabilityDiscoveryObservation {
            items: CapabilityItems::Tools(tools.clone()),
            flags: CapSyncFlags::TOOLS,
            kind_states: Vec::new(),
        });
    }
    let (items, flags, failure, kind_states) = fetch_runtime_items(
        ctx.capability,
        owner.peer.clone(),
        ctx.operation_timeout,
        &owner.server_id,
        &owner.server_name,
        &owner.instance_id,
    )
    .await
    .map_err(|error| RuntimeFailure {
        kind: RuntimeFailureKind::Other,
        message: Some(error.to_string()),
        timeout_ms: None,
    })?;
    match failure {
        Some(failure) => Err(failure),
        None => Ok(CapabilityDiscoveryObservation {
            items,
            flags,
            kind_states,
        }),
    }
}

pub(crate) async fn discover_all_kinds_owner(
    ctx: &ListCtx,
    owner: &CapabilityOwner,
) -> std::result::Result<CapabilityFullDiscoveryObservation, RuntimeFailure> {
    if owner.server_id != ctx.server_id {
        return Err(RuntimeFailure {
            kind: RuntimeFailureKind::Other,
            message: Some(format!(
                "owner targets server '{}' instead of '{}'",
                owner.server_id, ctx.server_id
            )),
            timeout_ms: None,
        });
    }

    let timeout = ctx.operation_timeout;
    let mut tools = Vec::new();
    let mut resources = Vec::new();
    let mut prompts = Vec::new();
    let mut templates = Vec::new();
    let mut flags = CapSyncFlags::NONE;
    let mut kind_states = Vec::new();
    let mut failures = Vec::new();
    let declared_capabilities = owner.peer.peer_info().map(|info| info.capabilities.clone());

    for capability in [
        CapabilityType::Tools,
        CapabilityType::Prompts,
        CapabilityType::Resources,
        CapabilityType::ResourceTemplates,
    ] {
        let declared = match capability {
            CapabilityType::Tools => declared_capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.tools.as_ref())
                .is_some(),
            CapabilityType::Prompts => declared_capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.prompts.as_ref())
                .is_some(),
            CapabilityType::Resources | CapabilityType::ResourceTemplates => declared_capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.resources.as_ref())
                .is_some(),
        };
        if !declared {
            let kind = catalog_kind(capability);
            kind_states.push(crate::config::server::capabilities::unsupported_complete_observation(
                kind,
            ));
            let capability_flags = match capability {
                CapabilityType::Tools => CapSyncFlags::TOOLS,
                CapabilityType::Prompts => CapSyncFlags::PROMPTS,
                CapabilityType::Resources => CapSyncFlags::RESOURCES,
                CapabilityType::ResourceTemplates => CapSyncFlags::RESOURCE_TEMPLATES,
            };
            flags = flags.union(capability_flags);
            continue;
        }
        if capability == CapabilityType::Tools
            && let Some(startup_tools) = owner.startup_tools.as_ref()
        {
            tools = startup_tools.clone();
            flags = flags.union(CapSyncFlags::TOOLS);
            continue;
        }
        let (items, capability_flags, failure, capability_states) = fetch_runtime_items(
            capability,
            owner.peer.clone(),
            timeout,
            &owner.server_id,
            &owner.server_name,
            &owner.instance_id,
        )
        .await
        .map_err(|error| RuntimeFailure {
            kind: RuntimeFailureKind::Other,
            message: Some(error.to_string()),
            timeout_ms: None,
        })?;
        if let Some(failure) = failure {
            let kind = catalog_kind(capability);
            let reason = format!(
                "server_id={} server_name={} kinds=[{}] instance={:?} generation={:?} reason={}",
                owner.server_id,
                owner.server_name,
                kind.as_str(),
                Some(&owner.instance_id),
                owner.connection_generation,
                failure
            );
            tracing::debug!(
                server_id = %owner.server_id,
                capability = ?capability,
                error = %failure,
                "Full discovery recorded a failing capability kind and continued"
            );
            kind_states.push(
                KindObservation::new(
                    kind,
                    mcpmate_capability_store::DeclarationState::Supported,
                    InventoryState::Failed,
                )
                .with_failure(
                    failure.kind.persisted(),
                    reason,
                    failure.timeout_ms.and_then(|timeout| u64::try_from(timeout).ok()),
                ),
            );
            failures.push(CapabilityKindFailure {
                kind: capability,
                failure,
            });
            flags = flags.union(capability_flags);
            continue;
        }
        flags = flags.union(capability_flags);
        kind_states.extend(capability_states);
        match items {
            CapabilityItems::Tools(items) => tools = items,
            CapabilityItems::Resources(items) => resources = items,
            CapabilityItems::Prompts(items) => prompts = items,
            CapabilityItems::ResourceTemplates(items) => templates = items,
        }
    }

    Ok(CapabilityFullDiscoveryObservation {
        tools,
        resources,
        prompts,
        templates,
        flags,
        kind_states,
        failures,
    })
}

pub(crate) async fn commit_full_discovery_observation(
    owner: &CapabilityOwner,
    observation: &CapabilityFullDiscoveryObservation,
    database: &Arc<Database>,
) -> Result<i64> {
    let commit = crate::config::server::capabilities::commit_capability_protocol_observation(
        &database.pool,
        database.capability_cache.as_ref(),
        &owner.server_id,
        &owner.server_name,
        &owner.config_fingerprint,
        crate::config::server::capabilities::CapabilityProtocolObservation {
            initialize: crate::core::transport::client::legacy_initialize_result(owner.peer.peer_info().as_deref())?,
            tools: observation.tools.clone(),
            resources: observation.resources.clone(),
            prompts: observation.prompts.clone(),
            templates: observation.templates.clone(),
            kinds: observation.flags,
            kind_states: observation.kind_states.clone(),
        },
    )
    .await
    .context("Failed to commit full live capability observation")?;
    tracing::debug!(
        server_id = %owner.server_id,
        instance_id = %owner.instance_id,
        owner_source = ?owner.source,
        revision = commit.revision,
        "Committed a full live capability observation"
    );
    Ok(commit.revision)
}

pub(crate) async fn commit_discovery_observation(
    owner: &CapabilityOwner,
    observation: &CapabilityDiscoveryObservation,
    database: &Arc<Database>,
) -> Result<i64> {
    let (tools, resources, prompts, templates) = protocol_vectors(&observation.items);
    let commit = crate::config::server::capabilities::commit_capability_protocol_observation(
        &database.pool,
        database.capability_cache.as_ref(),
        &owner.server_id,
        &owner.server_name,
        &owner.config_fingerprint,
        crate::config::server::capabilities::CapabilityProtocolObservation {
            initialize: crate::core::transport::client::legacy_initialize_result(owner.peer.peer_info().as_deref())?,
            tools,
            resources,
            prompts,
            templates,
            kinds: observation.flags,
            kind_states: observation.kind_states.clone(),
        },
    )
    .await
    .context("Failed to commit live capability observation")?;
    tracing::debug!(
        server_id = %owner.server_id,
        instance_id = %owner.instance_id,
        owner_source = ?owner.source,
        revision = commit.revision,
        "Committed a live capability observation"
    );
    Ok(commit.revision)
}

pub(crate) async fn project_discovery_observation(
    ctx: &ListCtx,
    owner: &CapabilityOwner,
    items: CapabilityItems,
    committed_revision: i64,
    database: &Arc<Database>,
) -> Result<ListResult> {
    let projection_epoch = database.capability_cache.projection_epoch();
    let (snapshot, _) = database
        .load_capability_snapshot_typed(&owner.server_id)
        .await
        .context("Failed to warm the committed capability snapshot")?;
    let snapshot = snapshot.context("Committed capability snapshot is missing")?;
    let projected = if snapshot.revision == committed_revision {
        let projection_database = database.clone();
        let projection_server_id = owner.server_id.clone();
        let projection_name_domain = ctx.name_domain;
        let visibility_snapshot = ctx.visibility_snapshot.clone();
        database
            .capability_cache
            .get_or_project_at_epoch(
                projection_key(ctx, committed_revision),
                projection_epoch,
                || async move {
                    let items = project_live_items_for_context(
                        &projection_database,
                        &projection_server_id,
                        items,
                        projection_name_domain,
                        visibility_snapshot.as_deref(),
                    )
                    .await?;
                    Ok::<_, anyhow::Error>(projection_payload(items))
                },
            )
            .await?
    } else {
        tracing::debug!(
            server_id = %owner.server_id,
            committed_revision,
            current_revision = snapshot.revision,
            "Skipped live projection cache warm because a newer catalog revision won"
        );
        Arc::new(projection_payload(
            project_live_items_for_context(
                database,
                &owner.server_id,
                items,
                ctx.name_domain,
                ctx.visibility_snapshot.as_deref(),
            )
            .await?,
        ))
    };
    tracing::debug!(
        server_id = %owner.server_id,
        instance_id = %owner.instance_id,
        owner_source = ?owner.source,
        committed_revision,
        current_revision = snapshot.revision,
        "Projected and cached a live capability observation"
    );
    Ok(ListResult {
        items: items_from_projection(&projected),
        meta: Meta {
            cache_hit: false,
            source: "live".to_string(),
            duration_ms: 0,
            had_peer: true,
        },
    })
}

pub(crate) async fn record_discovery_failure(
    ctx: &ListCtx,
    server_name: &str,
    instance_id: Option<&str>,
    connection_generation: Option<u64>,
    reason: &str,
    failure: Option<&RuntimeFailure>,
    database: &Arc<Database>,
) -> mcpmate_capability_store::Result<()> {
    record_discovery_failures(
        ctx,
        server_name,
        &[ctx.capability],
        instance_id,
        connection_generation,
        reason,
        failure,
        database,
    )
    .await
}

pub(crate) async fn record_discovery_failures(
    ctx: &ListCtx,
    _server_name: &str,
    kinds: &[CapabilityType],
    instance_id: Option<&str>,
    connection_generation: Option<u64>,
    reason: &str,
    failure: Option<&RuntimeFailure>,
    database: &Arc<Database>,
) -> mcpmate_capability_store::Result<()> {
    let catalog_kinds = kinds.iter().copied().map(catalog_kind).collect::<Vec<_>>();
    crate::config::server::capabilities::record_capability_failures(
        &database.pool,
        database.capability_cache.as_ref(),
        &catalog_kinds,
        crate::config::server::capabilities::CapabilityFailureEvidence {
            server_id: ctx.server_id.clone(),
            kind: catalog_kinds[0],
            instance_id: instance_id.map(ToOwned::to_owned),
            connection_generation,
            failure_kind: failure.map(|failure| failure.kind.persisted()),
            timeout_ms: failure
                .and_then(|failure| failure.timeout_ms.and_then(|timeout_ms| u64::try_from(timeout_ms).ok())),
            reason: reason.to_string(),
        },
    )
    .await?;
    Ok(())
}

/// Feeds capability-negating evidence observed while executing `tools/call`, `resources/read`,
/// or `prompts/get` back into the capability catalog, so a stale Ready declaration is not kept
/// serving a capability whose upstream session or transport has actually gone away.
///
/// Only transport-level session-gone/closed evidence qualifies here. Ordinary business or
/// parameter errors returned by the tool/resource/prompt itself must never reach this path,
/// per the correction design's explicit rule against escalating routine failures to a
/// whole-catalog invalidation.
pub(crate) async fn record_capability_usage_evidence(
    database: &Arc<Database>,
    server_id: &str,
    kind: CatalogKind,
    instance_id: Option<&str>,
    error: &anyhow::Error,
) {
    let Some(service_error) = error
        .chain()
        .find_map(|source| source.downcast_ref::<rmcp::service::ServiceError>())
    else {
        return;
    };
    if !matches!(service_error, rmcp::service::ServiceError::TransportClosed) {
        return;
    }

    let reason = error.to_string();
    let outcome = crate::config::server::capabilities::record_capability_failure(
        &database.pool,
        database.capability_cache.as_ref(),
        crate::config::server::capabilities::CapabilityFailureEvidence {
            server_id: server_id.to_string(),
            kind,
            instance_id: instance_id.map(ToOwned::to_owned),
            connection_generation: None,
            failure_kind: Some(KindFailureKind::TransportClosed),
            timeout_ms: None,
            reason,
        },
    )
    .await;
    if let Err(error) = outcome {
        tracing::warn!(
            server_id,
            ?kind,
            %error,
            "failed to record capability usage evidence for a closed transport"
        );
    }
}

fn protocol_vectors(
    items: &CapabilityItems
) -> (
    Vec<rmcp::model::Tool>,
    Vec<rmcp::model::Resource>,
    Vec<rmcp::model::Prompt>,
    Vec<rmcp::model::ResourceTemplate>,
) {
    match items {
        CapabilityItems::Tools(items) => (items.clone(), Vec::new(), Vec::new(), Vec::new()),
        CapabilityItems::Resources(items) => (Vec::new(), items.clone(), Vec::new(), Vec::new()),
        CapabilityItems::Prompts(items) => (Vec::new(), Vec::new(), items.clone(), Vec::new()),
        CapabilityItems::ResourceTemplates(items) => (Vec::new(), Vec::new(), Vec::new(), items.clone()),
    }
}

async fn fetch_runtime_items(
    capability: CapabilityType,
    peer: Peer<RoleClient>,
    timeout: Duration,
    server_id: &str,
    server_name: &str,
    instance_id: &str,
) -> Result<(
    CapabilityItems,
    CapSyncFlags,
    Option<RuntimeFailure>,
    Vec<mcpmate_capability_store::KindObservation>,
)> {
    match capability {
        CapabilityType::Tools => {
            let fetch_page =
                move |p: Peer<RoleClient>,
                      cursor: Option<String>|
                      -> BoxFuture<'static, anyhow::Result<(Vec<rmcp::model::Tool>, Option<String>)>> {
                    Box::pin(async move {
                        let result = p
                            .list_tools(Some(rmcp::model::PaginatedRequestParams::default().with_cursor(cursor)))
                            .await?;
                        Ok((result.tools, result.next_cursor))
                    })
                };
            let out = collect_capability_from_instance_peer(
                peer,
                timeout,
                fetch_page,
                |tool, _s, _i, _n| tool,
                server_id,
                server_name,
                instance_id,
                is_method_not_supported,
            )
            .await;
            if matches!(&out.failure, Some(CapabilityFetchFailure::Unsupported { .. })) {
                return Ok((
                    CapabilityItems::Tools(Vec::new()),
                    CapSyncFlags::TOOLS,
                    None,
                    vec![crate::config::server::capabilities::unsupported_complete_observation(
                        CatalogKind::Tools,
                    )],
                ));
            }
            let runtime_failure = runtime_failure_from_capability(out.failure);
            if runtime_failure.is_some() {
                return Ok((
                    CapabilityItems::Tools(Vec::new()),
                    CapSyncFlags::TOOLS,
                    runtime_failure,
                    Vec::new(),
                ));
            }
            Ok((
                CapabilityItems::Tools(out.items),
                CapSyncFlags::TOOLS,
                runtime_failure,
                Vec::new(),
            ))
        }
        CapabilityType::Prompts => {
            let fetch_page = move |p: Peer<RoleClient>,
                                   cursor: Option<String>|
                  -> BoxFuture<
                'static,
                anyhow::Result<(Vec<rmcp::model::Prompt>, Option<String>)>,
            > {
                Box::pin(async move {
                    let result = p
                        .list_prompts(Some(rmcp::model::PaginatedRequestParams::default().with_cursor(cursor)))
                        .await?;
                    Ok((result.prompts, result.next_cursor))
                })
            };
            let out = collect_capability_from_instance_peer(
                peer,
                timeout,
                fetch_page,
                |prompt, _s, _i, _n| prompt,
                server_id,
                server_name,
                instance_id,
                is_method_not_supported,
            )
            .await;
            if matches!(&out.failure, Some(CapabilityFetchFailure::Unsupported { .. })) {
                return Ok((
                    CapabilityItems::Prompts(Vec::new()),
                    CapSyncFlags::PROMPTS,
                    None,
                    vec![crate::config::server::capabilities::unsupported_complete_observation(
                        CatalogKind::Prompts,
                    )],
                ));
            }
            let runtime_failure = runtime_failure_from_capability(out.failure);
            if runtime_failure.is_some() {
                return Ok((
                    CapabilityItems::Prompts(Vec::new()),
                    CapSyncFlags::PROMPTS,
                    runtime_failure,
                    Vec::new(),
                ));
            }
            Ok((
                CapabilityItems::Prompts(out.items),
                CapSyncFlags::PROMPTS,
                None,
                Vec::new(),
            ))
        }
        CapabilityType::Resources => {
            let fetch_page = move |p: Peer<RoleClient>,
                                   cursor: Option<String>|
                  -> BoxFuture<
                'static,
                anyhow::Result<(Vec<rmcp::model::Resource>, Option<String>)>,
            > {
                Box::pin(async move {
                    let result = p
                        .list_resources(Some(rmcp::model::PaginatedRequestParams::default().with_cursor(cursor)))
                        .await?;
                    Ok((result.resources, result.next_cursor))
                })
            };
            let out = collect_capability_from_instance_peer(
                peer,
                timeout,
                fetch_page,
                |resource, _s, _i, _n| resource,
                server_id,
                server_name,
                instance_id,
                is_method_not_supported,
            )
            .await;
            if matches!(&out.failure, Some(CapabilityFetchFailure::Unsupported { .. })) {
                return Ok((
                    CapabilityItems::Resources(Vec::new()),
                    CapSyncFlags::RESOURCES,
                    None,
                    vec![crate::config::server::capabilities::unsupported_complete_observation(
                        CatalogKind::Resources,
                    )],
                ));
            }
            let runtime_failure = runtime_failure_from_capability(out.failure);
            if runtime_failure.is_some() {
                return Ok((
                    CapabilityItems::Resources(Vec::new()),
                    CapSyncFlags::RESOURCES,
                    runtime_failure,
                    Vec::new(),
                ));
            }
            Ok((
                CapabilityItems::Resources(out.items),
                CapSyncFlags::RESOURCES,
                None,
                Vec::new(),
            ))
        }
        CapabilityType::ResourceTemplates => {
            let fetch_page = move |p: Peer<RoleClient>,
                                   cursor: Option<String>|
                  -> BoxFuture<
                'static,
                anyhow::Result<(Vec<rmcp::model::ResourceTemplate>, Option<String>)>,
            > {
                Box::pin(async move {
                    let result = p
                        .list_resource_templates(Some(
                            rmcp::model::PaginatedRequestParams::default().with_cursor(cursor),
                        ))
                        .await?;
                    Ok((result.resource_templates, result.next_cursor))
                })
            };
            let out = collect_capability_from_instance_peer(
                peer,
                timeout,
                fetch_page,
                |template, _s, _i, _n| template,
                server_id,
                server_name,
                instance_id,
                is_method_not_supported,
            )
            .await;
            if matches!(&out.failure, Some(CapabilityFetchFailure::Unsupported { .. })) {
                return Ok((
                    CapabilityItems::ResourceTemplates(Vec::new()),
                    CapSyncFlags::RESOURCE_TEMPLATES,
                    None,
                    vec![crate::config::server::capabilities::unsupported_complete_observation(
                        CatalogKind::ResourceTemplates,
                    )],
                ));
            }
            let runtime_failure = runtime_failure_from_capability(out.failure);
            if runtime_failure.is_some() {
                return Ok((
                    CapabilityItems::ResourceTemplates(Vec::new()),
                    CapSyncFlags::RESOURCE_TEMPLATES,
                    runtime_failure,
                    Vec::new(),
                ));
            }
            Ok((
                CapabilityItems::ResourceTemplates(out.items),
                CapSyncFlags::RESOURCE_TEMPLATES,
                None,
                Vec::new(),
            ))
        }
    }
}

async fn call_tool_impl(
    ctx: &CallCtx,
    pool: &Arc<Mutex<UpstreamConnectionPool>>,
) -> Result<rmcp::model::CallToolResult> {
    let timeout = ctx.timeout.unwrap_or_else(|| Duration::from_secs(60));
    let t0 = std::time::Instant::now();

    let fetch_peer = || async {
        let pool_guard = pool.lock().await;
        let snap = pool_guard.get_snapshot();
        if let Some(selection) = ctx.connection_selection.as_ref() {
            if let Ok(Some(selected_instance_id)) = pool_guard.select_ready_instance_id(selection) {
                if let Some(instances) = snap.get(&ctx.server_id) {
                    if let Some((instance_id, status, _res, _prm, peer)) =
                        instances.iter().find(|(candidate_id, st, _, _, p)| {
                            **candidate_id == selected_instance_id
                                && matches!(st, crate::core::foundation::types::ConnectionStatus::Ready)
                                && p.is_some()
                        })
                    {
                        return (peer.clone(), Some(instance_id.clone()), Some(status.clone()));
                    }
                }
            }
        }
        if let Some(instances) = snap.get(&ctx.server_id) {
            if let Some((instance_id, status, _res, _prm, peer)) = instances.iter().find(|(_, st, _, _, p)| {
                matches!(st, crate::core::foundation::types::ConnectionStatus::Ready) && p.is_some()
            }) {
                return (peer.clone(), Some(instance_id.clone()), Some(status.clone()));
            }
        }
        (None, None, None)
    };

    let t_fetch_begin = std::time::Instant::now();
    let mut peer_info = fetch_peer().await;
    let t_fetch_ms = t_fetch_begin.elapsed().as_millis();
    if peer_info.0.is_none() {
        let t_connect_begin = std::time::Instant::now();
        if let Some(selection) = ctx.connection_selection.as_ref() {
            UpstreamConnectionPool::ensure_connected_coordinated(pool, selection).await?;
        } else {
            let selection = crate::core::capability::ConnectionSelection {
                server_id: ctx.server_id.clone(),
                affinity_key: crate::core::capability::AffinityKey::Default,
            };
            UpstreamConnectionPool::ensure_connected_coordinated(pool, &selection).await?;
        }
        peer_info = fetch_peer().await;
        tracing::debug!(
            server_id = %ctx.server_id,
            tool = %ctx.tool_name,
            fetch_ms = %t_fetch_ms,
            ensure_connected_ms = %t_connect_begin.elapsed().as_millis(),
            "[CALL] ensured connection before tool call"
        );
    } else {
        tracing::debug!(
            server_id = %ctx.server_id,
            tool = %ctx.tool_name,
            fetch_ms = %t_fetch_ms,
            "[CALL] found ready peer in snapshot"
        );
    }

    let (peer_opt, selected_instance, _selected_status) = peer_info;
    let peer: Peer<RoleClient> =
        peer_opt.ok_or_else(|| anyhow::anyhow!("No ready instance for server {}", ctx.server_id))?;

    if let Some(instance_id) = selected_instance.as_ref() {
        let mut pool_guard = pool.lock().await;
        pool_guard.mark_instance_activity(&ctx.server_id, instance_id);
    }

    let selected_instance_id = selected_instance.clone();
    let tool_name = ctx.tool_name.clone();
    let arguments = ctx.arguments.clone();
    let fut = async move {
        tracing::debug!(server_id = %ctx.server_id, tool = %ctx.tool_name, timeout_secs = %timeout.as_secs(), "[CALL] sending to upstream");
        {
            let mut params = rmcp::model::CallToolRequestParams::new(tool_name);
            if let Some(arguments) = arguments {
                params = params.with_arguments(arguments);
            }
            peer.call_tool(params)
        }
        .await
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(res)) => {
            if selected_instance_id.is_some() {
                let mut pool = pool.lock().await;
                pool.clear_failure_state(&ctx.server_id);
                if let Some(instance_id) = selected_instance_id.as_ref() {
                    pool.mark_instance_activity(&ctx.server_id, instance_id);
                }
            }
            tracing::info!(
                server_id = %ctx.server_id,
                tool = %ctx.tool_name,
                elapsed_ms = %t0.elapsed().as_millis(),
                "[CALL] upstream call succeeded"
            );
            Ok(res)
        }
        Ok(Err(e)) => {
            let error_msg = e.to_string();
            if let Some(instance_id) = selected_instance_id.as_deref() {
                let kind = match &e {
                    rmcp::service::ServiceError::TransportClosed => RuntimeFailureKind::TransportClosed,
                    rmcp::service::ServiceError::Timeout { .. } => RuntimeFailureKind::Timeout,
                    rmcp::service::ServiceError::McpError(_) => RuntimeFailureKind::Application,
                    _ => RuntimeFailureKind::Other,
                };
                handle_runtime_failure(
                    pool,
                    &ctx.server_id,
                    instance_id,
                    RuntimeFailure {
                        kind,
                        message: Some(error_msg.clone()),
                        timeout_ms: None,
                    },
                )
                .await;
            }
            tracing::error!(
                server_id = %ctx.server_id,
                tool = %ctx.tool_name,
                elapsed_ms = %t0.elapsed().as_millis(),
                error = %error_msg,
                "[CALL] upstream call failed"
            );
            Err(anyhow::Error::new(e).context("Tool call failed"))
        }
        Err(_) => {
            // TODO(mcpmate): revisit after RMCP fixes SSE stream recovery (see GitMCP long-tail investigation).
            if let Some(instance_id) = selected_instance_id.as_deref() {
                handle_runtime_failure(
                    pool,
                    &ctx.server_id,
                    instance_id,
                    RuntimeFailure {
                        kind: RuntimeFailureKind::Timeout,
                        message: Some(format!("Tool call timed out after {:.1}s", timeout.as_secs_f32())),
                        timeout_ms: Some(timeout.as_millis()),
                    },
                )
                .await;
            }
            tracing::error!(
                server_id = %ctx.server_id,
                tool = %ctx.tool_name,
                elapsed_ms = %t0.elapsed().as_millis(),
                timeout_secs = %timeout.as_secs(),
                "[CALL] upstream call timeout"
            );
            Err(anyhow::anyhow!("Tool call timeout for server {}", ctx.server_id))
        }
    }
}

#[cfg(test)]
fn convert_cached_resource(cached: CachedResourceInfo) -> Option<rmcp::model::Resource> {
    let resolved_name = cached
        .name
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| cached.uri.clone());
    let mut resource = rmcp::model::Resource::new(cached.uri.clone(), resolved_name);
    resource.description = cached.description;
    resource.mime_type = cached.mime_type;
    resource.icons = cached.icons;
    Some(resource)
}

#[cfg(test)]
fn convert_cached_resource_template(cached: CachedResourceTemplateInfo) -> Option<rmcp::model::ResourceTemplate> {
    let resolved_name = cached
        .name
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| cached.uri_template.clone());
    let mut template = rmcp::model::ResourceTemplate::new(cached.uri_template, resolved_name);
    if let Some(description) = cached.description {
        template = template.with_description(description);
    }
    if let Some(mime_type) = cached.mime_type {
        template = template.with_mime_type(mime_type);
    }
    Some(template)
}

async fn project_external_items(
    database: &Arc<Database>,
    server_id: &str,
    mut items: CapabilityItems,
    name_domain: NameDomain,
) -> Result<CapabilityItems> {
    if name_domain == NameDomain::Upstream {
        return Ok(items);
    }

    match &mut items {
        CapabilityItems::Tools(tools) => {
            for tool in tools {
                tool.name = crate::core::capability::naming::load_external_identifier(
                    &database.pool,
                    NamingKind::Tool,
                    server_id,
                    &tool.name,
                )
                .await?
                .into();
            }
        }
        CapabilityItems::Prompts(prompts) => {
            for prompt in prompts {
                prompt.name = crate::core::capability::naming::load_external_identifier(
                    &database.pool,
                    NamingKind::Prompt,
                    server_id,
                    &prompt.name,
                )
                .await?;
            }
        }
        CapabilityItems::Resources(resources) => {
            for resource in resources {
                resource.uri = crate::core::capability::naming::load_external_identifier(
                    &database.pool,
                    NamingKind::Resource,
                    server_id,
                    &resource.uri,
                )
                .await?;
            }
        }
        CapabilityItems::ResourceTemplates(templates) => {
            let server_name = sqlx::query_scalar::<_, String>("SELECT name FROM server_config WHERE id = ?")
                .bind(server_id)
                .fetch_optional(&database.pool)
                .await
                .context("Failed to load server namespace for Resource Template projection")?
                .with_context(|| format!("Server '{server_id}' not found"))?;
            let mut projected = Vec::with_capacity(templates.len());
            for mut template in std::mem::take(templates) {
                let upstream_template = template.uri_template.to_string();
                if !crate::core::capability::resource_uri::resource_template_is_projectable(
                    &server_name,
                    &upstream_template,
                )? {
                    continue;
                }
                template.uri_template = crate::core::capability::naming::load_external_identifier(
                    &database.pool,
                    NamingKind::ResourceTemplate,
                    server_id,
                    &upstream_template,
                )
                .await?;
                projected.push(template);
            }
            *templates = projected;
        }
    }

    Ok(items)
}

async fn project_live_items_for_context(
    database: &Arc<Database>,
    server_id: &str,
    items: CapabilityItems,
    name_domain: NameDomain,
    visibility_snapshot: Option<&crate::core::profile::visibility::VisibilitySnapshot>,
) -> Result<CapabilityItems> {
    Ok(apply_visibility_filter(
        project_external_items(database, server_id, items, name_domain).await?,
        visibility_snapshot,
    ))
}

fn apply_visibility_filter(
    items: CapabilityItems,
    visibility_snapshot: Option<&crate::core::profile::visibility::VisibilitySnapshot>,
) -> CapabilityItems {
    let Some(snapshot) = visibility_snapshot else {
        return items;
    };
    let visibility = crate::core::profile::visibility::ProfileVisibilityService::new(None, None);
    match items {
        CapabilityItems::Tools(items) => CapabilityItems::Tools(visibility.filter_tools_with_snapshot(snapshot, items)),
        CapabilityItems::Prompts(items) => {
            CapabilityItems::Prompts(visibility.filter_prompts_with_snapshot(snapshot, items))
        }
        CapabilityItems::Resources(items) => {
            let (resources, _) = visibility.filter_resources_with_snapshot(snapshot, items, Vec::new());
            CapabilityItems::Resources(resources)
        }
        CapabilityItems::ResourceTemplates(items) => {
            let (_, templates) = visibility.filter_resources_with_snapshot(snapshot, Vec::new(), items);
            CapabilityItems::ResourceTemplates(templates)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        models::{Server, ServerTransportDraft},
        server::upsert_server_definition,
    };
    use crate::core::capability::index::{CachedResourceInfo, CachedResourceTemplateInfo};
    use crate::core::models::Config;
    use chrono::Utc;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::{path::PathBuf, str::FromStr};
    use tempfile::TempDir;

    async fn list(
        ctx: &ListCtx,
        pool: &Arc<Mutex<UpstreamConnectionPool>>,
        database: &Arc<Database>,
    ) -> Result<ListResult> {
        crate::core::capability::read_service::CapabilityReadService::from_runtime(database.clone(), pool.clone())
            .list(ctx)
            .await
            .map_err(anyhow::Error::from)
    }

    async fn test_database() -> Arc<Database> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("initialize client tables");
        crate::config::profile::init::initialize_profile_tables(&pool)
            .await
            .expect("initialize profile tables");
        Arc::new(Database {
            pool,
            path: PathBuf::new(),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        })
    }

    #[test]
    fn insufficient_scope_fetch_preserves_runtime_and_persisted_failure_kinds() {
        let failure = runtime_failure_from_capability(Some(CapabilityFetchFailure::InsufficientScope {
            message: "scope tools.read is required".to_string(),
        }))
        .expect("fetch failure maps to a runtime failure");

        assert_eq!(failure.kind, RuntimeFailureKind::InsufficientScope);
        assert_eq!(failure.kind.persisted(), KindFailureKind::InsufficientScope);
        assert_eq!(
            failure.kind.authentication_code(),
            Some(CapabilityAuthenticationFailureCode::InsufficientScope)
        );
    }

    fn runtime_initialize_result() -> rmcp::model::InitializeResult {
        serde_json::from_value(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {"listChanged": true}},
            "serverInfo": {"name": "runtime-fixture", "version": "1.0.0"}
        }))
        .expect("fixture must match RMCP 2.2")
    }

    fn runtime_tool(name: &str) -> rmcp::model::Tool {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "description": "Runtime cache fixture",
            "inputSchema": {"type": "object"},
            "outputSchema": {"type": "object"},
            "_meta": {"fixture": "runtime-cache"}
        }))
        .expect("fixture must match RMCP 2.2")
    }

    async fn insert_stdio_server(
        pool: &sqlx::SqlitePool,
        server_id: &str,
        name: &str,
    ) {
        let mut server = Server::new_stdio(name.to_string(), Some("fixture-command".to_string()));
        server.id = Some(server_id.to_string());
        upsert_server_definition(
            pool,
            &server,
            &ServerTransportDraft::Stdio {
                command: Some("fixture-command".to_string()),
                args: Vec::new(),
                env: Default::default(),
            },
        )
        .await
        .expect("insert typed stdio server fixture");
    }

    async fn insert_runtime_server(database: &Database) {
        insert_stdio_server(&database.pool, "server-a", "runtime_fixture").await;
    }

    async fn commit_runtime_catalog(
        database: &Database,
        tools: Vec<rmcp::model::Tool>,
    ) {
        crate::config::server::capabilities::commit_protocol_items_for_kinds(
            &database.pool,
            "server-a",
            "runtime_fixture",
            Some(runtime_initialize_result()),
            tools,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            CapSyncFlags::ALL,
        )
        .await
        .expect("commit runtime catalog fixture");
    }

    fn empty_runtime_pool(database: Arc<Database>) -> Arc<Mutex<UpstreamConnectionPool>> {
        Arc::new(Mutex::new(UpstreamConnectionPool::new(
            Arc::new(Config::default()),
            Some(database),
        )))
    }

    fn list_ctx(refresh: RefreshStrategy) -> ListCtx {
        ListCtx {
            capability: CapabilityType::Tools,
            server_id: "server-a".to_string(),
            refresh: Some(refresh),
            operation_timeout: crate::core::transport::timeout_policy::DEFAULT_CAPABILITY_OPERATION_TIMEOUT,
            validation_session: None,
            runtime_identity: None,
            connection_selection: None,
            visibility_snapshot: None,
            name_domain: NameDomain::Upstream,
        }
    }

    #[tokio::test]
    async fn external_catalog_read_skips_identity_reprojection() {
        let database = test_database().await;
        insert_runtime_server(&database).await;
        commit_runtime_catalog(&database, vec![runtime_tool("analyze")]).await;

        let external: String = sqlx::query_scalar(
            "SELECT unique_name FROM server_tools WHERE server_id = 'server-a' AND tool_name = 'analyze'",
        )
        .fetch_one(&database.pool)
        .await
        .expect("external tool name");

        let mut ctx = list_ctx(RefreshStrategy::CacheFirst);
        ctx.name_domain = NameDomain::External;
        let pool = empty_runtime_pool(database.clone());

        let result = list(&ctx, &pool, &database)
            .await
            .expect("external catalog read must not re-project persisted payload names");
        let tools = result.items.into_tools().expect("tools result");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name.as_ref(), external);
        assert_ne!(tools[0].name.as_ref(), "analyze");
    }

    #[tokio::test]
    async fn cache_first_serves_ready_sqlite_catalog_without_a_peer() {
        let database = test_database().await;
        insert_runtime_server(&database).await;
        commit_runtime_catalog(&database, vec![runtime_tool("cached-tool")]).await;
        let pool = empty_runtime_pool(database.clone());

        let result = list(&list_ctx(RefreshStrategy::CacheFirst), &pool, &database)
            .await
            .expect("ready SQLite catalog must not require a peer");

        assert!(result.meta.cache_hit);
        assert_eq!(result.meta.source, "sqlite_catalog");
        assert!(!result.meta.had_peer);
        let tools = result.items.into_tools().expect("tools result");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name.as_ref(), "cached-tool");

        let memory_result = list(&list_ctx(RefreshStrategy::CacheFirst), &pool, &database)
            .await
            .expect("second request must reuse the in-process cache");
        assert!(memory_result.meta.cache_hit);
        assert_eq!(memory_result.meta.source, "memory_cache");
        assert!(!memory_result.meta.had_peer);
        assert_eq!(database.capability_cache.metrics().await.raw_loads, 1);
        assert_eq!(database.capability_cache.metrics().await.projection_loads, 1);
    }

    #[tokio::test]
    async fn cache_first_invalidation_after_snapshot_load_does_not_restore_projection() {
        let database = test_database().await;
        insert_runtime_server(&database).await;
        commit_runtime_catalog(&database, vec![runtime_tool("stale-tool")]).await;

        let (snapshot_loaded_tx, snapshot_loaded_rx) = tokio::sync::oneshot::channel();
        let (continue_tx, continue_rx) = tokio::sync::oneshot::channel();
        let read_database = database.clone();
        let read_task = tokio::spawn(async move {
            try_catalog_read_with_hook(&list_ctx(RefreshStrategy::CacheFirst), &read_database, || async move {
                snapshot_loaded_tx.send(()).expect("signal snapshot load");
                continue_rx.await.expect("continue stale read");
            })
            .await
            .unwrap_or_else(|_| panic!("stale request remains request-local"))
            .expect("stale request has a catalog result")
        });

        snapshot_loaded_rx.await.expect("snapshot loaded before invalidation");
        database.capability_cache.invalidate_server("server-a").await;
        continue_tx.send(()).expect("release stale read");

        let stale_result = read_task.await.expect("join stale catalog read");
        assert_eq!(
            stale_result.items.into_tools().expect("stale tools result")[0]
                .name
                .as_ref(),
            "stale-tool"
        );
        assert_eq!(
            database.capability_cache.metrics().await.projection_entries,
            0,
            "an invalidated catalog read must not repopulate the projection cache"
        );

        let fresh_result = try_catalog_read(&list_ctx(RefreshStrategy::CacheFirst), &database)
            .await
            .unwrap_or_else(|_| panic!("fresh catalog read succeeds"))
            .expect("fresh catalog result exists");
        assert_eq!(fresh_result.meta.source, "sqlite_catalog");
        assert_eq!(database.capability_cache.metrics().await.projection_entries, 1);
    }

    #[tokio::test]
    async fn stale_catalog_reader_cannot_invalidate_a_newer_ready_revision() {
        let database = test_database().await;
        insert_runtime_server(&database).await;
        commit_runtime_catalog(&database, vec![runtime_tool("revision-one")]).await;

        let (snapshot_loaded_tx, snapshot_loaded_rx) = tokio::sync::oneshot::channel();
        let (continue_tx, continue_rx) = tokio::sync::oneshot::channel();
        let read_database = database.clone();
        let read_task = tokio::spawn(async move {
            try_catalog_read_with_hook(&list_ctx(RefreshStrategy::CacheFirst), &read_database, || async move {
                snapshot_loaded_tx.send(()).expect("signal revision-one snapshot load");
                continue_rx.await.expect("continue stale reader");
            })
            .await
            .unwrap_or_else(|_| panic!("stale catalog read must not fail"))
        });

        snapshot_loaded_rx.await.expect("reader loaded revision one");
        commit_runtime_catalog(&database, vec![runtime_tool("revision-two")]).await;
        continue_tx.send(()).expect("release stale reader");
        let _ = read_task.await.expect("join stale reader");

        let snapshot = SqliteCapabilityCatalog::new(database.pool.clone())
            .load_snapshot("server-a")
            .await
            .expect("load latest catalog snapshot")
            .expect("latest catalog snapshot exists");
        assert_eq!(snapshot.revision, 2);
        assert_eq!(snapshot.state, SnapshotState::Ready);
        assert_eq!(snapshot.records.len(), 1);
        match &snapshot.records[0].source_payload {
            CapabilityPayload::Tool(tool) => assert_eq!(tool.name.as_ref(), "revision-two"),
            other => panic!("unexpected latest payload: {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_restart_cache_first_preserves_all_protocol_payloads_without_a_peer() {
        let temp_dir = TempDir::new().expect("create database directory");
        let database_path = temp_dir.path().join("capability-restart.db");
        let database_url = format!("sqlite://{}", database_path.display());
        let connect = || {
            SqliteConnectOptions::from_str(&database_url)
                .expect("parse database URL")
                .create_if_missing(true)
                .foreign_keys(true)
        };
        let first_pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(connect())
            .await
            .expect("open first database instance");
        crate::helpers::prepare_config_database(&first_pool).await;
        crate::config::initialization::run_initialization(&first_pool)
            .await
            .expect("initialize first database instance");
        insert_stdio_server(&first_pool, "server-a", "runtime_fixture").await;

        let initialize: rmcp::model::InitializeResult = serde_json::from_value(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {
                "tools": {"listChanged": true},
                "prompts": {"listChanged": true},
                "resources": {"subscribe": true, "listChanged": true}
            },
            "serverInfo": {"name": "restart-fixture", "title": "Restart Fixture", "version": "2.2.0"},
            "instructions": "Persist every standard field.",
            "_meta": {"fixture": "post-restart"}
        }))
        .expect("build initialize fixture");
        let tool: rmcp::model::Tool = serde_json::from_value(serde_json::json!({
            "name": "analyze",
            "title": "Analyze",
            "description": "Analyze input",
            "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}},
            "outputSchema": {"type": "object", "properties": {"result": {"type": "string"}}},
            "icons": [{"src": "https://icons.example/tool.svg", "mimeType": "image/svg+xml"}],
            "_meta": {"fixture": "tool"}
        }))
        .expect("build tool fixture");
        let prompt: rmcp::model::Prompt = serde_json::from_value(serde_json::json!({
            "name": "summarize",
            "title": "Summarize",
            "description": "Summarize input",
            "arguments": [{"name": "document", "title": "Document", "required": true}],
            "icons": [{"src": "https://icons.example/prompt.svg", "mimeType": "image/svg+xml"}],
            "_meta": {"fixture": "prompt"}
        }))
        .expect("build prompt fixture");
        let resource: rmcp::model::Resource = serde_json::from_value(serde_json::json!({
            "uri": "file:///fixture/report.md",
            "name": "report",
            "title": "Fixture Report",
            "description": "Persisted resource",
            "mimeType": "text/markdown",
            "size": 4096,
            "icons": [{"src": "https://icons.example/resource.svg", "mimeType": "image/svg+xml"}],
            "annotations": {"audience": ["user", "assistant"], "priority": 0.75},
            "_meta": {"fixture": "resource"}
        }))
        .expect("build resource fixture");
        let template: rmcp::model::ResourceTemplate = serde_json::from_value(serde_json::json!({
            "uriTemplate": "file:///fixture/{name}.md",
            "name": "fixture-template",
            "title": "Fixture Template",
            "description": "Persisted template",
            "mimeType": "text/markdown",
            "icons": [{"src": "https://icons.example/template.svg", "mimeType": "image/svg+xml"}],
            "annotations": {"audience": ["assistant"], "priority": 0.5},
            "_meta": {"fixture": "template"}
        }))
        .expect("build template fixture");
        crate::config::server::capabilities::commit_protocol_items_for_kinds(
            &first_pool,
            "server-a",
            "runtime_fixture",
            Some(initialize),
            vec![tool.clone()],
            vec![resource.clone()],
            vec![prompt.clone()],
            vec![template.clone()],
            CapSyncFlags::ALL,
        )
        .await
        .expect("commit live protocol observation");
        first_pool.close().await;

        let reopened_pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(connect())
            .await
            .expect("reopen database");
        crate::config::initialization::run_initialization(&reopened_pool)
            .await
            .expect("initialize reopened database");
        let database = Arc::new(Database {
            pool: reopened_pool,
            path: database_path,
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        });
        let pool = empty_runtime_pool(database.clone());
        let fixtures = [
            (CapabilityType::Tools, ProjectionPayload::Tools(vec![tool])),
            (CapabilityType::Prompts, ProjectionPayload::Prompts(vec![prompt])),
            (CapabilityType::Resources, ProjectionPayload::Resources(vec![resource])),
            (
                CapabilityType::ResourceTemplates,
                ProjectionPayload::ResourceTemplates(vec![template]),
            ),
        ];

        for (capability, expected) in fixtures {
            let mut ctx = list_ctx(RefreshStrategy::CacheFirst);
            ctx.capability = capability;
            let result = list(&ctx, &pool, &database)
                .await
                .expect("post-restart SQLite snapshot must be readable without an upstream peer");
            assert_eq!(result.meta.source, "sqlite_catalog");
            assert!(result.meta.cache_hit);
            assert!(!result.meta.had_peer);
            assert_eq!(projection_payload(result.items), expected);
        }
        assert!(pool.lock().await.connections.is_empty());

        let memory_result = list(&list_ctx(RefreshStrategy::CacheFirst), &pool, &database)
            .await
            .expect("second post-restart read must use the node-local LRU");
        assert_eq!(memory_result.meta.source, "memory_cache");
        assert!(!memory_result.meta.had_peer);
    }

    #[tokio::test]
    async fn supported_complete_empty_catalog_is_a_cache_hit_without_discovery() {
        let database = test_database().await;
        insert_runtime_server(&database).await;
        commit_runtime_catalog(&database, Vec::new()).await;
        let pool = empty_runtime_pool(database.clone());

        let result = list(&list_ctx(RefreshStrategy::CacheFirst), &pool, &database)
            .await
            .expect("supported empty catalog must not require discovery");

        assert!(result.meta.cache_hit);
        assert_eq!(result.meta.source, "sqlite_catalog");
        assert!(!result.meta.had_peer);
        assert!(result.items.into_tools().expect("tools result").is_empty());
    }

    #[tokio::test]
    async fn invalidated_catalog_does_not_fall_back_to_last_known_good() {
        let database = test_database().await;
        insert_runtime_server(&database).await;
        commit_runtime_catalog(&database, vec![runtime_tool("stale-tool")]).await;
        SqliteCapabilityCatalog::new(database.pool.clone())
            .invalidate_server("server-a", "explicit validation evidence")
            .await
            .expect("invalidate catalog");
        let pool = empty_runtime_pool(database.clone());

        let error = list(&list_ctx(RefreshStrategy::CacheFirst), &pool, &database)
            .await
            .expect_err("invalidated catalog must require live confirmation");

        assert!(
            error.to_string().contains("capability discovery failed"),
            "unexpected error: {error}"
        );
        let snapshot = SqliteCapabilityCatalog::new(database.pool.clone())
            .load_snapshot("server-a")
            .await
            .expect("load catalog")
            .expect("catalog remains available for diagnosis");
        assert_eq!(snapshot.state, SnapshotState::Unavailable);
        assert_eq!(snapshot.records.len(), 1);
    }

    #[tokio::test]
    async fn shadow_index_divergence_invalidates_the_catalog_instead_of_serving_a_stale_read() {
        let database = test_database().await;
        insert_runtime_server(&database).await;
        commit_runtime_catalog(&database, vec![runtime_tool("cached-tool")]).await;

        // Manufacture a divergence between the catalog's tool records and the tools shadow
        // index, simulating a bug that wrote one without the other outside the observation
        // commit. The read path must treat this as untrusted rather than silently serving it.
        sqlx::query("DELETE FROM server_tools WHERE server_id = 'server-a'")
            .execute(&database.pool)
            .await
            .expect("manufacture a shadow index divergence");

        let result = try_catalog_read(&list_ctx(RefreshStrategy::CacheFirst), &database)
            .await
            .unwrap_or_else(|_| panic!("integrity mismatch must not surface as a catalog error"));
        assert!(
            result.is_none(),
            "a diverged shadow index must not be served as a cache hit"
        );

        let snapshot = SqliteCapabilityCatalog::new(database.pool.clone())
            .load_snapshot("server-a")
            .await
            .expect("load catalog after integrity mismatch")
            .expect("catalog remains available for diagnosis");
        assert_eq!(snapshot.state, SnapshotState::Invalidated);
        assert_eq!(snapshot.revision, 2);
        assert!(
            snapshot
                .last_error
                .as_deref()
                .is_some_and(|reason| reason.contains("shadow index")),
            "unexpected last_error: {:?}",
            snapshot.last_error
        );

        assert_eq!(
            database.capability_cache.metrics().await.projection_entries,
            0,
            "an untrusted catalog must not populate the projection cache"
        );
    }

    #[tokio::test]
    async fn opaque_capability_errors_never_invalidate_the_catalog_by_message_text() {
        for error_message in [
            "session not found (status: 404)",
            "upstream returned 410 gone",
            "the transport is gone",
            "resource not found: file:///reports/q410/report.pdf",
            "invalid params: missing required field 'path'",
        ] {
            let database = test_database().await;
            insert_runtime_server(&database).await;
            commit_runtime_catalog(&database, vec![runtime_tool("cached-tool")]).await;

            let error = anyhow::anyhow!(error_message);
            record_capability_usage_evidence(
                &database,
                "server-a",
                CatalogKind::Tools,
                Some("runtime-instance"),
                &error,
            )
            .await;

            let snapshot = SqliteCapabilityCatalog::new(database.pool.clone())
                .load_snapshot("server-a")
                .await
                .expect("load catalog after opaque usage error")
                .expect("catalog remains available");
            assert_eq!(
                snapshot.state,
                SnapshotState::Ready,
                "opaque error text must not invalidate the catalog: {error_message}"
            );
            assert_eq!(snapshot.revision, 1);
        }
    }

    #[tokio::test]
    async fn structured_transport_closed_evidence_marks_the_catalog_unavailable() {
        let database = test_database().await;
        insert_runtime_server(&database).await;
        commit_runtime_catalog(&database, vec![runtime_tool("cached-tool")]).await;
        let error = anyhow::Error::new(rmcp::service::ServiceError::TransportClosed)
            .context("Failed to call upstream capability");

        record_capability_usage_evidence(
            &database,
            "server-a",
            CatalogKind::Tools,
            Some("runtime-instance"),
            &error,
        )
        .await;

        let snapshot = SqliteCapabilityCatalog::new(database.pool.clone())
            .load_snapshot("server-a")
            .await
            .expect("load catalog after structured transport failure")
            .expect("catalog remains available for diagnosis");
        assert_eq!(snapshot.state, SnapshotState::Unavailable);
        let tools = snapshot
            .kind_states
            .iter()
            .find(|state| state.kind == CatalogKind::Tools)
            .expect("tools failure is persisted");
        assert_eq!(tools.failure_kind, Some(KindFailureKind::TransportClosed));
    }

    #[tokio::test]
    async fn config_fingerprint_change_invalidates_catalog_and_publishes_revision() {
        let database = test_database().await;
        insert_runtime_server(&database).await;
        commit_runtime_catalog(&database, vec![runtime_tool("stale-tool")]).await;
        let mut changed_server = Server::new_stdio("runtime_fixture".to_string(), Some("changed-command".to_string()));
        changed_server.id = Some("server-a".to_string());
        upsert_server_definition(
            &database.pool,
            &changed_server,
            &ServerTransportDraft::Stdio {
                command: Some("changed-command".to_string()),
                args: Vec::new(),
                env: Default::default(),
            },
        )
        .await
        .expect("change typed server configuration");
        let pool = empty_runtime_pool(database.clone());
        let mut events = crate::core::events::EventBus::global().subscribe_async();

        let error = list(&list_ctx(RefreshStrategy::CacheFirst), &pool, &database)
            .await
            .expect_err("config mismatch must require live confirmation");

        assert!(
            error.to_string().contains("capability discovery failed"),
            "unexpected error: {error}"
        );
        let snapshot = SqliteCapabilityCatalog::new(database.pool.clone())
            .load_snapshot("server-a")
            .await
            .expect("load catalog")
            .expect("catalog remains available for diagnosis");
        assert_eq!(snapshot.state, SnapshotState::Unavailable);
        assert_eq!(snapshot.revision, 3);
        let committed_revision = tokio::time::timeout(Duration::from_secs(1), async move {
            loop {
                match events.recv().await {
                    Ok(crate::core::events::Event::CapabilityCatalogCommitted {
                        server_id, revision, ..
                    }) if server_id == "server-a" => break revision,
                    Ok(_) => continue,
                    Err(error) => panic!("event receiver failed: {error}"),
                }
            }
        })
        .await
        .expect("catalog revision event must be published");
        assert_eq!(committed_revision, 2);
    }

    #[tokio::test]
    async fn failure_commit_event_uses_canonical_database_server_name() {
        let database = test_database().await;
        insert_runtime_server(&database).await;
        commit_runtime_catalog(&database, vec![runtime_tool("stale-tool")]).await;
        let mut events = crate::core::events::EventBus::global().subscribe_async();

        record_discovery_failure(
            &list_ctx(RefreshStrategy::Force),
            "runtime_fixture",
            Some("runtime-instance"),
            None,
            "upstream unavailable",
            None,
            &database,
        )
        .await
        .expect("record failure evidence");

        let event_name = tokio::time::timeout(Duration::from_secs(1), async move {
            loop {
                match events.recv().await {
                    Ok(crate::core::events::Event::CapabilityCatalogCommitted {
                        server_id, server_name, ..
                    }) if server_id == "server-a" => break server_name,
                    Ok(_) => continue,
                    Err(error) => panic!("event receiver failed: {error}"),
                }
            }
        })
        .await
        .expect("failure commit event");

        assert_eq!(event_name, "runtime_fixture");
    }

    #[test]
    fn cached_resource_models_preserve_baseline_persisted_fields() {
        let icon = rmcp::model::Icon::new("https://example.com/resource.png");
        let resource = convert_cached_resource(CachedResourceInfo {
            uri: "file:///guide.md".to_string(),
            name: Some("Guide".to_string()),
            description: Some("Project guide".to_string()),
            mime_type: Some("text/markdown".to_string()),
            icons: Some(vec![icon.clone()]),
            enabled: true,
            cached_at: Utc::now(),
        })
        .expect("convert cached resource");

        assert_eq!(resource.uri, "file:///guide.md");
        assert_eq!(resource.name, "Guide");
        assert_eq!(resource.description.as_deref(), Some("Project guide"));
        assert_eq!(resource.mime_type.as_deref(), Some("text/markdown"));
        assert_eq!(resource.icons, Some(vec![icon]));

        let template = convert_cached_resource_template(CachedResourceTemplateInfo {
            uri_template: "file:///{path}".to_string(),
            name: Some("Files".to_string()),
            description: Some("Project files".to_string()),
            mime_type: Some("text/plain".to_string()),
            enabled: true,
            cached_at: Utc::now(),
        })
        .expect("convert cached resource template");

        assert_eq!(template.uri_template, "file:///{path}");
        assert_eq!(template.name, "Files");
        assert_eq!(template.description.as_deref(), Some("Project files"));
        assert_eq!(template.mime_type.as_deref(), Some("text/plain"));
    }

    #[tokio::test]
    async fn force_list_without_a_peer_returns_an_explicit_error() {
        let database = test_database().await;
        insert_stdio_server(&database.pool, "server-a", "searxng").await;
        let pool = Arc::new(Mutex::new(UpstreamConnectionPool::new(
            Arc::new(Config::default()),
            Some(database.clone()),
        )));
        let ctx = ListCtx {
            capability: CapabilityType::Tools,
            server_id: "server-a".to_string(),
            refresh: Some(RefreshStrategy::Force),
            operation_timeout: crate::core::transport::timeout_policy::DEFAULT_CAPABILITY_OPERATION_TIMEOUT,
            validation_session: Some("validation-a".to_string()),
            runtime_identity: None,
            connection_selection: None,
            visibility_snapshot: None,
            name_domain: NameDomain::Upstream,
        };

        let error = list(&ctx, &pool, &database)
            .await
            .expect_err("a missing validation peer must not look like an empty inventory");

        assert!(error.to_string().contains("validation-a"));
        assert!(error.to_string().contains("server-a"));
    }

    fn base_list_ctx() -> ListCtx {
        ListCtx {
            capability: CapabilityType::Tools,
            server_id: "server-a".to_string(),
            refresh: None,
            operation_timeout: crate::core::transport::timeout_policy::DEFAULT_CAPABILITY_OPERATION_TIMEOUT,
            validation_session: None,
            runtime_identity: None,
            connection_selection: None,
            visibility_snapshot: None,
            name_domain: NameDomain::External,
        }
    }

    #[test]
    fn projection_key_uses_runtime_identity_fingerprint_when_present() {
        let mut ctx = base_list_ctx();
        ctx.runtime_identity = Some(RuntimeIdentity {
            client_id: "claude-code".to_string(),
            profile_id: None,
            surface_fingerprint: "fp-runtime".to_string(),
        });

        let key = projection_key(&ctx, 1);
        assert_eq!(key.surface_fingerprint, "fp-runtime");
    }

    #[test]
    fn projection_key_uses_visibility_snapshot_fingerprint_without_runtime_identity() {
        // Builtin/broker call sites resolve visibility ad hoc and never populate
        // `runtime_identity`. A filtered projection computed from that visibility scope
        // must never land in the unscoped "shared_raw" bucket shared with unfiltered
        // management/API reads (Codex review, PR #160).
        let mut ctx = base_list_ctx();
        ctx.visibility_snapshot = Some(Arc::new(
            crate::core::profile::visibility::VisibilitySnapshot::for_test(
                "hosted-client",
                "fp-hosted-profile-a",
                vec!["server-a".to_string()],
                true,
            ),
        ));

        let key = projection_key(&ctx, 1);
        assert_eq!(key.surface_fingerprint, "fp-hosted-profile-a");
        assert_ne!(key.surface_fingerprint, "shared_raw");
    }

    #[test]
    fn projection_key_falls_back_to_shared_raw_only_without_any_visibility_context() {
        let ctx = base_list_ctx();
        let key = projection_key(&ctx, 1);
        assert_eq!(key.surface_fingerprint, "shared_raw");
    }

    #[test]
    fn capability_items_empty_returns_correct_variant() {
        assert!(matches!(
            CapabilityItems::empty(CapabilityType::Tools),
            CapabilityItems::Tools(v) if v.is_empty()
        ));
        assert!(matches!(
            CapabilityItems::empty(CapabilityType::Prompts),
            CapabilityItems::Prompts(v) if v.is_empty()
        ));
        assert!(matches!(
            CapabilityItems::empty(CapabilityType::Resources),
            CapabilityItems::Resources(v) if v.is_empty()
        ));
        assert!(matches!(
            CapabilityItems::empty(CapabilityType::ResourceTemplates),
            CapabilityItems::ResourceTemplates(v) if v.is_empty()
        ));
    }

    #[test]
    fn capability_items_is_empty_works_correctly() {
        assert!(CapabilityItems::empty(CapabilityType::Tools).is_empty());
        let schema: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        let tool = rmcp::model::Tool::new(
            std::borrow::Cow::Borrowed("test"),
            std::borrow::Cow::Borrowed("desc"),
            std::sync::Arc::new(schema),
        );
        assert!(!CapabilityItems::Tools(vec![tool]).is_empty());
    }
}
