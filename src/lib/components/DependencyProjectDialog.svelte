<script lang="ts">
  import type { JobSnapshot, ProjectCoverage, ProjectReviewState } from "../package-insights";
  let { open, project, names, onclose, onrun, onupdateplan, onopenarchitect, onopenpackage }: {
    open: boolean; project: JobSnapshot<ProjectReviewState>; names: ReadonlyMap<string, string>;
    onclose: () => void; onrun: () => void; onupdateplan: () => void; onopenarchitect: () => void; onopenpackage: (packageId: string) => void;
  } = $props();
  let dialog = $state<HTMLDialogElement>();
  let programmaticCloses = 0;
  const coverage = $derived(project.state?.coverage ?? []);
  const counts = $derived({
    complete: coverage.filter(entry => entry.status === "complete").length,
    pending: coverage.filter(entry => entry.status === "pending").length,
    error: coverage.filter(entry => entry.status === "error").length,
    skipped: coverage.filter(entry => entry.status === "skippedInhouse").length
  });
  const finished = $derived(project.state?.status === "complete");
  const incomplete = $derived(finished && (counts.error > 0 || counts.skipped > 0));
  const STATUS: Record<ProjectCoverage["status"], string> = {
    pending: "Pending",
    complete: "Complete",
    error: "Error",
    skippedInhouse: "Skipped (in-house)"
  };
  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    else if (!open && dialog.open) { programmaticCloses++; dialog.close(); }
  });
</script>
<dialog bind:this={dialog} aria-labelledby="dependency-project-title" onclose={() => { if (programmaticCloses) programmaticCloses--; else onclose(); }} oncancel={event => { event.preventDefault(); onclose(); }}>
  <section>
    <header><h3 id="dependency-project-title">Full project security review</h3><button onclick={onclose}>Close</button></header>
    <div class="body">
      <p class="dim">Reviews every direct dependency, including hidden built-in packages; checks transitive dependencies where relevant. Not limited by the current graph filters.</p>
      {#if project.state}
        <div class="block">
          <p>Project: {project.state.folder}<br />Context ID: {project.state.contextId ?? "Not recorded"}<br />Direct packages: {coverage.length}</p>
          <p role="status">{counts.complete} complete · {counts.pending} pending · {counts.error} error · {counts.skipped} skipped (in-house)</p>
        </div>
      {/if}
      {#if project.error}<p class="err" role="alert">{project.error}</p>{/if}
      {#if project.state?.status === "error"}<p class="err" role="alert">{project.state.error ?? "The project review ended with an error."} You can start a new project review.</p>{/if}
      {#if project.admission?.error && project.admission.error !== project.error}<p class="err" role="alert">{project.admission.error}</p>{/if}
      {#if project.admission?.promptSent === null}<p role="status">Prompt delivery acknowledgement is uncertain. Waiting for possible late reports; the request will not be resent automatically.</p>{/if}
      {#if project.admission?.promptSent === false && !project.error}<p role="alert">The project review prompt was not sent.</p>{/if}
      {#if project.timedOut}<p role="status">The Architect has not finished every package yet; work may still be running.</p>
      {:else if project.pending}<p role="status">{project.admission ? (project.admission.disposition === "pending" ? "Joined the project review already in progress. Waiting for the package reviews to finish…" : "Waiting for The Architect to review each direct package…") : "Requesting the project review…"}</p>{/if}
      {#if finished}
        <p class="notice" role="note">Saved analysis — source code and dependencies may have changed since this review.</p>
        {#if incomplete}<p class="warn" role="alert">Not every direct package was reviewed. Errors and skipped packages below are not shown to be safe.</p>{/if}
      {/if}
      {#if project.state?.summary}<h4>Summary</h4><p>{project.state.summary}</p>{/if}
      {#if coverage.length}
        <h4>Direct packages</h4>
        <ul class="coverage">
          {#each coverage as entry (entry.packageId)}
            <li data-status={entry.status}>
              <div class="row"><strong>{names.get(entry.packageId) ?? entry.packageId}</strong><span>{STATUS[entry.status]}</span></div>
              <small class="dim">{entry.packageId}</small>
              {#if entry.reason}<p>{entry.reason}</p>{/if}
              {#if entry.requestId}<button onclick={() => onopenpackage(entry.packageId)}>{entry.status === "complete" ? "Open saved review" : entry.status === "error" ? "Open review (error)" : "Open review in progress"}</button>{/if}
            </li>
          {/each}
        </ul>
      {:else if finished}
        <p>This project has no direct dependencies; nothing was reviewed.</p>
      {/if}
      {#if project.state && project.state.limitations.length}
        <h4>Limitations</h4>
        <ul>{#each project.state.limitations as limitation}<li>{limitation}</li>{/each}</ul>
      {/if}
      {#if project.state?.completedAt}<small class="dim">Completed: {new Date(project.state.completedAt).toLocaleString()}</small>{/if}
      <div class="actions">
        <button onclick={onrun} disabled={project.pending} title="Replaces a finished project review with a new one. Package reviews already in progress are joined, not duplicated.">Run new project review</button>
        <button onclick={onupdateplan} disabled={!finished} title={finished ? "Draft an update plan from this saved project review for your approval." : "Available once the project review has completed."}>Create update plan</button>
        <button onclick={onopenarchitect}>Open Architect</button>
      </div>
      <p class="dim">Closing stops this popup's polling, not the review or The Architect's shared conversation.</p>
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
  ul { margin: 0; padding-left: 20px; }
  .block { display: flex; flex-direction: column; gap: 6px; }
  .notice, .warn { border: 1px solid var(--line); border-radius: 6px; padding: 8px 10px; }
  .coverage { list-style: none; padding: 0; display: flex; flex-direction: column; gap: 8px; }
  .coverage li { border: 1px solid var(--line); border-radius: 6px; padding: 10px 12px; display: flex; flex-direction: column; gap: 6px; align-items: flex-start; }
  .row { display: flex; justify-content: space-between; gap: 12px; align-self: stretch; }
  .actions { display: flex; flex-wrap: wrap; gap: 8px; }
</style>
