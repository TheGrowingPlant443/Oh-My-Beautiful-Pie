import type { DepNode } from "./dependency-graph";
import type { Instance } from "./api";

// Same synthetic project id as dependency-graph.ts. Library modules import each other for types only (Node tests run the TS directly).
const ROOT = "project";

export type InsightStatus = "available" | "unavailable" | "rateLimited" | "error";
export interface CountMetric {
  value: number | null; status: InsightStatus; source: string;
  period: string | null; reason: string | null; retryAt: number | null;
}
export interface RepositoryInsights {
  url: string; ownerLogin: string; ownerType: "user" | "organization" | "unknown";
  stars: CountMetric; contributors: CountMetric;
  checkedAt: number;
}
export interface PackageInsights {
  id: string; registryUrl: string; newestVersion: string | null; newestStatus: InsightStatus;
  repository: RepositoryInsights | null; repositoryReason: string | null; downloads: CountMetric;
  checkedAt: number; notes: string[];
  description: string | null; repositoryUrl: string | null; githubPending: boolean;
}
export type GithubMode = "skip" | "demand" | "bulk";
export interface PackageInsightsStatus {
  githubAuthenticated: boolean; githubLimit: number | null; githubRemaining: number | null;
  githubResetAt: number | null; githubCooldownUntil: number | null;
  githubSource: string | null; githubProblem: string | null;
}
export interface InhouseStatus {
  gate: boolean; complete: boolean; total: number; judged: number;
  inhouse: Record<string, string>; updatedAt: number | null;
}
export interface InhouseAdmission {
  architect: Instance | null; promptSent: boolean | null; requested: number; remaining: number; error: string | null;
}
export type JobDisposition = "new" | "pending" | "complete" | "error";
export interface DependencyJobAdmission { requestId: string; architectId: string | null; promptSent: boolean | null; error: string | null; disposition: JobDisposition }
export type InsightJobStatus = "pending" | "complete" | "error";
export interface SecurityEvidence { title: string; url: string }
export interface ProjectSourceEvidence { path: string; lineStart: number; lineEnd: number; explanation: string }
export interface ProjectApplicability { status: "relevant" | "notRelevant" | "unknown"; reason: string; conditions: string[]; evidence: ProjectSourceEvidence[] }
export interface SecurityFinding {
  title: string; severity: "critical" | "high" | "moderate" | "low" | "unknown";
  majorVersion: string | null; affectedVersions: string; fixedVersion: string | null;
  advisoryId: string | null; evidence: SecurityEvidence[]; applicability: ProjectApplicability;
}
export interface SecurityReviewReport { summary: string; findings: SecurityFinding[]; limitations: string[]; completedAt: number }
/** What a saved review or explanation actually covered; never today's node versions. */
export interface ReviewedProvenance {
  folder: string; contextId: string | null; requestedAt: number | null; resolvedVersions: string[];
  declaredVersions: string[]; newestVersion: string | null; newestStatus: InsightStatus;
}
export interface SecurityReviewState {
  requestId: string; packageId: string; status: InsightJobStatus;
  report: SecurityReviewReport | null; error: string | null; reviewed: ReviewedProvenance;
}
export interface PackageUsageReport {
  status: "used" | "notObserved" | "unknown"; summary: string; purpose: string;
  projectRole: "runtime" | "build" | "test" | "configuration" | "transitiveFeature" | "unknown";
  benefits: string[];
  paths: { packageIds: string[]; explanation: string; evidence: ProjectSourceEvidence[] }[];
  sourceEvidence: ProjectSourceEvidence[]; limitations: string[]; completedAt: number;
}
export interface UsageExplanationState {
  requestId: string; packageId: string; status: InsightJobStatus;
  report: PackageUsageReport | null; error: string | null; reviewed: ReviewedProvenance;
}
export interface ProjectCoverage { packageId: string; requestId: string | null; status: InsightJobStatus | "skippedInhouse"; reason: string | null }
export interface ProjectReviewState {
  requestId: string; folder: string; contextId: string | null; status: InsightJobStatus;
  coverage: ProjectCoverage[]; summary: string | null; limitations: string[]; completedAt: number | null; error: string | null;
}
export interface UpdatePlanProposal { revisionId: string; hash: string; title: string; summary: string; markdown: string; sourceReviewIds: string[]; completedAt: number }
export interface UpdatePlanHandoff { status: "attempting" | "accepted" | "knownUnsent" | "uncertain"; managerId: string; revisionId: string; error: string | null }
/** Which saved review a draft plans from, and which current-scan snapshot it used; `historicalContextNote` is non-null for legacy reviews and shown verbatim. */
export interface UpdatePlanSource {
  kind: "security" | "project"; reviewId: string; contextId: string | null; completedAt: number;
  planningContextId: string; planningCapturedAt: number; planningCheckedAt: number; historicalContextNote: string | null;
}
export interface UpdatePlanState {
  requestId: string; folder: string; contextId: string | null; status: InsightJobStatus;
  proposal: UpdatePlanProposal | null; handoff: UpdatePlanHandoff | null; error: string | null; source?: UpdatePlanSource;
}
export interface HandoffPrepared {
  confirmationToken: string; expiresAt: number; revisionId: string; managerId: string; managerName: string;
  needsStart: boolean; projectFolder: string; title: string;
}

