import test from "node:test";
import assert from "node:assert/strict";
import { get } from "svelte/store";
import { importTs } from "./test-loader.mjs";

const { createSystemOverviewStore, deriveModel, effectiveIndex, selectedSystemResult } = await importTs("system-overview");

const caps = { available: true, probed: true, runtime: { source: "env", python: null, version: null, treeSitter: null, error: null }, profiles: [], hint: null };
const job = (state, extra = {}) => ({ jobId: "job-1", snapshotId: null, folder: "C:/repo", state, indexedAt: null, sourceHash: null, completedFiles: 0, totalFiles: 10, stale: null, error: null, capabilities: caps, coverage: null, ...extra });
const complete = (snapshotId, extra = {}) => job("complete", { snapshotId, indexedAt: 111, sourceHash: "hash", completedFiles: 10, ...extra });
const group = (id, label, extra = {}) => ({ id: `scc:${id}`, kind: "scc", level: "files", label, cyclic: false, memberCount: 1, members: [id], membersTruncated: false, internalEdges: 0, linksTotal: 0, linksTruncated: false, links: [], ...extra });
const file = (id, path) => ({ id, path, sha256: "", bytes: 1, language: "typescript", status: "parsed", diagnostics: [] });
const insight = (state, extra = {}) => ({ requestId: "sys-1", folder: "C:/repo", kind: "systemOverview", state, message: null, result: null, snapshotId: "snap-1", createdAt: 10, producer: { instanceId: "arch", generation: 1, launchId: "l", sessionId: "s" }, ...extra });
const payload = { kind: "systemOverview", schemaVersion: 1, snapshotId: "snap-1", nodes: [{ id: "app", kind: "application", label: "App", parentId: null, description: "", evidenceIds: [] }], edges: [], evidence: [], coverage: [], limitations: [] };

function setup(overrides = {}) {
  let nextTimer = 1;
  const timers = new Map();
  let now = 0;
  const clock = { now: () => now, setTimeout: (fn, ms) => { const id = nextTimer++; timers.set(id, { fn, at: now + ms }); return id; }, clearTimeout: id => { timers.delete(id); } };
  const calls = [];
  const flush = async () => { for (let i = 0; i < 20; i++) await Promise.resolve(); await new Promise(resolve => setImmediate(resolve)); };
  async function tick(ms = 1000) {
    now += ms;
    for (const [id, timer] of [...timers]) if (timer.at <= now) { timers.delete(id); timer.fn(); }
    await flush();
  }
  const state = { selection: { selected: complete("snap-1"), latestAttempt: complete("snap-1") }, pages: {}, insight: null };
  const api = {
    getSourceIndex: async folder => { calls.push(["get", folder]); return state.selection; },
    startSourceIndex: async folder => { calls.push(["start", folder]); return job("inventory"); },
    readSourceIndexJob: async id => { calls.push(["read", id]); return state.read ? state.read() : complete("snap-2"); },
    cancelSourceIndex: async id => { calls.push(["cancel", id]); return job("cancelled"); },
    querySourceIndex: async (snapshot, query) => {
      calls.push(["query", snapshot, query.kind, query.level ?? "", query.groupBy ?? "", query.offset]);
      if (state.query) return state.query(snapshot, query);
      return { total: 0, offset: query.offset, nextOffset: null, items: [] };
    },
    requestSystemVisualization: async (...args) => { calls.push(["request", ...args]); return state.admit ?? insight("pending"); },
    readProjectInsight: async id => { calls.push(["readInsight", id]); return state.insightRead ?? insight("pending"); },
    getProjectInsight: async (folder, kind) => { calls.push(["getInsight", folder, kind]); return state.insight; },
    ...overrides,
  };
  const memory = new Map();
  const store = createSystemOverviewStore({ api, clock, storage: { getItem: key => memory.get(key) ?? null, setItem: (key, value) => memory.set(key, value) } });
  const hooks = { opened: [], returned: [], following: true, quiescent: true };
  store.attach({
    architectId: () => "arch", architectSession: () => ({ id: "arch", generation: 1, sessionId: "s1", ready: true }),
    openArchitect: async id => { hooks.opened.push(id); return true; }, following: () => hooks.following, quiescent: () => hooks.quiescent,
    returnToOverview: ticket => hooks.returned.push(ticket.key),
  });
  return { store, api, calls, state, tick, flush, hooks, memory, snapshot: () => get(store) };
}
const count = (calls, name) => calls.filter(call => call[0] === name).length;

