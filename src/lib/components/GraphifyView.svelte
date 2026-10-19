<script module lang="ts">
  import { knowledgeApi } from "../knowledge";
  import { IndexClient, type IndexWorkerLike } from "../knowledge-index";
  import IndexWorker from "../knowledge-index.worker?worker";
  import { createGraphifyStore } from "../graphify-view";

  // Parent-lived: leaving and returning keeps the admitted artifact, the search and the camera. Re-admission happens only on Refresh.
  const graphify = createGraphifyStore({ api: knowledgeApi, indexClient: new IndexClient(() => new IndexWorker() as unknown as IndexWorkerLike) });
</script>
<script lang="ts">
  import { get } from "svelte/store";
  import { refKey, REPLACED_TEXT, type GraphifyState } from "../graphify-view";
  import type { GraphifyLink, GraphifyNode, GraphifySearchItem } from "../knowledge";
  import { graphifyNodeId } from "../knowledge-graph";
  import type { GraphIndex, ViewEdge, ViewNode } from "../system-graph";
  import KnowledgeGraph from "./KnowledgeGraph.svelte";

  // Read-only view of the project's Graphify output as one immutable artifact: IDs, directedness, parallel edges, self-links,
  // communities, provenance and source locations are kept exactly as found. Graphify itself is never run and the repository's own
  // `graph.html` (which loads remote scripts) is never opened. An artifact hash proves byte identity, not freshness of the source.
  let { folder, onBack }: { folder: string; onBack: () => void } = $props();

  // Store snapshots are replaced, never mutated, and hold whole graphs: raw state avoids proxying them.
  let view = $state.raw<GraphifyState>(get(graphify));
  $effect(() => {
    const stop = graphify.subscribe(value => { view = value; });
    return () => { stop(); graphify.suspend(); };
  });
  let started = "";
  $effect(() => {
    const target = folder;
    if (started === target) return;
    started = target;
    const known = get(graphify);
    // Returning to the folder already open keeps its artifact; anything else is admitted anew (an explicit click got us here).
    if (known.folder !== target || known.status === "idle" || known.status === "error") void graphify.open(target);
  });
  $effect(() => {
    const onFocus = (): void => { void graphify.check(); };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  });

  const nodeByModelId = $derived(new Map<string, GraphifyNode>(view.nodes.map(item => [graphifyNodeId(item.ref), item])));
  const linkById = $derived(new Map<string, GraphifyLink>(view.links.map(item => [item.id, item])));
  const hyperedgeById = $derived(new Map(view.hyperedges.map(item => [item.id, item])));
  const groupMembers = $derived(new Set((view.selectedGroup ? hyperedgeById.get(view.selectedGroup)?.nodeIds ?? [] : []).map(graphifyNodeId)));
  const selectedGroup = $derived(view.selectedGroup ? hyperedgeById.get(view.selectedGroup) ?? null : null);
  let groupLimit = $state(200);
  let selectedRecord = $state.raw<GraphifyLink | null>(null);
  $effect(() => {
    void view.artifact?.artifactId;
    groupLimit = 200;
    selectedRecord = null;
  });
  let text = $state("");
  let timer: ReturnType<typeof setTimeout> | null = null;
  function typed(value: string): void {
    text = value;
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => void graphify.setSearch(value), 250);
  }
  $effect(() => () => { if (timer) clearTimeout(timer); });
  const hits = $derived(new Set(view.search.items.flatMap(item => {
    switch (item.kind) {
      case "node": return [graphifyNodeId(item.node.ref)];
      case "link": return [graphifyNodeId(item.link.source), graphifyNodeId(item.link.target)];
      case "hyperedge": return item.hyperedge.nodeIds.map(graphifyNodeId);
    }
  })));
  const matched = $derived(new Set([...hits, ...groupMembers]));
  let selectedNode = $state<string | null>(null);
  let selectedEdge = $state<string | null>(null);
  let focus = $state<{ id: string; seq: number } | null>(null);
  let focusSeq = 0;

  function pick(selection: { node: string | null; edge: string | null }): void {
    selectedRecord = null;
    graphify.selectGroup(null);
    selectedNode = selection.node;
    selectedEdge = selection.edge;
    const found = selection.node === null ? undefined : nodeByModelId.get(selection.node);
    void graphify.inspect(found ? found.ref : null);
  }
  function show(item: GraphifyNode): void { focus = { id: graphifyNodeId(item.ref), seq: ++focusSeq }; }
  function showSearch(item: GraphifySearchItem): void {
    selectedNode = null; selectedEdge = null; selectedRecord = null;
    graphify.selectGroup(null);
    switch (item.kind) {
      case "node": show(item.node); break;
      case "link": selectedRecord = item.link; break;
      case "hyperedge": graphify.selectGroup(item.hyperedge.id); break;
    }
  }
  function showGroup(id: string): void {
    selectedRecord = null; selectedNode = null; selectedEdge = null;
    graphify.selectGroup(view.selectedGroup === id ? null : id);
  }
  const label = (id: GraphifyLink["source"]): string => nodeByModelId.get(graphifyNodeId(id))?.label ?? id.id;
  const clip = (value: unknown, max = 1500): string => { const textual = JSON.stringify(value, null, 1) ?? ""; return textual.length > max ? `${textual.slice(0, max)}\n… (${(textual.length - max).toLocaleString()} more characters in the artifact)` : textual; };
  const sourceAt = (item: { sourceFile: string | null; sourceLocation: string | null }): string => item.sourceFile ? `${item.sourceFile}${item.sourceLocation ? `:${item.sourceLocation}` : ""}` : "no source location";
  const loading = $derived(view.status === "opening" || view.status === "loading");
  const metadata = $derived(Object.entries(view.artifact?.metadata ?? {}).slice(0, 40));
