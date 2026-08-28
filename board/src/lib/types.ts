// API Response Types
export interface ServerIcon {
  src: string;
  mimeType?: string | null;
  sizes?: string | null;
}

export interface RegistryRepositoryInfo {
  url?: string | null;
  source?: string | null;
  subfolder?: string | null;
  id?: string | null;
}

export interface RegistryOfficialMeta {
  status?: string | null;
  publishedAt?: string | null;
  updatedAt?: string | null;
  isLatest?: boolean | null;
  serverId?: string | null;
  versionId?: string | null;
}

export interface RegistryMetaPayload {
  "io.modelcontextprotocol.registry/official"?: RegistryOfficialMeta | null;
  "io.modelcontextprotocol.registry/publisher-provided"?: Record<
    string,
    unknown
  > | null;
  [namespace: string]: unknown;
}

export type SnapshotState = "ready" | "invalidated" | "unavailable";
export type DeclarationState = "unknown" | "unsupported" | "supported";
export type InventoryState = "unknown" | "complete" | "failed";
export type CapabilityFailureKind =
  | "timeout"
  | "session_gone"
  | "transport_closed"
  | "stale_generation"
  | "authentication"
  | "auth_required"
  | "unauthorized"
  | "forbidden"
  | "insufficient_scope"
  | "protocol"
  | "application"
  | "other";

export interface CapabilityKindSummary {
  declaration: DeclarationState;
  inventory: InventoryState;
  currentCount: number;
  currentAvailable: boolean;
  lastError?: string | null;
  failureKind?: CapabilityFailureKind | null;
}

export interface ServerCapabilitySummary {
  snapshotState: SnapshotState;
  revision: number;
  observedAt: string;
  tools: CapabilityKindSummary;
  prompts: CapabilityKindSummary;
  resources: CapabilityKindSummary;
  resourceTemplates: CapabilityKindSummary;
}

export interface ServerMetaInfo {
  description?: string | null;
  version?: string | null;
  websiteUrl?: string | null;
  repository?: RegistryRepositoryInfo | null;
  _meta?: RegistryMetaPayload | null;
  extras?: Record<string, unknown> | null;
  icons?: ServerIcon[];
}

/** Discriminated source type for a server's origin. */
export type ServerSourceType =
  "registry" | "catalog" | "browser" | "portal" | "local" | "other";

/** Structured origin record attached to a server. */
export interface ServerSource {
  type: ServerSourceType;
  ref?: string;
}

export interface StandardServerInfo {
  name?: string | null;
  title?: string | null;
  version?: string | null;
}

export interface ServerNamespaceConflict {
  server_id: string;
  namespace: string;
}

export interface ServerNamespaceIssue {
  code:
    | "namespace_conflict"
    | "manual_remediation_required"
    | "pending_repair"
    | "capability_collision";
  current_namespace: string;
  suggested_namespace?: string | null;
  conflicts?: ServerNamespaceConflict[];
  remediation_allowed?: boolean;
}

export type TransportValidityState = "valid" | "invalid" | "missing";

export interface TransportValidityDiagnostic {
  code: string;
  field: string;
}

export type ApiConfigValue =
  | { kind: "literal"; value: string }
  | { kind: "secret_ref"; alias: string };

export type ApiServerTransportDraft =
  | {
      kind: "stdio";
      command?: string | null;
      args: string[];
      env: Record<string, ApiConfigValue>;
    }
  | {
      kind: "http";
      protocol: "sse" | "streamable_http";
      endpoint?: string | null;
      headers: Record<string, ApiConfigValue>;
    }
  | { kind: "unrecognized"; declared_type: string };

export interface ServerTransportValidity {
  state: TransportValidityState;
  diagnostics: TransportValidityDiagnostic[];
  draft?: ApiServerTransportDraft | null;
}

export type TransportFocusField = "type" | "command" | "url";

export function resolveTransportFocusField(
  diagnosticField: string | null | undefined,
  serverType: string | null | undefined,
): TransportFocusField {
	if (diagnosticField === "transport") return "type";
	if (diagnosticField === "command") return "command";
  if (diagnosticField === "endpoint" || diagnosticField === "url") {
    return "url";
  }
  return serverType?.toLowerCase() === "stdio" ? "command" : "url";
}

export interface ServerSummary {
  id: string;
  name: string;
  server_type?: string;
	status?: string;
	auth_mode?: string | null;
	enabled?: boolean;
  unify_direct_exposure_eligible?: boolean;
  url?: string | null;
  globally_enabled?: boolean;
  enabled_in_suits?: boolean;
  enabled_in_profile?: boolean;
  source?: ServerSource;
  instance_count?: number;
  instances?: InstanceSummary[];
  meta?: ServerMetaInfo;
  server_info?: StandardServerInfo | null;
  icons?: ServerIcon[];
  capability?: ServerCapabilitySummary;
  protocol_version?: string | null;
  server_version?: string | null;
  created_at?: string | null;
  updated_at?: string | null;
  oauth_status?: OAuthStatus["state"] | null;
  oauth_custody_state?: OAuthStatus["custody_state"] | null;
  oauth_requires_reconnect?: boolean | null;
  oauth_issue?: OAuthStatus["issue"] | null;
  namespace_issue?: ServerNamespaceIssue | null;
  transport_validity?: ServerTransportValidity;
  source_revision_set?: CatalogRevisionSet;
}

export interface ServerListResponse {
  servers: ServerSummary[];
}

export interface InstanceSummary {
  id: string;
  name: string;
  status: string;
  startTime?: string;
  started_at?: string;
  startedAt?: string;
  lastResponseAt?: unknown;
}

export interface ServerDetail extends ServerSummary {
  command?: string;
  commandPath?: string;
  args?: string[];
  env?: Record<string, string>;
  headers?: Record<string, string> | null;
  instances: InstanceSummary[];
  capability_discovery?: ServerCapabilityDiscovery;
}

export interface ServerCapabilityDiscovery {
  attempted: boolean;
  status: "succeeded" | "failed";
  error?: string;
}

export interface RegistryInput {
  description?: string;
  isRequired?: boolean;
  isSecret?: boolean;
  value?: string;
  default?: string;
  placeholder?: string;
  choices?: string[];
}

export interface RegistryTransportHeader extends RegistryInput {
  name: string;
}

export interface RegistryTransport {
  type?: string;
  url?: string;
  headers?: RegistryTransportHeader[] | null;
  variables?: Record<string, RegistryInput> | null;
}

export interface RegistryPackageArgument {
  name?: string;
  description?: string;
  type?: string;
  value?: string;
  isRequired?: boolean;
  default?: string;
  valueHint?: string;
}

export interface RegistryPackage {
  registryType?: string;
  registryBaseUrl?: string;
  identifier?: string;
  version?: string;
  runtimeHint?: string;
  fileSha256?: string;
  transport?: { type: string };
  environmentVariables?: RegistryTransportHeader[] | null;
  packageArguments?: RegistryPackageArgument[] | null;
  runtimeArguments?: RegistryPackageArgument[] | null;
}

