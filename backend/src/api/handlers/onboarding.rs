use std::{collections::HashMap, sync::Arc};

use axum::{Json, extract::State};
use schemars::JsonSchema;
use serde::Serialize;

use super::ApiError;
use super::client::parse_rule_from_api_data;
use crate::api::models::onboarding::{
    OnboardingCompleteReq, OnboardingServerCandidate, OnboardingServerScanData, OnboardingServerScanError,
    OnboardingServerScanReq, OnboardingServerScanResp, OnboardingStatusData, OnboardingStatusResp, RuntimeCheckData,
    RuntimeCheckResp, RuntimeEntry,
};
use crate::api::routes::AppState;
use crate::common::constants::database::tables;
use crate::common::server::ServerType;
use crate::config::server::import::build_import_plan_from_entries;
use crate::macros::resp::api_resp;
use crate::runtime::{ResolveSource, RuntimeDetection, RuntimeDetector, RuntimeProbe};

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(description = "Onboarding action result")]
pub struct OnboardingActionData {
    #[schemars(description = "Whether the action succeeded")]
    pub ok: bool,
}

api_resp!(OnboardingActionResp, OnboardingActionData, "Onboarding action response");

async fn set_onboarding_completed(
    state: Arc<AppState>,
    completed: bool,
) -> Result<Json<OnboardingActionResp>, ApiError> {
    let db = state
        .database
        .as_ref()
        .ok_or_else(|| ApiError::InternalError("Database not available".into()))?;

    crate::system::settings::set_onboarding_completed(&db.pool, completed)
        .await
        .map_err(|err| ApiError::InternalError(err.to_string()))?;

    Ok(Json(OnboardingActionResp::success(OnboardingActionData { ok: true })))
}

fn candidate_fingerprint(config: &crate::api::models::server::ServersImportConfig) -> String {
    let Ok(st) = ServerType::from_client_format(config.kind.trim()) else {
        return String::new();
    };
    crate::config::server::fingerprint::server_dedup_fingerprint(
        st,
        config.command.as_deref(),
        config.url.as_deref(),
        config.args.as_deref().unwrap_or_default(),
    )
    .value
}

fn push_source(
    candidate: &mut OnboardingServerCandidate,
    client_id: &str,
    client_name: &str,
) {
    if !candidate.source_client_ids.iter().any(|value| value == client_id) {
        candidate.source_client_ids.push(client_id.to_string());
        candidate.source_clients.push(client_name.to_string());
    }
}

/// GET /api/onboarding/status
pub async fn get_status(State(state): State<Arc<AppState>>) -> Result<Json<OnboardingStatusResp>, ApiError> {
    let db = state
        .database
        .as_ref()
        .ok_or_else(|| ApiError::InternalError("Database not available".into()))?;

    let settings = crate::system::settings::get_settings(&db.pool)
        .await
        .map_err(|err| ApiError::InternalError(err.to_string()))?;

    let servers_count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {}", tables::SERVER_CONFIG))
        .fetch_one(&db.pool)
        .await
        .map_err(|err| ApiError::InternalError(err.to_string()))?;

    let clients_count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {}", tables::CLIENT))
        .fetch_one(&db.pool)
        .await
        .map_err(|err| ApiError::InternalError(err.to_string()))?;

    Ok(Json(OnboardingStatusResp::success(OnboardingStatusData {
        completed: settings.onboarding_completed,
        servers_count: servers_count as usize,
        clients_count: clients_count as usize,
    })))
}

/// POST /api/onboarding/complete
pub async fn complete(
    State(state): State<Arc<AppState>>,
    Json(request): Json<OnboardingCompleteReq>,
) -> Result<Json<OnboardingActionResp>, ApiError> {
    set_onboarding_completed(state, request.completed).await
}

/// POST /api/onboarding/reset
pub async fn reset(State(state): State<Arc<AppState>>) -> Result<Json<OnboardingActionResp>, ApiError> {
    set_onboarding_completed(state, false).await
}

