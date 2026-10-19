<script module lang="ts">
  import { knowledgeApi } from "../knowledge";
  import { IndexClient } from "../knowledge-index";
  import IndexWorker from "../knowledge-index.worker?worker";
  import { createVaultStore, type VaultStore } from "../vault-documents";

  // One store per vault for the life of the page, so leaving and returning keeps the selection, the open folders, the camera and the splits.
  const stores = new Map<string, VaultStore>();
  function storeFor(vaultId: string): VaultStore {
    let known = stores.get(vaultId);
    if (!known) {
      known = createVaultStore({ api: knowledgeApi, indexClient: new IndexClient(() => new IndexWorker() as unknown as import("../knowledge-index").IndexWorkerLike), vaultId });
      stores.set(vaultId, known);
    }
    return known;
  }
</script>
<script lang="ts">
  import { untrack } from "svelte";
  import { get } from "svelte/store";
  import { knowledge, obsidianInstalled, type OpenNoteResult, type VaultBinding, type VaultEntry, type VaultLinkRecord } from "../knowledge";
  import { noteNodeId } from "../knowledge-graph";
  import type { VaultState } from "../vault-documents";
  import type { GraphIndex, ViewEdge, ViewNode } from "../system-graph";
  import KnowledgeGraph from "./KnowledgeGraph.svelte";
  import VaultDocument from "./VaultDocument.svelte";
  import VaultFileTree from "./VaultFileTree.svelte";

  // Explorer, rendered note and note graph of one vault (global agent assets, a project's documentation, a finished SRS). Reading is
  // read-only: nothing here writes, runs or fetches. `initialPath` opens that note when the view is first shown for this value.
  let { vaultId, initialPath = null, onBack, backLabel = "Back", locations = [], refreshKey = 0, title = "Vault" }: {
    vaultId: string; initialPath?: string | null; onBack?: () => void; backLabel?: string;
    /** Quick links (for example Agent setup targets) shown in a menu; each is a vault-relative note or folder path. */
    locations?: { label: string; path: string }[];
    /** Changing it re-reads the vault (a known publication: Apply, SRS completion). */
    refreshKey?: number;
    title?: string;
  } = $props();

  const store = $derived(storeFor(vaultId));
  // Store snapshots are replaced, never mutated, and hold whole note graphs: raw state avoids proxying them.
  let view = $state.raw<VaultState>(get(untrack(() => storeFor(vaultId))));
  $effect(() => {
    const active = store;
    const stop = active.subscribe(value => { view = value; });
    return () => { stop(); active.suspend(); };
  });

  // ----- what is open -----
  let openPath = $state<string | null>(null);
  let openFragment = $state<string | null>(null);
  let openMarkdown = $state(true);
  let trail = $state<string[]>([]);
  let focus = $state<{ id: string; seq: number } | null>(null);
  let focusSeq = 0;
  let selectedNode = $state<string | null>(null);
  let selectedEdge = $state<string | null>(null);
  let binding = $state<VaultBinding | null>(null);
  let bindingError = $state<string | null>(null);

  function open(path: string, fragment: string | null = null, markdown = true, remember = true): void {
    if (remember && openPath && openPath !== path) trail = [...trail.slice(-49), openPath];
    openPath = path;
    openFragment = fragment;
    openMarkdown = markdown;
    store.select(path);
    store.reveal(path);
    selectedEdge = null;
    selectedNode = noteNodeId(path);
    focus = { id: noteNodeId(path), seq: ++focusSeq };
  }
  /** A vault-relative note opens rendered; anything else is treated as a folder namespace: it is expanded in the explorer and never read as a document. */
  function openLocation(location: string, remember = true): void {
    const clean = location.replace(/\\/g, "/").replace(/^\/+|\/+$/g, "");
    if (/\.(md|markdown)$/i.test(clean)) { open(clean, null, true, remember); return; }
    if (clean === "") return;
    store.reveal(`${clean}/.`);
  }
  function back(): void {
    const previous = trail[trail.length - 1];
    if (previous === undefined) return;
    trail = trail.slice(0, -1);
    open(previous, null, true, false);
  }

  let initialization = 0;
  $effect(() => {
    const id = vaultId, wanted = initialPath, active = store;
    const mine = ++initialization;
    let live = true;
    const current = (): boolean => live && mine === initialization && id === vaultId && active === store;
    untrack(() => {
    // Another vault (or note) was asked for: nothing of the previous one stays selected.
    openFragment = null; trail = []; selectedNode = null; selectedEdge = null; launched = null; launchError = null; binding = null; text = "";
    openPath = null; openMarkdown = true;
    void active.refresh().then(() => { if (current() && wanted) openLocation(wanted, false); });
    const retained = get(active).selected;
    if (!wanted && retained) open(retained, null, true, false);
    knowledgeApi.listVaults().then(list => {
      if (!current()) return;
      binding = list.find(item => item.vaultId === id) ?? null;
      bindingError = binding ? null : "This vault is no longer registered.";
    }, failure => { if (current()) bindingError = String(failure); });
    });
    return () => { live = false; };
  });
  let lastRefresh = untrack(() => refreshKey);
  $effect(() => { if (refreshKey !== lastRefresh) { lastRefresh = refreshKey; void store.refresh(); } });
  $effect(() => {
    const onFocus = (): void => { void store.refresh(); };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  });

  // ----- Obsidian -----
  const caps = $state({ value: get(knowledge).capabilities });
  $effect(() => knowledge.subscribe(value => { caps.value = value.capabilities; }));
  $effect(() => { if (caps.value === null) void knowledge.probe(get(knowledge).folder); });
  let launched = $state<OpenNoteResult | null>(null);
  let launchError = $state<string | null>(null);
  async function openInObsidian(): Promise<void> {
    launched = null; launchError = null;
    try { launched = await knowledgeApi.openNote(vaultId, openPath); } catch (failure) { launchError = String(failure); }
  }
  async function vaultManager(): Promise<void> {
    try { await knowledgeApi.openObsidianVaultManager(); } catch (failure) { launchError = String(failure); }
  }

  // ----- search -----
  let text = $state("");
  let timer: ReturnType<typeof setTimeout> | null = null;
  function typed(value: string): void {
    text = value;
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => void store.setSearch(value), 250);
  }
  $effect(() => () => { if (timer) clearTimeout(timer); });
  const searching = $derived(view.search.text !== "" || text.trim() !== "");
  const matched = $derived(new Set(view.search.items.map(item => noteNodeId(item.path))));

  // ----- layout -----
  let width = $state(1200);
  const narrow = $derived(width < 980);
  let dragging: "tree" | "graph" | null = null;
  function drag(which: "tree" | "graph", event: PointerEvent): void {
    dragging = which;
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
  }
  function move(event: PointerEvent): void {
    if (!dragging) return;
    const rect = (event.currentTarget as HTMLElement).parentElement!.getBoundingClientRect();
    if (dragging === "tree") store.setSplit({ tree: Math.round(Math.min(Math.max(event.clientX - rect.left, 160), rect.width - 360)) });
    else store.setSplit({ graph: Math.round(Math.min(Math.max(rect.right - event.clientX, 240), rect.width - 360)) });
  }
  function nudge(which: "tree" | "graph", event: KeyboardEvent): void {
    const step = event.key === "ArrowLeft" ? -20 : event.key === "ArrowRight" ? 20 : 0;
    if (!step) return;
    event.preventDefault();
    store.setSplit(which === "tree" ? { tree: Math.max(160, view.split.tree + step) } : { graph: Math.max(240, view.split.graph - step) });
  }

  // ----- graph inspector -----
  const openEntry = (entry: VaultEntry): void => open(entry.path, null, entry.kind === "markdown");
  function pick(selection: { node: string | null; edge: string | null }): void {
    selectedNode = selection.node;
    selectedEdge = selection.edge;
    const found = selection.node === null ? null : view.graph.result?.model.nodes.find(item => item.id === selection.node && !item.synthetic);
    // Opening a note focuses it in the graph, which selects it again: only a different note is a new request.
    if (found?.path && found.path !== openPath) open(found.path, null, true);
  }
  const statusLine = $derived.by(() => {
    const stats = view.graph.result?.stats;
    if (!stats) return "";
    const parts = [`${stats.links.missing.toLocaleString()} missing`, `${stats.links.ambiguous.toLocaleString()} ambiguous`, `${stats.links.nonDocument.toLocaleString()} to non-Markdown files`, `${stats.links.external.toLocaleString()} external`, `${stats.links.anchor.toLocaleString()} within a note`];
    return `${stats.notes.toLocaleString()} notes · not drawn as lines: ${parts.join(", ")}${stats.orphanSources ? `, ${stats.orphanSources} from unlisted notes` : ""}`;
  });
  const recordText = (record: VaultLinkRecord): string => `${record.status}${record.kind === "embed" ? " · embedded" : ""}${record.fragment ? ` · #${record.fragment}` : ""}`;
