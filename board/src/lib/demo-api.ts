import type {
	AuditEventRecord,
	ClientInfo,
	PasswordStatusData,
	RuntimeStatusResponse,
	ServerDetail,
	SecretMetadata,
	SecretStoreStatusData,
	SecretUsage,
	SystemMetrics,
	SystemStatus,
} from "./types";

const now = Date.now();
const timestamp = new Date(now).toISOString();

const demoProfiles = [
	{
		id: "demo-profile-research",
		name: "Research",
		description: "Focused tools for documentation lookup, GitHub review, and source inspection.",
		profile_type: "workflow",
		multi_select: true,
		priority: 10,
		is_active: true,
		is_default: true,
		role: "research",
		allowed_operations: ["tools/list", "tools/call", "resources/read"],
	},
	{
		id: "demo-profile-build",
		name: "Build",
		description: "Coding workflow with repository, filesystem, and package-reference tools.",
		profile_type: "workflow",
		multi_select: true,
		priority: 20,
		is_active: true,
		is_default: false,
		role: "coding",
		allowed_operations: ["tools/list", "tools/call"],
	},
	{
		id: "demo-profile-ops",
		name: "Operations",
		description: "Runtime evidence and audit review for local MCP workflows.",
		profile_type: "workflow",
		multi_select: false,
		priority: 30,
		is_active: false,
		is_default: false,
		role: "operator",
		allowed_operations: ["tools/list", "resources/read"],
	},
] as const;

const demoServers: ServerDetail[] = [
	{
		id: "github-mcp",
		name: "github-mcp",
		server_type: "stdio",
		status: "connected",
		enabled: true,
		globally_enabled: true,
		enabled_in_suits: true,
		source: { type: "registry", ref: "io.github.github-mcp-server" },
		instance_count: 1,
		capability: {
			supports_tools: true,
			supports_prompts: false,
			supports_resources: true,
			tools_count: 18,
			prompts_count: 0,
			resources_count: 4,
			resource_templates_count: 2,
		},
		meta: {
			description: "Repository issues, pull requests, and code review context.",
			version: "0.6.2",
		},
		instances: [{ id: "github-mcp-main", name: "github-mcp", status: "connected" }],
	},
	{
		id: "context7",
		name: "context7",
		server_type: "stdio",
		status: "connected",
		enabled: true,
		globally_enabled: true,
		enabled_in_suits: true,
		source: { type: "registry", ref: "io.context7.mcp" },
		instance_count: 1,
		instances: [{ id: "context7-main", name: "context7", status: "connected" }],
		capability: {
			supports_tools: true,
			supports_prompts: false,
			supports_resources: true,
			tools_count: 7,
			prompts_count: 0,
			resources_count: 12,
			resource_templates_count: 3,
		},
		meta: {
			description: "Versioned library documentation lookup for coding workflows.",
			version: "1.0.0",
		},
	},
	{
		id: "filesystem-workspace",
		name: "filesystem-workspace",
		server_type: "stdio",
		status: "connected",
		enabled: true,
		globally_enabled: true,
		enabled_in_suits: true,
		source: { type: "local" },
		instance_count: 1,
		instances: [
			{ id: "filesystem-workspace-main", name: "filesystem-workspace", status: "connected" },
		],
		capability: {
			supports_tools: true,
			supports_prompts: false,
			supports_resources: true,
			tools_count: 9,
			prompts_count: 0,
			resources_count: 6,
			resource_templates_count: 0,
		},
		meta: {
			description: "Workspace-scoped file access with explicit local boundaries.",
			version: "0.4.1",
		},
	},
];

