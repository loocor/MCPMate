use std::{collections::HashSet, sync::Arc};

use axum::{
    Json,
    extract::{Query, State},
};

use super::{common::*, unified_capability_query::query_unified_capabilities};
use crate::{
    api::{
        handlers::{ApiError, server::common::InspectParams},
        models::token_estimate::{CapTypeEstimate, TokenBreakdownResponse, TokenEstimateQuery, TokenEstimateResponse},
    },
    core::{
        capability::{
            CapabilityItem, CapabilityType,
            naming::{NamingKind, load_external_identifier},
        },
        estimate,
    },
};

pub async fn token_estimate(
    State(state): State<Arc<AppState>>,
    Query(params): Query<TokenEstimateQuery>,
) -> Result<Json<TokenEstimateResponse>, ApiError> {
    let profile_id = params.profile_id;

    let db = get_database(&state).await?;

    let profile = crate::config::profile::get_profile(&db.pool, &profile_id)
        .await
        .map_err(|e| ApiError::InternalError(format!("Failed to get profile: {e}")))?;

    let Some(_) = profile else {
        return Err(ApiError::NotFound(format!("Profile '{profile_id}' not found")));
    };

    let profile_servers = crate::config::profile::get_profile_servers(&db.pool, &profile_id)
        .await
        .map_err(|e| ApiError::InternalError(format!("Failed to get profile servers: {e}")))?;

    let enabled_tools = get_enabled_tools_for_profile(&db.pool, &profile_id).await?;
    let enabled_prompts = get_enabled_prompts_for_profile(&db.pool, &profile_id).await?;
    let enabled_resources = get_enabled_resources_for_profile(&db.pool, &profile_id).await?;
    let enabled_templates = get_enabled_resource_templates_for_profile(&db.pool, &profile_id).await?;

    let params = InspectParams::default();

    let mut tools_estimate = CapTypeEstimate::default();
    let mut prompts_estimate = CapTypeEstimate::default();
    let mut resources_estimate = CapTypeEstimate::default();
    let mut templates_estimate = CapTypeEstimate::default();

    for profile_server in &profile_servers {
        let server_id = &profile_server.server_id;

        accumulate_estimate(
            query_unified_capabilities(&state, server_id, CapabilityType::Tools, &params).await,
            &mut tools_estimate,
            profile_server.enabled,
            |item| match item {
                CapabilityItem::Tool(tool) => Some((server_id.clone(), tool.name.clone())),
                _ => None,
            },
            &enabled_tools,
        );

        accumulate_estimate(
            query_unified_capabilities(&state, server_id, CapabilityType::Prompts, &params).await,
            &mut prompts_estimate,
            profile_server.enabled,
            |item| match item {
                CapabilityItem::Prompt(prompt) => Some((server_id.clone(), prompt.name.clone())),
                _ => None,
            },
            &enabled_prompts,
        );

        accumulate_estimate(
            query_unified_capabilities(&state, server_id, CapabilityType::Resources, &params).await,
            &mut resources_estimate,
            profile_server.enabled,
            |item| match item {
                CapabilityItem::Resource(resource) => Some((server_id.clone(), resource.uri.clone())),
                _ => None,
            },
            &enabled_resources,
        );

        accumulate_estimate(
            query_unified_capabilities(&state, server_id, CapabilityType::ResourceTemplates, &params).await,
            &mut templates_estimate,
            profile_server.enabled,
            |item| match item {
                CapabilityItem::ResourceTemplate(template) => Some((server_id.clone(), template.uri_template.clone())),
                _ => None,
            },
            &enabled_templates,
        );
    }

    finalize_estimate(&mut tools_estimate);
    finalize_estimate(&mut prompts_estimate);
    finalize_estimate(&mut resources_estimate);
    finalize_estimate(&mut templates_estimate);

    let total_available_tokens = tools_estimate.available_tokens
        + prompts_estimate.available_tokens
        + resources_estimate.available_tokens
        + templates_estimate.available_tokens;

    let visible_tokens = tools_estimate.visible_tokens
        + prompts_estimate.visible_tokens
        + resources_estimate.visible_tokens
        + templates_estimate.visible_tokens;

    let savings_tokens = tools_estimate.savings_tokens
        + prompts_estimate.savings_tokens
        + resources_estimate.savings_tokens
        + templates_estimate.savings_tokens;

    let response = TokenEstimateResponse {
        total_available_tokens,
        visible_tokens,
        savings_tokens,
        breakdown: TokenBreakdownResponse {
            tools: tools_estimate,
            prompts: prompts_estimate,
            resources: resources_estimate,
            templates: templates_estimate,
        },
        estimation_method: "chars_div_4".to_string(),
        approximate: true,
    };

    Ok(Json(response))
}

