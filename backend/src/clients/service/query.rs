use super::ClientConfigService;
use crate::clients::error::{ConfigError, ConfigResult};
use crate::clients::models::{CapabilitySource, ServerTemplateInput};
use crate::common::constants::defaults;
use crate::config::profile::basic::get_active_profile;
use serde_json::json;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow)]
pub struct ServerRow {
    pub id: String,
    pub name: String,
}

#[derive(Debug)]
pub enum ServerSelection {
    AllEnabled,
    Profile(String),
    Profiles(Vec<String>),
    Explicit(Vec<String>),
}

impl ClientConfigService {
    pub async fn prepare_servers(
        &self,
        options: &super::ClientRenderOptions,
    ) -> ConfigResult<Vec<ServerTemplateInput>> {
        let selection = self.resolve_server_selection(options).await?;
        let rows = self.fetch_servers(selection).await?;

        let mut servers = Vec::with_capacity(rows.len());
        for row in rows {
            servers.push(self.map_server_row(row).await?);
        }

        Ok(servers)
    }

    async fn resolve_server_selection(
        &self,
        options: &super::ClientRenderOptions,
    ) -> ConfigResult<ServerSelection> {
        if matches!(options.mode, crate::clients::models::ConfigMode::Native) {
            if let Some(ids) = &options.server_ids {
                if !ids.is_empty() {
                    return Ok(ServerSelection::Explicit(ids.clone()));
                }
            }
        }

        if let Some(profile_id) = &options.profile_id {
            return Ok(ServerSelection::Profile(profile_id.clone()));
        }

        if let Some(state) = self.fetch_state(&options.client_id).await? {
            let capability_config = state.capability_config()?;
            match capability_config.capability_source {
                CapabilitySource::Activated => {}
                CapabilitySource::Profiles => {
                    return Ok(ServerSelection::Profiles(capability_config.selected_profile_ids));
                }
                CapabilitySource::Custom => {
                    let profile_id = capability_config.custom_profile_id.ok_or_else(|| {
                        ConfigError::DataAccessError(format!(
                            "custom capability source requires a custom profile for {}",
                            options.client_id
                        ))
                    })?;
                    return Ok(ServerSelection::Profile(profile_id));
                }
            }
        }

        let active_profiles = get_active_profile(&self.db_pool)
            .await
            .map_err(|err| crate::clients::ConfigError::DataAccessError(err.to_string()))?;

        let mut active_ids: Vec<String> = active_profiles.into_iter().filter_map(|p| p.id).collect();
        active_ids.sort();
        active_ids.dedup();

        if active_ids.is_empty() {
            return Ok(ServerSelection::AllEnabled);
        }
        if active_ids.len() == 1 {
            return Ok(ServerSelection::Profile(active_ids.remove(0)));
        }
        Ok(ServerSelection::Profiles(active_ids))
    }

