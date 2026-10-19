<script lang="ts">
  import { knowledgeApi, type VaultLinkRecord } from "../knowledge";
  import { createDocumentSession, documentComplete, documentProgress, type DocumentSession, type DocumentState, type LinkList } from "../vault-documents";
  import { fragmentSlug, NOT_A_DOCUMENT } from "../vault-markdown";
  import { sanitizeVaultHtml } from "../vault-sanitize";

  // One rendered note. Pages of at most 64 KiB are read in order and rendered as they arrive; a long note says how much is read and
  // never presents a partial render as the whole document. Nothing inside the note is executed or fetched.
  let { vaultId, path, revision = null, fragment = null, markdown = true, backlinks, onNavigate, onMoreBacklinks }: {
    vaultId: string; path: string; revision?: number | null; fragment?: string | null;
    /** False for a non-Markdown vault entry: it is only named, never read. */
    markdown?: boolean;
    backlinks: LinkList;
    onNavigate: (path: string, fragment: string | null) => void;
    onMoreBacklinks: () => void;
  } = $props();

  let session = $state.raw<DocumentSession | null>(null);
  let doc = $state.raw<DocumentState | null>(null);
  let host = $state<HTMLElement>();
  let notice = $state<string | null>(null);
  let reloads = $state(0);

  $effect(() => {
    const id = vaultId, target = path, readable = markdown;
    void reloads;
    // The same path can have new bytes and link resolutions after a refresh or publication.
    void revision;
    notice = null;
    if (!readable) { session = null; doc = null; return; }
    const created = createDocumentSession({ api: knowledgeApi, vaultId: id, path: target, sanitize: sanitizeVaultHtml });
    const stop = created.subscribe(value => { doc = value; });
    session = created;
    void created.start();
    return () => { stop(); created.dispose(); if (session === created) session = null; };
  });

  function find(slug: string, block: string | null): Element | null {
    if (!host) return null;
    if (block !== null) return host.querySelector(`[data-block="${CSS.escape(block)}"]`);
    return host.querySelector(`[data-anchor="${CSS.escape(slug)}"]`);
  }
  /** Scrolls to a heading/block reference; a section on a page not read yet is searched for by reading on. */
  let jumped = "";
  $effect(() => {
    const wanted = fragment;
    const sections = doc?.sections.length ?? 0;
    const current = session;
    if (!wanted || !current || !doc || sections === 0) return;
    const last = wanted.split("#").filter(Boolean).pop() ?? wanted;
    const target = last.startsWith("^") ? find("", last.slice(1)) : find(fragmentSlug(wanted), null);
    if (target) {
      const key = `${path}|${wanted}`;
      if (jumped !== key) { jumped = key; target.scrollIntoView({ block: "start" }); }
      notice = null;
    } else if (!documentComplete(doc)) {
      if (!doc.busy) void current.loadMore();
    } else if (doc.status === "ready") notice = `“${wanted}” was not found in this note.`;
  });

  function click(event: MouseEvent): void {
    const link = (event.target as Element | null)?.closest<HTMLElement>("a[data-kind]");
    if (!link) return;
    event.preventDefault();
    const kind = link.dataset.kind;
    if (kind === "note" && link.dataset.path) onNavigate(link.dataset.path, link.dataset.fragment ?? null);
    else if (kind === "anchor") {
      const wanted = link.dataset.fragment ?? "";
      const target = find(fragmentSlug(wanted), wanted.startsWith("^") ? wanted.slice(1) : null);
      if (target) target.scrollIntoView({ block: "start" }); else notice = `“${wanted}” was not found in the part of this note that is loaded.`;
    } else if (kind === "external" && link.dataset.url) {
      knowledgeApi.openLink(link.dataset.url).catch(failure => { notice = String(failure); });
    }
  }

  const grouped = $derived.by(() => {
    const seen = new Map<string, VaultLinkRecord[]>();
    for (const record of backlinks.items) { const list = seen.get(record.source); if (list) list.push(record); else seen.set(record.source, [record]); }
    return [...seen];
  });
