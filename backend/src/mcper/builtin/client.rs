use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context, Result, anyhow};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, GetPromptRequestParams, GetPromptResult, Prompt, PromptMessage, Role, Tool,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    clients::{
        models::{CapabilitySource, UnifyDirectExposureConfig},
        service::ClientConfigService,
    },
    config::{database::Database, profile},
    core::{pool::UpstreamConnectionPool, profile::publication::PublishedWorkflowDirectSet},
};

use super::{
    broker::profile_tool_descriptions,
    helpers::load_profile_detail_components,
    names::{
        MCPMATE_CLIENT_CUSTOM_PROFILE_DETAILS_TOOL, MCPMATE_PROFILE_ADD_TOOL, MCPMATE_PROFILE_GET_TOOL,
        MCPMATE_PROFILE_REMOVE_TOOL, MCPMATE_PROFILE_SET_TOOL,
    },
    registry::BuiltinService,
    types::{PromptDetail, ResourceDetail, ServerDetail, ToolDetail},
};

#[derive(Debug, Clone)]
pub struct ClientBuiltinContext {
    pub client_id: String,
    pub session_id: Option<String>,
    pub config_mode: Option<String>,
    pub capability_source: CapabilitySource,
    pub selected_profile_ids: Vec<String>,
    pub custom_profile_id: Option<String>,
    pub unify_workspace: Option<UnifyDirectExposureConfig>,
    pub workflow_direct: PublishedWorkflowDirectSet,
}

pub struct ClientService {
    database: Arc<Database>,
    _connection_pool: Arc<Mutex<UpstreamConnectionPool>>,
    client_config_service: Arc<ClientConfigService>,
}

impl ClientService {
    pub fn new(
        database: Arc<Database>,
        connection_pool: Arc<Mutex<UpstreamConnectionPool>>,
        client_config_service: Arc<ClientConfigService>,
    ) -> Self {
        Self {
            database,
            _connection_pool: connection_pool,
            client_config_service,
        }
    }

    async fn profile_get(
        &self,
        context: &ClientBuiltinContext,
    ) -> Result<CallToolResult> {
        let source_revision_set = self
            .client_config_service
            .catalog_revision_set()
            .await
            .map_err(|error| anyhow!(error.to_string()))?
            .into_iter()
            .collect();
        let result = ClientConfigurationResponse {
            client_id: context.client_id.clone(),
            config_mode: context.config_mode.clone().unwrap_or_else(|| "unify".to_string()),
            capability_source: context.capability_source.as_str().to_string(),
            selected_profile_ids: if matches!(context.capability_source, CapabilitySource::Profiles) {
                Some(context.selected_profile_ids.clone())
            } else {
                None
            },
            custom_profile_id: if matches!(context.capability_source, CapabilitySource::Custom) {
                context.custom_profile_id.clone()
            } else {
                None
            },
            source_revision_set,
        };

        Ok(CallToolResult::success(vec![rmcp::model::ContentBlock::text(
            serde_json::to_string_pretty(&result).context("Failed to serialize client configuration response")?,
        )]))
    }

