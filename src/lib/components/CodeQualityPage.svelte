<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { api } from "../api";
  import { instances, projectHistory, visibleProjectHistory, computedFolders, rememberProjectFolder } from "../stores";
  import { pickFolder } from "../ui";
  import { projectFolders, repoKey } from "../repo-agents";
  import { folderOptions as historyOptions } from "../project-history";
  import { codeQualityClient } from "../code-quality-client";
  import { isActiveScan, modeLabel, pageScreen, parseQualityPreferences, qualityLocked, QualityGeneration, QualityViewEntry, runtimeLabel, scanAction, setupAction, STARTING_TEXT, type QualityProject, type QualityProjectDetection, type QualityScan, type QualityScanPreparation } from "../code-quality";
  import CodeQualitySettings from "./CodeQualitySettings.svelte";
  import CodeQualityReport from "./CodeQualityReport.svelte";
  import CodeQualityOwnership from "./CodeQualityOwnership.svelte";
  import CodeQualityCredentials from "./CodeQualityCredentials.svelte";
  let { onBack, initialFolder, backLabel = "Back to The Architect" }: { onBack: () => void; initialFolder?: string; backLabel?: string } = $props();
  const PREFS_KEY = "omp-manager.code-quality";
  const saved = parseQualityPreferences(localStorage.getItem(PREFS_KEY));
  const service = codeQualityClient;
  const quality = $derived($service.state);
  const locked = $derived(qualityLocked($service));
  let entered = $state(false);
  let view = $state<"scan" | "report">("scan");
  let folder = $state(untrack(() => initialFolder || saved.folder || $visibleProjectHistory[0] || ""));
  let tool = $state(saved.tool), severity = $state(saved.severity), text = $state(saved.text);
  let project = $state<QualityProject | null>(null);
  let detection = $state.raw<QualityProjectDetection | null>(null);
  let selectionConfirmed = $state(false);
  let buildArgs = $state(""), exclusions = $state("");
  let preparation = $state.raw<QualityScanPreparation | null>(null);
  let setupConsent = $state(false);
  let loading = $state(false);
  let error = $state("");
  let scans = $state.raw<QualityScan[]>([]);
  let selectedId = $state("");
  let scan = $state.raw<QualityScan | null>(null);
  const folderGeneration = new QualityGeneration();
  let currentFolderGeneration = 0, prepareGeneration = 0, scanReadGeneration = 0;
  let pollReading = false;
  let watchedScanId: string | null = null;
  const screen = $derived(pageScreen(quality, entered));
  const setup = $derived(setupAction(quality));
  let startingScan = $state(false);
  let recipeApproval = $state(false);
  const folders = $derived(projectFolders($instances));
  const folderOptions = $derived(historyOptions($projectHistory, folders, folder, $computedFolders));
  const draftProject = $derived<QualityProject | null>(project ? { ...project, buildArgs: buildArgs.split("\n").map(value => value.trim()).filter(Boolean), exclusions: exclusions.split("\n").map(value => value.trim()).filter(Boolean) } : null);
  const preparationKey = $derived(JSON.stringify([draftProject, quality?.revision]));
  const selectionNeeded = $derived(quality?.config.mode !== "alternate" && !!detection?.needsSelection && !selectionConfirmed);
  const entryNeeded = $derived(quality?.config.mode !== "alternate" && project?.scanner === "dotnet" && !project.entryFile && (detection?.entryFiles.length ?? 0) > 1);
  const prepareWanted = $derived(!!draftProject && !!quality && !loading && !selectionNeeded && !entryNeeded);
  const projectReady = $derived(!!detection && !!draftProject && !!preparation && !loading && !selectionNeeded && !entryNeeded);
  const credentialMissing = $derived(quality?.config.mode === "sonarqubeCommunity" ? !quality.credentials.community : quality?.config.mode === "sonarqubeEnterprise" ? !quality.credentials.enterprise : false);
  const runtimeReady = $derived(quality?.prerequisites.some(item => item.runtime.kind === quality.config.runtime.kind && item.ready) ?? false);
  const activeId = $derived(quality?.activeScanId ?? (isActiveScan(scan) ? scan?.id ?? null : null));
  const action = $derived(scanAction({ quality, preparationReady: !!preparation && !loading, selectionNeeded, entryNeeded: !!entryNeeded, requiresConsent: !!preparation?.requiresConsent, activeScanId: activeId, locked: !activeId && locked }));
  const scanBlocked = $derived(!quality || credentialMissing || !runtimeReady || !quality.effectiveMode || !!quality.unresolved.length || (quality.config.mode === "sonarqubeCommunity" && quality.community.running !== true) || (quality.config.mode === "alternate" && !quality.rules.acknowledged));
  const actionDisabled = $derived(action.kind === "cancel" ? $service.pending || $service.uncertain || quality?.ownership.status !== "owned" || quality?.phase === "cancelling" : action.kind === "blocked" || scanBlocked || startingScan);
  const scannerNote = $derived(quality?.config.mode === "alternate" ? "MegaLinter + Opengrep + Semgrep CE — no build" : project?.scanner === "cli" ? "SonarScanner CLI — no build, no approval needed" : project ? `${project.scanner === "maven" ? "Maven" : project.scanner === "gradle" ? "Gradle" : ".NET"} build in the isolated snapshot — one approval per recipe` : "");
  const serverOrigin = $derived.by(() => {
    if (!quality || quality.config.mode === "alternate") return null;
    try { return new URL(quality.config.mode === "sonarqubeCommunity" ? quality.community.url ?? `http://127.0.0.1:${quality.config.communityPort}` : quality.config.enterpriseUrl).origin; } catch { return quality.config.enterpriseUrl; }
  });
  $effect(() => {
    const target = folder;
    untrack(() => { void loadFolder(target); });
  });
  $effect(() => {
    preparationKey;
    untrack(() => { prepareGeneration++; preparation = null; });
  });
  // The recipe is prepared automatically (debounced while options are edited); preparation never touches an engine.
  $effect(() => {
    preparationKey;
    if (!prepareWanted) return;
    const timer = window.setTimeout(() => { if (!preparation) void prepare(); }, 300);
    return () => window.clearTimeout(timer);
  });
  $effect(() => {
    folder; tool; severity; text;
    try { localStorage.setItem(PREFS_KEY, JSON.stringify({ folder, tool, severity, text })); } catch { /* Preferences never own scans or configuration. */ }
  });
  // A scan that was active and is now terminal opens the report.
  $effect(() => {
    const current = scan;
    untrack(() => {
      if (watchedScanId !== null && current?.id === watchedScanId && !isActiveScan(current)) view = "report";
      watchedScanId = current && isActiveScan(current) ? current.id : null;
    });
  });
  async function loadFolder(target: string): Promise<void> {
    const generation = folderGeneration.begin(target); currentFolderGeneration = generation; scanReadGeneration++;
    project = null; detection = null; scans = []; selectedId = ""; scan = null; selectionConfirmed = false; error = ""; loading = !!target; view = "scan";
    if (!target) return;
    try {
      const [detected, history] = await Promise.allSettled([api.detectCodeQualityProject(target), api.listCodeQualityScans(target)]);
      if (!folderGeneration.accepts(generation, folder)) return;
      if (detected.status === "fulfilled") {
        rememberProjectFolder(detected.value.project.folder, true);
        detection = detected.value; project = detected.value.project; buildArgs = project.buildArgs.join("\n"); exclusions = project.exclusions.join("\n");
      } else error = String(detected.reason);
      if (history.status === "fulfilled") {
        const canonical = project?.folder ?? target;
        scans = history.value.filter(item => repoKey(item.folder) === repoKey(canonical)).slice(0, 20);
        selectedId = scans[0]?.id ?? ""; scan = scans[0] ?? null;
        view = scans[0] && !isActiveScan(scans[0]) ? "report" : "scan";
      } else error = [error, String(history.reason)].filter(Boolean).join("\n");
    } catch (failure) { if (folderGeneration.accepts(generation, folder)) error = String(failure); }
    finally { if (folderGeneration.accepts(generation, folder)) loading = false; }
  }
  async function browse(): Promise<void> {
    try { const picked = await pickFolder(); if (picked) folder = picked; } catch (failure) { error = String(failure); }
  }
  async function prepare(): Promise<void> {
    if (!draftProject) return;
    const candidate = structuredClone($state.snapshot(draftProject)), generation = ++prepareGeneration;
    const folderToken = currentFolderGeneration, target = folder;
    error = ""; preparation = null;
    try {
      const result = await api.prepareCodeQualityScan(candidate);
      if (generation === prepareGeneration && folderGeneration.accepts(folderToken, folder) && repoKey(target) === repoKey(folder)) preparation = result;
    } catch (failure) { if (generation === prepareGeneration) error = String(failure); }
  }
  async function readScan(id: string): Promise<QualityScan | null> {
    const generation = ++scanReadGeneration, folderToken = currentFolderGeneration;
    try {
      const result = await api.readCodeQualityScan(id);
      if (!folderGeneration.accepts(folderToken, folder) || generation !== scanReadGeneration || selectedId !== id || repoKey(result.folder) !== repoKey(project?.folder ?? folder)) return null;
      scan = result;
      scans = [result, ...scans.filter(item => item.id !== id)].sort((a, b) => b.startedAt - a.startedAt).slice(0, 20);
      return result;
    } catch (failure) { if (generation === scanReadGeneration && folderGeneration.accepts(folderToken, folder)) error = String(failure); return null; }
  }
  async function startScan(recipeHash: string, approved: boolean, preparedProject: QualityProject | null = draftProject): Promise<void> {
    if (!preparedProject) return;
    const candidate = structuredClone($state.snapshot(preparedProject));
    const folderToken = currentFolderGeneration;
    error = ""; view = "scan";
    try {
      const result = await service.mutate(() => api.startCodeQualityScan(candidate, recipeHash, approved));
      if (!folderGeneration.accepts(folderToken, folder)) return;
      selectedId = result.scanId; scan = null;
      const read = await readScan(result.scanId);
      if (read && !isActiveScan(read)) view = "report";
    } catch (failure) { if (folderGeneration.accepts(folderToken, folder)) error = String(failure); }
  }
  async function scanOrCancel(): Promise<void> {
    if (action.kind === "cancel") await cancel();
    else if (action.kind === "scan" && preparation) await startScan(preparation.recipeHash, preparation.requiresConsent, preparation.project);
  }
  async function startAndScan(install = false): Promise<void> {
    if (!projectReady || !draftProject || (install && !setupConsent)) return;
    const candidate = structuredClone($state.snapshot(draftProject)), folderToken = currentFolderGeneration;
    error = ""; startingScan = true; recipeApproval = false;
    try {
      await service.mutate(current => install ? api.setupCodeQuality(current.revision, setupConsent) : api.startCodeQualityService(current.revision));
      // Start may return while its worker is still bootstrapping the server/token.
      while (service.snapshot.state?.busy) {
        await new Promise(resolve => window.setTimeout(resolve, 500));
        if (!folderGeneration.accepts(folderToken, folder)) return;
        await service.refresh();
      }
      const current = service.snapshot.state;
      if (!current || current.ownership.status !== "owned" || current.effectiveMode !== current.config.mode || current.error || current.suspended) return;
      const fresh = await api.prepareCodeQualityScan(candidate);
      if (!folderGeneration.accepts(folderToken, folder)) return;
      preparation = fresh;
      // Installation consent is never build execution consent.
      if (fresh.requiresConsent) { recipeApproval = true; view = "scan"; return; }
      await startScan(fresh.recipeHash, false, fresh.project);
    } catch (failure) { if (folderGeneration.accepts(folderToken, folder)) error = String(failure); }
    finally { startingScan = false; }
  }
  async function shutdownServer(): Promise<void> {
    error = "";
    try { await service.mutate(() => api.shutdownCodeQualityServer()); } catch (failure) { error = String(failure); }
  }
  async function enterAgain(): Promise<void> {
    error = "";
    try { await service.mutate(current => api.startCodeQualityService(current.revision)); } catch (failure) { error = String(failure); }
  }
  async function cancel(): Promise<void> {
    const id = activeId; if (!id) return;
    error = "";
    try { await service.mutate(() => api.cancelCodeQualityScan(id), true); if (selectedId === id) await readScan(id); }
    catch (failure) { error = String(failure); }
  }
  function chooseRun(id: string): void {
    selectedId = id; scan = null;
    if (id) { view = "report"; void readScan(id); }
  }
  const viewEntry = new QualityViewEntry();
  $effect(() => {
    const current = quality, blocked = locked;
    if (!entered) return;
    untrack(() => {
      if (viewEntry.shouldEnter(current, blocked)) {
        void service.mutate(() => api.enterCodeQuality()).catch(() => { /* Authoritative state carries the failure. */ });
      }
    });
  });
  onMount(() => {
    let alive = true;
    // The entry effect admits initial entry and later ownership acquisition, once each.
    void (async () => {
      await service.refresh();
      if (alive) entered = true;
    })();
    const timer = window.setInterval(() => {
      if (pollReading || !selectedId || (scan && !isActiveScan(scan) && selectedId !== quality?.activeScanId)) return;
      pollReading = true;
      void readScan(selectedId).finally(() => { pollReading = false; });
    }, 1000);
    return () => { alive = false; window.clearInterval(timer); folderGeneration.invalidate(); prepareGeneration++; scanReadGeneration++; };
  });
  // Service subscriptions detach with this component; containers and server jobs do not.
