import type {
	ApiResponse,
	AuditEventDetailsResp,
	AuditListResp,
	AuditPolicyResp,
	AuditPolicySetReq,
	CapabilitiesKeysResponse,
	CapabilitiesMetricsStats,
	CapabilitiesStatsResponse,
	CapabilitiesStorageStats,
	CapabilityKindSummary,
	CatalogRevisionSet,
	ClearCacheResponse,
	ClientBackupActionResp,
	ClientBackupListResp,
	ClientBackupPolicyResp,
	ClientBackupPolicySetReq,
	ClientCapabilityConfigReq,
	ClientCapabilityConfigResp,
	ClientCheckResp,
	ClientConfigFileParse,
	ClientConfigFileParseInspectCall,
	ClientConfigFileParseInspectResp,
	ClientConfigFileState,
	ClientConfigResp,
	ClientConfigRestoreReq,
	ClientConfigUpdateReq,
	ClientConfigUpdateResp,
	ClientSettingsUpdateResp,
	ClientMergeStrategy,
	ClientAttachResp,
	ClientDeleteResp,
	ClientDetachResp,
	ClientRecordLifecycleResp,
	ClientRecordReviewReq,
	TransportRuleData,
	ConfigPreset,
	ConfigSuit,
	ConfigSuitListResponse,
	ConfigSuitPrompt,
	ConfigSuitPromptsResponse,
	ConfigSuitResource,
	ConfigSuitResourceTemplate,
	ConfigSuitResourceTemplatesResponse,
	ConfigSuitResourcesResponse,
	ConfigSuitServer,
	ConfigSuitServersResponse,
	ConfigSuitTool,
	ConfigSuitToolsResponse,
	ProfileMode,
	WorkflowSpecification,
	WorkflowSpecificationPreview,
	WorkflowGuide,
	WorkflowGuideExternalDocument,
	WorkflowGuidePreviewRequest,
	WorkflowGuidePreviewResponse,
	WorkflowGuideSaveRequest,
	WorkflowGuideSaveResponse,
	WorkflowGuidePackageFileSaveResponse,
	WorkflowGuidePackageFileDeleteRequest,
	InspectorSessionCloseData,
	InspectorSessionOpenData,
	InspectorToolCallCancelData,
	InspectorToolCallStartData,
	InstallResponse,
	InstallRuntimeRequest,
	InstanceDetail,
	InstanceDetailsResp,
	InstanceHealth,
	InstanceHealthResp,
	InstanceListResp,
	InstanceSummary,
	MCPConfig,
	MCPServerConfig,
	OperationResponseResp,
	RegistryMetaPayload,
	RegistryRepositoryInfo,
	RuntimeCacheResponse,
	RuntimeStatusResponse,
	ServerCapabilitySummary,
	ServerDetail,
	ServerDetailsResp,
	ServerIcon,
	ServerListResp,
	ServerListResponse,
	ServerMetaInfo,
	ServerNamespaceIssue,
	ServerSource,
	ServerTransportValidity,
	SurfaceIntentPreviewData,
	SurfaceIntentPreviewReq,
	SurfaceIntentResolveReq,
	SurfaceReviewActionData,
	SurfaceReviewActionReq,
	SurfaceReviewItem,
	SurfaceReviewLifecycle,
	SurfaceReviewOwnerType,
	SurfaceReviewSummary,
  StandardServerInfo,
	SecretDeleteResp,
	SecretKind,
	SecretListResp,
	SecretMetadata,
	SecretMetadataResp,
	SecretOrigin,
	SecretStoreStatusData,
	SecretStoreStatusResp,
	SwitchableSecretStoreProviderMode,
	ProviderSwitchResp,
	PasswordStatusResp,
	PasswordSetResp,
	PasswordVerifyResp,
	ProfileAuthoringSaveRequest,
	ProfileAuthoringSaveResponse,
	ProfileAuthoringView,
	ApiConflictDetails,
	SecretUsage,
	SecretUsageListResp,
	OAuthCallbackRequest,
	OAuthConfigRequest,
	OAuthInitiateResponse,
	OAuthStatus,
	ServerSummary,
	ServersImportData,
	SkippedServer,
	SystemMetrics,
	SystemStatus,
	CapabilityTokenLedgerResponse,
	TokenEstimateResponse,
	ToolDetail,
	LlmProviderConfig,
	LlmProviderCreateInput,
	LlmProviderModelPreviewInput,
	LlmProviderUpdateInput,
	LlmConnectivityResult,
} from "./types";
import { assertServerCrudUpdate } from "./server-update-contract";
import { useMemo } from "react";
import { isTauriEnvironmentSync } from "./platform";
import { fetchCatalogEntryForSource } from "./market/refresh";
import { useQuery } from "@tanstack/react-query";
import { handleDemoApiRequest } from "./demo-api";
import { isBoardDemoMode } from "./demo-mode";

// Base API configuration
// Prefer VITE_API_BASE_URL; otherwise infer from runtime context with sane fallbacks.
// Persist a single frontend bridge source (`apiBaseUrl`) when runtime port discovery needs to
// outlive a page reload (desktop shell or localhost browser workflows).
const API_BASE_OVERRIDE_KEY = "mcpmate.api_base_override";
export const API_BASE_CHANGED_EVENT = "mcpmate:api-base-changed";

const readEnvApiBaseUrl = (): string | null => {
	const envBase =
    typeof import.meta !== "undefined"
      ? import.meta.env?.VITE_API_BASE_URL
      : undefined;
	if (typeof envBase !== "string") {
		return null;
	}
	const trimmed = envBase.trim();
	return trimmed.length > 0 ? trimmed : null;
};

const isDesktopShellProtocol = (protocol: string): boolean =>
	protocol === "tauri:" || protocol === "app:" || protocol === "file:";

const readStoredApiBaseUrl = (): string | null => {
	if (typeof window === "undefined") {
		return null;
	}
	try {
		const stored = window.localStorage?.getItem(API_BASE_OVERRIDE_KEY)?.trim();
		return stored ? stored : null;
	} catch {
		return null;
	}
};

const canReuseStoredApiBaseUrl = (): boolean => {
	if (typeof window === "undefined" || typeof window.location === "undefined") {
		return false;
	}

	const protocol = window.location.protocol.toLowerCase();
	return isDesktopShellProtocol(protocol);
};

const resolveApiBaseUrl = (): string => {
	const envBase = readEnvApiBaseUrl();
	if (envBase) {
		return envBase;
	}

	const storedBase = readStoredApiBaseUrl();
	if (storedBase && canReuseStoredApiBaseUrl()) {
		return storedBase;
	}

	if (typeof window !== "undefined" && typeof window.location !== "undefined") {
		const protocol = window.location.protocol.toLowerCase();

		if (isDesktopShellProtocol(protocol)) {
			return "http://127.0.0.1:8080";
		}

		return "";
	}

	return "http://127.0.0.1:8080";
};

export let API_BASE_URL = resolveApiBaseUrl();

export function requireApiBaseUrl(context: string): string {
	const trimmed = API_BASE_URL.trim();
	if (trimmed.length > 0) {
		return trimmed;
	}
	throw new Error(`Missing MCPMate API base URL for ${context}`);
}

export function resolveApiUrl(path: string): string {
	const trimmed = API_BASE_URL.trim();
	if (trimmed.length === 0) {
		return path;
	}

	try {
		return new URL(path, trimmed).toString();
	} catch {
		return path;
	}
}

export function setApiBaseUrl(newBase: string | null | undefined) {
	const candidate = (newBase ?? "").trim();
	if (candidate.length > 0) {
		API_BASE_URL = candidate;
		try {
			if (isTauriEnvironmentSync()) {
				window.localStorage?.setItem(API_BASE_OVERRIDE_KEY, candidate);
			} else {
				window.localStorage?.removeItem(API_BASE_OVERRIDE_KEY);
			}
			window.dispatchEvent(
        new CustomEvent(API_BASE_CHANGED_EVENT, {
          detail: { apiBaseUrl: candidate },
        }),
			);
		} catch {
			// ignore persistence errors
		}
		return;
	}
	try {
		window.localStorage?.removeItem(API_BASE_OVERRIDE_KEY);
	} catch {
		// LocalStorage access may fail in restricted environments
	}
	API_BASE_URL = resolveApiBaseUrl();
	try {
		window.dispatchEvent(
			new CustomEvent(API_BASE_CHANGED_EVENT, {
				detail: { apiBaseUrl: API_BASE_URL },
			}),
		);
	} catch {
		// ignore event dispatch errors
	}
}

export function syncApiBaseUrlForRuntimePort(apiPort: number) {
	if (readEnvApiBaseUrl()) {
		return;
	}

	if (!isTauriEnvironmentSync()) {
		return;
	}

	setApiBaseUrl(`http://127.0.0.1:${apiPort}`);
}

export const resolveWebSocketUrl = (): string => {
	if (typeof window === "undefined") {
		return "";
	}

	const baseCandidate = API_BASE_URL || window.location.origin;

	try {
		const parsed = new URL(baseCandidate);
		const protocol = parsed.protocol === "https:" ? "wss:" : "ws:";
		const host = parsed.host || parsed.hostname;
		return `${protocol}//${host}/ws`;
	} catch (error) {
		console.warn(
			"Failed to derive WebSocket URL from API base, falling back to window location",
			error,
		);
		const fallbackProtocol =
			window.location.protocol === "https:" ? "wss:" : "ws:";
		return `${fallbackProtocol}//${window.location.host}/ws`;
	}
};

// Utility types
interface ApiWrapper<T> {
	success: boolean;
	data?: T;
	error?: unknown;
}

interface SuitTool {
	id: string;
	name?: string;
	tool_name?: string;
  unique_name?: string;
	server_name: string;
	description?: string;
	enabled?: boolean;
	is_enabled?: boolean;
}

/** Tools/resources/prompts list payloads from server capability endpoints */
export type CapabilityListResult = {
	items: unknown[];
	meta?: unknown;
	state?: string;
	degraded_reason?: string;
};

export type CapabilityAuthenticationCode =
	| "auth_required"
	| "unauthorized"
	| "forbidden"
	| "insufficient_scope";

export type CapabilityAuthenticationFailure = {
	code: CapabilityAuthenticationCode;
	reason: string;
};

export type CapabilityBatchLists = {
	tools: CapabilityListResult;
	resources: CapabilityListResult;
	prompts: CapabilityListResult;
	templates: CapabilityListResult;
	authentication?: CapabilityAuthenticationFailure | null;
};

type CapabilityBatchKind = "tools" | "resources" | "prompts" | "templates";

export type CapabilityBatchFailure = {
	kind: CapabilityBatchKind;
	reason: string;
};

export function getCapabilityBatchFailures(
	lists: CapabilityBatchLists,
): CapabilityBatchFailure[] {
	if (lists.authentication) {
		return [];
	}

	const kindResults: Array<[CapabilityBatchKind, CapabilityListResult]> = [
		["tools", lists.tools],
		["resources", lists.resources],
		["prompts", lists.prompts],
		["templates", lists.templates],
	];
	return kindResults.flatMap(([kind, result]) => {
		if (!result.degraded_reason && result.state !== "failed" && result.state !== "transport_error") {
			return [];
		}
		return [
			{
				kind,
				reason: result.degraded_reason ?? result.state ?? "capability discovery failed",
			},
		];
	});
}

export function assertCompleteCapabilityBatch(
	lists: CapabilityBatchLists,
): void {
	if (lists.authentication) {
		throw new Error(
			`Capability discovery is blocked by authentication: ${lists.authentication.code}`,
		);
	}
	const failures = getCapabilityBatchFailures(lists);
	if (failures.length === 0) {
		return;
	}
	throw new Error(
		`Capability discovery is incomplete: ${failures
			.map(({ kind, reason }) => `${kind}: ${reason}`)
			.join("; ")}`,
	);
}

type ServerCapabilityRefreshResult = {
	server_id: string;
	catalog_revision: number;
	catalog_changed: boolean;
};

type ServerCapabilityListsResult = {
	tools: CapabilityListResult;
	resources: CapabilityListResult;
	prompts: CapabilityListResult;
	resource_templates: CapabilityListResult;
	authentication?: CapabilityAuthenticationFailure | null;
};

type CapabilityDetailResult = {
	item?: unknown | null;
	meta?: unknown;
	state?: string;
	degraded_reason?: string;
};

function normalizeCapabilityListPayload(
  resp:
    | {
        success: boolean;
        data?: CapabilityListResult | null;
        error?: unknown | null;
      }
    | null
    | undefined,
): CapabilityListResult {
	if (!resp?.data) {
		return {
			items: [],
			state: "empty_data",
			degraded_reason: "empty_payload",
		};
	}
	const items = Array.isArray(resp.data.items)
		? resp.data.items.map((item) => {
				if (!item || typeof item !== "object" || Array.isArray(item)) {
					return item;
				}
				const record = item as Record<string, unknown>;
				const refId = record.ref_id;
				return typeof refId === "string" && refId.trim()
					? { ...record, id: refId }
					: item;
			})
		: [];
	if (items.length === 0 && !resp.data.state) {
		return { ...resp.data, items, state: "empty_data" };
	}
	return { ...resp.data, items };
}

function capabilityTransportErrorResult(): CapabilityListResult {
  return {
    items: [],
    state: "transport_error",
    degraded_reason: "request_failed",
  };
}

function normalizeBatchCapabilityLists(
	data: ServerCapabilityListsResult,
): CapabilityBatchLists {
  return {
    tools: normalizeCapabilityListPayload({ success: true, data: data.tools }),
    resources: normalizeCapabilityListPayload({
      success: true,
      data: data.resources,
    }),
    prompts: normalizeCapabilityListPayload({ success: true, data: data.prompts }),
    templates: normalizeCapabilityListPayload({
      success: true,
      data: data.resource_templates,
    }),
		authentication: data.authentication,
  };
}

async function fetchCapabilityDetail(
	serverId: string,
	kind: "tools" | "resources" | "prompts" | "templates",
	key: string,
): Promise<CapabilityDetailResult> {
	const q = new URLSearchParams({ id: serverId, kind, key });
	const resp = await fetchApi<{
		success: boolean;
		data?: CapabilityDetailResult | null;
		error?: unknown | null;
	}>(`/api/mcp/servers/capability/detail?${q}`);
	if (!resp?.data) {
		return {
			item: null,
			state: "empty_data",
			degraded_reason: "empty_payload",
		};
	}
	return resp.data;
}

