//! Server capability handling utilities
//!
//! This module provides comprehensive functionality for managing MCP Server capabilities including:
//! - Database mapping and persistence for tools, prompts, resources, and resource templates
//! - Data enrichment with unique identifiers and database relationships
//! - JSON formatting for API responses
//! - Capability extraction from live server instances
//! - Refresh mechanisms for cache invalidation and temporary instances
//!
//! All capability types (Tools, Prompts, Resources, ResourceTemplates) follow unified patterns
//! for consistent handling across the API.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use rmcp::model::Icon;
use serde_json::{Map, Value};
use sqlx::{Pool, Sqlite};
use std::{collections::HashMap, sync::Arc};

use crate::api::handlers::ApiError;
use crate::api::handlers::server::common::{
    InspectQuery, RefreshStrategy, ServerIdentification, get_database_from_state, get_server_info_for_inspect,
};
use crate::api::models::cache::{
    CacheCatalogStats, CacheDetailsData, CacheDetailsReq, CacheDetailsResp, CacheKeyItem, CacheMemoryStats,
    CacheMetricsStats, CacheResetData, CacheResetResp, CacheStorageStats, CacheViewType,
};
use crate::api::models::server::{
    ServerCapabilityAuthenticationCode, ServerCapabilityAuthenticationFailure, ServerCapabilityDetailData,
    ServerCapabilityDetailReq, ServerCapabilityDetailResp, ServerCapabilityListsData, ServerCapabilityListsResp,
    ServerCapabilityMeta, ServerCapabilityRefreshData, ServerCapabilityRefreshReq, ServerCapabilityRefreshResp,
    ServerCapabilityReq, ServerPromptsData, ServerResourceTemplatesData, ServerResourcesData, ServerToolsData,
};
use crate::api::routes::AppState;
use crate::audit::{AuditAction, AuditStatus};

#[derive(Debug, Clone, Copy)]
pub enum CapabilityType {
    Tools,
    Prompts,
    Resources,
    ResourceTemplates,
}

pub type CapabilityIdentityMapping = HashMap<String, (String, String, String)>;

async fn resolve_capability_read(
    state: &Arc<AppState>,
    request: &ServerCapabilityReq,
) -> Result<(Arc<crate::config::database::Database>, String, Option<RefreshStrategy>), ApiError> {
    let query = InspectQuery {
        refresh: request.refresh.as_ref().map(|refresh| (*refresh).into()),
        format: None,
        include_meta: None,
        timeout: None,
    };
    let (db, server_info, params) = get_server_info_for_inspect(state, &request.id, &query).await?;
    Ok((db, server_info.server_id, params.refresh))
}

fn runtime_kind(capability_type: CapabilityType) -> crate::core::capability::CapabilityType {
    match capability_type {
        CapabilityType::Tools => crate::core::capability::CapabilityType::Tools,
        CapabilityType::Prompts => crate::core::capability::CapabilityType::Prompts,
        CapabilityType::Resources => crate::core::capability::CapabilityType::Resources,
        CapabilityType::ResourceTemplates => crate::core::capability::CapabilityType::ResourceTemplates,
    }
}

fn runtime_list_refresh(refresh: Option<RefreshStrategy>) -> Option<crate::core::capability::runtime::RefreshStrategy> {
    match refresh {
        Some(RefreshStrategy::Force) => Some(crate::core::capability::runtime::RefreshStrategy::Force),
        _ => Some(crate::core::capability::runtime::RefreshStrategy::CacheFirst),
    }
}

fn list_ctx(
    server_id: &str,
    refresh: Option<RefreshStrategy>,
    capability_type: CapabilityType,
) -> crate::core::capability::runtime::ListCtx {
    crate::core::capability::runtime::ListCtx {
        capability: runtime_kind(capability_type),
        server_id: server_id.to_string(),
        refresh: runtime_list_refresh(refresh),
        operation_timeout: crate::core::transport::timeout_policy::DEFAULT_CAPABILITY_OPERATION_TIMEOUT,
        validation_session: None,
        runtime_identity: None,
        connection_selection: None,
        visibility_snapshot: None,
        name_domain: crate::core::capability::runtime::NameDomain::External,
    }
}

async fn read_capability_list_result(
    state: &Arc<AppState>,
    server_id: &str,
    refresh: Option<RefreshStrategy>,
    capability_type: CapabilityType,
) -> Result<crate::core::capability::runtime::ListResult, ApiError> {
    let database = state
        .database
        .as_ref()
        .ok_or_else(|| ApiError::ServiceUnavailable("Database is not initialized".to_string()))?;
    let service = crate::core::capability::read_service::CapabilityReadService::from_runtime(
        database.clone(),
        state.connection_pool.clone(),
    );
    service
        .list(&list_ctx(server_id, refresh, capability_type))
        .await
        .map_err(|error| {
            tracing::error!(
                server_id = %server_id,
                capability = ?capability_type,
                error = %error,
                "Failed to read server capability list"
            );
            crate::core::capability::service::map_capability_read_error(&error)
        })
}

pub(crate) async fn list_server_capability(
    state: &Arc<AppState>,
    request: &ServerCapabilityReq,
    capability_type: CapabilityType,
) -> Result<ServerToolsData, ApiError> {
    let (db, server_id, refresh) = resolve_capability_read(state, request).await?;
    let list_result = read_capability_list_result(state, &server_id, refresh, capability_type).await?;
    project_list_payload(&db, &server_id, capability_type, list_result, refresh).await
}

/// Refresh all capability kinds for one server in a single catalog observation.
pub async fn refresh_server_capabilities(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ServerCapabilityRefreshReq>,
) -> Result<Json<ServerCapabilityRefreshResp>, ApiError> {
    let database = get_database_from_state(&state)?;
    let server = crate::config::server::get_server_by_id(&database.pool, &request.id)
        .await
        .map_err(crate::api::handlers::common::errors::map_anyhow_error)?
        .ok_or_else(|| ApiError::NotFound(format!("Server '{}' not found", request.id)))?;
    let previous_revision = database
        .load_capability_snapshot(&request.id)
        .await
        .map_err(crate::api::handlers::common::errors::map_anyhow_error)?
        .0
        .map(|snapshot| snapshot.revision);
    let service = crate::core::capability::read_service::CapabilityReadService::from_runtime(
        database.clone(),
        state.connection_pool.clone(),
    );
    let lists = service
        .list_all_kinds(
            &request.id,
            Some(crate::core::capability::runtime::RefreshStrategy::Force),
        )
        .await
        .map_err(|error| crate::core::capability::service::map_capability_read_error(&error))?;
    if let Some(error) = lists.into_first_error() {
        return Err(crate::core::capability::service::map_capability_read_error(&error));
    }
    let snapshot = database
        .load_capability_snapshot(&request.id)
        .await
        .map_err(crate::api::handlers::common::errors::map_anyhow_error)?
        .0
        .ok_or_else(|| {
            ApiError::ServiceUnavailable(format!(
                "Capability refresh completed without a catalog snapshot for server '{}'",
                server.name
            ))
        })?;

    Ok(Json(ServerCapabilityRefreshResp::success(
        ServerCapabilityRefreshData {
            server_id: request.id,
            catalog_revision: snapshot.revision,
            catalog_changed: previous_revision != Some(snapshot.revision),
        },
    )))
}

