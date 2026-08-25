//! Token estimation utilities for MCP capabilities.
//!
//! Provides approximate token counting for MCP tools, prompts, resources,
//! and resource templates using a simple chars/4 heuristic.
//!
//! This module is used to estimate token savings when MCPMate filters
//! capabilities based on profiles and client configurations.

use std::sync::Arc;

use rmcp::model::{Prompt, PromptArgument, Resource, ResourceTemplate, Tool};
use serde::Serialize;

use crate::clients::models::CapabilitySource;
use crate::mcper::{
    MCPMATE_PROFILE_ADD_TOOL, MCPMATE_PROFILE_GET_TOOL, MCPMATE_PROFILE_REMOVE_TOOL, MCPMATE_PROFILE_SET_TOOL,
    MCPMATE_UCAN_CALL_TOOL, MCPMATE_UCAN_CATALOG_TOOL, MCPMATE_UCAN_DETAILS_TOOL,
};

/// Result of builtin overhead calculation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinOverhead {
    /// Estimated token count for builtin tools.
    pub tokens: u32,
    /// Number of builtin tools.
    pub tool_count: u32,
    /// Capability source mode name.
    pub mode: String,
}

/// Estimate token count from a JSON string using chars/4 approximation.
///
/// This is a simple heuristic that assumes approximately 4 characters per token,
/// which works reasonably well for English text and JSON structures.
///
/// # Arguments
/// * `json_str` - JSON string to estimate tokens for.
///
/// # Returns
/// Estimated token count (chars.len() / 4).
pub fn estimate_capability_tokens(json_str: &str) -> u32 {
    (json_str.chars().count() / 4) as u32
}

fn estimate_json_tokens<T: Serialize>(value: &T) -> u32 {
    serde_json::to_string(value)
        .map(|json| estimate_capability_tokens(&json))
        .unwrap_or(0)
}

/// Estimate token count for a tool definition.
///
/// Serializes the tool's name, description, and input_schema to JSON,
/// then estimates tokens using the chars/4 heuristic.
///
/// # Arguments
/// * `tool` - The tool to estimate.
///
/// # Returns
/// Estimated token count for the tool definition.
pub fn estimate_tool_tokens(tool: &Tool) -> u32 {
    #[derive(Serialize)]
    struct ToolForEstimate<'a> {
        name: &'a str,
        description: &'a Option<std::borrow::Cow<'a, str>>,
        input_schema: &'a Arc<rmcp::model::JsonObject>,
    }

    let estimate = ToolForEstimate {
        name: &tool.name,
        description: &tool.description,
        input_schema: &tool.input_schema,
    };

    estimate_json_tokens(&estimate)
}

/// Estimate token count for a prompt definition.
///
/// Serializes the prompt's name, description, and arguments to JSON,
/// then estimates tokens using the chars/4 heuristic.
///
/// # Arguments
/// * `prompt` - The prompt to estimate.
///
/// # Returns
/// Estimated token count for the prompt definition.
pub fn estimate_prompt_tokens(prompt: &Prompt) -> u32 {
    #[derive(Serialize)]
    struct PromptForEstimate<'a> {
        name: &'a str,
        description: &'a Option<String>,
        arguments: &'a Option<Vec<PromptArgument>>,
    }

    let estimate = PromptForEstimate {
        name: &prompt.name,
        description: &prompt.description,
        arguments: &prompt.arguments,
    };

    estimate_json_tokens(&estimate)
}

/// Estimate token count for a resource definition.
///
/// Serializes the resource's uri, name, description, and mime_type to JSON,
/// then estimates tokens using the chars/4 heuristic.
///
/// # Arguments
/// * `resource` - The resource to estimate.
///
/// # Returns
/// Estimated token count for the resource definition.
pub fn estimate_resource_tokens(resource: &Resource) -> u32 {
    #[derive(Serialize)]
    struct ResourceForEstimate<'a> {
        uri: &'a str,
        name: &'a str,
        description: &'a Option<String>,
        mime_type: &'a Option<String>,
    }

    let estimate = ResourceForEstimate {
        uri: &resource.uri,
        name: &resource.name,
        description: &resource.description,
        mime_type: &resource.mime_type,
    };

    estimate_json_tokens(&estimate)
}

