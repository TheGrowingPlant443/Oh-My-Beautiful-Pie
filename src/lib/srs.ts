import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { writable, type Readable } from "svelte/store";
import type { VaultMode } from "./knowledge";

export type SrsPhase = "survey" | "specify" | "gapReview" | "verify";
export type SrsState = "inventory" | "running" | "waitingForHuman" | "paused" | "interrupted" | "uncertain" | "reviewing" | "completed" | "cancelled";
export type ResumeMode = "sameConversation" | "currentConversationFromCheckpoints";
export type SrsSection = "stages" | "work" | "questions" | "documents" | "coverage" | "activity" | "receipts";
export interface SrsJob {
  jobId: string; folder: string; repositoryRoot: string; revision: number; state: SrsState; phase: SrsPhase | null;
  vaultId: string; vaultMode: VaultMode; vaultAvailable: boolean; outputPath: string; masterPath: string;
  template: { revision: string }; scopeHash: string; sourceHash: string | null; attemptId: string | null; stageId: string | null;
  stage: { stageId: string; number: number; phase: SrsPhase; state: "pending" | "queued" | "active" | "committed" } | null;
  producer: { instanceId: string; generation: number; sessionId: string } | null;
  counts: { areas: number; evidenceFiles: number; work: number; workOpen: number; documents: number; requirements: number; questions: number; questionsOpen: number; blocked: number; excluded: number };
  checkpoint: { revision: number; at: number; summary: string } | null; error: { code: string; message: string } | null;
  usage: { tokens: number | null; costUsd: number | null }; createdAt: number; updatedAt: number;
  completion: { manifestHash: string; scopeHash: string; at: number; label: string } | null;
  resumeRequired: "none" | ResumeMode; humanExclusions: number; architectModel: string | null;
}
export interface ScopePreparation {
  token: string; expiresAt: number; folder: string; repositoryRoot: string; vaultId: string; vaultMode: VaultMode; vaultRoot: string;
  roots: { path: string; kind: "repository" | "evidence"; rationale: string }[];
  exclusions: { path: string; reason: string; source: "app" | "human" }[];
  template: { revision: string; files: { name: string; sha256: string }[] }; warnings: string[];
}
export interface ResumePreparation { token: string; expiresAt: number; jobId: string; mode: ResumeMode; summary: string; warnings: string[] }
export interface SrsPage<T> { jobId: string; revision: number; section: SrsSection; total: number; offset: number; items: T[]; nextOffset: number | null }
export interface SrsJobPage { items: SrsJob[]; total: number; offset: number; nextOffset: number | null }
export interface SrsQuestion { questionId: string; prompt: string; options: { label: string; description?: string }[]; multi: boolean; required: boolean; state: "open" | "asked" | "answered" | "excluded"; answerReceiptId: string | null; live: boolean; workIds: string[] }
export interface SrsAnswer { selectedOptions: string[]; customInput?: string; note?: string }
export interface SrsDocument { documentId: string; type: string; title: string; path: string | null; state: "draft" | "committed"; revisionId: string; hash: string | null; bytes: number; requirementId: string | null; specificity: string | null }
export interface SrsWork { id: string; parentId: string | null; phase: SrsPhase; kind: string; status: string; evidenceIds: string[]; requirementIds: string[]; questionIds: string[]; rationale: string }
export interface SrsDocuments { vaultId: string; vaultMode: VaultMode; path: string; completed: boolean; draft: boolean }