    async fn fetch_servers(
        &self,
        selection: ServerSelection,
    ) -> ConfigResult<Vec<ServerRow>> {
        match selection {
            ServerSelection::AllEnabled => {
                let rows = sqlx::query_as::<_, ServerRow>(
                    r#"
                    SELECT id, name
                    FROM server_config
                    WHERE enabled = 1
                    ORDER BY name
                    "#,
                )
                .fetch_all(&*self.db_pool)
                .await
                .map_err(|err| crate::clients::ConfigError::DataAccessError(err.to_string()))?;
                Ok(rows)
            }
            ServerSelection::Profile(profile_id) => {
                let rows = sqlx::query_as::<_, ServerRow>(
                    r#"
                    SELECT sc.id, sc.name
                    FROM server_config sc
                    WHERE sc.enabled = 1
                      AND (
                        EXISTS (
                          SELECT 1 FROM profile_server_relationships psr
                          WHERE psr.profile_id = ?
                            AND psr.server_id = sc.id
                            AND psr.enabled = 1
                        )
                        OR EXISTS (
                          SELECT 1
                          FROM profile_capability_refs pcr
                          JOIN capability_refs cr ON cr.ref_id = pcr.ref_id
                          WHERE pcr.profile_id = ?
                            AND cr.server_id = sc.id
                            AND pcr.enabled = 1
                            AND NOT EXISTS (
                              SELECT 1
                              FROM profile_server_relationships gate
                              WHERE gate.profile_id = pcr.profile_id
                                AND gate.server_id = cr.server_id
                                AND gate.enabled = 0
                            )
                        )
                      )
                    ORDER BY sc.name
                    "#,
                )
                .bind(&profile_id)
                .bind(&profile_id)
                .fetch_all(&*self.db_pool)
                .await
                .map_err(|err| crate::clients::ConfigError::DataAccessError(err.to_string()))?;
                Ok(rows)
            }
            ServerSelection::Profiles(profile_ids) => {
                if profile_ids.is_empty() {
                    return Ok(Vec::new());
                }
                let placeholders = vec!["?"; profile_ids.len()].join(", ");
                let sql = format!(
                    r#"
                    SELECT DISTINCT sc.id, sc.name
                    FROM server_config sc
                    WHERE sc.enabled = 1
                      AND (
                        EXISTS (
                          SELECT 1 FROM profile_server_relationships psr
                          WHERE psr.profile_id IN ({})
                            AND psr.server_id = sc.id
                            AND psr.enabled = 1
                        )
                        OR EXISTS (
                          SELECT 1
                          FROM profile_capability_refs pcr
                          JOIN capability_refs cr ON cr.ref_id = pcr.ref_id
                          WHERE pcr.profile_id IN ({})
                            AND cr.server_id = sc.id
                            AND pcr.enabled = 1
                            AND NOT EXISTS (
                              SELECT 1
                              FROM profile_server_relationships gate
                              WHERE gate.profile_id = pcr.profile_id
                                AND gate.server_id = cr.server_id
                                AND gate.enabled = 0
                            )
                        )
                      )
                    ORDER BY sc.name
                    "#,
                    placeholders, placeholders
                );
                let mut query = sqlx::query_as::<_, ServerRow>(&sql);
                for id in &profile_ids {
                    query = query.bind(id);
                }
                for id in &profile_ids {
                    query = query.bind(id);
                }
                query
                    .fetch_all(&*self.db_pool)
                    .await
                    .map_err(|err| crate::clients::ConfigError::DataAccessError(err.to_string()))
            }
            ServerSelection::Explicit(ids) => {
                if ids.is_empty() {
                    return Ok(Vec::new());
                }
                let placeholders = vec!["?"; ids.len()].join(", ");
                let sql = format!(
                    r#"
                    SELECT id, name
                    FROM server_config
                    WHERE id IN ({}) AND enabled = 1
                    ORDER BY name
                    "#,
                    placeholders
                );
                let mut query = sqlx::query_as::<_, ServerRow>(&sql);
                for id in ids {
                    query = query.bind(id);
                }
                query
                    .fetch_all(&*self.db_pool)
                    .await
                    .map_err(|err| crate::clients::ConfigError::DataAccessError(err.to_string()))
            }
        }
    }
}

