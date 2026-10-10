<script lang="ts">
  import { api, type SessionInfo } from "../api";
  import { errorText, refreshInstances, startInstance } from "../stores";
  import { basename, pickFolder } from "../ui";
  let { open, onclose, oncreated }: { open: boolean; onclose: () => void; oncreated: (id: string) => void } = $props();
  let projectPath = $state("");
  let name = $state("");
  let profile = $state("");
  let model = $state("");
  let configs = $state<string[]>([]);
  let resume = $state("");
  let startNow = $state(true);
  let sessions = $state<SessionInfo[]>([]);
  let loading = $state(false);
  let busy = $state(false);
  let error = $state("");
  let request = 0;

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
      const created = await api.createInstance({ name: name.trim() || basename(projectPath), projectPath: projectPath.trim(), profile: profile.trim(), model: model.trim(), configs: configs.map(c => c.trim()).filter(Boolean), resumeSessionId: resume || null });
      if (startNow) await startInstance(created.id);
      await refreshInstances();
      oncreated(created.id);
      onclose();
    } catch (e) { error = errorText(e); }
    finally { busy = false; }
  }
</script>

{#if open}
  <div class="backdrop" role="presentation" onclick={e => { if (e.target === e.currentTarget && !busy) onclose(); }}>
    <form class="dialog panel" onsubmit={submit}>
      <header><h3>New OMP instance</h3><span class="spacer"></span><button type="button" onclick={onclose} disabled={busy}>Close</button></header>
      <div class="body form">
        <label>Working directory <div class="row"><input bind:value={projectPath} required placeholder="C:/path/to/project" /><button type="button" onclick={browse}>Browse…</button></div></label>
        <label>Name <input bind:value={name} placeholder={basename(projectPath) || "Project name"} /></label>
        <label>OMP profile <input bind:value={profile} placeholder="Default profile" /></label>
        <label>Model / role override <input bind:value={model} placeholder="Use OMP configuration" /></label>
        <div class="config-list">
          <span>Config overlays (applied in order)</span>
          {#each configs as _, index}
            <div class="row"><input aria-label={`Config overlay ${index + 1}`} bind:value={configs[index]} placeholder="Path to config.yml" /><button type="button" onclick={() => configs = configs.filter((_, i) => i !== index)}>Remove</button></div>
          {/each}
          <button type="button" onclick={() => configs = [...configs, ""]}>Add config overlay</button>
        </div>
        <label>Reconnect to an existing session
          <div class="row"><select bind:value={resume} disabled={loading}>
            <option value="">New session</option>
            {#each sessions as session (session.path)}<option value={session.path}>{session.title} · {new Date(session.modifiedMs).toLocaleString()}</option>{/each}
          </select><button type="button" disabled={!projectPath || loading} onclick={() => loadSessions(projectPath, profile)}>Refresh</button></div>
          <small>{loading ? "Reading native OMP sessions…" : `${sessions.length} saved sessions for this project/profile`}</small>
        </label>
        <label class="row"><input type="checkbox" bind:checked={startNow} /> Start immediately</label>
        <p class="dim">Thinking visibility, tools, approvals, and agent behavior remain controlled by OMP.</p>
        {#if error}<p role="alert" class="err">{error}</p>{/if}
      </div>
      <footer><button type="submit" class="primary" disabled={busy || !projectPath.trim()}>{busy ? "Creating…" : "Create instance"}</button></footer>
    </form>
  </div>
{/if}
<style>
  .dialog { width: min(650px, 95vw); max-height: 90vh; }
  .form { display: flex; flex-direction: column; gap: 12px; padding: 16px; }
  label, .config-list { display: flex; flex-direction: column; gap: 6px; }
  label.row { flex-direction: row; }
  label.row input { width: auto; }
  .row input, .row select { flex: 1; min-width: 0; }
  h3, p { margin: 0; }
  footer { padding: 12px 16px; border-top: 1px solid var(--line); }
</style>
