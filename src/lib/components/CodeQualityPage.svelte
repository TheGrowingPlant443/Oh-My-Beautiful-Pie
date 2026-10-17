<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { api } from "../api";
  import { instances } from "../stores";
  import { pickFolder } from "../ui";
  import { projectFolders, repoKey } from "../repo-agents";
  import { safePublicUrl } from "../package-insights";
  import { codeQualityClient } from "../code-quality-client";
  import { filterQualityFindings, findingTools, isActiveScan, mergeFindingAttribution, modeLabel, parseQualityPreferences, qualityLocked, qualityPage, QualityGeneration, runtimeLabel, scanPresentation, type QualityFinding, type QualityProject, type QualityProjectDetection, type QualityReportPreview, type QualityScan, type QualityScanPreparation } from "../code-quality";
  import CodeQualitySettings from "./CodeQualitySettings.svelte";
  let { onBack }: { onBack: () => void } = $props();
  const PREFS_KEY = "omp-manager.code-quality";
  const saved = parseQualityPreferences(localStorage.getItem(PREFS_KEY));
  const service = codeQualityClient;
  const quality = $derived($service.state);
  const locked = $derived(qualityLocked($service));
  let folder = $state(saved.folder);
  let tool = $state(saved.tool), severity = $state(saved.severity), text = $state(saved.text);
  let project = $state<QualityProject | null>(null);
  let detection = $state.raw<QualityProjectDetection | null>(null);
  let selectionConfirmed = $state(false);
  let buildArgs = $state(""), exclusions = $state("");
  let preparation = $state.raw<QualityScanPreparation | null>(null);
  let consent = $state(false);
  let loading = $state(false), preparing = $state(false);
  let error = $state("");
  let scans = $state.raw<QualityScan[]>([]);
  let selectedId = $state("");
  let scan = $state.raw<QualityScan | null>(null);
  let selectedFinding = $state.raw<QualityFinding | null>(null);
  let reportName = $state("");
  let report = $state.raw<QualityReportPreview | null>(null);
  let reportLoading = $state(false);
  let page = $state(0);
  const folderGeneration = new QualityGeneration();
  let currentFolderGeneration = 0, prepareGeneration = 0, reportGeneration = 0, scanReadGeneration = 0;
  let pollReading = false;
  const folders = $derived(projectFolders($instances));
  const folderOptions = $derived(folder && !folders.some(value => repoKey(value) === repoKey(folder)) ? [folder, ...folders] : folders);
  const draftProject = $derived<QualityProject | null>(project ? { ...project, buildArgs: buildArgs.split("\n").map(value => value.trim()).filter(Boolean), exclusions: exclusions.split("\n").map(value => value.trim()).filter(Boolean) } : null);
  const preparationKey = $derived(JSON.stringify([draftProject, quality?.revision]));
  const selectionNeeded = $derived(quality?.config.mode !== "alternate" && !!detection?.needsSelection && !selectionConfirmed);
  const entryNeeded = $derived(quality?.config.mode !== "alternate" && project?.scanner === "dotnet" && !project.entryFile && (detection?.entryFiles.length ?? 0) > 1);
  const credentialMissing = $derived(quality?.config.mode === "sonarqubeCommunity" ? !quality.credentials.community : quality?.config.mode === "sonarqubeEnterprise" ? !quality.credentials.enterprise : false);
  const runtimeReady = $derived(quality?.prerequisites.some(item => item.runtime.kind === quality.config.runtime.kind && item.ready) ?? false);
  const runDisabled = $derived(locked || loading || preparing || !preparation || !draftProject || selectionNeeded || entryNeeded || credentialMissing || !runtimeReady || !quality?.effectiveMode || !!quality?.activeScanId || !!quality?.unresolved.length || (quality?.config.mode === "sonarqubeCommunity" && quality.community.running !== true) || (quality?.config.mode === "alternate" && !quality.rules.acknowledged) || (preparation.requiresConsent && !consent));
  const activeId = $derived(quality?.activeScanId ?? (isActiveScan(scan) ? scan?.id ?? null : null));
  const tools = $derived(findingTools(scan?.findings ?? []));
  const findings = $derived(filterQualityFindings(mergeFindingAttribution(scan?.findings ?? []), { tool, severity, text }));
  const displayed = $derived(qualityPage(findings, page));
  const presentation = $derived(scan ? scanPresentation(scan) : null);
  const reports = $derived([...new Set((scan?.tools ?? []).flatMap(outcome => outcome.reportNames))]);
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
    untrack(() => { prepareGeneration++; preparation = null; consent = false; });
  });
  $effect(() => {
    folder; tool; severity; text;
    try { localStorage.setItem(PREFS_KEY, JSON.stringify({ folder, tool, severity, text })); } catch { /* Preferences never own scans or configuration. */ }
  });
  $effect(() => { tool; severity; text; selectedId; untrack(() => { page = 0; selectedFinding = null; reportGeneration++; report = null; reportName = ""; reportLoading = false; }); });
  async function loadFolder(target: string): Promise<void> {
    const generation = folderGeneration.begin(target); currentFolderGeneration = generation; scanReadGeneration++;
    project = null; detection = null; scans = []; selectedId = ""; scan = null; selectionConfirmed = false; error = ""; loading = !!target;
    if (!target) return;
    try {
      const [detected, history] = await Promise.allSettled([api.detectCodeQualityProject(target), api.listCodeQualityScans(target)]);
      if (!folderGeneration.accepts(generation, folder)) return;
      if (detected.status === "fulfilled") {
        detection = detected.value; project = detected.value.project; buildArgs = project.buildArgs.join("\n"); exclusions = project.exclusions.join("\n");
      } else error = String(detected.reason);
      if (history.status === "fulfilled") {
        const canonical = project?.folder ?? target;
        scans = history.value.filter(item => repoKey(item.folder) === repoKey(canonical)).slice(0, 20);
        selectedId = scans[0]?.id ?? ""; scan = scans[0] ?? null;
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
    preparing = true; error = ""; preparation = null; consent = false;
    try {
      const result = await service.mutate(() => api.prepareCodeQualityScan(candidate));
      if (generation === prepareGeneration && folderGeneration.accepts(folderToken, folder) && repoKey(target) === repoKey(folder)) preparation = result;
    } catch (failure) { if (generation === prepareGeneration) error = String(failure); }
    finally { preparing = false; }
  }
  async function readScan(id: string): Promise<void> {
    const generation = ++scanReadGeneration, folderToken = currentFolderGeneration;
    try {
      const result = await api.readCodeQualityScan(id);
      if (!folderGeneration.accepts(folderToken, folder) || generation !== scanReadGeneration || selectedId !== id || repoKey(result.folder) !== repoKey(project?.folder ?? folder)) return;
      scan = result;
      scans = [result, ...scans.filter(item => item.id !== id)].sort((a, b) => b.startedAt - a.startedAt).slice(0, 20);
    } catch (failure) { if (generation === scanReadGeneration && folderGeneration.accepts(folderToken, folder)) error = String(failure); }
  }
  async function run(): Promise<void> {
    if (runDisabled || !preparation || !draftProject) return;
    const candidate = structuredClone($state.snapshot(draftProject)), recipeHash = preparation.recipeHash;
    const folderToken = currentFolderGeneration;
    error = "";
    try {
      const result = await service.mutate(() => api.startCodeQualityScan(candidate, recipeHash, consent));
      if (!folderGeneration.accepts(folderToken, folder)) return;
      selectedId = result.scanId; scan = null; await readScan(result.scanId);
    } catch (failure) { if (folderGeneration.accepts(folderToken, folder)) error = String(failure); }
  }
  async function cancel(): Promise<void> {
    const id = activeId; if (!id) return;
    error = "";
    try { await service.mutate(() => api.cancelCodeQualityScan(id), true); if (selectedId === id) await readScan(id); }
    catch (failure) { error = String(failure); }
  }
  async function preview(name: string): Promise<void> {
    const id = scan?.id, generation = ++reportGeneration; reportName = name; report = null;
    if (!id || !name) { reportLoading = false; return; }
    reportLoading = true;
    try { const result = await api.readCodeQualityReport(id, name); if (generation === reportGeneration && scan?.id === id) report = result; }
    catch (failure) { if (generation === reportGeneration) error = String(failure); }
    finally { if (generation === reportGeneration) reportLoading = false; }
  }
  async function openDashboard(): Promise<void> { try { await api.openCodeQualityDashboard(scan?.id ?? null); } catch (failure) { error = String(failure); } }
  onMount(() => {
    const timer = window.setInterval(() => {
      if (pollReading || !selectedId || (scan && !isActiveScan(scan) && selectedId !== quality?.activeScanId)) return;
      pollReading = true;
      void readScan(selectedId).finally(() => { pollReading = false; });
    }, 1000);
    return () => { window.clearInterval(timer); folderGeneration.invalidate(); prepareGeneration++; reportGeneration++; scanReadGeneration++; };
  });
  // Service subscriptions detach with this component; containers and server jobs do not.
</script>

<div class="quality-page">
  <header class="panel quality-actions"><button onclick={onBack}>Back to The Architect</button><h2>Code quality</h2><span class="spacer"></span><span class="dim">No agent prompts · analysis runs in isolated snapshots</span></header>
  <div class="quality-content">
    <details class="panel quality-section"><summary>Code quality settings · {modeLabel(quality?.config.mode ?? null)} · {quality?.phase ?? "loading"}</summary><CodeQualitySettings /></details>
    <section class="panel quality-section" aria-label="Scan project">
      <div class="quality-actions"><label class="quality-grow">Project folder<select bind:value={folder}><option value="">Choose a project folder</option>{#each folderOptions as option}<option value={option}>{option}</option>{/each}</select></label><button onclick={browse}>Browse…</button></div>
      {#if quality}<p>Committed: {modeLabel(quality.config.mode)} · {runtimeLabel(quality.config.runtime)}<br />Effective: {modeLabel(quality.effectiveMode)} · <strong>{quality.phase}</strong></p>{/if}
      {#if quality?.error}<p class="err" role="alert">{quality.error}</p>{/if}
      {#if quality?.phase === "needsRestart"}<p class="warn">Windows needs a manual restart before setup can continue. OMP Manager does not reboot Windows.</p>{/if}
      {#if quality?.unresolved.length}<p class="warn">Unresolved owned resources or server tasks block new scans. Use Start / retry in settings to reconcile.</p>{/if}
      {#if serverOrigin}<p class="warn">Source analysis is sent to {serverOrigin} ({quality?.config.mode === "sonarqubeEnterprise" ? "external Enterprise server" : "managed local Community server"}).</p>{/if}
      {#if credentialMissing}<p class="warn">A saved user token is required. Open settings to save/test it.</p>{/if}
      {#if quality && !runtimeReady}<p class="warn">The selected scanner runtime is not ready. Review runtime prerequisites in settings.</p>{/if}
      {#if quality?.config.mode === "alternate"}<p>Frozen MegaLinter + Opengrep + Semgrep CE profile · manifest <span class="mono">{quality.rules.manifestHash}</span>. {quality.rules.acknowledged ? "Rules license acknowledged." : "Rules license acknowledgement required in settings."} Analysis containers have no network; findings never imply coverage for skipped languages.</p>{/if}
      {#if loading}<p role="status">Detecting project and reading saved runs…</p>{/if}
      {#each detection?.warnings ?? [] as warning}<p class="warn">{warning}</p>{/each}
      {#if project}
        <fieldset disabled={locked || loading || preparing}>
          <legend>Project analysis options</legend>
          <div class="quality-grid">
            <label>Sonar project key<input bind:value={project.projectKey} disabled={quality?.config.mode === "alternate"} /></label>
            <label>Scanner<select value={selectionNeeded ? "" : project.scanner} disabled={quality?.config.mode === "alternate"} onchange={e => { if (project) project.scanner = e.currentTarget.value as QualityProject["scanner"]; selectionConfirmed = true; }}><option value="" disabled>Select build family</option><option value="cli">CLI · no build (not C# / VB)</option><option value="maven">Maven</option><option value="gradle">Gradle</option><option value="dotnet">.NET</option></select></label>
            <label>Build entry file<input list="quality-entry-files" value={project.entryFile ?? ""} oninput={e => { if (project) project.entryFile = e.currentTarget.value || null; }} placeholder="Select or enter a relative entry file" /><datalist id="quality-entry-files">{#each detection?.entryFiles ?? [] as entry}<option value={entry}></option>{/each}</datalist></label>
            <label>Approved custom analysis image (optional)<input value={project.image ?? ""} oninput={e => { if (project) project.image = e.currentTarget.value || null; }} placeholder="Pinned compatible image" /></label>
            <label>Build arguments · one argv item per line<textarea rows="3" bind:value={buildArgs}></textarea></label>
            <label>Additional relative exclusion globs · one per line<textarea rows="3" bind:value={exclusions}></textarea></label>
          </div>
          <label class="quality-check"><input type="checkbox" bind:checked={project.prepareNpmDependencies} />Prepare npm dependencies in the disposable snapshot (package-lock.json: npm ci --ignore-scripts). Other package managers require an approved compatible custom image.</label>
          {#if selectionNeeded || entryNeeded}<p class="warn">Choose the build family and required entrypoint before preparing. Mixed build families are never silently analyzed with CLI.</p>{/if}
          <button disabled={selectionNeeded || entryNeeded} onclick={prepare}>{preparing ? "Preparing…" : "Prepare / review scan recipe"}</button>
        </fieldset>
      {/if}
      {#if preparation}
        <section aria-label="Scan approval" class="quality-approval"><strong>Review exact redacted recipe</strong><pre class="quality-log">{preparation.recipe.join("\n")}</pre><small class="mono">Recipe: {preparation.recipeHash}<br />Image: {preparation.project.image ?? "managed pinned image"}</small>
          {#each preparation.warnings as warning}<p class="warn">{warning}</p>{/each}
          <p>The original working tree is not mounted or modified. Dirty/untracked sources are copied subject to ignores and displayed exclusions. Existing lint configs run only inside the isolated scanner and may execute code there. Reports are plain text; no automatic fixes.</p>
          {#if preparation.requiresConsent}<label class="quality-check"><input type="checkbox" bind:checked={consent} disabled={locked} />I approve this displayed recipe, dependency downloads and snapshot-only build/config execution. Sonar build tools receive this connection's analysis token, not unrelated credentials. Tests are skipped by the default build recipes.</label>{/if}
        </section>
      {/if}
      <div class="quality-actions">
        {#if activeId}<button class="danger" disabled={$service.pending || $service.uncertain || quality?.phase === "cancelling"} onclick={cancel}>Cancel scan</button><span class="warn">Active scan {activeId}{activeId !== selectedId ? " · may belong to another folder; selections do not rename it" : ""}</span>
        {:else}<button class="primary" disabled={runDisabled} onclick={run}>Run scan</button>{/if}
      </div>
      {#if error || $service.error}<p class="err" role="alert">{error || $service.error}</p>{/if}
    </section>
    <section class="panel quality-section" aria-label="Saved scan results">
      <label>Latest saved runs (up to 20)<select value={selectedId} onchange={e => { selectedId = e.currentTarget.value; scan = null; if (selectedId) void readScan(selectedId); }}><option value="">No run selected</option>{#each scans as item}<option value={item.id}>{new Date(item.startedAt).toLocaleString()} · {modeLabel(item.config.mode)} · {item.status}</option>{/each}</select></label>
      {#if scan && presentation}
        <div class="quality-actions"><strong class={presentation.tone} role="status">{presentation.label}</strong>{#if scan.config.mode !== "alternate"}<button onclick={openDashboard}>Open SonarQube dashboard</button>{/if}</div>
        <p class="mono quality-provenance">{scan.folder}<br />{modeLabel(scan.config.mode)} · {runtimeLabel(scan.config.runtime)}<br />Started {new Date(scan.startedAt).toLocaleString()}{scan.finishedAt ? ` · finished ${new Date(scan.finishedAt).toLocaleString()}` : ""}<br />Scanner: {scan.project.scanner} · Image: {scan.project.image ?? "managed pinned image (see tool versions)"}<br />Project key: {scan.project.projectKey}<br />Additional exclusions: {scan.project.exclusions.join(", ") || "none"}<br />Source: {scan.snapshotHash}<br />Rules: {scan.rulesHash}<br />Analysis: {scan.analysisId ?? "unavailable"}</p>
        <p>Quality gate: <strong class:err={scan.qualityGate === "failed"} class:ok={scan.qualityGate === "passed"}>{scan.qualityGate ?? "not applicable / unavailable"}</strong>. Coverage is unavailable unless the analysis supplied a coverage report.</p>
        <div class="quality-actions">{#each scan.tools as outcome}<span class="pill" class:err={outcome.status === "error"} class:warn={outcome.status === "partial" || outcome.status === "skipped"} class:ok={outcome.status === "complete"}>{outcome.tool} {outcome.version} · {outcome.status} · {outcome.findingCount} findings · {outcome.analyzedFiles ?? "unknown"} files</span>{/each}</div>
        {#each scan.tools.filter(outcome => outcome.error) as outcome}<p class="err">{outcome.tool}: {outcome.error}</p>{/each}
        {#if scan.error}<p class="err" role="alert">{scan.error}</p>{/if}
        {#each scan.warnings as warning}<p class="warn">{warning}</p>{/each}
        <details open={isActiveScan(scan)}><summary>Staged progress · {scan.phase}</summary><pre class="quality-log" aria-live="polite">{scan.log.join("\n") || scan.phase}</pre></details>
        <div class="quality-grid quality-filters"><label>Tool<select bind:value={tool}><option value="">All tools</option>{#each tools as name}<option value={name}>{name}</option>{/each}</select></label><label>Severity<select bind:value={severity}><option value="">All severities</option>{#each ["critical", "high", "medium", "low", "info"] as level}<option value={level}>{level}</option>{/each}</select></label><label>Filter findings<input type="search" bind:value={text} placeholder="Rule, file or message" /></label></div>
        <div class="quality-table-wrap"><table class="quality-table"><thead><tr><th>Severity</th><th>Tool</th><th>Rule</th><th>File:line</th><th>Message</th></tr></thead><tbody>{#each displayed.items as finding (finding.id)}<tr><td><span class="pill" class:err={finding.severity === "critical" || finding.severity === "high"}>{finding.severity}</span></td><td>{finding.tools.join(" + ")}</td><td class="mono">{finding.ruleId}</td><td class="mono">{finding.path ?? "No source location"}{finding.line ? `:${finding.line}` : ""}</td><td><button class="ghost quality-message" onclick={() => selectedFinding = finding}>{finding.hotspot ? "Review needed · " : ""}{finding.message}</button></td></tr>{/each}</tbody></table></div>
        {#if !displayed.total}<p class="dim">{scan.status === "complete" && !scan.findings.length ? "Completed with zero findings in the analyzed coverage. Skipped tools/languages are not a clean bill of health." : "No findings match these filters. Incomplete or failed analysis is not proof of clean code."}</p>{/if}
        <nav class="quality-actions" aria-label="Findings pages"><button disabled={displayed.page === 0} onclick={() => page = displayed.page - 1}>Previous</button><span>Page {displayed.page + 1} / {displayed.pages} · {displayed.total} findings · 100 per page</span><button disabled={displayed.page + 1 >= displayed.pages} onclick={() => page = displayed.page + 1}>Next</button></nav>
        {#if selectedFinding}<aside class="quality-approval" aria-label="Finding details"><div class="quality-actions"><strong>{selectedFinding.ruleId}</strong><span class="spacer"></span><button onclick={() => selectedFinding = null}>Close details</button></div><p>{selectedFinding.tools.join(" + ")} · {selectedFinding.severity} · {selectedFinding.category}</p><pre class="quality-log">{selectedFinding.message}</pre><p class="mono">{selectedFinding.path ?? "No source location"} · {selectedFinding.line ?? "?"}:{selectedFinding.column ?? "?"}–{selectedFinding.endLine ?? selectedFinding.line ?? "?"}</p>{#if selectedFinding.helpUrl && safePublicUrl(selectedFinding.helpUrl)}<a href={safePublicUrl(selectedFinding.helpUrl)!} target="_blank" rel="noopener noreferrer">Rule documentation</a>{/if}{#if selectedFinding.hotspot}<p class="warn">Security hotspot: review needed, not a confirmed vulnerability.</p>{/if}</aside>{/if}
        <label>Allowed reports<select value={reportName} onchange={e => void preview(e.currentTarget.value)}><option value="">Choose report preview</option>{#each reports as name}<option value={name}>{name}</option>{/each}</select></label>
        {#if reportLoading}<p role="status">Reading report…</p>{/if}{#if report}<pre class="quality-log">{report.text}</pre>{#if report.truncated}<p class="warn">Preview truncated at the backend's 2 MiB limit.</p>{/if}{/if}
      {:else}<p class="dim">{selectedId ? "Reading saved result…" : "Select a folder and a saved run, or prepare a new scan."}</p>{/if}
    </section>
  </div>
</div>
