<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { knowledge, knowledgeApi, type VaultBinding } from "../knowledge";
  import { srs, srsApi, activeSrs, resumableSrs, completedSrs, srsDocumentLabel, srsModelLabel, srsStateLabel, srsPhaseLabel, type ResumeMode, type ResumePreparation, type SrsAnswer, type SrsDocument, type SrsDocuments, type SrsJobPage, type SrsPage, type SrsQuestion, type SrsSection, type SrsWork } from "../srs";
  import VaultView from "./VaultView.svelte";
  import VaultConfiguration from "./VaultConfiguration.svelte";

  let { folder, initialJobId = null, onBack, onCreate, onOpenArchitect }: {
    folder: string; initialJobId?: string | null; onBack: () => void; onCreate: (folder: string) => void;
    onOpenArchitect: (id: string) => Promise<boolean>;
  } = $props();
  let selected = $state<string | null>(null);
  const job = $derived(selected ? $srs.jobs.get(selected) ?? null : null);
  let listing = $state<SrsJobPage | null>(null);
  let listLoading = $state(false);
  let connected = $state(false);
  let error = $state<string | null>(null);
  let busy = $state(false);
  let section = $state<SrsSection>("questions");
  let page = $state<SrsPage<unknown> | null>(null);
  let pageLoading = $state(false);
  let pageError = $state<string | null>(null);
  let listEpoch = 0, pageEpoch = 0;
  let entered = "";
  let documents = $state<SrsDocuments | null>(null);
  let resumePreview = $state<ResumePreparation | null>(null);
  let resumeMode = $state<ResumeMode>("sameConversation");
  let confirmCancel = $state(false);
  let disposition = $state<{ itemId: string; question: SrsQuestion | null; revision: number; kind: "provideHumanAnswer" | "excludeFromScope" } | null>(null);
  let answer = $state<SrsAnswer>({ selectedOptions: [] });
  let rationale = $state("");
  let locating = $state(false);
  let configureVault = $state(false);
  let vaults = $state<VaultBinding[]>([]);
  let candidateVault = $state("");

  onMount(() => {
    let mounted = true;
    void srs.connect().then(() => { if (mounted) connected = true; }, failure => { if (mounted) { error = `Live connection unavailable: ${String(failure)}. Check connection to reconnect; saved job state is retained.`; } });
    return () => { mounted = false; };
  });
  $effect(() => {
    if (!connected || entered === `${folder}|${initialJobId ?? ""}`) return;
    entered = `${folder}|${initialJobId ?? ""}`;
    selected = initialJobId; page = null; documents = null;
    void untrack(() => loadJobs(0));
    void knowledge.probe(folder);
  });
  $effect(() => { if (connected && selected) return srs.observe(selected); });
  $effect(() => {
    if (!connected || !selected) return;
    // A new selection/section resets pagination; events do not erase a human's answer draft.
    const id = selected, target = section;
    untrack(() => { page = null; disposition = null; resumePreview = null; confirmCancel = false; void loadPage(id, target, 0); });
  });
  $effect(() => {
    const id = selected, revision = job?.revision;
    if (!id || revision === undefined) return;
    untrack(() => {
      if (page && page.jobId === id && page.revision < revision && !disposition && !pageLoading) void loadPage(id, section, page.offset);
    });
  });
  async function loadJobs(offset = 0): Promise<void> {
    const mine = ++listEpoch, source = folder; listLoading = true;
    try {
      const result = await srs.list(source, offset);
      if (mine !== listEpoch || source !== folder) return;
      listing = result;
      if (!selected && result.items.length) selected = result.items[0].jobId;
    } catch (failure) { if (mine === listEpoch) error = String(failure); }
    finally { if (mine === listEpoch) listLoading = false; }
  }
  async function loadPage(id: string, target: SrsSection, offset: number): Promise<void> {
    const mine = ++pageEpoch; pageLoading = true; pageError = null;
    try {
      const result = await srsApi.query<unknown>(id, target, offset, 100);
      if (mine !== pageEpoch || id !== selected || target !== section) return;
      if (page && page.jobId === id && page.section === target && result.revision < page.revision) return;
      page = result;
    } catch (failure) { if (mine === pageEpoch) pageError = String(failure); }
    finally { if (mine === pageEpoch) pageLoading = false; }
  }
  async function act(run: () => Promise<unknown>): Promise<void> {
    busy = true; error = null;
    try { await run(); } catch (failure) { error = String(failure); } finally { busy = false; }
  }
  async function refresh(): Promise<void> {
    await act(async () => {
      await srs.connect(); connected = true;
      await srs.reconcile(); await loadJobs(listing?.offset ?? 0);
      if (selected) await loadPage(selected, section, page?.offset ?? 0);
    });
  }
  function select(id: string): void {
    srs.noteNavigation(); selected = id; documents = null; disposition = null;
  }
  async function showDocuments(): Promise<void> {
    if (!job) return;
    const id = job.jobId;
    await act(async () => { const result = await srsApi.openDocuments(id); if (selected === id) documents = result; });
  }
  function beginDisposition(id: string, kind: "provideHumanAnswer" | "excludeFromScope", question: SrsQuestion | null = null): void {
    if (!job || !page) return;
    disposition = { itemId: id, kind, question, revision: page.revision };
    rationale = ""; answer = { selectedOptions: [], customInput: "", note: "" };
  }
  async function submitDisposition(): Promise<void> {
    if (!job || !disposition || !rationale.trim()) return;
    const id = job.jobId, current = disposition;
    await act(async () => {
      await srs.dispose(id, current.revision, current.itemId, current.kind, rationale.trim(), current.kind === "provideHumanAnswer" ? answer : undefined);
      disposition = null;
      await loadPage(id, section, page?.offset ?? 0);
    });
  }
  async function prepareResume(): Promise<void> {
    if (!job) return;
    const id = job.jobId;
    await act(async () => { const result = await srsApi.prepareResume(id, resumeMode); if (selected === id) resumePreview = result; });
  }
  async function locate(): Promise<void> {
    await act(async () => { vaults = await knowledgeApi.listVaults(); candidateVault = ""; locating = true; });
  }
  async function rebind(): Promise<void> {
    if (!job || !candidateVault) return;
    await act(async () => { await srs.rebindVault(job!.jobId, candidateVault); locating = false; await knowledge.probe(folder); });
  }
  const questions = $derived(section === "questions" ? (page?.items ?? []) as SrsQuestion[] : []);
  const works = $derived(section === "work" ? (page?.items ?? []) as SrsWork[] : []);
  const notes = $derived(section === "documents" ? (page?.items ?? []) as SrsDocument[] : []);
  const sections: { id: SrsSection; label: string }[] = [ { id: "questions", label: "Questions" }, { id: "work", label: "Work ledger" }, { id: "documents", label: "Documents" }, { id: "coverage", label: "Coverage" }, { id: "stages", label: "Stages" }, { id: "activity", label: "Activity" }, { id: "receipts", label: "Human receipts" } ];
