import { writable, type Readable } from "svelte/store";
import { repoKey } from "./repo-agents";
import { TicketBook, type TicketClock, type VisualizationTicket } from "./visualization-tickets";
import {
  baseName, modelFromDirectories, modelFromNeighbors, modelFromOverview, modelFromSystem, placeholderSystemModel, scopeModel, SIGMA_MAX_NODES,
  type GraphModel, type ModelEdge, type ModelNode,
} from "./system-graph";
import type {
  CoverageRow, NeighborRow, OverviewGroupRow, ProjectInsightStatus, SourceFileRow, SourceIndexPage, SourceIndexQuery,
  SourceIndexSelection, SourceIndexStatus, SourceSiteRow, SourceSymbolRow, SystemOverviewPayload, HelicopterPayload,
} from "./api";
import type { api as backend } from "./api";
import { EMPTY_INVENTORY, type ArchitectDescriptor, type GenerateOutcome, type GenerateRequest, type InventoryState, type StructureHost, type VisualizationSink } from "./overview-types";
import { modelFromHelicopter } from "./helicopter-graph";

export type OverviewTab = "helicopter" | "system" | "files" | "functions";
export type OverviewArea = "diagram" | "structure";
/** The two tabs that show a picture the existing Architect generated. Each keeps its own saved revisions, selection and admission. */
export type ArchitectTab = "helicopter" | "system";
export type SourceTab = "files" | "functions";
export const isSourceTab = (tab: OverviewTab): tab is SourceTab => tab === "files" || tab === "functions";
const SLOT_KIND: Record<ArchitectTab, "systemOverview" | "helicopter"> = { system: "systemOverview", helicopter: "helicopter" };

export type OverviewApi = Pick<typeof backend, "getSourceIndex" | "startSourceIndex" | "readSourceIndexJob" | "cancelSourceIndex" | "querySourceIndex" | "requestSystemVisualization" | "readProjectInsight" | "getProjectInsight">;

const PAGE = 200;
const NEIGHBOR_CAP = 1000;
const DIRECTORY_PAGE_CAP = 100;
const POLL_MS = 1000;
const PREFS_KEY = "omp-manager.system-overview";

/** Restricts Files/Functions to the files a diagram node cites (or a directory). Never a path typed by the user. */
export interface Scope { label: string; exact: string[]; prefixes: string[] }
export interface Camera { x: number; y: number; ratio: number }
export interface Neighborhood { id: string; label: string; path: string | null }

/** Folder, tab, search, camera and selection live here so page unmounts (generation, Architect round trips) never reset them. */
export interface TabView {
  search: string;
  /** Opened containers; null means the automatic default cut. */
  expanded: string[] | null;
  selectedNode: string | null;
  selectedEdge: string | null;
  camera: Camera | null;
  relations: string[] | null;
  grades: string[] | null;
  derived: boolean;
  scope: Scope | null;
  neighborhood: Neighborhood | null;
  page: number;
}

export interface LevelData {
  snapshotId: string | null;
  status: "idle" | "loading" | "ready" | "error";
  /** `nodes`: the per-node strongly connected overview; `directories`: aggregate when the graph exceeds the Sigma bound. */
  mode: "nodes" | "directories";
  total: number;
  loaded: number;
  groups: OverviewGroupRow[];
  files: SourceFileRow[];
  symbols: SourceSymbolRow[];
  directories: string[];
  /** Members of opened cycles (loaded on demand): nodes plus the cycle edges between them. */
  extra: { nodes: ModelNode[]; edges: ModelEdge[] };
  error: string | null;
}

export interface Inspector {
  level: SourceTab;
  id: string;
  snapshotId: string;
  status: "loading" | "ready" | "error";
  rows: NeighborRow[];
  total: number;
  nextOffset: number | null;
  error: string | null;
}

export interface SearchResults {
  text: string;
  status: "idle" | "loading" | "ready" | "error";
  items: (SourceFileRow | SourceSymbolRow)[];
  total: number;
  nextOffset: number | null;
  error: string | null;
}

export interface CoverageState { status: "idle" | "loading" | "ready" | "error"; snapshotId: string | null; items: CoverageRow[]; total: number; nextOffset: number | null; error: string | null }

export interface IndexView {
  phase: "idle" | "loading" | "ready" | "error";
  selected: SourceIndexStatus | null;
  attempt: SourceIndexStatus | null;
  /** The user chose to view the latest partial attempt instead of the selected complete index. */
  viewPartial: boolean;
  error: string | null;
}

export interface SystemRevisions {
  revisions: ProjectInsightStatus[];
  selectedId: string | null;
  loading: boolean;
  error: string | null;
  /** A generation request is being admitted. */
  admitting: boolean;
}

export interface BackEntry { tab: OverviewTab; area: OverviewArea; view: TabView }

export interface OverviewState {
  folder: string;
  tab: OverviewTab;
  area: OverviewArea;
  index: IndexView;
  levels: Record<SourceTab, LevelData>;
  inventory: InventoryState;
  inspector: Inspector | null;
  hood: Inspector | null;
  search: Record<SourceTab, SearchResults>;
  coverage: CoverageState;
  system: SystemRevisions;
  helicopter: SystemRevisions;
  views: Record<OverviewTab, TabView>;
  back: BackEntry[];
  tickets: VisualizationTicket[];
  navigationEpoch: number;
}

