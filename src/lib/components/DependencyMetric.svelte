<script lang="ts">
  import { formatCount, safePublicUrl, type CountMetric } from "../package-insights";
  let { label, metric, pending = false, loaded = false, reason = null, error = null }: {
    label: string; metric?: CountMetric; pending?: boolean; loaded?: boolean; reason?: string | null; error?: string | null;
  } = $props();
</script>
<div class="metric">
  <span>{label}: <strong title={pending && !metric ? "Hover for a moment, pin the package or press Refresh to load it" : undefined}>{error ? "Unknown" : metric ? formatCount(metric) : pending ? "Not loaded" : loaded ? "Unavailable" : "Loading…"}</strong></span>
  {#if metric}
    <small class="dim">Source: {#if safePublicUrl(metric.source)}<a href={safePublicUrl(metric.source)!} target="_blank" rel="noopener noreferrer">{metric.source}</a>{:else}{metric.source}{/if}{#if metric.period} · {metric.period}{/if}</small>
    {#if metric.reason}<small class="dim">{metric.reason}</small>{/if}
    {#if metric.retryAt}<small class="dim">Retry after {new Date(metric.retryAt).toLocaleString()}</small>{/if}
  {:else if error || reason}<small class="dim">{error ?? reason}</small>{/if}
</div>
<style>
  .metric { display: flex; flex-direction: column; gap: 2px; }
  small { overflow-wrap: anywhere; }
</style>
