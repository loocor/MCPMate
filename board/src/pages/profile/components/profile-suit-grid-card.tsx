import { useTranslation } from "react-i18next";
import { EntityCard } from "../../../components/entity-card";
import { Badge } from "../../../components/ui/badge";
import { Switch } from "../../../components/ui/switch";
import { useProfileTokenChartSource } from "../../../lib/api";
import { useAppStore } from "../../../lib/store";
import type { ConfigSuit } from "../../../lib/types";
import { ProfileTokenUsageChart } from "./profile-token-usage-chart";

export interface ProfileSuitGridCardProps {
	suit: ConfigSuit;
	statItems: Array<{ label: string; value: string | number }>;
	displayName: string;
	avatarInitial: string;
	isDefaultAnchor: boolean;
	isTogglePending: boolean;
	onNavigate: () => void;
	onToggle: () => void;
	enabledByComponentId: Map<string, boolean>;
	/** When loaded and zero, chart shows gray ring with "-" (omit while stats loading). */
	profileServerCount?: number;
	reviewCount?: number;
}

export function ProfileSuitGridCard({
	suit,
	statItems,
	displayName,
	avatarInitial,
	isDefaultAnchor,
	isTogglePending,
	onNavigate,
	onToggle,
	enabledByComponentId,
	profileServerCount,
	reviewCount = 0,
}: ProfileSuitGridCardProps) {
	const { t } = useTranslation();
	const isWorkflowProfile = suit.profile_mode === "workflow";
	const profileTokenEstimateMethod = useAppStore(
		(state) => state.dashboardSettings.profileTokenEstimateMethod,
	);
	const tokenSource = useProfileTokenChartSource(suit.id, enabledByComponentId, {
		enabled: !isWorkflowProfile,
	});
	const profileModeLabel = t(
		`profiles:badges.${suit.profile_mode === "workflow" ? "workflow" : "capability"}`,
	);

	return (
		<EntityCard
			id={suit.id}
			title={displayName}
			description={suit.description}
			topRightBadgePosition="corner"
			avatar={{
				fallback: avatarInitial,
			}}
			topRightBadge={
				<>
					{!isWorkflowProfile ? (
						<ProfileTokenUsageChart
							layout="chartOnly"
							ledgerItems={tokenSource.ledgerItems}
							fallbackEstimate={tokenSource.fallbackEstimate}
							isLoading={tokenSource.isLoading}
							isError={tokenSource.isError}
							enabledByComponentId={enabledByComponentId}
							estimateMethod={profileTokenEstimateMethod}
							profileServerCount={profileServerCount}
							className="-mr-1"
						/>
					) : null}
					{reviewCount > 0 ? (
						<Badge className="border-amber-300 bg-amber-100 text-amber-900 hover:bg-amber-100 dark:border-amber-700 dark:bg-amber-950 dark:text-amber-200">
							{t("surfaceReview:badge", {
								count: reviewCount,
								defaultValue: "{{count}} to review",
							})}
						</Badge>
					) : null}
				</>
			}
			className={
				reviewCount > 0
					? "border-amber-400 bg-amber-50/50 dark:border-amber-700 dark:bg-amber-950/20"
					: undefined
			}
			stats={statItems}
			bottomLeft={
				<Badge variant={suit.is_active ? "default" : "secondary"}>
					{profileModeLabel}
				</Badge>
			}
			bottomRight={
				<Switch
					checked={suit.is_active}
					onCheckedChange={onToggle}
					disabled={isTogglePending || isDefaultAnchor || isWorkflowProfile}
					onClick={(e) => e.stopPropagation()}
				/>
			}
			onClick={onNavigate}
		/>
	);
}
