use super::common::{
    ClientContext, ManagedClientContextResolver, SessionBoundClientContextResolver,
    bind_managed_session_after_initialize,
};
use crate::{
    audit::AuditService,
    clients::models::FirstContactBehavior,
    clients::service::ClientConfigService,
    common::constants::protocol,
    common::startup_diagnostics::{self, StartupDegradedEvent, component},
    config::audit_database::AuditDatabase,
    config::database::Database,
    core::{pool::UpstreamConnectionPool, transport::TransportType},
    mcper::builtin::BuiltinServiceRegistry,
};
use anyhow::Context;
use async_trait::async_trait;
use once_cell::sync::OnceCell;
use rmcp::model::{
    CallToolRequestParams, GetPromptRequestParams, InitializeRequestParams, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, ReadResourceRequestParams, RequestId,
    ResourceUpdatedNotificationParam, ServerInfo, SubscribeRequestParams, UnsubscribeRequestParams,
};
use rmcp::{ServerHandler, service::RequestContext};
use serde_json::{Map, Value};
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::Mutex;

static GLOBAL_PROXY_SERVER: OnceCell<Arc<Mutex<ProxyServer>>> = OnceCell::new();
const STARTUP_DIAGNOSTIC_COMPONENT: &str = "startup_proxy";

struct ProxySurfaceOutboxDelivery;

