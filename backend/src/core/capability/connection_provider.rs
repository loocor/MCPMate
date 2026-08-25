//! Typed connection ownership boundary for capability discovery.

use std::{
    sync::{Arc, Weak},
    time::Duration,
};

use async_trait::async_trait;
use rmcp::service::{Peer, RoleClient};
use tokio::sync::Mutex;

use crate::config::database::Database;
use crate::core::{
    capability::{AffinityKey, ConnectionSelection, runtime::ListCtx},
    foundation::types::ConnectionStatus,
    pool::{UpstreamConnectionPool, ValidationReservationLease, ValidationReservationToken},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OwnerSource {
    Existing,
    Fresh,
    Validation,
}

pub(crate) struct CapabilityOwner {
    pub server_id: String,
    pub server_name: String,
    pub config_fingerprint: String,
    pub instance_id: String,
    pub connection_generation: Option<u64>,
    pub peer: Peer<RoleClient>,
    pub startup_tools: Option<Vec<rmcp::model::Tool>>,
    pub source: OwnerSource,
    pub cleanup: Option<CapabilityOwnerCleanup>,
}

pub(crate) struct CapabilityOwnerCleanup {
    reservation: ValidationReservationToken,
    pool: Weak<Mutex<UpstreamConnectionPool>>,
    armed: bool,
}

impl CapabilityOwnerCleanup {
    fn from_lease(
        lease: ValidationReservationLease,
        pool: &Arc<Mutex<UpstreamConnectionPool>>,
    ) -> Self {
        Self {
            reservation: lease.into_persistent_token(),
            pool: Arc::downgrade(pool),
            armed: true,
        }
    }

    async fn release(&mut self) -> Result<(), CapabilityOwnerError> {
        let pool = self.pool.upgrade().ok_or_else(|| CapabilityOwnerError::Other {
            reason: format!(
                "capability pool disappeared before validation session '{}' could be released",
                self.reservation.session_id()
            ),
        })?;
        let shutdown = UpstreamConnectionPool::release_validation_reservation(&pool, &self.reservation).await;
        self.armed = false;
        shutdown.map_err(|error| CapabilityOwnerError::Other {
            reason: format!(
                "failed to release validation session '{}': {}",
                self.reservation.session_id(),
                error
            ),
        })
    }
}

impl Drop for CapabilityOwnerCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(pool) = self.pool.upgrade() else {
            return;
        };
        let reservation = self.reservation.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                session_id = reservation.session_id(),
                "No Tokio runtime is available for best-effort capability owner cleanup"
            );
            return;
        };
        handle.spawn(async move {
            if let Err(error) = UpstreamConnectionPool::release_validation_reservation(&pool, &reservation).await {
                tracing::warn!(
                    session_id = reservation.session_id(),
                    error = %error,
                    "Best-effort capability owner cleanup failed"
                );
            }
        });
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiscoveryRetryDisposition {
    FreshOnce,
    DoNotRetry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CapabilityAuthenticationFailureCode {
    AuthRequired,
    Unauthorized,
    Forbidden,
    InsufficientScope,
}

#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum CapabilityOwnerError {
    #[error("no capability owner is available: {reason}")]
    Missing { reason: String },
    #[error("capability owner is stale: {reason}")]
    Stale { reason: String },
    #[error("capability owner acquisition timed out after {timeout_ms} ms")]
    Timeout { timeout_ms: u128 },
    #[error("capability owner acquisition deferred by backoff for {remaining_ms} ms")]
    Backoff { remaining_ms: u128 },
    #[error("capability owner authentication failed ({code:?}): {reason}")]
    Authentication {
        code: CapabilityAuthenticationFailureCode,
        reason: String,
    },
    #[error("capability owner configuration is invalid: {reason}")]
    Configuration { reason: String },
    #[error("capability owner acquisition failed: {reason}")]
    Other { reason: String },
}

impl CapabilityOwnerError {
    pub(crate) const fn retry_disposition(&self) -> DiscoveryRetryDisposition {
        match self {
            Self::Missing { .. } | Self::Stale { .. } => DiscoveryRetryDisposition::FreshOnce,
            Self::Timeout { .. }
            | Self::Backoff { .. }
            | Self::Authentication { .. }
            | Self::Configuration { .. }
            | Self::Other { .. } => DiscoveryRetryDisposition::DoNotRetry,
        }
    }
}

#[async_trait]
pub(crate) trait CapabilityConnectionProvider: Send + Sync {
    async fn existing_owner(
        &self,
        ctx: &ListCtx,
    ) -> Result<Option<CapabilityOwner>, CapabilityOwnerError>;
    async fn fresh_owner(
        &self,
        ctx: &ListCtx,
    ) -> Result<CapabilityOwner, CapabilityOwnerError>;
    async fn release_owner(
        &self,
        owner: CapabilityOwner,
    ) -> Result<(), CapabilityOwnerError>;
}

pub(crate) struct PoolCapabilityConnectionProvider {
    pool: Arc<Mutex<UpstreamConnectionPool>>,
    database: Arc<Database>,
}

impl PoolCapabilityConnectionProvider {
    pub(crate) fn new(
        pool: Arc<Mutex<UpstreamConnectionPool>>,
        database: Arc<Database>,
    ) -> Self {
        Self { pool, database }
    }

    fn validation_session(ctx: &ListCtx) -> (String, bool) {
        match ctx.validation_session.as_ref() {
            Some(session_id) => (session_id.clone(), false),
            None => (crate::generate_id!("capval"), true),
        }
    }

    fn authentication_status_code(status: reqwest::StatusCode) -> Option<CapabilityAuthenticationFailureCode> {
        match status {
            reqwest::StatusCode::UNAUTHORIZED => Some(CapabilityAuthenticationFailureCode::Unauthorized),
            reqwest::StatusCode::FORBIDDEN => Some(CapabilityAuthenticationFailureCode::Forbidden),
            _ => None,
        }
    }

