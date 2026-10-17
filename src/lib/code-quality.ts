export type QualityMode = "sonarqubeCommunity" | "sonarqubeEnterprise" | "alternate";
export type QualityRuntime = { kind: "dockerDesktop" } | { kind: "dockerWsl"; distro: string } | { kind: "podman"; connection: string };
export interface QualityConfig { version: 1; mode: QualityMode; runtime: QualityRuntime; communityPort: number; enterpriseUrl: string }
export type QualityPhase = "idle" | "checking" | "installing" | "pulling" | "stopping" | "starting" | "waitingForReady" | "validating" | "scanning" | "cancelling" | "needsSetup" | "needsRestart" | "error";
export interface QualityCredentialUpdate { target: "community" | "enterprise"; action: "replace" | "clear"; token?: string }
export interface QualityState {
  revision: number; config: QualityConfig; effectiveMode: QualityMode | null; phase: QualityPhase; busy: boolean; operationId: string | null;
  requestedConfig: QualityConfig | null; error: string | null; activated: boolean;
  community: { running: boolean | null; url: string | null; runtimeId: string | null };
  credentials: { community: boolean; enterprise: boolean };
  rules: { manifestHash: string; licenseHash: string; acknowledged: boolean; licenseText: string };
  prerequisites: { runtime: QualityRuntime; installed: boolean; ready: boolean; needsRestart: boolean; message: string | null }[];
  options: { wslDistros: string[]; podmanConnections: string[]; wingetAvailable: boolean; podmanAppConnection: string | null };
  diagnostics: string[]; unresolved: string[]; activeScanId: string | null;
}
export interface QualityProject {
  folder: string; projectKey: string; scanner: "cli" | "maven" | "gradle" | "dotnet"; entryFile: string | null; image: string | null;
  buildArgs: string[]; prepareNpmDependencies: boolean; exclusions: string[];
}
export type QualityScanStatus = "running" | "cancelling" | "cancelled" | "complete" | "partial" | "error" | "interrupted";
export interface QualityToolOutcome { tool: string; version: string; status: "complete" | "partial" | "error" | "skipped"; exitCode: number | null; analyzedFiles: number | null; findingCount: number; error: string | null; reportNames: string[] }
export interface QualityFinding {
  id: string; tools: string[]; ruleId: string; category: string; severity: "critical" | "high" | "medium" | "low" | "info";
  message: string; path: string | null; line: number | null; column: number | null; endLine: number | null; helpUrl: string | null; hotspot: boolean;
}
export interface QualityScan {
  id: string; folder: string; config: QualityConfig; project: QualityProject; status: QualityScanStatus; phase: string; startedAt: number; finishedAt: number | null;
  snapshotHash: string; rulesHash: string; tools: QualityToolOutcome[]; findings: QualityFinding[]; warnings: string[]; error: string | null;
  analysisId: string | null; dashboardUrl: string | null; qualityGate: "passed" | "failed" | "unknown" | null; log: string[];
}
export interface QualityScanPreparation { project: QualityProject; recipe: string[]; recipeHash: string; requiresConsent: boolean; warnings: string[] }
export interface QualityProjectDetection { project: QualityProject; families: QualityProject["scanner"][]; entryFiles: string[]; explicitKey: boolean; needsSelection: boolean; warnings: string[] }
export interface QualityReportPreview { text: string; truncated: boolean }
export const QUALITY_BUSY = "Code quality is busy; wait for the current operation to finish.";
export function defaultQualityConfig(): QualityConfig { return { version: 1, mode: "sonarqubeCommunity", runtime: { kind: "podman", connection: "" }, communityPort: 9000, enterpriseUrl: "" }; }
export function modeSelectors(mode: QualityMode): { analyzer: "sonarqube" | "alternate"; connection: "community" | "enterprise" } { return { analyzer: mode === "alternate" ? "alternate" : "sonarqube", connection: mode === "sonarqubeEnterprise" ? "enterprise" : "community" }; }
export function selectorMode(analyzer: "sonarqube" | "alternate", connection: "community" | "enterprise"): QualityMode { return analyzer === "alternate" ? "alternate" : connection === "enterprise" ? "sonarqubeEnterprise" : "sonarqubeCommunity"; }
export function modeLabel(mode: QualityMode | null): string { return mode === null ? "Not active" : mode === "alternate" ? "MegaLinter + Opengrep + Semgrep CE" : mode === "sonarqubeEnterprise" ? "SonarQube Enterprise" : "SonarQube Community"; }
export function runtimeLabel(runtime: QualityRuntime): string { return runtime.kind === "podman" ? `Podman · ${runtime.connection || "app-managed machine (not resolved)"}` : runtime.kind === "dockerWsl" ? `Docker in WSL · ${runtime.distro || "select a distro"}` : "Docker Desktop"; }
export const qualityFolderKey = (folder: string): string => folder.replace(/\\/g, "/").replace(/\/+$/, "").toLowerCase();
/** An async result belongs to both a folder and the generation that requested it. */
export class QualityGeneration {
  private value = 0;
  private folder = "";
  begin(folder: string): number { this.folder = qualityFolderKey(folder); return ++this.value; }
  accepts(generation: number, folder: string): boolean { return this.value === generation && this.folder === qualityFolderKey(folder); }
  invalidate(): void { this.value++; }
}
export interface FindingFilter { tool: string; severity: string; text: string }
export function filterQualityFindings(findings: readonly QualityFinding[], filter: FindingFilter): QualityFinding[] {
  const text = filter.text.trim().toLowerCase();
  return findings.filter(f => (!filter.tool || f.tools.includes(filter.tool)) && (!filter.severity || f.severity === filter.severity) && (!text || [f.message, f.ruleId, f.path ?? "", f.category, ...f.tools].join(" ").toLowerCase().includes(text)));
}
export function findingTools(findings: readonly QualityFinding[]): string[] { return [...new Set(findings.flatMap(f => f.tools))].sort(); }
/** Union attribution only for an identical finding id; backend owns semantic deduplication. */
export function mergeFindingAttribution(findings: readonly QualityFinding[]): QualityFinding[] {
  const result = new Map<string, QualityFinding>();
  for (const finding of findings) { const previous = result.get(finding.id); result.set(finding.id, { ...finding, tools: [...new Set([...(previous?.tools ?? []), ...finding.tools])].sort() }); }
  return [...result.values()];
}
export function qualityPage<T>(items: readonly T[], requested: number, size = 100): { items: T[]; page: number; pages: number; total: number } {
  const count = Math.max(1, Math.floor(size)); const pages = Math.max(1, Math.ceil(items.length / count));
  const page = Math.min(pages - 1, Math.max(0, Math.floor(requested) || 0));
  return { items: items.slice(page * count, (page + 1) * count), page, pages, total: items.length };
}
export function scanPresentation(scan: Pick<QualityScan, "status" | "findings" | "qualityGate">): { label: string; tone: "ok" | "warn" | "err" | "dim" } {
  if (scan.status === "complete") return { label: scan.findings.length ? `Completed · ${scan.findings.length} findings` : "Completed · zero findings in analyzed coverage", tone: scan.qualityGate === "failed" ? "warn" : "ok" };
  const labels: Record<Exclude<QualityScanStatus, "complete">, string> = { running: "Running", cancelling: "Cancelling · waiting for work to stop", cancelled: "Cancelled · analysis incomplete", partial: "Partial · coverage incomplete", error: "Failed · not a clean result", interrupted: "Interrupted · analysis incomplete" };
  return { label: labels[scan.status], tone: scan.status === "error" ? "err" : scan.status === "running" ? "dim" : "warn" };
}
export function isActiveScan(scan: QualityScan | null): boolean { return scan?.status === "running" || scan?.status === "cancelling"; }
export function parseQualityPreferences(raw: string | null): { folder: string; tool: string; severity: string; text: string } {
  try { const value: unknown = JSON.parse(raw ?? "{}"); if (typeof value === "object" && value !== null) { const v = value as Record<string, unknown>; return { folder: typeof v.folder === "string" ? v.folder : "", tool: typeof v.tool === "string" ? v.tool : "", severity: typeof v.severity === "string" ? v.severity : "", text: typeof v.text === "string" ? v.text : "" }; } } catch { /* Non-authoritative UI preferences can be discarded. */ }
  return { folder: "", tool: "", severity: "", text: "" };
}

