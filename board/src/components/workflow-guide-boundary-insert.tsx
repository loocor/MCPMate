import {
  ChevronLeft,
  Database,
  FileCode,
  FilePlus2,
  FileText,
  Files,
  LayoutTemplate,
  MessageSquare,
  Paperclip,
  Plus,
  Save,
  Wrench,
  X,
} from "lucide-react";
import {
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
  type RefObject,
} from "react";
import { useTranslation } from "react-i18next";

import type {
  WorkflowCapabilityKind,
  WorkflowCapabilityOption,
} from "../lib/profile-workflow-specification";
import { resolveWorkflowCapabilityKind } from "../lib/profile-workflow-specification";
import type {
  WorkflowGuidePackageCategory,
  WorkflowGuidePackageFile,
} from "../lib/types";
import { cn } from "../lib/utils";
import { CapabilityCombobox } from "./capability-combobox";
import { Button } from "./ui/button";
import { Input } from "./ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "./ui/select";
import { Textarea } from "./ui/textarea";

type InsertKind =
  | "external_markdown"
  | "reference"
  | "capability"
  | "script"
  | "asset";

export const COMPOSER_SHELL_CLASS =
  "overflow-hidden border-y border-slate-200/90 bg-muted/60 dark:border-slate-700/80 dark:bg-muted/40 " +
  "shadow-[inset_0_5px_7px_-4px_rgba(15,23,42,0.14),inset_0_-3px_5px_-4px_rgba(15,23,42,0.06)] " +
  "dark:shadow-[inset_0_6px_8px_-4px_rgba(0,0,0,0.28),inset_0_-4px_6px_-4px_rgba(0,0,0,0.18)]";

export const GUIDE_ICON_BUTTON_CLASS =
  "bg-transparent text-foreground shadow-none hover:bg-transparent hover:text-foreground " +
  "[&_svg]:opacity-25 [&_svg]:transition-opacity hover:[&_svg]:opacity-100 focus-visible:[&_svg]:opacity-100";

export const GUIDE_SAVE_BUTTON_CLASS =
  "h-7 w-7 bg-transparent text-foreground shadow-none hover:bg-transparent";

export const GUIDE_ACTION_BUTTON_CLASS = "h-7 px-2.5 text-xs";

const GUIDE_FIELD_CLASS = "h-9 bg-background text-xs";
const GUIDE_COMBO_TRIGGER_CLASS = "h-9 px-3 text-xs font-normal";

function fitGuideAutosizeTextarea(editor: HTMLTextAreaElement) {
  const scroller = editor.closest("[data-card-list-scroll]");
  editor.style.height = "auto";
  editor.style.overflowY = "hidden";
  const contentHeight = editor.scrollHeight;
  if (!(scroller instanceof HTMLElement)) {
    editor.style.height = `${contentHeight}px`;
    return;
  }
  const editorTop = editor.getBoundingClientRect().top;
  const visibleTop = Math.max(editorTop, scroller.getBoundingClientRect().top);
  const maxHeight = Math.max(0, scroller.getBoundingClientRect().bottom - visibleTop);
  const nextHeight = Math.min(contentHeight, maxHeight);
  editor.style.height = `${nextHeight}px`;
  editor.style.overflowY = contentHeight > maxHeight ? "auto" : "hidden";
}