/** One-line GitHub budget/credential summary shown beside the metadata controls. */
export function githubStatusText(status: PackageInsightsStatus, formatTime: (epochMs: number) => string = value => new Date(value).toLocaleTimeString()): string {
  const limit = status.githubLimit;
  const parts: string[] = [];
  if (status.githubRemaining !== null && limit !== null) parts.push(`${status.githubRemaining}/${limit} left`);
  else if (limit !== null) parts.push(`${limit}/hour`);
  if (status.githubRemaining !== null && status.githubResetAt !== null) parts.push(`resets ${formatTime(status.githubResetAt)}`);
  parts.push(status.githubAuthenticated ? `token${status.githubSource ? ` (${status.githubSource})` : ""}` : "anonymous");
  return `GitHub API: ${parts.join(" · ")}`;
}

/** Why no GitHub login was found; shown on its own line, only while the budget is the small anonymous one. */
export function githubProblemText(status: PackageInsightsStatus): string | null {
  return status.githubAuthenticated ? null : status.githubProblem;
}

const knownCount = (metric: CountMetric | undefined): number => metric?.status === "available" && metric.value !== null && Number.isFinite(metric.value) && metric.value >= 0 ? metric.value : Infinity;
export function rankPackages(nodes: readonly DepNode[], insights: ReadonlyMap<string, PackageInsights>): DepNode[] {
  return nodes.filter(node => node.id !== ROOT).sort((a, b) => {
    const left = insights.get(a.id)?.repository, right = insights.get(b.id)?.repository;
    const starsA = knownCount(left?.stars), starsB = knownCount(right?.stars);
    if (starsA !== starsB) return starsA < starsB ? -1 : 1;
    const contributorsA = knownCount(left?.contributors), contributorsB = knownCount(right?.contributors);
    if (contributorsA !== contributorsB) return contributorsA < contributorsB ? -1 : 1;
    return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
  });
}
export function formatCount(metric: CountMetric | undefined): string {
  if (!metric) return "Loading…";
  if (metric.status === "unavailable") return "Unavailable";
  if (metric.status === "rateLimited") return "Rate limited";
  return knownCount(metric) === Infinity ? "Unknown" : metric.value!.toLocaleString("en-US");
}
export function rankingIncomplete(nodes: readonly DepNode[], insights: ReadonlyMap<string, PackageInsights>): boolean {
  return nodes.some(node => node.id !== ROOT && (knownCount(insights.get(node.id)?.repository?.stars) === Infinity || knownCount(insights.get(node.id)?.repository?.contributors) === Infinity));
}
export const PAGE_SIZE = 50;
export function packagePage(nodes: readonly DepNode[], insights: ReadonlyMap<string, PackageInsights>, page: number): DepNode[] {
  return rankPackages(nodes, insights).slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE);
}
export function parseNumericMajor(version: string): number | null {
  const match = /^v?(0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:\.(?:0|[1-9]\d*))?(?:[-+][0-9A-Za-z][0-9A-Za-z.-]*)?$/.exec(version.trim());
  if (!match) return null;
  const major = Number(match[1]);
  return Number.isSafeInteger(major) ? major : null;
}
export function packageMajorLag(node: DepNode, insights: PackageInsights | undefined): number | null {
  if (node.id === ROOT || !node.resolvedVersions.length || insights?.newestStatus !== "available" || !insights.newestVersion) return null;
  const newest = parseNumericMajor(insights.newestVersion);
  if (newest === null) return null;
  let largest = 0, unknown = false;
  for (const version of node.resolvedVersions) {
    const used = parseNumericMajor(version);
    if (used === null) unknown = true;
    else largest = Math.max(largest, newest - used);
  }
  return largest >= 2 ? largest : unknown ? null : largest;
}
export function projectVersionText(node: DepNode): string {
  const label = node.ecosystem === "cargo" ? " (lockfile versions)" : "";
  if (node.resolvedVersions.length) return `Project uses: ${node.resolvedVersions.join(", ")}${node.resolvedVersions.length > 1 ? " (multiple resolved versions)" : ""}${label}`;
  return `Project version: not resolved · Declared: ${node.declaredVersions.join(", ") || "Unknown"}`;
}
export function newestText(insights: PackageInsights | undefined, error?: string): string {
  if (error) return "Unknown";
  if (!insights) return "Loading…";
  if (insights.newestStatus === "available" && insights.newestVersion) return insights.newestVersion;
  return insights.newestStatus === "rateLimited" ? "Rate limited" : insights.newestStatus === "unavailable" ? "Unavailable" : "Unknown";
}
export function ownerText(repository: RepositoryInsights | null | undefined): string {
  return repository?.ownerType === "user" ? "personal account" : repository?.ownerType === "organization" ? "organization" : "unknown";
}
export function safePublicUrl(value: string): string | null {
  try {
    const url = new URL(value);
    return url.protocol === "https:" && !url.username && !url.password ? url.href : null;
  } catch { return null; }
}
export function groupFindings(findings: readonly SecurityFinding[]): [string, SecurityFinding[]][] {
  const groups = new Map<string, SecurityFinding[]>();
  for (const finding of findings) {
    const key = finding.majorVersion ?? "Unspecified";
    const entries = groups.get(key);
    if (entries) entries.push(finding); else groups.set(key, [finding]);
  }
  return [...groups];
}

