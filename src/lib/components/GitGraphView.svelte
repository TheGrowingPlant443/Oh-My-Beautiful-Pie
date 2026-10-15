<script lang="ts">
  import { onMount } from "svelte";
  import { SvelteFlow, Controls } from "@xyflow/svelte";
  import type { Edge, NodeTypes } from "@xyflow/svelte";
  import "@xyflow/svelte/dist/style.css";
  import { api } from "../api";
  import type { GitGraph, HelperAdmission } from "../api";
  import { layoutGraph, legendOf, mergedPullRequests, prEdgeColor, INITIAL_FIT_NODES, NODE_W, NODE_H, LANE_W, ROW_H } from "../git-graph";
  import type { CommitFlowNode, PullRequestBranch } from "../git-graph";
  import { errorText, instancesById } from "../stores";
  import { confirmDanger } from "../ui";
  import CommitNode from "./CommitNode.svelte";
  import CommitDiffView from "./CommitDiffView.svelte";
  import HelperPane from "./HelperPane.svelte";

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
  let prs = $state<PullRequestBranch[]>([]);
  const legend = $derived(legendOf(prs));
  // Git helper: a read-only agent that searches the history replaces the tree; the tree stays mounted behind it.
  let agent = $state<{ id: string; admission: HelperAdmission } | null>(null);
  let helperOpen = $state(false);
  let opening = $state(false);
  const helper = $derived(agent ? $instancesById.get(agent.id) : undefined);
  const prByMerge = $derived(new Map(prs.map(pr => [pr.merge, pr] as const)));
  let explain = $state<{ title: string; x: number; y: number; status: "loading" | "done" | "error"; text: string } | null>(null);
  const explained = new Map<string, string>();
  let explainSeq = 0;

  const shortHash = (hash: string): string => hash.slice(0, 7);
  function closeExplain(): void { explainSeq++; explain = null; }
  async function showExplanation(event: MouseEvent, kind: "pr" | "commit" | "range", base: string | null, target: string, title: string): Promise<void> {
    const seq = ++explainSeq;
    const key = `${kind}:${base ?? ""}:${target}`;
    const x = Math.max(8, Math.min(event.clientX, window.innerWidth - 372));
    const y = Math.max(8, Math.min(event.clientY + 12, window.innerHeight - 160));
    const cached = explained.get(key);
    if (cached !== undefined) { explain = { title, x, y, status: "done", text: cached }; return; }
    explain = { title, x, y, status: "loading", text: "Explaining…" };
    try {
      const text = await api.explainGitChange(instanceId, kind, base, target);
      explained.set(key, text);
      if (seq === explainSeq && explain) explain = { ...explain, status: "done", text };
    } catch (e) {
      if (seq === explainSeq && explain) explain = { ...explain, status: "error", text: errorText(e) };
    }
  }
  // Ctrl+Shift+click: the difference between the two selected commits, a merged PR (its merge commit), or a commit.
  // It stops the click before the node's own selection handler; plain Ctrl+click stays a normal multi-select.
  function modifierClick(event: MouseEvent, hash: string): void {
    if (!(event.ctrlKey || event.metaKey) || !event.shiftKey || !graph) return;
    event.preventDefault();
    event.stopPropagation();
    const pr = prByMerge.get(hash);
    if (selected.length === 2 && selected.includes(hash)) {
      const pair = graph.commits.filter(commit => selected.includes(commit.hash));
      void showExplanation(event, "range", pair[1].hash, pair[0].hash, `${shortHash(pair[1].hash)}..${shortHash(pair[0].hash)}`);
    } else if (pr) {
      void showExplanation(event, "pr", null, pr.merge, pr.subject);
    } else {
      const commit = graph.commits.find(candidate => candidate.hash === hash);
      void showExplanation(event, "commit", null, hash, `${shortHash(hash)} ${commit?.subject ?? ""}`.trim());
    }
  }

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
      prs = mergedPullRequests(result.commits, result.mainCommit);
      const marks = new Map<string, { color: string; role: "member" | "merge" }>();
      for (const pr of prs) {
        for (const hash of pr.members) marks.set(hash, { color: pr.color, role: "member" });
        marks.set(pr.merge, { color: pr.color, role: "merge" });
      }
      nodes = result.commits.map(commit => {
        const position = layout.positions.get(commit.hash)!;
        return { id: commit.hash, type: "commit", position: { x: position.lane * LANE_W, y: position.row * ROW_H }, width: NODE_W, height: NODE_H,
          draggable: false, connectable: false, data: { commit, head: commit.hash === result.head, onOpen: openCommit, pr: marks.get(commit.hash) ?? null, onModifierClick: modifierClick } };
      });
      const edgeColor = prEdgeColor(prs);
      edges = layout.edges.map(edge => {
        const color = edgeColor(edge.source, edge.target);
        return { ...edge, type: "smoothstep", selectable: false, focusable: false, style: `stroke:${color ?? "var(--accent)"};stroke-width:${color ? 2.5 : 1.5}` };
      });
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
  async function openHelper(): Promise<void> {
    if (opening) return;
    opening = true;
    try {
      const admission = await api.openHelper(instanceId, "git", null, null);
      agent = { id: admission.helper.id, admission };
      closeExplain();
      helperOpen = true;
    } catch (e) {
      error = [error, errorText(e)].filter(Boolean).join("\n");
    } finally {
      opening = false;
    }
  }
  async function toggleHelper(): Promise<void> {
    if (helperOpen) helperOpen = false;
    else await openHelper();
  }
  onMount(() => { void load(); return () => { sequence++; }; });