/** Row shape from `/api/mcp/profile/list` */
interface ProfileApiRow {
	id: string;
	name: string;
	description?: string | null;
	profile_type?: string;
	priority?: number;
	is_active?: boolean;
	is_default?: boolean;
	role?: string | null;
	allowed_operations?: string[];
	authoring_generation: number;
	profile_mode?: ProfileMode;
}

function profileRowToConfigSuit(p: ProfileApiRow): ConfigSuit {
	return {
		id: p.id,
		name: p.name,
		description: p.description ?? undefined,
		suit_type: p.profile_type ?? "",
		priority: p.priority ?? 0,
		is_active: p.is_active ?? false,
		is_default: p.is_default ?? false,
		authoring_generation: p.authoring_generation,
		role: p.role ?? undefined,
		allowed_operations: p.allowed_operations ?? [],
		profile_mode: p.profile_mode,
	};
}

function asOptionalString(v: unknown): string | undefined {
	return typeof v === "string" ? v : undefined;
}

function asOAuthIssue(v: unknown): OAuthStatus["issue"] | undefined {
	if (!v || typeof v !== "object") return undefined;
	const record = v as Record<string, unknown>;
	if (typeof record.code !== "string" || typeof record.message !== "string") {
		return undefined;
	}
	return {
		code: record.code,
		message: record.message,
	};
}

function normalizeOAuthSummary(record: Record<string, unknown>) {
	return {
		oauth_status: asOptionalString(record.oauth_status) ?? null,
		oauth_custody_state: asOptionalString(record.oauth_custody_state) ?? null,
		oauth_requires_reconnect:
			typeof record.oauth_requires_reconnect === "boolean"
				? record.oauth_requires_reconnect
				: null,
		oauth_issue: asOAuthIssue(record.oauth_issue) ?? null,
	};
}

interface NotificationData {
	[key: string]: unknown;
	event?: string;
}

export class ApiRequestError extends Error {
	readonly status: number;
	readonly code?: string;
	readonly details?: ApiConflictDetails;

	constructor(
		message: string,
		status: number,
		code?: string,
		details?: ApiConflictDetails,
	) {
		super(message);
		this.name = "ApiRequestError";
		this.status = status;
		this.code = code;
		this.details = details;
	}
}

// Enhanced error handling utility
function createApiError(response: Response, parsed?: unknown): ApiRequestError {
	if (parsed && typeof parsed === "object") {
		const obj = parsed as Record<string, unknown>;
		if (typeof obj.message === "string") {
			return new ApiRequestError(obj.message, response.status);
		}
		if (obj.error && typeof obj.error === "object") {
			const errorObj = obj.error as Record<string, unknown>;
			if (typeof errorObj.message === "string") {
				return new ApiRequestError(
					errorObj.message,
					response.status,
					typeof errorObj.code === "string" ? errorObj.code : undefined,
					errorObj.details && typeof errorObj.details === "object"
						? (errorObj.details as ApiConflictDetails)
						: undefined,
				);
			}
		}
	}
	return new ApiRequestError(
		`API Error: ${response.status} ${response.statusText}`,
		response.status,
	);
}

/** True when fetchApi failed with an HTTP 404 (e.g. older backend without a route). */
export function isApiNotFoundError(error: unknown): boolean {
	if (error instanceof ApiRequestError) {
		return error.status === 404;
	}
	if (!(error instanceof Error)) {
		return false;
	}
	return /\b404\b/.test(error.message);
}

export function isInspectorSessionUnavailableError(error: unknown): boolean {
	if (!(error instanceof Error)) {
		return false;
	}
	const message = error.message.toLowerCase();
	return (
		(message.includes("inspector session") ||
			message.includes("native inspector validation session")) &&
		(message.includes("not found or expired") ||
			message.includes("no longer connected"))
	);
}

const toTrimmedString = (value: unknown): string | undefined => {
	if (typeof value !== "string") return undefined;
	const trimmed = value.trim();
	return trimmed.length > 0 ? trimmed : undefined;
};

const normalizeServerIcon = (icon: unknown): ServerIcon | null => {
	if (!icon || typeof icon !== "object") return null;
	const o = icon as Record<string, unknown>;
	const src =
    toTrimmedString(o.src) || toTrimmedString(o.url) || toTrimmedString(o.href);
	if (!src) return null;
	const mimeType =
    toTrimmedString(o.mime_type) || toTrimmedString(o.mimeType) || undefined;
	const sizes =
		toTrimmedString(o.sizes) || toTrimmedString(o.size) || undefined;
	const normalized: ServerIcon = { src };
	if (mimeType) normalized.mimeType = mimeType;
	if (sizes) normalized.sizes = sizes;
	return normalized;
};

const normalizeServerIconList = (icons: unknown): ServerIcon[] => {
	if (!icons) return [];
	const array = Array.isArray(icons) ? icons : [icons];
	return array
		.map((icon) => normalizeServerIcon(icon))
		.filter((icon): icon is ServerIcon => Boolean(icon));
};

const normalizeRepositoryInfo = (
  repo: unknown,
): RegistryRepositoryInfo | null => {
	if (!repo) return null;
	if (typeof repo === "string") {
		const url = toTrimmedString(repo);
		return url ? { url } : null;
	}
	if (typeof repo !== "object") return null;
	const r = repo as Record<string, unknown>;
	const info: RegistryRepositoryInfo = {};
	const url = toTrimmedString(r.url);
	const source = toTrimmedString(r.source);
	const subfolder = toTrimmedString(r.subfolder);
	const id = toTrimmedString(r.id);
	if (url) info.url = url;
	if (source) info.source = source;
	if (subfolder) info.subfolder = subfolder;
	if (id) info.id = id;
	return Object.keys(info).length > 0 ? info : null;
};

export const normalizeServerMeta = (
  meta: unknown,
): ServerMetaInfo | undefined => {
	if (!meta || typeof meta !== "object") return undefined;
	const m = meta as Record<string, unknown>;
	const normalized: ServerMetaInfo = {};
	const description = toTrimmedString(m.description);
	const version = toTrimmedString(m.version);
	const websiteUrl =
		toTrimmedString(m.websiteUrl) ||
		toTrimmedString(m.website_url) ||
		toTrimmedString(m.website);
	const repository = normalizeRepositoryInfo(m.repository);
	const icons = normalizeServerIconList(m.icons);

	if (description) normalized.description = description;
	if (version) normalized.version = version;
	if (websiteUrl) normalized.websiteUrl = websiteUrl;
	if (repository) normalized.repository = repository;
	if (icons.length) normalized.icons = icons;

	if (m._meta && typeof m._meta === "object") {
		normalized._meta = m._meta as RegistryMetaPayload;
	}

	if (m.extras && typeof m.extras === "object") {
		normalized.extras = m.extras as Record<string, unknown>;
	}

	const extrasObj =
		m.extras && typeof m.extras === "object"
			? (m.extras as Record<string, unknown>)
			: undefined;
	const legacyFromExtras =
		extrasObj && "legacy" in extrasObj ? extrasObj.legacy : undefined;
	const legacy = m.legacy ?? legacyFromExtras;
	if (!normalized.extras && legacy && typeof legacy === "object") {
		normalized.extras = { legacy };
	}

	return Object.keys(normalized).length > 0 ? normalized : undefined;
};

const normalizeApiConfigValue = (value: unknown) => {
	if (!value || typeof value !== "object") return undefined;
	const record = value as Record<string, unknown>;
	if (record.kind === "literal" && typeof record.value === "string") {
		return { kind: "literal" as const, value: record.value };
	}
	if (record.kind === "secret_ref" && typeof record.alias === "string") {
		return { kind: "secret_ref" as const, alias: record.alias };
	}
	return undefined;
};

const normalizeConfigValueRecord = (value: unknown) => {
	if (!value || typeof value !== "object" || Array.isArray(value)) {
		return undefined;
	}
	const result: Record<string, NonNullable<ReturnType<typeof normalizeApiConfigValue>>> = {};
	for (const [key, configValue] of Object.entries(value)) {
		const normalized = normalizeApiConfigValue(configValue);
		if (!normalized) return undefined;
		result[key] = normalized;
	}
	return result;
};

const normalizeServerTransportValidity = (
	value: unknown,
): ServerTransportValidity | undefined => {
	if (!value || typeof value !== "object") return undefined;
	const record = value as Record<string, unknown>;
	if (
		record.state !== "valid" &&
		record.state !== "invalid" &&
		record.state !== "missing"
	) {
		return undefined;
	}
	const diagnostics = Array.isArray(record.diagnostics)
		? record.diagnostics.flatMap((diagnostic) => {
			if (!diagnostic || typeof diagnostic !== "object") return [];
			const item = diagnostic as Record<string, unknown>;
			return typeof item.code === "string" && typeof item.field === "string"
				? [{ code: item.code, field: item.field }]
				: [];
		})
		: [];

	let draft: ServerTransportValidity["draft"];
	if (record.draft && typeof record.draft === "object") {
		const draftRecord = record.draft as Record<string, unknown>;
		if (draftRecord.kind === "stdio") {
			const env = normalizeConfigValueRecord(draftRecord.env);
			if (env && Array.isArray(draftRecord.args) && draftRecord.args.every((arg) => typeof arg === "string")) {
				draft = {
					kind: "stdio",
					command:
						typeof draftRecord.command === "string" || draftRecord.command === null
							? draftRecord.command
							: undefined,
					args: draftRecord.args,
					env,
				};
			}
		} else if (draftRecord.kind === "http") {
			const headers = normalizeConfigValueRecord(draftRecord.headers);
			if (
				headers &&
				(draftRecord.protocol === "sse" || draftRecord.protocol === "streamable_http")
			) {
				draft = {
					kind: "http",
					protocol: draftRecord.protocol,
					endpoint:
						typeof draftRecord.endpoint === "string" || draftRecord.endpoint === null
							? draftRecord.endpoint
							: undefined,
					headers,
				};
			}
		} else if (
			draftRecord.kind === "unrecognized" &&
			typeof draftRecord.declared_type === "string"
		) {
			draft = { kind: "unrecognized", declared_type: draftRecord.declared_type };
		}
	}

	return { state: record.state, diagnostics, draft };
};

const serializeRepositoryForApi = (
	repo: RegistryRepositoryInfo | null | undefined,
): Record<string, string> | undefined => {
	if (!repo || typeof repo !== "object") return undefined;
	const payload: Record<string, string> = {};
	const assign = (key: string, value?: string | null) => {
		const trimmed = toTrimmedString(value);
		if (trimmed) payload[key] = trimmed;
	};
	assign("url", repo.url ?? undefined);
	assign("source", repo.source ?? undefined);
	assign("subfolder", repo.subfolder ?? undefined);
	assign("id", repo.id ?? undefined);
	return Object.keys(payload).length > 0 ? payload : undefined;
};

export const serializeMetaForApi = (
	meta: ServerMetaInfo | null | undefined,
): Record<string, unknown> | undefined => {
	if (!meta || typeof meta !== "object") return undefined;
	const payload: Record<string, unknown> = {};

	const assignString = (key: string, value: string | null | undefined) => {
		const trimmed = toTrimmedString(value);
		if (trimmed) {
			payload[key] = trimmed;
		}
	};

	assignString("description", meta.description ?? undefined);
	assignString("version", meta.version ?? undefined);
	assignString("websiteUrl", meta.websiteUrl ?? undefined);

	const repositoryPayload = serializeRepositoryForApi(meta.repository);
	if (repositoryPayload) {
		payload.repository = repositoryPayload;
	}

	if (meta._meta && typeof meta._meta === "object") {
		payload._meta = meta._meta;
	}

	if (meta.extras && typeof meta.extras === "object") {
		payload.extras = meta.extras;
	}

	if (Array.isArray(meta.icons) && meta.icons.length > 0) {
		payload.icons = meta.icons.map((icon) => {
			const normalized: Record<string, string> = { src: icon.src };
			if (icon.mimeType) normalized.mimeType = icon.mimeType;
			if (icon.sizes) normalized.sizes = icon.sizes;
			return normalized;
		});
	}

	return Object.keys(payload).length > 0 ? payload : undefined;
};

type TransportConfigValue =
	| { kind: "literal"; value: string }
	| { kind: "secret_ref"; alias: string };

const EXACT_SECRET_REFERENCE = /^\[\[secret:([^\]]+)\]\]$/;

const serializeTransportConfigValues = (
	values?: Record<string, string>,
): Record<string, TransportConfigValue> => {
	if (!values || typeof values !== "object") return {};

	return Object.fromEntries(
		Object.entries(values).map(([key, value]) => {
			const secretReference = value.match(EXACT_SECRET_REFERENCE);
			return [
				key,
				secretReference
					? { kind: "secret_ref", alias: secretReference[1] }
					: { kind: "literal", value },
			];
		}),
	);
};

export const serializeServerTransport = (
	serverConfig: Partial<MCPServerConfig>,
	requireKind = false,
) => {
	if (requireKind && !serverConfig.kind) {
		throw new Error("Server updates require a complete transport kind");
	}

	const kind = serverConfig.kind ?? "stdio";
	if (kind === "stdio") {
		return {
			kind: "stdio" as const,
			command: serverConfig.command ?? null,
			args: Array.isArray(serverConfig.args) ? serverConfig.args : [],
			env: serializeTransportConfigValues(serverConfig.env),
		};
	}

	return {
		kind: "http" as const,
		protocol: kind === "sse" ? ("sse" as const) : ("streamable_http" as const),
		endpoint: serverConfig.url ?? serverConfig.command ?? null,
		headers: serializeTransportConfigValues(serverConfig.headers),
	};
};

export type ServersPreviewRequest = {
	include_details?: boolean | null;
	timeout_ms?: number | null;
	servers: Array<{
		name: string;
		server_id?: string;
		transport: ReturnType<typeof serializeServerTransport>;
	}>;
};

