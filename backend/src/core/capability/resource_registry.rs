use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use sqlx::{Pool, Sqlite, SqliteConnection};
use url::Url;

use super::resource_uri::{
    ResourceAddressKind, expand_upstream_resource_template, project_upstream_resource_uri_through_template,
    resource_alias_candidates,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResourceRouteSource {
    Listed,
    Template {
        upstream_template: String,
        arguments: BTreeMap<String, String>,
    },
    Issued,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedResourceRoute {
    pub(crate) server_id: String,
    pub(crate) server_name: String,
    pub(crate) external_uri: String,
    pub(crate) upstream_uri: String,
    pub(crate) source: ResourceRouteSource,
}

#[derive(Debug, thiserror::Error)]
#[error("{source}")]
struct InvalidResourceRouteError {
    #[source]
    source: anyhow::Error,
}

fn invalid_resource_route_error(source: anyhow::Error) -> anyhow::Error {
    InvalidResourceRouteError { source }.into()
}

pub(crate) fn is_invalid_resource_route_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|source| source.downcast_ref::<InvalidResourceRouteError>().is_some())
}

fn parse_external_uri(external_uri: &str) -> Result<Url> {
    validate_percent_encoding(external_uri)?;
    let parsed =
        Url::parse(external_uri).with_context(|| format!("Invalid canonical resource URI '{external_uri}'"))?;
    if parsed.scheme() != "mcpmate" || parsed.host_str() != Some("resources") {
        bail!("Canonical resource URI must use 'mcpmate://resources'");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.port().is_some() {
        bail!("Canonical resource URI cannot contain user information or a port");
    }
    if parsed.fragment().is_some() {
        bail!("Canonical resource URI cannot contain a fragment");
    }
    let segments = parsed
        .path_segments()
        .context("Canonical resource URI must have path segments")?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let minimum_segments = if segments.first().copied() == Some("template") {
        3
    } else {
        2
    };
    if segments.len() < minimum_segments {
        bail!("Canonical resource URI has an invalid path structure");
    }
    Ok(parsed)
}

pub(crate) fn validate_external_resource_uri(external_uri: &str) -> Result<()> {
    parse_external_uri(external_uri).map(|_| ())
}

fn validate_percent_encoding(value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                bail!("Canonical resource URI contains invalid percent encoding");
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    Ok(())
}

pub(crate) async fn resolve_resource_route(
    pool: &Pool<Sqlite>,
    external_uri: &str,
) -> Result<ResolvedResourceRoute> {
    let parsed = parse_external_uri(external_uri).map_err(invalid_resource_route_error)?;
    if let Some((server_id, server_name, upstream_uri)) = sqlx::query_as::<_, (String, String, String)>(
        "SELECT server_id, server_name, resource_uri FROM server_resources WHERE unique_uri = ?",
    )
    .bind(external_uri)
    .fetch_optional(pool)
    .await
    .context("Failed to resolve listed resource route")?
    {
        return Ok(ResolvedResourceRoute {
            server_id,
            server_name,
            external_uri: external_uri.to_string(),
            upstream_uri,
            source: ResourceRouteSource::Listed,
        });
    }
    if let Some((server_id, server_name, upstream_uri)) = sqlx::query_as::<_, (String, String, String)>(
        "SELECT server_id, server_name, resource_uri FROM server_issued_resources WHERE unique_uri = ?",
    )
    .bind(external_uri)
    .fetch_optional(pool)
    .await
    .context("Failed to resolve issued resource route")?
    {
        return Ok(ResolvedResourceRoute {
            server_id,
            server_name,
            external_uri: external_uri.to_string(),
            upstream_uri,
            source: ResourceRouteSource::Issued,
        });
    }

    let segments = parsed
        .path_segments()
        .context("Canonical resource URI must have path segments")?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.first().copied() != Some("template") {
        return Err(invalid_resource_route_error(anyhow!(
            "Canonical resource URI '{external_uri}' is not registered"
        )));
    }
    let namespace = segments
        .get(1)
        .context("Canonical resource template URI is missing its namespace")
        .map_err(invalid_resource_route_error)?;
    let template_rows = sqlx::query_as::<_, (String, String, String, String)>(
        "SELECT server_id, server_name, uri_template, unique_name FROM server_resource_templates WHERE server_name = ?",
    )
    .bind(namespace)
    .fetch_all(pool)
    .await
    .context("Failed to resolve resource template route")?;
    let mut matches = Vec::new();
    for (server_id, server_name, upstream_template, external_template) in template_rows {
        let expansion = match expand_upstream_resource_template(&external_template, &upstream_template, external_uri) {
            Ok(expansion) => expansion,
            Err(error) if super::resource_uri::is_malformed_canonical_resource_uri_error(&error) => {
                return Err(invalid_resource_route_error(error));
            }
            Err(error) => return Err(error),
        };
        if let Some((upstream_uri, arguments)) = expansion {
            matches.push((server_id, server_name, upstream_template, upstream_uri, arguments));
        }
    }
    let [(server_id, server_name, upstream_template, upstream_uri, arguments)] = matches.as_slice() else {
        if matches.is_empty() {
            return Err(invalid_resource_route_error(anyhow!(
                "Canonical resource URI '{external_uri}' is not registered"
            )));
        }
        return Err(invalid_resource_route_error(anyhow!(
            "Canonical resource template URI '{external_uri}' is ambiguous"
        )));
    };

    Ok(ResolvedResourceRoute {
        server_id: server_id.clone(),
        server_name: server_name.clone(),
        external_uri: external_uri.to_string(),
        upstream_uri: upstream_uri.clone(),
        source: ResourceRouteSource::Template {
            upstream_template: upstream_template.clone(),
            arguments: arguments.clone(),
        },
    })
}

async fn external_uri_is_occupied(
    connection: &mut SqliteConnection,
    external_uri: &str,
) -> Result<bool> {
    let occupied: i64 = sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM server_resources WHERE unique_uri = ?
            UNION ALL
            SELECT 1 FROM server_issued_resources WHERE unique_uri = ?
            UNION ALL
            SELECT 1 FROM server_resource_templates WHERE route_uri = ?
        )
        "#,
    )
    .bind(external_uri)
    .bind(external_uri)
    .bind(external_uri)
    .fetch_one(connection)
    .await
    .context("Failed to check Resource Address Registry occupancy")?;
    Ok(occupied != 0)
}

