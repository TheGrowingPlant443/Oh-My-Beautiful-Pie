<script lang="ts">
  import { untrack } from "svelte";
  import type { StructureSourceRow } from "../api";
  import type { ArchitectDescriptor, InventoryState } from "../overview-types";
  import { approveCheck, folderKey, generateBlock, inFlightRevision, selectedRevision, selectedSource, sourceViewed, supersededBy, type StructureStore } from "../structure-overview";
  import {
    approvalNotice, buildCurrentTree, capturedText, conflictList, conventionRows, differentDocument, entryLookup, excerptLine, generalize,
    indexChanged, mappingIndex, proposedTree, shortHash, sizeText, sourceKindLabel,
  } from "../structure-view";
  import StructureTree from "./StructureTree.svelte";

  let { folder, snapshotId, inventory, store, onOpenArchitect, architect }: {
    folder: string; snapshotId: string | null; inventory: InventoryState; store: StructureStore;
    onOpenArchitect: (id: string, replayRequestId?: string) => Promise<boolean>;
    architect: ArchitectDescriptor | null;
  } = $props();

  let approveDialog = $state<HTMLDialogElement>();

  // Complete inventory, shown immediately; generalization never drops a path.
  const tree = $derived(buildCurrentTree(inventory.files, inventory.directories));
  const general = $derived(generalize(tree));
  const hasInventory = $derived(inventory.files.length > 0 || inventory.directories.length > 0);

  const s = $derived($store);
  const analysis = $derived(selectedRevision(s, "structureCurrent"));
  const analysisPayload = $derived(analysis?.state === "completed" && analysis.result?.kind === "structureCurrent" ? analysis.result : null);
  const lookup = $derived(entryLookup(analysisPayload));
  const conventions = $derived(conventionRows(analysisPayload));
  const preview = $derived(selectedRevision(s, "structureProposed"));
  const previewPayload = $derived(preview?.state === "completed" && preview.result?.kind === "structureProposed" ? preview.result : null);
  const target = $derived(proposedTree(previewPayload));
  const mappings = $derived(mappingIndex(previewPayload));
  const conflicts = $derived(conflictList(previewPayload));
  const chosen = $derived(selectedSource(s));
  const chosenRef = $derived(chosen ? { sourceId: chosen.sourceId, sha256: chosen.sha256 } : null);
  const previewSource = $derived<StructureSourceRow | null>(preview?.sourceId ? s.sources.find(row => row.sourceId === preview.sourceId) ?? null : null);
  const viewing = $derived(s.sources.find(row => row.sourceId === s.previewId) ?? null);
  const viewPage = $derived(viewing ? s.pages[viewing.sourceId] : undefined);
  const chosenPage = $derived(chosen ? s.pages[chosen.sourceId] : undefined);
  const analyzeBlock = $derived(generateBlock(s, "structureCurrent", architect));
  const previewBlock = $derived(generateBlock(s, "structureProposed", architect));
  const approval = $derived(approveCheck(s));
  const discussTarget = $derived(preview?.state === "completed" ? preview : analysis?.state === "completed" ? analysis : null);
  const notice = $derived(preview?.approval ? approvalNotice(preview.approval) : null);
  const waiting = $derived({ structureCurrent: inFlightRevision(s, "structureCurrent"), structureProposed: inFlightRevision(s, "structureProposed") });
  const failedOf = (kind: "structureCurrent" | "structureProposed") => {
    const newest = s.revisions[kind][0];
    return newest && (newest.state === "failed" || newest.state === "uncertain") ? newest : null;
  };
  const failedAnalysis = $derived(failedOf("structureCurrent"));
  const failedPreview = $derived(failedOf("structureProposed"));
  const noLookup = entryLookup(null);
  const noMappings = mappingIndex(null);

  // Passive reads of the Architect's captured Markdown list; they never generate. The effect depends only on this scalar, so the freshly
  // allocated Architect object App derives for every instance snapshot never causes another request; only a real folder, owner,
  // session or readiness transition (and mount, the Refresh button and window focus) does.
  const refreshKey = $derived(`${folderKey(folder)}|${architect ? `${architect.id}:${architect.generation}:${architect.sessionId ?? ""}:${architect.ready ? "ready" : "notReady"}` : ""}`);
  $effect(() => {
    void refreshKey;
    untrack(() => { void store.refreshSources({ automatic: true }); });
  });

  const openArchitect = async (replay?: string): Promise<void> => { if (architect) await onOpenArchitect(architect.id, replay); };
  async function confirmApproval(): Promise<void> {
    await store.approve();
    if (!s.approvalError) approveDialog?.close();
  }
  function revisionLabel(revision: { requestId: string; createdAt?: number; state: string }): string {
    return `${shortHash(revision.requestId)} · ${revision.createdAt ? capturedText(revision.createdAt) : "time unknown"} · ${revision.state}`;
  }
