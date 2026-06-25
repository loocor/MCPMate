import type { ClientConfigFileParse, ClientConfigFileState, TransportRuleData } from "./types";
import { isBoardDemoMode } from "./demo-mode";

export type AdminDiscoverySurface = "onboarding" | "extension";
export type AdminDiscoveryPlatform = "macos" | "windows" | "linux";
export type AdminDiscoveryLocale = "en" | "zh" | "ja";

export function adminDiscoveryLocaleFromI18n(language: string | undefined): AdminDiscoveryLocale {
	const lower = language?.toLowerCase() ?? "";
	if (lower.startsWith("zh")) return "zh";
	if (lower.startsWith("ja")) return "ja";
	return "en";
}

export function adminDiscoveryAcceptLanguage(locale: AdminDiscoveryLocale): string {
	switch (locale) {
		case "zh":
			return "zh-CN,zh;q=0.9,en;q=0.8";
		case "ja":
			return "ja-JP,ja;q=0.9,en;q=0.8";
		default:
			return "en-US,en;q=0.9";
	}
}

const MAX_ADMIN_DISCOVERY_LIMIT = 50;
const MAX_ADMIN_DISCOVERY_RANDOM = 12;
const DEFAULT_ADMIN_DISCOVERY_BASE_URL = "https://public.mcp.umate.ai";
const CANONICAL_TRANSPORT_KEYS = ["streamable_http", "sse", "stdio"] as const;
const CONFIG_PARSE_FORMATS = ["json", "json5", "toml", "yaml"] as const;
type ConfigParseFormat = (typeof CONFIG_PARSE_FORMATS)[number];

export const ADMIN_DISCOVERY_BASE_URL = trimTrailingSlash(
	(typeof import.meta !== "undefined" &&
		import.meta.env?.VITE_ADMIN_API_BASE_URL &&
		String(import.meta.env.VITE_ADMIN_API_BASE_URL).trim()) ||
		DEFAULT_ADMIN_DISCOVERY_BASE_URL,
);

export interface AdminDiscoveryQuery {
	surface?: AdminDiscoverySurface;
	limit?: number;
	offset?: number;
	random?: number;
	platform?: AdminDiscoveryPlatform;
	locale?: string;
}

export interface AdminDiscoveryClientCandidate {
	identifier: string;
	displayName: string;
	configFileChoice: ClientConfigFileState;
	configPath: string;
	configFileParseFormat: ConfigParseFormat;
	configFileParseContainerType: "standard" | "array";
	configFileParseContainerKeysText: string;
	mergeStrategy: "replace" | "deep_merge";
	description: string;
	homepageUrl: string;
	docsUrl: string;
	supportUrl: string;
	logoUrl: string;
	tags: string[];
	supportedTransports: string[];
	transports: Record<string, TransportRuleData>;
}

export interface AdminDiscoveryDiagnostic {
	identifier?: string;
	reason: string;
}

export interface AdminDiscoveryClientCatalog {
	clients: AdminDiscoveryClientCandidate[];
	diagnostics: AdminDiscoveryDiagnostic[];
}

export interface AdminClientUpdatePayload {
	identifier: string;
	display_name: string;
	config_file_state: ClientConfigFileState;
	config_path?: string;
	description?: string;
	homepage_url?: string;
	docs_url?: string;
	support_url?: string;
	logo_url?: string;
	config_file_parse?: ClientConfigFileParse;
	clear_config_file_parse: boolean;
	transports?: Record<string, TransportRuleData>;
	clear_transports: boolean;
}

function demoStdioTransport(): Record<string, TransportRuleData> {
	return {
		stdio: {
			command_field: "command",
			args_field: "args",
			env_field: "env",
			selected: true,
		},
	};
}

