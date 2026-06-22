<script lang="ts">
  import CircleAlert from "@lucide/svelte/icons/circle-alert";
  import Database from "@lucide/svelte/icons/database";
  import MessagesSquare from "@lucide/svelte/icons/messages-square";
  import TrendingUp from "@lucide/svelte/icons/trending-up";
  import TokenTrendChart from "../components/TokenTrendChart.svelte";
  import {
    cacheRate,
    fmtCompact,
    fmtCost,
    fmtPercent,
    fmtTime,
    shortId,
  } from "../lib/format";
  import type { Usage } from "../lib/types";

  export let usage: Usage | null;
  export let onSelectRun: (runId: string) => void;

  $: summary = usage?.summary;
  $: bucketUsage = usage?.buckets ?? [];
  $: usageGrain = usage?.usage_grain ?? "day";
  $: usageTableTitle = usageGrain === "month" ? "MONTHLY USAGE" : "DAILY USAGE";
  $: sourceUsage = usage?.source_usage ?? [];
  $: recentSessions = usage?.recent_sessions ?? [];
  $: toolSummaries = usage?.tool_summaries ?? [];
  $: toolFailures = usage?.tool_failures ?? [];
  $: modelSummaries = usage?.model_summaries ?? [];
  $: recentRuns = usage?.recent_runs ?? [];
  $: totalCacheRate = summary?.cache_hit_rate ?? null;
  $: failedToolRate = summary?.tool_failure_rate ?? null;
  $: topRowIsSparse = bucketUsage.length > 1 && bucketUsage.length <= 4;
  $: hasUsage =
    Boolean(summary) &&
    ((summary?.total_tokens ?? 0) > 0 ||
      (summary?.sessions ?? 0) > 0 ||
      (summary?.turns ?? 0) > 0 ||
      (summary?.tool_calls ?? 0) > 0);
</script>

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
  <section class="usage-dashboard-grid" class:sparse-chart={topRowIsSparse}>
    <TokenTrendChart points={bucketUsage} grain={usageGrain} />

    <section class="panel stats-panel source-usage-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="panel-title">SOURCE USAGE</div>
    <div class="table-scroll">
    <table>
      <thead>
        <tr><th>Source</th><th>Sessions</th><th>Turns</th><th>Tokens</th><th>Cache</th><th>Cost</th><th>Failed tools</th></tr>
      </thead>
      <tbody>
        {#each sourceUsage as item}
          <tr>
            <td>{item.source}</td>
            <td>{item.sessions}</td>
            <td>{item.turns}</td>
            <td>{fmtCompact(item.input_tokens + item.output_tokens)}<br /><span class="muted">{fmtCompact(item.input_tokens)} in / {fmtCompact(item.output_tokens)} out</span></td>
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

  <section class="stats-grid usage-stats-grid">
    <section class="panel stats-panel usage-bucket-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="panel-title">{usageTableTitle}</div>
    <div class="table-scroll">
    <table>
      <thead>
        <tr><th>Date</th><th>Sessions</th><th>Turns</th><th>Tokens</th><th>Cache</th><th>Cost</th><th>Failed tools</th></tr>
      </thead>
      <tbody>
        {#each bucketUsage as day}
          <tr>
            <td>{day.date}</td>
            <td>{day.sessions}</td>
            <td>{day.turns}</td>
            <td>{fmtCompact(day.input_tokens + day.output_tokens)}<br /><span class="muted">{fmtCompact(day.input_tokens)} in / {fmtCompact(day.output_tokens)} out</span></td>
            <td>{fmtCompact(day.cache_read_tokens)} <span class="muted">{fmtPercent(cacheRate(day.input_tokens, day.cache_read_tokens))}</span></td>
            <td>{fmtCost(day.total_cost_usd)}</td>
            <td class={day.failed_tool_calls ? "attention" : ""}>{day.failed_tool_calls}<br /><span class="muted">{fmtCompact(day.tool_calls)} calls</span></td>
          </tr>
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
    <table>
      <thead>
        <tr><th>Model</th><th>Calls</th><th>Tokens</th><th>Cache</th><th>Cost</th></tr>
      </thead>
      <tbody>
        {#each modelSummaries.slice(0, 8) as model}
          <tr>
            <td>{model.model}</td>
            <td>{model.calls}</td>
            <td>{fmtCompact(model.input_tokens + model.output_tokens)}<br /><span class="muted">{fmtCompact(model.input_tokens)} in / {fmtCompact(model.output_tokens)} out</span></td>
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
        <tr><th>Last active</th><th>Session</th><th>Turns</th><th>Tokens</th><th>Cache</th><th>Cost</th><th>Models</th></tr>
      </thead>
      <tbody>
        {#each recentSessions.slice(0, 5) as session}
          <tr>
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
        <tr><th>Source</th><th>Tool</th><th>Failed</th><th>Calls</th><th>Failure</th></tr>
      </thead>
      <tbody>
        {#each toolFailures.slice(0, 10) as tool}
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
        <tr><th>Tool</th><th>Calls</th><th>Success</th><th>Failed</th><th>Failure</th></tr>
      </thead>
      <tbody>
        {#each toolSummaries.slice(0, 8) as tool}
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
        <tr><th>Run</th><th>Source</th><th>Tokens</th><th>Cost</th><th>Tools</th></tr>
      </thead>
      <tbody>
        {#each recentRuns.slice(0, 5) as run}
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
