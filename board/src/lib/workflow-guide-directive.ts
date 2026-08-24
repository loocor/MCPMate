// Workflow Guide parse/render helpers for Board authoring. Projection strings and
// SKILL.md output live in backend/config/projection.json5; preview/save use the API.

import type { WorkflowBindingPolicy } from "./types";

export interface WorkflowGuideCapabilityBlock {
  name: string;
  exposure: "direct" | "meta_on_demand";
  guide: string;
  startLine: number;
  endLine: number;
}

export interface WorkflowGuideExternalBlock {
  title: string;
  path: string;
  guide: string;
  startLine: number;
  endLine: number;
}

export interface WorkflowGuideParseResult {
  headings: Array<{ level: number; text: string; offset: number }>;
  capabilities: WorkflowGuideCapabilityBlock[];
  externals: WorkflowGuideExternalBlock[];
  errors: string[];
}

export interface WorkflowGuideDocumentCell {
  id: string;
  kind: "markdown" | "external_reference" | "capability";
  source: string;
  startOffset: number;
  endOffset: number;
  capability?: WorkflowGuideCapabilityBlock;
  externalReference?: { title: string; relativePath: string; guide?: string };
}

const CAPABILITY_START = /^:::capability\s+(\{.*\})\s*$/;
const EXTERNAL_START = /^:::external\s+(\{.*\})\s*$/;
const DIRECTIVE_END = /^:::\s*$/;
const STANDALONE_EXTERNAL_REFERENCE =
  /^\s*\[([^\]\n]+)\]\((references\/[^\s)#]+\.md)(?:#[^\s)]+)?\)\s*$/;
const UUID_REFERENCE = /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/i;

/** Mirrors backend/config/projection.json5 for test-only projection rendering. */
const TEST_PROJECTION = {
  capability: {
    listThreshold: 2,
    sectionIntro: {
      base: "The following steps are MCP server capability invocations.",
      onDemand:
        "Steps marked `(on-demand)` must be inspected with `mcpmate_ucan_details`, then invoked with `mcpmate_ucan_call`.",
      direct: "Steps marked `(direct)` can be invoked by tool name.",
    },
    item: {
      directOnly: "Use `{name}` (direct).",
      onDemandOnly: "Invoke `{name}` (on-demand).",
      directWithGuide: "`{name}` (direct): {guide}",
      onDemandWithGuide: "`{name}` (on-demand): {guide}",
    },
  },
  external: {
    withGuide: "{guide}\n\n[{title}]({path})",
    linkOnly: "[{title}]({path})",
  },
} as const;

export function buildLineOffsets(markdown: string): number[] {
  const lineOffsets = [0];
  for (let index = 0; index < markdown.length; index += 1) {
    if (markdown[index] === "\n") {
      lineOffsets.push(index + 1);
    }
  }
  return lineOffsets;
}

export function stripLeadingSkillFrontMatter(content: string) {
  if (!content.startsWith("---\n")) return { body: content };
  const closingOffset = content.indexOf("\n---\n", 4);
  if (closingOffset < 0) return { body: content };
  return { body: content.slice(closingOffset + 5) };
}

export function parseWorkflowGuide(markdown: string): WorkflowGuideParseResult {
  const headings: Array<{ level: number; text: string; offset: number }> = [];
  const capabilities: WorkflowGuideCapabilityBlock[] = [];
  const externals: WorkflowGuideExternalBlock[] = [];
  const errors: string[] = [];
  const lineOffsets = buildLineOffsets(markdown);
  const lines = markdown.split("\n");
  let fence: MarkdownFence | null = null;
  let activeCapability: {
    name: string;
    exposure: "direct" | "meta_on_demand";
    startLine: number;
    lines: string[];
  } | null = null;
  let activeExternal: {
    title: string;
    path: string;
    startLine: number;
    lines: string[];
  } | null = null;

  for (const [index, line] of lines.entries()) {
    const lineNumber = index + 1;
    if (fence) {
      if (closesFence(line, fence)) {
        fence = null;
        if (activeCapability) activeCapability.lines.push(line);
        if (activeExternal) activeExternal.lines.push(line);
        continue;
      }
      if (containsReservedWorkflowGuideSyntax(line)) {
        errors.push(`line ${lineNumber}: Workflow Guide directives and references are not allowed in fenced code`);
      }
      if (activeCapability) activeCapability.lines.push(line);
      if (activeExternal) activeExternal.lines.push(line);
      continue;
    }
    const openingFence = parseOpeningFence(line);
    if (openingFence) {
      fence = openingFence;
      if (activeCapability) activeCapability.lines.push(line);
      if (activeExternal) activeExternal.lines.push(line);
      continue;
    }

    if (activeCapability) {
      if (DIRECTIVE_END.test(line)) {
        capabilities.push({
          name: activeCapability.name,
          exposure: activeCapability.exposure,
          guide: activeCapability.lines.join("\n").trim(),
          startLine: activeCapability.startLine,
          endLine: lineNumber,
        });
        activeCapability = null;
      } else {
        activeCapability.lines.push(line);
      }
      continue;
    }

    if (activeExternal) {
      if (DIRECTIVE_END.test(line)) {
        externals.push({
          title: activeExternal.title,
          path: activeExternal.path,
          guide: activeExternal.lines.join("\n").trim(),
          startLine: activeExternal.startLine,
          endLine: lineNumber,
        });
        activeExternal = null;
      } else {
        activeExternal.lines.push(line);
      }
      continue;
    }

    const leadingWhitespace = line.length - line.trimStart().length;
    const trimmed = line.trimStart();
    const lineStartHeading = /^(#{1,6}) (?!$)(.+?)\s*$/.exec(trimmed);
    if (lineStartHeading) {
      headings.push({
        level: lineStartHeading[1].length,
        text: lineStartHeading[2].trim(),
        offset: lineOffsets[index] + leadingWhitespace,
      });
    }
    const capabilityStart = CAPABILITY_START.exec(line);
    if (capabilityStart) {
      try {
        const header = JSON.parse(capabilityStart[1]) as { name?: unknown; exposure?: unknown };
        if (typeof header.name !== "string" || !header.name.trim()) {
          errors.push(`line ${lineNumber}: Capability name must not be empty`);
        } else if (header.exposure !== "direct" && header.exposure !== "meta_on_demand") {
          errors.push(`line ${lineNumber}: Capability exposure must be direct or meta_on_demand`);
        } else {
          activeCapability = {
            name: header.name,
            exposure: header.exposure,
            startLine: lineNumber,
            lines: [],
          };
        }
      } catch (error) {
        errors.push(
          `line ${lineNumber}: invalid Capability directive: ${error instanceof Error ? error.message : String(error)}`,
        );
      }
      continue;
    }
    const externalStart = EXTERNAL_START.exec(line);
    if (externalStart) {
      try {
        const header = JSON.parse(externalStart[1]) as { title?: unknown; path?: unknown };
        if (typeof header.title !== "string" || !header.title.trim()) {
          errors.push(`line ${lineNumber}: External document title must not be empty`);
        } else if (typeof header.path !== "string" || !header.path.startsWith("references/") || !header.path.endsWith(".md")) {
          errors.push(`line ${lineNumber}: External document path must be a references/*.md file`);
        } else {
          activeExternal = {
            title: header.title.trim(),
            path: header.path,
            startLine: lineNumber,
            lines: [],
          };
        }
      } catch (error) {
        errors.push(
          `line ${lineNumber}: invalid External document directive: ${error instanceof Error ? error.message : String(error)}`,
        );
      }
      continue;
    }
    if (line.trimStart().startsWith(":::capability")) {
      errors.push(`line ${lineNumber}: invalid Capability directive; expected JSON name and exposure`);
      continue;
    }
    if (line.trimStart().startsWith(":::external")) {
      errors.push(`line ${lineNumber}: invalid External document directive; expected JSON title and path`);
      continue;
    }
    if (DIRECTIVE_END.test(line)) {
      errors.push(`line ${lineNumber}: directive end has no matching start`);
      continue;
    }
  }

  if (activeCapability) {
    errors.push(`line ${activeCapability.startLine}: Capability '${activeCapability.name}' directive is not closed`);
  }
  if (activeExternal) {
    errors.push(`line ${activeExternal.startLine}: External document '${activeExternal.title}' directive is not closed`);
  }
  lines.forEach((line, index) => {
    if (UUID_REFERENCE.test(line)) errors.push(`line ${index + 1}: opaque identifiers are not allowed in a Workflow Guide`);
    if (line.includes("skill://")) errors.push(`line ${index + 1}: skill:// references are not allowed in a Workflow Guide`);
  });
  return { headings, capabilities, externals, errors };
}

function interpolateProjectionTemplate(
  template: string,
  values: Record<string, string>,
): string {
  let output = template;
  for (const [key, value] of Object.entries(values)) {
    output = output.replaceAll(`{${key}}`, value);
  }
  return output;
}

export type { WorkflowBindingPolicy } from "./types";

function capabilityExposureSummary(
  capabilities: Array<{ name: string; exposure: WorkflowBindingPolicy }>,
  effectiveExposure: Map<string, WorkflowBindingPolicy>,
): { hasMetaOnDemand: boolean; hasDirect: boolean } {
  let hasMetaOnDemand = false;
  let hasDirect = false;
  for (const capability of capabilities) {
    const exposure = effectiveExposure.get(capability.name) ?? capability.exposure;
    if (exposure === "meta_on_demand") {
      hasMetaOnDemand = true;
    } else {
      hasDirect = true;
    }
  }
  return { hasMetaOnDemand, hasDirect };
}

function mcpCapabilitySectionIntro(hasMetaOnDemand: boolean, hasDirect: boolean): string {
  const sentences = [TEST_PROJECTION.capability.sectionIntro.base];
  if (hasMetaOnDemand) {
    sentences.push(TEST_PROJECTION.capability.sectionIntro.onDemand);
  }
  if (hasDirect) {
    sentences.push(TEST_PROJECTION.capability.sectionIntro.direct);
  }
  return sentences.join(" ");
}

export function effectiveExposureByName(
  capabilities: Array<{ name: string; exposure: WorkflowBindingPolicy }>,
): Map<string, WorkflowBindingPolicy> {
  const effective = new Map<string, WorkflowBindingPolicy>();
  for (const capability of capabilities) {
    if (effective.get(capability.name) === "direct") continue;
    effective.set(capability.name, capability.exposure);
  }
  return effective;
}

// Projection strings live in backend/config/projection.json5.
// Preview and save use the backend API; keep this mirror aligned for unit tests only.
export function renderWorkflowSkill(
  markdown: string,
  effectiveExposure?: Map<string, WorkflowBindingPolicy>,
) {
  const parsed = parseWorkflowGuide(markdown);
  if (parsed.errors.length > 0) return { markdown: "", errors: parsed.errors };

  const capabilitiesByStartLine = new Map(
    parsed.capabilities.map((capability) => [capability.startLine, capability]),
  );
  const externalsByStartLine = new Map(
    parsed.externals.map((external) => [external.startLine, external]),
  );
  const resolvedExposure = effectiveExposure ?? effectiveExposureByName(parsed.capabilities);
  const lines = markdown.split("\n");
  const output: string[] = [];
  let capabilitySectionShown = false;
  const useCapabilityListStyle =
    parsed.capabilities.length >= TEST_PROJECTION.capability.listThreshold;
  const { hasMetaOnDemand, hasDirect } = capabilityExposureSummary(
    parsed.capabilities,
    resolvedExposure,
  );

  for (let index = 0; index < lines.length; index += 1) {
    const lineNumber = index + 1;
    const capability = capabilitiesByStartLine.get(lineNumber);
    if (capability) {
      const exposure = resolvedExposure.get(capability.name) ?? capability.exposure;
      if (!capabilitySectionShown) {
        if (output.length > 0 && output[output.length - 1].trim() !== "") {
          output.push("");
        }
        output.push(mcpCapabilitySectionIntro(hasMetaOnDemand, hasDirect));
        capabilitySectionShown = true;
      }
      const previousIsListItem =
        output.length > 0 && output[output.length - 1].startsWith("- ");
      if (!useCapabilityListStyle || !previousIsListItem) {
        if (output.length > 0 && output[output.length - 1].trim() !== "") {
          output.push("");
        }
      }
      output.push(renderCapability(capability, exposure, useCapabilityListStyle));
      const followedByCapability = nextCapabilityAfter(
        lines,
        capability.endLine,
        capabilitiesByStartLine,
      );
      if (!useCapabilityListStyle || !followedByCapability) {
        if (lines[capability.endLine]?.trim() !== "") {
          output.push("");
        }
      }
      index = capability.endLine - 1;
      continue;
    }
    const external = externalsByStartLine.get(lineNumber);
    if (external) {
      if (output.length > 0 && output[output.length - 1].trim() !== "") {
        output.push("");
      }
      output.push(renderExternalReference(external));
      if (lines[external.endLine]?.trim() !== "") {
        output.push("");
      }
      index = external.endLine - 1;
      continue;
    }
    const standaloneExternal = STANDALONE_EXTERNAL_REFERENCE.exec(lines[index]);
    if (standaloneExternal) {
      if (output.length > 0 && output[output.length - 1].trim() !== "") {
        output.push("");
      }
      output.push(
        renderExternalReference({
          title: standaloneExternal[1],
          path: standaloneExternal[2],
          guide: "",
        }),
      );
      continue;
    }
    if (
      useCapabilityListStyle &&
      lines[index].trim() === "" &&
      isBlankBetweenConsecutiveCapabilities(
        lineNumber,
        lines,
        parsed.capabilities,
        capabilitiesByStartLine,
      )
    ) {
      continue;
    }
    output.push(lines[index]);
  }

  return { markdown: output.join("\n").replace(/\n{3,}/g, "\n\n").trim(), errors: [] };
}

export const IN_PLACE_MARKDOWN_SNIPPET = "## New section\n";

export function markdownCellAfterInsert(
  markdown: string,
  insertOffset: number,
  snippet = IN_PLACE_MARKDOWN_SNIPPET,
  sourcePath = "SKILL.md",
): WorkflowGuideDocumentCell | undefined {
  const prefix =
    insertOffset > 0 && markdown[insertOffset - 1] !== "\n" ? "\n" : "";
  const suffix =
    insertOffset < markdown.length && markdown[insertOffset] !== "\n"
      ? "\n"
      : "";
  const effectiveSnippet = `${prefix}${snippet}${suffix}`;
  const headingAt = effectiveSnippet.indexOf("##");
  const headingOffset =
    headingAt >= 0 ? insertOffset + headingAt : insertOffset + prefix.length;
  const nextMarkdown = `${markdown.slice(0, insertOffset)}${effectiveSnippet}${markdown.slice(insertOffset)}`;
  const cells = splitWorkflowGuideDocument(nextMarkdown, sourcePath);
  return cells.find(
    (cell) =>
      cell.kind === "markdown" &&
      cell.startOffset <= headingOffset &&
      headingOffset < cell.endOffset,
  );
}

export function splitWorkflowGuideDocument(
  markdown: string,
  sourcePath = "SKILL.md",
): WorkflowGuideDocumentCell[] {
  const parsed = parseWorkflowGuide(markdown);
  const lineOffsets = buildLineOffsets(markdown);

  const cells: WorkflowGuideDocumentCell[] = [];
  let cursor = 0;
  const structuredBlocks = [
    ...parsed.capabilities.map((capability) => ({
      kind: "capability" as const,
      startLine: capability.startLine,
      endLine: capability.endLine,
      capability,
    })),
    ...parsed.externals.map((external) => ({
      kind: "external" as const,
      startLine: external.startLine,
      endLine: external.endLine,
      external,
    })),
  ].sort((left, right) => left.startLine - right.startLine);

  for (const block of structuredBlocks) {
    const startOffset = lineOffsets[block.startLine - 1] ?? markdown.length;
    const endOffset = lineOffsets[block.endLine] ?? markdown.length;
    if (cursor < startOffset) {
      if (markdown.slice(cursor, startOffset).trim().length > 0) {
        cells.push({
          id: `markdown-${cursor}`,
          kind: "markdown",
          source: markdown.slice(cursor, startOffset),
          startOffset: cursor,
          endOffset: startOffset,
        });
      }
    }
    if (block.kind === "capability") {
      cells.push({
        id: `capability-${startOffset}`,
        kind: "capability",
        source: markdown.slice(startOffset, endOffset),
        startOffset,
        endOffset,
        capability: block.capability,
      });
    } else {
      cells.push({
        id: `external-${startOffset}`,
        kind: "external_reference",
        source: markdown.slice(startOffset, endOffset),
        startOffset,
        endOffset,
        externalReference: {
          title: block.external.title,
          relativePath: block.external.path,
          guide: block.external.guide,
        },
      });
    }
    cursor = endOffset;
  }
  if (cursor < markdown.length || cells.length === 0) {
    const source = markdown.slice(cursor);
    if (source.trim().length > 0 || cells.length === 0) {
      cells.push({
        id: `markdown-${cursor}`,
        kind: "markdown",
        source,
        startOffset: cursor,
        endOffset: markdown.length,
      });
    }
  }
  return cells
    .flatMap(splitMarkdownHeadings)
    .flatMap((cell) => splitExternalMarkdownReferences(cell, sourcePath));
}

function splitMarkdownHeadings(cell: WorkflowGuideDocumentCell): WorkflowGuideDocumentCell[] {
  if (cell.kind !== "markdown") return [cell];
  const lines = cell.source.match(/[^\n]*\n|[^\n]+/g) ?? [];
  const result: WorkflowGuideDocumentCell[] = [];
  let offset = cell.startOffset;
  let segmentStart = offset;
  for (const line of lines) {
    const content = line.endsWith("\n") ? line.slice(0, -1) : line;
    const trimmed = content.trimStart();
    if (/^(#{1,6}) (?!$).+$/.test(trimmed) && segmentStart < offset) {
      const segment = cell.source.slice(
        segmentStart - cell.startOffset,
        offset - cell.startOffset,
      );
      if (segment.trim().length > 0) {
        result.push({
          ...cell,
          id: `markdown-${segmentStart}`,
          source: segment,
          startOffset: segmentStart,
          endOffset: offset,
        });
      }
      segmentStart = offset;
    }
    offset += line.length;
  }
  if (segmentStart < cell.endOffset || result.length === 0) {
    result.push({
      ...cell,
      id: `markdown-${segmentStart}`,
      source: cell.source.slice(segmentStart - cell.startOffset),
      startOffset: segmentStart,
      endOffset: cell.endOffset,
    });
  }
  return result;
}

/** Ensures persisted Markdown ends on a line boundary for stable cell offsets. */
export function sanitizeWorkflowGuideMarkdown(markdown: string): string {
  if (markdown.length === 0 || markdown.endsWith("\n")) return markdown;
  return `${markdown}\n`;
}

/** Hides the structural trailing newline while editing a notebook cell. */
export function formatMarkdownCellSourceForEditor(source: string): string {
  return source.endsWith("\n") ? source.slice(0, -1) : source;
}

/**
 * Restores the structural trailing newline when committing an editor value.
 * It is appended unconditionally so this inverts
 * `formatMarkdownCellSourceForEditor`: appending only when absent would swallow
 * a newline typed at the end of the cell.
 */
export function commitMarkdownCellSource(editorValue: string): string {
  if (editorValue.length === 0) return editorValue;
  return `${editorValue}\n`;
}

/**
 * Locates where the open cell editor belongs in the notebook. An edit that
 * empties its cell collapses to a zero-width range that owns no cell, so the
 * editor anchors between neighbours instead of hiding one of them.
 */
export function markdownCellEditAnchor(
  cells: WorkflowGuideDocumentCell[],
  session: { start: number; end: number },
):
  | { mode: "replace"; index: number }
  | { mode: "before"; index: number }
  | { mode: "append" } {
  if (session.end > session.start) {
    const index = cells.findIndex(
      (cell) =>
        cell.kind === "markdown" &&
        cell.startOffset < session.end &&
        cell.endOffset > session.start,
    );
    if (index >= 0) return { mode: "replace", index };
  }
  const index = cells.findIndex((cell) => cell.startOffset >= session.start);
  return index >= 0 ? { mode: "before", index } : { mode: "append" };
}

/** Blocks deleting the document title cell (first level-1 heading). */
export function canDeleteWorkflowGuideCell(
  headings: Array<{ level: number; offset: number }>,
  cell: WorkflowGuideDocumentCell,
): boolean {
  const title = headings[0];
  if (!title || title.level !== 1 || cell.kind !== "markdown") {
    return true;
  }
  return !(cell.startOffset <= title.offset && title.offset < cell.endOffset);
}

function splitExternalMarkdownReferences(
  cell: WorkflowGuideDocumentCell,
  sourcePath: string,
): WorkflowGuideDocumentCell[] {
  if (cell.kind !== "markdown") return [cell];
  const result: WorkflowGuideDocumentCell[] = [];
  const lines = cell.source.match(/[^\n]*\n|[^\n]+/g) ?? [];
  let offset = cell.startOffset;
  let markdownStart = offset;
  for (const line of lines) {
    const reference = /^\s*\[([^\]\n]+)\]\(((?:references\/[^\s)#]+\.md)|(?:(?:\.\/)?[^/\s)#]+\.md))(?:#[^\s)]+)?\)\s*$/.exec(line);
    const nextOffset = offset + line.length;
    const relativePath = reference && resolveExternalMarkdownPath(sourcePath, reference[2]);
    if (!reference || !relativePath) {
      offset = nextOffset;
      continue;
    }
    if (markdownStart < offset) {
      const segment = cell.source.slice(
        markdownStart - cell.startOffset,
        offset - cell.startOffset,
      );
      if (segment.trim().length > 0) {
        result.push({
          ...cell,
          id: `${cell.id}-markdown-${markdownStart}`,
          source: segment,
          startOffset: markdownStart,
          endOffset: offset,
        });
      }
    }
    result.push({
      ...cell,
      id: `${cell.id}-external-${offset}`,
      kind: "external_reference",
      source: line,
      startOffset: offset,
      endOffset: nextOffset,
      externalReference: {
        title: reference[1],
        relativePath,
      },
    });
    markdownStart = nextOffset;
    offset = nextOffset;
  }
  if (markdownStart < cell.endOffset) {
    result.push({
      ...cell,
      id: `${cell.id}-markdown-${markdownStart}`,
      source: cell.source.slice(markdownStart - cell.startOffset),
      startOffset: markdownStart,
      endOffset: cell.endOffset,
    });
  }
  return result.length > 0 ? result : [cell];
}

function resolveExternalMarkdownPath(sourcePath: string, target: string): string | null {
  if (target.startsWith("references/")) return target;
  const separator = sourcePath.lastIndexOf("/");
  if (separator < 0) return null;
  const parent = sourcePath.slice(0, separator);
  return `${parent}/${target.replace(/^\.\//, "")}`;
}

function containsReservedWorkflowGuideSyntax(line: string) {
  return line.trimStart().startsWith(":::capability")
    || line.trimStart().startsWith(":::external")
    || DIRECTIVE_END.test(line)
    || /\[[^\]]+\]\((references|scripts|assets)\/[^\s)]+\)/.test(line);
}

interface MarkdownFence {
  delimiter: "`" | "~";
  length: number;
}

function parseOpeningFence(line: string): MarkdownFence | null {
  const trimmed = line.trimStart();
  const delimiter = trimmed[0];
  if (delimiter !== "`" && delimiter !== "~") return null;
  const length = countLeadingDelimiter(trimmed, delimiter);
  return length >= 3 ? { delimiter, length } : null;
}

function closesFence(line: string, fence: MarkdownFence): boolean {
  const trimmed = line.trimStart();
  const length = countLeadingDelimiter(trimmed, fence.delimiter);
  return length >= fence.length && trimmed.slice(length).trim().length === 0;
}

function countLeadingDelimiter(value: string, delimiter: "`" | "~"): number {
  let length = 0;
  while (value[length] === delimiter) length += 1;
  return length;
}

/** Boundary inserts at or before the first heading would un-pin the document title. */
export function canInsertAtWorkflowGuideBoundary(
  headings: Array<{ offset: number }>,
  offset: number,
): boolean {
  const titleOffset = headings[0]?.offset;
  return titleOffset === undefined || offset > titleOffset;
}

export function externalReferenceSource(
  title: string,
  relativePath: string,
  guide = "",
) {
  const header = JSON.stringify({ title, path: relativePath });
  return `:::external ${header}\n${guide.trim()}\n:::`;
}

export function capabilitySource(
  name: string,
  exposure: "direct" | "meta_on_demand",
  guide: string,
) {
  const header = JSON.stringify({ name, exposure });
  return `:::capability ${header}\n${guide.trim()}\n:::`;
}

function nextCapabilityAfter(
  lines: string[],
  afterLine: number,
  capabilitiesByStartLine: Map<number, WorkflowGuideCapabilityBlock>,
): boolean {
  const nextLine = nextNonBlankLine(lines, afterLine + 1);
  return nextLine !== undefined && capabilitiesByStartLine.has(nextLine);
}

function nextNonBlankLine(lines: string[], fromLine: number): number | undefined {
  for (let lineNumber = fromLine; lineNumber <= lines.length; lineNumber += 1) {
    if (lines[lineNumber - 1].trim() === "") {
      continue;
    }
    return lineNumber;
  }
  return undefined;
}

function previousNonBlankLine(lines: string[], beforeLine: number): number | undefined {
  for (let lineNumber = beforeLine; lineNumber >= 1; lineNumber -= 1) {
    if (lines[lineNumber - 1].trim() === "") {
      continue;
    }
    return lineNumber;
  }
  return undefined;
}

function lineInCapability(
  lineNumber: number,
  capabilities: WorkflowGuideCapabilityBlock[],
): boolean {
  return capabilities.some(
    (capability) => lineNumber >= capability.startLine && lineNumber <= capability.endLine,
  );
}

function isBlankBetweenConsecutiveCapabilities(
  blankLineNumber: number,
  lines: string[],
  capabilities: WorkflowGuideCapabilityBlock[],
  capabilitiesByStartLine: Map<number, WorkflowGuideCapabilityBlock>,
): boolean {
  const previousLine = previousNonBlankLine(lines, blankLineNumber - 1);
  const nextLine = nextNonBlankLine(lines, blankLineNumber + 1);
  if (previousLine === undefined || nextLine === undefined) {
    return false;
  }
  return lineInCapability(previousLine, capabilities) && capabilitiesByStartLine.has(nextLine);
}

function formatAsMarkdownListItem(text: string): string {
  const trimmed = text.trim();
  if (!trimmed) {
    return "-";
  }
  const [first, ...rest] = trimmed.split("\n");
  let output = `- ${first}`;
  for (const line of rest) {
    output += line.trim() === "" ? "\n" : `\n  ${line}`;
  }
  return output;
}

function renderExternalReference(reference: {
  title: string;
  path: string;
  guide: string;
}) {
  const guide = reference.guide.trim();
  return interpolateProjectionTemplate(
    guide ? TEST_PROJECTION.external.withGuide : TEST_PROJECTION.external.linkOnly,
    {
      title: reference.title,
      path: reference.path,
      guide,
    },
  );
}

function renderCapabilityBody(
  capability: WorkflowGuideCapabilityBlock,
  exposure: WorkflowBindingPolicy,
): string {
  const guide = capability.guide.trim();
  if (!guide) {
    return interpolateProjectionTemplate(
      exposure === "direct"
        ? TEST_PROJECTION.capability.item.directOnly
        : TEST_PROJECTION.capability.item.onDemandOnly,
      { name: capability.name },
    );
  }
  return interpolateProjectionTemplate(
    exposure === "meta_on_demand"
      ? TEST_PROJECTION.capability.item.onDemandWithGuide
      : TEST_PROJECTION.capability.item.directWithGuide,
    { name: capability.name, guide },
  );
}

function renderCapability(
  capability: WorkflowGuideCapabilityBlock,
  exposure: WorkflowBindingPolicy,
  listStyle: boolean,
) {
  const body = renderCapabilityBody(capability, exposure);
  return listStyle ? formatAsMarkdownListItem(body) : body;
}
