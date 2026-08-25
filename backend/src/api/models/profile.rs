// MCP Proxy API models for Profile management
// Contains data models for Profile endpoints

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::config::models::ProfileMode;
use crate::core::profile::guide::{
    RenderedWorkflowSkill, WorkflowGuideCapability, WorkflowGuideExternalDocument, WorkflowGuidePackageFile,
    WorkflowGuideView,
};
use crate::core::profile::materials::{
    WorkflowMaterial, WorkflowMaterialKind, WorkflowMaterialsReorderCommand, WorkflowMaterialsView,
    WorkflowStepMaterialsSaveCommand,
};
use crate::core::profile::workflow::{
    WorkflowGuidanceSaveCommand, WorkflowSpecification, WorkflowSpecificationPreview,
};

// Import the unified response macro
use crate::macros::resp::api_resp;

// ==========================================
// COMMON REQUEST STRUCTURES
// ==========================================

/// Generic request with profile ID
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Request with profile ID")]
pub struct ProfileIdReq {
    #[schemars(description = "Profile ID")]
    pub id: String,
}

// ==========================================
// STANDARDIZED REQUEST/RESPONSE MODELS
// Following server module patterns with JsonSchema annotations
// ==========================================

// Action Enums
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(description = "Available profile management actions")]
pub enum ProfileAction {
    #[schemars(description = "Activate the profile")]
    Activate,
    #[schemars(description = "Deactivate the profile")]
    Deactivate,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(description = "Available component management actions")]
pub enum ProfileComponentAction {
    #[schemars(description = "Enable the component")]
    Enable,
    #[schemars(description = "Disable the component")]
    Disable,
    #[schemars(description = "Remove the component")]
    Remove,
    #[schemars(description = "Replace the complete component selection")]
    Replace,
}

// Query Request Models
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Request for profile list operation")]
pub struct ProfileListReq {
    #[serde(default)]
    #[schemars(description = "Filter by profile status: active, inactive, all")]
    pub filter_type: Option<String>,

    #[serde(default)]
    #[schemars(description = "Filter by profile type: host_app, scenario, shared")]
    pub profile_type: Option<String>,

    #[serde(default)]
    #[schemars(description = "Page limit for pagination (max 100)")]
    pub limit: Option<usize>,

    #[serde(default)]
    #[schemars(description = "Page offset for pagination")]
    pub offset: Option<usize>,
}

/// Request for profile details operation
pub type ProfileDetailsReq = ProfileIdReq;

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Request for profile component list (servers, tools, etc.)")]
pub struct ProfileComponentListReq {
    #[schemars(description = "Profile identifier")]
    pub profile_id: String,

    #[serde(default)]
    #[schemars(description = "Show only enabled components")]
    pub enabled_only: Option<bool>,
}

// Payload Request Models
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Request for profile management operations")]
pub struct ProfileManageReq {
    #[schemars(description = "Profile identifiers (single or multiple)")]
    pub ids: Vec<String>,

    #[schemars(description = "Management action to perform")]
    pub action: ProfileAction,

    #[schemars(description = "Exact authoring generation for every target Profile")]
    pub expected_authoring_generations: BTreeMap<String, i64>,

    #[schemars(description = "Whether to trigger client configuration synchronization")]
    #[serde(default)]
    pub sync: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Request for component management operations (unified single and batch operations)")]
pub struct ProfileComponentManageReq {
    #[schemars(description = "Profile identifier")]
    pub profile_id: String,

    #[schemars(description = "Component identifiers (single element for individual operations, multiple for batch)")]
    pub component_ids: Vec<String>,

    #[schemars(description = "Management action to perform on component(s)")]
    pub action: ProfileComponentAction,

    #[schemars(description = "Expected Profile authoring generation")]
    pub expected_authoring_generation: i64,

    #[schemars(description = "Exact revisions for the Servers related to the selected capabilities")]
    pub source_revision_set: super::CatalogRevisionSet,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Request for Profile Server relationship management")]
pub struct ProfileServerManageReq {
    #[schemars(description = "Profile identifier")]
    pub profile_id: String,

    #[schemars(description = "Server identifiers")]
    pub component_ids: Vec<String>,

    #[schemars(description = "Management action")]
    pub action: ProfileComponentAction,

    #[schemars(description = "Expected Profile authoring generation")]
    pub expected_authoring_generation: i64,
}

/// Request for profile deletion
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Request for deleting a profile and republishing affected consumers")]
pub struct ProfileDeleteReq {
    #[schemars(description = "Profile ID")]
    pub id: String,

