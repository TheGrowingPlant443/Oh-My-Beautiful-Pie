import { writable, type Readable } from "svelte/store";
import type { KnowledgeApi, VaultDocumentPage, VaultEntry, VaultLinkRecord, VaultNoteRecord, VaultSearchHit } from "./knowledge";
import type { IndexClient, IndexJob, IndexResult } from "./knowledge-index";
import type { Camera } from "./system-overview";
import {
  createMarkdown, EMBED_BYTE_BUDGET, extractFragment, frontmatterPending, MAX_EMBED_DEPTH, newEnv, renderProperties, renderSegment, splitFrontmatter, splitStable,
  type EmbedResult, type RenderServices,
} from "./vault-markdown";

function errorText(error: unknown): string {
  if (error instanceof Error) return error.message;
  return typeof error === "string" ? error : JSON.stringify(error);
}
export const PAGE_ROWS = 200;

// =====================================================================================================================
// One document, read in 64 KiB pages and rendered section by section.
// =====================================================================================================================

export type DocumentStatus = "loading" | "ready" | "error" | "changed";
export interface DocumentSection { id: number; html: string }
export interface DocumentState {
  vaultId: string;
  path: string;
  status: DocumentStatus;
  error: string | null;
  revision: number | null;
  /** Hash and size of the whole document as the first page reported them; every later page must agree. */
  hash: string | null;
  byteLength: number;
  loadedBytes: number;
  /** Null once every byte has been read. */
  nextOffset: number | null;
  sections: DocumentSection[];
  /** Text of the last block is held back until the next page shows where it ends (an open code fence, a list, a table). */
  waiting: boolean;
  busy: boolean;
}
export type DocumentApi = Pick<KnowledgeApi, "readDocument" | "resolveLinks">;
export interface DocumentSession extends Readable<DocumentState> {
  /** Reads the first page. Idempotent. */
  start(): Promise<void>;
  /** Reads pages until something new is rendered (or the document ends). */
  loadMore(): Promise<void>;
  /** Reads every remaining page. Cancels with `dispose`. */
  loadAll(): Promise<void>;
  /** Renders nothing more and ignores answers still on their way. */
  dispose(): void;
}

const markdown = createMarkdown();

export const documentComplete = (state: DocumentState): boolean => state.status === "ready" && state.nextOffset === null && !state.waiting;
export const documentProgress = (state: DocumentState): string =>
  state.byteLength === 0 ? "" : `${state.loadedBytes.toLocaleString()} of ${state.byteLength.toLocaleString()} bytes read`;