export interface RegistryServerMeta {
  "io.modelcontextprotocol.registry/official"?: RegistryOfficialMeta;
  "io.modelcontextprotocol.registry/publisher-provided"?: Record<
    string,
    unknown
  >;
  [key: string]: unknown;
}

export interface RegistryServerEntry {
  name: string;
  title?: string;
  description?: string;
  version: string;
  status?: string;
  repository?: RegistryRepositoryInfo | null;
  websiteUrl?: string;
  icons?: ServerIcon[];
  remotes?: RegistryTransport[] | null;
  packages?: RegistryPackage[] | null;
  _meta?: RegistryServerMeta;
  extras?: Record<string, unknown> | null;
  $schema?: string;
}

export interface RegistryServerListResponse {
  servers: RegistryServerEntryWrapper[];
  metadata: {
    nextCursor?: string;
    count: number;
  };
}

export interface RegistryServerEntryWrapper {
  server: RegistryServerEntry;
  _meta?: RegistryServerMeta;
}

export interface ServersImportData {
  imported_count: number;
  imported_servers: string[];
  skipped_count: number;
  skipped_servers: SkippedServer[];
  failed_count: number;
  failed_servers: string[];
  error_details?: Record<string, string> | null;
  runtime_sync_error?: string | null;
}

export interface SkippedServer {
  name: string;
  reason: string;
  existing_query?: string | null;
  incoming_query?: string | null;
}

export interface ServerInstanceDetails {
  connection_attempts: number;
  last_connected_seconds?: number;
  tools_count: number;
  error_message?: string;
  server_type: string;
  process_id?: number;
  cpu_usage?: number;
  memory_usage?: number;
  last_health_check?: string;
}

export interface InspectorToolCallStartData {
  call_id: string;
  server_id: string;
  mode: "proxy" | "native";
  session_id?: string;
  request_id: string;
  progress_token: string;
}

export interface InspectorSessionOpenData {
  session_id: string;
  server_id: string;
  mode: "proxy" | "native";
  expires_at_epoch_ms: number;
}

export interface InspectorSessionCloseData {
  closed: boolean;
}

export interface InspectorToolCallCancelData {
  cancelled: boolean;
}

export type AuditCategory =
  | "mcp_request"
  | "server_config"
  | "profile_config"
  | "capability_control"
  | "management"
  | "client_config"
  | "runtime_control";

export type AuditStatus = "success" | "failed" | "cancelled";

export type AuditAction =
  | "initialize"
  | "tools_list"
  | "tools_call"
  | "resources_list"
  | "resources_read"
  | "prompts_list"
  | "prompts_get"
  | "notification_progress"
  | "notification_cancelled"
  | "logging_set_level"
  | "notification_message"
  | "server_create"
  | "server_import"
  | "server_update"
  | "server_delete"
  | "server_enable"
  | "server_disable"
  | "profile_create"
  | "profile_update"
  | "profile_delete"
  | "profile_activate"
  | "profile_deactivate"
  | "client_manage_enable"
  | "client_manage_disable"
  | "client_settings_update"
  | "client_config_apply"
  | "client_config_attach"
  | "client_config_detach"
  | "client_config_restore"
  | "client_config_import"
  | "client_capability_update"
  | "client_delete"
  | "client_approve"
  | "client_suspend"
  | "client_backup_delete"
  | "client_backup_policy_update"
  | "onboarding_policy_update"
  | "first_contact_behavior_update"
  | "core_source_apply"
  | "local_core_service_start"
  | "local_core_service_restart"
  | "local_core_service_stop"
  | "local_core_service_install"
  | "local_core_service_uninstall"
  | "desktop_managed_core_start"
  | "desktop_managed_core_restart"
  | "desktop_managed_core_stop"
  | "capability_grant"
  | "capability_revoke"
  | "surface_review_resolve"
  | "surface_publish"
  | "surface_rollback"
  | "surface_reconciliation"
  | "profile_server_enable"
  | "profile_server_disable"
  | "profile_server_remove"
  | "profile_server_replace"
  | "server_instance_disconnect"
  | "server_instance_force_disconnect"
  | "server_instance_reconnect"
  | "server_instance_reset_reconnect"
  | "server_instance_recover"
  | "server_instance_cancel"
  | "server_cache_reset"
  | "runtime_install"
  | "runtime_cache_reset"
  | "audit_policy_update"
  | "inspector_timeout_update";

export interface AuditEventRecord {
  id?: number | null;
  category: AuditCategory;
  action: AuditAction;
  status: AuditStatus;
  occurred_at_ms: number;
  actor?: string | null;
  request_id?: string | null;
  client_id?: string | null;
  client_name?: string | null;
  profile_id?: string | null;
  profile_name?: string | null;
  server_id?: string | null;
  server_name?: string | null;
  session_id?: string | null;
  protocol_version?: string | null;
  http_method?: string | null;
  route?: string | null;
  mcp_method?: string | null;
  target?: string | null;
  direction?: string | null;
  error_code?: string | null;
  error_message?: string | null;
  detail?: string | null;
  duration_ms?: number | null;
  data?: unknown;
  task_id?: string | null;
  related_task_id?: string | null;
  progress_token?: string | null;
}

export interface AuditListData {
  events: AuditEventRecord[];
  next_cursor?: string | null;
}

export interface AuditListResp {
  data?: AuditListData | null;
  error?: unknown | null;
  success: boolean;
}

export interface AuditEventDetailsData {
  event: AuditEventRecord;
}

export interface AuditEventDetailsResp {
  data?: AuditEventDetailsData | null;
  error?: unknown | null;
  success: boolean;
}

export type AuditRetentionPolicy =
  | "off"
  | { keep_days: { days: number } }
  | { keep_count: { count: number } }
  | { combined: { days: number; count: number } };

export interface AuditPolicyData {
  policy: AuditRetentionPolicy;
  sweep_interval_secs: number;
}

export interface AuditPolicyResp {
  data?: AuditPolicyData | null;
  error?: unknown | null;
  success: boolean;
}

export interface AuditPolicySetReq {
  policy: AuditRetentionPolicy;
  sweep_interval_secs?: number;
}

export type InspectorSseEvent =
  | {
      event: "started";
      call_id: string;
      server_id: string;
      mode: "proxy" | "native";
      session_id?: string;
      started_at_epoch_ms: number;
    }
  | {
      event: "progress";
      call_id: string;
      progress: number;
      total?: number;
      message?: string;
    }
  | {
      event: "log";
      call_id: string;
      level?: string;
      logger?: string;
      data: unknown;
    }
  | {
      event: "result";
      call_id: string;
      server_id: string;
      elapsed_ms: number;
      result: unknown;
    }
  | {
      event: "error";
      call_id: string;
      server_id: string;
      message: string;
    }
  | {
      event: "cancelled";
      call_id: string;
      server_id: string;
      reason?: string;
    };