export const srsApi = {
  prepareScope: (folder: string, vaultId: string, evidenceFolders: string[], exclusions: string[]) => invoke<ScopePreparation>("prepare_srs_scope", { folder, vaultId, evidenceFolders, exclusions }),
  start: (scopeToken: string) => invoke<SrsJob>("start_srs", { scopeToken }),
  get: (jobId: string) => invoke<SrsJob>("get_srs_job", { jobId }),
  list: (folder: string, offset: number, limit: number) => invoke<SrsJobPage>("list_srs_jobs", { folder, offset, limit }),
  pause: (jobId: string) => invoke<SrsJob>("pause_srs", { jobId }),
  prepareResume: (jobId: string, mode: ResumeMode) => invoke<ResumePreparation>("prepare_srs_resume", { jobId, mode }),
  resume: (token: string) => invoke<SrsJob>("resume_srs", { token }),
  cancel: (jobId: string) => invoke<SrsJob>("cancel_srs", { jobId }),
  dispose: (jobId: string, expectedRevision: number, itemId: string, disposition: "excludeFromScope" | "provideHumanAnswer", rationale: string, answer?: SrsAnswer) => invoke<SrsJob>("dispose_srs_item", { jobId, expectedRevision, itemId, disposition, rationale, answer }),
  query: <T>(jobId: string, section: SrsSection, offset: number, limit: number) => invoke<SrsPage<T>>("query_srs_job", { jobId, section, offset, limit }),
  openDocuments: (jobId: string) => invoke<SrsDocuments>("open_srs_documents", { jobId }),
  rebindVault: (jobId: string, vaultId: string) => invoke<SrsJob>("rebind_srs_vault", { jobId, vaultId }),
};
export type SrsApi = typeof srsApi;
export const srsTransport = { listen: (receive: (job: SrsJob) => void) => listen<SrsJob>("srs_job", event => receive(event.payload)) };
export const activeSrs = (job: SrsJob): boolean => ["inventory", "running", "waitingForHuman", "reviewing"].includes(job.state);
export const resumableSrs = (job: SrsJob): boolean => ["paused", "interrupted", "uncertain"].includes(job.state);
/** Completion is the native frozen manifest, never prose or a finished agent turn. */
export const completedSrs = (job: SrsJob): boolean => job.state === "completed" && !!job.completion?.manifestHash && job.completion.scopeHash === job.scopeHash;
const SRS_STATE_LABELS: Partial<Record<SrsState, string>> = {
  inventory: "Recording evidence inventory", running: "Running", waitingForHuman: "Waiting for you", paused: "Paused", interrupted: "Interrupted — needs attention",
  uncertain: "Delivery uncertain — inspect before resuming", reviewing: "Reviewing", cancelled: "Cancelled",
};
export function srsStateLabel(job: SrsJob): string {
  if (completedSrs(job)) return "SRS complete for the recorded evidence scope";
  if (job.state === "completed") return "Completion manifest unavailable — recheck";
  return SRS_STATE_LABELS[job.state] ?? job.state;
}
export const srsPhaseLabel = (phase: SrsPhase | null): string => phase ? ({ survey: "Survey", specify: "Specify requirements", gapReview: "Gap review", verify: "Verification" })[phase] : "Inventory";
export function srsDocumentLabel(job: SrsJob): string {
  if (!completedSrs(job)) return "View draft documents";
  return job.vaultMode === "obsidian" ? "Obsidian view" : "Documents view";
}
export const srsModelLabel = (model: string | null | undefined): string => model?.trim() ? `Architect model: ${model} — locality and provider charges are not inferred from its name` : "Model resolved by OMP; locality/cost unknown";

export interface SrsTicket { jobId: string; attemptId: string | null; folder: string; scopeHash: string; producer: SrsJob["producer"]; navigationEpoch: number; eligible: boolean; lateResult: boolean; settled: boolean }
export interface SrsHooks {
  architectId(): string | null; openArchitect(id: string): Promise<boolean>;
  following(ticket: SrsTicket): boolean; quiescent(ticket: SrsTicket): boolean; returnToDocumentation(ticket: SrsTicket): void;
}
export interface SrsClock { setTimeout(callback: () => void, ms: number): number; clearTimeout(timer: number): void }
export interface SrsStoreState { jobs: ReadonlyMap<string, SrsJob>; observationErrors: ReadonlyMap<string, string>; connectionError: string | null; connected: boolean; tickets: SrsTicket[]; navigationEpoch: number; actionError: string | null }
export interface SrsStore extends Readable<SrsStoreState> {
  attach(hooks: SrsHooks): void; connect(transport?: typeof srsTransport): Promise<() => void>;
  receive(job: SrsJob): void; read(jobId: string): Promise<SrsJob>; list(folder: string, offset?: number): Promise<SrsJobPage>;
  observe(jobId: string): () => void; reconcile(): Promise<void>;
  start(token: string): Promise<SrsJob>; resume(token: string): Promise<SrsJob>; pause(jobId: string): Promise<SrsJob>; cancel(jobId: string): Promise<SrsJob>;
  dispose(jobId: string, expectedRevision: number, itemId: string, disposition: "excludeFromScope" | "provideHumanAnswer", rationale: string, answer?: SrsAnswer): Promise<SrsJob>;
  rebindVault(jobId: string, vaultId: string): Promise<SrsJob>; noteNavigation(): void; invalidateAutoReturn(): void; openResult(jobId: string): boolean; disposeStore(): void;
}
const errorText = (error: unknown): string => error instanceof Error ? error.message : String(error);
const systemClock: SrsClock = { setTimeout: (fn, ms) => globalThis.setTimeout(fn, ms) as unknown as number, clearTimeout: timer => globalThis.clearTimeout(timer) };

