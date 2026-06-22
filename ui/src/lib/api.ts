import type { Overview, RunDetail, Usage, UsageFilters } from "./types";

async function fetchJson<T>(path: string): Promise<T> {
  const response = await fetch(path);
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
  if (filters.source) params.set("source", filters.source);
  if (filters.model) params.set("model", filters.model);
  return fetchJson<Usage>(`/api/usage?${params.toString()}`);
}

export function fetchRunDetail(runId: string) {
  return fetchJson<RunDetail>(`/api/runs/${encodeURIComponent(runId)}`);
}
