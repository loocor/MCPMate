import { useQuery, useQueryClient } from "@tanstack/react-query";
import { MessagesSquare } from "lucide-react";
import React from "react";
import { useTranslation } from "react-i18next";
import { Outlet, useLocation, useNavigate } from "react-router-dom";
import { MCPMATE_DISCORD_COMMUNITY_HREF } from "../../lib/mcpmate-community-urls";
import { onboardingApi } from "../../lib/onboarding-api";
import {
	auditApi,
	configSuitsApi,
	notificationsService,
	serversApi,
	setApiBaseUrl,
	systemApi,
} from "../../lib/api";
import { isBoardDemoMode } from "../../lib/demo-mode";
import { isTauriEnvironmentSync } from "../../lib/platform";
import { useAppStore } from "../../lib/store";
import type { ServerSource } from "../../lib/types";
import { websiteLangParam } from "../../lib/website-lang";
import { Header } from "./header";
import { Sidebar } from "./sidebar";

function applyDesktopApiBaseUrl(apiBaseUrl: string): void {
	setApiBaseUrl(apiBaseUrl);
	notificationsService.reconnectAfterApiBaseChanged();
}

async function invalidateDesktopCoreQueries(
	queryClient: ReturnType<typeof useQueryClient>,
): Promise<void> {
	await Promise.all([
		queryClient.invalidateQueries({ queryKey: ["systemStatus"] }),
		queryClient.invalidateQueries({ queryKey: ["systemMetrics"] }),
		queryClient.invalidateQueries({ queryKey: ["servers"] }),
		queryClient.invalidateQueries({ queryKey: ["clients"] }),
		queryClient.invalidateQueries({ queryKey: ["configSuits"] }),
	]);
}

function asRecordPayload(raw: unknown): Record<string, unknown> | undefined {
	if (!raw || typeof raw !== "object") {
		return undefined;
	}
	return raw as Record<string, unknown>;
}

type DesktopInvoke = (
	command: string,
	args?: Record<string, unknown>,
) => Promise<unknown>;

function summarizeImportServerPayload(raw: unknown): {
	hasPayload: boolean;
	hasText: boolean;
	textLength?: number;
	hasFormat: boolean;
	hasSource: boolean;
} {
	const payload = asRecordPayload(raw);
	if (!payload) {
		return {
			hasPayload: false,
			hasText: false,
			hasFormat: false,
			hasSource: false,
		};
	}
	const text = typeof payload.text === "string" ? payload.text : "";
	const source = payload.source;
	return {
		hasPayload: true,
		hasText: Boolean(text.trim()),
		textLength: text.length,
		hasFormat: typeof payload.format === "string",
		hasSource:
			typeof source === "string"
				? Boolean(source.trim())
				: Boolean(source && typeof source === "object" && "type" in source),
	};
}

function logDesktopImportDiagnostic(
	invoke: DesktopInvoke,
	stage: string,
	raw: unknown,
	handled: boolean,
): void {
	const summary = summarizeImportServerPayload(raw);
	void invoke("mcp_deep_link_log_frontend_import", {
		payload: {
			stage,
			handled,
			hasPayload: summary.hasPayload,
			hasText: summary.hasText,
			textLen: summary.textLength,
			hasFormat: summary.hasFormat,
			hasSource: summary.hasSource,
			currentPath: window.location.pathname,
		},
	}).catch((error) => {
		if (import.meta.env.DEV) {
			console.warn("[Layout] Failed to log desktop import diagnostic", error);
		}
	});
}

function handleImportServerPayload(navigate: ReturnType<typeof useNavigate>, raw: unknown): boolean {
	const payload = asRecordPayload(raw);
	if (!payload) {
		return false;
	}
	const text = typeof payload.text === "string" ? payload.text : "";
	if (!text.trim()) {
		return false;
	}

	const format = typeof payload.format === "string" ? payload.format : undefined;
	const rawSource = payload.source;
	const source = rawSource && typeof rawSource === "object" && "type" in rawSource
		? rawSource as ServerSource
		: typeof rawSource === "string" && rawSource.trim()
			? { type: "browser" as const }
			: undefined;
	useAppStore.getState().setPendingServerDeepLinkImport({
		text,
		format,
		source,
	});
	navigate("/servers");
	return true;
}

