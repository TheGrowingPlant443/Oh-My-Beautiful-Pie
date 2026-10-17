<script lang="ts">
  import type { HandoffPrepared, JobSnapshot, UpdatePlanState } from "../package-insights";
  let { plan, prepared, handoffError, busy, onclose, onregenerate, onprepare, onconfirm, oncancelprepare }: {
    plan: JobSnapshot<UpdatePlanState> | null; prepared: HandoffPrepared | null; handoffError: string | null; busy: boolean;
    onclose: () => void; onregenerate: () => void; onprepare: () => void; onconfirm: () => void; oncancelprepare: () => void;
  } = $props();
  let planDialog = $state<HTMLDialogElement>();
  let confirmDialog = $state<HTMLDialogElement>();
  let planProgrammatic = 0;
  let confirmProgrammatic = 0;
  const proposal = $derived(plan?.state?.proposal ?? null);
  const handoff = $derived(plan?.state?.handoff ?? null);
  const handoverBlocked = $derived(!proposal || busy || !!plan?.pending || handoff?.status === "accepted" || handoff?.status === "attempting" || handoff?.status === "uncertain");
  const blockedReason = $derived(
    !proposal ? "Available once the plan has been drafted."
    : handoff?.status === "accepted" ? "This plan revision has already been handed over."
    : handoff?.status === "attempting" ? "A handoff of this plan revision is in progress."
    : handoff?.status === "uncertain" ? "A handoff of this plan revision may have been delivered; it will not be resent automatically."
    : "Review the plan, then confirm the handoff in the next step."
  );
  const when = (value: number): string => new Date(value).toLocaleString();
  $effect(() => {
    if (!planDialog) return;
    if (plan && !planDialog.open) planDialog.showModal();
    else if (!plan && planDialog.open) { planProgrammatic++; planDialog.close(); }
  });
  $effect(() => {
    if (!confirmDialog) return;
    if (prepared && !confirmDialog.open) confirmDialog.showModal();
    else if (!prepared && confirmDialog.open) { confirmProgrammatic++; confirmDialog.close(); }
  });
