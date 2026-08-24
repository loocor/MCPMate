use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
use once_cell::sync::Lazy;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, ContentBlock, Resource, ResourceTemplate,
    Tool,
};
use rmcp::service::{PeerRequestOptions, ServiceError};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::config::database::Database;
use crate::core::capability::aggregate::{AggregateListCompletionError, AggregateListStatus};
use crate::core::capability::naming::{NamingKind, resolve_capability_route};
use crate::core::foundation::types::ConnectionStatus;
use crate::core::pool::UpstreamConnectionPool;
use crate::core::profile::visibility::{ProfileVisibilityService, VisibilitySnapshot};
use crate::core::proxy::server::{ClientContext, ClientIdentitySource, ClientTransport};

use super::{
    ClientBuiltinContext,
    helpers::{ProfileCapabilityCounts, load_profile_capability_counts, load_profile_detail_components},
    names::{MCPMATE_UCAN_CALL_TOOL, MCPMATE_UCAN_CATALOG_TOOL, MCPMATE_UCAN_DETAILS_TOOL},
    registry::BuiltinService,
};
use crate::clients::models::CapabilitySource;
use crate::common::profile::ProfileType;
use crate::config::profile as profile_repo;

#[derive(Debug, thiserror::Error)]
enum CatalogAuthorityError {
    #[error("The requested capability catalog is incomplete")]
    Incomplete,
}

fn ensure_catalog_result_is_authoritative(
    has_usable_entries: bool,
    kind_filter: Option<&[String]>,
    aggregates: &[(&str, &AggregateListStatus)],
) -> std::result::Result<(), CatalogAuthorityError> {
    if has_usable_entries {
        return Ok(());
    }

    let requested_kinds = kind_filter.map(|kinds| kinds.iter().map(String::as_str).collect::<HashSet<_>>());
    let incomplete = aggregates
        .iter()
        .filter(|(kind, _)| {
            requested_kinds
                .as_ref()
                .is_none_or(|requested| requested.contains(kind))
        })
        .any(|(_, aggregate)| aggregate.has_failures());

    if incomplete {
        return Err(CatalogAuthorityError::Incomplete);
    }
    Ok(())
}

pub(crate) fn is_catalog_authority_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<AggregateListCompletionError>().is_some()
        || error.downcast_ref::<CatalogAuthorityReadError>().is_some()
}

#[derive(Debug, thiserror::Error)]
#[error("Trusted resource catalog is unavailable for server '{server_name}' ({server_id}): {source}")]
struct CatalogAuthorityReadError {
    server_id: String,
    server_name: String,
    #[source]
    source: anyhow::Error,
}

fn catalog_authority_error_for_route(
    server_id: &str,
    server_name: &str,
    source: anyhow::Error,
) -> anyhow::Error {
    anyhow::Error::new(CatalogAuthorityReadError {
        server_id: server_id.to_string(),
        server_name: server_name.to_string(),
        source,
    })
}

/// Structured error response for UCAN tools, designed for LLM parsing and recovery.
#[derive(Debug, Clone, Serialize)]
pub struct UcanError {
    /// Error code for programmatic handling (e.g., "capability_not_found")
    pub error_code: String,
    /// Human-readable error message
    pub message: String,
    /// Actionable guidance for recovery
    pub recovery_hint: String,
    /// Similar capability names (fuzzy match suggestions)
    pub alternatives: Vec<String>,
    /// Whether retry makes sense
    pub retry_eligible: bool,
}

impl UcanError {
    /// Creates a catalog_incomplete error without exposing upstream failure details.
    pub fn catalog_incomplete() -> Self {
        Self {
            error_code: "catalog_incomplete".to_string(),
            message: "The current capability directory could not be completed because one or more visible servers are unavailable."
                .to_string(),
            recovery_hint: "Wait for the affected server to recover, then call mcpmate_ucan_catalog again."
                .to_string(),
            alternatives: Vec::new(),
            retry_eligible: true,
        }
    }

    /// Creates a capability_not_found error with fuzzy-matched alternatives.
    pub fn capability_not_found(
        capability_kind: &str,
        capability_name: &str,
        surface_names: &[String],
    ) -> Self {
        let alternatives = find_similar_names(capability_name, surface_names, 3);
        Self {
            error_code: "capability_not_found".to_string(),
            message: format!(
                "{} '{}' is not available in the current MCP surface.",
                capitalize_kind(capability_kind),
                capability_name
            ),
            recovery_hint: if alternatives.is_empty() {
                "Use mcpmate_ucan_catalog to list available surface items.".to_string()
            } else {
                "Check the 'alternatives' field for similar surface item names, or use mcpmate_ucan_catalog to browse the current MCP surface.".to_string()
            },
            alternatives,
            retry_eligible: false,
        }
    }

    /// Creates a server_unreachable error for connection failures.
    pub fn server_unreachable(
        server_id: &str,
        server_name: &str,
    ) -> Self {
        Self {
            error_code: "server_unreachable".to_string(),
            message: format!(
                "Server '{}' ({}) is not reachable or not ready.",
                server_name, server_id
            ),
            recovery_hint: "Check if the server process is running. Verify server configuration and network connectivity. The server may need time to start up.".to_string(),
            alternatives: Vec::new(),
            retry_eligible: true,
        }
    }

    /// Creates a visibility_denied error when capability is disabled.
    pub fn visibility_denied(
        capability_kind: &str,
        capability_name: &str,
    ) -> Self {
        Self {
            error_code: "visibility_denied".to_string(),
            message: format!(
                "{} '{}' is not available in the current MCP surface.",
                capitalize_kind(capability_kind),
                capability_name
            ),
            recovery_hint: "Re-run mcpmate_ucan_catalog to refresh visibility, then choose a surface item that appears in the latest directory.".to_string(),
            alternatives: Vec::new(),
            retry_eligible: false,
        }
    }

    /// Creates an invalid_parameters error.
    pub fn invalid_parameters(
        tool_name: &str,
        details: &str,
    ) -> Self {
        Self {
            error_code: "invalid_parameters".to_string(),
            message: format!("Invalid parameters for {}: {}", tool_name, details),
            recovery_hint: "Check the tool schema for required parameters and their types. Use mcpmate_ucan_details to inspect surface item schemas.".to_string(),
            alternatives: Vec::new(),
            retry_eligible: false,
        }
    }

    pub fn missing_required_arguments(
        capability_kind: &str,
        capability_name: &str,
        missing: &[String],
    ) -> Self {
        let missing_joined = missing.join(", ");
        Self {
            error_code: "missing_required_arguments".to_string(),
            message: format!(
                "{} '{}' is missing required arguments: {}",
                capitalize_kind(capability_kind),
                capability_name,
                missing_joined
            ),
            recovery_hint: "Call mcpmate_ucan_details with detail_level=full for this surface item, then retry mcpmate_ucan_call with all required arguments.".to_string(),
            alternatives: Vec::new(),
            retry_eligible: true,
        }
    }

    pub fn resource_standard_read_required(capability_kind: &str) -> Self {
        Self {
            error_code: "resource_standard_read_required".to_string(),
            message: format!(
                "{} targets must be read through standard resources/read.",
                capitalize_kind(capability_kind)
            ),
            recovery_hint: standard_read_recovery_hint(),
            alternatives: Vec::new(),
            retry_eligible: false,
        }
    }

    /// Creates an upstream_error for errors from the upstream MCP server.
    pub fn upstream_error(
        capability_kind: &str,
        capability_name: &str,
    ) -> Self {
        Self {
            error_code: "upstream_error".to_string(),
            message: format!(
                "Upstream server returned an error for {} '{}'.",
                capability_kind, capability_name
            ),
            recovery_hint: "The upstream MCP server encountered an error. Check the server logs for details. Verify the selected surface item is correctly implemented and the arguments are valid.".to_string(),
            alternatives: Vec::new(),
            retry_eligible: false,
        }
    }

    /// Creates a timeout error for operation timeouts.
    pub fn timeout(
        capability_kind: &str,
        capability_name: &str,
        timeout_secs: u64,
    ) -> Self {
        Self {
            error_code: "timeout".to_string(),
            message: format!(
                "{} '{}' execution timed out after {} seconds.",
                capitalize_kind(capability_kind),
                capability_name,
                timeout_secs
            ),
            recovery_hint: "The operation took longer than the configured timeout. Consider increasing MCPMATE_TOOL_CALL_TIMEOUT_SECS environment variable, or check if the upstream server is responsive.".to_string(),
            alternatives: Vec::new(),
            retry_eligible: true,
        }
    }

    /// Creates a context_required error.
    pub fn context_required(tool_name: &str) -> Self {
        Self {
            error_code: "context_required".to_string(),
            message: format!("Tool '{}' requires client context.", tool_name),
            recovery_hint:
                "This tool must be called through a client session. Use call_tool_with_context instead of call_tool."
                    .to_string(),
            alternatives: Vec::new(),
            retry_eligible: false,
        }
    }

    /// Creates an unknown_tool error.
    pub fn unknown_tool(tool_name: &str) -> Self {
        Self {
            error_code: "unknown_tool".to_string(),
            message: format!("Unknown broker tool: {}", tool_name),
            recovery_hint: format!(
                "Available tools are: {MCPMATE_UCAN_CATALOG_TOOL}, {MCPMATE_UCAN_DETAILS_TOOL}, {MCPMATE_UCAN_CALL_TOOL}."
            ),
            alternatives: vec![
                MCPMATE_UCAN_CATALOG_TOOL.to_string(),
                MCPMATE_UCAN_DETAILS_TOOL.to_string(),
                MCPMATE_UCAN_CALL_TOOL.to_string(),
            ],
            retry_eligible: false,
        }
    }

    /// Converts the error to a JSON string for MCP tool response.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| {
            serde_json::to_string_pretty(&serde_json::json!({
                "error_code": "serialization_error",
                "message": "Failed to serialize error response",
                "recovery_hint": "An internal error occurred while formatting the error response.",
                "alternatives": [],
                "retry_eligible": false
            }))
            .unwrap_or_else(|_| "{}".to_string())
        })
    }

    /// Converts the error to a failed CallToolResult.
    pub fn to_call_tool_result(&self) -> CallToolResult {
        CallToolResult::error(vec![ContentBlock::text(self.to_json())])
    }
}

/// Capitalizes the capability kind for display.
fn capitalize_kind(kind: &str) -> &'static str {
    match kind {
        "tool" => "Tool",
        "prompt" => "Prompt",
        "resource" => "Resource",
        "resource_template" => "Resource template",
        _ => "Capability",
    }
}

/// Finds similar names using Levenshtein distance.
/// Returns up to `limit` names sorted by similarity (closest first).
fn find_similar_names(
    query: &str,
    candidates: &[String],
    limit: usize,
) -> Vec<String> {
    if candidates.is_empty() || limit == 0 {
        return Vec::new();
    }

    let query_lower = query.to_lowercase();
    let mut scored: Vec<(usize, &String)> = candidates
        .iter()
        .filter(|c| !c.is_empty())
        .map(|candidate| {
            let candidate_lower = candidate.to_lowercase();
            let distance = levenshtein_distance(&query_lower, &candidate_lower);
            (distance, candidate)
        })
        .collect();

    scored.sort_by_key(|(distance, _)| *distance);

    scored
        .into_iter()
        .take(limit)
        .map(|(_, candidate)| candidate.clone())
        .collect()
}

/// Computes the Levenshtein distance between two strings.
/// Uses dynamic programming with O(min(m,n)) space.
fn levenshtein_distance(
    a: &str,
    b: &str,
) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let a_len = a_chars.len();
    let b_len = b_chars.len();

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    // Use the shorter string for the inner loop to minimize space
    let (longer, shorter) = if a_len > b_len {
        (&a_chars, &b_chars)
    } else {
        (&b_chars, &a_chars)
    };

    let mut prev_row: Vec<usize> = (0..=shorter.len()).collect();
    let mut curr_row: Vec<usize> = vec![0; shorter.len() + 1];

    for (i, long_char) in longer.iter().enumerate() {
        curr_row[0] = i + 1;
        for (j, short_char) in shorter.iter().enumerate() {
            let cost = if long_char == short_char { 0 } else { 1 };
            curr_row[j + 1] = (prev_row[j + 1] + 1).min(curr_row[j] + 1).min(prev_row[j] + cost);
        }
        std::mem::swap(&mut prev_row, &mut curr_row);
    }

    prev_row[shorter.len()]
}

pub struct BrokerService {
    database: Arc<Database>,
    connection_pool: Arc<Mutex<UpstreamConnectionPool>>,
}

const UCAN_RELOAD_TTL: Duration = Duration::from_secs(2);

static UCAN_PROMPT_REPO: Lazy<UcanPromptRepository> = Lazy::new(UcanPromptRepository::new);

#[derive(Debug, Clone)]
pub(crate) struct ProfileToolDescriptions {
    pub(crate) get: String,
    pub(crate) set: String,
    pub(crate) add: String,
    pub(crate) remove: String,
}

pub(crate) fn profile_tool_descriptions() -> ProfileToolDescriptions {
    let config = UCAN_PROMPT_REPO.get_blocking();
    ProfileToolDescriptions {
        get: config.profile_get_description,
        set: config.profile_set_description,
        add: config.profile_add_description,
        remove: config.profile_remove_description,
    }
}

#[derive(Debug, Clone, Serialize)]
struct SurfaceDirectoryItem {
    capability_name: String,
    capability_kind: SurfaceKind,
    summary: Option<String>,
    action: &'static str,
    next_step: &'static str,
    server_id: String,
    server_name: String,
    interaction_mode: &'static str,
    detail_hint: &'static str,
    /// Intentionally raw string (e.g. "registry:google-ads") for LLM prompt consumption.
    /// The structured API surface uses `ServerSource` objects instead.
    source: Option<String>,
}

#[derive(Debug, Serialize)]
struct SurfaceDirectoryResponse {
    format: Vec<String>,
    page: usize,
    page_size: usize,
    total_items: usize,
    total_pages: usize,
    has_next_page: bool,
    next_page: Option<usize>,
    usage: String,
    stale_hint: String,
    error_recovery_hint: String,
    items: Vec<SurfaceDirectoryItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct UcanPromptConfig {
    catalog_tool_description: String,
    details_tool_description: String,
    call_tool_description: String,
    catalog_usage: String,
    #[serde(default = "default_catalog_stale_hint")]
    catalog_stale_hint: String,
    #[serde(default = "default_error_recovery_hint")]
    error_recovery_hint: String,
    catalog_format: Vec<String>,
    catalog_page_size_default: usize,
    catalog_page_size_max: usize,
    #[serde(default)]
    catalog_sort_weights: CatalogSortWeights,
    #[serde(default)]
    workflow_hints: WorkflowHints,
    #[serde(default = "default_catalog_enrich_from_source")]
    catalog_enrich_from_source: bool,
    #[serde(default = "default_profile_get_description")]
    profile_get_description: String,
    #[serde(default = "default_profile_set_description")]
    profile_set_description: String,
    #[serde(default = "default_profile_add_description")]
    profile_add_description: String,
    #[serde(default = "default_profile_remove_description")]
    profile_remove_description: String,
}

fn default_catalog_enrich_from_source() -> bool {
    true
}

fn default_catalog_stale_hint() -> String {
    "Surface data may be stale if server status changed recently. Re-run mcpmate_ucan_catalog before deciding."
        .to_string()
}

fn default_error_recovery_hint() -> String {
    "Recovery path: verify capability_name and capability_kind from mcpmate_ucan_catalog, inspect Details with detail_level=full, then retry Tool or Prompt with corrected arguments. For Resource or ResourceTemplate, use Details to obtain or expand the concrete canonical URI, then invoke resources/read.".to_string()
}

fn default_profile_get_description() -> String {
    "MCPMATE SURFACE PROFILE STATUS\nROLE: Show which profile bundles shape this client's MCP surface.\nUSE_WHEN: Before modifying profiles, or when you need to understand the current surface selection.\nRETURNS: Current client mode, capability source, and active profile IDs.\nWORKFLOW: get -> directory/details -> set/add/remove.\nRULES: Read-only; never changes state.\nExample: mcpmate_profile_get()".to_string()
}

fn default_profile_set_description() -> String {
    "MCPMATE SURFACE PROFILE SWITCH\nROLE: Replace the profile bundles that shape this client's MCP surface.\nUSE_WHEN: The user wants to switch to a different profile or a specific combination.\nRETURNS: The finalized profile selection after replacement.\nWORKFLOW: get -> directory(kind_filter=[\"profile\"]) -> details -> set. Only set profiles you have inspected.\nRULES: Requires profile_ids with at least one ID. Previous selection is fully replaced.\nExample: mcpmate_profile_set(profile_ids=[\"prof_abc123\"]) or mcpmate_profile_set(profile_ids=[\"prof_abc123\",\"prof_def456\"])".to_string()
}

fn default_profile_add_description() -> String {
    "MCPMATE SURFACE PROFILE EXTEND\nROLE: Add profile bundles to the active MCP surface without removing existing ones.\nUSE_WHEN: The user wants to layer additional surface items on top of the current set.\nRETURNS: The merged profile selection after addition.\nWORKFLOW: get -> directory(kind_filter=[\"profile\"]) -> details -> add. Duplicate IDs are ignored.\nRULES: Requires profile_ids with at least one ID. Existing selection is preserved.\nExample: mcpmate_profile_add(profile_ids=[\"prof_def456\"])".to_string()
}

fn default_profile_remove_description() -> String {
    "MCPMATE SURFACE PROFILE REDUCE\nROLE: Remove profile bundles from the active MCP surface without deleting profile definitions.\nUSE_WHEN: The user wants to drop specific profiles from the current working set.\nRETURNS: The remaining profile selection after removal.\nWORKFLOW: get -> remove. IDs not in the current selection are silently skipped.\nRULES: Requires profile_ids with at least one ID. Profile definitions are never deleted.\nExample: mcpmate_profile_remove(profile_ids=[\"prof_def456\"])".to_string()
}

#[derive(Debug, Clone, Deserialize)]
struct WorkflowHints {
    #[serde(default = "default_workflow_hints_tool")]
    tool: Vec<String>,
    #[serde(default = "default_workflow_hints_prompt")]
    prompt: Vec<String>,
    #[serde(default = "default_workflow_hints_resource")]
    resource: Vec<String>,
    #[serde(default = "default_workflow_hints_resource_template")]
    resource_template: Vec<String>,
    #[serde(default = "default_workflow_hints_profile")]
    profile: Vec<String>,
}

impl Default for WorkflowHints {
    fn default() -> Self {
        Self {
            tool: default_workflow_hints_tool(),
            prompt: default_workflow_hints_prompt(),
            resource: default_workflow_hints_resource(),
            resource_template: default_workflow_hints_resource_template(),
            profile: default_workflow_hints_profile(),
        }
    }
}

impl WorkflowHints {
    fn normalize(&mut self) {
        self.tool = normalize_string_list(std::mem::take(&mut self.tool));
        self.prompt = normalize_string_list(std::mem::take(&mut self.prompt));
        self.resource = normalize_string_list(std::mem::take(&mut self.resource));
        self.resource_template = normalize_string_list(std::mem::take(&mut self.resource_template));
        self.profile = normalize_string_list(std::mem::take(&mut self.profile));
    }
}

fn default_workflow_hints_tool() -> Vec<String> {
    vec![
        "Use detail_level=summary first, then detail_level=full when arguments remain unclear.".to_string(),
        "Fill all required arguments before mcpmate_ucan_call.".to_string(),
    ]
}

fn default_workflow_hints_prompt() -> Vec<String> {
    vec![
        "Inspect prompt arguments first, then call through mcpmate_ucan_call.".to_string(),
        "Use required arguments only; avoid speculative fields.".to_string(),
    ]
}

fn default_workflow_hints_resource() -> Vec<String> {
    vec!["Inspect resource details first, then invoke resources/read with the canonical URI.".to_string()]
}

fn default_workflow_hints_resource_template() -> Vec<String> {
    vec![
        "Inspect resource template details first, then expand its RFC 6570 variables to a concrete canonical URI."
            .to_string(),
        "Invoke resources/read with the concrete canonical URI.".to_string(),
    ]
}

fn default_workflow_hints_profile() -> Vec<String> {
    vec![
        "Profiles are selectable bundles that shape the MCP surface. Discover with mcpmate_ucan_catalog(kind_filter=[\"profile\"]), then inspect the selected surface item before activating via mcpmate_profile_set.".to_string(),
        "After profile selection changes, exposed tools update automatically. Re-fetch tools/list if your client does not auto-refresh.".to_string(),
    ]
}

/// Multi-factor sorting weights for catalog relevance ranking.
/// Lower values = higher priority (sorted first).
#[derive(Debug, Clone, Deserialize, Default)]
struct CatalogSortWeights {
    #[serde(default)]
    kind: KindWeights,
    #[serde(default)]
    health: HealthWeights,
}

#[derive(Debug, Clone, Deserialize)]
struct KindWeights {
    #[serde(default = "default_kind_weight_tool")]
    tool: u32,
    #[serde(default = "default_kind_weight_prompt")]
    prompt: u32,
    #[serde(default = "default_kind_weight_resource")]
    resource: u32,
    #[serde(default = "default_kind_weight_resource_template")]
    resource_template: u32,
    #[serde(default = "default_kind_weight_profile")]
    profile: u32,
}

impl Default for KindWeights {
    fn default() -> Self {
        Self {
            tool: default_kind_weight_tool(),
            prompt: default_kind_weight_prompt(),
            resource: default_kind_weight_resource(),
            resource_template: default_kind_weight_resource_template(),
            profile: default_kind_weight_profile(),
        }
    }
}

fn default_kind_weight_tool() -> u32 {
    0
}
fn default_kind_weight_prompt() -> u32 {
    1
}
fn default_kind_weight_resource() -> u32 {
    2
}
fn default_kind_weight_resource_template() -> u32 {
    3
}
fn default_kind_weight_profile() -> u32 {
    0
}

#[derive(Debug, Clone, Deserialize)]
struct HealthWeights {
    #[serde(default = "default_health_weight_ready")]
    ready: u32,
    #[serde(default = "default_health_weight_reconnecting")]
    reconnecting: u32,
    #[serde(default = "default_health_weight_other")]
    other: u32,
}

impl Default for HealthWeights {
    fn default() -> Self {
        Self {
            ready: default_health_weight_ready(),
            reconnecting: default_health_weight_reconnecting(),
            other: default_health_weight_other(),
        }
    }
}

fn default_health_weight_ready() -> u32 {
    0
}
fn default_health_weight_reconnecting() -> u32 {
    1
}
fn default_health_weight_other() -> u32 {
    2
}

#[derive(Debug, Clone)]
struct UcanPromptState {
    config: UcanPromptConfig,
    last_loaded_at: Instant,
}

struct UcanPromptRepository {
    state: StdRwLock<Option<UcanPromptState>>,
    reload_lock: StdMutex<()>,
}

impl UcanPromptRepository {
    fn new() -> Self {
        Self {
            state: StdRwLock::new(None),
            reload_lock: StdMutex::new(()),
        }
    }

