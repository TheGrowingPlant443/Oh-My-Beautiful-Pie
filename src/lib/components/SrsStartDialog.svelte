<script lang="ts">
  import { untrack } from "svelte";
  import { open as pickDirectory } from "@tauri-apps/plugin-dialog";
  import { knowledge, type VaultBinding } from "../knowledge";
  import { srs, srsApi, srsModelLabel, type ScopePreparation, type SrsJob } from "../srs";
  import VaultConfiguration from "./VaultConfiguration.svelte";

  let { open, folder, architectModel = null, onClose, onExisting, onStarted, onOpenArchitect }: {
    open: boolean; folder: string; architectModel?: string | null; onClose: () => void;
    onExisting: (folder: string) => void; onStarted: (job: SrsJob) => void; onOpenArchitect: () => void | Promise<void>;
  } = $props();
  let dialog = $state<HTMLDialogElement>();
  let programmaticCloses = 0;
  let step = $state<"explanation" | "scope">("explanation");
  let vaultPicker = $state(false);
  let binding = $state<VaultBinding | null>(null);
  let evidence = $state<string[]>([]);
  let exclusions = $state<string[]>([]);
  let exclusion = $state("");
  let preview = $state<ScopePreparation | null>(null);
  let understood = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let epoch = 0;
  let openedFor = "";
  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    else if (!open && dialog.open) { programmaticCloses++; dialog.close(); }
  });
  $effect(() => {
    if (!open) { openedFor = ""; return; }
    if (openedFor === folder) return;
    openedFor = folder; epoch++;
    step = "explanation"; binding = null; preview = null; evidence = []; exclusions = []; understood = false; error = null; busy = false;
  });
  async function configure(): Promise<void> {
    step = "scope"; busy = true; error = null;
    const mine = ++epoch, target = folder;
    try {
      const caps = await knowledge.probe(target);
      if (mine !== epoch || !open) return;
      binding = caps?.projectVault ?? null;
      if (!caps) { error = "Could not inspect the project's vault. Check again or explicitly choose a destination."; return; }
      if (!binding) vaultPicker = true;
    } finally { if (mine === epoch) busy = false; }
  }
  function invalidate(): void { epoch++; preview = null; understood = false; busy = false; }
  async function addEvidence(): Promise<void> {
    try {
      const picked = await pickDirectory({ directory: true, multiple: false, title: "Add a supplementary evidence folder" });
      if (typeof picked === "string" && !evidence.includes(picked)) { evidence = [...evidence, picked]; invalidate(); }
    } catch (failure) { error = String(failure); }
  }
  async function prepare(): Promise<void> {
    if (!binding) return;
    const mine = ++epoch; busy = true; preview = null; error = null;
    try { const result = await srsApi.prepareScope(folder, binding.vaultId, [...evidence], [...exclusions]); if (mine === epoch) preview = result; }
    catch (failure) { if (mine === epoch) error = String(failure); }
    finally { if (mine === epoch) busy = false; }
  }
  async function start(): Promise<void> {
    if (!preview || !understood) return;
    busy = true; error = null;
    try {
      // A real event listener is installed before start returns or any initial status reads occur.
      await srs.connect();
      const job = await srs.start(preview.token);
      onStarted(job);
    } catch (failure) { error = String(failure); preview = null; understood = false; }
    finally { busy = false; }
  }
  // A source switch or closing the dialog fences late folder/preparation responses.
  $effect(() => { if (!open) untrack(() => { epoch++; vaultPicker = false; }); });
</script>

