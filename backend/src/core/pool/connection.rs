//! Core connection pool implementation separated from module index.

use std::{
    collections::HashMap,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{self, Result};
use mcpmate_secrets::SecretResolver;
use rmcp::service::{Peer, RoleClient};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing;

use tokio::sync::Mutex;

use crate::core::{
    foundation::{monitor::ProcessMonitor, types::ConnectionStatus},
    models::Config,
    secrets::store::LocalSecretStore,
};

const STANDARD_INSTANCE_IDLE_SECS: u64 = 5 * 60;

use super::config::PoolConfigManager;
use super::sync::ServerSyncManager;
use super::types::{self, FailureKind};

type InstanceSnapshot = (String, ConnectionStatus, bool, bool, Option<Peer<RoleClient>>);
type SnapshotMap = HashMap<String, Vec<InstanceSnapshot>>;

const VALIDATION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
#[error("Server '{server_name}' is backing off for {remaining_ms} ms")]
pub(crate) struct ValidationConnectionBackoff {
    pub server_name: String,
    pub remaining_ms: u128,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidationReservationToken {
    session_id: Arc<str>,
    generation: u64,
}

impl ValidationReservationToken {
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ValidationOwnerEpoch(u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidationOwnerObservation {
    pub(crate) owner_epoch: ValidationOwnerEpoch,
}

#[derive(Clone, Debug)]
struct ValidationReservationState {
    generation: u64,
    expires_at: Instant,
    authority: Arc<ValidationReservationAuthority>,
    in_flight: HashMap<u64, (String, CancellationToken)>,
}

#[derive(Debug)]
struct ValidationReservationAuthority {
    holders: AtomicUsize,
    committed: AtomicBool,
}

pub(crate) struct ValidationReservationLease {
    token: ValidationReservationToken,
    authority: Arc<ValidationReservationAuthority>,
    pool: Weak<Mutex<UpstreamConnectionPool>>,
    armed: bool,
}

impl ValidationReservationLease {
    pub(crate) fn token(&self) -> &ValidationReservationToken {
        &self.token
    }

    pub(crate) fn into_persistent_token(mut self) -> ValidationReservationToken {
        self.authority.committed.store(true, Ordering::SeqCst);
        self.authority.holders.fetch_sub(1, Ordering::SeqCst);
        self.armed = false;
        self.token.clone()
    }
}

impl Drop for ValidationReservationLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let was_last_holder = self.authority.holders.fetch_sub(1, Ordering::SeqCst) == 1;
        if !was_last_holder || self.authority.committed.load(Ordering::SeqCst) {
            return;
        }
        let Some(pool) = self.pool.upgrade() else {
            return;
        };
        let token = self.token.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(error) = UpstreamConnectionPool::release_validation_reservation(&pool, &token).await {
                    tracing::warn!(error = %error, session_id = token.session_id(), "Validation reservation cleanup failed");
                }
            });
        }
    }
}

struct ValidationAttemptGuard {
    token: ValidationReservationToken,
    attempt_id: u64,
    server_id: String,
    cancellation: CancellationToken,
    pool: Weak<Mutex<UpstreamConnectionPool>>,
    armed: bool,
}

enum ValidationSuccessFinalization {
    Published,
    Joined(types::UpstreamConnection),
    Lost(types::UpstreamConnection),
}

