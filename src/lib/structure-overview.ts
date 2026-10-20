import { writable, type Readable } from "svelte/store";
import type { api, ProjectInsightStatus, StructureApproval, StructureSourceRow } from "./api";
import type { ArchitectDescriptor, GenerateOutcome, GenerateRequest, StructureHost, VisualizationSink } from "./overview-types";
import { canApprove, type ApprovalCheck } from "./structure-view";

// Parent-lived store of the repository-structure area. It never reaches the Architect or a model on its own:
// only generateCurrent / generateProposed / regenerate (explicit user actions) call `host.generate`; everything else
// is a passive backend read of already saved data. The runtime `api` is injected by `connect`, never imported.

export type StructureApi = Pick<typeof api, "getProjectInsight" | "readProjectInsight" | "listStructureSources" | "readStructureSource" | "selectLastStructureSource" | "approveStructurePreview">;
export type StructureKind = "structureCurrent" | "structureProposed";
export const STRUCTURE_KINDS: readonly StructureKind[] = ["structureCurrent", "structureProposed"];

export const NEED_ARCHITECT = "Open The Architect first";
const NOT_ADMITTED = "The Architect did not accept the request.";
const UNCERTAIN = "Delivery to the Architect is uncertain. The request is not resent automatically; open the Architect to check.";
const PAGE_CACHE = 8;

export interface SourcePage {
  /** Text read so far (pages of at most 64 KiB, accumulated by nextOffset). */
  text: string; nextOffset: number | null; sha256: string;
  /** The first page has been read, so the human has been shown the beginning of the document. */
  loaded: boolean; loading: boolean; error: string | null;
}
export interface StructureState {
  folder: string; snapshotId: string | null;
  sources: StructureSourceRow[]; sessionId: string | null;
  /** `id:generation:sessionId` of the Architect owner the list was read from. */
  sourcesKey: string | null; sourcesLoading: boolean; sourcesLoaded: boolean; sourcesError: string | null; selectingLast: boolean;
  /** The source the target preview is generated from. */
  selectedSourceId: string | null;
  /** The source whose text the viewer shows. */
  previewId: string | null;
  pages: Record<string, SourcePage>;
  /** Newest first, keyed by requestId. */
  revisions: Record<StructureKind, ProjectInsightStatus[]>;
  selected: Record<StructureKind, string | null>;
  busy: Record<StructureKind, boolean>;
  errors: Record<StructureKind, string | null>;
  uncertain: Record<StructureKind, boolean>;
  savedLoading: boolean; savedError: string | null;
  approving: boolean; approvalError: string | null; discussError: string | null;
}

const initialState = (folder: string, snapshotId: string | null): StructureState => ({
  folder, snapshotId, sources: [], sessionId: null, sourcesKey: null, sourcesLoading: false, sourcesLoaded: false, sourcesError: null, selectingLast: false,
  selectedSourceId: null, previewId: null, pages: {},
  revisions: { structureCurrent: [], structureProposed: [] }, selected: { structureCurrent: null, structureProposed: null },
  busy: { structureCurrent: false, structureProposed: false }, errors: { structureCurrent: null, structureProposed: null },
  uncertain: { structureCurrent: false, structureProposed: false },
  savedLoading: false, savedError: null, approving: false, approvalError: null, discussError: null,
});

export const folderKey = (folder: string): string => folder.replace(/^\\\\\?\\/, "").replace(/\\/g, "/").replace(/\/+$/, "").toLowerCase();
/** The owner whose captured messages may be shown: Architect id + generation + known session. Null while the session is unknown. */
const architectKey = (architect: ArchitectDescriptor | null): string | null => architect && architect.sessionId ? `${architect.id}:${architect.generation}:${architect.sessionId}` : null;
const errorText = (error: unknown): string => error instanceof Error ? error.message : String(error);
const stateRank = (state: ProjectInsightStatus["state"]): number => state === "completed" ? 3 : state === "pending" ? 0 : state === "submitted" ? 1 : 2;

