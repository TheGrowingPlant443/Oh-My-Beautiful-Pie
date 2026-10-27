export interface DepNode { id: string; name: string; version: string | null; resolvedVersions: string[]; declaredVersions: string[]; ecosystem: "project" | "npm" | "pypi" | "maven" | "cargo"; direct: boolean; dev: boolean }
export interface DependencyGraph { folder: string; key: string; nodes: DepNode[]; edges: [string, string][]; notes: string[]; truncated: boolean }
export interface Classification { items: Record<string, { basic: boolean; reason: string }>; updatedAt: number | null }
export interface KnownStandard { items: Record<string, string>; counts: Record<string, number>; derivedCounts: Record<string, number> }
export type DependencyMode = "direct" | "transitive";
export interface VisibleGraph { nodes: DepNode[]; edges: [string, string][]; hidden: number; standardHidden: number; differentHidden: number; commonHidden: number; ownersHidden: number; depthHidden: number; maxDepth: number }
/** Extra node ids removed (with whatever is only reachable through them) after the basic-direct pass; ignored when showing all. */
export interface HideSets { known?: ReadonlySet<string> | null; different?: ReadonlySet<string> | null; common?: ReadonlySet<string> | null; owners?: ReadonlySet<string> | null }
export interface DerivedPackage { id: string; versions: string[]; standardVersions: string[] }
export interface DerivedStandard { complete: boolean; hide: string[]; different: DerivedPackage[]; unproven: DerivedPackage[]; learned: number }

export const ROOT = "project";

export type PackageEcosystem = Exclude<DepNode["ecosystem"], "project">;
export type EcosystemChoice = "all" | PackageEcosystem;

/**
 * The package-language selection that is valid for the ecosystems actually present. A selector exists only when more than one
 * package ecosystem is present, so a lone ecosystem (or a language the scan no longer contains) means "all".
 */
export function validEcosystemChoice(choice: EcosystemChoice, ecosystems: ReadonlySet<string>): EcosystemChoice {
  return choice !== "all" && ecosystems.size > 1 && ecosystems.has(choice) ? choice : "all";
}

/**
 * Keeps the synthetic root, the packages of one ecosystem and only the edges whose two endpoints survive. Edges are never
 * invented (a package whose parents were in another ecosystem stays an orphan, which only Show all displays). "all" is the scan itself.
 */
export function filterGraphByEcosystem(graph: DependencyGraph, choice: EcosystemChoice): DependencyGraph {
  if (choice === "all") return graph;
  const nodes = graph.nodes.filter(node => node.id === ROOT || node.ecosystem === choice);
  const kept = new Set(nodes.map(node => node.id));
  return { ...graph, nodes, edges: graph.edges.filter(([from, to]) => kept.has(from) && kept.has(to)) };
}

function reachableDepths(outgoing: ReadonlyMap<string, string[]>, hasRoot: boolean, removed?: ReadonlySet<string>): { depths: Map<string, number>; maxDepth: number } {
  const depths = new Map<string, number>();
  const queue = hasRoot ? [ROOT] : [];
  if (hasRoot) depths.set(ROOT, 0);
  let maxDepth = 0;
  for (const current of queue) {
    const depth = depths.get(current)! + 1;
    for (const next of outgoing.get(current) ?? []) {
      if (removed?.has(next) || depths.has(next)) continue;
      depths.set(next, depth);
      maxDepth = Math.max(maxDepth, depth);
      queue.push(next);
    }
  }
  return { depths, maxDepth };
}

function removeHiddenNodes(base: readonly DepNode[], hidden: ReadonlySet<string>, removed: Set<string>): void {
  for (const node of base) {
    if (node.id !== ROOT && hidden.has(node.id)) removed.add(node.id);
  }
}

function outgoingMap(edges: readonly (readonly [string, string])[]): Map<string, string[]> {
  const outgoing = new Map<string, string[]>();
  for (const [from, to] of edges) {
    const list = outgoing.get(from);
    if (list) list.push(to); else outgoing.set(from, [to]);
  }
  return outgoing;
}

/**
 * Basic direct packages and packages only reachable through them are removed first. A layer cap then
 * uses the shortest surviving path from the project (root 0, direct 1), independently of that count.
 * All preserves the existing unbounded base set, including scanner nodes without known edges.
 */
