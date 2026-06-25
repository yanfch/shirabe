<script lang="ts">
  import CircleAlert from "@lucide/svelte/icons/circle-alert";
  import Database from "@lucide/svelte/icons/database";
  import MessagesSquare from "@lucide/svelte/icons/messages-square";
  import TrendingUp from "@lucide/svelte/icons/trending-up";
  import LatencyInsightPanel from "../components/LatencyInsightPanel.svelte";
  import SortableHeader from "../components/SortableHeader.svelte";
  import TokenTrendChart from "../components/TokenTrendChart.svelte";
  import {
    cacheRate,
    fmtCompact,
    fmtCost,
    fmtPercent,
    fmtTime,
    shortId,
  } from "../lib/format";
  import type {
    ModelSummary,
    Run,
    SessionSummary,
    SkillSummary,
    SourceUsageSummary,
    ToolFailureSummary,
    ToolSummary,
    Usage,
    UsageFilters,
  } from "../lib/types";

  export let usage: Usage | null;
  export let filters: UsageFilters;
  export let refreshing = false;
  export let onFilterChange: (filters: UsageFilters) => void;
  export let onSelectRun: (runId: string) => void;
  export let onSelectSession: (sessionId: string) => void;

  type SortDirection = "asc" | "desc";
  type SortState = {
    column: string;
    direction: SortDirection;
  };
  type SortValue = string | number | null | undefined;

  let sourceSort: SortState = { column: "tokens", direction: "desc" };
  let skillSort: SortState = { column: "runs", direction: "desc" };
  let modelSort: SortState = { column: "cost", direction: "desc" };
  let sessionSort: SortState = { column: "last_active", direction: "desc" };
  let toolFailureSort: SortState = { column: "failed", direction: "desc" };
  let toolSort: SortState = { column: "calls", direction: "desc" };
  let runSort: SortState = { column: "started", direction: "desc" };

  function nextSort(current: SortState, column: string): SortState {
    if (current.column === column) {
      return { column, direction: current.direction === "desc" ? "asc" : "desc" };
    }
    return { column, direction: "desc" };
  }

  function compareValues(left: SortValue, right: SortValue) {
    if (typeof left === "string" || typeof right === "string") {
      return String(left ?? "").localeCompare(String(right ?? ""));
    }
    return Number(left ?? 0) - Number(right ?? 0);
  }

  function sortRows<T>(
    rows: T[],
    sort: SortState,
    getters: Record<string, (row: T) => SortValue>,
  ) {
    const getter = getters[sort.column];
    if (!getter) return rows;

    return [...rows].sort((left, right) => {
      const result = compareValues(getter(left), getter(right));
      return sort.direction === "asc" ? result : -result;
    });
  }

  function applySourceFilter(source: string) {
    onFilterChange({
      ...filters,
      source: filters.source === source ? null : source,
    });
  }

  function applyModelFilter(model: string) {
    onFilterChange({
      ...filters,
      model: filters.model === model ? null : model,
    });
  }

  function activateOnKey(event: KeyboardEvent, action: () => void) {
    if (event.key !== "Enter" && event.key !== " ") return;
    event.preventDefault();
    action();
  }

  const sourceGetters = {
    source: (row: SourceUsageSummary) => row.source,
    sessions: (row: SourceUsageSummary) => row.sessions,
    turns: (row: SourceUsageSummary) => row.turns,
    tokens: (row: SourceUsageSummary) => row.input_tokens + row.output_tokens,
    cache: (row: SourceUsageSummary) => row.cache_read_tokens,
    cost: (row: SourceUsageSummary) => row.total_cost_usd,
    failed: (row: SourceUsageSummary) => row.failed_tool_calls,
  };
  const skillGetters = {
    source: (row: SkillSummary) => row.source,
    skill: (row: SkillSummary) => row.skill_name,
    loaded: (row: SkillSummary) => row.loaded_count,
    invoked: (row: SkillSummary) => row.invoked_count,
    attributed: (row: SkillSummary) => row.attributed_count,
    runs: (row: SkillSummary) => row.runs,
    confidence: (row: SkillSummary) => row.confidence,
  };
  const modelGetters = {
    model: (row: ModelSummary) => row.model,
    calls: (row: ModelSummary) => row.calls,
    tokens: (row: ModelSummary) => row.input_tokens + row.output_tokens,
    cache: (row: ModelSummary) => row.cache_read_tokens,
    cost: (row: ModelSummary) => row.total_cost_usd,
  };
  const sessionGetters = {
    last_active: (row: SessionSummary) => row.last_seen_ns,
    session: (row: SessionSummary) => row.session_id,
    turns: (row: SessionSummary) => row.turns,
    tokens: (row: SessionSummary) => row.input_tokens + row.output_tokens,
    cache: (row: SessionSummary) => row.cache_read_tokens,
    cost: (row: SessionSummary) => row.total_cost_usd,
    models: (row: SessionSummary) => row.models.join(", "),
  };
  const toolFailureGetters = {
    source: (row: ToolFailureSummary) => row.source,
    tool: (row: ToolFailureSummary) => row.tool_name,
    failed: (row: ToolFailureSummary) => row.failed_calls,
    calls: (row: ToolFailureSummary) => row.calls,
    failure: (row: ToolFailureSummary) => row.failure_rate,
  };
  const toolGetters = {
    tool: (row: ToolSummary) => row.tool_name,
    calls: (row: ToolSummary) => row.calls,
    success: (row: ToolSummary) => row.success_calls,
    failed: (row: ToolSummary) => row.failed_calls,
    failure: (row: ToolSummary) => row.failure_rate,
  };
  const runGetters = {
    run: (row: Run) => row.run_id,
    started: (row: Run) => row.started_at_ns,
    source: (row: Run) => row.source,
    tokens: (row: Run) => row.input_tokens + row.output_tokens,
    cost: (row: Run) => row.total_cost_usd,
    tools: (row: Run) => row.tool_call_count,
  };

  $: summary = usage?.summary;
  $: bucketUsage = usage?.buckets ?? [];
  $: latencyBuckets = usage?.latency_buckets ?? [];
  $: usageGrain = usage?.usage_grain ?? "day";
  $: sourceUsage = usage?.source_usage ?? [];
  $: recentSessions = usage?.recent_sessions ?? [];
  $: toolSummaries = usage?.tool_summaries ?? [];
  $: toolFailures = usage?.tool_failures ?? [];
  $: skillSummaries = usage?.skill_summaries ?? [];
  $: modelSummaries = usage?.model_summaries ?? [];
  $: recentRuns = usage?.recent_runs ?? [];
  $: sortedSourceUsage = sortRows(sourceUsage, sourceSort, sourceGetters);
  $: sortedSkillSummaries = sortRows(skillSummaries, skillSort, skillGetters);
  $: sortedModelSummaries = sortRows(modelSummaries, modelSort, modelGetters);
  $: sortedRecentSessions = sortRows(recentSessions, sessionSort, sessionGetters);
  $: sortedToolFailures = sortRows(toolFailures, toolFailureSort, toolFailureGetters);
  $: sortedToolSummaries = sortRows(toolSummaries, toolSort, toolGetters);
  $: sortedRecentRuns = sortRows(recentRuns, runSort, runGetters);
  $: totalCacheRate = summary?.cache_hit_rate ?? null;
  $: failedToolRate = summary?.tool_failure_rate ?? null;
  $: topRowIsSparse = bucketUsage.length > 1 && bucketUsage.length <= 4;
  $: singleBucketMode = bucketUsage.length <= 1;
  $: hasUsage =
    Boolean(summary) &&
    ((summary?.total_tokens ?? 0) > 0 ||
      (summary?.sessions ?? 0) > 0 ||
      (summary?.turns ?? 0) > 0 ||
      (summary?.tool_calls ?? 0) > 0);
