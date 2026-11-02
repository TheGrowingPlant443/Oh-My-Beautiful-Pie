import { writable, type Readable } from "svelte/store";
import type {
  GraphifyArtifact, GraphifyCommunity, GraphifyHyperedge, GraphifyLink, GraphifyNode, GraphifyRef, GraphifySearchItem, KnowledgeApi,
} from "./knowledge";
import type { GraphifyStats } from "./knowledge-graph";
import type { IndexClient, IndexJob, IndexResult } from "./knowledge-index";
import type { Camera } from "./system-overview";
import type { GraphModel } from "./system-graph";

// Store behind GraphifyView. It admits a Graphify output only when the human asks (open/refresh), reads one immutable artifact page by
// page, and never mixes pages, search results or inspector records of different builds: everything carries the artifact id it came from.
// It runs no Graphify, no extraction and no model, and never touches the repository's `graph.html`.

const PAGE = 200;
function errorText(error: unknown): string {
  if (error instanceof Error) return error.message;
  return typeof error === "string" ? error : JSON.stringify(error);
}
/** The backend's wording when a live output was replaced or removed after it was admitted. */
export const isGone = (message: string): boolean => /no longer available/i.test(message);
export const REPLACED_TEXT = "New Graphify output available — Refresh";

export type GraphifyApi = Pick<KnowledgeApi, "openGraphifyArtifact" | "checkGraphifyArtifact" | "readGraphify" | "queryGraphify">;
export type Progress = { loaded: number; total: number };
export interface GraphifyResult { model: GraphModel; stats: GraphifyStats }
export interface GraphifySearch { text: string; status: "idle" | "loading" | "ready" | "error"; items: GraphifySearchItem[]; total: number; nextOffset: number | null; error: string | null }
export interface GraphifyNeighbors { key: string | null; status: "idle" | "loading" | "ready" | "error"; items: GraphifyLink[]; total: number; nextOffset: number | null; error: string | null; groups: GraphifyHyperedge[] }
export interface GraphifyState {
  folder: string | null;
  status: "idle" | "opening" | "loading" | "ready" | "error";
  error: string | null;
  /** Set when the admitted output changed or vanished: reading stops until the human refreshes. */
  stale: string | null;
  artifact: GraphifyArtifact | null;
  progress: Record<"communities" | "nodes" | "links" | "hyperedges", Progress>;
  result: GraphifyResult | null;
  nodes: GraphifyNode[];
  links: GraphifyLink[];
  hyperedges: GraphifyHyperedge[];
  communities: GraphifyCommunity[];
  search: GraphifySearch;
  neighbors: GraphifyNeighbors;
  /** A group (hyperedge) whose members are highlighted. It is a record, never an edge. */
  selectedGroup: string | null;
  camera: Camera | null;
}
export interface GraphifyStore extends Readable<GraphifyState> {
  /** Explicit action: admits the output of `folder` (a new artifact id) and reads it. Also the Refresh button. */
  open(folder: string): Promise<void>;
  /** Cheap check on focus: is the admitted artifact still readable? Sets `stale` when it is not. */
  check(): Promise<void>;
  setSearch(text: string): Promise<void>;
  searchMore(): Promise<void>;
  inspect(ref: GraphifyRef | null, more?: boolean): Promise<void>;
  selectGroup(id: string | null): void;
  setCamera(camera: Camera | null): void;
  suspend(): void;
}

const blankProgress = (): GraphifyState["progress"] => ({ communities: { loaded: 0, total: 0 }, nodes: { loaded: 0, total: 0 }, links: { loaded: 0, total: 0 }, hyperedges: { loaded: 0, total: 0 } });
const blankSearch = (): GraphifySearch => ({ text: "", status: "idle", items: [], total: 0, nextOffset: null, error: null });
const blankNeighbors = (): GraphifyNeighbors => ({ key: null, status: "idle", items: [], total: 0, nextOffset: null, error: null, groups: [] });
const initial = (): GraphifyState => ({
  folder: null, status: "idle", error: null, stale: null, artifact: null, progress: blankProgress(), result: null, nodes: [], links: [], hyperedges: [], communities: [],
  search: blankSearch(), neighbors: blankNeighbors(), selectedGroup: null, camera: null,
});

export const refKey = (ref: GraphifyRef): string => `${ref.idKind}:${ref.id}`;

