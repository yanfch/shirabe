<script lang="ts">
  import { fmtCompact, fmtCost, fmtDuration, fmtPercent } from "../lib/format";
  import type { LlmCall, RunDetail, RunSignal, RunStep, ToolCall } from "../lib/types";

  export let detail: RunDetail;
  export let groupedSignals: RunSignal[] = [];

  type TraceFilter = "all" | "llm" | "tool" | "failed" | "slow" | "high-token";
  type SpanKind = "group" | "llm" | "tool" | "other";
  type TraceSpan = RunStep & {
    display_name: string;
    duration_value: number;
    end_ns: number;
    kind: SpanKind;
    level: number;
    offset_percent: number;
    width_percent: number;
    token_total: number;
    timeline_index: number;
  };
  type SignalSummary = {
    title: string;
    severity: string;
    count: number;
  };

  const filters: { value: TraceFilter; label: string }[] = [
    { value: "all", label: "All" },
    { value: "llm", label: "LLM" },
    { value: "tool", label: "Tools" },
    { value: "failed", label: "Failed" },
    { value: "slow", label: "Slow" },
    { value: "high-token", label: "High token" },
  ];

  let activeFilter: TraceFilter = "all";
  let selectedStepId: string | null = null;

  $: spans = buildTraceSpans(detail.timeline, detail.run.started_at_ns, detail.run.ended_at_ns);
  $: runStartNs = getRunStart(spans, detail.run.started_at_ns);
  $: runEndNs = getRunEnd(spans, detail.run.ended_at_ns ?? detail.run.started_at_ns);
  $: runRangeNs = Math.max(1, runEndNs - runStartNs);
  $: ticks = buildTicks(runRangeNs);
  $: visibleSpans = spans.filter((span) => matchesFilter(span, activeFilter));
  $: selectedCandidate = selectedStepId ? spans.find((span) => span.step_id === selectedStepId) : null;
  $: selectedSpan =
    selectedCandidate && matchesFilter(selectedCandidate, activeFilter)
      ? selectedCandidate
      : defaultSelectedSpan(visibleSpans) ?? (activeFilter === "all" ? defaultSelectedSpan(spans) : null);
  $: modelRows = summarizeModels(detail.llm_calls);
  $: toolRows = summarizeTools(detail.tool_calls);
  $: signalRows = summarizeSignals(groupedSignals);

  function buildTraceSpans(steps: RunStep[], fallbackStartNs: number, fallbackEndNs: number | null): TraceSpan[] {
    const sorted = [...steps].sort((a, b) => {
      const timeDelta = a.started_at_ns - b.started_at_ns;
      if (timeDelta !== 0) return timeDelta;
      return (a.order_index ?? Number.MAX_SAFE_INTEGER) - (b.order_index ?? Number.MAX_SAFE_INTEGER);
    });

    const startNs = Math.min(fallbackStartNs, ...sorted.map((step) => step.started_at_ns));
    const endNs = Math.max(
      fallbackEndNs ?? fallbackStartNs,
      ...sorted.map((step) => getStepEndNs(step)),
    );
    const rangeNs = Math.max(1, endNs - startNs);
    let leafLevel = 1;

    return sorted.map((step, index) => {
      const structuralLevel = getStructuralLevel(step);
      const kind = getSpanKind(step);
      const level = structuralLevel ?? leafLevel;
      if (structuralLevel != null) {
        leafLevel = Math.min(structuralLevel + 1, 4);
      }
      const stepEndNs = getStepEndNs(step);
      const durationValue = Math.max(0, step.duration_ns ?? stepEndNs - step.started_at_ns);
      return {
        ...step,
        display_name: displayName(step),
        duration_value: durationValue,
        end_ns: stepEndNs,
        kind,
        level,
        offset_percent: clamp(((step.started_at_ns - startNs) / rangeNs) * 100, 0, 100),
        width_percent: clamp((Math.max(durationValue, 1) / rangeNs) * 100, 0.18, 100),
        token_total: step.input_tokens + step.output_tokens,
        timeline_index: index,
      };
    });
  }

  function getStepEndNs(step: RunStep) {
    if (step.ended_at_ns != null && step.ended_at_ns >= step.started_at_ns) return step.ended_at_ns;
    if (step.duration_ns != null && step.duration_ns > 0) return step.started_at_ns + step.duration_ns;
    return step.started_at_ns;
  }

  function getRunStart(traceSpans: TraceSpan[], fallback: number) {
    return Math.min(fallback, ...traceSpans.map((span) => span.started_at_ns));
  }

  function getRunEnd(traceSpans: TraceSpan[], fallback: number) {
    return Math.max(fallback, ...traceSpans.map((span) => span.end_ns));
  }

  function getSpanKind(step: RunStep): SpanKind {
    if (step.llm_call_id || step.step_type === "llm") return "llm";
    if (step.tool_call_id || step.step_type === "tool") return "tool";
    if (["workflow", "phase", "agent", "task"].some((value) => step.step_type.includes(value))) return "group";
    return "other";
  }

  function getStructuralLevel(step: RunStep) {
    if (step.step_type.includes("workflow") || step.name === "workflow.task") return 0;
    if (step.step_type.includes("phase")) return 1;
    if (step.step_type.includes("agent")) return 2;
    if (step.step_type.includes("task")) return 0;
    return null;
  }

  function displayName(step: RunStep) {
    if (step.step_type === "llm") return `llm ${step.name}`;
    if (step.step_type === "tool") return `tool ${step.name}`;
    if (step.step_type.includes("phase")) {
      if (step.name === "workflow.phase" || step.name === "phase") {
        return `phase ${Math.floor((step.order_index ?? 1) / 2) + 1}`;
      }
      return step.name.startsWith("phase") ? step.name : `phase: ${step.name.replace(/^workflow\./, "")}`;
    }
    return step.name;
  }

  function buildTicks(rangeNs: number) {
    const minutes = Math.max(1, rangeNs / 60_000_000_000);
    const roughStep = minutes <= 10 ? 1 : minutes <= 35 ? 5 : minutes <= 90 ? 10 : 30;
    const result: { label: string; percent: number }[] = [];
    for (let minute = 0; minute <= minutes; minute += roughStep) {
      result.push({ label: `${minute}m`, percent: clamp((minute / minutes) * 100, 0, 100) });
    }
    return result;
  }

  function matchesFilter(span: TraceSpan, filter: TraceFilter) {
    if (filter === "all") return true;
    if (filter === "llm") return span.kind === "group" || span.kind === "llm";
    if (filter === "tool") return span.kind === "group" || span.kind === "tool";
    if (filter === "failed") return isFailed(span);
    if (filter === "slow") return span.duration_value >= 60_000_000_000;
    if (filter === "high-token") return span.token_total >= 50_000;
    return true;
  }

  function defaultSelectedSpan(traceSpans: TraceSpan[]) {
    return traceSpans.find(isFailed) ?? traceSpans.find((span) => span.kind === "llm") ?? traceSpans[0] ?? null;
  }

  function selectSpan(span: TraceSpan) {
    selectedStepId = span.step_id;
  }

  function handleRowKeydown(event: KeyboardEvent, span: TraceSpan) {
    if (event.key !== "Enter" && event.key !== " ") return;
    event.preventDefault();
    selectSpan(span);
  }

  function isFailed(span: TraceSpan) {
    return span.status !== "success" || span.error_type != null;
  }

  function spanDot(span: TraceSpan) {
    if (span.kind === "llm") return "L";
    if (span.kind === "tool") return "T";
    if (span.kind === "group") return "G";
    return "S";
  }

  function barWidth(span: TraceSpan) {
    if (span.width_percent < 0.7) return "7px";
    return `${span.width_percent}%`;
  }

  function durationLabel(span: TraceSpan) {
    if (span.duration_value === 0) return "<1ms";
    return fmtDuration(span.duration_value);
  }

  function summarizeModels(calls: LlmCall[]) {
    const rows = new Map<string, { model: string; calls: number; tokens: number; cost: number }>();
    for (const call of calls) {
      const model = call.model ?? call.operation ?? "unknown";
      const current = rows.get(model) ?? { model, calls: 0, tokens: 0, cost: 0 };
      current.calls += 1;
      current.tokens += call.input_tokens + call.output_tokens;
      current.cost += call.total_cost_usd;
      rows.set(model, current);
    }
    return [...rows.values()].sort((a, b) => b.tokens - a.tokens).slice(0, 3);
  }

  function summarizeTools(calls: ToolCall[]) {
    const rows = new Map<string, { tool: string; calls: number; failed: number }>();
    for (const call of calls) {
      const current = rows.get(call.tool_name) ?? { tool: call.tool_name, calls: 0, failed: 0 };
      current.calls += 1;
      if (call.status !== "success" || call.error_type) current.failed += 1;
      rows.set(call.tool_name, current);
    }
    return [...rows.values()].sort((a, b) => b.calls - a.calls).slice(0, 3);
  }

  function summarizeSignals(signals: RunSignal[]): SignalSummary[] {
    const rows = new Map<string, SignalSummary>();
    const severityRank = new Map([
      ["high", 3],
      ["medium", 2],
      ["low", 1],
    ]);

    for (const signal of signals) {
      const current = rows.get(signal.title);
      const count = "count" in signal && typeof signal.count === "number" ? signal.count : 1;
      if (!current) {
        rows.set(signal.title, { title: signal.title, severity: signal.severity, count });
        continue;
      }

      current.count += count;
      if ((severityRank.get(signal.severity) ?? 0) > (severityRank.get(current.severity) ?? 0)) {
        current.severity = signal.severity;
      }
    }

    return [...rows.values()]
      .sort((a, b) => (severityRank.get(b.severity) ?? 0) - (severityRank.get(a.severity) ?? 0) || b.count - a.count)
      .slice(0, 2);
  }

  function miniTop(span: TraceSpan) {
    return 5 + (span.timeline_index % 5) * 6;
  }

  function clamp(value: number, min: number, max: number) {
    return Math.max(min, Math.min(max, value));
  }
