import type { ProjectInsightStatus, SourceFileRow, VisualizationKind } from "./api";

/** A user's explicit request to have the existing Architect generate a visualization. Only an explicit click builds one. */
export interface GenerateRequest {
  kind: VisualizationKind;
  /** Canonical folder shown by the page. */
  folder: string;
  /** The selected complete source-index snapshot every visualization is bound to. */
  snapshotId: string;
  /** structureProposed only: the human-selected captured Markdown source. */
  sourceId?: string | null;
  /** structureProposed only: a completed structureCurrent request for the same snapshot. */
  baseRequestId?: string | null;
  /** Present only for an explicit regeneration; creates a new immutable request that names its predecessor. */
  supersedesRequestId?: string | null;
  /** Which area the parent returns to when this exact request's validated result is saved. */
  area: "helicopter" | "system" | "structure";
}

export interface GenerateOutcome {
  /** True when a request was admitted or joined (the Architect was asked, or the work was already saved/in flight). */
  admitted: boolean;
  status: ProjectInsightStatus | null;
  error: string | null;
  /** The prompt delivery outcome is unknown. It is never resent automatically. */
  uncertain: boolean;
}

/** The complete admitted-file inventory of the selected snapshot, loaded page by page without any agent. */
export interface InventoryState {
  snapshotId: string | null;
  status: "idle" | "loading" | "ready" | "error";
  files: SourceFileRow[];
  /** Admitted directory paths, including empty ones ("" is the repository root). */
  directories: string[];
  total: number;
  error: string | null;
}

export const EMPTY_INVENTORY: InventoryState = { snapshotId: null, status: "idle", files: [], directories: [], total: 0, error: null };

/**
 * The one local description of the existing Architect shared by App's attach hook, SystemOverviewPage, StructureView and StructureHost.
 * `sessionId` is read from the matching-generation terminal owner (null while unknown); it identifies the captured-message session.
 */
export interface ArchitectDescriptor { id: string; generation: number; sessionId: string | null; ready: boolean }

/** What the parent gives the structure area; it never reaches for app stores, the Architect or the backend on its own. */
export interface StructureHost {
  folder(): string;
  /** Selected complete index snapshot of the shown folder, or null while none exists. */
  snapshotId(): string | null;
  /** The existing Architect session, when one is known. */
  architect(): ArchitectDescriptor | null;
  generate(request: GenerateRequest): Promise<GenerateOutcome>;
  openArchitect(id: string, replayRequestId?: string): Promise<boolean>;
}

/** Called by the parent's watcher with every passively read status of a structure request. */
export interface VisualizationSink {
  receive(status: ProjectInsightStatus): void;
}
