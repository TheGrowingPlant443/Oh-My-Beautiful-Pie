<script lang="ts">
  import type { DiffFile, DiffFileAction, DiffGroup } from "../api";
  import { buildTree, visibleRows } from "../diff-tree";
  let { files, prefix, selected, collapsed, disabled, onSelect, onToggle, onAction }: {
    files: DiffFile[]; prefix: string; selected: { group: DiffGroup | null; path: string } | null;
    collapsed: ReadonlySet<string>; disabled: boolean;
    onSelect: (file: DiffFile) => void; onToggle: (key: string) => void; onAction: (file: DiffFile, action: DiffFileAction) => void;
  } = $props();
  const STATUS_NAMES: Record<string, string> = { M: "Modified", A: "Added", D: "Deleted", R: "Renamed", C: "Copied", T: "Type changed", U: "Unmerged", "?": "Untracked" };
  // Yellow: changed, green: new, red: deleted (or conflicted).
  const NAME_TONE: Record<string, string> = { M: "changed", T: "changed", R: "changed", A: "added", "?": "added", C: "added", D: "deleted", U: "deleted" };
  const rows = $derived(visibleRows(buildTree(files, prefix), collapsed));
</script>
<ul class="tree">
  {#each rows as row (row.kind === "folder" ? row.key : `${row.file.group ?? ""}:${row.file.path}`)}
    <li style={`padding-left: ${row.depth * 14}px`}>
      {#if row.kind === "folder"}
        <button class="folder-row" aria-expanded={!collapsed.has(row.key)} onclick={() => onToggle(row.key)}>
          <span class="chevron" aria-hidden="true">{collapsed.has(row.key) ? "▸" : "▾"}</span><span class="folder-name">{row.name}</span><span class="count dim">{row.count}</span>
        </button>
      {:else}
        {@const file = row.file}
        {@const active = !!selected && selected.path === file.path && selected.group === file.group}
        <div class="row" class:active>
          <button class="file" aria-current={active ? "true" : undefined} disabled={disabled && !active} title={file.oldPath ? `${file.oldPath} → ${file.path}` : file.path} onclick={() => onSelect(file)}>
            <span class={`status ${file.status === "?" ? "untracked" : file.status.toLowerCase()}`} title={STATUS_NAMES[file.status] ?? file.status}>{file.status}</span>
            <span class={`name tone-${NAME_TONE[file.status] ?? ""}`}>{row.name}{#if file.oldPath}<small class="dim"> ← {file.oldPath}</small>{/if}</span>
          </button>
          {#if file.group === "staged"}
            <span class="actions">
              <button class="icon" disabled={disabled} aria-label={`Unstage ${file.path}`} title="Unstage" onclick={() => onAction(file, "unstage")}>
                <svg aria-hidden="true" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"><path d="M3 8h10" /></svg>
              </button>
            </span>
          {:else if file.group === "changes"}
            <span class="actions">
              <button class="icon" disabled={disabled} aria-label={`Stage ${file.path}`} title="Stage" onclick={() => onAction(file, "stage")}>
                <svg aria-hidden="true" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"><path d="M8 3v10M3 8h10" /></svg>
              </button>
              {#if file.status !== "U" && file.status !== "A"}
                <button class="icon" disabled={disabled} aria-label={`Revert ${file.path}`} title={file.status === "?" ? "Delete untracked file" : "Discard changes"} onclick={() => onAction(file, "revert")}>
                  <svg aria-hidden="true" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M5.5 2.5 2.5 5.5l3 3M2.5 5.5H9a4 4 0 0 1 0 8H5" /></svg>
                </button>
              {/if}
            </span>
          {/if}
        </div>
      {/if}
    </li>
  {/each}
</ul>
<style>
  .tree { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 1px; }
  .folder-row { width: 100%; display: flex; gap: 6px; align-items: center; text-align: left; border-color: transparent; background: transparent; padding: 3px 6px; }
  .chevron { width: 1em; flex: none; color: var(--text-dim); }
  .folder-name { flex: 1; overflow-wrap: anywhere; font-size: 13px; }
  .count { font-size: 11px; }
  .row { display: flex; align-items: center; border: 1px solid transparent; border-radius: 6px; }
  .row.active { background: #22406b; border-color: var(--accent); }
  .file { flex: 1; min-width: 0; display: flex; gap: 8px; align-items: baseline; text-align: left; border-color: transparent; background: transparent; padding: 3px 6px; }
  .name { overflow-wrap: anywhere; font-size: 13px; }
  .name.tone-changed { color: var(--warn); }
  .name.tone-added { color: var(--ok); }
  .name.tone-deleted { color: var(--err); }
  .status { font: 600 11px var(--mono); width: 1.4em; text-align: center; flex: none; }
  .status.m, .status.t { color: var(--warn); }
  .status.a, .status.untracked { color: var(--ok); }
  .status.d, .status.u { color: var(--err); }
  .status.r, .status.c { color: var(--accent); }
  .actions { display: flex; gap: 2px; padding-right: 2px; flex: none; }
  .icon { display: inline-flex; align-items: center; justify-content: center; padding: 3px; border-color: transparent; background: transparent; }
</style>