/// Estimate token count for a resource template definition.
///
/// Serializes the template's uri_template, name, description, and mime_type to JSON,
/// then estimates tokens using the chars/4 heuristic.
///
/// # Arguments
/// * `template` - The resource template to estimate.
///
/// # Returns
/// Estimated token count for the resource template definition.
pub fn estimate_resource_template_tokens(template: &ResourceTemplate) -> u32 {
    #[derive(Serialize)]
    struct TemplateForEstimate<'a> {
        uri_template: &'a str,
        name: &'a str,
        description: &'a Option<String>,
        mime_type: &'a Option<String>,
    }

    let estimate = TemplateForEstimate {
        uri_template: &template.uri_template,
        name: &template.name,
        description: &template.description,
        mime_type: &template.mime_type,
    };

    estimate_json_tokens(&estimate)
}

/// Calculate builtin tool overhead for a given capability source mode.
///
/// # Arguments
/// * `capability_source` - The capability source mode.
/// * `_has_custom_profile` - Kept for API compatibility; current runtime builtins are source-gated.
///
/// # Returns
/// `BuiltinOverhead` with token count, tool count, and mode name.
pub fn calculate_builtin_overhead(
    capability_source: &CapabilitySource,
    _has_custom_profile: bool,
) -> BuiltinOverhead {
    let tools = create_builtin_tools_for_mode(capability_source);
    let tokens: u32 = tools.iter().map(estimate_tool_tokens).sum();
    let tool_count = tools.len() as u32;

    BuiltinOverhead {
        tokens,
        tool_count,
        mode: capability_source.as_str().to_string(),
    }
}

/// Create builtin tools for a given capability source mode.
fn create_builtin_tools_for_mode(capability_source: &CapabilitySource) -> Vec<Tool> {
    match capability_source {
        CapabilitySource::Activated => Vec::new(),
        CapabilitySource::Profiles => vec![
            create_ucan_catalog_tool(),
            create_ucan_details_tool(),
            create_ucan_call_tool(),
            create_profile_get_tool(),
            create_profile_set_tool(),
            create_profile_add_tool(),
            create_profile_remove_tool(),
        ],
        CapabilitySource::Custom => Vec::new(),
    }
}

// --- Builtin tool definitions ---

fn create_ucan_catalog_tool() -> Tool {
    Tool::new(
        MCPMATE_UCAN_CATALOG_TOOL,
        "MCPMATE SURFACE DIRECTORY. Discover the current MCP surface before calling or selecting anything.",
        Arc::new(
            serde_json::json!({
                "type": "object",
                "properties": {
                    "page": { "type": "integer", "minimum": 1 },
                    "page_size": { "type": "integer", "minimum": 1, "maximum": 50 },
                    "search": { "type": "string" },
                    "kind_filter": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["tool", "prompt", "resource", "resource_template", "profile"] }
                    }
                },
                "required": []
            })
            .as_object()
            .unwrap()
            .clone(),
        ),
    )
}

fn create_ucan_details_tool() -> Tool {
    Tool::new(
        MCPMATE_UCAN_DETAILS_TOOL,
        "MCPMATE SURFACE INSPECTOR. Inspect one MCP surface item before calling or selecting it.",
        Arc::new(
            serde_json::json!({
                "type": "object",
                "properties": {
                    "capability_kind": { "type": "string", "enum": ["tool", "prompt", "resource", "resource_template", "profile"] },
                    "capability_name": { "type": "string" },
                    "detail_level": { "type": "string", "enum": ["summary", "full"] }
                },
                "required": ["capability_kind", "capability_name"]
            })
            .as_object()
            .unwrap()
            .clone(),
        ),
    )
}