const normalizeCapabilitySummary = (
	capability: unknown,
): ServerCapabilitySummary | undefined => {
	if (capability === null || capability === undefined) return undefined;
	if (typeof capability !== "object") {
		throw new Error("Invalid server capability summary");
	}

	const source = capability as Record<string, unknown>;
	const snapshotStates = new Set(["ready", "invalidated", "unavailable"]);
	if (!snapshotStates.has(String(source.snapshotState))) {
		throw new Error("Invalid server capability snapshot state");
	}
	if (!Number.isSafeInteger(source.revision) || (source.revision as number) < 0) {
		throw new Error("Invalid server capability revision");
	}
	if (typeof source.observedAt !== "string" || source.observedAt.length === 0) {
		throw new Error("Invalid server capability observation time");
	}

	const normalizeKind = (key: string): CapabilityKindSummary => {
		const value = source[key];
		if (!value || typeof value !== "object") {
			throw new Error(`Invalid ${key} capability summary`);
		}
		const kind = value as Record<string, unknown>;
		if (!["unknown", "unsupported", "supported"].includes(String(kind.declaration))) {
			throw new Error(`Invalid ${key} capability declaration`);
		}
		if (!["unknown", "complete", "failed"].includes(String(kind.inventory))) {
			throw new Error(`Invalid ${key} capability inventory`);
		}
		if (!Number.isSafeInteger(kind.currentCount) || (kind.currentCount as number) < 0) {
			throw new Error(`Invalid ${key} capability count`);
		}
		if (typeof kind.currentAvailable !== "boolean") {
			throw new Error(`Invalid ${key} capability availability`);
		}
		if (
			kind.lastError !== undefined &&
			kind.lastError !== null &&
			typeof kind.lastError !== "string"
		) {
			throw new Error(`Invalid ${key} capability error`);
		}
		return kind as unknown as CapabilityKindSummary;
	};

	return {
		snapshotState: source.snapshotState as ServerCapabilitySummary["snapshotState"],
		revision: source.revision as number,
		observedAt: source.observedAt,
		tools: normalizeKind("tools"),
		prompts: normalizeKind("prompts"),
		resources: normalizeKind("resources"),
		resourceTemplates: normalizeKind("resourceTemplates"),
	};
};

const uniqBySrc = (icons: ServerIcon[]): ServerIcon[] => {
	const seen = new Set<string>();
	const result: ServerIcon[] = [];
	for (const icon of icons) {
		if (!seen.has(icon.src)) {
			seen.add(icon.src);
			result.push(icon);
		}
	}
	return result;
};

const enrichServerRecord = <T extends Record<string, unknown>>(server: T) => {
	const base: Record<string, unknown> = { ...server };
	const meta = normalizeServerMeta(server.meta);
	const directIcons = normalizeServerIconList(server.icons);
	const combinedIcons = uniqBySrc([...(meta?.icons ?? []), ...directIcons]);
	const capability = normalizeCapabilitySummary(
		server.capability,
	);
	const transportValidity = normalizeServerTransportValidity(
		server.transport_validity,
	);

	if (meta || combinedIcons.length) {
		base.meta = {
			...(meta ?? {}),
			...(combinedIcons.length ? { icons: combinedIcons } : {}),
		} as ServerMetaInfo;
	} else {
		delete base.meta;
	}

	if (combinedIcons.length) {
		base.icons = combinedIcons;
	} else {
		delete base.icons;
	}

	if (capability) {
		base.capability = capability;
	} else {
		delete base.capability;
	}

	if (transportValidity) {
		base.transport_validity = transportValidity;
	} else {
		delete base.transport_validity;
	}

	return base as T & {
		meta?: ServerMetaInfo;
		icons?: ServerIcon[];
		capability?: ServerCapabilitySummary;
		transport_validity?: ServerTransportValidity;
	};
};

function normalizeServerDetail(
  enhanced: Record<string, unknown>,
  id: string,
): ServerDetail {
	const enabledValue =
		typeof enhanced?.enabled === "boolean"
			? enhanced.enabled
			: typeof enhanced?.globally_enabled === "boolean"
				? enhanced.globally_enabled
				: undefined;

	const instances = Array.isArray(enhanced?.instances)
		? (enhanced.instances as InstanceSummary[])
		: [];

	const detailRecord = enhanced as Record<string, unknown>;
	const source = detailRecord.source as ServerSource | undefined;

	const serverType =
		toTrimmedString(detailRecord.server_type as string | undefined) ??
		toTrimmedString(detailRecord.kind as string | undefined);

	const displayName = asOptionalString(detailRecord.name) ?? id;

	return {
		id: asOptionalString(detailRecord.id) ?? id,
		name: displayName,
		status:
			(typeof enhanced?.status === "string" && enhanced.status) || "unknown",
		server_type: serverType,
		source,
		enabled: enabledValue,
		unify_direct_exposure_eligible:
			typeof enhanced?.unify_direct_exposure_eligible === "boolean"
				? enhanced.unify_direct_exposure_eligible
				: undefined,
		globally_enabled:
			typeof enhanced?.globally_enabled === "boolean"
				? enhanced.globally_enabled
				: undefined,
		enabled_in_suits:
			typeof enhanced?.enabled_in_suits === "boolean"
				? enhanced.enabled_in_suits
				: undefined,
		enabled_in_profile:
			typeof enhanced?.enabled_in_profile === "boolean"
				? enhanced.enabled_in_profile
				: undefined,
		instances,
		command: asOptionalString(detailRecord.command),
		args: Array.isArray(enhanced?.args)
			? (enhanced.args as string[])
			: undefined,
		env:
			typeof enhanced?.env === "object" && enhanced?.env !== null
				? (enhanced.env as Record<string, string>)
				: undefined,
		url: typeof enhanced?.url === "string" ? enhanced.url : undefined,
		auth_mode: asOptionalString(detailRecord.auth_mode) ?? null,
		...normalizeOAuthSummary(detailRecord),
		headers:
			typeof enhanced?.headers === "object" && enhanced?.headers !== null
				? (enhanced.headers as Record<string, string>)
				: undefined,
		meta: normalizeServerMeta(enhanced?.meta),
    server_info:
      typeof detailRecord.server_info === "object" &&
      detailRecord.server_info !== null
        ? (detailRecord.server_info as StandardServerInfo)
        : detailRecord.server_info === null
          ? null
          : undefined,
		icons: normalizeServerIconList(enhanced?.icons),
		capability: enhanced.capability as ServerCapabilitySummary | undefined,
		transport_validity: normalizeServerTransportValidity(
			detailRecord.transport_validity,
		),
		source_revision_set:
			enhanced.source_revision_set as CatalogRevisionSet | undefined,
		namespace_issue:
			typeof detailRecord.namespace_issue === "object" &&
			detailRecord.namespace_issue !== null
				? (detailRecord.namespace_issue as ServerNamespaceIssue)
				: detailRecord.namespace_issue === null
					? null
					: undefined,
	};
}

type FetchApiOptions = RequestInit & {
	logFailure?: boolean;
};

// Core API request function
async function fetchApi<T>(
  endpoint: string,
  options?: FetchApiOptions,
): Promise<T> {
	if (isBoardDemoMode()) {
		return handleDemoApiRequest<T>(endpoint, options);
	}

	const url = resolveApiUrl(endpoint);
	const { logFailure = true, ...requestOptions } = options ?? {};
	const headers =
		requestOptions.headers instanceof Headers
			? requestOptions.headers
			: new Headers(requestOptions.headers ?? {});
	// Avoid forcing JSON header on GET requests to keep caching friendly.
	if (!headers.has("content-type") && requestOptions.body) {
		if (!(requestOptions.body instanceof FormData)) {
			headers.set("content-type", "application/json");
		}
	}
	try {
		const requestInit: RequestInit = {
			credentials: "include",
			...requestOptions,
		};
		requestInit.headers = headers;
		const response = await fetch(url, requestInit);

		if (!response.ok) {
			const errorText = await response.text();
			let parsed: unknown;
			let errorPayload: unknown = errorText;
			try {
				parsed = JSON.parse(errorText);
				errorPayload = parsed;
			} catch {
				/* keep raw response text as the error payload */
			}
			if (logFailure) {
				console.error(`API Error (${response.status}):`, errorPayload);
			}
			throw createApiError(response, parsed);
		}

		if (response.status === 204) {
			return undefined as T;
		}

		const text = await response.text();
		if (!text) {
			return undefined as T;
		}
		return JSON.parse(text) as T;
	} catch (error) {
		if (logFailure) {
			console.error("API request failed for endpoint:", endpoint, error);
		}
		throw error;
	}
}

// Utility function to extract data from wrapped API responses
function extractApiData<T>(response: ApiWrapper<T>): T {
    if (response.success && response.data) {
        return response.data;
    }
    // Prefer detailed backend message when available, but also preserve non-fatal warnings if any were returned.
    // This function remains focused on data extraction; pages now read warning arrays on success paths.
    throw new Error(
        response.error ? String(response.error) : "API request failed",
    );
}

export const secretsApi = {
	status: async (): Promise<SecretStoreStatusData> => {
		const resp = await fetchApi<SecretStoreStatusResp>("/api/secrets/status");
		return extractApiData(resp as ApiWrapper<SecretStoreStatusData>);
	},

	list: async (): Promise<SecretMetadata[]> => {
		const resp = await fetchApi<SecretListResp>("/api/secrets/list");
		return resp.data?.secrets ?? [];
	},

	get: async (alias: string): Promise<SecretMetadata> => {
		const q = new URLSearchParams({ alias });
    const resp = await fetchApi<SecretMetadataResp>(
      `/api/secrets/details?${q}`,
    );
		return extractApiData(resp as ApiWrapper<SecretMetadata>);
	},

	listUsages: async (alias: string): Promise<SecretUsage[]> => {
		const q = new URLSearchParams({ alias });
    const resp = await fetchApi<SecretUsageListResp>(
      `/api/secrets/usages?${q}`,
    );
		return resp.data?.usages ?? [];
	},

	create: async (payload: {
		alias: string;
		kind: SecretKind;
		value: string;
		label?: string | null;
		origin?: SecretOrigin | null;
	}): Promise<SecretMetadata> => {
		const resp = await fetchApi<SecretMetadataResp>("/api/secrets/create", {
			method: "POST",
			body: JSON.stringify(payload),
		});
		return extractApiData(resp as ApiWrapper<SecretMetadata>);
	},

	update: async (payload: {
		alias: string;
		kind?: SecretKind;
		value?: string;
		label?: string | null;
		origin?: SecretOrigin | null;
	}): Promise<SecretMetadata> => {
		const resp = await fetchApi<SecretMetadataResp>("/api/secrets/update", {
			method: "POST",
			body: JSON.stringify(payload),
		});
		return extractApiData(resp as ApiWrapper<SecretMetadata>);
	},

  delete: async (
    alias: string,
    force = false,
  ): Promise<{ alias: string; deleted: boolean }> => {
		const resp = await fetchApi<SecretDeleteResp>("/api/secrets/delete", {
			method: "DELETE",
			body: JSON.stringify({ alias, force }),
		});
    return extractApiData(
      resp as ApiWrapper<{ alias: string; deleted: boolean }>,
    );
	},

	unlock: async (passphrase: string): Promise<SecretStoreStatusData> => {
		const resp = await fetchApi<SecretStoreStatusResp>("/api/secrets/unlock", {
			method: "POST",
			body: JSON.stringify({ passphrase }),
		});
		return extractApiData(resp as ApiWrapper<SecretStoreStatusData>);
	},

	rotatePassphrase: async (
		currentPassphrase: string,
		newPassphrase: string,
		confirm: string,
	): Promise<SecretStoreStatusData> => {
    const resp = await fetchApi<SecretStoreStatusResp>(
      "/api/secrets/passphrase/rotate",
      {
			method: "POST",
			body: JSON.stringify({
				current_passphrase: currentPassphrase,
				new_passphrase: newPassphrase,
				confirm,
			}),
      },
    );
		return extractApiData(resp as ApiWrapper<SecretStoreStatusData>);
	},

	switchProvider: async (
		mode: SwitchableSecretStoreProviderMode,
		options?: {
			passphrase?: string;
			currentPassphrase?: string;
			confirmationPhrase?: string;
		},
	): Promise<SecretStoreStatusData> => {
		const body: {
			mode: typeof mode;
			passphrase?: string;
			current_passphrase?: string;
			confirmation_phrase?: string;
		} = { mode };
		if (options?.passphrase) {
			body.passphrase = options.passphrase;
		}
		if (options?.currentPassphrase) {
			body.current_passphrase = options.currentPassphrase;
		}
		if (options?.confirmationPhrase) {
			body.confirmation_phrase = options.confirmationPhrase;
		}
    const resp = await fetchApi<ProviderSwitchResp>(
      "/api/secrets/provider/switch",
      {
			method: "POST",
			body: JSON.stringify(body),
      },
    );
    const data = extractApiData(
      resp as ApiWrapper<{ new_status: SecretStoreStatusData }>,
    );
		return data.new_status;
	},

	// Password protection
	passwordStatus: async (): Promise<{
		enabled: boolean;
		scope: string[];
		has_password: boolean;
	}> => {
    const resp = await fetchApi<PasswordStatusResp>(
      "/api/secrets/password/status",
    );
		return extractApiData(
			resp as ApiWrapper<{
				enabled: boolean;
				scope: string[];
				has_password: boolean;
			}>,
		);
	},

	setPassword: async (
		password: string,
		confirm: string,
		scope?: string[],
	): Promise<{ enabled: boolean; scope: string[] }> => {
		const resp = await fetchApi<PasswordSetResp>("/api/secrets/password/set", {
			method: "POST",
			body: JSON.stringify({ password, confirm, scope }),
		});
    return extractApiData(
      resp as ApiWrapper<{ enabled: boolean; scope: string[] }>,
    );
	},

	verifyPassword: async (password: string): Promise<{ valid: boolean }> => {
    const resp = await fetchApi<PasswordVerifyResp>(
      "/api/secrets/password/verify",
      {
			method: "POST",
			body: JSON.stringify({ password }),
      },
    );
		return extractApiData(resp as ApiWrapper<{ valid: boolean }>);
	},

	changePassword: async (
		oldPassword: string,
		newPassword: string,
		confirm: string,
	): Promise<{ enabled: boolean; scope: string[]; has_password: boolean }> => {
    const resp = await fetchApi<PasswordStatusResp>(
      "/api/secrets/password/change",
      {
			method: "POST",
        body: JSON.stringify({
          old_password: oldPassword,
          new_password: newPassword,
          confirm,
        }),
      },
    );
    return extractApiData(
      resp as ApiWrapper<{
        enabled: boolean;
        scope: string[];
        has_password: boolean;
      }>,
    );
	},

  clearPassword: async (
    password: string,
  ): Promise<{ enabled: boolean; scope: string[]; has_password: boolean }> => {
    const resp = await fetchApi<PasswordStatusResp>(
      "/api/secrets/password/clear",
      {
			method: "POST",
			body: JSON.stringify({ password }),
      },
    );
    return extractApiData(
      resp as ApiWrapper<{
        enabled: boolean;
        scope: string[];
        has_password: boolean;
      }>,
    );
	},

	updatePasswordScope: async (
		scope: string[],
		currentPassword: string,
	): Promise<{ enabled: boolean; scope: string[]; has_password: boolean }> => {
    const resp = await fetchApi<PasswordStatusResp>(
      "/api/secrets/password/scope",
      {
			method: "POST",
			body: JSON.stringify({ scope, current_password: currentPassword }),
      },
    );
    return extractApiData(
      resp as ApiWrapper<{
        enabled: boolean;
        scope: string[];
        has_password: boolean;
      }>,
    );
	},
};

