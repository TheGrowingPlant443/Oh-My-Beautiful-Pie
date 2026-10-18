import type { Instance } from "./api";

export const repoKey = (path: string): string => path.trim().replace(/\\/g, "/").replace(/^\/\/\?\/UNC\//i, "//").replace(/^\/\/\?\//, "").replace(/\/+$/, "").toLowerCase();

/** Project folders of the saved sessions, one per folder (slash style, case and a trailing slash are ignored). */
export function projectFolders(rows: readonly { kind: string; projectPath: string }[]): string[] {
  const seen = new Map<string, string>();
  for (const row of rows) {
    if (row.kind !== "instance") continue;
    const key = repoKey(row.projectPath);
    if (!seen.has(key)) seen.set(key, row.projectPath);
  }
  return [...seen.values()].sort((a, b) => a.toLowerCase().localeCompare(b.toLowerCase()));
}

// Other agents working right now on the same project folder as `current`: sessions on that folder and the
// Code diff / Git tree helpers of those sessions. Subagents are not instances and plan agents are skipped.
export function sameRepoAgents(all: readonly Instance[], current: Instance): Instance[] {
  const byId = new Map(all.map(row => [row.id, row]));
  const projectOf = (row: Instance): Instance | undefined => row.helper ? byId.get(row.helper.target) : row;
  const here = projectOf(current);
  if (!here) return [];
  const key = repoKey(here.projectPath);
  return all
    .filter(row => {
      if (row.id === current.id || row.planTarget !== null) return false;
      if (row.status !== "running" && row.status !== "starting") return false;
      const project = projectOf(row);
      return project !== undefined && repoKey(project.projectPath) === key;
    })
    .sort((a, b) => Number(a.helper !== null) - Number(b.helper !== null) || a.name.localeCompare(b.name));
}
