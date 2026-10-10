<script lang="ts">
  import type { Instance } from "../api";
  import Tile from "./Tile.svelte";
  let { instances, onSelect, onPlan, onSubagents, onCreate, tileMin = 420 }: {
    instances: Instance[]; onSelect: (id: string) => void; onPlan: (id: string) => void;
    onSubagents: (id: string) => void; onCreate: () => void; tileMin?: number;
  } = $props();
</script>
{#if !instances.length}
  <div class="empty-state"><h2>Your OMP workspace</h2><p>Create an independent instance or reopen a saved session.</p><button class="primary" onclick={onCreate}>New instance</button></div>
{:else}
  <div class="grid" style={`--tile-min:${tileMin}px`}>
    {#each instances as instance (instance.id)}<Tile {instance} {onSelect} {onPlan} {onSubagents} />{/each}
  </div>
{/if}
<style>
  .grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(min(100%, var(--tile-min)), 1fr)); gap: 14px; align-content: start; }
  .empty-state { min-height: 60vh; display: flex; flex-direction: column; justify-content: center; align-items: center; gap: 12px; }
  h2, p { margin: 0; }
</style>
