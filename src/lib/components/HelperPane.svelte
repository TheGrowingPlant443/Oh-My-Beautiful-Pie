<script lang="ts">
  import type { HelperAdmission, Instance } from "../api";
  import { startInstance } from "../stores";
  import Term from "./Term.svelte";
  // The read-only helper agent of a project (Code diff: explain / review; Git tree: history search), as a live terminal.
  let { helper, admission, onRetry }: { helper: Instance | undefined; admission: HelperAdmission | null; onRetry: () => void } = $props();
</script>
<div class="pane">
  {#if admission?.promptSent === false}
    <p class="warn" role="alert">The helper did not receive its request: {admission.error} <button onclick={onRetry}>Send again</button></p>
  {:else if admission?.promptSent === null}
    <p class="warn" role="alert">{admission.error}</p>
  {/if}
  <div class="screen">
    {#if helper && helper.generation > 0}
      <Term instanceId={helper.id} interactive cols={helper.cols} rows={helper.rows} generation={helper.generation} scrollback={8000} />
    {:else}
      <p class="empty">The helper is not running.</p>
      {#if helper}<button onclick={() => void startInstance(helper.id)}>Start helper</button>{/if}
    {/if}
  </div>
</div>
<style>
  .pane { height: 100%; min-height: 0; display: flex; flex-direction: column; gap: 8px; }
  .warn { margin: 0; }
  .screen { flex: 1; min-height: 0; background: #0b1017; border: 1px solid var(--line); border-radius: 8px; overflow: hidden; }
</style>
