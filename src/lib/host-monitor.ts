import { writable, type Readable } from "svelte/store";
import type {
  api, HostExplainResult, HostMonitorContext, HostMonitorPage, HostMonitorQuery, HostOwnerKind, HostSnapshotSummary, HostTcpView, HostEndpoint, HostSocketRow, InsightProducer, Instance, TerminalOwner,
} from "./api";

// Controller of the per-agent "Outbound & Processes" tab. It owns one backend view at a time and samples the whole computer
// only while the pane is visible: the first sample is immediate, the next one starts at least SAMPLE_INTERVAL_MS after the
// previous one STARTED and only after it finished (no overlap, no catch-up). Hiding, a context change, a covering modal or
// unmounting cancels the timer, closes the view and fences every late result with an epoch.
//
// Display protocol (backend): the first collect's snapshot is displayed at once; every later collect returns a *candidate*.
// The controller loads the candidate's page through `read`, swaps summary + page together, and passes that id as
// `displayedSnapshotId` on the next collect, which acknowledges it. A failed candidate page load keeps the old display id.
// Paging, section switches, filters and search only `read` the displayed snapshot; they never collect.

export const SAMPLE_INTERVAL_MS = 2000;
export const PAGE_SIZE = 100;

export type HostMonitorTransport = Pick<typeof api,
  "openHostMonitor" | "collectHostMonitor" | "readHostMonitor" | "closeHostMonitor" | "explainHostMonitor" | "explainHostMonitorAfterSrsPause" | "cancelHostMonitorExplanation">;

