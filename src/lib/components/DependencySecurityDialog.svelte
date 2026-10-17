<script lang="ts">
  import type { DepNode } from "../dependency-graph";
  import { groupFindings, newestText, projectVersionText, safePublicUrl, type PackageInsights, type ProjectApplicability, type ReviewedProvenance, type ReviewSnapshot } from "../package-insights";
  import DependencySourceEvidence from "./DependencySourceEvidence.svelte";
  let { node, insights, metadataError, review, explainDisabled, onclose, onrun, onexplain, onupdateplan, onopenarchitect }: {
    node: DepNode | null; insights?: PackageInsights; metadataError?: string; review: ReviewSnapshot; explainDisabled: boolean;
    onclose: () => void; onrun: () => void; onexplain: () => void; onupdateplan: () => void; onopenarchitect: () => void;
  } = $props();
  let dialog = $state<HTMLDialogElement>();
  let programmaticCloses = 0;
  const groups = $derived(groupFindings(review.state?.report?.findings ?? []));
  const complete = $derived(review.state?.status === "complete" && review.state.report !== null);
  const APPLICABILITY: Record<ProjectApplicability["status"], string> = {
    relevant: "Relevant to this project",
    notRelevant: "Not relevant to this project",
    unknown: "Unknown — not shown to be safe"
  };
  const reviewedNewest = (reviewed: ReviewedProvenance): string => reviewed.newestStatus === "available" && reviewed.newestVersion ? reviewed.newestVersion : `Unknown (${reviewed.newestStatus})`;
  const when = (value: number | null): string => value === null ? "Not recorded" : new Date(value).toLocaleString();
  $effect(() => {
    if (!dialog) return;
    if (node && !dialog.open) dialog.showModal();
    else if (!node && dialog.open) { programmaticCloses++; dialog.close(); }
  });
