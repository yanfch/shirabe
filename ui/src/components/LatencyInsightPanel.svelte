<script lang="ts">
  import type {
    LatencyBucketSummary,
    LatencyPanel,
    LatencySummary,
    UsageFilters,
  } from "../lib/types";

  export let latency: LatencySummary | null = null;
  export let panel: LatencyPanel | null = null;
  export let buckets: LatencyBucketSummary[] = [];
  export let filters: UsageFilters;

  const maxVisibleBuckets = 36;
  let hover:
    | {
        bucket: LatencyBucketSummary;
        x: number;
        y: number;
      }
    | null = null;

  $: orderedBuckets = [...buckets].reverse();
  $: visibleBuckets = compactLatencyBuckets(orderedBuckets);
  $: isHourly = visibleBuckets.length > 0 && visibleBuckets.every((bucket) => /^\d{2}:00$/.test(bucket.bucket_key));
  $: maxDelay = Math.max(
    1,
    ...visibleBuckets.map((bucket) => bucket.p90_response_delay_ns ?? bucket.p50_response_delay_ns ?? 0),
  );
  $: bestWindow = panel?.best_windows[0] ?? null;
  $: slowWindow = panel?.slow_windows[0] ?? null;
  $: providerPatterns = panel?.provider_patterns ?? [];
  $: slowestProvider = providerPatterns
    .filter((provider) => provider.slow_p50_response_delay_ns != null)
    .sort((left, right) => (right.slow_p50_response_delay_ns ?? 0) - (left.slow_p50_response_delay_ns ?? 0))[0] ?? null;
  $: activeFilterLabel = filterLabel(filters);
  $: hoveredBucket = hover?.bucket ?? null;
  $: latencyAxisLabels = isHourly
    ? [
        { key: "00:00", label: "00:00", left: 0 },
        { key: "06:00", label: "06:00", left: 25 },
        { key: "12:00", label: "12:00", left: 50 },
        { key: "18:00", label: "18:00", left: 75 },
        { key: "24:00", label: "24:00", left: 100 },
      ]
    : visibleBuckets
        .map((bucket, index) => ({
          key: bucket.bucket_key,
          label: bucketLabel(bucket),
          left: latencyLeftPercent(index),
          index,
        }))
        .filter((item) => shouldShowLatencyLabel(item.index));

  function compactLatencyBuckets(source: LatencyBucketSummary[]) {
    if (source.length <= maxVisibleBuckets) return source;
    const groupSize = Math.ceil(source.length / maxVisibleBuckets);
    const compacted: LatencyBucketSummary[] = [];
    for (let index = 0; index < source.length; index += groupSize) {
      compacted.push(aggregateLatencyBuckets(source.slice(index, index + groupSize)));
    }
    return compacted;
  }

  function aggregateLatencyBuckets(group: LatencyBucketSummary[]): LatencyBucketSummary {
    const first = group[0]!;
    const last = group[group.length - 1]!;
    const weightedCalls = group.reduce((sum, bucket) => sum + bucket.good_calls, 0);
    const weighted = (getter: (bucket: LatencyBucketSummary) => number | null) => {
      if (weightedCalls <= 0) return null;
      const value = group.reduce((sum, bucket) => {
        const next = getter(bucket);
        return next == null ? sum : sum + next * bucket.good_calls;
      }, 0);
      return value / weightedCalls;
    };
    return {
      bucket_key: group.length === 1 ? first.bucket_key : `${first.bucket_key}..${last.bucket_key}`,
      date: group.length === 1 ? first.date : `${first.date} - ${last.date}`,
      bucket_count: group.reduce((sum, bucket) => sum + bucket.bucket_count, 0),
      good_calls: weightedCalls,
      p50_response_delay_ns: nullableRound(weighted((bucket) => bucket.p50_response_delay_ns)),
      p90_response_delay_ns: nullableRound(weighted((bucket) => bucket.p90_response_delay_ns)),
      avg_observed_output_tps: weighted((bucket) => bucket.avg_observed_output_tps),
      p50_observed_output_tps: weighted((bucket) => bucket.p50_observed_output_tps),
    };
  }

  function nullableRound(value: number | null) {
    return value == null ? null : Math.round(value);
  }

  function filterLabel(value: UsageFilters) {
    const parts = [];
    if (value.source) parts.push(`source ${value.source}`);
    if (value.model) parts.push(`model ${value.model}`);
    return parts.length > 0 ? parts.join(" / ") : "all sources and models";
  }

  function fmtLatency(value: number | null | undefined) {
    if (value == null) return "-";
    if (value < 1_000_000_000) return "<1s";
    const seconds = value / 1_000_000_000;
    if (seconds < 100) return `${seconds.toFixed(1)}s`;
    const minutes = Math.floor(seconds / 60);
    const remaining = Math.round(seconds % 60);
    return remaining > 0 ? `${minutes}m ${remaining}s` : `${minutes}m`;
  }

  function fmtTps(value: number | null | undefined) {
    if (value == null || !Number.isFinite(value)) return "-";
    return `${value.toFixed(value >= 10 ? 1 : 2)}/s`;
  }

  function fmtCoverage(value: LatencySummary | null) {
    if (!value || value.llm_calls <= 0) return "-";
    return `${Math.round((value.good_calls / value.llm_calls) * 100)}%`;
  }

  function hourNumber(value: string | null | undefined) {
    if (!value) return null;
    const hour = Number.parseInt(value.slice(0, 2), 10);
    return Number.isFinite(hour) ? hour : null;
  }

  function fmtHourLabel(value: string | null | undefined) {
    const hour = hourNumber(value);
    if (hour == null) return "--";
    const period = hour >= 12 ? "PM" : "AM";
    const hour12 = hour % 12 || 12;
    return `${hour12}${period}`;
  }

  function fmtWindowLabel(value: string | null | undefined) {
    if (!value) return "-";
    const [startRaw, endRaw] = value.split("-");
    const start = Number.parseInt(startRaw, 10);
    const end = Number.parseInt(endRaw, 10);
    if (!Number.isFinite(start) || !Number.isFinite(end)) return value;
    const startPeriod = start >= 12 ? "PM" : "AM";
    const endPeriod = end % 24 >= 12 ? "PM" : "AM";
    const start12 = start % 12 || 12;
    const end12 = end % 12 || 12;
    return startPeriod === endPeriod
      ? `${start12}-${end12}${endPeriod}`
      : `${start12}${startPeriod}-${end12}${endPeriod}`;
  }

  function bucketLabel(bucket: LatencyBucketSummary) {
    if (/^\d{2}:00$/.test(bucket.bucket_key)) return bucket.bucket_key;
    if (bucket.bucket_key.includes("..")) return bucket.bucket_key.split("..").map(shortDate).join("-");
    return shortDate(bucket.bucket_key);
  }

  function shortDate(value: string) {
    if (/^\d{4}-\d{2}$/.test(value)) return value.slice(2);
    if (/^\d{4}-\d{2}-\d{2}$/.test(value)) return value.slice(5);
    return value;
  }

  function barStyle(bucket: LatencyBucketSummary) {
    const delay = bucket.p90_response_delay_ns ?? bucket.p50_response_delay_ns ?? 0;
    const height = delay <= 0 ? 2 : Math.max(4, (delay / maxDelay) * 100);
    const intensity = delay <= 0 ? 0 : Math.max(0.18, Math.min(1, delay / maxDelay));
    return [
      `--latency-height: ${height.toFixed(2)}%`,
      `--latency-top: ${(0.24 + intensity * 0.54).toFixed(3)}`,
      `--latency-bottom: ${(0.09 + intensity * 0.22).toFixed(3)}`,
      `--latency-border: ${(0.2 + intensity * 0.38).toFixed(3)}`,
      `--latency-glow: ${(0.03 + intensity * 0.12).toFixed(3)}`,
    ].join("; ");
  }

  function latencyLeftPercent(index: number) {
    if (visibleBuckets.length <= 0) return 50;
    return ((index + 0.5) / visibleBuckets.length) * 100;
  }

  function shouldShowLatencyLabel(index: number) {
    const maxLabels = 8;
    if (visibleBuckets.length <= maxLabels) return true;
    if (index === 0 || index === visibleBuckets.length - 1) return true;

    const interval = Math.ceil((visibleBuckets.length - 1) / (maxLabels - 1));
    return index % interval === 0;
  }

  function showTooltip(event: MouseEvent | FocusEvent, bucket: LatencyBucketSummary) {
    const stage = event.currentTarget instanceof HTMLElement
      ? event.currentTarget.closest(".latency-chart-stage")
      : null;
    const rect = stage?.getBoundingClientRect();
    if (!rect) return;
    const clientX = "clientX" in event ? event.clientX : rect.left + rect.width / 2;
    const clientY = "clientY" in event ? event.clientY : rect.top + rect.height / 2;
    hover = {
      bucket,
      x: Math.min(Math.max(clientX - rect.left, 96), Math.max(96, rect.width - 96)),
      y: Math.min(Math.max(clientY - rect.top, 48), rect.height - 12),
    };
  }
