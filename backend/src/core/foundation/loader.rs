//! Database configuration loader for core MCPMate
//! Contains functions for loading configuration from the database - completely independent from core

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::{
    config::{
        database::Database,
        models::Server,
        server::{ServerEnabledService, headers::has_non_empty_authorization_header},
    },
    core::profile::merge::ProfileMerger,
    core::{
        models::{Config, MCPServerConfig},
        oauth::OAuthManager,
        proxy::args::StartupMode,
        secrets::store::LocalSecretStore,
    },
};

type DegradedLoad<T> = (T, bool);

const STARTUP_DIAGNOSTIC_COMPONENT: &str = "startup_loader";
const STARTUP_DIAGNOSTIC_PHASE: &str = "pool_base_config_load";

fn empty_config() -> Config {
    Config {
        mcp_servers: HashMap::new(),
        pagination: None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigBuildPolicy {
    Strict,
    /// Startup pool base load: per-server field failures warn and degrade instead of aborting core.
    /// Structural failures (for example stdio args) skip the server entry instead of inserting
    /// a broken configuration that would fail unpredictably at connection time.
    DegradePerServer,
}

impl ConfigBuildPolicy {
    fn degrades(self) -> bool {
        self == Self::DegradePerServer
    }
}

#[derive(Clone, Copy)]
enum StartupServerSource {
    GlobalPool,
    ActiveProfile,
}

fn has_manual_authorization(headers: Option<&HashMap<String, String>>) -> bool {
    headers.is_some_and(has_non_empty_authorization_header)
}

fn warn_omit_server_from_startup(
    server_id: &str,
    server_name: &str,
    reason_code: &'static str,
    detail: &'static str,
    error: Option<&anyhow::Error>,
) {
    tracing::warn!(
        component = STARTUP_DIAGNOSTIC_COMPONENT,
        phase = STARTUP_DIAGNOSTIC_PHASE,
        server_id = %server_id,
        server_name = %server_name,
        degraded = true,
        startup_continues = true,
        server_startup_allowed = false,
        degraded_field = "server_config",
        action_taken = "omit_server_from_startup_pool",
        reason_code,
        detail,
        error = error.map(std::string::ToString::to_string),
        "Omitting server from startup pool configuration"
    );
}

async fn load_server_headers(
    oauth_manager: &OAuthManager,
    server_id: &str,
    server_name: &str,
    manual_headers: Option<HashMap<String, String>>,
    degrade: bool,
) -> Result<DegradedLoad<Option<HashMap<String, String>>>> {
    match oauth_manager
        .get_effective_server_headers(server_id, manual_headers.clone())
        .await
    {
        Ok(headers) => Ok((headers, false)),
        Err(error) if degrade => {
            let preserves_manual_authorization = has_manual_authorization(manual_headers.as_ref());
            let action_taken = if preserves_manual_authorization {
                "preserve_manual_authorization_headers"
            } else {
                "omit_server_from_startup_pool"
            };
            tracing::warn!(
                component = STARTUP_DIAGNOSTIC_COMPONENT,
                phase = STARTUP_DIAGNOSTIC_PHASE,
                server_id,
                server_name,
                degraded = true,
                startup_continues = true,
                server_startup_allowed = preserves_manual_authorization,
                degraded_field = "oauth_headers",
                reason_code = "oauth_header_injection_failed",
                action_taken,
                error = %error,
                "Skipping OAuth header injection while loading startup configuration"
            );
            Ok((manual_headers.clone(), !preserves_manual_authorization))
        }
        Err(error) => Err(error).context("Failed to get effective server headers"),
    }
}

async fn build_config_from_servers(
    db: &Database,
    servers: &[Server],
    secret_store: Option<Arc<LocalSecretStore>>,
    build_policy: ConfigBuildPolicy,
) -> Result<Config> {
    let mut config = empty_config();
    let oauth_manager = OAuthManager::new_optional_store(db.pool.clone(), secret_store);
    let degrade = build_policy.degrades();

    for selected_server in servers {
        let Some(server_id) = selected_server.id.as_ref() else {
            continue;
        };
        let initial_fingerprint =
            match crate::config::server::capabilities::current_config_fingerprint(&db.pool, server_id).await {
                Ok(fingerprint) => fingerprint,
                Err(error) if degrade => {
                    warn_omit_server_from_startup(
                        server_id,
                        &selected_server.name,
                        "server_fingerprint_load_failed",
                        "server configuration fingerprint could not be loaded",
                        Some(&error),
                    );
                    continue;
                }
                Err(error) => {
                    return Err(error).context("Failed to get server headers or capture a stable server configuration");
                }
            };
        let Some(server) = crate::config::server::get_server_by_id(&db.pool, server_id).await? else {
            if degrade {
                warn_omit_server_from_startup(
                    server_id,
                    &selected_server.name,
                    "server_disappeared",
                    "server disappeared while its runtime configuration was being loaded",
                    None,
                );
                continue;
            }
            anyhow::bail!("Server '{server_id}' disappeared while loading its runtime configuration");
        };

        let transport = match crate::config::server::load_validated_server_transport(&db.pool, server_id).await {
            Ok(transport) => transport,
            Err(error) if degrade => {
                warn_omit_server_from_startup(
                    server_id,
                    &server.name,
                    "server_transport_invalid",
                    "persisted server transport draft could not be materialized",
                    Some(&error),
                );
                continue;
            }
            Err(error) => {
                return Err(error)
                    .context("Failed to materialize persisted server transport for runtime configuration");
            }
        };
        let mut server_config = transport.to_mcp_config();
        let (headers, headers_degraded) = load_server_headers(
            &oauth_manager,
            server_id,
            &server.name,
            server_config.headers.clone(),
            degrade,
        )
        .await?;

        if degrade && headers_degraded {
            warn_omit_server_from_startup(
                server_id,
                &server.name,
                "server_headers_unavailable",
                "server headers could not be safely materialized",
                None,
            );
            continue;
        }

        let final_fingerprint =
            crate::config::server::capabilities::current_config_fingerprint(&db.pool, server_id).await?;
        if initial_fingerprint != final_fingerprint {
            if degrade {
                warn_omit_server_from_startup(
                    server_id,
                    &server.name,
                    "server_configuration_changed",
                    "server configuration changed while its runtime configuration was being loaded",
                    None,
                );
                continue;
            }
            anyhow::bail!("Server '{server_id}' changed while loading its runtime configuration");
        }

        server_config.source_fingerprint = Some(final_fingerprint);
        server_config.headers = headers;
        config.mcp_servers.insert(server_id.clone(), server_config);
    }

    Ok(config)
}

async fn get_globally_enabled_servers(db: &Database) -> Result<Vec<Server>> {
    let mut servers = crate::config::server::get_all_servers(&db.pool)
        .await
        .context("Failed to load all servers from database")?;
    servers.retain(|server| server.id.is_some() && server.enabled.as_bool());
    servers.sort_by(|left, right| left.name.cmp(&right.name).then_with(|| left.id.cmp(&right.id)));
    Ok(servers)
}

async fn servers_for_startup_mode(
    db: &Database,
    startup_mode: &StartupMode,
    source: StartupServerSource,
) -> Result<Vec<Server>> {
    match startup_mode {
        StartupMode::Minimal | StartupMode::NoProfile => Ok(Vec::new()),
        StartupMode::Default => match source {
            StartupServerSource::GlobalPool => get_globally_enabled_servers(db).await,
            StartupServerSource::ActiveProfile => get_enabled_servers_from_active_profile(&db.pool)
                .await
                .context("Failed to get enabled servers from active profile"),
        },
        StartupMode::SpecificProfile(profile_ids) => ServerEnabledService::new(db.pool.clone())
            .get_enabled_servers_from_profile(profile_ids)
            .await
            .context("Failed to get enabled servers from specific profile"),
    }
}

/// Unified function to load servers from active profile
/// Returns both Server list and Config formats
pub async fn load_servers_from_active_profile(db: &Database) -> anyhow::Result<(Vec<Server>, Config)> {
    // Use ProfileMerger's merge logic
    let merger = ProfileMerger::new(Arc::new(db.clone()));
    let merge_result = merger
        .merge_all_configs()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to merge configurations: {}", e))?;

    let mut servers = Vec::new();
    for server_config in &merge_result.servers {
        if let Ok(Some(server)) = crate::config::server::get_server_by_id(&db.pool, &server_config.server_id).await {
            servers.push(server);
        }
    }
    let config = build_config_from_servers(db, &servers, None, ConfigBuildPolicy::Strict).await?;

    tracing::info!("Loaded {} servers from active profile (unified loader)", servers.len());

    Ok((servers, config))
}

/// Get enabled servers from all active profile using unified service
async fn get_enabled_servers_from_active_profile(pool: &sqlx::Pool<sqlx::Sqlite>) -> anyhow::Result<Vec<Server>> {
    // Use the unified server enabled service
    let service = ServerEnabledService::new(pool.clone());
    let servers = service.get_all_enabled_servers().await?;
    Ok(servers)
}

pub async fn load_pool_base_config(
    db: &Database,
    secret_store: Option<Arc<LocalSecretStore>>,
) -> Result<Config> {
    let servers = get_globally_enabled_servers(db).await?;
    let config = build_config_from_servers(db, &servers, secret_store, ConfigBuildPolicy::Strict).await?;

    tracing::info!(
        "Loaded {} globally enabled servers for pool base configuration",
        config.mcp_servers.len()
    );

    Ok(config)
}

pub async fn load_server_config_strict(
    db: &Database,
    server_id: &str,
    secret_store: Option<Arc<LocalSecretStore>>,
) -> Result<(Server, MCPServerConfig)> {
    let server = crate::config::server::get_server_by_id(&db.pool, server_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Server '{server_id}' not found"))?;
    let mut config = build_config_from_servers(
        db,
        std::slice::from_ref(&server),
        secret_store,
        ConfigBuildPolicy::Strict,
    )
    .await?;
    let server_config = config
        .mcp_servers
        .remove(server_id)
        .ok_or_else(|| anyhow::anyhow!("Server '{server_id}' could not be materialized for validation"))?;
    Ok((server, server_config))
}

/// Load pool base configuration for core startup.
///
/// Uses [`ConfigBuildPolicy::DegradePerServer`]: per-server args, env, and header/OAuth failures
/// warn and degrade instead of aborting core initialization. Servers that would be structurally
/// unsafe to run (for example stdio without loadable args) are omitted from the startup config.
/// Reload paths keep [`ConfigBuildPolicy::Strict`].
pub async fn load_pool_base_config_with_params(
    db: &Database,
    startup_mode: &StartupMode,
    secret_store: Option<Arc<LocalSecretStore>>,
) -> Result<Config> {
    tracing::info!(
        "Loading pool base configuration from database with startup mode: {:?}",
        startup_mode
    );

    match startup_mode {
        StartupMode::Minimal | StartupMode::NoProfile => {
            tracing::info!("Minimal/NoProfile mode: not loading any pool servers");
        }
        StartupMode::Default => {
            tracing::info!("Default mode: loading pool base config from globally enabled servers");
        }
        StartupMode::SpecificProfile(profile_ids) => {
            tracing::info!(
                "Specific profile mode: loading pool servers from profile: {:?}",
                profile_ids
            );
        }
    }

    let servers = servers_for_startup_mode(db, startup_mode, StartupServerSource::GlobalPool).await?;
    build_config_from_servers(db, &servers, secret_store, ConfigBuildPolicy::DegradePerServer).await
}

/// Load the MCP server configuration from the database with startup parameters
pub async fn load_server_config_with_params(
    db: &Database,
    startup_mode: &StartupMode,
) -> Result<Config> {
    tracing::info!(
        "Loading server configuration from database with startup mode: {:?}",
        startup_mode
    );

    match startup_mode {
        StartupMode::Minimal | StartupMode::NoProfile => {
            tracing::info!("Minimal/NoProfile mode: not loading any servers");
        }
        StartupMode::Default => {
            tracing::info!("Default mode: loading servers from all active profile");
        }
        StartupMode::SpecificProfile(profile_ids) => {
            tracing::info!("Specific profile mode: loading servers from profile: {:?}", profile_ids);
        }
    }

    let servers = servers_for_startup_mode(db, startup_mode, StartupServerSource::ActiveProfile).await?;
    let config = build_config_from_servers(db, &servers, None, ConfigBuildPolicy::Strict).await?;

    tracing::info!(
        "Successfully loaded {} enabled servers from database using core loader (mode: {:?})",
        config.mcp_servers.len(),
        startup_mode
    );

    // Publish ConfigReloaded event using core events
    crate::core::events::EventBus::global().publish(crate::core::events::Event::ConfigReloaded);
    tracing::info!("Published ConfigReloaded event using core events");

    Ok(config)
}

/// Load the MCP server configuration from the database (legacy function for backward compatibility)
pub async fn load_server_config(db: &Database) -> Result<Config> {
    load_server_config_with_params(db, &StartupMode::Default).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{server::ServerType, status::EnabledStatus};
    use crate::config::{
        initialization::run_initialization,
        models::{ConfigValue, HttpTransportKind, ServerOAuthConfig, ServerOAuthToken, ServerTransportDraft},
        server::{
            upsert_server, upsert_server_definition, upsert_server_oauth_config, upsert_server_oauth_token,
            upsert_server_transport_draft_tx,
        },
    };
    use crate::core::secrets::store::{LocalSecretStore, SecretCreateInput, SecretKindInput};
    use crate::helpers::oauth_secret_origin;
    use chrono::{Duration, Utc};
    use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
    use tempfile::TempDir;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, method, path},
    };

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

    fn test_server(
        server_id: &str,
        name: &str,
        enabled: bool,
    ) -> Server {
        Server {
            id: Some(server_id.to_string()),
            name: name.to_string(),
            server_type: ServerType::Stdio,
            command: Some("demo-command".to_string()),
            url: None,
            source: None,
            enabled: EnabledStatus::from_bool(enabled),
            unify_direct_exposure_eligible: false,
            pending_import: false,
            created_at: None,
            updated_at: None,
        }
    }

    async fn insert_server_without_transport_draft(
        pool: &SqlitePool,
        server_id: &str,
        name: &str,
        enabled: bool,
    ) {
        upsert_server(pool, &test_server(server_id, name, enabled))
            .await
            .expect("insert legacy server without transport draft");
    }

    async fn insert_server(
        pool: &SqlitePool,
        server_id: &str,
        name: &str,
        enabled: bool,
    ) {
        let draft = ServerTransportDraft::Stdio {
            command: Some("demo-command".to_string()),
            args: Vec::new(),
            env: Default::default(),
        };
        upsert_server_definition(pool, &test_server(server_id, name, enabled), &draft)
            .await
            .expect("insert typed stdio server");
    }

    async fn insert_http_server(
        pool: &SqlitePool,
        server_id: &str,
        name: &str,
        enabled: bool,
    ) {
        let mut server = test_server(server_id, name, enabled);
        server.server_type = ServerType::StreamableHttp;
        server.command = None;
        server.url = Some("https://example.com/mcp".to_string());
        let draft = ServerTransportDraft::Http {
            protocol: HttpTransportKind::StreamableHttp,
            endpoint: Some("https://example.com/mcp".to_string()),
            headers: Default::default(),
        };
        upsert_server_definition(pool, &server, &draft)
            .await
            .expect("insert typed HTTP server");
    }

    async fn replace_http_server_headers(
        pool: &SqlitePool,
        server_id: &str,
        name: &str,
        headers: HashMap<String, String>,
    ) {
        let mut server = test_server(server_id, name, true);
        server.server_type = ServerType::StreamableHttp;
        server.command = None;
        server.url = Some("https://example.com/mcp".to_string());
        let draft = ServerTransportDraft::Http {
            protocol: HttpTransportKind::StreamableHttp,
            endpoint: Some("https://example.com/mcp".to_string()),
            headers: headers
                .into_iter()
                .map(|(key, value)| (key, ConfigValue::Literal { value }))
                .collect(),
        };
        upsert_server_definition(pool, &server, &draft)
            .await
            .expect("replace typed HTTP server headers");
    }

    #[tokio::test]
    async fn runtime_loader_rejects_missing_transport_draft_and_omits_it_at_startup() {
        let (_temp_dir, db) = create_test_database().await;
        insert_server_without_transport_draft(&db.pool, "server-missing-draft", "Missing Draft Server", true).await;

        let strict_error = load_pool_base_config(&db, None)
            .await
            .expect_err("strict load must reject a server without a persisted transport draft");
        assert!(
            format!("{strict_error:#}").contains("persisted ServerTransportDraft is missing"),
            "unexpected error: {strict_error:#}"
        );

        let startup_config = load_pool_base_config_with_params(&db, &StartupMode::Default, None)
            .await
            .expect("startup loader must continue after skipping the invalid server");
        assert!(!startup_config.mcp_servers.contains_key("server-missing-draft"));
    }

    #[tokio::test]
    async fn runtime_loader_rejects_invalid_transport_draft_and_omits_it_at_startup() {
        let (_temp_dir, db) = create_test_database().await;
        insert_server_without_transport_draft(&db.pool, "server-invalid-draft", "Invalid Draft Server", true).await;
        let invalid_draft = ServerTransportDraft::Stdio {
            command: None,
            args: Vec::new(),
            env: Default::default(),
        };
        let mut transaction = db.pool.begin().await.expect("begin invalid draft transaction");
        upsert_server_transport_draft_tx(&mut transaction, "server-invalid-draft", &invalid_draft)
            .await
            .expect("persist invalid transport draft");
        transaction.commit().await.expect("commit invalid transport draft");

        let strict_error = load_pool_base_config(&db, None)
            .await
            .expect_err("strict load must reject an invalid persisted transport draft");
        assert!(
            format!("{strict_error:#}").contains("persisted ServerTransportDraft is invalid"),
            "unexpected error: {strict_error:#}"
        );

        let startup_config = load_pool_base_config_with_params(&db, &StartupMode::Default, None)
            .await
            .expect("startup loader must continue after skipping the invalid server");
        assert!(!startup_config.mcp_servers.contains_key("server-invalid-draft"));
    }

    #[tokio::test]
    async fn typed_stdio_config_remains_available_when_legacy_args_projection_is_missing() {
        let (_temp_dir, db) = create_test_database().await;
        let server_id = "server-stdio-args";
        let draft = ServerTransportDraft::Stdio {
            command: Some("demo-command".to_string()),
            args: vec!["server.js".to_string()],
            env: Default::default(),
        };
        upsert_server_definition(&db.pool, &test_server(server_id, "Stdio Args Server", true), &draft)
            .await
            .expect("insert typed stdio server");
        sqlx::query("DROP TABLE server_args")
            .execute(&db.pool)
            .await
            .expect("drop server_args table");

        let config = load_pool_base_config(&db, None)
            .await
            .expect("typed stdio config should not read the legacy args projection");
        let server = config.mcp_servers.get(server_id).expect("typed stdio server");
        assert_eq!(server.command.as_deref(), Some("demo-command"));
        assert_eq!(server.args.as_deref(), Some(["server.js".to_string()].as_slice()));
    }

    #[tokio::test]
    async fn startup_pool_base_config_omits_stdio_server_without_command() {
        let (_temp_dir, db) = create_test_database().await;
        insert_server(&db.pool, "server-stdio-no-command", "Stdio Missing Command", true).await;
        let mut transaction = db.pool.begin().await.expect("begin invalid stdio draft transaction");
        upsert_server_transport_draft_tx(
            &mut transaction,
            "server-stdio-no-command",
            &ServerTransportDraft::Stdio {
                command: None,
                args: Vec::new(),
                env: Default::default(),
            },
        )
        .await
        .expect("persist invalid stdio transport draft");
        transaction
            .commit()
            .await
            .expect("commit invalid stdio draft transaction");

        let startup_config = load_pool_base_config_with_params(&db, &StartupMode::Default, None)
            .await
            .expect("startup pool load should continue without the broken stdio server");

        assert!(!startup_config.mcp_servers.contains_key("server-stdio-no-command"));
    }

    #[tokio::test]
    async fn startup_pool_base_config_omits_remote_server_without_url() {
        let (_temp_dir, db) = create_test_database().await;
        insert_http_server(&db.pool, "server-remote-no-url", "Remote Missing URL", true).await;
        let mut transaction = db.pool.begin().await.expect("begin invalid HTTP draft transaction");
        upsert_server_transport_draft_tx(
            &mut transaction,
            "server-remote-no-url",
            &ServerTransportDraft::Http {
                protocol: HttpTransportKind::StreamableHttp,
                endpoint: None,
                headers: Default::default(),
            },
        )
        .await
        .expect("persist invalid HTTP transport draft");
        transaction
            .commit()
            .await
            .expect("commit invalid HTTP draft transaction");

        let startup_config = load_pool_base_config_with_params(&db, &StartupMode::Default, None)
            .await
            .expect("startup pool load should continue without the broken remote server");

        assert!(!startup_config.mcp_servers.contains_key("server-remote-no-url"));
    }

    #[tokio::test]
    async fn typed_http_config_remains_available_when_legacy_headers_projection_is_missing() {
        let (_temp_dir, db) = create_test_database().await;
        let server_id = "server-header-read";
        replace_http_server_headers(
            &db.pool,
            server_id,
            "Header Read Server",
            HashMap::from([("X-Test".to_string(), "typed-value".to_string())]),
        )
        .await;
        sqlx::query("DROP TABLE server_headers")
            .execute(&db.pool)
            .await
            .expect("drop server_headers table");

        let config = load_pool_base_config(&db, None)
            .await
            .expect("typed HTTP config should not read the legacy headers projection");
        let headers = config
            .mcp_servers
            .get(server_id)
            .and_then(|server| server.headers.as_ref())
            .expect("typed HTTP headers");
        assert!(
            matches!(headers.get("X-Test"), Some(value) if value == "typed-value"),
            "unexpected headers: {headers:?}"
        );
    }

    #[tokio::test]
    async fn load_pool_base_config_uses_globally_enabled_servers_without_profile_merge() {
        let (_temp_dir, db) = create_test_database().await;

        insert_server(&db.pool, "server-global", "Global Server", true).await;

        let pool_config = load_pool_base_config(&db, None).await.expect("load pool base config");
        let (_, active_profile_config) = load_servers_from_active_profile(&db)
            .await
            .expect("load active-profile config");

        assert!(pool_config.mcp_servers.contains_key("server-global"));
        assert!(!active_profile_config.mcp_servers.contains_key("server-global"));
    }

    const OAUTH_SERVER_ID: &str = "server-oauth";
    const OAUTH_SERVER_NAME: &str = "OAuth Server";

    enum OAuthTokenEndpointResponse {
        RefreshSuccess,
        InvalidGrant,
    }

    struct ExpiredOAuthFixture {
        _temp_dir: TempDir,
        _mock_server: MockServer,
        db: Database,
        secret_store: Arc<LocalSecretStore>,
    }

    async fn mount_oauth_token_endpoint(
        mock_server: &MockServer,
        response: OAuthTokenEndpointResponse,
    ) {
        let template = match response {
            OAuthTokenEndpointResponse::RefreshSuccess => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-new",
                "token_type": "bearer",
                "expires_in": 3600
            })),
            OAuthTokenEndpointResponse::InvalidGrant => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant"
            })),
        };

        let mut mock = Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=refresh-123"));

        if matches!(response, OAuthTokenEndpointResponse::RefreshSuccess) {
            mock = mock.and(body_string_contains("resource=https%3A%2F%2Fexample.com%2Fmcp"));
        }

        mock.respond_with(template).mount(mock_server).await;
    }

    async fn setup_expired_oauth_server(
        token_response: OAuthTokenEndpointResponse,
        manual_headers: Option<HashMap<String, String>>,
    ) -> ExpiredOAuthFixture {
        let (temp_dir, db) = create_test_database().await;
        let secret_store = Arc::new(
            LocalSecretStore::initialize_with_development_root_key(
                db.pool.clone(),
                temp_dir.path().join("secrets").join("local-root.key"),
            )
            .await
            .expect("initialize secret store"),
        );
        insert_http_server(&db.pool, OAUTH_SERVER_ID, OAUTH_SERVER_NAME, true).await;
        let mock_server = MockServer::start().await;
        mount_oauth_token_endpoint(&mock_server, token_response).await;

        upsert_server_oauth_config(
            &db.pool,
            &ServerOAuthConfig {
                id: None,
                server_id: OAUTH_SERVER_ID.to_string(),
                authorization_endpoint: format!("{}/authorize", mock_server.uri()),
                token_endpoint: format!("{}/token", mock_server.uri()),
                client_id: "client-1".to_string(),
                client_secret: None,
                scopes: Some("read write".to_string()),
                redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("save oauth config");

        if let Some(headers) = manual_headers {
            replace_http_server_headers(&db.pool, OAUTH_SERVER_ID, OAUTH_SERVER_NAME, headers).await;
        }

        let access_token = secret_store
            .create_secret(SecretCreateInput {
                alias: format!("oauth/{OAUTH_SERVER_ID}/access-token"),
                kind: SecretKindInput::OAuthAccessToken,
                value: "access-old".to_string(),
                label: Some(format!("OAuth access token for {OAUTH_SERVER_NAME}")),
                origin: Some(oauth_secret_origin(OAUTH_SERVER_ID, OAUTH_SERVER_NAME, "access-token")),
            })
            .await
            .expect("store access token")
            .placeholder;
        let refresh_token = secret_store
            .create_secret(SecretCreateInput {
                alias: format!("oauth/{OAUTH_SERVER_ID}/refresh-token"),
                kind: SecretKindInput::OAuthRefreshToken,
                value: "refresh-123".to_string(),
                label: Some(format!("OAuth refresh token for {OAUTH_SERVER_NAME}")),
                origin: Some(oauth_secret_origin(OAUTH_SERVER_ID, OAUTH_SERVER_NAME, "refresh-token")),
            })
            .await
            .expect("store refresh token")
            .placeholder;

        upsert_server_oauth_token(
            &db.pool,
            &ServerOAuthToken {
                id: None,
                server_id: OAUTH_SERVER_ID.to_string(),
                access_token,
                refresh_token: Some(refresh_token),
                token_type: "bearer".to_string(),
                expires_at: Some((Utc::now() - Duration::minutes(1)).to_rfc3339()),
                scope: Some("read write".to_string()),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("store expired token");

        ExpiredOAuthFixture {
            _temp_dir: temp_dir,
            _mock_server: mock_server,
            db,
            secret_store,
        }
    }

    async fn load_startup_default_pool_config(fixture: &ExpiredOAuthFixture) -> Config {
        load_pool_base_config_with_params(
            &fixture.db,
            &StartupMode::Default,
            Some(Arc::clone(&fixture.secret_store)),
        )
        .await
        .expect("startup config should load")
    }

    #[tokio::test]
    async fn load_pool_base_config_refreshes_expired_oauth_headers() {
        let fixture = setup_expired_oauth_server(OAuthTokenEndpointResponse::RefreshSuccess, None).await;

        let config = load_pool_base_config(&fixture.db, Some(fixture.secret_store))
            .await
            .expect("load pool config");
        let headers = config
            .mcp_servers
            .get(OAUTH_SERVER_ID)
            .and_then(|server| server.headers.as_ref())
            .expect("headers");

        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer access-new")
        );
    }

    #[tokio::test]
    async fn load_pool_base_config_fails_when_oauth_refresh_fails() {
        let fixture = setup_expired_oauth_server(OAuthTokenEndpointResponse::InvalidGrant, None).await;

        let error = load_pool_base_config(&fixture.db, Some(fixture.secret_store))
            .await
            .expect_err("strict pool reload should fail");

        assert!(
            error.to_string().contains("Failed to get effective server headers"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn startup_pool_base_config_continues_when_oauth_refresh_fails() {
        let fixture = setup_expired_oauth_server(OAuthTokenEndpointResponse::InvalidGrant, None).await;

        let config = load_startup_default_pool_config(&fixture).await;

        assert!(!config.mcp_servers.contains_key(OAUTH_SERVER_ID));
    }

    #[tokio::test]
    async fn startup_pool_base_config_preserves_manual_authorization_when_oauth_refresh_fails() {
        let manual_headers = HashMap::from([("Authorization".to_string(), "Bearer manual-token".to_string())]);
        let fixture = setup_expired_oauth_server(OAuthTokenEndpointResponse::InvalidGrant, Some(manual_headers)).await;

        let config = load_startup_default_pool_config(&fixture).await;
        let headers = config
            .mcp_servers
            .get(OAUTH_SERVER_ID)
            .and_then(|server| server.headers.as_ref())
            .expect("manual authorization headers should be preserved");

        let authorization = headers
            .iter()
            .find_map(|(key, value)| key.eq_ignore_ascii_case("authorization").then_some(value.as_str()));

        assert_eq!(authorization, Some("Bearer manual-token"));
    }

    #[tokio::test]
    async fn startup_pool_base_config_continues_when_one_oauth_server_refresh_fails() {
        let fixture = setup_expired_oauth_server(OAuthTokenEndpointResponse::InvalidGrant, None).await;
        insert_server(&fixture.db.pool, "server-stdio", "Stdio Server", true).await;

        let config = load_startup_default_pool_config(&fixture).await;

        let stdio_server = config
            .mcp_servers
            .get("server-stdio")
            .expect("stdio server remains configured");

        assert!(!config.mcp_servers.contains_key(OAUTH_SERVER_ID));
        assert_eq!(stdio_server.command.as_deref(), Some("demo-command"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn startup_oauth_refresh_failure_emits_diagnostic_reason_code() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for CaptureWriter {
            fn write(
                &mut self,
                buf: &[u8],
            ) -> std::io::Result<usize> {
                self.0.lock().expect("capture lock").extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer({
                let buffer = Arc::clone(&buffer);
                move || CaptureWriter(Arc::clone(&buffer))
            })
            .with_ansi(false)
            .without_time()
            .with_level(false)
            .with_target(false)
            .finish();

        let fixture = setup_expired_oauth_server(OAuthTokenEndpointResponse::InvalidGrant, None).await;
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            load_startup_default_pool_config(&fixture).await;
        }

        let output = String::from_utf8(buffer.lock().expect("capture lock").clone()).expect("utf8 logs");
        assert!(
            output.contains("oauth_header_injection_failed"),
            "expected oauth degrade reason_code in logs: {output}"
        );
        assert!(
            output.contains("startup_loader"),
            "expected startup_loader component in logs: {output}"
        );
        assert!(
            output.contains("startup_continues=true") || output.contains("startup_continues: true"),
            "expected startup_continues marker in logs: {output}"
        );
    }
}
