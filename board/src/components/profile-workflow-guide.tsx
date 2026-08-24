import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  BookOpenText,
  Check,
  ChevronLeft,
  Eye,
  MapPin,
  PanelRight,
  Pencil,
  Save,
  Trash2,
  Wrench,
} from "lucide-react";
import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type RefObject,
  type SyntheticEvent,
} from "react";
import { useTranslation } from "react-i18next";
import ReactMarkdown from "react-markdown";
import rehypeSanitize from "rehype-sanitize";
import remarkGfm from "remark-gfm";

import { ApiRequestError, configSuitsApi } from "../lib/api";
import {
  buildLineOffsets,
  canInsertAtWorkflowGuideBoundary,
  canDeleteWorkflowGuideCell,
  capabilitySource,
  externalReferenceSource,
  commitMarkdownCellSource,
  formatMarkdownCellSourceForEditor,
  markdownCellAfterInsert,
  markdownCellEditAnchor,
  parseWorkflowGuide,
  sanitizeWorkflowGuideMarkdown,
  splitWorkflowGuideDocument,
  stripLeadingSkillFrontMatter,
  IN_PLACE_MARKDOWN_SNIPPET,
  type WorkflowGuideDocumentCell,
  type WorkflowGuideParseResult,
} from "../lib/workflow-guide-directive";
import type {
  WorkflowGuide,
  WorkflowGuideCapability,
  WorkflowGuideExternalDocument,
  WorkflowGuidePackageCategory,
  WorkflowGuidePackageFile,
  WorkflowGuideReclamationConfirmation,
} from "../lib/types";
import type { WorkflowCapabilityOption } from "../lib/profile-workflow-specification";
import { cn } from "../lib/utils";
import { notifyError, notifySuccess } from "../lib/notify";
import { BulkSelectionHeader } from "./bulk-selection";
import { CardListScrollBody } from "./card-list-scroll-body";
import { PROFILE_EDITOR_SIDEBAR_SCROLL_CLASS } from "./capsule-stripe-list";
import {
  COMPOSER_SHELL_CLASS,
  GUIDE_ACTION_BUTTON_CLASS,
  GUIDE_ICON_BUTTON_CLASS,
  GUIDE_SAVE_BUTTON_CLASS,
  GuideBoundaryInsert,
  GuideCapabilityBlock,
  GuideCapabilityFields,
  GuideComposerHeader,
} from "./workflow-guide-boundary-insert";
import { Button } from "./ui/button";
import { Input } from "./ui/input";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "./ui/tooltip";
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
import { Textarea } from "./ui/textarea";
import { ResizableSplitPane } from "./resizable-split-pane";

interface ProfileWorkflowGuideProps {
  profileId: string;
  capabilities: WorkflowCapabilityOption[];
  capabilitiesLoading?: boolean;
  onDirtyChange?: (dirty: boolean) => void;
}

const GUIDE_NAV_DOCUMENT_CLASS =
  "flex w-full items-center gap-1.5 px-3 py-1.5 text-left text-xs hover:bg-muted focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-inset focus-visible:ring-ring";
const GUIDE_NAV_NESTED_CLASS =
  "flex w-full items-center gap-1.5 py-1.5 pr-3 text-left text-xs hover:bg-muted focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-inset focus-visible:ring-ring";
const GUIDE_INSPECTOR_ROW_CLASS =
  "group rounded-sm px-1.5 py-1 text-foreground/70 transition-colors hover:text-foreground focus-within:text-foreground";
const GUIDE_INSPECTOR_NAV_NESTED_CLASS =
  "flex w-full items-center gap-1.5 py-1.5 pr-3 text-left text-xs hover:font-medium focus-visible:font-medium focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-inset focus-visible:ring-ring";

const GUIDE_NAV_INDENT_STEP_REM = 0.75;

/** First outline child sits one step beyond the document row (px-3). */
function guideNavOutlineIndent(depth: number) {
  return {
    paddingLeft: `${GUIDE_NAV_INDENT_STEP_REM * (1 + depth)}rem`,
  };
}

/** Inspector hover children use a single step from the parent row label. */
function guideNavInspectorIndent() {
  return { paddingLeft: `${GUIDE_NAV_INDENT_STEP_REM}rem` };
}

function outlineHeadingDepth(
  headings: Array<{ level: number }>,
  heading: { level: number },
) {
  const baseLevel = headings[0]?.level ?? 1;
  return heading.level - baseLevel + 1;
}

function outlineHeadingsForDocument(
  title: string,
  headings: Array<{ level: number; text: string; offset: number }>,
) {
  const normalizedTitle = title.trim().toLowerCase();
  if (
    headings.length > 0 &&
    headings[0].level === 1 &&
    headings[0].text.trim().toLowerCase() === normalizedTitle
  ) {
    return headings.slice(1);
  }
  return headings;
}

function guidePlaceAriaLabel(
  index: number,
  path: string,
  placeLabel: string,
) {
  return `${placeLabel} ${index + 1} · ${path}`;
}