</script>

<div class="usage-page" class:refreshing={refreshing}>
<section class="panel health-panel ledger-hero">
  <span class="corner tl"></span><span class="corner tr"></span>
  <span class="corner bl"></span><span class="corner br"></span>

  <div class="health-left">
    <div class="health-ring"><span>$</span></div>
    <div>
      <div class="health-title">Usage ledger</div>
      <p class="health-copy">Sessions, turns, token usage, cache tokens, cost, and tool calls.</p>
    </div>
  </div>

  <div class="health-stat">
    <MessagesSquare class="stat-icon" size={22} strokeWidth={1.5} aria-hidden="true" />
    <div>
      <div class="stat-label">Sessions / Turns</div>
      <div class="stat-value">{summary?.sessions ?? 0} / {summary?.turns ?? 0}</div>
      <div class="stat-caption">{summary?.runs ?? 0} runs</div>
    </div>
  </div>
  <div class="health-stat">
    <Database class="stat-icon" size={22} strokeWidth={1.5} aria-hidden="true" />
    <div>
      <div class="stat-label">Total tokens</div>
      <div class="stat-value">{fmtCompact(summary?.total_tokens)}</div>
      <div class="stat-caption">{fmtCompact(summary?.input_tokens)} in / {fmtCompact(summary?.output_tokens)} out</div>
    </div>
  </div>
  <div class="health-stat">
    <TrendingUp class="stat-icon" size={22} strokeWidth={1.5} aria-hidden="true" />
    <div>
      <div class="stat-label">Cost</div>
      <div class="stat-value">{fmtCost(summary?.total_cost_usd)}</div>
      <div class="stat-caption">computed from cached unit prices</div>
    </div>
  </div>
  <div class="health-stat">
    <CircleAlert class="stat-icon" size={22} strokeWidth={1.5} aria-hidden="true" />
    <div>
      <div class="stat-label">Failed tools</div>
      <div class="stat-value {summary?.failed_tool_calls ? 'attention' : 'health'}">{summary?.failed_tool_calls ?? 0}</div>
      <div class="stat-caption">{fmtPercent(failedToolRate)} of {fmtCompact(summary?.tool_calls)} calls</div>
    </div>
  </div>