test("opening reads the saved index and saved diagram passively; nothing asks the Architect or starts work", async () => {
  const h = setup();
  await h.store.enter({ folder: "C:/repo" });
  await h.flush();
  const s = h.snapshot();
  assert.equal(s.folder, "C:/repo");
  assert.equal(s.index.selected.snapshotId, "snap-1");
  assert.equal(count(h.calls, "start"), 0, "a saved index is never re-indexed on mount");
  assert.equal(count(h.calls, "request"), 0, "no model request on mount");
  assert.equal(h.hooks.opened.length, 0);
  assert.deepEqual(h.calls.filter(call => call[0] === "getInsight").map(call => call[2]), ["systemOverview", "helicopter"]);
});

test("a repository with no index is indexed locally once; reopening or failing never starts another silently", async () => {
  const h = setup();
  h.state.selection = { selected: null, latestAttempt: null };
  await h.store.enter({ folder: "C:/repo" });
  await h.flush();
  assert.equal(count(h.calls, "start"), 1);
  assert.equal(h.snapshot().index.attempt.state, "inventory");
  await h.store.enter({ folder: "C:/repo" });
  await h.store.switchFolder("C:/other");
  await h.store.switchFolder("C:/repo");
  await h.flush();
  assert.equal(count(h.calls, "start"), 2, "one per repository, not per open");
});

test("progress polls the job; completion selects the new snapshot and resets level data; a failed refresh keeps the last good index", async () => {
  const h = setup();
  await h.store.enter({ folder: "C:/repo" });
  await h.store.refreshIndex();
  await h.flush();
  assert.equal(count(h.calls, "start"), 1);
  // The job runs, then completes; the backend selects the new complete snapshot.
  let polls = 0;
  h.state.read = () => (++polls < 2 ? job("parsing", { completedFiles: 5 }) : complete("snap-2"));
  h.state.selection = { selected: complete("snap-2"), latestAttempt: complete("snap-2") };
  await h.tick();
  assert.equal(h.snapshot().index.attempt.completedFiles, 5);
  await h.tick();
  assert.equal(h.snapshot().index.selected.snapshotId, "snap-2");
  // A later failing refresh: the selected complete index stays, the attempt carries the error.
  h.state.selection = { selected: complete("snap-2"), latestAttempt: job("failed", { error: "parser crashed", jobId: "job-9" }) };
  h.state.read = () => job("failed", { error: "parser crashed", jobId: "job-9" });
  await h.store.refreshIndex();
  await h.tick();
  await h.tick();
  const s = h.snapshot();
  assert.equal(s.index.selected.snapshotId, "snap-2");
  assert.equal(s.index.attempt.error, "parser crashed");
  assert.equal(effectiveIndex(s).snapshotId, "snap-2");
});

test("a partial attempt can be viewed explicitly without replacing the selected complete index", async () => {
  const h = setup();
  h.state.selection = { selected: complete("snap-1"), latestAttempt: job("partial", { snapshotId: "snap-p", jobId: "job-p" }) };
  await h.store.enter({ folder: "C:/repo" });
  await h.flush();
  assert.equal(effectiveIndex(h.snapshot()).snapshotId, "snap-1");
  h.store.viewPartial(true);
  assert.equal(effectiveIndex(h.snapshot()).snapshotId, "snap-p");
  h.store.viewPartial(false);
  assert.equal(effectiveIndex(h.snapshot()).snapshotId, "snap-1");
});

