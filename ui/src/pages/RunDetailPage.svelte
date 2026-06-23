<script lang="ts">
  import MetricCard from "../components/MetricCard.svelte";
  import { fmtCompact, fmtDuration, fmtPercent, runCacheRatio, shortId } from "../lib/format";
  import type { RunDetail, RunSignal } from "../lib/types";

  export let detail: RunDetail;
  export let onBack: () => void;

  type GroupedSignal = RunSignal & { count: number };

  $: groupedSignals = groupSignals(detail.signals);
  $: signalEventDetail = detail.signals.length > groupedSignals.length ? `${detail.signals.length} events` : "";

  function groupSignals(signals: RunSignal[]): GroupedSignal[] {
    const grouped = new Map<string, GroupedSignal>();
    for (const signal of signals) {
      const key = [signal.signal_type, signal.severity, signal.title, signal.suggestion ?? ""].join("\u0000");
      const current = grouped.get(key);
      if (current) {
        current.count += 1;
      } else {
        grouped.set(key, { ...signal, count: 1 });
      }
    }
    return [...grouped.values()];
  }
</script>

<section class="panel run-hero">
  <div>
    <button class="text-button" on:click={onBack}>← Back</button>
    <div class="hero-title">{shortId(detail.run.run_id)}</div>
    <p>{detail.run.source} / {detail.run.kind} / {detail.run.status}</p>
  </div>
  <div class="health-stat"><MetricCard label="Signals" value={groupedSignals.length} detail={signalEventDetail} /></div>
  <div class="health-stat"><MetricCard label="Runtime" value={fmtDuration(detail.run.duration_ns)} /></div>
  <div class="health-stat"><MetricCard label="Cache hit" value={fmtPercent(runCacheRatio(detail.run))} /></div>
  <div class="health-stat"><MetricCard label="Tokens" value={fmtCompact(detail.run.input_tokens + detail.run.output_tokens)} /></div>
</section>

<section class="content-grid">
  <section class="panel">
    <div class="panel-title">Run Signals</div>
    {#if groupedSignals.length}
      {#each groupedSignals as signal}
        <div class="signal-card">
          <div class="signal-card-head">
            <strong>{signal.title}</strong>
            <span class:high={signal.severity === "high"} class:medium={signal.severity === "medium"}>
              {signal.severity}{#if signal.count > 1} ×{signal.count}{/if}
            </span>
          </div>
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
      <thead><tr><th>Model</th><th>Runtime</th><th>Input</th><th>Output</th><th>Cache</th><th>Context</th></tr></thead>
      <tbody>
        {#each detail.llm_calls as call}
          <tr>
            <td>{call.model ?? call.operation ?? "model"}</td>
            <td>{fmtDuration(call.duration_ns)}</td>
            <td>{fmtCompact(call.input_tokens)}</td>
            <td>{fmtCompact(call.output_tokens)}</td>
            <td>{fmtPercent(call.cache_ratio)}</td>
            <td>
              {#if call.context_window_percent == null}
                <span class="muted">not captured</span>
              {:else}
                {fmtPercent(call.context_window_percent)}
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  </section>
</section>

<section class="panel">
  <div class="panel-title">Timeline</div>
  <table>
    <thead><tr><th>#</th><th>Type</th><th>Name</th><th>Status</th><th>Runtime</th><th>Tokens</th></tr></thead>
    <tbody>
      {#each detail.timeline as step, index}
        <tr>
          <td>{index + 1}</td>
          <td>{step.step_type}</td>
          <td>{step.name}</td>
          <td>{step.status}</td>
          <td>{fmtDuration(step.duration_ns)}</td>
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