</script>
<section class="vault" bind:clientWidth={width} aria-label={title}>
  <header class="bar panel">
    {#if onBack}<button onclick={onBack}>{backLabel}</button>{/if}
    <h2>{title}</h2>
    {#if binding}<span class="dim root" title={binding.root}>{binding.mode === "obsidian" ? "Obsidian vault" : "Markdown folder"} · {binding.root}</span>{:else if bindingError}<span class="err">{bindingError}</span>{/if}
    <span class="grow"></span>
    {#if locations.length}
      <label class="loc">Go to <select onchange={event => { const chosen = (event.currentTarget as HTMLSelectElement).value; if (chosen) openLocation(chosen); (event.currentTarget as HTMLSelectElement).value = ""; }}>
        <option value="">…</option>
        {#each locations as place (place.path)}<option value={place.path}>{place.label}</option>{/each}
      </select></label>
    {/if}
    <button disabled={trail.length === 0} onclick={back}>Previous note</button>
    <input type="search" placeholder="Search note names and headings" value={text} oninput={event => typed((event.currentTarget as HTMLInputElement).value)} aria-label="Search the vault" />
    <button disabled={view.refreshing} onclick={() => void store.refresh()}>{view.refreshing ? "Refreshing…" : "Refresh"}</button>
    {#if obsidianInstalled(caps.value)}
      <button onclick={() => void openInObsidian()} title="Opens this note in the Obsidian app, separately from OMP Manager">Open in Obsidian</button>
    {/if}
  </header>
  {#if view.refreshError}<p class="banner err" role="alert">{view.refreshError}</p>{/if}
  {#if launchError}<p class="banner err" role="alert">{launchError}</p>{/if}
  {#if launched?.needsVaultRegistration}
    <p class="banner warn" role="status">Obsidian does not know this folder yet. Add it as a vault in Obsidian: <code>{launched.vaultPath}</code>
      <button onclick={() => void vaultManager()}>Open vault manager</button></p>
  {:else if launched}
    <p class="banner" role="status">{launched.opened ? "Sent to Obsidian." : launched.message ?? "Obsidian did not accept the request."}</p>
  {/if}
  {#if view.summary}<p class="dim summary">{view.summary.notes.toLocaleString()} notes · {view.summary.links.toLocaleString()} links{view.summary.unreadable ? ` · ${view.summary.unreadable.toLocaleString()} unreadable` : ""}</p>{/if}

  <div class="body" class:stack={narrow}>
    <aside class="tree" style:width={narrow ? "100%" : `${view.split.tree}px`} style:max-height={narrow ? "220px" : "none"} aria-label="Files">
      {#if searching}
        {#if view.search.status === "loading" && view.search.items.length === 0}<p class="dim" role="status">Searching…</p>
        {:else if view.search.status === "error"}<p class="err" role="alert">{view.search.error}</p>
        {:else if view.search.status === "ready" && view.search.items.length === 0}<p class="dim">No note or heading matches “{view.search.text}”.</p>{/if}
        <ul class="hits">
          {#each view.search.items as hit, i (`${hit.path}:${hit.line ?? 0}:${i}`)}
            <li><button class="hit" class:active={hit.path === openPath} onclick={() => open(hit.path, null, true)}><strong>{hit.title || hit.path}</strong><small class="dim">{hit.path}{hit.line ? `:${hit.line}` : ""}</small>{#if hit.snippet}<small>{hit.snippet}</small>{/if}</button></li>
          {/each}
        </ul>
        {#if view.search.status === "ready"}<p class="dim count">{view.search.items.length.toLocaleString()} of {view.search.total.toLocaleString()} matches</p>{/if}
        {#if view.search.nextOffset !== null}<button onclick={() => void store.searchMore()}>Show more matches</button>{/if}
      {:else}
        <VaultFileTree state={view} selected={openPath} onOpen={openEntry} onToggle={folder => store.toggle(folder)} onMore={prefix => void store.loadFolder(prefix, true)} onRetry={prefix => void store.loadFolder(prefix)} />
      {/if}
    </aside>
    {#if !narrow}<!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions --><div class="split" role="separator" aria-orientation="vertical" aria-label="Resize the file list" tabindex="0" aria-valuenow={view.split.tree}
      onpointerdown={event => drag("tree", event)} onpointermove={move} onpointerup={() => { dragging = null; }} onkeydown={event => nudge("tree", event)}></div>{/if}
    <div class="center" class:stack={narrow}>
      <main class="document" aria-label="Note">
        {#if openPath}
          <VaultDocument {vaultId} path={openPath} revision={view.revision} fragment={openFragment} markdown={openMarkdown} backlinks={view.backlinks}
            onNavigate={(path, fragment) => open(path, fragment, true)} onMoreBacklinks={() => void store.loadLinks("backlinks", openPath, true)} />
        {:else}
          <p class="empty dim">Choose a note in the file list{view.search.text ? "" : " or search for one"}. Opening a note never changes it.</p>
        {/if}
      </main>
      {#if !narrow}<!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions --><div class="split" role="separator" aria-orientation="vertical" aria-label="Resize the note graph" tabindex="0" aria-valuenow={view.split.graph}
        onpointerdown={event => drag("graph", event)} onpointermove={move} onpointerup={() => { dragging = null; }} onkeydown={event => nudge("graph", event)}></div>{/if}
      <aside class="graph" style:width={narrow ? "100%" : `${view.split.graph}px`} style:height={narrow ? "320px" : "auto"} aria-label="Note graph">
        {#if view.graph.status === "loading"}
          <p class="dim" role="status">Reading links… {view.graph.notes.loaded.toLocaleString()} of {view.graph.notes.total.toLocaleString()} notes · {view.graph.links.loaded.toLocaleString()} of {view.graph.links.total.toLocaleString()} links</p>
        {:else if view.graph.status === "error"}
          <p class="banner err" role="alert">{view.graph.error} <button onclick={() => void store.refresh()}>Try again</button></p>
        {/if}
        {#if view.graph.result}
          <p class="dim stats">{statusLine}</p>
          <KnowledgeGraph model={view.graph.result.model} loopKind="selfLink" label="Note graph" layoutKey={`${vaultId}:${view.revision}`}
            {selectedNode} {selectedEdge} {matched} camera={view.camera} {focus} empty="This vault has no notes yet."
            onSelect={pick} onCamera={camera => store.setCamera(camera)}>
            {#snippet inspector({ node, edge, index }: { node: ViewNode | null; edge: ViewEdge | null; index: GraphIndex })}
              <div class="inspect" aria-label="Details">
                {#if edge}
                  {@const ids = new Set(edge.edgeIds)}
                  {@const records = index.edges.filter(item => ids.has(item.id))}
                  <h4>Relationship · {edge.count.toLocaleString()} link{edge.count === 1 ? "" : "s"}</h4>
                  <p class="dim">{edge.direction === "undirected" ? "Drawn as a plain line: a note relationship has no direction of its own. Each record below keeps the note it was written in." : "Directed."}</p>
                  <ul>{#each records.slice(0, 50) as record (record.id)}
                    <li><button class="link" onclick={() => open(record.source.replace(/^note:/, ""), null, true)}>{record.source.replace(/^note:/, "")}</button> → <button class="link" onclick={() => open(record.target.replace(/^note:/, ""), null, true)}>{record.target.replace(/^note:/, "")}</button>
                      <small class="dim">{record.relation === "embed" ? "embedded" : "linked"}{record.source === record.target ? " · Self-link" : ""}{record.evidenceIds[0] ? ` · ${record.evidenceIds[0].replace(/^.*:/, "line ")}` : ""}</small></li>
                  {/each}</ul>
                  {#if records.length > 50}<p class="dim">{records.length - 50} more records are in this line; open either note to see all of its links.</p>{/if}
                {:else if node}
                  <h4>{node.label}</h4>
                  <p class="dim">{node.kind}{node.collapsed ? ` · ${node.memberCount.toLocaleString()} notes inside` : ""}{node.within.length ? ` · in ${node.within.join(" › ")}` : ""}</p>
                  {#if node.selfLoops > 0}<p class="dim">↻ {node.selfLoops.toLocaleString()} Self-link{node.selfLoops === 1 ? "" : "s"} (this note links to itself).</p>{/if}
                  {#if node.path && !node.collapsed}
                    <p class="dim">Links of this note{view.neighbors.status === "ready" ? ` (${view.neighbors.total.toLocaleString()})` : ""}</p>
                    {#if view.neighbors.status === "loading" && view.neighbors.items.length === 0}<p class="dim">Looking…</p>
                    {:else if view.neighbors.status === "error"}<p class="err" role="alert">{view.neighbors.error}</p>
                    {:else}
                      <ul>{#each view.neighbors.items as record (record.id)}
                        <li><span class="dim">{record.source === node.path ? "→" : "←"}</span> {#if record.target && record.status === "resolved"}<button class="link" onclick={() => open(record.source === node.path ? record.target! : record.source, null, true)}>{record.source === node.path ? record.target : record.source}</button>{:else}<code>{record.raw}</code>{/if} <small class="dim">{recordText(record)}</small></li>
                      {/each}</ul>
                      {#if view.neighbors.nextOffset !== null}<button onclick={() => void store.loadLinks("neighbors", node.path, true)}>Show more links</button>{/if}
                    {/if}
                  {/if}
                {:else}
                  <p class="dim">Select a note or a line. Nothing here starts an agent.</p>
                {/if}
              </div>
            {/snippet}
          </KnowledgeGraph>
        {/if}
      </aside>
    </div>
  </div>
</section>
<style>
  .vault { height: 100%; min-height: 0; display: flex; flex-direction: column; gap: 6px; }
  .bar { display: flex; flex-direction: row; flex-wrap: wrap; align-items: center; gap: 8px; padding: 8px 12px; }
  h2 { margin: 0; font-size: 18px; }
  .grow { flex: 1; }
  .root { font-size: 11px; max-width: 420px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .loc { display: inline-flex; align-items: center; gap: 4px; font-size: 12px; }
  input[type="search"] { width: 240px; }
  .banner { margin: 0; padding: 6px 10px; border: 1px solid var(--line); border-radius: 8px; font-size: 12px; overflow-wrap: anywhere; }
  .banner.warn { color: var(--warn); border-color: var(--warn); }
  .banner.err { border-color: var(--err); color: var(--err); }
  .summary, .stats, .count { margin: 0; font-size: 12px; }
  .body { flex: 1; min-height: 0; display: flex; gap: 0; }
  .body.stack { flex-direction: column; gap: 6px; overflow: auto; }
  .tree { flex: none; min-width: 0; overflow: auto; border: 1px solid var(--line); border-radius: 8px; padding: 6px; }
  .center { flex: 1; min-width: 0; display: flex; gap: 0; min-height: 0; }
  .center.stack { flex-direction: column; gap: 6px; }
  .document { flex: 1; min-width: 0; min-height: 200px; border: 1px solid var(--line); border-radius: 8px; overflow: hidden; }
  .graph { flex: none; min-width: 0; display: flex; flex-direction: column; gap: 6px; border: 1px solid var(--line); border-radius: 8px; padding: 6px; overflow: auto; }
  .split { flex: none; width: 8px; cursor: col-resize; background: transparent; }
  .split:hover, .split:focus-visible { background: var(--line); outline: none; }
  .hits { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 2px; }
  .hit { width: 100%; display: flex; flex-direction: column; align-items: flex-start; gap: 1px; text-align: left; border-color: transparent; background: transparent; padding: 3px 6px; overflow-wrap: anywhere; }
  .hit.active { background: #22406b; border-color: var(--accent); }
  .empty { margin: 24px; }
  .inspect { border-top: 1px solid var(--line); padding-top: 6px; max-height: 260px; overflow: auto; font-size: 12px; }
  .inspect h4 { margin: 0 0 4px; font-size: 13px; overflow-wrap: anywhere; }
  .inspect ul { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 2px; }
  .link { border: 0; background: transparent; color: var(--accent); padding: 0; text-align: left; overflow-wrap: anywhere; cursor: pointer; }
  .dim { color: var(--text-dim); }
  .err { color: var(--err); }
</style>