</script>

<section class="panel trace-workspace">
  <span class="corner tl"></span><span class="corner tr"></span>
  <span class="corner bl"></span><span class="corner br"></span>

  <div class="trace-workspace-head">
    <div>
      <div class="panel-title">Trace Workspace</div>
      <p>{fmtDuration(runRangeNs)} across {spans.length} spans</p>
    </div>
    <div class="trace-filters">
      {#each filters as filter}
        <button type="button" class:active={activeFilter === filter.value} on:click={() => (activeFilter = filter.value)}>
          {filter.label}
        </button>
      {/each}
    </div>
  </div>

  <div class="trace-minimap" aria-label="Trace overview">
    {#each spans as span}
      <span
        class="trace-mini-span {span.kind}"
        class:failed={isFailed(span)}
        style="left: {span.offset_percent}%; width: {barWidth(span)}; top: {miniTop(span)}px"
      ></span>
    {/each}
  </div>

  <div class="trace-scroll">
    <div class="trace-table">
      <div class="trace-row trace-header-row">
        <div>Span</div>
        <div>Duration</div>
        <div class="trace-axis">
          {#each ticks as tick}
            <span class="trace-tick" style="left: {tick.percent}%">{tick.label}</span>
          {/each}
        </div>
      </div>

      {#if visibleSpans.length}
        {#each visibleSpans as span}
          <div
            class="trace-row trace-span-row {span.kind}"
            class:selected={selectedSpan?.step_id === span.step_id}
            class:failed={isFailed(span)}
            role="button"
            tabindex="0"
            on:click={() => selectSpan(span)}
            on:keydown={(event) => handleRowKeydown(event, span)}
          >
            <div class="trace-span-cell" style="--trace-level: {span.level}">
              <span class="trace-branch" aria-hidden="true"></span>
              <span class="trace-dot">{spanDot(span)}</span>
              <span class="trace-name">{span.display_name}</span>
              {#if span.status !== "success"}
                <span class="trace-status">{span.status}</span>
              {/if}
            </div>
            <div class="trace-duration">{durationLabel(span)}</div>
            <div class="trace-lane">
              <span
                class="trace-bar {span.kind}"
                class:failed={isFailed(span)}
                class:instant={span.width_percent < 0.7}
                style="left: {span.offset_percent}%; width: {barWidth(span)}"
              ></span>
              {#if span.token_total > 0 && span.width_percent > 4}
                <span class="trace-token-label" style="left: {clamp(span.offset_percent + span.width_percent + 1.2, 0, 88)}%">
                  {fmtCompact(span.token_total)}
                </span>
              {/if}
            </div>
          </div>
        {/each}
      {:else}
        <div class="trace-empty">No spans match this filter.</div>
      {/if}
    </div>
  </div>

  <div class="selected-span-strip">
    <div class="panel-title">Selected Span</div>
    {#if selectedSpan}
      <div class="selected-span-line">
        <span class="trace-dot {selectedSpan.kind}">{spanDot(selectedSpan)}</span>
        <strong>{selectedSpan.display_name}</strong>
        <span>{selectedSpan.step_type}</span>
        <span>{durationLabel(selectedSpan)}</span>
        <span>{fmtCompact(selectedSpan.input_tokens)} in / {fmtCompact(selectedSpan.output_tokens)} out</span>
        <span>{fmtCost(selectedSpan.cost_usd)}</span>
        <span class:attention={isFailed(selectedSpan)} class:health={!isFailed(selectedSpan)}>{selectedSpan.status}</span>
      </div>
    {:else}
      <p class="empty">No span selected.</p>
    {/if}
  </div>

  <div class="trace-summary-row">
    <div>
      <span class="summary-label">Models</span>
      {#if modelRows.length}
        {#each modelRows as model}
          <span class="summary-chip">{model.model} · {model.calls} calls · {fmtCompact(model.tokens)} · {fmtCost(model.cost)}</span>
        {/each}
      {:else}
        <span class="muted">none</span>
      {/if}
    </div>
    <div>
      <span class="summary-label">Tools</span>
      {#if toolRows.length}
        {#each toolRows as tool}
          <span class="summary-chip" class:attention={tool.failed > 0}>{tool.tool} · {tool.calls} calls · {tool.failed} failed</span>
        {/each}
      {:else}
        <span class="muted">none</span>
      {/if}
    </div>
    <div>
      <span class="summary-label">Signals</span>
      {#if signalRows.length}
        {#each signalRows as signal}
          <span class="summary-chip" class:attention={signal.severity !== "low"} title={signal.title}>
            {signal.title}{#if signal.count > 1} ×{signal.count}{/if}
          </span>
        {/each}
      {:else}
        <span class="muted">none</span>
      {/if}
    </div>
  </div>
</section>