/// List all capability kinds for one server using a single discovery pass.
pub async fn server_capability_lists(
    State(state): State<Arc<AppState>>,
    Query(request): Query<ServerCapabilityReq>,
) -> Result<Json<ServerCapabilityListsResp>, ApiError> {
    let (db, server_id, refresh) = resolve_capability_read(&state, &request).await?;
    let service = crate::core::capability::read_service::CapabilityReadService::from_runtime(
        db.clone(),
        state.connection_pool.clone(),
    );
    let lists = service
        .list_all_kinds(&server_id, runtime_list_refresh(refresh))
        .await
        .map_err(|error| {
            tracing::error!(
                server_id = %server_id,
                error = %error,
                "Failed to list all capability kinds"
            );
            crate::core::capability::service::map_capability_read_error(&error)
        })?;
    let authentication = [
        lists.tools.as_ref().err(),
        lists.resources.as_ref().err(),
        lists.prompts.as_ref().err(),
        lists.resource_templates.as_ref().err(),
    ]
    .into_iter()
    .flatten()
    .find_map(capability_authentication_failure);

    let tools = project_list_outcome(&db, &server_id, CapabilityType::Tools, lists.tools, refresh).await?;
    let resources_payload =
        project_list_outcome(&db, &server_id, CapabilityType::Resources, lists.resources, refresh).await?;
    let prompts_payload =
        project_list_outcome(&db, &server_id, CapabilityType::Prompts, lists.prompts, refresh).await?;
    let resource_templates_payload = project_list_outcome(
        &db,
        &server_id,
        CapabilityType::ResourceTemplates,
        lists.resource_templates,
        refresh,
    )
    .await?;

    Ok(Json(ServerCapabilityListsResp::success(ServerCapabilityListsData {
        authentication,
        tools,
        resources: ServerResourcesData {
            items: resources_payload.items,
            state: resources_payload.state,
            degraded_reason: resources_payload.degraded_reason,
            meta: resources_payload.meta,
        },
        prompts: ServerPromptsData {
            items: prompts_payload.items,
            state: prompts_payload.state,
            degraded_reason: prompts_payload.degraded_reason,
            meta: prompts_payload.meta,
        },
        resource_templates: ServerResourceTemplatesData {
            items: resource_templates_payload.items,
            state: resource_templates_payload.state,
            degraded_reason: resource_templates_payload.degraded_reason,
            meta: resource_templates_payload.meta,
        },
    })))
}

fn capability_authentication_failure(
    error: &crate::core::capability::read_service::CapabilityReadError
) -> Option<ServerCapabilityAuthenticationFailure> {
    use crate::core::capability::connection_provider::CapabilityAuthenticationFailureCode;

    let (code, reason) = error.authentication_failure()?;
    let code = match code {
        CapabilityAuthenticationFailureCode::AuthRequired => ServerCapabilityAuthenticationCode::AuthRequired,
        CapabilityAuthenticationFailureCode::Unauthorized => ServerCapabilityAuthenticationCode::Unauthorized,
        CapabilityAuthenticationFailureCode::Forbidden => ServerCapabilityAuthenticationCode::Forbidden,
        CapabilityAuthenticationFailureCode::InsufficientScope => ServerCapabilityAuthenticationCode::InsufficientScope,
    };
    Some(ServerCapabilityAuthenticationFailure {
        code,
        reason: reason.to_string(),
    })
}

async fn project_list_outcome(
    db: &Arc<crate::config::database::Database>,
    server_id: &str,
    capability_type: CapabilityType,
    outcome: Result<
        crate::core::capability::runtime::ListResult,
        crate::core::capability::read_service::CapabilityReadError,
    >,
    refresh: Option<RefreshStrategy>,
) -> Result<ServerToolsData, ApiError> {
    match outcome {
        Ok(result) => project_list_payload(db, server_id, capability_type, result, refresh).await,
        Err(error) => Ok(ServerToolsData {
            items: Vec::new(),
            state: "failed".to_string(),
            degraded_reason: Some(error.to_string()),
            meta: ServerCapabilityMeta {
                cache_hit: false,
                strategy: capability_list_strategy(refresh),
                source: "catalog_failure".to_string(),
            },
        }),
    }
}

async fn project_list_payload(
    db: &Arc<crate::config::database::Database>,
    server_id: &str,
    capability_type: CapabilityType,
    list_result: crate::core::capability::runtime::ListResult,
    refresh: Option<RefreshStrategy>,
) -> Result<ServerToolsData, ApiError> {
    let crate::core::capability::runtime::ListResult { items, meta } = list_result;
    let json_items = capability_items_to_json(capability_type, server_id, items)?;
    let enriched = enrich_capability_items(capability_type, &db.pool, server_id, json_items)
        .await
        .map_err(|error| {
            tracing::error!(
                server_id = %server_id,
                capability = ?capability_type,
                error = %error,
                "Capability naming projection failed"
            );
            ApiError::InternalError(format!("Capability naming projection failed: {error}"))
        })?;
    Ok(build_capability_list_data(enriched, &meta, refresh))
}

fn capability_list_state(source: &str) -> String {
    if source.starts_with("capability-") {
        "unsupported".to_string()
    } else {
        "ok".to_string()
    }
}

fn capability_list_strategy(refresh: Option<RefreshStrategy>) -> String {
    serde_json::to_value(refresh.unwrap_or_default())
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "cacheFirst".to_string())
}

fn build_capability_list_data(
    items: Vec<serde_json::Value>,
    meta: &crate::core::capability::runtime::Meta,
    refresh: Option<RefreshStrategy>,
) -> ServerToolsData {
    ServerToolsData {
        items,
        state: capability_list_state(&meta.source),
        degraded_reason: None,
        meta: ServerCapabilityMeta {
            cache_hit: meta.cache_hit,
            strategy: capability_list_strategy(refresh),
            source: meta.source.clone(),
        },
    }
}

