import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Download, RotateCcw } from "lucide-react";
import { lazy, Suspense, useEffect, useRef, useState, type ReactNode } from "react";
import {
	BrowserRouter,
	Navigate,
	Route,
	Routes,
	useLocation,
	useNavigate,
	useParams,
} from "react-router-dom";
import { useTranslation } from "react-i18next";
import { Layout } from "./components/layout/layout";
import { LanguageSynchronizer } from "./components/language-synchronizer.ts";
import { MasterPasswordGate } from "./components/master-password-gate";
import { ThemeSynchronizer } from "./components/theme-synchronizer";
import { ApiDocsPage } from "./pages/api-docs/api-docs-page";
import { AuditPage } from "./pages/audit/audit-page";
import { ClientDirectCapabilitiesPage } from "./pages/clients/client-direct-page.tsx";
import { ClientsPage } from "./pages/clients/clients-page";
import { DashboardPage } from "./pages/dashboard/dashboard-page";
import { MarketDetailPage } from "./pages/market/market-detail-page";
import { MarketPage } from "./pages/market/market-page";
import { CatalogProvider } from "./lib/market";
import { NotFoundPage } from "./pages/not-found-page";
import { ProfileDetailPage } from "./pages/profile/profile-detail-page";
import { ProfilePage } from "./pages/profile/profile-page";
import { ProfilePresetPage } from "./pages/profile/profile-preset-page";
import { RuntimePage } from "./pages/runtime/runtime-page";
import { SecretsPage } from "./pages/secrets/secrets-page";
import { InstanceDetailPage } from "./pages/servers/instance-detail-page";
import { ServerDetailPage } from "./pages/servers/server-detail-page";
import { OAuthCallbackPage } from "./pages/servers/oauth-callback-page";
import { ServerListPage } from "./pages/servers/server-list-page";
import { SettingsPage } from "./pages/settings/settings-page";
import { OnboardingPage } from "./pages/onboarding/onboarding-page";
import { TrayOperatorPanelPage } from "./pages/operator/tray-operator-panel-page";
import { OperatorBackendWaitingPage } from "./pages/operator/operator-backend-waiting";
import {
	API_BASE_URL,
	notificationsService,
	setApiBaseUrl,
	systemApi,
} from "./lib/api";
import {
	describeBackendReadinessIssue,
	describeCoreStartupIssue,
	backendReadinessStatusKey,
	shouldReportBackendReadinessAttempt,
	translateBackendReadinessIssue,
	type CoreStartupSnapshot,
	type BackendReadinessIssue,
} from "./lib/backend-readiness-diagnostics";
import {
	exportDesktopDiagnostics,
	recordDesktopDiagnosticEvent,
} from "./lib/desktop-diagnostics";
import { shouldBlockDesktopDropNavigation } from "./lib/desktop-drop-guard";
import { isBoardDemoMode } from "./lib/demo-mode";
import { isTauriEnvironmentSync } from "./lib/platform";

const ClientDetailPage = lazy(() =>
	import("./pages/clients/client-detail-page").then((module) => ({
		default: module.ClientDetailPage,
	})),
);

const OPEN_FULL_BOARD_PATH_EVENT = "mcpmate://open-full-board-path";

let pendingFullBoardNavigation:
	| {
		path: string;
		pathname: string;
	}
	| null = null;
let pendingFullBoardNavigationTake:
	| Promise<{
		path: string;
		pathname: string;
	} | null>
	| null = null;

// Initialize the query client
const queryClient = new QueryClient({
	defaultOptions: {
		queries: {
			staleTime: 30 * 1000, // 30 seconds
			retry: 1,
			refetchOnWindowFocus: true,
		},
	},
});

