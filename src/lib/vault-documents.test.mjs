import test from "node:test";
import assert from "node:assert/strict";
import { get } from "svelte/store";
import { importTs } from "./test-loader.mjs";

const lib = await importTs("vault-documents");
const { createDocumentSession, createVaultStore, documentComplete, treeRows, folderChain } = lib;

const settle = () => new Promise(resolve => setTimeout(resolve, 0));
const deferred = () => Promise.withResolvers();

/** Backend double: whole documents served in UTF-8 pages that never split a character. */
function backend(documents, { pageBytes = 64, resolutions = {} } = {}) {
  const log = [];
  const api = {
    log,
    async readDocument(vaultId, path, offset) {
      log.push(["read", path, offset]);
      const doc = documents[path];
      if (!doc) throw new Error(`no such note ${path}`);
      const bytes = Buffer.from(doc.text, "utf8");
      let end = Math.min(bytes.length, offset + pageBytes);
      while (end < bytes.length && (bytes[end] & 0xc0) === 0x80) end--;
      return {
        vaultId, revision: doc.revision ?? 1, path, hash: doc.hash ?? `hash:${path}`, byteLength: bytes.length, byteOffset: offset, length: end - offset,
        nextOffset: end >= bytes.length ? null : end, text: bytes.subarray(offset, end).toString("utf8"), startsMidLine: false, fence: null, frontmatter: false, lineNumber: 1,
      };
    },
    async resolveLinks(vaultId, _from, targets) {
      return { vaultId, revision: 1, total: targets.length, offset: 0, nextOffset: null, items: targets.map(raw => ({ raw, ...(resolutions[raw] ?? { status: "missing", path: null, candidates: [] }) })) };
    },
  };
  return api;
}
const session = (api, path, extra = {}) => createDocumentSession({ api, vaultId: "v", path, sanitize: html => html, ...extra });
const html = state => state.sections.map(item => item.html).join("");

test("a long note is read in order, rendered section by section, and only complete when every byte is shown", async () => {
  const body = ["# Title", "", ...Array.from({ length: 40 }, (_, i) => `Paragraph ${i} with some words to fill the page.`).flatMap(line => [line, ""]), "```js", "const a = 1;", "const b = 2;", "```", "", "The end."].join("\n");
  const api = backend({ "long.md": { text: body } }, { pageBytes: 200 });
  const s = session(api, "long.md");
  await s.start();
  let state = get(s);
  assert.equal(state.status, "ready");
  assert.ok(state.nextOffset !== null, "the first page is not the whole note");
  assert.equal(documentComplete(state), false, "a partial render is never presented as the whole note");
  assert.ok(state.loadedBytes < state.byteLength);
  await s.loadAll();
  state = get(s);
  assert.ok(documentComplete(state));
  assert.equal(state.loadedBytes, state.byteLength);
  assert.equal((html(state).match(/<h1 /g) ?? []).length, 1);
  assert.equal((html(state).match(/Paragraph \d+/g) ?? []).length, 40, "no paragraph lost or repeated at page edges");
  assert.match(html(state), /<pre><code class="language-js">const a = 1;\nconst b = 2;\n<\/code><\/pre>/, "a fence that spans pages renders as one block");
  assert.match(html(state), /The end\./);
  const offsets = api.log.filter(item => item[0] === "read" && item[1] === "long.md").map(item => item[2]);
  assert.deepEqual(offsets, [...offsets].sort((a, b) => a - b));
  assert.equal(new Set(offsets).size, offsets.length, "no page is read twice");
});

test("multi-byte text survives page boundaries and CRLF split across pages", async () => {
  const text = "Ünïcödé ✓ 日本語\r\nsecond line\r\n\r\nthird ✓";
  const api = backend({ "u.md": { text } }, { pageBytes: 7 });
  const s = session(api, "u.md");
  await s.start();
  await s.loadAll();
  const out = html(get(s));
  assert.match(out, /Ünïcödé ✓ 日本語/);
  assert.match(out, /second line/);
  assert.match(out, /third ✓/);
  assert.ok(!out.includes("\r"));
});

