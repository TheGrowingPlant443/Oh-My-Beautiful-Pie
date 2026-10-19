<script lang="ts">
  import { untrack, type Snippet } from "svelte";
  import { knowledgeNeighborhood } from "../knowledge-neighborhood";
  import { buildIndex, edgeArrow, edgeCountsReconcile, projectGraph, relationColor, revealPath, type GraphIndex, type GraphModel, type LoopKind, type Projection, type ViewEdge, type ViewNode } from "../system-graph";
  import type { Camera } from "../system-overview";
  import type { LayoutStatus } from "../overview-layout";
  import OverviewFlow from "./OverviewFlow.svelte";
  import OverviewGraph from "./OverviewGraph.svelte";

  // The shared note/Graphify graph: the existing Flow (small) and Sigma (large) renderers over one projection. Directed records keep
  // arrowheads, undirected ones are plain lines, parallel and opposite records stay separate, and containers are never drawn as edges.
  let { model, loopKind, label, layoutKey, selectedNode, selectedEdge, matched, camera, focus = null, empty = "Nothing to show.", onSelect, onCamera, inspector }: {
    model: GraphModel; loopKind: LoopKind; label: string; layoutKey: string;
    selectedNode: string | null; selectedEdge: string | null; matched: ReadonlySet<string>; camera: Camera | null;
    /** Reveal and select this node (opens the groups around it). `seq` makes repeated requests for one id count. */
    focus?: { id: string; seq: number } | null;
    empty?: string;
    onSelect: (selection: { node: string | null; edge: string | null }) => void;
    onCamera: (camera: Camera | null) => void;
    inspector?: Snippet<[{ node: ViewNode | null; edge: ViewEdge | null; index: GraphIndex; projection: Projection }]>;
  } = $props();

  const index = $derived(buildIndex(model));
  // Null: the automatic grouped cut. Opening or closing a group switches to an explicit set; a new model starts automatic again.
  let expanded = $state<string[] | null>(null);
  let page = $state(0);
  let neighborhoodId = $state<string | null>(null);
  let neighborhoodPage = $state(0);
  let layout = $state<LayoutStatus | null>(null);
  let lastModel: GraphModel | null = null;
  $effect(() => {
    if (lastModel !== model) { lastModel = model; expanded = null; page = 0; neighborhoodId = null; neighborhoodPage = 0; }
  });
  const neighborhood = $derived(neighborhoodId ? knowledgeNeighborhood(index, neighborhoodId, neighborhoodPage, loopKind) : null);
  const projection = $derived(neighborhood?.projection ?? projectGraph(index, { mode: "source", expanded: expanded ? new Set(expanded) : null, page, loopKind }));
  const nodeView = $derived(projection.nodes.find(item => item.id === selectedNode) ?? null);
  const edgeView = $derived(projection.edges.find(item => item.id === selectedEdge) ?? null);
  const key = $derived(`${layoutKey}|${neighborhoodId ?? ""}:${neighborhoodPage}|${page}|${projection.expanded.length}:${projection.expanded.slice().sort().join("\u0000")}`);
  const relations = $derived([...new Set(projection.edges.map(item => item.relation))].sort());
  const hasUndirected = $derived(projection.edges.some(item => !edgeArrow(item)));
  const hasDirected = $derived(projection.edges.some(item => edgeArrow(item)));

  function toggle(id: string): void {
    const open = new Set(projection.expanded);
    if (open.has(id)) open.delete(id); else open.add(id);
    expanded = [...open];
    page = 0;
    onCamera(null);
  }
  let lastFocus = 0;
  let lastFocusModel: GraphModel | null = null;
  $effect(() => {
    const wanted = focus, currentModel = model, currentIndex = index;
    if (!wanted || (wanted.seq === lastFocus && lastFocusModel === currentModel)) return;
    // A focus issued while the graph was still loading must be retried when its model arrives.
    if (!currentIndex.nodes.has(wanted.id)) return;
    lastFocus = wanted.seq;
    lastFocusModel = currentModel;
    untrack(() => {
      const revealed = revealPath(currentIndex, wanted.id, expanded ? new Set(expanded) : null, "source");
      const fitted = projectGraph(currentIndex, { mode: "source", expanded: revealed, page: 0, loopKind });
      expanded = [...revealed];
      page = 0;
      neighborhoodId = fitted.nodes.some(item => item.id === wanted.id) ? null : wanted.id;
      neighborhoodPage = 0;
      onCamera(null);
      onSelect({ node: wanted.id, edge: null });
    });
  });
  const selectNode = (id: string | null): void => onSelect({ node: id, edge: null });
  const selectEdge = (id: string | null): void => onSelect({ node: null, edge: id });
