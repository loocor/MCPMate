//! Server synchronization manager
//!
//! Handles the business logic for synchronizing servers based on configuration changes.
//! This includes loading configurations from database, comparing server states,
//! and managing server lifecycle (start/stop/connect).

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing;

use super::UpstreamConnectionPool;
use crate::config::database::Database;
use crate::core::models::Config;
use crate::core::secrets::store::LocalSecretStore;

/// Manager for synchronizing server configurations and states
///
/// This manager handles the business logic of:
/// - Loading server configurations from active profile
/// - Comparing required vs current server states
/// - Managing server lifecycle (start/stop/connect)
/// - Ensuring proper server state transitions
#[derive(Debug)]
pub struct ServerSyncManager {
    /// Database reference for loading configurations
    database: Arc<Database>,
}

impl ServerSyncManager {
    /// Create a new server sync manager
    pub fn new(database: Arc<Database>) -> Self {
        Self { database }
    }

    /// Sync all servers in the connection pool based on active profile
    ///
    /// This is the main entry point for server synchronization. It:
    /// 1. Loads the current active configuration from database
    /// 2. Updates the connection pool configuration
    /// 3. Calculates required server state changes
    /// 4. Executes the necessary server lifecycle operations
    ///
    /// # Arguments
    /// * `pool` - Mutable reference to the connection pool to sync
    ///
    /// # Returns
    /// * `Ok(())` - If synchronization completed successfully
    /// * `Err(...)` - If any step of synchronization failed
    pub async fn sync_servers_from_active_profile(
        &self,
        pool: &mut UpstreamConnectionPool,
    ) -> Result<()> {
        tracing::debug!("Starting server synchronization from active profile");

        let blocked_server_ids = self.repair_enabled_server_namespaces().await?;
        for server_id in &blocked_server_ids {
            pool.block_server_after_capability_collision(server_id).await;
        }
        let mut config = self.load_pool_base_configuration(pool.secret_store.clone()).await?;
        config
            .mcp_servers
            .retain(|server_id, _| !blocked_server_ids.contains(server_id));

        // Step 2: Update connection pool configuration
        pool.set_config(Arc::new(config))?;

        // Step 3: Calculate required server state changes
        let sync_plan = self.calculate_sync_plan(pool)?;

        // Step 4: Execute the synchronization plan
        self.execute_sync_plan(pool, sync_plan).await?;

        tracing::info!("Server synchronization completed successfully");
        Ok(())
    }

    async fn repair_enabled_server_namespaces(&self) -> Result<HashSet<String>> {
        let server_ids =
            sqlx::query_scalar::<_, String>("SELECT id FROM server_config WHERE enabled = 1 ORDER BY created_at, id")
                .fetch_all(&self.database.pool)
                .await
                .context("Failed to load enabled servers for namespace activation gate")?;
        let mut blocked = HashSet::new();
        for server_id in server_ids {
            if let Err(error) = crate::config::server::namespace_repair::ensure_canonical_namespace_before_exposure(
                &self.database.pool,
                &server_id,
            )
            .await
            {
                if !crate::config::server::namespace_repair::is_namespace_exposure_blocked(&error) {
                    return Err(error)
                        .with_context(|| format!("Namespace activation gate failed for server '{server_id}'"));
                }
                tracing::error!(
                    server_id = %server_id,
                    error = %error,
                    "Blocking server activation because its namespace could not be canonicalized"
                );
                blocked.insert(server_id);
            }
        }
        Ok(blocked)
    }

    async fn load_pool_base_configuration(
        &self,
        secret_store: Option<Arc<LocalSecretStore>>,
    ) -> Result<Config> {
        tracing::debug!("Loading server configuration from globally enabled pool base source");

        let config = crate::core::foundation::loader::load_pool_base_config(&self.database, secret_store)
            .await
            .context("Failed to load pool base configuration")?;

        tracing::debug!("Loaded configuration with {} servers", config.mcp_servers.len());
        Ok(config)
    }