#[async_trait]
impl crate::core::capability::reconciliation::SurfaceOutboxDelivery for ProxySurfaceOutboxDelivery {
    async fn deliver(
        &self,
        event: &mcpmate_capability_store::SurfaceOutboxEvent,
    ) -> mcpmate_capability_store::Result<()> {
        let proxy =
            ProxyServer::global().ok_or_else(|| mcpmate_capability_store::CatalogError::InvalidSurfaceValue {
                field: "surface outbox delivery",
                value: "proxy server is not available".to_string(),
            })?;
        let proxy = proxy.lock().await.clone();
        proxy.deliver_consumer_surface_changed(&event.aggregate_id).await?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct DownstreamRoute {
    pub session_id: String,
    pub client_id: String,
    pub surface_fingerprint: Option<String>,
    pub peer: rmcp::service::Peer<rmcp::RoleServer>,
}

fn resolve_cancelled_route<T>(
    request_id: Option<&RequestId>,
    lookup: impl FnOnce(&RequestId) -> Option<T>,
) -> Option<(RequestId, T)> {
    let request_id = request_id?;
    lookup(request_id).map(|route| (request_id.clone(), route))
}

pub struct ProxyServer {
    pub connection_pool: Arc<Mutex<UpstreamConnectionPool>>,
    pub database: Option<Arc<Database>>,
    pub audit_database: Option<Arc<AuditDatabase>>,
    pub audit_service: Option<Arc<AuditService>>,
    pub profile_service: Option<Arc<crate::core::profile::ProfileService>>,
    pub paginator: crate::core::foundation::pagination::ProxyPaginator,
    pub builtin_services: Arc<BuiltinServiceRegistry>,
    pub cancellation_token: tokio_util::sync::CancellationToken,
    pub client_context_resolver: Arc<SessionBoundClientContextResolver>,
    pub downstream_clients: Arc<dashmap::DashMap<String, rmcp::service::Peer<rmcp::RoleServer>>>,
    pub resource_subscriptions: Arc<dashmap::DashMap<(String, String), String>>, // (session_id, unique_uri) -> server_id
    pub server_resource_index: Arc<dashmap::DashMap<String, dashmap::DashSet<(String, String)>>>, // server_id -> {(session_id, unique_uri)}
    pub call_sessions_by_token: Arc<dashmap::DashMap<rmcp::model::ProgressToken, DownstreamRoute>>,
    pub call_sessions_by_request: Arc<dashmap::DashMap<RequestId, DownstreamRoute>>,
    /// Used for first-contact governance on MCP `initialize` (unknown clients + policy).
    pub client_config_service: Option<Arc<ClientConfigService>>,
    /// Shared guard that serializes listChanged notifications across server clones.
    list_changed_guard: Arc<Mutex<()>>,
}

impl Clone for ProxyServer {
    fn clone(&self) -> Self {
        Self {
            connection_pool: self.connection_pool.clone(),
            database: self.database.clone(),
            audit_database: self.audit_database.clone(),
            audit_service: self.audit_service.clone(),
            profile_service: self.profile_service.clone(),
            paginator: self.paginator.clone(),
            builtin_services: self.builtin_services.clone(),
            cancellation_token: self.cancellation_token.clone(),
            client_context_resolver: self.client_context_resolver.clone(),
            downstream_clients: self.downstream_clients.clone(),
            resource_subscriptions: self.resource_subscriptions.clone(),
            server_resource_index: self.server_resource_index.clone(),
            call_sessions_by_token: self.call_sessions_by_token.clone(),
            call_sessions_by_request: self.call_sessions_by_request.clone(),
            client_config_service: self.client_config_service.clone(),
            list_changed_guard: self.list_changed_guard.clone(),
        }
    }
}

impl ProxyServer {
    pub fn set_global(server: Arc<Mutex<ProxyServer>>) {
        let _ = GLOBAL_PROXY_SERVER.set(server);
    }

    pub fn global() -> Option<Arc<Mutex<ProxyServer>>> {
        GLOBAL_PROXY_SERVER.get().cloned()
    }

    fn is_streamable_http(
        &self,
        context: &RequestContext<rmcp::RoleServer>,
    ) -> bool {
        context.extensions.get::<axum::http::request::Parts>().is_some()
    }

    fn map_client_context_error(
        &self,
        error: anyhow::Error,
    ) -> rmcp::ErrorData {
        tracing::warn!(error = %error, "Managed client context resolution failed");
        rmcp::ErrorData::invalid_request(error.to_string(), None)
    }

    pub async fn resolve_initialize_client_context(
        &self,
        context: &RequestContext<rmcp::RoleServer>,
        initialize: &InitializeRequestParams,
    ) -> Result<ClientContext, rmcp::ErrorData> {
        let client = self
            .client_context_resolver
            .resolve_initialize_context(initialize, context)
            .await
            .map_err(|error| self.map_client_context_error(error))?;
        // Persist observed initialize facts before governance gating; first-contact review/deny
        // can reject the request, but we still need the record to reflect what was observed.
        self.persist_initialize_observation(&client).await;
        self.attach_runtime_identity(client).await
    }

    pub async fn resolve_bound_client_context(
        &self,
        context: &RequestContext<rmcp::RoleServer>,
    ) -> Result<ClientContext, rmcp::ErrorData> {
        let client = self
            .client_context_resolver
            .resolve_request_context(context)
            .await
            .map_err(|error| self.map_client_context_error(error))?;
        let client = self.attach_runtime_identity(client).await?;

        if let Some(session_id) = client.session_id.as_deref() {
            if !self.downstream_clients.contains_key(session_id) {
                self.register_downstream_client(&client, context.peer.clone()).await?;
            }
        }

        Ok(client)
    }

    pub(super) async fn resolve_consumer_access_context(
        &self,
        client: &ClientContext,
    ) -> Result<crate::core::proxy::server::ConsumerAccessContext, rmcp::ErrorData> {
        let database = self.database.as_ref().ok_or_else(|| {
            rmcp::ErrorData::internal_error("Managed Consumer resolution requires database access".to_string(), None)
        })?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"
            SELECT identifier, approval_status
            FROM client
            WHERE identifier = ?
            "#,
        )
        .bind(&client.client_id)
        .fetch_all(&database.pool)
        .await
        .map_err(|error| rmcp::ErrorData::internal_error(error.to_string(), None))?;
        let [(consumer_id, approval_status)] = rows.as_slice() else {
            return Err(rmcp::ErrorData::invalid_request(
                format!(
                    "Managed client identity '{}' does not resolve to exactly one Consumer",
                    client.client_id
                ),
                None,
            ));
        };
        if approval_status != "approved" {
            return Err(rmcp::ErrorData::invalid_request(
                format!("Consumer '{consumer_id}' is not approved for managed access"),
                None,
            ));
        }
        if !matches!(client.config_mode.as_deref(), Some("unify" | "hosted")) {
            return Err(rmcp::ErrorData::invalid_request(
                format!("Consumer '{consumer_id}' does not have a managed MCP Surface"),
                None,
            ));
        }
        client
            .consumer_access_context(consumer_id)
            .map_err(|error| rmcp::ErrorData::invalid_request(error.to_string(), None))
    }

    pub(super) async fn load_active_surface(
        &self,
        client: &ClientContext,
    ) -> Result<crate::core::capability::surface_read::ActiveSurface, rmcp::ErrorData> {
        let access = self.resolve_consumer_access_context(client).await?;
        let database = self.database.as_ref().ok_or_else(|| {
            rmcp::ErrorData::internal_error("Managed Surface read requires database access".to_string(), None)
        })?;
        crate::core::capability::surface_read::SurfaceReader::new(database.pool.clone())
            .load(&access.consumer_id)
            .await
            .map_err(|error| rmcp::ErrorData::invalid_request(error.to_string(), None))
    }

    pub(super) async fn require_active_surface_entry(
        &self,
        client: &ClientContext,
        kind: mcpmate_capability_store::CapabilityKind,
        external_key: &str,
    ) -> Result<crate::core::capability::surface_read::ActiveSurfaceEntry, rmcp::ErrorData> {
        let access = self.resolve_consumer_access_context(client).await?;
        let database = self.database.as_ref().ok_or_else(|| {
            rmcp::ErrorData::internal_error("Managed Surface read requires database access".to_string(), None)
        })?;
        crate::core::capability::surface_read::SurfaceReader::new(database.pool.clone())
            .require(kind, &access.consumer_id, external_key)
            .await
            .map_err(|error| rmcp::ErrorData::invalid_params(error.to_string(), None))
    }

    fn require_session_id(
        &self,
        client: &ClientContext,
    ) -> Result<String, rmcp::ErrorData> {
        client.session_id.clone().ok_or_else(|| {
            rmcp::ErrorData::invalid_request(
                "Managed downstream session is required for this request".to_string(),
                None,
            )
        })
    }

    pub(super) async fn resolve_active_resource_route(
        &self,
        client: &ClientContext,
        external_uri: &str,
    ) -> Result<Option<crate::core::capability::surface_read::ActiveResourceRoute>, rmcp::ErrorData> {
        let access = self.resolve_consumer_access_context(client).await?;
        let database = self.database.as_ref().ok_or_else(|| {
            rmcp::ErrorData::internal_error("Active Surface resolution requires database access".to_string(), None)
        })?;
        crate::core::capability::surface_read::SurfaceReader::new(database.pool.clone())
            .try_resolve_resource_route(&access.consumer_id, external_uri)
            .await
            .map_err(|error| {
                rmcp::ErrorData::invalid_params(
                    format!("Resource is not in the active Surface publication: {error}"),
                    None,
                )
            })
    }

    pub fn build_downstream_route(
        &self,
        client: &ClientContext,
        peer: rmcp::service::Peer<rmcp::RoleServer>,
    ) -> Result<DownstreamRoute, rmcp::ErrorData> {
        Ok(DownstreamRoute {
            session_id: self.require_session_id(client)?,
            client_id: client.client_id.clone(),
            surface_fingerprint: client.surface_fingerprint.clone(),
            peer,
        })
    }

    async fn attach_runtime_identity(
        &self,
        mut client: ClientContext,
    ) -> Result<ClientContext, rmcp::ErrorData> {
        if let Some(ref svc) = self.client_config_service {
            self.enforce_client_governance_for_initialize(svc, &client).await?;
        }

        if client.config_mode.is_none() {
            client.config_mode = Some(self.resolve_effective_config_mode(&client.client_id).await?);
        }

        if matches!(client.config_mode.as_deref(), Some("unify")) && client.unify_workspace.is_none() {
            if let Some(ref svc) = self.client_config_service {
                client.unify_workspace = svc
                    .get_unify_direct_exposure_config(&client.client_id)
                    .await
                    .map_err(|error| self.map_client_context_error(anyhow::anyhow!(error.to_string())))?;
            }
        }

        if client.surface_fingerprint.is_some() {
            return Ok(client);
        }

        let vis = crate::core::profile::visibility::ProfileVisibilityService::new(
            self.database.clone(),
            self.profile_service.clone(),
        );
        let snapshot = vis
            .resolve_snapshot_for_client(&client)
            .await
            .map_err(|error| self.map_client_context_error(error))?;
        client.surface_fingerprint = Some(snapshot.surface_fingerprint);
        Ok(client)
    }

    /// Enforce default client governance on MCP `initialize`: deny / review / allow for unknown clients.
    /// Uses JSON-RPC–style `invalid_request` / `internal_error` per MCP error mapping.
    async fn enforce_client_governance_for_initialize(
        &self,
        svc: &Arc<ClientConfigService>,
        client: &ClientContext,
    ) -> Result<(), rmcp::ErrorData> {
        let policy = svc.get_first_contact_behavior().await.map_err(|e| {
            rmcp::ErrorData::internal_error(format!("Failed to read client governance policy: {e}"), None)
        })?;

        let display_name = client
            .observed_client_info
            .as_ref()
            .map(|o| o.name.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(client.client_id.as_str());

        let state_opt = svc
            .fetch_state(&client.client_id)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(format!("Failed to read client state: {e}"), None))?;

        if let Some(ref state) = state_opt {
            let state = if state.governance_kind() == crate::clients::models::ClientGovernanceKind::Passive {
                svc.apply_first_contact_behavior_to_passive_state(&client.client_id, display_name)
                    .await
                    .map_err(|e| {
                        rmcp::ErrorData::internal_error(
                            format!("Failed to refresh client first-contact state: {e}"),
                            None,
                        )
                    })?
            } else {
                state.clone()
            };

            return match state.approval_status() {
                "approved" => Ok(()),
                "suspended" => Err(rmcp::ErrorData::invalid_request(
                    "This client is suspended in MCPMate; connection is not allowed.".to_string(),
                    None,
                )),
                "pending" => Err(rmcp::ErrorData::invalid_request(
                    "This client is pending approval in MCPMate. Approve it in the dashboard, then reconnect."
                        .to_string(),
                    None,
                )),
                _ => Ok(()),
            };
        }

        match policy {
            FirstContactBehavior::Allow => Ok(()),
            FirstContactBehavior::Deny => Err(rmcp::ErrorData::invalid_request(
                "Unknown client identifier is denied by MCPMate policy. Register the client before connecting."
                    .to_string(),
                None,
            )),
            FirstContactBehavior::Review => {
                svc.ensure_passive_runtime_observed_row(&client.client_id, display_name)
                    .await
                    .map_err(|e| {
                        rmcp::ErrorData::internal_error(format!("Failed to register client for review: {e}"), None)
                    })?;
                Err(rmcp::ErrorData::invalid_request(
                    "This client is pending approval in MCPMate. Approve it in the dashboard, then reconnect."
                        .to_string(),
                    None,
                ))
            }
        }
    }

    async fn resolve_effective_config_mode(
        &self,
        client_id: &str,
    ) -> Result<String, rmcp::ErrorData> {
        let db = self
            .database
            .as_ref()
            .ok_or_else(|| rmcp::ErrorData::internal_error("Database not available".to_string(), None))?;
        let explicit_mode: Option<String> = sqlx::query_scalar("SELECT config_mode FROM client WHERE identifier = ?")
            .bind(client_id)
            .fetch_optional(&db.pool)
            .await
            .map_err(|error| self.map_client_context_error(error.into()))?;

        match explicit_mode.filter(|mode| !mode.trim().is_empty()) {
            Some(mode) => Ok(mode),
            None => crate::config::client::init::resolve_default_client_config_mode(&db.pool)
                .await
                .map_err(|error| self.map_client_context_error(error)),
        }
    }

    async fn persist_initialize_observation(
        &self,
        client: &ClientContext,
    ) {
        let Some(service) = self.client_config_service.as_ref() else {
            return;
        };

        let observed_display_name = client
            .observed_client_info
            .as_ref()
            .and_then(|info| info.title.as_deref())
            .or_else(|| client.observed_client_info.as_ref().map(|info| info.name.as_str()));
        let client_version = client.observed_client_info.as_ref().map(|info| info.version.as_str());
        let observed_description = client
            .observed_client_info
            .as_ref()
            .and_then(|info| info.description.as_deref());
        let observed_homepage_url = client
            .observed_client_info
            .as_ref()
            .and_then(|info| info.website_url.as_deref());
        let observed_logo_url = client
            .observed_client_info
            .as_ref()
            .and_then(|info| info.logo_url.as_deref());
        let transport = match client.transport {
            super::common::ClientTransport::StreamableHttp => Some("streamable_http"),
            super::common::ClientTransport::Other => None,
        };

        if let Err(err) = service
            .persist_handshake_observation(
                &client.client_id,
                observed_display_name,
                client_version,
                transport,
                observed_description,
                observed_homepage_url,
                observed_logo_url,
            )
            .await
        {
            tracing::warn!(client = %client.client_id, error = %err, "Failed to persist initialize observation");
        }
    }

    pub async fn register_downstream_client(
        &self,
        client: &ClientContext,
        peer: rmcp::service::Peer<rmcp::RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        let session_id = self.require_session_id(client)?;
        self.client_context_resolver
            .bind_session(&session_id, client)
            .await
            .map_err(|error| self.map_client_context_error(error))?;
        self.downstream_clients.insert(session_id.clone(), peer);

        tracing::debug!(
            session_id = %session_id,
            client_id = %client.client_id,
            profile_id = ?client.profile_id,
            source = ?client.source,
            transport = ?client.transport,
            total_clients = %self.downstream_clients.len(),
            "downstream client registered"
        );
        Ok(())
    }

    pub async fn refresh_bound_session_runtime_identity(
        &self,
        session_id: &str,
        client_id: &str,
    ) -> Result<(), rmcp::ErrorData> {
        let vis = crate::core::profile::visibility::ProfileVisibilityService::new(
            self.database.clone(),
            self.profile_service.clone(),
        );

        let snapshot = if let Some(binding) = self.client_context_resolver.session_bindings.get(session_id) {
            let client = ClientContext {
                client_id: binding.client_id.clone(),
                session_id: Some(session_id.to_string()),
                profile_id: binding.profile_id.clone(),
                config_mode: binding.config_mode.clone(),
                unify_workspace: binding.unify_workspace.clone(),
                surface_fingerprint: binding.surface_fingerprint.clone(),
                transport: crate::core::proxy::server::common::ClientTransport::StreamableHttp,
                source: crate::core::proxy::server::common::ClientIdentitySource::SessionBinding,
                observed_client_info: binding.observed_client_info.clone(),
            };
            vis.resolve_snapshot_for_client(&client)
                .await
                .map_err(|error| self.map_client_context_error(error))?
        } else {
            vis.resolve_snapshot(client_id, None)
                .await
                .map_err(|error| self.map_client_context_error(error))?
        };

        self.client_context_resolver
            .refresh_session_surface_fingerprint(session_id, snapshot.surface_fingerprint)
            .await
            .map_err(|error| self.map_client_context_error(error))
    }

    /// Refresh surface fingerprint for all bound sessions (all clients, all sessions).
    ///
    /// Used after non-client-config surface changes (profile/server status, server constraint
    /// changes) that may alter the resolved capability surface for any bound session.
    pub async fn refresh_all_bound_sessions(&self) -> usize {
        let session_ids = self
            .client_context_resolver
            .session_bindings
            .iter()
            .map(|entry| (entry.session_id.clone(), entry.client_id.clone()))
            .collect::<Vec<_>>();

        let mut refreshed = 0;
        for (session_id, client_id) in session_ids {
            if self
                .refresh_bound_session_runtime_identity(&session_id, &client_id)
                .await
                .is_ok()
            {
                refreshed += 1;
            }
        }
        refreshed
    }

    pub async fn refresh_transparent_bound_sessions(&self) -> usize {
        let sessions = self
            .client_context_resolver
            .session_bindings
            .iter()
            .map(|entry| (entry.session_id.clone(), entry.client_id.clone()))
            .collect::<Vec<_>>();

        let mut refreshed = 0;
        for (session_id, client_id) in sessions {
            match self.resolve_effective_config_mode(&client_id).await {
                Ok(mode) if mode == "transparent" => {
                    if self
                        .refresh_bound_session_runtime_identity(&session_id, &client_id)
                        .await
                        .is_ok()
                    {
                        refreshed += 1;
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        client_id = %client_id,
                        error = %error,
                        "Failed to resolve downstream mode for transparent runtime refresh"
                    );
                }
            }
        }
        refreshed
    }

    pub async fn update_unify_session_workspace(
        &self,
        session_id: &str,
        client_id: &str,
        workspace: crate::clients::models::UnifyDirectExposureConfig,
    ) -> Result<(), rmcp::ErrorData> {
        self.client_context_resolver
            .set_unify_workspace(session_id, Some(workspace))
            .await
            .map_err(|error| self.map_client_context_error(error))?;

        self.refresh_bound_session_runtime_identity(session_id, client_id).await
    }

    pub async fn apply_persisted_client_runtime_state(
        &self,
        client_id: &str,
        config_mode: Option<String>,
        unify_workspace: Option<crate::clients::models::UnifyDirectExposureConfig>,
    ) -> anyhow::Result<()> {
        let session_ids = self
            .client_context_resolver
            .session_bindings
            .iter()
            .filter(|entry| entry.client_id == client_id)
            .map(|entry| entry.session_id.clone())
            .collect::<Vec<_>>();

        for session_id in session_ids {
            self.client_context_resolver
                .set_runtime_state(&session_id, config_mode.clone(), unify_workspace.clone())
                .await?;
            self.refresh_bound_session_runtime_identity(&session_id, client_id)
                .await
                .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        }

        Ok(())
    }

    pub async fn apply_persisted_unify_direct_exposure(
        &self,
        client_id: &str,
        workspace: crate::clients::models::UnifyDirectExposureConfig,
    ) -> anyhow::Result<()> {
        self.apply_persisted_client_runtime_state(client_id, Some("unify".to_string()), Some(workspace))
            .await
    }

    /// Remove all state associated with a downstream session.
    ///
    /// ## Invariants
    ///
    /// This method maintains consistency across all session-related data structures:
    ///
    /// - `downstream_clients`: The peer is removed.
    /// - `resource_subscriptions`: All entries for this session are removed.
    /// - `server_resource_index`: For each subscription, the reverse index entry is removed.
    /// - `call_sessions_by_token` / `call_sessions_by_request`: In-flight call mappings are cleared.
    /// - `session_bindings`: The client context resolver's binding is removed.
    ///
    /// ## Trigger Points
    ///
    /// Cleanup is triggered reactively when session usage fails:
    /// - `notify_resource_updated_for_session` fails to send
    /// - `broadcast_notify` fails for a session
    /// - `forward_upstream_progress` / `forward_upstream_cancelled` / `forward_upstream_log` fail
    ///
    /// The MCP protocol lacks an explicit "session closed" notification, so stale sessions
    /// are only detected when subsequent operations fail.
    pub async fn remove_downstream_session(
        &self,
        session_id: &str,
    ) {
        self.downstream_clients.remove(session_id);
        self.remove_resource_subscriptions_for_session(session_id);

        let progress_tokens: Vec<rmcp::model::ProgressToken> = self
            .call_sessions_by_token
            .iter()
            .filter(|entry| entry.value().session_id == session_id)
            .map(|entry| entry.key().clone())
            .collect();
        for progress_token in progress_tokens {
            self.call_sessions_by_token.remove(&progress_token);
        }

        let request_ids: Vec<RequestId> = self
            .call_sessions_by_request
            .iter()
            .filter(|entry| entry.value().session_id == session_id)
            .map(|entry| entry.key().clone())
            .collect();
        for request_id in request_ids {
            self.call_sessions_by_request.remove(&request_id);
        }

        if let Err(error) = self.client_context_resolver.unbind_session(session_id).await {
            tracing::warn!(session_id = %session_id, error = %error, "Failed to unbind downstream session");
        }
    }

    fn remove_resource_subscriptions_for_session(
        &self,
        session_id: &str,
    ) {
        let subscription_keys: Vec<((String, String), String)> = self
            .resource_subscriptions
            .iter()
            .filter(|entry| entry.key().0 == session_id)
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();
        for ((subscription_session, unique_uri), server_id) in subscription_keys {
            self.resource_subscriptions
                .remove(&(subscription_session.clone(), unique_uri.clone()));
            if !server_id.is_empty() {
                if let Some(index) = self.server_resource_index.get(&server_id) {
                    index.remove(&(subscription_session, unique_uri));
                }
            }
        }
    }

    fn remove_resource_subscription(
        &self,
        session_id: &str,
        uri: &str,
    ) {
        if let Some((_, server_id)) = self
            .resource_subscriptions
            .remove(&(session_id.to_string(), uri.to_string()))
            && let Some(subscriptions) = self.server_resource_index.get(&server_id)
        {
            subscriptions.remove(&(session_id.to_string(), uri.to_string()));
        }
    }

    pub async fn remove_downstream_sessions_for_client(
        &self,
        client_id: &str,
    ) -> usize {
        let session_ids = self
            .client_context_resolver
            .session_bindings
            .iter()
            .filter(|entry| entry.client_id == client_id)
            .map(|entry| entry.session_id.clone())
            .collect::<Vec<_>>();
        let count = session_ids.len();

        for session_id in session_ids {
            self.remove_downstream_session(&session_id).await;
        }

        count
    }

    fn allowed_origin(origin: &str) -> bool {
        crate::common::env::is_allowed_origin(origin)
    }

    fn enforce_origin_if_present(
        &self,
        context: &RequestContext<rmcp::RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        if let Some(parts) = context.extensions.get::<axum::http::request::Parts>() {
            if let Some(val) = parts.headers.get(axum::http::header::ORIGIN) {
                if let Ok(s) = val.to_str() {
                    if !Self::allowed_origin(s) {
                        tracing::warn!(origin = %s, "Rejected request due to disallowed Origin");
                        return Err(rmcp::ErrorData::invalid_request(
                            format!("Disallowed Origin: {}", s),
                            None,
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn enforce_mcp_protocol_header(
        &self,
        context: &RequestContext<rmcp::RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        if !self.is_streamable_http(context) {
            return Ok(());
        }
        let parts = match context.extensions.get::<axum::http::request::Parts>() {
            Some(p) => p,
            None => return Ok(()),
        };

        // Distinguish three cases:
        // 1. Header present and valid UTF-8 -> validate as normal
        // 2. Header present but invalid UTF-8 -> reject (do not silently fall back)
        // 3. Header absent -> fall back to negotiated version from initialize
        let header_value = parts.headers.get(protocol::MCP_PROTOCOL_VERSION_HEADER);
        let (explicit_version, header_present) = match header_value {
            Some(v) => match v.to_str() {
                Ok(s) => (Some(s), true),
                Err(_) => {
                    return Err(rmcp::ErrorData::invalid_request(
                        format!("Invalid {} header encoding", protocol::MCP_PROTOCOL_VERSION_HEADER),
                        None,
                    ));
                }
            },
            None => (None, false),
        };

        let negotiated_version = if header_present {
            None
        } else {
            context.peer.peer_info().map(|info| info.protocol_version.to_string())
        };

        match Self::resolve_effective_protocol_version(explicit_version, negotiated_version) {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(rmcp::ErrorData::invalid_request(
                format!("Missing {} header", protocol::MCP_PROTOCOL_VERSION_HEADER),
                None,
            )),
            Err(error) => Err(error),
        }
    }

    fn protocol_version_from_context(
        &self,
        context: &RequestContext<rmcp::RoleServer>,
    ) -> Option<String> {
        let parts = context.extensions.get::<axum::http::request::Parts>()?;

        let explicit_version: Option<String> = parts
            .headers
            .get(protocol::MCP_PROTOCOL_VERSION_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(ToString::to_string);

        // Only compute negotiated version when header is absent
        let negotiated_version: Option<String> = if explicit_version.is_some() {
            None
        } else {
            context.peer.peer_info().map(|info| info.protocol_version.to_string())
        };

        if let Some(v) = explicit_version {
            if Self::is_valid_protocol_version(&v) {
                return Some(v);
            }
        }

        negotiated_version.filter(|version| Self::is_valid_protocol_version(version))
    }

    fn resolve_effective_protocol_version(
        header_protocol_version: Option<&str>,
        negotiated_protocol_version: Option<String>,
    ) -> Result<Option<String>, rmcp::ErrorData> {
        if let Some(version) = header_protocol_version {
            return Self::validate_protocol_version(version).map(|_| Some(version.to_string()));
        }

        if let Some(version) = negotiated_protocol_version {
            Self::validate_protocol_version(&version)?;
            return Ok(Some(version));
        }

        Ok(None)
    }

    fn is_valid_protocol_version(protocol_version: &str) -> bool {
        protocol::supports_downstream_protocol_version(protocol_version)
    }

    fn validate_protocol_version(protocol_version: &str) -> Result<(), rmcp::ErrorData> {
        if Self::is_valid_protocol_version(protocol_version) {
            return Ok(());
        }
        let requested = serde_json::from_value(serde_json::Value::String(protocol_version.to_string()))
            .map_err(|error| rmcp::ErrorData::invalid_params(error.to_string(), None))?;
        Err(Self::unsupported_protocol_version(requested))
    }

    fn unsupported_protocol_version(protocol_version: rmcp::model::ProtocolVersion) -> rmcp::ErrorData {
        rmcp::ErrorData::unsupported_protocol_version(
            protocol_version,
            protocol::supported_downstream_protocol_versions().as_ref(),
        )
    }

    pub fn new(config: Arc<crate::core::models::Config>) -> Self {
        Self::try_new(config).expect("Failed to create ProxyServer")
    }

    pub fn try_new(config: Arc<crate::core::models::Config>) -> anyhow::Result<Self> {
        let mut pool = UpstreamConnectionPool::new(config.clone(), None);
        pool.initialize();
        let connection_pool = Arc::new(Mutex::new(pool));
        UpstreamConnectionPool::start_health_check(connection_pool.clone());

        let paginator = crate::core::foundation::pagination::ProxyPaginator::new();
        let builtin_services = Arc::new(BuiltinServiceRegistry::new());
        Ok(Self {
            connection_pool,
            database: None,
            audit_database: None,
            audit_service: None,
            profile_service: None,
            paginator,
            builtin_services,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            client_context_resolver: Arc::new(SessionBoundClientContextResolver::new()),
            downstream_clients: Arc::new(dashmap::DashMap::new()),
            resource_subscriptions: Arc::new(dashmap::DashMap::new()),
            server_resource_index: Arc::new(dashmap::DashMap::new()),
            call_sessions_by_token: Arc::new(dashmap::DashMap::new()),
            call_sessions_by_request: Arc::new(dashmap::DashMap::new()),
            client_config_service: None,
            list_changed_guard: Arc::new(Mutex::new(())),
        })
    }

    async fn bootstrap_client_services(
        &mut self,
        db_arc: &Arc<Database>,
    ) {
        let bootstrap_result = ClientConfigService::bootstrap(Arc::new(db_arc.pool.clone())).await;
        let client_config_service = match bootstrap_result {
            Ok(service) => Arc::new(service),
            Err(error) => {
                tracing::warn!(
                    component = STARTUP_DIAGNOSTIC_COMPONENT,
                    phase = "client_services_bootstrap",
                    subsystem = "builtin_services",
                    degraded = true,
                    startup_continues = true,
                    action_taken = "use_reduced_builtin_services",
                    reason_code = "client_config_bootstrap_failed",
                    error = %error,
                    "Client configuration bootstrap failed during startup; continuing with reduced builtin services"
                );
                self.builtin_services = Arc::new(BuiltinServiceRegistry::new());
                return;
            }
        };

        self.client_config_service = Some(Arc::clone(&client_config_service));
        self.builtin_services = Arc::new(BuiltinServiceRegistry::new().with_mcpmate_services(
            Arc::clone(db_arc),
            Arc::clone(&self.connection_pool),
            client_config_service,
        ));
    }

    pub async fn set_database(
        &mut self,
        db: Database,
    ) -> anyhow::Result<()> {
        let db_arc = Arc::new(db);
        self.database = Some(db_arc.clone());
        crate::core::capability::naming::initialize(db_arc.pool.clone());
        self.profile_service = Some(Arc::new(crate::core::profile::ProfileService::new(db_arc.clone())));
        self.bootstrap_client_services(&db_arc).await;
        let resumed_transitions = crate::system::settings::resume_pending_configuration_mode_transitions(
            crate::common::paths::global_paths(),
            &db_arc.pool,
            self.client_config_service.clone(),
        )
        .await
        .map_err(|error| anyhow::anyhow!("Configuration mode transition recovery failed: {error}"))?;
        if resumed_transitions > 0 {
            tracing::info!(
                resumed_transitions,
                "Pending configuration mode transitions completed before capability bootstrap"
            );
        }
        let builtin_records = self
            .builtin_services
            .catalog_records()
            .map_err(|error| anyhow::anyhow!("Builtin capability Catalog materialization failed: {error}"))?;
        let (catalog_commit, materializations) =
            crate::core::capability::materializer::synchronize_builtin_catalog_and_bootstrap_managed_surfaces(
                &db_arc.pool,
                builtin_records,
            )
            .await
            .map_err(|error| anyhow::anyhow!("Builtin capability Catalog synchronization failed: {error}"))?;
        let published_consumers = materializations
            .iter()
            .filter(|(_, commit)| commit.effective_surface_changed)
            .count();
        tracing::info!(
            catalog_revision = catalog_commit.revision,
            catalog_changed = catalog_commit.changed,
            evaluated_consumers = materializations.len(),
            published_consumers,
            "Builtin capability Catalog and managed Surfaces synchronized"
        );
        if let Err(error) = crate::core::capability::resolver::init(db_arc.clone()).await {
            startup_diagnostics::warn_degraded(
                StartupDegradedEvent {
                    component: component::PROXY,
                    phase: "resolver_setup",
                    reason_code: "resolver_init_failed",
                    action_taken: "continue_without_resolver_cache",
                    subsystem: "capability",
                },
                &error,
                "Failed to initialize global resolver; continuing without in-memory name resolution cache",
            );
        } else {
            tracing::info!("Global server resolver initialized");
        }
        {
            let mut pool = self.connection_pool.lock().await;
            pool.set_database(Some(db_arc.clone()));
        }
        if let Err(error) = self.setup_event_handlers().await {
            tracing::warn!(
                component = STARTUP_DIAGNOSTIC_COMPONENT,
                phase = "event_handlers_setup",
                subsystem = "event_sync",
                degraded = true,
                startup_continues = true,
                action_taken = "disable_event_driven_sync",
                reason_code = "event_handler_init_failed",
                error = %error,
                "Event handler initialization failed during startup; continuing without event-driven sync"
            );
        }
        tracing::debug!(
            "Database connection, builtin services, server manager, and event handlers set for proxy server"
        );
        Ok(())
    }

    pub fn start_surface_background_workers(&self) -> anyhow::Result<()> {
        let database = self
            .database
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Cannot start surface workers before database setup"))?;
        crate::core::capability::reconciliation::spawn_surface_background_workers(
            database.pool.clone(),
            self.cancellation_token.clone(),
            self.audit_service.clone(),
            Some(Arc::new(ProxySurfaceOutboxDelivery)),
        );
        Ok(())
    }

    pub fn set_audit_service(
        &mut self,
        audit_database: Arc<AuditDatabase>,
        audit_service: Arc<AuditService>,
    ) {
        self.audit_database = Some(audit_database);
        self.audit_service = Some(audit_service);
    }

    async fn setup_event_handlers(&self) -> anyhow::Result<()> {
        let mut handlers = crate::core::events::EventHandlers::new();
        if let Some(profile_service) = &self.profile_service {
            handlers.set_profile_service(profile_service.clone());
        }
        if let Some(client_config_service) = &self.client_config_service {
            handlers.set_client_config_service(client_config_service.clone());
        }
        handlers.set_connection_pool(self.connection_pool.clone());
        if let Some(database) = &self.database {
            let event_capability_manager = Arc::new(crate::core::events::EventDrivenCapabilityManager::new(
                Arc::new(database.pool.clone()),
                database.capability_cache.clone(),
                self.connection_pool.clone(),
            ));
            handlers.set_event_capability_manager(event_capability_manager);
        } else {
            tracing::warn!("No database available for event-driven capability manager in event handlers");
        }
        crate::core::events::init_with_handlers(handlers)?;
        tracing::info!("Event handlers initialized with direct integration");
        Ok(())
    }

    pub async fn start(
        &self,
        transport_type: TransportType,
        bind_address: SocketAddr,
    ) -> anyhow::Result<()> {
        tracing::info!("Starting proxy server with transport type: {:?}", transport_type);
        match transport_type {
            TransportType::StreamableHttp => self.start_streamable_http_server(bind_address, "/mcp").await,
            TransportType::Stdio => Err(anyhow::anyhow!("Stdio transport not supported for proxy server")),
        }
    }

    pub async fn start_unified(
        &self,
        bind_address: SocketAddr,
    ) -> anyhow::Result<tokio::task::JoinHandle<anyhow::Result<()>>> {
        tracing::info!("Starting unified proxy server on {}", bind_address);
        let server_clone = self.clone();
        let factory = move || server_clone.clone();
        let config = super::common::UnifiedHttpServerConfig {
            bind_address,
            streamable_http_path: "/mcp".to_string(),
            keep_alive_interval: Some(std::time::Duration::from_secs(15)),
            cancellation_token: self.cancellation_token.clone(),
        };
        let server = super::common::UnifiedHttpServer::with_config(config);
        let client_context_resolver = self.client_context_resolver.clone();
        let server_handle = server.start(factory, client_context_resolver).await?;
        crate::core::events::EventBus::global().publish(crate::core::events::Event::ServerTransportReady {
            transport_type: TransportType::StreamableHttp,
            ready: true,
        });
        tracing::info!("Unified proxy server started successfully");
        Ok(server_handle)
    }

    pub async fn initiate_shutdown(&self) -> anyhow::Result<()> {
        self.cancellation_token.cancel();
        Ok(())
    }

    pub async fn complete_shutdown(&self) -> anyhow::Result<()> {
        let mut pool = self.connection_pool.lock().await;
        pool.disconnect_all().await?;
        Ok(())
    }

    async fn start_streamable_http_server(
        &self,
        bind_address: SocketAddr,
        path: &str,
    ) -> anyhow::Result<()> {
        tracing::info!("Starting Streamable HTTP server on {} at path {}", bind_address, path);
        let server_clone = self.clone();
        let factory = move || Ok(server_clone.clone());
        let server_config = rmcp::transport::StreamableHttpServerConfig::default()
            .with_sse_keep_alive(Some(std::time::Duration::from_secs(15)))
            .with_sse_retry(Some(std::time::Duration::from_secs(3)))
            .with_legacy_session_mode(true)
            .with_json_response(false)
            .with_cancellation_token(self.cancellation_token.clone());
        let session_manager = std::sync::Arc::new(
            rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default(),
        );
        let streamable_service = rmcp::transport::StreamableHttpService::new(factory, session_manager, server_config);
        let resolver = self.client_context_resolver.clone();
        let app = axum::Router::new()
            .route_service(path, streamable_service)
            .layer(axum::middleware::from_fn(move |request, next| {
                bind_managed_session_after_initialize(resolver.clone(), request, next)
            }))
            .layer(axum::Extension(if bind_address.ip().is_loopback() {
                super::common::ManagedEndpointTrust::LocalOnly
            } else {
                super::common::ManagedEndpointTrust::VerifiedCredentialRequired
            }));
        let listener = tokio::net::TcpListener::bind(bind_address)
            .await
            .context("Failed to bind Streamable HTTP server")?;
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("Streamable HTTP server error: {}", e);
            }
        });
        crate::core::events::EventBus::global().publish(crate::core::events::Event::ServerTransportReady {
            transport_type: TransportType::StreamableHttp,
            ready: true,
        });
        tracing::info!("Streamable HTTP server started successfully");
        Ok(())
    }

    pub async fn notify_tool_list_changed(&self) -> usize {
        self.broadcast_notify(|peer| Box::pin(async move { peer.notify_tool_list_changed().await }))
            .await
    }

    pub async fn notify_prompt_list_changed(&self) -> usize {
        self.broadcast_notify(|peer| Box::pin(async move { peer.notify_prompt_list_changed().await }))
            .await
    }

    pub async fn notify_resource_list_changed(&self) -> usize {
        self.broadcast_notify(|peer| Box::pin(async move { peer.notify_resource_list_changed().await }))
            .await
    }

    pub async fn notify_all_list_changed(&self) -> (usize, usize, usize) {
        let _guard = self.list_changed_guard.lock().await;
        let t = self.notify_tool_list_changed().await;
        let p = self.notify_prompt_list_changed().await;
        let r = self.notify_resource_list_changed().await;
        (t, p, r)
    }

    async fn transparent_downstream_peers(&self) -> Vec<(String, rmcp::service::Peer<rmcp::RoleServer>)> {
        let sessions = self
            .downstream_clients
            .iter()
            .filter_map(|entry| {
                let session_id = entry.key().clone();
                let peer = entry.value().clone();
                self.client_context_resolver
                    .session_bindings
                    .get(&session_id)
                    .map(|binding| (session_id, binding.client_id.clone(), peer))
            })
            .collect::<Vec<_>>();

        let mut recipients = Vec::new();
        for (session_id, client_id, peer) in sessions {
            match self.resolve_effective_config_mode(&client_id).await {
                Ok(mode) if mode == "transparent" => recipients.push((session_id, peer)),
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        client_id = %client_id,
                        error = %error,
                        "Failed to resolve downstream mode for transparent listChanged notification"
                    );
                }
            }
        }
        recipients
    }

    pub async fn notify_transparent_tool_list_changed(&self) -> usize {
        let _guard = self.list_changed_guard.lock().await;
        let recipients = self.transparent_downstream_peers().await;
        self.broadcast_notify_to(recipients, |peer| {
            Box::pin(async move { peer.notify_tool_list_changed().await })
        })
        .await
    }

    pub async fn notify_transparent_prompt_list_changed(&self) -> usize {
        let _guard = self.list_changed_guard.lock().await;
        let recipients = self.transparent_downstream_peers().await;
        self.broadcast_notify_to(recipients, |peer| {
            Box::pin(async move { peer.notify_prompt_list_changed().await })
        })
        .await
    }

    pub async fn notify_transparent_resource_list_changed(&self) -> usize {
        let _guard = self.list_changed_guard.lock().await;
        let recipients = self.transparent_downstream_peers().await;
        self.broadcast_notify_to(recipients, |peer| {
            Box::pin(async move { peer.notify_resource_list_changed().await })
        })
        .await
    }

    pub async fn notify_transparent_all_list_changed(&self) -> (usize, usize, usize) {
        let _guard = self.list_changed_guard.lock().await;
        let recipients = self.transparent_downstream_peers().await;
        let tools = self
            .broadcast_notify_to(recipients.clone(), |peer| {
                Box::pin(async move { peer.notify_tool_list_changed().await })
            })
            .await;
        let prompts = self
            .broadcast_notify_to(recipients.clone(), |peer| {
                Box::pin(async move { peer.notify_prompt_list_changed().await })
            })
            .await;
        let resources = self
            .broadcast_notify_to(recipients, |peer| {
                Box::pin(async move { peer.notify_resource_list_changed().await })
            })
            .await;
        (tools, prompts, resources)
    }

    pub async fn deliver_consumer_surface_changed(
        &self,
        consumer_id: &str,
    ) -> mcpmate_capability_store::Result<(usize, usize, usize)> {
        let _guard = self.list_changed_guard.lock().await;
        let service = self.client_config_service.as_ref().ok_or_else(|| {
            mcpmate_capability_store::CatalogError::InvalidSurfaceValue {
                field: "surface outbox delivery",
                value: "client configuration service is not available".to_string(),
            }
        })?;
        let effective_mode = service.get_effective_config_mode(consumer_id).await.map_err(|error| {
            mcpmate_capability_store::CatalogError::InvalidSurfaceValue {
                field: "surface outbox Consumer mode",
                value: format!("{consumer_id}: {error}"),
            }
        })?;
        if !matches!(effective_mode.as_str(), "hosted" | "unify") {
            self.remove_downstream_sessions_for_client(consumer_id).await;
            return Ok((0, 0, 0));
        }
        let unify_workspace = if effective_mode == "unify" {
            Some(
                service
                    .get_unify_direct_exposure_config(consumer_id)
                    .await
                    .map_err(|error| mcpmate_capability_store::CatalogError::InvalidSurfaceValue {
                        field: "surface outbox Direct Exposure",
                        value: format!("{consumer_id}: {error}"),
                    })?
                    .unwrap_or_default(),
            )
        } else {
            None
        };
        self.apply_persisted_client_runtime_state(consumer_id, Some(effective_mode), unify_workspace)
            .await
            .map_err(|error| mcpmate_capability_store::CatalogError::InvalidSurfaceValue {
                field: "surface outbox runtime state",
                value: format!("{consumer_id}: {error}"),
            })?;

        let session_ids = self
            .client_context_resolver
            .session_bindings
            .iter()
            .filter(|entry| entry.client_id == consumer_id)
            .map(|entry| entry.session_id.clone())
            .collect::<Vec<_>>();

        let mut peers = Vec::new();
        for session_id in session_ids {
            self.remove_resource_subscriptions_for_session(&session_id);
            if let Some(peer) = self.downstream_clients.get(&session_id) {
                peers.push((session_id, peer.clone()));
            }
        }

        let mut tools = 0;
        let mut prompts = 0;
        let mut resources = 0;
        let mut stale_sessions = Vec::new();
        for (session_id, peer) in peers {
            let tools_result = peer.notify_tool_list_changed().await;
            let prompts_result = peer.notify_prompt_list_changed().await;
            let resources_result = peer.notify_resource_list_changed().await;
            if tools_result.is_ok() && prompts_result.is_ok() && resources_result.is_ok() {
                tools += 1;
                prompts += 1;
                resources += 1;
            } else {
                stale_sessions.push(session_id);
            }
        }
        for session_id in stale_sessions {
            self.remove_downstream_session(&session_id).await;
        }

        Ok((tools, prompts, resources))
    }

    pub async fn notify_consumer_surface_changed(
        &self,
        consumer_id: &str,
    ) -> (usize, usize, usize) {
        match self.deliver_consumer_surface_changed(consumer_id).await {
            Ok(counts) => counts,
            Err(error) => {
                tracing::warn!(
                    consumer_id = %consumer_id,
                    error = %error,
                    "Failed to deliver direct Consumer Surface notification"
                );
                (0, 0, 0)
            }
        }
    }

    async fn notify_resource_updated_for_session(
        &self,
        session_id: &str,
        uri: &str,
    ) -> bool {
        let Some(binding) = self.client_context_resolver.session_bindings.get(session_id) else {
            self.remove_resource_subscriptions_for_session(session_id);
            return false;
        };
        let consumer_id = binding.client_id.clone();
        drop(binding);
        if self
            .refresh_bound_session_runtime_identity(session_id, &consumer_id)
            .await
            .is_err()
        {
            self.remove_downstream_session(session_id).await;
            return false;
        }
        let Some(binding) = self.client_context_resolver.session_bindings.get(session_id) else {
            self.remove_resource_subscriptions_for_session(session_id);
            return false;
        };
        let client = ClientContext {
            client_id: binding.client_id.clone(),
            session_id: Some(session_id.to_string()),
            profile_id: binding.profile_id.clone(),
            config_mode: binding.config_mode.clone(),
            unify_workspace: binding.unify_workspace.clone(),
            surface_fingerprint: binding.surface_fingerprint.clone(),
            transport: super::common::ClientTransport::StreamableHttp,
            source: binding.source,
            observed_client_info: binding.observed_client_info.clone(),
        };
        drop(binding);
        if self
            .resolve_active_resource_route(&client, uri)
            .await
            .ok()
            .flatten()
            .is_none()
        {
            self.remove_resource_subscription(session_id, uri);
            return false;
        }
        let Some(peer_ref) = self.downstream_clients.get(session_id) else {
            return false;
        };
        let peer = peer_ref.clone();
        drop(peer_ref);
        let param = ResourceUpdatedNotificationParam::new(uri);
        match peer.notify_resource_updated(param).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(session_id = %session_id, uri = %uri, error = %error, "notify resources/updated failed, removing stale session");
                self.remove_downstream_session(session_id).await;
                false
            }
        }
    }

    pub async fn notify_resource_updated(
        &self,
        uri: &str,
    ) -> usize {
        let routes: Vec<String> = self
            .resource_subscriptions
            .iter()
            .filter(|entry| entry.key().1 == uri)
            .map(|entry| entry.key().0.clone())
            .collect();
        let mut ok = 0usize;
        for session_id in routes {
            if self.notify_resource_updated_for_session(&session_id, uri).await {
                ok += 1;
            }
        }
        ok
    }

    pub async fn notify_resource_updates_for_server(
        &self,
        server_id: &str,
    ) -> usize {
        let Some(subscriptions) = self.server_resource_index.get(server_id) else {
            return 0;
        };

        let routes: Vec<(String, String)> = subscriptions.iter().map(|entry| entry.key().clone()).collect();
        drop(subscriptions);

        let mut total = 0usize;
        for (session_id, uri) in routes {
            if self.notify_resource_updated_for_session(&session_id, &uri).await {
                total += 1;
            }
        }
        total
    }

    async fn broadcast_notify<F, Fut>(
        &self,
        make_call: F,
    ) -> usize
    where
        F: Fn(rmcp::service::Peer<rmcp::RoleServer>) -> Fut,
        Fut: std::future::Future<Output = Result<(), rmcp::service::ServiceError>>,
    {
        let recipients: Vec<(String, rmcp::service::Peer<rmcp::RoleServer>)> = self
            .downstream_clients
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();
        self.broadcast_notify_to(recipients, make_call).await
    }

    async fn broadcast_notify_to<F, Fut>(
        &self,
        recipients: Vec<(String, rmcp::service::Peer<rmcp::RoleServer>)>,
        make_call: F,
    ) -> usize
    where
        F: Fn(rmcp::service::Peer<rmcp::RoleServer>) -> Fut,
        Fut: std::future::Future<Output = Result<(), rmcp::service::ServiceError>>,
    {
        let mut ok = 0usize;
        let mut stale_sessions: Vec<String> = Vec::new();
        for (session_id, peer) in recipients {
            match make_call(peer).await {
                Ok(()) => ok += 1,
                Err(error) => {
                    tracing::warn!(session_id = %session_id, error = %error, "notify downstream failed, marking stale session");
                    stale_sessions.push(session_id);
                }
            }
        }
        for session_id in stale_sessions {
            self.remove_downstream_session(&session_id).await;
        }
        ok
    }

    pub fn register_call_session(
        &self,
        progress_token: rmcp::model::ProgressToken,
        request_id: RequestId,
        route: DownstreamRoute,
    ) {
        tracing::debug!(
            progress_token = ?progress_token,
            request_id = ?request_id,
            session_id = %route.session_id,
            client_id = %route.client_id,
            "Registered call session for downstream mapping"
        );
        self.call_sessions_by_token.insert(progress_token, route.clone());
        self.call_sessions_by_request.insert(request_id, route);
    }

    pub fn unregister_call_session(
        &self,
        progress_token: &rmcp::model::ProgressToken,
        request_id: &RequestId,
    ) {
        self.call_sessions_by_token.remove(progress_token);
        self.call_sessions_by_request.remove(request_id);
    }

    fn build_base_event_data(route: &DownstreamRoute) -> Map<String, Value> {
        let mut data = Map::new();
        data.insert("client_id".to_string(), Value::String(route.client_id.clone()));
        data.insert("session_id".to_string(), Value::String(route.session_id.clone()));
        data
    }

    pub async fn forward_upstream_progress(
        &self,
        _server_id: &str,
        param: rmcp::model::ProgressNotificationParam,
        _meta_token: Option<rmcp::model::ProgressToken>,
    ) -> bool {
        let Some(route_ref) = self.call_sessions_by_token.get(&param.progress_token) else {
            return false;
        };
        let route = route_ref.clone();
        drop(route_ref);

        tracing::trace!(
            progress_token = ?param.progress_token,
            session_id = %route.session_id,
            client_id = %route.client_id,
            progress = ?param.progress,
            "Forwarded progress to downstream"
        );
        let mut data = Self::build_base_event_data(&route);
        data.insert("progress".to_string(), Value::from(param.progress));
        if let Some(total) = param.total {
            data.insert("total".to_string(), Value::from(total));
        }
        if let Some(message) = param.message.clone() {
            data.insert("message".to_string(), Value::String(message));
        }
        crate::audit::interceptor::emit_event(
            self.audit_service.as_ref(),
            crate::audit::interceptor::build_mcp_event(
                crate::audit::AuditAction::NotificationProgress,
                crate::audit::AuditStatus::Success,
                None,
                None,
                None,
                None,
                Some(data),
                None,
            ),
        )
        .await;
        match route.peer.notify_progress(param.clone()).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(session_id = %route.session_id, client_id = %route.client_id, error = %error, "Failed to forward progress; removing stale session");
                self.call_sessions_by_token.remove(&param.progress_token);
                self.remove_downstream_session(&route.session_id).await;
                false
            }
        }
    }

