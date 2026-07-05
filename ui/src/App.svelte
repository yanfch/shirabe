<script lang="ts">
  import { onMount } from "svelte";
  import AppShell from "./components/AppShell.svelte";
  import MenubarPage from "./pages/MenubarPage.svelte";
  import OverviewPage from "./pages/OverviewPage.svelte";
  import RunDetailPage from "./pages/RunDetailPage.svelte";
  import SessionDetailPage from "./pages/SessionDetailPage.svelte";
  import SessionListPage from "./pages/SessionListPage.svelte";
  import UsagePage from "./pages/UsagePage.svelte";
  import UsageFiltersBar from "./components/UsageFiltersBar.svelte";
  import { fetchOverview, fetchRunDetail, fetchSessionDetail, fetchSessions, fetchSyncStatus, fetchUsage, startSync } from "./lib/api";
  import type { Overview, RunDetail, SessionDetail, SessionList, SyncStatus, Usage, UsageFilters } from "./lib/types";

  let overview: Overview | null = null;
  let usage: Usage | null = null;
  let sessionList: SessionList | null = null;
  let sessionDetail: SessionDetail | null = null;
  let runDetail: RunDetail | null = null;
  let syncStatus: SyncStatus | null = null;
  let syncPoll: number | null = null;
  const initialParams = new URLSearchParams(location.search);
  const isMenubarView = initialParams.get("view") === "menubar";
  let selectedRunId: string | null = initialParams.get("run");
  let selectedSessionId: string | null = selectedRunId ? null : initialParams.get("session");
  let activePage: "overview" | "usage" | "sessions" = pageFromParams(initialParams);
  let usageFilters: UsageFilters = usageFiltersFromParams(initialParams);
  let initialLoading = true;
  let refreshing = false;
  let error: string | null = null;

  if (!history.state?.shirabe) {
    history.replaceState({ shirabe: true, depth: 0 }, "", location.href);
  }

  let navigationDepth = typeof history.state?.depth === "number" ? history.state.depth : 0;

  $: currentHasData =
    Boolean(runDetail) ||
    Boolean(sessionDetail) ||
    (activePage === "usage"
      ? Boolean(usage)
      : activePage === "sessions"
        ? Boolean(sessionList)
        : Boolean(overview));
  $: viewRefreshing = refreshing && currentHasData;
  $: viewKey = selectedRunId
    ? `run:${selectedRunId}`
    : selectedSessionId
      ? `session:${selectedSessionId}`
      : activePage;

  function normalizePreset(value: string | null) {
    return value === "today" ||
      value === "7d" ||
      value === "30d" ||
      value === "this_month" ||
      value === "all" ||
      value === "custom"
      ? value
      : "30d";
  }

  function normalizeGrain(value: string | null): UsageFilters["grain"] {
    return value === "day" || value === "month" ? value : "auto";
  }

  function normalizeDateParam(value: string | null) {
    return value && /^\d{4}-\d{2}-\d{2}$/.test(value) ? value : null;
  }

  function pageFromParams(params: URLSearchParams): "overview" | "usage" | "sessions" {
    if (params.get("run")) return "overview";
    if (params.get("session") || params.get("page") === "sessions") return "sessions";
    if (params.get("page") === "usage") return "usage";
    return "overview";
  }

  function usageFiltersFromParams(params: URLSearchParams): UsageFilters {
    const preset = normalizePreset(params.get("preset") ?? params.get("range"));
    const customRange = defaultCustomRange();
    return {
      preset,
      range: preset,
      from: preset === "custom" ? normalizeDateParam(params.get("from")) ?? customRange.from : null,
      to: preset === "custom" ? normalizeDateParam(params.get("to")) ?? customRange.to : null,
      grain: normalizeGrain(params.get("grain")),
      profile_id: params.get("profile_id"),
      device_id: params.get("device_id"),
      source: params.get("source"),
      model: params.get("model"),
    };
  }

  function defaultCustomRange() {
    const to = new Date();
    to.setDate(to.getDate() + 1);
    const from = new Date(to);
    from.setDate(from.getDate() - 30);
    return {
      from: formatDateInput(from),
      to: formatDateInput(to),
    };
  }

  function formatDateInput(date: Date) {
    const year = date.getFullYear();
    const month = String(date.getMonth() + 1).padStart(2, "0");
    const day = String(date.getDate()).padStart(2, "0");
    return `${year}-${month}-${day}`;
  }

  function usagePath(filters = usageFilters) {
    const params = new URLSearchParams();
    params.set("page", "usage");
    params.set("preset", filters.preset);
    params.set("grain", filters.grain);
    if (filters.preset === "custom") {
      if (filters.from) params.set("from", filters.from);
      if (filters.to) params.set("to", filters.to);
    }
    if (filters.profile_id) params.set("profile_id", filters.profile_id);
    if (filters.device_id) params.set("device_id", filters.device_id);
    if (filters.source) params.set("source", filters.source);
    if (filters.model) params.set("model", filters.model);
    return `/?${params.toString()}`;
  }

  function syncUsageFiltersFromResponse(nextUsage: Usage) {
    const canonical = nextUsage.filters;
    usageFilters = canonical;
    const nextPath = usagePath(canonical);
    if (location.pathname + location.search !== nextPath) {
      history.replaceState(
        { shirabe: true, depth: navigationDepth },
        "",
        nextPath,
      );
    }
  }

  function sessionsPath() {
    return "/?page=sessions";
  }

  function pushAppRoute(path: string) {
    navigationDepth += 1;
    history.pushState({ shirabe: true, depth: navigationDepth }, "", path);
  }

  async function loadCurrent() {
    if (isMenubarView) {
      initialLoading = false;
      return;
    }

    initialLoading = true;
    refreshing = false;
    error = null;
    try {
      overview = await fetchOverview();
      syncStatus = await fetchSyncStatus();
      if (syncStatus.state === "running") beginSyncPolling();
      if (selectedRunId) {
        runDetail = await fetchRunDetail(selectedRunId);
      } else if (selectedSessionId) {
        sessionDetail = await fetchSessionDetail(selectedSessionId);
      } else if (activePage === "usage") {
        usage = await fetchUsage(usageFilters);
        if (usage) syncUsageFiltersFromResponse(usage);
      } else if (activePage === "sessions") {
        sessionList = await fetchSessions();
      }
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      initialLoading = false;
    }
  }

  async function loadFromLocation() {
    const hadData = currentHasData;
    const params = new URLSearchParams(location.search);

    navigationDepth = typeof history.state?.depth === "number" ? history.state.depth : 0;
    selectedRunId = params.get("run");
    selectedSessionId = selectedRunId ? null : params.get("session");
    activePage = pageFromParams(params);
    usageFilters = usageFiltersFromParams(params);
    runDetail = null;
    sessionDetail = null;
    initialLoading = !hadData;
    refreshing = hadData;
    error = null;

    try {
      if (selectedRunId) {
        runDetail = await fetchRunDetail(selectedRunId);
      } else if (selectedSessionId) {
        sessionDetail = await fetchSessionDetail(selectedSessionId);
      } else if (activePage === "usage") {
        usage = await fetchUsage(usageFilters);
        if (usage) syncUsageFiltersFromResponse(usage);
      } else if (activePage === "sessions") {
        sessionList = await fetchSessions();
      } else if (!overview) {
        overview = await fetchOverview();
      }
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      initialLoading = false;
      refreshing = false;
    }
  }

  async function selectRun(runId: string) {
    refreshing = currentHasData;
    error = null;
    try {
      runDetail = await fetchRunDetail(runId);
      selectedRunId = runId;
      selectedSessionId = null;
      sessionDetail = null;
      activePage = "overview";
      pushAppRoute(`/?run=${encodeURIComponent(runId)}`);
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      refreshing = false;
    }
  }

  function showOverview() {
    selectedRunId = null;
    selectedSessionId = null;
    runDetail = null;
    sessionDetail = null;
    activePage = "overview";
    pushAppRoute("/");
  }

  function goBackFromDetail() {
    if (navigationDepth > 0) {
      history.back();
      return;
    }

    showOverview();
  }

  function goBackFromSessionDetail() {
    if (navigationDepth > 0) {
      history.back();
      return;
    }

    void showSessions();
  }

  async function showUsage() {
    selectedRunId = null;
    selectedSessionId = null;
    runDetail = null;
    sessionDetail = null;
    activePage = "usage";
    pushAppRoute(usagePath());
    await loadUsage();
  }

  async function showSessions() {
    selectedRunId = null;
    selectedSessionId = null;
    runDetail = null;
    sessionDetail = null;
    activePage = "sessions";
    pushAppRoute(sessionsPath());
    await loadSessions();
  }

  async function selectSession(sessionId: string) {
    refreshing = currentHasData;
    error = null;
    try {
      sessionDetail = await fetchSessionDetail(sessionId);
      selectedSessionId = sessionId;
      selectedRunId = null;
      runDetail = null;
      activePage = "sessions";
      pushAppRoute(`/?session=${encodeURIComponent(sessionId)}`);
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      refreshing = false;
    }
  }

  async function updateUsageFilters(next: UsageFilters) {
    usageFilters = next;
    selectedRunId = null;
    selectedSessionId = null;
    runDetail = null;
    sessionDetail = null;
    activePage = "usage";
    pushAppRoute(usagePath(next));
    await loadUsage();
  }

  async function loadUsage() {
    const hadUsage = usage !== null;
    initialLoading = !hadUsage;
    refreshing = hadUsage;
    error = null;
    try {
      usage = await fetchUsage(usageFilters);
      if (usage) syncUsageFiltersFromResponse(usage);
      if (!overview) {
        overview = await fetchOverview();
      }
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      initialLoading = false;
      refreshing = false;
    }
  }

  async function loadSessions() {
    const hadSessions = sessionList !== null;
    initialLoading = !hadSessions;
    refreshing = hadSessions;
    error = null;
    try {
      sessionList = await fetchSessions();
      if (!overview) {
        overview = await fetchOverview();
      }
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      initialLoading = false;
      refreshing = false;
    }
  }

  async function triggerSync() {
    error = null;
    try {
      syncStatus = await startSync();
      beginSyncPolling();
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
      syncStatus = await fetchSyncStatus();
      if (syncStatus.state === "running") beginSyncPolling();
    }
  }

  function beginSyncPolling() {
    if (syncPoll !== null) {
      window.clearInterval(syncPoll);
    }

    syncPoll = window.setInterval(async () => {
      try {
        const next = await fetchSyncStatus();
        syncStatus = next;
        if (next.state !== "running") {
          if (syncPoll !== null) {
            window.clearInterval(syncPoll);
            syncPoll = null;
          }
          await refreshCurrentView();
        }
      } catch (err) {
        if (syncPoll !== null) {
          window.clearInterval(syncPoll);
          syncPoll = null;
        }
        error = err instanceof Error ? err.message : String(err);
      }
    }, 1500);
  }

  async function refreshCurrentView() {
    refreshing = currentHasData;
    try {
      overview = await fetchOverview();
      if (runDetail && selectedRunId) {
        runDetail = await fetchRunDetail(selectedRunId);
      } else if (sessionDetail && selectedSessionId) {
        sessionDetail = await fetchSessionDetail(selectedSessionId);
      } else if (activePage === "usage") {
        usage = await fetchUsage(usageFilters);
      } else if (activePage === "sessions") {
        sessionList = await fetchSessions();
      }
    } finally {
      refreshing = false;
    }
  }

  loadCurrent();

  onMount(() => {
    const onPopState = () => {
      void loadFromLocation();
    };
    window.addEventListener("popstate", onPopState);
    return () => window.removeEventListener("popstate", onPopState);
  });

  $: usageSourceOptions = usage?.source_options ?? [];
  $: usageModelOptions = usage?.model_options ?? [];
  $: usageProfileOptions = usage?.profile_options ?? [];
