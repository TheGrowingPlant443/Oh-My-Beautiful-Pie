import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
import { get } from "svelte/store";

const rewrite = (code, from, to) => code.replace(`"${from}"`, JSON.stringify(to));
let source = await readFile(new URL("./structure-overview.ts", import.meta.url), "utf8");
source = rewrite(source, "svelte/store", import.meta.resolve("svelte/store"));
source = rewrite(source, "./structure-view", new URL("./structure-view.ts", import.meta.url).href);
const { createStructureStore, approveCheck, generateBlock, selectedRevision, selectedSource, sourceViewed, supersededBy, NEED_ARCHITECT } =
  await import(`data:text/javascript,${encodeURIComponent(stripTypeScriptTypes(source))}`);

const settle = () => new Promise(resolve => setTimeout(resolve, 0));
const deferred = () => Promise.withResolvers();

const FOLDER = "C:/p";
const row = (id, capturedAt, extra = {}) => ({ sourceId: id, capturedAt, kind: "interactiveMessage", excerpt: `# ${id}`, sha256: `sha-${id}`, byteLength: 10, error: null, ...extra });
let clock = 0;
const status = (id, kind, extra = {}) => ({
  requestId: id, folder: FOLDER, kind, state: "completed", message: null, result: { kind, snapshotId: "snap", sourceId: null, sourceHash: null, entries: [], conventions: [], mappings: [], conflicts: [], evidence: [], limitations: [], schemaVersion: 1 },
  createdAt: ++clock, resultHash: `rh-${id}`, snapshotId: "snap", baseRequestId: null, supersedesRequestId: null, supersededBy: null,
  sourceId: kind === "structureProposed" ? "A" : null, sourceHash: kind === "structureProposed" ? "sha-A" : null, producer: null, approval: null, ...extra,
});

function harness({ architect = { id: "arch", generation: 1, ready: true }, sources = [row("A", 10)], texts = { A: "target markdown" }, chunk = 1000 } = {}) {
  const calls = { generate: [], list: 0, read: [], saved: [], approve: [], open: [] };
  const state = { architect, folder: FOLDER, snapshotId: "snap" };
  const gates = { list: null, saved: null, approve: null, generate: null };
  const saved = { structureCurrent: null, structureProposed: null };
  const host = {
    folder: () => state.folder, snapshotId: () => state.snapshotId, architect: () => state.architect,
    generate: async request => {
      calls.generate.push(request);
      if (gates.generate) await gates.generate.promise;
      return { admitted: true, status: status(`gen-${calls.generate.length}`, request.kind, { state: "pending", result: null, resultHash: null, sourceId: request.sourceId ?? null, sourceHash: null }), error: null, uncertain: false };
    },
    openArchitect: async (id, replay) => { calls.open.push([id, replay]); return true; },
  };
  const api = {
    getProjectInsight: async (folder, kind) => { calls.saved.push([folder, kind]); if (gates.saved) await gates.saved.promise; return saved[kind]; },
    readProjectInsight: async requestId => status(requestId, "structureProposed"),
    listStructureSources: async () => { calls.list++; if (gates.list) await gates.list.promise; return { sessionId: "s1", sources }; },
    readStructureSource: async (_id, _gen, sourceId, offset) => {
      calls.read.push([sourceId, offset]);
      const text = texts[sourceId] ?? "";
      const part = text.slice(offset, offset + chunk);
      return { sourceId, sha256: `sha-${sourceId}`, text: part, nextOffset: offset + chunk < text.length ? offset + chunk : null };
    },
    selectLastStructureSource: async () => sources[0],
    approveStructurePreview: async (requestId, resultHash, sourceHash) => {
      calls.approve.push([requestId, resultHash, sourceHash]);
      if (gates.approve) await gates.approve.promise;
      const current = get(store).revisions.structureProposed.find(item => item.requestId === requestId);
      return { ...current, approval: { approvedAt: 5, resultHash, sourceHash, hasConflicts: false, delivery: "sent", deliveryError: null } };
    },
  };
  const store = createStructureStore();
  store.connect(host, api);
  store.setFolder(FOLDER, "snap");
  return { store, host, api, calls, state, gates, saved, snap: () => get(store) };
}
const ready = async h => { await h.store.refreshSources(); await settle(); return h; };