export interface InstanceDetail {
  id: string;
  name: string;
  server_name: string;
  status: string;
  allowed_operations: string[];
  details: ServerInstanceDetails;
}

export interface InstanceHealth {
  id: string;
  name: string;
  healthy: boolean;
  message: string;
  status: string;
  checked_at: string;
  resource_metrics?: {
    cpu_usage?: number;
    memory_usage?: number;
    process_id?: number;
  };
  connection_stability?: number;
  details?: string;
}

export interface OperationResponse {
  id: string;
  name: string;
  result: string;
  status: string;
  allowed_operations: string[];
}

export interface Tool {
  tool_name: string;
  server_name: string;
  is_enabled: boolean;
  description?: string;
  tool_id?: string;
}

export interface ToolDetail extends Tool {
  configuration: Record<string, unknown>;
}

export interface SystemStatus {
  status: "running" | "degraded" | "stopped" | "error";
  uptime: number;
  version?: string;
  total_servers?: number;
  connected_servers?: number;
  active_mcp_servers?: number;
  aggregated_tools_count?: number;
}

export interface SystemMetrics {
  cpu_usage_percent?: number;
  cpu_usage?: number;
  memory_usage_bytes?: number;
  memory_usage?: number;
  memory_usage_percent?: number;
  active_connections?: number;
  total_requests_mcp?: number;
  error_rate_mcp?: number;
  uptime_seconds?: number;
  timestamp?: string;
  connected_servers_count?: number;
  total_instances_count?: number;
  ready_instances_count?: number;
  error_instances_count?: number;
  initializing_instances_count?: number;
  busy_instances_count?: number;
  shutdown_instances_count?: number;
  system_cpu_usage?: number;
  system_memory_usage?: number;
  system_memory_total?: number;
  system_memory_usage_percent?: number;
  total_tools_count?: number;
  unique_tools_count?: number;
  config_application_status?: string | null;
}

// API Response Status
export type ApiResponse<T> = {
  status: string;
  message?: string;
  data?: T;
};

// Notification Types
export interface ToolsChangedNotification {
  event: "tools_list_changed";
}

// UI Mode Type
export type UIMode = "wizard" | "expert";

// Theme Type
export type Theme = "light" | "dark" | "system";

// Configuration Types
export interface ConfigPreset {
  id: string;
  name: string;
  description?: string;
  created_at: string;
  updated_at: string;
  is_active: boolean;
  config: MCPConfig;
}

export interface MCPConfig {
  servers: MCPServerConfig[];
  tools: MCPToolConfig[];
  global_settings: GlobalSettings;
}

/**
 * MCP server configuration used by direct CRUD flows.
 *
 * Supported server types:
 * - "stdio": process-backed server
 * - "sse": legacy SSE HTTP server (persisted; protocol uses Streamable HTTP)
 * - "streamable_http": HTTP-based server
 */
export interface MCPServerConfig {
  /** Immutable MCPMate-managed server namespace. */
  name: string;

  /** Direct server type accepted by the backend CRUD API. */
  kind: "stdio" | "sse" | "streamable_http";

  /** 启动命令（stdio类型必填） */
  command?: string;

  /** 命令路径 */
  command_path?: string;

  /** HTTP endpoint URL (non-stdio servers) */
  url?: string;

  /** 命令参数 */
  args?: string[];

  /** 环境变量 */
  env?: Record<string, string>;

  /** HTTP headers for non-stdio servers */
  headers?: Record<string, string>;

  profile_ids?: string[];
  enabled?: boolean;
  unify_direct_exposure_eligible?: boolean;
  source?: ServerSource;

  /** Whether this server is a hidden pre-import record */
  pending_import?: boolean;

  /** 最大实例数 */
  max_instances?: number;

  /** 重试策略 */
  retry_policy?: RetryPolicy;

  /** 服务器元数据信息 */
  meta?: ServerMetaInfo;
}

export type SecretKind =
  | "generic"
  | "token"
  | "api_key"
  | "password"
  | "oauth_client_secret"
  | "oauth_access_token"
  | "oauth_refresh_token"
  | "url_credential"
  | "header_value";

export type SwitchableSecretStoreProviderMode =
  "operating_system" | "passphrase" | "local_file";

export type SecretStoreProviderMode =
  SwitchableSecretStoreProviderMode | "development" | "custom";

/** Known secure-store issue reason codes emitted by the backend. */
export const SECRET_STORE_REASON_CODES = [
  "passphrase_unlock_required",
  "provider_unavailable",
  "read_lock_failed",
  "database_unavailable",
  "schema_migration_required",
  "cache_error",
  "missing_root_key",
  "secret_key_mismatch",
  "invalid_root_key",
  "local_storage_error",
  "development_storage_error",
  "initialization_failed",
] as const;

export type SecretStoreReasonCode = (typeof SECRET_STORE_REASON_CODES)[number];

export type SecretStoreIssueReasonCode = SecretStoreReasonCode | "unknown";

export interface SecretMetadata {
  alias: string;
  placeholder: string;
  kind: SecretKind;
  label?: string | null;
  origin?: SecretOrigin | null;
  provider_id: string;
  provider_kind: string;
  version: number;
  used_by_count: number;
  historical_usage_count: number;
  unknown_usage_count?: number;
  created_at?: string | null;
  updated_at?: string | null;
}

export interface SecretOrigin {
  server_id?: string | null;
  server_name?: string | null;
  server_kind?: string | null;
  source?: string | null;
  field_group?: string | null;
  field_key?: string | null;
  field_index?: number | null;
  field_path?: string | null;
}

export interface SecretUsageLocation {
  [key: string]: unknown;
}

export type SecretUsageStatus = "active" | "stale";

export interface SecretUsage {
  alias: string;
  server_id: string;
  location: SecretUsageLocation | string;
  status?: SecretUsageStatus;
}

export interface SecretListResp {
  success: boolean;
  data?: { secrets: SecretMetadata[] } | null;
  error?: unknown;
}

export interface SecretMetadataResp {
  success: boolean;
  data?: SecretMetadata | null;
  error?: unknown;
}

export interface SecretUsageListResp {
  success: boolean;
  data?: { usages: SecretUsage[] } | null;
  error?: unknown;
}

export interface SecretDeleteResp {
  success: boolean;
  data?: { alias: string; deleted: boolean } | null;
  error?: unknown;
}

export interface SecretStoreProviderData {
  provider_id: string;
  provider_kind: string;
  provider_mode: SecretStoreProviderMode;
  security_level: string;
}

export interface SecretStoreIssueData {
  reason_code: SecretStoreReasonCode | (string & {});
  message: string;
}

export interface SecretStoreStatusData {
  status: string;
  provider?: SecretStoreProviderData | null;
  issue?: SecretStoreIssueData | null;
}

