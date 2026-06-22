<script lang="ts">
  export type FilterOption = {
    value: string | null;
    label: string;
  };

  export let label: string;
  export let value: string | null = null;
  export let options: FilterOption[] = [];
  export let onSelect: (value: string | null) => void;

  const dropdownId = crypto.randomUUID();
  let open = false;

  $: selectedLabel = options.find((option) => option.value === value)?.label ?? "All";

  function toggle(event: MouseEvent) {
    event.stopPropagation();
    open = !open;
    if (open) {
      window.dispatchEvent(new CustomEvent("shirabe-filter-open", { detail: dropdownId }));
    }
  }

  function choose(event: MouseEvent, next: string | null) {
    event.stopPropagation();
    open = false;
    onSelect(next);
  }

  function handleKeydown(event: KeyboardEvent) {
    if (event.key === "Escape") {
      open = false;
    }
  }

  function handlePeerOpen(event: Event) {
    if (event instanceof CustomEvent && event.detail !== dropdownId) {
      open = false;
    }
  }
</script>

<svelte:window on:click={() => (open = false)} on:keydown={handleKeydown} on:shirabe-filter-open={handlePeerOpen} />

<div class="filter-dropdown" class:open>
  <button class="filter-trigger" type="button" aria-haspopup="menu" aria-expanded={open} on:click={toggle}>
    <span class="filter-trigger-label">{label}</span>
    <span class="filter-trigger-value">{selectedLabel}</span>
    <span class="filter-caret" aria-hidden="true"></span>
  </button>

  {#if open}
    <div class="filter-menu" role="menu">
      {#each options as option}
        <button
          class:active={option.value === value}
          type="button"
          role="menuitemradio"
          aria-checked={option.value === value}
          on:click={(event) => choose(event, option.value)}
        >
          <span>{option.label}</span>
        </button>
      {/each}
    </div>
  {/if}
</div>