function demoClientCatalog(platform?: AdminDiscoveryPlatform): AdminDiscoveryClientCatalog {
	const macos = platform === "macos" || !platform;
	return {
		diagnostics: [],
		clients: [
			{
				identifier: "claude_desktop",
				displayName: "Claude Desktop",
				configFileChoice: "with_config_file",
				configPath: macos
					? "/Users/demo/Library/Application Support/Claude/claude_desktop_config.json"
					: "",
				configFileParseFormat: "json",
				configFileParseContainerType: "standard",
				configFileParseContainerKeysText: "mcpServers",
				description: "Desktop assistant client with a local MCP server config file.",
				homepageUrl: "https://claude.ai/download",
				docsUrl: "https://support.anthropic.com",
				supportUrl: "https://support.anthropic.com",
				logoUrl: "",
				tags: ["desktop", "chat"],
				supportedTransports: ["stdio"],
				transports: demoStdioTransport(),
			},
			{
				identifier: "cursor",
				displayName: "Cursor",
				configFileChoice: "with_config_file",
				configPath: macos ? "/Users/demo/.cursor/mcp.json" : "",
				configFileParseFormat: "json",
				configFileParseContainerType: "standard",
				configFileParseContainerKeysText: "mcpServers",
				description: "Editor workflow with project-local MCP capabilities.",
				homepageUrl: "https://cursor.com",
				docsUrl: "https://docs.cursor.com",
				supportUrl: "https://cursor.com",
				logoUrl: "",
				tags: ["editor", "coding"],
				supportedTransports: ["stdio", "streamable_http"],
				transports: {
					...demoStdioTransport(),
					streamable_http: {
						url_field: "url",
						headers_field: "headers",
					},
				},
			},
		],
	};
}

function demoServerCatalog(): AdminDiscoveryServerCandidate[] {
	return [
		{
			key: "demo-admin:context7",
			name: "Context7",
			kind: "stdio",
			command: "npx",
			args: ["-y", "@upstash/context7-mcp"],
			env: {},
			categories: ["documentation", "coding"],
			tags: ["documentation", "coding"],
			description: "Fetch current library docs and examples for coding workflows.",
			logoUrl: "",
			source_clients: ["MCPMate"],
			source_client_ids: [],
			import_config: {
				type: "stdio",
				command: "npx",
				args: ["-y", "@upstash/context7-mcp"],
				source: { type: "catalog", ref: "context7" },
			},
		},
		{
			key: "demo-admin:browser-tools",
			name: "Browser Tools",
			kind: "stdio",
			command: "npx",
			args: ["-y", "@agentdeskai/browser-tools-mcp"],
			env: {},
			categories: ["browser", "automation"],
			tags: ["browser", "automation"],
			description: "Inspect pages, screenshots, and browser console output from MCP clients.",
			logoUrl: "",
			source_clients: ["MCPMate"],
			source_client_ids: [],
			import_config: {
				type: "stdio",
				command: "npx",
				args: ["-y", "@agentdeskai/browser-tools-mcp"],
				source: { type: "catalog", ref: "browser-tools" },
			},
		},
	];
}

export interface AdminDiscoveryServerCandidate {
	key: string;
	name: string;
	kind: string;
	command?: string;
	args: string[];
	env: Record<string, string>;
	url?: string;
	categories: string[];
	tags: string[];
	description: string;
	logoUrl: string;
	source_clients: string[];
	source_client_ids: string[];
	import_config: Record<string, unknown>;
}

function trimTrailingSlash(url: string): string {
	return url.replace(/\/+$/, "");
}

function compactString(value: unknown): string | undefined {
	return typeof value === "string" && value.trim() ? value.trim() : undefined;
}

function firstCompactString(...values: unknown[]): string | undefined {
	return values.find((value): value is string => Boolean(compactString(value)))?.trim();
}

function recordValue(value: unknown): Record<string, unknown> {
	return value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : {};
}

function optionalRecordValue(value: unknown): Record<string, unknown> | null {
	if (value === undefined) return {};
	if (value === null) return null;
	return value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : null;
}

function stringArrayValue(value: unknown): string[] {
	return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
}

function stringRecordValue(value: unknown): Record<string, string> {
	return Object.fromEntries(
		Object.entries(recordValue(value)).filter((entry): entry is [string, string] => typeof entry[1] === "string"),
	);
}