    pub(crate) fn authentication_failure_code(error: &anyhow::Error) -> Option<CapabilityAuthenticationFailureCode> {
        for source in error.chain() {
            if let Some(code) = source
                .downcast_ref::<reqwest::Error>()
                .and_then(reqwest::Error::status)
                .and_then(Self::authentication_status_code)
            {
                return Some(code);
            }
            let Some(rmcp::service::ClientInitializeError::TransportError { error, .. }) =
                source.downcast_ref::<rmcp::service::ClientInitializeError>()
            else {
                continue;
            };
            let Some(streamable) = error
                .error
                .downcast_ref::<rmcp::transport::streamable_http_client::StreamableHttpError<reqwest::Error>>()
            else {
                continue;
            };
            return match streamable {
                rmcp::transport::streamable_http_client::StreamableHttpError::AuthRequired(_) => {
                    Some(CapabilityAuthenticationFailureCode::AuthRequired)
                }
                rmcp::transport::streamable_http_client::StreamableHttpError::InsufficientScope(_) => {
                    Some(CapabilityAuthenticationFailureCode::InsufficientScope)
                }
                rmcp::transport::streamable_http_client::StreamableHttpError::Client(client_error) => {
                    client_error.status().and_then(Self::authentication_status_code)
                }
                _ => None,
            };
        }
        None
    }

    fn classify_acquisition_error(error: anyhow::Error) -> CapabilityOwnerError {
        if let Some(code) = Self::authentication_failure_code(&error) {
            CapabilityOwnerError::Authentication {
                code,
                reason: error.to_string(),
            }
        } else if let Some(timeout) = error
            .chain()
            .find_map(|source| source.downcast_ref::<crate::core::transport::timeout_policy::ServerStartupTimeout>())
        {
            CapabilityOwnerError::Timeout {
                timeout_ms: timeout.timeout_ms,
            }
        } else if let Some(backoff) = error
            .chain()
            .find_map(|source| source.downcast_ref::<crate::core::pool::ValidationConnectionBackoff>())
        {
            CapabilityOwnerError::Backoff {
                remaining_ms: backoff.remaining_ms,
            }
        } else if let Some(backoff) = error
            .chain()
            .find_map(|source| source.downcast_ref::<crate::core::pool::ProductionConnectionBackoff>())
        {
            CapabilityOwnerError::Backoff {
                remaining_ms: backoff.remaining_ms,
            }
        } else {
            CapabilityOwnerError::Other {
                reason: error.to_string(),
            }
        }
    }

    async fn canonical_server_name(
        &self,
        server_id: &str,
    ) -> Result<String, CapabilityOwnerError> {
        let server = crate::config::server::get_server_by_id(&self.database.pool, server_id)
            .await
            .map_err(|error| CapabilityOwnerError::Other {
                reason: format!("failed to load server '{server_id}' for capability ownership: {error}"),
            })?
            .ok_or_else(|| CapabilityOwnerError::Configuration {
                reason: format!("server '{server_id}' is missing from the canonical database"),
            })?;
        crate::config::server::validate_server_namespace(&server.name).map_err(|error| {
            CapabilityOwnerError::Configuration {
                reason: format!("server '{server_id}' has an invalid canonical namespace: {error}"),
            }
        })?;
        Ok(server.name)
    }

    fn owner_from_validation_session(
        pool: &UpstreamConnectionPool,
        ctx: &ListCtx,
        reservation: &ValidationReservationToken,
        source: OwnerSource,
        cleanup: Option<CapabilityOwnerCleanup>,
    ) -> Result<CapabilityOwner, CapabilityOwnerError> {
        let connection = pool
            .validation_sessions
            .get(reservation.session_id())
            .and_then(|servers| servers.get(&ctx.server_id))
            .ok_or_else(|| CapabilityOwnerError::Missing {
                reason: format!(
                    "validation session '{}' has no owner for server '{}'",
                    reservation.session_id(),
                    ctx.server_id
                ),
            })?;
        if !matches!(connection.status, ConnectionStatus::Ready) {
            return Err(CapabilityOwnerError::Stale {
                reason: format!(
                    "validation owner '{}' for server '{}' is not ready",
                    connection.id, ctx.server_id
                ),
            });
        }
        let service = connection.service.as_ref().ok_or_else(|| CapabilityOwnerError::Stale {
            reason: format!(
                "validation owner '{}' for server '{}' has no RunningService",
                connection.id, ctx.server_id
            ),
        })?;
        if service.is_closed() {
            return Err(CapabilityOwnerError::Stale {
                reason: format!(
                    "validation owner '{}' for server '{}' is closed",
                    connection.id, ctx.server_id
                ),
            });
        }
        let config_fingerprint =
            connection
                .config_fingerprint
                .clone()
                .ok_or_else(|| CapabilityOwnerError::Configuration {
                    reason: format!(
                        "validation owner '{}' for server '{}' has no bound configuration fingerprint",
                        connection.id, ctx.server_id
                    ),
                })?;

        Ok(CapabilityOwner {
            server_id: ctx.server_id.clone(),
            server_name: connection.server_name.clone(),
            config_fingerprint,
            instance_id: connection.id.clone(),
            connection_generation: Some(reservation.generation()),
            peer: service.peer().clone(),
            startup_tools: None,
            source,
            cleanup,
        })
    }

