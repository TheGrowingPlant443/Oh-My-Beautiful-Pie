<script lang="ts">
  import type { Instance } from "../api";
  import Term from "./Term.svelte";
  import OwnershipControls from "./OwnershipControls.svelte";
  import { removeInstance, restartInstance, startInstance, stopInstance } from "../stores";
  import { basename, confirmDanger, statusClass } from "../ui";
  import AgentDirectory from "./AgentDirectory.svelte";
  import { nativeHubDisabled } from "../native-agent-hub";
  let { instance, onSelect, onOpen, onPlan, onPlanChange, onSubagents }: { instance: Instance; onSelect: (id: string) => void; onOpen: (id: string) => Promise<void>; onPlan: (id: string) => void; onPlanChange: (id: string) => void; onSubagents: (id: string) => void } = $props();
  let busy = $state(false);
  const live = $derived(instance.status === "running" || instance.status === "starting");
  async function run(action: () => Promise<boolean>): Promise<void> {
    busy = true;
    try { await action(); } finally { busy = false; }
  }
  async function remove(): Promise<void> {
    if (await confirmDanger(`Remove '${instance.name}' from the dashboard? OMP history is kept.`)) await run(() => removeInstance(instance.id));
  }
</script>
<article class="tile panel" data-id={instance.id}>
  <header>
    <button class="name" title={instance.projectPath} onclick={() => onSelect(instance.id)}>{instance.name}</button>
    <span class="spacer"></span><span class="pill {statusClass(instance.status)}">{live ? instance.activity : instance.status}{instance.exitCode !== null ? ` (${instance.exitCode})` : ""}</span>
  </header>
  <div class="metadata">
    {#if !instance.helper && !instance.planTarget}<AgentDirectory {instance} compact />{:else}<strong title={instance.projectPath}>{basename(instance.projectPath)}</strong>{/if}
    <span>{instance.profile || "Default profile"} · {instance.model || "OMP model setup"}</span>
    {#if instance.configs.length}<span title={instance.configs.join("\n")}>{instance.configs.length} config overlay(s)</span>{/if}
    <p>{instance.task || "No active todo task"}{instance.todoTotal ? ` · ${instance.todoDone}/${instance.todoTotal}` : ""}</p>
    <OwnershipControls {instance} onOpen={() => onSelect(instance.id)} />
  </div>
  <div class="row links">
    <button disabled={busy || nativeHubDisabled(instance)} title={instance.bridgeReady && !instance.capabilities?.agentHub ? "Reopen this session with a compatible OMP version." : "Open this session's native Agent Hub"} onclick={() => onSubagents(instance.id)}>Subagents: {instance.subagents.length}</button>
    {#if instance.planKind}<button onclick={() => onPlanChange(instance.id)}>Plan</button>{/if}
    {#if instance.planPath}<button onclick={() => onPlan(instance.id)}>View Plan</button>{/if}
  </div>
  <button class="preview" aria-label={`Open ${instance.name}`} onclick={() => void onOpen(instance.id)}>
    {#if instance.generation > 0}
      <Term instanceId={instance.id} cols={instance.cols} rows={instance.rows} generation={instance.generation} />
    {:else}<span>Start or reopen this OMP session</span>{/if}
  </button>
  {#if instance.lastError}<p class="err" role="alert">{instance.lastError}</p>{/if}
  <footer>
    <button disabled={busy} onclick={() => run(() => live ? stopInstance(instance.id) : startInstance(instance.id))}>{live ? "Stop" : instance.sessionPath ? "Reopen" : "Start"}</button>
    <button disabled={busy} onclick={() => run(() => restartInstance(instance.id))}>Restart</button>
    <span class="spacer"></span><button class="ghost" disabled={busy} onclick={remove}>Remove</button>
  </footer>
</article>
<style>
  .tile { min-width: 0; }
  .name { font-weight: 650; font-size: 15px; background: none; border: none; text-align: left; }
  .metadata { display: flex; flex-direction: column; gap: 5px; padding: 8px 12px; color: var(--text-dim); font-size: 12px; }
  .metadata strong { color: var(--text); }
  p { margin: 0; }
  .links { padding: 8px 12px; }
  .preview { width: 100%; min-height: 180px; padding: 0; background: #0b1017; border: none; border-radius: 0; overflow: hidden; text-align: left; }
  .preview :global(.viewport) { pointer-events: none; }
  .preview span { display: block; padding: 40px; text-align: center; color: var(--text-dim); }
  footer { padding: 10px; }
  .err { padding: 8px 12px; }
</style>