    #[schemars(description = "Expected Profile authoring generation")]
    pub expected_authoring_generation: i64,
}

// Response Models (with Resp suffix)
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(description = "Response for profile list operation")]
pub struct ProfileListData {
    #[schemars(description = "List of profile")]
    pub profile: Vec<ProfileData>,

    #[schemars(description = "Total number of profile matching filter")]
    pub total: usize,

    #[schemars(description = "ISO 8601 timestamp of response")]
    pub timestamp: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(description = "Response for profile details operation")]
pub struct ProfileDetailsData {
    #[schemars(description = "Profile details")]
    pub profile: ProfileData,

    #[schemars(description = "Number of enabled servers in profile")]
    pub servers_count: usize,

    #[schemars(description = "Number of enabled tools in profile")]
    pub tools_count: usize,

    #[schemars(description = "Number of enabled resources in profile")]
    pub resources_count: usize,

    #[schemars(description = "Number of enabled prompts in profile")]
    pub prompts_count: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(description = "Single profile operation result")]
pub struct ProfileOperationResult {
    #[schemars(description = "Profile identifier")]
    pub id: String,

    #[schemars(description = "Profile name")]
    pub name: String,

    #[schemars(description = "Operation result")]
    pub result: String,

    #[schemars(description = "Current profile status after operation")]
    pub status: String,

    #[schemars(description = "Error message if operation failed")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(description = "Response for profile management operations")]
pub struct ProfileManageData {
    #[schemars(description = "Number of successful operations")]
    pub success_count: usize,

    #[schemars(description = "Number of failed operations")]
    pub failed_count: usize,

    #[schemars(description = "List of operation results")]
    pub results: Vec<ProfileOperationResult>,

    #[schemars(description = "ISO 8601 timestamp of operation")]
    pub timestamp: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(description = "Response for profile servers list operation")]
pub struct ProfileServersListData {
    #[schemars(description = "Profile identifier")]
    pub profile_id: String,

    #[schemars(description = "Profile name")]
    pub profile_name: String,

    #[schemars(description = "List of servers in this profile")]
    pub servers: Vec<ProfileServerResp>,

    #[schemars(description = "Total number of servers in profile")]
    pub total: usize,

    #[schemars(description = "Profile authoring generation represented by this payload")]
    pub authoring_generation: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(description = "Response for profile tools list operation")]
pub struct ProfileToolsListData {
    #[schemars(description = "Profile identifier")]
    pub profile_id: String,

    #[schemars(description = "Profile name")]
    pub profile_name: String,

    #[schemars(description = "List of tools in this profile")]
    pub tools: Vec<ProfileToolData>,

    #[schemars(description = "Total number of tools in profile")]
    pub total: usize,

    #[schemars(description = "Catalog revision set represented by this management payload")]
    pub source_revision_set: super::CatalogRevisionSet,

    #[schemars(description = "Profile authoring generation represented by this payload")]
    pub authoring_generation: i64,
}

api_resp!(
    ProfileResourcesListResp,
    ProfileResourcesListData,
    "Response for profile resources list operation"
);

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(description = "Data for profile resources list operation")]
pub struct ProfileResourcesListData {
    #[schemars(description = "Profile identifier")]
    pub profile_id: String,

    #[schemars(description = "Profile name")]
    pub profile_name: String,

    #[schemars(description = "List of resources in this profile")]
    pub resources: Vec<ProfileResourceData>,

    #[schemars(description = "Total number of resources in profile")]
    pub total: usize,

    #[schemars(description = "Catalog revision set represented by this management payload")]
    pub source_revision_set: super::CatalogRevisionSet,

    #[schemars(description = "Profile authoring generation represented by this payload")]
    pub authoring_generation: i64,
}

api_resp!(
    ProfileResourceTemplatesListResp,
    ProfileResourceTemplatesListData,
    "Response for profile resource templates list operation"
);

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(description = "Data for profile resource templates list operation")]
pub struct ProfileResourceTemplatesListData {
    #[schemars(description = "Profile identifier")]
    pub profile_id: String,

    #[schemars(description = "Profile name")]
    pub profile_name: String,

    #[schemars(description = "List of resource templates in this profile")]
    pub templates: Vec<ProfileResourceTemplateData>,

    #[schemars(description = "Total number of templates in profile")]
    pub total: usize,

    #[schemars(description = "Catalog revision set represented by this management payload")]
    pub source_revision_set: super::CatalogRevisionSet,

    #[schemars(description = "Profile authoring generation represented by this payload")]
    pub authoring_generation: i64,
}

