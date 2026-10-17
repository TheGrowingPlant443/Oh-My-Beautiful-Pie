import { writable, derived, get } from "svelte/store";
import { Channel } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { api, type Instance, type PtyChunk, type GlobalConfigInfo, type BrowserInfo, type TerminalOwner } from "./api";
import { parseAppSettings, startsFresh, type AppSettings } from "./settings";
import { defaultTerminalUi, shellKey, type TerminalUi } from "./terminal";

// Every row the backend reports, including plan agents (looked up by id for the plan view).
export const allInstances = writable<Instance[]>([]);
// What dashboards, pickers and counters show: plan and helper agents are never listed as sessions.
export const instances = derived(allInstances, list => list.filter(row => row.planTarget === null && row.helper === null));
export const instancesById = derived(allInstances, list => new Map(list.map(i => [i.id, i])));
export const managerInstance = derived(instances, list => list.find(instance => instance.kind === "manager"));
export const architectInstance = derived(instances, list => list.find(instance => instance.kind === "architect"));
export const globalConfig = writable<GlobalConfigInfo>({ path: null, error: null });
export const knownStandardVersion = writable(0);
export const browserStatuses = writable<ReadonlyMap<string, BrowserInfo>>(new Map());
// Live AI commands, held state and the user shell per agent, as reported by the backend.
export const terminalOwners = writable<ReadonlyMap<string, TerminalOwner>>(new Map());
// Selected pane/tab and Follow per agent (or the Dashboard page); kept so hiding the pane never resets them.
export const terminalUi = writable<ReadonlyMap<string, TerminalUi>>(new Map());
export function updateTerminalUi(key: string, patch: Partial<TerminalUi>): void {
  terminalUi.update(map => new Map(map).set(key, { ...(map.get(key) ?? defaultTerminalUi()), ...patch }));
}
// Starts (or returns) the user's shell. The reply updates the owner at once, so selecting its tab cannot race the event.
export async function openUserTerminal(id: string, select: boolean): Promise<void> {
  try {
    const shell = await api.openUserTerminal(id);
    terminalOwners.update(map => {
      const owner = map.get(id);
      return owner ? new Map(map).set(id, { ...owner, userShell: shell }) : map;
    });
    if (select) updateTerminalUi(id, { tab: shellKey(id) });
  } catch (error) { reportError("Open user terminal", error); }
}
export async function refreshGlobalConfig(): Promise<void> {
  try { globalConfig.set(await api.getGlobalConfig()); }
  catch (error) { globalConfig.set({ path: null, error: errorText(error) }); }
}
export const toasts = writable<{ id: number; message: string }[]>([]);
let toastId = 0;
export function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
export function reportError(context: string, error: unknown): void {
  const id = ++toastId;
  toasts.update(list => [...list.slice(-4), { id, message: `${context}: ${errorText(error)}` }]);
  setTimeout(() => dismissToast(id), 8000);
}
export function dismissToast(id: number): void { toasts.update(list => list.filter(t => t.id !== id)); }

const LIMIT = 256 * 1024;
type Listener = (bytes: Uint8Array | null) => void;
interface Stream {
  chunks: Uint8Array[];
  bytes: number;
  listeners: Set<Listener>;
  attached: Promise<void> | null;
  token: number;
}
const streams = new Map<string, Stream>();
function stream(id: string): Stream {
  let state = streams.get(id);
  if (!state) {
    state = { chunks: [], bytes: 0, listeners: new Set(), attached: null, token: 0 };
    streams.set(id, state);
  }
  return state;
}

export const ptyBus = {
  subscribe(id: string, listener: Listener, interactive = false): () => void {
    const state = stream(id);
    if (interactive) state.listeners = new Set([listener, ...state.listeners]);
    else state.listeners.add(listener);
    for (const chunk of state.chunks) listener(chunk);
    return () => state.listeners.delete(listener);
  },
  isResponder(id: string, listener: Listener): boolean {
    return stream(id).listeners.values().next().value === listener;
  },
  ensureAttached(id: string): Promise<void> {
    const state = stream(id);
    if (state.attached) return state.attached;
    const token = state.token;
    const channel = new Channel<PtyChunk>();
    channel.onmessage = data => {
      if (state.token !== token) return;
      const bytes = new Uint8Array(data);
      state.chunks.push(bytes);
      state.bytes += bytes.length;
      while (state.bytes > LIMIT && state.chunks.length > 1) state.bytes -= state.chunks.shift()!.length;
      if (state.bytes > LIMIT) {
        state.chunks[0] = state.chunks[0].slice(-LIMIT);
        state.bytes = LIMIT;
      }
      for (const listener of state.listeners) listener(bytes);
    };
    state.attached = api.attachPty(id, channel).catch(error => {
      if (state.token === token) state.attached = null;
      throw error;
    });
    return state.attached;
  },
  reset(id: string): void {
    const state = stream(id);
    state.token++;
    state.attached = null;
    state.chunks = [];
    state.bytes = 0;
    for (const listener of state.listeners) listener(null);
  },
  dispose(id: string): void { this.reset(id); streams.delete(id); },
};

