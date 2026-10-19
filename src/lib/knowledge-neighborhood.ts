import { buildIndex, projectGraph, type GraphIndex, type LoopKind, type Projection } from "./system-graph";

/** A displayed page of exact incident model records, not a limit on the searchable underlying graph. */
export interface KnowledgeNeighborhood {
  projection: Projection;
  centerId: string;
  page: number;
  pages: number;
  records: number;
  first: number;
  last: number;
}
export const NEIGHBORHOOD_PAGE_RECORDS = 199;

/**
 * Always pins the requested node, even in a flat graph with millions of nodes or an oversized container which fitting would collapse.
 * Each page contains at most 199 original incident records plus the center, so neither node fitting nor edge fitting can hide it.
 * Opposite/parallel records retain their ids and direction. The full index remains the search/inspector authority.
 */
export function knowledgeNeighborhood(index: GraphIndex, centerId: string, page: number, loopKind: LoopKind): KnowledgeNeighborhood | null {
  const center = index.nodes.get(centerId);
  if (!center) return null;
  const incident = index.edges.filter(edge => edge.source === centerId || edge.target === centerId);
  const pages = Math.max(1, Math.ceil(incident.length / NEIGHBORHOOD_PAGE_RECORDS));
  const selectedPage = Math.max(0, Math.min(page, pages - 1));
  const first = selectedPage * NEIGHBORHOOD_PAGE_RECORDS;
  const edges = incident.slice(first, first + NEIGHBORHOOD_PAGE_RECORDS);
  const ids = new Set([centerId]);
  for (const edge of edges) { ids.add(edge.source); ids.add(edge.target); }
  const nodes = [...ids].map(id => {
    const original = index.nodes.get(id)!;
    return { ...original, parentId: null, lazyMembers: [], synthetic: false, selfLoops: id === centerId ? original.selfLoops : 0 };
  });
  return {
    projection: projectGraph(buildIndex({ nodes, edges }), { mode: "source", expanded: new Set(), loopKind }),
    centerId, page: selectedPage, pages, records: incident.length, first, last: first + edges.length,
  };
}
