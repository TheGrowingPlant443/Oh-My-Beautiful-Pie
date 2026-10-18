<script lang="ts">
  import { untrack } from "svelte";
  import { api, type AgentFiles, type OmpProfiles, type SessionInfo } from "../api";
  import { errorText, refreshInstances, startInstance, globalConfig, refreshGlobalConfig } from "../stores";
  import { basename, pickFolder } from "../ui";
  import ConfigPicker from "./ConfigPicker.svelte";
  let { open, onclose, oncreated, purpose = "instance" }: { open: boolean; onclose: () => void; oncreated: (id: string) => void; purpose?: "instance" | "manager" | "architect" } = $props();
  let projectPath = $state("");
  let name = $state("");
  let profile = $state("");
  let advanced = $state(false);
  let profiles = $state<OmpProfiles>({ profiles: [], error: null });
  let persona = $state("");
  let agentFiles = $state<AgentFiles>({ files: [], error: null });
  let extraConfig = $state("");
  const selectedProfile = $derived(profiles.profiles.find(p => p.name === profile));
  let configs = $state<string[]>([]);
  let resume = $state("");
  let startNow = $state(true);
  let useGlobalConfig = $state(true);
  let sessions = $state<SessionInfo[]>([]);
  let loading = $state(false);
  let busy = $state(false);
  let error = $state("");
  let request = 0;
  let dialog = $state<HTMLDialogElement>();
  let previousPurpose = "instance";
  $effect(() => {
    if (purpose === previousPurpose) return;
    previousPurpose = purpose;
    name = purpose === "manager" ? "The Manager" : purpose === "architect" ? "The Architect" : "";
  });

  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) {
      advanced = false;
      profile = "";
      persona = "";
      extraConfig = "";
      profiles = { profiles: [], error: null };
      agentFiles = { files: [], error: null };
      dialog.showModal();
      void api.listOmpProfiles().then(found => { profiles = found; }).catch(e => { profiles = { profiles: [], error: errorText(e) }; });
      void api.listAgentFiles(purpose).then(found => { agentFiles = found; }).catch(e => { agentFiles = { files: [], error: errorText(e) }; });
      void refreshGlobalConfig().then(() => { useGlobalConfig = !!$globalConfig.path || !!$globalConfig.error; advanced = !!$globalConfig.error; });
      dialog.querySelector<HTMLInputElement>("input")?.focus();
    } else if (!open && dialog.open) {
      dialog.close();
    }
  });
  $effect(() => {
    const named = profile !== "";
    useGlobalConfig = !named && untrack(() => !!$globalConfig.path || !!$globalConfig.error);
  });

  async function loadSessions(path: string, selectedProfile: string): Promise<void> {
    const token = ++request;
    loading = true;
    try {
      const found = await api.listSessions(path, selectedProfile);
      if (token !== request) return;
      sessions = found;
      if (!found.some(s => s.path === resume)) resume = "";
    } catch (e) { if (token === request) error = errorText(e); }
    finally { if (token === request) loading = false; }
  }
  $effect(() => {
    if (!open) return;
    const path = projectPath.trim();
    const selectedProfile = profile.trim();
    sessions = [];
    resume = "";
    request++;
    if (!path) return;
    const timer = setTimeout(() => void loadSessions(path, selectedProfile), 300);
    return () => clearTimeout(timer);
  });
  async function browse(): Promise<void> {
    try {
      const path = await pickFolder();
      if (path) { projectPath = path; if (!name) name = basename(path); }
    } catch (e) { error = errorText(e); }
  }
  async function submit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    busy = true;
    error = "";
    try {
      const config = { name: name.trim() || basename(projectPath), projectPath: projectPath.trim(), profile, model: "", configs: [extraConfig, ...configs.map(c => c.trim())].filter(Boolean), resumeSessionId: resume || null, useGlobalConfig, agentFile: persona || null };
      const created = purpose === "manager" ? await api.createManager(config) : purpose === "architect" ? await api.createArchitect(config) : await api.createInstance(config);
      if (purpose === "instance" && startNow) await startInstance(created.id, !!resume);
      await refreshInstances();
      oncreated(created.id);
      onclose();
    } catch (e) { error = errorText(e); }
    finally { busy = false; }
  }
</script>

