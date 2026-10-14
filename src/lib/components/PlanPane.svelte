<script lang="ts">
  /** Current plan of a session: headline list or full text; a headline jumps to its place in the full text. */
  import { tick, untrack } from "svelte";
  import { api } from "../api";
  import type { Instance, PlanBackup } from "../api";
  import { errorText, reportError } from "../stores";
  import { planHeadlines } from "../plan";
  import { confirmDanger } from "../ui";

  let { target }: { target: Instance } = $props();

  const POLL_MS = 3000;
  let text = $state<string | null>(null);
  let error = $state<string | null>(null);
  let loading = $state(false);
  let mode = $state<"short" | "full">("short");
  let flash = $state<number | null>(null);
  let body = $state<HTMLDivElement>();
  let backups = $state<PlanBackup[]>([]);
  let historyOpen = $state(false);
  let selectedBackup = $state("");
  let restoring = $state(false);
  let requestSequence = 0;
  const selected = $derived(backups.find(backup => backup.id === selectedBackup));
  const id = $derived(target.id);
  const headlines = $derived(text === null ? [] : planHeadlines(text));
  const headingLines = $derived(new Set(headlines.map(h => h.line)));
  const lines = $derived(text === null ? [] : text.split(/\r?\n/));

  async function load(owner: string): Promise<void> {
    if (restoring) return;
    const sequence = ++requestSequence;
    loading = true;
    try {
      const [next, history] = await Promise.all([api.readPlan(owner), api.listPlanBackups(owner)]);
      if (owner !== id || sequence !== requestSequence) return;
      if (next !== text) text = next;
      backups = history;
      error = null;
    } catch (err) {
      if (owner !== id) return;
      error = errorText(err);
      reportError("read plan failed", err);
    } finally {
      if (owner === id && sequence === requestSequence) loading = false;
    }
  }
  $effect(() => {
    const owner = id;
    untrack(() => void load(owner));
    const timer = setInterval(() => void load(owner), POLL_MS);
    return () => clearInterval(timer);
  });

  async function restore(): Promise<void> {
    if (!selected || restoring) return;
    const backupId = selected.id;
    if (!await confirmDanger("Restore this backup? The current plan is backed up first. A running target will be interrupted; no handoff is made.")) return;
    restoring = true;
    ++requestSequence;
    try {
      await api.restorePlanBackup(id, backupId);
      mode = "full";
    } catch (err) { reportError("Restore plan backup", err); }
    finally { restoring = false; await load(id); }
  }

  async function reveal(line: number): Promise<void> {
    mode = "full";
    flash = line;
    await tick();
    body?.querySelector(`[data-line="${line}"]`)?.scrollIntoView({ block: "start" });
    setTimeout(() => { if (flash === line) flash = null; }, 1500);
  }
  const kindLabel = { todo: "todo list", file: "plan file", saved: "last saved plan" } as const;
</script>
<section class="plan-pane" aria-label="Current plan">
  <header>
    <strong>Plan · {target.name}</strong>
    <span class="pill">{target.status}</span>
    {#if target.planKind}<span class="pill dim">{kindLabel[target.planKind]}</span>{/if}
    {#if target.todoTotal}<span class="pill dim">{target.todoDone}/{target.todoTotal}</span>{/if}
    <span class="spacer"></span>
    <button onclick={() => mode = mode === "short" ? "full" : "short"}>{mode === "short" ? "Full text" : "Headlines"}</button>
    <button disabled={loading} onclick={() => void load(id)}>Refresh</button>
    <button aria-expanded={historyOpen} onclick={() => historyOpen = !historyOpen}>Backups ({backups.length})</button>
  </header>
  {#if target.task}<p class="task dim">{target.task}</p>{/if}
  {#if historyOpen}
    <div class="backup-controls">
      {#if backups.length}
        <label>Previous plan
          <select bind:value={selectedBackup}>
            <option value="">Choose a backup</option>
            {#each backups as backup (backup.id)}<option value={backup.id}>{new Date(backup.savedAt).toLocaleString()} · {backup.id.slice(0, 8)}</option>{/each}
          </select>
        </label>
        <button disabled={!selected || restoring} onclick={() => void restore()}>{restoring ? "Restoring…" : "Restore backup"}</button>
        {#if selected}<pre class="backup-preview mono">{selected.text}</pre>{/if}
      {:else}<p class="dim">A backup is saved automatically before each plan change.</p>{/if}
    </div>
  {/if}
  <div class="body" bind:this={body}>
    {#if error}<p class="err">{error}</p>
    {:else if text === null}<p class="empty">{loading ? "Reading plan…" : "No active plan: this session has none yet, or its plan is completed."}</p>
    {:else if mode === "short"}
      {#if headlines.length}
        <ol class="headlines">
          {#each headlines as h (h.line)}
            <li><button class="headline" style:padding-left={`${(h.level - 1) * 14 + 6}px`} onclick={() => void reveal(h.line)}>{h.title}</button></li>
          {/each}
        </ol>
      {:else}<p class="empty">The plan is empty.</p>{/if}
    {:else}
      <div class="plan mono">
        {#each lines as line, i (i)}<div class="line" class:heading={headingLines.has(i)} class:flash={flash === i} data-line={i}>{line === "" ? " " : line}</div>{/each}
      </div>
    {/if}
  </div>
</section>
<style>
  .plan-pane { height: 100%; min-width: 0; display: flex; flex-direction: column; background: #101722; border: 1px solid var(--line); }
  header { display: flex; align-items: center; gap: 8px; padding: 8px 10px; border-bottom: 1px solid var(--line); flex-wrap: wrap; }
  .task { margin: 0; padding: 6px 10px; font-size: 12px; overflow-wrap: anywhere; }
  .backup-controls { padding: 8px 10px; border-bottom: 1px solid var(--line); display: flex; flex-wrap: wrap; gap: 8px; align-items: center; }
  .backup-preview { flex-basis: 100%; max-height: 140px; overflow: auto; white-space: pre-wrap; font-size: 12px; }
  .body { flex: 1; min-height: 0; overflow: auto; padding: 10px; }
  .headlines { list-style: none; margin: 0; padding: 0; }
  .headline { display: block; width: 100%; text-align: left; background: none; border: none; padding-top: 5px; padding-bottom: 5px; cursor: pointer; }
  .headline:hover { background: rgba(255, 255, 255, 0.06); }
  .plan { font-size: 12px; line-height: 1.55; white-space: pre-wrap; word-break: break-word; color: var(--text); }
  .heading { font-weight: 700; }
  .flash { background: rgba(76, 141, 255, 0.25); }
  p { margin: 0; overflow-wrap: anywhere; }
</style>
