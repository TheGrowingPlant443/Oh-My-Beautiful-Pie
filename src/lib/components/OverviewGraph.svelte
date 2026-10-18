<script module lang="ts">
  // Layouts found earlier, per projection key, so returning to a view starts where its nodes were.
  const layoutCache = new Map<string, Map<string, { x: number; y: number }>>();
  function remember(key: string, positions: Map<string, { x: number; y: number }>): void {
    layoutCache.delete(key);
    layoutCache.set(key, positions);
    while (layoutCache.size > 6) layoutCache.delete(layoutCache.keys().next().value as string);
  }
</script>
<script lang="ts">
  import { untrack } from "svelte";
  import Graph from "graphology";
  import Sigma from "sigma";
  import { extremityArrow } from "sigma/rendering";
  import LayoutWorker from "../overview-layout.worker?worker";
  import { LayoutClient, positionMap, type LayoutPositions, type LayoutStatus, type LayoutWorkerLike } from "../overview-layout";
  import type { Camera } from "../system-overview";
  import { COLUMN_W, gradeProven, relationColor, ROW_H, type Projection, type ViewNode } from "../system-graph";

  let { projection, layoutKey, selectedNode, selectedEdge, matched, camera, onSelectNode, onSelectEdge, onCamera, onLayout }: {
    projection: Projection; layoutKey: string; selectedNode: string | null; selectedEdge: string | null; matched: ReadonlySet<string>; camera: Camera | null;
    onSelectNode: (id: string | null) => void; onSelectEdge: (id: string | null) => void; onCamera: (camera: Camera) => void; onLayout: (status: LayoutStatus) => void;
  } = $props();

  let host = $state<HTMLDivElement>();
  // Only the refresh handle is needed outside the build effect (Sigma's generic parameters depend on the primitives passed).
  let renderer: { refresh: () => void } | null = null;
  let graph: Graph | null = null;
  const client = new LayoutClient(() => new LayoutWorker() as unknown as LayoutWorkerLike);

  function nodeColor(node: ViewNode): string {
    if (node.cyclic && node.memberCount > 1) return "#a371f7";
    switch (node.evidence) {
      case "parsed": return node.kind === "file" ? "#3fb950" : node.kind === "directory" ? "#61708a" : "#4ea1ff";
      case "partial": return "#d29922";
      case "failed": return "#f85149";
      default: return node.kind === "directory" ? "#61708a" : "#8b9bb4";
    }
  }
  const nodeSize = (node: ViewNode): number => 4 + 2 * Math.log2(1 + node.degree) + (node.collapsed ? 3 + Math.log2(1 + node.memberCount) : 0);

  // Rebuild only when the projection (the shared induced subgraph) changes; selection and camera never rebuild it.
  $effect(() => {
    const view = projection;
    const container = host;
    const key = layoutKey;
    if (!container) return;
    return untrack(() => {
      const started = performance.now();
      const cached = layoutCache.get(key);
      const maxX = Math.max(1, ...view.nodes.map(node => Math.abs(node.x))), maxY = Math.max(1, ...view.nodes.map(node => Math.abs(node.y)));
      const built = new Graph({ type: "directed", multi: true });
      for (const [index, node] of view.nodes.entries()) {
        const start = cached?.get(node.id) ?? { x: (node.x / Math.max(maxX, COLUMN_W)) * 100 + (index % 7) * 1e-3, y: (node.y / Math.max(maxY, ROW_H)) * 100 + (index % 11) * 1e-3 };
        built.addNode(node.id, { label: node.label, labelVisibility: view.nodes.length <= 60 ? "visible" : "auto", size: nodeSize(node), color: nodeColor(node), x: start.x, y: start.y });
      }
      for (const edge of view.edges) {
        built.addEdgeWithKey(edge.id, edge.source, edge.target, { size: 1 + Math.min(3, Math.log10(edge.count + 1) * 1.5), color: relationColor(edge.relation), opacity: gradeProven(edge.grade) && !edge.response ? 1 : 0.45 });
      }
      const sigma = new Sigma(built, container, {
        primitives: { edges: { extremities: [extremityArrow({ lengthRatio: 4, widthRatio: 3 })], defaultHead: "arrow" } },
        settings: { enableCameraRotation: false, enableCameraMouseRotation: false, enableCameraPanning: true, enableCameraZooming: true, minCameraRatio: 0.02, maxCameraRatio: 5, renderEdgeLabels: false, labelRenderedSizeThreshold: 6, labelDensity: 0.6, itemSizesReference: "screen", enableEdgeEvents: true },
        styles: {
          nodes: [{ color: { attribute: "color" }, size: { attribute: "size", defaultValue: 6 }, label: { attribute: "label" }, labelVisibility: { attribute: "labelVisibility", defaultValue: "auto" }, labelColor: "#dfe6ef" }, { whenState: "isHovered", then: { labelVisibility: "visible" } }],
          edges: [{ color: { attribute: "color" }, size: { attribute: "size", defaultValue: 1 }, opacity: { attribute: "opacity", defaultValue: 1 } }],
        },
      });
      renderer = sigma;
      graph = built;
      if (camera) sigma.getCamera().setState({ x: camera.x, y: camera.y, ratio: camera.ratio, angle: 0 });
      let cameraTimer: ReturnType<typeof setTimeout> | null = null;
      const moved = (): void => {
        if (cameraTimer) clearTimeout(cameraTimer);
        cameraTimer = setTimeout(() => { const state = sigma.getCamera().getState(); onCamera({ x: state.x, y: state.y, ratio: state.ratio }); }, 250);
      };
      sigma.getCamera().on("updated", moved);
      sigma.on("clickNode", ({ node }) => onSelectNode(node));
      sigma.on("clickEdge", ({ edge }) => onSelectEdge(edge));
      sigma.on("clickStage", () => { onSelectNode(null); onSelectEdge(null); });
      let live = true;
      if (cached) onLayout({ state: "done", message: "Reused the earlier layout", milliseconds: 0 });
      else if (view.nodes.length > 1) {
        onLayout({ state: "running", message: null, milliseconds: 0 });
        const apply = (positions: LayoutPositions): void => {
          if (!live || graph !== built) return;
          positions.ids.forEach((id, i) => { if (built.hasNode(id)) { built.setNodeAttribute(id, "x", positions.x[i]); built.setNodeAttribute(id, "y", positions.y[i]); } });
          sigma.refresh();
        };
        const seeds = new Map(view.nodes.map(node => [node.id, { x: built.getNodeAttribute(node.id, "x") as number, y: built.getNodeAttribute(node.id, "y") as number }]));
        void client.run({
          ids: view.nodes.map(node => node.id), seeds, edges: view.edges.map(edge => [edge.source, edge.target] as const),
          iterations: Math.min(400, Math.max(60, Math.round(2_000_000 / view.nodes.length))), onProgress: apply,
        }).then(positions => {
          if (!positions || !live) return;
          apply(positions);
          remember(key, positionMap(positions));
          onLayout({ state: "done", message: null, milliseconds: Math.round(performance.now() - started) });
        }, error => { if (live) onLayout({ state: "error", message: error instanceof Error ? error.message : String(error), milliseconds: 0 }); });
      } else onLayout({ state: "done", message: null, milliseconds: 0 });
      return () => {
        live = false;
        client.cancel();
        if (cameraTimer) clearTimeout(cameraTimer);
        sigma.getCamera().removeListener("updated", moved);
        sigma.kill();
        if (renderer === sigma) { renderer = null; graph = null; }
      };
    });
  });

  // Selection and search highlighting change attributes only.
  $effect(() => {
    const picked = selectedNode, edgePicked = selectedEdge, hits = matched;
    const view = projection;
    const built = graph;
    if (!built || !renderer) return;
    for (const node of view.nodes) {
      if (!built.hasNode(node.id)) continue;
      const on = node.id === picked, hit = hits.has(node.id);
      built.setNodeAttribute(node.id, "color", on ? "#ffffff" : hit ? "#d29922" : nodeColor(node));
      built.setNodeAttribute(node.id, "labelVisibility", on || hit || view.nodes.length <= 60 ? "visible" : "auto");
    }
    for (const edge of view.edges) {
      if (!built.hasEdge(edge.id)) continue;
      built.setEdgeAttribute(edge.id, "size", (edge.id === edgePicked ? 4 : 1) + Math.min(3, Math.log10(edge.count + 1) * 1.5));
    }
    renderer.refresh();
  });
  $effect(() => () => { client.dispose(); });
</script>
<div class="sigma" bind:this={host} role="application" aria-label="Large dependency graph"></div>
<style>
  .sigma { width: 100%; height: 100%; min-height: 0; background: #0d131c; }
</style>