</script>

<!-- Returning to this window after pasting the target Markdown in the Architect picks the new capture up; a read already running is joined. -->
<svelte:window onfocus={() => void store.refreshSources({ automatic: true })} />

<section class="structure" aria-label="Repository structure">
  <div class="toolbar" role="toolbar" aria-label="Structure actions">
    <button class="primary" disabled={!!analyzeBlock} onclick={() => store.generateCurrent()}>Analyze current structure &amp; naming</button>
    <button class="primary" disabled={!!previewBlock} onclick={() => store.generateProposed()}>Preview target structure</button>
    <button disabled={!approval.ok} onclick={() => approveDialog?.showModal()}>Approve preview &amp; discuss</button>
    <button disabled={!discussTarget} onclick={() => discussTarget && store.discuss(discussTarget.requestId)}>Discuss with Architect</button>
  </div>
  <ul class="reasons" aria-label="Why an action is unavailable">
    {#if analyzeBlock}<li>Analyze: {analyzeBlock}</li>{/if}
    {#if previewBlock}<li>Preview: {previewBlock}</li>{/if}
    {#if !approval.ok && approval.reason}<li>Approve: {approval.reason}</li>{/if}
    {#if !discussTarget}<li>Discuss: There is no saved result to discuss yet.</li>{/if}
  </ul>
  {#each [s.errors.structureCurrent, s.errors.structureProposed, s.savedError, s.discussError] as problem}
    {#if problem}<p class="alert" role="alert">{problem}</p>{/if}
  {/each}
  {#if s.uncertain.structureCurrent || s.uncertain.structureProposed}
    <p class="warn">Delivery to the Architect is uncertain and is never resent automatically. <button onclick={() => openArchitect()} disabled={!architect}>Open Architect</button></p>
  {/if}

  <details class="panel" open>
    <summary><h3>Current structure</h3>
      <span class="meta">{tree.fileCount} files · {tree.dirCount} directories{#if inventory.total > inventory.files.length} (inventory {inventory.files.length} of {inventory.total} files loaded){/if}</span>
    </summary>
    <div class="body">
      {#if inventory.snapshotId !== snapshotId && snapshotId}<p class="warn">The inventory belongs to another index snapshot; it is being refreshed.</p>{/if}
      {#if inventory.status === "loading"}<p class="dim" role="status">Loading the file inventory…</p>{/if}
      {#if inventory.status === "error"}<p class="alert" role="alert">{inventory.error ?? "The file inventory could not be loaded."}</p>{/if}
      {#if hasInventory}
        <StructureTree {general} {lookup} mappings={noMappings} />
      {:else if inventory.status === "ready"}<p class="dim">The index contains no admitted files.</p>
      {:else if inventory.status === "idle"}<p class="dim">No source index snapshot is selected yet.</p>{/if}
      {#if analysis}
        <div class="analysis">
          <h4>Architect analysis <span class="pill accent">{shortHash(analysis.requestId)}</span>
            {#if indexChanged(analysis, snapshotId)}<span class="pill warn">Index changed since this analysis</span>{/if}
            {#if supersededBy(s.revisions.structureCurrent, analysis)}<span class="pill warn">Replaced by a newer analysis</span>{/if}</h4>
          {#if waiting.structureCurrent}<p class="dim" role="status">Waiting for the Architect… showing the last saved analysis.</p>{/if}
          {#if failedAnalysis}<p class="warn">The newest analysis request is {failedAnalysis.state}: {failedAnalysis.message ?? "no details"}</p>{/if}
          {#if analysisPayload}
            <p class="dim">Group purposes above come from this analysis. Naming conventions:</p>
            {#each conventions as convention}
              <div class="convention"><strong>{convention.scope}</strong>: {convention.current}{#if convention.proposed} → {convention.proposed}{/if}
                <div class="dim">{convention.rationale}</div><small class="cite">Evidence: {convention.citations.join("; ") || "none cited"}</small></div>
            {:else}<p class="dim">The analysis lists no naming conventions.</p>{/each}
            {#each analysisPayload.limitations as limitation}<p class="dim">Limitation: {limitation}</p>{/each}
          {/if}
          <button onclick={() => store.regenerate("structureCurrent")} disabled={s.busy.structureCurrent || !snapshotId}>Re-run analysis</button>
        </div>
      {:else if waiting.structureCurrent}<p class="dim" role="status">Waiting for the Architect…</p>{/if}
    </div>
  </details>

  <section class="panel target" aria-labelledby="target-title">
    <header><h3 id="target-title">Target structure preview</h3>
      {#if preview}
        <select aria-label="Preview revision" value={preview.requestId} onchange={event => store.selectRevision("structureProposed", event.currentTarget.value)}>
          {#each s.revisions.structureProposed as revision (revision.requestId)}<option value={revision.requestId}>{revisionLabel(revision)}</option>{/each}
        </select>
        <button onclick={() => store.regenerate("structureProposed")} disabled={!!previewBlock}>Re-run preview</button>
      {/if}
    </header>
    <div class="body">
      <p class="dim">The pasted Markdown remains the source of truth; this diagram is a visualization aid only.</p>
      {#if waiting.structureProposed}<p class="dim" role="status">Waiting for the Architect…</p>{/if}
      {#if failedPreview}<p class="warn">The newest preview request is {failedPreview.state}: {failedPreview.message ?? "no details"}</p>{/if}
      {#if preview}
        <p class="badges">
          {#if indexChanged(preview, snapshotId)}<span class="pill warn">Index changed since this analysis</span>{/if}
          {#if differentDocument(preview, chosenRef)}<span class="pill warn">From a different document</span>{/if}
          {#if supersededBy(s.revisions.structureProposed, preview)}<span class="pill warn">Replaced by a newer preview</span>{/if}
          {#if preview.approval}<span class="pill ok">Approved for discussion</span>{/if}
        </p>
        <dl class="identity" aria-label="Source document">
          <dt>Source document</dt>
          <dd>{#if previewSource}{sourceKindLabel(previewSource.kind)} · captured {capturedText(previewSource.capturedAt)} · sha256 {shortHash(previewSource.sha256)} · {sizeText(previewSource.byteLength)}
            <div class="excerpt">{excerptLine(previewSource.excerpt)}</div>
            {:else}{preview.sourceId ?? "unknown source"} · sha256 {shortHash(preview.sourceHash)} (not in the listed Architect session){/if}</dd>
        </dl>
        {#if conflicts.length}
          <div class="conflicts" role="status"><strong>Target document conflicts</strong>
            <ul>{#each conflicts as conflict}<li>{conflict}</li>{/each}</ul></div>
        {/if}
        {#if previewPayload}
          <StructureTree proposed={target.roots} lookup={noLookup} {mappings} />
          {#each target.issues as issue}<p class="dim">{issue}</p>{/each}
          {#each previewPayload.limitations as limitation}<p class="dim">Limitation: {limitation}</p>{/each}
        {/if}
        {#if notice}
          <p class={notice.tone === "ok" ? "ok" : notice.tone === "warn" ? "warn" : "alert"} role="status">{notice.text}
            {#if notice.openArchitect}<button onclick={() => openArchitect()} disabled={!architect}>Open Architect</button>{/if}
            {#if preview.approval?.delivery === "pending" || preview.approval?.delivery === "claimed"}<button onclick={() => store.refreshRevision(preview.requestId)}>Refresh delivery status</button>{/if}
          </p>
          {#if preview.approval?.hasConflicts}<p class="warn">Approved with Target document conflicts retained.</p>{/if}
        {/if}
      {:else if !waiting.structureProposed}<p class="dim">No target preview yet. Select the target Markdown below, then choose Preview target structure.</p>{/if}
    </div>
  </section>

  <section class="panel" aria-labelledby="source-title">
    <header><h3 id="source-title">Target Markdown source</h3>
      <button onclick={() => store.refreshSources()} disabled={s.sourcesLoading}>Refresh list</button>
      <button onclick={() => store.useLastSubmitted()} disabled={s.selectingLast || !architect}>Use last submitted message</button>
      <span class="checking" role="status">{s.sourcesLoading ? (s.sourcesLoaded ? "Checking captured messages…" : "Reading the Architect's captured messages…") : ""}</span>
    </header>
    <div class="body">
      {#if s.sourcesError}<p class="alert" role="alert">{s.sourcesError}</p>{/if}
      {#if s.sources.length === 0 && (s.sourcesLoaded || !s.sourcesLoading)}
        <p><strong>Paste the target Markdown in Architect chat first</strong></p>
        <button onclick={() => openArchitect()} disabled={!architect}>Open Architect</button>
      {/if}
      <ul class="sources" aria-label="Captured messages">
        {#each s.sources as row (row.sourceId)}
          <li><button class="source" class:active={row.sourceId === s.previewId} aria-current={row.sourceId === s.previewId ? "true" : undefined} onclick={() => store.readSource(row.sourceId)}>
            <span>{capturedText(row.capturedAt)} · {sourceKindLabel(row.kind)} · {sizeText(row.byteLength)} · sha256 {shortHash(row.sha256)}
              {#if row.sourceId === s.selectedSourceId}<span class="pill ok">In use</span>{/if}</span>
            <span class="excerpt">{row.error ? `Not usable: ${row.error}` : excerptLine(row.excerpt)}</span></button></li>
        {/each}
      </ul>
      {#if viewing}
        <div class="viewer">
          <p class="meta">Read-only text of {viewing.sourceId} · sha256 {shortHash(viewing.sha256)}</p>
          {#if viewPage?.error}<p class="alert" role="alert">{viewPage.error}</p>{/if}
          {#if viewPage?.loading}<p class="dim" role="status">Reading…</p>{/if}
          <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
          <pre tabindex="0" aria-label="Markdown text, read only">{viewPage?.text ?? ""}</pre>
          {#if viewPage?.nextOffset != null}<button onclick={() => store.readSource(viewing.sourceId, true)} disabled={viewPage.loading}>Load more</button>{/if}
          <button class="primary" disabled={!viewPage?.loaded || !!viewing.error || viewing.sourceId === s.selectedSourceId} onclick={() => store.selectSource(viewing.sourceId)}>Use this Markdown</button>
        </div>
      {/if}
    </div>
  </section>
</section>

<dialog bind:this={approveDialog} aria-labelledby="approve-title">
  <h3 id="approve-title">Approve preview for discussion</h3>
  {#if preview}
    <dl class="ids">
      <dt>Request id</dt><dd>{preview.requestId}</dd><dt>Result hash</dt><dd>{preview.resultHash}</dd>
      <dt>Source id</dt><dd>{preview.sourceId}</dd><dt>Source hash</dt><dd>{preview.sourceHash}</dd>
    </dl>
    {#if conflicts.length}
      <div class="conflicts" role="status"><strong>Target document conflicts</strong> (retained with this approval)
        <ul>{#each conflicts as conflict}<li>{conflict}</li>{/each}</ul></div>
    {/if}
    <p class="meta">Target Markdown{#if chosenPage?.nextOffset != null} (loaded so far; the document continues){/if}</p>
    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
    <pre tabindex="0" aria-label="Target Markdown text, read only">{chosenPage?.text ?? ""}</pre>
    {#if chosen && chosenPage?.nextOffset != null}<button onclick={() => store.readSource(chosen.sourceId, true)} disabled={chosenPage.loading}>Load more</button>{/if}
    <p>This selects the preview for discussion only. It does not authorize repository edits, execution or restructuring; the pasted Markdown remains the source of truth.</p>
    {#if s.approvalError}<p class="alert" role="alert">{s.approvalError}</p>{/if}
  {/if}
  <div class="actions">
    <button type="button" onclick={() => approveDialog?.close()}>Cancel</button>
    <button class="primary" type="button" disabled={!approval.ok || s.approving || !sourceViewed(s)} onclick={confirmApproval}>Approve for discussion</button>
  </div>
</dialog>

<style>
  .structure { display: flex; flex-direction: column; gap: 10px; min-height: 0; }
  /* Natural height: the page's .structure wrapper is the one scroller, so no panel is squeezed below its content or clipped at the bottom. */
  .structure > * { flex: 0 0 auto; }
  .panel > .body { flex: 0 0 auto; min-height: auto; overflow: visible; }
  .toolbar { display: flex; flex-wrap: wrap; gap: 8px; }
  .reasons { margin: 0; padding: 0; list-style: none; font-size: 12px; color: var(--text-dim); }
  p.alert, p.warn, p.ok { margin: 0; padding: 8px 10px; border: 1px solid var(--err); border-radius: 8px; overflow-wrap: anywhere; white-space: pre-wrap; }
  p.warn { border-color: var(--warn); }
  p.ok { border-color: var(--ok); }
  .dim, .meta { color: var(--text-dim); font-size: 12px; margin: 4px 0; overflow-wrap: anywhere; }
  summary { display: flex; align-items: center; gap: 10px; padding: 8px 10px; cursor: pointer; background: #141b25; }
  summary h3, header h3 { margin: 0; font-size: 12px; letter-spacing: .07em; text-transform: uppercase; color: var(--text-dim); }
  .checking { margin-left: auto; color: var(--text-dim); font-size: 12px; }
  .target { border: 2px solid var(--accent-2); }
  .pill { display: inline-block; font-size: 11px; padding: 0 6px; border: 1px solid var(--line); border-radius: 999px; font-weight: 400; text-transform: none; letter-spacing: 0; }
  .pill.warn { color: var(--warn); border-color: var(--warn); }
  .pill.ok { color: var(--ok); border-color: var(--ok); }
  .badges { display: flex; flex-wrap: wrap; gap: 6px; margin: 4px 0; }
  .identity, .ids { margin: 6px 0; font-size: 12px; }
  .identity dt, .ids dt { color: var(--text-dim); }
  .identity dd, .ids dd { margin: 0 0 4px; overflow-wrap: anywhere; }
  .ids dd { font-family: var(--mono); }
  .excerpt { color: var(--text-dim); overflow-wrap: anywhere; }
  .conflicts { border: 1px solid var(--warn); border-radius: 8px; padding: 8px 10px; margin: 6px 0; font-size: 12px; }
  .conflicts ul { margin: 4px 0 0; padding-left: 18px; }
  .analysis { margin-top: 12px; border-top: 1px solid var(--line-soft); padding-top: 8px; }
  .analysis h4 { margin: 0 0 6px; font-size: 13px; }
  .convention { font-size: 12px; margin: 6px 0; overflow-wrap: anywhere; }
  .cite { color: var(--text-faint); font: 11px var(--mono); overflow-wrap: anywhere; }
  .sources { list-style: none; margin: 8px 0; padding: 0; display: flex; flex-direction: column; gap: 4px; max-height: 200px; overflow: auto; }
  .source { width: 100%; display: flex; flex-direction: column; align-items: flex-start; gap: 2px; text-align: left; font-size: 12px; }
  .viewer pre, dialog pre { margin: 6px 0; padding: 8px; max-height: 240px; overflow: auto; white-space: pre-wrap; overflow-wrap: anywhere; background: #0b1017; border: 1px solid var(--line); border-radius: 8px; font: 12px var(--mono); }
  dialog { background: var(--panel); color: var(--text); border: 1px solid var(--line); border-radius: 10px; padding: 18px; width: min(720px, 92vw); max-height: 90vh; overflow: auto; }
  dialog::backdrop { background: #0008; }
  dialog h3 { margin-top: 0; }
  .actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 16px; }
</style>