fn capability_items_to_json(
    capability_type: CapabilityType,
    server_id: &str,
    items: crate::core::capability::runtime::CapabilityItems,
) -> Result<Vec<serde_json::Value>, ApiError> {
    let values = match items {
        crate::core::capability::runtime::CapabilityItems::Tools(items) => {
            if !matches!(capability_type, CapabilityType::Tools) {
                return Err(ApiError::InternalError(
                    "Capability read service returned mismatched tool items".to_string(),
                ));
            }
            items
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()
        }
        crate::core::capability::runtime::CapabilityItems::Resources(items) => {
            if !matches!(capability_type, CapabilityType::Resources) {
                return Err(ApiError::InternalError(
                    "Capability read service returned mismatched resource items".to_string(),
                ));
            }
            items
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()
        }
        crate::core::capability::runtime::CapabilityItems::Prompts(items) => {
            if !matches!(capability_type, CapabilityType::Prompts) {
                return Err(ApiError::InternalError(
                    "Capability read service returned mismatched prompt items".to_string(),
                ));
            }
            items
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()
        }
        crate::core::capability::runtime::CapabilityItems::ResourceTemplates(items) => {
            if !matches!(capability_type, CapabilityType::ResourceTemplates) {
                return Err(ApiError::InternalError(
                    "Capability read service returned mismatched resource template items".to_string(),
                ));
            }
            items
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()
        }
    }
    .map_err(|error| {
        tracing::error!(server_id = %server_id, error = %error, "Failed to serialize capability items");
        ApiError::InternalError(format!("Failed to serialize capability items: {error}"))
    })?;
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::{
        CapabilityIdentityMapping, CapabilityType, ExtractedCapability, capability_authentication_failure,
        enrich_prompt_item, enrich_resource_item, enrich_resource_template_item, enrich_tool_item,
        persist_extracted_inventory, prompt_json, resource_json, resource_template_json,
        resource_template_json_from_cached, tool_json,
    };
    use crate::{
        api::{handlers::server::common::ServerIdentification, routes::AppState},
        config::database::Database,
        core::{
            capability::index::{CachedPromptInfo, CachedResourceInfo, CachedResourceTemplateInfo, CachedToolInfo},
            models::Config,
            pool::UpstreamConnectionPool,
            profile::ConfigApplicationStateManager,
        },
        inspector::{calls::InspectorCallRegistry, sessions::InspectorSessionManager},
        system::metrics::MetricsCollector,
    };
    use chrono::Utc;
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
    use tokio::sync::{Mutex, RwLock};

    fn mapping(
        upstream: &str,
        external: &str,
    ) -> CapabilityIdentityMapping {
        HashMap::from([(
            upstream.to_string(),
            (
                "capability-id".to_string(),
                external.to_string(),
                "capability-ref-id".to_string(),
            ),
        )])
    }

    #[test]
    fn cached_detail_json_uses_standard_mcp_wire_fields() {
        let tool = tool_json(
            "everything_get-tiny-image",
            Some("Returns a tiny image".to_string()),
            json!({ "type": "object" }),
            Some(json!({ "type": "object" })),
            None,
        );
        assert_eq!(tool["name"], "everything_get-tiny-image");
        assert_eq!(tool["inputSchema"], json!({ "type": "object" }));
        assert_eq!(tool["outputSchema"], json!({ "type": "object" }));
        assert!(tool.get("input_schema").is_none());
        assert!(tool.get("output_schema").is_none());
        assert!(tool.get("unique_name").is_none());
        assert!(tool.get("id").is_none());

        let resource = resource_json(
            "mcpmate://resources/everything/demo/static/document/architecture.md",
            Some("architecture.md".to_string()),
            Some("Architecture document".to_string()),
            Some("text/markdown".to_string()),
            None,
        );
        assert_eq!(resource["mimeType"], "text/markdown");
        assert!(resource.get("mime_type").is_none());
        assert!(resource.get("unique_uri").is_none());
        assert!(resource.get("id").is_none());

        let template = resource_template_json(
            "mcpmate://resources/template/everything/demo/dynamic/text/{resourceId}",
            Some("Dynamic Text Resource".to_string()),
            Some("Dynamic text".to_string()),
            Some("text/plain".to_string()),
        );
        assert_eq!(
            template["uriTemplate"],
            "mcpmate://resources/template/everything/demo/dynamic/text/{resourceId}"
        );
        assert_eq!(template["mimeType"], "text/plain");
        assert!(template.get("uri_template").is_none());
        assert!(template.get("unique_uri_template").is_none());
        assert!(template.get("id").is_none());

        let prompt = prompt_json(
            "everything_args-prompt",
            Some("Prompt with arguments".to_string()),
            vec![crate::core::capability::index::PromptArgument {
                name: "message".to_string(),
                description: Some("Message".to_string()),
                required: true,
            }],
            None,
        );
        assert_eq!(prompt["name"], "everything_args-prompt");
        assert!(prompt.get("unique_name").is_none());
        assert!(prompt.get("id").is_none());
    }

    #[test]
    fn capability_batch_exposes_insufficient_scope_as_structured_authentication() {
        use crate::core::capability::connection_provider::{
            CapabilityAuthenticationFailureCode, CapabilityOwnerError, OwnerSource,
        };
        use crate::core::capability::read_service::CapabilityReadError;

        let error = CapabilityReadError::CleanupFailed {
            server_id: "server-a".to_string(),
            server_name: "auth_server".to_string(),
            operation: "management catalog warm",
            instance_id: "instance-a".to_string(),
            connection_generation: None,
            owner_source: OwnerSource::Fresh,
            error: Box::new(CapabilityOwnerError::Authentication {
                code: CapabilityAuthenticationFailureCode::InsufficientScope,
                reason: "upstream requires an additional scope".to_string(),
            }),
        };

        let failure = capability_authentication_failure(&error).expect("authentication remains structured");

        assert_eq!(
            failure.code,
            crate::api::models::server::ServerCapabilityAuthenticationCode::InsufficientScope
        );
        assert!(failure.reason.contains("additional scope"));
    }

    #[test]
    fn cached_detail_projection_uses_external_identifiers() {
        let cached_at = Utc::now();
        let tool = super::tool_json_from_cached(
            &CachedToolInfo {
                name: "get-tiny-image".to_string(),
                description: None,
                input_schema_json: r#"{"type":"object"}"#.to_string(),
                output_schema_json: None,
                unique_name: None,
                icons: None,
                enabled: true,
                cached_at,
            },
            "everything_get-tiny-image",
        );
        let prompt = super::prompt_json_from_cached(
            CachedPromptInfo {
                name: "args-prompt".to_string(),
                description: None,
                arguments: Vec::new(),
                icons: None,
                enabled: true,
                cached_at,
            },
            "everything_args-prompt",
        );
        let resource = super::resource_json_from_cached(
            CachedResourceInfo {
                uri: "demo://resource/static/document/architecture.md".to_string(),
                name: Some("architecture.md".to_string()),
                description: None,
                mime_type: Some("text/markdown".to_string()),
                icons: None,
                enabled: true,
                cached_at,
            },
            "mcpmate://resources/everything/demo/static/document/architecture.md",
        );
        let template = resource_template_json_from_cached(
            CachedResourceTemplateInfo {
                uri_template: "demo://resource/dynamic/text/{resourceId}".to_string(),
                name: Some("Dynamic Text Resource".to_string()),
                description: None,
                mime_type: Some("text/plain".to_string()),
                enabled: true,
                cached_at,
            },
            "mcpmate://resources/template/everything/demo/dynamic/text/{resourceId}",
        );

        assert_eq!(tool["name"], "everything_get-tiny-image");
        assert_eq!(prompt["name"], "everything_args-prompt");
        assert_eq!(
            resource["uri"],
            "mcpmate://resources/everything/demo/static/document/architecture.md"
        );
        assert_eq!(
            template["uriTemplate"],
            "mcpmate://resources/template/everything/demo/dynamic/text/{resourceId}"
        );
    }

    #[test]
    fn management_tool_projection_uses_external_name_and_keeps_upstream_metadata() {
        let projected = enrich_tool_item(
            json!({ "name": "get_searxng_status" }),
            &mapping("get_searxng_status", "searxng_get_status"),
        )
        .expect("project tool");

        assert_eq!(projected["name"], "searxng_get_status");
        assert_eq!(projected["unique_name"], "searxng_get_status");
        assert_eq!(projected["tool_name"], "get_searxng_status");
        assert_eq!(projected["id"], "capability-id");
        assert_eq!(projected["ref_id"], "capability-ref-id");
    }

    #[test]
    fn management_projections_resolve_already_external_values() {
        let canonical_resource = crate::core::capability::resource_uri::encode_resource_uri("docs", "file:///guide.md")
            .expect("encode resource");
        let upstream_template = "demo://resource/lookup/{id}";
        let canonical_template =
            crate::core::capability::resource_uri::encode_resource_template("docs", upstream_template)
                .expect("encode resource template");
        let prompt =
            enrich_prompt_item(json!({ "name": "docs_help" }), &mapping("help", "docs_help")).expect("project prompt");
        let resource = enrich_resource_item(
            json!({ "uri": canonical_resource }),
            &mapping("file:///guide.md", &canonical_resource),
        )
        .expect("project resource");
        let template = enrich_resource_template_item(
            json!({ "name": "Lookup", "uri_template": upstream_template }),
            &mapping(upstream_template, &canonical_template),
        )
        .expect("project resource template");

        assert_eq!(prompt["name"], "docs_help");
        assert_eq!(prompt["prompt_name"], "help");
        assert_eq!(resource["uri"], canonical_resource);
        assert_eq!(resource["resource_uri"], "file:///guide.md");
        assert_eq!(template["name"], "Lookup");
        assert_eq!(template["uri_template"], upstream_template);
        assert_eq!(template["unique_uri_template"], canonical_template);
    }

    #[test]
    fn management_resource_template_projection_accepts_rmcp_wire_shape() {
        let upstream_template = "demo://resource/dynamic/text/{resourceId}";
        let canonical_template = "mcpmate://resources/template/everything/demo/dynamic/text/{resourceId}".to_string();
        let template = rmcp::model::ResourceTemplate::new(canonical_template.clone(), "Dynamic Text Resource");
        let wire_item = serde_json::to_value(template).expect("serialize RMCP resource template");

        assert_eq!(wire_item["uriTemplate"], canonical_template);
        let projected = enrich_resource_template_item(wire_item, &mapping(upstream_template, &canonical_template))
            .expect("project RMCP resource template");

        assert_eq!(projected["name"], "Dynamic Text Resource");
        assert_eq!(projected["uri_template"], upstream_template);
        assert_eq!(projected["unique_uri_template"], canonical_template);
        assert_eq!(projected["id"], "capability-id");
    }

    #[test]
    fn management_resource_template_projection_rejects_display_name_as_identity() {
        let display_name = "demo://resource/dynamic/text/{resourceId}";
        let result = enrich_resource_template_item(
            json!({ "name": display_name }),
            &mapping(
                display_name,
                "mcpmate://resources/template/everything/demo/dynamic/text/{resourceId}",
            ),
        );

        assert!(result.is_err());
    }

    #[test]
    fn cached_resource_template_protocol_projection_accepts_external_identifier() {
        let fixtures = [
            (
                "demo://resource/dynamic/blob/{resourceId}",
                "mcpmate://resources/template/everything/demo/dynamic/blob/{resourceId}",
            ),
            (
                "demo://resource/dynamic/text/{resourceId}",
                "mcpmate://resources/template/everything/demo/dynamic/text/{resourceId}",
            ),
        ];

        for (upstream_template, external_template) in fixtures {
            let cached_payload = resource_template_json_from_cached(
                CachedResourceTemplateInfo {
                    uri_template: upstream_template.to_string(),
                    name: Some("Dynamic resource".to_string()),
                    description: Some("Read a dynamic resource".to_string()),
                    mime_type: Some("application/octet-stream".to_string()),
                    enabled: true,
                    cached_at: Utc::now(),
                },
                external_template,
            );
            assert_eq!(cached_payload["uriTemplate"], external_template);

            let projected =
                enrich_resource_template_item(cached_payload, &mapping(upstream_template, external_template))
                    .expect("project cached resource template");

            assert_eq!(projected["uri_template"], upstream_template);
            assert_eq!(projected["unique_uri_template"], external_template);
            assert_eq!(projected["id"], "capability-id");
        }
    }

    #[tokio::test]
    async fn naming_projection_fails_when_catalog_mapping_cannot_be_loaded() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("create test database");

        let error = super::enrich_capability_items(
            super::CapabilityType::Tools,
            &pool,
            "server-1",
            vec![json!({ "name": "upstream_tool" })],
        )
        .await
        .expect_err("missing capability catalog must not expose an upstream name");

        assert!(error.to_string().contains("Failed to load tool naming mappings"));
    }

    #[tokio::test]
    async fn naming_projection_rejects_an_unmapped_upstream_value() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("create test database");
        sqlx::query("CREATE TABLE server_tools (server_id TEXT, tool_name TEXT, id TEXT, unique_name TEXT)")
            .execute(&pool)
            .await
            .expect("create tool catalog");
        sqlx::query("CREATE TABLE capability_refs (ref_id TEXT, server_id TEXT, kind TEXT, origin_key TEXT)")
            .execute(&pool)
            .await
            .expect("create capability refs");

        let error = super::enrich_capability_items(
            super::CapabilityType::Tools,
            &pool,
            "server-1",
            vec![json!({ "name": "unmapped_upstream_tool" })],
        )
        .await
        .expect_err("unmapped upstream values must not escape the connection pool");

        assert!(error.to_string().contains("unmapped_upstream_tool"));
    }

    #[tokio::test]
    async fn force_refresh_collision_records_namespace_remediation_issue() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::initialization::run_initialization(&pool)
            .await
            .expect("initialize schema");
        mcpmate_capability_store::SqliteCapabilityCatalog::new(pool.clone())
            .ensure_schema()
            .await
            .expect("initialize capability catalog");
        for (server_id, namespace) in [("server-owner", "a"), ("server-challenger", "a_b")] {
            let mut server = crate::config::models::Server::new_stdio(namespace.to_string(), Some("node".to_string()));
            server.id = Some(server_id.to_string());
            crate::config::server::upsert_server_definition(
                &pool,
                &server,
                &crate::config::models::ServerTransportDraft::Stdio {
                    command: Some("node".to_string()),
                    args: Vec::new(),
                    env: Default::default(),
                },
            )
            .await
            .expect("insert typed server definition");
        }
        crate::config::server::tools::upsert_server_tool(&pool, "server-owner", "a", "b_c", None)
            .await
            .expect("insert owner tool");

        let database = Arc::new(Database {
            pool: pool.clone(),
            path: PathBuf::from(":memory:"),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        });
        let state = Arc::new(AppState {
            connection_pool: Arc::new(Mutex::new(UpstreamConnectionPool::new(
                Arc::new(Config::default()),
                Some(database.clone()),
            ))),
            metrics_collector: Arc::new(MetricsCollector::new(Duration::from_secs(1))),
            http_proxy: None,
            profile_merge_service: None,
            database: Some(database),
            audit_database: None,
            audit_service: None,
            config_application_state: Arc::new(ConfigApplicationStateManager::new()),
            client_service: None,
            inspector_calls: Arc::new(InspectorCallRegistry::new()),
            inspector_sessions: Arc::new(InspectorSessionManager::new()),
            oauth_manager: RwLock::new(None),
            secret_store: RwLock::new(None),
            secret_store_readiness: RwLock::new(crate::api::routes::unavailable_secret_store_readiness(
                "test_unavailable",
            )),
        });
        crate::config::server::capabilities::store_dual_write(
            &pool,
            "server-challenger",
            "a_b",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        )
        .await
        .expect("store full baseline before scoped force refresh");
        let extracted = ExtractedCapability {
            tools: vec![rmcp::model::Tool::new(
                "c",
                "collision fixture",
                Arc::new(serde_json::Map::new()),
            )],
            ..ExtractedCapability::default()
        };
        let mut events = crate::core::events::EventBus::global().subscribe_async();

        persist_extracted_inventory(
            &state,
            &ServerIdentification {
                server_id: "server-challenger".to_string(),
                server_name: "a_b".to_string(),
            },
            CapabilityType::Tools,
            &extracted,
        )
        .await
        .expect_err("force refresh collision must fail");

        let issue = crate::config::server::namespace_repair::inspect_namespace_issue(&pool, "server-challenger")
            .await
            .expect("inspect issue");
        assert!(
            issue.is_some(),
            "force refresh must record a Board-visible remediation issue"
        );
        let event = tokio::time::timeout(Duration::from_millis(100), events.recv())
            .await
            .expect("force refresh collision must publish a block event")
            .expect("event channel must remain open");
        assert!(matches!(
            event,
            crate::core::events::Event::CapabilityCollisionDetected { server_id, .. }
                if server_id == "server-challenger"
        ));
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct ExtractedCapability {
    pub data: Vec<serde_json::Value>,
    pub initialize: Option<rmcp::model::InitializeResult>,
    pub tools: Vec<rmcp::model::Tool>,
    pub prompts: Vec<rmcp::model::Prompt>,
    pub resources: Vec<rmcp::model::Resource>,
    pub resource_templates: Vec<rmcp::model::ResourceTemplate>,
    pub kind_states: Vec<mcpmate_capability_store::KindObservation>,
}

