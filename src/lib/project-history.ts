import { repoKey } from "./repo-agents";

export const PROJECT_HISTORY_KEY = "omp-manager.project-folders";
/** A folder the app computed for shared ownership (not a project). Live ordinary project folders are never computed. */
export function isHiddenComputed(folder: string, computed: readonly string[], live: readonly string[]): boolean {
  const key = repoKey(folder);
  return computed.some(path => repoKey(path) === key) && !live.some(path => repoKey(path) === key);
}
/** Choices for a folder selector: computed shared folders appear only when they are live project folders or the current selection. */
export function folderOptions(history: readonly string[], liveFolders: readonly string[], selected: string, computed: readonly string[] = []): string[] {
  const hidden = new Set(computed.map(repoKey));
  const exempt = new Set([repoKey(selected), ...liveFolders.map(repoKey)]);
  const seen = new Set<string>();
  const result: string[] = [];
  for (const value of [selected, ...history, ...liveFolders]) {
    const folder = value.trim();
    const key = repoKey(folder);
    if (!folder || seen.has(key) || (hidden.has(key) && !exempt.has(key))) continue;
    seen.add(key);
    result.push(folder);
  }
  return result;
}
/** Whether a folder may enter the stored history: explicit choices and live projects always, computed shared folders otherwise never. */
export function isRememberable(folder: string, computed: readonly string[], live: readonly string[], explicit: boolean): boolean {
  return explicit || !isHiddenComputed(folder, computed, live);
}
/** Folders to record when a coordinator's working directory moved: bootstrap temporary and computed shared folders are skipped. */
export function coordinatorMoveFolders(previous: string, next: string, temporary: readonly string[], computed: readonly string[], live: readonly string[]): string[] {
  const temporaryKeys = temporary.map(repoKey);
  return [previous, next].filter(folder => !temporaryKeys.includes(repoKey(folder)) && isRememberable(folder, computed, live, false));
}
export function rememberFolder(history: readonly string[], folder: string): string[] {
  return folderOptions(history, [], folder);
}
export function parseProjectHistory(raw: string | null): { folders: string[]; error: string | null } {
  if (raw === null) return { folders: [], error: null };
  try {
    const value = JSON.parse(raw);
    if (!value || value.version !== 1 || !Array.isArray(value.folders) || !value.folders.every((path: unknown) => typeof path === "string")) throw new Error("Invalid or unsupported project folder history");
    return { folders: folderOptions(value.folders, [], ""), error: null };
  } catch (error) { return { folders: [], error: `Project folder history could not be persisted: ${String(error)}` }; }
}

/** Merge concurrent storage before writing; malformed data is never overwritten. Seeds that are computed shared folders (and not live projects or the selection) are not added; stored entries are kept as-is and hidden by `folderOptions`. */
export function persistProjectHistory(storage: Pick<Storage, "getItem" | "setItem">, history: readonly string[], seeds: readonly string[] = [], selected = "", computed: readonly string[] = [], live: readonly string[] = []): { folders: string[]; error: string | null } {
  let folders = folderOptions(history, seeds.filter(seed => isRememberable(seed, computed, live, repoKey(seed) === repoKey(selected))), selected);
  try {
    const stored = parseProjectHistory(storage.getItem(PROJECT_HISTORY_KEY));
    folders = folderOptions(folders, stored.folders, "");
    if (stored.error) return { folders, error: stored.error };
    storage.setItem(PROJECT_HISTORY_KEY, JSON.stringify({ version: 1, folders }));
    return { folders, error: null };
  } catch (error) { return { folders, error: `Project folder history could not be persisted: ${String(error)}` }; }
}
