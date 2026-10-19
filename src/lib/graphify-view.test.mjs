import test from "node:test";
import assert from "node:assert/strict";
import { get } from "svelte/store";
import { importTs } from "./test-loader.mjs";

const { createGraphifyStore, isGone, REPLACED_TEXT, refKey } = await importTs("graphify-view");

const settle = () => new Promise(resolve => setTimeout(resolve, 0));
const ref = (id, idKind = "text") => ({ id, idKind });
const node = (id, extra = {}) => ({ ref: ref(id), label: id, fileType: "code", sourceFile: "a.py", sourceLocation: "L1", community: null, origin: null, attributes: {}, ...extra });
const link = (ordinal, source, target) => ({ ordinal, id: `graphify:H:link:${ordinal}`, nativeId: null, source: ref(source), target: ref(target), key: null, relation: "calls", directed: true, confidence: "EXTRACTED", confidenceScore: 1, sourceFile: null, sourceLocation: null, origin: null, attributes: {}, dangling: false });

function artifact(id, hash, counts) {
  return { artifactId: id, artifactHash: hash, folder: "C:/p", sourceRoot: "C:/p", provenance: "verified", directed: true, multigraph: false, counts, metadata: {}, bytes: 10, issues: [], issueCount: 0, outputKind: "project", provenanceCounts: {} };
}
function backend(options = {}) {
  const log = [];
  let generation = 0;
  const builds = options.builds ?? [{ id: "art-1", hash: "H1", nodes: [node("a"), node("b")], links: [link(0, "a", "b")], groups: [], communities: [] }];
  const state = { gone: new Set(), replaced: new Set(), mixHash: false };
  const current = () => builds[Math.min(generation, builds.length - 1)];
  const api = {
    log, state,
    async openGraphifyArtifact(folder) {
      log.push(["open", folder]);
      const build = builds[Math.min(generation++, builds.length - 1)];
      return artifact(build.id, build.hash, { nodes: build.nodes.length, links: build.links.length, hyperedges: build.groups.length, communities: build.communities.length });
    },
    async readGraphify(artifactId, section, offset, limit) {
      log.push(["read", artifactId, section, offset]);
      if (state.gone.has(artifactId)) throw new Error("This Graphify output is no longer available: it was replaced");
      const build = builds.find(item => item.id === artifactId) ?? current();
      const rows = { nodes: build.nodes, links: build.links, hyperedges: build.groups, communities: build.communities, metadata: [] }[section];
      const items = rows.slice(offset, offset + Math.min(limit, options.pageSize ?? limit));
      return { artifactId, artifactHash: state.mixHash && section === "links" ? "OTHER" : build.hash, section, total: rows.length, offset, items, nextOffset: offset + items.length < rows.length ? offset + items.length : null };
    },
    async checkGraphifyArtifact(artifactId) {
      log.push(["check", artifactId]);
      return { artifactId, current: !state.replaced.has(artifactId), message: null };
    },
    async queryGraphify(artifactId, query) {
      log.push(["query", artifactId, query.kind]);
      if (state.gone.has(artifactId)) throw new Error("no longer available");
      const build = builds.find(item => item.id === artifactId) ?? current();
      const items = query.kind === "search"
        ? [...build.nodes.filter(item => item.label.includes(query.text)).map(node => ({ kind: "node", node })), ...build.links.filter(() => query.text === "calls").map(link => ({ kind: "link", link })), ...build.groups.filter(() => query.text === "G").map(hyperedge => ({ kind: "hyperedge", hyperedge }))]
        : query.kind === "neighbors" ? build.links : [];
      return { artifactId, artifactHash: build.hash, kind: query.kind, total: items.length, offset: query.offset ?? 0, items, nextOffset: null };
    },
  };
  return api;
}
function fakeIndex() {
  return {
    cancelled: 0,
    begin() {
      const job = { chunks: [], push(chunk) { this.chunks.push(chunk); }, finish() { return Promise.resolve({ kind: "graphify", model: { nodes: [], edges: [] }, stats: { nodes: this.chunks.length, links: 0, dangling: 0, confidence: {}, hyperedges: 0, communities: 0 } }); }, cancel() {} };
      return job;
    },
    cancel() { this.cancelled++; },
    dispose() {},
  };
}