type GraphifyRecord = GraphifyNode | GraphifyLink | GraphifyHyperedge | GraphifyCommunity;
interface Collected { nodes: GraphifyNode[]; links: GraphifyLink[]; hyperedges: GraphifyHyperedge[]; communities: GraphifyCommunity[] }
type SectionKey = keyof Collected;
const SECTIONS: readonly SectionKey[] = ["communities", "nodes", "links", "hyperedges"];

/** Keeps one page for the finished view and streams it to the index worker. */
function collect(into: Collected, job: IndexJob, key: SectionKey, items: GraphifyRecord[]): void {
  switch (key) {
    case "nodes": into.nodes.push(...(items as GraphifyNode[])); job.push({ section: "graphifyNodes", items: items as GraphifyNode[] }); return;
    case "links": into.links.push(...(items as GraphifyLink[])); job.push({ section: "graphifyLinks", items: items as GraphifyLink[] }); return;
    case "hyperedges": into.hyperedges.push(...(items as GraphifyHyperedge[])); job.push({ section: "graphifyHyperedges", items: items as GraphifyHyperedge[] }); return;
    case "communities": into.communities.push(...(items as GraphifyCommunity[])); job.push({ section: "graphifyCommunities", items: items as GraphifyCommunity[] }); return;
  }
}

