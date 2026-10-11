<script lang="ts">
  import type { Instance } from "../api";
  import { managerInstance, setManagerOwner, returnToManager } from "../stores";
  let { instance, onOpen }: { instance: Instance; onOpen: () => void } = $props();
  let busy = $state(false);
  async function handover(owner: string | null): Promise<void> {
    busy = true;
    try { await setManagerOwner(instance.id, owner); } finally { busy = false; }
  }
  async function resume(): Promise<void> {
    busy = true;
    try { await returnToManager(instance.id); } finally { busy = false; }
  }
</script>
<div class="ownership">
  {#if instance.kind === "manager"}
    <span class="pill">The Manager</span><span class="dim">{instance.bridgeReady ? "Controller connected" : "Bridge connecting / offline"}</span>
    {#if instance.pendingDecisionCount}<button class="warn" onclick={onOpen}>Needs you · {instance.pendingDecisionCount}</button>{/if}
  {:else}
    {#if instance.managerId}
      <span class="pill">Managed by The Manager</span>
      <button disabled={busy} onclick={() => handover(null)}>Take back control</button>
    {:else if $managerInstance}
      <button disabled={busy} onclick={() => handover($managerInstance?.id ?? null)}>Hand over to The Manager</button>
    {/if}
    {#if instance.humanRequest}
      <strong class="warn">Needs you</strong><p>{instance.humanRequest.reason}</p>
      <button onclick={onOpen}>Open session</button><button disabled={busy} onclick={resume}>Return to The Manager</button>
    {:else if instance.pendingDecisionCount}<span class="dim">{instance.pendingDecisionCount} native decision(s) pending</span>{/if}
  {/if}
</div>
<style>
  .ownership { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; font-size: 12px; }
  button { font-size: 12px; }
  p { flex-basis: 100%; margin: 0; color: var(--text-dim); }
</style>
