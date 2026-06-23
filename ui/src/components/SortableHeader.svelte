<script lang="ts">
  import ArrowDown from "@lucide/svelte/icons/arrow-down";
  import ArrowUp from "@lucide/svelte/icons/arrow-up";
  import ArrowUpDown from "@lucide/svelte/icons/arrow-up-down";

  export let label: string;
  export let column: string;
  export let sortColumn: string;
  export let sortDirection: "asc" | "desc";
  export let onSort: (column: string) => void;

  $: active = column === sortColumn;
  $: Icon = active ? (sortDirection === "asc" ? ArrowUp : ArrowDown) : ArrowUpDown;
  $: ariaSort = active ? (sortDirection === "asc" ? "ascending" : "descending") : "none";
</script>

<th aria-sort={ariaSort} class:sort-active={active}>
  <button class="sort-header" type="button" on:click={() => onSort(column)}>
    <span>{label}</span>
    <svelte:component this={Icon} size={11} strokeWidth={1.6} aria-hidden="true" />
  </button>
</th>
