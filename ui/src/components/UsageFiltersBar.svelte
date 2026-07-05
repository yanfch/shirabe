<script lang="ts">
  import FilterDropdown from "./FilterDropdown.svelte";
  import type { ProfileOption, UsageFilters, UsageGrain } from "../lib/types";

  export let filters: UsageFilters;
  export let profileOptions: ProfileOption[] = [];
  export let sourceOptions: string[] = [];
  export let modelOptions: string[] = [];
  export let onFilterChange: (filters: UsageFilters) => void;

  const timePickerId = crypto.randomUUID();
  let timeOpen = false;
  let customExpanded = false;
  let draftFrom = "";
  let draftTo = "";

  const presets = [
    { value: "today", label: "Today" },
    { value: "7d", label: "7d" },
    { value: "30d", label: "30d" },
    { value: "this_month", label: "MTD" },
    { value: "all", label: "All" },
  ];

  $: sourceFilterOptions = [
    { value: null, label: "All sources" },
    ...sourceOptions.map((source) => ({ value: source, label: source })),
  ];
  $: modelFilterOptions = [
    { value: null, label: "All models" },
    ...modelOptions.map((model) => ({ value: model, label: model })),
  ];
  $: profileFilterOptions = [
    { value: null, label: "All accounts" },
    ...profileOptions.map((profile) => ({
      value: profile.profile_id,
      label: profile.profile_label,
    })),
  ];
  $: timeLabel = `${presetLabel(filters.preset)} · ${grainLabel(filters.grain)}`;

  function setPreset(preset: string) {
    timeOpen = false;
    onFilterChange({ ...filters, preset, range: preset, from: null, to: null });
  }

  function toggleTime(event: MouseEvent) {
    event.stopPropagation();
    timeOpen = !timeOpen;
    if (timeOpen) {
      const defaults = defaultCustomRange();
      draftFrom = filters.from ?? defaults.from;
      draftTo = filters.to ?? defaults.to;
      customExpanded = false;
      window.dispatchEvent(new CustomEvent("shirabe-filter-open", { detail: timePickerId }));
    }
  }

  function toggleCustom(event: MouseEvent) {
    event.stopPropagation();
    customExpanded = !customExpanded;
  }

  function applyCustom(event: MouseEvent) {
    event.stopPropagation();
    if (!draftFrom || !draftTo || draftFrom >= draftTo) return;
    timeOpen = false;
    customExpanded = false;
    onFilterChange({
      ...filters,
      preset: "custom",
      range: "custom",
      from: draftFrom,
      to: draftTo,
    });
  }

  function setGrain(grain: UsageGrain) {
    onFilterChange({ ...filters, grain });
  }

  function setSource(source: string | null) {
    onFilterChange({ ...filters, source });
  }

  function setProfile(profile_id: string | null) {
    onFilterChange({ ...filters, profile_id });
  }

  function setModel(model: string | null) {
    onFilterChange({ ...filters, model });
  }

  function handlePeerOpen(event: Event) {
    if (event instanceof CustomEvent && event.detail !== timePickerId) {
      timeOpen = false;
      customExpanded = false;
    }
  }

  function handleKeydown(event: KeyboardEvent) {
    if (event.key === "Escape") {
      timeOpen = false;
      customExpanded = false;
    }
  }

  function presetLabel(preset: string) {
    return presets.find((option) => option.value === preset)?.label ?? "Custom";
  }

  function grainLabel(grain: UsageGrain) {
    return grain === "day" ? "Day" : grain === "month" ? "Month" : "Auto";
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
</script>

<svelte:window on:click={() => (timeOpen = false)} on:keydown={handleKeydown} on:shirabe-filter-open={handlePeerOpen} />

<div class="page-filter-bar topbar-filter-bar">
  <div class="filter-dropdown time-filter-dropdown" class:open={timeOpen}>
    <button class="filter-trigger" type="button" aria-haspopup="dialog" aria-expanded={timeOpen} on:click={toggleTime}>
      <span class="filter-trigger-label">Time</span>
      <span class="filter-trigger-value">{timeLabel}</span>
      <span class="filter-caret" aria-hidden="true"></span>
    </button>

    {#if timeOpen}
      <div class="time-menu" role="dialog" tabindex="-1" on:click|stopPropagation on:keydown|stopPropagation>
        <div class="time-menu-section">
          <div class="time-menu-label">Range</div>
          <div class="time-menu-presets">
            {#each presets as preset}
              <button
                class:active={filters.preset === preset.value}
                type="button"
                on:click={() => setPreset(preset.value)}
              >{preset.label}</button>
            {/each}
            <button class:active={filters.preset === "custom"} type="button" on:click={toggleCustom}>Custom</button>
          </div>

          {#if customExpanded}
            <div class="time-custom-fields">
              <label>
                <span>FROM</span>
                <input type="date" bind:value={draftFrom} />
              </label>
              <label>
                <span>UNTIL</span>
                <input type="date" bind:value={draftTo} />
              </label>
              <div class="time-custom-actions">
                <button type="button" disabled={!draftFrom || !draftTo || draftFrom >= draftTo} on:click={applyCustom}>Apply custom</button>
              </div>
            </div>
          {/if}
        </div>

        <div class="time-menu-section">
          <div class="time-menu-label">Sampling</div>
          <div class="range-filter grain-filter" aria-label="Sampling grain">
            <button class:active={filters.grain === "auto"} on:click={() => setGrain("auto")}>Auto</button>
            <button class:active={filters.grain === "day"} on:click={() => setGrain("day")}>Day</button>
            <button class:active={filters.grain === "month"} on:click={() => setGrain("month")}>Month</button>
          </div>
        </div>
      </div>
    {/if}
  </div>

  <FilterDropdown label="Account" value={filters.profile_id} options={profileFilterOptions} onSelect={setProfile} />
  <FilterDropdown label="Source" value={filters.source} options={sourceFilterOptions} onSelect={setSource} />
  <FilterDropdown label="Model" value={filters.model} options={modelFilterOptions} onSelect={setModel} />
</div>