/** Desired work can change without cancelling active IPC calls. */
export class PackageInsightsQueue {
  readonly insights = new Map<string, PackageInsights>();
  readonly errors = new Map<string, string>();
  readonly refreshing: ReadonlySet<string> = new Set<string>();
  private queue: { id: string; mode: GithubMode }[] = [];
  private attempted = new Set<string>();
  private active = new Set<string>();
  private inFlight = 0;
  private githubInFlight = 0;
  private generation = 0;
  private stopped = false;
  private readonly lookup: (id: string, github: GithubMode, refresh: boolean) => Promise<PackageInsights>;
  private readonly changed: () => void;
  constructor(lookup: (id: string, github: GithubMode, refresh: boolean) => Promise<PackageInsights>, changed: () => void) {
    this.lookup = lookup;
    this.changed = changed;
  }
  prioritize(plan: { demand: readonly string[]; registry: readonly string[]; bulk: readonly string[] }): void {
    if (this.stopped) return;
    const work = new Map<string, GithubMode>();
    const strength = { skip: 0, bulk: 1, demand: 2 };
    for (const [ids, mode] of [[plan.demand, "demand"], [plan.registry, "skip"], [plan.bulk, "bulk"]] as const) {
      for (const id of ids) {
        if (id === ROOT) continue;
        const previous = work.get(id);
        if (previous === undefined || strength[mode] > strength[previous]) work.set(id, mode);
      }
    }
    this.queue = [...work].map(([id, mode]) => ({ id, mode }));
    this.pump();
  }
  forget(ids: readonly string[]): void {
    for (const id of ids) {
      this.attempted.delete(`${id}|demand`);
      this.attempted.delete(`${id}|bulk`);
    }
  }
  async refresh(id: string): Promise<void> {
    if (this.stopped || id === ROOT || this.refreshing.has(id)) return;
    const generation = this.generation;
    const refreshing = this.refreshing as Set<string>;
    refreshing.add(id);
    this.changed();
    try {
      const result = await this.lookup(id, "demand", true);
      if (!this.stopped && generation === this.generation) {
        this.insights.set(id, result);
        this.errors.delete(id);
      }
    } catch (error) {
      if (!this.stopped && generation === this.generation) this.errors.set(id, error instanceof Error ? error.message : String(error));
    } finally {
      if (generation === this.generation) { refreshing.delete(id); this.changed(); this.pump(); }
    }
  }
  reset(): void {
    this.generation++;
    this.queue = [];
    this.attempted.clear();
    this.insights.clear();
    this.errors.clear();
    (this.refreshing as Set<string>).clear();
    this.changed();
  }
  stop(): void { this.reset(); this.stopped = true; }
  private pump(): void {
    if (this.stopped) return;
    while (this.inFlight < 4) {
      const index = this.queue.findIndex(({ id, mode }) =>
        !this.active.has(id) && !this.refreshing.has(id) &&
        !this.attempted.has(`${id}|${mode}`) &&
        !(this.insights.has(id) && (mode === "skip" || !this.insights.get(id)!.githubPending)) &&
        (mode === "skip" || this.githubInFlight === 0));
      if (index < 0) return;
      const { id, mode } = this.queue.splice(index, 1)[0];
      const generation = this.generation;
      this.attempted.add(`${id}|${mode}`);
      this.active.add(id);
      this.inFlight++;
      if (mode !== "skip") this.githubInFlight++;
      void this.lookup(id, mode, false).then(result => {
        if (!this.stopped && generation === this.generation) {
          this.insights.set(id, result);
          this.errors.delete(id);
          this.changed();
        }
      }, error => {
        if (!this.stopped && generation === this.generation) {
          this.errors.set(id, error instanceof Error ? error.message : String(error));
          this.changed();
        }
      }).finally(() => {
        this.active.delete(id);
        this.inFlight--;
        if (mode !== "skip") this.githubInFlight--;
        this.pump();
      });
    }
  }
}

