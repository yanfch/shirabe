import type { Run, Totals } from "./types";

export function fmtCompact(value: number | null | undefined) {
  if (value == null) return "-";
  return new Intl.NumberFormat("en-US", {
    maximumFractionDigits: 1,
    notation: "compact",
  }).format(value);
}

export function fmtNumber(value: number | null | undefined) {
  if (value == null) return "-";
  return new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 }).format(value);
}

export function fmtPercent(value: number | null | undefined) {
  if (value == null || Number.isNaN(value)) return "-";
  return `${Math.round(value * 100)}%`;
}

export function fmtCost(value: number | null | undefined) {
  if (value == null) return "-";
  return new Intl.NumberFormat("en-US", {
    currency: "USD",
    maximumFractionDigits: value >= 1 ? 2 : 4,
    minimumFractionDigits: value >= 1 ? 2 : 4,
    style: "currency",
  }).format(value);
}

export function fmtTime(ns: number | null | undefined) {
  if (!ns) return "-";
  return new Date(Math.floor(ns / 1_000_000)).toLocaleString();
}

export function cacheRatio(totals: Totals | null | undefined) {
  if (!totals || totals.input_tokens <= 0) return null;
  return totals.cache_read_tokens / totals.input_tokens;
}

export function cacheRate(inputTokens: number, cacheReadTokens: number) {
  if (inputTokens <= 0) return null;
  return cacheReadTokens / inputTokens;
}

export function runCacheRatio(run: Run) {
  if (run.input_tokens <= 0) return null;
  return run.cache_read_tokens / run.input_tokens;
}

export function shortId(id: string) {
  const parts = id.split(":");
  return parts.at(-1) ?? id;
}

export function signalLabel(value: string | null) {
  return value ? value.replaceAll("_", " ") : "none";
}