const demoClients: ClientInfo[] = [
	{
		identifier: "claude_desktop",
		display_name: "Claude Desktop",
		category: "desktop",
		enabled: true,
		detected: true,
		config_path: "/Users/demo/Library/Application Support/Claude/claude_desktop_config.json",
		config_exists: true,
		has_mcp_config: true,
		mcp_servers_count: 3,
		approval_status: "approved",
		attachment_state: "attached",
		writable_config: true,
		governed_by_default_policy: true,
		template: {
			format: "json",
			container_type: "json",
			merge_strategy: "object",
			keep_original_config: true,
			storage: { kind: "file", path_strategy: "known_path" },
			homepage_url: "https://claude.ai/download",
		},
		description: "Desktop client receiving profile-filtered MCPMate output.",
		homepage_url: "https://claude.ai/download",
	},
	{
		identifier: "codex",
		display_name: "Codex",
		category: "terminal",
		enabled: true,
		detected: true,
		config_path: "/Users/demo/.codex/config.toml",
		config_exists: true,
		has_mcp_config: true,
		mcp_servers_count: 3,
		approval_status: "approved",
		attachment_state: "attached",
		writable_config: true,
		governed_by_default_policy: true,
		template: {
			format: "toml",
			container_type: "toml",
			merge_strategy: "object",
			keep_original_config: true,
			storage: { kind: "file", path_strategy: "known_path" },
		},
		description: "Terminal workflow client with MCPMate-managed capability exposure.",
	},
	{
		identifier: "cursor",
		display_name: "Cursor",
		category: "editor",
		enabled: true,
		detected: true,
		config_path: "/Users/demo/.cursor/mcp.json",
		config_exists: true,
		has_mcp_config: true,
		mcp_servers_count: 2,
		approval_status: "approved",
		attachment_state: "attached",
		writable_config: true,
		governed_by_default_policy: true,
		template: {
			format: "json",
			container_type: "json",
			merge_strategy: "object",
			keep_original_config: true,
			storage: { kind: "file", path_strategy: "known_path" },
			homepage_url: "https://cursor.com",
		},
		description: "Editor client with selected MCP server config applied through MCPMate.",
		homepage_url: "https://cursor.com",
	},
];

const demoSecrets: SecretMetadata[] = [
	{
		alias: "github-token",
		placeholder: "[[secret:github-token]]",
		kind: "api_key",
		label: "GitHub token",
		origin: {
			server_id: "github-mcp",
			server_name: "github-mcp",
			field_group: "env",
			field_key: "GITHUB_PERSONAL_ACCESS_TOKEN",
		},
		provider_id: "demo-os-keychain",
		provider_kind: "operating_system",
		version: 2,
		used_by_count: 1,
		historical_usage_count: 0,
		created_at: new Date(now - 86_400_000).toISOString(),
		updated_at: new Date(now - 3_600_000).toISOString(),
	},
];

const demoAuditEvents: AuditEventRecord[] = [
	{
		id: 1003,
		category: "profile_config",
		action: "profile_update",
		status: "success",
		occurred_at_ms: now - 90_000,
		profile_id: "demo-profile-research",
		profile_name: "Research",
		target: "Research",
		detail: "Profile narrowed Claude Desktop to research-safe capability exposure.",
		duration_ms: 42,
	},
	{
		id: 1002,
		category: "client_config",
		action: "client_config_apply",
		status: "success",
		occurred_at_ms: now - 150_000,
		client_id: "claude_desktop",
		client_name: "Claude Desktop",
		profile_id: "demo-profile-research",
		profile_name: "Research",
		target: "Claude Desktop",
		detail: "Client config received MCPMate-managed server output.",
		duration_ms: 86,
	},
	{
		id: 1001,
		category: "server_config",
		action: "server_import",
		status: "success",
		occurred_at_ms: now - 240_000,
		server_id: "github-mcp",
		server_name: "github-mcp",
		target: "github-mcp",
		detail: "Registry server reviewed and imported into local demo state.",
		duration_ms: 118,
	},
	{
		id: 1000,
		category: "mcp_request",
		action: "tools_list",
		status: "success",
		occurred_at_ms: now - 320_000,
		client_id: "claude_desktop",
		client_name: "Claude Desktop",
		profile_id: "demo-profile-research",
		profile_name: "Research",
		target: "tools/list",
		detail: "Client listed profile-filtered tools through MCPMate.",
		duration_ms: 31,
	},
];

function wrapped<T>(data: T): { success: true; data: T } {
	return {
		success: true,
		data,
	};
}

function demoSystemStatus(): SystemStatus {
	return {
		status: "running",
		uptime: 386,
		version: "demo",
		total_servers: demoServers.length,
		connected_servers: demoServers.filter((server) => server.status === "connected").length,
		active_mcp_servers: demoServers.length,
		aggregated_tools_count: 34,
	};
}

