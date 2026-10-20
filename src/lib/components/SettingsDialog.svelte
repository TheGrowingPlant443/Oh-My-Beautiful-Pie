<script lang="ts">
  import { untrack } from "svelte";
  import { appSettings, instances, knownStandardVersion } from "../stores";
  import type { AppSettings, ProjectLanguage, ProjectRole } from "../settings";
  import type { KnownStandard } from "../dependency-graph";
  import { api } from "../api";
  import { languageLabel } from "../project-filter";
  import { confirmDanger } from "../ui";
  import ProjectPicker from "./ProjectPicker.svelte";
  import CodeQualitySettings from "./CodeQualitySettings.svelte";
  import { open as pickFile } from "@tauri-apps/plugin-dialog";
  import { get } from "svelte/store";
  import { knowledge, knowledgeApi } from "../knowledge";
  let integrationError = $state<string | null>(null);
  let integrationBusy = $state(false);
  async function locateObsidian(): Promise<void> {
    integrationError = null;
    try {
      const selected = await pickFile({ directory: false, multiple: false, title: "Select your installed Obsidian application" });
      if (typeof selected !== "string") return;
      integrationBusy = true;
      await knowledgeApi.configureObsidian(selected);
      await knowledge.probe(get(knowledge).folder);
    } catch (error) { integrationError = String(error); }
    finally { integrationBusy = false; }
  }
  let { open, onclose }: { open: boolean; onclose: () => void } = $props();
  let dialog = $state<HTMLDialogElement>();
  const startupProject = $derived($instances.find(row => row.kind === "instance" && row.id === $appSettings.startupProjectId));
  const languages: ProjectLanguage[] = ["maven", "npm", "pypi", "cargo"];
  const roles: ProjectRole[] = ["backend", "frontend"];
  let known = $state<KnownStandard | null>(null);
  let knownError = $state("");
  let knownMessage = $state("");
  let knownLanguage = $state("all");
  let clearing = $state(false);
  let knownRequest = 0;
  // A language's facts are the packages the Architect judged standard plus transitive packages derived from them.
  const knownTotals = $derived<Record<string, number>>(Object.fromEntries(languages.map(language => [language, (known?.counts[language] ?? 0) + (known?.derivedCounts[language] ?? 0)])));
  const knownCount = $derived(knownLanguage === "all" ? Object.values(knownTotals).reduce((sum, count) => sum + count, 0) : knownTotals[knownLanguage] ?? 0);
  async function loadKnown(): Promise<void> {
    const request = ++knownRequest;
    known = null;
    knownError = "";
    try {
      const result = await api.getKnownStandard();
      if (request === knownRequest) known = result;
    } catch (error) {
      if (request === knownRequest) knownError = String(error);
    }
  }
  $effect(() => {
    if (open) untrack(() => { knownMessage = ""; void loadKnown(); });
    else untrack(() => { knownRequest++; });
  });
  async function clearKnown(): Promise<void> {
    const value = knownLanguage, count = knownCount;
    const label = value === "all" ? "all languages" : languageLabel(value as ProjectLanguage);
    if (!await confirmDanger(`Forget ${count} known standard package(s) for ${label}, including derived transitive packages? The Architect will be asked about them again the next time you classify, and built-in transitive packages are derived again from scratch.`)) return;
    clearing = true;
    knownError = "";
    try {
      const removed = await api.clearKnownStandard(value === "all" ? null : value);
      knownMessage = `Cleared ${removed} package(s).`;
      knownStandardVersion.update(n => n + 1);
      await loadKnown();
    } catch (error) { knownError = String(error); }
    finally { clearing = false; }
  }
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
  function toggle<K extends "filterLanguages" | "filterRoles">(key: K, value: AppSettings[K][number]): void {
    appSettings.update(settings => {
      const values = settings[key] as string[];
      return { ...settings, [key]: values.includes(value) ? values.filter(item => item !== value) : [...values, value] };
    });
  }
</script>