// Server Management API
export const serversApi = {
	getAll: async (query?: {
		limit?: number;
		offset?: number;
	}): Promise<ServerListResponse> => {
		const search = new URLSearchParams();
		if (query?.limit !== undefined) {
			search.set("limit", String(query.limit));
		}
		if (query?.offset !== undefined) {
			search.set("offset", String(query.offset));
		}
		const suffix = search.size > 0 ? `?${search}` : "";
		const resp = await fetchApi<ServerListResp>(`/api/mcp/servers/list${suffix}`);
		const rawServers = Array.isArray(resp?.data?.servers)
			? resp.data.servers
			: [];
		const servers = rawServers.map((server): ServerSummary => {
			const rec = server as unknown as Record<string, unknown>;
			const enhanced = enrichServerRecord(rec);
			const er = enhanced as Record<string, unknown>;
			const source = er.source as ServerSource | undefined;
			const serverType =
				toTrimmedString(er.server_type as string | undefined) ??
				toTrimmedString(er.serverType as string | undefined) ??
				toTrimmedString(er.kind as string | undefined);
			return {
				...enhanced,
				server_type: serverType,
				source,
				auth_mode: asOptionalString(er.auth_mode) ?? null,
				...normalizeOAuthSummary(er),
			} as ServerSummary;
		});
		return { servers };
	},

	getOAuthStatus: async (id: string): Promise<OAuthStatus | null> => {
		try {
			const q = new URLSearchParams({ id });
			const resp = await fetchApi<ApiWrapper<OAuthStatus>>(
				`/api/mcp/servers/oauth/status?${q}`,
			);
			return resp?.data ?? null;
		} catch (error) {
			console.error("Failed to fetch OAuth status:", error);
			return null;
		}
	},

	prepareOAuth: async (
		id: string,
		payload: { redirect_uri: string; scopes?: string },
	): Promise<OAuthStatus> => {
		const resp = await fetchApi<ApiWrapper<OAuthStatus>>(
			"/api/mcp/servers/oauth/prepare",
			{
				method: "POST",
				body: JSON.stringify({ server_id: id, ...payload }),
			},
		);
		return extractApiData(resp);
	},

	saveOAuthConfig: async (
		id: string,
    config: Omit<OAuthConfigRequest, "server_id">,
	): Promise<OAuthStatus> => {
		const resp = await fetchApi<ApiWrapper<OAuthStatus>>(
			"/api/mcp/servers/oauth/config",
			{
			method: "POST",
			body: JSON.stringify({ server_id: id, ...config }),
		},
		);
		return extractApiData(resp);
	},

	initiateOAuth: async (id: string): Promise<OAuthInitiateResponse> => {
		const resp = await fetchApi<ApiWrapper<OAuthInitiateResponse>>(
			"/api/mcp/servers/oauth/initiate",
			{
			method: "POST",
			body: JSON.stringify({ server_id: id }),
		},
		);
		return extractApiData(resp);
	},

	revokeOAuth: async (id: string): Promise<OAuthStatus> => {
		const resp = await fetchApi<ApiWrapper<OAuthStatus>>(
			"/api/mcp/servers/oauth/revoke",
			{
			method: "POST",
			body: JSON.stringify({ server_id: id }),
		},
		);
		return extractApiData(resp);
	},

	handleOAuthCallback: async (
		payload: OAuthCallbackRequest,
	): Promise<OAuthStatus> => {
		const resp = await fetchApi<ApiWrapper<OAuthStatus>>(
			"/api/mcp/servers/oauth/callback",
			{
			method: "POST",
			body: JSON.stringify(payload),
		},
		);
		return extractApiData(resp);
	},

	getServer: async (id: string): Promise<ServerDetail> => {
		const q = new URLSearchParams({ id });
		const resp = await fetchApi<ServerDetailsResp>(
			`/api/mcp/servers/details?${q}`,
		);
		const data = (resp?.data ?? {}) as Record<string, unknown>;
		const enhanced = enrichServerRecord(data);

		return normalizeServerDetail(enhanced, id);
	},

	refreshRegistryMetadata: async (id: string): Promise<ServerDetail> => {
		const current = await serversApi.getServer(id);
		if (!current.source || current.source.type !== "registry") {
			throw new Error(
				`Cannot refresh metadata: server '${current.name}' does not have a registry source`,
			);
		}
		if (!current.source.ref) {
			throw new Error(
				`Cannot refresh metadata: registry source for '${current.name}' has no identifier`,
			);
		}
		const entry = await fetchCatalogEntryForSource(current.source);
		if (!entry) {
			throw new Error(
				`Server '${current.source.ref}' was not found in the registry catalog`,
			);
		}

    const officialMeta =
      entry._meta?.["io.modelcontextprotocol.registry/official"];

		return {
			...current,
			meta: {
				description: entry.description,
				version: entry.version,
				websiteUrl: entry.websiteUrl,
				repository: entry.repository,
				icons: entry.icons,
				_meta: entry._meta,
        extras: {
          packages: entry.packages,
          remotes: entry.remotes,
          status: officialMeta?.status,
        },
			},
		};
	},

	getInstances: async (serverId: string) => {
		const q = new URLSearchParams({ id: serverId });
		const resp = await fetchApi<InstanceListResp>(
			`/api/mcp/servers/instances/list?${q}`,
		);
		return (resp?.data?.instances as InstanceSummary[]) ?? [];
	},

	getInstance: async (
		serverId: string,
		instanceId: string,
	): Promise<InstanceDetail> => {
		try {
			const q = new URLSearchParams({ server: serverId, instance: instanceId });
			const resp = await fetchApi<InstanceDetailsResp>(
				`/api/mcp/servers/instances/details?${q}`,
			);
			const data = resp?.data;
			return {
				id: data?.id ?? instanceId,
				name: data?.name ?? instanceId,
				server_name: data?.server_name ?? serverId,
				status: data?.status ?? "unknown",
				allowed_operations: data?.allowed_operations ?? [],
				details: data?.details ?? {
					connection_attempts: 0,
					tools_count: 0,
					server_type: "unknown",
				},
			};
		} catch (error) {
			console.error(
				`Error fetching instance details for ${serverId}/${instanceId}:`,
				error,
			);
			return {
				id: instanceId,
				name: instanceId,
				server_name: serverId,
				status: "error",
				allowed_operations: [],
				details: {
					connection_attempts: 0,
					tools_count: 0,
					server_type: "unknown",
					error_message: error instanceof Error ? error.message : String(error),
				},
			};
		}
	},

	getInstanceHealth: async (serverId: string, instanceId: string) => {
		const q = new URLSearchParams({ server: serverId, instance: instanceId });
		const resp = await fetchApi<InstanceHealthResp>(
			`/api/mcp/servers/instances/health?${q}`,
		);
		return (
			(resp?.data as InstanceHealth) ?? {
				id: instanceId,
				name: instanceId,
				healthy: false,
				message: "No data",
				status: "unknown",
				checked_at: new Date().toISOString(),
			}
		);
	},

	// Instance management operations
	_manageInstance: (serverId: string, instanceId: string, action: string) =>
		fetchApi<OperationResponseResp>("/api/mcp/servers/instances/manage", {
			method: "POST",
			body: JSON.stringify({ server: serverId, instance: instanceId, action }),
		}),

	disconnectInstance: (serverId: string, instanceId: string) =>
		serversApi._manageInstance(serverId, instanceId, "Disconnect"),

	forceDisconnectInstance: (serverId: string, instanceId: string) =>
		serversApi._manageInstance(serverId, instanceId, "ForceDisconnect"),

	reconnectInstance: (serverId: string, instanceId: string) =>
		serversApi._manageInstance(serverId, instanceId, "Reconnect"),

	resetAndReconnectInstance: (serverId: string, instanceId: string) =>
		serversApi._manageInstance(serverId, instanceId, "ResetReconnect"),

	cancelInstance: (serverId: string, instanceId: string) =>
		serversApi._manageInstance(serverId, instanceId, "Cancel"),

	// Server management operations
	_manageServer: (
		serverId: string,
		action:
			| "Enable"
			| "Disable"
			| "Allow_Direct_Exposure"
			| "Deny_Direct_Exposure",
		sourceRevisionSet: CatalogRevisionSet,
		sync = false,
	) =>
		fetchApi<ApiResponse<null>>("/api/mcp/servers/manage", {
			method: "POST",
			body: JSON.stringify({
				id: serverId,
				action,
				sync,
				source_revision_set: sourceRevisionSet,
			}),
		}),

	enableServer: (
		serverId: string,
		sourceRevisionSet: CatalogRevisionSet,
		sync?: boolean,
	) => serversApi._manageServer(serverId, "Enable", sourceRevisionSet, sync),

	disableServer: (
		serverId: string,
		sourceRevisionSet: CatalogRevisionSet,
		sync?: boolean,
	) => serversApi._manageServer(serverId, "Disable", sourceRevisionSet, sync),

	setDirectExposureEligibility: (
		serverId: string,
		eligible: boolean,
		sourceRevisionSet: CatalogRevisionSet,
	) =>
		serversApi._manageServer(
			serverId,
			eligible ? "Allow_Direct_Exposure" : "Deny_Direct_Exposure",
			sourceRevisionSet,
		),

	// Server start/stop operations (mapped to enable/disable)
	startServer: (
		serverId: string,
		sourceRevisionSet: CatalogRevisionSet,
		sync?: boolean,
	) => serversApi._manageServer(serverId, "Enable", sourceRevisionSet, sync),

	stopServer: (
		serverId: string,
		sourceRevisionSet: CatalogRevisionSet,
		sync?: boolean,
	) => serversApi._manageServer(serverId, "Disable", sourceRevisionSet, sync),

	// CRUD operations
	createServer: async (serverConfig: Partial<MCPServerConfig>) => {
		const base: Record<string, unknown> = {
			name: serverConfig.name,
			transport: serializeServerTransport(serverConfig),
			unify_direct_exposure_eligible:
				serverConfig.unify_direct_exposure_eligible ?? undefined,
			source: serverConfig.source ?? undefined,
		};
		if (serverConfig.pending_import !== undefined) {
			base.pending_import = serverConfig.pending_import;
		}
		// Add meta information if present
		const metaPayload = serializeMetaForApi(serverConfig.meta ?? undefined);
		if (metaPayload) {
			base.meta = metaPayload;
		}
		return await fetchApi<ServerDetailsResp>("/api/mcp/servers/create", {
			method: "POST",
			body: JSON.stringify(base),
		});
	},

	updateServer: async (
		serverId: string,
		serverConfig: Partial<MCPServerConfig>,
	) => {
		assertServerCrudUpdate(serverConfig);
		const body: Record<string, unknown> = {
			id: serverId,
			transport: serializeServerTransport(serverConfig, true),
			source: serverConfig.source ?? undefined,
		};
		if (serverConfig.pending_import !== undefined) {
			body.pending_import = serverConfig.pending_import;
		}
		// Add meta information if present
		const metaPayload = serializeMetaForApi(serverConfig.meta ?? undefined);
		if (metaPayload) {
			body.meta = metaPayload;
		}
		return fetchApi<ServerDetailsResp>("/api/mcp/servers/update", {
			method: "POST",
			body: JSON.stringify(body),
		});
	},

	remediateNamespace: (serverId: string, namespace: string) =>
		fetchApi<OperationResponseResp>("/api/mcp/servers/namespace/remediate", {
			method: "POST",
			body: JSON.stringify({ id: serverId, namespace }),
		}),

	deleteServer: (serverId: string) =>
		fetchApi<ServerDetailsResp>("/api/mcp/servers/delete", {
			method: "DELETE",
			body: JSON.stringify({ id: serverId }),
		}),

	listAllCapabilities: async (
		serverId: string,
		refresh: "auto" | "force" | "cache" = "auto",
	): Promise<CapabilityBatchLists> => {
		try {
			const q = new URLSearchParams({ id: serverId, refresh });
			const resp = await fetchApi<ApiWrapper<ServerCapabilityListsResult>>(
				`/api/mcp/servers/capabilities/lists?${q.toString()}`,
			);
			const data = extractApiData(resp);
			return normalizeBatchCapabilityLists(data);
		} catch {
			const transportError = capabilityTransportErrorResult();
			return {
				tools: transportError,
				resources: transportError,
				prompts: transportError,
				templates: transportError,
			};
		}
	},
	refreshCapabilities: async (
		serverId: string,
	): Promise<ServerCapabilityRefreshResult> => {
		const resp = await fetchApi<ApiWrapper<ServerCapabilityRefreshResult>>(
			"/api/mcp/servers/capabilities/refresh",
			{
				method: "POST",
				body: JSON.stringify({ id: serverId }),
			},
		);
		return extractApiData(resp);
	},
	getCapabilityDetail: fetchCapabilityDetail,

	// Import servers from JSON-like configuration object, or from a registered client's config
	importServers: async (payload: {
		mcpServers?: Record<string, unknown>;
		client_identifier?: string;
		selected_server_names?: string[];
		dry_run?: boolean;
	}): Promise<ApiWrapper<ServersImportData>> => {
		return await fetchApi(`/api/mcp/servers/import`, {
			method: "POST",
			body: JSON.stringify(payload),
		});
	},

	// Preview capabilities for proposed server configs without importing
	previewServers: async (
		payload: ServersPreviewRequest,
	): Promise<ServersPreviewResponse> => {
		return await fetchApi(`/api/mcp/servers/preview`, {
			method: "POST",
			body: JSON.stringify(payload),
		});
	},
};

export type ServersImportResponse = Awaited<
	ReturnType<typeof serversApi.importServers>
>;

export type ServersPreviewResponse = {
	success: boolean;
	data?: { items: unknown[] } | null;
	error?: unknown | null;
};

export interface ImportStats {
	importedCount: number;
	importedServers: string[];
	skippedCount: number;
	skippedServers: string[];
	skippedDetails: SkippedServer[];
	failedCount: number;
	failedServers: string[];
	errorDetails?: Record<string, string> | null;
	runtimeSyncError?: string | null;
}

