use super::*;
use crate::mcper::builtin::ClientBuiltinContext;
use rmcp::ErrorData as McpError;
use rmcp::model::{GetPromptRequestParams, GetPromptResult, ListPromptsResult, PaginatedRequestParams};
use rmcp::service::RequestContext;
use std::collections::HashSet;

pub(super) async fn list_prompts(
    server: &ProxyServer,
    _request: Option<PaginatedRequestParams>,
    _context: RequestContext<rmcp::RoleServer>,
) -> Result<ListPromptsResult, McpError> {
    let client = server.resolve_bound_client_context(&_context).await?;
    let surface = server.load_active_surface(&client).await?;
    let page = server.paginator.paginate_prompts(&_request, surface.prompts())?;

    tracing::info!(
        total = page.items.len(),
        has_next = page.next_cursor.is_some(),
        consumer_id = %surface.consumer_id,
        publication_id = %surface.publication_id,
        generation = surface.generation,
        "Proxy listed prompts from active Surface publication"
    );

    Ok(ListPromptsResult {
        prompts: page.items,
        next_cursor: page.next_cursor,
        ..Default::default()
    })
}

pub(super) async fn get_prompt(
    server: &ProxyServer,
    request: GetPromptRequestParams,
    _context: RequestContext<rmcp::RoleServer>,
) -> Result<GetPromptResult, McpError> {
    let client = server.resolve_bound_client_context(&_context).await?;
    let surface_entry = server
        .require_active_surface_entry(
            &client,
            mcpmate_capability_store::CapabilityKind::Prompts,
            request.name.as_ref(),
        )
        .await?;
    let is_builtin = surface_entry.source_server_id == mcpmate_capability_store::BUILTIN_CAPABILITY_SOURCE_ID;
    tracing::debug!("Getting prompt: {}", request.name);

    let vis = crate::core::profile::visibility::ProfileVisibilityService::new(
        server.database.clone(),
        server.profile_service.clone(),
    );
    let capability_config = vis
        .resolve_capability_config_for_client(&client)
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    let workflow_direct = if matches!(client.config_mode.as_deref(), Some("unify")) {
        match server.database.as_ref() {
            Some(database) => crate::core::profile::publication::load_published_direct_set(&database.pool)
                .await
                .map_err(|error| McpError::internal_error(error.to_string(), None))?,
            None => crate::core::profile::publication::PublishedWorkflowDirectSet::default(),
        }
    } else {
        crate::core::profile::publication::PublishedWorkflowDirectSet::default()
    };
    let builtin_context = ClientBuiltinContext {
        client_id: client.client_id.clone(),
        session_id: client.session_id.clone(),
        config_mode: client.config_mode.clone(),
        capability_source: capability_config.capability_source,
        selected_profile_ids: capability_config.selected_profile_ids,
        custom_profile_id: capability_config.custom_profile_id,
        unify_workspace: client.unify_workspace.clone(),
        workflow_direct,
    };

    if is_builtin {
        if let Some(result) = server
            .builtin_services
            .get_prompt_with_context(&request, Some(&builtin_context))
            .await
        {
            return result.map_err(|e| McpError::internal_error(e.to_string(), None));
        }
    }

    let server_filter = surface_entry.source_server_id;
    let lookup_name = surface_entry.upstream_key;
    let server_name: String = sqlx::query_scalar("SELECT name FROM server_config WHERE id = ?")
        .bind(&server_filter)
        .fetch_one(
            &server
                .database
                .as_ref()
                .expect("database required by Surface reader")
                .pool,
        )
        .await
        .map_err(|error| McpError::internal_error(format!("Failed to resolve pinned prompt source: {error}"), None))?;
    let canonical_name = request.name.clone();
    let mut filter = HashSet::new();
    filter.insert(server_filter.clone());
    let prompt_mapping =
        crate::core::capability::facade::build_prompt_mapping_filtered(&server.connection_pool, Some(&filter))
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    if requires_live_prompt_mapping(Some(&server_filter)) && !prompt_mapping.contains_key(&lookup_name) {
        return Err(McpError::invalid_params(
            format!(
                "Prompt '{}' is not available from its routed upstream server",
                canonical_name
            ),
            None,
        ));
    }

    let connection_selection = client.connection_selection(server_filter.clone());

    match crate::core::capability::facade::get_upstream_prompt(
        &server.connection_pool,
        &prompt_mapping,
        &lookup_name,
        request.arguments,
        Some(&server_filter),
        connection_selection.as_ref(),
    )
    .await
    {
        Ok(mut result) => {
            let database = server.database.as_ref().ok_or_else(|| {
                McpError::internal_error("Prompt result projection requires registry metadata".to_string(), None)
            })?;
            crate::core::capability::resource_uri::rewrite_get_prompt_result(
                &database.pool,
                &server_filter,
                &server_name,
                &mut result,
            )
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            Ok(result)
        }
        Err(e) => {
            tracing::error!("Failed to get prompt '{}': {}", request.name, e);
            if let Some(database) = server.database.as_ref() {
                crate::core::capability::runtime::record_capability_usage_evidence(
                    database,
                    &server_filter,
                    mcpmate_capability_store::CapabilityKind::Prompts,
                    None,
                    &e,
                )
                .await;
            }
            Err(McpError::internal_error(e.to_string(), None))
        }
    }
}

fn requires_live_prompt_mapping(target_server_id: Option<&str>) -> bool {
    target_server_id.is_none()
}

#[cfg(test)]
mod tests {
    use super::requires_live_prompt_mapping;

    #[test]
    fn pinned_prompt_route_does_not_require_a_warm_mapping() {
        assert!(!requires_live_prompt_mapping(Some("server-a-id")));
        assert!(requires_live_prompt_mapping(None));
    }
}
