<script lang="ts">
  import { onMount } from "svelte";
  import { api, type SetupDraft, type SetupAdmission, type PlanSessionAdmission } from "./lib/api";
  import { connectInstances, instances, instancesById, previewsEnabled, toasts, dismissToast, reportError, managerInstance, appSettings, startInstance, errorText, refreshInstances } from "./lib/stores";
  import Grid from "./lib/components/Grid.svelte";
  import Tree from "./lib/components/Tree.svelte";
  import SessionView from "./lib/components/SessionView.svelte";
  import NewInstanceDialog from "./lib/components/NewInstanceDialog.svelte";
  import PlanPanel from "./lib/components/PlanPanel.svelte";
  import SubagentPanel from "./lib/components/SubagentPanel.svelte";
  import PlanChangeView from "./lib/components/PlanChangeView.svelte";
  import CodeDiffView from "./lib/components/CodeDiffView.svelte";
  import GitGraphView from "./lib/components/GitGraphView.svelte";
  import GlobalConfigDialog from "./lib/components/GlobalConfigDialog.svelte";
  import AgentSetupPage from "./lib/components/AgentSetupPage.svelte";
  import SettingsDialog from "./lib/components/SettingsDialog.svelte";
  import ProjectPicker from "./lib/components/ProjectPicker.svelte";
  import TerminalsPage from "./lib/components/TerminalsPage.svelte";
  import { dashboardInstances, resolveStartupTarget } from "./lib/settings";
  let initializing = $state(true);
  let startupSelected = false;
  let restoredIds = $state<ReadonlySet<string>>(new Set());
  let browserConfigured = $state(false);
  let browserSettingsError = $state<string | null>(null);
  let appliedBrowserKey: string | null = null;
  let browserSettingsQueue = Promise.resolve();
  function configureBrowserSettings(headless: boolean, embedded: boolean): Promise<void> {
    const key = `${headless}:${embedded}`;
    const next = browserSettingsQueue.catch(() => {}).then(async () => {
      if (appliedBrowserKey === key) return;
      await api.configureBrowser({ headless, embedded });
      appliedBrowserKey = key;
    });
    browserSettingsQueue = next;
    return next;
  }
  $effect(() => {
    if (!browserConfigured) return;
    const headless = $appSettings.browserHeadless;
    const embedded = $appSettings.browserEmbedded;
    void configureBrowserSettings(headless, embedded).then(() => browserSettingsError = null).catch(error => {
      browserSettingsError = errorText(error);
      reportError("Configure browser", error);
    });
  });
  let openedIds = $state<ReadonlySet<string>>(new Set());
  let settingsOpen = $state(false);
  let view = $state<"dashboard" | "agent_setup" | "code_diff" | "git_graph" | "terminals">("dashboard");
  let configOpen = $state(false);
  let configMode = $state<"save" | "import">("save");
  let setupDraft = $state<SetupDraft>({ scope: null, kinds: [], items: [], step: 0, instanceId: null, projectInstanceId: null });
  let setupContextId = $state<string | null>(null);
  let setupAdmission = $state<SetupAdmission | null>(null);
  let returnToSetup = $state(false);
  let focusedId = $state<string | null>(null);
  let dialogOpen = $state(false);
  let dialogPurpose = $state<"instance" | "manager">("instance");
  let openingManager = $state(false);
  let panel = $state<{ kind: "plan" | "subagents"; id: string; child?: string } | null>(null);
  let planChange = $state<{ targetId: string; plannerId: string; admission: PlanSessionAdmission; handoffAtOpen: string | null } | null>(null);
  let planOpening = false;
  let diffInstanceId = $state<string | null>(null);
  let diffName = $state("");
  // Set by the mounted Code diff page; resolves false when the user keeps unsaved edits.
  let diffGuard: (() => Promise<boolean>) | null = null;
  let popoverOpen = $state(false);
  const browserHidden = $derived(settingsOpen || dialogOpen || configOpen || !!panel || popoverOpen);
  const focused = $derived(focusedId ? $instancesById.get(focusedId) : undefined);
  const panelOwner = $derived(panel ? $instancesById.get(panel.id) : undefined);
  const planTarget = $derived(planChange ? $instancesById.get(planChange.targetId) : undefined);
  const planner = $derived(planChange ? $instancesById.get(planChange.plannerId) : undefined);
  const overview = $derived(dashboardInstances($instances, $appSettings, restoredIds, openedIds));
  const needsHuman = $derived(!!$managerInstance?.pendingDecisionCount || $instances.some(instance => !!instance.humanRequest));
  const fullscreen = $derived(view === "dashboard" && !!focused && !planChange);
  const setupProject = $derived(setupContextId ? $instancesById.get(setupContextId) ?? null : null);
  function openAgentSetup(): void {
    if (view === "agent_setup") return;
    planChange = null;
    const sourceId = fullscreen ? focused?.id : (view === "code_diff" || view === "git_graph") ? diffInstanceId : null;
    setupContextId = sourceId && $instancesById.get(sourceId)?.kind === "instance" ? sourceId : null;
    if (!setupDraft.instanceId) {
      if (setupDraft.projectInstanceId !== setupContextId && setupDraft.scope === "repository") setupDraft.scope = null;
      setupDraft.projectInstanceId = setupContextId;
    }
    view = "agent_setup";
  }
  // Every user-driven exit from the Code diff page runs its unsaved-edits check first.
  async function requestNavigation(action: () => void | Promise<void>): Promise<void> {
    if (view === "code_diff" && diffGuard && !await diffGuard()) return;
    await action();
  }
  function openCodeDiff(id: string): void {
    const target = $instancesById.get(id);
    if (!target) { reportError("Open Code diff", "Project is no longer available"); return; }
    planChange = null;
    diffInstanceId = id;
    diffName = target.name;
    void api.setBrowserView(null, null, null).catch(error => reportError("Hide browser", error));
    view = "code_diff";
  }
  // A global page, not an agent view: it never starts an agent, and leaving it stops nothing.
  function openTerminals(): void {
    planChange = null;
    void api.setBrowserView(null, null, null).catch(error => reportError("Hide browser", error));
    view = "terminals";
  }
  function closeCodeDiff(): void {
    const id = diffInstanceId;
    if (id && $instancesById.has(id)) focus(id);
    else { reportError("Open project", "Project is no longer available"); focus(null); }
  }
  function newInstance(): void { returnToSetup = false; dialogPurpose = "instance"; dialogOpen = true; }
  function configureManager(): void { returnToSetup = true; dialogPurpose = "manager"; dialogOpen = true; }
  function created(id: string): void {
    void requestNavigation(() => {
      if (returnToSetup) { returnToSetup = false; view = "agent_setup"; }
      else focus(id);
    });
  }
  function openConfig(mode: "save" | "import"): void { configMode = mode; configOpen = true; }
  async function openManager(): Promise<void> {
    const manager = $managerInstance;
    if (!manager) { returnToSetup = false; dialogPurpose = "manager"; dialogOpen = true; return; }
    openingManager = true;
    try {
      if (manager.status === "stopped" || manager.status === "error") await startInstance(manager.id);
      focus(manager.id);
    } finally { openingManager = false; }
  }
  function focus(id: string | null): void {
    planChange = null;
    const target = id === null ? undefined : $instancesById.get(id);
    if (id !== null && !target) { reportError("Open project", "Project is no longer available"); return; }
    if (target?.kind === "instance" && !target.helper) {
      if (!openedIds.has(target.id)) openedIds = new Set([...openedIds, target.id]);
      if ($appSettings.latestProjectId !== target.id) appSettings.update(settings => ({ ...settings, latestProjectId: target.id }));
    }
    // Only a session being left returns to preview size; re-entering the same one (Code diff -> Back) keeps its PTY.
    if (focused && focused.generation > 0 && focused.id !== id) {
      void api.resizePty(focused.id, 120, 32).catch(error => reportError("Restore preview dimensions", error));
    }
    view = "dashboard";
    focusedId = id;
  }
  const openingProjects = new Set<string>();
  async function openProject(id: string): Promise<void> {
    const target = $instancesById.get(id);
    if (!target) { focus(id); return; }
    if (view !== "dashboard" || focusedId !== id) focus(id);
    if (target.status === "running" || target.status === "starting" || openingProjects.has(id)) return;
    openingProjects.add(id);
    try { await startInstance(id); }
    finally { openingProjects.delete(id); }
  }
  function cycleTerminal(step: 1 | -1): void {
    const list = $instances;
    const n = list.length;
    const start = list.findIndex(i => i.id === focusedId);
    for (let k = 1; k <= n; k++) {
      const c = list[(((start + step * k) % n) + n) % n];
      if (c.id !== focusedId && (c.status === "running" || c.status === "starting")) { focus(c.id); return; }
    }
  }
  function onTabKey(e: KeyboardEvent): void {
    if (!(fullscreen && e.ctrlKey && !e.altKey && !e.metaKey && e.key === "Tab")) return;
    if (document.querySelector("dialog[open], [popover]:popover-open") || panel) return;
    e.preventDefault();
    e.stopPropagation();
    cycleTerminal(e.shiftKey ? -1 : 1);
  }

  function onPlan(id: string): void { panel = { kind: "plan", id }; }
  function onSubagents(id: string): void { panel = { kind: "subagents", id }; }
  function onSubagent(id: string, child: string): void { panel = { kind: "subagents", id, child }; }
  async function openPlanChange(id: string): Promise<void> {
    if (planOpening) return;
    planOpening = true;
    try {
      const admission = await api.openPlanSession(id);
      await refreshInstances();
      planChange = { targetId: id, plannerId: admission.planner.id, admission, handoffAtOpen: $instancesById.get(id)?.lastPlanHandoff ?? null };
    } catch (error) { reportError("Open plan agent", error); }
    finally { planOpening = false; }
  }
  async function retryPlanContext(): Promise<void> {
    if (!planChange) return;
    const targetId = planChange.targetId;
    try {
      const admission = await api.openPlanSession(targetId);
      if (planChange?.targetId === targetId) planChange = { ...planChange, admission };
    } catch (error) { reportError("Send plan context", error); }
  }
  $effect(() => {
    if (!planChange) return;
    if (!planTarget || !planner) { planChange = null; return; }
    // Only a new successful handoff for the plan view currently being watched navigates away.
    if (planTarget.lastPlanHandoff && planTarget.lastPlanHandoff !== planChange.handoffAtOpen) {
      focus(planTarget.id);
    }
  });
  onMount(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void configureBrowserSettings($appSettings.browserHeadless, $appSettings.browserEmbedded).then(() => {
      browserConfigured = true;
      return connectInstances();
    }).then(async cleanup => {
      if (disposed) { cleanup(); return; }
      unlisten = cleanup;
      if (startupSelected) return;
      startupSelected = true;
      restoredIds = new Set($instances.filter(row => row.kind === "instance").map(row => row.id));
      const settings = $appSettings;
      if (settings.startupTarget === "dashboard") {
        focus(null);
        initializing = false;
        return;
      }
      const selection = resolveStartupTarget(settings, $instances);
      if (selection.missingProject) reportError("Startup project", "Saved project is unavailable; opening The Manager instead.");
      if (selection.instanceId) {
        focus(selection.instanceId);
        initializing = false;
        const target = $instancesById.get(selection.instanceId);
        if (settings.autoStart && target && (target.status === "stopped" || target.status === "error")) await startInstance(target.id);
      } else {
        initializing = false;
        returnToSetup = false;
        dialogPurpose = "manager";
        dialogOpen = true;
      }
    }).catch(error => { if (!disposed) { initializing = false; reportError(browserConfigured ? "Connect desktop" : "Configure browser", error); } });
    function onToggle(): void { popoverOpen = !!document.querySelector("[popover]:popover-open"); }
    window.addEventListener("keydown", onTabKey, true);
    document.addEventListener("toggle", onToggle, true);
    return () => { disposed = true; unlisten?.(); window.removeEventListener("keydown", onTabKey, true); document.removeEventListener("toggle", onToggle, true); };
  });
