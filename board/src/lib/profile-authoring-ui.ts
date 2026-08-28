import { ApiRequestError, configSuitsApi } from "./api";
import type {
	ProfileAuthoringSaveRequest,
	ProfileAuthoringSaveResponse,
	ProfileAuthoringView,
	ProfileMode,
	ServerSummary,
	WorkflowGuidanceSaveRequest,
} from "./types";

export type ProfileAuthoringSubmission =
	| { status: "saved"; data: ProfileAuthoringSaveResponse }
	| {
			status: "conflict";
			error: ApiRequestError;
			latestView: ProfileAuthoringView;
	  };

export async function submitProfileAuthoring(
	request: ProfileAuthoringSaveRequest,
): Promise<ProfileAuthoringSubmission> {
	try {
		return {
			status: "saved",
			data: await configSuitsApi.saveAuthoring(request),
		};
	} catch (error) {
		if (
			error instanceof ApiRequestError &&
			error.status === 409 &&
			error.code === "profile_authoring_changed" &&
			request.id
		) {
			return {
				status: "conflict",
				error,
				latestView: await configSuitsApi.getAuthoringView(request.id),
			};
		}
		throw error;
	}
}

export interface ProfileFormDraft {
	name: string;
	skill_name: string;
	package_distribution: "" | "symlink" | "copy";
	description: string;
	suit_type: string;
	priority: number;
	is_active: boolean;
	is_default: boolean;
	clone_from_id: string;
	profile_mode?: ProfileMode;
}

export function isValidSkillName(value: string): boolean {
	return value.length <= 64 && /^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(value);
}

export function isCapabilityWorkingSetProfile(profile: {
	profile_mode?: ProfileMode | null;
}): boolean {
	return profile.profile_mode !== "workflow";
}

export interface ProfileAuthoringResetIdentity {
	open: boolean;
	mode: "create" | "edit";
	profileId: string | null;
}

export function shouldResetProfileAuthoringState(
	previous: ProfileAuthoringResetIdentity,
	next: ProfileAuthoringResetIdentity,
): boolean {
	return (
		(!previous.open && next.open) ||
		previous.mode !== next.mode ||
		previous.profileId !== next.profileId
	);
}

interface BuildProfileAuthoringSaveRequestInput {
	mode: "create" | "edit";
	profileId: string | null;
	draft: ProfileFormDraft;
	serverIds: string[];
	authoringView?: ProfileAuthoringView;
	expectedAuthoringGeneration?: number;
	workflowGuidance?: WorkflowGuidanceSaveRequest;
}

export function buildProfileAuthoringSaveRequest({
	mode,
	profileId,
	draft,
	serverIds,
	authoringView,
	expectedAuthoringGeneration,
	workflowGuidance,
}: BuildProfileAuthoringSaveRequestInput): ProfileAuthoringSaveRequest {
	if (mode === "edit" && (!profileId || !authoringView)) {
		throw new Error("Profile authoring state is not loaded");
	}
	return {
		id: mode === "edit" ? profileId : null,
		expected_authoring_generation:
			mode === "edit"
				? (expectedAuthoringGeneration ??
					authoringView!.profile.authoring_generation)
				: null,
		name: draft.name,
		description: draft.description || null,
		profile_type: draft.suit_type,
		priority: draft.priority,
		is_active: draft.is_active,
		is_default: draft.is_default,
		server_ids: serverIds,
		clone_from_id:
			mode === "create" &&
			draft.clone_from_id &&
			draft.clone_from_id !== "none"
				? draft.clone_from_id
				: null,
			...(draft.profile_mode === "workflow"
				? {
						profile_mode: "workflow" as const,
						skill_name: draft.skill_name.trim(),
						package_distribution: draft.package_distribution
							? draft.package_distribution
							: null,
						...(workflowGuidance ? { workflow_guidance: workflowGuidance } : {}),
					}
				: {}),
	};
}

export interface ProfileServerAssignmentChange {
	id: string;
	name: string;
}

export interface ProfileServerAssignmentChanges {
	added: ProfileServerAssignmentChange[];
	removed: ProfileServerAssignmentChange[];
}

