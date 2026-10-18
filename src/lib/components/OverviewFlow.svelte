<script lang="ts">
  import { SvelteFlow, Controls, MarkerType } from "@xyflow/svelte";
  import type { NodeTypes } from "@xyflow/svelte";
  import "@xyflow/svelte/dist/style.css";
  import type { Camera } from "../system-overview";
  import type { OverviewFlowEdge, OverviewFlowNode } from "../overview-flow";
  import { edgeDashed, gradeText, NODE_H, NODE_W, relationColor, type Projection } from "../system-graph";
  import OverviewNode from "./OverviewNode.svelte";

  let { projection, selectedNode, selectedEdge, matched, camera, onSelectNode, onSelectEdge, onToggle, onCamera }: {
    projection: Projection; selectedNode: string | null; selectedEdge: string | null; matched: ReadonlySet<string>; camera: Camera | null;
    onSelectNode: (id: string | null) => void; onSelectEdge: (id: string | null) => void; onToggle: (id: string) => void; onCamera: (camera: Camera) => void;
  } = $props();
  const nodeTypes: NodeTypes = { overview: OverviewNode };
  let nodes = $state.raw<OverviewFlowNode[]>([]);
  let edges = $state.raw<OverviewFlowEdge[]>([]);
  // The view is re-fitted only when the set of cards changes; panning and selecting never remount it.
  const signature = $derived(`${projection.nodes.length}:${projection.nodes.map(node => node.id).join("\u0000")}`);

  $effect(() => {
    nodes = projection.nodes.map((view): OverviewFlowNode => ({
      id: view.id, type: "overview", position: { x: view.x, y: view.y }, width: NODE_W, height: NODE_H, draggable: false, connectable: false,
      selected: view.id === selectedNode, data: { view, matched: matched.has(view.id), onToggle },
    }));
  });
  $effect(() => {
    edges = projection.edges.map((edge): OverviewFlowEdge => {
      const color = relationColor(edge.relation);
      const chosen = edge.id === selectedEdge;
      const width = (chosen ? 3.5 : 1.5) + Math.min(2.5, Math.log10(edge.count + 1));
      return {
        id: edge.id, source: edge.source, target: edge.target, type: "smoothstep", selectable: false, focusable: true, interactionWidth: 18,
        label: `${edge.relation}${edge.count > 1 ? ` ×${edge.count.toLocaleString()}` : ""}${edge.derived ? " (from calls)" : ""}${edge.cycle ? " ↺" : ""}`,
        labelStyle: "font-size:10px",
        ariaLabel: `${edge.relation} from ${edge.source} to ${edge.target}, ${gradeText(edge.grade)}`,
        markerEnd: { type: MarkerType.ArrowClosed, color },
        style: `stroke:${color};stroke-width:${width};stroke-dasharray:${edgeDashed(edge) ? "6 4" : "none"};opacity:${edge.response ? 0.75 : 1}`,
        data: { edge },
      };
    });
  });
</script>
<div class="flow" role="application" aria-label="System overview diagram">
  {#key signature}
    <SvelteFlow bind:nodes bind:edges {nodeTypes} colorMode="dark" fitView={!camera} fitViewOptions={{ maxZoom: 1, padding: 0.12 }}
      initialViewport={camera ? { x: camera.x, y: camera.y, zoom: camera.ratio } : undefined} minZoom={0.1}
      nodesDraggable={false} nodesConnectable={false} deleteKey={null} zoomOnDoubleClick={false}
      onnodeclick={({ node }) => onSelectNode(node.id)} onedgeclick={({ edge }) => onSelectEdge(edge.id)} onpaneclick={() => { onSelectNode(null); onSelectEdge(null); }}
      onmoveend={(_event, viewport) => onCamera({ x: viewport.x, y: viewport.y, ratio: viewport.zoom })}>
      <Controls showLock={false} />
    </SvelteFlow>
  {/key}
</div>
<style>
  .flow { width: 100%; height: 100%; min-height: 0; }
  :global(.svelte-flow__attribution a) { pointer-events: none; }
</style>
