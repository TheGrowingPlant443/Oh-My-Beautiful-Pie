<script lang="ts">
  import type { StructureEntryPayload } from "../api";
  import {
    citationTexts, entryForGroup, entryForPath, mappingsForProposed, mappingText, OPERATION_LABEL,
    type EntryLookup, type GeneralDir, type GeneralNode, type MappingIndex, type ProposedNode,
  } from "../structure-view";
  // Renders either the generalized current tree (`general`) or the proposed tree (`proposed`). Every string is plain text.
  let { general = null, proposed = null, lookup, mappings }: {
    general?: GeneralDir | null; proposed?: ProposedNode[] | null; lookup: EntryLookup; mappings: MappingIndex;
  } = $props();
  const PAGE = 200;
  let open = $state<Record<string, boolean>>({});
  let shown = $state<Record<string, number>>({});
  const isOpen = (key: string, fallback = false): boolean => open[key] ?? fallback;
  const toggle = (key: string, fallback = false): void => { open[key] = !isOpen(key, fallback); };
  const limit = (key: string): number => shown[key] ?? PAGE;
  const nodeKey = (node: GeneralNode): string => node.kind === "group" ? `g:${node.id}` : `${node.kind}:${node.path}`;
  const groupEntry = (node: Extract<GeneralNode, { kind: "group" }>): StructureEntryPayload | null => lookup.byId.size ? entryForGroup(lookup, node) : null;
</script>

{#snippet note(entry: StructureEntryPayload | null)}
  {#if entry}
    {#if entry.purpose}<span class="purpose">{entry.purpose}</span>{/if}
    {#if entry.evidenceIds.length}<small class="cite">Evidence: {citationTexts(lookup, entry.evidenceIds).join("; ")}</small>{/if}
  {/if}
{/snippet}

{#snippet list(key: string, items: GeneralNode[], depth: number)}
  <ul class="tree">
    {#each items.slice(0, limit(key)) as child (nodeKey(child))}
      {@render current(child, depth)}
    {/each}
    {#if items.length > limit(key)}
      <li style:padding-left="{depth * 14}px"><button class="more" onclick={() => { shown[key] = items.length; }}>Show all {items.length} entries ({items.length - limit(key)} more)</button></li>
    {/if}
  </ul>
{/snippet}

{#snippet current(node: GeneralNode, depth: number)}
  {#if node.kind === "file"}
    <li class="row" style:padding-left="{depth * 14 + 18}px"><span class="name">{node.name}</span>{@render note(lookup.byId.size ? entryForPath(lookup, node.path) : null)}</li>
  {:else if node.kind === "dir"}
    {@const key = nodeKey(node)}
    <li>
      <button class="row toggle" style:padding-left="{depth * 14}px" aria-expanded={isOpen(key)} onclick={() => toggle(key)}>
        <span class="chev" aria-hidden="true">{isOpen(key) ? "▾" : "▸"}</span><span class="name">{node.name}/</span>
        <span class="count">{node.fileCount} files</span>{@render note(lookup.byId.size ? entryForPath(lookup, node.path) : null)}
      </button>
      {#if isOpen(key)}{@render list(key, node.children, depth + 1)}{/if}
    </li>
  {:else}
    {@const key = nodeKey(node)}
    <li>
      <button class="row toggle group" style:padding-left="{depth * 14}px" aria-expanded={isOpen(key)} onclick={() => toggle(key)}>
        <span class="chev" aria-hidden="true">{isOpen(key) ? "▾" : "▸"}</span><span class="name">{node.label}</span><code class="pattern">{node.pattern}</code>
        <span class="count">{node.fileCount} files{#if node.dirCount} · {node.dirCount} directories{/if}</span>{@render note(groupEntry(node))}
      </button>
      {#if isOpen(key)}{@render list(key, node.nodes, depth + 1)}{/if}
    </li>
  {/if}
{/snippet}

{#snippet target(node: ProposedNode, depth: number)}
  {@const key = `p:${node.id}`}
  {@const found = mappingsForProposed(mappings, node)}
  {@const expandable = node.children.length > 0 || node.members.length > 0}
  <li>
    <div class="row" style:padding-left="{depth * 14}px">
      {#if expandable}
        <button class="chevbtn" aria-expanded={isOpen(key, depth === 0)} aria-label={`Toggle ${node.label}`} onclick={() => toggle(key, depth === 0)}>{isOpen(key, depth === 0) ? "▾" : "▸"}</button>
      {:else}<span class="chev"></span>{/if}
      <span class="name">{node.label}</span>
      {#if node.path !== null}<code class="pattern">{node.path}</code>{/if}
      {#if node.pattern}<code class="pattern">{node.pattern}</code>{/if}
      {#each found as mapping (mapping)}<span class="op {mapping.operation}" title={mappingText(mapping)}>{OPERATION_LABEL[mapping.operation]}</span>{/each}
      {#if node.cycleBroken}<span class="op remove">parent cycle cut</span>{/if}
      {#if node.purpose}<span class="purpose">{node.purpose}</span>{/if}
    </div>
    {#each found as mapping (mapping)}
      <div class="mapping" style:padding-left="{depth * 14 + 18}px">{mappingText(mapping)}{#if mapping.rationale} — {mapping.rationale}{/if}</div>
    {/each}
    {#if isOpen(key, depth === 0)}
      {#if node.members.length}
        <ul class="members" style:padding-left="{depth * 14 + 18}px" aria-label={`Members of ${node.label}`}>
          {#each node.members as member}<li>{member}</li>{/each}
        </ul>
      {/if}
      {#if node.children.length}
        <ul class="tree">{#each node.children as child (child.id)}{@render target(child, depth + 1)}{/each}</ul>
      {/if}
    {/if}
  </li>
{/snippet}

{#if general}{@render list("root", general.children, 0)}{/if}
{#if proposed}
  <ul class="tree">{#each proposed as root (root.id)}{@render target(root, 0)}{/each}</ul>
{/if}

<style>
  .tree, .members { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 1px; }
  .members { font: 12px var(--mono); color: var(--text-dim); margin: 2px 0 4px; }
  .members li { overflow-wrap: anywhere; }
  .row { display: flex; flex-wrap: wrap; align-items: baseline; gap: 4px 8px; font-size: 13px; padding-top: 2px; padding-bottom: 2px; }
  button.row { width: 100%; text-align: left; background: transparent; border-color: transparent; padding-right: 6px; }
  .chev { display: inline-block; width: 1em; flex: none; color: var(--text-dim); }
  .chevbtn { padding: 0 4px; background: transparent; border-color: transparent; color: var(--text-dim); }
  .name { overflow-wrap: anywhere; }
  .count { color: var(--text-dim); font-size: 11px; }
  .pattern { font: 11px var(--mono); color: var(--accent); overflow-wrap: anywhere; }
  .purpose { color: var(--text-dim); font-size: 12px; overflow-wrap: anywhere; }
  .cite { color: var(--text-faint); font: 11px var(--mono); overflow-wrap: anywhere; }
  .mapping { color: var(--text-dim); font-size: 12px; overflow-wrap: anywhere; }
  .more { font-size: 12px; }
  .op { font-size: 11px; padding: 0 6px; border: 1px solid var(--line); border-radius: 999px; }
  .op.unchanged { color: var(--text-dim); }
  .op.rename, .op.move { color: var(--accent); border-color: var(--accent); }
  .op.split, .op.merge { color: var(--accent-2); border-color: var(--accent-2); }
  .op.create { color: var(--ok); border-color: var(--ok); }
  .op.remove { color: var(--err); border-color: var(--err); }
</style>
