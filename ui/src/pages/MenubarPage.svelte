<script lang="ts">
  import ExternalLink from "@lucide/svelte/icons/external-link";
  import RefreshCw from "@lucide/svelte/icons/refresh-cw";
  import { onDestroy, onMount } from "svelte";
  import { fetchMenubarUsage, startSync } from "../lib/api";
  import {
    cacheRate,
    fmtCompact,
    fmtCost,
    fmtPercent,
    fmtTime,
    shortId,
  } from "../lib/format";
  import type { MenubarUsage, UsageBucketSummary } from "../lib/types";

  type RangeOption = "today" | "7d" | "30d";

  const ranges: { value: RangeOption; label: string }[] = [
    { value: "today", label: "Today" },
    { value: "7d", label: "7d" },
    { value: "30d", label: "30d" },
  ];

  const menubarDocumentClass = "menubar-document";
  const refreshIntervalMs = 60_000;

  let range: RangeOption = normalizeRange(new URLSearchParams(location.search).get("range"));
  let data: MenubarUsage | null = null;
  let loading = true;
  let syncing = false;
  let error: string | null = null;
  let syncPoll: number | null = null;
  let refreshTimer: number | null = null;
  let trendHover:
    | {
        index: number;
        x: number;
        y: number;
      }
    | null = null;

  $: summary = data?.summary;
  $: trend = [...(data?.trend ?? [])].reverse();
  $: sourceUsage = data?.source_usage ?? [];
  $: recentRuns = data?.recent_runs ?? [];
  $: totalTokens = summary?.total_tokens ?? 0;
  $: cacheHit = summary?.cache_hit_rate ?? null;
  $: failedRate = summary?.tool_failure_rate ?? null;
  $: hiddenSources = Math.max(sourceUsage.length - 2, 0);
  $: syncRunning = data?.sync.state === "running" || syncing;
  $: hoveredTrend = trendHover ? trend[trendHover.index] : null;
  $: isHourlyTrend = range === "today";
  $: hasUsage =
    (summary?.sessions ?? 0) > 0 ||
    (summary?.runs ?? 0) > 0 ||
    totalTokens > 0 ||
    (summary?.tool_calls ?? 0) > 0;
  $: isRangeEmpty = Boolean(data) && !hasUsage;
  $: latestRun = data?.latest_run ?? null;
  $: emptyCaption = latestRun
    ? `Last active ${fmtTime(latestRun.started_at_ns)} · ${latestRun.source}`
    : "No local runs in this range.";

  function normalizeRange(value: string | null): RangeOption {
    return value === "7d" || value === "30d" ? value : "today";
  }

  function rangeCaption(value: RangeOption) {
    return value === "today" ? "today" : value === "7d" ? "last 7 days" : "last 30 days";
  }

  async function load(nextRange = range) {
    loading = !data;
    error = null;
    try {
      data = await fetchMenubarUsage(nextRange);
      range = normalizeRange(data.range);
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      loading = false;
    }
  }

  async function chooseRange(nextRange: RangeOption) {
    range = nextRange;
    trendHover = null;
    const params = new URLSearchParams(location.search);
    params.set("view", "menubar");
    params.set("range", nextRange);
    history.replaceState(history.state, "", `/?${params.toString()}`);
    await load(nextRange);
  }

  async function syncNow() {
    if (syncRunning) return;
    syncing = true;
    error = null;
    try {
      await startSync();
      beginSyncPolling();
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
      syncing = false;
      await load();
    }
  }

  function beginSyncPolling() {
    if (syncPoll !== null) window.clearInterval(syncPoll);
    syncPoll = window.setInterval(async () => {
      await load();
      if (data?.sync.state !== "running") {
        if (syncPoll !== null) window.clearInterval(syncPoll);
        syncPoll = null;
        syncing = false;
      }
    }, 1500);
  }

  function refreshIfActive() {
    if (document.visibilityState === "hidden" || syncPoll !== null || syncing) return;
    void load();
  }

  function handleVisibilityChange() {
    if (document.visibilityState === "visible") refreshIfActive();
  }

  function openDashboard() {
    window.location.href = "/?page=usage";
  }

  function openRun(runId: string) {
    window.location.href = `/?run=${encodeURIComponent(runId)}`;
  }

  function totalFor(point: UsageBucketSummary) {
    return point.input_tokens + point.output_tokens;
  }

  $: maxTrend = Math.max(1, ...trend.map(totalFor));

  function barHeight(point: UsageBucketSummary) {
    const value = totalFor(point);
    if (value <= 0) return 2;
    return Math.max(6, (value / maxTrend) * 100);
  }

  function barIntensity(point: UsageBucketSummary) {
    const value = totalFor(point);
    if (value <= 0) return 0;
    return Math.max(0.24, value / maxTrend);
  }

  function barStyle(point: UsageBucketSummary) {
    const intensity = barIntensity(point);
    const topAlpha = totalFor(point) <= 0 ? 0.11 : 0.26 + intensity * 0.52;
    const bottomAlpha = totalFor(point) <= 0 ? 0.08 : 0.13 + intensity * 0.24;
    const borderAlpha = totalFor(point) <= 0 ? 0.1 : 0.22 + intensity * 0.42;
    const glowAlpha = totalFor(point) <= 0 ? 0 : 0.04 + intensity * 0.13;
    return [
      `height: ${barHeight(point)}%`,
      `--fill-top: ${topAlpha.toFixed(3)}`,
      `--fill-bottom: ${bottomAlpha.toFixed(3)}`,
      `--border-alpha: ${borderAlpha.toFixed(3)}`,
      `--glow-alpha: ${glowAlpha.toFixed(3)}`,
    ].join("; ");
  }

  function shortDate(point: UsageBucketSummary) {
    if (point.bucket_key.includes(":")) return point.bucket_key;
    return point.date.length > 7 ? point.date.slice(5) : point.date;
  }

  function trendSubtitle() {
    if (range === "today") return "Today (24h view)";
    return rangeCaption(range);
  }

  function sourceShare(tokens: number) {
    return totalTokens > 0 ? tokens / totalTokens : null;
  }

  function showTrendTooltip(event: Event, index: number) {
    const bar = event.currentTarget instanceof HTMLElement ? event.currentTarget : null;
    const chart = bar?.closest(".spark-bars");
    const barRect = bar?.getBoundingClientRect();
    const chartRect = chart?.getBoundingClientRect();
    if (!barRect || !chartRect) return;

    const centerX = barRect.left + barRect.width / 2 - chartRect.left;
    const barTop = barRect.top - chartRect.top;
    const tooltipHalfWidth = 70;
    const tooltipAnchorY = Math.min(Math.max(barTop, 22), chartRect.height - 10);

    trendHover = {
      index,
      x: Math.min(Math.max(centerX, tooltipHalfWidth), Math.max(tooltipHalfWidth, chartRect.width - tooltipHalfWidth)),
      y: tooltipAnchorY,
    };
  }

  function fmtMenubarCost(value: number | null | undefined) {
    if (value == null) return "-";
    if (Math.abs(value) >= 1000) return `$${fmtCompact(value)}`;
    return fmtCost(value);
  }

  onMount(() => {
    document.documentElement.classList.add(menubarDocumentClass);
    document.body.classList.add(menubarDocumentClass);
    window.scrollTo(0, 0);
    void load();
    refreshTimer = window.setInterval(refreshIfActive, refreshIntervalMs);
    window.addEventListener("focus", refreshIfActive);
    document.addEventListener("visibilitychange", handleVisibilityChange);
  });

  onDestroy(() => {
    if (syncPoll !== null) window.clearInterval(syncPoll);
    if (refreshTimer !== null) window.clearInterval(refreshTimer);
    window.removeEventListener("focus", refreshIfActive);
    document.removeEventListener("visibilitychange", handleVisibilityChange);
    document.documentElement.classList.remove(menubarDocumentClass);
    document.body.classList.remove(menubarDocumentClass);
  });