function cappedCount(value: unknown, max: number): number | undefined {
	if (typeof value !== "number" || !Number.isFinite(value)) return undefined;
	return Math.max(0, Math.min(max, Math.round(value)));
}

export function buildAdminDiscoveryUrl(
	path: "/discovery/clients" | "/discovery/servers",
	options: AdminDiscoveryQuery = {},
	baseUrl = ADMIN_DISCOVERY_BASE_URL,
): string {
	const url = new URL(path, `${trimTrailingSlash(baseUrl)}/`);
	if (options.surface) {
		url.searchParams.set("surface", options.surface);
	}
	const locale = options.locale ? adminDiscoveryLocaleFromI18n(options.locale) : undefined;
	const random = cappedCount(options.random, MAX_ADMIN_DISCOVERY_RANDOM);
	if (typeof random === "number") {
		url.searchParams.set("random", String(random));
		if (locale) {
			url.searchParams.set("locale", locale);
		}
		return url.toString();
	}
	const limit = cappedCount(options.limit, MAX_ADMIN_DISCOVERY_LIMIT);
	if (typeof limit === "number") {
		url.searchParams.set("limit", String(limit));
	}
	if (typeof options.offset === "number" && Number.isFinite(options.offset)) {
		url.searchParams.set("offset", String(Math.max(0, Math.round(options.offset))));
	}
	if (locale) {
		url.searchParams.set("locale", locale);
	}
	return url.toString();
}

async function fetchAdminDiscoveryEnvelope(path: "/discovery/clients" | "/discovery/servers", options: AdminDiscoveryQuery) {
	const locale = options.locale ? adminDiscoveryLocaleFromI18n(options.locale) : undefined;
	const headers: HeadersInit = { Accept: "application/json" };
	if (locale) {
		headers["Accept-Language"] = adminDiscoveryAcceptLanguage(locale);
	}
	const response = await fetch(buildAdminDiscoveryUrl(path, options), {
		credentials: "omit",
		headers,
	});
	if (!response.ok) {
		throw new Error(`Admin discovery request failed with HTTP ${response.status}`);
	}
	return response.json() as Promise<unknown>;
}

function discoveryItems(envelope: unknown, key: "clients" | "servers"): unknown[] {
	const record = recordValue(envelope);
	const items = record[key];
	if (!Array.isArray(items)) {
		throw new Error(`Admin discovery response is missing '${key}' array`);
	}
	return items;
}

export async function fetchAdminDiscoveryClients(options: AdminDiscoveryQuery): Promise<AdminDiscoveryClientCandidate[]> {
	const catalog = await fetchAdminDiscoveryClientCatalog(options);
	if (catalog.diagnostics.length > 0) {
		console.warn("Skipped invalid Admin discovery clients.", catalog.diagnostics);
	}
	return catalog.clients;
}

export async function fetchAdminDiscoveryClientCatalog(options: AdminDiscoveryQuery): Promise<AdminDiscoveryClientCatalog> {
	if (isBoardDemoMode()) {
		return demoClientCatalog(options.platform);
	}

	const envelope = await fetchAdminDiscoveryEnvelope("/discovery/clients", options);
	const clients: AdminDiscoveryClientCandidate[] = [];
	const diagnostics: AdminDiscoveryDiagnostic[] = [];
	for (const item of discoveryItems(envelope, "clients")) {
		const candidate = adminDiscoveryClientToCandidate(item, { platform: options.platform });
		if (candidate) {
			clients.push(candidate);
		} else {
			diagnostics.push({
				identifier: adminDiscoveryClientIdentifier(item),
				reason: "Invalid Admin discovery client contract.",
			});
		}
	}
	return { clients, diagnostics };
}

export async function fetchAdminDiscoveryServers(options: AdminDiscoveryQuery): Promise<AdminDiscoveryServerCandidate[]> {
	if (isBoardDemoMode()) {
		return demoServerCatalog().slice(0, options.limit ?? undefined);
	}

	const envelope = await fetchAdminDiscoveryEnvelope("/discovery/servers", options);
	return discoveryItems(envelope, "servers").flatMap((item) => {
		const candidate = adminDiscoveryServerToOnboardingCandidate(item);
		return candidate ? [candidate] : [];
	});
}