test("late index, level and search results of a previous folder are dropped after a folder switch", async () => {
  const gates = [];
  const h = setup();
  const original = h.api.getSourceIndex;
  h.api.getSourceIndex = folder => new Promise(resolve => gates.push(() => resolve({ selected: complete(`snap-${folder}`, { folder }), latestAttempt: null })));
  const first = h.store.enter({ folder: "C:/one" });
  await h.flush();
  const second = h.store.switchFolder("C:/two");
  await h.flush();
  gates[1]();
  await h.flush();
  gates[0]();
  await Promise.all([first, second]);
  await h.flush();
  assert.equal(h.snapshot().folder, "C:/two");
  assert.equal(h.snapshot().index.selected.snapshotId, "snap-C:/two", "the late answer for C:/one was refused");
  h.api.getSourceIndex = original;
  // A slow level load for the first folder cannot land on the second.
  const slow = [];
  h.state.query = (snapshot, query) => new Promise(resolve => slow.push(() => resolve({ total: 1, offset: 0, nextOffset: null, items: query.kind === "files" ? [file("f1", "a.ts")] : [group("f1", "a.ts")] })));
  h.state.selection = { selected: complete("snap-A", { folder: "C:/a" }), latestAttempt: null };
  await h.store.switchFolder("C:/a");
  const loading = h.store.ensureLevel("files");
  await h.flush();
  h.state.selection = { selected: complete("snap-B", { folder: "C:/b" }), latestAttempt: null };
  await h.store.switchFolder("C:/b");
  while (slow.length) { slow.shift()(); await h.flush(); }
  await loading;
  assert.equal(h.snapshot().levels.files.status, "idle", "nothing from the old folder is shown");
  assert.equal(h.snapshot().levels.files.snapshotId, null);
});

test("levels page the overview, load facts, and become directory aggregates above the Sigma bound", async () => {
  const h = setup();
  h.state.query = (snapshot, query) => {
    if (query.kind === "overview" && query.groupBy === "scc") {
      const total = query.offset === 0 && h.state.huge ? 6000 : 250;
      const size = Math.min(query.limit, total - query.offset);
      return Promise.resolve({ total, offset: query.offset, nextOffset: query.offset + size < total ? query.offset + size : null, items: Array.from({ length: size }, (_, i) => group(`f${query.offset + i}`, `src/f${query.offset + i}.ts`)) });
    }
    if (query.kind === "overview") return Promise.resolve({ total: 1, offset: 0, nextOffset: null, items: [{ id: "dir:src", kind: "directory", level: "files", label: "src", path: "src", cyclic: false, memberCount: 6000, members: [], membersTruncated: true, internalEdges: 0, linksTotal: 0, linksTruncated: false, links: [] }] });
    const total = 250;
    const size = Math.min(query.limit, total - query.offset);
    return Promise.resolve({ total, offset: query.offset, nextOffset: query.offset + size < total ? query.offset + size : null, items: Array.from({ length: size }, (_, i) => file(`f${query.offset + i}`, `src/f${query.offset + i}.ts`)) });
  };
  await h.store.enter({ folder: "C:/repo" });
  await h.store.ensureLevel("files");
  await h.flush();
  let level = h.snapshot().levels.files;
  assert.equal(level.status, "ready");
  assert.equal(level.mode, "nodes");
  assert.equal(level.groups.length, 250, "all pages were read");
  assert.equal(level.files.length, 250);
  const { model } = deriveModel(h.snapshot(), "files");
  assert.equal(model.nodes.filter(node => !node.synthetic).length, 250);
  const before = count(h.calls, "query");
  await h.store.ensureLevel("files");
  assert.equal(count(h.calls, "query"), before, "ready level data is reused");
  h.state.huge = true;
  h.state.selection = { selected: complete("snap-2"), latestAttempt: null };
  await h.store.switchFolder("C:/repo");
  await h.store.ensureLevel("files");
  await h.flush();
  level = h.snapshot().levels.files;
  assert.equal(level.mode, "directories");
  assert.equal(level.total, 6000);
  const directories = deriveModel(h.snapshot(), "files").model;
  assert.equal(directories.nodes[0].kind, "directory");
});

test("search ignores stale responses; empty text clears results; coverage and neighbours are bounded pages", async () => {
  const h = setup();
  const waiting = [];
  h.state.query = (snapshot, query) => new Promise(resolve => waiting.push(() => resolve({ total: 1, offset: 0, nextOffset: null, items: [{ id: query.text ?? query.kind }] })));
  await h.store.enter({ folder: "C:/repo" });
  const a = h.store.runSearch("files", "alpha");
  const b = h.store.runSearch("files", "beta");
  await h.flush();
  waiting[1]();
  await h.flush();
  waiting[0]();
  await Promise.all([a, b]);
  assert.equal(h.snapshot().search.files.text, "beta");
  assert.equal(h.snapshot().search.files.items[0].id, "beta");
  await h.store.runSearch("files", "  ");
  assert.equal(h.snapshot().search.files.status, "idle");
  assert.ok(h.calls.filter(call => call[2] === "search").every(call => call[call.length - 1] === 0));
});