    async fn profile_set(
        &self,
        context: &ClientBuiltinContext,
        profile_ids: Vec<String>,
        source_revision_set: BTreeMap<String, i64>,
    ) -> Result<CallToolResult> {
        if !matches!(context.capability_source, CapabilitySource::Profiles) {
            return Err(anyhow!(
                "profile_set is only available for clients using 'profiles' capability source. Current source: {}",
                context.capability_source.as_str()
            ));
        }

        let selected_profile_ids = self
            .client_config_service
            .update_capability_config_and_invalidate(
                &context.client_id,
                CapabilitySource::Profiles,
                profile_ids,
                source_revision_set.into_iter().collect(),
            )
            .await
            .map_err(|error| anyhow!(error.to_string()))?
            .selected_profile_ids;

        Ok(CallToolResult::success(vec![rmcp::model::ContentBlock::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "success": true,
                    "message": format!(
                        "Updated the active profile selection to {} selected profile(s) for '{}'",
                        selected_profile_ids.len(),
                        context.client_id
                    ),
                    "client_id": context.client_id,
                    "selected_profile_ids": selected_profile_ids,
                    "refresh_required": true,
                    "refresh_hint": "If your client does not refresh tools automatically after MCP notifications, re-fetch tools/list now.",
                }))
            .context("Failed to serialize response")?,
        )]))
    }

    async fn profile_add(
        &self,
        context: &ClientBuiltinContext,
        profile_ids: Vec<String>,
        source_revision_set: BTreeMap<String, i64>,
    ) -> Result<CallToolResult> {
        if !matches!(context.capability_source, CapabilitySource::Profiles) {
            return Err(anyhow!(
                "profile_add is only available for clients using 'profiles' capability source. Current source: {}",
                context.capability_source.as_str()
            ));
        }

        let mut merged = context.selected_profile_ids.clone();
        merged.extend(profile_ids);
        merged.sort();
        merged.dedup();

        let selected_profile_ids = self
            .client_config_service
            .update_capability_config_and_invalidate(
                &context.client_id,
                CapabilitySource::Profiles,
                merged,
                source_revision_set.into_iter().collect(),
            )
            .await
            .map_err(|error| anyhow!(error.to_string()))?
            .selected_profile_ids;

        Ok(CallToolResult::success(vec![rmcp::model::ContentBlock::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "success": true,
                    "message": format!(
                        "Added profile(s) to the active profile selection. '{}' now has {} selected profile(s)",
                        context.client_id,
                        selected_profile_ids.len()
                    ),
                    "client_id": context.client_id,
                    "selected_profile_ids": selected_profile_ids,
                    "refresh_required": true,
                "refresh_hint": "If your client does not refresh tools automatically after MCP notifications, re-fetch tools/list now.",
            }))
            .context("Failed to serialize response")?,
        )]))
    }

    async fn profile_remove(
        &self,
        context: &ClientBuiltinContext,
        profile_ids: Vec<String>,
        source_revision_set: BTreeMap<String, i64>,
    ) -> Result<CallToolResult> {
        if !matches!(context.capability_source, CapabilitySource::Profiles) {
            return Err(anyhow!(
                "profile_remove is only available for clients using 'profiles' capability source. Current source: {}",
                context.capability_source.as_str()
            ));
        }

        let remaining = context
            .selected_profile_ids
            .iter()
            .filter(|current| !profile_ids.contains(current))
            .cloned()
            .collect::<Vec<_>>();

        let selected_profile_ids = self
            .client_config_service
            .update_capability_config_and_invalidate(
                &context.client_id,
                CapabilitySource::Profiles,
                remaining,
                source_revision_set.into_iter().collect(),
            )
            .await
            .map_err(|error| anyhow!(error.to_string()))?
            .selected_profile_ids;

        Ok(CallToolResult::success(vec![rmcp::model::ContentBlock::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "success": true,
                    "message": format!(
                        "Removed profile(s) from the active profile selection without deleting profile definitions. '{}' now has {} selected profile(s)",
                        context.client_id,
                        selected_profile_ids.len()
                    ),
                    "client_id": context.client_id,
                    "selected_profile_ids": selected_profile_ids,
                    "refresh_required": true,
                "refresh_hint": "If your client does not refresh tools automatically after MCP notifications, re-fetch tools/list now.",
            }))
            .context("Failed to serialize response")?,
        )]))
    }

    async fn client_custom_profile_details(
        &self,
        context: &ClientBuiltinContext,
    ) -> Result<CallToolResult> {
        if !matches!(context.capability_source, CapabilitySource::Custom) {
            return Err(anyhow!(
                "client_custom_profile_details is only available for clients using 'custom' capability source. Current source: {}",
                context.capability_source.as_str()
            ));
        }

        let Some(profile_id) = &context.custom_profile_id else {
            return Err(anyhow!(
                "Client '{}' does not have a custom profile provisioned. Save the capability config first to create one.",
                context.client_id
            ));
        };

        let profile = profile::get_profile(&self.database.pool, profile_id)
            .await
            .context("Failed to get custom profile")?
            .ok_or_else(|| anyhow!("Custom profile '{}' not found", profile_id))?;

        let detail_components = load_profile_detail_components(&self.database.pool, profile_id).await?;

        let details = CustomProfileDetails {
            client_id: context.client_id.clone(),
            profile_id: profile_id.clone(),
            profile_name: profile.name,
            description: profile.description,
            servers: detail_components.servers,
            tools: detail_components.tools,
            prompts: detail_components.prompts,
            resources: detail_components.resources,
        };

        Ok(CallToolResult::success(vec![rmcp::model::ContentBlock::text(
            serde_json::to_string_pretty(&details).context("Failed to serialize custom profile details")?,
        )]))
    }

    fn unify_mode_guide_prompt() -> Prompt {
        Prompt::new(
            "mcpmate_unify_mode_guide",
            Some("Explain how Unify Mode works and when to use builtin UCAN tools."),
            None,
        )
    }

    fn unify_mode_next_actions_prompt() -> Prompt {
        Prompt::new(
            "mcpmate_unify_mode_next_actions",
            Some("Summarize the current Unify Mode state and recommend the next builtin UCAN tool to call."),
            None,
        )
    }

    fn build_unify_mode_guide(
        &self,
        context: &ClientBuiltinContext,
    ) -> GetPromptResult {
        let content = format!(
            concat!(
                "You are helping a user operate MCPMate Unify Mode for client '{client_id}'.\n\n",
                "Unify Mode concepts:\n",
                "1. Unify Mode is session-scoped and starts with builtin MCP control-plane tools only.\n",
                "2. Unify Mode uses globally enabled servers rather than profile selection.\n",
                "3. There is no second-level profile selector in the UI for Unify Mode.\n\n",
                "Tool guidance:\n",
                "- Use mcpmate_ucan_catalog to browse the MCP surface from globally enabled servers.\n",
                "- Use mcpmate_ucan_details to inspect one surface item before calling or selecting it.\n",
                "- Use mcpmate_ucan_call to invoke a callable surface item through Unify Mode.\n",
                "- If the user needs durable or profile-scoped selection, move to Hosted or Transparent mode instead.\n\n",
                "After any tool that changes surface visibility, if the client does not refresh tools automatically, ask it to re-fetch tools/list."
            ),
            client_id = context.client_id,
        );

        GetPromptResult::new(vec![PromptMessage::new_text(Role::User, content)])
            .with_description("Unify Mode guide for the current client")
    }

    fn build_unify_mode_next_actions(
        &self,
        context: &ClientBuiltinContext,
    ) -> GetPromptResult {
        let next_action = match context.capability_source {
            CapabilitySource::Activated => {
                "Use mcpmate_ucan_catalog to browse the current MCP surface from globally enabled servers."
            }
            CapabilitySource::Profiles => {
                "Use mcpmate_ucan_catalog and mcpmate_ucan_details to inspect surface items before calling or selecting them."
            }
            CapabilitySource::Custom => {
                "Unify Mode does not use profile-scoped overlays. Prefer Hosted or Transparent mode for profile selection."
            }
        };

        let content = format!(
            concat!(
                "Client: {client_id}\n",
                "Mode: Unify\n",
                "Capability source: {capability_source}\n\n",
                "Recommended next action:\n",
                "{next_action}\n\n",
                "If the user asks what is available, start with mcpmate_ucan_catalog.\n",
                "If the user asks for one specific tool, inspect it with mcpmate_ucan_details before calling when needed.\n",
                "If a tool changes visible surface items, ask the client to re-fetch tools/list when auto-refresh is not reliable.\n",
                "Unify Mode resets when the MCP session ends; promote to Hosted if the user wants durable profile-based behavior."
            ),
            client_id = context.client_id,
            capability_source = context.capability_source.as_str(),
            next_action = next_action,
        );

        GetPromptResult::new(vec![PromptMessage::new_text(Role::User, content)])
            .with_description("Recommended Unify Mode next actions for the current client")
    }
}