export interface SecretStoreStatusResp {
  success: boolean;
  data?: SecretStoreStatusData | null;
  error?: unknown;
}

export interface ProviderSwitchResp {
  success: boolean;
  data?: { new_status: SecretStoreStatusData } | null;
  error?: unknown;
}

export interface PasswordStatusData {
  enabled: boolean;
  scope: string[];
  has_password: boolean;
}

export interface PasswordStatusResp {
  success: boolean;
  data?: PasswordStatusData | null;
  error?: unknown;
}

export interface PasswordSetResp {
  success: boolean;
  data?: { enabled: boolean; scope: string[] } | null;
  error?: unknown;
}

export interface PasswordVerifyResp {
  success: boolean;
  data?: { valid: boolean } | null;
  error?: unknown;
}

export interface MCPToolConfig {
  name: string;
  server_name: string;
  is_enabled: boolean; // Keep this field name for consistency with API
  settings?: Record<string, unknown>;
}

export interface GlobalSettings {
  max_concurrent_connections: number;
  request_timeout_ms: number;
  enable_metrics: boolean;
  log_level: "debug" | "info" | "warn" | "error";
}

export interface RetryPolicy {
  max_attempts: number;
  initial_delay_ms: number;
  max_delay_ms: number;
  backoff_multiplier: number;
}

/**
 * 验证服务器类型格式
 *
 * @param kind 服务器类型字符串
 * @returns 是否为有效的标准格式
 */
export function validateServerType(
  kind: string,
): kind is "stdio" | "sse" | "streamable_http" {
  const validTypes = ["stdio", "sse", "streamable_http"] as const;
  return validTypes.some((type) => type === kind);
}

/**
 * 获取服务器类型格式错误提示
 *
 * @param invalidKind 无效的服务器类型
 * @returns 详细的错误提示信息
 */
export function getServerTypeErrorMessage(invalidKind: string): string {
  return `Invalid server type '${invalidKind}'.

Correct format requirements:
- Use "stdio" (not "Stdio" or other variants)
- Use "sse" or "streamable_http" (lowercase; not "http", "streamable-http", or "streamableHttp")`;
}

// Config Suits Types
export interface ConfigSuit {
  id: string;
  name: string;
  description?: string;
  suit_type: string;
  priority: number;
  is_active: boolean;
  is_default: boolean;
  authoring_generation: number;
  role?: string;
  allowed_operations: string[];
  profile_mode?: ProfileMode;
}

export type ProfileMode = "capability" | "workflow";

export interface ConfigSuitListResponse {
  suits: ConfigSuit[];
}

export interface ApiConflictDetails {
  currentAuthoringGeneration?: number;
  dependencyServerIds?: string[];
  packageFiles?: WorkflowGuidePackageFile[];
  capabilities?: WorkflowGuideCapability[];
  relativePaths?: string[];
}

export interface ProfileAuthoringView {
  profile: ConfigSuit;
  server_ids: string[];
  profile_mode?: ProfileMode;
  skill_name?: string | null;
  package_distribution?: "symlink" | "copy" | null;
}

export interface ProfileAuthoringSaveRequest {
  id: string | null;
  expected_authoring_generation: number | null;
  name: string;
  description: string | null;
  profile_type: string;
  priority: number;
  is_active: boolean;
  is_default: boolean;
  server_ids: string[];
  clone_from_id: string | null;
  profile_mode?: ProfileMode;
  skill_name?: string | null;
  package_distribution?: "symlink" | "copy" | null;
  workflow_guidance?: WorkflowGuidanceSaveRequest;
}

export interface WorkflowGuidanceSaveRequest {
  expected_specification_revision: number | null;
  validation_notes: string | null;
  avoid_rules: string | null;
}

export interface ProfileAuthoringSaveResponse {
  profile: ConfigSuit;
  profile_mode?: ProfileMode;
}

export type WorkflowBindingPolicy = "meta_on_demand" | "direct";
export type WorkflowBindingValidation = "valid" | "drifted" | "unavailable";

export interface WorkflowBinding {
  ref_id: string;
  binding_policy: WorkflowBindingPolicy;
}

export interface WorkflowStep {
  step_id: string;
  title: string;
  description: string | null;
  bindings: WorkflowBinding[];
}

export interface WorkflowSpecification {
  profile_id: string;
  specification_revision: number | null;
  validation_notes: string | null;
  avoid_rules: string | null;
  tool_binding_count: number;
  steps: WorkflowStep[];
}

export interface WorkflowBindingPreview extends WorkflowBinding {
  validation: WorkflowBindingValidation;
}

export interface WorkflowPreviewStep {
  title: string;
  description: string | null;
  bindings: WorkflowBindingPreview[];
}

export interface WorkflowSpecificationPreview {
  profile_id: string;
  specification_revision: number | null;
  validation_notes: string | null;
  avoid_rules: string | null;
  steps: WorkflowPreviewStep[];
  valid: boolean;
}

export type WorkflowGuidePackageCategory = "reference" | "script" | "asset";

export interface WorkflowGuideCapability {
  name: string;
  exposure: WorkflowBindingPolicy;
  guide: string;
  start_line: number;
  end_line: number;
}

export interface WorkflowGuidePackageFile {
  package_file_id: string;
  file_revision: number;
  title: string;
  category: WorkflowGuidePackageCategory;
  relative_path: string;
  mime_type: string | null;
  extension: string | null;
  file_size: number | null;
}

export interface WorkflowGuide {
  profile_id: string;
  guide_revision: number;
  markdown: string;
  capabilities: WorkflowGuideCapability[];
  package_files: WorkflowGuidePackageFile[];
  documents: WorkflowGuideExternalDocument[];
}

export interface WorkflowGuideSaveRequest {
  profile_id: string;
  expected_guide_revision: number;
  markdown: string;
  reclamation_confirmation?: WorkflowGuideReclamationConfirmation;
}

export interface WorkflowGuideReclamationConfirmation {
  package_files: Array<{
    package_file_id: string;
    file_revision: number;
  }>;
  capability_names: string[];
}

export interface WorkflowGuideSaveResponse {
  guide: WorkflowGuide;
  projected_skill: { markdown: string };
}

export interface WorkflowGuidePackageFileSaveResponse extends WorkflowGuideSaveResponse {
  package_file: WorkflowGuidePackageFile;
}

export interface WorkflowGuidePreviewRequest {
  profile_id: string;
  relative_path?: string;
  markdown: string;
}

export interface WorkflowGuidePreviewResponse {
  projected_skill: { markdown: string };
  active_document: { markdown: string };
  orphaned_package_files: WorkflowGuidePackageFile[];
  orphaned_capabilities: WorkflowGuideCapability[];
}

export interface WorkflowGuideExternalDocument {
  package_file_id: string;
  file_revision: number;
  title: string;
  relative_path: string;
  markdown: string;
}

