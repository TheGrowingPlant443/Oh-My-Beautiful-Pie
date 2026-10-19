import { DependencyJobSession, type JobSnapshot } from "./package-insights";
import { toAdmission, toJobState, type InsightJobState } from "./project-insight";
import type { InsightProducer, ProjectInsightStatus, VisualizationKind } from "./api";
import type { GenerateOutcome, GenerateRequest } from "./overview-types";

export type TicketState = "admitting" | "waiting" | "uncertain" | "completed" | "failed" | "timedOut" | "unreadable";

/** One explicit user generation target. It lives in the parent, so it survives the System overview page being unmounted. */
export interface VisualizationTicket {
  key: string;
  requestId: string | null;
  folder: string;
  snapshotId: string;
  kind: VisualizationKind;
  sourceId: string | null;
  producer: InsightProducer | null;
  /** Where the validated result is shown: the Helicopter or System tab, or the repository-structure area. */
  destinationTab: "helicopter" | "system" | "structure";
  /** The revision this ticket produced (the admitted request); a different, newer revision is never substituted. */
  selectedRevisionId: string | null;
  navigationEpoch: number;
  autoReturnEligible: boolean;
  state: TicketState;
  message: string | null;
  startedAt: number;
  /** The typed result arrived while the user was elsewhere: offer it instead of stealing the screen. */
  lateResult: boolean;
}

export interface TicketClock {
  setTimeout(callback: () => void, milliseconds: number): number;
  clearTimeout(timer: number): void;
  now(): number;
}

export interface TicketHooks {
  api: {
    requestSystemVisualization(folder: string, kind: VisualizationKind, snapshotId: string, sourceId: string | null, baseRequestId: string | null, supersedesRequestId: string | null): Promise<ProjectInsightStatus>;
    readProjectInsight(requestId: string): Promise<ProjectInsightStatus>;
  };
  /** The existing Architect, when the admitted request has no producer yet. */
  architectId(): string | null;
  openArchitect(id: string): Promise<boolean>;
  /** Monotonic counter bumped by every user navigation. */
  epoch(): number;
  /** True while the user is still looking at the Architect this ticket opened. */
  following(ticket: VisualizationTicket): boolean;
  /** True when that Architect's current turn finished and nothing is queued (a presentation guard, never the result authority). */
  quiescent(ticket: VisualizationTicket): boolean;
  returnToOverview(ticket: VisualizationTicket): void;
  /** Every passively read status (any state) of a request this book watches. */
  received(status: ProjectInsightStatus): void;
  changed(tickets: readonly VisualizationTicket[]): void;
  clock: TicketClock;
}

const RETURN_POLL_MS = 400;
const RETURN_WAIT_MS = 30_000;

export const ticketKey = (request: Pick<GenerateRequest, "kind" | "folder" | "snapshotId" | "sourceId">, folderKey: string): string =>
  JSON.stringify([request.kind, folderKey, request.snapshotId, request.sourceId ?? null]);

const errorText = (error: unknown): string => error instanceof Error ? error.message : String(error);

export class TicketBook {
  private readonly tickets = new Map<string, VisualizationTicket>();
  private readonly sessions = new Map<string, DependencyJobSession<InsightJobState>>();
  private readonly admissions = new Map<string, Promise<GenerateOutcome>>();
  private readonly returns = new Map<string, number>();
  private readonly hooks: TicketHooks;
  private readonly folderKey: (folder: string) => string;
  constructor(hooks: TicketHooks, folderKey: (folder: string) => string) { this.hooks = hooks; this.folderKey = folderKey; }

  list(): VisualizationTicket[] { return [...this.tickets.values()]; }
  get(key: string): VisualizationTicket | undefined { return this.tickets.get(key); }
  private publish(): void { this.hooks.changed(this.list()); }
  private patch(ticket: VisualizationTicket, change: Partial<VisualizationTicket>): void {
    // Tickets are replaced by key; a stale ticket object never overwrites its successor.
    if (this.tickets.get(ticket.key) !== ticket) return;
    const next = { ...ticket, ...change };
    this.tickets.set(ticket.key, next);
    Object.assign(ticket, next);
    this.publish();
  }

