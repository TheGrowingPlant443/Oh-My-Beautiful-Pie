<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "../api";
  import type { DiffContent, DiffFile, DiffOverview } from "../api";
  import { errorText } from "../stores";
  import DiffFileTree from "./DiffFileTree.svelte";
  import CodeDiffEditor from "./CodeDiffEditor.svelte";
  let { instanceId, base, target, onBack }: { instanceId: string; base: string | null; target: string; onBack: () => void } = $props();
  let overview = $state<DiffOverview | null>(null);
  let content = $state<DiffContent | null>(null);
  let selected = $state<{ path: string; group: null } | null>(null);
  let collapsed = $state<ReadonlySet<string>>(new Set());
  let loading = $state(true);
  let loadingFile = $state(false);
  let error = $state("");
  let contentKey = $state(0);
  let sequence = 0;
  let disposed = false;
  const title = $derived(base ? `${overview?.baseLabel ?? base.slice(0, 7)} → ${content?.modifiedLabel ?? target.slice(0, 7)}` : `Commit ${content?.modifiedLabel ?? target.slice(0, 7)}`);
  async function select(file: DiffFile): Promise<void> {
    if (!overview || loadingFile) return;
    const seq = ++sequence;
    selected = { path: file.path, group: null };
    loadingFile = true; error = ""; content = null;
    try {
      const result = await api.readCodeDiffFile(instanceId, overview.snapshotId, file.path, null);
      if (disposed || seq !== sequence) return;
      content = result; contentKey++;
    } catch (e) { if (!disposed && seq === sequence) error = errorText(e); }
    finally { if (!disposed && seq === sequence) loadingFile = false; }
  }
  function toggle(key: string): void {
    const next = new Set(collapsed);
    if (!next.delete(key)) next.add(key);
    collapsed = next;
  }
  onMount(() => {
    void api.getCommitDiff(instanceId, base, target).then(async result => {
      if (disposed) return;
      overview = result;
      if (result.files[0]) await select(result.files[0]);
    }).catch(e => { if (!disposed) error = errorText(e); }).finally(() => { if (!disposed) loading = false; });
    return () => { disposed = true; sequence++; };
  });
</script>
<section class="commit-diff">
  <header class="bar"><button onclick={onBack}>Back to Git tree</button><h2 title={title}>{title}</h2></header>
  {#if error}<p class="alert err" role="alert">{error}</p>{/if}
  <div class="body">
    <aside aria-label="Commit files">
      <DiffFileTree files={overview?.files ?? []} prefix="" {selected} {collapsed} disabled={loadingFile} onSelect={select} onToggle={toggle} onAction={() => {}} />
    </aside>
    <div class="main">
      {#if content}
        <div class="labels"><span>{content.originalLabel}</span><span>{content.modifiedLabel}</span></div>
        <p class="note">{content.readOnlyReason}</p>
        {#if content.original !== null && content.modified !== null}
          {#key contentKey}<div class="editor-slot"><CodeDiffEditor {content} onChange={() => {}} onSave={() => {}} /></div>{/key}
        {:else}<p class="empty">No comparison can be shown for this file.</p>{/if}
      {:else if loading || loadingFile}<p role="status">Loading commit changes…</p>
      {:else if overview && !overview.files.length}<p class="empty">No file changes in this commit.</p>{/if}
    </div>
  </div>
</section>
<style>
  .commit-diff { flex: 1; height: 100%; min-height: 0; display: flex; flex-direction: column; gap: 10px; }
  .bar { display: flex; align-items: center; gap: 10px; min-width: 0; }
  h2 { margin: 0; font-size: 16px; overflow: hidden; white-space: nowrap; text-overflow: ellipsis; }
  .body { flex: 1; min-height: 0; display: flex; gap: 10px; }
  aside { flex: 0 0 260px; min-width: 180px; max-width: 35%; overflow: auto; padding: 8px; border: 1px solid var(--line); border-radius: 8px; background: var(--panel); }
  .main { flex: 1; min-width: 0; min-height: 0; display: flex; flex-direction: column; gap: 6px; }
  .labels { display: flex; justify-content: space-between; gap: 12px; font-size: 12px; color: var(--text-dim); }
  .note { margin: 0; color: var(--text-dim); font-size: 12px; }
  .editor-slot { flex: 1; min-height: 0; border: 1px solid var(--line); overflow: hidden; border-radius: 8px; }
  .alert { margin: 0; padding: 10px; border: 1px solid var(--err); white-space: pre-wrap; }
</style>
