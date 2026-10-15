<script lang="ts">
  import { api, type BrowserInfo, type Instance } from "../api";
  import { agentName, confirmDanger } from "../ui";
  import Term from "./Term.svelte";
  import BrowserPane from "./BrowserPane.svelte";
  import OwnershipControls from "./OwnershipControls.svelte";
  import { restartInstance, startInstance, stopInstance, browserStatuses, errorText, reportError, appSettings, terminalOwners, terminalUi, updateTerminalUi, openUserTerminal } from "../stores";
  import { defaultTerminalUi } from "../terminal";
  import TerminalWorkspace from "./TerminalWorkspace.svelte";
  let { instance, instances, onSwitch, onClose, onPlan, onPlanChange, onSubagents, onCodeDiff, showBrowser = false, browserHidden = false }: {
    instance: Instance; instances: Instance[]; onSwitch: (id: string) => void;
    onClose: () => void; onPlan: (id: string) => void; onPlanChange: (id: string) => void; onSubagents: (id: string) => void;
    onCodeDiff: (id: string) => void; showBrowser?: boolean; browserHidden?: boolean;
  } = $props();
  const TERMINAL_MIN = 160;
  const BROWSER_MIN = 520;
  const TERMINAL_PANE_MIN = 360;
  let busy = $state(false);
  let ratio = $state(0.5);
  let dragging = $state(false);
  let workspace = $state<HTMLDivElement>();
  const browser = $derived($browserStatuses.get(instance.id));
  const currentBrowser = $derived(browser?.generation === instance.generation && browser.state !== "closed" ? browser : undefined);
  const ui = $derived($terminalUi.get(instance.id) ?? defaultTerminalUi());
  const terminalOpen = $derived(showBrowser && ui.pane === "terminal");
  // Browser and Terminal share one right-hand slot; the hidden one keeps running.
  const paneBrowser = $derived(showBrowser && !terminalOpen && currentBrowser?.embedded && !currentBrowser.headless ? currentBrowser : undefined);
  const rightPane = $derived(terminalOpen ? "terminal" : paneBrowser ? "browser" : null);
  const rightMin = $derived(rightPane === "terminal" ? TERMINAL_PANE_MIN : BROWSER_MIN);
  const owner = $derived($terminalOwners.get(instance.id));
  const live = $derived(instance.status === "running" || instance.status === "starting");
  async function run(action: () => Promise<boolean>): Promise<void> {
    busy = true;
    try { await action(); } finally { busy = false; }
  }
  const HEADLESS_RESTART = "browser_headless_restart_required";
  async function openBrowser(): Promise<void> {
    const { id, generation } = instance;
    busy = true;
    try {
      let info: BrowserInfo;
      try { info = await api.openSessionBrowser(id, generation, false); }
      catch (error) {
        if (errorText(error).trim() !== HEADLESS_RESTART) throw error;
        if (!await confirmDanger("This session's headless browser must restart to show it inside the app. Its open tabs will be closed. Continue?")) return;
        info = await api.openSessionBrowser(id, generation, true);
      }
      if (instance.id === id && instance.generation === generation) {
        updateTerminalUi(id, { pane: "browser" });
        const next = new Map($browserStatuses);
        const current = next.get(id);
        if (!current || current.generation !== info.generation || info.epoch >= current.epoch) next.set(id, info);
        browserStatuses.set(next);
      }
    } catch (error) { reportError("Open session browser", error); }
    finally { busy = false; }
  }
  async function toggleTerminal(): Promise<void> {
    if (terminalOpen) { updateTerminalUi(instance.id, { pane: null }); return; }
    updateTerminalUi(instance.id, { pane: "terminal" });
    // The user shell exists only when asked for here: auto-open and the Dashboard page never create one.
    if ($appSettings.terminalCreateUser && !owner?.userShell) await openUserTerminal(instance.id, true);
  }
  // A command that starts while this session is on screen shows the pane when the setting is on. Closing it by hand
  // lasts until the next command starts; this never leaves the open session or creates a shell.
  let seenCommands = new Set<string>();
  let autoWas = false;
  $effect(() => {
    const autoOpen = showBrowser && $appSettings.terminalAutoOpen;
    const ids = (owner && owner.generation === instance.generation ? owner.active : []).map(command => command.executionId);
    const fresh = autoOpen && !autoWas ? ids : ids.filter(id => !seenCommands.has(id));
    seenCommands = new Set(ids);
    autoWas = autoOpen;
    if (autoOpen && fresh.length && ui.pane !== "terminal") updateTerminalUi(instance.id, { pane: "terminal" });
  });
  function clampRatio(value: number, width: number): number {
    if (width <= TERMINAL_MIN + rightMin + 6) return TERMINAL_MIN / Math.max(width, 1);
    return Math.min((width - rightMin - 6) / width, Math.max(TERMINAL_MIN / width, value));
  }
  function dragStart(event: PointerEvent): void {
    dragging = true;
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
  }
  function dragMove(event: PointerEvent): void {
    if (!dragging || !workspace) return;
    const box = workspace.getBoundingClientRect();
    ratio = clampRatio((event.clientX - box.left) / box.width, box.width);
  }
  function dragEnd(event: PointerEvent): void {
    dragging = false;
    (event.currentTarget as HTMLElement).releasePointerCapture(event.pointerId);
  }
  function dividerKey(event: KeyboardEvent): void {
    if (!workspace || (event.key !== "ArrowLeft" && event.key !== "ArrowRight")) return;
    event.preventDefault();
    ratio = clampRatio(ratio + (event.key === "ArrowRight" ? 0.03 : -0.03), workspace.getBoundingClientRect().width);
  }