</script>

{#if isMenubarView}
  <MenubarPage />
{:else}
<AppShell {overview} {syncStatus} {selectedRunId} {selectedSessionId} {activePage} onOverview={showOverview} onUsage={showUsage} onSessions={showSessions} onSync={triggerSync}>
  <svelte:fragment slot="topbar-extra">
    {#if activePage === "usage" && !selectedRunId}
        <UsageFiltersBar
          filters={usageFilters}
          profileOptions={usageProfileOptions}
          sourceOptions={usageSourceOptions}
        modelOptions={usageModelOptions}
        onFilterChange={updateUsageFilters}
      />
    {/if}
  </svelte:fragment>

  {#if error}
    <section class="panel error-panel">{error}</section>
  {/if}

  {#if initialLoading && !currentHasData}
    <section class="panel initial-loading-panel">
      <span class="corner tl"></span><span class="corner tr"></span>
      <span class="corner bl"></span><span class="corner br"></span>
      <div>
        <strong>Loading</strong>
        <span>Reading local usage data...</span>
      </div>
    </section>
  {:else}
    {#key viewKey}
      <div class="view-shell" class:refreshing={viewRefreshing} aria-busy={viewRefreshing}>
        {#if viewRefreshing}
          <div class="view-updating-badge">Updating</div>
        {/if}

        {#if runDetail}
          <RunDetailPage detail={runDetail} onBack={goBackFromDetail} />
        {:else if sessionDetail}
          <SessionDetailPage detail={sessionDetail} onBack={goBackFromSessionDetail} onSelectRun={selectRun} />
        {:else if activePage === "usage"}
          <UsagePage {usage} filters={usageFilters} refreshing={viewRefreshing} onFilterChange={updateUsageFilters} onSelectRun={selectRun} onSelectSession={selectSession} />
        {:else if activePage === "sessions"}
          <SessionListPage sessions={sessionList?.sessions ?? []} onSelectSession={selectSession} />
        {:else}
          <OverviewPage {overview} onSelectRun={selectRun} />
        {/if}
      </div>
    {/key}
  {/if}
</AppShell>
{/if}