function demoSystemMetrics(): SystemMetrics {
	return {
		timestamp,
		cpu_usage_percent: 0.9,
		memory_usage_bytes: 168 * 1024 * 1024,
		system_cpu_usage: 14.5,
		system_memory_usage: 7_420 * 1024 * 1024,
		system_memory_total: 32_000 * 1024 * 1024,
		total_tools_count: 34,
		unique_tools_count: 29,
	};
}

function demoRuntimeStatus(): RuntimeStatusResponse {
	return {
		user_home: "/Users/demo",
		node: {
			runtime_type: "node",
			available: true,
			path: "/Users/demo/.mcpmate/runtimes/node/bin/node",
			version: "22.13.1",
			message: "Managed Node runtime ready",
		},
		bun: {
			runtime_type: "bun",
			available: true,
			path: "/Users/demo/.mcpmate/runtimes/bun/bin/bun",
			version: "1.2.18",
			message: "Managed Bun runtime ready",
		},
		uv: {
			runtime_type: "uv",
			available: true,
			path: "/Users/demo/.mcpmate/runtimes/uv/bin/uv",
			version: "0.7.19",
			message: "Managed uv runtime ready",
		},
	};
}

function demoSecretStoreStatus(): SecretStoreStatusData {
	return {
		status: "ready",
		provider: {
			provider_id: "demo-os-keychain",
			provider_kind: "operating_system",
			provider_mode: "operating_system",
			security_level: "high",
		},
		issue: null,
	};
}

function demoPasswordStatus(): PasswordStatusData {
	return {
		enabled: true,
		scope: ["startup", "settings"],
		has_password: true,
	};
}

function demoSecretUsages(alias: string): SecretUsage[] {
	if (alias !== "github-token") {
		return [];
	}

	return [
		{
			alias,
			server_id: "github-mcp",
			location: {
				group: "env",
				key: "GITHUB_PERSONAL_ACCESS_TOKEN",
			},
			status: "active",
		},
	];
}