test("opening admits a new artifact and reads every section page by page", async () => {
  const api = backend({ pageSize: 1, builds: [{ id: "art-1", hash: "H1", nodes: [node("a"), node("b"), node("c")], links: [link(0, "a", "b"), link(1, "b", "c")], groups: [{ id: "h", label: "G", relation: null, nodeIds: [ref("a")], confidence: null, sourceFile: null, attributes: {} }], communities: [{ id: 1, label: null, size: 2 }] }] });
  const store = createGraphifyStore({ api, indexClient: fakeIndex() });
  await store.open("C:/p");
  const state = get(store);
  assert.equal(state.status, "ready");
  assert.equal(state.artifact.artifactId, "art-1");
  assert.equal(state.nodes.length, 3);
  assert.equal(state.links.length, 2);
  assert.equal(state.hyperedges.length, 1);
  assert.deepEqual(state.progress.nodes, { loaded: 3, total: 3 });
  assert.equal(api.log.filter(item => item[0] === "read" && item[2] === "nodes").length, 3, "paged one record at a time");
  assert.equal(api.log.filter(item => item[0] === "open").length, 1);
});

test("a page from another build is never mixed in", async () => {
  const api = backend();
  api.state.mixHash = true;
  const store = createGraphifyStore({ api, indexClient: fakeIndex() });
  await store.open("C:/p");
  const state = get(store);
  assert.equal(state.status, "error");
  assert.match(state.error, /different Graphify build/);
  assert.equal(state.result, null);
});

test("an output replaced after it was opened keeps the opened build and offers a refresh; refresh admits a new artifact id", async () => {
  const builds = [
    { id: "art-1", hash: "H1", nodes: [node("a")], links: [], groups: [], communities: [] },
    { id: "art-2", hash: "H2", nodes: [node("a"), node("z")], links: [], groups: [], communities: [] },
  ];
  const api = backend({ builds });
  const store = createGraphifyStore({ api, indexClient: fakeIndex() });
  await store.open("C:/p");
  assert.equal(get(store).nodes.length, 1);
  api.state.replaced.add("art-1");
  await store.check();
  let state = get(store);
  assert.equal(state.stale, REPLACED_TEXT);
  assert.equal(state.status, "ready", "what was read stays visible");
  assert.equal(state.nodes.length, 1);
  await store.setSearch("a");
  assert.equal(get(store).search.items.length, 1, "the immutable snapshot is still searchable");
  await store.open("C:/p");
  state = get(store);
  assert.equal(state.stale, null);
  assert.equal(state.artifact.artifactId, "art-2");
  assert.equal(state.nodes.length, 2);
});

test("a read that fails as gone during opening is reported, not shown as an empty graph", async () => {
  const api = backend();
  api.state.gone.add("art-1");
  const store = createGraphifyStore({ api, indexClient: fakeIndex() });
  await store.open("C:/p");
  const state = get(store);
  assert.equal(state.status, "error");
  assert.equal(state.stale, REPLACED_TEXT);
  assert.equal(state.result, null);
});

test("opening again supersedes a read still in flight", async () => {
  const gate = Promise.withResolvers();
  const api = backend({ builds: [
    { id: "art-1", hash: "H1", nodes: [node("a")], links: [], groups: [], communities: [] },
    { id: "art-2", hash: "H2", nodes: [node("b"), node("c")], links: [], groups: [], communities: [] },
  ] });
  const original = api.readGraphify;
  api.readGraphify = async (artifactId, ...rest) => { if (artifactId === "art-1") await gate.promise; return original(artifactId, ...rest); };
  const store = createGraphifyStore({ api, indexClient: fakeIndex() });
  const first = store.open("C:/p");
  await settle();
  await store.open("C:/p");
  gate.resolve();
  await first;
  const state = get(store);
  assert.equal(state.artifact.artifactId, "art-2");
  assert.deepEqual(state.nodes.map(item => item.ref.id), ["b", "c"]);
});

test("search and inspection results belong to the build they were asked of", async () => {
  const api = backend();
  const store = createGraphifyStore({ api, indexClient: fakeIndex() });
  await store.open("C:/p");
  await store.setSearch("a");
  assert.deepEqual(get(store).search.items.map(item => item.node.ref.id), ["a"]);
  await store.setSearch("calls");
  assert.deepEqual(get(store).search.items.map(item => item.kind), ["link"], "a relationship result is a link record, never cast to a node");
  await store.inspect(ref("a"));
  assert.equal(get(store).neighbors.key, refKey(ref("a")));
  assert.equal(get(store).neighbors.items.length, 1);
  assert.notEqual(refKey(ref("1")), refKey(ref("1", "number")));
  assert.equal(isGone("The output is no longer available"), true);
  assert.equal(isGone("permission denied"), false);
  await store.setSearch("");
  assert.equal(get(store).search.status, "idle");
});

test("suspend during a read stops it and does not leave a half-read graph shown as ready", async () => {
  const gate = Promise.withResolvers();
  const api = backend();
  const original = api.readGraphify;
  api.readGraphify = async (...args) => { await gate.promise; return original(...args); };
  const store = createGraphifyStore({ api, indexClient: fakeIndex() });
  const opening = store.open("C:/p");
  await settle();
  store.suspend();
  gate.resolve();
  await opening;
  assert.equal(get(store).status, "idle");
  assert.equal(get(store).result, null);
});