export function GuideBoundaryInsert({
  capabilities,
  capabilitiesLoading,
  files,
  offset,
  expanded,
  hoverEnabled = true,
  onExpandedChange,
  onInsert,
  onInsertCapability,
  onInsertInPlaceMarkdown,
  onCreateExternalDocument,
  creatingExternalDocument,
  onCreatePackageFile,
  creatingPackageFile,
  onSetInsertionPoint,
}: {
  capabilities: WorkflowCapabilityOption[];
  capabilitiesLoading: boolean;
  files: WorkflowGuidePackageFile[];
  offset: number;
  expanded: boolean;
  hoverEnabled?: boolean;
  onExpandedChange: (expanded: boolean) => void;
  onInsert: (value: string) => void;
  onInsertInPlaceMarkdown: (offset: number) => void;
  onInsertCapability: (
    capability: WorkflowCapabilityOption,
    exposure: "direct" | "meta_on_demand",
    guide: string,
  ) => void;
  onCreateExternalDocument: (title: string) => Promise<void>;
  creatingExternalDocument: boolean;
  onCreatePackageFile: (draft: {
    title: string;
    category: WorkflowGuidePackageCategory;
    file: File;
  }) => Promise<void>;
  creatingPackageFile: boolean;
  onSetInsertionPoint: (offset: number) => void;
}) {
  const { t } = useTranslation(["profiles", "common"]);
  const panelRef = useRef<HTMLDivElement>(null);
  const composerId = useId();
  const [activeInsert, setActiveInsert] = useState<InsertKind | null>(null);
  const [externalDocumentTitle, setExternalDocumentTitle] = useState("");
  const [packageTitle, setPackageTitle] = useState("");
  const [packageUpload, setPackageUpload] = useState<File | null>(null);
  const [selectedCapability, setSelectedCapability] =
    useState<WorkflowCapabilityOption | null>(null);
  const [bindingPolicy, setBindingPolicy] = useState<
    "direct" | "meta_on_demand"
  >("meta_on_demand");
  const [capabilityGuide, setCapabilityGuide] = useState("");
  const packageCategory = packageCategoryForInsert(activeInsert);
  const visibleFiles = files.filter(
    (file) =>
      file.category === packageCategory &&
      !(packageCategory === "reference" && file.extension === "md"),
  );

  const resetInsert = () => {
    setActiveInsert(null);
    setExternalDocumentTitle("");
    setPackageTitle("");
    setPackageUpload(null);
    setSelectedCapability(null);
    setBindingPolicy("meta_on_demand");
    setCapabilityGuide("");
  };

  const closeInsert = () => {
    resetInsert();
    onExpandedChange(false);
  };

  const insertAtBoundary = (value: string) => {
    onSetInsertionPoint(offset);
    onInsert(value);
    closeInsert();
  };

  useEffect(() => {
    if (expanded) return;
    resetInsert();
  }, [expanded]);

  useEffect(() => {
    if (!expanded) return;
    panelRef.current?.focus();
  }, [expanded]);

  useEffect(() => {
    if (!expanded) return;
    panelRef.current?.scrollIntoView({ block: "nearest", behavior: "smooth" });
  }, [activeInsert, expanded]);

  useEffect(() => {
    if (!expanded) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      onExpandedChange(false);
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [expanded, onExpandedChange]);

  if (!expanded) {
    return (
      <div
        className={cn(
          "relative z-10 h-0",
          hoverEnabled && "group/boundary",
          !hoverEnabled && "pointer-events-none",
        )}
      >
        <div className="absolute inset-x-0 top-1/2 flex h-4 -translate-y-1/2 items-center justify-center">
          <div className="h-px flex-1 bg-transparent transition-colors group-hover/boundary:bg-border group-focus-within/boundary:bg-border" />
          <Button
            aria-expanded={false}
            aria-label={t("profiles:detail.workflow.guide.insertAtPosition", {
              defaultValue: "Insert at this position",
            })}
            className={cn(
              "mx-1 h-5 w-5 rounded-full border bg-background p-0 shadow-none",
              hoverEnabled
                ? "opacity-0 transition-opacity group-hover/boundary:opacity-100 group-focus-within/boundary:opacity-100"
                : "invisible",
            )}
            onClick={() => {
              onSetInsertionPoint(offset);
              onExpandedChange(true);
            }}
            size="icon"
            type="button"
            variant="ghost"
          >
            <Plus className="h-3 w-3" />
          </Button>
          <div className="h-px flex-1 bg-transparent transition-colors group-hover/boundary:bg-border group-focus-within/boundary:bg-border" />
        </div>
      </div>
    );
  }

  return (
    <div className="-mx-2" ref={panelRef} tabIndex={-1}>
      {activeInsert === null ? (
        <div className={COMPOSER_SHELL_CLASS}>
          <GuideComposerHeader
            onClose={closeInsert}
            title={t("profiles:detail.workflow.guide.insert", {
              defaultValue: "Insert",
            })}
          />
          <div className="grid grid-cols-2 gap-1 px-3 py-2 sm:grid-cols-3">
            {insertTypeOptions.map((option) => {
              const Icon = option.icon;
              return (
                <Button
                  className={cn(
                    "h-8 justify-start px-2 text-xs",
                    GUIDE_ICON_BUTTON_CLASS,
                  )}
                  key={option.id}
                  onClick={() => {
                    onSetInsertionPoint(offset);
                    if (option.id === "markdown") {
                      onInsertInPlaceMarkdown(offset);
                      closeInsert();
                      return;
                    }
                    setActiveInsert(option.id);
                  }}
                  size="sm"
                  type="button"
                  variant="ghost"
                >
                  <Icon className="mr-1.5 h-3.5 w-3.5 shrink-0" />
                  <span className="truncate">
                    {t(option.labelKey, { defaultValue: option.defaultValue })}
                  </span>
                </Button>
              );
            })}
          </div>
        </div>
      ) : (
        <InsertComposerShell
          actions={
            activeInsert === "capability" ? (
              <Button
                aria-label={t(
                  "profiles:detail.workflow.guide.insertCapability",
                  { defaultValue: "Insert capability" },
                )}
                className={GUIDE_SAVE_BUTTON_CLASS}
                disabled={!selectedCapability}
                onClick={() => {
                  if (!selectedCapability) return;
                  onSetInsertionPoint(offset);
                  onInsertCapability(
                    selectedCapability,
                    bindingPolicy,
                    capabilityGuide,
                  );
                  closeInsert();
                }}
                size="icon"
                type="button"
                variant="ghost"
              >
                <Save className="h-3.5 w-3.5" />
              </Button>
            ) : activeInsert === "external_markdown" ? (
              <Button
                aria-label={t(
                  "profiles:detail.workflow.guide.createExternalMarkdown",
                  { defaultValue: "Create external Markdown" },
                )}
                className={GUIDE_SAVE_BUTTON_CLASS}
                disabled={
                  !externalDocumentTitle.trim() || creatingExternalDocument
                }
                onClick={() => {
                  onSetInsertionPoint(offset);
                  void onCreateExternalDocument(externalDocumentTitle).then(
                    closeInsert,
                    () => undefined,
                  );
                }}
                size="icon"
                type="button"
                variant="ghost"
              >
                <Save className="h-3.5 w-3.5" />
              </Button>
            ) : (
              <Button
                aria-label={t(
                  "profiles:detail.workflow.guide.uploadAndInsert",
                  { defaultValue: "Upload and insert" },
                )}
                className={GUIDE_SAVE_BUTTON_CLASS}
                disabled={!packageUpload || creatingPackageFile}
                onClick={() => {
                  if (!packageUpload) return;
                  onSetInsertionPoint(offset);
                  void onCreatePackageFile({
                    title: packageTitle,
                    category: packageCategory,
                    file: packageUpload,
                  }).then(closeInsert, () => undefined);
                }}
                size="icon"
                type="button"
                variant="ghost"
              >
                <Save className="h-3.5 w-3.5" />
              </Button>
            )
          }
          onBack={resetInsert}
          onClose={closeInsert}
          title={
            activeInsert === "capability" && selectedCapability
              ? selectedCapability.label
              : insertKindTitle(activeInsert, t)
          }
        >
          {activeInsert === "capability" ? (
            <CapabilityInsertFields
              bindingPolicy={bindingPolicy}
              capabilities={capabilities}
              capabilitiesLoading={capabilitiesLoading}
              capabilityGuide={capabilityGuide}
              onCapabilityGuideChange={setCapabilityGuide}
              onSelectCapability={setSelectedCapability}
              onBindingPolicyChange={setBindingPolicy}
              selectedCapability={selectedCapability}
            />
          ) : null}
          {activeInsert === "external_markdown" ? (
            <ExternalMarkdownInsertFields
              composerId={composerId}
              offset={offset}
              onTitleChange={setExternalDocumentTitle}
              title={externalDocumentTitle}
            />
          ) : null}
          {activeInsert === "reference" ||
            activeInsert === "script" ||
            activeInsert === "asset" ? (
            <PackageInsertFields
              composerId={composerId}
              files={visibleFiles}
              onInsertExisting={(file) =>
                insertAtBoundary(`[${file.title}](${file.relative_path})`)
              }
              onPackageTitleChange={setPackageTitle}
              onPackageUploadChange={setPackageUpload}
              packageCategory={packageCategory}
              packageFile={packageUpload}
              packageTitle={packageTitle}
            />
          ) : null}
        </InsertComposerShell>
      )}
    </div>
  );
}

function InsertComposerShell({
  title,
  onBack,
  onClose,
  children,
  actions,
}: {
  title: string;
  onBack: () => void;
  onClose: () => void;
  children: ReactNode;
  actions?: ReactNode;
}) {
  const { t } = useTranslation(["profiles"]);
  return (
    <div className={COMPOSER_SHELL_CLASS}>
      <GuideComposerHeader
        actions={actions}
        onBack={onBack}
        onClose={onClose}
        title={title}
        backLabel={t("profiles:detail.workflow.guide.backToInsertTypes", {
          defaultValue: "Back to insert types",
        })}
      />
      <div className="flex flex-col gap-2 px-3 py-3">{children}</div>
    </div>
  );
}

export function GuideComposerHeader({
  title,
  onBack,
  onClose,
  onTitleClick,
  backLabel,
  closeLabel,
  actions,
  showClose = true,
}: {
  title: string;
  onBack?: () => void;
  onClose: () => void;
  onTitleClick?: () => void;
  backLabel?: string;
  closeLabel?: string;
  actions?: ReactNode;
  showClose?: boolean;
}) {
  const { t } = useTranslation(["profiles", "common"]);
  return (
    <header className="flex h-9 items-center gap-1 border-b border-border/50 px-3">
      {onBack ? (
        <Button
          aria-label={backLabel}
          className={cn("h-7 shrink-0 px-0", GUIDE_ICON_BUTTON_CLASS)}
          onClick={onBack}
          size="sm"
          type="button"
          variant="ghost"
        >
          <ChevronLeft className="mr-1 h-3.5 w-3.5" />
          {t("profiles:form.buttons.back", { defaultValue: "Back" })}
        </Button>
      ) : (
        <span className="w-0 shrink-0" />
      )}
      {onTitleClick ? (
        <button
          className="min-w-0 flex-1 truncate text-left text-xs font-medium text-primary hover:underline"
          onClick={onTitleClick}
          type="button"
        >
          {title}
        </button>
      ) : (
        <p className="min-w-0 flex-1 truncate text-xs font-medium">{title}</p>
      )}
      {actions}
      {showClose ? (
        <Button
          aria-label={
            closeLabel ??
            t("profiles:detail.workflow.guide.closeInsert", {
              defaultValue: "Close insert",
            })
          }
          className={cn("h-7 w-7 shrink-0", GUIDE_ICON_BUTTON_CLASS)}
          onClick={onClose}
          size="icon"
          type="button"
          variant="ghost"
        >
          <X className="h-3.5 w-3.5" />
        </Button>
      ) : null}
    </header>
  );
}

function GuideAutosizeTextarea({
  ariaLabel,
  autoFocus = false,
  editorRef,
  placeholder,
  value,
  onChange,
}: {
  ariaLabel: string;
  autoFocus?: boolean;
  editorRef?: RefObject<HTMLTextAreaElement | null>;
  placeholder: string;
  value: string;
  onChange: (value: string) => void;
}) {
  const localRef = useRef<HTMLTextAreaElement | null>(null);
  const assignRef = (node: HTMLTextAreaElement | null) => {
    localRef.current = node;
    if (editorRef) editorRef.current = node;
    if (node) fitGuideAutosizeTextarea(node);
  };
  useLayoutEffect(() => {
    const editor = localRef.current;
    if (!editor) return;
    const fit = () => fitGuideAutosizeTextarea(editor);
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
  }, [value]);
  return (
    <Textarea
      autoFocus={autoFocus}
      ref={assignRef}
      aria-label={ariaLabel}
      className="min-h-16 resize-none overflow-hidden overscroll-contain bg-background font-mono text-xs leading-5"
      placeholder={placeholder}
      rows={2}
      value={value}
      onChange={(event) => onChange(event.target.value)}
    />
  );
}

export function GuideCapabilityFields({
  capabilities,
  capabilitiesLoading,
  name,
  exposure,
  guide,
  onNameChange,
  onExposureChange,
  onGuideChange,
  editorRef,
  autoFocus = false,
  showGuide = true,
}: {
  capabilities: WorkflowCapabilityOption[];
  capabilitiesLoading: boolean;
  name: string;
  exposure: "direct" | "meta_on_demand";
  guide?: string;
  onNameChange: (name: string, capability?: WorkflowCapabilityOption) => void;
  onExposureChange: (value: "direct" | "meta_on_demand") => void;
  onGuideChange?: (value: string) => void;
  editorRef?: RefObject<HTMLTextAreaElement | null>;
  autoFocus?: boolean;
  showGuide?: boolean;
}) {
  const { t } = useTranslation(["profiles"]);
  const options = useMemo(() => {
    if (!name || capabilities.some((item) => item.label === name)) {
      return capabilities;
    }
    return [
      {
        ref_id: name,
        server_id: "",
        label: name,
        kind: "tool",
      },
      ...capabilities,
    ];
  }, [capabilities, name]);
  return (
    <div className="flex flex-col gap-2">
      <div className="grid min-w-0 grid-cols-[2fr_1fr] gap-2">
        <CapabilityCombobox
          emptyLabel={t("profiles:detail.workflow.guide.noMatchingCapabilities", {
            defaultValue: "No matching capabilities.",
          })}
          getDescription={(item) => item.description}
          getKey={(item) => item.label}
          getLabel={(item) => item.label}
          items={options}
          kind="capability"
          loading={capabilitiesLoading}
          onChange={(key, item) => onNameChange(item?.label ?? key, item)}
          placeholder={t("profiles:detail.workflow.guide.searchCapabilities", {
            defaultValue: "Search capabilities...",
          })}
          triggerClassName={GUIDE_COMBO_TRIGGER_CLASS}
          value={name || undefined}
        />
        <Select
          onValueChange={(value) =>
            onExposureChange(value as "direct" | "meta_on_demand")
          }
          value={exposure}
        >
          <SelectTrigger
            aria-label={t("profiles:detail.workflow.guide.capabilityExposure", {
              defaultValue: "Capability exposure",
            })}
            className="h-9 min-w-0 px-3 text-xs"
          >
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectGroup>
              <SelectItem value="meta_on_demand">
                {t("profiles:detail.workflow.guide.metaOnDemand", {
                  defaultValue: "Meta on demand",
                })}
              </SelectItem>
              <SelectItem value="direct">
                {t("profiles:detail.workflow.guide.directExposure", {
                  defaultValue: "Direct exposure",
                })}
              </SelectItem>
            </SelectGroup>
          </SelectContent>
        </Select>
      </div>
      {showGuide ? (
        <GuideAutosizeTextarea
          autoFocus={autoFocus}
          editorRef={editorRef}
          ariaLabel={t("profiles:detail.workflow.guide.capabilityGuide", {
            defaultValue: "Guide",
          })}
          placeholder={t(
            "profiles:detail.workflow.guide.capabilityGuidePlaceholder",
            { defaultValue: "How this occurrence should be used" },
          )}
          value={guide ?? ""}
          onChange={(value) => onGuideChange?.(value)}
        />
      ) : null}
    </div>
  );
}

const GUIDE_CAPABILITY_KIND_ICONS = {
  tool: Wrench,
  prompt: MessageSquare,
  resource: Database,
  template: LayoutTemplate,
} as const satisfies Record<WorkflowCapabilityKind, typeof Wrench>;

function GuideCapabilityFieldRow({
  label,
  children,
  muted = false,
}: {
  label: string;
  children: ReactNode;
  muted?: boolean;
}) {
  return (
    <p className="min-w-0 whitespace-pre-wrap leading-6">
      <span className="text-muted-foreground">{label}:</span>{" "}
      <span className={cn(muted && "italic text-muted-foreground")}>
        {children}
      </span>
    </p>
  );
}

export function GuideCapabilityBlock({
  capability,
  capabilities,
}: {
  capability: {
    name: string;
    exposure: "direct" | "meta_on_demand";
    guide: string;
  };
  capabilities: WorkflowCapabilityOption[];
}) {
  const { t } = useTranslation(["profiles"]);
  const kind = resolveWorkflowCapabilityKind(capability.name, capabilities);
  const Icon = GUIDE_CAPABILITY_KIND_ICONS[kind];
  const exposureValue =
    capability.exposure === "direct"
      ? t("profiles:detail.workflow.guide.directExposure", {
        defaultValue: "Direct exposure",
      })
      : t("profiles:detail.workflow.guide.metaOnDemand", {
        defaultValue: "Meta on demand",
      });
  const guideValue = capability.guide.trim();

  return (
    <div className="space-y-1 pl-5 text-sm leading-6 text-slate-700 dark:text-slate-300">
      <div className="flex min-w-0 items-start gap-2">
        <Icon
          aria-hidden
          className="mt-0.5 h-4 w-4 shrink-0 text-muted-foreground"
        />
        <span className="min-w-0 font-medium text-foreground">
          {capability.name}
        </span>
      </div>
      <GuideCapabilityFieldRow
        label={t("profiles:detail.workflow.guide.exposure", {
          defaultValue: "Exposure",
        })}
      >
        {exposureValue}
      </GuideCapabilityFieldRow>
      <GuideCapabilityFieldRow
        label={t("profiles:detail.workflow.guide.capabilityGuide", {
          defaultValue: "Guide",
        })}
        muted={!guideValue}
      >
        {guideValue ||
          t("profiles:detail.workflow.guide.capabilityGuidePlaceholder", {
            defaultValue: "How this occurrence should be used",
          })}
      </GuideCapabilityFieldRow>
    </div>
  );
}

function CapabilityInsertFields({
  capabilities,
  capabilitiesLoading,
  selectedCapability,
  bindingPolicy,
  capabilityGuide,
  onSelectCapability,
  onBindingPolicyChange,
  onCapabilityGuideChange,
}: {
  capabilities: WorkflowCapabilityOption[];
  capabilitiesLoading: boolean;
  selectedCapability: WorkflowCapabilityOption | null;
  bindingPolicy: "direct" | "meta_on_demand";
  capabilityGuide: string;
  onSelectCapability: (capability: WorkflowCapabilityOption) => void;
  onBindingPolicyChange: (value: "direct" | "meta_on_demand") => void;
  onCapabilityGuideChange: (value: string) => void;
}) {
  return (
    <GuideCapabilityFields
      capabilities={capabilities}
      capabilitiesLoading={capabilitiesLoading}
      exposure={bindingPolicy}
      guide={capabilityGuide}
      name={selectedCapability?.label ?? ""}
      onExposureChange={onBindingPolicyChange}
      onGuideChange={onCapabilityGuideChange}
      onNameChange={(_name, capability) => {
        if (capability) onSelectCapability(capability);
      }}
    />
  );
}

function ExternalMarkdownInsertFields({
  composerId,
  offset,
  title,
  onTitleChange,
}: {
  composerId: string;
  offset: number;
  title: string;
  onTitleChange: (value: string) => void;
}) {
  const { t } = useTranslation(["profiles"]);
  const titleId = `${composerId}-external-${offset}`;
  return (
    <Input
      autoFocus
      aria-label={t("profiles:detail.workflow.guide.sectionName", {
        defaultValue: "Section name",
      })}
      className={GUIDE_FIELD_CLASS}
      id={titleId}
      value={title}
      onChange={(event) => onTitleChange(event.target.value)}
      placeholder={t("profiles:detail.workflow.guide.externalMarkdownPlaceholder", {
        defaultValue:
          "Enter a section name to create a Markdown document at this position",
      })}
    />
  );
}

function PackageInsertFields({
  composerId,
  files,
  packageCategory,
  packageFile,
  packageTitle,
  onInsertExisting,
  onPackageTitleChange,
  onPackageUploadChange,
}: {
  composerId: string;
  files: WorkflowGuidePackageFile[];
  packageCategory: WorkflowGuidePackageCategory;
  packageFile: File | null;
  packageTitle: string;
  onInsertExisting: (file: WorkflowGuidePackageFile) => void;
  onPackageTitleChange: (value: string) => void;
  onPackageUploadChange: (file: File | null) => void;
}) {
  const { t } = useTranslation(["profiles"]);
  const titleId = `${composerId}-package-title`;
  const uploadId = `${composerId}-package-upload`;
  const uploadInputRef = useRef<HTMLInputElement>(null);
  const acceptedTypes = acceptedExtensionList(packageCategory);
  const emptyStatus = t("profiles:detail.workflow.guide.noFileSelected", {
    defaultValue: "No file selected",
  });
  const status = packageFile ? packageFile.name : `${emptyStatus}  ${acceptedTypes}`;
  const hasExistingFiles = files.length > 0;
  return (
    <>
      <div className={hasExistingFiles ? "grid min-w-0 grid-cols-[2fr_1fr] gap-2" : undefined}>
        <div
          className={cn(
            GUIDE_FIELD_CLASS,
            "flex min-w-0 items-center gap-2 overflow-hidden rounded-md border border-input p-2",
          )}
        >
          <input
            ref={uploadInputRef}
            accept={acceptedExtensions(packageCategory)}
            aria-hidden={true}
            className="sr-only"
            id={uploadId}
            tabIndex={-1}
            type="file"
            onChange={(event) => {
              const file = event.target.files?.[0] ?? null;
              onPackageUploadChange(file);
              if (file && !packageTitle.trim()) {
                onPackageTitleChange(titleFromFileName(file.name));
              }
            }}
          />
          <Button
            aria-label={`${t("profiles:detail.workflow.guide.chooseFile", {
              defaultValue: "Choose file",
            })}. ${status}`}
            className="h-auto shrink-0 p-0 text-xs font-normal hover:bg-transparent hover:text-foreground"
            onClick={() => uploadInputRef.current?.click()}
            size="sm"
            type="button"
            variant="ghost"
          >
            {t("profiles:detail.workflow.guide.chooseFile", {
              defaultValue: "Choose file",
            })}
          </Button>
          <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground" title={status}>
            {status}
          </span>
        </div>
        {hasExistingFiles ? (
          <CapabilityCombobox
            emptyLabel={t("profiles:detail.workflow.guide.noMatchingPackageFiles", {
              defaultValue: "No matching files.",
            })}
            getDescription={(file) => file.relative_path}
            getKey={(file) => file.package_file_id}
            getLabel={(file) => file.title}
            items={files}
            kind="resource"
            onChange={(_key, file) => {
              if (file) onInsertExisting(file);
            }}
            placeholder={t("profiles:detail.workflow.guide.searchExistingFiles", {
              defaultValue: "Search existing...",
            })}
            triggerClassName={GUIDE_COMBO_TRIGGER_CLASS}
          />
        ) : null}
      </div>
      <Input
        aria-label={t("profiles:detail.workflow.guide.fileTitle", {
          defaultValue: "File title",
        })}
        className={GUIDE_FIELD_CLASS}
        id={titleId}
        value={packageTitle}
        onChange={(event) => onPackageTitleChange(event.target.value)}
        placeholder={t("profiles:detail.workflow.guide.fileTitle", {
          defaultValue: "File title",
        })}
      />
    </>
  );
}

const insertTypeOptions = [
  {
    id: "markdown" as const,
    icon: FileText,
    labelKey: "profiles:detail.workflow.guide.inPlaceMarkdown",
    defaultValue: "In-Place Markdown",
  },
  {
    id: "external_markdown" as const,
    icon: FilePlus2,
    labelKey: "profiles:detail.workflow.guide.externalMarkdown",
    defaultValue: "External Markdown",
  },
  {
    id: "reference" as const,
    icon: Files,
    labelKey: "profiles:detail.workflow.guide.reference",
    defaultValue: "Reference",
  },
  {
    id: "capability" as const,
    icon: Wrench,
    labelKey: "profiles:detail.workflow.guide.capability",
    defaultValue: "Capability",
  },
  {
    id: "script" as const,
    icon: FileCode,
    labelKey: "profiles:detail.workflow.guide.script",
    defaultValue: "Script",
  },
  {
    id: "asset" as const,
    icon: Paperclip,
    labelKey: "profiles:detail.workflow.guide.asset",
    defaultValue: "Asset",
  },
];

function insertKindTitle(
  kind: InsertKind | null,
  t: (key: string, options: { defaultValue: string }) => string,
) {
  const option = insertTypeOptions.find((candidate) => candidate.id === kind);
  if (!option) {
    return t("profiles:detail.workflow.guide.reference", {
      defaultValue: "Reference",
    });
  }
  return t(option.labelKey, { defaultValue: option.defaultValue });
}

function titleFromFileName(name: string) {
  const base = name.split(/[/\\]/).pop() ?? name;
  const dot = base.lastIndexOf(".");
  return (dot > 0 ? base.slice(0, dot) : base).trim();
}

function acceptedExtensions(category: WorkflowGuidePackageCategory) {
  if (category === "reference") return ".json,.yaml,.yml,.toml";
  if (category === "script") return ".js,.mjs,.cjs,.py,.sh,.bat";
  return ".pdf,.docx,.xlsx,.png,.jpg,.jpeg,.webp,.svg,.gif,.ico,.csv,.sql";
}

function acceptedExtensionList(category: WorkflowGuidePackageCategory) {
  return acceptedExtensions(category).split(",").join(", ");
}

function packageCategoryForInsert(
  activeInsert: InsertKind | null,
): WorkflowGuidePackageCategory {
  if (activeInsert === "script") return "script";
  if (activeInsert === "asset") return "asset";
  return "reference";
}