    pub async fn forward_upstream_cancelled(
        &self,
        _server_id: &str,
        param: rmcp::model::CancelledNotificationParam,
    ) -> bool {
        let Some((request_id, route)) = resolve_cancelled_route(param.request_id.as_ref(), |request_id| {
            self.call_sessions_by_request
                .get(request_id)
                .map(|route_ref| route_ref.clone())
        }) else {
            return false;
        };

        tracing::trace!(
            request_id = ?request_id,
            session_id = %route.session_id,
            client_id = %route.client_id,
            reason = ?param.reason,
            "Forwarded cancellation to downstream"
        );
        let mut data = Self::build_base_event_data(&route);
        data.insert("request_id".to_string(), Value::String(request_id.to_string()));
        crate::audit::interceptor::emit_event(
            self.audit_service.as_ref(),
            crate::audit::interceptor::build_mcp_event(
                crate::audit::AuditAction::NotificationCancelled,
                crate::audit::AuditStatus::Cancelled,
                None,
                None,
                None,
                None,
                Some(data),
                param.reason.clone().map(|reason| reason.to_string()),
            ),
        )
        .await;
        match route.peer.notify_cancelled(param.clone()).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(session_id = %route.session_id, client_id = %route.client_id, error = %error, "Failed to forward cancellation; removing stale session");
                self.call_sessions_by_request.remove(&request_id);
                self.remove_downstream_session(&route.session_id).await;
                false
            }
        }
    }

    #[expect(
        deprecated,
        reason = "MCPMate preserves negotiated logging forwarding until protocol removal"
    )]
    pub async fn forward_upstream_log(
        &self,
        _server_id: &str,
        param: rmcp::model::LoggingMessageNotificationParam,
        meta_token: Option<rmcp::model::ProgressToken>,
    ) -> bool {
        let Some(token) = meta_token else {
            return false;
        };
        let Some(route_ref) = self.call_sessions_by_token.get(&token) else {
            return false;
        };
        let route = route_ref.clone();
        drop(route_ref);

        tracing::trace!(
            progress_token = ?token,
            session_id = %route.session_id,
            client_id = %route.client_id,
            level = ?param.level,
            "Forwarded log message to downstream"
        );
        let mut data = Self::build_base_event_data(&route);
        data.insert(
            "level".to_string(),
            Value::String(
                match param.level {
                    rmcp::model::LoggingLevel::Debug => "debug",
                    rmcp::model::LoggingLevel::Info => "info",
                    rmcp::model::LoggingLevel::Notice => "notice",
                    rmcp::model::LoggingLevel::Warning => "warning",
                    rmcp::model::LoggingLevel::Error => "error",
                    rmcp::model::LoggingLevel::Critical => "critical",
                    rmcp::model::LoggingLevel::Alert => "alert",
                    rmcp::model::LoggingLevel::Emergency => "emergency",
                }
                .to_string(),
            ),
        );
        if let Some(logger) = param.logger.clone() {
            data.insert("logger".to_string(), Value::String(logger.to_string()));
        }
        data.insert("data".to_string(), param.data.clone());
        crate::audit::interceptor::emit_event(
            self.audit_service.as_ref(),
            crate::audit::interceptor::build_mcp_event(
                crate::audit::AuditAction::NotificationMessage,
                crate::audit::AuditStatus::Success,
                None,
                None,
                None,
                None,
                Some(data),
                None,
            ),
        )
        .await;
        match route.peer.notify_logging_message(param.clone()).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(session_id = %route.session_id, client_id = %route.client_id, error = %error, "Failed to forward log message; removing stale session");
                self.call_sessions_by_token.remove(&token);
                self.remove_downstream_session(&route.session_id).await;
                false
            }
        }
    }
}

