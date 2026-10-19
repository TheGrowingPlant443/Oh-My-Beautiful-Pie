<script lang="ts">
  import { Handle, Position } from "@xyflow/svelte";
  import type { NodeProps } from "@xyflow/svelte";
  import type { OverviewFlowNode } from "../overview-flow";
  import { clipText, loopText, NODE_H, NODE_W } from "../system-graph";
  let { data, selected }: NodeProps<OverviewFlowNode> = $props();
  const view = $derived(data.view);
  const evidenceLabel = $derived.by(() => {
    switch (view.evidence) {
      case "analysis": return "Architect analysis";
      case "parsed": return "parsed";
      case "partial": return "partially parsed";
      case "failed": return "parse failed";
      case "unsupported": return "not parsed (unsupported)";
      case "skipped": return "skipped";
      case "none": return "";
      default: return view.evidence;
    }
  });
  const count = $derived(view.collapsed ? `${view.memberCount.toLocaleString()} ${view.memberCount === 1 ? "item" : "items"}` : view.cyclic && view.memberCount > 1 ? `${view.memberCount} in a cycle` : view.open && view.kind === "directory" && view.memberCount > 0 ? `${view.memberCount.toLocaleString()} directly inside` : "");
</script>
<Handle type="target" position={Position.Left} isConnectable={false} style="opacity:0" />
<div class={`card kind-${view.kind}`} class:selected class:matched={data.matched} class:collapsed={view.collapsed} class:open={view.open} style={`width:${NODE_W}px;height:${NODE_H}px`} role="group" aria-label={`${view.kind} ${view.label}`}>
  <div class="top">
    <span class="kind">{view.kind}</span>
    {#if evidenceLabel}<span class="evidence" class:warn={view.evidence === "partial" || view.evidence === "failed" || view.evidence === "unsupported" || view.evidence === "skipped"}>{evidenceLabel}</span>{/if}
    {#if view.selfLoops > 0}<span class="badge" title={`${view.selfLoops} ${loopText(data.loopKind ?? "recursion", view.selfLoops)}${data.loopKind === "selfLink" ? "" : " to itself"}`}>↻ {view.selfLoops}</span>{/if}
    {#if view.cyclic && view.memberCount > 1}<span class="badge" title="Strongly connected component">cycle</span>{/if}
  </div>
  <strong class="label" title={view.label}>{clipText(view.label, 60)}</strong>
  <div class="meta">
    <span class="sub" title={view.subtitle || view.within.join(" › ")}>{clipText(view.subtitle || view.within.join(" › "), 70)}</span>
    {#if count}<span>{count}</span>{/if}
    {#if view.internal > 0}<span title="Relationships entirely inside this group">{view.internal} inside</span>{/if}
  </div>
  {#if view.expandable}
    <button class="toggle nodrag nopan" aria-label={view.collapsed ? `Open ${view.label}` : `Close ${view.label}`} aria-expanded={!view.collapsed} title={view.collapsed ? "Show what is inside" : "Group again"} onclick={event => { event.stopPropagation(); data.onToggle(view.id); }}>{view.collapsed ? "+" : "−"}</button>
  {/if}
</div>
<Handle type="source" position={Position.Right} isConnectable={false} style="opacity:0" />
<style>
  .card { position: relative; box-sizing: border-box; border: 1px solid var(--line); border-radius: 8px; background: var(--panel); color: var(--text); padding: 6px 10px; display: flex; flex-direction: column; gap: 3px; cursor: pointer; overflow: hidden; }
  .card.collapsed { border-style: dashed; background: color-mix(in srgb, var(--accent) 8%, var(--panel)); }
  .card.open { border-color: var(--ok); }
  .card.matched { box-shadow: 0 0 0 2px #d2992266; }
  .card.selected { border: 2px solid var(--accent); padding: 5px 9px; box-shadow: 0 0 0 2px #4ea1ff30; }
  .card.kind-application { border-color: var(--accent); }
  .card.kind-actor, .card.kind-entrypoint { border-color: #8b9bb4; }
  .card.kind-database, .card.kind-cache, .card.kind-queue, .card.kind-storage { border-color: #d29922; }
  .card.kind-external, .card.kind-service, .card.kind-sidecar { border-color: #a371f7; }
  .top { display: flex; align-items: center; gap: 5px; min-height: 16px; }
  .kind, .evidence, .badge { font-size: 10px; padding: 0 4px; border: 1px solid var(--line); border-radius: 3px; white-space: nowrap; }
  .kind { color: var(--accent); }
  .evidence { color: var(--text-dim); overflow: hidden; text-overflow: ellipsis; }
  .evidence.warn { color: var(--warn); border-color: var(--warn); }
  .badge { color: var(--warn); }
  .label { font-size: 13px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .meta { font-size: 11px; color: var(--text-dim); display: flex; gap: 8px; align-items: center; }
  .sub { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .toggle { position: absolute; right: 6px; bottom: 6px; width: 22px; height: 22px; padding: 0; line-height: 1; }
</style>
