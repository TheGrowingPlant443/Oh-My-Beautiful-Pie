import type { Instance } from "./api";
import type { AppSettings, ProjectLanguage } from "./settings";

export type ProjectKindName = "backend" | "frontend" | "fullstack" | "library" | "other";
export interface ProjectKindStatus { kind: ProjectKindName | null; reason: string; updatedAt: number | null }
export interface KindAdmission { architect: Instance | null; promptSent: boolean | null; error: string | null }
export interface KindSnapshot { running: boolean; error: string | null; warning: string | null; status: ProjectKindStatus | null }
export type FilterVerdict = { kind: "ok" } | { kind: "manifest"; languages: ProjectLanguage[] } | { kind: "ask" } | { kind: "ai"; role: ProjectKindName; reason: string };
export function projectFilterVerdict(settings: Pick<AppSettings, "manifestFilter" | "filterLanguages" | "aiFilter" | "filterRoles">, ecosystems: ReadonlySet<string>, kind: ProjectKindStatus | null, overridden: boolean): FilterVerdict {
  if (overridden) return { kind: "ok" };
  const languages = settings.filterLanguages.filter(language => ecosystems.has(language));
  if (settings.manifestFilter && languages.length) return { kind: "manifest", languages };
  if (settings.aiFilter && settings.filterRoles.length) {
    if (!kind?.kind) return { kind: "ask" };
    if (settings.filterRoles.some(role => kind.kind === role || kind.kind === "fullstack")) return { kind: "ai", role: kind.kind, reason: kind.reason };
  }
  return { kind: "ok" };
}
export function projectEcosystems(nodes: readonly { id: string; ecosystem: string }[]): Set<string> {
  return new Set(nodes.filter(node => node.id !== "project").map(node => node.ecosystem));
}
export function languageLabel(language: ProjectLanguage): string {
  const labels: Record<ProjectLanguage, string> = { maven: "Java (Maven/Gradle)", npm: "JavaScript / TypeScript (npm)", pypi: "Python", cargo: "Rust (Cargo)" };
  return labels[language];
}
interface ProbeDependencies {
  start(folder: string): Promise<KindAdmission>; status(folder: string): Promise<ProjectKindStatus>;
  now(): number; setTimer(fn: () => void, ms: number): unknown; clearTimer(handle: unknown): void;
  changed(snapshot: KindSnapshot): void; admitted?: (admission: KindAdmission) => void; idleTimeoutMs?: number;
}
export class KindProbe {
  private readonly deps: ProbeDependencies;
  private generation = 0;
  private timer: unknown = null;
  private snapshot: KindSnapshot = { running: false, error: null, warning: null, status: null };
  constructor(deps: ProbeDependencies) { this.deps = deps; }
  stop(): void {
    this.generation++;
    if (this.timer !== null) this.deps.clearTimer(this.timer);
    this.timer = null;
    this.snapshot = { ...this.snapshot, running: false };
    this.deps.changed(this.snapshot);
  }
  run(folder: string, previousUpdatedAt: number | null): void {
    if (this.snapshot.running) return;
    const generation = ++this.generation, startedAt = this.deps.now();
    this.snapshot = { running: true, error: null, warning: null, status: null };
    this.deps.changed(this.snapshot);
    const current = () => generation === this.generation;
    const fail = (error: unknown) => {
      if (!current()) return;
      this.snapshot = { ...this.snapshot, error: error instanceof Error ? error.message : String(error) };
      this.stop();
    };
    const poll = async (): Promise<void> => {
      try {
        const status = await this.deps.status(folder);
        if (!current()) return;
        this.snapshot = { ...this.snapshot, status };
        if (status.kind !== null && (status.updatedAt ?? 0) > (previousUpdatedAt ?? 0)) { this.stop(); return; }
        if (this.deps.now() - startedAt > (this.deps.idleTimeoutMs ?? 600000)) { fail("The Architect has not answered. Press the button to ask again."); return; }
        this.deps.changed(this.snapshot);
        schedule();
      } catch (error) { fail(error); }
    };
    const schedule = () => { this.timer = this.deps.setTimer(() => { this.timer = null; void poll(); }, 2000); };
    void this.deps.start(folder).then(admission => {
      if (!current()) return;
      if (admission.promptSent === false) { fail(admission.error ?? "The Architect prompt was not sent."); return; }
      this.snapshot = { ...this.snapshot, warning: admission.promptSent === null ? admission.error : null };
      this.deps.changed(this.snapshot);
      this.deps.admitted?.(admission);
      if (current()) schedule();
    }, fail);
  }
}