export interface QualityClientSnapshot { state: QualityState | null; pending: boolean; uncertain: boolean; error: string | null }
export function qualityLocked(snapshot: QualityClientSnapshot): boolean { return !snapshot.state || snapshot.pending || snapshot.uncertain || snapshot.state.busy; }
interface QualityClientDependencies { read(): Promise<QualityState>; setTimer(fn: () => void, ms: number): unknown; clearTimer(handle: unknown): void }
/** One admission and polling owner shared by every mounted settings/page surface. */
export class QualityClient {
  private deps: QualityClientDependencies;
  private listeners = new Set<(snapshot: QualityClientSnapshot) => void>();
  private timer: unknown = null;
  private generation = 0;
  private request = 0;
  private applied = 0;
  private value: QualityClientSnapshot = { state: null, pending: false, uncertain: false, error: null };
  constructor(deps: QualityClientDependencies) { this.deps = deps; }
  get snapshot(): QualityClientSnapshot { return this.value; }
  subscribe = (listener: (snapshot: QualityClientSnapshot) => void): (() => void) => {
    this.listeners.add(listener); listener(this.value);
    if (this.listeners.size === 1) { void this.refresh(); this.schedule(); }
    return () => { this.listeners.delete(listener); this.schedule(); };
  };
  private publish(patch: Partial<QualityClientSnapshot>): void { this.value = { ...this.value, ...patch }; for (const listener of this.listeners) listener(this.value); this.schedule(); }
  private schedule(): void {
    const wanted = this.listeners.size > 0 || this.value.pending || this.value.uncertain || this.value.state?.busy;
    if (!wanted && this.timer !== null) { this.deps.clearTimer(this.timer); this.timer = null; }
    if (wanted && this.timer === null) this.timer = this.deps.setTimer(() => { this.timer = null; void this.refresh(); this.schedule(); }, 1000);
  }
  async refresh(): Promise<void> {
    const generation = this.generation, request = ++this.request;
    try {
      const state = await this.deps.read();
      if (generation !== this.generation || request < this.applied) return;
      this.applied = request;
      this.publish({ state, uncertain: false });
    } catch (error) { if (generation === this.generation && request >= this.applied) this.publish({ error: String(error) }); }
  }
  async mutate<T>(action: (state: QualityState) => Promise<T>, allowActiveScanCancel = false): Promise<T> {
    if (!this.value.state || this.value.pending || this.value.uncertain || (this.value.state.busy && !allowActiveScanCancel)) throw new Error(QUALITY_BUSY);
    const state = this.value.state;
    this.generation++;
    this.publish({ pending: true, error: null });
    try { return await action(state); }
    catch (error) { this.publish({ error: String(error) }); throw error; }
    finally {
      this.generation++;
      this.publish({ uncertain: true });
      await this.refresh();
      this.publish({ pending: false });
    }
  }
}