export function extractImportStats(
	response:
    ApiWrapper<ServersImportData> | ServersImportData | null | undefined,
): ImportStats {
	let payload: ServersImportData | null = null;
	if (response && typeof response === "object") {
		if ("data" in response && response.data) {
			payload = response.data as ServersImportData;
		} else if ("imported_count" in response) {
			payload = response as ServersImportData;
		}
	}

	const importedServers = Array.isArray(payload?.imported_servers)
		? payload!.imported_servers
		: [];
	const importedCount =
		typeof payload?.imported_count === "number"
			? payload.imported_count
			: importedServers.length;
	const skippedDetails = Array.isArray(payload?.skipped_servers)
		? payload!.skipped_servers
		: [];
	const skippedServers = skippedDetails.map((item) => item.name);
	const skippedCount =
		typeof payload?.skipped_count === "number"
			? payload.skipped_count
			: skippedServers.length;
	const failedServers = Array.isArray(payload?.failed_servers)
		? payload!.failed_servers
		: [];
	const failedCount =
		typeof payload?.failed_count === "number"
			? payload.failed_count
			: failedServers.length;
	return {
		importedCount,
		importedServers,
		skippedCount,
		skippedServers,
		skippedDetails,
		failedCount,
		failedServers,
		errorDetails: payload?.error_details ?? null,
		runtimeSyncError: payload?.runtime_sync_error ?? null,
	};
}

export function assertCompleteServerImport(stats: ImportStats): void {
	if (stats.failedCount > 0) {
		throw new Error(
			stats.errorDetails
				? JSON.stringify(stats.errorDetails)
				: `${stats.failedCount} server import(s) failed`,
		);
	}
	if (stats.runtimeSyncError) {
		throw new Error(
			`Servers were imported, but runtime synchronization failed: ${stats.runtimeSyncError}`,
		);
	}
}

interface InspectorTargetQuery {
	server_id?: string;
	server_name?: string;
	session_id?: string;
	mode?: "proxy" | "native";
	timeout_ms?: number;
}

interface InspectorListQuery extends InspectorTargetQuery {
	refresh?: boolean;
}

export interface InspectorTemplateReadRequest {
	uri_template: string;
	arguments?: Record<string, unknown>;
	mode?: "proxy" | "native";
	server_id?: string;
	server_name?: string;
	session_id?: string;
	timeout_ms?: number;
}

function createInspectorQuery(
	query: InspectorListQuery,
	initial?: Record<string, string>,
): URLSearchParams {
	const params = new URLSearchParams(initial);
	if (query.server_id) params.set("server_id", query.server_id);
	if (query.server_name) params.set("server_name", query.server_name);
	if (query.session_id) params.set("session_id", query.session_id);
	if (query.mode) params.set("mode", query.mode);
	if (query.refresh != null) params.set("refresh", String(query.refresh));
	if (query.timeout_ms != null) {
		params.set("timeout_ms", String(query.timeout_ms));
	}
	return params;
}

// Inspector API
export const inspectorApi = {
	toolsList: (query: InspectorListQuery) => {
		return fetchApi(
			`/api/mcp/inspector/tool/list?${createInspectorQuery(query)}`,
		);
	},
	toolCall: (req: {
		tool: string;
		server_id?: string;
		server_name?: string;
		arguments?: Record<string, unknown>;
		mode?: "proxy" | "native";
		timeout_ms?: number;
		session_id?: string;
	}) =>
		fetchApi(`/api/mcp/inspector/tool/call`, {
			method: "POST",
			body: JSON.stringify(req),
		}),
	toolCallStart: async (req: {
		tool: string;
		server_id?: string;
		server_name?: string;
		arguments?: Record<string, unknown>;
		mode?: "proxy" | "native";
		timeout_ms?: number;
		session_id?: string;
	}) =>
		fetchApi<ApiWrapper<InspectorToolCallStartData>>(
			`/api/mcp/inspector/tool/call/start`,
			{
				method: "POST",
				body: JSON.stringify(req),
			},
		),
	toolCallCancel: async (req: { call_id: string; reason?: string }) =>
		fetchApi<ApiWrapper<InspectorToolCallCancelData>>(
			`/api/mcp/inspector/tool/call/cancel`,
			{
				method: "POST",
				body: JSON.stringify(req),
			},
		),
	sessionOpen: async (req: {
		mode: "proxy" | "native";
		server_id?: string;
		server_name?: string;
		timeout_ms?: number;
	}) =>
		fetchApi<ApiWrapper<InspectorSessionOpenData>>(
			`/api/mcp/inspector/session/open`,
			{
				method: "POST",
				body: JSON.stringify(req),
			},
		),
	sessionRefresh: async (req: { session_id: string }) =>
		fetchApi<ApiWrapper<InspectorSessionOpenData>>(
			`/api/mcp/inspector/session/refresh`,
			{
				method: "POST",
				body: JSON.stringify(req),
			},
		),
	sessionClose: async (req: { session_id: string }) =>
		fetchApi<ApiWrapper<InspectorSessionCloseData>>(
			`/api/mcp/inspector/session/close`,
			{
				method: "POST",
				body: JSON.stringify(req),
			},
		),
	toolCallEventsWsUrl: (callId: string) => {
		const wsBase = resolveWebSocketUrl();
		return `${wsBase}/inspector/events?call_id=${encodeURIComponent(callId)}`;
	},
	resourcesList: (query: InspectorListQuery) => {
		return fetchApi(
			`/api/mcp/inspector/resource/list?${createInspectorQuery(query)}`,
		);
	},
	resourceRead: (query: InspectorTargetQuery & {
		uri: string;
	}) => {
		const params = createInspectorQuery(query, { uri: query.uri });
		return fetchApi(`/api/mcp/inspector/resource/read?${params}`);
	},
	templateRead: (req: InspectorTemplateReadRequest) =>
		fetchApi(`/api/mcp/inspector/template/read`, {
			method: "POST",
			body: JSON.stringify(req),
		}),
	promptsList: (query: InspectorListQuery) => {
		return fetchApi(
			`/api/mcp/inspector/prompt/list?${createInspectorQuery(query)}`,
		);
	},
	promptGet: (req: {
		name: string;
		server_id?: string;
		server_name?: string;
		session_id?: string;
		arguments?: Record<string, unknown>;
		mode?: "proxy" | "native";
		timeout_ms?: number;
	}) =>
		fetchApi(`/api/mcp/inspector/prompt/get`, {
			method: "POST",
			body: JSON.stringify(req),
		}),
	templatesList: (query: InspectorListQuery) => {
		return fetchApi(
			`/api/mcp/inspector/template/list?${createInspectorQuery(query)}`,
		);
	},
};

export const auditApi = {
	list: async (query?: {
		cursor?: string;
		limit?: number;
		category?: string;
		action?: string;
		status?: string;
		client_id?: string;
		profile_id?: string;
		server_id?: string;
		session_id?: string;
	}) => {
		const qs = new URLSearchParams();
		if (query?.cursor) qs.set("cursor", query.cursor);
		if (query?.limit != null) qs.set("limit", String(query.limit));
		if (query?.category) qs.set("category", query.category);
		if (query?.action) qs.set("action", query.action);
		if (query?.status) qs.set("status", query.status);
		if (query?.client_id) qs.set("client_id", query.client_id);
		if (query?.profile_id) qs.set("profile_id", query.profile_id);
		if (query?.server_id) qs.set("server_id", query.server_id);
		if (query?.session_id) qs.set("session_id", query.session_id);
		const resp = await fetchApi<AuditListResp>(`/api/audit/events?${qs}`);
		const page = extractApiData(resp);
		if (!page) {
			throw new Error("Audit list response missing data");
		}
		return page;
	},
	details: async (id: number) => {
		const qs = new URLSearchParams({ id: String(id) });
    const resp = await fetchApi<AuditEventDetailsResp>(
      `/api/audit/events/details?${qs}`,
    );
		const event = resp.data?.event;
		if (!event) {
      throw new Error(
        `Audit event details response missing event for id ${id}`,
      );
		}
		return event;
	},
	eventsWsUrl: () => {
		const wsBase = resolveWebSocketUrl();
		return `${wsBase}/audit/events`;
	},
	getPolicy: async () => {
		const resp = await fetchApi<AuditPolicyResp>("/api/audit/policy");
		return extractApiData(resp);
	},
	setPolicy: async (payload: AuditPolicySetReq) => {
		const resp = await fetchApi<AuditPolicyResp>("/api/audit/policy", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(payload),
		});
		return extractApiData(resp);
	},
};

// Tools Management API
export const toolsApi = {
	getAll: async () => {
		try {
			// Try to get tools from active profile first
			const suitsResponse = await configSuitsApi.getAll();
			const active = suitsResponse?.suits?.find((s) => s.is_active);
			if (active) {
				try {
					const q = new URLSearchParams({ profile_id: active.id });
					const resp = await fetchApi<
						ApiWrapper<{
							profile_id: string;
							profile_name: string;
							tools: SuitTool[];
						}>
					>(`/api/mcp/profile/tools/list?${q}`);
					const data = extractApiData(resp);
					if (data?.tools) {
						const tools = data.tools.map((tool: SuitTool) => ({
              tool_name: tool.unique_name || tool.name || "",
							server_name: tool.server_name || "",
							is_enabled: tool.is_enabled ?? tool.enabled ?? true,
							description: tool.description || "",
							tool_id: tool.id || "",
						}));
						return { tools };
					}
				} catch (e) {
					console.error("Failed to fetch profile tools:", e);
				}
			}

			// Fallback to specs endpoint
			const response = await fetchApi<
				Array<{
					name: string;
					tool_name?: string;
					description?: string;
					id?: string;
					server_name?: string;
				}>
			>("/api/mcp/specs/tools");

			const tools = response.map((tool) => {
				let serverName = tool.server_name;
				if (!serverName && tool.description?.includes("server '")) {
					serverName = tool.description.split("server '")[1].split("'")[0];
				}
				const toolName = tool.tool_name || tool.name || "";
				const toolId = tool.id || `${serverName}_${toolName}`;

				return {
					tool_name: toolName,
					server_name: serverName || "",
					is_enabled: true,
					description: tool.description || "",
					tool_id: toolId,
				};
			});

			return { tools };
		} catch (error) {
			console.error("Failed to fetch tools:", error);
			return { tools: [] };
		}
	},

	// Deprecated helpers removed; use configSuitsApi instead

	getTool: (serverId: string, toolName: string) =>
		fetchApi<ToolDetail>(`/api/mcp/specs/tools/${serverId}/${toolName}`),

	updateTool: (
		serverId: string,
		toolName: string,
		config: Partial<ToolDetail>,
	) =>
		fetchApi<ApiResponse<ToolDetail>>(
			`/api/mcp/specs/tools/${serverId}/${toolName}`,
			{
				method: "POST",
				body: JSON.stringify(config),
			},
		),

	// Tool management operations
	_manageTool: async (
		profileId: string,
		toolId: string,
		action: "enable" | "disable",
	) =>
		fetchApi<ApiResponse<null>>("/api/mcp/profile/tools/manage", {
			method: "POST",
			body: JSON.stringify({
				profile_id: profileId,
				component_ids: [toolId],
				action,
			}),
		}),

	enableTool: (profileId: string, toolId: string) =>
		toolsApi._manageTool(profileId, toolId, "enable"),

	disableTool: (profileId: string, toolId: string) =>
		toolsApi._manageTool(profileId, toolId, "disable"),
};

// System Management API
export const systemApi = {
	getReadiness: () =>
		fetchApi<{
			type?: string;
			status?: string;
			reason?: string;
		}>("/api/system/readiness", { logFailure: false }),
	getStatus: () => fetchApi<SystemStatus>("/api/system/status"),
	getMetrics: () => fetchApi<SystemMetrics>("/api/system/metrics"),
	getSettings: async (): Promise<{
		api_port: number;
		mcp_port: number;
		api_url: string;
		mcp_http_url: string;
		first_contact_behavior: "deny" | "review" | "allow" | string;
		onboarding_policy: "auto_manage" | "require_approval" | "manual" | string;
		inspector_timeout_ms: number;
		default_config_mode: "unify" | "hosted" | "transparent" | string;
		default_merge_strategy_override: "deep_merge" | "replace" | null;
	}> => {
		const response = await fetchApi<
			ApiWrapper<{
				api_port: number;
				mcp_port: number;
				api_url: string;
				mcp_http_url: string;
				first_contact_behavior: "deny" | "review" | "allow" | string;
        onboarding_policy:
          "auto_manage" | "require_approval" | "manual" | string;
				inspector_timeout_ms: number;
				default_config_mode: "unify" | "hosted" | "transparent" | string;
				default_merge_strategy_override: "deep_merge" | "replace" | null;
			}>
		>("/api/system/settings");
		return extractApiData(response);
	},
	setSettings: async (payload: {
		api_port?: number;
		mcp_port?: number;
		first_contact_behavior?: "deny" | "review" | "allow" | string;
		inspector_timeout_ms?: number;
		default_config_mode?: "unify" | "hosted" | "transparent" | string;
		default_merge_strategy_override?: "deep_merge" | "replace";
		clear_default_merge_strategy_override?: boolean;
	}) => {
		const response = await fetchApi<
			ApiWrapper<{
				api_port: number;
				mcp_port: number;
				api_url: string;
				mcp_http_url: string;
				first_contact_behavior: "deny" | "review" | "allow" | string;
        onboarding_policy:
          "auto_manage" | "require_approval" | "manual" | string;
				inspector_timeout_ms: number;
				default_config_mode: "unify" | "hosted" | "transparent" | string;
				default_merge_strategy_override: "deep_merge" | "replace" | null;
			}>
		>("/api/system/settings", {
			method: "POST",
			body: JSON.stringify(payload),
		});
		return extractApiData(response);
	},

	shutdown: () =>
		fetchApi<{ status: string; message?: string }>("/api/system/shutdown", {
			method: "POST",
		}),

	restart: () =>
		fetchApi<{ status: string; message?: string; mcp_port?: number }>(
			"/api/system/restart",
			{ method: "POST" },
		),
};

// Runtime Management API
export const runtimeApi = {
	getStatus: async (): Promise<RuntimeStatusResponse> => {
		const response = await fetchApi<ApiWrapper<RuntimeStatusResponse>>(
			"/api/runtime/status",
		);
		return extractApiData(response);
	},

	getCache: async (): Promise<RuntimeCacheResponse> => {
		const response =
			await fetchApi<ApiWrapper<RuntimeCacheResponse>>("/api/runtime/cache");
		return extractApiData(response);
	},

	async resetCache(
		cacheType?: "all" | "uv" | "bun" | "node",
	): Promise<ClearCacheResponse> {
		const response = await fetchApi<ApiWrapper<ClearCacheResponse>>(
			"/api/runtime/cache/reset",
			{
				method: "POST",
				body: JSON.stringify({ cache_type: cacheType || "all" }),
			},
		);
		const data = extractApiData(response);
		if (!data.success) {
			throw new Error("Runtime cache reset failed");
		}
		return data;
	},

	async install(req: InstallRuntimeRequest): Promise<InstallResponse> {
		const response = await fetchApi<ApiWrapper<InstallResponse>>(
			"/api/runtime/install",
			{
				method: "POST",
				body: JSON.stringify(req),
			},
		);
		const data = extractApiData(response);
		if (!data.success) {
			throw new Error(data.message || "Runtime installation failed");
		}
		return data;
	},
};

