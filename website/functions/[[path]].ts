// Cloudflare Pages Functions - catch‑all to support SPA deep links
// Only fallback to index.html for selected client routes to avoid masking real 404s.

const SPA_PREFIXES = ["/docs/"] as const;
const SPA_PATHS = new Set(["/privacy", "/terms"]);
const INDEX_PATH = "/index.html";

function acceptsHtml(request: Request) {
	const accept = request.headers.get("accept") || "";
	return accept.includes("text/html");
}

function isSpaPath(pathname: string) {
	if (SPA_PATHS.has(pathname)) return true;
	return SPA_PREFIXES.some((p) => pathname.startsWith(p));
}

export const onRequest: PagesFunction = async (context) => {
	const { request, env } = context;
	const url = new URL(request.url);

	// Skip processing for non-GET requests or non-HTML requests
	if (request.method !== "GET" || !acceptsHtml(request)) {
		return context.next();
	}

	// Try to serve static asset first
	const response = await context.next();

	// If asset found or not a SPA path, return as is
	if (response.status !== 404 || !isSpaPath(url.pathname)) {
		return response;
	}

	// For SPA paths with 404, serve index.html
	// Try env.ASSETS first (preferred method for Pages Functions)
	if (env.ASSETS) {
		const assetUrl = new URL(INDEX_PATH, url);
		return env.ASSETS.fetch(assetUrl);
	}

	// Fallback: create a new request for index.html
	const indexUrl = new URL(INDEX_PATH, url);
	const indexRequest = new Request(indexUrl, request);
	return context.env.ASSETS ? context.env.ASSETS.fetch(indexRequest) : context.next();
};