test("view state (tab, search, selection, camera, expansion, scope) survives re-entering the page; drilling pushes a back stack", async () => {
  const h = setup();
  await h.store.enter({ folder: "C:/repo" });
  h.store.setTab("files");
  h.store.setSearch("files", "util");
  h.store.setExpanded("files", ["dir:src"]);
  h.store.select("files", "n1", null, "ref1");
  h.store.setCamera("files", { x: 1, y: 2, ratio: 0.5 });
  await h.store.enter({ folder: "C:/repo" });
  await h.store.enter({ resume: true });
  let s = h.snapshot();
  assert.equal(s.tab, "files");
  assert.equal(s.views.files.search, "util");
  assert.deepEqual(s.views.files.expanded, ["dir:src"]);
  assert.equal(s.views.files.selectedNode, "n1");
  assert.deepEqual(s.views.files.camera, { x: 1, y: 2, ratio: 0.5 });
  h.store.drill("functions", { label: "App", exact: ["src/app.ts"], prefixes: [] });
  s = h.snapshot();
  assert.equal(s.tab, "functions");
  assert.deepEqual(s.views.functions.scope.exact, ["src/app.ts"]);
  assert.equal(s.back.length, 1);
  assert.equal(h.store.back(), true);
  assert.equal(h.snapshot().tab, "files");
  assert.equal(h.snapshot().views.files.search, "util");
  assert.equal(h.store.back(), false);
  // A different project through the page entry switches and resets; the saved tab preference survives in storage.
  await h.store.enter({ folder: "C:/other" });
  assert.equal(h.snapshot().folder, "C:/other");
  assert.equal(h.snapshot().views.files.search, "");
  assert.match(h.memory.get("omp-manager.system-overview"), /C:\/other/);
});

test("explicit system generation admits through the ticket, opens the Architect once, then shows the matching result and returns", async () => {
  const h = setup();
  await h.store.enter({ folder: "C:/repo" });
  const outcome = await h.store.generateSystem(false);
  assert.equal(outcome.admitted, true);
  assert.equal(count(h.calls, "request"), 1);
  assert.deepEqual(h.hooks.opened, ["arch"]);
  assert.equal(h.snapshot().tickets.length, 1);
  assert.equal(h.snapshot().tickets[0].snapshotId, "snap-1");
  // The saved typed result arrives: the status is the authority.
  h.state.insightRead = insight("completed", { result: payload, resultHash: "r" });
  await h.tick();
  await h.tick();
  const s = h.snapshot();
  assert.equal(s.system.selectedId, "sys-1");
  assert.equal(selectedSystemResult(s).result.nodes[0].id, "app");
  assert.deepEqual(h.hooks.returned.length, 1);
  assert.equal(s.tab, "system");
  assert.equal(count(h.calls, "request"), 1, "no resend");
  const { model } = deriveModel(s, "system");
  assert.equal(model.nodes[0].label, "App");
});

test("navigation after the click makes the result a late action; the store never auto-opens it", async () => {
  const h = setup();
  await h.store.enter({ folder: "C:/repo" });
  await h.store.generateSystem(false);
  h.store.noteNavigation();
  h.state.insightRead = insight("completed", { result: payload, resultHash: "r" });
  await h.tick();
  await h.tick();
  assert.equal(h.hooks.returned.length, 0);
  assert.equal(h.snapshot().tickets[0].lateResult, true);
  assert.equal(h.store.openResult(h.snapshot().tickets[0].key), true);
  assert.equal(h.hooks.returned.length, 1);
});

