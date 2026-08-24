import { describe, expect, test } from "bun:test";

const releaseWorkflowUrl = new URL("../../.github/workflows/release.yml", import.meta.url);
const dockerWorkflowUrl = new URL("../../.github/workflows/docker-publish.yml", import.meta.url);
const injectVersionActionUrl = new URL("../../.github/actions/inject-version/action.yml", import.meta.url);
const nightlyWorkflowUrl = new URL("../../.github/workflows/nightly.yml", import.meta.url);
const tauriConfigUrl = new URL("../../desktop/src-tauri/tauri.conf.json", import.meta.url);
const tauriReleaseOverlayUrl = new URL(
  "../../desktop/src-tauri/tauri.release-overlay.json",
  import.meta.url,
);
const desktopWorkflowUrls = ["desktop-macos.yml", "desktop-windows.yml", "desktop-linux.yml"].map(
  (name) => new URL(`../../.github/workflows/${name}`, import.meta.url),
);

describe("release workflow contract", () => {
  test("classifies the release before platform builds", async () => {
    const workflow = await Bun.file(releaseWorkflowUrl).text();
    const validateJob = workflow.indexOf("  validate-tag:");
    const firstBuildJob = workflow.indexOf("  build-macos:");

    expect(validateJob).toBeGreaterThanOrEqual(0);
    expect(firstBuildJob).toBeGreaterThan(validateJob);
    expect(workflow.slice(validateJob, firstBuildJob)).toContain("bun packaging/desktop/release-tag.ts");
    expect(workflow).toContain("release-channel: ${{ steps.classify.outputs.release-channel }}");
    expect(workflow).toContain("app-version: ${{ steps.classify.outputs.app-version }}");
  });

  test("fails on missing distribution tokens before platform builds or release mutation", async () => {
    const workflow = await Bun.file(releaseWorkflowUrl).text();
    const validateJob = workflow.indexOf("  validate-tag:");
    const firstBuildJob = workflow.indexOf("  build-macos:");
    const prebuild = workflow.slice(validateJob, firstBuildJob);

    expect(prebuild).toContain("- name: Validate distribution credentials");
    expect(prebuild).toContain("DOWNLOADS_WORKFLOW_TOKEN: ${{ secrets.DOWNLOADS_WORKFLOW_TOKEN }}");
    expect(prebuild).toContain("HOMEBREW_TAP_TOKEN: ${{ secrets.HOMEBREW_TAP_TOKEN }}");
    expect(prebuild).toContain('test -n "$DOWNLOADS_WORKFLOW_TOKEN"');
    expect(prebuild).toContain('test -n "$HOMEBREW_TAP_TOKEN"');
  });

  test("uses the classified channel and app version throughout the release", async () => {
    const workflow = await Bun.file(releaseWorkflowUrl).text();

    expect(workflow).toContain("RELEASE_CHANNEL: ${{ needs.validate-tag.outputs.release-channel }}");
    expect(workflow).toContain("APP_VERSION: ${{ needs.validate-tag.outputs.app-version }}");
    expect(workflow).toContain("prerelease: false");
    expect(workflow).not.toContain("contains(env.RELEASE_TAG, '-')");
  });

  test("requires Windows ARM64 artifacts consistently with the release manifest", async () => {
    const workflow = await Bun.file(releaseWorkflowUrl).text();
    const windowsJob = workflow.slice(
      workflow.indexOf("  build-windows:"),
      workflow.indexOf("  build-linux:"),
    );

    expect(windowsJob).not.toContain("experimental:");
    expect(windowsJob).not.toContain("continue-on-error:");
    expect(workflow).toContain('"MCPMate_${VERSION}_windows_arm64.msi"');
    expect(workflow).toContain(
      '::error::Missing required release asset: MCPMate_${VERSION}_windows_arm64.msi.sig or MCPMate_${VERSION}_windows_arm64.msi.zip.sig',
    );
    expect(workflow).not.toContain("::warning::Experimental asset not found");
  });

  test("synchronizes Admin and release notes before dispatching the Tap", async () => {
    const workflow = await Bun.file(releaseWorkflowUrl).text();
    const synchronize = workflow.indexOf("- name: Synchronize exact-tag download manifest");
    const updateNotes = workflow.indexOf("- name: Update GitHub Release body");
    const dispatch = workflow.indexOf("- name: Dispatch Homebrew Tap update");

    expect(synchronize).toBeGreaterThanOrEqual(0);
    expect(updateNotes).toBeGreaterThan(synchronize);
    expect(dispatch).toBeGreaterThan(updateNotes);
    expect(workflow.slice(dispatch)).toContain("bun packaging/desktop/dispatch-homebrew-tap.ts");
  });

  test("gates GitHub Release publishing on the reusable Docker workflow", async () => {
    const releaseWorkflow = await Bun.file(releaseWorkflowUrl).text();
    const dockerWorkflow = await Bun.file(dockerWorkflowUrl).text();
    const dockerJobStart = releaseWorkflow.indexOf("  publish-docker:");
    const releaseJobStart = releaseWorkflow.indexOf("  release:");

    expect(dockerJobStart).toBeGreaterThanOrEqual(0);
    expect(releaseJobStart).toBeGreaterThan(dockerJobStart);

    const dockerJob = releaseWorkflow.slice(
      dockerJobStart,
      releaseJobStart,
    );
    const releaseJob = releaseWorkflow.slice(releaseJobStart);

    expect(dockerWorkflow).toContain("  workflow_call:");
    expect(dockerWorkflow).toContain("  pull_request:\n    branches:\n      - main");
    expect(dockerWorkflow.match(/default: false/g)).toHaveLength(2);
    expect(dockerWorkflow).not.toContain('    tags:\n      - "v*"');
    expect(dockerWorkflow).toContain("uses: dtolnay/rust-toolchain@1.98.0");
    expect(dockerWorkflow).toContain("enable=${{ inputs.push_image == true }}");
    expect(dockerWorkflow).toContain("push: ${{ inputs.push_image == true }}");
    expect(dockerJob).toContain("uses: ./.github/workflows/docker-publish.yml");
    expect(dockerJob).toContain("packages: write");
    expect(dockerJob).toContain("release_tag: ${{ needs.validate-tag.outputs.tag }}");
    expect(dockerJob).toContain("push_image: true");
    expect(releaseJob).toContain(
      "needs: [validate-tag, build-macos, build-windows, build-linux, publish-docker]",
    );
  });

  test("excludes Docker build records from release asset downloads", async () => {
    const workflow = await Bun.file(releaseWorkflowUrl).text();
    const releaseJob = workflow.slice(workflow.indexOf("  release:"));

    expect(releaseJob).toContain("- name: Download all artifacts");
    expect(releaseJob).toContain('pattern: "!*.dockerbuild"');
  });

  test("pins Docker validation and publishing to immutable commits", async () => {
    const workflow = await Bun.file(dockerWorkflowUrl).text();

    expect(workflow.match(/ref: refs\/tags\/\$\{\{ inputs\.release_tag \}\}/g)).toHaveLength(2);
    expect(workflow).toContain("ref: ${{ github.sha }}");
    expect(workflow).not.toContain("inputs.release_tag || github.ref");
  });

  test("requires a release tag and skips publishing work for validation-only runs", async () => {
    const workflow = await Bun.file(dockerWorkflowUrl).text();
    const validateJobStart = workflow.indexOf("  validate:");
    const buildJobStart = workflow.indexOf("  build:");

    expect(validateJobStart).toBeGreaterThanOrEqual(0);
    expect(buildJobStart).toBeGreaterThan(validateJobStart);

    const validateJob = workflow.slice(validateJobStart, buildJobStart);
    const buildJob = workflow.slice(buildJobStart);
    const guardStep = validateJob.indexOf("- name: Require release tag for image publishing");
    const checkoutStep = validateJob.indexOf("- name: Checkout release tag");

    expect(guardStep).toBeGreaterThanOrEqual(0);
    expect(checkoutStep).toBeGreaterThan(guardStep);
    expect(validateJob).toContain("if: inputs.push_image == true && inputs.release_tag == ''");
    expect(validateJob).toContain("if: inputs.push_image != true");
    expect(buildJob).toContain("  build:\n    if: inputs.push_image == true");
    expect(workflow).not.toContain("github.event.inputs");
  });

  test("does not infer reusable invocation from the caller event name", async () => {
    const workflow = await Bun.file(dockerWorkflowUrl).text();

    expect(workflow).not.toContain("github.event_name == 'workflow_call'");
  });

  test("does not dispatch Homebrew from Nightly or desktop build workflows", async () => {
    const workflows = await Promise.all(
      [nightlyWorkflowUrl, ...desktopWorkflowUrls].map((url) => Bun.file(url).text()),
    );

    for (const workflow of workflows) {
      expect(workflow).not.toContain("mcpmate_release");
      expect(workflow).not.toContain("dispatch-homebrew-tap.ts");
    }
  });
});

