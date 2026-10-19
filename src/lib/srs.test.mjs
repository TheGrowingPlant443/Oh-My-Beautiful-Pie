import test from "node:test";
import assert from "node:assert/strict";
import { get } from "svelte/store";
import { readFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { importTs } from "./test-loader.mjs";
const { createSrsStore, completedSrs, srsDocumentLabel, srsModelLabel, srsStateLabel } = await importTs("srs");

const job = (revision = 1, extra = {}) => ({
  jobId: "job-1", folder: "C:/project", repositoryRoot: "C:/project", revision, state: "running", phase: "survey",
  vaultId: "vault-1", vaultMode: "markdown", vaultAvailable: true, outputPath: "SRS/job-1", masterPath: "SRS/job-1/SRS.md",
  template: { revision: "3965e951fa5ea1c1dd1bbc7a66b7f35f7e85c006" }, scopeHash: "scope-1", sourceHash: "source-1",
  attemptId: "attempt-1", stageId: "stage-1", stage: null,
  producer: { instanceId: "architect", generation: 3, sessionId: "same-native-conversation" },
  counts: { areas: 3, evidenceFiles: 100, work: 10, workOpen: 10, documents: 0, requirements: 0, questions: 0, questionsOpen: 0, blocked: 0, excluded: 0 },
  checkpoint: null, error: null, usage: { tokens: null, costUsd: null }, createdAt: 1, updatedAt: 1, completion: null,
  resumeRequired: "none", humanExclusions: 0, architectModel: null, ...extra,
});
const complete = (revision = 2, extra = {}) => job(revision, { state: "completed", completion: { manifestHash: "manifest", scopeHash: "scope-1", at: 100, label: "SRS complete for the recorded evidence scope" }, ...extra });
function fakeClock() {
  let n = 0;
  const timers = new Map();
  return { timers, setTimeout(fn, ms) { const id = ++n; timers.set(id, { fn, ms }); return id; }, clearTimeout(id) { timers.delete(id); }, async tick() { const items = [...timers]; timers.clear(); for (const [, item] of items) item.fn(); for (let i = 0; i < 20; i++) await Promise.resolve(); } };
}
function fixture(overrides = {}) {
  const calls = [];
  const api = { get: async id => { calls.push(["get", id]); return job(); }, list: async (folder, offset, limit) => { calls.push(["list", folder, offset, limit]); return { items: [job()], total: 1, offset, nextOffset: null }; }, start: async token => { calls.push(["start", token]); return job(); }, resume: async token => { calls.push(["resume", token]); return job(3, { attemptId: "attempt-2" }); }, pause: async id => job(2, { state: "paused" }), cancel: async id => job(3, { state: "cancelled" }), dispose: async (...args) => { calls.push(["dispose", ...args]); return job(2); }, rebindVault: async () => job(2), ...overrides };
  const clock = fakeClock(); const store = createSrsStore(api, clock);
  let receive;
  const transport = { async listen(fn) { calls.push(["listen"]); receive = fn; return () => calls.push(["unlisten"]); } };
  return { store, api, calls, clock, transport, event: value => receive(value) };
}
function attach(store, extra = {}) {
  const returned = [], opened = [];
  store.attach({ architectId: () => "architect", openArchitect: async id => { opened.push(id); return true; }, following: () => true, quiescent: () => true, returnToDocumentation: ticket => returned.push(ticket), ...extra });
  return { returned, opened };
}

test("event subscription precedes initial reads; late initial snapshots cannot regress event revisions", async () => {
  let resolve;
  const f = fixture({ get: () => new Promise(done => resolve = done) });
  const disconnect = await f.store.connect(f.transport);
  const reading = f.store.read("job-1");
  f.event(job(7)); resolve(job(2));
  assert.equal((await reading).revision, 7);
  assert.equal(get(f.store).jobs.get("job-1").revision, 7);
  assert.equal(f.calls[0][0], "listen");
  f.event(job(6, { state: "cancelled" })); assert.equal(get(f.store).jobs.get("job-1").state, "running");
  disconnect(); f.store.disposeStore();
});

test("passive list/get/observation never starts or resumes a model and >500 documents are not a job limit", async () => {
  const large = job(8, { counts: { ...job().counts, documents: 1200, requirements: 900 } });
  const f = fixture({ get: async () => large, list: async () => ({ items: [large], total: 501, offset: 0, nextOffset: 100 }) });
  await f.store.connect(f.transport); await f.store.list("C:/project");
  const stop = f.store.observe("job-1"); await f.store.reconcile();
  assert.equal(get(f.store).jobs.get("job-1").counts.requirements, 900);
  assert.ok(!f.calls.some(([name]) => name === "start" || name === "resume"));
  stop(); f.store.disposeStore();
});

test("multi-day observation and connection failures retain durable state and keep reconciling, without total deadlines", async () => {
  let fail = false, reads = 0;
  const f = fixture({ get: async () => { reads++; if (fail) throw new Error("offline"); return job(1); } });
  await f.store.connect(f.transport); f.store.receive(job());
  for (let days = 0; days < 8; days++) await f.clock.tick();
  fail = true; await f.clock.tick();
  assert.equal(get(f.store).jobs.get("job-1").state, "running");
  assert.equal(get(f.store).observationErrors.get("job-1"), "offline");
  assert.ok(f.clock.timers.size, "observation continues after failure");
  fail = false; await f.clock.tick(); assert.equal(get(f.store).observationErrors.has("job-1"), false);
  assert.ok(reads > 8); f.store.disposeStore();
});

test("explicit Start/Resume focuses the admitted existing Architect, never another instance", async () => {
  const f = fixture(); const h = attach(f.store);
  await f.store.start("human-scope-token");
  await f.store.resume("human-resume-token");
  assert.deepEqual(h.opened, ["architect", "architect"]);
  assert.equal(get(f.store).tickets[0].attemptId, "attempt-2");
  assert.deepEqual(f.calls.filter(([name]) => name === "start" || name === "resume"), [["start", "human-scope-token"], ["resume", "human-resume-token"]]);
  f.store.disposeStore();
});

test("return requires exact completion manifest, original attempt/scope/producer and quiescence", async () => {
  let idle = false;
  const f = fixture({ get: async () => complete(5) }); const h = attach(f.store, { quiescent: () => idle });
  await f.store.start("token");
  f.store.receive(job(2, { state: "completed", completion: null })); assert.equal(h.returned.length, 0);
  f.store.receive(complete(3)); assert.equal(h.returned.length, 0);
  idle = true; await f.store.reconcile(); assert.equal(h.returned.length, 1);
  f.store.receive(complete(6)); assert.equal(h.returned.length, 1, "exactly one navigation");
  f.store.disposeStore();
});

test("manual navigation, hub/project/source changes or another analysis invalidate auto-return, but offer the saved result", async () => {
  for (const invalidate of [store => store.noteNavigation(), store => store.invalidateAutoReturn()]) {
    const f = fixture(); const h = attach(f.store); await f.store.start("token"); invalidate(f.store);
    f.store.receive(complete()); assert.equal(h.returned.length, 0); assert.equal(get(f.store).tickets[0].lateResult, true);
    assert.equal(f.store.openResult("job-1"), true); assert.equal(h.returned.length, 1); f.store.disposeStore();
  }
});

test("a changed producer, scope or attempt cannot steal focus on completion", async () => {
  for (const changed of [{ attemptId: "other" }, { scopeHash: "different", completion: { manifestHash: "m", scopeHash: "different", at: 4 } }, { producer: { ...job().producer, sessionId: "replacement" } }]) {
    const f = fixture(); const h = attach(f.store); await f.store.start("token"); f.store.receive(complete(3, changed));
    assert.equal(h.returned.length, 0); assert.equal(get(f.store).tickets[0].lateResult, true); f.store.disposeStore();
  }
});

test("navigation during Start or Resume keeps observing without stealing Architect focus", async () => {
  for (const action of ["start", "resume"]) {
    let resolve;
    const f = fixture({ [action]: () => new Promise(done => resolve = done) }); const h = attach(f.store);
    const starting = f.store[action]("token"); f.store.noteNavigation(); resolve(job()); await starting;
    assert.deepEqual(h.opened, [], `${action} must not steal focus after intervening navigation`);
    assert.equal(get(f.store).jobs.get("job-1").state, "running", "the admitted job remains observable");
    f.store.receive(complete()); assert.equal(h.returned.length, 0); assert.equal(get(f.store).tickets[0].lateResult, true); f.store.disposeStore();
  }
});

test("revision-fenced human disposition forwards exact answer and rationale; errors remain visible", async () => {
  const f = fixture(); const answer = { selectedOptions: ["Keep existing rule"], note: "Confirmed by product owner" };
  await f.store.dispose("job-1", 7, "question-4", "provideHumanAnswer", "Stakeholder confirmation", answer);
  assert.deepEqual(f.calls.find(([name]) => name === "dispose"), ["dispose", "job-1", 7, "question-4", "provideHumanAnswer", "Stakeholder confirmation", answer]);
  f.store.disposeStore();
});

test("labels distinguish draft, Markdown mode and native manifest completion without guessing cost/model locality", () => {
  assert.equal(completedSrs(job(2, { state: "completed" })), false);
  assert.equal(completedSrs(complete(3, { scopeHash: "changed" })), false);
  assert.equal(srsDocumentLabel(job()), "View draft documents");
  assert.equal(srsDocumentLabel(complete()), "Documents view");
  assert.equal(srsDocumentLabel(complete(3, { vaultMode: "obsidian" })), "Obsidian view");
  assert.equal(srsStateLabel(complete()), "SRS complete for the recorded evidence scope");
  assert.match(srsModelLabel(null), /locality\/cost unknown/);
  assert.match(srsModelLabel("local-sounding-name"), /not inferred/);
});

test("vendored templates retain upstream pinned blob hashes and every recorded SHA256", async () => {
  const root = new URL("../../resources/srs-template/", import.meta.url);
  const revision = await readFile(new URL("REVISION", root), "utf8");
  assert.match(revision, /revision=3965e951fa5ea1c1dd1bbc7a66b7f35f7e85c006/);
  let count = 0;
  for (const line of revision.split(/\r?\n/)) {
    const match = /^([a-f0-9]{64})\s+([a-f0-9]{40})\s+(\d+)\s+(\S+)$/.exec(line);
    if (!match) continue;
    const [, sha, blob, length, name] = match;
    const bytes = await readFile(new URL(name, root));
    assert.equal(bytes.length, Number(length), name);
    assert.equal(createHash("sha256").update(bytes).digest("hex"), sha, name);
    assert.equal(createHash("sha1").update(Buffer.from(`blob ${bytes.length}\0`)).update(bytes).digest("hex"), blob, name);
    count++;
  }
  assert.equal(count, 5);
});
