import type { MenubarUsage, Overview, RunDetail, SessionDetail, SessionList, SyncStatus, Usage, UsageFilters, WorkspaceStatus } from "./types";

async function fetchJson<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, init);
  if (!response.ok) {
    throw new Error(await response.text());
  }
  return response.json() as Promise<T>;
}

export function fetchOverview() {
  return fetchJson<Overview>("/api/overview");
}

export function fetchUsage(filters: UsageFilters) {
  const params = new URLSearchParams();
  params.set("preset", filters.preset);
  params.set("grain", filters.grain);
  if (filters.preset === "custom") {
    if (filters.from) params.set("from", filters.from);
    if (filters.to) params.set("to", filters.to);
  }
  if (filters.profile_id) params.set("profile_id", filters.profile_id);
  if (filters.device_id) params.set("device_id", filters.device_id);
  if (filters.source) params.set("source", filters.source);
  if (filters.model) params.set("model", filters.model);
  return fetchJson<Usage>(`/api/usage?${params.toString()}`);
}

export function fetchMenubarUsage(range: string, profileId?: string | null) {
  const params = new URLSearchParams();
  params.set("range", range);
  if (profileId) params.set("profile_id", profileId);
  return fetchJson<MenubarUsage>(`/api/menubar?${params.toString()}`);
}

export function fetchRunDetail(runId: string) {
  return fetchJson<RunDetail>(`/api/runs/${encodeURIComponent(runId)}`);
}

export function fetchSessions() {
  return fetchJson<SessionList>("/api/sessions");
}

export function fetchSessionDetail(sessionId: string) {
  return fetchJson<SessionDetail>(`/api/sessions/${encodeURIComponent(sessionId)}`);
}

export function fetchSyncStatus() {
  return fetchJson<SyncStatus>("/api/sync/status");
}

export function fetchWorkspaceStatus() {
  return fetchJson<WorkspaceStatus>("/api/workspace");
}

export function repairWorkspace() {
  return fetchJson<WorkspaceStatus>("/api/workspace/repair", { method: "POST" });
}

export function startSync() {
  return fetchJson<SyncStatus>("/api/sync", { method: "POST" });
}
