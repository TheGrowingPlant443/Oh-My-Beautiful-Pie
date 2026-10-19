<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { api, type Instance, type OmpProfile } from "../api";
  import { agentMarkdownTargetKey, canApplyMarkdown, createAgentMarkdownStore, unappliedMarkdown, type AgentMarkdownTarget } from "../agent-markdown";
  import { knowledge, obsidianInstalled, type ScopeRequest } from "../knowledge";
  import VaultConfiguration from "./VaultConfiguration.svelte";
  import VaultView from "./VaultView.svelte";

  let { setupTarget, projectContext, repositoryRoot, refreshKey = "", onChanged }: { setupTarget: AgentMarkdownTarget | null; projectContext: Instance | null; repositoryRoot: string | null; refreshKey?: string; onChanged: () => void } = $props();
  const markdown = createAgentMarkdownStore();
  let profiles = $state<OmpProfile[]>([]);
  let profileError = $state<string | null>(null);
  let selection = $state("global");
  let configure = $state(false);
  let documents = $state(false);
  let vaultRefresh = $state(0);
  let disposed = false, profileEpoch = 0;
  const targets = $derived<{ value: string; label: string; target: AgentMarkdownTarget }[]>([
    { value: "global", label: "Global — default profile", target: { kind: "global" } },
    { value: "manager", label: "Manager", target: { kind: "manager" } },
    { value: "architect", label: "Architect", target: { kind: "architect" } },
    ...(projectContext?.kind === "instance" ? [{ value: "repository", label: `Repository — ${projectContext.name}`, target: { kind: "repository" as const, projectInstanceId: projectContext.id } }] : setupTarget?.kind === "repository" ? [{ value: "repository", label: "Repository — active setup", target: setupTarget }] : []),
    ...profiles.filter(profile => profile.name !== "default").map(profile => ({ value: `profile:${profile.name}`, label: `Profile — ${profile.name}`, target: { kind: "profile" as const, profileName: profile.name } })),
  ]);
  const active = $derived(targets.find(target => target.value === selection) ?? targets[0]);
  const activeKey = $derived(agentMarkdownTargetKey(active.target));
  const status = $derived($markdown.status && agentMarkdownTargetKey($markdown.status.target) === activeKey ? $markdown.status : null);
  const scope = $derived<ScopeRequest>(active.target.kind === "repository" ? { kind: "project" } : { kind: "global" });
  const folder = $derived(active.target.kind === "repository" ? projectContext?.projectPath ?? repositoryRoot : null);
  const pending = $derived(status ? unappliedMarkdown(status) : []);
  const locations = $derived(targets.filter(target => (target.target.kind === "repository") === (active.target.kind === "repository")).map(target => ({ label: target.label, path: target.target.kind === "profile" ? `Agent setup/Profiles/${target.target.profileName}` : `Agent setup/${target.target.kind === "global" ? "Global" : target.target.kind === "manager" ? "Manager" : target.target.kind === "architect" ? "Architect" : "Repository"}` })));

  let lastSetup = "";
  $effect(() => {
    const key = setupTarget ? agentMarkdownTargetKey(setupTarget) : "";
    if (key === lastSetup) return;
    lastSetup = key;
    if (setupTarget) selection = setupTarget.kind === "profile" ? `profile:${setupTarget.profileName}` : setupTarget.kind;
  });
  $effect(() => {
    activeKey;
    const target = untrack(() => active.target);
    documents = false; configure = false;
    untrack(() => { void markdown.selectTarget(target); });
  });
  let previousRefresh = untrack(() => refreshKey);
  $effect(() => {
    const key = refreshKey;
    if (key !== previousRefresh) { previousRefresh = key; untrack(() => { void markdown.refresh(); void refreshProfiles(); vaultRefresh++; }); }
  });
  async function refreshProfiles(): Promise<void> {
    const mine = ++profileEpoch;
    try { const found = await api.listOmpProfiles(); if (!disposed && mine === profileEpoch) { profiles = found.profiles; profileError = found.error; } }
    catch (failure) { if (!disposed && mine === profileEpoch) profileError = String(failure); }
  }
  onMount(() => {
    const focus = (): void => { void markdown.refresh(); void refreshProfiles(); };
    void refreshProfiles();
    window.addEventListener("focus", focus);
    return () => { disposed = true; markdown.dispose(); window.removeEventListener("focus", focus); };
  });
  function openView(): void {
    if (!status?.vaultId) { configure = true; return; }
    documents = true;
  }
  function toggle(id: string, checked: boolean): void {
    const review = $markdown.review;
    if (!review) return;
    const selected = checked ? [...review.selectedIds, id] : review.selectedIds.filter(candidate => candidate !== id);
    if (review.kind === "apply") markdown.selectApply(selected);
    else void markdown.prepare("import", selected);
  }
  async function confirm(): Promise<void> { await markdown.confirm(); vaultRefresh++; onChanged(); }