export interface WorkflowGuidePackageFileDeleteRequest {
  profile_id: string;
  package_file_id: string;
  expected_file_revision: number;
  expected_guide_revision: number;
}

export interface ConfigSuitServer {
  id: string;
  name: string;
  enabled: boolean;
  allowed_operations: string[];
}

export interface ConfigSuitServersResponse {
  suit_id: string;
  suit_name: string;
  servers: ConfigSuitServer[];
  authoring_generation: number;
}

export interface ConfigSuitTool {
  id: string;
  server_id: string;
  server_name: string;
  tool_name: string;
  unique_name: string;
  description?: string | null;
  enabled: boolean;
  state: string;
  state_generation: number;
  allowed_operations: string[];
}

export interface ConfigSuitToolsResponse {
  suit_id: string;
  suit_name: string;
  tools: ConfigSuitTool[];
  source_revision_set: CatalogRevisionSet;
  authoring_generation: number;
}

export interface ConfigSuitResource {
  id: string;
  server_id: string;
  server_name: string;
  resource_uri: string;
  unique_uri: string;
  description?: string | null;
  enabled: boolean;
  state: string;
  state_generation: number;
  allowed_operations: string[];
}

export interface ConfigSuitResourcesResponse {
  suit_id: string;
  suit_name: string;
  resources: ConfigSuitResource[];
  source_revision_set: CatalogRevisionSet;
  authoring_generation: number;
}

export interface ConfigSuitResourceTemplate {
  id: string;
  server_id: string;
  server_name: string;
  uri_template: string;
  unique_uri_template: string;
  description?: string | null;
  enabled: boolean;
  state: string;
  state_generation: number;
  allowed_operations: string[];
}

export interface ConfigSuitResourceTemplatesResponse {
  suit_id: string;
  suit_name: string;
  templates: ConfigSuitResourceTemplate[];
  source_revision_set: CatalogRevisionSet;
  authoring_generation: number;
}

export interface ConfigSuitPrompt {
  id: string;
  server_id: string;
  server_name: string;
  prompt_name: string;
  unique_name: string;
  description?: string | null;
  enabled: boolean;
  state: string;
  state_generation: number;
  allowed_operations: string[];
}

export interface ConfigSuitPromptsResponse {
  suit_id: string;
  suit_name: string;
  prompts: ConfigSuitPrompt[];
  source_revision_set: CatalogRevisionSet;
  authoring_generation: number;
}

export interface BatchOperationRequest {
  ids: string[];
}

export interface BatchOperationResponse {
  success_count: number;
  successful_ids: string[];
  failed_ids: Record<string, string>;
}

// Runtime Types
export type RuntimeType = "uv" | "bun" | "node";

export interface RuntimeStatusItem {
  runtime_type: RuntimeType;
  available: boolean;
  path?: string | null;
  version?: string | null;
  /** Present when managed runtime is missing but a system PATH binary was detected */
  system_fallback_path?: string | null;
  message: string;
}

export interface RuntimeStatusResponse {
	/** Server process home directory; used to shorten paths with a tilde in the UI */
	user_home?: string | null;
	uv: RuntimeStatusItem;
	bun: RuntimeStatusItem;
	node: RuntimeStatusItem;
}

export interface RuntimeCacheItem {
  path: string;
  size_bytes: number;
  package_count: number;
  last_modified?: string | null;
}

export interface RuntimeCacheSummaryInfo {
  total_size_bytes: number;
  last_cleanup?: string | null;
}

export interface RuntimeCacheResponse {
  summary: RuntimeCacheSummaryInfo;
  uv: RuntimeCacheItem;
  bun: RuntimeCacheItem;
  node: RuntimeCacheItem;
}

export interface InstallRuntimeRequest {
  runtime_type: RuntimeType; // "uv" | "bun" | "node"
  version?: string;
  timeout?: number;
  max_retries?: number;
  verbose?: boolean;
  interactive?: boolean;
}

export interface InstallResponse {
  success: boolean;
  message: string;
  runtime_type: RuntimeType;
}

export interface ClearCacheResponse {
  success: boolean;
}

// Capabilities Cache Types
export interface CapabilitiesCatalogStats {
  snapshots: number;
  readySnapshots: number;
  invalidatedSnapshots: number;
  unavailableSnapshots: number;
  records: number;
  tools: number;
  resources: number;
  prompts: number;
  resourceTemplates: number;
}

export interface CapabilitiesMemoryStats {
  rawSnapshotEntries: number;
  projectionEntries: number;
}

export interface CapabilitiesStorageStats {
  catalog: CapabilitiesCatalogStats;
  memory: CapabilitiesMemoryStats;
}

export interface CapabilitiesMetricsStats {
  totalQueries: number;
  cacheHits: number;
  cacheMisses: number;
  hitRatio: number;
  rawSnapshotHits: number;
  rawSnapshotMisses: number;
  projectionHits: number;
  projectionMisses: number;
  singleFlightWaits: number;
  evictions: number;
  cacheInvalidations: number;
}

export interface CapabilitiesStatsResponse {
  storage: CapabilitiesStorageStats;
  metrics: CapabilitiesMetricsStats;
  generatedAt: string;
}

export interface CapabilitiesKeyItem {
  cache: string;
  keyHash: string;
  approxValueSizeBytes: number;
  cachedAt?: string;
}

export interface CapabilitiesKeysResponse {
  keys: CapabilitiesKeyItem[];
  total: number;
}

// --------------------
// OpenAPI Wrapped Responses (RPC-style)
// --------------------

export interface ServerListResp {
  data?: { servers: ServerDetail[] } | null;
  error?: unknown | null;
  success: boolean;
}

export interface ServerDetailsResp {
  data?: ServerDetail | null;
  error?: unknown | null;
  success: boolean;
}

export interface InstanceListResp {
  data?: { name: string; instances: InstanceSummary[] } | null;
  error?: unknown | null;
  success: boolean;
}

export interface InstanceDetailsResp {
  data?: InstanceDetail | null;
  error?: unknown | null;
  success: boolean;
}

export interface InstanceHealthResp {
  data?: InstanceHealth | null;
  error?: unknown | null;
  success: boolean;
}

export interface OperationResponseResp {
  data?: OperationResponse | null;
  error?: unknown | null;
  success: boolean;
}

// Server capabilities (per-server) minimal typings
export interface ServerCapabilityMeta {
  cache_hit: boolean;
  source: string;
  strategy: string;
}

export interface ServerCapabilityList<T = unknown> {
  items: T[];
  state: string;
  meta: ServerCapabilityMeta;
}

export interface ServerCapabilityResp<T = unknown> {
  data?: ServerCapabilityList<T> | null;
  error?: unknown | null;
  success: boolean;
}

// --------------------
// Clients (Host applications) Types
// --------------------

export type ClientCategory = "editor" | "terminal" | "browser" | string;