test("a note that changes while it is being read stops as changed instead of mixing versions", async () => {
  const api = backend({ "n.md": { text: "para one\n\n".repeat(40) } }, { pageBytes: 100 });
  const s = session(api, "n.md");
  await s.start();
  const before = get(s).sections.length;
  const original = api.readDocument;
  api.readDocument = async (...args) => ({ ...(await original(...args)), hash: "changed" });
  await s.loadMore();
  const state = get(s);
  assert.equal(state.status, "changed");
  assert.match(state.error, /changed while it was being read/);
  assert.equal(state.sections.length, before, "nothing from the new version was rendered");
});

test("a page that does not start where the last one ended is refused so no text is skipped", async () => {
  const api = backend({ "n.md": { text: "para one\n\n".repeat(40) } }, { pageBytes: 100 });
  const s = session(api, "n.md");
  await s.start();
  const original = api.readDocument;
  api.readDocument = async (vaultId, path, offset) => original(vaultId, path, offset + 10);
  await s.loadAll();
  assert.equal(get(s).status, "error");
  assert.match(get(s).error, /no text is skipped/);
});

test("properties are shown separately and never rendered as Markdown", async () => {
  const api = backend({ "p.md": { text: "---\ntitle: <b>x</b>\ntags: [a]\n---\n# Body\n" } }, { pageBytes: 12 });
  const s = session(api, "p.md");
  await s.start();
  await s.loadAll();
  const out = html(get(s));
  assert.match(out, /<details class="vault-properties"><summary>Properties<\/summary><pre>title: &lt;b&gt;x&lt;\/b&gt;/);
  assert.match(out, /<h1 /);
});

test("embedding: a cycle is named and not expanded, a missing section and a partial read are stated", async () => {
  const api = backend({
    "A.md": { text: "# A\n\n![[B]]\n" },
    "B.md": { text: "# B\n\n![[A]]\n\nB body" },
    "Big.md": { text: `# Big\n\n${"word ".repeat(60000)}\n\n## Tail\nx` },
    "C.md": { text: "![[Big#Nowhere]]\n\n![[Big]]" },
  }, { pageBytes: 64 * 1024, resolutions: { A: { status: "resolved", path: "A.md", candidates: [] }, B: { status: "resolved", path: "B.md", candidates: [] }, Big: { status: "resolved", path: "Big.md", candidates: [] } } });
  const a = session(api, "A.md");
  await a.start();
  const out = html(get(a));
  assert.match(out, /B body/);
  assert.match(out, /Not expanded: A\.md → B\.md → A\.md would embed itself\./);
  const c = session(api, "C.md");
  await c.start();
  const embedded = html(get(c));
  assert.match(embedded, /“Nowhere” was not found in the part of Big\.md read for this embed/);
  assert.match(embedded, /Only the first \d+ KiB of Big\.md is shown here; open the note to read the rest\./);
});

test("a disposed session renders nothing from answers still on their way", async () => {
  const gate = deferred();
  const api = backend({ "d.md": { text: "# Late" } });
  const original = api.readDocument;
  api.readDocument = async (...args) => { await gate.promise; return original(...args); };
  const s = session(api, "d.md");
  const started = s.start();
  s.dispose();
  gate.resolve();
  await started;
  assert.equal(get(s).sections.length, 0);
});

// ---------- vault store ----------