/// POST /api/onboarding/server-scan
pub async fn server_scan(
    State(state): State<Arc<AppState>>,
    Json(request): Json<OnboardingServerScanReq>,
) -> Result<Json<OnboardingServerScanResp>, ApiError> {
    let service = state
        .client_service
        .as_ref()
        .ok_or_else(|| ApiError::InternalError("Client service unavailable".into()))?;
    let descriptors = service
        .list_clients(true, false)
        .await
        .map_err(|err| ApiError::InternalError(err.to_string()))?;
    let mut descriptor_by_id = descriptors
        .into_iter()
        .map(|descriptor| (descriptor.state.identifier().to_string(), descriptor))
        .collect::<HashMap<_, _>>();
    let mut candidates = HashMap::<String, OnboardingServerCandidate>::new();
    let mut by_name = HashMap::<String, String>::new();
    let mut by_fingerprint = HashMap::<String, String>::new();
    let mut errors = Vec::new();

    for client in request.clients {
        let Some(descriptor) = descriptor_by_id.remove(&client.identifier) else {
            errors.push(OnboardingServerScanError {
                client_name: client.identifier.clone(),
                message: "Client is not currently detected or registered".to_string(),
            });
            continue;
        };
        let client_name = descriptor.state.display_name().to_string();
        let scan_result = async {
            let request_config_path = client.config_path.trim();
            let config_path = if request_config_path.is_empty() {
                descriptor.config_path.as_deref()
            } else {
                Some(request_config_path)
            }
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "Client has no detected local config path".to_string())?;
            let parse_rule_owned = client.config_file_parse.as_ref().map(parse_rule_from_api_data);
            let inspected = service
                .inspect_config_path_for_import(&descriptor.state, config_path, parse_rule_owned.as_ref())
                .await
                .map_err(|e| e.to_string())?;
            Ok(build_import_plan_from_entries(
                inspected.inspection.entries,
                &client_name,
            ))
        }
        .await;

        let plan = match scan_result {
            Ok(plan) => plan,
            Err(message) => {
                errors.push(OnboardingServerScanError { client_name, message });
                continue;
            }
        };

        for (name, config) in plan.items {
            let normalized_name = name.trim().to_ascii_lowercase();
            if normalized_name.is_empty() {
                continue;
            }
            let name_key = format!("name:{normalized_name}");
            let fingerprint = candidate_fingerprint(&config);
            let fingerprint_key = if fingerprint.is_empty() {
                None
            } else {
                Some(format!("fingerprint:{fingerprint}"))
            };
            let existing_key = by_name.get(&name_key).cloned().or_else(|| {
                fingerprint_key
                    .as_ref()
                    .and_then(|fingerprint_key| by_fingerprint.get(fingerprint_key).cloned())
            });

            if let Some(existing_key) = existing_key {
                if let Some(candidate) = candidates.get_mut(&existing_key) {
                    push_source(candidate, &client.identifier, &client_name);
                }
                continue;
            }

            let key = fingerprint_key.clone().unwrap_or_else(|| name_key.clone());
            let kind = match ServerType::from_client_format(config.kind.trim()) {
                Ok(st) => st.client_format().to_string(),
                Err(_) => continue,
            };
            let candidate = OnboardingServerCandidate {
                key: key.clone(),
                name,
                kind,
                command: config.command,
                args: config.args.unwrap_or_default(),
                env: config.env.unwrap_or_default(),
                url: config.url,
                source_clients: vec![client_name.clone()],
                source_client_ids: vec![client.identifier.clone()],
            };
            by_name.insert(name_key, key.clone());
            if let Some(fingerprint_key) = fingerprint_key {
                by_fingerprint.insert(fingerprint_key, key.clone());
            }
            candidates.insert(key, candidate);
        }
    }

    let mut candidates = candidates.into_values().collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| candidate.name.to_ascii_lowercase());

    Ok(Json(OnboardingServerScanResp::success(OnboardingServerScanData {
        candidates,
        errors,
    })))
}

