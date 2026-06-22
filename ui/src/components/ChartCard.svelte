<script lang="ts">
  export let label: string;
  export let value: string;
  export let ratio: number | null = null;
</script>

<div class="chart-card">
  <div>
    <span>{label}</span>
    <strong>{value}</strong>
  </div>
  <div class="bar-track">
    <div class="bar-fill" style={`width: ${Math.max(0, Math.min(1, ratio ?? 0)) * 100}%`}></div>
  </div>
</div>