export function createDocumentSession(options: { api: DocumentApi; vaultId: string; path: string; sanitize: (html: string) => string }): DocumentSession {
  const { api, vaultId, path, sanitize } = options;
  let state: DocumentState = {
    vaultId, path, status: "loading", error: null, revision: null, hash: null, byteLength: 0, loadedBytes: 0, nextOffset: 0, sections: [], waiting: false, busy: false,
  };
  const store = writable(state);
  const commit = (patch: Partial<DocumentState>): void => { state = { ...state, ...patch }; store.set(state); };
  let disposed = false;
  let pending = "";
  let carry = "";
  let frontDone = false;
  let nextSection = 1;
  const env = newEnv(path);
  // Keep the source of rendered segments: a reference definition can legally arrive later than a use.
  // Only a changed reference table rerenders old segments; ordinary pages append without reparsing the whole note.
  const renderedSources: { id: number; source: string }[] = [];
  let inflight: Promise<void> | null = null;

  const services = (stack: readonly string[], from: string): RenderServices => ({
    resolve: async (_from, targets) => (await api.resolveLinks(vaultId, from, targets)).items,
    loadEmbed: (target, fragment) => embed(target, fragment, stack),
    cancelled: () => disposed,
  });

  type EmbedRead = { text: string; offset: number; complete: boolean } | { stop: EmbedResult };

  /** Reads whole pages of an embedded note up to the byte budget; a note that changes underneath stops the embed. */
  async function readEmbedText(target: string): Promise<EmbedRead> {
    let text = "";
    let offset = 0;
    let hash: string | null = null;
    for (;;) {
      const page = await api.readDocument(vaultId, target, offset);
      if (disposed) return { stop: { html: "", notice: null } };
      if (hash !== null && page.hash !== hash) return { stop: { html: "", notice: `${target} changed while it was being embedded; reload to see one consistent version.` } };
      hash = page.hash;
      text += page.text;
      if (page.nextOffset === null) return { text, offset, complete: true };
      offset = page.nextOffset;
      if (offset >= EMBED_BYTE_BUDGET) return { text, offset, complete: false };
    }
  }

  /** Reads an embedded note (up to a byte budget) and renders the requested section. It never silently drops text: a partial read says so. */
  async function embed(target: string, fragment: string | null, stack: readonly string[]): Promise<EmbedResult> {
    if (stack.includes(target)) return { html: "", notice: `Not expanded: ${[...stack, target].join(" → ")} would embed itself.` };
    if (stack.length > MAX_EMBED_DEPTH) return { html: "", notice: `Not expanded: embedded notes are followed ${MAX_EMBED_DEPTH} levels deep.` };
    const read = await readEmbedText(target);
    if ("stop" in read) return read.stop;
    const { complete, offset } = read;
    let body = read.text.replaceAll(/\r\n?/g, "\n");
    const front = splitFrontmatter(body);
    if (front) body = front.body;
    const notice = complete ? null : `Only the first ${Math.round(offset / 1024).toLocaleString()} KiB of ${target} is shown here; open the note to read the rest.`;
    if (fragment) {
      const part = extractFragment(body, fragment);
      if (part === null) {
        const where = complete ? target : `the part of ${target} read for this embed; open the note to look further`;
        return { html: "", notice: `“${fragment}” was not found in ${where}.` };
      }
      body = part;
    }
    const html = await renderSegment(markdown, body, newEnv(target), services([...stack, target], target));
    return { html: html ?? "", notice };
  }

  async function render(source: string): Promise<boolean> {
    if (!source.trim()) return true;
    const before = JSON.stringify(env.references ?? {});
    const html = await renderSegment(markdown, source, env, services([path], path));
    if (html === null || disposed) return false;
    const part = { id: nextSection++, source };
    renderedSources.push(part);
    if (before === JSON.stringify(env.references ?? {})) {
      commit({ sections: [...state.sections, { id: part.id, html: sanitize(html) }] });
      return true;
    }
    // Newly discovered definitions resolve forward references in already displayed text too.
    // Reuse resolution/embed caches, reset only heading numbering, and replace the same section ids atomically.
    env.slugs.clear();
    const replacements = new Map<number, string>();
    for (const previous of renderedSources) {
      const updated = await renderSegment(markdown, previous.source, env, services([path], path));
      if (updated === null || disposed) return false;
      replacements.set(previous.id, sanitize(updated));
    }
    const sections = state.sections.map(section => ({ id: section.id, html: replacements.get(section.id) ?? section.html }));
    sections.push({ id: part.id, html: replacements.get(part.id)! });
    commit({ sections });
    return true;
  }

  async function consume(page: VaultDocumentPage): Promise<boolean> {
    const final = page.nextOffset === null;
    let text = carry + page.text;
    carry = "";
    // A CR at a page edge may be half of a CRLF.
    if (!final && text.endsWith("\r")) { carry = "\r"; text = text.slice(0, -1); }
    pending += text.replace(/\r\n?/g, "\n");
    if (!frontDone) {
      if (!final && frontmatterPending(pending)) { commit({ waiting: true }); return true; }
      const front = splitFrontmatter(pending);
      if (front) {
        pending = front.body;
        commit({ sections: [...state.sections, { id: nextSection++, html: sanitize(renderProperties(front.properties)) }] });
      }
      frontDone = true;
    }
    const { stable, tail } = splitStable(markdown, pending, final);
    pending = tail;
    const shown = await render(stable);
    if (!shown) return false;
    commit({ waiting: tail !== "" });
    return true;
  }

  async function readNext(): Promise<boolean> {
    if (state.nextOffset === null || disposed) return false;
    const requested = state.nextOffset;
    const page = await api.readDocument(vaultId, path, requested);
    if (disposed) return false;
    if (state.hash !== null && (page.hash !== state.hash || page.byteLength !== state.byteLength)) {
      commit({ status: "changed", busy: false, error: "This note changed while it was being read. Reload it to see one consistent version." });
      return false;
    }
    if (page.byteOffset !== requested) {
      commit({ status: "error", busy: false, error: `The page that arrived starts at byte ${page.byteOffset}, not ${requested}; reading stopped so no text is skipped.` });
      return false;
    }
    const loaded = page.byteOffset + page.length;
    commit({ revision: page.revision, hash: page.hash, byteLength: page.byteLength, loadedBytes: loaded, nextOffset: page.nextOffset });
    if (page.nextOffset === null && loaded !== page.byteLength) {
      commit({ status: "error", busy: false, error: `The document ended after ${loaded.toLocaleString()} of ${page.byteLength.toLocaleString()} bytes.` });
      return false;
    }
    return consume(page);
  }

  function guarded(work: () => Promise<void>): Promise<void> {
    if (inflight) return inflight;
    commit({ busy: true });
    inflight = work().catch(error => {
      if (!disposed) commit({ status: "error", error: errorText(error) });
    }).finally(() => {
      inflight = null;
      if (!disposed) commit({ busy: false });
    });
    return inflight;
  }

  const readUntilVisible = async (all: boolean): Promise<void> => {
    const startSections = state.sections.length;
    while (state.nextOffset !== null && !disposed && (state.status === "loading" || state.status === "ready")) {
      if (!(await readNext())) return;
      if (state.status === "loading") commit({ status: "ready" });
      if (!all && (state.sections.length > startSections || state.nextOffset === null)) return;
    }
  };

  return {
    subscribe: store.subscribe,
    start() { return state.hash !== null ? (inflight ?? Promise.resolve()) : guarded(() => readUntilVisible(false)); },
    loadMore() { return guarded(() => readUntilVisible(false)); },
    loadAll() { return guarded(() => readUntilVisible(true)); },
    dispose() { disposed = true; },
  };
}

