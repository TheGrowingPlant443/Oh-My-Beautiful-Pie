<script lang="ts">
  import type { DepNode } from "../dependency-graph";
  import type { JobSnapshot, PackageUsageReport, ReviewedProvenance, UsageExplanationState } from "../package-insights";
  import DependencySourceEvidence from "./DependencySourceEvidence.svelte";
  let { node, usage, onclose, onrun, onopenarchitect }: {
    node: DepNode | null; usage: JobSnapshot<UsageExplanationState>;
    onclose: () => void; onrun: () => void; onopenarchitect: () => void;
  } = $props();
  let dialog = $state<HTMLDialogElement>();
  let programmaticCloses = 0;
  const report = $derived(usage.state?.report ?? null);
  const STATUS: Record<PackageUsageReport["status"], string> = {
    used: "Used by this project",
    notObserved: "No use observed",
    unknown: "Unknown"
  };
  const ROLE: Record<PackageUsageReport["projectRole"], string> = {
    runtime: "Runtime",
    build: "Build tooling",
    test: "Tests",
    configuration: "Configuration",
    transitiveFeature: "Transitive feature",
    unknown: "Unknown"
  };
  const reviewedNewest = (reviewed: ReviewedProvenance): string => reviewed.newestStatus === "available" && reviewed.newestVersion ? reviewed.newestVersion : `Unknown (${reviewed.newestStatus})`;
  const when = (value: number | null): string => value === null ? "Not recorded" : new Date(value).toLocaleString();
  $effect(() => {
    if (!dialog) return;
    if (node && !dialog.open) dialog.showModal();
    else if (!node && dialog.open) { programmaticCloses++; dialog.close(); }
  });
</script>
<dialog bind:this={dialog} aria-labelledby="dependency-usage-title" onclose={() => { if (programmaticCloses) programmaticCloses--; else onclose(); }} oncancel={event => { event.preventDefault(); onclose(); }}>
  <section>
    <header><h3 id="dependency-usage-title">How this package is used</h3><button onclick={onclose}>Close</button></header>
    <div class="body">
      {#if node}<strong>{node.name} · {node.ecosystem}</strong>{/if}
      {#if usage.state}
        {@const reviewed = usage.state.reviewed}
        <div class="block">
          <h4>Reviewed versions / reviewed registry release</h4>
          <p>Resolved versions: {reviewed.resolvedVersions.join(", ") || "Unknown"}<br />Declared versions: {reviewed.declaredVersions.join(", ") || "Unknown"}<br />Newest registry release: {reviewedNewest(reviewed)}<br />Project: {reviewed.folder}<br />Requested: {when(reviewed.requestedAt)}<br />Context ID: {reviewed.contextId ?? "Not recorded (legacy request)"}</p>
        </div>
      {/if}
      <p class="dim">Architect explanation from this project's source, configuration and dependency paths — not a removal recommendation.</p>
      {#if usage.error}<p class="err" role="alert">{usage.error}</p>{/if}
      {#if usage.state?.status === "error"}<p class="err" role="alert">{usage.state.error ?? "The explanation ended with an error."} You can request a new explanation.</p>{/if}
      {#if usage.admission?.error && usage.admission.error !== usage.error}<p class="err" role="alert">{usage.admission.error}</p>{/if}
      {#if usage.admission?.promptSent === null}<p role="status">Prompt delivery acknowledgement is uncertain. Waiting for a possible late explanation; the request will not be resent automatically.</p>{/if}
      {#if usage.admission?.promptSent === false && !usage.error}<p role="alert">The explanation prompt was not sent.</p>{/if}
      {#if usage.timedOut}<p role="status">The Architect has not submitted an explanation yet; it may still be running.</p>
      {:else if usage.pending}<p role="status">{usage.admission ? (usage.admission.disposition === "pending" ? "Joined the explanation already in progress. Waiting for The Architect…" : "Waiting for The Architect to submit a source-cited explanation…") : "Requesting an explanation from The Architect…"}</p>{/if}
      {#if report}
        <p class="notice" role="note">Saved analysis — source code and dependencies may have changed since this explanation.</p>
        <p><strong>{STATUS[report.status]}</strong> · Role: {ROLE[report.projectRole]}</p>
        <h4>Summary</h4><p>{report.summary}</p>
        <h4>What it does for this project</h4><p>{report.purpose}</p>
        {#if report.benefits.length}<h4>Benefits</h4><ul>{#each report.benefits as benefit}<li>{benefit}</li>{/each}</ul>{/if}
        {#if report.paths.length}
          <h4>Dependency paths</h4>
          {#each report.paths as path}
            <article>
              <strong>{path.packageIds.join(" → ")}</strong>
              <p>{path.explanation}</p>
              <DependencySourceEvidence evidence={path.evidence} />
            </article>
          {/each}
        {/if}
        <DependencySourceEvidence evidence={report.sourceEvidence} label="Source evidence" />
        {#if report.status !== "unknown" && !report.sourceEvidence.length && !report.paths.some(path => path.evidence.length)}<p class="dim">No local source evidence was recorded for this explanation.</p>{/if}
        <h4>Limitations</h4>
        {#if report.limitations.length}<ul>{#each report.limitations as limitation}<li>{limitation}</li>{/each}</ul>{:else}<p>No additional limitations were supplied.</p>{/if}
        <small class="dim">Completed: {new Date(report.completedAt).toLocaleString()}</small>
      {/if}
      <div class="actions">
        <button onclick={onrun} disabled={usage.pending} title="Replaces a finished explanation with a new analysis. One already in progress is joined, not duplicated.">Run new explanation</button>
        <button onclick={onopenarchitect}>Open Architect</button>
      </div>
      <p class="dim">Closing stops this popup's polling, not The Architect's shared conversation.</p>
    </div>
  </section>
</dialog>
<style>
  dialog { width: min(760px, 95vw); max-height: 85vh; box-sizing: border-box; border: 1px solid var(--line); padding: 0; border-radius: var(--radius); background: var(--panel); color: var(--text); }
  dialog::backdrop { background: rgba(4, 7, 12, .66); }
  section { display: flex; flex-direction: column; max-height: 85vh; }
  header { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 14px 16px; border-bottom: 1px solid var(--line); }
  h3, h4, p { margin: 0; }
  .body { overflow: auto; padding: 16px; display: flex; flex-direction: column; gap: 14px; white-space: pre-wrap; overflow-wrap: anywhere; }
  article { border: 1px solid var(--line); border-radius: 6px; padding: 12px; display: flex; flex-direction: column; gap: 8px; }
  ul { margin: 0; padding-left: 20px; }
  .block { display: flex; flex-direction: column; gap: 6px; }
  .notice { border: 1px solid var(--line); border-radius: 6px; padding: 8px 10px; }
  .actions { display: flex; flex-wrap: wrap; gap: 8px; }
</style>
