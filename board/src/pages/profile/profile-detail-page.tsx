import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
	Check,
	Edit3,
	Eye,
	Play,
	RefreshCw,
	Square,
	Trash2,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { useNavigate, useParams, useSearchParams } from "react-router-dom";
import { usePageTranslations } from "../../lib/i18n/usePageTranslations";
import { capabilityRecordMatchesSearch } from "../../lib/capability-search";
import { capabilityKey, splitCapabilityKey } from "../../lib/capability-keys";
import { capabilityKindLabel } from "../../lib/capability-kind-label";
import { formatWorkflowToolBindingCount } from "../../lib/profile-workflow-metrics";
import { useCapabilityKindFilters } from "../../hooks/use-capability-kind-filters";
import { useUrlTab } from "../../lib/hooks/use-url-state";
import { CachedAvatar } from "../../components/cached-avatar";
import { AuditLogsPanel } from "../../components/audit-logs-panel";
import {
	BulkSelectionCheckbox,
	BulkSelectionHeader,
	useBulkSelection,
	useBulkSelectionLabels,
	useEnableDisableBulkActions,
} from "../../components/bulk-selection";
import CapabilityList, {
	type CapabilityKind,
} from "../../components/capability-list";
import { CapabilityManagementPanel } from "../../components/capability-management-panel";
import type { CapabilityPreviewFlatItem } from "../../components/capability-preview-list";
import { CardListScrollBody } from "../../components/card-list-scroll-body";
import { CAPABILITY_SCROLL_CARD_CLASS } from "../../components/capability-scroll-card-layout";
import {
	CapsuleStripeList,
	CapsuleStripeListItem,
	PROFILE_EDITOR_SIDEBAR_ITEM_CLASS,
	PROFILE_EDITOR_SIDEBAR_LIST_CLASS,
	PROFILE_EDITOR_SIDEBAR_SCROLL_CLASS,
} from "../../components/capsule-stripe-list";
import { CapsuleStripeRowBody } from "../../components/capsule-stripe-row";
import { ProfileFormDrawer } from "../../components/profile-form-drawer";
import { ResizableSplitPane } from "../../components/resizable-split-pane";
import {
	ProfileSurfaceMetrics,
	type ProfileSurfaceMetric,
} from "../../components/profile-surface-metrics";
import { ProfileWorkflowGuide } from "../../components/profile-workflow-guide";
import { buildWorkflowCapabilityOptions } from "../../lib/profile-workflow-specification";
import { SurfaceReviewDialog } from "../../components/surface-review-dialog";
import {
	DETAIL_TAB_CONTENT_CLASS,
	DETAIL_OVERVIEW_PINNED_SECTION_CLASS,
	DETAIL_OVERVIEW_STACK_CLASS,
} from "../../components/detail-tab-content-class";
import { ProfileTokenUsageChart } from "./components/profile-token-usage-chart";
import {
	resolveProfileDetailReviewTab,
	resolveProfileDetailTab,
} from "./profile-detail-tabs";
import {
	AlertDialog,
	AlertDialogAction,
	AlertDialogCancel,
	AlertDialogContent,
	AlertDialogDescription,
	AlertDialogFooter,
	AlertDialogHeader,
	AlertDialogTitle,
} from "../../components/ui/alert-dialog";
import { Avatar, AvatarFallback } from "../../components/ui/avatar";
import { Badge } from "../../components/ui/badge";
import { Button } from "../../components/ui/button";
import { ButtonGroup } from "../../components/ui/button-group";
import { Card, CardContent } from "../../components/ui/card";
import { Input } from "../../components/ui/input";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "../../components/ui/select";
import { Switch } from "../../components/ui/switch";
import {
	Tabs,
	TabsContent,
	TabsList,
	TabsTrigger,
} from "../../components/ui/tabs";
import {
	auditApi,
	configSuitsApi,
	serversApi,
	useProfileTokenChartSource,
} from "../../lib/api";
import { DEFAULT_ANCHOR_ROLE } from "../../lib/default-profile";
import { notifyError, notifySuccess } from "../../lib/notify";
import { handleProfileCapabilityMutationError } from "../../lib/profile-capability-conflict";
import {
	requireSelectedCapabilityRevisionSet,
	type CapabilityRevisionSource,
} from "../../lib/profile-capability-revisions";
import {
	ProfileSyncClientError,
	profileSyncErrorTranslationKey,
} from "../../lib/profile-sync-error";
import { useAppStore } from "../../lib/store";
import { toTitleCase } from "../../lib/utils";
import type {
	CatalogRevisionSet,
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
} from "../../lib/types";
import type { CapabilityRecord } from "../../types/capabilities";

const PROFILE_DETAIL_TABS = [
	"overview",
	"workflow",
	"capabilities",
];

const ALL_CAPABILITY_SERVERS_ID = "__all_servers__";

type ProfileFlatCapabilityItem = CapabilityRecord & {
	__profileCapabilityKind: CapabilityKind;
};

function requireMatchingAuthoringGeneration(
	generations: Array<number | undefined>,
): number {
	const available = generations.filter(
		(value): value is number => value !== undefined,
	);
	if (available.length !== generations.length || available.length === 0) {
		throw new ProfileSyncClientError("profile_authoring_state_missing");
	}
	if (available.some((value) => value !== available[0])) {
		throw new ProfileSyncClientError("profile_authoring_state_mismatch");
	}
	return available[0];
}

function requireCapabilityRevisionSet(
	selectedIds: string[],
	items: Array<{ id: string; server_id: string }> | undefined,
	sourceRevisionSet: CatalogRevisionSet | undefined,
): CatalogRevisionSet {
	return requireSelectedCapabilityRevisionSet([
		{
			selectedIds,
			items: items ?? [],
			sourceRevisionSet,
		},
	]);
}

function capabilityDetailsCacheToken(items: ProfileFlatCapabilityItem[]) {
	return items
		.map((item) =>
			[
				item.__profileCapabilityKind,
				item.id,
				item.server_id ?? "",
				item.enabled ? "1" : "0",
				item.description ?? "",
			].join(":"),
		)
		.join("|");
}

function firstCapabilityString(
	item: CapabilityRecord,
	keys: Array<keyof CapabilityRecord | string>,
) {
	for (const key of keys) {
		const value = item[key];
		if (typeof value === "string" && value.trim()) {
			return value;
		}
	}
	return undefined;
}

function profileCapabilityDetailKey(
	item: ProfileFlatCapabilityItem,
	kind: CapabilityKind,
) {
	if (kind === "tools") {
		return firstCapabilityString(item, ["unique_name"]);
	}
	if (kind === "resources") {
		return firstCapabilityString(item, ["unique_uri"]);
	}
	if (kind === "prompts") {
		return firstCapabilityString(item, ["unique_name"]);
	}
	return firstCapabilityString(item, ["unique_uri_template"]);
}

type ProfileGlobalServerSummary = {
	name?: string;
	icons?: Array<{ src?: string }>;
};

const compactSelectTriggerClass =
	"relative h-9 w-full min-w-9 px-2 pr-8 [&>span]:min-w-0 [&>span]:truncate [&>svg]:pointer-events-none [&>svg]:absolute [&>svg]:right-2.5 [&>svg]:top-1/2 [&>svg]:-translate-y-1/2";

