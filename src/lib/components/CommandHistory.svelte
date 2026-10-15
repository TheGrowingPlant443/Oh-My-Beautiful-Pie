<script lang="ts">
  import { api, type HistoryEntry } from "../api";
  import { errorText, terminalOwners } from "../stores";
  import { stripAnsi } from "../terminal";

  // null = every agent's current conversation (Dashboard page), otherwise one agent.
  let { instanceId }: { instanceId: string | null } = $props();
  const owners = $derived(instanceId === null ? [...$terminalOwners.values()] : [$terminalOwners.get(instanceId)].filter(owner => owner !== undefined));
  // Starting, finishing or restarting a command changes an owner's revision: reload the newest page then.
  const refreshKey = $derived(owners.map(owner => `${owner.instanceId}:${owner.generation}:${owner.revision}`).join("|"));

  let entries = $state<HistoryEntry[]>([]);
  let cursor = $state<string | null>(null);
  let loaded = $state(false);
  let error = $state<string | null>(null);
  let loadingOlder = $state(false);
  let expanded = $state<Record<string, { text: string | null; output: string | null; error: string | null }>>({});
  let requests = 0;

  const rowKey = (entry: HistoryEntry): string => `${entry.instanceId}:${entry.command.executionId}`;
  const newestFirst = (a: HistoryEntry, b: HistoryEntry): number =>
    b.command.startedAt - a.command.startedAt || b.instanceId.localeCompare(a.instanceId) || b.command.executionId.localeCompare(a.command.executionId);

  async function refresh(): Promise<void> {
    const request = ++requests;
    try {
      const page = await api.listTerminalHistory(instanceId, null);
      if (request !== requests) return;
      const merged = new Map(entries.map(entry => [rowKey(entry), entry]));
      for (const entry of page.entries) merged.set(rowKey(entry), entry);
      entries = [...merged.values()].sort(newestFirst);
      if (!loaded) cursor = page.nextCursor;
      loaded = true;
      error = null;
    } catch (failure) { if (request === requests) error = errorText(failure); }
  }
  $effect(() => { void refreshKey; void refresh(); });

  async function loadOlder(): Promise<void> {
    if (!cursor || loadingOlder) return;
    loadingOlder = true;
    try {
      const page = await api.listTerminalHistory(instanceId, cursor);
      const known = new Set(entries.map(rowKey));
      entries = [...entries, ...page.entries.filter(entry => !known.has(rowKey(entry)))].sort(newestFirst);
      cursor = page.nextCursor;
    } catch (failure) { error = errorText(failure); }
    finally { loadingOlder = false; }
  }

  async function toggle(entry: HistoryEntry): Promise<void> {
    const key = rowKey(entry);
    if (expanded[key]) {
      const { [key]: _closed, ...rest } = expanded;
      expanded = rest;
      return;
    }
    expanded = { ...expanded, [key]: { text: null, output: null, error: null } };
    try {
      const [text, output] = await Promise.all([
        api.readTerminalScript(entry.instanceId, entry.command.sessionId, entry.command.executionId),
        entry.command.hasOutput ? api.readTerminalOutput(entry.instanceId, entry.command.sessionId, entry.command.executionId) : Promise.resolve(null),
      ]);
      if (expanded[key]) expanded = { ...expanded, [key]: { text, output, error: null } };
    } catch (failure) {
      if (expanded[key]) expanded = { ...expanded, [key]: { text: null, output: null, error: errorText(failure) } };
    }
  }
  const kindName = { bash: "Bash", js: "JavaScript", py: "Python" } as const;
</script>

<section class="history" aria-label="Command history">
  {#if error}<p class="err" role="alert">{error}</p>{/if}
  {#if loaded && entries.length === 0}<p class="dim">No commands recorded for this conversation yet.</p>{/if}
  <ol>
    {#each entries as entry (rowKey(entry))}
      {@const command = entry.command}
      {@const open = expanded[rowKey(entry)]}
      <li>
        <button type="button" class="row" aria-expanded={open !== undefined} onclick={() => toggle(entry)}>
          <span class="time dim" title={new Date(command.startedAt).toISOString()}>{new Date(command.startedAt).toLocaleString()}</span>
          {#if instanceId === null}<span class="pill accent">{entry.instanceName}</span>{/if}
          {#if command.agentId !== "main"}<span class="pill" title="Run by a subagent">{command.agentId}</span>{/if}
          <span class="pill">{kindName[command.kind]}</span>
          <span class="pill" class:ok={command.status === "completed"} class:err={command.status === "failed"} class:warn={command.status === "interrupted" || command.status === "cancelled" || command.status === "stopping"}>{command.status}{command.exitCode !== null && command.exitCode !== 0 ? ` (${command.exitCode})` : ""}</span>
          {#if command.description}<span class="description">{command.description}</span>{/if}
          <span class="code mono dim">{command.preview}</span>
        </button>
        {#if open}
          <div class="detail">
            {#if command.description}<p class="description">{command.description}</p>{/if}
            <p class="dim mono">{command.cwd}{command.historical ? " · imported from the conversation journal" : ""}</p>
            {#if open.error}<p class="err" role="alert">{open.error}</p>
            {:else if open.text === null}<p class="dim" role="status">Loading…</p>
            {:else}
              <pre class="mono">{open.text}</pre>
              {#if open.output !== null}
                <p class="dim output-label">Output of the failed command (last 256 KiB)</p>
                <pre class="mono failure">{stripAnsi(open.output)}</pre>
              {/if}
            {/if}
          </div>
        {/if}
      </li>
    {/each}
  </ol>
  {#if cursor}<button type="button" disabled={loadingOlder} onclick={loadOlder}>Load older</button>{/if}
</section>

<style>
  .history { height: 100%; overflow: auto; display: flex; flex-direction: column; gap: 6px; }
  ol { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 4px; }
  .row { width: 100%; display: flex; align-items: center; gap: 8px; text-align: left; overflow: hidden; white-space: nowrap; }
  .time { flex: 0 0 auto; font-size: 12px; }
  .description { flex: 0 1 auto; max-width: 40%; overflow: hidden; text-overflow: ellipsis; }
  .code { flex: 1 1 0; min-width: 0; overflow: hidden; text-overflow: ellipsis; }
  .detail { border: 1px solid var(--line); border-top: 0; padding: 8px; background: #0b1017; }
  .detail p { margin: 0 0 6px; overflow-wrap: anywhere; font-size: 12px; }
  .detail .description { max-width: none; font-size: 13px; }
  pre { margin: 0; max-height: 50vh; overflow: auto; white-space: pre; user-select: text; font-size: 12px; }
  .output-label { margin-top: 8px; }
  .failure { color: #ff9d97; border-top: 1px solid var(--line); margin-top: 6px; padding-top: 6px; }
  p { margin: 0; }
</style>