export type ClientConfigType = "json" | "yaml" | "toml" | string;

export interface ClientTemplateStorageMetadata {
  kind: string;
  path_strategy?: string | null;
}

export interface ClientTemplateMetadata {
  format: string;
  container_type: ClientConfigType;
  merge_strategy: string;
  keep_original_config: boolean;
  storage: ClientTemplateStorageMetadata;
  description?: string | null;
  homepage_url?: string | null;
  docs_url?: string | null;
  support_url?: string | null;
  protocol_revision?: string | null;
  managed_source?: string | null;
  // Legacy fields that may still surface from cached payloads
  name?: string | null;
  version?: string | null;
}

export interface ClientInfo {
  identifier: string;
  display_name: string;
  config_file_state?: ClientConfigFileState | null;
  registration_origin?: ClientRegistrationOrigin | null;
  runtime_observed?: boolean | null;
  category: ClientCategory;
  enabled: boolean;
  detected: boolean;
  config_path: string;
  config_exists: boolean;
  has_mcp_config: boolean;
  approval_status?: "approved" | "pending" | "suspended" | string | null;
  attachment_state?: "attached" | "detached" | "not_applicable" | string | null;
  writable_config?: boolean | null;
  governed_by_default_policy?: boolean | null;
  description?: string | null;
  homepage_url?: string | null;
  docs_url?: string | null;
  support_url?: string | null;
  config_type?: ClientConfigType | null;
  /** Resolved rules used for parsing (template default or legacy row). */
  config_file_parse_effective?: ClientConfigFileParse | null;
  /** User override when set; when absent, uses effective/template. */
  config_file_parse_override?: ClientConfigFileParse | null;
  /** True when no stored override exists (template/runtime default). */
  uses_template_parse_default?: boolean;
  config_mode?: string | null;
  transport?: string | null;
  client_version?: string | null;
  install_path?: string | null;
  logo_url?: string | null;
  last_detected?: string | null;
  last_modified?: string | null;
  capability_source?: CapabilitySource;
  selected_profile_ids?: string[];
  custom_profile_id?: string | null;
  custom_profile_missing?: boolean;
  template: ClientTemplateMetadata;
  mcp_servers_count?: number | null;
  transports?: Record<string, TransportRuleData> | null;
}

export interface ClientCheckData {
  client: ClientInfo[];
  total: number;
  last_updated: string;
}

export interface ClientCheckResp {
  data?: ClientCheckData | null;
  error?: unknown | null;
  success: boolean;
}

export interface ClientRecordLifecycleResp {
  identifier: string;
  status: "approved" | "pending" | "suspended" | string;
}

export interface ClientDetachResp {
  data?: {
    identifier: string;
    attachment_state: "detached" | string;
    changed: boolean;
  } | null;
  error?: unknown | null;
  success: boolean;
}

export interface ClientAttachResp {
  data?: {
    identifier: string;
    attachment_state: "attached" | string;
    changed: boolean;
  } | null;
  error?: unknown | null;
  success: boolean;
}

export interface ClientDeleteResp {
  data?: {
    identifier: string;
    deleted: boolean;
  } | null;
  error?: unknown | null;
  success: boolean;
}

export interface ClientObserveReq {
  identifier: string;
  display_name?: string | null;
}

export interface ClientRecordReviewReq {
  identifier: string;
}

export type ClientConfigFileState = "with_config_file" | "without_config_file";
export type ClientRegistrationOrigin =
  "manual" | "config_detection" | "runtime_initialize" | string;

export interface DefaultClientPolicyData {
  config_mode: string;
  capability_source: CapabilitySource;
  first_contact_behavior: "deny" | "review" | "allow" | string;
}

export interface DefaultClientPolicyResp {
  data?: DefaultClientPolicyData | null;
  error?: unknown | null;
  success: boolean;
}

// Client config details
export type ClientConfigMode = "unify" | "hosted" | "transparent" | string;
export type ClientCapabilitySourceSelection =
  "default" | "profile" | "custom" | string;
export type ClientConfigSelected =
  | "default"
  | { profile: { profile_id: string } }
  | { servers: { server_ids: string[] } }
  | string;

export interface ServerEntryData {
  name: string;
  transport: string;
  import_status?: "importable" | "skipped" | string;
  skip_reason?: string | null;
  issue?: string | null;
  managed_by_mcpmate?: boolean;
  command?: string | null;
  args: string[];
  env: Record<string, string>;
  headers: Record<string, string>;
  url?: string | null;
}

export type ClientMergeStrategy = "deep_merge" | "replace";

export interface ClientConfigData {
  config_exists: boolean;
  config_path: string;
  config_mode?: ClientConfigMode | null;
  config_type?: ClientConfigType | null;
  config_file_parse_effective?: ClientConfigFileParse | null;
  config_file_parse_override?: ClientConfigFileParse | null;
  uses_template_parse_default?: boolean;
  content: unknown;
  warnings?: string[];
  degraded_reasons?: string[];
  has_mcp_config: boolean;
  configured_server_entries?: ServerEntryData[];
  last_modified?: string | null;
  config_file_state?: ClientConfigFileState | null;
  registration_origin?: ClientRegistrationOrigin | null;
  runtime_observed?: boolean | null;
  mcp_servers_count: number;
  approval_status?: "approved" | "pending" | "suspended" | string | null;
  attachment_state?: "attached" | "detached" | "not_applicable" | string | null;
  writable_config?: boolean | null;
  governed_by_default_policy?: boolean | null;
  template_merge_strategy: ClientMergeStrategy;
  merge_strategy_override?: ClientMergeStrategy | null;
  system_merge_strategy_override?: ClientMergeStrategy | null;
  effective_merge_strategy: ClientMergeStrategy;
  merge_strategy_source:
    | "template"
    | "system_default"
    | "system_override"
    | "client_override";
  supported_merge_strategies: ClientMergeStrategy[];
  template: ClientTemplateMetadata;
  description?: string | null;
  homepage_url?: string | null;
  docs_url?: string | null;
  support_url?: string | null;
  logo_url?: string | null;
  capability_source?: CapabilitySource;
  selected_profile_ids?: string[];
  custom_profile_id?: string | null;
  custom_profile_missing?: boolean;
  transports?: Record<string, TransportRuleData> | null;
}

export interface ClientSettingsSourceData {
  display_name: "provided" | "stored" | "default" | string;
  approval_status: "provided" | "stored" | "default" | string;
  config_file_state: "provided" | "derived" | "stored" | string;
}