export interface OverviewHooks {
  architectId(): string | null;
  /** The live Architect session this page may show structure sources for. */
  architectSession(): ArchitectDescriptor | null;
  openArchitect(id: string, replayRequestId?: string): Promise<boolean>;
  following(ticket: VisualizationTicket): boolean;
  quiescent(ticket: VisualizationTicket): boolean;
  returnToOverview(ticket: VisualizationTicket): void;
}


const blankView = (): TabView => ({ search: "", expanded: null, selectedNode: null, selectedEdge: null, camera: null, relations: null, grades: null, derived: true, scope: null, neighborhood: null, page: 0 });
const blankRevisions = (): SystemRevisions => ({ revisions: [], selectedId: null, loading: false, error: null, admitting: false });
const blankLevel = (): LevelData => ({ snapshotId: null, status: "idle", mode: "nodes", total: 0, loaded: 0, groups: [], files: [], symbols: [], directories: [], extra: { nodes: [], edges: [] }, error: null });
const blankSearch = (): SearchResults => ({ text: "", status: "idle", items: [], total: 0, nextOffset: null, error: null });
const blankCoverage = (): CoverageState => ({ status: "idle", snapshotId: null, items: [], total: 0, nextOffset: null, error: null });
const blankIndex = (): IndexView => ({ phase: "idle", selected: null, attempt: null, viewPartial: false, error: null });

export function initialOverviewState(): OverviewState {
  return {
    folder: "", tab: "system", area: "diagram", index: blankIndex(), levels: { files: blankLevel(), functions: blankLevel() }, inventory: EMPTY_INVENTORY, inspector: null, hood: null,
    search: { files: blankSearch(), functions: blankSearch() }, coverage: blankCoverage(), system: blankRevisions(), helicopter: blankRevisions(),
    views: { helicopter: blankView(), system: blankView(), files: blankView(), functions: blankView() }, back: [], tickets: [], navigationEpoch: 0,
  };
}

const errorText = (error: unknown): string => error instanceof Error ? error.message : String(error);
const isActive = (status: SourceIndexStatus | null): boolean => status !== null && (status.state === "inventory" || status.state === "parsing" || status.state === "resolving");

/** The index snapshot the page shows: the selected complete one, or the latest partial attempt when the user chose to view it. */
export function effectiveIndex(state: Pick<OverviewState, "index">): SourceIndexStatus | null {
  const { selected, attempt, viewPartial } = state.index;
  if (viewPartial && attempt?.state === "partial" && attempt.snapshotId) return attempt;
  return selected;
}

export function selectedSystemResult(state: Pick<OverviewState, "system">): { status: ProjectInsightStatus; result: SystemOverviewPayload } | null {
  const status = state.system.revisions.find(item => item.requestId === state.system.selectedId);
  if (!status || status.state !== "completed" || status.result?.kind !== "systemOverview") return null;
  return { status, result: status.result };
}

export function selectedHelicopterResult(state: Pick<OverviewState, "helicopter">): { status: ProjectInsightStatus; result: HelicopterPayload } | null {
  const status = state.helicopter.revisions.find(item => item.requestId === state.helicopter.selectedId);
  if (!status || status.state !== "completed" || status.result?.kind !== "helicopter") return null;
  return { status, result: status.result };
}

function inScope(item: ModelNode, scope: Scope): boolean {
  const path = item.path;
  if (path === null) return false;
  const under = (value: string, prefix: string): boolean => value === prefix || value.startsWith(`${prefix}/`);
  if (item.kind === "directory") return scope.exact.some(exact => under(exact, path)) || scope.prefixes.some(prefix => under(path, prefix) || under(prefix, path));
  return scope.exact.includes(path) || scope.prefixes.some(prefix => under(path, prefix));
}

/** The model a tab draws, derived purely from store state (the page indexes and projects it). */
export function deriveModel(state: OverviewState, tab: OverviewTab): { model: GraphModel; sites: SourceSiteRow[]; note: string | null } {
  if (tab === "system") {
    const found = selectedSystemResult(state);
    if (found) return { model: modelFromSystem(found.result), sites: [], note: null };
    return { model: placeholderSystemModel(baseName(state.folder.replace(/[\\/]+$/, "")) || "Application"), sites: [], note: null };
  }
  if (tab === "helicopter") {
    // No generated business view yet: nothing is invented around an empty page.
    const found = selectedHelicopterResult(state);
    return { model: found ? modelFromHelicopter(found.result) : { nodes: [], edges: [] }, sites: [], note: null };
  }
  const level = state.levels[tab];
  const view = state.views[tab];
  if (view.neighborhood) {
    const hood = state.hood;
    if (!hood || hood.level !== tab || hood.id !== view.neighborhood.id || hood.status === "loading") return { model: { nodes: [], edges: [] }, sites: [], note: "Loading neighbourhood…" };
    const built = modelFromNeighbors({ level: tab, centerId: view.neighborhood.id, centerLabel: view.neighborhood.label, centerPath: view.neighborhood.path, rows: hood.rows });
    return { model: built.model, sites: built.sites, note: hood.total > hood.rows.length ? `Showing ${hood.rows.length.toLocaleString()} of ${hood.total.toLocaleString()} neighbours.` : null };
  }
  if (level.status !== "ready") return { model: { nodes: [], edges: [] }, sites: [], note: null };
  let model: GraphModel;
  if (level.mode === "directories") model = modelFromDirectories(level.groups);
  else {
    const base = modelFromOverview({ level: tab, groups: level.groups, files: level.files, symbols: level.symbols, directories: level.directories });
    model = level.extra.nodes.length ? { nodes: [...base.nodes, ...level.extra.nodes], edges: [...base.edges, ...level.extra.edges] } : base;
  }
  if (view.scope) {
    const scope = view.scope;
    model = scopeModel(model, item => inScope(item, scope));
  }
  return { model, sites: [], note: null };
}

