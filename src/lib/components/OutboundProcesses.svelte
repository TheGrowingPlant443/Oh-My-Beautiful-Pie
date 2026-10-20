<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { api, type HostSocketRow, type HostTcpView, type InsightProducer } from "../api";
  import { architectInstance } from "../stores";
  import {
    createHostMonitor, directionLabel, formatAddress, formatEndpoint, metricLabel, ownerLabel, PAGE_SIZE, sectionNotes,
    type HostMetric, type HostSection,
  } from "../host-monitor";
  import { windowVisible } from "../window-visibility";

  // One agent's whole-computer, read-only view of running processes and external TCP connections. It samples only while
  // this tab is mounted AND on screen AND the window is visible AND nothing covers the app; all other state lives in the controller.
  let { instanceId, generation, projectPath, covered = false, onFocusArchitect }: {
    instanceId: string; generation: number; projectPath: string; covered?: boolean; onFocusArchitect?: (producer: InsightProducer, current: () => boolean) => boolean | Promise<boolean>;
  } = $props();

  const monitor = createHostMonitor({ transport: api, focusArchitect: (producer, current) => onFocusArchitect?.(producer, current) ?? false });
  const view = monitor.state;

  let root = $state<HTMLElement>();
  let onScreen = $state(false);
  const shown = $derived(onScreen && $windowVisible && !covered);
  $effect(() => {
    const context = { instanceId, generation, projectPath };
    untrack(() => monitor.setContext(context));
  });
  $effect(() => {
    const next = shown;
    untrack(() => monitor.setVisible(next));
  });

  onMount(() => {
    const element = root!;
    let inView = typeof IntersectionObserver === "undefined";
    let sized = typeof ResizeObserver === "undefined";
    const apply = (): void => { onScreen = inView && sized; };
    const intersection = typeof IntersectionObserver === "undefined" ? null : new IntersectionObserver(entries => {
      inView = entries[entries.length - 1].isIntersecting;
      apply();
    });
    const resize = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(entries => {
      const box = entries[entries.length - 1].contentRect;
      sized = box.width > 0 && box.height > 0;
      apply();
    });
    intersection?.observe(element);
    resize?.observe(element);
    apply();
    return () => {
      intersection?.disconnect();
      resize?.disconnect();
      clearTimeout(searchTimer);
      monitor.dispose();
    };
  });

  // ----- Derived display state -----
  const tcpFilters: { view: HostTcpView; label: string; metric: HostMetric | null; title: string }[] = [
    { view: "external", label: "External", metric: "external", title: "TCP connections whose remote address is not loopback or wildcard, including private network peers" },
    { view: "confirmedOutbound", label: "Confirmed outbound", metric: "confirmedOutbound", title: "Connections still in the SYN_SENT state: an observed connection attempt" },
    { view: "unknownDirection", label: "Direction unknown", metric: "unknownDirection", title: "Established or closing connections: Windows does not record who started them" },
    { view: "listeners", label: "Listeners", metric: null, title: "Ports a process is listening on" },
    { view: "all", label: "All TCP", metric: null, title: "Every TCP row, including loopback" },
  ];
  const summary = $derived($view.summary);
  const socketPage = $derived($view.page && $view.page.section !== "processes" && $view.page.section === $view.section ? $view.page : null);
  const processPage = $derived($view.page && $view.page.section === "processes" && $view.section === "processes" ? $view.page : null);
  const explain = $derived($view.explain);
  const askBusy = $derived(explain.phase === "pending" || explain.phase === "admitted" || explain.phase === "uncertain" || explain.phase === "blocked");
  const notes = $derived(summary ? sectionNotes(summary) : []);
  const clock = (ms: number): string => new Date(ms).toLocaleTimeString();
  const count = (metric: HostMetric): string => summary ? metricLabel(summary, metric) : "…";
  const monitoring = $derived(
    $view.unsupported ? { text: "Monitoring: not supported here", tone: "err" }
    : $view.frozen ? { text: explain.phase === "idle" ? "Monitoring: frozen" : "Monitoring: frozen for explanation", tone: "warn" }
    : !$view.visible ? { text: "Monitoring: paused (not visible)", tone: "" }
    : $view.collecting && !summary ? { text: "Monitoring: starting…", tone: "accent" }
    : { text: "Monitoring: live, every 2 s while visible", tone: "ok" });

  // Search is applied after a short pause; the controller resets it when the section changes.
  let searchDraft = $state("");
  let searchTimer: number | undefined;
  const storedText = $derived($view.text);
  // Only an outside change (a section switch clears the search) rewrites what the human is typing.
  $effect(() => { const text = storedText; untrack(() => { if (searchDraft.trim() !== text) searchDraft = text; }); });
  function typing(value: string): void {
    searchDraft = value;
    clearTimeout(searchTimer);
    searchTimer = window.setTimeout(() => monitor.setText(value.trim()), 250);
  }
  function show(section: HostSection, tcpView?: HostTcpView): void {
    window.clearTimeout(searchTimer);
    searchDraft = "";
    monitor.showSection(section, tcpView);
  }

  let expanded = $state<Record<string, boolean>>({});
  const toggle = (key: string): void => { expanded = { ...expanded, [key]: !expanded[key] }; };
  const processName = (row: HostSocketRow): string => row.processName ?? (row.pid === null ? "unknown process" : `PID ${row.pid}`);
  const filterCount = (metric: HostMetric | null): string => metric ? ` ${count(metric)}` : "";
  const emptyText = (section: HostSection, filter: HostTcpView, searching: boolean): string => {
    if (searching) return "Nothing in this snapshot matches the search.";
    if (section === "processes") return "No processes were listed.";
    if (section === "udp") return "No UDP endpoints in this snapshot.";
    return filter === "external" ? "No external TCP connections in this snapshot." : "No TCP rows match this filter in this snapshot.";
  };
  const askArchitect = (): void => { void monitor.explainDisplayed({ architectId: $architectInstance?.id ?? null }); };