    async fn get(&self) -> UcanPromptConfig {
        self.get_blocking()
    }

    fn get_blocking(&self) -> UcanPromptConfig {
        {
            let guard = self.state.read().expect("ucan prompt state read lock");
            if let Some(state) = guard.as_ref()
                && state.last_loaded_at.elapsed() < UCAN_RELOAD_TTL
            {
                return state.config.clone();
            }
        }

        let _lock = self.reload_lock.lock().expect("ucan prompt reload lock");
        {
            let guard = self.state.read().expect("ucan prompt state read lock");
            if let Some(state) = guard.as_ref()
                && state.last_loaded_at.elapsed() < UCAN_RELOAD_TTL
            {
                return state.config.clone();
            }
        }

        let next_config = load_ucan_prompt_config_blocking().unwrap_or_else(|error| {
            tracing::warn!("Failed to reload UCAN prompt config: {error}");
            default_ucan_prompt_config()
        });

        let mut guard = self.state.write().expect("ucan prompt state write lock");
        let effective = next_config.clone();
        *guard = Some(UcanPromptState {
            config: next_config,
            last_loaded_at: Instant::now(),
        });
        effective
    }
}

/// Related capability reference for cross-linking in details response.
#[derive(Debug, Clone, Serialize)]
struct RelatedSurfaceItem {
    capability_name: String,
    capability_kind: SurfaceKind,
    summary: Option<String>,
}

/// Argument tip extracted from schema for LLM guidance.
#[derive(Debug, Clone, Serialize)]
struct ArgumentTip {
    name: String,
    required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    type_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Debug, Serialize)]
struct SurfaceDetailsResponse {
    capability_kind: SurfaceKind,
    capability_name: String,
    server_id: String,
    server_name: String,
    detail_level: UcanDetailLevel,
    details: serde_json::Value,
    /// Workflow hints for LLM to understand how to use this capability.
    workflow_hints: Vec<String>,
    /// Related capabilities from the same server (max 5).
    related_capabilities: Vec<RelatedSurfaceItem>,
    /// Argument tips extracted from schema (for tools and prompts).
    argument_tips: Vec<ArgumentTip>,
    call_requirements: CallRequirements,
    error_recovery_hint: String,
}

#[derive(Debug, Clone, Serialize)]
struct CallRequirements {
    accepts_arguments: bool,
    required_arguments: Vec<String>,
    call_ready_without_arguments: bool,
}

#[derive(Debug, Deserialize)]
struct SurfaceLookupParams {
    capability_kind: SurfaceKind,
    capability_name: String,
    #[serde(default)]
    detail_level: UcanDetailLevel,
}

