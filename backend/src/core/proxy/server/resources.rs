use super::*;
#[cfg(test)]
use crate::core::capability::resource_registry::resolve_resource_route;
use crate::core::capability::resource_registry::{
    ResolvedResourceRoute, ResourceRouteSource, rewrite_read_resource_result,
};
use rmcp::ErrorData as McpError;
use rmcp::model::{
    ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResult,
};
use rmcp::service::RequestContext;
use std::collections::HashSet;

#[derive(Debug)]
pub(super) struct ResolvedExternalResourceTarget {
    pub(super) server_id: String,
    pub(super) route: ResolvedResourceRoute,
}

impl ResolvedExternalResourceTarget {
    pub(super) fn upstream_uri(&self) -> &str {
        &self.route.upstream_uri
    }

    pub(super) fn canonical_uri(&self) -> &str {
        &self.route.external_uri
    }
}

#[cfg(test)]
pub(super) async fn resolve_external_resource_target(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    external_uri: &str,
) -> anyhow::Result<ResolvedExternalResourceTarget> {
    let route = resolve_resource_route(pool, external_uri).await?;
    let server_id = route.server_id.clone();
    Ok(ResolvedExternalResourceTarget { server_id, route })
}

pub(super) async fn resolve_active_external_resource_target(
    server: &ProxyServer,
    surface_route: crate::core::capability::surface_read::ActiveResourceRoute,
) -> Result<ResolvedExternalResourceTarget, McpError> {
    let Some(db) = &server.database else {
        return Err(McpError::internal_error(
            "Resource routing requires database-backed registry metadata".to_string(),
            None,
        ));
    };
    let server_name: String = sqlx::query_scalar("SELECT name FROM server_config WHERE id = ?")
        .bind(&surface_route.source_server_id)
        .fetch_one(&db.pool)
        .await
        .map_err(|error| {
            McpError::internal_error(format!("Failed to resolve pinned Resource source: {error}"), None)
        })?;
    let source = match (surface_route.upstream_template, surface_route.template_arguments) {
        (Some(upstream_template), Some(arguments)) => ResourceRouteSource::Template {
            upstream_template,
            arguments,
        },
        (None, None) => ResourceRouteSource::Listed,
        _ => {
            return Err(McpError::internal_error(
                "Active Surface Resource route has inconsistent template metadata".to_string(),
                None,
            ));
        }
    };
    Ok(ResolvedExternalResourceTarget {
        server_id: surface_route.source_server_id.clone(),
        route: ResolvedResourceRoute {
            server_id: surface_route.source_server_id,
            server_name,
            external_uri: surface_route.external_uri,
            upstream_uri: surface_route.upstream_uri,
            source,
        },
    })
}

pub(super) async fn resolve_authorized_external_resource_target(
    server: &ProxyServer,
    client: &crate::core::proxy::server::common::ClientContext,
    external_uri: &str,
) -> Result<ResolvedExternalResourceTarget, McpError> {
    let surface_route = server.resolve_active_resource_route(client, external_uri).await?.ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "Resource is not in the active Surface publication: invalid surface value for active surface resource route: {}/{}",
                client.client_id, external_uri
            ),
            None,
        )
    })?;
    resolve_active_external_resource_target(server, surface_route).await
}

async fn resolve_broker_external_resource_target(
    server: &ProxyServer,
    client: &crate::core::proxy::server::common::ClientContext,
    external_uri: &str,
) -> Result<ResolvedExternalResourceTarget, McpError> {
    let database = server.database.as_ref().ok_or_else(|| {
        McpError::internal_error(
            "Resource routing requires database-backed registry metadata".to_string(),
            None,
        )
    })?;
    let broker = crate::mcper::builtin::BrokerService::new(database.clone(), server.connection_pool.clone());
    let route = match broker
        .resolve_current_broker_resource_route_for_client(client, external_uri)
        .await
    {
        Ok(Some(route)) => route,
        Ok(None) => {
            return Err(McpError::invalid_params(
                format!("Resource '{external_uri}' is not available in the current MCP surface"),
                None,
            ));
        }
        Err(error) if crate::mcper::builtin::is_catalog_authority_error(&error) => {
            return Err(McpError::internal_error(
                "The current capability directory could not be completed".to_string(),
                Some(serde_json::json!({
                    "error_code": "catalog_incomplete",
                    "retry_eligible": true,
                })),
            ));
        }
        Err(error) if crate::core::capability::resource_registry::is_invalid_resource_route_error(&error) => {
            return Err(McpError::invalid_params(error.to_string(), None));
        }
        Err(error) => return Err(McpError::internal_error(error.to_string(), None)),
    };
    let server_id = route.server_id.clone();
    Ok(ResolvedExternalResourceTarget { server_id, route })
}