// Capabilities Cache API
export const capabilitiesApi = {
	getStats: async (): Promise<CapabilitiesStatsResponse> => {
		const response = await fetchApi<
			ApiWrapper<{
				storage: CapabilitiesStorageStats;
				metrics: CapabilitiesMetricsStats;
				generated_at: string;
			}>
		>("/api/mcp/servers/cache/detail?view=stats");

		const data = extractApiData(response);
		return {
			storage: data.storage,
			metrics: data.metrics,
			generatedAt: data.generated_at,
		};
	},

	getKeys: (params?: { limit?: number; offset?: number; search?: string }) => {
		const q = new URLSearchParams({ view: "keys" });
		if (params?.limit != null) q.set("limit", String(params.limit));
		if (params?.offset != null) q.set("offset", String(params.offset));
		if (params?.search) q.set("search", params.search);
		return fetchApi<CapabilitiesKeysResponse>(
			`/api/mcp/servers/cache/detail?${q}`,
		);
	},

	reset: () =>
		fetchApi<ClearCacheResponse>("/api/mcp/servers/cache/reset", {
			method: "POST",
		}),
};

// Configuration Management API
export const configApi = {
	getCurrentConfig: async (): Promise<MCPConfig> => {
		try {
			return await fetchApi<MCPConfig>("/api/config/current");
		} catch (error) {
			console.warn(
				"Config API not available, building from available data:",
				error,
			);

			// Build config from available APIs
			const config: MCPConfig = {
				servers: [],
				tools: [],
				global_settings: {
					max_concurrent_connections: 10,
					request_timeout_ms: 30000,
					enable_metrics: true,
					log_level: "info",
				},
			};

			// Try to populate with real data
			try {
				const serversResponse = await serversApi.getAll();
				if (Array.isArray(serversResponse?.servers)) {
					config.servers = serversResponse.servers.map((server) => ({
						name: server.name,
            kind: server.server_type === "stdio" ? "stdio" : "streamable_http",
						command: "",
						command_path: undefined,
						args: [],
						env: {},
						max_instances: 1,
						retry_policy: {
							max_attempts: 3,
							initial_delay_ms: 1000,
							max_delay_ms: 10000,
							backoff_multiplier: 1.5,
						},
					}));
				}
			} catch (e) {
				console.error("Failed to fetch servers for config:", e);
			}

			try {
				const toolsResponse = await toolsApi.getAll();
				if (Array.isArray(toolsResponse?.tools)) {
					config.tools = toolsResponse.tools.map((tool) => ({
						name: tool.tool_name,
						server_name: tool.server_name ?? "",
						is_enabled: tool.is_enabled,
						settings: {},
					}));
				}
			} catch (e) {
				console.error("Failed to fetch tools for config:", e);
			}

			return config;
		}
	},

	updateConfig: async (config: MCPConfig) => {
		try {
			return await fetchApi<ApiResponse<MCPConfig>>("/api/config", {
				method: "POST",
				body: JSON.stringify(config),
			});
		} catch (error) {
			console.warn("Config API not available, using mock response:", error);
			return {
				status: "success",
				message: "Configuration updated (mock)",
				data: config,
			};
		}
	},

	// Preset management with simplified mock fallback
	getPresets: async () => {
		try {
			return await fetchApi<ConfigPreset[]>("/api/config/presets");
		} catch (error) {
			console.warn("Config API not available, using mock data:", error);
			return [
				{
					id: "default",
					name: "Default Configuration",
					description: "Default system configuration",
					created_at: new Date().toISOString(),
					updated_at: new Date().toISOString(),
					is_active: true,
					config: await configApi.getCurrentConfig(),
				},
			];
		}
	},

	getPreset: async (id: string) => {
		try {
			return await fetchApi<ConfigPreset>(`/api/config/presets/${id}`);
		} catch (error) {
			console.warn("Config API not available, using mock data:", error);
			const presets = await configApi.getPresets();
			const preset = presets.find((p) => p.id === id);
			if (!preset) throw new Error(`Preset with ID ${id} not found`);
			return preset;
		}
	},

	createPreset: (
		preset: Omit<ConfigPreset, "id" | "created_at" | "updated_at">,
	) =>
		fetchApi<ApiResponse<ConfigPreset>>("/api/config/presets", {
			method: "POST",
			body: JSON.stringify(preset),
		}),

	updatePreset: (id: string, preset: Partial<ConfigPreset>) =>
		fetchApi<ApiResponse<ConfigPreset>>(`/api/config/presets/${id}`, {
			method: "PUT",
			body: JSON.stringify(preset),
		}),

	deletePreset: (id: string) =>
		fetchApi<ApiResponse<null>>(`/api/config/presets/${id}`, {
			method: "DELETE",
		}),

	applyPreset: (id: string) =>
		fetchApi<ApiResponse<null>>(`/api/config/presets/${id}/apply`, {
			method: "POST",
		}),
};

type ProfileCapabilityWire<T extends { id: string }> = Omit<T, "id"> & {
	ref_id: string;
};

function profileCapabilityFromWire<T extends { ref_id: string }>(
	item: T,
): Omit<T, "ref_id"> & { id: string } {
	const { ref_id, ...capability } = item;
	return { ...capability, id: ref_id };
}

// Config Suits Management API
export const configSuitsApi = {
	getAll: async (): Promise<ConfigSuitListResponse> => {
		const response = await fetchApi<
			ApiWrapper<{
				profile: ProfileApiRow[];
				total: number;
				timestamp: string;
			}>
		>("/api/mcp/profile/list");
		const data = extractApiData(response);
		return { suits: data.profile.map(profileRowToConfigSuit) };
	},

	getSuit: async (id: string): Promise<ConfigSuit> => {
		const q = new URLSearchParams({ id });
		const response = await fetchApi<
			ApiWrapper<{
				profile: ProfileApiRow;
			}>
		>(
			`/api/mcp/profile/details?${q}`,
		);
		const data = extractApiData(response);
		if (!data.profile) {
			throw new Error("Profile details response missing profile");
		}
		return profileRowToConfigSuit(data.profile);
	},

	getAuthoringView: async (id: string): Promise<ProfileAuthoringView> => {
		const query = new URLSearchParams({ id });
		const response = await fetchApi<
		ApiWrapper<{
			profile: ProfileApiRow;
			server_ids: string[];
			profile_mode?: ProfileMode;
			skill_name?: string | null;
		}>
		>(`/api/mcp/profile/authoring/view?${query}`);
		const data = extractApiData(response);
		return {
			profile: profileRowToConfigSuit(data.profile),
			server_ids: data.server_ids,
			profile_mode: data.profile_mode,
			skill_name: data.skill_name,
		};
	},

	saveAuthoring: async (
		request: ProfileAuthoringSaveRequest,
	): Promise<ProfileAuthoringSaveResponse> => {
		const response = await fetchApi<
			ApiWrapper<{ profile: ProfileApiRow; profile_mode?: ProfileMode }>
		>(
			"/api/mcp/profile/authoring/save",
			{
				method: "POST",
				body: JSON.stringify(request),
			},
		);
		const data = extractApiData(response);
		return {
			profile: profileRowToConfigSuit(data.profile),
			profile_mode: data.profile_mode,
		};
	},

	getWorkflowSpecification: async (
		profileId: string,
	): Promise<WorkflowSpecification> => {
		const query = new URLSearchParams({ id: profileId });
		const response = await fetchApi<ApiWrapper<{ specification: WorkflowSpecification }>>(
			`/api/mcp/profile/workflow/specification/view?${query}`,
		);
		return extractApiData(response).specification;
	},

	getWorkflowSpecificationPreview: async (
		profileId: string,
	): Promise<WorkflowSpecificationPreview> => {
		const query = new URLSearchParams({ id: profileId });
		const response = await fetchApi<
			ApiWrapper<{ preview: WorkflowSpecificationPreview }>
		>(`/api/mcp/profile/workflow/specification/preview?${query}`);
		return extractApiData(response).preview;
	},

	getWorkflowGuide: async (profileId: string): Promise<WorkflowGuide> => {
		const query = new URLSearchParams({ id: profileId });
		const response = await fetchApi<ApiWrapper<{ guide: WorkflowGuide }>>(
			`/api/mcp/profile/workflow/guide/view?${query}`,
		);
		return extractApiData(response).guide;
	},

	saveWorkflowGuide: async (
		request: WorkflowGuideSaveRequest,
	): Promise<WorkflowGuideSaveResponse> => {
		const response = await fetchApi<ApiWrapper<WorkflowGuideSaveResponse>>(
			"/api/mcp/profile/workflow/guide/save",
			{ method: "POST", body: JSON.stringify(request) },
		);
		return extractApiData(response);
	},

	previewWorkflowGuide: async (
		request: WorkflowGuidePreviewRequest,
	): Promise<WorkflowGuidePreviewResponse> => {
		const response = await fetchApi<ApiWrapper<WorkflowGuidePreviewResponse>>(
			"/api/mcp/profile/workflow/guide/preview",
			{ method: "POST", body: JSON.stringify(request) },
		);
		return extractApiData(response);
	},

	getWorkflowGuideExternalDocument: async (
		profileId: string,
		packageFileId: string,
	): Promise<WorkflowGuideExternalDocument> => {
		const query = new URLSearchParams({ profile_id: profileId, package_file_id: packageFileId });
		const response = await fetchApi<ApiWrapper<{ document: WorkflowGuideExternalDocument }>>(
			`/api/mcp/profile/workflow/guide/external-document?${query}`,
		);
		return extractApiData(response).document;
	},

	uploadWorkflowGuidePackageFile: async (formData: FormData): Promise<WorkflowGuidePackageFileSaveResponse> => {
		const response = await fetchApi<ApiWrapper<WorkflowGuidePackageFileSaveResponse>>(
			"/api/mcp/profile/workflow/guide/package-files/upload",
			{ method: "POST", body: formData },
		);
		return extractApiData(response);
	},

	deleteWorkflowGuidePackageFile: async (
		request: WorkflowGuidePackageFileDeleteRequest,
	): Promise<WorkflowGuideSaveResponse> => {
		const response = await fetchApi<ApiWrapper<WorkflowGuideSaveResponse>>(
			"/api/mcp/profile/workflow/guide/package-files/delete",
			{ method: "DELETE", body: JSON.stringify(request) },
		);
		return extractApiData(response);
	},

	repairWorkflowGuide: async (profileId: string): Promise<WorkflowGuideSaveResponse> => {
		const response = await fetchApi<ApiWrapper<WorkflowGuideSaveResponse>>(
			"/api/mcp/profile/workflow/guide/repair",
			{ method: "POST", body: JSON.stringify({ profile_id: profileId }) },
		);
		return extractApiData(response);
	},

	deleteSuit: async (
		id: string,
		expectedAuthoringGeneration: number,
	): Promise<ApiResponse<void>> => {
		const response = await fetchApi<ApiWrapper<void>>(
			`/api/mcp/profile/delete`,
			{
				method: "DELETE",
				body: JSON.stringify({
					id,
					expected_authoring_generation: expectedAuthoringGeneration,
				}),
			},
		);
		extractApiData(response);
		return {
			status: "success",
			message: "Config suit deleted successfully",
		};
	},

	// Suit management operations
	_manageSuits: async (
		expectedAuthoringGenerations: Record<string, number>,
		action: "activate" | "deactivate",
	) => {
		const ids = Object.keys(expectedAuthoringGenerations);
		const response = await fetchApi<ApiWrapper<unknown>>(
			"/api/mcp/profile/manage",
			{
				method: "POST",
				body: JSON.stringify({
					ids,
					action,
					expected_authoring_generations: expectedAuthoringGenerations,
				}),
			},
		);
		extractApiData(response);
		return {
			status: "success",
			message: `Config suit ${action}d successfully`,
			data: null,
		};
	},

	activateSuit: (id: string, expectedAuthoringGeneration: number) =>
		configSuitsApi._manageSuits(
			{ [id]: expectedAuthoringGeneration },
			"activate",
		),
	deactivateSuit: (id: string, expectedAuthoringGeneration: number) =>
		configSuitsApi._manageSuits(
			{ [id]: expectedAuthoringGeneration },
			"deactivate",
		),

	// Batch operations
	batchActivate: (expectedAuthoringGenerations: Record<string, number>) =>
		configSuitsApi._manageSuits(expectedAuthoringGenerations, "activate"),
	batchDeactivate: (expectedAuthoringGenerations: Record<string, number>) =>
		configSuitsApi._manageSuits(expectedAuthoringGenerations, "deactivate"),

	// Suit content management
	getServers: async (suitId: string): Promise<ConfigSuitServersResponse> => {
		const q = new URLSearchParams({ profile_id: suitId });
		const response = await fetchApi<
			ApiWrapper<{
				profile_id: string;
				profile_name: string;
				servers: ConfigSuitServer[];
				authoring_generation: number;
			}>
		>(`/api/mcp/profile/servers/list?${q}`);
		const data = extractApiData(response);
		return {
			suit_id: data.profile_id,
			suit_name: data.profile_name,
			servers: data.servers || [],
			authoring_generation: data.authoring_generation,
		};
	},

	getTools: async (suitId: string): Promise<ConfigSuitToolsResponse> => {
		const q = new URLSearchParams({ profile_id: suitId });
		const response = await fetchApi<
			ApiWrapper<{
				profile_id: string;
				profile_name: string;
				tools: ProfileCapabilityWire<ConfigSuitTool>[];
				source_revision_set: CatalogRevisionSet;
				authoring_generation: number;
			}>
		>(`/api/mcp/profile/tools/list?${q}`);
		const data = extractApiData(response);
		return {
			suit_id: data.profile_id,
			suit_name: data.profile_name,
			tools: (data.tools || []).map(profileCapabilityFromWire),
			source_revision_set: data.source_revision_set,
			authoring_generation: data.authoring_generation,
		};
	},

	getResources: async (
		suitId: string,
	): Promise<ConfigSuitResourcesResponse> => {
		const q = new URLSearchParams({ profile_id: suitId });
		const response = await fetchApi<
			ApiWrapper<{
				profile_id: string;
				profile_name: string;
				resources: ProfileCapabilityWire<ConfigSuitResource>[];
				source_revision_set: CatalogRevisionSet;
				authoring_generation: number;
			}>
		>(`/api/mcp/profile/resources/list?${q}`);
		const data = extractApiData(response);
		return {
			suit_id: data.profile_id,
			suit_name: data.profile_name,
			resources: (data.resources || []).map(profileCapabilityFromWire),
			source_revision_set: data.source_revision_set,
			authoring_generation: data.authoring_generation,
		};
	},

	getPrompts: async (suitId: string): Promise<ConfigSuitPromptsResponse> => {
		const q = new URLSearchParams({ profile_id: suitId });
		const response = await fetchApi<
			ApiWrapper<{
				profile_id: string;
				profile_name: string;
				prompts: ProfileCapabilityWire<ConfigSuitPrompt>[];
				source_revision_set: CatalogRevisionSet;
				authoring_generation: number;
			}>
		>(`/api/mcp/profile/prompts/list?${q}`);
		const data = extractApiData(response);
		return {
			suit_id: data.profile_id,
			suit_name: data.profile_name,
			prompts: (data.prompts || []).map(profileCapabilityFromWire),
			source_revision_set: data.source_revision_set,
			authoring_generation: data.authoring_generation,
		};
	},

	getResourceTemplates: async (
		suitId: string,
	): Promise<ConfigSuitResourceTemplatesResponse> => {
		const q = new URLSearchParams({ profile_id: suitId });
		const response = await fetchApi<
			ApiWrapper<{
				profile_id: string;
				profile_name: string;
				templates: ProfileCapabilityWire<ConfigSuitResourceTemplate>[];
				source_revision_set: CatalogRevisionSet;
				authoring_generation: number;
			}>
		>(`/api/mcp/profile/resource-templates/list?${q}`);
		const data = extractApiData(response);
		return {
			suit_id: data.profile_id,
			suit_name: data.profile_name,
			templates: (data.templates || []).map(profileCapabilityFromWire),
			source_revision_set: data.source_revision_set,
			authoring_generation: data.authoring_generation,
		};
	},

	// Component management operations
	_manageServers: (
		suitId: string,
		componentIds: string[],
		action: "enable" | "disable" | "remove" | "replace",
		expectedAuthoringGeneration: number,
	) =>
		fetchApi<ApiResponse<null>>("/api/mcp/profile/servers/manage", {
			method: "POST",
			body: JSON.stringify({
				profile_id: suitId,
				component_ids: componentIds,
				action,
				expected_authoring_generation: expectedAuthoringGeneration,
			}),
		}),

	_manageCapabilities: (
		endpoint: "tools" | "resources" | "prompts" | "resource-templates" | "capabilities",
		suitId: string,
		componentIds: string[],
		action: "enable" | "disable" | "remove",
		expectedAuthoringGeneration: number,
		sourceRevisionSet: CatalogRevisionSet,
	) =>
		fetchApi<ApiResponse<null>>(`/api/mcp/profile/${endpoint}/manage`, {
			method: "POST",
			body: JSON.stringify({
				profile_id: suitId,
				component_ids: componentIds,
				action,
				expected_authoring_generation: expectedAuthoringGeneration,
				source_revision_set: sourceRevisionSet,
			}),
		}),
	bulkCapabilities: (
		suitId: string,
		componentIds: string[],
		action: "enable" | "disable",
		expectedAuthoringGeneration: number,
		sourceRevisionSet: CatalogRevisionSet,
	) => configSuitsApi._manageCapabilities(
		"capabilities",
		suitId,
		componentIds,
		action,
		expectedAuthoringGeneration,
		sourceRevisionSet,
	),

	enableServer: (suitId: string, serverId: string, expectedAuthoringGeneration: number) =>
		configSuitsApi._manageServers(suitId, [serverId], "enable", expectedAuthoringGeneration),
	disableServer: (suitId: string, serverId: string, expectedAuthoringGeneration: number) =>
		configSuitsApi._manageServers(suitId, [serverId], "disable", expectedAuthoringGeneration),
	removeServer: (suitId: string, serverId: string, expectedAuthoringGeneration: number) =>
		configSuitsApi._manageServers(suitId, [serverId], "remove", expectedAuthoringGeneration),
	bulkServers: async (
		suitId: string,
		ids: string[],
		action: "enable" | "disable" | "remove" | "replace",
		expectedAuthoringGeneration: number,
	) => configSuitsApi._manageServers(suitId, ids, action, expectedAuthoringGeneration),
	enableTool: (suitId: string, toolId: string, expectedAuthoringGeneration: number, revisions: CatalogRevisionSet) =>
		configSuitsApi._manageCapabilities("tools", suitId, [toolId], "enable", expectedAuthoringGeneration, revisions),
	disableTool: (suitId: string, toolId: string, expectedAuthoringGeneration: number, revisions: CatalogRevisionSet) =>
		configSuitsApi._manageCapabilities("tools", suitId, [toolId], "disable", expectedAuthoringGeneration, revisions),
	bulkTools: (
		suitId: string,
		ids: string[],
		action: "enable" | "disable",
		expectedAuthoringGeneration: number,
		revisions: CatalogRevisionSet,
	) => configSuitsApi._manageCapabilities("tools", suitId, ids, action, expectedAuthoringGeneration, revisions),
	enableResource: (suitId: string, resourceId: string, expectedAuthoringGeneration: number, revisions: CatalogRevisionSet) =>
		configSuitsApi._manageCapabilities("resources", suitId, [resourceId], "enable", expectedAuthoringGeneration, revisions),
	disableResource: (suitId: string, resourceId: string, expectedAuthoringGeneration: number, revisions: CatalogRevisionSet) =>
		configSuitsApi._manageCapabilities("resources", suitId, [resourceId], "disable", expectedAuthoringGeneration, revisions),
	bulkResources: (
		suitId: string,
		ids: string[],
		action: "enable" | "disable",
		expectedAuthoringGeneration: number,
		revisions: CatalogRevisionSet,
	) => configSuitsApi._manageCapabilities("resources", suitId, ids, action, expectedAuthoringGeneration, revisions),
	enableResourceTemplate: (suitId: string, id: string, expectedAuthoringGeneration: number, revisions: CatalogRevisionSet) =>
		configSuitsApi._manageCapabilities("resource-templates", suitId, [id], "enable", expectedAuthoringGeneration, revisions),
	disableResourceTemplate: (suitId: string, id: string, expectedAuthoringGeneration: number, revisions: CatalogRevisionSet) =>
		configSuitsApi._manageCapabilities(
      "resource-templates",
      suitId,
		[id],
      "disable",
		expectedAuthoringGeneration,
      revisions,
    ),
	bulkResourceTemplates: (
		suitId: string,
		ids: string[],
		action: "enable" | "disable",
		expectedAuthoringGeneration: number,
		revisions: CatalogRevisionSet,
	) =>
		configSuitsApi._manageCapabilities(
			"resource-templates",
			suitId,
			ids,
			action,
			expectedAuthoringGeneration,
			revisions,
		),
	enablePrompt: (suitId: string, promptId: string, expectedAuthoringGeneration: number, revisions: CatalogRevisionSet) =>
		configSuitsApi._manageCapabilities("prompts", suitId, [promptId], "enable", expectedAuthoringGeneration, revisions),
	disablePrompt: (suitId: string, promptId: string, expectedAuthoringGeneration: number, revisions: CatalogRevisionSet) =>
		configSuitsApi._manageCapabilities("prompts", suitId, [promptId], "disable", expectedAuthoringGeneration, revisions),
	bulkPrompts: (
		suitId: string,
		ids: string[],
		action: "enable" | "disable",
		expectedAuthoringGeneration: number,
		revisions: CatalogRevisionSet,
	) => configSuitsApi._manageCapabilities("prompts", suitId, ids, action, expectedAuthoringGeneration, revisions),
};