// ---------- Derived, pure ----------

export const selectedRevision = (state: StructureState, kind: StructureKind): ProjectInsightStatus | null =>
  state.revisions[kind].find(revision => revision.requestId === state.selected[kind]) ?? null;
export const selectedSource = (state: StructureState): StructureSourceRow | null =>
  state.sources.find(row => row.sourceId === state.selectedSourceId) ?? null;
/** True only after the text of the selected source was read from the Architect session and can be shown to the human. */
export function sourceViewed(state: StructureState): boolean {
  const row = selectedSource(state);
  const page = row ? state.pages[row.sourceId] : undefined;
  return !!row && !row.error && !!page && page.loaded && page.sha256 === row.sha256;
}
/** The id of a completed newer revision that replaced this one (the backend's `supersededBy`, or a locally known completed successor). */
export const supersededBy = (list: readonly ProjectInsightStatus[], revision: ProjectInsightStatus): string | null =>
  revision.supersededBy || list.find(other => other.supersedesRequestId === revision.requestId && other.state === "completed")?.requestId || null;
/** The newest request of a kind that is still waiting for the Architect. */
export const inFlightRevision = (state: StructureState, kind: StructureKind): ProjectInsightStatus | null =>
  state.revisions[kind].find(revision => revision.state === "pending" || revision.state === "submitted") ?? null;

export function approveCheck(state: StructureState): ApprovalCheck {
  const list = state.revisions.structureProposed;
  const revision = selectedRevision(state, "structureProposed");
  const row = selectedSource(state);
  const check = canApprove(revision && { ...revision, supersededBy: supersededBy(list, revision) }, {
    snapshotId: state.snapshotId, source: row ? { sourceId: row.sourceId, sha256: row.sha256 } : null,
  });
  if (!check.ok) return check;
  if (state.approving || state.busy.structureProposed) return { ok: false, reason: "Another request is in progress." };
  if (!sourceViewed(state)) return { ok: false, reason: "Read the selected Markdown before approving." };
  return check;
}
/** Why Analyze / Preview cannot be clicked right now, or null when they can. */
export function generateBlock(state: StructureState, kind: StructureKind, architect: ArchitectDescriptor | null): string | null {
  if (!state.snapshotId) return "No complete source index snapshot is selected yet.";
  if (state.busy[kind]) return "Waiting for the Architect…";
  if (kind === "structureProposed") {
    if (!architect) return NEED_ARCHITECT;
    const row = selectedSource(state);
    if (!row) return "Select the target Markdown first.";
    if (row.error) return `The selected message cannot be used: ${row.error}`;
    if (!sourceViewed(state)) return "Read the selected Markdown first.";
    if (state.sourcesKey !== architectKey(architect)) return "The Markdown list belongs to another Architect session; refresh it.";
  }
  return null;
}

// ---------- Store ----------

/** The parent-lived store of the structure area; it also is the sink the parent's watcher feeds with saved statuses. */
export interface StructureStore extends VisualizationSink {
  subscribe: Readable<StructureState>["subscribe"];
  /** Injects the host and the backend, then passively reads saved results for the current folder. Never generates. */
  connect(host: StructureHost, api: StructureApi): void;
  /** A folder change resets everything; a snapshot change keeps sources and revisions (shown as stale) and drops in-flight results. Passively reads saved results. */
  setFolder(folder: string, snapshotId: string | null): void;
  /** Reads the Architect's captured Markdown list. `automatic` (a mount, owner or focus trigger) reads only a ready Architect with a known session. */
  refreshSources(options?: { automatic?: boolean }): Promise<void>;
  /** Uses this Markdown for the target preview (and reads it). */
  selectSource(sourceId: string): Promise<void>;
  /** Reads the first page of a source, or with `more` the next page, into the viewer. Never changes the selection. */
  readSource(sourceId: string, more?: boolean): Promise<void>;
  useLastSubmitted(): Promise<void>;
  selectRevision(kind: StructureKind, requestId: string): void;
  refreshRevision(requestId: string): Promise<void>;
  loadSaved(): Promise<void>;
  generateCurrent(): Promise<void>;
  generateProposed(): Promise<void>;
  regenerate(kind: StructureKind): Promise<void>;
  approve(): Promise<void>;
  /** Replays the saved result into the Architect (no approval). */
  discuss(requestId: string): Promise<boolean>;
}