test("a late older result does not replace the selected newer revision, and regeneration names its predecessor", async () => {
  const h = setup();
  h.state.insight = insight("completed", { requestId: "new", createdAt: 50, result: payload, resultHash: "n" });
  await h.store.enter({ folder: "C:/repo" });
  await h.flush();
  assert.equal(h.snapshot().system.selectedId, "new");
  h.store.receive(insight("completed", { requestId: "old", createdAt: 5, result: payload, resultHash: "o" }));
  assert.equal(h.snapshot().system.selectedId, "new");
  assert.equal(h.snapshot().system.revisions.length, 2);
  // A late read of a request that already completed cannot regress it to pending.
  h.store.receive(insight("pending", { requestId: "new", createdAt: 50 }));
  assert.equal(h.snapshot().system.revisions.find(item => item.requestId === "new").state, "completed");
  h.state.admit = insight("pending", { requestId: "newer", createdAt: 60 });
  await h.store.generateSystem(true);
  const request = h.calls.find(call => call[0] === "request");
  assert.equal(request[6], "new", "supersedesRequestId");
});

test("Generate after a failed request supersedes it, so a stopped Architect session never blocks asking again", async () => {
  const h = setup();
  await h.store.enter({ folder: "C:/repo" });
  await h.flush();
  h.store.receive(insight("failed", { requestId: "dead", createdAt: 70, message: "The Architect session that was asked for this result was stopped, switched or replaced before it answered. Ask again." }));
  h.state.admit = insight("pending", { requestId: "fresh", createdAt: 80 });
  await h.store.generateSystem(false);
  assert.equal(h.calls.find(call => call[0] === "request")[6], "dead");
  assert.equal(h.snapshot().system.revisions[0].requestId, "fresh");
});

test("structure statuses go to the structure sink; folder switch invalidates auto-return of other projects", async () => {
  const h = setup();
  const received = [];
  h.store.attachStructureSink({ receive: status => received.push(status.requestId) });
  await h.store.enter({ folder: "C:/repo" });
  h.store.receive(insight("completed", { requestId: "st-1", kind: "structureCurrent" }));
  h.store.receive(insight("completed", { requestId: "sys-x" }));
  assert.deepEqual(received, ["st-1"]);
  h.state.admit = insight("pending", { requestId: "st-2", kind: "structureProposed", sourceId: "src-1" });
  const host = h.store.structureHost();
  h.state.insightRead = h.state.admit;
  assert.equal(host.snapshotId(), "snap-1");
  assert.equal(host.folder(), "C:/repo");
  const outcome = await host.generate({ kind: "structureProposed", folder: "C:/repo", snapshotId: "snap-1", sourceId: "src-1", area: "structure" });
  assert.equal(outcome.admitted, true);
  h.store.noteSourceSelection("other-source");
  h.state.insightRead = insight("completed", { requestId: "st-2", kind: "structureProposed", sourceId: "src-1", result: { kind: "structureProposed" }, resultHash: "x" });
  await h.tick();
  await h.tick();
  assert.equal(h.hooks.returned.length, 0, "selecting another source invalidates the automatic return");
  assert.equal(h.snapshot().tickets[0].lateResult, true);
});

test("an open neighbourhood keeps its own rows while another node's callers load into the inspector", async () => {
  const h = setup();
  const row = (peer, direction) => ({ kind: "edge", direction, self: false, peerId: peer, peerLabel: `src/${peer}.ts`, edge: { id: `e-${peer}`, sourceId: "c", targetId: peer, relation: "imports", resolution: "resolvedStatic", evidenceIds: [] }, sites: [], sitesTruncated: false });
  h.state.query = (snapshot, query) => Promise.resolve({ total: 2, offset: 0, nextOffset: null, items: query.kind === "neighbors" ? (query.id === "c" ? [row("p1", "out"), row("p2", "in")] : [row("other", "out")]) : [] });
  await h.store.enter({ folder: "C:/repo" });
  h.store.openNeighborhood("files", { id: "c", label: "src/c.ts", path: "src/c.ts" });
  await h.flush();
  h.store.select("files", "p1", null, "p1");
  await h.flush();
  const s = h.snapshot();
  assert.equal(s.inspector.id, "p1");
  assert.equal(s.hood.id, "c");
  const { model } = deriveModel(s, "files");
  assert.deepEqual(model.nodes.map(node => node.id).sort(), ["c", "p1", "p2"]);
});

// ---- Helicopter --------------------------------------------------------------------------------------------------