fn map_resource_read_error(error: anyhow::Error) -> McpError {
    for source in error.chain() {
        if let Some(rmcp::service::ServiceError::McpError(upstream)) =
            source.downcast_ref::<rmcp::service::ServiceError>()
            && upstream.code == rmcp::model::ErrorCode::RESOURCE_NOT_FOUND
        {
            return upstream.clone();
        }
    }
    McpError::internal_error(error.to_string(), None)
}

pub(super) async fn list_resources(
    server: &ProxyServer,
    _request: Option<PaginatedRequestParams>,
    _context: RequestContext<rmcp::RoleServer>,
) -> Result<ListResourcesResult, McpError> {
    let client = server.resolve_bound_client_context(&_context).await?;
    let surface = server.load_active_surface(&client).await?;
    let mut resources = surface.resources();
    if matches!(client.config_mode.as_deref(), Some("unify")) {
        resources.insert(0, super::resource_guide::listed_resource());
        let mut seen_uris = HashSet::new();
        resources.retain(|resource| seen_uris.insert(resource.uri.clone()));
    }
    let page = server.paginator.paginate_resources(&_request, resources)?;

    tracing::info!(
        total = page.items.len(),
        has_next = page.next_cursor.is_some(),
        consumer_id = %surface.consumer_id,
        publication_id = %surface.publication_id,
        generation = surface.generation,
        "Proxy listed resources from active Surface publication"
    );

    Ok(ListResourcesResult {
        resources: page.items,
        next_cursor: page.next_cursor,
        ..Default::default()
    })
}

pub(super) async fn list_resource_templates(
    server: &ProxyServer,
    _request: Option<PaginatedRequestParams>,
    _context: RequestContext<rmcp::RoleServer>,
) -> Result<ListResourceTemplatesResult, McpError> {
    let client = server.resolve_bound_client_context(&_context).await?;
    let surface = server.load_active_surface(&client).await?;
    let mut resource_templates = surface.resource_templates();
    if matches!(client.config_mode.as_deref(), Some("unify")) {
        resource_templates.insert(0, super::resource_guide::listed_template());
        let mut seen_uris = HashSet::new();
        resource_templates.retain(|template| seen_uris.insert(template.uri_template.clone()));
    }
    let page = server
        .paginator
        .paginate_resource_templates(&_request, resource_templates)?;

    tracing::info!(
        total = page.items.len(),
        has_next = page.next_cursor.is_some(),
        consumer_id = %surface.consumer_id,
        publication_id = %surface.publication_id,
        generation = surface.generation,
        "Proxy listed Resource Templates from active Surface publication"
    );

    Ok(ListResourceTemplatesResult {
        resource_templates: page.items,
        next_cursor: page.next_cursor,
        ..Default::default()
    })
}