fn create_ucan_call_tool() -> Tool {
    Tool::new(
        MCPMATE_UCAN_CALL_TOOL,
        "MCPMATE SURFACE CALLER. Call one callable MCP surface item selected from the directory and verified by details.",
        Arc::new(
            serde_json::json!({
                "type": "object",
                "properties": {
                    "capability_kind": { "type": "string", "enum": ["tool", "prompt", "resource", "resource_template", "profile"] },
                    "capability_name": { "type": "string" },
                    "arguments": { "type": "object" }
                },
                "required": ["capability_kind", "capability_name"]
            })
            .as_object()
            .unwrap()
            .clone(),
        ),
    )
}

fn create_profile_get_tool() -> Tool {
    Tool::new(
        MCPMATE_PROFILE_GET_TOOL,
        "Get the current effective scope for this client, including mode, source, selected profiles, and custom profile ID when present.",
        Arc::new(
            serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            })
            .as_object()
            .unwrap()
            .clone(),
        ),
    )
}

fn create_profile_set_tool() -> Tool {
    Tool::new(
        MCPMATE_PROFILE_SET_TOOL,
        "Replace the effective profile scope with an exact list of shared profiles. Use this to switch to a single scene or exact set.",
        Arc::new(
            serde_json::json!({
                "type": "object",
                "properties": {
                    "profile_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Shared profile IDs to keep in the effective profile scope"
                    }
                },
                "required": ["profile_ids"]
            })
            .as_object()
            .unwrap()
            .clone(),
        ),
    )
}

fn create_profile_add_tool() -> Tool {
    Tool::new(
        MCPMATE_PROFILE_ADD_TOOL,
        "Add shared profiles to the effective profile scope without replacing the existing selection.",
        Arc::new(
            serde_json::json!({
                "type": "object",
                "properties": {
                    "profile_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Shared profile IDs to add to the effective profile scope"
                    }
                },
                "required": ["profile_ids"]
            })
            .as_object()
            .unwrap()
            .clone(),
        ),
    )
}

