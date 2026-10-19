<script lang="ts">
  import { open as pickFolderDialog } from "@tauri-apps/plugin-dialog";
  import { get } from "svelte/store";
  import { knowledge, knowledgeApi, obsidianInstalled, type ScopeRequest, type VaultBinding, type VaultMode, type VaultPreparation } from "../knowledge";

  // Human-only: choosing where a project's (or the global) documentation vault lives. Nothing is created, adopted or moved until
  // the preview below is confirmed. Existing notes stay as they are, an existing Obsidian vault is only an option in the list, and
  // Obsidian itself is never launched, configured or installed from here.
  let { open, scope, folder, title = "Choose a documentation vault", offerObsidianInstall = true, onClose, onBound }: {
    open: boolean; scope: ScopeRequest; folder: string | null; title?: string;
    /** Show the install / decline choice when Obsidian is not found (the SRS start flow). */
    offerObsidianInstall?: boolean;
    onClose: () => void; onBound: (binding: VaultBinding) => void;
  } = $props();

  let dialog = $state<HTMLDialogElement>();
  let programmaticCloses = 0;
  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    else if (!open && dialog.open) { programmaticCloses++; dialog.close(); }
  });

  let caps = $state(get(knowledge).capabilities);
  $effect(() => knowledge.subscribe(value => { caps = value.capabilities; }));
  $effect(() => { if (open) void knowledge.probe(folder); });

  const installed = $derived(obsidianInstalled(caps));
  const declined = $derived(caps?.obsidian.declinedForProject === true);
  const suggested = $derived(scope.kind === "global" ? caps?.suggestedGlobalRoot ?? null : caps?.suggestedProjectRoot ?? null);
  const registered = $derived(installed ? caps?.obsidian.registeredVaults ?? [] : []);
  /** Obsidian is missing and the human has neither declined nor installed it yet. */
  const undecided = $derived(offerObsidianInstall && !!caps && caps.obsidian.state === "absent" && !declined);

  let destination = $state<string | null>(null);
  let mode = $state<VaultMode>("markdown");
  let modeTouched = $state(false);
  $effect(() => { if (!modeTouched) mode = installed ? "obsidian" : "markdown"; });
  let preparation = $state<VaultPreparation | null>(null);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let note = $state<string | null>(null);
  let epoch = 0;

  async function prepare(path: string): Promise<void> {
    const mine = ++epoch;
    destination = path;
    preparation = null; error = null; busy = true;
    try {
      const prepared = await knowledgeApi.prepareVault(scope, folder, path, mode);
      if (mine === epoch) preparation = prepared;
    } catch (failure) {
      if (mine === epoch) error = String(failure);
    } finally {
      if (mine === epoch) busy = false;
    }
  }
  async function browse(): Promise<void> {
    try {
      const picked = await pickFolderDialog({ directory: true, multiple: false, title: "Choose the vault folder" });
      if (typeof picked === "string") await prepare(picked);
    } catch (failure) { error = String(failure); }
  }
  async function confirm(): Promise<void> {
    if (!preparation) return;
    busy = true; error = null;
    try {
      const binding = await knowledgeApi.confirmVault(preparation.token);
      preparation = null;
      await knowledge.probe(folder);
      onBound(binding);
    } catch (failure) {
      // The token is consumed or the folder changed under the preview: ask again rather than guessing.
      error = String(failure);
      if (destination) void prepare(destination);
    } finally { busy = false; }
  }
  async function install(): Promise<void> {
    try { await knowledgeApi.openObsidianDownload(); note = "The Obsidian download page was opened. Install it yourself, then choose Check again. Nothing was installed by OMP Manager."; } catch (failure) { error = String(failure); }
  }
  async function checkAgain(): Promise<void> {
    note = null;
    const found = await knowledge.probe(folder);
    note = found?.obsidian.state === "installed" ? "Obsidian was found." : "Obsidian was still not found.";
  }
  async function decline(): Promise<void> {
    if (folder === null) return;
    try { knowledge.adopt(await knowledgeApi.recordObsidianDecline(folder)); modeTouched = true; mode = "markdown"; note = "Obsidian will not be offered again for this project. Notes are plain Markdown files."; } catch (failure) { error = String(failure); }
  }
  const blocked = $derived(!!preparation && preparation.conflicts.length > 0);