// =====================================================================================================================
// The vault itself: tree listings, search, index refresh, note graph and per-note link lists.
// =====================================================================================================================

export interface Listing { items: VaultEntry[]; total: number; nextOffset: number | null; loading: boolean; error: string | null }
export type LoadStatus = "idle" | "loading" | "ready" | "error";
export interface SearchState { text: string; status: LoadStatus; items: VaultSearchHit[]; total: number; nextOffset: number | null; error: string | null }
export interface LinkList { path: string | null; status: LoadStatus; items: VaultLinkRecord[]; total: number; nextOffset: number | null; error: string | null }
export type VaultIndexResult = Extract<IndexResult, { kind: "vault" }>;
export interface VaultGraphState {
  status: LoadStatus;
  notes: { loaded: number; total: number };
  links: { loaded: number; total: number };
  result: VaultIndexResult | null;
  error: string | null;
  /** Revision the result was built from. */
  revision: number | null;
}
export interface VaultState {
  vaultId: string;
  /** Index revision every shown row belongs to. Null until the first refresh answers. */
  revision: number | null;
  refreshing: boolean;
  refreshError: string | null;
  /** Counts from the last refresh. */
  summary: { notes: number; links: number; unreadable: number } | null;
  listings: Record<string, Listing>;
  expanded: string[];
  selected: string | null;
  search: SearchState;
  graph: VaultGraphState;
  neighbors: LinkList;
  backlinks: LinkList;
  camera: Camera | null;
  /** Widths of the explorer and the graph column, in pixels (the document takes the rest). */
  split: { tree: number; graph: number };
}
export type VaultApi = Pick<KnowledgeApi, "refreshIndex" | "listEntries" | "queryLinks">;
export interface VaultStore extends Readable<VaultState> {
  /** Entry, focus and the Refresh button: re-reads the index and, when it changed, everything built from it. */
  refresh(): Promise<void>;
  toggle(folder: string): void;
  loadFolder(prefix: string, more?: boolean): Promise<void>;
  select(path: string | null): void;
  /** Reveals `path` in the tree by opening its folders. */
  reveal(path: string): void;
  setSearch(text: string): Promise<void>;
  searchMore(): Promise<void>;
  loadLinks(kind: "neighbors" | "backlinks", path: string | null, more?: boolean): Promise<void>;
  setCamera(camera: Camera | null): void;
  setSplit(split: Partial<VaultState["split"]>): void;
  /** Stops background work (page left); state is kept for the next visit. */
  suspend(): void;
}