impl ClientConfigService {
    pub(super) async fn map_server_row(
        &self,
        row: crate::clients::service::query::ServerRow,
    ) -> ConfigResult<ServerTemplateInput> {
        let validated = crate::config::server::load_validated_server_transport(&self.db_pool, &row.id)
            .await
            .map_err(|err| ConfigError::DataAccessError(err.to_string()))?;
        let transport = validated.server_type().client_format().to_string();
        let (command, args, env, url) = match validated {
            crate::config::models::ValidatedTransport::Stdio { command, args, env } => (
                Some(command),
                args,
                env.into_iter()
                    .map(|(key, value)| (key, value.runtime_value()))
                    .collect(),
                None,
            ),
            crate::config::models::ValidatedTransport::Sse { endpoint, .. }
            | crate::config::models::ValidatedTransport::StreamableHttp { endpoint, .. } => (
                None,
                Vec::new(),
                std::collections::HashMap::new(),
                Some(endpoint.to_string()),
            ),
        };

        let mut metadata = std::collections::HashMap::new();
        metadata.insert("server_id".to_string(), json!(row.id));
        metadata.insert("runtime".to_string(), json!(defaults::RUNTIME));

        Ok(ServerTemplateInput {
            name: row.name.clone(),
            display_name: Some(row.name),
            transport,
            command,
            args,
            env,
            url,
            headers: std::collections::HashMap::new(),
            metadata,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::models::ConfigMode;
    use crate::clients::source::{ClientConfigSource, DbTemplateSource, FileTemplateSource, TemplateRoot};
    use crate::common::profile::ProfileType;
    use crate::config::{
        models::{ConfigValue, HttpTransportKind, Profile, Server, ServerTransportDraft},
        profile,
        server::upsert_server_definition,
    };
    use sqlx::sqlite::SqlitePoolOptions;
    use std::{
        collections::{BTreeMap, HashMap},
        sync::Arc,
    };
    use tempfile::TempDir;

    async fn create_test_service() -> (TempDir, ClientConfigService) {
        let temp_dir = TempDir::new().expect("temp dir");
        let pool = Arc::new(
            SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .expect("sqlite pool"),
        );
        crate::helpers::prepare_config_database(pool.as_ref()).await;
        crate::config::initialization::run_initialization(pool.as_ref())
            .await
            .expect("initialize database");

        let template_root = TemplateRoot::new(temp_dir.path().join("client-templates"));
        let source = Arc::new(
            FileTemplateSource::bootstrap(template_root)
                .await
                .expect("template source"),
        );
        ClientConfigService::seed_runtime_template_snapshots(pool.as_ref(), source.as_ref())
            .await
            .expect("seed runtime templates");
        ClientConfigService::seed_client_runtime_rows(pool.as_ref(), source.as_ref())
            .await
            .expect("seed runtime rows");
        let runtime_source: Arc<dyn ClientConfigSource> =
            Arc::new(DbTemplateSource::new(pool.clone()).expect("runtime source"));
        let service = ClientConfigService::with_source(pool, runtime_source)
            .await
            .expect("client config service");

        (temp_dir, service)
    }

    async fn insert_profile(
        service: &ClientConfigService,
        name: &str,
        profile_type: ProfileType,
        is_active: bool,
    ) -> String {
        let mut profile = Profile::new(name.to_string(), profile_type);
        profile.is_active = is_active;
        crate::helpers::insert_profile(service.db_pool.as_ref(), &profile).await
    }

    fn native_render_options(server_id: String) -> crate::clients::ClientRenderOptions {
        crate::clients::ClientRenderOptions {
            client_id: "client-a".to_string(),
            mode: ConfigMode::Native,
            profile_id: None,
            server_ids: Some(vec![server_id]),
            dry_run: true,
        }
    }

    #[tokio::test]
    async fn prepare_servers_uses_validated_stdio_transport_over_legacy_projections() {
        let (_temp_dir, service) = create_test_service().await;
        let server = Server::new_stdio("typed-stdio".to_string(), Some("unused".to_string()));
        let server_id = upsert_server_definition(
            service.db_pool.as_ref(),
            &server,
            &ServerTransportDraft::Stdio {
                command: Some("typed-command".to_string()),
                args: vec!["--typed-arg".to_string()],
                env: BTreeMap::from([
                    (
                        "MODE".to_string(),
                        ConfigValue::Literal {
                            value: "typed".to_string(),
                        },
                    ),
                    (
                        "TOKEN".to_string(),
                        ConfigValue::SecretRef {
                            alias: "typed-token".to_string(),
                        },
                    ),
                ]),
            },
        )
        .await
        .expect("store typed stdio definition");

        sqlx::query(
            "UPDATE server_config \
             SET command = 'legacy-command', \
                 url = 'https://legacy.example.test/mcp', \
                 server_type = 'streamable_http' \
             WHERE id = ?",
        )
        .bind(&server_id)
        .execute(service.db_pool.as_ref())
        .await
        .expect("diverge legacy server projection");
        sqlx::query("DELETE FROM server_args WHERE server_id = ?")
            .bind(&server_id)
            .execute(service.db_pool.as_ref())
            .await
            .expect("clear projected arguments");
        sqlx::query(
            "INSERT INTO server_args (id, server_id, server_name, arg_index, arg_value) \
             VALUES ('legacy-arg', ?, 'typed-stdio', 0, '--legacy-arg')",
        )
        .bind(&server_id)
        .execute(service.db_pool.as_ref())
        .await
        .expect("insert legacy argument");
        sqlx::query("DELETE FROM server_env WHERE server_id = ?")
            .bind(&server_id)
            .execute(service.db_pool.as_ref())
            .await
            .expect("clear projected environment");
        sqlx::query(
            "INSERT INTO server_env (id, server_id, server_name, env_key, env_value) \
             VALUES ('legacy-env', ?, 'typed-stdio', 'TOKEN', 'legacy-token')",
        )
        .bind(&server_id)
        .execute(service.db_pool.as_ref())
        .await
        .expect("insert legacy environment");

        let servers = service
            .prepare_servers(&native_render_options(server_id))
            .await
            .expect("prepare typed server");

        assert_eq!(servers.len(), 1);
        let exported = &servers[0];
        assert_eq!(exported.transport, "stdio");
        assert_eq!(exported.command.as_deref(), Some("typed-command"));
        assert_eq!(exported.args, ["--typed-arg"]);
        assert_eq!(
            exported.env,
            HashMap::from([
                ("MODE".to_string(), "typed".to_string()),
                ("TOKEN".to_string(), "[[secret:typed-token]]".to_string()),
            ])
        );
        assert!(exported.url.is_none());
        assert!(exported.headers.is_empty());
    }

    #[tokio::test]
    async fn prepare_servers_uses_typed_http_endpoint_without_exporting_headers() {
        let (_temp_dir, service) = create_test_service().await;
        let server = Server::new_stdio("typed-http".to_string(), Some("unused".to_string()));
        let server_id = upsert_server_definition(
            service.db_pool.as_ref(),
            &server,
            &ServerTransportDraft::Http {
                protocol: HttpTransportKind::StreamableHttp,
                endpoint: Some("https://typed.example.test/mcp".to_string()),
                headers: BTreeMap::from([(
                    "Authorization".to_string(),
                    ConfigValue::SecretRef {
                        alias: "http-token".to_string(),
                    },
                )]),
            },
        )
        .await
        .expect("store typed HTTP definition");

        sqlx::query(
            "UPDATE server_config \
             SET command = 'legacy-command', \
                 url = 'https://legacy.example.test/mcp', \
                 server_type = 'stdio' \
             WHERE id = ?",
        )
        .bind(&server_id)
        .execute(service.db_pool.as_ref())
        .await
        .expect("diverge legacy HTTP projection");

        let servers = service
            .prepare_servers(&native_render_options(server_id))
            .await
            .expect("prepare typed HTTP server");

        assert_eq!(servers.len(), 1);
        let exported = &servers[0];
        assert_eq!(exported.transport, "streamable_http");
        assert!(exported.command.is_none());
        assert!(exported.args.is_empty());
        assert!(exported.env.is_empty());
        assert_eq!(exported.url.as_deref(), Some("https://typed.example.test/mcp"));
        assert!(exported.headers.is_empty());
    }

    #[tokio::test]
    async fn prepare_servers_fails_closed_when_the_transport_draft_is_missing() {
        let (_temp_dir, service) = create_test_service().await;
        let server_id = crate::config::server::upsert_server(
            service.db_pool.as_ref(),
            &Server::new_stdio("missing-transport".to_string(), Some("legacy-command".to_string())),
        )
        .await
        .expect("store legacy server projection");

        let error = service
            .prepare_servers(&native_render_options(server_id))
            .await
            .expect_err("missing transport draft must fail the entire export");

        assert!(matches!(error, ConfigError::DataAccessError(_)));
        assert!(error.to_string().contains("ServerTransportDraft is missing"));
    }

    #[tokio::test]
    async fn prepare_servers_fails_closed_when_the_transport_draft_is_invalid() {
        let (_temp_dir, service) = create_test_service().await;
        let server_id = crate::config::server::upsert_server(
            service.db_pool.as_ref(),
            &Server::new_stdio("invalid-transport".to_string(), Some("legacy-command".to_string())),
        )
        .await
        .expect("store legacy server projection");
        sqlx::query("INSERT INTO server_transport (server_id, draft_json) VALUES (?, ?)")
            .bind(&server_id)
            .bind(r#"{"kind":"stdio","command":null,"args":[],"env":{}}"#)
            .execute(service.db_pool.as_ref())
            .await
            .expect("store invalid transport draft");

        let error = service
            .prepare_servers(&native_render_options(server_id))
            .await
            .expect_err("invalid transport draft must fail the entire export");

        assert!(matches!(error, ConfigError::DataAccessError(_)));
        assert!(error.to_string().contains("ServerTransportDraft is invalid"));
    }

    #[tokio::test]
    async fn set_capability_config_normalizes_selected_profiles() {
        let (_temp_dir, service) = create_test_service().await;
        let profile_a = insert_profile(&service, "profile-a", ProfileType::Shared, false).await;
        let profile_b = insert_profile(&service, "profile-b", ProfileType::Shared, false).await;

        let config = service
            .set_capability_config(
                "client-a",
                CapabilitySource::Profiles,
                vec![format!("  {}  ", profile_b), profile_a.clone(), profile_b.clone()],
            )
            .await
            .expect("set capability config");

        let mut expected = vec![profile_a, profile_b];
        expected.sort();

        assert_eq!(config.capability_source, CapabilitySource::Profiles);
        assert_eq!(config.selected_profile_ids, expected);
        assert!(config.custom_profile_id.is_none());
        assert_eq!(
            service
                .get_capability_config("client-a")
                .await
                .expect("get capability config")
                .expect("stored config"),
            config
        );
    }

    #[tokio::test]
    async fn set_capability_config_custom_creates_host_app_profile() {
        let (_temp_dir, service) = create_test_service().await;

        let config = service
            .set_capability_config("client-a", CapabilitySource::Custom, vec!["ignored".to_string()])
            .await
            .expect("set custom capability config");

        let custom_profile_id = config.custom_profile_id.clone().expect("custom profile id");
        let profile = profile::get_profile(service.db_pool.as_ref(), &custom_profile_id)
            .await
            .expect("load custom profile")
            .expect("custom profile exists");

        assert_eq!(config.capability_source, CapabilitySource::Custom);
        assert!(config.selected_profile_ids.is_empty());
        assert_eq!(profile.profile_type, ProfileType::HostApp);
        assert_eq!(profile.name, "client-a_custom");
    }

    #[tokio::test]
    async fn update_capability_config_and_invalidate_rejects_empty_profiles_selection() {
        let (_temp_dir, service) = create_test_service().await;

        let error = service
            .update_capability_config_and_invalidate("client-a", CapabilitySource::Profiles, Vec::new(), HashMap::new())
            .await
            .expect_err("empty profiles selection should fail");

        assert!(
            error
                .to_string()
                .contains("profiles capability source requires at least one selected profile")
        );
    }

    #[tokio::test]
    async fn resolve_server_selection_prefers_client_profiles_over_active_profiles() {
        let (_temp_dir, service) = create_test_service().await;
        let active_profile_id = insert_profile(&service, "active-profile", ProfileType::Shared, true).await;
        let selected_profile_id = insert_profile(&service, "selected-profile", ProfileType::Shared, false).await;

        service
            .set_capability_config(
                "client-a",
                CapabilitySource::Profiles,
                vec![selected_profile_id.clone()],
            )
            .await
            .expect("set profile capability config");

        let selection = service
            .resolve_server_selection(&crate::clients::ClientRenderOptions {
                client_id: "client-a".to_string(),
                mode: ConfigMode::Managed,
                profile_id: None,
                server_ids: None,
                dry_run: true,
            })
            .await
            .expect("resolve selection");

        match selection {
            ServerSelection::Profiles(profile_ids) => assert_eq!(profile_ids, vec![selected_profile_id.clone()]),
            other => panic!("expected selected profile, got {other:?}"),
        }

        assert_ne!(selected_profile_id, active_profile_id);
    }

    #[tokio::test]
    async fn resolve_server_selection_uses_custom_profile() {
        let (_temp_dir, service) = create_test_service().await;

        let config = service
            .set_capability_config("client-a", CapabilitySource::Custom, Vec::new())
            .await
            .expect("set custom capability config");

        let selection = service
            .resolve_server_selection(&crate::clients::ClientRenderOptions {
                client_id: "client-a".to_string(),
                mode: ConfigMode::Managed,
                profile_id: None,
                server_ids: None,
                dry_run: true,
            })
            .await
            .expect("resolve selection");

        match selection {
            ServerSelection::Profile(profile_id) => {
                assert_eq!(Some(profile_id), config.custom_profile_id)
            }
            other => panic!("expected custom profile, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_server_selection_rejects_custom_source_without_its_profile() {
        let (_temp_dir, service) = create_test_service().await;
        service
            .set_capability_config("client-a", CapabilitySource::Activated, Vec::new())
            .await
            .expect("create client capability config");
        let updated = sqlx::query(
            "UPDATE client \
             SET capability_source = 'custom', custom_profile_id = NULL \
             WHERE identifier = 'client-a'",
        )
        .execute(service.db_pool.as_ref())
        .await
        .expect("corrupt custom capability config");
        assert_eq!(updated.rows_affected(), 1);

        let error = service
            .resolve_server_selection(&crate::clients::ClientRenderOptions {
                client_id: "client-a".to_string(),
                mode: ConfigMode::Native,
                profile_id: None,
                server_ids: None,
                dry_run: true,
            })
            .await
            .expect_err("custom capability source without a profile must fail");

        assert!(
            error
                .to_string()
                .contains("custom capability source requires a custom profile")
        );
    }
}