</section>

<section class="panel metric-strip">
  <span class="corner tl"></span><span class="corner tr"></span>
  <span class="corner bl"></span><span class="corner br"></span>
  <div class="panel-kicker">TOKENS</div>
  <div class="metric-grid">
    <div class="metric-cell">
      <div class="metric-label">Input</div>
      <div class="metric-value">{fmtCompact(summary?.input_tokens)}</div>
      <div class="metric-delta muted">{summary?.llm_calls ?? 0} LLM calls</div>
    </div>
    <div class="metric-cell">
      <div class="metric-label">Output</div>
      <div class="metric-value">{fmtCompact(summary?.output_tokens)}</div>
      <div class="metric-delta muted">assistant output</div>
    </div>
    <div class="metric-cell">
      <div class="metric-label">Cache read</div>
      <div class="metric-value">{fmtCompact(summary?.cache_read_tokens)}</div>
      <div class="metric-delta muted">{fmtPercent(totalCacheRate)} of input</div>
    </div>
    <div class="metric-cell">
      <div class="metric-label">Cache rate</div>
      <div class="metric-value">{fmtPercent(totalCacheRate)}</div>
      <div class="metric-delta muted">{fmtCompact(summary?.cache_write_tokens)} write tokens</div>
    </div>
    <div class="metric-cell">
      <div class="metric-label">LLM calls</div>
      <div class="metric-value">{fmtCompact(summary?.llm_calls)}</div>
      <div class="metric-delta muted">{fmtCompact(summary?.tool_calls)} tool calls</div>
    </div>
  </div>
</section>

