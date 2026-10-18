import test from "node:test";
import assert from "node:assert/strict";
import { DependencyJobSession } from "./package-insights.ts";
import { citationText, discoveryEcosystems, insightTransport, toAdmission, toJobState } from "./project-insight.ts";

const flush = async () => { for (let index = 0; index < 8; index++) await Promise.resolve(); };
class Clock {
  time = 0;
  nextId = 0;
  timers = new Map();
  setTimeout = (callback, milliseconds) => { const id = ++this.nextId; this.timers.set(id, { callback, at: this.time + milliseconds }); return id; };
  clearTimeout = id => { this.timers.delete(id); };
  async advance(milliseconds) {
    const until = this.time + milliseconds;
    while (true) {
      const next = [...this.timers].filter(([, timer]) => timer.at <= until).sort((a, b) => a[1].at - b[1].at)[0];
      if (!next) break;
      this.time = next[1].at;
      this.timers.delete(next[0]);
      next[1].callback();
      await flush();
    }
    this.time = until;
  }
}
const status = (state, extra = {}) => ({ requestId: "request-1", folder: "C:/p", kind: "dependencyDiscovery", state, message: null, result: null, ...extra });
const fakeApi = statuses => {
  const calls = { requests: [], reads: [] };
  let index = 0;
  return {
    calls,
    requestProjectInsight: async (folder, kind, rerun) => { calls.requests.push([folder, kind, rerun]); return statuses[0]; },
    getProjectInsight: async (folder, kind) => { calls.reads.push([folder, kind]); return statuses[Math.min(++index, statuses.length - 1)]; },
  };
};

test("states map onto the generic job statuses and admissions", () => {
  assert.deepEqual(["pending", "submitted", "uncertain", "completed", "failed"].map(state => toJobState(status(state)).status), ["pending", "pending", "pending", "complete", "error"]);
  assert.equal(toJobState(status("failed", { message: "ended" })).error, "ended");
  assert.equal(toJobState(status("uncertain", { message: "unknown outcome" })).error, null);
  assert.deepEqual(toAdmission(status("submitted")), { requestId: "request-1", architectId: null, promptSent: true, error: null, disposition: "pending" });
  assert.equal(toAdmission(status("uncertain")).promptSent, null);
  assert.equal(toAdmission(status("completed")).disposition, "complete");
  assert.deepEqual([toAdmission(status("failed", { message: "no" })).disposition, toAdmission(status("failed", { message: "no" })).error], ["error", "no"]);
});

test("a click admits once, polls the saved request by folder and kind, and stops at the result", async () => {
  const clock = new Clock();
  const api = fakeApi([status("submitted"), status("submitted"), status("completed", { result: { kind: "dependencyDiscovery", items: [], searchedAreas: ["x"], limitations: ["y"] } })]);
  const session = new DependencyJobSession(insightTransport(api, "dependencyDiscovery"), () => {}, clock);
  const opened = await session.open("C:/p", null, false);
  assert.deepEqual(api.calls.requests, [["C:/p", "dependencyDiscovery", false]]);
  assert.equal(opened.disposition, "pending");
  assert.equal(session.snapshot.pending, true);
  await clock.advance(1000);
  assert.equal(session.snapshot.state.status, "complete");
  assert.equal(session.snapshot.state.result.kind, "dependencyDiscovery");
  assert.deepEqual(api.calls.reads, [["C:/p", "dependencyDiscovery"], ["C:/p", "dependencyDiscovery"]]);
  assert.equal(api.calls.requests.length, 1);
  assert.equal(clock.timers.size, 0);
});

test("an uncertain admission keeps waiting for a late result and is never resent by polling", async () => {
  const clock = new Clock();
  const api = fakeApi([status("uncertain", { message: "Prompt admission outcome unknown." }), status("uncertain", { message: "Prompt admission outcome unknown." }), status("completed")]);
  const session = new DependencyJobSession(insightTransport(api, "dependencyDiscovery"), () => {}, clock);
  const opened = await session.open("C:/p", null);
  assert.equal(opened.disposition, "pending");
  assert.equal(session.snapshot.admission.promptSent, null);
  assert.equal(session.snapshot.state.state, "uncertain");
  assert.equal(session.snapshot.pending, true);
  await clock.advance(1000);
  assert.equal(session.snapshot.state.status, "complete");
  assert.equal(api.calls.requests.length, 1);
});

test("a rerun is forwarded, a failure keeps its message, and a replaced request is refused", async () => {
  const clock = new Clock();
  const failed = fakeApi([status("failed", { message: "The Architect session was stopped." }), status("failed", { message: "The Architect session was stopped." })]);
  const session = new DependencyJobSession(insightTransport(failed, "requirements"), () => {}, clock);
  await session.open("C:/p", null, true);
  assert.deepEqual(failed.calls.requests, [["C:/p", "requirements", true]]);
  assert.equal(session.snapshot.state.status, "error");
  assert.equal(session.snapshot.error, "The Architect session was stopped.");
  assert.equal(session.snapshot.pending, false);
  const replaced = fakeApi([status("submitted"), status("submitted", { requestId: "request-2" })]);
  const other = new DependencyJobSession(insightTransport(replaced, "dependencyDiscovery"), () => {}, clock);
  await other.open("C:/p", null);
  assert.match(other.snapshot.error, /did not match|changed/);
  assert.equal(other.snapshot.pending, false);
  const missing = fakeApi([status("submitted"), null]);
  const vanished = new DependencyJobSession(insightTransport(missing, "dependencyDiscovery"), () => {}, clock);
  await vanished.open("C:/p", null);
  assert.match(vanished.snapshot.error, /changed/);
  assert.equal(clock.timers.size, 0);
});

test("discovered ecosystems are distinct, known and stable; citations show the line when there is one", () => {
  const item = (ecosystem, name = "x") => ({ name, ecosystem, declaredVersion: null, resolvedVersion: null, relationship: null, citations: [], notes: "" });
  assert.deepEqual(discoveryEcosystems([item("pypi"), item(null), item("npm"), item("pypi", "y")]), ["npm", "pypi"]);
  assert.deepEqual(discoveryEcosystems([]), []);
  assert.equal(citationText({ path: "app.py", line: 3 }), "app.py:3");
  assert.equal(citationText({ path: "app.py", line: null }), "app.py");
});