pub(crate) async fn issue_resource_route(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    upstream_uri: &str,
) -> Result<String> {
    let mut transaction = crate::core::capability::naming::begin_naming_transaction(pool)
        .await
        .context("Failed to serialize issued resource registration")?;
    let persisted_name = sqlx::query_scalar::<_, String>("SELECT name FROM server_config WHERE id = ?")
        .bind(server_id)
        .fetch_optional(&mut *transaction)
        .await
        .context("Failed to load server for issued resource route")?
        .with_context(|| format!("Server '{server_id}' not found"))?;
    if persisted_name != server_name {
        bail!("Server namespace '{server_name}' does not match persisted server '{persisted_name}'");
    }
    if let Some(unique_uri) = sqlx::query_scalar::<_, String>(
        "SELECT unique_uri FROM server_resources WHERE server_id = ? AND resource_uri = ?",
    )
    .bind(server_id)
    .bind(upstream_uri)
    .fetch_optional(&mut *transaction)
    .await
    .context("Failed to find listed resource mapping")?
    {
        transaction
            .commit()
            .await
            .context("Failed to finish listed resource route lookup")?;
        return Ok(unique_uri);
    }

    let template_rows = sqlx::query_as::<_, (String, String)>(
        "SELECT uri_template, unique_name FROM server_resource_templates WHERE server_id = ?",
    )
    .bind(server_id)
    .fetch_all(&mut *transaction)
    .await
    .context("Failed to load Resource Templates for resource route projection")?;
    let mut template_matches = Vec::new();
    for (upstream_template, external_template) in template_rows {
        if let Some(external_uri) =
            project_upstream_resource_uri_through_template(&external_template, &upstream_template, upstream_uri)?
        {
            template_matches.push(external_uri);
        }
    }
    match template_matches.as_slice() {
        [external_uri] => {
            transaction
                .commit()
                .await
                .context("Failed to finish Resource Template route projection")?;
            return Ok(external_uri.clone());
        }
        [] => {}
        _ => bail!(
            "Upstream resource URI '{upstream_uri}' is ambiguous across registered Resource Templates for server '{server_id}'"
        ),
    }

    if let Some(unique_uri) = sqlx::query_scalar::<_, String>(
        "SELECT unique_uri FROM server_issued_resources WHERE server_id = ? AND resource_uri = ?",
    )
    .bind(server_id)
    .bind(upstream_uri)
    .fetch_optional(&mut *transaction)
    .await
    .context("Failed to find issued resource mapping")?
    {
        sqlx::query(
            "UPDATE server_issued_resources SET last_seen_at = CURRENT_TIMESTAMP WHERE server_id = ? AND resource_uri = ?",
        )
        .bind(server_id)
        .bind(upstream_uri)
        .execute(&mut *transaction)
        .await
        .context("Failed to update issued resource last-seen timestamp")?;
        transaction
            .commit()
            .await
            .context("Failed to finish issued resource route reuse")?;
        return Ok(unique_uri);
    }

    let candidates = resource_alias_candidates(ResourceAddressKind::Static, server_name, upstream_uri)?;
    let mut selected = None;
    for candidate in [candidates.preferred, candidates.expanded, candidates.digested] {
        if !external_uri_is_occupied(&mut transaction, &candidate).await? {
            selected = Some(candidate);
            break;
        }
    }
    let unique_uri = selected
        .with_context(|| format!("Cannot allocate a deterministic issued resource address for '{upstream_uri}'"))?;
    sqlx::query(
        r#"
        INSERT INTO server_issued_resources (id, server_id, server_name, resource_uri, unique_uri)
        VALUES (?, ?, ?, ?, ?)
        "#,
    )
    .bind(crate::generate_id!("sirs"))
    .bind(server_id)
    .bind(server_name)
    .bind(upstream_uri)
    .bind(&unique_uri)
    .execute(&mut *transaction)
    .await
    .context("Failed to persist issued resource route")?;
    transaction
        .commit()
        .await
        .context("Failed to commit issued resource route")?;
    Ok(unique_uri)
}

