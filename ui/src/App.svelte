<script lang="ts">
  import AppShell from "./components/AppShell.svelte";
  import OverviewPage from "./pages/OverviewPage.svelte";
  import RunDetailPage from "./pages/RunDetailPage.svelte";
  import UsagePage from "./pages/UsagePage.svelte";
  import UsageFiltersBar from "./components/UsageFiltersBar.svelte";
  import { fetchOverview, fetchRunDetail, fetchUsage } from "./lib/api";
  import type { Overview, RunDetail, Usage, UsageFilters } from "./lib/types";

  let overview: Overview | null = null;
  let usage: Usage | null = null;
  let runDetail: RunDetail | null = null;
  const initialParams = new URLSearchParams(location.search);
  let selectedRunId: string | null = initialParams.get("run");
  let activePage: "overview" | "usage" = initialParams.get("page") === "usage" ? "usage" : "overview";
  const initialPreset = normalizePreset(initialParams.get("preset") ?? initialParams.get("range"));
  const initialFrom = normalizeDateParam(initialParams.get("from"));
  const initialTo = normalizeDateParam(initialParams.get("to"));
  const initialCustomRange = defaultCustomRange();
  let usageFilters: UsageFilters = {
    preset: initialPreset,
    range: initialPreset,
    from: initialPreset === "custom" ? initialFrom ?? initialCustomRange.from : null,
    to: initialPreset === "custom" ? initialTo ?? initialCustomRange.to : null,
    grain: normalizeGrain(initialParams.get("grain")),
    source: initialParams.get("source"),
    model: initialParams.get("model"),
  };
  let loading = true;
  let error: string | null = null;

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
    if (filters.source) params.set("source", filters.source);
    if (filters.model) params.set("model", filters.model);
    return `/?${params.toString()}`;
  }

  async function loadCurrent() {
    loading = true;
    error = null;
    try {
      overview = await fetchOverview();
      if (selectedRunId) {
        runDetail = await fetchRunDetail(selectedRunId);
      } else if (activePage === "usage") {
        usage = await fetchUsage(usageFilters);
      }
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      loading = false;
    }
  }

  async function selectRun(runId: string) {
    loading = true;
    error = null;
    try {
      runDetail = await fetchRunDetail(runId);
      selectedRunId = runId;
      activePage = "overview";
      history.pushState({}, "", `/?run=${encodeURIComponent(runId)}`);
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      loading = false;
    }
  }

  function showOverview() {
    selectedRunId = null;
    runDetail = null;
    activePage = "overview";
    history.pushState({}, "", "/");
  }

  async function showUsage() {
    selectedRunId = null;
    runDetail = null;
    activePage = "usage";
    history.pushState({}, "", usagePath());
    await loadUsage();
  }

  async function updateUsageFilters(next: UsageFilters) {
    usageFilters = next;
    selectedRunId = null;
    runDetail = null;
    activePage = "usage";
    history.pushState({}, "", usagePath(next));
    await loadUsage();
  }

  async function loadUsage() {
    loading = true;
    error = null;
    try {
      usage = await fetchUsage(usageFilters);
      if (!overview) {
        overview = await fetchOverview();
      }
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      loading = false;
    }
  }

  loadCurrent();

  $: usageSourceOptions = usage?.source_options ?? [];
  $: usageModelOptions = usage?.model_options ?? [];
</script>

<AppShell {overview} {selectedRunId} {activePage} onOverview={showOverview} onUsage={showUsage}>
  <svelte:fragment slot="topbar-extra">
    {#if activePage === "usage" && !selectedRunId}
      <UsageFiltersBar
        filters={usageFilters}
        sourceOptions={usageSourceOptions}
        modelOptions={usageModelOptions}
        onFilterChange={updateUsageFilters}
      />
    {/if}
  </svelte:fragment>

  {#if error}
    <section class="panel error-panel">{error}</section>
  {/if}

  {#if runDetail}
    <RunDetailPage detail={runDetail} onOverview={showOverview} />
  {:else if activePage === "usage"}
    <UsagePage {usage} onSelectRun={selectRun} />
  {:else}
    <OverviewPage {overview} onSelectRun={selectRun} />
  {/if}

  {#if loading}
    <div class="loading">Loading...</div>
  {/if}
</AppShell>