const blankListing = (): Listing => ({ items: [], total: 0, nextOffset: null, loading: false, error: null });
const blankLinks = (): LinkList => ({ path: null, status: "idle", items: [], total: 0, nextOffset: null, error: null });
const blankSearch = (): SearchState => ({ text: "", status: "idle", items: [], total: 0, nextOffset: null, error: null });
const blankGraph = (): VaultGraphState => ({ status: "idle", notes: { loaded: 0, total: 0 }, links: { loaded: 0, total: 0 }, result: null, error: null, revision: null });

export const folderChain = (path: string): string[] => {
  const parts = path.split("/").slice(0, -1);
  return parts.map((_, i) => parts.slice(0, i + 1).join("/"));
};

export function createVaultStore(options: { api: VaultApi; indexClient: Pick<IndexClient, "begin" | "cancel" | "dispose">; vaultId: string }): VaultStore {
  const { api, indexClient, vaultId } = options;
  let state: VaultState = {
    vaultId, revision: null, refreshing: false, refreshError: null, summary: null, listings: {}, expanded: [], selected: null, search: blankSearch(), graph: blankGraph(),
    neighbors: blankLinks(), backlinks: blankLinks(), camera: null, split: { tree: 260, graph: 380 },
  };
  const store = writable(state);
  const commit = (patch: Partial<VaultState>): void => { state = { ...state, ...patch }; store.set(state); };
  /** Bumped when everything built from the old revision must be dropped; late answers carry the value they started with. */
  let epoch = 0;
  let searchSeq = 0;
  let graphSeq = 0;
  const linkSeq = { neighbors: 0, backlinks: 0 };
  const listingSeq = new Map<string, number>();
  let refreshing: Promise<void> | null = null;
  /** Something saw a newer revision while a refresh was already running: read again once it ends. */
  let rerun = false;

  /** `ok`: belongs to the current revision. `newer`: the vault changed under us, re-read. `older`: a late answer, dropped. */
  const judge = (revision: number): "ok" | "newer" | "older" => {
    if (state.revision === null || revision === state.revision) return "ok";
    return revision > state.revision ? "newer" : "older";
  };

  function setListing(prefix: string, patch: Partial<Listing>): void {
    commit({ listings: { ...state.listings, [prefix]: { ...(state.listings[prefix] ?? blankListing()), ...patch } } });
  }

  async function loadFolder(prefix: string, more = false): Promise<void> {
    const mine = epoch;
    const sequence = (listingSeq.get(prefix) ?? 0) + 1;
    listingSeq.set(prefix, sequence);
    const known = state.listings[prefix];
    const offset = more && known ? known.nextOffset ?? 0 : 0;
    if (more && known?.nextOffset === null) return;
    setListing(prefix, { loading: true, error: null, ...(more ? {} : { items: known?.items ?? [] }) });
    try {
      const page = await api.listEntries(vaultId, prefix, offset, PAGE_ROWS);
      if (mine !== epoch || sequence !== listingSeq.get(prefix)) return;
      const verdict = judge(page.revision);
      if (verdict === "older") return;
      if (verdict === "newer") { void refresh(); return; }
      setListing(prefix, { items: more && known ? [...known.items, ...page.items] : page.items, total: page.total, nextOffset: page.nextOffset, loading: false });
    } catch (error) {
      if (mine === epoch && sequence === listingSeq.get(prefix)) setListing(prefix, { loading: false, error: errorText(error) });
    }
  }

  /** Pages one section into the worker. False when the load was superseded or the vault moved on; the job is then cancelled. */
  async function pushSection(job: IndexJob, section: "notes" | "links", mine: number, revision: number | null): Promise<boolean> {
    let offset = 0;
    for (;;) {
      const page = await api.queryLinks<VaultNoteRecord | VaultLinkRecord>(vaultId, { kind: section, offset, limit: PAGE_ROWS });
      if (mine !== graphSeq) { job.cancel(); return false; }
      if (page.revision !== revision) {
        job.cancel();
        if (judge(page.revision) === "newer") void refresh();
        return false;
      }
      if (section === "notes") job.push({ section, items: page.items as VaultNoteRecord[] });
      else job.push({ section, items: page.items as VaultLinkRecord[] });
      const progress = { loaded: offset + page.items.length, total: page.total };
      commit({ graph: { ...state.graph, [section]: progress } });
      if (page.nextOffset === null) return true;
      offset = page.nextOffset;
    }
  }

  async function loadGraph(): Promise<void> {
    const mine = ++graphSeq;
    const revision = state.revision;
    const job = indexClient.begin("vault");
    commit({ graph: { ...blankGraph(), status: "loading", revision } });
    try {
      for (const section of ["notes", "links"] as const) {
        if (!(await pushSection(job, section, mine, revision))) return;
      }
      const result = await job.finish();
      if (mine !== graphSeq || result?.kind !== "vault") return;
      commit({ graph: { ...state.graph, status: "ready", result, error: null } });
    } catch (error) {
      job.cancel();
      if (mine === graphSeq) commit({ graph: { ...state.graph, status: "error", error: errorText(error) } });
    }
  }

  function dropRevision(revision: number): void {
    epoch++;
    graphSeq++;
    searchSeq++;
    linkSeq.neighbors++;
    linkSeq.backlinks++;
    listingSeq.clear();
    indexClient.cancel();
    commit({ revision, listings: {}, graph: blankGraph(), neighbors: blankLinks(), backlinks: blankLinks(), search: { ...blankSearch(), text: state.search.text } });
  }

  function refresh(): Promise<void> {
    if (refreshing) { rerun = true; return refreshing; }
    commit({ refreshing: true, refreshError: null });
    refreshing = (async () => {
      try {
        const done = await api.refreshIndex(vaultId);
        const changed = done.revision !== state.revision;
        if (changed) dropRevision(done.revision);
        commit({ summary: { notes: done.notes, links: done.links, unreadable: done.unreadable } });
        // Folders can change without a revision (new empty folders, non-Markdown files): the visible listings are always re-read.
        await Promise.all(["", ...state.expanded].map(prefix => loadFolder(prefix)));
        if (changed || state.graph.status === "idle" || state.graph.status === "error") void loadGraph();
        if (changed && state.search.text) void setSearch(state.search.text);
        if (state.selected) { void loadLinks("neighbors", state.selected); void loadLinks("backlinks", state.selected); }
      } catch (error) {
        commit({ refreshError: errorText(error) });
      } finally {
        refreshing = null;
        commit({ refreshing: false });
        if (rerun) { rerun = false; void refresh(); }
      }
    })();
    return refreshing;
  }

  async function runSearch(text: string, more: boolean): Promise<void> {
    const sequence = more ? searchSeq : ++searchSeq;
    const mine = epoch;
    const trimmed = text.trim();
    if (!trimmed) { searchSeq++; commit({ search: blankSearch() }); return; }
    const previous = state.search;
    const continuing = more && previous.text === trimmed;
    const offset = continuing ? previous.nextOffset ?? 0 : 0;
    if (continuing && previous.nextOffset === null) return;
    commit({ search: { ...(continuing ? previous : blankSearch()), text: trimmed, status: "loading", error: null } });
    try {
      const page = await api.queryLinks<VaultSearchHit>(vaultId, { kind: "search", text: trimmed, offset, limit: PAGE_ROWS });
      if (mine !== epoch || sequence !== searchSeq) return;
      const verdict = judge(page.revision);
      if (verdict === "older") return;
      if (verdict === "newer") { void refresh(); return; }
      commit({ search: { text: trimmed, status: "ready", items: continuing ? [...previous.items, ...page.items] : page.items, total: page.total, nextOffset: page.nextOffset, error: null } });
    } catch (error) {
      if (mine === epoch && sequence === searchSeq) commit({ search: { ...state.search, status: "error", error: errorText(error) } });
    }
  }
  const setSearch = (text: string): Promise<void> => runSearch(text, false);

  async function loadLinks(kind: "neighbors" | "backlinks", path: string | null, more = false): Promise<void> {
    const sequence = more ? linkSeq[kind] : ++linkSeq[kind];
    const mine = epoch;
    if (path === null) { commit({ [kind]: blankLinks() } as Partial<VaultState>); return; }
    const previous = state[kind];
    const continuing = more && previous.path === path;
    const offset = continuing ? previous.nextOffset ?? 0 : 0;
    if (continuing && previous.nextOffset === null) return;
    commit({ [kind]: { ...(continuing ? previous : blankLinks()), path, status: "loading", error: null } } as Partial<VaultState>);
    try {
      const page = await api.queryLinks<VaultLinkRecord>(vaultId, { kind, path, offset, limit: PAGE_ROWS });
      if (mine !== epoch || sequence !== linkSeq[kind]) return;
      const verdict = judge(page.revision);
      if (verdict === "older") return;
      if (verdict === "newer") { void refresh(); return; }
      commit({ [kind]: { path, status: "ready", items: continuing ? [...previous.items, ...page.items] : page.items, total: page.total, nextOffset: page.nextOffset, error: null } } as Partial<VaultState>);
    } catch (error) {
      if (mine === epoch && sequence === linkSeq[kind]) commit({ [kind]: { ...state[kind], status: "error", error: errorText(error) } } as Partial<VaultState>);
    }
  }

  return {
    subscribe: store.subscribe,
    refresh,
    toggle(folder) {
      if (state.expanded.includes(folder)) { commit({ expanded: state.expanded.filter(item => item !== folder) }); return; }
      commit({ expanded: [...state.expanded, folder] });
      if (!state.listings[folder] || state.listings[folder].error) void loadFolder(folder);
    },
    loadFolder,
    select(path) {
      if (path === state.selected) return;
      commit({ selected: path });
      if (path !== null) { void loadLinks("neighbors", path); void loadLinks("backlinks", path); }
      else { void loadLinks("neighbors", null); void loadLinks("backlinks", null); }
    },
    reveal(path) {
      const chain = folderChain(path);
      const opened = chain.filter(folder => !state.expanded.includes(folder));
      if (!opened.length) return;
      commit({ expanded: [...state.expanded, ...opened] });
      for (const folder of opened) if (!state.listings[folder]) void loadFolder(folder);
    },
    setSearch,
    searchMore: () => runSearch(state.search.text, true),
    loadLinks,
    setCamera(camera) { commit({ camera }); },
    setSplit(split) { commit({ split: { ...state.split, ...split } }); },
    suspend() {
      graphSeq++;
      searchSeq++;
      indexClient.dispose();
      if (state.graph.status === "loading") commit({ graph: blankGraph() });
    },
  };
}

