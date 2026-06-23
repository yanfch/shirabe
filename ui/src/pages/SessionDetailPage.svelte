<script lang="ts">
  import { onMount, tick } from "svelte";
  import Database from "@lucide/svelte/icons/database";
  import MessagesSquare from "@lucide/svelte/icons/messages-square";
  import CircleAlert from "@lucide/svelte/icons/circle-alert";
  import TrendingUp from "@lucide/svelte/icons/trending-up";
  import { cacheRate, fmtCompact, fmtCost, fmtDuration, fmtPercent, fmtTime, shortId } from "../lib/format";
  import type { Run, SessionDetail, SessionTimelineEvent } from "../lib/types";

  export let detail: SessionDetail;
  export let onBack: () => void;
  export let onSelectRun: (runId: string) => void;
  let timelineList: HTMLDivElement | null = null;

  function eventTone(event: SessionTimelineEvent) {
    if (event.status === "failed") return "failed";
    if (event.event_type === "llm") return "llm";
    if (event.event_type === "tool") return "tool";
    if (event.event_type === "skill") return "skill";
    return "default";
  }

  function eventMeta(event: SessionTimelineEvent) {
    if (event.event_type === "llm") {
      return `${fmtDuration(event.duration_ns)} · ${fmtCompact(event.input_tokens)} in / ${fmtCompact(event.output_tokens)} out · ${fmtCost(event.total_cost_usd)}`;
    }
    if (event.event_type === "tool") {
      return event.error_type
        ? `${fmtDuration(event.duration_ns)} · ${event.status} · ${event.error_type}`
        : `${fmtDuration(event.duration_ns)} · ${event.status}`;
    }
    if (event.event_type === "skill") {
      return event.skill_event_type ?? event.status;
    }
    return `${event.status} · ${fmtDuration(event.duration_ns)}`;
  }

  function activateRun(event: KeyboardEvent, run: Run) {
    if (event.key !== "Enter" && event.key !== " ") return;
    event.preventDefault();
    onSelectRun(run.run_id);
  }

  onMount(async () => {
    await tick();
    if (timelineList) {
      timelineList.scrollTop = timelineList.scrollHeight;
    }
  });

  $: session = detail.session;
  $: totalTokens = session.input_tokens + session.output_tokens;
  $: cacheHit = cacheRate(session.input_tokens, session.cache_read_tokens);
  $: failedToolRate = cacheRate(session.tool_calls, session.failed_tool_calls);
  $: timeline = detail.timeline;
</script>

<section class="panel health-panel session-hero">
  <span class="corner tl"></span><span class="corner tr"></span>
  <span class="corner bl"></span><span class="corner br"></span>
  <div class="health-left">
    <div class="health-ring"><span>#</span></div>
    <div>
      <button class="text-button" on:click={onBack}>← Back</button>
      <div class="health-title">{shortId(session.session_id)}</div>
      <p>{session.source} / {session.title ?? session.kind} / last active {fmtTime(session.last_seen_ns)}</p>
    </div>
  </div>
  <div class="health-stat">
    <MessagesSquare class="stat-icon" size={22} strokeWidth={1.5} aria-hidden="true" />
    <div>
      <div class="stat-label">Runs / Turns</div>
      <div class="stat-value">{session.runs} / {session.turns}</div>
      <div class="stat-caption">{fmtCompact(session.llm_calls)} LLM calls</div>
    </div>
  </div>
  <div class="health-stat">
    <Database class="stat-icon" size={22} strokeWidth={1.5} aria-hidden="true" />
    <div>
      <div class="stat-label">Tokens</div>
      <div class="stat-value">{fmtCompact(totalTokens)}</div>
      <div class="stat-caption">{fmtCompact(session.input_tokens)} in / {fmtCompact(session.output_tokens)} out</div>
    </div>
  </div>
  <div class="health-stat">
    <TrendingUp class="stat-icon" size={22} strokeWidth={1.5} aria-hidden="true" />
    <div>
      <div class="stat-label">Cost</div>
      <div class="stat-value">{fmtCost(session.total_cost_usd)}</div>
      <div class="stat-caption">cache {fmtCompact(session.cache_read_tokens)} {fmtPercent(cacheHit)}</div>
    </div>
  </div>
  <div class="health-stat">
    <CircleAlert class="stat-icon" size={22} strokeWidth={1.5} aria-hidden="true" />
    <div>
      <div class="stat-label">Failed tools</div>
      <div class="stat-value {session.failed_tool_calls ? 'attention' : 'health'}">{session.failed_tool_calls}</div>
      <div class="stat-caption">{fmtPercent(failedToolRate)} of {fmtCompact(session.tool_calls)} calls</div>
    </div>
  </div>
</section>