fn accumulate_estimate<F>(
    items: Option<Vec<CapabilityItem>>,
    estimate: &mut CapTypeEstimate,
    server_enabled: bool,
    key_fn: F,
    enabled_keys: &HashSet<(String, String)>,
) where
    F: Fn(&CapabilityItem) -> Option<(String, String)>,
{
    let Some(items) = items else {
        return;
    };

    for item in items {
        let Some(key) = key_fn(&item) else {
            continue;
        };

        let tokens = estimate_item_tokens(&item);
        estimate.available_count += 1;
        estimate.available_tokens += tokens;

        if server_enabled && enabled_keys.contains(&key) {
            estimate.visible_count += 1;
            estimate.visible_tokens += tokens;
        }
    }
}

fn finalize_estimate(estimate: &mut CapTypeEstimate) {
    estimate.disabled_count = estimate.available_count.saturating_sub(estimate.visible_count);
    estimate.savings_tokens = estimate.available_tokens.saturating_sub(estimate.visible_tokens);
}

fn estimate_item_tokens(item: &CapabilityItem) -> u32 {
    serde_json::to_string(item)
        .map(|json| estimate::estimate_capability_tokens(&json))
        .unwrap_or(0)
}

async fn get_enabled_tools_for_profile(
    pool: &sqlx::SqlitePool,
    profile_id: &str,
) -> Result<HashSet<(String, String)>, ApiError> {
    let tools = crate::config::profile::get_profile_tools(pool, profile_id)
        .await
        .map_err(|e| ApiError::InternalError(format!("Failed to get profile tools: {e}")))?;

    Ok(tools
        .into_iter()
        .filter(|tool| tool.enabled)
        .map(|tool| (tool.server_id, tool.unique_name))
        .collect())
}

async fn get_enabled_prompts_for_profile(
    pool: &sqlx::SqlitePool,
    profile_id: &str,
) -> Result<HashSet<(String, String)>, ApiError> {
    let prompts = crate::config::profile::get_enabled_prompts_for_profile(pool, profile_id)
        .await
        .map_err(|e| ApiError::InternalError(format!("Failed to get enabled prompts: {e}")))?;

    let mut keys = HashSet::new();
    for prompt in prompts {
        let external_name = load_external_identifier(pool, NamingKind::Prompt, &prompt.server_id, &prompt.prompt_name)
            .await
            .map_err(|error| ApiError::InternalError(error.to_string()))?;
        keys.insert((prompt.server_id, external_name));
    }
    Ok(keys)
}

async fn get_enabled_resources_for_profile(
    pool: &sqlx::SqlitePool,
    profile_id: &str,
) -> Result<HashSet<(String, String)>, ApiError> {
    let resources = crate::config::profile::get_enabled_resources_for_profile(pool, profile_id)
        .await
        .map_err(|e| ApiError::InternalError(format!("Failed to get enabled resources: {e}")))?;

    let mut keys = HashSet::new();
    for resource in resources {
        let external_uri =
            load_external_identifier(pool, NamingKind::Resource, &resource.server_id, &resource.resource_uri)
                .await
                .map_err(|error| ApiError::InternalError(error.to_string()))?;
        keys.insert((resource.server_id, external_uri));
    }
    Ok(keys)
}