</script>
<dialog bind:this={dialog} aria-labelledby="vault-config-title" onclose={() => { if (programmaticCloses) programmaticCloses--; else onClose(); }} oncancel={event => { event.preventDefault(); onClose(); }}>
  <section>
    <header><h3 id="vault-config-title">{title}</h3><button onclick={onClose}>Cancel</button></header>
    <div class="body">
      <p class="dim">{scope.kind === "global" ? "One vault holds your agent setup Markdown for every project." : `One vault holds the documentation of ${scope.repositoryRoot ?? folder ?? "this project"}.`} It contains Markdown only; scripts, tools, settings and credentials never enter it.</p>
      {#if undecided}
        <div class="notice" role="status">
          <p><strong>Obsidian was not found on this computer.</strong> It is optional: without it you read and write the same notes as plain Markdown in the built-in viewer.</p>
          <div class="actions">
            <button onclick={() => void install()}>Install Obsidian</button>
            <button onclick={() => void checkAgain()}>Check again</button>
            <button onclick={() => void decline()}>No, I don't want Obsidian</button>
            <button onclick={onClose}>Cancel</button>
          </div>
        </div>
      {:else if caps && caps.obsidian.state === "unreadable"}
        <p class="warn" role="status">Obsidian's install could not be read ({caps.obsidian.message ?? "unknown reason"}); this is not proof that it is missing. Plain Markdown mode works.</p>
      {/if}
      {#if note}<p role="status">{note}</p>{/if}

      <div class="block">
        <h4>Where should the vault live?</h4>
        {#if suggested}
          <label class="choice"><input type="radio" name="vault-destination" checked={destination === suggested} onchange={() => void prepare(suggested)} /> <span><strong>New folder (recommended)</strong><br /><code>{suggested}</code></span></label>
        {/if}
        {#each registered as path (path)}
          <label class="choice"><input type="radio" name="vault-destination" checked={destination === path} onchange={() => void prepare(path)} /> <span><strong>An existing Obsidian vault</strong><br /><code>{path}</code><br /><small class="dim">Only used if you pick it and confirm the preview. Your other notes are not touched.</small></span></label>
        {/each}
        <button onclick={() => void browse()}>Choose another folder…</button>
      </div>

      <fieldset class="block">
        <legend>How is it opened?</legend>
        <label class="choice"><input type="radio" name="vault-mode" value="obsidian" disabled={!installed} checked={mode === "obsidian"} onchange={() => { mode = "obsidian"; modeTouched = true; if (destination) void prepare(destination); }} /> <span>Obsidian vault <small class="dim">{installed ? `(Obsidian ${caps?.obsidian.version ?? ""} found)` : "(Obsidian not found)"}</small></span></label>
        <label class="choice"><input type="radio" name="vault-mode" value="markdown" checked={mode === "markdown"} onchange={() => { mode = "markdown"; modeTouched = true; if (destination) void prepare(destination); }} /> <span>Plain Markdown folder, shown in OMP Manager only</span></label>
      </fieldset>

      {#if busy && !preparation}<p role="status">Checking the folder…</p>{/if}
      {#if error}<p class="err" role="alert">{error}</p>{/if}
      {#if preparation}
        <div class="block preview" aria-label="Preview">
          <h4>Preview</h4>
          <p><code>{preparation.root}</code><br />{preparation.exists ? `The folder exists with ${preparation.existingMarkdownCount.toLocaleString()} Markdown note${preparation.existingMarkdownCount === 1 ? "" : "s"}; they stay as they are.` : "The folder will be created only when you confirm Use this vault."}</p>
          {#if preparation.conflicts.length}
            <p class="err" role="alert">This folder cannot be used as it is:</p>
            <ul>{#each preparation.conflicts as item (item)}<li>{item}</li>{/each}</ul>
          {/if}
          {#if preparation.notes.length}<ul>{#each preparation.notes as item (item)}<li>{item}</li>{/each}</ul>{/if}
          <div class="actions"><button disabled={busy || blocked} onclick={() => void confirm()}>Use this vault</button></div>
        </div>
      {/if}
    </div>
  </section>
</dialog>
<style>
  dialog { width: min(720px, 95vw); max-height: 85vh; box-sizing: border-box; border: 1px solid var(--line); padding: 0; border-radius: var(--radius); background: var(--panel); color: var(--text); }
  dialog::backdrop { background: rgba(4, 7, 12, .66); }
  section { display: flex; flex-direction: column; max-height: 85vh; }
  header { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 14px 16px; border-bottom: 1px solid var(--line); }
  h3, h4, p { margin: 0; }
  .body { overflow: auto; padding: 16px; display: flex; flex-direction: column; gap: 14px; overflow-wrap: anywhere; }
  .block { display: flex; flex-direction: column; gap: 6px; border: 0; padding: 0; margin: 0; }
  .preview { border: 1px solid var(--line); border-radius: 8px; padding: 10px 12px; }
  .notice, .warn { border: 1px solid var(--warn); border-radius: 8px; padding: 8px 10px; }
  .choice { display: flex; gap: 8px; align-items: flex-start; font-size: 13px; }
  .choice input { width: auto; flex: 0 0 auto; margin: 3px 0 0; }
  .choice span { min-width: 0; }
  .actions { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 6px; }
  ul { margin: 0; padding-left: 20px; }
  .dim { color: var(--text-dim); }
  .err { color: var(--err); }
</style>