impl ServerHandler for ProxyServer {
    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [rmcp::model::ProtocolVersion]> {
        protocol::supported_downstream_protocol_versions()
    }

    fn discover(
        &self,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<rmcp::model::DiscoverResult, rmcp::ErrorData>> + Send + '_ {
        std::future::ready(Err(rmcp::ErrorData::method_not_found::<
            rmcp::model::DiscoverRequestMethod,
        >()))
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ServerInfo, rmcp::ErrorData> {
        let started_at = std::time::Instant::now();
        if !Self::is_valid_protocol_version(request.protocol_version.as_str()) {
            return Err(Self::unsupported_protocol_version(request.protocol_version.clone()));
        }
        self.enforce_origin_if_present(&context)?;
        tracing::info!(
            client_protocol = %request.protocol_version,
            has_roots = %request.capabilities.roots.is_some(),
            has_sampling = %request.capabilities.sampling.is_some(),
            has_elicitation = %request.capabilities.elicitation.is_some(),
            client_name = %request.client_info.name,
            client_version = %request.client_info.version,
            "MCP client initialize"
        );

        if let Some(parts) = context.extensions.get::<axum::http::request::Parts>() {
            if let Some(v) = parts
                .headers
                .get(protocol::MCP_PROTOCOL_VERSION_HEADER)
                .and_then(|h| h.to_str().ok())
            {
                tracing::debug!(
                    header_mcp_protocol_version = %v,
                    header = protocol::MCP_PROTOCOL_VERSION_HEADER,
                    "HTTP header observed"
                );
            }
            if let Some(v) = parts
                .headers
                .get(axum::http::header::ORIGIN)
                .and_then(|h| h.to_str().ok())
            {
                tracing::debug!(header_origin = %v, "HTTP header: Origin");
            }
        }

        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request.clone());
        }

        let client = self.resolve_initialize_client_context(&context, &request).await?;
        if client.session_id.is_some() {
            self.register_downstream_client(&client, context.peer.clone()).await?;
        } else {
            self.client_context_resolver
                .approve_initialize_context(&context, client.clone())
                .map_err(|error| self.map_client_context_error(error))?;
        }

        let mut data = Map::new();
        data.insert(
            "client_name".to_string(),
            Value::String(request.client_info.name.clone()),
        );
        data.insert(
            "client_version".to_string(),
            Value::String(request.client_info.version.clone()),
        );
        data.insert(
            "has_roots".to_string(),
            Value::Bool(request.capabilities.roots.is_some()),
        );
        data.insert(
            "has_sampling".to_string(),
            Value::Bool(request.capabilities.sampling.is_some()),
        );
        data.insert(
            "has_elicitation".to_string(),
            Value::Bool(request.capabilities.elicitation.is_some()),
        );
        crate::audit::interceptor::emit_event(
            self.audit_service.as_ref(),
            crate::audit::interceptor::build_mcp_event(
                crate::audit::AuditAction::Initialize,
                crate::audit::AuditStatus::Success,
                Some(&client),
                Some(request.protocol_version.to_string()),
                None,
                Some(started_at.elapsed().as_millis() as u64),
                Some(data),
                None,
            ),
        )
        .await;

