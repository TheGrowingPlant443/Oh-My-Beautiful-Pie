<script lang="ts">
  import type { EvidenceRef, HelicopterEdgePayload, HelicopterPayload } from "../api";
  import { capabilityRows, certaintyText, evidenceFor, helicopterSummary, HELICOPTER_KIND_WORDS, needsClarification, NEEDS_CLARIFICATION, type JourneyRow } from "../helicopter-graph";
  import type { ViewEdge, ViewNode } from "../system-graph";

  let { result, node, edge, filter, journeys, onReveal, onToggleJourney }: {
    result: HelicopterPayload | null;
    node: ViewNode | null;
    edge: ViewEdge | null;
    /** The page's search text. It narrows the capability list only; the graph search is separate. */
    filter: string;
    /** Every journey with its openness. An opened journey is not a card, so this list is where it is grouped again. */
    journeys: JourneyRow[];
    /** Opens every journey that contains these nodes, so each becomes a visible card, and selects the first. */
    onReveal: (ids: string[]) => void;
    onToggleJourney: (id: string) => void;
  } = $props();

  /** Technical proof (file paths and line ranges) is opt-in: the business view never needs it. */
  let technical = $state(false);

  const summary = $derived(result ? helicopterSummary(result) : null);
  const labels = $derived(new Map((result?.nodes ?? []).map(item => [item.id, item.label])));
  const capabilities = $derived(result ? capabilityRows(result) : []);
  const needle = $derived(filter.trim().toLowerCase());
  const shown = $derived(needle ? capabilities.filter(item => `${item.label} ${item.description} ${item.steps.map(step => step.label).join(" ")}`.toLowerCase().includes(needle)) : capabilities);
  const picked = $derived.by(() => {
    const payload = result;
    return payload && node ? payload.nodes.find(item => item.id === node.id) ?? null : null;
  });
  // An aggregated arrow stands for several original records; every one stays readable.
  const records = $derived.by((): HelicopterEdgePayload[] => {
    const payload = result;
    return payload && edge ? edge.edgeIds.map(id => payload.edges.find(item => item.id === id)).filter((item): item is HelicopterEdgePayload => !!item) : [];
  });
  const proof = $derived<EvidenceRef[]>(result ? evidenceFor(result, edge ? edge.evidenceIds : node?.evidenceIds ?? []) : []);
  const lineText = (item: EvidenceRef): string => `${item.path}:${item.lineStart}${item.lineEnd !== item.lineStart ? `-${item.lineEnd}` : ""}`;
  const labelOf = (id: string): string => labels.get(id) ?? id;
</script>

