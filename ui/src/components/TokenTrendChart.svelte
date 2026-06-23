<script lang="ts">
  import { cacheRate, fmtCompact, fmtCost, fmtPercent } from "../lib/format";
  import type { UsageBucketSummary } from "../lib/types";

  export let points: UsageBucketSummary[] = [];
  export let grain: "day" | "month" = "day";

  type DisplayBucket = {
    bucket_key: string;
    date: string;
    startDate: string;
    endDate: string;
    pointCount: number;
    summary: UsageBucketSummary;
  };

  const MAX_VISIBLE_BARS = 72;
  let chartKey = "";
  let hover:
    | {
        index: number;
        x: number;
        y: number;
      }
    | null = null;

  $: ordered = [...points].reverse();
  $: displayBuckets = buildDisplayBuckets(ordered);
  $: maxValue = Math.max(
    1,
    ...displayBuckets.map((bucket) => totalFor(bucket.summary)),
  );
  $: totalTokens = ordered.reduce(
    (sum, point) => sum + totalFor(point),
    0,
  );
  $: totalBucketCount = ordered.reduce(
    (sum, point) => sum + (point.bucket_count ?? 1),
    0,
  );
  $: singlePoint = ordered.length === 1 ? ordered[0] : null;
  $: hoveredBucket = hover ? displayBuckets[hover.index] : null;
  $: nextChartKey = `${grain}:${displayBuckets.map((bucket) => bucket.bucket_key).join("|")}`;
  $: if (nextChartKey !== chartKey) {
    chartKey = nextChartKey;
    hover = null;
  }
  function totalFor(point: UsageBucketSummary) {
    return point.input_tokens + point.output_tokens;
  }

  function labelFor(value: string) {
    return grain === "month" ? value : value.slice(5);
  }

  function rangeStart(value: string) {
    return value.split(" - ")[0] ?? value;
  }

  function buildDisplayBuckets(source: UsageBucketSummary[]): DisplayBucket[] {
    if (source.length <= MAX_VISIBLE_BARS) {
      return source.map((point) => displayBucketFromPoints([point]));
    }

    const groupSize = Math.ceil(source.length / MAX_VISIBLE_BARS);
    const buckets: DisplayBucket[] = [];
    for (let index = 0; index < source.length; index += groupSize) {
      buckets.push(displayBucketFromPoints(source.slice(index, index + groupSize)));
    }
    return buckets;
  }

  function displayBucketFromPoints(source: UsageBucketSummary[]): DisplayBucket {
    const summary = aggregatePoints(source);
    const first = source[0]!;
    const last = source[source.length - 1]!;
    return {
      bucket_key: source.length === 1 ? first.bucket_key : `${first.bucket_key}..${last.bucket_key}`,
      date: source.length === 1 ? first.date : `${first.date} - ${last.date}`,
      startDate: rangeStart(first.date),
      endDate: last.date,
      pointCount: source.length,
      summary,
    };
  }

  function aggregatePoints(source: UsageBucketSummary[]): UsageBucketSummary {
    const first = source[0]!;
    const last = source[source.length - 1]!;
    return source.reduce(
      (summary, point) => ({
        bucket_key: summary.bucket_key,
        date: summary.date,
        bucket_count: summary.bucket_count + (point.bucket_count ?? 1),
        sessions: summary.sessions + point.sessions,
        runs: summary.runs + point.runs,
        turns: summary.turns + point.turns,
        llm_calls: summary.llm_calls + point.llm_calls,
        tool_calls: summary.tool_calls + point.tool_calls,
        failed_tool_calls: summary.failed_tool_calls + point.failed_tool_calls,
        input_tokens: summary.input_tokens + point.input_tokens,
        output_tokens: summary.output_tokens + point.output_tokens,
        cache_read_tokens: summary.cache_read_tokens + point.cache_read_tokens,
        cache_write_tokens: summary.cache_write_tokens + point.cache_write_tokens,
        total_cost_usd: summary.total_cost_usd + point.total_cost_usd,
      }),
      {
        bucket_key: source.length === 1 ? first.bucket_key : `${first.bucket_key}..${last.bucket_key}`,
        date: source.length === 1 ? first.date : `${first.date} - ${last.date}`,
        bucket_count: 0,
        sessions: 0,
        runs: 0,
        turns: 0,
        llm_calls: 0,
        tool_calls: 0,
        failed_tool_calls: 0,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        total_cost_usd: 0,
      },
    );
  }

  function shouldShowLabel(index: number) {
    const maxLabels = grain === "month" ? 12 : 8;
    if (displayBuckets.length <= maxLabels) return true;
    if (index === 0 || index === displayBuckets.length - 1) return true;

    const interval = Math.ceil((displayBuckets.length - 1) / (maxLabels - 1));
    return index % interval === 0;
  }

  function shouldShowValueLabel(index: number) {
    if (displayBuckets.length <= 12) return true;
    return hover?.index === index;
  }

  function showTooltip(event: MouseEvent, index: number) {
    const stage = event.currentTarget instanceof HTMLElement
      ? event.currentTarget.closest(".token-chart-stage")
      : null;
    const rect = stage?.getBoundingClientRect();
    if (!rect) return;

    const localX = event.clientX - rect.left;
    const localY = event.clientY - rect.top;
    hover = {
      index,
      x: Math.min(Math.max(localX, 92), Math.max(92, rect.width - 92)),
      y: Math.min(Math.max(localY, 46), rect.height - 8),
    };
  }

  $: axisLabels = displayBuckets
    .map((bucket, index) => ({ index, label: labelFor(bucket.startDate) }))
    .filter((item) => shouldShowLabel(item.index));

  function leftPercent(index: number) {
    if (displayBuckets.length <= 0) return 50;
    return ((index + 0.5) / displayBuckets.length) * 100;
  }

  function barHeight(point: UsageBucketSummary) {
    return Math.max(2, (totalFor(point) / maxValue) * 88);
  }