</script>
<dialog bind:this={dialog} aria-labelledby="dependency-review-title" onclose={() => { if (programmaticCloses) programmaticCloses--; else onclose(); }} oncancel={event => { event.preventDefault(); onclose(); }}>
  <section>
    <header><h3 id="dependency-review-title">Dependency security review</h3><button onclick={onclose}>Close</button></header>
    <div class="body">
      {#if node}
        <strong>{node.name} · {node.ecosystem}</strong>
        <div class="provenance">
          <h4>Current scan / current registry cache</h4>
          <p>{projectVersionText(node)}</p>
          <p>Resolved evidence: {node.resolvedVersions.join(", ") || "Unknown"}<br />Declared evidence: {node.declaredVersions.join(", ") || "Unknown"}<br />Newest (registry): {newestText(insights, metadataError)}</p>
          {#if metadataError}<p class="dim">Metadata lookup failed: {metadataError}. The Architect may still inspect source evidence.</p>{/if}
        </div>
      {/if}
      {#if review.state}
        {@const reviewed = review.state.reviewed}
        <div class="provenance">
          <h4>Reviewed versions / reviewed registry release</h4>
          <p>Resolved versions: {reviewed.resolvedVersions.join(", ") || "Unknown"}<br />Declared versions: {reviewed.declaredVersions.join(", ") || "Unknown"}<br />Newest registry release: {reviewedNewest(reviewed)}<br />Project: {reviewed.folder}<br />Requested: {when(reviewed.requestedAt)}<br />Context ID: {reviewed.contextId ?? "Not recorded (legacy review)"}</p>
        </div>
      {/if}
      <p class="dim">Architect review — sourced findings, not a complete security audit.</p>
      {#if review.error}<p class="err" role="alert">{review.error}</p>{/if}
      {#if review.state?.status === "error"}<p class="err" role="alert">{review.state.error ?? "The review ended with an error."} You can start a new review.</p>{/if}
      {#if review.admission?.error && review.admission.error !== review.error}<p class="err" role="alert">{review.admission.error}</p>{/if}
      {#if review.admission?.promptSent === null}<p role="status">Prompt delivery acknowledgement is uncertain. Waiting for a possible late report; the request will not be resent automatically.</p>{/if}
      {#if review.admission?.promptSent === false && !review.error}<p role="alert">The review prompt was not sent.</p>{/if}
      {#if review.timedOut}<p role="status">The Architect has not submitted a report yet; it may still be running.</p>
      {:else if review.pending}<p role="status">{review.admission ? (review.admission.disposition === "pending" ? "Joined the review already in progress. Waiting for The Architect to submit a sourced report…" : "Waiting for The Architect to submit a sourced report…") : "Requesting review from The Architect…"}</p>{/if}
      {#if review.state?.report}
        {#if complete}<p class="notice" role="note">Saved analysis — source code and dependencies may have changed since this review.</p>{/if}
        <h4>Summary</h4><p>{review.state.report.summary}</p>
        {#if !review.state.report.findings.length}<p>No verified findings were submitted. This is not a clean bill of health.</p>{/if}
        {#each groups as [major, findings]}
          <h4>Major version: {major}</h4>
          {#each findings as finding}
            {@const applicability = finding.applicability}
            <article>
              <strong>{finding.title}</strong><p>Severity: {finding.severity}<br />Affected versions: {finding.affectedVersions}<br />First fixed release: {finding.fixedVersion ?? "Unknown"}<br />Advisory: {finding.advisoryId ?? "Not specified"}</p>
              <ul>{#each finding.evidence as evidence}<li>{evidence.title}<br />{#if safePublicUrl(evidence.url)}<a href={safePublicUrl(evidence.url)!} target="_blank" rel="noopener noreferrer">{evidence.url}</a>{:else}<span>{evidence.url} (unsupported link)</span>{/if}</li>{/each}</ul>
              <div class="applicability" data-status={applicability.status}>
                <strong>Project applicability: {APPLICABILITY[applicability.status]}</strong>
                <p>{applicability.reason || "No reasoning was supplied."}</p>
                {#if applicability.conditions.length}<div><strong>Conditions</strong><ul>{#each applicability.conditions as condition}<li>{condition}</li>{/each}</ul></div>{/if}
                <DependencySourceEvidence evidence={applicability.evidence} />
                {#if applicability.status !== "unknown" && !applicability.evidence.length}<p class="dim">No local source evidence was recorded for this claim.</p>{/if}
              </div>
            </article>
          {/each}
        {/each}
        <h4>Limitations</h4>
        {#if review.state.report.limitations.length}<ul>{#each review.state.report.limitations as limitation}<li>{limitation}</li>{/each}</ul>{:else}<p>No additional limitations were supplied. This report is not an exhaustive audit.</p>{/if}
        <small class="dim">Completed: {new Date(review.state.report.completedAt).toLocaleString()}</small>
      {/if}
      <div class="actions">
        <button onclick={onrun} disabled={review.pending} title="Replaces a finished review with a new analysis. A review already in progress is joined, not duplicated.">Run new review</button>
        <button onclick={onexplain} disabled={explainDisabled} title={explainDisabled ? "Usage explanation is unavailable for this package." : "Ask The Architect how this project uses the package."}>Explain how this package is used</button>
        <button onclick={onupdateplan} disabled={!complete} title={complete ? "Draft an update plan from this saved review for your approval." : "Available once a review has completed."}>Create update plan</button>
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
  article p { margin-top: 0; }
  article > p { margin-top: 8px; }
  ul { margin: 0; padding-left: 20px; }
  .provenance, .applicability { display: flex; flex-direction: column; gap: 6px; }
  .applicability { border-top: 1px solid var(--line); padding-top: 8px; }
  .notice { border: 1px solid var(--line); border-radius: 6px; padding: 8px 10px; }
  .actions { display: flex; flex-wrap: wrap; gap: 8px; }
</style>
