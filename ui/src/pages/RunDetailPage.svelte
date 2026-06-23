<script lang="ts">
  import TraceWorkspace from "../components/TraceWorkspace.svelte";
  import { fmtCompact, fmtCost, fmtDuration, fmtPercent, runCacheRatio, shortId } from "../lib/format";
  import type { RunDetail, RunSignal } from "../lib/types";

  export let detail: RunDetail;
  export let onBack: () => void;

  type GroupedSignal = RunSignal & { count: number };

  $: groupedSignals = groupSignals(detail.signals);

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

<div class="run-detail-page">
  <section class="panel trace-run-summary">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="trace-run-id">
      <button class="text-button compact" on:click={onBack}>← Back</button>
      <strong>{shortId(detail.run.run_id)}</strong>
      <span>{detail.run.source} / {detail.run.kind} / {detail.run.status}</span>
    </div>
    <div class="trace-run-metric">
      <span>Runtime</span>
      <strong>{fmtDuration(detail.run.duration_ns)}</strong>
    </div>
    <div class="trace-run-metric">
      <span>Tokens</span>
      <strong>{fmtCompact(detail.run.input_tokens + detail.run.output_tokens)}</strong>
    </div>
    <div class="trace-run-metric">
      <span>Cache</span>
      <strong>{fmtPercent(runCacheRatio(detail.run))}</strong>
    </div>
    <div class="trace-run-metric">
      <span>Cost</span>
      <strong>{fmtCost(detail.run.total_cost_usd)}</strong>
    </div>
    <div class="trace-run-metric">
      <span>LLM / Tools</span>
      <strong>{detail.run.llm_call_count} / {detail.run.tool_call_count}</strong>
    </div>
  </section>

  <TraceWorkspace {detail} groupedSignals={groupedSignals} />
</div>