pub(crate) async fn remap_issued_resource_routes(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    server_id: &str,
    server_name: &str,
) -> Result<()> {
    let issued_rows = sqlx::query_as::<_, (String, String)>(
        "SELECT id, resource_uri FROM server_issued_resources WHERE server_id = ? ORDER BY resource_uri",
    )
    .bind(server_id)
    .fetch_all(&mut **tx)
    .await
    .context("Failed to load issued resource routes for namespace repair")?;
    if issued_rows.is_empty() {
        return Ok(());
    }

    let reserved = sqlx::query_scalar::<_, String>(
        r#"
        SELECT unique_uri FROM server_resources
        UNION ALL
        SELECT unique_uri FROM server_issued_resources WHERE server_id != ?
        UNION ALL
        SELECT route_uri FROM server_resource_templates WHERE route_uri IS NOT NULL
        "#,
    )
    .bind(server_id)
    .fetch_all(&mut **tx)
    .await
    .context("Failed to load occupied resource routes for namespace repair")?
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();
    let upstream_values = issued_rows
        .iter()
        .map(|(_, upstream_uri)| upstream_uri.clone())
        .collect::<Vec<_>>();
    let planned = super::resource_uri::plan_resource_addresses_with_reserved(
        ResourceAddressKind::Static,
        server_name,
        &upstream_values,
        &BTreeMap::new(),
        &reserved,
    )?;

    for (id, _) in &issued_rows {
        sqlx::query("UPDATE server_issued_resources SET unique_uri = ? WHERE id = ?")
            .bind(format!("\u{1f}mcpmate-issued:{id}"))
            .bind(id)
            .execute(&mut **tx)
            .await
            .context("Failed to stage issued route namespace repair")?;
    }
    for (id, upstream_uri) in issued_rows {
        sqlx::query(
            "UPDATE server_issued_resources SET server_name = ?, unique_uri = ?, last_seen_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(server_name)
        .bind(&planned[&upstream_uri])
        .bind(id)
        .execute(&mut **tx)
        .await
        .context("Failed to finalize issued route namespace repair")?;
    }
    Ok(())
}

pub(crate) async fn rewrite_read_resource_result(
    pool: &Pool<Sqlite>,
    request_route: &ResolvedResourceRoute,
    result: &mut rmcp::model::ReadResourceResult,
) -> Result<()> {
    for content in &mut result.contents {
        let Some(upstream_uri) = (match content {
            rmcp::model::ResourceContents::TextResourceContents { uri, .. }
            | rmcp::model::ResourceContents::BlobResourceContents { uri, .. } => Some(uri),
            _ => None,
        }) else {
            continue;
        };
        if *upstream_uri == request_route.upstream_uri {
            upstream_uri.clone_from(&request_route.external_uri);
        } else {
            *upstream_uri =
                issue_resource_route(pool, &request_route.server_id, &request_route.server_name, upstream_uri).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use sqlx::{
        Pool, Sqlite,
        sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    };
    use tokio::sync::Barrier;

    use super::*;

    async fn registry_pool() -> Pool<Sqlite> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory registry");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::initialization::run_initialization(&pool)
            .await
            .expect("initialize database");
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("enable foreign keys");
        sqlx::query("INSERT INTO server_config (id, name, server_type) VALUES ('server-a', 'everything', 'stdio')")
            .execute(&pool)
            .await
            .expect("insert server");
        pool
    }

    #[test]
    fn validates_canonical_grammar_without_requiring_a_registry_row() {
        assert!(validate_external_resource_uri("mcpmate://resources/docs/file/generated.md").is_ok());
        for invalid in [
            "file:///generated.md",
            "mcpmate://other/docs/file/generated.md",
            "mcpmate://resources/docs/file/generated.md#fragment",
            "mcpmate://resources/docs/file/%ZZ",
        ] {
            assert!(
                validate_external_resource_uri(invalid).is_err(),
                "invalid canonical grammar must be rejected: {invalid}"
            );
        }
    }

    #[tokio::test]
    async fn resolves_listed_resource_from_canonical_uri() {
        let pool = registry_pool().await;
        sqlx::query(
            "INSERT INTO server_resources (id, server_id, server_name, resource_uri, unique_uri) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("resource-a")
        .bind("server-a")
        .bind("everything")
        .bind("demo://resource/static/document/architecture.md")
        .bind("mcpmate://resources/everything/demo/static/document/architecture.md")
        .execute(&pool)
        .await
        .expect("insert listed resource");

        let route = resolve_resource_route(
            &pool,
            "mcpmate://resources/everything/demo/static/document/architecture.md",
        )
        .await
        .expect("resolve listed resource");

        assert_eq!(route.server_id, "server-a");
        assert_eq!(route.upstream_uri, "demo://resource/static/document/architecture.md");
        assert_eq!(route.source, ResourceRouteSource::Listed);
    }

    #[tokio::test]
    async fn resolves_a_registered_root_resource_uri() {
        let pool = registry_pool().await;
        sqlx::query(
            "INSERT INTO server_resources (id, server_id, server_name, resource_uri, unique_uri) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("resource-root")
        .bind("server-a")
        .bind("everything")
        .bind("file:///")
        .bind("mcpmate://resources/everything/file")
        .execute(&pool)
        .await
        .expect("insert root resource");

        let route = resolve_resource_route(&pool, "mcpmate://resources/everything/file")
            .await
            .expect("resolve root resource");

        assert_eq!(route.upstream_uri, "file:///");
        assert_eq!(route.source, ResourceRouteSource::Listed);
    }

    #[tokio::test]
    async fn resolves_path_template_arguments_from_the_registered_canonical_pattern() {
        let pool = registry_pool().await;
        sqlx::query(
            "INSERT INTO server_resource_templates (id, server_id, server_name, uri_template, unique_name, route_uri, name) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("template-a")
        .bind("server-a")
        .bind("everything")
        .bind("demo://resource/dynamic/{kind}/{resourceId}")
        .bind("mcpmate://resources/template/everything/demo/dynamic/{kind}/{resourceId}")
        .bind("mcpmate://resources/template/everything/demo/dynamic/{}/{}")
        .bind("Dynamic Resource")
        .execute(&pool)
        .await
        .expect("insert resource template");

        let route = resolve_resource_route(&pool, "mcpmate://resources/template/everything/demo/dynamic/text/42")
            .await
            .expect("resolve resource template");

        assert_eq!(route.server_id, "server-a");
        assert_eq!(route.upstream_uri, "demo://resource/dynamic/text/42");
        assert_eq!(
            route.source,
            ResourceRouteSource::Template {
                upstream_template: "demo://resource/dynamic/{kind}/{resourceId}".to_string(),
                arguments: std::collections::BTreeMap::from([
                    ("kind".to_string(), "text".to_string()),
                    ("resourceId".to_string(), "42".to_string()),
                ]),
            }
        );
    }

    #[tokio::test]
    async fn resolves_query_template_arguments_independently_of_query_order() {
        let pool = registry_pool().await;
        sqlx::query(
            "INSERT INTO server_resource_templates (id, server_id, server_name, uri_template, unique_name, route_uri, name) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("template-search")
        .bind("server-a")
        .bind("everything")
        .bind("search://items{?q,lang}")
        .bind("mcpmate://resources/template/everything/search/items{?q,lang}")
        .bind("mcpmate://resources/template/everything/search/items{?lang,q}")
        .bind("Search")
        .execute(&pool)
        .await
        .expect("insert query resource template");

        let route = resolve_resource_route(
            &pool,
            "mcpmate://resources/template/everything/search/items?lang=en&q=rust",
        )
        .await
        .expect("resolve query resource template");

        assert_eq!(route.upstream_uri, "search://items?lang=en&q=rust");
        assert_eq!(
            route.source,
            ResourceRouteSource::Template {
                upstream_template: "search://items{?q,lang}".to_string(),
                arguments: std::collections::BTreeMap::from([
                    ("lang".to_string(), "en".to_string()),
                    ("q".to_string(), "rust".to_string()),
                ]),
            }
        );
    }

    #[tokio::test]
    async fn resolves_rfc6570_path_operator_without_persisting_an_instance() {
        let pool = registry_pool().await;
        sqlx::query(
            "INSERT INTO server_resource_templates (id, server_id, server_name, uri_template, unique_name, route_uri, name) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("template-files")
        .bind("server-a")
        .bind("everything")
        .bind("file:///files{/segments*}{?revision}")
        .bind("mcpmate://resources/template/everything/file/files{/segments*}{?revision}")
        .bind("mcpmate://resources/template/everything/file/files{/segments*}{?revision}")
        .bind("Files")
        .execute(&pool)
        .await
        .expect("insert path operator resource template");

        let route = resolve_resource_route(
            &pool,
            "mcpmate://resources/template/everything/file/files/docs/guide.md?revision=2",
        )
        .await
        .expect("resolve path operator resource template");

        assert_eq!(route.upstream_uri, "file:///files/docs/guide.md?revision=2");
        let issued_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM server_issued_resources")
            .fetch_one(&pool)
            .await
            .expect("count issued resource routes");
        assert_eq!(issued_count, 0);
    }

    #[tokio::test]
    async fn issued_route_is_reused_touched_and_cascades_with_server() {
        let pool = registry_pool().await;
        let first = issue_resource_route(&pool, "server-a", "everything", "demo://resource/generated/report.md")
            .await
            .expect("issue resource route");
        let first_seen: String =
            sqlx::query_scalar("SELECT last_seen_at FROM server_issued_resources WHERE server_id = 'server-a'")
                .fetch_one(&pool)
                .await
                .expect("load first seen timestamp");

        tokio::time::sleep(Duration::from_millis(1100)).await;
        let second = issue_resource_route(&pool, "server-a", "everything", "demo://resource/generated/report.md")
            .await
            .expect("reuse resource route");
        let second_seen: String =
            sqlx::query_scalar("SELECT last_seen_at FROM server_issued_resources WHERE server_id = 'server-a'")
                .fetch_one(&pool)
                .await
                .expect("load updated seen timestamp");

        assert_eq!(first, second);
        assert!(second_seen > first_seen);
        let resolved = resolve_resource_route(&pool, &second)
            .await
            .expect("resolve issued resource route");
        assert_eq!(resolved.source, ResourceRouteSource::Issued);
        assert_eq!(resolved.upstream_uri, "demo://resource/generated/report.md");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_issued_resources")
                .fetch_one(&pool)
                .await
                .expect("count issued routes"),
            1
        );

        sqlx::query("DELETE FROM server_config WHERE id = 'server-a'")
            .execute(&pool)
            .await
            .expect("delete server");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_issued_resources")
                .fetch_one(&pool)
                .await
                .expect("count cascaded issued routes"),
            0
        );
    }

    #[tokio::test]
    async fn concurrent_issued_route_registration_is_idempotent() {
        let temp_dir = tempfile::TempDir::new().expect("create registry directory");
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(temp_dir.path().join("registry.db"))
                    .create_if_missing(true),
            )
            .await
            .expect("connect registry database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::initialization::run_initialization(&pool)
            .await
            .expect("initialize database");
        sqlx::query("INSERT INTO server_config (id, name, server_type) VALUES ('server-a', 'everything', 'stdio')")
            .execute(&pool)
            .await
            .expect("insert server");

        let barrier = Arc::new(Barrier::new(16));
        let registrations = (0..16)
            .map(|_| {
                let pool = pool.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    issue_resource_route(&pool, "server-a", "everything", "demo://resource/generated/report.md").await
                })
            })
            .collect::<Vec<_>>();

        let mut aliases = Vec::new();
        for registration in registrations {
            aliases.push(
                registration
                    .await
                    .expect("join issued route registration")
                    .expect("register issued route"),
            );
        }
        assert!(aliases.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_issued_resources")
                .fetch_one(&pool)
                .await
                .expect("count issued routes"),
            1
        );
    }

    #[tokio::test]
    async fn rejects_unregistered_raw_legacy_unknown_and_invalid_template_routes() {
        let pool = registry_pool().await;
        sqlx::query(
            "INSERT INTO server_resource_templates (id, server_id, server_name, uri_template, unique_name, route_uri, name) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("template-a")
        .bind("server-a")
        .bind("everything")
        .bind("demo://resource/dynamic/text/{resourceId}")
        .bind("mcpmate://resources/template/everything/demo/dynamic/text/{resourceId}")
        .bind("mcpmate://resources/template/everything/demo/dynamic/text/{}")
        .bind("Dynamic Text Resource")
        .execute(&pool)
        .await
        .expect("insert resource template");

        for invalid in [
            "demo://resource/static/document/architecture.md",
            "mcpmate://resources/everything/ZGVtbzovL3Jlc291cmNlL3N0YXRpYw",
            "mcpmate://resources/unknown/demo/static/document/architecture.md",
            "mcpmate://resources/everything/demo/missing",
            "mcpmate://resources/template/everything/demo/dynamic/text",
            "mcpmate://resources/template/everything/demo/dynamic/text/1/2",
            "mcpmate://resources/template/everything/demo/dynamic/text/%ZZ",
        ] {
            assert!(
                resolve_resource_route(&pool, invalid).await.is_err(),
                "route must fail closed: {invalid}"
            );
        }
    }

    #[tokio::test]
    async fn read_result_reuses_request_identity_and_issues_unlisted_typed_contents() {
        let pool = registry_pool().await;
        sqlx::query(
            "INSERT INTO server_resources (id, server_id, server_name, resource_uri, unique_uri) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("resource-a")
        .bind("server-a")
        .bind("everything")
        .bind("demo://resource/static/document/architecture.md")
        .bind("mcpmate://resources/everything/demo/static/document/architecture.md")
        .execute(&pool)
        .await
        .expect("insert listed resource");
        let route = resolve_resource_route(
            &pool,
            "mcpmate://resources/everything/demo/static/document/architecture.md",
        )
        .await
        .expect("resolve request route");
        let mut result = rmcp::model::ReadResourceResult::new(vec![
            rmcp::model::ResourceContents::text("architecture", "demo://resource/static/document/architecture.md"),
            rmcp::model::ResourceContents::text("generated", "demo://resource/generated/related.md"),
        ]);

        rewrite_read_resource_result(&pool, &route, &mut result)
            .await
            .expect("rewrite read result");

        let rewritten_uris = result
            .contents
            .iter()
            .map(|content| match content {
                rmcp::model::ResourceContents::TextResourceContents { uri, .. }
                | rmcp::model::ResourceContents::BlobResourceContents { uri, .. } => uri.as_str(),
                _ => panic!("expected known resource contents"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rewritten_uris,
            [
                "mcpmate://resources/everything/demo/static/document/architecture.md",
                "mcpmate://resources/everything/demo/generated/related.md",
            ]
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_issued_resources")
                .fetch_one(&pool)
                .await
                .expect("count issued routes"),
            1
        );
    }
}