function App() {
	const demoMode = isBoardDemoMode();
	const router = (
		<BrowserRouter
			future={{
				v7_startTransition: true,
				v7_relativeSplatPath: true,
			}}
		>
			<DesktopFullBoardPathBridge>
				<BackspaceNavigationGuard />
				<DesktopDropNavigationGuard />
				<Routes>
					<Route path="oauth/callback" element={<OAuthCallbackPage />} />
					<Route path="onboarding" element={<OnboardingPage />} />
					<Route path="operator" element={<TrayOperatorPanelPage />} />
					<Route path="/" element={<Layout />}>
						<Route index element={<DashboardPage />} />
						{/* New canonical routes */}
						<Route path="profiles" element={<ProfilePage />} />
						<Route
							path="profiles/presets/:presetId"
							element={<ProfilePresetPage />}
						/>
						<Route path="profiles/:profileId" element={<ProfileDetailPage />} />
						{/* Back-compat: redirect old routes */}
						<Route
							path="config"
							element={<Navigate to="/profiles" replace />}
						/>
						<Route
							path="config/presets/:presetId"
							element={<LegacyPresetRedirect />}
						/>
						<Route
							path="config/suits/:suitId"
							element={<LegacySuitRedirect />}
						/>
						<Route
							path="config/profiles/:suitId"
							element={<LegacySuitRedirect />}
						/>
						<Route path="market" element={<CatalogProvider><MarketPage /></CatalogProvider>} />
						<Route path="market/:registryKey" element={<CatalogProvider><MarketDetailPage /></CatalogProvider>} />
						<Route path="servers" element={<ServerListPage />} />
						<Route path="servers/:serverId" element={<ServerDetailPage />} />
						<Route
							path="servers/:serverId/instances/:instanceId"
							element={<InstanceDetailPage />}
						/>
						{/* Tools route removed */}
						<Route path="clients" element={<ClientsPage />} />
						<Route
							path="clients/:identifier/direct/:serverId"
							element={<ClientDirectCapabilitiesPage />}
						/>
						<Route
							path="clients/:identifier"
							element={
								<Suspense fallback={null}>
									<ClientDetailPage />
								</Suspense>
							}
						/>
						<Route path="runtime" element={<RuntimePage />} />
						<Route path="secrets" element={<SecretsPage />} />
						<Route path="audit" element={<AuditPage />} />
						<Route path="api-docs" element={<ApiDocsPage />} />
						<Route path="account" element={<Navigate to="/" replace />} />
						<Route path="settings" element={<SettingsPage />} />

						<Route path="404" element={<NotFoundPage />} />
						<Route path="*" element={<Navigate to="/404" replace />} />
					</Route>
				</Routes>
			</DesktopFullBoardPathBridge>
		</BrowserRouter>
	);

	return (
		<QueryClientProvider client={queryClient}>
			<LanguageSynchronizer />
			<ThemeSynchronizer />
			{demoMode ? router : (
				<BackendReadinessGate>
					<MasterPasswordGate>{router}</MasterPasswordGate>
				</BackendReadinessGate>
			)}
		</QueryClientProvider>
	);
}

function isEditableBackspaceTarget(target: EventTarget | null): boolean {
	if (!(target instanceof HTMLElement)) {
		return false;
	}
	if (target.isContentEditable) {
		return true;
	}
	const tagName = target.tagName.toLowerCase();
	if (tagName === "textarea") {
		return true;
	}
	if (tagName !== "input") {
		return false;
	}
	const input = target as HTMLInputElement;
	return !input.readOnly && !input.disabled;
}

function shouldPreventBackspaceNavigation(event: KeyboardEvent): boolean {
	return (
		!event.defaultPrevented &&
		event.key === "Backspace" &&
		!event.metaKey &&
		!event.ctrlKey &&
		!event.altKey &&
		!isEditableBackspaceTarget(event.target)
	);
}

function BackspaceNavigationGuard() {
	useEffect(() => {
		const handleKeyDown = (event: KeyboardEvent) => {
			if (shouldPreventBackspaceNavigation(event)) {
				event.preventDefault();
			}
		};

		window.addEventListener("keydown", handleKeyDown);
		return () => window.removeEventListener("keydown", handleKeyDown);
	}, []);

	return null;
}

