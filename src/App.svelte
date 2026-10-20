<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { humanDecisions, hasDecisionAttention } from "./lib/human-decisions";
  import type { HumanDecisionSnapshot } from "./lib/api";
  import { api, type SetupDraft, type SetupAdmission, type PlanSessionAdmission, type Instance, type InsightProducer } from "./lib/api";
  import { connectInstances, instances, instancesById, previewsEnabled, toasts, dismissToast, reportError, managerInstance, architectInstance, appSettings, startInstance, errorText, refreshInstances } from "./lib/stores";
  import { initializeProjectHistory, visibleProjectHistory, computedFolders, rememberProjectFolder } from "./lib/stores";
  import { coordinatorTemporaryFolders } from "./lib/stores";
  import { architectEntryFolder } from "./lib/workspace";
  import { isHiddenComputed } from "./lib/project-history";
  import { projectFolders } from "./lib/repo-agents";
  import Grid from "./lib/components/Grid.svelte";
  import Tree from "./lib/components/Tree.svelte";
  import SessionView from "./lib/components/SessionView.svelte";
  import NewInstanceDialog from "./lib/components/NewInstanceDialog.svelte";
  import PlanPanel from "./lib/components/PlanPanel.svelte";
  import PlanChangeView from "./lib/components/PlanChangeView.svelte";
  import CodeDiffView from "./lib/components/CodeDiffView.svelte";
  import GitGraphView from "./lib/components/GitGraphView.svelte";
  import GlobalConfigDialog from "./lib/components/GlobalConfigDialog.svelte";
  import AgentSetupPage from "./lib/components/AgentSetupPage.svelte";
  import SettingsDialog from "./lib/components/SettingsDialog.svelte";
  import ProjectPicker from "./lib/components/ProjectPicker.svelte";
  import TerminalsPage from "./lib/components/TerminalsPage.svelte";
  import DependencyTreePage from "./lib/components/DependencyTreePage.svelte";
  import CodeQualityPage from "./lib/components/CodeQualityPage.svelte";
  import SystemOverviewPage from "./lib/components/SystemOverviewPage.svelte";
  import GraphifyView from "./lib/components/GraphifyView.svelte";
  import { knowledge } from "./lib/knowledge";
  import type { ArchitectDescriptor } from "./lib/overview-types";
  import { terminalOwners, updateTerminalUi } from "./lib/stores";
  import { GLOBAL_UI_KEY, DECISIONS_KEY } from "./lib/terminal";
  import { monitorProducerCurrent } from "./lib/host-monitor";
  import SrsStartDialog from "./lib/components/SrsStartDialog.svelte";
  import SrsDocumentationPage from "./lib/components/SrsDocumentationPage.svelte";
  import { srs } from "./lib/srs";
  import { systemOverview } from "./lib/system-overview-instance";
  import { dashboardInstances, resolveStartupTarget } from "./lib/settings";
  import { awaitNativeAgentHub, describeNativeHubError } from "./lib/native-agent-hub";
  let initializing = $state(true);
  let startupSelected = false;
  let provisioningError = $state<string | null>(null);
  let temporaryFolders = $state<string[]>([]);
  let analyzerInitialFolder = $state<string | undefined>(undefined);
  // Which agent opened Code quality and where Back returns: the agent's own view or the Change plan split view.
  let analyzerContext = $state<{ sourceId: string; returnTo: { kind: "agent"; id: string } | { kind: "planChange" } | { kind: "dependencyTree" } } | null>(null);
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
  let view = $state<"dashboard" | "agent_setup" | "code_diff" | "git_graph" | "terminals" | "dependency_tree" | "system_overview" | "code_quality" | "graphify" | "srs_documentation">("dashboard");
  let srsStartFolder = $state<string | null>(null);
  let srsPageFolder = $state<string | null>(null);
  let srsPageJob = $state<string | null>(null);
  function openSrsStart(folder: string): void { srsStartFolder = folder; }
  function openSrsPage(folder: string, jobId: string | null = null): void {
    srsStartFolder = null; planChange = null; srsPageFolder = folder; srsPageJob = jobId;
    void api.setBrowserView(null, null, null).catch(error => reportError("Hide browser", error));
    view = "srs_documentation";
  }
  let graphifyFolder = $state<string | null>(null);
  // Passive, sequence-fenced capability probes: startup (global), the shown System overview project, and window focus.
  const overviewFolder = $derived(view === "system_overview" ? $systemOverview.folder || null : null);
  $effect(() => { const folder = overviewFolder; untrack(() => void knowledge.probe(folder)); });
  $effect(() => {
    const onFocus = (): void => { void knowledge.probe(untrack(() => overviewFolder)); };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  });
  let configOpen = $state(false);
  let configMode = $state<"save" | "import">("save");
  let setupDraft = $state<SetupDraft>({ scope: null, kinds: [], items: [], step: 0, instanceId: null, projectInstanceId: null });
  let setupContextId = $state<string | null>(null);
  let setupAdmission = $state<SetupAdmission | null>(null);
  let returnToSetup = $state(false);
  let focusedId = $state<string | null>(null);
  let dialogOpen = $state(false);
  let dialogPurpose = $state<"instance" | "manager" | "architect">("instance");
  let openingManager = $state(false);
  let openingArchitect = $state(false);
  let panel = $state<{ kind: "plan"; id: string } | null>(null);
  let focusRequest = $state(0);
  let hubSequence = 0;
  let hubRequest = $state<{ id: string; sequence: number } | null>(null);
  // Entry of the System overview page: the Architect's real folder, or the parent-lived state when returning to a generated result.
  let overviewEntry = $state<{ folder: string | undefined; resume: boolean }>({ folder: undefined, resume: false });
  const readyTerminals = new Map<string, number>();
  function onTerminalReady(id: string, generation: number, ready: boolean): void {
    if (ready) readyTerminals.set(id, generation);
    else if (readyTerminals.get(id) === generation) readyTerminals.delete(id);
  }
  $effect(() => {
    if (hubRequest && (view !== "dashboard" || focusedId !== hubRequest.id || planChange || panel)) {
      hubSequence++;
      hubRequest = null;
    }
  });
  let planChange = $state<{ targetId: string; plannerId: string; admission: PlanSessionAdmission; handoffAtOpen: string | null } | null>(null);
  let planOpening = false;
  let diffInstanceId = $state<string | null>(null);
  let diffName = $state("");
  // Set by the mounted Code diff page; resolves false when the user keeps unsaved edits.
  let diffGuard: (() => Promise<boolean>) | null = null;
  let popoverOpen = $state(false);
  const browserHidden = $derived(settingsOpen || dialogOpen || configOpen || srsStartFolder !== null || !!panel || popoverOpen);
  const focused = $derived(focusedId ? $instancesById.get(focusedId) : undefined);
  const panelOwner = $derived(panel ? $instancesById.get(panel.id) : undefined);
  const planTarget = $derived(planChange ? $instancesById.get(planChange.targetId) : undefined);
  const planner = $derived(planChange ? $instancesById.get(planChange.plannerId) : undefined);
  const overview = $derived(dashboardInstances($instances, $appSettings, restoredIds, openedIds));
  const needsHuman = $derived(!!$managerInstance?.pendingDecisionCount || $instances.some(instance => !!instance.humanRequest));
  const fullscreen = $derived(view === "dashboard" && !!focused && !planChange);
  const setupProject = $derived(setupContextId ? $instancesById.get(setupContextId) ?? null : null);
  // The Architect as the overview and structure views see it: the terminal session of exactly this generation.
  const architectDescriptor = $derived.by((): ArchitectDescriptor | null => {
    const architect = $architectInstance;
    if (!architect) return null;
    const owner = $terminalOwners.get(architect.id);
    return {
      id: architect.id, generation: architect.generation,
      sessionId: owner && owner.generation === architect.generation ? owner.sessionId : null,
      ready: architect.bridgeReady && architect.status === "running",
    };
  });
  // Reported by the mounted Code diff / Git graph page; fenced by route and source so an old page's cleanup cannot erase a new identity.
  let helperVisible = $state<{ view: "code_diff" | "git_graph"; instanceId: string; id: string } | null>(null);
  function reportHelperVisible(routeView: "code_diff" | "git_graph", instanceId: string | null, id: string | null): void {
    if (view !== routeView || diffInstanceId !== instanceId || !instanceId) return;
    helperVisible = id ? { view: routeView, instanceId, id } : null;
  }
  // The conversation actually on screen: the planner (not its target), a valid embedded setup session, a rendered helper, or the focused session. Non-chat routes show none.
  const visibleAgentId = $derived.by((): string | null => {
    if (view === "git_graph" || view === "code_diff") return helperVisible && helperVisible.view === view && helperVisible.instanceId === diffInstanceId ? helperVisible.id : null;
    if (view === "agent_setup") return setupDraft.instanceId && $instancesById.has(setupDraft.instanceId) ? setupDraft.instanceId : null;
    if (view !== "dashboard") return null;
    if (planChange && planTarget && planner) return planner.id;
    return focused ? focused.id : null;
  });
  const attention = $derived(hasDecisionAttention($humanDecisions.items, visibleAgentId));
  // Inside the guarded navigation: an attention click lands on the global Decisions tab without Follow; otherwise the remembered tab stays.
  function openTerminalsFromButton(): void {
    if (attention) updateTerminalUi(GLOBAL_UI_KEY, { tab: DECISIONS_KEY, follow: false });
    openTerminals();
  }
  // Focus only: never starts, restarts or replays the Architect.
  async function focusArchitectOnly(producer: InsightProducer, current: () => boolean): Promise<boolean> {
    // Admission can start the existing Architect before its periodic UI snapshot arrives. Read authoritative identities;
    // recheck the origin after these awaits so a late receipt can never navigate away from a newer user action.
    try {
      const [list, owners] = await Promise.all([api.listInstances(), api.listAllTerminals()]);
      const architect = list.find(row => row.kind === "architect");
      const owner = owners.find(row => row.instanceId === producer.instanceId);
      if (!current() || !monitorProducerCurrent(producer, architect, owner)) return false;
      focus(producer.instanceId);
      return true;
    } catch (error) {
      reportError("Open Architect", error);
      return false;
    }
  }
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
    hubSequence++;
    systemOverview.noteNavigation();
    srs.noteNavigation();
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
  // A global page, not an agent view: leaving it returns to The Architect's terminal.
  function openDependencyTree(): void {
    analyzerInitialFolder = architectEntryFolder($architectInstance?.projectPath, temporaryFolders);
    planChange = null;
    void api.setBrowserView(null, null, null).catch(error => reportError("Hide browser", error));
    view = "dependency_tree";
  }
  // A global page like Dependency tree: it never moves an agent's folder or hands a repository over.
  function openSystemOverview(resume = false): void {
    const architect = $architectInstance;
    overviewEntry = { folder: resume || !architect ? undefined : qualityFolder(architect), resume };
    planChange = null;
    void api.setBrowserView(null, null, null).catch(error => reportError("Hide browser", error));
    view = "system_overview";
  }
  function openGraphify(folder: string): void {
    graphifyFolder = folder;
    planChange = null;
    view = "graphify";
  }
  // The repository an agent works on: its own folder; a helper's or plan agent's target project; for the
  // Manager and Architect their committed folder, or the last remembered real folder when it is a temporary or computed shared folder.
  function qualityFolder(source: Instance): string | undefined {
    const targetId = source.helper?.target ?? source.planTarget;
    if (targetId) return $instancesById.get(targetId)?.projectPath ?? source.helper?.repo;
    if (source.kind === "manager" || source.kind === "architect") {
      const live = projectFolders($instances);
      const computed = $computedFolders.filter(folder => isHiddenComputed(folder, $computedFolders, live));
      return architectEntryFolder(source.projectPath, [...temporaryFolders, ...computed]) ?? $visibleProjectHistory[0];
    }
    return source.projectPath;
  }
  function openCodeQuality(sourceId: string, returnTo: { kind: "agent"; id: string } | { kind: "planChange" } = { kind: "agent", id: sourceId }): void {
    const source = $instancesById.get(sourceId);
    if (!source) { reportError("Open Code quality", "Agent is no longer available"); return; }
    analyzerInitialFolder = qualityFolder(source);
    analyzerContext = { sourceId, returnTo };
    if (returnTo.kind === "agent") planChange = null;
    void api.setBrowserView(null, null, null).catch(error => reportError("Hide browser", error));
    view = "code_quality";
  }
  // Opened from the Project requirements dialog: Back returns to the Dependency tree at the same folder.
  function openCodeQualityFromDependency(folder: string): void {
    analyzerInitialFolder = folder;
    analyzerContext = { sourceId: "", returnTo: { kind: "dependencyTree" } };
    view = "code_quality";
  }
  const qualityBackLabel = $derived.by(() => {
    const returnTo = analyzerContext?.returnTo;
    if (returnTo?.kind === "dependencyTree") return "Back to Dependency tree";
    if (returnTo?.kind === "planChange") return "Back to Change plan";
    const agent = returnTo?.kind === "agent" ? $instancesById.get(returnTo.id) : null;
    return agent ? `Back to ${agent.name}` : "Back to The Architect";
  });
  function closeCodeQuality(): void {
    const returnTo = analyzerContext?.returnTo;
    analyzerContext = null;
    if (returnTo?.kind === "dependencyTree") { view = "dependency_tree"; return; }
    if (returnTo?.kind === "planChange" && planChange) { view = "dashboard"; return; }
    if (returnTo?.kind === "agent" && $instancesById.has(returnTo.id)) { focus(returnTo.id); return; }
    focus(null);
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
  function configureCoordinator(kind: "manager" | "architect", returnToSetupPage: boolean): void { returnToSetup = returnToSetupPage; dialogPurpose = kind; dialogOpen = true; }
  function created(id: string): void {
    void requestNavigation(() => {
      if (returnToSetup) { returnToSetup = false; view = "agent_setup"; }
      else focus(id);
    });
  }
  function openConfig(mode: "save" | "import"): void { configMode = mode; configOpen = true; }
  async function provisionCoordinators(): Promise<boolean> {
    try {
      await initializeProjectHistory();
      const result = await api.ensureCoordinators($visibleProjectHistory.filter(folder => !temporaryFolders.includes(folder)));
      temporaryFolders = result.temporaryFolders;
      coordinatorTemporaryFolders.set(result.temporaryFolders);
      await refreshInstances();
      provisioningError = null;
      return true;
    } catch (error) {
      provisioningError = errorText(error);
      return false;
    }
  }
  async function selectStartup(): Promise<void> {
    initializing = true;
    try {
      if (!await provisionCoordinators()) return;
      const settings = $appSettings;
      const selection = resolveStartupTarget(settings, $instances);
      if (selection.missingProject) reportError("Startup project", "Saved project is unavailable; opening The Manager instead.");
      focus(selection.instanceId);
      const target = selection.instanceId ? $instancesById.get(selection.instanceId) : null;
      if (settings.autoStart && target && (target.status === "stopped" || target.status === "error")) await startInstance(target.id);
    } finally { initializing = false; }
  }
  async function openManager(): Promise<void> {
    if (!$managerInstance && !await provisionCoordinators()) return;
    const manager = $managerInstance;
    if (!manager) return;
    openingManager = true;
    try {
      if (manager.status === "stopped" || manager.status === "error") await startInstance(manager.id);
      focus(manager.id);
    } finally { openingManager = false; }
  }
  async function openArchitect(): Promise<void> {
    if (!$architectInstance && !await provisionCoordinators()) return;
    const architect = $architectInstance;
    if (!architect) return;
    openingArchitect = true;
    try {
      if (architect.status === "stopped" || architect.status === "error") await startInstance(architect.id);
      focus(architect.id);
    } finally { openingArchitect = false; }
  }
  // Dependency workflows open the existing Architect only: resume it without starting a new session, replay a saved result before focusing, never create a replacement.
  async function onOpenArchitect(id: string, replayRequestId?: string): Promise<boolean> {
    const architect = $architectInstance;
    if (!architect || architect.id !== id) return false;
    openingArchitect = true;
    try {
      if (architect.status === "stopped" || architect.status === "error") {
        if (!await startInstance(id, true)) return false;
      }
      if (replayRequestId) {
        const deadline = Date.now() + 60_000;
        while (!$architectInstance?.bridgeReady) {
          const current = $architectInstance;
          if (!current || current.id !== id || current.status === "stopped" || current.status === "error" || Date.now() > deadline) {
            reportError("Show saved result in The Architect", "The Architect did not become ready.");
            return false;
          }
          await new Promise(resolve => setTimeout(resolve, 400));
          await refreshInstances();
        }
        await api.showDependencyResultInArchitect(replayRequestId);
      }
      if ($architectInstance?.id !== id) return false;
      focus(id);
      return true;
    } catch (error) {
      reportError("Open The Architect", error);
      return false;
    } finally { openingArchitect = false; }
  }
  function focus(id: string | null): void {
    hubSequence++;
    planChange = null;
    const target = id === null ? undefined : $instancesById.get(id);
    if (id !== null && !target) { reportError("Open project", "Project is no longer available"); return; }
    if (target?.kind === "instance" && !target.helper) {
      if (!target.planTarget) rememberProjectFolder(target.projectPath);
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
  async function openProject(id: string, keepSession = false): Promise<void> {
    const target = $instancesById.get(id);
    if (!target) { focus(id); return; }
    if (view !== "dashboard" || focusedId !== id) focus(id);
    if (target.status === "running" || target.status === "starting" || openingProjects.has(id)) return;
    openingProjects.add(id);
    try { await startInstance(id, keepSession); }
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
  function onSubagents(id: string): void {
    if (hubRequest?.id === id) return;
    void requestNavigation(async () => {
      focus(id);
      if (focusedId !== id) return;
      const sequence = ++hubSequence;
      hubRequest = { id, sequence };
      focusRequest++;
      const current = () => hubRequest?.sequence === sequence && hubSequence === sequence && view === "dashboard" && focusedId === id && !planChange && !panel;
      const initial = $instancesById.get(id);
      const liveGeneration = initial?.status === "running" || initial?.status === "starting" ? initial.generation : null;
      try {
        await openProject(id, true);
        if (!current()) return;
        if (liveGeneration !== null && $instancesById.get(id)?.generation !== liveGeneration) return;
        await awaitNativeAgentHub({
          current,
          instance: () => $instancesById.get(id),
          terminalReady: generation => readyTerminals.get(id) === generation,
          open: generation => api.openNativeAgentHub(id, generation),
          wait: async () => {
            await new Promise(resolve => setTimeout(resolve, 100));
          },
        });
      } catch (error) {
        if (current()) reportError("Open native Agent Hub", describeNativeHubError(error).message);
      } finally {
        if (hubRequest?.sequence === sequence) hubRequest = null;
      }
    });
  }
  function onSubagent(id: string, _child: string): void { onSubagents(id); }
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
    systemOverview.attach({
      architectId: () => $architectInstance?.id ?? null,
      architectSession: () => architectDescriptor,
      openArchitect: (id, replay) => onOpenArchitect(id, replay),
      following: ticket => view === "dashboard" && !planChange && !panel && focusedId === (ticket.producer?.instanceId ?? $architectInstance?.id ?? null),
      quiescent: ticket => { const architect = ticket.producer ? $instancesById.get(ticket.producer.instanceId) : $architectInstance; return !!architect && architect.activity === "idle" && architect.pendingDecisionCount === 0 && !architect.humanRequest; },
      returnToOverview: () => openSystemOverview(true),
    });
    srs.attach({
      architectId: () => $architectInstance?.id ?? null,
      openArchitect: id => onOpenArchitect(id),
      following: ticket => view === "dashboard" && !planChange && !panel && focusedId === (ticket.producer?.instanceId ?? $architectInstance?.id ?? null),
      quiescent: ticket => { const architect = ticket.producer ? $instancesById.get(ticket.producer.instanceId) : $architectInstance; return !!architect && architect.activity === "idle" && architect.pendingDecisionCount === 0 && !architect.humanRequest; },
      returnToDocumentation: ticket => openSrsPage(ticket.folder, ticket.jobId),
    });
    void srs.connect().catch(() => undefined);
    let unlisten: (() => void) | undefined;
    let disconnectDecisions: (() => void) | undefined;
    void humanDecisions.connect({
      listen: receive => listen<HumanDecisionSnapshot>("human_decisions", event => receive(event.payload)),
      list: api.listHumanDecisions,
      respond: api.respondHumanDecision,
    }).then(cleanup => { if (disposed) cleanup(); else disconnectDecisions = cleanup; })
      .catch(error => { if (!disposed) humanDecisions.connectionError(error); });
    void configureBrowserSettings($appSettings.browserHeadless, $appSettings.browserEmbedded).then(() => {
      browserConfigured = true;
      return connectInstances();
    }).then(async cleanup => {
      if (disposed) { cleanup(); return; }
      unlisten = cleanup;
      if (startupSelected) return;
      startupSelected = true;
      restoredIds = new Set($instances.filter(row => row.kind === "instance").map(row => row.id));
      await selectStartup();
    }).catch(error => { if (!disposed) { initializing = false; reportError(browserConfigured ? "Connect desktop" : "Configure browser", error); } });
    function onToggle(): void { popoverOpen = !!document.querySelector("[popover]:popover-open"); }
    window.addEventListener("keydown", onTabKey, true);
    document.addEventListener("toggle", onToggle, true);
    return () => { disposed = true; unlisten?.(); disconnectDecisions?.(); srs.disposeStore(); window.removeEventListener("keydown", onTabKey, true); document.removeEventListener("toggle", onToggle, true); };
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
    <button disabled={initializing} class:active={view === "terminals"} class:attention={attention} title={attention ? "An agent is waiting for your decision" : "Every agent's history, human decisions and live commands"} aria-label={attention ? `Terminals - ${$humanDecisions.items.length} pending ${$humanDecisions.items.length === 1 ? "decision" : "decisions"}` : undefined} onclick={() => requestNavigation(openTerminalsFromButton)}>Terminals{#if $humanDecisions.items.length}<span class="pill">{$humanDecisions.items.length}</span>{/if}</button>
    {#each $systemOverview.tickets.filter(ticket => ticket.lateResult && ticket.state === "completed").slice(0, 1) as ticket (ticket.key)}
      <button class="primary" title="A requested diagram or structure preview was saved while you were elsewhere" onclick={() => systemOverview.openResult(ticket.key)}>Open System overview result</button>
    {/each}
    {#each $srs.tickets.filter(ticket => ticket.lateResult && $srs.jobs.get(ticket.jobId)?.state === "completed").slice(0, 1) as ticket (ticket.jobId)}
      <button class="primary" title="An SRS finished while you were elsewhere" onclick={() => srs.openResult(ticket.jobId)}>Open SRS documentation</button>
    {/each}
    <button aria-label="The Architect" disabled={openingArchitect || initializing} onclick={() => requestNavigation(openArchitect)}>The Architect</button>
    <button aria-label="The Manager" disabled={openingManager || initializing} onclick={() => requestNavigation(openManager)}>The Manager{#if needsHuman}<span class="pill warn">Needs you</span>{/if}</button>
    <ProjectPicker projects={$instances} label="Previous projects" disabled={initializing} onselect={id => requestNavigation(() => openProject(id))} />
    <button disabled={initializing} class="primary" onclick={newInstance}>New instance</button>
  </header>
  {#if browserSettingsError}<p class="err" role="alert">Configure browser: {browserSettingsError}</p>{/if}
  {#if provisioningError}
    <div class="err" role="alert">
      <p>Provision coordinators: {provisioningError}</p>
      <button disabled={initializing} onclick={() => void selectStartup()}>Retry</button>
      {#if !$managerInstance}<button onclick={() => configureCoordinator("manager", false)}>Configure manually — The Manager</button>{/if}
      {#if !$architectInstance}<button onclick={() => configureCoordinator("architect", false)}>Configure manually — The Architect</button>{/if}
    </div>
  {/if}
  <main class:focused={(view === "dashboard" && (!!focused || !!planChange)) || view === "code_diff" || view === "git_graph" || view === "terminals" || view === "dependency_tree" || view === "system_overview" || view === "code_quality" || view === "graphify" || view === "srs_documentation"}>
    {#if initializing}
      <p role="status">Loading workspace…</p>
    {:else if view === "agent_setup"}
      <AgentSetupPage bind:draft={setupDraft} bind:admission={setupAdmission} projectContext={setupProject} onback={() => view = "dashboard"} onSetupManager={() => configureCoordinator("manager", true)} onOpenManager={openManager} onSetupArchitect={() => configureCoordinator("architect", true)} onOpenArchitect={openArchitect} onSelectSession={focus} {onPlan} onPlanChange={openPlanChange} {onSubagents} onCodeDiff={openCodeDiff} onSystemOverview={() => openSystemOverview()} />
    {:else if view === "terminals"}
      <TerminalsPage onBack={() => requestNavigation(() => focus(null))} onOpenSession={id => void requestNavigation(() => { focus(id); focusRequest++; })} />
    {:else if view === "dependency_tree"}
      <DependencyTreePage initialFolder={analyzerInitialFolder} onBack={() => focus($architectInstance?.id ?? null)} {onOpenArchitect} onOpenManager={openManager} onOpenCodeQuality={openCodeQualityFromDependency} />
    {:else if view === "system_overview"}
      <SystemOverviewPage initialFolder={overviewEntry.folder} resume={overviewEntry.resume} onBack={() => requestNavigation(() => focus($architectInstance?.id ?? null))} {onOpenArchitect} onGraphify={openGraphify} onSrsDocumentation={openSrsStart}
        architect={architectDescriptor} />
    {:else if view === "graphify" && graphifyFolder}
      <GraphifyView folder={graphifyFolder} onBack={() => openSystemOverview(true)} />
    {:else if view === "srs_documentation" && srsPageFolder}
      <SrsDocumentationPage folder={srsPageFolder} initialJobId={srsPageJob} onBack={() => openSystemOverview(true)} onCreate={openSrsStart} onOpenArchitect={async id => { let opened = false; await requestNavigation(async () => { opened = await onOpenArchitect(id); }); return opened; }} />
    {:else if view === "code_quality"}
      <CodeQualityPage initialFolder={analyzerInitialFolder} onBack={closeCodeQuality} backLabel={qualityBackLabel} />
    {:else if view === "git_graph" && diffInstanceId}
      <GitGraphView instanceId={diffInstanceId} name={diffName} onBack={() => view = "code_diff"} onVisibleAgentChange={id => reportHelperVisible("git_graph", diffInstanceId, id)} />
    {:else if view === "code_diff" && diffInstanceId}
      <CodeDiffView instanceId={diffInstanceId} name={diffName} onBack={closeCodeDiff} onGitTree={() => view = "git_graph"} onGuardChange={guard => diffGuard = guard} onVisibleAgentChange={id => reportHelperVisible("code_diff", diffInstanceId, id)} />
    {:else if planChange && planTarget && planner}
      <PlanChangeView target={planTarget} {planner} admission={planChange.admission} onBack={() => planChange = null} onRetry={retryPlanContext} onCodeQuality={() => openCodeQuality(planner.id, { kind: "planChange" })} />
    {:else if focused}
      <SessionView instance={focused} instances={$instances} onSwitch={id => void openProject(id)} onClose={() => focus(null)} {onPlan} onPlanChange={openPlanChange} {onSubagents} {focusRequest} {onTerminalReady} onCodeDiff={openCodeDiff} onDependencyTree={() => openDependencyTree()} onSystemOverview={() => openSystemOverview()} onCodeQuality={id => openCodeQuality(id)} onFocusArchitect={focusArchitectOnly} showBrowser={!focused.helper} browserHidden={browserHidden} />
    {:else if $previewsEnabled}
      <Grid instances={overview} onSelect={focus} onOpen={openProject} {onPlan} onPlanChange={openPlanChange} {onSubagents} tileMin={$appSettings.previewSize} onCreate={newInstance} />
    {:else}
      <Tree list={overview} onSelect={focus} {onSubagents} {onSubagent} {onPlan} onPlanChange={openPlanChange} />
    {/if}
  </main>
</div>
<NewInstanceDialog open={dialogOpen} purpose={dialogPurpose} onclose={() => dialogOpen = false} oncreated={created} />
<GlobalConfigDialog open={configOpen} mode={configMode} initialSourceId={focused?.id || $managerInstance?.id || null} onclose={() => configOpen = false} />
<SettingsDialog open={settingsOpen} onclose={() => settingsOpen = false} />
<SrsStartDialog open={srsStartFolder !== null} folder={srsStartFolder ?? ""} architectModel={$architectInstance?.model ?? null}
  onClose={() => srsStartFolder = null} onExisting={folder => openSrsPage(folder)}
  onStarted={() => { srsStartFolder = null; }} onOpenArchitect={() => requestNavigation(openArchitect)} />
{#if panel && panelOwner}
  <div class="backdrop" role="presentation" onclick={e => { if (e.target === e.currentTarget) panel = null; }}>
    <div class="viewer panel" role="dialog" aria-modal="true" aria-label="OMP plan">
      <header><strong>{panelOwner.name}</strong><span class="spacer"></span><button onclick={() => panel = null}>Close</button></header>
      {#key panelOwner.id}
        <PlanPanel instanceId={panelOwner.id} />
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
  .toolbar button.attention { border-color: var(--warn); color: var(--warn); box-shadow: 0 0 0 1px var(--warn), 0 0 10px color-mix(in srgb, var(--warn) 55%, transparent); animation: attention-glow 2.4s ease-in-out infinite; }
  @keyframes attention-glow { 50% { box-shadow: 0 0 0 1px var(--warn), 0 0 2px color-mix(in srgb, var(--warn) 20%, transparent); } }
  @media (prefers-reduced-motion: reduce) { .toolbar button.attention { animation: none; } }
</style>
