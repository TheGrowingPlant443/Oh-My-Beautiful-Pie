<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "./lib/api";
  import { connectInstances, instances, instancesById, previewsEnabled, toasts, dismissToast, reportError } from "./lib/stores";
  import Grid from "./lib/components/Grid.svelte";
  import Tree from "./lib/components/Tree.svelte";
  import SessionView from "./lib/components/SessionView.svelte";
  import NewInstanceDialog from "./lib/components/NewInstanceDialog.svelte";
  import PlanPanel from "./lib/components/PlanPanel.svelte";
  import SubagentPanel from "./lib/components/SubagentPanel.svelte";
  let focusedId = $state<string | null>(null);
  let dialogOpen = $state(false);
  let tileMin = $state(420);
  let panel = $state<{ kind: "plan" | "subagents"; id: string; child?: string } | null>(null);
  const focused = $derived(focusedId ? $instancesById.get(focusedId) : undefined);
  const panelOwner = $derived(panel ? $instancesById.get(panel.id) : undefined);
  function focus(id: string | null): void {
    if (focused && focused.generation > 0) {
      void api.resizePty(focused.id, 120, 32).catch(error => reportError("Restore preview dimensions", error));
    }
    focusedId = id;
  }

  function onPlan(id: string): void { panel = { kind: "plan", id }; }
  function onSubagents(id: string): void { panel = { kind: "subagents", id }; }
  function onSubagent(id: string, child: string): void { panel = { kind: "subagents", id, child }; }
  onMount(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void connectInstances().then(cleanup => { if (disposed) cleanup(); else unlisten = cleanup; }).catch(error => reportError("Connect desktop", error));
    return () => { disposed = true; unlisten?.(); };
  });
</script>
<svelte:window onkeydown={e => { if (e.key === "Escape") { if (panel) panel = null; else if (dialogOpen) dialogOpen = false; } }} />
<div class="app">
  <header class="toolbar">
    <h1>OMP Manager</h1><span class="pill">{$instances.length} instances</span><span class="spacer"></span>
    <button aria-pressed={$previewsEnabled} onclick={() => previewsEnabled.update(value => !value)}>Previews {$previewsEnabled ? "On" : "Off"}</button>
    {#if $previewsEnabled}<select aria-label="Preview size" bind:value={tileMin}><option value={420}>Compact tiles</option><option value={640}>Readable tiles</option><option value={1000}>Large previews</option></select>{/if}
    <button class="primary" onclick={() => dialogOpen = true}>New instance</button>
  </header>
  <main class:focused={!!focused}>
    {#if focused}
      <SessionView instance={focused} instances={$instances} onSwitch={focus} onClose={() => focus(null)} {onPlan} {onSubagents} />
    {:else if $previewsEnabled}
      <Grid instances={$instances} onSelect={focus} {onPlan} {onSubagents} {tileMin} onCreate={() => dialogOpen = true} />
    {:else}
      <Tree list={$instances} onSelect={focus} {onSubagent} {onPlan} />
    {/if}
  </main>
</div>
<NewInstanceDialog open={dialogOpen} onclose={() => dialogOpen = false} oncreated={focus} />
{#if panel && panelOwner}
  <div class="backdrop" role="presentation" onclick={e => { if (e.target === e.currentTarget) panel = null; }}>
    <div class="viewer panel" role="dialog" aria-modal="true" aria-label={panel.kind === "plan" ? "OMP plan" : "Subagent chats"}>
      <header><strong>{panelOwner.name}</strong><span class="spacer"></span><button onclick={() => panel = null}>Close</button></header>
      {#key panelOwner.id}
        {#if panel.kind === "plan"}<PlanPanel instanceId={panelOwner.id} />{:else}<SubagentPanel instance={panelOwner} selectedChild={panel.child || null} />{/if}
      {/key}
    </div>
  </div>
{/if}
<div class="toast-stack">
  {#each $toasts as toast (toast.id)}<div class="toast error" role="alert"><span>{toast.message}</span><button onclick={() => dismissToast(toast.id)}>Dismiss</button></div>{/each}
</div>
<style>
  .app { height: 100vh; display: flex; flex-direction: column; }
  .toolbar { display: flex; align-items: center; padding: 12px 16px; gap: 10px; border-bottom: 1px solid var(--line); background: #101722; flex-wrap: wrap; }
  h1 { font-size: 18px; margin: 0; }
  select { width: auto; }
  main { flex: 1; min-height: 0; padding: 16px; overflow: auto; }
  main.focused { overflow: hidden; }
  .viewer { width: min(1100px, 96vw); height: 90vh; }
  .viewer > :global(section) { flex: 1; min-height: 0; border: none; }
</style>