</script>
<svelte:window onkeydown={event => { if (event.key === "Escape" && explain) closeExplain(); }} />
<section class="git-tree">
  <div class="graph-page" hidden={!!diffRequest}>
    <header class="bar">
      <button disabled={acting} onclick={onBack}>Back to Code diff</button><h2>Git tree · {name}</h2>
      <span class="spacer"></span><button disabled={busy} onclick={refresh}>Refresh</button>
      <button aria-pressed={helperOpen} class:active={helperOpen} disabled={opening} title="Ask a read-only agent to search this repository's history" onclick={toggleHelper}>Git helper</button>
      {#if graph?.truncated && limit < 5000}<button disabled={busy} onclick={() => { limit = Math.min(5000, limit + 300); void refresh(); }}>Load more</button>{/if}
      <button disabled={busy || selected.length !== 2} title="Ctrl+click two commits to compare them" onclick={compare}>Compare selected</button>
      <button disabled={busy || !single} onclick={() => mutate("revert")}>Revert</button>
      <button disabled={busy || !single} onclick={() => mutate("pick")}>Cherry-pick</button>
      <button disabled={busy || !head?.parents.length} onclick={() => mutate("reset")}>Reset HEAD~1</button>
      <button disabled={busy} onclick={newBranch}>New branch</button>
    </header>
    <div class="tree-body" hidden={helperOpen}>
    {#if graph}<p class="meta">{graph.root} · {graph.currentBranch ?? "Detached HEAD"} · {graph.commits.length} commits{graph.truncated ? " (more history available)" : ""}</p>{/if}
    {#if legend.length}
      <ul class="legend" aria-label="Pull request branch colours">
        {#each legend as entry}<li><span class="swatch" style:background={entry.color}></span>{entry.label}</li>{/each}
      </ul>
    {/if}
    <p class="hint">Click a commit to select it. Ctrl+click to select several, then Compare selected. Double-click a commit to see its changes. Ctrl+Shift+click a commit for a one-line summary of it, a merged pull request's merge commit (the PR badge) for a summary of the whole PR, or, with two commits selected, one of them for a summary of the difference between them. Drag to pan; scroll to zoom.</p>
    {#if error}<p class="alert err" role="alert">{error}</p>{/if}
    {#if notice}<p class="notice" role="status">{notice}</p>{/if}
    {#if loading}<p role="status">Loading history…</p>{/if}
    {#if graph && !graph.commits.length}<p class="empty">This repository has no commits yet.</p>{/if}
    <div class="flow">
      {#if graph?.commits.length}
        {#key loadCount}
          <SvelteFlow bind:nodes bind:edges {nodeTypes} colorMode="dark" fitView fitViewOptions={{ nodes: nodes.slice(0, INITIAL_FIT_NODES).map(node => ({ id: node.id })), maxZoom: 1, padding: 0.1 }} minZoom={0.2}
            nodesDraggable={false} nodesConnectable={false} deleteKey={null} zoomOnDoubleClick={false}
            onselectionchange={({ nodes: selection }) => selected = selection.map(node => node.id)}>
            <Controls showLock={false} />
          </SvelteFlow>
        {/key}
      {/if}
    </div>
    </div>
    {#if helperOpen && agent}
      <div class="flow"><HelperPane {helper} admission={agent.admission} onRetry={openHelper} /></div>
    {/if}
  </div>
  {#if diffRequest}
    <CommitDiffView {instanceId} base={diffRequest.base} target={diffRequest.target} onBack={() => diffRequest = null} />
  {/if}
  {#if explain}
    <div class="explain" role="dialog" aria-label="Explanation" style:left={`${explain.x}px`} style:top={`${explain.y}px`}>
      <strong>{explain.title}</strong>
      <p class:err={explain.status === "error"} role="status">{explain.text}</p>
      <button type="button" onclick={closeExplain}>Close</button>
    </div>
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
  .tree-body { flex: 1; min-height: 0; display: flex; flex-direction: column; gap: 10px; }
  .tree-body[hidden] { display: none; }
  .flow { flex: 1; min-height: 0; border: 1px solid var(--line); border-radius: 8px; overflow: hidden; }
  .alert, .notice { margin: 0; padding: 10px; border: 1px solid var(--err); border-radius: 8px; white-space: pre-wrap; }
  .legend { list-style: none; margin: 0; padding: 0; display: flex; flex-wrap: wrap; gap: 4px 16px; font-size: 12px; }
  .legend li { display: inline-flex; align-items: center; gap: 6px; }
  .swatch { width: 12px; height: 12px; border-radius: 3px; flex: none; }
  .explain { position: fixed; z-index: 50; width: 360px; padding: 10px 12px; display: flex; flex-direction: column; gap: 6px; background: var(--panel); border: 1px solid var(--line); border-radius: 8px; box-shadow: 0 8px 30px #0008; }
  .explain p { margin: 0; overflow-wrap: anywhere; }
  .explain strong { overflow-wrap: anywhere; font-size: 12px; }
  .explain button { align-self: flex-end; }
  .notice { border-color: var(--ok); }
  :global(.svelte-flow__attribution a) { pointer-events: none; }
  dialog { background: var(--panel); color: var(--text); border: 1px solid var(--line); border-radius: 10px; padding: 18px; }
  dialog::backdrop { background: #0008; }
  h3 { margin-top: 0; }
  label { display: flex; flex-direction: column; gap: 6px; }
  .actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 16px; }
</style>
