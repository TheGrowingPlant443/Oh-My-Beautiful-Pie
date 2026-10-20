import { readable, type Readable } from "svelte/store";

// Whether the main window can actually be seen: native visible and not minimized, and the document not hidden.
// A WebView keeps reporting `visible` while the native window is minimized, so both are consulted. Reading never throws:
// when the native state cannot be read the window counts as visible. The value is false only until the first read settles.

export interface NativeWindowState { visible: boolean; minimized: boolean }
export interface WindowVisibilitySource {
  nativeState(): Promise<NativeWindowState>;
  /** Calls `onChange` after native transitions (resize, focus, move, scale); resolves to the unsubscribe. */
  watch(onChange: () => void): Promise<() => void>;
  documentHidden(): boolean;
  watchDocument(onChange: () => void): () => void;
}

export function createWindowVisibility(source: WindowVisibilitySource): Readable<boolean> {
  return readable(false, set => {
    let alive = true;
    let sequence = 0;
    let native: boolean | null = null;
    const publish = (): void => {
      if (native === null) return;
      let hidden = false;
      try { hidden = source.documentHidden(); } catch { /* a document that cannot be read counts as shown */ }
      set(native && !hidden);
    };
    async function evaluate(): Promise<void> {
      const mine = ++sequence;
      let next = true;
      try { const state = await source.nativeState(); next = state.visible && !state.minimized; } catch { next = true; }
      // Only the newest evaluation counts: an older read may finish after a newer transition.
      if (!alive || mine !== sequence) return;
      native = next;
      publish();
    }
    let stopDocument: () => void = () => {};
    try { stopDocument = source.watchDocument(() => { publish(); void evaluate(); }); } catch { /* native events still drive it */ }
    let stopNative: (() => void) | null = null;
    void Promise.resolve().then(() => source.watch(() => void evaluate())).then(stop => {
      if (alive) stopNative = stop; else stop();
    }).catch(() => {});
    void evaluate();
    return () => { alive = false; stopDocument(); stopNative?.(); };
  });
}