const NODE_RUNTIME_PROBES: &[RuntimeProbe<'_>] = &[RuntimeProbe {
    command: "node",
    args: &["--version"],
}];
const NPX_RUNTIME_PROBES: &[RuntimeProbe<'_>] = &[RuntimeProbe {
    command: "npx",
    args: &["--version"],
}];
const BUN_RUNTIME_PROBES: &[RuntimeProbe<'_>] = &[RuntimeProbe {
    command: "bun",
    args: &["--version"],
}];
const BUNX_RUNTIME_PROBES: &[RuntimeProbe<'_>] = &[RuntimeProbe {
    command: "bunx",
    args: &["--version"],
}];
#[cfg(all(windows, test))]
const PYTHON_RUNTIME_PROBES: &[RuntimeProbe<'_>] = &[
    RuntimeProbe {
        command: "python",
        args: &["--version"],
    },
    RuntimeProbe {
        command: "py",
        args: &["-3", "--version"],
    },
];
#[cfg(all(not(windows), test))]
const PYTHON_RUNTIME_PROBES: &[RuntimeProbe<'_>] = &[RuntimeProbe {
    command: "python3",
    args: &["--version"],
}];
const UV_RUNTIME_PROBES: &[RuntimeProbe<'_>] = &[RuntimeProbe {
    command: "uv",
    args: &["--version"],
}];
const UVX_RUNTIME_PROBES: &[RuntimeProbe<'_>] = &[RuntimeProbe {
    command: "uvx",
    args: &["--version"],
}];

fn runtime_detection_to_entry(detection: RuntimeDetection) -> RuntimeEntry {
    let source = detection.resolve_source.as_ref().map(resolve_source_to_label);

    RuntimeEntry {
        name: detection.name,
        available: detection.available,
        version: detection.version,
        path: detection.path,
        source,
    }
}

fn resolve_source_to_label(source: &ResolveSource) -> String {
    match source {
        ResolveSource::McpMateManaged => "mcpMate".to_string(),
        ResolveSource::SystemPath => "system".to_string(),
        ResolveSource::UvPython => "uvPython".to_string(),
    }
}

fn update_runtime_summary(
    entry: &RuntimeEntry,
    has_js: &mut bool,
    has_python: &mut bool,
) {
    if !entry.available {
        return;
    }

    match entry.name.as_str() {
        "node" | "bun" | "npx" | "bunx" => *has_js = true,
        "uv" | "uvx" => *has_python = true,
        _ => {}
    }
}