api_resp!(
    ProfilePromptsListResp,
    ProfilePromptsListData,
    "Response for profile prompts list operation"
);

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(description = "Data for profile prompts list operation")]
pub struct ProfilePromptsListData {
    #[schemars(description = "Profile identifier")]
    pub profile_id: String,

    #[schemars(description = "Profile name")]
    pub profile_name: String,

    #[schemars(description = "List of prompts in this profile")]
    pub prompts: Vec<ProfilePromptData>,

    #[schemars(description = "Total number of prompts in profile")]
    pub total: usize,

    #[schemars(description = "Catalog revision set represented by this management payload")]
    pub source_revision_set: super::CatalogRevisionSet,

    #[schemars(description = "Profile authoring generation represented by this payload")]
    pub authoring_generation: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(description = "Response for component management operations")]
pub struct ProfileServerManageData {
    #[schemars(description = "Profile identifier")]
    pub profile_id: String,

    #[schemars(description = "Operation results (single element for individual operations, multiple for batch)")]
    pub results: Vec<ComponentOperationResult>,

    #[schemars(description = "Operation summary")]
    pub summary: String,

    #[schemars(description = "Overall operation status")]
    pub status: String,

    #[schemars(description = "ISO 8601 timestamp of operation")]
    pub timestamp: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(description = "Individual component operation result")]
pub struct ComponentOperationResult {
    #[schemars(description = "Component identifier")]
    pub component_id: String,

    #[schemars(description = "Component type")]
    pub component_type: String,

    #[schemars(description = "Whether the operation succeeded")]
    pub success: bool,

    #[schemars(description = "Operation result message")]
    pub result: String,