export interface ClientSettingsUpdateData {
  created: boolean;
  identifier: string;
  display_name: string;
  transport: string;
  client_version?: string | null;
  config_file_state?: ClientConfigFileState | null;
  registration_origin?: ClientRegistrationOrigin | null;
  runtime_observed?: boolean | null;
  config_path?: string | null;
  description?: string | null;
  homepage_url?: string | null;
  docs_url?: string | null;
  support_url?: string | null;
  logo_url?: string | null;
  template_merge_strategy: ClientMergeStrategy;
  merge_strategy_override?: ClientMergeStrategy | null;
  system_merge_strategy_override?: ClientMergeStrategy | null;
  effective_merge_strategy: ClientMergeStrategy;
  merge_strategy_source:
    | "template"
    | "system_default"
    | "system_override"
    | "client_override";
  supported_merge_strategies: ClientMergeStrategy[];
  setting_sources?: ClientSettingsSourceData | null;
  transports?: Record<string, TransportRuleData> | null;
}

export interface ClientSettingsUpdateResp {
  data?: ClientSettingsUpdateData | null;
  error?: unknown | null;
  success: boolean;
}

export interface ClientConfigResp {
  data?: ClientConfigData | null;
  error?: unknown | null;
  success: boolean;
}

export type CapabilitySource = "activated" | "profiles" | "custom";

export interface UnifyDirectCapabilityRefs {
  tool_refs?: string[];
  prompt_refs?: string[];
  resource_refs?: string[];
  template_refs?: string[];
}

export interface UnifyDirectExposureConfig {
  route_mode: "broker_only" | "server_level" | "capability_level";
  server_ids?: string[];
  capability_refs?: UnifyDirectCapabilityRefs;
  diagnostics?: {
    invalid_server_ids?: string[];
    invalid_capability_refs?: string[];
    invalid_tool_surfaces?: {
      server_id: string;
      tool_name: string;
      reason: string;
    }[];
    invalid_prompt_surfaces?: {
      server_id: string;
      prompt_name: string;
      reason: string;
    }[];
    invalid_resource_surfaces?: {
      server_id: string;
      resource_uri: string;
      reason: string;
    }[];
    invalid_template_surfaces?: {
      server_id: string;
      uri_template: string;
      reason: string;
    }[];
  };
  resolved_capabilities?: {
    route_mode: "broker_only" | "server_level" | "capability_level";
    selected_server_ids?: string[];
    selected_tool_surfaces?: { server_id: string; tool_name: string }[];
    selected_prompt_surfaces?: { server_id: string; prompt_name: string }[];
    selected_resource_surfaces?: { server_id: string; resource_uri: string }[];
    selected_template_surfaces?: { server_id: string; uri_template: string }[];
  };
}

export interface ClientCapabilityConfigData {
  identifier: string;
  capability_source: CapabilitySource;
  selected_profile_ids: string[];
  custom_profile_id?: string | null;
  custom_profile_missing?: boolean;
  unify_direct_exposure?: UnifyDirectExposureConfig | null;
  source_revision_set: CatalogRevisionSet;
}

export interface ClientCapabilityConfigResp {
  data?: ClientCapabilityConfigData | null;
  error?: unknown | null;
  success: boolean;
}

export interface ClientCapabilityConfigReq {
  identifier: string;
  config_mode?: string;
  capability_source: CapabilitySource;
  selected_profile_ids?: string[];
  unify_direct_exposure?: UnifyDirectExposureConfig | null;
  source_revision_set: CatalogRevisionSet;
}

export type CatalogRevisionSet = Record<string, number>;

export type SurfaceReviewLifecycle = "pending" | "resolved" | "obsolete";

export type SurfaceReviewOwnerType =
  | "standard_profile"
  | "custom_profile"
  | "consumer_direct_exposure"
  | "profile_server_exposure"
  | "consumer_server_exposure"
  | "mode_rule";

export type SurfaceReviewResolutionAction =
  | "approve_target"
  | "reject_target"
  | "keep_intent"
  | "remove_intent"
  | "rebind_ref";

export interface SurfaceReviewOwner {
  owner_type: SurfaceReviewOwnerType;
  owner_id: string;
}

export interface SurfaceReviewDecision {
  decision_id: string;
  resolution_action: SurfaceReviewResolutionAction;
  resolution_payload?: unknown | null;
  actor: string;
  decided_at: string;
  supersedes_decision_id?: string | null;
}

export interface SurfaceReviewFieldDiff {
  path: string;
  before?: unknown | null;
  target?: unknown | null;
}

export interface SurfaceReviewItem {
  review_item_id: string;
  proposal_id: string;
  consumer_id: string;
  binding_generation: number | null;
  ref_id: string;
  before_capability_id?: string | null;
  target_capability_id?: string | null;
  target_key: string;
  change_class: string;
  policy_action: string;
  lifecycle: SurfaceReviewLifecycle;
  owners: SurfaceReviewOwner[];
  current_decision?: SurfaceReviewDecision | null;
  before_record?: unknown | null;
  target_record?: unknown | null;
  field_diff: SurfaceReviewFieldDiff[];
  created_at: string;
  updated_at: string;
}

export interface SurfaceReviewSummaryEntry {
  owner: SurfaceReviewOwner;
  pending_count: number;
  earliest_created_at: string;
  change_classes: Record<string, number>;
}

export interface SurfaceReviewSummary {
  pending_count: number;
  failed_reconciliation_count: number;
  entries: SurfaceReviewSummaryEntry[];
}

export interface SurfaceReviewActionReq {
  expected_target_key: string;
  expected_binding_generation: number;
  actor: string;
}

export interface SurfaceReviewActionData {
  review_item_id: string;
  decision_id: string;
  resolution_action: SurfaceReviewResolutionAction;
  binding_generation: number;
  effective_surface_changed: boolean;
}

export type SurfaceIntentResolutionAction =
  | "keep_intent"
  | "remove_intent"
  | "rebind_ref";

export interface SurfaceIntentPreviewReq {
  action: SurfaceIntentResolutionAction;
  owner?: SurfaceReviewOwner | null;
  new_ref_id?: string | null;
}

export interface SurfaceIntentPreviewData {
  review_item_id: string;
  action: SurfaceIntentResolutionAction;
  owner?: SurfaceReviewOwner | null;
  owner_revision: string;
  impacted_consumer_ids: string[];
  impact_token: string;
}

export interface SurfaceIntentResolveReq extends SurfaceIntentPreviewReq {
  expected_owner_revision: string;
  impact_token: string;
  expected_target_key: string;
  expected_binding_generation: number;
  actor: string;
}

export interface ClientConfigUpdateReq {
  identifier: string;
  mode?: ClientConfigMode;
  preview?: boolean;
  selected_config?: ClientConfigSelected;
  backup_policy?: ClientBackupPolicyPayload;
}

export interface ClientConfigUpdateData {
  success: boolean;
  preview: Record<string, unknown> | null;
  applied: boolean;
  backup_path?: string | null;
  warnings: string[];
  diff_format?: string | null;
  diff_before?: string | null;
  diff_after?: string | null;
  scheduled?: boolean | null;
  scheduled_reason?: string | null;
}

export interface ClientConfigUpdateResp {
  data?: ClientConfigUpdateData | null;
  error?: unknown | null;
  success: boolean;
}

