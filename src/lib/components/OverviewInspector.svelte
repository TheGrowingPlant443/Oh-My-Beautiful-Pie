<script lang="ts">
  import type { EvidenceRef, NeighborRow, SourceFileRow, SourceSiteRow, SourceSymbolRow } from "../api";
  import type { Inspector, OverviewTab } from "../system-overview";
  import { clipText, edgeArrow, gradeProven, gradeText, loopText, type LoopKind, type ViewEdge, type ViewNode } from "../system-graph";

  let { tab, node, edge, inspector, evidence, symbol, file, loopKind = "recursion", onDrillFiles, onDrillFunctions, onNeighborhood, onPeer, onMore }: {
    tab: OverviewTab; node: ViewNode | null; edge: ViewEdge | null; inspector: Inspector | null;
    /** Evidence records of the shown Architect analysis (system tab only), by id. */
    evidence: ReadonlyMap<string, EvidenceRef>; symbol: SourceSymbolRow | null; file: SourceFileRow | null;
    /** Graph-domain views name a node's relationship to itself a self-link, not source-code recursion. */
    loopKind?: LoopKind;
    onDrillFiles: (paths: string[], label: string) => void; onDrillFunctions: (path: string, label: string) => void;
    onNeighborhood: (node: ViewNode) => void; onPeer: (id: string, label: string | null) => void; onMore: () => void;
  } = $props();

  const mine = $derived(inspector && node?.ref && inspector.id === node.ref ? inspector : null);
  const edges = $derived((mine?.rows ?? []).filter((row): row is Extract<NeighborRow, { kind: "edge" }> => row.kind === "edge"));
  const callers = $derived(edges.filter(row => row.direction === "in"));
  const callees = $derived(edges.filter(row => row.direction === "out"));
  const aggregates = $derived((mine?.rows ?? []).filter((row): row is Extract<NeighborRow, { kind: "aggregate" }> => row.kind === "aggregate"));
  const missing = $derived((mine?.rows ?? []).filter((row): row is Extract<NeighborRow, { kind: "site" }> => row.kind === "site").map(row => row.site));
  const nodeEvidence = $derived((node?.evidenceIds ?? []).map(id => evidence.get(id)).filter((item): item is EvidenceRef => !!item));
  const edgeEvidence = $derived((edge?.evidenceIds ?? []).map(id => evidence.get(id)).filter((item): item is EvidenceRef => !!item));
  const edgeFacts = $derived((edge?.evidenceIds ?? []).filter(id => !evidence.has(id)));
  const evidencePaths = $derived([...new Set(nodeEvidence.map(item => item.path))]);
  const lineText = (item: EvidenceRef): string => `${item.path}:${item.lineStart}${item.lineEnd !== item.lineStart ? `-${item.lineEnd}` : ""}`;
  const siteText = (site: SourceSiteRow): string => `${site.path}:${site.span.lineStart}`;
