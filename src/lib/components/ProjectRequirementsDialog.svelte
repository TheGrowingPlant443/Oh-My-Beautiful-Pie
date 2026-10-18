<script lang="ts">
  import { onDestroy, untrack } from "svelte";
  import { api } from "../api";
  import type { ProjectInsightStatus, ProjectRequirements, RequirementItem } from "../api";
  import { errorText } from "../stores";
  import { architectItems, detectedLabel, environmentLabel, evidenceLocation, groupRequirements, installLabel, insightInFlight, needLabel, sourceLabel } from "../requirements-view";
  let { open, folder, onclose, onOpenCodeQuality }: {
    open: boolean; folder: string; onclose: () => void;
    /** Navigates to Code quality for `folder` (its setup lives there); the link is hidden when this is not supplied. */
    onOpenCodeQuality?: (folder: string) => void;
  } = $props();
  const POLL_MS = 2000;
  const POLL_LIMIT_MS = 10 * 60_000;
  let dialog = $state<HTMLDialogElement>();
  let popup = $state<HTMLDialogElement>();
  let programmaticCloses = 0;
  let requirements = $state.raw<ProjectRequirements | null>(null);
  let loading = $state(false);
  let error = $state("");
  let selected = $state.raw<RequirementItem | null>(null);
  let insight = $state.raw<ProjectInsightStatus | null>(null);
  let insightError = $state("");
  let asking = $state(false);
  let loadEpoch = 0;
  let insightEpoch = 0;
  let pollTimer: ReturnType<typeof setTimeout> | null = null;
  const groups = $derived(requirements ? groupRequirements(requirements.items) : []);
  const requirementsResult = $derived(insight?.result?.kind === "requirements" ? insight.result : null);
  const findings = $derived(requirementsResult ? architectItems(requirementsResult.items) : []);

  function stopInsight(): void {
    insightEpoch++;
    if (pollTimer !== null) clearTimeout(pollTimer);
    pollTimer = null;
    asking = false;
  }
  function stopAll(): void { loadEpoch++; stopInsight(); }
  /** Reads the static analysis and any saved Architect result once. Nothing is started and nothing polls. */
  async function load(target: string): Promise<void> {
    const mine = ++loadEpoch;
    loading = true; error = ""; insightError = "";
    const [analysis, saved] = await Promise.allSettled([api.getProjectRequirements(target), api.getProjectInsight(target, "requirements")]);
    if (mine !== loadEpoch) return;
    loading = false;
    if (analysis.status === "fulfilled") requirements = analysis.value; else error = errorText(analysis.reason);
    if (saved.status === "fulfilled") insight = saved.value?.kind === "requirements" ? saved.value : null;
    else insightError = errorText(saved.reason);
  }
  /** Polls only after the human asked (or asked to check again); stops on close, folder change, unmount, a terminal state or the deadline. */
  function poll(mine: number, target: string, deadline: number): void {
    if (mine !== insightEpoch) return;
    if (!insight || !insightInFlight(insight.state)) { asking = false; return; }
    if (Date.now() >= deadline) {
      asking = false;
      insightError = "The Architect has not finished yet; it may still be working. Use Check Architect progress later.";
      return;
    }
    pollTimer = setTimeout(async () => {
      pollTimer = null;
      try {
        const status = await api.getProjectInsight(target, "requirements");
        if (mine !== insightEpoch) return;
        if (status?.kind === "requirements") insight = status;
      } catch (reason) {
        if (mine !== insightEpoch) return;
        insightError = errorText(reason);
        asking = false;
        return;
      }
      poll(mine, target, deadline);
    }, POLL_MS);
  }
  async function askArchitect(): Promise<void> {
    // An explicit click on a finished (completed or failed) request asks again; an in-flight one is joined, never resent.
    const rerun = insight !== null && !insightInFlight(insight.state);
    stopInsight();
    const mine = insightEpoch, target = folder;
    asking = true; insightError = "";
    try {
      const status = await api.requestProjectInsight(target, "requirements", rerun);
      if (mine !== insightEpoch) return;
      insight = status;
      poll(mine, target, Date.now() + POLL_LIMIT_MS);
    } catch (reason) {
      if (mine !== insightEpoch) return;
      insightError = errorText(reason);
      asking = false;
    }
  }
  function checkProgress(): void {
    stopInsight();
    asking = true; insightError = "";
    poll(insightEpoch, folder, Date.now() + POLL_LIMIT_MS);
  }
  function insightMessage(status: ProjectInsightStatus): string {
    if (status.state === "pending") return "Request admitted; waiting to reach the Architect…";
    if (status.state === "submitted") return "The Architect is working on it…";
    if (status.state === "uncertain") return "Delivery to the Architect is uncertain; the request was not sent again. A late result may still arrive.";
    if (status.state === "failed") return status.message ?? "The request ended without a usable result.";
    return findings.length ? "Saved Architect findings. They are unverified and never change the states above." : "The Architect found nothing further to add.";
  }
  function openSetup(): void {
    const target = folder;
    selected = null;
    onclose();
    onOpenCodeQuality?.(target);
  }

  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    else if (!open && dialog.open) { programmaticCloses++; dialog.close(); }
  });
  $effect(() => {
    if (!popup) return;
    if (selected && !popup.open) popup.showModal();
    else if (!selected && popup.open) popup.close();
  });
  $effect(() => {
    const target = folder, visible = open;
    untrack(() => {
      stopAll();
      requirements = null; insight = null; selected = null; error = ""; insightError = "";
      if (visible && target) void load(target);
    });
  });
  onDestroy(stopAll);
