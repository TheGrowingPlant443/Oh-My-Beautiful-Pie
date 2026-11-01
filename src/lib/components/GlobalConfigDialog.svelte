<script lang="ts">
  import { open as pickFolder } from "@tauri-apps/plugin-dialog";
  import ConfigPicker from "./ConfigPicker.svelte";
  import { api, type ConfigPreview, type SavedConfigs } from "../api";
  import { allInstances, errorText, refreshGlobalConfig, refreshInstances } from "../stores";
  let { open, initialSourceId, onclose }: { open: boolean; initialSourceId: string | null; onclose: () => void } = $props();
  let dialog = $state<HTMLDialogElement>();
  let mode = $state<"save" | "import">("save");
  let sourceId = $state("");
  let sourcePath = $state("");
  let importPath = $state("");
  let preview = $state<ConfigPreview | null>(null);
  let name = $state("");
  let approveExecution = $state(false);
  let savedPath = $state("");
  let error = $state("");
  let notice = $state("");
  let busy = $state(false);
  let data = $state<SavedConfigs | null>(null);
  let assignments = $state<{model: string; configPath: string}[]>([]);
  let models = $state<{selector: string; name: string; available: boolean | null}[]>([]);
  let modelProfile = $state("");
  const modelChoices = $derived.by(() => {
    const choices = new Map(models.map(model => [model.selector, model]));
    for (const instance of $allInstances) {
      if (instance.status !== "running" || !instance.bridgeReady || !instance.configState) continue;
      for (const selector of [instance.configState.model, ...(instance.configState.children ?? []).map(child => child.model)]) {
        if (selector && !choices.has(selector)) choices.set(selector, {selector, name: selector, available: null});
      }
    }
    return [...choices.values()];
  });
  let generation = 0;
  let modelRequest = 0;
  let assetPage = $state(0);
  const assetsPerPage = 25;
  function invalidate(): void { generation++; preview = null; savedPath = ""; approveExecution = false; notice = ""; assetPage = 0; }
  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) {
      invalidate(); sourceId = initialSourceId || ""; sourcePath = ""; importPath = ""; mode = "save"; error = "";
      dialog.showModal(); void reload();
    } else if (!open && dialog.open) { invalidate(); dialog.close(); }
  });
  $effect(() => { if (open) void loadModels(sourceId || null); });
  async function loadModels(id: string | null): Promise<void> {
    const version = ++modelRequest;
    models = []; modelProfile = "";
    try { const result = await api.listConfigModels(id); if (version !== modelRequest || !open) return; models = result.models; modelProfile = result.profile; if (result.error) error = result.error; }
    catch (e) { if (version === modelRequest && open) error = errorText(e); }
  }
  async function reload(): Promise<void> {
    const version = generation;
    try { const result = await api.listSavedConfigs(); if (version !== generation || !open) return; data = result; assignments = result.assignments.map(row => ({...row})); if (result.error) error = result.error; }
    catch (e) { if (version === generation && open) error = errorText(e); }
  }
  async function load(): Promise<void> {
    invalidate(); const version = generation; busy = true; error = "";
    try {
      const result = await api.previewGlobalConfig(mode === "save" && !sourcePath ? sourceId || null : null, mode === "import" ? importPath : sourcePath || null);
      if (version === generation && open) { preview = result; name = result.name || ""; }
    } catch (e) { if (version === generation && open) error = errorText(e); }
    finally { busy = false; }
  }
  async function save(activate: boolean): Promise<void> {
    if (!preview) return;
    busy = true; error = "";
    try {
      savedPath = (await api.saveGlobalConfig(preview, name.trim(), activate, approveExecution)).path;
      notice = activate ? "Setup saved and selected as the global default. Existing sessions are unchanged." : "Setup saved. The global default and existing sessions are unchanged.";
      await Promise.all([refreshGlobalConfig(), refreshInstances(), reload()]);
    } catch (e) { error = errorText(e); }
    finally { busy = false; }
  }
  async function persistAssignments(): Promise<void> {
    busy = true; error = "";
    try { await api.saveModelConfigAssignments(assignments); await Promise.all([refreshGlobalConfig(), refreshInstances(), reload()]); notice = "Model/setup assignments saved."; }
    catch (e) { error = errorText(e); }
    finally { busy = false; }
  }
  async function activateSaved(): Promise<void> {
    if (!sourcePath || !preview) return;
    busy = true; error = "";
    try {
      await api.activateGlobalConfig(sourcePath);
      await Promise.all([refreshGlobalConfig(), refreshInstances(), reload()]);
      notice = "Global default changed. Only new sessions use this setup; existing sessions are unchanged.";
    } catch (e) { error = errorText(e); }
    finally { busy = false; }
  }
  async function exportBundle(path = savedPath): Promise<void> {
    if (!path) return;
    busy = true; error = "";
    try {
      const parent = await pickFolder({directory: true, multiple: false, title: "Choose parent folder for a new exported bundle"});
      if (typeof parent === "string") {
        const setupName = data?.snapshots.find(setup => setup.path === path)?.name || name || "setup";
        const folder = `${setupName.replace(/[^a-zA-Z0-9_-]+/g, "-").replace(/^-+|-+$/g, "") || "setup"}-${crypto.randomUUID()}`;
        const destination = `${parent.replace(/[\\/]$/, "")}/${folder}`;
        await api.exportConfig(path, destination);
        notice = `Setup bundle exported to ${destination}`;
      }
    }
    catch (e) { error = errorText(e); }
    finally { busy = false; }
  }
