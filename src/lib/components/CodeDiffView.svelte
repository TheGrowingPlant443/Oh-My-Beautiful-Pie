<script lang="ts">
  import { onMount } from "svelte";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { api, type DiffContent, type DiffFile, type DiffFileAction, type DiffGroup, type DiffMode, type DiffOverview, type HelperAdmission } from "../api";
  import { errorText, instancesById, reportError } from "../stores";
  import { renderedHelperAgentId } from "../human-decisions";
  import { confirmDanger } from "../ui";
  import CodeDiffEditor from "./CodeDiffEditor.svelte";
  import DiffFileTree from "./DiffFileTree.svelte";
  import HelperPane from "./HelperPane.svelte";
  type Choice = "save" | "discard" | "cancel";
  type Sel = { group: DiffGroup | null; path: string };
  let { instanceId, name, onBack, onGitTree, onGuardChange, onVisibleAgentChange }: {
    instanceId: string; name: string; onBack: () => void; onGitTree: () => void; onGuardChange: (guard: (() => Promise<boolean>) | null) => void;
    /** Id of the helper conversation this page actually renders, or null. Cleanup reports null on that same callback. */
    onVisibleAgentChange?: (id: string | null) => void;
  } = $props();
  const GROUPS: { group: DiffGroup; label: string }[] = [{ group: "staged", label: "Staged Changes" }, { group: "changes", label: "Changes" }];
  let mode = $state<DiffMode>("head");
  let overview = $state<DiffOverview | null>(null);
  let selected = $state<Sel | null>(null);
  let content = $state<DiffContent | null>(null);
  let contentKey = $state(0);
  let text = $state("");
  let loading = $state(true);
  let fileLoading = $state(false);
  let saving = $state(false);
  let acting = $state(false);
  let error = $state("");
  let conflict = $state(false);
  let notice = $state<{ text: string; prUrl: string | null } | null>(null);
  let collapsed = $state<ReadonlySet<string>>(new Set());
  let message = $state("");
  let branchOn = $state(false);
  let newBranch = $state("");
  let createPr = $state(false);
  let leaveDialog = $state<HTMLDialogElement>();
  let pending: ((choice: Choice) => void) | null = null;
  let overviewSeq = 0;
  let fileSeq = 0;
  const editable = $derived(!!content && content.modified !== null && content.original !== null && !content.readOnlyReason);
  const dirty = $derived(editable && text !== content?.modified);
  const selectedFile = $derived(overview?.files.find(file => file.path === selected?.path && file.group === selected?.group) ?? null);
  const recreating = $derived(!!selectedFile && selectedFile.status === "D" && !!content && content.workingHash === null);
  const busy = $derived(loading || fileLoading || saving || acting);
  const stagedCount = $derived(overview?.files.filter(file => file.group === "staged").length ?? 0);
  const canCommit = $derived(!busy && stagedCount > 0 && message.trim().length > 0);
  const canPublish = $derived(!busy && (branchOn ? newBranch.trim().length > 0 : !!overview?.currentBranch));
  // Explain / Review: a read-only helper agent replaces the diff area. The editor stays mounted behind it so unsaved edits survive.
  let agent = $state<{ id: string; admission: HelperAdmission } | null>(null);
  let agentOpen = $state(false);
  let opening = $state(false);
  let lastRequest: "explain" | "review" = "explain";
  const helper = $derived(agent ? $instancesById.get(agent.id) : undefined);
  // Exactly the render predicate: agentOpen && agent && helper && helper.generation > 0.
  const visibleAgentId = $derived(agentOpen && agent ? renderedHelperAgentId(true, helper) : null);
  $effect(() => {
    const report = onVisibleAgentChange;
    if (!report) return;
    report(visibleAgentId);
    return () => report(null);
  });

  function askLeave(): Promise<Choice> {
    return new Promise(resolve => { pending = resolve; leaveDialog?.showModal(); });
  }
  function choose(choice: Choice): void {
    const resolve = pending;
    pending = null;
    if (leaveDialog?.open) leaveDialog.close();
    resolve?.(choice);
  }
  function discardEdits(): void {
    text = content?.modified ?? "";
    contentKey++;
  }
  // Every way out of the current buffer (file/base switch, refresh, Git actions, Back, app navigation, window close) asks here first.
  async function beforeLeave(): Promise<boolean> {
    if (!dirty) return true;
    const choice = await askLeave();
    if (choice === "cancel") return false;
    if (choice === "discard") { discardEdits(); return true; }
    return save();
  }
  // Same entry if it still exists, else the same path in another group, else the first entry.
  function pick(files: DiffFile[], keep: Sel | null): Sel | null {
    const hit = (keep && (files.find(file => file.group === keep.group && file.path === keep.path) ?? files.find(file => file.path === keep.path))) ?? files[0];
    return hit ? { group: hit.group, path: hit.path } : null;
  }
  async function loadFile(target: Sel | null, snapshotId: string): Promise<void> {
    const seq = ++fileSeq;
    selected = target;
    content = null;
    text = "";
    conflict = false;
    if (!target) { fileLoading = false; return; }
    fileLoading = true;
    try {
      const result = await api.readCodeDiffFile(instanceId, snapshotId, target.path, target.group);
      if (seq !== fileSeq) return;
      content = result;
      text = result.modified ?? "";
      contentKey++;
    } catch (e) {
      if (seq === fileSeq) error = errorText(e);
    } finally {
      if (seq === fileSeq) fileLoading = false;
    }
  }
  async function loadOverview(next: DiffMode, keep: Sel | null): Promise<boolean> {
    const seq = ++overviewSeq;
    loading = true;
    error = "";
    try {
      const result = await api.getCodeDiff(instanceId, next);
      if (seq !== overviewSeq) return false;
      mode = next;
      overview = result;
      await loadFile(pick(result.files, keep), result.snapshotId);
      return true;
    } catch (e) {
      if (seq === overviewSeq) error = errorText(e);
      return false;
    } finally {
      if (seq === overviewSeq) loading = false;
    }
  }
  async function select(file: DiffFile): Promise<void> {
    if (busy || !overview) return;
    agentOpen = false;
    if (selected?.path === file.path && selected.group === file.group) return;
    if (await beforeLeave()) await loadFile({ group: file.group, path: file.path }, overview.snapshotId);
  }
  async function switchMode(event: Event): Promise<void> {
    const control = event.currentTarget as HTMLSelectElement;
    const next = control.value as DiffMode;
    if (next !== mode && await beforeLeave()) { notice = null; await loadOverview(next, selected); }
    control.value = mode;
  }
  async function refresh(): Promise<void> {
    if (busy) return;
    if (await beforeLeave()) await loadOverview(mode, selected);
  }
  async function gitTree(): Promise<void> {
    if (!busy && await beforeLeave()) onGitTree();
  }
  async function openAgent(request: "explain" | "review"): Promise<void> {
    if (opening || mode === "commits") return;
    lastRequest = request;
    opening = true;
    try {
      const admission = await api.openHelper(instanceId, "diff", request, mode);
      agent = { id: admission.helper.id, admission };
      agentOpen = true;
    } catch (e) {
      error = errorText(e);
    } finally {
      opening = false;
    }
  }
  async function toggleExplain(): Promise<void> {
    if (agentOpen) agentOpen = false;
    else await openAgent("explain");
  }
  async function back(): Promise<void> {
    if (!saving && !acting && await beforeLeave()) onBack();
  }
  function toggle(key: string): void {
    const next = new Set(collapsed);
    if (!next.delete(key)) next.add(key);
    collapsed = next;
  }
  async function save(): Promise<boolean> {
    if (!overview || !content || !selected || !editable || saving) return false;
    const { path, group } = selected;
    saving = true;
    error = "";
    conflict = false;
    try {
      const saved = await api.saveCodeDiffFile(instanceId, overview.snapshotId, path, group, content.workingHash, text);
      content = saved;
      text = saved.modified ?? text;
    } catch (e) {
      error = errorText(e);
      conflict = error.startsWith("diff_conflict");
      saving = false;
      return false;
    }
    saving = false;
    try {
      // A new snapshot reflects the saved bytes; the editor keeps its cursor and undo history.
      const fresh = await api.getCodeDiff(instanceId, mode);
      overview = fresh;
      if (!fresh.files.some(file => file.path === path && file.group === group)) await loadFile(pick(fresh.files, null), fresh.snapshotId);
    } catch (e) {
      error = `Saved, but the file list could not be refreshed: ${errorText(e)}`;
    }
    return true;
  }
  // Runs one Git action behind the dirty-buffer guard, then always reloads the list: the index or branch may have changed.
  async function runAction(work: (snapshotId: string) => Promise<void>): Promise<void> {
    if (busy || !overview) return;
    if (!await beforeLeave()) return;
    const snapshotId = overview.snapshotId;
    acting = true;
    error = "";
    notice = null;
    let failure = "";
    try { await work(snapshotId); } catch (e) { failure = errorText(e); }
    acting = false;
    await loadOverview("uncommitted", selected);
    if (failure) error = failure;
  }
  async function fileAction(file: DiffFile, action: DiffFileAction): Promise<void> {
    if (action === "revert" && !await confirmDanger(file.status === "?" ? `Delete untracked file ${file.path}? This cannot be undone.` : `Discard unstaged changes in ${file.path}? This cannot be undone.`)) return;
    await runAction(snapshotId => api.changeCodeDiffFile(instanceId, snapshotId, file.path, file.group, action));
  }
  async function commit(): Promise<void> {
    if (!canCommit) return;
    await runAction(async snapshotId => {
      const result = await api.commitCodeDiff(instanceId, snapshotId, message);
      message = "";
      notice = { text: `Committed ${result.commit}: ${result.summary}`, prUrl: null };
    });
  }
  async function publish(): Promise<void> {
    if (!canPublish) return;
    await runAction(async snapshotId => {
      const result = await api.publishCodeDiff(instanceId, snapshotId, { newBranch: branchOn ? newBranch.trim() : null, createPr });
      notice = { text: result.steps.join(" · "), prUrl: result.prUrl };
      branchOn = false;
      newBranch = "";
    });
  }
  function onKey(event: KeyboardEvent): void {
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") {
      event.preventDefault();
      void save();
    }
  }
  onMount(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    onGuardChange(beforeLeave);
    void loadOverview("head", null);
    // The native close button must not drop a dirty buffer either.
    getCurrentWindow().onCloseRequested(async event => {
      if (!await beforeLeave()) event.preventDefault();
    }).then(stop => { if (disposed) stop(); else unlisten = stop; }).catch(e => reportError("Guard window close", e));
    return () => { disposed = true; unlisten?.(); onGuardChange(null); choose("cancel"); };
  });