test("loading, refreshing and selecting never call the Architect to generate", async () => {
  const h = harness({ sources: [row("A", 10), row("B", 20)], texts: { A: "a", B: "b" } });
  h.saved.structureCurrent = status("c1", "structureCurrent");
  await h.store.loadSaved();
  await h.store.refreshSources();
  await h.store.selectSource("A");
  await h.store.readSource("B");
  h.store.receive(status("c2", "structureCurrent"));
  h.store.setFolder(FOLDER, "snap2");
  await settle();
  assert.equal(h.calls.generate.length, 0);
  assert.ok(h.calls.saved.length >= 2);
  assert.equal(h.calls.approve.length, 0);
});

test("sources default to the newest readable source and are read before being considered viewed", async () => {
  const h = harness({ sources: [row("old", 5), row("bad", 30, { error: "not markdown" }), row("new", 20)], texts: { old: "o", new: "n" } });
  assert.equal(sourceViewed(h.snap()), false);
  await ready(h);
  const state = h.snap();
  assert.deepEqual(state.sources.map(item => item.sourceId), ["bad", "new", "old"]);
  assert.equal(state.selectedSourceId, "new");
  assert.equal(state.sessionId, "s1");
  assert.equal(selectedSource(state).sourceId, "new");
  assert.equal(state.pages.new.text, "n");
  assert.equal(sourceViewed(state), true);
  await h.store.selectSource("bad");
  assert.equal(h.snap().selectedSourceId, "new");
  assert.match(h.snap().sourcesError, /not markdown/);
});

test("source text pages accumulate by nextOffset", async () => {
  const text = "0123456789".repeat(5);
  const h = harness({ texts: { A: text }, chunk: 20 });
  await ready(h);
  assert.equal(h.snap().pages.A.text.length, 20);
  assert.equal(h.snap().pages.A.nextOffset, 20);
  await h.store.readSource("A", true);
  await h.store.readSource("A", true);
  assert.equal(h.snap().pages.A.text, text);
  assert.equal(h.snap().pages.A.nextOffset, null);
  const reads = h.calls.read.length;
  await h.store.readSource("A", true);
  assert.equal(h.calls.read.length, reads);
  assert.deepEqual(h.calls.read.map(call => call[1]), [0, 20, 40]);
});

test("missing Architect gives the explicit message and never prompts", async () => {
  const h = harness({ architect: null });
  await h.store.refreshSources();
  assert.equal(h.snap().sourcesError, NEED_ARCHITECT);
  await h.store.useLastSubmitted();
  assert.equal(h.snap().sourcesError, NEED_ARCHITECT);
  assert.equal(await h.store.discuss("r"), false);
  assert.equal(h.snap().discussError, NEED_ARCHITECT);
  assert.equal(generateBlock(h.snap(), "structureProposed", null), NEED_ARCHITECT);
  assert.equal(h.calls.list, 0);
  assert.equal(h.calls.generate.length, 0);
});

test("backend errors for sources are surfaced and keep the state", async () => {
  const h = await ready(harness());
  h.api.listStructureSources = async () => { throw new Error("Architect generation changed"); };
  await h.store.refreshSources();
  assert.match(h.snap().sourcesError, /generation changed/);
  assert.equal(h.snap().sources.length, 1);
  assert.equal(h.snap().sourcesLoading, false);
});

