<script lang="ts">
  import { untrack } from "svelte";
  import { api } from "../api";
  import { allInstances, reportError, terminalOwners, terminalUi, updateTerminalUi } from "../stores";
  import { defaultTerminalUi, GLOBAL_UI_KEY, HISTORY_KEY, nextSelection, ownerStatus, terminalTabs } from "../terminal";
  import CommandHistory from "./CommandHistory.svelte";
  import CommandOutput from "./CommandOutput.svelte";
  import UserTerminal from "./UserTerminal.svelte";

  // instanceId: one agent's pane next to its terminal. null: every agent (the Dashboard's Terminals page).
  let { instanceId }: { instanceId: string | null } = $props();
  const uiKey = $derived(instanceId ?? GLOBAL_UI_KEY);
  const ui = $derived($terminalUi.get(uiKey) ?? defaultTerminalUi());
  const owners = $derived(instanceId === null
    ? $allInstances.flatMap(row => $terminalOwners.get(row.id) ?? [])
    : [$terminalOwners.get(instanceId)].filter(owner => owner !== undefined));
  const tabs = $derived(terminalTabs(instanceId, owners));
  const notices = $derived(owners.map(owner => ({ owner, text: ownerStatus(owner) })).filter(item => item.text !== null));
  const selected = $derived(tabs.find(tab => tab.key === ui.tab) ?? tabs.find(tab => tab.key === HISTORY_KEY)!);
  let known = new Set(untrack(() => tabs).map(tab => tab.key));

  // Keep the selection valid, and select a newly started command only while Follow is on.
  $effect(() => {
    const next = nextSelection(tabs, known, ui.tab, ui.follow);
    known = new Set(tabs.map(tab => tab.key));
    if (next !== ui.tab) updateTerminalUi(uiKey, { tab: next });
  });

  async function closeShell(owner: string, shellId: string): Promise<void> {
    updateTerminalUi(uiKey, { tab: HISTORY_KEY });
    try { await api.closeUserTerminal(owner, shellId); }
    catch (error) { reportError("Close terminal", error); }
  }
  const ownerOf = (id: string | null) => owners.find(owner => owner.instanceId === id);
</script>

<section class="workspace" aria-label="Terminal">
  <div class="tabs" role="tablist">
    {#each tabs as tab (tab.key)}
      {@const shell = tab.kind === "shell" ? ownerOf(tab.instanceId)?.userShell : null}
      <span class="tab" class:active={tab.key === selected.key}>
        <button type="button" role="tab" aria-selected={tab.key === selected.key} title={tab.title} onclick={() => updateTerminalUi(uiKey, { tab: tab.key })}>{tab.label}</button>
        {#if shell && tab.instanceId}<button type="button" class="close ghost" aria-label="Close terminal" title="Close this terminal and end its shell" onclick={() => closeShell(tab.instanceId!, shell.shellId)}>×</button>{/if}
      </span>
    {/each}
    <span class="spacer"></span>
    <button type="button" class:active={ui.follow} aria-pressed={ui.follow} title="Switch to a command's tab when the AI starts it" onclick={() => updateTerminalUi(uiKey, { follow: !ui.follow })}>Follow</button>
  </div>
  {#each notices as notice (notice.owner.instanceId)}
    <p class:err={!notice.owner.held} class:warn={notice.owner.held} role="status">{instanceId === null ? `${notice.owner.name}: ` : ""}{notice.text}</p>
  {/each}
  <div class="body">
    {#key selected.key}
      {#if selected.kind === "history"}
        <CommandHistory {instanceId} />
      {:else if selected.kind === "shell"}
        {@const owner = ownerOf(selected.instanceId)}
        {#if owner?.userShell}<UserTerminal instanceId={owner.instanceId} shell={owner.userShell} />{/if}
      {:else}
        {@const owner = ownerOf(selected.instanceId)}
        {#if owner && selected.command}<CommandOutput {owner} command={selected.command} />{/if}
      {/if}
    {/key}
  </div>
</section>

<style>
  .workspace { height: 100%; min-height: 0; min-width: 0; display: flex; flex-direction: column; gap: 6px; padding: 8px; background: #101722; border: 1px solid var(--line); }
  .tabs { display: flex; align-items: center; gap: 4px; flex-wrap: wrap; }
  .tab { display: inline-flex; align-items: center; max-width: 28ch; border: 1px solid var(--line); border-radius: 6px; overflow: hidden; }
  .tab.active { border-color: var(--accent); background: #22406b; }
  .tab button[role="tab"] { border: 0; background: transparent; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0; }
  .close { border: 0; padding: 0 8px; }
  .spacer { flex: 1; }
  .body { flex: 1; min-height: 0; min-width: 0; }
  p { margin: 0; font-size: 12px; overflow-wrap: anywhere; }
  p.warn { color: #e8c46b; }
</style>
