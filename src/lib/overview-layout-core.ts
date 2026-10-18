import Graph from "graphology";
import forceAtlas2 from "graphology-layout-forceatlas2";

/** Bump when the message shapes change: both sides refuse a different version instead of guessing. */
export const LAYOUT_PROTOCOL_VERSION = 1;

export interface LayoutJob {
  /** x, y per node in the caller's node order. */
  seeds: Float32Array;
  /** from, to node-index pairs. */
  edges: Uint32Array;
  iterations: number;
}

export interface LayoutHooks {
  cancelled(): boolean;
  /** Gives the worker's event loop a turn so a cancel message can arrive between chunks. */
  yieldNow(): Promise<void>;
  progress?(positions: Float32Array, iteration: number): void;
}

/** Deterministic distinct start position from an FNV-1a hash (ForceAtlas2 needs distinct positions). */
export function seededPosition(id: string, index: number): { x: number; y: number } {
  let h = 2166136261;
  for (let i = 0; i < id.length; i++) { h ^= id.charCodeAt(i); h = Math.imul(h, 16777619); }
  const a = (h >>> 0) / 4294967296;
  h = Math.imul(h ^ (h >>> 15), 2246822507);
  h ^= h >>> 13;
  const b = (h >>> 0) / 4294967296;
  // The index term keeps nodes with colliding hashes apart.
  return { x: (a - 0.5) * 200 + (index % 7) * 1e-3, y: (b - 0.5) * 200 + (index % 11) * 1e-3 };
}

const readPositions = (graph: Graph, count: number): Float32Array => {
  const out = new Float32Array(count * 2);
  for (let i = 0; i < count; i++) {
    const attributes = graph.getNodeAttributes(String(i));
    out[2 * i] = attributes.x as number;
    out[2 * i + 1] = attributes.y as number;
  }
  return out;
};

/** ForceAtlas2 in chunks. Returns null when cancelled; positions of an unfinished job are never returned as final. */
export async function runLayout(job: LayoutJob, hooks: LayoutHooks): Promise<Float32Array | null> {
  const count = job.seeds.length / 2;
  if (!Number.isInteger(count)) throw new Error("Layout seeds must hold x and y for every node");
  if (job.edges.length % 2 !== 0) throw new Error("Layout edges must hold from and to for every edge");
  if (count === 0) return new Float32Array(0);
  const graph = new Graph({ type: "directed", multi: false, allowSelfLoops: false });
  for (let i = 0; i < count; i++) graph.addNode(String(i), { x: job.seeds[2 * i], y: job.seeds[2 * i + 1] });
  for (let e = 0; e < job.edges.length; e += 2) {
    const from = job.edges[e], to = job.edges[e + 1];
    if (from >= count || to >= count) throw new Error("Layout edge refers to a node that does not exist");
    if (from !== to) graph.mergeEdge(String(from), String(to));
  }
  const settings = { ...forceAtlas2.inferSettings(graph), barnesHutOptimize: count > 500, slowDown: 1 + Math.log(count) };
  const chunk = count > 2000 ? 8 : 25;
  let done = 0;
  while (done < job.iterations) {
    if (hooks.cancelled()) return null;
    const step = Math.min(chunk, job.iterations - done);
    forceAtlas2.assign(graph, { iterations: step, settings });
    done += step;
    await hooks.yieldNow();
    if (hooks.cancelled()) return null;
    hooks.progress?.(readPositions(graph, count), done);
  }
  return readPositions(graph, count);
}
