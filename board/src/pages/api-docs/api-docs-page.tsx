import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { AlertCircle, Braces, KeyRound, Route, type LucideIcon } from "lucide-react";
import { API_BASE_CHANGED_EVENT, resolveApiUrl } from "../../lib/api";
import { isBoardDemoMode } from "../../lib/demo-mode";

type DemoApiEndpoint = {
	icon: LucideIcon;
	title: string;
	method: string;
	path: string;
	body: string;
};

const DEMO_API_ENDPOINTS: DemoApiEndpoint[] = [
	{
		icon: Route,
		title: "System",
		method: "GET",
		path: "/api/system/status",
		body: '{ "status": "running", "connected_servers": 3 }',
	},
	{
		icon: Braces,
		title: "Profiles",
		method: "GET",
		path: "/api/mcp/profile/list",
		body: '{ "profile": [{ "name": "Research" }, { "name": "Build" }] }',
	},
	{
		icon: KeyRound,
		title: "Secrets",
		method: "GET",
		path: "/api/secrets/status",
		body: '{ "status": "ready", "provider_mode": "operating_system" }',
	},
];

function resolveApiDocsUrl(): string {
	return resolveApiUrl("/docs");
}

type DemoApiDocsPageProps = {
	pageTitle: string;
};

type DemoApiEndpointCardProps = {
	endpoint: DemoApiEndpoint;
};

function DemoApiEndpointCard({ endpoint }: DemoApiEndpointCardProps) {
	const Icon = endpoint.icon;

	return (
		<div className="rounded-lg border border-slate-200 bg-white p-4 shadow-sm dark:border-slate-700 dark:bg-slate-900">
			<div className="flex items-center gap-2">
				<Icon className="h-4 w-4 text-slate-500" aria-hidden />
				<h3 className="text-sm font-semibold text-slate-950 dark:text-slate-50">
					{endpoint.title}
				</h3>
			</div>
			<div className="mt-4 flex items-center gap-2 text-xs">
				<span className="rounded bg-emerald-50 px-2 py-1 font-semibold text-emerald-700 dark:bg-emerald-950 dark:text-emerald-300">
					{endpoint.method}
				</span>
				<code className="truncate text-slate-600 dark:text-slate-300">
					{endpoint.path}
				</code>
			</div>
			<pre className="mt-4 overflow-auto rounded-md bg-slate-950 p-3 text-xs leading-5 text-slate-100">
				{endpoint.body}
			</pre>
		</div>
	);
}

function DemoApiDocsPage({ pageTitle }: DemoApiDocsPageProps) {
	return (
		<div className="flex h-[calc(100vh-8rem)] flex-col gap-4 overflow-auto">
			<div className="rounded-lg border border-slate-200 bg-white p-5 shadow-sm dark:border-slate-700 dark:bg-slate-900">
				<div className="flex items-start gap-3">
					<div className="rounded-md bg-sky-50 p-2 text-sky-700 dark:bg-sky-950 dark:text-sky-300">
						<Braces className="h-5 w-5" aria-hidden />
					</div>
					<div>
						<h2 className="text-lg font-semibold text-slate-950 dark:text-slate-50">
							{pageTitle}
						</h2>
						<p className="mt-1 max-w-2xl text-sm text-muted-foreground">
							Demo preview of the MCPMate API surface. This route is rendered by the board in demo mode and does not require the backend Swagger server.
						</p>
					</div>
				</div>
			</div>
			<div className="grid gap-4 lg:grid-cols-3">
				{DEMO_API_ENDPOINTS.map((endpoint) => (
					<DemoApiEndpointCard key={endpoint.path} endpoint={endpoint} />
				))}
			</div>
		</div>
	);
}

export function ApiDocsPage() {
	const { t } = useTranslation();
	const [docsUrl, setDocsUrl] = useState(resolveApiDocsUrl);
	const [iframeFailed, setIframeFailed] = useState(false);
	const demoMode = isBoardDemoMode();

	useEffect(() => {
		const handleApiBaseChanged = () => {
			setIframeFailed(false);
			setDocsUrl(resolveApiDocsUrl());
		};

		window.addEventListener(API_BASE_CHANGED_EVENT, handleApiBaseChanged);
		return () => {
			window.removeEventListener(API_BASE_CHANGED_EVENT, handleApiBaseChanged);
		};
	}, []);

	const pageTitle = t("nav.apiDocs", { defaultValue: "API Docs" });

	if (demoMode) {
		return <DemoApiDocsPage pageTitle={pageTitle} />;
	}

	return (
		<div className="flex h-[calc(100vh-8rem)] flex-col gap-3">
			<div className="relative min-h-0 flex-1 overflow-hidden rounded-lg border border-slate-200 bg-white dark:border-slate-700 dark:bg-slate-900">
				<iframe
					key={docsUrl}
					src={docsUrl}
					title={pageTitle}
					className="h-full w-full"
					onError={() => setIframeFailed(true)}
				/>
				{iframeFailed ? (
					<div className="absolute inset-0 flex items-center justify-center bg-background/95">
						<div className="max-w-md space-y-2 p-4 text-center">
							<AlertCircle className="mx-auto h-8 w-8 text-amber-500" />
							<p className="text-sm font-medium text-foreground">
								{t("errors.serverUnavailable", {
									defaultValue: "Service unavailable",
								})}
							</p>
							<p className="text-xs text-muted-foreground">
								{t("settings:system.portsReloadFailedDescription", {
									defaultValue:
										"Check the desktop app is healthy and try Reload again.",
								})}
							</p>
						</div>
					</div>
				) : null}
			</div>
		</div>
	);
}