</script>
<svelte:window onkeydown={onKey} />
<section class="diff">
  <header class="bar">
    <button disabled={saving || acting} onclick={back}>Back to agent</button>
    <h2>Code diff · {name}</h2>
    {#if overview}<span class="dim mono root" title={overview.root}>{overview.root}</span>{/if}
    <span class="spacer"></span>
    {#if dirty}<span class="pill warn">Unsaved changes</span>{/if}
    <button disabled={busy} onclick={refresh}>Refresh</button>
    <button aria-pressed={agentOpen} class:active={agentOpen} disabled={saving || acting || opening} title="Short overall explanation of these changes by a read-only agent; type All there for every file" onclick={toggleExplain}>Explain</button>
    <button disabled={saving || acting || opening} title="Ask a read-only agent to review these changes" onclick={() => openAgent("review")}>Review</button>
    <button disabled={busy} onclick={gitTree}>Git tree</button>
    <button class="primary" disabled={!dirty || saving} onclick={() => save()}>{saving ? "Saving…" : "Save"}</button>
  </header>
  {#if error}
    <p class="err alert" role="alert">{error}
      {#if conflict}<button disabled={busy} onclick={refresh}>Reload from disk</button>{/if}
    </p>
  {/if}
  {#if notice}
    <p class="notice" role="status">{notice.text}
      {#if notice.prUrl}<button onclick={() => api.openCodeDiffPullRequest(instanceId).catch(e => reportError("Open pull request", e))}>Open pull request</button>{/if}
    </p>
  {/if}
  <div class="body">
    <aside class="side" aria-label="Changed files">
      <select aria-label="Compare with" value={mode} disabled={busy} onchange={switchMode}>
        <option value="head">Since last commit</option>
        <option value="uncommitted">Uncommitted changes</option>
        <option value="branch" disabled={overview !== null && overview.branchRef === null}>Compared with {overview?.branchRef ?? "master / default branch"}</option>
      </select>
      {#if mode === "uncommitted"}
        <div class="commit">
          <textarea rows="3" aria-label="Commit message" placeholder="Commit message" bind:value={message} disabled={acting}
            onkeydown={event => { if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) { event.preventDefault(); void commit(); } }}></textarea>
          <button class="primary" disabled={!canCommit} title={stagedCount ? "Commit the staged changes" : "Stage changes with + first"} onclick={commit}>Commit{stagedCount ? ` (${stagedCount})` : ""}</button>
        </div>
        <details class="publish">
          <summary>Publish…</summary>
          <p class="dim note">Branch {overview?.currentBranch ?? "(detached)"} → {overview?.upstream ?? "no upstream"}</p>
          <label class="check"><input type="checkbox" bind:checked={branchOn} />Create new branch</label>
          <input aria-label="New branch name" placeholder="branch-name" bind:value={newBranch} disabled={!branchOn} />
          <label class="check"><input type="checkbox" bind:checked={createPr} />Create pull request</label>
          <button disabled={!canPublish} onclick={publish}>Publish</button>
        </details>
      {/if}
      {#if mode === "branch" || overview?.branchRef === null}
        <p class="dim note">{overview?.branchRef === null ? "No master or default branch exists in this repository." : overview?.baseLabel}</p>
      {/if}
      {#if overview && mode === "uncommitted"}
        {#each GROUPS as { group, label } (group)}
          {@const list = overview.files.filter(file => file.group === group)}
          {#if list.length}
            <section class="group">
              <button class="group-head" aria-expanded={!collapsed.has(`section:${group}`)} onclick={() => toggle(`section:${group}`)}>
                <span aria-hidden="true">{collapsed.has(`section:${group}`) ? "▸" : "▾"}</span> {label} ({list.length})
              </button>
              {#if !collapsed.has(`section:${group}`)}
                <DiffFileTree files={list} prefix={group} {selected} {collapsed} disabled={busy} onSelect={select} onToggle={toggle} onAction={fileAction} />
              {/if}
            </section>
          {/if}
        {/each}
      {:else if overview}
        <DiffFileTree files={overview.files} prefix="" {selected} {collapsed} disabled={busy} onSelect={select} onToggle={toggle} onAction={fileAction} />
      {/if}
      {#if overview && !overview.files.length}<p class="dim note">No changed files for this comparison</p>{/if}
      {#if loading && !overview}<p class="dim note" role="status">Loading changes…</p>{/if}
    </aside>
    <div class="main" hidden={agentOpen}>
      {#if content && selectedFile}
        <div class="labels">
          <span><strong>{content.originalLabel}</strong></span><span><strong>{content.modifiedLabel}</strong>{#if content.eol === "crlf"} · CRLF{/if}{#if content.bom} · UTF-8 BOM{/if}</span>
        </div>
        {#if recreating}<p class="note warn">This file is deleted in the working tree. Saving recreates it.</p>{/if}
        {#if content.readOnlyReason}<p class="note warn">{content.readOnlyReason}</p>{/if}
        {#if content.original === null || content.modified === null}
          <p class="empty dim">No comparison can be shown for this file.</p>
        {:else}
          {#key contentKey}
            <div class="editor-slot"><CodeDiffEditor {content} onChange={value => text = value} onSave={() => void save()} /></div>
          {/key}
        {/if}
      {:else if fileLoading}
        <p class="empty dim" role="status">Loading file…</p>
      {:else if overview && !overview.files.length}
        <p class="empty dim">No changed files for this comparison</p>
      {:else if !loading && !overview}
        <p class="empty dim">Code diff is unavailable for this session.</p>
      {/if}
    </div>
    {#if agentOpen && agent}
      <div class="main"><HelperPane {helper} admission={agent.admission} onRetry={() => openAgent(lastRequest)} /></div>
    {/if}
  </div>
</section>
<dialog bind:this={leaveDialog} aria-labelledby="leave-title" oncancel={event => { event.preventDefault(); choose("cancel"); }}>
  <h3 id="leave-title">Unsaved changes</h3>
  <p>{selected?.path} has edits that are not saved to disk.</p>
  <div class="actions">
    <button class="primary" onclick={() => choose("save")}>Save</button>
    <button class="danger" onclick={() => choose("discard")}>Discard</button>
    <button onclick={() => choose("cancel")}>Cancel</button>
  </div>
</dialog>
<style>
  .diff { height: 100%; display: flex; flex-direction: column; gap: 10px; min-height: 0; }
  .bar { display: flex; align-items: center; flex-wrap: wrap; gap: 10px; }
  h2 { margin: 0; font-size: 18px; }
  .root { font-size: 12px; overflow-wrap: anywhere; }
  .alert { margin: 0; padding: 10px; border: 1px solid var(--err); border-radius: 8px; display: flex; flex-wrap: wrap; gap: 10px; align-items: center; }
  .notice { margin: 0; padding: 10px; border: 1px solid var(--ok); border-radius: 8px; display: flex; flex-wrap: wrap; gap: 10px; align-items: center; }
  .body { flex: 1; min-height: 0; display: flex; gap: 10px; }
  .side { flex: 0 0 280px; min-width: 200px; max-width: 40%; resize: horizontal; overflow: auto; display: flex; flex-direction: column; gap: 8px; padding: 8px; border: 1px solid var(--line); border-radius: 8px; background: var(--panel); }
  .side select { width: 100%; }
  .commit { display: flex; flex-direction: column; gap: 6px; }
  .commit textarea { resize: vertical; width: 100%; }
  .publish { border: 1px solid var(--line); border-radius: 8px; padding: 6px 8px; display: flex; flex-direction: column; gap: 6px; }
  .publish summary { cursor: pointer; }
  .publish[open] { gap: 8px; }
  .check { display: flex; flex-direction: row; align-items: center; gap: 6px; }
  .group { display: flex; flex-direction: column; gap: 2px; }
  .group-head { width: 100%; text-align: left; border-color: transparent; background: transparent; font-weight: 600; font-size: 12px; text-transform: uppercase; letter-spacing: .03em; color: var(--text-dim); }
  .note { margin: 0; font-size: 12px; }
  .main { flex: 1; min-width: 0; min-height: 0; display: flex; flex-direction: column; gap: 6px; }
  .main[hidden] { display: none; }
  .labels { display: flex; justify-content: space-between; font-size: 12px; color: var(--text-dim); }
  .editor-slot { flex: 1; min-height: 0; border: 1px solid var(--line); border-radius: 8px; overflow: hidden; }
  .empty { margin: 24px; }
  dialog { background: var(--panel); color: var(--text); border: 1px solid var(--line); border-radius: 10px; padding: 18px; }
  dialog::backdrop { background: rgba(0, 0, 0, .55); }
  dialog h3 { margin: 0 0 8px; }
  .actions { display: flex; gap: 8px; justify-content: flex-end; margin-top: 14px; }
</style>