#[derive(Debug, Deserialize)]
struct CatalogParams {
    #[serde(default = "default_catalog_page")]
    page: usize,
    #[serde(default = "default_catalog_page_size")]
    page_size: usize,
    /// Case-insensitive substring search in capability_name and summary.
    #[serde(default)]
    search: Option<String>,
    /// Filter by capability_kind. Valid values: "tool", "prompt", "resource", "resource_template".
    #[serde(default)]
    kind_filter: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct SurfaceCallParams {
    capability_kind: SurfaceKind,
    capability_name: String,
    #[serde(default)]
    arguments: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum SurfaceKind {
    Tool,
    Prompt,
    Resource,
    ResourceTemplate,
    Profile,
}

impl SurfaceKind {
    fn weight(
        &self,
        weights: &KindWeights,
    ) -> u32 {
        match self {
            SurfaceKind::Tool => weights.tool,
            SurfaceKind::Prompt => weights.prompt,
            SurfaceKind::Resource => weights.resource,
            SurfaceKind::ResourceTemplate => weights.resource_template,
            SurfaceKind::Profile => weights.profile,
        }
    }
}

type InstanceSnapshot = (
    String,
    ConnectionStatus,
    bool,
    bool,
    Option<rmcp::service::Peer<rmcp::RoleClient>>,
);
type PoolSnapshot = std::collections::HashMap<String, Vec<InstanceSnapshot>>;

fn health_weight_for_server(
    server_id: &str,
    snapshot: &PoolSnapshot,
    weights: &HealthWeights,
) -> u32 {
    if let Some(instances) = snapshot.get(server_id) {
        for (_, status, _, _, _) in instances {
            if matches!(status, ConnectionStatus::Ready) {
                return weights.ready;
            }
            if matches!(status, ConnectionStatus::Initializing) {
                return weights.reconnecting;
            }
        }
    }
    weights.other
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
enum UcanDetailLevel {
    #[default]
    Summary,
    Full,
}

#[derive(Clone)]
struct VisibleToolEntry {
    server_id: String,
    server_name: String,
    raw_tool_name: String,
    tool: Tool,
}

#[derive(Clone)]
struct VisiblePromptEntry {
    server_id: String,
    server_name: String,
    raw_prompt_name: String,
    prompt: rmcp::model::Prompt,
}

#[derive(Clone)]
struct VisibleResourceEntry {
    server_id: String,
    server_name: String,
    raw_resource_uri: String,
    resource: Resource,
}

#[derive(Clone)]
struct VisibleResourceTemplateEntry {
    server_id: String,
    server_name: String,
    raw_uri_template: String,
    resource_template: ResourceTemplate,
}

struct VisibleListing<T> {
    entries: Vec<T>,
    aggregate: AggregateListStatus,
}

struct UcanCatalogScope {
    client_context: ClientContext,
    snapshot: VisibilitySnapshot,
    visible_server_ids: HashSet<String>,
    enabled_servers: Vec<(String, String, bool)>,
    eligible_server_ids: HashSet<String>,
}

impl<T> VisibleListing<T> {
    fn finish(self) -> Result<Vec<T>> {
        self.aggregate.finish_for_result(!self.entries.is_empty())?;
        Ok(self.entries)
    }

    fn find_authoritatively(
        self,
        predicate: impl FnMut(&T) -> bool,
    ) -> Result<Option<T>> {
        let found = self.entries.into_iter().find(predicate);
        if found.is_none() {
            self.aggregate.finish_for_result(false)?;
        }
        Ok(found)
    }
}

fn retain_brokered_tools(
    context: &ClientBuiltinContext,
    eligible_server_ids: &HashSet<String>,
    visible: &mut Vec<VisibleToolEntry>,
) {
    visible.retain(|entry| {
        !crate::core::proxy::server::unify_directly_exposed_tool_allowed(
            context.unify_workspace.as_ref(),
            eligible_server_ids,
            &entry.server_id,
            &entry.raw_tool_name,
        )
    });
}

fn retain_brokered_prompts(
    context: &ClientBuiltinContext,
    eligible_server_ids: &HashSet<String>,
    visible: &mut Vec<VisiblePromptEntry>,
) {
    visible.retain(|entry| {
        !crate::core::proxy::server::unify_directly_exposed_prompt_allowed(
            context.unify_workspace.as_ref(),
            eligible_server_ids,
            &entry.server_id,
            &entry.raw_prompt_name,
        )
    });
}

fn retain_brokered_resources(
    context: &ClientBuiltinContext,
    eligible_server_ids: &HashSet<String>,
    visible: &mut Vec<VisibleResourceEntry>,
) {
    visible.retain(|entry| {
        !crate::core::proxy::server::unify_directly_exposed_resource_allowed(
            context.unify_workspace.as_ref(),
            eligible_server_ids,
            &entry.server_id,
            &entry.raw_resource_uri,
        )
    });
}

fn retain_brokered_resource_templates(
    context: &ClientBuiltinContext,
    eligible_server_ids: &HashSet<String>,
    visible: &mut Vec<VisibleResourceTemplateEntry>,
) {
    visible.retain(|entry| {
        !crate::core::proxy::server::unify_directly_exposed_template_allowed(
            context.unify_workspace.as_ref(),
            eligible_server_ids,
            &entry.server_id,
            &entry.raw_uri_template,
        )
    });
}

fn profile_capabilities_visible(context: &ClientBuiltinContext) -> bool {
    matches!(context.config_mode.as_deref(), None | Some("hosted"))
        && context.capability_source == CapabilitySource::Profiles
}

impl BrokerService {
    pub fn new(
        database: Arc<Database>,
        connection_pool: Arc<Mutex<UpstreamConnectionPool>>,
    ) -> Self {
        Self {
            database,
            connection_pool,
        }
    }

    async fn ucan_prompt_config(&self) -> UcanPromptConfig {
        UCAN_PROMPT_REPO.get().await
    }

    async fn load_enabled_servers(
        &self,
        context: &'static str,
    ) -> Result<Vec<(String, String, bool)>> {
        sqlx::query_as(
            r#"
            SELECT sc.id, sc.name, sc.unify_direct_exposure_eligible
            FROM server_config sc
            WHERE sc.enabled = 1
            ORDER BY sc.name, sc.id
            "#,
        )
        .fetch_all(&self.database.pool)
        .await
        .context(context)
    }

    async fn fetch_source_enrichment(
        &self,
        server_ids: &[String],
    ) -> Result<HashMap<String, Option<String>>> {
        if server_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let placeholders: Vec<String> = server_ids.iter().map(|_| "?".to_string()).collect();
        let placeholders_str = placeholders.join(",");
        let query_str = format!(
            r#"
            SELECT sc.id, sc.source
            FROM server_config sc
            WHERE sc.id IN ({})
            "#,
            placeholders_str
        );

        let mut query = sqlx::query_as::<_, (String, Option<String>)>(&query_str);
        for id in server_ids {
            query = query.bind(id);
        }
        let rows = query
            .fetch_all(&self.database.pool)
            .await
            .context("Failed to fetch server sources")?;

        Ok(rows.into_iter().collect())
    }

    async fn tool_catalog(
        &self,
        context: &ClientBuiltinContext,
        page: usize,
        page_size: usize,
        search: Option<&str>,
        kind_filter: Option<&[String]>,
    ) -> Result<CallToolResult> {
        let prompt_config = self.ucan_prompt_config().await;
        let enrich_enabled = prompt_config.catalog_enrich_from_source;
        let scope = self.resolve_ucan_catalog_scope(context).await?;

        let VisibleListing {
            entries: tools,
            aggregate: tools_aggregate,
        } = self.visible_tools_listing_with_scope(context, &scope).await?;
        let VisibleListing {
            entries: prompts,
            aggregate: prompts_aggregate,
        } = self.visible_prompts_listing_with_scope(context, &scope).await?;
        let VisibleListing {
            entries: resources,
            aggregate: resources_aggregate,
        } = self.visible_resources_listing_with_scope(context, &scope).await?;
        let VisibleListing {
            entries: resource_templates,
            aggregate: resource_templates_aggregate,
        } = self
            .visible_resource_templates_listing_with_scope(context, &scope)
            .await?;

        let all_server_ids: Vec<String> = {
            let mut ids = HashSet::new();
            for entry in &tools {
                ids.insert(entry.server_id.clone());
            }
            for entry in &prompts {
                ids.insert(entry.server_id.clone());
            }
            for entry in &resources {
                ids.insert(entry.server_id.clone());
            }
            for entry in &resource_templates {
                ids.insert(entry.server_id.clone());
            }
            ids.into_iter().collect()
        };

        let enrichment_map = if enrich_enabled {
            self.fetch_source_enrichment(&all_server_ids).await?
        } else {
            HashMap::new()
        };

        let get_enrichment =
            |server_id: &str| -> Option<String> { enrichment_map.get(server_id).cloned().unwrap_or(None) };

        let mut summaries: Vec<SurfaceDirectoryItem> = tools
            .into_iter()
            .map(|entry| {
                let source = get_enrichment(&entry.server_id);
                SurfaceDirectoryItem {
                    capability_name: entry.tool.name.to_string(),
                    capability_kind: SurfaceKind::Tool,
                    summary: compact_description(entry.tool.description.as_deref()),
                    action: "inspect_first",
                    next_step: "details",
                    server_id: entry.server_id,
                    server_name: entry.server_name,
                    interaction_mode: "model_controlled",
                    detail_hint: "Use mcpmate_ucan_details with detail_level=summary first; switch to full before constructing arguments if needed.",
                    source,
                }
            })
            .collect();

        summaries.extend(
            prompts.into_iter().map(|entry| {
                let source = get_enrichment(&entry.server_id);
                SurfaceDirectoryItem {
                    capability_name: entry.prompt.name.to_string(),
                    capability_kind: SurfaceKind::Prompt,
                    summary: compact_description(extract_description_from_value(&entry.prompt).as_deref()),
                    action: "inspect_first",
                    next_step: "details",
                    server_id: entry.server_id,
                    server_name: entry.server_name,
                    interaction_mode: "user_controlled_template",
                    detail_hint: "Prompt results are brokered through mcpmate_ucan_call; inspect arguments with mcpmate_ucan_details first.",
                    source,
                }
            }),
        );

        summaries.extend(resources.into_iter().map(|entry| {
            let source = get_enrichment(&entry.server_id);
            SurfaceDirectoryItem {
                capability_name: entry.resource.uri.to_string(),
                capability_kind: SurfaceKind::Resource,
                summary: compact_description(extract_description_from_value(&entry.resource).as_deref()),
                action: "inspect_first",
                next_step: "details",
                server_id: entry.server_id,
                server_name: entry.server_name,
                interaction_mode: "application_context",
                detail_hint: "Inspect resource details, then invoke resources/read with the canonical URI.",
                source,
            }
        }));

        summaries.extend(
            resource_templates.into_iter().map(|entry| {
                let source = get_enrichment(&entry.server_id);
                SurfaceDirectoryItem {
                    capability_name: entry.resource_template.uri_template.to_string(),
                    capability_kind: SurfaceKind::ResourceTemplate,
                    summary: compact_description(extract_description_from_value(&entry.resource_template).as_deref()),
                    action: "inspect_first",
                    next_step: "details",
                    server_id: entry.server_id,
                    server_name: entry.server_name,
                    interaction_mode: "application_context_template",
                    detail_hint: "Inspect the canonical URI template, expand its RFC 6570 variables, then invoke resources/read with the concrete URI.",
                    source,
                }
            }),
        );

        // In Hosted mode with Profiles source, include profile entries in the catalog.
        if profile_capabilities_visible(context) {
            if let Ok(profiles) = profile_repo::get_all_profile(&self.database.pool).await {
                for prof in profiles {
                    if !matches!(prof.profile_type, ProfileType::Shared) {
                        continue;
                    }
                    let Some(ref profile_id) = prof.id else { continue };
                    let counts = load_profile_capability_counts(&self.database.pool, profile_id)
                        .await
                        .unwrap_or(ProfileCapabilityCounts {
                            server_count: 0,
                            tool_count: 0,
                            prompt_count: 0,
                            resource_count: 0,
                        });
                    let count_summary = format!(
                        "{} servers, {} tools, {} prompts, {} resources",
                        counts.server_count, counts.tool_count, counts.prompt_count, counts.resource_count
                    );
                    let summary_text = match prof.description.clone() {
                        Some(description) if !description.trim().is_empty() => {
                            format!("{} — {}", prof.name, description)
                        }
                        _ => format!("{} — {}", prof.name, count_summary),
                    };
                    summaries.push(SurfaceDirectoryItem {
                        capability_name: profile_id.clone(),
                        capability_kind: SurfaceKind::Profile,
                        summary: Some(summary_text),
                        action: "inspect_first",
                        next_step: "details",
                        server_id: format!("profile:{}", profile_id),
                        server_name: prof.name.clone(),
                        interaction_mode: "scope_managed",
                        detail_hint: "Use this surface item's capability_kind and capability_name with mcpmate_ucan_details to see all surface items in this profile. Activate via mcpmate_profile_set(profile_ids=[profile_id]).",
                        source: None,
                    });
                }
            }
        }

        let weights = &prompt_config.catalog_sort_weights;
        let pool_snapshot = self.connection_pool.lock().await.get_snapshot();

        summaries.sort_by(|left, right| {
            let left_kind_weight = left.capability_kind.weight(&weights.kind);
            let right_kind_weight = right.capability_kind.weight(&weights.kind);

            let left_health_weight = health_weight_for_server(&left.server_id, &pool_snapshot, &weights.health);
            let right_health_weight = health_weight_for_server(&right.server_id, &pool_snapshot, &weights.health);

            let left_score = left_kind_weight.saturating_add(left_health_weight);
            let right_score = right_kind_weight.saturating_add(right_health_weight);

            left_score
                .cmp(&right_score)
                .then_with(|| left.capability_name.cmp(&right.capability_name))
        });

        // Apply filters before pagination
        if let Some(search_term) = search {
            let search_lower = search_term.to_lowercase();
            summaries.retain(|item| {
                let name_match = item.capability_name.to_lowercase().contains(&search_lower);
                let summary_match = item
                    .summary
                    .as_ref()
                    .map(|s| s.to_lowercase().contains(&search_lower))
                    .unwrap_or(false);
                name_match || summary_match
            });
        }

        if let Some(kinds) = kind_filter {
            let allowed_kinds: HashSet<&str> = kinds.iter().map(|s| s.as_str()).collect();
            summaries.retain(|item| {
                let kind_str = match item.capability_kind {
                    SurfaceKind::Tool => "tool",
                    SurfaceKind::Prompt => "prompt",
                    SurfaceKind::Resource => "resource",
                    SurfaceKind::ResourceTemplate => "resource_template",
                    SurfaceKind::Profile => "profile",
                };
                allowed_kinds.contains(kind_str)
            });
        }

        if let Err(CatalogAuthorityError::Incomplete) = ensure_catalog_result_is_authoritative(
            !summaries.is_empty(),
            kind_filter,
            &[
                ("tool", &tools_aggregate),
                ("prompt", &prompts_aggregate),
                ("resource", &resources_aggregate),
                ("resource_template", &resource_templates_aggregate),
            ],
        ) {
            return Ok(UcanError::catalog_incomplete().to_call_tool_result());
        }

        let page_size = page_size.clamp(1, 50);
        let page_size = page_size.clamp(1, prompt_config.catalog_page_size_max.max(1));
        let total_items = summaries.len();
        let total_pages = if total_items == 0 {
            1
        } else {
            total_items.div_ceil(page_size)
        };
        let page = page.clamp(1, total_pages);
        let start = (page - 1) * page_size;
        let end = (start + page_size).min(total_items);
        let items = if start < total_items {
            summaries[start..end].to_vec()
        } else {
            Vec::new()
        };
        let response = SurfaceDirectoryResponse {
            format: if prompt_config.catalog_format.is_empty() {
                vec![
                    "capability_name".to_string(),
                    "capability_kind".to_string(),
                    "summary".to_string(),
                    "action".to_string(),
                    "next_step".to_string(),
                    "server_id".to_string(),
                    "server_name".to_string(),
                    "interaction_mode".to_string(),
                    "detail_hint".to_string(),
                    "source".to_string(),
                ]
            } else {
                prompt_config.catalog_format.clone()
            },
            page,
            page_size,
            total_items,
            total_pages,
            has_next_page: page < total_pages,
            next_page: (page < total_pages).then_some(page + 1),
            usage: prompt_config.catalog_usage.clone(),
            stale_hint: prompt_config.catalog_stale_hint.clone(),
            error_recovery_hint: prompt_config.error_recovery_hint.clone(),
            items,
        };

        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string_pretty(&response).context("Failed to serialize Unify tool catalog")?,
        )]))
    }

    async fn collect_surface_names_for_kind(
        &self,
        context: &ClientBuiltinContext,
        kind: SurfaceKind,
    ) -> Result<Vec<String>> {
        let names = match kind {
            SurfaceKind::Tool => self
                .visible_tools(context)
                .await?
                .into_iter()
                .map(|entry| entry.tool.name.to_string())
                .collect(),
            SurfaceKind::Prompt => self
                .visible_prompts(context)
                .await?
                .into_iter()
                .map(|entry| entry.prompt.name.to_string())
                .collect(),
            SurfaceKind::Resource => self
                .visible_resources(context)
                .await?
                .into_iter()
                .map(|entry| entry.resource.uri.to_string())
                .collect(),
            SurfaceKind::ResourceTemplate => self
                .visible_resource_templates(context)
                .await?
                .into_iter()
                .map(|entry| entry.resource_template.uri_template.to_string())
                .collect(),
            SurfaceKind::Profile => {
                if !profile_capabilities_visible(context) {
                    Vec::new()
                } else {
                    profile_repo::get_all_profile(&self.database.pool)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|p| matches!(p.profile_type, ProfileType::Shared))
                        .filter_map(|p| p.id.clone())
                        .collect()
                }
            }
        };
        Ok(names)
    }

    async fn tool_details(
        &self,
        context: &ClientBuiltinContext,
        capability_kind: SurfaceKind,
        capability_name: &str,
        detail_level: UcanDetailLevel,
    ) -> Result<CallToolResult> {
        let prompt_config = self.ucan_prompt_config().await;
        let mut resource_link = None;
        let workflow_hints = match capability_kind {
            SurfaceKind::Tool => prompt_config.workflow_hints.tool.clone(),
            SurfaceKind::Prompt => prompt_config.workflow_hints.prompt.clone(),
            SurfaceKind::Resource => prompt_config.workflow_hints.resource.clone(),
            SurfaceKind::ResourceTemplate => prompt_config.workflow_hints.resource_template.clone(),
            SurfaceKind::Profile => prompt_config.workflow_hints.profile.clone(),
        };

        let response = match capability_kind {
            SurfaceKind::Tool => match self.find_visible_tool(context, capability_name).await {
                Err(error) if is_catalog_authority_error(&error) => {
                    return Ok(UcanError::catalog_incomplete().to_call_tool_result());
                }
                Err(error) => return Err(error),
                Ok(tool) => match tool {
                    Some(tool) => {
                        let related = self
                            .find_related_surface_items(context, &tool.server_id, capability_name, capability_kind)
                            .await;
                        let argument_tips = extract_argument_tips_from_tool(&tool.tool);
                        SurfaceDetailsResponse {
                            capability_kind,
                            capability_name: tool.tool.name.to_string(),
                            server_id: tool.server_id,
                            server_name: tool.server_name,
                            detail_level,
                            details: tool_details_value(&tool.tool, detail_level),
                            workflow_hints,
                            related_capabilities: related,
                            argument_tips,
                            call_requirements: call_requirements_for_tool(&tool.tool),
                            error_recovery_hint: prompt_config.error_recovery_hint.clone(),
                        }
                    }
                    None => {
                        let surface_names = self.collect_surface_names_for_kind(context, capability_kind).await?;
                        return Ok(UcanError::capability_not_found("tool", capability_name, &surface_names)
                            .to_call_tool_result());
                    }
                },
            },
            SurfaceKind::Prompt => match self.find_visible_prompt(context, capability_name).await {
                Err(error) if is_catalog_authority_error(&error) => {
                    return Ok(UcanError::catalog_incomplete().to_call_tool_result());
                }
                Err(error) => return Err(error),
                Ok(prompt) => match prompt {
                    Some(prompt) => {
                        let related = self
                            .find_related_surface_items(context, &prompt.server_id, capability_name, capability_kind)
                            .await;
                        let argument_tips = extract_argument_tips_from_prompt(&prompt.prompt);
                        SurfaceDetailsResponse {
                            capability_kind,
                            capability_name: prompt.prompt.name.to_string(),
                            server_id: prompt.server_id,
                            server_name: prompt.server_name,
                            detail_level,
                            details: prompt_details_value(&prompt.prompt, detail_level)
                                .context("Failed to serialize Unify prompt details")?,
                            workflow_hints,
                            related_capabilities: related,
                            argument_tips,
                            call_requirements: call_requirements_for_prompt(&prompt.prompt),
                            error_recovery_hint: prompt_config.error_recovery_hint.clone(),
                        }
                    }
                    None => {
                        let surface_names = self.collect_surface_names_for_kind(context, capability_kind).await?;
                        return Ok(
                            UcanError::capability_not_found("prompt", capability_name, &surface_names)
                                .to_call_tool_result(),
                        );
                    }
                },
            },
            SurfaceKind::Resource => match self.find_visible_resource(context, capability_name).await {
                Err(error) if is_catalog_authority_error(&error) => {
                    return Ok(UcanError::catalog_incomplete().to_call_tool_result());
                }
                Err(error) => return Err(error),
                Ok(resource) => match resource {
                    Some(resource) => {
                        let related = self
                            .find_related_surface_items(context, &resource.server_id, capability_name, capability_kind)
                            .await;
                        resource_link = Some(resource.resource.clone());
                        SurfaceDetailsResponse {
                            capability_kind,
                            capability_name: resource.resource.uri.to_string(),
                            server_id: resource.server_id,
                            server_name: resource.server_name,
                            detail_level,
                            details: resource_details_value(&resource.resource, detail_level)
                                .context("Failed to serialize Unify resource details")?,
                            workflow_hints,
                            related_capabilities: related,
                            argument_tips: Vec::new(),
                            call_requirements: CallRequirements {
                                accepts_arguments: false,
                                required_arguments: Vec::new(),
                                call_ready_without_arguments: false,
                            },
                            error_recovery_hint: standard_read_recovery_hint(),
                        }
                    }
                    None => {
                        let surface_names = self.collect_surface_names_for_kind(context, capability_kind).await?;
                        return Ok(
                            UcanError::capability_not_found("resource", capability_name, &surface_names)
                                .to_call_tool_result(),
                        );
                    }
                },
            },
            SurfaceKind::ResourceTemplate => {
                match self.find_visible_resource_template(context, capability_name).await {
                    Err(error) if is_catalog_authority_error(&error) => {
                        return Ok(UcanError::catalog_incomplete().to_call_tool_result());
                    }
                    Err(error) => return Err(error),
                    Ok(template) => match template {
                        Some(template) => {
                            let related = self
                                .find_related_surface_items(
                                    context,
                                    &template.server_id,
                                    capability_name,
                                    capability_kind,
                                )
                                .await;
                            SurfaceDetailsResponse {
                                capability_kind,
                                capability_name: template.resource_template.uri_template.to_string(),
                                server_id: template.server_id,
                                server_name: template.server_name,
                                detail_level,
                                details: resource_template_details_value(&template.resource_template, detail_level)
                                    .context("Failed to serialize Unify resource template details")?,
                                workflow_hints,
                                related_capabilities: related,
                                argument_tips: Vec::new(),
                                call_requirements: CallRequirements {
                                    accepts_arguments: false,
                                    required_arguments: Vec::new(),
                                    call_ready_without_arguments: false,
                                },
                                error_recovery_hint: standard_read_recovery_hint(),
                            }
                        }
                        None => {
                            let surface_names = self.collect_surface_names_for_kind(context, capability_kind).await?;
                            return Ok(UcanError::capability_not_found(
                                "resource_template",
                                capability_name,
                                &surface_names,
                            )
                            .to_call_tool_result());
                        }
                    },
                }
            }
            SurfaceKind::Profile => {
                if !profile_capabilities_visible(context) {
                    let surface_names = self.collect_surface_names_for_kind(context, capability_kind).await?;
                    return Ok(
                        UcanError::capability_not_found("profile", capability_name, &surface_names)
                            .to_call_tool_result(),
                    );
                }

                let profiles = profile_repo::get_all_profile(&self.database.pool)
                    .await
                    .context("Failed to list profiles")?;
                let profile = profiles.into_iter().find(|p| {
                    matches!(p.profile_type, ProfileType::Shared)
                        && p.id.as_deref().is_some_and(|profile_id| profile_id == capability_name)
                });
                match profile {
                    Some(prof) => {
                        let profile_id = prof.id.as_deref().unwrap_or_default();
                        let detail_components = load_profile_detail_components(&self.database.pool, profile_id)
                            .await
                            .context("Failed to load profile detail components")?;
                        let details = serde_json::json!({
                            "id": profile_id,
                            "name": prof.name,
                            "description": prof.description,
                            "is_active": prof.is_active,
                            "profile_type": prof.profile_type.to_string(),
                            "servers": detail_components.servers,
                            "tools": detail_components.tools,
                            "prompts": detail_components.prompts,
                            "resources": detail_components.resources,
                        });
                        let detail_value = match detail_level {
                            UcanDetailLevel::Summary => {
                                serde_json::json!({
                                    "id": profile_id,
                                    "name": prof.name,
                                    "description": prof.description,
                                    "is_active": prof.is_active,
                                    "server_count": detail_components.servers.len(),
                                    "tool_count": detail_components.tools.len(),
                                    "prompt_count": detail_components.prompts.len(),
                                    "resource_count": detail_components.resources.len(),
                                })
                            }
                            UcanDetailLevel::Full => details,
                        };
                        SurfaceDetailsResponse {
                            capability_kind,
                            capability_name: profile_id.to_string(),
                            server_id: format!("profile:{}", profile_id),
                            server_name: prof.name.clone(),
                            detail_level,
                            details: detail_value,
                            workflow_hints,
                            related_capabilities: Vec::new(),
                            argument_tips: Vec::new(),
                            call_requirements: CallRequirements {
                                accepts_arguments: false,
                                required_arguments: Vec::new(),
                                call_ready_without_arguments: true,
                            },
                            error_recovery_hint:
                                "Activate this profile via mcpmate_profile_set, then call exposed tools directly."
                                    .to_string(),
                        }
                    }
                    None => {
                        let surface_names = self.collect_surface_names_for_kind(context, capability_kind).await?;
                        return Ok(
                            UcanError::capability_not_found("profile", capability_name, &surface_names)
                                .to_call_tool_result(),
                        );
                    }
                }
            }
        };

        let mut content = vec![ContentBlock::text(
            serde_json::to_string_pretty(&response).context("Failed to serialize Unify tool details")?,
        )];
        if let Some(resource) = resource_link {
            content.push(ContentBlock::resource_link(resource));
        }
        Ok(CallToolResult::success(content))
    }

    async fn find_related_surface_items(
        &self,
        context: &ClientBuiltinContext,
        server_id: &str,
        exclude_name: &str,
        exclude_kind: SurfaceKind,
    ) -> Vec<RelatedSurfaceItem> {
        let mut related = Vec::new();

        if exclude_kind != SurfaceKind::Tool {
            if let Ok(tools) = self.visible_tools(context).await {
                for entry in tools.iter().take(50) {
                    if entry.server_id == server_id && entry.tool.name.as_ref() != exclude_name {
                        related.push(RelatedSurfaceItem {
                            capability_name: entry.tool.name.to_string(),
                            capability_kind: SurfaceKind::Tool,
                            summary: compact_description(entry.tool.description.as_deref()),
                        });
                        if related.len() >= 5 {
                            break;
                        }
                    }
                }
            }
        }

        if related.len() < 5 && exclude_kind != SurfaceKind::Prompt {
            if let Ok(prompts) = self.visible_prompts(context).await {
                for entry in prompts.iter().take(50) {
                    if entry.server_id == server_id && entry.prompt.name.as_str() != exclude_name {
                        related.push(RelatedSurfaceItem {
                            capability_name: entry.prompt.name.to_string(),
                            capability_kind: SurfaceKind::Prompt,
                            summary: compact_description(extract_description_from_value(&entry.prompt).as_deref()),
                        });
                        if related.len() >= 5 {
                            break;
                        }
                    }
                }
            }
        }

        if related.len() < 5 && exclude_kind != SurfaceKind::Resource {
            if let Ok(resources) = self.visible_resources(context).await {
                for entry in resources.iter().take(50) {
                    if entry.server_id == server_id && entry.resource.uri.as_str() != exclude_name {
                        related.push(RelatedSurfaceItem {
                            capability_name: entry.resource.uri.to_string(),
                            capability_kind: SurfaceKind::Resource,
                            summary: compact_description(extract_description_from_value(&entry.resource).as_deref()),
                        });
                        if related.len() >= 5 {
                            break;
                        }
                    }
                }
            }
        }

        if related.len() < 5 && exclude_kind != SurfaceKind::ResourceTemplate {
            if let Ok(templates) = self.visible_resource_templates(context).await {
                for entry in templates.iter().take(50) {
                    if entry.server_id == server_id && entry.resource_template.uri_template.as_str() != exclude_name {
                        related.push(RelatedSurfaceItem {
                            capability_name: entry.resource_template.uri_template.to_string(),
                            capability_kind: SurfaceKind::ResourceTemplate,
                            summary: compact_description(
                                extract_description_from_value(&entry.resource_template).as_deref(),
                            ),
                        });
                        if related.len() >= 5 {
                            break;
                        }
                    }
                }
            }
        }

        related
    }

    async fn broker_tool_call(
        &self,
        context: &ClientBuiltinContext,
        capability_kind: SurfaceKind,
        capability_name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<CallToolResult> {
        match capability_kind {
            SurfaceKind::Tool => self.broker_tool_call_inner(context, capability_name, arguments).await,
            SurfaceKind::Prompt => self.broker_prompt_call(context, capability_name, arguments).await,
            SurfaceKind::Resource => Ok(UcanError::resource_standard_read_required("resource").to_call_tool_result()),
            SurfaceKind::ResourceTemplate => {
                Ok(UcanError::resource_standard_read_required("resource_template").to_call_tool_result())
            }
            SurfaceKind::Profile => {
                if !profile_capabilities_visible(context) {
                    let surface_names = self.collect_surface_names_for_kind(context, capability_kind).await?;
                    return Ok(
                        UcanError::capability_not_found("profile", capability_name, &surface_names)
                            .to_call_tool_result(),
                    );
                }

                Ok(UcanError {
                    error_code: "profile_not_invocable".to_string(),
                    message: format!("Profile '{}' is not directly invocable.", capability_name),
                    recovery_hint:
                        "Activate this profile via mcpmate_profile_set, then call the exposed tools directly."
                            .to_string(),
                    alternatives: Vec::new(),
                    retry_eligible: false,
                }
                .to_call_tool_result())
            }
        }
    }

    async fn broker_tool_call_inner(
        &self,
        context: &ClientBuiltinContext,
        tool_name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<CallToolResult> {
        let tool_entry = match self.find_visible_tool(context, tool_name).await {
            Err(error) if is_catalog_authority_error(&error) => {
                return Ok(UcanError::catalog_incomplete().to_call_tool_result());
            }
            Err(error) => return Err(error),
            Ok(None) => {
                let surface_names = self.collect_surface_names_for_kind(context, SurfaceKind::Tool).await?;
                return Ok(UcanError::capability_not_found("tool", tool_name, &surface_names).to_call_tool_result());
            }
            Ok(Some(tool_entry)) => tool_entry,
        };
        {
            let required_arguments = required_arguments_from_tool(&tool_entry.tool);
            let missing_required: Vec<String> = required_arguments
                .into_iter()
                .filter(|name| !arguments.contains_key(name))
                .collect();
            if !missing_required.is_empty() {
                return Ok(
                    UcanError::missing_required_arguments("tool", tool_name, &missing_required).to_call_tool_result(),
                );
            }
        }

        let client_context = context.as_client_context();
        let visibility = ProfileVisibilityService::new(Some(self.database.clone()), None);
        let snapshot = visibility
            .resolve_snapshot_for_client(&client_context)
            .await
            .context("Failed to resolve Unify visibility snapshot")?;

        if visibility
            .assert_tool_allowed_with_snapshot(&snapshot, tool_name)
            .await
            .is_err()
        {
            return Ok(UcanError::visibility_denied("tool", tool_name).to_call_tool_result());
        }

        let route = resolve_capability_route(NamingKind::Tool, tool_name)
            .await
            .with_context(|| format!("Failed to resolve unique tool '{}'", tool_name))?;
        let server_id = route.server_id;
        let server_name = route.server_name;
        let original_tool_name = route.upstream_value;

        let peer = match self.acquire_peer(&client_context, &server_id).await {
            Ok(p) => p,
            Err(_) => {
                return Ok(UcanError::server_unreachable(&server_id, &server_name).to_call_tool_result());
            }
        };
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(
            CallToolRequestParams::new(original_tool_name).with_arguments(arguments),
        ));
        let timeout_secs = std::env::var("MCPMATE_TOOL_CALL_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(60);
        let mut options = PeerRequestOptions::no_options();
        options.timeout = Some(std::time::Duration::from_secs(timeout_secs));
        let handle = match peer.send_cancellable_request(request, options).await {
            Ok(handle) => handle,
            Err(error) => {
                tracing::warn!(
                    capability_kind = "tool",
                    capability_name = tool_name,
                    server_id,
                    server_name,
                    error = %error,
                    "Upstream capability invocation could not be sent"
                );
                let error = anyhow::Error::new(error);
                self.record_usage_evidence(&server_id, mcpmate_capability_store::CapabilityKind::Tools, &error)
                    .await;
                return Ok(UcanError::upstream_error("tool", tool_name).to_call_tool_result());
            }
        };

        match handle.await_response().await {
            Ok(rmcp::model::ServerResult::CallToolResult(mut result)) => {
                if result.is_error == Some(true) {
                    let error = anyhow!("upstream CallToolResult marked is_error=true: {:?}", result.content);
                    tracing::warn!(
                        capability_kind = "tool",
                        capability_name = tool_name,
                        server_id,
                        server_name,
                        error = %error,
                        "Upstream capability invocation returned an error result"
                    );
                    self.record_usage_evidence(&server_id, mcpmate_capability_store::CapabilityKind::Tools, &error)
                        .await;
                    return Ok(UcanError::upstream_error("tool", tool_name).to_call_tool_result());
                }
                crate::core::capability::resource_uri::rewrite_call_tool_result(
                    &self.database.pool,
                    &server_id,
                    &server_name,
                    &mut result,
                )
                .await?;
                Ok(result)
            }
            Ok(other) => {
                tracing::warn!(
                    capability_kind = "tool",
                    capability_name = tool_name,
                    server_id,
                    server_name,
                    error = ?other,
                    "Upstream capability invocation returned an unexpected response"
                );
                Ok(UcanError::upstream_error("tool", tool_name).to_call_tool_result())
            }
            Err(error) => {
                let is_timeout = matches!(error, ServiceError::Timeout { .. });
                tracing::warn!(
                    capability_kind = "tool",
                    capability_name = tool_name,
                    server_id,
                    server_name,
                    error = %error,
                    "Upstream capability invocation failed"
                );
                let error = anyhow::Error::new(error);
                self.record_usage_evidence(&server_id, mcpmate_capability_store::CapabilityKind::Tools, &error)
                    .await;
                if is_timeout {
                    Ok(UcanError::timeout("tool", tool_name, timeout_secs).to_call_tool_result())
                } else {
                    Ok(UcanError::upstream_error("tool", tool_name).to_call_tool_result())
                }
            }
        }
    }

    async fn resolve_ucan_catalog_scope(
        &self,
        context: &ClientBuiltinContext,
    ) -> Result<UcanCatalogScope> {
        self.resolve_ucan_catalog_scope_for_client(&context.as_client_context())
            .await
    }

    async fn resolve_ucan_catalog_scope_for_client(
        &self,
        client_context: &ClientContext,
    ) -> Result<UcanCatalogScope> {
        let visibility = ProfileVisibilityService::new(Some(self.database.clone()), None);
        let snapshot = visibility
            .resolve_snapshot_for_client(client_context)
            .await
            .context("Failed to resolve UCan visibility snapshot")?;
        let visible_server_ids = snapshot.server_ids.iter().cloned().collect::<HashSet<_>>();
        let enabled_servers = self
            .load_enabled_servers("Failed to load enabled servers for UCan catalog")
            .await?;
        let eligible_server_ids = enabled_servers
            .iter()
            .filter_map(|(server_id, _server_name, eligible)| eligible.then_some(server_id.clone()))
            .collect();

        Ok(UcanCatalogScope {
            client_context: client_context.clone(),
            snapshot,
            visible_server_ids,
            enabled_servers,
            eligible_server_ids,
        })
    }

    #[cfg(test)]
    pub(crate) async fn resolve_current_broker_resource_route(
        &self,
        context: &ClientBuiltinContext,
        external_uri: &str,
    ) -> Result<Option<crate::core::capability::resource_registry::ResolvedResourceRoute>> {
        self.resolve_current_broker_resource_route_for_client(&context.as_client_context(), external_uri)
            .await
    }

    pub(crate) async fn resolve_current_broker_resource_route_for_client(
        &self,
        client_context: &ClientContext,
        external_uri: &str,
    ) -> Result<Option<crate::core::capability::resource_registry::ResolvedResourceRoute>> {
        let route =
            crate::core::capability::resource_registry::resolve_resource_route(&self.database.pool, external_uri)
                .await
                .with_context(|| format!("Failed to resolve canonical resource URI '{external_uri}'"))?;
        if matches!(
            &route.source,
            crate::core::capability::resource_registry::ResourceRouteSource::Issued
        ) {
            return Ok(None);
        }

        let scope = self.resolve_ucan_catalog_scope_for_client(client_context).await?;
        if !scope.visible_server_ids.contains(&route.server_id)
            || !scope
                .enabled_servers
                .iter()
                .any(|(server_id, _, _)| server_id == &route.server_id)
        {
            return Ok(None);
        }
        if crate::core::proxy::server::unify_directly_exposed_resource_route_allowed(
            client_context.unify_workspace.as_ref(),
            &scope.eligible_server_ids,
            &route.server_id,
            &route,
        ) {
            return Ok(None);
        }

        let capability = match &route.source {
            crate::core::capability::resource_registry::ResourceRouteSource::Listed => {
                crate::core::capability::CapabilityType::Resources
            }
            crate::core::capability::resource_registry::ResourceRouteSource::Template { .. } => {
                crate::core::capability::CapabilityType::ResourceTemplates
            }
            crate::core::capability::resource_registry::ResourceRouteSource::Issued => unreachable!(),
        };
        let ctx = crate::core::capability::runtime::ListCtx {
            capability,
            server_id: route.server_id.clone(),
            refresh: Some(crate::core::capability::runtime::RefreshStrategy::CacheFirst),
            operation_timeout: crate::core::transport::timeout_policy::DEFAULT_CAPABILITY_OPERATION_TIMEOUT,
            validation_session: None,
            runtime_identity: scope.client_context.runtime_identity(),
            connection_selection: scope.client_context.connection_selection(route.server_id.clone()),
            visibility_snapshot: Some(Arc::new(scope.snapshot)),
            name_domain: crate::core::capability::runtime::NameDomain::External,
        };
        let catalog = match crate::core::capability::read_service::CapabilityReadService::from_runtime(
            self.database.clone(),
            self.connection_pool.clone(),
        )
        .catalog_only(&ctx)
        .await
        {
            Ok(Some(catalog)) => catalog,
            Ok(None) => {
                return Err(catalog_authority_error_for_route(
                    &route.server_id,
                    &route.server_name,
                    anyhow!("trusted catalog snapshot is unavailable"),
                ));
            }
            Err(error) => {
                return Err(catalog_authority_error_for_route(
                    &route.server_id,
                    &route.server_name,
                    anyhow::Error::new(error),
                ));
            }
        };

        let current = match (&route.source, catalog.items) {
            (crate::core::capability::resource_registry::ResourceRouteSource::Listed, items) => {
                let resources = items.into_resources().ok_or_else(|| {
                    anyhow!(
                        "Resource catalog returned a different capability kind for server '{}'",
                        route.server_id
                    )
                })?;
                resources
                    .iter()
                    .any(|resource| resource.uri.as_str() == route.external_uri.as_str())
            }
            (
                crate::core::capability::resource_registry::ResourceRouteSource::Template { upstream_template, .. },
                items,
            ) => {
                let templates = items.into_resource_templates().ok_or_else(|| {
                    anyhow!(
                        "Resource template catalog returned a different capability kind for server '{}'",
                        route.server_id
                    )
                })?;
                let mut current = false;
                for template in templates {
                    let raw_template = crate::core::proxy::server::resolve_direct_surface_value(
                        NamingKind::ResourceTemplate,
                        &route.server_id,
                        template.uri_template.as_ref(),
                    )
                    .await?;
                    if raw_template == *upstream_template {
                        current = true;
                        break;
                    }
                }
                current
            }
            (crate::core::capability::resource_registry::ResourceRouteSource::Issued, _) => unreachable!(),
        };

        Ok(current.then_some(route))
    }

    async fn visible_tools_listing(
        &self,
        context: &ClientBuiltinContext,
    ) -> Result<VisibleListing<VisibleToolEntry>> {
        let scope = self.resolve_ucan_catalog_scope(context).await?;
        self.visible_tools_listing_with_scope(context, &scope).await
    }

    async fn visible_tools_listing_with_scope(
        &self,
        context: &ClientBuiltinContext,
        scope: &UcanCatalogScope,
    ) -> Result<VisibleListing<VisibleToolEntry>> {
        let visibility = ProfileVisibilityService::new(Some(self.database.clone()), None);
        let database = self.database.clone();
        let connection_pool = self.connection_pool.clone();
        let runtime_identity = scope.client_context.runtime_identity();

        let mut tasks = Vec::new();
        for (server_id, server_name, _unify_direct_exposure_eligible) in scope.enabled_servers.iter().cloned() {
            if !scope.visible_server_ids.contains(&server_id) {
                continue;
            }

            let ctx = crate::core::capability::runtime::ListCtx {
                capability: crate::core::capability::CapabilityType::Tools,
                server_id: server_id.clone(),
                refresh: Some(crate::core::capability::runtime::RefreshStrategy::CacheFirst),
                operation_timeout: crate::core::transport::timeout_policy::DEFAULT_CAPABILITY_OPERATION_TIMEOUT,
                validation_session: None,
                runtime_identity: runtime_identity.clone(),
                connection_selection: scope.client_context.connection_selection(server_id.clone()),
                visibility_snapshot: Some(Arc::new(scope.snapshot.clone())),
                name_domain: crate::core::capability::runtime::NameDomain::External,
            };
            let database = database.clone();
            let connection_pool = connection_pool.clone();
            tasks.push(async move {
                let service = crate::core::capability::read_service::CapabilityReadService::from_runtime(
                    database,
                    connection_pool,
                );
                let result = service.list(&ctx).await;
                (server_id, server_name, result)
            });
        }

        let mut visible = Vec::new();
        let mut aggregate = AggregateListStatus::new("tools");
        for (server_id, server_name, result) in futures::stream::iter(tasks)
            .buffer_unordered(crate::core::capability::facade::concurrency_limit())
            .collect::<Vec<_>>()
            .await
        {
            let server_tools = async {
                let result = result.with_context(|| format!("Failed to list tools for server '{server_id}'"))?;
                let tools = result.items.into_tools().ok_or_else(|| {
                    anyhow!("Tool listing returned a different capability kind for server '{server_id}'")
                })?;
                let mut entries = Vec::with_capacity(tools.len());
                for tool in tools {
                    let raw_tool_name = crate::core::proxy::server::resolve_direct_surface_value(
                        NamingKind::Tool,
                        &server_id,
                        tool.name.as_ref(),
                    )
                    .await?;
                    entries.push(VisibleToolEntry {
                        server_id: server_id.clone(),
                        server_name: server_name.clone(),
                        raw_tool_name,
                        tool,
                    });
                }
                Ok::<_, anyhow::Error>(entries)
            }
            .await;
            match server_tools {
                Ok(entries) => {
                    aggregate.record_success();
                    visible.extend(entries);
                }
                Err(error) => aggregate.record_failure(&server_id, &server_name, error),
            }
        }
        let filtered_names = visibility
            .filter_tools_with_snapshot(
                &scope.snapshot,
                visible.iter().map(|entry| entry.tool.clone()).collect(),
            )
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<HashSet<_>>();
        visible.retain(|entry| filtered_names.contains(entry.tool.name.as_ref()));
        retain_brokered_tools(context, &scope.eligible_server_ids, &mut visible);
        visible.sort_by(|left, right| {
            left.server_name
                .cmp(&right.server_name)
                .then_with(|| left.tool.name.as_ref().cmp(right.tool.name.as_ref()))
        });

        Ok(VisibleListing {
            entries: visible,
            aggregate,
        })
    }

    async fn visible_tools(
        &self,
        context: &ClientBuiltinContext,
    ) -> Result<Vec<VisibleToolEntry>> {
        self.visible_tools_listing(context).await?.finish()
    }

    async fn find_visible_tool(
        &self,
        context: &ClientBuiltinContext,
        tool_name: &str,
    ) -> Result<Option<VisibleToolEntry>> {
        self.visible_tools_listing(context)
            .await?
            .find_authoritatively(|entry| entry.tool.name.as_ref() == tool_name)
    }

    async fn visible_prompts_listing(
        &self,
        context: &ClientBuiltinContext,
    ) -> Result<VisibleListing<VisiblePromptEntry>> {
        let scope = self.resolve_ucan_catalog_scope(context).await?;
        self.visible_prompts_listing_with_scope(context, &scope).await
    }

    async fn visible_prompts_listing_with_scope(
        &self,
        context: &ClientBuiltinContext,
        scope: &UcanCatalogScope,
    ) -> Result<VisibleListing<VisiblePromptEntry>> {
        let visibility = ProfileVisibilityService::new(Some(self.database.clone()), None);
        let database = self.database.clone();
        let connection_pool = self.connection_pool.clone();
        let runtime_identity = scope.client_context.runtime_identity();

        let mut tasks = Vec::new();
        for (server_id, server_name, _unify_direct_exposure_eligible) in scope.enabled_servers.iter().cloned() {
            if !scope.visible_server_ids.contains(&server_id) {
                continue;
            }
            let ctx = crate::core::capability::runtime::ListCtx {
                capability: crate::core::capability::CapabilityType::Prompts,
                server_id: server_id.clone(),
                refresh: Some(crate::core::capability::runtime::RefreshStrategy::CacheFirst),
                operation_timeout: crate::core::transport::timeout_policy::DEFAULT_CAPABILITY_OPERATION_TIMEOUT,
                validation_session: None,
                runtime_identity: runtime_identity.clone(),
                connection_selection: scope.client_context.connection_selection(server_id.clone()),
                visibility_snapshot: Some(Arc::new(scope.snapshot.clone())),
                name_domain: crate::core::capability::runtime::NameDomain::External,
            };
            let database = database.clone();
            let connection_pool = connection_pool.clone();
            let server_name_cloned = server_name.clone();
            tasks.push(async move {
                let service = crate::core::capability::read_service::CapabilityReadService::from_runtime(
                    database,
                    connection_pool,
                );
                let result = service.list(&ctx).await;
                (server_id, server_name_cloned, result)
            });
        }

        let mut visible = Vec::new();
        let mut aggregate = AggregateListStatus::new("prompts");
        for (server_id, server_name, result) in futures::stream::iter(tasks)
            .buffer_unordered(crate::core::capability::facade::concurrency_limit())
            .collect::<Vec<_>>()
            .await
        {
            let server_prompts = async {
                let result = result.with_context(|| format!("Failed to list prompts for server '{server_id}'"))?;
                let prompts = result.items.into_prompts().ok_or_else(|| {
                    anyhow!("Prompt listing returned a different capability kind for server '{server_id}'")
                })?;
                let mut entries = Vec::with_capacity(prompts.len());
                for prompt in prompts {
                    let presented_name = prompt.name.to_string();
                    let raw_prompt_name = crate::core::proxy::server::resolve_direct_surface_value(
                        NamingKind::Prompt,
                        &server_id,
                        &presented_name,
                    )
                    .await?;
                    entries.push(VisiblePromptEntry {
                        server_id: server_id.clone(),
                        server_name: server_name.clone(),
                        raw_prompt_name,
                        prompt,
                    });
                }
                Ok::<_, anyhow::Error>(entries)
            }
            .await;
            match server_prompts {
                Ok(entries) => {
                    aggregate.record_success();
                    visible.extend(entries);
                }
                Err(error) => aggregate.record_failure(&server_id, &server_name, error),
            }
        }
        let filtered_names = visibility
            .filter_prompts_with_snapshot(
                &scope.snapshot,
                visible.iter().map(|entry| entry.prompt.clone()).collect(),
            )
            .into_iter()
            .map(|prompt| prompt.name.to_string())
            .collect::<HashSet<_>>();
        visible.retain(|entry| filtered_names.contains(entry.prompt.name.as_str()));
        retain_brokered_prompts(context, &scope.eligible_server_ids, &mut visible);
        visible.sort_by(|left, right| {
            left.server_name
                .cmp(&right.server_name)
                .then_with(|| left.prompt.name.as_str().cmp(right.prompt.name.as_str()))
        });

        Ok(VisibleListing {
            entries: visible,
            aggregate,
        })
    }

    async fn visible_prompts(
        &self,
        context: &ClientBuiltinContext,
    ) -> Result<Vec<VisiblePromptEntry>> {
        self.visible_prompts_listing(context).await?.finish()
    }

    async fn find_visible_prompt(
        &self,
        context: &ClientBuiltinContext,
        prompt_name: &str,
    ) -> Result<Option<VisiblePromptEntry>> {
        self.visible_prompts_listing(context)
            .await?
            .find_authoritatively(|entry| entry.prompt.name.as_str() == prompt_name)
    }

    async fn visible_resources_listing(
        &self,
        context: &ClientBuiltinContext,
    ) -> Result<VisibleListing<VisibleResourceEntry>> {
        let scope = self.resolve_ucan_catalog_scope(context).await?;
        self.visible_resources_listing_with_scope(context, &scope).await
    }

    async fn visible_resources_listing_with_scope(
        &self,
        context: &ClientBuiltinContext,
        scope: &UcanCatalogScope,
    ) -> Result<VisibleListing<VisibleResourceEntry>> {
        let visibility = ProfileVisibilityService::new(Some(self.database.clone()), None);
        let database = self.database.clone();
        let connection_pool = self.connection_pool.clone();
        let runtime_identity = scope.client_context.runtime_identity();

        let mut tasks = Vec::new();
        for (server_id, server_name, _unify_direct_exposure_eligible) in scope.enabled_servers.iter().cloned() {
            if !scope.visible_server_ids.contains(&server_id) {
                continue;
            }
            let ctx = crate::core::capability::runtime::ListCtx {
                capability: crate::core::capability::CapabilityType::Resources,
                server_id: server_id.clone(),
                refresh: Some(crate::core::capability::runtime::RefreshStrategy::CacheFirst),
                operation_timeout: crate::core::transport::timeout_policy::DEFAULT_CAPABILITY_OPERATION_TIMEOUT,
                validation_session: None,
                runtime_identity: runtime_identity.clone(),
                connection_selection: scope.client_context.connection_selection(server_id.clone()),
                visibility_snapshot: Some(Arc::new(scope.snapshot.clone())),
                name_domain: crate::core::capability::runtime::NameDomain::External,
            };
            let database = database.clone();
            let connection_pool = connection_pool.clone();
            let server_name_cloned = server_name.clone();
            tasks.push(async move {
                let service = crate::core::capability::read_service::CapabilityReadService::from_runtime(
                    database,
                    connection_pool,
                );
                let result = service.list(&ctx).await;
                (server_id, server_name_cloned, result)
            });
        }

        let mut visible = Vec::new();
        let mut aggregate = AggregateListStatus::new("resources");
        for (server_id, server_name, result) in futures::stream::iter(tasks)
            .buffer_unordered(crate::core::capability::facade::concurrency_limit())
            .collect::<Vec<_>>()
            .await
        {
            let server_resources = async {
                let result = result.with_context(|| format!("Failed to list resources for server '{server_id}'"))?;
                let resources = result.items.into_resources().ok_or_else(|| {
                    anyhow!("Resource listing returned a different capability kind for server '{server_id}'")
                })?;
                let mut entries = Vec::with_capacity(resources.len());
                for resource in resources {
                    let presented_uri = resource.uri.to_string();
                    let raw_resource_uri = crate::core::proxy::server::resolve_direct_surface_value(
                        NamingKind::Resource,
                        &server_id,
                        &presented_uri,
                    )
                    .await?;
                    entries.push(VisibleResourceEntry {
                        server_id: server_id.clone(),
                        server_name: server_name.clone(),
                        raw_resource_uri,
                        resource,
                    });
                }
                Ok::<_, anyhow::Error>(entries)
            }
            .await;
            match server_resources {
                Ok(entries) => {
                    aggregate.record_success();
                    visible.extend(entries);
                }
                Err(error) => aggregate.record_failure(&server_id, &server_name, error),
            }
        }
        let filtered_names = visibility
            .filter_resources_with_snapshot(
                &scope.snapshot,
                visible.iter().map(|entry| entry.resource.clone()).collect(),
                Vec::new(),
            )
            .0
            .into_iter()
            .map(|resource| resource.uri.to_string())
            .collect::<HashSet<_>>();
        visible.retain(|entry| filtered_names.contains(entry.resource.uri.as_str()));
        retain_brokered_resources(context, &scope.eligible_server_ids, &mut visible);
        visible.sort_by(|left, right| {
            left.server_name
                .cmp(&right.server_name)
                .then_with(|| left.resource.uri.as_str().cmp(right.resource.uri.as_str()))
        });

        Ok(VisibleListing {
            entries: visible,
            aggregate,
        })
    }

    async fn visible_resources(
        &self,
        context: &ClientBuiltinContext,
    ) -> Result<Vec<VisibleResourceEntry>> {
        self.visible_resources_listing(context).await?.finish()
    }

    async fn find_visible_resource(
        &self,
        context: &ClientBuiltinContext,
        resource_name: &str,
    ) -> Result<Option<VisibleResourceEntry>> {
        self.visible_resources_listing(context)
            .await?
            .find_authoritatively(|entry| entry.resource.uri.as_str() == resource_name)
    }

    async fn visible_resource_templates_listing(
        &self,
        context: &ClientBuiltinContext,
    ) -> Result<VisibleListing<VisibleResourceTemplateEntry>> {
        let scope = self.resolve_ucan_catalog_scope(context).await?;
        self.visible_resource_templates_listing_with_scope(context, &scope)
            .await
    }

    async fn visible_resource_templates_listing_with_scope(
        &self,
        context: &ClientBuiltinContext,
        scope: &UcanCatalogScope,
    ) -> Result<VisibleListing<VisibleResourceTemplateEntry>> {
        let visibility = ProfileVisibilityService::new(Some(self.database.clone()), None);
        let database = self.database.clone();
        let connection_pool = self.connection_pool.clone();
        let runtime_identity = scope.client_context.runtime_identity();

        let mut tasks = Vec::new();
        for (server_id, server_name, _unify_direct_exposure_eligible) in scope.enabled_servers.iter().cloned() {
            if !scope.visible_server_ids.contains(&server_id) {
                continue;
            }
            let ctx = crate::core::capability::runtime::ListCtx {
                capability: crate::core::capability::CapabilityType::ResourceTemplates,
                server_id: server_id.clone(),
                refresh: Some(crate::core::capability::runtime::RefreshStrategy::CacheFirst),
                operation_timeout: crate::core::transport::timeout_policy::DEFAULT_CAPABILITY_OPERATION_TIMEOUT,
                validation_session: None,
                runtime_identity: runtime_identity.clone(),
                connection_selection: scope.client_context.connection_selection(server_id.clone()),
                visibility_snapshot: Some(Arc::new(scope.snapshot.clone())),
                name_domain: crate::core::capability::runtime::NameDomain::External,
            };
            let database = database.clone();
            let connection_pool = connection_pool.clone();
            let server_name_cloned = server_name.clone();
            tasks.push(async move {
                let service = crate::core::capability::read_service::CapabilityReadService::from_runtime(
                    database,
                    connection_pool,
                );
                let result = service.list(&ctx).await;
                (server_id, server_name_cloned, result)
            });
        }

        let mut visible = Vec::new();
        let mut aggregate = AggregateListStatus::new("resource templates");
        for (server_id, server_name, result) in futures::stream::iter(tasks)
            .buffer_unordered(crate::core::capability::facade::concurrency_limit())
            .collect::<Vec<_>>()
            .await
        {
            let server_templates = async {
                let result =
                    result.with_context(|| format!("Failed to list resource templates for server '{server_id}'"))?;
                let templates = result.items.into_resource_templates().ok_or_else(|| {
                    anyhow!("Resource template listing returned a different capability kind for server '{server_id}'")
                })?;
                let mut entries = Vec::with_capacity(templates.len());
                for resource_template in templates {
                    let raw_uri_template = crate::core::proxy::server::resolve_direct_surface_value(
                        NamingKind::ResourceTemplate,
                        &server_id,
                        resource_template.uri_template.as_ref(),
                    )
                    .await?;
                    entries.push(VisibleResourceTemplateEntry {
                        server_id: server_id.clone(),
                        server_name: server_name.clone(),
                        raw_uri_template,
                        resource_template,
                    });
                }
                Ok::<_, anyhow::Error>(entries)
            }
            .await;
            match server_templates {
                Ok(entries) => {
                    aggregate.record_success();
                    visible.extend(entries);
                }
                Err(error) => aggregate.record_failure(&server_id, &server_name, error),
            }
        }
        let filtered_names = visibility
            .filter_resources_with_snapshot(
                &scope.snapshot,
                Vec::new(),
                visible.iter().map(|entry| entry.resource_template.clone()).collect(),
            )
            .1
            .into_iter()
            .map(|template| template.uri_template.to_string())
            .collect::<HashSet<_>>();
        visible.retain(|entry| filtered_names.contains(entry.resource_template.uri_template.as_str()));
        retain_brokered_resource_templates(context, &scope.eligible_server_ids, &mut visible);
        visible.sort_by(|left, right| {
            left.server_name.cmp(&right.server_name).then_with(|| {
                left.resource_template
                    .uri_template
                    .as_str()
                    .cmp(right.resource_template.uri_template.as_str())
            })
        });

        Ok(VisibleListing {
            entries: visible,
            aggregate,
        })
    }

    async fn visible_resource_templates(
        &self,
        context: &ClientBuiltinContext,
    ) -> Result<Vec<VisibleResourceTemplateEntry>> {
        self.visible_resource_templates_listing(context).await?.finish()
    }

    async fn find_visible_resource_template(
        &self,
        context: &ClientBuiltinContext,
        template_name: &str,
    ) -> Result<Option<VisibleResourceTemplateEntry>> {
        self.visible_resource_templates_listing(context)
            .await?
            .find_authoritatively(|entry| entry.resource_template.uri_template.as_str() == template_name)
    }

    async fn broker_prompt_call(
        &self,
        context: &ClientBuiltinContext,
        prompt_name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<CallToolResult> {
        let prompt_entry = match self.find_visible_prompt(context, prompt_name).await {
            Err(error) if is_catalog_authority_error(&error) => {
                return Ok(UcanError::catalog_incomplete().to_call_tool_result());
            }
            Err(error) => return Err(error),
            Ok(None) => {
                let surface_names = self
                    .collect_surface_names_for_kind(context, SurfaceKind::Prompt)
                    .await?;
                return Ok(UcanError::capability_not_found("prompt", prompt_name, &surface_names).to_call_tool_result());
            }
            Ok(Some(prompt_entry)) => prompt_entry,
        };
        {
            let required_arguments = required_arguments_from_prompt(&prompt_entry.prompt);
            let missing_required: Vec<String> = required_arguments
                .into_iter()
                .filter(|name| !arguments.contains_key(name))
                .collect();
            if !missing_required.is_empty() {
                return Ok(
                    UcanError::missing_required_arguments("prompt", prompt_name, &missing_required)
                        .to_call_tool_result(),
                );
            }
        }

        let client_context = context.as_client_context();
        let visibility = ProfileVisibilityService::new(Some(self.database.clone()), None);
        let snapshot = visibility
            .resolve_snapshot_for_client(&client_context)
            .await
            .context("Failed to resolve Unify visibility snapshot")?;
        if visibility
            .assert_prompt_allowed_with_snapshot(&snapshot, prompt_name)
            .await
            .is_err()
        {
            return Ok(UcanError::visibility_denied("prompt", prompt_name).to_call_tool_result());
        }

        let route = resolve_capability_route(NamingKind::Prompt, prompt_name)
            .await
            .with_context(|| format!("Failed to resolve unique prompt '{}'", prompt_name))?;
        let server_id = route.server_id;
        let server_name = route.server_name;
        let upstream_prompt_name = route.upstream_value;

        let filter: HashSet<_> = std::iter::once(server_id.clone()).collect();
        let prompt_mapping =
            crate::core::capability::facade::build_prompt_mapping_filtered(&self.connection_pool, Some(&filter))
                .await
                .context("Failed to build routed prompt inventory")?;

        match crate::core::capability::facade::get_upstream_prompt(
            &self.connection_pool,
            &prompt_mapping,
            &upstream_prompt_name,
            Some(arguments),
            Some(server_id.as_str()),
            client_context.connection_selection(server_id.clone()).as_ref(),
        )
        .await
        {
            Ok(mut result) => {
                crate::core::capability::resource_uri::rewrite_get_prompt_result(
                    &self.database.pool,
                    &server_id,
                    &server_name,
                    &mut result,
                )
                .await?;
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    serde_json::to_string_pretty(&result).context("Failed to serialize Unify prompt result")?,
                )]))
            }
            Err(e) => {
                tracing::warn!(
                    capability_kind = "prompt",
                    capability_name = prompt_name,
                    server_id,
                    server_name,
                    error = %e,
                    "Upstream capability invocation failed"
                );
                self.record_usage_evidence(&server_id, mcpmate_capability_store::CapabilityKind::Prompts, &e)
                    .await;
                Ok(UcanError::upstream_error("prompt", prompt_name).to_call_tool_result())
            }
        }
    }

    async fn record_usage_evidence(
        &self,
        server_id: &str,
        kind: mcpmate_capability_store::CapabilityKind,
        error: &anyhow::Error,
    ) {
        crate::core::capability::runtime::record_capability_usage_evidence(
            &self.database,
            server_id,
            kind,
            None,
            error,
        )
        .await;
    }

    async fn acquire_peer(
        &self,
        client_context: &ClientContext,
        server_id: &str,
    ) -> Result<rmcp::service::Peer<rmcp::RoleClient>> {
        let peer_opt = {
            let pool_guard = self.connection_pool.lock().await;
            let snapshot = pool_guard.get_snapshot();
            let mut peer: Option<rmcp::service::Peer<rmcp::RoleClient>> = None;

            if let Some(selection) = client_context.connection_selection(server_id.to_string()) {
                if let Ok(Some(selected_instance_id)) = pool_guard.select_ready_instance_id(&selection) {
                    if let Some(instances) = snapshot.get(server_id) {
                        if let Some((_, _, _, _, selected_peer)) =
                            instances.iter().find(|(candidate_id, _, _, _, peer)| {
                                **candidate_id == selected_instance_id && peer.is_some()
                            })
                        {
                            peer = selected_peer.clone();
                        }
                    }
                }
            }

            if peer.is_none() {
                if let Some(instances) = snapshot.get(server_id) {
                    if let Some((_, _, _, _, selected_peer)) = instances
                        .iter()
                        .find(|(_, status, _, _, peer)| matches!(status, ConnectionStatus::Ready) && peer.is_some())
                    {
                        peer = selected_peer.clone();
                    }
                }
            }

            peer
        };

        if let Some(peer) = peer_opt {
            return Ok(peer);
        }

        {
            let selection = client_context
                .connection_selection(server_id.to_string())
                .unwrap_or_else(|| crate::core::capability::ConnectionSelection {
                    server_id: server_id.to_string(),
                    affinity_key: crate::core::capability::AffinityKey::Default,
                });
            UpstreamConnectionPool::ensure_connected_coordinated(&self.connection_pool, &selection)
                .await
                .with_context(|| format!("Failed to connect Unify broker to server '{}'", server_id))?;
        }

        let pool_guard = self.connection_pool.lock().await;
        let snapshot = pool_guard.get_snapshot();
        let instances = snapshot
            .get(server_id)
            .ok_or_else(|| anyhow!("No instance found after connecting to server '{}'", server_id))?;
        let (_, _, _, _, peer) = instances
            .iter()
            .find(|(_, status, _, _, peer)| matches!(status, ConnectionStatus::Ready) && peer.is_some())
            .ok_or_else(|| anyhow!("Ready instance not found for server '{}'", server_id))?;

        peer.clone()
            .ok_or_else(|| anyhow!("Ready peer missing for server '{}'", server_id))
    }
}

