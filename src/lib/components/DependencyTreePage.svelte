<script module lang="ts">
  // Positions found by earlier layouts, per scanned folder, so toggling a view starts from where the nodes were.
  const layouts = new Map<string, Map<string, { x: number; y: number }>>();
  const PREFS_KEY = "omp-manager.dependency-tree";

  // Deterministic start position from an FNV-1a hash of the package id (ForceAtlas2 needs distinct positions).
  function seeded(id: string): { x: number; y: number } {
    let h = 2166136261;
    for (let i = 0; i < id.length; i++) { h ^= id.charCodeAt(i); h = Math.imul(h, 16777619); }
    const a = (h >>> 0) / 4294967296;
    h = Math.imul(h ^ (h >>> 15), 2246822507);
    h ^= h >>> 13;
    return { x: (a - 0.5) * 200, y: ((h >>> 0) / 4294967296 - 0.5) * 200 };
  }
</script>
<script lang="ts">
  import { onMount, untrack } from "svelte";
  import Graph from "graphology";
  import forceAtlas2 from "graphology-layout-forceatlas2";
  import Sigma from "sigma";
  import { api } from "../api";
  import { errorText, instances } from "../stores";
  import { pickFolder } from "../ui";
  import { projectFolders, repoKey } from "../repo-agents";
  import { ROOT, nodeColor, nodeSize, pendingDirect, visibleGraph, type Classification, type DependencyGraph, type DependencyMode, type VisibleGraph } from "../dependency-graph";
  let { onBack }: { onBack: () => void } = $props();

  function loadPrefs(): { folder: string; mode: DependencyMode; showAll: boolean } {
    try {
      const value = JSON.parse(localStorage.getItem(PREFS_KEY) ?? "null") as Record<string, unknown> | null;
      return {
        folder: typeof value?.folder === "string" ? value.folder : "",
        mode: value?.mode === "transitive" ? "transitive" : "direct",
        showAll: value?.showAll === true,
      };
    } catch { return { folder: "", mode: "direct", showAll: false }; }
  }
  const saved = loadPrefs();
  let folder = $state(saved.folder);
  let mode = $state<DependencyMode>(saved.mode);
  let showAll = $state(saved.showAll);
  let graph = $state.raw<DependencyGraph | null>(null);
  let classification = $state.raw<Classification>({ items: {}, updatedAt: null });
  let loading = $state(false);
  let error = $state<string | null>(null);
  let classifying = $state(false);
  let classifyError = $state<string | null>(null);
  let hovered = $state<string | null>(null);
  let selected = $state<string | null>(null);
  let layoutMs = $state(0);
  let container = $state<HTMLDivElement>();

  const folders = $derived(projectFolders($instances));
  const folderOptions = $derived(folder && !folders.some(known => repoKey(known) === repoKey(folder)) ? [folder, ...folders] : folders);
  const empty: VisibleGraph = { nodes: [], edges: [], hidden: 0 };
  const visible = $derived(graph ? visibleGraph(graph, classification, mode, showAll) : empty);
  const pending = $derived(graph ? pendingDirect(graph, classification) : []);
  const directCount = $derived(graph ? graph.nodes.filter(node => node.direct).length : 0);
  const classified = $derived(directCount - pending.length);
  const names = $derived(new Map(visible.nodes.map(node => [node.id, node.name])));
  const focusId = $derived(hovered ?? selected);
  const focusNode = $derived(focusId ? visible.nodes.find(node => node.id === focusId) ?? null : null);
  const dependsOn = $derived(focusId ? visible.edges.filter(([from]) => from === focusId).map(([, to]) => to) : []);
  const requiredBy = $derived(focusId ? visible.edges.filter(([, to]) => to === focusId).map(([from]) => from) : []);

  $effect(() => { if (!folder && folders.length) folder = folders[0]; });
  $effect(() => { localStorage.setItem(PREFS_KEY, JSON.stringify({ folder, mode, showAll })); });

  let loadSequence = 0;
  async function load(target: string): Promise<void> {
    const sequence = ++loadSequence;
    stopPolling();
    loading = true;
    error = null;
    classifyError = null;
    try {
      const [scanned, judged] = await Promise.all([api.scanDependencies(target), api.getDependencyClassification(target)]);
      if (sequence !== loadSequence) return;
      graph = scanned;
      classification = judged;
    } catch (e) {
      if (sequence !== loadSequence) return;
      graph = null;
      error = errorText(e);
    } finally {
      if (sequence === loadSequence) loading = false;
    }
  }
  $effect(() => {
    const target = folder;
    if (target) untrack(() => void load(target));
  });

  let pollTimer: ReturnType<typeof setInterval> | null = null;
  function stopPolling(): void {
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = null;
    classifying = false;
  }
  async function classify(): Promise<void> {
    const target = folder;
    if (!target) return;
    classifyError = null;
    classifying = true;
    try {
      const admission = await api.classifyDependencies(target);
      if (admission.error) classifyError = admission.error;
      if (admission.requested === 0 || admission.promptSent === false) { classifying = false; return; }
      const until = Date.now() + 10 * 60_000;
      pollTimer = setInterval(() => {
        void api.getDependencyClassification(target).then(judged => {
          if (target !== folder) return;
          classification = judged;
          if ((graph && pendingDirect(graph, judged).length === 0) || Date.now() > until) stopPolling();
        }).catch(e => { classifyError = errorText(e); stopPolling(); });
      }, 2000);
    } catch (e) {
      classifyError = errorText(e);
      classifying = false;
    }
  }
  async function browse(): Promise<void> {
    const picked = await pickFolder();
    if (picked) folder = picked;
  }

  // One Sigma renderer per visible graph; the layout is computed up front so the first frame is already settled.
  $effect(() => {
    const view = visible;
    const host = container;
    const scanned = graph;
    const judged = classification;
    if (!host || !scanned || view.nodes.length === 0) { layoutMs = 0; return; }
    const started = performance.now();
    const inDegree = new Map<string, number>();
    for (const [, to] of view.edges) inDegree.set(to, (inDegree.get(to) ?? 0) + 1);
    const cache = layouts.get(scanned.key) ?? new Map<string, { x: number; y: number }>();
    layouts.set(scanned.key, cache);
    const built = new Graph({ type: "directed" });
    for (const node of view.nodes) {
      const at = cache.get(node.id) ?? seeded(node.id);
      built.addNode(node.id, {
        label: node.name,
        size: node.id === ROOT ? 16 : nodeSize(inDegree.get(node.id) ?? 0),
        color: nodeColor(node, judged.items[node.id]?.basic === true),
        x: at.x,
        y: at.y,
      });
    }
    for (const [from, to] of view.edges) if (!built.hasEdge(from, to)) built.addEdge(from, to, { size: 0.5, color: "#2a3546" });
    if (built.order > 1) {
      forceAtlas2.assign(built, {
        iterations: Math.min(500, Math.max(20, Math.round(1_500_000 / built.order))),
        settings: { ...forceAtlas2.inferSettings(built), barnesHutOptimize: built.order > 500, slowDown: 5 },
      });
    }
    built.forEachNode((id, attributes) => cache.set(id, { x: attributes.x as number, y: attributes.y as number }));
    const renderer = new Sigma(built, host, {
      settings: { renderEdgeLabels: false, labelRenderedSizeThreshold: 6, labelDensity: 0.6, itemSizesReference: "screen", enableEdgeEvents: false },
      styles: {
        nodes: [
          { color: { attribute: "color" }, size: { attribute: "size", defaultValue: 6 }, label: { attribute: "label" }, labelColor: "#dfe6ef" },
          { whenState: "isHovered", then: { labelVisibility: "visible" } },
        ],
        edges: [{ color: { attribute: "color" }, size: { attribute: "size", defaultValue: 0.5 } }],
      },
    });
    renderer.on("enterNode", ({ node }) => { hovered = node; });
    renderer.on("leaveNode", () => { hovered = null; });
    renderer.on("clickNode", ({ node }) => { selected = node; });
    renderer.on("clickStage", () => { selected = null; });
    layoutMs = Math.round(performance.now() - started);
    return () => { renderer.kill(); hovered = null; };
  });

  onMount(() => () => { loadSequence++; stopPolling(); });

  const legend = [
    { label: "Project", color: "#4ea1ff" },
    { label: "Chosen direct dependency", color: "#3fb950" },
    { label: "Basic direct dependency (Show all)", color: "#d29922" },
    { label: "Transitive dependency", color: "#61708a" },
  ];