export interface OverviewDeps {
  api: OverviewApi;
  clock?: TicketClock;
  storage?: Pick<Storage, "getItem" | "setItem"> | null;
}

export interface SystemOverviewStore extends Readable<OverviewState>, VisualizationSink {
  attach(hooks: OverviewHooks): void;
  attachStructureSink(sink: VisualizationSink | null): void;
  /** Called when the page opens. `resume` keeps the stored state (return from generation); a different folder otherwise switches. */
  enter(options: { folder?: string; resume?: boolean }): Promise<void>;
  switchFolder(folder: string): Promise<void>;
  refreshIndex(): Promise<void>;
  cancelIndex(): Promise<void>;
  viewPartial(on: boolean): void;
  setTab(tab: OverviewTab): void;
  setArea(area: OverviewArea): void;
  setSearch(tab: OverviewTab, text: string): void;
  runSearch(level: SourceTab, text: string, more?: boolean): Promise<void>;
  setExpanded(tab: OverviewTab, expanded: string[] | null): void;
  setFilters(tab: OverviewTab, patch: Partial<Pick<TabView, "relations" | "grades" | "derived" | "page">>): void;
  select(tab: OverviewTab, nodeId: string | null, edgeId?: string | null, ref?: string | null): void;
  setCamera(tab: OverviewTab, camera: Camera | null): void;
  openNeighborhood(level: SourceTab, target: Neighborhood | null): void;
  loadCycle(level: SourceTab, cycleId: string): Promise<void>;
  ensureLevel(level: SourceTab): Promise<void>;
  ensureInventory(): Promise<void>;
  loadCoverage(more?: boolean): Promise<void>;
  moreNeighbors(): Promise<void>;
  /** Drill from a diagram node to the files/functions it cites. Pushes the back stack. */
  drill(tab: SourceTab, scope: Scope, neighborhood?: Neighborhood | null): void;
  back(): boolean;
  clearScope(tab: SourceTab): void;
  loadSystem(): Promise<void>;
  selectSystemRevision(requestId: string): void;
  generateSystem(regenerate: boolean): Promise<GenerateOutcome>;
  /** Passive read of the saved Helicopter revisions; never generates. */
  loadHelicopter(): Promise<void>;
  selectHelicopterRevision(requestId: string): void;
  /** Explicit user action: ask the existing Architect for the plain-language business view of the selected snapshot. */
  generateHelicopter(regenerate: boolean): Promise<GenerateOutcome>;
  generate(request: GenerateRequest): Promise<GenerateOutcome>;
  structureHost(): StructureHost;
  /** Every user navigation: pending automatic returns lose their eligibility. */
  noteNavigation(): void;
  noteSourceSelection(sourceId: string | null): void;
  openResult(key: string): boolean;
  recheckTicket(key: string): void;
  dismissTicket(key: string): void;
  dispose(): void;
}