#[async_trait::async_trait]
impl BuiltinService for BrokerService {
    fn name(&self) -> &'static str {
        "mcpmate_broker"
    }

    fn tools(&self) -> Vec<Tool> {
        let prompt_config = UCAN_PROMPT_REPO.get_blocking();
        vec![
            Tool::new(
                MCPMATE_UCAN_CATALOG_TOOL,
                prompt_config.catalog_tool_description.clone(),
                std::sync::Arc::new(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "page": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "Surface directory page number. Start with 1."
                            },
                            "page_size": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": 50,
                                "description": "Number of items per page. Use a small page size to reduce token usage."
                            },
                            "search": {
                                "type": "string",
                                "description": "Case-insensitive substring search in surface item name and summary fields."
                            },
                            "kind_filter": {
                                "type": "array",
                                "items": {
                                    "type": "string",
                                    "enum": ["tool", "prompt", "resource", "resource_template", "profile"]
                                },
                                "description": "Filter by surface kind. Returns only matching kinds."
                            }
                        },
                        "required": []
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
            ),
            Tool::new(
                MCPMATE_UCAN_DETAILS_TOOL,
                prompt_config.details_tool_description.clone(),
                std::sync::Arc::new(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "capability_kind": {
                                "type": "string",
                                "enum": ["tool", "prompt", "resource", "resource_template", "profile"],
                                "description": "Surface kind returned by mcpmate_ucan_catalog"
                            },
                            "capability_name": {
                                "type": "string",
                                "description": "Surface item name returned by mcpmate_ucan_catalog"
                            },
                            "detail_level": {
                                "type": "string",
                                "enum": ["summary", "full"],
                                "description": "Use summary first. Use full only when summary is not enough for safe calling or selection."
                            }
                        },
                        "required": ["capability_kind", "capability_name"]
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
            ),
            Tool::new(
                MCPMATE_UCAN_CALL_TOOL,
                prompt_config.call_tool_description.clone(),
                std::sync::Arc::new(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "capability_kind": {
                                "type": "string",
                                "enum": ["tool", "prompt", "resource", "resource_template", "profile"],
                                "description": "Surface kind returned by mcpmate_ucan_catalog"
                            },
                            "capability_name": {
                                "type": "string",
                                "description": "Surface item name returned by mcpmate_ucan_catalog"
                            },
                            "arguments": {
                                "type": "object",
                                "description": "Arguments for Tool or Prompt targets only. Resource and ResourceTemplate targets use a concrete canonical URI with resources/read."
                            }
                        },
                        "required": ["capability_kind", "capability_name"]
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
            ),
        ]
    }

    async fn call_tool(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<CallToolResult> {
        Ok(UcanError::context_required(&request.name).to_call_tool_result())
    }

    async fn call_tool_with_context(
        &self,
        request: &CallToolRequestParams,
        context: Option<&ClientBuiltinContext>,
    ) -> Result<CallToolResult> {
        let context = match context {
            Some(ctx) => ctx,
            None => {
                return Ok(UcanError::context_required(&request.name).to_call_tool_result());
            }
        };

        match request.name.as_ref() {
            MCPMATE_UCAN_CATALOG_TOOL => {
                let args = serde_json::Value::Object(request.arguments.clone().unwrap_or_default());
                match serde_json::from_value::<CatalogParams>(args) {
                    Ok(params) => {
                        self.tool_catalog(
                            context,
                            params.page,
                            params.page_size,
                            params.search.as_deref(),
                            params.kind_filter.as_deref(),
                        )
                        .await
                    }
                    Err(e) => {
                        Ok(UcanError::invalid_parameters(MCPMATE_UCAN_CATALOG_TOOL, &e.to_string())
                            .to_call_tool_result())
                    }
                }
            }
            MCPMATE_UCAN_DETAILS_TOOL => {
                let args = serde_json::Value::Object(request.arguments.clone().unwrap_or_default());
                match serde_json::from_value::<SurfaceLookupParams>(args) {
                    Ok(params) => {
                        self.tool_details(
                            context,
                            params.capability_kind,
                            &params.capability_name,
                            params.detail_level,
                        )
                        .await
                    }
                    Err(e) => {
                        Ok(UcanError::invalid_parameters(MCPMATE_UCAN_DETAILS_TOOL, &e.to_string())
                            .to_call_tool_result())
                    }
                }
            }
            MCPMATE_UCAN_CALL_TOOL => {
                let args = serde_json::Value::Object(request.arguments.clone().unwrap_or_default());
                match serde_json::from_value::<SurfaceCallParams>(args) {
                    Ok(params) => {
                        self.broker_tool_call(
                            context,
                            params.capability_kind,
                            &params.capability_name,
                            params.arguments,
                        )
                        .await
                    }
                    Err(e) => {
                        Ok(UcanError::invalid_parameters(MCPMATE_UCAN_CALL_TOOL, &e.to_string()).to_call_tool_result())
                    }
                }
            }
            _ => Ok(UcanError::unknown_tool(&request.name).to_call_tool_result()),
        }
    }
}