    async fn create_fresh_owner(
        &self,
        ctx: &ListCtx,
        session_id: &str,
        owns_session: bool,
    ) -> Result<CapabilityOwner, CapabilityOwnerError> {
        let lease = UpstreamConnectionPool::ensure_validation_instance(
            &self.pool,
            &ctx.server_id,
            session_id,
            Duration::from_secs(300),
        )
        .await
        .map_err(Self::classify_acquisition_error)?;

        let reservation = lease.token().clone();
        let cleanup = if owns_session {
            Some(CapabilityOwnerCleanup::from_lease(lease, &self.pool))
        } else {
            None
        };
        let pool = self.pool.lock().await;

        let source = if ctx.validation_session.is_some() {
            OwnerSource::Validation
        } else {
            OwnerSource::Fresh
        };
        Self::owner_from_validation_session(&pool, ctx, &reservation, source, cleanup)
    }
}

#[async_trait]
impl CapabilityConnectionProvider for PoolCapabilityConnectionProvider {
    async fn existing_owner(
        &self,
        ctx: &ListCtx,
    ) -> Result<Option<CapabilityOwner>, CapabilityOwnerError> {
        let server_name = self.canonical_server_name(&ctx.server_id).await?;
        let mut pool = self.pool.lock().await;
        if let Some(session_id) = ctx.validation_session.as_ref() {
            let Some(reservation) = pool.current_validation_token(session_id) else {
                return Ok(None);
            };
            let observation = pool.validation_owner_observation(&reservation, &ctx.server_id);
            let owner = Self::owner_from_validation_session(&pool, ctx, &reservation, OwnerSource::Validation, None);
            return match owner {
                Ok(mut owner) => {
                    owner.server_name = server_name;
                    Ok(Some(owner))
                }
                Err(CapabilityOwnerError::Missing { .. }) => Ok(None),
                Err(error @ CapabilityOwnerError::Stale { .. }) => {
                    let observation = observation.expect("stale validation observations retain publication identity");
                    let detached = pool.detach_validation_connection_if_matches(
                        &reservation,
                        &ctx.server_id,
                        observation.owner_epoch,
                    );
                    drop(pool);
                    if let Some(detached) = detached {
                        UpstreamConnectionPool::shutdown_detached_validation_connection(
                            &reservation,
                            &ctx.server_id,
                            detached,
                        )
                        .await
                        .map_err(|shutdown| CapabilityOwnerError::Other {
                            reason: format!(
                                "failed to detach stale validation owner '{}' for server '{}': {shutdown}",
                                reservation.session_id(),
                                ctx.server_id
                            ),
                        })?;
                    }
                    Err(error)
                }
                Err(error) => Err(error),
            };
        }

        let selection = ctx.connection_selection.clone().unwrap_or_else(|| ConnectionSelection {
            server_id: ctx.server_id.clone(),
            affinity_key: AffinityKey::Default,
        });
        if selection.server_id != ctx.server_id {
            return Err(CapabilityOwnerError::Configuration {
                reason: format!(
                    "connection selection targets server '{}' instead of '{}'",
                    selection.server_id, ctx.server_id
                ),
            });
        }
        let Some(instance_id) =
            pool.select_ready_instance_id(&selection)
                .map_err(|error| CapabilityOwnerError::Other {
                    reason: error.to_string(),
                })?
        else {
            return Ok(None);
        };
        let closed_or_owner = {
            let connection =
                pool.get_instance_mut(&ctx.server_id, &instance_id)
                    .map_err(|error| CapabilityOwnerError::Stale {
                        reason: error.to_string(),
                    })?;
            if !matches!(connection.status, ConnectionStatus::Ready) {
                return Err(CapabilityOwnerError::Stale {
                    reason: format!("selected owner '{}' is not ready", instance_id),
                });
            }
            let service = connection.service.as_ref().ok_or_else(|| CapabilityOwnerError::Stale {
                reason: format!("selected owner '{}' has no RunningService", instance_id),
            })?;
            if service.is_closed() {
                Err(format!("selected owner '{instance_id}' is closed"))
            } else {
                let Some(config_fingerprint) = connection.config_fingerprint.clone() else {
                    return Err(CapabilityOwnerError::Configuration {
                        reason: format!("selected owner '{instance_id}' has no bound configuration fingerprint"),
                    });
                };
                let startup_tools = (connection.startup_tools_pending
                    && ctx.operation_timeout
                        >= crate::core::transport::timeout_policy::DEFAULT_CAPABILITY_OPERATION_TIMEOUT)
                    .then(|| {
                        connection.startup_tools_pending = false;
                        connection.tools.clone()
                    });
                Ok((service.peer().clone(), config_fingerprint, startup_tools))
            }
        };
        let (peer, config_fingerprint, startup_tools) = match closed_or_owner {
            Ok(owner) => owner,
            Err(reason) => {
                pool.register_failure(
                    &ctx.server_id,
                    crate::core::pool::FailureKind::RuntimeGone,
                    Some(reason.clone()),
                );
                if let Ok(connection) = pool.get_instance_mut(&ctx.server_id, &instance_id) {
                    if let Some(service) = connection.service.take() {
                        service.cancellation_token().cancel();
                    }
                    connection.update_disconnected();
                }
                if let Some(tokens) = pool.cancellation_tokens.get_mut(&ctx.server_id) {
                    if let Some(token) = tokens.remove(&instance_id) {
                        token.cancel();
                    }
                }
                return Err(CapabilityOwnerError::Stale { reason });
            }
        };
        pool.mark_instance_activity(&ctx.server_id, &instance_id);

        Ok(Some(CapabilityOwner {
            server_id: ctx.server_id.clone(),
            server_name,
            config_fingerprint,
            instance_id,
            connection_generation: None,
            peer,
            startup_tools,
            source: OwnerSource::Existing,
            cleanup: None,
        }))
    }