export function ProfileWorkflowGuide({
  profileId,
  capabilities,
  capabilitiesLoading = false,
  onDirtyChange,
}: ProfileWorkflowGuideProps) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [outlineOpen, setOutlineOpen] = useState(false);
  const [insertComposerOffset, setInsertComposerOffset] = useState<
    number | null
  >(null);
  const editorRef = useRef<HTMLTextAreaElement>(null);
  const selectionRef = useRef({ start: 0, end: 0 });
  const loadedGuideRef = useRef<{
    profileId: string;
    guideRevision: number;
  } | null>(null);
  const rootDirtyRef = useRef(false);
  const guideQuery = useQuery({
    queryKey: ["workflowGuide", profileId],
    queryFn: () => configSuitsApi.getWorkflowGuide(profileId),
  });
  const [markdown, setMarkdown] = useState("");
  const [rootDirty, setRootDirty] = useState(false);
  const [packageFiles, setPackageFiles] = useState<WorkflowGuidePackageFile[]>(
    [],
  );
  const [activeDocumentPath, setActiveDocumentPath] = useState("SKILL.md");
  const [externalDocuments, setExternalDocuments] = useState<
    Record<string, WorkflowGuideExternalDocument>
  >({});
  const [externalBaselines, setExternalBaselines] = useState<
    Record<string, string>
  >({});
  const [editorMode, setEditorMode] = useState<"notebook" | "preview">(
    "notebook",
  );
  const [editingCellId, setEditingCellId] = useState<string | null>(null);
  // Frozen character range for the open markdown cell edit; typing inside the
  // form replaces this exact range instead of racing the live cell re-split.
  const [editSession, setEditSession] = useState<{
    start: number;
    end: number;
    // Snapshot the cell had when the editor opened; cancel restores it, or
    // removes the whole range for freshly inserted in-place cells.
    original: string;
    inserted: boolean;
  } | null>(null);
  const [pendingLocation, setPendingLocation] = useState<{
    path: string;
    offset: number;
  } | null>(null);
  const [pendingCellDelete, setPendingCellDelete] =
    useState<WorkflowGuideDocumentCell | null>(null);
  const [pendingReclamation, setPendingReclamation] = useState<{
    packageFiles: WorkflowGuidePackageFile[];
    capabilities: WorkflowGuideCapability[];
  } | null>(null);
  const editorOffsetRef = useRef(0);
  useEffect(() => {
    if (!guideQuery.data) return;
    setPackageFiles(guideQuery.data.package_files);
    const profileChanged = loadedGuideRef.current?.profileId !== profileId;
    const revisionChanged =
      loadedGuideRef.current?.guideRevision !== guideQuery.data.guide_revision;
    if (profileChanged || (revisionChanged && !rootDirtyRef.current)) {
      const normalizedMarkdown = sanitizeWorkflowGuideMarkdown(
        stripLeadingSkillFrontMatter(guideQuery.data.markdown).body,
      );
      setMarkdown(normalizedMarkdown);
      selectionRef.current = {
        start: normalizedMarkdown.length,
        end: normalizedMarkdown.length,
      };
      closeCellEditor();
      rootDirtyRef.current = false;
      setRootDirty(false);
    }
    if (profileChanged) {
      setActiveDocumentPath("SKILL.md");
      setExternalDocuments({});
      setExternalBaselines(
        Object.fromEntries(
          (guideQuery.data.documents ?? []).map((document) => [
            document.relative_path,
            sanitizeWorkflowGuideMarkdown(document.markdown),
          ]),
        ),
      );
      setEditorMode("notebook");
      setPendingLocation(null);
      setPendingReclamation(null);
    }
    loadedGuideRef.current = {
      profileId,
      guideRevision: guideQuery.data.guide_revision,
    };
  }, [guideQuery.data, profileId]);

  const loadedExternalDocuments = useMemo(
    () =>
      Object.fromEntries(
        (guideQuery.data?.documents ?? []).map((document) => [
          document.relative_path,
          {
            ...document,
            markdown: sanitizeWorkflowGuideMarkdown(document.markdown),
          },
        ]),
      ),
    [guideQuery.data?.documents],
  );
  const resolvedExternalDocuments = useMemo(
    () => ({ ...loadedExternalDocuments, ...externalDocuments }),
    [loadedExternalDocuments, externalDocuments],
  );
  const externalDirtyPaths = useMemo(
    () =>
      Object.entries(resolvedExternalDocuments)
        .filter(
          ([path, document]) =>
            externalBaselines[path] !== undefined &&
            document.markdown !== externalBaselines[path],
        )
        .map(([path]) => path),
    [resolvedExternalDocuments, externalBaselines],
  );
  const hasDirtyChanges = rootDirty || externalDirtyPaths.length > 0;
  useEffect(() => {
    onDirtyChange?.(hasDirtyChanges);
  }, [hasDirtyChanges, onDirtyChange]);
  useEffect(() => {
    if (!hasDirtyChanges) return;
    const onBeforeUnload = (event: BeforeUnloadEvent) => {
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", onBeforeUnload);
    return () => window.removeEventListener("beforeunload", onBeforeUnload);
  }, [hasDirtyChanges]);
  const parsedSkillGuide = useMemo(() => parseWorkflowGuide(markdown), [markdown]);
  const parsedDirtyExternalGuides = useMemo(() => {
    const guides = new Map<string, WorkflowGuideParseResult>();
    for (const path of externalDirtyPaths) {
      guides.set(
        path,
        parseWorkflowGuide(resolvedExternalDocuments[path]?.markdown ?? ""),
      );
    }
    return guides;
  }, [externalDirtyPaths, resolvedExternalDocuments]);
  const hasBlockingGuideErrors = useMemo(() => {
    if (rootDirty && parsedSkillGuide.errors.length > 0) return true;
    return externalDirtyPaths.some(
      (path) => (parsedDirtyExternalGuides.get(path)?.errors.length ?? 0) > 0,
    );
  }, [externalDirtyPaths, parsedDirtyExternalGuides, parsedSkillGuide.errors, rootDirty]);
  const reachableExternalDocuments = useMemo(
    () =>
      Object.values(loadedExternalDocuments).map(
        (document) => resolvedExternalDocuments[document.relative_path] ?? document,
      ),
    [loadedExternalDocuments, resolvedExternalDocuments],
  );
  const activeExternalDocument =
    activeDocumentPath === "SKILL.md"
      ? null
      : resolvedExternalDocuments[activeDocumentPath];
  const activeMarkdown = activeExternalDocument?.markdown ?? markdown;
  const notebookMarkdown = useMemo(
    () => stripLeadingSkillFrontMatter(activeMarkdown).body,
    [activeMarkdown],
  );
  // `normalize: false` keeps an open edit session's character offsets exact:
  // sanitizing can insert heading newlines and shift the range out of sync.
  const updateActiveMarkdown = (
    updater: (current: string) => string,
    options?: { normalize?: boolean },
  ) => {
    const apply = (current: string) =>
      options?.normalize === false
        ? updater(current)
        : sanitizeWorkflowGuideMarkdown(updater(current));
    if (activeDocumentPath !== "SKILL.md") {
      setExternalDocuments((current) => {
        const existing =
          current[activeDocumentPath] ??
          loadedExternalDocuments[activeDocumentPath];
        if (!existing) return current;
        return {
          ...current,
          [activeDocumentPath]: {
            ...existing,
            markdown: apply(existing.markdown),
          },
        };
      });
      return;
    }
    setMarkdown((current) => apply(current));
    rootDirtyRef.current = true;
    setRootDirty(true);
  };
  const strippedRootBody = useMemo(
    () => stripLeadingSkillFrontMatter(markdown).body,
    [markdown],
  );
  const parsedRootBodyGuide = useMemo(
    () => parseWorkflowGuide(strippedRootBody),
    [strippedRootBody],
  );
  const guide = useMemo(
    () => parseWorkflowGuide(notebookMarkdown),
    [notebookMarkdown],
  );
  const mainGuideTitle = useMemo(
    () => parsedRootBodyGuide.headings[0]?.text ?? "SKILL.md",
    [parsedRootBodyGuide],
  );
  const referenceMarkdownFiles = useMemo(
    () =>
      packageFiles.filter(
        (file) => file.category === "reference" && file.extension === "md",
      ),
    [packageFiles],
  );
  const parsedExternalBodyGuides = useMemo(() => {
    const guides = new Map<string, WorkflowGuideParseResult>();
    for (const file of referenceMarkdownFiles) {
      const document =
        resolvedExternalDocuments[file.relative_path] ??
        loadedExternalDocuments[file.relative_path];
      if (!document) continue;
      guides.set(
        file.relative_path,
        parseWorkflowGuide(stripLeadingSkillFrontMatter(document.markdown).body),
      );
    }
    return guides;
  }, [loadedExternalDocuments, referenceMarkdownFiles, resolvedExternalDocuments]);
  const outlineSections = useMemo(
    () => [
      {
        path: "SKILL.md",
        title: mainGuideTitle,
        badge: "SKILL.md",
        headings: outlineHeadingsForDocument(
          mainGuideTitle,
          parsedRootBodyGuide.headings,
        ),
      },
      ...referenceMarkdownFiles.map((file) => {
        const parsed = parsedExternalBodyGuides.get(file.relative_path);
        return {
          path: file.relative_path,
          title: file.title,
          badge: "reference" as const,
          headings: parsed
            ? outlineHeadingsForDocument(file.title, parsed.headings)
            : [],
        };
      }),
    ],
    [mainGuideTitle, parsedExternalBodyGuides, parsedRootBodyGuide.headings, referenceMarkdownFiles],
  );
  const documentCells = useMemo(
    () => splitWorkflowGuideDocument(notebookMarkdown, activeDocumentPath),
    [activeDocumentPath, notebookMarkdown],
  );
  const editAnchor = useMemo(
    () =>
      editSession ? markdownCellEditAnchor(documentCells, editSession) : null,
    [documentCells, editSession],
  );
  const editAnchorCell =
    editAnchor?.mode === "replace" ? documentCells[editAnchor.index] : undefined;
  useEffect(() => {
    if (!pendingLocation || pendingLocation.path !== activeDocumentPath) return;
    const cell = documentCells.find(
      (candidate) =>
        candidate.startOffset <= pendingLocation.offset &&
        candidate.endOffset >= pendingLocation.offset,
    );
    if (!cell) return;
    document
      .getElementById(`guide-cell-${cell.id}`)
      ?.scrollIntoView({ behavior: "smooth", block: "center" });
    setPendingLocation(null);
  }, [activeDocumentPath, documentCells, pendingLocation]);
  useEffect(() => {
    setInsertComposerOffset(null);
  }, [activeDocumentPath, editorMode]);
  const documentSources = useMemo(
    () => [
      { path: "SKILL.md", title: "SKILL.md", markdown },
      ...reachableExternalDocuments.map((document) => ({
        path: document.relative_path,
        title: document.title,
        markdown: document.markdown,
      })),
    ],
    [markdown, reachableExternalDocuments],
  );
  const parsedDocumentGuides = useMemo(() => {
    const guides = new Map<string, WorkflowGuideParseResult>();
    for (const source of documentSources) {
      guides.set(source.path, parseWorkflowGuide(source.markdown));
    }
    return guides;
  }, [documentSources]);
  const capabilityOccurrences = useMemo(
    () => collectCapabilityOccurrences(documentSources, parsedDocumentGuides),
    [documentSources, parsedDocumentGuides],
  );
  const materialOccurrences = useMemo(
    () => collectMaterialOccurrences(documentSources, parsedDocumentGuides),
    [documentSources, parsedDocumentGuides],
  );
  const captureReclamation = (error: unknown) => {
    if (
      !(error instanceof ApiRequestError) ||
      error.code !== "workflow_guide_reclamation_required"
    ) {
      return false;
    }
    setPendingReclamation({
      packageFiles: error.details?.packageFiles ?? [],
      capabilities: error.details?.capabilities ?? [],
    });
    return true;
  };
  const captureCommittedCleanup = (error: unknown) => {
    if (
      !(error instanceof ApiRequestError) ||
      error.code !== "workflow_guide_trash_cleanup_pending"
    ) {
      return false;
    }
    void queryClient.invalidateQueries({
      queryKey: ["workflowGuide", profileId],
    });
    void queryClient.invalidateQueries({
      queryKey: ["workflowSpecification", profileId],
    });
    notifyError(
      t("profiles:detail.workflow.guide.cleanupPending", {
        defaultValue:
          "Changes were saved, but Trash cleanup is pending. Run Repair to finish cleanup.",
      }),
    );
    return true;
  };
  const saveMutation = useMutation({
    mutationFn: (
      reclamationConfirmation?: WorkflowGuideReclamationConfirmation,
    ) => {
      const guide = queryClient.getQueryData<WorkflowGuide>([
        "workflowGuide",
        profileId,
      ]);
      if (!guide) throw new Error("Workflow Guide is not loaded yet");
      return configSuitsApi.saveWorkflowGuide({
        profile_id: profileId,
        expected_guide_revision: guide.guide_revision,
        markdown: sanitizeWorkflowGuideMarkdown(markdown),
        reclamation_confirmation: reclamationConfirmation,
      });
    },
    onSuccess: async (saved) => {
      await queryClient.invalidateQueries({
        queryKey: ["workflowGuide", profileId],
      });
      await queryClient.invalidateQueries({
        queryKey: ["workflowSpecification", profileId],
      });
      notifySuccess(
        t("profiles:detail.workflow.guide.saved", {
          defaultValue: "Workflow Guide saved",
        }),
      );
      setMarkdown(saved.guide.markdown);
      rootDirtyRef.current = false;
      setRootDirty(false);
      setPendingReclamation(null);
    },
    onError: (error) => {
      if (captureCommittedCleanup(error)) return;
      if (captureReclamation(error)) return;
      notifyError(
        error instanceof Error
          ? error.message
          : t("profiles:detail.workflow.guide.saveFailed", {
            defaultValue: "Failed to save Workflow Guide",
          }),
      );
    },
  });
  const previewMutation = useMutation({
    mutationFn: () =>
      configSuitsApi.previewWorkflowGuide({
        profile_id: profileId,
        relative_path: activeExternalDocument?.relative_path,
        markdown: sanitizeWorkflowGuideMarkdown(activeMarkdown),
      }),
    onError: (error) => {
      if (captureCommittedCleanup(error)) return;
      notifyError(
        error instanceof Error
          ? error.message
          : t("profiles:detail.workflow.guide.previewFailed", {
            defaultValue: "Failed to render Skill Preview",
          }),
      );
    },
  });
  // Depend on `mutate` (a stable instance method), not the mutation result:
  // the result object gets a new identity on every status transition and
  // would re-arm this debounce into an endless preview loop.
  const previewMutate = previewMutation.mutate;
  useEffect(() => {
    if (editorMode !== "preview") return;
    const timer = window.setTimeout(() => {
      previewMutate();
    }, 400);
    return () => window.clearTimeout(timer);
  }, [activeDocumentPath, activeMarkdown, editorMode, previewMutate]);
  const externalDocumentMutation = useMutation({
    mutationFn: (file: WorkflowGuidePackageFile) =>
      configSuitsApi.getWorkflowGuideExternalDocument(
        profileId,
        file.package_file_id,
      ),
    onSuccess: (document) => {
      const markdown = sanitizeWorkflowGuideMarkdown(document.markdown);
      setExternalDocuments((current) => ({
        ...current,
        [document.relative_path]: {
          ...document,
          markdown,
        },
      }));
      setExternalBaselines((current) => ({
        ...current,
        [document.relative_path]: markdown,
      }));
      setActiveDocumentPath(document.relative_path);
      closeCellEditor();
    },
    onError: (error) => {
      if (captureCommittedCleanup(error)) return;
      notifyError(
        error instanceof Error
          ? error.message
          : t("profiles:detail.workflow.guide.documentLoadFailed", {
            defaultValue: "Failed to load external Markdown document",
          }),
      );
    },
  });
  const packageFileMutation = useMutation({
    mutationFn: (draft: {
      title: string;
      category: WorkflowGuidePackageCategory;
      file: File;
    }) => {
      const formData = new FormData();
      formData.append("profile_id", profileId);
      formData.append(
        "expected_guide_revision",
        String(guideQuery.data!.guide_revision),
      );
      formData.append("title", draft.title.trim() || draft.file.name);
      formData.append("category", draft.category);
      formData.append("file", draft.file);
      return configSuitsApi.uploadWorkflowGuidePackageFile(formData);
    },
    onSuccess: (saved) => {
      const file = saved.package_file;
      queryClient.setQueryData(["workflowGuide", profileId], saved.guide);
      setPackageFiles(saved.guide.package_files);
      insert(`[${file.title}](${file.relative_path})`);
      notifySuccess(
        t("profiles:detail.workflow.guide.fileSaved", {
          defaultValue: "Package file saved",
        }),
      );
    },
    onError: (error) => {
      if (captureCommittedCleanup(error)) return;
      notifyError(
        error instanceof Error
          ? error.message
          : t("profiles:detail.workflow.guide.fileSaveFailed", {
            defaultValue: "Failed to save package file",
          }),
      );
    },
  });
  const saveExternalDocumentMutation = useMutation({
    mutationFn: (input: {
      path: string;
      markdown: string;
      reclamationConfirmation?: WorkflowGuideReclamationConfirmation;
    }) => {
      const guide = queryClient.getQueryData<WorkflowGuide>([
        "workflowGuide",
        profileId,
      ]);
      const file = guide?.package_files.find(
        (candidate) => candidate.relative_path === input.path,
      );
      if (!guide || !file) {
        throw new Error("External Markdown document is no longer available");
      }
      const formData = new FormData();
      formData.append("profile_id", profileId);
      formData.append("package_file_id", file.package_file_id);
      formData.append("expected_file_revision", String(file.file_revision));
      formData.append("expected_guide_revision", String(guide.guide_revision));
      if (input.reclamationConfirmation) {
        formData.append(
          "reclamation_confirmation",
          JSON.stringify(input.reclamationConfirmation),
        );
      }
      formData.append("title", file.title);
      formData.append("category", "reference");
      formData.append(
        "file",
        new File(
          [sanitizeWorkflowGuideMarkdown(input.markdown)],
          input.path.split("/").pop() ?? "reference.md",
          { type: "text/markdown" },
        ),
      );
      return configSuitsApi.uploadWorkflowGuidePackageFile(formData);
    },
    onSuccess: async (saved, variables) => {
      const file = saved.package_file;
      setExternalDocuments((current) => {
        const existing = current[variables.path];
        if (!existing) return current;
        return {
          ...current,
          [file.relative_path]: {
            ...existing,
            title: file.title,
            file_revision: file.file_revision,
            relative_path: file.relative_path,
          },
        };
      });
      setExternalBaselines((current) => ({
        ...current,
        [file.relative_path]: variables.markdown,
      }));
      queryClient.setQueryData(["workflowGuide", profileId], saved.guide);
      setPackageFiles(saved.guide.package_files);
      await queryClient.invalidateQueries({
        queryKey: ["workflowSpecification", profileId],
      });
      notifySuccess(
        t("profiles:detail.workflow.guide.documentSaved", {
          defaultValue: "External Markdown document saved",
        }),
      );
      setPendingReclamation(null);
    },
    onError: (error) => {
      if (captureCommittedCleanup(error)) return;
      if (captureReclamation(error)) return;
      notifyError(
        error instanceof Error
          ? error.message
          : t("profiles:detail.workflow.guide.documentSaveFailed", {
            defaultValue: "Failed to save external Markdown document",
          }),
      );
    },
  });
  const createExternalDocumentMutation = useMutation({
    mutationFn: async (titleInput: string) => {
      const title = titleInput.trim();
      if (!title)
        throw new Error("External Markdown document title is required");
      const formData = new FormData();
      formData.append("profile_id", profileId);
      formData.append(
        "expected_guide_revision",
        String(guideQuery.data!.guide_revision),
      );
      formData.append("title", title);
      formData.append("category", "reference");
      const markdown = `# ${title}\n`;
      formData.append(
        "file",
        new File([markdown], `${title}.md`, { type: "text/markdown" }),
      );
      const saved =
        await configSuitsApi.uploadWorkflowGuidePackageFile(formData);
      const file = saved.package_file;
      return {
        saved,
        document: {
          package_file_id: file.package_file_id,
          file_revision: file.file_revision,
          title: file.title,
          relative_path: file.relative_path,
          markdown,
        },
      };
    },
    onSuccess: ({ saved, document }) => {
      queryClient.setQueryData(["workflowGuide", profileId], saved.guide);
      setPackageFiles(saved.guide.package_files);
      setExternalDocuments((current) => ({
        ...current,
        [document.relative_path]: document,
      }));
      setExternalBaselines((current) => ({
        ...current,
        [document.relative_path]: document.markdown,
      }));
      insert(externalReferenceSource(document.title, document.relative_path));
      setActiveDocumentPath(document.relative_path);
      setEditorMode("notebook");
      closeCellEditor();
      notifySuccess(
        t("profiles:detail.workflow.guide.documentCreated", {
          defaultValue: "External Markdown document created",
        }),
      );
    },
    onError: (error) => {
      if (captureCommittedCleanup(error)) return;
      notifyError(
        error instanceof Error
          ? error.message
          : t("profiles:detail.workflow.guide.documentCreateFailed", {
            defaultValue: "Failed to create external Markdown document",
          }),
      );
    },
  });
  const repairMutation = useMutation({
    mutationFn: () => configSuitsApi.repairWorkflowGuide(profileId),
    onSuccess: async () => {
      await queryClient.invalidateQueries({
        queryKey: ["workflowGuide", profileId],
      });
      notifySuccess(
        t("profiles:detail.workflow.guide.repaired", {
          defaultValue: "Skill package repaired",
        }),
      );
    },
    onError: (error) =>
      notifyError(
        error instanceof Error
          ? error.message
          : t("profiles:detail.workflow.guide.repairFailed", {
            defaultValue: "Failed to repair Skill package",
          }),
      ),
  });

  const insert = (value: string) => {
    const editor = editorRef.current;
    const { start, end } = selectionRef.current;
    updateActiveMarkdown((current) => {
      // Keep inserted content on its own line; paragraph separation for
      // capability blocks is guaranteed by the projector, not the authoring
      // source, so no blank-line padding is added here.
      const prefix = start > 0 && current[start - 1] !== "\n" ? "\n" : "";
      const suffix = end < current.length && current[end] !== "\n" ? "\n" : "";
      return `${current.slice(0, start)}${prefix}${value}${suffix}${current.slice(end)}`;
    });
    if (!editor) return;
    requestAnimationFrame(() => {
      editor.focus();
      const cursor = start + value.length;
      const localCursor = cursor - editorOffsetRef.current;
      editor.setSelectionRange(localCursor, localCursor);
      selectionRef.current = { start: cursor, end: cursor };
    });
  };

  const updateCell = (cell: WorkflowGuideDocumentCell, value: string) => {
    updateActiveMarkdown(
      (current) =>
        `${current.slice(0, cell.startOffset)}${value}${current.slice(cell.endOffset)}`,
    );
  };

  const updateCapability = (
    cell: WorkflowGuideDocumentCell,
    name: string,
    exposure: "direct" | "meta_on_demand",
    guide: string,
  ) => {
    if (!cell.capability) return;
    updateCell(cell, `${capabilitySource(name, exposure, guide)}\n`);
  };

  const updateExternalReference = (
    cell: WorkflowGuideDocumentCell,
    title: string,
    guide: string,
  ) => {
    if (!cell.externalReference) return;
    updateCell(
      cell,
      `${externalReferenceSource(title, cell.externalReference.relativePath, guide)}\n`,
    );
  };

  const deleteCell = (cell: WorkflowGuideDocumentCell) => {
    closeCellEditor();
    updateActiveMarkdown(
      (current) =>
        `${current.slice(0, cell.startOffset)}${current.slice(cell.endOffset)}`,
    );
  };

  const beginCellEdit = (
    cell: WorkflowGuideDocumentCell,
    options?: { inserted?: boolean },
  ) => {
    const offset = cell.startOffset;
    editorOffsetRef.current = offset;
    const cursor = cell.endOffset;
    selectionRef.current = { start: cursor, end: cursor };
    setEditingCellId(cell.id);
    if (cell.kind === "markdown") {
      setEditSession({
        start: cell.startOffset,
        end: cell.endOffset,
        original: cell.source,
        inserted: options?.inserted ?? false,
      });
    } else {
      setEditSession(null);
    }
  };
  const closeCellEditor = () => {
    setEditingCellId(null);
    setEditSession(null);
  };
  const cancelCellEdit = () => {
    if (!editSession) {
      closeCellEditor();
      return;
    }
    const { start, end, inserted } = editSession;
    if (inserted) {
      updateActiveMarkdown(
        (current) => current.slice(0, start) + current.slice(end),
      );
    }
    closeCellEditor();
  };
  const finalizeCellEdit = (editorValue: string) => {
    if (!editSession) {
      closeCellEditor();
      return;
    }
    const { start, end } = editSession;
    const isolated = commitMarkdownCellSource(editorValue);
    updateActiveMarkdown(
      (current) => `${current.slice(0, start)}${isolated}${current.slice(end)}`,
    );
    closeCellEditor();
  };
  const markdownEditForm = editSession ? (
    <GuideMarkdownEditForm
      editorRef={editorRef}
      initialSource={notebookMarkdown.slice(
        editSession.start,
        editSession.end,
      )}
      onCancel={cancelCellEdit}
      onDelete={
        editAnchorCell &&
          canDeleteWorkflowGuideCell(guide.headings, editAnchorCell)
          ? () => setPendingCellDelete(editAnchorCell)
          : undefined
      }
      onDone={finalizeCellEdit}
      onSelect={(event) => trackSelection(event, editSession.start)}
    />
  ) : null;

  const insertInPlaceMarkdownAt = (offset: number) => {
    let nextMarkdown = notebookMarkdown;
    updateActiveMarkdown(
      (current) => {
        const prefix = offset > 0 && current[offset - 1] !== "\n" ? "\n" : "";
        const suffix =
          offset < current.length && current[offset] !== "\n" ? "\n" : "";
        nextMarkdown = `${current.slice(0, offset)}${prefix}${IN_PLACE_MARKDOWN_SNIPPET}${suffix}${current.slice(offset)}`;
        return nextMarkdown;
      },
      { normalize: false },
    );
    const cell = markdownCellAfterInsert(
      nextMarkdown,
      offset,
      IN_PLACE_MARKDOWN_SNIPPET,
      activeDocumentPath,
    );
    if (cell) beginCellEdit(cell, { inserted: true });
  };

  const trackSelection = (
    event: SyntheticEvent<HTMLTextAreaElement>,
    offset: number,
  ) => {
    const target = event.currentTarget;
    editorOffsetRef.current = offset;
    selectionRef.current = {
      start: offset + target.selectionStart,
      end: offset + target.selectionEnd,
    };
  };

  const insertCapability = (
    capability: WorkflowCapabilityOption,
    exposure: "direct" | "meta_on_demand",
    guide: string,
  ) => {
    insert(capabilitySource(capability.label, exposure, guide));
  };
  const openOccurrence = (path: string, offset: number) => {
    closeCellEditor();
    setPendingLocation({ path, offset });
    if (path === "SKILL.md") {
      setActiveDocumentPath(path);
      return;
    }
    if (loadedExternalDocuments[path] || resolvedExternalDocuments[path]) {
      setActiveDocumentPath(path);
      return;
    }
    const file = guideQuery.data?.package_files.find(
      (candidate) => candidate.relative_path === path,
    );
    if (file) externalDocumentMutation.mutate(file);
  };
  const returnToMainGuide = () => {
    openOccurrence("SKILL.md", 0);
  };
  const saveAllDirty = async (
    reclamationConfirmation?: WorkflowGuideReclamationConfirmation,
  ) => {
    for (const path of externalDirtyPaths) {
      const document = resolvedExternalDocuments[path];
      if (!document) continue;
      try {
        await saveExternalDocumentMutation.mutateAsync({
          path,
          markdown: document.markdown,
          reclamationConfirmation,
        });
      } catch {
        // Errors are surfaced by the mutation's onError handler;
        // stop the sequential save so the failure stays visible.
        return;
      }
    }
    if (!rootDirty) return;
    try {
      await saveMutation.mutateAsync(reclamationConfirmation);
    } catch {
      // Errors are surfaced by the mutation's onError handler.
    }
  };
  const confirmReclamation = () => {
    if (!pendingReclamation) return;
    const confirmation: WorkflowGuideReclamationConfirmation = {
      package_files: pendingReclamation.packageFiles.map((file) => ({
        package_file_id: file.package_file_id,
        file_revision: file.file_revision,
      })),
      capability_names: pendingReclamation.capabilities.map(
        (capability) => capability.name,
      ),
    };
    setPendingReclamation(null);
    void saveAllDirty(confirmation);
  };

  if (guideQuery.isLoading) {
    return (
      <p className="p-4 text-sm text-muted-foreground">
        {t("common:loading", { defaultValue: "Loading..." })}
      </p>
    );
  }
  if (guideQuery.isError || !guideQuery.data) {
    return (
      <p className="p-4 text-sm text-destructive">
        {t("profiles:detail.workflow.guide.loadFailed", {
          defaultValue: "Failed to load Workflow Guide.",
        })}
      </p>
    );
  }

  return (
    <section
      className="flex min-h-0 flex-1 flex-col overflow-hidden"
      aria-label={t("profiles:detail.workflow.guide.regionLabel", {
        defaultValue: "Workflow Guide",
      })}
    >
      <ResizableSplitPane
        className="min-h-0 flex-1"
        dividerAriaLabel={t("profiles:detail.workflow.guide.resizeInspectorPanel", {
          defaultValue: "Resize capabilities and materials panel",
        })}
        initialLeftWidth={280}
        minLeftWidth={208}
        maxLeftWidth={520}
        preferRightPanelSpace
      >
        <div className="flex min-h-0 flex-col">
          <div className="shrink-0 p-3">
            <BulkSelectionHeader
              className="mb-0"
              title={t("profiles:detail.workflow.guide.inspectorTitle", {
                defaultValue: "Capabilities & Materials",
              })}
              description={t(
                "profiles:detail.workflow.guide.inspectorDescription",
                {
                  defaultValue:
                    "Capability and material references in this workflow guide.",
                },
              )}
              showModeToggle={false}
            />
          </div>
          <aside
            className="flex min-h-0 flex-1 flex-col overflow-hidden"
            aria-label={t("profiles:detail.workflow.guide.inspectorLabel", {
              defaultValue: "Guide inspector",
            })}
          >
            <CardListScrollBody className={PROFILE_EDITOR_SIDEBAR_SCROLL_CLASS}>
              <div className="px-2 py-3.5">
                <section>
                  <h4 className="px-1.5 text-xs font-medium">
                    {t("profiles:detail.workflow.guide.capabilities", {
                      defaultValue: "Capabilities",
                    })}{" "}
                    ({capabilityOccurrences.size})
                  </h4>
                  <div className="mt-1 space-y-0.5">
                    {[...capabilityOccurrences.entries()].map(
                      ([name, occurrences]) => {
                        const placeLabel = t(
                          "profiles:detail.workflow.guide.place",
                          { defaultValue: "Place" },
                        );
                        return (
                          <div className={GUIDE_INSPECTOR_ROW_CLASS} key={name}>
                            <div className="flex items-center gap-1.5">
                              <p
                                className="min-w-0 flex-1 truncate text-xs font-medium"
                                title={name}
                              >
                                {name}
                              </p>
                              <span className="shrink-0 text-[10px] text-muted-foreground/70 transition-colors group-hover:text-muted-foreground">
                                {occurrences.length}
                              </span>
                            </div>
                            <div className="grid grid-rows-[0fr] overflow-hidden opacity-0 transition-[grid-template-rows,opacity] duration-150 group-hover:grid-rows-[1fr] group-hover:opacity-100 group-focus-within:grid-rows-[1fr] group-focus-within:opacity-100">
                              <div className="min-h-0">
                                <div className="pt-1.5">
                                  {occurrences.map((occurrence, index) => {
                                    const label = guidePlaceAriaLabel(
                                      index,
                                      occurrence.path,
                                      placeLabel,
                                    );
                                    return (
                                      <button
                                        className={GUIDE_INSPECTOR_NAV_NESTED_CLASS}
                                        key={`${occurrence.path}-${occurrence.offset}`}
                                        onClick={() =>
                                          openOccurrence(
                                            occurrence.path,
                                            occurrence.offset,
                                          )
                                        }
                                        style={guideNavInspectorIndent()}
                                        title={label}
                                        type="button"
                                      >
                                        <MapPin className="h-3 w-3 shrink-0 opacity-50" />
                                        <span className="min-w-0 flex-1 truncate">
                                          {occurrence.path}
                                        </span>
                                      </button>
                                    );
                                  })}
                                </div>
                              </div>
                            </div>
                          </div>
                        );
                      },
                    )}
                    {capabilityOccurrences.size === 0 ? (
                      <p className="px-1.5 py-1 text-xs text-muted-foreground">
                        {t("profiles:detail.workflow.guide.noCapabilities", {
                          defaultValue: "No capability references yet.",
                        })}
                      </p>
                    ) : null}
                  </div>
                </section>
                <section className="mt-4">
                  <h4 className="px-1.5 text-xs font-medium">
                    {t("profiles:detail.workflow.guide.materials", {
                      defaultValue: "Materials",
                    })}{" "}
                    ({materialOccurrences.size})
                  </h4>
                  <div className="mt-1 space-y-0.5">
                    {[...materialOccurrences.entries()].map(
                      ([path, occurrences]) => {
                        const file = packageFiles.find(
                          (candidate) => candidate.relative_path === path,
                        );
                        const isReferenceMarkdown =
                          file?.category === "reference" &&
                          file.extension === "md";
                        const materialTitle = file?.title ?? path;
                        const placeLabel = t(
                          "profiles:detail.workflow.guide.place",
                          { defaultValue: "Place" },
                        );
                        return (
                          <div className={GUIDE_INSPECTOR_ROW_CLASS} key={path}>
                            <div className="flex items-center gap-1.5">
                              {isReferenceMarkdown ? (
                                <button
                                  className="min-w-0 flex-1 truncate text-left text-xs font-medium focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-inset focus-visible:ring-ring"
                                  onClick={() => openOccurrence(path, 0)}
                                  title={materialTitle}
                                  type="button"
                                >
                                  {file.title}
                                </button>
                              ) : (
                                <p
                                  className="min-w-0 flex-1 truncate text-xs font-medium"
                                  title={materialTitle}
                                >
                                  {materialTitle}
                                </p>
                              )}
                              <span className="shrink-0 text-[10px] text-muted-foreground/70 transition-colors group-hover:text-muted-foreground">
                                {occurrences.length}
                              </span>
                            </div>
                            <div className="grid grid-rows-[0fr] overflow-hidden opacity-0 transition-[grid-template-rows,opacity] duration-150 group-hover:grid-rows-[1fr] group-hover:opacity-100 group-focus-within:grid-rows-[1fr] group-focus-within:opacity-100">
                              <div className="min-h-0">
                                <div className="pt-1.5">
                                  {occurrences.map((occurrence, index) => {
                                    const label = guidePlaceAriaLabel(
                                      index,
                                      occurrence.path,
                                      placeLabel,
                                    );
                                    return (
                                      <button
                                        className={GUIDE_INSPECTOR_NAV_NESTED_CLASS}
                                        key={`${occurrence.path}-${occurrence.offset}`}
                                        onClick={() =>
                                          openOccurrence(
                                            occurrence.path,
                                            occurrence.offset,
                                          )
                                        }
                                        style={guideNavInspectorIndent()}
                                        title={label}
                                        type="button"
                                      >
                                        <MapPin className="h-3 w-3 shrink-0 opacity-50" />
                                        <span className="min-w-0 flex-1 truncate">
                                          {occurrence.path}
                                        </span>
                                      </button>
                                    );
                                  })}
                                </div>
                              </div>
                            </div>
                          </div>
                        );
                      },
                    )}
                    {materialOccurrences.size === 0 ? (
                      <p className="px-1.5 py-1 text-xs text-muted-foreground">
                        {t("profiles:detail.workflow.guide.noMaterials", {
                          defaultValue: "No material references yet.",
                        })}
                      </p>
                    ) : null}
                  </div>
                </section>
              </div>
            </CardListScrollBody>
          </aside>
        </div>
        <div className="flex min-w-0 min-h-0 flex-col">
          <div className="shrink-0 p-3">
            <BulkSelectionHeader
              className="mb-0"
              leading={
                activeExternalDocument ? (
                  <div className="flex min-w-0 items-center gap-2">
                    <Button
                      className="h-8 shrink-0 px-2"
                      onClick={returnToMainGuide}
                      size="sm"
                      type="button"
                      variant="ghost"
                    >
                      <ChevronLeft className="mr-1 h-4 w-4" />
                      {t("profiles:detail.workflow.guide.backToMainGuide", {
                        defaultValue: "Back",
                      })}
                    </Button>
                    <div className="min-w-0">
                      <div className="truncate text-sm font-semibold text-slate-900 dark:text-slate-100">
                        {activeExternalDocument.title}
                      </div>
                      <div
                        className="truncate text-xs text-slate-500 dark:text-slate-400"
                        title={t("profiles:detail.workflow.guide.description", {
                          defaultValue:
                            "Write the Skill narrative and insert readable references where they are needed.",
                        })}
                      >
                        {t("profiles:detail.workflow.guide.description", {
                          defaultValue:
                            "Write the Skill narrative and insert readable references where they are needed.",
                        })}
                      </div>
                    </div>
                  </div>
                ) : undefined
              }
              title={
                activeExternalDocument
                  ? undefined
                  : t("profiles:detail.workflow.guide.title", {
                    defaultValue: "Workflow Guide",
                  })
              }
              description={
                activeExternalDocument
                  ? undefined
                  : t("profiles:detail.workflow.guide.description", {
                    defaultValue:
                      "Write the Skill narrative and insert readable references where they are needed.",
                  })
              }
              showModeToggle={false}
              trailing={
                <TooltipProvider delayDuration={200}>
                  <div className="inline-flex overflow-hidden rounded-md border border-input">
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          type="button"
                          variant={editorMode === "notebook" ? "default" : "ghost"}
                          size="icon"
                          className="h-8 w-8 rounded-none border-0 shadow-none"
                          aria-label={t("profiles:detail.workflow.guide.notebook", {
                            defaultValue: "Notebook",
                          })}
                          aria-pressed={editorMode === "notebook"}
                          onClick={() => {
                            closeCellEditor();
                            setEditorMode("notebook");
                          }}
                        >
                          <BookOpenText className="h-4 w-4" />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent side="bottom">
                        {t("profiles:detail.workflow.guide.notebook", {
                          defaultValue: "Notebook",
                        })}
                      </TooltipContent>
                    </Tooltip>
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          type="button"
                          variant={editorMode === "preview" ? "default" : "ghost"}
                          size="icon"
                          className="h-8 w-8 rounded-none border-0 shadow-none"
                          aria-label={t("profiles:detail.workflow.guide.preview", {
                            defaultValue: "Preview",
                          })}
                          aria-pressed={editorMode === "preview"}
                          onClick={() => {
                            closeCellEditor();
                            setEditorMode("preview");
                          }}
                        >
                          <Eye className="h-4 w-4" />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent side="bottom">
                        {t("profiles:detail.workflow.guide.previewDescription", {
                          defaultValue:
                            "Rendered from the current draft without saving.",
                        })}
                      </TooltipContent>
                    </Tooltip>
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          type="button"
                          data-guide-outline-toggle
                          variant={outlineOpen ? "default" : "ghost"}
                          size="icon"
                          className="h-8 w-8 rounded-none border-0 shadow-none"
                          aria-label={
                            outlineOpen
                              ? t("profiles:detail.workflow.guide.hideOutline", {
                                defaultValue: "Hide outline",
                              })
                              : t("profiles:detail.workflow.guide.showOutline", {
                                defaultValue: "Show outline",
                              })
                          }
                          aria-pressed={outlineOpen}
                          onClick={() => setOutlineOpen((current) => !current)}
                        >
                          <PanelRight className="h-4 w-4" />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent side="bottom">
                        {outlineOpen
                          ? t("profiles:detail.workflow.guide.hideOutline", {
                            defaultValue: "Hide outline",
                          })
                          : t("profiles:detail.workflow.guide.showOutline", {
                            defaultValue: "Show outline",
                          })}
                      </TooltipContent>
                    </Tooltip>
                  </div>
                </TooltipProvider>
              }
            />
          </div>
          <div className="flex min-h-0 min-w-0 flex-1 flex-col">
            <CardListScrollBody
              className={PROFILE_EDITOR_SIDEBAR_SCROLL_CLASS}
              scrollLocked={outlineOpen}
            >
              {(() => {
                const documentPane = (
                  <div
                    className={cn(
                      "min-w-0",
                      outlineOpen &&
                      "min-h-0 overflow-y-auto overscroll-contain",
                    )}
                  >
                    {editorMode === "preview" ? (
                      <section
                        aria-label={t("profiles:detail.workflow.guide.preview", {
                          defaultValue: "Preview",
                        })}
                        className="p-2"
                      >
                        <div className="px-1.5 py-1">
                          {previewMutation.isPending ? (
                            <p className="text-sm text-muted-foreground">
                              {t("common:loading", { defaultValue: "Loading..." })}
                            </p>
                          ) : null}
                          {previewMutation.data ? (
                            <SkillPreview
                              content={
                                activeExternalDocument
                                  ? previewMutation.data.active_document.markdown
                                  : previewMutation.data.projected_skill.markdown
                              }
                            />
                          ) : null}
                        </div>
                      </section>
                    ) : (
                      <div
                        className="space-y-0 p-2"
                        aria-label={t(
                          "profiles:detail.workflow.guide.notebookRegion",
                          { defaultValue: "Workflow Guide notebook" },
                        )}
                      >
                        {documentCells.map((cell, cellIndex) => {
                          const insertComposerOpen = insertComposerOffset !== null;
                          // Cells inside the session range are already being
                          // edited as raw text, so only the form renders.
                          const hiddenByEditSession =
                            editSession !== null &&
                            cell.startOffset < editSession.end &&
                            cell.endOffset > editSession.start;
                          if (
                            editAnchor?.mode === "replace" &&
                            editAnchor.index === cellIndex
                          ) {
                            return <div key={cell.id}>{markdownEditForm}</div>;
                          }
                          const anchorsEditFormBefore =
                            editAnchor?.mode === "before" &&
                            editAnchor.index === cellIndex;
                          if (hiddenByEditSession) {
                            // Typing can promote part of the session range into
                            // a heading cell; render the form in its slot rather
                            // than beside a duplicate of the live text.
                            return anchorsEditFormBefore ? (
                              <div key={cell.id}>{markdownEditForm}</div>
                            ) : null;
                          }
                          // Cell ids are offset-derived, so a neighbour can
                          // inherit the id of a cell the session just emptied.
                          // Markdown editing renders through `editAnchor`;
                          // only non-markdown cells edit inline by id.
                          const editsInline =
                            editSession === null && editingCellId === cell.id;
                          return (
                            <div key={cell.id}>
                              {anchorsEditFormBefore ? markdownEditForm : null}
                              <article
                                className={cn(
                                  "group relative transition-colors",
                                  insertComposerOpen && "pointer-events-none",
                                  editsInline
                                    ? "-mx-2"
                                    : insertComposerOpen
                                      ? "rounded-md px-1.5 py-1"
                                      : "rounded-md px-1.5 py-1 hover:bg-muted focus-within:bg-muted",
                                )}
                                id={`guide-cell-${cell.id}`}
                              >
                                {insertComposerOpen || editsInline ? null : (
                                  <header className="absolute right-1.5 top-1.5 z-10 flex items-center gap-1 opacity-0 transition-opacity group-hover:opacity-100 group-focus-within:opacity-100">
                                    <span className="text-[10px] leading-none text-muted-foreground">
                                      {cell.kind === "capability"
                                        ? t(
                                          "profiles:detail.workflow.guide.capability",
                                          { defaultValue: "Capability" },
                                        )
                                        : cell.kind === "external_reference"
                                          ? t(
                                            "profiles:detail.workflow.guide.externalReference",
                                            { defaultValue: "External Markdown" },
                                          )
                                          : t(
                                            "profiles:detail.workflow.guide.markdownBlock",
                                            { defaultValue: "Markdown" },
                                          )}
                                    </span>
                                    <Button
                                      aria-label={t(
                                        "profiles:detail.workflow.guide.editBlock",
                                        { defaultValue: "Edit block" },
                                      )}
                                      className={cn("h-6 w-6", GUIDE_ICON_BUTTON_CLASS)}
                                      size="icon"
                                      variant="ghost"
                                      onClick={() => beginCellEdit(cell)}
                                    >
                                      <Pencil className="h-3.5 w-3.5" />
                                    </Button>
                                  </header>
                                )}
                                <div className="min-w-0">
                                  {editsInline &&
                                    cell.kind === "external_reference" &&
                                    cell.externalReference ? (
                                    <GuideExternalReferenceEditForm
                                      cell={cell}
                                      onDelete={() => setPendingCellDelete(cell)}
                                      onDone={closeCellEditor}
                                      onOpenDocument={() => {
                                        const file = packageFiles.find(
                                          (candidate) =>
                                            candidate.relative_path ===
                                            cell.externalReference!.relativePath,
                                        );
                                        if (file) externalDocumentMutation.mutate(file);
                                      }}
                                      onUpdate={updateExternalReference}
                                    />
                                  ) : editsInline &&
                                    cell.kind === "capability" &&
                                    cell.capability ? (
                                    <GuideCapabilityEditForm
                                      capabilities={capabilities}
                                      capabilitiesLoading={capabilitiesLoading}
                                      cell={cell}
                                      editorRef={editorRef}
                                      onDelete={() => setPendingCellDelete(cell)}
                                      onDone={closeCellEditor}
                                      onUpdate={updateCapability}
                                    />
                                  ) : (
                                    <GuideWorkflowCellBrowse
                                      capabilities={capabilities}
                                      cell={cell}
                                      onOpenExternalDocument={(relativePath) => {
                                        const file = packageFiles.find(
                                          (candidate) =>
                                            candidate.relative_path === relativePath,
                                        );
                                        if (file) externalDocumentMutation.mutate(file);
                                      }}
                                    />
                                  )}
                                </div>
                              </article>
                              {canInsertAtWorkflowGuideBoundary(
                                guide.headings,
                                cell.endOffset,
                              ) ? (
                                <GuideBoundaryInsert
                                  capabilities={capabilities}
                                  capabilitiesLoading={capabilitiesLoading}
                                  files={packageFiles}
                                  expanded={insertComposerOffset === cell.endOffset}
                                  hoverEnabled={
                                    !insertComposerOpen && editSession === null
                                  }
                                  onExpandedChange={(nextExpanded) => {
                                    if (nextExpanded) closeCellEditor();
                                    setInsertComposerOffset(
                                      nextExpanded ? cell.endOffset : null,
                                    );
                                  }}
                                  onInsert={insert}
                                  onInsertInPlaceMarkdown={insertInPlaceMarkdownAt}
                                  onInsertCapability={insertCapability}
                                  onCreateExternalDocument={(title) =>
                                    createExternalDocumentMutation
                                      .mutateAsync(title)
                                      .then(() => undefined)
                                  }
                                  creatingExternalDocument={
                                    createExternalDocumentMutation.isPending
                                  }
                                  onCreatePackageFile={(draft) =>
                                    packageFileMutation
                                      .mutateAsync(draft)
                                      .then(() => undefined)
                                  }
                                  creatingPackageFile={packageFileMutation.isPending}
                                  onSetInsertionPoint={(nextOffset) => {
                                    selectionRef.current = {
                                      start: nextOffset,
                                      end: nextOffset,
                                    };
                                  }}
                                  offset={cell.endOffset}
                                />
                              ) : null}
                            </div>
                          );
                        })}
                        {editAnchor?.mode === "append" ? markdownEditForm : null}
                      </div>
                    )}
                    {guide.errors.map((error) => (
                      <p className="p-2 text-sm text-destructive" key={error}>
                        {error}
                      </p>
                    ))}
                  </div>
                );
                if (!outlineOpen) return documentPane;
                return (
                  <ResizableSplitPane
                    trailingFixed
                    className="h-full"
                    dividerAriaLabel={t(
                      "profiles:detail.workflow.guide.resizeOutlinePanel",
                      { defaultValue: "Resize guide outline panel" },
                    )}
                    initialTrailingWidth={224}
                    maxTrailingWidth={400}
                    minTrailingWidth={160}
                  >
                    {documentPane}
                    <nav
                      className="min-h-0 overflow-y-auto overscroll-contain pb-3"
                      data-guide-outline
                      aria-label={t("profiles:detail.workflow.guide.outlineLabel", {
                        defaultValue: "Guide outline",
                      })}
                    >
                      <ol className="list-none text-sm">
                        {outlineSections.map((section) => {
                          const sectionBadge =
                            section.badge === "SKILL.md"
                              ? section.badge
                              : t("profiles:detail.workflow.guide.reference", {
                                defaultValue: "Reference",
                              });
                          const sectionTitle = `${section.title} · ${sectionBadge}`;
                          return (
                            <li key={section.path}>
                              <div>
                                <button
                                  aria-label={
                                    section.path === "SKILL.md"
                                      ? t(
                                        "profiles:detail.workflow.guide.openMainGuide",
                                        { defaultValue: "Open main Guide" },
                                      )
                                      : t(
                                        "profiles:detail.workflow.guide.openDocument",
                                        { defaultValue: "Open document" },
                                      )
                                  }
                                  className={cn(
                                    GUIDE_NAV_DOCUMENT_CLASS,
                                    "text-sm",
                                    activeDocumentPath === section.path && "bg-muted",
                                  )}
                                  onClick={() => openOccurrence(section.path, 0)}
                                  title={sectionTitle}
                                  type="button"
                                >
                                  <span
                                    className="min-w-0 flex-1 truncate"
                                    title={section.title}
                                  >
                                    {section.title}
                                  </span>
                                  <span className="shrink-0 rounded-sm bg-muted px-1 text-[10px] text-muted-foreground">
                                    {sectionBadge}
                                  </span>
                                </button>
                                {section.headings.length > 0 ? (
                                  <ol className="list-none">
                                    {section.headings.map((heading, index) => (
                                      <li key={`${section.path}-${heading.text}-${index}`}>
                                        <button
                                          className={cn(
                                            GUIDE_NAV_NESTED_CLASS,
                                            "text-sm",
                                          )}
                                          onClick={() =>
                                            openOccurrence(section.path, heading.offset)
                                          }
                                          style={guideNavOutlineIndent(
                                            outlineHeadingDepth(
                                              section.headings,
                                              heading,
                                            ),
                                          )}
                                          title={`${heading.text} · H${heading.level}`}
                                          type="button"
                                        >
                                          <span
                                            aria-hidden="true"
                                            className={cn(
                                              "h-1.5 w-1.5 shrink-0 rounded-full",
                                              heading.level === 1
                                                ? "bg-foreground/70"
                                                : "bg-muted-foreground/50",
                                            )}
                                          />
                                          <span
                                            className="min-w-0 flex-1 truncate"
                                            title={heading.text}
                                          >
                                            {heading.text}
                                          </span>
                                          <span className="shrink-0 font-mono text-[10px] text-muted-foreground">
                                            H{heading.level}
                                          </span>
                                        </button>
                                      </li>
                                    ))}
                                  </ol>
                                ) : null}
                              </div>
                            </li>
                          );
                        })}
                      </ol>
                    </nav>
                  </ResizableSplitPane>
                );
              })()}
            </CardListScrollBody>
          </div>
          <footer className="mx-3 mb-3 flex shrink-0 items-center justify-between">
            <Button
              className={GUIDE_ACTION_BUTTON_CLASS}
              size="sm"
              variant="ghost"
              onClick={() => repairMutation.mutate()}
              disabled={repairMutation.isPending}
            >
              <Wrench className="mr-1 h-3.5 w-3.5" />
              {t("profiles:detail.workflow.guide.repair", {
                defaultValue: "Repair",
              })}
            </Button>
            <Button
              className={GUIDE_ACTION_BUTTON_CLASS}
              size="sm"
              onClick={() => void saveAllDirty()}
              disabled={
                !hasDirtyChanges ||
                saveMutation.isPending ||
                saveExternalDocumentMutation.isPending ||
                hasBlockingGuideErrors
              }
            >
              <Save className="mr-1 h-3.5 w-3.5" />
              {t("common:save", { defaultValue: "Save" })}
            </Button>
          </footer>
        </div>
      </ResizableSplitPane>
      <AlertDialog
        open={pendingReclamation !== null}
        onOpenChange={(open) => {
          if (!open) setPendingReclamation(null);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>
              {t("profiles:detail.workflow.guide.reclamationTitle", {
                defaultValue: "Confirm removed references",
              })}
            </AlertDialogTitle>
            <AlertDialogDescription>
              {t("profiles:detail.workflow.guide.reclamationDescription", {
                defaultValue:
                  "Saving will remove Profile bindings and move package files that are no longer reachable to Trash.",
              })}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <div className="flex max-h-72 flex-col gap-4 overflow-auto text-sm">
            {pendingReclamation?.capabilities.length ? (
              <section className="flex flex-col gap-2">
                <h4 className="font-medium">
                  {t("profiles:detail.workflow.guide.capabilities", {
                    defaultValue: "Capabilities",
                  })}
                </h4>
                <ul className="flex flex-col gap-1 text-muted-foreground">
                  {pendingReclamation.capabilities.map((capability) => (
                    <li key={capability.name}>{capability.name}</li>
                  ))}
                </ul>
              </section>
            ) : null}
            {pendingReclamation?.packageFiles.length ? (
              <section className="flex flex-col gap-2">
                <h4 className="font-medium">
                  {t("profiles:detail.workflow.guide.materials", {
                    defaultValue: "Materials",
                  })}
                </h4>
                <ul className="flex flex-col gap-1 text-muted-foreground">
                  {pendingReclamation.packageFiles.map((file) => (
                    <li key={file.package_file_id}>
                      {file.title} ({file.relative_path})
                    </li>
                  ))}
                </ul>
              </section>
            ) : null}
          </div>
          <AlertDialogFooter>
            <AlertDialogCancel>
              {t("common:cancel", { defaultValue: "Cancel" })}
            </AlertDialogCancel>
            <AlertDialogAction onClick={confirmReclamation}>
              {t("profiles:detail.workflow.guide.confirmSave", {
                defaultValue: "Confirm and save",
              })}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
      <AlertDialog
        open={pendingCellDelete !== null}
        onOpenChange={(open) => {
          if (!open) setPendingCellDelete(null);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>
              {t("profiles:detail.workflow.guide.deleteCellTitle", {
                defaultValue: "Delete this block?",
              })}
            </AlertDialogTitle>
            <AlertDialogDescription>
              {t("profiles:detail.workflow.guide.deleteCellDescription", {
                defaultValue:
                  "This removes the selected block from the Guide. Save the Guide to apply the change to the projected Skill.",
              })}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>
              {t("common:cancel", { defaultValue: "Cancel" })}
            </AlertDialogCancel>
            <AlertDialogAction
              className="bg-destructive text-destructive-foreground hover:bg-destructive/90"
              onClick={() => {
                if (pendingCellDelete) deleteCell(pendingCellDelete);
                setPendingCellDelete(null);
              }}
            >
              {t("profiles:detail.workflow.guide.deleteCell", {
                defaultValue: "Delete block",
              })}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </section>
  );
}

const GUIDE_MARKDOWN_EDITOR_MIN_ROWS = 5;

function guideMarkdownEditorMinHeight(editor: HTMLTextAreaElement) {
  const styles = window.getComputedStyle(editor);
  const fontSize = Number.parseFloat(styles.fontSize) || 14;
  const lineHeight =
    styles.lineHeight === "normal"
      ? fontSize * 1.5
      : Number.parseFloat(styles.lineHeight) || fontSize * 1.5;
  const padding =
    Number.parseFloat(styles.paddingTop) + Number.parseFloat(styles.paddingBottom);
  return lineHeight * GUIDE_MARKDOWN_EDITOR_MIN_ROWS + padding;
}

function fitGuideMarkdownEditor(editor: HTMLTextAreaElement) {
  const minHeight = guideMarkdownEditorMinHeight(editor);
  editor.style.height = "0px";
  editor.style.overflowY = "hidden";
  const contentHeight = Math.max(editor.scrollHeight, minHeight);
  const scroller = editor.closest("[data-card-list-scroll]");
  if (!(scroller instanceof HTMLElement)) {
    editor.style.height = `${contentHeight}px`;
    return;
  }
  const editorTop = editor.getBoundingClientRect().top;
  const scrollerRect = scroller.getBoundingClientRect();
  const visibleTop = Math.max(editorTop, scrollerRect.top);
  const available = Math.max(minHeight, scrollerRect.bottom - visibleTop);
  const nextHeight = Math.min(contentHeight, available);
  editor.style.height = `${nextHeight}px`;
  editor.style.overflowY = contentHeight > nextHeight ? "auto" : "hidden";
}

function GuideMarkdownSourceEditor({
  ariaLabel,
  editorRef,
  value,
  onChange,
  onSelect,
}: {
  ariaLabel: string;
  editorRef: RefObject<HTMLTextAreaElement | null>;
  value: string;
  onChange: (event: SyntheticEvent<HTMLTextAreaElement>) => void;
  onSelect: (event: SyntheticEvent<HTMLTextAreaElement>) => void;
}) {
  useLayoutEffect(() => {
    const editor = editorRef.current;
    if (!editor) return;
    const fit = () => fitGuideMarkdownEditor(editor);
    fit();
    const scroller = editor.closest("[data-card-list-scroll]");
    const observer = new ResizeObserver(fit);
    if (scroller instanceof HTMLElement) {
      observer.observe(scroller);
      scroller.addEventListener("scroll", fit, { passive: true });
    }
    window.addEventListener("resize", fit);
    return () => {
      observer.disconnect();
      if (scroller instanceof HTMLElement) {
        scroller.removeEventListener("scroll", fit);
      }
      window.removeEventListener("resize", fit);
    };
  }, [editorRef, value]);
  return (
    <Textarea
      autoFocus
      ref={(node) => {
        editorRef.current = node;
        if (node) fitGuideMarkdownEditor(node);
      }}
      aria-label={ariaLabel}
      className="min-h-[5lh] w-full resize-none overflow-hidden overscroll-contain rounded-none border-0 bg-transparent px-0 py-0 font-mono text-sm leading-5 shadow-none focus-visible:ring-0 focus-visible:ring-offset-0"
      rows={5}
      value={value}
      onChange={onChange}
      onSelect={onSelect}
    />
  );
}

function GuideCellSaveButton({
  onDone,
  embedded = false,
  className,
}: {
  onDone: () => void;
  embedded?: boolean;
  className?: string;
}) {
  const { t } = useTranslation();
  return (
    <Button
      aria-label={t("profiles:detail.workflow.guide.doneEdit", {
        defaultValue: "Done",
      })}
      className={cn(
        GUIDE_SAVE_BUTTON_CLASS,
        !embedded && "absolute right-1.5 top-1.5 z-10",
        className,
      )}
      size="icon"
      type="button"
      variant="ghost"
      onClick={onDone}
    >
      <Check className="h-3.5 w-3.5" />
    </Button>
  );
}

function GuideCellEditorActions({
  onSave,
  onDelete,
}: {
  onSave: () => void;
  onDelete?: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex items-center gap-1">
      {onDelete ? (
        <Button
          aria-label={t("profiles:detail.workflow.guide.deleteCell", {
            defaultValue: "Delete block",
          })}
          className={cn(
            GUIDE_SAVE_BUTTON_CLASS,
            "text-destructive hover:bg-destructive/10 hover:text-destructive",
          )}
          onClick={onDelete}
          size="icon"
          type="button"
          variant="ghost"
        >
          <Trash2 className="h-3.5 w-3.5" />
        </Button>
      ) : null}
      <GuideCellSaveButton embedded onDone={onSave} />
    </div>
  );
}

function GuideMarkdownEditForm({
  editorRef,
  initialSource,
  onCancel,
  onDelete,
  onDone,
  onSelect,
}: {
  editorRef: RefObject<HTMLTextAreaElement | null>;
  initialSource: string;
  onCancel: () => void;
  onDelete?: () => void;
  onDone: (value: string) => void;
  onSelect: (event: SyntheticEvent<HTMLTextAreaElement>) => void;
}) {
  const { t } = useTranslation();
  const [source, setSource] = useState(() =>
    formatMarkdownCellSourceForEditor(initialSource),
  );
  const title =
    source.match(/^#{1,6}\s+(.+?)\s*$/m)?.[1] ??
    t("profiles:detail.workflow.guide.markdownBlock", {
      defaultValue: "Markdown",
    });
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      onCancel();
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onCancel]);
  return (
    <div className={COMPOSER_SHELL_CLASS}>
      <GuideComposerHeader
        actions={
          <GuideCellEditorActions
            onDelete={onDelete}
            onSave={() => onDone(source)}
          />
        }
        closeLabel={t("profiles:detail.workflow.guide.cancelEdit", {
          defaultValue: "Cancel",
        })}
        onClose={onCancel}
        title={title}
      />
      <div className="px-3 py-3">
        <GuideMarkdownSourceEditor
          ariaLabel={t("profiles:detail.workflow.guide.markdownSource", {
            defaultValue: "Markdown block source",
          })}
          editorRef={editorRef}
          value={source}
          onChange={(event) => setSource(event.currentTarget.value)}
          onSelect={onSelect}
        />
      </div>
    </div>
  );
}

function GuideExternalReferenceEditForm({
  cell,
  onDelete,
  onDone,
  onOpenDocument,
  onUpdate,
}: {
  cell: WorkflowGuideDocumentCell;
  onDelete?: () => void;
  onDone: () => void;
  onOpenDocument: () => void;
  onUpdate: (
    cell: WorkflowGuideDocumentCell,
    title: string,
    guide: string,
  ) => void;
}) {
  const { t } = useTranslation();
  const reference = cell.externalReference;
  const [title, setTitle] = useState(reference?.title ?? "");
  const [guide, setGuide] = useState(reference?.guide ?? "");
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      onDone();
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onDone]);
  if (!reference) return null;
  return (
    <div className={COMPOSER_SHELL_CLASS}>
      <GuideComposerHeader
        actions={
          <GuideCellEditorActions
            onDelete={onDelete}
            onSave={() => {
              if (!title.trim()) return;
              onUpdate(cell, title.trim(), guide);
              onDone();
            }}
          />
        }
        closeLabel={t("profiles:detail.workflow.guide.cancelEdit", {
          defaultValue: "Cancel",
        })}
        onClose={onDone}
        onTitleClick={onOpenDocument}
        title={t("profiles:detail.workflow.guide.openDocumentHint", {
          defaultValue: "Click to open document",
        })}
      />
      <div className="flex flex-col gap-2 px-3 py-3">
        <Input
          aria-label={t("profiles:detail.workflow.guide.sectionName", {
            defaultValue: "Section name",
          })}
          className="h-9 bg-background text-xs"
          onChange={(event) => setTitle(event.target.value)}
          placeholder={t("profiles:detail.workflow.guide.sectionName", {
            defaultValue: "Section name",
          })}
          value={title}
        />
        <Textarea
          aria-label={t("profiles:detail.workflow.guide.externalGuide", {
            defaultValue: "Context in main Guide",
          })}
          className="min-h-[5lh] resize-y bg-background text-xs"
          onChange={(event) => setGuide(event.target.value)}
          placeholder={t(
            "profiles:detail.workflow.guide.externalGuidePlaceholder",
            {
              defaultValue:
                "Explain when readers should open this external document.",
            },
          )}
          value={guide}
        />
      </div>
    </div>
  );
}

function GuideCapabilityEditForm({
  cell,
  capabilities,
  capabilitiesLoading,
  editorRef,
  onDelete,
  onDone,
  onUpdate,
}: {
  cell: WorkflowGuideDocumentCell;
  capabilities: WorkflowCapabilityOption[];
  capabilitiesLoading: boolean;
  editorRef: RefObject<HTMLTextAreaElement | null>;
  onDelete?: () => void;
  onDone: () => void;
  onUpdate: (
    cell: WorkflowGuideDocumentCell,
    name: string,
    exposure: "direct" | "meta_on_demand",
    guide: string,
  ) => void;
}) {
  const { t } = useTranslation();
  const capability = cell.capability;
  const [name, setName] = useState(capability?.name ?? "");
  const [exposure, setExposure] = useState<"direct" | "meta_on_demand">(
    capability?.exposure ?? "meta_on_demand",
  );
  const [guide, setGuide] = useState(capability?.guide ?? "");
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      onDone();
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onDone]);
  if (!capability) return null;
  return (
    <div className={COMPOSER_SHELL_CLASS}>
      <GuideComposerHeader
        actions={
          <GuideCellEditorActions
            onDelete={onDelete}
            onSave={() => {
              onUpdate(cell, name, exposure, guide);
              onDone();
            }}
          />
        }
        closeLabel={t("profiles:detail.workflow.guide.cancelEdit", {
          defaultValue: "Cancel",
        })}
        onClose={onDone}
        title={name}
      />
      <div className="flex flex-col gap-2 px-3 py-3">
        <GuideCapabilityFields
          autoFocus
          capabilities={capabilities}
          capabilitiesLoading={capabilitiesLoading}
          editorRef={editorRef}
          exposure={exposure}
          guide={guide}
          name={name}
          onExposureChange={setExposure}
          onGuideChange={setGuide}
          onNameChange={setName}
        />
      </div>
    </div>
  );
}