</script>
<article class="doc" aria-label={path}>
  <header>
    <h3 title={path}>{path.slice(path.lastIndexOf("/") + 1)}</h3>
    <span class="dim path">{path}</span>
  </header>
  {#if !markdown}
    <p class="banner" role="status">{NOT_A_DOCUMENT}. Scripts, tools and other files are never copied into a vault or opened from here.</p>
  {:else if !doc || (doc.status === "loading" && doc.sections.length === 0 && !doc.error)}
    <p class="dim" role="status">Reading…</p>
  {/if}
  {#if doc?.error}<p class="banner err" role="alert">{doc.error}</p>{/if}
  {#if notice}<p class="banner warn" role="status">{notice}</p>{/if}
  <!-- Delegated click on real <a href="#"> anchors: Enter/Space already reach them as clicks. -->
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
  <div class="prose" role="document" bind:this={host} onclick={click}>
    {#each doc?.sections ?? [] as section (section.id)}<div class="segment">{@html section.html}</div>{/each}
  </div>
  {#if doc && doc.status !== "loading" && markdown}
    <footer>
      {#if doc.status === "changed"}
        <button onclick={() => { reloads++; }}>Reload</button>
      {:else if documentComplete(doc)}
        <span class="dim">{doc.byteLength.toLocaleString()} bytes · whole note shown</span>
      {:else if doc.status === "ready"}
        <span class="dim" role="status">{documentProgress(doc)} — the rest of this note is not shown yet.</span>
        <button disabled={doc.busy} onclick={() => void session?.loadMore()}>Read next part</button>
        <button disabled={doc.busy} onclick={() => void session?.loadAll()}>Read all</button>
      {/if}
    </footer>
    <section class="backlinks" aria-label="Backlinks">
      <h4>Backlinks{backlinks.status === "ready" ? ` (${backlinks.total.toLocaleString()})` : ""}</h4>
      {#if backlinks.status === "loading" && backlinks.items.length === 0}<p class="dim">Looking…</p>
      {:else if backlinks.status === "error"}<p class="err" role="alert">{backlinks.error}</p>
      {:else if backlinks.status === "ready" && backlinks.items.length === 0}<p class="dim">No other note links here.</p>
      {:else}
        <ul>
          {#each grouped as [source, records] (source)}
            <li><button class="link" onclick={() => onNavigate(source, null)}>{source}</button> <small class="dim">{records.length === 1 ? `line ${records[0].line}` : `${records.length} links`}{records.some(item => item.kind === "embed") ? " · embedded" : ""}</small></li>
          {/each}
        </ul>
        {#if backlinks.nextOffset !== null}<button onclick={onMoreBacklinks}>Show more backlinks</button>{/if}
      {/if}
    </section>
  {/if}
</article>
<style>
  .doc { display: flex; flex-direction: column; gap: 8px; min-width: 0; padding: 8px 12px; overflow: auto; height: 100%; }
  header { display: flex; flex-wrap: wrap; align-items: baseline; gap: 8px; }
  h3 { margin: 0; font-size: 16px; }
  .path { font-size: 11px; overflow-wrap: anywhere; }
  .dim { color: var(--text-dim); }
  .err { color: var(--err); }
  .banner { margin: 0; padding: 6px 10px; border: 1px solid var(--line); border-radius: 8px; font-size: 12px; overflow-wrap: anywhere; }
  .banner.warn { color: var(--warn); border-color: var(--warn); }
  .banner.err { border-color: var(--err); }
  footer { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; font-size: 12px; }
  .backlinks h4 { margin: 8px 0 4px; font-size: 13px; }
  .backlinks ul { list-style: none; margin: 0 0 6px; padding: 0; display: flex; flex-direction: column; gap: 2px; font-size: 12px; }
  .link { border: 0; background: transparent; color: var(--accent); padding: 0; text-align: left; overflow-wrap: anywhere; cursor: pointer; }
  .prose { font-size: 14px; line-height: 1.55; overflow-wrap: anywhere; }
  .prose :global(h1), .prose :global(h2), .prose :global(h3), .prose :global(h4) { margin: 1.1em 0 0.4em; line-height: 1.25; }
  .prose :global(h1) { font-size: 1.6em; } .prose :global(h2) { font-size: 1.35em; } .prose :global(h3) { font-size: 1.15em; }
  .prose :global(p), .prose :global(ul), .prose :global(ol), .prose :global(blockquote), .prose :global(pre), .prose :global(table) { margin: 0.5em 0; }
  .prose :global(blockquote) { border-left: 3px solid var(--line); padding-left: 10px; color: var(--text-dim); }
  .prose :global(pre) { background: var(--bg-2); border: 1px solid var(--line); border-radius: 6px; padding: 8px 10px; overflow: auto; }
  .prose :global(code) { font-family: var(--mono); font-size: 12.5px; }
  .prose :global(:not(pre) > code) { background: var(--bg-2); padding: 0 4px; border-radius: 4px; }
  .prose :global(table) { border-collapse: collapse; }
  .prose :global(th), .prose :global(td) { border: 1px solid var(--line); padding: 3px 8px; }
  .prose :global(.task-list-item) { list-style: none; margin-left: -1.2em; }
  .prose :global(.task-box) { margin-right: 4px; }
  .prose :global(.vault-link) { border-radius: 3px; }
  .prose :global(a.vault-link) { color: var(--accent); text-decoration: underline; cursor: pointer; }
  .prose :global(a.vault-link.external) { color: var(--accent-2); }
  .prose :global(span.vault-link.missing) { color: var(--err); text-decoration: line-through dotted; }
  .prose :global(span.vault-link.ambiguous) { color: var(--warn); text-decoration: underline dotted; }
  .prose :global(span.vault-link.inert), .prose :global(span.vault-link.pending) { color: var(--text-dim); text-decoration: underline dotted; }
  .prose :global(.vault-embed) { border: 1px solid var(--line); border-radius: 8px; padding: 6px 10px; margin: 8px 0; background: color-mix(in srgb, var(--accent) 5%, var(--panel)); }
  .prose :global(.vault-embed-head) { font-size: 12px; margin-bottom: 4px; }
  .prose :global(.vault-embed-notice) { color: var(--warn); font-size: 12px; margin: 4px 0 0; }
  .prose :global(.vault-code figcaption) { font-size: 11px; color: var(--warn); }
  .prose :global(.vault-properties) { font-size: 12px; color: var(--text-dim); border: 1px solid var(--line); border-radius: 6px; padding: 4px 8px; }
</style>