    async fn fresh_owner(
        &self,
        ctx: &ListCtx,
    ) -> Result<CapabilityOwner, CapabilityOwnerError> {
        if ctx.validation_session.is_none() {
            let selection = ctx.connection_selection.clone().unwrap_or_else(|| ConnectionSelection {
                server_id: ctx.server_id.clone(),
                affinity_key: AffinityKey::Default,
            });
            if selection.server_id != ctx.server_id {
                return Err(CapabilityOwnerError::Configuration {
                    reason: format!(
                        "connection selection targets server '{}' instead of '{}'",
                        selection.server_id, ctx.server_id
                    ),
                });
            }
            let production_managed = self.pool.lock().await.config.mcp_servers.contains_key(&ctx.server_id);
            if production_managed {
                UpstreamConnectionPool::ensure_connected_coordinated(&self.pool, &selection)
                    .await
                    .map_err(Self::classify_acquisition_error)?;
                return self
                    .existing_owner(ctx)
                    .await?
                    .ok_or_else(|| CapabilityOwnerError::Missing {
                        reason: format!(
                            "production startup completed without an owner for server '{}'",
                            ctx.server_id
                        ),
                    });
            }
        }

        let (session_id, owns_session) = Self::validation_session(ctx);
        self.create_fresh_owner(ctx, &session_id, owns_session).await
    }

    async fn release_owner(
        &self,
        mut owner: CapabilityOwner,
    ) -> Result<(), CapabilityOwnerError> {
        let Some(mut cleanup) = owner.cleanup.take() else {
            return Ok(());
        };
        cleanup.release().await
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

    use rmcp::{ServerHandler, ServiceExt};
    use sqlx::sqlite::SqlitePoolOptions;
    use tokio::{
        io::AsyncReadExt,
        net::TcpListener,
        sync::{Barrier, Mutex, Notify},
    };
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    use super::{
        CapabilityAuthenticationFailureCode, CapabilityConnectionProvider, CapabilityOwnerCleanup,
        CapabilityOwnerError, DiscoveryRetryDisposition, OwnerSource, PoolCapabilityConnectionProvider,
    };
    use crate::config::{
        database::Database,
        models::{HttpTransportKind, Server, ServerTransportDraft},
        server::upsert_server_definition,
    };
    use crate::core::{
        capability::{
            AffinityKey, CapabilityType, ConnectionSelection,
            runtime::{ListCtx, NameDomain},
        },
        foundation::types::ConnectionStatus,
        models::Config,
        pool::{UpstreamConnection, UpstreamConnectionPool},
        transport::client::UpstreamClientHandler,
    };

    #[derive(Clone, Default)]
    struct TestServer;

    impl ServerHandler for TestServer {}

    fn list_ctx(
        validation_session: Option<&str>,
        connection_selection: Option<ConnectionSelection>,
    ) -> ListCtx {
        ListCtx {
            capability: CapabilityType::Tools,
            server_id: "server-1".to_string(),
            refresh: None,
            operation_timeout: Duration::from_secs(1),
            validation_session: validation_session.map(str::to_string),
            runtime_identity: None,
            connection_selection,
            visibility_snapshot: None,
            name_domain: NameDomain::Upstream,
        }
    }

    fn empty_pool() -> UpstreamConnectionPool {
        UpstreamConnectionPool::new(
            Arc::new(Config {
                mcp_servers: HashMap::new(),
                pagination: None,
            }),
            None,
        )
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
        Arc::new(Database {
            pool,
            path: PathBuf::new(),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        })
    }

    async fn insert_stdio_server(
        database: &Database,
        server_id: &str,
        name: &str,
        command: &str,
    ) {
        let mut server = Server::new_stdio(name.to_string(), Some(command.to_string()));
        server.id = Some(server_id.to_string());
        upsert_server_definition(
            &database.pool,
            &server,
            &ServerTransportDraft::Stdio {
                command: Some(command.to_string()),
                args: Vec::new(),
                env: Default::default(),
            },
        )
        .await
        .expect("insert typed stdio server fixture");
    }

    async fn insert_streamable_http_server(
        database: &Database,
        server_id: &str,
        name: &str,
        endpoint: String,
    ) {
        let mut server = Server::new_streamable_http(name.to_string(), Some(endpoint.clone()));
        server.id = Some(server_id.to_string());
        upsert_server_definition(
            &database.pool,
            &server,
            &ServerTransportDraft::Http {
                protocol: HttpTransportKind::StreamableHttp,
                endpoint: Some(endpoint),
                headers: Default::default(),
            },
        )
        .await
        .expect("insert typed HTTP server fixture");
    }

    async fn connected_instance() -> (UpstreamConnection, tokio::task::JoinHandle<anyhow::Result<()>>) {
        let (server_transport, client_transport) = tokio::io::duplex(4096);
        let server_handle = tokio::spawn(async move {
            let service = TestServer.serve(server_transport).await?;
            service.waiting().await?;
            Ok(())
        });
        let service = UpstreamClientHandler::new("selected-server".to_string())
            .serve(client_transport)
            .await
            .expect("client should initialize");
        let mut connection = UpstreamConnection::new("selected-server".to_string());
        connection.update_connected(service, Vec::new(), Some(rmcp::model::ServerCapabilities::default()));
        connection.config_fingerprint = Some("test-config".to_string());
        (connection, server_handle)
    }

    #[tokio::test]
    async fn dropping_owned_cleanup_schedules_best_effort_session_destruction() {
        let pool = Arc::new(Mutex::new(empty_pool()));
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "drop-session", Duration::from_secs(60)).await;
        let cleanup = CapabilityOwnerCleanup::from_lease(lease, &pool);
        drop(cleanup);
        tokio::task::yield_now().await;

        let guard = pool.lock().await;
        assert!(!guard.validation_sessions.contains_key("drop-session"));
        assert!(!guard.validation_expirations.contains_key("drop-session"));
    }