impl ClientBuiltinContext {
    pub(crate) fn as_client_context(&self) -> ClientContext {
        ClientContext {
            client_id: self.client_id.clone(),
            session_id: self.session_id.clone(),
            profile_id: None,
            config_mode: self.config_mode.clone(),
            unify_workspace: self.unify_workspace.clone(),
            surface_fingerprint: None,
            transport: ClientTransport::Other,
            source: ClientIdentitySource::SessionBinding,
            observed_client_info: None,
        }
    }
}

fn default_catalog_page() -> usize {
    1
}

fn default_catalog_page_size() -> usize {
    default_ucan_prompt_config().catalog_page_size_default
}

fn default_ucan_prompt_config() -> UcanPromptConfig {
    UcanPromptConfig {
        catalog_tool_description: "MCPMATE SURFACE DIRECTORY\nROLE: Discover the current MCP surface exposed to this client.\nUSE_WHEN: Before starting any task, call this first to find the most relevant surface item.\nRETURNS: A paginated surface directory with lightweight summaries.\nWORKFLOW: directory -> details -> invoke, read, or select.\nRULES: Use the current page first. If you still have not found a good match, request the next page instead of expanding everything at once.\nExample: mcpmate_ucan_catalog(page=1, page_size=10) or mcpmate_ucan_catalog(page=1, page_size=10, kind_filter=[\"tool\"]) or mcpmate_ucan_catalog(page=1, page_size=10, search=\"github\")".to_string(),
        details_tool_description: "MCPMATE SURFACE INSPECTOR\nROLE: Explain how to use one surface item selected from MCPMate's surface directory.\nUSE_WHEN: After directory, before invocation, standard resource read, or profile selection.\nRETURNS: Summary or full details for the selected surface item.\nWORKFLOW: Use summary first for quick judgment. Use full only when you need complete metadata.\nRULES: Do not inspect unrelated surface items in full.\nExample: mcpmate_ucan_details(capability_kind=\"tool\", capability_name=\"github_search\", detail_level=\"summary\")".to_string(),
        call_tool_description: "MCPMATE SURFACE CALLER\nROLE: Call one Tool or Prompt selected from MCPMate's surface directory.\nUSE_WHEN: Only after you already know which callable surface item to use.\nRETURNS: The execution result produced by the selected surface item.\nWORKFLOW: directory -> details -> call for Tool or Prompt; use resources/read for Resource or ResourceTemplate targets.\nRULES: Do not use this tool to read resources. Use details first when arguments or behavior are unclear.\nExample: mcpmate_ucan_call(capability_kind=\"tool\", capability_name=\"github_search\", arguments={\"query\":\"rust mcp\"})".to_string(),
        catalog_usage: "Use mcpmate_ucan_catalog to choose one surface item, then inspect it with mcpmate_ucan_details. Call Tool or Prompt with mcpmate_ucan_call. For Resource or ResourceTemplate, use Details to obtain or expand the concrete canonical URI, then invoke resources/read.".to_string(),
        catalog_stale_hint: default_catalog_stale_hint(),
        error_recovery_hint: default_error_recovery_hint(),
        catalog_format: vec![
            "capability_name".to_string(),
            "capability_kind".to_string(),
            "summary".to_string(),
            "action".to_string(),
            "next_step".to_string(),
            "server_id".to_string(),
            "server_name".to_string(),
            "interaction_mode".to_string(),
            "detail_hint".to_string(),
            "source".to_string(),
        ],
        catalog_page_size_default: 20,
        catalog_page_size_max: 50,
        catalog_sort_weights: CatalogSortWeights::default(),
        workflow_hints: WorkflowHints::default(),
        catalog_enrich_from_source: true,
        profile_get_description: default_profile_get_description(),
        profile_set_description: default_profile_set_description(),
        profile_add_description: default_profile_add_description(),
        profile_remove_description: default_profile_remove_description(),
    }
}

fn resolve_ucan_prompt_config_override_path() -> Result<Option<PathBuf>> {
    crate::common::json5_config::resolve_env_config_override(
        "MCPMATE_UCAN_CONFIG",
        "Create PathService for UCAN prompt config",
    )
}

const BUNDLED_UCAN_PROMPT_CONFIG: &str = include_str!("../../../config/ucan.json5");
const UCAN_PROMPT_CONFIG_LABEL: &str = "UCAN prompt config";

/// Load UCAN prompt copy from override path or bundled JSON5.
///
/// Override failures return an error; callers should fall back to [`default_ucan_prompt_config`].
fn load_ucan_prompt_config_blocking() -> Result<UcanPromptConfig> {
    if let Some(path) = resolve_ucan_prompt_config_override_path()? {
        return load_ucan_prompt_config_from_path(&path);
    }
    parse_ucan_prompt_config(BUNDLED_UCAN_PROMPT_CONFIG, "bundled ucan.json5")
}

fn load_ucan_prompt_config_from_path(path: &std::path::Path) -> Result<UcanPromptConfig> {
    let config = crate::common::json5_config::load_json5_object_from_path(path, UCAN_PROMPT_CONFIG_LABEL)?;
    Ok(normalize_ucan_prompt_config(config))
}

fn parse_ucan_prompt_config(
    content: &str,
    source: &str,
) -> Result<UcanPromptConfig> {
    let config = crate::common::json5_config::parse_json5_object(content, source, UCAN_PROMPT_CONFIG_LABEL)?;
    Ok(normalize_ucan_prompt_config(config))
}

fn normalize_multiline_text(raw: &str) -> String {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<String> = normalized.lines().map(|line| line.trim_end().to_string()).collect();
    lines.join("\n").trim().to_string()
}

fn normalize_string_list(items: Vec<String>) -> Vec<String> {
    items
        .into_iter()
        .map(|item| normalize_multiline_text(&item))
        .filter(|item| !item.is_empty())
        .collect()
}

fn normalize_ucan_prompt_config(mut config: UcanPromptConfig) -> UcanPromptConfig {
    config.catalog_tool_description = normalize_multiline_text(&config.catalog_tool_description);
    config.details_tool_description = normalize_multiline_text(&config.details_tool_description);
    config.call_tool_description = normalize_multiline_text(&config.call_tool_description);
    config.catalog_usage = normalize_multiline_text(&config.catalog_usage);
    config.catalog_stale_hint = normalize_multiline_text(&config.catalog_stale_hint);
    config.error_recovery_hint = normalize_multiline_text(&config.error_recovery_hint);
    config.profile_get_description = normalize_multiline_text(&config.profile_get_description);
    config.profile_set_description = normalize_multiline_text(&config.profile_set_description);
    config.profile_add_description = normalize_multiline_text(&config.profile_add_description);
    config.profile_remove_description = normalize_multiline_text(&config.profile_remove_description);
    config.catalog_format = normalize_string_list(config.catalog_format);
    config.workflow_hints.normalize();
    config
}

fn compact_description(description: Option<&str>) -> Option<String> {
    let description = description?.trim();
    if description.is_empty() {
        return None;
    }

    let first_line = description
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .to_string();

    Some(first_line)
}

fn extract_description_from_value<T: Serialize>(value: &T) -> Option<String> {
    let json = serde_json::to_value(value).ok()?;
    let object = json.as_object()?;

    ["description", "title", "name", "uri_template", "uri"]
        .into_iter()
        .find_map(|key| object.get(key).and_then(|value| value.as_str()).map(ToOwned::to_owned))
}

