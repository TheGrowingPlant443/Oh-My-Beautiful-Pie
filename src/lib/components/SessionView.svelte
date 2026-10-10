<script lang="ts">
  import type { Instance } from "../api";
  import Term from "./Term.svelte";
  import { restartInstance, startInstance, stopInstance } from "../stores";
  let { instance, instances, onSwitch, onClose, onPlan, onSubagents }: {
    instance: Instance; instances: Instance[]; onSwitch: (id: string) => void;
    onClose: () => void; onPlan: (id: string) => void; onSubagents: (id: string) => void;
  } = $props();
  let busy = $state(false);
  const live = $derived(instance.status === "running" || instance.status === "starting");
  async function run(action: () => Promise<boolean>): Promise<void> {
    busy = true;
    try { await action(); } finally { busy = false; }
  }
</script>
<div class="view">
  <header class="panel topbar">
    <button onclick={onClose}>Dashboard</button>
    <select aria-label="Switch session" value={instance.id} onchange={e => onSwitch(e.currentTarget.value)}>
      {#each instances as row (row.id)}<option value={row.id}>{row.name} · {row.status}</option>{/each}
    </select>
    <span class="pill">{live ? instance.activity : instance.status}</span>
    <span class="dim">{instance.profile || "Default profile"}</span><span class="spacer"></span>
    <button disabled={!instance.subagents.length} onclick={() => onSubagents(instance.id)}>Subagents: {instance.subagents.length}</button>
    {#if instance.planPath}<button onclick={() => onPlan(instance.id)}>View Plan</button>{/if}
    <button disabled={busy} onclick={() => run(() => live ? stopInstance(instance.id) : startInstance(instance.id))}>{live ? "Stop" : "Start / reopen"}</button>
    <button disabled={busy} onclick={() => run(() => restartInstance(instance.id))}>Restart</button>
  </header>
  <p class="identity mono">{instance.projectPath}{instance.task ? ` · ${instance.task}` : ""}</p>
  {#if instance.lastError}<p class="err" role="alert">{instance.lastError}</p>{/if}
  <div class="screen">
    {#if instance.generation > 0}<Term instanceId={instance.id} interactive cols={instance.cols} rows={instance.rows} generation={instance.generation} scrollback={8000} />
    {:else}<p class="empty">Start this instance to open its native OMP terminal.</p>{/if}
  </div>
</div>
<style>
  .view { height: 100%; display: flex; flex-direction: column; gap: 8px; min-height: 0; }
  .topbar { flex-direction: row; align-items: center; flex-wrap: wrap; padding: 10px; gap: 8px; }
  select { width: auto; max-width: 35ch; }
  .identity { font-size: 12px; color: var(--text-dim); margin: 0; overflow-wrap: anywhere; }
  .screen { flex: 1; min-height: 0; background: #0b1017; }
</style>