</script>

<section
  class="panel token-trend-panel"
  class:sparse={ordered.length > 1 && ordered.length <= 4}
  class:compact={ordered.length > 1 && ordered.length <= 7}
  class:single={ordered.length === 1}
  class:empty={ordered.length === 0}
>
  <span class="corner tl"></span><span class="corner tr"></span>
  <span class="corner bl"></span><span class="corner br"></span>

  <div class="chart-title-row">
    <div>
      <div class="panel-title">{grain === "month" ? "MONTHLY TOKEN TREND" : "DAILY TOKEN TREND"}</div>
      <p class="chart-note">{fmtCompact(totalTokens)} consumed tokens across {totalBucketCount} {grain === "month" ? "months" : "days"}.</p>
    </div>
    <div class="legend"><i class="legend-context"></i>Input + output</div>
  </div>

  {#if ordered.length > 1}
    <div class="token-chart-wrap">
      <div class="token-chart-y top">{fmtCompact(maxValue)}</div>
      <div class="token-chart-y bottom">0</div>
      <div class="token-chart-stage">
        <div class="token-grid-lines" aria-hidden="true"></div>
        <div class="token-bars" style={`--bar-count: ${displayBuckets.length};`} role="img" aria-label="Token consumption trend">
          {#each displayBuckets as bucket, index (bucket.bucket_key)}
            {@const total = totalFor(bucket.summary)}
            <button
              class="token-bar-button"
              class:active={hover?.index === index}
              type="button"
              tabindex="-1"
              aria-label={`${bucket.date}: ${fmtCompact(total)} tokens`}
              style={`--bar-height: ${barHeight(bucket.summary)}%;`}
              on:mousemove={(event) => showTooltip(event, index)}
              on:mouseenter={(event) => showTooltip(event, index)}
              on:mouseleave={() => (hover = null)}
            >
              {#if shouldShowValueLabel(index)}
                <span class="token-bar-label" class:active={hover?.index === index}>{fmtCompact(total)}</span>
              {/if}
              <span class="token-bar-fill"></span>
            </button>
          {/each}
        </div>
        {#if hover && hoveredBucket}
          <div class="token-tooltip" style={`left: ${hover.x}px; top: ${hover.y}px;`}>
            <div class="token-tooltip-date">{hoveredBucket.date}</div>
            <strong>Total {fmtCompact(totalFor(hoveredBucket.summary))}</strong>
            <span>Input {fmtCompact(hoveredBucket.summary.input_tokens)} / Output {fmtCompact(hoveredBucket.summary.output_tokens)}</span>
            <span>{hoveredBucket.summary.sessions} sessions / {hoveredBucket.summary.turns} turns</span>
            <span>Cache {fmtCompact(hoveredBucket.summary.cache_read_tokens)} {fmtPercent(cacheRate(hoveredBucket.summary.input_tokens, hoveredBucket.summary.cache_read_tokens))}</span>
            <span>{fmtCost(hoveredBucket.summary.total_cost_usd)} · {hoveredBucket.summary.failed_tool_calls} failed tools</span>
          </div>
        {/if}
        <div class="token-chart-axis">
          {#each axisLabels as item (displayBuckets[item.index]?.bucket_key ?? item.index)}
            <span style={`left: ${leftPercent(item.index)}%`}>{item.label}</span>
          {/each}
        </div>
      </div>
    </div>
  {:else if singlePoint}
    <div class="token-single-state">
      <div>
        <span>{singlePoint.date}</span>
        <strong>{fmtCompact(totalFor(singlePoint))} tokens</strong>
      </div>
      <div class="token-single-meta">
        <span>Input {fmtCompact(singlePoint.input_tokens)} / Output {fmtCompact(singlePoint.output_tokens)}</span>
        <span>Cache {fmtCompact(singlePoint.cache_read_tokens)} {fmtPercent(cacheRate(singlePoint.input_tokens, singlePoint.cache_read_tokens))}</span>
        <span>{fmtCost(singlePoint.total_cost_usd)} · {singlePoint.failed_tool_calls} failed tools</span>
      </div>
    </div>
  {:else}
    <div class="token-empty-state">
      <strong>No token usage</strong>
      <span>0 consumed tokens in this range.</span>
    </div>
  {/if}
</section>