const helicopterPayload = {
  kind: "helicopter", schemaVersion: 1, snapshotId: "snap-1", purpose: "Lets customers book visits.",
  nodes: [
    { id: "customer", kind: "actor", label: "Customer", parentId: null, description: "", evidenceIds: ["e1"] },
    { id: "journey", kind: "businessGroup", label: "Booking a visit", parentId: null, description: "", evidenceIds: [] },
    { id: "book", kind: "activity", label: "Clinic books the visit", parentId: "journey", description: "", evidenceIds: ["e1"] },
    { id: "done", kind: "outcome", label: "Visit is confirmed", parentId: null, description: "Needs clarification: whether a reminder is sent", evidenceIds: ["e1"] },
  ],
  edges: [
    { id: "h1", sourceId: "customer", targetId: "book", label: "", evidenceIds: ["e1"], certainty: "inferred" },
    { id: "h2", sourceId: "book", targetId: "done", label: "", evidenceIds: [], certainty: "unknown" },
  ],
  capabilities: [{ id: "cap", label: "Book a visit", description: "", nodeIds: ["book", "done"], evidenceIds: ["e1"] }],
  evidence: [{ id: "e1", path: "src/app.ts", lineStart: 1, lineEnd: 2, contentHash: "h" }], coverage: [], limitations: [],
};
const helicopterInsight = (state, extra = {}) => insight(state, { requestId: "hel-1", kind: "helicopter", createdAt: 20, ...extra });
const completedHelicopter = (extra = {}) => helicopterInsight("completed", { result: helicopterPayload, resultHash: "r", ...extra });

test("Helicopter mounts passively: its saved views are read, nothing is generated or invented", async () => {
  const h = setup();
  await h.store.enter({ folder: "C:/repo" });
  await h.flush();
  assert.deepEqual(h.calls.filter(call => call[0] === "getInsight").map(call => call[2]).sort(), ["helicopter", "systemOverview"]);
  assert.equal(count(h.calls, "request"), 0);
  assert.equal(h.hooks.opened.length, 0);
  h.store.setTab("helicopter");
  const s = h.snapshot();
  assert.equal(s.tab, "helicopter");
  assert.deepEqual(deriveModel(s, "helicopter").model, { nodes: [], edges: [] }, "no generated view: nothing is drawn");
  assert.equal(count(h.calls, "query"), 0, "the business tab never loads source levels");
  assert.equal(count(h.calls, "request"), 0);
  assert.match(h.memory.get("omp-manager.system-overview"), /"tab":"helicopter"/);
  // System stays the default for a project that was never opened before.
  assert.equal(setup().snapshot().tab, "system");
});

test("a saved Helicopter view selects itself and derives a directed business graph", async () => {
  const h = setup();
  h.state.insight = completedHelicopter();
  await h.store.enter({ folder: "C:/repo" });
  await h.flush();
  // getInsight answers every kind with the same record in this fake; only a record of the asked kind may land in its slice.
  assert.equal(h.snapshot().system.revisions.length, 0);
  assert.equal(h.snapshot().helicopter.selectedId, "hel-1");
  const { model } = deriveModel(h.snapshot(), "helicopter");
  assert.deepEqual(model.nodes.map(node => node.id), ["customer", "journey", "book", "done"]);
  assert.ok(model.edges.length === 2 && model.edges.every(edge => edge.direction === "directed"));
  assert.equal(model.edges[1].grade, "unknown");
  assert.equal(h.snapshot().helicopter.revisions[0].state, "completed");
});