function metadataRecord(client: Record<string, unknown>): Record<string, unknown> {
	return recordValue(client.metadata);
}

function adminDiscoveryClientIdentifier(raw: unknown): string | undefined {
	const client = recordValue(raw);
	return compactString(client.identifier) ?? compactString(client.id) ?? compactString(client.name);
}

function configPathFromDiscoveryClient(
	file: Record<string, unknown>,
	platform?: AdminDiscoveryPlatform,
): string {
	if (!platform) return "";
	return compactString(recordValue(file.paths)[platform]) ?? "";
}

function adminConfigFileParse(file: Record<string, unknown>): {
	format: ConfigParseFormat;
	containerType: "standard" | "array";
	containerKeys: string[];
} | null {
	const format = adminConfigParseFormat(file.format);
	if (!format) return null;
	const container = recordValue(file.container);
	const containerType = compactString(container.type ?? file.containerType ?? file.container_type);
	if (containerType && !["standard", "object_map", "array"].includes(containerType)) return null;
	const containerKeys = stringArrayValue(container.keys ?? file.containerKeys ?? file.container_keys);
	if (containerKeys.length === 0) return null;
	return {
		format,
		containerType: containerType === "array" ? "array" : "standard",
		containerKeys,
	};
}

function adminConfigParseFormat(value: unknown): ConfigParseFormat | null {
	const format = compactString(value) ?? "json";
	return CONFIG_PARSE_FORMATS.find((candidate) => candidate === format) ?? null;
}

function adminMergeStrategy(value: unknown): "replace" | "deep_merge" | null {
	if (value == null) return "replace";
	if (value === "replace" || value === "deep_merge") return value;
	return null;
}

function isCanonicalTransportKey(value: string): boolean {
	return (CANONICAL_TRANSPORT_KEYS as readonly string[]).includes(value);
}

function hasCompactString(value: unknown): boolean {
	return typeof value === "string" && value.trim().length > 0;
}

function isOptionalCompactString(value: unknown): boolean {
	return value === undefined || value === null || hasCompactString(value);
}

function isOptionalBoolean(value: unknown): boolean {
	return value === undefined || value === null || typeof value === "boolean";
}

function isOptionalRecord(value: unknown): boolean {
	return value === undefined || value === null || (typeof value === "object" && value !== null && !Array.isArray(value));
}

function isValidTransportRule(key: string, rule: Record<string, unknown>): boolean {
	for (const field of Object.keys(rule)) {
		if (!isTransportRuleField(field)) return false;
	}
	if (Object.prototype.hasOwnProperty.call(rule, "requires_type_field")) return false;
	for (const field of ["command_field", "args_field", "env_field", "type_value", "url_field", "headers_field"]) {
		if (!isOptionalCompactString(rule[field])) return false;
	}
	if (!isOptionalBoolean(rule.include_type) || !isOptionalBoolean(rule.selected)) return false;
	if (!isOptionalRecord(rule.extra_fields)) return false;
	if (rule.include_type === true && !hasCompactString(rule.type_value)) return false;
	if (key === "stdio") return hasCompactString(rule.command_field);
	return hasCompactString(rule.url_field);
}

function isTransportRuleField(field: string): boolean {
	return [
		"template",
		"command_field",
		"args_field",
		"env_field",
		"include_type",
		"type_value",
		"url_field",
		"headers_field",
		"extra_fields",
		"selected",
	].includes(field);
}

function adminTransports(config: Record<string, unknown>): Record<string, TransportRuleData> | null {
	const transports = optionalRecordValue(config.transports);
	if (!transports) return null;
	const entries: Array<[string, TransportRuleData]> = [];

	for (const [key, value] of Object.entries(transports)) {
		if (!isCanonicalTransportKey(key)) return null;
		const rule = optionalRecordValue(value);
		if (!rule) return null;
		if (!isValidTransportRule(key, rule)) return null;
		entries.push([key, rule as TransportRuleData]);
	}

	return Object.fromEntries(entries);
}