export function visibleGraph(graph: DependencyGraph, classification: Classification, mode: DependencyMode, showAll: boolean, layers: number | null = null, hide: HideSets = {}): VisibleGraph {
  const base = graph.nodes.filter(node => mode === "transitive" || node.id === ROOT || node.direct);
  const inBase = new Set(base.map(node => node.id));
  const edges = graph.edges.filter(([from, to]) => inBase.has(from) && inBase.has(to));
  const removed = new Set(showAll ? [] : base.filter(node => node.direct && classification.items[node.id]?.basic === true).map(node => node.id));
  const outgoing = outgoingMap(edges);
  let { depths, maxDepth } = reachableDepths(outgoing, inBase.has(ROOT), removed);
  const survivors = () => showAll ? base.length : base.filter(node => depths.has(node.id)).length;
  const afterBasic = survivors();
  // Each extra filter removes its non-root members and whatever is then only reachable through them.
  const counts: number[] = [];
  let before = afterBasic;
  for (const set of [hide.known, hide.different, hide.common, hide.owners]) {
    if (!showAll && set?.size) {
      removeHiddenNodes(base, set, removed);
      ({ depths, maxDepth } = reachableDepths(outgoing, inBase.has(ROOT), removed));
    }
    const after = survivors();
    counts.push(before - after);
    before = after;
  }
  const surviving = showAll ? base : base.filter(node => depths.has(node.id));
  const nodes = mode === "transitive" && layers !== null
    ? surviving.filter(node => (depths.get(node.id) ?? Infinity) <= layers)
    : surviving;
  const included = new Set(nodes.map(node => node.id));
  return {
    nodes, edges: edges.filter(([from, to]) => included.has(from) && included.has(to)),
    hidden: base.length - afterBasic,
    standardHidden: counts[0], differentHidden: counts[1], commonHidden: counts[2], ownersHidden: counts[3],
    depthHidden: surviving.length - nodes.length, maxDepth,
  };
}

/** Direct packages nobody has judged yet. */
export function pendingDirect(graph: DependencyGraph, classification: Classification, known: ReadonlySet<string> = new Set()): string[] {
  return graph.nodes.filter(node => node.direct && !(node.id in classification.items) && !known.has(node.id)).map(node => node.id);
}

/** Pink marks a transitive package used in a proven different version than the built-in packages use; it overrides the age warning. */
export function nodeColor(node: DepNode, basic: boolean, majorLag: number | null = null, different = false): string {
  if (node.id === ROOT) return "#4ea1ff";
  if (different) return "#ff69b4";
  if (majorLag !== null && majorLag >= 2) return "#f85149";
  if (!node.direct) return "#61708a";
  return basic ? "#d29922" : "#3fb950";
}

export const nodeSize = (inDegree: number): number => Math.min(14, 4 + 2 * Math.log2(1 + inDegree));

function reachableNodes(start: string, adjacency: ReadonlyMap<string, string[]>): Set<string> {
  const seen = new Set([start]), queue = [start];
  for (const current of queue) {
    for (const next of adjacency.get(current) ?? []) {
      if (!seen.has(next)) { seen.add(next); queue.push(next); }
    }
  }
  return seen;
}

function includeDescendants(included: Set<string>, target: string, forward: ReadonlyMap<string, string[]>, limit: number | null): void {
  // Shortest depth from the project decides which descendants fall inside the layer limit.
  const { depths } = reachableDepths(forward, true);
  const seen = new Set([target]), pending = [target];
  for (const current of pending) {
    for (const next of forward.get(current) ?? []) {
      if (seen.has(next) || (limit !== null && (depths.get(next) ?? Infinity) > limit)) continue;
      seen.add(next); pending.push(next); included.add(next);
    }
  }
}

/**
 * The packages on every project-to-target path. `descendants` additionally keeps the target's own transitive packages:
 * undefined none, null all of them, a number only those within that many layers of the project.
 */
export function pathToNode(view: VisibleGraph, target: string, descendants?: number | null): VisibleGraph | null {
  const ids = new Set(view.nodes.map(node => node.id));
  if (target === ROOT || !ids.has(ROOT) || !ids.has(target)) return null;
  const edges = view.edges.filter(([from, to]) => ids.has(from) && ids.has(to));
  const forward = outgoingMap(edges), backward = outgoingMap(edges.map(([from, to]): [string, string] => [to, from]));
  const fromRoot = reachableNodes(ROOT, forward);
  if (!fromRoot.has(target)) return null;
  const toTarget = reachableNodes(target, backward);
  const included = new Set([...fromRoot].filter(id => toTarget.has(id)));
  if (descendants !== undefined) includeDescendants(included, target, forward, descendants);
  return {
    ...view, nodes: view.nodes.filter(node => included.has(node.id)),
    edges: view.edges.filter(([from, to]) => included.has(from) && included.has(to)),
  };
}

export function focusCamera(center: { x: number; y: number }, neighbors: readonly { x: number; y: number }[], span: { width: number; height: number }): { x: number; y: number; ratio: number } {
  if (!neighbors.length) return { ...center, ratio: 0.25 };
  let dx = 0, dy = 0;
  for (const neighbor of neighbors) {
    dx = Math.max(dx, Math.abs(neighbor.x - center.x));
    dy = Math.max(dy, Math.abs(neighbor.y - center.y));
  }
  return { ...center, ratio: Math.min(1, Math.max(0.1, 1.3 * Math.max(2 * dx / span.width, 2 * dy / span.height))) };
}

/**
 * Unique scanned package identities (never versions, never the synthetic root). Built in = project judgement, the global
 * known-standard memory or a derived built-in transitive; pink and unproven-version packages are not in `derivedBuiltIn`.
 * Packages without any judgement stay in `nonBuiltIn` and are also counted as `unclassified`.
 */
