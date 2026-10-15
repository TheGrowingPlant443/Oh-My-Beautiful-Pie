<script lang="ts">
  import { onMount } from "svelte";
  import { SvelteFlow, Controls } from "@xyflow/svelte";
  import type { Edge, NodeTypes } from "@xyflow/svelte";
  import "@xyflow/svelte/dist/style.css";
  import { api } from "../api";
  import type { GitGraph } from "../api";
  import { layoutGraph, NODE_W, NODE_H, LANE_W, ROW_H } from "../git-graph";
  import type { CommitFlowNode } from "../git-graph";
  import { errorText } from "../stores";
  import { confirmDanger } from "../ui";
  import CommitNode from "./CommitNode.svelte";
  import CommitDiffView from "./CommitDiffView.svelte";

  let { instanceId, name, onBack }: { instanceId: string; name: string; onBack: () => void } = $props();
  const nodeTypes: NodeTypes = { commit: CommitNode };
  let graph = $state<GitGraph | null>(null);
  let nodes = $state.raw<CommitFlowNode[]>([]);
  let edges = $state.raw<Edge[]>([]);
  let limit = $state(300);
  let loadCount = $state(0);
  let selected = $state<string[]>([]);
  let diffRequest = $state<{ base: string | null; target: string } | null>(null);
  let loading = $state(true);
  let acting = $state(false);
  let error = $state("");
  let notice = $state("");
  let branchDialog = $state<HTMLDialogElement>();
  let branchName = $state("");
  let branchStart = $state<string | null>(null);
  let sequence = 0;
  const busy = $derived(loading || acting);
  const single = $derived(selected.length === 1 ? graph?.commits.find(c => c.hash === selected[0]) : undefined);
  const head = $derived(graph?.commits.find(c => c.hash === graph?.head));

  function openCommit(hash: string): void {
    if (!busy) diffRequest = { base: null, target: hash };
  }
  async function load(): Promise<void> {
    const seq = ++sequence;
    loading = true;
    try {
      const result = await api.getGitGraph(instanceId, limit);
      if (seq !== sequence) return;
      graph = result;
      const layout = layoutGraph(result.commits);
      nodes = result.commits.map(commit => {
        const position = layout.positions.get(commit.hash)!;
        return { id: commit.hash, type: "commit", position: { x: position.lane * LANE_W, y: position.row * ROW_H }, width: NODE_W, height: NODE_H,
          draggable: false, connectable: false, data: { commit, head: commit.hash === result.head, onOpen: openCommit } };
      });
      edges = layout.edges.map(edge => ({ ...edge, type: "smoothstep", selectable: false, focusable: false, style: "stroke:var(--accent);stroke-width:1.5" }));
      selected = [];
      loadCount++;
    } catch (e) {
      if (seq === sequence) error = [error, errorText(e)].filter(Boolean).join("\n");
    } finally {
      if (seq === sequence) loading = false;
    }
  }
  async function refresh(): Promise<void> {
    if (busy) return;
    error = "";
    await load();
  }
  function compare(): void {
    if (!graph || selected.length !== 2 || busy) return;
    const pair = graph.commits.filter(commit => selected.includes(commit.hash));
    diffRequest = { base: pair[1].hash, target: pair[0].hash };
  }
  async function mutate(action: "revert" | "pick" | "reset"): Promise<void> {
    if (busy || !graph) return;
    const commit = single;
    const current = graph.currentBranch ?? "the detached HEAD";
    let prompt: string;
    if (action === "reset") {
      if (!head?.parents.length) return;
      prompt = `Reset ${current} to ${head.parents[0].slice(0, 7)}? The latest commit ${head.hash.slice(0, 7)} "${head.subject}" is removed from the branch; its changes stay in your working tree as unstaged edits.`;
    } else {
      if (!commit) return;
      prompt = action === "revert"
        ? `Revert ${commit.hash.slice(0, 7)} "${commit.subject}"? A new commit that undoes it is added to ${current}.${commit.parents.length > 1 ? " This merge is reverted relative to its first parent." : ""}`
        : `Cherry-pick ${commit.hash.slice(0, 7)} "${commit.subject}" onto ${current}?${commit.parents.length > 1 ? " This merge is applied relative to its first parent." : ""}`;
    }
    acting = true;
    try {
      if (!await confirmDanger(prompt)) return;
      error = ""; notice = "";
      notice = action === "reset" ? await api.resetLastCommit(instanceId)
        : action === "revert" ? await api.revertCommit(instanceId, commit!.hash)
        : await api.cherryPickCommit(instanceId, commit!.hash);
    } catch (e) { error = errorText(e); }
    finally { acting = false; }
    await load();
  }
  function newBranch(): void {
    if (busy) return;
    branchName = "";
    branchStart = single?.hash ?? null;
    branchDialog?.showModal();
  }
  async function createBranch(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    if (busy || !branchName.trim()) return;
    branchDialog?.close();
    acting = true; error = ""; notice = "";
    try { notice = await api.createBranchAt(instanceId, branchName.trim(), branchStart); }
    catch (e) { error = errorText(e); }
    finally { acting = false; }
    await load();
  }
  onMount(() => { void load(); return () => { sequence++; }; });
