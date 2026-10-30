import type { DiscoveredDependency, ProjectInsightKind, ProjectInsightState, ProjectInsightStatus } from "./api";
import type { DependencyJobAdmission, InsightJobStatus, JobTransport } from "./package-insights";

/** A project-insight status as the shared job session reads it: completed/failed are final, everything else keeps polling. */
export type InsightJobState = ProjectInsightStatus & { status: InsightJobStatus; error: string | null };
export interface InsightApi {
  requestProjectInsight: (folder: string, kind: ProjectInsightKind, rerun?: boolean) => Promise<ProjectInsightStatus>;
  getProjectInsight: (folder: string, kind: ProjectInsightKind) => Promise<ProjectInsightStatus | null>;
}

const JOB_STATUS: Record<ProjectInsightState, InsightJobStatus> = { pending: "pending", submitted: "pending", uncertain: "pending", completed: "complete", failed: "error" };

export function toJobState(status: ProjectInsightStatus): InsightJobState {
  return { ...status, status: JOB_STATUS[status.state], error: status.state === "failed" ? status.message : null };
}

/**
 * Admission as the generic session understands it. The backend never resends a prompt for saved or unfinished work, so only
 * an uncertain delivery has an unknown prompt outcome; a failed request is read once so its message is shown with its state.
 */
export function toAdmission(status: ProjectInsightStatus): DependencyJobAdmission {
  return {
    requestId: status.requestId, architectId: null, promptSent: status.state === "uncertain" ? null : true,
    error: status.state === "failed" ? status.message : null,
    disposition: JOB_STATUS[status.state],
  };
}

/** Subject-less repository jobs: the folder given to `start` is the one `read` asks about. */
export function insightTransport(api: InsightApi, kind: ProjectInsightKind): JobTransport<InsightJobState> {
  let folder: string | null = null;
  return {
    start: async (target, _subject, rerun) => {
      const status = await api.requestProjectInsight(target, kind, rerun);
      folder = target;
      return toAdmission(status);
    },
    read: async requestId => {
      if (folder === null) throw new Error("No Architect request is open.");
      const status = await api.getProjectInsight(folder, kind);
      if (status?.requestId !== requestId) throw new Error("The saved Architect request changed. Open it again.");
      return toJobState(status);
    },
  };
}

/** Distinct, known ecosystems of the discovered packages, in a stable order. */
export function discoveryEcosystems(items: readonly DiscoveredDependency[]): string[] {
  return [...new Set(items.flatMap(item => (item.ecosystem ? [item.ecosystem] : [])))].sort((a, b) => a.localeCompare(b));
}

export function citationText(citation: { path: string; line: number | null }): string {
  return citation.line ? `${citation.path}:${citation.line}` : citation.path;
}
