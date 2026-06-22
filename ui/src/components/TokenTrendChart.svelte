<script lang="ts">
  import { fmtCompact } from "../lib/format";
  import type { UsageBucketSummary } from "../lib/types";

  export let points: UsageBucketSummary[] = [];
  export let grain: "day" | "month" = "day";
  let chartKey = "";
  let hover:
    | {
        index: number;
        x: number;
        y: number;
      }
    | null = null;

  $: ordered = [...points].reverse();
  $: maxValue = Math.max(
    1,
    ...ordered.map((point) => totalFor(point)),
  );
  $: totalTokens = ordered.reduce(
    (sum, point) => sum + totalFor(point),
    0,
  );
  $: singlePoint = ordered.length === 1 ? ordered[0] : null;
  $: hoveredPoint = hover ? ordered[hover.index] : null;
  $: nextChartKey = `${grain}:${ordered.map((point) => point.bucket_key).join("|")}`;
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

  function shouldShowLabel(index: number) {
    if (ordered.length <= 12) return true;
    return index === 0 || index === ordered.length - 1 || (index % 3 === 0 && index < ordered.length - 2);
  }

  function shouldShowValueLabel(index: number) {
    if (ordered.length <= 12) return true;
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

  $: axisLabels = ordered
    .map((point, index) => ({ index, label: labelFor(point.date) }))
    .filter((item) => shouldShowLabel(item.index));

  function leftPercent(index: number) {
    if (ordered.length <= 0) return 50;
    return ((index + 0.5) / ordered.length) * 100;
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
      <p class="chart-note">{fmtCompact(totalTokens)} consumed tokens across {ordered.length} {grain === "month" ? "months" : "days"}.</p>
    </div>
    <div class="legend"><i class="legend-context"></i>Input + output</div>
  </div>

  {#if ordered.length > 1}
    <div class="token-chart-wrap">
      <div class="token-chart-y top">{fmtCompact(maxValue)}</div>
      <div class="token-chart-y bottom">0</div>
      <div class="token-chart-stage">
        {#key chartKey}
          <div class="token-grid-lines" aria-hidden="true"></div>
          <div class="token-bars" style={`--bar-count: ${ordered.length};`} role="img" aria-label="Token consumption trend">
            {#each ordered as point, index (point.bucket_key)}
              {@const total = totalFor(point)}
            <button
              class="token-bar-button"
              class:active={hover?.index === index}
              type="button"
              tabindex="-1"
              aria-label={`${point.date}: ${fmtCompact(total)} tokens`}
              style={`--bar-height: ${barHeight(point)}%;`}
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
        {/key}
        {#if hover && hoveredPoint}
          <div class="token-tooltip" style={`left: ${hover.x}px; top: ${hover.y}px;`}>
            <div class="token-tooltip-date">{hoveredPoint.date}</div>
            <strong>Total {fmtCompact(totalFor(hoveredPoint))}</strong>
            <span>Input {fmtCompact(hoveredPoint.input_tokens)} / Output {fmtCompact(hoveredPoint.output_tokens)}</span>
          </div>
        {/if}
        <div class="token-chart-axis">
          {#each axisLabels as item (ordered[item.index]?.bucket_key ?? item.index)}
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
      <span>Input {fmtCompact(singlePoint.input_tokens)} / Output {fmtCompact(singlePoint.output_tokens)}</span>
    </div>
  {:else}
    <div class="token-empty-state">
      <strong>No token usage</strong>
      <span>0 consumed tokens in this range.</span>
    </div>
  {/if}
</section>
