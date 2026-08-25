import type { ReactNode } from "react";
import { cn } from "../../lib/utils";
import { BulkSelectionToolbar } from "./bulk-selection-toolbar";
import type { BulkAction } from "./types";
import { useBulkSelectionLabels } from "./use-bulk-selection-labels";

type BulkSelectionHeaderProps = {
	title?: ReactNode;
	description?: ReactNode;
	leading?: ReactNode;
	isBulkMode?: boolean;
	onToggleBulkMode?: () => void;
	actions?: BulkAction[];
	trailing?: ReactNode;
	showModeToggle?: boolean;
	className?: string;
};

export function BulkSelectionHeader({
	title,
	description,
	leading,
	isBulkMode = false,
	onToggleBulkMode,
	actions = [],
	trailing,
	showModeToggle = true,
	className,
}: BulkSelectionHeaderProps) {
	const { modeToggleLabel, modeExitLabel } = useBulkSelectionLabels();
	const descriptionTitle =
		typeof description === "string" ? description : undefined;
	return (
		<div
			className={cn(
				"mb-3 flex shrink-0 items-center justify-between gap-3",
				className,
			)}
		>
			{leading ? (
				<div className="min-w-0 flex-1">{leading}</div>
			) : (
				<div className="min-w-0">
					{title ? (
						<div className="text-sm font-semibold text-slate-900 dark:text-slate-100">
							{title}
						</div>
					) : null}
					{description ? (
						<div
							className="truncate text-xs text-slate-500 dark:text-slate-400"
							title={descriptionTitle}
						>
							{description}
						</div>
					) : null}
				</div>
			)}
			{trailing || (showModeToggle && onToggleBulkMode) ? (
				<div className="flex shrink-0 items-center gap-2">
					{trailing}
					{showModeToggle && onToggleBulkMode ? (
						<BulkSelectionToolbar
							isBulkMode={isBulkMode}
							onToggleMode={onToggleBulkMode}
							modeToggleLabel={modeToggleLabel}
							modeExitLabel={modeExitLabel}
							actions={actions}
						/>
					) : null}
				</div>
			) : null}
		</div>
	);
}