function toFullBoardPath(raw: unknown): string | undefined {
	if (typeof raw !== "string") {
		return undefined;
	}
	if (!raw.startsWith("/") || raw.startsWith("//")) {
		return undefined;
	}
	const routePath = raw.split(/[?#]/, 1)[0].replace(/\/+$/, "");
	if (routePath === "/operator" || routePath.startsWith("/operator/")) {
		return undefined;
	}
	return raw;
}

function DesktopDropNavigationGuard() {
	useEffect(() => {
		if (!isTauriEnvironmentSync()) {
			return;
		}

		const handleDragOver = (event: DragEvent) => {
			if (
				event.defaultPrevented ||
				!shouldBlockDesktopDropNavigation(event.dataTransfer, event.target)
			) {
				return;
			}
			event.preventDefault();
			if (event.dataTransfer) {
				event.dataTransfer.dropEffect = "none";
			}
		};

		const handleDrop = (event: DragEvent) => {
			if (
				event.defaultPrevented ||
				!shouldBlockDesktopDropNavigation(event.dataTransfer, event.target)
			) {
				return;
			}
			event.preventDefault();
			event.stopPropagation();
		};

		window.addEventListener("dragover", handleDragOver);
		window.addEventListener("drop", handleDrop);
		return () => {
			window.removeEventListener("dragover", handleDragOver);
			window.removeEventListener("drop", handleDrop);
		};
	}, []);

	return null;
}

function DesktopFullBoardPathBridge({ children }: { children: ReactNode }) {
	const navigate = useNavigate();
	const location = useLocation();
	const isOperatorRoute = location.pathname === "/operator";
	const [ready, setReady] = useState(false);
	const [pendingNavigationPath, setPendingNavigationPath] = useState<string | null>(null);

	useEffect(() => {
		if (!pendingNavigationPath) {
			return;
		}
		if (location.pathname === pendingNavigationPath) {
			if (pendingFullBoardNavigation?.pathname === pendingNavigationPath) {
				pendingFullBoardNavigation = null;
				pendingFullBoardNavigationTake = null;
			}
			setPendingNavigationPath(null);
			setReady(true);
		}
	}, [location.pathname, pendingNavigationPath]);

	useEffect(() => {
		if (!isTauriEnvironmentSync() || isOperatorRoute) {
			setReady(true);
			return;
		}

		if (
			pendingFullBoardNavigation &&
			location.pathname !== pendingFullBoardNavigation.pathname
		) {
			setPendingNavigationPath(pendingFullBoardNavigation.pathname);
			navigate(pendingFullBoardNavigation.path, { replace: true });
			return;
		}

		let cancelled = false;
		let unlisten: (() => void) | undefined;

		async function takeAndNavigate(): Promise<boolean> {
			try {
				if (!pendingFullBoardNavigationTake) {
					pendingFullBoardNavigationTake = (async () => {
						const { invoke } = await import("@tauri-apps/api/core");
						const rawPath = await invoke<unknown>("mcp_shell_take_pending_full_board_path");
						const path = toFullBoardPath(rawPath);
						if (!path) {
							if (rawPath !== null && rawPath !== undefined && import.meta.env.DEV) {
								console.warn("[App] Ignoring invalid full board path", rawPath);
							}
							return null;
						}
						const pathname = path.split(/[?#]/, 1)[0].replace(/\/+$/, "") || "/";
						return { path, pathname };
					})();
				}
				const navigation = await pendingFullBoardNavigationTake;
				if (cancelled) {
					return false;
				}
				if (!navigation) {
					pendingFullBoardNavigationTake = null;
					return false;
				}
				if (navigation) {
					pendingFullBoardNavigation = navigation;
					setPendingNavigationPath(navigation.pathname);
					navigate(navigation.path, { replace: true });
					return true;
				}
			} catch (error) {
				pendingFullBoardNavigationTake = null;
				if (import.meta.env.DEV) {
					console.warn("[App] Failed to take pending full board path", error);
				}
			}
			return false;
		}

		async function bind(): Promise<void> {
			let waitingForNavigation = false;
			try {
				const { listen } = await import("@tauri-apps/api/event");
				unlisten = await listen(OPEN_FULL_BOARD_PATH_EVENT, () => {
					void takeAndNavigate();
				});
				const isNavigating = await takeAndNavigate();
				if (isNavigating) {
					waitingForNavigation = true;
					return;
				}
			} catch (error) {
				if (import.meta.env.DEV) {
					console.warn("[App] Failed to bind full board path delivery", error);
				}
			} finally {
				if (!cancelled && !waitingForNavigation) {
					setReady(true);
				}
			}
		}

		void bind();
		return () => {
			cancelled = true;
			if (unlisten) {
				void unlisten();
			}
		};
	}, [isOperatorRoute, location.pathname, navigate]);

	return ready ? <>{children}</> : null;
}

type BackendReadinessMessageKey = "starting" | "waitingForBackend" | "confirmingReadiness";

const backendReadinessFallbacks: Record<BackendReadinessMessageKey, string> = {
	starting: "Starting MCPMate Core...",
	waitingForBackend: "Waiting for MCPMate backend",
	confirmingReadiness: "Confirming backend readiness",
};

const DEV_CORE_SOURCE_PATH = "/__mcpmate/dev-core-source";

function BackendReadinessGate({ children }: { children: ReactNode }) {
	const { t } = useTranslation();
	const [coreSourceReady, setCoreSourceReady] = useState(
		() => !isTauriEnvironmentSync() && !import.meta.env.DEV,
	);
	const [backendReady, setBackendReady] = useState(false);
	const [attempt, setAttempt] = useState(0);
	const [messageKey, setMessageKey] = useState<BackendReadinessMessageKey>("starting");
	const [readinessIssue, setReadinessIssue] = useState<BackendReadinessIssue | null>(null);
	const [diagnosticsExporting, setDiagnosticsExporting] = useState(false);
	const [diagnosticsExportPath, setDiagnosticsExportPath] = useState<string | null>(null);
	const [diagnosticsExportError, setDiagnosticsExportError] = useState<string | null>(null);
	const [readinessStartedAtMs] = useState(() => Date.now());
	const lastDiagnosticRef = useRef<{
		reportedAtMs: number;
		statusKey: string;
	} | null>(null);
	const desktopCoreEventVersionRef = useRef(0);

	useEffect(() => {
		if (!isTauriEnvironmentSync()) {
			if (import.meta.env.DEV) {
				let cancelled = false;
				void syncDevCoreSource().finally(() => {
					if (!cancelled) {
						setCoreSourceReady(true);
					}
				});
				return () => {
					cancelled = true;
				};
			}
			setCoreSourceReady(true);
			return;
		}

		let cancelled = false;
		let unlisten: (() => void) | undefined;
		const applyDesktopCoreSource = (source: CoreStartupSnapshot) => {
			if (typeof source.apiBaseUrl === "string") {
				setApiBaseUrl(source.apiBaseUrl);
				notificationsService.reconnectAfterApiBaseChanged();
			}
			setReadinessIssue(describeCoreStartupIssue(source));
		};
		const syncDesktopCoreSource = async () => {
			try {
				const { listen } = await import("@tauri-apps/api/event");
				unlisten = await listen("mcpmate://core/status-changed", (event) => {
					if (cancelled) {
						return;
					}
					desktopCoreEventVersionRef.current += 1;
					applyDesktopCoreSource(event.payload as CoreStartupSnapshot);
					setCoreSourceReady(true);
					setAttempt((value) => value + 1);
				});
			} catch (error) {
				if (import.meta.env.DEV) {
					console.warn("[App] Failed to bind desktop core source listener", error);
				}
			}

			try {
				const { invoke } = await import("@tauri-apps/api/core");
				const eventVersion = desktopCoreEventVersionRef.current;
				const source = (await invoke("mcp_shell_read_core_source")) as CoreStartupSnapshot;
				if (!cancelled && desktopCoreEventVersionRef.current === eventVersion) {
					applyDesktopCoreSource(source);
				}
			} catch (error) {
				if (!cancelled) {
					setReadinessIssue(
						describeBackendReadinessIssue(
							null,
							error,
							API_BASE_URL,
						),
					);
				}
				if (import.meta.env.DEV) {
					console.warn("[App] Failed to resolve desktop core source", error);
				}
			} finally {
				if (!cancelled) {
					setCoreSourceReady(true);
				}
			}
		};

		void syncDesktopCoreSource();

		return () => {
			cancelled = true;
			if (unlisten) {
				void unlisten();
			}
		};
	}, []);

	useEffect(() => {
		if (!coreSourceReady || backendReady || readinessIssue?.terminal) {
			return;
		}

		let cancelled = false;
		let retryTimer: number | undefined;

		function scheduleRetry(): void {
			if (cancelled) {
				return;
			}
			retryTimer = window.setTimeout(() => setAttempt((value) => value + 1), 1_000);
		}

		function reportReadinessWait(payload: unknown, error: unknown): void {
			const nowMs = Date.now();
			const reportAttempt = attempt + 1;
			const elapsedMs = nowMs - readinessStartedAtMs;
			const statusKey = backendReadinessStatusKey(
				isReadinessPayload(payload) ? payload : null,
				error,
			);
			if (
				!shouldReportBackendReadinessAttempt({
					attempt: reportAttempt,
					lastReportedAtMs: lastDiagnosticRef.current?.reportedAtMs ?? null,
					lastStatusKey: lastDiagnosticRef.current?.statusKey ?? null,
					nowMs,
					statusKey,
				})
			) {
				return;
			}

			lastDiagnosticRef.current = { reportedAtMs: nowMs, statusKey };
			const data = { attempt: reportAttempt, elapsedMs, statusKey };
			console.info("[MCPMate] waiting for backend readiness", data);
			void recordDesktopDiagnosticEvent({
				level: "info",
				source: "backend-readiness",
				message: "waiting for backend readiness",
				data,
			}).catch((error) => {
				if (import.meta.env.DEV) {
					console.warn("[MCPMate] failed to persist readiness diagnostic", error);
				}
			});
		}

		async function checkReadiness(): Promise<void> {
			setMessageKey("waitingForBackend");
			let payload: unknown = null;
			let readinessError: unknown = null;
			try {
				setMessageKey("confirmingReadiness");
				payload = await systemApi.getReadiness();
				if (cancelled) {
					return;
				}
				if (isReadinessPayload(payload) && payload.type === "ready" && payload.status === "ok") {
					setReadinessIssue(null);
					setBackendReady(true);
					return;
				}
			} catch (error) {
				readinessError = error;
				// Keep retrying until the backend API is reachable.
			}
			setReadinessIssue((current) =>
				current?.statusKey.startsWith("core:")
					? current
					: describeBackendReadinessIssue(
							isReadinessPayload(payload) ? payload : null,
							readinessError,
							API_BASE_URL,
						),
			);
			reportReadinessWait(payload, readinessError);
			scheduleRetry();
		}

		void checkReadiness();

		return () => {
			cancelled = true;
			if (retryTimer !== undefined) {
				window.clearTimeout(retryTimer);
			}
		};
	}, [
		attempt,
		backendReady,
		coreSourceReady,
		readinessIssue?.terminal,
		readinessStartedAtMs,
	]);

	async function handleRetryCoreStartup(): Promise<void> {
		const { invoke } = await import("@tauri-apps/api/core");
		await invoke("mcp_shell_manage_local_core_service", {
			action: readinessIssue?.action === "restart" ? "restart" : "start",
		});
		setReadinessIssue(null);
		setDiagnosticsExportError(null);
		setDiagnosticsExportPath(null);
		setMessageKey("starting");
		setAttempt((value) => value + 1);
	}

	async function handleExportDiagnostics(): Promise<void> {
		setDiagnosticsExporting(true);
		setDiagnosticsExportPath(null);
		setDiagnosticsExportError(null);
		try {
			const response = await exportDesktopDiagnostics();
			if (response?.exportPath) {
				setDiagnosticsExportPath(response.exportPath);
			}
		} catch (error) {
			setDiagnosticsExportError(error instanceof Error ? error.message : String(error));
		} finally {
			setDiagnosticsExporting(false);
		}
	}

	if (!backendReady) {
		const waitingProps = {
			diagnosticsAvailable: isTauriEnvironmentSync(),
			diagnosticsExportError,
			diagnosticsExporting,
			diagnosticsExportPath,
			issue: readinessIssue,
			onExportDiagnostics: handleExportDiagnostics,
			onRetryCoreStartup: handleRetryCoreStartup,
		};
		if (isOperatorSurfacePath()) {
			return <OperatorBackendWaitingPage messageKey={messageKey} {...waitingProps} />;
		}
		return (
			<BackendWaitingPage
				message={t(`backendReadiness.${messageKey}`, {
					defaultValue: backendReadinessFallbacks[messageKey],
				})}
				{...waitingProps}
			/>
		);
	}

	return <>{children}</>;
}

async function syncDevCoreSource(): Promise<boolean> {
	if (!import.meta.env.DEV || isTauriEnvironmentSync()) {
		return false;
	}
	try {
		const response = await fetch(DEV_CORE_SOURCE_PATH, { cache: "no-store" });
		if (!response.ok) {
			return false;
		}
		const payload: unknown = await response.json();
		if (!isDevCoreSourcePayload(payload)) {
			return false;
		}
		if (payload.apiBaseUrl === API_BASE_URL) {
			return false;
		}
		setApiBaseUrl(payload.apiBaseUrl);
		notificationsService.reconnectAfterApiBaseChanged();
		return true;
	} catch {
		return false;
	}
}

function isDevCoreSourcePayload(value: unknown): value is { apiBaseUrl: string } {
	if (typeof value !== "object" || value === null) {
		return false;
	}
	const candidate = value as { apiBaseUrl?: unknown };
	return typeof candidate.apiBaseUrl === "string" && candidate.apiBaseUrl.trim().length > 0;
}

function isReadinessPayload(value: unknown): value is {
	reason?: string;
	status?: string;
	type?: string;
} {
	return typeof value === "object" && value !== null;
}

function isOperatorSurfacePath(): boolean {
	if (typeof window === "undefined") {
		return false;
	}
	const normalized = window.location.pathname.replace(/\/+$/, "") || "/";
	return normalized === "/operator";
}

function BackendWaitingPage({
	diagnosticsAvailable,
	diagnosticsExportError,
	diagnosticsExporting,
	diagnosticsExportPath,
	issue,
	message,
	onExportDiagnostics,
	onRetryCoreStartup,
}: {
	diagnosticsAvailable: boolean;
	diagnosticsExportError: string | null;
	diagnosticsExporting: boolean;
	diagnosticsExportPath: string | null;
	issue: BackendReadinessIssue | null;
	message: string;
	onExportDiagnostics: () => Promise<void>;
	onRetryCoreStartup: () => Promise<void>;
}) {
	const { t } = useTranslation();
	const terminalMessage = issue
		? translateBackendReadinessIssue(t, issue)
		: t("backendReadiness.startupFailed", {
				defaultValue: "MCPMate Core did not finish starting.",
			});

	return (
		<div className="relative flex min-h-screen items-center justify-center bg-slate-50 px-6 text-slate-900 dark:bg-slate-950 dark:text-white">
			<div className="flex max-w-sm flex-col items-center text-center">
				<img
					src="/logo.svg"
					alt="MCPMate"
					className="mb-6 h-12 w-12 object-contain dark:invert dark:brightness-0"
				/>
				<div
					className={`mb-4 h-8 w-8 rounded-full border-2 border-slate-300 border-t-emerald-500 ${
						issue?.terminal ? "" : "animate-spin"
					}`}
				/>
				<h1 className="text-xl font-semibold">
					{issue?.terminal
						? t("backendReadiness.failedTitle", { defaultValue: "MCPMate Core failed to start" })
						: t("backendReadiness.title", { defaultValue: "MCPMate is starting" })}
				</h1>
				<p className="mt-2 text-sm text-slate-500 dark:text-slate-400">
					{issue?.terminal ? terminalMessage : message}
				</p>
			</div>
			<StartupAttentionFooter
				diagnosticsAvailable={diagnosticsAvailable}
				diagnosticsExportError={diagnosticsExportError}
				diagnosticsExporting={diagnosticsExporting}
				diagnosticsExportPath={diagnosticsExportPath}
				issue={issue}
				onExportDiagnostics={onExportDiagnostics}
				onRetryCoreStartup={onRetryCoreStartup}
			/>
		</div>
	);
}

function StartupAttentionFooter({
	diagnosticsAvailable,
	diagnosticsExportError,
	diagnosticsExporting,
	diagnosticsExportPath,
	issue,
	onExportDiagnostics,
	onRetryCoreStartup,
}: {
	diagnosticsAvailable: boolean;
	diagnosticsExportError: string | null;
	diagnosticsExporting: boolean;
	diagnosticsExportPath: string | null;
	issue: BackendReadinessIssue | null;
	onExportDiagnostics: () => Promise<void>;
	onRetryCoreStartup: () => Promise<void>;
}) {
	const { t } = useTranslation();
	if (!issue && !diagnosticsExportError && !diagnosticsExportPath) {
		return null;
	}

	let detail = issue ? translateBackendReadinessIssue(t, issue) : undefined;
	if (diagnosticsExportError) {
		detail = t("backendReadiness.exportFailed", {
			defaultValue: "Unable to export diagnostics: {{error}}",
			error: diagnosticsExportError,
		});
	} else if (diagnosticsExportPath) {
		detail = t("backendReadiness.exportSuccess", {
			defaultValue: "Diagnostics exported to {{path}}",
			path: diagnosticsExportPath,
		});
	}

	return (
		<div className="pointer-events-none fixed inset-x-0 bottom-5 z-10 flex justify-center px-4">
			<div
				className="pointer-events-auto flex h-10 max-w-[min(48rem,calc(100vw-2rem))] items-center gap-2 rounded-full border border-amber-200/70 bg-white/85 px-3 text-xs text-slate-600 opacity-75 shadow-sm shadow-slate-950/5 backdrop-blur transition-opacity duration-200 hover:opacity-100 dark:border-amber-400/20 dark:bg-slate-950/80 dark:text-slate-300"
				aria-live="polite"
				title={detail}
			>
				<span className="relative flex h-2.5 w-2.5 flex-none" aria-hidden="true">
					<span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-amber-400 opacity-40" />
					<span className="relative inline-flex h-2.5 w-2.5 rounded-full bg-amber-500" />
				</span>
				<span className="min-w-0 truncate">{detail}</span>
				{issue?.action ? (
					<button
						type="button"
						onClick={() => void onRetryCoreStartup()}
						className="ml-1 inline-flex h-7 flex-none items-center gap-1 rounded-full border border-slate-200 bg-white px-2 text-[11px] font-medium text-slate-700 transition hover:border-slate-300 hover:bg-slate-50 dark:border-slate-700 dark:bg-slate-900 dark:text-slate-200 dark:hover:bg-slate-800"
					>
						<RotateCcw className="h-3 w-3" aria-hidden="true" />
						<span className="hidden sm:inline">
							{issue.action === "restart"
								? t("backendReadiness.restartCore", { defaultValue: "Restart Core" })
								: t("backendReadiness.retryCoreStartup", { defaultValue: "Retry startup" })}
						</span>
					</button>
				) : null}
				{diagnosticsAvailable ? (
					<button
						type="button"
						onClick={() => void onExportDiagnostics()}
						disabled={diagnosticsExporting}
						className="ml-1 inline-flex h-7 flex-none items-center gap-1 rounded-full border border-slate-200 bg-white px-2 text-[11px] font-medium text-slate-700 transition hover:border-slate-300 hover:bg-slate-50 disabled:cursor-not-allowed disabled:opacity-60 dark:border-slate-700 dark:bg-slate-900 dark:text-slate-200 dark:hover:bg-slate-800"
					>
						<Download className="h-3 w-3" aria-hidden="true" />
						<span className="hidden sm:inline">
							{diagnosticsExporting
								? t("backendReadiness.exportingDiagnostics", {
									defaultValue: "Exporting diagnostics...",
								})
								: t("backendReadiness.exportDiagnostics", {
									defaultValue: "Export diagnostics",
								})}
						</span>
					</button>
				) : null}
			</div>
		</div>
	);
}

function LegacyPresetRedirect() {
	const { presetId } = useParams();
	return <Navigate to={`/profiles/presets/${presetId ?? ""}`} replace />;
}

function LegacySuitRedirect() {
	const { suitId } = useParams();
	return <Navigate to={`/profiles/${suitId ?? ""}`} replace />;
}

export default App;
