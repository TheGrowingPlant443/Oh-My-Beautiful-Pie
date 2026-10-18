<script lang="ts">
  import { api, type ManifestPreparation } from "../api";
  import { errorText } from "../stores";
  import type { JobSnapshot } from "../package-insights";
  import { citationText, discoveryEcosystems, type InsightJobState } from "../project-insight";
  let { open, discovery, folder, onclose, onrun, onopenarchitect, onrescan }: {
    open: boolean; discovery: JobSnapshot<InsightJobState>; folder: string;
    onclose: () => void; onrun: () => void; onopenarchitect: () => void; onrescan: () => void;
  } = $props();
  let dialog = $state<HTMLDialogElement>();
  let programmaticCloses = 0;
  let ecosystem = $state("");
  let prepared = $state.raw<ManifestPreparation | null>(null);
  let manifestError = $state<string | null>(null);
  let busy = $state(false);
  const requestId = $derived(discovery.requestId);
  const phase = $derived(discovery.state?.state ?? null);
  const finished = $derived(phase === "completed");
  const result = $derived.by(() => { const value = discovery.state?.result; return value?.kind === "dependencyDiscovery" ? value : null; });
  const ecosystems = $derived(discoveryEcosystems(result?.items ?? []));
  const chosen = $derived(ecosystems.includes(ecosystem) ? ecosystem : (ecosystems.find(name => name === "pypi" || name === "npm") ?? ecosystems[0] ?? ""));
  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    else if (!open && dialog.open) { programmaticCloses++; dialog.close(); }
  });
  // A different request (or a closed dialog) never keeps an old preview or token.
  $effect(() => { void requestId; void open; prepared = null; manifestError = null; });
  async function prepare(): Promise<void> {
    const id = discovery.requestId;
    if (!id || !chosen || busy) return;
    busy = true; manifestError = null; prepared = null;
    try {
      const value = await api.prepareManifestCreation(id, chosen);
      if (discovery.requestId === id && open) prepared = value;
    } catch (error) { manifestError = errorText(error); }
    finally { busy = false; }
  }
  async function confirm(): Promise<void> {
    const token = prepared?.confirmationToken;
    if (!token || busy) return;
    busy = true; manifestError = null;
    try {
      await api.confirmManifestCreation(token);
      prepared = null;
      onrescan();
    } catch (error) { prepared = null; manifestError = errorText(error); }
    finally { busy = false; }
  }