</script>

{#snippet folderPicker()}
  <div class="quality-actions quality-wide"><label class="quality-grow">Project folder<select bind:value={folder}><option value="">Choose a project folder</option>{#each folderOptions as option}<option value={option}>{option}</option>{/each}</select></label><button onclick={browse}>Browse…</button></div>
  {#if loading}<p role="status">Detecting project and reading saved runs…</p>{/if}
  {#each detection?.warnings ?? [] as warning}<p class="warn">{warning}</p>{/each}
  {#if detection && project}<p class="quality-wide">Detected: {detection.labels.join(" · ") || "unknown project type"}. Scanner: {scannerNote}</p>{/if}
{/snippet}

{#snippet recipe()}
  {#if preparation}
    <details open={preparation.requiresConsent} class="quality-approval quality-wide"><summary><strong>{preparation.requiresConsent ? "Review the exact redacted build recipe" : "Scan recipe"}</strong></summary><pre class="quality-log">{preparation.recipe.join("\n")}</pre><small class="mono">Recipe: {preparation.recipeHash}<br />Image: {preparation.project.image ?? "managed pinned image"}</small>
      {#each preparation.warnings as warning}<p class="warn">{warning}</p>{/each}
      <p>The original working tree is not mounted or modified. Dirty/untracked sources are copied subject to ignores and displayed exclusions. Existing lint configs run only inside the isolated scanner and may execute code there. Reports are plain text; no automatic fixes.</p>
      {#if preparation.requiresConsent}<p class="warn">Build tools run inside the isolated snapshot with dependency downloads; Sonar build tools receive this connection's analysis token, not unrelated credentials. Tests are skipped by the default build recipes. Pressing the scan button approves this recipe.</p>{/if}
    </details>
  {/if}
{/snippet}

{#snippet analysisOptions()}
  {#if project && quality}
    <details>
      <summary>Advanced analysis options</summary>
      <fieldset disabled={locked || loading}>
        <legend>Project analysis options</legend>
        <div class="quality-grid">
          <label>Sonar project key<input bind:value={project.projectKey} disabled={quality.config.mode === "alternate"} /></label>
          <label>Scanner<select value={selectionNeeded ? "" : project.scanner} disabled={quality.config.mode === "alternate"} onchange={e => { if (project) project.scanner = e.currentTarget.value as QualityProject["scanner"]; selectionConfirmed = true; }}><option value="" disabled>Select build family</option><option value="cli">Node / JavaScript / TypeScript / Python / Go / other — no build (not C# / VB)</option><option value="maven">Maven</option><option value="gradle">Gradle</option><option value="dotnet">.NET</option></select></label>
          <label>Build entry file<input list="quality-entry-files" value={project.entryFile ?? ""} oninput={e => { if (project) project.entryFile = e.currentTarget.value || null; }} placeholder="Select or enter a relative entry file" /><datalist id="quality-entry-files">{#each detection?.entryFiles ?? [] as entry}<option value={entry}></option>{/each}</datalist></label>
          <label>Approved custom analysis image (optional)<input value={project.image ?? ""} oninput={e => { if (project) project.image = e.currentTarget.value || null; }} placeholder="Pinned compatible image" /></label>
          <label>Build arguments · one argv item per line<textarea rows="3" bind:value={buildArgs}></textarea></label>
          <label>Additional relative exclusion globs · one per line<textarea rows="3" bind:value={exclusions}></textarea></label>
        </div>
        <label class="quality-check"><input type="checkbox" bind:checked={project.prepareNpmDependencies} />Prepare npm dependencies in the disposable snapshot (package-lock.json: npm ci --ignore-scripts). Other package managers require an approved compatible custom image.</label>
      </fieldset>
    </details>
  {/if}
{/snippet}

<div class="quality-page">
  <header class="panel quality-actions"><button onclick={onBack}>{backLabel}</button><h2>Code quality</h2><span class="spacer"></span><span class="dim">No agent prompts · analysis runs in isolated snapshots</span></header>
  <div class="quality-content">
    <details class="panel quality-section"><summary>Advanced settings · {modeLabel(quality?.config.mode ?? null)} · {quality?.phase ?? "loading"}</summary><CodeQualitySettings enterOnAcquire={false} /></details>
    <div class="panel quality-section quality-actions">
      <CodeQualityCredentials />
      {#if quality && (quality.community.running || quality.setup.steps.some(step => step.id === "machine" && step.state === "done"))}
        <button disabled={locked || startingScan} onclick={shutdownServer}>{quality.config.runtime.kind === "podman" ? "Shutdown the Podman server" : "Shutdown code quality server"}</button>
        {#if activeId}<span class="dim">Cancel scan first.</span>{/if}
      {/if}
    </div>
    {#if screen === "loading"}
      <section class="panel quality-section" aria-label="Loading"><p role="status">Reading code quality state…</p></section>
    {:else if screen === "ownership"}
      <section class="panel quality-section"><CodeQualityOwnership enterOnAcquire={false} />{@render folderPicker()}{#if scan && !isActiveScan(scan)}<CodeQualityReport {scan} onScanAgain={() => { view = "scan"; }} bind:tool bind:severity bind:text />{/if}</section>
    {:else if screen === "setup" && quality}
      <section class="panel quality-section quality-center" aria-label="Set up code quality">
        <h3>{setup.install ? "Set up code quality" : "Start and scan"}</h3>
        <p class="dim quality-wide">{setup.install ? "Approve installation of the missing prerequisites. Build execution approval is separate." : "Prerequisites are installed. Start the analyzer and scan your selected project without running the installer."}</p>
        {@render folderPicker()}
        {#if quality.setup.steps.length}
          <ul class="quality-steps quality-wide" aria-label="Setup steps">{#each quality.setup.steps as step (step.id)}<li class={step.state === "done" ? "ok" : step.state === "blocked" ? "warn" : ""} title={step.state}><span aria-hidden="true">{step.state === "done" ? "✓" : step.state === "blocked" ? "⏸" : "•"}</span> {step.label}</li>{/each}</ul>
        {/if}
        {#if quality.busy}<pre class="quality-log quality-wide" aria-live="polite">{quality.diagnostics.slice(-8).join("\n")}</pre>{/if}
        {#if quality.phase === "needsRestart"}<p class="warn quality-wide" role="status">Restart Windows, reopen OMP Manager and press Continue setup.</p>{/if}
        {#if quality.error}<p class="err quality-wide" role="alert">{quality.error}</p>{/if}
        {@render analysisOptions()}
        {#if selectionNeeded || entryNeeded}<p class="warn">Choose the build family and required entrypoint under Advanced analysis options.</p>{/if}
        {#if setup.install}
          <label class="quality-check quality-wide"><input type="checkbox" bind:checked={setupConsent} disabled={locked} />I approve enabling WSL features (a Windows restart may be needed), installing Podman with winget, creating the app's WSL machine, raising VM limits, downloading SonarQube and PostgreSQL images, and saving a random SonarQube admin password and scan token in Windows Credential Manager. Windows will show administrator prompts. This does not approve running a build recipe.</label>
        {/if}
        <button class="primary" disabled={!setup.ready || !projectReady || (setup.install && !setupConsent) || locked || startingScan} onclick={() => startAndScan(setup.install)}>{setup.label}</button>
        {@render recipe()}
      </section>
    {:else if screen === "starting" && quality}
      <section class="panel quality-section quality-center" aria-label="Starting code quality">
        <progress aria-label="Starting code quality"></progress>
        <h3>Starting code quality…</h3>
        <p role="status">{STARTING_TEXT[quality.phase] ?? "Working…"}</p>
        {#if quality.diagnostics.length}<pre class="quality-log quality-wide" aria-live="polite">{quality.diagnostics.slice(-6).join("\n")}</pre>{/if}
      </section>
    {:else if screen === "failed" && quality}
      <section class="panel quality-section quality-center" aria-label="Code quality failed">
        <h3>Code quality could not start</h3>
        <p class="err quality-wide" role="alert">{quality.error ?? "The analyzer did not start."}</p>
        <button class="primary" disabled={locked} onclick={enterAgain}>Try again</button>
        <p class="dim">Open Advanced settings above for manual setup.</p>
      </section>
    {:else if screen === "scan" && quality}
      <section class="panel quality-section" aria-label="Scan project">
        <div class="quality-actions"><label class="quality-grow">Project folder<select bind:value={folder}><option value="">Choose a project folder</option>{#each folderOptions as option}<option value={option}>{option}</option>{/each}</select></label><button onclick={browse}>Browse…</button></div>
        {#if scans.length}<label>Latest saved runs (up to 20)<select value={selectedId} onchange={e => chooseRun(e.currentTarget.value)}><option value="">No run selected</option>{#each scans as item}<option value={item.id}>{new Date(item.startedAt).toLocaleString()} · {modeLabel(item.config.mode)} · {item.status}</option>{/each}</select></label>{/if}
        {#if loading}<p role="status">Detecting project and reading saved runs…</p>{/if}
        {#each detection?.warnings ?? [] as warning}<p class="warn">{warning}</p>{/each}
          {#if quality.suspended}<p role="status"><strong>Stopped</strong> · Saved reports remain available.</p><button class="primary" disabled={!projectReady || locked || startingScan} onclick={() => startAndScan()}>Start and scan</button>{/if}
        {#if view === "report" && scan && !isActiveScan(scan)}
          <CodeQualityReport {scan} onScanAgain={() => { view = "scan"; }} bind:tool bind:severity bind:text />
        {:else}
          <p>Committed: {modeLabel(quality.config.mode)} · {runtimeLabel(quality.config.runtime)}<br />Effective: {modeLabel(quality.effectiveMode)} · <strong>{quality.phase}</strong></p>
          {#if detection && project}<p>Detected: {detection.labels.join(" · ") || "unknown project type"}. Scanner: {scannerNote}</p>{/if}
          {#if quality.unresolved.length}<p class="warn">Unresolved owned resources or server tasks block new scans. Use Start / retry in Advanced settings to reconcile.</p>{/if}
          {#if serverOrigin}<p class="warn">Source analysis is sent to {serverOrigin} ({quality.config.mode === "sonarqubeEnterprise" ? "external Enterprise server" : "managed local Community server"}).</p>{/if}
          {#if credentialMissing}<p class="warn">A saved user token is required. Open Advanced settings to save/test it.</p>{/if}
          {#if !runtimeReady}<p class="warn">The selected scanner runtime is not ready. Review runtime prerequisites in Advanced settings.</p>{/if}
          {#if quality.config.mode === "alternate"}<p>Frozen MegaLinter + Opengrep + Semgrep CE profile · manifest <span class="mono">{quality.rules.manifestHash}</span>. {quality.rules.acknowledged ? "Rules license acknowledged." : "Rules license acknowledgement required in Advanced settings."} Analysis containers have no network; findings never imply coverage for skipped languages.</p>{/if}
          {@render analysisOptions()}
          {#if selectionNeeded || entryNeeded}<p class="warn">Choose the build family and required entrypoint under Advanced analysis options. Mixed build families are never silently analyzed with CLI.</p>{/if}
          {#if preparation?.requiresConsent}{@render recipe()}{/if}
          {#if recipeApproval}<p class="warn">The server is ready. Review the refreshed build recipe, then approve it with the scan button below.</p>{/if}
          <div class="quality-actions">
            <button class={action.kind === "cancel" ? "danger" : "primary"} disabled={actionDisabled} onclick={scanOrCancel}>{action.label}</button>
            {#if action.reason}<span class="dim" role="status">{action.reason}</span>{/if}
            {#if activeId}<span class="warn">Active scan {activeId}{activeId !== selectedId ? " · may belong to another folder; selections do not rename it" : ""}</span>{/if}
          </div>
          {#if scan && isActiveScan(scan)}<details open><summary>Staged progress · {scan.phase}</summary><pre class="quality-log" aria-live="polite">{scan.log.join("\n") || scan.phase}</pre></details>{/if}
          {#if !scan && selectedId}<p class="dim">Reading saved result…</p>{/if}
        {/if}
      </section>
    {/if}
    {#if error || $service.error}<p class="err" role="alert">{error || $service.error}</p>{/if}
  </div>
</div>
