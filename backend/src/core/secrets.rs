use std::collections::{HashMap, HashSet};

use mcpmate_secrets::{
    SecretError, SecretResolver, UnavailableSecretResolver, extract_secret_references, resolve_placeholders,
};

use crate::core::models::MCPServerConfig;
use store::{LocalSecretStore, SecretUsageLocationInput, SecretUsageUpsertInput, SecretUsageView};

pub mod store;

pub fn resolve_runtime_server_config(
    config: &MCPServerConfig,
    resolver: &(impl SecretResolver + ?Sized),
) -> Result<MCPServerConfig, SecretError> {
    Ok(MCPServerConfig {
        source_fingerprint: config.source_fingerprint.clone(),
        kind: config.kind,
        command: resolve_optional_string(config.command.as_ref(), resolver)?,
        args: resolve_optional_vec(config.args.as_ref(), resolver)?,
        url: resolve_optional_string(config.url.as_ref(), resolver)?,
        env: resolve_optional_map(config.env.as_ref(), resolver)?,
        headers: resolve_optional_map(config.headers.as_ref(), resolver)?,
    })
}

pub fn resolve_runtime_server_config_with_optional_resolver(
    config: &MCPServerConfig,
    resolver: Option<&dyn SecretResolver>,
) -> Result<MCPServerConfig, SecretError> {
    match resolver {
        Some(resolver) => resolve_runtime_server_config(config, resolver),
        None => resolve_runtime_server_config(config, &UnavailableSecretResolver),
    }
}

fn resolve_optional_string(
    value: Option<&String>,
    resolver: &(impl SecretResolver + ?Sized),
) -> Result<Option<String>, SecretError> {
    value.map(|item| resolve_placeholders(item, resolver)).transpose()
}

