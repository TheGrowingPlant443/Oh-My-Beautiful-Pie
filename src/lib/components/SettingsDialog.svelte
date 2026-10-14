<script lang="ts">
  import { appSettings, instances } from "../stores";
  import type { AppSettings } from "../settings";
  import ProjectPicker from "./ProjectPicker.svelte";
  let { open, onclose }: { open: boolean; onclose: () => void } = $props();
  let dialog = $state<HTMLDialogElement>();
  const startupProject = $derived($instances.find(row => row.kind === "instance" && row.id === $appSettings.startupProjectId));
  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) {
      dialog.showModal();
      dialog.querySelector<HTMLSelectElement>("select")?.focus();
    } else if (!open && dialog.open) dialog.close();
  });
  function update<K extends keyof AppSettings>(key: K, value: AppSettings[K]): void {
    appSettings.update(settings => ({ ...settings, [key]: value }));
  }
</script>

<dialog bind:this={dialog} class="native-dialog" aria-label="Settings" onclose={onclose} oncancel={onclose}>
  <section class="settings panel">
    <header><h3>Settings</h3><span class="spacer"></span><button type="button" onclick={onclose}>Close</button></header>
    <div class="body">
      <label>Open on startup
        <select value={$appSettings.startupTarget} onchange={e => update("startupTarget", e.currentTarget.value as AppSettings["startupTarget"])}>
          <option value="manager">The Manager</option><option value="project">Chosen project</option><option value="latest">Latest project</option>
          <option value="dashboard">Dashboard</option>
        </select>
      </label>
      {#if $appSettings.startupTarget === "project"}
        <ProjectPicker projects={$instances} label="Startup project" value={$appSettings.startupProjectId} onselect={id => update("startupProjectId", id)} />
        {#if startupProject}<small class="mono dim">{startupProject.projectPath}</small>
        {:else}<small class="dim">Choose a saved project. If it is unavailable on startup, The Manager will open instead.</small>{/if}
      {/if}
      <label class="check"><input type="checkbox" checked={$appSettings.autoStart} disabled={$appSettings.startupTarget === "dashboard"} onchange={e => update("autoStart", e.currentTarget.checked)} />Automatically start startup target</label>
      <small class="dim">{$appSettings.startupTarget === "dashboard" ? "Dashboard opens without starting any terminals." : "This setting controls app startup only. Opening a previous project starts its terminal independently."}</small>
      <label class="check"><input type="checkbox" checked={$appSettings.showPreviousProjects} onchange={e => update("showPreviousProjects", e.currentTarget.checked)} />Show previous projects on dashboard</label>
      <small class="dim">Hidden projects remain searchable. Projects opened or created this launch still appear.</small>
      <label class="check"><input type="checkbox" checked={$appSettings.showManagerTile} onchange={e => update("showManagerTile", e.currentTarget.checked)} />Show Manager tile</label>
      <label>Preview size
        <select value={$appSettings.previewSize} onchange={e => update("previewSize", Number(e.currentTarget.value) as AppSettings["previewSize"])}>
          <option value={420}>Compact tiles</option><option value={640}>Readable tiles</option><option value={1000}>Large previews</option>
        </select>
      </label>
      <label class="check"><input type="checkbox" checked={$appSettings.browserEmbedded} onchange={e => update("browserEmbedded", e.currentTarget.checked)} />Show browser inside app</label>
      <small class="dim">Browsers appear on the right of fullscreen terminals. Off opens the same isolated browser in an external window.</small>
      <label class="check"><input type="checkbox" checked={$appSettings.browserHeadless} onchange={e => update("browserHeadless", e.currentTarget.checked)} />Headless browser</label>
      <small class="dim">No browser window or split screen. Applies to the next browser launch; existing browsers keep their mode until closed.</small>
    </div>
  </section>
</dialog>

<style>
  .native-dialog { border: none; padding: 0; border-radius: var(--radius); background: var(--panel); color: var(--text); max-width: 95vw; max-height: 90vh; }
  .native-dialog::backdrop { background: rgba(4, 7, 12, .66); }
  .settings { width: min(600px, 95vw); max-height: 90vh; }
  .body { padding: 16px; overflow: auto; display: flex; flex-direction: column; gap: 12px; }
  label { display: flex; flex-direction: column; gap: 6px; }
  label.check { flex-direction: row; align-items: center; }
  .check input { width: auto; }
  h3 { margin: 0; }
</style>