    /// Calculate what changes need to be made to synchronize the pool
    fn calculate_sync_plan(
        &self,
        pool: &UpstreamConnectionPool,
    ) -> Result<ServerSyncPlan> {
        let required_servers: HashSet<String> = pool.config.mcp_servers.keys().cloned().collect();
        let current_servers: HashSet<String> = pool.connections.keys().cloned().collect();

        let servers_to_start: HashSet<String> = required_servers.difference(&current_servers).cloned().collect();
        let servers_to_stop: HashSet<String> = current_servers.difference(&required_servers).cloned().collect();
        let servers_to_check: HashSet<String> = required_servers.intersection(&current_servers).cloned().collect();

        // Filter servers that need connection (existing but not connected)
        let mut servers_to_connect = HashSet::new();
        for server_name in &servers_to_check {
            let needs_connection = if let Some(instances) = pool.connections.get(server_name) {
                instances.values().all(|conn| !conn.is_connected())
            } else {
                true // No instances exist, definitely needs connection
            };

            if needs_connection {
                servers_to_connect.insert(server_name.clone());
            }
        }

        let plan = ServerSyncPlan {
            servers_to_start,
            servers_to_stop,
            servers_to_connect,
        };

        tracing::debug!(
            "Calculated sync plan: {} to start, {} to stop, {} to connect",
            plan.servers_to_start.len(),
            plan.servers_to_stop.len(),
            plan.servers_to_connect.len()
        );

        Ok(plan)
    }

