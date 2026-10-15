import type { Instance } from "./api";

export const repoKey = (path: string): string => path.replace(/\\/g, "/").replace(/\/+$/, "").toLowerCase();

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
