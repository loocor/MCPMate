import type { WorkflowBinding, WorkflowSpecification, WorkflowStep } from "./types";

export type WorkflowCapabilityKind = "tool" | "prompt" | "resource" | "template";

export interface WorkflowCapabilityOption {
	ref_id: string;
	server_id: string;
	label: string;
	description?: string;
	kind: WorkflowCapabilityKind;
}

interface WorkflowCapabilityBatch {
	server: { id: string; name: string };
	lists: {
		tools: { items?: unknown[] };
		resources: { items?: unknown[] };
		prompts: { items?: unknown[] };
		templates: { items?: unknown[] };
	};
}

function readString(
	item: Record<string, unknown>,
	keys: string[],
): string | undefined {
	for (const key of keys) {
		const value = item[key];
		if (typeof value === "string" && value.trim()) {
			return value;
		}
	}
	return undefined;
}

function option(
	item: unknown,
	refKeys: string[],
	labelKeys: string[],
	serverId: string,
	kind: WorkflowCapabilityKind,
): WorkflowCapabilityOption | null {
	if (!item || typeof item !== "object" || Array.isArray(item)) {
		return null;
	}
	const record = item as Record<string, unknown>;
	const refId = readString(record, refKeys);
	if (!refId) {
		return null;
	}
	const label = readString(record, labelKeys) ?? refId;
	const description = readString(record, ["description"]);
	return description
		? { ref_id: refId, server_id: serverId, label, description, kind }
		: { ref_id: refId, server_id: serverId, label, kind };
}

export function buildWorkflowCapabilityOptions(
	batches: WorkflowCapabilityBatch[],
): WorkflowCapabilityOption[] {
	const options: WorkflowCapabilityOption[] = [];
	for (const { server, lists } of batches) {
		for (const item of lists.tools.items ?? []) {
			const mapped = option(
				item,
				["ref_id", "id"],
				["unique_name", "tool_name", "name"],
				server.id,
				"tool",
			);
			if (mapped) options.push(mapped);
		}
		for (const item of lists.resources.items ?? []) {
			const mapped = option(
				item,
				["ref_id", "id"],
				["unique_uri", "resource_uri", "uri"],
				server.id,
				"resource",
			);
			if (mapped) options.push(mapped);
		}
		for (const item of lists.prompts.items ?? []) {
			const mapped = option(
				item,
				["ref_id", "id"],
				["unique_name", "prompt_name", "name"],
				server.id,
				"prompt",
			);
			if (mapped) options.push(mapped);
		}
		for (const item of lists.templates.items ?? []) {
			const mapped = option(
				item,
				["ref_id", "id"],
				["unique_uri_template", "uri_template", "uriTemplate"],
				server.id,
				"template",
			);
			if (mapped) options.push(mapped);
		}
	}
	return options;
}

export function resolveWorkflowCapabilityKind(
	name: string,
	options: WorkflowCapabilityOption[],
): WorkflowCapabilityKind {
	const match = options.find((option) => option.label === name);
	if (match) return match.kind;
	if (name.includes("://") || name.includes("/")) return "resource";
	return "tool";
}

export interface WorkflowStepDraft {
    step_id?: string;
	title: string;
	description: string;
	bindings: WorkflowBinding[];
}

export function createEmptyWorkflowStep(): WorkflowStepDraft {
	return {
		step_id: crypto.randomUUID(),
		title: "",
		description: "",
		bindings: [],
	};
}

export function isUnsavedWorkflowStep(
	step: WorkflowStepDraft | undefined,
	persistedStepIds: ReadonlySet<string>,
): boolean {
	if (!step?.step_id) return true;
	return !persistedStepIds.has(step.step_id);
}

export function withSingleWorkflowCapabilityBinding(
	step: WorkflowStepDraft,
	refId: string | null,
): WorkflowStepDraft {
	return {
		...step,
		bindings: refId
			? [
					{
						ref_id: refId,
						binding_policy:
							step.bindings.find((binding) => binding.ref_id === refId)
								?.binding_policy ?? "meta_on_demand",
					},
				]
			: [],
	};
}

export function workflowDraftFromSpecification(
	specification: WorkflowSpecification | undefined,
): WorkflowStepDraft[] {
	return (specification?.steps ?? []).map((step) => ({
		step_id: step.step_id,
		title: step.title,
		description: step.description ?? "",
		bindings: step.bindings.map((binding) => ({ ...binding })),
	}));
}

export function serializeWorkflowSteps(steps: WorkflowStepDraft[]): WorkflowStep[] {
	return steps.map((step) => ({
		step_id: step.step_id ?? crypto.randomUUID(),
		title: step.title.trim(),
		description: step.description.trim() || null,
		bindings: step.bindings.map((binding) => ({
			ref_id: binding.ref_id,
			binding_policy: binding.binding_policy,
		})),
	}));
}
