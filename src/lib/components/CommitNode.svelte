<script lang="ts">
  import { Handle, Position } from "@xyflow/svelte";
  import type { NodeProps } from "@xyflow/svelte";
  import type { CommitFlowNode } from "../git-graph";
  import { NODE_W, NODE_H, parseRefs } from "../git-graph";
  let { data, selected }: NodeProps<CommitFlowNode> = $props();
  const refs = $derived(parseRefs(data.commit.refs));
</script>
<Handle type="target" position={Position.Top} isConnectable={false} style="opacity:0" />
<div class="card" class:selected class:head={data.head} class:pr={!!data.pr} style={`width:${NODE_W}px;height:${NODE_H}px${data.pr ? `;--pr:${data.pr.color}` : ""}`} role="group" aria-label={`${data.commit.subject} by ${data.commit.author}`} ondblclick={() => data.onOpen(data.commit.hash)} onclickcapture={event => data.onModifierClick(event, data.commit.hash)}>
  <div class="refs">{#if data.pr?.role === "merge"}<span class="ref pr-pill" title="Merged pull request">PR</span>{/if}{#each refs.slice(0, 3) as ref}<span class={`ref ${ref.kind}`} title={ref.name}>{ref.name}</span>{/each}{#if refs.length > 3}<span class="ref">+{refs.length - 3}</span>{/if}</div>
  <strong class="subject" title={data.commit.subject}>{data.commit.subject || "(no commit message)"}</strong>
  <div class="meta"><span class="author" title={data.commit.author}>{data.commit.author}</span><code>{data.commit.hash.slice(0, 7)}</code><time datetime={data.commit.date}>{new Date(data.commit.date).toLocaleDateString()}</time></div>
</div>
<Handle type="source" position={Position.Bottom} isConnectable={false} style="opacity:0" />
<style>
  .card { box-sizing: border-box; border: 1px solid var(--line); border-radius: 8px; background: var(--panel); color: var(--text); padding: 6px 10px; display: flex; flex-direction: column; gap: 4px; cursor: pointer; }
  .card.head { border-color: var(--ok); }
  .card.pr { border-left: 5px solid var(--pr); padding-left: 6px; background: color-mix(in srgb, var(--pr) 14%, var(--panel)); }
  .card.selected { border: 2px solid var(--accent); padding: 5px 9px; box-shadow: 0 0 0 2px #4ea1ff30; }
  .card.pr.selected { border-left: 5px solid var(--pr); padding-left: 6px; }
  .pr-pill { color: var(--pr); border-color: var(--pr); font-weight: 700; }
  .refs { min-height: 17px; display: flex; gap: 4px; overflow: hidden; }
  .ref { font-size: 10px; padding: 0 4px; border: 1px solid var(--line); border-radius: 3px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .ref.head, .ref.branch { color: var(--accent); }
  .ref.remote { color: var(--text-dim); }
  .ref.tag { color: var(--warn); }
  .subject { font-size: 13px; font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .meta { font-size: 11px; color: var(--text-dim); display: flex; align-items: center; gap: 8px; }
  .author { flex: 1; min-width: 0; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  code { font-size: 10px; }
</style>