export function adminDiscoveryClientToCandidate(
	raw: unknown,
	options?: { platform?: AdminDiscoveryPlatform },
): AdminDiscoveryClientCandidate | null {
	const client = recordValue(raw);
	const metadata = metadataRecord(client);
	const config = recordValue(client.config);
	const transports = adminTransports(config);
	if (!transports) return null;
	const identifier = compactString(client.identifier) ?? compactString(client.id) ?? compactString(client.name);
	if (!identifier) return null;
	const displayName =
		firstCompactString(client.displayName, client.display_name, metadata.display_name) ?? identifier;
	const fileRecord = recordValue(config.file);
	const merge = recordValue(fileRecord.merge);
	const mergeStrategy = adminMergeStrategy(merge?.strategy);
	if (!mergeStrategy) return null;
	const configPath = configPathFromDiscoveryClient(fileRecord, options?.platform);
	const configKind = compactString(config.kind);
	const hasConfigFileKind = configKind === "file";
	const hasWritableConfig = Boolean(configPath && hasConfigFileKind);
	const file = hasWritableConfig ? adminConfigFileParse(fileRecord) : null;
	if (hasWritableConfig && !file) return null;
	const configFileChoice: ClientConfigFileState = hasWritableConfig ? "with_config_file" : "without_config_file";
	const links = recordValue(client.links);
	const icon = recordValue(client.icon);

	const tags = [
		...new Set([
			...stringArrayValue(client.tags),
			...stringArrayValue(metadata.tags),
			...stringArrayValue(client.categories),
			...stringArrayValue(metadata.categories),
			...(compactString(client.category) ? [compactString(client.category)!] : []),
			...(compactString(metadata.category) ? [compactString(metadata.category)!] : []),
		]),
	];

	return {
		identifier,
		displayName,
		configFileChoice,
		configPath: configFileChoice === "with_config_file" ? configPath : "",
		configFileParseFormat: file?.format ?? "json",
		configFileParseContainerType: file?.containerType ?? "standard",
		configFileParseContainerKeysText: file?.containerKeys.join(", ") ?? "",
		mergeStrategy,
		description: compactString(client.description) ?? compactString(metadata.description) ?? "",
		homepageUrl: firstCompactString(links.homepage, client.homepageUrl, client.homepage_url, metadata.homepage_url) ?? "",
		docsUrl: firstCompactString(links.docs, client.docsUrl, client.docs_url, metadata.docs_url) ?? "",
		supportUrl: firstCompactString(links.support, client.supportUrl, client.support_url, metadata.support_url) ?? "",
		logoUrl: firstCompactString(icon.url, client.logoUrl, client.logo_url, metadata.logo_url) ?? "",
		tags,
		supportedTransports: Object.keys(transports),
		transports,
	};
}

function resolvedAdminDiscoveryClientCandidate(raw: unknown): AdminDiscoveryClientCandidate | null {
	const candidate = recordValue(raw);
	const identifier = compactString(candidate.identifier);
	const displayName = compactString(candidate.displayName);
	if (!identifier || !displayName) return null;
	if (candidate.configFileChoice !== "with_config_file" && candidate.configFileChoice !== "without_config_file") {
		return null;
	}
	const supportedTransports = stringArrayValue(candidate.supportedTransports);
	if (supportedTransports.some((transport) => !isCanonicalTransportKey(transport))) return null;
	const transports = adminTransports({ transports: candidate.transports });
	if (!transports) return null;
	const mergeStrategy = adminMergeStrategy(candidate.mergeStrategy);
	if (!mergeStrategy) return null;
	const configFileParseFormat = adminConfigParseFormat(candidate.configFileParseFormat);
	if (!configFileParseFormat) return null;
	return {
		identifier,
		displayName,
		configFileChoice: candidate.configFileChoice,
		configPath: compactString(candidate.configPath) ?? "",
		configFileParseFormat,
		configFileParseContainerType: candidate.configFileParseContainerType === "array" ? "array" : "standard",
		configFileParseContainerKeysText: compactString(candidate.configFileParseContainerKeysText) ?? "",
		mergeStrategy,
		description: compactString(candidate.description) ?? "",
		homepageUrl: compactString(candidate.homepageUrl) ?? "",
		docsUrl: compactString(candidate.docsUrl) ?? "",
		supportUrl: compactString(candidate.supportUrl) ?? "",
		logoUrl: compactString(candidate.logoUrl) ?? "",
		tags: stringArrayValue(candidate.tags),
		supportedTransports,
		transports,
	};
}