export function packageTotals(graph: DependencyGraph, classification: Classification, knownIds: ReadonlySet<string>, derivedBuiltIn: ReadonlySet<string>): { total: number; nonBuiltIn: number; unclassified: number } {
  const seen = new Set<string>();
  let nonBuiltIn = 0, unclassified = 0;
  for (const node of graph.nodes) {
    if (node.id === ROOT || seen.has(node.id)) continue;
    seen.add(node.id);
    const judgement = classification.items[node.id];
    if (judgement?.basic === true || knownIds.has(node.id) || derivedBuiltIn.has(node.id)) continue;
    nonBuiltIn++;
    if (!judgement) unclassified++;
  }
  return { total: seen.size, nonBuiltIn, unclassified };
}

export interface PackageSearchEntry { node: DepNode; searchName: string; searchId: string }

/** One lowercase index per raw scan; the synthetic root is not a package. Retains node references and never mutates the scanner arrays. */
export function buildPackageSearchIndex(nodes: readonly DepNode[]): PackageSearchEntry[] {
  const index: PackageSearchEntry[] = [];
  for (const node of nodes) if (node.id !== ROOT) index.push({ node, searchName: node.name.toLowerCase(), searchId: node.id.toLowerCase() });
  return index;
}

function compareEntries(a: PackageSearchEntry, b: PackageSearchEntry): number {
  if (a.searchName !== b.searchName) return a.searchName < b.searchName ? -1 : 1;
  if (a.node.name !== b.node.name) return a.node.name < b.node.name ? -1 : 1;
  if (a.node.id === b.node.id) return 0;
  return a.node.id < b.node.id ? -1 : 1;
}

function searchTier(entry: PackageSearchEntry, needle: string): number {
  if (entry.searchName === needle || entry.searchId === needle) return 0;
  if (entry.searchName.startsWith(needle) || entry.searchId.startsWith(needle)) return 1;
  if (entry.searchName.includes(needle) || entry.searchId.includes(needle)) return 2;
  return -1;
}

/** Literal, case-insensitive name/id substring search: exact matches, then prefixes, then other substrings. Empty query finds nothing. */
export function searchPackages(index: readonly PackageSearchEntry[], query: string): DepNode[] {
  const needle = query.trim().toLowerCase();
  if (!needle) return [];
  const hits: { entry: PackageSearchEntry; tier: number }[] = [];
  for (const entry of index) {
    const tier = searchTier(entry, needle);
    if (tier >= 0) hits.push({ entry, tier });
  }
  hits.sort((a, b) => a.tier - b.tier || compareEntries(a.entry, b.entry));
  return hits.map(hit => hit.entry.node);
}

/** Enter on a query equal to a package name selects the exact name match with the lowest id. */
export function exactNameMatch(index: readonly PackageSearchEntry[], query: string): DepNode | null {
  const needle = query.trim().toLowerCase();
  if (!needle) return null;
  let best: PackageSearchEntry | null = null;
  for (const entry of index) {
    if (entry.searchName === needle && (best === null || entry.node.id < best.node.id)) best = entry;
  }
  return best?.node ?? null;
}

export const DEFAULT_LAYERS = 2;
export interface DependencyPrefs {
  folder: string; mode: DependencyMode; showAll: boolean; hideStandard: boolean; hidePink: boolean; hideCommon: boolean; hideOwners: boolean;
  presentation: "graph" | "list"; layers: number | null;
}

/**
 * Saved view preferences. An explicit stored `layers: null` means All and a positive safe integer is kept; a missing or
 * invalid value falls back to 2. Removing known standard packages defaults ON only when no boolean was ever saved.
 */
export function parseDependencyPrefs(raw: string | null): DependencyPrefs {
  const fallback: DependencyPrefs = { folder: "", mode: "direct", showAll: false, hideStandard: true, hidePink: false, hideCommon: false, hideOwners: false, presentation: "graph", layers: DEFAULT_LAYERS };
  try {
    const value = JSON.parse(raw ?? "null") as Record<string, unknown> | null;
    if (value === null || typeof value !== "object" || Array.isArray(value)) return fallback;
    let layers: number | null = DEFAULT_LAYERS;
    if (value.layers === null) layers = null;
    else if (typeof value.layers === "number" && Number.isSafeInteger(value.layers) && value.layers >= 1) layers = value.layers;
    return {
      folder: typeof value.folder === "string" ? value.folder : "",
      mode: value.mode === "transitive" ? "transitive" : "direct",
      showAll: value.showAll === true,
      hideStandard: typeof value.hideStandard === "boolean" ? value.hideStandard : true,
      hidePink: value.hidePink === true,
      hideCommon: value.hideCommon === true,
      hideOwners: value.hideOwners === true,
      presentation: value.presentation === "list" ? "list" : "graph",
      layers,
    };
  } catch { return fallback; }
}
