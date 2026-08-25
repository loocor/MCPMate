export type DemoCapabilityKind = "tools" | "resources" | "prompts" | "templates";

export type DemoCapabilityLists = {
	tools: Record<string, unknown>[];
	resources: Record<string, unknown>[];
	prompts: Record<string, unknown>[];
	templates: Record<string, unknown>[];
};

function demoTool(
	serverId: string,
	serverName: string,
	toolName: string,
	description: string,
): Record<string, unknown> {
	return {
		ref_id: `${serverId}:${toolName}`,
		id: `${serverId}:${toolName}`,
		server_id: serverId,
		server_name: serverName,
		tool_name: toolName,
		unique_name: `${serverId}__${toolName}`,
		name: toolName,
		description,
		inputSchema: { type: "object", properties: {} },
	};
}

function demoResource(
	serverId: string,
	serverName: string,
	uri: string,
	name: string,
	description: string,
): Record<string, unknown> {
	return {
		ref_id: `${serverId}:${uri}`,
		id: `${serverId}:${uri}`,
		server_id: serverId,
		server_name: serverName,
		name,
		resource_uri: uri,
		unique_uri: `${serverId}__${uri}`,
		uri,
		description,
		mime_type: "text/plain",
	};
}

function demoTemplate(
	serverId: string,
	serverName: string,
	uriTemplate: string,
	name: string,
	description: string,
): Record<string, unknown> {
	return {
		ref_id: `${serverId}:${uriTemplate}`,
		id: `${serverId}:${uriTemplate}`,
		server_id: serverId,
		server_name: serverName,
		name,
		uri_template: uriTemplate,
		uriTemplate,
		unique_uri_template: `${serverId}__${uriTemplate}`,
		description,
	};
}

function demoPrompt(
	serverId: string,
	serverName: string,
	promptName: string,
	description: string,
): Record<string, unknown> {
	return {
		ref_id: `${serverId}:${promptName}`,
		id: `${serverId}:${promptName}`,
		server_id: serverId,
		server_name: serverName,
		prompt_name: promptName,
		unique_name: `${serverId}__${promptName}`,
		name: promptName,
		description,
	};
}

export const demoCapabilityCatalog: Record<string, DemoCapabilityLists> = {
	everything: {
		tools: [
			["echo", "Echo a message back from the reference server."],
			["add", "Add two numbers."],
			["printEnv", "Print the server environment."],
			["longRunningOperation", "Run a long-running progress example."],
			["sampleLLM", "Call a sample LLM-backed tool."],
			["annotatedMessage", "Return an annotated message payload."],
			["getTinyImage", "Return a tiny demo image."],
		].map(([name, description]) =>
			demoTool("everything", "everything", name, description),
		),
		resources: [
			["test://static/resource/1", "Resource 1", "First static everything resource."],
			["test://static/resource/2", "Resource 2", "Second static everything resource."],
		].map(([uri, name, description]) =>
			demoResource("everything", "everything", uri, name, description),
		),
		prompts: [
			["simple_prompt", "A simple prompt from the everything server."],
			["complex_prompt", "A prompt that takes arguments."],
		].map(([name, description]) =>
			demoPrompt("everything", "everything", name, description),
		),
		templates: [
			[
				"test://static/resource/{id}",
				"Static resource",
				"Read a numbered everything resource.",
			],
		].map(([uri, name, description]) =>
			demoTemplate("everything", "everything", uri, name, description),
		),
	},
	playwright: {
		tools: [
			["browser_navigate", "Navigate the browser to a URL."],
			["browser_snapshot", "Capture an accessibility snapshot of the page."],
			["browser_click", "Click an element on the page."],
			["browser_type", "Type text into an element."],
			["browser_take_screenshot", "Take a screenshot of the page or an element."],
			["browser_hover", "Hover over an element."],
			["browser_select_option", "Select an option from a dropdown."],
			["browser_press_key", "Press a keyboard key."],
			["browser_wait_for", "Wait for a condition or time."],
			["browser_tab_list", "List open browser tabs."],
			["browser_evaluate", "Evaluate JavaScript in the page."],
			["browser_close", "Close the browser."],
		].map(([name, description]) =>
			demoTool("playwright", "playwright", name, description),
		),
		resources: [],
		prompts: [],
		templates: [],
	},
	sequential_thinking: {
		tools: [
			[
				"sequentialthinking",
				"Break a problem into sequential thinking steps.",
			],
		].map(([name, description]) =>
			demoTool(
				"sequential_thinking",
				"sequential-thinking-server",
				name,
				description,
			),
		),
		resources: [],
		prompts: [],
		templates: [],
	},
	context7: {
		tools: [
			["resolve-library-id", "Resolve a library name to a Context7 ID."],
			["get-library-docs", "Fetch versioned documentation for a library."],
			["search-libraries", "Search the Context7 catalog."],
			["get-library-examples", "Get usage examples for a library."],
			["list-library-versions", "List known versions of a library."],
			["compare-versions", "Compare documentation between two versions."],
			["get-api-reference", "Fetch a specific API reference page."],
		].map(([name, description]) =>
			demoTool("context7", "context7", name, description),
		),
		resources: [
			["docs://react/hooks", "React hooks", "React hooks documentation."],
			["docs://react/server-components", "RSC", "React Server Components."],
			["docs://typescript/handbook", "TypeScript", "TypeScript handbook."],
			["docs://node/fs", "Node fs", "Node.js filesystem API."],
			["docs://bun/cli", "Bun CLI", "Bun command reference."],
			["docs://vite/config", "Vite config", "Vite configuration."],
			["docs://tailwind/utilities", "Tailwind", "Tailwind utility classes."],
			["docs://mcp/spec", "MCP spec", "Model Context Protocol spec."],
			["docs://github/rest", "GitHub REST", "GitHub REST API."],
			["docs://cloudflare/pages", "Pages", "Cloudflare Pages."],
			["docs://rust/book", "Rust book", "The Rust Programming Language."],
			["docs://python/typing", "Python typing", "Python typing documentation."],
		].map(([uri, name, description]) =>
			demoResource("context7", "context7", uri, name, description),
		),
		prompts: [],
		templates: [
			[
				"docs://{library}/{topic}",
				"Library topic",
				"Documentation page for a library topic.",
			],
			[
				"docs://{library}/v/{version}",
				"Versioned docs",
				"Docs pinned to a library version.",
			],
			[
				"docs://{library}/api/{symbol}",
				"API symbol",
				"API reference for one symbol.",
			],
		].map(([uri, name, description]) =>
			demoTemplate("context7", "context7", uri, name, description),
		),
	},
};

export function demoCapabilitiesFor(
	serverId: string | null,
): DemoCapabilityLists {
	if (serverId && demoCapabilityCatalog[serverId]) {
		return demoCapabilityCatalog[serverId];
	}
	return { tools: [], resources: [], prompts: [], templates: [] };
}

export function demoListResult(items: Record<string, unknown>[]) {
	return {
		items,
		state: items.length > 0 ? "ready" : "empty_data",
	};
}

export function demoCapabilityCounts(serverId: string) {
	const lists = demoCapabilitiesFor(serverId);
	return {
		tools: lists.tools.length,
		prompts: lists.prompts.length,
		resources: lists.resources.length,
		templates: lists.templates.length,
	};
}