export function adminDiscoveryClientToUpdatePayload(
	raw: unknown,
	options?: { configPath?: string; forceWithoutConfigFile?: boolean },
): AdminClientUpdatePayload {
	const resolvedCandidate = resolvedAdminDiscoveryClientCandidate(raw);
	const isResolvedCandidateInput = "configFileChoice" in recordValue(raw);
	if (isResolvedCandidateInput && !resolvedCandidate) {
		throw new Error("Admin discovery client candidate is invalid.");
	}
	const candidate = resolvedCandidate ?? adminDiscoveryClientToCandidate(raw);
	if (!candidate) {
		throw new Error("Admin discovery client is missing a usable identifier.");
	}
	const configPath = options?.configPath?.trim() ?? candidate.configPath.trim();
	const hasConfigFile = !options?.forceWithoutConfigFile && candidate.configFileChoice === "with_config_file" && Boolean(configPath);
	const configFileParse: ClientConfigFileParse = {
		format: candidate.configFileParseFormat,
		container_type: candidate.configFileParseContainerType,
		container_keys: candidate.configFileParseContainerKeysText
			.split(",")
			.map((key) => key.trim())
			.filter(Boolean),
	};
	return {
		identifier: candidate.identifier,
		display_name: candidate.displayName,
		config_file_state: hasConfigFile ? "with_config_file" : "without_config_file",
		config_path: hasConfigFile ? configPath : undefined,
		description: candidate.description || undefined,
		homepage_url: candidate.homepageUrl || undefined,
		docs_url: candidate.docsUrl || undefined,
		support_url: candidate.supportUrl || undefined,
		logo_url: candidate.logoUrl || undefined,
		config_file_parse: hasConfigFile ? configFileParse : undefined,
		clear_config_file_parse: !hasConfigFile,
		transports: hasConfigFile ? candidate.transports : undefined,
		clear_transports: !hasConfigFile,
	};
}

function discoveryMetaRecord(raw: Record<string, unknown>): Record<string, unknown> {
	const nestedServer = recordValue(raw.server);
	return (
		recordValue(recordValue(raw._meta)["ai.mcpmate/discovery"]) ||
		recordValue(recordValue(raw.metadata).discovery) ||
		recordValue(recordValue(nestedServer._meta)["ai.mcpmate/discovery"])
	);
}

function iconSrcFromIconList(value: unknown): string | undefined {
	if (!Array.isArray(value) || value.length === 0) return undefined;
	const first = recordValue(value[0]);
	return firstCompactString(first.src, first.url, first.href);
}