export interface ClientConfigRestoreReq {
  identifier: string;
  backup: string;
}

// Backups
export interface ClientBackupEntry {
  identifier: string;
  backup: string;
  path: string;
  size: number;
  created_at?: string | null;
}

export interface ClientBackupListData {
  backups: ClientBackupEntry[];
}

export interface ClientBackupListResp {
  data?: ClientBackupListData | null;
  error?: unknown | null;
  success: boolean;
}

export interface ClientBackupActionResp {
  data?: { identifier: string; backup: string; message: string } | null;
  error?: unknown | null;
  success: boolean;
}

// Backup policy
export interface ClientBackupPolicyData {
  identifier: string;
  policy: string; // e.g., 'keep_n'
  limit?: number | null;
}

export interface ClientBackupPolicyResp {
  data?: ClientBackupPolicyData | null;
  error?: unknown | null;
  success: boolean;
}

export interface ClientBackupPolicyPayload {
  policy: string;
  limit?: number | null;
}

export interface ClientBackupPolicySetReq {
	identifier: string;
	policy: ClientBackupPolicyPayload;
}

// Token Estimation Types
export interface CapTypeEstimate {
	available_count: number;
	visible_count: number;
	disabled_count: number;
	available_tokens: number;
	visible_tokens: number;
	savings_tokens: number;
}

export interface TokenBreakdownResponse {
	tools: CapTypeEstimate;
	prompts: CapTypeEstimate;
	resources: CapTypeEstimate;
	templates: CapTypeEstimate;
}

export interface TokenEstimateResponse {
	total_available_tokens: number;
	visible_tokens: number;
	savings_tokens: number;
	breakdown: TokenBreakdownResponse;
	estimation_method: string;
	approximate: boolean;
}

/** Per-row payloads for client-side tokenizer (profile detail header). */
export interface CapabilityTokenLedgerRow {
	profile_row_id: string;
	kind: string;
	server_id: string;
	server_enabled_in_profile: boolean;
	payload_json: string;
}

export interface CapabilityTokenLedgerResponse {
	items: CapabilityTokenLedgerRow[];
	tokenizer_note: string;
}

export interface OAuthStatus {
  server_id: string;
  configured: boolean;
  state?:
    "not_configured" | "disconnected" | "connected" | "expired" | string | null;
  authorization_endpoint?: string | null;
  token_endpoint?: string | null;
  client_id?: string | null;
  scopes?: string | null;
  redirect_uri?: string | null;
  has_client_secret?: boolean | null;
  manual_authorization_override?: boolean | null;
  expires_at?: string | null;
  custody_state?:
    "missing" | "secure" | "legacy_plaintext" | "unavailable" | string | null;
  requires_reconnect?: boolean | null;
  issue?: {
    code: string;
    message: string;
  } | null;
}

export interface OAuthConfigRequest {
  server_id?: string;
  authorization_endpoint: string;
  token_endpoint: string;
  client_id: string;
  client_secret?: string;
  scopes?: string;
  redirect_uri: string;
}

export interface OAuthInitiateResponse {
  server_id: string;
  authorization_url: string;
  state: string;
}

export type OAuthCallbackAccessKind = "web" | "desktop_loopback";

export interface OAuthCallbackAccessContract {
  kind: OAuthCallbackAccessKind;
  redirect_uri: string;
}

export interface OAuthCallbackNotificationPayload {
  type?: "OAUTH_CALLBACK_SUCCESS" | "OAUTH_CALLBACK_ERROR" | string;
  serverId?: string;
  error?: string;
  timestamp?: number;
}

export interface OAuthCallbackRequest {
  code: string;
  state: string;
}

export interface ClientConfigFileParse {
  format: string;
  container_type: string;
  container_keys?: string[];
}

export interface TransportRuleData {
  command_field?: string | null;
  args_field?: string | null;
  env_field?: string | null;
  include_type?: boolean;
  type_value?: string | null;
  url_field?: string | null;
  headers_field?: string | null;
  extra_fields?: Record<string, unknown> | null;
  selected?: boolean | null;
}

interface ClientConfigFileParseDraftPayload {
  config_file_parse?: ClientConfigFileParse;
}

export interface ClientConfigFileParseInspectReq extends ClientConfigFileParseDraftPayload {
  config_path: string;
}

export interface ClientConfigFileParseInspectExistingReq extends ClientConfigFileParseDraftPayload {
  identifier: string;
}

/** Discriminated request for `clientsApi.inspectClientConfigFileParse` (not sent to the server). */
export type ClientConfigFileParseInspectCall =
  | (ClientConfigFileParseInspectReq & { inspectTarget: "path" })
  | (ClientConfigFileParseInspectExistingReq & { inspectTarget: "existing" });

export interface ClientConfigFileParseValidation {
  matches: boolean;
  format_matches: boolean;
  container_found: boolean;
  server_count: number;
}

export interface ClientConfigFileParseInspectResp {
  normalized_path: string;
  detected_format?: string | null;
  inferred_parse?: ClientConfigFileParse | null;
  validation?: ClientConfigFileParseValidation | null;
  preview?: unknown;
}

// LLM Provider Types
export interface LlmProviderDefaultParams {
  temperature: number;
  max_tokens: number;
  thinking: LlmProviderThinkingConfig;
}

export type LlmProviderThinkingMode = "default" | "disabled" | "enabled";

export interface LlmProviderThinkingConfig {
  mode: LlmProviderThinkingMode;
  budget_tokens?: number | null;
}

export interface LlmProviderConfig {
  id: string;
  name: string;
  provider_type:
    "openai_chat" | "openai_responses" | "anthropic" | "openai_compatible";
  base_url: string;
  model_id: string;
  has_api_key: boolean;
  is_default: boolean;
  default_params: LlmProviderDefaultParams;
  created_at?: string | null;
  updated_at?: string | null;
}

export interface LlmProviderCreateInput {
  name: string;
  provider_type:
    "openai_chat" | "openai_responses" | "anthropic" | "openai_compatible";
  base_url: string;
  model_id: string;
  api_key?: string;
  default_params?: {
    temperature?: number;
    max_tokens?: number;
    thinking?: LlmProviderThinkingConfig;
  };
}

export interface LlmProviderModelPreviewInput {
  provider_id?: string;
  provider_type:
    "openai_chat" | "openai_responses" | "anthropic" | "openai_compatible";
  base_url: string;
  model_id: string;
  api_key?: string;
}

export interface LlmProviderUpdateInput {
  id: string;
  name?: string;
  provider_type?: string;
  base_url?: string;
  model_id?: string;
  api_key?: string | null;
  default_params?: {
    temperature?: number;
    max_tokens?: number;
    thinking?: LlmProviderThinkingConfig;
  };
}

export interface LlmConnectivityResult {
  success: boolean;
  latency_ms: number;
  model: string;
  error?: string | null;
}