/** Parent-lived, passive observation. No total watch deadline; storage/provider pauses and failed reads are not completion. */
export function createSrsStore(api: SrsApi = srsApi, clock: SrsClock = systemClock): SrsStore {
  let state: SrsStoreState = { jobs: new Map(), observationErrors: new Map(), connectionError: null, connected: false, tickets: [], navigationEpoch: 0, actionError: null };
  const store = writable(state);
  let hooks: SrsHooks | null = null;
  let timer: number | null = null;
  let stopped = false, connecting: Promise<() => void> | null = null, unlisten: (() => void) | null = null;
  const observers = new Map<string, number>();
  const pending = new Map<string, Promise<SrsJob>>();
  const patch = (change: Partial<SrsStoreState>): void => { state = { ...state, ...change }; store.set(state); };
  function checkReturn(): void {
    for (const ticket of state.tickets) {
      const job = state.jobs.get(ticket.jobId);
      if (!job || !completedSrs(job) || ticket.settled) continue;
      const same = job.attemptId === ticket.attemptId && job.scopeHash === ticket.scopeHash && !!job.producer && !!ticket.producer &&
        job.producer.instanceId === ticket.producer.instanceId && job.producer.generation === ticket.producer.generation && job.producer.sessionId === ticket.producer.sessionId;
      if (!ticket.eligible || !same || ticket.navigationEpoch !== state.navigationEpoch || !hooks?.following(ticket)) {
        patch({ tickets: state.tickets.map(item => item === ticket ? { ...item, eligible: false, lateResult: true, settled: true } : item) });
      } else if (hooks.quiescent(ticket)) {
        patch({ tickets: state.tickets.map(item => item === ticket ? { ...item, eligible: false, lateResult: false, settled: true } : item) });
        hooks.returnToDocumentation(ticket);
      }
    }
  }
  function schedule(): void {
    if (timer !== null || stopped) return;
    const ids = watched();
    if (!ids.length && !state.tickets.some(ticket => ticket.eligible)) return;
    timer = clock.setTimeout(() => { timer = null; void reconcile().finally(schedule); }, 3000);
  }
  function watched(): string[] {
    return [...new Set([...observers.keys(), ...state.tickets.filter(ticket => !ticket.settled && ticket.eligible).map(ticket => ticket.jobId), ...[...state.jobs.values()].filter(activeSrs).map(job => job.jobId)])];
  }
  function receive(job: SrsJob): void {
    const old = state.jobs.get(job.jobId);
    if (old && job.revision <= old.revision) return;
    const errors = new Map(state.observationErrors); errors.delete(job.jobId);
    patch({ jobs: new Map(state.jobs).set(job.jobId, job), observationErrors: errors });
    checkReturn(); schedule();
  }
  async function read(jobId: string): Promise<SrsJob> {
    const inflight = pending.get(jobId); if (inflight) return inflight;
    const request = api.get(jobId).then(job => {
      receive(job);
      const errors = new Map(state.observationErrors); errors.delete(jobId); patch({ observationErrors: errors });
      return state.jobs.get(jobId) ?? job;
    }, error => { patch({ observationErrors: new Map(state.observationErrors).set(jobId, errorText(error)) }); throw error; }).finally(() => pending.delete(jobId));
    pending.set(jobId, request); return request;
  }
  async function reconcile(): Promise<void> {
    await Promise.all(watched().map(id => read(id).catch(() => undefined)));
    checkReturn(); schedule();
  }
  async function action(run: () => Promise<SrsJob>): Promise<SrsJob> {
    patch({ actionError: null });
    try { const job = await run(); receive(job); return state.jobs.get(job.jobId) ?? job; }
    catch (error) { patch({ actionError: errorText(error) }); throw error; }
  }
  async function admit(run: () => Promise<SrsJob>): Promise<SrsJob> {
    const epoch = state.navigationEpoch;
    invalidateAutoReturn();
    const job = await action(run);
    if (completedSrs(job)) return job;
    const ticket: SrsTicket = { jobId: job.jobId, folder: job.folder, scopeHash: job.scopeHash, attemptId: job.attemptId, producer: job.producer, navigationEpoch: epoch, eligible: epoch === state.navigationEpoch, lateResult: false, settled: false };
    patch({ tickets: [...state.tickets.filter(item => item.jobId !== job.jobId), ticket] });
    // Admission may outlive the page that started it. Keep observing, but never steal focus after a human has navigated.
    if (epoch !== state.navigationEpoch) { schedule(); return job; }
    const id = job.producer?.instanceId ?? hooks?.architectId();
    if (id && hooks) {
      try { if (!await hooks.openArchitect(id)) patch({ actionError: "The job is saved, but the existing Architect could not be focused. Open it manually." }); }
      catch (error) { patch({ actionError: `The job is saved; Architect focus failed: ${errorText(error)}` }); }
    } else patch({ actionError: "The job is saved; open the existing Architect to follow its work." });
    schedule(); return job;
  }
  function invalidateAutoReturn(): void { patch({ tickets: state.tickets.map(ticket => ({ ...ticket, eligible: false })) }); }
  return {
    subscribe: store.subscribe, attach(next) { hooks = next; }, receive, read,
    async connect(transport = srsTransport) {
      if (connecting) return connecting;
      stopped = false;
      // Listener is established BEFORE any initial get/list: events cannot be lost between subscribing and reading.
      connecting = transport.listen(receive).then(stop => {
        unlisten = stop; patch({ connected: true, connectionError: null });
        void reconcile();
        return () => { stop(); unlisten = null; connecting = null; patch({ connected: false }); };
      }).catch(error => { connecting = null; patch({ connected: false, connectionError: errorText(error) }); schedule(); throw error; });
      return connecting;
    },
    async list(folder, offset = 0) { const page = await api.list(folder, offset, 100); page.items.forEach(receive); return { ...page, items: page.items.map(job => state.jobs.get(job.jobId) ?? job) }; },
    observe(jobId) { observers.set(jobId, (observers.get(jobId) ?? 0) + 1); void read(jobId).catch(() => undefined); schedule(); return () => { const n = (observers.get(jobId) ?? 1) - 1; if (n) observers.set(jobId, n); else observers.delete(jobId); }; },
    reconcile, start: token => admit(() => api.start(token)), resume: token => admit(() => api.resume(token)),
    pause: jobId => action(() => api.pause(jobId)), cancel: jobId => action(() => api.cancel(jobId)),
    dispose: (jobId, revision, id, disposition, rationale, answer) => action(() => api.dispose(jobId, revision, id, disposition, rationale, answer)),
    rebindVault: (jobId, vaultId) => action(() => api.rebindVault(jobId, vaultId)),
    noteNavigation() { patch({ navigationEpoch: state.navigationEpoch + 1 }); invalidateAutoReturn(); }, invalidateAutoReturn,
    openResult(jobId) {
      const ticket = state.tickets.find(item => item.jobId === jobId);
      const job = state.jobs.get(jobId);
      if (!ticket || !job || !completedSrs(job) || !hooks) return false;
      patch({ tickets: state.tickets.map(item => item === ticket ? { ...item, eligible: false, lateResult: false, settled: true } : item) });
      hooks.returnToDocumentation(ticket);
      return true;
    },
    disposeStore() {
      stopped = true;
      if (timer !== null) clock.clearTimeout(timer);
      timer = null;
      unlisten?.();
      unlisten = null;
      connecting = null;
      observers.clear();
    },
  };
}
export const srs = createSrsStore();