export function createSystemOverviewStore(deps: OverviewDeps): SystemOverviewStore {
  const api = deps.api;
  const clock: TicketClock = deps.clock ?? { setTimeout: (fn, ms) => window.setTimeout(fn, ms), clearTimeout: timer => window.clearTimeout(timer), now: () => Date.now() };
  const storage = deps.storage === undefined ? (typeof localStorage === "undefined" ? null : localStorage) : deps.storage;
  let state = initialOverviewState();
  const store = writable(state);
  const commit = (patch: Partial<OverviewState>): void => { state = { ...state, ...patch }; store.set(state); };
  let hooks: OverviewHooks | null = null;
  let structureSink: VisualizationSink | null = null;
  /** Bumped on every folder switch: late index/level/search/system results of another folder are dropped. */
  let generation = 0;
  /** Bumped when the effective snapshot changes. */
  let epoch = 0;
  let pollTimer: number | null = null;
  const autoStarted = new Set<string>();
  const sequences = { search: { files: 0, functions: 0 }, inspector: 0, hood: 0, coverage: 0, inventory: 0, system: 0, helicopter: 0 };

  const snapshotId = (): string | null => effectiveIndex(state)?.snapshotId ?? null;
  const setLevel = (level: SourceTab, patch: Partial<LevelData>): void => commit({ levels: { ...state.levels, [level]: { ...state.levels[level], ...patch } } });
  const setView = (tab: OverviewTab, patch: Partial<TabView>): void => commit({ views: { ...state.views, [tab]: { ...state.views[tab], ...patch } } });
  const query = <T>(snapshot: string, request: SourceIndexQuery): Promise<SourceIndexPage<T>> => api.querySourceIndex<T>(snapshot, request);
  const persist = (): void => {
    try { storage?.setItem(PREFS_KEY, JSON.stringify({ folder: state.folder, tab: state.tab })); } catch { /* Preferences never own results. */ }
  };

  /** Pages a query to its end (or `max` pages); returns null when the caller's scope became stale meanwhile. */
  async function collect<T>(fetch: (offset: number) => Promise<SourceIndexPage<T>>, valid: () => boolean, each?: (items: T[], total: number) => void, max = Infinity): Promise<{ items: T[]; total: number } | null> {
    const items: T[] = [];
    let offset: number | null = 0, total = 0;
    for (let pages = 0; offset !== null && pages < max; pages++) {
      const page: SourceIndexPage<T> = await fetch(offset);
      if (!valid()) return null;
      total = page.total;
      items.push(...page.items);
      each?.(page.items, total);
      offset = page.nextOffset;
    }
    return { items, total };
  }

  // ---- index ------------------------------------------------------------------------------------------------

  function stopPolling(): void {
    if (pollTimer !== null) clock.clearTimeout(pollTimer);
    pollTimer = null;
  }

  function applySelection(selection: SourceIndexSelection, mine: number): void {
    if (mine !== generation) return;
    const before = snapshotId();
    const selected = selection.selected;
    const attempt = selection.latestAttempt;
    commit({ index: { ...state.index, phase: "ready", selected, attempt, error: null, viewPartial: state.index.viewPartial && attempt?.state === "partial" } });
    if (snapshotId() !== before) snapshotChanged();
    if (isActive(attempt) && attempt) schedulePoll(attempt.jobId, mine);
  }

  function snapshotChanged(): void {
    epoch++;
    sequences.inspector++; sequences.hood++; sequences.coverage++; sequences.inventory++;
    sequences.search.files++; sequences.search.functions++;
    commit({
      levels: { files: blankLevel(), functions: blankLevel() }, inventory: EMPTY_INVENTORY, inspector: null, hood: null, coverage: blankCoverage(),
      search: { files: blankSearch(), functions: blankSearch() },
      views: { ...state.views, files: { ...state.views.files, expanded: null, selectedNode: null, selectedEdge: null, neighborhood: null, scope: null, page: 0 }, functions: { ...state.views.functions, expanded: null, selectedNode: null, selectedEdge: null, neighborhood: null, scope: null, page: 0 } },
      back: [],
    });
    const wanted = state.tab;
    if (isSourceTab(wanted) && state.area === "diagram") void ensureLevel(wanted);
    if (state.area === "structure") void ensureInventory();
  }

  function schedulePoll(jobId: string, mine: number): void {
    stopPolling();
    pollTimer = clock.setTimeout(async () => {
      pollTimer = null;
      if (mine !== generation) return;
      try {
        const status = await api.readSourceIndexJob(jobId);
        if (mine !== generation) return;
        if (isActive(status)) { commit({ index: { ...state.index, attempt: status } }); schedulePoll(jobId, mine); return; }
        // Terminal: the backend decides which complete index is selected.
        await loadIndex(false, mine);
      } catch (error) {
        if (mine === generation) commit({ index: { ...state.index, error: `Could not read the index job: ${errorText(error)}` } });
      }
    }, POLL_MS);
  }

  async function loadIndex(startIfMissing: boolean, mine: number): Promise<void> {
    const folder = state.folder;
    if (!folder) return;
    try {
      const selection = await api.getSourceIndex(folder);
      if (mine !== generation) return;
      applySelection(selection, mine);
      const key = repoKey(folder);
      // First visit of a repository: index it once, locally. Opening again (or failing) never starts another one silently.
      if (startIfMissing && !selection.selected && !isActive(selection.latestAttempt) && !autoStarted.has(key)) {
        autoStarted.add(key);
        await start(mine);
      }
    } catch (error) {
      if (mine === generation) commit({ index: { ...state.index, phase: "error", error: errorText(error) } });
    }
  }

  async function start(mine: number): Promise<void> {
    try {
      const status = await api.startSourceIndex(state.folder);
      if (mine !== generation) return;
      commit({ index: { ...state.index, attempt: status, error: null } });
      if (isActive(status)) schedulePoll(status.jobId, mine); else await loadIndex(false, mine);
    } catch (error) {
      if (mine === generation) commit({ index: { ...state.index, error: errorText(error) } });
    }
  }

  async function switchFolder(folder: string): Promise<void> {
    generation++;
    epoch++;
    stopPolling();
    sequences.system++; sequences.helicopter++; sequences.inspector++; sequences.hood++; sequences.coverage++; sequences.inventory++;
    sequences.search.files++; sequences.search.functions++;
    // A new project: pending automatic returns belong to the previous one.
    book.invalidateAutoReturn(ticket => repoKey(ticket.folder) !== repoKey(folder));
    const keep = state.tickets;
    state = { ...initialOverviewState(), folder, tab: state.tab, area: state.area, tickets: keep, navigationEpoch: state.navigationEpoch };
    state.index = { ...blankIndex(), phase: folder ? "loading" : "idle" };
    store.set(state);
    persist();
    if (!folder) return;
    const mine = generation;
    await loadIndex(true, mine);
    if (mine !== generation) return;
    await Promise.all([loadSystem(), loadHelicopter()]);
    if (mine === generation && state.area === "structure") void ensureInventory();
  }

  // ---- levels -----------------------------------------------------------------------------------------------

  async function ensureLevel(level: SourceTab): Promise<void> {
    const snapshot = snapshotId();
    if (!snapshot) return;
    const current = state.levels[level];
    if (current.snapshotId === snapshot && (current.status === "loading" || current.status === "ready")) return;
    const mine = generation, mineEpoch = epoch;
    const valid = (): boolean => mine === generation && mineEpoch === epoch;
    setLevel(level, { ...blankLevel(), snapshotId: snapshot, status: "loading" });
    try {
      const overview = (groupBy: "scc" | "directory") => (offset: number) => query<OverviewGroupRow>(snapshot, { kind: "overview", level, groupBy, offset, limit: PAGE });
      const first = await overview("scc")(0);
      if (!valid()) return;
      if (first.total > SIGMA_MAX_NODES) {
        // More nodes than the graph views can hold: the directory aggregation is the overview; exact nodes open by search/neighbourhood.
        const directories = await collect(overview("directory"), valid, (items, total) => setLevel(level, { loaded: Math.min(total, state.levels[level].loaded + items.length), total }), DIRECTORY_PAGE_CAP);
        if (!directories) return;
        setLevel(level, { status: "ready", mode: "directories", groups: directories.items, total: first.total, loaded: directories.items.length, directories: directories.items.map(group => group.path ?? "") });
        return;
      }
      const groups = await collect(overview("scc"), valid, (items, total) => setLevel(level, { loaded: Math.min(total, state.levels[level].loaded + items.length), total }));
      if (!groups) return;
      setLevel(level, { groups: groups.items, total: groups.total });
      const facts = level === "files"
        ? await collect<SourceFileRow>(offset => query(snapshot, { kind: "files", offset, limit: PAGE }), valid)
        : await collect<SourceSymbolRow>(offset => query(snapshot, { kind: "symbols", offset, limit: PAGE }), valid);
      if (!facts) return;
      const directories = await collect(overview("directory"), valid, undefined, DIRECTORY_PAGE_CAP);
      if (!directories) return;
      setLevel(level, {
        status: "ready", mode: "nodes", files: level === "files" ? facts.items as SourceFileRow[] : [], symbols: level === "functions" ? facts.items as SourceSymbolRow[] : [],
        directories: directories.items.map(group => group.path ?? ""),
      });
    } catch (error) {
      if (valid()) setLevel(level, { status: "error", error: errorText(error) });
    }
  }

  async function ensureInventory(): Promise<void> {
    const snapshot = snapshotId();
    if (!snapshot) return;
    if (state.inventory.snapshotId === snapshot && (state.inventory.status === "loading" || state.inventory.status === "ready")) return;
    const mine = generation, mineEpoch = epoch, sequence = ++sequences.inventory;
    const valid = (): boolean => mine === generation && mineEpoch === epoch && sequence === sequences.inventory;
    commit({ inventory: { ...EMPTY_INVENTORY, snapshotId: snapshot, status: "loading" } });
    try {
      const files: SourceFileRow[] = [];
      const loaded = await collect<SourceFileRow>(offset => query(snapshot, { kind: "files", offset, limit: PAGE }), valid, (items, total) => {
        files.push(...items);
        commit({ inventory: { ...state.inventory, files: [...files], total } });
      });
      if (!loaded) return;
      const directories = await collect(offset => query<OverviewGroupRow>(snapshot, { kind: "overview", level: "files", groupBy: "directory", offset, limit: PAGE }), valid, undefined, DIRECTORY_PAGE_CAP);
      if (!directories) return;
      commit({ inventory: { snapshotId: snapshot, status: "ready", files: loaded.items, directories: directories.items.map(group => group.path ?? ""), total: loaded.total, error: null } });
    } catch (error) {
      if (valid()) commit({ inventory: { ...state.inventory, status: "error", error: errorText(error) } });
    }
  }

  // ---- inspection ------------------------------------------------------------------------------------------

  /** `inspector` shows the selected node's callers/callees; `hood` holds the rows of the open neighbourhood graph (loaded to the cap). */
  async function inspect(level: SourceTab, id: string, more: boolean, slot: "inspector" | "hood" = "inspector"): Promise<void> {
    const snapshot = snapshotId();
    if (!snapshot) return;
    const mine = generation, mineEpoch = epoch, sequence = more ? sequences[slot] : ++sequences[slot];
    const valid = (): boolean => mine === generation && mineEpoch === epoch && sequence === sequences[slot];
    const previous = state[slot];
    let base: Inspector = more && previous && previous.id === id && previous.level === level
      ? { ...previous, status: "loading" } : { level, id, snapshotId: snapshot, status: "loading", rows: [], total: 0, nextOffset: 0, error: null };
    commit({ [slot]: base } as Partial<OverviewState>);
    try {
      do {
        const page = await query<NeighborRow>(snapshot, { kind: "neighbors", level, id, direction: "both", offset: base.nextOffset ?? 0, limit: PAGE });
        if (!valid()) return;
        const rows = [...base.rows, ...page.items];
        base = { ...base, status: "ready", rows, total: page.total, nextOffset: page.nextOffset !== null && rows.length < NEIGHBOR_CAP ? page.nextOffset : null, error: null };
        commit({ [slot]: base } as Partial<OverviewState>);
      } while (slot === "hood" && base.nextOffset !== null);
    } catch (error) {
      if (valid()) commit({ [slot]: { ...base, status: "error", error: errorText(error) } } as Partial<OverviewState>);
    }
  }

  async function loadCycle(level: SourceTab, cycleId: string): Promise<void> {
    const snapshot = snapshotId();
    const levelData = state.levels[level];
    const group = levelData.groups.find(item => item.id === cycleId);
    if (!snapshot || !group || group.membersTruncated || levelData.extra.nodes.some(item => item.parentId === cycleId)) return;
    const mine = generation, mineEpoch = epoch;
    const valid = (): boolean => mine === generation && mineEpoch === epoch;
    try {
      const members = new Set(group.members);
      const nodes = new Map<string, ModelNode>();
      const edges = new Map<string, ModelEdge>();
      const parentNode = group.id;
      for (const member of group.members.slice(0, 60)) {
        const page = await query<NeighborRow>(snapshot, { kind: "neighbors", level, id: member, direction: "out", offset: 0, limit: PAGE });
        if (!valid()) return;
        const file = levelData.files.find(item => item.id === member);
        const symbol = levelData.symbols.find(item => item.id === member);
        const own = nodes.get(member);
        if (!own) nodes.set(member, { id: member, kind: level === "files" ? "file" : symbol?.kind ?? "function", label: level === "files" ? baseName(file?.path ?? member) : symbol?.qualifiedName || symbol?.name || member, subtitle: file?.path ?? symbol?.path ?? "", description: "", parentId: parentNode, synthetic: false, evidence: "parsed", role: null, evidenceIds: [], memberCount: 1, cyclic: true, selfLoops: 0, selfLoopIds: [], lazyMembers: [], path: file?.path ?? symbol?.path ?? null, ref: member });
        for (const row of page.items) {
          if (row.kind !== "edge" || !members.has(row.peerId)) continue;
          edges.set(row.edge.id, { id: row.edge.id, source: member, target: row.peerId, direction: "directed", relation: row.edge.relation, grade: row.edge.resolution, derived: false, label: row.edge.relation, count: 1, evidenceIds: row.edge.evidenceIds, truncated: false });
        }
      }
      setLevel(level, { extra: { nodes: [...levelData.extra.nodes, ...nodes.values()], edges: [...levelData.extra.edges, ...edges.values()] } });
    } catch (error) {
      if (valid()) setLevel(level, { error: `Could not load the cycle members: ${errorText(error)}` });
    }
  }

  async function runSearch(level: SourceTab, text: string, more = false): Promise<void> {
    const snapshot = snapshotId();
    const trimmed = text.trim();
    if (!snapshot || !trimmed) { sequences.search[level]++; commit({ search: { ...state.search, [level]: blankSearch() } }); return; }
    const mine = generation, mineEpoch = epoch, sequence = more ? sequences.search[level] : ++sequences.search[level];
    const valid = (): boolean => mine === generation && mineEpoch === epoch && sequence === sequences.search[level];
    const previous = state.search[level];
    const base: SearchResults = more && previous.text === trimmed ? { ...previous, status: "loading" } : { ...blankSearch(), text: trimmed, status: "loading" };
    const offset = more && previous.text === trimmed ? previous.nextOffset ?? 0 : 0;
    commit({ search: { ...state.search, [level]: base } });
    try {
      const page = await query<SourceFileRow | SourceSymbolRow>(snapshot, { kind: "search", level, text: trimmed, offset, limit: PAGE });
      if (!valid()) return;
      commit({ search: { ...state.search, [level]: { ...base, status: "ready", items: [...(offset === 0 ? [] : base.items), ...page.items], total: page.total, nextOffset: page.nextOffset } } });
    } catch (error) {
      if (valid()) commit({ search: { ...state.search, [level]: { ...base, status: "error", error: errorText(error) } } });
    }
  }

  async function loadCoverage(more = false): Promise<void> {
    const snapshot = snapshotId();
    if (!snapshot) return;
    const mine = generation, mineEpoch = epoch, sequence = ++sequences.coverage;
    const valid = (): boolean => mine === generation && mineEpoch === epoch && sequence === sequences.coverage;
    const previous = state.coverage;
    const base: CoverageState = more && previous.snapshotId === snapshot ? { ...previous, status: "loading" } : { ...blankCoverage(), snapshotId: snapshot, status: "loading" };
    const offset = base.items.length === 0 ? 0 : base.nextOffset ?? 0;
    commit({ coverage: base });
    try {
      const page = await query<CoverageRow>(snapshot, { kind: "coverage", offset, limit: PAGE });
      if (!valid()) return;
      commit({ coverage: { ...base, status: "ready", items: [...base.items, ...page.items], total: page.total, nextOffset: page.nextOffset, error: null } });
    } catch (error) {
      if (valid()) commit({ coverage: { ...base, status: "error", error: errorText(error) } });
    }
  }

  // ---- saved system diagram + visualization statuses -------------------------------------------------------

  /** Newest first by creation time; a late older result is kept in the list but never displaces a newer selection. */
  function mergeRevision(slot: ArchitectTab, status: ProjectInsightStatus): void {
    const kind = SLOT_KIND[slot];
    if (status.kind !== kind || repoKey(status.folder) !== repoKey(state.folder)) return;
    const current = state[slot];
    const others = current.revisions.filter(item => item.requestId !== status.requestId);
    const known = current.revisions.find(item => item.requestId === status.requestId);
    // Never regress a revision to an older state (a late read of a pending request after completion).
    const next = known?.state === "completed" && status.state !== "completed" ? known : status;
    const revisions = [next, ...others].sort((a, b) => (b.createdAt ?? 0) - (a.createdAt ?? 0));
    const completed = (item: ProjectInsightStatus | undefined): boolean => item?.state === "completed" && item.result?.kind === kind;
    const selected = revisions.find(item => item.requestId === current.selectedId);
    const newest = revisions.find(item => completed(item) && !item.supersededBy);
    let selectedId = current.selectedId;
    if (!selected || !completed(selected)) selectedId = newest?.requestId ?? selectedId;
    else if (completed(next) && next.requestId !== selected.requestId && (next.createdAt ?? 0) > (selected.createdAt ?? 0) && !next.supersededBy) selectedId = next.requestId;
    commit({ [slot]: { ...current, revisions, selectedId } } as Partial<OverviewState>);
  }

  /** Passive: reads what is saved for the kind; never asks the Architect. */
  async function loadSaved(slot: ArchitectTab): Promise<void> {
    if (!state.folder) return;
    const mine = generation, sequence = ++sequences[slot];
    commit({ [slot]: { ...state[slot], loading: true, error: null } } as Partial<OverviewState>);
    try {
      const found = await api.getProjectInsight(state.folder, SLOT_KIND[slot]);
      if (mine !== generation || sequence !== sequences[slot]) return;
      commit({ [slot]: { ...state[slot], loading: false } } as Partial<OverviewState>);
      if (found) mergeRevision(slot, found);
    } catch (error) {
      if (mine === generation && sequence === sequences[slot]) commit({ [slot]: { ...state[slot], loading: false, error: errorText(error) } } as Partial<OverviewState>);
    }
  }
  const loadSystem = (): Promise<void> => loadSaved("system");
  const loadHelicopter = (): Promise<void> => loadSaved("helicopter");

  // ---- tickets ---------------------------------------------------------------------------------------------

  const book = new TicketBook({
    api: { requestSystemVisualization: (...args) => api.requestSystemVisualization(...args), readProjectInsight: id => api.readProjectInsight(id) },
    architectId: () => hooks?.architectId() ?? null,
    openArchitect: id => hooks ? hooks.openArchitect(id) : Promise.resolve(false),
    epoch: () => state.navigationEpoch,
    following: ticket => hooks?.following(ticket) ?? false,
    quiescent: ticket => hooks?.quiescent(ticket) ?? false,
    returnToOverview: ticket => { applyTicketTarget(ticket); hooks?.returnToOverview(ticket); },
    received: status => receive(status),
    changed: tickets => commit({ tickets: [...tickets] }),
    clock,
  }, repoKey);

  function receive(status: ProjectInsightStatus): void {
    if (status.kind === "systemOverview") mergeRevision("system", status);
    else if (status.kind === "helicopter") mergeRevision("helicopter", status);
    else if (status.kind === "structureCurrent" || status.kind === "structureProposed") structureSink?.receive(status);
  }

  /** Makes the store show the corresponding view of a finished ticket (the parent then navigates to the page). */
  function applyTicketTarget(ticket: VisualizationTicket): void {
    if (repoKey(ticket.folder) !== repoKey(state.folder)) { void switchFolder(ticket.folder); }
    if (ticket.destinationTab === "system" || ticket.destinationTab === "helicopter") {
      const slot: ArchitectTab = ticket.destinationTab;
      commit({ tab: slot, area: "diagram" });
      const wanted = ticket.selectedRevisionId;
      if (wanted && state[slot].revisions.some(item => item.requestId === wanted && item.state === "completed")) commit({ [slot]: { ...state[slot], selectedId: wanted } } as Partial<OverviewState>);
    } else commit({ area: "structure" });
    persist();
  }

  /** The Architect-generated picture a request kind fills, or null for the structure kinds (they have their own store). */
  const slotOf = (kind: GenerateRequest["kind"]): ArchitectTab | null => kind === "systemOverview" ? "system" : kind === "helicopter" ? "helicopter" : null;

  async function generate(request: GenerateRequest): Promise<GenerateOutcome> {
    const slot = slotOf(request.kind);
    const settle = (admitting: boolean, error: string | null): void => { if (slot) commit({ [slot]: { ...state[slot], admitting, error } } as Partial<OverviewState>); };
    settle(true, null);
    try {
      const outcome = await book.generate(request);
      settle(false, outcome.error);
      return outcome;
    } catch (error) {
      settle(false, errorText(error));
      return { admitted: false, status: null, error: errorText(error), uncertain: false };
    }
  }

  const self: SystemOverviewStore = {
    subscribe: store.subscribe,
    attach(next) { hooks = next; },
    attachStructureSink(sink) { structureSink = sink; },
    receive,
    async enter({ folder, resume }) {
      const target = folder ?? "";
      if (state.folder === "") {
        let saved: { folder?: string; tab?: OverviewTab } = {};
        try { saved = JSON.parse(storage?.getItem(PREFS_KEY) ?? "{}") as typeof saved; } catch { saved = {}; }
        if (saved.tab === "files" || saved.tab === "functions" || saved.tab === "system" || saved.tab === "helicopter") commit({ tab: saved.tab });
        await switchFolder(target || saved.folder || "");
        return;
      }
      if (resume || !target || repoKey(target) === repoKey(state.folder)) {
        // Same project: keep the view; look at the index metadata again (staleness) without starting work.
        if (state.index.phase !== "loading") void loadIndex(false, generation);
        return;
      }
      await switchFolder(target);
    },
    switchFolder,
    async refreshIndex() {
      if (!state.folder) return;
      const mine = generation;
      commit({ index: { ...state.index, error: null } });
      await start(mine);
    },
    async cancelIndex() {
      const attempt = state.index.attempt;
      if (!attempt || !isActive(attempt)) return;
      const mine = generation;
      try {
        const status = await api.cancelSourceIndex(attempt.jobId);
        if (mine !== generation) return;
        stopPolling();
        commit({ index: { ...state.index, attempt: status } });
        await loadIndex(false, mine);
      } catch (error) {
        if (mine === generation) commit({ index: { ...state.index, error: errorText(error) } });
      }
    },
    viewPartial(on) {
      const before = snapshotId();
      commit({ index: { ...state.index, viewPartial: on && state.index.attempt?.state === "partial" } });
      if (snapshotId() !== before) snapshotChanged();
    },
    setTab(tab) {
      if (state.tab === tab && state.area === "diagram") return;
      commit({ tab, area: "diagram" });
      persist();
      if (isSourceTab(tab)) void ensureLevel(tab);
    },
    setArea(area) {
      commit({ area });
      if (area === "structure") void ensureInventory();
      else if (isSourceTab(state.tab)) void ensureLevel(state.tab);
    },
    setSearch(tab, text) { setView(tab, { search: text }); },
    runSearch,
    setExpanded(tab, expanded) { setView(tab, { expanded, page: 0 }); },
    setFilters(tab, patch) { setView(tab, patch); },
    select(tab, nodeId, edgeId = null, ref = null) {
      setView(tab, { selectedNode: nodeId, selectedEdge: edgeId });
      if (isSourceTab(tab) && nodeId && ref) void inspect(tab, ref, false);
    },
    setCamera(tab, camera) { setView(tab, { camera }); },
    openNeighborhood(level, target) {
      setView(level, { neighborhood: target, selectedNode: target ? target.id : null, selectedEdge: null, camera: null, page: 0 });
      if (target) { void inspect(level, target.id, false, "hood"); void inspect(level, target.id, false); }
    },
    loadCycle,
    ensureLevel,
    ensureInventory,
    loadCoverage,
    async moreNeighbors() {
      const current = state.inspector;
      if (current && current.status === "ready" && current.nextOffset !== null) await inspect(current.level, current.id, true);
    },
    drill(tab, scope, neighborhood = null) {
      commit({ back: [...state.back, { tab: state.tab, area: state.area, view: { ...state.views[state.tab] } }].slice(-20) });
      setView(tab, { scope, neighborhood, expanded: null, selectedNode: null, selectedEdge: null, camera: null, page: 0 });
      commit({ tab, area: "diagram" });
      persist();
      void ensureLevel(tab);
      if (neighborhood) { void inspect(tab, neighborhood.id, false, "hood"); void inspect(tab, neighborhood.id, false); }
    },
    clearScope(tab) { setView(tab, { scope: null, expanded: null, camera: null, selectedNode: null, selectedEdge: null, page: 0 }); },
    back() {
      const entry = state.back[state.back.length - 1];
      if (!entry) return false;
      commit({ back: state.back.slice(0, -1), tab: entry.tab, area: entry.area, views: { ...state.views, [entry.tab]: entry.view } });
      persist();
      return true;
    },
    loadSystem,
    loadHelicopter,
    selectSystemRevision(requestId) {
      if (state.system.revisions.some(item => item.requestId === requestId && item.state === "completed")) commit({ system: { ...state.system, selectedId: requestId } });
    },
    selectHelicopterRevision(requestId) {
      if (state.helicopter.revisions.some(item => item.requestId === requestId && item.state === "completed")) commit({ helicopter: { ...state.helicopter, selectedId: requestId } });
    },
    generateSystem(regenerate) {
      const index = effectiveIndex(state);
      if (!index?.snapshotId) return Promise.resolve({ admitted: false, status: null, error: "Build the source index first.", uncertain: false });
      const previous = selectedSystemResult(state)?.status.requestId ?? null;
      // The backend answers a plain request with the newest request of this snapshot, even a failed one (an ended Architect
      // session), so Generate after a failure must explicitly supersede that request or it could never ask again.
      const newest = state.system.revisions[0];
      const failed = newest?.state === "failed" && newest.snapshotId === index.snapshotId ? newest.requestId : null;
      return generate({ kind: "systemOverview", folder: index.folder, snapshotId: index.snapshotId, supersedesRequestId: regenerate ? previous : failed, area: "system" });
    },
    generateHelicopter(regenerate) {
      const index = effectiveIndex(state);
      if (!index?.snapshotId) return Promise.resolve({ admitted: false, status: null, error: "Build the source index first.", uncertain: false });
      const previous = selectedHelicopterResult(state)?.status.requestId ?? null;
      // Same rule as the System diagram: after a failed request (an ended Architect session) a plain request would return that failure
      // forever, so Generate explicitly supersedes it.
      const newest = state.helicopter.revisions[0];
      const failed = newest?.state === "failed" && newest.snapshotId === index.snapshotId ? newest.requestId : null;
      return generate({ kind: "helicopter", folder: index.folder, snapshotId: index.snapshotId, supersedesRequestId: regenerate ? previous : failed, area: "helicopter" });
    },
    generate,
    structureHost() {
      return {
        folder: () => effectiveIndex(state)?.folder ?? state.folder,
        snapshotId: () => effectiveIndex(state)?.snapshotId ?? null,
        architect: () => hooks?.architectSession() ?? null,
        generate,
        openArchitect: (id, replay) => hooks ? hooks.openArchitect(id, replay) : Promise.resolve(false),
      };
    },
    noteNavigation() { commit({ navigationEpoch: state.navigationEpoch + 1 }); },
    noteSourceSelection(sourceId) { book.invalidateAutoReturn(ticket => ticket.kind === "structureProposed" && ticket.sourceId !== sourceId); },
    openResult: key => book.openResult(key),
    recheckTicket: key => book.recheck(key),
    dismissTicket: key => book.dismiss(key),
    dispose() { stopPolling(); book.dispose(); },
  };
  return self;
}

