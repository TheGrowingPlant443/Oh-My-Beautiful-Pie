import type { Instance } from "./api";

export type StartupTarget = "manager" | "architect" | "project" | "latest" | "dashboard";
export type ProjectLanguage = "maven" | "npm" | "pypi" | "cargo";
export type ProjectRole = "backend" | "frontend";
const LANGUAGES: ReadonlySet<ProjectLanguage> = new Set(["maven", "npm", "pypi", "cargo"]);
const ROLES: ReadonlySet<ProjectRole> = new Set(["backend", "frontend"]);
const STARTUP_TARGETS: ReadonlySet<unknown> = new Set(["architect", "project", "latest", "dashboard"]);
export interface AppSettings {
  version: 1;
  startupTarget: StartupTarget;
  startupProjectId: string | null;
  latestProjectId: string | null;
  showPreviousProjects: boolean;
  showManagerTile: boolean;
  previewSize: 420 | 640 | 1000;
  autoStart: boolean;
  browserHeadless: boolean;
  browserEmbedded: boolean;
  /** Show the Terminal pane by itself when the AI starts a command (never opens another session or page). */
  terminalAutoOpen: boolean;
  /** Opening the Terminal pane starts the user's own shell (Tab 0). */
  terminalCreateUser: boolean;
  /** Starting an agent begins a new session instead of resuming the previous one. */
  alwaysNewSession: boolean;
  packageLookupGate: boolean;
  manifestFilter: boolean;
  filterLanguages: ProjectLanguage[];
  aiFilter: boolean;
  filterRoles: ProjectRole[];
}

const flag = (value: Record<string, unknown>, key: keyof AppSettings, fallback: boolean): boolean => (typeof value[key] === "boolean" ? (value[key] as boolean) : fallback);
const projectId = (value: unknown): string | null => typeof value === "string" && value.trim() ? value : null;
const choices = <T>(value: unknown, allowed: ReadonlySet<T>): T[] => Array.isArray(value) ? [...new Set(value.filter((item): item is T => allowed.has(item)))] : [];

export function parseAppSettings(raw: string | null, legacyShowManager: string | null): AppSettings {
  const defaults: AppSettings = {
    version: 1, startupTarget: "manager", startupProjectId: null, latestProjectId: null,
    showPreviousProjects: false, showManagerTile: legacyShowManager !== "off", previewSize: 640, autoStart: true,
    browserHeadless: false, browserEmbedded: true, terminalAutoOpen: false, terminalCreateUser: true, alwaysNewSession: false,
    packageLookupGate: false, manifestFilter: false, filterLanguages: [], aiFilter: false, filterRoles: [],
  };
  let data: unknown;
  try { data = raw === null ? null : JSON.parse(raw); }
  catch { return defaults; }
  if (!data || typeof data !== "object" || Array.isArray(data)) return defaults;
  const value = data as Record<string, unknown>;
  if (value.version !== 1) return defaults;
  return {
    version: 1,
    startupProjectId: projectId(value.startupProjectId),
    latestProjectId: projectId(value.latestProjectId),
    startupTarget: STARTUP_TARGETS.has(value.startupTarget) ? value.startupTarget as StartupTarget : "manager",
    showPreviousProjects: flag(value, "showPreviousProjects", defaults.showPreviousProjects),
    showManagerTile: flag(value, "showManagerTile", defaults.showManagerTile),
    previewSize: value.previewSize === 420 || value.previewSize === 1000 ? value.previewSize : 640,
    autoStart: flag(value, "autoStart", defaults.autoStart),
    browserHeadless: flag(value, "browserHeadless", defaults.browserHeadless),
    browserEmbedded: flag(value, "browserEmbedded", defaults.browserEmbedded),
    terminalAutoOpen: flag(value, "terminalAutoOpen", defaults.terminalAutoOpen),
    terminalCreateUser: flag(value, "terminalCreateUser", defaults.terminalCreateUser),
    alwaysNewSession: flag(value, "alwaysNewSession", defaults.alwaysNewSession),
    packageLookupGate: flag(value, "packageLookupGate", defaults.packageLookupGate),
    manifestFilter: flag(value, "manifestFilter", defaults.manifestFilter),
    filterLanguages: choices(value.filterLanguages, LANGUAGES),
    aiFilter: flag(value, "aiFilter", defaults.aiFilter),
    filterRoles: choices(value.filterRoles, ROLES),
  };
}

// Helper and plan agents keep their conversation; an explicitly chosen session is always resumed.
export function startsFresh(settings: AppSettings, row: Pick<Instance, "planTarget" | "helper"> | undefined, keepSession: boolean): boolean {
  return settings.alwaysNewSession && !keepSession && !!row && row.planTarget === null && row.helper === null;
}

export function resolveStartupTarget(settings: AppSettings, list: readonly Instance[]): {
  instanceId: string | null; missingProject: boolean;
} {
  if (settings.startupTarget === "dashboard") return { instanceId: null, missingProject: false };
  const manager = list.find(row => row.kind === "manager");
  if (settings.startupTarget === "manager") return { instanceId: manager?.id ?? null, missingProject: false };
  if (settings.startupTarget === "architect") return { instanceId: list.find(row => row.kind === "architect")?.id ?? null, missingProject: false };
  const id = settings.startupTarget === "project" ? settings.startupProjectId : settings.latestProjectId;
  const project = list.find(row => row.id === id && row.kind === "instance");
  return { instanceId: project?.id ?? manager?.id ?? null, missingProject: !project };
}

export function dashboardInstances(
  list: readonly Instance[], settings: AppSettings,
  restoredIds: ReadonlySet<string>, openedIds: ReadonlySet<string>,
): Instance[] {
  return list.filter(row => row.kind === "manager" ? settings.showManagerTile
    : settings.showPreviousProjects || !restoredIds.has(row.id) || openedIds.has(row.id));
}
