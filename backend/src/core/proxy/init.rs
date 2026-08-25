//! Initialization logic for core proxy server
//!
//! This module handles the setup and initialization of the proxy server using core modules.

use super::{Args, ProxyServer, args::StartupMode};
use crate::{
    api::handlers::system,
    audit::{AuditRetentionPolicySetting, AuditService, AuditStore, run_retention_worker},
    config::audit_database::AuditDatabase,
    config::database::Database,
    core::{
        capability::naming,
        foundation::loader,
        secrets::store::{SecretStoreBootstrap, SecretStoreReadiness},
    },
};
use anyhow::{Context, Result, bail};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{self, EnvFilter};

const STARTUP_DIAGNOSTIC_COMPONENT: &str = "startup_init";

fn parse_file_log_enabled(raw: Option<&str>) -> Result<bool> {
    use crate::common::constants::{defaults, env_vars};

    let Some(raw) = raw else {
        return Ok(defaults::LOG_TO_FILE_DEFAULT);
    };

    let normalized = raw.trim().to_lowercase();
    match normalized.as_str() {
        "1" | "true" | "on" | "yes" => Ok(true),
        "0" | "false" | "off" | "no" => Ok(false),
        _ => bail!(
            "Invalid {} value '{}'; expected one of true, false, 1, 0, on, off, yes, no",
            env_vars::MCPMATE_LOG_TO_FILE,
            raw
        ),
    }
}

fn resolve_log_file_path() -> Option<std::path::PathBuf> {
    use crate::common::paths::global_paths;

    let logs_dir = global_paths().logs_dir();
    match std::fs::create_dir_all(&logs_dir) {
        Ok(()) => Some(logs_dir.join("mcpmate.log")),
        Err(error) => {
            eprintln!(
                "File logging disabled: failed to create log directory {}: {}",
                logs_dir.display(),
                error
            );
            None
        }
    }
}

fn open_log_file(path: &std::path::Path) -> Option<Arc<std::sync::Mutex<std::fs::File>>> {
    match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => Some(Arc::new(std::sync::Mutex::new(file))),
        Err(error) => {
            eprintln!(
                "File logging disabled: failed to open log file {}: {}",
                path.display(),
                error
            );
            None
        }
    }
}