</script>
{#snippet row(item: RequirementItem)}
  <li>
    <button class="item" onclick={() => { selected = item; }} aria-haspopup="dialog">
      <span class="name">{item.name}</span>
      <span class="badges">
        <span class="badge" data-need={item.need}>{needLabel(item.need)}</span>
        <span class="badge">{environmentLabel(item.environment)}</span>
        <span class="badge" data-detected={item.detected.state}>{detectedLabel(item.detected)}</span>
        <span class="badge" data-install={item.install.availability}>{installLabel(item.install)}</span>
        {#if item.source === "architect"}<span class="badge arch">{sourceLabel(item.source)}</span>{/if}
      </span>
    </button>
  </li>
{/snippet}
<dialog bind:this={dialog} aria-labelledby="project-requirements-title" onclose={() => { if (programmaticCloses) programmaticCloses--; else onclose(); }} oncancel={event => { event.preventDefault(); onclose(); }}>
  <section>
    <header><h3 id="project-requirements-title">Project requirements</h3><button onclick={onclose}>Close</button></header>
    <div class="body">
      <p class="dim">What this repository's own files say it needs, and whether this computer appears to have it. Nothing is installed, started or run from here.<br />{folder}</p>
      {#if loading}<p role="status">Reading the project files and checking tool versions…</p>{/if}
      {#if error}<p class="err" role="alert">{error}</p>{/if}
      {#each groups as group (group.category)}
        <h4>{group.label}</h4>
        <ul class="items">{#each group.items as item (item.id)}{@render row(item)}{/each}</ul>
      {/each}
      {#if requirements && !groups.length}<p>No requirements were found in the project files.</p>{/if}
      {#if requirements?.limitations.length}
        <h4>Limitations</h4>
        <ul>{#each requirements.limitations as limitation}<li>{limitation}</li>{/each}</ul>
      {/if}
      <h4>Fill the gaps with the Architect</h4>
      <p class="dim">Asks the Architect to read the repository files and list anything the static view missed. Its findings are unverified and never change the states above.</p>
      <div class="actions">
        <button onclick={() => void askArchitect()} disabled={asking || loading || !requirements}>Ask Architect to fill gaps</button>
        {#if insight && insightInFlight(insight.state) && !asking}<button onclick={checkProgress}>Check Architect progress</button>{/if}
        <button onclick={() => void load(folder)} disabled={loading || asking} title="Reads the project files and tool versions again.">Refresh</button>
      </div>
      {#if insight}<p role="status">{insightMessage(insight)}</p>{/if}
      {#if insightError}<p class="err" role="alert">{insightError}</p>{/if}
      {#if findings.length}
        <h4>Architect findings (unverified)</h4>
        <ul class="items">{#each findings as item}{@render row(item)}{/each}</ul>
      {/if}
      {#if requirementsResult?.searchedAreas.length}
        <h4>Where the Architect looked</h4>
        <ul>{#each requirementsResult.searchedAreas as area}<li>{area}</li>{/each}</ul>
      {/if}
      {#if requirementsResult?.limitations.length}
        <h4>Architect limitations</h4>
        <ul>{#each requirementsResult.limitations as limitation}<li>{limitation}</li>{/each}</ul>
      {/if}
    </div>
  </section>
</dialog>
<dialog class="popup" bind:this={popup} aria-labelledby="requirement-popup-title" onclose={() => { selected = null; }}>
  {#if selected}
    <section>
      <header><h3 id="requirement-popup-title">{selected.name}</h3><button onclick={() => { selected = null; }}>Close</button></header>
      <div class="body">
        <div class="badges">
          <span class="badge" data-need={selected.need}>{needLabel(selected.need)}</span>
          <span class="badge">{environmentLabel(selected.environment)}</span>
          <span class="badge" data-detected={selected.detected.state}>{detectedLabel(selected.detected)}</span>
          <span class="badge" data-install={selected.install.availability}>{installLabel(selected.install)}</span>
          <span class="badge" class:arch={selected.source === "architect"}>{sourceLabel(selected.source)}</span>
        </div>
        <h4>Why it is listed</h4>
        <p>{selected.explanation}</p>
        <h4>Where it runs</h4>
        <p>{environmentLabel(selected.environment)}</p>
        <h4>On this computer</h4>
        <p>{detectedLabel(selected.detected)}{selected.detected.path ? ` — ${selected.detected.path}` : ""}</p>
        <h4>Installing it</h4>
        <p>{selected.install.note}</p>
        <h4>Evidence</h4>
        {#if selected.source === "architect"}<p class="dim">Architect citations are unverified until you check them.</p>{/if}
        {#if selected.evidence.length}
          <ul>{#each selected.evidence as entry}<li><code>{evidenceLocation(entry)}</code> {entry.text}</li>{/each}</ul>
        {:else}<p class="dim">No file evidence.</p>{/if}
        {#if selected.setupLink === "codeQuality" && onOpenCodeQuality}
          <div class="actions"><button onclick={openSetup} title="Opens Code quality for this project. Nothing is installed until you confirm a step there.">Open Code quality setup</button></div>
        {/if}
      </div>
    </section>
  {/if}
</dialog>
<style>
  dialog:not([open]) { display: none; }
  dialog { width: min(820px, 95vw); max-height: 85vh; box-sizing: border-box; border: 1px solid var(--line); padding: 0; border-radius: var(--radius); background: var(--panel); color: var(--text); }
  dialog.popup { width: min(620px, 92vw); }
  dialog::backdrop { background: rgba(4, 7, 12, .66); }
  section { display: flex; flex-direction: column; max-height: 85vh; }
  header { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 14px 16px; border-bottom: 1px solid var(--line); }
  h3, h4, p { margin: 0; }
  .body { overflow: auto; padding: 16px; display: flex; flex-direction: column; gap: 12px; overflow-wrap: anywhere; }
  .dim { color: var(--text-dim); }
  ul { margin: 0; padding-left: 20px; }
  ul.items { list-style: none; padding: 0; display: flex; flex-direction: column; gap: 6px; }
  button.item { width: 100%; display: flex; flex-wrap: wrap; align-items: center; justify-content: space-between; gap: 8px; text-align: left; }
  .name { font-weight: 600; }
  .badges { display: flex; flex-wrap: wrap; gap: 6px; }
  .badge { padding: 0 6px; font-size: 11px; border: 1px solid var(--line); border-radius: 999px; color: var(--text-dim); }
  .badge[data-need="required"] { color: var(--text); }
  .badge[data-detected="found"] { color: var(--ok); }
  .badge[data-detected="missing"] { color: var(--err); }
  .badge[data-install="automatic"] { color: var(--accent); }
  .badge.arch { color: var(--warn); }
  .actions { display: flex; flex-wrap: wrap; gap: 8px; }
  code { font-family: var(--mono); font-size: 12px; }
</style>