</script>

<section class="monitor" bind:this={root} aria-label="Outbound and processes">
  <header class="head">
    <h3>Outbound &amp; Processes</h3>
    <span class="pill" title="Every process and connection on this computer, not just this agent's">Whole computer</span>
    <span class="pill" title="Nothing here can end a process or change a firewall rule">Read-only</span>
    <span class="pill {monitoring.tone}" role="status">{monitoring.text}</span>
  </header>

  {#if $view.unsupported}
    <p class="banner err" role="alert">The operating system tables this view reads are not available on this platform, so nothing was collected.</p>
  {/if}
  {#if $view.stale}
    <p class="banner warn" role="status">{summary ? `Showing the last good snapshot (${clock(summary.completedAt)}). ` : ""}The latest sample failed: {$view.stale.message}</p>
  {/if}
  {#if $view.frozen && !$view.unsupported}
    <p class="banner" role="status">{explain.phase === "idle" ? "Sampling is frozen." : "Sampling is frozen on the snapshot that was sent to the Architect."} Use Refresh to check whether it can continue.</p>
  {/if}
  {#if !$view.visible && summary && !$view.unsupported}
    <p class="banner" role="status">Paused: the snapshot below is from {clock(summary.completedAt)}. Sampling resumes when this tab is visible again.</p>
  {/if}

  <div class="cards">
    <button type="button" class="card" class:active={$view.section === "processes"} aria-pressed={$view.section === "processes"} onclick={() => show("processes")}>
      <span class="label">Running processes</span><strong>{count("processes")}</strong>
      <span class="hint">{summary && summary.limitedProcessCount ? `${metricLabel(summary, "limited")} with limited access` : "Open the process list"}</span>
    </button>
    <button type="button" class="card" class:active={$view.section !== "processes"} aria-pressed={$view.section !== "processes"} onclick={() => show("tcp", "external")}>
      <span class="label">External connections</span><strong>{count("external")}</strong>
      <span class="hint">TCP peers outside this computer</span>
    </button>
  </div>

  <div class="bar">
    <span class="dim" title="One snapshot read from the operating system's process and socket tables">
      {summary ? `Sampled ${clock(summary.completedAt)} · ${summary.durationMs} ms` : $view.collecting ? "Sampling…" : "No snapshot yet"}
    </span>
    <span class="spacer"></span>
    <button type="button" disabled={!$view.visible || $view.collecting || explain.phase === "pending" || explain.phase === "blocked"} onclick={() => monitor.refresh()}>{$view.collecting ? "Refreshing…" : "Refresh"}</button>
    <button type="button" class="primary" disabled={!summary || askBusy || !$architectInstance} title={$architectInstance ? "Ask the existing Architect to explain this snapshot" : "The Architect has not been created yet"} onclick={askArchitect}>Ask Architect to explain</button>
  </div>
  <p class="consent dim">Ask Architect to explain sends this snapshot's process names, PIDs and network endpoints to the existing Architect's AI model, and freezes this view on that snapshot. Nothing is sent unless you press it.</p>

  {#if explain.phase === "pending"}
    <p class="banner" role="status">Asking the Architect…</p>
  {:else if explain.phase === "admitted"}
    <p class="banner ok" role="status">Sent to the Architect{explain.focused ? " and its chat was opened" : ""}. Read the reply in the Architect chat. This view stays frozen on the snapshot you sent. <button type="button" onclick={() => monitor.refresh()}>Resume live view</button></p>
  {:else if explain.phase === "uncertain"}
    <p class="banner warn" role="alert">Delivery to the Architect is uncertain, so it was not sent again: {explain.message} Open the Architect to check; this view stays frozen. <button type="button" onclick={() => monitor.refresh()}>Check if sampling can resume</button></p>
  {:else if explain.phase === "blocked"}
    <div class="banner warn" role="alert">
      <p>{explain.message}</p>
      <p class="dim">Pausing SRS checkpoints its documentation job; you can resume it later from SRS documentation. Then this same snapshot is sent.</p>
      <div class="row">
        <button type="button" class="primary" onclick={() => void monitor.continueAfterSrsPause()}>Pause SRS and continue</button>
        <button type="button" onclick={() => monitor.dismissExplanation()}>Dismiss</button>
      </div>
    </div>
  {:else if explain.phase === "failed"}
    <p class="banner err" role="alert">Not sent: {explain.message} <button type="button" onclick={() => monitor.dismissExplanation()}>Dismiss</button></p>
  {/if}

  {#if $view.section === "processes"}
    <div class="sub">
      <button type="button" onclick={() => show("tcp", "external")}>← Back</button>
      <h4>Running processes</h4>
    </div>
  {:else}
    <div class="filters" role="group" aria-label="Connection filter">
      {#each tcpFilters as filter (filter.view)}
        <button type="button" class:active={$view.section === "tcp" && $view.tcpView === filter.view} aria-pressed={$view.section === "tcp" && $view.tcpView === filter.view} title={filter.title} onclick={() => monitor.setTcpView(filter.view)}>{filter.label}{filterCount(filter.metric)}</button>
      {/each}
      <button type="button" class:active={$view.section === "udp"} aria-pressed={$view.section === "udp"} title="UDP sockets have no remote peer in the operating system table" onclick={() => show("udp")}>UDP endpoints {count("udp")}</button>
    </div>
  {/if}

  <input type="search" class="search" aria-label="Search by process name, PID or address" placeholder="Search name, PID or address" value={searchDraft} oninput={event => typing(event.currentTarget.value)} />

  {#if $view.pageError}<p class="banner err" role="alert">Could not load this view: {$view.pageError}</p>{/if}

  <div class="results" aria-busy={$view.pageLoading}>
    {#if processPage}
      <div class="scroll">
        <table>
          <thead><tr><th>Process</th><th>PID</th><th>Parent</th><th>Threads</th></tr></thead>
          <tbody>
            {#each processPage.items as row (row.key)}
              <tr>
                <td>{row.name}
                  {#if row.owner}<span class="pill accent" title="Started by an agent managed by this app">{ownerLabel(row.owner, instanceId)}</span>{/if}
                  {#if row.availability !== "available"}<span class="pill warn" title={row.availability === "limited" ? "Windows did not allow even a limited query of this process" : "This process ended while the snapshot was read"}>{row.availability === "limited" ? "limited access" : "exited"}</span>{/if}
                </td>
                <td class="mono">{row.pid}</td>
                <td class="mono">{row.parentPid ?? "—"}</td>
                <td class="mono">{row.threadCount}</td>
              </tr>
            {:else}
              <tr><td colspan="4" class="dim">{emptyText("processes", $view.tcpView, $view.text !== "")}</td></tr>
            {/each}
          </tbody>
        </table>
      </div>
    {:else if socketPage}
      <div class="scroll">
        <table>
          <thead>
            {#if socketPage.section === "udp"}<tr><th>Process</th><th>Local address</th><th>Port</th><th></th></tr>
            {:else}<tr><th>Process</th><th>Remote address</th><th>Remote port</th><th></th></tr>{/if}
          </thead>
          {#each socketPage.items as row (row.key)}
            {@const endpoint = socketPage.section === "udp" ? row.local : row.remote}
            <tbody>
              <tr>
                <td>{processName(row)}</td>
                <td class="mono">{endpoint ? formatAddress(endpoint) : "—"}</td>
                <td class="mono">{endpoint ? endpoint.port : "—"}</td>
                <td class="end"><button type="button" class="ghost" aria-expanded={!!expanded[row.key]} onclick={() => toggle(row.key)}>{expanded[row.key] ? "Hide" : "Details"}</button></td>
              </tr>
              {#if expanded[row.key]}
                <tr class="detail">
                  <td colspan="4">
                    <dl>
                      <dt>Local</dt><dd class="mono">{formatEndpoint(row.local)}</dd>
                      {#if row.remote}<dt>Remote</dt><dd class="mono">{formatEndpoint(row.remote)} · {row.addressScope}</dd>{/if}
                      <dt>PID</dt><dd class="mono">{row.pid ?? "unknown"}</dd>
                      <dt>State</dt><dd>{row.state ?? "—"}</dd>
                      <dt>Direction</dt><dd>{directionLabel(row)}</dd>
                    </dl>
                  </td>
                </tr>
              {/if}
            </tbody>
          {:else}
            <tbody><tr><td colspan="4" class="dim">{emptyText(socketPage.section, $view.tcpView, $view.text !== "")}</td></tr></tbody>
          {/each}
        </table>
      </div>
    {:else if summary}
      <p class="dim">{$view.pageLoading ? "Loading…" : "Nothing to show yet."}</p>
    {:else}
      <p class="dim">{$view.unsupported ? "" : "Waiting for the first snapshot…"}</p>
    {/if}
  </div>

  {#if processPage || socketPage}
    {@const current = processPage ?? socketPage!}
    <nav class="pager" aria-label="Pages">
      <span class="dim">{current.total === 0 ? "0 results" : `${current.offset + 1}–${current.offset + current.items.length} of ${current.total}`}{$view.pageLoading ? " · loading…" : ""}</span>
      <span class="spacer"></span>
      <button type="button" disabled={current.offset === 0 || $view.pageLoading} onclick={() => monitor.loadPage(current.offset - PAGE_SIZE)}>Previous</button>
      <button type="button" disabled={current.nextOffset === null || $view.pageLoading} onclick={() => monitor.loadPage(current.nextOffset ?? current.offset)}>Next</button>
    </nav>
  {/if}

  {#if notes.length}
    <ul class="notes warn" aria-label="Collection problems">{#each notes as note}<li>{note}</li>{/each}</ul>
  {/if}

  <details class="limits">
    <summary>What this can and cannot show</summary>
    <ul>
      <li>Established TCP connections carry no initiator in the Windows table, so their direction is unknown. Only a connection still in SYN_SENT is a confirmed outbound attempt.</li>
      <li>External means a TCP peer that is not loopback or wildcard. It includes private network (LAN) addresses and says nothing about intent.</li>
      <li>UDP has no peer or direction in the table; UDP endpoints are listed separately.</li>
      <li>Processes Windows will not let this app open (protected or other users') are limited: they keep their PID, but their sockets may have no process name.</li>
      <li>Only names, PIDs, parent PIDs, thread counts and addresses are collected: no CPU or memory use, command lines, environment, file paths or user names. Nothing can be ended or blocked from here.</li>
      <li>Sampling happens only while this tab is visible, and stops when it is hidden, covered, minimized or left.</li>
    </ul>
  </details>
</section>

<style>
  .monitor { height: 100%; min-width: 0; overflow: auto; display: flex; flex-direction: column; gap: 10px; padding: 10px; background: var(--panel); border: 1px solid var(--line); border-radius: 8px; color: var(--text); }
  .head { display: flex; flex-wrap: wrap; align-items: center; gap: 6px 8px; }
  h3, h4, p { margin: 0; }
  h3 { font-size: 15px; margin-right: 4px; }
  h4 { font-size: 13px; }
  p, li { font-size: 12px; overflow-wrap: anywhere; }
  .banner { padding: 8px 10px; border: 1px solid var(--line); border-radius: 7px; background: var(--panel-2); display: flex; flex-direction: column; gap: 6px; align-items: flex-start; }
  .banner.warn { border-color: #57451a; background: #241d0e; color: #e8c46b; }
  .banner.err { border-color: #55262a; background: #261213; color: #ff9d97; }
  .banner.ok { border-color: #1d4d2c; background: #10231a; color: #7ee2a1; }
  .banner button { align-self: flex-start; }
  .cards { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 8px; }
  .card { display: flex; flex-direction: column; align-items: flex-start; gap: 2px; padding: 10px 12px; text-align: left; background: var(--panel-2); border: 1px solid var(--line); border-radius: 8px; min-width: 0; }
  .card.active { border-color: var(--accent); background: #162a44; }
  .card .label { font-size: 12px; color: var(--text-dim); }
  .card strong { font-size: 24px; line-height: 1.2; overflow-wrap: anywhere; }
  .card .hint { font-size: 11px; color: var(--text-faint); overflow-wrap: anywhere; }
  .bar { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; }
  .spacer { flex: 1 1 auto; }
  .consent { font-size: 11px; }
  .sub { display: flex; align-items: center; gap: 8px; }
  .filters { display: flex; flex-wrap: wrap; gap: 6px; }
  .filters button { font-size: 12px; padding: 4px 8px; }
  .search { width: 100%; min-width: 0; }
  .results { min-width: 0; }
  .scroll { overflow-x: auto; border: 1px solid var(--line-soft); border-radius: 7px; }
  table { width: 100%; border-collapse: collapse; font-size: 12px; }
  th, td { text-align: left; vertical-align: top; padding: 6px 8px; border-bottom: 1px solid var(--line-soft); overflow-wrap: anywhere; }
  th { position: sticky; top: 0; background: var(--panel-2); color: var(--text-dim); font-weight: 600; white-space: nowrap; }
  td.end { text-align: right; white-space: nowrap; }
  td .pill { margin-left: 4px; }
  tr.detail td { background: var(--bg-2); }
  dl { display: grid; grid-template-columns: max-content minmax(0, 1fr); gap: 2px 12px; margin: 0; font-size: 12px; }
  dt { color: var(--text-dim); }
  dd { margin: 0; overflow-wrap: anywhere; }
  .pager { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
  .notes { margin: 0; padding-left: 18px; }
  .limits { font-size: 12px; color: var(--text-dim); }
  .limits summary { cursor: pointer; }
  .limits ul { margin: 6px 0 0; padding-left: 18px; display: flex; flex-direction: column; gap: 4px; }
  .row { display: flex; flex-wrap: wrap; gap: 8px; }
</style>
