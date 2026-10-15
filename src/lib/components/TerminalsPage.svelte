<script lang="ts">
  import TerminalWorkspace from "./TerminalWorkspace.svelte";

  let { onBack }: { onBack: () => void } = $props();
</script>

<div class="page">
  <header class="panel topbar">
    <button type="button" onclick={onBack}>Back to Dashboard</button>
    <h3>Terminals</h3>
    <span class="dim">Every agent's command history, live AI commands and user terminals</span>
  </header>
  <div class="content"><TerminalWorkspace instanceId={null} /></div>
</div>

<style>
  .page { height: 100%; display: flex; flex-direction: column; gap: 8px; min-height: 0; }
  .topbar { flex-direction: row; align-items: center; flex-wrap: wrap; padding: 10px; gap: 10px; }
  h3 { margin: 0; }
  .content { flex: 1; min-height: 0; }
</style>
