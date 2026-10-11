<script lang="ts">
  import { onMount } from "svelte";
  import { api, type SetupDraft, type SetupAdmission } from "./lib/api";
  import { connectInstances, instances, instancesById, previewsEnabled, toasts, dismissToast, reportError, managerInstance, showManagerTile, startInstance } from "./lib/stores";
  import Grid from "./lib/components/Grid.svelte";
  import Tree from "./lib/components/Tree.svelte";
  import SessionView from "./lib/components/SessionView.svelte";
  import NewInstanceDialog from "./lib/components/NewInstanceDialog.svelte";
  import PlanPanel from "./lib/components/PlanPanel.svelte";
  import SubagentPanel from "./lib/components/SubagentPanel.svelte";
  import GlobalConfigDialog from "./lib/components/GlobalConfigDialog.svelte";
  import AgentSetupPage from "./lib/components/AgentSetupPage.svelte";
  let view = $state<"dashboard" | "agent_setup">("dashboard");
  let configOpen = $state(false);
  let configMode = $state<"save" | "import">("save");
  let setupDraft = $state<SetupDraft>({ scope: null, items: [], step: 0, instanceId: null });
  let setupAdmission = $state<SetupAdmission | null>(null);
  let returnToSetup = $state(false);
  let focusedId = $state<string | null>(null);
  let dialogOpen = $state(false);
  let dialogPurpose = $state<"instance" | "manager">("instance");
  let openingManager = $state(false);
  let tileMin = $state(420);
  let panel = $state<{ kind: "plan" | "subagents"; id: string; child?: string } | null>(null);
  const focused = $derived(focusedId ? $instancesById.get(focusedId) : undefined);
  const panelOwner = $derived(panel ? $instancesById.get(panel.id) : undefined);
  const overview = $derived($instances.filter(instance => instance.kind !== "manager" || $showManagerTile));
  const needsHuman = $derived(!!$managerInstance?.pendingDecisionCount || $instances.some(instance => !!instance.humanRequest));
  function newInstance(): void { returnToSetup = false; dialogPurpose = "instance"; dialogOpen = true; }
  function configureManager(): void { returnToSetup = true; dialogPurpose = "manager"; dialogOpen = true; }
  function created(id: string): void {
    if (returnToSetup) { returnToSetup = false; view = "agent_setup"; }
    else focus(id);
  }
  function openConfig(mode: "save" | "import"): void { configMode = mode; configOpen = true; }
  async function openManager(): Promise<void> {
    const manager = $managerInstance;
    if (!manager) { returnToSetup = false; dialogPurpose = "manager"; dialogOpen = true; return; }
    openingManager = true;
    try {
      if (manager.status === "stopped" || manager.status === "error") await startInstance(manager.id);
      focus(manager.id);
    } finally { openingManager = false; }
  }
  function focus(id: string | null): void {
    if (focused && focused.generation > 0) {
      void api.resizePty(focused.id, 120, 32).catch(error => reportError("Restore preview dimensions", error));
    }
    view = "dashboard";
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
    <button aria-label="The Manager" disabled={openingManager} onclick={openManager}>The Manager{#if needsHuman}<span class="pill warn">Needs you</span>{/if}</button>
    {#if $managerInstance}<label class="manager-toggle"><input type="checkbox" bind:checked={$showManagerTile} />Show Manager tile</label>{/if}
    <button onclick={() => openConfig("save")}>Save as global config</button>
    <button onclick={() => openConfig("import")}>Import config</button>
    <button class:active={view === "agent_setup"} onclick={() => view = "agent_setup"}>Agent setup</button>
    <button class="primary" onclick={newInstance}>New instance</button>
  </header>
  <main class:focused={view === "dashboard" && !!focused}>
    {#if view === "agent_setup"}
      <AgentSetupPage bind:draft={setupDraft} bind:admission={setupAdmission} onback={() => view = "dashboard"} onSetupManager={configureManager} onOpenManager={openManager} onSelectSession={focus} {onPlan} {onSubagents} />
    {:else if focused}
      <SessionView instance={focused} instances={$instances} onSwitch={focus} onClose={() => focus(null)} {onPlan} {onSubagents} />
    {:else if $previewsEnabled}
      <Grid instances={overview} onSelect={focus} {onPlan} {onSubagents} {tileMin} onCreate={newInstance} />
    {:else}
      <Tree list={overview} onSelect={focus} {onSubagent} {onPlan} />
    {/if}
  </main>
</div>
<NewInstanceDialog open={dialogOpen} purpose={dialogPurpose} onclose={() => dialogOpen = false} oncreated={created} />
<GlobalConfigDialog open={configOpen} mode={configMode} initialSourceId={focused?.id || $managerInstance?.id || null} onclose={() => configOpen = false} />
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
  .manager-toggle { display: flex; align-items: center; gap: 6px; font-size: 12px; }
  .manager-toggle input { width: auto; }
</style>
