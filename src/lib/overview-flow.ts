import type { Edge, Node } from "@xyflow/svelte";
import type { ViewEdge, ViewNode } from "./system-graph";

/** What a card needs to draw itself; every string is rendered as plain text. */
export interface OverviewNodeData extends Record<string, unknown> {
  view: ViewNode;
  /** Highlighted by the search box. */
  matched: boolean;
  /** Opens or closes the container this card stands for. */
  onToggle: (id: string) => void;
}
export type OverviewFlowNode = Node<OverviewNodeData, "overview">;

export interface OverviewEdgeData extends Record<string, unknown> { edge: ViewEdge }
export type OverviewFlowEdge = Edge<OverviewEdgeData>;
