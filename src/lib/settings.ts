import type { Instance } from "./api";

export type StartupTarget = "manager" | "project" | "latest" | "dashboard";
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
}

export function parseAppSettings(raw: string | null, legacyShowManager: string | null): AppSettings {
  const defaults: AppSettings = {
    version: 1, startupTarget: "manager", startupProjectId: null, latestProjectId: null,
    showPreviousProjects: false, showManagerTile: legacyShowManager !== "off", previewSize: 640, autoStart: true,
    browserHeadless: false, browserEmbedded: true, terminalAutoOpen: false, terminalCreateUser: true, alwaysNewSession: false,
  };
  let data: unknown;
  try { data = raw === null ? null : JSON.parse(raw); }
  catch { return defaults; }
  if (!data || typeof data !== "object" || Array.isArray(data)) return defaults;
  const value = data as Record<string, unknown>;
  if (value.version !== 1) return defaults;
  return {
    version: 1,
    startupProjectId: typeof value.startupProjectId === "string" && value.startupProjectId.trim() ? value.startupProjectId : null,
    latestProjectId: typeof value.latestProjectId === "string" && value.latestProjectId.trim() ? value.latestProjectId : null,
    startupTarget: value.startupTarget === "project" || value.startupTarget === "latest" || value.startupTarget === "dashboard" ? value.startupTarget : "manager",
    showPreviousProjects: typeof value.showPreviousProjects === "boolean" ? value.showPreviousProjects : defaults.showPreviousProjects,
    showManagerTile: typeof value.showManagerTile === "boolean" ? value.showManagerTile : defaults.showManagerTile,
    previewSize: value.previewSize === 420 || value.previewSize === 1000 ? value.previewSize : 640,
    autoStart: typeof value.autoStart === "boolean" ? value.autoStart : defaults.autoStart,
    browserHeadless: typeof value.browserHeadless === "boolean" ? value.browserHeadless : defaults.browserHeadless,
    browserEmbedded: typeof value.browserEmbedded === "boolean" ? value.browserEmbedded : defaults.browserEmbedded,
    terminalAutoOpen: typeof value.terminalAutoOpen === "boolean" ? value.terminalAutoOpen : defaults.terminalAutoOpen,
    terminalCreateUser: typeof value.terminalCreateUser === "boolean" ? value.terminalCreateUser : defaults.terminalCreateUser,
    alwaysNewSession: typeof value.alwaysNewSession === "boolean" ? value.alwaysNewSession : defaults.alwaysNewSession,
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