function applySnapshot(list: Instance[]): void {
  const previous = get(allInstances);
  for (const row of list) {
    const old = previous.find(i => i.id === row.id);
    if (old && row.generation !== old.generation) ptyBus.reset(row.id);
    if (row.generation > 0 && streams.get(row.id)?.listeners.size) {
      void ptyBus.ensureAttached(row.id).catch(error => reportError("Terminal attachment", error));
    }
  }
  for (const old of previous) if (!list.some(i => i.id === old.id)) ptyBus.dispose(old.id);
  const known = new Set(list.map(row => row.id));
  const owners = get(terminalOwners);
  if ([...owners.keys()].some(id => !known.has(id))) terminalOwners.set(new Map([...owners].filter(([id]) => known.has(id))));
  allInstances.set(list);
}
export async function refreshInstances(): Promise<void> {
  try { applySnapshot(await api.listInstances()); }
  catch (error) { reportError("Load instances", error); }
}
export async function connectInstances(): Promise<() => void> {
  let disposed = false;
  let browserEventSeen = false;
  const terminalEventsSeen = new Set<string>();
  const unlisten = await listen<Instance[]>("instances", event => { if (!disposed) applySnapshot(event.payload); });
  let unlistenBrowsers: (() => void) | undefined;
  let unlistenTerminals: (() => void) | undefined;
  function cleanup(): void { disposed = true; unlisten(); unlistenBrowsers?.(); unlistenTerminals?.(); }
  try {
    unlistenBrowsers = await listen<BrowserInfo[]>("browser_state", event => {
      browserEventSeen = true;
      if (!disposed) browserStatuses.set(new Map(event.payload.map(info => [info.instanceId, info])));
    });
    // An event is newer than the initial listing that may be requested around it.
    unlistenTerminals = await listen<TerminalOwner>("terminal_state", event => {
      terminalEventsSeen.add(event.payload.instanceId);
      if (!disposed) terminalOwners.update(map => new Map(map).set(event.payload.instanceId, event.payload));
    });
    const [list, browsers, terminals] = await Promise.all([api.listInstances(), api.listBrowsers(), api.listAllTerminals()]);
    if (!disposed) {
      applySnapshot(list);
      if (!browserEventSeen) browserStatuses.set(new Map(browsers.map(info => [info.instanceId, info])));
      terminalOwners.update(map => {
        const next = new Map(map);
        for (const owner of terminals) if (!terminalEventsSeen.has(owner.instanceId)) next.set(owner.instanceId, owner);
        return next;
      });
    }
    await refreshGlobalConfig();
    return cleanup;
  } catch (error) {
    cleanup();
    throw error;
  }
}

async function action(id: string, command: (id: string) => Promise<void>): Promise<boolean> {
  try { await command(id); await refreshInstances(); return true; }
  catch (error) { reportError("Instance action", error); return false; }
}
export function startInstance(id: string, keepSession = false): Promise<boolean> {
  const fresh = startsFresh(get(appSettings), get(instancesById).get(id), keepSession);
  return action(id, id => api.startInstance(id, fresh));
}
export const stopInstance = (id: string) => action(id, api.stopInstance);
export const restartInstance = (id: string) => action(id, api.restartInstance);
export const removeInstance = (id: string) => action(id, api.removeInstance);
export function setManagerOwner(id: string, managerId: string | null): Promise<boolean> {
  return action(id, id => api.setManagerOwner(id, managerId));
}
export const returnToManager = (id: string) => action(id, api.returnToManager);

export const previewsEnabled = writable(localStorage.getItem("omp-manager.previews") !== "off");
previewsEnabled.subscribe(on => localStorage.setItem("omp-manager.previews", on ? "on" : "off"));

function loadSettings(): AppSettings {
  try { return parseAppSettings(localStorage.getItem("omp-manager.settings"), localStorage.getItem("omp-manager.show-manager")); }
  catch (error) { reportError("Load settings", error); return parseAppSettings(null, null); }
}
export const appSettings = writable<AppSettings>(loadSettings());
let settingsSaveFailed = false;
appSettings.subscribe(settings => {
  try {
    localStorage.setItem("omp-manager.settings", JSON.stringify(settings));
    localStorage.removeItem("omp-manager.show-manager");
    settingsSaveFailed = false;
  } catch (error) {
    if (!settingsSaveFailed) reportError("Save settings", error);
    settingsSaveFailed = true;
  }
});
