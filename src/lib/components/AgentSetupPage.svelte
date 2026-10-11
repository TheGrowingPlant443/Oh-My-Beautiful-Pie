<script lang="ts">
  import { onMount } from "svelte";
  import { api, type SetupDraft, type SetupPaths, type SetupAdmission, type SetupItem, type SetupKind, type SetupRequest, type SetupScope } from "../api";
  import { instances, instancesById, managerInstance, errorText, refreshInstances } from "../stores";
  import SessionView from "./SessionView.svelte";
  let { draft = $bindable(), admission = $bindable(), onback, onSetupManager, onOpenManager, onSelectSession, onPlan, onSubagents }: {
    draft: SetupDraft; admission: SetupAdmission | null; onback: () => void; onSetupManager: () => void;
    onOpenManager: () => void; onSelectSession: (id: string) => void; onPlan: (id: string) => void; onSubagents: (id: string) => void;
  } = $props();
  const steps = ["Scope", "Agents.md", "Rules.md", "Skills", "Tools", "Workflows", "Review"];
  const stepKinds: SetupKind[] = ["agents", "rules", "skill", "tool", "workflow"];
  let paths = $state<SetupPaths | null>(null);
  let error = $state("");
  let busy = $state(false);
  let loading = $state(true);
  const current = $derived(draft.instanceId ? $instancesById.get(draft.instanceId) : undefined);
  const target = $derived(draft.scope === "global" ? paths?.globalAgentFolder : paths?.managerFolder);
  const selected = $derived(draft.items.filter(item => item.name.trim() || item.description.trim()).map(item => ({ ...item, name: slug(item.name), description: item.description.trim() })));
  const currentKind = $derived(draft.step > 0 && draft.step < 6 ? stepKinds[draft.step - 1] : null);
  onMount(() => {
    let disposed = false;
    void api.getSetupPaths().then(value => { if (!disposed) paths = value; }).catch(e => { if (!disposed) error = errorText(e); }).finally(() => { if (!disposed) loading = false; });
    return () => { disposed = true; };
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
  function output(item: SetupItem): string[] {
    const name = slug(item.name);
    switch (item.kind) {
      case "agents": return ["AGENTS.md"];
      case "rules": return ["RULES.md"];
      case "skill": return [`skills/${name}/SKILL.md`, "SKILLS.md"];
      case "workflow": return [`rules/workflow-${name}.md`];
      case "tool": return [`tools/${name}/index.ts`, ...(item.language === "python" ? [`tools/${name}/run.py`] : item.language === "bash" ? [`tools/${name}/run.sh`] : [])];
    }
  }
  function request(): SetupRequest {
    if (!draft.scope) throw new Error("Choose Global or The Manager first");
    if (draft.scope === "manager" && !$managerInstance) throw new Error("Set up The Manager first");
    if (!selected.length) throw new Error("Describe at least one setup item");
    for (const item of selected) {
      if (!item.description) throw new Error("Every named item needs a description");
      if (!["agents", "rules"].includes(item.kind) && !/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(item.name)) throw new Error("Names require letters, numbers, and single hyphens");
      if (item.kind === "tool" && !item.language) throw new Error("Choose a language for each tool");
    }
    return { scope: draft.scope, items: selected };
  }
  function next(): void {
    error = "";
    if (draft.step === 0 && !draft.scope) { error = "Choose a setup scope"; return; }
    draft.step = Math.min(6, draft.step + 1);
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
    try { await api.openSetupFolder(scope); }
    catch (e) { error = errorText(e); }
  }
  function newSetup(): void { draft.instanceId = null; admission = null; draft.step = 0; error = ""; }
</script>

<section class="setup-page">
  <header class="page-header">
    <button onclick={onback}>Back</button><h2>Agent setup</h2><span class="spacer"></span>
    <button onclick={() => editor("global")}>Open global .omp in VS Code</button>
    <button onclick={() => editor("manager")}>Open Manager folder in VS Code</button>
  </header>
  <div class="folders dim"><span>Global folder: <code>{paths?.globalOmpFolder || (loading ? "Loading…" : "Unavailable")}</code></span><span>Manager folder: <code>{paths?.managerFolder || (loading ? "Loading…" : "Unavailable")}</code></span></div>
  {#if error}<p class="err alert" role="alert">{error}</p>{/if}
  {#if draft.instanceId}
    <div class="session-summary panel">
      <header><strong>{draft.scope === "global" ? "Global — default OMP profile" : "The Manager — private setup"}</strong><span class="spacer"></span><button disabled={busy} onclick={newSetup}>New setup</button></header>
      <div class="summary-body"><p>Review and approve the actual native OMP plan below. Existing files must be preserved; completion and generated paths are reported by OMP.</p>
        {#if admission?.promptSent === false}<p class="warn">Description was not sent. Complete native startup, then explicitly send it.</p><button disabled={busy} onclick={() => start(true)}>{busy ? "Sending…" : "Send setup description"}</button>
        {:else if admission?.promptSent === null}<p class="warn">Admission outcome is unknown. Inspect the native conversation before entering another request; automatic resend is disabled.</p>
        {:else}<p class="dim">Instructions load next Manager turn; skill/tool/rule catalogs need native reload or an explicit reopen. No automatic Manager restart.</p>{/if}
        {#if $managerInstance}<button onclick={onOpenManager}>Open The Manager</button>{/if}
      </div>
    </div>
    {#if current}
      <div class="native-session"><SessionView instance={current} instances={$instances} onSwitch={onSelectSession} onClose={onback} {onPlan} {onSubagents} /></div>
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
          {#if !$managerInstance}<button onclick={onSetupManager}>Set up The Manager first</button>{/if}
        {:else if currentKind === "agents" || currentKind === "rules"}
          <p>{currentKind === "agents" ? "Describe how the agent should behave. Existing AGENTS.md instructions will be preserved and merged." : "Describe the persistent rules. Existing RULES.md policy will be preserved."}</p>
          <label>Description<textarea rows="9" value={description(currentKind)} oninput={event => setDescription(currentKind!, event.currentTarget.value)} placeholder={currentKind === "agents" ? "What should the agent do, prioritize, and avoid?" : "Which policy should always apply?"}></textarea></label>
          <p class="dim">Leave blank to skip this file.</p>
        {:else if currentKind}
          <p>{currentKind === "skill" ? "Skills use native SKILL.md definitions. SKILLS.md is a readable catalog." : currentKind === "tool" ? "Create reusable local Python, Bash, or TypeScript code with a native OMP tool wrapper and approval policy." : "Workflows are native rules: describe when to delegate, require review, report completion, or ask for a decision. They are not a separate scheduler."}</p>
          {#each draft.items.filter(item => item.kind === currentKind) as item}
            <article class="item-form">
              <div class="row"><label>Name<input bind:value={item.name} placeholder="local-review" /></label><button onclick={() => remove(item)}>Remove</button></div>
              {#if item.kind === "tool"}<label>Language<select bind:value={item.language}><option value="python">Python</option><option value="bash">Bash</option><option value="typescript">TypeScript</option></select></label>{/if}
              <label>Description<textarea bind:value={item.description} rows="5" placeholder="Describe the behavior, inputs, outputs, and boundaries."></textarea></label>
            </article>
          {/each}
          <button onclick={() => add(currentKind!)}>Add {currentKind === "skill" ? "skill" : currentKind === "tool" ? "tool" : "workflow"}</button>
          <p class="dim">Skip this section if none are needed. Use a distinct name for each item.</p>
        {:else if draft.step === 6}
          <p><strong>{draft.scope === "global" ? "Global — default OMP profile" : draft.scope === "manager" ? "The Manager — private setup" : "Choose a scope first"}</strong></p>
          <p class="mono">{target || "Target folder unavailable"}</p>
          {#each selected as item}<article class="review-item"><h4>{item.kind === "agents" ? "Agents.md" : item.kind === "rules" ? "Rules.md" : `${item.kind}: ${item.name}`}</h4><p class="description">{item.description}</p>{#each output(item) as file}<code>{file}</code>{/each}</article>{/each}
          {#if !selected.length}<p class="warn">No descriptions yet. Add at least one item in the preceding sections.</p>{/if}
          <p class="dim">A real, human-controlled OMP session will propose a native plan. No target files are written before you approve. No credentials/config files, dependencies, telemetry, automatic workflows, or unrelated code are installed.</p>
          <button class="primary" disabled={busy || !paths || !draft.scope || !selected.length} onclick={() => start()}>{busy ? "Starting native setup…" : "Start setup agent"}</button>
        {/if}
      </div>
      <footer>{#if draft.step > 0}<button disabled={busy} onclick={() => draft.step--}>Previous</button>{/if}<span class="spacer"></span>{#if draft.step < 6}<button class="primary" disabled={busy} onclick={next}>{draft.step === 0 ? "Next" : "Next / skip"}</button>{/if}</footer>
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
