import { describe, expect, test } from "bun:test";

import {
  capabilitySource,
  effectiveExposureByName,
  parseWorkflowGuide,
  renderWorkflowSkill,
  splitWorkflowGuideDocument,
  sanitizeWorkflowGuideMarkdown,
  commitMarkdownCellSource,
  markdownCellEditAnchor,
  formatMarkdownCellSourceForEditor,
  markdownCellAfterInsert,
  IN_PLACE_MARKDOWN_SNIPPET,
  restoreAfterCanceledInsert,
} from "./workflow-guide-directive";

describe("Workflow Guide directives", () => {
  test("parses occurrence-level canonical name, exposure, and guide", () => {
    const guide = parseWorkflowGuide(
      '# Release investigation\n\n:::capability {"name":"server__lookup","exposure":"direct"}\nUse PNG output for this occurrence.\n:::\n',
    );

    expect(guide.errors).toEqual([]);
    expect(guide.headings).toEqual([
      { level: 1, text: "Release investigation", offset: 0 },
    ]);
    expect(guide.capabilities).toEqual([
      {
        name: "server__lookup",
        exposure: "direct",
        guide: "Use PNG output for this occurrence.",
        startLine: 3,
        endLine: 5,
      },
    ]);
  });

  test("allows repeated names with independent exposure and guide", () => {
    const guide = parseWorkflowGuide(
      `${capabilitySource("server__lookup", "meta_on_demand", "Inspect details first.")}\n${capabilitySource("server__lookup", "direct", "Call directly here.")}`,
    );

    expect(guide.errors).toEqual([]);
    expect(guide.capabilities.map(({ exposure, guide }) => ({ exposure, guide }))).toEqual([
      { exposure: "meta_on_demand", guide: "Inspect details first." },
      { exposure: "direct", guide: "Call directly here." },
    ]);
  });

  test("rejects reserved syntax inside fenced code", () => {
    const guide = parseWorkflowGuide(
      '```markdown\n:::capability {"name":"server__lookup","exposure":"direct"}\n[Example](references/example.md)\n:::\n```',
    );
    expect(guide.capabilities).toEqual([]);
    expect(guide.errors).toEqual([
      "line 2: Workflow Guide directives and references are not allowed in fenced code",
      "line 3: Workflow Guide directives and references are not allowed in fenced code",
      "line 4: Workflow Guide directives and references are not allowed in fenced code",
    ]);
  });

  test("keeps shorter fence markers inside a longer fence", () => {
    const guide = parseWorkflowGuide(
      '````markdown\n```\n:::capability {"name":"server__lookup","exposure":"direct"}\n:::\n```\n````',
    );

    expect(guide.capabilities).toEqual([]);
    expect(guide.errors).toContain(
      "line 3: Workflow Guide directives and references are not allowed in fenced code",
    );
    expect(guide.errors).toContain(
      "line 4: Workflow Guide directives and references are not allowed in fenced code",
    );
  });

  test("reports malformed directives and opaque identifiers", () => {
    const guide = parseWorkflowGuide(
      ':::capability {"name":"server__lookup"}\n123e4567-e89b-12d3-a456-426614174000\nskill://private',
    );

    expect(guide.errors).toHaveLength(3);
    expect(guide.errors[0]).toContain("Capability exposure");
    expect(guide.errors[1]).toContain("opaque identifiers");
    expect(guide.errors[2]).toContain("skill://");
  });

  test("does not rewrite placeholders inside capability names", () => {
    const skill = renderWorkflowSkill(
      capabilitySource("server://docs/{guide}", "direct", "Then capture."),
    );
    expect(skill.errors).toEqual([]);
    expect(skill.markdown).toContain("`server://docs/{guide}` (direct): Then capture.");
  });

  test("keeps standalone reference fragments in projected Markdown", () => {
    const skill = renderWorkflowSkill("# Investigate\n\n[API](references/api.md#auth)\n");
    expect(skill.errors).toEqual([]);
    expect(skill.markdown).toContain("[API](references/api.md#auth)");
    expect(skill.markdown).not.toContain("[API](references/api.md)\n");
  });

  test("projects readable neutral Markdown", () => {
    const skill = renderWorkflowSkill(
      `# Release investigation\n\n${capabilitySource("server__lookup", "direct", "Use PNG output.")}`,
    );

    expect(skill.errors).toEqual([]);
    expect(skill.markdown).toContain("The following steps are MCP server capability invocations.");
    expect(skill.markdown).toContain("Use PNG output.");
    expect(skill.markdown).toContain("`server__lookup` (direct):");
    expect(skill.markdown).not.toContain("**Capability: server__lookup**");
    expect(skill.markdown).not.toContain("Use `server__lookup` directly.");
    expect(skill.markdown).not.toContain(":::capability");
    expect(skill.markdown).not.toContain("Exposure:");
  });

  test("normalizes mixed occurrence exposure to one invocation strategy", () => {
    const markdown = [
      capabilitySource("server__lookup", "meta_on_demand", "Inspect details first."),
      capabilitySource("server__lookup", "direct", "Call directly here."),
    ].join("\n");
    const parsed = parseWorkflowGuide(markdown);
    const skill = renderWorkflowSkill(markdown, effectiveExposureByName(parsed.capabilities));

    expect(skill.errors).toEqual([]);
    expect(skill.markdown).toContain("The following steps are MCP server capability invocations.");
    expect(skill.markdown).toContain("- `server__lookup` (direct): Inspect details first.");
    expect(skill.markdown).toContain("- `server__lookup` (direct): Call directly here.");
    expect(skill.markdown).not.toContain("mcpmate_ucan_details");
    expect(skill.markdown).not.toContain("Exposure:");
  });

  test("projects meta-on-demand invocation without direct labels", () => {
    const markdown = [
      capabilitySource("server__lookup", "meta_on_demand", "Inspect first."),
      capabilitySource("server__lookup", "meta_on_demand", "Then summarize."),
    ].join("\n");
    const skill = renderWorkflowSkill(markdown);

    expect(skill.markdown).toContain("The following steps are MCP server capability invocations.");
    expect(skill.markdown).toContain(
      "Steps marked `(on-demand)` must be inspected with `mcpmate_ucan_details`, then invoked with `mcpmate_ucan_call`.",
    );
    expect(skill.markdown.match(/mcpmate_ucan_details/g)).toHaveLength(1);
    expect(skill.markdown).toContain("- `server__lookup` (on-demand): Inspect first.");
    expect(skill.markdown).toContain("- `server__lookup` (on-demand): Then summarize.");
    expect(skill.markdown).not.toContain("Use `server__lookup` directly.");
    expect(skill.markdown).not.toContain("**Capability:");
  });

  test("projects multiple capabilities as a markdown list", () => {
    const markdown = [
      "# Release investigation",
      "",
      capabilitySource("open-page", "direct", "Open the target URL."),
      capabilitySource("capture-shot", "direct", "Capture the screenshot."),
    ].join("\n");
    const skill = renderWorkflowSkill(markdown);

    expect(skill.errors).toEqual([]);
    expect(skill.markdown).toBe(
      [
        "# Release investigation",
        "",
        "The following steps are MCP server capability invocations. Steps marked `(direct)` can be invoked by tool name.",
        "",
        "- `open-page` (direct): Open the target URL.",
        "- `capture-shot` (direct): Capture the screenshot.",
      ].join("\n"),
    );
  });

  test("splits narrative and Capability cells without changing source ranges", () => {
    const markdown = `# Release investigation\n\nBefore starting.\n\n${capabilitySource("server__lookup", "direct", "Read the logs.")}\n\nClose with findings.\n`;
    const cells = splitWorkflowGuideDocument(markdown);

    expect(cells.map((cell) => cell.kind)).toEqual([
      "markdown",
      "capability",
      "markdown",
    ]);
    expect(cells.map((cell) => cell.source).join("")).toBe(markdown);
    expect(cells[0].source).toBe("# Release investigation\n\nBefore starting.\n\n");
    expect(cells[1].capability).toMatchObject({
      name: "server__lookup",
      exposure: "direct",
      guide: "Read the logs.",
    });
  });

  test("splits markdown cells at line-start headings", () => {
    const markdown = "# Title\n\nIntro.\n\n## Section\n\nBody.\n";
    const cells = splitWorkflowGuideDocument(markdown);

    expect(cells.map((cell) => cell.kind)).toEqual(["markdown", "markdown"]);
    expect(cells.map((cell) => cell.source)).toEqual([
      "# Title\n\nIntro.\n\n",
      "## Section\n\nBody.\n",
    ]);
    expect(cells.map((cell) => cell.source).join("")).toBe(markdown);
  });

  test("recognizes headings after leading whitespace", () => {
    expect(parseWorkflowGuide("  ## Indented\n").headings).toEqual([
      { level: 2, text: "Indented", offset: 2 },
    ]);
    expect(parseWorkflowGuide("##  Padded\n").headings).toEqual([
      { level: 2, text: "Padded", offset: 0 },
    ]);
    expect(parseWorkflowGuide("##\tTabbed\n").headings).toEqual([]);
    expect(parseWorkflowGuide("## \n").headings).toEqual([]);
    expect(parseWorkflowGuide("C# stays prose\n").headings).toEqual([]);
    expect(parseWorkflowGuide("Item ends here.## Glued\n").headings).toEqual([]);
  });


  test("creates a notebook cell for each outline heading", () => {
    const markdown = "# First section\n\nIntro.\n\n## Second section\n\nDetails.\n";
    const guide = parseWorkflowGuide(markdown);
    const cells = splitWorkflowGuideDocument(markdown);
    for (const heading of guide.headings) {
      expect(
        cells.find(
          (cell) =>
            cell.kind === "markdown" &&
            cell.startOffset <= heading.offset &&
            heading.offset < cell.endOffset,
        ),
      ).toBeDefined();
    }
  });

  test("recognizes standalone external Markdown references", () => {
    const cells = splitWorkflowGuideDocument(
      "[Release policy](references/release-policy.md)\n",
    );
    expect(cells).toHaveLength(1);
    expect(cells[0]).toMatchObject({
      kind: "external_reference",
      externalReference: {
        title: "Release policy",
        relativePath: "references/release-policy.md",
      },
    });
  });

  test("recognizes external Markdown directives with guide text", () => {
    const cells = splitWorkflowGuideDocument(
      ':::external {"title":"Release policy","path":"references/release-policy.md"}\nRead this before approving a release.\n:::\n',
    );
    expect(cells).toHaveLength(1);
    expect(cells[0]).toMatchObject({
      kind: "external_reference",
      externalReference: {
        title: "Release policy",
        relativePath: "references/release-policy.md",
        guide: "Read this before approving a release.",
      },
    });
    const rendered = renderWorkflowSkill(cells[0].source);
    expect(rendered.markdown).toContain("[Release policy](references/release-policy.md)");
    expect(rendered.markdown).toContain("Read this before approving a release.");
    expect(rendered.markdown).not.toContain("**External document:");
    expect(rendered.markdown).not.toContain(":::external");
  });

  test("keeps leading whitespace with the first heading cell", () => {
    const cells = splitWorkflowGuideDocument(
      "\n# Release investigation\n\nUse this guide to collect an evidence-based release report.",
    );
    expect(cells.map((cell) => cell.kind)).toEqual(["markdown"]);
    expect(cells[0]).toMatchObject({
      startOffset: 1,
      source: "# Release investigation\n\nUse this guide to collect an evidence-based release report.",
    });
  });

  test("drops whitespace-only gaps around capability blocks", () => {
    const cells = splitWorkflowGuideDocument(
      "# Snapshot\n\nContext paragraph.\n\n:::capability {\"name\":\"cap_a\",\"exposure\":\"direct\"}\nUse it.\n:::\n\n## Next\n",
    );
    expect(cells.map((cell) => cell.kind)).toEqual([
      "markdown",
      "capability",
      "markdown",
    ]);
  });

  test("locates the inserted in-place markdown cell", () => {
    const markdown = "# Title\n\nBody.\n";
    const cell = markdownCellAfterInsert(markdown, markdown.length, IN_PLACE_MARKDOWN_SNIPPET);
    expect(cell?.source).toBe("## New section\n");
    expect(cell?.kind).toBe("markdown");
  });

  test("restores the pre-insert document when canceling an in-place insert", () => {
    const baseline = "# Title\n\nBody.\n";
    const inserted = `${baseline}## New section\n`;
    expect(
      restoreAfterCanceledInsert(inserted, {
        start: baseline.length,
        end: inserted.length,
        restoreMarkdown: baseline,
      }),
    ).toBe(baseline);
    expect(
      restoreAfterCanceledInsert(inserted, {
        start: baseline.length,
        end: inserted.length,
      }),
    ).toBe(baseline);
  });

  test("ends persisted Markdown on a line boundary", () => {
    expect(sanitizeWorkflowGuideMarkdown("Body")).toBe("Body\n");
    expect(sanitizeWorkflowGuideMarkdown("Body\n")).toBe("Body\n");
    expect(sanitizeWorkflowGuideMarkdown("")).toBe("");
    expect(
      sanitizeWorkflowGuideMarkdown(
        "Use Playwright MCP to take a screenshot of the specified URL.\n## Goal\n",
      ),
    ).toBe("Use Playwright MCP to take a screenshot of the specified URL.\n## Goal\n");
    expect(sanitizeWorkflowGuideMarkdown("4. Item ends here.## 常见问题\n")).toBe(
      "4. Item ends here.## 常见问题\n",
    );
  });

  test("hides only the structural trailing newline in the cell editor", () => {
    expect(formatMarkdownCellSourceForEditor("## Section\n\nBody\n")).toBe(
      "## Section\n\nBody",
    );
    expect(formatMarkdownCellSourceForEditor("## Section\n\nBody\n\n")).toBe(
      "## Section\n\nBody\n",
    );
  });

  test("restores the structural trailing newline when committing cell edits", () => {
    expect(commitMarkdownCellSource("## Section\n\nBody")).toBe(
      "## Section\n\nBody\n",
    );
    expect(commitMarkdownCellSource("")).toBe("");
  });

  test("keeps a newline typed at the end of a cell editor", () => {
    // The editor hides the structural trailing newline, so committing must
    // append unconditionally or an Enter keypress at the end is swallowed.
    const editorValue = formatMarkdownCellSourceForEditor("## Section\n\nBody\n");
    expect(editorValue).toBe("## Section\n\nBody");
    const afterEnter = `${editorValue}\n`;
    const committed = commitMarkdownCellSource(afterEnter);
    expect(committed).toBe("## Section\n\nBody\n\n");
    expect(formatMarkdownCellSourceForEditor(committed)).toBe(afterEnter);
  });

  test("anchors the cell editor on the prose cell it owns", () => {
    const markdown =
      '# Snapshot\n\n## Workflow\n\nRun the steps below.\n\n:::capability {"name":"cap_a","exposure":"direct"}\nUse it.\n:::\n';
    const cells = splitWorkflowGuideDocument(markdown);
    expect(cells.map((cell) => cell.kind)).toEqual([
      "markdown",
      "markdown",
      "capability",
    ]);
    const proseCell = cells[1]!;
    expect(
      markdownCellEditAnchor(cells, {
        start: proseCell.startOffset,
        end: proseCell.endOffset,
      }),
    ).toEqual({ mode: "replace", index: 1 });
  });

  test("anchors an emptied cell editor between neighbours instead of hiding one", () => {
    const markdown =
      '# Snapshot\n\n:::capability {"name":"cap_a","exposure":"direct"}\nUse it.\n:::\n';
    const cells = splitWorkflowGuideDocument(markdown);
    const capabilityCell = cells[1]!;
    expect(capabilityCell.kind).toBe("capability");
    // A cleared in-place insert collapses to a zero-width range that owns no
    // cell; it must not take over the neighbouring capability cell.
    expect(
      markdownCellEditAnchor(cells, {
        start: capabilityCell.startOffset,
        end: capabilityCell.startOffset,
      }),
    ).toEqual({ mode: "before", index: 1 });
    expect(
      markdownCellEditAnchor(cells, {
        start: markdown.length,
        end: markdown.length,
      }),
    ).toEqual({ mode: "append" });
  });


  test("renders capability blocks as separate paragraphs from tight markdown", () => {
    const rendered = renderWorkflowSkill(
      "# Snapshot\nContext paragraph.\n:::capability {\"name\":\"cap_a\",\"exposure\":\"direct\"}\nUse it.\n:::\nFollowing paragraph.",
    );
    expect(rendered.markdown).toBe(
      [
        "# Snapshot",
        "Context paragraph.",
        "",
        "The following steps are MCP server capability invocations. Steps marked `(direct)` can be invoked by tool name.",
        "",
        "`cap_a` (direct): Use it.",
        "",
        "Following paragraph.",
      ].join("\n"),
    );
  });

  test("resolves sibling Markdown references only when standalone", () => {
    const standalone = splitWorkflowGuideDocument(
      "[REST API](rest-api.md#authentication)\n",
      "references/cli-and-mcp.md",
    );
    expect(standalone[0]).toMatchObject({
      kind: "external_reference",
      externalReference: { relativePath: "references/rest-api.md" },
    });

    const inline = "See [REST API](rest-api.md#authentication) for details.\n";
    expect(
      splitWorkflowGuideDocument(inline, "references/cli-and-mcp.md")[0],
    ).toMatchObject({ kind: "markdown", source: inline });
  });
});
