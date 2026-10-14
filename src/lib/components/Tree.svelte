<script lang="ts">
  import type { Instance } from "../api";
  import { startInstance, stopInstance } from "../stores";
  import { basename } from "../ui";
  import OwnershipControls from "./OwnershipControls.svelte";
  let { list, onSelect, onSubagent, onPlan, onPlanChange }: { list: Instance[]; onSelect: (id: string) => void; onSubagent: (id: string, child: string) => void; onPlan: (id: string) => void; onPlanChange: (id: string) => void } = $props();
  const groups = $derived.by(() => {
    const folders = new Map<string, Instance[]>();
    for (const instance of list) {
      const group = folders.get(instance.projectPath) || [];
      group.push(instance);
      folders.set(instance.projectPath, group);
    }
    return [...folders.entries()];
  });
</script>
<div class="tree">
  {#if !list.length}<p>No instances yet. Use New instance to get started.</p>{/if}
  {#each groups as [path, entries] (path)}
    <section class="panel">
      <header title={path}><strong>{basename(path)}</strong><span class="dim mono">{path}</span></header>
      <ul>{#each entries as instance (instance.id)}
        <li>
          <div class="row">
            <button class="name" onclick={() => onSelect(instance.id)}>{instance.name}</button>
            <span class="pill">{instance.status === "running" ? instance.activity : instance.status}</span>
            <span class="dim">{instance.profile || "Default profile"}</span>
            <span class="spacer"></span>
            {#if instance.planKind}<button onclick={() => onPlanChange(instance.id)}>Plan</button>{/if}
            {#if instance.planPath}<button onclick={() => onPlan(instance.id)}>View Plan</button>{/if}
            <button onclick={() => instance.status === "running" || instance.status === "starting" ? stopInstance(instance.id) : startInstance(instance.id)}>{instance.status === "running" || instance.status === "starting" ? "Stop" : "Start / reopen"}</button>
          </div>
          <OwnershipControls {instance} onOpen={() => onSelect(instance.id)} />
          {#if instance.task}<p>{instance.task}</p>{/if}
          <ul class="subagents">{#each instance.subagents as child (child.id)}
            <li><button onclick={() => onSubagent(instance.id, child.id)}>{child.name}</button><span class="dim">{child.agent || "Subagent"} · {child.status}</span></li>
          {/each}</ul>
        </li>
      {/each}</ul>
    </section>
  {/each}
</div>
<style>
  .tree { display: flex; flex-direction: column; gap: 14px; }
  header { gap: 12px; flex-wrap: wrap; }
  ul { list-style: none; padding: 0 16px; margin: 0; }
  li { padding: 8px 0; }
  .subagents { margin-left: 20px; border-left: 1px solid var(--line); }
  .subagents li { display: flex; gap: 12px; align-items: center; }
  .name { font-weight: 600; }
  .row { flex-wrap: wrap; }
  p { margin: 8px; color: var(--text-dim); }
</style>