test("Helicopter then System, and System then Helicopter, are two explicit requests to the same Architect with no hidden prerequisite", async () => {
  for (const order of [["helicopter", "system"], ["system", "helicopter"]]) {
    const h = setup();
    const saved = new Map();
    h.api.readProjectInsight = async id => saved.get(id) ?? (id === "hel-1" ? helicopterInsight("pending") : insight("pending"));
    await h.store.enter({ folder: "C:/repo" });
    for (const view of order) {
      h.state.admit = view === "helicopter" ? helicopterInsight("pending") : insight("pending");
      const outcome = await (view === "helicopter" ? h.store.generateHelicopter(false) : h.store.generateSystem(false));
      assert.equal(outcome.admitted, true, view);
    }
    const requests = h.calls.filter(call => call[0] === "request");
    assert.deepEqual(requests.map(call => call[2]), order.map(view => view === "helicopter" ? "helicopter" : "systemOverview"));
    assert.ok(requests.every(call => call[3] === "snap-1" && call[1] === "C:/repo"), "both are bound to the same snapshot of the same project");
    assert.deepEqual(h.hooks.opened, ["arch", "arch"], "both open the one existing Architect; no other session or transfer exists");
    assert.deepEqual(h.calls.map(call => call[0]).filter(name => !["get", "getInsight", "request", "readInsight"].includes(name)), [], "no context-transfer or extra analysis call exists");
    // The helicopter request names no predecessor and the other kind's absence never blocks it.
    assert.deepEqual(requests.find(call => call[2] === "helicopter").slice(4), [null, null, null]);
    assert.equal(h.snapshot().tickets.length, 2);
    assert.equal(h.snapshot().system.admitting, false);
    assert.equal(h.snapshot().helicopter.admitting, false);
    // Each result lands in its own slice and opens its own tab; the other view is untouched.
    saved.set("hel-1", completedHelicopter());
    saved.set("sys-1", insight("completed", { result: payload, resultHash: "s" }));
    await h.tick();
    await h.tick();
    const s = h.snapshot();
    assert.equal(s.helicopter.selectedId, "hel-1");
    assert.equal(s.system.selectedId, "sys-1");
    assert.equal(s.helicopter.revisions.every(item => item.kind === "helicopter"), true);
    assert.equal(s.system.revisions.every(item => item.kind === "systemOverview"), true);
    assert.equal(count(h.calls, "request"), 2, "polling never resends");
  }
});

test("generating one view never admits the other and a click while admitting is a single request", async () => {
  const h = setup();
  await h.store.enter({ folder: "C:/repo" });
  const gate = { release: null };
  h.api.requestSystemVisualization = (...args) => new Promise(resolve => { h.calls.push(["request", ...args]); gate.release = () => resolve(helicopterInsight("pending")); });
  const first = h.store.generateHelicopter(false);
  const second = h.store.generateHelicopter(false);
  await h.flush();
  assert.equal(h.snapshot().helicopter.admitting, true);
  assert.equal(h.snapshot().system.admitting, false, "the other view is not affected");
  gate.release();
  await Promise.all([first, second]);
  assert.equal(count(h.calls, "request"), 1, "a double click joins the admission in flight");
  assert.equal(h.calls.some(call => call[0] === "request" && call[2] === "systemOverview"), false);
  assert.equal(h.snapshot().helicopter.admitting, false);
});

test("a refused Helicopter request stays in the page with its error and leaves the System view alone", async () => {
  const h = setup();
  await h.store.enter({ folder: "C:/repo" });
  h.api.requestSystemVisualization = async () => { throw new Error("Architect is not running"); };
  const outcome = await h.store.generateHelicopter(false);
  assert.equal(outcome.admitted, false);
  assert.equal(h.snapshot().helicopter.error, "Architect is not running");
  assert.equal(h.snapshot().system.error, null);
  assert.equal(h.snapshot().tickets.length, 0);
  assert.equal(h.hooks.opened.length, 0);
});

test("a finished Helicopter result returns to the Helicopter tab and selects exactly the requested revision", async () => {
  const h = setup();
  await h.store.enter({ folder: "C:/repo" });
  h.store.setTab("files");
  h.state.admit = helicopterInsight("pending");
  h.state.insightRead = helicopterInsight("pending");
  await h.store.generateHelicopter(false);
  h.store.noteNavigation();
  h.state.insightRead = completedHelicopter();
  await h.tick();
  await h.tick();
  // Navigation after the click made it a late action; opening it takes the user to the Helicopter tab, never elsewhere.
  assert.equal(h.hooks.returned.length, 0);
  const [ticket] = h.snapshot().tickets;
  assert.equal(ticket.kind, "helicopter");
  assert.equal(ticket.lateResult, true);
  assert.equal(h.snapshot().tab, "files", "nothing stole the screen");
  assert.equal(h.store.openResult(ticket.key), true);
  assert.equal(h.snapshot().tab, "helicopter");
  assert.equal(h.snapshot().helicopter.selectedId, "hel-1");
  assert.deepEqual(h.hooks.returned, [ticket.key]);
});

