import { writable, type Readable } from "svelte/store";
import type {
  GraphifyArtifact, GraphifyCommunity, GraphifyHyperedge, GraphifyLink, GraphifyNode, GraphifyRef, GraphifySearchItem, GraphifySection, KnowledgeApi,
} from "./knowledge";
import type { GraphifyStats } from "./knowledge-graph";
import type { IndexClient, IndexJob, IndexResult } from "./knowledge-index";
import type { Camera } from "./system-overview";
import type { GraphModel } from "./system-graph";

// Store behind GraphifyView. It admits a Graphify output only when the human asks (open/refresh), reads one immutable artifact page by
// page, and never mixes pages, search results or inspector records of different builds: everything carries the artifact id it came from.
// It runs no Graphify, no extraction and no model, and never touches the repository's `graph.html`.

const PAGE = 200;
const errorText = (error: unknown): string => error instanceof Error ? error.message : typeof error === "string" ? error : JSON.stringify(error);
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
      const nodes: GraphifyNode[] = [], links: GraphifyLink[] = [], hyperedges: GraphifyHyperedge[] = [], communities: GraphifyCommunity[] = [];
      const plan: { section: GraphifySection; key: "communities" | "nodes" | "links" | "hyperedges" }[] = [
        { section: "communities", key: "communities" }, { section: "nodes", key: "nodes" }, { section: "links", key: "links" }, { section: "hyperedges", key: "hyperedges" },
      ];
      for (const { section, key } of plan) {
        let offset = 0;
        for (;;) {
          const page = await api.readGraphify<GraphifyNode | GraphifyLink | GraphifyHyperedge | GraphifyCommunity>(artifact.artifactId, section, offset, PAGE);
          if (mine !== seq) { job.cancel(); return; }
          // Pages of another build are never mixed in: the hash, not only the id, must match.
          if (page.artifactId !== artifact.artifactId || page.artifactHash !== artifact.artifactHash) throw new Error("A page of a different Graphify build arrived; reading stopped so builds are not mixed.");
          if (key === "nodes") { nodes.push(...(page.items as GraphifyNode[])); job.push({ section: "graphifyNodes", items: page.items as GraphifyNode[] }); }
          else if (key === "links") { links.push(...(page.items as GraphifyLink[])); job.push({ section: "graphifyLinks", items: page.items as GraphifyLink[] }); }
          else if (key === "hyperedges") { hyperedges.push(...(page.items as GraphifyHyperedge[])); job.push({ section: "graphifyHyperedges", items: page.items as GraphifyHyperedge[] }); }
          else { communities.push(...(page.items as GraphifyCommunity[])); job.push({ section: "graphifyCommunities", items: page.items as GraphifyCommunity[] }); }
          commit({ progress: { ...state.progress, [key]: { loaded: offset + page.items.length, total: page.total } } });
          if (page.nextOffset === null) break;
          offset = page.nextOffset;
        }
      }
      const built: IndexResult | null = await job.finish();
      if (mine !== seq || built === null || built.kind !== "graphify") return;
      commit({ status: "ready", nodes, links, hyperedges, communities, result: { model: built.model, stats: built.stats } });
    } catch (error) {
      job?.cancel();
      if (mine !== seq) return;
      const message = errorText(error);
      if (isGone(message)) gone(message);
      else commit({ status: "error", error: message });
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

  async function runSearch(text: string, more: boolean): Promise<void> {
    const artifact = state.artifact;
    const sequence = more ? searchSeq : ++searchSeq;
    const trimmed = text.trim();
    if (!artifact || !trimmed) { if (!more) { searchSeq++; commit({ search: blankSearch() }); } return; }
    const previous = state.search;
    const continuing = more && previous.text === trimmed;
    if (continuing && previous.nextOffset === null) return;
    const offset = continuing ? previous.nextOffset ?? 0 : 0;
    const mine = seq;
    commit({ search: { ...(continuing ? previous : blankSearch()), text: trimmed, status: "loading", error: null } });
    try {
      const page = await api.queryGraphify(artifact.artifactId, { kind: "search", text: trimmed, offset, limit: PAGE });
      if (mine !== seq || sequence !== searchSeq || page.artifactId !== artifact.artifactId) return;
      commit({ search: { text: trimmed, status: "ready", items: continuing ? [...previous.items, ...(page.items as GraphifySearchItem[])] : (page.items as GraphifySearchItem[]), total: page.total, nextOffset: page.nextOffset, error: null } });
    } catch (error) {
      if (mine !== seq || sequence !== searchSeq) return;
      const message = errorText(error);
      if (isGone(message)) gone(message); else commit({ search: { ...state.search, status: "error", error: message } });
    }
  }

  async function inspect(ref: GraphifyRef | null, more = false): Promise<void> {
    const artifact = state.artifact;
    const sequence = more ? neighborSeq : ++neighborSeq;
    if (ref === null || !artifact) { if (!more) commit({ neighbors: blankNeighbors() }); return; }
    const key = refKey(ref);
    const previous = state.neighbors;
    const continuing = more && previous.key === key;
    if (continuing && previous.nextOffset === null) return;
    const offset = continuing ? previous.nextOffset ?? 0 : 0;
    const mine = seq;
    commit({ neighbors: { ...(continuing ? previous : blankNeighbors()), key, status: "loading", error: null } });
    try {
      const page = await api.queryGraphify(artifact.artifactId, { kind: "neighbors", id: ref, direction: "both", offset, limit: PAGE });
      if (mine !== seq || sequence !== neighborSeq || page.artifactId !== artifact.artifactId) return;
      const groups = continuing ? previous.groups : (await api.queryGraphify(artifact.artifactId, { kind: "hyperedgesOf", id: ref, offset: 0, limit: PAGE })).items as GraphifyHyperedge[];
      if (mine !== seq || sequence !== neighborSeq) return;
      commit({ neighbors: { key, status: "ready", items: continuing ? [...previous.items, ...(page.items as GraphifyLink[])] : (page.items as GraphifyLink[]), total: page.total, nextOffset: page.nextOffset, error: null, groups } });
    } catch (error) {
      if (mine !== seq || sequence !== neighborSeq) return;
      const message = errorText(error);
      if (isGone(message)) gone(message); else commit({ neighbors: { ...state.neighbors, status: "error", error: message } });
    }
  }

  return {
    subscribe: store.subscribe,
    open,
    check,
    setSearch: text => runSearch(text, false),
    searchMore: () => runSearch(state.search.text, true),
    inspect,
    selectGroup(id) { commit({ selectedGroup: id }); },
    setCamera(camera) { commit({ camera }); },
    suspend() {
      // Leaves the loaded graph; stops a read that is still going so a half-read artifact is never shown as complete.
      if (state.status === "loading" || state.status === "opening") { seq++; indexClient.cancel(); commit({ status: "idle", progress: blankProgress() }); }
    },
  };
}