export async function handleDemoApiRequest<T>(
	endpoint: string,
	options?: RequestInit,
): Promise<T> {
	const method = (options?.method ?? "GET").toUpperCase();
	const url = new URL(endpoint, "http://demo.mcpmate.local");
	const path = url.pathname;

	if (method === "GET" && path === "/api/system/readiness") {
		return { type: "ready", status: "ok" } as T;
	}
	if (method === "GET" && path === "/api/system/status") {
		return demoSystemStatus() as T;
	}
	if (method === "GET" && path === "/api/system/metrics") {
		return demoSystemMetrics() as T;
	}
	if (method === "GET" && path === "/api/system/settings") {
		return wrapped({
			api_port: 8080,
			mcp_port: 8090,
			api_url: "http://127.0.0.1:8080",
			mcp_http_url: "http://127.0.0.1:8090",
			first_contact_behavior: "review",
			onboarding_policy: "require_approval",
			inspector_timeout_ms: 120_000,
			default_config_mode: "unify",
		}) as T;
	}
	if (method === "GET" && path === "/api/runtime/status") {
		return wrapped(demoRuntimeStatus()) as T;
	}
	if (method === "GET" && path === "/api/mcp/servers/list") {
		return wrapped({ servers: demoServers }) as T;
	}
	if (method === "GET" && path === "/api/mcp/profile/list") {
		return wrapped({
			profile: demoProfiles,
			total: demoProfiles.length,
			timestamp,
		}) as T;
	}
	if (method === "GET" && path === "/api/client/list") {
		return wrapped({
			client: demoClients,
			total: demoClients.length,
			last_updated: timestamp,
		}) as T;
	}
	if (method === "GET" && path === "/api/client/detect") {
		return wrapped({
			client: demoClients,
			total: demoClients.length,
			last_updated: timestamp,
		}) as T;
	}
	if (method === "GET" && path === "/api/client/capability-config") {
		const identifier = url.searchParams.get("identifier") ?? demoClients[0]?.identifier ?? "demo";
		return wrapped({
			identifier,
			capability_source: "profiles",
			selected_profile_ids: ["demo-profile-research"],
			unify_direct_exposure: null,
		}) as T;
	}
	if (method === "GET" && path === "/api/audit/events") {
		return wrapped({
			events: demoAuditEvents,
			next_cursor: null,
		}) as T;
	}
	if (method === "GET" && path === "/api/audit/policy") {
		return wrapped({
			enabled: true,
			retention_days: 30,
			include_payloads: false,
		}) as T;
	}
	if (method === "GET" && path === "/api/onboarding/status") {
		return wrapped({
			completed: false,
			servers_count: demoServers.length,
			clients_count: demoClients.length,
		}) as T;
	}
	if (method === "POST" && path === "/api/onboarding/complete") {
		return wrapped({ ok: true }) as T;
	}
	if (method === "POST" && path === "/api/onboarding/reset") {
		return wrapped({ ok: true }) as T;
	}
	if (method === "GET" && path === "/api/onboarding/runtime-check") {
		return wrapped({
			runtimes: [
				{ name: "node", available: true, version: "22.13.1", source: "mcpMate" },
				{ name: "bun", available: true, version: "1.2.18", source: "mcpMate" },
				{ name: "uv", available: true, version: "0.7.19", source: "mcpMate" },
			],
			has_js_runtime: true,
			has_python_runtime: true,
		}) as T;
	}
	if (method === "POST" && path === "/api/onboarding/server-scan") {
		return wrapped({
			candidates: [
				{
					key: "demo-local:github-mcp",
					name: "github-mcp",
					kind: "stdio",
					command: "npx",
					args: ["-y", "@modelcontextprotocol/server-github"],
					env: { GITHUB_PERSONAL_ACCESS_TOKEN: "[[secret:github-token]]" },
					url: null,
					source_clients: ["Claude Desktop"],
					source_client_ids: ["claude_desktop"],
				},
				{
					key: "demo-local:filesystem-workspace",
					name: "filesystem-workspace",
					kind: "stdio",
					command: "npx",
					args: ["-y", "@modelcontextprotocol/server-filesystem", "/Users/demo/Projects"],
					env: {},
					url: null,
					source_clients: ["Codex"],
					source_client_ids: ["codex"],
				},
			],
			errors: [],
		}) as T;
	}
	if (method === "GET" && path === "/api/secrets/status") {
		return wrapped(demoSecretStoreStatus()) as T;
	}
	if (method === "GET" && path === "/api/secrets/list") {
		return wrapped({ secrets: demoSecrets }) as T;
	}
	if (method === "GET" && path === "/api/secrets/usages") {
		const alias = url.searchParams.get("alias") ?? "github-token";
		return wrapped({
			usages: demoSecretUsages(alias),
		}) as T;
	}
	if (method === "GET" && path === "/api/secrets/password/status") {
		return wrapped(demoPasswordStatus()) as T;
	}
	if (method === "POST" && path.startsWith("/api/secrets/password/")) {
		return wrapped(demoPasswordStatus()) as T;
	}
	if (method === "POST" && path === "/api/secrets/provider/switch") {
		return wrapped({ new_status: demoSecretStoreStatus() }) as T;
	}
	if (method === "POST" && path === "/api/secrets/passphrase/rotate") {
		return wrapped(demoSecretStoreStatus()) as T;
	}
	if (method === "GET" && path === "/api/mcp/profile/capability-token-ledger") {
		const profileId = url.searchParams.get("profile_id") ?? "demo-profile-research";
		return {
			items: [
				{
					profile_row_id: profileId,
					kind: "tools",
					server_id: "github-mcp",
					server_enabled_in_profile: true,
					payload_json: JSON.stringify({
						tools: ["issues.search", "pulls.review", "repos.read"],
					}),
				},
				{
					profile_row_id: profileId,
					kind: "tools",
					server_id: "context7",
					server_enabled_in_profile: true,
					payload_json: JSON.stringify({
						tools: ["docs.resolve-library", "docs.get"],
					}),
				},
			],
			tokenizer_note: "demo",
		} as T;
	}

	throw new Error(`Demo API endpoint is not implemented: ${method} ${path}`);
}
