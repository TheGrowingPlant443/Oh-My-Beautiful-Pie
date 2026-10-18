<script lang="ts">
  import { onMount } from "svelte";
  import { api, type Instance, type SetupDraft, type SetupPaths, type SetupAdmission, type SetupItem, type SetupKind, type SetupRequest, type SetupScope } from "../api";
  import { instances, instancesById, managerInstance, architectInstance, errorText, refreshInstances } from "../stores";
  import SessionView from "./SessionView.svelte";
  let { draft = $bindable(), admission = $bindable(), projectContext, onback, onSetupManager, onOpenManager, onSetupArchitect, onOpenArchitect, onSelectSession, onPlan, onPlanChange, onSubagents, onCodeDiff, onSystemOverview }: {
    draft: SetupDraft; admission: SetupAdmission | null; projectContext: Instance | null; onback: () => void; onSetupManager: () => void; onSetupArchitect: () => void;
    onOpenManager: () => void; onOpenArchitect: () => void; onSelectSession: (id: string) => void; onPlan: (id: string) => void; onPlanChange: (id: string) => void; onSubagents: (id: string) => void; onSystemOverview?: (id: string) => void;
    onCodeDiff: (id: string) => void;
  } = $props();
  const steps = ["Scope", "What to create", "Describe", "Tools (optional)", "Review"];
  const REVIEW = 4;
  const choices: { kind: SetupKind; title: string; help: string }[] = [
    { kind: "agents", title: "Agents.md", help: "Instructions for an agent; named files can be chosen as a Persona when creating an instance" },
    { kind: "rules", title: "Rules.md", help: "Always-on rules" },
    { kind: "skill", title: "Skills", help: "Reusable skills with native SKILL.md definitions and a SKILLS.md catalog" },
    { kind: "workflow", title: "Workflows", help: "Native rules that describe when to delegate, require review, or ask for a decision" },
    { kind: "plugin", title: "OMP plugin", help: "A packaged native OMP extension with the tools, commands, hooks or skills you describe; loaded in this scope after reload/reopen" },
    { kind: "profile", title: "OMP profile", help: "A named profile with its own settings, for example one that starts in plan mode. It has its own login" },
  ];
  const headings: Record<SetupKind, string> = { agents: "Agents.md", rules: "Rules.md", skill: "Skills", workflow: "Workflows", plugin: "OMP plugin", profile: "OMP profile", tool: "Tools" };
  const addLabels: Record<SetupKind, string> = { agents: "Add another Agents.md", rules: "", skill: "Add another skill", workflow: "Add another workflow", plugin: "Add another OMP plugin", profile: "Add another OMP profile", tool: "Add tool" };
  const describeOrder: SetupKind[] = ["agents", "rules", "skill", "workflow", "plugin", "profile"];
  let paths = $state<SetupPaths | null>(null);
  let error = $state("");
  let busy = $state(false);
  let loading = $state(true);
  let repositoryRoot = $state<string | null>(null);
  let repositoryError = $state("");
  let repositoryLoading = $state(false);
  const repositoryAvailable = $derived(projectContext?.kind === "instance");
  const current = $derived(draft.instanceId ? $instancesById.get(draft.instanceId) : undefined);
  const target = $derived.by(() => {
    switch (draft.scope) {
      case "global": return paths?.globalAgentFolder;
      case "manager": return paths?.managerFolder;
      case "architect": return paths?.architectFolder;
      case "repository": return repositoryRoot ? `${repositoryRoot}/.omp` : undefined;
      default: return undefined;
    }
  });
  const scopeTitle = $derived.by(() => {
    switch (draft.scope) {
      case "global": return "Global — default OMP profile";
      case "manager": return "The Manager — private setup";
      case "architect": return "The Architect — private setup";
      case "repository": return `Repository — ${repositoryRoot ?? (repositoryLoading ? "Loading…" : "Unavailable")}`;
      default: return "Choose a scope first";
    }
  });
  const selected = $derived(draft.items.filter(item => (item.kind === "tool" || draft.kinds.includes(item.kind)) && (item.kind !== "profile" || draft.scope === "global") && (item.name.trim() || item.description.trim())).map(item => ({ ...item, name: slug(item.name), description: item.description.trim() })));
  onMount(() => {
    let disposed = false;
    void api.getSetupPaths().then(value => { if (!disposed) paths = value; }).catch(e => { if (!disposed) error = errorText(e); }).finally(() => { if (!disposed) loading = false; });
    return () => { disposed = true; };
  });
  $effect(() => {
    const id = draft.projectInstanceId;
    let disposed = false;
    repositoryRoot = null;
    repositoryError = "";
    repositoryLoading = !!id;
    if (id) void api.getRepositorySetupRoot(id)
      .then(root => { if (!disposed) repositoryRoot = root; })
      .catch(e => { if (!disposed) repositoryError = errorText(e); })
      .finally(() => { if (!disposed) repositoryLoading = false; });
    return () => { disposed = true; };
  });
  $effect(() => {
    if (!draft.instanceId && draft.scope === "repository" && (!repositoryAvailable || projectContext?.id !== draft.projectInstanceId)) {
      draft.scope = null;
      draft.projectInstanceId = repositoryAvailable ? projectContext!.id : null;
    }
  });
  $effect(() => {
    if (draft.scope && draft.scope !== "repository") draft.projectInstanceId = null;
    if (draft.scope !== "global" && draft.kinds.includes("profile")) draft.kinds = draft.kinds.filter(kind => kind !== "profile");
  });
  function slug(name: string): string { return name.trim().toLowerCase().replace(/\s+/g, "-"); }
  function description(kind: SetupKind): string { return draft.items.find(item => item.kind === kind)?.description || ""; }
  function setDescription(kind: SetupKind, text: string): void {
    const item = draft.items.find(item => item.kind === kind);
    if (item) item.description = text;
    else draft.items = [...draft.items, { kind, name: "", description: text }];
  }
  function add(kind: SetupKind): void {
    const item: SetupItem = { kind, name: "", description: "" };
    if (kind === "tool") item.language = "python";
    draft.items = [...draft.items, item];
  }
  function remove(item: SetupItem): void { draft.items = draft.items.filter(candidate => candidate !== item); }
  function entries(kind: SetupKind): SetupItem[] { return draft.items.filter(item => item.kind === kind); }
  function toggleKind(kind: SetupKind, on: boolean): void {
    if (on) {
      if (!draft.kinds.includes(kind)) draft.kinds = [...draft.kinds, kind];
      if (!entries(kind).length) add(kind);
    } else draft.kinds = draft.kinds.filter(candidate => candidate !== kind);
  }
  function output(item: SetupItem): string[] {
    const name = slug(item.name);
    let files: string[];
    switch (item.kind) {
      case "agents": files = [name ? `agent-instructions/${name}/AGENTS.md` : "AGENTS.md"]; break;
      case "rules": files = ["RULES.md"]; break;
      case "skill": files = [`skills/${name}/SKILL.md`, "SKILLS.md"]; break;
      case "workflow": files = [`rules/workflow-${name}.md`]; break;
      case "plugin": files = [`extensions/${name}/package.json`, `extensions/${name}/index.ts`]; break;
      case "profile": return [`${paths?.globalOmpFolder ?? "<global .omp folder>"}/profiles/${name}/agent/config.yml`];
      case "tool": files = [`tools/${name}/index.ts`, ...(item.language === "python" ? [`tools/${name}/run.py`] : item.language === "bash" ? [`tools/${name}/run.sh`] : [])]; break;
    }
    if (draft.scope === "repository") return target ? files.map(file => `${target}/${file}`) : [];
    return files;
  }
  function reviewTitle(item: SetupItem): string {
    if (item.kind === "rules") return "Rules.md";
    if (item.kind === "agents") return item.name ? `Agents.md: ${item.name}` : "Agents.md";
    if (item.kind === "profile") return `OMP profile: ${item.name}`;
    if (item.kind === "plugin") return `OMP plugin: ${item.name}`;
    return `${item.kind}: ${item.name}`;
  }
  function request(): SetupRequest {
    if (!draft.scope) throw new Error("Choose a setup scope first");
    if (draft.scope === "manager" && !$managerInstance) throw new Error("Set up The Manager first");
    if (draft.scope === "architect" && !$architectInstance) throw new Error("Set up The Architect first");
    if (draft.scope === "repository") {
      const source = draft.projectInstanceId ? $instancesById.get(draft.projectInstanceId) : undefined;
      if (source?.kind !== "instance" || (!draft.instanceId && (!repositoryAvailable || projectContext?.id !== source.id))) throw new Error("Repository setup requires a project");
      if (repositoryLoading || !repositoryRoot || repositoryError) throw new Error(repositoryError || "Repository setup target is unavailable");
    }
    for (const kind of draft.kinds) {
      if (!selected.some(item => item.kind === kind)) throw new Error("Describe every selected item or deselect it");
    }
    if (!selected.length) throw new Error("Describe at least one setup item");
    for (const item of selected) {
      if (!item.description) throw new Error("Every named item needs a description");
      if (item.kind !== "rules" && (item.kind !== "agents" || item.name) && !/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(item.name)) throw new Error("Names require letters, numbers, and single hyphens");
      if (item.kind === "profile" && item.name === "default") throw new Error("The default profile cannot be created; choose another name");
      if (item.kind === "tool" && !item.language) throw new Error("Choose a language for each tool");
    }
    return { scope: draft.scope, items: selected, projectInstanceId: draft.scope === "repository" ? draft.projectInstanceId : null };
  }
  function next(): void {
    error = "";
    if (draft.step === 0 && !draft.scope) { error = "Choose a setup scope"; return; }
    draft.step = Math.min(REVIEW, draft.step + 1);
  }
  async function start(retry = false): Promise<void> {
    error = "";
    let input: SetupRequest;
    try { input = request(); } catch (e) { error = errorText(e); return; }
    busy = true;
    try {
      admission = retry && draft.instanceId ? await api.sendAgentSetup(draft.instanceId, input) : await api.startAgentSetup(input);
      draft.instanceId = admission.instance.id;
      await refreshInstances();
      if (admission.error) error = admission.error;
    } catch (e) { error = errorText(e); }
    finally { busy = false; }
  }
  async function editor(scope: SetupScope): Promise<void> {
    try { await api.openSetupFolder(scope, scope === "repository" ? draft.projectInstanceId : null); }
    catch (e) { error = errorText(e); }
  }
  function newSetup(): void {
    const sourceId = repositoryAvailable ? projectContext!.id : null;
    if (draft.scope === "repository" && (!sourceId || draft.projectInstanceId !== sourceId)) draft.scope = null;
    draft.instanceId = null;
    admission = null;
    draft.projectInstanceId = draft.scope === "repository" ? sourceId : null;
    draft.step = 0;
    error = "";
  }