// ---------- explorer rows ----------

export type TreeRow =
  | { kind: "folder"; path: string; name: string; depth: number; expanded: boolean }
  | { kind: "entry"; entry: VaultEntry; depth: number }
  | { kind: "status"; prefix: string; depth: number; text: string; error: boolean; more: boolean };

/** Depth-first rows of the explorer: only folders the user opened, every listing page that was loaded, and explicit loading/error/continue rows. */
export function treeRows(state: Pick<VaultState, "listings" | "expanded">): TreeRow[] {
  const rows: TreeRow[] = [];
  const open = new Set(state.expanded);
  const walk = (prefix: string, depth: number): void => {
    const listing = state.listings[prefix];
    if (!listing) { rows.push({ kind: "status", prefix, depth, text: "Loading…", error: false, more: false }); return; }
    for (const entry of listing.items) {
      if (entry.kind === "folder") {
        const expanded = open.has(entry.path);
        rows.push({ kind: "folder", path: entry.path, name: entry.name, depth, expanded });
        if (expanded) walk(entry.path, depth + 1);
      } else rows.push({ kind: "entry", entry, depth });
    }
    if (listing.error) rows.push({ kind: "status", prefix, depth, text: listing.error, error: true, more: false });
    else if (listing.loading) rows.push({ kind: "status", prefix, depth, text: "Loading…", error: false, more: false });
    else if (listing.nextOffset !== null) rows.push({ kind: "status", prefix, depth, text: `Show more (${listing.items.length.toLocaleString()} of ${listing.total.toLocaleString()})`, error: false, more: true });
    else if (listing.items.length === 0) rows.push({ kind: "status", prefix, depth, text: "Empty", error: false, more: false });
  };
  walk("", 0);
  return rows;
}