/// GET /api/onboarding/runtime-check
/// Detects available runtimes (node, bun, uv, etc.) on the host system.
pub async fn runtime_check(State(_state): State<Arc<AppState>>) -> Result<Json<RuntimeCheckResp>, ApiError> {
    let checks: &[(&str, &[RuntimeProbe<'_>])] = &[
        ("node", NODE_RUNTIME_PROBES),
        ("npx", NPX_RUNTIME_PROBES),
        ("bun", BUN_RUNTIME_PROBES),
        ("bunx", BUNX_RUNTIME_PROBES),
        ("uv", UV_RUNTIME_PROBES),
        ("uvx", UVX_RUNTIME_PROBES),
    ];

    let mut runtimes = Vec::with_capacity(checks.len());
    let mut has_js = false;
    let mut has_python = false;
    let mut detector = RuntimeDetector::new(crate::common::paths::global_paths());

    for &(name, probes) in checks {
        let entry = runtime_detection_to_entry(detector.detect(name, probes).await);
        update_runtime_summary(&entry, &mut has_js, &mut has_python);
        runtimes.push(entry);
    }

    Ok(Json(RuntimeCheckResp::success(RuntimeCheckData {
        runtimes,
        has_js_runtime: has_js,
        has_python_runtime: has_python,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::client::{ClientConfigFileParseData, ClientConfigType};
    use crate::api::models::onboarding::{OnboardingCompleteReq, OnboardingServerScanClient, OnboardingServerScanReq};
    use crate::api::routes::AppState;
    use crate::clients::models::ClientConfigFileState;
    use crate::clients::{
        ClientConfigService,
        service::settings::ActiveClientSettingsUpdate,
        source::{FileTemplateSource, TemplateRoot, db_client_config_source},
    };
    use crate::common::constants::database::tables;
    use crate::config::{
        client::init::{initialize_client_table, initialize_system_settings},
        database::Database,
        profile::init::initialize_profile_tables,
        server::init::initialize_server_tables,
    };
    use crate::core::{models::Config, pool::UpstreamConnectionPool, profile::ConfigApplicationStateManager};
    use crate::inspector::{calls::InspectorCallRegistry, sessions::InspectorSessionManager};
    use crate::system::metrics::MetricsCollector;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::{collections::HashSet, path::PathBuf, sync::Arc, time::Duration};
    use tempfile::TempDir;
    use tokio::sync::{Mutex, RwLock};

    #[test]
    fn resolve_source_to_label_classifies_correctly() {
        assert_eq!(resolve_source_to_label(&ResolveSource::McpMateManaged), "mcpMate");
        assert_eq!(resolve_source_to_label(&ResolveSource::SystemPath), "system");
        assert_eq!(resolve_source_to_label(&ResolveSource::UvPython), "uvPython");
    }

    #[test]
    fn runtime_detection_to_entry_uses_resolve_source() {
        use crate::runtime::RuntimeDetection;
        let detection = RuntimeDetection {
            name: "node".to_string(),
            available: true,
            version: Some("v22.0.0".to_string()),
            path: Some("/opt/homebrew/bin/node".to_string()),
            resolve_source: Some(ResolveSource::SystemPath),
        };
        let entry = runtime_detection_to_entry(detection);
        assert_eq!(entry.source.as_deref(), Some("system"));
    }

    #[test]
    fn runtime_detection_to_entry_no_source_when_unavailable() {
        use crate::runtime::RuntimeDetection;
        let detection = RuntimeDetection {
            name: "node".to_string(),
            available: false,
            version: None,
            path: None,
            resolve_source: None,
        };
        let entry = runtime_detection_to_entry(detection);
        assert!(entry.source.is_none());
    }

    struct TestContext {
        _temp_dir: TempDir,
        state: Arc<AppState>,
        pool: sqlx::SqlitePool,
    }

    async fn create_test_context() -> TestContext {
        let temp_dir = TempDir::new().expect("temp dir");
        let db_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");

        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&db_pool)
            .await
            .expect("enable foreign keys");

        crate::helpers::prepare_config_database(&db_pool).await;
        initialize_server_tables(&db_pool).await.expect("init server tables");
        initialize_profile_tables(&db_pool).await.expect("init profile tables");
        initialize_client_table(&db_pool).await.expect("init client table");
        crate::config::database::initialize_capability_catalog(&db_pool)
            .await
            .expect("init capability catalog");
        initialize_system_settings(&db_pool)
            .await
            .expect("init system settings");

        let database = Arc::new(Database {
            pool: db_pool.clone(),
            path: PathBuf::from(":memory:"),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        });

        let template_root = TemplateRoot::new(temp_dir.path().join("client-templates"));
        let template_source = Arc::new(
            FileTemplateSource::bootstrap(template_root)
                .await
                .expect("template source"),
        );
        ClientConfigService::seed_runtime_template_snapshots(&db_pool, template_source.as_ref())
            .await
            .expect("seed runtime templates");
        ClientConfigService::seed_client_runtime_rows(&db_pool, template_source.as_ref())
            .await
            .expect("seed runtime rows");

        let pool = Arc::new(db_pool);
        let client_service = Arc::new(
            ClientConfigService::with_source(
                pool.clone(),
                db_client_config_source(pool.clone()).expect("runtime source"),
            )
            .await
            .expect("client service"),
        );

        let state = Arc::new(AppState {
            connection_pool: Arc::new(Mutex::new(UpstreamConnectionPool::new(
                Arc::new(Config::default()),
                Some(database.clone()),
            ))),
            metrics_collector: Arc::new(MetricsCollector::new(Duration::from_secs(5))),
            http_proxy: None,
            profile_merge_service: None,
            database: Some(database),
            audit_database: None,
            audit_service: None,
            config_application_state: Arc::new(ConfigApplicationStateManager::new()),
            client_service: Some(client_service),
            inspector_calls: Arc::new(InspectorCallRegistry::new()),
            inspector_sessions: Arc::new(InspectorSessionManager::new()),
            oauth_manager: RwLock::new(Some(Arc::new(crate::core::oauth::OAuthManager::new(
                pool.as_ref().clone(),
            )))),
            secret_store: RwLock::new(None),
            secret_store_readiness: RwLock::new(crate::api::routes::unavailable_secret_store_readiness(
                "test_unavailable",
            )),
        });

        TestContext {
            _temp_dir: temp_dir,
            state,
            pool: pool.as_ref().clone(),
        }
    }

    #[tokio::test]
    async fn status_complete_and_reset_update_onboarding_state() {
        let context = create_test_context().await;

        sqlx::query(&format!(
            "INSERT INTO {} (id, identifier, name, display_name) VALUES (?, ?, ?, ?)",
            tables::CLIENT
        ))
        .bind("client-1")
        .bind("client-1")
        .bind("Client One")
        .bind("Client One")
        .execute(&context.pool)
        .await
        .expect("insert client");

        sqlx::query(&format!(
            "INSERT INTO {} (id, name, server_type, command) VALUES (?, ?, ?, ?)",
            tables::SERVER_CONFIG
        ))
        .bind("server-1")
        .bind("Server One")
        .bind("stdio")
        .bind("echo")
        .execute(&context.pool)
        .await
        .expect("insert server");

        let Json(initial_status) = get_status(State(context.state.clone()))
            .await
            .expect("get initial status");
        assert!(initial_status.success);
        let initial_data = initial_status.data.expect("initial status data");
        assert!(!initial_data.completed);
        assert_eq!(initial_data.clients_count, 1);
        assert_eq!(initial_data.servers_count, 1);

        let Json(complete_resp) = complete(
            State(context.state.clone()),
            Json(OnboardingCompleteReq { completed: true }),
        )
        .await
        .expect("complete onboarding");
        assert!(complete_resp.success);
        assert!(complete_resp.data.expect("complete data").ok);

        let Json(completed_status) = get_status(State(context.state.clone()))
            .await
            .expect("get completed status");
        assert!(completed_status.data.expect("completed status data").completed);

        let Json(reset_resp) = reset(State(context.state.clone())).await.expect("reset onboarding");
        assert!(reset_resp.success);
        assert!(reset_resp.data.expect("reset data").ok);

        let Json(reset_status) = get_status(State(context.state)).await.expect("get reset status");
        assert!(!reset_status.data.expect("reset status data").completed);
    }

    #[tokio::test]
    async fn server_scan_returns_error_for_unknown_client_identifier() {
        let context = create_test_context().await;

        let Json(response) = server_scan(
            State(context.state),
            Json(OnboardingServerScanReq {
                clients: vec![OnboardingServerScanClient {
                    identifier: "missing-client".to_string(),
                    display_name: Some("Missing Client".to_string()),
                    config_path: "/tmp/missing.json".to_string(),
                    config_file_parse: None,
                }],
            }),
        )
        .await
        .expect("server scan response");

        assert!(response.success);
        let data = response.data.expect("server scan data");
        assert!(data.candidates.is_empty());
        assert_eq!(data.errors.len(), 1);
        assert_eq!(data.errors[0].client_name, "missing-client");
    }

    #[tokio::test]
    async fn server_scan_uses_requested_parse_rule_for_selected_client() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("custom-client.json");
        tokio::fs::write(
            &config_path,
            r#"{"context_servers":{"Server A":{"command":"node","args":["server.js"],"env":{"A":"B"}}}}"#,
        )
        .await
        .expect("write client config");

        let service = context.state.client_service.as_ref().expect("client service");
        service
            .set_active_client_settings(
                "custom.client",
                ActiveClientSettingsUpdate {
                    display_name: Some("Custom Client".to_string()),
                    config_file_state: Some(ClientConfigFileState::WithConfigFile),
                    config_path: Some(config_path.to_string_lossy().to_string()),
                    clear_config_file_parse: true,
                    ..ActiveClientSettingsUpdate::default()
                },
            )
            .await
            .expect("create custom client");

        let Json(response) = server_scan(
            State(context.state),
            Json(OnboardingServerScanReq {
                clients: vec![OnboardingServerScanClient {
                    identifier: "custom.client".to_string(),
                    display_name: Some("Custom Client".to_string()),
                    config_path: config_path.to_string_lossy().to_string(),
                    config_file_parse: Some(ClientConfigFileParseData {
                        format: "json".to_string(),
                        container_type: ClientConfigType::Standard,
                        container_keys: vec!["context_servers".to_string()],
                    }),
                }],
            }),
        )
        .await
        .expect("server scan response");

        let data = response.data.expect("server scan data");
        assert!(data.errors.is_empty());
        assert_eq!(data.candidates.len(), 1);
        assert_eq!(data.candidates[0].name, "Server A");
        assert_eq!(data.candidates[0].command.as_deref(), Some("node"));
    }

    #[tokio::test]
    async fn server_scan_uses_wildcard_parse_rule_for_project_scoped_servers() {
        let context = create_test_context().await;
        let config_path = context._temp_dir.path().join("claude-code.json");
        tokio::fs::write(
            &config_path,
            r#"{"projects":{"/Volumes/External/GitHub/MCPMate":{"mcpServers":{"Project Server":{"command":"node","args":["server.js"],"env":{"A":"B"}}}}}}"#,
        )
        .await
        .expect("write client config");

        let service = context.state.client_service.as_ref().expect("client service");
        service
            .set_active_client_settings(
                "custom.client",
                ActiveClientSettingsUpdate {
                    display_name: Some("Custom Client".to_string()),
                    config_file_state: Some(ClientConfigFileState::WithConfigFile),
                    config_path: Some(config_path.to_string_lossy().to_string()),
                    clear_config_file_parse: true,
                    ..ActiveClientSettingsUpdate::default()
                },
            )
            .await
            .expect("create custom client");

        let Json(response) = server_scan(
            State(context.state),
            Json(OnboardingServerScanReq {
                clients: vec![OnboardingServerScanClient {
                    identifier: "custom.client".to_string(),
                    display_name: Some("Custom Client".to_string()),
                    config_path: config_path.to_string_lossy().to_string(),
                    config_file_parse: Some(ClientConfigFileParseData {
                        format: "json".to_string(),
                        container_type: ClientConfigType::Standard,
                        container_keys: vec!["projects.*.mcpServers".to_string()],
                    }),
                }],
            }),
        )
        .await
        .expect("server scan response");

        let data = response.data.expect("server scan data");
        assert!(data.errors.is_empty());
        assert_eq!(data.candidates.len(), 1);
        assert_eq!(data.candidates[0].name, "Project Server");
        assert_eq!(data.candidates[0].command.as_deref(), Some("node"));
    }

    #[tokio::test]
    async fn runtime_check_returns_expected_runtime_matrix_shape() {
        let context = create_test_context().await;

        let Json(response) = runtime_check(State(context.state))
            .await
            .expect("runtime check response");

        assert!(response.success);
        let data = response.data.expect("runtime check data");

        let names = data
            .runtimes
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<HashSet<_>>();
        let expected = HashSet::from(["node", "npx", "bun", "bunx", "uv", "uvx"]);
        assert_eq!(names, expected);

        let has_js_from_rows = data
            .runtimes
            .iter()
            .any(|entry| entry.available && matches!(entry.name.as_str(), "node" | "npx" | "bun" | "bunx"));
        let has_python_from_rows = data
            .runtimes
            .iter()
            .any(|entry| entry.available && matches!(entry.name.as_str(), "uv" | "uvx"));

        assert_eq!(data.has_js_runtime, has_js_from_rows);
        assert_eq!(data.has_python_runtime, has_python_from_rows);
    }

    #[cfg(windows)]
    #[test]
    fn python_runtime_probes_cover_windows_launchers() {
        let programs = PYTHON_RUNTIME_PROBES
            .iter()
            .map(|probe| probe.command)
            .collect::<HashSet<_>>();

        assert!(programs.contains("python"));
        assert!(programs.contains("py"));
    }

    #[cfg(not(windows))]
    #[test]
    fn python_runtime_probes_use_python3_off_windows() {
        assert_eq!(PYTHON_RUNTIME_PROBES.len(), 1);
        assert_eq!(PYTHON_RUNTIME_PROBES[0].command, "python3");
        assert_eq!(PYTHON_RUNTIME_PROBES[0].args, ["--version"]);
    }
}