<dialog bind:this={dialog} class="native-dialog" aria-label={purpose === "manager" ? "Set up The Manager" : purpose === "architect" ? "Set up The Architect" : "New OMP instance"} onclose={onclose} oncancel={onclose} onclick={e => { if (e.target === e.currentTarget && !busy) onclose(); }}>
    <form class="dialog panel" onsubmit={submit}>
      <header><h3>{purpose === "manager" ? "Set up The Manager" : purpose === "architect" ? "Set up The Architect" : "New OMP instance"}</h3><span class="spacer"></span><button type="button" onclick={onclose} disabled={busy}>Close</button></header>
      <div class="body form">
        <label>Working directory <div class="row"><input bind:value={projectPath} required placeholder="C:/path/to/project" /><button type="button" onclick={browse}>Browse…</button></div></label>
        {#if sessions.length > 0}
          <label>Reconnect to an existing session
            <div class="row"><select bind:value={resume} disabled={loading}>
              <option value="">New session</option>
              {#each sessions as session (session.path)}<option value={session.path}>{session.title} · {new Date(session.modifiedMs).toLocaleString()}</option>{/each}
            </select><button type="button" disabled={!projectPath || loading} onclick={() => loadSessions(projectPath, profile)}>Refresh</button></div>
            <small>{loading ? "Reading native OMP sessions…" : `${sessions.length} saved sessions for this project/profile`}</small>
          </label>
        {/if}
        {#if purpose === "instance"}<label class="row"><input type="checkbox" bind:checked={startNow} /> Start immediately</label>{/if}
        <button type="button" aria-expanded={advanced} onclick={() => advanced = !advanced}>{advanced ? "Hide advanced options" : "Show advanced options"}</button>
        {#if advanced}
          <label>Name <input bind:value={name} disabled={purpose !== "instance"} placeholder={basename(projectPath) || "Project name"} /></label>
          <label>OMP profile
            <select bind:value={profile}>
              <option value="">Default profile</option>
              {#each profiles.profiles as p (p.name)}<option value={p.name}>{p.name}{p.hasLogin ? "" : " · no saved login"}</option>{/each}
            </select>
          </label>
          {#if profiles.error}<small class="dim">{profiles.error}</small>{/if}
          {#if selectedProfile && !selectedProfile.hasLogin}<small class="warn">This profile has no saved login. Run /login inside its terminal.</small>{/if}
          <label>Persona
            <select bind:value={persona}>
              <option value="">No persona</option>
              {#each agentFiles.files as file (file.path)}<option value={file.path}>{file.name} · {file.scope === "global" ? "Global" : file.scope === "architect" ? "The Architect" : "The Manager"}</option>{/each}
            </select>
          </label>
          {#if agentFiles.error}<small class="dim">{agentFiles.error}</small>{/if}
          <ConfigPicker mode="overlay" bind:value={extraConfig} active={open} label="Model / role override" />
          <div class="config-list">
            <span>Config overlays (applied in order)</span>
            {#each configs as _, index}
              <div class="row"><input aria-label={`Config overlay ${index + 1}`} bind:value={configs[index]} placeholder="Path to config.yml" /><button type="button" onclick={() => configs = configs.filter((_, i) => i !== index)}>Remove</button></div>
            {/each}
            <button type="button" onclick={() => configs = [...configs, ""]}>Add config overlay</button>
          </div>
          {#if $globalConfig.path || $globalConfig.error}
            <label class="row"><input type="checkbox" bind:checked={useGlobalConfig} />Use global config</label>
            {#if profile}<small class="dim">Off by default for named profiles: the global config would override their own model roles and plan-mode settings.</small>{/if}
            {#if $globalConfig.path}<small class="mono">{$globalConfig.path}</small>{/if}
            {#if $globalConfig.error}<p class="err">{$globalConfig.error} Uncheck to use native OMP configuration.</p>{/if}
          {/if}
        {/if}
        <p class="dim">Thinking visibility, tools, approvals, and agent behavior remain controlled by OMP.</p>
        {#if purpose === "manager"}<p class="dim">A bundled OMP extension connects through authenticated, app-lifetime localhost IPC. Only created or handed-over sessions are controllable.</p>{/if}
        {#if purpose === "architect"}<p class="dim">The Architect is an ordinary agent that you ask about a project's architecture. Its Dependency tree button analyses a folder you choose.</p>{/if}
        {#if error}<p role="alert" class="err">{error}</p>{/if}
      </div>
      <footer><button type="submit" class="primary" disabled={busy || !projectPath.trim()}>{busy ? "Creating…" : purpose === "manager" ? "Create The Manager" : purpose === "architect" ? "Create The Architect" : "Create instance"}</button></footer>
    </form>
</dialog>
<style>
  .native-dialog { border: none; padding: 0; border-radius: var(--radius); background: var(--panel); color: var(--text); max-width: 95vw; max-height: 90vh; }
  .native-dialog::backdrop { background: rgba(4, 7, 12, .66); }
  .dialog { width: min(650px, 95vw); max-height: 90vh; }
  .form { display: flex; flex-direction: column; gap: 12px; padding: 16px; }
  label, .config-list { display: flex; flex-direction: column; gap: 6px; }
  label.row { flex-direction: row; }
  label.row input { width: auto; }
  .row input, .row select { flex: 1; min-width: 0; }
  h3, p { margin: 0; }
  footer { padding: 12px 16px; border-top: 1px solid var(--line); }
</style>
