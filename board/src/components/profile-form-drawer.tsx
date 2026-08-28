import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { HelpCircle, Loader2 } from "lucide-react";
import {
	useCallback,
	useEffect,
	useId,
	useMemo,
	useReducer,
	useRef,
	useState,
} from "react";
import { useTranslation } from "react-i18next";
import { usePageTranslations } from "../lib/i18n/usePageTranslations";
import { useNavigate } from "react-router-dom";
import { configSuitsApi, serversApi } from "../lib/api";
import { notifyError, notifySuccess } from "../lib/notify";
import { profileSyncErrorTranslationKey } from "../lib/profile-sync-error";
import {
	buildProfileServerAssignmentChanges,
	buildProfileServerTransferItems,
	buildProfileAuthoringSaveRequest,
	createProfileAuthoringConflictState,
	profileFormDraftFromAuthoringView,
	reduceProfileAuthoringConflict,
	isValidSkillName,
	shouldResetProfileAuthoringState,
	submitProfileAuthoring,
	type ProfileFormDraft,
	type ProfileServerAssignmentChanges,
	type ProfileServerPresentationLabels,
} from "../lib/profile-authoring-ui";
import type {
	ConfigSuit,
	ServerSummary,
} from "../lib/types";
import { Button } from "./ui/button";
import {
	AlertDialog,
	AlertDialogAction,
	AlertDialogCancel,
	AlertDialogContent,
	AlertDialogDescription,
	AlertDialogFooter,
	AlertDialogHeader,
	AlertDialogTitle,
} from "./ui/alert-dialog";
import {
	Drawer,
	DrawerContent,
	DrawerDescription,
	DrawerFooter,
	DrawerHeader,
	DrawerTitle,
} from "./ui/drawer";
import { Input } from "./ui/input";
import { Label } from "./ui/label";
import { ScrollArea } from "./ui/scroll-area";
import { Switch } from "./ui/switch";
import { Textarea } from "./ui/textarea";
import {
	Tooltip,
	TooltipContent,
	TooltipProvider,
	TooltipTrigger,
} from "./ui/tooltip";
import { Transfer, type TransferItem } from "./ui/transfer";
import { Segment, type SegmentOption } from "./ui/segment";

type DrawerStep = "details" | "workflow-rules" | "servers";

const arraysEqual = (a: string[], b: string[]) => {
	if (a.length !== b.length) {
		return false;
	}
	const setB = new Set(b);
	return a.every((id) => setB.has(id));
};

const suggestedSkillName = (name: string) =>
	name
		.trim()
		.toLowerCase()
		.replace(/[^a-z0-9]+/g, "-")
		.replace(/^-+|-+$/g, "")
		.slice(0, 64)
		.replace(/-+$/g, "");

interface ProfileAuthoringConflictSummaryProps {
	changes: ProfileServerAssignmentChanges;
	labels: {
		added: string;
		removed: string;
		unchanged: string;
	};
}

export function ProfileAuthoringConflictSummary({
	changes,
	labels,
}: ProfileAuthoringConflictSummaryProps) {
	if (changes.added.length === 0 && changes.removed.length === 0) {
		return <p className="text-sm text-muted-foreground">{labels.unchanged}</p>;
	}

	return (
		<ScrollArea className="h-56 rounded-md border">
			<div className="flex flex-col gap-4 p-3">
				{changes.added.length > 0 && (
					<section className="flex flex-col gap-2">
						<h4 className="text-sm font-medium">{labels.added}</h4>
						<ul className="flex flex-col gap-1 text-sm text-muted-foreground">
							{changes.added.map((server) => (
								<li key={server.id}>{server.name}</li>
							))}
						</ul>
					</section>
				)}
				{changes.removed.length > 0 && (
					<section className="flex flex-col gap-2">
						<h4 className="text-sm font-medium">{labels.removed}</h4>
						<ul className="flex flex-col gap-1 text-sm text-muted-foreground">
							{changes.removed.map((server) => (
								<li key={server.id}>{server.name}</li>
							))}
						</ul>
					</section>
				)}
			</div>
		</ScrollArea>
	);
}


interface ProfileServerTransferProps {
	servers: ServerSummary[];
	selectedServerIds: string[];
	labels: ProfileServerPresentationLabels;
	onChange: (targetKeys: string[]) => void;
	onItemInfo?: (item: TransferItem) => void;
	leftTitle: string;
	rightTitle: string;
	searchPlaceholder: string;
	emptyText: string;
	disabled: boolean;
	loading: boolean;
}

export function ProfileServerTransfer({
	servers,
	selectedServerIds,
	labels,
	onChange,
	onItemInfo,
	leftTitle,
	rightTitle,
	searchPlaceholder,
	emptyText,
	disabled,
	loading,
}: ProfileServerTransferProps) {
	const dataSource = buildProfileServerTransferItems(
		servers,
		labels,
	);
	return (
		<Transfer
			dataSource={dataSource}
			targetKeys={selectedServerIds}
			onChange={onChange}
			onItemInfo={onItemInfo}
			leftTitle={leftTitle}
			rightTitle={rightTitle}
			searchPlaceholder={searchPlaceholder}
			emptyText={emptyText}
			disabled={disabled}
			loading={loading}
			className="flex-1"
		/>
	);
}

interface ProfileFormDrawerProps {
	open: boolean;
	onOpenChange: (open: boolean) => void;
	mode: "create" | "edit";
	suit?: ConfigSuit;
	onSuccess?: () => void;
	restrictProfileType?: string; // Restrict to specific profile type
}

