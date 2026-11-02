import type { Node } from "@xyflow/svelte";
import type { GraphCommit } from "./api";

export const NODE_W = 300, NODE_H = 76, LANE_W = 330, ROW_H = 100;
/** The first view fits this many commits from the top of the history. */
export const INITIAL_FIT_NODES = 10;
export const PR_COLORS = ["#4ea1ff", "#f2994a", "#27ae60", "#bb6bd9", "#eb5757", "#f2c94c", "#2dd4bf", "#ff7ab6"];
export const LEGEND_PEOPLE = 3;
export type PrMark = { color: string; role: "member" | "merge" };
export type CommitNodeData = {
  commit: GraphCommit; head: boolean; onOpen: (hash: string) => void;
  pr: PrMark | null; onModifierClick: (event: MouseEvent, hash: string) => void;
};
export type CommitFlowNode = Node<CommitNodeData, "commit">;

export interface PullRequestBranch { merge: string; subject: string; members: string[]; authors: string[]; key: string; color: string }
export interface LegendEntry { color: string; names: string[]; more: boolean; label: string }

/** The default branch's first-parent chain, newest first. */
function firstParentChain(byHash: ReadonlyMap<string, GraphCommit>, mainCommit: string): GraphCommit[] {
  const main: GraphCommit[] = [];
  const onMain = new Set<string>();
  for (let hash: string | undefined = mainCommit; hash && !onMain.has(hash);) {
    const commit = byHash.get(hash);
    if (!commit) break;
    main.push(commit);
    onMain.add(hash);
    hash = commit.parents[0];
  }
  return main;
}

/** Commits reachable from the merge's other parents that no earlier merge claimed (git's `M^1..M^2`). */
function mergeMembers(merge: GraphCommit, byHash: ReadonlyMap<string, GraphCommit>, seen: Set<string>): string[] {
  const members: string[] = [];
  for (const parent of merge.parents.slice(1)) {
    const stack = [parent];
    while (stack.length) {
      const hash = stack.pop()!;
      const commit = byHash.get(hash);
      if (!commit || seen.has(hash)) continue;
      seen.add(hash);
      members.push(hash);
      stack.push(...commit.parents);
    }
  }
  seen.add(merge.hash);
  return members;
}

/** Authors by commit count, then name. */
function authorsByCount(members: readonly string[], byHash: ReadonlyMap<string, GraphCommit>): string[] {
  const counts = new Map<string, number>();
  for (const hash of members) {
    const author = byHash.get(hash)!.author;
    counts.set(author, (counts.get(author) ?? 0) + 1);
  }
  return [...counts.keys()].sort((a, b) => counts.get(b)! - counts.get(a)! || a.localeCompare(b));
}

/**
 * Merge commits on the default branch's first-parent chain, with the commits their other parents brought in
 * (git's `M^1..M^2`). Colour is per distinct set of authors, assigned in first-seen order, newest merge first.
 */
export function mergedPullRequests(commits: GraphCommit[], mainCommit: string | null): PullRequestBranch[] {
  if (!mainCommit) return [];
  const byHash = new Map(commits.map(commit => [commit.hash, commit]));
  const order = new Map(commits.map((commit, index) => [commit.hash, index]));
  const seen = new Set<string>();
  const found: Omit<PullRequestBranch, "color">[] = [];
  for (const merge of firstParentChain(byHash, mainCommit).reverse()) {
    const members = mergeMembers(merge, byHash, seen);
    if (!members.length) continue;
    members.sort((a, b) => order.get(a)! - order.get(b)!);
    const authors = authorsByCount(members, byHash);
    found.push({ merge: merge.hash, subject: merge.subject, members, authors, key: [...authors].sort((a, b) => a.localeCompare(b)).join("\n") });
  }
  found.reverse();
  const colors = new Map<string, string>();
  return found.map(pr => {
    if (!colors.has(pr.key)) colors.set(pr.key, PR_COLORS[colors.size % PR_COLORS.length]);
    return { ...pr, color: colors.get(pr.key)! };
  });
}

export function legendOf(prs: PullRequestBranch[]): LegendEntry[] {
  const entries = new Map<string, LegendEntry>();
  for (const pr of prs) {
    if (entries.has(pr.key)) continue;
    const names = pr.authors.slice(0, LEGEND_PEOPLE);
    const more = pr.authors.length > LEGEND_PEOPLE;
    entries.set(pr.key, { color: pr.color, names, more, label: names.join(", ") + (more ? ", ..." : "") });
  }
  return [...entries.values()];
}

/** The colour of an edge inside a PR branch, or from its merge commit down to the branch tip. */
export function prEdgeColor(prs: PullRequestBranch[]): (source: string, target: string) => string | null {
  const owner = new Map<string, PullRequestBranch>();
  for (const pr of prs) for (const hash of pr.members) owner.set(hash, pr);
  return (source, target) => {
    const pr = owner.get(target);
    return pr && (owner.get(source) === pr || source === pr.merge) ? pr.color : null;
  };
}

function laneFor(slots: (string | null)[], hash: string): number {
  const lane = slots.indexOf(hash);
  if (lane >= 0) return lane;
  const free = slots.indexOf(null);
  return free < 0 ? slots.length : free;
}

function reserveParents(slots: (string | null)[], parents: string[]): void {
  for (const parent of parents) {
    if (slots.includes(parent)) continue;
    const free = slots.indexOf(null);
    slots[free < 0 ? slots.length : free] = parent;
  }
}

/** Input is Git's child-before-parent topo order. Each active lane reserves the next expected parent. */
export function layoutGraph(commits: GraphCommit[]): {
  positions: Map<string, { lane: number; row: number }>;
  edges: { id: string; source: string; target: string }[];
  lanes: number;
} {
  const slots: (string | null)[] = [];
  const positions = new Map<string, { lane: number; row: number }>();
  for (const [row, commit] of commits.entries()) {
    const lane = laneFor(slots, commit.hash);
    for (let other = 0; other < slots.length; other++) if (other !== lane && slots[other] === commit.hash) slots[other] = null;
    positions.set(commit.hash, { lane, row });
    slots[lane] = commit.parents[0] ?? null;
    reserveParents(slots, commit.parents.slice(1));
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
