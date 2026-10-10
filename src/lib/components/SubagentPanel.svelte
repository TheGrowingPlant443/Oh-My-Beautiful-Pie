<script lang="ts">
  import { untrack } from "svelte";
  import { api, type Instance } from "../api";
  import { errorText } from "../stores";
  let { instance, selectedChild = null }: { instance: Instance; selectedChild?: string | null } = $props();
  let opened = $state<string | null>(null);
  let transcripts = $state<Record<string, string>>({});
  let errors = $state<Record<string, string>>({});
  let pending = $state<Record<string, boolean>>({});
  let epoch = 0;
  const ids = $derived(instance.subagents.map(s => s.id).join("\n"));
  const ownerId = $derived(instance.id);

  async function load(id: string, owner: string, token: number): Promise<void> {
    if (token !== epoch) return;
    pending = { ...pending, [id]: true };
    try {
      const text = await api.renderTranscript(owner, id);
      if (token === epoch) transcripts = { ...transcripts, [id]: text };
    } catch (e) { if (token === epoch) errors = { ...errors, [id]: errorText(e) }; }
    finally { if (token === epoch) pending = { ...pending, [id]: false }; }
  }
  $effect(() => {
    const owner = ownerId;
    const children = ids;
    const token = ++epoch;
    opened = selectedChild;
    transcripts = {};
    errors = {};
    pending = {};
    // Render only this opened panel's subagents, never every dashboard tile.
    untrack(() => {
      void (async () => { for (const child of children.split("\n").filter(Boolean)) await load(child, owner, token); })();
    });
    return () => { epoch++; };
  });
</script>
<section class="panel subagent-panel">
  <header><h3>Subagents · {instance.name}</h3><span class="spacer"></span>{#if opened}<button onclick={() => opened = null}>Back to previews</button>{/if}</header>
  <div class="body chats">
    {#if !instance.subagents.length}<p>No subagent transcripts in this OMP session.</p>{/if}
    {#each instance.subagents as child (child.id)}
      {#if !opened || opened === child.id}
        <article>
          <div class="row"><button class="name" onclick={() => opened = child.id}>{child.name}</button><span class="pill">{child.status}</span><span class="dim">{child.agent || "Subagent"}</span><span class="spacer"></span><button disabled={pending[child.id]} onclick={() => load(child.id, instance.id, epoch)}>Refresh</button></div>
          {#if child.task}<p class="dim">{child.task}</p>{/if}
          {#if errors[child.id]}<p class="err" role="alert">{errors[child.id]}</p>
          {:else if transcripts[child.id] !== undefined}<button class="transcript" class:expanded={opened === child.id} onclick={() => opened = child.id} aria-label={`Open ${child.name} chat`}><pre>{opened === child.id ? transcripts[child.id] : transcripts[child.id].slice(-2200)}</pre></button>
          {:else}<p class="dim">Rendering native OMP transcript…</p>{/if}
        </article>
      {/if}
    {/each}
  </div>
</section>
<style>
  h3, p { margin: 0; }
  h3 { font-size: 14px; }
  .chats { padding: 16px; display: flex; flex-direction: column; gap: 16px; }
  article { min-width: 0; }
  .row { flex-wrap: wrap; }
  .name { font-weight: 650; }
  .transcript { display: block; width: 100%; max-height: 180px; padding: 10px; overflow: auto; text-align: left; background: #0b1017; margin-top: 8px; }
  .transcript.expanded { max-height: none; }
  pre { font: 13px/1.5 var(--mono); white-space: pre-wrap; overflow-wrap: anywhere; margin: 0; }
</style>
