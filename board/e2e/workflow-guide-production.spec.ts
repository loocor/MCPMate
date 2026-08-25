import { expect, test } from "@playwright/test";
import { readFile, rm } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const apiBaseUrl = process.env.MCPMATE_UAT_API_BASE;
const dataDirectory = process.env.MCPMATE_UAT_DATA_DIR;
const screenshotDirectory =
  process.env.MCPMATE_UAT_SCREENSHOT_DIR ?? "test-results/workflow-guide-production";
const workflowGuideFixture = fileURLToPath(new URL("./fixtures/workflow-guide-mcp.py", import.meta.url));

function screenshotPath(name: string): string {
  return join(screenshotDirectory, name);
}

test("Workflow Guide notebook documents and preview persist the intended Markdown", async ({ page, request }) => {
  test.skip(!apiBaseUrl || !dataDirectory, "requires MCPMATE_UAT_API_BASE and MCPMATE_UAT_DATA_DIR for an isolated backend");
  await page.route("**/__mcpmate/dev-core-source", (route) => {
    return route.fulfill({ json: { apiBaseUrl } });
  });

  const onboardingResponse = await request.post(`${apiBaseUrl}/api/onboarding/complete`, {
    data: { completed: true },
  });
  await expect(onboardingResponse).toBeOK();

  const uatSuffix = Date.now();
  const capabilityName = `workflow_guide_uat_${uatSuffix}_collect_evidence`;
  const serverResponse = await request.post(`${apiBaseUrl}/api/mcp/servers/create`, {
    data: {
      name: `workflow_guide_uat_${uatSuffix}`,
      transport: {
        kind: "stdio",
        command: "python3",
        args: [workflowGuideFixture],
        env: {},
      },
    },
  });
  await expect(serverResponse).toBeOK();
  const serverPayload = await serverResponse.json();
  expect(serverPayload.success).toBe(true);
  const serverId = serverPayload.data.id as string;
  const profileResponse = await request.post(`${apiBaseUrl}/api/mcp/profile/authoring/save`, {
    data: {
      id: null,
      expected_authoring_generation: null,
      name: `Release investigation UAT ${uatSuffix}`,
      description: "Isolated browser acceptance profile",
      profile_type: "shared",
      priority: 0,
      is_active: false,
      is_default: false,
      server_ids: [serverId],
      clone_from_id: null,
      profile_mode: "workflow",
      skill_name: `release-investigation-guide-${uatSuffix}`,
    },
  });
  await expect(profileResponse).toBeOK();
  const profilePayload = await profileResponse.json();
  expect(profilePayload.success).toBe(true);
  const profileId = profilePayload.data.profile.id as string;

  const guideResponse = await request.get(`${apiBaseUrl}/api/mcp/profile/workflow/guide/view?id=${profileId}`);
  await expect(guideResponse).toBeOK();
  const guidePayload = await guideResponse.json();
  expect(guidePayload.success).toBe(true);
  const initialRevision = guidePayload.data.guide.guide_revision as number;
  const initialMarkdown = [
    "---",
    "name: imported-skill-creator",
    "description: Imported metadata must not become the Profile identity.",
    "---",
    "",
    "# Release investigation",
    "",
    "Use this guide to collect an evidence-based release report.",
  ].join("\n");
  const saveResponse = await request.post(`${apiBaseUrl}/api/mcp/profile/workflow/guide/save`, {
    data: {
      profile_id: profileId,
      expected_guide_revision: initialRevision,
      markdown: initialMarkdown,
    },
  });
  await expect(saveResponse).toBeOK();
  expect((await saveResponse.json()).success).toBe(true);

  await page.setViewportSize({ width: 1440, height: 1080 });
  await page.goto(`/profiles/${profileId}`);
  await page.getByRole("tab", { name: "Workflow" }).click();
  await expect(page.getByRole("region", { name: "Workflow Guide" })).toBeVisible();
  const workflowGuideSave = () =>
    page
      .getByRole("region", { name: "Workflow Guide" })
      .getByRole("button", { name: "Save", exact: true });
  // The outline toggle is named "Show outline" / "Hide outline" depending on
  // state, and the outline may already be open from an earlier step.
  const ensureOutlineOpen = async () => {
    const toggle = page.getByRole("button", { name: /^(Show|Hide) outline$/ });
    if ((await toggle.getAttribute("aria-pressed")) !== "true") {
      await toggle.click();
    }
  };
  await ensureOutlineOpen();
  await expect(page.getByLabel("Guide outline", { exact: true })).toContainText("Release investigation");
  await expect(page.getByLabel("Guide outline", { exact: true })).not.toContainText("Guide documents");
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("collect an evidence-based release report");
  await page.screenshot({ path: screenshotPath("notebook.png"), fullPage: true });

  const boundaryInsert = page.getByLabel("Insert at this position").first();
  await boundaryInsert.hover();
  await expect(boundaryInsert).toBeVisible();
  await boundaryInsert.click();
  for (const action of ["In-Place Markdown", "External Markdown", "Reference", "Capability", "Script", "Asset"]) {
    await expect(page.getByRole("button", { name: action, exact: true })).toBeVisible();
  }
  await page.screenshot({ path: screenshotPath("boundary-insert.png"), fullPage: true });
  await page.keyboard.press("Escape");

  // Prose is edited in a local buffer and committed on Done.
  await page.getByRole("button", { name: "Edit block" }).nth(1).click();
  const markdownBlock = page.getByLabel("Markdown block source");
  await expect(markdownBlock).toHaveValue("Use this guide to collect an evidence-based release report.");
  await markdownBlock.fill(
    "Use this guide to collect an evidence-based release report.\n\n## Evidence handoff\nPersist the concise report.\n",
  );
  await page.getByRole("button", { name: "Done" }).click();
  await expect(workflowGuideSave()).toBeEnabled();
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("Evidence handoff");

  const capabilityInsert = page.getByLabel("Insert at this position").first();
  await capabilityInsert.hover();
  await capabilityInsert.click();
  await page.getByRole("button", { name: "Capability", exact: true }).click();
  for (const action of ["In-Place Markdown", "External Markdown", "Reference", "Capability", "Script", "Asset"]) {
    await expect(page.getByRole("button", { name: action, exact: true })).not.toBeVisible();
  }
  await expect(page.getByRole("button", { name: "Back to insert types" })).toBeVisible();
  await page.screenshot({ path: screenshotPath("boundary-capability-panel.png"), fullPage: true });
  await page.getByRole("button", { name: "Search capabilities..." }).click();
  await page.getByRole("option", { name: capabilityName }).click();
  await page.getByLabel("Capability exposure").click();
  await page.getByRole("option", { name: "Direct exposure", exact: true }).click();
  await page.getByRole("textbox", { name: "Guide", exact: true }).fill("Inspect release notes, then compare linked pull requests.");
  await page.getByRole("button", { name: "Insert capability" }).click();
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("Capability");
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText(capabilityName);
  await expect(page.getByLabel("Workflow Guide notebook")).not.toContainText("Empty Markdown block");
  await page.screenshot({ path: screenshotPath("capability-insert.png"), fullPage: true });

  const saveRequest = page.waitForResponse((response) =>
    response.url().endsWith("/api/mcp/profile/workflow/guide/save") && response.request().method() === "POST",
  );
  await page.getByRole("button", { name: "Save" }).click();
  expect((await saveRequest).ok()).toBeTruthy();
  await expect(workflowGuideSave()).toBeDisabled();
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("Evidence handoff");
  await expect(page.getByLabel("Guide inspector")).toContainText("collect_evidence");
  // Exposure moved from the inspector rows to the notebook capability cells
  // in the Inspector/outline rework; keep asserting it renders after save.
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("Direct exposure");
  await page.screenshot({ path: screenshotPath("saved-notebook.png"), fullPage: true });

  const persistedGuide = await request.get(`${apiBaseUrl}/api/mcp/profile/workflow/guide/view?id=${profileId}`);
  await expect(persistedGuide).toBeOK();
  await expect((await persistedGuide.json()).data.guide.markdown).toContain("## Evidence handoff");

  const externalInsert = page.getByLabel("Insert at this position").first();
  await externalInsert.hover();
  await externalInsert.click();
  await page.getByRole("button", { name: "External Markdown", exact: true }).click();
  await page.getByLabel("Section name").fill("Release policy");
  await page.getByRole("button", { name: "Create external Markdown" }).click();
  await ensureOutlineOpen();
  await expect(page.locator('nav[aria-label="Guide outline"]').last()).toContainText("Release policy");
  await page.getByRole("button", { name: "Release policy" }).last().click();
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("Release policy");
  await page.screenshot({ path: screenshotPath("external-notebook.png"), fullPage: true });

  await ensureOutlineOpen();
  await page.getByLabel("Open main Guide").click();
  await page.getByRole("button", { name: "Edit block" }).nth(1).click();
  const rootDraft = page.getByLabel("Markdown block source");
  await rootDraft.fill(`${await rootDraft.inputValue()}\n\nUnsaved root draft survives external saves.\n`);
  await page.getByRole("button", { name: "Done" }).click();
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("Unsaved root draft survives external saves.");
  await page.getByRole("button", { name: "Release policy" }).last().click();
  const externalProseBoundary = page.getByLabel("Insert at this position").first();
  await externalProseBoundary.hover();
  await externalProseBoundary.click();
  await page.getByRole("button", { name: "In-Place Markdown", exact: true }).click();
  await page.getByLabel("Markdown block source").fill("Unsaved external preview.\n");
  await page.getByRole("button", { name: "Done" }).click();
  const externalCapabilityInsert = page.getByLabel("Insert at this position").first();
  await externalCapabilityInsert.hover();
  await externalCapabilityInsert.click();
  await page.getByRole("button", { name: "Capability", exact: true }).click();
  await page.getByRole("button", { name: "Search capabilities..." }).click();
  await page.getByRole("option", { name: capabilityName }).click();
  await page.getByLabel("Capability exposure").click();
  await page.getByRole("option", { name: "Direct exposure", exact: true }).click();
  await page.getByRole("textbox", { name: "Guide", exact: true }).fill("Collect external release policy evidence.");
  await page.getByRole("button", { name: "Insert capability" }).click();
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("Capability");
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText(capabilityName);
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("Unsaved external preview.");
  await page.screenshot({ path: screenshotPath("external-preview.png"), fullPage: true });

  const externalSaveRequest = page.waitForResponse((response) =>
    response.url().endsWith("/api/mcp/profile/workflow/guide/package-files/upload") && response.request().method() === "POST",
  );
  await page.getByRole("button", { name: "Save" }).click();
  expect((await externalSaveRequest).ok()).toBeTruthy();
  await expect(workflowGuideSave()).toBeDisabled();
  await ensureOutlineOpen();
  await expect(page.getByLabel("Guide outline", { exact: true })).toContainText("Release policy");

  await page.getByLabel("Open main Guide").click();
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("Evidence handoff");
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("Unsaved root draft survives external saves.");

  for (const material of [
    { category: "reference", title: "Release checklist", file: { name: "checklist.yaml", mimeType: "application/yaml", buffer: Buffer.from("checks: []\n") } },
    { category: "script", title: "Summarize evidence", file: { name: "summarize.py", mimeType: "text/x-python", buffer: Buffer.from("print('summary')\n") } },
    { category: "asset", title: "Report template", file: { name: "report.pdf", mimeType: "application/pdf", buffer: Buffer.from("%PDF-1.4\n") } },
  ] as const) {
    const materialInsert = page.getByLabel("Insert at this position").first();
    await materialInsert.hover();
    await materialInsert.click();
    const action = {
      reference: "Reference",
      script: "Script",
      asset: "Asset",
    }[material.category];
    await page.getByRole("button", { name: action, exact: true }).click();
    await page.getByPlaceholder("File title").fill(material.title);
    await page.getByLabel("Package file upload").setInputFiles(material.file);
    const packageSaveRequest = page.waitForResponse((response) =>
      response.url().endsWith("/api/mcp/profile/workflow/guide/package-files/upload") && response.request().method() === "POST",
    );
    await page.getByRole("button", { name: "Upload and insert" }).click();
    const packageResponse = await packageSaveRequest;
    expect(packageResponse.ok()).toBeTruthy();
    await expect(page.getByLabel("Workflow Guide notebook")).toContainText(material.title);
    await page.keyboard.press("Escape");
  }

  const finalSaveRequest = page.waitForResponse((response) =>
    response.url().endsWith("/api/mcp/profile/workflow/guide/save") && response.request().method() === "POST",
  );
  await page.getByRole("button", { name: "Save" }).click();
  expect((await finalSaveRequest).ok()).toBeTruthy();
  await expect(workflowGuideSave()).toBeDisabled();
  await expect(page.getByLabel("Guide inspector")).toContainText("Summarize evidence");
  await page.getByRole("button", { name: "Refresh" }).click();
  await expect(page.getByLabel("Guide inspector")).toContainText("Summarize evidence");
  await ensureOutlineOpen();
  await expect(page.getByLabel("Guide outline", { exact: true })).toContainText("Release policy");
  await page.screenshot({ path: screenshotPath("materials-and-inspector.png"), fullPage: true });

  const skillPath = join(dataDirectory!, "skills", `release-investigation-guide-${uatSuffix}`, "SKILL.md");
  await rm(skillPath);
  await expect.poll(async () => readFile(skillPath, "utf8").then(() => true, () => false)).toBe(false);
  const repairRequest = page.waitForResponse((response) =>
    response.url().endsWith("/api/mcp/profile/workflow/guide/repair") && response.request().method() === "POST",
  );
  await page.getByRole("button", { name: "Repair" }).click();
  expect((await repairRequest).ok()).toBeTruthy();
  await expect.poll(async () => readFile(skillPath, "utf8")).toContain(`name: release-investigation-guide-${uatSuffix}`);
  // Final invocation wording: marker-style items plus a single section intro
  // that explains the UCAN wrapper tools (no per-item full sentences).
  await expect.poll(async () => readFile(skillPath, "utf8")).toContain("Steps marked `(direct)` can be invoked by tool name");
  await expect.poll(async () => readFile(skillPath, "utf8")).toContain("(direct): Inspect release notes, then compare linked pull requests.");
  await expect.poll(async () => readFile(skillPath, "utf8")).not.toContain("Exposure:");
  await page.screenshot({ path: screenshotPath("repaired-skill.png"), fullPage: true });

  await page.getByRole("button", { name: "Edit block" }).nth(1).click();
  await page.getByLabel("Markdown block source").fill("Draft edits that stay unsaved.\n");
  await page.getByRole("button", { name: "Done" }).click();
  await expect(workflowGuideSave()).toBeEnabled();
  await page.getByRole("tab", { name: "Overview" }).click();
  const discardDialog = page.getByRole("alertdialog");
  await expect(discardDialog).toContainText("Discard unsaved Workflow Guide changes?");
  await page.getByRole("button", { name: "Discard and leave" }).click();
  await expect(page.getByRole("region", { name: "Workflow Guide" })).toBeHidden();

  // Frozen cell edit sessions: a prose edit keeps its own character range and
  // Done applies the full typed content.
  await page.getByRole("tab", { name: "Workflow" }).click();
  await expect(page.getByRole("region", { name: "Workflow Guide" })).toBeVisible();
  await page.getByRole("button", { name: "Edit block" }).nth(1).click();
  const sessionEditor = page.getByLabel("Markdown block source");
  const multiLineDraft = "Section body line.\n\nSub body.\n";
  await sessionEditor.fill(multiLineDraft);
  // The editor hides exactly one structural trailing newline; the typed
  // content must survive the round-trip.
  await expect(sessionEditor).toHaveValue(multiLineDraft.slice(0, -1));
  await page.getByRole("button", { name: "Done" }).click();
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("Sub body.");

  // Clearing a prose cell keeps the editor anchored in place instead of
  // collapsing the document structure around it.
  await page.getByRole("button", { name: "Edit block" }).nth(1).click();
  await page.getByLabel("Markdown block source").fill("");
  await expect(page.getByLabel("Markdown block source")).toHaveValue("");
  await page.getByRole("button", { name: "Cancel" }).first().click();
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("Sub body.");

  // Bare markers are inert: `##` without a space is prose, not a heading, in
  // both the notebook and the backend parser.
  await page.getByRole("button", { name: "Edit block" }).nth(1).click();
  await page.getByLabel("Markdown block source").fill("## Marker text stays prose\nC# stays prose\n");
  await expect(page.getByLabel("Markdown block source")).toHaveValue("## Marker text stays prose\nC# stays prose");
  await page.getByRole("button", { name: "Cancel" }).first().click();

  // In-Place Markdown lands at the clicked boundary with a starter heading line.
  const inPlaceBoundary = page.getByLabel("Insert at this position").first();
  await inPlaceBoundary.hover();
  await inPlaceBoundary.click();
  await page.getByRole("button", { name: "In-Place Markdown", exact: true }).click();
  const newSectionEditor = page.getByLabel("Markdown block source");
  await expect(newSectionEditor).toHaveValue("## New section");
  await newSectionEditor.fill("## Inserted section\n\nInserted prose block.\n");
  await page.getByRole("button", { name: "Done" }).click();
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("Inserted prose block.");

  // Cancel in an edit session restores the pre-edit cell content instead of
  // keeping the live-typed draft.
  await page.getByRole("button", { name: "Edit block" }).nth(1).click();
  await page.getByLabel("Markdown block source").fill("CANCELLED EDIT BODY\n");
  await page.getByRole("button", { name: "Cancel" }).first().click();
  await expect(page.getByLabel("Workflow Guide notebook")).not.toContainText("CANCELLED EDIT BODY");
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("Sub body.");

  // Cancel (Escape) after an in-place insert discards the inserted cell
  // entirely, while previously committed cells stay untouched.
  const cancelBoundary = page.getByLabel("Insert at this position").first();
  await cancelBoundary.hover();
  await cancelBoundary.click();
  await page.getByRole("button", { name: "In-Place Markdown", exact: true }).click();
  await page.getByLabel("Markdown block source").fill("CANCELLED INSERT\n");
  await page.keyboard.press("Escape");
  await expect(page.getByLabel("Workflow Guide notebook")).not.toContainText("CANCELLED INSERT");
  await expect(page.getByLabel("Workflow Guide notebook")).toContainText("Inserted prose block.");
});
