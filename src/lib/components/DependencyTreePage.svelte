<script module lang="ts">
  // Positions found by earlier layouts, per scanned folder, so toggling a view starts from where the nodes were.
  const layouts = new Map<string, Map<string, { x: number; y: number }>>();
  const PREFS_KEY = "omp-manager.dependency-tree";
  const filterOverrides = new Set<string>();
  let filterSettingsKey: string | null = null;

  // Deterministic start position from an FNV-1a hash of the package id (ForceAtlas2 needs distinct positions).
  function seeded(id: string): { x: number; y: number } {
    let h = 2166136261;
    for (let i = 0; i < id.length; i++) { h ^= id.charCodeAt(i); h = Math.imul(h, 16777619); }
    const a = (h >>> 0) / 4294967296;
    h = Math.imul(h ^ (h >>> 15), 2246822507);
    h ^= h >>> 13;
    return { x: (a - 0.5) * 200, y: ((h >>> 0) / 4294967296 - 0.5) * 200 };
  }
</script>
<script lang="ts">
  import { onMount, tick, untrack } from "svelte";
  import { on } from "svelte/events";
  import Graph from "graphology";
  import forceAtlas2 from "graphology-layout-forceatlas2";
  import Sigma from "sigma";
  import { api } from "../api";
  import { errorText, instances, appSettings, knownStandardVersion, architectInstance, reportError } from "../stores";
  import { pickFolder } from "../ui";
  import { projectFolders, repoKey } from "../repo-agents";
  import { ROOT, nodeColor, nodeSize, pendingDirect, visibleGraph, pathToNode, focusCamera, packageTotals, buildPackageSearchIndex, searchPackages, exactNameMatch, parseDependencyPrefs, type KnownStandard, type Classification, type DepNode, type DependencyGraph, type DependencyMode, type HideSets, type VisibleGraph, type DerivedStandard } from "../dependency-graph";
  import { DependencyReviewSession, DependencyJobSession, PackageInsightsQueue, PAGE_SIZE, formatCount, githubProblemText, githubStatusText, newestText, ownerText, packageMajorLag, projectVersionText, rankPackages, rankingIncomplete, type DependencyJobAdmission, type HandoffPrepared, type InhouseStatus, type PackageInsightsStatus, type ProjectReviewState, type UpdatePlanState, type UsageExplanationState } from "../package-insights";
  import { isCommonlyUsed, knownOwnerOf } from "../known-packages";
  import { InhouseSweep, type SweepSnapshot } from "../inhouse-sweep";
  import { KindProbe, projectFilterVerdict, projectEcosystems, languageLabel, type ProjectKindStatus, type KindSnapshot } from "../project-filter";
  import DependencyPackageDetails from "./DependencyPackageDetails.svelte";
  import DependencySecurityDialog from "./DependencySecurityDialog.svelte";
  import DependencyUsageDialog from "./DependencyUsageDialog.svelte";
  import DependencyProjectDialog from "./DependencyProjectDialog.svelte";
  import DependencyUpdatePlanDialog from "./DependencyUpdatePlanDialog.svelte";
  let { onBack, onOpenArchitect, onOpenManager }: {
    onBack: () => void;
    onOpenArchitect: (id: string, replayRequestId?: string) => Promise<boolean>;
    onOpenManager: () => Promise<void>;
  } = $props();

  const SEARCH_PAGE = 30;
  const saved = parseDependencyPrefs(localStorage.getItem(PREFS_KEY));
  let folder = $state(saved.folder);
  let mode = $state<DependencyMode>(saved.mode);
  let showAll = $state(saved.showAll);
  let hideStandard = $state(saved.hideStandard);
  let hidePink = $state(saved.hidePink);
  let hideCommon = $state(saved.hideCommon);
  let hideOwners = $state(saved.hideOwners);
  let presentation = $state(saved.presentation);
  let layers = $state<number | null>(saved.layers);
  let page = $state(0);
  let activeFolder = "";
  const insightQueue = new PackageInsightsQueue(async (id, github, refresh) => {
    const result = await api.getPackageInsights(activeFolder, id, github, refresh);
    if (github !== "skip") refreshStatus();
    return result;
  }, () => {
    metadata = { insights: insightQueue.insights, errors: insightQueue.errors };
  });
  let metadata = $state.raw({ insights: insightQueue.insights, errors: insightQueue.errors });
  let reviewNode = $state.raw<DepNode | null>(null);
  const reviewSession = new DependencyReviewSession({
    start: api.startDependencySecurityReview, read: api.readDependencySecurityReview,
  }, snapshot => { review = snapshot; });
  let review = $state.raw(reviewSession.snapshot);
  let usageNode = $state.raw<DepNode | null>(null);
  const usageSession = new DependencyJobSession<UsageExplanationState>({
    start: (target, packageId, rerun) => api.startDependencyUsageExplanation(target, packageId ?? "", rerun), read: api.readDependencyUsageExplanation,
  }, snapshot => { usage = snapshot; });
  let usage = $state.raw(usageSession.snapshot);
  let projectOpen = $state(false);
  const projectSession = new DependencyJobSession<ProjectReviewState>({
    start: (target, _subject, rerun) => api.startProjectDependencySecurityReview(target, rerun), read: api.readProjectDependencySecurityReview,
  }, snapshot => { project = snapshot; });
  let project = $state.raw(projectSession.snapshot);
  let planOpen = $state(false);
  let planSource: string | null = null;
  const planSession = new DependencyJobSession<UpdatePlanState>({
    start: (target, reviewRequestId, regenerate) => api.startDependencyUpdatePlan(target, reviewRequestId ?? "", regenerate), read: api.readDependencyUpdatePlan,
  }, snapshot => { plan = snapshot; });
  let plan = $state.raw(planSession.snapshot);
  let prepared = $state.raw<HandoffPrepared | null>(null);
  let handoffError = $state<string | null>(null);
  let handoffBusy = $state(false);
  let reviewAction = 0, usageAction = 0, projectAction = 0, planAction = 0;
  // Only an index of which jobs exist: the backend decides whether to reuse, so a remount or restart still finds saved work.
  type JobKind = "security" | "usage" | "project";
  const JOBS_KEY = "omp-manager.dependency-jobs";
  function loadJobs(): Set<string> {
    try {
      const value = JSON.parse(localStorage.getItem(JOBS_KEY) ?? "[]") as unknown;
      return new Set(Array.isArray(value) ? value.filter((entry): entry is string => typeof entry === "string").slice(-2000) : []);
    } catch { return new Set(); }
  }
  let savedJobs = $state.raw(loadJobs());
  const jobKey = (kind: JobKind, target: string, id: string): string => `${kind}|${repoKey(target)}|${id}`;
  const hasSaved = (kind: JobKind, id: string): boolean => savedJobs.has(jobKey(kind, folder, id));
  function remember(kind: JobKind, target: string, id: string): void {
    const key = jobKey(kind, target, id);
    if (savedJobs.has(key)) return;
    const list = [...savedJobs, key].slice(-2000);
    savedJobs = new Set(list);
    try { localStorage.setItem(JOBS_KEY, JSON.stringify(list)); } catch { /* the index is only a convenience */ }
  }
  let renderedGraph = $state.raw<Graph | null>(null);
  let graph = $state.raw<DependencyGraph | null>(null);
  let classification = $state.raw<Classification>({ items: {}, updatedAt: null });
  let loading = $state(false);
  let error = $state<string | null>(null);
  let classifying = $state(false);
  let classifyError = $state<string | null>(null);
  let hovered = $state<string | null>(null);
  let selected = $state<string | null>(null);
  let layoutMs = $state(0);
  let container = $state<HTMLDivElement>();
  let pathTarget = $state<string | null>(null);
  let pathFromSearch = $state(false);
  let searchRevealed = $state(false);
  let query = $state("");
  let searchOpen = $state(false);
  let searchLimit = $state(SEARCH_PAGE);
  let activeIndex = $state(-1);
  let centerRequest = $state.raw<{ id: string; folder: string } | null>(null);
  let listFocus = $state<string | null>(null);
  let hoverIntent = $state<string | null>(null);
  let activeRenderer: Sigma | null = null;
  let cameraText = $state("0.500,0.500,1.000");
  let bulkActive = $state(false);
  let bulkPausedUntil = $state<number | null>(null);
  let pauseTimer: ReturnType<typeof setTimeout> | null = null;
  let status = $state.raw<PackageInsightsStatus | null>(null);
  let statusRequestedAt = 0;
  let statusReady = $state(false);
  let inhouse = $state.raw<InhouseStatus | null>(null);
  let inhouseError = $state<string | null>(null);
  let projectKind = $state.raw<ProjectKindStatus | null>(null);
  let kindError = $state<string | null>(null);
  let known = $state.raw<KnownStandard | null>(null);
  let derivedStd = $state.raw<DerivedStandard | null>(null);
  let derivedError = $state<string | null>(null);
  let recheckBusy = $state(false);
  let githubRecheckError = $state<string | null>(null);
  let githubCheckedAt = 0;
  let overrideTick = $state(0);
  let sweepState = $state.raw<SweepSnapshot>({ running: false, error: null, warning: null, status: null });
  let kindState = $state.raw<KindSnapshot>({ running: false, error: null, warning: null, status: null });
  const sweep = new InhouseSweep({
    start: api.startInhouseSweep, status: f => api.getInhouseStatus(f, false, $appSettings.packageLookupGate),
    now: Date.now, setTimer: (fn, ms) => setTimeout(fn, ms), clearTimer: handle => clearTimeout(handle as ReturnType<typeof setTimeout>),
    changed: snapshot => { sweepState = snapshot; if (snapshot.status) inhouse = snapshot.status; },
    admitted: a => { if (a.promptSent === true && a.remaining === 0) onBack(); },
  });
  const kindProbe = new KindProbe({
    start: api.startProjectKind, status: api.getProjectKind,
    now: Date.now, setTimer: (fn, ms) => setTimeout(fn, ms), clearTimer: handle => clearTimeout(handle as ReturnType<typeof setTimeout>),
    changed: snapshot => { kindState = snapshot; if (snapshot.status) projectKind = snapshot.status; },
    admitted: a => { if (a.promptSent === true) onBack(); },
  });
  const knownIds = $derived(new Set(Object.keys(known?.items ?? {})));
  const derivedHide = $derived(new Set(derivedStd?.hide ?? []));
  const differentById = $derived(new Map((derivedStd?.different ?? []).map(entry => [entry.id, entry])));
  const unprovenById = $derived(new Map((derivedStd?.unproven ?? []).map(entry => [entry.id, entry])));
  const differentIds = $derived(new Set(differentById.keys()));
  const hideSet = $derived(hideStandard ? new Set([...knownIds, ...derivedHide].filter(id => !differentIds.has(id))) : null);
  const hidePinkSet = $derived(hidePink ? differentIds : null);
  const knownInGraph = $derived(graph?.nodes.filter(node => node.id !== ROOT && (knownIds.has(node.id) || derivedHide.has(node.id))).length ?? 0);
  const derivedInGraph = $derived(graph?.nodes.filter(node => node.id !== ROOT && derivedHide.has(node.id)).length ?? 0);
  const pinkInGraph = $derived(graph?.nodes.filter(node => differentIds.has(node.id)).length ?? 0);
  const commonIds = $derived(new Set((graph?.nodes ?? []).filter(isCommonlyUsed).map(node => node.id)));
  function ownerOf(node: DepNode) {
    const info = metadata.insights.get(node.id);
    return knownOwnerOf(node, info?.repositoryUrl, info?.repository?.ownerLogin);
  }
  // Keyed by content so metadata batches that find no new owner never re-layout the graph.
  const ownerKey = $derived((graph?.nodes ?? []).filter(node => node.id !== ROOT && ownerOf(node)).map(node => node.id).sort().join("\u0000"));
  const ownerIds = $derived(new Set(ownerKey ? ownerKey.split("\u0000") : []));
  const commonInGraph = $derived(commonIds.size);
  const ownersInGraph = $derived(ownerIds.size);
  const unprovenInGraph = $derived(graph?.nodes.filter(node => unprovenById.has(node.id)).length ?? 0);
  const ecosystems = $derived(graph ? projectEcosystems(graph.nodes) : new Set<string>());
  const verdict = $derived.by(() => { overrideTick; return projectFilterVerdict($appSettings, ecosystems, projectKind, filterOverrides.has(repoKey(folder))); });
  const gated = $derived($appSettings.packageLookupGate && !(inhouse?.gate === true && inhouse.complete));
  const held = $derived(verdict.kind !== "ok" || gated);
  const inhouseSet = $derived(gated || !$appSettings.packageLookupGate || !inhouse ? new Set<string>() : new Set(Object.keys(inhouse.inhouse)));

  const folders = $derived(projectFolders($instances));
  const folderOptions = $derived(folder && !folders.some(known => repoKey(known) === repoKey(folder)) ? [folder, ...folders] : folders);
  const empty: VisibleGraph = { nodes: [], edges: [], hidden: 0, depthHidden: 0, maxDepth: 0, standardHidden: 0, differentHidden: 0, commonHidden: 0, ownersHidden: 0 };
  const hideSets = $derived<HideSets>({ known: hideSet, different: hidePinkSet, common: hideCommon ? commonIds : null, owners: hideOwners ? ownerIds : null });
  const transitive = $derived(graph ? visibleGraph(graph, classification, "transitive", showAll, null, hideSets) : empty);
  const visible = $derived(graph ? mode === "transitive" && layers === null ? transitive : visibleGraph(graph, classification, mode, showAll, layers, hideSets) : empty);
  // A search result hidden by the view filters is shown against a temporary unfiltered base; saved filters are never touched.
  const rawBase = $derived(graph && searchRevealed ? visibleGraph(graph, classification, "transitive", true, null) : null);
  const pathBase = $derived(rawBase ?? visible);
  // The package's own transitive packages come with its path, down to the selected layer (All = every one; Direct view = none).
  const foundPath = $derived(pathTarget ? pathToNode(pathBase, pathTarget, mode === "transitive" ? layers : undefined) : null);
  const pathView = $derived.by((): VisibleGraph | null => {
    if (!pathTarget || foundPath || !pathFromSearch) return foundPath;
    // The scanner found no project -> package path: show only the project and the package, with no invented edge.
    const root = pathBase.nodes.find(node => node.id === ROOT);
    const target = pathBase.nodes.find(node => node.id === pathTarget);
    return root && target ? { ...pathBase, nodes: [root, target], edges: [] } : null;
  });
  const pathUnresolved = $derived(pathFromSearch && pathTarget !== null && pathView !== null && foundPath === null);
  const displayed = $derived(pathView ?? visible);
  const ranked = $derived(rankPackages(displayed.nodes, metadata.insights));
  const publicNodes = $derived(ranked.filter(node => !inhouseSet.has(node.id)));
  const bulkIds = $derived(visible.nodes.filter(node => node.id !== ROOT && !inhouseSet.has(node.id)).map(node => node.id));
  const bulkLoaded = $derived(bulkIds.filter(id => metadata.insights.has(id) && !metadata.insights.get(id)!.githubPending).length);
  const pageCount = $derived(Math.max(1, Math.ceil(ranked.length / PAGE_SIZE)));
  const pageNodes = $derived(ranked.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE));
  const lookedUp = $derived(publicNodes.filter(node => metadata.insights.has(node.id)).length);
  const lookupFailures = $derived(publicNodes.filter(node => metadata.errors.has(node.id)).length);
  const incomplete = $derived(rankingIncomplete(publicNodes, metadata.insights));
  const focusId = $derived(hovered ?? selected);
  const focusNode = $derived(focusId ? displayed.nodes.find(node => node.id === focusId) ?? null : null);
  const focusInsights = $derived(focusNode ? metadata.insights.get(focusNode.id) : undefined);
  const focusError = $derived(focusNode ? metadata.errors.get(focusNode.id) : undefined);
  const reviewInsights = $derived(reviewNode ? metadata.insights.get(reviewNode.id) : undefined);
  const reviewMetadataError = $derived(reviewNode ? metadata.errors.get(reviewNode.id) : undefined);
  const pending = $derived(graph ? pendingDirect(graph, classification, knownIds) : []);
  const directCount = $derived(graph ? graph.nodes.filter(node => node.direct).length : 0);
  const classified = $derived(directCount - pending.length);
  const names = $derived(new Map(displayed.nodes.map(node => [node.id, node.name])));
  const dependsOn = $derived(focusId ? displayed.edges.filter(([from]) => from === focusId).map(([, to]) => to) : []);
  const requiredBy = $derived(focusId ? displayed.edges.filter(([, to]) => to === focusId).map(([from]) => from) : []);
  const shownCount = $derived(displayed.nodes.filter(node => node.id !== ROOT).length);
  const totals = $derived(graph ? packageTotals(graph, classification, knownIds, derivedHide) : { total: 0, nonBuiltIn: 0, unclassified: 0 });
  const forceLabels = $derived(mode === "direct" || pathTarget !== null || (mode === "transitive" && layers !== null && layers <= 2));
  const searchIndex = $derived(graph ? buildPackageSearchIndex(graph.nodes) : []);
  const matches = $derived(searchPackages(searchIndex, query));
  const shownMatches = $derived(matches.slice(0, searchLimit));
  const allNames = $derived(new Map((graph?.nodes ?? []).map(node => [node.id, node.name])));
  const reviewExplainDisabled = $derived(!reviewNode || reviewNode.id === ROOT || held || inhouseSet.has(reviewNode.id));
  function derivedNoteFor(node: DepNode): string | undefined {
    const different = differentById.get(node.id);
    if (different) return `Different version than the built-in packages: this project uses ${different.versions.join(", ")}; built-in packages use ${different.standardVersions.join(", ")}.`;
    if (unprovenById.has(node.id)) return "Also used by a built-in package; its version could not be proven from local files.";
    return undefined;
  }
  function badges(node: DepNode): string[] {
    const list: string[] = [];
    if (classification.items[node.id]?.basic === true || knownIds.has(node.id)) list.push("Built-in");
    else if (derivedHide.has(node.id)) list.push("Built-in (derived)");
    if (differentIds.has(node.id)) list.push("Pink");
    if (commonIds.has(node.id)) list.push("Commonly used");
    const owner = ownerOf(node);
    if (owner) list.push(`Known owner: ${owner.name}`);
    list.push(node.direct ? "Direct" : "Transitive");
    if (inhouseSet.has(node.id)) list.push("In-house");
    return list;
  }

  $effect(() => { if (!folder && folders.length) folder = folders[0]; });
  $effect(() => { localStorage.setItem(PREFS_KEY, JSON.stringify({ folder, mode, showAll, hideStandard, hidePink, hideCommon, hideOwners, presentation, layers })); });
  $effect(() => { if (page >= pageCount) page = pageCount - 1; });
  $effect(() => { if (pathTarget && graph && !pathView) setPath(null); });
  $effect(() => { const last = shownMatches.length - 1; if (activeIndex > last) activeIndex = last; });
  // Centre a search result once the path view's renderer has processed the node; stale renderers/folders are ignored.
  $effect(() => {
    const built = renderedGraph;
    const request = centerRequest;
    if (!built || !request || presentation !== "graph") return;
    const frame = requestAnimationFrame(() => {
      const renderer = activeRenderer;
      if (request.folder !== folder || centerRequest !== request || renderedGraph !== built || !renderer || !built.hasNode(request.id)) return;
      const data = renderer.getNodeDisplayData(request.id);
      if (!data) return;
      renderer.getCamera().animate({ x: data.x, y: data.y, ratio: 0.25 }, { duration: 500 });
      centerRequest = null;
    });
    return () => cancelAnimationFrame(frame);
  });
  // In List, put the result on its containing page of the path set and focus its row once; later metadata reranking never refocuses.
  $effect(() => {
    const id = listFocus;
    if (!id || presentation !== "list" || pathTarget !== id) return;
    const index = ranked.findIndex(node => node.id === id);
    if (index < 0) return;
    listFocus = null;
    page = Math.floor(index / PAGE_SIZE);
    void tick().then(() => document.querySelector<HTMLButtonElement>(`tr[data-package-id="${CSS.escape(id)}"] .package-name`)?.focus());
  });
  $effect(() => {
    const id = hovered;
    hoverIntent = null;
    const timer = setTimeout(() => { hoverIntent = id; }, 400);
    return () => clearTimeout(timer);
  });
  $effect(() => {
    if (!statusReady || held) { insightQueue.prioritize({ demand: [], registry: [], bulk: [] }); return; }
    const demand = [reviewNode?.id, selected, hoverIntent, ...(presentation === "list" ? pageNodes.map(node => node.id) : [])]
      .filter((id): id is string => !!id && id !== ROOT && !inhouseSet.has(id));
    insightQueue.prioritize({ demand, registry: bulkIds, bulk: bulkActive && !bulkPausedUntil ? bulkIds : [] });
  });
  $effect(() => {
    if (held || !bulkActive) return;
    let until = 0;
    for (const id of bulkIds) {
      const repo = metadata.insights.get(id)?.repository;
      for (const metric of [repo?.stars, repo?.contributors]) if (metric?.status === "rateLimited") until = Math.max(until, metric.retryAt ?? 0);
    }
    if (until <= Date.now() || until === bulkPausedUntil) return;
    bulkPausedUntil = until;
    if (pauseTimer) clearTimeout(pauseTimer);
    pauseTimer = setTimeout(() => { bulkPausedUntil = null; insightQueue.forget(bulkIds); pauseTimer = null; }, until - Date.now() + 1000);
  });
  $effect(() => {
    const key = JSON.stringify([$appSettings.manifestFilter, $appSettings.filterLanguages, $appSettings.aiFilter, $appSettings.filterRoles]);
    untrack(() => { if (filterSettingsKey !== key) { filterOverrides.clear(); overrideTick++; filterSettingsKey = key; } });
  });
  $effect(() => {
    const gate = $appSettings.packageLookupGate;
    const scanned = graph;
    if (!scanned || !untrack(() => statusReady)) return;
    const target = folder;
    untrack(() => { statusReady = false; });
    void api.getInhouseStatus(target, false, gate).then(value => { if (target === folder) { inhouse = value; inhouseError = null; } })
      .catch(e => { if (target === folder) { inhouse = null; inhouseError = errorText(e); } })
      .finally(() => { if (target === folder) statusReady = true; });
  });
  $effect(() => {
    const active = $appSettings.aiFilter && $appSettings.filterRoles.length > 0;
    if (!active || !graph || projectKind) return;
    const target = folder;
    void api.getProjectKind(target).then(value => { if (target === folder) projectKind = value; }).catch(e => { if (target === folder) kindError = errorText(e); });
  });
  let seenKnownVersion = $knownStandardVersion;
  $effect(() => {
    const version = $knownStandardVersion;
    untrack(() => { if (version === seenKnownVersion) return; seenKnownVersion = version; if (graph) void refreshStandard(); });
  });
  async function refreshKnown(): Promise<void> {
    const sequence = loadSequence;
    try { const value = await api.getKnownStandard(); if (sequence === loadSequence) known = value; } catch { if (sequence === loadSequence) known = null; }
  }
  async function refreshDerived(): Promise<void> {
    const sequence = loadSequence;
    try {
      const value = await api.getDerivedStandard(activeFolder);
      if (sequence === loadSequence) { derivedStd = value; derivedError = null; }
    } catch (e) {
      if (sequence === loadSequence) { derivedStd = null; derivedError = errorText(e); }
    }
  }
  async function refreshStandard(): Promise<void> { await refreshKnown(); await refreshDerived(); }
  function applyStatus(value: PackageInsightsStatus): void {
    const regained = status?.githubAuthenticated === false && value.githubAuthenticated;
    status = value;
    // A new credential: earlier rate-limited GitHub metrics are fetched again.
    if (regained) {
      bulkPausedUntil = null;
      if (pauseTimer) clearTimeout(pauseTimer);
      pauseTimer = null;
      insightQueue.forget(bulkIds);
    }
  }
  function refreshStatus(): void {
    if (Date.now() - statusRequestedAt < 2000) return;
    statusRequestedAt = Date.now();
    void api.getPackageInsightsStatus().then(applyStatus).catch(() => {});
  }
  async function recheckGithub(): Promise<void> {
    if (recheckBusy) return;
    recheckBusy = true;
    githubCheckedAt = Date.now();
    githubRecheckError = null;
    try { applyStatus(await api.recheckGithubCredential()); }
    catch (e) { githubRecheckError = errorText(e); }
    finally { recheckBusy = false; }
  }
  function overrideFilter(): void { filterOverrides.add(repoKey(folder)); overrideTick++; }
  function resetView(): void {
    const fromSearch = pathFromSearch;
    setPath(null);
    selected = null;
    if (fromSearch) { query = ""; searchOpen = false; activeIndex = -1; searchLimit = SEARCH_PAGE; }
    activeRenderer?.getCamera().animate({ x: .5, y: .5, ratio: 1, angle: 0 }, { duration: 400 });
    renderedGraph?.forEachNode(id => renderedGraph?.setNodeAttribute(id, "labelVisibility", forceLabels ? "visible" : "auto"));
  }

  function setPath(id: string | null, fromSearch = false, revealed = false): void {
    pathTarget = id;
    pathFromSearch = id !== null && fromSearch;
    searchRevealed = id !== null && revealed;
    if (id === null) { centerRequest = null; listFocus = null; }
  }
  function selectSearchResult(node: DepNode): void {
    if (!graph || node.id === ROOT) return;
    setPath(node.id, true, !visible.nodes.some(entry => entry.id === node.id));
    selected = node.id;
    hovered = null;
    searchOpen = false;
    centerRequest = presentation === "graph" ? { id: node.id, folder } : null;
    listFocus = presentation === "list" ? node.id : null;
  }
  function onSearchInput(event: Event & { currentTarget: HTMLInputElement }): void {
    query = event.currentTarget.value;
    searchLimit = SEARCH_PAGE;
    activeIndex = -1;
    searchOpen = true;
    if (!query.trim() && pathFromSearch) setPath(null);
  }
  function clearSearch(): void {
    query = "";
    searchOpen = false;
    activeIndex = -1;
    searchLimit = SEARCH_PAGE;
    if (pathFromSearch) setPath(null);
  }
  function onSearchKey(event: KeyboardEvent): void {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      if (!shownMatches.length) return;
      event.preventDefault();
      searchOpen = true;
      activeIndex = event.key === "ArrowDown" ? Math.min(activeIndex + 1, shownMatches.length - 1) : Math.max(activeIndex - 1, 0);
      void tick().then(() => document.getElementById(`package-search-option-${activeIndex}`)?.scrollIntoView({ block: "nearest" }));
    } else if (event.key === "Enter") {
      event.preventDefault();
      const node = searchOpen && activeIndex >= 0 ? shownMatches[activeIndex] : exactNameMatch(searchIndex, query) ?? matches[0];
      if (node) selectSearchResult(node);
    } else if (event.key === "Escape" && searchOpen) {
      event.preventDefault();
      event.stopPropagation();
      searchOpen = false;
    }
  }

  function closeReview(): void { reviewNode = null; reviewSession.close(); }
  function closeUsage(): void { usageNode = null; usageSession.close(); }
  function closeProject(): void { projectOpen = false; projectSession.close(); }
  function closePlan(): void { planOpen = false; planSession.close(); prepared = null; handoffError = null; handoffBusy = false; }
  function resetJobs(): void {
    reviewAction++; usageAction++; projectAction++; planAction++;
    closeReview(); closeUsage(); closeProject(); closePlan();
    reviewSession.reset(); usageSession.reset(); projectSession.reset(); planSession.reset();
  }
  const blockedFromAi = (node: DepNode): boolean => node.id === ROOT || held || inhouseSet.has(node.id);
  /**
   * Opens the existing Architect for an authoritatively admitted job. Returns false when the saved popup must show the outcome instead:
   * errors, known-unsent prompts, a missing Architect or a failed resume/replay. Never resends a prompt.
   */
  async function routeToArchitect(admission: DependencyJobAdmission | null, snapshot: { state: { status: string } | null; error: string | null }, current: () => boolean): Promise<boolean> {
    if (!admission || admission.error || snapshot.error || admission.disposition === "error") return false;
    const status = snapshot.state?.status ?? (admission.disposition === "complete" ? "complete" : "pending");
    if (status === "error") return false;
    if (admission.promptSent === false && status !== "complete") return false;
    const id = admission.architectId;
    if (!id) return false;
    if (!current()) return true;
    if (admission.promptSent === null && status === "pending") reportError("Dependency analysis", "The Architect prompt may not have been delivered; it was not sent again.");
    return await onOpenArchitect(id, status === "complete" ? admission.requestId : undefined);
  }
  async function openArchitectFor(snapshot: { requestId: string | null; admission: DependencyJobAdmission | null; state: { status: string } | null }): Promise<void> {
    const id = snapshot.admission?.architectId ?? $architectInstance?.id ?? null;
    if (!id) { reportError("Open The Architect", "The Architect has not been created yet. Create it from its toolbar button."); return; }
    await onOpenArchitect(id, snapshot.state?.status === "complete" && snapshot.requestId ? snapshot.requestId : undefined);
  }
  async function openReview(node: DepNode, destination: "popup" | "architect", rerun = false): Promise<void> {
    if (blockedFromAi(node)) return;
    const target = folder;
    const sequence = ++reviewAction;
    const current = () => sequence === reviewAction && target === folder;
    reviewNode = destination === "popup" ? node : null;
    const admission = await reviewSession.open(target, node.id, rerun);
    if (!current()) return;
    if (admission) remember("security", target, node.id);
    if (destination === "architect" && !await routeToArchitect(admission, reviewSession.snapshot, current) && current()) reviewNode = node;
  }
  async function openUsage(node: DepNode, destination: "popup" | "architect", rerun = false): Promise<void> {
    if (blockedFromAi(node)) return;
    const target = folder;
    const sequence = ++usageAction;
    const current = () => sequence === usageAction && target === folder;
    usageNode = destination === "popup" ? node : null;
    const admission = await usageSession.open(target, node.id, rerun);
    if (!current()) return;
    if (admission) remember("usage", target, node.id);
    if (destination === "architect" && !await routeToArchitect(admission, usageSession.snapshot, current) && current()) usageNode = node;
  }
  async function openProjectReview(rerun = false): Promise<void> {
    if (held || !graph || loading) return;
    const target = folder;
    const sequence = ++projectAction;
    projectOpen = true;
    const admission = await projectSession.open(target, null, rerun);
    if (sequence === projectAction && target === folder && admission) remember("project", target, "");
  }
  function openProjectPackage(packageId: string): void {
    const node = graph?.nodes.find(entry => entry.id === packageId);
    if (!node) return;
    closeProject();
    void openReview(node, "popup");
  }
  async function openUpdatePlan(reviewRequestId: string | null, regenerate = false): Promise<void> {
    if (!reviewRequestId) return;
    const target = folder;
    const sequence = ++planAction;
    planSource = reviewRequestId;
    planOpen = true;
    prepared = null;
    handoffError = null;
    await planSession.open(target, reviewRequestId, regenerate);
    if (sequence !== planAction || target !== folder) return;
  }
  function updatePlanFromReview(): void {
    const id = review.requestId;
    closeReview();
    void openUpdatePlan(id);
  }
  function updatePlanFromProject(): void {
    const id = project.requestId;
    closeProject();
    void openUpdatePlan(id);
  }
  async function prepareHandoff(): Promise<void> {
    const requestId = plan.requestId;
    const proposal = plan.state?.proposal;
    if (!requestId || !proposal || handoffBusy) return;
    const target = folder;
    handoffBusy = true;
    handoffError = null;
    try {
      const result = await api.prepareDependencyUpdateHandoff(requestId, proposal.revisionId);
      if (target === folder && planOpen) prepared = result;
    } catch (e) { if (target === folder) handoffError = errorText(e); }
    finally { handoffBusy = false; }
  }
  async function confirmHandoff(): Promise<void> {
    const token = prepared?.confirmationToken;
    if (!token || handoffBusy) return;
    handoffBusy = true;
    handoffError = null;
    try {
      const result = await api.confirmDependencyUpdateHandoff(token);
      prepared = null;
      if (result.status === "accepted") {
        closePlan();
        await onOpenManager();
      } else {
        handoffError = result.error ?? (result.status === "uncertain"
          ? "Delivery to The Manager is uncertain; the plan was not sent again."
          : "The plan was not delivered to The Manager.");
      }
    } catch (e) { prepared = null; handoffError = errorText(e); }
    finally { handoffBusy = false; }
  }
  function cancelHandoff(): void { prepared = null; handoffError = null; }
  function selectPackage(event: MouseEvent, node: DepNode): void {
    if (event.ctrlKey || event.metaKey) { event.preventDefault(); void openReview(node, "architect"); }
    else selected = node.id;
  }
  // Delegate row clicks without replacing native table/button keyboard semantics.
  function rowInteractions(element: HTMLTableSectionElement): { destroy: () => void } {
    return { destroy: on(element, "click", event => {
      if (!(event.target instanceof Element)) return;
      const id = event.target.closest<HTMLTableRowElement>("tr[data-package-id]")?.dataset.packageId;
      const node = displayed.nodes.find(entry => entry.id === id);
      if (node) selectPackage(event, node);
    }) };
  }

  let loadSequence = 0;
  async function load(target: string): Promise<void> {
    const sequence = ++loadSequence;
    stopPolling();
    sweep.stop(); kindProbe.stop();
    activeFolder = target;
    statusReady = false; inhouse = null; inhouseError = null; projectKind = null; kindError = null; known = null;
    setPath(null); query = ""; searchOpen = false; searchLimit = SEARCH_PAGE; activeIndex = -1; derivedStd = null; derivedError = null; bulkActive = false; bulkPausedUntil = null;
    if (pauseTimer) clearTimeout(pauseTimer);
    pauseTimer = null;
    resetJobs();
    insightQueue.reset();
    graph = null;
    hovered = null;
    selected = null;
    page = 0;
    loading = true;
    error = null;
    classifyError = null;
    try {
      const [scanned, judged] = await Promise.all([api.scanDependencies(target), api.getDependencyClassification(target)]);
      if (sequence !== loadSequence) return;
      graph = scanned;
      classification = judged;
      await refreshKnown();
      if (sequence !== loadSequence) return;
      await refreshDerived();
      if (sequence !== loadSequence) return;
      try { const value = await api.getInhouseStatus(target, true, $appSettings.packageLookupGate); if (sequence === loadSequence) inhouse = value; }
      catch (e) { if (sequence === loadSequence) { inhouseError = errorText(e); inhouse = null; } }
      if (sequence !== loadSequence) return;
      if ($appSettings.aiFilter && $appSettings.filterRoles.length > 0) {
        try { const value = await api.getProjectKind(target); if (sequence === loadSequence) projectKind = value; } catch (e) { if (sequence === loadSequence) kindError = errorText(e); }
      }
      if (sequence !== loadSequence) return;
      statusReady = true;
    } catch (e) {
      if (sequence !== loadSequence) return;
      graph = null;
      error = errorText(e);
    } finally {
      if (sequence === loadSequence) loading = false;
    }
  }
  $effect(() => {
    const target = folder;
    if (target) untrack(() => void load(target));
  });

  let pollTimer: ReturnType<typeof setInterval> | null = null;
  function stopPolling(): void {
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = null;
    classifying = false;
  }
  async function classify(): Promise<void> {
    const target = folder;
    if (!target) return;
    const sequence = loadSequence;
    classifyError = null;
    classifying = true;
    try {
      const admission = await api.classifyDependencies(target);
      if (target !== folder || sequence !== loadSequence) return;
      if (admission.promptSent === true && admission.requested > 0) { onBack(); return; }
      if (admission.error) classifyError = admission.error;
      if (admission.requested === 0 || admission.promptSent === false) { classifying = false; return; }
      const until = Date.now() + 10 * 60_000;
      pollTimer = setInterval(() => {
        void api.getDependencyClassification(target).then(judged => {
          if (target !== folder || sequence !== loadSequence) return;
          classification = judged;
          if ((graph && pendingDirect(graph, judged, knownIds).length === 0) || Date.now() > until) { stopPolling(); void refreshStandard(); }
        }).catch(e => { if (target !== folder || sequence !== loadSequence) return; classifyError = errorText(e); stopPolling(); });
      }, 2000);
    } catch (e) {
      if (target !== folder || sequence !== loadSequence) return;
      classifyError = errorText(e);
      classifying = false;
    }
  }
  async function browse(): Promise<void> {
    const picked = await pickFolder();
    if (picked) folder = picked;
  }

  // One Sigma renderer per visible graph; the layout is computed up front so the first frame is already settled.
  $effect(() => {
    const view = displayed;
    const host = container;
    const scanned = graph;
    const judged = classification;
    const display = presentation;
    if (held || display !== "graph" || !host || !scanned || view.nodes.length === 0) { layoutMs = 0; return; }
    const started = performance.now();
    const inDegree = new Map<string, number>();
    for (const [, to] of view.edges) inDegree.set(to, (inDegree.get(to) ?? 0) + 1);
    const cache = layouts.get(scanned.key) ?? new Map<string, { x: number; y: number }>();
    layouts.set(scanned.key, cache);
    const built = new Graph({ type: "directed" });
    for (const node of view.nodes) {
      const at = cache.get(node.id) ?? seeded(node.id);
      built.addNode(node.id, {
        label: node.name,
        labelVisibility: forceLabels ? "visible" : "auto",
        size: node.id === ROOT ? 16 : nodeSize(inDegree.get(node.id) ?? 0),
        color: nodeColor(node, judged.items[node.id]?.basic === true, null, untrack(() => differentIds.has(node.id))),
        x: at.x,
        y: at.y,
      });
    }
    for (const [from, to] of view.edges) if (!built.hasEdge(from, to)) built.addEdge(from, to, { size: 1, color: "#87919e" });
    if (built.order > 1) {
      forceAtlas2.assign(built, {
        iterations: Math.min(500, Math.max(20, Math.round(1_500_000 / built.order))),
        settings: { ...forceAtlas2.inferSettings(built), barnesHutOptimize: built.order > 500, slowDown: 5 },
      });
    }
    built.forEachNode((id, attributes) => cache.set(id, { x: attributes.x as number, y: attributes.y as number }));
    const renderer = new Sigma(built, host, {
      settings: { enableCameraRotation: false, enableCameraMouseRotation: false, enableCameraPanning: true, enableCameraZooming: true, minCameraRatio: 0.02, maxCameraRatio: 3, renderEdgeLabels: false, labelRenderedSizeThreshold: 6, labelDensity: 0.6, itemSizesReference: "screen", enableEdgeEvents: false },
      styles: {
        nodes: [
          { color: { attribute: "color" }, size: { attribute: "size", defaultValue: 6 }, label: { attribute: "label" }, labelVisibility: { attribute: "labelVisibility", defaultValue: "auto" }, labelColor: "#dfe6ef" },
          { whenState: "isHovered", then: { labelVisibility: "visible" } },
        ],
        edges: [{ color: { attribute: "color" }, size: { attribute: "size", defaultValue: 1 } }],
      },
    });
    activeRenderer = renderer;
    const updateCamera = () => { const c = renderer.getCamera().getState(); cameraText = [c.x, c.y, c.ratio].map(value => value.toFixed(3)).join(","); };
    updateCamera();
    renderer.getCamera().on("updated", updateCamera);
    renderer.on("rightClickNode", ({ node, event }) => { event.original.preventDefault(); if (node !== ROOT) setPath(node); });
    renderer.on("rightClickStage", ({ event }) => { event.original.preventDefault(); setPath(null); });
    renderer.on("doubleClickNode", ({ node, event }) => {
      event.preventSigmaDefault();
      const center = renderer.getNodeDisplayData(node);
      if (!center) return;
      const ids = view.edges.filter(([from]) => from === node).map(([, to]) => to);
      const neighbors = ids.flatMap(id => { const data = renderer.getNodeDisplayData(id); return data ? [data] : []; });
      const { width, height } = renderer.getDimensions();
      const override = { cameraState: { x: .5, y: .5, angle: 0, ratio: 1 } };
      const a = renderer.viewportToFramedGraph({ x: 0, y: 0 }, override);
      const b = renderer.viewportToFramedGraph({ x: width, y: height }, override);
      renderer.getCamera().animate(focusCamera(center, neighbors, { width: Math.abs(b.x - a.x), height: Math.abs(b.y - a.y) }), { duration: 600 });
      built.forEachNode(id => built.setNodeAttribute(id, "labelVisibility", forceLabels ? "visible" : "auto"));
      for (const id of [node, ...ids]) built.setNodeAttribute(id, "labelVisibility", "visible");
    });
    renderer.on("enterNode", ({ node }) => { hovered = node; });
    renderer.on("leaveNode", () => { hovered = null; });
    renderer.on("clickNode", ({ node, event }) => {
      const original = event.original;
      if (original instanceof MouseEvent && (original.ctrlKey || original.metaKey)) {
        event.preventSigmaDefault();
        const target = view.nodes.find(entry => entry.id === node);
        if (target) void openReview(target, "architect");
        return;
      }
      selected = node;
    });
    renderer.on("clickStage", () => { selected = null; });
    layoutMs = Math.round(performance.now() - started);
    renderedGraph = built;
    return () => { renderer.getCamera().removeListener("updated", updateCamera); activeRenderer = null; renderer.kill(); renderedGraph = null; hovered = null; };
  });

  // HTTP responses update only colour attributes. They are deliberately not layout dependencies.
  $effect(() => {
    const built = renderedGraph;
    if (!built) return;
    for (const node of displayed.nodes) {
      if (built.hasNode(node.id)) built.setNodeAttribute(node.id, "color", nodeColor(node, classification.items[node.id]?.basic === true, packageMajorLag(node, metadata.insights.get(node.id)), differentIds.has(node.id)));
    }
  });

  onMount(() => {
    refreshStatus();
    const keydown = (event: KeyboardEvent) => { if (event.key === "Escape" && !document.querySelector("dialog[open]")) setPath(null); };
    // Back in the window after `gh auth login`: look for the new credential, at most every ten seconds.
    const focusListener = () => { if (status?.githubAuthenticated === false && Date.now() - githubCheckedAt >= 10_000) void recheckGithub(); };
    window.addEventListener("keydown", keydown);
    window.addEventListener("focus", focusListener);
    return () => { loadSequence++; stopPolling(); sweep.stop(); kindProbe.stop(); insightQueue.stop(); resetJobs(); if (pauseTimer) clearTimeout(pauseTimer); window.removeEventListener("keydown", keydown); window.removeEventListener("focus", focusListener); };
  });

  const legend = [
    { label: "Project", color: "#4ea1ff" },
    { label: "Chosen direct dependency", color: "#3fb950" },
    { label: "Basic direct dependency (Show all)", color: "#d29922" },
    { label: "Transitive dependency", color: "#61708a" },
    { label: "At least 2 numeric majors behind registry (not a security verdict)", color: "#f85149" },
    { label: "Different version than the built-in packages use", color: "#ff69b4" },
  ];