async fn get_enabled_resource_templates_for_profile(
    pool: &sqlx::SqlitePool,
    profile_id: &str,
) -> Result<HashSet<(String, String)>, ApiError> {
    let templates = crate::config::profile::get_enabled_resource_templates_for_profile(pool, profile_id)
        .await
        .map_err(|e| ApiError::InternalError(format!("Failed to get enabled resource templates: {e}")))?;

    let mut keys = HashSet::new();
    for template in templates {
        let external_template = load_external_identifier(
            pool,
            NamingKind::ResourceTemplate,
            &template.server_id,
            &template.resource_uri,
        )
        .await
        .map_err(|error| ApiError::InternalError(error.to_string()))?;
        keys.insert((template.server_id, external_template));
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use sqlx::sqlite::SqlitePoolOptions;

    use super::{
        get_enabled_prompts_for_profile, get_enabled_resource_templates_for_profile, get_enabled_resources_for_profile,
        get_enabled_tools_for_profile,
    };

    #[tokio::test]
    async fn enabled_profile_keys_use_external_capability_identifiers() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("create test database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::initialization::run_initialization(&pool)
            .await
            .expect("initialize database");

        let mut server = crate::config::models::Server::new_stdio("docs".to_string(), Some("node".to_string()));
        server.id = Some("server-a".to_string());
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

        for statement in [
            "INSERT INTO profile (id, name, type) VALUES ('profile-a', 'Profile A', 'shared')",
            "INSERT INTO profile_server_relationships (profile_id, server_id, new_ref_policy) VALUES ('profile-a', 'server-a', 'follow')",
        ] {
            sqlx::query(statement)
                .execute(&pool)
                .await
                .expect("insert capability fixture");
        }
        crate::config::server::capabilities::commit_protocol_items_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(
                serde_json::from_value(serde_json::json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {
                        "tools": {"listChanged": true},
                        "prompts": {"listChanged": true},
                        "resources": {"subscribe": false, "listChanged": true}
                    },
                    "serverInfo": {"name": "docs", "version": "1.0.0"}
                }))
                .expect("decode initialize fixture"),
            ),
            vec![
                serde_json::from_value(serde_json::json!({
                    "name": "read",
                    "description": "Read a document",
                    "inputSchema": {"type": "object"}
                }))
                .expect("decode tool fixture"),
            ],
            vec![
                serde_json::from_value(serde_json::json!({
                    "uri": "file:///guide.md",
                    "name": "Guide"
                }))
                .expect("decode resource fixture"),
            ],
            vec![
                serde_json::from_value(serde_json::json!({
                    "name": "review",
                    "description": "Review a document"
                }))
                .expect("decode prompt fixture"),
            ],
            vec![
                serde_json::from_value(serde_json::json!({
                    "uriTemplate": "file:///{path}",
                    "name": "File"
                }))
                .expect("decode resource template fixture"),
            ],
            crate::core::pool::CapSyncFlags::ALL,
        )
        .await
        .expect("commit capability fixture");
        sqlx::query(
            r#"
            INSERT INTO profile_capability_refs (profile_id, ref_id, enabled)
            SELECT 'profile-a', ref_id, 1
            FROM capability_refs
            WHERE server_id = 'server-a'
            "#,
        )
        .execute(&pool)
        .await
        .expect("attach capability refs to profile");

        assert_eq!(
            get_enabled_tools_for_profile(&pool, "profile-a")
                .await
                .expect("load enabled tools"),
            HashSet::from([("server-a".to_string(), "docs_read".to_string())])
        );
        assert_eq!(
            get_enabled_prompts_for_profile(&pool, "profile-a")
                .await
                .expect("load enabled prompts"),
            HashSet::from([("server-a".to_string(), "docs_review".to_string())])
        );
        assert_eq!(
            get_enabled_resources_for_profile(&pool, "profile-a")
                .await
                .expect("load enabled resources"),
            HashSet::from([(
                "server-a".to_string(),
                "mcpmate://resources/docs/file/guide.md".to_string(),
            )])
        );
        assert_eq!(
            get_enabled_resource_templates_for_profile(&pool, "profile-a")
                .await
                .expect("load enabled resource templates"),
            HashSet::from([(
                "server-a".to_string(),
                "mcpmate://resources/template/docs/file/{path}".to_string(),
            )])
        );
    }
}