export async function addServersToProfile(
	profileId: string,
	serverIds: string[],
): Promise<ProfileAuthoringSaveResponse | null> {
	if (serverIds.length === 0) {
		return null;
	}
	const view = await configSuitsApi.getAuthoringView(profileId);
	const nextServerIds = Array.from(
		new Set([...view.server_ids, ...serverIds]),
	);
	if (nextServerIds.length === view.server_ids.length) {
		return null;
	}
	const profile = view.profile;
	return configSuitsApi.saveAuthoring({
		id: profile.id,
		expected_authoring_generation: profile.authoring_generation,
		name: profile.name,
		description: profile.description ?? null,
		profile_type: profile.suit_type,
		priority: profile.priority,
		is_active: profile.is_active,
		is_default: profile.is_default,
		server_ids: nextServerIds,
		clone_from_id: null,
	});
}

export async function associateImportedServersWithProfile(
	profileId: string | null,
	importedServerNames: string[],
): Promise<ProfileAuthoringSaveResponse | null> {
	if (!profileId || importedServerNames.length === 0) {
		return null;
	}
	const importedNames = new Set(importedServerNames);
	const matches = new Map<string, string[]>();
	const pageLimit = 100;
	let offset = 0;
	while (true) {
		const { servers } = await serversApi.getAll({ limit: pageLimit, offset });
		for (const server of servers) {
			if (!importedNames.has(server.name)) {
				continue;
			}
			const ids = matches.get(server.name) ?? [];
			ids.push(server.id);
			matches.set(server.name, ids);
		}
		const ambiguous = Array.from(matches.values()).some(
			(ids) => ids.length > 1,
		);
		if (ambiguous) {
			throw new ProfileAssociationError("imported_server_ambiguous");
		}
		if (
			Array.from(importedNames).every((name) => matches.has(name)) ||
			servers.length < pageLimit
		) {
			break;
		}
		offset += pageLimit;
	}
	const serverIds = Array.from(importedNames).flatMap(
		(name) => matches.get(name) ?? [],
	);
	if (serverIds.length !== importedNames.size) {
		throw new ProfileAssociationError("imported_server_missing");
	}
	return addServersToProfile(profileId, serverIds);
}

export async function completeServerImportForProfile(
	profileId: string | null,
	stats: ImportStats,
): Promise<void> {
	await associateImportedServersWithProfile(profileId, stats.importedServers);
	assertCompleteServerImport(stats);
}

export type ProfileAssociationErrorCode =
	| "imported_server_missing"
	| "imported_server_ambiguous";

export class ProfileAssociationError extends Error {
	readonly code: ProfileAssociationErrorCode;

	constructor(code: ProfileAssociationErrorCode) {
		super(code);
		this.name = "ProfileAssociationError";
		this.code = code;
	}
}

// WebSocket Notifications Service
export class NotificationsService {
	private ws: WebSocket | null = null;
	private listeners: Map<string, ((data: NotificationData) => void)[]> =
		new Map();
	private reconnectAttempts = 0;
	private readonly maxReconnectAttempts = 5;
	private readonly reconnectDelay = 5000;
	/** When true, the next `onclose` reconnects immediately to the current `API_BASE_URL` (no backoff). */
	private reconnectToNewBasePending = false;

	private hasActiveListeners(): boolean {
		for (const callbacks of this.listeners.values()) {
			if (callbacks.length > 0) {
				return true;
			}
		}
		return false;
	}

	connect() {
		if (this.ws) return;
		if (!this.hasActiveListeners()) return;

		const wsUrl = resolveWebSocketUrl();
		if (!wsUrl) {
			console.warn(
				"WebSocket URL could not be resolved; skipping connection attempt",
			);
			return;
		}
		console.log(`Connecting to WebSocket at ${wsUrl}`);

		try {
			this.ws = new WebSocket(wsUrl);

			this.ws.onopen = () => {
				console.log("WebSocket connection established");
				this.reconnectAttempts = 0;
			};

			this.ws.onmessage = (event) => {
				try {
					const data = JSON.parse(event.data) as NotificationData;
					if (data.event) {
						const eventListeners = this.listeners.get(data.event) || [];
						eventListeners.forEach((listener) => listener(data));
					}
				} catch (error) {
					console.error("Error parsing WebSocket message:", error);
				}
			};

			this.ws.onerror = (error) => {
				console.error("WebSocket error:", error);
			};

			this.ws.onclose = (event) => {
				console.log(
					`WebSocket connection closed: ${event.code} ${event.reason}`,
				);
				this.ws = null;

				if (!this.hasActiveListeners()) {
					this.reconnectToNewBasePending = false;
					this.reconnectAttempts = 0;
					return;
				}

				if (this.reconnectToNewBasePending) {
					this.reconnectToNewBasePending = false;
					this.reconnectAttempts = 0;
					queueMicrotask(() => this.connect());
					return;
				}

				if (this.reconnectAttempts < this.maxReconnectAttempts) {
					const delay = this.reconnectDelay * 1.5 ** this.reconnectAttempts;
					this.reconnectAttempts++;
					console.log(
						`Attempting to reconnect in ${delay}ms (attempt ${this.reconnectAttempts}/${this.maxReconnectAttempts})`,
					);
					setTimeout(() => this.connect(), delay);
				} else {
					console.error(
						`Failed to reconnect after ${this.maxReconnectAttempts} attempts`,
					);
				}
			};
		} catch (error) {
			console.error("Error creating WebSocket connection:", error);
		}
	}

	subscribe(event: string, callback: (data: NotificationData) => void) {
		let list = this.listeners.get(event);
		if (!list) {
			list = [];
			this.listeners.set(event, list);
		}
		list.push(callback);

		if (!this.ws) {
			this.connect();
		}

		return () => {
			const eventListeners = this.listeners.get(event) || [];
			const index = eventListeners.indexOf(callback);
			if (index !== -1) {
				eventListeners.splice(index, 1);
			}

			if (eventListeners.length === 0) {
				this.listeners.delete(event);
			}

			if (!this.hasActiveListeners()) {
				this.disconnect();
			}
		};
	}

	disconnect() {
		if (this.ws) {
			this.ws.close();
			this.ws = null;
		}
	}

	/**
	 * Use after `setApiBaseUrl` (e.g. desktop Settings port change) so notifications use the new host
	 * without waiting for reconnect backoff or being blocked by `if (this.ws) return`.
	 */
	reconnectAfterApiBaseChanged() {
		if (this.ws) {
			this.reconnectToNewBasePending = true;
			this.ws.close();
			return;
		}
		this.reconnectAttempts = 0;
		this.connect();
	}
}

export const notificationsService = new NotificationsService();

