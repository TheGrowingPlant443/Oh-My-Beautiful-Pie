<script lang="ts">
  import ConfigPicker from "./ConfigPicker.svelte";
  import { api, type ConfigPreview } from "../api";
  import { instances, errorText, refreshGlobalConfig } from "../stores";
  let { open, mode, initialSourceId, onclose }: { open: boolean; mode: "save" | "import"; initialSourceId: string | null; onclose: () => void } = $props();
  let dialog = $state<HTMLDialogElement>();
  let sourceId = $state("");
  let importPath = $state("");
  let importSource = $state<"current" | "snapshot" | "native" | "custom" | "none">("none");
  let preview = $state<ConfigPreview | null>(null);
  let savedPath = $state("");
  let error = $state("");
  let busy = $state(false);
  const command = $derived(savedPath ? `omp --config '${savedPath.replaceAll("'", "''")}'` : "");
  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) {
      sourceId = initialSourceId || "";
      importPath = "";
      importSource = "none";
      preview = null;
      savedPath = "";
      error = "";
      dialog.showModal();
      dialog.querySelector<HTMLElement>("select, input, button")?.focus();
    } else if (!open && dialog.open) dialog.close();
  });
  async function load(): Promise<void> {
    busy = true;
    error = "";
    preview = null;
    savedPath = "";
    try { preview = await api.previewGlobalConfig(mode === "save" ? sourceId || null : null, mode === "import" ? importPath.trim() : null); }
    catch (e) { error = errorText(e); }
    finally { busy = false; }
  }
  async function save(): Promise<void> {
    if (!preview) return;
    busy = true;
    error = "";
    try {
      savedPath = (await api.saveGlobalConfig(preview.yaml)).path;
      await refreshGlobalConfig();
    } catch (e) { error = errorText(e); }
    finally { busy = false; }
  }
  async function activate(): Promise<void> {
    busy = true;
    error = "";
    try {
      savedPath = (await api.activateGlobalConfig(importPath)).path;
      await refreshGlobalConfig();
    } catch (e) { error = errorText(e); }
    finally { busy = false; }
  }
</script>

<dialog bind:this={dialog} class="native-dialog" aria-label={mode === "save" ? "Save as global config" : "Import config"} onclose={onclose} oncancel={e => { if (busy) e.preventDefault(); else onclose(); }}>
  <section class="panel config-dialog">
    <header><h3>{mode === "save" ? "Save as global config" : "Import config"}</h3><span class="spacer"></span><button disabled={busy} onclick={onclose}>Close</button></header>
    <div class="body">
      <p>Save non-secret native OMP settings as the default for new terminals and Manager-created workers. Existing sessions and native <code>.omp</code> config files stay unchanged.</p>
      {#if mode === "save"}
        <label>Config source
          <select bind:value={sourceId} disabled={busy} onchange={() => { preview = null; savedPath = ""; }}>
            <option value="">Native global config · default OMP profile</option>
            {#each $instances as instance (instance.id)}<option value={instance.id}>{instance.name} · {instance.profile || "default profile"}</option>{/each}
          </select>
        </label>
        <small class="dim">Reads config.yml and the source's ordered overlays. CLI model overrides and transient terminal model switches are not included.</small>
      {:else}
        <ConfigPicker mode="import" bind:value={importPath} active={open && mode === "import"} label="Config" disabled={busy} onchange={(_, s) => { importSource = s; preview = null; savedPath = ""; }} />
        {#if importSource === "current"}<p class="ok">This is already the default for new terminals.</p>{/if}
      {/if}
      <button disabled={busy || (mode === "import" && !importPath.trim())} onclick={load}>{busy ? "Working…" : "Preview config"}</button>
      {#if preview}
        <h4>Model assignments</h4>
        {#if Object.keys(preview.modelRoles).length}
          <table><thead><tr><th>Role</th><th>Native model selection</th></tr></thead><tbody>{#each Object.entries(preview.modelRoles) as [role, model]}<tr><td>{role}</td><td class="mono">{model === null ? "Cleared (native null)" : model}</td></tr>{/each}</tbody></table>
        {:else}<p class="dim">This config has no explicit model-role assignments.</p>{/if}
        <label>Sanitized YAML<textarea class="mono" readonly value={preview.yaml} rows="10"></textarea></label>
        {#if preview.omitted.length}<details open><summary>Excluded settings ({preview.omitted.length})</summary><ul>{#each preview.omitted as key}<li class="mono">{key}</li>{/each}</ul></details>{/if}
        <p class="dim">Credentials, unknown namespaces, executable modules/hooks, environment maps, credential routing, telemetry and service configuration are excluded. Model catalogs and authentication must already be available locally.</p>
        {#if !savedPath}
          {#if mode === "import" && importSource === "snapshot"}<button class="primary" disabled={busy} onclick={activate}>Use for new terminals</button>
          {:else if !(mode === "import" && importSource === "current")}<button class="primary" disabled={busy} onclick={save}>{mode === "save" ? "Save and use for new terminals" : "Import and use for new terminals"}</button>{/if}
        {/if}
      {/if}
      {#if savedPath}
        <p class="ok">Global default saved. Only new terminals will use this snapshot.</p>
        <label>Saved config path<input class="mono" readonly value={savedPath} /></label>
        <label>Use in another local terminal (PowerShell)<input class="mono" readonly value={command} /></label>
      {/if}
      {#if error}<p class="err" role="alert">{error}</p>{/if}
    </div>
  </section>
</dialog>
<style>
  .native-dialog { border: none; padding: 0; border-radius: var(--radius); background: var(--panel); color: var(--text); max-width: 96vw; max-height: 92vh; }
  .native-dialog::backdrop { background: rgba(4, 7, 12, .66); }
  .config-dialog { width: min(780px, 95vw); max-height: 90vh; }
  .body { padding: 16px; overflow: auto; display: flex; flex-direction: column; gap: 12px; }
  h3, h4, p { margin: 0; }
  label { display: flex; flex-direction: column; gap: 6px; }
  table { border-collapse: collapse; width: 100%; }
  th, td { text-align: left; padding: 6px 8px; border-bottom: 1px solid var(--line); overflow-wrap: anywhere; }
  textarea { resize: vertical; min-height: 150px; }
  ul { max-height: 120px; overflow: auto; padding-left: 24px; }
</style>
