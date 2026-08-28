import { afterEach, describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import "../lib/i18n/index";
import * as authoringUi from "../lib/profile-authoring-ui";
import type {
	ProfileAuthoringView,
	ProfileAuthoringSaveRequest,
	ServerSummary,
} from "../lib/types";
import {
	ProfileAuthoringConflictSummary,
	ProfileServerTransfer,
} from "./profile-form-drawer";

const originalFetch = globalThis.fetch;

afterEach(() => {
	globalThis.fetch = originalFetch;
});

const createRequest: ProfileAuthoringSaveRequest = {
	id: null,
	expected_authoring_generation: null,
	name: "Focused work",
	description: "Only the tools needed for focused work",
	profile_type: "shared",
	priority: 50,
	is_active: true,
	is_default: false,
	server_ids: ["server-a", "server-b"],
	clone_from_id: null,
};

const savedProfile = {
	id: "profile-a",
	name: createRequest.name,
	description: createRequest.description,
	profile_type: createRequest.profile_type,
	priority: createRequest.priority,
	is_active: createRequest.is_active,
	is_default: createRequest.is_default,
	authoring_generation: 0,
	role: "user",
	allowed_operations: ["update", "delete"],
};

const serverPresentationLabels = {
	globalStatus: "Global status",
	catalog: "Catalog",
	enabled: "Enabled",
	disabled: "Disabled",
	notReported: "Not reported",
	ready: "Ready",
	unavailable: "Unavailable",
	notObserved: "Not observed",
};

function successResponse(profile = savedProfile): Response {
	return Response.json({ success: true, data: { profile } });
}

describe("ProfileFormDrawer authoring", () => {
	test("creates a Profile with one atomic authoring request", async () => {
		const requests: Array<{ url: string; init?: RequestInit }> = [];
		globalThis.fetch = async (input, init) => {
			requests.push({ url: String(input), init });
			return successResponse();
		};

		await authoringUi.submitProfileAuthoring(createRequest);

		expect(requests).toHaveLength(1);
		expect(requests[0]?.url).toEndWith("/api/mcp/profile/authoring/save");
		expect(requests[0]?.init?.method).toBe("POST");
		expect(JSON.parse(String(requests[0]?.init?.body))).toEqual(createRequest);
	});

	test("creates a Workflow Profile as inactive and non-default", () => {
		const request = authoringUi.buildProfileAuthoringSaveRequest({
			mode: "create",
			profileId: null,
				draft: {
					...createRequest,
					skill_name: "workflow-test",
					package_distribution: "symlink",
					suit_type: "shared",
				clone_from_id: "none",
				profile_mode: "workflow",
				is_active: false,
				is_default: false,
			},
			serverIds: ["server-a"],
			workflowGuidance: {
				expected_specification_revision: null,
				validation_notes: "Verify source evidence.",
				avoid_rules: "Do not infer missing facts.",
			},
		});

			expect(request).toMatchObject({
				profile_mode: "workflow",
				skill_name: "workflow-test",
				package_distribution: "symlink",
				is_active: false,
			is_default: false,
			workflow_guidance: {
				expected_specification_revision: null,
				validation_notes: "Verify source evidence.",
				avoid_rules: "Do not infer missing facts.",
			},
		});
	});

	test("clears Skill package distribution when unset", () => {
		const request = authoringUi.buildProfileAuthoringSaveRequest({
			mode: "create",
			profileId: null,
			draft: {
				...createRequest,
				skill_name: "workflow-test",
				package_distribution: "",
				suit_type: "shared",
				clone_from_id: "none",
				profile_mode: "workflow",
				is_active: false,
				is_default: false,
			},
			serverIds: ["server-a"],
		});

		expect(request.package_distribution).toBeNull();
	});

	test("validates Workflow Skill names before submission", () => {
		expect(authoringUi.isValidSkillName("screenshot")).toBe(true);
		expect(authoringUi.isValidSkillName("screenshot-workflow-2")).toBe(true);
		expect(authoringUi.isValidSkillName("Screenshot")).toBe(false);
		expect(authoringUi.isValidSkillName("screenshot_workflow")).toBe(false);
		expect(authoringUi.isValidSkillName("-screenshot")).toBe(false);
		expect(authoringUi.isValidSkillName("screenshot-")).toBe(false);
		expect(authoringUi.isValidSkillName("screenshot--workflow")).toBe(false);
		expect(authoringUi.isValidSkillName("a".repeat(65))).toBe(false);
	});

	test("updates metadata servers activation and default with one request", async () => {
		const request: ProfileAuthoringSaveRequest = {
			...createRequest,
			id: "profile-a",
			expected_authoring_generation: 12,
			name: "Focused work v2",
			description: "Updated description",
			is_active: false,
			is_default: true,
			server_ids: ["server-b", "server-c"],
		};
		const requests: Array<{ url: string; init?: RequestInit }> = [];
		globalThis.fetch = async (input, init) => {
			requests.push({ url: String(input), init });
			return successResponse({
				...savedProfile,
				name: request.name,
				description: request.description,
				is_active: request.is_active,
				is_default: request.is_default,
				authoring_generation: 13,
			});
		};

		await authoringUi.submitProfileAuthoring(request);

		expect(requests).toHaveLength(1);
		expect(JSON.parse(String(requests[0]?.init?.body))).toEqual(request);
	});

	test("keeps the draft open after profile_authoring_changed", async () => {
		const request: ProfileAuthoringSaveRequest = {
			...createRequest,
			id: "profile-a",
			expected_authoring_generation: 12,
			name: "Unsaved local name",
			description: "Unsaved local description",
			server_ids: ["server-local"],
		};
		const requests: string[] = [];
		globalThis.fetch = async (input) => {
			const url = String(input);
			requests.push(url);
			if (url.endsWith("/api/mcp/profile/authoring/save")) {
				return Response.json(
					{
						error: {
							message: "Profile was changed by another author",
							status: 409,
							code: "profile_authoring_changed",
							details: { currentAuthoringGeneration: 13 },
						},
					},
					{ status: 409 },
				);
			}
			return Response.json({
				success: true,
				data: {
					profile: { ...savedProfile, authoring_generation: 13 },
					server_ids: ["server-remote"],
				},
			});
		};

		const result = await authoringUi.submitProfileAuthoring(request);
		if (result.status !== "conflict") {
			throw new Error("Expected an authoring conflict");
		}
		expect(result).toMatchObject({
			status: "conflict",
			latestView: {
				profile: { authoring_generation: 13 },
				server_ids: ["server-remote"],
			},
		});
		expect(request).toMatchObject({
			name: "Unsaved local name",
			description: "Unsaved local description",
			server_ids: ["server-local"],
		});
		expect(requests).toHaveLength(2);
		expect(requests[1]).toContain(
			"/api/mcp/profile/authoring/view?id=profile-a",
		);
	});

	test("uses the latest generation only for an explicit overwrite", async () => {
		let submittedBody: unknown;
		globalThis.fetch = async (_input, init) => {
			submittedBody = init?.body ? JSON.parse(String(init.body)) : undefined;
			return successResponse({ ...savedProfile, authoring_generation: 14 });
		};
			const draft: authoringUi.ProfileFormDraft = {
				name: "Unsaved local name",
				skill_name: "",
				package_distribution: "",
				description: "Unsaved local description",
			suit_type: "shared",
			priority: 50,
			is_active: true,
			is_default: false,
			clone_from_id: "none",
		};
		const request = authoringUi.buildProfileAuthoringSaveRequest({
			mode: "edit",
			profileId: "profile-a",
			draft,
			serverIds: ["server-local"],
			authoringView: {
				profile: { ...savedProfile, authoring_generation: 12 },
				server_ids: ["server-old"],
			},
			expectedAuthoringGeneration: 13,
		});
		await authoringUi.submitProfileAuthoring(request);

		expect(submittedBody).toMatchObject({
			expected_authoring_generation: 13,
			name: "Unsaved local name",
			description: "Unsaved local description",
			server_ids: ["server-local"],
		});
	});

	test("keeps the original generation for a normal save after conflict", () => {
		const request = authoringUi.buildProfileAuthoringSaveRequest({
			mode: "edit",
			profileId: "profile-a",
			draft: {
				name: "Unsaved local name",
				description: "Unsaved local description",
				suit_type: "shared",
				priority: 50,
				is_active: true,
				is_default: false,
				clone_from_id: "none",
			},
			serverIds: ["server-local"],
			authoringView: {
				profile: { ...savedProfile, authoring_generation: 12 },
				server_ids: ["server-old"],
			},
		});

		expect(request.expected_authoring_generation).toBe(12);
	});

	test("shows added and removed servers when the assignment count is unchanged", () => {
		const baseline: ProfileAuthoringView = {
			profile: { ...savedProfile, authoring_generation: 12 },
			server_ids: ["server-a", "server-b"],
		};
		const latest: ProfileAuthoringView = {
			profile: { ...savedProfile, authoring_generation: 13 },
			server_ids: ["server-b", "server-c"],
		};
		const servers: ServerSummary[] = [
			{ id: "server-a", name: "Server A" },
			{ id: "server-b", name: "Server B" },
			{ id: "server-c", name: "Server C" },
		];
		const changes = authoringUi.buildProfileServerAssignmentChanges(
			baseline,
			latest,
			servers,
		);

		expect(changes).toEqual({
			added: [{ id: "server-c", name: "Server C" }],
			removed: [{ id: "server-a", name: "Server A" }],
		});

		const markup = renderToStaticMarkup(
			<ProfileAuthoringConflictSummary
				changes={changes}
				labels={{
					added: "Added",
					removed: "Removed",
					unchanged: "Server assignments are unchanged.",
				}}
			/>,
		);
		expect(markup).toContain("Added");
		expect(markup).toContain("Server C");
		expect(markup).toContain("Removed");
		expect(markup).toContain("Server A");
	});

	test("keeps conflict decisions explicit across cancel load and repeated conflicts", () => {
		const baseline: ProfileAuthoringView = {
			profile: { ...savedProfile, authoring_generation: 12 },
			server_ids: ["server-a"],
		};
		const firstLatest: ProfileAuthoringView = {
			profile: { ...savedProfile, authoring_generation: 13 },
			server_ids: ["server-b"],
		};
		const secondLatest: ProfileAuthoringView = {
			profile: { ...savedProfile, authoring_generation: 14 },
			server_ids: ["server-c"],
		};
		let state = authoringUi.createProfileAuthoringConflictState();

		state = authoringUi.reduceProfileAuthoringConflict(state, {
			type: "baselineLoaded",
			view: baseline,
		});
		state = authoringUi.reduceProfileAuthoringConflict(state, {
			type: "baselineLoaded",
			view: firstLatest,
		});
		expect(state.baselineView).toEqual(baseline);

		state = authoringUi.reduceProfileAuthoringConflict(state, {
			type: "conflictReceived",
			view: firstLatest,
		});
		expect(state).toMatchObject({
			baselineView: baseline,
			latestView: firstLatest,
			dialogOpen: true,
		});

		state = authoringUi.reduceProfileAuthoringConflict(state, {
			type: "dialogCancelled",
		});
		expect(state).toMatchObject({ latestView: firstLatest, dialogOpen: false });
		state = authoringUi.reduceProfileAuthoringConflict(state, {
			type: "saveRequested",
		});
		expect(state.dialogOpen).toBeTrue();

		state = authoringUi.reduceProfileAuthoringConflict(state, {
			type: "overwriteStarted",
		});
		expect(state).toMatchObject({ latestView: firstLatest, dialogOpen: false });
		state = authoringUi.reduceProfileAuthoringConflict(state, {
			type: "conflictReceived",
			view: secondLatest,
		});
		expect(state).toMatchObject({ latestView: secondLatest, dialogOpen: true });

		state = authoringUi.reduceProfileAuthoringConflict(state, {
			type: "loadLatest",
		});
		expect(state).toEqual({
			baselineView: secondLatest,
			latestView: null,
			dialogOpen: false,
		});
		expect(authoringUi.profileFormDraftFromAuthoringView(secondLatest)).toMatchObject({
			name: secondLatest.profile.name,
			description: secondLatest.profile.description,
		});
	});

	test("does not reset a draft when the same Profile prop refreshes", () => {
		expect(
			authoringUi.shouldResetProfileAuthoringState(
				{ open: true, mode: "edit", profileId: "profile-a" },
				{ open: true, mode: "edit", profileId: "profile-a" },
			),
		).toBeFalse();
	});

	test("resets a draft on reopen or a different Profile", () => {
		expect(
			authoringUi.shouldResetProfileAuthoringState(
				{ open: false, mode: "edit", profileId: "profile-a" },
				{ open: true, mode: "edit", profileId: "profile-a" },
			),
		).toBeTrue();
		expect(
			authoringUi.shouldResetProfileAuthoringState(
				{ open: true, mode: "edit", profileId: "profile-a" },
				{ open: true, mode: "edit", profileId: "profile-b" },
			),
		).toBeTrue();
	});

	test("does not render Status Unknown for a server without aggregate status", () => {
		const servers: ServerSummary[] = [
			{
				id: "server-a",
				name: "Server A",
				server_type: "very-long-server-transport-type",
				enabled: true,
				capability: {
					snapshotState: "invalidated",
					revision: 4,
					observedAt: "2026-08-07T00:00:00Z",
					tools: {
						declaration: "supported",
						inventory: "failed",
						currentCount: 0,
						currentAvailable: false,
					},
					prompts: {
						declaration: "unknown",
						inventory: "unknown",
						currentCount: 0,
						currentAvailable: false,
					},
					resources: {
						declaration: "unknown",
						inventory: "unknown",
						currentCount: 0,
						currentAvailable: false,
					},
					resourceTemplates: {
						declaration: "unknown",
						inventory: "unknown",
						currentCount: 0,
						currentAvailable: false,
					},
				},
			},
		];

		const items = authoringUi.buildProfileServerTransferItems(
			servers,
			serverPresentationLabels,
		);

		expect(items[0]?.description).toBe("Enabled • Unavailable");
		expect(items[0]?.descriptionAriaLabel).toBe(
			"Global status: Enabled. Catalog: Unavailable.",
		);
		expect(items[0]?.description).not.toContain("Profile:");

		const markup = renderToStaticMarkup(
			<ProfileServerTransfer
				servers={servers}
				selectedServerIds={[]}
				labels={serverPresentationLabels}
				onChange={() => undefined}
				leftTitle="Available Servers"
				rightTitle="Profile Servers"
				searchPlaceholder="Search servers..."
				emptyText="No data"
				disabled={false}
				loading={false}
			/>,
		);
		expect(markup).toContain("Enabled • Unavailable");
		expect(markup).toContain("max-w-[50%] shrink-0 truncate");
		expect(markup).toContain(
			'<span class="sr-only">Global status: Enabled. Catalog: Unavailable.</span>',
		);
		expect(markup).not.toContain("Configuration:");
		expect(markup).not.toContain("Profile: Not selected");
		expect(markup).toContain(
			"grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)]",
		);
		expect(markup).toContain("Available Servers (1)");
		expect(markup).not.toContain("Available Servers (0/1)");
		expect(markup).toContain("Profile Servers (0)");
		expect(markup).not.toContain("Profile Servers (0/0)");
	});

});

	describe("capability working-set filter", () => {
		test("keeps Capability profiles and drops Workflow from Hosted/Transparent working sets", () => {
			expect(
				authoringUi.isCapabilityWorkingSetProfile({ profile_mode: "capability" }),
			).toBeTrue();
			expect(authoringUi.isCapabilityWorkingSetProfile({})).toBeTrue();
			expect(
				authoringUi.isCapabilityWorkingSetProfile({ profile_mode: "workflow" }),
			).toBeFalse();
		});
	});