/// Setup logging based on command line arguments
/// This function is safe to call multiple times - it will only initialize once
pub fn setup_logging(args: &Args) -> Result<()> {
    // TODO(temporary): This file logging toggle and multiplexer are temporary.
    // Once the audit logging subsystem is implemented, remove MCPMATE_LOG_TO_FILE,
    // the MultiWriter, and all file-path handling here.
    use crate::common::constants::env_vars;
    let (env_filter, log_config_msg) = if let Ok(rust_log) = std::env::var("RUST_LOG") {
        // If RUST_LOG is set, respect it completely - no overrides
        let msg = format!("Using RUST_LOG environment variable: {} (full control)", rust_log);
        (EnvFilter::from_default_env(), msg)
    } else {
        // If RUST_LOG is not set, use application defaults with noise reduction
        let default_level = args.log_level.parse().unwrap_or(tracing::Level::INFO.into());
        let msg = format!(
            "No RUST_LOG set, using application default: {} (with noise reduction)",
            args.log_level
        );

        // Create filter with noise reduction for common noisy modules
        let filter = EnvFilter::from_default_env()
            .add_directive(default_level)
            // Keep the really noisy third-party modules quiet
            .add_directive("sqlx=warn".parse().unwrap()) // SQL queries are too verbose
            .add_directive("rmcp=warn".parse().unwrap()) // MCP protocol noise
            .add_directive("hyper=warn".parse().unwrap()) // HTTP client noise
            .add_directive("reqwest=warn".parse().unwrap()) // HTTP requests
            .add_directive("tokio=warn".parse().unwrap()); // Async runtime noise

        (filter, msg)
    };

    // Setup with stdout + optional file logging
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    // Determine whether to enable file logging (env overrides default)
    let raw_file_log = std::env::var(env_vars::MCPMATE_LOG_TO_FILE).ok();
    let enable_file_log = parse_file_log_enabled(raw_file_log.as_deref())?;

    // Determine log file path (only if enabled)
    let log_file_path = if enable_file_log { resolve_log_file_path() } else { None };

    // Create multi-writer that writes to both stdout and file
    #[derive(Clone)]
    struct MultiWriter {
        file: Option<Arc<Mutex<std::fs::File>>>,
    }

    impl Write for MultiWriter {
        fn write(
            &mut self,
            buf: &[u8],
        ) -> std::io::Result<usize> {
            // Always write to stdout
            std::io::stdout().write_all(buf).ok();

            // Also write to file if available
            if let Some(ref file) = self.file {
                if let Ok(mut f) = file.lock() {
                    f.write_all(buf).ok();
                }
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            std::io::stdout().flush().ok();
            if let Some(ref file) = self.file {
                if let Ok(mut f) = file.lock() {
                    f.flush().ok();
                }
            }
            Ok(())
        }
    }

    let file_handle = log_file_path.as_ref().and_then(|path| open_log_file(path));

    let multi_writer = MultiWriter { file: file_handle };

    let result = tracing_subscriber::fmt()
        .with_writer(move || multi_writer.clone())
        .with_env_filter(env_filter)
        .try_init();

    match result {
        Ok(()) => {
            tracing::info!("Logging system initialized successfully");
            tracing::info!("{}", log_config_msg);
            if let Some(ref path) = log_file_path {
                tracing::info!("Log file: {}", path.display());
            } else {
                tracing::info!(
                    "File logging disabled (set {}=true to enable)",
                    env_vars::MCPMATE_LOG_TO_FILE
                );
            }
        }
        Err(_) => {
            // Global subscriber already set, this is fine for FFI mode
            tracing::debug!("Logging system already initialized, skipping");
        }
    }

    Ok(())
}

/// Setup database connection and perform necessary migrations
pub async fn setup_database() -> Result<Database> {
    // Initialize server start time
    system::initialize_server_start_time();

    // Debug database path before initialization
    use crate::common::paths::global_paths;
    let db_path = global_paths().database_path();
    tracing::info!("FFI Database setup - Expected path: {}", db_path.display());
    tracing::info!("FFI Database setup - File exists: {}", db_path.exists());

    // Initialize database
    let db = match Database::new().await {
        Ok(db) => {
            tracing::info!("FFI Database initialized successfully");
            db
        }
        Err(e) => {
            tracing::error!("FFI Failed to initialize database: {}", e);
            return Err(anyhow::anyhow!("Failed to initialize database: {}", e));
        }
    };

    // Initialize naming store once the database is ready
    naming::initialize(db.pool.clone());

    // Runtime migration removed - simplified runtime management
    tracing::debug!("Runtime management simplified - no migration needed");

    Ok(db)
}

pub async fn setup_audit_database() -> Result<Option<AuditDatabase>> {
    match AuditDatabase::new().await {
        Ok(database) => {
            tracing::info!("Audit database initialized successfully");
            Ok(Some(database))
        }
        Err(error) => {
            tracing::warn!(
                component = STARTUP_DIAGNOSTIC_COMPONENT,
                phase = "audit_database_setup",
                subsystem = "audit",
                degraded = true,
                startup_continues = true,
                action_taken = "disable_audit_subsystem",
                reason_code = "audit_database_init_failed",
                error = %error,
                "Audit database initialization failed; continuing without audit subsystem"
            );
            Ok(None)
        }
    }
}

async fn init_audit_subsystem(
    audit_db: AuditDatabase,
    proxy: &mut ProxyServer,
) -> Result<Arc<AuditStore>> {
    let audit_db = Arc::new(audit_db);
    let audit_store = Arc::new(AuditStore::from_database(audit_db.as_ref()));
    audit_store
        .initialize()
        .await
        .context("Failed to initialize audit store")?;

    let audit_service = Arc::new(
        AuditService::new(audit_store.clone())
            .await
            .context("Failed to initialize audit service")?,
    );

    proxy.set_audit_service(audit_db, audit_service);
    tracing::info!("Audit service initialized and attached to proxy server");
    Ok(audit_store)
}

fn spawn_audit_retention_worker(audit_store: Arc<AuditStore>) {
    let cancellation_token = CancellationToken::new();
    let retention_token = cancellation_token.clone();
    tokio::spawn(async move {
        let policy = match audit_store.get_policy().await {
            Ok(policy) => policy,
            Err(error) => {
                tracing::warn!(
                    component = STARTUP_DIAGNOSTIC_COMPONENT,
                    phase = "audit_retention_setup",
                    subsystem = "audit",
                    degraded = true,
                    startup_continues = true,
                    action_taken = "use_default_audit_retention_policy",
                    reason_code = "audit_retention_policy_load_failed",
                    error = %error,
                    "Failed to load audit retention policy, using default"
                );
                AuditRetentionPolicySetting::default()
            }
        };
        run_retention_worker(audit_store, policy, retention_token).await;
    });
}

fn warn_secret_store_unavailable(bootstrap: &SecretStoreBootstrap) {
    let SecretStoreReadiness::Unavailable {
        reason_code,
        message,
        provider,
    } = &bootstrap.readiness
    else {
        return;
    };

    tracing::warn!(
        component = STARTUP_DIAGNOSTIC_COMPONENT,
        phase = "secret_store_bootstrap",
        subsystem = "secure_store",
        degraded = true,
        startup_continues = true,
        action_taken = "continue_without_secret_store",
        reason_code = %reason_code,
        provider_id = provider.as_ref().map(|provider| provider.provider_id.as_str()).unwrap_or("none"),
        provider_kind = provider.as_ref().map(|provider| provider.provider_kind.as_str()).unwrap_or("none"),
        provider_mode = provider.as_ref().map(|provider| provider.provider_mode.as_str()).unwrap_or("none"),
        security_level = provider.as_ref().map(|provider| provider.security_level.as_str()).unwrap_or("unknown"),
        detail = %message,
        "Secure Store unavailable during startup; continuing without secret resolver"
    );
}

/// Setup proxy server with startup parameters
pub async fn setup_proxy_server_with_params(
    db: Database,
    audit_db: Option<AuditDatabase>,
    startup_mode: &StartupMode,
) -> Result<(Arc<ProxyServer>, Arc<ProxyServer>)> {
    let data_dir = db.path.parent().unwrap_or(std::path::Path::new("."));
    let secret_store_bootstrap = crate::core::secrets::store::bootstrap_secret_store(db.pool.clone(), data_dir).await;
    warn_secret_store_unavailable(&secret_store_bootstrap);
    let startup_secret_store = secret_store_bootstrap.store.map(Arc::new);

    // Load configuration from database using core loader with startup parameters
    let config = loader::load_pool_base_config_with_params(&db, startup_mode, startup_secret_store.clone()).await?;

    tracing::info!(
        "Loaded configuration from database with startup mode: {:?}",
        startup_mode
    );
    tracing::info!(
        "Found {} enabled MCP servers in database configuration",
        config.mcp_servers.len()
    );

    // Create proxy server using core implementation
    let mut proxy = ProxyServer::try_new(Arc::new(config))?;
    if let Some(secret_store) = startup_secret_store {
        proxy.connection_pool.lock().await.set_secret_resolver(secret_store);
    }

    // Use the existing database connection
    proxy.set_database(db).await?;
    tracing::info!("Using database connection for tool-level configuration.");

    let audit_store = if let Some(audit_db) = audit_db {
        Some(init_audit_subsystem(audit_db, &mut proxy).await?)
    } else {
        None
    };
    proxy.start_surface_background_workers()?;

    let proxy_arc = Arc::new(proxy.clone());
    ProxyServer::set_global(Arc::new(tokio::sync::Mutex::new(proxy)));

    if let Some(audit_store) = audit_store {
        spawn_audit_retention_worker(audit_store);
    }

    tracing::info!("Proxy server created, event system will be initialized with handlers");

    Ok((Arc::clone(&proxy_arc), proxy_arc))
}

/// Setup proxy server with database and configuration using core modules (legacy function for backward compatibility)
pub async fn setup_proxy_server(db: Database) -> Result<(Arc<ProxyServer>, Arc<ProxyServer>)> {
    setup_proxy_server_with_params(db, None, &StartupMode::Default).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::discovery::ADMIN_DISCOVERY_BASE_URL_ENV;
    use crate::common::status::EnabledStatus;
    use crate::config::initialization::run_initialization;
    use crate::config::models::{Server, ServerTransportDraft};
    use crate::config::server::upsert_server_definition;
    use sqlx::sqlite::SqlitePoolOptions;
    use tempfile::TempDir;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    async fn mount_unavailable_admin_discovery(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/discovery/clients"))
            .respond_with(ResponseTemplate::new(503))
            .mount(server)
            .await;
    }

    fn restore_env_var(
        key: &str,
        previous: Option<String>,
    ) {
        match previous {
            Some(value) => unsafe {
                std::env::set_var(key, value);
            },
            None => unsafe {
                std::env::remove_var(key);
            },
        }
    }

    async fn create_test_database() -> (TempDir, Database) {
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
        sqlx::query("INSERT INTO secure_store_provider_config (id, provider_mode) VALUES (1, 'local_file')")
            .execute(&pool)
            .await
            .expect("select non-interactive test secret provider");
        let db_path = temp_dir.path().join("test.db");

        (
            temp_dir,
            Database {
                pool,
                path: db_path,
                capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
            },
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
        server.enabled = EnabledStatus::from_bool(enabled);

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

    #[test]
    fn parse_file_log_enabled_uses_default_when_unset() {
        let parsed = parse_file_log_enabled(None).expect("parse default file logging setting");

        assert_eq!(parsed, crate::common::constants::defaults::LOG_TO_FILE_DEFAULT);
    }

    #[test]
    fn parse_file_log_enabled_rejects_unknown_values() {
        let error = parse_file_log_enabled(Some("maybe")).expect_err("unknown values must fail");

        assert!(
            error
                .to_string()
                .contains(crate::common::constants::env_vars::MCPMATE_LOG_TO_FILE)
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn setup_proxy_server_default_mode_seeds_pool_from_globally_enabled_servers() {
        let admin_discovery_server = MockServer::start().await;
        mount_unavailable_admin_discovery(&admin_discovery_server).await;

        let (temp_dir, db) = create_test_database().await;
        insert_server(&db.pool, "server-global", "Global Server", true).await;

        let template_root = temp_dir.path().join("client-template-root");
        std::fs::create_dir_all(&template_root).expect("create isolated template root");
        let previous_template_root = std::env::var("MCPMATE_TEMPLATE_ROOT").ok();
        let previous_admin_discovery = std::env::var(ADMIN_DISCOVERY_BASE_URL_ENV).ok();
        unsafe {
            std::env::set_var("MCPMATE_TEMPLATE_ROOT", &template_root);
            std::env::set_var(ADMIN_DISCOVERY_BASE_URL_ENV, admin_discovery_server.uri());
        }

        let setup_result = setup_proxy_server_with_params(db, None, &StartupMode::Default)
            .await
            .expect("setup proxy server");

        restore_env_var("MCPMATE_TEMPLATE_ROOT", previous_template_root);
        restore_env_var(ADMIN_DISCOVERY_BASE_URL_ENV, previous_admin_discovery);

        let (proxy, _) = setup_result;

        let pool = proxy.connection_pool.lock().await;

        assert!(pool.config.mcp_servers.contains_key("server-global"));
        assert!(pool.connections.contains_key("server-global"));
    }
}