<section class="session-detail-grid">
  <section class="panel session-timeline-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="panel-title">TIMELINE</div>
    <div class="timeline-list" bind:this={timelineList}>
      {#each timeline as event}
        <div class="timeline-item {eventTone(event)}">
          <div class="timeline-marker">{event.event_type.slice(0, 1)}</div>
          <div class="timeline-body">
            <div class="timeline-head">
              <strong>{event.label}</strong>
              <span>{fmtTime(event.started_at_ns)}</span>
            </div>
            <div class="timeline-meta">
              <span>{event.event_type}</span>
              <span>{eventMeta(event)}</span>
              <span>{shortId(event.run_id)}</span>
            </div>
          </div>
        </div>
      {:else}
        <p class="empty">No timeline events for this session.</p>
      {/each}
    </div>
  </section>

  <section class="panel stats-panel session-runs-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="panel-title">RUNS</div>
    <div class="table-scroll">
      <table>
        <thead><tr><th>Run</th><th>Runtime</th><th>Tokens</th><th>Cost</th><th>Tools</th></tr></thead>
        <tbody>
          {#each detail.runs.slice(0, 12) as run}
            <tr
              class="clickable"
              role="button"
              tabindex="0"
              on:click={() => onSelectRun(run.run_id)}
              on:keydown={(event) => activateRun(event, run)}
            >
              <td>{shortId(run.run_id)}<br /><span class="muted">{fmtTime(run.started_at_ns)}</span></td>
              <td>{fmtDuration(run.duration_ns)}</td>
              <td>{fmtCompact(run.input_tokens + run.output_tokens)}<br /><span class="muted">{fmtCompact(run.cache_read_tokens)} cache</span></td>
              <td>{fmtCost(run.total_cost_usd)}</td>
              <td>{run.tool_call_count}<br /><span class={run.failed_tool_count ? "attention" : "muted"}>{run.failed_tool_count} failed</span></td>
            </tr>
          {:else}
            <tr><td colspan="5" class="muted">No runs</td></tr>
          {/each}
        </tbody>
      </table>
    </div>
  </section>

  <section class="panel stats-panel session-turns-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="panel-title">TURNS</div>
    <div class="table-scroll">
      <table>
        <thead><tr><th>Turn</th><th>Runtime</th><th>Tokens</th><th>Cache</th><th>Models</th><th>Tools</th></tr></thead>
        <tbody>
          {#each detail.turns.slice(0, 16) as turn, index}
            <tr>
              <td>{index + 1}<br /><span class="muted">{fmtTime(turn.started_at_ns)}</span></td>
              <td>{fmtDuration(turn.duration_ns)}</td>
              <td>{fmtCompact(turn.input_tokens + turn.output_tokens)}<br /><span class="muted token-detail">{fmtCompact(turn.input_tokens)} in / {fmtCompact(turn.output_tokens)} out</span></td>
              <td>{fmtCompact(turn.cache_read_tokens)} <span class="muted">{fmtPercent(cacheRate(turn.input_tokens, turn.cache_read_tokens))}</span></td>
              <td>{turn.models.join(", ") || "-"}</td>
              <td>{turn.tool_calls}<br /><span class={turn.failed_tool_calls ? "attention" : "muted"}>{turn.failed_tool_calls} failed</span></td>
            </tr>
          {:else}
            <tr><td colspan="6" class="muted">No turn boundaries were imported for this session.</td></tr>
          {/each}
        </tbody>
      </table>
    </div>
  </section>

  <section class="panel stats-panel session-side-panel">
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
    <div class="session-side-scroll">
      <div class="panel-title">MODELS</div>
      <table>
        <thead><tr><th>Model</th><th>Calls</th><th>Tokens</th><th>Cost</th></tr></thead>
        <tbody>
          {#each detail.model_summaries.slice(0, 6) as model}
            <tr>
              <td>{model.model}</td>
              <td>{model.calls}</td>
              <td>{fmtCompact(model.input_tokens + model.output_tokens)}</td>
              <td>{fmtCost(model.total_cost_usd)}</td>
            </tr>
          {:else}
            <tr><td colspan="4" class="muted">No models</td></tr>
          {/each}
        </tbody>
      </table>

      <div class="panel-title inline-title">TOOLS</div>
      <table>
        <thead><tr><th>Tool</th><th>Calls</th><th>Failed</th></tr></thead>
        <tbody>
          {#each detail.tool_summaries.slice(0, 6) as tool}
            <tr>
              <td>{tool.tool_name}</td>
              <td>{tool.calls}</td>
              <td class={tool.failed_calls ? "attention" : ""}>{tool.failed_calls}</td>
            </tr>
          {:else}
            <tr><td colspan="3" class="muted">No tools</td></tr>
          {/each}
        </tbody>
      </table>

      <div class="panel-title inline-title">SKILLS</div>
      <table>
        <thead><tr><th>Skill</th><th>Loaded</th><th>Invoked</th></tr></thead>
        <tbody>
          {#each detail.skill_summaries.slice(0, 6) as skill}
            <tr>
              <td>{skill.skill_name}</td>
              <td>{skill.loaded_count}</td>
              <td>{skill.invoked_count}</td>
            </tr>
          {:else}
            <tr><td colspan="3" class="muted">No skills</td></tr>
          {/each}
        </tbody>
      </table>
    </div>
  </section>
</section>
