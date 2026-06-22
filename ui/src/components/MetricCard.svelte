<script lang="ts">
  export let label: string;
  export let value: string | number;
  export let detail = "";
</script>

<div class="metric">
  <span class="corner tl"></span><span class="corner tr"></span>
  <span class="corner bl"></span><span class="corner br"></span>
  <span>{label}</span>
  <strong>{value}</strong>
  {#if detail}
    <small>{detail}</small>
  {/if}
</div>