test("late source list, saved read and generation results for another folder or snapshot are dropped", async () => {
  const h = harness();
  h.gates.list = deferred();
  const refreshing = h.store.refreshSources();
  h.store.setFolder("C:/other", "snap");
  h.gates.list.resolve();
  await refreshing;
  await settle();
  assert.deepEqual(h.snap().sources, []);
  assert.equal(h.snap().folder, "C:/other");
  assert.equal(h.snap().sourcesLoading, false);

  const g = harness();
  g.gates.saved = deferred();
  g.saved.structureCurrent = status("stale", "structureCurrent");
  const loading = g.store.loadSaved();
  g.store.setFolder("C:/other", "snap");
  g.gates.saved.resolve();
  await loading;
  await settle();
  assert.deepEqual(g.snap().revisions.structureCurrent, []);

  const s = await ready(harness());
  s.gates.generate = deferred();
  const generating = s.store.generateCurrent();
  assert.equal(s.snap().busy.structureCurrent, true);
  s.store.setFolder(FOLDER, "snap-2");
  assert.equal(s.snap().busy.structureCurrent, false);
  s.gates.generate.resolve();
  await generating;
  assert.deepEqual(s.snap().revisions.structureCurrent, []);
  assert.equal(s.snap().errors.structureCurrent, null);
  assert.equal(s.snap().busy.structureCurrent, false);
});

test("a status for another folder is ignored", async () => {
  const h = harness();
  h.store.receive(status("x", "structureCurrent", { folder: "C:/elsewhere" }));
  h.store.receive({ ...status("y", "structureCurrent"), kind: "systemOverview" });
  assert.deepEqual(h.snap().revisions.structureCurrent, []);
  h.store.receive(status("z", "structureCurrent", { folder: "c:\\P\\" }));
  assert.equal(h.snap().revisions.structureCurrent.length, 1);
});

test("a late older result never replaces the newer selected revision; supersession is respected", async () => {
  const h = harness();
  const older = status("old", "structureCurrent");
  const newer = status("new", "structureCurrent", { supersedesRequestId: "old" });
  h.store.receive(newer);
  h.store.receive(older);
  let state = h.snap();
  assert.equal(selectedRevision(state, "structureCurrent").requestId, "new");
  assert.deepEqual(state.revisions.structureCurrent.map(item => item.requestId), ["new", "old"]);
  assert.equal(supersededBy(state.revisions.structureCurrent, older), "new");
  // A late non-terminal read must not downgrade a completed status.
  h.store.receive({ ...newer, state: "submitted", result: null });
  assert.equal(selectedRevision(h.snap(), "structureCurrent").state, "completed");
  // A pending regeneration does not displace the last completed result.
  h.store.receive(status("pending", "structureCurrent", { state: "pending", result: null, resultHash: null, supersedesRequestId: "new" }));
  assert.equal(selectedRevision(h.snap(), "structureCurrent").requestId, "new");
  state = h.snap();
  assert.equal(state.revisions.structureCurrent[0].requestId, "pending");
});

test("explicit revision choice survives later merges until it is newly superseded", async () => {
  const h = harness();
  h.store.receive(status("a", "structureCurrent"));
  h.store.receive(status("b", "structureCurrent"));
  h.store.selectRevision("structureCurrent", "a");
  h.store.receive(status("c", "structureProposed"));
  assert.equal(selectedRevision(h.snap(), "structureCurrent").requestId, "a");
  h.store.receive(status("d", "structureCurrent", { supersedesRequestId: "a" }));
  assert.equal(selectedRevision(h.snap(), "structureCurrent").requestId, "d");
});

test("generateProposed needs a selected source whose text was read and passes source and base ids", async () => {
  const h = harness();
  await h.store.generateProposed();
  assert.equal(h.calls.generate.length, 0);
  assert.match(h.snap().errors.structureProposed, /Select the target Markdown/);

  const g = await ready(harness());
  g.store.receive(status("cur-old", "structureCurrent", { snapshotId: "other", createdAt: 999 }));
  g.store.receive(status("cur", "structureCurrent"));
  await g.store.generateProposed();
  assert.equal(g.calls.generate.length, 1);
  assert.deepEqual(g.calls.generate[0], { kind: "structureProposed", folder: FOLDER, snapshotId: "snap", area: "structure", sourceId: "A", baseRequestId: "cur", supersedesRequestId: null });
  assert.equal(g.snap().busy.structureProposed, false);
  assert.equal(g.snap().revisions.structureProposed[0].state, "pending");
  const flags = g.snap();
  assert.equal(flags.errors.structureProposed, null);
});