</script>
<div class="view">
  <header class="panel topbar">
    <button onclick={onClose}>Dashboard</button>
    <select aria-label="Switch session" title="Ctrl+Tab / Ctrl+Shift+Tab: next / previous running terminal" value={instance.id} onchange={e => onSwitch(e.currentTarget.value)}>
      {#each instances as row (row.id)}<option value={row.id}>{row.name} · {row.status}</option>{/each}
    </select>
    <span class="pill">{live ? instance.activity : instance.status}</span>
    <span class="dim">{instance.profile || "Default profile"}{instance.agentFile ? ` · ${agentName(instance.agentFile)}` : ""}</span><span class="spacer"></span>
    {#if currentBrowser && !paneBrowser}
      <span class="pill">{currentBrowser.state === "error" ? "Browser error" : currentBrowser.state === "opening" ? "Opening browser…" : currentBrowser.headless ? "Browser: headless" : "Browser: external"}</span>
    {/if}
    <button disabled={!instance.subagents.length} onclick={() => onSubagents(instance.id)}>Subagents: {instance.subagents.length}</button>
    {#if showBrowser}<button disabled={busy || !live} title={live ? "Show this session's browser next to the terminal" : "Start this session first"} onclick={openBrowser}>Browser</button>{/if}
    {#if showBrowser}<button class:active={terminalOpen} aria-pressed={terminalOpen} title="Your own shell, the AI's command history and its live commands next to the terminal" onclick={toggleTerminal}>Terminal</button>{/if}
    {#if showBrowser}<button title="Review and edit this project's changes" onclick={() => onCodeDiff(instance.id)}>Code diff</button>{/if}
    {#if instance.planKind}<button onclick={() => onPlanChange(instance.id)}>Plan</button>{/if}
    {#if instance.planPath}<button onclick={() => onPlan(instance.id)}>View Plan</button>{/if}
    <button disabled={busy} onclick={() => run(() => live ? stopInstance(instance.id) : startInstance(instance.id))}>{live ? "Stop" : "Start / reopen"}</button>
    <button disabled={busy} onclick={() => run(() => restartInstance(instance.id))}>Restart</button>
  </header>
  <p class="identity mono">{instance.projectPath}{instance.task ? ` · ${instance.task}` : ""}</p>
  <OwnershipControls {instance} onOpen={() => onSwitch(instance.id)} />
  {#if instance.lastError}<p class="err" role="alert">{instance.lastError}</p>{/if}
  {#if currentBrowser?.error && !paneBrowser}<p class="browser-error err" role="alert">{currentBrowser.error}</p>{/if}
  <div class="workspace" bind:this={workspace}>
    <div class="screen" style={rightPane ? `flex: 0 0 ${ratio * 100}%` : undefined}>
      {#if instance.generation > 0}<Term instanceId={instance.id} interactive cols={instance.cols} rows={instance.rows} generation={instance.generation} scrollback={8000} />
      {:else}<p class="empty">Start this instance to open its native OMP terminal.</p>{/if}
    </div>
    {#if rightPane}
      <div class="divider" role="slider" aria-orientation="horizontal" aria-valuemin="0" aria-valuemax="100" aria-valuenow={Math.round(ratio * 100)} aria-label={rightPane === "terminal" ? "Agent and terminal pane width" : "Terminal and browser width"} tabindex="0" onpointerdown={dragStart} onpointermove={dragMove} onpointerup={dragEnd} onpointercancel={dragEnd} onkeydown={dividerKey}></div>
      {#if rightPane === "terminal"}
        <div class="terminal-column"><TerminalWorkspace instanceId={instance.id} /></div>
      {:else if paneBrowser}
        <div class="browser-column"><BrowserPane instanceId={instance.id} generation={instance.generation} info={paneBrowser} hidden={browserHidden || dragging} /></div>
      {/if}
    {/if}
  </div>
</div>
<style>
  .view { height: 100%; display: flex; flex-direction: column; gap: 8px; min-height: 0; }
  .topbar { flex-direction: row; align-items: center; flex-wrap: wrap; padding: 10px; gap: 8px; }
  select { width: auto; max-width: 35ch; }
  .identity { font-size: 12px; color: var(--text-dim); margin: 0; overflow-wrap: anywhere; }
  .workspace { flex: 1; min-height: 0; min-width: 0; display: flex; }
  .screen { flex: 1; min-width: 160px; min-height: 0; background: #0b1017; }
  .divider { flex: 0 0 6px; cursor: col-resize; background: var(--line); touch-action: none; }
  .divider:hover, .divider:focus-visible { background: var(--accent, #4c8dff); outline: none; }
  .browser-column { flex: 1 1 0; min-width: 520px; min-height: 0; }
  .terminal-column { flex: 1 1 0; min-width: 360px; min-height: 0; }
  .browser-error { margin: 0; overflow-wrap: anywhere; }
</style>
