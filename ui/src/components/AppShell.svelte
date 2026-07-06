<script lang="ts">
  import type { Overview, SyncStatus, WorkspaceStatus } from "../lib/types";

  export let overview: Overview | null;
  export let syncStatus: SyncStatus | null;
  export let workspaceStatus: WorkspaceStatus | null;
  export let workspaceBusy = false;
  export let selectedRunId: string | null;
  export let selectedSessionId: string | null;
  export let activePage: "overview" | "usage" | "sessions";
  export let onOverview: () => void;
  export let onUsage: () => void;
  export let onSessions: () => void;
  export let onSync: () => void;
  export let onRepairWorkspace: () => void;

  $: title = selectedRunId
    ? "Run Detail"
    : selectedSessionId
      ? "Session Detail"
      : activePage === "usage"
        ? "Usage"
        : activePage === "sessions"
          ? "Sessions"
          : "Overview";
  $: subtitle = selectedRunId
    ? selectedRunId
    : selectedSessionId
      ? selectedSessionId
      : activePage === "usage"
        ? "Usage, tokens, cost, cache, sessions, and tools"
        : activePage === "sessions"
          ? "Trace sessions, turns, timeline, models, tools, and skills"
          : "Local AI operations overview";
  $: syncRunning = syncStatus?.state === "running";
  $: syncLabel = syncRunning ? "Syncing" : syncStatus?.state === "failed" ? "Retry Sync" : "Sync";
  $: syncDetail = syncRunning
    ? syncStatus?.phase
    : syncStatus?.finished_at_ns
      ? `Last sync ${syncStatus.sources.filter((source) => source.status === "ok").length}/${syncStatus.sources.length}`
      : "Manual refresh";
  $: workspaceMode = workspaceStatus?.mode === "shared" ? "SHARED" : "LOCAL";
  $: workspacePath = workspaceStatus?.mode === "shared"
    ? workspaceStatus.workspace_dir
    : workspaceStatus?.shared_workspace_dir;
  $: workspaceAction = workspaceBusy
    ? "Working"
    : workspaceStatus?.mode === "shared"
      ? "Repair"
      : workspaceStatus?.shared_workspace_exists
        ? "Repair"
        : "Prepare Shared";
  $: workspaceDetail = workspaceStatus?.issue
    ?? (workspaceStatus?.mode === "shared"
      ? `${workspaceStatus.profile_count} profiles`
      : workspaceStatus?.shared_workspace_exists
        ? "Restart to join shared"
        : "Local only");
</script>

<div class="app-shell">
  <aside class="sidebar">
    <div class="brand">
      <div class="brand-name">Shirabe</div>
      <div class="brand-subtitle">AI OPERATIONS DIAGNOSTICS</div>
    </div>
    <nav class="nav-list">
      <button class:active={!selectedRunId && !selectedSessionId && activePage === "overview"} on:click={onOverview}>~ Overview</button>
      <button class:active={!selectedRunId && !selectedSessionId && activePage === "usage"} on:click={onUsage}>% Usage</button>
      <button class:active={!selectedRunId && activePage === "sessions"} on:click={onSessions}>= Sessions</button>
    </nav>
    <div class="sidebar-status">
      <div class="status-row"><span class="dot"></span>LOCAL API</div>
      <div class="status-grid">
        <span>Sources</span><strong>{overview?.totals.import_sources ?? "-"}</strong>
        <span>Files</span><strong>{overview?.totals.import_files ?? "-"}</strong>
      </div>
      <div class="sync-control">
        <button class:running={syncRunning} on:click={onSync} disabled={syncRunning}>{syncLabel}</button>
        <span>{syncDetail}</span>
      </div>
      <div class="workspace-card" class:attention={workspaceStatus && (!workspaceStatus.writable || workspaceStatus.restart_required)}>
        <div class="workspace-head">
          <span>{workspaceMode}</span>
          <strong>{workspaceStatus?.current_profile_label ?? "-"}</strong>
        </div>
        <div class="workspace-path" title={workspacePath}>{workspacePath ?? "-"}</div>
        <div class="workspace-foot">
          <button on:click={onRepairWorkspace} disabled={workspaceBusy || !workspaceStatus?.repair_available}>{workspaceAction}</button>
          <span title={workspaceDetail}>{workspaceDetail}</span>
        </div>
      </div>
    </div>
  </aside>

  <main class="main">
    <header class="topbar">
      <div>
        {#if selectedRunId}
          <p class="breadcrumb">Runs &gt; Detail</p>
        {:else if selectedSessionId}
          <p class="breadcrumb">Sessions &gt; Detail</p>
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