        Ok(self.get_info())
    }

    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        self.enforce_mcp_protocol_header(&_context)?;
        self.enforce_origin_if_present(&_context)?;
        let client = self.resolve_bound_client_context(&_context).await?;
        let session_id = self.require_session_id(&client)?;
        let target = super::resources::resolve_authorized_external_resource_target(self, &client, &request.uri).await?;
        let unique_uri = target.canonical_uri().to_string();
        let server_id = target.server_id;

        self.resource_subscriptions
            .insert((session_id.clone(), unique_uri.clone()), server_id.clone());
        let entry = self.server_resource_index.entry(server_id.clone()).or_default();
        entry.insert((session_id.clone(), unique_uri.clone()));
        tracing::info!(
            server_id = %server_id,
            session_id = %session_id,
            client_id = %client.client_id,
            uri = %unique_uri,
            "Subscribed resource updates"
        );
        Ok(())
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        self.enforce_mcp_protocol_header(&_context)?;
        self.enforce_origin_if_present(&_context)?;
        let client = self.resolve_bound_client_context(&_context).await?;
        let session_id = self.require_session_id(&client)?;
        crate::core::capability::resource_registry::validate_external_resource_uri(&request.uri).map_err(|error| {
            rmcp::ErrorData::invalid_params(format!("Invalid canonical resource URI: {error}"), None)
        })?;
        let unique_uri = request.uri;
        if let Some((_, server_id)) = self
            .resource_subscriptions
            .remove(&(session_id.clone(), unique_uri.clone()))
        {
            if !server_id.is_empty() {
                if let Some(set) = self.server_resource_index.get(&server_id) {
                    set.remove(&(session_id.clone(), unique_uri.clone()));
                }
            }
            tracing::info!(
                server_id = %server_id,
                session_id = %session_id,
                client_id = %client.client_id,
                uri = %unique_uri,
                "Unsubscribed resource updates"
            );
        }
        Ok(())
    }

    fn get_info(&self) -> ServerInfo {
        let capabilities = rmcp::model::ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .enable_prompts()
            .enable_tool_list_changed()
            .enable_prompts_list_changed()
            .enable_resources_list_changed()
            .enable_resources_subscribe()
            .build();
        ServerInfo::new(capabilities)
            .with_protocol_version(rmcp::model::ProtocolVersion::V_2025_11_25)
            .with_server_info(crate::common::constants::branding::create_implementation())
            .with_instructions(crate::common::constants::branding::DESCRIPTION.to_string())
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let audit_client = self.resolve_bound_client_context(&_context).await.ok();
        let started_at = std::time::Instant::now();
        let request_data = _request.as_ref().map(paginated_request_data);
        let protocol_version = self.protocol_version_from_context(&_context);
        self.enforce_mcp_protocol_header(&_context)?;
        self.enforce_origin_if_present(&_context)?;
        let result = super::tools::list_tools(self, _request, _context).await;
        emit_mcp_result(
            self,
            crate::audit::AuditAction::ToolsList,
            audit_client.as_ref(),
            None,
            started_at.elapsed().as_millis() as u64,
            McpAuditExtras {
                data: request_data,
                protocol_version,
                request_id: None,
                progress_token: None,
                detail: None,
            },
            result.as_ref().err().map(ToString::to_string),
        )
        .await;
        result
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let audit_client = self.resolve_bound_client_context(&_context).await.ok();
        let started_at = std::time::Instant::now();
        let target = Some(request.name.to_string());
        let protocol_version = self.protocol_version_from_context(&_context);
        let mut request_data = Map::new();
        request_data.insert("tool_name".to_string(), Value::String(request.name.to_string()));
        if let Some(arguments) = request.arguments.clone() {
            request_data.insert("arguments".to_string(), Value::Object(arguments));
        }
        self.enforce_mcp_protocol_header(&_context)?;
        self.enforce_origin_if_present(&_context)?;
        let result = super::tools::call_tool(self, request, _context).await;
        emit_mcp_result(
            self,
            crate::audit::AuditAction::ToolsCall,
            audit_client.as_ref(),
            target,
            started_at.elapsed().as_millis() as u64,
            McpAuditExtras {
                data: Some(request_data),
                protocol_version,
                request_id: None,
                progress_token: None,
                detail: Some("Called MCP tool".to_string()),
            },
            result.as_ref().err().map(ToString::to_string),
        )
        .await;
        result.map(Into::into)
    }

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListResourcesResult, rmcp::ErrorData> {
        let audit_client = self.resolve_bound_client_context(&_context).await.ok();
        let started_at = std::time::Instant::now();
        let request_data = _request.as_ref().map(paginated_request_data);
        let protocol_version = self.protocol_version_from_context(&_context);
        self.enforce_mcp_protocol_header(&_context)?;
        self.enforce_origin_if_present(&_context)?;
        let result = super::resources::list_resources(self, _request, _context).await;
        emit_mcp_result(
            self,
            crate::audit::AuditAction::ResourcesList,
            audit_client.as_ref(),
            None,
            started_at.elapsed().as_millis() as u64,
            McpAuditExtras {
                data: request_data,
                protocol_version,
                request_id: None,
                progress_token: None,
                detail: None,
            },
            result.as_ref().err().map(ToString::to_string),
        )
        .await;
        result
    }

    async fn list_resource_templates(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListResourceTemplatesResult, rmcp::ErrorData> {
        self.enforce_mcp_protocol_header(&_context)?;
        self.enforce_origin_if_present(&_context)?;
        super::resources::list_resource_templates(self, _request, _context).await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, rmcp::ErrorData> {
        let audit_client = self.resolve_bound_client_context(&_context).await.ok();
        let started_at = std::time::Instant::now();
        let target = Some(request.uri.to_string());
        let protocol_version = self.protocol_version_from_context(&_context);
        let mut request_data = Map::new();
        request_data.insert("resource_uri".to_string(), Value::String(request.uri.to_string()));
        self.enforce_mcp_protocol_header(&_context)?;
        self.enforce_origin_if_present(&_context)?;
        let result = super::resources::read_resource(self, request, _context).await;
        emit_mcp_result(
            self,
            crate::audit::AuditAction::ResourcesRead,
            audit_client.as_ref(),
            target,
            started_at.elapsed().as_millis() as u64,
            McpAuditExtras {
                data: Some(request_data),
                protocol_version,
                request_id: None,
                progress_token: None,
                detail: Some("Read MCP resource".to_string()),
            },
            result.as_ref().err().map(ToString::to_string),
        )
        .await;
        result.map(Into::into)
    }

    async fn list_prompts(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListPromptsResult, rmcp::ErrorData> {
        let audit_client = self.resolve_bound_client_context(&_context).await.ok();
        let started_at = std::time::Instant::now();
        let request_data = _request.as_ref().map(paginated_request_data);
        let protocol_version = self.protocol_version_from_context(&_context);
        self.enforce_mcp_protocol_header(&_context)?;
        self.enforce_origin_if_present(&_context)?;
        let result = super::prompts::list_prompts(self, _request, _context).await;
        emit_mcp_result(
            self,
            crate::audit::AuditAction::PromptsList,
            audit_client.as_ref(),
            None,
            started_at.elapsed().as_millis() as u64,
            McpAuditExtras {
                data: request_data,
                protocol_version,
                request_id: None,
                progress_token: None,
                detail: None,
            },
            result.as_ref().err().map(ToString::to_string),
        )
        .await;
        result
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::GetPromptResponse, rmcp::ErrorData> {
        let audit_client = self.resolve_bound_client_context(&_context).await.ok();
        let started_at = std::time::Instant::now();
        let target = Some(request.name.to_string());
        let protocol_version = self.protocol_version_from_context(&_context);
        let mut request_data = Map::new();
        request_data.insert("prompt_name".to_string(), Value::String(request.name.to_string()));
        if let Some(arguments) = request.arguments.clone() {
            request_data.insert("arguments".to_string(), Value::Object(arguments));
        }
        self.enforce_mcp_protocol_header(&_context)?;
        self.enforce_origin_if_present(&_context)?;
        let result = super::prompts::get_prompt(self, request, _context).await;
        emit_mcp_result(
            self,
            crate::audit::AuditAction::PromptsGet,
            audit_client.as_ref(),
            target,
            started_at.elapsed().as_millis() as u64,
            McpAuditExtras {
                data: Some(request_data),
                protocol_version,
                request_id: None,
                progress_token: None,
                detail: Some("Get MCP prompt".to_string()),
            },
            result.as_ref().err().map(ToString::to_string),
        )
        .await;
        result.map(Into::into)
    }
}

struct McpAuditExtras {
    data: Option<Map<String, Value>>,
    protocol_version: Option<String>,
    request_id: Option<String>,
    progress_token: Option<String>,
    detail: Option<String>,
}

async fn emit_mcp_result(
    server: &ProxyServer,
    action: crate::audit::AuditAction,
    client: Option<&ClientContext>,
    target: Option<String>,
    duration_ms: u64,
    extras: McpAuditExtras,
    error_message: Option<String>,
) {
    let status = if error_message.is_some() {
        crate::audit::AuditStatus::Failed
    } else {
        crate::audit::AuditStatus::Success
    };
    let mut event = crate::audit::AuditEvent::new(action, status)
        .with_mcp_method(crate::audit::interceptor::mcp_method_name(action))
        .with_direction("client_to_server")
        .with_duration_ms(duration_ms);
    if let Some(client) = client {
        event = apply_client_audit_context(event, client);
    }
    if let Some(target) = target {
        event = event.with_target(target);
    }
    if let Some(protocol_version) = extras.protocol_version {
        event = event.with_protocol_version(protocol_version);
    }
    if let Some(data) = extras.data {
        event = event.with_mcp_data(data);
    }
    if let Some(request_id) = extras.request_id {
        event = event.with_request_id(request_id);
    }
    if let Some(progress_token) = extras.progress_token {
        event = event.with_task_metadata(None, None, Some(progress_token));
    }
    if let Some(detail) = extras.detail {
        event = event.with_detail(detail);
    }
    if let Some(error_message) = error_message {
        event = event.with_error(None::<String>, error_message);
    }
    crate::audit::interceptor::emit_event(server.audit_service.as_ref(), event.build()).await;
}

fn paginated_request_data(request: &rmcp::model::PaginatedRequestParams) -> Map<String, Value> {
    let mut data = Map::new();
    if let Some(cursor) = request.cursor.clone() {
        data.insert("cursor".to_string(), Value::String(cursor));
    }
    data
}

fn apply_client_audit_context(
    mut event: crate::audit::AuditEvent,
    client: &ClientContext,
) -> crate::audit::AuditEvent {
    event = event.with_client_id(client.client_id.clone());
    if let Some(profile_id) = &client.profile_id {
        event = event.with_profile_id(profile_id.clone());
    }
    if let Some(session_id) = &client.session_id {
        event = event.with_session_id(session_id.clone());
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::source::{ClientConfigSource, DbTemplateSource};
    use crate::config::client::init::{
        initialize_client_table, initialize_system_settings, set_default_client_config_mode,
    };
    use crate::config::database::Database;
    use crate::config::models::{Profile, Server, ServerTransportDraft};
    use crate::core::models::Config;
    use crate::core::proxy::server::common::{ClientIdentitySource, ClientTransport};
    use axum::http::Request;
    use mcpmate_capability_store::CapabilityCatalog;
    use rmcp::ServiceExt;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    #[derive(Clone)]
    struct SubscriptionContextServer;

    impl rmcp::ServerHandler for SubscriptionContextServer {}

    #[test]
    fn upstream_cancellation_without_request_id_fails_closed_before_lookup() {
        let lookup_attempted = std::cell::Cell::new(false);
        let result = resolve_cancelled_route::<&str>(None, |_| {
            lookup_attempted.set(true);
            Some("unexpected route")
        });

        assert_eq!(result, None);
        assert!(!lookup_attempted.get(), "missing IDs must not reach route lookup");
    }

    #[tokio::test]
    async fn unified_proxy_start_reports_an_occupied_listener_before_ready() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("occupy a loopback port");
        let server = ProxyServer::new(Arc::new(Config::default()));

        let error = server
            .start_unified(occupied.local_addr().expect("read occupied port"))
            .await
            .expect_err("an occupied MCP port must stop startup before API can begin");

        assert!(error.to_string().contains("Failed to bind"));
    }

    #[test]
    fn upstream_cancellation_routes_only_the_exact_request_id() {
        let routes = dashmap::DashMap::new();
        let exact_id = RequestId::String("request-exact".into());
        let other_id = RequestId::String("request-other".into());
        routes.insert(exact_id.clone(), "exact route");
        routes.insert(other_id, "other route");

        let resolved = resolve_cancelled_route(Some(&exact_id), |request_id| {
            routes.get(request_id).map(|route| *route.value())
        });

        assert_eq!(resolved, Some((exact_id, "exact route")));
    }

    #[test]
    fn upstream_cancellation_with_unknown_request_id_does_not_route() {
        let routes = dashmap::DashMap::new();
        routes.insert(RequestId::String("request-known".into()), "known route");
        let unknown_id = RequestId::String("request-unknown".into());

        let resolved = resolve_cancelled_route(Some(&unknown_id), |request_id| {
            routes.get(request_id).map(|route| *route.value())
        });

        assert_eq!(resolved, None);
    }

    #[derive(Clone)]
    struct SubscriptionResourceServer {
        resource_list_calls: Arc<AtomicUsize>,
    }

    impl rmcp::ServerHandler for SubscriptionResourceServer {
        fn get_info(&self) -> rmcp::model::ServerInfo {
            rmcp::model::ServerInfo::new(rmcp::model::ServerCapabilities::builder().enable_resources().build())
        }

        async fn list_resources(
            &self,
            _request: Option<rmcp::model::PaginatedRequestParams>,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> Result<rmcp::model::ListResourcesResult, rmcp::ErrorData> {
            self.resource_list_calls.fetch_add(1, Ordering::SeqCst);
            Err(rmcp::ErrorData::internal_error(
                "Intentional resources/list failure".to_string(),
                None,
            ))
        }

        async fn read_resource(
            &self,
            request: ReadResourceRequestParams,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> Result<rmcp::model::ReadResourceResponse, rmcp::ErrorData> {
            Ok(
                rmcp::model::ReadResourceResult::new(vec![rmcp::model::ResourceContents::text(
                    format!("read:{}", request.uri),
                    request.uri,
                )])
                .into(),
            )
        }
    }

    async fn install_subscription_resource_connection(
        server: &ProxyServer,
        server_id: &str,
    ) -> Arc<AtomicUsize> {
        let (server_transport, client_transport) = tokio::io::duplex(4096);
        let resource_list_calls = Arc::new(AtomicUsize::new(0));
        let upstream_resource_list_calls = resource_list_calls.clone();
        tokio::spawn(async move {
            let service = SubscriptionResourceServer {
                resource_list_calls: upstream_resource_list_calls,
            }
            .serve(server_transport)
            .await
            .expect("serve subscription resource");
            service.waiting().await.expect("wait for subscription resource");
        });
        let handler = crate::core::transport::client::UpstreamClientHandler::new("subscription-resource".to_string());
        let service = handler
            .serve(client_transport)
            .await
            .expect("connect subscription resource");
        let capabilities = service.peer_info().map(|info| info.capabilities.clone());
        let server_config = crate::core::models::MCPServerConfig {
            source_fingerprint: Some("subscription-resource-config".to_string()),
            kind: crate::common::server::ServerType::Stdio,
            command: Some("subscription-resource".to_string()),
            args: None,
            url: None,
            env: None,
            headers: None,
        };
        let runtime_fingerprint = crate::config::server::fingerprint::materialized_runtime_fingerprint(&server_config)
            .expect("fingerprint subscription resource fixture");
        let mut connection = crate::core::pool::UpstreamConnection::new("subscription_docs".to_string());
        let instance_id = connection.id.clone();
        connection.update_connected(service, Vec::new(), capabilities);
        connection.config_fingerprint = server_config.source_fingerprint.clone();
        connection.runtime_fingerprint = Some(runtime_fingerprint);

        let mut pool = server.connection_pool.lock().await;
        Arc::make_mut(&mut pool.config)
            .mcp_servers
            .insert(server_id.to_string(), server_config);
        pool.connections
            .entry(server_id.to_string())
            .or_default()
            .insert(instance_id.clone(), connection);
        pool.production_routes
            .insert(crate::core::pool::ProductionRouteKey::shareable(server_id), instance_id);
        resource_list_calls
    }

    async fn subscription_request_context(
        client_id: &str,
        session_id: &str,
    ) -> (
        rmcp::service::RequestContext<rmcp::RoleServer>,
        rmcp::service::RunningService<rmcp::RoleClient, ()>,
        rmcp::service::RunningService<rmcp::RoleServer, SubscriptionContextServer>,
    ) {
        let (server_transport, client_transport) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            SubscriptionContextServer
                .serve(server_transport)
                .await
                .expect("serve context peer")
        });
        let client_service = ().serve(client_transport).await.expect("connect context client");
        let server_service = server_task.await.expect("join context server");
        let mut context = rmcp::service::RequestContext::new(
            rmcp::model::RequestId::String("subscription-test".into()),
            server_service.peer().clone(),
        );
        let request = Request::builder()
            .uri(format!("/mcp?client_id={client_id}"))
            .header("mcp-session-id", session_id)
            .body(())
            .expect("build request parts");
        context.extensions.insert(request.into_parts().0);
        (context, client_service, server_service)
    }

    async fn create_subscription_test_server() -> (TempDir, sqlx::SqlitePool, ProxyServer, String, String) {
        let temp_dir = TempDir::new().expect("temp dir");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::initialization::run_initialization(&pool)
            .await
            .expect("initialize database");
        crate::core::capability::naming::initialize(pool.clone());

        let mut upstream = Server::new_stdio(
            "subscription_docs".to_string(),
            Some("subscription-resource".to_string()),
        );
        upstream.unify_direct_exposure_eligible = true;
        let server_id = crate::config::server::upsert_server_definition(
            &pool,
            &upstream,
            &ServerTransportDraft::Stdio {
                command: Some("subscription-resource".to_string()),
                args: Vec::new(),
                env: Default::default(),
            },
        )
        .await
        .expect("insert typed subscription server");
        crate::core::capability::resolver::upsert(&server_id, "subscription_docs").await;

        let mut profile = Profile::new(
            "Subscription Profile".to_string(),
            crate::common::profile::ProfileType::Shared,
        );
        profile.is_active = true;
        let profile_id = crate::helpers::insert_profile(&pool, &profile).await;
        crate::config::profile::add_server_to_profile(&pool, &profile_id, &server_id, true)
            .await
            .expect("add server to profile");
        crate::config::server::capabilities::commit_protocol_items_for_kinds(
            &pool,
            &server_id,
            "subscription_docs",
            Some(
                serde_json::from_value(serde_json::json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {"resources": {"subscribe": true, "listChanged": true}},
                    "serverInfo": {"name": "subscription_docs", "version": "1.0.0"}
                }))
                .expect("decode subscription initialize fixture"),
            ),
            Vec::new(),
            vec![rmcp::model::Resource::new("fixture://documents/guide.md", "Guide")],
            Vec::new(),
            vec![
                serde_json::from_value(serde_json::json!({
                    "uriTemplate": "fixture://documents/{path}",
                    "name": "Documents"
                }))
                .expect("decode subscription template fixture"),
            ],
            crate::core::pool::CapSyncFlags::ALL,
        )
        .await
        .expect("commit subscription template catalog row");
        crate::config::server::capabilities::upsert_shadow_resource(
            &pool,
            &server_id,
            "subscription_docs",
            "fixture://conflicts/readme.md",
            Some("Conflicting Readme"),
            Some("upstream Resource that must not replace the built-in Readme"),
            Some("text/plain"),
        )
        .await
        .expect("add conflicting Resource to the fixture shadow index");
        crate::config::server::capabilities::upsert_shadow_resource_template(
            &pool,
            &server_id,
            "subscription_docs",
            "fixture://conflicts/guides/{topic}",
            Some("Conflicting Guide"),
            Some("upstream Resource Template that must not replace the built-in guide"),
        )
        .await
        .expect("add conflicting Resource Template to the fixture shadow index");
        let fixture_resource = mcpmate_capability_store::CatalogRecord::materialize(
            &server_id,
            "fixture://documents/guide.md",
            crate::core::capability::resource_uri::encode_resource_uri(
                "subscription_docs",
                "fixture://documents/guide.md",
            )
            .expect("encode fixture Resource URI"),
            mcpmate_capability_store::CapabilityPayload::Resource(rmcp::model::Resource::new(
                "fixture://documents/guide.md",
                "Guide",
            )),
        )
        .expect("materialize fixture Resource");
        let fixture_template = mcpmate_capability_store::CatalogRecord::materialize(
            &server_id,
            "fixture://documents/{path}",
            crate::core::capability::resource_uri::encode_resource_template(
                "subscription_docs",
                "fixture://documents/{path}",
            )
            .expect("encode fixture Resource Template URI"),
            mcpmate_capability_store::CapabilityPayload::ResourceTemplate(rmcp::model::ResourceTemplate::new(
                "fixture://documents/{path}",
                "Documents",
            )),
        )
        .expect("materialize fixture Resource Template");
        let conflicting_readme = mcpmate_capability_store::CatalogRecord::materialize(
            &server_id,
            "fixture://conflicts/readme.md",
            "mcpmate://resources/mcpmate/readme",
            mcpmate_capability_store::CapabilityPayload::Resource(
                rmcp::model::Resource::new("fixture://conflicts/readme.md", "Conflicting Readme")
                    .with_description("upstream Resource that must not replace the built-in Readme")
                    .with_mime_type("text/plain"),
            ),
        )
        .expect("materialize conflicting Readme");
        let conflicting_guide = mcpmate_capability_store::CatalogRecord::materialize(
            &server_id,
            "fixture://conflicts/guides/{topic}",
            "mcpmate://resources/template/mcpmate/guide/{topic}",
            mcpmate_capability_store::CapabilityPayload::ResourceTemplate(
                rmcp::model::ResourceTemplate::new("fixture://conflicts/guides/{topic}", "Conflicting Guide")
                    .with_description("upstream Resource Template that must not replace the built-in guide")
                    .with_mime_type("text/plain"),
            ),
        )
        .expect("materialize conflicting Resource Template");
        let initialize_result = serde_json::from_value(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"resources": {"subscribe": true, "listChanged": true}},
            "serverInfo": {"name": "subscription_docs", "version": "1.0.0"}
        }))
        .expect("decode custom catalog initialize fixture");
        let config_fingerprint = crate::config::server::capabilities::current_config_fingerprint(&pool, &server_id)
            .await
            .expect("load current fixture configuration fingerprint");
        mcpmate_capability_store::SqliteCapabilityCatalog::new(pool.clone())
            .commit_observation(mcpmate_capability_store::CapabilityObservation::new(
                &server_id,
                "subscription_docs",
                config_fingerprint,
                initialize_result,
                vec![
                    mcpmate_capability_store::KindObservation::new(
                        mcpmate_capability_store::CapabilityKind::Resources,
                        mcpmate_capability_store::DeclarationState::Supported,
                        mcpmate_capability_store::InventoryState::Complete,
                    ),
                    mcpmate_capability_store::KindObservation::new(
                        mcpmate_capability_store::CapabilityKind::ResourceTemplates,
                        mcpmate_capability_store::DeclarationState::Supported,
                        mcpmate_capability_store::InventoryState::Complete,
                    ),
                ],
                vec![
                    fixture_resource.clone(),
                    fixture_template.clone(),
                    conflicting_readme.clone(),
                    conflicting_guide.clone(),
                ],
            ))
            .await
            .expect("commit immutable conflicting Resource catalog records");
        let template_ref = fixture_template.ref_id.to_string();
        crate::config::profile::add_resource_template_to_profile(&pool, &profile_id, &server_id, &template_ref, true)
            .await
            .expect("add template to profile");
        let capability_id = fixture_template.capability_id.to_string();
        let external_template = fixture_template.external_key.clone();
        for consumer_id in [
            "hosted-client",
            "unify-client",
            "direct-client",
            "unify-duplicate-client",
        ] {
            let config_mode = match consumer_id {
                "hosted-client" => "hosted",
                "direct-client" => "transparent",
                _ => "unify",
            };
            sqlx::query(
                r#"
                INSERT INTO client (
                    id, name, identifier, config_mode, approval_status
                ) VALUES (?, ?, ?, ?, 'approved')
                "#,
            )
            .bind(consumer_id)
            .bind(consumer_id)
            .bind(consumer_id)
            .bind(config_mode)
            .execute(&pool)
            .await
            .expect("insert subscription Consumer");
            let mut entries = vec![mcpmate_capability_store::SurfaceManifestEntryInput::new(
                template_ref.parse().expect("parse subscription template ref"),
                capability_id
                    .parse()
                    .expect("parse subscription template capability id"),
                mcpmate_capability_store::CapabilityKind::ResourceTemplates,
                external_template.clone(),
            )];
            if consumer_id == "unify-duplicate-client" {
                entries.push(mcpmate_capability_store::SurfaceManifestEntryInput::new(
                    conflicting_readme.ref_id.clone(),
                    conflicting_readme.capability_id.clone(),
                    mcpmate_capability_store::CapabilityKind::Resources,
                    conflicting_readme.external_key.clone(),
                ));
                entries.push(mcpmate_capability_store::SurfaceManifestEntryInput::new(
                    conflicting_guide.ref_id.clone(),
                    conflicting_guide.capability_id.clone(),
                    mcpmate_capability_store::CapabilityKind::ResourceTemplates,
                    conflicting_guide.external_key.clone(),
                ));
            }
            let manifest = mcpmate_capability_store::SurfaceManifest::compile(consumer_id, entries)
                .expect("compile subscription Surface manifest");
            let store = mcpmate_capability_store::SqliteSurfaceStore::new(pool.clone());
            let mut transaction = pool.begin().await.expect("begin Surface publication");
            store
                .insert_manifest_in_transaction(&mut transaction, &manifest)
                .await
                .expect("insert subscription Surface manifest");
            store
                .publish_and_bind_in_transaction(
                    &mut transaction,
                    &mcpmate_capability_store::SurfacePublication::new(
                        format!("publication-{consumer_id}"),
                        consumer_id,
                        manifest.manifest_id,
                        None,
                        "test_fixture",
                        "test",
                        None,
                    ),
                    None,
                )
                .await
                .expect("publish subscription Surface");
            transaction
                .commit()
                .await
                .expect("commit subscription Surface publication");
        }

        let database = Arc::new(Database {
            pool: pool.clone(),
            path: temp_dir.path().join("test.db"),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        });
        let mut server = ProxyServer::new(Arc::new(Config::default()));
        server.database = Some(database);
        (temp_dir, pool, server, server_id, profile_id)
    }

    async fn bind_resource_client(
        server: &ProxyServer,
        client_id: &str,
        session_id: &str,
        config_mode: &str,
        unify_workspace: Option<crate::clients::models::UnifyDirectExposureConfig>,
    ) -> rmcp::service::RequestContext<rmcp::RoleServer> {
        let client = ClientContext {
            client_id: client_id.to_string(),
            session_id: Some(session_id.to_string()),
            profile_id: None,
            config_mode: Some(config_mode.to_string()),
            unify_workspace,
            surface_fingerprint: None,
            transport: ClientTransport::StreamableHttp,
            source: ClientIdentitySource::ManagedQuery,
            observed_client_info: None,
        };
        server
            .client_context_resolver
            .bind_session(session_id, &client)
            .await
            .expect("bind Resource Consumer");
        let (context, _client_service, _server_service) = subscription_request_context(client_id, session_id).await;
        context
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn canonical_resource_subscription_targets_resolve_static_and_template_routes() {
        let (_temp_dir, pool, _server, server_id, _profile_id) = create_subscription_test_server().await;
        let static_uri = crate::config::server::capabilities::upsert_shadow_resource(
            &pool,
            &server_id,
            "subscription_docs",
            "file:///guide.md",
            None,
            None,
            None,
        )
        .await
        .expect("insert static resource");
        let template_uri = crate::config::server::capabilities::upsert_shadow_resource_template(
            &pool,
            &server_id,
            "subscription_docs",
            "file:///{path}",
            Some("Files"),
            None,
        )
        .await
        .expect("insert template resource")
        .replace("{path}", "guide.md");

        assert_eq!(
            crate::core::proxy::server::resources::resolve_external_resource_target(&pool, &static_uri)
                .await
                .expect("resolve static resource")
                .server_id,
            server_id
        );
        assert_eq!(
            crate::core::proxy::server::resources::resolve_external_resource_target(&pool, &template_uri)
                .await
                .expect("resolve template resource")
                .server_id,
            server_id
        );
        assert!(
            crate::core::proxy::server::resources::resolve_external_resource_target(&pool, "file:///guide.md")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    #[expect(
        deprecated,
        reason = "MCPMate intentionally verifies pre-2026 resource subscriptions"
    )]
    async fn resource_subscription_handler_reuses_visibility_and_canonical_routing() {
        let (_temp_dir, pool, mut server, server_id, profile_id) = create_subscription_test_server().await;
        let hosted_session = "hosted-subscription";
        let hosted_client = ClientContext {
            client_id: "hosted-client".to_string(),
            session_id: Some(hosted_session.to_string()),
            profile_id: Some(profile_id),
            config_mode: Some("hosted".to_string()),
            unify_workspace: None,
            surface_fingerprint: None,
            transport: ClientTransport::StreamableHttp,
            source: ClientIdentitySource::ManagedQuery,
            observed_client_info: None,
        };
        server
            .client_context_resolver
            .bind_session(hosted_session, &hosted_client)
            .await
            .expect("bind hosted client");
        let (hosted_context, _hosted_client_service, _hosted_server_service) =
            subscription_request_context("hosted-client", hosted_session).await;
        server
            .downstream_clients
            .insert(hosted_session.to_string(), hosted_context.peer.clone());

        let template = crate::core::capability::resource_uri::encode_resource_template(
            "subscription_docs",
            "fixture://documents/{path}",
        )
        .expect("encode selected template");
        let allowed_uri = template.replace("{path}", "guide.md");
        rmcp::ServerHandler::subscribe(
            &server,
            SubscribeRequestParams::new(allowed_uri.clone()),
            hosted_context.clone(),
        )
        .await
        .expect("subscribe selected template resource");
        assert!(
            server
                .resource_subscriptions
                .contains_key(&(hosted_session.to_string(), allowed_uri.clone()))
        );
        assert_eq!(server.notify_resource_updates_for_server(&server_id).await, 1);
        let runtime_source: Arc<dyn ClientConfigSource> =
            Arc::new(DbTemplateSource::new(Arc::new(pool.clone())).expect("runtime template source"));
        server.client_config_service = Some(Arc::new(
            ClientConfigService::with_source(Arc::new(pool), runtime_source)
                .await
                .expect("client config service"),
        ));
        server
            .deliver_consumer_surface_changed("hosted-client")
            .await
            .expect("deliver target Consumer Surface change");
        server.client_config_service = None;
        assert!(
            !server
                .resource_subscriptions
                .contains_key(&(hosted_session.to_string(), allowed_uri.clone()))
        );

        let denied_uri = crate::core::capability::resource_uri::encode_resource_uri(
            "subscription_docs",
            "fixture://private/secret.md",
        )
        .expect("encode denied resource");
        assert!(
            rmcp::ServerHandler::subscribe(&server, SubscribeRequestParams::new(denied_uri), hosted_context.clone(),)
                .await
                .is_err()
        );
        assert!(
            rmcp::ServerHandler::subscribe(
                &server,
                SubscribeRequestParams::new("fixture://documents/guide.md"),
                hosted_context.clone(),
            )
            .await
            .is_err()
        );
        assert!(
            rmcp::ServerHandler::unsubscribe(
                &server,
                UnsubscribeRequestParams::new("fixture://documents/guide.md"),
                hosted_context.clone(),
            )
            .await
            .is_err()
        );

        crate::core::capability::resolver::remove_by_id(&server_id).await;
        rmcp::ServerHandler::unsubscribe(
            &server,
            UnsubscribeRequestParams::new(allowed_uri.clone()),
            hosted_context,
        )
        .await
        .expect("unsubscribe after resolver removal");
        assert!(
            !server
                .resource_subscriptions
                .contains_key(&(hosted_session.to_string(), allowed_uri))
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    #[expect(
        deprecated,
        reason = "MCPMate intentionally verifies pre-2026 resource subscriptions"
    )]
    async fn resource_update_drops_subscription_after_surface_contraction() {
        let (_temp_dir, pool, server, server_id, profile_id) = create_subscription_test_server().await;
        let session_id = "contracted-subscription";
        let client = ClientContext {
            client_id: "hosted-client".to_string(),
            session_id: Some(session_id.to_string()),
            profile_id: Some(profile_id),
            config_mode: Some("hosted".to_string()),
            unify_workspace: None,
            surface_fingerprint: None,
            transport: ClientTransport::StreamableHttp,
            source: ClientIdentitySource::ManagedQuery,
            observed_client_info: None,
        };
        server
            .client_context_resolver
            .bind_session(session_id, &client)
            .await
            .expect("bind hosted client");
        let (context, _client_service, _server_service) =
            subscription_request_context("hosted-client", session_id).await;
        server
            .downstream_clients
            .insert(session_id.to_string(), context.peer.clone());

        let uri = crate::core::capability::resource_uri::encode_resource_template(
            "subscription_docs",
            "fixture://documents/{path}",
        )
        .expect("encode selected template")
        .replace("{path}", "guide.md");
        rmcp::ServerHandler::subscribe(&server, SubscribeRequestParams::new(uri.clone()), context)
            .await
            .expect("subscribe selected resource");

        let store = mcpmate_capability_store::SqliteSurfaceStore::new(pool.clone());
        let binding = store
            .load_binding("hosted-client")
            .await
            .expect("load active binding")
            .expect("active binding should exist");
        let empty_manifest = mcpmate_capability_store::SurfaceManifest::compile("hosted-client", Vec::new())
            .expect("compile contracted Surface");
        let publication = mcpmate_capability_store::SurfacePublication::new(
            "publication-hosted-client-contracted",
            "hosted-client",
            empty_manifest.manifest_id.clone(),
            None,
            "test_surface_contraction",
            "test",
            Some(binding.active_publication_id.clone()),
        );
        let mut transaction = pool.begin().await.expect("begin contracted Surface publication");
        store
            .insert_manifest_in_transaction(&mut transaction, &empty_manifest)
            .await
            .expect("insert contracted Surface");
        store
            .publish_and_bind_in_transaction(&mut transaction, &publication, Some(binding.generation))
            .await
            .expect("publish contracted Surface");
        transaction.commit().await.expect("commit contracted Surface");

        assert_eq!(server.notify_resource_updates_for_server(&server_id).await, 0);
        assert!(
            !server
                .resource_subscriptions
                .contains_key(&(session_id.to_string(), uri))
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn transparent_list_changed_recipients_exclude_managed_consumers() {
        let (_temp_dir, pool, server, _server_id, profile_id) = create_subscription_test_server().await;
        sqlx::query(
            r#"
            INSERT INTO client (id, name, identifier, config_mode, approval_status)
            VALUES ('transparent-client', 'Transparent Client', 'transparent-client', 'transparent', 'approved')
            "#,
        )
        .execute(&pool)
        .await
        .expect("insert transparent Consumer");

        let managed_session = "managed-list-changed";
        let managed = ClientContext {
            client_id: "hosted-client".to_string(),
            session_id: Some(managed_session.to_string()),
            profile_id: Some(profile_id),
            config_mode: Some("hosted".to_string()),
            unify_workspace: None,
            surface_fingerprint: Some("managed-before-legacy-event".to_string()),
            transport: ClientTransport::StreamableHttp,
            source: ClientIdentitySource::ManagedQuery,
            observed_client_info: None,
        };
        server
            .client_context_resolver
            .bind_session(managed_session, &managed)
            .await
            .expect("bind managed client");
        let (managed_context, _managed_client, _managed_server) =
            subscription_request_context("hosted-client", managed_session).await;
        server
            .downstream_clients
            .insert(managed_session.to_string(), managed_context.peer.clone());

        let transparent_session = "transparent-list-changed";
        let transparent = ClientContext {
            client_id: "transparent-client".to_string(),
            session_id: Some(transparent_session.to_string()),
            profile_id: None,
            config_mode: Some("transparent".to_string()),
            unify_workspace: None,
            surface_fingerprint: Some("transparent-before-legacy-event".to_string()),
            transport: ClientTransport::StreamableHttp,
            source: ClientIdentitySource::ManagedQuery,
            observed_client_info: None,
        };
        server
            .client_context_resolver
            .bind_session(transparent_session, &transparent)
            .await
            .expect("bind transparent client");
        let (transparent_context, _transparent_client, _transparent_server) =
            subscription_request_context("transparent-client", transparent_session).await;
        server
            .downstream_clients
            .insert(transparent_session.to_string(), transparent_context.peer.clone());

        let recipient_ids = server
            .transparent_downstream_peers()
            .await
            .into_iter()
            .map(|(session_id, _)| session_id)
            .collect::<Vec<_>>();
        assert_eq!(recipient_ids, vec![transparent_session.to_string()]);

        assert_eq!(server.refresh_transparent_bound_sessions().await, 1);
        assert_eq!(
            server
                .client_context_resolver
                .session_bindings
                .get(managed_session)
                .expect("managed binding")
                .surface_fingerprint
                .as_deref(),
            Some("managed-before-legacy-event")
        );
        assert_ne!(
            server
                .client_context_resolver
                .session_bindings
                .get(transparent_session)
                .expect("transparent binding")
                .surface_fingerprint
                .as_deref(),
            Some("transparent-before-legacy-event")
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn surface_outbox_delivery_synchronizes_target_consumer_runtime_state() {
        let (_temp_dir, pool, mut server, _server_id, profile_id) = create_subscription_test_server().await;
        let runtime_source: Arc<dyn ClientConfigSource> =
            Arc::new(DbTemplateSource::new(Arc::new(pool.clone())).expect("runtime template source"));
        server.client_config_service = Some(Arc::new(
            ClientConfigService::with_source(Arc::new(pool.clone()), runtime_source)
                .await
                .expect("client config service"),
        ));

        let session_id = "outbox-runtime-sync";
        let client = ClientContext {
            client_id: "hosted-client".to_string(),
            session_id: Some(session_id.to_string()),
            profile_id: Some(profile_id),
            config_mode: Some("hosted".to_string()),
            unify_workspace: None,
            surface_fingerprint: Some("before-outbox".to_string()),
            transport: ClientTransport::StreamableHttp,
            source: ClientIdentitySource::ManagedQuery,
            observed_client_info: None,
        };
        server
            .client_context_resolver
            .bind_session(session_id, &client)
            .await
            .expect("bind managed Consumer");
        let (context, _client_service, _server_service) =
            subscription_request_context("hosted-client", session_id).await;
        server
            .downstream_clients
            .insert(session_id.to_string(), context.peer.clone());
        sqlx::query("UPDATE client SET config_mode = 'unify' WHERE identifier = 'hosted-client'")
            .execute(&pool)
            .await
            .expect("persist new managed mode");

        server
            .deliver_consumer_surface_changed("hosted-client")
            .await
            .expect("deliver target Consumer Surface change");

        let binding = server
            .client_context_resolver
            .session_bindings
            .get(session_id)
            .expect("target Consumer binding");
        assert_eq!(binding.config_mode.as_deref(), Some("unify"));
        assert_ne!(binding.surface_fingerprint.as_deref(), Some("before-outbox"));
    }

    #[tokio::test]
    #[serial_test::serial]
    #[expect(
        deprecated,
        reason = "MCPMate intentionally verifies pre-2026 resource subscriptions"
    )]
    async fn unify_resource_subscription_accepts_only_selected_template_routes() {
        let (_temp_dir, _pool, server, server_id, _profile_id) = create_subscription_test_server().await;
        let session_id = "unify-subscription";
        let workspace = crate::clients::models::UnifyDirectExposureConfig {
            route_mode: crate::clients::models::UnifyRouteMode::CapabilityLevel,
            selected_template_surfaces: vec![crate::clients::models::UnifyDirectTemplateSurface {
                server_id: server_id.clone(),
                uri_template: "fixture://documents/{path}".to_string(),
            }],
            ..Default::default()
        };
        let context = bind_resource_client(&server, "unify-client", session_id, "unify", Some(workspace)).await;
        let _resource_list_calls = install_subscription_resource_connection(&server, &server_id).await;

        let selected = crate::core::capability::resource_uri::encode_resource_template(
            "subscription_docs",
            "fixture://documents/{path}",
        )
        .expect("encode selected template")
        .replace("{path}", "guide.md");
        rmcp::ServerHandler::subscribe(&server, SubscribeRequestParams::new(selected.clone()), context.clone())
            .await
            .expect("subscribe selected unify template");
        let result = crate::core::proxy::server::resources::read_resource(
            &server,
            ReadResourceRequestParams::new(
                crate::core::capability::resource_uri::encode_resource_template(
                    "subscription_docs",
                    "fixture://documents/{path}",
                )
                .expect("encode selected read template")
                .replace("{path}", "guide.md"),
            ),
            context.clone(),
        )
        .await
        .expect("read selected unify template without resources/list");
        assert_eq!(result.contents.len(), 1);
        match &result.contents[0] {
            rmcp::model::ResourceContents::TextResourceContents { uri, text, .. } => {
                assert_eq!(uri, &selected);
                assert_eq!(text, "read:fixture://documents/guide.md");
            }
            rmcp::model::ResourceContents::BlobResourceContents { .. } => panic!("expected text resource"),
            _ => panic!("expected known resource contents"),
        }

        let unselected = crate::core::capability::resource_uri::encode_resource_template(
            "subscription_docs",
            "fixture://private/{path}",
        )
        .expect("encode unselected template")
        .replace("{path}", "secret.md");
        assert!(
            rmcp::ServerHandler::subscribe(&server, SubscribeRequestParams::new(unselected), context,)
                .await
                .is_err()
        );
        crate::core::capability::resolver::remove_by_id(&server_id).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn broker_only_standard_resource_read_uses_current_catalog_route_without_upstream_list() {
        let (_temp_dir, _pool, server, server_id, _profile_id) = create_subscription_test_server().await;
        let session_id = "broker-only-standard-read";
        let context = bind_resource_client(
            &server,
            "unify-client",
            session_id,
            "unify",
            Some(crate::clients::models::UnifyDirectExposureConfig {
                route_mode: crate::clients::models::UnifyRouteMode::BrokerOnly,
                ..Default::default()
            }),
        )
        .await;
        let resource_list_calls = install_subscription_resource_connection(&server, &server_id).await;
        let canonical_uri = crate::core::capability::resource_uri::encode_resource_uri(
            "subscription_docs",
            "fixture://documents/guide.md",
        )
        .expect("encode cataloged listed Resource URI");

        let result = crate::core::proxy::server::resources::read_resource(
            &server,
            ReadResourceRequestParams::new(canonical_uri.clone()),
            context,
        )
        .await
        .expect("read BrokerOnly canonical Resource through the standard handler");

        assert_eq!(resource_list_calls.load(Ordering::SeqCst), 0);
        assert_eq!(result.contents.len(), 1);
        match &result.contents[0] {
            rmcp::model::ResourceContents::TextResourceContents { uri, text, .. } => {
                assert_eq!(uri, &canonical_uri);
                assert_eq!(text, "read:fixture://documents/guide.md");
            }
            rmcp::model::ResourceContents::BlobResourceContents { .. } => panic!("expected text resource"),
            _ => panic!("expected known resource contents"),
        }

        crate::core::capability::resolver::remove_by_id(&server_id).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn broker_only_standard_resource_read_returns_catalog_incomplete_error_data() {
        let (_temp_dir, pool, server, server_id, _profile_id) = create_subscription_test_server().await;
        let context = bind_resource_client(
            &server,
            "unify-client",
            "broker-only-catalog-error",
            "unify",
            Some(crate::clients::models::UnifyDirectExposureConfig {
                route_mode: crate::clients::models::UnifyRouteMode::BrokerOnly,
                ..Default::default()
            }),
        )
        .await;
        server
            .database
            .as_ref()
            .expect("database")
            .capability_cache
            .invalidate_server(&server_id)
            .await;
        sqlx::query("UPDATE capability_server_snapshots SET snapshot_state = 'invalidated' WHERE server_id = ?")
            .bind(&server_id)
            .execute(&pool)
            .await
            .expect("invalidate trusted resource catalog");

        let error = crate::core::proxy::server::resources::read_resource(
            &server,
            ReadResourceRequestParams::new(
                crate::core::capability::resource_uri::encode_resource_uri(
                    "subscription_docs",
                    "fixture://documents/guide.md",
                )
                .expect("encode cataloged listed Resource URI"),
            ),
            context,
        )
        .await
        .expect_err("missing catalog authority must fail standard Broker read");

        assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert_eq!(
            error.data,
            Some(serde_json::json!({"error_code": "catalog_incomplete", "retry_eligible": true}))
        );
        crate::core::capability::resolver::remove_by_id(&server_id).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn broker_only_standard_resource_read_keeps_corrupt_persisted_template_internal() {
        let (_temp_dir, pool, server, server_id, _profile_id) = create_subscription_test_server().await;
        let context = bind_resource_client(
            &server,
            "unify-client",
            "broker-only-corrupt-template",
            "unify",
            Some(Default::default()),
        )
        .await;
        sqlx::query(
            "UPDATE server_resource_templates SET uri_template = 'fixture://documents/{path', unique_name = 'mcpmate://resources/template/subscription_docs/fixture/broker/{path}' WHERE uri_template = 'fixture://documents/{path}'",
        )
            .execute(&pool)
            .await
            .expect("corrupt persisted Broker template");

        let error = crate::core::proxy::server::resources::read_resource(
            &server,
            ReadResourceRequestParams::new("mcpmate://resources/template/subscription_docs/fixture/broker/guide.md"),
            context,
        )
        .await
        .expect_err("corrupt persisted template must fail the standard Broker read");

        assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        crate::core::capability::resolver::remove_by_id(&server_id).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn broker_only_standard_resource_read_rejects_non_utf8_canonical_template_argument() {
        let (_temp_dir, pool, server, server_id, _profile_id) = create_subscription_test_server().await;
        let context = bind_resource_client(
            &server,
            "unify-client",
            "broker-only-non-utf8-template-argument",
            "unify",
            Some(Default::default()),
        )
        .await;
        sqlx::query(
            "UPDATE server_resource_templates SET unique_name = 'mcpmate://resources/template/subscription_docs/fixture/broker/{path}' WHERE uri_template = 'fixture://documents/{path}'",
        )
        .execute(&pool)
        .await
        .expect("move persisted Broker template outside Active Surface");
        let error = crate::core::proxy::server::resources::read_resource(
            &server,
            ReadResourceRequestParams::new("mcpmate://resources/template/subscription_docs/fixture/broker/%FF"),
            context,
        )
        .await
        .expect_err("non-UTF-8 canonical argument must fail standard Broker read");

        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        crate::core::capability::resolver::remove_by_id(&server_id).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn non_unify_standard_resource_read_does_not_enter_broker_resolution() {
        let (_temp_dir, _pool, server, server_id, _profile_id) = create_subscription_test_server().await;
        let context = bind_resource_client(&server, "hosted-client", "hosted-standard-read", "hosted", None).await;

        let error = crate::core::proxy::server::resources::read_resource(
            &server,
            ReadResourceRequestParams::new(
                crate::core::capability::resource_uri::encode_resource_uri(
                    "subscription_docs",
                    "fixture://documents/guide.md",
                )
                .expect("encode cataloged listed Resource URI"),
            ),
            context,
        )
        .await
        .expect_err("non-Unify Consumer must not read a Broker-only Resource");

        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        crate::core::capability::resolver::remove_by_id(&server_id).await;
    }

    fn assert_markdown(
        result: rmcp::model::ReadResourceResponse,
        expected_uri: &str,
        expected_heading: &str,
    ) {
        let rmcp::model::ReadResourceResponse::Complete(result) = result else {
            panic!("expected complete built-in Resource read");
        };
        assert_eq!(result.contents.len(), 1);
        match &result.contents[0] {
            rmcp::model::ResourceContents::TextResourceContents {
                uri, mime_type, text, ..
            } => {
                assert_eq!(uri, expected_uri);
                assert_eq!(mime_type.as_deref(), Some("text/markdown"));
                assert!(
                    text.contains(expected_heading),
                    "built-in guide content must be distinct Markdown"
                );
            }
            rmcp::model::ResourceContents::BlobResourceContents { .. } => panic!("expected Markdown text resource"),
            _ => panic!("expected known resource contents"),
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unify_builtin_resource_guide_is_listed_readable_and_hidden_from_hosted() {
        let (_temp_dir, _pool, server, server_id, _profile_id) = create_subscription_test_server().await;
        let unify_context =
            bind_resource_client(&server, "unify-client", "builtin-resource-guide-unify", "unify", None).await;

        let unify_resources = rmcp::ServerHandler::list_resources(&server, None, unify_context.clone())
            .await
            .expect("list Unify resources");
        assert!(
            unify_resources
                .resources
                .iter()
                .any(|item| item.uri == "mcpmate://resources/mcpmate/readme"),
            "the empty Resource Surface first page must include the built-in Readme"
        );

        let unify_templates = rmcp::ServerHandler::list_resource_templates(&server, None, unify_context.clone())
            .await
            .expect("list Unify Resource Templates");
        assert!(
            unify_templates
                .resource_templates
                .iter()
                .any(|item| item.uri_template == "mcpmate://resources/template/mcpmate/guide/{topic}"),
            "the BrokerOnly Resource Template first page must include the built-in guide"
        );

        let duplicate_context = bind_resource_client(
            &server,
            "unify-duplicate-client",
            "builtin-resource-guide-duplicate",
            "unify",
            None,
        )
        .await;
        let duplicate_client = server
            .resolve_bound_client_context(&duplicate_context)
            .await
            .expect("resolve duplicate Consumer");
        let duplicate_surface = server
            .load_active_surface(&duplicate_client)
            .await
            .expect("load duplicate Consumer Surface");
        assert!(duplicate_surface.entries.iter().any(|entry| {
            entry.kind == mcpmate_capability_store::CapabilityKind::Resources
                && entry.external_key == "mcpmate://resources/mcpmate/readme"
        }));
        assert!(duplicate_surface.entries.iter().any(|entry| {
            entry.kind == mcpmate_capability_store::CapabilityKind::ResourceTemplates
                && entry.external_key == "mcpmate://resources/template/mcpmate/guide/{topic}"
        }));

        let duplicate_resources = rmcp::ServerHandler::list_resources(&server, None, duplicate_context.clone())
            .await
            .expect("list Unify resources with conflicting active Surface entry");
        let duplicate_readmes: Vec<_> = duplicate_resources
            .resources
            .iter()
            .filter(|item| item.uri == "mcpmate://resources/mcpmate/readme")
            .collect();
        assert_eq!(duplicate_readmes.len(), 1);
        assert_eq!(duplicate_readmes[0].name, "Readme");
        assert_eq!(
            duplicate_readmes[0].description.as_deref(),
            Some("MCPMate resource workflow overview")
        );
        assert_eq!(duplicate_readmes[0].mime_type.as_deref(), Some("text/markdown"));

        let duplicate_templates =
            rmcp::ServerHandler::list_resource_templates(&server, None, duplicate_context.clone())
                .await
                .expect("list Unify Resource Templates with conflicting active Surface entry");
        let duplicate_guides: Vec<_> = duplicate_templates
            .resource_templates
            .iter()
            .filter(|item| item.uri_template == "mcpmate://resources/template/mcpmate/guide/{topic}")
            .collect();
        assert_eq!(duplicate_guides.len(), 1);
        assert_eq!(duplicate_guides[0].name, "MCPMate Resource Guide");
        assert_eq!(
            duplicate_guides[0].description.as_deref(),
            Some("Read a stable MCPMate resource workflow guide by topic")
        );
        assert_eq!(duplicate_guides[0].mime_type.as_deref(), Some("text/markdown"));

        assert_markdown(
            rmcp::ServerHandler::read_resource(
                &server,
                ReadResourceRequestParams::new("mcpmate://resources/mcpmate/readme"),
                duplicate_context.clone(),
            )
            .await
            .expect("read built-in Readme before a conflicting active Surface route"),
            "mcpmate://resources/mcpmate/readme",
            "# MCPMate Resource Readme",
        );
        assert_markdown(
            rmcp::ServerHandler::read_resource(
                &server,
                ReadResourceRequestParams::new("mcpmate://resources/template/mcpmate/guide/read"),
                duplicate_context,
            )
            .await
            .expect("read built-in guide before a conflicting active Surface template route"),
            "mcpmate://resources/template/mcpmate/guide/read",
            "# Standard Read",
        );

        assert_markdown(
            rmcp::ServerHandler::read_resource(
                &server,
                ReadResourceRequestParams::new("mcpmate://resources/mcpmate/readme"),
                unify_context.clone(),
            )
            .await
            .expect("read built-in Readme"),
            "mcpmate://resources/mcpmate/readme",
            "# MCPMate Resource Readme",
        );
        assert_markdown(
            rmcp::ServerHandler::read_resource(
                &server,
                ReadResourceRequestParams::new("mcpmate://resources/template/mcpmate/guide/read"),
                unify_context,
            )
            .await
            .expect("read built-in guide topic"),
            "mcpmate://resources/template/mcpmate/guide/read",
            "# Standard Read",
        );

        let hosted_context = bind_resource_client(
            &server,
            "hosted-client",
            "builtin-resource-guide-hosted",
            "hosted",
            None,
        )
        .await;
        let hosted_resources = rmcp::ServerHandler::list_resources(&server, None, hosted_context.clone())
            .await
            .expect("list Hosted resources");
        assert!(
            !hosted_resources
                .resources
                .iter()
                .any(|item| item.uri.contains("/mcpmate/readme"))
        );
        let hosted_error = rmcp::ServerHandler::read_resource(
            &server,
            ReadResourceRequestParams::new("mcpmate://resources/mcpmate/readme"),
            hosted_context.clone(),
        )
        .await
        .expect_err("Hosted Consumer must not read the Unify built-in Readme");
        assert_eq!(hosted_error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        let hosted_templates = rmcp::ServerHandler::list_resource_templates(&server, None, hosted_context)
            .await
            .expect("list Hosted Resource Templates");
        assert!(
            !hosted_templates
                .resource_templates
                .iter()
                .any(|item| item.uri_template.contains("/mcpmate/guide/"))
        );

        let direct_context = bind_resource_client(
            &server,
            "direct-client",
            "builtin-resource-guide-direct",
            "transparent",
            None,
        )
        .await;
        rmcp::ServerHandler::read_resource(
            &server,
            ReadResourceRequestParams::new("mcpmate://resources/mcpmate/readme"),
            direct_context,
        )
        .await
        .expect_err("Direct Consumer must not read the Unify built-in Readme");

        crate::core::capability::resolver::remove_by_id(&server_id).await;
    }

    struct TestServerState {
        downstream_clients: Arc<dashmap::DashMap<String, rmcp::service::Peer<rmcp::RoleServer>>>,
        resource_subscriptions: Arc<dashmap::DashMap<(String, String), String>>,
        server_resource_index: Arc<dashmap::DashMap<String, dashmap::DashSet<(String, String)>>>,
        session_bindings: Arc<SessionBoundClientContextResolver>,
    }

    fn create_test_server_state() -> TestServerState {
        TestServerState {
            downstream_clients: Arc::new(dashmap::DashMap::new()),
            resource_subscriptions: Arc::new(dashmap::DashMap::new()),
            server_resource_index: Arc::new(dashmap::DashMap::new()),
            session_bindings: Arc::new(SessionBoundClientContextResolver::new()),
        }
    }

    async fn create_mode_resolution_test_server() -> (TempDir, sqlx::SqlitePool, ProxyServer) {
        let temp_dir = TempDir::new().expect("temp dir");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");

        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("enable foreign keys");
        crate::helpers::prepare_config_database(&pool).await;
        initialize_client_table(&pool).await.expect("init client table");
        initialize_system_settings(&pool)
            .await
            .expect("init system settings store");

        let database = Arc::new(Database {
            pool: pool.clone(),
            path: temp_dir.path().join("test.db"),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        });

        let mut server = ProxyServer::new(Arc::new(Config::default()));
        server.database = Some(database);

        (temp_dir, pool, server)
    }

    async fn cleanup_session_state(
        session_id: &str,
        state: &TestServerState,
    ) {
        state.downstream_clients.remove(session_id);

        let subscription_keys: Vec<((String, String), String)> = state
            .resource_subscriptions
            .iter()
            .filter(|entry| entry.key().0 == session_id)
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();
        for ((subscription_session, unique_uri), server_id) in subscription_keys {
            state
                .resource_subscriptions
                .remove(&(subscription_session.clone(), unique_uri.clone()));
            if !server_id.is_empty() {
                if let Some(index) = state.server_resource_index.get(&server_id) {
                    index.remove(&(subscription_session, unique_uri));
                }
            }
        }

        let _ = state.session_bindings.unbind_session(session_id).await;
    }

    #[tokio::test]
    async fn resource_subscriptions_cleanup_removes_session_entries() {
        let state = create_test_server_state();
        let session_id = "test-session";

        state.resource_subscriptions.insert(
            (session_id.to_string(), "resource://a".to_string()),
            "srv-1".to_string(),
        );
        state.resource_subscriptions.insert(
            (session_id.to_string(), "resource://b".to_string()),
            "srv-1".to_string(),
        );
        state
            .resource_subscriptions
            .insert(("other".to_string(), "resource://c".to_string()), "srv-1".to_string());

        {
            let idx = state.server_resource_index.entry("srv-1".to_string()).or_default();
            idx.insert((session_id.to_string(), "resource://a".to_string()));
            idx.insert((session_id.to_string(), "resource://b".to_string()));
            idx.insert(("other".to_string(), "resource://c".to_string()));
        }

        assert_eq!(state.resource_subscriptions.len(), 3);

        cleanup_session_state(session_id, &state).await;

        assert!(
            !state
                .resource_subscriptions
                .contains_key(&(session_id.to_string(), "resource://a".to_string()))
        );
        assert!(
            !state
                .resource_subscriptions
                .contains_key(&(session_id.to_string(), "resource://b".to_string()))
        );
        assert!(
            state
                .resource_subscriptions
                .contains_key(&("other".to_string(), "resource://c".to_string()))
        );

        let idx = state.server_resource_index.get("srv-1").expect("index should exist");
        assert!(!idx.contains(&(session_id.to_string(), "resource://a".to_string())));
        assert!(idx.contains(&("other".to_string(), "resource://c".to_string())));
    }

    #[tokio::test]
    async fn cleanup_is_idempotent() {
        let state = create_test_server_state();
        let session_id = "test-idempotent";

        cleanup_session_state(session_id, &state).await;
        cleanup_session_state(session_id, &state).await;
    }

    #[tokio::test]
    async fn session_binding_cleanup_via_resolver() {
        let resolver = Arc::new(SessionBoundClientContextResolver::new());
        let session_id = "test-binding-cleanup";

        let context = ClientContext {
            client_id: "client-1".to_string(),
            session_id: Some(session_id.to_string()),
            profile_id: None,
            config_mode: Some("hosted".to_string()),
            unify_workspace: None,
            surface_fingerprint: None,
            transport: ClientTransport::StreamableHttp,
            source: ClientIdentitySource::ManagedHeader,
            observed_client_info: None,
        };

        resolver
            .bind_session(session_id, &context)
            .await
            .expect("bind should succeed");
        assert!(resolver.session_bindings.contains_key(session_id));

        resolver
            .unbind_session(session_id)
            .await
            .expect("unbind should succeed");
        assert!(!resolver.session_bindings.contains_key(session_id));

        resolver
            .unbind_session(session_id)
            .await
            .expect("unbind should be idempotent");
    }

    #[tokio::test]
    async fn multiple_sessions_cleanup_isolation() {
        let state = create_test_server_state();
        let session_a = "session-a";
        let session_b = "session-b";

        state
            .resource_subscriptions
            .insert((session_a.to_string(), "resource://1".to_string()), "srv".to_string());
        state
            .resource_subscriptions
            .insert((session_b.to_string(), "resource://2".to_string()), "srv".to_string());

        {
            let idx = state.server_resource_index.entry("srv".to_string()).or_default();
            idx.insert((session_a.to_string(), "resource://1".to_string()));
            idx.insert((session_b.to_string(), "resource://2".to_string()));
        }

        cleanup_session_state(session_a, &state).await;

        assert!(
            !state
                .resource_subscriptions
                .contains_key(&(session_a.to_string(), "resource://1".to_string()))
        );
        assert!(
            state
                .resource_subscriptions
                .contains_key(&(session_b.to_string(), "resource://2".to_string()))
        );
    }

    #[test]
    fn client_context_without_session() {
        let context_no_session = ClientContext {
            client_id: "no-session".to_string(),
            session_id: None,
            profile_id: None,
            config_mode: Some("hosted".to_string()),
            unify_workspace: None,
            surface_fingerprint: None,
            transport: ClientTransport::StreamableHttp,
            source: ClientIdentitySource::ManagedHeader,
            observed_client_info: None,
        };

        assert!(context_no_session.session_id.is_none());
    }

    #[test]
    fn client_context_with_session() {
        let context_with_session = ClientContext {
            client_id: "with-session".to_string(),
            session_id: Some("sess-123".to_string()),
            profile_id: None,
            config_mode: Some("hosted".to_string()),
            unify_workspace: None,
            surface_fingerprint: None,
            transport: ClientTransport::StreamableHttp,
            source: ClientIdentitySource::ManagedHeader,
            observed_client_info: None,
        };

        assert_eq!(context_with_session.session_id.as_deref(), Some("sess-123"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_effective_config_mode_prefers_explicit_client_mode() {
        let (_temp_dir, pool, server) = create_mode_resolution_test_server().await;

        set_default_client_config_mode(&pool, "transparent")
            .await
            .expect("set default mode");

        sqlx::query(
            r#"
            INSERT INTO client (id, name, identifier, config_mode, backup_policy, backup_limit)
            VALUES (?, ?, ?, ?, 'keep_n', 5)
            "#,
        )
        .bind(crate::generate_id!("clnt"))
        .bind("Recognized Client")
        .bind("recognized-client")
        .bind("unify")
        .execute(&pool)
        .await
        .expect("insert client row");

        let mode = server
            .resolve_effective_config_mode("recognized-client")
            .await
            .expect("resolve config mode");

        assert_eq!(mode, "unify");
    }

    #[test]
    fn resolve_effective_protocol_version_prefers_explicit_header() {
        let header_version = protocol::CURRENT_VERSION.to_string();
        let negotiated_version = rmcp::model::ProtocolVersion::V_2025_03_26.to_string();

        let resolved =
            ProxyServer::resolve_effective_protocol_version(Some(header_version.as_str()), Some(negotiated_version))
                .expect("explicit header should be accepted");

        assert_eq!(resolved.as_deref(), Some(header_version.as_str()));
    }

    #[test]
    fn resolve_effective_protocol_version_accepts_every_supported_negotiated_version() {
        for protocol_version in protocol::SUPPORTED_DOWNSTREAM_PROTOCOL_VERSION_VALUES {
            let negotiated_version = protocol_version.to_string();

            let resolved = ProxyServer::resolve_effective_protocol_version(None, Some(negotiated_version.clone()))
                .expect("supported negotiated protocol version should be accepted");

            assert_eq!(resolved.as_deref(), Some(negotiated_version.as_str()));
        }
    }

    #[test]
    fn resolve_effective_protocol_version_rejects_unsupported_header() {
        let negotiated_version = rmcp::model::ProtocolVersion::V_2025_03_26.to_string();

        let error = ProxyServer::resolve_effective_protocol_version(Some("2026-07-28"), Some(negotiated_version))
            .expect_err("unsupported explicit header should fail");

        assert_eq!(error.code, rmcp::model::ErrorCode::UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(
            error.data,
            Some(serde_json::json!({
                "requested": "2026-07-28",
                "supported": protocol::SUPPORTED_DOWNSTREAM_PROTOCOL_VERSION_VALUES,
            }))
        );
    }

    #[test]
    fn resolve_effective_protocol_version_returns_none_without_header_or_fallback() {
        let resolved = ProxyServer::resolve_effective_protocol_version(None, None)
            .expect("missing protocol version should not error before enforcement");

        assert_eq!(resolved, None);
    }

    #[tokio::test]
    async fn proxy_server_exposes_only_supported_compatibility_lifecycle() {
        let (_temp_dir, _pool, server) = create_mode_resolution_test_server().await;
        let (context, client_service, server_service) =
            subscription_request_context("protocol-client", "protocol-session").await;

        assert_eq!(
            server.supported_protocol_versions().as_ref(),
            &[
                rmcp::model::ProtocolVersion::V_2025_11_25,
                rmcp::model::ProtocolVersion::V_2025_06_18,
                rmcp::model::ProtocolVersion::V_2025_03_26,
                rmcp::model::ProtocolVersion::V_2024_11_05,
            ]
        );
        let error = server
            .discover(context)
            .await
            .expect_err("compatibility mode must not expose server/discover");
        assert_eq!(error.code, rmcp::model::ErrorCode::METHOD_NOT_FOUND);

        client_service.cancel().await.expect("cancel context client");
        server_service.cancel().await.expect("cancel context server");
    }

    #[tokio::test]
    async fn proxy_initialize_rejects_2026_07_28_before_context_side_effects() {
        let (_temp_dir, pool, server) = create_mode_resolution_test_server().await;
        let (context, client_service, server_service) =
            subscription_request_context("protocol-client", "protocol-session").await;
        let request = InitializeRequestParams::new(
            rmcp::model::ClientCapabilities::default(),
            rmcp::model::Implementation::new("protocol-client", "1.0.0"),
        )
        .with_protocol_version(rmcp::model::ProtocolVersion::V_2026_07_28);

        let error = rmcp::ServerHandler::initialize(&server, request, context)
            .await
            .expect_err("2026-07-28 must be rejected before client context publication");

        assert_eq!(error.code, rmcp::model::ErrorCode::UNSUPPORTED_PROTOCOL_VERSION);
        assert!(
            !server
                .client_context_resolver
                .session_bindings
                .contains_key("protocol-session")
        );
        assert!(!server.downstream_clients.contains_key("protocol-session"));
        let observed_client_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client WHERE identifier = ?")
            .bind("protocol-client")
            .fetch_one(&pool)
            .await
            .expect("count observed clients");
        assert_eq!(observed_client_count, 0);
        client_service.cancel().await.expect("cancel context client");
        server_service.cancel().await.expect("cancel context server");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_effective_config_mode_uses_settings_default_for_recognized_client_without_explicit_mode() {
        let (_temp_dir, pool, server) = create_mode_resolution_test_server().await;

        set_default_client_config_mode(&pool, "transparent")
            .await
            .expect("set default mode");

        sqlx::query(
            r#"
            INSERT INTO client (id, name, identifier, config_mode, backup_policy, backup_limit)
            VALUES (?, ?, ?, ?, 'keep_n', 5)
            "#,
        )
        .bind(crate::generate_id!("clnt"))
        .bind("Recognized Client Without Explicit Mode")
        .bind("recognized-client-with-default")
        .bind(Option::<String>::None)
        .execute(&pool)
        .await
        .expect("insert client row with null mode");

        let mode = server
            .resolve_effective_config_mode("recognized-client-with-default")
            .await
            .expect("resolve config mode");

        assert_eq!(mode, "transparent");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn managed_access_uses_the_effective_inherited_config_mode() {
        let (_temp_dir, pool, server) = create_mode_resolution_test_server().await;

        set_default_client_config_mode(&pool, "unify")
            .await
            .expect("set default mode");
        sqlx::query(
            r#"
            INSERT INTO client (
                id, name, identifier, config_mode, approval_status, backup_policy, backup_limit
            )
            VALUES (?, ?, ?, NULL, 'approved', 'keep_n', 5)
            "#,
        )
        .bind(crate::generate_id!("clnt"))
        .bind("Inherited Managed Client")
        .bind("inherited-managed-client")
        .execute(&pool)
        .await
        .expect("insert client row");

        let context = ClientContext {
            client_id: "inherited-managed-client".to_string(),
            session_id: Some("session-inherited-managed".to_string()),
            profile_id: None,
            config_mode: Some("unify".to_string()),
            unify_workspace: None,
            surface_fingerprint: Some("surface-fingerprint".to_string()),
            transport: ClientTransport::StreamableHttp,
            source: ClientIdentitySource::ManagedHeader,
            observed_client_info: None,
        };

        let access = server
            .resolve_consumer_access_context(&context)
            .await
            .expect("effective managed mode should authorize Surface access");

        assert_eq!(access.consumer_id, "inherited-managed-client");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_effective_config_mode_uses_settings_default_for_unrecognized_client() {
        let (_temp_dir, pool, server) = create_mode_resolution_test_server().await;

        set_default_client_config_mode(&pool, "transparent")
            .await
            .expect("set default mode");

        let mode = server
            .resolve_effective_config_mode("manual-unrecognized-client")
            .await
            .expect("resolve config mode");

        assert_eq!(mode, "transparent");
    }
}
