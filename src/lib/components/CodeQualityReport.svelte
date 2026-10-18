<script lang="ts">
  import { untrack } from "svelte";
  import { api } from "../api";
  import { basename, pickReportPath } from "../ui";
  import { safePublicUrl } from "../package-insights";
  import { countBySeverity, filterQualityFindings, findingTools, isActiveScan, mergeFindingAttribution, modeLabel, qualityPage, runtimeLabel, scanPresentation, type QualityFinding, type QualityReportPreview, type QualityScan } from "../code-quality";
  let { scan, onScanAgain, tool = $bindable(""), severity = $bindable(""), text = $bindable("") }: { scan: QualityScan; onScanAgain: () => void; tool?: string; severity?: string; text?: string } = $props();
  const LEVELS = ["critical", "high", "medium", "low", "info"] as const;
  let selectedFinding = $state.raw<QualityFinding | null>(null);
  let reportName = $state("");
  let report = $state.raw<QualityReportPreview | null>(null);
  let reportLoading = $state(false);
  let page = $state(0);
  let error = $state("");
  let savedPath = $state("");
  let saving = $state(false);
  let reportGeneration = 0;
  const merged = $derived(mergeFindingAttribution(scan.findings));
  const tools = $derived(findingTools(scan.findings));
  const findings = $derived(filterQualityFindings(merged, { tool, severity, text }));
  const displayed = $derived(qualityPage(findings, page));
  const presentation = $derived(scanPresentation(scan));
  const counts = $derived(countBySeverity(merged));
  const reports = $derived([...new Set(scan.tools.flatMap(outcome => outcome.reportNames))]);
  $effect(() => { tool; severity; text; scan.id; untrack(() => { page = 0; selectedFinding = null; reportGeneration++; report = null; reportName = ""; reportLoading = false; }); });
  $effect(() => { scan.id; untrack(() => { savedPath = ""; error = ""; }); });
  async function preview(name: string): Promise<void> {
    const id = scan.id, generation = ++reportGeneration; reportName = name; report = null;
    if (!name) { reportLoading = false; return; }
    reportLoading = true;
    try { const result = await api.readCodeQualityReport(id, name); if (generation === reportGeneration && scan.id === id) report = result; }
    catch (failure) { if (generation === reportGeneration) error = String(failure); }
    finally { if (generation === reportGeneration) reportLoading = false; }
  }
  async function openDashboard(): Promise<void> { error = ""; try { await api.openCodeQualityDashboard(scan.id); } catch (failure) { error = String(failure); } }
  async function saveHtml(): Promise<void> {
    const id = scan.id; error = ""; savedPath = ""; saving = true;
    try {
      const path = await pickReportPath(`${basename(scan.folder) || "project"}-code-quality.html`);
      if (!path) return;
      await api.saveCodeQualityReport(id, path);
      if (scan.id === id) savedPath = path;
    } catch (failure) { error = String(failure); }
    finally { saving = false; }
  }
</script>