export interface HostMonitorClock {
  now(): number;
  setTimeout(callback: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
}
export const realClock: HostMonitorClock = {
  now: () => Date.now(),
  setTimeout: (callback, ms) => setTimeout(callback, ms),
  clearTimeout: handle => clearTimeout(handle as number),
};

export type HostSection = "tcp" | "processes" | "udp";

/** Who asked: the Architect the human intends to be taken to, plus an optional extra guard owned by the caller (for example a navigation epoch). */
export interface ExplainOrigin { architectId: string | null; current?: () => boolean }

export type ExplainState =
  | { phase: "idle" }
  | { phase: "pending"; snapshotId: string }
  | { phase: "admitted"; snapshotId: string; requestId: string; focused: boolean }
  | { phase: "uncertain"; snapshotId: string; requestId: string | null; message: string }
  | { phase: "blocked"; snapshotId: string; requestId: string; jobId: string; attemptId: string; message: string }
  | { phase: "failed"; snapshotId: string; code: string | null; message: string };

export interface HostMonitorState {
  context: HostMonitorContext | null;
  visible: boolean;
  /** A collection request (or the open before it) is in flight. */
  collecting: boolean;
  /** The platform has no native collector: sampling stopped. */
  unsupported: boolean;
  /** The backend (or a pending explanation) holds the display: no sampling until Refresh finds it thawed. */
  frozen: boolean;
  /** The displayed snapshot's summary; the page below always belongs to the same snapshot. */
  summary: HostSnapshotSummary | null;
  page: HostMonitorPage | null;
  pageLoading: boolean;
  pageError: string | null;
  section: HostSection;
  tcpView: HostTcpView;
  text: string;
  /** Requested (and, once a page arrives, effective) offset. */
  offset: number;
  /** The latest collection failure. The last good snapshot stays shown and is labelled stale. */
  stale: { message: string; at: number } | null;
  explain: ExplainState;
}

export interface HostMonitorOptions {
  transport: HostMonitorTransport;
  clock?: HostMonitorClock;
  /** Focus-only: show the existing Architect. Never launches, restarts or replays it. */
  focusArchitect?: (producer: InsightProducer, current: () => boolean) => boolean | Promise<boolean>;
}

export interface HostMonitor {
  readonly state: Readable<HostMonitorState>;
  setContext(context: HostMonitorContext | null): void;
  setVisible(visible: boolean): void;
  showSection(section: HostSection, tcpView?: HostTcpView): void;
  setTcpView(view: HostTcpView): void;
  setText(text: string): void;
  loadPage(offset: number): void;
  /** One extra sample on request, still honouring the minimum start interval; also retries after a failure or a freeze. */
  refresh(): void;
  explainDisplayed(origin: ExplainOrigin): Promise<void>;
  continueAfterSrsPause(): Promise<void>;
  /** Drops a failure notice or a blocked continuation (the latter calls cancelHostMonitorExplanation and thaws). */
  dismissExplanation(): void;
  dispose(): void;
}

// ---------- Pure helpers ----------

export const errorText = (error: unknown): string => typeof error === "string" ? error : error instanceof Error ? error.message : String(error);
/** The backend's bare code ("monitor_frozen" or "monitor_frozen: text"), or null for a free-text native message. */
export function errorCode(error: unknown): string | null {
  const match = /^\s*([a-z][a-z0-9_]*)\s*(?::|$)/.exec(errorText(error));
  return match ? match[1] : null;
}

const pathKey = (path: string): string => path.replace(/^\\\\\?\\/, "").replace(/\\/g, "/").replace(/\/+$/, "").toLowerCase();
export function sameContext(a: HostMonitorContext | null, b: HostMonitorContext | null): boolean {
  if (a === null || b === null) return a === b;
  return a.instanceId === b.instanceId && a.generation === b.generation && pathKey(a.projectPath) === pathKey(b.projectPath);
}

export type HostMetric = "processes" | "external" | "confirmedOutbound" | "unknownDirection" | "udp" | "limited";
const METRIC_FIELDS: Record<HostMetric, { value: keyof HostSnapshotSummary; sections: readonly ("processes" | "tcp4" | "tcp6" | "udp4" | "udp6")[] }> = {
  processes: { value: "processCount", sections: ["processes"] },
  limited: { value: "limitedProcessCount", sections: ["processes"] },
  external: { value: "externalTcpCount", sections: ["tcp4", "tcp6"] },
  confirmedOutbound: { value: "confirmedOutboundCount", sections: ["tcp4", "tcp6"] },
  unknownDirection: { value: "unknownDirectionCount", sections: ["tcp4", "tcp6"] },
  udp: { value: "udpEndpointCount", sections: ["udp4", "udp6"] },
};
/** "unavailable" for a null count, "at least N (partial)" when any contributing section is not fully read, else the number. */
export function metricLabel(summary: HostSnapshotSummary, metric: HostMetric): string {
  const field = METRIC_FIELDS[metric];
  const count = summary[field.value] as number | null;
  if (count === null) return "unavailable";
  return field.sections.some(name => summary.sections[name].status !== "ok") ? `at least ${count} (partial)` : String(count);
}
/** Section problems worth showing the human, as "IPv4 TCP: message" lines. */
export function sectionNotes(summary: HostSnapshotSummary): string[] {
  const names: Record<keyof HostSnapshotSummary["sections"], string> = { processes: "Processes", tcp4: "IPv4 TCP", tcp6: "IPv6 TCP", udp4: "IPv4 UDP", udp6: "IPv6 UDP" };
  return (Object.keys(names) as (keyof typeof names)[]).flatMap(key => {
    const section = summary.sections[key];
    if (section.status === "ok") return [];
    return [`${names[key]} ${section.status === "partial" ? "partially read" : "unavailable"}${section.error ? `: ${section.error}` : ""}`];
  });
}

/** The address column: IPv6 scope ids are kept as `%id`. */
export const formatAddress = (endpoint: HostEndpoint): string => endpoint.scopeId ? `${endpoint.address}%${endpoint.scopeId}` : endpoint.address;
export function formatEndpoint(endpoint: HostEndpoint | null): string {
  if (!endpoint) return "—";
  const scoped = formatAddress(endpoint);
  return endpoint.address.includes(":") ? `[${scoped}]:${endpoint.port}` : `${scoped}:${endpoint.port}`;
}
export function directionLabel(row: Pick<HostSocketRow, "direction" | "directionEvidence" | "protocol">): string {
  if (row.protocol === "udp") return "UDP: no peer or direction in the OS table";
  switch (row.direction) {
    case "outbound": return "Outbound attempt seen (SYN sent)";
    case "inbound": return "Inbound handshake seen (SYN received)";
    case "listener": return "Listening";
    default: return "Direction unknown (established or closing)";
  }
}
export function ownerLabel(owner: { instanceId: string; kind: HostOwnerKind }, ownInstanceId: string | null): string {
  const kind = { agentRoot: "Agent process", descendant: "Agent child", browserRoot: "Agent browser", browserDescendant: "Browser child" }[owner.kind];
  return owner.instanceId === ownInstanceId ? kind : `${kind} (other agent)`;
}

// ---------- Controller ----------

const FENCED = Symbol("fenced");
const RELOAD_TRIES = 5;

const initialState = (): HostMonitorState => ({
  context: null, visible: false, collecting: false, unsupported: false, frozen: false, summary: null, page: null, pageLoading: false, pageError: null,
  section: "tcp", tcpView: "external", text: "", offset: 0, stale: null, explain: { phase: "idle" },
});

/** A receipt may arrive after the Architect was restarted or switched sessions. Never focus its replacement. */
export function monitorProducerCurrent(
  producer: InsightProducer,
  architect: Pick<Instance, "id" | "generation" | "status" | "bridgeReady"> | null | undefined,
  owner: Pick<TerminalOwner, "generation" | "sessionId"> | null | undefined,
): boolean {
  return !!architect && architect.id === producer.instanceId && architect.generation === producer.generation
    && architect.status === "running" && architect.bridgeReady
    && owner?.generation === producer.generation && owner.sessionId === producer.sessionId;
}

export function createHostMonitor(options: HostMonitorOptions): HostMonitor {
  const { transport, focusArchitect } = options;
  const clock = options.clock ?? realClock;
  const store = writable<HostMonitorState>(initialState());
  const read = (): HostMonitorState => { let value!: HostMonitorState; store.subscribe(next => { value = next; })(); return value; };
  const patch = (change: Partial<HostMonitorState>): void => store.update(state => ({ ...state, ...change }));

  let context: HostMonitorContext | null = null;
  let visible = false;
  let disposed = false;
  let epoch = 0;
  let queryVersion = 0;
  let viewId: string | null = null;
  /** The last DISPLAYED snapshot id: acknowledged on every collect. Null until a snapshot of this view is displayed. */
  let displayedId: string | null = null;
  let collecting = false;
  let frozen = false;
  let unsupported = false;
  let timer: unknown = null;
  let lastStart = Number.NEGATIVE_INFINITY;
  let blockedOrigin: ExplainOrigin | null = null;

  const clearTimer = (): void => { if (timer !== null) { clock.clearTimeout(timer); timer = null; } };
  const quietly = (call: () => Promise<unknown>): void => { try { void call().catch(() => {}); } catch { /* the backend closes idempotently */ } };
  const closeView = (id: string): void => quietly(() => transport.closeHostMonitor(id));
  const delay = (): number => Math.max(0, lastStart + SAMPLE_INTERVAL_MS - clock.now());
  const current = (token: number): boolean => token === epoch && !disposed;

  function query(): HostMonitorQuery {
    const { section, tcpView, text, offset } = read();
    const common = { text, offset, limit: PAGE_SIZE };
    return section === "processes" ? { kind: "processes", ...common } : section === "udp" ? { kind: "udp", ...common } : { kind: "tcp", view: tcpView, ...common };
  }

  /** Reads the current query from one snapshot; retried if the human changed the query while it loaded. */
  async function pageFor(snapshotId: string, token: number): Promise<HostMonitorPage> {
    for (let tries = 0; tries < RELOAD_TRIES; tries++) {
      const version = queryVersion;
      const page = await transport.readHostMonitor(snapshotId, query());
      if (!current(token)) throw FENCED;
      if (version === queryVersion) return page;
    }
    throw new Error("The view kept changing while the snapshot loaded.");
  }

  async function reload(): Promise<void> {
    const version = ++queryVersion;
    const token = epoch;
    const id = displayedId;
    patch({ pageLoading: id !== null, pageError: null });
    if (id === null) return;
    try {
      const page = await transport.readHostMonitor(id, query());
      if (!current(token) || version !== queryVersion || id !== displayedId) return;
      patch({ page, offset: page.offset, pageLoading: false, pageError: null });
    } catch (error) {
      if (!current(token) || version !== queryVersion) return;
      patch({ pageLoading: false, pageError: errorText(error) });
    }
  }

  function schedule(token: number, wait: number): void {
    clearTimer();
    if (!current(token)) return;
    if (wait <= 0) { void cycle(token); return; }
    timer = clock.setTimeout(() => { timer = null; void cycle(token); }, wait);
  }

  async function ensureView(token: number): Promise<string> {
    if (viewId) return viewId;
    const wanted = context;
    if (!wanted) throw FENCED;
    const opened = await transport.openHostMonitor(wanted.instanceId, wanted.generation);
    if (!current(token)) { closeView(opened.viewId); throw FENCED; }
    if (!sameContext(opened.context, wanted)) {
      closeView(opened.viewId);
      throw new Error("The monitor opened for a different agent or folder than this pane; it was closed.");
    }
    viewId = opened.viewId;
    return opened.viewId;
  }

  async function attempt(token: number): Promise<void> {
    const id = await ensureView(token);
    const summary = await transport.collectHostMonitor(id, displayedId);
    if (!current(token) || frozen) throw FENCED;
    if (summary.viewId !== id || !context || summary.instanceId !== context.instanceId || summary.generation !== context.generation) {
      throw new Error("The snapshot belongs to a different monitor context and was discarded.");
    }
    // The first snapshot of a view is its display already; a later one is only a candidate until its page is loaded.
    const first = displayedId === null;
    let page: HostMonitorPage | null = null;
    let pageError: string | null = null;
    if (first) {
      try { page = await pageFor(summary.snapshotId, token); }
      catch (error) { if (error === FENCED) throw error; pageError = errorText(error); }
    } else page = await pageFor(summary.snapshotId, token);
    if (!current(token) || frozen) throw FENCED;
    displayedId = summary.snapshotId;
    const explain = read().explain;
    patch({
      summary, page, pageError, pageLoading: false, stale: null, offset: page ? page.offset : read().offset,
      explain: explain.phase === "admitted" || explain.phase === "uncertain" ? { phase: "idle" } : explain,
    });
  }

  /** Returns true when sampling should continue on the normal cadence. */
  function fail(error: unknown): boolean {
    const code = errorCode(error);
    const message = errorText(error);
    const label = (): void => patch({ stale: { message, at: clock.now() } });
    switch (code) {
      case "monitor_cancelled": case "collection_in_progress": return true;
      case "monitor_frozen": frozen = true; patch({ frozen: true }); return false;
      case "view_closed": viewId = null; displayedId = null; return true;
      case "snapshot_expired": case "snapshot_not_displayed": displayedId = null; label(); return true;
      case "unsupported_platform": {
        unsupported = true;
        if (viewId) { closeView(viewId); viewId = null; }
        displayedId = null;
        patch({ unsupported: true, stale: null });
        return false;
      }
      case "monitor_closed": case "unknown_instance": case "generation_mismatch": label(); return false;
      default: label(); return true;
    }
  }

  async function cycle(token: number): Promise<void> {
    if (!current(token) || collecting || !context || !visible || frozen || unsupported) return;
    collecting = true;
    lastStart = clock.now();
    patch({ collecting: true });
    let again = true;
    try { await attempt(token); }
    catch (error) { if (error !== FENCED && current(token)) again = fail(error); }
    finally {
      if (current(token)) {
        collecting = false;
        patch({ collecting: false });
        if (again && !frozen && !unsupported) schedule(token, delay());
      }
    }
  }

  function start(): void {
    if (disposed || !visible || !context) return;
    schedule(epoch, 0);
  }

  /** Stops every timer and request of this epoch: late results are dropped and the backend view is closed. */
  function cancelAll(): void {
    epoch++;
    queryVersion++;
    clearTimer();
    const id = viewId;
    viewId = null;
    displayedId = null;
    collecting = false;
    frozen = false;
    unsupported = false;
    lastStart = Number.NEGATIVE_INFINITY;
    const explain = read().explain;
    if (explain.phase === "blocked") quietly(() => transport.cancelHostMonitorExplanation(explain.snapshotId));
    blockedOrigin = null;
    patch({ collecting: false, frozen: false, unsupported: false, pageLoading: false, explain: { phase: "idle" } });
    if (id) closeView(id);
  }

  // ----- Explanation -----

  function resume(token: number): void {
    frozen = false;
    patch({ frozen: false });
    if (!collecting && current(token)) schedule(token, delay());
  }

  async function runExplain(origin: ExplainOrigin, snapshotId: string, call: () => Promise<HostExplainResult>): Promise<void> {
    const token = epoch;
    const live = (): boolean => current(token) && (origin.current ? origin.current() : true);
    patch({ explain: { phase: "pending", snapshotId } });
    let result: HostExplainResult;
    try { result = await call(); }
    catch (error) {
      // A rejected command leaves delivery unknown: keep the display frozen and never resend on our own.
      if (current(token)) patch({ explain: { phase: "uncertain", snapshotId, requestId: null, message: errorText(error) } });
      return;
    }
    const mine = live();
    if (result.promptSent === true) {
      if (!current(token)) return;
      let focused = false;
      if (mine && origin.architectId && result.producer?.instanceId === origin.architectId && focusArchitect) {
        focused = await focusArchitect(result.producer, live);
      }
      if (current(token)) patch({ explain: { phase: "admitted", snapshotId, requestId: result.requestId, focused } });
      return;
    }
    const failure = result.error ?? { code: "not_sent", message: "The Architect did not accept the request." };
    if (result.promptSent === null) {
      if (current(token)) patch({ explain: { phase: "uncertain", snapshotId, requestId: result.requestId, message: failure.message } });
      return;
    }
    if (result.blockedBy) {
      if (!mine) { quietly(() => transport.cancelHostMonitorExplanation(snapshotId)); return; }
      blockedOrigin = origin;
      patch({ explain: { phase: "blocked", snapshotId, requestId: result.requestId, jobId: result.blockedBy.jobId, attemptId: result.blockedBy.attemptId, message: failure.message } });
      return;
    }
    // Provably unsent: every pin is already released and the backend thawed the display.
    if (!current(token)) return;
    patch({ explain: { phase: "failed", snapshotId, code: failure.code, message: failure.message } });
    resume(token);
  }

  return {
    state: { subscribe: store.subscribe },

    setContext(next) {
      if (disposed || sameContext(context, next)) return;
      cancelAll();
      context = next;
      patch({ ...initialState(), context: next, visible });
      start();
    },

    setVisible(next) {
      if (disposed || visible === next) return;
      visible = next;
      patch({ visible: next });
      if (next) start(); else cancelAll();
    },

    showSection(section, tcpView) {
      patch({ section, tcpView: tcpView ?? read().tcpView, text: "", offset: 0 });
      void reload();
    },
    setTcpView(view) { patch({ section: "tcp", tcpView: view, offset: 0 }); void reload(); },
    setText(text) { if (text === read().text) return; patch({ text, offset: 0 }); void reload(); },
    loadPage(offset) { patch({ offset: Math.max(0, Math.floor(offset)) }); void reload(); },

    refresh() {
      if (disposed || !visible || !context || collecting) return;
      const phase = read().explain.phase;
      if (phase === "pending" || phase === "blocked") return;
      frozen = false;
      unsupported = false;
      patch({ frozen: false, unsupported: false });
      schedule(epoch, delay());
    },

    async explainDisplayed(origin) {
      const state = read();
      const snapshotId = displayedId;
      if (disposed || !visible || !snapshotId || !state.summary || state.summary.snapshotId !== snapshotId) return;
      if (state.explain.phase !== "idle" && state.explain.phase !== "failed") return;
      // Freeze locally first so no candidate can replace the snapshot being explained.
      frozen = true;
      clearTimer();
      patch({ frozen: true });
      await runExplain(origin, snapshotId, () => transport.explainHostMonitor(snapshotId));
    },

    async continueAfterSrsPause() {
      const state = read().explain;
      const origin = blockedOrigin;
      if (disposed || state.phase !== "blocked" || !origin) return;
      blockedOrigin = null;
      await runExplain(origin, state.snapshotId, () => transport.explainHostMonitorAfterSrsPause(state.snapshotId, state.jobId, state.attemptId));
    },

    dismissExplanation() {
      const state = read().explain;
      if (state.phase === "blocked") {
        blockedOrigin = null;
        quietly(() => transport.cancelHostMonitorExplanation(state.snapshotId));
        patch({ explain: { phase: "idle" } });
        resume(epoch);
      } else if (state.phase === "failed") patch({ explain: { phase: "idle" } });
    },

    dispose() {
      if (disposed) return;
      cancelAll();
      disposed = true;
    },
  };
}