<aside class="panel-side" aria-label="Business view details">
  {#if !result}
    <h3>Helicopter view</h3>
    <p class="dim">A plain-language picture of who uses this product, what starts each journey, what happens and how it ends. It is generated only when you ask for it.</p>
  {:else}
    <h3>What this product does</h3>
    <p>{result.purpose}</p>
    {#if summary}
      <p class="dim">{summary.journeys.toLocaleString()} {summary.journeys === 1 ? "journey" : "journeys"} · {summary.steps.toLocaleString()} people, steps and results · {summary.capabilities.toLocaleString()} business {summary.capabilities === 1 ? "capability" : "capabilities"}</p>
      {#if summary.unclear > 0}<p class="warn" role="status">{summary.unclear.toLocaleString()} item{summary.unclear === 1 ? "" : "s"} marked “{NEEDS_CLARIFICATION}”: the evidence does not settle them. Nothing was filled in by guessing.</p>{/if}
    {/if}

    {#if edge}
      <h4>Selected arrow</h4>
      <p><strong>{labelOf(edge.source)}</strong> → <strong>{labelOf(edge.target)}</strong></p>
      {#each records as record (record.id)}
        <p class:warn={record.certainty === "unknown"}>{record.label ? `“${record.label}” · ` : ""}{certaintyText(record.certainty)}</p>
      {/each}
      {#if records.length > 1}<p class="dim">This arrow joins {records.length.toLocaleString()} flows between the same cards.</p>{/if}
    {:else if node && picked}
      <h4>Selected {HELICOPTER_KIND_WORDS[picked.kind].toLowerCase()}</h4>
      <p><strong>{picked.label}</strong></p>
      {#if node.within.length}<p class="dim">Part of {node.within.join(" › ")}</p>{/if}
      {#if node.collapsed}<p class="dim">{node.memberCount.toLocaleString()} {node.memberCount === 1 ? "item" : "items"} inside. Use + on the card to open it.</p>{/if}
      {#if picked.description}<p class:warn={needsClarification(picked.description)}>{picked.description}</p>{/if}
    {/if}

    {#if journeys.length}
      <h4>Journeys ({journeys.length.toLocaleString()})</h4>
      <ul class="rows" aria-label="Journeys">
        {#each journeys as item (item.id)}
          <li style:padding-left={`${item.depth * 12}px`}>
            <button class="link" aria-expanded={item.open} title={item.open ? "Group these steps into one card again" : "Show the steps inside this journey"} onclick={() => onToggleJourney(item.id)}>{item.open ? "Group" : "Open"}: {item.label}</button>
            <small class="dim">{item.steps.toLocaleString()} {item.steps === 1 ? "item" : "items"} inside</small>
          </li>
        {/each}
      </ul>
    {/if}

    <h4>Business capabilities ({capabilities.length.toLocaleString()})</h4>
    {#if capabilities.length === 0}
      <p class="dim">None were identified.</p>
    {:else}
      {#if shown.length !== capabilities.length}<p class="dim">{shown.length.toLocaleString()} match “{filter.trim()}”.</p>{/if}
      <ul class="rows" aria-label="Business capabilities">
        {#each shown as item (item.id)}
          <li>
            <button class="link" disabled={item.steps.length === 0} title={item.steps.length ? "Show where this appears in the picture" : "None of its steps could be located"} onclick={() => onReveal(item.steps.map(step => step.id))}>{item.label}</button>
            {#if item.needsClarification}<span class="badge warn">{NEEDS_CLARIFICATION}</span>{/if}
            {#if item.description}<small class="dim">{item.description}</small>{/if}
            <small class="dim">{item.steps.length.toLocaleString()} {item.steps.length === 1 ? "step" : "steps"}</small>
            {#if technical}
              <small class="dim">{evidenceFor(result, item.evidenceIds).map(lineText).join(", ")}</small>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}

    {#if result.limitations.length}
      <h4>What this view cannot tell you</h4>
      <ul>{#each result.limitations as item, at (at)}<li>{item}</li>{/each}</ul>
    {/if}
    <p class="dim">The Architect’s reading of the repository at the time it was indexed, written from the code and files, not a verified description of how the product behaves in production.</p>

    <label class="toggle"><input type="checkbox" bind:checked={technical} />Show technical evidence</label>
    {#if technical}
      {#if proof.length}
        <h4>Evidence behind the selection</h4>
        <ul>{#each proof as item (item.id)}<li><code>{lineText(item)}</code> <small class="dim">{item.contentHash.slice(0, 12)}</small></li>{/each}</ul>
      {:else}
        <p class="dim">Select a card or an arrow to see the repository evidence behind it.</p>
      {/if}
      {#if result.coverage.length}
        <h4>Areas examined ({result.coverage.length.toLocaleString()})</h4>
        <ul>{#each result.coverage as item, at (at)}<li>{item.area}: {item.status}{item.notes ? ` · ${item.notes}` : ""}</li>{/each}</ul>
      {/if}
    {/if}
  {/if}
</aside>

<style>
  .panel-side { width: 340px; flex: none; overflow: auto; padding: 10px 12px; border: 1px solid var(--line); border-radius: 8px; background: var(--panel); display: flex; flex-direction: column; gap: 6px; font-size: 12px; }
  h3, h4, p { margin: 0; overflow-wrap: anywhere; }
  h3 { font-size: 14px; }
  h4 { margin-top: 6px; font-size: 12px; color: var(--text-dim); }
  ul { margin: 0; padding-left: 16px; display: flex; flex-direction: column; gap: 3px; }
  ul.rows { list-style: none; padding-left: 0; gap: 6px; }
  ul.rows li { display: flex; flex-direction: column; gap: 1px; }
  .dim { color: var(--text-dim); }
  .warn { color: var(--warn); }
  .badge { align-self: flex-start; padding: 0 5px; border: 1px solid var(--warn); border-radius: 3px; font-size: 10px; }
  .link { background: none; border: none; padding: 0; color: var(--accent); cursor: pointer; text-align: left; font-size: 12px; }
  .link:disabled { color: var(--text-dim); cursor: default; }
  .toggle { display: inline-flex; align-items: center; gap: 6px; margin-top: 6px; }
  code { overflow-wrap: anywhere; }
</style>
