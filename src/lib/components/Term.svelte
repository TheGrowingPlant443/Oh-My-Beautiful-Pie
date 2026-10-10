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

  $effect(() => {
    const id = instanceId;
    const element = host;
    const run = generation;
    if (!element) return;
    const term = new Terminal({
      cols: untrack(() => cols), rows: untrack(() => rows), fontSize,
      fontFamily: '"Cascadia Mono", Consolas, monospace', scrollback,
      cursorBlink: interactive,
      theme: { background: "#0b1017", foreground: "#dfe6ef" },
    });
    terminal = term;
    const fit = new FitAddon();
    if (interactive) term.loadAddon(fit);
    term.open(element);
    // disableStdin also blocks xterm's protocol replies. Block physical keys instead.
    if (!interactive) term.attachCustomKeyEventHandler(() => false);
    let disposed = false;
    let replaying = true;
    const sink = (bytes: Uint8Array | null) => {
      if (!bytes) { term.reset(); return; }
      term.write(bytes);
    };
    const input = term.onData(data => {
      const protocolReply = /^\x1b(?:\[[?>=]?[0-9;]*[Rcnuy]|\])/.test(data);
      if (run === 0 || !ptyBus.isResponder(id, sink)) return;
      if ((replaying && protocolReply) || (!interactive && !protocolReply)) return;
      void api.writePty(id, data).catch(error => reportError("Send terminal input", error));
    });
    const unsubscribe = ptyBus.subscribe(id, sink, interactive);
    term.write("", () => { replaying = false; });
    if (run > 0) void ptyBus.ensureAttached(id).catch(error => reportError("Attach terminal", error));
    let frame = 0;
    function layout(): void {
      if (disposed || element!.clientWidth === 0) return;
      if (interactive) {
        fit.fit();
        term.resize(Math.min(term.cols, 500), Math.min(term.rows, 200));
        if (run > 0) void api.resizePty(id, term.cols, term.rows).catch(error => reportError("Resize terminal", error));
        term.focus();
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
    observer.observe(interactive ? element : element.parentElement!);
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