export function createStructureStore(): StructureStore {
  let state = initialState("", null);
  const store = writable(state);
  const commit = (patch: Partial<StructureState>): void => { state = { ...state, ...patch }; store.set(state); };
  const setKind = <T>(field: Record<StructureKind, T>, kind: StructureKind, value: T): Record<StructureKind, T> => ({ ...field, [kind]: value });

  let host: StructureHost | null = null;
  let backend: StructureApi | null = null;
  /** Bumped when the folder changes: drops every late source/approval/saved result. */
  let scope = 0;
  /** Bumped when the folder or the snapshot changes: additionally drops late generation results. */
  let epoch = 0;
  let sourcePinned = false;
  const pinned: Record<StructureKind, { id: string; supersededBy: string | null } | null> = { structureCurrent: null, structureProposed: null };
  const arrival = new Map<string, number>();
  let arrivalSeq = 0;
  let savedFlight: { epoch: number; promise: Promise<void> } | null = null;
  /** The newest source-list read: `key` is the owner and its readiness, so a not-ready read never stands in for the later ready one. */
  let sourcesFlight: { scope: number; key: string; promise: Promise<void> | null } | null = null;

  const architect = (): ArchitectDescriptor | null => host?.architect() ?? null;
  const sameArchitect = (a: ArchitectDescriptor | null, b: ArchitectDescriptor | null): boolean =>
    a !== null && b !== null && a.id === b.id && a.generation === b.generation && a.sessionId === b.sessionId;
  const sourceCurrent = (mine: number, arch: ArchitectDescriptor): boolean => mine === scope && sameArchitect(architect(), arch);

  // ----- revisions -----

  function reselect(kind: StructureKind): void {
    const list = state.revisions[kind];
    const pin = pinned[kind];
    if (pin) {
      const kept = list.find(revision => revision.requestId === pin.id);
      if (kept && supersededBy(list, kept) === pin.supersededBy) { if (state.selected[kind] !== pin.id) commit({ selected: setKind(state.selected, kind, pin.id) }); return; }
      pinned[kind] = null;
    }
    const score = (revision: ProjectInsightStatus): number => (revision.state === "completed" ? 16 : 0) + (supersededBy(list, revision) ? 0 : 8)
      + (kind === "structureProposed" && revision.sourceId && revision.sourceId === state.selectedSourceId ? 4 : 0) + (revision.snapshotId === state.snapshotId ? 2 : 0);
    let best: ProjectInsightStatus | null = null;
    let bestScore = -1;
    for (const revision of list) { const value = score(revision); if (value > bestScore) { best = revision; bestScore = value; } }
    const id = best?.requestId ?? null;
    if (id !== state.selected[kind]) commit({ selected: setKind(state.selected, kind, id) });
  }

  /** Merges a saved status; an older state or a late read never replaces a newer one, and supersession/approval only ever accumulate. */
  function merge(status: ProjectInsightStatus): boolean {
    if (status.kind !== "structureCurrent" && status.kind !== "structureProposed") return false;
    if (folderKey(status.folder) !== folderKey(state.folder)) return false;
    const kind: StructureKind = status.kind;
    const list = state.revisions[kind];
    const existing = list.find(revision => revision.requestId === status.requestId);
    let next = status;
    if (existing) {
      const base = stateRank(status.state) >= stateRank(existing.state) ? status : existing;
      const incomingApproval: StructureApproval | null = status.approval ?? null;
      const knownApproval: StructureApproval | null = existing.approval ?? null;
      const unsettled = (value: StructureApproval): boolean => value.delivery === "pending" || value.delivery === "claimed";
      const approval = !knownApproval ? incomingApproval : !incomingApproval ? knownApproval
        : (unsettled(incomingApproval) && (!unsettled(knownApproval) || (incomingApproval.delivery === "pending" && knownApproval.delivery === "claimed")) ? knownApproval : incomingApproval);
      next = { ...base, supersededBy: status.supersededBy ?? existing.supersededBy ?? null, approval };
    } else arrival.set(status.requestId, ++arrivalSeq);
    const revisions = [...list.filter(revision => revision.requestId !== status.requestId), next]
      .sort((a, b) => (b.createdAt ?? 0) - (a.createdAt ?? 0) || (arrival.get(b.requestId) ?? 0) - (arrival.get(a.requestId) ?? 0));
    commit({ revisions: setKind(state.revisions, kind, revisions) });
    reselect(kind);
    return true;
  }

  function loadSaved(): Promise<void> {
    if (!backend || !state.folder) return Promise.resolve();
    if (savedFlight && savedFlight.epoch === epoch) return savedFlight.promise;
    const api = backend;
    const mine = epoch;
    const folder = state.folder;
    const flight: { epoch: number; promise: Promise<void> } = { epoch: mine, promise: Promise.resolve() };
    flight.promise = (async () => {
      commit({ savedLoading: true, savedError: null });
      const results = await Promise.allSettled(STRUCTURE_KINDS.map(kind => Promise.resolve().then(() => api.getProjectInsight(folder, kind))));
      if (mine !== epoch) return;
      const failures: string[] = [];
      for (const result of results) {
        if (result.status === "rejected") failures.push(errorText(result.reason));
        else if (result.value) merge(result.value);
      }
      if (savedFlight === flight) savedFlight = null;
      commit({ savedLoading: false, savedError: failures.length ? failures.join("; ") : null });
    })();
    savedFlight = flight;
    return flight.promise;
  }

  async function refreshRevision(requestId: string): Promise<void> {
    if (!backend) return;
    const mine = scope;
    try {
      const status = await backend.readProjectInsight(requestId);
      if (mine === scope) merge(status);
    } catch (error) {
      if (mine === scope) commit({ savedError: errorText(error) });
    }
  }

  function selectRevision(kind: StructureKind, requestId: string): void {
    const list = state.revisions[kind];
    const revision = list.find(candidate => candidate.requestId === requestId);
    if (!revision) return;
    pinned[kind] = { id: requestId, supersededBy: supersededBy(list, revision) };
    commit({ selected: setKind(state.selected, kind, requestId) });
  }

  // ----- sources -----

  function setPage(id: string, page: SourcePage): void {
    const pages: Record<string, SourcePage> = { ...state.pages, [id]: page };
    for (const key of Object.keys(pages)) {
      if (Object.keys(pages).length <= PAGE_CACHE) break;
      if (key !== id && key !== state.selectedSourceId && !pages[key].loading) delete pages[key];
    }
    commit({ pages });
  }

  async function readSource(id: string, more = false): Promise<void> {
    const api = backend;
    if (!api || !host) return;
    const arch = architect();
    if (!arch) { commit({ sourcesError: NEED_ARCHITECT }); return; }
    if (state.sourcesKey === null || state.sourcesKey !== architectKey(arch)) { commit({ sourcesError: "The Markdown list belongs to another Architect session; refresh it." }); return; }
    const row = state.sources.find(candidate => candidate.sourceId === id);
    if (!row) { commit({ sourcesError: `Unknown Markdown source ${id}; refresh the list.` }); return; }
    const known = state.pages[id];
    if (known?.loading) return;
    const empty: SourcePage = { text: "", nextOffset: null, sha256: row.sha256, loaded: false, loading: false, error: null };
    if (row.error) { commit({ previewId: id }); setPage(id, { ...empty, error: `This message cannot be read: ${row.error}` }); return; }
    const base = more && known ? known : empty;
    const offset = more ? known?.nextOffset ?? null : 0;
    if (offset === null) return;
    const mine = scope;
    commit({ previewId: id, sourcesError: null });
    setPage(id, { ...base, loading: true, error: null });
    try {
      const page = await api.readStructureSource(arch.id, arch.generation, id, offset);
      if (!sourceCurrent(mine, arch)) return;
      const fresh = state.sources.find(candidate => candidate.sourceId === id);
      if (!fresh || page.sourceId !== id || page.sha256 !== fresh.sha256 || (more && base.loaded && base.sha256 !== page.sha256)) {
        setPage(id, { ...empty, error: "The Markdown changed or does not match the listed message; refresh the list." });
        return;
      }
      const stalled = page.nextOffset !== null && page.nextOffset <= offset;
      setPage(id, {
        text: base.text + page.text, nextOffset: stalled ? null : page.nextOffset, sha256: page.sha256, loaded: true, loading: false,
        error: stalled ? "The backend returned a page offset that does not advance; the text may be incomplete." : null,
      });
    } catch (error) {
      if (sourceCurrent(mine, arch)) setPage(id, { ...base, loading: false, error: errorText(error) });
    }
  }

  /** Drops the list, pages and selection and any read still running for them; `settled` marks the list as read (with `message`) instead of unknown. */
  function clearSources(message: string | null, settled = true): void {
    sourcePinned = false;
    sourcesFlight = null;
    commit({ sources: [], sessionId: null, sourcesKey: null, sourcesLoading: false, sourcesLoaded: settled, sourcesError: message, selectedSourceId: null, previewId: null, pages: {} });
    reselect("structureProposed");
  }

  function applyList(rows: StructureSourceRow[], sessionId: string | null, key: string): void {
    const sources = [...rows].sort((a, b) => b.capturedAt - a.capturedAt);
    let selectedId = state.selectedSourceId;
    if (selectedId && !sources.some(row => row.sourceId === selectedId)) { selectedId = null; sourcePinned = false; }
    if (!sourcePinned || !selectedId) selectedId = sources.find(row => !row.error)?.sourceId ?? null;
    const pages: Record<string, SourcePage> = {};
    for (const row of sources) { const page = state.pages[row.sourceId]; if (page && page.sha256 === row.sha256) pages[row.sourceId] = page; }
    const previewId = state.previewId && sources.some(row => row.sourceId === state.previewId) ? state.previewId : selectedId;
    commit({ sources, sessionId, sourcesKey: key, sourcesLoaded: true, sourcesError: null, selectedSourceId: selectedId, previewId, pages });
    reselect("structureProposed");
    if (selectedId && !state.pages[selectedId]?.loaded) void readSource(selectedId);
  }

  function refreshSources(options: { automatic?: boolean } = {}): Promise<void> {
    const api = backend;
    if (!api || !host) return Promise.resolve();
    const arch = architect();
    if (!arch) { clearSources(NEED_ARCHITECT); return Promise.resolve(); }
    const key = architectKey(arch);
    // A list read from another owner or session is not authorized for this one, whatever is read next.
    if (state.sourcesKey !== null && state.sourcesKey !== key) clearSources(null, false);
    if (!key) {
      if (!options.automatic) commit({ sourcesError: "The Architect's session is not known yet; wait until it is running." });
      return Promise.resolve();
    }
    if (options.automatic && !arch.ready) return Promise.resolve();
    const flightKey = `${key}:${arch.ready ? "ready" : "notReady"}`;
    if (sourcesFlight && sourcesFlight.scope === scope && sourcesFlight.key === flightKey && sourcesFlight.promise) return sourcesFlight.promise;
    const mine = scope;
    const flight: NonNullable<typeof sourcesFlight> = { scope: mine, key: flightKey, promise: null };
    sourcesFlight = flight;
    flight.promise = (async () => {
      commit({ sourcesLoading: true, sourcesError: null });
      try {
        const list = await api.listStructureSources(arch.id, arch.generation);
        if (sourcesFlight !== flight || !sourceCurrent(mine, arch)) return;
        if (list.sessionId !== arch.sessionId) commit({ sourcesError: "The Architect returned captured messages from another session; refresh the list.", sourcesLoaded: true });
        else applyList(list.sources, list.sessionId, key);
      } catch (error) {
        if (sourcesFlight === flight && sourceCurrent(mine, arch)) commit({ sourcesError: errorText(error), sourcesLoaded: true });
      } finally {
        // Only the read that still owns the loading state clears it; a superseded read must not hide the newer one's progress.
        if (sourcesFlight === flight) { sourcesFlight = null; commit({ sourcesLoading: false }); }
      }
    })();
    return flight.promise;
  }

  async function selectSource(id: string): Promise<void> {
    const row = state.sources.find(candidate => candidate.sourceId === id);
    if (!row) { commit({ sourcesError: `Unknown Markdown source ${id}; refresh the list.` }); return; }
    if (row.error) { commit({ sourcesError: `This message cannot be used: ${row.error}` }); return; }
    sourcePinned = true;
    commit({ selectedSourceId: id, previewId: id, sourcesError: null, errors: setKind(state.errors, "structureProposed", null) });
    reselect("structureProposed");
    if (!state.pages[id]?.loaded) await readSource(id);
  }

  async function useLastSubmitted(): Promise<void> {
    const api = backend;
    if (!api || !host || state.selectingLast) return;
    const arch = architect();
    if (!arch) { commit({ sourcesError: NEED_ARCHITECT }); return; }
    const key = architectKey(arch);
    if (!key) { commit({ sourcesError: "The Architect's session is not known yet; wait until it is running." }); return; }
    const mine = scope;
    commit({ selectingLast: true, sourcesError: null });
    try {
      const row = await api.selectLastStructureSource(arch.id, arch.generation);
      if (!sourceCurrent(mine, arch) || !key) return;
      const sameSession = state.sourcesKey === key;
      const rows = sameSession ? [row, ...state.sources.filter(other => other.sourceId !== row.sourceId)] : [row];
      const sources = rows.sort((a, b) => b.capturedAt - a.capturedAt);
      commit({ sources, sourcesKey: key, sourcesLoaded: true, sessionId: arch.sessionId, pages: sameSession ? state.pages : {} });
      await selectSource(row.sourceId);
    } catch (error) {
      if (sourceCurrent(mine, arch)) commit({ sourcesError: errorText(error) });
    } finally {
      if (mine === scope) commit({ selectingLast: false });
    }
  }

  // ----- generation (explicit user actions only) -----

  const baseRevision = (): ProjectInsightStatus | null => {
    const list = state.revisions.structureCurrent;
    const usable = (revision: ProjectInsightStatus | null): revision is ProjectInsightStatus =>
      !!revision && revision.state === "completed" && !!revision.snapshotId && revision.snapshotId === state.snapshotId && !supersededBy(list, revision);
    const chosen = selectedRevision(state, "structureCurrent");
    return usable(chosen) ? chosen : list.find(revision => usable(revision)) ?? null;
  };
  const lineageHead = (kind: StructureKind): ProjectInsightStatus | null => {
    const list = state.revisions[kind];
    return list.find(revision => revision.snapshotId === state.snapshotId
      && (kind === "structureCurrent" || (!!revision.sourceId && revision.sourceId === state.selectedSourceId))
      && !supersededBy(list, revision) && revision.state !== "failed") ?? null;
  };

  async function run(kind: StructureKind, regenerating: boolean): Promise<void> {
    if (!host || state.busy[kind]) return;
    const block = generateBlock(state, kind, architect());
    const snapshotId = state.snapshotId;
    if (block || !snapshotId) { commit({ errors: setKind(state.errors, kind, block ?? "No complete source index snapshot is selected yet.") }); return; }
    const proposed = kind === "structureProposed";
    const request: GenerateRequest = {
      kind, folder: state.folder, snapshotId, area: "structure",
      ...(proposed ? { sourceId: state.selectedSourceId, baseRequestId: baseRevision()?.requestId ?? null } : {}),
      supersedesRequestId: regenerating ? lineageHead(kind)?.requestId ?? null : null,
    };
    commit({ busy: setKind(state.busy, kind, true), errors: setKind(state.errors, kind, null), uncertain: setKind(state.uncertain, kind, false) });
    const mine = epoch;
    let outcome: GenerateOutcome;
    try { outcome = await host.generate(request); }
    catch (error) { outcome = { admitted: false, status: null, error: errorText(error), uncertain: false }; }
    if (mine !== epoch) return;
    if (outcome.status) merge(outcome.status);
    commit({
      busy: setKind(state.busy, kind, false), uncertain: setKind(state.uncertain, kind, outcome.uncertain),
      errors: setKind(state.errors, kind, outcome.error ?? (outcome.uncertain ? UNCERTAIN : outcome.admitted ? null : NOT_ADMITTED)),
    });
  }

  // ----- approval and discussion -----

  async function approve(): Promise<void> {
    const api = backend;
    if (!api || state.approving) return;
    const revision = selectedRevision(state, "structureProposed");
    const check = approveCheck(state);
    if (!check.ok || !revision?.resultHash || !revision.sourceHash) { commit({ approvalError: check.reason ?? "This preview cannot be approved." }); return; }
    const mine = scope;
    commit({ approving: true, approvalError: null });
    try {
      const updated = await api.approveStructurePreview(revision.requestId, revision.resultHash, revision.sourceHash);
      if (mine !== scope) return;
      if (updated.requestId === revision.requestId) merge(updated);
      commit({ approvalError: updated.requestId === revision.requestId && updated.approval ? null : "The backend did not return an approval record for this preview." });
    } catch (error) {
      if (mine === scope) commit({ approvalError: errorText(error) });
    } finally {
      if (mine === scope) commit({ approving: false });
    }
  }

  /** Replays the saved result into the existing Architect. No approval, no resend of an approval message. */
  async function discuss(requestId: string): Promise<boolean> {
    const arch = architect();
    if (!host) return false;
    if (!arch) { commit({ discussError: NEED_ARCHITECT }); return false; }
    commit({ discussError: null });
    try {
      const opened = await host.openArchitect(arch.id, requestId);
      if (!opened) commit({ discussError: "The Architect could not be opened." });
      return opened;
    } catch (error) {
      commit({ discussError: errorText(error) });
      return false;
    }
  }

  // ----- folder / snapshot -----

  function setFolder(folder: string, snapshotId: string | null): void {
    const sameFolder = folderKey(folder) === folderKey(state.folder);
    if (sameFolder && snapshotId === state.snapshotId) { if (folder !== state.folder) commit({ folder }); return; }
    epoch++;
    savedFlight = null;
    if (sameFolder) {
      commit({
        folder, snapshotId, savedLoading: false, busy: { structureCurrent: false, structureProposed: false },
        errors: { structureCurrent: null, structureProposed: null }, uncertain: { structureCurrent: false, structureProposed: false },
      });
      reselect("structureCurrent");
      reselect("structureProposed");
    } else {
      scope++;
      sourcePinned = false;
      sourcesFlight = null;
      pinned.structureCurrent = null;
      pinned.structureProposed = null;
      arrival.clear();
      state = initialState(folder, snapshotId);
      store.set(state);
    }
    if (folder) void loadSaved();
  }

  return {
    subscribe: store.subscribe,
    connect(nextHost, nextApi) { host = nextHost; backend = nextApi; if (state.folder) void loadSaved(); },
    setFolder,
    refreshSources,
    selectSource,
    readSource,
    useLastSubmitted,
    selectRevision,
    refreshRevision,
    loadSaved,
    generateCurrent: () => run("structureCurrent", false),
    generateProposed: () => run("structureProposed", false),
    regenerate: kind => run(kind, true),
    approve,
    discuss,
    receive: status => { merge(status); },
  };
}