<dialog bind:this={dialog} aria-labelledby="srs-start-title" onclose={() => { if (programmaticCloses) programmaticCloses--; else onClose(); }} oncancel={event => { event.preventDefault(); if (!busy) onClose(); }}>
  <section>
    <header><h2 id="srs-start-title">SRS documentation</h2><button disabled={busy} onclick={onClose}>Cancel</button></header>
    <div class="body">
      <p>A <strong>Software Requirements Specification (SRS)</strong> describes what a product does: externally observable behavior, business rules, interfaces, error and boundary cases, and testable acceptance criteria.</p>
      <p>It is a detailed modernization starting point, <strong>not a 1:1 port of today's technology, source structure, screens or navigation</strong>. Current UI and implementation observations are documented separately as redesignable context. Unknown business rules are questions for you, not invented requirements.</p>
      <aside class="warning" role="note"><strong>Potentially very large token and provider charges.</strong> Without local AI this can be expensive and run 24/7 for days. There may be arbitrarily many linked documents. OMP imposes no total token, elapsed-time, document or iteration limit. Local models still require compute and time; provider, context and storage limits still apply and may pause the job.</aside>
      <p>{srsModelLabel(architectModel)}. <button onclick={() => { onClose(); void onOpenArchitect(); }}>Open Architect to change model</button></p>
      <p class="dim">Uses the existing Architect conversation and its selected model; it never silently switches providers. OMP Manager must stay open (minimized is fine). Closing checkpoints/pauses the job; reopening requires explicit Resume. No background service is installed.</p>
      {#if step === "explanation"}
        <div class="actions"><button class="primary" disabled={!folder} onclick={() => void configure()}>Create SRS</button><button disabled={!folder} onclick={() => { const target = folder; onClose(); onExisting(target); }}>View existing SRS jobs</button><button onclick={onClose}>Cancel</button></div>
      {:else}
        <h3>Confirm source scope and destination</h3>
        <p>Project: <code>{folder}</code></p>
        {#if binding}
          <p>Destination: <strong>{binding.mode === "obsidian" ? "Obsidian vault" : "Plain Markdown folder"}</strong><br /><code>{binding.root}/SRS/&lt;job ID&gt;/SRS.md</code></p>
        {:else}<button disabled={busy} onclick={() => vaultPicker = true}>Choose documentation vault…</button>{/if}
        <p class="dim">Only a human-picked vault is used. Existing notes remain untouched. The source scope is read-only: no project scripts, tests, builds or migrations are executed.</p>
        <h4>Supplementary evidence</h4>
        <p class="dim">Add folders only when they are relevant and safe to read. Secrets, caches and generated material are classified/excluded by the native inventory; ignored or unreadable evidence is not silently treated as no behavior.</p>
        <ul>{#each evidence as path (path)}<li><code>{path}</code> <button disabled={busy} onclick={() => { evidence = evidence.filter(item => item !== path); invalidate(); }}>Remove</button></li>{/each}</ul>
        <button disabled={busy} onclick={() => void addEvidence()}>Add evidence folder…</button>
        <label class="field">Explicit scope exclusion (repository-relative path or absolute path)<span class="row"><input bind:value={exclusion} spellcheck="false" /><button disabled={busy || !exclusion.trim()} onclick={() => { const path = exclusion.trim(); if (!exclusions.includes(path)) exclusions = [...exclusions, path]; exclusion = ""; invalidate(); }}>Add exclusion</button></span></label>
        <ul>{#each exclusions as path (path)}<li><code>{path}</code> <button disabled={busy} onclick={() => { exclusions = exclusions.filter(item => item !== path); invalidate(); }}>Remove</button></li>{/each}</ul>
        <button disabled={busy || !binding} onclick={() => void prepare()}>{busy ? "Working…" : "Preview recorded scope"}</button>
        {#if preview}
          <div class="preview">
            <h4>Recorded scope preview</h4>
            <ul>{#each preview.roots as root (root.path)}<li><code>{root.path}</code> — {root.rationale}</li>{/each}</ul>
            <details><summary>Exclusions ({preview.exclusions.length})</summary><ul>{#each preview.exclusions as item, i (i)}<li><strong>{item.source === "human" ? "Human exclusion" : "Safety exclusion"}:</strong> <code>{item.path}</code> — {item.reason}</li>{/each}</ul></details>
            <p>Destination: <code>{preview.vaultRoot}</code> ({preview.vaultMode}). Pinned template: <code>{preview.template.revision}</code>.</p>
            {#each preview.warnings as warning, i (i)}<p class="warning">{warning}</p>{/each}
            <label class="choice"><input type="checkbox" bind:checked={understood} /> I confirm this source scope and destination, understand the potentially very large charges and open-ended runtime, and want to start with the Architect's current model.</label>
            <button class="primary" disabled={busy || !understood} onclick={() => void start()}>Start SRS</button>
          </div>
        {/if}
        <button disabled={busy} onclick={() => { step = "explanation"; invalidate(); }}>Back to explanation</button>
      {/if}
      {#if error}<p class="err" role="alert">{error}</p>{/if}
    </div>
  </section>
</dialog>
<VaultConfiguration open={vaultPicker} scope={{ kind: "project" }} {folder} title="Choose the SRS documentation destination" offerObsidianInstall={true} onClose={() => vaultPicker = false} onBound={vault => { binding = vault; vaultPicker = false; invalidate(); }} />

<style>
  dialog { width: min(820px, 95vw); max-height: 90vh; padding: 0; border: 1px solid var(--line); border-radius: var(--radius); background: var(--panel); color: var(--text); }
  dialog::backdrop { background: rgba(4, 7, 12, .7); }
  section { display: flex; flex-direction: column; max-height: 90vh; }
  header { display: flex; gap: 12px; align-items: center; justify-content: space-between; padding: 14px 16px; border-bottom: 1px solid var(--line); }
  h2, h3, h4, p { margin: 0; } h2 { font-size: 20px; }
  .body { overflow: auto; padding: 16px; display: flex; flex-direction: column; gap: 14px; overflow-wrap: anywhere; }
  .warning { padding: 12px; border: 1px solid var(--warn); border-radius: 8px; color: var(--warn); }
  .preview { border: 1px solid var(--line); padding: 12px; border-radius: 8px; display: flex; flex-direction: column; gap: 12px; }
  .actions, .row { display: flex; flex-wrap: wrap; gap: 8px; } .row input { flex: 1; min-width: 140px; }
  .field { display: flex; flex-direction: column; gap: 6px; } .choice { display: flex; gap: 8px; align-items: flex-start; }
  ul { margin: 0; padding-left: 20px; } li { margin: 5px 0; } .dim { color: var(--text-dim); } .err { color: var(--err); }
</style>