export function ProfileFormDrawer({
	open,
	onOpenChange,
	mode,
	suit,
	onSuccess,
	restrictProfileType,
}: ProfileFormDrawerProps) {
	const { t, i18n } = useTranslation();
	usePageTranslations("profiles");
	const queryClient = useQueryClient();
	const navigate = useNavigate();

	// Form state
		const [formData, setFormData] = useState<ProfileFormDraft>({
			name: "",
			skill_name: "",
			package_distribution: "",
			description: "",
			suit_type: restrictProfileType || "shared",
			priority: 50,
			is_active: false,
			is_default: false,
			clone_from_id: "none",
			profile_mode: "capability",
		});

	// Generate unique IDs for form elements
		const nameId = useId();
		const skillNameId = useId();
		const packageDistributionId = useId();
		const descriptionId = useId();
	const validationNotesId = useId();
	const avoidRulesId = useId();
	const isActiveId = useId();
	const isDefaultId = useId();
	const profileModeId = useId();

	const [step, setStep] = useState<DrawerStep>("details");
	const [validationNotes, setValidationNotes] = useState("");
	const [avoidRules, setAvoidRules] = useState("");
	const [selectedServerIds, setSelectedServerIds] = useState<string[]>([]);
	const [selectionInitialized, setSelectionInitialized] = useState(false);
	const [, setServerSelectionTouched] = useState(false);
	const [isClosing, setIsClosing] = useState(false);
	const [conflictState, dispatchConflict] = useReducer(
		reduceProfileAuthoringConflict,
		undefined,
		createProfileAuthoringConflictState,
	);
	const {
		baselineView: authoringBaselineView,
		latestView: latestAuthoringView,
		dialogOpen: conflictDialogOpen,
	} = conflictState;
	const resetIdentityRef = useRef({
		open: false,
		mode,
		profileId: suit?.id ?? null,
	});
	const workflowRulesHydrationRef = useRef<string | null>(null);
	const skillNameTouchedRef = useRef(false);

	const isHostAppProfile = restrictProfileType === "host_app" || suit?.suit_type === "host_app";
	const isWorkflowProfile = formData.profile_mode === "workflow";
	const isWorkflowEdit = mode === "edit" && isWorkflowProfile;
	const workflowSpecificationQuery = useQuery({
		queryKey: ["workflowSpecification", suit?.id],
		queryFn: () => configSuitsApi.getWorkflowSpecification(suit!.id),
		enabled: open && isWorkflowEdit && Boolean(suit?.id),
		retry: false,
	});
	const hasWorkflowGuidanceChanges =
		isWorkflowProfile &&
		(mode === "create" ||
			validationNotes !== (workflowSpecificationQuery.data?.validation_notes ?? "") ||
			avoidRules !== (workflowSpecificationQuery.data?.avoid_rules ?? ""));
	const workflowGuidanceRequest = hasWorkflowGuidanceChanges
		? {
				expected_specification_revision:
					workflowSpecificationQuery.data?.specification_revision ?? null,
				validation_notes: validationNotes.trim() || null,
				avoid_rules: avoidRules.trim() || null,
			}
		: undefined;
	const profileModeOptions = useMemo<SegmentOption[]>(
		() => [
			{
				value: "capability",
				label: t("profiles:form.profileModes.capability"),
			},
			{
				value: "workflow",
				label: t("profiles:form.profileModes.workflow"),
			},
		],
		[i18n.language, t],
	);

	const steps: Array<{ id: DrawerStep; label: string; hint: string }> = isHostAppProfile
		? [
				{
					id: "servers",
					label: t("profiles:form.steps.servers", { defaultValue: "Servers" }),
					hint: t("profiles:form.steps.hints.assign", { defaultValue: "Assign" }),
				},
			]
		: isWorkflowProfile
			? [
					{
						id: "details" as const,
						label: t("profiles:form.steps.profile", { defaultValue: "Profile" }),
						hint: t("profiles:form.steps.hints.basics", { defaultValue: "Basics" }),
					},
					{
						id: "workflow-rules",
						label: t("profiles:form.steps.workflow", { defaultValue: "Workflow" }),
						hint: t("profiles:form.steps.hints.rules", { defaultValue: "Rules" }),
					},
				]
			: [
				{
					id: "details",
					label: t("profiles:form.steps.profile", { defaultValue: "Profile" }),
					hint: t("profiles:form.steps.hints.basics", { defaultValue: "Basics" }),
				},
				{
					id: "servers",
					label: t("profiles:form.steps.servers", { defaultValue: "Servers" }),
					hint: t("profiles:form.steps.hints.assign", { defaultValue: "Assign" }),
				},
			];

	// 完全重置所有状态的函数
	const resetAllStates = useCallback(() => {
		setStep(isHostAppProfile ? "servers" : "details");
		setValidationNotes("");
		setAvoidRules("");
		setSelectionInitialized(false);
		setServerSelectionTouched(false);
		setSelectedServerIds([]);
		dispatchConflict({ type: "reset" });

		if (mode === "edit" && suit) {
				setFormData({
					name: suit.name,
					skill_name: "",
					package_distribution: "",
					description: suit.description || "",
					suit_type: suit.suit_type,
					priority: suit.priority,
					is_active: suit.is_active,
					is_default: suit.is_default,
					clone_from_id: "none", // Not applicable in edit mode
					profile_mode: suit.profile_mode,
				});
		} else {
			// Create mode - reset to empty form
				setFormData({
					name: "",
					skill_name: "",
					package_distribution: "",
					description: "",
					suit_type: restrictProfileType || "shared",
					priority: 50,
					is_active: false,
					is_default: false,
					clone_from_id: "none",
					profile_mode: "capability",
				});
		}
	}, [mode, suit, restrictProfileType, isHostAppProfile]);

	useEffect(() => {
		if (
			!open ||
			step !== "workflow-rules" ||
			!suit?.id ||
			!workflowSpecificationQuery.data
		) {
			return;
		}
		if (workflowRulesHydrationRef.current === suit.id) {
			return;
		}
		setValidationNotes(workflowSpecificationQuery.data.validation_notes ?? "");
		setAvoidRules(workflowSpecificationQuery.data.avoid_rules ?? "");
		workflowRulesHydrationRef.current = suit.id;
	}, [open, step, suit?.id, workflowSpecificationQuery.data]);

	useEffect(() => {
		if (!open) {
			workflowRulesHydrationRef.current = null;
		}
	}, [open]);

	// Overlay close handler (immediate, no delay)
	const handleOverlayClose = useCallback(() => {
		if (!isClosing) {
			setIsClosing(true);
			resetAllStates();
			onOpenChange(false);
			setIsClosing(false);
		}
	}, [onOpenChange, resetAllStates, isClosing]);

	// Cancel close handler (with delay for complete reset)
	const handleCancelClose = useCallback(() => {
		if (!isClosing) {
			setIsClosing(true);
			setTimeout(() => {
				resetAllStates();
				onOpenChange(false);
				setIsClosing(false);
			}, 150); // Small delay to allow animation
		}
	}, [onOpenChange, resetAllStates, isClosing]);

	const closeDrawer = useCallback(
		() => handleCancelClose(),
		[handleCancelClose],
	);

	// Reset form data when dialog opens or mode/suit changes
	useEffect(() => {
		const nextIdentity = {
			open,
			mode,
			profileId: suit?.id ?? null,
		};
		const shouldReset = shouldResetProfileAuthoringState(
			resetIdentityRef.current,
			nextIdentity,
		);
		resetIdentityRef.current = nextIdentity;
		if (shouldReset) {
			skillNameTouchedRef.current = false;
			resetAllStates();
		}
		if (!open) {
			if (suit?.id) {
				queryClient.removeQueries({
					queryKey: ["profileAuthoringView", suit.id],
					exact: true,
				});
			}
			// 关闭时清理查询缓存，防止状态残留
			setTimeout(() => {
				queryClient.removeQueries({
					queryKey: ["configSuitDrawerServers"],
					exact: true,
				});
				queryClient.removeQueries({
					queryKey: ["configSuitClonePreview"],
					exact: false,
				});
			}, 200);
		}
	}, [open, mode, suit?.id, resetAllStates, queryClient]);

	// Fetch all suits for cloning option
	const { data: suitsResponse } = useQuery({
		queryKey: ["configSuits"],
		queryFn: configSuitsApi.getAll,
		enabled: open,
	});

	const { data: allServersResponse, isLoading: isLoadingAllServers } = useQuery(
		{
			queryKey: ["configSuitDrawerServers"],
			queryFn: serversApi.getAll,
			enabled: open,
			staleTime: 30_000,
		},
	);

	const { data: authoringView, isLoading: isLoadingAuthoringView } =
		useQuery({
			queryKey: ["profileAuthoringView", suit?.id],
			queryFn: () =>
				suit?.id
					? configSuitsApi.getAuthoringView(suit.id)
					: Promise.resolve(undefined),
			enabled: open && mode === "edit" && !!suit?.id,
			staleTime: 15_000,
		});

	const defaultSuitId = useMemo(
		() => (suitsResponse?.suits ?? []).find((profile) => profile.is_default)?.id,
		[suitsResponse],
	);

	useEffect(() => {
		if (!open) {
			return;
		}
		if (mode === "edit") {
			if (!authoringView || selectionInitialized) {
				return;
			}
			setFormData(profileFormDraftFromAuthoringView(authoringView));
			skillNameTouchedRef.current = Boolean(authoringView.skill_name);
			setSelectedServerIds(authoringView.server_ids);
			dispatchConflict({ type: "baselineLoaded", view: authoringView });
			setSelectionInitialized(true);
		} else if (mode === "create" && !selectionInitialized) {
			setSelectionInitialized(true);
		}
	}, [open, mode, selectionInitialized, authoringView]);


	const authoringMutation = useMutation({
		mutationFn: submitProfileAuthoring,
		onSuccess: (result) => {
			if (result.status === "conflict") {
				dispatchConflict({
					type: "conflictReceived",
					view: result.latestView,
				});
				return;
			}
			const profileId = result.data.profile.id;
			queryClient.invalidateQueries({ queryKey: ["configSuits"] });
			if (profileId) {
				queryClient.invalidateQueries({ queryKey: ["configSuit", profileId] });
				queryClient.invalidateQueries({
					queryKey: ["configSuitServers", profileId],
				});
				queryClient.invalidateQueries({
					queryKey: ["profileAuthoringView", profileId],
				});
				queryClient.invalidateQueries({
					queryKey: ["workflowSpecification", profileId],
				});
				queryClient.invalidateQueries({
					queryKey: ["workflowSpecificationPreview", profileId],
				});
			}
			notifySuccess(
				mode === "create"
					? t("profiles:form.messages.created", { defaultValue: "Created" })
					: t("profiles:form.messages.updated", { defaultValue: "Updated" }),
				mode === "create"
					? t("profiles:form.messages.createdDescription", {
							defaultValue: "Profile created successfully",
						})
					: t("profiles:form.messages.updatedDescription", {
							defaultValue: "Profile updated successfully",
						}),
			);
			closeDrawer();
			if (mode === "create" && result.data.profile_mode === "workflow" && profileId) {
				navigate(`/profiles/${encodeURIComponent(profileId)}?tab=workflow`);
			}
			onSuccess?.();
		},
		onError: (error: Error) => {
			notifyError(
				mode === "create"
					? t("profiles:form.messages.createFailed", {
							defaultValue: "Create failed",
						})
					: t("profiles:form.messages.updateFailed", {
							defaultValue: "Update failed",
						}),
				t(profileSyncErrorTranslationKey(error)),
			);
		},
	});

	// Handle form submission
	const handleSubmit = (e: React.FormEvent) => {
		e.preventDefault();

		if (!formData.name.trim()) {
			notifyError(
				t("profiles:form.messages.validationFailed", {
					defaultValue: "Validation failed",
				}),
				t("profiles:form.messages.nameRequired", {
					defaultValue: "Name is required",
				}),
			);
			return;
		}
		if (isWorkflowProfile && !isValidSkillName(formData.skill_name.trim())) {
			notifyError(
				t("profiles:form.messages.validationFailed", { defaultValue: "Validation failed" }),
				t("profiles:form.messages.skillNameInvalid"),
			);
			return;
		}

		if (step === "details" && !isWorkflowProfile) {
			setStep("servers");
			return;
		}
		if (step === "details" && isWorkflowProfile) {
			setStep("workflow-rules");
			return;
		}

		if (mode === "create") {
			const request = buildProfileAuthoringSaveRequest({
				mode,
				profileId: null,
				draft: formData,
				serverIds: selectedServerIds,
				workflowGuidance: workflowGuidanceRequest,
			});
			authoringMutation.mutate(request);
		} else if (suit && authoringBaselineView) {
			if (latestAuthoringView) {
				dispatchConflict({ type: "saveRequested" });
				return;
			}
			const currentTargetKeys = targetServerKeys;
			const selectionChanged = !arraysEqual(
				selectedServerIds,
				currentTargetKeys,
			);
			const current = authoringBaselineView.profile;
			const hasFieldUpdates =
				formData.name !== current.name ||
				formData.description !== (current.description || "") ||
				formData.suit_type !== current.suit_type ||
				formData.priority !== current.priority ||
				formData.is_active !== current.is_active ||
				formData.is_default !== current.is_default ||
				formData.profile_mode !==
					(authoringBaselineView.profile_mode ?? current.profile_mode ?? "capability") ||
					formData.skill_name !== (authoringBaselineView.skill_name ?? "") ||
					formData.package_distribution !==
						(authoringBaselineView.package_distribution ?? "");
			if (!latestAuthoringView && !selectionChanged && !hasFieldUpdates && !hasWorkflowGuidanceChanges) {
				closeDrawer();
				return;
			}
			authoringMutation.mutate(
				buildProfileAuthoringSaveRequest({
					mode,
					profileId: suit.id,
					draft: formData,
					serverIds: selectedServerIds,
					authoringView: authoringBaselineView,
					workflowGuidance: workflowGuidanceRequest,
				}),
			);
		}
	};

	const isMutating = authoringMutation.isPending;
	const detailsStepValid =
		isHostAppProfile ||
		(formData.name.trim().length > 0 &&
			(!isWorkflowProfile ||
				(formData.description.trim().length > 0 &&
					isValidSkillName(formData.skill_name.trim()))));

	const allServers = useMemo(
		() => allServersResponse?.servers ?? [],
		[allServersResponse?.servers],
	);
	const serverAssignmentChanges = useMemo(
		() =>
			authoringBaselineView && latestAuthoringView
				? buildProfileServerAssignmentChanges(
						authoringBaselineView,
						latestAuthoringView,
						allServers,
					)
				: { added: [], removed: [] },
		[authoringBaselineView, latestAuthoringView, allServers],
	);

	const handleLoadLatest = () => {
		if (!latestAuthoringView || !suit) {
			return;
		}
		setFormData(profileFormDraftFromAuthoringView(latestAuthoringView));
		setSelectedServerIds(latestAuthoringView.server_ids);
		queryClient.setQueryData(
			["profileAuthoringView", suit.id],
			latestAuthoringView,
		);
		dispatchConflict({ type: "loadLatest" });
	};

	const handleOverwrite = () => {
		if (!latestAuthoringView || !suit || !authoringBaselineView) {
			return;
		}
			dispatchConflict({ type: "overwriteStarted" });
			authoringMutation.mutate(
				buildProfileAuthoringSaveRequest({
				mode: "edit",
				profileId: suit.id,
				draft: formData,
				serverIds: selectedServerIds,
				authoringView: authoringBaselineView,
				expectedAuthoringGeneration:
					latestAuthoringView.profile.authoring_generation,
				workflowGuidance: workflowGuidanceRequest,
			}),
		);
	};

	const serverPresentationLabels: ProfileServerPresentationLabels = {
		globalStatus: t("profiles:form.serverState.globalStatus", {
			defaultValue: "Global status",
		}),
		catalog: t("profiles:form.serverState.catalog", {
			defaultValue: "Catalog",
		}),
		enabled: t("status.enabled", { defaultValue: "Enabled" }),
		disabled: t("status.disabled", { defaultValue: "Disabled" }),
		notReported: t("profiles:form.serverState.notReported", {
			defaultValue: "Not reported",
		}),
		ready: t("status.ready", { defaultValue: "Ready" }),
		unavailable: t("profiles:form.serverState.unavailable", {
			defaultValue: "Unavailable",
		}),
		notObserved: t("profiles:form.serverState.notObserved", {
			defaultValue: "Not observed",
		}),
	};

	// 获取当前已经纳入管理的服务器 ID 列表
	const targetServerKeys = useMemo(() => {
		return authoringBaselineView?.server_ids ?? [];
	}, [authoringBaselineView?.server_ids]);

	const selectedServerCount = targetServerKeys.length;
	const totalServerCount = allServers.length;
	const isServersStepLoading =
		step === "servers" &&
		(isLoadingAllServers ||
			(mode === "edit" && !selectionInitialized && isLoadingAuthoringView));
	const isWorkflowRulesLoading =
		step === "workflow-rules" && isWorkflowEdit && workflowSpecificationQuery.isLoading;
	const isWorkflowRulesUnavailable =
		step === "workflow-rules" &&
		isWorkflowEdit &&
		(workflowSpecificationQuery.isError || !workflowSpecificationQuery.data);

	const primaryDisabled =
		isMutating ||
		(step === "details" && !detailsStepValid) ||
		(step === "servers" && isServersStepLoading) ||
		isWorkflowRulesLoading ||
		isWorkflowRulesUnavailable;
	const primaryLabel = isMutating
		? t("profiles:form.buttons.saving", { defaultValue: "Saving..." })
		: step === "details"
			? t("profiles:form.buttons.next", { defaultValue: "Next" })
			: mode === "create"
				? t("profiles:form.buttons.create", { defaultValue: "Create Profile" })
				: t("profiles:form.buttons.save", { defaultValue: "Save Changes" });

	const showDefaultToggle =
		mode === "edit"
			? !defaultSuitId || defaultSuitId === suit?.id
			: !defaultSuitId;

	useEffect(() => {
		if (!showDefaultToggle) {
			setFormData((prev) =>
				prev.is_default ? { ...prev, is_default: false } : prev,
			);
		}
	}, [showDefaultToggle]);

	useEffect(() => {
		if (isWorkflowProfile) {
			setFormData((prev) =>
				prev.is_default ? { ...prev, is_default: false } : prev,
			);
		}
	}, [isWorkflowProfile]);

	// Transfer 组件的处理函数
	const handleTransferChange = useCallback((targetKeys: string[]) => {
		setServerSelectionTouched(true);
		setSelectedServerIds(targetKeys);
	}, []);

	const handleServerInfo = useCallback(
		(item: TransferItem) => {
			// 跳转到服务器详情页面
			navigate(`/servers/${item.id}`);
		},
		[navigate],
	);

	const createModeSection = (
		<div className="space-y-4">
			<div className="flex items-center gap-4">
				<span className="w-32 text-sm font-medium text-slate-600 dark:text-slate-300">
					{t("profiles:form.fields.status", { defaultValue: "Status" })}
				</span>
				<div className="flex flex-wrap items-center gap-6">
					<div className="flex items-center gap-2">
						<Switch
							id={isActiveId}
							checked={formData.is_active}
							onCheckedChange={(checked) =>
								setFormData((prev) => ({
									...prev,
									is_active: checked,
								}))
							}
						/>
						<Label htmlFor={isActiveId} className="text-sm">
							{isWorkflowProfile
								? t("profiles:form.labels.publishWorkflow", {
										defaultValue: "Publish Skill package",
									})
								: t("profiles:form.labels.activateImmediately", {
										defaultValue: "Activate immediately",
									})}
						</Label>
					</div>
					{showDefaultToggle && (
						<div className="flex items-center gap-2 hidden">
							<Switch
								id={isDefaultId}
								checked={formData.is_default}
								onCheckedChange={(checked) =>
									setFormData((prev) => ({
										...prev,
										is_default: checked,
									}))
								}
							/>
							<Label htmlFor={isDefaultId} className="text-sm">
								{t("profiles:form.labels.setAsDefault", {
									defaultValue: "Set as default profile",
								})}
							</Label>
						</div>
					)}
				</div>
			</div>
		</div>
	);

	const editModeSection = (
		<div className="space-y-4">
			<div className="flex items-center gap-4">
				<span className="w-32 text-sm font-medium text-slate-600 dark:text-slate-300">
					{t("profiles:form.fields.status", { defaultValue: "Status" })}
				</span>
				<div className="flex flex-wrap items-center gap-6">
					<div className="flex items-center gap-2">
						<Switch
							id={isActiveId}
							checked={formData.is_active}
							onCheckedChange={(checked) =>
								setFormData((prev) => ({
									...prev,
									is_active: checked,
								}))
							}
						/>
						<Label htmlFor={isActiveId} className="text-sm">
							{isWorkflowProfile
								? t("profiles:form.labels.publishWorkflow", {
										defaultValue: "Publish Skill package",
									})
								: t("profiles:form.labels.activateImmediately", {
										defaultValue: "Activate immediately",
									})}
						</Label>
					</div>
					{showDefaultToggle && (
						<div className="flex items-center gap-2 hidden">
							<Switch
								id={isDefaultId}
								checked={formData.is_default}
								onCheckedChange={(checked) =>
									setFormData((prev) => ({
										...prev,
										is_default: checked,
									}))
								}
							/>
							<Label htmlFor={isDefaultId} className="text-sm">
								{t("profiles:form.labels.setAsDefault", {
									defaultValue: "Set as default profile",
								})}
							</Label>
						</div>
					)}
				</div>
			</div>
		</div>
	);

	const detailsModeContent =
		mode === "create" ? createModeSection : editModeSection;

	// 使用组合键确保每次打开时组件完全重新渲染
	const drawerKey = `suit-form-drawer-${mode}-${suit?.id || "new"}-${open ? "open" : "closed"}`;

	return (
		<Drawer
			key={drawerKey}
			open={open}
			onOpenChange={(open) => !open && handleOverlayClose()}
		>
			<DrawerContent className="h-full flex flex-col">
				<DrawerHeader>
					<DrawerTitle>
						{isHostAppProfile
							? t("profiles:form.title.manageServers", {
									defaultValue: "Manage Servers",
								})
							: mode === "create"
								? t("profiles:form.title.create", {
										defaultValue: "Create New Profile",
									})
								: t("profiles:form.title.edit", { defaultValue: "Edit Profile" })}
					</DrawerTitle>
					<DrawerDescription>
						{isHostAppProfile
							? t("profiles:form.description.manageServers", {
									defaultValue:
										"Configure which MCP servers are available to this client.",
								})
							: mode === "create"
								? t("profiles:form.description.create", {
										defaultValue:
											"Create a new profile to organize your MCP servers and tools.",
									})
								: t("profiles:form.description.edit", {
										defaultValue: "Update the profile settings.",
									})}
					</DrawerDescription>
				</DrawerHeader>

				<form onSubmit={handleSubmit} className="flex h-full flex-col">
					{/* Content area - scrollable */}
					<div className="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto overflow-x-hidden p-4">
						{!isHostAppProfile && (
							<div className="flex flex-wrap items-center gap-4">
							{steps.map((item, index) => {
								const isActive = step === item.id;
								const canNavigate =
									item.id === "details" ||
									(item.id === "workflow-rules" && detailsStepValid) ||
									(item.id === "servers" && detailsStepValid);

								return (
									<div key={item.id} className="flex items-center gap-2">
										<button
											type="button"
											onClick={() => {
												if (canNavigate && !isMutating) {
													setStep(item.id);
												}
											}}
											disabled={!canNavigate || isMutating}
											className={`flex h-7 w-7 items-center justify-center rounded-full text-xs font-semibold transition-colors ${
												isActive
													? "bg-primary text-primary-foreground"
													: canNavigate
														? "bg-slate-200 text-slate-600 hover:bg-slate-300 dark:bg-slate-800 dark:text-slate-300 dark:hover:bg-slate-700 cursor-pointer"
														: "bg-slate-100 text-slate-400 dark:bg-slate-900 dark:text-slate-500 cursor-not-allowed"
											}`}
										>
											{index + 1}
										</button>
										<button
											type="button"
											onClick={() => {
												if (canNavigate && !isMutating) {
													setStep(item.id);
												}
											}}
											disabled={!canNavigate || isMutating}
											className="flex flex-col text-left transition-colors hover:opacity-80 disabled:cursor-not-allowed disabled:opacity-50"
										>
											<span
												className={`text-sm font-medium ${
													isActive
														? "text-primary"
														: canNavigate
															? "text-slate-600 dark:text-slate-300"
															: "text-slate-400 dark:text-slate-500"
												}`}
											>
												{item.label}
											</span>
											<span className="text-xs text-muted-foreground">
												{item.hint}
											</span>
										</button>
										{index < steps.length - 1 && (
											<span className="hidden h-px w-10 bg-slate-200 md:block dark:bg-slate-800" />
										)}
									</div>
								);
							})}
						</div>
						)}

						{step === "details" && (
							<div className="space-y-4">
								<div className="flex items-center gap-4">
									<Label
										htmlFor={nameId}
										className="w-32 text-sm font-medium text-slate-600 dark:text-slate-300"
									>
										{t("profiles:form.fields.nameRequired", {
											defaultValue: "Name *",
										})}
									</Label>
									<Input
										id={nameId}
										value={formData.name}
										onChange={(e) =>
											setFormData((prev) => ({
												...prev,
												name: e.target.value,
												...(prev.profile_mode === "workflow" && !skillNameTouchedRef.current
													? { skill_name: suggestedSkillName(e.target.value) }
													: {}),
											}))
										}
										placeholder={t("profiles:form.placeholders.profileName", {
											defaultValue: "Enter profile name",
										})}
										required
										className="flex-1"
									/>
								</div>

								{isWorkflowProfile && (
									<div className="flex items-start gap-4">
										<Label
											htmlFor={skillNameId}
											className="flex w-32 items-center gap-1 pt-2 text-sm font-medium text-slate-600 dark:text-slate-300"
										>
												{t("profiles:form.fields.skillName")}
											<TooltipProvider delayDuration={200}>
												<Tooltip>
													<TooltipTrigger asChild>
														<span
															className="inline-flex cursor-help text-muted-foreground"
														aria-label={t("profiles:form.fields.skillNameHelp")}
														>
															<HelpCircle className="size-3.5" />
														</span>
													</TooltipTrigger>
													<TooltipContent side="right" className="max-w-xs">
														{t("profiles:form.fields.skillNameHelp")}
													</TooltipContent>
												</Tooltip>
											</TooltipProvider>
										</Label>
										<div className="min-w-0 flex-1">
											<Input
												id={skillNameId}
												value={formData.skill_name}
												onChange={(e) => {
													skillNameTouchedRef.current = true;
													setFormData((prev) => ({ ...prev, skill_name: e.target.value }));
												}}
											placeholder={t("profiles:form.placeholders.skillName")}
												maxLength={64}
												pattern="[a-z0-9]+(-[a-z0-9]+)*"
												aria-invalid={
													formData.skill_name.length > 0 &&
													!isValidSkillName(formData.skill_name.trim())
												}
												required
											/>
											</div>
										</div>
									)}

									{isWorkflowProfile && (
										<div className="flex items-start gap-4">
											<Label
												htmlFor={packageDistributionId}
												className="flex w-32 items-center gap-1 pt-2 text-sm font-medium text-slate-600 dark:text-slate-300"
											>
												{t("profiles:form.fields.packageDistribution")}
												<TooltipProvider delayDuration={200}>
													<Tooltip>
														<TooltipTrigger asChild>
															<span
																className="inline-flex cursor-help text-muted-foreground"
																aria-label={t("profiles:form.fields.packageDistributionHelp")}
															>
																<HelpCircle className="size-3.5" />
															</span>
														</TooltipTrigger>
														<TooltipContent side="right" className="max-w-xs">
															{t("profiles:form.fields.packageDistributionHelp")}
														</TooltipContent>
													</Tooltip>
												</TooltipProvider>
											</Label>
											<select
												id={packageDistributionId}
												value={formData.package_distribution}
												onChange={(e) =>
													setFormData((prev) => ({
														...prev,
														package_distribution: e.target.value as
															| ""
															| "symlink"
															| "copy",
													}))
												}
												className="flex h-9 min-w-0 flex-1 rounded-md border border-input bg-transparent px-3 py-1 text-sm shadow-xs outline-none focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/50"
											>
												<option value="">
													{t("profiles:form.labels.packageDistributionNone")}
												</option>
												<option value="symlink">
													{t("profiles:form.labels.packageDistributionSymlink")}
												</option>
												<option value="copy">
													{t("profiles:form.labels.packageDistributionCopy")}
												</option>
											</select>
										</div>
									)}

									<div className="flex items-start gap-4">
									<Label
										htmlFor={descriptionId}
										className="w-32 text-sm font-medium text-slate-600 dark:text-slate-300"
									>
										{t("profiles:form.fields.description", {
											defaultValue: "Description",
										})}
										{isWorkflowProfile ? " *" : null}
									</Label>
									<Textarea
										id={descriptionId}
										value={formData.description}
										onChange={(e) =>
											setFormData((prev) => ({
												...prev,
												description: e.target.value,
											}))
										}
										placeholder={
											isWorkflowProfile
												? t("profiles:form.placeholders.workflowDescription", {
													defaultValue:
														"Describe the workflow goal, expected outcome, and key constraints",
												})
												: t("profiles:form.placeholders.profileDescription", {
													defaultValue: "Provide a short summary",
												})
										}
										rows={3}
										className="flex-1"
									/>
								</div>

								<div className="flex items-start gap-4" role="group" aria-labelledby={profileModeId}>
									<Label id={profileModeId} className="flex w-32 items-center gap-1 pt-2 text-sm font-medium text-slate-600 dark:text-slate-300">
										{t("profiles:form.fields.profileMode")}
										<TooltipProvider delayDuration={200}>
											<Tooltip>
												<TooltipTrigger asChild>
													<span
														className="inline-flex cursor-help text-muted-foreground"
														aria-label="Profile mode help"
													>
														<HelpCircle className="size-3.5" />
													</span>
												</TooltipTrigger>
												<TooltipContent side="right" className="max-w-xs">
													{isWorkflowProfile
														? t("profiles:form.profileModes.workflowDescription")
														: t("profiles:form.profileModes.capabilityDescription")}
													{mode === "edit" ? ` ${t("profiles:form.profileModes.editLockedHint")}` : null}
												</TooltipContent>
											</Tooltip>
										</TooltipProvider>
									</Label>
									<div className="min-w-0 flex-1">
										<Segment
											value={formData.profile_mode}
											onValueChange={(value) => {
												if (mode !== "create") return;
												setFormData((prev) => ({
													...prev,
													profile_mode: value as "capability" | "workflow",
													...(value === "workflow" && !skillNameTouchedRef.current
														? { skill_name: suggestedSkillName(prev.name) }
														: {}),
													...(value === "workflow" ? { is_default: false } : {}),
												}));
											}}
											options={profileModeOptions}
											showDots={false}
											disabled={mode === "edit"}
										/>
									</div>
								</div>

							{detailsModeContent}
							</div>
					)}
					{step === "workflow-rules" && isWorkflowProfile && (
						<div className="space-y-4">
							{isWorkflowRulesLoading ? (
								<div className="flex min-h-48 items-center justify-center rounded-lg border border-dashed border-slate-200 text-sm text-muted-foreground dark:border-slate-700">
									<div className="flex items-center gap-2">
										<Loader2 className="h-4 w-4 animate-spin" />
										{t("profiles:detail.workflow.loading")}
									</div>
								</div>
							) : isWorkflowRulesUnavailable ? (
								<p role="alert" className="text-sm text-destructive">
									{t("profiles:detail.workflow.loadFailed", {
										defaultValue: "Unable to load workflow specification.",
									})}
								</p>
							) : (
								<>
									<div className="flex items-start gap-4">
										<Label
											htmlFor={validationNotesId}
											className="w-32 pt-2 text-sm font-medium text-slate-600 dark:text-slate-300"
										>
											{t("profiles:detail.workflow.brief.validationNotes")}
										</Label>
										<Textarea
											id={validationNotesId}
											value={validationNotes}
											onChange={(event) => setValidationNotes(event.target.value)}
											rows={6}
											className="flex-1"
										/>
									</div>
									<div className="flex items-start gap-4">
										<Label
											htmlFor={avoidRulesId}
											className="w-32 pt-2 text-sm font-medium text-slate-600 dark:text-slate-300"
										>
											{t("profiles:detail.workflow.brief.avoidRules")}
										</Label>
										<Textarea
											id={avoidRulesId}
											value={avoidRules}
											onChange={(event) => setAvoidRules(event.target.value)}
											rows={6}
											className="flex-1"
										/>
									</div>
								</>
							)}
						</div>
					)}
						{step === "servers" && (
							<div className="flex min-h-0 flex-1 flex-col gap-4">
								<div>
									<p className="text-xs text-muted-foreground">
										{t("profiles:form.serverSelection.title", {
											defaultValue:
												"Choose which servers belong to this profile. Server enable/disable status is managed separately.",
										})}{" "}
										{selectedServerCount}{" "}
										{t("profiles:form.serverSelection.assigned", {
											defaultValue: "servers assigned",
										})}
										, {totalServerCount}{" "}
										{t("profiles:form.serverSelection.available", {
											defaultValue: "available servers",
										})}
									</p>
								</div>

								<div className="flex min-h-0 flex-1">
									{isServersStepLoading ? (
										<div className="flex-1 flex items-center justify-center rounded-lg border border-dashed border-slate-200 text-sm text-muted-foreground dark:border-slate-700">
											<div className="flex items-center gap-2">
												<Loader2 className="h-4 w-4 animate-spin" />
												{t("profiles:form.serverSelection.loading", {
													defaultValue: "Loading server list…",
												})}
											</div>
										</div>
									) : totalServerCount === 0 ? (
										<div className="flex-1 flex items-center justify-center rounded-lg border border-dashed border-slate-200 text-center text-sm text-muted-foreground dark:border-slate-700">
											{t("profiles:form.serverSelection.noAvailable", {
												defaultValue: "No available servers",
											})}
										</div>
									) : (
										<ProfileServerTransfer
											servers={allServers}
											selectedServerIds={selectedServerIds}
											labels={serverPresentationLabels}
											onChange={handleTransferChange}
											onItemInfo={handleServerInfo}
											leftTitle={t(
												"profiles:form.serverSelection.availableServers",
												{ defaultValue: "Available Servers" },
											)}
											rightTitle={t(
												"profiles:form.serverSelection.profileServers",
												{ defaultValue: "Profile Servers" },
											)}
											searchPlaceholder={t(
												"profiles:form.placeholders.searchServers",
												{ defaultValue: "Search servers..." },
											)}
											emptyText={t("profiles:form.serverSelection.noData", {
												defaultValue: "No data",
											})}
											disabled={isMutating}
											loading={isServersStepLoading}
										/>
									)}
								</div>
							</div>
						)}
					</div>

					<DrawerFooter className="border-t bg-background">
						<div className="flex w-full flex-wrap items-center justify-between gap-2">
							<div className="flex gap-2">
								{isHostAppProfile ? (
									<Button
										type="button"
										variant="outline"
										onClick={closeDrawer}
										disabled={isMutating}
									>
										{t("profiles:form.buttons.cancel", {
											defaultValue: "Cancel",
										})}
									</Button>
								) : (
									<Button
										type="button"
										variant="outline"
										onClick={() => {
											if (step === "details") {
												closeDrawer();
											} else {
												setStep("details");
											}
										}}
										disabled={isMutating}
									>
										{step === "details"
											? t("profiles:form.buttons.cancel", {
													defaultValue: "Cancel",
												})
											: t("profiles:form.buttons.back", { defaultValue: "Back" })}
									</Button>
								)}
								{step !== "details" && !isHostAppProfile && (
									<Button
										type="button"
										variant="ghost"
										onClick={closeDrawer}
										disabled={isMutating}
									>
										{t("profiles:form.buttons.cancel", {
											defaultValue: "Cancel",
										})}
									</Button>
								)}
							</div>
							<Button
								type="submit"
								disabled={primaryDisabled}
								className="min-w-[140px]"
							>
								{primaryLabel}
							</Button>
						</div>
					</DrawerFooter>
				</form>
			</DrawerContent>
			<AlertDialog
				open={conflictDialogOpen}
				onOpenChange={(nextOpen) =>
					dispatchConflict({
						type: nextOpen ? "saveRequested" : "dialogCancelled",
					})
				}
			>
				<AlertDialogContent>
					<AlertDialogHeader>
						<AlertDialogTitle>
							{t("profiles:form.conflict.title", {
								defaultValue: "Profile modified elsewhere",
							})}
						</AlertDialogTitle>
						<AlertDialogDescription>
							{t("profiles:form.conflict.description", {
								defaultValue:
									"Load the latest Profile and discard this draft, or overwrite it with your current draft.",
							})}
						</AlertDialogDescription>
					</AlertDialogHeader>
					<ProfileAuthoringConflictSummary
						changes={serverAssignmentChanges}
						labels={{
							added: t("profiles:form.conflict.serversAdded", {
								defaultValue: "Servers added elsewhere",
							}),
							removed: t("profiles:form.conflict.serversRemoved", {
								defaultValue: "Servers removed elsewhere",
							}),
							unchanged: t("profiles:form.conflict.serversUnchanged", {
								defaultValue: "Server assignments are unchanged.",
							}),
						}}
					/>
					<AlertDialogFooter>
						<AlertDialogCancel>
							{t("profiles:form.conflict.cancel", {
								defaultValue: "Cancel",
							})}
						</AlertDialogCancel>
						<AlertDialogAction onClick={handleLoadLatest}>
							{t("profiles:form.conflict.loadLatest", {
								defaultValue: "Load latest version",
							})}
						</AlertDialogAction>
						<AlertDialogAction onClick={handleOverwrite}>
							{t("profiles:form.conflict.overwrite", {
								defaultValue: "Overwrite with current draft",
							})}
						</AlertDialogAction>
					</AlertDialogFooter>
				</AlertDialogContent>
			</AlertDialog>
		</Drawer>
	);
}