</script>
<section class="page">
  <header class="bar">
    <button onclick={onBack}>Back to The Architect</button><h2>Dependency tree</h2>
    <select aria-label="Project folder" value={folder} onchange={e => { folder = e.currentTarget.value; }}>
      {#if !folderOptions.length}<option value="">No project folders yet</option>{/if}
      {#each folderOptions as option (option)}<option value={option}>{option}</option>{/each}
    </select>
    <button onclick={browse}>Browse…</button>
    {#if !held}
    <select aria-label="Dependency view" value={mode} onchange={e => { mode = e.currentTarget.value as DependencyMode; }}>
      <option value="direct">Direct dependencies</option>
      <option value="transitive">Transitive dependencies</option>
    </select>
    {#if mode === "transitive"}
      <label class="layers">Layers
        <select aria-label="Transitive layers" value={layers === null ? "all" : String(layers)} onchange={e => { layers = e.currentTarget.value === "all" ? null : Number(e.currentTarget.value); page = 0; }}>
          <option value="all">All</option>
          {#each Array.from({ length: Math.max(transitive.maxDepth, layers ?? 0) }, (_, index) => index + 1) as depth}<option value={String(depth)}>{depth}</option>{/each}
        </select>
      </label>
    {/if}
    <button onclick={() => { presentation = presentation === "graph" ? "list" : "graph"; }}>{presentation === "graph" ? "List view" : "Graph view"}</button>
    {#if presentation === "graph" || pathTarget !== null}<button onclick={resetView}>Reset view</button>{/if}
    <button class:active={showAll} aria-pressed={showAll} title="Also show packages the Architect judged built in / basic" onclick={() => { showAll = !showAll; }}>Show all</button>
    <button aria-pressed={hideStandard} class:active={hideStandard} disabled={knownInGraph === 0} title={knownInGraph === 0 ? "No known standard packages in this project yet. Classify with The Architect to learn them." : "Hides packages The Architect has judged standard (built in) in any project, wherever they appear here. Show all overrides this."} onclick={() => { hideStandard = !hideStandard; }}>Remove known standard packages{knownInGraph ? ` (${knownInGraph})` : ""}</button>
    <button aria-pressed={hidePink} class:active={hidePink} disabled={pinkInGraph === 0} title={pinkInGraph === 0 ? "No packages use a different version than the built-in packages." : "Pink nodes are transitive packages used here in a different version than the built-in packages use. Toggle to hide them and packages reachable only through them."} onclick={() => { hidePink = !hidePink; }}>Hide pink nodes{pinkInGraph ? ` (${pinkInGraph})` : ""}</button>
    <button aria-pressed={hideCommon} class:active={hideCommon} disabled={commonInGraph === 0} title={commonInGraph === 0 ? "No commonly used packages in this project." : "Hides packages whose name already says what they do (prettier, jest, stylelint, test-runner…) and packages reachable only through them, in every language. Show all overrides this."} onclick={() => { hideCommon = !hideCommon; }}>Commonly used packages{commonInGraph ? ` (${commonInGraph})` : ""}</button>
    <button aria-pressed={hideOwners} class:active={hideOwners} disabled={ownersInGraph === 0} title={ownersInGraph === 0 ? "No packages from known owners were found yet (owners are read from the packages' repository links as they load)." : "Hides packages owned by well-known companies (Google, Microsoft, Amazon, NVIDIA, Netflix…) and packages reachable only through them. Show all overrides this."} onclick={() => { hideOwners = !hideOwners; }}>Known owners{ownersInGraph ? ` (${ownersInGraph})` : ""}</button>
    <button onclick={() => { bulkActive = !bulkActive; }}>{bulkActive ? "Stop loading" : "Load GitHub stats for all shown packages"}</button>
    <span class="meta">{bulkLoaded}/{bulkIds.length} loaded</span>
    {#if status}
      <span class="meta">{githubStatusText(status)}</span>
      {#if !status.githubAuthenticated}<button disabled={recheckBusy} onclick={() => void recheckGithub()}>{recheckBusy ? "Checking…" : "Recheck GitHub login"}</button>{/if}
      {#if githubRecheckError}<span class="meta" role="alert">{githubRecheckError}</span>{/if}
      {#if githubProblemText(status)}<span class="meta warn" role="status">{githubProblemText(status)}</span>{/if}
    {/if}
    {#if bulkPausedUntil}<span class="meta">Paused: GitHub budget used; resumes at {new Date(bulkPausedUntil).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}</span>{/if}
    <div class="search" role="presentation" onfocusout={event => { if (!event.currentTarget.contains(event.relatedTarget as Node | null)) searchOpen = false; }}>
      <input type="text" aria-label="Search all packages" aria-controls="package-search-results" aria-activedescendant={searchOpen && activeIndex >= 0 ? `package-search-option-${activeIndex}` : undefined} placeholder="Search all packages, including built-ins…" autocomplete="off" spellcheck="false" value={query} disabled={!graph} oninput={onSearchInput} onfocus={() => { if (query.trim()) searchOpen = true; }} onkeydown={onSearchKey} />
      {#if query}<button onclick={clearSearch}>Clear search</button>{/if}
      {#if searchOpen && query.trim()}
        <div class="suggestions" id="package-search-results">
          <p class="meta" role="status">{matches.length ? `${matches.length} matching ${matches.length === 1 ? "package" : "packages"}` : "No matching packages"}</p>
          {#if shownMatches.length}
            <ul class="results" aria-label="Package search results">
              {#each shownMatches as node, index (node.id)}
                <li><button type="button" id={`package-search-option-${index}`} class:active={index === activeIndex} onmousedown={event => event.preventDefault()} onclick={() => selectSearchResult(node)}><strong>{node.name}</strong><span class="dim">{node.ecosystem}</span>{#each badges(node) as badge}<span class="badge">{badge}</span>{/each}</button></li>
              {/each}
            </ul>
          {/if}
          {#if matches.length > shownMatches.length}<button type="button" onmousedown={event => event.preventDefault()} onclick={() => { searchLimit += SEARCH_PAGE; }}>Show more results</button>{/if}
        </div>
      {/if}
    </div>
    <span class="spacer"></span>
    <button disabled={held || !graph || loading} title="Reviews every direct dependency, including hidden built-in packages; checks transitive dependencies where relevant." onclick={() => void openProjectReview()}>Full project security review</button>
    {#if hasSaved("project", "")}<button disabled={held || !graph || loading} onclick={() => void openProjectReview()}>Open saved project review</button>{/if}
    <button disabled={classifying || !graph || pending.length === 0} title="Ask The Architect which direct dependencies are basic (built in) and which were chosen" onclick={classify}>Classify with The Architect</button>
    {/if}
  </header>
  {#if held}
    <div class="held">
      <div class="held-panel">
        {#if verdict.kind === "manifest"}
          <h2>Dependency tree skipped by your settings</h2>
          <p>Your settings skip the Dependency tree for {verdict.languages.map(languageLabel).join(", ")} projects (detected from this project's manifests). You can change this under Settings → Dependency tree filters.</p>
          <button onclick={overrideFilter}>Build the dependency tree anyway</button>
        {:else if verdict.kind === "ask"}
          <h2>Checking the project type</h2>
          <p>Your settings skip backend or frontend projects, decided by The Architect. It has not looked at this project yet. The Architect reads the project's files.</p>
          <button disabled={kindState.running} onclick={() => kindProbe.run(folder, projectKind?.updatedAt ?? null)}>{kindState.running ? "Waiting for The Architect…" : "Ask the Architect what kind of project this is"}</button>
        {:else if verdict.kind === "ai"}
          <h2>Dependency tree skipped by your settings</h2>
          <p>Based on your filter, The Architect concluded this is a {verdict.role} project ({verdict.reason}), which your settings skip. Do you want to build the dependency tree anyway?</p>
          <button onclick={overrideFilter}>Build the dependency tree anyway</button>
          <button disabled={kindState.running} onclick={() => kindProbe.run(folder, projectKind?.updatedAt ?? null)}>Ask the Architect again</button>
        {:else}
          <h2>Package lookups are on hold</h2>
          <p>No package is looked up until The Architect has decided which packages are in-house. The Architect sees the package names.</p>
          <button disabled={sweepState.running || !folder || loading} onclick={() => sweep.run(folder)}>{sweepState.running ? `Waiting for The Architect… ${inhouse?.judged ?? 0}/${inhouse?.total ?? 0}` : inhouse && inhouse.judged > 0 ? "Continue asking The Architect" : "Ask the Architect which packages are in-house"}</button>
        {/if}
        {#if verdict.kind !== "ok"}
          {#if kindState.error || kindError}<p role="alert">{kindState.error ?? kindError}</p>{/if}
          {#if kindState.warning}<p class="dim" role="alert">{kindState.warning}</p>{/if}
        {:else}
          {#if sweepState.error || inhouseError}<p role="alert">{sweepState.error ?? inhouseError}</p>{/if}
          {#if sweepState.warning}<p class="dim">{sweepState.warning}</p>{/if}
        {/if}
        {#if error}<p role="alert">{error}</p>{/if}
      </div>
    </div>
  {:else}
  <ul class="legend" aria-label="Colours">
    {#each legend as entry}<li><span class="swatch" style:background={entry.color}></span>{entry.label}</li>{/each}
  </ul>
  <p class="meta">Hover for package statistics and versions. Ctrl+click a package to open The Architect with its security review.</p>
  <p class="meta">Public metadata lookup sends package names to official registries and GitHub; it does not upload project files.</p>
  {#if loading}<p role="status">Scanning dependencies…</p>{/if}
  {#if error}<p class="alert err" role="alert">{error}</p>{/if}
  {#if classifyError}<p class="alert err" role="alert">{classifyError}</p>{/if}
  {#if graph}
    <p class="meta" role="status">
      {shownCount} packages shown · {displayed.hidden} hidden as built in{#if hideStandard} · {visible.standardHidden} removed as known standard ({derivedInGraph} derived from built-in packages){/if} · {displayed.depthHidden} hidden by depth{#if hidePink} · {visible.differentHidden} pink hidden{/if}{#if hideCommon} · {visible.commonHidden} commonly used hidden{/if}{#if hideOwners} · {visible.ownersHidden} known owners hidden{/if}{#if pinkInGraph} · {pinkInGraph} pink{/if}{#if unprovenInGraph} · {unprovenInGraph} version unproven{/if} · Total packages in project: {totals.total} · Total non-built-in packages in repo: {totals.nonBuiltIn} · {classified}/{directCount} direct packages classified{#if classifying} · Waiting for The Architect… ({classified}/{directCount}){/if}
      <br />Non-built-in includes packages not yet classified.
      {#if classified === 0 && directCount > 0}<br />Not classified yet: showing every package. Press "Classify with The Architect".{/if}
    </p>
    {#if graph.notes.length}<ul class="notes dim">{#each graph.notes as note}<li>{note}</li>{/each}</ul>{/if}
    {#if derivedError}<p class="meta dim" role="status">Built-in transitive analysis failed: {derivedError}</p>{/if}
  {/if}
  {#if pathTarget && pathView}
    {#if searchRevealed}
      <p class="meta banner" role="status">Search result: {names.get(pathTarget) ?? pathTarget} — temporarily showing packages hidden by view filters <button onclick={() => setPath(null)}>Return to filtered view</button></p>
    {:else}
      <p class="meta banner">Showing paths from the project to {names.get(pathTarget) ?? pathTarget} · {shownCount} packages <button onclick={() => setPath(null)}>Show full graph</button></p>
    {/if}
    {#if pathUnresolved}<p class="meta">No dependency path was resolved by the scanner.</p>{/if}
  {/if}
  <div class="stage">
    {#if presentation === "graph"}
      <div class="canvas" role="presentation" bind:this={container} oncontextmenu={event => event.preventDefault()} data-nodes={displayed.nodes.length} data-edges={displayed.edges.length} data-hidden={displayed.hidden} data-depth-hidden={displayed.depthHidden} data-layout-ms={layoutMs} data-forced-labels={forceLabels ? displayed.nodes.length : 0} data-camera={cameraText}></div>
      {#if graph && displayed.nodes.length <= 1}<p class="empty">No packages to show.</p>{/if}
    {:else}
      <div class="list" class:with-card={focusNode !== null}>
        <p class="meta">Sorted by stars ↑, then GitHub contributors ↑; unknown values last.</p>
        <p class="meta" role="status">Metadata available for {lookedUp}/{publicNodes.length} packages{#if lookupFailures} · {lookupFailures} lookup errors{/if}{#if incomplete}<br />Ranking is incomplete; unresolved packages appear last.{/if}</p>
        <div class="table-scroll">
          <table>
            <caption class="dim">{ranked.length} filtered packages · click a package to pin details; Ctrl+click or use Security review to ask The Architect.</caption>
            <thead><tr><th scope="col">Package</th><th scope="col">Project version(s)</th><th scope="col">Newest (registry)</th><th scope="col">GitHub stars</th><th scope="col">GitHub contributors</th><th scope="col">Owner</th><th scope="col">Downloads (period)</th><th scope="col">Actions</th></tr></thead>
            <tbody use:rowInteractions>
              {#each pageNodes as node (node.id)}
                {@const info = metadata.insights.get(node.id)}
                {@const failure = metadata.errors.get(node.id)}
                {@const lag = packageMajorLag(node, info)}
                <tr data-package-id={node.id} class:pinned={selected === node.id}>
                  <td><button class="package-name" onfocus={() => { selected = node.id; }}>{#if lag !== null && lag >= 2}<span class="lag-marker" title={`At least one resolved version is ${lag} numeric majors behind the registry release; not a security verdict`} aria-label={`${lag} numeric majors behind registry`}>●</span>{/if}{node.name}</button><small class="dim">{node.ecosystem}{node.dev ? " · dev" : ""}</small></td>
                  <td>{projectVersionText(node)}</td>
                  {#if inhouseSet.has(node.id)}
                    <td>In-house — not looked up</td><td>—</td><td>—</td><td>—</td><td>—</td>
                  {:else}
                  <td title={failure ?? info?.notes.join(" · ")}>{newestText(info, failure)}</td>
                  <td title={failure ?? info?.repositoryReason ?? info?.repository?.stars.reason ?? ""}>{failure ? "Unknown" : info?.githubPending ? "Not loaded" : info && !info.repository ? "Unavailable" : formatCount(info?.repository?.stars)}</td>
                  <td title={failure ?? info?.repositoryReason ?? info?.repository?.contributors.reason ?? ""}>{failure ? "Unknown" : info?.githubPending ? "Not loaded" : info && !info.repository ? "Unavailable" : formatCount(info?.repository?.contributors)}</td>
                  <td>{info ? ownerText(info.repository) : failure ? "unknown" : "Loading…"}</td>
                  <td title={failure ?? info?.downloads.reason ?? ""}>{failure ? "Unknown" : formatCount(info?.downloads)}{#if info?.downloads.period}<small class="dim">{info.downloads.period}</small>{/if}</td>
                  {/if}
                  <td><button disabled={inhouseSet.has(node.id)} onclick={event => { event.stopPropagation(); void openReview(node, "popup"); }}>{hasSaved("security", node.id) ? "Open saved review" : "Security review"}</button></td>
                </tr>
              {/each}
            </tbody>
          </table>
          {#if graph && !ranked.length}<p>No packages to show.</p>{/if}
        </div>
        <nav class="pagination" aria-label="Package pages">
          <button disabled={page === 0} onclick={() => { page--; }}>Previous</button>
          <span>Page {page + 1}/{pageCount} · {ranked.length} packages</span>
          <button disabled={page + 1 >= pageCount} onclick={() => { page++; }}>Next</button>
        </nav>
      </div>
    {/if}
    {#if focusNode}
      <aside class="card" class:pinned={selected === focusNode.id} inert={selected !== focusNode.id} aria-label="Package details">
        <div class="card-heading"><strong>{focusNode.name}</strong>{#if selected === focusNode.id}<button aria-label="Unpin package details" onclick={() => { selected = null; }}>×</button>{/if}</div>
        <span class="dim">{focusNode.ecosystem} · {focusNode.id === ROOT ? "project" : focusNode.direct ? "direct" : "transitive"}{focusNode.dev ? " · dev" : ""}</span>
        <DependencyPackageDetails node={focusNode} insights={focusInsights} error={focusError} derivedNote={derivedNoteFor(focusNode)} savedReview={hasSaved("security", focusNode.id)} savedUsage={hasSaved("usage", focusNode.id)} explainDisabled={held || blockedFromAi(focusNode)} inhouseReason={inhouseSet.has(focusNode.id) ? inhouse?.inhouse[focusNode.id] ?? "" : undefined} refreshing={!!metadata && insightQueue.refreshing.has(focusNode.id)} onrefresh={() => { if (focusNode && !held && !inhouseSet.has(focusNode.id)) void insightQueue.refresh(focusNode.id); }} onreview={() => { if (focusNode) void openReview(focusNode, "popup"); }} onexplain={() => { if (focusNode) void openUsage(focusNode, hasSaved("usage", focusNode.id) ? "popup" : "architect"); }} />
        {#if classification.items[focusNode.id]}
          <span>{classification.items[focusNode.id].basic ? "Basic" : "Chosen"}{classification.items[focusNode.id].reason ? ` — ${classification.items[focusNode.id].reason}` : ""}</span>
        {/if}
        <span class="dim">Depends on {dependsOn.length} · Required by {requiredBy.length}</span>
        {#if selected === focusNode.id}
          {#if dependsOn.length}<span class="dim">Depends on: {dependsOn.slice(0, 12).map(id => names.get(id) ?? id).join(", ")}{dependsOn.length > 12 ? ", …" : ""}</span>{/if}
          {#if requiredBy.length}<span class="dim">Required by: {requiredBy.slice(0, 12).map(id => names.get(id) ?? id).join(", ")}{requiredBy.length > 12 ? ", …" : ""}</span>{/if}
        {/if}
      </aside>
    {/if}
  </div>
  {/if}
</section>
<DependencySecurityDialog node={reviewNode} insights={reviewInsights} metadataError={reviewMetadataError} {review} explainDisabled={reviewExplainDisabled} onclose={closeReview} onrun={() => { if (reviewNode) void openReview(reviewNode, "popup", true); }} onexplain={() => { const node = reviewNode; if (!node) return; closeReview(); void openUsage(node, "architect"); }} onupdateplan={updatePlanFromReview} onopenarchitect={() => void openArchitectFor(review)} />
<DependencyUsageDialog node={usageNode} {usage} onclose={closeUsage} onrun={() => { if (usageNode) void openUsage(usageNode, "popup", true); }} onopenarchitect={() => void openArchitectFor(usage)} />
<DependencyProjectDialog open={projectOpen} {project} names={allNames} onclose={closeProject} onrun={() => void openProjectReview(true)} onupdateplan={updatePlanFromProject} onopenarchitect={() => void openArchitectFor(project)} onopenpackage={openProjectPackage} />
<DependencyUpdatePlanDialog plan={planOpen ? plan : null} {prepared} {handoffError} busy={handoffBusy} onclose={closePlan} onregenerate={() => void openUpdatePlan(planSource, true)} onprepare={() => void prepareHandoff()} onconfirm={() => void confirmHandoff()} oncancelprepare={cancelHandoff} />
<style>
  .page { height: 100%; min-height: 0; display: flex; flex-direction: column; gap: 10px; }
  .bar { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; }
  h2 { margin: 0; font-size: 18px; }
  select { width: auto; max-width: 42ch; }
  .meta { margin: 0; color: var(--text-dim); font-size: 12px; }
  .notes { margin: 0; padding-left: 18px; font-size: 12px; }
  .alert { margin: 0; padding: 10px; border: 1px solid var(--err); border-radius: 8px; white-space: pre-wrap; }
  .legend { list-style: none; margin: 0; padding: 0; display: flex; flex-wrap: wrap; gap: 4px 16px; font-size: 12px; }
  .legend li { display: inline-flex; align-items: center; gap: 6px; }
  .swatch { width: 12px; height: 12px; border-radius: 50%; flex: none; }
  .stage { position: relative; flex: 1; min-height: 0; border: 1px solid var(--line); border-radius: 8px; overflow: hidden; background: #0b1017; }
  .held { flex: 1; min-height: 0; display: grid; place-items: center; overflow: auto; }
  .held-panel { max-width: 660px; padding: 24px; line-height: 1.6; text-align: center; }
  .canvas { position: absolute; inset: 0; }
  .empty { position: absolute; inset: 0; display: grid; place-items: center; margin: 0; color: var(--text-faint); pointer-events: none; }
  .card { position: absolute; top: 10px; right: 10px; width: min(380px, calc(100% - 20px)); max-height: calc(100% - 20px); box-sizing: border-box; display: flex; flex-direction: column; gap: 8px; padding: 10px 12px; background: var(--panel); border: 1px solid var(--line); border-radius: 8px; font-size: 12px; overflow: auto; overflow-wrap: anywhere; pointer-events: none; }
  .card.pinned { pointer-events: auto; user-select: text; }
  .card-heading { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
  .card-heading button { padding: 2px 8px; }
  .layers { display: flex; align-items: center; gap: 6px; }
  .list { height: 100%; display: flex; flex-direction: column; gap: 10px; padding: 12px; box-sizing: border-box; }
  .list.with-card { padding-right: min(402px, 45%); }
  .table-scroll { flex: 1; min-height: 0; overflow: auto; }
  table { width: 100%; border-collapse: collapse; font-size: 12px; text-align: left; }
  th, td { padding: 8px; border-bottom: 1px solid var(--line); vertical-align: top; }
  th { white-space: nowrap; position: sticky; top: 0; background: var(--panel); }
  td { min-width: 90px; overflow-wrap: anywhere; }
  td small { display: block; margin-top: 4px; }
  caption { text-align: left; padding-bottom: 10px; }
  .package-name { text-align: left; white-space: normal; }
  tr.pinned { background: #162131; }
  .lag-marker { color: #f85149; margin-right: 5px; }
  .search { position: relative; display: inline-flex; align-items: center; gap: 6px; }
  .search input { width: 30ch; }
  .suggestions { position: absolute; top: calc(100% + 4px); left: 0; z-index: 30; width: min(460px, 80vw); max-height: 380px; overflow: auto; box-sizing: border-box; display: flex; flex-direction: column; gap: 6px; padding: 8px; background: var(--panel); border: 1px solid var(--line); border-radius: 8px; box-shadow: 0 8px 24px #0008; }
  .results { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 2px; }
  .results button { width: 100%; display: flex; flex-wrap: wrap; align-items: center; justify-content: flex-start; gap: 8px; text-align: left; }
  .results button.active { outline: 2px solid #4ea1ff; }
  .badge { padding: 0 6px; font-size: 11px; border: 1px solid var(--line); border-radius: 999px; color: var(--text-dim); }
  .banner { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
  .pagination { display: flex; align-items: center; gap: 12px; }
</style>