export function Layout() {
	const queryClient = useQueryClient();
	const { sidebarOpen, setSidebarOpen } = useAppStore();
	const navigate = useNavigate();
	const location = useLocation();
	const { t, i18n } = useTranslation();
	const demoMode = isBoardDemoMode();
	const { data: onboardingResp } = useQuery({
		queryKey: ["onboardingStatus"],
		queryFn: () => onboardingApi.getStatus(),
		enabled: !demoMode,
		staleTime: 60_000,
		retry: false,
		refetchOnWindowFocus: false,
	});

	React.useEffect(() => {
		if (demoMode) {
			return;
		}
		if (location.pathname === "/onboarding") {
			return;
		}
		if (onboardingResp?.data?.completed === false) {
			navigate("/onboarding", { replace: true });
		}
	}, [demoMode, location.pathname, navigate, onboardingResp?.data?.completed]);

	React.useEffect(() => {
		const staleMs = 30 * 1000;
		void queryClient.prefetchQuery({
			queryKey: ["systemMetrics"],
			queryFn: systemApi.getMetrics,
			staleTime: staleMs,
			retry: false,
		});
		void queryClient.prefetchQuery({
			queryKey: ["configSuits", "dashboard"],
			queryFn: configSuitsApi.getAll,
			staleTime: staleMs,
			retry: false,
		});
		void queryClient.prefetchQuery({
			queryKey: ["servers"],
			queryFn: serversApi.getAll,
			staleTime: staleMs,
			retry: false,
		});
		void queryClient.prefetchQuery({
			queryKey: ["audit", "mcp-calls-stats"],
			queryFn: () => auditApi.list({ limit: 1000 }),
			staleTime: staleMs,
			retry: false,
		});
	}, [queryClient]);

	React.useEffect(() => {
		let unlistenSettings: (() => void) | undefined;
		let unlistenImportServer: (() => void) | undefined;
		let unlistenCoreState: (() => void) | undefined;
		let cancelled = false;

		async function bind(): Promise<void> {
			if (!isTauriEnvironmentSync()) {
				return;
			}
			try {
				const { listen } = await import("@tauri-apps/api/event");
				const { invoke } = await import("@tauri-apps/api/core");
				if (cancelled) {
					return;
				}
				unlistenSettings = await listen(
					"mcpmate://open-settings",
					(event) => {
						const payload = asRecordPayload(event.payload);
						const tab = typeof payload?.tab === "string" ? payload.tab : undefined;
						const target = tab ? `/settings?tab=${tab}` : "/settings";
						navigate(target);
					},
				);
				unlistenImportServer = await listen("mcp-import/server", (event) => {
					const handled = handleImportServerPayload(navigate, event.payload);
					logDesktopImportDiagnostic(
						invoke,
						"event:mcp-import/server",
						event.payload,
						handled,
					);
				});
				unlistenCoreState = await listen("mcpmate://core/status-changed", (event) => {
					const payload = asRecordPayload(event.payload);
					const apiBaseUrl = payload?.apiBaseUrl;
					if (typeof apiBaseUrl === "string") {
						applyDesktopApiBaseUrl(apiBaseUrl);
						void invalidateDesktopCoreQueries(queryClient);
					}
				});

				const pending = await invoke<unknown>(
					"mcp_deep_link_take_pending_server_import",
				);
				const handled = handleImportServerPayload(navigate, pending);
				logDesktopImportDiagnostic(
					invoke,
					"invoke:mcp_deep_link_take_pending_server_import",
					pending,
					handled,
				);
			} catch (error) {
				if (import.meta.env.DEV) {
					console.warn("[Layout] Failed to bind desktop shell events", error);
				}
			}
		}

		void bind();
		return () => {
			cancelled = true;
			if (unlistenSettings) {
				void unlistenSettings();
			}
			if (unlistenImportServer) {
				void unlistenImportServer();
			}
			if (unlistenCoreState) {
				void unlistenCoreState();
			}
		};
	}, [navigate, queryClient]);

	const sidebarOpenRef = React.useRef(sidebarOpen);
	React.useEffect(() => {
		sidebarOpenRef.current = sidebarOpen;
	}, [sidebarOpen]);

	// Responsive sidebar: auto-collapse once on entering narrow widths, and allow
	// manual expand while narrow. Re-open on wide screens only if last close was auto.
	React.useEffect(() => {
		const autoKey = "mcp_sidebar_auto";
		const BREAKPOINT = 1200;
		const handler = () => {
			try {
				const w = window.innerWidth;
				if (w < BREAKPOINT) {
					if (sidebarOpenRef.current) {
						setSidebarOpen(false);
						try {
							localStorage.setItem(autoKey, "1");
						} catch {
							/* noop */
						}
					}
					return;
				}
				// Wide screens: only auto-open if last close was auto
				let wasAuto = false;
				try {
					wasAuto = localStorage.getItem(autoKey) === "1";
				} catch {
					/* noop */
				}
				if (w >= BREAKPOINT && wasAuto && !sidebarOpenRef.current) {
					setSidebarOpen(true);
					try {
						localStorage.removeItem(autoKey);
					} catch {
						/* noop */
					}
				}
			} catch {
				/* noop */
			}
		};
		handler();
		window.addEventListener("resize", handler);
		return () => window.removeEventListener("resize", handler);
	}, [setSidebarOpen]);

	const termsLabel = t("layout.terms", { defaultValue: "Terms" });
	const privacyLabel = t("layout.privacy", { defaultValue: "Privacy" });
	const langParam = websiteLangParam(i18n.language);
	const termsHref = `https://mcp.umate.ai/terms?lang=${langParam}`;
	const privacyHref = `https://mcp.umate.ai/privacy?lang=${langParam}`;

	return (
		<div className="h-screen flex flex-col overflow-hidden">
			<Sidebar />
			<Header />
			<main
				className={`flex-1 min-h-0 min-w-0 pt-16 transition-all duration-300 ease-in-out ${sidebarOpen ? "ml-64" : "ml-16"
					}`}
			>
				{/* Viewport-height column: outlet fills space above footer; pages can use h-full + inner scroll */}
				<div className="box-border flex h-full w-full min-w-0 flex-col gap-4 overflow-hidden p-4">
					<div className="flex min-h-0 min-w-0 flex-1 flex-col overflow-y-auto">
						<Outlet />
					</div>
					<footer className="shrink-0 text-[11px] text-slate-500 border-t border-slate-200 dark:border-slate-700 pt-2 pb-1 flex flex-col sm:flex-row sm:items-center sm:justify-between gap-2">
						<div className="flex items-center gap-4 flex-wrap">
							<a
								className="hover:underline"
								href="https://mcp.umate.ai"
								target="_blank"
								rel="noreferrer"
							>
								{t("layout.copyright", {
									defaultValue: "© 2026 MCPMate",
								})}
							</a>
							<div className="flex items-center gap-3">
								<a
									className="hover:underline"
									href={termsHref}
									target="_blank"
									rel="noreferrer"
								>
									{termsLabel}
								</a>
								<span className="text-slate-300">•</span>
								<a
									className="hover:underline"
									href={privacyHref}
									target="_blank"
									rel="noreferrer"
								>
									{privacyLabel}
								</a>
							</div>
						</div>
						<div className="flex items-center gap-2">
							<a
								className="inline-flex items-center gap-1.5 hover:underline text-inherit"
								href={MCPMATE_DISCORD_COMMUNITY_HREF}
								target="_blank"
								rel="noopener noreferrer"
								aria-label={t("layout.discordAria", {
									defaultValue: "Open MCPMate Discord community in a new tab",
								})}
								title={t("layout.discordAria", {
									defaultValue: "Open MCPMate Discord community in a new tab",
								})}
							>
								<MessagesSquare className="h-3.5 w-3.5 shrink-0 text-[#5865F2]" aria-hidden />
								<span>{t("layout.discord", { defaultValue: "Discord" })}</span>
							</a>
						</div>
					</footer>
				</div>
			</main>
		</div>
	);
}