{#if hasUsage}
  <section class="usage-dashboard-grid" class:sparse-chart={topRowIsSparse} class:single-bucket={singleBucketMode}>
    <TokenTrendChart points={bucketUsage} grain={usageGrain} />

    <section class="panel stats-panel source-usage-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="panel-title">SOURCE USAGE</div>
    <div class="table-scroll">
    <table class="usage-table source-usage-table">
      <colgroup>
        <col class="col-source" />
        <col class="col-compact" />
        <col class="col-compact" />
        <col class="col-tokens" />
        <col class="col-cache" />
        <col class="col-cost" />
        <col class="col-failed" />
      </colgroup>
      <thead>
        <tr>
          <SortableHeader label="Source" column="source" sortColumn={sourceSort.column} sortDirection={sourceSort.direction} onSort={(column) => (sourceSort = nextSort(sourceSort, column))} />
          <SortableHeader label="Sessions" column="sessions" sortColumn={sourceSort.column} sortDirection={sourceSort.direction} onSort={(column) => (sourceSort = nextSort(sourceSort, column))} />
          <SortableHeader label="Turns" column="turns" sortColumn={sourceSort.column} sortDirection={sourceSort.direction} onSort={(column) => (sourceSort = nextSort(sourceSort, column))} />
          <SortableHeader label="Tokens" column="tokens" sortColumn={sourceSort.column} sortDirection={sourceSort.direction} onSort={(column) => (sourceSort = nextSort(sourceSort, column))} />
          <SortableHeader label="Cache" column="cache" sortColumn={sourceSort.column} sortDirection={sourceSort.direction} onSort={(column) => (sourceSort = nextSort(sourceSort, column))} />
          <SortableHeader label="Cost" column="cost" sortColumn={sourceSort.column} sortDirection={sourceSort.direction} onSort={(column) => (sourceSort = nextSort(sourceSort, column))} />
          <SortableHeader label="Failed tools" column="failed" sortColumn={sourceSort.column} sortDirection={sourceSort.direction} onSort={(column) => (sourceSort = nextSort(sourceSort, column))} />
        </tr>
      </thead>
      <tbody>
        {#each sortedSourceUsage as item}
          <tr
            class="clickable filter-row"
            class:active={filters.source === item.source}
            role="button"
            tabindex="0"
            title={filters.source === item.source ? "Clear source filter" : `Filter source: ${item.source}`}
            on:click={() => applySourceFilter(item.source)}
            on:keydown={(event) => activateOnKey(event, () => applySourceFilter(item.source))}
          >
            <td>{item.source}</td>
            <td>{item.sessions}</td>
            <td>{item.turns}</td>
            <td>{fmtCompact(item.input_tokens + item.output_tokens)}<br /><span class="muted token-detail">{fmtCompact(item.input_tokens)} in / {fmtCompact(item.output_tokens)} out</span></td>
            <td>{fmtCompact(item.cache_read_tokens)} <span class="muted">{fmtPercent(cacheRate(item.input_tokens, item.cache_read_tokens))}</span></td>
            <td>{fmtCost(item.total_cost_usd)}</td>
            <td class={item.failed_tool_calls ? "attention" : ""}>{item.failed_tool_calls}<br /><span class="muted">{fmtCompact(item.tool_calls)} calls</span></td>
          </tr>
        {/each}
      </tbody>
    </table>
    </div>
    </section>
  </section>

  <LatencyInsightPanel
    latency={usage?.latency ?? null}
    panel={usage?.latency_panel ?? null}
    buckets={latencyBuckets}
    {filters}
  />

  <section class="stats-grid usage-stats-grid">
    <section class="panel stats-panel skill-usage-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="panel-title">SKILL USAGE</div>
    <div class="table-scroll">
    <table class="usage-table skill-usage-table">
      <colgroup>
        <col class="col-source" />
        <col class="col-skill" />
        <col class="col-compact" />
        <col class="col-compact" />
        <col class="col-attributed" />
        <col class="col-runs" />
        <col class="col-confidence" />
      </colgroup>
      <thead>
        <tr>
          <SortableHeader label="Source" column="source" sortColumn={skillSort.column} sortDirection={skillSort.direction} onSort={(column) => (skillSort = nextSort(skillSort, column))} />
          <SortableHeader label="Skill" column="skill" sortColumn={skillSort.column} sortDirection={skillSort.direction} onSort={(column) => (skillSort = nextSort(skillSort, column))} />
          <SortableHeader label="Loaded" column="loaded" sortColumn={skillSort.column} sortDirection={skillSort.direction} onSort={(column) => (skillSort = nextSort(skillSort, column))} />
          <SortableHeader label="Invoked" column="invoked" sortColumn={skillSort.column} sortDirection={skillSort.direction} onSort={(column) => (skillSort = nextSort(skillSort, column))} />
          <SortableHeader label="Attributed" column="attributed" sortColumn={skillSort.column} sortDirection={skillSort.direction} onSort={(column) => (skillSort = nextSort(skillSort, column))} />
          <SortableHeader label="Runs" column="runs" sortColumn={skillSort.column} sortDirection={skillSort.direction} onSort={(column) => (skillSort = nextSort(skillSort, column))} />
          <SortableHeader label="Confidence" column="confidence" sortColumn={skillSort.column} sortDirection={skillSort.direction} onSort={(column) => (skillSort = nextSort(skillSort, column))} />
        </tr>
      </thead>
      <tbody>
        {#each sortedSkillSummaries as skill}
          <tr>
            <td>{skill.source}</td>
            <td>{skill.skill_name}</td>
            <td>{skill.loaded_count}</td>
            <td>{skill.invoked_count}</td>
            <td>{skill.attributed_count}</td>
            <td>{skill.runs}<br /><span class="muted">{skill.sessions} sessions</span></td>
            <td>{fmtPercent(skill.confidence)}</td>
          </tr>
        {:else}
          <tr><td colspan="7" class="muted">No skill events in this range</td></tr>
        {/each}
      </tbody>
    </table>
    </div>
    </section>

    <section class="panel stats-panel model-usage-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="panel-title">MODEL USAGE</div>
    <div class="table-scroll">
    <table class="usage-table model-usage-table">
      <colgroup>
        <col class="col-model" />
        <col class="col-compact" />
        <col class="col-tokens" />
        <col class="col-cache" />
        <col class="col-cost" />
      </colgroup>
      <thead>
        <tr>
          <SortableHeader label="Model" column="model" sortColumn={modelSort.column} sortDirection={modelSort.direction} onSort={(column) => (modelSort = nextSort(modelSort, column))} />
          <SortableHeader label="Calls" column="calls" sortColumn={modelSort.column} sortDirection={modelSort.direction} onSort={(column) => (modelSort = nextSort(modelSort, column))} />
          <SortableHeader label="Tokens" column="tokens" sortColumn={modelSort.column} sortDirection={modelSort.direction} onSort={(column) => (modelSort = nextSort(modelSort, column))} />
          <SortableHeader label="Cache" column="cache" sortColumn={modelSort.column} sortDirection={modelSort.direction} onSort={(column) => (modelSort = nextSort(modelSort, column))} />
          <SortableHeader label="Cost" column="cost" sortColumn={modelSort.column} sortDirection={modelSort.direction} onSort={(column) => (modelSort = nextSort(modelSort, column))} />
        </tr>
      </thead>
      <tbody>
        {#each sortedModelSummaries.slice(0, 8) as model}
          <tr
            class="clickable filter-row"
            class:active={filters.model === model.model}
            role="button"
            tabindex="0"
            title={filters.model === model.model ? "Clear model filter" : `Filter model: ${model.model}`}
            on:click={() => applyModelFilter(model.model)}
            on:keydown={(event) => activateOnKey(event, () => applyModelFilter(model.model))}
          >
            <td>{model.model}</td>
            <td>{model.calls}</td>
            <td>{fmtCompact(model.input_tokens + model.output_tokens)}<br /><span class="muted token-detail">{fmtCompact(model.input_tokens)} in / {fmtCompact(model.output_tokens)} out</span></td>
            <td>{fmtCompact(model.cache_read_tokens)} <span class="muted">{fmtPercent(cacheRate(model.input_tokens, model.cache_read_tokens))}</span></td>
            <td>{fmtCost(model.total_cost_usd)}</td>
          </tr>
        {/each}
      </tbody>
    </table>
    </div>
    </section>

    <section class="panel stats-panel sessions-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="panel-title">RECENT SESSIONS</div>
    <div class="table-scroll">
    <table class="sessions-table">
      <thead>
        <tr>
          <SortableHeader label="Last active" column="last_active" sortColumn={sessionSort.column} sortDirection={sessionSort.direction} onSort={(column) => (sessionSort = nextSort(sessionSort, column))} />
          <SortableHeader label="Session" column="session" sortColumn={sessionSort.column} sortDirection={sessionSort.direction} onSort={(column) => (sessionSort = nextSort(sessionSort, column))} />
          <SortableHeader label="Turns" column="turns" sortColumn={sessionSort.column} sortDirection={sessionSort.direction} onSort={(column) => (sessionSort = nextSort(sessionSort, column))} />
          <SortableHeader label="Tokens" column="tokens" sortColumn={sessionSort.column} sortDirection={sessionSort.direction} onSort={(column) => (sessionSort = nextSort(sessionSort, column))} />
          <SortableHeader label="Cache" column="cache" sortColumn={sessionSort.column} sortDirection={sessionSort.direction} onSort={(column) => (sessionSort = nextSort(sessionSort, column))} />
          <SortableHeader label="Cost" column="cost" sortColumn={sessionSort.column} sortDirection={sessionSort.direction} onSort={(column) => (sessionSort = nextSort(sessionSort, column))} />
          <SortableHeader label="Models" column="models" sortColumn={sessionSort.column} sortDirection={sessionSort.direction} onSort={(column) => (sessionSort = nextSort(sessionSort, column))} />
        </tr>
      </thead>
      <tbody>
        {#each sortedRecentSessions.slice(0, 5) as session}
          <tr
            class="clickable"
            role="button"
            tabindex="0"
            on:click={() => onSelectSession(session.session_id)}
            on:keydown={(event) => activateOnKey(event, () => onSelectSession(session.session_id))}
          >
            <td>{fmtTime(session.last_seen_ns)}</td>
            <td>{shortId(session.session_id)}<br /><span class="muted">{session.title ?? session.kind}</span></td>
            <td>{session.turns}</td>
            <td>{fmtCompact(session.input_tokens + session.output_tokens)}<br /><span class="muted">{fmtCompact(session.input_tokens)} in / {fmtCompact(session.output_tokens)} out</span></td>
            <td>{fmtCompact(session.cache_read_tokens)} <span class="muted">{fmtPercent(cacheRate(session.input_tokens, session.cache_read_tokens))}</span></td>
            <td>{fmtCost(session.total_cost_usd)}</td>
            <td>{session.models.join(", ") || "-"}</td>
          </tr>
        {/each}
      </tbody>
    </table>
    </div>
    </section>

    <section class="panel stats-panel tool-failure-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="panel-title">TOOL FAILURE HOTSPOTS</div>
    <div class="table-scroll">
    <table>
      <thead>
        <tr>
          <SortableHeader label="Source" column="source" sortColumn={toolFailureSort.column} sortDirection={toolFailureSort.direction} onSort={(column) => (toolFailureSort = nextSort(toolFailureSort, column))} />
          <SortableHeader label="Tool" column="tool" sortColumn={toolFailureSort.column} sortDirection={toolFailureSort.direction} onSort={(column) => (toolFailureSort = nextSort(toolFailureSort, column))} />
          <SortableHeader label="Failed" column="failed" sortColumn={toolFailureSort.column} sortDirection={toolFailureSort.direction} onSort={(column) => (toolFailureSort = nextSort(toolFailureSort, column))} />
          <SortableHeader label="Calls" column="calls" sortColumn={toolFailureSort.column} sortDirection={toolFailureSort.direction} onSort={(column) => (toolFailureSort = nextSort(toolFailureSort, column))} />
          <SortableHeader label="Failure" column="failure" sortColumn={toolFailureSort.column} sortDirection={toolFailureSort.direction} onSort={(column) => (toolFailureSort = nextSort(toolFailureSort, column))} />
        </tr>
      </thead>
      <tbody>
        {#each sortedToolFailures.slice(0, 10) as tool}
          <tr>
            <td>{tool.source}</td>
            <td>{tool.tool_name}</td>
            <td class={tool.failed_calls ? "attention" : ""}>{tool.failed_calls}</td>
            <td>{tool.calls}</td>
            <td>{fmtPercent(tool.failure_rate)}</td>
          </tr>
        {:else}
          <tr><td colspan="5" class="muted">No failed tools</td></tr>
        {/each}
      </tbody>
    </table>
    </div>
    </section>

    <section class="panel stats-panel tool-calls-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="panel-title">TOOL CALLS</div>
    <div class="table-scroll">
    <table>
      <thead>
        <tr>
          <SortableHeader label="Tool" column="tool" sortColumn={toolSort.column} sortDirection={toolSort.direction} onSort={(column) => (toolSort = nextSort(toolSort, column))} />
          <SortableHeader label="Calls" column="calls" sortColumn={toolSort.column} sortDirection={toolSort.direction} onSort={(column) => (toolSort = nextSort(toolSort, column))} />
          <SortableHeader label="Success" column="success" sortColumn={toolSort.column} sortDirection={toolSort.direction} onSort={(column) => (toolSort = nextSort(toolSort, column))} />
          <SortableHeader label="Failed" column="failed" sortColumn={toolSort.column} sortDirection={toolSort.direction} onSort={(column) => (toolSort = nextSort(toolSort, column))} />
          <SortableHeader label="Failure" column="failure" sortColumn={toolSort.column} sortDirection={toolSort.direction} onSort={(column) => (toolSort = nextSort(toolSort, column))} />
        </tr>
      </thead>
      <tbody>
        {#each sortedToolSummaries.slice(0, 8) as tool}
          <tr>
            <td>{tool.tool_name}</td>
            <td>{tool.calls}</td>
            <td>{tool.success_calls}</td>
            <td class={tool.failed_calls ? "attention" : ""}>{tool.failed_calls}</td>
            <td>{fmtPercent(tool.failure_rate)}</td>
          </tr>
        {/each}
      </tbody>
    </table>
    </div>
    </section>

    <section class="panel stats-panel recent-runs-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="panel-title">RECENT RUN LINKS</div>
    <div class="table-scroll">
    <table>
      <thead>
        <tr>
          <SortableHeader label="Run" column="run" sortColumn={runSort.column} sortDirection={runSort.direction} onSort={(column) => (runSort = nextSort(runSort, column))} />
          <SortableHeader label="Source" column="source" sortColumn={runSort.column} sortDirection={runSort.direction} onSort={(column) => (runSort = nextSort(runSort, column))} />
          <SortableHeader label="Tokens" column="tokens" sortColumn={runSort.column} sortDirection={runSort.direction} onSort={(column) => (runSort = nextSort(runSort, column))} />
          <SortableHeader label="Cost" column="cost" sortColumn={runSort.column} sortDirection={runSort.direction} onSort={(column) => (runSort = nextSort(runSort, column))} />
          <SortableHeader label="Tools" column="tools" sortColumn={runSort.column} sortDirection={runSort.direction} onSort={(column) => (runSort = nextSort(runSort, column))} />
        </tr>
      </thead>
      <tbody>
        {#each sortedRecentRuns.slice(0, 5) as run}
          <tr class="clickable" on:click={() => onSelectRun(run.run_id)}>
            <td>{shortId(run.run_id)}<br /><span class="muted">{fmtTime(run.started_at_ns)}</span></td>
            <td>{run.source}</td>
            <td>{fmtCompact(run.input_tokens + run.output_tokens)}<br /><span class="muted">{fmtCompact(run.cache_read_tokens)} cache</span></td>
            <td>{fmtCost(run.total_cost_usd)}</td>
            <td>{run.tool_call_count}<br /><span class={run.failed_tool_count ? "attention" : "muted"}>{run.failed_tool_count} failed</span></td>
          </tr>
        {/each}
      </tbody>
    </table>
    </div>
    </section>
  </section>
{:else}
  <TokenTrendChart points={bucketUsage} grain={usageGrain} />
{/if}
</div>