  /**
   * Explicit user action only. Admission is delegated to the backend (it joins in-flight and saved requests); a repeated click while
   * admitting returns the same outcome. A uncertain prompt delivery is shown as waiting and never resent here.
   */
  generate(request: GenerateRequest): Promise<GenerateOutcome> {
    const key = ticketKey(request, this.folderKey(request.folder));
    const running = this.admissions.get(key);
    if (running) return running;
    const created = this.admit(request, key).finally(() => { this.admissions.delete(key); });
    this.admissions.set(key, created);
    return created;
  }

  private async admit(request: GenerateRequest, key: string): Promise<GenerateOutcome> {
    // Navigation after the click (while the request is admitted) must already count against the automatic return.
    const epoch = this.hooks.epoch();
    let status: ProjectInsightStatus;
    try {
      status = await this.hooks.api.requestSystemVisualization(request.folder, request.kind, request.snapshotId, request.sourceId ?? null, request.baseRequestId ?? null, request.supersedesRequestId ?? null);
    } catch (error) {
      // The backend refused before anything was sent: stay in the overview and show why.
      return { admitted: false, status: null, error: errorText(error), uncertain: false };
    }
    this.hooks.received(status);
    const previous = this.tickets.get(key);
    if (previous) this.stop(previous);
    const ticket: VisualizationTicket = {
      key, requestId: status.requestId, folder: request.folder, snapshotId: request.snapshotId, kind: request.kind, sourceId: request.sourceId ?? null,
      producer: status.producer ?? null, destinationTab: request.area, selectedRevisionId: status.requestId, navigationEpoch: epoch, autoReturnEligible: true,
      state: status.state === "completed" ? "completed" : status.state === "failed" ? "failed" : status.state === "uncertain" ? "uncertain" : "waiting",
      message: status.message, startedAt: this.hooks.clock.now(), lateResult: false,
    };
    this.tickets.set(key, ticket);
    this.publish();
    if (status.state === "failed") return { admitted: false, status, error: status.message ?? "The Architect request failed.", uncertain: false };
    // Saved work opens passively: no Architect focus, no model turn.
    if (status.state === "completed") return { admitted: true, status, error: null, uncertain: false };
    this.watch(this.tickets.get(key)!, status);
    const architect = status.producer?.instanceId ?? this.hooks.architectId();
    let message: string | null = null;
    if (architect === null) message = "The Architect is not available. The request is saved; open The Architect to continue.";
    else if (!await this.hooks.openArchitect(architect)) message = "The Architect could not be opened. The request continues in the background.";
    const current = this.tickets.get(key);
    if (message !== null && current && current.requestId === status.requestId) this.patch(current, { message });
    return { admitted: true, status, error: null, uncertain: status.state === "uncertain" };
  }

  private watch(ticket: VisualizationTicket, admitted: ProjectInsightStatus): void {
    const session = new DependencyJobSession<InsightJobState>({
      // The request was already admitted by `generate`; the session only reads it by id and never asks the Architect again.
      start: async () => toAdmission(admitted),
      read: async requestId => {
        const status = await this.hooks.api.readProjectInsight(requestId);
        if (this.tickets.get(ticket.key)?.requestId === requestId) this.hooks.received(status);
        return toJobState(status);
      },
    }, snapshot => this.onSnapshot(ticket, snapshot), this.hooks.clock);
    this.sessions.set(ticket.key, session);
    void session.open(ticket.folder, null);
  }

