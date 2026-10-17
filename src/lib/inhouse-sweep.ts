import type { InhouseAdmission, InhouseStatus } from "./package-insights";

export interface SweepSnapshot { running: boolean; error: string | null; warning: string | null; status: InhouseStatus | null }
interface SweepDependencies {
  start(folder: string): Promise<InhouseAdmission>; status(folder: string): Promise<InhouseStatus>;
  now(): number; setTimer(fn: () => void, ms: number): unknown; clearTimer(handle: unknown): void;
  changed(snapshot: SweepSnapshot): void; admitted?: (admission: InhouseAdmission) => void; idleTimeoutMs?: number;
}
export class InhouseSweep {
  private deps: SweepDependencies;
  private generation = 0;
  private timer: unknown = null;
  private snapshot: SweepSnapshot = { running: false, error: null, warning: null, status: null };
  constructor(deps: SweepDependencies) { this.deps = deps; }
  stop(): void {
    this.generation++;
    if (this.timer !== null) this.deps.clearTimer(this.timer);
    this.timer = null;
    this.snapshot = { ...this.snapshot, running: false };
    this.deps.changed(this.snapshot);
  }
  run(folder: string): void {
    if (this.snapshot.running) return;
    const generation = ++this.generation;
    let lastProgressAt = this.deps.now(), lastJudged = 0, targetJudged = 0;
    this.snapshot = { running: true, error: null, warning: null, status: null };
    this.deps.changed(this.snapshot);
    const current = () => generation === this.generation;
    const fail = (error: unknown) => {
      if (!current()) return;
      this.snapshot = { ...this.snapshot, error: error instanceof Error ? error.message : String(error) };
      this.stop();
    };
    const send = async (status: InhouseStatus): Promise<void> => {
      const admission = await this.deps.start(folder);
      if (!current()) return;
      if (admission.promptSent === false) { fail(admission.error ?? "The Architect prompt was not sent."); return; }
      targetJudged = status.judged + admission.requested;
      this.snapshot = { ...this.snapshot, warning: admission.promptSent === null ? admission.error : null };
      this.deps.changed(this.snapshot);
      this.deps.admitted?.(admission);
      if (!current()) return;
      if (admission.requested === 0) { fail("Nothing left to ask but some packages are still unjudged."); return; }
      if (current()) schedule();
    };
    const poll = async (initial = false): Promise<void> => {
      try {
        const status = await this.deps.status(folder);
        if (!current()) return;
        this.snapshot = { ...this.snapshot, status };
        if (status.complete) { this.stop(); return; }
        if (initial || status.judged > lastJudged) lastProgressAt = this.deps.now();
        lastJudged = status.judged;
        this.deps.changed(this.snapshot);
        if (this.deps.now() - lastProgressAt > (this.deps.idleTimeoutMs ?? 600000)) { fail("The Architect has not finished judging the packages. Press the button to continue."); return; }
        if (initial || status.judged >= targetJudged) await send(status);
        else schedule();
      } catch (error) { fail(error); }
    };
    const schedule = () => { this.timer = this.deps.setTimer(() => { this.timer = null; void poll(); }, 2000); };
    void poll(true);
  }
}
