<script lang="ts">
  import { onMount } from "svelte";
  import { instances, projectHistory, computedFolders, rememberProjectFolder } from "../stores";
  import { pickFolder } from "../ui";
  import { projectFolders, repoKey } from "../repo-agents";
  import { folderOptions as historyOptions } from "../project-history";
  import { systemOverview, structureOverview } from "../system-overview-instance";
  import { deriveModel, effectiveIndex, selectedSystemResult, type OverviewTab, type SourceTab } from "../system-overview";
  import { buildIndex, edgeDashed, matchNodes, projectGraph, relationColor, revealPath, type GraphIndex, type ViewNode } from "../system-graph";
  import type { LayoutStatus } from "../overview-layout";
  import type { EvidenceRef, SourceFileRow, SourceSymbolRow } from "../api";
  import OverviewFlow from "./OverviewFlow.svelte";
  import OverviewGraph from "./OverviewGraph.svelte";
  import OverviewInspector from "./OverviewInspector.svelte";
  import OverviewCoverage from "./OverviewCoverage.svelte";
  import StructureView from "./StructureView.svelte";

  let { initialFolder, resume = false, onBack, onOpenArchitect, architect }: {
    initialFolder?: string; resume?: boolean; onBack: () => void;
    onOpenArchitect: (id: string, replayRequestId?: string) => Promise<boolean>;
    architect: { id: string; generation: number; ready: boolean } | null;
  } = $props();

  const TABS: { id: OverviewTab; label: string }[] = [{ id: "system", label: "System" }, { id: "files", label: "Files" }, { id: "functions", label: "Functions" }];
  const s = $derived($systemOverview);
  const tab = $derived(s.tab);
  const status = $derived(effectiveIndex(s));
  const attempt = $derived(s.index.attempt);
  const indexing = $derived(attempt !== null && (attempt.state === "inventory" || attempt.state === "parsing" || attempt.state === "resolving"));
  const level = $derived(tab === "system" ? null : s.levels[tab]);

  // Heavy derivations are keyed by the slices they read, so moving the camera or typing never rebuilds the graph.
  const expandedList = $derived(s.views[tab].expanded);
  const relationList = $derived(s.views[tab].relations);
  const gradeList = $derived(s.views[tab].grades);
  const showDerived = $derived(s.views[tab].derived);
  const pageNumber = $derived(s.views[tab].page);
  const selectedId = $derived(s.views[tab].selectedNode);
  const selectedEdgeId = $derived(s.views[tab].selectedEdge);
  const camera = $derived(s.views[tab].camera);
  const scope = $derived(s.views[tab].scope);
  const neighborhood = $derived(s.views[tab].neighborhood);
  const searchText = $derived(s.views[tab].search);

  let memo: { deps: readonly unknown[]; value: ReturnType<typeof deriveModel> } | null = null;
  const built = $derived.by(() => {
    const deps = [s.folder, tab, s.system.revisions, s.system.selectedId, s.levels[tab === "system" ? "files" : tab], scope, neighborhood, tab !== "system" && neighborhood ? s.hood : null];
    if (memo && memo.deps.length === deps.length && memo.deps.every((value, i) => value === deps[i])) return memo.value;
    const value = deriveModel(s, tab);
    memo = { deps, value };
    return value;
  });
  const graphIndex = $derived<GraphIndex>(buildIndex(built.model));
  const projection = $derived(projectGraph(graphIndex, {
    mode: tab === "system" ? "system" : "source", expanded: expandedList ? new Set(expandedList) : null,
    relations: relationList ? new Set(relationList) : null, grades: gradeList ? new Set(gradeList) : null, derived: showDerived, page: pageNumber,
  }));
  const relationsPresent = $derived([...new Set(graphIndex.edges.map(edge => edge.relation))].sort());
  const gradesPresent = $derived([...new Set(graphIndex.edges.map(edge => edge.grade))].sort());
  const result = $derived(selectedSystemResult(s));
  const evidence = $derived<ReadonlyMap<string, EvidenceRef>>(new Map((result?.result.evidence ?? []).map(item => [item.id, item])));
  const selectedNode = $derived(projection.nodes.find(item => item.id === selectedId) ?? null);
  const selectedEdge = $derived(projection.edges.find(item => item.id === selectedEdgeId) ?? null);
  const symbols = $derived(s.levels.functions.symbols);
  const files = $derived(s.levels.files.files);
  const symbolRow = $derived<SourceSymbolRow | null>(tab === "functions" && selectedNode?.ref ? symbols.find(item => item.id === selectedNode.ref) ?? null : null);
  const fileRow = $derived<SourceFileRow | null>(tab === "files" && selectedNode?.ref ? files.find(item => item.id === selectedNode.ref) ?? null : null);
  const systemMatches = $derived(tab === "system" ? matchNodes(graphIndex, searchText) : []);
  const search = $derived(tab === "system" ? null : s.search[tab]);
  const matched = $derived<ReadonlySet<string>>(new Set(tab === "system" ? systemMatches : (search?.items ?? []).map(item => item.id)));
  const layoutKey = $derived(JSON.stringify([status?.snapshotId, tab, scope?.label, neighborhood?.id, expandedList, relationList, gradeList, showDerived, pageNumber]));
  const folders = $derived(projectFolders($instances));
  const folderChoices = $derived(historyOptions($projectHistory, folders, s.folder, $computedFolders));
  const tickets = $derived(s.tickets.filter(ticket => repoKey(ticket.folder) === repoKey(s.folder) && ticket.kind === "systemOverview"));
  const stale = $derived(!!result && !!status?.snapshotId && result.status.snapshotId !== status.snapshotId);
  const canGenerate = $derived(!!status?.snapshotId && !s.system.admitting);
  let layout = $state<LayoutStatus | null>(null);
  let searchTimer: ReturnType<typeof setTimeout> | null = null;
  let pickError = $state<string | null>(null);

  onMount(() => {
    void systemOverview.enter({ folder: initialFolder, resume });
    return () => { if (searchTimer) clearTimeout(searchTimer); };
  });
  // Files and Functions load their level on demand; nothing here asks a model.
  $effect(() => {
    if (s.area === "diagram" && tab !== "system" && status?.snapshotId) void systemOverview.ensureLevel(tab);
  });

  function choose(folder: string): void {
    if (!folder || repoKey(folder) === repoKey(s.folder)) return;
    rememberProjectFolder(folder, true);
    void systemOverview.switchFolder(folder);
  }
  async function browse(): Promise<void> {
    pickError = null;
    try { const picked = await pickFolder(); if (picked) choose(picked); } catch (error) { pickError = String(error); }
  }
  function typeSearch(value: string): void {
    systemOverview.setSearch(tab, value);
    if (tab === "system") return;
    const target: SourceTab = tab;
    if (searchTimer) clearTimeout(searchTimer);
    searchTimer = setTimeout(() => { void systemOverview.runSearch(target, value); }, 300);
  }
  function toggle(id: string): void {
    const open = new Set(projection.expanded);
    if (open.has(id)) open.delete(id);
    else { open.add(id); if (tab !== "system") void systemOverview.loadCycle(tab, id); }
    systemOverview.setExpanded(tab, [...open]);
    systemOverview.setCamera(tab, null);
  }
  function selectNode(id: string | null): void {
    const node = id ? projection.nodes.find(item => item.id === id) : null;
    systemOverview.select(tab, id, null, node?.ref ?? null);
  }
  function selectEdge(id: string | null): void { systemOverview.select(tab, selectedId, id); }
  function reveal(id: string): void {
    systemOverview.setExpanded(tab, [...revealPath(graphIndex, id, expandedList ? new Set(expandedList) : new Set(projection.expanded), tab === "system" ? "system" : "source")]);
    systemOverview.setCamera(tab, null);
    systemOverview.select(tab, id, null, graphIndex.nodes.get(id)?.ref ?? null);
  }
  function neighbourhoodOf(node: ViewNode): void {
    if (tab === "system" || !node.ref) return;
    systemOverview.openNeighborhood(tab, { id: node.ref, label: tab === "files" ? node.path ?? node.label : node.label, path: node.path });
  }
  function peer(id: string, label: string | null): void {
    if (tab === "system") return;
    systemOverview.openNeighborhood(tab, { id, label: label ?? id, path: tab === "files" ? label : null });
  }
  function openHit(item: SourceFileRow | SourceSymbolRow): void {
    if (tab === "system") return;
    const isFile = "sha256" in item;
    systemOverview.openNeighborhood(tab, { id: item.id, label: isFile ? item.path : item.qualifiedName || item.name, path: item.path });
  }
  const showResult = (): void => { if (result && architect) void onOpenArchitect(architect.id, result.status.requestId); };
  const when = (ms: number | null): string => ms === null ? "" : new Date(ms).toLocaleString();
