import { GripVertical } from "lucide-react";
import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";

import { cn } from "../lib/utils";

interface ResizableSplitPaneProps {
	children: [ReactNode, ReactNode];
	dividerAriaLabel: string;
	className?: string;
	initialLeftWidth?: number;
	minLeftWidth?: number;
	maxLeftWidth?: number;
	preferRightPanelSpace?: boolean;
	/** When true, the trailing panel keeps a resizable fixed width. */
	trailingFixed?: boolean;
	initialTrailingWidth?: number;
	minTrailingWidth?: number;
	maxTrailingWidth?: number;
}

export function ResizableSplitPane({
	children: [left, right],
	dividerAriaLabel,
	className,
	initialLeftWidth = 300,
	minLeftWidth = 240,
	maxLeftWidth = 460,
	preferRightPanelSpace = false,
	trailingFixed = false,
	initialTrailingWidth = 224,
	minTrailingWidth = 160,
	maxTrailingWidth = 400,
}: ResizableSplitPaneProps) {
	const [leadingWidth, setLeadingWidth] = useState(initialLeftWidth);
	const [trailingWidth, setTrailingWidth] = useState(initialTrailingWidth);
	const containerRef = useRef<HTMLDivElement>(null);
	const containerWidthRef = useRef<number | null>(null);
	const panelWidth = trailingFixed ? trailingWidth : leadingWidth;
	const setPanelWidth = trailingFixed ? setTrailingWidth : setLeadingWidth;
	const minPanelWidth = trailingFixed ? minTrailingWidth : minLeftWidth;
	const maxPanelWidth = trailingFixed ? maxTrailingWidth : maxLeftWidth;
	const shrinkOnContainerResize = trailingFixed || preferRightPanelSpace;

	useEffect(() => {
		if (!shrinkOnContainerResize || !containerRef.current) return;

		const observer = new ResizeObserver(([entry]) => {
			const width = entry.contentRect.width;
			const previousWidth = containerWidthRef.current;
			containerWidthRef.current = width;
			if (previousWidth !== null && width < previousWidth) {
				setPanelWidth((current) =>
					Math.max(minPanelWidth, current - (previousWidth - width)),
				);
			}
		});
		observer.observe(containerRef.current);
		return () => observer.disconnect();
	}, [minPanelWidth, shrinkOnContainerResize, setPanelWidth]);

	const handleDividerPointerDown = useCallback(
		(event: React.PointerEvent<HTMLButtonElement>) => {
			event.preventDefault();
			const startX = event.clientX;
			const startWidth = panelWidth;
			const handlePointerMove = (moveEvent: PointerEvent) => {
				const delta = moveEvent.clientX - startX;
				const nextWidth = trailingFixed ? startWidth - delta : startWidth + delta;
				setPanelWidth(Math.min(maxPanelWidth, Math.max(minPanelWidth, nextWidth)));
			};
			const handlePointerUp = () => {
				window.removeEventListener("pointermove", handlePointerMove);
				window.removeEventListener("pointerup", handlePointerUp);
			};

			window.addEventListener("pointermove", handlePointerMove);
			window.addEventListener("pointerup", handlePointerUp);
		},
		[maxPanelWidth, minPanelWidth, panelWidth, setPanelWidth, trailingFixed],
	);

	const gridTemplateColumns = trailingFixed
		? `minmax(0, 1fr) 8px ${panelWidth}px`
		: `${panelWidth}px 8px minmax(0, 1fr)`;

	return (
		<div
			ref={containerRef}
			className={cn("grid min-h-0 flex-1 overflow-hidden", className)}
			style={{ gridTemplateColumns }}
		>
			{left}
			<button
				type="button"
				aria-label={dividerAriaLabel}
				className="group flex cursor-col-resize items-center justify-center border-x border-border bg-muted/20 text-muted-foreground transition-colors hover:bg-muted/50 focus:outline-none focus-visible:ring-2 focus-visible:ring-ring"
				onPointerDown={handleDividerPointerDown}
			>
				<GripVertical className="h-4 w-4 opacity-50 group-hover:opacity-80" />
			</button>
			{right}
		</div>
	);
}