describe("inject-version action contract", () => {
  test("uses the canonical classifier for tag-driven releases without a configured-version fallback", async () => {
    const action = await Bun.file(injectVersionActionUrl).text();

    expect(action).toContain("bun packaging/desktop/release-tag.ts");
    expect(action).toContain('if [[ -n "$RELEASE_TAG" ]]');
    expect(action).toContain("jq -r '.appVersion'");
    expect(action).toContain('--configured-version "$CONFIGURED_VERSION"');
    expect(action).not.toContain("fall back");
    expect(action).not.toContain('VERSION="${VERSION%%-*}"');
  });
});

describe("desktop updater configuration", () => {
  test("uses the Admin updater endpoint without legacy activation fields", async () => {
    const [config, releaseOverlay] = await Promise.all([
      Bun.file(tauriConfigUrl).json(),
      Bun.file(tauriReleaseOverlayUrl).json(),
    ]);

    expect(config.plugins.updater.endpoints).toEqual(["https://public.mcp.umate.ai/updates/latest"]);
    expect(config.plugins.updater).not.toHaveProperty("active");
    expect(config.plugins.updater).not.toHaveProperty("dialog");
    expect(releaseOverlay).not.toHaveProperty("plugins");
    expect(releaseOverlay.bundle.createUpdaterArtifacts).toBe(true);
  });
});