export function createGraphifyStore(options: { api: GraphifyApi; indexClient: Pick<IndexClient, "begin" | "cancel" | "dispose"> }): GraphifyStore {
  const { api, indexClient } = options;
  let state = initial();
  const store = writable(state);
  const commit = (patch: Partial<GraphifyState>): void => { state = { ...state, ...patch }; store.set(state); };
  /** Bumped for every admission: a late page, query or build of an earlier one is dropped. */
  let seq = 0;
  let searchSeq = 0;
  let neighborSeq = 0;

  const gone = (message: string): void => {
    seq++;
    indexClient.cancel();
    commit({ status: state.result ? "ready" : "error", stale: REPLACED_TEXT, error: state.result ? null : message });
  };

  function failWith(error: unknown, otherwise: (message: string) => Partial<GraphifyState>): void {
    const message = errorText(error);
    if (isGone(message)) gone(message);
    else commit(otherwise(message));
  }

  async function readSection(artifact: GraphifyArtifact, job: IndexJob, into: Collected, key: SectionKey, mine: number): Promise<boolean> {
    let offset = 0;
    for (;;) {
      const page = await api.readGraphify<GraphifyRecord>(artifact.artifactId, key, offset, PAGE);
      if (mine !== seq) { job.cancel(); return false; }
      // Pages of another build are never mixed in: the hash, not only the id, must match.
      if (page.artifactId !== artifact.artifactId || page.artifactHash !== artifact.artifactHash) throw new Error("A page of a different Graphify build arrived; reading stopped so builds are not mixed.");
      collect(into, job, key, page.items);
      commit({ progress: { ...state.progress, [key]: { loaded: offset + page.items.length, total: page.total } } });
      if (page.nextOffset === null) return true;
      offset = page.nextOffset;
    }
  }

  async function open(folder: string): Promise<void> {
    const mine = ++seq;
    searchSeq++;
    neighborSeq++;
    indexClient.cancel();
    state = { ...initial(), folder, status: "opening", camera: null };
    store.set(state);
    let job: IndexJob | null = null;
    try {
      const artifact = await api.openGraphifyArtifact(folder);
      if (mine !== seq) return;
      commit({
        artifact, status: "loading",
        progress: { communities: { loaded: 0, total: artifact.counts.communities }, nodes: { loaded: 0, total: artifact.counts.nodes }, links: { loaded: 0, total: artifact.counts.links }, hyperedges: { loaded: 0, total: artifact.counts.hyperedges } },
      });
      job = indexClient.begin("graphify");
      const into: Collected = { nodes: [], links: [], hyperedges: [], communities: [] };
      for (const key of SECTIONS) {
        if (!await readSection(artifact, job, into, key, mine)) return;
      }
      const built: IndexResult | null = await job.finish();
      if (mine !== seq || built?.kind !== "graphify") return;
      commit({ status: "ready", ...into, result: { model: built.model, stats: built.stats } });
    } catch (error) {
      job?.cancel();
      if (mine !== seq) return;
      failWith(error, message => ({ status: "error", error: message }));
    }
  }

  async function check(): Promise<void> {
    const mine = seq;
    const artifact = state.artifact;
    if (!artifact || state.status !== "ready") return;
    try {
      const freshness = await api.checkGraphifyArtifact(artifact.artifactId);
      if (mine !== seq || freshness.artifactId !== artifact.artifactId) return;
      commit({ stale: freshness.current ? null : REPLACED_TEXT });
    } catch (error) {
      if (mine === seq) commit({ error: `Could not check Graphify freshness: ${errorText(error)}` });
    }
  }

  function searchStale(mine: number, sequence: number): boolean { return mine !== seq || sequence !== searchSeq; }
  function neighborsStale(mine: number, sequence: number): boolean { return mine !== seq || sequence !== neighborSeq; }

  async function runSearch(artifact: GraphifyArtifact, trimmed: string, more: boolean): Promise<void> {
    const sequence = more ? searchSeq : ++searchSeq;
    const previous = state.search;
    const continuing = more && previous.text === trimmed;
    if (continuing && previous.nextOffset === null) return;
    const base = continuing ? previous : blankSearch();
    const mine = seq;
    commit({ search: { ...base, text: trimmed, status: "loading", error: null } });
    try {
      const page = await api.queryGraphify(artifact.artifactId, { kind: "search", text: trimmed, offset: base.nextOffset ?? 0, limit: PAGE });
      if (searchStale(mine, sequence) || page.artifactId !== artifact.artifactId) return;
      commit({ search: { text: trimmed, status: "ready", items: [...base.items, ...(page.items as GraphifySearchItem[])], total: page.total, nextOffset: page.nextOffset, error: null } });
    } catch (error) {
      if (searchStale(mine, sequence)) return;
      failWith(error, message => ({ search: { ...state.search, status: "error", error: message } }));
    }
  }

  /** A continued list keeps the groups it already has; the first page asks for them. */
  async function neighborGroups(artifact: GraphifyArtifact, ref: GraphifyRef, continuing: boolean, known: GraphifyHyperedge[]): Promise<GraphifyHyperedge[]> {
    if (continuing) return known;
    return (await api.queryGraphify(artifact.artifactId, { kind: "hyperedgesOf", id: ref, offset: 0, limit: PAGE })).items as GraphifyHyperedge[];
  }

  async function loadNeighbors(artifact: GraphifyArtifact, ref: GraphifyRef, more: boolean): Promise<void> {
    const sequence = more ? neighborSeq : ++neighborSeq;
    const key = refKey(ref);
    const previous = state.neighbors;
    const continuing = more && previous.key === key;
    if (continuing && previous.nextOffset === null) return;
    const base = continuing ? previous : blankNeighbors();
    const mine = seq;
    commit({ neighbors: { ...base, key, status: "loading", error: null } });
    try {
      const page = await api.queryGraphify(artifact.artifactId, { kind: "neighbors", id: ref, direction: "both", offset: base.nextOffset ?? 0, limit: PAGE });
      if (neighborsStale(mine, sequence) || page.artifactId !== artifact.artifactId) return;
      const groups = await neighborGroups(artifact, ref, continuing, previous.groups);
      if (neighborsStale(mine, sequence)) return;
      commit({ neighbors: { key, status: "ready", items: [...base.items, ...(page.items as GraphifyLink[])], total: page.total, nextOffset: page.nextOffset, error: null, groups } });
    } catch (error) {
      if (neighborsStale(mine, sequence)) return;
      failWith(error, message => ({ neighbors: { ...state.neighbors, status: "error", error: message } }));
    }
  }

  async function inspect(ref: GraphifyRef | null, more = false): Promise<void> {
    const artifact = state.artifact;
    if (ref !== null && artifact) {
      await loadNeighbors(artifact, ref, more);
      return;
    }
    if (!more) {
      neighborSeq++;
      commit({ neighbors: blankNeighbors() });
    }
  }

  return {
    subscribe: store.subscribe,
    open,
    check,
    setSearch: async text => {
      if (state.artifact && text.trim()) await runSearch(state.artifact, text.trim(), false);
      else { searchSeq++; commit({ search: blankSearch() }); }
    },
    searchMore: async () => { if (state.artifact && state.search.text) await runSearch(state.artifact, state.search.text, true); },
    inspect,
    selectGroup(id) { commit({ selectedGroup: id }); },
    setCamera(camera) { commit({ camera }); },
    suspend() {
      // Leaves the loaded graph; stops a read that is still going so a half-read artifact is never shown as complete.
      if (state.status === "loading" || state.status === "opening") { seq++; indexClient.cancel(); commit({ status: "idle", progress: blankProgress() }); }
    },
  };
}