</script>
<dialog bind:this={planDialog} aria-labelledby="dependency-plan-title" onclose={() => { if (planProgrammatic) planProgrammatic--; else onclose(); }} oncancel={event => { event.preventDefault(); onclose(); }}>
  <section>
    <header><h3 id="dependency-plan-title">Dependency update plan</h3><button onclick={onclose}>Close</button></header>
    <div class="body">
      {#if plan}
        <p class="dim">A proposal drafted by The Architect for your review. Nothing has been installed, edited or sent to The Manager.</p>
        {#if plan.error}<p class="err" role="alert">{plan.error}</p>{/if}
        {#if plan.state?.status === "error"}<p class="err" role="alert">{plan.state.error ?? "The plan draft ended with an error."} You can generate a new draft.</p>{/if}
        {#if plan.admission?.error && plan.admission.error !== plan.error}<p class="err" role="alert">{plan.admission.error}</p>{/if}
        {#if plan.admission?.promptSent === null}<p role="status">Prompt delivery acknowledgement is uncertain. Waiting for a possible late plan; the request will not be resent automatically.</p>{/if}
        {#if plan.admission?.promptSent === false && !plan.error}<p role="alert">The plan prompt was not sent.</p>{/if}
        {#if plan.timedOut}<p role="status">The Architect has not submitted a plan yet; it may still be running.</p>
        {:else if plan.pending}<p role="status">{plan.admission?.disposition === "pending" ? "Joined the plan draft already in progress. Waiting for The Architect…" : "Waiting for The Architect to draft the update plan…"}</p>{/if}
        {#if plan.state}
          <div class="block">
            <p>Project: {plan.state.folder}<br />Planning context ID: {plan.state.source?.planningContextId ?? plan.state.contextId ?? "Not recorded"}{#if plan.state.source}<br />Source {plan.state.source.kind} review: {plan.state.source.reviewId} (completed {when(plan.state.source.completedAt)})<br />Review context ID: {plan.state.source.contextId ?? "Not recorded"}<br />Planning scan captured {when(plan.state.source.planningCapturedAt)} · checked {when(plan.state.source.planningCheckedAt)}{/if}</p>
            {#if plan.state.source?.historicalContextNote}<p class="warn" role="note">{plan.state.source.historicalContextNote}</p>{/if}
          </div>
        {/if}
        {#if proposal}
          <div class="block">
            <h4>{proposal.title}</h4>
            <p>{proposal.summary}</p>
            <small class="dim">Revision {proposal.revisionId} · {when(proposal.completedAt)}<br />Hash {proposal.hash}<br />Source review{proposal.sourceReviewIds.length === 1 ? "" : "s"}: {proposal.sourceReviewIds.join(", ") || "None recorded"}</small>
          </div>
          <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
          <div class="plan" role="region" tabindex="0" aria-label="Update plan text">{proposal.markdown}</div>
        {/if}
        {#if handoff}
          <p role="status">{handoff.status === "accepted" ? `Handed over to The Manager (revision ${handoff.revisionId}).`
            : handoff.status === "attempting" ? `Handoff to The Manager is in progress or was interrupted${handoff.error ? `: ${handoff.error}` : ""}. It will not be resent automatically.`
            : handoff.status === "uncertain" ? `Handoff outcome is uncertain${handoff.error ? `: ${handoff.error}` : ""}. It will not be resent automatically; check The Manager before deciding what to do.`
            : `The last handoff was not sent${handoff.error ? `: ${handoff.error}` : ""}. You can try again.`}</p>
        {/if}
        {#if handoffError}<p class="err" role="alert">{handoffError}</p>{/if}
        <div class="actions">
          <button onclick={onregenerate} disabled={busy || plan.pending} title="Drafts a new revision; a draft already in progress is joined, not duplicated.">Generate new draft</button>
          <button onclick={onprepare} disabled={handoverBlocked} title={blockedReason}>Hand over to The Manager</button>
        </div>
      {/if}
    </div>
  </section>
</dialog>
<dialog bind:this={confirmDialog} aria-labelledby="dependency-handoff-title" onclose={() => { if (confirmProgrammatic) confirmProgrammatic--; else if (!busy) oncancelprepare(); }} oncancel={event => { event.preventDefault(); if (!busy) oncancelprepare(); }}>
  <section>
    <header><h3 id="dependency-handoff-title">Hand over to The Manager?</h3></header>
    <div class="body">
      {#if prepared}
        <p>Project: {prepared.projectFolder}<br />Plan: {prepared.title}<br />Revision: {prepared.revisionId}<br />Destination: {prepared.managerName}</p>
        <p>{prepared.needsStart ? `${prepared.managerName} is stopped and will be started to receive the plan.` : `${prepared.managerName} is running.`}</p>
        <p class="warn" role="note">This interrupts any current Manager turn and supplies this approved plan as a new task. It does not stop workers or change native tool-approval settings.</p>
        <small class="dim">Confirmation valid until {when(prepared.expiresAt)}. Cancelling does nothing and does not start The Manager.</small>
        {#if handoffError}<p class="err" role="alert">{handoffError}</p>{/if}
        {#if busy}<p role="status">Handing over…</p>{/if}
        <div class="actions">
          <button onclick={oncancelprepare} disabled={busy}>Cancel</button>
          <button onclick={onconfirm} disabled={busy}>Hand over now</button>
        </div>
      {/if}
    </div>
  </section>
</dialog>
<style>
  dialog { width: min(820px, 95vw); max-height: 88vh; box-sizing: border-box; border: 1px solid var(--line); padding: 0; border-radius: var(--radius); background: var(--panel); color: var(--text); }
  dialog::backdrop { background: rgba(4, 7, 12, .66); }
  section { display: flex; flex-direction: column; max-height: 88vh; }
  header { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 14px 16px; border-bottom: 1px solid var(--line); }
  h3, h4, p { margin: 0; }
  .body { overflow: auto; padding: 16px; display: flex; flex-direction: column; gap: 14px; white-space: pre-wrap; overflow-wrap: anywhere; }
  .block { display: flex; flex-direction: column; gap: 6px; }
  .plan { border: 1px solid var(--line); border-radius: 6px; padding: 12px; max-height: 45vh; overflow: auto; white-space: pre-wrap; overflow-wrap: anywhere; font-family: var(--mono, ui-monospace, monospace); font-size: 12.5px; }
  .warn { border: 1px solid var(--line); border-radius: 6px; padding: 8px 10px; }
  .actions { display: flex; flex-wrap: wrap; gap: 8px; }
</style>
