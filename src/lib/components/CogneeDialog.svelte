<script lang="ts">
  import { open as pickFolderDialog } from "@tauri-apps/plugin-dialog";
  import { get } from "svelte/store";
  import { knowledge, knowledgeApi, validateCogneeUrl } from "../knowledge";

  // Opens an already running Cognee UI in the system browser. It never starts Cognee, downloads anything, runs its CLI or touches its
  // memory: installing and starting it stays manual. The address must be a loopback http(s) address; the backend checks it again.
  let { open, onClose }: { open: boolean; onClose: () => void } = $props();

  let dialog = $state<HTMLDialogElement>();
  let programmaticCloses = 0;
  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    else if (!open && dialog.open) { programmaticCloses++; dialog.close(); }
  });

  let caps = $state(get(knowledge).capabilities);
  $effect(() => knowledge.subscribe(value => { caps = value.capabilities; }));
  const cognee = $derived(caps?.cognee ?? null);

  let url = $state("");
  let installation = $state("");
  let seeded = false;
  $effect(() => {
    if (!open) { seeded = false; return; }
    if (seeded || !cognee) return;
    seeded = true;
    url = cognee.uiUrl ?? "";
    installation = cognee.installationFolder ?? "";
  });
  let busy = $state(false);
  let error = $state<string | null>(null);
  let note = $state<string | null>(null);
  const checked = $derived(url.trim() === "" ? null : validateCogneeUrl(url));

  async function save(): Promise<void> {
    const parsed = validateCogneeUrl(url);
    if (!parsed.ok) { error = parsed.message; return; }
    busy = true; error = null; note = null;
    try {
      await knowledgeApi.configureCognee(parsed.url, installation.trim() || null);
      await knowledge.probe(get(knowledge).folder);
      note = "Saved. Cognee itself was not started, probed or changed.";
    } catch (failure) { error = String(failure); } finally { busy = false; }
  }
  async function browse(): Promise<void> {
    try {
      const picked = await pickFolderDialog({ directory: true, multiple: false, title: "Choose the Cognee installation folder" });
      if (typeof picked === "string") installation = picked;
    } catch (failure) { error = String(failure); }
  }
  async function launch(): Promise<void> {
    busy = true; error = null;
    try { await knowledgeApi.openCognee(); note = "Opened in your browser."; } catch (failure) { error = String(failure); } finally { busy = false; }
  }
</script>
<dialog bind:this={dialog} aria-labelledby="cognee-title" onclose={() => { if (programmaticCloses) programmaticCloses--; else onClose(); }} oncancel={event => { event.preventDefault(); onClose(); }}>
  <section>
    <header><h3 id="cognee-title">Cognee</h3><button onclick={onClose}>Close</button></header>
    <div class="body">
      {#if !cognee}
        <p class="dim" role="status">Cognee has not been looked for yet.</p>
      {:else if cognee.state === "unreadable"}
        <p class="err" role="status"><strong>Cognee discovery could not be completed.</strong> {cognee.message ?? "The installation metadata could not be read."} This is not an absent-installation result. Check permissions or select the installation folder and check again.</p>
      {:else if !cognee.installed}
        <p role="status">Cognee was not found on this computer{cognee.message ? ` (${cognee.message})` : ""}. This is not proof it is absent; if it is installed elsewhere, give its folder below.</p>
      {:else if !cognee.uiUrl}
        <p role="status"><strong>Cognee is installed; its UI is not configured.</strong>{cognee.version ? ` (version ${cognee.version})` : ""}</p>
        <p class="dim">OMP Manager does not start Cognee or its interface. Start the UI yourself, then enter the address it listens on (for example <code>http://127.0.0.1:3000</code>).</p>
      {:else}
        <p role="status">Cognee {cognee.version ?? ""} is installed. The saved UI address is <code>{cognee.uiUrl}</code>.</p>
      {/if}
      <label class="field">Address of the running Cognee UI
        <input type="url" bind:value={url} placeholder="http://127.0.0.1:3000" autocomplete="off" spellcheck="false" />
      </label>
      {#if checked && !checked.ok}<p class="err" role="alert">{checked.message}</p>{/if}
      <label class="field">Cognee installation folder (optional)
        <span class="pick"><input type="text" bind:value={installation} placeholder="Found automatically when empty" spellcheck="false" /><button onclick={() => void browse()}>Browse…</button></span>
      </label>
      <p class="dim">Only the folder and address are stored, in this app's own settings. Cognee's configuration and memory are never read or changed here.</p>
      {#if error}<p class="err" role="alert">{error}</p>{/if}
      {#if note}<p role="status">{note}</p>{/if}
      <div class="actions">
        <button disabled={busy || !(checked?.ok ?? false)} onclick={() => void save()}>Save</button>
        <button disabled={busy || cognee?.state !== "installed" || !cognee.uiUrl} onclick={() => void launch()}>Open Cognee UI</button>
      </div>
    </div>
  </section>
</dialog>
<style>
  dialog { width: min(600px, 95vw); max-height: 85vh; box-sizing: border-box; border: 1px solid var(--line); padding: 0; border-radius: var(--radius); background: var(--panel); color: var(--text); }
  dialog::backdrop { background: rgba(4, 7, 12, .66); }
  section { display: flex; flex-direction: column; max-height: 85vh; }
  header { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 14px 16px; border-bottom: 1px solid var(--line); }
  h3, p { margin: 0; }
  .body { overflow: auto; padding: 16px; display: flex; flex-direction: column; gap: 12px; overflow-wrap: anywhere; }
  .field { display: flex; flex-direction: column; gap: 4px; font-size: 13px; }
  .pick { display: flex; gap: 6px; }
  .pick input { flex: 1; }
  .actions { display: flex; gap: 8px; }
  .dim { color: var(--text-dim); }
  .err { color: var(--err); }
</style>