  private onSnapshot(ticket: VisualizationTicket, snapshot: JobSnapshot<InsightJobState>): void {
    const live = this.tickets.get(ticket.key);
    if (!live || live.requestId !== ticket.requestId || ticket.requestId === null) return;
    const state = snapshot.state;
    if (state?.status === "complete") {
      if (live.state !== "completed") { this.patch(live, { state: "completed", message: null }); this.finished(this.tickets.get(ticket.key)!); }
    } else if (state?.status === "error") {
      this.patch(live, { state: "failed", message: state.message ?? snapshot.error ?? "The Architect request failed." });
    } else if (snapshot.timedOut) {
      this.patch(live, { state: "timedOut", message: "Still waiting after ten minutes. The Architect may still finish; the result opens from here when it is saved." });
    } else if (snapshot.error && !snapshot.pending) {
      this.patch(live, { state: "unreadable", message: `Could not read the request: ${snapshot.error}` });
    } else if (state) {
      if (state.state === "uncertain") { if (live.state !== "uncertain") this.patch(live, { state: "uncertain", message: state.message }); }
      else if (live.state === "uncertain") this.patch(live, { state: "waiting", message: null });
    }
  }

  /** The validated typed result is saved: return only when everything that justified following still holds. */
  private finished(ticket: VisualizationTicket): void {
    if (!ticket.autoReturnEligible || ticket.navigationEpoch !== this.hooks.epoch()) { this.late(ticket); return; }
    const started = this.hooks.clock.now();
    const step = (): void => {
      this.returns.delete(ticket.key);
      const live = this.tickets.get(ticket.key);
      if (!live || live.requestId !== ticket.requestId) return;
      if (!live.autoReturnEligible || live.navigationEpoch !== this.hooks.epoch() || !this.hooks.following(live)) { this.late(live); return; }
      if (this.hooks.quiescent(live)) {
        this.patch(live, { lateResult: false });
        this.hooks.returnToOverview(this.tickets.get(ticket.key)!);
        return;
      }
      if (this.hooks.clock.now() - started > RETURN_WAIT_MS) { this.late(live); return; }
      this.returns.set(ticket.key, this.hooks.clock.setTimeout(step, RETURN_POLL_MS));
    };
    step();
  }

  private late(ticket: VisualizationTicket): void { this.patch(ticket, { lateResult: true, autoReturnEligible: false }); }

  /** Manual or source/project/navigation change: this ticket will never take the screen. Optional filter limits it. */
  invalidateAutoReturn(match?: (ticket: VisualizationTicket) => boolean): void {
    for (const ticket of this.tickets.values()) {
      if (ticket.autoReturnEligible && (!match || match(ticket))) this.patch(ticket, { autoReturnEligible: false });
    }
  }

  /** User chose to see a result that arrived while they were elsewhere. */
  openResult(key: string): boolean {
    const ticket = this.tickets.get(key);
    if (!ticket || ticket.state !== "completed") return false;
    this.patch(ticket, { lateResult: false, autoReturnEligible: false });
    this.hooks.returnToOverview(this.tickets.get(key)!);
    return true;
  }

  /** Reads the request again after a stalled or timed-out watch; never admits or resends anything. */
  recheck(key: string): void {
    const ticket = this.tickets.get(key);
    if (!ticket || ticket.requestId === null || (ticket.state !== "timedOut" && ticket.state !== "unreadable")) return;
    this.stopSession(key);
    this.patch(ticket, { state: "waiting", message: null });
    const admitted: ProjectInsightStatus = { requestId: ticket.requestId, folder: ticket.folder, kind: ticket.kind, state: "submitted", message: null, result: null };
    this.watch(this.tickets.get(key)!, admitted);
  }

  dismiss(key: string): void {
    const ticket = this.tickets.get(key);
    if (!ticket) return;
    this.stop(ticket);
    this.tickets.delete(key);
    this.publish();
  }

  private stopSession(key: string): void {
    this.sessions.get(key)?.close();
    this.sessions.delete(key);
    const timer = this.returns.get(key);
    if (timer !== undefined) this.hooks.clock.clearTimeout(timer);
    this.returns.delete(key);
  }
  private stop(ticket: VisualizationTicket): void { this.stopSession(ticket.key); }

  dispose(): void {
    for (const key of [...this.sessions.keys()]) this.stopSession(key);
    this.tickets.clear();
  }
}