#[async_trait::async_trait]
impl BuiltinService for ClientService {
    fn name(&self) -> &'static str {
        "mcpmate_client"
    }

    fn tools(&self) -> Vec<Tool> {
        let descriptions = profile_tool_descriptions();
        vec![
            Tool::new(
                MCPMATE_PROFILE_GET_TOOL,
                descriptions.get,
                std::sync::Arc::new(
                    serde_json::json!({
                        "type": "object",
                        "properties": {},
                        "required": []
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
            ),
            Tool::new(
                MCPMATE_PROFILE_SET_TOOL,
                descriptions.set,
                std::sync::Arc::new(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "profile_ids": {
                                "type": "array",
                                "items": { "type": "string" },
                                "minItems": 1,
                                "uniqueItems": true,
                                "description": "Shared profile IDs to keep as the active surface selection. Replaces the current selection entirely. Use mcpmate_ucan_catalog(kind_filter=[\"profile\"]) to discover available IDs."
                            },
                            "source_revision_set": {
                                "type": "object",
                                "additionalProperties": { "type": "integer" },
                                "description": "Exact source_revision_set returned by the preceding mcpmate_profile_get call."
                            }
                        },
                        "required": ["profile_ids", "source_revision_set"]
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
            ),
            Tool::new(
                MCPMATE_PROFILE_ADD_TOOL,
                descriptions.add,
                std::sync::Arc::new(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "profile_ids": {
                                "type": "array",
                                "items": { "type": "string" },
                                "minItems": 1,
                                "uniqueItems": true,
                                "description": "Shared profile IDs to add to the active selection. Existing selection is preserved. Duplicate IDs are ignored."
                            },
                            "source_revision_set": {
                                "type": "object",
                                "additionalProperties": { "type": "integer" },
                                "description": "Exact source_revision_set returned by the preceding mcpmate_profile_get call."
                            }
                        },
                        "required": ["profile_ids", "source_revision_set"]
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
            ),
            Tool::new(
                MCPMATE_PROFILE_REMOVE_TOOL,
                descriptions.remove,
                std::sync::Arc::new(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "profile_ids": {
                                "type": "array",
                                "items": { "type": "string" },
                                "minItems": 1,
                                "uniqueItems": true,
                                "description": "Shared profile IDs to remove from the active selection. IDs not in the current selection are silently skipped. Profile definitions are never deleted."
                            },
                            "source_revision_set": {
                                "type": "object",
                                "additionalProperties": { "type": "integer" },
                                "description": "Exact source_revision_set returned by the preceding mcpmate_profile_get call."
                            }
                        },
                        "required": ["profile_ids", "source_revision_set"]
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
            ),
            Tool::new(
                MCPMATE_CLIENT_CUSTOM_PROFILE_DETAILS_TOOL,
                "Get custom profile details: servers, tools, prompts, resources (custom mode only)",
                std::sync::Arc::new(
                    serde_json::json!({
                        "type": "object",
                        "properties": {},
                        "required": []
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
            ),
        ]
    }

    fn prompts(&self) -> Vec<Prompt> {
        vec![Self::unify_mode_guide_prompt(), Self::unify_mode_next_actions_prompt()]
    }

    async fn call_tool(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<CallToolResult> {
        match request.name.as_ref() {
            MCPMATE_PROFILE_GET_TOOL
            | MCPMATE_PROFILE_SET_TOOL
            | MCPMATE_PROFILE_ADD_TOOL
            | MCPMATE_PROFILE_REMOVE_TOOL
            | MCPMATE_CLIENT_CUSTOM_PROFILE_DETAILS_TOOL => Err(anyhow!(
                "Client-aware tool '{}' requires client context. Use call_tool_with_context instead.",
                request.name
            )),
            _ => Err(anyhow!("Unknown tool: {}", request.name)),
        }
    }

    async fn call_tool_with_context(
        &self,
        request: &CallToolRequestParams,
        context: Option<&ClientBuiltinContext>,
    ) -> Result<CallToolResult> {
        match request.name.as_ref() {
            MCPMATE_PROFILE_GET_TOOL => {
                let ctx = require_client_context(context, MCPMATE_PROFILE_GET_TOOL)?;
                self.profile_get(ctx).await
            }
            MCPMATE_PROFILE_SET_TOOL => {
                let ctx = require_client_context(context, MCPMATE_PROFILE_SET_TOOL)?;
                let params = parse_profiles_select_params(request, "Invalid parameters for profile_set")?;
                self.profile_set(ctx, params.profile_ids, params.source_revision_set)
                    .await
            }
            MCPMATE_PROFILE_ADD_TOOL => {
                let ctx = require_client_context(context, MCPMATE_PROFILE_ADD_TOOL)?;
                let params = parse_profiles_select_params(request, "Invalid parameters for profile_add")?;
                self.profile_add(ctx, params.profile_ids, params.source_revision_set)
                    .await
            }
            MCPMATE_PROFILE_REMOVE_TOOL => {
                let ctx = require_client_context(context, MCPMATE_PROFILE_REMOVE_TOOL)?;
                let params = parse_profiles_select_params(request, "Invalid parameters for profile_remove")?;
                self.profile_remove(ctx, params.profile_ids, params.source_revision_set)
                    .await
            }
            MCPMATE_CLIENT_CUSTOM_PROFILE_DETAILS_TOOL => {
                let ctx = require_client_context(context, MCPMATE_CLIENT_CUSTOM_PROFILE_DETAILS_TOOL)?;
                self.client_custom_profile_details(ctx).await
            }
            _ => Err(anyhow!("Unknown tool: {}", request.name)),
        }
    }

    async fn get_prompt_with_context(
        &self,
        request: &GetPromptRequestParams,
        context: Option<&ClientBuiltinContext>,
    ) -> Result<GetPromptResult> {
        let ctx = context.ok_or_else(|| anyhow!("Client context required for builtin Unify Mode prompts"))?;

        match request.name.as_ref() {
            "mcpmate_unify_mode_guide" => Ok(self.build_unify_mode_guide(ctx)),
            "mcpmate_unify_mode_next_actions" => Ok(self.build_unify_mode_next_actions(ctx)),
            _ => Err(anyhow!("Unknown prompt: {}", request.name)),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ProfilesSelectParams {
    profile_ids: Vec<String>,
    source_revision_set: BTreeMap<String, i64>,
}

fn require_client_context<'a>(
    context: Option<&'a ClientBuiltinContext>,
    tool_name: &str,
) -> Result<&'a ClientBuiltinContext> {
    context.ok_or_else(|| anyhow!("Client context required for {}", tool_name))
}

fn parse_profiles_select_params(
    request: &CallToolRequestParams,
    error_message: &'static str,
) -> Result<ProfilesSelectParams> {
    let args = serde_json::Value::Object(request.arguments.clone().unwrap_or_default());
    serde_json::from_value(args).context(error_message)
}

#[derive(Debug, Serialize)]
struct ClientConfigurationResponse {
    client_id: String,
    config_mode: String,
    capability_source: String,
    selected_profile_ids: Option<Vec<String>>,
    custom_profile_id: Option<String>,
    source_revision_set: BTreeMap<String, i64>,
}

#[derive(Debug, Serialize)]
struct CustomProfileDetails {
    client_id: String,
    profile_id: String,
    profile_name: String,
    description: Option<String>,
    servers: Vec<ServerDetail>,
    tools: Vec<ToolDetail>,
    prompts: Vec<PromptDetail>,
    resources: Vec<ResourceDetail>,
}