const entry = (path, kind = "markdown") => ({ path, name: path.slice(path.lastIndexOf("/") + 1), kind, bytes: 1, hash: kind === "folder" ? null : "h" });
function vaultApi(options = {}) {
  const calls = [];
  const state = { revision: options.revision ?? 1, listings: options.listings ?? { "": [entry("docs", "folder"), entry("a.md")], docs: [entry("docs/b.md")] }, hold: options.hold ?? null };
  const api = {
    calls, state,
    async refreshIndex(vaultId) { calls.push(["refresh"]); return { vaultId, revision: state.revision, notes: 2, links: 1, unreadable: 0 }; },
    async listEntries(vaultId, prefix, offset, limit) {
      calls.push(["list", prefix, offset]);
      if (state.hold) await state.hold(prefix);
      const all = state.listings[prefix] ?? [];
      const items = all.slice(offset, offset + limit);
      return { vaultId, revision: state.revision, prefix, total: all.length, offset, items, nextOffset: offset + items.length < all.length ? offset + items.length : null };
    },
    async queryLinks(vaultId, query) {
      calls.push(["query", query.kind, query.offset ?? 0]);
      if (state.hold) await state.hold(query.kind);
      const rows = query.kind === "notes" ? [{ path: "a.md", title: "a", bytes: 1, hash: "h", outgoing: 1, incoming: 0 }] : query.kind === "links" ? [{ id: "l", source: "a.md", target: "docs/b.md", kind: "wiki", status: "resolved", raw: "b", label: null, fragment: null, line: 1, candidates: [] }]
        : query.kind === "search" ? [{ path: `hit-${query.text}.md`, title: query.text, line: null, snippet: "" }] : [];
      return { vaultId, revision: state.revision, total: rows.length, offset: query.offset ?? 0, items: rows, nextOffset: null };
    },
  };
  return api;
}
function fakeIndex() {
  const jobs = [];
  return {
    jobs,
    begin(kind) {
      const job = { kind, chunks: [], cancelled: false, push(chunk) { this.chunks.push(chunk); }, finish() { return Promise.resolve(this.cancelled ? null : { kind: "vault", model: { nodes: [], edges: [] }, stats: { notes: this.chunks.length } }); }, cancel() { this.cancelled = true; } };
      jobs.push(job);
      return job;
    },
    cancel() { for (const job of jobs) if (!job.cancelled) job.cancelled = true; },
    dispose() { this.cancel(); },
  };
}

test("refresh reads the index, the visible listings and the note graph; opening folders loads them on demand", async () => {
  const api = vaultApi();
  const index = fakeIndex();
  const store = createVaultStore({ api, indexClient: index, vaultId: "v" });
  await store.refresh();
  await settle();
  let state = get(store);
  assert.equal(state.revision, 1);
  assert.deepEqual(state.listings[""].items.map(item => item.path), ["docs", "a.md"]);
  assert.equal(state.graph.status, "ready");
  assert.deepEqual(index.jobs[0].chunks.map(chunk => chunk.section), ["notes", "links"]);
  let rows = treeRows(state);
  assert.deepEqual(rows.map(row => row.kind === "folder" ? `${row.path}/` : row.kind === "entry" ? row.entry.path : row.text), ["docs/", "a.md"]);
  store.toggle("docs");
  await settle();
  state = get(store);
  rows = treeRows(state);
  assert.deepEqual(rows.map(row => row.kind === "folder" ? `${row.path}/` : row.kind === "entry" ? row.entry.path : row.text), ["docs/", "docs/b.md", "a.md"]);
  store.toggle("docs");
  assert.equal(treeRows(get(store)).length, 2, "collapsing hides the folder's rows");
  assert.deepEqual(folderChain("a/b/c.md"), ["a", "a/b"]);
});

test("a new index revision drops everything built from the old one and reloads it", async () => {
  const api = vaultApi();
  const index = fakeIndex();
  const store = createVaultStore({ api, indexClient: index, vaultId: "v" });
  await store.refresh();
  await settle();
  api.state.revision = 2;
  api.state.listings[""] = [entry("new.md")];
  await store.refresh();
  await settle();
  const state = get(store);
  assert.equal(state.revision, 2);
  assert.deepEqual(state.listings[""].items.map(item => item.path), ["new.md"]);
  assert.equal(index.jobs.length, 2, "the graph was rebuilt for the new revision");
  assert.equal(index.jobs[0].cancelled, true);
});

