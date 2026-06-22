<script lang="ts">
  import type { Overview } from "../lib/types";

  export let overview: Overview | null;
  export let selectedRunId: string | null;
  export let activePage: "overview" | "usage";
  export let onOverview: () => void;
  export let onUsage: () => void;

  $: title = selectedRunId ? "Run Detail" : activePage === "usage" ? "Usage" : "Overview";
  $: subtitle = selectedRunId
    ? selectedRunId
    : activePage === "usage"
      ? "Usage, tokens, cost, cache, sessions, and tools"
      : "Local AI operations overview";
</script>

<div class="app-shell">
  <aside class="sidebar">
    <div class="brand">
      <div class="brand-name">shirabe</div>
      <div class="brand-subtitle">AI OPERATIONS DIAGNOSTICS</div>
    </div>
    <nav class="nav-list">
      <button class:active={!selectedRunId && activePage === "overview"} on:click={onOverview}>~ Overview</button>
      <button class:active={!selectedRunId && activePage === "usage"} on:click={onUsage}>% Usage</button>
      <button class:active={!!selectedRunId} disabled={!selectedRunId}># Run Detail</button>
    </nav>
    <div class="sidebar-status">
      <div class="status-row"><span class="dot"></span>LOCAL API</div>
      <div class="status-grid">
        <span>Sources</span><strong>{overview?.totals.import_sources ?? "-"}</strong>
        <span>Files</span><strong>{overview?.totals.import_files ?? "-"}</strong>
      </div>
    </div>
  </aside>

  <main class="main">
    <header class="topbar">
      <div>
        {#if selectedRunId}
          <p class="breadcrumb">Runs &gt; Detail</p>
        {/if}
        <h1>&gt;_ {title}</h1>
        <p>{subtitle}</p>
      </div>
      <div class="topbar-actions">
        <div class="topbar-extra"><slot name="topbar-extra" /></div>
      </div>
    </header>

    <slot />
  </main>
</div>