impl ValidationAttemptGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ValidationAttemptGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.cancellation.cancel();
        let Some(pool) = self.pool.upgrade() else {
            return;
        };
        let token = self.token.clone();
        let attempt_id = self.attempt_id;
        let server_id = self.server_id.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                pool.lock()
                    .await
                    .remove_validation_attempt(&token, attempt_id, &server_id);
            });
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ValidationReservationError {
    #[error("validation reservation '{session_id}' generation {generation} was invalidated")]
    Invalidated { session_id: String, generation: u64 },
    #[error(transparent)]
    Shutdown(#[from] ValidationShutdownError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ValidationShutdownError {
    #[error("validation owner shutdown timed out after {timeout_ms} ms")]
    Timeout { timeout_ms: u128 },
    #[error("validation owner shutdown failed: {source}")]
    Operation {
        #[source]
        source: anyhow::Error,
    },
    #[error("validation shutdown task failed: {source}")]
    Join {
        #[source]
        source: tokio::task::JoinError,
    },
}

/// Build-in lightweight HTTP client registry kept at pool layer.
///
/// The registry maps an origin (scheme://host[:port]) to a shared reqwest::Client
/// enabling TCP/TLS/HTTP2 reuse across requests to the same upstream.
/// It's intentionally located in the pool to keep strategy/state close to
/// connection/backoff logic while keeping transport layer stateless.
#[derive(Clone, Debug)]
pub(crate) struct HttpClientRegistry {
    inner: Arc<tokio::sync::RwLock<std::collections::HashMap<String, reqwest::Client>>>,
}

impl HttpClientRegistry {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        }
    }

    /// Derive a reuse key from URL string (scheme://host[:port])
    pub(crate) fn origin_key(url: &str) -> Option<String> {
        if let Ok(parsed) = url::Url::parse(url) {
            let scheme = parsed.scheme();
            if let Some(host) = parsed.host_str() {
                let port = parsed.port().map(|p| format!(":{}", p)).unwrap_or_default();
                return Some(format!("{}://{}{}", scheme, host, port));
            }
        }
        None
    }

    /// Get or create a reqwest Client for the given origin key
    pub(crate) async fn get_or_create(
        &self,
        origin: &str,
    ) -> reqwest::Client {
        {
            let map = self.inner.read().await;
            if let Some(c) = map.get(origin) {
                return c.clone();
            }
        }

        let client = Self::build_client();
        let mut map = self.inner.write().await;
        let entry = map.entry(origin.to_string()).or_insert_with(|| client.clone());
        entry.clone()
    }

    fn build_client() -> reqwest::Client {
        // Environment-configurable HTTP pooling and timeouts (with sensible defaults)
        let idle_ms = std::env::var("MCPMATE_HTTP_POOL_IDLE_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(45_000);
        let max_idle = std::env::var("MCPMATE_HTTP_POOL_MAX_IDLE_PER_HOST")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(8);
        let keepalive_ms = std::env::var("MCPMATE_HTTP_TCP_KEEPALIVE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30_000);
        let connect_ms = std::env::var("MCPMATE_HTTP_CONNECT_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60_000);
        let request_ms = std::env::var("MCPMATE_HTTP_REQUEST_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60_000);
        let accept_invalid = std::env::var("MCPMATE_ACCEPT_INVALID_CERTS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        let user_agent = std::env::var("MCPMATE_USER_AGENT").unwrap_or_else(|_| "MCPMate/1.0".to_string());

        reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_millis(idle_ms))
            .pool_max_idle_per_host(max_idle)
            .tcp_keepalive(std::time::Duration::from_millis(keepalive_ms))
            .connect_timeout(std::time::Duration::from_millis(connect_ms))
            .danger_accept_invalid_certs(accept_invalid)
            .timeout(std::time::Duration::from_millis(request_ms))
            .user_agent(user_agent)
            .build()
            .expect("Failed to build shared reqwest Client")
    }
}

/// Pool of connections to upstream MCP servers
///
/// This is the core connection pool that manages active connections to upstream MCP servers.
/// It focuses purely on connection storage, access, and basic lifecycle management.
/// Business logic for configuration synchronization and server management is handled
/// by dedicated managers (PoolConfigManager and ServerSyncManager).
#[derive(Debug, Clone)]
pub struct UpstreamConnectionPool {
    /// Map of server ID to map of instance ID to connection (shared production instances)
    pub connections: HashMap<String, HashMap<String, types::UpstreamConnection>>,
    /// Production routes keyed by (server_id, affinity_key) -> instance_id
    pub production_routes: HashMap<types::ProductionRouteKey, String>,
    /// Client-bound connections: (server_id, client_id) -> instance_id -> connection
    pub client_bound_connections: HashMap<(String, String), HashMap<String, types::UpstreamConnection>>,
    /// Exploration sessions: session_id -> map of server_id to connection (minimal skeleton)
    pub exploration_sessions: HashMap<String, HashMap<String, types::UpstreamConnection>>,
    /// Validation sessions: session_id -> map of server_id to connection (minimal skeleton)
    pub validation_sessions: HashMap<String, HashMap<String, types::UpstreamConnection>>,
    validation_owner_epochs: HashMap<String, HashMap<String, ValidationOwnerEpoch>>,
    /// Exploration session expirations
    pub exploration_expirations: HashMap<String, std::time::Instant>,
    /// Validation session expirations
    pub validation_expirations: HashMap<String, std::time::Instant>,
    validation_reservations: HashMap<String, ValidationReservationState>,
    next_validation_reservation: Arc<AtomicU64>,
    next_validation_attempt: Arc<AtomicU64>,
    next_validation_owner_epoch: Arc<AtomicU64>,
    health_reconnect_epochs: HashMap<(String, String), u64>,
    next_health_reconnect_epoch: Arc<AtomicU64>,
    /// In-flight production startup attempts keyed by production route identity.
    pub(crate) startup_attempts: HashMap<types::ProductionRouteKey, super::startup::StartupAttemptEntry>,
    pub(crate) next_startup_attempt: Arc<AtomicU64>,
    pub(crate) preview_owners: HashMap<super::runtime::PreviewOwnerKey, super::runtime::PreviewOwnerEntry>,
    pub(crate) preview_attempts: HashMap<super::runtime::PreviewOwnerKey, super::runtime::PreviewAttemptEntry>,
    pub(crate) next_preview_attempt: Arc<AtomicU64>,
    pub(crate) server_lifecycle_generations: HashMap<String, u64>,
    /// Server configuration
    pub config: Arc<Config>,
    /// Map of server ID to map of instance ID to cancellation token
    pub cancellation_tokens: HashMap<String, HashMap<String, CancellationToken>>,
    /// Process monitor for tracking resource usage
    pub process_monitor: Option<Arc<ProcessMonitor>>,
    /// Database reference for checking server status (used by sync manager)
    pub database: Option<Arc<crate::config::database::Database>>,
    /// Failure tracking for circuit breaking/backoff
    pub failure_states: HashMap<String, types::FailureState>,
    /// Optional shared HTTP client registry for transport reuse
    pub(crate) http_clients: Option<Arc<crate::core::pool::connection::HttpClientRegistry>>,
    /// Optional runtime secret resolver for managed upstream server startup parameters.
    pub(crate) secret_resolver: Option<Arc<dyn SecretResolver>>,
    /// Optional writable secret store for OAuth token refresh during runtime config loading.
    pub(crate) secret_store: Option<Arc<LocalSecretStore>>,
}

impl UpstreamConnectionPool {
    pub(crate) fn runtime_worker(&self) -> Self {
        let mut worker = self.clone();
        worker.clear_preview_runtime_state();
        worker
    }

    fn validation_worker(&self) -> Self {
        Self {
            connections: HashMap::new(),
            production_routes: HashMap::new(),
            client_bound_connections: HashMap::new(),
            exploration_sessions: HashMap::new(),
            validation_sessions: HashMap::new(),
            validation_owner_epochs: HashMap::new(),
            exploration_expirations: HashMap::new(),
            validation_expirations: HashMap::new(),
            validation_reservations: HashMap::new(),
            next_validation_reservation: self.next_validation_reservation.clone(),
            next_validation_attempt: self.next_validation_attempt.clone(),
            next_validation_owner_epoch: self.next_validation_owner_epoch.clone(),
            health_reconnect_epochs: HashMap::new(),
            next_health_reconnect_epoch: self.next_health_reconnect_epoch.clone(),
            startup_attempts: HashMap::new(),
            next_startup_attempt: self.next_startup_attempt.clone(),
            preview_owners: HashMap::new(),
            preview_attempts: HashMap::new(),
            next_preview_attempt: self.next_preview_attempt.clone(),
            server_lifecycle_generations: HashMap::new(),
            config: self.config.clone(),
            cancellation_tokens: HashMap::new(),
            process_monitor: None,
            database: self.database.clone(),
            failure_states: self.failure_states.clone(),
            http_clients: self.http_clients.clone(),
            secret_resolver: self.secret_resolver.clone(),
            secret_store: self.secret_store.clone(),
        }
    }

    /// Create a new connection pool
    ///
    /// # Arguments
    /// * `config` - The server configuration
    /// * `database` - Optional database reference for checking server status
    pub fn new(
        config: Arc<Config>,
        database: Option<Arc<crate::config::database::Database>>,
    ) -> Self {
        // Create process monitor with 5 second update interval
        let process_monitor = Arc::new(ProcessMonitor::new(Duration::from_secs(5)));

        // Start process monitoring
        ProcessMonitor::start_monitoring(process_monitor.clone());

        Self {
            connections: HashMap::new(),
            production_routes: HashMap::new(),
            client_bound_connections: HashMap::new(),
            exploration_sessions: HashMap::new(),
            validation_sessions: HashMap::new(),
            validation_owner_epochs: HashMap::new(),
            exploration_expirations: HashMap::new(),
            validation_expirations: HashMap::new(),
            validation_reservations: HashMap::new(),
            next_validation_reservation: Arc::new(AtomicU64::new(1)),
            next_validation_attempt: Arc::new(AtomicU64::new(1)),
            next_validation_owner_epoch: Arc::new(AtomicU64::new(1)),
            health_reconnect_epochs: HashMap::new(),
            next_health_reconnect_epoch: Arc::new(AtomicU64::new(1)),
            startup_attempts: HashMap::new(),
            next_startup_attempt: Arc::new(AtomicU64::new(1)),
            preview_owners: HashMap::new(),
            preview_attempts: HashMap::new(),
            next_preview_attempt: Arc::new(AtomicU64::new(1)),
            server_lifecycle_generations: HashMap::new(),
            config,
            cancellation_tokens: HashMap::new(),
            process_monitor: Some(process_monitor),
            database,
            failure_states: HashMap::new(),
            http_clients: {
                // Prefer new env name MCPMATE_HTTP_CLIENT_REUSE, fallback to legacy MCMP_MATE_HTTP_CLIENT_REUSE
                let reuse = std::env::var("MCPMATE_HTTP_CLIENT_REUSE")
                    .ok()
                    .or_else(|| std::env::var("MCMP_MATE_HTTP_CLIENT_REUSE").ok());
                let enabled = match reuse.as_deref() {
                    Some("0") | Some("false") | Some("off") => false,
                    _ => true, // default ON unless explicitly disabled
                };
                if enabled {
                    Some(Arc::new(crate::core::pool::connection::HttpClientRegistry::new()))
                } else {
                    None
                }
            },
            secret_resolver: None,
            secret_store: None,
        }
    }

    pub fn with_secret_resolver(
        mut self,
        store: Arc<LocalSecretStore>,
    ) -> Self {
        self.secret_resolver = Some(store.clone());
        self.secret_store = Some(store);
        self
    }

    pub fn set_secret_resolver(
        &mut self,
        store: Arc<LocalSecretStore>,
    ) {
        self.secret_resolver = Some(store.clone());
        self.secret_store = Some(store);
    }

    pub(crate) fn runtime_server_config(
        &self,
        server_id: &str,
    ) -> Result<crate::core::models::MCPServerConfig> {
        let config = self
            .config
            .mcp_servers
            .get(server_id)
            .ok_or_else(|| anyhow::anyhow!("Server '{}' not found in pool configuration", server_id))?;

        crate::core::secrets::resolve_runtime_server_config_with_optional_resolver(
            config,
            self.secret_resolver.as_deref(),
        )
        .map_err(|err| anyhow::anyhow!("Failed to resolve runtime secrets for server '{}': {}", server_id, err))
    }

    // (moved to module scope) HttpClientRegistry definition

    /// Update the configuration using the configuration manager
    ///
    /// This method delegates to PoolConfigManager for the actual configuration logic.
    /// It maintains the public API while separating business logic concerns.
    ///
    /// Returns Ok(()) on success, or Err(CoreError) if configuration update fails.
    pub fn set_config(
        &mut self,
        config: Arc<Config>,
    ) -> Result<(), crate::core::foundation::error::CoreError> {
        // Use the configuration manager to handle the complex logic
        PoolConfigManager::update_configuration(&mut self.connections, &mut self.cancellation_tokens, config.clone())?;

        // Update the stored configuration reference
        self.config = config;
        tracing::info!("Pool configuration updated successfully");
        Ok(())
    }

    /// Set the database reference
    pub fn set_database(
        &mut self,
        database: Option<Arc<crate::config::database::Database>>,
    ) {
        self.database = database;
        tracing::info!("Database reference updated for connection pool");
    }

    /// Idle timeout for standard instances (may become configurable later)
    pub fn standard_instance_idle_timeout() -> Duration {
        let env_override = std::env::var("MCPMATE_INSTANCE_IDLE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok());
        Duration::from_secs(env_override.unwrap_or(STANDARD_INSTANCE_IDLE_SECS))
    }

    /// Disconnect instances that have been idle beyond the configured timeout.
    pub async fn reap_idle_instances(&mut self) {
        let idle_timeout = Self::standard_instance_idle_timeout();
        let now = Instant::now();
        let mut targets: Vec<(String, String)> = Vec::new();

        for (server_id, instances) in &self.connections {
            for (instance_id, conn) in instances {
                if matches!(conn.status, ConnectionStatus::Ready)
                    && now.duration_since(conn.last_activity) >= idle_timeout
                {
                    targets.push((server_id.clone(), instance_id.clone()));
                }
            }
        }

        for (server_id, instance_id) in targets {
            match self.disconnect_non_blocking(&server_id, &instance_id).await {
                Ok(()) => tracing::info!(
                    server_id = %server_id,
                    instance_id = %instance_id,
                    idle_timeout_secs = idle_timeout.as_secs(),
                    "Disconnected idle instance after timeout"
                ),
                Err(e) => tracing::warn!(
                    server_id = %server_id,
                    instance_id = %instance_id,
                    error = %e,
                    "Failed to disconnect idle instance"
                ),
            }
        }
    }

    fn failure_state_mut(
        &mut self,
        server_id: &str,
    ) -> &mut types::FailureState {
        self.failure_states.entry(server_id.to_string()).or_default()
    }

    pub fn register_failure(
        &mut self,
        server_id: &str,
        kind: FailureKind,
        reason: Option<String>,
    ) -> Duration {
        self.register_failure_internal(server_id, kind, reason, false)
    }

    pub fn register_claimed_recovery_failure(
        &mut self,
        server_id: &str,
        kind: FailureKind,
        reason: Option<String>,
    ) -> Duration {
        self.register_failure_internal(server_id, kind, reason, true)
    }

    fn register_failure_internal(
        &mut self,
        server_id: &str,
        kind: FailureKind,
        reason: Option<String>,
        attempt_already_claimed: bool,
    ) -> Duration {
        let state = self.failure_state_mut(server_id);
        let backoff = if attempt_already_claimed {
            state.register_claimed_recovery_failure(Instant::now(), kind, reason.clone())
        } else {
            state.register_failure(Instant::now(), kind, reason.clone())
        };

        tracing::warn!(
            server_id = server_id,
            failure_kind = kind.as_str(),
            backoff_secs = backoff.as_secs_f32(),
            reason = reason.as_deref().unwrap_or("<none>"),
            "Registering failure for server, entering backoff"
        );

        backoff
    }

    pub fn clear_failure_state(
        &mut self,
        server_id: &str,
    ) {
        if let Some(state) = self.failure_states.get_mut(server_id) {
            if state.consecutive_failures > 0 {
                tracing::debug!(
                    server_id = server_id,
                    "Clearing failure state after successful operation"
                );
            }
            state.record_success();
        }
    }

    pub(crate) fn begin_health_reconnect(
        &mut self,
        server_id: &str,
        instance_id: &str,
    ) -> u64 {
        let epoch = self.next_health_reconnect_epoch.fetch_add(1, Ordering::Relaxed);
        self.health_reconnect_epochs
            .insert((server_id.to_string(), instance_id.to_string()), epoch);
        epoch
    }

    pub(crate) fn health_reconnect_epoch(
        &self,
        server_id: &str,
        instance_id: &str,
    ) -> Option<u64> {
        self.health_reconnect_epochs
            .get(&(server_id.to_string(), instance_id.to_string()))
            .copied()
    }

    pub(crate) fn invalidate_health_reconnect(
        &mut self,
        server_id: &str,
        instance_id: &str,
    ) {
        self.health_reconnect_epochs
            .remove(&(server_id.to_string(), instance_id.to_string()));
    }

    pub fn remaining_backoff(
        &self,
        server_id: &str,
    ) -> Option<Duration> {
        let now = Instant::now();
        self.failure_states
            .get(server_id)
            .and_then(|state| state.remaining_backoff(now))
    }

    pub fn automatic_recovery_exhausted(
        &self,
        server_id: &str,
    ) -> bool {
        self.failure_states
            .get(server_id)
            .is_some_and(types::FailureState::automatic_recovery_exhausted)
    }

    pub fn claim_automatic_recovery_attempt(
        &mut self,
        server_id: &str,
    ) -> bool {
        self.failure_state_mut(server_id).claim_automatic_recovery_attempt()
    }

    /// Initialize the connection pool with all servers
    ///
    /// This method delegates to PoolConfigManager for the initialization logic.
    pub fn initialize(&mut self) {
        PoolConfigManager::initialize_connections(&mut self.connections, &self.config);
    }

    // Instance helper methods are now in instance_helpers.rs

    /// Sync all servers based on active profile
    ///
    /// This method delegates to ServerSyncManager for the complex synchronization logic.
    /// It maintains the public API while separating business logic concerns.
    pub async fn sync_servers_from_active_profile(&mut self) -> Result<()> {
        let db = self
            .database
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Database not available for server sync"))?;

        let sync_manager = ServerSyncManager::new(db.clone());
        sync_manager.sync_servers_from_active_profile(self).await
    }

    /// Create or refresh an exploration session with TTL
    pub fn upsert_exploration_session(
        &mut self,
        session_id: &str,
        ttl: Duration,
    ) {
        use std::time::Instant;
        self.exploration_sessions.entry(session_id.to_string()).or_default();
        self.exploration_expirations
            .insert(session_id.to_string(), Instant::now() + ttl);
    }

    /// Create or refresh a validation session with TTL
    pub fn upsert_validation_session(
        &mut self,
        session_id: &str,
        ttl: Duration,
    ) {
        use std::time::Instant;
        self.validation_sessions.entry(session_id.to_string()).or_default();
        self.validation_owner_epochs.entry(session_id.to_string()).or_default();
        self.validation_expirations
            .insert(session_id.to_string(), Instant::now() + ttl);
    }

    /// Refresh an existing validation session without creating a new owner.
    pub fn refresh_validation_session(
        &mut self,
        session_id: &str,
        ttl: Duration,
    ) -> bool {
        use std::time::Instant;
        let now = Instant::now();
        let is_active = self.validation_sessions.contains_key(session_id)
            && self
                .validation_expirations
                .get(session_id)
                .is_some_and(|expires_at| *expires_at > now);
        if !is_active {
            self.validation_sessions.remove(session_id);
            self.validation_owner_epochs.remove(session_id);
            self.validation_expirations.remove(session_id);
            return false;
        }

        self.validation_expirations.insert(session_id.to_string(), now + ttl);
        true
    }

    /// Cleanup expired exploration/validation sessions
    pub fn cleanup_expired_sessions(&mut self) {
        use std::time::Instant;
        let now = Instant::now();
        let expired_exploration: Vec<String> = self
            .exploration_expirations
            .iter()
            .filter_map(|(sid, &exp)| if exp <= now { Some(sid.clone()) } else { None })
            .collect();
        for sid in expired_exploration {
            self.exploration_expirations.remove(&sid);
            self.exploration_sessions.remove(&sid);
        }

        let expired_validation: Vec<String> = self
            .validation_expirations
            .iter()
            .filter_map(|(sid, &exp)| if exp <= now { Some(sid.clone()) } else { None })
            .collect();
        for sid in expired_validation {
            self.validation_expirations.remove(&sid);
            if let Some(state) = self.validation_reservations.remove(&sid) {
                for (_, cancellation) in state.in_flight.into_values() {
                    cancellation.cancel();
                }
            }
            let detached = self
                .validation_sessions
                .remove(&sid)
                .map(HashMap::into_iter)
                .unwrap_or_default()
                .collect::<Vec<_>>();
            self.validation_owner_epochs.remove(&sid);
            if !detached.is_empty() {
                spawn_validation_shutdown(sid, detached, VALIDATION_SHUTDOWN_TIMEOUT);
            }
        }
    }

    /// Get active instance counts for runtime status
    pub fn active_instance_counts(&self) -> (usize, usize, usize) {
        let production = self.connections.iter().filter(|(_, m)| !m.is_empty()).count();
        let exploration = self.exploration_sessions.len();
        let validation = self.validation_sessions.len();
        (production, exploration, validation)
    }

    /// Get connection pool snapshot for read-only operations (optimized for concurrent access)
    ///
    /// This method provides a fast, consistent snapshot of the connection pool state
    /// without requiring exclusive access. It's designed for API queries and monitoring.
    pub fn get_connection_snapshot(&self) -> HashMap<String, Vec<(String, types::UpstreamConnection)>> {
        let mut result = HashMap::new();
        let snapshot_time = Instant::now();

        for (server_id, instances) in &self.connections {
            let instance_clones: Vec<(String, types::UpstreamConnection)> = instances
                .iter()
                .map(|(id, conn)| {
                    // Create a lightweight clone for snapshot
                    let mut conn_clone = conn.clone();
                    // Add snapshot metadata
                    conn_clone.last_snapshot = Some(snapshot_time);
                    (id.clone(), conn_clone)
                })
                .collect();

            if !instance_clones.is_empty() {
                result.insert(server_id.clone(), instance_clones);
            }
        }

        tracing::trace!(
            "Created connection pool snapshot with {} servers and {} total instances",
            result.len(),
            result.values().map(|instances| instances.len()).sum::<usize>()
        );

        result
    }

    /// Get a lightweight snapshot for read-only operations (minimal cloning, no tool vectors)
    /// Returns: server_id -> Vec of (instance_id, status, supports_resources, supports_prompts, service_peer)
    pub fn get_snapshot(&self) -> SnapshotMap {
        let mut result: SnapshotMap = HashMap::new();

        for (server_id, instances) in &self.connections {
            let mut vec = Vec::with_capacity(instances.len());
            for (id, conn) in instances.iter() {
                let supports_resources = conn.supports_resources();
                let supports_prompts = conn.supports_prompts();
                let peer = conn.service.as_ref().map(|svc| svc.peer().clone());
                vec.push((
                    id.clone(),
                    conn.status.clone(),
                    supports_resources,
                    supports_prompts,
                    peer,
                ));
            }
            if !vec.is_empty() {
                result.insert(server_id.clone(), vec);
            }
        }

        for ((server_id, _bound_id), instances) in &self.client_bound_connections {
            let entry = result.entry(server_id.clone()).or_default();
            for (id, conn) in instances.iter() {
                let supports_resources = conn.supports_resources();
                let supports_prompts = conn.supports_prompts();
                let peer = conn.service.as_ref().map(|svc| svc.peer().clone());
                entry.push((
                    id.clone(),
                    conn.status.clone(),
                    supports_resources,
                    supports_prompts,
                    peer,
                ));
            }
        }

        result
    }

    /// Get server status summary for quick API responses
    ///
    /// Returns a lightweight summary without full connection details
    pub fn get_server_status_summary(&self) -> HashMap<String, (usize, usize, String)> {
        let mut summary = HashMap::new();

        for (server_id, instances) in &self.connections {
            let total_instances = instances.len();
            let connected_instances = instances
                .values()
                .filter(|conn| matches!(conn.status, ConnectionStatus::Ready))
                .count();

            let overall_status = if connected_instances == 0 {
                "disconnected".to_string()
            } else if connected_instances == total_instances {
                "connected".to_string()
            } else {
                "partial".to_string()
            };

            summary.insert(
                server_id.clone(),
                (total_instances, connected_instances, overall_status),
            );
        }

        summary
    }

    /// Get or create an exploration instance for a server
    pub fn get_or_create_exploration_instance(
        &mut self,
        server_name: &str,
        session_id: &str,
        ttl: Duration,
    ) -> Result<Option<&types::UpstreamConnection>, anyhow::Error> {
        // Ensure session exists
        self.upsert_exploration_session(session_id, ttl);

        // Check if server connection already exists in this session
        if let Some(session_servers) = self.exploration_sessions.get(session_id) {
            if let Some(connection) = session_servers.get(server_name) {
                return Ok(Some(connection));
            }
        }

        // For now, return None - full implementation would create actual connection
        // This would involve:
        // 1. Get server config from database
        // 2. Create new UpstreamConnection
        // 3. Initialize connection to server
        // 4. Store in exploration_sessions

        tracing::debug!(
            "Exploration instance for server '{}' in session '{}' not implemented yet",
            server_name,
            session_id
        );
        Ok(None)
    }

    /// Get or create a validation instance for a server
    ///
    /// This method implements "create-use-destroy" lifecycle for validation instances:
    /// 1. Check if validation instance already exists in session
    /// 2. If not, create temporary validation instance
    /// 3. Instance will be destroyed after use (handled by caller)
    pub async fn get_or_create_validation_instance(
        &mut self,
        server_id: &str,
        session_id: &str,
        _ttl: Duration, // TTL not used for validation instances per requirements
    ) -> Result<Option<&types::UpstreamConnection>, anyhow::Error> {
        // Check if server connection already exists in this session
        if let Some(session_servers) = self.validation_sessions.get(session_id) {
            if session_servers.contains_key(server_id) {
                return Ok(self
                    .validation_sessions
                    .get(session_id)
                    .and_then(|session| session.get(server_id)));
            }
        }

        // Create temporary validation instance
        let connection = self
            .create_temporary_validation_instance(server_id, session_id, CancellationToken::new())
            .await?;

        // Store in validation_sessions
        let owner_epoch = self.allocate_validation_owner_epoch();
        let session_servers = self.validation_sessions.entry(session_id.to_string()).or_default();
        session_servers.insert(server_id.to_string(), connection);
        self.validation_owner_epochs
            .entry(session_id.to_string())
            .or_default()
            .insert(server_id.to_string(), owner_epoch);

        // Return reference to the stored connection
        Ok(self
            .validation_sessions
            .get(session_id)
            .and_then(|session| session.get(server_id)))
    }

    /// Create a temporary validation instance for a server
    ///
    /// This creates a temporary connection that will be used for capability inspection
    /// and then immediately destroyed. It does not affect the server's enabled status.
    async fn create_temporary_validation_instance(
        &mut self,
        server_id: &str,
        session_id: &str,
        cancellation: CancellationToken,
    ) -> Result<types::UpstreamConnection, anyhow::Error> {
        tracing::info!("Creating temporary validation instance for server ID: {}", server_id);

        // Get database connection
        let db = self
            .database
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Database connection not available"))?;

        // Get server configuration from database
        crate::config::server::namespace_repair::ensure_canonical_namespace_before_exposure(&db.pool, server_id)
            .await?;
        let initial_fingerprint =
            crate::config::server::capabilities::current_config_fingerprint(&db.pool, server_id).await?;
        let server = crate::config::server::get_server_by_id(&db.pool, server_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Server '{}' disappeared after namespace repair", server_id))?;
        let server_name = server.name.as_str();

        let failure_key = format!(
            "validation:{}",
            server.id.clone().unwrap_or_else(|| server_name.to_string())
        );

        // Respect backoff window for faulty upstreams to avoid repeated connection storms
        if let Some(remaining) = self.remaining_backoff(&failure_key) {
            tracing::warn!(
                server = %server_name,
                backoff_secs = remaining.as_secs_f32(),
                "Skipping validation instance creation due to active backoff"
            );
            return Err(ValidationConnectionBackoff {
                server_name: server_name.to_string(),
                remaining_ms: remaining.as_millis(),
            }
            .into());
        }

        // Convert database Server to MCPServerConfig (reusing existing conversion logic)
        let mut server_config = self.convert_server_to_config(&server, &db.pool).await?;
        let final_fingerprint =
            crate::config::server::capabilities::current_config_fingerprint(&db.pool, server_id).await?;
        if initial_fingerprint != final_fingerprint {
            return Err(anyhow::anyhow!(
                "Server '{}' changed while creating its validation connection",
                server_id
            ));
        }
        server_config.source_fingerprint = Some(final_fingerprint);

        // Resolve secret placeholders before connecting.
        let server_config = crate::core::secrets::resolve_runtime_server_config_with_optional_resolver(
            &server_config,
            self.secret_resolver.as_deref(),
        )
        .map_err(|err| anyhow::anyhow!("Failed to resolve secrets for validation of '{}': {}", server_name, err))?;
        let config_fingerprint = server_config.source_fingerprint.clone();

        // Create temporary connection instance with validation prefix (unified helper)
        let instance_id = crate::core::pool::helpers::format_validation_instance_id(server_name, session_id);
        let mut connection = types::UpstreamConnection::new(server_name.to_string());
        connection.id = instance_id;

        // Set validation status to distinguish from production instances
        connection.status = ConnectionStatus::Validating;

        // Connect to server using unified transport interface with a short timeout to avoid startup stalls
        // Determine transport type strictly from server_type (DB no longer stores transport_type)
        let effective_transport = server.server_type.wire_transport();
        let timeout_policy = crate::core::transport::timeout_policy::McpTimeoutPolicy::for_server(
            server.server_type,
            server_config.command.as_deref(),
            None,
        );

        let service = match crate::core::transport::unified::connect_server_initialized_for_validation(
            server_name,
            &server_config,
            server.server_type,
            effective_transport,
            Some(cancellation),
            Some(&db.pool),
            timeout_policy.startup,
        )
        .await
        {
            Ok(service) => service,
            Err(e) => {
                // Register failure and enter backoff to protect startup/import flows
                let reason = format!("{}", e);
                let _ = self.register_failure(&failure_key, FailureKind::Connect, Some(reason));
                return Err(e);
            }
        };

        self.clear_failure_state(&failure_key);

        let capabilities = service.peer_info().map(|info| info.capabilities.clone());
        connection.update_connected(service, Vec::new(), capabilities);
        connection.config_fingerprint = config_fingerprint;

        tracing::info!("Created temporary validation instance for server '{}'", server_name);
        Ok(connection)
    }

    /// Create or reuse a validation owner without holding the shared pool mutex
    /// across database, OAuth, transport initialization, or loser shutdown work.
    pub(crate) async fn reserve_validation_session(
        shared: &Arc<Mutex<Self>>,
        session_id: &str,
        ttl: Duration,
    ) -> ValidationReservationLease {
        let (token, authority, detached) = {
            let mut pool = shared.lock().await;
            let now = Instant::now();
            if let Some(generation) = pool.validation_reservations.get(session_id).and_then(|state| {
                (state.expires_at > now && pool.validation_sessions.contains_key(session_id))
                    .then_some(state.generation)
            }) {
                pool.validation_reservations
                    .get_mut(session_id)
                    .expect("active validation reservation exists")
                    .expires_at = now + ttl;
                pool.validation_expirations.insert(session_id.to_string(), now + ttl);
                let authority = pool
                    .validation_reservations
                    .get(session_id)
                    .expect("active validation reservation exists")
                    .authority
                    .clone();
                authority.holders.fetch_add(1, Ordering::SeqCst);
                (
                    ValidationReservationToken {
                        session_id: Arc::from(session_id),
                        generation,
                    },
                    authority,
                    Vec::new(),
                )
            } else {
                let detached = pool
                    .validation_sessions
                    .remove(session_id)
                    .map(HashMap::into_iter)
                    .unwrap_or_default()
                    .collect::<Vec<_>>();
                pool.validation_owner_epochs.remove(session_id);
                pool.validation_expirations.remove(session_id);
                if let Some(state) = pool.validation_reservations.remove(session_id) {
                    for (_, cancellation) in state.in_flight.into_values() {
                        cancellation.cancel();
                    }
                }
                let generation = pool
                    .next_validation_reservation
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| current.checked_add(1))
                    .expect("validation reservation generation exhausted");
                let token = ValidationReservationToken {
                    session_id: Arc::from(session_id),
                    generation,
                };
                let authority = Arc::new(ValidationReservationAuthority {
                    holders: AtomicUsize::new(1),
                    committed: AtomicBool::new(false),
                });
                pool.validation_sessions.insert(session_id.to_string(), HashMap::new());
                pool.validation_owner_epochs
                    .insert(session_id.to_string(), HashMap::new());
                pool.validation_expirations.insert(session_id.to_string(), now + ttl);
                pool.validation_reservations.insert(
                    session_id.to_string(),
                    ValidationReservationState {
                        generation,
                        expires_at: now + ttl,
                        authority: authority.clone(),
                        in_flight: HashMap::new(),
                    },
                );
                (token, authority, detached)
            }
        };
        if !detached.is_empty() {
            spawn_validation_shutdown(session_id.to_string(), detached, VALIDATION_SHUTDOWN_TIMEOUT);
        }
        ValidationReservationLease {
            token,
            authority,
            pool: Arc::downgrade(shared),
            armed: true,
        }
    }

    pub(crate) fn validation_reservation_matches(
        &self,
        token: &ValidationReservationToken,
    ) -> bool {
        self.validation_reservation_identity_matches(token)
            && self
                .validation_reservations
                .get(token.session_id())
                .is_some_and(|state| state.expires_at > Instant::now())
    }

    fn validation_reservation_identity_matches(
        &self,
        token: &ValidationReservationToken,
    ) -> bool {
        self.validation_reservations
            .get(token.session_id())
            .is_some_and(|state| state.generation == token.generation)
            && self.validation_sessions.contains_key(token.session_id())
    }

    fn allocate_validation_owner_epoch(&self) -> ValidationOwnerEpoch {
        let epoch = self
            .next_validation_owner_epoch
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| current.checked_add(1))
            .expect("validation owner epoch exhausted");
        ValidationOwnerEpoch(epoch)
    }

    pub(crate) fn validation_owner_observation(
        &self,
        token: &ValidationReservationToken,
        server_id: &str,
    ) -> Option<ValidationOwnerObservation> {
        if !self.validation_reservation_identity_matches(token) {
            return None;
        }
        self.validation_sessions.get(token.session_id())?.get(server_id)?;
        let owner_epoch = *self.validation_owner_epochs.get(token.session_id())?.get(server_id)?;
        Some(ValidationOwnerObservation { owner_epoch })
    }

    pub(crate) fn current_validation_token(
        &self,
        session_id: &str,
    ) -> Option<ValidationReservationToken> {
        let state = self.validation_reservations.get(session_id)?;
        (state.expires_at > Instant::now() && self.validation_sessions.contains_key(session_id)).then(|| {
            ValidationReservationToken {
                session_id: Arc::from(session_id),
                generation: state.generation,
            }
        })
    }

    pub(crate) fn refresh_validation_reservation(
        &mut self,
        token: &ValidationReservationToken,
        ttl: Duration,
    ) -> bool {
        if !self.validation_reservation_matches(token) {
            return false;
        }
        let expires_at = Instant::now() + ttl;
        self.validation_reservations
            .get_mut(token.session_id())
            .expect("matching validation reservation exists")
            .expires_at = expires_at;
        self.validation_expirations
            .insert(token.session_id().to_string(), expires_at);
        true
    }

    #[cfg(test)]
    pub(crate) async fn publish_validation_connection(
        shared: &Arc<Mutex<Self>>,
        token: &ValidationReservationToken,
        server_id: &str,
        connection: types::UpstreamConnection,
        ttl: Duration,
    ) -> Result<(), ValidationReservationError> {
        let mut connection = Some(connection);
        let published = {
            let mut pool = shared.lock().await;
            if !pool.validation_reservation_matches(token) {
                false
            } else {
                let already_published = pool
                    .validation_sessions
                    .get(token.session_id())
                    .expect("matching reservation retains its session shell")
                    .contains_key(server_id);
                if already_published {
                    false
                } else {
                    let owner_epoch = pool.allocate_validation_owner_epoch();
                    pool.validation_sessions
                        .get_mut(token.session_id())
                        .expect("matching reservation retains its session shell")
                        .insert(
                            server_id.to_string(),
                            connection.take().expect("validation connection is present"),
                        );
                    pool.validation_owner_epochs
                        .get_mut(token.session_id())
                        .expect("matching reservation retains its owner epoch shell")
                        .insert(server_id.to_string(), owner_epoch);
                    pool.refresh_validation_reservation(token, ttl);
                    true
                }
            }
        };
        if published {
            return Ok(());
        }

        if let Some(connection) = connection {
            spawn_validation_shutdown(
                token.session_id().to_string(),
                vec![(server_id.to_string(), connection)],
                VALIDATION_SHUTDOWN_TIMEOUT,
            );
        }
        if shared.lock().await.validation_reservation_matches(token) {
            Ok(())
        } else {
            Err(ValidationReservationError::Invalidated {
                session_id: token.session_id().to_string(),
                generation: token.generation(),
            })
        }
    }

    fn begin_validation_attempt(
        &mut self,
        token: &ValidationReservationToken,
        server_id: &str,
        cancellation: CancellationToken,
        shared: &Arc<Mutex<Self>>,
    ) -> std::result::Result<ValidationAttemptGuard, ValidationReservationError> {
        if !self.validation_reservation_matches(token) {
            return Err(ValidationReservationError::Invalidated {
                session_id: token.session_id().to_string(),
                generation: token.generation(),
            });
        }
        let attempt_id = self
            .next_validation_attempt
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| current.checked_add(1))
            .expect("validation attempt identifier exhausted");
        self.validation_reservations
            .get_mut(token.session_id())
            .expect("active validation reservation exists")
            .in_flight
            .insert(attempt_id, (server_id.to_string(), cancellation.clone()));
        Ok(ValidationAttemptGuard {
            token: token.clone(),
            attempt_id,
            server_id: server_id.to_string(),
            cancellation,
            pool: Arc::downgrade(shared),
            armed: true,
        })
    }

    pub(crate) fn invalidate_validation_attempts_for_server(
        &mut self,
        server_id: &str,
    ) -> usize {
        let mut invalidated = 0;
        for state in self.validation_reservations.values_mut() {
            let attempt_ids = state
                .in_flight
                .iter()
                .filter_map(|(attempt_id, (attempt_server_id, _))| {
                    (attempt_server_id == server_id).then_some(*attempt_id)
                })
                .collect::<Vec<_>>();
            for attempt_id in attempt_ids {
                if let Some((_, cancellation)) = state.in_flight.remove(&attempt_id) {
                    cancellation.cancel();
                    invalidated += 1;
                }
            }
        }
        invalidated
    }

    fn remove_validation_attempt(
        &mut self,
        token: &ValidationReservationToken,
        attempt_id: u64,
        server_id: &str,
    ) -> bool {
        if !self.validation_reservation_identity_matches(token) {
            return false;
        }
        let state = self
            .validation_reservations
            .get_mut(token.session_id())
            .expect("matching validation reservation exists");
        let owns_attempt = state
            .in_flight
            .get(&attempt_id)
            .is_some_and(|(attempt_server_id, _)| attempt_server_id == server_id);
        if owns_attempt {
            state.in_flight.remove(&attempt_id);
        }
        owns_attempt
    }

    fn claim_validation_attempt(
        &mut self,
        token: &ValidationReservationToken,
        attempt_id: u64,
        server_id: &str,
    ) -> bool {
        let is_active = self.validation_reservation_matches(token);
        self.remove_validation_attempt(token, attempt_id, server_id) && is_active
    }

    fn commit_validation_success(
        &mut self,
        attempt: &ValidationAttemptGuard,
        connection: types::UpstreamConnection,
        failure_updates: Vec<(String, types::FailureState)>,
        ttl: Duration,
    ) -> ValidationSuccessFinalization {
        if !self.claim_validation_attempt(&attempt.token, attempt.attempt_id, &attempt.server_id) {
            return ValidationSuccessFinalization::Lost(connection);
        }

        apply_pool_failure_updates(self, failure_updates);
        let session = self
            .validation_sessions
            .get(attempt.token.session_id())
            .expect("claimed validation attempt retains its session shell");
        if session.contains_key(&attempt.server_id) {
            return ValidationSuccessFinalization::Joined(connection);
        }

        let owner_epoch = self.allocate_validation_owner_epoch();
        self.validation_sessions
            .get_mut(attempt.token.session_id())
            .expect("claimed validation attempt retains its session shell")
            .insert(attempt.server_id.clone(), connection);
        self.validation_owner_epochs
            .get_mut(attempt.token.session_id())
            .expect("claimed validation attempt retains its owner epoch shell")
            .insert(attempt.server_id.clone(), owner_epoch);
        let expires_at = Instant::now() + ttl;
        self.validation_reservations
            .get_mut(attempt.token.session_id())
            .expect("claimed validation attempt retains its reservation")
            .expires_at = expires_at;
        self.validation_expirations
            .insert(attempt.token.session_id().to_string(), expires_at);
        ValidationSuccessFinalization::Published
    }

    async fn finalize_validation_success(
        shared: &Arc<Mutex<Self>>,
        mut attempt: ValidationAttemptGuard,
        pending: &mut PendingValidationConnection,
        failure_updates: Vec<(String, types::FailureState)>,
        ttl: Duration,
    ) -> Result<(), anyhow::Error> {
        let finalization = {
            let mut pool = shared.lock().await;
            let connection = pending
                .take()
                .expect("successful validation attempt retains its pending owner");
            pool.commit_validation_success(&attempt, connection, failure_updates, ttl)
        };
        attempt.disarm();

        match finalization {
            ValidationSuccessFinalization::Published => {
                pending.disarm();
                Ok(())
            }
            ValidationSuccessFinalization::Joined(connection) => {
                Self::shutdown_detached_validation_connection(&attempt.token, &attempt.server_id, connection).await?;
                pending.disarm();
                Ok(())
            }
            ValidationSuccessFinalization::Lost(connection) => {
                Self::shutdown_detached_validation_connection(&attempt.token, &attempt.server_id, connection).await?;
                Err(ValidationReservationError::Invalidated {
                    session_id: attempt.token.session_id().to_string(),
                    generation: attempt.token.generation(),
                }
                .into())
            }
        }
    }

    async fn finalize_validation_failure(
        shared: &Arc<Mutex<Self>>,
        mut attempt: ValidationAttemptGuard,
        failure_updates: Vec<(String, types::FailureState)>,
    ) -> bool {
        let committed = {
            let mut pool = shared.lock().await;
            if pool.claim_validation_attempt(&attempt.token, attempt.attempt_id, &attempt.server_id) {
                apply_pool_failure_updates(&mut pool, failure_updates);
                true
            } else {
                false
            }
        };
        attempt.disarm();
        committed
    }

    fn detach_validation_reservation(
        &mut self,
        token: &ValidationReservationToken,
    ) -> Vec<(String, types::UpstreamConnection)> {
        if !self.validation_reservation_identity_matches(token) {
            return Vec::new();
        }
        if let Some(state) = self.validation_reservations.remove(token.session_id()) {
            for (_, cancellation) in state.in_flight.into_values() {
                cancellation.cancel();
            }
        }
        self.validation_expirations.remove(token.session_id());
        self.validation_owner_epochs.remove(token.session_id());
        self.validation_sessions
            .remove(token.session_id())
            .map(HashMap::into_iter)
            .unwrap_or_default()
            .collect()
    }

    pub(crate) async fn release_validation_reservation(
        shared: &Arc<Mutex<Self>>,
        token: &ValidationReservationToken,
    ) -> Result<(), ValidationShutdownError> {
        let connections = shared.lock().await.detach_validation_reservation(token);
        if connections.is_empty() {
            return Ok(());
        }
        spawn_validation_shutdown(token.session_id().to_string(), connections, VALIDATION_SHUTDOWN_TIMEOUT)
            .await
            .map_err(|source| ValidationShutdownError::Join { source })?
    }

    pub(crate) fn detach_validation_connection_if_matches(
        &mut self,
        token: &ValidationReservationToken,
        server_id: &str,
        expected_owner_epoch: ValidationOwnerEpoch,
    ) -> Option<types::UpstreamConnection> {
        if !self.validation_reservation_identity_matches(token) {
            return None;
        }
        let matches_expected = self
            .validation_owner_epochs
            .get(token.session_id())
            .and_then(|owner_epochs| owner_epochs.get(server_id))
            .is_some_and(|owner_epoch| *owner_epoch == expected_owner_epoch);
        if !matches_expected {
            return None;
        }
        if let Some(state) = self.validation_reservations.get_mut(token.session_id()) {
            let attempt_ids = state
                .in_flight
                .iter()
                .filter_map(|(attempt_id, (attempt_server_id, cancellation))| {
                    if attempt_server_id != server_id {
                        return None;
                    }
                    cancellation.cancel();
                    Some(*attempt_id)
                })
                .collect::<Vec<_>>();
            for attempt_id in attempt_ids {
                state.in_flight.remove(&attempt_id);
            }
        }
        self.validation_owner_epochs
            .get_mut(token.session_id())
            .and_then(|owner_epochs| owner_epochs.remove(server_id));
        self.validation_sessions
            .get_mut(token.session_id())
            .and_then(|servers| servers.remove(server_id))
    }

    pub(crate) async fn shutdown_detached_validation_connection(
        token: &ValidationReservationToken,
        server_id: &str,
        connection: types::UpstreamConnection,
    ) -> Result<(), ValidationShutdownError> {
        spawn_validation_shutdown(
            token.session_id().to_string(),
            vec![(server_id.to_string(), connection)],
            VALIDATION_SHUTDOWN_TIMEOUT,
        )
        .await
        .map_err(|source| ValidationShutdownError::Join { source })?
    }

    pub(crate) async fn ensure_validation_instance(
        shared: &Arc<Mutex<Self>>,
        server_id: &str,
        session_id: &str,
        ttl: Duration,
    ) -> Result<ValidationReservationLease, anyhow::Error> {
        let lease = Self::reserve_validation_session(shared, session_id, ttl).await;
        let token = lease.token().clone();
        let mut worker = {
            let pool = shared.lock().await;
            if pool
                .validation_sessions
                .get(token.session_id())
                .is_some_and(|servers| servers.contains_key(server_id))
            {
                return Ok(lease);
            }
            if pool.database.is_none() {
                anyhow::bail!("Database connection not available");
            }
            pool.validation_worker()
        };

        let cancellation = CancellationToken::new();
        let attempt_guard = {
            let mut pool = shared.lock().await;
            pool.begin_validation_attempt(&token, server_id, cancellation.clone(), shared)?
        };
        let task_server_id = server_id.to_string();
        let task_session_id = session_id.to_string();
        let task_cancellation = cancellation.clone();
        let mut connect_task = tokio::spawn(async move {
            let initial_failure_states = worker.failure_states.clone();
            let connection = worker
                .create_temporary_validation_instance(&task_server_id, &task_session_id, task_cancellation.clone())
                .await;
            let connection = connection.map(|connection| {
                let mut pending = PendingValidationConnection::new(task_cancellation);
                pending.set(connection);
                pending
            });
            let failure_updates = worker
                .failure_states
                .into_iter()
                .filter(|(key, state)| initial_failure_states.get(key) != Some(state))
                .collect::<Vec<_>>();
            (failure_updates, connection)
        });
        let mut abort_guard = AbortTaskOnDrop::new(connect_task.abort_handle(), cancellation.clone());
        let (failure_updates, connection) = (&mut connect_task)
            .await
            .map_err(|error| anyhow::anyhow!("validation connect task failed: {error}"))?;
        abort_guard.disarm();
        let mut pending = match connection {
            Ok(pending) => pending,
            Err(error) => {
                if Self::finalize_validation_failure(shared, attempt_guard, failure_updates).await {
                    return Err(error);
                }
                return Err(ValidationReservationError::Invalidated {
                    session_id: token.session_id().to_string(),
                    generation: token.generation(),
                }
                .into());
            }
        };
        Self::finalize_validation_success(shared, attempt_guard, &mut pending, failure_updates, ttl).await?;
        Ok(lease)
    }

    /// Convert a persisted server definition to runtime configuration.
    async fn convert_server_to_config(
        &self,
        server: &crate::config::models::Server,
        pool: &sqlx::Pool<sqlx::Sqlite>,
    ) -> Result<crate::core::models::MCPServerConfig, anyhow::Error> {
        let server_id = server
            .id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Server '{}' has no persisted ID", server.name))?;
        let transport = crate::config::server::load_validated_server_transport(pool, server_id).await?;
        let manual_headers = transport.runtime_headers();
        let manager = crate::core::oauth::OAuthManager::new_optional_store(pool.clone(), self.secret_store.clone());
        let headers = manager
            .get_effective_server_headers(server_id, (!manual_headers.is_empty()).then_some(manual_headers))
            .await?;
        let mut config = transport.to_mcp_config();
        config.headers = headers;
        Ok(config)
    }

    /// Destroy a validation instance after use
    ///
    /// This implements the "immediate cleanup" part of the create-use-destroy lifecycle
    pub async fn destroy_validation_instance(
        &mut self,
        server_id: &str,
        session_id: &str,
    ) -> Result<(), anyhow::Error> {
        if let Some(session_servers) = self.validation_sessions.get_mut(session_id) {
            if let Some(connection) = session_servers.remove(server_id) {
                if let Some(owner_epochs) = self.validation_owner_epochs.get_mut(session_id) {
                    owner_epochs.remove(server_id);
                }
                shutdown_validation_connection(connection).await?;
                tracing::info!("Destroyed validation instance for server ID '{}'", server_id);
            }
        }

        Ok(())
    }

    /// Destroy a full validation session and all server instances it owns.
    pub async fn destroy_validation_session(
        &mut self,
        session_id: &str,
    ) -> Result<(), anyhow::Error> {
        let mut cleanup_error = None;
        if let Some(session_servers) = self.validation_sessions.remove(session_id) {
            self.validation_owner_epochs.remove(session_id);
            for (server_name, connection) in session_servers {
                if let Err(error) = shutdown_validation_connection(connection).await {
                    cleanup_error.get_or_insert_with(|| {
                        anyhow::anyhow!(
                            "Failed to await validation owner cleanup for server '{}' in session '{}': {}",
                            server_name,
                            session_id,
                            error
                        )
                    });
                }
                tracing::info!(
                    "Destroyed validation instance for server '{}' in session '{}'",
                    server_name,
                    session_id
                );
            }
        }
        self.validation_expirations.remove(session_id);

        match cleanup_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

struct PendingValidationConnection {
    cancellation: CancellationToken,
    connection: Option<types::UpstreamConnection>,
    armed: bool,
}

pub(crate) fn apply_pool_failure_updates(
    pool: &mut UpstreamConnectionPool,
    updates: Vec<(String, types::FailureState)>,
) {
    for (key, state) in updates {
        if let Some(kind) = state.last_kind {
            pool.register_failure(&key, kind, state.last_error);
        } else {
            pool.clear_failure_state(&key);
        }
    }
}

struct AbortTaskOnDrop {
    handle: tokio::task::AbortHandle,
    cancellation: CancellationToken,
    armed: bool,
}

impl AbortTaskOnDrop {
    fn new(
        handle: tokio::task::AbortHandle,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            handle,
            cancellation,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
            self.handle.abort();
        }
    }
}

impl PendingValidationConnection {
    fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            connection: None,
            armed: true,
        }
    }

    fn set(
        &mut self,
        connection: types::UpstreamConnection,
    ) {
        self.connection = Some(connection);
    }

    fn take(&mut self) -> Option<types::UpstreamConnection> {
        self.connection.take()
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingValidationConnection {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.cancellation.cancel();
        if let Some(service) = self
            .connection
            .as_ref()
            .and_then(|connection| connection.service.as_ref())
        {
            service.cancellation_token().cancel();
        }
    }
}

async fn shutdown_validation_connection(mut connection: types::UpstreamConnection) -> Result<(), anyhow::Error> {
    let service = connection.service.take();
    connection.update_disconnected();

    let Some(service) = service else {
        return Ok(());
    };

    match Arc::try_unwrap(service) {
        Ok(service) => {
            service
                .cancel()
                .await
                .map_err(|error| anyhow::anyhow!("Validation RunningService cleanup failed: {error}"))?;
            Ok(())
        }
        Err(service) => {
            service.cancellation_token().cancel();
            Err(anyhow::anyhow!(
                "Validation RunningService is still shared; issued best-effort cancellation"
            ))
        }
    }
}

async fn await_validation_shutdown<F>(
    future: F,
    deadline: Duration,
) -> Result<(), ValidationShutdownError>
where
    F: std::future::Future<Output = Result<(), anyhow::Error>>,
{
    tokio::time::timeout(deadline, future)
        .await
        .map_err(|_| ValidationShutdownError::Timeout {
            timeout_ms: deadline.as_millis(),
        })?
        .map_err(|source| ValidationShutdownError::Operation { source })
}

fn spawn_validation_shutdown(
    session_id: String,
    connections: Vec<(String, types::UpstreamConnection)>,
    deadline: Duration,
) -> tokio::task::JoinHandle<Result<(), ValidationShutdownError>> {
    for (_, connection) in &connections {
        if let Some(service) = connection.service.as_ref() {
            service.cancellation_token().cancel();
        }
    }
    tokio::spawn(async move {
        await_validation_shutdown(
            async move {
                let mut cleanup_error = None;
                for (server_name, connection) in connections {
                    if let Err(error) = shutdown_validation_connection(connection).await {
                        cleanup_error.get_or_insert_with(|| {
                            anyhow::anyhow!(
                                "Failed to await validation owner cleanup for server '{}' in session '{}': {}",
                                server_name,
                                session_id,
                                error
                            )
                        });
                    }
                }
                cleanup_error.map_or(Ok(()), Err)
            },
            deadline,
        )
        .await
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    use rmcp::{ServerHandler, ServiceExt};
    use tokio_util::sync::CancellationToken;

    use super::types::FailureState;
    use super::{
        FailureKind, PendingValidationConnection, UpstreamConnectionPool, ValidationReservationError,
        ValidationShutdownError, await_validation_shutdown, spawn_validation_shutdown,
    };
    use crate::{
        common::{server::ServerType, status::EnabledStatus},
        config::{
            initialization::run_initialization,
            models::{Server, ServerTransportDraft},
            server::{upsert_server, upsert_server_transport_draft_tx},
        },
        core::{models::Config, pool::UpstreamConnection, transport::client::UpstreamClientHandler},
    };

    #[derive(Clone, Default)]
    struct TestServer;

    impl ServerHandler for TestServer {}

    async fn validation_connection() -> (UpstreamConnection, tokio::task::JoinHandle<anyhow::Result<()>>) {
        let (server_transport, client_transport) = tokio::io::duplex(4096);
        let server_handle = tokio::spawn(async move {
            let service = TestServer.serve(server_transport).await?;
            service.waiting().await?;
            Ok(())
        });
        let service = UpstreamClientHandler::new("validation-test".to_string())
            .serve(client_transport)
            .await
            .expect("validation client should initialize");
        let mut connection = UpstreamConnection::new("validation-test".to_string());
        connection.update_connected(service, Vec::new(), Some(rmcp::model::ServerCapabilities::default()));
        (connection, server_handle)
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

    async fn server_definition_pool() -> sqlx::SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("enable foreign keys");
        crate::helpers::prepare_config_database(&pool).await;
        run_initialization(&pool).await.expect("initialize schema");
        pool
    }

    fn legacy_stdio_server(server_id: &str) -> Server {
        Server {
            id: Some(server_id.to_string()),
            name: format!("server-{server_id}"),
            server_type: ServerType::Stdio,
            command: Some("legacy-command".to_string()),
            url: None,
            source: None,
            enabled: EnabledStatus::Enabled,
            unify_direct_exposure_eligible: false,
            pending_import: false,
            created_at: None,
            updated_at: None,
        }
    }

    #[tokio::test]
    async fn validation_config_rejects_legacy_server_without_transport_draft() {
        let pool = server_definition_pool().await;
        let server = legacy_stdio_server("server-missing-draft");
        upsert_server(&pool, &server)
            .await
            .expect("insert legacy server without transport draft");

        let error = empty_pool()
            .convert_server_to_config(&server, &pool)
            .await
            .expect_err("validation config must reject a missing typed transport draft");

        assert!(
            format!("{error:#}").contains("persisted ServerTransportDraft is missing"),
            "unexpected error: {error:#}"
        );
    }

    #[tokio::test]
    async fn validation_config_rejects_invalid_transport_draft_despite_legacy_projection() {
        let pool = server_definition_pool().await;
        let server = legacy_stdio_server("server-invalid-draft");
        upsert_server(&pool, &server)
            .await
            .expect("insert legacy server with a valid legacy projection");
        let invalid_draft = ServerTransportDraft::Stdio {
            command: None,
            args: Vec::new(),
            env: Default::default(),
        };
        let mut transaction = pool.begin().await.expect("begin invalid draft transaction");
        upsert_server_transport_draft_tx(&mut transaction, "server-invalid-draft", &invalid_draft)
            .await
            .expect("persist invalid transport draft");
        transaction.commit().await.expect("commit invalid transport draft");

        let error = empty_pool()
            .convert_server_to_config(&server, &pool)
            .await
            .expect_err("validation config must reject an invalid typed transport draft");

        assert!(
            format!("{error:#}").contains("persisted ServerTransportDraft is invalid"),
            "unexpected error: {error:#}"
        );
    }

    #[tokio::test]
    async fn validation_cleanup_reports_when_running_service_cannot_be_awaited() {
        let (connection, server_handle) = validation_connection().await;
        let retained_service = connection
            .service
            .as_ref()
            .expect("validation service should exist")
            .clone();
        let mut pool = empty_pool();
        pool.validation_sessions
            .entry("validation-session".to_string())
            .or_default()
            .insert("server-1".to_string(), connection);
        pool.validation_expirations.insert(
            "validation-session".to_string(),
            std::time::Instant::now() + Duration::from_secs(60),
        );

        let error = pool
            .destroy_validation_session("validation-session")
            .await
            .expect_err("shared RunningService cannot provide awaited cleanup");

        assert!(error.to_string().contains("still shared"));
        assert!(retained_service.is_closed());
        assert!(!pool.validation_sessions.contains_key("validation-session"));
        assert!(!pool.validation_expirations.contains_key("validation-session"));

        drop(retained_service);
        tokio::time::timeout(Duration::from_secs(1), server_handle)
            .await
            .expect("server should stop after best-effort cancellation")
            .expect("server task should join")
            .expect("server should shut down cleanly");
    }

    #[tokio::test]
    async fn published_reservation_is_cleaned_when_transfer_is_cancelled() {
        let pool = Arc::new(tokio::sync::Mutex::new(empty_pool()));
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "cancel-transfer", Duration::from_secs(60)).await;
        let token = lease.token().clone();
        let (connection, server_handle) = validation_connection().await;
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &token,
            "server-1",
            connection,
            Duration::from_secs(60),
        )
        .await
        .expect("publish validation owner");

        let guard = pool.lock().await;
        let task_pool = pool.clone();
        let transfer = tokio::spawn(async move {
            let _lease = lease;
            let _blocked = task_pool.lock().await;
        });
        tokio::task::yield_now().await;
        transfer.abort();
        assert!(transfer.await.expect_err("transfer should abort").is_cancelled());
        drop(guard);

        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.lock().await.validation_sessions.contains_key("cancel-transfer") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("armed reservation lease must detach on cancellation");
        server_handle
            .await
            .expect("server task should join")
            .expect("server should stop");
    }

    #[tokio::test]
    async fn old_reservation_cannot_publish_or_remove_a_replacement() {
        let pool = Arc::new(tokio::sync::Mutex::new(empty_pool()));
        let old_lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "replace-session", Duration::from_secs(60)).await;
        let old_token = old_lease.token().clone();
        UpstreamConnectionPool::release_validation_reservation(&pool, &old_token)
            .await
            .expect("close old reservation");

        let new_lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "replace-session", Duration::from_secs(60)).await;
        let new_token = new_lease.token().clone();
        assert_ne!(old_token.generation(), new_token.generation());
        let (new_connection, new_server) = validation_connection().await;
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &new_token,
            "server-1",
            new_connection,
            Duration::from_secs(60),
        )
        .await
        .expect("publish replacement owner");

        let (old_connection, old_server) = validation_connection().await;
        let error = UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &old_token,
            "server-1",
            old_connection,
            Duration::from_secs(60),
        )
        .await
        .expect_err("closed reservation must not publish into replacement");
        assert!(matches!(error, ValidationReservationError::Invalidated { .. }));
        drop(old_lease);

        let guard = pool.lock().await;
        assert!(guard.validation_sessions.get("replace-session").is_some_and(|session| {
            session.get("server-1").is_some() && guard.validation_reservation_matches(&new_token)
        }));
        drop(guard);
        old_server
            .await
            .expect("old server task should join")
            .expect("rejected old owner should stop");
        UpstreamConnectionPool::release_validation_reservation(&pool, &new_token)
            .await
            .expect("release replacement");
        drop(new_lease);
        new_server
            .await
            .expect("new server task should join")
            .expect("replacement server should stop");
    }

    #[tokio::test]
    async fn expired_reservation_token_still_has_cleanup_authority() {
        let pool = Arc::new(tokio::sync::Mutex::new(empty_pool()));
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "expired-close", Duration::from_millis(50)).await;
        let token = lease.token().clone();
        let (connection, server_handle) = validation_connection().await;
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &token,
            "server-1",
            connection,
            Duration::from_millis(1),
        )
        .await
        .expect("publish validation owner");
        tokio::time::sleep(Duration::from_millis(5)).await;

        UpstreamConnectionPool::release_validation_reservation(&pool, &token)
            .await
            .expect("expired token must retain cleanup authority");

        assert!(!pool.lock().await.validation_sessions.contains_key("expired-close"));
        drop(lease);
        server_handle
            .await
            .expect("server task should join")
            .expect("expired owner should stop");
    }

    #[tokio::test]
    async fn committed_joiner_survives_creator_lease_drop_in_the_same_generation() {
        let pool = Arc::new(tokio::sync::Mutex::new(empty_pool()));
        let creator =
            UpstreamConnectionPool::reserve_validation_session(&pool, "joined-session", Duration::from_secs(60)).await;
        let creator_token = creator.token().clone();
        let joiner =
            UpstreamConnectionPool::reserve_validation_session(&pool, "joined-session", Duration::from_secs(60)).await;
        assert_eq!(creator_token, *joiner.token());
        let persistent_token = joiner.into_persistent_token();
        let (connection, server_handle) = validation_connection().await;
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &persistent_token,
            "server-1",
            connection,
            Duration::from_secs(60),
        )
        .await
        .expect("joiner publishes owner");

        drop(creator);
        tokio::task::yield_now().await;

        assert!(pool.lock().await.validation_reservation_matches(&persistent_token));
        UpstreamConnectionPool::release_validation_reservation(&pool, &persistent_token)
            .await
            .expect("release persistent joiner");
        server_handle
            .await
            .expect("server task should join")
            .expect("joined owner should stop");
    }

    #[tokio::test]
    async fn late_stale_observer_does_not_cancel_replacement_attempt() {
        let pool = Arc::new(tokio::sync::Mutex::new(empty_pool()));
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "stale-attempt", Duration::from_secs(60)).await;
        let token = lease.token().clone();
        let (stale_connection, stale_server) = validation_connection().await;
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &token,
            "server-1",
            stale_connection,
            Duration::from_secs(60),
        )
        .await
        .expect("publish stale owner");
        let observed_owner_epoch = pool
            .lock()
            .await
            .validation_owner_observation(&token, "server-1")
            .expect("observe stale publication")
            .owner_epoch;

        let detached = pool
            .lock()
            .await
            .detach_validation_connection_if_matches(&token, "server-1", observed_owner_epoch)
            .expect("first observer detaches expected owner");
        let shutdown = spawn_validation_shutdown(
            token.session_id().to_string(),
            vec![("server-1".to_string(), detached)],
            super::VALIDATION_SHUTDOWN_TIMEOUT,
        );
        let cancellation = CancellationToken::new();
        let attempt = pool
            .lock()
            .await
            .begin_validation_attempt(&token, "server-1", cancellation.clone(), &pool)
            .expect("register replacement attempt");

        assert!(
            pool.lock()
                .await
                .detach_validation_connection_if_matches(&token, "server-1", observed_owner_epoch)
                .is_none()
        );
        assert!(!cancellation.is_cancelled());
        assert!(UpstreamConnectionPool::finalize_validation_failure(&pool, attempt, Vec::new()).await);

        shutdown
            .await
            .expect("shutdown task should join")
            .expect("stale owner should stop");
        stale_server
            .await
            .expect("stale server task should join")
            .expect("stale server should stop");
        UpstreamConnectionPool::release_validation_reservation(&pool, &token)
            .await
            .expect("release reservation");
        drop(lease);
    }

    #[tokio::test]
    async fn late_stale_observer_does_not_remove_published_replacement() {
        let pool = Arc::new(tokio::sync::Mutex::new(empty_pool()));
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "stale-replacement", Duration::from_secs(60))
                .await;
        let token = lease.token().clone();
        let (stale_connection, stale_server) = validation_connection().await;
        let expected_instance = stale_connection.id.clone();
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &token,
            "server-1",
            stale_connection,
            Duration::from_secs(60),
        )
        .await
        .expect("publish stale owner");
        let observed_owner_epoch = pool
            .lock()
            .await
            .validation_owner_observation(&token, "server-1")
            .expect("observe stale publication")
            .owner_epoch;
        let detached = pool
            .lock()
            .await
            .detach_validation_connection_if_matches(&token, "server-1", observed_owner_epoch)
            .expect("first observer detaches expected owner");
        let shutdown = spawn_validation_shutdown(
            token.session_id().to_string(),
            vec![("server-1".to_string(), detached)],
            super::VALIDATION_SHUTDOWN_TIMEOUT,
        );
        let (mut replacement, replacement_server) = validation_connection().await;
        replacement.id = expected_instance.clone();
        let replacement_service = replacement.service.as_ref().expect("replacement service").clone();
        let replacement_instance = replacement.id.clone();
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &token,
            "server-1",
            replacement,
            Duration::from_secs(60),
        )
        .await
        .expect("publish replacement");
        let replacement_owner_epoch = pool
            .lock()
            .await
            .validation_owner_observation(&token, "server-1")
            .expect("observe replacement publication")
            .owner_epoch;
        assert_ne!(observed_owner_epoch, replacement_owner_epoch);

        assert!(
            pool.lock()
                .await
                .detach_validation_connection_if_matches(&token, "server-1", observed_owner_epoch)
                .is_none()
        );
        assert_eq!(
            pool.lock().await.validation_sessions[token.session_id()]["server-1"].id,
            replacement_instance
        );
        assert!(!replacement_service.is_closed());
        drop(replacement_service);

        shutdown
            .await
            .expect("shutdown task should join")
            .expect("stale owner should stop");
        stale_server
            .await
            .expect("stale server task should join")
            .expect("stale server should stop");
        UpstreamConnectionPool::release_validation_reservation(&pool, &token)
            .await
            .expect("release replacement");
        replacement_server
            .await
            .expect("replacement server task should join")
            .expect("replacement should stop");
        drop(lease);
    }

    #[tokio::test]
    async fn detached_attempt_finishes_without_worker_authority() {
        let pool = Arc::new(tokio::sync::Mutex::new(empty_pool()));
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "detached-attempt", Duration::from_secs(60))
                .await;
        let token = lease.token().clone();
        let (connection, server_handle) = validation_connection().await;
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &token,
            "server-1",
            connection,
            Duration::from_secs(60),
        )
        .await
        .expect("publish observed owner");
        let observed_owner_epoch = pool
            .lock()
            .await
            .validation_owner_observation(&token, "server-1")
            .expect("observe publication")
            .owner_epoch;
        let cancellation = CancellationToken::new();
        let attempt = pool
            .lock()
            .await
            .begin_validation_attempt(&token, "server-1", cancellation.clone(), &pool)
            .expect("register replacement attempt");

        let detached = pool
            .lock()
            .await
            .detach_validation_connection_if_matches(&token, "server-1", observed_owner_epoch)
            .expect("detach expected owner");
        assert!(cancellation.is_cancelled());
        assert!(!UpstreamConnectionPool::finalize_validation_failure(&pool, attempt, Vec::new()).await);

        spawn_validation_shutdown(
            token.session_id().to_string(),
            vec![("server-1".to_string(), detached)],
            super::VALIDATION_SHUTDOWN_TIMEOUT,
        )
        .await
        .expect("shutdown task should join")
        .expect("detached owner should stop");
        server_handle
            .await
            .expect("server task should join")
            .expect("server should stop");
        UpstreamConnectionPool::release_validation_reservation(&pool, &token)
            .await
            .expect("release reservation");
        drop(lease);
    }

    #[tokio::test]
    async fn late_successful_attempt_cannot_publish_after_matching_epoch_detach() {
        let pool = Arc::new(tokio::sync::Mutex::new(empty_pool()));
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "late-success", Duration::from_secs(60)).await;
        let token = lease.token().clone();
        let cancellation = CancellationToken::new();
        let attempt = pool
            .lock()
            .await
            .begin_validation_attempt(&token, "server-1", cancellation.clone(), &pool)
            .expect("register worker A");
        let (worker_connection, worker_server) = validation_connection().await;
        let mut worker_result = PendingValidationConnection::new(cancellation);
        worker_result.set(worker_connection);

        let (replacement, replacement_server) = validation_connection().await;
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &token,
            "server-1",
            replacement,
            Duration::from_secs(60),
        )
        .await
        .expect("worker B publishes replacement");
        let replacement_epoch = pool
            .lock()
            .await
            .validation_owner_observation(&token, "server-1")
            .expect("observe replacement")
            .owner_epoch;
        let detached = pool
            .lock()
            .await
            .detach_validation_connection_if_matches(&token, "server-1", replacement_epoch)
            .expect("matching epoch detach completes");
        UpstreamConnectionPool::shutdown_detached_validation_connection(&token, "server-1", detached)
            .await
            .expect("replacement shutdown");
        replacement_server
            .await
            .expect("replacement server should join")
            .expect("replacement should stop");

        let error = UpstreamConnectionPool::finalize_validation_success(
            &pool,
            attempt,
            &mut worker_result,
            Vec::new(),
            Duration::from_secs(60),
        )
        .await
        .expect_err("late worker A must lose publication authority");
        assert!(matches!(
            error.downcast_ref::<ValidationReservationError>(),
            Some(ValidationReservationError::Invalidated { .. })
        ));
        assert!(
            !pool
                .lock()
                .await
                .validation_sessions
                .get(token.session_id())
                .is_some_and(|servers| servers.contains_key("server-1"))
        );
        worker_server
            .await
            .expect("late worker server should join")
            .expect("late worker owner should receive bounded shutdown");

        UpstreamConnectionPool::release_validation_reservation(&pool, &token)
            .await
            .expect("release reservation");
        drop(lease);
    }

    #[tokio::test]
    async fn late_failed_attempt_cannot_apply_updates_after_matching_epoch_detach() {
        let pool = Arc::new(tokio::sync::Mutex::new(empty_pool()));
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "late-failure", Duration::from_secs(60)).await;
        let token = lease.token().clone();
        let cancellation = CancellationToken::new();
        let attempt = pool
            .lock()
            .await
            .begin_validation_attempt(&token, "server-1", cancellation, &pool)
            .expect("register worker A");
        let mut late_failure = FailureState::new();
        late_failure.register_failure(
            Instant::now(),
            FailureKind::Connect,
            Some("late worker failure".to_string()),
        );

        let (replacement, replacement_server) = validation_connection().await;
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &token,
            "server-1",
            replacement,
            Duration::from_secs(60),
        )
        .await
        .expect("worker B publishes replacement");
        let replacement_epoch = pool
            .lock()
            .await
            .validation_owner_observation(&token, "server-1")
            .expect("observe replacement")
            .owner_epoch;
        let detached = pool
            .lock()
            .await
            .detach_validation_connection_if_matches(&token, "server-1", replacement_epoch)
            .expect("matching epoch detach completes");
        UpstreamConnectionPool::shutdown_detached_validation_connection(&token, "server-1", detached)
            .await
            .expect("replacement shutdown");
        replacement_server
            .await
            .expect("replacement server should join")
            .expect("replacement should stop");

        assert!(
            !UpstreamConnectionPool::finalize_validation_failure(
                &pool,
                attempt,
                vec![("late-worker".to_string(), late_failure)],
            )
            .await
        );
        assert!(!pool.lock().await.failure_states.contains_key("late-worker"));

        UpstreamConnectionPool::release_validation_reservation(&pool, &token)
            .await
            .expect("release reservation");
        drop(lease);
    }

    #[tokio::test]
    async fn invalidated_server_attempt_cannot_restore_backoff_after_oauth_completion() {
        let pool = Arc::new(tokio::sync::Mutex::new(empty_pool()));
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "oauth-complete", Duration::from_secs(60)).await;
        let token = lease.token().clone();
        let cancellation = CancellationToken::new();
        let attempt = pool
            .lock()
            .await
            .begin_validation_attempt(&token, "server-1", cancellation.clone(), &pool)
            .expect("register pre-OAuth validation attempt");

        assert_eq!(
            pool.lock().await.invalidate_validation_attempts_for_server("server-1"),
            1
        );
        assert!(cancellation.is_cancelled());
        let mut late_failure = FailureState::new();
        late_failure.register_failure(
            Instant::now(),
            FailureKind::Connect,
            Some("old credential failure".to_string()),
        );

        assert!(
            !UpstreamConnectionPool::finalize_validation_failure(
                &pool,
                attempt,
                vec![("validation:server-1".to_string(), late_failure)],
            )
            .await
        );
        assert!(pool.lock().await.remaining_backoff("validation:server-1").is_none());

        UpstreamConnectionPool::release_validation_reservation(&pool, &token)
            .await
            .expect("release reservation");
        drop(lease);
    }

    #[tokio::test]
    async fn validation_owner_epoch_allocator_is_shared_with_clones_and_workers() {
        let pool = empty_pool();
        let cloned_pool = pool.clone();
        let worker = pool.validation_worker();

        let pool_epoch = pool.allocate_validation_owner_epoch();
        let clone_epoch = cloned_pool.allocate_validation_owner_epoch();
        let worker_epoch = worker.allocate_validation_owner_epoch();

        assert_ne!(pool_epoch, clone_epoch);
        assert_ne!(pool_epoch, worker_epoch);
        assert_ne!(clone_epoch, worker_epoch);
        assert!(Arc::ptr_eq(
            &pool.next_validation_owner_epoch,
            &cloned_pool.next_validation_owner_epoch
        ));
        assert!(Arc::ptr_eq(
            &pool.next_validation_owner_epoch,
            &worker.next_validation_owner_epoch
        ));
    }

    #[tokio::test]
    async fn validation_owner_epoch_overflow_is_fail_stop() {
        let pool = empty_pool();
        pool.next_validation_owner_epoch.store(u64::MAX, Ordering::Relaxed);

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.allocate_validation_owner_epoch();
        }));

        assert!(panic.is_err());
    }

    #[tokio::test]
    async fn lazy_expiry_replacement_shuts_down_only_the_old_generation() {
        let pool = Arc::new(tokio::sync::Mutex::new(empty_pool()));
        let old_lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "lazy-expiry", Duration::from_millis(50)).await;
        let old_token = old_lease.token().clone();
        let (old_connection, old_server) = validation_connection().await;
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &old_token,
            "server-1",
            old_connection,
            Duration::from_millis(1),
        )
        .await
        .expect("publish old owner");
        tokio::time::sleep(Duration::from_millis(5)).await;

        let new_lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "lazy-expiry", Duration::from_secs(60)).await;
        let new_token = new_lease.token().clone();
        assert_ne!(old_token.generation(), new_token.generation());
        tokio::time::timeout(Duration::from_secs(1), old_server)
            .await
            .expect("expired owner should receive bounded shutdown")
            .expect("old server task should join")
            .expect("old server should stop");
        drop(old_lease);
        tokio::task::yield_now().await;
        assert!(pool.lock().await.validation_reservation_matches(&new_token));

        UpstreamConnectionPool::release_validation_reservation(&pool, &new_token)
            .await
            .expect("release replacement generation");
        drop(new_lease);
    }

    #[tokio::test]
    async fn lazy_expiry_cleanup_detaches_identity_and_uses_bounded_shutdown() {
        let pool = Arc::new(tokio::sync::Mutex::new(empty_pool()));
        let lease =
            UpstreamConnectionPool::reserve_validation_session(&pool, "lazy-cleanup", Duration::from_millis(50)).await;
        let token = lease.token().clone();
        let (connection, server_handle) = validation_connection().await;
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &token,
            "server-1",
            connection,
            Duration::from_millis(1),
        )
        .await
        .expect("publish expiring owner");
        tokio::time::sleep(Duration::from_millis(5)).await;

        pool.lock().await.cleanup_expired_sessions();

        {
            let pool = pool.lock().await;
            assert!(!pool.validation_reservation_identity_matches(&token));
            assert!(!pool.validation_reservations.contains_key(token.session_id()));
        }
        tokio::time::timeout(Duration::from_secs(1), server_handle)
            .await
            .expect("lazy cleanup should bound owner shutdown")
            .expect("server task should join")
            .expect("expired server should stop");
        drop(lease);
    }

    #[tokio::test]
    async fn failed_creator_does_not_remove_another_server_committed_by_joiner() {
        let pool = Arc::new(tokio::sync::Mutex::new(empty_pool()));
        let creator =
            UpstreamConnectionPool::reserve_validation_session(&pool, "multi-server", Duration::from_secs(60)).await;
        let joiner =
            UpstreamConnectionPool::reserve_validation_session(&pool, "multi-server", Duration::from_secs(60)).await;
        let token = joiner.into_persistent_token();
        let (connection, server_handle) = validation_connection().await;
        UpstreamConnectionPool::publish_validation_connection(
            &pool,
            &token,
            "server-b",
            connection,
            Duration::from_secs(60),
        )
        .await
        .expect("server B joiner publishes");

        drop(creator);
        tokio::task::yield_now().await;
        assert!(
            pool.lock()
                .await
                .validation_sessions
                .get(token.session_id())
                .is_some_and(|servers| servers.contains_key("server-b"))
        );

        UpstreamConnectionPool::release_validation_reservation(&pool, &token)
            .await
            .expect("release multi-server reservation");
        server_handle
            .await
            .expect("server task should join")
            .expect("server B should stop");
    }

    #[tokio::test]
    async fn validation_shutdown_deadline_drops_a_never_finishing_close() {
        struct DropProbe(Arc<AtomicBool>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let probe = DropProbe(dropped.clone());
        let error = await_validation_shutdown(
            async move {
                let _probe = probe;
                std::future::pending::<Result<(), anyhow::Error>>().await
            },
            Duration::from_millis(10),
        )
        .await
        .expect_err("never-finishing close must hit the deadline");

        assert!(matches!(error, ValidationShutdownError::Timeout { .. }));
        assert!(dropped.load(Ordering::SeqCst), "timed-out shutdown must drop ownership");
    }
}