</script>

{#if documents && job}
  <VaultView vaultId={documents.vaultId} initialPath={documents.path} onBack={() => documents = null} backLabel="Back to SRS documentation" title={documents.draft ? "Draft SRS documents — not complete" : "SRS documentation"} refreshKey={job.revision} />
{:else}
  <section class="srs-page">
    <header class="bar panel"><button onclick={onBack}>Back to System overview</button><h2>SRS documentation</h2><span class="grow"></span><button disabled={busy} onclick={() => void refresh()}>Check connection / Refresh</button><button onclick={() => onCreate(folder)}>Create SRS</button></header>
    <p class="notice">Jobs continue while OMP Manager is open, including minimized. Navigating does not pause work. Closing checkpoints/pauses it; reopening never automatically starts a model.</p>
    {#if error}<p class="notice err" role="alert">{error}</p>{/if}
    {#if $srs.connectionError}<p class="notice warn" role="alert">Live observation failed: {$srs.connectionError}. This is not completion or cancellation. Check connection / Refresh to reconnect.</p>{/if}
    {#if $srs.actionError}<p class="notice warn" role="status">{$srs.actionError}</p>{/if}
    <div class="layout">
      <aside class="jobs panel" aria-label="SRS jobs">
        <h3>Recorded jobs</h3><p class="dim root">{folder}</p>
        {#if listLoading}<p role="status">Reading saved jobs…</p>{/if}
        {#each listing?.items ?? [] as saved (saved.jobId)}
          {@const current = $srs.jobs.get(saved.jobId) ?? saved}
          <button class="job" class:active={selected === current.jobId} onclick={() => select(current.jobId)}><strong>{new Date(current.createdAt).toLocaleString()}</strong><span>{srsStateLabel(current)}</span><small>{current.jobId}</small></button>
        {:else}{#if !listLoading}<p>No saved SRS jobs. Create SRS opens the explanation and scope confirmation; it does not start a model.</p>{/if}{/each}
        <div class="actions"><button disabled={!listing || listing.offset === 0 || listLoading} onclick={() => void loadJobs(Math.max(0, (listing?.offset ?? 0) - 100))}>Previous</button><button disabled={listing?.nextOffset == null || listLoading} onclick={() => void loadJobs(listing!.nextOffset!)}>Next</button></div>
        {#if listing}<p class="dim">{listing.total.toLocaleString()} jobs</p>{/if}
      </aside>
      <main class="detail panel">
        {#if job}
          <h3 role="status">{srsStateLabel(job)}</h3>
          {#if selected && $srs.observationErrors.has(selected)}<p class="notice warn" role="alert">Could not observe job: {$srs.observationErrors.get(selected)}. Last recorded state is shown; observation keeps retrying without a total deadline.</p>{/if}
          <p><strong>Phase: {srsPhaseLabel(job.phase)}</strong>{job.stage ? ` · Stage ${job.stage.number} (${job.stage.state})` : ""}</p>
          <p class="dim">{srsModelLabel(job.architectModel)}</p>
          <dl class="counts"><div><dt>Evidence files</dt><dd>{job.counts.evidenceFiles.toLocaleString()}</dd></div><div><dt>Areas</dt><dd>{job.counts.areas.toLocaleString()}</dd></div><div><dt>Work open / recorded</dt><dd>{job.counts.workOpen.toLocaleString()} / {job.counts.work.toLocaleString()}</dd></div><div><dt>Requirements</dt><dd>{job.counts.requirements.toLocaleString()}</dd></div><div><dt>Documents</dt><dd>{job.counts.documents.toLocaleString()}</dd></div><div><dt>Questions open / recorded</dt><dd>{job.counts.questionsOpen.toLocaleString()} / {job.counts.questions.toLocaleString()}</dd></div><div><dt>Blocked</dt><dd>{job.counts.blocked.toLocaleString()}</dd></div><div><dt>Excluded</dt><dd>{job.counts.excluded.toLocaleString()}</dd></div></dl>
          <p class="dim">Work is still being discovered; these are actual counts, not a completion percentage. Tokens: {job.usage.tokens === null ? "not reported" : job.usage.tokens.toLocaleString()} · Observed provider cost: {job.usage.costUsd === null ? "not reported (not zero)" : `$${job.usage.costUsd.toFixed(4)}`}.</p>
          {#if job.checkpoint}<p><strong>Last durable checkpoint</strong> · revision {job.checkpoint.revision} · {new Date(job.checkpoint.at).toLocaleString()}<br />{job.checkpoint.summary}</p>{:else}<p class="dim">No committed checkpoint yet.</p>{/if}
          {#if job.error}<p class="notice err" role="alert"><strong>{job.error.code}</strong>: {job.error.message}</p>{/if}
          {#if job.humanExclusions}<p class="notice warn"><strong>{job.humanExclusions} human scope exclusion(s).</strong> Completion applies only to the recorded scope, not excluded or undocumented production behavior. See Coverage and Human receipts.</p>{/if}
          <details><summary>Recorded identity and evidence scope</summary><dl class="identity"><dt>Job</dt><dd>{job.jobId} · revision {job.revision}</dd><dt>Repository</dt><dd>{job.repositoryRoot}</dd><dt>Scope hash</dt><dd>{job.scopeHash}</dd><dt>Source hash</dt><dd>{job.sourceHash ?? "Inventory not frozen yet"}</dd><dt>Template revision</dt><dd>{job.template.revision}</dd><dt>Attempt / stage</dt><dd>{job.attemptId ?? "—"} / {job.stageId ?? "—"}</dd><dt>Conversation</dt><dd>{job.producer?.sessionId ?? "Not admitted"}</dd>{#if completedSrs(job)}<dt>Final manifest hash</dt><dd>{job.completion?.manifestHash}</dd>{/if}</dl></details>
          <div class="actions">
            {#if activeSrs(job)}<button disabled={busy} onclick={() => void act(() => srs.pause(job!.jobId))}>Pause</button>{/if}
            {#if resumableSrs(job)}<label>Resume mode <select bind:value={resumeMode} onchange={() => resumePreview = null}><option value="sameConversation" disabled={job.resumeRequired === "currentConversationFromCheckpoints"}>Same Architect conversation</option><option value="currentConversationFromCheckpoints">Continue from saved checkpoints in the current Architect</option></select></label><button disabled={busy || (job.resumeRequired === "currentConversationFromCheckpoints" && resumeMode === "sameConversation")} onclick={() => void prepareResume()}>Review Resume</button>{/if}
            {#if job.state !== "cancelled" && !completedSrs(job)}<button disabled={busy} onclick={() => confirmCancel = true}>Cancel job…</button>{/if}
            {#if job.producer}<button onclick={() => void onOpenArchitect(job!.producer!.instanceId)}>Open Architect</button>{/if}
            <button disabled={busy || !job.vaultAvailable || job.counts.documents === 0} onclick={() => void showDocuments()}>{srsDocumentLabel(job)}</button>
            {#if !job.vaultAvailable}<button disabled={busy} onclick={() => void locate()}>Locate vault…</button>{/if}
          </div>
          {#if job.resumeRequired === "currentConversationFromCheckpoints"}<p class="notice warn">The original native conversation was replaced or deleted. Continuing in the current Architect is an explicit new-context continuation from durable checkpoints, not the same conversation.</p>{/if}
          {#if resumePreview}<div class="notice"><h4>Resume confirmation</h4><p>{resumePreview.summary}</p>{#each resumePreview.warnings as warning, i (i)}<p class="warn">{warning}</p>{/each}<div class="actions"><button class="primary" disabled={busy} onclick={() => void act(async () => { await srs.resume(resumePreview!.token); resumePreview = null; })}>Resume SRS</button><button disabled={busy} onclick={() => resumePreview = null}>Keep paused</button></div></div>{/if}
          {#if confirmCancel}<div class="notice warn"><p>Cancel this run? Committed documents and checkpoints remain recorded; cancellation does not mean the SRS is complete.</p><div class="actions"><button disabled={busy} onclick={() => void act(async () => { await srs.cancel(job!.jobId); confirmCancel = false; })}>Cancel SRS job</button><button disabled={busy} onclick={() => confirmCancel = false}>Keep job</button></div></div>{/if}
          {#if locating}<div class="notice"><h4>Locate the recorded documents</h4><p>Pick/register the folder containing this run's existing notes. The backend must verify the recorded committed hashes before rebinding. Unrelated notes are never adopted and this does not regenerate documents.</p><label>Registered vault <select bind:value={candidateVault}><option value="">Select…</option>{#each vaults as vault (vault.vaultId)}<option value={vault.vaultId}>{vault.root}</option>{/each}</select></label><div class="actions"><button onclick={() => configureVault = true}>Locate / register a folder…</button><button disabled={busy || !candidateVault} onclick={() => void rebind()}>Verify hashes and use this vault</button><button onclick={() => locating = false}>Cancel</button></div></div>{/if}
          <div class="tabs" role="tablist" aria-label="SRS job details">{#each sections as item (item.id)}<button role="tab" aria-selected={section === item.id} onclick={() => section = item.id}>{item.label}</button>{/each}</div>
          {#if pageError}<p class="err" role="alert">{pageError}</p>{/if}
          {#if pageLoading}<p role="status">Reading {section}…</p>{/if}
          {#if page && page.jobId === job.jobId && page.section === section}
            <p class="dim">{page.total.toLocaleString()} {section} records · ledger revision {page.revision}{page.revision < job.revision ? " (newer status available — Refresh)" : ""}</p>
            {#if section === "questions"}
              {#each questions as question (question.questionId)}<article class="record"><h4>{question.prompt}</h4><p class="dim">{question.questionId} · {question.state} · {question.required ? "required" : "optional"}{question.live ? " · live native Decisions question" : ""}</p>{#if question.answerReceiptId}<p class="dim">Authenticated human receipt: {question.answerReceiptId}</p>{/if}{#if question.state === "open" || question.state === "asked"}<div class="actions"><button disabled={busy} onclick={() => beginDisposition(question.questionId, "provideHumanAnswer", question)}>Provide human answer</button><button disabled={busy} onclick={() => beginDisposition(question.questionId, "excludeFromScope", question)}>Exclude from scope…</button></div>{/if}</article>{:else}<p>No questions recorded on this page. That alone is not proof of completion.</p>{/each}
            {:else if section === "work"}
              {#each works as work (work.id)}<article class="record"><h4>{work.kind} · {work.status}</h4><p>{work.rationale}</p><p class="dim">{work.id} · {srsPhaseLabel(work.phase)}</p>{#if work.status !== "excluded"}<button disabled={busy} onclick={() => beginDisposition(work.id, "excludeFromScope")}>Exclude from scope…</button>{/if}</article>{:else}<p>No work records on this page.</p>{/each}
            {:else if section === "documents"}
              {#each notes as note (note.documentId)}<article class="record"><h4>{note.title}</h4><p>{note.state} · {note.bytes.toLocaleString()} bytes{note.requirementId ? ` · ${note.requirementId}` : ""}</p><p class="dim">{note.path ?? "Not published yet"}{note.specificity ? ` · ${note.specificity}` : ""}</p></article>{:else}<p>No documents recorded on this page.</p>{/each}
            {:else}
              {#each page.items as record, i (`${page.revision}:${page.offset + i}`)}<pre class="record">{JSON.stringify(record, null, 2)}</pre>{:else}<p>No {section} records on this page.</p>{/each}
            {/if}
            <div class="actions"><button disabled={pageLoading || page.offset === 0} onclick={() => void loadPage(job!.jobId, section, Math.max(0, page!.offset - 100))}>Previous</button><button disabled={pageLoading || page.nextOffset === null} onclick={() => void loadPage(job!.jobId, section, page!.nextOffset!)}>Next</button><button disabled={pageLoading} onclick={() => void loadPage(job!.jobId, section, page!.offset)}>Refresh this ledger page</button></div>
          {/if}
          {#if disposition}<div class="notice answer"><h4>{disposition.kind === "provideHumanAnswer" ? "Provide human answer" : "Explicitly exclude from recorded scope"}</h4><p>{disposition.question?.prompt ?? disposition.itemId}</p>{#if disposition.kind === "provideHumanAnswer"}{#if disposition.question?.live}<p class="dim">This submits through the existing native Decisions responder. Its live Ask settles once, only after the authenticated answer receipt is saved.</p>{/if}{#each disposition.question?.options ?? [] as option (option.label)}<label class="choice"><input type={disposition.question?.multi ? "checkbox" : "radio"} name="srs-answer" checked={answer.selectedOptions.includes(option.label)} onchange={event => { answer = { ...answer, selectedOptions: disposition?.question?.multi ? event.currentTarget.checked ? [...answer.selectedOptions, option.label] : answer.selectedOptions.filter(item => item !== option.label) : [option.label] }; }} /> {option.label}{option.description ? ` — ${option.description}` : ""}</label>{/each}<label class="field">Your answer / additional detail<textarea bind:value={answer.customInput}></textarea></label><label class="field">Notes<textarea bind:value={answer.note}></textarea></label>{:else}<p class="warn">This reduces the scope and invalidates affected reviews/completion. The exclusion remains visible in history; it does not prove the behavior was verified.</p>{/if}<label class="field">Rationale (required)<textarea bind:value={rationale}></textarea></label><div class="actions"><button disabled={busy || !rationale.trim() || (disposition.kind === "provideHumanAnswer" && !answer.selectedOptions.length && !answer.customInput?.trim())} onclick={() => void submitDisposition()}>Confirm human disposition</button><button disabled={busy} onclick={() => disposition = null}>Cancel</button></div><p class="dim">Expected ledger revision {disposition.revision}; concurrent changes are rejected for review, never silently overwritten.</p></div>{/if}
        {:else}<p>{selected ? "Reading the selected job…" : "Select a job to read its durable status. Viewing saved jobs never starts or resumes a model."}</p>{/if}
      </main>
    </div>
  </section>
{/if}
<VaultConfiguration open={configureVault} scope={{ kind: "project" }} {folder} title="Locate existing SRS documents" offerObsidianInstall={false} onClose={() => configureVault = false} onBound={binding => { configureVault = false; candidateVault = binding.vaultId; vaults = [...vaults.filter(item => item.vaultId !== binding.vaultId), binding]; }} />

<style>
  .srs-page { height: 100%; min-height: 0; display: flex; flex-direction: column; gap: 10px; }
  .bar, .actions, .tabs { display: flex; flex-direction: row; align-items: center; flex-wrap: wrap; gap: 8px; } .bar { padding: 10px 12px; } .grow { flex: 1; }
  h2, h3, h4, p { margin: 0; } h2 { font-size: 19px; } .notice { padding: 10px 12px; border: 1px solid var(--line); border-radius: 8px; display: flex; flex-direction: column; gap: 8px; }
  .layout { display: flex; flex: 1; min-height: 0; gap: 10px; } .jobs { flex: 0 0 260px; overflow: auto; padding: 12px; display: flex; flex-direction: column; gap: 10px; }
  .detail { flex: 1; min-width: 0; overflow: auto; padding: 16px; display: flex; flex-direction: column; gap: 14px; overflow-wrap: anywhere; }
  .job { text-align: left; display: flex; flex-direction: column; gap: 3px; overflow-wrap: anywhere; } .job.active { border-color: var(--accent); background: #203653; } .job small, .dim { color: var(--text-dim); }
  .counts { display: grid; grid-template-columns: repeat(auto-fit, minmax(140px, 1fr)); gap: 12px; margin: 0; } .counts dd { margin: 3px 0 0; font-size: 20px; } dt { color: var(--text-dim); font-size: 12px; }
  .identity { display: grid; grid-template-columns: minmax(110px, auto) 1fr; gap: 5px 12px; } .identity dd { margin: 0; }
  .record { border: 1px solid var(--line); border-radius: 6px; padding: 10px; display: flex; flex-direction: column; gap: 8px; } pre.record { display: block; white-space: pre-wrap; font-size: 12px; margin: 0; }
  .field { display: flex; flex-direction: column; gap: 4px; } textarea { min-height: 65px; resize: vertical; } .choice { display: flex; align-items: flex-start; gap: 6px; }
  .warn { color: var(--warn); border-color: var(--warn); } .err { color: var(--err); border-color: var(--err); } .root { font-size: 12px; overflow-wrap: anywhere; }
  @media (max-width: 850px) { .layout { flex-direction: column; overflow: auto; } .jobs { flex: 0 0 auto; max-height: 240px; } .detail { flex: none; overflow: visible; } }
</style>