</script>
<svelte:window onkeydown={e => { if (e.key === "Escape" && !document.querySelector("dialog[open], [popover]:popover-open") && panel) panel = null; }} />
<div class="app">
  <header class="toolbar">
    <h1>OMP Manager</h1><span class="pill">{$instances.length} instances</span><span class="spacer"></span>
    <button class="settings-button" title="Settings" disabled={initializing} onclick={() => settingsOpen = true}><svg aria-hidden="true" viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.8"><path d="m9 3-.5 3-2 1.2L4 6l-2 3.5 2.5 1.8v2.4L2 15.5 4 19l2.5-1.2 2 1.2.5 3h6l.5-3 2-1.2L20 19l2-3.5-2.5-1.8v-2.4L22 9.5 20 6l-2.5 1.2-2-1.2L15 3Z" /><circle cx="12" cy="12" r="3" /></svg>Settings</button>
    {#if !fullscreen}
      <button disabled={initializing} aria-pressed={$previewsEnabled} onclick={() => previewsEnabled.update(value => !value)}>Previews {$previewsEnabled ? "On" : "Off"}</button>
    {/if}
    {#if fullscreen}<button disabled={initializing} onclick={() => openConfig("save")}>Save as global config</button>{/if}
    <button disabled={initializing} onclick={() => openConfig("import")}>Import config</button>
    <button disabled={initializing} class:active={view === "agent_setup"} onclick={() => requestNavigation(openAgentSetup)}>Agent setup</button>
    {#if view === "dashboard" && !focused && !planChange}<button disabled={initializing} title="Every agent's command history and live commands" onclick={() => requestNavigation(openTerminals)}>Terminals</button>{/if}
    <button aria-label="The Manager" disabled={openingManager || initializing} onclick={() => requestNavigation(openManager)}>The Manager{#if needsHuman}<span class="pill warn">Needs you</span>{/if}</button>
    <ProjectPicker projects={$instances} label="Previous projects" disabled={initializing} onselect={id => requestNavigation(() => openProject(id))} />
    <button disabled={initializing} class="primary" onclick={newInstance}>New instance</button>
  </header>
  {#if browserSettingsError}<p class="err" role="alert">Configure browser: {browserSettingsError}</p>{/if}
  <main class:focused={(view === "dashboard" && (!!focused || !!planChange)) || view === "code_diff" || view === "git_graph" || view === "terminals"}>
    {#if initializing}
      <p role="status">Loading workspace…</p>
    {:else if view === "agent_setup"}
      <AgentSetupPage bind:draft={setupDraft} bind:admission={setupAdmission} projectContext={setupProject} onback={() => view = "dashboard"} onSetupManager={configureManager} onOpenManager={openManager} onSelectSession={focus} {onPlan} onPlanChange={openPlanChange} {onSubagents} onCodeDiff={openCodeDiff} />
    {:else if view === "terminals"}
      <TerminalsPage onBack={() => focus(null)} />
    {:else if view === "git_graph" && diffInstanceId}
      <GitGraphView instanceId={diffInstanceId} name={diffName} onBack={() => view = "code_diff"} />
    {:else if view === "code_diff" && diffInstanceId}
      <CodeDiffView instanceId={diffInstanceId} name={diffName} onBack={closeCodeDiff} onGitTree={() => view = "git_graph"} onGuardChange={guard => diffGuard = guard} />
    {:else if planChange && planTarget && planner}
      <PlanChangeView target={planTarget} {planner} admission={planChange.admission} onBack={() => planChange = null} onRetry={retryPlanContext} />
    {:else if focused}
      <SessionView instance={focused} instances={$instances} onSwitch={id => void openProject(id)} onClose={() => focus(null)} {onPlan} onPlanChange={openPlanChange} {onSubagents} onCodeDiff={openCodeDiff} showBrowser={!focused.helper} browserHidden={browserHidden} />
    {:else if $previewsEnabled}
      <Grid instances={overview} onSelect={focus} onOpen={openProject} {onPlan} onPlanChange={openPlanChange} {onSubagents} tileMin={$appSettings.previewSize} onCreate={newInstance} />
    {:else}
      <Tree list={overview} onSelect={focus} {onSubagent} {onPlan} onPlanChange={openPlanChange} />
    {/if}
  </main>
</div>
<NewInstanceDialog open={dialogOpen} purpose={dialogPurpose} onclose={() => dialogOpen = false} oncreated={created} />
<GlobalConfigDialog open={configOpen} mode={configMode} initialSourceId={focused?.id || $managerInstance?.id || null} onclose={() => configOpen = false} />
<SettingsDialog open={settingsOpen} onclose={() => settingsOpen = false} />
{#if panel && panelOwner}
  <div class="backdrop" role="presentation" onclick={e => { if (e.target === e.currentTarget) panel = null; }}>
    <div class="viewer panel" role="dialog" aria-modal="true" aria-label={panel.kind === "plan" ? "OMP plan" : "Subagent chats"}>
      <header><strong>{panelOwner.name}</strong><span class="spacer"></span><button onclick={() => panel = null}>Close</button></header>
      {#key panelOwner.id}
        {#if panel.kind === "plan"}<PlanPanel instanceId={panelOwner.id} />{:else}<SubagentPanel instance={panelOwner} selectedChild={panel.child || null} />{/if}
      {/key}
    </div>
  </div>
{/if}
<div class="toast-stack">
  {#each $toasts as toast (toast.id)}<div class="toast error" role="alert"><span>{toast.message}</span><button onclick={() => dismissToast(toast.id)}>Dismiss</button></div>{/each}
</div>
<style>
  .app { height: 100vh; display: flex; flex-direction: column; }
  .toolbar { display: flex; align-items: center; padding: 12px 16px; gap: 10px; border-bottom: 1px solid var(--line); background: #101722; flex-wrap: wrap; }
  h1 { font-size: 18px; margin: 0; }
  main { flex: 1; min-height: 0; padding: 16px; overflow: auto; }
  main.focused { overflow: hidden; }
  .viewer { width: min(1100px, 96vw); height: 90vh; }
  .viewer > :global(section) { flex: 1; min-height: 0; border: none; }
  .settings-button { display: inline-flex; align-items: center; gap: 6px; }
</style>
