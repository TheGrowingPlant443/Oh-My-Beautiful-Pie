import { writable, type Readable } from "svelte/store";
import { projectFolder, type GraphifyArtifact, type KnowledgeApi, type KnowledgeCapabilities } from "./knowledge";

// Controller behind GraphifyAccessDialog: the human picks an existing Graphify output folder, the backend registers it only when its
// `.graphify_root` names the shown project, and the artifact is admitted immutably before the view opens. Graphify is never run and no
// HTML is opened. One operation epoch is captured before the native picker; after every await the same epoch and the project that is
// shown *now* are checked, so a late picker/registration/admission for another project (including A to B to A) changes nothing.

export interface GraphifyAccessState { busy: boolean; error: string | null }
export type GraphifyAccessResult = "opened" | "cancelled" | "stale" | "failed";

export interface GraphifyAccessDeps {
  api: Pick<KnowledgeApi, "registerGraphifyOutput" | "openGraphifyArtifact">;
  /** Native folder picker; `null` when the human cancelled. */
  pick(): Promise<string | null>;
  /** The project shown right now (read live, after every await). */
  current(): { folder: string | null; projectKey: string | null };
  /** Replaces the shared capabilities with the backend answer for the project. */
  adopt(capabilities: KnowledgeCapabilities): void;
  /** Opens the graph view of the project (called at most once per accepted operation). */
  navigate(folder: string, artifact: GraphifyArtifact): void;
}

export interface GraphifyAccess extends Readable<GraphifyAccessState> {
  /** Picks, registers and admits for `folder` (identified by `projectKey` when it is already known). */
  choose(folder: string, projectKey: string | null): Promise<GraphifyAccessResult>;
  /** Drops the running operation's right to change anything (project switch, dialog closed, unmount) and clears the message. */
  invalidate(): void;
}

function errorText(error: unknown): string {
  if (error instanceof Error) return error.message;
  return typeof error === "string" ? error : JSON.stringify(error);
}

export function createGraphifyAccess(deps: GraphifyAccessDeps): GraphifyAccess {
  let state: GraphifyAccessState = { busy: false, error: null };
  const store = writable(state);
  const commit = (patch: Partial<GraphifyAccessState>): void => { state = { ...state, ...patch }; store.set(state); };
  let epoch = 0;
  return {
    subscribe: store.subscribe,
    invalidate() {
      epoch++;
      commit({ busy: false, error: null });
    },
    async choose(folder, projectKey) {
      if (state.busy) return "stale";
      const target = projectFolder(folder);
      if (target === null) return "stale";
      const mine = ++epoch;
      // The same epoch, and the very project the operation began for, must still be the one shown.
      const live = (): boolean => {
        if (mine !== epoch) return false;
        const now = deps.current();
        return projectFolder(now.folder) === target && (projectKey === null || now.projectKey === null || now.projectKey === projectKey);
      };
      // A newer epoch (`invalidate`) already reset the state; only a project mismatch under the same epoch releases it here.
      const stale = (): GraphifyAccessResult => {
        if (mine === epoch) commit({ busy: false });
        return "stale";
      };
      commit({ busy: true, error: null });
      try {
        const picked = await deps.pick();
        if (!live()) return stale();
        if (picked === null) {
          commit({ busy: false });
          return "cancelled";
        }
        const capabilities = await deps.api.registerGraphifyOutput(target, picked);
        if (!live()) return stale();
        if (capabilities.folder !== target || capabilities.projectKey === null || (projectKey !== null && capabilities.projectKey !== projectKey)) {
          throw new Error("The answer was for a different project, so nothing was changed in the view.");
        }
        if (!capabilities.graphify.present || (capabilities.graphify.state !== "present" && capabilities.graphify.state !== "unverified")) {
          throw new Error(capabilities.graphify.message ?? "The chosen folder did not provide a usable Graphify output for this project.");
        }
        const artifact = await deps.api.openGraphifyArtifact(target);
        if (!live()) return stale();
        commit({ busy: false, error: null });
        deps.adopt(capabilities);
        deps.navigate(target, artifact);
        return "opened";
      } catch (error) {
        if (!live()) return stale();
        commit({ busy: false, error: errorText(error) });
        return "failed";
      }
    },
  };
}
