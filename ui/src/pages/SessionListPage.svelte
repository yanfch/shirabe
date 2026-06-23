<script lang="ts">
  import SortableHeader from "../components/SortableHeader.svelte";
  import { cacheRate, fmtCompact, fmtCost, fmtPercent, fmtTime, shortId } from "../lib/format";
  import type { SessionSummary } from "../lib/types";

  export let sessions: SessionSummary[] = [];
  export let onSelectSession: (sessionId: string) => void;

  type SortDirection = "asc" | "desc";
  type SortState = {
    column: string;
    direction: SortDirection;
  };
  type SortValue = string | number | null | undefined;

  let sort: SortState = { column: "last_active", direction: "desc" };

  const getters = {
    last_active: (row: SessionSummary) => row.last_seen_ns,
    session: (row: SessionSummary) => row.session_id,
    source: (row: SessionSummary) => row.source,
    turns: (row: SessionSummary) => row.turns,
    tokens: (row: SessionSummary) => row.input_tokens + row.output_tokens,
    cache: (row: SessionSummary) => row.cache_read_tokens,
    cost: (row: SessionSummary) => row.total_cost_usd,
    failed: (row: SessionSummary) => row.failed_tool_calls,
    models: (row: SessionSummary) => row.models.join(", "),
  };

  function nextSort(column: string): SortState {
    if (sort.column === column) {
      return { column, direction: sort.direction === "desc" ? "asc" : "desc" };
    }
    return { column, direction: "desc" };
  }

  function compareValues(left: SortValue, right: SortValue) {
    if (typeof left === "string" || typeof right === "string") {
      return String(left ?? "").localeCompare(String(right ?? ""));
    }
    return Number(left ?? 0) - Number(right ?? 0);
  }

  function activateOnKey(event: KeyboardEvent, sessionId: string) {
    if (event.key !== "Enter" && event.key !== " ") return;
    event.preventDefault();
    onSelectSession(sessionId);
  }

  $: sortedSessions = [...sessions].sort((left, right) => {
    const getter = getters[sort.column as keyof typeof getters];
    const result = getter ? compareValues(getter(left), getter(right)) : 0;
    return sort.direction === "asc" ? result : -result;
  });
</script>

<section class="panel sessions-list-panel">
  <span class="corner tl"></span><span class="corner tr"></span>
  <span class="corner bl"></span><span class="corner br"></span>
  <div class="panel-title">SESSIONS</div>
  <div class="table-scroll">
    <table class="usage-table sessions-list-table">
      <colgroup>
        <col class="col-time" />
        <col class="col-session" />
        <col class="col-source" />
        <col class="col-compact" />
        <col class="col-tokens" />
        <col class="col-cache" />
        <col class="col-cost" />
        <col class="col-failed" />
        <col class="col-models" />
      </colgroup>
      <thead>
        <tr>
          <SortableHeader label="Last active" column="last_active" sortColumn={sort.column} sortDirection={sort.direction} onSort={(column) => (sort = nextSort(column))} />
          <SortableHeader label="Session" column="session" sortColumn={sort.column} sortDirection={sort.direction} onSort={(column) => (sort = nextSort(column))} />
          <SortableHeader label="Source" column="source" sortColumn={sort.column} sortDirection={sort.direction} onSort={(column) => (sort = nextSort(column))} />
          <SortableHeader label="Turns" column="turns" sortColumn={sort.column} sortDirection={sort.direction} onSort={(column) => (sort = nextSort(column))} />
          <SortableHeader label="Tokens" column="tokens" sortColumn={sort.column} sortDirection={sort.direction} onSort={(column) => (sort = nextSort(column))} />
          <SortableHeader label="Cache" column="cache" sortColumn={sort.column} sortDirection={sort.direction} onSort={(column) => (sort = nextSort(column))} />
          <SortableHeader label="Cost" column="cost" sortColumn={sort.column} sortDirection={sort.direction} onSort={(column) => (sort = nextSort(column))} />
          <SortableHeader label="Failed" column="failed" sortColumn={sort.column} sortDirection={sort.direction} onSort={(column) => (sort = nextSort(column))} />
          <SortableHeader label="Models" column="models" sortColumn={sort.column} sortDirection={sort.direction} onSort={(column) => (sort = nextSort(column))} />
        </tr>
      </thead>
      <tbody>
        {#each sortedSessions as session}
          <tr
            class="clickable"
            role="button"
            tabindex="0"
            on:click={() => onSelectSession(session.session_id)}
            on:keydown={(event) => activateOnKey(event, session.session_id)}
          >
            <td>{fmtTime(session.last_seen_ns)}</td>
            <td>{shortId(session.session_id)}<br /><span class="muted">{session.title ?? session.kind}</span></td>
            <td>{session.source}</td>
            <td>{session.turns}<br /><span class="muted">{session.runs} runs</span></td>
            <td>{fmtCompact(session.input_tokens + session.output_tokens)}<br /><span class="muted token-detail">{fmtCompact(session.input_tokens)} in / {fmtCompact(session.output_tokens)} out</span></td>
            <td>{fmtCompact(session.cache_read_tokens)} <span class="muted">{fmtPercent(cacheRate(session.input_tokens, session.cache_read_tokens))}</span></td>
            <td>{fmtCost(session.total_cost_usd)}</td>
            <td class={session.failed_tool_calls ? "attention" : ""}>{session.failed_tool_calls}<br /><span class="muted">{fmtCompact(session.tool_calls)} calls</span></td>
            <td>{session.models.join(", ") || "-"}</td>
          </tr>
        {:else}
          <tr><td colspan="9" class="muted">No sessions imported yet</td></tr>
        {/each}
      </tbody>
    </table>
  </div>
</section>