export function ProfileDetailPage() {
	const { t, i18n } = useTranslation();
	usePageTranslations("profiles");
	usePageTranslations("servers");
	const { profileId } = useParams<{ profileId: string }>();
	const [searchParams, setSearchParams] = useSearchParams();
	const queryClient = useQueryClient();
	const navigate = useNavigate();

	const openServerDetail = useCallback(
		(targetServerId: string) => {
			navigate(`/servers/${encodeURIComponent(targetServerId)}`);
		},
		[navigate],
	);

	/** Refetch capability JSON payloads when server membership or live MCP definitions may have changed. */
	const invalidateProfileCapabilityLedger = useCallback(() => {
		if (profileId) {
			void queryClient.invalidateQueries({
				queryKey: ["capabilityTokenLedger", profileId],
			});
			void queryClient.invalidateQueries({
				queryKey: ["profileChartTokenEstimate", profileId],
			});
		}
	}, [profileId, queryClient]);

	const showProfileLiveLogs = useAppStore(
		(state) => state.dashboardSettings.showProfileLiveLogs,
	);
	const profileTokenEstimateMethod = useAppStore(
		(state) => state.dashboardSettings.profileTokenEstimateMethod,
	);
	const { activeTab } = useUrlTab({
		paramName: "tab",
		defaultTab: "overview",
		validTabs: PROFILE_DETAIL_TABS,
	});

	const mode = searchParams.get("mode");
	const reviewItemId = searchParams.get("review_item");
	const reviewRefId = searchParams.get("ref_id");
	const hasExplicitTab = searchParams.has("tab");
	const [workflowGuideDirty, setWorkflowGuideDirty] = useState(false);
	const [pendingWorkflowTab, setPendingWorkflowTab] = useState<string | null>(
		null,
	);
	const applyDetailTabChange = useCallback(
		(tab: string) => {
			setSearchParams(
				(current) => {
					const next = new URLSearchParams(current);
					next.set("tab", tab);
					next.delete("id");
					next.delete("step");
					next.delete("material");
					return next;
				},
				{ replace: true },
			);
		},
		[setSearchParams],
	);
	const handleDetailTabChange = useCallback(
		(tab: string) => {
			if (
				activeTab === "workflow" &&
				workflowGuideDirty &&
				tab !== activeTab
			) {
				setPendingWorkflowTab(tab);
				return;
			}
			applyDetailTabChange(tab);
		},
		[activeTab, applyDetailTabChange, workflowGuideDirty],
	);

	const [isEditDialogOpen, setIsEditDialogOpen] = useState(false);
	const [isDeleteDialogOpen, setIsDeleteDialogOpen] = useState(false);
	// Filters: servers
	const [serverQuery, setServerQuery] = useState("");
	const [serverStatus, setServerStatus] = useState<
		"all" | "enabled" | "disabled"
	>("all");
	const [selectedCapabilityServerId, setSelectedCapabilityServerId] =
		useState<string>(ALL_CAPABILITY_SERVERS_ID);
	const [capabilityQuery, setCapabilityQuery] = useState("");
	const [capabilityServerFilters, setCapabilityServerFilters] = useState<
		string[]
	>([]);
	const {
		kindFilters: capabilityKindFilters,
		kindMatches: capabilityKindMatches,
		kindFilterLabel: capabilityKindFilterLabel,
		kindFilterOptions: capabilityKindFilterOptions,
		toggleKindFilter: toggleCapabilityKindFilter,
		clearKindFilters: clearCapabilityKindFilters,
	} = useCapabilityKindFilters(t);
	const [capabilityStatus, setCapabilityStatus] = useState<
		"all" | "enabled" | "disabled"
	>("all");
	useEffect(() => {
		if (!reviewItemId) return;
		const reviewTab = resolveProfileDetailReviewTab(
			activeTab,
			true,
			hasExplicitTab,
		);
		if (reviewTab !== activeTab) {
			handleDetailTabChange(reviewTab);
		}
		if (reviewRefId) {
			setCapabilityQuery(reviewRefId);
		}
	}, [
		activeTab,
		handleDetailTabChange,
		hasExplicitTab,
		reviewItemId,
		reviewRefId,
	]);
	const { bulkModeDescription } = useBulkSelectionLabels();
	const serverBulk = useBulkSelection<string>();
	const capabilityBulk = useBulkSelection<string>();
	const [logFilter, setLogFilter] = useState("");
	const [logPageSize, setLogPageSize] = useState<number>(10);
	const [logPageCursors, setLogPageCursors] = useState<string[]>([]);
	const [logCurrentPageIndex, setLogCurrentPageIndex] = useState(0);
	const [isLogPaginationActionLoading, setIsLogPaginationActionLoading] =
		useState(false);
	const logCurrentCursor = logPageCursors[logCurrentPageIndex];

	const profileLogsQuery = useQuery({
		queryKey: [
			"profile-audit-logs",
			profileId,
			logCurrentCursor,
			logPageSize,
			showProfileLiveLogs,
		],
		queryFn: () =>
			auditApi.list({
				limit: logPageSize,
				cursor: logCurrentCursor,
				profile_id: profileId,
			}),
		enabled: Boolean(profileId && showProfileLiveLogs),
		refetchOnWindowFocus: false,
		retry: false,
	});

	useEffect(() => {
		setLogPageCursors([]);
		setLogCurrentPageIndex(0);
	}, [profileId, logPageSize]);

	const filteredProfileLogs = useMemo(() => {
		const logs = profileLogsQuery.data?.events ?? [];
		const term = logFilter.trim().toLowerCase();
		if (!term) return logs;
		return logs.filter((event) => {
			const haystacks = [
				event.action,
				event.category,
				event.status,
				event.target,
				event.route,
				event.error_message,
				event.detail,
				event.request_id,
				event.mcp_method,
			]
				.filter(Boolean)
				.map((value) => String(value).toLowerCase());
			return haystacks.some((value) => value.includes(term));
		});
	}, [profileLogsQuery.data?.events, logFilter]);

	const handleProfileLogsNextPage = () => {
		if (!profileLogsQuery.data?.next_cursor) return;
		const nextCursor = profileLogsQuery.data.next_cursor;
		setLogPageCursors((prev) => {
			const next = [...prev];
			next[logCurrentPageIndex + 1] = nextCursor;
			return next;
		});
		setLogCurrentPageIndex((prev) => prev + 1);
	};

	const handleProfileLogsPrevPage = () => {
		if (logCurrentPageIndex > 0) {
			setLogCurrentPageIndex((prev) => prev - 1);
		}
	};

	const handleProfileLogsFirstPage = () => {
		setLogCurrentPageIndex(0);
	};

	const handleProfileLogsLastPage = async () => {
		if (!profileLogsQuery.data?.next_cursor || !profileId) return;
		setIsLogPaginationActionLoading(true);
		try {
			let nextCursor: string | undefined = profileLogsQuery.data.next_cursor;
			let targetPageIndex = logCurrentPageIndex;
			const nextPageCursors = [...logPageCursors];
			while (nextCursor) {
				targetPageIndex += 1;
				nextPageCursors[targetPageIndex] = nextCursor;
				const page = await auditApi.list({
					limit: logPageSize,
					cursor: nextCursor,
					profile_id: profileId,
				});
				nextCursor = page.next_cursor ?? undefined;
			}
			setLogPageCursors(nextPageCursors);
			setLogCurrentPageIndex(targetPageIndex);
		} finally {
			setIsLogPaginationActionLoading(false);
		}
	};

	const bulkCapabilitiesM = useMutation({
		mutationFn: async ({ enable, ids }: { enable: boolean; ids: string[] }) => {
			const grouped = ids.reduce(
				(acc, key) => {
					const capability = splitCapabilityKey(key);
					if (capability.capability_type === "tools") {
						acc.tools.push(capability.capability_id);
					} else if (capability.capability_type === "resources") {
						acc.resources.push(capability.capability_id);
					} else if (capability.capability_type === "prompts") {
						acc.prompts.push(capability.capability_id);
					} else if (capability.capability_type === "templates") {
						acc.templates.push(capability.capability_id);
					}
					return acc;
				},
				{
					tools: [] as string[],
					resources: [] as string[],
					prompts: [] as string[],
					templates: [] as string[],
				},
			);
			const action = enable ? "enable" : "disable";
			const revisionSources: CapabilityRevisionSource[] = [];
			const authoringGenerations: Array<number | undefined> = [];
			if (grouped.tools.length) {
				const response = queryClient.getQueryData<ConfigSuitToolsResponse>([
					"configSuitTools",
					profileId,
				]);
				revisionSources.push({
					selectedIds: grouped.tools,
					items: response?.tools ?? [],
					sourceRevisionSet: response?.source_revision_set,
				});
				authoringGenerations.push(response?.authoring_generation);
			}
			if (grouped.resources.length) {
				const response = queryClient.getQueryData<ConfigSuitResourcesResponse>([
					"configSuitResources",
					profileId,
				]);
				revisionSources.push({
					selectedIds: grouped.resources,
					items: response?.resources ?? [],
					sourceRevisionSet: response?.source_revision_set,
				});
				authoringGenerations.push(response?.authoring_generation);
			}
			if (grouped.prompts.length) {
				const response = queryClient.getQueryData<ConfigSuitPromptsResponse>([
					"configSuitPrompts",
					profileId,
				]);
				revisionSources.push({
					selectedIds: grouped.prompts,
					items: response?.prompts ?? [],
					sourceRevisionSet: response?.source_revision_set,
				});
				authoringGenerations.push(response?.authoring_generation);
			}
			if (grouped.templates.length) {
				const response =
					queryClient.getQueryData<ConfigSuitResourceTemplatesResponse>([
						"configSuitResourceTemplates",
						profileId,
					]);
				revisionSources.push({
					selectedIds: grouped.templates,
					items: response?.templates ?? [],
					sourceRevisionSet: response?.source_revision_set,
				});
				authoringGenerations.push(response?.authoring_generation);
			}
			const revisionSet = requireSelectedCapabilityRevisionSet(revisionSources);
			const authoringGeneration =
				requireMatchingAuthoringGeneration(authoringGenerations);
			await configSuitsApi.bulkCapabilities(
				profileId!,
				[
					...grouped.tools,
					...grouped.resources,
					...grouped.prompts,
					...grouped.templates,
				],
				action,
				authoringGeneration,
				revisionSet,
			);
		},
		onSuccess: () => {
			capabilityBulk.clearSelection();
			capabilityBulk.exitBulkMode();
			refreshProfileCapabilitySurface();
			notifySuccess(
				t("profiles:detail.messages.capabilitiesUpdated", {
					defaultValue: "Capabilities updated",
				}),
				t("profiles:detail.messages.bulkOperationCompleted", {
					defaultValue: "Bulk operation completed",
				}),
			);
		},
		onError: (e) => {
			void handleCapabilityMutationError(e);
			notifyError(
				t("profiles:detail.messages.capabilitiesUpdateFailed", {
					defaultValue: "Capabilities update failed",
				}),
				t(profileSyncErrorTranslationKey(e)),
			);
		},
	});

	const bulkServersM = useMutation({
		mutationFn: ({ enable, ids }: { enable: boolean; ids: string[] }) =>
			configSuitsApi.bulkServers(
				profileId!,
				ids,
				enable ? "enable" : "disable",
				requireMatchingAuthoringGeneration([
					queryClient.getQueryData<ConfigSuitServersResponse>([
						"configSuitServers",
						profileId,
					])?.authoring_generation,
				]),
			),
		onSuccess: () => {
			serverBulk.clearSelection();
			serverBulk.exitBulkMode();
			refreshProfileCapabilitySurface();
			notifySuccess(
				t("profiles:detail.messages.serversUpdated", {
					defaultValue: "Servers updated",
				}),
				t("profiles:detail.messages.bulkOperationCompleted", {
					defaultValue: "Bulk operation completed",
				}),
			);
		},
		onError: (e) =>
			notifyError(
				t("profiles:detail.messages.serversUpdateFailed", {
					defaultValue: "Servers update failed",
				}),
				t(profileSyncErrorTranslationKey(e)),
			),
	});

	// Force cleanup when drawer closes to prevent overlay issues
	useEffect(() => {
		if (!isEditDialogOpen) {
			// 使用 requestAnimationFrame 确保在正确时机清理
			requestAnimationFrame(() => {
				setTimeout(() => {
					// 清理所有可能的遮罩层和覆盖元素
					const overlays = document.querySelectorAll(
						"[data-radix-popper-content-wrapper], [data-radix-dialog-overlay], [data-vaul-overlay], [data-vaul-drawer-wrapper], .fixed.inset-0, [data-vaul-drawer]",
					);
					overlays.forEach((overlay) => {
						const element = overlay as HTMLElement;
						if (
							element.getAttribute("data-state") === "closed" ||
							!element.closest('[data-state="open"]') ||
							element.style.pointerEvents === "none"
						) {
							element.remove();
						}
					});

					// 确保 body 样式被正确重置
					document.body.style.removeProperty("pointer-events");
					document.body.style.removeProperty("overflow");
					document.body.removeAttribute("data-scroll-locked");
					document.body.removeAttribute("aria-hidden");
					document.body.removeAttribute("data-vaul-drawer-wrapper");
				}, 50);
			});
		}
	}, [isEditDialogOpen]);

	// Do not early-return before hooks; guard queries with `enabled`

	// Fetch config suit details
	const {
		data: suit,
		isLoading: isLoadingSuit,
		refetch: refetchSuit,
		isRefetching: isRefetchingSuit,
	} = useQuery({
		queryKey: ["configSuit", profileId],
		queryFn: async () => {
			if (!profileId) return undefined;
			console.log("Fetching profile details for:", profileId);
			const result = await configSuitsApi.getSuit(profileId);
			console.log("Profile details response:", result);
			return result;
		},
		enabled: !!profileId,
		retry: 1,
	});
	const isWorkflowProfile = suit?.profile_mode === "workflow";
	const overviewSurfaceTab = isWorkflowProfile ? "workflow" : "capabilities";
	const profileModeLabel = t(
		`profiles:badges.${isWorkflowProfile ? "workflow" : "capability"}`,
	);
	const workflowUnsetLabel = "-";
	const {
		data: workflowSpecification,
		isLoading: isLoadingWorkflowSpecification,
		isError: isWorkflowSpecificationError,
	} = useQuery({
		queryKey: ["workflowSpecification", profileId],
		queryFn: () => configSuitsApi.getWorkflowSpecification(profileId!),
		enabled: Boolean(profileId) && isWorkflowProfile,
		retry: false,
	});

	useEffect(() => {
		if (!suit) {
			return;
		}

		const resolvedTab = resolveProfileDetailTab(
			activeTab,
			suit.profile_mode,
		);
		if (resolvedTab !== activeTab) {
			handleDetailTabChange(resolvedTab);
		}
	}, [activeTab, handleDetailTabChange, suit]);

	// Fetch servers in suit
	const {
		data: serversResponse,
		isLoading: isLoadingServers,
		refetch: refetchServers,
	} = useQuery({
		queryKey: ["configSuitServers", profileId],
		queryFn: async () => {
			if (!profileId) return undefined;
			console.log("Fetching servers for profile:", profileId);
			const result = await configSuitsApi.getServers(profileId);
			console.log("Profile servers response:", result);
			return result;
		},
		enabled: !!profileId,
		retry: 1,
	});
	// Fetch tools in suit
	const {
		data: toolsResponse,
		isLoading: isLoadingTools,
		refetch: refetchTools,
	} = useQuery({
		queryKey: ["configSuitTools", profileId],
		queryFn: () =>
			profileId
				? configSuitsApi.getTools(profileId)
				: Promise.resolve(undefined),
		enabled: !!profileId,
		retry: 1,
	});

	// Fetch resources in suit
	const {
		data: resourcesResponse,
		isLoading: isLoadingResources,
		refetch: refetchResources,
	} = useQuery({
		queryKey: ["configSuitResources", profileId],
		queryFn: () =>
			profileId
				? configSuitsApi.getResources(profileId)
				: Promise.resolve(undefined),
		enabled: !!profileId,
		retry: 1,
	});

	// Fetch prompts in suit
	const {
		data: promptsResponse,
		isLoading: isLoadingPrompts,
		refetch: refetchPrompts,
	} = useQuery({
		queryKey: ["configSuitPrompts", profileId],
		queryFn: () =>
			profileId
				? configSuitsApi.getPrompts(profileId)
				: Promise.resolve(undefined),
		enabled: !!profileId,
		retry: 1,
	});

	// Fetch resource templates in suit
	const {
		data: templatesResponse,
		isLoading: isLoadingTemplates,
		refetch: refetchTemplates,
	} = useQuery({
		queryKey: ["configSuitResourceTemplates", profileId],
		queryFn: () =>
			profileId
				? configSuitsApi.getResourceTemplates(profileId)
				: Promise.resolve(undefined),
		enabled: !!profileId,
		retry: 1,
	});

	const refreshProfileCapabilitySurface = useCallback((): void => {
		invalidateProfileCapabilityLedger();
		void refetchServers();
		void refetchTools();
		void refetchResources();
		void refetchPrompts();
		void refetchTemplates();
		void queryClient.invalidateQueries({
			queryKey: ["configSuitStats", profileId],
		});
	}, [
		invalidateProfileCapabilityLedger,
		profileId,
		queryClient,
		refetchPrompts,
		refetchResources,
		refetchServers,
		refetchTemplates,
		refetchTools,
	]);

	const handleCapabilityMutationError = useCallback(
		(error: unknown) => {
			if (!profileId) {
				return Promise.resolve(false);
			}
			return handleProfileCapabilityMutationError({
				error,
				profileId,
				invalidateQueries: (predicate) =>
					queryClient.invalidateQueries({
						predicate: (query) => predicate(query.queryKey, query.state.data),
					}),
			});
		},
		[profileId, queryClient],
	);

	// Activation/deactivation mutations
	const activateSuitMutation = useMutation({
		mutationFn: () => {
			if (!suit) throw new Error("Profile details are not loaded");
			return configSuitsApi.activateSuit(suit.id, suit.authoring_generation);
		},
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["configSuit", profileId] });
			queryClient.invalidateQueries({ queryKey: ["configSuits"] });
			notifySuccess(
				t("profiles:detail.messages.profileActivated", {
					defaultValue: "Profile activated",
				}),
				t("profiles:detail.messages.profileActivatedDescription", {
					defaultValue: "Profile has been successfully activated",
				}),
			);
		},
		onError: (error) => {
			notifyError(
				t("profiles:detail.messages.activationFailed", {
					defaultValue: "Activation failed",
				}),
				t(profileSyncErrorTranslationKey(error)),
			);
		},
	});

	const deactivateSuitMutation = useMutation({
		mutationFn: () => {
			if (!suit) throw new Error("Profile details are not loaded");
			return configSuitsApi.deactivateSuit(suit.id, suit.authoring_generation);
		},
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["configSuit", profileId] });
			queryClient.invalidateQueries({ queryKey: ["configSuits"] });
			notifySuccess(
				t("profiles:detail.messages.profileDeactivated", {
					defaultValue: "Profile deactivated",
				}),
				t("profiles:detail.messages.profileDeactivatedDescription", {
					defaultValue: "Profile has been successfully deactivated",
				}),
			);
		},
		onError: (error) => {
			notifyError(
				t("profiles:detail.messages.deactivationFailed", {
					defaultValue: "Deactivation failed",
				}),
				t(profileSyncErrorTranslationKey(error)),
			);
		},
	});

	// Delete profile mutation
	const deleteSuitMutation = useMutation({
		mutationFn: () => {
			if (!profileId)
				return Promise.reject(
					t("profiles:detail.errors.noSuitId", { defaultValue: "No suit ID" }),
				);
			if (!suit) throw new Error("Profile details are not loaded");
			return configSuitsApi.deleteSuit(profileId, suit.authoring_generation);
		},
		onSuccess: () => {
			// Invalidate queries to refresh the profiles list
			queryClient.invalidateQueries({ queryKey: ["configSuits"] });
			notifySuccess(
				t("profiles:detail.messages.profileDeleted", {
					defaultValue: "Profile deleted",
				}),
				t("profiles:detail.messages.profileDeletedDescription", {
					defaultValue: "Profile has been successfully deleted",
				}),
			);
			navigate("/profiles");
		},
		onError: (error) => {
			notifyError(
				t("profiles:detail.messages.deleteFailed", {
					defaultValue: "Delete failed",
				}),
				t(profileSyncErrorTranslationKey(error)),
			);
		},
	});

	// Server toggle mutations
	const serverToggleMutation = useMutation({
		mutationFn: ({
			serverId,
			enable,
		}: {
			serverId: string;
			enable: boolean;
		}) => {
			return enable
				? configSuitsApi.enableServer(
					profileId!,
					serverId,
					requireMatchingAuthoringGeneration([
						serversResponse?.authoring_generation,
					]),
				)
				: configSuitsApi.disableServer(
					profileId!,
					serverId,
					requireMatchingAuthoringGeneration([
						serversResponse?.authoring_generation,
					]),
				);
		},
		onSuccess: () => {
			refreshProfileCapabilitySurface();

			notifySuccess(
				t("profiles:detail.messages.serverUpdated", {
					defaultValue: "Server updated",
				}),
				"Server status has been updated",
			);
		},
		onError: (error) => {
			notifyError(
				t("profiles:detail.messages.serverUpdateFailed", {
					defaultValue: "Server update failed",
				}),
				t(profileSyncErrorTranslationKey(error)),
			);
		},
	});

	// Tool toggle mutations
	const toolToggleMutation = useMutation({
		mutationFn: ({ toolId, enable }: { toolId: string; enable: boolean }) => {
			return enable
				? configSuitsApi.enableTool(
					profileId!,
					toolId,
					requireMatchingAuthoringGeneration([
						toolsResponse?.authoring_generation,
					]),
					requireCapabilityRevisionSet(
						[toolId],
						toolsResponse?.tools,
						toolsResponse?.source_revision_set,
					),
				)
				: configSuitsApi.disableTool(
					profileId!,
					toolId,
					requireMatchingAuthoringGeneration([
						toolsResponse?.authoring_generation,
					]),
					requireCapabilityRevisionSet(
						[toolId],
						toolsResponse?.tools,
						toolsResponse?.source_revision_set,
					),
				);
		},
		onSuccess: () => {
			refreshProfileCapabilitySurface();
			notifySuccess(
				t("profiles:detail.messages.toolUpdated", {
					defaultValue: "Tool updated",
				}),
				"Tool status has been updated",
			);
		},
		onError: (error) => {
			void handleCapabilityMutationError(error);
			notifyError(
				t("profiles:detail.messages.toolUpdateFailed", {
					defaultValue: "Tool update failed",
				}),
				t(profileSyncErrorTranslationKey(error)),
			);
		},
	});

	// Resource toggle mutations
	const resourceToggleMutation = useMutation({
		mutationFn: ({
			resourceId,
			enable,
		}: {
			resourceId: string;
			enable: boolean;
		}) => {
			return enable
				? configSuitsApi.enableResource(
					profileId!,
					resourceId,
					requireMatchingAuthoringGeneration([
						resourcesResponse?.authoring_generation,
					]),
					requireCapabilityRevisionSet(
						[resourceId],
						resourcesResponse?.resources,
						resourcesResponse?.source_revision_set,
					),
				)
				: configSuitsApi.disableResource(
					profileId!,
					resourceId,
					requireMatchingAuthoringGeneration([
						resourcesResponse?.authoring_generation,
					]),
					requireCapabilityRevisionSet(
						[resourceId],
						resourcesResponse?.resources,
						resourcesResponse?.source_revision_set,
					),
				);
		},
		onSuccess: () => {
			refreshProfileCapabilitySurface();
			notifySuccess(
				t("profiles:detail.messages.resourceUpdated", {
					defaultValue: "Resource updated",
				}),
				"Resource status has been updated",
			);
		},
		onError: (error) => {
			void handleCapabilityMutationError(error);
			notifyError(
				t("profiles:detail.messages.resourceUpdateFailed", {
					defaultValue: "Resource update failed",
				}),
				t(profileSyncErrorTranslationKey(error)),
			);
		},
	});

	// Prompt toggle mutations
	const promptToggleMutation = useMutation({
		mutationFn: ({
			promptId,
			enable,
		}: {
			promptId: string;
			enable: boolean;
		}) => {
			return enable
				? configSuitsApi.enablePrompt(
					profileId!,
					promptId,
					requireMatchingAuthoringGeneration([
						promptsResponse?.authoring_generation,
					]),
					requireCapabilityRevisionSet(
						[promptId],
						promptsResponse?.prompts,
						promptsResponse?.source_revision_set,
					),
				)
				: configSuitsApi.disablePrompt(
					profileId!,
					promptId,
					requireMatchingAuthoringGeneration([
						promptsResponse?.authoring_generation,
					]),
					requireCapabilityRevisionSet(
						[promptId],
						promptsResponse?.prompts,
						promptsResponse?.source_revision_set,
					),
				);
		},
		onSuccess: () => {
			refreshProfileCapabilitySurface();
			notifySuccess(
				t("profiles:detail.messages.promptUpdated", {
					defaultValue: "Prompt updated",
				}),
				"Prompt status has been updated",
			);
		},
		onError: (error) => {
			void handleCapabilityMutationError(error);
			notifyError(
				t("profiles:detail.messages.promptUpdateFailed", {
					defaultValue: "Prompt update failed",
				}),
				t(profileSyncErrorTranslationKey(error)),
			);
		},
	});

	const suitRole = suit?.role ?? "user";
	const isDefaultAnchor = suitRole === DEFAULT_ANCHOR_ROLE;
	const isHostApp = suit?.suit_type === "host_app";
	const isCustomMode = mode === "custom";

	const handleSuitToggle = () => {
		if (isDefaultAnchor) {
			return;
		}
		if (suit?.is_active) {
			deactivateSuitMutation.mutate();
		} else {
			activateSuitMutation.mutate();
		}
	};

	const handleRefreshAll = () => {
		void refetchSuit();
		refreshProfileCapabilitySurface();
		void queryClient.refetchQueries({ queryKey: ["workflowGuide", profileId] });
	};
	const overviewActionButtonClass =
		"gap-2 rounded-none first:rounded-l-md last:rounded-r-md";

	const handleEditDrawerClose = (open: boolean) => {
		setIsEditDialogOpen(open);
	};

	const servers = useMemo(
		() => (serversResponse?.servers ?? []) as ConfigSuitServer[],
		[serversResponse],
	);
	const tools = useMemo(
		() => (toolsResponse?.tools ?? []) as ConfigSuitTool[],
		[toolsResponse],
	);
	const resources = useMemo(
		() => (resourcesResponse?.resources ?? []) as ConfigSuitResource[],
		[resourcesResponse],
	);
	const prompts = useMemo(
		() => (promptsResponse?.prompts ?? []) as ConfigSuitPrompt[],
		[promptsResponse],
	);
	const templates = useMemo(
		() => (templatesResponse?.templates ?? []) as ConfigSuitResourceTemplate[],
		[templatesResponse],
	);
	const workflowCapabilityCatalogQuery = useQuery({
		queryKey: ["workflowCapabilityCatalog", profileId],
		queryFn: async () => {
			const { servers } = await serversApi.getAll();
			const enabledServers = servers.filter(
				(server) => server.globally_enabled ?? server.enabled,
			);
			const batches = await Promise.all(
				enabledServers.map(async (server) => ({
					server: { id: server.id, name: server.name },
					lists: await serversApi.listAllCapabilities(server.id),
				})),
			);
			return buildWorkflowCapabilityOptions(batches);
		},
		enabled: Boolean(profileId) && isWorkflowProfile,
		staleTime: 30_000,
		retry: 1,
	});
	const workflowCapabilities = useMemo(
		() => workflowCapabilityCatalogQuery.data ?? [],
		[workflowCapabilityCatalogQuery.data],
	);

	const enabledServers = servers.filter((s: ConfigSuitServer) => s.enabled);
	const enabledTools = tools.filter((t: ConfigSuitTool) => t.enabled);
	const enabledResources = resources.filter(
		(r: ConfigSuitResource) => r.enabled,
	);
	const enabledPrompts = prompts.filter((p: ConfigSuitPrompt) => p.enabled);
	const workflowToolsValue = formatWorkflowToolBindingCount(
		workflowSpecification?.tool_binding_count,
		isLoadingWorkflowSpecification,
		isWorkflowSpecificationError,
	);
	// For profile counts, available = total in this profile (not global state)
	const availableServersInProfile = servers;
	const profileSurfaceMetrics: ProfileSurfaceMetric[] = [
		{
			id: "servers",
			label: t("profiles:detail.labels.servers", { defaultValue: "Servers" }),
			value: `${enabledServers.length}/${availableServersInProfile.length}`,
		},
		{
			id: "tools",
			label: capabilityKindLabel(t, "tools"),
			value: isWorkflowProfile
				? workflowToolsValue
				: `${enabledTools.length}/${tools.length}`,
		},
		{
			id: "resources",
			label: capabilityKindLabel(t, "resources"),
			value: `${enabledResources.length}/${resources.length}`,
		},
		{
			id: "prompts",
			label: capabilityKindLabel(t, "prompts"),
			value: `${enabledPrompts.length}/${prompts.length}`,
		},
	];

	const enabledByComponentId = useMemo(() => {
		const m = new Map<string, boolean>();
		for (const s of servers) {
			m.set(s.id, s.enabled);
		}
		for (const item of tools) {
			m.set(item.id, item.enabled);
		}
		for (const r of resources) {
			m.set(r.id, r.enabled);
		}
		for (const p of prompts) {
			m.set(p.id, p.enabled);
		}
		for (const tmpl of templates as ReadonlyArray<{
			id: string;
			enabled: boolean;
		}>) {
			m.set(tmpl.id, tmpl.enabled);
		}
		return m;
	}, [servers, tools, resources, prompts, templates]);

	const tokenChartSource = useProfileTokenChartSource(
		profileId,
		enabledByComponentId,
		{ enabled: Boolean(suit && !isWorkflowProfile) },
	);

	// Global servers for availability(connected) calculation
	const { data: globalServersResp } = useQuery({
		queryKey: ["all-servers-for-profile-overview"],
		queryFn: serversApi.getAll,
		staleTime: 30_000,
	});
	const globalServers = globalServersResp?.servers ?? [];

	// Filtered datasets
	const visibleServers = useMemo(
		() =>
			servers.filter((s: ConfigSuitServer) => {
				const queryPass =
					serverQuery.trim() === "" ||
					s.name.toLowerCase().includes(serverQuery.toLowerCase());
				const statusPass =
					serverStatus === "all" ||
					(serverStatus === "enabled" ? s.enabled : !s.enabled);
				return queryPass && statusPass;
			}),
		[serverQuery, serverStatus, servers],
	);

	const capabilityCountsByServerId = useMemo(() => {
		const createCounts = () => ({
			tools: 0,
			resources: 0,
			prompts: 0,
			templates: 0,
			enabled: 0,
			total: 0,
		});
		const counts = new Map<string, ReturnType<typeof createCounts>>();
		const ensure = (serverId: string) => {
			const current = counts.get(serverId);
			if (current) return current;
			const next = createCounts();
			counts.set(serverId, next);
			return next;
		};
		for (const tool of tools) {
			const entry = ensure(tool.server_id);
			entry.tools += 1;
			entry.total += 1;
			if (tool.enabled) entry.enabled += 1;
		}
		for (const resource of resources) {
			const entry = ensure(resource.server_id);
			entry.resources += 1;
			entry.total += 1;
			if (resource.enabled) entry.enabled += 1;
		}
		for (const prompt of prompts) {
			const entry = ensure(prompt.server_id);
			entry.prompts += 1;
			entry.total += 1;
			if (prompt.enabled) entry.enabled += 1;
		}
		for (const template of templates) {
			const entry = ensure(template.server_id);
			entry.templates += 1;
			entry.total += 1;
			if (template.enabled) entry.enabled += 1;
		}
		return counts;
	}, [prompts, resources, templates, tools]);

	const isAllCapabilityServersSelected =
		selectedCapabilityServerId === ALL_CAPABILITY_SERVERS_ID;

	const selectedCapabilityServer = useMemo(() => {
		if (isAllCapabilityServersSelected) return null;
		const candidates = visibleServers.length ? visibleServers : servers;
		return (
			candidates.find((server) => server.id === selectedCapabilityServerId) ??
			null
		);
	}, [
		isAllCapabilityServersSelected,
		selectedCapabilityServerId,
		servers,
		visibleServers,
	]);

	useEffect(() => {
		if (isAllCapabilityServersSelected) {
			return;
		}
		const stillVisible = visibleServers.some(
			(server) => server.id === selectedCapabilityServerId,
		);
		if (!stillVisible) {
			setSelectedCapabilityServerId(ALL_CAPABILITY_SERVERS_ID);
		}
	}, [
		isAllCapabilityServersSelected,
		selectedCapabilityServerId,
		visibleServers,
	]);

	useEffect(() => {
		if (!isAllCapabilityServersSelected) {
			if (capabilityServerFilters.length > 0) {
				setCapabilityServerFilters([]);
			}
			return;
		}
		const visibleServerIds = new Set(visibleServers.map((server) => server.id));
		const nextFilters = capabilityServerFilters.filter((serverId) =>
			visibleServerIds.has(serverId),
		);
		if (
			nextFilters.length !== capabilityServerFilters.length ||
			nextFilters.length === visibleServers.length
		) {
			if (nextFilters.length === 0 && capabilityServerFilters.length === 0) {
				return;
			}
			setCapabilityServerFilters(
				nextFilters.length === visibleServers.length ? [] : nextFilters,
			);
		}
	}, [capabilityServerFilters, isAllCapabilityServersSelected, visibleServers]);

	const selectedCapabilityServerIds = useMemo(() => {
		if (isAllCapabilityServersSelected) {
			if (capabilityServerFilters.length > 0) {
				return new Set(capabilityServerFilters);
			}
			return new Set(visibleServers.map((server) => server.id));
		}
		return new Set(
			selectedCapabilityServer ? [selectedCapabilityServer.id] : [],
		);
	}, [
		capabilityServerFilters,
		isAllCapabilityServersSelected,
		selectedCapabilityServer,
		visibleServers,
	]);
	const visibleServerCapabilityCounts = useMemo(
		() =>
			visibleServers.reduce(
				(acc, server) => {
					const counts = capabilityCountsByServerId.get(server.id);
					if (!counts) {
						return acc;
					}
					acc.enabled += counts.enabled;
					acc.total += counts.total;
					return acc;
				},
				{ enabled: 0, total: 0 },
			),
		[capabilityCountsByServerId, visibleServers],
	);

	const capabilityStatusFilter = useCallback(
		(item: { enabled: boolean }) =>
			capabilityStatus === "all" ||
			(capabilityStatus === "enabled" ? item.enabled : !item.enabled),
		[capabilityStatus],
	);

	const hasCapabilitySelection =
		isAllCapabilityServersSelected || selectedCapabilityServer !== null;

	const selectedServerTools = useMemo(
		() =>
			hasCapabilitySelection
				? tools.filter(
					(tool) =>
						selectedCapabilityServerIds.has(tool.server_id) &&
						capabilityStatusFilter(tool),
				)
				: [],
		[
			capabilityStatusFilter,
			hasCapabilitySelection,
			selectedCapabilityServerIds,
			tools,
		],
	);
	const selectedServerResources = useMemo(
		() =>
			hasCapabilitySelection
				? resources.filter(
					(resource) =>
						selectedCapabilityServerIds.has(resource.server_id) &&
						capabilityStatusFilter(resource),
				)
				: [],
		[
			capabilityStatusFilter,
			hasCapabilitySelection,
			resources,
			selectedCapabilityServerIds,
		],
	);
	const selectedServerPrompts = useMemo(
		() =>
			hasCapabilitySelection
				? prompts.filter(
					(prompt) =>
						selectedCapabilityServerIds.has(prompt.server_id) &&
						capabilityStatusFilter(prompt),
				)
				: [],
		[
			capabilityStatusFilter,
			hasCapabilitySelection,
			prompts,
			selectedCapabilityServerIds,
		],
	);
	const selectedServerTemplates = useMemo(
		() =>
			hasCapabilitySelection
				? templates.filter(
					(template) =>
						selectedCapabilityServerIds.has(template.server_id) &&
						capabilityStatusFilter(template),
				)
				: [],
		[
			capabilityStatusFilter,
			hasCapabilitySelection,
			selectedCapabilityServerIds,
			templates,
		],
	);

	const visibleCapabilityKeys = useMemo(
		() => [
			...(capabilityKindMatches("tools")
				? selectedServerTools
					.filter((tool) =>
						capabilityRecordMatchesSearch(
							tool as CapabilityRecord,
							capabilityQuery,
						),
					)
					.map((tool) => capabilityKey("tools", tool.id))
				: []),
			...(capabilityKindMatches("resources")
				? selectedServerResources
					.filter((resource) =>
						capabilityRecordMatchesSearch(
							resource as CapabilityRecord,
							capabilityQuery,
						),
					)
					.map((resource) => capabilityKey("resources", resource.id))
				: []),
			...(capabilityKindMatches("prompts")
				? selectedServerPrompts
					.filter((prompt) =>
						capabilityRecordMatchesSearch(
							prompt as CapabilityRecord,
							capabilityQuery,
						),
					)
					.map((prompt) => capabilityKey("prompts", prompt.id))
				: []),
			...(capabilityKindMatches("templates")
				? selectedServerTemplates
					.filter((template) =>
						capabilityRecordMatchesSearch(
							template as CapabilityRecord,
							capabilityQuery,
						),
					)
					.map((template) => capabilityKey("templates", template.id))
				: []),
		],
		[
			capabilityQuery,
			capabilityKindMatches,
			selectedServerPrompts,
			selectedServerResources,
			selectedServerTemplates,
			selectedServerTools,
		],
	);

	const capabilityServerFilterLabel = useMemo(() => {
		if (capabilityServerFilters.length === 0) {
			return t("profiles:detail.filters.server.all", {
				defaultValue: "All Servers",
			});
		}
		if (capabilityServerFilters.length === 1) {
			return (
				visibleServers.find(
					(server) => server.id === capabilityServerFilters[0],
				)?.name ??
				t("profiles:detail.placeholders.server", { defaultValue: "Server" })
			);
		}
		return t("profiles:detail.filters.server.selected", {
			count: capabilityServerFilters.length,
			defaultValue: "{{count}} Servers",
		});
	}, [capabilityServerFilters, i18n.language, t, visibleServers]);

	const capabilityStatusLabel = useMemo(() => {
		if (capabilityStatus === "enabled") {
			return t("servers:detail.filters.status.enabled", {
				defaultValue: "Enabled",
			});
		}
		if (capabilityStatus === "disabled") {
			return t("servers:detail.filters.status.disabled", {
				defaultValue: "Disabled",
			});
		}
		return t("servers:detail.filters.status.all", {
			defaultValue: "All",
		});
	}, [capabilityStatus, i18n.language, t]);

	const serverStatusLabel = useMemo(() => {
		if (serverStatus === "enabled") {
			return t("servers:detail.filters.status.enabled", {
				defaultValue: "Enabled",
			});
		}
		if (serverStatus === "disabled") {
			return t("servers:detail.filters.status.disabled", {
				defaultValue: "Disabled",
			});
		}
		return t("servers:detail.filters.status.all", {
			defaultValue: "All",
		});
	}, [i18n.language, serverStatus, t]);

	const toggleCapabilityServerFilter = useCallback(
		(serverId: string, checked: boolean) => {
			setCapabilityServerFilters((current) => {
				if (checked) {
					return current.includes(serverId) ? current : [...current, serverId];
				}
				return current.filter((value) => value !== serverId);
			});
		},
		[],
	);

	const serverBulkActions = useEnableDisableBulkActions(
		serverBulk,
		visibleServers.map((server) => server.id),
		bulkServersM,
	);
	const capabilityBulkActions = useEnableDisableBulkActions(
		capabilityBulk,
		visibleCapabilityKeys,
		bulkCapabilitiesM,
	);

	// Template toggle mutations
	const templateToggleMutation = useMutation({
		mutationFn: ({
			templateId,
			enable,
		}: {
			templateId: string;
			enable: boolean;
		}) =>
			enable
				? configSuitsApi.enableResourceTemplate(
					profileId!,
					templateId,
					requireMatchingAuthoringGeneration([
						templatesResponse?.authoring_generation,
					]),
					requireCapabilityRevisionSet(
						[templateId],
						templatesResponse?.templates,
						templatesResponse?.source_revision_set,
					),
				)
				: configSuitsApi.disableResourceTemplate(
					profileId!,
					templateId,
					requireMatchingAuthoringGeneration([
						templatesResponse?.authoring_generation,
					]),
					requireCapabilityRevisionSet(
						[templateId],
						templatesResponse?.templates,
						templatesResponse?.source_revision_set,
					),
				),
		onSuccess: () => {
			refreshProfileCapabilitySurface();
			notifySuccess(
				t("profiles:detail.messages.templateUpdated", {
					defaultValue: "Template updated",
				}),
				"Template status has been updated",
			);
		},
		onError: (error) => {
			void handleCapabilityMutationError(error);
			notifyError(
				t("profiles:detail.messages.templateUpdateFailed", {
					defaultValue: "Template update failed",
				}),
				t(profileSyncErrorTranslationKey(error)),
			);
		},
	});

	const loadProfileCapabilityDetails = useCallback(
		async (
			item: ProfileFlatCapabilityItem,
			kind: CapabilityKind,
		): Promise<CapabilityRecord | null> => {
			const serverId =
				typeof item.server_id === "string" ? item.server_id : undefined;
			if (!serverId) {
				return null;
			}

			const key = profileCapabilityDetailKey(item, kind);
			if (!key) {
				return null;
			}

			const detail = await serversApi.getCapabilityDetail(serverId, kind, key);
			return (detail.item ?? null) as CapabilityRecord | null;
		},
		[],
	);

	const renderProfileFlatCapabilityList = useCallback(
		(items: CapabilityPreviewFlatItem[]) => {
			const flatItems: ProfileFlatCapabilityItem[] = items.map(
				({ kind, item }) => ({
					...item,
					__profileCapabilityKind: kind,
				}),
			);

			return (
				<CapabilityList<ProfileFlatCapabilityItem>
					asCard={false}
					kind="tools"
					getKind={(item) => item.__profileCapabilityKind}
					context="profile"
					leadingIcon="kind"
					items={flatItems}
					scrollContainedBody
					enableToggle
					getId={(item) => capabilityKey(item.__profileCapabilityKind, item.id)}
					getEnabled={(item) => !!item.enabled}
					getItemClassName={(item) =>
						reviewRefId && item.id === reviewRefId
							? "border border-dashed border-amber-500 bg-amber-50/60 dark:border-amber-600 dark:bg-amber-950/30"
							: undefined
					}
					onToggle={(_, next, item) => {
						if (item.__profileCapabilityKind === "tools") {
							toolToggleMutation.mutate({
								toolId: item.id,
								enable: next,
							});
							return;
						}
						if (item.__profileCapabilityKind === "resources") {
							resourceToggleMutation.mutate({
								resourceId: item.id,
								enable: next,
							});
							return;
						}
						if (item.__profileCapabilityKind === "prompts") {
							promptToggleMutation.mutate({
								promptId: item.id,
								enable: next,
							});
							return;
						}
						templateToggleMutation.mutate({
							templateId: item.id,
							enable: next,
						});
					}}
					emptyText={t("servers:detail.capabilityList.emptyAll", {
						defaultValue: "No capabilities from this server",
					})}
					selectable={capabilityBulk.isBulkMode}
					selectedIds={capabilityBulk.selectedIds}
					onSelectToggle={(id) => capabilityBulk.toggleItem(id)}
					loadDetails={loadProfileCapabilityDetails}
					detailsCacheScope={capabilityDetailsCacheToken(flatItems)}
				/>
			);
		},
		[
			capabilityBulk,
			loadProfileCapabilityDetails,
			promptToggleMutation,
			resourceToggleMutation,
			t,
			templateToggleMutation,
			toolToggleMutation,
			reviewRefId,
		],
	);

	return (
		<div className="flex h-full min-h-0 flex-col gap-4 overflow-hidden">
			<div className="flex shrink-0 items-center justify-between">
				<div className="flex items-center">
					{suit && (
						<div className="flex items-center gap-3">
							<div className="flex flex-col">
								<div className="flex items-center gap-3">
									<h2 className="text-3xl font-bold tracking-tight">
										{toTitleCase(suit.name)}
									</h2>
									<Badge variant={suit.is_active ? "default" : "secondary"}>
										{profileModeLabel}
									</Badge>
									{suit.is_active && (
										<span className="flex items-center rounded-full bg-emerald-50 px-2 py-1 text-xs font-medium text-emerald-700 dark:bg-emerald-950/50 dark:text-emerald-400">
											<Check className="mr-1 h-3 w-3" />
											{t("profiles:detail.status.active", {
												defaultValue: "Active",
											})}
										</span>
									)}
									{suitRole === DEFAULT_ANCHOR_ROLE ? (
										<Badge variant="outline">
											{t("profiles:badges.defaultAnchor", {
												defaultValue: "Default Anchor",
											})}
										</Badge>
									) : suit.is_default ? (
										<Badge variant="outline">
											{t("profiles:badges.inDefault", {
												defaultValue: "In Default",
											})}
										</Badge>
									) : null}
								</div>
							</div>
						</div>
					)}
				</div>
				<div className="flex flex-shrink-0 items-center gap-3">
					{profileId && suit && !isWorkflowProfile ? (
						<ProfileTokenUsageChart
							ledgerItems={tokenChartSource.ledgerItems}
							fallbackEstimate={tokenChartSource.fallbackEstimate}
							isLoading={tokenChartSource.isLoading}
							isError={tokenChartSource.isError}
							enabledByComponentId={enabledByComponentId}
							estimateMethod={profileTokenEstimateMethod}
							profileServerCount={isLoadingServers ? undefined : servers.length}
						/>
					) : null}
				</div>
			</div>

			{!profileId ? (
				<Card>
					<CardContent className="p-4">
						<p className="text-center text-slate-500">
							{t("profiles:detail.labels.profileId", {
								defaultValue: "Profile ID not provided",
							})}
						</p>
					</CardContent>
				</Card>
			) : isLoadingSuit ? (
				<Card>
					<CardContent className="p-4">
						<div className="h-32 animate-pulse rounded bg-slate-200 dark:bg-slate-800"></div>
					</CardContent>
				</Card>
			) : suit ? (
				<Tabs
					value={activeTab}
					onValueChange={handleDetailTabChange}
					className="flex min-h-0 flex-1 flex-col gap-4"
				>
					<div className="flex shrink-0 items-center justify-between">
						<TabsList className="flex items-center gap-2">
							<TabsTrigger value="overview">
								{t("profiles:detail.tabs.overview", {
									defaultValue: "Overview",
								})}
							</TabsTrigger>
							{isWorkflowProfile && (
								<>
									<TabsTrigger value="workflow">
										{t("profiles:detail.tabs.workflow")}
									</TabsTrigger>
								</>
							)}
							{!isWorkflowProfile && (
								<TabsTrigger value="capabilities">
									{t("profiles:detail.tabs.capabilities", {
										defaultValue: "Capabilities",
									})}
								</TabsTrigger>
							)}
						</TabsList>
						<ButtonGroup className="ml-auto flex-shrink-0 flex-nowrap self-start">
							<Button
								variant="outline"
								size="sm"
								onClick={handleRefreshAll}
								disabled={isRefetchingSuit}
								className={overviewActionButtonClass}
							>
								<RefreshCw
									className={`h-4 w-4 ${isRefetchingSuit ? "animate-spin" : ""}`}
								/>
								{t("profiles:detail.buttons.refresh", {
									defaultValue: "Refresh",
								})}
							</Button>
							<Button
								variant="outline"
								size="sm"
								onClick={() => setIsEditDialogOpen(true)}
								className={overviewActionButtonClass}
							>
								<Edit3 className="h-4 w-4" />
								{t("profiles:detail.buttons.edit", { defaultValue: "Edit" })}
							</Button>
						</ButtonGroup>
					</div>

					<TabsContent value="overview" className={DETAIL_TAB_CONTENT_CLASS}>
						<div className={DETAIL_OVERVIEW_STACK_CLASS}>
							<Card className={DETAIL_OVERVIEW_PINNED_SECTION_CLASS}>
								<CardContent className="relative p-4">
									{!isHostApp && !isCustomMode && (
										<div className="absolute right-4 top-4">
											<Button
												variant="destructive"
												size="sm"
												onClick={() => setIsDeleteDialogOpen(true)}
												disabled={!!suit?.is_default}
												className="gap-2"
											>
												<Trash2 className="h-4 w-4" />
												{t("profiles:detail.buttons.delete", {
													defaultValue: "Delete",
												})}
											</Button>
										</div>
									)}
									<div className="grid gap-4 xl:grid-cols-[minmax(0,1fr)_auto] xl:items-start">
										<div className="flex min-w-0 items-start gap-4">
											<Avatar className="text-sm">
												<AvatarFallback>
													{suit.name.slice(0, 1).toUpperCase()}
												</AvatarFallback>
											</Avatar>
											<div className="grid min-w-0 flex-1 grid-cols-[auto_minmax(0,1fr)] gap-x-5 gap-y-2 text-sm">
												<span className="hidden">
													{t("profiles:detail.labels.status", {
														defaultValue: "Status",
													})}
												</span>
												<Badge
													variant="secondary"
													className={`hidden justify-self-start border px-2.5 py-0.5 leading-none min-h-[1.5rem] ${suit.is_active
															? "border-emerald-200 bg-emerald-100 text-emerald-700 hover:bg-emerald-100 dark:border-emerald-400/50 dark:bg-emerald-500/20 dark:text-emerald-200"
															: "border-slate-200 bg-slate-50 text-slate-600 hover:bg-slate-100 dark:border-slate-700 dark:bg-slate-800/80 dark:text-slate-300"
														}`}
												>
													{suit.is_active
														? t("profiles:detail.status.active", {
															defaultValue: "Active",
														})
														: t("profiles:detail.status.inactive", {
															defaultValue: "Inactive",
														})}
												</Badge>

												<span className="text-xs uppercase text-slate-500">
													{t("profiles:detail.labels.type", {
														defaultValue: "Type",
													})}
												</span>
												<span className="text-sm leading-tight">
													{profileModeLabel}
												</span>

												<span className="text-xs uppercase text-slate-500">
													{t("profiles:detail.labels.priority", {
														defaultValue: "Priority",
													})}
												</span>
												<span className="font-mono text-sm leading-tight">
													{suit.priority}
												</span>

												{isWorkflowProfile && (
													<>
														<span className="text-xs uppercase text-slate-500">
															{t(
																"profiles:detail.workflow.brief.validationNotes",
															)}
														</span>
														<span className="min-w-0 truncate text-sm leading-tight text-slate-400 dark:text-slate-500">
															{workflowSpecification?.validation_notes ?? workflowUnsetLabel}
														</span>

														<span className="text-xs uppercase text-slate-500">
															{t("profiles:detail.workflow.brief.avoidRules")}
														</span>
														<span className="min-w-0 truncate text-sm leading-tight text-slate-400 dark:text-slate-500">
															{workflowSpecification?.avoid_rules ?? workflowUnsetLabel}
														</span>

														<span className="text-xs uppercase text-slate-500">
															{t("profiles:detail.labels.description", {
																defaultValue: "Description",
															})}
														</span>
														<span
															className="min-w-0 line-clamp-3 text-sm leading-5"
															title={suit.description || workflowUnsetLabel}
														>
															{suit.description || workflowUnsetLabel}
														</span>
													</>
												)}
											</div>
										</div>
										{suitRole === "user" &&
											!isHostApp &&
											!isCustomMode &&
											!isWorkflowProfile && (
												<ButtonGroup className="ml-auto flex-shrink-0 flex-nowrap self-start">
													<Button
														variant="outline"
														size="sm"
														onClick={handleSuitToggle}
														disabled={
															isDefaultAnchor ||
															activateSuitMutation.isPending ||
															deactivateSuitMutation.isPending
														}
														className={overviewActionButtonClass}
													>
														{suit?.is_active ? (
															<Square className="h-4 w-4" />
														) : (
															<Play className="h-4 w-4" />
														)}
														{suit?.is_active
															? t("profiles:detail.buttons.disable", {
																defaultValue: "Disable",
															})
															: t("profiles:detail.buttons.enable", {
																defaultValue: "Enable",
															})}
													</Button>
												</ButtonGroup>
											)}
									</div>
								</CardContent>
							</Card>

							{!isWorkflowProfile && (
								<ProfileSurfaceMetrics
									metrics={profileSurfaceMetrics}
									description={t("profiles:detail.overview.enabledAvailable", {
										defaultValue: "enabled / available",
									})}
									onSelect={() => handleDetailTabChange(overviewSurfaceTab)}
									className={DETAIL_OVERVIEW_PINNED_SECTION_CLASS}
								/>
							)}
							{showProfileLiveLogs ? (
								<AuditLogsPanel
									fillHeight
									title={t("profiles:detail.logs.title", {
										defaultValue: "Logs",
									})}
									description={t("profiles:detail.logs.description", {
										defaultValue:
											"Runtime and activity logs related to this profile.",
									})}
									searchPlaceholder={t(
										"profiles:detail.logs.searchPlaceholder",
										{
											defaultValue: "Search logs...",
										},
									)}
									refreshLabel={t("profiles:detail.logs.refresh", {
										defaultValue: "Refresh Logs",
									})}
									loadingLabel={t("profiles:detail.logs.loading", {
										defaultValue: "Loading logs...",
									})}
									emptyLabel={t("profiles:detail.logs.empty", {
										defaultValue:
											"No log entries recorded for this profile yet.",
									})}
									headers={{
										timestamp: t("profiles:detail.logs.headers.timestamp", {
											defaultValue: "Timestamp",
										}),
										action: t("profiles:detail.logs.headers.action", {
											defaultValue: "Action",
										}),
										category: t("profiles:detail.logs.headers.category", {
											defaultValue: "Category",
										}),
										status: t("profiles:detail.logs.headers.status", {
											defaultValue: "Status",
										}),
										target: t("profiles:detail.logs.headers.target", {
											defaultValue: "Target",
										}),
									}}
									searchValue={logFilter}
									onSearchChange={setLogFilter}
									onRefresh={() => void profileLogsQuery.refetch()}
									rows={filteredProfileLogs}
									isLoading={profileLogsQuery.isLoading}
									isFetching={profileLogsQuery.isFetching}
									isPaginationActionLoading={isLogPaginationActionLoading}
									currentPage={logCurrentPageIndex + 1}
									hasPreviousPage={logCurrentPageIndex > 0}
									hasNextPage={Boolean(profileLogsQuery.data?.next_cursor)}
									itemsPerPage={logPageSize}
									onItemsPerPageChange={setLogPageSize}
									onPreviousPage={handleProfileLogsPrevPage}
									onFirstPage={handleProfileLogsFirstPage}
									onNextPage={handleProfileLogsNextPage}
									onLastPage={() => void handleProfileLogsLastPage()}
									expandLabel={t("profiles:detail.logs.expand", {
										defaultValue: "Expand Logs",
									})}
									collapseLabel={t("profiles:detail.logs.collapse", {
										defaultValue: "Collapse Logs",
									})}
								/>
							) : null}
						</div>
					</TabsContent>

					{isWorkflowProfile && profileId && (
						<TabsContent value="workflow" className={DETAIL_TAB_CONTENT_CLASS}>
							<Card className={CAPABILITY_SCROLL_CARD_CLASS}>
								<CardContent className="flex min-h-0 flex-1 flex-col overflow-hidden p-0">
									<ProfileWorkflowGuide
										profileId={profileId}
										capabilities={workflowCapabilities}
										capabilitiesLoading={
											workflowCapabilityCatalogQuery.isLoading
										}
										onDirtyChange={setWorkflowGuideDirty}
									/>
								</CardContent>
							</Card>
						</TabsContent>
					)}

					{!isWorkflowProfile && (
						<TabsContent
							value="capabilities"
							className={DETAIL_TAB_CONTENT_CLASS}
						>
							<Card className={CAPABILITY_SCROLL_CARD_CLASS}>
								<CardContent className="flex min-h-0 flex-1 flex-col overflow-hidden p-0">
									<ResizableSplitPane
										dividerAriaLabel={t(
											"profiles:detail.labels.resizeCapabilityColumns",
											{ defaultValue: "Resize capability columns" },
										)}
									>
										<div className="flex min-h-0 flex-col">
											<div className="shrink-0 p-3">
												<BulkSelectionHeader
													className="mb-3"
													title={t("profiles:detail.labels.servers", {
														defaultValue: "Servers",
													})}
													description={
														serverBulk.isBulkMode
															? bulkModeDescription(serverBulk.selectedCount)
															: t(
																"profiles:detail.descriptions.capabilityServers",
																{
																	defaultValue:
																		"Select a server to manage its profile capabilities.",
																},
															)
													}
													isBulkMode={serverBulk.isBulkMode}
													onToggleBulkMode={serverBulk.toggleMode}
													actions={serverBulkActions}
												/>
												<div className="grid min-w-0 grid-cols-[minmax(0,3fr)_minmax(2.25rem,1fr)] gap-2">
													<Input
														placeholder={t(
															"profiles:detail.placeholders.searchServers",
															{ defaultValue: "Search servers..." },
														)}
														value={serverQuery}
														onChange={(e) => setServerQuery(e.target.value)}
														className="h-9 min-w-0"
													/>
													<Select
														value={serverStatus}
														onValueChange={(v) =>
															setServerStatus(
																v as "all" | "enabled" | "disabled",
															)
														}
													>
														<SelectTrigger
															title={serverStatusLabel}
															className={compactSelectTriggerClass}
														>
															<SelectValue
																placeholder={t(
																	"profiles:detail.placeholders.status",
																	{ defaultValue: "Status" },
																)}
															/>
														</SelectTrigger>
														<SelectContent>
															<SelectItem value="all">
																{t("servers:detail.filters.status.all", {
																	defaultValue: "All",
																})}
															</SelectItem>
															<SelectItem value="enabled">
																{t("servers:detail.filters.status.enabled", {
																	defaultValue: "Enabled",
																})}
															</SelectItem>
															<SelectItem value="disabled">
																{t("servers:detail.filters.status.disabled", {
																	defaultValue: "Disabled",
																})}
															</SelectItem>
														</SelectContent>
													</Select>
												</div>
											</div>
											<CardListScrollBody className={PROFILE_EDITOR_SIDEBAR_SCROLL_CLASS}>
												{isLoadingServers ? (
													<div className="space-y-3">
														{["s1", "s2", "s3"].map((id) => (
															<div
																key={`capabilities-server-skel-${id}`}
																className="h-16 animate-pulse rounded-md bg-slate-200 dark:bg-slate-800"
															/>
														))}
													</div>
												) : visibleServers.length > 0 ? (
													<CapsuleStripeList className={PROFILE_EDITOR_SIDEBAR_LIST_CLASS}>
														<CapsuleStripeListItem
															key={ALL_CAPABILITY_SERVERS_ID}
															interactive
															className={`${PROFILE_EDITOR_SIDEBAR_ITEM_CLASS} ${isAllCapabilityServersSelected
																	? "bg-primary/10"
																	: ""
																}`}
															onClick={() =>
																setSelectedCapabilityServerId(
																	ALL_CAPABILITY_SERVERS_ID,
																)
															}
															onKeyDown={(event) => {
																if (
																	event.key === "Enter" ||
																	event.key === " "
																) {
																	event.preventDefault();
																	setSelectedCapabilityServerId(
																		ALL_CAPABILITY_SERVERS_ID,
																	);
																}
															}}
														>
															<CapsuleStripeRowBody
																lead={
																	<div className="flex h-9 w-9 items-center justify-center rounded-md border border-slate-200 bg-white text-xs font-semibold uppercase text-slate-600 dark:border-slate-700 dark:bg-slate-900/40 dark:text-slate-300">
																		{t(
																			"profiles:detail.labels.allServersShort",
																			{
																				defaultValue: "All",
																			},
																		)}
																	</div>
																}
																trailing={
																	<Badge variant="outline">
																		{visibleServers.length}
																	</Badge>
																}
															>
																<div className="min-w-0">
																	<div
																		className="truncate font-medium text-slate-900 dark:text-slate-100"
																		title={t(
																			"profiles:detail.labels.allServers",
																			{
																				defaultValue: "All servers",
																			},
																		)}
																	>
																		{t("profiles:detail.labels.allServers", {
																			defaultValue: "All servers",
																		})}
																	</div>
																	<div
																		className="mt-1 truncate text-xs text-slate-500"
																		title={`${visibleServerCapabilityCounts.enabled}/${visibleServerCapabilityCounts.total} ${t(
																			"profiles:detail.labels.enabledCapabilities",
																			{
																				defaultValue: "enabled capabilities",
																			},
																		)}`}
																	>
																		{visibleServerCapabilityCounts.enabled}/
																		{visibleServerCapabilityCounts.total}{" "}
																		{t(
																			"profiles:detail.labels.enabledCapabilities",
																			{
																				defaultValue: "enabled capabilities",
																			},
																		)}
																	</div>
																</div>
															</CapsuleStripeRowBody>
														</CapsuleStripeListItem>
														{visibleServers.map((server) => {
															const global = (
																globalServers as ProfileGlobalServerSummary[]
															).find((gs) => gs.name === server.name);
															const globalIcon = global?.icons?.[0]?.src;
															const iconAlt =
																global?.name || server.name || server.id;
															const avatarFallback = (
																server.name ||
																server.id ||
																"S"
															)
																.slice(0, 1)
																.toUpperCase();
															const counts = capabilityCountsByServerId.get(
																server.id,
															) ?? {
																enabled: 0,
																prompts: 0,
																resources: 0,
																templates: 0,
																tools: 0,
																total: 0,
															};
															const isSelected =
																selectedCapabilityServer?.id === server.id;
															const bulkSelected =
																serverBulk.isBulkMode &&
																serverBulk.selectedIdSet.has(server.id);
															let serverItemStateClass = "";
															if (isSelected) {
																serverItemStateClass = "bg-primary/10";
															} else if (bulkSelected) {
																serverItemStateClass = "bg-accent/40";
															}
															const serverLeadClassName = serverBulk.isBulkMode
																? "flex items-center gap-3"
																: "flex items-center gap-0";
															const serverAvatarClassName = isSelected
																? "border-primary bg-primary text-primary-foreground"
																: "border-slate-200 bg-white dark:border-slate-700 dark:bg-slate-900/40";
															const serverTitleClassName = isSelected
																? "truncate font-medium text-primary"
																: "truncate font-medium text-slate-900 dark:text-slate-100";

															return (
																<CapsuleStripeListItem
																	key={server.id}
																	interactive
																	className={`${PROFILE_EDITOR_SIDEBAR_ITEM_CLASS} ${serverItemStateClass}`}
																	onClick={() =>
																		setSelectedCapabilityServerId(server.id)
																	}
																	onKeyDown={(event) => {
																		if (
																			event.key === "Enter" ||
																			event.key === " "
																		) {
																			event.preventDefault();
																			setSelectedCapabilityServerId(server.id);
																		}
																	}}
																>
																	<CapsuleStripeRowBody
																		lead={
																			<div className={serverLeadClassName}>
																				<BulkSelectionCheckbox
																					visible={serverBulk.isBulkMode}
																					checked={bulkSelected}
																					onToggle={() =>
																						serverBulk.toggleItem(server.id)
																					}
																					ariaLabel={t(
																						"profiles:detail.bulk.selectItem",
																						{
																							name: server.name,
																							defaultValue: "Select {{name}}",
																						},
																					)}
																				/>
																				<CachedAvatar
																					src={globalIcon}
																					alt={
																						iconAlt
																							? `${iconAlt} icon`
																							: undefined
																					}
																					fallback={avatarFallback}
																					size="sm"
																					shape="rounded"
																					className={`border ${serverAvatarClassName}`}
																					fallbackClassName={
																						isSelected ? "bg-primary text-primary-foreground" : undefined
																					}
																					imageClassName={
																						isSelected ? "m-1 h-6 w-6 rounded-md" : undefined
																					}
																				/>
																			</div>
																		}
																		trailing={
																			<div className="flex w-[4.25rem] shrink-0 items-center justify-end gap-1">
																				{!serverBulk.isBulkMode ? (
																					<button
																						type="button"
																						className="flex h-7 w-7 shrink-0 items-center justify-center border-0 bg-transparent p-0 text-muted-foreground opacity-0 shadow-none transition-[color,opacity] hover:bg-transparent hover:text-foreground focus-visible:opacity-100 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent/60 group-hover:opacity-100"
																						onClick={(event) => {
																							event.stopPropagation();
																							openServerDetail(server.id);
																						}}
																						aria-label={t(
																							"profiles:detail.labels.browseServer",
																							{ defaultValue: "Browse server" },
																						)}
																					>
																						<Eye className="h-4 w-4" />
																					</button>
																				) : null}
																				<Switch
																					checked={server.enabled}
																					onClick={(e) => e.stopPropagation()}
																					onCheckedChange={(enabled) =>
																						serverToggleMutation.mutate({
																							serverId: server.id,
																							enable: enabled,
																						})
																					}
																					disabled={
																						serverToggleMutation.isPending
																					}
																				/>
																			</div>
																		}
																	>
																		<div className="min-w-0">
																			<div
																				className={serverTitleClassName}
																				title={server.name}
																			>
																				{server.name}
																			</div>
																			<div
																				className="mt-1 truncate text-xs text-slate-500"
																				title={`${counts.enabled}/${counts.total} ${t(
																					"profiles:detail.labels.enabledCapabilities",
																					{
																						defaultValue:
																							"enabled capabilities",
																					},
																				)}`}
																			>
																				{counts.enabled}/{counts.total}{" "}
																				{t(
																					"profiles:detail.labels.enabledCapabilities",
																					{
																						defaultValue:
																							"enabled capabilities",
																					},
																				)}
																			</div>
																		</div>
																	</CapsuleStripeRowBody>
																</CapsuleStripeListItem>
															);
														})}
													</CapsuleStripeList>
												) : (
													<div className="flex min-h-full items-center justify-center px-4 py-8 text-center text-sm text-muted-foreground">
														{t("profiles:detail.emptyStates.noServers", {
															defaultValue: "No servers found in this profile",
														})}
													</div>
												)}
											</CardListScrollBody>
										</div>

										<CapabilityManagementPanel
											title={
												isAllCapabilityServersSelected
													? t("profiles:detail.labels.allServers", {
														defaultValue: "All servers",
													})
													: selectedCapabilityServer
														? selectedCapabilityServer.name
														: t("servers:detail.overview.labels.capabilities", {
															defaultValue: "Capabilities",
														})
											}
											description={
												isAllCapabilityServersSelected
													? t(
														"profiles:detail.descriptions.allCapabilityGroups",
														{
															defaultValue:
																"Manage tools, resources, prompts, and resource templates across the visible servers.",
														},
													)
													: selectedCapabilityServer
														? t(
															"profiles:detail.descriptions.capabilityGroups",
															{
																defaultValue:
																	"Manage tools, resources, prompts, and resource templates for the selected server.",
															},
														)
														: t("profiles:detail.emptyStates.selectServer", {
															defaultValue:
																"Select a server to inspect its capabilities.",
														})
											}
											isBulkMode={capabilityBulk.isBulkMode}
											onToggleBulkMode={capabilityBulk.toggleMode}
											bulkActions={capabilityBulkActions}
											searchValue={capabilityQuery}
											onSearchChange={setCapabilityQuery}
											searchPlaceholder={t(
												"servers:wizard.preview.filterCapabilities",
												{ defaultValue: "Filter capabilities..." },
											)}
											serverFilter={
												isAllCapabilityServersSelected
													? {
														label: capabilityServerFilterLabel,
														allLabel: t(
															"profiles:detail.filters.server.all",
															{
																defaultValue: "All Servers",
															},
														),
														options: visibleServers.map((server) => ({
															value: server.id,
															label: server.name,
															title: server.name,
														})),
														selectedValues: capabilityServerFilters,
														onClear: () => setCapabilityServerFilters([]),
														onToggle: toggleCapabilityServerFilter,
													}
													: undefined
											}
											kindFilter={{
												label: capabilityKindFilterLabel,
												allLabel: t("servers:detail.filters.kind.all", {
													defaultValue: "All Types",
												}),
												options: capabilityKindFilterOptions,
												selectedValues: capabilityKindFilters,
												onClear: clearCapabilityKindFilters,
												onToggle: (value, checked) =>
													toggleCapabilityKindFilter(
														value as CapabilityKind,
														checked,
													),
											}}
											statusFilter={{
												label: capabilityStatusLabel,
												value: capabilityStatus,
												placeholder: t("profiles:detail.placeholders.status", {
													defaultValue: "Status",
												}),
												options: [
													{
														value: "all",
														label: t("servers:detail.filters.status.all", {
															defaultValue: "All",
														}),
													},
													{
														value: "enabled",
														label: t("servers:detail.filters.status.enabled", {
															defaultValue: "Enabled",
														}),
													},
													{
														value: "disabled",
														label: t("servers:detail.filters.status.disabled", {
															defaultValue: "Disabled",
														}),
													},
												],
												onValueChange: (value) =>
													setCapabilityStatus(
														value as "all" | "enabled" | "disabled",
													),
											}}
											hasSource={hasCapabilitySelection}
											isLoading={
												isLoadingTools ||
												isLoadingResources ||
												isLoadingPrompts ||
												isLoadingTemplates
											}
											tools={selectedServerTools as CapabilityRecord[]}
											resources={selectedServerResources as CapabilityRecord[]}
											prompts={selectedServerPrompts as CapabilityRecord[]}
											templates={selectedServerTemplates as CapabilityRecord[]}
											kindFilters={capabilityKindFilters}
											selectHintText={t(
												"profiles:detail.emptyStates.selectServer",
												{
													defaultValue:
														"Select a server to inspect its capabilities.",
												},
											)}
											emptyText={t("servers:detail.capabilityList.emptyAll", {
												defaultValue: "No capabilities from this server",
											})}
											emptySearchText={t(
												"servers:detail.capabilityList.emptyAll",
												{
													defaultValue: "No capabilities from this server",
												},
											)}
											renderFlatList={renderProfileFlatCapabilityList}
										/>
									</ResizableSplitPane>
								</CardContent>
							</Card>
						</TabsContent>
					)}
				</Tabs>
			) : (
				<Card>
					<CardContent className="p-4">
						<p className="text-center text-slate-500">
							{t("profiles:detail.emptyStates.profileNotFound", {
								defaultValue: "Profile not found",
							})}
						</p>
					</CardContent>
				</Card>
			)}

			{/* Edit Suit Drawer */}
			<ProfileFormDrawer
				open={isEditDialogOpen}
				onOpenChange={handleEditDrawerClose}
				mode="edit"
				suit={suit}
				restrictProfileType={isHostApp ? "host_app" : undefined}
				onSuccess={() => {
					handleEditDrawerClose(false);
					handleRefreshAll();
				}}
			/>
			<SurfaceReviewDialog
				reviewItemId={reviewItemId}
				open={!!reviewItemId}
				onOpenChange={(open) => {
					if (open) return;
					const next = new URLSearchParams(searchParams);
					next.delete("review_item");
					next.delete("ref_id");
					setSearchParams(next, { replace: true });
				}}
			/>

			{/* Delete Confirmation Dialog */}
			<AlertDialog
				open={isDeleteDialogOpen}
				onOpenChange={setIsDeleteDialogOpen}
			>
				<AlertDialogContent>
					<AlertDialogHeader>
						<AlertDialogTitle>
							{t("profiles:detail.dialogs.deleteTitle", {
								defaultValue: "Delete Configuration Profile",
							})}
						</AlertDialogTitle>
						<AlertDialogDescription>
							{t("profiles:detail.dialogs.deleteDescription", {
								defaultValue:
									'Are you sure you want to delete "{{name}}"? This action cannot be undone. All associated configurations will be permanently removed.',
								name: suit?.name ?? "",
							})}
						</AlertDialogDescription>
					</AlertDialogHeader>
					<AlertDialogFooter>
						<AlertDialogCancel>
							{t("profiles:form.buttons.cancel", { defaultValue: "Cancel" })}
						</AlertDialogCancel>
						<AlertDialogAction
							onClick={() => {
								deleteSuitMutation.mutate();
								setIsDeleteDialogOpen(false);
							}}
							className="bg-destructive text-destructive-foreground hover:bg-destructive/90"
							disabled={deleteSuitMutation.isPending}
						>
							{deleteSuitMutation.isPending
								? t("profiles:detail.buttons.deleting", {
									defaultValue: "Deleting...",
								})
								: t("profiles:detail.buttons.delete", {
									defaultValue: "Delete",
								})}
						</AlertDialogAction>
					</AlertDialogFooter>
				</AlertDialogContent>
			</AlertDialog>
			{/* Workflow Guide unsaved-changes guard */}
			<AlertDialog
				open={pendingWorkflowTab !== null}
				onOpenChange={(open) => {
					if (!open) setPendingWorkflowTab(null);
				}}
			>
				<AlertDialogContent>
					<AlertDialogHeader>
						<AlertDialogTitle>
							{t("profiles:detail.workflow.guide.unsavedLeaveTitle", {
								defaultValue: "Discard unsaved Workflow Guide changes?",
							})}
						</AlertDialogTitle>
						<AlertDialogDescription>
							{t(
								"profiles:detail.workflow.guide.unsavedLeaveDescription",
								{
									defaultValue:
										"The Workflow Guide has unsaved changes. Leaving this tab will discard them.",
								},
							)}
						</AlertDialogDescription>
					</AlertDialogHeader>
					<AlertDialogFooter>
						<AlertDialogCancel>
							{t("profiles:form.buttons.cancel", { defaultValue: "Cancel" })}
						</AlertDialogCancel>
						<AlertDialogAction
							onClick={() => {
								const tab = pendingWorkflowTab;
								setPendingWorkflowTab(null);
								setWorkflowGuideDirty(false);
								if (tab) applyDetailTabChange(tab);
							}}
						>
							{t("profiles:detail.workflow.guide.discardAndLeave", {
								defaultValue: "Discard and leave",
							})}
						</AlertDialogAction>
					</AlertDialogFooter>
				</AlertDialogContent>
			</AlertDialog>
		</div>
	);
}
