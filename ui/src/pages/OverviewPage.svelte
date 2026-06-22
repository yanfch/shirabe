<script lang="ts">
  import CircleAlert from "@lucide/svelte/icons/circle-alert";
  import FileStack from "@lucide/svelte/icons/file-stack";
  import MessagesSquare from "@lucide/svelte/icons/messages-square";
  import { fmtCompact, fmtCost, fmtTime, shortId } from "../lib/format";
  import type { Overview } from "../lib/types";

  export let overview: Overview | null;
  export let onSelectRun: (runId: string) => void;

  $: totals = overview?.totals;
  $: imports = overview?.imports ?? [];
  $: topSignals = overview?.top_signals ?? [];
  $: recentRuns = overview?.recent_runs ?? [];
</script>

<section class="panel health-panel">
  <span class="corner tl"></span><span class="corner tr"></span>
  <span class="corner bl"></span><span class="corner br"></span>

  <div class="health-left">
    <div class="health-ring"><span>~</span></div>
    <div>
      <div class="health-title">Overview</div>
      <p class="health-copy">Ingest status, latest activity, and the current diagnostic surface.</p>
    </div>
  </div>

  <div class="health-stat">
    <FileStack class="stat-icon" size={22} strokeWidth={1.5} aria-hidden="true" />
    <div>
      <div class="stat-label">Sources / Files</div>
      <div class="stat-value">{totals?.import_sources ?? 0} / {totals?.imported_files ?? 0}</div>
      <div class="stat-caption">{totals?.failed_files ?? 0} failed imports</div>
    </div>
  </div>
  <div class="health-stat">
    <MessagesSquare class="stat-icon" size={22} strokeWidth={1.5} aria-hidden="true" />
    <div>
      <div class="stat-label">Sessions / Runs</div>
      <div class="stat-value">{totals?.sessions ?? 0} / {totals?.runs ?? 0}</div>
      <div class="stat-caption">{totals?.turns ?? 0} turns</div>
    </div>
  </div>
  <div class="health-stat">
    <CircleAlert class="stat-icon" size={22} strokeWidth={1.5} aria-hidden="true" />
    <div>
      <div class="stat-label">Signals</div>
      <div class="stat-value {totals?.signals ? 'attention' : 'health'}">{totals?.signals ?? 0}</div>
      <div class="stat-caption">{totals?.failed_tool_calls ?? 0} failed tool calls</div>
    </div>
  </div>
</section>

<section class="panel metric-strip">
  <span class="corner tl"></span><span class="corner tr"></span>
  <span class="corner bl"></span><span class="corner br"></span>
  <div class="panel-kicker">CURRENT TOTALS</div>
  <div class="metric-grid">
    <div class="metric-cell">
      <div class="metric-label">Input</div>
      <div class="metric-value">{fmtCompact(totals?.input_tokens)}</div>
      <div class="metric-delta muted">{totals?.llm_calls ?? 0} LLM calls</div>
    </div>
    <div class="metric-cell">
      <div class="metric-label">Output</div>
      <div class="metric-value">{fmtCompact(totals?.output_tokens)}</div>
      <div class="metric-delta muted">assistant tokens</div>
    </div>
    <div class="metric-cell">
      <div class="metric-label">Cache read</div>
      <div class="metric-value">{fmtCompact(totals?.cache_read_tokens)}</div>
      <div class="metric-delta muted">{fmtCompact(totals?.cache_write_tokens)} written</div>
    </div>
    <div class="metric-cell">
      <div class="metric-label">Tools</div>
      <div class="metric-value">{totals?.tool_calls ?? 0}</div>
      <div class="metric-delta muted">{totals?.failed_tool_calls ?? 0} failed</div>
    </div>
    <div class="metric-cell">
      <div class="metric-label">Cost</div>
      <div class="metric-value">{fmtCost(totals?.total_cost_usd)}</div>
      <div class="metric-delta muted">from cached prices</div>
    </div>
  </div>
</section>

<section class="stats-grid">
  <section class="panel stats-panel wide-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="panel-title">RECENT RUNS</div>
    <table>
      <thead>
        <tr><th>Run</th><th>Source</th><th>Started</th><th>Tokens</th><th>Tools</th><th>Signal</th></tr>
      </thead>
      <tbody>
        {#each recentRuns as run}
          <tr class="clickable" on:click={() => onSelectRun(run.run_id)}>
            <td>{shortId(run.run_id)}</td>
            <td>{run.source}</td>
            <td>{fmtTime(run.started_at_ns)}</td>
            <td>{fmtCompact(run.input_tokens + run.output_tokens)}<br /><span class="muted">{fmtCompact(run.cache_read_tokens)} cache</span></td>
            <td>{run.tool_call_count}<br /><span class={run.failed_tool_count ? "attention" : "muted"}>{run.failed_tool_count} failed</span></td>
            <td>{run.primary_signal ?? "none"}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  </section>

  <section class="panel stats-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="panel-title">IMPORTS</div>
    <table>
      <thead>
        <tr><th>Source</th><th>Files</th><th>Events</th><th>Bytes</th></tr>
      </thead>
      <tbody>
        {#each imports as item}
          <tr>
            <td>{item.source}<br /><span class="muted">{item.source_kind}</span></td>
            <td>{item.file_count}</td>
            <td>{item.event_count}</td>
            <td>{fmtCompact(item.source_bytes)}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  </section>

  <section class="panel stats-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="panel-title">SIGNALS</div>
    {#if topSignals.length}
      <table>
        <thead>
          <tr><th>Type</th><th>Severity</th><th>Count</th></tr>
        </thead>
        <tbody>
          {#each topSignals as signal}
            <tr>
              <td>{signal.signal_type}</td>
              <td class={signal.severity}>{signal.severity}</td>
              <td>{signal.count}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    {:else}
      <p class="empty">No signals yet.</p>
    {/if}
  </section>
</section>