</script>

<section class="setup-page">
  <header class="page-header">
    <button onclick={onback}>Back</button><h2>Agent setup</h2><span class="spacer"></span>
    <button onclick={() => editor("global")}>Open global .omp in VS Code</button>
    <button onclick={() => editor("manager")}>Open Manager folder in VS Code</button>
    <button onclick={() => editor("architect")}>Open Architect folder in VS Code</button>
    {#if repositoryAvailable}<button disabled={repositoryLoading || !!repositoryError || !repositoryRoot} onclick={() => editor("repository")}>Open repository in VS Code</button>{/if}
  </header>
  <div class="folders dim"><span>Global folder: <code>{paths?.globalOmpFolder || (loading ? "Loading…" : "Unavailable")}</code></span><span>Manager folder: <code>{paths?.managerFolder || (loading ? "Loading…" : "Unavailable")}</code></span></div>
  <div class="folders dim"><span>Architect folder: <code>{paths?.architectFolder || (loading ? "Loading…" : "Unavailable")}</code></span></div>
  {#if repositoryAvailable || draft.scope === "repository"}<div class="folders dim"><span>Repository: <code>{repositoryRoot || (repositoryLoading ? "Loading…" : "Unavailable")}</code></span><span>Setup folder: <code>{repositoryRoot ? `${repositoryRoot}/.omp` : "Unavailable"}</code></span></div>{/if}
  {#if repositoryError}<p class="err alert" role="alert">{repositoryError}</p>{/if}
  {#if error}<p class="err alert" role="alert">{error}</p>{/if}
  {#if draft.instanceId}
    <div class="session-summary panel">
      <header><strong>{scopeTitle}</strong><span class="spacer"></span><button disabled={busy} onclick={newSetup}>New setup</button></header>
      <div class="summary-body"><p>Review and approve the actual native OMP plan below. Existing files must be preserved; completion and generated paths are reported by OMP.</p>
        {#if admission?.promptSent === false}<p class="warn">Description was not sent. Complete native startup, then explicitly send it.</p><button disabled={busy} onclick={() => start(true)}>{busy ? "Sending…" : "Send setup description"}</button>
        {:else if admission?.promptSent === null}<p class="warn">Admission outcome is unknown. Inspect the native conversation before entering another request; automatic resend is disabled.</p>
        {:else if draft.scope === "repository"}<p class="dim">Existing project processes are not reloaded automatically. Reopen OMP from the repository root to use root-scoped rules/tools.</p>
        {:else if draft.scope === "architect"}<p class="dim">Instructions load next Architect turn; skill/tool/rule catalogs need native reload or an explicit reopen. No automatic Architect restart.</p>
        {:else}<p class="dim">Instructions load next Manager turn; skill/tool/rule catalogs need native reload or an explicit reopen. No automatic Manager restart.</p>{/if}
        {#if draft.scope === "architect" && $architectInstance}<button onclick={onOpenArchitect}>Open The Architect</button>
        {:else if draft.scope === "manager" && $managerInstance}<button onclick={onOpenManager}>Open The Manager</button>{/if}
      </div>
    </div>
    {#if current}
      <div class="native-session"><SessionView instance={current} instances={$instances} onSwitch={onSelectSession} onClose={onback} {onPlan} {onPlanChange} {onSubagents} {onCodeDiff} {onSystemOverview} /></div>
    {:else}<p class="dim">This setup session is no longer in the dashboard.</p>{/if}
  {:else}
    <nav aria-label="Agent setup steps">{#each steps as step, index}<button class:active={draft.step === index} aria-current={draft.step === index ? "step" : undefined} disabled={busy} onclick={() => draft.step = index}>{index + 1}. {step}</button>{/each}</nav>
    <div class="wizard panel">
      <header><h3>{steps[draft.step]}</h3><span class="spacer"></span><span class="dim">{draft.step + 1} / {steps.length}</span></header>
      <div class="form-body">
        {#if draft.step === 0}
          <p>Choose where the generated setup applies. You can create one item or a complete set of instructions, skills, tools and workflows.</p>
          <label class="choice"><input type="radio" name="setup-scope" value="global" bind:group={draft.scope} /><span><strong>Global — default OMP profile</strong><small>Native instructions under {paths?.globalAgentFolder || "the global OMP agent folder"}. Named profiles remain independent.</small></span></label>
          <label class="choice"><input type="radio" name="setup-scope" value="manager" bind:group={draft.scope} disabled={!$managerInstance} /><span><strong>Just The Manager</strong><small>Private app-owned setup; not loaded into workers or the Manager's project .omp folder.</small></span></label>
          <label class="choice"><input type="radio" name="setup-scope" value="architect" bind:group={draft.scope} disabled={!$architectInstance} /><span><strong>Just the architect</strong><small>Private app-owned setup; not loaded into Manager, workers or the Architect's project .omp folder.</small></span></label>
          {#if repositoryAvailable}
            <label class="choice"><input type="radio" name="setup-scope" value="repository" bind:group={draft.scope} disabled={repositoryLoading || !!repositoryError || !repositoryRoot} /><span><strong>This repository</strong><small>Create native setup under {repositoryRoot ? `${repositoryRoot}/.omp` : (repositoryLoading ? "Loading…" : "an unavailable repository")}. Existing project sessions stay unchanged.</small></span></label>
          {/if}
          {#if !$managerInstance}<button onclick={onSetupManager}>Set up The Manager first</button>{/if}
          {#if !$architectInstance}<button onclick={onSetupArchitect}>Set up The Architect first</button>{/if}
        {:else if draft.step === 1}
          <p>Choose what to create. You describe each selected item in the next step.</p>
          {#each choices.filter(choice => choice.kind !== "profile" || draft.scope === "global") as choice (choice.kind)}
            <label class="choice"><input type="checkbox" checked={draft.kinds.includes(choice.kind)} onchange={event => toggleKind(choice.kind, event.currentTarget.checked)} /><span><strong>{choice.title}</strong><small>{choice.kind === "agents" && draft.scope === "repository" ? "Instructions inside this repository's .omp; named files are standalone and not automatically loaded" : choice.help}</small></span></label>
          {/each}
        {:else if draft.step === 2}
          {#each describeOrder.filter(kind => draft.kinds.includes(kind)) as kind (kind)}
            <section class="kind-section">
              <h4>{headings[kind]}</h4>
              {#if kind === "rules"}
                <p>Describe the persistent rules. Existing RULES.md policy will be preserved.</p>
                <label>Description<textarea rows="7" value={description("rules")} oninput={event => setDescription("rules", event.currentTarget.value)} placeholder="Which policy should always apply?"></textarea></label>
              {:else}
                {#if kind === "agents"}<p>Describe how the agent should behave. {#if draft.scope === "repository"}An unnamed entry merges into .omp/AGENTS.md; a named entry is a standalone file, not automatically loaded or listed as a Global/Manager Persona. Use it with an explicit --append-system-prompt argument.{:else}An unnamed entry merges into the scope's AGENTS.md; a named entry becomes a standalone file selectable as a Persona.{/if}</p>
                {:else if kind === "skill"}<p>Skills use native SKILL.md definitions. SKILLS.md is a readable catalog.</p>
                {:else if kind === "workflow"}<p>Workflows are native rules: describe when to delegate, require review, report completion, or ask for a decision. They are not a separate scheduler.</p>
                {:else if kind === "plugin"}<p>A plugin is a native OMP extension package (package.json plus index.ts) loaded from this scope's extensions folder. Describe the tools, commands or hooks it should provide; nothing is published or installed globally.</p>
                {:else}<p>The profile is created in the global OMP folder with its own config. It has its own login; run /login inside it.</p>{/if}
                {#each entries(kind) as item}
                  <article class="item-form">
                    <div class="row"><label>Name<input bind:value={item.name} placeholder={kind === "agents" ? "reviewer — leave empty to edit the scope's AGENTS.md" : kind === "profile" ? "plan-mode" : kind === "plugin" ? "release-notes" : "local-review"} /></label>{#if entries(kind).length > 1}<button onclick={() => remove(item)}>Remove</button>{/if}</div>
                    <label>Description<textarea bind:value={item.description} rows="5" placeholder={kind === "profile" ? "How should this profile behave? For example: starts in plan mode and uses the small model." : kind === "agents" ? "What should the agent do, prioritize, and avoid?" : kind === "plugin" ? "Which tools, commands or hooks should the plugin provide, and what should they do?" : "Describe the behavior, inputs, outputs, and boundaries."}></textarea></label>
                  </article>
                {/each}
                <button onclick={() => add(kind)}>{addLabels[kind]}</button>
              {/if}
            </section>
          {/each}
          {#if !draft.kinds.length}<p class="dim">Nothing selected — go back, or continue to Tools.</p>{/if}
        {:else if draft.step === 3}
          <p>Optional. Tools are usually best described together with the skill or workflow that uses them.</p>
          <p class="dim">Create reusable local Python, Bash, or TypeScript code with a native OMP tool wrapper and approval policy.</p>
          {#each entries("tool") as item}
            <article class="item-form">
              <div class="row"><label>Name<input bind:value={item.name} placeholder="local-review" /></label><button onclick={() => remove(item)}>Remove</button></div>
              <label>Language<select bind:value={item.language}><option value="python">Python</option><option value="bash">Bash</option><option value="typescript">TypeScript</option></select></label>
              <label>Description<textarea bind:value={item.description} rows="5" placeholder="Describe the behavior, inputs, outputs, and boundaries."></textarea></label>
            </article>
          {/each}
          <button onclick={() => add("tool")}>Add tool</button>
        {:else if draft.step === REVIEW}
          <p><strong>{scopeTitle}</strong></p>
          <p class="mono">{target || "Target folder unavailable"}</p>
          {#each selected as item}<article class="review-item"><h4>{reviewTitle(item)}</h4><p class="description">{item.description}</p>{#each output(item) as file}<code>{file}</code>{/each}</article>{/each}
          {#if !selected.length}<p class="warn">No descriptions yet. Select and describe at least one item in the preceding steps.</p>{/if}
          <p class="dim">A real, human-controlled OMP session will propose a native plan. No target files are written before you approve. No credentials, dependencies, telemetry, automatic workflows, or unrelated code are installed.</p>
          {#if draft.scope === "repository"}<p class="dim">Existing project processes are not reloaded or restarted automatically. A nearer .omp or nested cwd can change discovery; reopen OMP from this repository root for root-scoped rules/tools.</p>{/if}
          <button class="primary" disabled={busy || !paths || !draft.scope || (draft.scope === "repository" && (repositoryLoading || !!repositoryError || !repositoryRoot))} onclick={() => start()}>{busy ? "Starting native setup…" : "Start setup agent"}</button>
        {/if}
      </div>
      <footer>{#if draft.step > 0}<button disabled={busy} onclick={() => draft.step--}>Previous</button>{/if}<span class="spacer"></span>{#if draft.step < REVIEW}<button class="primary" disabled={busy} onclick={next}>{draft.step === 3 && !entries("tool").length ? "Skip" : "Next"}</button>{/if}</footer>
    </div>
  {/if}
</section>
<style>
  .setup-page { display: flex; flex-direction: column; gap: 12px; height: 100%; min-height: 0; }
  .page-header { display: flex; align-items: center; flex-wrap: wrap; gap: 10px; }
  h2, h3, h4, p { margin: 0; }
  h2 { font-size: 20px; }
  .folders { display: flex; flex-wrap: wrap; gap: 8px 24px; font-size: 12px; overflow-wrap: anywhere; }
  nav { display: flex; flex-wrap: wrap; gap: 6px; }
  .wizard { flex: 1; min-height: 0; }
  .form-body { padding: 18px; display: flex; flex-direction: column; gap: 16px; overflow: auto; }
  label { display: flex; flex-direction: column; gap: 6px; }
  .choice { flex-direction: row; padding: 16px; border: 1px solid var(--line); border-radius: 8px; align-items: flex-start; }
  .choice input { width: auto; }
  .choice small { display: block; color: var(--text-dim); margin-top: 6px; }
  textarea { resize: vertical; }
  .kind-section { display: flex; flex-direction: column; gap: 12px; }
  .item-form { display: flex; flex-direction: column; gap: 12px; padding: 16px; border: 1px solid var(--line); border-radius: 8px; }
  .row { display: flex; align-items: flex-end; gap: 10px; }
  .row label { flex: 1; }
  .review-item { display: flex; flex-direction: column; gap: 8px; padding-bottom: 14px; border-bottom: 1px solid var(--line); }
  .description { white-space: pre-wrap; }
  footer { display: flex; padding: 12px 16px; gap: 10px; border-top: 1px solid var(--line); }
  .session-summary { flex-shrink: 0; }
  .summary-body { padding: 12px; display: flex; flex-wrap: wrap; align-items: center; gap: 10px; }
  .summary-body p { flex-basis: 100%; }
  .native-session { flex: 1; min-height: 280px; }
  .native-session :global(.view) { height: 100%; }
  .alert { padding: 10px; border: 1px solid var(--err); border-radius: 8px; }
  @media (max-height: 650px) { .setup-page { height: auto; } }
</style>