    /// Execute the calculated synchronization plan
    async fn execute_sync_plan(
        &self,
        pool: &mut UpstreamConnectionPool,
        plan: ServerSyncPlan,
    ) -> Result<()> {
        // Start new servers (lazy: create placeholder instances without connecting)
        for server_name in plan.servers_to_start {
            tracing::info!(
                "Registering server '{}' for lazy startup (connection deferred)",
                server_name
            );

            let instances = pool.connections.entry(server_name.clone()).or_default();
            if instances.is_empty() {
                let connection = crate::core::pool::UpstreamConnection::new(server_name.clone());
                instances.insert(connection.id.clone(), connection);
            }

            if let Some(instance) = instances.values_mut().next() {
                if matches!(
                    instance.status,
                    crate::core::foundation::types::ConnectionStatus::Shutdown
                ) {
                    instance.status = crate::core::foundation::types::ConnectionStatus::Idle;
                }
            }
        }

        // Stop removed servers
        for server_name in plan.servers_to_stop {
            tracing::info!("Stopping removed server: {}", server_name);
            if let Err(e) = pool.disable_server(&server_name).await {
                tracing::warn!("Failed to stop removed server '{}': {}", server_name, e);
            }
        }

        // Existing servers keep placeholder, wait for first demand to trigger connection
        for server_name in plan.servers_to_connect {
            let instances = pool.connections.entry(server_name.clone()).or_default();
            if instances.is_empty() {
                let connection = crate::core::pool::UpstreamConnection::new(server_name.clone());
                instances.insert(connection.id.clone(), connection);
            }

            if let Some(instance) = instances.values_mut().next() {
                if matches!(
                    instance.status,
                    crate::core::foundation::types::ConnectionStatus::Shutdown
                        | crate::core::foundation::types::ConnectionStatus::Initializing
                ) {
                    instance.status = crate::core::foundation::types::ConnectionStatus::Idle;
                }
            }

            tracing::info!(
                "Server '{}' kept idle; connection will be established on first demand",
                server_name
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::initialization::run_initialization,
        config::{
            models::{Server, ServerTransportDraft},
            server::upsert_server_definition,
        },
        core::{models::Config, pool::types::ProductionRouteKey},
    };
    use sqlx::sqlite::SqlitePoolOptions;
    use std::collections::HashMap;
    use tempfile::TempDir;

    async fn create_test_database() -> (TempDir, Arc<Database>) {
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
        run_initialization(&pool).await.expect("initialize schema");
        let db_path = temp_dir.path().join("test.db");

        (
            temp_dir,
            Arc::new(Database {
                pool,
                path: db_path,
                capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
            }),
        )
    }

    async fn insert_server(
        pool: &sqlx::SqlitePool,
        server_id: &str,
        name: &str,
        enabled: bool,
    ) {
        let mut server = Server::new_stdio(name.to_string(), Some("demo-command".to_string()));
        server.id = Some(server_id.to_string());
        server.enabled = crate::common::status::EnabledStatus::from_bool(enabled);
        upsert_server_definition(
            pool,
            &server,
            &ServerTransportDraft::Stdio {
                command: Some("demo-command".to_string()),
                args: Vec::new(),
                env: Default::default(),
            },
        )
        .await
        .expect("insert typed stdio server fixture");
    }

    #[tokio::test]
    async fn sync_servers_registers_globally_enabled_server_without_profile_membership() {
        let (_temp_dir, database) = create_test_database().await;
        insert_server(&database.pool, "server-global", "Global Server", true).await;

        let mut pool = UpstreamConnectionPool::new(Arc::new(Config::default()), Some(database.clone()));
        let sync_manager = ServerSyncManager::new(database.clone());

        sync_manager
            .sync_servers_from_active_profile(&mut pool)
            .await
            .expect("sync servers");

        assert!(pool.config.mcp_servers.contains_key("server-global"));
        assert!(pool.connections.contains_key("server-global"));
        let namespace: String = sqlx::query_scalar("SELECT name FROM server_config WHERE id = 'server-global'")
            .fetch_one(&database.pool)
            .await
            .expect("load canonicalized namespace");
        assert_eq!(namespace, "global_server");
    }

    #[tokio::test]
    async fn sync_blocks_only_legacy_server_with_namespace_collision() {
        let (_temp_dir, database) = create_test_database().await;
        insert_server(&database.pool, "server-canonical", "global_server", true).await;
        insert_server(&database.pool, "server-legacy", "Global Server", true).await;

        let mut pool = UpstreamConnectionPool::new(Arc::new(Config::default()), Some(database.clone()));
        let sync_manager = ServerSyncManager::new(database);

        sync_manager
            .sync_servers_from_active_profile(&mut pool)
            .await
            .expect("unrelated canonical server should still synchronize");

        assert!(pool.config.mcp_servers.contains_key("server-canonical"));
        assert!(pool.connections.contains_key("server-canonical"));
        assert!(!pool.config.mcp_servers.contains_key("server-legacy"));
        assert!(!pool.connections.contains_key("server-legacy"));
    }

    #[tokio::test]
    async fn sync_propagates_namespace_gate_infrastructure_errors_without_blocking_server() {
        let (_temp_dir, database) = create_test_database().await;
        insert_server(&database.pool, "server-canonical", "global_server", true).await;
        sqlx::query("DROP TABLE server_namespace_issue")
            .execute(&database.pool)
            .await
            .expect("remove namespace issue table to simulate storage failure");

        let mut pool = UpstreamConnectionPool::new(Arc::new(Config::default()), Some(database.clone()));
        pool.connections.insert(
            "server-canonical".to_string(),
            HashMap::from([(
                "existing".to_string(),
                crate::core::pool::UpstreamConnection::new("server-canonical".to_string()),
            )]),
        );

        let error = ServerSyncManager::new(database)
            .sync_servers_from_active_profile(&mut pool)
            .await
            .expect_err("storage failure must abort synchronization instead of blocking the server");

        assert!(
            error
                .to_string()
                .to_ascii_lowercase()
                .contains("namespace activation gate")
        );
        assert!(pool.connections.contains_key("server-canonical"));
    }

    #[tokio::test]
    async fn sync_removes_all_exposure_for_capability_collision_challenger() {
        let (_temp_dir, database) = create_test_database().await;
        insert_server(&database.pool, "server-owner", "a", true).await;
        insert_server(&database.pool, "server-challenger", "a_b", true).await;
        crate::config::server::namespace_repair::record_capability_collision(
            &database.pool,
            &crate::core::capability::naming::ExternalIdentifierCollision {
                kind: crate::core::capability::naming::NamingKind::Tool,
                external_identifier: "a_b_c".to_string(),
                server_id: "server-challenger".to_string(),
                upstream_value: "c".to_string(),
                conflicting_server_id: "server-owner".to_string(),
                conflicting_upstream_value: "b_c".to_string(),
            },
        )
        .await
        .expect("record collision");

        let mut pool = UpstreamConnectionPool::new(Arc::new(Config::default()), Some(database.clone()));
        pool.connections.insert(
            "server-challenger".to_string(),
            HashMap::from([(
                "challenger-default".to_string(),
                crate::core::pool::UpstreamConnection::new("server-challenger".to_string()),
            )]),
        );
        pool.client_bound_connections.insert(
            ("server-challenger".to_string(), "client-a".to_string()),
            HashMap::from([(
                "challenger-client".to_string(),
                crate::core::pool::UpstreamConnection::new("server-challenger".to_string()),
            )]),
        );
        let route = ProductionRouteKey::per_client("server-challenger", "client-a");
        pool.production_routes
            .insert(route.clone(), "challenger-client".to_string());

        ServerSyncManager::new(database)
            .sync_servers_from_active_profile(&mut pool)
            .await
            .expect("sync with collision blocker");

        assert!(!pool.connections.contains_key("server-challenger"));
        assert!(!pool.has_affinity_bound_connection("server-challenger", "client-a"));
        assert!(!pool.production_routes.contains_key(&route));
    }
}

/// Plan for synchronizing servers
///
/// This struct represents the calculated changes needed to bring the connection pool
/// into sync with the required configuration.
#[derive(Debug, Clone)]
struct ServerSyncPlan {
    /// Servers that need to be started (new servers)
    servers_to_start: HashSet<String>,
    /// Servers that need to be stopped (removed servers)
    servers_to_stop: HashSet<String>,
    /// Servers exist but are not connected (keep placeholder, trigger connection on demand)
    servers_to_connect: HashSet<String>,
}
