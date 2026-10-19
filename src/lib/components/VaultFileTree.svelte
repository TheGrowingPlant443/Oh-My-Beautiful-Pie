<script lang="ts">
  import { treeRows, type VaultState } from "../vault-documents";
  import type { VaultEntry } from "../knowledge";

  // Explorer of one vault. Folder rows share the diff tree's look; there are no Git statuses, staging or revert actions here.
  let { state, selected, onOpen, onToggle, onMore, onRetry }: {
    state: Pick<VaultState, "listings" | "expanded">; selected: string | null;
    onOpen: (entry: VaultEntry) => void; onToggle: (folder: string) => void; onMore: (prefix: string) => void; onRetry: (prefix: string) => void;
  } = $props();
  const rows = $derived(treeRows(state));
</script>
<ul class="tree" aria-label="Vault files">
  {#each rows as row, i (row.kind === "folder" ? `f:${row.path}` : row.kind === "entry" ? `e:${row.entry.path}` : `s:${row.prefix}:${i}`)}
    <li style={`padding-left: ${row.depth * 14}px`}>
      {#if row.kind === "folder"}
        <button class="folder-row" aria-expanded={row.expanded} onclick={() => onToggle(row.path)}>
          <span class="chevron" aria-hidden="true">{row.expanded ? "▾" : "▸"}</span><span class="folder-name">{row.name}</span>
        </button>
      {:else if row.kind === "entry"}
        {@const entry = row.entry}
        {@const active = selected === entry.path}
        <div class="row" class:active>
          <button class="file" aria-current={active ? "true" : undefined} class:inert={entry.kind !== "markdown"}
            title={entry.kind === "markdown" ? entry.path : `${entry.path} — Not a Markdown vault document`} onclick={() => onOpen(entry)}>
            <span class="name">{entry.name}</span>
            {#if entry.kind !== "markdown"}<small class="dim">not a note</small>{/if}
          </button>
        </div>
      {:else if row.more}
        <button class="more" onclick={() => onMore(row.prefix)}>{row.text}</button>
      {:else if row.error}
        <p class="status err" role="alert">{row.text} <button onclick={() => onRetry(row.prefix)}>Retry</button></p>
      {:else}
        <p class="status dim" role="status">{row.text}</p>
      {/if}
    </li>
  {/each}
</ul>
<style>
  .tree { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 1px; }
  .folder-row { width: 100%; display: flex; gap: 6px; align-items: center; text-align: left; border-color: transparent; background: transparent; padding: 3px 6px; }
  .chevron { width: 1em; flex: none; color: var(--text-dim); }
  .folder-name { flex: 1; overflow-wrap: anywhere; font-size: 13px; }
  .row { display: flex; align-items: center; border: 1px solid transparent; border-radius: 6px; }
  .row.active { background: #22406b; border-color: var(--accent); }
  .file { flex: 1; min-width: 0; display: flex; gap: 8px; align-items: baseline; text-align: left; border-color: transparent; background: transparent; padding: 3px 6px; }
  .file.inert { color: var(--text-dim); }
  .name { overflow-wrap: anywhere; font-size: 13px; }
  .more { font-size: 12px; margin-left: 6px; }
  .status { margin: 2px 6px; font-size: 12px; overflow-wrap: anywhere; }
  .dim { color: var(--text-dim); }
  .err { color: var(--err); }
</style>