<dialog bind:this={dialog} class="native-dialog" aria-label="Settings" onclose={onclose} oncancel={onclose}>
  <section class="settings panel">
    <header><h3>Settings</h3><span class="spacer"></span><button type="button" onclick={onclose}>Close</button></header>
    <div class="body">
      <label>Open on startup
        <select value={$appSettings.startupTarget} onchange={e => update("startupTarget", e.currentTarget.value as AppSettings["startupTarget"])}>
          <option value="manager">The Manager</option><option value="architect">The Architect</option><option value="project">Chosen project</option><option value="latest">Latest project</option>
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
      <label class="check"><input type="checkbox" checked={$appSettings.alwaysNewSession} onchange={e => update("alwaysNewSession", e.currentTarget.checked)} />Always start a new session</label>
      <small class="dim">Off (default): starting an agent resumes its previous session. On: Start / reopen, opening a stopped agent and startup auto-start begin a fresh session; the old one stays in OMP's session list (Reconnect). Restart always keeps the session.</small>
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
      <label class="check"><input type="checkbox" checked={$appSettings.terminalAutoOpen} onchange={e => update("terminalAutoOpen", e.currentTarget.checked)} />Automatically show terminal view for AI commands</label>
      <small class="dim">Off by default. When on, the open session's Terminal pane appears when the AI starts a command. It never switches sessions or pages.</small>
      <label class="check"><input type="checkbox" checked={$appSettings.terminalCreateUser} onchange={e => update("terminalCreateUser", e.currentTarget.checked)} />Create a user terminal (Tab 0) when opening Terminal</label>
      <small class="dim">Applies to the next time you open Terminal. An existing user terminal keeps running until you close its tab.</small>
      <label class="check"><input type="checkbox" checked={$appSettings.packageLookupGate} onchange={e => update("packageLookupGate", e.currentTarget.checked)} />Block package lookups until The Architect has identified in-house packages</label>
      <small class="dim">Off by default. When on, the Dependency tree shows nothing but a button until The Architect has judged which packages are in-house (company-internal, confidential or not worth looking up). In-house packages are never looked up on public registries or GitHub and are not sent for security review. The Architect sees the package names.</small>
      <h3>Dependency tree filters</h3>
      <label class="check"><input type="checkbox" checked={$appSettings.manifestFilter} onchange={e => update("manifestFilter", e.currentTarget.checked)} />Skip projects by language (detected from the manifests)</label>
      {#if $appSettings.manifestFilter}
        {#each languages as language}
          <label class="check"><input type="checkbox" checked={$appSettings.filterLanguages.includes(language)} onchange={() => toggle("filterLanguages", language)} />{languageLabel(language)}</label>
        {/each}
      {/if}
      <label class="check"><input type="checkbox" checked={$appSettings.aiFilter} onchange={e => update("aiFilter", e.currentTarget.checked)} />Ask The Architect whether the project is backend or frontend</label>
      {#if $appSettings.aiFilter}
        {#each roles as role}
          <label class="check"><input type="checkbox" checked={$appSettings.filterRoles.includes(role)} onchange={() => toggle("filterRoles", role)} />Skip {role} projects</label>
        {/each}
      {/if}
      <small class="dim">Off by default. A skipped project shows a panel instead of the tree, with a button to build it anyway. The language filter reads local manifests only. The Architect filter sends one question per project to The Architect, which reads the project's files.</small>
      <h3>Known standard packages</h3>
      <select aria-label="Known standard packages language" bind:value={knownLanguage}>
        <option value="all">All languages ({Object.values(knownTotals).reduce((sum, count) => sum + count, 0)})</option>
        {#each ["npm", "pypi", "maven", "cargo"] as language}
          <option value={language}>{languageLabel(language as ProjectLanguage)} ({knownTotals[language] ?? 0})</option>
        {/each}
      </select>
      <button type="button" disabled={!known || knownCount === 0 || clearing} onclick={clearKnown}>Clear known standard packages</button>
      {#if knownError}<small class="dim">{knownError}</small>{/if}
      {#if knownMessage}<small class="dim">{knownMessage}</small>{/if}
      <small class="dim">Packages The Architect judged standard (built in) are remembered across projects so it is not asked about them again, and can be removed from the tree with the "Remove known standard packages" button. Transitive packages found only under those built-in packages are derived and remembered with them (the count includes these derived packages). Clearing forgets this list, including the derived transitive packages; per-project classifications stay.</small>
      <h3>Optional knowledge integrations</h3>
      <small class="dim">Discovery is read-only. Installation and Obsidian vault registration remain manual. No integration is installed or started by these settings.</small>
      <p>Obsidian: {$knowledge.capabilities?.obsidian.state ?? "Not checked"} {$knowledge.capabilities?.obsidian.version ?? ""}</p>
      <button disabled={integrationBusy} onclick={locateObsidian}>Locate installed Obsidian…</button>
      <button disabled={integrationBusy} onclick={() => void knowledge.probe(get(knowledge).folder)}>Check integrations again</button>
      <small class="dim">Cognee opens from the System overview toolbar when it is installed.</small>
      {#if integrationError}<small class="err" role="alert">{integrationError}</small>{/if}
      {#if open}<CodeQualitySettings />{/if}
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