export interface JobSnapshot<T> {
  requestId: string | null; admission: DependencyJobAdmission | null; state: T | null;
  error: string | null; pending: boolean; timedOut: boolean;
}
export type ReviewSnapshot = JobSnapshot<SecurityReviewState> & { packageId: string | null };
export interface JobTransport<T> {
  start: (folder: string, subject: string | null, rerun: boolean) => Promise<DependencyJobAdmission>;
  read: (requestId: string) => Promise<T>;
}
interface ReviewClock {
  setTimeout: (callback: () => void, milliseconds: number) => number;
  clearTimeout: (timer: number) => void;
}
const reviewClock: ReviewClock = { setTimeout: (fn, ms) => window.setTimeout(fn, ms), clearTimeout: timer => window.clearTimeout(timer) };
const blankJob = <T>(): JobSnapshot<T> => ({ requestId: null, admission: null, state: null, error: null, pending: false, timedOut: false });

/**
 * Admission, reuse and polling for one selected-package, project or update-plan job. The backend is authoritative: every
 * user open asks it again (it joins pending work and reuses saved results), and the promise Map only collapses admission
 * IPC calls that have not settled yet. UI cancellation (close) never cancels or resends backend work.
 */
export class DependencyJobSession<T extends { requestId: string; status: InsightJobStatus; error?: string | null; packageId?: string }> {
  snapshot: JobSnapshot<T> = blankJob<T>();
  private sequence = 0;
  private generation = 0;
  private pollTimer: number | null = null;
  private deadlineTimer: number | null = null;
  private release: (() => void) | null = null;
  private readonly admissions = new Map<string, Promise<DependencyJobAdmission>>();
  private readonly transport: JobTransport<T>;
  private readonly changed: (snapshot: JobSnapshot<T>) => void;
  private readonly clock: ReviewClock;
  constructor(transport: JobTransport<T>, changed: (snapshot: JobSnapshot<T>) => void, clock: ReviewClock = reviewClock) {
    this.transport = transport;
    this.changed = changed;
    this.clock = clock;
  }
  close(): void {
    this.supersede();
    this.update(blankJob<T>());
  }
  /** Drops local state and any in-flight admission dedupe; backend history is untouched. */
  reset(): void { this.close(); this.generation++; this.admissions.clear(); }
  private update(snapshot: JobSnapshot<T>): void {
    this.snapshot = snapshot;
    this.changed(snapshot);
  }
  /** Stops this session's timers and frees any open() still waiting on IPC; backend work continues. */
  private supersede(): void {
    this.sequence++;
    if (this.pollTimer !== null) this.clock.clearTimeout(this.pollTimer);
    if (this.deadlineTimer !== null) this.clock.clearTimeout(this.deadlineTimer);
    this.pollTimer = null;
    this.deadlineTimer = null;
    this.release?.();
    this.release = null;
  }
  /**
   * Resolves after admission and one authoritative read. The returned disposition reflects that read (a result that completed
   * while no popup was open reads as "complete"). Null: superseded, closed, timed out, or no admission was obtained (see snapshot.error).
   */
  async open(folder: string, subject: string | null, rerun = false): Promise<DependencyJobAdmission | null> {
    this.supersede();
    const sequence = this.sequence, generation = this.generation;
    const current = (): boolean => sequence === this.sequence && generation === this.generation;
    const released = new Promise<null>(resolve => { this.release = () => resolve(null); });
    this.update({ ...blankJob<T>(), pending: true });
    const key = `${folder}\0${subject ?? ""}\0${rerun}`;
    let pendingAdmission = this.admissions.get(key);
    if (!pendingAdmission) {
      const created = this.transport.start(folder, subject, rerun);
      pendingAdmission = created;
      this.admissions.set(key, created);
      const settled = (): void => { if (this.admissions.get(key) === created) this.admissions.delete(key); };
      created.then(settled, settled);
    }
    this.deadlineTimer = this.clock.setTimeout(() => {
      if (!current()) return;
      this.supersede();
      this.update({ ...this.snapshot, pending: false, timedOut: true });
    }, 10 * 60_000);
    const stopDeadline = (): void => {
      if (this.deadlineTimer !== null) this.clock.clearTimeout(this.deadlineTimer);
      this.deadlineTimer = null;
    };
    const fail = (error: unknown): void => {
      stopDeadline();
      this.update({ ...this.snapshot, pending: false, error: error instanceof Error ? error.message : String(error) });
    };
    let accepted: DependencyJobAdmission | null;
    try {
      accepted = await Promise.race([pendingAdmission, released]);
    } catch (error) {
      if (current()) fail(error);
      return null;
    }
    if (!accepted || !current()) return null;
    const admitted = accepted;
    this.update({ ...this.snapshot, requestId: admitted.requestId, admission: admitted, error: admitted.error, pending: admitted.promptSent !== false || admitted.disposition === "complete" });
    if (admitted.promptSent === false && admitted.disposition !== "complete") { stopDeadline(); return admitted; }
    const step = async (): Promise<T | null> => {
      try {
        const state = await this.transport.read(admitted.requestId);
        if (!current()) return null;
        if (state.requestId !== admitted.requestId || (subject !== null && state.packageId !== undefined && state.packageId !== subject)) {
          throw new Error("The review response did not match the requested package.");
        }
        this.update({ ...this.snapshot, state, pending: state.status === "pending", error: state.error ?? admitted.error });
        if (state.status === "pending") this.pollTimer = this.clock.setTimeout(() => { void step(); }, 1000);
        else stopDeadline();
        return state;
      } catch (error) {
        if (current()) fail(error);
        return null;
      }
    };
    const first = await Promise.race([step(), released]);
    if (!current()) return null;
    if (!first) return admitted;
    const disposition: JobDisposition = first.status === "complete" ? "complete" : first.status === "error" ? "error" : admitted.disposition === "new" ? "new" : "pending";
    const refreshed = { ...admitted, disposition };
    this.update({ ...this.snapshot, admission: refreshed });
    return refreshed;
  }
}

