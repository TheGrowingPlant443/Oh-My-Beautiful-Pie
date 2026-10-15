<script lang="ts">
  import { Terminal } from "@xterm/xterm";
  import { FitAddon } from "@xterm/addon-fit";
  import { Channel } from "@tauri-apps/api/core";
  import "@xterm/xterm/css/xterm.css";
  import { api, type PtyChunk, type UserShellInfo } from "../api";
  import { reportError } from "../stores";

  let { instanceId, shell }: { instanceId: string; shell: UserShellInfo } = $props();
  let host = $state<HTMLDivElement>();

  // The user's own shell: input goes to this shell id only, never to the agent's terminal or an AI command.
  $effect(() => {
    const element = host;
    const id = instanceId;
    const shellId = shell.shellId;
    if (!element) return;
    const term = new Terminal({
      cols: shell.cols, rows: shell.rows, fontSize: 13, scrollback: 8000, cursorBlink: true,
      fontFamily: '"Cascadia Mono", Consolas, monospace', theme: { background: "#0b1017", foreground: "#dfe6ef" },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(element);
    term.focus();
    let disposed = false;
    let replaying = true;
    const channel = new Channel<PtyChunk>();
    channel.onmessage = data => { if (!disposed) term.write(new Uint8Array(data)); };
    const input = term.onData(data => {
      // Cursor reports are answered by the backend, and replayed history must not be answered a second time.
      if (/^\x1b\[\??\d+;\d+R$/.test(data)) return;
      if (replaying && /^\x1b(?:\[[?>=]?[0-9;]*[Rcnuy]|\])/.test(data)) return;
      void api.writeUserTerminal(id, shellId, data).catch(error => { if (!shell.exited && !disposed) reportError("Send terminal input", error); });
    });
    api.attachUserTerminal(id, shellId, channel)
      .then(() => term.write("", () => { replaying = false; }))
      .catch(error => reportError("Attach terminal", error));
    let frame = 0;
    // The first fit always reports its size: the backend ignores a resize to the size the PTY already has.
    let sentCols = 0;
    let sentRows = 0;
    function layout(): void {
      if (disposed || element!.clientWidth === 0) return;
      const dimensions = fit.proposeDimensions();
      if (!dimensions) return;
      const cols = Math.max(2, Math.min(dimensions.cols, 500));
      const rows = Math.max(2, Math.min(dimensions.rows, 200));
      if (term.cols !== cols || term.rows !== rows) term.resize(cols, rows);
      if (sentCols !== cols || sentRows !== rows) {
        sentCols = cols;
        sentRows = rows;
        void api.resizeUserTerminal(id, shellId, cols, rows).catch(error => reportError("Resize terminal", error));
      }
    }
    const observer = new ResizeObserver(() => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(layout);
    });
    observer.observe(element);
    frame = requestAnimationFrame(layout);
    return () => {
      disposed = true;
      cancelAnimationFrame(frame);
      observer.disconnect();
      input.dispose();
      term.dispose();
    };
  });
</script>

<section class="shell" aria-label="User terminal">
  {#if shell.exited}<p class="dim" role="status">The shell exited{shell.exitCode === null ? "" : ` with code ${shell.exitCode}`}. Close this tab to start a new one.</p>{/if}
  <div class="screen" bind:this={host}></div>
</section>

<style>
  .shell { height: 100%; min-height: 0; display: flex; flex-direction: column; gap: 6px; }
  .screen { flex: 1; min-height: 0; background: #0b1017; overflow: hidden; }
  p { margin: 0; font-size: 12px; }
</style>