test("generateProposed is blocked without snapshot or while the selected Markdown is unread", async () => {
  const h = harness();
  h.api.readStructureSource = async () => { throw new Error("cannot read"); };
  await ready(h);
  assert.equal(sourceViewed(h.snap()), false);
  assert.equal(generateBlock(h.snap(), "structureProposed", h.host.architect()), "Read the selected Markdown first.");
  await h.store.generateProposed();
  assert.equal(h.calls.generate.length, 0);
  assert.match(h.snap().pages.A.error, /cannot read/);
  h.store.setFolder(FOLDER, null);
  assert.match(generateBlock(h.snap(), "structureCurrent", h.host.architect()), /snapshot/);
  await h.store.generateCurrent();
  assert.equal(h.calls.generate.length, 0);
});

test("generateCurrent sends no source and a double click admits one request", async () => {
  const h = await ready(harness());
  h.gates.generate = deferred();
  const first = h.store.generateCurrent();
  const second = h.store.generateCurrent();
  h.gates.generate.resolve();
  await Promise.all([first, second]);
  assert.equal(h.calls.generate.length, 1);
  assert.deepEqual(h.calls.generate[0], { kind: "structureCurrent", folder: FOLDER, snapshotId: "snap", area: "structure", supersedesRequestId: null });
});

test("regenerate passes supersedesRequestId of the lineage head; a new document starts a new lineage", async () => {
  const h = await ready(harness({ sources: [row("A", 10), row("B", 20)], texts: { A: "a", B: "b" } }));
  await h.store.selectSource("A");
  h.store.receive(status("c1", "structureCurrent"));
  h.store.receive(status("c2", "structureCurrent", { supersedesRequestId: "c1" }));
  await h.store.regenerate("structureCurrent");
  assert.equal(h.calls.generate.at(-1).supersedesRequestId, "c2");
  h.store.receive(status("p1", "structureProposed", { sourceId: "A", sourceHash: "sha-A" }));
  await h.store.regenerate("structureProposed");
  assert.equal(h.calls.generate.at(-1).supersedesRequestId, "p1");
  assert.equal(h.calls.generate.at(-1).sourceId, "A");
  await h.store.selectSource("B");
  await h.store.regenerate("structureProposed");
  assert.equal(h.calls.generate.at(-1).sourceId, "B");
  assert.equal(h.calls.generate.at(-1).supersedesRequestId, null);
});

test("a preview from another document or snapshot is never approvable", async () => {
  const h = await ready(harness({ sources: [row("A", 10), row("B", 20)], texts: { A: "a", B: "b" } }));
  await h.store.selectSource("A");
  h.store.receive(status("p1", "structureProposed"));
  assert.deepEqual(approveCheck(h.snap()), { ok: true, reason: null });
  await h.store.selectSource("B");
  assert.equal(selectedRevision(h.snap(), "structureProposed").requestId, "p1");
  assert.equal(approveCheck(h.snap()).ok, false);
  assert.match(approveCheck(h.snap()).reason, /different document/);
  await h.store.approve();
  assert.equal(h.calls.approve.length, 0);
  assert.match(h.snap().approvalError, /different document/);
  await h.store.selectSource("A");
  h.store.setFolder(FOLDER, "snap-new");
  assert.match(approveCheck(h.snap()).reason, /Index changed/);
});

test("a preview superseded by a completed successor cannot be approved", async () => {
  const h = await ready(harness());
  h.store.receive(status("p1", "structureProposed"));
  h.store.receive(status("p2", "structureProposed", { supersedesRequestId: "p1" }));
  h.store.selectRevision("structureProposed", "p1");
  assert.match(approveCheck(h.snap()).reason, /newer preview/);
  h.store.selectRevision("structureProposed", "p2");
  assert.equal(approveCheck(h.snap()).ok, true);
});

test("approve submits once for a double click, stores the returned approval and calls the backend with exact hashes", async () => {
  const h = await ready(harness());
  h.store.receive(status("p1", "structureProposed"));
  h.gates.approve = deferred();
  const first = h.store.approve();
  const second = h.store.approve();
  assert.equal(h.snap().approving, true);
  h.gates.approve.resolve();
  await Promise.all([first, second]);
  assert.deepEqual(h.calls.approve, [["p1", "rh-p1", "sha-A"]]);
  const revision = selectedRevision(h.snap(), "structureProposed");
  assert.equal(revision.approval.delivery, "sent");
  assert.equal(h.snap().approving, false);
  assert.equal(h.snap().approvalError, null);
  await h.store.approve();
  assert.equal(h.calls.approve.length, 1);
  assert.match(h.snap().approvalError, /already approved/);
});