    #[schemars(description = "Error message if operation failed")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ==========================================
// LEGACY MODELS (kept for backward compatibility)
// ==========================================

/// Profile response
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProfileData {
    /// Unique ID
    pub id: String,
    /// Name of the profile
    pub name: String,
    /// Description of the profile
    pub description: Option<String>,
    /// Type of the profile (host_app, scenario, shared)
    pub profile_type: String,
    /// Role of the profile (user, default_anchor)
    pub role: String,
    /// Priority of the profile (higher priority wins in case of conflicts)
    pub priority: i32,
    /// Whether the profile is currently active
    pub is_active: bool,
    /// Whether the profile is the default one
    pub is_default: bool,
    /// Monotonic Profile authoring generation.
    pub authoring_generation: i64,
    /// Authoring mode (capability or workflow).
    pub profile_mode: ProfileMode,
    /// Allowed operations on this profile
    pub allowed_operations: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ProfileAuthoringViewData {
    pub profile: ProfileData,
    pub server_ids: Vec<String>,
    pub profile_mode: ProfileMode,
    pub skill_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProfileAuthoringSaveReq {
    pub id: Option<String>,
    pub expected_authoring_generation: Option<i64>,
    pub name: String,
    pub description: Option<String>,
    pub profile_type: String,
    pub priority: i32,
    pub is_active: bool,
    pub is_default: bool,
    pub server_ids: Vec<String>,
    pub clone_from_id: Option<String>,
    #[serde(default)]
    pub profile_mode: Option<ProfileMode>,
    #[serde(default)]
    pub skill_name: Option<String>,
    #[serde(default)]
    pub workflow_guidance: Option<WorkflowGuidanceSaveCommand>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ProfileAuthoringSaveData {
    pub profile: ProfileData,
    pub profile_mode: ProfileMode,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowSpecificationViewData {
    pub specification: WorkflowSpecification,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowSpecificationPreviewData {
    pub preview: WorkflowSpecificationPreview,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowGuideSaveReq {
    pub profile_id: String,
    pub expected_guide_revision: i64,
    pub markdown: String,
    pub reclamation_confirmation: Option<WorkflowGuideReclamationConfirmationReq>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowGuidePreviewReq {
    pub profile_id: String,
    pub relative_path: Option<String>,
    pub markdown: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowGuideReclamationConfirmationReq {
    pub package_files: Vec<WorkflowGuidePackageFileRevisionReq>,
    pub capability_names: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowGuidePackageFileRevisionReq {
    pub package_file_id: String,
    pub file_revision: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowGuidePackageFileDeleteReq {
    pub profile_id: String,
    pub package_file_id: String,
    pub expected_file_revision: i64,
    pub expected_guide_revision: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowGuideExternalDocumentReq {
    pub profile_id: String,
    pub package_file_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowGuideRepairReq {
    pub profile_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowGuideViewData {
    pub guide: WorkflowGuideView,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowGuideSaveData {
    pub guide: WorkflowGuideView,
    pub projected_skill: RenderedWorkflowSkill,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowGuidePackageFileSaveData {
    pub guide: WorkflowGuideView,
    pub projected_skill: RenderedWorkflowSkill,
    pub package_file: WorkflowGuidePackageFile,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowGuidePreviewData {
    pub projected_skill: RenderedWorkflowSkill,
    pub active_document: RenderedWorkflowSkill,
    pub orphaned_package_files: Vec<WorkflowGuidePackageFile>,
    pub orphaned_capabilities: Vec<WorkflowGuideCapability>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowGuideExternalDocumentData {
    pub document: WorkflowGuideExternalDocument,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowMaterialSaveReq {
    pub profile_id: String,
    pub material_id: Option<String>,
    pub expected_material_revision: Option<i64>,
    pub expected_materials_revision: i64,
    pub title: String,
    pub kind: WorkflowMaterialKind,
    pub external_url: Option<String>,
    pub markdown_content: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowMaterialDeleteReq {
    pub profile_id: String,
    pub material_id: String,
    pub expected_material_revision: i64,
    pub expected_materials_revision: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowStepMaterialsSaveReq {
    pub profile_id: String,
    pub step_id: String,
    pub material_ids: Vec<String>,
    pub expected_materials_revision: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowMaterialsReorderReq {
    pub profile_id: String,
    pub material_ids: Vec<String>,
    pub expected_materials_revision: i64,
}

impl From<WorkflowStepMaterialsSaveReq> for WorkflowStepMaterialsSaveCommand {
    fn from(value: WorkflowStepMaterialsSaveReq) -> Self {
        Self {
            profile_id: value.profile_id,
            step_id: value.step_id,
            material_ids: value.material_ids,
            expected_materials_revision: value.expected_materials_revision,
        }
    }
}

impl From<WorkflowMaterialsReorderReq> for WorkflowMaterialsReorderCommand {
    fn from(value: WorkflowMaterialsReorderReq) -> Self {
        Self {
            profile_id: value.profile_id,
            material_ids: value.material_ids,
            expected_materials_revision: value.expected_materials_revision,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowMaterialsViewData {
    pub materials: WorkflowMaterialsView,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowMaterialSaveData {
    pub material: WorkflowMaterial,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowMaterialDeleteData {
    pub material_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowStepMaterialsSaveData {
    pub material_ids: Vec<String>,
}

pub type WorkflowMaterialsReorderData = WorkflowStepMaterialsSaveData;
pub type WorkflowMaterialsReorderResp = WorkflowStepMaterialsSaveResp;

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowMaterialPreviewData {
    pub content: String,
}

/// Operation response
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Operation response details")]
pub struct ProfileOperationData {
    /// Unique ID
    #[schemars(description = "Unique identifier of the profile")]
    pub id: String,
    /// Name of the profile
    #[schemars(description = "Name of the profile")]
    pub name: String,
    /// Result of the operation
    #[schemars(description = "Result description of the operation")]
    pub result: String,
    /// Status after the operation
    #[schemars(description = "Current status after the operation")]
    pub status: String,
    /// Allowed operations after this operation
    #[schemars(description = "List of operations allowed on this profile")]
    pub allowed_operations: Vec<String>,
}

/// Profile server response
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProfileServerResp {
    /// Server ID
    pub id: String,
    /// Server name
    pub name: String,
    /// Whether the server is enabled in this profile
    pub enabled: bool,
    /// Allowed operations on this server
    pub allowed_operations: Vec<String>,
}

/// Profile tool response
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProfileToolData {
    /// Stable CapabilityRef identity.
    pub ref_id: String,
    /// Server ID
    pub server_id: String,
    /// Server name
    pub server_name: String,
    /// Tool name (original name from upstream server)
    pub tool_name: String,
    /// Unique name for external display and routing
    pub unique_name: String,
    /// Tool description from the cached server capability snapshot
    pub description: Option<String>,
    /// Whether the tool is enabled in this profile
    pub enabled: bool,
    pub state: String,
    pub state_generation: i64,
    /// Allowed operations on this tool
    pub allowed_operations: Vec<String>,
}

/// Profile resource response
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProfileResourceData {
    /// Stable CapabilityRef identity.
    pub ref_id: String,
    /// Server ID
    pub server_id: String,
    /// Server name
    pub server_name: String,
    /// Resource URI (original URI from upstream server)
    pub resource_uri: String,
    /// External resource identifier used for selection and routing
    pub unique_uri: String,
    /// Resource description from the cached server capability snapshot
    pub description: Option<String>,
    /// Whether the resource is enabled in this profile
    pub enabled: bool,
    pub state: String,
    pub state_generation: i64,
    /// Allowed operations on this resource
    pub allowed_operations: Vec<String>,
}

/// Profile resource template response (reuse shape as ProfileResourceData but with uri_template)
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProfileResourceTemplateData {
    /// Stable CapabilityRef identity.
    pub ref_id: String,
    /// Server ID
    pub server_id: String,
    /// Server name
    pub server_name: String,
    /// Resource URI template (original template from upstream server)
    pub uri_template: String,
    /// External resource template identifier used for selection and routing
    pub unique_uri_template: String,
    /// Resource template description from the cached server capability snapshot
    pub description: Option<String>,
    /// Whether the template is enabled in this profile
    pub enabled: bool,
    pub state: String,
    pub state_generation: i64,
    /// Allowed operations on this template
    pub allowed_operations: Vec<String>,
}

/// Profile prompt response
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProfilePromptData {
    /// Stable CapabilityRef identity.
    pub ref_id: String,
    /// Server ID
    pub server_id: String,
    /// Server name
    pub server_name: String,
    /// Prompt name (original name from upstream server)
    pub prompt_name: String,
    /// External prompt identifier used for selection and routing
    pub unique_name: String,
    /// Prompt description from the cached server capability snapshot
    pub description: Option<String>,
    /// Whether the prompt is enabled in this profile
    pub enabled: bool,
    pub state: String,
    pub state_generation: i64,
    /// Allowed operations on this prompt
    pub allowed_operations: Vec<String>,
}

// ==========================================
// SPECIFIC API RESPONSE TYPES
// ==========================================

// Generate response structures using macro
api_resp!(ProfileListResp, ProfileListData, "Profile list API response");
api_resp!(ProfileDetailsResp, ProfileDetailsData, "Profile details API response");

api_resp!(ProfileManageResp, ProfileManageData, "Profile management API response");
api_resp!(ProfileResp, ProfileData, "Profile API response");
api_resp!(
    ProfileAuthoringViewResp,
    ProfileAuthoringViewData,
    "Profile authoring view API response"
);
api_resp!(
    ProfileAuthoringSaveResp,
    ProfileAuthoringSaveData,
    "Profile authoring save API response"
);
api_resp!(
    WorkflowSpecificationViewResp,
    WorkflowSpecificationViewData,
    "Workflow specification view API response"
);
api_resp!(
    WorkflowSpecificationPreviewResp,
    WorkflowSpecificationPreviewData,
    "Workflow specification preview API response"
);
api_resp!(
    WorkflowGuideViewResp,
    WorkflowGuideViewData,
    "Workflow Guide view API response"
);
api_resp!(
    WorkflowGuideSaveResp,
    WorkflowGuideSaveData,
    "Workflow Guide save API response"
);
api_resp!(
    WorkflowGuidePackageFileSaveResp,
    WorkflowGuidePackageFileSaveData,
    "Workflow Guide package-file save API response"
);
api_resp!(
    WorkflowGuidePreviewResp,
    WorkflowGuidePreviewData,
    "Workflow Guide preview API response"
);
api_resp!(
    WorkflowGuideExternalDocumentResp,
    WorkflowGuideExternalDocumentData,
    "Workflow Guide external Markdown document API response"
);
api_resp!(
    WorkflowMaterialsViewResp,
    WorkflowMaterialsViewData,
    "Workflow Materials view API response"
);
api_resp!(
    WorkflowMaterialSaveResp,
    WorkflowMaterialSaveData,
    "Workflow Material save API response"
);
api_resp!(
    WorkflowMaterialDeleteResp,
    WorkflowMaterialDeleteData,
    "Workflow Material delete API response"
);
api_resp!(
    WorkflowStepMaterialsSaveResp,
    WorkflowStepMaterialsSaveData,
    "Workflow Step Materials save API response"
);
api_resp!(
    WorkflowMaterialPreviewResp,
    WorkflowMaterialPreviewData,
    "Workflow Material preview API response"
);
api_resp!(
    ProfileServersListResp,
    ProfileServersListData,
    "Profile servers list API response"
);
api_resp!(
    ProfileToolsListResp,
    ProfileToolsListData,
    "Profile tools list API response"
);
api_resp!(
    ProfileServerManageResp,
    ProfileServerManageData,
    "Profile component manage API response"
);