fn resolve_optional_vec(
    values: Option<&Vec<String>>,
    resolver: &(impl SecretResolver + ?Sized),
) -> Result<Option<Vec<String>>, SecretError> {
    values
        .map(|items| {
            items
                .iter()
                .map(|item| resolve_placeholders(item, resolver))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
}

fn resolve_optional_map(
    values: Option<&HashMap<String, String>>,
    resolver: &(impl SecretResolver + ?Sized),
) -> Result<Option<HashMap<String, String>>, SecretError> {
    values
        .map(|items| {
            items
                .iter()
                .map(|(key, value)| resolve_placeholders(value, resolver).map(|resolved| (key.clone(), resolved)))
                .collect::<Result<HashMap<_, _>, _>>()
        })
        .transpose()
}

pub async fn sync_server_secret_usages(
    store: &LocalSecretStore,
    server_id: &str,
    config: &MCPServerConfig,
) -> anyhow::Result<()> {
    let usages = collect_secret_usages(server_id, config)?;
    store.replace_server_usages(server_id, usages).await
}

pub fn collect_secret_usages(
    server_id: &str,
    config: &MCPServerConfig,
) -> anyhow::Result<Vec<SecretUsageUpsertInput>> {
    let mut usages = Vec::new();

    if let Some(command) = config.command.as_ref() {
        push_usages_from_value(&mut usages, server_id, command, SecretUsageLocationInput::StdioCommand)?;
    }

    if let Some(args) = config.args.as_ref() {
        for (index, value) in args.iter().enumerate() {
            push_usages_from_value(
                &mut usages,
                server_id,
                value,
                SecretUsageLocationInput::StdioArgument { index },
            )?;
        }
    }

    if let Some(env) = config.env.as_ref() {
        for (name, value) in env {
            push_usages_from_value(
                &mut usages,
                server_id,
                value,
                SecretUsageLocationInput::StdioEnv { name: name.clone() },
            )?;
        }
    }

    if let Some(url) = config.url.as_ref() {
        push_usages_from_value(&mut usages, server_id, url, SecretUsageLocationInput::StreamableHttpUrl)?;
    }

    if let Some(headers) = config.headers.as_ref() {
        for (name, value) in headers {
            push_usages_from_value(
                &mut usages,
                server_id,
                value,
                SecretUsageLocationInput::StreamableHttpHeader { name: name.clone() },
            )?;
        }
    }

    dedup_secret_usages(&mut usages);
    Ok(usages)
}

pub fn is_usage_active_in_config(
    alias: &str,
    _server_id: &str,
    location: &SecretUsageLocationInput,
    config: &MCPServerConfig,
) -> anyhow::Result<bool> {
    // Check only the specific config field for this location, O(1) per usage.
    let value = match location {
        SecretUsageLocationInput::StdioCommand => config.command.as_deref(),
        SecretUsageLocationInput::StdioArgument { index } => config
            .args
            .as_ref()
            .and_then(|args| args.get(*index).map(|s| s.as_str())),
        SecretUsageLocationInput::StdioEnv { name } => config
            .env
            .as_ref()
            .and_then(|env| env.get(name.as_str()).map(|s| s.as_str())),
        SecretUsageLocationInput::StreamableHttpUrl => config.url.as_deref(),
        SecretUsageLocationInput::StreamableHttpHeader { name } => config
            .headers
            .as_ref()
            .and_then(|h| h.get(name.as_str()).map(|s| s.as_str())),
        SecretUsageLocationInput::OAuthToken | SecretUsageLocationInput::LlmProviderApiKey => return Ok(false),
    };

    let Some(value) = value else {
        return Ok(false);
    };

    // Check if the value still contains a reference to this secret alias.
    for reference in extract_secret_references(value)? {
        if reference.alias() == alias {
            return Ok(true);
        }
    }
    Ok(false)
}

fn push_usages_from_value(
    usages: &mut Vec<SecretUsageUpsertInput>,
    server_id: &str,
    value: &str,
    location: SecretUsageLocationInput,
) -> anyhow::Result<()> {
    for reference in extract_secret_references(value)? {
        usages.push(SecretUsageUpsertInput {
            alias: reference.alias().to_string(),
            server_id: server_id.to_string(),
            location: location.clone(),
        });
    }
    Ok(())
}

fn dedup_secret_usages(usages: &mut Vec<SecretUsageUpsertInput>) {
    let mut seen = HashSet::new();
    usages.retain(|usage| seen.insert((usage.alias.clone(), usage.server_id.clone(), usage.location.clone())));
}

pub fn usage_binding_key(
    server_id: &str,
    location: &SecretUsageLocationInput,
) -> String {
    location.binding_key(server_id)
}

pub async fn mcp_config_from_server(
    pool: &sqlx::SqlitePool,
    server_id: &str,
    server: &crate::config::models::server::Server,
) -> anyhow::Result<MCPServerConfig> {
    use crate::config::server::{get_server_args, get_server_env, get_server_headers};

    let args = get_server_args(pool, server_id)
        .await?
        .into_iter()
        .map(|arg| arg.arg_value)
        .collect::<Vec<_>>();
    let env = get_server_env(pool, server_id).await?;
    let headers = get_server_headers(pool, server_id).await?;

    Ok(MCPServerConfig {
        source_fingerprint: None,
        kind: server.server_type,
        command: server.command.clone(),
        args: if args.is_empty() { None } else { Some(args) },
        url: server.url.clone(),
        env: if env.is_empty() { None } else { Some(env) },
        headers: if headers.is_empty() { None } else { Some(headers) },
    })
}

pub async fn load_mcp_server_config(
    pool: &sqlx::SqlitePool,
    server_id: &str,
) -> anyhow::Result<Option<MCPServerConfig>> {
    use crate::config::server::get_server_by_id;

    let Some(server) = get_server_by_id(pool, server_id).await? else {
        return Ok(None);
    };

    Ok(Some(mcp_config_from_server(pool, server_id, &server).await?))
}

pub async fn preload_mcp_server_configs(
    pool: &sqlx::SqlitePool,
    server_ids: impl IntoIterator<Item = String>,
) -> anyhow::Result<HashMap<String, Option<MCPServerConfig>>> {
    let unique_ids: Vec<String> = server_ids.into_iter().collect::<HashSet<_>>().into_iter().collect();
    if unique_ids.is_empty() {
        return Ok(HashMap::new());
    }

    use crate::{
        common::constants::database::{columns, tables},
        config::models::{Server, ServerArg, ServerEnv},
    };

    let mut servers_by_id = HashMap::new();
    let mut args_by_server: HashMap<String, Vec<String>> = HashMap::new();
    let mut env_by_server: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut headers_by_server: HashMap<String, HashMap<String, String>> = HashMap::new();

    for chunk in unique_ids.chunks(500) {
        let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");

        let servers_query = format!(
            "SELECT * FROM {} WHERE {} IN ({placeholders})",
            tables::SERVER_CONFIG,
            columns::ID,
        );
        let mut servers_builder = sqlx::query_as::<_, Server>(&servers_query);
        for id in chunk {
            servers_builder = servers_builder.bind(id);
        }
        for server in servers_builder.fetch_all(pool).await? {
            if let Some(id) = server.id.clone() {
                servers_by_id.insert(id, server);
            }
        }

        let args_query = format!(
            "SELECT * FROM {} WHERE {} IN ({placeholders}) ORDER BY {}, arg_index",
            tables::SERVER_ARGS,
            columns::SERVER_ID,
            columns::SERVER_ID,
        );
        let mut args_builder = sqlx::query_as::<_, ServerArg>(&args_query);
        for id in chunk {
            args_builder = args_builder.bind(id);
        }
        for arg in args_builder.fetch_all(pool).await? {
            args_by_server.entry(arg.server_id).or_default().push(arg.arg_value);
        }

        let env_query = format!(
            "SELECT * FROM {} WHERE {} IN ({placeholders})",
            tables::SERVER_ENV,
            columns::SERVER_ID,
        );
        let mut env_builder = sqlx::query_as::<_, ServerEnv>(&env_query);
        for id in chunk {
            env_builder = env_builder.bind(id);
        }
        for env_var in env_builder.fetch_all(pool).await? {
            env_by_server
                .entry(env_var.server_id)
                .or_default()
                .insert(env_var.env_key, env_var.env_value);
        }

        let headers_query = format!(
            "SELECT {}, header_key, header_value FROM {} WHERE {} IN ({placeholders}) ORDER BY {}, header_key",
            columns::SERVER_ID,
            tables::SERVER_HEADERS,
            columns::SERVER_ID,
            columns::SERVER_ID,
        );
        let mut headers_builder = sqlx::query_as::<_, (String, String, String)>(&headers_query);
        for id in chunk {
            headers_builder = headers_builder.bind(id);
        }
        for (server_id, header_key, header_value) in headers_builder.fetch_all(pool).await? {
            headers_by_server
                .entry(server_id)
                .or_default()
                .insert(header_key, header_value);
        }
    }

    let mut cache = HashMap::with_capacity(unique_ids.len());
    for id in unique_ids {
        let config = servers_by_id.remove(&id).map(|server| {
            let args = args_by_server.remove(&id).filter(|args| !args.is_empty());
            let env = env_by_server.remove(&id).filter(|env| !env.is_empty());
            let headers = headers_by_server.remove(&id).filter(|headers| !headers.is_empty());
            MCPServerConfig {
                source_fingerprint: None,
                kind: server.server_type,
                command: server.command,
                args,
                url: server.url,
                env,
                headers,
            }
        });
        cache.insert(id, config);
    }

    Ok(cache)
}

pub fn resolve_secret_usage_status_from_cache(
    usage: &SecretUsageView,
    cache: &HashMap<String, Option<MCPServerConfig>>,
) -> anyhow::Result<&'static str> {
    let Some(config) = cache.get(&usage.server_id).and_then(|config| config.as_ref()) else {
        return Ok("stale");
    };

    Ok(
        if is_usage_active_in_config(&usage.alias, &usage.server_id, &usage.location, config)? {
            "active"
        } else {
            "stale"
        },
    )
}

/// Scan persisted server configs for secret placeholder references.
///
/// This is the source of truth for config-owned active usage counts. The
/// `secure_store_usages` index can lag behind config when sync did not run
/// (imports, legacy data, etc.).
pub async fn discover_config_usages(pool: &sqlx::SqlitePool) -> anyhow::Result<Vec<SecretUsageView>> {
    discover_config_usages_filtered(pool, None).await
}

/// Scan all owner records that can actively hold secret placeholders.
pub async fn discover_active_secret_usages(pool: &sqlx::SqlitePool) -> anyhow::Result<Vec<SecretUsageView>> {
    discover_active_secret_usages_filtered(pool, None).await
}

pub async fn discover_config_usages_for_alias(
    pool: &sqlx::SqlitePool,
    alias: &str,
) -> anyhow::Result<Vec<SecretUsageView>> {
    discover_config_usages_filtered(pool, Some(alias)).await
}

pub async fn discover_active_secret_usages_for_alias(
    pool: &sqlx::SqlitePool,
    alias: &str,
) -> anyhow::Result<Vec<SecretUsageView>> {
    discover_active_secret_usages_filtered(pool, Some(alias)).await
}

async fn discover_active_secret_usages_filtered(
    pool: &sqlx::SqlitePool,
    alias_filter: Option<&str>,
) -> anyhow::Result<Vec<SecretUsageView>> {
    use crate::config::server::get_all_servers;

    let servers = get_all_servers(pool).await?;
    let mut usages = discover_config_usages_with_servers(pool, &servers, alias_filter).await?;
    usages.extend(discover_oauth_usages_with_servers(pool, &servers, alias_filter).await?);
    usages.extend(discover_llm_provider_usages(pool, alias_filter).await?);
    dedup_secret_usage_views(&mut usages);
    Ok(usages)
}

async fn discover_config_usages_filtered(
    pool: &sqlx::SqlitePool,
    alias_filter: Option<&str>,
) -> anyhow::Result<Vec<SecretUsageView>> {
    use crate::config::server::get_all_servers;

    let servers = get_all_servers(pool).await?;
    discover_config_usages_with_servers(pool, &servers, alias_filter).await
}

async fn discover_config_usages_with_servers(
    pool: &sqlx::SqlitePool,
    servers: &[crate::config::models::server::Server],
    alias_filter: Option<&str>,
) -> anyhow::Result<Vec<SecretUsageView>> {
    let mut usages = Vec::new();
    for server in servers {
        let Some(server_id) = server.id.clone() else {
            continue;
        };
        let config = mcp_config_from_server(pool, &server_id, server).await?;
        for entry in collect_secret_usages(&server_id, &config)? {
            if alias_filter.is_some_and(|alias| entry.alias != alias) {
                continue;
            }
            usages.push(entry.into());
        }
    }
    Ok(usages)
}

async fn discover_oauth_usages_with_servers(
    pool: &sqlx::SqlitePool,
    servers: &[crate::config::models::server::Server],
    alias_filter: Option<&str>,
) -> anyhow::Result<Vec<SecretUsageView>> {
    use crate::config::server::{get_all_oauth_configs, get_all_oauth_tokens};

    let oauth_configs = get_all_oauth_configs(pool).await?;
    let oauth_tokens = get_all_oauth_tokens(pool).await?;

    let config_by_server: std::collections::HashMap<String, _> =
        oauth_configs.into_iter().map(|c| (c.server_id.clone(), c)).collect();
    let token_by_server: std::collections::HashMap<String, _> =
        oauth_tokens.into_iter().map(|t| (t.server_id.clone(), t)).collect();

    let mut usages = Vec::new();
    for server_model in servers {
        let Some(server_id) = server_model.id.as_deref() else {
            continue;
        };

        if let Some(config) = config_by_server.get(server_id) {
            if let Some(client_secret) = config.client_secret.as_deref() {
                push_oauth_usages_from_value(&mut usages, alias_filter, server_id, client_secret)?;
            }
        }

        if let Some(token) = token_by_server.get(server_id) {
            push_oauth_usages_from_value(&mut usages, alias_filter, server_id, &token.access_token)?;
            if let Some(refresh_token) = token.refresh_token.as_deref() {
                push_oauth_usages_from_value(&mut usages, alias_filter, server_id, refresh_token)?;
            }
        }
    }
    Ok(usages)
}

fn push_oauth_usages_from_value(
    usages: &mut Vec<SecretUsageView>,
    alias_filter: Option<&str>,
    server_id: &str,
    value: &str,
) -> anyhow::Result<()> {
    for reference in extract_secret_references(value)? {
        if alias_filter.is_some_and(|alias| reference.alias() != alias) {
            continue;
        }
        usages.push(SecretUsageView {
            alias: reference.alias().to_string(),
            server_id: server_id.to_string(),
            location: SecretUsageLocationInput::OAuthToken,
        });
    }
    Ok(())
}

async fn discover_llm_provider_usages(
    pool: &sqlx::SqlitePool,
    alias_filter: Option<&str>,
) -> anyhow::Result<Vec<SecretUsageView>> {
    use crate::config::llm::crud::get_all_providers;

    let providers = get_all_providers(pool).await?;
    let mut usages = Vec::new();
    for provider in providers {
        let Some(alias) = provider.secret_alias else {
            continue;
        };
        if alias_filter.is_some_and(|filter| alias != filter) {
            continue;
        }
        let Some(provider_id) = provider.id else {
            continue;
        };
        usages.push(SecretUsageView {
            alias,
            server_id: provider_id,
            location: SecretUsageLocationInput::LlmProviderApiKey,
        });
    }
    Ok(usages)
}

fn dedup_secret_usage_views(usages: &mut Vec<SecretUsageView>) {
    let mut seen = HashSet::new();
    usages.retain(|usage| seen.insert((usage.alias.clone(), usage.server_id.clone(), usage.location.clone())));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        common::{server::ServerType, status::EnabledStatus},
        config::{models::Server, server},
    };

    async fn setup_pool() -> sqlx::SqlitePool {
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
        server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        pool
    }

    fn build_server(
        id: &str,
        name: &str,
    ) -> Server {
        Server {
            id: Some(id.to_string()),
            name: name.to_string(),
            server_type: ServerType::StreamableHttp,
            command: None,
            url: Some(format!("https://example.com/{name}/[[secret:url-token]]")),
            source: None,
            enabled: EnabledStatus::Enabled,
            unify_direct_exposure_eligible: false,
            pending_import: false,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn discover_collects_stdio_argument_and_http_header_for_same_alias() {
        let server_id = "serv-context7";
        let config = MCPServerConfig {
            source_fingerprint: None,
            kind: ServerType::StreamableHttp,
            command: None,
            args: None,
            url: Some("https://example.com/mcp".to_string()),
            env: None,
            headers: Some(HashMap::from([(
                "context7_api_key".to_string(),
                "[[secret:server-context7-token]]".to_string(),
            )])),
        };
        let http_usages = collect_secret_usages(server_id, &config).expect("http usages");
        assert_eq!(http_usages.len(), 1);
        assert_eq!(http_usages[0].alias, "server-context7-token");

        let stdio_config = MCPServerConfig {
            source_fingerprint: None,
            kind: ServerType::Stdio,
            command: Some("npx".to_string()),
            args: Some(vec![
                "-y".to_string(),
                "@upstash/context7-mcp".to_string(),
                "--api-key".to_string(),
                "[[secret:server-context7-token]]".to_string(),
            ]),
            url: None,
            env: None,
            headers: None,
        };
        let stdio_usages = collect_secret_usages("serv-stdio", &stdio_config).expect("stdio usages");
        assert_eq!(stdio_usages.len(), 1);
        assert_eq!(stdio_usages[0].alias, "server-context7-token");
        assert!(matches!(
            stdio_usages[0].location,
            SecretUsageLocationInput::StdioArgument { index: 3 }
        ));
    }

    #[test]
    fn collect_deduplicates_same_alias_in_same_location() {
        let config = MCPServerConfig {
            source_fingerprint: None,
            kind: ServerType::StreamableHttp,
            command: None,
            args: None,
            url: Some("https://example.com/[[secret:token]]/[[secret:token]]".to_string()),
            env: None,
            headers: None,
        };

        let usages = collect_secret_usages("serv-http", &config).expect("secret usages");

        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].alias, "token");
        assert!(matches!(
            usages[0].location,
            SecretUsageLocationInput::StreamableHttpUrl
        ));
    }

    #[tokio::test]
    async fn preload_server_configs_batches_full_config_and_missing_servers() {
        let pool = setup_pool().await;
        let server_id = server::upsert_server(&pool, &build_server("serv-http", "http-server"))
            .await
            .expect("insert server");
        server::upsert_server_args(
            &pool,
            &server_id,
            &["--token".to_string(), "[[secret:arg-token]]".to_string()],
        )
        .await
        .expect("insert server args");
        server::upsert_server_env(
            &pool,
            &server_id,
            &HashMap::from([("API_KEY".to_string(), "[[secret:env-token]]".to_string())]),
        )
        .await
        .expect("insert server env");
        server::upsert_server_headers(
            &pool,
            &server_id,
            &HashMap::from([(
                "authorization".to_string(),
                "Bearer [[secret:header-token]]".to_string(),
            )]),
        )
        .await
        .expect("insert server headers");

        let cache = preload_mcp_server_configs(&pool, [server_id.clone(), "missing-server".to_string()])
            .await
            .expect("preload server configs");

        let config = cache
            .get(&server_id)
            .expect("server cache entry")
            .as_ref()
            .expect("server config");
        assert_eq!(
            config.args.as_deref(),
            Some(["--token".to_string(), "[[secret:arg-token]]".to_string()].as_slice())
        );
        assert_eq!(
            config
                .env
                .as_ref()
                .and_then(|env| env.get("API_KEY"))
                .map(String::as_str),
            Some("[[secret:env-token]]")
        );
        assert_eq!(
            config
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer [[secret:header-token]]")
        );
        assert!(matches!(cache.get("missing-server"), Some(None)));

        let active_usage = SecretUsageView {
            alias: "header-token".to_string(),
            server_id: server_id.clone(),
            location: SecretUsageLocationInput::StreamableHttpHeader {
                name: "authorization".to_string(),
            },
        };
        let stale_usage = SecretUsageView {
            alias: "header-token".to_string(),
            server_id: "missing-server".to_string(),
            location: SecretUsageLocationInput::StreamableHttpHeader {
                name: "authorization".to_string(),
            },
        };

        assert_eq!(
            resolve_secret_usage_status_from_cache(&active_usage, &cache).expect("active status"),
            "active"
        );
        assert_eq!(
            resolve_secret_usage_status_from_cache(&stale_usage, &cache).expect("stale status"),
            "stale"
        );
    }
}