</script>

<details class="markdown-panel">
  <summary>Agent setup Markdown — Import, Apply and vault views{#if pending.length} · Unapplied Markdown changes: {pending.length}{/if}</summary>
  <section aria-label="Agent setup Markdown">
  <header><h3>Agent setup Markdown</h3><label>Target <select bind:value={selection} disabled={$markdown.busy}>{#each targets as target (target.value)}<option value={target.value}>{target.label}</option>{/each}</select></label><button disabled={$markdown.busy} onclick={() => void markdown.refresh()}>Refresh status</button></header>
  <div class="actions"><button disabled={$markdown.busy} onclick={openView}>{obsidianInstalled($knowledge.capabilities) ? "Obsidian view" : "Documents view"}</button><button disabled={$markdown.busy} onclick={() => configure = true}>Configure vault</button></div>
  {#if profileError}<p class="warn" role="status">Named native profiles could not all be listed: {profileError}</p>{/if}
  {#if $markdown.error}<p class="err" role="alert">{$markdown.error} Refresh status before preparing another review if a write failed.</p>{/if}
  {#if $markdown.busy}<p role="status">Reading or preparing agent Markdown…</p>{/if}
  {#if status}
    <p><strong>{status.enabled ? "Markdown source enabled" : "Native setup remains authoritative"}</strong> — <code>{status.namespace}</code></p>
    <p class="paths">Markdown source: <code>{status.sourceRoot ?? "No vault bound"}</code><br />Native runtime: <code>{status.nativeRoot}</code></p>
    <p class="revisions">Source revision: <code>{status.sourceRevision || "(empty)"}</code><br />Applied revision: <code>{status.appliedRevision ?? "Not applied"}</code></p>
    <p class:warn={pending.length > 0}><strong>Unapplied Markdown changes: {pending.length}</strong>{#if status.sourceRevision !== status.appliedRevision} · Source and applied revisions differ.{/if}</p>
    <p class="dim">Binding a vault does not migrate or enable this target. Review Import to copy native Markdown and explicitly enable it; an empty selection enables without copying. Existing native files are retained. Scripts, tools, plugin code, config and credentials stay native. Skill references to native scripts/resources are preserved, not copied into the vault.</p>
    <p class="dim">Editing Markdown does not change a running agent. Review and explicitly Apply selected changes, then use the native reload or reopen the session. Import/Apply batches are recoverable, not externally atomic: other processes may observe intermediate files.</p>
    {#each status.warnings as warning}<p class="warn" role="status">{warning}</p>{/each}
    <div class="actions"><button disabled={$markdown.busy || !status.vaultId} onclick={() => void markdown.prepare("import")}>Review Import from native</button><button disabled={$markdown.busy || !status.enabled || !status.vaultId} onclick={() => void markdown.prepare("apply")}>Review Apply to native</button></div>
    {#if !status.vaultId}<p class="dim">Choose a vault using either view or Configure vault, then review Import. Reading status creates nothing.</p>{/if}
    {#if status.items.length}<details><summary>Source and runtime files ({status.items.length})</summary>{#each status.items as item (item.id)}<article><strong>{item.id} — {item.state}</strong><div>Markdown: <code>{item.vaultPath}</code></div><div>Runtime: <code>{item.nativePath}</code></div>{#if item.nativeDependencies.length}<p class="dim">Native dependencies (not migrated): {item.nativeDependencies.join(", ")}</p>{/if}</article>{/each}</details>{/if}
  {/if}
  {#if $markdown.review}
    {@const review = $markdown.review}
    <section class="review" aria-label={`${review.kind} review`}>
      <h4>{review.kind === "import" ? "Import review: native → Markdown source" : "Apply review: Markdown source → native runtime"}</h4>
      <p>Review every selected diff, including deletions. This one-use review expires in ten minutes. Changing import selection prepares a new reviewed token; conflicts require an explicit selection to replace the Markdown source.</p>
      {#if review.kind === "apply"}<p>Source revision: <code>{"sourceRevision" in review.preparation ? review.preparation.sourceRevision : ""}</code>. Native changes and conflicts cannot be applied; reconcile them through a reviewed import or edit first.</p>{/if}
      {#each review.preparation.warnings as warning}<p class="warn">{warning}</p>{/each}
      <div class="review-items">{#each review.preparation.items as item (item.id)}
        <article class:conflict={item.state === "conflict" || item.state === "nativeChanged"}>
          <label class="item-choice"><input type="checkbox" checked={review.selectedIds.includes(item.id)} disabled={$markdown.busy || (review.kind === "apply" ? !canApplyMarkdown(item) : item.nativeHash === null)} onchange={event => toggle(item.id, event.currentTarget.checked)} /><strong>{item.id} — {item.state}{review.kind === "apply" && item.state === "deleted" ? " (DELETE native file)" : ""}</strong></label>
          <div>From: <code>{review.kind === "import" ? item.nativePath : item.vaultPath}</code></div><div>To: <code>{review.kind === "import" ? item.vaultPath : item.nativePath}</code></div>
          {#if item.nativeDependencies.length}<p class="dim">Keep native dependencies: {item.nativeDependencies.join(", ")}</p>{/if}
          <pre>{item.diff || "No textual diff supplied for this item."}</pre>
        </article>
      {/each}</div>
      {#if review.kind === "import" && !review.selectedIds.length}<p class="warn">No files selected. Confirming still enables this target's Markdown source.</p>{/if}
      <div class="actions"><button disabled={$markdown.busy || (review.kind === "apply" && !review.selectedIds.length)} onclick={() => void confirm()}>{review.kind === "import" ? "Confirm Import and enable Markdown" : `Apply ${review.selectedIds.length} selected changes`}</button><button disabled={$markdown.busy} onclick={() => markdown.cancel()}>Cancel review</button></div>
    </section>
  {/if}
  {#if documents && status?.vaultId}<div class="documents"><VaultView vaultId={status.vaultId} initialPath={status.namespace} {locations} refreshKey={vaultRefresh} title="Agent setup documents" backLabel="Back to agent setup" onBack={() => documents = false} /></div>{/if}
  </section>
</details>
<VaultConfiguration open={configure} {scope} {folder} offerObsidianInstall={false} title="Choose an agent setup vault" onClose={() => configure = false} onBound={() => { configure = false; void markdown.refresh(); onChanged(); }} />

<style>
  .markdown-panel { border: 1px solid var(--line); border-radius: 8px; padding: 12px; flex-shrink: 0; overflow-wrap: anywhere; max-height: 70vh; overflow: auto; }
  .markdown-panel > section { display: flex; flex-direction: column; gap: 10px; margin-top: 10px; }
  header, .actions { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; }
  header h3 { margin-right: auto; }
  h3, h4, p { margin: 0; }
  label { display: flex; align-items: center; gap: 8px; }
  .paths, .revisions, article { font-size: 12px; }
  .dim { color: var(--text-dim); font-size: 12px; }
  .err { color: var(--err); }
  .warn { color: var(--warn); }
  .review { display: flex; flex-direction: column; gap: 10px; border: 1px solid var(--line); padding: 12px; border-radius: 8px; }
  .review-items { max-height: 480px; overflow: auto; }
  article { display: flex; flex-direction: column; gap: 6px; padding: 10px 0; border-bottom: 1px solid var(--line); }
  article.conflict { border-left: 3px solid var(--warn); padding-left: 8px; }
  .item-choice { align-items: flex-start; }
  input[type="checkbox"] { width: auto; }
  pre { margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; font-size: 12px; }
  .documents { min-height: 600px; height: 70vh; }
</style>