fn tool_details_value(
    tool: &Tool,
    detail_level: UcanDetailLevel,
) -> serde_json::Value {
    match detail_level {
        UcanDetailLevel::Summary => {
            let schema = tool.schema_as_json_value();
            let properties = schema
                .get("properties")
                .and_then(|value| value.as_object())
                .map(|properties| properties.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            let required = schema
                .get("required")
                .and_then(|value| value.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            serde_json::json!({
                "description": tool.description.clone().map(|d| d.into_owned()),
                "input_fields": properties,
                "required_fields": required,
                "has_output_schema": tool.output_schema.is_some(),
            })
        }
        UcanDetailLevel::Full => serde_json::json!({
            "description": tool.description.clone().map(|d| d.into_owned()),
            "input_schema": tool.schema_as_json_value(),
            "output_schema": tool
                .output_schema
                .as_ref()
                .map(|schema| serde_json::Value::Object((**schema).clone())),
        }),
    }
}

fn prompt_details_value(
    prompt: &rmcp::model::Prompt,
    detail_level: UcanDetailLevel,
) -> Result<serde_json::Value> {
    match detail_level {
        UcanDetailLevel::Summary => Ok(serde_json::json!({
            "description": extract_description_from_value(prompt),
            "argument_names": prompt
                .arguments
                .as_ref()
                .map(|arguments| arguments.iter().map(|arg| arg.name.clone()).collect::<Vec<_>>())
                .unwrap_or_default(),
            "required_arguments": prompt
                .arguments
                .as_ref()
                .map(|arguments| {
                    arguments
                        .iter()
                        .filter(|arg| arg.required.unwrap_or(false))
                        .map(|arg| arg.name.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        })),
        UcanDetailLevel::Full => serde_json::to_value(prompt).context("Serialize prompt detail"),
    }
}

fn resource_details_value(
    resource: &Resource,
    detail_level: UcanDetailLevel,
) -> Result<serde_json::Value> {
    match detail_level {
        UcanDetailLevel::Summary => Ok(serde_json::json!({
            "description": extract_description_from_value(resource),
            "uri": resource.uri,
            "read_with": "resources/read",
        })),
        UcanDetailLevel::Full => {
            let mut details = serde_json::to_value(resource).context("Serialize resource detail")?;
            details["read_with"] = serde_json::Value::String("resources/read".to_string());
            Ok(details)
        }
    }
}

fn resource_template_details_value(
    resource_template: &ResourceTemplate,
    detail_level: UcanDetailLevel,
) -> Result<serde_json::Value> {
    match detail_level {
        UcanDetailLevel::Summary => Ok(serde_json::json!({
            "description": extract_description_from_value(resource_template),
            "uri_template": resource_template.uri_template,
            "read_with": "resources/read",
        })),
        UcanDetailLevel::Full => {
            let mut details = serde_json::to_value(resource_template).context("Serialize resource template detail")?;
            details["read_with"] = serde_json::Value::String("resources/read".to_string());
            details["instruction"] = serde_json::Value::String(
                "Expand RFC 6570 variables to a concrete canonical URI, then invoke resources/read.".to_string(),
            );
            Ok(details)
        }
    }
}

fn standard_read_recovery_hint() -> String {
    "Use mcpmate_ucan_catalog to select the target, inspect it with mcpmate_ucan_details, expand RFC 6570 variables for a resource template, then invoke resources/read with the concrete canonical URI.".to_string()
}

fn extract_argument_tips_from_tool(tool: &Tool) -> Vec<ArgumentTip> {
    let schema = tool.schema_as_json_value();
    let properties = match schema.get("properties").and_then(|v| v.as_object()) {
        Some(props) => props,
        None => return Vec::new(),
    };

    let required: std::collections::HashSet<&str> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|item| item.as_str()).collect())
        .unwrap_or_default();

    properties
        .iter()
        .map(|(name, prop)| {
            let type_hint = prop.get("type").and_then(|v| v.as_str()).map(ToOwned::to_owned);
            let description = prop.get("description").and_then(|v| v.as_str()).map(ToOwned::to_owned);
            ArgumentTip {
                name: name.clone(),
                required: required.contains(name.as_str()),
                type_hint,
                description,
            }
        })
        .collect()
}

fn required_arguments_from_tool(tool: &Tool) -> Vec<String> {
    let schema = tool.schema_as_json_value();
    schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn extract_argument_tips_from_prompt(prompt: &rmcp::model::Prompt) -> Vec<ArgumentTip> {
    prompt
        .arguments
        .as_ref()
        .map(|args| {
            args.iter()
                .map(|arg| ArgumentTip {
                    name: arg.name.clone(),
                    required: arg.required.unwrap_or(false),
                    type_hint: None,
                    description: arg.description.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn required_arguments_from_prompt(prompt: &rmcp::model::Prompt) -> Vec<String> {
    prompt
        .arguments
        .as_ref()
        .map(|args| {
            args.iter()
                .filter(|arg| arg.required.unwrap_or(false))
                .map(|arg| arg.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn call_requirements_for_tool(tool: &Tool) -> CallRequirements {
    let required_arguments = required_arguments_from_tool(tool);
    CallRequirements {
        accepts_arguments: true,
        call_ready_without_arguments: required_arguments.is_empty(),
        required_arguments,
    }
}

fn call_requirements_for_prompt(prompt: &rmcp::model::Prompt) -> CallRequirements {
    let required_arguments = required_arguments_from_prompt(prompt);
    CallRequirements {
        accepts_arguments: true,
        call_ready_without_arguments: required_arguments.is_empty(),
        required_arguments,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClientBuiltinContext, UcanDetailLevel, UcanError, UcanPromptRepository, VisibleListing, VisiblePromptEntry,
        VisibleResourceEntry, VisibleResourceTemplateEntry, VisibleToolEntry, capitalize_kind, compact_description,
        ensure_catalog_result_is_authoritative, extract_description_from_value, find_similar_names,
        levenshtein_distance, profile_capabilities_visible, retain_brokered_prompts,
        retain_brokered_resource_templates, retain_brokered_resources, retain_brokered_tools, tool_details_value,
    };
    use crate::clients::models::{
        CapabilitySource, UnifyDirectExposureConfig, UnifyDirectPromptSurface, UnifyDirectResourceSurface,
        UnifyDirectToolSurface, UnifyRouteMode,
    };
    use crate::common::profile::ProfileType;
    use crate::config::{
        database::Database,
        models::{Profile, Server, ServerTransportDraft},
        server::upsert_server_definition,
    };
    use crate::core::{
        capability::resource_registry::issue_resource_route,
        models::Config,
        pool::{CapSyncFlags, UpstreamConnectionPool},
    };
    use crate::mcper::builtin::registry::BuiltinService;
    use rmcp::model::{
        Annotations, CallToolRequestParams, ContentBlock, Icon, MetaObject, Prompt, Resource, ResourceTemplate, Tool,
    };
    use sqlx::sqlite::SqlitePoolOptions;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    fn test_client_context(
        config_mode: Option<&str>,
        capability_source: CapabilitySource,
    ) -> ClientBuiltinContext {
        ClientBuiltinContext {
            client_id: "client-1".to_string(),
            session_id: None,
            config_mode: config_mode.map(str::to_string),
            capability_source,
            selected_profile_ids: Vec::new(),
            custom_profile_id: None,
            unify_workspace: None,
        }
    }

    async fn resolver_fixture(
        resources: Vec<Resource>,
        templates: Vec<ResourceTemplate>,
        workspace: Option<UnifyDirectExposureConfig>,
    ) -> (super::BrokerService, ClientBuiltinContext, Arc<Database>) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect resolver database");
        crate::test_helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("initialize client table");
        crate::config::profile::init::initialize_profile_tables(&pool)
            .await
            .expect("initialize profile tables");
        crate::config::database::initialize_capability_catalog(&pool)
            .await
            .expect("initialize catalog");
        crate::core::capability::naming::initialize(pool.clone());

        let mut server = Server::new_stdio("docs".to_string(), Some("fixture-command".to_string()));
        server.id = Some("server-a".to_string());
        upsert_server_definition(
            &pool,
            &server,
            &ServerTransportDraft::Stdio {
                command: Some("fixture-command".to_string()),
                args: Vec::new(),
                env: Default::default(),
            },
        )
        .await
        .expect("insert resolver server");
        sqlx::query("UPDATE server_config SET unify_direct_exposure_eligible = 1 WHERE id = 'server-a'")
            .execute(&pool)
            .await
            .expect("enable direct exposure fixture");
        crate::config::server::capabilities::commit_protocol_items_for_kinds(
            &pool,
            "server-a",
            "docs",
            Some(serde_json::from_value(serde_json::json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"docs","version":"1.0.0"}})).expect("build resolver initialize")),
            Vec::new(),
            resources,
            Vec::new(),
            templates,
            CapSyncFlags::ALL,
        )
        .await
        .expect("commit resolver catalog");

        let database = Arc::new(Database {
            pool,
            path: PathBuf::new(),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        });
        let broker = super::BrokerService::new(
            database.clone(),
            Arc::new(Mutex::new(UpstreamConnectionPool::new(
                Arc::new(Config::default()),
                Some(database.clone()),
            ))),
        );
        let mut context = test_client_context(Some("unify"), CapabilitySource::Profiles);
        context.unify_workspace = workspace;
        (broker, context, database)
    }

    async fn call_ucan_with_context(
        broker: &super::BrokerService,
        context: &ClientBuiltinContext,
        tool_name: &'static str,
        arguments: serde_json::Value,
    ) -> rmcp::model::CallToolResult {
        let request = CallToolRequestParams::new(tool_name)
            .with_arguments(serde_json::from_value(arguments).expect("serialize UCAN call arguments"));
        broker
            .call_tool_with_context(&request, Some(context))
            .await
            .expect("call UCAN tool")
    }

    async fn details_with_context(
        broker: &super::BrokerService,
        context: &ClientBuiltinContext,
        capability_kind: &str,
        capability_name: &str,
        detail_level: &str,
    ) -> rmcp::model::CallToolResult {
        call_ucan_with_context(
            broker,
            context,
            super::MCPMATE_UCAN_DETAILS_TOOL,
            serde_json::json!({
                "capability_kind": capability_kind,
                "capability_name": capability_name,
                "detail_level": detail_level,
            }),
        )
        .await
    }

    fn text_response(result: &rmcp::model::CallToolResult) -> serde_json::Value {
        let ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected UCAN text envelope");
        };
        serde_json::from_str(&text.text).expect("parse UCAN text envelope")
    }

    fn assert_exact_details_envelope(response: &serde_json::Value) {
        let object = response.as_object().expect("details envelope");
        assert_eq!(object.len(), 11);
        assert!(
            [
                "argument_tips",
                "call_requirements",
                "capability_kind",
                "capability_name",
                "detail_level",
                "details",
                "error_recovery_hint",
                "related_capabilities",
                "server_id",
                "server_name",
                "workflow_hints",
            ]
            .into_iter()
            .all(|key| object.contains_key(key))
        );
    }

    #[test]
    fn catalog_keeps_usable_entries_when_an_upstream_listing_fails() {
        let mut templates = crate::core::capability::aggregate::AggregateListStatus::new("resource templates");
        templates.record_failure("server-a", "alpha", "offline");
        let tools = crate::core::capability::aggregate::AggregateListStatus::new("tools");

        assert!(
            ensure_catalog_result_is_authoritative(true, None, &[("tool", &tools), ("resource_template", &templates)],)
                .is_ok()
        );
    }

    #[test]
    fn catalog_rejects_empty_results_when_a_requested_listing_is_incomplete() {
        let mut templates = crate::core::capability::aggregate::AggregateListStatus::new("resource templates");
        templates.record_failure("server-a", "alpha", "offline");
        templates.record_success();

        assert!(ensure_catalog_result_is_authoritative(false, None, &[("resource_template", &templates)]).is_err());
        assert!(
            ensure_catalog_result_is_authoritative(
                false,
                Some(&["tool".to_string()]),
                &[("resource_template", &templates)],
            )
            .is_ok()
        );
    }

    #[test]
    fn authoritative_lookup_does_not_report_missing_from_partial_inventory() {
        let mut aggregate = crate::core::capability::aggregate::AggregateListStatus::new("tools");
        aggregate.record_failure("server-a", "alpha", "offline");
        aggregate.record_success();

        let missing = VisibleListing {
            entries: vec!["echo"],
            aggregate,
        }
        .find_authoritatively(|name| *name == "missing");

        assert!(missing.is_err());
    }

    #[test]
    fn profile_capabilities_are_visible_only_in_hosted_profiles_mode() {
        assert!(profile_capabilities_visible(&test_client_context(
            None,
            CapabilitySource::Profiles
        )));
        assert!(profile_capabilities_visible(&test_client_context(
            Some("hosted"),
            CapabilitySource::Profiles,
        )));
        assert!(!profile_capabilities_visible(&test_client_context(
            Some("unify"),
            CapabilitySource::Profiles,
        )));
        assert!(!profile_capabilities_visible(&test_client_context(
            Some("transparent"),
            CapabilitySource::Profiles,
        )));
        assert!(!profile_capabilities_visible(&test_client_context(
            None,
            CapabilitySource::Activated
        )));
        assert!(!profile_capabilities_visible(&test_client_context(
            None,
            CapabilitySource::Custom
        )));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolver_reads_listed_and_template_routes_from_catalog_only() {
        let (broker, context, _) = resolver_fixture(
            vec![Resource::new("file:///guide.md", "Guide")],
            vec![ResourceTemplate::new("file:///guides/{name}.md", "Guides")],
            None,
        )
        .await;

        let listed = broker
            .resolve_current_broker_resource_route(&context, "mcpmate://resources/docs/file/guide.md")
            .await
            .expect("resolve listed catalog route")
            .expect("listed route remains current");
        assert_eq!(listed.upstream_uri, "file:///guide.md");
        let template = broker
            .resolve_current_broker_resource_route(&context, "mcpmate://resources/template/docs/file/guides/rust.md")
            .await
            .expect("resolve template catalog route")
            .expect("template route remains current");
        assert_eq!(template.upstream_uri, "file:///guides/rust.md");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resource_details_emit_minimal_standard_read_metadata_and_canonical_link() {
        let resource = Resource::new("file:///guide.md", "Guide")
            .with_title("Guide title")
            .with_description("Project architecture document")
            .with_mime_type("text/markdown")
            .with_size(42)
            .with_icons(vec![Icon::new("https://example.test/guide.svg")])
            .with_meta(MetaObject(
                serde_json::json!({"origin": "fixture"})
                    .as_object()
                    .expect("fixture metadata")
                    .clone(),
            ))
            .with_annotations(Annotations::default().with_priority(0.75));
        let (broker, context, _) = resolver_fixture(vec![resource], Vec::new(), None).await;
        let canonical_uri = "mcpmate://resources/docs/file/guide.md";
        let expected_resource = serde_json::json!({
            "uri": canonical_uri,
            "name": "Guide",
            "title": "Guide title",
            "description": "Project architecture document",
            "mimeType": "text/markdown",
            "size": 42, "icons": [{"src": "https://example.test/guide.svg"}],
            "_meta": {"origin": "fixture"},
            "annotations": {"priority": 0.75},
        });

        let summary = details_with_context(&broker, &context, "resource", canonical_uri, "summary").await;
        let full = details_with_context(&broker, &context, "resource", canonical_uri, "full").await;
        let summary_json = text_response(&summary);
        let full_json = text_response(&full);
        assert_exact_details_envelope(&summary_json);
        assert_exact_details_envelope(&full_json);
        assert!(
            summary_json["workflow_hints"][0]
                .as_str()
                .is_some_and(|value| value.contains("resources/read"))
        );
        assert!(
            summary_json["error_recovery_hint"]
                .as_str()
                .is_some_and(|value| value.contains("resources/read"))
        );
        assert_eq!(
            summary_json["details"],
            serde_json::json!({
                "description": "Project architecture document",
                "uri": canonical_uri,
                "read_with": "resources/read",
            })
        );
        assert_eq!(summary_json["argument_tips"], serde_json::json!([]));
        let requirements = &summary_json["call_requirements"];
        assert_eq!(requirements["accepts_arguments"], false);
        assert_eq!(requirements["required_arguments"], serde_json::json!([]));
        assert_eq!(requirements["call_ready_without_arguments"], false);
        let mut expected_full = expected_resource.clone();
        expected_full["read_with"] = serde_json::json!("resources/read");
        assert_eq!(full_json["details"], expected_full);
        for result in [&summary, &full] {
            assert_eq!(result.content.len(), 2);
            let ContentBlock::ResourceLink(link) = &result.content[1] else {
                panic!("expected ResourceLink after details envelope");
            };
            assert_eq!(
                serde_json::to_value(link).expect("serialize ResourceLink"),
                expected_resource
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resource_template_details_use_uri_template_as_the_only_parameter_contract() {
        let template = ResourceTemplate::new("file:///guides/{name}.md", "Guides")
            .with_title("Guide template")
            .with_description("Read a guide by name")
            .with_mime_type("text/markdown")
            .with_icons(vec![Icon::new("https://example.test/guides.svg")])
            .with_meta(MetaObject(
                serde_json::json!({"origin": "fixture"})
                    .as_object()
                    .expect("fixture metadata")
                    .clone(),
            ))
            .with_annotations(Annotations::default().with_priority(0.5));
        let (broker, context, _) = resolver_fixture(Vec::new(), vec![template], None).await;
        let canonical_template = "mcpmate://resources/template/docs/file/guides/{name}.md";
        let expected_template = serde_json::json!({
            "uriTemplate": canonical_template,
            "name": "Guides",
            "title": "Guide template",
            "description": "Read a guide by name",
            "mimeType": "text/markdown",
            "icons": [{"src": "https://example.test/guides.svg"}],
            "_meta": {"origin": "fixture"},
            "annotations": {"priority": 0.5},
        });

        let summary = details_with_context(&broker, &context, "resource_template", canonical_template, "summary").await;
        let full = details_with_context(&broker, &context, "resource_template", canonical_template, "full").await;
        let summary_json = text_response(&summary);
        let full_json = text_response(&full);
        assert_eq!([summary.content.len(), full.content.len()], [1, 1]);
        assert_exact_details_envelope(&summary_json);
        assert_exact_details_envelope(&full_json);
        assert_eq!(
            summary_json["details"],
            serde_json::json!({
                "description": "Read a guide by name",
                "uri_template": canonical_template,
                "read_with": "resources/read",
            })
        );
        assert_eq!(summary_json["argument_tips"], serde_json::json!([]));
        assert_eq!(summary_json["call_requirements"]["accepts_arguments"], false);
        assert_eq!(summary_json["call_requirements"]["call_ready_without_arguments"], false);

        let mut expected_full = expected_template;
        expected_full["read_with"] = serde_json::json!("resources/read");
        expected_full["instruction"] =
            serde_json::json!("Expand RFC 6570 variables to a concrete canonical URI, then invoke resources/read.");
        assert_eq!(full_json["details"], expected_full);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn call_tool_schema_reserves_arguments_for_tool_and_prompt() {
        let arguments = resolver_fixture(Vec::new(), Vec::new(), None)
            .await
            .0
            .tools()
            .into_iter()
            .find(|tool| tool.name == super::MCPMATE_UCAN_CALL_TOOL)
            .expect("public call tool")
            .schema_as_json_value()["properties"]["arguments"]
            .clone();

        assert_eq!(
            arguments,
            serde_json::json!({
                "type": "object",
                "description": "Arguments for Tool or Prompt targets only. Resource and ResourceTemplate targets use a concrete canonical URI with resources/read.",
            })
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resource_call_rejects_before_catalog_resolution() {
        let (broker, context, _) = resolver_fixture(Vec::new(), Vec::new(), None).await;
        let resource = call_ucan_with_context(
            &broker,
            &context,
            super::MCPMATE_UCAN_CALL_TOOL,
            serde_json::json!({"capability_kind": "resource", "capability_name": "mcpmate://resources/unknown"}),
        )
        .await;
        let template = call_ucan_with_context(
            &broker,
            &context,
            super::MCPMATE_UCAN_CALL_TOOL,
            serde_json::json!({"capability_kind": "resource_template", "capability_name": "mcpmate://resources/unknown"}),
        )
        .await;
        assert!(
            text_response(&resource)["recovery_hint"]
                .as_str()
                .is_some_and(|value| value.contains("resources/read"))
        );
        for result in [&resource, &template] {
            let error = text_response(result);
            assert_eq!(result.is_error, Some(true));
            assert_eq!(error["error_code"], "resource_standard_read_required");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolver_rejects_issued_disabled_and_direct_routes() {
        let direct = UnifyDirectExposureConfig {
            route_mode: UnifyRouteMode::CapabilityLevel,
            selected_resource_surfaces: vec![UnifyDirectResourceSurface {
                server_id: "server-a".to_string(),
                resource_uri: "file:///guide.md".to_string(),
            }],
            ..Default::default()
        };
        let (broker, context, database) = resolver_fixture(
            vec![Resource::new("file:///guide.md", "Guide")],
            Vec::new(),
            Some(direct),
        )
        .await;
        assert!(
            broker
                .resolve_current_broker_resource_route(&context, "mcpmate://resources/docs/file/guide.md")
                .await
                .expect("direct route resolves")
                .is_none()
        );

        let mut broker_only = context.clone();
        broker_only.unify_workspace.as_mut().expect("workspace").route_mode = UnifyRouteMode::BrokerOnly;
        assert!(
            broker
                .resolve_current_broker_resource_route(&broker_only, "mcpmate://resources/docs/file/guide.md")
                .await
                .expect("broker-only route resolves")
                .is_some()
        );

        let issued = issue_resource_route(&database.pool, "server-a", "docs", "file:///issued.md")
            .await
            .expect("issue resource route");
        assert!(
            broker
                .resolve_current_broker_resource_route(&broker_only, &issued)
                .await
                .expect("issued route resolves")
                .is_none()
        );

        database.capability_cache.invalidate_server("server-a").await;
        sqlx::query("DELETE FROM capability_server_snapshots WHERE server_id = 'server-a'")
            .execute(&database.pool)
            .await
            .expect("remove resolver catalog");
        let missing_catalog = broker
            .resolve_current_broker_resource_route(&broker_only, "mcpmate://resources/docs/file/guide.md")
            .await
            .expect_err("missing catalog is non-authoritative");
        assert!(super::is_catalog_authority_error(&missing_catalog));

        sqlx::query("UPDATE server_config SET enabled = 0 WHERE id = 'server-a'")
            .execute(&database.pool)
            .await
            .expect("disable resolver server");
        assert!(
            broker
                .resolve_current_broker_resource_route(&broker_only, "mcpmate://resources/docs/file/guide.md")
                .await
                .expect("disabled route resolves")
                .is_none()
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolver_returns_none_for_stale_listed_registry_route() {
        let (broker, context, database) =
            resolver_fixture(vec![Resource::new("file:///guide.md", "Guide")], Vec::new(), None).await;
        sqlx::query("UPDATE server_resources SET unique_uri = 'mcpmate://resources/docs/file/stale.md' WHERE server_id = 'server-a'")
            .execute(&database.pool)
            .await
            .expect("make listed registry route stale");

        assert!(
            broker
                .resolve_current_broker_resource_route(&context, "mcpmate://resources/docs/file/stale.md")
                .await
                .expect("stale listed route resolves")
                .is_none()
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolver_returns_none_for_invisible_resource_and_template() {
        let (broker, mut context, database) = resolver_fixture(
            vec![
                Resource::new("file:///allowed.md", "Allowed"),
                Resource::new("file:///hidden.md", "Hidden"),
            ],
            vec![
                ResourceTemplate::new("file:///allowed/{name}.md", "Allowed"),
                ResourceTemplate::new("file:///hidden/{name}.md", "Hidden"),
            ],
            None,
        )
        .await;
        let profile_id = crate::test_helpers::insert_profile(
            &database.pool,
            &Profile::new("filtered".to_string(), ProfileType::Scenario),
        )
        .await;
        for (kind, origin) in [
            ("resources", "file:///allowed.md"),
            ("resource_templates", "file:///allowed/{name}.md"),
        ] {
            sqlx::query(
                "INSERT INTO profile_capability_refs (profile_id, ref_id, enabled) SELECT ?, ref_id, 1 FROM capability_refs WHERE server_id = 'server-a' AND kind = ? AND origin_key = ?",
            )
            .bind(&profile_id)
            .bind(kind)
            .bind(origin)
            .execute(&database.pool)
            .await
            .expect("allow selected capability");
        }
        sqlx::query(
            "INSERT INTO client (id, name, identifier, capability_source, selected_profile_ids) VALUES ('client-filtered', 'client-1', 'client-1', 'profiles', ?)",
        )
        .bind(serde_json::to_string(&vec![profile_id]).expect("serialize selected profile"))
        .execute(&database.pool)
        .await
        .expect("configure filtered resolver client");
        context.config_mode = None;

        for external_uri in [
            "mcpmate://resources/docs/file/hidden.md",
            "mcpmate://resources/template/docs/file/hidden/rust.md",
        ] {
            assert!(
                broker
                    .resolve_current_broker_resource_route(&context, external_uri)
                    .await
                    .expect("invisible capability route resolves")
                    .is_none()
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolver_keeps_invalid_and_ambiguous_routes_as_errors() {
        let (broker, context, database) = resolver_fixture(Vec::new(), Vec::new(), None).await;
        for external_uri in ["not a uri", "mcpmate://resources/docs/file/missing.md"] {
            assert!(
                broker
                    .resolve_current_broker_resource_route(&context, external_uri)
                    .await
                    .is_err()
            );
        }
        for (id, template, unique_name) in [
            (
                "template-a",
                "file:///guides/{name}.md",
                "mcpmate://resources/template/docs/file/guides/{name}.md",
            ),
            (
                "template-b",
                "file:///guides/{slug}.md",
                "mcpmate://resources/template/docs/file/guides/{slug}.md",
            ),
        ] {
            sqlx::query(
                "INSERT INTO server_resource_templates (id, server_id, server_name, uri_template, unique_name, name) VALUES (?, 'server-a', 'docs', ?, ?, ?)",
            )
            .bind(id)
            .bind(template)
            .bind(unique_name)
            .bind(id)
            .execute(&database.pool)
            .await
            .expect("insert ambiguous resolver template");
        }
        let error = broker
            .resolve_current_broker_resource_route(&context, "mcpmate://resources/template/docs/file/guides/rust.md")
            .await
            .expect_err("overlapping templates are ambiguous");
        assert!(error.chain().any(|cause| cause.to_string().contains("ambiguous")));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolver_wraps_real_catalog_read_errors_with_their_source() {
        let (broker, context, database) =
            resolver_fixture(vec![Resource::new("file:///guide.md", "Guide")], Vec::new(), None).await;
        sqlx::query("UPDATE capability_server_snapshots SET record_format_version = 99 WHERE server_id = 'server-a'")
            .execute(&database.pool)
            .await
            .expect("corrupt resolver catalog format");

        let error = broker
            .resolve_current_broker_resource_route(&context, "mcpmate://resources/docs/file/guide.md")
            .await
            .expect_err("catalog read failure is not a missing route");
        assert!(super::is_catalog_authority_error(&error));
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().contains("unsupported record format version 99"))
        );
    }

    #[test]
    fn catalog_read_failure_remains_catalog_authority_error() {
        let route =
            super::catalog_authority_error_for_route("server-a", "docs", anyhow::anyhow!("catalog query failed"));
        assert!(super::is_catalog_authority_error(&route));
        assert!(route.chain().any(|cause| cause.to_string() == "catalog query failed"));
    }

    #[test]
    fn compact_description_keeps_first_non_empty_line() {
        let value = compact_description(Some("\n\nFirst line.\nSecond line."));
        assert_eq!(value.as_deref(), Some("First line."));
    }

    #[test]
    fn compact_description_keeps_long_lines() {
        let source = format!("{} extra", "a".repeat(200));
        let value = compact_description(Some(&source)).expect("summary");
        assert_eq!(value, source);
    }

    #[test]
    fn extract_description_from_value_prefers_description_field() {
        let value = serde_json::json!({"description": "hello", "name": "fallback"});
        assert_eq!(extract_description_from_value(&value).as_deref(), Some("hello"));
    }

    #[test]
    fn tool_details_summary_is_lighter_than_full() {
        let tool = Tool::new(
            "demo",
            "Demo tool",
            Arc::new(
                serde_json::json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}, "force": {"type": "boolean"}},
                    "required": ["path"]
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        );
        let summary = tool_details_value(&tool, UcanDetailLevel::Summary);
        let full = tool_details_value(&tool, UcanDetailLevel::Full);
        assert!(summary.get("input_fields").is_some());
        assert!(summary.get("input_schema").is_none());
        assert!(full.get("input_schema").is_some());
    }

    #[test]
    fn levenshtein_distance_basic_cases() {
        assert_eq!(levenshtein_distance("", ""), 0);
        assert_eq!(levenshtein_distance("a", ""), 1);
        assert_eq!(levenshtein_distance("", "a"), 1);
        assert_eq!(levenshtein_distance("abc", "abc"), 0);
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
        assert_eq!(levenshtein_distance("saturday", "sunday"), 3);
    }

    #[test]
    fn levenshtein_distance_is_case_sensitive() {
        assert_eq!(levenshtein_distance("Tool", "tool"), 1);
        assert_eq!(levenshtein_distance("TOOL", "tool"), 4);
    }

    #[test]
    fn find_similar_names_is_case_insensitive() {
        let candidates = vec!["Tool".to_string(), "TOOL".to_string()];
        let result = find_similar_names("tool", &candidates, 2);
        assert!(!result.is_empty());
    }

    #[test]
    fn find_similar_names_returns_closest_matches() {
        let candidates = vec![
            super::MCPMATE_UCAN_CATALOG_TOOL.to_string(),
            super::MCPMATE_UCAN_DETAILS_TOOL.to_string(),
            super::MCPMATE_UCAN_CALL_TOOL.to_string(),
            "other_tool".to_string(),
        ];
        let result = find_similar_names("mcpmate_ucan_catlog", &candidates, 3);
        assert_eq!(result.len(), 3);
        assert!(result.contains(&super::MCPMATE_UCAN_CATALOG_TOOL.to_string()));
    }

    #[test]
    fn find_similar_names_respects_limit() {
        let candidates = vec![
            "tool_a".to_string(),
            "tool_b".to_string(),
            "tool_c".to_string(),
            "tool_d".to_string(),
            "tool_e".to_string(),
        ];
        let result = find_similar_names("tool_x", &candidates, 2);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn find_similar_names_handles_empty_candidates() {
        let candidates: Vec<String> = vec![];
        let result = find_similar_names("any_tool", &candidates, 3);
        assert!(result.is_empty());
    }

    #[test]
    fn test_error_capability_not_found_includes_alternatives() {
        let surface_names = vec![
            super::MCPMATE_UCAN_CATALOG_TOOL.to_string(),
            super::MCPMATE_UCAN_DETAILS_TOOL.to_string(),
            super::MCPMATE_UCAN_CALL_TOOL.to_string(),
        ];
        let error = UcanError::capability_not_found("tool", "mcpmate_ucan_catlog", &surface_names);

        assert_eq!(error.error_code, "capability_not_found");
        assert!(error.message.contains("Tool"));
        assert!(error.message.contains("mcpmate_ucan_catlog"));
        assert!(!error.alternatives.is_empty());
        assert!(
            error
                .alternatives
                .contains(&super::MCPMATE_UCAN_CATALOG_TOOL.to_string())
        );
        assert!(!error.retry_eligible);
        assert!(error.recovery_hint.contains("alternatives"));
    }

    #[test]
    fn test_error_capability_not_found_empty_catalog() {
        let surface_names: Vec<String> = vec![];
        let error = UcanError::capability_not_found("prompt", "unknown_prompt", &surface_names);

        assert_eq!(error.error_code, "capability_not_found");
        assert!(error.alternatives.is_empty());
        assert!(error.recovery_hint.contains(super::MCPMATE_UCAN_CATALOG_TOOL));
    }

    #[test]
    fn test_error_server_unreachable_includes_retry_hint() {
        let error = UcanError::server_unreachable("server-123", "my-server");

        assert_eq!(error.error_code, "server_unreachable");
        assert!(error.message.contains("my-server"));
        assert!(error.message.contains("server-123"));
        assert!(error.retry_eligible);
        assert!(error.recovery_hint.contains("running"));
        assert!(error.alternatives.is_empty());
    }

    #[test]
    fn test_error_visibility_denied() {
        let error = UcanError::visibility_denied("tool", "sensitive_tool");

        assert_eq!(error.error_code, "visibility_denied");
        assert!(error.message.contains("not available"));
        assert!(!error.retry_eligible);
        assert!(error.recovery_hint.contains(super::MCPMATE_UCAN_CATALOG_TOOL));
    }

    #[test]
    fn test_error_missing_required_arguments() {
        let missing = vec!["path".to_string(), "mode".to_string()];
        let error = UcanError::missing_required_arguments("tool", "read_file", &missing);

        assert_eq!(error.error_code, "missing_required_arguments");
        assert!(error.message.contains("read_file"));
        assert!(error.message.contains("path"));
        assert!(error.retry_eligible);
        assert!(error.recovery_hint.contains("detail_level=full"));
    }

    #[test]
    fn test_error_upstream_error() {
        let error = UcanError::upstream_error("tool", "my_tool");

        assert_eq!(error.error_code, "upstream_error");
        assert!(error.message.contains("my_tool"));
        assert!(!error.message.contains("connection refused"));
        assert!(!error.retry_eligible);
        assert!(error.recovery_hint.contains("upstream"));
    }

    #[test]
    fn test_error_timeout() {
        let error = UcanError::timeout("tool", "slow_tool", 60);

        assert_eq!(error.error_code, "timeout");
        assert!(error.message.contains("60 seconds"));
        assert!(error.retry_eligible);
        assert!(error.recovery_hint.contains("MCPMATE_TOOL_CALL_TIMEOUT_SECS"));
    }

    #[test]
    fn test_error_context_required() {
        let error = UcanError::context_required(super::MCPMATE_UCAN_CATALOG_TOOL);

        assert_eq!(error.error_code, "context_required");
        assert!(error.message.contains(super::MCPMATE_UCAN_CATALOG_TOOL));
        assert!(!error.retry_eligible);
    }

    #[test]
    fn test_error_unknown_tool() {
        let error = UcanError::unknown_tool("invalid_tool");

        assert_eq!(error.error_code, "unknown_tool");
        assert!(error.message.contains("invalid_tool"));
        assert!(!error.retry_eligible);
        assert!(
            error
                .alternatives
                .contains(&super::MCPMATE_UCAN_CATALOG_TOOL.to_string())
        );
        assert!(
            error
                .alternatives
                .contains(&super::MCPMATE_UCAN_DETAILS_TOOL.to_string())
        );
        assert!(error.alternatives.contains(&super::MCPMATE_UCAN_CALL_TOOL.to_string()));
    }

    #[test]
    fn test_error_invalid_parameters() {
        let error = UcanError::invalid_parameters(super::MCPMATE_UCAN_CALL_TOOL, "missing capability_kind");

        assert_eq!(error.error_code, "invalid_parameters");
        assert!(error.message.contains("missing capability_kind"));
        assert!(!error.retry_eligible);
    }

    #[test]
    fn test_error_response_is_valid_json() {
        let error = UcanError::capability_not_found("tool", "test_tool", &["alt1".to_string(), "alt2".to_string()]);
        let json = error.to_json();

        let parsed: serde_json::Value = serde_json::from_str(&json).expect("JSON should be valid");
        assert_eq!(parsed["error_code"], "capability_not_found");
        assert!(parsed["message"].is_string());
        assert!(parsed["recovery_hint"].is_string());
        assert!(parsed["alternatives"].is_array());
        assert!(parsed["retry_eligible"].is_boolean());
    }

    #[test]
    fn test_error_to_call_tool_result() {
        let error = UcanError::server_unreachable("server-1", "test-server");
        let result = error.to_call_tool_result();

        assert_eq!(result.is_error, Some(true));
        assert!(!result.content.is_empty());
    }

    #[test]
    fn capitalize_kind_returns_correct_strings() {
        assert_eq!(capitalize_kind("tool"), "Tool");
        assert_eq!(capitalize_kind("prompt"), "Prompt");
        assert_eq!(capitalize_kind("resource"), "Resource");
        assert_eq!(capitalize_kind("resource_template"), "Resource template");
        assert_eq!(capitalize_kind("unknown"), "Capability");
    }

    #[test]
    fn unify_direct_exposure_catalog_exclusion_keeps_broker_only_tools() {
        let context = ClientBuiltinContext {
            client_id: "client-1".to_string(),
            session_id: Some("session-1".to_string()),
            config_mode: Some("unify".to_string()),
            capability_source: CapabilitySource::Profiles,
            selected_profile_ids: Vec::new(),
            custom_profile_id: None,
            unify_workspace: Some(UnifyDirectExposureConfig {
                route_mode: UnifyRouteMode::BrokerOnly,
                selected_server_ids: vec!["server-a".to_string()],
                selected_tool_surfaces: vec![UnifyDirectToolSurface {
                    server_id: "server-a".to_string(),
                    tool_name: "tool-one".to_string(),
                }],
                selected_prompt_surfaces: Vec::new(),
                selected_resource_surfaces: Vec::new(),
                selected_template_surfaces: Vec::new(),
            }),
        };
        let mut visible = vec![VisibleToolEntry {
            server_id: "server-a".to_string(),
            server_name: "Server A".to_string(),
            raw_tool_name: "tool-one".to_string(),
            tool: Tool::new("server-a__tool-one", "demo", Arc::new(serde_json::Map::new())),
        }];

        retain_brokered_tools(&context, &HashSet::from(["server-a".to_string()]), &mut visible);

        assert_eq!(visible.len(), 1);
    }

    #[test]
    fn unify_direct_exposure_catalog_exclusion_removes_server_level_tools() {
        let context = ClientBuiltinContext {
            client_id: "client-1".to_string(),
            session_id: Some("session-1".to_string()),
            config_mode: Some("unify".to_string()),
            capability_source: CapabilitySource::Profiles,
            selected_profile_ids: Vec::new(),
            custom_profile_id: None,
            unify_workspace: Some(UnifyDirectExposureConfig {
                route_mode: UnifyRouteMode::ServerLevel,
                selected_server_ids: Vec::new(),
                selected_tool_surfaces: vec![UnifyDirectToolSurface {
                    server_id: "server-a".to_string(),
                    tool_name: "tool-one".to_string(),
                }],
                selected_prompt_surfaces: Vec::new(),
                selected_resource_surfaces: Vec::new(),
                selected_template_surfaces: Vec::new(),
            }),
        };
        let mut visible = vec![
            VisibleToolEntry {
                server_id: "server-a".to_string(),
                server_name: "Server A".to_string(),
                raw_tool_name: "tool-one".to_string(),
                tool: Tool::new("server-a__tool-one", "demo", Arc::new(serde_json::Map::new())),
            },
            VisibleToolEntry {
                server_id: "server-b".to_string(),
                server_name: "Server B".to_string(),
                raw_tool_name: "tool-two".to_string(),
                tool: Tool::new("server-b__tool-two", "demo", Arc::new(serde_json::Map::new())),
            },
        ];

        retain_brokered_tools(&context, &HashSet::from(["server-a".to_string()]), &mut visible);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].tool.name.as_ref(), "server-b__tool-two");
    }

    #[test]
    fn unify_direct_exposure_catalog_exclusion_removes_only_selected_capability_level_tools() {
        let context = ClientBuiltinContext {
            client_id: "client-1".to_string(),
            session_id: Some("session-1".to_string()),
            config_mode: Some("unify".to_string()),
            capability_source: CapabilitySource::Profiles,
            selected_profile_ids: Vec::new(),
            custom_profile_id: None,
            unify_workspace: Some(UnifyDirectExposureConfig {
                route_mode: UnifyRouteMode::CapabilityLevel,
                selected_server_ids: vec!["server-a".to_string()],
                selected_tool_surfaces: vec![UnifyDirectToolSurface {
                    server_id: "server-a".to_string(),
                    tool_name: "tool-one".to_string(),
                }],
                selected_prompt_surfaces: Vec::new(),
                selected_resource_surfaces: Vec::new(),
                selected_template_surfaces: Vec::new(),
            }),
        };
        let mut visible = vec![
            VisibleToolEntry {
                server_id: "server-a".to_string(),
                server_name: "Server A".to_string(),
                raw_tool_name: "tool-one".to_string(),
                tool: Tool::new("server-a__tool-one", "demo", Arc::new(serde_json::Map::new())),
            },
            VisibleToolEntry {
                server_id: "server-a".to_string(),
                server_name: "Server A".to_string(),
                raw_tool_name: "tool-two".to_string(),
                tool: Tool::new("server-a__tool-two", "demo", Arc::new(serde_json::Map::new())),
            },
        ];

        retain_brokered_tools(&context, &HashSet::from(["server-a".to_string()]), &mut visible);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].tool.name.as_ref(), "server-a__tool-two");
    }

    #[test]
    fn unify_direct_exposure_catalog_exclusion_removes_server_level_prompts() {
        let context = ClientBuiltinContext {
            client_id: "client-1".to_string(),
            session_id: Some("session-1".to_string()),
            config_mode: Some("unify".to_string()),
            capability_source: CapabilitySource::Profiles,
            selected_profile_ids: Vec::new(),
            custom_profile_id: None,
            unify_workspace: Some(UnifyDirectExposureConfig {
                route_mode: UnifyRouteMode::ServerLevel,
                selected_server_ids: Vec::new(),
                selected_tool_surfaces: Vec::new(),
                selected_prompt_surfaces: vec![UnifyDirectPromptSurface {
                    server_id: "server-a".to_string(),
                    prompt_name: "prompt-one".to_string(),
                }],
                selected_resource_surfaces: Vec::new(),
                selected_template_surfaces: Vec::new(),
            }),
        };
        let mut visible = vec![
            VisiblePromptEntry {
                server_id: "server-a".to_string(),
                server_name: "Server A".to_string(),
                raw_prompt_name: "prompt-one".to_string(),
                prompt: Prompt::new(
                    "server-a__prompt-one",
                    Some("demo".to_string()),
                    None::<Vec<rmcp::model::PromptArgument>>,
                ),
            },
            VisiblePromptEntry {
                server_id: "server-b".to_string(),
                server_name: "Server B".to_string(),
                raw_prompt_name: "prompt-two".to_string(),
                prompt: Prompt::new(
                    "server-b__prompt-two",
                    Some("demo".to_string()),
                    None::<Vec<rmcp::model::PromptArgument>>,
                ),
            },
        ];

        retain_brokered_prompts(&context, &HashSet::from(["server-a".to_string()]), &mut visible);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].prompt.name.as_str(), "server-b__prompt-two");
    }

    #[test]
    fn unify_direct_exposure_catalog_exclusion_removes_only_selected_capability_level_prompts() {
        let context = ClientBuiltinContext {
            client_id: "client-1".to_string(),
            session_id: Some("session-1".to_string()),
            config_mode: Some("unify".to_string()),
            capability_source: CapabilitySource::Profiles,
            selected_profile_ids: Vec::new(),
            custom_profile_id: None,
            unify_workspace: Some(UnifyDirectExposureConfig {
                route_mode: UnifyRouteMode::CapabilityLevel,
                selected_server_ids: vec!["server-a".to_string()],
                selected_tool_surfaces: Vec::new(),
                selected_prompt_surfaces: vec![UnifyDirectPromptSurface {
                    server_id: "server-a".to_string(),
                    prompt_name: "prompt-one".to_string(),
                }],
                selected_resource_surfaces: Vec::new(),
                selected_template_surfaces: Vec::new(),
            }),
        };
        let mut visible = vec![
            VisiblePromptEntry {
                server_id: "server-a".to_string(),
                server_name: "Server A".to_string(),
                raw_prompt_name: "prompt-one".to_string(),
                prompt: Prompt::new(
                    "server-a__prompt-one",
                    Some("demo".to_string()),
                    None::<Vec<rmcp::model::PromptArgument>>,
                ),
            },
            VisiblePromptEntry {
                server_id: "server-a".to_string(),
                server_name: "Server A".to_string(),
                raw_prompt_name: "prompt-two".to_string(),
                prompt: Prompt::new(
                    "server-a__prompt-two",
                    Some("demo".to_string()),
                    None::<Vec<rmcp::model::PromptArgument>>,
                ),
            },
        ];

        retain_brokered_prompts(&context, &HashSet::from(["server-a".to_string()]), &mut visible);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].raw_prompt_name, "prompt-two");
    }

    #[test]
    fn unify_direct_exposure_catalog_exclusion_removes_server_level_resources() {
        let context = ClientBuiltinContext {
            client_id: "client-1".to_string(),
            session_id: Some("session-1".to_string()),
            config_mode: Some("unify".to_string()),
            capability_source: CapabilitySource::Profiles,
            selected_profile_ids: Vec::new(),
            custom_profile_id: None,
            unify_workspace: Some(UnifyDirectExposureConfig {
                route_mode: UnifyRouteMode::ServerLevel,
                selected_server_ids: Vec::new(),
                selected_tool_surfaces: Vec::new(),
                selected_prompt_surfaces: Vec::new(),
                selected_resource_surfaces: vec![UnifyDirectResourceSurface {
                    server_id: "server-a".to_string(),
                    resource_uri: "resource-one".to_string(),
                }],
                selected_template_surfaces: Vec::new(),
            }),
        };
        let mut visible = vec![
            VisibleResourceEntry {
                server_id: "server-a".to_string(),
                server_name: "Server A".to_string(),
                raw_resource_uri: "resource-one".to_string(),
                resource: Resource::new("server-a://resource-one", "server-a://resource-one"),
            },
            VisibleResourceEntry {
                server_id: "server-b".to_string(),
                server_name: "Server B".to_string(),
                raw_resource_uri: "resource-two".to_string(),
                resource: Resource::new("server-b://resource-two", "server-b://resource-two"),
            },
        ];

        retain_brokered_resources(&context, &HashSet::from(["server-a".to_string()]), &mut visible);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].resource.uri.as_str(), "server-b://resource-two");
    }

    #[test]
    fn unify_direct_exposure_catalog_exclusion_removes_only_selected_capability_level_resources() {
        let context = ClientBuiltinContext {
            client_id: "client-1".to_string(),
            session_id: Some("session-1".to_string()),
            config_mode: Some("unify".to_string()),
            capability_source: CapabilitySource::Profiles,
            selected_profile_ids: Vec::new(),
            custom_profile_id: None,
            unify_workspace: Some(UnifyDirectExposureConfig {
                route_mode: UnifyRouteMode::CapabilityLevel,
                selected_server_ids: vec!["server-a".to_string()],
                selected_tool_surfaces: Vec::new(),
                selected_prompt_surfaces: Vec::new(),
                selected_resource_surfaces: vec![UnifyDirectResourceSurface {
                    server_id: "server-a".to_string(),
                    resource_uri: "resource-one".to_string(),
                }],
                selected_template_surfaces: Vec::new(),
            }),
        };
        let mut visible = vec![
            VisibleResourceEntry {
                server_id: "server-a".to_string(),
                server_name: "Server A".to_string(),
                raw_resource_uri: "resource-one".to_string(),
                resource: Resource::new("server-a://resource-one", "server-a://resource-one"),
            },
            VisibleResourceEntry {
                server_id: "server-a".to_string(),
                server_name: "Server A".to_string(),
                raw_resource_uri: "resource-two".to_string(),
                resource: Resource::new("server-a://resource-two", "server-a://resource-two"),
            },
        ];

        retain_brokered_resources(&context, &HashSet::from(["server-a".to_string()]), &mut visible);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].raw_resource_uri, "resource-two");
    }

    #[test]
    fn unify_direct_exposure_catalog_exclusion_removes_server_level_resource_templates() {
        let context = ClientBuiltinContext {
            client_id: "client-1".to_string(),
            session_id: Some("session-1".to_string()),
            config_mode: Some("unify".to_string()),
            capability_source: CapabilitySource::Profiles,
            selected_profile_ids: Vec::new(),
            custom_profile_id: None,
            unify_workspace: Some(UnifyDirectExposureConfig {
                route_mode: UnifyRouteMode::ServerLevel,
                selected_server_ids: Vec::new(),
                selected_tool_surfaces: Vec::new(),
                selected_prompt_surfaces: Vec::new(),
                selected_resource_surfaces: Vec::new(),
                selected_template_surfaces: vec![crate::clients::models::UnifyDirectTemplateSurface {
                    server_id: "server-a".to_string(),
                    uri_template: "server-a://{id}".to_string(),
                }],
            }),
        };
        let mut visible = vec![
            VisibleResourceTemplateEntry {
                server_id: "server-a".to_string(),
                server_name: "Server A".to_string(),
                raw_uri_template: "server-a://{id}".to_string(),
                resource_template: ResourceTemplate::new("server-a://{id}", "server-a://{id}"),
            },
            VisibleResourceTemplateEntry {
                server_id: "server-b".to_string(),
                server_name: "Server B".to_string(),
                raw_uri_template: "server-b://{id}".to_string(),
                resource_template: ResourceTemplate::new("server-b://{id}", "server-b://{id}"),
            },
        ];

        retain_brokered_resource_templates(&context, &HashSet::from(["server-a".to_string()]), &mut visible);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].resource_template.uri_template.as_str(), "server-b://{id}");
    }

    #[test]
    fn unify_direct_exposure_catalog_exclusion_removes_only_selected_capability_level_resource_templates() {
        let context = ClientBuiltinContext {
            client_id: "client-1".to_string(),
            session_id: Some("session-1".to_string()),
            config_mode: Some("unify".to_string()),
            capability_source: CapabilitySource::Profiles,
            selected_profile_ids: Vec::new(),
            custom_profile_id: None,
            unify_workspace: Some(UnifyDirectExposureConfig {
                route_mode: UnifyRouteMode::CapabilityLevel,
                selected_server_ids: vec!["server-a".to_string()],
                selected_tool_surfaces: Vec::new(),
                selected_prompt_surfaces: Vec::new(),
                selected_resource_surfaces: Vec::new(),
                selected_template_surfaces: vec![crate::clients::models::UnifyDirectTemplateSurface {
                    server_id: "server-a".to_string(),
                    uri_template: "server-a://{id}".to_string(),
                }],
            }),
        };
        let mut visible = vec![
            VisibleResourceTemplateEntry {
                server_id: "server-a".to_string(),
                server_name: "Server A".to_string(),
                raw_uri_template: "server-a://{id}".to_string(),
                resource_template: ResourceTemplate::new("server-a://{id}", "server-a://{id}"),
            },
            VisibleResourceTemplateEntry {
                server_id: "server-a".to_string(),
                server_name: "Server A".to_string(),
                raw_uri_template: "server-a://{name}".to_string(),
                resource_template: ResourceTemplate::new("server-a://{name}", "server-a://{name}"),
            },
        ];

        retain_brokered_resource_templates(&context, &HashSet::from(["server-a".to_string()]), &mut visible);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].raw_uri_template, "server-a://{name}");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn ucan_prompt_repository_hot_reloads_from_disk() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("ucan.json5");
        std::fs::write(
            &config_path,
            r#"{
                catalog_tool_description: "first",
                details_tool_description: "details",
                call_tool_description: "call",
                catalog_usage: "usage",
                catalog_format: [
                    "capability_name",
                    "capability_kind",
                    "summary",
                    "action",
                    "next_step",
                    "server_id",
                    "server_name",
                    "interaction_mode",
                ],
                catalog_page_size_default: 20,
                catalog_page_size_max: 50,
            }"#,
        )
        .expect("write config");

        unsafe { std::env::set_var("MCPMATE_UCAN_CONFIG", &config_path) };

        let repo = UcanPromptRepository::new();
        let first = repo.get().await;
        assert_eq!(first.catalog_tool_description, "first");

        std::fs::write(
            &config_path,
            r#"{
                catalog_tool_description: "second",
                details_tool_description: "details",
                call_tool_description: "call",
                catalog_usage: "usage",
                catalog_format: [
                    "capability_name",
                    "capability_kind",
                    "summary",
                    "action",
                    "next_step",
                    "server_id",
                    "server_name",
                    "interaction_mode",
                ],
                catalog_page_size_default: 20,
                catalog_page_size_max: 50,
            }"#,
        )
        .expect("rewrite config");

        tokio::time::sleep(Duration::from_secs(3)).await;
        let second = repo.get().await;
        assert_eq!(second.catalog_tool_description, "second");

        unsafe { std::env::remove_var("MCPMATE_UCAN_CONFIG") };
    }

    #[test]
    fn normalize_multiline_text_cleans_line_endings_and_edges() {
        let input = "\r\n  line one\r\nline two  \r\n\r\n";
        let normalized = super::normalize_multiline_text(input);
        assert_eq!(normalized, "line one\nline two");
        assert!(!normalized.contains('\r'));
    }

    #[test]
    fn load_ucan_prompt_config_requires_object_root() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("ucan.json5");
        std::fs::write(&config_path, "[1, 2, 3]").expect("write invalid root");

        unsafe { std::env::set_var("MCPMATE_UCAN_CONFIG", &config_path) };
        let result = super::load_ucan_prompt_config_blocking();
        assert!(result.is_err());
        let message = format!("{}", result.expect_err("load error"));
        assert!(message.contains("must be a JSON5 object"));
        unsafe { std::env::remove_var("MCPMATE_UCAN_CONFIG") };
    }

    #[test]
    fn bundled_ucan_json5_is_well_formed_object() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe { std::env::remove_var("MCPMATE_UCAN_CONFIG") };
        let bundled = super::parse_ucan_prompt_config(
            super::BUNDLED_UCAN_PROMPT_CONFIG,
            "bundled ucan.json5",
        )
        .expect("parse bundled prompt config");
        let loaded = super::load_ucan_prompt_config_blocking().expect("load bundled prompt config");
        let fallback = super::default_ucan_prompt_config();
        assert_eq!(bundled.catalog_usage, fallback.catalog_usage);
        assert_eq!(bundled.error_recovery_hint, fallback.error_recovery_hint);
        assert_eq!(loaded.catalog_usage, bundled.catalog_usage);
        assert_eq!(loaded.error_recovery_hint, bundled.error_recovery_hint);
    }

    #[test]
    fn test_catalog_sort_tools_before_prompts() {
        let weights = super::KindWeights {
            tool: 0,
            prompt: 1,
            resource: 2,
            resource_template: 3,
            profile: 0,
        };

        let tool_weight = super::SurfaceKind::Tool.weight(&weights);
        let prompt_weight = super::SurfaceKind::Prompt.weight(&weights);
        let resource_weight = super::SurfaceKind::Resource.weight(&weights);
        let template_weight = super::SurfaceKind::ResourceTemplate.weight(&weights);

        assert!(
            tool_weight < prompt_weight,
            "tools should have lower weight than prompts"
        );
        assert!(
            prompt_weight < resource_weight,
            "prompts should have lower weight than resources"
        );
        assert!(
            resource_weight < template_weight,
            "resources should have lower weight than templates"
        );
    }

    #[test]
    fn test_catalog_sort_healthy_servers_first() {
        use super::{HealthWeights, PoolSnapshot, health_weight_for_server};
        use crate::core::foundation::types::ConnectionStatus;

        let weights = HealthWeights {
            ready: 0,
            reconnecting: 1,
            other: 2,
        };

        let mut snapshot: PoolSnapshot = std::collections::HashMap::new();
        snapshot.insert(
            "ready-server".to_string(),
            vec![("inst-1".to_string(), ConnectionStatus::Ready, false, false, None)],
        );
        snapshot.insert(
            "reconnecting-server".to_string(),
            vec![("inst-2".to_string(), ConnectionStatus::Initializing, false, false, None)],
        );
        snapshot.insert(
            "error-server".to_string(),
            vec![(
                "inst-3".to_string(),
                ConnectionStatus::Error(crate::core::foundation::types::ErrorDetails {
                    message: "error".to_string(),
                    error_type: crate::core::foundation::types::ErrorType::Temporary,
                    failure_count: 1,
                    first_failure_time: 0,
                    last_failure_time: 0,
                }),
                false,
                false,
                None,
            )],
        );

        let ready_weight = health_weight_for_server("ready-server", &snapshot, &weights);
        let reconnecting_weight = health_weight_for_server("reconnecting-server", &snapshot, &weights);
        let error_weight = health_weight_for_server("error-server", &snapshot, &weights);
        let unknown_weight = health_weight_for_server("unknown-server", &snapshot, &weights);

        assert_eq!(ready_weight, 0, "ready server should have weight 0");
        assert_eq!(reconnecting_weight, 1, "reconnecting server should have weight 1");
        assert_eq!(error_weight, 2, "error server should have weight 2");
        assert_eq!(unknown_weight, 2, "unknown server should have weight 2 (other)");
    }

    #[test]
    fn test_catalog_sort_stable_with_equal_scores() {
        let weights = super::KindWeights {
            tool: 0,
            prompt: 1,
            resource: 2,
            resource_template: 3,
            profile: 0,
        };

        let tool_a = super::SurfaceKind::Tool.weight(&weights);
        let tool_b = super::SurfaceKind::Tool.weight(&weights);

        assert_eq!(tool_a, tool_b, "same capability kinds should have equal weights");
    }

    #[test]
    fn test_default_catalog_sort_weights() {
        let weights = super::CatalogSortWeights::default();

        assert_eq!(weights.kind.tool, 0);
        assert_eq!(weights.kind.prompt, 1);
        assert_eq!(weights.kind.resource, 2);
        assert_eq!(weights.kind.resource_template, 3);

        assert_eq!(weights.health.ready, 0);
        assert_eq!(weights.health.reconnecting, 1);
        assert_eq!(weights.health.other, 2);
    }

    #[test]
    fn test_catalog_params_deserialize_search() {
        let json = serde_json::json!({
            "page": 1,
            "page_size": 10,
            "search": "file"
        });
        let params: super::CatalogParams = serde_json::from_value(json).expect("deserialize");
        assert_eq!(params.page, 1);
        assert_eq!(params.page_size, 10);
        assert_eq!(params.search, Some("file".to_string()));
        assert!(params.kind_filter.is_none());
    }

    #[test]
    fn test_catalog_params_deserialize_kind_filter() {
        let json = serde_json::json!({
            "page": 1,
            "page_size": 10,
            "kind_filter": ["tool", "prompt"]
        });
        let params: super::CatalogParams = serde_json::from_value(json).expect("deserialize");
        assert_eq!(params.page, 1);
        assert_eq!(params.page_size, 10);
        assert!(params.search.is_none());
        assert_eq!(params.kind_filter, Some(vec!["tool".to_string(), "prompt".to_string()]));
    }

    #[test]
    fn test_catalog_params_deserialize_both_filters() {
        let json = serde_json::json!({
            "page": 2,
            "page_size": 5,
            "search": "read",
            "kind_filter": ["resource"]
        });
        let params: super::CatalogParams = serde_json::from_value(json).expect("deserialize");
        assert_eq!(params.page, 2);
        assert_eq!(params.page_size, 5);
        assert_eq!(params.search, Some("read".to_string()));
        assert_eq!(params.kind_filter, Some(vec!["resource".to_string()]));
    }

    #[test]
    fn test_catalog_params_deserialize_defaults() {
        let json = serde_json::json!({});
        let params: super::CatalogParams = serde_json::from_value(json).expect("deserialize");
        assert_eq!(params.page, 1);
        assert_eq!(params.page_size, 20);
        assert!(params.search.is_none());
        assert!(params.kind_filter.is_none());
    }

    #[test]
    fn test_catalog_search_filter_logic() {
        use super::{SurfaceDirectoryItem, SurfaceKind};

        let summaries = [
            SurfaceDirectoryItem {
                capability_name: "read_file".to_string(),
                capability_kind: SurfaceKind::Tool,
                summary: Some("Read file contents".to_string()),
                action: "inspect_first",
                next_step: "details",
                server_id: "server-1".to_string(),
                server_name: "filesystem".to_string(),
                interaction_mode: "model_controlled",
                detail_hint: "Use details first.",
                source: None,
            },
            SurfaceDirectoryItem {
                capability_name: "write_file".to_string(),
                capability_kind: SurfaceKind::Tool,
                summary: Some("Write content to file".to_string()),
                action: "inspect_first",
                next_step: "details",
                server_id: "server-1".to_string(),
                server_name: "filesystem".to_string(),
                interaction_mode: "model_controlled",
                detail_hint: "Use details first.",
                source: None,
            },
            SurfaceDirectoryItem {
                capability_name: "list_directory".to_string(),
                capability_kind: SurfaceKind::Tool,
                summary: Some("List directory contents".to_string()),
                action: "inspect_first",
                next_step: "details",
                server_id: "server-1".to_string(),
                server_name: "filesystem".to_string(),
                interaction_mode: "model_controlled",
                detail_hint: "Use details first.",
                source: None,
            },
        ];

        let search_lower = "file".to_lowercase();
        let filtered: Vec<_> = summaries
            .iter()
            .filter(|item| {
                let name_match = item.capability_name.to_lowercase().contains(&search_lower);
                let summary_match = item
                    .summary
                    .as_ref()
                    .map(|s| s.to_lowercase().contains(&search_lower))
                    .unwrap_or(false);
                name_match || summary_match
            })
            .collect();

        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().any(|s| s.capability_name == "read_file"));
        assert!(filtered.iter().any(|s| s.capability_name == "write_file"));
        assert!(!filtered.iter().any(|s| s.capability_name == "list_directory"));
    }

    #[test]
    fn test_catalog_search_filter_by_summary() {
        use super::{SurfaceDirectoryItem, SurfaceKind};

        let summaries = [
            SurfaceDirectoryItem {
                capability_name: "execute_command".to_string(),
                capability_kind: SurfaceKind::Tool,
                summary: Some("Run shell commands in terminal".to_string()),
                action: "inspect_first",
                next_step: "details",
                server_id: "server-2".to_string(),
                server_name: "shell".to_string(),
                interaction_mode: "model_controlled",
                detail_hint: "Use details first.",
                source: None,
            },
            SurfaceDirectoryItem {
                capability_name: "get_weather".to_string(),
                capability_kind: SurfaceKind::Tool,
                summary: Some("Fetch weather data from API".to_string()),
                action: "inspect_first",
                next_step: "details",
                server_id: "server-3".to_string(),
                server_name: "weather".to_string(),
                interaction_mode: "model_controlled",
                detail_hint: "Use details first.",
                source: None,
            },
        ];

        let search_lower = "shell".to_lowercase();
        let filtered: Vec<_> = summaries
            .iter()
            .filter(|item| {
                let name_match = item.capability_name.to_lowercase().contains(&search_lower);
                let summary_match = item
                    .summary
                    .as_ref()
                    .map(|s| s.to_lowercase().contains(&search_lower))
                    .unwrap_or(false);
                name_match || summary_match
            })
            .collect();

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].capability_name, "execute_command");
    }

    #[test]
    fn test_catalog_kind_filter_logic() {
        use super::{SurfaceDirectoryItem, SurfaceKind};
        use std::collections::HashSet;

        let summaries = [
            SurfaceDirectoryItem {
                capability_name: "tool_one".to_string(),
                capability_kind: SurfaceKind::Tool,
                summary: None,
                action: "inspect_first",
                next_step: "details",
                server_id: "s1".to_string(),
                server_name: "server".to_string(),
                interaction_mode: "model_controlled",
                detail_hint: "",
                source: None,
            },
            SurfaceDirectoryItem {
                capability_name: "prompt_one".to_string(),
                capability_kind: SurfaceKind::Prompt,
                summary: None,
                action: "inspect_first",
                next_step: "details",
                server_id: "s1".to_string(),
                server_name: "server".to_string(),
                interaction_mode: "user_controlled_template",
                detail_hint: "",
                source: None,
            },
            SurfaceDirectoryItem {
                capability_name: "resource_one".to_string(),
                capability_kind: SurfaceKind::Resource,
                summary: None,
                action: "inspect_first",
                next_step: "details",
                server_id: "s1".to_string(),
                server_name: "server".to_string(),
                interaction_mode: "application_context",
                detail_hint: "",
                source: None,
            },
            SurfaceDirectoryItem {
                capability_name: "template_one".to_string(),
                capability_kind: SurfaceKind::ResourceTemplate,
                summary: None,
                action: "inspect_first",
                next_step: "details",
                server_id: "s1".to_string(),
                server_name: "server".to_string(),
                interaction_mode: "application_context_template",
                detail_hint: "",
                source: None,
            },
        ];

        let kinds = ["tool", "resource"];
        let allowed_kinds: HashSet<&str> = kinds.iter().copied().collect();

        let filtered: Vec<_> = summaries
            .iter()
            .filter(|item| {
                let kind_str = match item.capability_kind {
                    SurfaceKind::Tool => "tool",
                    SurfaceKind::Prompt => "prompt",
                    SurfaceKind::Resource => "resource",
                    SurfaceKind::ResourceTemplate => "resource_template",
                    SurfaceKind::Profile => "profile",
                };
                allowed_kinds.contains(kind_str)
            })
            .collect();

        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().any(|s| matches!(s.capability_kind, SurfaceKind::Tool)));
        assert!(
            filtered
                .iter()
                .any(|s| matches!(s.capability_kind, SurfaceKind::Resource))
        );
    }

    #[test]
    fn test_catalog_search_empty_returns_all() {
        use super::{SurfaceDirectoryItem, SurfaceKind};

        let summaries = [
            SurfaceDirectoryItem {
                capability_name: "tool_a".to_string(),
                capability_kind: SurfaceKind::Tool,
                summary: Some("Tool A".to_string()),
                action: "inspect_first",
                next_step: "details",
                server_id: "s1".to_string(),
                server_name: "server".to_string(),
                interaction_mode: "model_controlled",
                detail_hint: "",
                source: None,
            },
            SurfaceDirectoryItem {
                capability_name: "tool_b".to_string(),
                capability_kind: SurfaceKind::Tool,
                summary: Some("Tool B".to_string()),
                action: "inspect_first",
                next_step: "details",
                server_id: "s1".to_string(),
                server_name: "server".to_string(),
                interaction_mode: "model_controlled",
                detail_hint: "",
                source: None,
            },
        ];

        let search: Option<&str> = None;
        let filtered: Vec<_> = if let Some(search_term) = search {
            let search_lower = search_term.to_lowercase();
            summaries
                .iter()
                .filter(|item| {
                    let name_match = item.capability_name.to_lowercase().contains(&search_lower);
                    let summary_match = item
                        .summary
                        .as_ref()
                        .map(|s| s.to_lowercase().contains(&search_lower))
                        .unwrap_or(false);
                    name_match || summary_match
                })
                .collect()
        } else {
            summaries.iter().collect()
        };

        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_catalog_search_case_insensitive() {
        use super::{SurfaceDirectoryItem, SurfaceKind};

        let summaries = [SurfaceDirectoryItem {
            capability_name: "Read_File".to_string(),
            capability_kind: SurfaceKind::Tool,
            summary: Some("READ FILE CONTENTS".to_string()),
            action: "inspect_first",
            next_step: "details",
            server_id: "s1".to_string(),
            server_name: "server".to_string(),
            interaction_mode: "model_controlled",
            detail_hint: "",
            source: None,
        }];

        let search_lower = "FILE".to_lowercase();
        let filtered: Vec<_> = summaries
            .iter()
            .filter(|item| {
                let name_match = item.capability_name.to_lowercase().contains(&search_lower);
                let summary_match = item
                    .summary
                    .as_ref()
                    .map(|s| s.to_lowercase().contains(&search_lower))
                    .unwrap_or(false);
                name_match || summary_match
            })
            .collect();

        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn test_details_includes_workflow_hints_for_tool() {
        use super::{UcanDetailLevel, extract_argument_tips_from_tool};

        let tool = Tool::new(
            "test_tool",
            "A test tool for workflow hints",
            Arc::new(
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path to read"},
                        "encoding": {"type": "string", "description": "File encoding"}
                    },
                    "required": ["path"]
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        );

        let details = tool_details_value(&tool, UcanDetailLevel::Summary);
        assert!(details.get("input_fields").is_some());
        assert!(details.get("required_fields").is_some());

        let tips = extract_argument_tips_from_tool(&tool);
        assert_eq!(tips.len(), 2);

        let path_tip = tips.iter().find(|t| t.name == "path").expect("path tip");
        assert!(path_tip.required);
        assert_eq!(path_tip.type_hint, Some("string".to_string()));
        assert_eq!(path_tip.description, Some("File path to read".to_string()));

        let encoding_tip = tips.iter().find(|t| t.name == "encoding").expect("encoding tip");
        assert!(!encoding_tip.required);
    }

    #[test]
    fn test_details_includes_related_capabilities() {
        use super::{RelatedSurfaceItem, SurfaceKind};

        let related = RelatedSurfaceItem {
            capability_name: "related_tool".to_string(),
            capability_kind: SurfaceKind::Tool,
            summary: Some("A related tool".to_string()),
        };

        let json = serde_json::to_string(&related).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["capability_name"], "related_tool");
        assert_eq!(parsed["capability_kind"], "tool");
        assert_eq!(parsed["summary"], "A related tool");
    }

    #[test]
    fn test_details_resource_template_explains_uri_construction() {
        use super::resource_template_details_value;
        use rmcp::model::ResourceTemplate;

        let template = ResourceTemplate::new("file:///{path}", "file_template").with_description("File template");

        let summary = resource_template_details_value(&template, UcanDetailLevel::Summary).expect("summary details");
        assert_eq!(summary["uri_template"], "file:///{path}");
        assert_eq!(summary["read_with"], "resources/read");
        assert!(summary.get("usage").is_none());

        let full = resource_template_details_value(&template, UcanDetailLevel::Full).expect("full details");
        assert_eq!(full["uriTemplate"], "file:///{path}");
        assert_eq!(full["read_with"], "resources/read");
        assert!(
            full["instruction"]
                .as_str()
                .is_some_and(|value| value.contains("RFC 6570"))
        );
    }

    #[test]
    fn test_argument_tips_from_tool() {
        use super::extract_argument_tips_from_tool;

        let tool = Tool::new(
            "complex_tool",
            "A tool with complex arguments",
            Arc::new(
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search query"},
                        "limit": {"type": "integer", "description": "Max results"},
                        "verbose": {"type": "boolean"}
                    },
                    "required": ["query"]
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        );

        let tips = extract_argument_tips_from_tool(&tool);
        assert_eq!(tips.len(), 3);

        let query_tip = tips.iter().find(|t| t.name == "query").expect("query tip");
        assert!(query_tip.required);
        assert_eq!(query_tip.type_hint, Some("string".to_string()));

        let limit_tip = tips.iter().find(|t| t.name == "limit").expect("limit tip");
        assert!(!limit_tip.required);
        assert_eq!(limit_tip.type_hint, Some("integer".to_string()));

        let verbose_tip = tips.iter().find(|t| t.name == "verbose").expect("verbose tip");
        assert!(!verbose_tip.required);
        assert_eq!(verbose_tip.type_hint, Some("boolean".to_string()));
        assert!(verbose_tip.description.is_none());
    }

    #[test]
    fn test_required_arguments_from_tool_and_call_requirements() {
        use super::{call_requirements_for_tool, required_arguments_from_tool};

        let tool = Tool::new(
            "validate_input",
            "A tool requiring query and format",
            Arc::new(
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "format": {"type": "string"},
                        "limit": {"type": "integer"}
                    },
                    "required": ["query", "format"]
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        );

        let required = required_arguments_from_tool(&tool);
        assert_eq!(required.len(), 2);
        assert!(required.contains(&"query".to_string()));
        assert!(required.contains(&"format".to_string()));

        let requirements = call_requirements_for_tool(&tool);
        assert!(requirements.accepts_arguments);
        assert!(!requirements.call_ready_without_arguments);
        assert_eq!(requirements.required_arguments.len(), 2);
    }

    #[test]
    fn test_required_arguments_from_prompt_and_call_requirements() {
        use super::{call_requirements_for_prompt, required_arguments_from_prompt};
        use rmcp::model::{Prompt, PromptArgument};

        let arguments = vec![
            PromptArgument::new("topic")
                .with_description("Topic")
                .with_required(true),
            PromptArgument::new("tone")
                .with_description("Tone")
                .with_required(false),
        ];

        let prompt = Prompt::new("compose", Some("Compose text"), Some(arguments));
        let required = required_arguments_from_prompt(&prompt);
        assert_eq!(required, vec!["topic".to_string()]);

        let requirements = call_requirements_for_prompt(&prompt);
        assert!(requirements.accepts_arguments);
        assert!(!requirements.call_ready_without_arguments);
        assert_eq!(requirements.required_arguments, vec!["topic".to_string()]);
    }

    #[test]
    fn test_argument_tips_from_prompt() {
        use super::extract_argument_tips_from_prompt;
        use rmcp::model::{Prompt, PromptArgument};

        let arguments = vec![
            PromptArgument::new("code")
                .with_description("Code to review")
                .with_required(true),
            PromptArgument::new("language")
                .with_description("Programming language")
                .with_required(false),
        ];

        let prompt = Prompt::new("code_review", Some("Review code"), Some(arguments));

        let tips = extract_argument_tips_from_prompt(&prompt);
        assert_eq!(tips.len(), 2);

        let code_tip = tips.iter().find(|t| t.name == "code").expect("code tip");
        assert!(code_tip.required);
        assert_eq!(code_tip.description, Some("Code to review".to_string()));
        assert!(code_tip.type_hint.is_none());

        let lang_tip = tips.iter().find(|t| t.name == "language").expect("language tip");
        assert!(!lang_tip.required);
    }

    #[test]
    fn test_workflow_hints_default_values() {
        use super::{
            WorkflowHints, default_workflow_hints_prompt, default_workflow_hints_resource,
            default_workflow_hints_resource_template, default_workflow_hints_tool,
        };

        let hints = WorkflowHints::default();

        assert!(!hints.tool.is_empty());
        assert!(!hints.prompt.is_empty());
        assert!(!hints.resource.is_empty());
        assert!(!hints.resource_template.is_empty());

        let tool_hints = default_workflow_hints_tool();
        assert!(tool_hints.iter().any(|hint| hint.contains("required arguments")));

        let prompt_hints = default_workflow_hints_prompt();
        assert!(
            prompt_hints
                .iter()
                .any(|hint| hint.contains(super::MCPMATE_UCAN_CALL_TOOL))
        );

        let resource_hints = default_workflow_hints_resource();
        assert!(resource_hints[0].contains("resources/read"));

        let template_hints = default_workflow_hints_resource_template();
        assert!(template_hints.iter().any(|hint| hint.contains("resources/read")));
    }

    #[test]
    fn test_catalog_enrichment_fields_in_summary() {
        use super::{SurfaceDirectoryItem, SurfaceKind};

        let summary_with_enrichment = SurfaceDirectoryItem {
            capability_name: "source_tool".to_string(),
            capability_kind: SurfaceKind::Tool,
            summary: Some("A tool from a source-linked server".to_string()),
            action: "inspect_first",
            next_step: "details",
            server_id: "server-1".to_string(),
            server_name: "source-linked-server".to_string(),
            interaction_mode: "model_controlled",
            detail_hint: "Use details first.",
            source: Some("registry:filesystem".to_string()),
        };

        assert!(summary_with_enrichment.source.is_some());
        assert_eq!(summary_with_enrichment.source.as_deref(), Some("registry:filesystem"));

        let summary_without_enrichment = SurfaceDirectoryItem {
            capability_name: "local_tool".to_string(),
            capability_kind: SurfaceKind::Tool,
            summary: Some("A local tool".to_string()),
            action: "inspect_first",
            next_step: "details",
            server_id: "server-2".to_string(),
            server_name: "local-server".to_string(),
            interaction_mode: "model_controlled",
            detail_hint: "Use details first.",
            source: None,
        };

        assert!(summary_without_enrichment.source.is_none());
    }

    #[test]
    fn test_catalog_enrichment_config_default() {
        use super::default_catalog_enrich_from_source;

        assert!(default_catalog_enrich_from_source());
    }
}
