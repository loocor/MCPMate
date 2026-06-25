const DEMO_MODE_ENABLED_VALUES = new Set(["1", "true", "yes", "on"]);

export function isBoardDemoModeEnabled(): boolean {
	const raw = import.meta.env.VITE_MCPMATE_BOARD_DEMO_MODE;
	return (
		typeof raw === "string" &&
		DEMO_MODE_ENABLED_VALUES.has(raw.trim().toLowerCase())
	);
}

export function isBoardDemoMode(): boolean {
	return isBoardDemoModeEnabled();
}