</script>

<section class="latency-dashboard-grid">
  <section class="panel latency-insight-panel latency-chart-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>

    <div class="latency-head">
      <div>
        <div class="panel-title">AI LATENCY TREND</div>
        <p>Observed model response delay for {activeFilterLabel}.</p>
      </div>
      <div class="latency-status" class:attention={panel?.status === "slower" || panel?.status === "very_slow"}>
        {panel?.status === "pattern" ? "PATTERN" : (panel?.status ?? "unknown").toUpperCase().replaceAll("_", " ")}
      </div>
    </div>

    <div class="latency-chart-wrap">
      <div class="latency-chart-y top">{fmtLatency(maxDelay)}</div>
      <div class="latency-chart-y bottom">0</div>
      <div class="latency-chart-stage">
        <div class="latency-grid-lines" aria-hidden="true"></div>
        <div
          class="latency-chart"
          class:hourly={isHourly}
          style={`--latency-count: ${Math.max(1, visibleBuckets.length)};`}
          role="img"
          aria-label="Latency trend"
          on:mouseleave={() => (hover = null)}
        >
          {#each visibleBuckets as bucket}
            <button
              type="button"
              class="latency-chart-column"
              aria-label={`${bucketLabel(bucket)} p50 ${fmtLatency(bucket.p50_response_delay_ns)} p90 ${fmtLatency(bucket.p90_response_delay_ns)} ${bucket.good_calls} calls`}
              on:mouseenter={(event) => showTooltip(event, bucket)}
              on:mousemove={(event) => showTooltip(event, bucket)}
              on:focus={(event) => showTooltip(event, bucket)}
              on:blur={() => (hover = null)}
            >
              <span class:empty={bucket.good_calls === 0} style={barStyle(bucket)}></span>
            </button>
          {:else}
            <div class="latency-empty-state">No latency samples for this filter.</div>
          {/each}
        </div>
        {#if hoveredBucket}
          <div class="latency-tooltip" style={`left: ${hover?.x ?? 0}px; top: ${hover?.y ?? 0}px;`}>
            <strong>{bucketLabel(hoveredBucket)}</strong>
            <span>P50 {fmtLatency(hoveredBucket.p50_response_delay_ns)}</span>
            <span>P90 {fmtLatency(hoveredBucket.p90_response_delay_ns)}</span>
            <span>TPS {fmtTps(hoveredBucket.p50_observed_output_tps)}</span>
            <span>{hoveredBucket.good_calls} good calls</span>
          </div>
        {/if}
        <div class="latency-chart-axis">
          {#each latencyAxisLabels as item (item.key)}
            <span style={`left: ${item.left}%`}>{item.label}</span>
          {/each}
        </div>
      </div>
    </div>
  </section>

  <section class="panel latency-insight-panel latency-detail-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>

    <div class="latency-head compact">
      <div>
        <div class="panel-title">AI LATENCY DETAIL</div>
        <p>{activeFilterLabel}</p>
      </div>
      <div class="latency-status" class:attention={panel?.status === "slower" || panel?.status === "very_slow"}>
        {panel?.mode === "today" ? "TODAY" : "PATTERN"}
      </div>
    </div>

    <div class="latency-summary-grid">
      <div>
        <span>P50</span>
        <strong>{fmtLatency(latency?.p50_response_delay_ns)}</strong>
      </div>
      <div>
        <span>P90</span>
        <strong class:attention={(latency?.p90_response_delay_ns ?? 0) >= 30_000_000_000}>{fmtLatency(latency?.p90_response_delay_ns)}</strong>
      </div>
      <div>
        <span>TPS</span>
        <strong>{fmtTps(latency?.p50_observed_output_tps)}</strong>
      </div>
      <div>
        <span>Samples</span>
        <strong>{latency?.good_calls ?? 0}</strong>
      </div>
      <div>
        <span>Coverage</span>
        <strong>{fmtCoverage(latency)}</strong>
      </div>
    </div>

    <div class="latency-section-title">
      <span>Pattern</span>
      <em>{panel?.mode === "today" ? "today" : "range"}</em>
    </div>

      {#if panel?.mode === "today"}
        <div class="latency-pattern-cards">
          <div>
            <span>Current hour</span>
            <strong>{fmtLatency(panel.current?.p50_response_delay_ns ?? panel.baseline.p50_response_delay_ns)}</strong>
            <em>{panel.current?.good_calls ? `${panel.current.good_calls} calls` : "no calls this hour"}</em>
          </div>
          <div>
            <span>Slowest now</span>
            <strong>{panel.current?.slowest_provider?.provider ?? "-"}</strong>
            <em>{fmtLatency(panel.current?.slowest_provider?.p50_response_delay_ns)}</em>
          </div>
        </div>
      {:else}
        <div class="latency-pattern-cards">
          <div>
            <span>Best window</span>
            <strong>{bestWindow ? fmtWindowLabel(bestWindow.label) : "-"}</strong>
            <em>{fmtLatency(bestWindow?.p50_response_delay_ns)} · {bestWindow?.good_calls ?? 0} calls</em>
          </div>
          <div>
            <span>Slow window</span>
            <strong class="attention">{slowWindow ? fmtWindowLabel(slowWindow.label) : "-"}</strong>
            <em>{fmtLatency(slowWindow?.p50_response_delay_ns)} · {slowWindow?.good_calls ?? 0} calls</em>
          </div>
        </div>
        <div class="latency-mini-list">
          <span>Provider patterns</span>
          <div class="latency-mini-head">
            <strong></strong>
            <em>Best</em>
            <small>Slow</small>
          </div>
          {#each providerPatterns.slice(0, 4) as provider}
            <div>
              <strong>{provider.provider}</strong>
              <em>{fmtHourLabel(provider.best_hour)} {fmtLatency(provider.best_p50_response_delay_ns)}</em>
              <small class="attention">{fmtHourLabel(provider.slow_hour)} {fmtLatency(provider.slow_p50_response_delay_ns)}</small>
            </div>
          {:else}
            <div class="muted">No provider pattern yet.</div>
          {/each}
        </div>
      {/if}

  </section>
</section>
