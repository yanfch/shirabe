<script lang="ts">
  import ChartCard from "../components/ChartCard.svelte";
  import MetricCard from "../components/MetricCard.svelte";
  import { fmtCompact, fmtPercent, runCacheRatio, shortId } from "../lib/format";
  import type { RunDetail } from "../lib/types";

  export let detail: RunDetail;
  export let onOverview: () => void;
</script>

<section class="panel run-hero">
  <div>
    <button class="text-button" on:click={onOverview}>← Overview</button>
    <div class="hero-title">{shortId(detail.run.run_id)}</div>
    <p>{detail.run.source} / {detail.run.kind} / {detail.run.status}</p>
  </div>
  <div class="health-stat"><MetricCard label="Signals" value={detail.signals.length} /></div>
  <div class="health-stat"><ChartCard label="Cache hit" value={fmtPercent(runCacheRatio(detail.run))} ratio={runCacheRatio(detail.run)} /></div>
  <div class="health-stat"><MetricCard label="Tokens" value={fmtCompact(detail.run.input_tokens + detail.run.output_tokens)} /></div>
</section>

<section class="content-grid">
  <section class="panel">
    <div class="panel-title">Run Signals</div>
    {#if detail.signals.length}
      {#each detail.signals as signal}
        <div class="signal-card">
          <strong>{signal.title}</strong>
          <span class:high={signal.severity === "high"} class:medium={signal.severity === "medium"}>{signal.severity}</span>
          <p>{signal.suggestion}</p>
        </div>
      {/each}
    {:else}
      <p class="empty">No signals for this run.</p>
    {/if}
  </section>

  <section class="panel">
    <div class="panel-title">LLM Calls</div>
    <table>
      <thead><tr><th>Model</th><th>Input</th><th>Output</th><th>Cache</th><th>Context</th></tr></thead>
      <tbody>
        {#each detail.llm_calls as call}
          <tr>
            <td>{call.model ?? call.operation ?? "model"}</td>
            <td>{fmtCompact(call.input_tokens)}</td>
            <td>{fmtCompact(call.output_tokens)}</td>
            <td>{fmtPercent(call.cache_ratio)}</td>
            <td>{fmtPercent(call.context_window_percent)}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  </section>
</section>

<section class="panel">
  <div class="panel-title">Timeline</div>
  <table>
    <thead><tr><th>#</th><th>Type</th><th>Name</th><th>Status</th><th>Tokens</th></tr></thead>
    <tbody>
      {#each detail.timeline as step}
        <tr>
          <td>{step.order_index ?? "-"}</td>
          <td>{step.step_type}</td>
          <td>{step.name}</td>
          <td>{step.status}</td>
          <td>{fmtCompact(step.input_tokens + step.output_tokens)}</td>
        </tr>
      {/each}
    </tbody>
  </table>
</section>

<section class="panel">
  <div class="panel-title">Tool Calls</div>
  <table>
    <thead><tr><th>Tool</th><th>Status</th><th>Error</th></tr></thead>
    <tbody>
      {#each detail.tool_calls as tool}
        <tr>
          <td>{tool.tool_name}</td>
          <td>{tool.status}</td>
          <td>{tool.error_type ?? "-"}</td>
        </tr>
      {/each}
    </tbody>
  </table>
</section>
