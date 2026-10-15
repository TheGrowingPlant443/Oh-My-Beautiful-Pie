<script lang="ts">
  import { Terminal } from "@xterm/xterm";
  import { FitAddon } from "@xterm/addon-fit";
  import { Channel } from "@tauri-apps/api/core";
  import "@xterm/xterm/css/xterm.css";
  import { api, type CommandOutput, type CommandSummary, type TerminalOwner } from "../api";
  import { errorText, reportError } from "../stores";
  import { commandLabel, outputWrite } from "../terminal";

  let { owner, command }: { owner: TerminalOwner; command: CommandSummary } = $props();
  let host = $state<HTMLDivElement>();
  let truncated = $state(false);
  let attachError = $state<string | null>(null);
  let killing = $state(false);

  // The AI process owns its size and input; this view only displays the tail the observer reports.
  $effect(() => {
    const element = host;
    const { instanceId, generation } = owner;
    const { sessionId, executionId } = command;
    if (!element) return;
    const term = new Terminal({
      disableStdin: true, convertEol: true, cursorBlink: false, fontSize: 13, scrollback: 10000,
      fontFamily: '"Cascadia Mono", Consolas, monospace', theme: { background: "#0b1017", foreground: "#dfe6ef" },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(element);
    let disposed = false;
    let shown = "";
    const channel = new Channel<CommandOutput>();
    channel.onmessage = frame => {
      if (disposed) return;
      const write = outputWrite(shown, frame);
      if (write.reset) term.reset();
      term.write(write.data);
      shown = frame.text;
      truncated = frame.truncated;
    };
    attachError = null;
    api.attachCommandOutput(instanceId, generation, sessionId, executionId, channel).catch(error => {
      if (!disposed) attachError = errorText(error);
    });
    let frame = 0;
    const observer = new ResizeObserver(() => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => { if (!disposed && element.clientWidth > 0) fit.fit(); });
    });
    observer.observe(element);
    return () => {
      disposed = true;
      cancelAnimationFrame(frame);
      observer.disconnect();
      term.dispose();
    };
  });

  async function kill(): Promise<void> {
    killing = true;
    try { await api.killTerminalCommand(owner.instanceId, owner.generation, command.sessionId, command.executionId); }
    catch (error) { reportError("Kill process", error); }
    finally { killing = false; }
  }
</script>

<section class="output" aria-label={`Command output: ${commandLabel(command)}`}>
  <header>
    <span class="pill accent">{command.kind === "bash" ? "Bash" : command.kind === "js" ? "JavaScript" : "Python"}</span>
    <span class="pill" class:warn={command.status === "stopping"}>{command.status}</span>
    <span class="title" title={command.preview}>{commandLabel(command)}</span>
    <span class="spacer"></span>
    <button class="danger" disabled={killing || command.status === "stopping" || !owner.supported} title="Stop this command and interrupt the agent, like pressing Esc. It will not run more commands until you send a new prompt." onclick={kill}>Kill process</button>
  </header>
  {#if attachError}<p class="err" role="alert">{attachError}</p>{/if}
  {#if truncated}<p class="dim note">Showing the last 256 KiB of output.</p>{/if}
  <div class="screen" bind:this={host}></div>
</section>

<style>
  .output { height: 100%; min-height: 0; display: flex; flex-direction: column; gap: 6px; }
  header { display: flex; align-items: center; gap: 8px; min-width: 0; }
  .title { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0; }
  .spacer { flex: 1; }
  .screen { flex: 1; min-height: 0; background: #0b1017; overflow: hidden; }
  p { margin: 0; overflow-wrap: anywhere; }
  .note { font-size: 12px; }
</style>