</script>

<section class="overview">
  <header class="panel bar">
    <button onclick={onBack}>Back to The Architect</button>
    <h2>System overview</h2>
    <label class="folder">Project folder
      <select value={s.folder} onchange={event => choose(event.currentTarget.value)}>
        <option value="" disabled={!!s.folder}>Choose a project folder</option>
        {#each folderChoices as option (option)}<option value={option}>{option}</option>{/each}
      </select>
    </label>
    <button onclick={browse}>Browse…</button>
    <div class="tabs" role="tablist" aria-label="Diagram level">
      {#each TABS as item (item.id)}
        <button role="tab" aria-selected={s.area === "diagram" && tab === item.id} class:active={s.area === "diagram" && tab === item.id} onclick={() => systemOverview.setTab(item.id)}>{item.label}</button>
      {/each}
    </div>
    <span class="spacer"></span>
    {#if indexing}<button onclick={() => void systemOverview.cancelIndex()}>Cancel indexing</button>{/if}
    <button disabled={!s.folder || indexing} title="Re-read the repository and rebuild the local source index. No agent is involved." onclick={() => void systemOverview.refreshIndex()}>Refresh index</button>
    <button class:active={s.area === "structure"} aria-pressed={s.area === "structure"} disabled={!s.folder} title="Current and target repository structure and naming" onclick={() => systemOverview.setArea(s.area === "structure" ? "diagram" : "structure")}>Repository structure</button>
  </header>

  <div class="strip" role="status">
    {#if !s.folder}<span>Choose a project folder to see its overview.</span>
    {:else if s.index.phase === "loading"}<span>Reading the saved source index…</span>
    {:else if indexing && attempt}<span>Indexing locally ({attempt.state}): {attempt.completedFiles.toLocaleString()} of {attempt.totalFiles.toLocaleString()} files. No agent or network is used.</span>
    {:else if status}<span>Indexed {when(status.indexedAt)} · {status.coverage?.admittedFiles.toLocaleString() ?? status.totalFiles.toLocaleString()} files{status.state === "partial" ? " · partial index" : ""}{status.sourceHash ? ` · ${status.sourceHash.slice(0, 12)}` : ""}</span>
    {:else}<span>No source index yet.</span>{/if}
    {#if status?.stale === true}<span class="badge warn" title="Only file metadata was compared. Refresh index re-reads content.">Files changed since this index (metadata); possibly stale</span>{/if}
    {#if attempt && !indexing && attempt.state === "partial" && !s.index.viewPartial}<button onclick={() => systemOverview.viewPartial(true)}>View partial index</button>{/if}
    {#if s.index.viewPartial}<button onclick={() => systemOverview.viewPartial(false)}>Back to the complete index</button>{/if}
    {#if attempt && (attempt.state === "failed" || attempt.state === "cancelled")}<span class="badge warn">Last refresh {attempt.state}{attempt.error ? `: ${attempt.error}` : ""}{status ? "; the earlier index is still shown." : ""}</span>{/if}
    {#if s.index.error}<span class="badge err" role="alert">{s.index.error}</span>{/if}
    {#if pickError}<span class="badge err" role="alert">{pickError}</span>{/if}
  </div>

  {#if status?.capabilities && !status.capabilities.available && status.capabilities.probed}
    <p class="notice warn" role="alert">{status.capabilities.hint ?? "No source parser is available. The file inventory is shown; functions and call relationships need a parser."}</p>
  {/if}

  {#if s.area === "structure"}
    <div class="structure"><StructureView folder={status?.folder ?? s.folder} snapshotId={status?.snapshotId ?? null} inventory={s.inventory} store={structureOverview} {onOpenArchitect} {architect} /></div>
  {:else}
    <div class="tools">
      <label class="search">Search
        <input type="search" value={searchText} placeholder={tab === "system" ? "Find a card" : tab === "files" ? "Find a file by path" : "Find a function by name or path"} oninput={event => typeSearch(event.currentTarget.value)} />
      </label>
      <details class="filters">
        <summary>Filters</summary>
        <fieldset><legend>Relationship kinds</legend>
          {#each relationsPresent as relation (relation)}
            <label><input type="checkbox" checked={!relationList || relationList.includes(relation)} onchange={event => {
              const current = relationList ?? relationsPresent;
              const next = event.currentTarget.checked ? [...new Set([...current, relation])] : current.filter(item => item !== relation);
              systemOverview.setFilters(tab, { relations: next.length === relationsPresent.length ? null : next, page: 0 });
            }} /><span class="swatch" style:background={relationColor(relation)}></span>{relation}</label>
          {/each}
        </fieldset>
        <fieldset><legend>{tab === "system" ? "Certainty" : "Resolution"}</legend>
          {#each gradesPresent as grade (grade)}
            <label><input type="checkbox" checked={!gradeList || gradeList.includes(grade)} onchange={event => {
              const current = gradeList ?? gradesPresent;
              const next = event.currentTarget.checked ? [...new Set([...current, grade])] : current.filter(item => item !== grade);
              systemOverview.setFilters(tab, { grades: next.length === gradesPresent.length ? null : next, page: 0 });
            }} />{grade}</label>
          {/each}
        </fieldset>
        {#if tab === "files"}<label><input type="checkbox" checked={showDerived} onchange={event => systemOverview.setFilters(tab, { derived: event.currentTarget.checked })} />Show cross-file arrows derived from function calls</label>{/if}
      </details>
      {#if s.back.length}<button onclick={() => systemOverview.back()}>Back</button>{/if}
      {#if tab === "system"}
        <span class="spacer"></span>
        {#if result}
          <select aria-label="Saved system diagrams" value={s.system.selectedId ?? ""} onchange={event => systemOverview.selectSystemRevision(event.currentTarget.value)}>
            {#each s.system.revisions.filter(item => item.state === "completed" && item.result?.kind === "systemOverview") as item (item.requestId)}<option value={item.requestId}>{when(item.createdAt ?? null)} {item.requestId.slice(0, 8)}</option>{/each}
          </select>
          <button disabled={!canGenerate} onclick={() => void systemOverview.generateSystem(true)}>Regenerate</button>
          <button disabled={!architect} title="Show this saved result in The Architect's conversation" onclick={showResult}>Discuss in The Architect</button>
        {:else}
          <button class="primary" disabled={!canGenerate} title={status?.snapshotId ? "Ask the existing Architect to analyze this repository" : "Build the source index first"} onclick={() => void systemOverview.generateSystem(false)}>Generate system diagram</button>
        {/if}
      {/if}
    </div>

    {#if scope}<p class="banner">Scoped to <strong>{scope.label}</strong> ({scope.exact.length + scope.prefixes.length} path{scope.exact.length + scope.prefixes.length === 1 ? "" : "s"}). <button onclick={() => systemOverview.clearScope(tab as SourceTab)}>Show everything</button></p>{/if}
    {#if neighborhood}<p class="banner">Exact neighbourhood of <strong title={neighborhood.label}>{neighborhood.label}</strong>. <button onclick={() => systemOverview.openNeighborhood(tab as SourceTab, null)}>Back to the overview</button></p>{/if}
    {#each tickets as ticket (ticket.key)}
      <p class="banner" role="status">
        {#if ticket.state === "waiting" || ticket.state === "admitting"}Waiting for The Architect to save the system diagram…
        {:else if ticket.state === "uncertain"}Delivery of the request is unconfirmed; it is not resent automatically. {ticket.message ?? ""}
        {:else if ticket.state === "completed"}{ticket.lateResult ? "The diagram arrived while you were elsewhere." : "The diagram is saved."}
        {:else}{ticket.message ?? ticket.state}{/if}
        {#if ticket.state === "completed" && ticket.lateResult}<button onclick={() => systemOverview.openResult(ticket.key)}>Open System overview result</button>{/if}
        {#if ticket.state === "timedOut" || ticket.state === "unreadable"}<button onclick={() => systemOverview.recheckTicket(ticket.key)}>Check again</button>{/if}
        {#if ticket.state === "failed" || ticket.state === "completed"}<button onclick={() => systemOverview.dismissTicket(ticket.key)}>Dismiss</button>{/if}
      </p>
    {/each}
    {#if s.system.error && tab === "system"}<p class="banner err" role="alert">{s.system.error}</p>{/if}
    {#if stale}<p class="banner warn">This diagram was generated from an older index. Regenerate to analyze the current one.</p>{/if}

    {#if tab !== "system" && searchText.trim() && search}
      <div class="hits" aria-label="Search results">
        {#if search.status === "loading" && !search.items.length}<span role="status">Searching…</span>{/if}
        {#if search.error}<span class="err" role="alert">{search.error}</span>{/if}
        {#if search.status === "ready" && !search.items.length}<span>No matches in the complete index.</span>{/if}
        {#each search.items.slice(0, 12) as item (item.id)}<button onclick={() => openHit(item)} title={"path" in item ? item.path : ""}>{"sha256" in item ? item.path : `${item.qualifiedName || item.name} — ${item.path}:${item.span.lineStart}`}</button>{/each}
        {#if search.total > 12}<small>{search.total.toLocaleString()} matches in total; refine the text to narrow them.</small>{/if}
      </div>
    {:else if tab === "system" && searchText.trim()}
      <div class="hits" aria-label="Matching cards">
        {#each systemMatches.slice(0, 12) as id (id)}<button onclick={() => reveal(id)}>{graphIndex.nodes.get(id)?.label ?? id}</button>{:else}<span>No card matches.</span>{/each}
      </div>
    {/if}

    <p class="counts" role="status">
      Showing {projection.displayed.nodes.toLocaleString()} of {projection.total.nodes.toLocaleString()} {tab === "functions" ? "functions" : tab === "files" ? "files" : "nodes"}{#if projection.total.groups !== projection.total.nodes} in {projection.total.groups.toLocaleString()} {level?.mode === "directories" ? "directories" : "nodes"}{/if}
      · {projection.displayed.edges.toLocaleString()} of {projection.total.edges.toLocaleString()} relationships drawn
      {#if projection.accounted.internal || projection.accounted.recursion || projection.accounted.filtered}({projection.accounted.internal.toLocaleString()} inside groups, {projection.accounted.recursion.toLocaleString()} recursion shown on cards, {projection.accounted.filtered.toLocaleString()} hidden by filters){/if}
      · {projection.renderer === "sigma" ? "large-graph view" : "diagram"}
      {#if layout?.state === "running" && projection.renderer === "sigma"}· arranging…{/if}
      {#if layout?.state === "error"}<span class="err">· layout failed: {layout.message}</span>{/if}
    </p>
    {#if projection.reason}<p class="banner warn">{projection.reason}</p>{/if}
    {#if projection.paged}
      <p class="banner">Window {projection.paged.page + 1} of {projection.paged.pages}
        <button disabled={projection.paged.page === 0} onclick={() => systemOverview.setFilters(tab, { page: projection.paged ? projection.paged.page - 1 : 0 })}>Previous</button>
        <button disabled={projection.paged.page + 1 >= projection.paged.pages} onclick={() => systemOverview.setFilters(tab, { page: projection.paged ? projection.paged.page + 1 : 0 })}>Next</button></p>
    {/if}
    {#if graphIndex.dangling.length}<p class="banner warn">{graphIndex.dangling.length.toLocaleString()} relationship record{graphIndex.dangling.length === 1 ? "" : "s"} point at items outside the loaded set and are not drawn.</p>{/if}

    <div class="main">
      <div class="canvas">
        {#if tab !== "system" && level && (level.status === "loading" || level.status === "idle") && !neighborhood}
          <p class="empty" role="status">{status?.snapshotId ? `Loading ${tab}… ${level.loaded.toLocaleString()}${level.total ? ` of ${level.total.toLocaleString()}` : ""}` : "Build the source index to see files and functions."}</p>
        {:else if tab !== "system" && level?.status === "error" && !neighborhood}
          <p class="empty err" role="alert">{level.error}</p>
        {:else if projection.nodes.length === 0}
          <p class="empty">{tab === "system" ? "Nothing to show." : neighborhood ? built.note ?? "Nothing to show." : "This repository has no indexed items for this view."}</p>
        {:else}
          {#if tab === "system" && !result}
            <p class="overlay" role="status"><strong>No generated system diagram yet.</strong> The box below is the application only; nothing around it is known until you generate a diagram. Inventory alone is not a system map.</p>
          {/if}
          {#if tab !== "system" && level?.mode === "directories" && !neighborhood}
            <p class="overlay" role="status">More than 5,000 {tab} are indexed, so directories are shown as groups. Search for an exact item to open its neighbourhood.</p>
          {/if}
          {#if projection.renderer === "sigma"}
            <OverviewGraph {projection} {layoutKey} selectedNode={selectedId} selectedEdge={selectedEdgeId} {matched} {camera} onSelectNode={selectNode} onSelectEdge={selectEdge} onCamera={next => systemOverview.setCamera(tab, next)} onLayout={next => layout = next} />
          {:else}
            <OverviewFlow {projection} selectedNode={selectedId} selectedEdge={selectedEdgeId} {matched} {camera} onSelectNode={selectNode} onSelectEdge={selectEdge} onToggle={toggle} onCamera={next => systemOverview.setCamera(tab, next)} />
          {/if}
        {/if}
      </div>
      <OverviewInspector {tab} node={selectedNode} edge={selectedEdge} inspector={s.inspector} {evidence} symbol={symbolRow} file={fileRow}
        onDrillFiles={(paths, label) => systemOverview.drill("files", { label, exact: paths, prefixes: [] })} onDrillFunctions={(path, label) => systemOverview.drill("functions", { label, exact: [path], prefixes: [] })}
        onNeighborhood={neighbourhoodOf} onPeer={peer} onMore={() => void systemOverview.moreNeighbors()} />
    </div>
    {#if relationsPresent.length}
      <ul class="legend" aria-label="Relationship colours">
        {#each relationsPresent as relation (relation)}<li><span class="swatch" style:background={relationColor(relation)}></span>{relation}</li>{/each}
        <li>Dashed: not statically proven, or a response (a response is never a new caller).</li>
        {#if projection.edges.some(edge => edgeDashed(edge))}<li class="dim">Arrows point from the caller or requester to the target.</li>{/if}
      </ul>
    {/if}
    <OverviewCoverage {status} coverage={s.coverage} onLoad={more => void systemOverview.loadCoverage(more)} />
  {/if}
</section>

<style>
  .overview { height: 100%; min-height: 0; display: flex; flex-direction: column; gap: 8px; }
  /* `.panel` is a column; the header must lay out as a wrapping row. */
  .bar { display: flex; flex-direction: row; flex-wrap: wrap; align-items: center; gap: 8px; padding: 8px 12px; }
  h2 { margin: 0; font-size: 18px; }
  .folder { display: flex; align-items: center; gap: 6px; }
  .folder select { max-width: 340px; }
  .tabs { display: inline-flex; gap: 4px; }
  .tabs button.active { border-color: var(--accent); color: var(--accent); }
  .strip { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; font-size: 12px; color: var(--text-dim); }
  .badge { padding: 1px 6px; border: 1px solid var(--line); border-radius: 4px; }
  .badge.warn, .notice.warn, .banner.warn { color: var(--warn); border-color: var(--warn); }
  .badge.err, .err { color: var(--err); }
  .notice, .banner { margin: 0; padding: 6px 10px; border: 1px solid var(--line); border-radius: 8px; font-size: 12px; overflow-wrap: anywhere; }
  .banner.err { border-color: var(--err); }
  .tools { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; }
  .search { display: flex; align-items: center; gap: 6px; }
  .search input { width: 260px; }
  .filters { position: relative; }
  .filters summary { cursor: pointer; }
  .filters fieldset { display: flex; flex-wrap: wrap; gap: 8px; border: 1px solid var(--line); border-radius: 6px; margin: 4px 0; }
  .filters label { display: inline-flex; align-items: center; gap: 4px; font-size: 12px; }
  .swatch { display: inline-block; width: 10px; height: 10px; border-radius: 2px; }
  .hits { display: flex; flex-wrap: wrap; align-items: center; gap: 6px; max-height: 96px; overflow: auto; font-size: 12px; }
  .counts { margin: 0; font-size: 12px; color: var(--text-dim); }
  .main { flex: 1; min-height: 240px; display: flex; gap: 8px; }
  .canvas { position: relative; flex: 1; min-width: 0; border: 1px solid var(--line); border-radius: 8px; overflow: hidden; }
  .overlay { position: absolute; z-index: 5; left: 12px; right: 12px; top: 8px; margin: 0; padding: 6px 10px; font-size: 12px; background: var(--panel); border: 1px solid var(--line); border-radius: 6px; }
  .empty { margin: 24px; color: var(--text-dim); }
  .structure { flex: 1; min-height: 0; overflow: auto; }
  .legend { list-style: none; margin: 0; padding: 0; display: flex; flex-wrap: wrap; gap: 4px 14px; font-size: 12px; }
  .legend li { display: inline-flex; align-items: center; gap: 5px; }
  .dim { color: var(--text-dim); }
</style>