</script>
<dialog bind:this={dialog} aria-labelledby="dependency-discovery-title" onclose={() => { if (programmaticCloses) programmaticCloses--; else onclose(); }} oncancel={event => { event.preventDefault(); onclose(); }}>
  <section>
    <header><h3 id="dependency-discovery-title">Dependencies found by The Architect</h3><button onclick={onclose}>Close</button></header>
    <div class="body">
      <p class="dim">This repository has no dependency manifest the app recognises. The Architect read its files and listed what it uses. These are sourced model findings, not a verified dependency graph: versions are only as declared, and nothing was installed, looked up or changed.</p>
      <p class="dim">Project: {folder}</p>
      {#if discovery.error}<p class="err" role="alert">{discovery.error}</p>{/if}
      {#if discovery.timedOut}<p role="status">The Architect has not answered yet; work may still be running.</p>
      {:else if phase === "uncertain"}<p role="status">Prompt delivery acknowledgement is uncertain. Waiting for a possible late result; the request will not be resent automatically.{discovery.state?.message ? ` ${discovery.state.message}` : ""}</p>
      {:else if discovery.pending}<p role="status">{discovery.admission ? "Waiting for The Architect to read the repository files…" : "Requesting the Architect's help…"}</p>{/if}
      {#if phase === "failed" && !discovery.error}<p class="err" role="alert">{discovery.state?.message ?? "The request ended without a result."}</p>{/if}
      {#if finished}<p class="notice" role="note">Saved analysis — the repository may have changed since.</p>{/if}
      {#if result}
        <h4>Dependencies ({result.items.length})</h4>
        {#if result.items.length}
          <ul class="items">
            {#each result.items as item, index (index)}
              <li>
                <div class="row"><strong>{item.name}</strong><span class="dim">{item.ecosystem ?? "unknown ecosystem"}</span></div>
                <small class="dim">Declared version: {item.declaredVersion ?? "unknown"} · Not resolved{item.relationship ? ` · ${item.relationship}` : ""}</small>
                {#if item.notes}<p>{item.notes}</p>{/if}
                <small>{#each item.citations as citation, at (at)}<code>{citationText(citation)}</code> <span class="badge" title={citation.verified ? "The cited file exists in this repository." : "The cited path could not be confirmed in this repository."}>{citation.verified ? "path exists" : "unverified"}</span>{" "}{/each}</small>
              </li>
            {/each}
          </ul>
        {:else}
          <p>The Architect found no dependencies.</p>
        {/if}
        <h4>Searched</h4>
        <ul>{#each result.searchedAreas as area}<li>{area}</li>{/each}</ul>
        <h4>Limitations</h4>
        {#if result.limitations.length}<ul>{#each result.limitations as limitation}<li>{limitation}</li>{/each}</ul>{:else}<p class="dim">None stated.</p>{/if}
        {#if result.items.length}
          <div class="manifest">
            <h4>Create a manifest</h4>
            {#if ecosystems.length}
              <label class="inline">Ecosystem
                <select aria-label="Manifest ecosystem" value={chosen} disabled={busy || prepared !== null} onchange={event => { ecosystem = event.currentTarget.value; }}>
                  {#each ecosystems as name (name)}<option value={name}>{name}</option>{/each}
                </select>
              </label>
              <button disabled={busy || prepared !== null || !chosen} title="Preview a requirements.txt (pypi) or package.json (npm) at the repository root before anything is written" onclick={() => void prepare()}>Create manifest…</button>
            {:else}
              <p class="dim">No item names a known ecosystem, so a manifest cannot be created.</p>
            {/if}
            {#if manifestError}<p class="err" role="alert">{manifestError}</p>{/if}
            {#if prepared}
              <p>This creates <code>{prepared.path}</code>. An existing file is never overwritten.</p>
              <pre aria-label="Manifest preview">{prepared.preview}</pre>
              <div class="actions">
                <button disabled={busy} onclick={() => void confirm()}>Create file</button>
                <button disabled={busy} onclick={() => { prepared = null; manifestError = null; }}>Cancel</button>
              </div>
            {/if}
          </div>
        {/if}
      {/if}
      <div class="actions">
        <button onclick={onrun} disabled={discovery.pending} title="Replaces a finished or failed request with a new one. A request still in progress is joined, not duplicated.">Run again</button>
        <button onclick={onopenarchitect}>Open Architect</button>
      </div>
      <p class="dim">Closing stops this popup's polling, not The Architect's work or its shared conversation.</p>
    </div>
  </section>
</dialog>
<style>
  dialog { width: min(760px, 95vw); max-height: 85vh; box-sizing: border-box; border: 1px solid var(--line); padding: 0; border-radius: var(--radius); background: var(--panel); color: var(--text); }
  dialog::backdrop { background: rgba(4, 7, 12, .66); }
  section { display: flex; flex-direction: column; max-height: 85vh; }
  header { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 14px 16px; border-bottom: 1px solid var(--line); }
  h3, h4, p { margin: 0; }
  .body { overflow: auto; padding: 16px; display: flex; flex-direction: column; gap: 14px; overflow-wrap: anywhere; }
  ul { margin: 0; padding-left: 20px; }
  .notice { border: 1px solid var(--line); border-radius: 6px; padding: 8px 10px; }
  .items { list-style: none; padding: 0; display: flex; flex-direction: column; gap: 8px; }
  .items li { border: 1px solid var(--line); border-radius: 6px; padding: 10px 12px; display: flex; flex-direction: column; gap: 6px; }
  .row { display: flex; justify-content: space-between; gap: 12px; }
  .badge { padding: 0 6px; font-size: 11px; border: 1px solid var(--line); border-radius: 999px; color: var(--text-dim); }
  .manifest { display: flex; flex-direction: column; gap: 10px; align-items: flex-start; border-top: 1px solid var(--line); padding-top: 12px; }
  .inline { display: inline-flex; align-items: center; gap: 8px; }
  pre { margin: 0; max-height: 240px; overflow: auto; align-self: stretch; padding: 8px 10px; border: 1px solid var(--line); border-radius: 6px; white-space: pre-wrap; }
  .actions { display: flex; flex-wrap: wrap; gap: 8px; }
</style>