/** UI cancellation never cancels or resends the shared Architect conversation. */
export class DependencyReviewSession {
  snapshot: ReviewSnapshot = { ...blankJob<SecurityReviewState>(), packageId: null };
  private packageId: string | null = null;
  private readonly job: DependencyJobSession<SecurityReviewState>;
  constructor(
    transport: { start: (folder: string, packageId: string, rerun: boolean) => Promise<DependencyJobAdmission>; read: (requestId: string) => Promise<SecurityReviewState> },
    changed: (snapshot: ReviewSnapshot) => void,
    clock: ReviewClock = reviewClock,
  ) {
    this.job = new DependencyJobSession<SecurityReviewState>(
      { start: (folder, subject, rerun) => transport.start(folder, subject ?? "", rerun), read: transport.read },
      snapshot => {
        this.snapshot = { ...snapshot, packageId: this.packageId };
        changed(this.snapshot);
      },
      clock,
    );
  }
  close(): void { this.packageId = null; this.job.close(); }
  reset(): void { this.packageId = null; this.job.reset(); }
  async open(folder: string, packageId: string, rerun = false): Promise<DependencyJobAdmission | null> {
    if (packageId === ROOT) return null;
    this.packageId = packageId;
    return this.job.open(folder, packageId, rerun);
  }
}
