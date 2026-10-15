import type { Node } from "@xyflow/svelte";
import type { GraphCommit } from "./api";

export const NODE_W = 300, NODE_H = 76, LANE_W = 330, ROW_H = 100;
export type CommitNodeData = { commit: GraphCommit; head: boolean; onOpen: (hash: string) => void };
export type CommitFlowNode = Node<CommitNodeData, "commit">;

/** Input is Git's child-before-parent topo order. Each active lane reserves the next expected parent. */
export function layoutGraph(commits: GraphCommit[]): {
  positions: Map<string, { lane: number; row: number }>;
  edges: { id: string; source: string; target: string }[];
  lanes: number;
} {
  const slots: (string | null)[] = [];
  const positions = new Map<string, { lane: number; row: number }>();
  for (const [row, commit] of commits.entries()) {
    let lane = slots.indexOf(commit.hash);
    if (lane < 0) {
      lane = slots.indexOf(null);
      if (lane < 0) lane = slots.length;
    }
    for (let other = 0; other < slots.length; other++) if (other !== lane && slots[other] === commit.hash) slots[other] = null;
    positions.set(commit.hash, { lane, row });
    slots[lane] = commit.parents[0] ?? null;
    for (const parent of commit.parents.slice(1)) {
      if (slots.includes(parent)) continue;
      const free = slots.indexOf(null);
      slots[free < 0 ? slots.length : free] = parent;
    }
  }
  const edges = commits.flatMap(commit => commit.parents.filter(parent => positions.has(parent)).map(parent => ({
    id: `${commit.hash}>${parent}`, source: commit.hash, target: parent,
  })));
  return { positions, edges, lanes: slots.length };
}

export function parseRefs(refs: string[]): { name: string; kind: "head" | "branch" | "remote" | "tag" }[] {
  return refs.map(ref => {
    if (ref.startsWith("HEAD -> ")) return { name: ref.slice(8), kind: "head" };
    if (ref === "HEAD") return { name: ref, kind: "head" };
    if (ref.startsWith("tag: ")) return { name: ref.slice(5), kind: "tag" };
    return { name: ref, kind: ref.startsWith("origin/") || ref.startsWith("upstream/") ? "remote" : "branch" };
  });
}