pub(super) async fn read_resource(
    server: &ProxyServer,
    request: ReadResourceRequestParams,
    _context: RequestContext<rmcp::RoleServer>,
) -> Result<ReadResourceResult, McpError> {
    let client = server.resolve_bound_client_context(&_context).await?;
    if matches!(client.config_mode.as_deref(), Some("unify"))
        && let Some(result) = super::resource_guide::try_read(&request.uri)
    {
        return result;
    }
    tracing::debug!("Reading resource: {}", request.uri);

    let target = match server.resolve_active_resource_route(&client, &request.uri).await? {
        Some(surface_route) => resolve_active_external_resource_target(server, surface_route).await?,
        None if matches!(client.config_mode.as_deref(), Some("unify")) => {
            resolve_broker_external_resource_target(server, &client, &request.uri).await?
        }
        None => {
            return Err(McpError::invalid_params(
                format!(
                    "Resource is not in the active Surface publication: invalid surface value for active surface resource route: {}/{}",
                    client.client_id, request.uri
                ),
                None,
            ));
        }
    };
    let server_filter = target.server_id.clone();
    let lookup_uri = target.upstream_uri().to_string();

    let connection_selection = client.connection_selection(server_filter.clone());

    match crate::core::capability::facade::read_routed_resource(
        &server.connection_pool,
        &server_filter,
        &lookup_uri,
        connection_selection.as_ref(),
    )
    .await
    {
        Ok(mut result) => {
            let db = server.database.as_ref().ok_or_else(|| {
                McpError::internal_error(
                    "Resource response projection requires registry metadata".to_string(),
                    None,
                )
            })?;
            rewrite_read_resource_result(&db.pool, &target.route, &mut result)
                .await
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            Ok(result)
        }
        Err(e) => {
            tracing::error!("Failed to read resource '{}': {}", request.uri, e);
            if let Some(database) = server.database.as_ref() {
                crate::core::capability::runtime::record_capability_usage_evidence(
                    database,
                    &server_filter,
                    mcpmate_capability_store::CapabilityKind::Resources,
                    None,
                    &e,
                )
                .await;
            }
            Err(map_resource_read_error(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn route_pool() -> sqlx::Pool<sqlx::Sqlite> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect route database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        crate::config::profile::init::initialize_profile_tables(&pool)
            .await
            .expect("initialize profile tables");
        sqlx::query("INSERT INTO server_config (id, name, server_type) VALUES ('server-a', 'everything', 'stdio')")
            .execute(&pool)
            .await
            .expect("insert server");
        pool
    }

    #[tokio::test]
    async fn canonical_static_resource_resolves_from_registry() {
        let pool = route_pool().await;
        let canonical = crate::config::server::capabilities::upsert_shadow_resource(
            &pool,
            "server-a",
            "everything",
            "demo://resource/static/document/architecture.md",
            None,
            None,
            None,
        )
        .await
        .expect("insert listed resource");

        let target = resolve_external_resource_target(&pool, &canonical)
            .await
            .expect("resolve resource target");

        assert_eq!(target.server_id, "server-a");
        assert_eq!(target.upstream_uri(), "demo://resource/static/document/architecture.md");
        assert_eq!(target.canonical_uri(), canonical);
    }

    #[tokio::test]
    async fn template_derived_resource_resolves_from_registry_without_static_row() {
        let pool = route_pool().await;
        let template = crate::config::server::capabilities::upsert_shadow_resource_template(
            &pool,
            "server-a",
            "everything",
            "demo://resource/dynamic/text/{resourceId}",
            Some("Dynamic Text Resource"),
            None,
        )
        .await
        .expect("insert template route");
        let canonical = template.replace("{resourceId}", "42");

        let target = resolve_external_resource_target(&pool, &canonical)
            .await
            .expect("resolve template target");

        assert_eq!(target.server_id, "server-a");
        assert_eq!(target.upstream_uri(), "demo://resource/dynamic/text/42");
    }

    #[tokio::test]
    async fn raw_or_unknown_external_resource_routes_fail_closed() {
        let pool = route_pool().await;
        assert!(
            resolve_external_resource_target(&pool, "file:///guide.md")
                .await
                .is_err()
        );
        assert!(
            resolve_external_resource_target(&pool, "mcpmate://resources/everything/ZGVtbzovL3Jlc291cmNlL3N0YXRpYw",)
                .await
                .is_err()
        );

        assert!(
            resolve_external_resource_target(&pool, "mcpmate://resources/everything/demo/static/document/missing.md",)
                .await
                .is_err()
        );
    }

    #[test]
    fn resource_not_found_error_code_survives_proxy_mapping() {
        let upstream = rmcp::ErrorData::resource_not_found("missing", None);
        let error = anyhow::Error::new(rmcp::service::ServiceError::McpError(upstream.clone()))
            .context("Failed to read resource from upstream server");

        assert_eq!(map_resource_read_error(error), upstream);
    }
}