function collectOccurrences(
  documents: Array<{ path: string; title: string; markdown: string }>,
  expression: RegExp,
) {
  const occurrences = new Map<
    string,
    Array<{ path: string; offset: number }>
  >();
  for (const document of documents) {
    expression.lastIndex = 0;
    for (const match of document.markdown.matchAll(expression)) {
      const key = match[1];
      const offset = match.index ?? 0;
      const items = occurrences.get(key) ?? [];
      items.push({ path: document.path, offset });
      occurrences.set(key, items);
    }
  }
  return occurrences;
}

function collectMaterialOccurrences(
  documents: Array<{ path: string; title: string; markdown: string }>,
  parsedGuides: Map<string, WorkflowGuideParseResult>,
) {
  const occurrences = collectOccurrences(
    documents,
    /\[[^\]\n]+\]\(((?:references|scripts|assets)\/[^\s)#]+)(?:#[^\s)]+)?\)/g,
  );
  const siblingReference = /\[[^\]\n]+\]\(((?:\.\/)?[^/\s)#]+\.md)(?:#[^\s)]+)?\)/g;
  for (const document of documents) {
    const parsed = parsedGuides.get(document.path);
    if (!parsed) continue;
    const lineOffsets = buildLineOffsets(document.markdown);
    for (const external of parsed.externals) {
      const offset = lineOffsets[external.startLine - 1] ?? 0;
      const items = occurrences.get(external.path) ?? [];
      items.push({ path: document.path, offset });
      occurrences.set(external.path, items);
    }
    if (document.path === "SKILL.md") continue;
    siblingReference.lastIndex = 0;
    const parent = document.path.slice(0, document.path.lastIndexOf("/"));
    for (const match of document.markdown.matchAll(siblingReference)) {
      const fileName = match[1].replace(/^\.\//, "");
      const key = `${parent}/${fileName}`;
      const items = occurrences.get(key) ?? [];
      items.push({ path: document.path, offset: match.index ?? 0 });
      occurrences.set(key, items);
    }
  }
  return occurrences;
}

function collectCapabilityOccurrences(
  documents: Array<{ path: string; title: string; markdown: string }>,
  parsedGuides: Map<string, WorkflowGuideParseResult>,
) {
  const occurrences = new Map<
    string,
    Array<{
      path: string;
      offset: number;
      exposure: "direct" | "meta_on_demand";
      guide: string;
    }>
  >();
  for (const document of documents) {
    const parsed = parsedGuides.get(document.path);
    if (!parsed) continue;
    const lineOffsets = buildLineOffsets(document.markdown);
    for (const capability of parsed.capabilities) {
      const occurrence = {
        path: document.path,
        offset: lineOffsets[capability.startLine - 1] ?? 0,
        exposure: capability.exposure,
        guide: capability.guide,
      };
      const items = occurrences.get(capability.name) ?? [];
      items.push(occurrence);
      occurrences.set(capability.name, items);
    }
  }
  return occurrences;
}

function GuideWorkflowCellBrowse({
  cell,
  capabilities,
  onOpenExternalDocument,
}: {
  cell: WorkflowGuideDocumentCell;
  capabilities: WorkflowCapabilityOption[];
  onOpenExternalDocument: (relativePath: string) => void;
}) {
  const { t } = useTranslation();

  if (cell.kind === "capability" && cell.capability) {
    return (
      <GuideCapabilityBlock
        capabilities={capabilities}
        capability={cell.capability}
      />
    );
  }

  if (cell.kind === "external_reference" && cell.externalReference) {
    return (
      <div className="space-y-1">
        <button
          className="text-left text-sm font-medium text-primary hover:underline"
          onClick={() => onOpenExternalDocument(cell.externalReference!.relativePath)}
          type="button"
        >
          {t("profiles:detail.workflow.guide.openDocumentHint", {
            defaultValue: "Click to open document",
          })}
        </button>
        {cell.externalReference.guide ? (
          <GuideMarkdownPreview content={cell.externalReference.guide} />
        ) : (
          <p className="text-sm italic text-muted-foreground">
            {t("profiles:detail.workflow.guide.externalGuidePlaceholder", {
              defaultValue:
                "Explain when readers should open this external document.",
            })}
          </p>
        )}
      </div>
    );
  }

  return (
    <GuideMarkdownPreview
      content={cell.source}
      emptyLabel={t("profiles:detail.workflow.guide.emptyMarkdownBlock", {
        defaultValue: "Empty Markdown block",
      })}
    />
  );
}

function GuideMarkdownPreview({
  content,
  emptyLabel,
}: {
  content: string;
  emptyLabel?: string;
}) {
  if (!content.trim()) {
    return (
      <p className="min-h-7 text-sm italic text-muted-foreground">
        {emptyLabel ?? "Empty"}
      </p>
    );
  }
  return (
    <div className="min-w-0 text-sm leading-6 text-slate-700 dark:text-slate-300">
      <ReactMarkdown
        rehypePlugins={[rehypeSanitize]}
        remarkPlugins={[remarkGfm]}
        components={{
          h1: ({ children }) => (
            <h1 className="mb-2 text-xl font-semibold last:mb-0">{children}</h1>
          ),
          h2: ({ children }) => (
            <h2 className="mb-2 text-lg font-semibold last:mb-0">{children}</h2>
          ),
          h3: ({ children }) => (
            <h3 className="mb-2 text-base font-semibold last:mb-0">{children}</h3>
          ),
          p: ({ children }) => <p className="mb-2 last:mb-0">{children}</p>,
          ol: ({ children }) => (
            <ol className="mb-2 list-decimal space-y-1 pl-5 last:mb-0">
              {children}
            </ol>
          ),
          ul: ({ children }) => (
            <ul className="mb-2 list-disc space-y-1 pl-5 last:mb-0">
              {children}
            </ul>
          ),
          li: ({ children }) => <li className="leading-6">{children}</li>,
          code: ({ children }) => (
            <code className="rounded bg-muted px-1 font-mono text-xs">
              {children}
            </code>
          ),
          pre: ({ children }) => (
            <pre className="overflow-x-auto rounded bg-muted p-3 font-mono text-xs">
              {children}
            </pre>
          ),
        }}
      >
        {content}
      </ReactMarkdown>
    </div>
  );
}

function SkillPreview({ content }: { content: string }) {
  return (
    <GuideMarkdownPreview content={stripLeadingSkillFrontMatter(content).body} />
  );
}