</script>
<div class="graph">
  {#if neighborhood}
    <p class="banner">Exact neighborhood of <strong>{index.nodes.get(neighborhood.centerId)?.label}</strong>:
      original records {neighborhood.records ? neighborhood.first + 1 : 0}–{neighborhood.last} of {neighborhood.records.toLocaleString()}.
      The full graph has {index.totalNodes.toLocaleString()} items and {index.edges.length.toLocaleString()} records; search and details still use that full graph.
      <button disabled={neighborhood.page === 0} onclick={() => { neighborhoodPage--; onCamera(null); }}>Previous records</button>
      <button disabled={neighborhood.page + 1 >= neighborhood.pages} onclick={() => { neighborhoodPage++; onCamera(null); }}>Next records</button>
      <button onclick={() => { neighborhoodId = null; onCamera(null); }}>Full graph</button>
    </p>
  {/if}
  <p class="counts" role="status">
    Showing {projection.displayed.nodes.toLocaleString()} of {projection.total.nodes.toLocaleString()} items
    {#if projection.total.groups !== projection.total.nodes}in {projection.total.groups.toLocaleString()} nodes{/if}
    · {projection.displayed.edges.toLocaleString()} of {projection.total.edges.toLocaleString()} relationships drawn
    {#if projection.accounted.internal || projection.accounted.recursion || projection.accounted.dangling}({projection.accounted.internal.toLocaleString()} inside groups, {projection.accounted.recursion.toLocaleString()} {loopKind === "selfLink" ? "self-links" : "recursion"} on cards, {projection.accounted.dangling.toLocaleString()} without a node){/if}
    · {projection.renderer === "sigma" ? "large-graph view" : "diagram"}
    {#if layout?.state === "running" && projection.renderer === "sigma"}· arranging…{/if}
    {#if layout?.state === "error"}<span class="err">· layout failed: {layout.message}</span>{/if}
    {#if !neighborhood && !edgeCountsReconcile(index, projection)}<span class="err">· counts do not reconcile</span>{/if}
  </p>
  {#if projection.reason}<p class="banner warn">{projection.reason}</p>{/if}
  {#if projection.paged}
    <p class="banner">Window {projection.paged.page + 1} of {projection.paged.pages}
      <button disabled={projection.paged.page === 0} onclick={() => { page = Math.max(0, page - 1); onCamera(null); }}>Previous</button>
      <button disabled={projection.paged.page + 1 >= projection.paged.pages} onclick={() => { page = page + 1; onCamera(null); }}>Next</button></p>
  {/if}
  <div class="canvas">
    {#if projection.nodes.length === 0}
      <p class="empty">{empty}</p>
    {:else if projection.renderer === "sigma"}
      <OverviewGraph {projection} layoutKey={key} selectedNode={selectedNode} selectedEdge={selectedEdge} {matched} {camera} {label} onSelectNode={selectNode} onSelectEdge={selectEdge} {onCamera} onLayout={next => layout = next} />
    {:else}
      <OverviewFlow {projection} selectedNode={selectedNode} selectedEdge={selectedEdge} {matched} {camera} {label} onSelectNode={selectNode} onSelectEdge={selectEdge} onToggle={toggle} {onCamera} />
    {/if}
  </div>
  {#if relations.length}
    <ul class="legend" aria-label="Relationship colours">
      {#each relations as relation (relation)}<li><span class="swatch" style:background={relationColor(relation)}></span>{relation}</li>{/each}
      {#if hasDirected}<li class="dim">Arrowhead: the record has a direction (source → target).</li>{/if}
      {#if hasUndirected}<li class="dim">Plain line: a mutual relationship with no direction.</li>{/if}
      <li class="dim">Dashed: not proven or inferred.</li>
    </ul>
  {/if}
  {@render inspector?.({ node: nodeView, edge: edgeView, index, projection })}
</div>
<style>
  .graph { display: flex; flex-direction: column; gap: 6px; min-width: 0; min-height: 0; height: 100%; }
  .counts, .banner { margin: 0; font-size: 12px; color: var(--text-dim); overflow-wrap: anywhere; }
  .banner { padding: 4px 8px; border: 1px solid var(--line); border-radius: 6px; }
  .banner.warn { color: var(--warn); border-color: var(--warn); }
  .canvas { position: relative; flex: 1; min-height: 160px; border: 1px solid var(--line); border-radius: 8px; overflow: hidden; }
  .empty { margin: 16px; color: var(--text-dim); font-size: 13px; }
  .legend { list-style: none; margin: 0; padding: 0; display: flex; flex-wrap: wrap; gap: 2px 12px; font-size: 11px; }
  .legend li { display: inline-flex; align-items: center; gap: 5px; }
  .swatch { display: inline-block; width: 10px; height: 10px; border-radius: 2px; }
  .dim { color: var(--text-dim); }
  .err { color: var(--err); }
</style>
