<script lang="ts">
  import type { Instance, PlanSessionAdmission } from "../api";
  import Term from "./Term.svelte";
  import PlanPane from "./PlanPane.svelte";
  import { startInstance } from "../stores";
  let { target, planner, admission, onBack, onRetry }: {
    target: Instance; planner: Instance; admission: PlanSessionAdmission; onBack: () => void; onRetry: () => void;
  } = $props();
  const TERMINAL_MIN = 320;
  const PANE_MIN = 360;
  let ratio = $state(0.5);
  let dragging = $state(false);
  let workspace = $state<HTMLDivElement>();
  function clampRatio(value: number, width: number): number {
    if (width <= TERMINAL_MIN + PANE_MIN + 6) return TERMINAL_MIN / Math.max(width, 1);
    return Math.min((width - PANE_MIN - 6) / width, Math.max(TERMINAL_MIN / width, value));
  }
  function dragStart(event: PointerEvent): void {
    dragging = true;
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
  }
  function dragMove(event: PointerEvent): void {
    if (!dragging || !workspace) return;
    const box = workspace.getBoundingClientRect();
    ratio = clampRatio((event.clientX - box.left) / box.width, box.width);
  }
  function dragEnd(event: PointerEvent): void {
    dragging = false;
    (event.currentTarget as HTMLElement).releasePointerCapture(event.pointerId);
  }
  function dividerKey(event: KeyboardEvent): void {
    if (!workspace || (event.key !== "ArrowLeft" && event.key !== "ArrowRight")) return;
    event.preventDefault();
    ratio = clampRatio(ratio + (event.key === "ArrowRight" ? 0.03 : -0.03), workspace.getBoundingClientRect().width);
  }
</script>
<div class="view">
  <header class="panel topbar">
    <button onclick={onBack}>Back</button>
    <strong>Change plan · {target.name}</strong>
    <span class="pill">Plan agent: {planner.status}</span>
    <span class="spacer"></span>
  </header>
  {#if admission.promptSent === false}
    <p class="warn" role="alert">Context was not sent to the plan agent: {admission.error} <button onclick={onRetry}>Send context</button></p>
  {:else if admission.promptSent === null}
    <p class="warn" role="alert">{admission.error}</p>
  {/if}
  <div class="workspace" bind:this={workspace}>
    <div class="screen" style={`flex: 0 0 ${ratio * 100}%`}>
      {#if planner.generation > 0}<Term instanceId={planner.id} interactive cols={planner.cols} rows={planner.rows} generation={planner.generation} scrollback={8000} />
      {:else}
        <p class="empty">The plan agent is not running.</p>
        <button onclick={() => void startInstance(planner.id)}>Start plan agent</button>
      {/if}
    </div>
    <div class="divider" role="slider" aria-orientation="horizontal" aria-valuemin="0" aria-valuemax="100" aria-valuenow={Math.round(ratio * 100)} aria-label="Plan agent and plan width" tabindex="0" onpointerdown={dragStart} onpointermove={dragMove} onpointerup={dragEnd} onpointercancel={dragEnd} onkeydown={dividerKey}></div>
    <div class="pane-column"><PlanPane {target} /></div>
  </div>
</div>
<style>
  .view { height: 100%; display: flex; flex-direction: column; gap: 8px; min-height: 0; }
  .topbar { flex-direction: row; align-items: center; flex-wrap: wrap; padding: 10px; gap: 8px; }
  .warn { margin: 0; }
  .workspace { flex: 1; min-height: 0; min-width: 0; display: flex; }
  .screen { min-width: 320px; min-height: 0; background: #0b1017; }
  .divider { flex: 0 0 6px; cursor: col-resize; background: var(--line); touch-action: none; }
  .divider:hover, .divider:focus-visible { background: var(--accent, #4c8dff); outline: none; }
  .pane-column { flex: 1 1 0; min-width: 360px; min-height: 0; }
</style>
