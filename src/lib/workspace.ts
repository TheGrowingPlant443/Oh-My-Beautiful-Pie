import { repoKey } from "./repo-agents";
import type { Instance, WorkspaceChange } from "./api";

/** Pending intent is not yet effective coordinator context. */
export function workspacePending(change: Pick<WorkspaceChange, "status">): boolean {
  return change.status === "pending" || change.status === "applying";
}

/** A page entry captures this once; background context changes never replace manual selection. */
export function architectEntryFolder(committedFolder: string | undefined, temporaryFolders: readonly string[]): string | undefined {
  if (!committedFolder || temporaryFolders.some(folder => repoKey(folder) === repoKey(committedFolder))) return undefined;
  return committedFolder;
}

/** Opening a selector is read-only: capture the human's intended project, never an execution worktree. */
export function workspaceEntryFolder(instance: Pick<Instance, "id" | "projectPath" | "projectFolder"> | undefined, changes: readonly WorkspaceChange[], temporaryFolders: readonly string[]): string | undefined {
  if (!instance) return undefined;
  let folder = instance.projectFolder ?? instance.projectPath;
  for (let index = changes.length - 1; index >= 0; index--) {
    const change = changes[index];
    if (change.coordinatorId === instance.id && workspacePending(change)) {
      folder = change.targetFolder;
      break;
    }
  }
  return architectEntryFolder(folder, temporaryFolders);
}

/** Linked plan/helper sessions read analysis for their target's logical project. */
export function qualityEntryFolder(source: Instance, instances: ReadonlyMap<string, Instance>, changes: readonly WorkspaceChange[], temporaryFolders: readonly string[], fallbackFolder: string | undefined): string | undefined {
  const targetId = source.helper?.target ?? source.planTarget;
  const subject = targetId ? instances.get(targetId) : source;
  if (!subject) return source.helper?.repo;
  if (subject.kind === "manager" || subject.kind === "architect") {
    return workspaceEntryFolder(subject, changes, temporaryFolders) ?? fallbackFolder;
  }
  return subject.projectFolder ?? subject.projectPath;
}