export function buildProfileServerAssignmentChanges(
	baseline: ProfileAuthoringView,
	latest: ProfileAuthoringView,
	servers: ServerSummary[],
): ProfileServerAssignmentChanges {
	const baselineIds = new Set(baseline.server_ids);
	const latestIds = new Set(latest.server_ids);
	const serverNames = new Map(
		servers.map((server) => [server.id, server.name || server.id]),
	);
	const present = (id: string): ProfileServerAssignmentChange => ({
		id,
		name: serverNames.get(id) ?? id,
	});
	const byName = (
		a: ProfileServerAssignmentChange,
		b: ProfileServerAssignmentChange,
	) => a.name.localeCompare(b.name);

	return {
		added: latest.server_ids
			.filter((id) => !baselineIds.has(id))
			.map(present)
			.sort(byName),
		removed: baseline.server_ids
			.filter((id) => !latestIds.has(id))
			.map(present)
			.sort(byName),
	};
}

export function profileFormDraftFromAuthoringView(
	view: ProfileAuthoringView,
): ProfileFormDraft {
	return {
		name: view.profile.name,
			skill_name: view.skill_name ?? "",
			package_distribution: view.package_distribution ?? "",
			description: view.profile.description || "",
		suit_type: view.profile.suit_type,
		priority: view.profile.priority,
		is_active: view.profile.is_active,
		is_default: view.profile.is_default,
		clone_from_id: "none",
		profile_mode: view.profile_mode ?? view.profile.profile_mode ?? "capability",
	};
}

export interface ProfileAuthoringConflictState {
	baselineView: ProfileAuthoringView | null;
	latestView: ProfileAuthoringView | null;
	dialogOpen: boolean;
}

export type ProfileAuthoringConflictAction =
	| { type: "reset" }
	| { type: "baselineLoaded"; view: ProfileAuthoringView }
	| { type: "conflictReceived"; view: ProfileAuthoringView }
	| { type: "dialogCancelled" }
	| { type: "saveRequested" }
	| { type: "loadLatest" }
	| { type: "overwriteStarted" };

export function createProfileAuthoringConflictState(): ProfileAuthoringConflictState {
	return {
		baselineView: null,
		latestView: null,
		dialogOpen: false,
	};
}

export function reduceProfileAuthoringConflict(
	state: ProfileAuthoringConflictState,
	action: ProfileAuthoringConflictAction,
): ProfileAuthoringConflictState {
	switch (action.type) {
		case "reset":
			return createProfileAuthoringConflictState();
		case "baselineLoaded":
			return state.baselineView ? state : { ...state, baselineView: action.view };
		case "conflictReceived":
			return { ...state, latestView: action.view, dialogOpen: true };
		case "dialogCancelled":
		case "overwriteStarted":
			return { ...state, dialogOpen: false };
		case "saveRequested":
			return state.latestView ? { ...state, dialogOpen: true } : state;
		case "loadLatest":
			if (!state.latestView) {
				return state;
			}
			return {
				baselineView: state.latestView,
				latestView: null,
				dialogOpen: false,
			};
	}
}

export interface ProfileServerPresentationLabels {
	globalStatus: string;
	catalog: string;
	enabled: string;
	disabled: string;
	notReported: string;
	ready: string;
	unavailable: string;
	notObserved: string;
}

export interface ProfileServerTransferItem {
	id: string;
	name: string;
	description: string;
	descriptionAriaLabel: string;
	type?: string;
}

export function buildProfileServerTransferItems(
	servers: ServerSummary[],
	labels: ProfileServerPresentationLabels,
): ProfileServerTransferItem[] {
	return servers
		.map((server) => {
			const configuration =
				server.enabled === true
					? labels.enabled
					: server.enabled === false
						? labels.disabled
						: labels.notReported;
			const catalog = !server.capability
				? labels.notObserved
				: server.capability.snapshotState === "ready"
					? labels.ready
					: labels.unavailable;
			return {
				id: server.id,
				name: server.name || server.id,
				description: `${configuration} • ${catalog}`,
				descriptionAriaLabel: `${labels.globalStatus}: ${configuration}. ${labels.catalog}: ${catalog}.`,
				type: server.server_type,
			};
		})
		.sort((a, b) => a.name.localeCompare(b.name));
}