</script>
<section class="page">
  <header class="bar">
    <button onclick={onBack}>Back to The Architect</button><h2>Dependency tree</h2>
    <select aria-label="Project folder" value={folder} onchange={e => { folder = e.currentTarget.value; }}>
      {#if !folderOptions.length}<option value="">No project folders yet</option>{/if}
      {#each folderOptions as option (option)}<option value={option}>{option}</option>{/each}
    </select>
    <button onclick={browse}>Browse…</button>
    <select aria-label="Dependency view" value={mode} onchange={e => { mode = e.currentTarget.value as DependencyMode; }}>
      <option value="direct">Direct dependencies</option>
      <option value="transitive">Transitive dependencies</option>
    </select>
    <button class:active={showAll} aria-pressed={showAll} title="Also show packages the Architect judged built in / basic" onclick={() => { showAll = !showAll; }}>Show all</button>
    <span class="spacer"></span>
    <button disabled={classifying || !graph || pending.length === 0} title="Ask The Architect which direct dependencies are basic (built in) and which were chosen" onclick={classify}>Classify with The Architect</button>
  </header>
  <ul class="legend" aria-label="Colours">
    {#each legend as entry}<li><span class="swatch" style:background={entry.color}></span>{entry.label}</li>{/each}
  </ul>
  {#if loading}<p role="status">Scanning dependencies…</p>{/if}
  {#if error}<p class="alert err" role="alert">{error}</p>{/if}
  {#if classifyError}<p class="alert err" role="alert">{classifyError}</p>{/if}
  {#if graph}
    <p class="meta" role="status">
      {visible.nodes.length} shown · {visible.hidden} hidden as built in · {classified}/{directCount} direct packages classified{#if classifying} · Waiting for The Architect… ({classified}/{directCount}){/if}
      {#if classified === 0 && directCount > 0}<br />Not classified yet: showing every package. Press "Classify with The Architect".{/if}
    </p>
    {#if graph.notes.length}<ul class="notes dim">{#each graph.notes as note}<li>{note}</li>{/each}</ul>{/if}
  {/if}
  <div class="stage">
    <div class="canvas" bind:this={container} data-nodes={visible.nodes.length} data-edges={visible.edges.length} data-hidden={visible.hidden} data-layout-ms={layoutMs}></div>
    {#if graph && visible.nodes.length <= 1}<p class="empty">No packages to show.</p>{/if}
    {#if focusNode}
      <aside class="card" aria-label="Package details">
        <strong>{focusNode.name}</strong>
        <span class="dim">{focusNode.ecosystem}{focusNode.version ? ` · ${focusNode.version}` : ""} · {focusNode.id === ROOT ? "project" : focusNode.direct ? "direct" : "transitive"}{focusNode.dev ? " · dev" : ""}</span>
        {#if classification.items[focusNode.id]}
          <span>{classification.items[focusNode.id].basic ? "Basic" : "Chosen"}{classification.items[focusNode.id].reason ? ` — ${classification.items[focusNode.id].reason}` : ""}</span>
        {/if}
        <span class="dim">Depends on {dependsOn.length} · Required by {requiredBy.length}</span>
        {#if selected === focusNode.id}
          {#if dependsOn.length}<span class="dim">Depends on: {dependsOn.slice(0, 12).map(id => names.get(id) ?? id).join(", ")}{dependsOn.length > 12 ? ", …" : ""}</span>{/if}
          {#if requiredBy.length}<span class="dim">Required by: {requiredBy.slice(0, 12).map(id => names.get(id) ?? id).join(", ")}{requiredBy.length > 12 ? ", …" : ""}</span>{/if}
        {/if}
      </aside>
    {/if}
  </div>
</section>
<style>
  .page { height: 100%; min-height: 0; display: flex; flex-direction: column; gap: 10px; }
  .bar { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; }
  h2 { margin: 0; font-size: 18px; }
  select { width: auto; max-width: 42ch; }
  .meta { margin: 0; color: var(--text-dim); font-size: 12px; }
  .notes { margin: 0; padding-left: 18px; font-size: 12px; }
  .alert { margin: 0; padding: 10px; border: 1px solid var(--err); border-radius: 8px; white-space: pre-wrap; }
  .legend { list-style: none; margin: 0; padding: 0; display: flex; flex-wrap: wrap; gap: 4px 16px; font-size: 12px; }
  .legend li { display: inline-flex; align-items: center; gap: 6px; }
  .swatch { width: 12px; height: 12px; border-radius: 50%; flex: none; }
  .stage { position: relative; flex: 1; min-height: 0; border: 1px solid var(--line); border-radius: 8px; overflow: hidden; background: #0b1017; }
  .canvas { position: absolute; inset: 0; }
  .empty { position: absolute; inset: 0; display: grid; place-items: center; margin: 0; color: var(--text-faint); pointer-events: none; }
  .card { position: absolute; top: 10px; right: 10px; max-width: 280px; display: flex; flex-direction: column; gap: 4px; padding: 10px 12px; background: var(--panel); border: 1px solid var(--line); border-radius: 8px; font-size: 12px; overflow-wrap: anywhere; pointer-events: none; }
</style>