#[cfg(test)]
impl ExtractedCapability {
    pub fn empty() -> Self {
        Self::default()
    }
}

/// Load tool name to (id, unique_name) mapping from database
///
/// # Arguments
/// * `pool` - SQLite connection pool
/// * `server_id` - Server identifier to filter tools
///
/// # Returns
/// HashMap mapping tool names to their (id, unique_name) tuples
pub async fn load_tool_mapping(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<CapabilityIdentityMapping, sqlx::Error> {
    Ok(sqlx::query_as::<_, (String, String, String, String)>(
        r#"
        SELECT st.tool_name, st.id, st.unique_name, cr.ref_id
        FROM server_tools st
        JOIN capability_refs cr
          ON cr.server_id = st.server_id
         AND cr.kind = 'tools'
         AND cr.origin_key = st.tool_name
        WHERE st.server_id = ?
        "#,
    )
    .bind(server_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(name, id, unique_name, ref_id)| (name, (id, unique_name, ref_id)))
    .collect())
}

/// Load prompt name to (id, unique_name) mapping from database
///
/// # Arguments
/// * `pool` - SQLite connection pool
/// * `server_id` - Server identifier to filter prompts
///
/// # Returns
/// HashMap mapping prompt names to their (id, unique_name) tuples
pub async fn load_prompt_mapping(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<CapabilityIdentityMapping, sqlx::Error> {
    Ok(sqlx::query_as::<_, (String, String, String, String)>(
        r#"
        SELECT sp.prompt_name, sp.id, sp.unique_name, cr.ref_id
        FROM server_prompts sp
        JOIN capability_refs cr
          ON cr.server_id = sp.server_id
         AND cr.kind = 'prompts'
         AND cr.origin_key = sp.prompt_name
        WHERE sp.server_id = ?
        "#,
    )
    .bind(server_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(name, id, unique_name, ref_id)| (name, (id, unique_name, ref_id)))
    .collect())
}

/// Load resource URI to (id, unique_uri) mapping from database
///
/// # Arguments
/// * `pool` - SQLite connection pool
/// * `server_id` - Server identifier to filter resources
///
/// # Returns
/// HashMap mapping resource URIs to their (id, unique_uri) tuples
pub async fn load_resource_mapping(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<CapabilityIdentityMapping, sqlx::Error> {
    Ok(sqlx::query_as::<_, (String, String, String, String)>(
        r#"
        SELECT sr.resource_uri, sr.id, sr.unique_uri, cr.ref_id
        FROM server_resources sr
        JOIN capability_refs cr
          ON cr.server_id = sr.server_id
         AND cr.kind = 'resources'
         AND cr.origin_key = sr.resource_uri
        WHERE sr.server_id = ?
        "#,
    )
    .bind(server_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(uri, id, unique_uri, ref_id)| (uri, (id, unique_uri, ref_id)))
    .collect())
}

pub async fn load_resource_template_mapping(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<CapabilityIdentityMapping, sqlx::Error> {
    Ok(sqlx::query_as::<_, (String, String, String, String)>(
        r#"
        SELECT srt.uri_template, srt.id, srt.unique_name, cr.ref_id
        FROM server_resource_templates srt
        JOIN capability_refs cr
          ON cr.server_id = srt.server_id
         AND cr.kind = 'resource_templates'
         AND cr.origin_key = srt.uri_template
        WHERE srt.server_id = ?
        "#,
    )
    .bind(server_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(tpl, id, unique_name, ref_id)| (tpl, (id, unique_name, ref_id)))
    .collect())
}

/// Return snapshot of the cache state for MCP server capabilities.
pub async fn server_cache_detail(
    State(state): State<Arc<AppState>>,
    Query(query): Query<CacheDetailsReq>,
) -> Result<Json<CacheDetailsResp>, StatusCode> {
    let result = cache_details_core(&query, &state).await?;
    Ok(Json(result))
}

/// Clear the cached capability data for all servers.
pub async fn server_cache_reset(State(state): State<Arc<AppState>>) -> Result<Json<CacheResetResp>, StatusCode> {
    let started_at = std::time::Instant::now();

    let result = cache_reset_core(&state).await;

    let (audit_status, audit_error) = match &result {
        Ok(_) => (AuditStatus::Success, None),
        Err(e) => (AuditStatus::Failed, Some(e.to_string())),
    };

    let mut data = Map::new();
    if let Ok(ref response) = result {
        if let Some(ref inner) = response.data {
            data.insert("success".to_string(), Value::Bool(inner.success));
            if let Some(ref msg) = inner.message {
                data.insert("message".to_string(), Value::String(msg.clone()));
            }
        }
    }

    crate::audit::interceptor::emit_event(
        state.audit_service.as_ref(),
        crate::audit::interceptor::build_rest_event(
            AuditAction::ServerCacheReset,
            audit_status,
            "POST",
            "/api/mcp/servers/cache/reset",
            Some(started_at.elapsed().as_millis() as u64),
            None,
            None,
            Some(data),
            audit_error,
        ),
    )
    .await;

    result.map(Json)
}

struct CapabilityDetailLookup {
    item: Option<serde_json::Value>,
    cache_hit: bool,
    source: String,
}

/// Return a single capability item for lazy detail expansion.
pub async fn server_capability_detail(
    State(state): State<Arc<AppState>>,
    Query(request): Query<ServerCapabilityDetailReq>,
) -> Result<Json<ServerCapabilityDetailResp>, ApiError> {
    let key = request.key.trim();
    if key.is_empty() {
        return Err(ApiError::BadRequest(
            "Capability detail key must not be empty".to_string(),
        ));
    }

    let query = InspectQuery {
        refresh: None,
        format: None,
        include_meta: None,
        timeout: None,
    };
    let (_db, server_info, _) = get_server_info_for_inspect(&state, &request.id, &query).await?;
    let capability_type = parse_capability_detail_type(&request.kind)?;
    let lookup = match cached_capability_detail_item(&state, &server_info, capability_type, key).await {
        Ok(lookup) => lookup,
        Err(error) => {
            tracing::error!(
                server_id = %server_info.server_id,
                kind = %request.kind,
                key = %key,
                error = %error,
                "Cached capability detail lookup failed"
            );
            return Err(error);
        }
    };

    let item = lookup.item;

    let state_name = if item.is_some() { "ok" } else { "missing" };
    Ok(Json(ServerCapabilityDetailResp::success(ServerCapabilityDetailData {
        item,
        state: state_name.to_string(),
        meta: ServerCapabilityMeta {
            cache_hit: lookup.cache_hit,
            strategy: "cache".to_string(),
            source: lookup.source,
        },
    })))
}

async fn cached_capability_detail_item(
    state: &Arc<AppState>,
    server_info: &ServerIdentification,
    capability_type: CapabilityType,
    key: &str,
) -> Result<CapabilityDetailLookup, ApiError> {
    let result = read_capability_list_result(state, &server_info.server_id, None, capability_type).await?;
    let item = find_capability_detail_item(capability_type, key, result.items)?;

    Ok(CapabilityDetailLookup {
        item,
        cache_hit: result.meta.cache_hit,
        source: result.meta.source,
    })
}

fn find_capability_detail_item(
    capability_type: CapabilityType,
    key: &str,
    items: crate::core::capability::runtime::CapabilityItems,
) -> Result<Option<Value>, ApiError> {
    let item = match items {
        crate::core::capability::runtime::CapabilityItems::Tools(items) => items
            .into_iter()
            .find(|item| item.name.as_ref() == key)
            .map(serde_json::to_value),
        crate::core::capability::runtime::CapabilityItems::Prompts(items) => items
            .into_iter()
            .find(|item| item.name == key)
            .map(serde_json::to_value),
        crate::core::capability::runtime::CapabilityItems::Resources(items) => {
            items.into_iter().find(|item| item.uri == key).map(serde_json::to_value)
        }
        crate::core::capability::runtime::CapabilityItems::ResourceTemplates(items) => items
            .into_iter()
            .find(|item| item.uri_template == key)
            .map(serde_json::to_value),
    };
    match item {
        Some(Ok(value)) => Ok(Some(value)),
        Some(Err(error)) => {
            tracing::error!(capability = ?capability_type, error = %error, "Failed to serialize capability detail item");
            Err(ApiError::InternalError(format!(
                "Failed to serialize capability detail item: {error}"
            )))
        }
        None => Ok(None),
    }
}

fn parse_capability_detail_type(kind: &str) -> Result<CapabilityType, StatusCode> {
    match kind.trim().to_ascii_lowercase().as_str() {
        "tool" | "tools" => Ok(CapabilityType::Tools),
        "resource" | "resources" => Ok(CapabilityType::Resources),
        "prompt" | "prompts" => Ok(CapabilityType::Prompts),
        "template" | "templates" | "resource_template" | "resource_templates" => Ok(CapabilityType::ResourceTemplates),
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 1000;

async fn cache_details_core(
    query: &CacheDetailsReq,
    state: &Arc<AppState>,
) -> Result<CacheDetailsResp, StatusCode> {
    match query.view {
        CacheViewType::Keys => {
            let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
            let database = get_database_from_state(state).map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            let keys: Vec<CacheKeyItem> = database
                .capability_cache
                .diagnostic_keys_for_server(limit, query.server_id.as_deref())
                .await
                .into_iter()
                .map(|e| CacheKeyItem {
                    cache: e.cache.to_string(),
                    key_hash: e.key_hash,
                    approx_value_size_bytes: e.approx_value_size_bytes,
                    cached_at: Some(e.cached_at.to_rfc3339()),
                })
                .collect();

            let total = keys.len();
            let response = CacheDetailsData {
                keys: Some(keys),
                storage: None,
                metrics: None,
                total: Some(total),
                generated_at: None,
            };

            Ok(CacheDetailsResp::success(response))
        }
        CacheViewType::Stats => {
            let database = get_database_from_state(state).map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            let catalog = mcpmate_capability_store::SqliteCapabilityCatalog::new(database.pool.clone());
            let stats = mcpmate_capability_store::CapabilityCatalog::stats(&catalog)
                .await
                .map_err(|error| {
                    tracing::error!(error = %error, "Failed to read SQLite capability catalog statistics");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
            let live = database.capability_cache.metrics().await;

            let storage = CacheStorageStats {
                catalog: CacheCatalogStats {
                    snapshots: stats.snapshots.max(0) as u64,
                    ready_snapshots: stats.ready_snapshots.max(0) as u64,
                    invalidated_snapshots: stats.invalidated_snapshots.max(0) as u64,
                    unavailable_snapshots: stats.unavailable_snapshots.max(0) as u64,
                    records: stats.records.max(0) as u64,
                    tools: stats.tools.max(0) as u64,
                    resources: stats.resources.max(0) as u64,
                    prompts: stats.prompts.max(0) as u64,
                    resource_templates: stats.resource_templates.max(0) as u64,
                },
                memory: CacheMemoryStats {
                    raw_snapshot_entries: live.raw_entries,
                    projection_entries: live.projection_entries,
                },
            };

            let cache_hits = live.raw_hits + live.projection_hits;
            let cache_misses = live.raw_misses + live.projection_misses;
            let hit_ratio = if live.total_queries == 0 {
                0.0
            } else {
                cache_hits as f64 / live.total_queries as f64
            };
            let hit_ratio = (hit_ratio * 10_000.0).round() / 10_000.0;

            let metrics = CacheMetricsStats {
                total_queries: live.total_queries,
                cache_hits,
                cache_misses,
                hit_ratio,
                raw_snapshot_hits: live.raw_hits,
                raw_snapshot_misses: live.raw_misses,
                projection_hits: live.projection_hits,
                projection_misses: live.projection_misses,
                single_flight_waits: live.single_flight_waits,
                evictions: live.raw_evictions + live.projection_evictions,
                cache_invalidations: live.invalidations,
            };

            let response = CacheDetailsData {
                keys: None,
                storage: Some(storage),
                metrics: Some(metrics),
                total: None,
                generated_at: Some(chrono::Utc::now().to_rfc3339()),
            };

            Ok(CacheDetailsResp::success(response))
        }
    }
}

async fn cache_reset_core(state: &Arc<AppState>) -> Result<CacheResetResp, StatusCode> {
    let database = get_database_from_state(state).map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let catalog = mcpmate_capability_store::SqliteCapabilityCatalog::new(database.pool.clone());
    let invalidated = catalog
        .invalidate_all("explicit capability cache reset")
        .await
        .map_err(|error| {
            tracing::error!(error = %error, "Failed to invalidate SQLite capability catalog");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    database.capability_cache.clear().await;
    for commit in &invalidated {
        crate::config::server::capabilities::publish_catalog_commit(
            &commit.server_id,
            &commit.server_name,
            commit.revision,
        );
    }

    let response = CacheResetData {
        success: true,
        message: Some(format!(
            "Cleared node-local capability caches and invalidated {} durable catalog snapshots",
            invalidated.len()
        )),
    };

    Ok(CacheResetResp::success(response))
}

/// Enrich tool items with database identifiers
#[inline]
fn enrich_tool_item(
    item: serde_json::Value,
    mapping: &CapabilityIdentityMapping,
) -> Result<serde_json::Value, ApiError> {
    let mut item = item;
    let presented_name = item
        .get("tool_name")
        .or_else(|| item.get("name"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| ApiError::InternalError("Tool response is missing a capability name".to_string()))?;
    let (upstream_name, id, unique_name, ref_id) =
        find_capability_mapping(mapping, presented_name).ok_or_else(|| {
            ApiError::InternalError(format!("Tool '{presented_name}' has no persisted external identifier"))
        })?;
    let obj = item
        .as_object_mut()
        .ok_or_else(|| ApiError::InternalError("Tool response is not an object".to_string()))?;
    obj.insert("name".to_string(), serde_json::json!(unique_name));
    obj.insert("tool_name".to_string(), serde_json::json!(upstream_name));
    obj.insert("unique_name".to_string(), serde_json::json!(unique_name));
    obj.insert("id".to_string(), serde_json::json!(id));
    obj.insert("ref_id".to_string(), serde_json::json!(ref_id));
    Ok(item)
}

/// Enrich prompt items with database identifiers
#[inline]
fn enrich_prompt_item(
    item: serde_json::Value,
    mapping: &CapabilityIdentityMapping,
) -> Result<serde_json::Value, ApiError> {
    let mut item = item;
    let presented_name = item
        .get("prompt_name")
        .or_else(|| item.get("name"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| ApiError::InternalError("Prompt response is missing a capability name".to_string()))?;
    let (upstream_name, id, unique_name, ref_id) =
        find_capability_mapping(mapping, presented_name).ok_or_else(|| {
            ApiError::InternalError(format!(
                "Prompt '{presented_name}' has no persisted external identifier"
            ))
        })?;
    let obj = item
        .as_object_mut()
        .ok_or_else(|| ApiError::InternalError("Prompt response is not an object".to_string()))?;
    obj.insert("name".to_string(), serde_json::json!(unique_name));
    obj.insert("prompt_name".to_string(), serde_json::json!(upstream_name));
    obj.insert("unique_name".to_string(), serde_json::json!(unique_name));
    obj.insert("id".to_string(), serde_json::json!(id));
    obj.insert("ref_id".to_string(), serde_json::json!(ref_id));
    Ok(item)
}

/// Enrich resource items with database identifiers
#[inline]
fn enrich_resource_item(
    item: serde_json::Value,
    mapping: &CapabilityIdentityMapping,
) -> Result<serde_json::Value, ApiError> {
    let mut item = item;
    let presented_uri = item
        .get("resource_uri")
        .or_else(|| item.get("uri"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| ApiError::InternalError("Resource response is missing a URI".to_string()))?;
    let (upstream_uri, id, unique_uri, ref_id) = find_capability_mapping(mapping, presented_uri).ok_or_else(|| {
        ApiError::InternalError(format!(
            "Resource '{presented_uri}' has no persisted external identifier"
        ))
    })?;
    let obj = item
        .as_object_mut()
        .ok_or_else(|| ApiError::InternalError("Resource response is not an object".to_string()))?;
    obj.insert("uri".to_string(), serde_json::json!(unique_uri));
    obj.insert("resource_uri".to_string(), serde_json::json!(upstream_uri));
    obj.insert("unique_uri".to_string(), serde_json::json!(unique_uri));
    obj.insert("id".to_string(), serde_json::json!(id));
    obj.insert("ref_id".to_string(), serde_json::json!(ref_id));
    Ok(item)
}

/// Enrich resource template items with database identifiers
#[inline]
fn enrich_resource_template_item(
    item: serde_json::Value,
    mapping: &CapabilityIdentityMapping,
) -> Result<serde_json::Value, ApiError> {
    let mut item = item;
    let presented_template = ["unique_uri_template", "uri_template", "uriTemplate"]
        .into_iter()
        .filter_map(|field| item.get(field).and_then(|value| value.as_str()))
        .find(|value| !value.trim().is_empty())
        .ok_or_else(|| ApiError::InternalError("Resource template response is missing a template".to_string()))?;
    let (upstream_template, id, unique_name, ref_id) = find_capability_mapping(mapping, presented_template)
        .ok_or_else(|| {
            ApiError::InternalError(format!(
                "Resource template '{presented_template}' has no persisted external identifier"
            ))
        })?;
    let obj = item
        .as_object_mut()
        .ok_or_else(|| ApiError::InternalError("Resource template response is not an object".to_string()))?;
    obj.insert("uri_template".to_string(), serde_json::json!(upstream_template));
    obj.insert("unique_uri_template".to_string(), serde_json::json!(unique_name));
    obj.insert("id".to_string(), serde_json::json!(id));
    obj.insert("ref_id".to_string(), serde_json::json!(ref_id));
    Ok(item)
}

fn find_capability_mapping<'a>(
    mapping: &'a CapabilityIdentityMapping,
    presented_value: &str,
) -> Option<(&'a str, &'a str, &'a str, &'a str)> {
    mapping
        .iter()
        .find(|(upstream_value, (_, external_value, _))| {
            upstream_value.as_str() == presented_value || external_value == presented_value
        })
        .map(|(upstream_value, (id, external_value, ref_id))| {
            (
                upstream_value.as_str(),
                id.as_str(),
                external_value.as_str(),
                ref_id.as_str(),
            )
        })
}

/// Enrich capability items with database-stored identifiers
///
/// Adds `id` and `unique_name` fields to capability items by looking up
/// the corresponding records in the database.
///
/// # Arguments
/// * `capability_type` - Type of capability (Tools, Prompts, Resources, ResourceTemplates)
/// * `pool` - SQLite connection pool
/// * `server_id` - Server identifier to filter records
/// * `items` - JSON objects representing capability items
///
/// # Returns
/// Enhanced JSON objects with `id` and `unique_name` fields added
pub async fn enrich_capability_items(
    capability_type: CapabilityType,
    pool: &Pool<Sqlite>,
    server_id: &str,
    items: Vec<serde_json::Value>,
) -> Result<Vec<serde_json::Value>, ApiError> {
    let enriched = match capability_type {
        CapabilityType::Tools => {
            let mapping = load_tool_mapping(pool, server_id)
                .await
                .map_err(|error| ApiError::InternalError(format!("Failed to load tool naming mappings: {error}")))?;
            items
                .into_iter()
                .map(|item| enrich_tool_item(item, &mapping))
                .collect::<Result<Vec<_>, _>>()?
        }
        CapabilityType::Prompts => {
            let mapping = load_prompt_mapping(pool, server_id)
                .await
                .map_err(|error| ApiError::InternalError(format!("Failed to load prompt naming mappings: {error}")))?;
            items
                .into_iter()
                .map(|item| enrich_prompt_item(item, &mapping))
                .collect::<Result<Vec<_>, _>>()?
        }
        CapabilityType::Resources => {
            let mapping = load_resource_mapping(pool, server_id).await.map_err(|error| {
                ApiError::InternalError(format!("Failed to load resource naming mappings: {error}"))
            })?;
            items
                .into_iter()
                .map(|item| enrich_resource_item(item, &mapping))
                .collect::<Result<Vec<_>, _>>()?
        }
        CapabilityType::ResourceTemplates => {
            let mapping = load_resource_template_mapping(pool, server_id).await.map_err(|error| {
                ApiError::InternalError(format!("Failed to load resource template naming mappings: {error}"))
            })?;
            items
                .into_iter()
                .map(|item| enrich_resource_template_item(item, &mapping))
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    Ok(enriched)
}

pub fn respond_with_enriched(
    data: Vec<serde_json::Value>,
    cache_hit: bool,
    refresh_strategy: Option<RefreshStrategy>,
    source: &str,
) -> Json<serde_json::Value> {
    crate::api::handlers::server::common::create_inspect_response(data, cache_hit, refresh_strategy, source)
}

/// Create standardized JSON representation of a tool
pub fn tool_json(
    name: &str,
    description: Option<String>,
    input_schema: serde_json::Value,
    output_schema: Option<serde_json::Value>,
    icons: Option<Vec<Icon>>,
) -> serde_json::Value {
    let mut item = Map::from_iter([
        ("name".to_string(), name.into()),
        ("inputSchema".to_string(), input_schema),
    ]);
    insert_optional_json(&mut item, "description", description);
    insert_optional_json(&mut item, "outputSchema", output_schema);
    insert_optional_json(&mut item, "icons", icons);
    Value::Object(item)
}

pub fn tool_json_from_cached(
    t: &crate::core::capability::index::CachedToolInfo,
    external_name: &str,
) -> serde_json::Value {
    let schema = t.input_schema().unwrap_or_else(|_| serde_json::json!({}));
    let out = t.output_schema();
    tool_json(external_name, t.description.clone(), schema, out, t.icons.clone())
}

pub fn tool_management_json_from_cached(t: &crate::core::capability::index::CachedToolInfo) -> serde_json::Value {
    serde_json::json!({
        "name": t.name,
        "description": t.description,
        "input_schema": t.input_schema().unwrap_or_else(|_| serde_json::json!({})),
        "output_schema": t.output_schema(),
        "unique_name": t.unique_name,
        "icons": t.icons,
    })
}

pub fn resource_json(
    uri: &str,
    name: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
    icons: Option<Vec<Icon>>,
) -> serde_json::Value {
    let mut item = Map::from_iter([
        ("uri".to_string(), uri.into()),
        ("name".to_string(), name.unwrap_or_else(|| uri.to_string()).into()),
    ]);
    insert_optional_json(&mut item, "description", description);
    insert_optional_json(&mut item, "mimeType", mime_type);
    insert_optional_json(&mut item, "icons", icons);
    Value::Object(item)
}

pub fn resource_json_from_cached(
    r: crate::core::capability::index::CachedResourceInfo,
    external_uri: &str,
) -> serde_json::Value {
    resource_json(external_uri, r.name, r.description, r.mime_type, r.icons)
}

pub fn resource_template_json(
    uri_template: &str,
    name: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
) -> serde_json::Value {
    let mut item = Map::from_iter([
        ("uriTemplate".to_string(), uri_template.into()),
        (
            "name".to_string(),
            name.unwrap_or_else(|| uri_template.to_string()).into(),
        ),
    ]);
    insert_optional_json(&mut item, "description", description);
    insert_optional_json(&mut item, "mimeType", mime_type);
    Value::Object(item)
}

pub fn resource_template_json_from_cached(
    t: crate::core::capability::index::CachedResourceTemplateInfo,
    external_uri_template: &str,
) -> serde_json::Value {
    resource_template_json(external_uri_template, t.name, t.description, t.mime_type)
}

pub fn prompt_json(
    name: &str,
    description: Option<String>,
    arguments: Vec<crate::core::capability::index::PromptArgument>,
    icons: Option<Vec<Icon>>,
) -> serde_json::Value {
    let args: Vec<serde_json::Value> = arguments
        .into_iter()
        .map(|a| {
            serde_json::json!({
                "name": a.name,
                "description": a.description,
                "required": a.required,
            })
        })
        .collect();

    let mut item = Map::from_iter([("name".to_string(), name.into())]);
    insert_optional_json(&mut item, "description", description);
    if !args.is_empty() {
        item.insert("arguments".to_string(), args.into());
    }
    insert_optional_json(&mut item, "icons", icons);
    Value::Object(item)
}

pub fn prompt_json_from_cached(
    p: crate::core::capability::index::CachedPromptInfo,
    external_name: &str,
) -> serde_json::Value {
    prompt_json(
        external_name,
        p.description.clone(),
        p.arguments.clone(),
        p.icons.clone(),
    )
}

fn insert_optional_json<T>(
    item: &mut Map<String, Value>,
    key: &str,
    value: Option<T>,
) where
    T: serde::Serialize,
{
    if let Some(value) = value {
        item.insert(
            key.to_string(),
            serde_json::to_value(value).expect("MCP capability fields must serialize"),
        );
    }
}

/// Persist the authoritative inventory for one explicitly refreshed kind.
#[cfg(test)]
async fn persist_extracted_inventory(
    state: &Arc<AppState>,
    server_info: &ServerIdentification,
    capability_type: CapabilityType,
    extracted: &ExtractedCapability,
) -> Result<(), ApiError> {
    let kinds = match capability_type {
        CapabilityType::Tools => crate::core::pool::CapSyncFlags::TOOLS,
        CapabilityType::Prompts => crate::core::pool::CapSyncFlags::PROMPTS,
        CapabilityType::Resources => crate::core::pool::CapSyncFlags::RESOURCES,
        CapabilityType::ResourceTemplates => crate::core::pool::CapSyncFlags::RESOURCE_TEMPLATES,
    };
    let db = get_database_from_state(state)?;
    if let Err(error) = crate::config::server::capabilities::commit_protocol_observation_for_kinds(
        &db.pool,
        &server_info.server_id,
        &server_info.server_name,
        extracted.initialize.clone(),
        extracted.tools.clone(),
        extracted.resources.clone(),
        extracted.prompts.clone(),
        extracted.resource_templates.clone(),
        kinds,
        extracted.kind_states.clone(),
    )
    .await
    {
        crate::config::server::namespace_repair::record_capability_collision_from_error(&db.pool, &error).await?;
        return Err(error.into());
    }
    db.capability_cache.invalidate_server(&server_info.server_id).await;
    Ok(())
}