// Clients Management API
export const clientsApi = {
	inspectClientConfigFileParse: async (
		req: ClientConfigFileParseInspectCall,
	): Promise<ClientConfigFileParseInspectResp> => {
		const { inspectTarget, ...body } = req;
		const path =
			inspectTarget === "path"
				? "/api/client/config-file-parse/inspect"
				: "/api/client/config-file-parse/inspect-existing";
    const resp = await fetchApi<ApiWrapper<ClientConfigFileParseInspectResp>>(
      path,
      {
			method: "POST",
			body: JSON.stringify(body),
      },
    );
		return extractApiData(resp);
	},
	getCapabilityConfig: async (identifier: string) => {
		const q = new URLSearchParams({ identifier });
		const resp = await fetchApi<ClientCapabilityConfigResp>(
			`/api/client/capability-config?${q}`,
		);
		return extractApiData(resp);
	},

	updateCapabilityConfig: async (payload: ClientCapabilityConfigReq) => {
		const resp = await fetchApi<ClientCapabilityConfigResp>(
			"/api/client/capability-config",
			{
				method: "POST",
				body: JSON.stringify(payload),
			},
		);
		return extractApiData(resp);
	},

	list: async (
		refresh = false,
		options?: { persistDetected?: boolean; includeDetected?: boolean },
	) => {
		const q = new URLSearchParams({ refresh: String(refresh) });
		if (typeof options?.persistDetected === "boolean") {
			q.set("persist_detected", String(options.persistDetected));
		}
		if (typeof options?.includeDetected === "boolean") {
			q.set("include_detected", String(options.includeDetected));
		}
		const resp = await fetchApi<ClientCheckResp>(`/api/client/list?${q}`);
		return extractApiData(resp);
	},

	detect: async (refresh = false) => {
		const q = new URLSearchParams({ refresh: String(refresh) });
		const resp = await fetchApi<ClientCheckResp>(`/api/client/detect?${q}`);
		return extractApiData(resp);
	},

	deleteRecord: async (identifier: string) => {
		const resp = await fetchApi<ClientDeleteResp>("/api/client/delete", {
			method: "POST",
			body: JSON.stringify({ identifier }),
		});
		return extractApiData(resp);
	},

	approveRecord: async (payload: ClientRecordReviewReq) => {
		return fetchApi<ClientRecordLifecycleResp>("/api/client/manage/approve", {
			method: "POST",
			body: JSON.stringify(payload),
		});
	},

	suspendRecord: async (payload: ClientRecordReviewReq) => {
		return fetchApi<ClientRecordLifecycleResp>("/api/client/manage/suspend", {
			method: "POST",
			body: JSON.stringify(payload),
		});
	},

	detach: async (identifier: string) => {
		const resp = await fetchApi<ClientDetachResp>("/api/client/detach", {
			method: "POST",
			body: JSON.stringify({ identifier }),
		});
		return extractApiData(resp);
	},

	attach: async (identifier: string) => {
		const resp = await fetchApi<ClientAttachResp>("/api/client/attach", {
			method: "POST",
			body: JSON.stringify({ identifier }),
		});
		return extractApiData(resp);
	},

	configDetails: async (identifier: string, doImport = false) => {
		const q = new URLSearchParams({ identifier, import: String(doImport) });
		const resp = await fetchApi<ClientConfigResp>(
			`/api/client/config/details?${q}`,
		);
		return extractApiData(resp);
	},

	update: async (payload: {
		identifier: string;
		display_name?: string;
		transport?: string;
		config_file_state?: ClientConfigFileState;
		config_path?: string;
		client_version?: string;
		description?: string;
		homepage_url?: string;
		docs_url?: string;
		support_url?: string;
		logo_url?: string;
		config_file_parse?: ClientConfigFileParse;
		clear_config_file_parse?: boolean;
		merge_strategy_override?: ClientMergeStrategy;
		clear_merge_strategy_override?: boolean;
		transports?: Record<string, TransportRuleData>;
		clear_transports?: boolean;
	}) => {
		const resp = await fetchApi<ClientSettingsUpdateResp>(`/api/client/update`, {
			method: "POST",
			body: JSON.stringify(payload),
		});
		return resp;
	},

	applyConfig: async (payload: ClientConfigUpdateReq) => {
		const resp = await fetchApi<ClientConfigUpdateResp>(
			`/api/client/config/apply`,
			{
				method: "POST",
				body: JSON.stringify(payload),
			},
		);
		return extractApiData(resp);
	},

	restoreConfig: async (payload: ClientConfigRestoreReq) => {
		const resp = await fetchApi<ClientBackupActionResp>(
			`/api/client/config/restore`,
			{
				method: "POST",
				body: JSON.stringify(payload),
			},
		);
		return extractApiData(resp);
	},

	listBackups: async (identifier?: string) => {
		const q = new URLSearchParams();
		if (identifier) q.set("identifier", identifier);
		const resp = await fetchApi<ClientBackupListResp>(
			`/api/client/backups/list?${q}`,
		);
		return extractApiData(resp);
	},

	deleteBackup: async (identifier: string, backup: string) => {
		const resp = await fetchApi<ClientBackupActionResp>(
			`/api/client/backups/delete`,
			{
				method: "POST",
				body: JSON.stringify({ identifier, backup }),
			},
		);
		return extractApiData(resp);
	},

	getBackupPolicy: async (identifier: string) => {
		const q = new URLSearchParams({ identifier });
		const resp = await fetchApi<ClientBackupPolicyResp>(
			`/api/client/backups/policy?${q}`,
		);
		return extractApiData(resp);
	},

	setBackupPolicy: async (payload: ClientBackupPolicySetReq) => {
		const resp = await fetchApi<ClientBackupPolicyResp>(
			`/api/client/backups/policy`,
			{
				method: "POST",
				body: JSON.stringify(payload),
			},
		);
		return extractApiData(resp);
	},
};

export const surfaceReviewsApi = {
	list: async (query?: {
		consumerId?: string;
		ownerType?: SurfaceReviewOwnerType;
		ownerId?: string;
		state?: SurfaceReviewLifecycle;
	}): Promise<SurfaceReviewItem[]> => {
		const params = new URLSearchParams();
		if (query?.consumerId) params.set("consumer_id", query.consumerId);
		if (query?.ownerType) params.set("owner_type", query.ownerType);
		if (query?.ownerId) params.set("owner_id", query.ownerId);
		if (query?.state) params.set("state", query.state);
		const suffix = params.size > 0 ? `?${params}` : "";
		const response = await fetchApi<ApiWrapper<{ items: SurfaceReviewItem[] }>>(
			`/api/client/surface/reviews${suffix}`,
		);
		return extractApiData(response).items;
	},

	get: async (reviewItemId: string): Promise<SurfaceReviewItem> => {
		const response = await fetchApi<ApiWrapper<SurfaceReviewItem>>(
			`/api/client/surface/reviews/${encodeURIComponent(reviewItemId)}`,
		);
		return extractApiData(response);
	},

	summary: async (): Promise<SurfaceReviewSummary> => {
		const response = await fetchApi<ApiWrapper<SurfaceReviewSummary>>(
			"/api/client/surface/reviews/summary",
		);
		return extractApiData(response);
	},

	approveTarget: async (
		reviewItemId: string,
		request: SurfaceReviewActionReq,
	): Promise<SurfaceReviewActionData> => {
		const response = await fetchApi<ApiWrapper<SurfaceReviewActionData>>(
			`/api/client/surface/reviews/${encodeURIComponent(reviewItemId)}/approve`,
			{
				method: "POST",
				body: JSON.stringify(request),
			},
		);
		return extractApiData(response);
	},

	rejectTarget: async (
		reviewItemId: string,
		request: SurfaceReviewActionReq,
	): Promise<SurfaceReviewActionData> => {
		const response = await fetchApi<ApiWrapper<SurfaceReviewActionData>>(
			`/api/client/surface/reviews/${encodeURIComponent(reviewItemId)}/reject`,
			{
				method: "POST",
				body: JSON.stringify(request),
			},
		);
		return extractApiData(response);
	},

	previewIntent: async (
		reviewItemId: string,
		request: SurfaceIntentPreviewReq,
	): Promise<SurfaceIntentPreviewData> => {
		const response = await fetchApi<ApiWrapper<SurfaceIntentPreviewData>>(
			`/api/client/surface/reviews/${encodeURIComponent(reviewItemId)}/resolve-intent/preview`,
			{
				method: "POST",
				body: JSON.stringify(request),
			},
		);
		return extractApiData(response);
	},

	resolveIntent: async (
		reviewItemId: string,
		request: SurfaceIntentResolveReq,
	): Promise<SurfaceReviewActionData> => {
		const response = await fetchApi<ApiWrapper<SurfaceReviewActionData>>(
			`/api/client/surface/reviews/${encodeURIComponent(reviewItemId)}/resolve-intent`,
			{
				method: "POST",
				body: JSON.stringify(request),
			},
		);
		return extractApiData(response);
	},
};

// Token Estimate API
export const tokenEstimateApi = {
  getEstimate: async (
    profileId: string,
    capabilitySource?: string,
  ): Promise<TokenEstimateResponse> => {
		const params = new URLSearchParams({ profile_id: profileId });
		if (capabilitySource) {
			params.append("capability_source", capabilitySource);
		}
		const url = `/api/mcp/profile/token-estimate?${params}`;
		return fetchApi<TokenEstimateResponse>(url);
	},
};

/** Full capability JSON per profile row for client-side cl100k counting. */
export const capabilityTokenLedgerApi = {
	get: async (profileId: string): Promise<CapabilityTokenLedgerResponse> => {
		const params = new URLSearchParams({ profile_id: profileId });
		return fetchApi<CapabilityTokenLedgerResponse>(
			`/api/mcp/profile/capability-token-ledger?${params}`,
		);
	},
};

// React Query hook for token estimates
export function useTokenEstimate(
	profileId: string | undefined,
	capabilitySource?: string,
	refreshKey?: string,
) {
	return useQuery({
		queryKey: ["tokenEstimate", profileId, capabilitySource, refreshKey],
		queryFn: () => {
			if (!profileId) return Promise.resolve(null);
			return tokenEstimateApi.getEstimate(profileId, capabilitySource);
		},
		enabled: !!profileId,
		// Profile header / detail must reflect enable toggles immediately; server is cheap and authoritative.
		staleTime: 0,
		retry: 1,
	});
}

/**
 * Ledger JSON for client tokenizer; if the route is missing (404, older proxy), falls back to
 * GET token-estimate keyed by capability enable fingerprint so toggles still refresh the chart.
 */
export function useProfileTokenChartSource(
	profileId: string | undefined,
	enabledByComponentId: ReadonlyMap<string, boolean>,
) {
	const capabilityFingerprint = useMemo(
		() =>
			[...enabledByComponentId.entries()]
				.sort(([a], [b]) => a.localeCompare(b))
				.map(([id, on]) => `${id}:${on ? 1 : 0}`)
				.join("|"),
		[enabledByComponentId],
	);

	const ledgerQuery = useQuery({
		queryKey: ["capabilityTokenLedger", profileId],
		queryFn: async () => {
			if (!profileId) return null;
			return capabilityTokenLedgerApi.get(profileId);
		},
		enabled: !!profileId,
		staleTime: 5 * 60 * 1000,
		refetchOnWindowFocus: false,
		retry: (failureCount, error) => {
			if (isApiNotFoundError(error)) {
				return false;
			}
			return failureCount < 1;
		},
	});

	const ledgerMissingRoute =
		ledgerQuery.isError && isApiNotFoundError(ledgerQuery.error);

	const estimateQuery = useQuery({
		queryKey: ["profileChartTokenEstimate", profileId, capabilityFingerprint],
		queryFn: async () => {
			if (!profileId) return null;
			return tokenEstimateApi.getEstimate(profileId);
		},
		enabled: !!profileId && ledgerMissingRoute,
		staleTime: 0,
		retry: 1,
	});

	const isLoading =
		ledgerQuery.isPending || (ledgerMissingRoute && estimateQuery.isPending);
	const isError =
		(ledgerQuery.isError && !ledgerMissingRoute) ||
		(ledgerMissingRoute && estimateQuery.isError);

	const ledgerItems = ledgerQuery.data?.items;
  const fallbackEstimate = ledgerMissingRoute
    ? (estimateQuery.data ?? null)
    : null;

	return {
		ledgerItems,
		fallbackEstimate,
		isLoading,
		isError,
	};
}

// LLM Provider API
export const llmApi = {
	listProviders: async (): Promise<LlmProviderConfig[]> => {
    const resp = await fetchApi<{
      success: boolean;
      data: LlmProviderConfig[];
    }>("/api/llm/providers");
		return resp.data ?? [];
	},

  createProvider: async (
    input: LlmProviderCreateInput,
  ): Promise<LlmProviderConfig> => {
    const resp = await fetchApi<{ success: boolean; data: LlmProviderConfig }>(
      "/api/llm/providers/create",
      {
			method: "POST",
			body: JSON.stringify(input),
      },
    );
		return resp.data!;
	},

  updateProvider: async (
    input: LlmProviderUpdateInput,
  ): Promise<LlmProviderConfig> => {
    const resp = await fetchApi<{ success: boolean; data: LlmProviderConfig }>(
      "/api/llm/providers/update",
      {
			method: "POST",
			body: JSON.stringify(input),
      },
    );
		return resp.data!;
	},

	deleteProvider: async (id: string): Promise<void> => {
		await fetchApi("/api/llm/providers/delete", {
			method: "POST",
			body: JSON.stringify({ id }),
		});
	},

	testProvider: async (id: string): Promise<LlmConnectivityResult> => {
    const resp = await fetchApi<{
      success: boolean;
      data: LlmConnectivityResult;
    }>("/api/llm/providers/test", {
			method: "POST",
			body: JSON.stringify({ id }),
		});
		return resp.data!;
	},

	listModels: async (id: string): Promise<string[]> => {
    const resp = await fetchApi<{
      success: boolean;
      data: { models: string[] };
    }>("/api/llm/providers/models", {
			method: "POST",
			body: JSON.stringify({ id }),
		});
		return resp.data?.models ?? [];
	},

  listModelsForConfig: async (
    input: LlmProviderModelPreviewInput,
  ): Promise<string[]> => {
    const resp = await fetchApi<{
      success: boolean;
      data: { models: string[] };
    }>("/api/llm/providers/models/preview", {
			method: "POST",
			body: JSON.stringify(input),
		});
		return resp.data?.models ?? [];
	},

	getDefaultProvider: async (): Promise<LlmProviderConfig | null> => {
    const resp = await fetchApi<{
      success: boolean;
      data: LlmProviderConfig | null;
    }>("/api/llm/providers/default");
		return resp.data ?? null;
	},

	setDefaultProvider: async (id: string): Promise<void> => {
		await fetchApi("/api/llm/providers/set-default", {
			method: "POST",
			body: JSON.stringify({ id }),
		});
	},
};
