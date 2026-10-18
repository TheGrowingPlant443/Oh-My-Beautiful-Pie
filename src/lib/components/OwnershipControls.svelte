<script lang="ts">
  import type { Instance } from "../api";
  import { managerInstance, workspaceChanges, handOverToManager, takeBackControl, returnToManager, giveRepoToArchitect } from "../stores";
  import { workspacePending } from "../workspace";
  let { instance, onOpen }: { instance: Instance; onOpen: () => void } = $props();
  let busy = $state(false);
  const changes = $derived($workspaceChanges.filter(change => change.sourceInstanceId === instance.id));
  const managerChange = $derived(changes.filter(change => change.kind === "managerHandover" || change.kind === "managerReturn").at(-1));
  const architectChange = $derived(changes.filter(change => change.kind === "architectRepo").at(-1));
  const managerPending = $derived(!!managerChange && workspacePending(managerChange));
  async function run(command: () => Promise<unknown>): Promise<void> {
    busy = true;
    try { await command(); } finally { busy = false; }
  }
</script>
<div class="ownership">
  {#if instance.kind === "manager"}
    <span class="pill">The Manager</span><span class="dim">{instance.bridgeReady ? "Controller connected" : "Bridge connecting / offline"}</span>
    {#if instance.pendingDecisionCount}<button class="warn" onclick={onOpen}>Needs you · {instance.pendingDecisionCount}</button>{/if}
  {:else if instance.kind === "architect"}
    <span class="pill">The Architect</span>
  {:else if !instance.helper && !instance.planTarget}
    {#if instance.managerId}
      <span class="pill">Managed by The Manager</span>
    {/if}
    {#if instance.managerId || managerPending}
      <button disabled={busy} onclick={() => run(() => takeBackControl(instance.id))}>Take back control</button>
    {:else if $managerInstance}
      <button disabled={busy} onclick={() => run(() => handOverToManager(instance.id))}>Hand over to The Manager</button>
    {/if}
    {#if managerChange}<span class:err={managerChange.status === "failed"} role="status">{managerChange.status === "pending" ? "Pending" : managerChange.status === "applying" ? "Applying" : managerChange.status === "applied" ? "Applied" : managerChange.status === "failed" ? `Failed: ${managerChange.error}` : "Cancelled"}</span>{/if}
    <button disabled={busy} onclick={() => run(() => giveRepoToArchitect(instance.id))}>Give repo to architect</button>
    {#if architectChange}<span class:err={architectChange.status === "failed"} role="status">{architectChange.status === "pending" ? "Pending" : architectChange.status === "applying" ? "Applying" : architectChange.status === "applied" ? "Applied" : architectChange.status === "failed" ? `Failed: ${architectChange.error}` : "Cancelled"}</span>{/if}
    {#if instance.humanRequest}
      <strong class="warn">Needs you</strong><p>{instance.humanRequest.reason}</p>
      <button onclick={onOpen}>Open session</button><button disabled={busy || managerPending || !instance.managerId} onclick={() => { const request = instance.humanRequest; if (request) void run(() => returnToManager(instance.id, request.id)); }}>Return to The Manager</button>
    {:else if instance.pendingDecisionCount}<span class="dim">{instance.pendingDecisionCount} native decision(s) pending</span>{/if}
  {/if}
</div>
<style>
  .ownership { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; font-size: 12px; }
  button { font-size: 12px; }
  p { flex-basis: 100%; margin: 0; color: var(--text-dim); }
</style>