test("approve failure keeps the state and shows the error; uncertain delivery is kept for the human", async () => {
  const h = await ready(harness());
  h.store.receive(status("p1", "structureProposed"));
  h.api.approveStructurePreview = async () => { throw new Error("result hash changed"); };
  await h.store.approve();
  assert.match(h.snap().approvalError, /result hash changed/);
  assert.equal(selectedRevision(h.snap(), "structureProposed").approval, null);
  assert.equal(h.snap().approving, false);
  h.api.approveStructurePreview = async (requestId, resultHash, sourceHash) => ({ ...status(requestId, "structureProposed"), approval: { approvedAt: 9, resultHash, sourceHash, hasConflicts: true, delivery: "uncertain", deliveryError: "no ack" } });
  await h.store.approve();
  assert.equal(selectedRevision(h.snap(), "structureProposed").approval.delivery, "uncertain");
  assert.equal(h.snap().approvalError, null);
  // A late read without approval/with an earlier delivery state cannot erase or regress the approval.
  h.store.receive(status("p1", "structureProposed"));
  assert.equal(selectedRevision(h.snap(), "structureProposed").approval.delivery, "uncertain");
});

test("approval is not stored when the backend returns no approval record", async () => {
  const h = await ready(harness());
  h.store.receive(status("p1", "structureProposed"));
  h.api.approveStructurePreview = async requestId => status(requestId, "structureProposed");
  await h.store.approve();
  assert.match(h.snap().approvalError, /did not return an approval/);
});

test("discuss replays the saved result into the existing Architect without approving", async () => {
  const h = harness();
  assert.equal(await h.store.discuss("p1"), true);
  assert.deepEqual(h.calls.open, [["arch", "p1"]]);
  assert.equal(h.calls.approve.length, 0);
  h.host.openArchitect = async () => false;
  assert.equal(await h.store.discuss("p1"), false);
  assert.match(h.snap().discussError, /could not be opened/);
});

test("use last submitted selects and reads the returned source", async () => {
  const h = harness({ sources: [row("L", 50)], texts: { L: "last" } });
  await h.store.useLastSubmitted();
  const state = h.snap();
  assert.equal(state.selectedSourceId, "L");
  assert.equal(state.pages.L.text, "last");
  assert.equal(state.selectingLast, false);
  assert.equal(sourceViewed(state), true);
  assert.equal(h.calls.generate.length, 0);
});

test("a failed generate request surfaces its error and uncertain delivery is never resent", async () => {
  const h = await ready(harness());
  h.host.generate = async () => ({ admitted: false, status: null, error: "Architect is busy", uncertain: false });
  await h.store.generateCurrent();
  assert.equal(h.snap().errors.structureCurrent, "Architect is busy");
  assert.equal(h.snap().busy.structureCurrent, false);
  let sends = 0;
  h.host.generate = async () => { sends++; return { admitted: true, status: null, error: null, uncertain: true }; };
  await h.store.generateCurrent();
  assert.equal(sends, 1);
  assert.equal(h.snap().uncertain.structureCurrent, true);
  assert.match(h.snap().errors.structureCurrent, /not resent automatically/);
  h.host.generate = async () => { throw new Error("boom"); };
  await h.store.generateCurrent();
  assert.equal(h.snap().errors.structureCurrent, "boom");
});

test("folder switch resets sources, revisions and selection", async () => {
  const h = await ready(harness());
  h.store.receive(status("p1", "structureProposed"));
  h.store.setFolder("C:/other", "x");
  const state = h.snap();
  assert.deepEqual(state.sources, []);
  assert.deepEqual(state.revisions.structureProposed, []);
  assert.equal(state.selectedSourceId, null);
  assert.equal(state.folder, "C:/other");
});
