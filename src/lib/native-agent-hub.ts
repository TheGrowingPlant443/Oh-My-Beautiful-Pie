import type { Instance } from "./api";

export const NATIVE_HUB_UNAVAILABLE = "native_agent_hub_unavailable: Reopen this session with a compatible OMP version.";

const NATIVE_HUB_ERRORS: ReadonlyArray<readonly [code: string, message: string]> = [
  ["native_ui_busy", "Another prompt is open in this session. Answer or dismiss it, then try again."],
  ["native_agent_hub_unavailable", "This session's OMP version does not support the native Agent Hub. Reopen the session with a compatible OMP version."],
  ["stale_generation", "The session was restarted or changed while opening the Agent Hub. Try again."],
  ["bridge_not_ready", "The session is still starting. Wait a moment, then try again."],
  ["not_controlled", "This session is controlled by another agent, so the Agent Hub cannot be opened from here."],
  ["manager bridge not initialized", "The app's session bridge is not ready yet. Try again in a moment."],
];

/** Maps a native Agent Hub open failure to human wording while keeping the raw code; unknown errors pass through unchanged. */
export function describeNativeHubError(error: unknown): { code: string | null; message: string } {
  const text = error instanceof Error ? error.message : String(error);
  for (const [code, message] of NATIVE_HUB_ERRORS) {
    const start = text.indexOf(code);
    if (start < 0) continue;
    const before = text[start - 1];
    const after = text[start + code.length];
    if ((before === undefined || !/\w/.test(before)) && (after === undefined || !/\w/.test(after))) return { code, message };
  }
  return { code: null, message: text };
}

export function nativeHubDisabled(instance: Instance): boolean {
  return instance.status === "starting" || (instance.status === "running" && (!instance.bridgeReady || instance.capabilities?.agentHub !== true));
}

export async function awaitNativeAgentHub(options: {
  current: () => boolean;
  instance: () => Instance | undefined;
  terminalReady: (generation: number) => boolean;
  open: (generation: number) => Promise<void>;
  wait: () => Promise<void>;
  now?: () => number;
}): Promise<void> {
  const now = options.now ?? Date.now;
  const generation = options.instance()?.generation;
  if (!options.current() || generation === undefined) return;
  const deadline = now() + 30_000;
  while (options.current()) {
    const instance = options.instance();
    if (instance?.generation !== generation) return;
    if (instance.status === "stopped" || instance.status === "error") throw new Error("The session did not become ready. Open its terminal to inspect the startup error.");
    if (instance.bridgeReady && instance.capabilities?.agentHub !== true) throw new Error(NATIVE_HUB_UNAVAILABLE);
    if (instance.bridgeReady && options.terminalReady(generation) && instance.capabilities?.agentHub === true) {
      await options.open(generation);
      return;
    }
    if (now() >= deadline) throw new Error("The native Agent Hub did not become ready. The session terminal remains available.");
    await options.wait();
  }
}
