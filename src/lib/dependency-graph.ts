export interface DepNode { id: string; name: string; version: string | null; ecosystem: "project" | "npm" | "pypi" | "maven" | "cargo"; direct: boolean; dev: boolean }
export interface DependencyGraph { folder: string; key: string; nodes: DepNode[]; edges: [string, string][]; notes: string[]; truncated: boolean }
export interface Classification { items: Record<string, { basic: boolean; reason: string }>; updatedAt: number | null }
export type DependencyMode = "direct" | "transitive";
export interface VisibleGraph { nodes: DepNode[]; edges: [string, string][]; hidden: number }

export const ROOT = "project";

/**
 * What the page draws. The base set is the project and its direct dependencies (`direct`) or everything
 * (`transitive`). Unless `showAll`, direct dependencies the Architect judged basic are removed together with every
 * package that can then no longer be reached from the project; unclassified packages stay.
 */
export function visibleGraph(graph: DependencyGraph, classification: Classification, mode: DependencyMode, showAll: boolean): VisibleGraph {
  const base = graph.nodes.filter(node => mode === "transitive" || node.id === ROOT || node.direct);
  const inBase = new Set(base.map(node => node.id));
  const edges = graph.edges.filter(([from, to]) => inBase.has(from) && inBase.has(to));
  if (showAll) return { nodes: base, edges, hidden: 0 };
  const removed = new Set(base.filter(node => node.direct && classification.items[node.id]?.basic === true).map(node => node.id));
  const outgoing = new Map<string, string[]>();
  for (const [from, to] of edges) {
    const list = outgoing.get(from);
    if (list) list.push(to); else outgoing.set(from, [to]);
  }
  const reached = new Set<string>();
  const queue = inBase.has(ROOT) ? [ROOT] : [];
  for (const id of queue) reached.add(id);
  for (let i = 0; i < queue.length; i++) {
    for (const next of outgoing.get(queue[i]) ?? []) {
      if (removed.has(next) || reached.has(next)) continue;
      reached.add(next);
      queue.push(next);
    }
  }
  const nodes = base.filter(node => reached.has(node.id));
  return { nodes, edges: edges.filter(([from, to]) => reached.has(from) && reached.has(to)), hidden: base.length - nodes.length };
}

/** Direct packages nobody has judged yet. */
export function pendingDirect(graph: DependencyGraph, classification: Classification): string[] {
  return graph.nodes.filter(node => node.direct && !(node.id in classification.items)).map(node => node.id);
}

export function nodeColor(node: DepNode, basic: boolean): string {
  if (node.id === ROOT) return "#4ea1ff";
  if (!node.direct) return "#61708a";
  return basic ? "#d29922" : "#3fb950";
}

export const nodeSize = (inDegree: number): number => Math.min(14, 4 + 2 * Math.log2(1 + inDegree));
