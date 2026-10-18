<script lang="ts">
  import type { Instance } from "../api";
  import { changeAgentDirectory, rememberProjectFolder, workspaceChanges, reportError } from "../stores";
  import { basename, pickFolder } from "../ui";
  import { workspacePending } from "../workspace";
  let { instance, compact = false }: { instance: Instance; compact?: boolean } = $props();
  let busy = $state(false);
  const changes = $derived($workspaceChanges.filter(change => change.coordinatorId === instance.id));
  const active = $derived(changes.filter(workspacePending));
  const latest = $derived(changes.at(-1));
  async function choose(): Promise<void> {
    busy = true;
    try {
      const folder = await pickFolder(instance.projectPath);
      if (folder && await changeAgentDirectory(instance.id, folder, instance.generation)) rememberProjectFolder(folder, true);
    } catch (error) { reportError("Change working directory", error); }
    finally { busy = false; }
  }
</script>
<span class="directory" class:compact>
  <button class="mono" aria-label="Change working directory" title={`Change working directory: ${instance.projectPath}`} disabled={busy} onclick={choose}>{compact ? basename(instance.projectPath) : instance.projectPath}</button>
  {#each active as change (change.requestId)}
    <small role="status">{change.status === "pending" ? "Pending" : "Applying"} → {change.targetFolder}</small>
  {/each}
  {#if !active.length && latest}<small class:err={latest.status === "failed"} role="status">{latest.status === "applied" ? "Applied" : latest.status === "failed" ? `Failed: ${latest.error ?? "Working directory was not changed"}` : "Cancelled"}</small>{/if}
</span>
<style>
  .directory { display: inline-flex; flex-wrap: wrap; align-items: center; gap: 6px; min-width: 0; overflow-wrap: anywhere; }
  button { max-width: 100%; text-align: left; overflow-wrap: anywhere; }
  small { color: var(--text-dim); }
  .compact { font-size: 12px; }
</style>