test("Helicopter revisions: a late older result never replaces the newer selection, regeneration names its predecessor, failures supersede", async () => {
  const h = setup();
  h.state.insight = completedHelicopter({ requestId: "new", createdAt: 50 });
  await h.store.enter({ folder: "C:/repo" });
  await h.flush();
  assert.equal(h.snapshot().helicopter.selectedId, "new");
  h.store.receive(completedHelicopter({ requestId: "old", createdAt: 5 }));
  assert.equal(h.snapshot().helicopter.selectedId, "new");
  assert.equal(h.snapshot().helicopter.revisions.length, 2);
  h.store.receive(helicopterInsight("pending", { requestId: "new", createdAt: 50 }));
  assert.equal(h.snapshot().helicopter.revisions.find(item => item.requestId === "new").state, "completed", "a late read cannot regress a completed revision");
  h.store.selectHelicopterRevision("old");
  assert.equal(h.snapshot().helicopter.selectedId, "old");
  h.store.selectHelicopterRevision("not-completed-or-unknown");
  assert.equal(h.snapshot().helicopter.selectedId, "old");
  h.store.selectHelicopterRevision("new");
  h.state.admit = helicopterInsight("pending", { requestId: "newer", createdAt: 60 });
  await h.store.generateHelicopter(true);
  assert.equal(h.calls.find(call => call[0] === "request")[6], "new", "supersedesRequestId");
  // After a failed request (a stopped Architect session) a plain Generate supersedes it instead of returning the same failure.
  h.store.receive(helicopterInsight("failed", { requestId: "dead", createdAt: 90, message: "The Architect session that was asked for this result was stopped" }));
  h.state.admit = helicopterInsight("pending", { requestId: "fresh", createdAt: 95 });
  await h.store.generateHelicopter(false);
  assert.equal(h.calls.filter(call => call[0] === "request")[1][6], "dead");
});

test("statuses of another kind or project, and unknown kinds, never enter the Helicopter slice", async () => {
  const h = setup();
  const structure = [];
  h.store.attachStructureSink({ receive: status => structure.push(status.requestId) });
  await h.store.enter({ folder: "C:/repo" });
  h.store.receive(completedHelicopter({ folder: "C:/elsewhere" }));
  h.store.receive(completedHelicopter({ kind: "mystery", requestId: "m-1" }));
  h.store.receive(insight("completed", { requestId: "sys-x", result: payload, resultHash: "x" }));
  h.store.receive(insight("completed", { requestId: "st-1", kind: "structureCurrent" }));
  assert.equal(h.snapshot().helicopter.revisions.length, 0);
  assert.equal(h.snapshot().system.revisions.length, 1);
  assert.deepEqual(structure, ["st-1"], "an unknown kind fails closed: it reaches no view at all");
  h.store.receive(completedHelicopter());
  assert.equal(h.snapshot().helicopter.revisions.length, 1);
  assert.equal(h.snapshot().system.revisions.length, 1);
});

test("a saved-view read for the previous folder cannot land after a folder switch", async () => {
  const h = setup();
  const gates = [];
  h.api.getProjectInsight = (folder, kind) => new Promise(resolve => gates.push(() => resolve(kind === "helicopter" ? completedHelicopter({ folder, requestId: `hel-${folder}` }) : null)));
  const first = h.store.enter({ folder: "C:/one" });
  await h.flush();
  const second = h.store.switchFolder("C:/two");
  await h.flush();
  while (gates.length) { gates.shift()(); await h.flush(); }
  await Promise.all([first, second]);
  const s = h.snapshot();
  assert.equal(s.folder, "C:/two");
  assert.ok(s.helicopter.revisions.every(item => item.folder === "C:/two"), "nothing from C:/one remains");
});

test("a replaced or failed Helicopter request is reported, never completed", async () => {
  const h = setup();
  await h.store.enter({ folder: "C:/repo" });
  h.state.admit = helicopterInsight("pending");
  h.api.readProjectInsight = async () => helicopterInsight("failed", { message: "The Architect session that was asked for this result was stopped, switched or replaced before it answered. Ask again." });
  await h.store.generateHelicopter(false);
  await h.tick();
  await h.tick();
  const [ticket] = h.snapshot().tickets;
  assert.equal(ticket.state, "failed");
  assert.match(ticket.message, /stopped, switched or replaced/);
  assert.equal(h.snapshot().helicopter.selectedId, null);
  assert.equal(deriveModel(h.snapshot(), "helicopter").model.nodes.length, 0);
  assert.equal(h.hooks.returned.length, 0);
});