    #[tokio::test]
    async fn successful_release_shuts_down_the_real_validation_owner() {
        let pool = Arc::new(Mutex::new(empty_pool()));
        let (connection, server_handle) = connected_instance().await;
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "release-session", Duration::from_secs(60)).await;
        {
            let mut guard = pool.lock().await;
            guard
                .validation_sessions
                .entry("release-session".to_string())
                .or_default()
                .insert("server-1".to_string(), connection);
            guard.validation_expirations.insert(
                "release-session".to_string(),
                std::time::Instant::now() + Duration::from_secs(60),
            );
        }

        let mut cleanup = CapabilityOwnerCleanup::from_lease(lease, &pool);
        cleanup.release().await.expect("release validation owner");

        let guard = pool.lock().await;
        assert!(!guard.validation_sessions.contains_key("release-session"));
        assert!(!guard.validation_expirations.contains_key("release-session"));
        drop(guard);
        server_handle
            .await
            .expect("server task should join")
            .expect("server should stop");
    }

    #[tokio::test]
    async fn aborting_release_while_the_pool_is_contended_still_cleans_the_owner() {
        let pool = Arc::new(Mutex::new(empty_pool()));
        let (connection, server_handle) = connected_instance().await;
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "contended-session", Duration::from_secs(60))
                .await;
        {
            let mut guard = pool.lock().await;
            guard
                .validation_sessions
                .entry("contended-session".to_string())
                .or_default()
                .insert("server-1".to_string(), connection);
            guard.validation_expirations.insert(
                "contended-session".to_string(),
                std::time::Instant::now() + Duration::from_secs(60),
            );
        }

        let guard = pool.lock().await;
        let barrier = Arc::new(Barrier::new(2));
        let task_pool = pool.clone();
        let task_barrier = barrier.clone();
        let release = tokio::spawn(async move {
            let mut cleanup = CapabilityOwnerCleanup::from_lease(lease, &task_pool);
            task_barrier.wait().await;
            cleanup.release().await
        });
        barrier.wait().await;
        tokio::task::yield_now().await;
        release.abort();
        assert!(release.await.expect_err("release should be aborted").is_cancelled());
        drop(guard);

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !pool.lock().await.validation_sessions.contains_key("contended-session") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drop fallback should detach the validation session");
        server_handle
            .await
            .expect("server task should join")
            .expect("server should stop");
    }

    #[tokio::test]
    async fn slow_validation_initialization_does_not_hold_the_pool_mutex() {
        let database = test_database().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fixture");
        let address = listener.local_addr().expect("fixture address");
        insert_streamable_http_server(&database, "server-1", "slow_fixture", format!("http://{address}/mcp")).await;
        let request_seen = Arc::new(Notify::new());
        let server_seen = request_seen.clone();
        let fixture = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept validation connection");
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).await.expect("read initialize request");
            assert!(read > 0);
            server_seen.notify_one();
            std::future::pending::<()>().await;
        });

        let mut raw_pool = empty_pool();
        raw_pool.database = Some(database);
        let pool = Arc::new(Mutex::new(raw_pool));
        let task_pool = pool.clone();
        let acquisition = tokio::spawn(async move {
            UpstreamConnectionPool::ensure_validation_instance(
                &task_pool,
                "server-1",
                "slow-session",
                Duration::from_secs(60),
            )
            .await
        });
        request_seen.notified().await;

        let guard = tokio::time::timeout(Duration::from_millis(100), pool.lock())
            .await
            .expect("pool mutex must remain available during initialize");
        drop(guard);
        acquisition.abort();
        match acquisition.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("acquisition should be aborted"),
        }
        assert!(
            !pool
                .lock()
                .await
                .validation_sessions
                .get("slow-session")
                .is_some_and(|servers| servers.contains_key("server-1")),
            "an aborted initialization must not publish an unobserved owner"
        );
        fixture.abort();
        assert!(fixture.await.expect_err("fixture should be stopped").is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !pool.lock().await.validation_sessions.contains_key("slow-session") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("aborted acquisition lease should clean its validation reservation");
    }

    #[tokio::test]
    async fn closing_reservation_cancels_slow_validation_ensure() {
        let database = test_database().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fixture");
        let address = listener.local_addr().expect("fixture address");
        insert_streamable_http_server(
            &database,
            "server-1",
            "slow_cancel_fixture",
            format!("http://{address}/mcp"),
        )
        .await;
        let request_seen = Arc::new(Notify::new());
        let server_seen = request_seen.clone();
        let fixture = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept validation connection");
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).await.expect("read initialize request");
            assert!(read > 0);
            server_seen.notify_one();
            std::future::pending::<()>().await;
        });

        let mut raw_pool = empty_pool();
        raw_pool.database = Some(database);
        let pool = Arc::new(Mutex::new(raw_pool));
        let creator =
            UpstreamConnectionPool::reserve_validation_session(&pool, "slow-close-session", Duration::from_secs(60))
                .await;
        let token = creator.token().clone();
        let task_pool = pool.clone();
        let acquisition = tokio::spawn(async move {
            UpstreamConnectionPool::ensure_validation_instance(
                &task_pool,
                "server-1",
                "slow-close-session",
                Duration::from_secs(60),
            )
            .await
        });
        request_seen.notified().await;

        UpstreamConnectionPool::release_validation_reservation(&pool, &token)
            .await
            .expect("release slow reservation");
        let result = tokio::time::timeout(Duration::from_secs(1), acquisition)
            .await
            .expect("release must cancel slow ensure")
            .expect("ensure task should join");
        assert!(result.is_err(), "cancelled ensure must not publish");
        assert!(!pool.lock().await.validation_sessions.contains_key("slow-close-session"));
        drop(creator);
        fixture.abort();
        assert!(fixture.await.expect_err("fixture should stop").is_cancelled());
    }

    #[tokio::test]
    async fn fresh_http_initialize_auth_failure_remains_typed_and_does_not_retry() {
        for (status, expected_code) in [
            (401, CapabilityAuthenticationFailureCode::AuthRequired),
            (403, CapabilityAuthenticationFailureCode::InsufficientScope),
        ] {
            let upstream = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).insert_header(
                    "www-authenticate",
                    "Bearer resource_metadata=\"https://example.test/.well-known/oauth-protected-resource\"",
                ))
                .mount(&upstream)
                .await;
            let database = test_database().await;
            insert_streamable_http_server(&database, "server-1", "auth_fixture", upstream.uri()).await;
            let mut raw_pool = empty_pool();
            raw_pool.database = Some(database.clone());
            let provider = PoolCapabilityConnectionProvider::new(Arc::new(Mutex::new(raw_pool)), database);

            let error = match provider.fresh_owner(&list_ctx(None, None)).await {
                Err(error) => error,
                Ok(_) => panic!("{status} initialize must fail acquisition"),
            };

            assert!(matches!(
                error,
                CapabilityOwnerError::Authentication { code, .. } if code == expected_code
            ));
            assert_eq!(error.retry_disposition(), DiscoveryRetryDisposition::DoNotRetry);
        }
    }

    #[tokio::test]
    async fn production_backoff_remains_typed_for_management_discovery() {
        let database = test_database().await;
        insert_stdio_server(&database, "server-1", "backoff_fixture", "missing-command").await;
        let fingerprint = crate::config::server::capabilities::current_config_fingerprint(&database.pool, "server-1")
            .await
            .expect("load backoff fixture fingerprint");
        let mut raw_pool = empty_pool();
        raw_pool.database = Some(database.clone());
        raw_pool.config = Arc::new(Config {
            mcp_servers: HashMap::from([(
                "server-1".to_string(),
                crate::core::models::MCPServerConfig {
                    source_fingerprint: Some(fingerprint),
                    kind: crate::common::server::ServerType::Stdio,
                    command: Some("missing-command".to_string()),
                    args: None,
                    url: None,
                    env: None,
                    headers: None,
                },
            )]),
            pagination: None,
        });
        raw_pool.register_failure(
            "server-1",
            crate::core::pool::FailureKind::Connect,
            Some("test backoff".to_string()),
        );
        let provider = PoolCapabilityConnectionProvider::new(Arc::new(Mutex::new(raw_pool)), database);

        let error = match provider.fresh_owner(&list_ctx(None, None)).await {
            Err(error) => error,
            Ok(_) => panic!("production backoff must reject acquisition"),
        };

        assert!(matches!(error, CapabilityOwnerError::Backoff { .. }));
        assert_eq!(error.retry_disposition(), DiscoveryRetryDisposition::DoNotRetry);
    }

    #[test]
    fn validation_backoff_remains_typed() {
        let error = PoolCapabilityConnectionProvider::classify_acquisition_error(
            crate::core::pool::ValidationConnectionBackoff {
                server_name: "auth_fixture".to_string(),
                remaining_ms: 10_000,
            }
            .into(),
        );

        assert!(matches!(error, CapabilityOwnerError::Backoff { remaining_ms: 10_000 }));
        assert_eq!(error.retry_disposition(), DiscoveryRetryDisposition::DoNotRetry);
    }

    #[tokio::test]
    async fn explicit_validation_owner_is_not_classified_as_existing_production() {
        let database = test_database().await;
        insert_stdio_server(&database, "server-1", "docs", "fixture-command").await;
        let pool = Arc::new(Mutex::new(empty_pool()));
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "inspector-session", Duration::from_secs(60))
                .await;
        let token = lease.into_persistent_token();
        let (connection, server_handle) = connected_instance().await;
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &token,
            "server-1",
            connection,
            Duration::from_secs(60),
        )
        .await
        .expect("publish validation owner");
        let provider = PoolCapabilityConnectionProvider::new(pool.clone(), database);

        let owner = provider
            .existing_owner(&list_ctx(Some(token.session_id()), None))
            .await
            .expect("owner lookup")
            .expect("validation owner");

        assert_eq!(owner.source, OwnerSource::Validation);
        assert_eq!(owner.connection_generation, Some(token.generation()));
        drop(owner);
        UpstreamConnectionPool::release_validation_reservation(&pool, &token)
            .await
            .expect("release validation owner");
        server_handle
            .await
            .expect("server task should join")
            .expect("server should stop");
    }

    #[tokio::test]
    async fn closed_validation_owner_is_detached_before_fresh_replacement() {
        let database = test_database().await;
        insert_stdio_server(&database, "server-1", "docs", "fixture-command").await;
        let pool = Arc::new(Mutex::new(empty_pool()));
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "stale-session", Duration::from_secs(60)).await;
        let token = lease.into_persistent_token();
        let (connection, stale_server) = connected_instance().await;
        let service = connection.service.as_ref().expect("stale service").clone();
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &token,
            "server-1",
            connection,
            Duration::from_secs(60),
        )
        .await
        .expect("publish stale owner");
        service.cancellation_token().cancel();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !service.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("validation service should close");
        drop(service);
        let provider = PoolCapabilityConnectionProvider::new(pool.clone(), database);
        let ctx = list_ctx(Some(token.session_id()), None);

        let error = match provider.existing_owner(&ctx).await {
            Err(error) => error,
            Ok(_) => panic!("closed validation owner should be typed stale"),
        };
        assert!(matches!(error, CapabilityOwnerError::Stale { .. }));
        assert!(
            !pool
                .lock()
                .await
                .validation_sessions
                .get(token.session_id())
                .is_some_and(|servers| servers.contains_key("server-1"))
        );
        stale_server
            .await
            .expect("stale server task should join")
            .expect("stale server should stop");

        let (replacement, replacement_server) = connected_instance().await;
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &token,
            "server-1",
            replacement,
            Duration::from_secs(60),
        )
        .await
        .expect("publish replacement");
        let owner = provider
            .existing_owner(&ctx)
            .await
            .expect("replacement lookup")
            .expect("replacement owner");
        assert_eq!(owner.connection_generation, Some(token.generation()));
        assert_eq!(owner.source, OwnerSource::Validation);
        drop(owner);
        UpstreamConnectionPool::release_validation_reservation(&pool, &token)
            .await
            .expect("release replacement");
        replacement_server
            .await
            .expect("replacement server task should join")
            .expect("replacement should stop");
    }

    #[tokio::test]
    async fn temporary_guard_keeps_cleanup_authority_after_fresh_joiner() {
        let database = test_database().await;
        insert_stdio_server(&database, "server-1", "docs", "fixture-command").await;
        let pool = Arc::new(Mutex::new(empty_pool()));
        let guard =
            UpstreamConnectionPool::reserve_validation_session(&pool, "temporary-stale", Duration::from_secs(60)).await;
        let token = guard.token().clone();
        let (stale_connection, stale_server) = connected_instance().await;
        let stale_service = stale_connection.service.as_ref().expect("stale service").clone();
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &token,
            "server-1",
            stale_connection,
            Duration::from_secs(60),
        )
        .await
        .expect("publish stale owner");
        stale_service.cancellation_token().cancel();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !stale_service.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("validation service should close");
        drop(stale_service);
        let provider = PoolCapabilityConnectionProvider::new(pool.clone(), database);
        let ctx = list_ctx(Some(token.session_id()), None);

        assert!(matches!(
            provider.existing_owner(&ctx).await,
            Err(CapabilityOwnerError::Stale { .. })
        ));
        stale_server
            .await
            .expect("stale server task should join")
            .expect("stale server should stop");
        let (replacement, replacement_server) = connected_instance().await;
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &token,
            "server-1",
            replacement,
            Duration::from_secs(60),
        )
        .await
        .expect("publish replacement");

        let owner = provider.fresh_owner(&ctx).await.expect("join replacement owner");
        assert!(owner.cleanup.is_none());
        drop(owner);
        drop(guard);

        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.lock().await.validation_sessions.contains_key(token.session_id()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("temporary guard cancellation must retain cleanup authority");
        {
            let pool = pool.lock().await;
            assert!(!pool.validation_expirations.contains_key(token.session_id()));
        }
        replacement_server
            .await
            .expect("replacement server task should join")
            .expect("replacement should stop");
    }

    #[tokio::test]
    async fn existing_owner_respects_connection_selection() {
        let database = test_database().await;
        insert_stdio_server(&database, "server-1", "selected_server", "fixture-command").await;
        let (mut connection, server_handle) = connected_instance().await;
        connection.id = "selected-instance".to_string();
        connection.last_activity -= Duration::from_secs(30);
        let previous_activity = connection.last_activity;
        let mut pool = empty_pool();
        pool.database = Some(database.clone());
        pool.client_bound_connections
            .entry(("server-1".to_string(), "client-1".to_string()))
            .or_default()
            .insert(connection.id.clone(), connection);
        let pool = Arc::new(Mutex::new(pool));
        let provider = PoolCapabilityConnectionProvider::new(pool.clone(), database);
        let ctx = list_ctx(
            None,
            Some(ConnectionSelection {
                server_id: "server-1".to_string(),
                affinity_key: AffinityKey::PerClient("client-1".to_string()),
            }),
        );

        let owner = provider
            .existing_owner(&ctx)
            .await
            .expect("owner lookup should succeed")
            .expect("selected owner should exist");

        assert_eq!(owner.instance_id, "selected-instance");
        assert_eq!(owner.server_name, "selected_server");
        assert_eq!(owner.source, OwnerSource::Existing);
        assert_eq!(owner.connection_generation, None);
        assert!(owner.cleanup.is_none());
        drop(owner);

        let mut guard = pool.lock().await;
        let mut connection = guard
            .client_bound_connections
            .get_mut(&("server-1".to_string(), "client-1".to_string()))
            .expect("bound server map")
            .remove("selected-instance")
            .expect("selected connection");
        assert!(connection.last_activity > previous_activity);
        let service = connection.service.take().expect("service owner");
        drop(guard);
        Arc::try_unwrap(service)
            .expect("test should retain the only RunningService owner")
            .cancel()
            .await
            .expect("client should cancel");
        server_handle
            .await
            .expect("server task should join")
            .expect("server should stop");
    }

    #[tokio::test]
    async fn existing_owner_keeps_the_configuration_bound_to_its_connection() {
        let database = test_database().await;
        insert_stdio_server(&database, "server-1", "selected_server", "old-command").await;
        let old_fingerprint =
            crate::config::server::capabilities::current_config_fingerprint(&database.pool, "server-1")
                .await
                .expect("load original fingerprint");
        let (mut connection, server_handle) = connected_instance().await;
        connection.id = "selected-instance".to_string();
        connection.config_fingerprint = Some(old_fingerprint.clone());
        let mut raw_pool = empty_pool();
        raw_pool.database = Some(database.clone());
        raw_pool
            .connections
            .entry("server-1".to_string())
            .or_default()
            .insert(connection.id.clone(), connection);
        insert_stdio_server(&database, "server-1", "selected_server", "new-command").await;
        let pool = Arc::new(Mutex::new(raw_pool));
        let provider = PoolCapabilityConnectionProvider::new(pool.clone(), database.clone());

        let owner = provider
            .existing_owner(&list_ctx(None, None))
            .await
            .expect("owner lookup should succeed")
            .expect("existing owner should exist");

        assert_eq!(owner.config_fingerprint, old_fingerprint);
        let error = crate::config::server::capabilities::commit_capability_protocol_observation(
            &database.pool,
            database.capability_cache.as_ref(),
            &owner.server_id,
            &owner.server_name,
            &owner.config_fingerprint,
            crate::config::server::capabilities::CapabilityProtocolObservation {
                initialize: None,
                tools: Vec::new(),
                resources: Vec::new(),
                prompts: Vec::new(),
                templates: Vec::new(),
                kinds: crate::core::pool::CapSyncFlags::TOOLS,
                kind_states: Vec::new(),
            },
        )
        .await
        .expect_err("stale connection configuration must be rejected");
        assert!(
            error
                .downcast_ref::<crate::config::server::capabilities::CapabilityConfigurationChanged>()
                .is_some()
        );
        drop(owner);

        let mut guard = pool.lock().await;
        let mut connection = guard
            .connections
            .get_mut("server-1")
            .expect("server map")
            .remove("selected-instance")
            .expect("selected connection");
        let service = connection.service.take().expect("service owner");
        drop(guard);
        Arc::try_unwrap(service)
            .expect("test should retain the only RunningService owner")
            .cancel()
            .await
            .expect("client should cancel");
        server_handle
            .await
            .expect("server task should join")
            .expect("server should stop");
    }

    #[tokio::test]
    async fn existing_owner_uses_the_canonical_database_namespace() {
        let database = test_database().await;
        insert_stdio_server(&database, "server-1", "inspector_fixture", "fixture-command").await;
        let (mut connection, server_handle) = connected_instance().await;
        connection.id = "selected-instance".to_string();
        connection.server_name = "SERVmixedCaseId".to_string();
        let mut pool = empty_pool();
        pool.database = Some(database.clone());
        pool.connections
            .entry("server-1".to_string())
            .or_default()
            .insert(connection.id.clone(), connection);
        let pool = Arc::new(Mutex::new(pool));
        let provider = PoolCapabilityConnectionProvider::new(pool.clone(), database);

        let owner = provider
            .existing_owner(&list_ctx(None, None))
            .await
            .expect("owner lookup should succeed")
            .expect("existing owner should exist");

        assert_eq!(owner.server_name, "inspector_fixture");
        drop(owner);
        let mut guard = pool.lock().await;
        let mut connection = guard
            .connections
            .get_mut("server-1")
            .expect("server map")
            .remove("selected-instance")
            .expect("selected connection");
        let service = connection.service.take().expect("service owner");
        drop(guard);
        Arc::try_unwrap(service)
            .expect("test should retain the only RunningService owner")
            .cancel()
            .await
            .expect("client should cancel");
        server_handle
            .await
            .expect("server task should join")
            .expect("server should stop");
    }

    #[tokio::test]
    async fn closed_ready_owner_is_quarantined_before_the_next_selection() {
        let database = test_database().await;
        insert_stdio_server(&database, "server-1", "docs", "fixture-command").await;
        let (mut connection, server_handle) = connected_instance().await;
        connection.id = "closed-instance".to_string();
        let service = connection.service.as_ref().expect("service owner").clone();
        service.cancellation_token().cancel();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !service.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("service should close");
        drop(service);
        let mut raw_pool = empty_pool();
        raw_pool.database = Some(database.clone());
        raw_pool
            .connections
            .entry("server-1".to_string())
            .or_default()
            .insert(connection.id.clone(), connection);
        let pool = Arc::new(Mutex::new(raw_pool));
        let provider = PoolCapabilityConnectionProvider::new(pool.clone(), database);
        let ctx = list_ctx(None, None);

        let first = match provider.existing_owner(&ctx).await {
            Err(error) => error,
            Ok(_) => panic!("closed Ready owner should be typed stale"),
        };
        assert!(matches!(first, CapabilityOwnerError::Stale { .. }));
        assert!(provider.existing_owner(&ctx).await.expect("second selection").is_none());

        let guard = pool.lock().await;
        let failure = guard.failure_states.get("server-1").expect("runtime failure state");
        assert_eq!(failure.last_kind, Some(crate::core::pool::FailureKind::RuntimeGone));
        let connection = guard
            .connections
            .get("server-1")
            .and_then(|connections| connections.get("closed-instance"))
            .expect("quarantined connection remains diagnosable");
        assert!(connection.service.is_none());
        assert!(!matches!(connection.status, ConnectionStatus::Ready));
        drop(guard);
        server_handle
            .await
            .expect("server task should join")
            .expect("server should stop");
    }

    #[test]
    fn typed_startup_timeout_preserves_the_timeout_evidence() {
        let error =
            anyhow::Error::new(crate::core::transport::timeout_policy::ServerStartupTimeout { timeout_ms: 321 })
                .context("validation owner startup failed");

        assert!(matches!(
            PoolCapabilityConnectionProvider::classify_acquisition_error(error),
            CapabilityOwnerError::Timeout { timeout_ms: 321 }
        ));
    }

    #[test]
    fn ordinary_fresh_sessions_are_unique_while_explicit_sessions_are_preserved() {
        let ordinary = list_ctx(None, None);
        let (first, first_owned) = PoolCapabilityConnectionProvider::validation_session(&ordinary);
        let (second, second_owned) = PoolCapabilityConnectionProvider::validation_session(&ordinary);

        assert_ne!(first, second);
        assert!(first_owned);
        assert!(second_owned);

        let explicit = list_ctx(Some("inspector-session"), None);
        let (session, owned) = PoolCapabilityConnectionProvider::validation_session(&explicit);
        assert_eq!(session, "inspector-session");
        assert!(!owned);
    }
}