test("an answer from an older revision is dropped and a newer one triggers a re-read", async () => {
  const api = vaultApi();
  const store = createVaultStore({ api, indexClient: fakeIndex(), vaultId: "v" });
  await store.refresh();
  await settle();
  // Simulate the vault moving on between the refresh and a later listing.
  api.state.revision = 3;
  api.state.listings[""] = [entry("fresh.md")];
  await store.loadFolder("");
  await settle();
  await settle();
  const state = get(store);
  assert.equal(state.revision, 3, "the newer revision was adopted through a refresh");
  assert.deepEqual(state.listings[""].items.map(item => item.path), ["fresh.md"]);
});

test("a slow listing for a vault state that has since been replaced cannot overwrite the new one", async () => {
  const gate = deferred();
  const api = vaultApi();
  const store = createVaultStore({ api, indexClient: fakeIndex(), vaultId: "v" });
  await store.refresh();
  await settle();
  api.state.hold = async prefix => { if (prefix === "") await gate.promise; };
  const slow = store.loadFolder("");
  api.state.hold = null;
  api.state.revision = 2;
  api.state.listings[""] = [entry("second.md")];
  await store.refresh();
  await settle();
  gate.resolve();
  await slow;
  await settle();
  assert.deepEqual(get(store).listings[""].items.map(item => item.path), ["second.md"]);
});

test("search keeps only the latest query's results", async () => {
  const gates = { first: deferred() };
  const api = vaultApi();
  const store = createVaultStore({ api, indexClient: fakeIndex(), vaultId: "v" });
  await store.refresh();
  await settle();
  const original = api.queryLinks;
  api.queryLinks = async (vaultId, query) => { if (query.kind === "search" && query.text === "first") await gates.first.promise; return original(vaultId, query); };
  const first = store.setSearch("first");
  const second = store.setSearch("second");
  await second;
  gates.first.resolve();
  await first;
  const state = get(store);
  assert.equal(state.search.text, "second");
  assert.deepEqual(state.search.items.map(item => item.path), ["hit-second.md"]);
  await store.setSearch("  ");
  assert.equal(get(store).search.status, "idle");
});

test("suspend stops a graph read that is still going and leaves nothing half-built", async () => {
  const gate = deferred();
  const api = vaultApi();
  const index = fakeIndex();
  const store = createVaultStore({ api, indexClient: index, vaultId: "v" });
  api.state.hold = async kind => { if (kind === "links") await gate.promise; };
  const refreshing = store.refresh();
  await settle();
  store.suspend();
  gate.resolve();
  await refreshing;
  await settle();
  assert.equal(get(store).graph.result, null);
  assert.equal(index.jobs[0].cancelled, true);
});

test("tree rows show loading, error, empty and continue rows explicitly", () => {
  const rows = treeRows({ expanded: ["a", "b", "c"], listings: {
    "": { items: [entry("a", "folder"), entry("b", "folder"), entry("c", "folder")], total: 3, nextOffset: null, loading: false, error: null },
    a: { items: [], total: 0, nextOffset: null, loading: true, error: null },
    b: { items: [], total: 0, nextOffset: null, loading: false, error: "denied" },
    c: { items: [entry("c/x.md")], total: 500, nextOffset: 200, loading: false, error: null },
  } });
  const text = rows.filter(row => row.kind === "status").map(row => row.text);
  assert.deepEqual(text, ["Loading…", "denied", "Show more (1 of 500)"]);
  assert.deepEqual(treeRows({ expanded: [], listings: {} }).map(row => row.text), ["Loading…"]);
});

test("a reference definition that arrives on a later page also resolves earlier uses", async () => {
  const text = `[open][ref]\n\nmiddle paragraph one\n\nmiddle paragraph two\n\n[ref]: Other.md\n`;
  const api = backend({ "r.md": { text } }, { pageBytes: 24, resolutions: { "Other.md": { status: "resolved", path: "Other.md", candidates: [] } } });
  const s = session(api, "r.md");
  await s.start();
  await s.loadAll();
  const out = html(get(s));
  assert.match(out, /data-path="Other\.md"[^>]*>open<\/a>/, "the early forward reference became a link");
  assert.equal((out.match(/middle paragraph/g) ?? []).length, 2, "no section repeated by the rerender");
});
