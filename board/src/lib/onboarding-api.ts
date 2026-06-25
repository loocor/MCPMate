import { API_BASE_URL } from "./api";
import { handleDemoApiRequest } from "./demo-api";
import { isBoardDemoMode } from "./demo-mode";
import type { ClientConfigFileParse } from "./types";

export interface OnboardingStatusData {
  completed: boolean;
  servers_count: number;
  clients_count: number;
}

export interface OnboardingStatusResp {
  success: boolean;
  data?: OnboardingStatusData;
  error?: { code: string; message: string };
}

export interface OnboardingActionResp {
  success: boolean;
  data?: { ok: boolean };
  error?: { code: string; message: string };
}

export interface RuntimeEntry {
  name: string;
  available: boolean;
  version?: string;
  path?: string;
  /** "mcpMate" | "system" | "uvPython"; undefined if unavailable */
  source?: "mcpMate" | "system" | "uvPython";
}

export interface RuntimeCheckData {
  runtimes: RuntimeEntry[];
  has_js_runtime: boolean;
  has_python_runtime: boolean;
}

export interface RuntimeCheckResp {
  success: boolean;
  data?: RuntimeCheckData;
  error?: { code: string; message: string };
}

export interface OnboardingServerScanClient {
  identifier: string;
  display_name?: string;
  config_path: string;
  config_file_parse?: ClientConfigFileParse | null;
}

export interface OnboardingServerCandidate {
  key: string;
  name: string;
  kind: string;
  command?: string | null;
  args: string[];
  env: Record<string, string>;
  url?: string | null;
  source_clients: string[];
  source_client_ids: string[];
}

export interface OnboardingServerScanError {
  client_name: string;
  message: string;
}

export interface OnboardingServerScanData {
  candidates: OnboardingServerCandidate[];
  errors: OnboardingServerScanError[];
}

export interface OnboardingServerScanResp {
  success: boolean;
  data?: OnboardingServerScanData;
  error?: { code: string; message: string };
}

async function fetchOnboarding<T>(
  path: string,
  init?: RequestInit,
): Promise<T> {
  if (isBoardDemoMode()) {
    return handleDemoApiRequest<T>(path, init);
  }

  const response = await fetch(`${API_BASE_URL}${path}`, {
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    credentials: "include",
    ...init,
  });
  if (!response.ok) {
    const text = await response.text().catch(() => "");
    throw new Error(`Onboarding API ${response.status}: ${text}`);
  }
  return response.json() as Promise<T>;
}

export const onboardingApi = {
  getStatus: () =>
    fetchOnboarding<OnboardingStatusResp>("/api/onboarding/status"),

  complete: (completed: boolean) =>
    fetchOnboarding<OnboardingActionResp>("/api/onboarding/complete", {
      method: "POST",
      body: JSON.stringify({ completed }),
    }),

  reset: () =>
    fetchOnboarding<OnboardingActionResp>("/api/onboarding/reset", {
      method: "POST",
    }),

  runtimeCheck: () =>
    fetchOnboarding<RuntimeCheckResp>("/api/onboarding/runtime-check"),

  scanServers: (clients: OnboardingServerScanClient[]) =>
    fetchOnboarding<OnboardingServerScanResp>("/api/onboarding/server-scan", {
      method: "POST",
      body: JSON.stringify({ clients }),
    }),
};
