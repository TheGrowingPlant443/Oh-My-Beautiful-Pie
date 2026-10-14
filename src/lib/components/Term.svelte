<script lang="ts">
  import { Terminal } from "@xterm/xterm";
  import { FitAddon } from "@xterm/addon-fit";
  import { untrack } from "svelte";
  import "@xterm/xterm/css/xterm.css";
  import { api } from "../api";
  import { ptyBus, reportError } from "../stores";

  interface Props {
    instanceId: string;
    interactive?: boolean;
    cols?: number;
    rows?: number;
    generation?: number;
    fontSize?: number;
    scrollback?: number;
  }
  let { instanceId, interactive = false, cols = 120, rows = 32, generation = 0, fontSize = 13, scrollback = 4000 }: Props = $props();
  let host = $state<HTMLDivElement>();
  let terminal: Terminal | undefined;
  let relayout: (() => void) | undefined;
  const terminalId = $derived(instanceId);
  const processGeneration = $derived(generation);
  const terminalMode = $derived(interactive);

  $effect(() => {
    const id = terminalId;
    const element = host;
    const run = processGeneration;
    const mode = terminalMode;
    if (!element) return;
    // Snapshot metadata must not dispose and recreate the terminal.
    const term = untrack(() => new Terminal({
      cols, rows, fontSize,
      fontFamily: '"Cascadia Mono", Consolas, monospace', scrollback,
      cursorBlink: mode,
      theme: { background: "#0b1017", foreground: "#dfe6ef" },
    }));
    terminal = term;
    const fit = new FitAddon();
    if (mode) term.loadAddon(fit);
    term.open(element);
    if (mode) term.focus();
    // disableStdin also blocks xterm's protocol replies. Block physical keys instead.
    if (!mode) term.attachCustomKeyEventHandler(() => false);
    let disposed = false;
    let replaying = true;
    const sink = (bytes: Uint8Array | null) => {
      if (!bytes) { term.reset(); return; }
      term.write(bytes);
    };
    const input = term.onData(data => {
      // Cursor reports are answered by Rust even when no xterm view is mounted.
      if (/^\x1b\[\??\d+;\d+R$/.test(data)) return;
      const protocolReply = /^\x1b(?:\[[?>=]?[0-9;]*[Rcnuy]|\])/.test(data);
      if (run === 0 || !ptyBus.isResponder(id, sink)) return;
      if ((replaying && protocolReply) || (!mode && !protocolReply)) return;
      void api.writePty(id, data).catch(error => reportError("Send terminal input", error));
    });
    const unsubscribe = ptyBus.subscribe(id, sink, mode);
    term.write("", () => { replaying = false; });
    if (run > 0) void ptyBus.ensureAttached(id).catch(error => reportError("Attach terminal", error));
    let frame = 0;
    // 0 forces the first layout after mount to report the fitted size: the snapshot's cols/rows can be stale, and the
    // backend ignores resizes to the size the PTY already has.
    let sentCols = 0;
    let sentRows = 0;
    function layout(): void {
      if (disposed || element!.clientWidth === 0) return;
      if (mode) {
        const dimensions = fit.proposeDimensions();
        if (!dimensions) return;
        const nextCols = Math.max(2, Math.min(dimensions.cols, 500));
        const nextRows = Math.max(2, Math.min(dimensions.rows, 200));
        if (term.cols !== nextCols || term.rows !== nextRows) term.resize(nextCols, nextRows);
        if (run > 0 && (sentCols !== nextCols || sentRows !== nextRows)) {
          sentCols = nextCols;
          sentRows = nextRows;
          void api.resizePty(id, nextCols, nextRows).catch(error => reportError("Resize terminal", error));
        }
      } else {
        const screen = element!.querySelector<HTMLElement>(".xterm-screen");
        if (!screen || screen.offsetWidth === 0) return;
        const scale = element!.parentElement!.clientWidth / screen.offsetWidth;
        element!.style.width = `${screen.offsetWidth}px`;
        element!.style.transform = `scale(${scale})`;
        element!.parentElement!.style.height = `${screen.offsetHeight * scale}px`;
      }
    }
    relayout = layout;
    const observer = new ResizeObserver(() => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(layout);
    });
    observer.observe(mode ? element : element.parentElement!);
    frame = requestAnimationFrame(layout);
    return () => {
      disposed = true;
      cancelAnimationFrame(frame);
      observer.disconnect();
      unsubscribe();
      input.dispose();
      term.dispose();
      if (terminal === term) { terminal = undefined; relayout = undefined; }
    };
  });

  $effect(() => {
    const width = cols;
    const height = rows;
    if (terminal && !interactive && (terminal.cols !== width || terminal.rows !== height)) {
      terminal.resize(width, height);
      requestAnimationFrame(() => relayout?.());
    }
  });
</script>

<div class="viewport" class:interactive>
  <div class="terminal" bind:this={host}></div>
</div>

<style>
  .viewport { width: 100%; overflow: hidden; min-height: 0; background: #0b1017; }
  .terminal { transform-origin: top left; }
  .viewport.interactive, .interactive .terminal { height: 100%; width: 100%; }
</style>