</script>

<main class="menubar-root">
  <section class="panel menubar-shell">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>

    <header class="menubar-header">
      <div class="menubar-title">Shirabe</div>
      <button class:running={syncRunning} class="sync-button" type="button" on:click={syncNow}>
        <RefreshCw class={syncRunning ? "spinning" : ""} size={13} strokeWidth={1.7} aria-hidden="true" />
        <span>{syncRunning ? "SYNCING" : "SYNC"}</span>
      </button>
    </header>

    <div class="range-tabs" aria-label="Time range">
      {#each ranges as item}
        <button
          class:active={range === item.value}
          type="button"
          on:click={() => chooseRange(item.value)}
        >
          {item.label}
        </button>
      {/each}
    </div>

    {#if error}
      <div class="menubar-error">{error}</div>
    {/if}

    {#if loading && !data}
      <div class="menubar-loading">Loading local usage...</div>
    {:else}
      <section class="section-frame hero-block">
        <span class="corner tl"></span><span class="corner tr"></span>
        <span class="corner bl"></span><span class="corner br"></span>
        <div class="hero-kicker">{rangeCaption(range)}</div>
        {#if isRangeEmpty}
          <div class="hero-value empty-value">No usage</div>
          <div class="hero-caption">{emptyCaption}</div>
        {:else}
          <div class="hero-value">{fmtCompact(totalTokens)}</div>
          <div class="hero-caption">
            {fmtCompact(summary?.input_tokens)} in / {fmtCompact(summary?.output_tokens)} out
          </div>
        {/if}
      </section>

      <section class="section-frame mini-metrics" aria-label="Usage metrics">
        <span class="corner tl"></span><span class="corner tr"></span>
        <span class="corner bl"></span><span class="corner br"></span>
        <div>
          <span>Cost</span>
          <strong title={fmtCost(summary?.total_cost_usd)}>{fmtMenubarCost(summary?.total_cost_usd)}</strong>
        </div>
        <div>
          <span>Cache</span>
          <strong>{fmtPercent(cacheHit)}</strong>
        </div>
        <div>
          <span>Failed</span>
          <strong class:attention={(summary?.failed_tool_calls ?? 0) > 0}>{summary?.failed_tool_calls ?? 0}</strong>
        </div>
      </section>

      <section class="section-frame detail-strip">
        <span class="corner tl"></span><span class="corner tr"></span>
        <span class="corner bl"></span><span class="corner br"></span>
        <div>
          <span>sessions</span>
          <strong>{summary?.sessions ?? 0}</strong>
        </div>
        <div>
          <span>turns</span>
          <strong>{summary?.turns ?? 0}</strong>
        </div>
        <div>
          <span>calls</span>
          <strong>{fmtCompact(summary?.llm_calls)}</strong>
        </div>
        <div>
          <span>cache</span>
          <strong>{fmtCompact(summary?.cache_read_tokens)}</strong>
        </div>
      </section>

      <section class="section-frame trend-block">
        <span class="corner tl"></span><span class="corner tr"></span>
        <span class="corner bl"></span><span class="corner br"></span>
        <div class="section-heading">
          <span>Token trend</span>
          <em>{trendSubtitle()}</em>
        </div>
        {#if trend.length > 0}
          <div
            class="spark-bars"
            class:sparse={trend.length <= 10 && !isHourlyTrend}
            class:hourly={isHourlyTrend}
            role="img"
            aria-label="Token trend"
            on:mouseleave={() => (trendHover = null)}
          >
            {#each trend as point, index}
              <button
                type="button"
                class="trend-bar"
                class:active={hoveredTrend === point}
                class:quiet={totalFor(point) === 0}
                aria-label={`${point.date}: ${fmtCompact(totalFor(point))}`}
                style={barStyle(point)}
                on:pointerenter={(event) => showTrendTooltip(event, index)}
                on:pointermove={(event) => showTrendTooltip(event, index)}
                on:pointerleave={() => (trendHover = null)}
                on:mouseover={(event) => showTrendTooltip(event, index)}
                on:mouseenter={(event) => showTrendTooltip(event, index)}
                on:mousemove={(event) => showTrendTooltip(event, index)}
                on:focus={(event) => showTrendTooltip(event, index)}
                on:blur={() => (trendHover = null)}
              ></button>
            {/each}
            {#if trendHover && hoveredTrend}
              <div class="trend-tooltip" style={`left: ${trendHover.x}px; top: ${trendHover.y}px;`}>
                <div>{isHourlyTrend ? hoveredTrend.bucket_key : hoveredTrend.date}</div>
                <strong>Total {fmtCompact(totalFor(hoveredTrend))}</strong>
                <span>In {fmtCompact(hoveredTrend.input_tokens)} / Out {fmtCompact(hoveredTrend.output_tokens)}</span>
                <span>Cache {fmtCompact(hoveredTrend.cache_read_tokens)} {fmtPercent(cacheRate(hoveredTrend.input_tokens, hoveredTrend.cache_read_tokens))}</span>
                <span>{fmtMenubarCost(hoveredTrend.total_cost_usd)} · {hoveredTrend.failed_tool_calls} failed</span>
              </div>
            {/if}
          </div>
          <div class="spark-axis">
            {#if isHourlyTrend}
              <span>00:00</span>
              <span>06:00</span>
              <span>12:00</span>
              <span>18:00</span>
              <span>24:00</span>
            {:else}
              <span>{shortDate(trend[0])}</span>
              <span>{shortDate(trend[trend.length - 1])}</span>
            {/if}
          </div>
        {:else}
          <div class="empty-line">No usage in this range.</div>
        {/if}
      </section>

      <section class="section-frame source-block">
        <span class="corner tl"></span><span class="corner tr"></span>
        <span class="corner bl"></span><span class="corner br"></span>
        <div class="section-heading">
          <span>Source split</span>
          {#if hiddenSources > 0}
            <em>+{hiddenSources} more</em>
          {/if}
        </div>
        <div class="mini-table source-list">
          <div class="mini-table-head source-row">
            <span>Source</span>
            <span>Tokens</span>
            <span>Failed</span>
          </div>
          {#each sourceUsage.slice(0, 2) as source}
            {@const sourceTokens = source.input_tokens + source.output_tokens}
            <div class="source-row">
              <div>
                <strong>{source.source}</strong>
                <span>{fmtCompact(source.cache_read_tokens)} cache</span>
              </div>
              <div>
                <strong>{fmtCompact(sourceTokens)}</strong>
                <span title={fmtCost(source.total_cost_usd)}>{fmtMenubarCost(source.total_cost_usd)} · {fmtPercent(sourceShare(sourceTokens))}</span>
              </div>
              <div class:attention={source.failed_tool_calls > 0}>
                <strong>{source.failed_tool_calls}</strong>
                <span>failed</span>
              </div>
            </div>
          {:else}
            <div class="empty-line">No sources.</div>
          {/each}
        </div>
      </section>

      <section class="section-frame recent-block">
        <span class="corner tl"></span><span class="corner tr"></span>
        <span class="corner bl"></span><span class="corner br"></span>
        <div class="section-heading">
          <span>Recent runs</span>
          <em>{fmtPercent(failedRate)} failed tools</em>
        </div>
        <div class="mini-table recent-list">
          <div class="mini-table-head recent-row">
            <span>Run</span>
            <span>Usage</span>
          </div>
          {#each recentRuns.slice(0, 2) as run}
            <button type="button" class="recent-row" on:click={() => openRun(run.run_id)}>
              <div>
                <strong>{shortId(run.run_id)}</strong>
                <span>{run.source} / {run.status}</span>
              </div>
              <div>
                <strong>{fmtCompact(run.input_tokens + run.output_tokens)}</strong>
                <span title={fmtCost(run.total_cost_usd)}>{fmtMenubarCost(run.total_cost_usd)}</span>
              </div>
            </button>
          {:else}
            <div class="empty-line">No recent runs.</div>
          {/each}
        </div>
      </section>

      <footer class="menubar-footer">
        <div>
          <span class="dot"></span>
          <span>LOCAL</span>
        </div>
        <button type="button" on:click={openDashboard}>
          <span>OPEN DASHBOARD</span>
          <ExternalLink size={12} strokeWidth={1.7} aria-hidden="true" />
        </button>
      </footer>
    {/if}
  </section>
</main>

<style>
  :global(html.menubar-document),
  :global(body.menubar-document),
  :global(body.menubar-document #app) {
    width: 100%;
    height: 100%;
    overflow: hidden;
  }

  :global(body.menubar-document) {
    margin: 0;
    background: transparent;
  }

  :global(body.menubar-document),
  :global(body.menubar-document *) {
    -webkit-user-drag: none;
    -webkit-user-select: none;
    user-select: none;
  }

  .menubar-root {
    width: 100%;
    height: 100vh;
    min-height: 0;
    overflow: hidden;
    margin: 0 auto;
    padding: 0;
    border-radius: 18px;
    background: transparent;
    color: var(--muted);
  }

  .menubar-shell {
    position: relative;
    overflow: hidden;
    display: flex;
    flex-direction: column;
    height: 100%;
    margin-top: 0;
    padding: 20px 18px 12px;
    border-color: rgba(231, 224, 210, 0.105);
    border-radius: 18px;
    background:
      repeating-linear-gradient(0deg, transparent 0, transparent 23px, rgba(231, 224, 210, 0.034) 24px),
      repeating-linear-gradient(90deg, transparent 0, transparent 23px, rgba(231, 224, 210, 0.03) 24px),
      radial-gradient(circle at 12% 0%, rgba(231, 224, 210, 0.03), transparent 30%),
      linear-gradient(rgba(15, 18, 18, 0.95), rgba(15, 18, 18, 0.95));
    box-shadow:
      0 18px 34px rgba(0, 0, 0, 0.26),
      inset 0 1px 0 rgba(242, 238, 226, 0.035),
      inset 0 0 0 1px rgba(255, 255, 255, 0.008);
  }

  .menubar-shell::before {
    position: absolute;
    inset: 0;
    pointer-events: none;
    background:
      linear-gradient(180deg, rgba(141, 181, 106, 0.055), transparent 16%),
      linear-gradient(90deg, transparent, rgba(141, 181, 106, 0.045), transparent);
    content: "";
    opacity: 0.58;
  }

  .menubar-shell > .corner {
    opacity: 0.72;
  }

  .menubar-shell > .corner.tl {
    top: 16px;
    left: 16px;
  }

  .menubar-shell > .corner.tr {
    top: 16px;
    right: 16px;
  }

  .menubar-shell > .corner.bl {
    bottom: 16px;
    left: 16px;
  }

  .menubar-shell > .corner.br {
    right: 16px;
    bottom: 16px;
  }

  .menubar-shell > :not(.corner) {
    position: relative;
    z-index: 1;
  }

  .menubar-header,
  .section-heading,
  .menubar-footer,
  .range-tabs,
  .mini-metrics,
  .detail-strip,
  .source-row,
  .recent-row {
    display: flex;
    align-items: center;
  }

  .menubar-header {
    justify-content: space-between;
    gap: 10px;
    min-height: 24px;
    padding: 0 14px;
  }

  .menubar-title {
    color: var(--text);
    font-size: 20px;
    line-height: 1;
    font-weight: 420;
  }

  .sync-button,
  .menubar-footer button,
  .range-tabs button {
    border: 1px solid var(--border);
    border-radius: 4px;
    background:
      linear-gradient(180deg, rgba(231, 224, 210, 0.025), transparent),
      rgba(13, 15, 14, 0.74);
    color: var(--muted);
    cursor: pointer;
    transition: border-color 120ms ease, background 120ms ease, color 120ms ease;
  }

  .sync-button {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 24px;
    padding: 0 7px;
    text-transform: uppercase;
  }

  .sync-button:hover,
  .menubar-footer button:hover,
  .range-tabs button:hover {
    border-color: rgba(141, 181, 106, 0.36);
    color: var(--text);
  }

  .sync-button.running {
    color: var(--green);
  }

  .sync-button :global(svg),
  .menubar-footer button :global(svg) {
    flex: 0 0 auto;
  }

  .sync-button :global(.spinning) {
    animation: spin 900ms linear infinite;
  }

  .range-tabs {
    width: 100%;
    margin-top: 8px;
    border: 1px solid var(--border);
    border-radius: 4px;
    overflow: hidden;
    background:
      repeating-linear-gradient(90deg, transparent 0, transparent 23px, rgba(231, 224, 210, 0.014) 24px),
      rgba(8, 10, 9, 0.36);
  }

  .range-tabs button {
    flex: 1;
    height: 20px;
    border: 0;
    border-radius: 0;
    border-right: 1px solid rgba(231, 224, 210, 0.08);
    background: transparent;
    color: var(--dim);
    font-size: 10px;
  }

  .range-tabs button:last-child {
    border-right: 0;
  }

  .range-tabs button.active {
    background:
      linear-gradient(90deg, rgba(141, 181, 106, 0.09), rgba(45, 52, 39, 0.32)),
      rgba(45, 52, 39, 0.28);
    color: var(--text);
    box-shadow: inset 0 0 0 1px rgba(141, 181, 106, 0.1);
  }

  .menubar-error,
  .menubar-loading,
  .empty-line {
    margin-top: 10px;
    color: var(--muted);
  }

  .menubar-error {
    color: var(--red);
  }

  .section-frame {
    position: relative;
    overflow: hidden;
    border: 1px solid var(--border);
    border-radius: 6px;
    background:
      repeating-linear-gradient(0deg, transparent 0, transparent 23px, rgba(231, 224, 210, 0.026) 24px),
      repeating-linear-gradient(90deg, transparent 0, transparent 23px, rgba(231, 224, 210, 0.022) 24px),
      radial-gradient(circle at 18% 0%, rgba(141, 181, 106, 0.035), transparent 34%),
      rgba(11, 13, 12, 0.6);
    box-shadow: inset 0 1px 0 rgba(242, 238, 226, 0.02);
  }

  .section-frame > :not(.corner) {
    position: relative;
    z-index: 1;
  }

  .hero-block {
    margin-top: 8px;
    min-height: 86px;
    padding: 22px 20px 14px 26px;
    background:
      repeating-linear-gradient(0deg, transparent 0, transparent 23px, rgba(231, 224, 210, 0.024) 24px),
      repeating-linear-gradient(90deg, transparent 0, transparent 23px, rgba(231, 224, 210, 0.02) 24px),
      linear-gradient(90deg, rgba(141, 181, 106, 0.065), transparent 58%),
      rgba(11, 13, 12, 0.6);
  }

  .hero-block::after {
    position: absolute;
    z-index: 0;
    top: 0;
    bottom: 0;
    left: -120px;
    width: 108px;
    height: auto;
    pointer-events: none;
    background:
      linear-gradient(90deg, transparent, rgba(141, 181, 106, 0.12), transparent),
      radial-gradient(circle at 72% 58%, rgba(232, 154, 54, 0.28), transparent 0 5px, transparent 9px);
    content: "";
    filter: blur(0.2px);
    opacity: 0;
    transform: skewX(-12deg);
    animation: hero-flow 6.5s cubic-bezier(0.55, 0, 0.2, 1) infinite;
  }

  .hero-kicker,
  .section-heading,
  .detail-strip span,
  .mini-metrics span,
  .source-row span,
  .recent-row span,
  .menubar-footer {
    color: var(--muted);
  }

  .hero-kicker {
    text-transform: uppercase;
    font-size: 9px;
  }

  .hero-value {
    margin-top: 4px;
    color: var(--text);
    font-size: 31px;
    line-height: 1;
    letter-spacing: 0;
  }

  .hero-value.empty-value {
    font-size: 24px;
  }

  .hero-caption {
    margin-top: 5px;
    font-size: 11px;
  }

  .mini-metrics {
    margin-top: 7px;
    padding: 14px 13px 12px;
  }

  .mini-metrics > div {
    position: relative;
    overflow: hidden;
    flex: 1;
    min-width: 0;
    padding: 0 7px;
    border-right: 1px solid rgba(231, 224, 210, 0.085);
    background: transparent;
    text-align: center;
  }

  .mini-metrics > div:last-child {
    border-right: 0;
  }

  .mini-metrics span,
  .detail-strip span {
    display: block;
    font-size: 9px;
    text-transform: uppercase;
  }

  .mini-metrics strong {
    display: block;
    margin-top: 3px;
    color: var(--text);
    font-size: 16px;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  strong.attention,
  .attention strong {
    color: var(--amber);
  }

  .detail-strip {
    display: grid;
    grid-template-columns: repeat(4, minmax(0, 1fr));
    margin-top: 7px;
    padding: 13px 10px 11px;
  }

  .detail-strip > div {
    min-width: 0;
    padding: 0 7px;
    border-right: 1px solid rgba(231, 224, 210, 0.075);
    background: transparent;
    text-align: center;
  }

  .detail-strip > div:last-child {
    border-right: 0;
  }

  .detail-strip strong {
    display: block;
    margin-top: 3px;
    color: var(--text);
    font-size: 12px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .trend-block,
  .source-block,
  .recent-block {
    margin-top: 7px;
    padding: 20px 16px 12px;
  }

  .section-frame > .corner {
    opacity: 0.72;
  }

  .section-frame > .corner.tl {
    top: 10px;
    left: 10px;
  }

  .section-frame > .corner.tr {
    top: 10px;
    right: 10px;
  }

  .section-frame > .corner.bl {
    bottom: 10px;
    left: 10px;
  }

  .section-frame > .corner.br {
    right: 10px;
    bottom: 10px;
  }

  .trend-block {
    overflow: visible;
  }

  .source-block {
    flex: 0 0 145px;
    height: 145px;
  }

  .section-heading {
    justify-content: space-between;
    gap: 8px;
    margin-bottom: 6px;
    color: var(--text);
    font-size: 11px;
    text-transform: uppercase;
  }

  .section-heading em {
    color: var(--dim);
    font-style: normal;
    text-transform: none;
  }

  .spark-bars {
    position: relative;
    overflow: visible;
    height: 44px;
    display: flex;
    align-items: flex-end;
    gap: 3px;
    padding: 6px 2px 2px;
    background:
      linear-gradient(180deg, transparent 0 32%, rgba(231, 224, 210, 0.055) 33% 34%, transparent 35% 64%, rgba(231, 224, 210, 0.055) 65% 66%, transparent 67%),
      repeating-linear-gradient(90deg, transparent 0, transparent 23px, rgba(231, 224, 210, 0.018) 24px),
      transparent;
  }

  .spark-bars.sparse {
    justify-content: space-between;
    gap: 0;
  }

  .trend-bar {
    position: relative;
    z-index: 1;
    flex: 1;
    min-width: 3px;
    border-radius: 3px 3px 0 0;
    background: linear-gradient(
      180deg,
      rgb(141 181 106 / var(--fill-top, 0.72)),
      rgb(141 181 106 / var(--fill-bottom, 0.24))
    );
    border: 1px solid rgb(141 181 106 / var(--border-alpha, 0.58));
    box-shadow: 0 0 12px rgb(141 181 106 / var(--glow-alpha, 0.1));
    cursor: default;
    padding: 0;
  }

  .spark-bars.hourly {
    gap: 2px;
  }

  .spark-bars.hourly .trend-bar {
    min-width: 2px;
    border-radius: 2px 2px 0 0;
  }

  .spark-bars.sparse .trend-bar {
    flex: 0 0 16px;
    max-width: 16px;
  }

  .trend-bar.quiet {
    border-color: rgba(231, 224, 210, 0.12);
    background: rgba(231, 224, 210, 0.11);
  }

  .trend-bar.active,
  .trend-bar:focus-visible {
    border-color: rgba(242, 238, 226, 0.72);
    box-shadow: 0 0 0 1px rgba(242, 238, 226, 0.16), 0 0 16px rgba(141, 181, 106, 0.18);
    outline: none;
  }

  .trend-tooltip {
    position: absolute;
    z-index: 8;
    width: 140px;
    pointer-events: none;
    transform: translate(-50%, calc(-100% - 7px));
    border: 1px solid rgba(231, 224, 210, 0.16);
    border-radius: 4px;
    background:
      repeating-linear-gradient(0deg, transparent 0, transparent 23px, rgba(231, 224, 210, 0.026) 24px),
      rgba(11, 13, 12, 0.96);
    box-shadow: 0 10px 22px rgba(0, 0, 0, 0.34);
    padding: 6px 7px;
    color: var(--muted);
    font-size: 9px;
    line-height: 1.28;
  }

  .trend-tooltip strong,
  .trend-tooltip span {
    display: block;
    margin-top: 2px;
  }

  .trend-tooltip strong {
    color: var(--text);
    font-size: 11px;
  }

  .spark-axis {
    display: flex;
    justify-content: space-between;
    margin-top: 4px;
    color: var(--dim);
    font-size: 10px;
  }

  .source-list,
  .recent-list {
    display: grid;
    gap: 0;
  }

  .source-list {
    height: 91px;
    min-height: 91px;
    overflow: hidden;
  }

  .recent-list {
    min-height: 78px;
  }

  .source-list .empty-line,
  .recent-list .empty-line {
    display: flex;
    align-items: center;
    min-height: 50px;
    margin-top: 0;
    padding: 4px 3px;
  }

  .mini-table {
    border-top: 1px solid rgba(231, 224, 210, 0.08);
    border-bottom: 1px solid rgba(231, 224, 210, 0.08);
    background:
      repeating-linear-gradient(90deg, transparent 0, transparent 23px, rgba(231, 224, 210, 0.014) 24px),
      transparent;
  }

  .mini-table-head {
    min-height: 21px;
    padding: 4px 0;
    color: var(--dim);
    cursor: default;
    font-size: 9px;
    text-transform: uppercase;
  }

  .mini-table-head span {
    color: var(--dim);
    font-weight: 400;
  }

  .source-row {
    display: grid;
    grid-template-columns: minmax(0, 1.25fr) minmax(0, 1fr) 54px;
    gap: 8px;
    padding: 4px 3px;
    border-bottom: 1px solid rgba(231, 224, 210, 0.08);
  }

  .source-row:last-child,
  .recent-row:last-child {
    border-bottom: 0;
  }

  .source-row strong,
  .recent-row strong {
    display: block;
    color: var(--text);
    font-size: 12px;
    font-weight: 650;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .source-row span,
  .recent-row span {
    display: block;
    margin-top: 1px;
    font-size: 9px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .recent-row {
    width: 100%;
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    gap: 10px;
    padding: 4px 3px;
    border: 0;
    border-bottom: 1px solid rgba(231, 224, 210, 0.08);
    background: transparent;
    text-align: left;
    cursor: pointer;
  }

  .recent-row:hover strong {
    color: var(--green);
  }

  .source-row:hover,
  .recent-row:hover {
    background: rgba(231, 224, 210, 0.025);
  }

  .mini-table-head:hover {
    background: transparent;
  }

  .recent-row > div:last-child {
    text-align: right;
  }

  .menubar-footer {
    justify-content: space-between;
    gap: 8px;
    margin-top: 5px;
    padding: 4px 9px 0;
    font-size: 10px;
  }

  .menubar-footer > div {
    flex: 0 0 auto;
    min-width: 0;
    display: inline-flex;
    align-items: center;
    gap: 7px;
    height: 24px;
    padding: 0 9px;
    border: 1px solid rgba(231, 224, 210, 0.075);
    border-radius: 4px;
    background: rgba(8, 10, 9, 0.34);
    color: var(--text);
  }

  .menubar-footer button {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    height: 24px;
    flex: 0 0 auto;
    padding: 0 9px;
    font-size: 9px;
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--green);
  }

  @keyframes hero-flow {
    0%,
    54% {
      opacity: 0;
      transform: translateX(0) skewX(-12deg);
    }
    66% {
      opacity: 0.74;
    }
    100% {
      opacity: 0;
      transform: translateX(600px) skewX(-12deg);
    }
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
</style>