<section class="quality-report" aria-label="Scan report">
  <div class="quality-actions"><strong class={presentation.tone} role="status">{presentation.label}</strong><span class="spacer"></span><button class="primary" onclick={onScanAgain}>Scan again</button><button disabled={saving || isActiveScan(scan)} onclick={saveHtml}>Save report as HTML</button>{#if scan.config.mode !== "alternate"}<button onclick={openDashboard}>Open SonarQube dashboard</button>{/if}</div>
  {#if savedPath}<p class="ok" role="status">Saved to {savedPath}</p>{/if}
  {#if error}<p class="err" role="alert">{error}</p>{/if}
  <div class="quality-counts" aria-label="Findings by severity">{#each LEVELS as level}<div class="quality-count" class:err={(level === "critical" || level === "high") && counts[level] > 0}><strong>{counts[level]}</strong><span>{level}</span></div>{/each}</div>
  <p class="mono quality-provenance">{scan.folder}<br />{modeLabel(scan.config.mode)} · {runtimeLabel(scan.config.runtime)}<br />Started {new Date(scan.startedAt).toLocaleString()}{scan.finishedAt ? ` · finished ${new Date(scan.finishedAt).toLocaleString()}` : ""}<br />Scanner: {scan.project.scanner} · Image: {scan.project.image ?? "managed pinned image (see tool versions)"}<br />Project key: {scan.project.projectKey}<br />Additional exclusions: {scan.project.exclusions.join(", ") || "none"}<br />Source: {scan.snapshotHash}<br />Rules: {scan.rulesHash}<br />Analysis: {scan.analysisId ?? "unavailable"}</p>
  <p>Quality gate: <strong class:err={scan.qualityGate === "failed"} class:ok={scan.qualityGate === "passed"}>{scan.qualityGate ?? "not applicable / unavailable"}</strong>. Coverage is unavailable unless the analysis supplied a coverage report.</p>
  <div class="quality-actions">{#each scan.tools as outcome}<span class="pill" class:err={outcome.status === "error"} class:warn={outcome.status === "partial" || outcome.status === "skipped"} class:ok={outcome.status === "complete"}>{outcome.tool} {outcome.version} · {outcome.status} · {outcome.findingCount} findings · {outcome.analyzedFiles ?? "unknown"} files</span>{/each}</div>
  {#each scan.tools.filter(outcome => outcome.error) as outcome}<p class="err">{outcome.tool}: {outcome.error}</p>{/each}
  {#if scan.error}<p class="err" role="alert">{scan.error}</p>{/if}
  {#each scan.warnings as warning}<p class="warn">{warning}</p>{/each}
  <details open={isActiveScan(scan)}><summary>Staged progress · {scan.phase}</summary><pre class="quality-log" aria-live="polite">{scan.log.join("\n") || scan.phase}</pre></details>
  <div class="quality-grid quality-filters"><label>Tool<select bind:value={tool}><option value="">All tools</option>{#each tools as name}<option value={name}>{name}</option>{/each}</select></label><label>Severity<select bind:value={severity}><option value="">All severities</option>{#each LEVELS as level}<option value={level}>{level}</option>{/each}</select></label><label>Filter findings<input type="search" bind:value={text} placeholder="Rule, file or message" /></label></div>
  <div class="quality-table-wrap"><table class="quality-table"><thead><tr><th>Severity</th><th>Tool</th><th>Rule</th><th>File:line</th><th>Message</th></tr></thead><tbody>{#each displayed.items as finding (finding.id)}<tr><td><span class="pill" class:err={finding.severity === "critical" || finding.severity === "high"}>{finding.severity}</span></td><td>{finding.tools.join(" + ")}</td><td class="mono">{finding.ruleId}</td><td class="mono">{finding.path ?? "No source location"}{finding.line ? `:${finding.line}` : ""}</td><td><button class="ghost quality-message" onclick={() => selectedFinding = finding}>{finding.hotspot ? "Review needed · " : ""}{finding.message}</button></td></tr>{/each}</tbody></table></div>
  {#if !displayed.total}<p class="dim">{scan.status === "complete" && !scan.findings.length ? "Completed with zero findings in the analyzed coverage. Skipped tools/languages are not a clean bill of health." : "No findings match these filters. Incomplete or failed analysis is not proof of clean code."}</p>{/if}
  <nav class="quality-actions" aria-label="Findings pages"><button disabled={displayed.page === 0} onclick={() => page = displayed.page - 1}>Previous</button><span>Page {displayed.page + 1} / {displayed.pages} · {displayed.total} findings · 100 per page</span><button disabled={displayed.page + 1 >= displayed.pages} onclick={() => page = displayed.page + 1}>Next</button></nav>
  {#if selectedFinding}<aside class="quality-approval" aria-label="Finding details"><div class="quality-actions"><strong>{selectedFinding.ruleId}</strong><span class="spacer"></span><button onclick={() => selectedFinding = null}>Close details</button></div><p>{selectedFinding.tools.join(" + ")} · {selectedFinding.severity} · {selectedFinding.category}</p><pre class="quality-log">{selectedFinding.message}</pre><p class="mono">{selectedFinding.path ?? "No source location"} · {selectedFinding.line ?? "?"}:{selectedFinding.column ?? "?"}–{selectedFinding.endLine ?? selectedFinding.line ?? "?"}</p>{#if selectedFinding.helpUrl && safePublicUrl(selectedFinding.helpUrl)}<a href={safePublicUrl(selectedFinding.helpUrl)!} target="_blank" rel="noopener noreferrer">Rule documentation</a>{/if}{#if selectedFinding.hotspot}<p class="warn">Security hotspot: review needed, not a confirmed vulnerability.</p>{/if}</aside>{/if}
  <label>Allowed reports<select value={reportName} onchange={e => void preview(e.currentTarget.value)}><option value="">Choose report preview</option>{#each reports as name}<option value={name}>{name}</option>{/each}</select></label>
  {#if reportLoading}<p role="status">Reading report…</p>{/if}{#if report}<pre class="quality-log">{report.text}</pre>{#if report.truncated}<p class="warn">Preview truncated at the backend's 2 MiB limit.</p>{/if}{/if}
</section>
