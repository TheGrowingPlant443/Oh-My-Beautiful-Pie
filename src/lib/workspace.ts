import { repoKey } from "./repo-agents";
import type { WorkspaceChange } from "./api";

/** Pending intent is not yet effective coordinator context. */
export function workspacePending(change: Pick<WorkspaceChange, "status">): boolean {
  return change.status === "pending" || change.status === "applying";
}

/** A page entry captures this once; background context changes never replace manual selection. */
export function architectEntryFolder(committedFolder: string | undefined, temporaryFolders: readonly string[]): string | undefined {
  if (!committedFolder || temporaryFolders.some(folder => repoKey(folder) === repoKey(committedFolder))) return undefined;
  return committedFolder;
}