</script>
<section class="git-tree">
  <div class="graph-page" hidden={!!diffRequest}>
    <header class="bar">
      <button disabled={acting} onclick={onBack}>Back to Code diff</button><h2>Git tree · {name}</h2>
      <span class="spacer"></span><button disabled={busy} onclick={refresh}>Refresh</button>
      {#if graph?.truncated && limit < 5000}<button disabled={busy} onclick={() => { limit = Math.min(5000, limit + 300); void refresh(); }}>Load more</button>{/if}
      <button disabled={busy || selected.length !== 2} title="Ctrl+click two commits to compare them" onclick={compare}>Compare selected</button>
      <button disabled={busy || !single} onclick={() => mutate("revert")}>Revert</button>
      <button disabled={busy || !single} onclick={() => mutate("pick")}>Cherry-pick</button>
      <button disabled={busy || !head?.parents.length} onclick={() => mutate("reset")}>Reset HEAD~1</button>
      <button disabled={busy} onclick={newBranch}>New branch</button>
    </header>
    {#if graph}<p class="meta">{graph.root} · {graph.currentBranch ?? "Detached HEAD"} · {graph.commits.length} commits{graph.truncated ? " (more history available)" : ""}</p>{/if}
    <p class="hint">Click a commit to select it. Ctrl+click two commits to compare. Double-click a commit to see its changes. Drag the canvas to pan; scroll to zoom.</p>
    {#if error}<p class="alert err" role="alert">{error}</p>{/if}
    {#if notice}<p class="notice" role="status">{notice}</p>{/if}
    {#if loading}<p role="status">Loading history…</p>{/if}
    {#if graph && !graph.commits.length}<p class="empty">This repository has no commits yet.</p>{/if}
    <div class="flow">
      {#if graph?.commits.length}
        {#key loadCount}
          <SvelteFlow bind:nodes bind:edges {nodeTypes} colorMode="dark" fitView fitViewOptions={{ nodes: nodes.slice(0, 12).map(node => ({ id: node.id })), maxZoom: 1, padding: 0.1 }} minZoom={0.2}
            nodesDraggable={false} nodesConnectable={false} deleteKey={null} zoomOnDoubleClick={false}
            onselectionchange={({ nodes: selection }) => selected = selection.map(node => node.id)}>
            <Controls showLock={false} />
          </SvelteFlow>
        {/key}
      {/if}
    </div>
  </div>
  {#if diffRequest}
    <CommitDiffView {instanceId} base={diffRequest.base} target={diffRequest.target} onBack={() => diffRequest = null} />
  {/if}
</section>
<dialog bind:this={branchDialog} aria-labelledby="branch-title">
  <form onsubmit={createBranch}>
    <h3 id="branch-title">Create and switch to a new branch</h3>
    <p>Start at {branchStart?.slice(0, 7) ?? "HEAD"}</p>
    <label>Branch name<input aria-label="New branch name" bind:value={branchName} placeholder="topic-name" /></label>
    <div class="actions"><button type="button" onclick={() => branchDialog?.close()}>Cancel</button><button class="primary" disabled={busy || !branchName.trim()}>Create</button></div>
  </form>
</dialog>
<style>
  .git-tree, .graph-page { height: 100%; min-height: 0; display: flex; flex-direction: column; gap: 10px; }
  .graph-page[hidden] { display: none; }
  .bar { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; }
  h2 { margin: 0; font-size: 18px; }
  .meta, .hint { margin: 0; color: var(--text-dim); font-size: 12px; overflow-wrap: anywhere; }
  .flow { flex: 1; min-height: 0; border: 1px solid var(--line); border-radius: 8px; overflow: hidden; }
  .alert, .notice { margin: 0; padding: 10px; border: 1px solid var(--err); border-radius: 8px; white-space: pre-wrap; }
  .notice { border-color: var(--ok); }
  :global(.svelte-flow__attribution a) { pointer-events: none; }
  dialog { background: var(--panel); color: var(--text); border: 1px solid var(--line); border-radius: 10px; padding: 18px; }
  dialog::backdrop { background: #0008; }
  h3 { margin-top: 0; }
  label { display: flex; flex-direction: column; gap: 6px; }
  .actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 16px; }
</style>
