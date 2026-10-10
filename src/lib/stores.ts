import { writable, derived, get } from "svelte/store";
import { Channel } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { api, type Instance, type PtyChunk } from "./api";

export const instances = writable<Instance[]>([]);
export const instancesById = derived(instances, list => new Map(list.map(i => [i.id, i])));
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
  const previous = get(instances);
  for (const row of list) {
    const old = previous.find(i => i.id === row.id);
    if (old && row.generation !== old.generation) ptyBus.reset(row.id);
    if (row.generation > 0 && streams.get(row.id)?.listeners.size) {
      void ptyBus.ensureAttached(row.id).catch(error => reportError("Terminal attachment", error));
    }
  }
  for (const old of previous) if (!list.some(i => i.id === old.id)) ptyBus.dispose(old.id);
  instances.set(list);
}
export async function refreshInstances(): Promise<void> {
  try { applySnapshot(await api.listInstances()); }
  catch (error) { reportError("Load instances", error); }
}
export async function connectInstances(): Promise<() => void> {
  const unlisten = await listen<Instance[]>("instances", event => applySnapshot(event.payload));
  await refreshInstances();
  return unlisten;
}

async function action(id: string, command: (id: string) => Promise<void>): Promise<boolean> {
  try { await command(id); await refreshInstances(); return true; }
  catch (error) { reportError("Instance action", error); return false; }
}
export const startInstance = (id: string) => action(id, api.startInstance);
export const stopInstance = (id: string) => action(id, api.stopInstance);
export const restartInstance = (id: string) => action(id, api.restartInstance);
export const removeInstance = (id: string) => action(id, api.removeInstance);

export const previewsEnabled = writable(localStorage.getItem("omp-manager.previews") !== "off");
previewsEnabled.subscribe(on => localStorage.setItem("omp-manager.previews", on ? "on" : "off"));