</script>
<section class="gfy" aria-label="Graphify">
  <header class="bar panel">
    <button onclick={onBack}>Back to System overview</button>
    <h2>Graphify</h2>
    {#if view.artifact}
      <span class="badge" class:warn={view.artifact.provenance !== "verified"} title={view.artifact.sourceRoot ?? "no .graphify_root"}>{view.artifact.provenance === "verified" ? "Provenance verified" : "Provenance unverified"}</span>
      <span class="badge">{view.artifact.directed ? "directed" : "undirected"}{view.artifact.multigraph ? " · multigraph" : ""}</span>
      <span class="badge" title={view.artifact.artifactHash}>hash {view.artifact.artifactHash.slice(0, 12)}</span>
    {/if}
    <span class="grow"></span>
    <button disabled={loading} onclick={() => void graphify.open(folder)}>Refresh</button>
  </header>
  <p class="dim note">Read-only view of the saved Graphify output. Nothing here runs Graphify or a model. The hash proves which bytes are shown, not that they match the current source code.</p>
  {#if view.stale}<p class="banner warn" role="status">{view.stale === REPLACED_TEXT ? REPLACED_TEXT : view.stale}
    <button onclick={() => void graphify.open(folder)}>Refresh</button> <span class="dim">What is shown stays the build you opened.</span></p>{/if}
  {#if view.status === "error"}<p class="banner err" role="alert">{view.error}</p>{/if}
  {#if loading}
    <p class="dim" role="status">Reading the output… {view.progress.nodes.loaded.toLocaleString()} of {view.progress.nodes.total.toLocaleString()} nodes · {view.progress.links.loaded.toLocaleString()} of {view.progress.links.total.toLocaleString()} links · {view.progress.hyperedges.loaded.toLocaleString()} of {view.progress.hyperedges.total.toLocaleString()} groups</p>
  {/if}
  {#if view.artifact && view.artifact.issueCount > 0}
    <details class="banner warn"><summary>{view.artifact.issueCount.toLocaleString()} validation finding{view.artifact.issueCount === 1 ? "" : "s"} in this output</summary>
      <ul>{#each view.artifact.issues.slice(0, 50) as issue, i (i)}<li><strong>{issue.severity}</strong> · {issue.section}{issue.ordinal !== null ? ` #${issue.ordinal}` : ""}: {issue.message}</li>{/each}</ul>
      {#if view.artifact.issueCount > Math.min(50, view.artifact.issues.length)}<p class="dim">{(view.artifact.issueCount - Math.min(50, view.artifact.issues.length)).toLocaleString()} more findings are not listed here.</p>{/if}</details>
  {/if}
  {#if view.artifact && view.artifact.provenance !== "verified"}
    <p class="banner warn">This output has no matching <code>.graphify_root</code>, so it is not proven to describe this project.</p>
  {/if}
  {#if view.result}
    <p class="dim stats">{view.result.stats.nodes.toLocaleString()} nodes · {view.result.stats.links.toLocaleString()} links ({Object.entries(view.result.stats.confidence).map(([name, count]) => `${count.toLocaleString()} ${name}`).join(", ")}) · {view.result.stats.communities.toLocaleString()} communities · {view.result.stats.hyperedges.toLocaleString()} groups{view.result.stats.dangling ? ` · ${view.result.stats.dangling.toLocaleString()} links point at missing nodes (kept, not drawn)` : ""}</p>
  {/if}
  <div class="body">
    <aside class="side" aria-label="Search and groups">
      <input type="search" placeholder="Search nodes, relationships and groups" value={text} oninput={event => typed((event.currentTarget as HTMLInputElement).value)} aria-label="Search Graphify records" disabled={!view.result} />
      {#if view.search.status === "loading" && view.search.items.length === 0}<p class="dim" role="status">Searching…</p>
      {:else if view.search.status === "error"}<p class="err" role="alert">{view.search.error}</p>
      {:else if view.search.status === "ready"}
        <p class="dim count">{view.search.items.length.toLocaleString()} of {view.search.total.toLocaleString()} matches</p>
        <ul class="list">{#each view.search.items as item (item.kind === "node" ? `node:${refKey(item.node.ref)}` : item.kind === "link" ? `link:${item.link.id}` : `group:${item.hyperedge.id}`)}
          <li><button class="row" onclick={() => showSearch(item)}>
            {#if item.kind === "node"}<strong>{item.node.label}</strong><small class="dim">Node · {item.node.ref.idKind} id {item.node.ref.id}</small>
            {:else if item.kind === "link"}<strong>{label(item.link.source)} {item.link.directed ? "→" : "—"} {label(item.link.target)}</strong><small class="dim">Relationship · {item.link.relation} · {item.link.id}</small>
            {:else}<strong>{item.hyperedge.label || item.hyperedge.id}</strong><small class="dim">Group · {item.hyperedge.nodeIds.length.toLocaleString()} members · {item.hyperedge.id}</small>{/if}
          </button></li>
        {/each}</ul>
        {#if view.search.nextOffset !== null}<button onclick={() => void graphify.searchMore()}>Show more matches</button>{/if}
      {/if}
      {#if view.hyperedges.length}
        <h4>Groups ({view.hyperedges.length.toLocaleString()})</h4>
        <p class="dim count">A group ties several nodes together. It is listed here and highlights its members; it is never drawn as lines between them.</p>
        <ul class="list">{#each view.hyperedges.slice(0, groupLimit) as group (group.id)}
          <li><button class="row" class:active={view.selectedGroup === group.id} onclick={() => showGroup(group.id)}>
            <strong>{group.label || group.id}</strong><small class="dim">{group.nodeIds.length.toLocaleString()} nodes{group.confidence ? ` · ${group.confidence}` : ""}{group.relation ? ` · ${group.relation}` : ""}</small></button></li>
        {/each}</ul>
        {#if view.hyperedges.length > groupLimit}<button onclick={() => { groupLimit += 200; }}>Show more groups ({(view.hyperedges.length - groupLimit).toLocaleString()} remaining)</button>{/if}
      {/if}
      {#if metadata.length}
        <details><summary>Build metadata</summary>
          <dl class="meta">{#each metadata as [name, value] (name)}<dt>{name}</dt><dd>{typeof value === "string" ? value : clip(value, 200)}</dd>{/each}</dl></details>
      {/if}
      {#if view.artifact?.metadataReceipt}
        {@const receipt = view.artifact.metadataReceipt}
        <p class="banner warn" role="status">{receipt.message} Complete metadata: {receipt.bytes.toLocaleString()} bytes, SHA-256 <code>{receipt.sha256.slice(0, 16)}…</code>, kept in the app's private copy (not opened or executed here).</p>
      {/if}
    </aside>
    <div class="graph">
      {#if view.result}
        <KnowledgeGraph model={view.result.model} loopKind="selfLink" label="Graphify graph" layoutKey={view.artifact?.artifactId ?? ""}
          {selectedNode} {selectedEdge} {matched} camera={view.camera} {focus} empty="This Graphify output has no nodes."
          onSelect={pick} onCamera={camera => graphify.setCamera(camera)}>
          {#snippet inspector({ node, edge }: { node: ViewNode | null; edge: ViewEdge | null; projection: unknown; index: GraphIndex })}
            <div class="inspect" aria-label="Details">
              {#if selectedRecord}
                <h4>Exact relationship record · {selectedRecord.id}</h4>
                <p>{label(selectedRecord.source)} {selectedRecord.directed ? "→" : "—"} {label(selectedRecord.target)}</p>
                <p class="dim">{selectedRecord.relation} · {selectedRecord.confidence ?? "no confidence"} · {sourceAt(selectedRecord)}</p>
                <details><summary>Complete original record and attributes</summary><pre>{JSON.stringify(selectedRecord, null, 2)}</pre></details>
              {:else if selectedGroup}
                <h4>Group · {selectedGroup.label || selectedGroup.id}</h4>
                <p class="dim">{selectedGroup.nodeIds.length.toLocaleString()} members · {selectedGroup.relation ?? "no relation"} · {selectedGroup.confidence ?? "no confidence"}. No dependency lines are invented between its members.</p>
                <ul>{#each selectedGroup.nodeIds as member, ordinal (ordinal)}
                  {@const found = nodeByModelId.get(graphifyNodeId(member))}
                  <li>{#if found}<button onclick={() => show(found)}>{found.label}</button>{:else}<span class="dim">Missing node: {member.idKind} {member.id}</span>{/if}</li>
                {/each}</ul>
                <details><summary>Complete group record and attributes</summary><pre>{JSON.stringify(selectedGroup, null, 2)}</pre></details>
              {:else if edge}
                {@const ids = edge.edgeIds.slice(0, 50)}
                <h4>{edge.relation} · {edge.count.toLocaleString()} record{edge.count === 1 ? "" : "s"}</h4>
                <p class="dim">{edge.direction === "directed" ? "Directed: source → target as recorded." : "Undirected: drawn as a plain line; each record keeps the stored source and target."}</p>
                {#each ids as id (id)}
                  {@const link = linkById.get(id)}
                  {#if link}
                    <div class="record">
                      <strong>{label(link.source)} {link.directed ? "→" : "—"} {label(link.target)}</strong>
                      <small class="dim">{link.relation} · {link.confidence ?? "no confidence"}{link.confidenceScore !== null ? ` (${link.confidenceScore})` : ""} · {link.directed ? "directed" : "undirected"}{link.key !== null ? ` · key ${link.key}` : ""} · record #{link.ordinal}{link.nativeId ? ` · id ${link.nativeId}` : ""}{link.origin ? ` · origin ${link.origin}` : ""}</small>
                      <small class="dim">{sourceAt(link)}</small>
                      {#if Object.keys(link.attributes).length}<details><summary>All attributes</summary><pre>{clip(link.attributes)}</pre></details>{/if}
                    </div>
                  {/if}
                {/each}
                {#if edge.edgeIds.length > ids.length}<p class="dim">{(edge.edgeIds.length - ids.length).toLocaleString()} more records are behind this line.</p>{/if}
              {:else if node}
                {@const item = node.ref ? nodeByModelId.get(node.ref) : undefined}
                <h4>{node.label}</h4>
                <p class="dim">{node.kind}{node.collapsed ? ` · ${node.memberCount.toLocaleString()} nodes inside` : ""}{node.within.length ? ` · in ${node.within.join(" › ")}` : ""}</p>
                {#if node.selfLoops > 0}<p class="dim">↻ {node.selfLoops.toLocaleString()} Self-link{node.selfLoops === 1 ? "" : "s"} (an edge from this node to itself).</p>{/if}
                {#if item}
                  <p class="dim">{item.ref.idKind === "number" ? "numeric id" : "text id"} <code>{item.ref.id}</code>{item.community !== null ? ` · community ${item.community}` : ""}{item.origin ? ` · origin ${item.origin}` : ""}</p>
                  <p class="dim">{sourceAt(item)}</p>
                  {#if Object.keys(item.attributes).length}<details><summary>All attributes</summary><pre>{clip(item.attributes)}</pre></details>{/if}
                  <p class="dim">Links of this node{view.neighbors.status === "ready" ? ` (${view.neighbors.total.toLocaleString()})` : ""}</p>
                  {#if view.neighbors.status === "loading" && view.neighbors.items.length === 0}<p class="dim">Looking…</p>
                  {:else if view.neighbors.status === "error"}<p class="err" role="alert">{view.neighbors.error}</p>
                  {:else}
                    <ul class="list">{#each view.neighbors.items as link (link.id)}
                      <li><span class="dim">{refKey(link.source) === refKey(item.ref) ? "→" : "←"}</span> {label(refKey(link.source) === refKey(item.ref) ? link.target : link.source)}
                        <small class="dim">{link.relation} · {link.confidence ?? "no confidence"} · {link.directed ? "directed" : "undirected"}{link.dangling ? " · missing endpoint" : ""}{refKey(link.source) === refKey(link.target) ? " · Self-link" : ""}</small></li>
                    {/each}</ul>
                    {#if view.neighbors.nextOffset !== null}<button onclick={() => void graphify.inspect(item.ref, true)}>Show more links</button>{/if}
                    {#if view.neighbors.groups.length}
                      <p class="dim">In groups: {view.neighbors.groups.map(group => group.label || group.id).join(", ")}</p>
                    {/if}
                  {/if}
                {/if}
              {:else}
                <p class="dim">Select a node or a line. Nothing here starts Graphify or an agent.</p>
              {/if}
            </div>
          {/snippet}
        </KnowledgeGraph>
      {:else if !loading && view.status !== "error"}
        <p class="dim empty">Nothing has been read yet.</p>
      {/if}
    </div>
  </div>
</section>
<style>
  .gfy { height: 100%; min-height: 0; display: flex; flex-direction: column; gap: 6px; }
  .bar { display: flex; flex-direction: row; flex-wrap: wrap; align-items: center; gap: 8px; padding: 8px 12px; }
  h2 { margin: 0; font-size: 18px; }
  h4 { margin: 8px 0 2px; font-size: 13px; }
  .grow { flex: 1; }
  .badge { padding: 1px 6px; border: 1px solid var(--line); border-radius: 4px; font-size: 12px; }
  .badge.warn { color: var(--warn); border-color: var(--warn); }
  .note, .stats, .count { margin: 0; font-size: 12px; }
  .banner { margin: 0; padding: 6px 10px; border: 1px solid var(--line); border-radius: 8px; font-size: 12px; overflow-wrap: anywhere; }
  .banner.warn { color: var(--warn); border-color: var(--warn); }
  .banner.err { color: var(--err); border-color: var(--err); }
  .banner ul { margin: 4px 0 0; padding-left: 18px; }
  .body { flex: 1; min-height: 0; display: flex; gap: 8px; }
  .side { flex: none; width: 300px; min-width: 0; display: flex; flex-direction: column; gap: 6px; overflow: auto; border: 1px solid var(--line); border-radius: 8px; padding: 6px; }
  .graph { flex: 1; min-width: 0; min-height: 0; }
  .list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 2px; font-size: 12px; }
  .row { width: 100%; display: flex; flex-direction: column; align-items: flex-start; gap: 1px; text-align: left; border-color: transparent; background: transparent; padding: 3px 6px; overflow-wrap: anywhere; }
  .row.active { background: #22406b; border-color: var(--accent); }
  .meta { display: grid; grid-template-columns: auto 1fr; gap: 2px 8px; margin: 4px 0 0; font-size: 11px; }
  .meta dt { color: var(--text-dim); } .meta dd { margin: 0; overflow-wrap: anywhere; }
  .inspect { border-top: 1px solid var(--line); padding-top: 6px; max-height: 280px; overflow: auto; font-size: 12px; }
  .inspect h4 { margin-top: 0; overflow-wrap: anywhere; }
  .record { display: flex; flex-direction: column; gap: 1px; margin-bottom: 6px; overflow-wrap: anywhere; }
  pre { margin: 4px 0; padding: 6px; background: var(--bg-2); border: 1px solid var(--line); border-radius: 6px; overflow: auto; font-size: 11px; max-height: 160px; }
  .empty { margin: 24px; }
  .dim { color: var(--text-dim); }
  .err { color: var(--err); }
</style>
