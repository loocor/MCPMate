import {
  useMutation,
  useQueries,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import {
  Check,
  Download,
  HardDrive,
  Info,
  Link2,
  MoreVertical,
  Pencil,
  RefreshCw,
  RotateCcw,
  ShieldCheck,
  ShieldX,
  Trash2,
  Unlink,
} from "lucide-react";
import { useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  Link,
  useNavigate,
  useParams,
  useSearchParams,
} from "react-router-dom";
import { AuditLogsPanel } from "../../components/audit-logs-panel";
import { CachedAvatar } from "../../components/cached-avatar";
import {
  CapsuleStripeList,
  CapsuleStripeListItem,
} from "../../components/capsule-stripe-list";
import {
  CapsuleStripeLeadCircle,
  CapsuleStripeRowBody,
} from "../../components/capsule-stripe-row";
import { ClientFormDrawer } from "../../components/client-form-drawer";
import { ConfirmDialog } from "../../components/confirm-dialog";
import { SurfaceReviewDialog } from "../../components/surface-review-dialog";
import {
  DETAIL_TAB_CONTENT_CLASS,
  DETAIL_OVERVIEW_PINNED_SECTION_CLASS,
  DETAIL_OVERVIEW_SCROLLABLE_LIST_CLASS,
  DETAIL_OVERVIEW_STACK_CLASS,
} from "../../components/detail-tab-content-class";
import { useUrlTab } from "../../lib/hooks/use-url-state";

import { Badge } from "../../components/ui/badge";
import { Button } from "../../components/ui/button";
import { ButtonGroup, ButtonGroupText } from "../../components/ui/button-group";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "../../components/ui/card";
import {
  Drawer,
  DrawerContent,
  DrawerFooter,
  DrawerHeader,
  DrawerTitle,
} from "../../components/ui/drawer";
import { Input } from "../../components/ui/input";
import { Label } from "../../components/ui/label";
import { Segment } from "../../components/ui/segment";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../components/ui/select";
import {
  Tabs,
  TabsContent,
  TabsList,
  TabsTrigger,
} from "../../components/ui/tabs";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "../../components/ui/tooltip";
import {
  auditApi,
  assertCompleteCapabilityBatch,
  completeServerImportForProfile,
  clientsApi,
  configSuitsApi,
  extractImportStats,
  serversApi,
} from "../../lib/api";
import { resolveAutoAddTargetProfileId } from "../../lib/default-profile";
import { isCapabilityWorkingSetProfile } from "../../lib/profile-authoring-ui";
import { buildClientServersImportRequest } from "../../lib/server-import-payload";
import { invalidateServerCatalogAfterImport } from "../../lib/server-query-cache";
import { profileSyncErrorTranslationKey } from "../../lib/profile-sync-error";
import { mapDashboardSettingsToClientBackupPolicy } from "../../lib/client-backup-policy";
import {
  applyClientConfigWithResolvedSelection,
  buildClientApplySelectedConfig,
  resolveClientConfigSyncErrorMessage,
} from "../../lib/client-config-sync";
import { notifyError, notifyInfo, notifySuccess } from "../../lib/notify";
import { useAppStore } from "../../lib/store";
import {
  formatCapabilityLifecycle,
  getCapabilityLifecycle,
  shouldDisplayCapabilityKind,
  type CapabilityLifecycleLabels,
} from "../../lib/capability-lifecycle";
import { useClientDetailTranslations } from "./client-detail-translations";
import type {
  ClientBackupEntry,
  ClientBackupPolicySetReq,
  ClientCapabilityConfigData,
  ClientCapabilityConfigReq,
  ClientCapabilitySourceSelection,
  ClientConfigMode,
  ClientConfigUpdateData,
  ClientInfo,
  ConfigSuit,
  ServerSummary,
  ServersImportData,
  TransportRuleData,
  UnifyDirectCapabilityRefs,
} from "../../lib/types";
import { formatBackupTime } from "../../lib/utils";
import { ClientImportReviewDrawer } from "./components/client-import-review-drawer";
import { ConfigurationProfileTokenChart } from "./components/configuration-profile-token-chart";
import { ServerEntryTransportBadge } from "./components/server-entry-transport-badge";
import { hasPendingClientWritebackMutation } from "./client-writeback-policy";
import { formatTransportTag } from "./transport-labels";

type UnifyRouteMode = "broker_only" | "server_level" | "capability_level";
type DirectExposureRouteMode = Extract<
  UnifyRouteMode,
  "server_level" | "capability_level"
>;

const governanceClientsApi = clientsApi as typeof clientsApi & {
  suspendRecord: (payload: { identifier: string }) => Promise<unknown>;
};

function isMcpMateEntryName(name: string): boolean {
  return name.trim().toLowerCase() === "mcpmate";
}

const arrangeProfilesWithDefaultFirst = (items: ConfigSuit[] = []) => {
  if (!items.length) {
    return [] as ConfigSuit[];
  }
  const byName = (a: ConfigSuit, b: ConfigSuit) =>
    a.name.localeCompare(b.name, undefined, { sensitivity: "base" });
  const defaults = items.filter((profile) => profile.is_default).sort(byName);
  const others = items.filter((profile) => !profile.is_default).sort(byName);
  return [...defaults, ...others];
};

const SUPPORTED_TRANSPORT_OPTIONS = [
  "streamable_http",
  "sse",
  "stdio",
] as const;
type SupportedTransportOption = (typeof SUPPORTED_TRANSPORT_OPTIONS)[number];

function isSupportedTransportOption(
  value: string,
): value is SupportedTransportOption {
  return SUPPORTED_TRANSPORT_OPTIONS.includes(
    value as SupportedTransportOption,
  );
}

function resolveSelectedTransport(
  transports: Record<string, TransportRuleData> | null | undefined,
): "auto" | SupportedTransportOption {
  for (const transport of SUPPORTED_TRANSPORT_OPTIONS) {
    if (transports?.[transport]?.selected === true) {
      return transport;
    }
  }
  return "auto";
}

function withSelectedTransport(
  transports: Record<string, TransportRuleData> | null | undefined,
  selected: string,
): Record<string, TransportRuleData> {
  return Object.fromEntries(
    Object.entries(transports ?? {}).map(([transport, rule]) => [
      transport,
      {
        ...rule,
        selected:
          selected !== "auto" && transport === selected ? true : undefined,
      },
    ]),
  );
}

function normalizeCapabilityIds(ids: string[] = []): string[] {
  return Array.from(new Set(ids.map((id) => id.trim()).filter(Boolean))).sort();
}

function getUnifyServerSurfaces<T extends { server_id: string }>(
  selectedSurfaces: T[],
  serverId: string,
): T[] {
  return selectedSurfaces.filter((entry) => entry.server_id === serverId);
}

function normalizeDirectCapabilityRefs(
  refs?: UnifyDirectCapabilityRefs,
): UnifyDirectCapabilityRefs {
  return {
    tool_refs: normalizeCapabilityIds(refs?.tool_refs),
    prompt_refs: normalizeCapabilityIds(refs?.prompt_refs),
    resource_refs: normalizeCapabilityIds(refs?.resource_refs),
    template_refs: normalizeCapabilityIds(refs?.template_refs),
  };
}

function resolveNextCapabilityIds(
  currentIds: string[] = [],
  hasSelectedForServer: boolean,
  serverCapabilityIds: string[],
): string[] {
  const serverCapabilityIdSet = new Set(serverCapabilityIds);
  const otherIds = currentIds.filter((id) => !serverCapabilityIdSet.has(id));

  if (hasSelectedForServer) {
    return otherIds;
  }

  return normalizeCapabilityIds([...otherIds, ...serverCapabilityIds]);
}

function getCapabilityId(
  item: Record<string, unknown>,
  keys: string[],
): string | null {
  for (const key of keys) {
    const value = item[key];
    if (typeof value === "string" && value.trim()) {
      return value;
    }
  }
  return null;
}

function isUnifyServerMixedRouting(
  routeMode: UnifyRouteMode,
  toolSurfaceCount: number,
  toolsCount: number | undefined,
): boolean {
  if (routeMode !== "capability_level" || toolSurfaceCount === 0) {
    return false;
  }

  if (!toolsCount) {
    return true;
  }

  return toolSurfaceCount < toolsCount;
}

function getClientDirectCapabilitiesPath(
  identifier: string | undefined,
  serverId: string,
): string | null {
  if (!identifier) {
    return null;
  }

  return `/clients/${identifier}/direct/${serverId}`;
}

function toggleSelectedServerIds(
  currentIds: string[],
  serverId: string,
): string[] {
  if (currentIds.includes(serverId)) {
    return currentIds.filter((id) => id !== serverId);
  }

  return [...currentIds, serverId];
}

function buildCapabilityConfigPayloadBase(
  identifier: string,
  currentConfig: ClientCapabilityConfigData,
): Omit<ClientCapabilityConfigReq, "unify_direct_exposure"> {
  return {
    identifier,
    capability_source: currentConfig.capability_source,
    selected_profile_ids: currentConfig.selected_profile_ids,
    source_revision_set: currentConfig.source_revision_set,
  };
}

function buildUnifyDirectExposurePayload(
  routeMode: UnifyRouteMode,
  serverIds: string[],
  capabilityRefs: UnifyDirectCapabilityRefs,
): NonNullable<ClientCapabilityConfigReq["unify_direct_exposure"]> {
  return {
    route_mode: routeMode,
    server_ids: routeMode === "server_level" ? serverIds : [],
    capability_refs:
      routeMode === "capability_level"
        ? normalizeDirectCapabilityRefs(capabilityRefs)
        : {},
  };
}

function buildServerLevelExposureUpdate(
  identifier: string,
  currentConfig: ClientCapabilityConfigData,
  serverId: string,
): ClientCapabilityConfigReq {
  const currentServerIds =
    currentConfig.unify_direct_exposure?.server_ids ?? [];

  return {
    ...buildCapabilityConfigPayloadBase(identifier, currentConfig),
    unify_direct_exposure: buildUnifyDirectExposurePayload(
      "server_level",
      toggleSelectedServerIds(currentServerIds, serverId),
      {},
    ),
  };
}

function buildCapabilityLevelExposureUpdate(
  identifier: string,
  currentConfig: ClientCapabilityConfigData,
  nextCapabilityRefs: UnifyDirectCapabilityRefs,
): ClientCapabilityConfigReq {
  return {
    ...buildCapabilityConfigPayloadBase(identifier, currentConfig),
    unify_direct_exposure: buildUnifyDirectExposurePayload(
      "capability_level",
      [],
      nextCapabilityRefs,
    ),
  };
}

function getTransportOptionLabel(
  transport: string,
  t: ReturnType<typeof useTranslation>["t"],
): string {
  switch (transport) {
    case "streamable_http":
      return t("detail.configuration.transportOptions.streamableHttpLegacy", {
        defaultValue: "Streamable HTTP",
      });
    case "sse":
      return t("detail.configuration.transportOptions.sseLegacy", {
        defaultValue: "SSE (Legacy)",
      });
    case "stdio":
      return t("detail.configuration.transportOptions.stdio", {
        defaultValue: "STDIO",
      });
    default:
      return transport.toUpperCase();
  }
}

function resolveConfigModeForWritableState(
  requestedMode: ClientConfigMode,
  isWritableConfig: boolean,
): ClientConfigMode {
  if (requestedMode === "transparent" && !isWritableConfig) {
    return "hosted";
  }

  return requestedMode;
}

function isDeniedApprovalStatus(status?: string | null): boolean {
  return status === "suspended";
}

export function ClientDetailPage() {
  const { identifier } = useParams<{ identifier: string }>();
  const qc = useQueryClient();
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  const reviewItemId = searchParams.get("review_item");
  useClientDetailTranslations();
  const { t } = useTranslation("clients");
  const showClientLiveLogs = useAppStore(
    (state) => state.dashboardSettings.showClientLiveLogs,
  );
  const dashboardSettings = useAppStore((state) => state.dashboardSettings);
  const clientDefaultMode = useAppStore(
    (state) => state.dashboardSettings.clientDefaultMode,
  );
  const [displayName, setDisplayName] = useState("");
  const [selectedBackups, setSelectedBackups] = useState<string[]>([]);
  const [isClientFormOpen, setIsClientFormOpen] = useState(false);
  const [bulkConfirmOpen, setBulkConfirmOpen] = useState(false);
  const [attachmentActionConfirm, setAttachmentActionConfirm] = useState<
    "detach" | "attach" | null
  >(null);

  const { data: clientsData } = useQuery({
    queryKey: ["clients"],
    queryFn: () => clientsApi.list(false),
    retry: 1,
  });

  const currentClient = useMemo(
    () =>
      clientsData?.client?.find((client) => client.identifier === identifier),
    [clientsData?.client, identifier],
  );
  const supportsBackupOperations =
    Boolean(currentClient) &&
    currentClient?.writable_config !== false &&
    currentClient?.template?.managed_source !== "runtime_active_client";
  const detailTabs = useMemo(
    () =>
      !supportsBackupOperations
        ? ["overview", "configuration"]
        : ["overview", "configuration", "backups"],
    [supportsBackupOperations],
  );
  const { activeTab: tabValue, setActiveTab: setTabValue } = useUrlTab({
    paramName: "tab",
    defaultTab: "overview",
    validTabs: detailTabs,
  });
  const [selectedProfiles, setSelectedProfiles] = useState<string[]>([]);

  const limitId = useId();
  const [mode, setMode] = useState<ClientConfigMode>("hosted");
  const [transport, setTransport] = useState<string>("auto");
  const [unifyRouteMode, setUnifyRouteMode] =
    useState<UnifyRouteMode>("broker_only");
  const [unifySelectedServers, setUnifySelectedServers] = useState<string[]>(
    [],
  );
  const [unifyCapabilityIds, setUnifyCapabilityIds] =
    useState<UnifyDirectCapabilityRefs>({});
  const [hasUnifyDraftChanges, setHasUnifyDraftChanges] = useState(false);

  useEffect(() => {
    if (!currentClient) {
      return;
    }
    const isWritableConfig = currentClient.writable_config !== false;
    setDisplayName(currentClient.display_name || "");
    if (typeof currentClient.config_mode === "string") {
      const configMode = currentClient.config_mode;
      if (configMode === "unify") {
        setMode("unify");
      } else if (configMode === "transparent") {
        setMode(
          resolveConfigModeForWritableState("transparent", isWritableConfig),
        );
      } else {
        setMode("hosted");
      }
    } else {
      setMode(
        resolveConfigModeForWritableState(clientDefaultMode, isWritableConfig),
      );
    }
    setTransport(resolveSelectedTransport(currentClient.transports));
  }, [clientDefaultMode, currentClient]);

  useEffect(() => {
    if (currentClient?.writable_config === false && tabValue === "backups") {
      setTabValue("configuration");
    }
  }, [currentClient?.writable_config, setTabValue, tabValue]);

  const [selectedConfig, setSelectedConfig] =
    useState<ClientCapabilitySourceSelection>("default");
  const [policyOpen, setPolicyOpen] = useState(false);
  const [importReviewOpen, setImportReviewOpen] = useState(false);

  const {
    data: configDetails,
    isLoading: loadingConfig,
    refetch: refetchDetails,
  } = useQuery({
    queryKey: ["client-config", identifier],
    queryFn: () => clientsApi.configDetails(identifier || "", false),
    enabled: !!identifier,
  });

  const { data: capabilityConfig } = useQuery({
    queryKey: ["client-capability-config", identifier],
    queryFn: () => clientsApi.getCapabilityConfig(identifier || ""),
    enabled: !!identifier,
  });

  const {
    data: backupsData,
    isLoading: loadingBackups,
    refetch: refetchBackups,
  } = useQuery({
    queryKey: ["client-backups", identifier],
    queryFn: () => clientsApi.listBackups(identifier || undefined),
    enabled: Boolean(identifier && supportsBackupOperations),
  });

  // Fetch eligible servers for Unify direct exposure
  const { data: serversListData } = useQuery({
    queryKey: ["servers", "unify-candidates"],
    queryFn: () => serversApi.getAll(),
    staleTime: 0,
    gcTime: 0,
    refetchOnMount: "always",
    refetchOnWindowFocus: "always",
    refetchOnReconnect: "always",
    retry: 1,
  });
  const eligibleServers = useMemo(() => {
    return (
      serversListData?.servers?.filter(
        (s) => s.unify_direct_exposure_eligible,
      ) || []
    );
  }, [serversListData]);

  const refreshClientCapabilityState = useCallback(async () => {
    await Promise.allSettled([
      qc.invalidateQueries({
        queryKey: ["client-capability-config", identifier],
      }),
      qc.invalidateQueries({ queryKey: ["client-config", identifier] }),
      qc.invalidateQueries({ queryKey: ["clients"] }),
    ]);
  }, [identifier, qc]);

  const { data: profilesData, isLoading: loadingProfiles } = useQuery({
    queryKey: ["profiles"],
    queryFn: () => configSuitsApi.getAll(),
    retry: 1,
  });

  const visibleBackups = useMemo(() => {
    const backups: ClientBackupEntry[] = backupsData?.backups || [];
    return backups.filter((backup) => backup.identifier === identifier);
  }, [backupsData?.backups, identifier]);

  // Process profiles data
  const activeProfiles = useMemo(() => {
    const profiles: ConfigSuit[] = profilesData?.suits || [];
    return arrangeProfilesWithDefaultFirst(
      profiles.filter(
        (profile) =>
          profile.is_active && isCapabilityWorkingSetProfile(profile),
      ),
    );
  }, [profilesData?.suits]);
  const sharedProfiles = useMemo(() => {
    const profiles: ConfigSuit[] = profilesData?.suits || [];
    return arrangeProfilesWithDefaultFirst(
      profiles.filter(
        (profile) =>
          profile.suit_type === "shared" &&
          isCapabilityWorkingSetProfile(profile),
      ),
    );
  }, [profilesData?.suits]);

  const effectiveCapabilityConfig =
    useMemo<ClientCapabilityConfigData | null>(() => {
      return capabilityConfig ?? null;
    }, [capabilityConfig]);

  const customProfileMissing =
    effectiveCapabilityConfig?.custom_profile_missing ??
    configDetails?.custom_profile_missing ??
    false;

  const customProfileId = customProfileMissing
    ? null
    : (effectiveCapabilityConfig?.custom_profile_id ??
      configDetails?.custom_profile_id ??
      null);

  useEffect(() => {
    setHasUnifyDraftChanges(false);
  }, [identifier]);

  useEffect(() => {
    if (!effectiveCapabilityConfig) {
      return;
    }
    if (hasUnifyDraftChanges) {
      return;
    }
    switch (effectiveCapabilityConfig.capability_source) {
      case "profiles":
        setSelectedConfig("profile");
        setSelectedProfiles(
          effectiveCapabilityConfig.selected_profile_ids || [],
        );
        break;
      case "custom":
        setSelectedConfig("custom");
        setSelectedProfiles([]);
        break;
      case "activated":
      default:
        setSelectedConfig("default");
        setSelectedProfiles([]);
        break;
    }
    if (effectiveCapabilityConfig.unify_direct_exposure) {
      const exposure = effectiveCapabilityConfig.unify_direct_exposure;
      setUnifyRouteMode(exposure.route_mode || "broker_only");
      setUnifySelectedServers(exposure.server_ids ?? []);
      setUnifyCapabilityIds(
        normalizeDirectCapabilityRefs(exposure.capability_refs),
      );
    } else {
      setUnifyRouteMode("broker_only");
      setUnifySelectedServers([]);
      setUnifyCapabilityIds({});
    }
  }, [effectiveCapabilityConfig, hasUnifyDraftChanges]);

  const capabilityProfileIds = useMemo(() => {
    const ids = new Set<string>();
    for (const profile of [...activeProfiles, ...sharedProfiles]) {
      ids.add(profile.id);
    }
    if (customProfileId) {
      ids.add(customProfileId);
    }
    return Array.from(ids);
  }, [activeProfiles, customProfileId, sharedProfiles]);

  // Fetch capabilities for profiles
  const profileCapabilitiesQueries = useQueries({
    queries: capabilityProfileIds.map((profileId) => ({
      queryKey: ["profile-capabilities", profileId],
      queryFn: async () => {
        const [serversRes, toolsRes, resourcesRes, promptsRes, templatesRes] =
          await Promise.all([
            configSuitsApi.getServers(profileId),
            configSuitsApi.getTools(profileId),
            configSuitsApi.getResources(profileId),
            configSuitsApi.getPrompts(profileId),
            configSuitsApi.getResourceTemplates(profileId),
          ]);

        const enabledByComponentId = new Map<string, boolean>();
        for (const s of serversRes?.servers ?? []) {
          enabledByComponentId.set(s.id, Boolean(s.enabled));
        }
        for (const tool of toolsRes?.tools ?? []) {
          enabledByComponentId.set(tool.id, Boolean(tool.enabled));
        }
        for (const r of resourcesRes?.resources ?? []) {
          enabledByComponentId.set(r.id, Boolean(r.enabled));
        }
        for (const p of promptsRes?.prompts ?? []) {
          enabledByComponentId.set(p.id, Boolean(p.enabled));
        }
        for (const tmpl of templatesRes?.templates ?? []) {
          enabledByComponentId.set(tmpl.id, Boolean(tmpl.enabled));
        }

        return {
          profileId,
          enabledByComponentId,
          servers: {
            total: serversRes?.servers?.length || 0,
            enabled:
              serversRes?.servers?.filter(
                (s: { enabled?: boolean }) => s.enabled,
              ).length || 0,
          },
          tools: {
            total: toolsRes?.tools?.length || 0,
            enabled:
              toolsRes?.tools?.filter(
                (tool: { enabled?: boolean }) => tool.enabled,
              ).length || 0,
          },
          resources: {
            total: resourcesRes?.resources?.length || 0,
            enabled:
              resourcesRes?.resources?.filter(
                (r: { enabled?: boolean }) => r.enabled,
              ).length || 0,
          },
          prompts: {
            total: promptsRes?.prompts?.length || 0,
            enabled:
              promptsRes?.prompts?.filter(
                (p: { enabled?: boolean }) => p.enabled,
              ).length || 0,
          },
        };
      },
      enabled: capabilityProfileIds.length > 0,
      retry: 1,
    })),
  });

  // Create capabilities map
  const profileCapabilities = useMemo(() => {
    const map = new Map();
    profileCapabilitiesQueries.forEach((query) => {
      const data = query.data ?? undefined;
      if (data) {
        map.set(data.profileId, data);
      }
    });
    return map;
  }, [profileCapabilitiesQueries]);

  const customProfileCapabilities = customProfileId
    ? profileCapabilities.get(customProfileId)
    : undefined;
  const managedTransportSupported =
    Object.keys(configDetails?.transports ?? {}).length > 0;
  const canWriteClientConfig = configDetails?.writable_config !== false;
  const isPendingApproval = configDetails?.approval_status === "pending";
  const isSuspendedClient = isDeniedApprovalStatus(
    configDetails?.approval_status,
  );
  const isAttachmentApplicable =
    configDetails?.attachment_state === "attached" ||
    configDetails?.attachment_state === "detached";
  const isAttachedClient =
    isAttachmentApplicable && configDetails?.attachment_state === "attached";
  const showLocalConfigMetadata =
    isAttachmentApplicable && Boolean(configDetails?.config_path);
  const overviewActionButtonClass =
    "gap-2 rounded-none first:rounded-l-md last:rounded-r-md";
  const canSaveManagementSettings = !isPendingApproval;
  const canApplyTransparentConfig =
    canSaveManagementSettings &&
    canWriteClientConfig &&
    !isDeniedApprovalStatus(configDetails?.approval_status);
  const canSyncManagedConfig =
    canWriteClientConfig &&
    canSaveManagementSettings &&
    !isDeniedApprovalStatus(configDetails?.approval_status) &&
    managedTransportSupported;
  const shouldRequireLocalConfigWrite = mode === "transparent";

  const supportedTransportOptions = useMemo(() => {
    const supported = Object.keys(configDetails?.transports ?? {});
    if (supported.length < 2) {
      return [] as string[];
    }

    const allowed = new Set(supported);
    return SUPPORTED_TRANSPORT_OPTIONS.filter((transportOption) =>
      allowed.has(transportOption),
    );
  }, [configDetails?.transports]);

  const unifyRouteModeSegmentOptions = useMemo(() => {
    return [
      {
        value: "broker_only",
        label: t(
          "detail.configuration.sections.source.unifyRouteModes.broker_only",
          {
            defaultValue: "Broker Only",
          },
        ),
      },
      {
        value: "server_level",
        label: t(
          "detail.configuration.sections.source.unifyRouteModes.server_level",
          {
            defaultValue: "Server Level",
          },
        ),
      },
      {
        value: "capability_level",
        label: t(
          "detail.configuration.sections.source.unifyRouteModes.capability_level",
          {
            defaultValue: "Capability Level",
          },
        ),
      },
    ];
  }, [t]);

  const configurationSourceSegmentOptions = useMemo(() => {
    const statusOrUndefined = (raw: string) => {
      const trimmed = raw.trim();
      return trimmed.length > 0 ? trimmed : undefined;
    };
    return [
      {
        value: "default",
        label: t("detail.configuration.sections.source.options.default", {
          defaultValue: "Active",
        }),
        status: statusOrUndefined(
          t("detail.configuration.sections.source.statusLabel.default", {
            defaultValue: "",
          }),
        ),
      },
      {
        value: "profile",
        label: t("detail.configuration.sections.source.options.profile", {
          defaultValue: "Profiles",
        }),
        status: statusOrUndefined(
          t("detail.configuration.sections.source.statusLabel.profile", {
            defaultValue: "",
          }),
        ),
      },
      {
        value: "custom",
        label: t("detail.configuration.sections.source.options.custom", {
          defaultValue: "Customize",
        }),
        status: statusOrUndefined(
          t("detail.configuration.sections.source.statusLabel.custom", {
            defaultValue: "",
          }),
        ),
      },
    ];
  }, [t]);

  const detailDescription = configDetails?.description ?? "";
  const detailHomepageUrl = configDetails?.homepage_url ?? "";
  const detailDocsUrl = configDetails?.docs_url ?? "";
  const detailSupportUrl = configDetails?.support_url ?? "";
  const renderOverviewActionButtons = () => {
    if (!configDetails) return null;
    return (
      <ButtonGroup className="ml-auto flex-shrink-0 flex-nowrap self-start">
        <Button
          variant="outline"
          size="sm"
          onClick={() => refreshDetectMutation.mutate()}
          disabled={refreshDetectMutation.isPending}
          className={overviewActionButtonClass}
        >
          <RefreshCw
            className={`h-4 w-4 ${refreshDetectMutation.isPending ? "animate-spin" : ""}`}
          />
          {t("detail.overview.buttons.refresh", {
            defaultValue: "Refresh",
          })}
        </Button>
        <Button
          variant="outline"
          size="sm"
          onClick={() => setIsClientFormOpen(true)}
          className={overviewActionButtonClass}
        >
          <Pencil className="h-4 w-4" />
          {t("detail.overview.buttons.edit", {
            defaultValue: "Edit",
          })}
        </Button>
      </ButtonGroup>
    );
  };
  const managementModeSegmentOptions = useMemo(() => {
    return [
      {
        value: "unify",
        label: t("detail.configuration.sections.mode.options.unify", {
          defaultValue: "Unify",
        }),
      },
      {
        value: "hosted",
        label: t("detail.configuration.sections.mode.options.hosted", {
          defaultValue: "Hosted",
        }),
      },
      {
        value: "transparent",
        label: t("detail.configuration.sections.mode.options.transparent", {
          defaultValue: "Transparent",
        }),
        disabled: configDetails?.writable_config === false,
        tooltip:
          configDetails?.writable_config === false
            ? t(
              "detail.configuration.sections.mode.transparentDisabledReason",
              {
                defaultValue:
                  "Transparent requires a writable local config path.",
              },
            )
            : undefined,
      },
    ];
  }, [configDetails?.writable_config, t]);
  const [logFilter, setLogFilter] = useState("");
  const [logPageSize, setLogPageSize] = useState<number>(10);
  const [logPageCursors, setLogPageCursors] = useState<string[]>([]);
  const [logCurrentPageIndex, setLogCurrentPageIndex] = useState(0);
  const [isLogPaginationActionLoading, setIsLogPaginationActionLoading] =
    useState(false);
  const logCurrentCursor = logPageCursors[logCurrentPageIndex];

  const logsQuery = useQuery({
    queryKey: [
      "client-audit-logs",
      identifier,
      logCurrentCursor,
      logPageSize,
      showClientLiveLogs,
    ],
    queryFn: () =>
      auditApi.list({
        limit: logPageSize,
        cursor: logCurrentCursor,
        client_id: identifier,
      }),
    enabled: Boolean(identifier && showClientLiveLogs),
    refetchOnWindowFocus: false,
    retry: false,
  });

  useEffect(() => {
    setLogPageCursors([]);
    setLogCurrentPageIndex(0);
  }, [identifier, logPageSize]);

  const filteredLogs = useMemo(() => {
    const logs = logsQuery.data?.events ?? [];
    const term = logFilter.trim().toLowerCase();
    if (!term) {
      return logs;
    }
    return logs.filter((event) => {
      const haystacks = [
        event.action,
        event.category,
        event.status,
        event.target,
        event.route,
        event.error_message,
        event.detail,
        event.mcp_method,
        event.request_id,
      ]
        .filter(Boolean)
        .map((value) => String(value).toLowerCase());
      return haystacks.some((value) => value.includes(term));
    });
  }, [logsQuery.data?.events, logFilter]);

  const handleLogsNextPage = () => {
    if (!logsQuery.data?.next_cursor) {
      return;
    }
    const nextCursor = logsQuery.data.next_cursor;
    setLogPageCursors((prev) => {
      const next = [...prev];
      next[logCurrentPageIndex + 1] = nextCursor;
      return next;
    });
    setLogCurrentPageIndex((prev) => prev + 1);
  };

  const handleLogsPrevPage = () => {
    if (logCurrentPageIndex > 0) {
      setLogCurrentPageIndex((prev) => prev - 1);
    }
  };

  const handleLogsFirstPage = () => {
    setLogCurrentPageIndex(0);
  };

  const handleLogsLastPage = async () => {
    if (!logsQuery.data?.next_cursor || !identifier) {
      return;
    }
    setIsLogPaginationActionLoading(true);
    try {
      let nextCursor: string | undefined = logsQuery.data.next_cursor;
      let targetPageIndex = logCurrentPageIndex;
      const nextPageCursors = [...logPageCursors];
      while (nextCursor) {
        targetPageIndex += 1;
        nextPageCursors[targetPageIndex] = nextCursor;
        const page = await auditApi.list({
          limit: logPageSize,
          cursor: nextCursor,
          client_id: identifier,
        });
        if (!page) {
          break;
        }
        nextCursor = page.next_cursor ?? undefined;
      }
      setLogPageCursors(nextPageCursors);
      setLogCurrentPageIndex(targetPageIndex);
    } finally {
      setIsLogPaginationActionLoading(false);
    }
  };

  const buildCapabilityConfigPayload = (): ClientCapabilityConfigReq => {
    if (!identifier) {
      throw new Error("No identifier provided");
    }
    if (!effectiveCapabilityConfig) {
      throw new Error("Client capability configuration is not loaded.");
    }

    const payload: ClientCapabilityConfigReq = {
      identifier,
      config_mode: mode,
      capability_source: "activated",
      selected_profile_ids: [],
      source_revision_set: effectiveCapabilityConfig.source_revision_set,
    };

    if (mode === "unify") {
      payload.capability_source = "activated";
      payload.selected_profile_ids = [];
      payload.unify_direct_exposure = buildUnifyDirectExposurePayload(
        unifyRouteMode,
        unifySelectedServers,
        unifyCapabilityIds,
      );
    } else {
      payload.unify_direct_exposure =
        effectiveCapabilityConfig?.unify_direct_exposure || null;
      if (selectedConfig === "profile") {
        const profileIds =
          selectedProfiles.length > 0
            ? selectedProfiles
            : ([sharedProfiles.find((p) => p.is_default)?.id].filter(
              Boolean,
            ) as string[]);

        if (profileIds.length === 0) {
          throw new Error(
            t("detail.configuration.errors.profileRequired", {
              defaultValue:
                "Select at least one shared profile before applying this capability source.",
            }),
          );
        }
        payload.capability_source = "profiles";
        payload.selected_profile_ids = profileIds;
      } else if (selectedConfig === "custom") {
        payload.capability_source = "custom";
        payload.selected_profile_ids = [];
      } else {
        payload.capability_source = "activated";
        payload.selected_profile_ids = [];
      }
    }

    return payload;
  };

  const directExposureServerMutation = useMutation<
    ClientCapabilityConfigData | null,
    unknown,
    { server: ServerSummary; routeMode: DirectExposureRouteMode }
  >({
    mutationFn: async ({ server, routeMode }) => {
      if (!identifier) {
        throw new Error("No identifier provided");
      }

      const currentConfig =
        (await clientsApi.getCapabilityConfig(identifier)) ??
        effectiveCapabilityConfig;
      if (!currentConfig) {
        throw new Error(
          t("detail.configuration.errors.capabilityConfigMissing", {
            defaultValue:
              "Capability configuration update returned no data. Please try again.",
          }),
        );
      }

      if (routeMode === "server_level") {
        return clientsApi.updateCapabilityConfig(
          buildServerLevelExposureUpdate(identifier, currentConfig, server.id),
        );
      }

      const capabilityLists = await serversApi.listAllCapabilities(server.id);
      assertCompleteCapabilityBatch(capabilityLists);
      const {
        tools: toolsResponse,
        prompts: promptsResponse,
        resources: resourcesResponse,
        templates: templatesResponse,
      } = capabilityLists;

      const currentUnifyExposure = currentConfig.unify_direct_exposure ?? {
        route_mode: "capability_level" as const,
        server_ids: [],
        capability_refs: {},
      };

      const currentCapabilityRefs = normalizeDirectCapabilityRefs(
        currentUnifyExposure.capability_refs,
      );

      const rawTools = Array.isArray(toolsResponse.items)
        ? (toolsResponse.items as Array<Record<string, unknown>>)
        : [];
      const rawPrompts = Array.isArray(promptsResponse.items)
        ? (promptsResponse.items as Array<Record<string, unknown>>)
        : [];
      const rawResources = Array.isArray(resourcesResponse.items)
        ? (resourcesResponse.items as Array<Record<string, unknown>>)
        : [];
      const rawTemplates = Array.isArray(templatesResponse.items)
        ? (templatesResponse.items as Array<Record<string, unknown>>)
        : [];

      const serverToolIds = rawTools
        .map((tool) => getCapabilityId(tool, ["ref_id"]))
        .filter((id): id is string => Boolean(id));
      const serverPromptIds = rawPrompts
        .map((prompt) => getCapabilityId(prompt, ["ref_id"]))
        .filter((id): id is string => Boolean(id));
      const serverResourceIds = rawResources
        .map((resource) => getCapabilityId(resource, ["ref_id"]))
        .filter((id): id is string => Boolean(id));
      const serverTemplateIds = rawTemplates
        .map((template) => getCapabilityId(template, ["ref_id"]))
        .filter((id): id is string => Boolean(id));
      const serverCapabilityIdSet = new Set([
        ...serverToolIds,
        ...serverPromptIds,
        ...serverResourceIds,
        ...serverTemplateIds,
      ]);
      const hasAnySelectedForServer = [
        ...(currentCapabilityRefs.tool_refs ?? []),
        ...(currentCapabilityRefs.prompt_refs ?? []),
        ...(currentCapabilityRefs.resource_refs ?? []),
        ...(currentCapabilityRefs.template_refs ?? []),
      ].some((id) => serverCapabilityIdSet.has(id));

      const nextCapabilityRefs: UnifyDirectCapabilityRefs = {
        tool_refs: resolveNextCapabilityIds(
          currentCapabilityRefs.tool_refs,
          hasAnySelectedForServer,
          serverToolIds,
        ),
        prompt_refs: resolveNextCapabilityIds(
          currentCapabilityRefs.prompt_refs,
          hasAnySelectedForServer,
          serverPromptIds,
        ),
        resource_refs: resolveNextCapabilityIds(
          currentCapabilityRefs.resource_refs,
          hasAnySelectedForServer,
          serverResourceIds,
        ),
        template_refs: resolveNextCapabilityIds(
          currentCapabilityRefs.template_refs,
          hasAnySelectedForServer,
          serverTemplateIds,
        ),
      };

      return clientsApi.updateCapabilityConfig(
        buildCapabilityLevelExposureUpdate(
          identifier,
          currentConfig,
          nextCapabilityRefs,
        ),
      );
    },
    onSuccess: async (data, { routeMode }) => {
      if (routeMode === "server_level") {
        setUnifySelectedServers(data?.unify_direct_exposure?.server_ids ?? []);
        setHasUnifyDraftChanges(false);
      }
      await refreshClientCapabilityState();
    },
    onError: (error) =>
      notifyError(
        t("detail.directExposure.notifications.saveFailedTitle", {
          defaultValue: "Unable to update direct capabilities",
        }),
        String(error),
      ),
  });

  const renderProfileCapabilitySummary = (
    capabilities: NonNullable<ReturnType<typeof profileCapabilities.get>>,
  ) => {
    const capabilityItems = [
      {
        key: "servers",
        label: t("detail.configuration.labels.servers", {
          defaultValue: "Servers",
        }),
        counts: capabilities.servers,
      },
      {
        key: "tools",
        label: t("detail.configuration.labels.tools", {
          defaultValue: "Tools",
        }),
        counts: capabilities.tools,
      },
      {
        key: "resources",
        label: t("detail.configuration.labels.resources", {
          defaultValue: "Resources",
        }),
        counts: capabilities.resources,
      },
      {
        key: "prompts",
        label: t("detail.configuration.labels.prompts", {
          defaultValue: "Prompts",
        }),
        counts: capabilities.prompts,
      },
    ];

    return (
      <div className="mt-1 flex gap-4 text-xs text-slate-500">
        {capabilityItems.map((item) => (
          <span key={item.key}>
            {item.label}: {item.counts.enabled}/{item.counts.total}
          </span>
        ))}
      </div>
    );
  };

  const renderUnifyEligibleServerCapabilitySummary = (
    server: ServerSummary,
    routeMode: DirectExposureRouteMode,
    exposedToolCount: number,
  ) => {
    const cap = server.capability;
    const lifecycleLabels: CapabilityLifecycleLabels = {
      unavailable: t("servers:capabilityLifecycle.capabilityUnavailable"),
      unsupported: t("servers:capabilityLifecycle.capabilityUnsupported"),
      unknown: t("servers:capabilityLifecycle.capabilityUnknown"),
      empty: t("servers:capabilityLifecycle.capabilityEmpty"),
      ready: t("servers:capabilityLifecycle.capabilityReady"),
    };
    const toolsTotal = cap?.tools.currentAvailable
      ? cap.tools.currentCount
      : undefined;
    // In capability_level mode the fraction already carries the total count, so append only
    // the lifecycle label here. Reusing `formatCapabilityLifecycle` (which also prefixes its
    // own count) would render the total twice, e.g. "3/10 · 10 · Ready" (Codex review
    // follow-up, PR #160).
    const toolsLifecycle = getCapabilityLifecycle(cap, "tools");
    const toolsValue = !shouldDisplayCapabilityKind(cap, "tools")
      ? null
      : routeMode === "capability_level"
        ? `${exposedToolCount}/${toolsTotal ?? "—"} · ${lifecycleLabels[toolsLifecycle.state]}`
        : formatCapabilityLifecycle(cap, "tools", lifecycleLabels);
    const items = [
      {
        key: "tools",
        label: t("detail.configuration.labels.tools", {
          defaultValue: "Tools",
        }),
        value: toolsValue,
      },
      {
        key: "resources",
        label: t("detail.configuration.labels.resources", {
          defaultValue: "Resources",
        }),
        value: formatCapabilityLifecycle(cap, "resources", lifecycleLabels),
      },
      {
        key: "prompts",
        label: t("detail.configuration.labels.prompts", {
          defaultValue: "Prompts",
        }),
        value: formatCapabilityLifecycle(cap, "prompts", lifecycleLabels),
      },
      {
        key: "resourceTemplates",
        label: t("detail.configuration.labels.resourceTemplates", {
          defaultValue: "Resource templates",
        }),
        value: formatCapabilityLifecycle(cap, "resourceTemplates", lifecycleLabels),
      },
    ].filter(
      (
        item,
      ): item is {
        key: string;
        label: string;
        value: string;
      } => item.value != null,
    );
    if (items.length === 0) {
      return null;
    }
    return (
      <div className="mt-1 flex flex-wrap gap-x-4 gap-y-1 text-xs text-slate-500">
        {items.map((item) => (
          <span key={item.key}>
            {item.label}: {item.value}
          </span>
        ))}
      </div>
    );
  };

  const getConfigurationProfileTokenSlot = (
    profile: ConfigSuit,
    stopPropagationOnNavigate: boolean,
  ) => {
    const capData = profileCapabilities.get(profile.id);
    const capIdx = capabilityProfileIds.indexOf(profile.id);
    const capQuery =
      capIdx >= 0 ? profileCapabilitiesQueries[capIdx] : undefined;

    if (capQuery?.isError) {
      return (
        <Link
          to={`/profiles/${profile.id}`}
          onClick={
            stopPropagationOnNavigate ? (e) => e.stopPropagation() : undefined
          }
          className="text-xs text-destructive underline underline-offset-2"
        >
          {t("detail.configuration.labels.openProfileDetail", {
            defaultValue: "Open profile details",
          })}
        </Link>
      );
    }

    if (!capData) {
      return (
        <div
          className="h-14 w-14 shrink-0 animate-pulse rounded-full bg-muted/50"
          aria-hidden
        />
      );
    }

    return (
      <ConfigurationProfileTokenChart
        profileId={profile.id}
        enabledByComponentId={capData.enabledByComponentId}
        profileServerCount={capData.servers.total}
        stopPropagationOnNavigate={stopPropagationOnNavigate}
      />
    );
  };

  const { data: policyData, refetch: refetchPolicy } = useQuery({
    queryKey: ["client-policy", identifier],
    queryFn: () => clientsApi.getBackupPolicy(identifier || ""),
    enabled: Boolean(identifier && supportsBackupOperations),
  });

  const reviewMutation = useMutation({
    mutationFn: async (action: "approve" | "suspend") => {
      if (!identifier) throw new Error("No identifier provided");
      return action === "approve"
        ? clientsApi.approveRecord({ identifier })
        : clientsApi.suspendRecord({ identifier });
    },
    onSuccess: (_, action) => {
      notifySuccess(
        t("detail.notifications.reviewSuccess.title", {
          defaultValue: "Success",
        }),
        t(
          action === "approve"
            ? "detail.notifications.reviewSuccess.messageApproved"
            : "detail.notifications.reviewSuccess.messageSuspended",
          {
            defaultValue:
              action === "approve"
                ? "Record approved successfully."
                : "Record suspended successfully.",
          },
        ),
      );
      qc.invalidateQueries({ queryKey: ["clients"] });
      qc.invalidateQueries({ queryKey: ["client-config", identifier] });
    },
    onError: (e) =>
      notifyError(
        t("detail.notifications.reviewFailed.title", {
          defaultValue: "Review failed",
        }),
        String(e),
      ),
  });

  const clientWritebackMutationLock = useRef<
    "apply" | "attachment" | null
  >(null);

  function beginClientWritebackMutation(
    operation: "apply" | "attachment",
  ): () => void {
    if (clientWritebackMutationLock.current) {
      throw new Error(
        t("detail.configuration.writeback.notifications.operationPending", {
          defaultValue:
            "Wait for the current client configuration operation to finish.",
        }),
      );
    }

    clientWritebackMutationLock.current = operation;
    return () => {
      if (clientWritebackMutationLock.current === operation) {
        clientWritebackMutationLock.current = null;
      }
    };
  }

  async function runClientWritebackMutation<T>(
    operation: "apply" | "attachment",
    mutation: () => Promise<T>,
  ): Promise<T> {
    const release = beginClientWritebackMutation(operation);
    try {
      return await mutation();
    } finally {
      release();
    }
  }

  const detachMutation = useMutation({
    mutationFn: async () => {
      if (!identifier) throw new Error("No identifier provided");
      return runClientWritebackMutation("attachment", () =>
        clientsApi.detach(identifier),
      );
    },
    onSuccess: () => {
      notifySuccess(
        t("detail.notifications.detachSuccess.title", {
          defaultValue: "Detached",
        }),
        t("detail.notifications.detachSuccess.message", {
          defaultValue:
            "MCPMate was removed from the client configuration file.",
        }),
      );
      qc.invalidateQueries({ queryKey: ["clients"] });
      qc.invalidateQueries({ queryKey: ["client-config", identifier] });
    },
    onError: (e) =>
      notifyError(
        t("detail.notifications.detachFailed.title", {
          defaultValue: "Detach failed",
        }),
        String(e),
      ),
  });

  const attachMutation = useMutation({
    mutationFn: async () => {
      if (!identifier) throw new Error("No identifier provided");
      return runClientWritebackMutation("attachment", () =>
        clientsApi.attach(identifier),
      );
    },
    onSuccess: () => {
      notifySuccess(
        t("detail.notifications.attachSuccess.title", {
          defaultValue: "Attached",
        }),
        t("detail.notifications.attachSuccess.message", {
          defaultValue: "MCPMate was written back to the client configuration.",
        }),
      );
      qc.invalidateQueries({ queryKey: ["clients"] });
      qc.invalidateQueries({ queryKey: ["client-config", identifier] });
    },
    onError: (e) =>
      notifyError(
        t("detail.notifications.attachFailed.title", {
          defaultValue: "Attach failed",
        }),
        String(e),
      ),
  });

  const applyMutation = useMutation<
    {
      data: ClientConfigUpdateData | null;
      preview: boolean;
      clientConfigApplied: boolean;
    },
    unknown,
    { preview: boolean }
  >({
    mutationFn: async ({ preview }) => {
      if (!identifier) throw new Error("No identifier provided");
      if (!canSaveManagementSettings) {
        throw new Error(
          t("detail.configuration.managementSettingsPendingReason", {
            defaultValue:
              "Save management settings after this client leaves pending approval.",
          }),
        );
      }

      // Validate transparent-mode requirements before persisting any management
      // settings, so a failed precondition never leaves partial state in MCPMate.
      if (shouldRequireLocalConfigWrite) {
        if (!canWriteClientConfig) {
          throw new Error(
            t("detail.configuration.writeTargetRequiredReason", {
              defaultValue:
                "Applying governance to the client configuration requires a verified writable local MCP config file.",
            }),
          );
        }
        if (!canApplyTransparentConfig) {
          throw new Error(
            t("detail.configuration.applyRequiresApprovedReason", {
              defaultValue:
                "Applying client configuration requires an approved governance state and a verified local config target.",
            }),
          );
        }
      }

      return runClientWritebackMutation("apply", async () => {
        const capabilityData = await clientsApi.updateCapabilityConfig(
          buildCapabilityConfigPayload(),
        );
        if (!capabilityData) {
          throw new Error(
            t("detail.configuration.errors.capabilityConfigMissing", {
              defaultValue:
                "Capability configuration update returned no data. Please try again.",
            }),
          );
        }

        const selectedConfigForManagedApply =
          mode === "unify"
            ? "default"
            : buildClientApplySelectedConfig(capabilityData);

        if (shouldRequireLocalConfigWrite) {
          const data = await clientsApi.applyConfig({
            identifier,
            mode,
            selected_config: buildClientApplySelectedConfig(capabilityData),
            preview,
            backup_policy: preview
              ? undefined
              : mapDashboardSettingsToClientBackupPolicy(dashboardSettings),
          });
          return { data: data ?? null, preview, clientConfigApplied: true };
        }

        if (!canSyncManagedConfig) {
          return { data: null, preview, clientConfigApplied: false };
        }

        const data = await clientsApi.applyConfig({
          identifier,
          mode,
          selected_config: selectedConfigForManagedApply,
          preview,
          backup_policy: preview
            ? undefined
            : mapDashboardSettingsToClientBackupPolicy(dashboardSettings),
        });
        return { data: data ?? null, preview, clientConfigApplied: true };
      });
    },
    onSuccess: ({ data, preview, clientConfigApplied }) => {
      if (preview) {
        if (data) {
          notifyInfo(
            t("detail.notifications.previewReady.title", {
              defaultValue: "Preview ready",
            }),
            t("detail.notifications.previewReady.message", {
              defaultValue: "Review the diff before applying.",
            }),
          );
        } else {
          notifyInfo(
            t("detail.notifications.previewReady.title", {
              defaultValue: "Preview ready",
            }),
            t("detail.notifications.previewReady.noChanges", {
              defaultValue: "No changes detected in this configuration.",
            }),
          );
        }
      } else if (clientConfigApplied) {
        notifySuccess(
          t("detail.notifications.applied.title", {
            defaultValue: "Applied",
          }),
          t("detail.notifications.applied.message", {
            defaultValue: "Configuration applied",
          }),
        );
        refetchBackups();
      } else {
        notifySuccess(
          t("detail.notifications.managementSaved.title", {
            defaultValue: "Saved",
          }),
          t("detail.notifications.managementSaved.message", {
            defaultValue:
              "Management settings were saved in MCPMate. Local client configuration was not updated.",
          }),
        );
      }
      void refreshClientCapabilityState();
      setHasUnifyDraftChanges(false);
    },
    onError: (e) =>
      notifyError(
        t("detail.notifications.applyFailed.title", {
          defaultValue: "Apply failed",
        }),
        resolveClientConfigSyncErrorMessage(e, t),
      ),
  });

  const clientWritebackMutationPending = hasPendingClientWritebackMutation({
    applyPending: applyMutation.isPending,
    attachmentPending:
      detachMutation.isPending || attachMutation.isPending,
  });

  const importMutation = useMutation<ServersImportData, Error, string[]>({
    mutationFn: async (selectedServerNames) => {
      if (!identifier) throw new Error("No identifier provided");
      const targetProfileId = await resolveAutoAddTargetProfileId({
        autoAddEnabled: dashboardSettings.autoAddServerToDefaultProfile,
      });
      const resp = await serversApi.importServers(
        buildClientServersImportRequest({
          clientIdentifier: identifier,
          selectedServerNames,
        }),
      );
      if (
        resp &&
        typeof resp === "object" &&
        "success" in resp &&
        (resp as { success?: boolean }).success === false
      ) {
        throw new Error(
          (resp as { error?: unknown }).error
            ? String((resp as { error: unknown }).error)
            : "Import failed",
        );
      }
      const stats = extractImportStats(resp);
      await completeServerImportForProfile(targetProfileId, stats);
      return {
        imported_count: stats.importedCount,
        imported_servers: stats.importedServers,
        skipped_count: stats.skippedCount,
        skipped_servers: stats.skippedDetails,
        failed_count: stats.failedCount,
        failed_servers: stats.failedServers,
        error_details: stats.errorDetails ?? null,
        runtime_sync_error: stats.runtimeSyncError ?? null,
      };
    },
    onSuccess: (res) => {
      if (res.runtime_sync_error) {
        notifyError(
          "Runtime synchronization failed",
          res.runtime_sync_error,
        );
      }
      const imported = res.imported_count ?? 0;
      void invalidateServerCatalogAfterImport(qc, imported);
      if (imported > 0) {
        notifySuccess(
          t("detail.notifications.imported.title", {
            defaultValue: "Imported",
          }),
          imported === 1
            ? t("detail.notifications.imported.messageSingle", {
              defaultValue: "{{count}} server imported successfully",
              count: imported,
            })
            : t("detail.notifications.imported.messagePlural", {
              defaultValue: "{{count}} servers imported successfully",
              count: imported,
            }),
        );
        setImportReviewOpen(false);
        refetchDetails();
        if (dashboardSettings.autoAddServerToDefaultProfile) {
          void qc.invalidateQueries({ queryKey: ["configSuits"] });
        }
      } else {
        notifyInfo(
          t("detail.notifications.nothingToImport.title", {
            defaultValue: "Nothing to import",
          }),
          t("detail.notifications.nothingToImport.message", {
            defaultValue:
              "All entries were skipped or no importable servers found.",
          }),
        );
        setImportReviewOpen(false);
      }
    },
    onError: (e) =>
      notifyError(
        t("detail.notifications.importFailed.title", {
          defaultValue: "Import failed",
        }),
        t(profileSyncErrorTranslationKey(e)),
      ),
  });

  // Header actions: refresh detection and update governance state
  const refreshDetectMutation = useMutation({
    mutationFn: async () => {
      const data = await clientsApi.list(true);
      if (data?.client) {
        const f = data.client.find((c) => c.identifier === identifier);
        if (f) {
          if (typeof f.display_name === "string")
            setDisplayName(f.display_name);
        }
      }
      await refetchDetails();
    },
    onSuccess: () =>
      notifySuccess(
        t("detail.notifications.refreshed.title", {
          defaultValue: "Refreshed",
        }),
        t("detail.notifications.refreshed.message", {
          defaultValue: "Detection refreshed",
        }),
      ),
    onError: (e) =>
      notifyError(
        t("detail.notifications.refreshFailed.title", {
          defaultValue: "Refresh failed",
        }),
        String(e),
      ),
  });

  const governanceMutation = useMutation<
    void,
    unknown,
    { nextStatus: "approved" | "suspended" }
  >({
    mutationFn: async ({ nextStatus }) => {
      if (!identifier) throw new Error("No identifier provided");
      if (nextStatus === "approved") {
        await clientsApi.approveRecord({ identifier });
        return;
      }

      await governanceClientsApi.suspendRecord({ identifier });
    },
    onSuccess: async (_, { nextStatus }) => {
      await Promise.allSettled([
        refetchDetails(),
        qc.invalidateQueries({ queryKey: ["clients"] }),
        qc.invalidateQueries({ queryKey: ["client-config", identifier] }),
      ]);

      notifySuccess(
        t(
          nextStatus === "approved"
            ? "detail.notifications.governanceAllowed.title"
            : "detail.notifications.governanceDenied.title",
          {
            defaultValue: "Updated",
          },
        ),
        t(
          nextStatus === "approved"
            ? "detail.notifications.governanceAllowed.message"
            : "detail.notifications.governanceDenied.message",
          {
            defaultValue:
              nextStatus === "approved"
                ? "Client governance is now allowed."
                : "Client governance is now denied.",
          },
        ),
      );
    },
    onError: (e) =>
      notifyError(
        t("detail.notifications.governanceFailed.title", {
          defaultValue: "Update failed",
        }),
        String(e),
      ),
  });

  const restoreMutation = useMutation({
    mutationFn: ({ backup }: { backup: string }) => {
      if (!identifier) throw new Error("No identifier provided");
      return clientsApi.restoreConfig({ identifier, backup });
    },
    onSuccess: () => {
      notifySuccess(
        t("detail.notifications.restored.title", {
          defaultValue: "Restored",
        }),
        t("detail.notifications.restored.message", {
          defaultValue: "Local client configuration restored from backup",
        }),
      );
      refetchDetails();
      refetchBackups();
    },
    onError: (e) =>
      notifyError(
        t("detail.notifications.restoreFailed.title", {
          defaultValue: "Restore failed",
        }),
        String(e),
      ),
  });

  const deleteBackupMutation = useMutation({
    mutationFn: ({ backup }: { backup: string }) => {
      if (!identifier) throw new Error("No identifier provided");
      return clientsApi.deleteBackup(identifier, backup);
    },
    onSuccess: () => {
      notifySuccess(
        t("detail.notifications.deleted.title", {
          defaultValue: "Deleted",
        }),
        t("detail.notifications.deleted.message", {
          defaultValue: "Backup deleted",
        }),
      );
      refetchBackups();
    },
    onError: (e) =>
      notifyError(
        t("detail.notifications.deleteFailed.title", {
          defaultValue: "Delete failed",
        }),
        String(e),
      ),
  });

  const bulkDeleteMutation = useMutation({
    mutationFn: async () => {
      if (!identifier) throw new Error("No identifier provided");
      const items = [...selectedBackups];
      const results = await Promise.allSettled(
        items.map((b) => clientsApi.deleteBackup(identifier, b)),
      );
      const failed = results.filter((r) => r.status === "rejected").length;
      if (failed > 0) throw new Error(`${failed} deletions failed`);
    },
    onSuccess: async () => {
      notifySuccess(
        t("detail.notifications.bulkDeleted.title", {
          defaultValue: "Deleted",
        }),
        t("detail.notifications.bulkDeleted.message", {
          defaultValue: "Selected backups have been deleted",
        }),
      );
      setSelectedBackups([]);
      setBulkConfirmOpen(false);
      await refetchBackups();
    },
    onError: (e) =>
      notifyError(
        t("detail.notifications.bulkDeleteFailed.title", {
          defaultValue: "Bulk delete failed",
        }),
        String(e),
      ),
  });

  const [confirm, setConfirm] = useState<null | {
    kind: "delete" | "restore";
    backup: string;
  }>(null);

  const setPolicyMutation = useMutation({
    mutationFn: (payload: ClientBackupPolicySetReq) =>
      clientsApi.setBackupPolicy(payload),
    onSuccess: () => {
      notifySuccess(
        t("detail.notifications.saved.title", {
          defaultValue: "Saved",
        }),
        t("detail.notifications.saved.message", {
          defaultValue: "Backup policy updated",
        }),
      );
      refetchPolicy();
    },
    onError: (e) =>
      notifyError(
        t("detail.notifications.saveFailed.title", {
          defaultValue: "Save failed",
        }),
        String(e),
      ),
  });

  const [policyLabel, setPolicyLabel] = useState<string>("keep_n");
  const [policyLimit, setPolicyLimit] = useState<number | undefined>(5);
  useEffect(() => {
    if (policyData) {
      setPolicyLabel(policyData.policy || "keep_n");
      setPolicyLimit(policyData.limit ?? undefined);
    }
  }, [policyData]);
  const backupsDisabledByPolicy =
    visibleBackups.length === 0 &&
    (policyData?.policy === "off" || policyData?.policy === "none");

  const currentServerEntries = useMemo(
    () => configDetails?.configured_server_entries ?? [],
    [configDetails?.configured_server_entries],
  );
  const importCandidateEntries = useMemo(
    () =>
      currentServerEntries.filter((entry) => !isMcpMateEntryName(entry.name)),
    [currentServerEntries],
  );

  if (!identifier)
    return (
      <div className="p-4">
        {t("detail.noIdentifier", {
          defaultValue: "No client identifier provided.",
        })}
      </div>
    );

  return (
    <div className="flex h-full min-h-0 flex-col gap-4 overflow-hidden">
      <div className="flex shrink-0 items-center justify-between">
        <div className="flex flex-col gap-1">
          <div className="flex items-center gap-3 flex-wrap">
            <h2 className="text-3xl font-bold tracking-tight">
              {displayName || identifier}
            </h2>

            {configDetails?.approval_status === "pending" && (
              <Badge
                variant="outline"
                className="bg-blue-50 text-blue-800 border-blue-200 dark:bg-blue-950 dark:text-blue-200 dark:border-blue-800"
              >
                {t("detail.badges.pendingReview", {
                  defaultValue: "Pending Review",
                })}
              </Badge>
            )}
            {isSuspendedClient && (
              <Badge variant="destructive">
                {t("detail.badges.suspended", { defaultValue: "Suspended" })}
              </Badge>
            )}
            {isAttachmentApplicable ? (
              <Badge variant={isAttachedClient ? "secondary" : "outline"}>
                {isAttachedClient
                  ? t("detail.badges.attached", { defaultValue: "Attached" })
                  : t("detail.badges.detached", {
                    defaultValue: "Detached",
                  })}
              </Badge>
            ) : null}
          </div>
          {detailDescription ? (
            <p className="text-sm text-muted-foreground leading-snug w-full truncate">
              {detailDescription}
            </p>
          ) : null}
        </div>
        {/* 操作按钮移至 Overview 卡片右上角 */}
      </div>

      <Tabs
        value={tabValue}
        onValueChange={setTabValue}
        className="flex min-h-0 flex-1 flex-col gap-4"
      >
        <div className="flex shrink-0 items-center justify-between">
          <TabsList>
            <TabsTrigger value="overview">
              {t("detail.tabs.overview", { defaultValue: "Overview" })}
            </TabsTrigger>
            <TabsTrigger value="configuration">
              {t("detail.tabs.configuration", {
                defaultValue: "Configuration",
              })}
            </TabsTrigger>
            {configDetails?.writable_config !== false && (
              <TabsTrigger value="backups">
                {t("detail.tabs.backups", { defaultValue: "Backups" })}
              </TabsTrigger>
            )}
          </TabsList>
          {renderOverviewActionButtons()}
        </div>

        <TabsContent value="overview" className={DETAIL_TAB_CONTENT_CLASS}>
          <div className={DETAIL_OVERVIEW_STACK_CLASS}>
            <Card className={DETAIL_OVERVIEW_PINNED_SECTION_CLASS}>
              {loadingConfig ? (
                <CardContent className="text-sm">
                  <div className="animate-pulse h-16 bg-slate-200 dark:bg-slate-800 rounded" />
                </CardContent>
              ) : configDetails ? (
                <CardContent className="p-4">
                  <div className="flex flex-col gap-4">
                    <div className="flex flex-col gap-4 md:flex-row md:items-start md:justify-between">
                      <div className="flex flex-wrap items-start gap-4">
                        <CachedAvatar
                          src={configDetails.logo_url ?? undefined}
                          alt={displayName || identifier}
                          fallback={displayName || identifier || "C"}
                          className="text-sm"
                        />
                        <div className="grid grid-cols-[auto_1fr] gap-x-5 gap-y-2 text-sm">
                          {showLocalConfigMetadata ? (
                            <>
                              <span className="text-xs uppercase text-slate-500">
                                {t("detail.overview.labels.configPath", {
                                  defaultValue: "Config Path",
                                })}
                              </span>
                              <span className="font-mono text-xs truncate max-w-[520px]">
                                {configDetails.config_path}
                              </span>

                              <span className="text-xs uppercase text-slate-500">
                                {t("detail.overview.labels.lastModified", {
                                  defaultValue: "Last Modified",
                                })}
                              </span>
                              <span className="text-xs">
                                {formatBackupTime(configDetails.last_modified)}
                              </span>
                            </>
                          ) : null}

                          <span className="text-xs uppercase text-slate-500">
                            {t("detail.overview.labels.supportedTransports", {
                              defaultValue: "Transports",
                            })}
                          </span>
                          <span className="text-xs flex gap-2">
                            {Object.keys(configDetails?.transports ?? {}).map(
                              (transport) => (
                                <span
                                  key={transport}
                                  className="inline-flex items-center rounded-full bg-slate-100 dark:bg-slate-800 px-2 py-0.5 text-[10px] font-medium tracking-wide"
                                  title={transport}
                                >
                                  {formatTransportTag(transport)}
                                </span>
                              ),
                            )}
                          </span>

                          {detailHomepageUrl ? (
                            <>
                              <span className="text-xs uppercase text-slate-500">
                                {t("detail.overview.labels.homepage", {
                                  defaultValue: "Homepage",
                                })}
                              </span>
                              <a
                                href={detailHomepageUrl}
                                target="_blank"
                                rel="noreferrer"
                                className="text-xs underline underline-offset-2 truncate"
                              >
                                {detailHomepageUrl}
                              </a>
                            </>
                          ) : null}
                          {detailDocsUrl ? (
                            <>
                              <span className="text-xs uppercase text-slate-500">
                                {t("detail.overview.labels.docs", {
                                  defaultValue: "Docs",
                                })}
                              </span>
                              <a
                                href={detailDocsUrl}
                                target="_blank"
                                rel="noreferrer"
                                className="text-xs underline underline-offset-2 truncate"
                              >
                                {detailDocsUrl}
                              </a>
                            </>
                          ) : null}
                          {detailSupportUrl ? (
                            <>
                              <span className="text-xs uppercase text-slate-500">
                                {t("detail.overview.labels.support", {
                                  defaultValue: "Support",
                                })}
                              </span>
                              <a
                                href={detailSupportUrl}
                                target="_blank"
                                rel="noreferrer"
                                className="text-xs underline underline-offset-2 truncate"
                              >
                                {detailSupportUrl}
                              </a>
                            </>
                          ) : null}
                        </div>
                      </div>
                      <ButtonGroup className="ml-auto flex-shrink-0 flex-nowrap self-start">
                        {configDetails?.approval_status === "pending" && (
                          <>
                            <Button
                              variant="outline"
                              size="sm"
                              className="bg-green-50 text-green-700 hover:bg-green-100 hover:text-green-800 border-green-200 dark:bg-green-950 dark:text-green-300 dark:hover:bg-green-900 dark:border-green-800 gap-2"
                              onClick={() => reviewMutation.mutate("approve")}
                              disabled={reviewMutation.isPending}
                            >
                              <Check className="h-4 w-4" />
                              {t("detail.overview.buttons.approve", {
                                defaultValue: "Approve",
                              })}
                            </Button>
                            <Button
                              variant="outline"
                              size="sm"
                              className="bg-red-50 text-red-700 hover:bg-red-100 hover:text-red-800 border-red-200 dark:bg-red-950 dark:text-red-300 dark:hover:bg-red-900 dark:border-red-800 gap-2"
                              onClick={() => reviewMutation.mutate("suspend")}
                              disabled={reviewMutation.isPending}
                            >
                              <Trash2 className="h-4 w-4" />
                              {t("detail.overview.buttons.suspend", {
                                defaultValue: "Suspend",
                              })}
                            </Button>
                          </>
                        )}
                        <Button
                          variant="outline"
                          size="sm"
                          onClick={() =>
                            governanceMutation.mutate({
                              nextStatus: isDeniedApprovalStatus(
                                configDetails?.approval_status,
                              )
                                ? "approved"
                                : "suspended",
                            })
                          }
                          disabled={
                            governanceMutation.isPending ||
                            !configDetails ||
                            configDetails.approval_status === "pending"
                          }
                          className="gap-2"
                        >
                          {isDeniedApprovalStatus(
                            configDetails?.approval_status,
                          ) ? (
                            <ShieldCheck className="h-4 w-4" />
                          ) : (
                            <ShieldX className="h-4 w-4" />
                          )}
                          {isDeniedApprovalStatus(
                            configDetails?.approval_status,
                          )
                            ? t("detail.overview.buttons.allow", {
                              defaultValue: "Allow",
                            })
                            : t("detail.overview.buttons.deny", {
                              defaultValue: "Deny",
                            })}
                        </Button>
                        {isAttachmentApplicable ? (
                          <Button
                            variant="outline"
                            size="sm"
                            onClick={() =>
                              setAttachmentActionConfirm(
                                isAttachedClient ? "detach" : "attach",
                              )
                            }
                            disabled={
                              !canWriteClientConfig ||
                              clientWritebackMutationPending
                            }
                            className="gap-2"
                          >
                            {isAttachedClient ? (
                              <Unlink className="h-4 w-4" />
                            ) : (
                              <Link2 className="h-4 w-4" />
                            )}
                            {isAttachedClient
                              ? t("detail.overview.buttons.detach", {
                                defaultValue: "Detach",
                              })
                              : t("detail.overview.buttons.attach", {
                                defaultValue: "Attach",
                              })}
                          </Button>
                        ) : null}
                        {supportedTransportOptions.length > 0 ? (
                          <ButtonGroupText className="h-9 p-0 select-none shadow-none">
                            <Select
                              value={transport}
                              onValueChange={async (v) => {
                                if (!identifier) return;
                                const previousTransport = transport;
                                setTransport(v);
                                try {
                                  const selectedTransport =
                                    isSupportedTransportOption(v) ? v : "auto";
                                  await clientsApi.update({
                                    identifier,
                                    transport: selectedTransport,
                                    transports: withSelectedTransport(
                                      configDetails?.transports,
                                      selectedTransport,
                                    ),
                                  });

                                  let configApplied = false;
                                  const shouldApplyManagedConfig =
                                    (mode === "hosted" || mode === "unify") &&
                                    canSyncManagedConfig;
                                  if (shouldApplyManagedConfig) {
                                    try {
                                      const capabilityData =
                                        (await clientsApi.getCapabilityConfig(
                                          identifier,
                                        )) ?? effectiveCapabilityConfig;
                                      if (!capabilityData) {
                                        throw new Error(
                                          t(
                                            "detail.configuration.errors.capabilityConfigMissing",
                                            {
                                              defaultValue:
                                                "Capability configuration update returned no data. Please try again.",
                                            },
                                          ),
                                        );
                                      }

                                      await applyClientConfigWithResolvedSelection(
                                        {
                                          identifier,
                                          mode,
                                          backupPolicy:
                                            mapDashboardSettingsToClientBackupPolicy(
                                              dashboardSettings,
                                            ),
                                          capabilityData,
                                        },
                                      );
                                      configApplied = true;
                                    } catch (err) {
                                      notifyError(
                                        t(
                                          "detail.notifications.applyFailed.title",
                                          {
                                            defaultValue: "Apply failed",
                                          },
                                        ),
                                        resolveClientConfigSyncErrorMessage(
                                          err,
                                          t,
                                        ),
                                      );
                                    }
                                  }

                                  notifySuccess(
                                    t("detail.overview.transport.updated", {
                                      defaultValue: "Transport updated",
                                    }),
                                    configApplied
                                      ? t(
                                        "detail.notifications.applied.message",
                                        {
                                          defaultValue:
                                            "Configuration applied",
                                        },
                                      )
                                      : "",
                                  );
                                  await refreshClientCapabilityState();
                                } catch (err) {
                                  setTransport(previousTransport);
                                  notifyError(
                                    t(
                                      "detail.overview.transport.updateFailed",
                                      { defaultValue: "Update failed" },
                                    ),
                                    resolveClientConfigSyncErrorMessage(err, t),
                                  );
                                }
                              }}
                            >
                              <TooltipProvider delayDuration={200}>
                                <Tooltip>
                                  <TooltipTrigger asChild>
                                    <SelectTrigger
                                      className="h-8 w-auto border-0 shadow-none focus:ring-0 focus:ring-offset-0 focus-visible:ring-0 focus:outline-none select-none bg-transparent px-3"
                                      aria-label={t(
                                        "detail.overview.transport.selectorAria",
                                        {
                                          defaultValue: "Transport selector",
                                        },
                                      )}
                                    >
                                      <SelectValue />
                                    </SelectTrigger>
                                  </TooltipTrigger>
                                  <TooltipContent side="top" align="center" className="max-w-xs">
                                    {t("detail.overview.transport.tooltip", {
                                      defaultValue:
                                        "Quickly choose the runtime transport used to connect this client.",
                                    })}
                                  </TooltipContent>
                                </Tooltip>
                              </TooltipProvider>
                              <SelectContent align="end">
                                <SelectItem value="auto">
                                  {t(
                                    "detail.configuration.transportOptions.auto",
                                    {
                                      defaultValue: "Auto",
                                    },
                                  )}
                                </SelectItem>
                                {supportedTransportOptions.map((v) => (
                                  <SelectItem key={v} value={v}>
                                    {getTransportOptionLabel(v, t)}
                                  </SelectItem>
                                ))}
                              </SelectContent>
                            </Select>
                          </ButtonGroupText>
                        ) : null}
                      </ButtonGroup>
                    </div>
                  </div>
                </CardContent>
              ) : (
                <CardContent className="text-sm text-slate-500">
                  {t("detail.overview.noDetails", {
                    defaultValue: "No details available",
                  })}
                </CardContent>
              )}
            </Card>
            <Card className={DETAIL_OVERVIEW_PINNED_SECTION_CLASS}>
              <CardHeader>
                <div className="flex items-center justify-between">
                  <CardTitle>
                    {t("detail.overview.currentServers.title", {
                      defaultValue: "Current Servers",
                    })}
                  </CardTitle>
                  <div className="flex items-center gap-2">
                    <Button
                      size="sm"
                      variant="outline"
                      onClick={() => setImportReviewOpen(true)}
                      disabled={configDetails?.writable_config === false}
                    >
                      <Download className="mr-2 h-4 w-4" />{" "}
                      {t("detail.overview.currentServers.import", {
                        defaultValue: "Import from Config",
                      })}
                    </Button>
                  </div>
                </div>
              </CardHeader>
              <CardContent className={DETAIL_OVERVIEW_SCROLLABLE_LIST_CLASS}>
                {loadingConfig ? (
                  <div className="space-y-2">
                    {[1, 2, 3].map((i) => (
                      <div
                        key={i}
                        className="h-8 bg-slate-200 dark:bg-slate-800 animate-pulse rounded-[10px]"
                      />
                    ))}
                  </div>
                ) : currentServerEntries.length ? (
                  <CapsuleStripeList>
                    {currentServerEntries.map((entry) => (
                      <CapsuleStripeListItem key={entry.name}>
                        <div className="font-mono">{entry.name}</div>
                        <ServerEntryTransportBadge entry={entry} />
                      </CapsuleStripeListItem>
                    ))}
                  </CapsuleStripeList>
                ) : (
                  <div className="text-sm text-slate-500">
                    {t("detail.overview.currentServers.empty", {
                      defaultValue: "No servers extracted from current config.",
                    })}
                  </div>
                )}
              </CardContent>
            </Card>
            {showClientLiveLogs ? (
              <AuditLogsPanel
                fillHeight
                title={t("detail.logs.title", { defaultValue: "Logs" })}
                description={t("detail.logs.description", {
                  defaultValue:
                    "Runtime warnings and backend notes for this client.",
                })}
                searchPlaceholder={t("detail.logs.searchPlaceholder", {
                  defaultValue: "Search logs...",
                })}
                refreshLabel={t("detail.logs.refresh", {
                  defaultValue: "Refresh Logs",
                })}
                loadingLabel={t("detail.logs.loading", {
                  defaultValue: "Loading logs...",
                })}
                emptyLabel={t("detail.logs.empty", {
                  defaultValue: "No log entries recorded for this client yet.",
                })}
                headers={{
                  timestamp: t("detail.logs.headers.timestamp", {
                    defaultValue: "Timestamp",
                  }),
                  action: t("detail.logs.headers.action", {
                    defaultValue: "Action",
                  }),
                  category: t("detail.logs.headers.category", {
                    defaultValue: "Category",
                  }),
                  status: t("detail.logs.headers.status", {
                    defaultValue: "Status",
                  }),
                  target: t("detail.logs.headers.target", {
                    defaultValue: "Target",
                  }),
                }}
                searchValue={logFilter}
                onSearchChange={setLogFilter}
                onRefresh={() => void logsQuery.refetch()}
                rows={filteredLogs}
                isLoading={logsQuery.isLoading}
                isFetching={logsQuery.isFetching}
                isPaginationActionLoading={isLogPaginationActionLoading}
                currentPage={logCurrentPageIndex + 1}
                hasPreviousPage={logCurrentPageIndex > 0}
                hasNextPage={Boolean(logsQuery.data?.next_cursor)}
                itemsPerPage={logPageSize}
                onItemsPerPageChange={setLogPageSize}
                onPreviousPage={handleLogsPrevPage}
                onFirstPage={handleLogsFirstPage}
                onNextPage={handleLogsNextPage}
                onLastPage={() => void handleLogsLastPage()}
                expandLabel={t("detail.logs.expand", {
                  defaultValue: "Expand Logs",
                })}
                collapseLabel={t("detail.logs.collapse", {
                  defaultValue: "Collapse Logs",
                })}
              />
            ) : null}
          </div>
        </TabsContent>

        <TabsContent value="configuration" className={DETAIL_TAB_CONTENT_CLASS}>
          <div className="min-h-0 flex-1 overflow-y-auto">
            <div className="grid gap-4">
              <Card>
                <CardHeader className="space-y-0">
                  <div className="flex items-start justify-between gap-4">
                    <div className="min-w-0 flex-1 space-y-1.5">
                      <CardTitle>
                        {t("detail.configuration.title", {
                          defaultValue: "Configuration Mode",
                        })}
                      </CardTitle>
                      <CardDescription>
                        {t("detail.configuration.description", {
                          defaultValue:
                            "If you don't understand what this means, please don't make any changes and keep the current settings.",
                        })}
                      </CardDescription>
                    </div>
                    <Button
                      size="sm"
                      variant="default"
                      onClick={() => applyMutation.mutate({ preview: false })}
                      disabled={
                        loadingConfig ||
                        clientWritebackMutationPending ||
                        !canSaveManagementSettings ||
                        (shouldRequireLocalConfigWrite &&
                          !canApplyTransparentConfig)
                      }
                      className="shrink-0 gap-2"
                    >
                      <HardDrive
                        className={`h-4 w-4 ${applyMutation.isPending ? "animate-pulse" : ""}`}
                      />
                      {t(
                        isAttachmentApplicable && isAttachedClient
                          ? "detail.configuration.reapply"
                          : "detail.configuration.apply",
                        {
                          defaultValue:
                            isAttachmentApplicable && isAttachedClient
                              ? "Re-Apply"
                              : "Apply",
                        },
                      )}
                    </Button>
                  </div>
                </CardHeader>
                <CardContent className="pt-0">
                  <div className="grid min-w-0 grid-cols-1 gap-8 lg:grid-cols-2 lg:items-start">
                    <div className="min-w-0 space-y-6">
                      {/* Mode Selection */}
                      <div className="min-w-0 space-y-3">
                        <div className="space-y-1">
                          <h4 className="text-sm font-medium text-slate-700 dark:text-slate-300">
                            {t("detail.configuration.sections.mode.title", {
                              defaultValue: "1. Management Mode",
                            })}
                          </h4>
                          <p className="text-xs text-slate-500 leading-relaxed">
                            {mode === "hosted" &&
                              t(
                                "detail.configuration.sections.mode.descriptions.hosted",
                                {
                                  defaultValue:
                                    "Hosted keeps a durable managed configuration for this client and remembers the selected working state.",
                                },
                              )}
                            {mode === "unify" &&
                              t(
                                "detail.configuration.sections.mode.descriptions.unify",
                                {
                                  defaultValue:
                                    "Unify starts with builtin control-plane tools only and keeps its workspace inside the current MCP session.",
                                },
                              )}
                            {mode === "transparent" &&
                              t(
                                "detail.configuration.sections.mode.descriptions.transparent",
                                {
                                  defaultValue:
                                    "MCPMate writes the selected profile servers directly into this client's MCP configuration and does not preserve capability-level controls.",
                                },
                              )}
                          </p>
                        </div>
                        <Segment
                          value={mode}
                          onValueChange={(v) => setMode(v as ClientConfigMode)}
                          options={managementModeSegmentOptions}
                          showDots={true}
                          className="w-full"
                        />
                      </div>

                      {/* Source Selection */}
                      <div className="min-w-0 space-y-3">
                        <div className="space-y-1">
                          <h4 className="text-sm font-medium text-slate-700 dark:text-slate-300">
                            {t("detail.configuration.sections.source.title", {
                              defaultValue: "2. Configuration",
                            })}
                          </h4>
                          <p className="text-xs text-slate-500 leading-relaxed">
                            {mode === "unify" &&
                              unifyRouteMode === "broker_only" &&
                              t(
                                "detail.configuration.sections.source.descriptions.unify_broker_only",
                                {
                                  defaultValue:
                                    "All capabilities are kept behind the UCAN broker catalog. Builtin MCP tools will browse and call capabilities from globally enabled servers.",
                                },
                              )}
                            {mode === "unify" &&
                              unifyRouteMode === "server_level" &&
                              t(
                                "detail.configuration.sections.source.descriptions.unify_server_level",
                                {
                                  defaultValue:
                                    "Directly expose all capabilities from selected eligible servers to this client. Exposed capabilities are excluded from the UCAN catalog.",
                                },
                              )}
                            {mode === "unify" &&
                              unifyRouteMode === "capability_level" &&
                              t(
                                "detail.configuration.sections.source.descriptions.unify_capability_level",
                                {
                                  defaultValue:
                                    "Directly expose only selected capabilities (tools, prompts, resources, templates) from eligible servers to this client. (Advanced)",
                                },
                              )}
                            {mode === "transparent" &&
                              selectedConfig === "default" &&
                              t(
                                "detail.configuration.sections.source.descriptions.transparentDefault",
                                {
                                  defaultValue:
                                    "Write the servers from all currently activated profiles directly into this client's MCP configuration.",
                                },
                              )}
                            {mode === "transparent" &&
                              selectedConfig === "profile" &&
                              t(
                                "detail.configuration.sections.source.descriptions.transparentProfile",
                                {
                                  defaultValue:
                                    "Write the servers from the selected shared profiles directly into this client's MCP configuration.",
                                },
                              )}
                            {mode === "transparent" &&
                              selectedConfig === "custom" &&
                              t(
                                "detail.configuration.sections.source.descriptions.transparentCustom",
                                {
                                  defaultValue:
                                    "Write the servers from the client-specific custom profile directly into this client's MCP configuration.",
                                },
                              )}
                            {mode === "hosted" &&
                              selectedConfig === "default" &&
                              t(
                                "detail.configuration.sections.source.descriptions.default",
                                {
                                  defaultValue:
                                    "Review the profiles that are currently active for this client runtime.",
                                },
                              )}
                            {mode === "hosted" &&
                              selectedConfig === "profile" &&
                              t(
                                "detail.configuration.sections.source.descriptions.profile",
                                {
                                  defaultValue:
                                    "Browse the shared scene library and choose the exact working set for this client.",
                                },
                              )}
                            {mode === "hosted" &&
                              selectedConfig === "custom" &&
                              t(
                                "detail.configuration.sections.source.descriptions.custom",
                                {
                                  defaultValue:
                                    "Create client-specific adjustments on top of the current unify-mode working state.",
                                },
                              )}
                          </p>
                        </div>
                        {mode === "unify" ? (
                          <Segment
                            value={unifyRouteMode}
                            onValueChange={(v) => {
                              setHasUnifyDraftChanges(true);
                              setUnifyRouteMode(v as UnifyRouteMode);
                            }}
                            options={unifyRouteModeSegmentOptions}
                            showDots={true}
                            className="w-full"
                          />
                        ) : (
                          <Segment
                            value={selectedConfig}
                            onValueChange={(v) =>
                              setSelectedConfig(
                                v as ClientCapabilitySourceSelection,
                              )
                            }
                            options={configurationSourceSegmentOptions}
                            showDots={true}
                            className="w-full"
                          />
                        )}
                      </div>

                    </div>

                    {/* Direct exposure / profiles */}
                    {(mode === "unify" ||
                      mode === "hosted" ||
                      mode === "transparent") && (
                        <div className="min-w-0">
                          <div className="mb-3">
                            <h4 className="text-sm font-medium text-slate-700 dark:text-slate-300">
                              {mode === "unify"
                                ? t(
                                  "detail.configuration.sections.exposure.title",
                                  {
                                    defaultValue: "3. Direct Exposure",
                                  },
                                )
                                : t(
                                  "detail.configuration.sections.profiles.title",
                                  {
                                    defaultValue: "3. Profiles",
                                  },
                                )}
                            </h4>
                            {mode === "unify" &&
                              unifyRouteMode === "broker_only" && (
                                <p className="text-xs text-slate-500 mt-1 leading-relaxed">
                                  {t(
                                    "detail.configuration.sections.exposure.descriptions.broker_only",
                                    {
                                      defaultValue:
                                        "All enabled MCP servers, including servers marked for direct exposure, remain reachable through the builtin UCAN tools in Broker Only mode.",
                                    },
                                  )}
                                </p>
                              )}
                            {mode === "unify" &&
                              unifyRouteMode === "server_level" && (
                                <p className="text-xs text-slate-500 mt-1 leading-relaxed">
                                  {t(
                                    "detail.configuration.sections.exposure.descriptions.server_level",
                                    {
                                      defaultValue:
                                        "Select the eligible servers whose tools, prompts, resources, and resource templates should be exposed directly to the client.",
                                    },
                                  )}
                                </p>
                              )}
                            {mode === "unify" &&
                              unifyRouteMode === "capability_level" && (
                                <p className="text-xs text-slate-500 mt-1 leading-relaxed">
                                  {t(
                                    "detail.configuration.sections.exposure.descriptions.capability_level",
                                    {
                                      defaultValue:
                                        "Toggle each eligible server on the left, then use the more button to configure direct exposure at capability level across tools, prompts, resources, and templates. (Advanced)",
                                    },
                                  )}
                                </p>
                              )}
                            {/* Dynamic description based on source */}
                            {mode !== "unify" && selectedConfig === "default" && (
                              <p className="text-xs text-slate-500 mt-1 leading-relaxed">
                                {mode === "transparent"
                                  ? t(
                                    "detail.configuration.sections.profiles.descriptions.transparentDefault",
                                    {
                                      defaultValue:
                                        "Transparent mode will write the enabled servers from all currently activated profiles directly into this client's MCP configuration.",
                                    },
                                  )
                                  : t(
                                    "detail.configuration.sections.profiles.descriptions.default",
                                    {
                                      defaultValue:
                                        "Review the profiles that are already active for this client runtime. This view is read-only to keep the active scene set consistent.",
                                    },
                                  )}
                              </p>
                            )}
                            {mode !== "unify" && selectedConfig === "profile" && (
                              <p className="text-xs text-slate-500 mt-1 leading-relaxed">
                                {mode === "transparent"
                                  ? t(
                                    "detail.configuration.sections.profiles.descriptions.transparentProfile",
                                    {
                                      defaultValue:
                                        "Select which shared profiles contribute enabled servers to this client's MCP configuration in transparent mode.",
                                    },
                                  )
                                  : t(
                                    "detail.configuration.sections.profiles.descriptions.profile",
                                    {
                                      defaultValue:
                                        "Choose the reusable shared profiles that define this client's working set.",
                                    },
                                  )}
                              </p>
                            )}
                            {mode !== "unify" && selectedConfig === "custom" && (
                              <p className="text-xs text-slate-500 mt-1 leading-relaxed">
                                {mode === "transparent"
                                  ? t(
                                    "detail.configuration.sections.profiles.descriptions.transparentCustom",
                                    {
                                      defaultValue:
                                        "Transparent mode uses only the enabled servers from this client-specific custom profile when writing the MCP configuration.",
                                    },
                                  )
                                  : t(
                                    "detail.configuration.sections.profiles.descriptions.custom",
                                    {
                                      defaultValue:
                                        "Create and maintain client-specific overrides for the current working state.",
                                    },
                                  )}
                              </p>
                            )}
                          </div>

                          {loadingProfiles ? (
                            <div className="space-y-2">
                              {[1, 2, 3].map((i) => (
                                <div
                                  key={i}
                                  className="h-12 bg-slate-200 dark:bg-slate-800 animate-pulse rounded"
                                />
                              ))}
                            </div>
                          ) : (
                            <CapsuleStripeList>
                              {mode === "unify" ? (
                                unifyRouteMode === "broker_only" ? (
                                  <CapsuleStripeListItem className="items-start">
                                    <div className="w-full text-xs leading-relaxed text-slate-500 dark:text-slate-400">
                                      {t(
                                        "detail.configuration.sections.exposure.labels.ucanRoutingDescription",
                                        {
                                          defaultValue:
                                            "In Broker Only mode, all enabled MCP servers — including servers marked for direct exposure — are still accessed through the UCAN catalog and call tools.",
                                        },
                                      )}
                                    </div>
                                  </CapsuleStripeListItem>
                                ) : eligibleServers.length > 0 ? (
                                  eligibleServers.map((server) => {
                                    const isSelected =
                                      unifySelectedServers.includes(server.id);
                                    const toolSurfaces = getUnifyServerSurfaces(
                                      effectiveCapabilityConfig
                                        ?.unify_direct_exposure
                                        ?.resolved_capabilities
                                        ?.selected_tool_surfaces ?? [],
                                      server.id,
                                    );
                                    const promptSurfaces = getUnifyServerSurfaces(
                                      effectiveCapabilityConfig
                                        ?.unify_direct_exposure
                                        ?.resolved_capabilities
                                        ?.selected_prompt_surfaces ?? [],
                                      server.id,
                                    );
                                    const resourceSurfaces =
                                      getUnifyServerSurfaces(
                                        effectiveCapabilityConfig
                                          ?.unify_direct_exposure
                                          ?.resolved_capabilities
                                          ?.selected_resource_surfaces ?? [],
                                        server.id,
                                      );
                                    const templateSurfaces =
                                      getUnifyServerSurfaces(
                                        effectiveCapabilityConfig
                                          ?.unify_direct_exposure
                                          ?.resolved_capabilities
                                          ?.selected_template_surfaces ?? [],
                                        server.id,
                                      );
                                    const selectedCapabilityCount =
                                      toolSurfaces.length +
                                      promptSurfaces.length +
                                      resourceSurfaces.length +
                                      templateSurfaces.length;
                                    const directPath =
                                      getClientDirectCapabilitiesPath(
                                        identifier,
                                        server.id,
                                      );
                                    const isMixed = isUnifyServerMixedRouting(
                                      unifyRouteMode,
                                      toolSurfaces.length,
                                      server.capability?.tools.currentAvailable
                                        ? server.capability.tools.currentCount
                                        : undefined,
                                    );
                                    const showDirectSelection =
                                      unifyRouteMode === "capability_level"
                                        ? selectedCapabilityCount > 0
                                        : isSelected;
                                    const serverDescription =
                                      server.meta?.description ||
                                      t(
                                        "detail.configuration.labels.noDescription",
                                        {
                                          defaultValue: "No description",
                                        },
                                      );

                                    return (
                                      <CapsuleStripeListItem
                                        key={server.id}
                                        interactive={true}
                                        className={`group relative transition-colors ${showDirectSelection ? "bg-primary/10 ring-1 ring-primary/40" : ""}`}
                                        onClick={() => {
                                          if (
                                            unifyRouteMode === "capability_level"
                                          ) {
                                            directExposureServerMutation.mutate({
                                              server,
                                              routeMode: "capability_level",
                                            });
                                          } else if (
                                            unifyRouteMode === "server_level"
                                          ) {
                                            if (hasUnifyDraftChanges) {
                                              setUnifySelectedServers((prev) =>
                                                toggleSelectedServerIds(
                                                  prev,
                                                  server.id,
                                                ),
                                              );
                                            } else {
                                              directExposureServerMutation.mutate(
                                                {
                                                  server,
                                                  routeMode: "server_level",
                                                },
                                              );
                                            }
                                          }
                                        }}
                                      >
                                        <CapsuleStripeRowBody
                                          lead={
                                            <CapsuleStripeLeadCircle
                                              variant="toggle"
                                              selected={showDirectSelection}
                                            />
                                          }
                                          trailing={
                                            unifyRouteMode ===
                                              "capability_level" ? (
                                              <Button
                                                variant="ghost"
                                                size="icon"
                                                className="h-8 w-8 text-slate-400 hover:text-slate-900 dark:hover:text-slate-100"
                                                onClick={(e) => {
                                                  e.stopPropagation();
                                                  if (directPath) {
                                                    navigate(directPath);
                                                  }
                                                }}
                                              >
                                                <MoreVertical className="h-4 w-4" />
                                              </Button>
                                            ) : undefined
                                          }
                                        >
                                          <div className="font-medium text-sm truncate">
                                            {server.name}
                                            {unifyRouteMode ===
                                              "capability_level" &&
                                              selectedCapabilityCount > 0 && (
                                                <span className="ml-2 text-xs font-normal text-slate-500 bg-slate-100 dark:bg-slate-800 px-1.5 py-0.5 rounded-md">
                                                  {selectedCapabilityCount}{" "}
                                                  {t(
                                                    "detail.configuration.labels.capabilitiesExposed",
                                                    {
                                                      defaultValue:
                                                        "capabilities exposed",
                                                    },
                                                  )}
                                                </span>
                                              )}
                                          </div>
                                          <div className="text-xs text-slate-500 truncate">
                                            {serverDescription}
                                          </div>
                                          {renderUnifyEligibleServerCapabilitySummary(
                                            server,
                                            unifyRouteMode,
                                            toolSurfaces.length,
                                          )}
                                          {isMixed && (
                                            <div className="mt-1 flex items-center gap-1 text-[11px] font-medium text-amber-600 dark:text-amber-500">
                                              <Info className="h-3 w-3" />
                                              {t(
                                                "detail.configuration.warnings.mixedRouting",
                                                {
                                                  defaultValue:
                                                    "Mixed routing: splitting stateful workflows may cause issues.",
                                                },
                                              )}
                                            </div>
                                          )}
                                        </CapsuleStripeRowBody>
                                      </CapsuleStripeListItem>
                                    );
                                  })
                                ) : (
                                  <CapsuleStripeListItem>
                                    <div className="text-sm text-slate-500 py-4 text-center w-full">
                                      {t(
                                        "detail.configuration.sections.exposure.empty.no_eligible",
                                        {
                                          defaultValue:
                                            "No eligible servers found. Enable Unify Direct Exposure on a server first.",
                                        },
                                      )}
                                    </div>
                                  </CapsuleStripeListItem>
                                )
                              ) : selectedConfig === "default" ? (
                                // Show active profiles for default source
                                activeProfiles.length > 0 ? (
                                  activeProfiles.map((profile) => {
                                    const capabilities = profileCapabilities.get(
                                      profile.id,
                                    );
                                    return (
                                      <CapsuleStripeListItem
                                        key={profile.id}
                                        className="cursor-default"
                                      >
                                        <CapsuleStripeRowBody
                                          lead={
                                            <CapsuleStripeLeadCircle variant="readOnlyActive" />
                                          }
                                          trailing={getConfigurationProfileTokenSlot(
                                            profile,
                                            false,
                                          )}
                                        >
                                          <div className="font-medium text-sm truncate">
                                            {profile.name}
                                          </div>
                                          <div className="text-xs text-slate-500 truncate">
                                            {profile.description ||
                                              t(
                                                "detail.configuration.labels.noDescription",
                                                {
                                                  defaultValue: "No description",
                                                },
                                              )}
                                          </div>
                                          {capabilities &&
                                            renderProfileCapabilitySummary(
                                              capabilities,
                                            )}
                                        </CapsuleStripeRowBody>
                                      </CapsuleStripeListItem>
                                    );
                                  })
                                ) : (
                                  <CapsuleStripeListItem>
                                    <div className="text-sm text-slate-500 py-4 text-center w-full">
                                      {t(
                                        "detail.configuration.sections.profiles.empty.active",
                                        {
                                          defaultValue:
                                            "No active profiles found",
                                        },
                                      )}
                                    </div>
                                  </CapsuleStripeListItem>
                                )
                              ) : selectedConfig === "profile" ? (
                                // Show shared profiles for profile source
                                sharedProfiles.length > 0 ? (
                                  sharedProfiles.map((profile) => {
                                    const capabilities = profileCapabilities.get(
                                      profile.id,
                                    );
                                    const isSelected = selectedProfiles.includes(
                                      profile.id,
                                    );
                                    return (
                                      <CapsuleStripeListItem
                                        key={profile.id}
                                        interactive={
                                          unifyRouteMode !== "broker_only"
                                        }
                                        className={`group relative transition-colors ${isSelected
                                          ? "bg-primary/10 ring-1 ring-primary/40"
                                          : ""
                                          }`}
                                        onClick={() => {
                                          setSelectedProfiles((prev) =>
                                            prev.includes(profile.id)
                                              ? prev.filter(
                                                (id) => id !== profile.id,
                                              )
                                              : [...prev, profile.id],
                                          );
                                        }}
                                      >
                                        <CapsuleStripeRowBody
                                          lead={
                                            <CapsuleStripeLeadCircle
                                              variant="toggle"
                                              selected={isSelected}
                                            />
                                          }
                                          trailing={getConfigurationProfileTokenSlot(
                                            profile,
                                            true,
                                          )}
                                        >
                                          <div className="font-medium text-sm truncate">
                                            {profile.name}
                                          </div>
                                          <div className="text-xs text-slate-500 truncate">
                                            {profile.description ||
                                              t(
                                                "detail.configuration.labels.noDescription",
                                                {
                                                  defaultValue: "No description",
                                                },
                                              )}
                                          </div>
                                          {capabilities &&
                                            renderProfileCapabilitySummary(
                                              capabilities,
                                            )}
                                        </CapsuleStripeRowBody>
                                      </CapsuleStripeListItem>
                                    );
                                  })
                                ) : (
                                  <CapsuleStripeListItem>
                                    <div className="text-sm text-slate-500 py-4 text-center w-full">
                                      {t(
                                        "detail.configuration.sections.profiles.empty.shared",
                                        {
                                          defaultValue:
                                            "No shared profiles found",
                                        },
                                      )}
                                    </div>
                                  </CapsuleStripeListItem>
                                )
                              ) : null}

                              {mode !== "unify" && (
                                <CapsuleStripeListItem
                                  interactive={!clientWritebackMutationPending}
                                  className={
                                    selectedConfig === "custom" && customProfileId
                                      ? "border-slate-300 dark:border-slate-600 hover:border-slate-400 dark:hover:border-slate-500"
                                      : "border-dashed border-slate-300 dark:border-slate-600 hover:border-slate-400 dark:hover:border-slate-500"
                                  }
                                  onClick={() => {
                                    if (clientWritebackMutationPending) return;
                                    if (selectedConfig === "custom") {
                                      if (customProfileId) {
                                        navigate(
                                          `/profiles/${customProfileId}?mode=custom`,
                                        );
                                      } else {
                                        applyMutation.mutate({ preview: false });
                                      }
                                    } else {
                                      navigate("/profiles");
                                    }
                                  }}
                                >
                                  <CapsuleStripeRowBody
                                    lead={
                                      <CapsuleStripeLeadCircle
                                        variant="ghost"
                                        hasProfile={
                                          selectedConfig === "custom" &&
                                          !!customProfileId
                                        }
                                      />
                                    }
                                  >
                                    <div className="font-medium text-sm truncate text-slate-700 dark:text-slate-300">
                                      {selectedConfig === "custom"
                                        ? t(
                                          "detail.configuration.sections.profiles.ghost.titleCustom",
                                          {
                                            defaultValue: customProfileId
                                              ? "Customize current state"
                                              : "Create custom workspace",
                                          },
                                        )
                                        : t(
                                          "detail.configuration.sections.profiles.ghost.titleDefault",
                                          {
                                            defaultValue:
                                              "Open profiles library",
                                          },
                                        )}
                                    </div>
                                    <div className="text-xs text-slate-400 dark:text-slate-600 truncate">
                                      {selectedConfig === "custom"
                                        ? t(
                                          mode === "transparent"
                                            ? "detail.configuration.sections.profiles.ghost.subtitleCustomTransparent"
                                            : "detail.configuration.sections.profiles.ghost.subtitleCustom",
                                          {
                                            defaultValue: customProfileId
                                              ? "Adjust client-specific capabilities on top of the current workspace"
                                              : "Create client-specific overrides for this workspace",
                                          },
                                        )
                                        : t(
                                          "detail.configuration.sections.profiles.ghost.subtitleDefault",
                                          {
                                            defaultValue:
                                              "Browse reusable shared scenes and edit them from the profiles page",
                                          },
                                        )}
                                    </div>
                                    {customProfileCapabilities
                                      ? renderProfileCapabilitySummary(
                                        customProfileCapabilities,
                                      )
                                      : null}
                                  </CapsuleStripeRowBody>
                                </CapsuleStripeListItem>
                              )}
                            </CapsuleStripeList>
                          )}
                        </div>
                      )}
                  </div>
                </CardContent>
              </Card>
            </div>
          </div>
        </TabsContent>

        <TabsContent
          value="backups"
          className="mt-0 flex min-h-0 flex-1 flex-col overflow-y-auto data-[state=inactive]:hidden"
        >
          <Card>
            <CardHeader className="flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
              <div>
                <CardTitle>
                  {t("detail.backups.title", { defaultValue: "Backups" })}
                </CardTitle>
                <CardDescription>
                  {t("detail.backups.description", {
                    defaultValue: "Restore or delete configuration snapshots.",
                  })}
                </CardDescription>
              </div>
              <div className="flex flex-wrap items-center gap-2">
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => refetchBackups()}
                  disabled={loadingBackups}
                >
                  <RefreshCw
                    className={`mr-2 h-4 w-4 ${loadingBackups ? "animate-spin" : ""}`}
                  />
                  {t("detail.backups.buttons.refresh", {
                    defaultValue: "Refresh",
                  })}
                </Button>
                {!loadingBackups && visibleBackups.length > 0 && (
                  <ButtonGroup>
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() =>
                        setSelectedBackups(visibleBackups.map((b) => b.backup))
                      }
                    >
                      {t("detail.backups.buttons.selectAll", {
                        defaultValue: "Select all",
                      })}
                    </Button>
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => setSelectedBackups([])}
                    >
                      {t("detail.backups.buttons.clear", {
                        defaultValue: "Clear",
                      })}
                    </Button>
                    <Button
                      size="sm"
                      variant="destructive"
                      disabled={
                        bulkDeleteMutation.isPending ||
                        selectedBackups.length === 0
                      }
                      onClick={() => setBulkConfirmOpen(true)}
                    >
                      <Trash2 className="mr-2 h-4 w-4" />
                      {t("detail.backups.buttons.deleteSelected", {
                        defaultValue: "Delete selected ({{count}})",
                        count: selectedBackups.length,
                      })}
                    </Button>
                  </ButtonGroup>
                )}
              </div>
            </CardHeader>
            <CardContent className="space-y-4 pt-0">
              {loadingBackups ? (
                <div className="space-y-2">
                  {[1, 2, 3].map((i) => (
                    <div
                      key={i}
                      className="h-10 bg-slate-200 dark:bg-slate-800 animate-pulse rounded"
                    />
                  ))}
                </div>
              ) : visibleBackups.length ? (
                <CapsuleStripeList>
                  {visibleBackups.map((b) => {
                    const selected = selectedBackups.includes(b.backup);
                    return (
                      <CapsuleStripeListItem
                        key={b.path}
                        interactive
                        className={`group relative transition-colors ${selected ? "bg-primary/10 ring-1 ring-primary/40" : ""}`}
                        onClick={() =>
                          setSelectedBackups((prev) =>
                            prev.includes(b.backup)
                              ? prev.filter((x) => x !== b.backup)
                              : [...prev, b.backup],
                          )
                        }
                      >
                        <div className="flex items-center justify-between flex-1">
                          <div className="flex items-center gap-3">
                            <div
                              className={`flex h-6 w-6 items-center justify-center rounded-full border text-[0px] transition-all duration-200 ${selected
                                ? "border-primary bg-primary text-white shadow-sm"
                                : "border-slate-300 text-transparent group-hover:border-primary/50 group-hover:text-primary/60 dark:border-slate-700 dark:group-hover:border-primary/50"
                                }`}
                            >
                              <Check className="h-3 w-3" />
                            </div>
                            <div
                              className={`font-mono transition-colors duration-200 ${selected
                                ? "text-primary"
                                : "text-slate-700 dark:text-slate-200"
                                }`}
                            >
                              {b.backup}
                            </div>
                          </div>
                          <div className="flex items-center justify-end gap-4">
                            <div
                              className={`flex items-center gap-4 text-slate-500 transition-all duration-200 ${selected ? "text-primary" : ""
                                }`}
                            >
                              <div>{formatBackupTime(b.created_at)}</div>
                              <div>{(b.size / 1024).toFixed(1)} KB</div>
                            </div>
                            <div className="flex items-center gap-2 overflow-hidden transition-all duration-200 opacity-0 max-w-0 pointer-events-none group-hover:max-w-[12rem] group-hover:opacity-100 group-hover:pointer-events-auto group-focus-within:max-w-[12rem] group-focus-within:opacity-100 group-focus-within:pointer-events-auto">
                              <Button
                                size="sm"
                                variant="outline"
                                onClick={(e) => {
                                  e.stopPropagation();
                                  setConfirm({
                                    kind: "restore",
                                    backup: b.backup,
                                  });
                                }}
                              >
                                <RotateCcw className="mr-2 h-4 w-4" />
                                {t("detail.backups.buttons.restore", {
                                  defaultValue: "Restore",
                                })}
                              </Button>
                              <Button
                                size="icon"
                                variant="outline"
                                onClick={(e) => {
                                  e.stopPropagation();
                                  setConfirm({
                                    kind: "delete",
                                    backup: b.backup,
                                  });
                                }}
                              >
                                <Trash2 className="h-4 w-4" />
                              </Button>
                            </div>
                          </div>
                        </div>
                      </CapsuleStripeListItem>
                    );
                  })}
                </CapsuleStripeList>
              ) : (
                <div className="text-slate-500 text-sm">
                  {backupsDisabledByPolicy
                    ? t("detail.backups.emptyDisabledByPolicy", {
                      defaultValue:
                        "Backups are currently disabled by system policy.",
                    })
                    : t("detail.backups.empty", {
                      defaultValue: "No backups.",
                    })}
                </div>
              )}
            </CardContent>
          </Card>
        </TabsContent>
      </Tabs>

      <ConfirmDialog
        isOpen={!!confirm}
        onClose={() => setConfirm(null)}
        title={
          confirm?.kind === "delete"
            ? t("detail.confirm.deleteTitle", { defaultValue: "Delete Backup" })
            : t("detail.confirm.restoreTitle", {
              defaultValue: "Restore Backup",
            })
        }
        description={
          confirm?.kind === "delete"
            ? t("detail.confirm.deleteDescription", {
              defaultValue:
                "Are you sure you want to delete this backup? This action cannot be undone.",
            })
            : t("detail.confirm.restoreDescription", {
              defaultValue:
                "Restore the local client configuration file from the selected backup? MCPMate management mode and capability settings stay unchanged.",
            })
        }
        confirmLabel={
          confirm?.kind === "delete"
            ? t("detail.confirm.deleteLabel", { defaultValue: "Delete" })
            : t("detail.confirm.restoreLabel", { defaultValue: "Restore" })
        }
        cancelLabel={t("detail.confirm.cancelLabel", {
          defaultValue: "Cancel",
        })}
        variant={confirm?.kind === "delete" ? "destructive" : "default"}
        isLoading={deleteBackupMutation.isPending || restoreMutation.isPending}
        onConfirm={async () => {
          if (!confirm) return;
          if (confirm.kind === "delete") {
            await deleteBackupMutation.mutateAsync({ backup: confirm.backup });
          } else {
            await restoreMutation.mutateAsync({ backup: confirm.backup });
          }
          setConfirm(null);
        }}
      />

      <ConfirmDialog
        isOpen={attachmentActionConfirm !== null}
        onClose={() => setAttachmentActionConfirm(null)}
        title={
          attachmentActionConfirm === "attach"
            ? t("detail.confirm.attachTitle", {
              defaultValue: "Attach MCPMate to this client configuration?",
            })
            : t("detail.confirm.detachTitle", {
              defaultValue: "Detach MCPMate from this client configuration?",
            })
        }
        description={
          attachmentActionConfirm === "attach"
            ? t("detail.confirm.attachDescription", {
              defaultValue:
                "This writes the MCPMate server entry back into the client configuration file. The client's MCP session may restart or reconnect; confirm before continuing.",
            })
            : t("detail.confirm.detachDescription", {
              defaultValue:
                "This removes the MCPMate server entry from the client configuration file. Running MCP connections for that client may break until you attach again; confirm before continuing.",
            })
        }
        confirmLabel={
          attachmentActionConfirm === "attach"
            ? t("detail.confirm.attachLabel", { defaultValue: "Attach" })
            : t("detail.confirm.detachLabel", { defaultValue: "Detach" })
        }
        cancelLabel={t("detail.confirm.cancelLabel", {
          defaultValue: "Cancel",
        })}
        variant="default"
        isLoading={clientWritebackMutationPending}
        onConfirm={async () => {
          if (
            !attachmentActionConfirm ||
            !identifier ||
            clientWritebackMutationPending
          )
            return;
          if (attachmentActionConfirm === "detach") {
            await detachMutation.mutateAsync();
          } else {
            await attachMutation.mutateAsync();
          }
          setAttachmentActionConfirm(null);
        }}
      />

      {/* Bulk delete confirmation */}
      <ConfirmDialog
        isOpen={bulkConfirmOpen}
        onClose={() => setBulkConfirmOpen(false)}
        title={t("detail.backups.bulk.title", {
          defaultValue: "Delete Selected Backups",
        })}
        description={t("detail.backups.bulk.description", {
          defaultValue:
            "Are you sure you want to delete {{count}} backup(s)? This action cannot be undone.",
          count: selectedBackups.length,
        })}
        confirmLabel={t("detail.confirm.deleteLabel", {
          defaultValue: "Delete",
        })}
        cancelLabel={t("detail.confirm.cancelLabel", {
          defaultValue: "Cancel",
        })}
        variant="destructive"
        isLoading={bulkDeleteMutation.isPending}
        onConfirm={() => bulkDeleteMutation.mutate()}
      />

      <Drawer open={policyOpen} onOpenChange={setPolicyOpen}>
        <DrawerContent>
          <DrawerHeader>
            <DrawerTitle>
              {t("detail.policy.title", { defaultValue: "Backup Policy" })}
            </DrawerTitle>
          </DrawerHeader>
          <div className="p-4 space-y-4">
            <div className="space-y-1">
              <Label>
                {t("detail.policy.fields.policy", { defaultValue: "Policy" })}
              </Label>
              <p className="text-xs text-slate-500">
                {t("detail.policy.fields.policyDescription", {
                  defaultValue:
                    'Backup retention strategy. For now, only "keep_n" is supported, which keeps at most N recent backups and prunes older ones.',
                })}
              </p>
              <Select value={policyLabel} onValueChange={setPolicyLabel}>
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="keep_n">
                    {t("detail.policy.fields.options.keepN", {
                      defaultValue: "keep_n",
                    })}
                  </SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-1">
              <Label htmlFor={limitId}>
                {t("detail.policy.fields.limit", { defaultValue: "Limit" })}
              </Label>
              <p className="text-xs text-slate-500">
                {t("detail.policy.fields.limitDescription", {
                  defaultValue:
                    "Maximum number of backups to keep for this client. Set to 0 for no limit.",
                })}
              </p>
              <Input
                id={limitId}
                type="number"
                min={0}
                value={policyLimit ?? 0}
                onChange={(event) => setPolicyLimit(Number(event.target.value))}
              />
            </div>
            <div>
              <Button
                onClick={() => {
                  if (!identifier) return;
                  setPolicyMutation.mutate({
                    identifier,
                    policy: { policy: policyLabel, limit: policyLimit },
                  });
                }}
                disabled={setPolicyMutation.isPending}
              >
                {t("detail.policy.buttons.save", {
                  defaultValue: "Save Policy",
                })}
              </Button>
            </div>
          </div>
          <DrawerFooter />
        </DrawerContent>
      </Drawer>

      <ClientImportReviewDrawer
        entries={importCandidateEntries}
        isImporting={importMutation.isPending}
        isLoading={loadingConfig}
        onImport={(selectedServerNames) => importMutation.mutate(selectedServerNames)}
        onOpenChange={setImportReviewOpen}
        open={importReviewOpen}
      />
      <ClientFormDrawer
        open={isClientFormOpen}
        onOpenChange={setIsClientFormOpen}
        mode="edit"
        client={(currentClient as ClientInfo | undefined) ?? null}
        onSuccess={() => {
          void refetchDetails();
          qc.invalidateQueries({ queryKey: ["clients"] });
        }}
        onDeleteSuccess={() => {
          navigate("/clients");
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
    </div>
  );
}

export default ClientDetailPage;