</script>

<dialog bind:this={dialog} class="native-dialog" aria-label="Configuration setups" onclose={onclose} oncancel={e => { if (busy) e.preventDefault(); else onclose(); }}>
  <section class="panel config-dialog">
    <header><h3>Configuration setups</h3><span class="spacer"></span><button disabled={busy} onclick={onclose}>Close</button></header>
    <div class="body">
      <p>Named app-owned snapshots keep settings and bundled skills/rules separate from authentication. Native configuration is never overwritten.</p>
      <label>Action<select bind:value={mode} disabled={busy} onchange={invalidate}><option value="save">Save setup</option><option value="import">Import bundle</option></select></label>
      {#if mode === "save"}
        <label>Settings source<select bind:value={sourceId} disabled={busy || !!sourcePath} onchange={invalidate}><option value="">Native global settings</option>{#each $allInstances as instance (instance.id)}<option value={instance.id}>{instance.name}</option>{/each}</select></label>
        {#if sourceId && !sourcePath && !$allInstances.find(instance => instance.id === sourceId)?.configState}<p class="dim">Reconstructed from saved settings, model selection and setup assignments; this is not an active runtime snapshot.</p>{/if}
        <label>Named setup (source or global default)<select bind:value={sourcePath} disabled={busy} onchange={invalidate}><option value="">Use settings source above</option>{#each data?.snapshots ?? [] as setup (setup.path)}<option value={setup.path}>{setup.name || "Saved setup"} · {new Date(setup.modifiedMs).toLocaleString()}{setup.path === data?.current ? " · global default" : ""}</option>{/each}</select></label>
        <button disabled={busy || !sourcePath} onclick={() => exportBundle(sourcePath)}>Export selected setup to folder…</button>
      {:else}
        <ConfigPicker mode="import" bind:value={importPath} active={open && mode === "import"} label="Setup bundle or saved setup" disabled={busy} onchange={invalidate} />
      {/if}
      <button disabled={busy || (mode === "import" && !importPath)} onclick={load}>Preview sanitized bundle</button>
      {#if preview}
        <label>Setup name<input bind:value={name} disabled={busy || !!savedPath} required /></label>
        <h4>Model roles</h4>
        <p class="dim">These are declared setup roles for new sessions/subagents, not evidence of a live connection. The default role chooses the initial model; changing setup does not replace the current model.</p>
        {#each Object.entries(preview.modelRoles) as [role, model]}<p><strong>{role}:</strong> {model === null ? "Native default" : model}</p>{/each}
        <label>Sanitized settings<textarea class="mono" readonly value={preview.yaml} rows="10"></textarea></label>
        <h4>Bundled assets ({preview.assets.length})</h4>
        {#each preview.assets.slice(assetPage * assetsPerPage, (assetPage + 1) * assetsPerPage) as asset (asset.path)}
          <details><summary>{asset.path}</summary>
            {#if asset.file}
              <p>File-backed asset · {asset.file.size.toLocaleString()} bytes</p>
              <label>Review source file<input class="mono" readonly value={asset.file.sourcePath} /></label>
              <label>SHA-256<input class="mono" readonly value={asset.file.sha256} /></label>
              <p class="dim">Content is not displayed here. Review this source file externally before approving its inclusion.</p>
            {:else}<label>Bundled content<textarea class="mono" readonly value={asset.content} rows="8"></textarea></label>{/if}
          </details>
        {/each}
        {#if preview.assets.length > assetsPerPage}<nav class="actions" aria-label="Bundled asset pages"><button disabled={assetPage === 0} onclick={() => assetPage--}>Previous assets</button><span role="status">Page {assetPage + 1} of {Math.ceil(preview.assets.length / assetsPerPage)} · all {preview.assets.length} assets will be saved</span><button disabled={(assetPage + 1) * assetsPerPage >= preview.assets.length} onclick={() => assetPage++}>Next assets</button></nav>{/if}
        {#if preview.omitted.length}<details open><summary>Excluded settings ({preview.omitted.length})</summary><ul>{#each preview.omitted as key}<li class="mono">{key}</li>{/each}</ul></details>{/if}
        <p class="dim">Review every asset, including MCP commands and skill instructions. A saved setup is not a login or a connected session.</p>
        {#if preview.requiresExecutionReview}<label class="check"><input type="checkbox" bind:checked={approveExecution} disabled={busy || !!savedPath} />I reviewed all assets, including file-backed source files and MCP/executable content, and explicitly approve their use when this setup is selected.</label>{/if}
        {#if mode === "save" && sourcePath}<button disabled={busy || data?.current === sourcePath} onclick={activateSaved}>Use reviewed saved setup as global default</button><small class="dim">Activation reuses this immutable snapshot, affects new sessions only, and requires its stored execution approval. To approve unreviewed executable content, save a reviewed copy below.</small>{/if}
        {#if !savedPath}<div class="actions"><button disabled={busy || !name.trim() || (preview.requiresExecutionReview && !approveExecution)} onclick={() => save(false)}>Save only</button><button class="primary" disabled={busy || !name.trim() || (preview.requiresExecutionReview && !approveExecution)} onclick={() => save(true)}>Save and activate global default</button></div>{/if}
      {/if}
      {#if savedPath}<label>Saved snapshot<input class="mono" readonly value={savedPath} /></label><button disabled={busy} onclick={() => exportBundle()}>Export bundle to folder…</button>{/if}
      <h4>Per-model setups</h4>
      <p class="dim">Credential availability is not session connectivity. Automatic selection uses the resolved model assignment, then the global default.</p>
      {#each assignments as row, index}
        <div class="assignment">
          <label>Model<select bind:value={row.model} disabled={busy}><option value="" disabled>Select model…</option>{#if row.model && !modelChoices.some(model => model.selector === row.model)}<option value={row.model}>{row.model} · not in catalog · credential availability not checked in queried profile</option>{/if}{#each modelChoices as model (model.selector)}<option value={model.selector}>{model.name} · {model.selector} · {model.available === null ? "credential availability not checked in queried profile" : `cached credentials ${model.available ? "available" : "unavailable"} in profile ${modelProfile}`}</option>{/each}</select></label>
          <label>Setup<select bind:value={row.configPath} disabled={busy}><option value="" disabled>Select setup…</option>{#if row.configPath && !data?.snapshots.some(setup => setup.path === row.configPath)}<option value={row.configPath}>{row.configPath} · missing snapshot</option>{/if}{#each data?.snapshots ?? [] as setup (setup.path)}<option value={setup.path}>{setup.name || "Saved setup"} · {new Date(setup.modifiedMs).toLocaleString()}</option>{/each}</select></label>
          <button disabled={busy} aria-label={`Remove model assignment ${index + 1}`} onclick={() => assignments = assignments.filter((_, i) => i !== index)}>Remove</button>
        </div>
      {/each}
      <div class="actions"><button disabled={busy} onclick={() => assignments = [...assignments, {model: "", configPath: ""}]}>+ Add model/setup</button><button disabled={busy || assignments.some(row => !row.model || !row.configPath) || new Set(assignments.map(row => row.model)).size !== assignments.length} onclick={persistAssignments}>Save assignments</button></div>
      <h4>Session relationships</h4>
      <p class="dim">Built-in native tasks retain their parent's startup skills, rules, context and MCP resources. Incompatible child-only resource changes are refused before partial application: change the parent's setup or use an independent managed session. Restarting the child does not change this inheritance. Owned SRS workers use their separate, explicitly restricted constructor setup. Child nodes show only native-reported effective setup state, not proof that every selected asset was independently loaded.</p>
      <div class="relationships" role="group" aria-label="Native-reported live session, model and setup connections">
        {#each $allInstances as instance (instance.id)}
          {#if instance.status === "running" && instance.bridgeReady && instance.configState?.model}
            <section class="connection" aria-label={`${instance.name} live configuration relationship`}>
              <div class="connection-row">
                <div class="node"><small>Connected session</small><strong>{instance.name}</strong></div>
                <span class="edge" aria-label="uses model">→</span>
                <div class="node"><small>Actual model</small><span class="mono">{instance.configState.model}</span></div>
                <span class="edge" aria-label="uses setup">→</span>
                <div class="node"><small>Applied setup · {instance.configState.source}</small><strong>{instance.configState.configName || instance.configState.configPath || "Native defaults"}</strong></div>
              </div>
              {#if instance.configState.error}<p class="err" role="status">{instance.configState.error}</p>{/if}
              {#if instance.configState.configFiles?.length}<details><summary>Ordered settings overlays</summary><ol>{#each instance.configState.configFiles as path}<li class="mono">{path}</li>{/each}</ol>{#if instance.configState.setupPaths?.length}<p>Contributing setup assets</p><ul>{#each instance.configState.setupPaths as path}<li class="mono">{path}</li>{/each}</ul>{/if}</details>{/if}
              {#each instance.configState.children ?? [] as child (child.id)}
                {#if child.model}
                  <div class="child-connection" role="group" aria-label={`${instance.name} native child ${child.id}`}>
                    <p class="parent-edge"><span aria-hidden="true">└─→</span> Native child of {instance.name}</p>
                    <div class="connection-row">
                      <div class="node"><small>Active native child</small><strong>{child.id}</strong></div>
                      <span class="edge" aria-label="uses model">→</span>
                      <div class="node"><small>Actual model</small><span class="mono">{child.model}</span></div>
                      <span class="edge" aria-label="uses setup">→</span>
                      <div class="node"><small>Reported effective setup · {child.source}</small><strong>{child.configName || child.configPath || "Native defaults"}</strong></div>
                    </div>
                    {#if child.mcpInheritedFrom}<p class="dim">MCP inherited from {child.mcpInheritedFrom}</p>{/if}
                    {#if child.error}<p class="err" role="status">{child.error}</p>{/if}
                  </div>
                {:else}<p class="dim">Native child {child.id}: no actual model reported.{#if child.error}<span class="err"> {child.error}</span>{/if}</p>{/if}
              {/each}
            </section>
          {:else}
            <p class="dim"><strong>{instance.name}</strong> · {instance.status}{instance.status === "running" && !instance.bridgeReady ? " · bridge not ready" : ""} · no live model/setup connection reported{#if instance.configState?.error}<span class="err"> · {instance.configState.error}</span>{/if}</p>
          {/if}
        {/each}
      </div>
      {#if notice}<p class="ok" role="status">{notice}</p>{/if}
      {#if error}<p class="err" role="alert">{error}</p>{/if}
    </div>
  </section>
</dialog>
<style>
  .native-dialog { border: none; padding: 0; border-radius: var(--radius); background: var(--panel); color: var(--text); max-width: 96vw; max-height: 92vh; }
  .native-dialog::backdrop { background: rgba(4, 7, 12, .66); }
  .config-dialog { width: min(900px, 95vw); max-height: 90vh; }
  .body { padding: 16px; overflow: auto; display: flex; flex-direction: column; gap: 12px; }
  h3, h4, p { margin: 0; }
  label { display: flex; flex-direction: column; gap: 6px; min-width: 0; }
  .assignment, .actions { display: flex; gap: 8px; flex-wrap: wrap; align-items: end; }
  .assignment label { flex: 1; }
  .check { flex-direction: row; align-items: start; }
  textarea { resize: vertical; width: 100%; }
  .relationships { display: flex; flex-direction: column; gap: 12px; }
  .connection { border: 1px solid var(--line); border-radius: var(--radius); padding: 12px; display: flex; flex-direction: column; gap: 10px; }
  .connection-row { display: grid; grid-template-columns: minmax(0, 1fr) auto minmax(0, 1fr) auto minmax(0, 1fr); gap: 8px; align-items: center; }
  .node { border: 1px solid var(--line); border-radius: var(--radius); padding: 10px; display: flex; flex-direction: column; gap: 5px; overflow-wrap: anywhere; }
  .node small, .parent-edge { color: var(--dim); }
  .edge { font-size: 1.4em; }
  .child-connection { margin-left: 14px; border-left: 2px solid var(--line); padding-left: 12px; display: flex; flex-direction: column; gap: 8px; }
  @media (max-width: 600px) { .connection-row { grid-template-columns: minmax(0, 1fr); } .edge { justify-self: center; transform: rotate(90deg); } }
</style>
