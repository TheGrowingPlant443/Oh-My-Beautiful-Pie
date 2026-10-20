import { getCurrentWindow } from "@tauri-apps/api/window";
import type { Readable } from "svelte/store";
import { createWindowVisibility, type WindowVisibilitySource } from "./window-visibility-core";

const tauriSource: WindowVisibilitySource = {
  async nativeState() {
    const window = getCurrentWindow();
    const [visible, minimized] = await Promise.all([window.isVisible(), window.isMinimized()]);
    return { visible, minimized };
  },
  async watch(onChange) {
    const window = getCurrentWindow();
    const results = await Promise.allSettled([window.onResized(onChange), window.onFocusChanged(onChange), window.onMoved(onChange), window.onScaleChanged(onChange)]);
    const stops = results.flatMap(result => result.status === "fulfilled" ? [result.value] : []);
    return () => { for (const stop of stops) stop(); };
  },
  documentHidden: () => typeof document !== "undefined" && document.visibilityState === "hidden",
  watchDocument(onChange) {
    if (typeof document === "undefined") return () => {};
    document.addEventListener("visibilitychange", onChange);
    return () => document.removeEventListener("visibilitychange", onChange);
  },
};

/** True while the main window is visible (native), not minimized, and the document is shown; true when it cannot be read. */
export const windowVisible: Readable<boolean> = createWindowVisibility(tauriSource);