fn create_profile_remove_tool() -> Tool {
    Tool::new(
        MCPMATE_PROFILE_REMOVE_TOOL,
        "Remove shared profiles from the effective profile scope without deleting the profile definitions themselves.",
        Arc::new(
            serde_json::json!({
                "type": "object",
                "properties": {
                    "profile_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Shared profile IDs to remove from the effective profile scope"
                    }
                },
                "required": ["profile_ids"]
            })
            .as_object()
            .unwrap()
            .clone(),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_capability_tokens_empty_string() {
        assert_eq!(estimate_capability_tokens(""), 0);
    }

    #[test]
    fn test_estimate_capability_tokens_simple_json() {
        // 15 chars / 4 = 3 tokens (integer division)
        let json = r#"{"name":"test"}"#;
        assert_eq!(estimate_capability_tokens(json), 3);
    }

    #[test]
    fn test_estimate_capability_tokens_longer_json() {
        // 79 chars / 4 = 19 tokens (integer division)
        let json = r#"{"name":"test_tool","description":"A test tool for testing","input_schema":{}}"#;
        assert_eq!(estimate_capability_tokens(json), 19);
    }

    #[test]
    fn test_estimate_tool_tokens_known_tool() {
        let tool = create_ucan_catalog_tool();
        let tokens = estimate_tool_tokens(&tool);

        assert!(tokens > 0, "Tool tokens should be positive");
        assert!(tokens < 150, "Tool tokens should be reasonable (< 150)");
    }

    #[test]
    fn test_estimate_tool_tokens_complex_schema() {
        let tool = create_profile_set_tool();
        let tokens = estimate_tool_tokens(&tool);

        // Profile set tool has a complex schema with array property
        assert!(tokens > 0, "Tool tokens should be positive");
        assert!(tokens > 30, "Complex tool should have more tokens than simple tool");
    }

    #[test]
    fn test_estimate_prompt_tokens() {
        let prompt = Prompt::new(
            "test_prompt",
            Some("A test prompt for testing"),
            Some(vec![
                PromptArgument::new("arg1")
                    .with_description("First argument")
                    .with_required(true),
            ]),
        );

        let tokens = estimate_prompt_tokens(&prompt);
        assert!(tokens > 0, "Prompt tokens should be positive");
    }

    #[test]
    fn test_estimate_resource_tokens() {
        let resource = Resource::new("file:///test/resource.txt", "Test Resource")
            .with_description("A test resource")
            .with_mime_type("text/plain");

        let tokens = estimate_resource_tokens(&resource);
        assert!(tokens > 0, "Resource tokens should be positive");
    }

    #[test]
    fn test_estimate_resource_template_tokens() {
        let template = ResourceTemplate::new("file:///test/{path}", "Test Template")
            .with_description("A test template")
            .with_mime_type("text/plain");

        let tokens = estimate_resource_template_tokens(&template);
        assert!(tokens > 0, "Resource template tokens should be positive");
    }

    #[test]
    fn test_builtin_overhead_activated_mode() {
        let overhead = calculate_builtin_overhead(&CapabilitySource::Activated, false);

        assert_eq!(overhead.tool_count, 0, "Activated mode should have 0 tools");
        assert_eq!(overhead.mode, "activated");
        assert_eq!(overhead.tokens, 0, "Activated mode should have no builtin overhead");
    }

    #[test]
    fn test_builtin_overhead_profiles_mode_without_custom() {
        let overhead = calculate_builtin_overhead(&CapabilitySource::Profiles, false);

        assert_eq!(
            overhead.tool_count, 7,
            "Profiles mode without custom profile should have 7 tools"
        );
        assert_eq!(overhead.mode, "profiles");
        assert!(overhead.tokens > 0, "Token count should be positive");
    }

    #[test]
    fn test_builtin_overhead_profiles_mode_with_custom() {
        let overhead = calculate_builtin_overhead(&CapabilitySource::Profiles, true);

        assert_eq!(
            overhead.tool_count, 7,
            "Profiles mode with custom profile should still have 7 tools"
        );
        assert_eq!(overhead.mode, "profiles");
        assert!(overhead.tokens > 0, "Token count should be positive");
    }

    #[test]
    fn test_builtin_overhead_custom_mode() {
        let overhead = calculate_builtin_overhead(&CapabilitySource::Custom, false);

        assert_eq!(
            overhead.tool_count, 0,
            "Custom mode without custom profile should have 0 tools"
        );
        assert_eq!(overhead.mode, "custom");
        assert_eq!(
            overhead.tokens, 0,
            "Custom mode without custom profile should have no builtin overhead"
        );
    }

    #[test]
    fn test_builtin_overhead_token_consistency() {
        // Verify that more tools = more tokens
        let activated = calculate_builtin_overhead(&CapabilitySource::Activated, false);
        let profiles_no_custom = calculate_builtin_overhead(&CapabilitySource::Profiles, false);
        let profiles_with_custom = calculate_builtin_overhead(&CapabilitySource::Profiles, true);
        let custom = calculate_builtin_overhead(&CapabilitySource::Custom, false);

        assert!(
            activated.tokens < profiles_no_custom.tokens,
            "Activated should have fewer tokens than Profiles without custom"
        );
        assert_eq!(
            profiles_no_custom.tokens, profiles_with_custom.tokens,
            "Profiles with and without custom profile should have the same builtin overhead"
        );
        assert!(
            custom.tokens < profiles_with_custom.tokens,
            "Custom should have fewer tokens than Profiles"
        );
    }

    #[test]
    fn test_tool_token_estimation_range() {
        // Test that all builtin tools fall within expected token ranges
        let tools = vec![
            create_ucan_catalog_tool(),
            create_ucan_details_tool(),
            create_ucan_call_tool(),
            create_profile_get_tool(),
            create_profile_set_tool(),
            create_profile_add_tool(),
            create_profile_remove_tool(),
        ];

        for tool in &tools {
            let tokens = estimate_tool_tokens(tool);
            assert!(
                (20..=150).contains(&tokens),
                "Tool {} has {} tokens, expected 20-150",
                tool.name,
                tokens
            );
        }
    }
}