</script>
<aside class="inspector" aria-label="Details">
  {#if edge}
    <h3>Relationship</h3>
    <p><strong>{edge.relation}</strong>{edge.count > 1 ? ` × ${edge.count.toLocaleString()}` : ""}{edge.derived ? " · derived from function-level references" : ""}</p>
    <p class:proven={gradeProven(edge.grade)} class="grade">{gradeText(edge.grade)}{tab === "system" ? " · Architect analysis, not verified runtime traffic" : ""}</p>
    {#if !edgeArrow(edge)}<p class="dim">Undirected: a mutual relationship, drawn without an arrowhead. Open the records below for the stored source and target.</p>{/if}
    {#if edge.response}<p class="dim">A response: it points back to its caller and is not a new request.</p>{/if}
    {#if edge.cycle}<p class="dim">Both ends are in the same strongly connected component.</p>{/if}
    {#if edge.label}<p>{edge.label}</p>{/if}
    {#if edgeEvidence.length}
      <h4>Evidence</h4>
      <ul>{#each edgeEvidence as item (item.id)}<li><code>{lineText(item)}</code> <small class="dim">{item.contentHash.slice(0, 12)}</small></li>{/each}</ul>
    {/if}
    {#if edgeFacts.length}
      <h4>{tab === "system" ? "Static fact" : "Underlying records"} ({edge.edgeIds.length.toLocaleString()} edge{edge.edgeIds.length === 1 ? "" : "s"}{edge.truncated ? ", sample shown" : ""})</h4>
      <ul>{#each edgeFacts.slice(0, 20) as id (id)}<li><code title={id}>{clipText(id, 24)}</code></li>{/each}</ul>
      {#if edgeFacts.length > 20 || edge.truncated}<p class="dim">Open a node's neighbourhood for every callsite behind this arrow.</p>{/if}
    {/if}
  {:else if node}
    <h3 title={node.label}>{clipText(node.label, 80)}</h3>
    <p class="dim">{node.kind}{node.collapsed ? ` · ${node.memberCount.toLocaleString()} items inside` : ""}{node.within.length ? ` · in ${node.within.join(" › ")}` : ""}</p>
    {#if node.evidence === "analysis"}<p class="grade">Architect analysis</p>{/if}
    {#if node.evidence === "partial" || node.evidence === "failed" || node.evidence === "unsupported" || node.evidence === "skipped"}
      <p class="warn" role="status">This file is {node.evidence === "unsupported" ? "not parsed (unsupported language)" : node.evidence}: no connections here are not proof that none exist.</p>
    {/if}
    {#if node.description}<p>{node.description}</p>{/if}
    {#if node.path}<p><code>{node.path}</code></p>{/if}
    {#if file}<p class="dim">{file.language ?? "no parser"} · {file.bytes.toLocaleString()} bytes · {file.status}{file.diagnostics.length ? ` · ${file.diagnostics.join("; ")}` : ""}</p>{/if}
    {#if symbol}<p class="dim">Lines {symbol.span.lineStart}–{symbol.span.lineEnd} · bytes {symbol.span.byteStart}–{symbol.span.byteEnd}{symbol.region ? ` · region ${symbol.region}` : ""}</p>{/if}
    {#if node.selfLoops > 0}<p class="dim">↻ {node.selfLoops.toLocaleString()} {loopText(loopKind, node.selfLoops)}{loopKind === "selfLink" ? "" : " to itself"}.</p>{/if}
    {#if node.internal > 0}<p class="dim">{node.internal.toLocaleString()} relationship{node.internal === 1 ? "" : "s"} entirely inside this group.</p>{/if}
    <div class="actions">
      {#if tab === "system" && evidencePaths.length}<button onclick={() => onDrillFiles(evidencePaths, node.label)}>Show files</button>{/if}
      {#if tab === "files" && node.path && node.ref && node.kind === "file"}<button onclick={() => onDrillFunctions(node.path ?? "", node.label)}>Show functions</button>{/if}
      {#if tab !== "system" && node.ref}<button onclick={() => onNeighborhood(node)}>Open neighbourhood</button>{/if}
    </div>
    {#if nodeEvidence.length}
      <h4>Evidence</h4>
      <ul>{#each nodeEvidence as item (item.id)}<li><code>{lineText(item)}</code> <small class="dim">{item.contentHash.slice(0, 12)}</small></li>{/each}</ul>
      <p class="dim">Configuration and files named here are evidence from the repository, not proof of live deployment.</p>
    {/if}
    {#if tab !== "system" && node.ref}
      {#if !mine || mine.status === "loading"}<p role="status">Loading callers and callees…</p>
      {:else if mine.status === "error"}<p class="err" role="alert">{mine.error}</p>
      {:else}
        <h4>{tab === "files" ? "Imported by" : "Callers"} ({callers.length})</h4>
        <ul class="rows">{#each callers as row (row.edge.id + row.direction)}
          <li><button class="link" onclick={() => onPeer(row.peerId, row.peerLabel)} title={row.peerLabel ?? row.peerId}>{clipText(row.peerLabel ?? row.peerId, 48)}</button> <small>{row.edge.relation} · <span class:proven={gradeProven(row.edge.resolution)}>{gradeText(row.edge.resolution)}</span></small>
            {#if row.sites[0]}<small class="dim"> {siteText(row.sites[0])}{row.sitesTruncated || row.sites.length > 1 ? ` (+${row.sites.length - 1}${row.sitesTruncated ? "+" : ""} more)` : ""}</small>{/if}</li>
        {/each}</ul>
        <h4>{tab === "files" ? "Imports" : "Callees"} ({callees.length})</h4>
        <ul class="rows">{#each callees as row (row.edge.id + row.direction)}
          <li><button class="link" onclick={() => onPeer(row.peerId, row.peerLabel)} title={row.peerLabel ?? row.peerId}>{clipText(row.peerLabel ?? row.peerId, 48)}</button> <small>{row.edge.relation} · <span class:proven={gradeProven(row.edge.resolution)}>{gradeText(row.edge.resolution)}</span>{row.self ? " · recursion" : ""}</small>
            {#if row.sites[0]}<small class="dim"> {siteText(row.sites[0])}{row.sitesTruncated || row.sites.length > 1 ? ` (+${row.sites.length - 1}${row.sitesTruncated ? "+" : ""} more)` : ""}</small>{/if}</li>
        {/each}</ul>
        {#if aggregates.length}
          <h4>Cross-file call/reference aggregates (derived)</h4>
          <ul class="rows">{#each aggregates as row (row.direction + (row.peerId ?? row.peerLabel) + row.relation + row.resolution)}<li>{row.direction === "out" ? "→" : "←"} {clipText(row.peerLabel, 48)} <small>{row.relation} ×{row.count.toLocaleString()} · {gradeText(row.resolution)}</small></li>{/each}</ul>
        {/if}
        {#if missing.length}
          <h4>Missing endpoints ({missing.length})</h4>
          <p class="dim">Recognized callsites with no resolved target. They are not connections and not proof that none exist.</p>
          <ul class="rows">{#each missing as site (site.id)}<li><code>{clipText(site.raw ?? "(unnamed)", 40)}</code> <small>{site.relation} · {gradeText(site.resolution)} · {siteText(site)}</small></li>{/each}</ul>
        {/if}
        {#if mine.nextOffset !== null}<button onclick={onMore}>Load more neighbours</button>{/if}
        <p class="dim">{mine.rows.length.toLocaleString()} of {mine.total.toLocaleString()} neighbour records loaded.</p>
      {/if}
    {/if}
  {:else}
    <h3>Details</h3>
    <p class="dim">Select a card or arrow. Nothing here starts an agent.</p>
  {/if}
</aside>
<style>
  .inspector { width: 340px; flex: none; overflow: auto; padding: 10px 12px; border: 1px solid var(--line); border-radius: 8px; background: var(--panel); display: flex; flex-direction: column; gap: 6px; font-size: 12px; }
  h3, h4, p { margin: 0; overflow-wrap: anywhere; }
  h3 { font-size: 14px; }
  h4 { margin-top: 6px; font-size: 12px; color: var(--text-dim); }
  ul { margin: 0; padding-left: 16px; display: flex; flex-direction: column; gap: 3px; }
  ul.rows { list-style: none; padding-left: 0; }
  .dim { color: var(--text-dim); }
  .grade { color: var(--warn); }
  .grade.proven, .proven { color: var(--ok); }
  .warn { color: var(--warn); }
  .err { color: var(--err); }
  .actions { display: flex; flex-wrap: wrap; gap: 6px; }
  .link { background: none; border: none; padding: 0; color: var(--accent); cursor: pointer; text-align: left; font-size: 12px; }
  code { overflow-wrap: anywhere; }
</style>