function resolveAdminDiscoveryImageUrl(value: string | undefined): string | undefined {
	const trimmed = compactString(value);
	if (!trimmed) return undefined;
	if (/^data:image\//i.test(trimmed) || /^https?:\/\//i.test(trimmed)) return trimmed;
	try {
		return new URL(trimmed, `${ADMIN_DISCOVERY_BASE_URL}/`).toString();
	} catch {
		return undefined;
	}
}

function adminDiscoveryServerLogoUrl(server: Record<string, unknown>): string {
	const official = recordValue(server.official);
	const curated = recordValue(server.curated);
	const icon = recordValue(server.icon);
	const curatedIcon = recordValue(curated.icon);
	const meta = discoveryMetaRecord(server);
	const officialIcon = iconSrcFromIconList(official.icons);
	const rawUrl = firstCompactString(
		icon.url,
		typeof server.icon === "string" ? server.icon : undefined,
		curatedIcon.url,
		typeof curated.icon === "string" ? curated.icon : undefined,
		meta.iconUrl,
		meta.brandIconUrl,
		compactString(server.iconUrl),
		compactString(server.logoUrl),
		officialIcon,
		iconSrcFromIconList(server.icons),
	);
	return resolveAdminDiscoveryImageUrl(rawUrl) ?? "";
}

export function adminDiscoveryServerToOnboardingCandidate(raw: unknown): AdminDiscoveryServerCandidate | null {
	const server = recordValue(raw);
	const official = recordValue(server.official);
	const curated = recordValue(server.curated);
	const runtime = recordValue(server.runtime);
	const installConfig = recordValue(runtime.install_config ?? server.install_config);
	const registryId =
		compactString(server.id) ?? compactString(official.name) ?? compactString(official.title) ?? "admin-server";
	const displayName = compactString(curated.displayName) ?? compactString(curated.display_name) ?? compactString(official.title) ?? registryId;
	const kind = compactString(installConfig.server_type) ?? compactString(installConfig.type) ?? "stdio";
	const command = compactString(installConfig.command);
	const url = compactString(installConfig.url);
	if (kind === "stdio" && !command) return null;
	if (kind !== "stdio" && !url) return null;
	const args = stringArrayValue(installConfig.args);
	const env = stringRecordValue(installConfig.env);
	const headers = recordValue(installConfig.headers);
	const importConfig: Record<string, unknown> = {
		type: kind,
		source: { type: "catalog", ref: registryId },
	};

	if (command) importConfig.command = command;
	if (args.length > 0) importConfig.args = args;
	if (Object.keys(env).length > 0) importConfig.env = env;
	if (url) importConfig.url = url;
	if (Object.keys(headers).length > 0) importConfig.headers = headers;

	const categories = [
		...new Set([
			...stringArrayValue(curated.categories),
			...stringArrayValue(curated.tags),
			...stringArrayValue(server.categories),
			...stringArrayValue(server.tags),
		]),
	];

	return {
		key: `admin:${registryId}`,
		name: displayName,
		kind,
		command,
		args,
		env,
		url,
		categories,
		tags: stringArrayValue(curated.tags),
		description:
			firstCompactString(curated.summary, curated.description, official.description, server.description) ?? "",
		logoUrl: adminDiscoveryServerLogoUrl(server),
		source_clients: ["MCPMate"],
		source_client_ids: [],
		import_config: importConfig,
	};
}

export type CatalogTagFilter = string;

export function filterCatalogItemsByTag<T extends { tags?: string[]; categories?: string[] }>(
	items: T[],
	tag: CatalogTagFilter,
): T[] {
	if (tag === "all") return items;
	const normalized = tag.toLowerCase();
	return items.filter((item) => {
		const tags = [...(item.tags ?? []), ...(item.categories ?? [])].map((entry) => entry.toLowerCase());
		return tags.includes(normalized);
	});
}

export function enrichLocalServerCandidates<
	T extends { name: string; categories?: string[]; tags?: string[]; logoUrl?: string },
>(
	localCandidates: T[],
	popularCandidates: Array<{ name: string; categories?: string[]; tags?: string[]; logoUrl?: string }>,
): Array<T & { categories: string[]; tags: string[]; logoUrl?: string }> {
	const popularByName = new Map(
		popularCandidates.map((candidate) => [candidate.name.trim().toLowerCase(), candidate]),
	);
	return localCandidates.map((candidate) => {
		const match = popularByName.get(candidate.name.trim().toLowerCase());
		return {
			...candidate,
			categories: match?.categories ?? candidate.categories ?? [],
			tags: match?.tags ?? candidate.tags ?? [],
			logoUrl: match?.logoUrl || candidate.logoUrl || undefined,
		};
	});
}

export function clientTagsForDetected(
	category: string | undefined,
	catalogTags: string[] | undefined,
): string[] {
	if (catalogTags && catalogTags.length > 0) return catalogTags;
	if (category?.trim()) {
		const normalized = category.trim().toLowerCase();
		if (normalized === "terminal") return ["cli"];
		return [normalized];
	}
	return [];
}
