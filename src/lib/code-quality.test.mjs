import test from "node:test";
import assert from "node:assert/strict";
import { defaultQualityConfig, modeSelectors, selectorMode, QualityClient, qualityLocked, QUALITY_BUSY, QualityGeneration, filterQualityFindings, findingTools, mergeFindingAttribution, qualityPage, scanPresentation, parseQualityPreferences } from "./code-quality.ts";

const flush = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
function deferred() { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; }
function state(extra = {}) {
  return { revision: 0, config: defaultQualityConfig(), effectiveMode: null, phase: "idle", busy: false, operationId: null, requestedConfig: null, error: null,
    activated: false, community: { running: false, url: null, runtimeId: null }, credentials: { community: false, enterprise: false }, rules: { manifestHash: "pack", licenseHash: "license", acknowledged: false, licenseText: "terms" }, prerequisites: [], options: { wslDistros: [], podmanConnections: [], wingetAvailable: false, podmanAppConnection: null }, diagnostics: [], unresolved: [], activeScanId: null, ...extra };
}
function harness(read = async () => state()) {
  let next = 0; const timers = new Map();
  const client = new QualityClient({ read, setTimer: (fn, ms) => { assert.equal(ms, 1000); timers.set(++next, fn); return next; }, clearTimer: handle => timers.delete(handle) });
  return { client, timers, async tick() { const current = [...timers.values()]; timers.clear(); current.forEach(fn => fn()); await flush(); } };
}
const finding = (id, tools, extra = {}) => ({ id, tools, ruleId: "omp-quality.eval", category: "security", severity: "high", message: "Review dynamic evaluation", path: "src/a.ts", line: 2, column: 1, endLine: 2, helpUrl: null, hotspot: false, ...extra });

test("defaults and exclusive analyzer/connection selectors round-trip", () => {
  assert.deepEqual(defaultQualityConfig(), { version: 1, mode: "sonarqubeCommunity", runtime: { kind: "podman", connection: "" }, communityPort: 9000, enterpriseUrl: "" });
  for (const mode of ["sonarqubeCommunity", "sonarqubeEnterprise", "alternate"]) { const selectors = modeSelectors(mode); assert.equal(selectorMode(selectors.analyzer, selectors.connection), mode); }
  assert.equal(selectorMode("alternate", "enterprise"), "alternate");
  const one = defaultQualityConfig(); one.runtime.connection = "changed"; assert.equal(defaultQualityConfig().runtime.connection, "");
});

test("two consumers share immediate mutation locking and no queued second action", async () => {
  const h = harness(); const seenA = [], seenB = [];
  const stopA = h.client.subscribe(value => seenA.push(qualityLocked(value))), stopB = h.client.subscribe(value => seenB.push(qualityLocked(value)));
  await flush(); assert.equal(h.timers.size, 1); assert.equal(qualityLocked(h.client.snapshot), false);
  const work = deferred(); let starts = 0;
  const first = h.client.mutate(async () => { starts++; await work.promise; });
  assert.equal(seenA.at(-1), true); assert.equal(seenB.at(-1), true);
  const rejected = await Promise.allSettled(Array.from({ length: 20 }, () => h.client.mutate(async () => { starts++; })));
  assert.ok(rejected.every(value => value.status === "rejected" && value.reason.message === QUALITY_BUSY)); assert.equal(starts, 1);
  work.resolve(); await first; assert.equal(seenA.at(-1), false); assert.equal(seenB.at(-1), false);
  stopA(); assert.equal(h.timers.size, 1); stopB(); assert.equal(h.timers.size, 0);
});

test("lost mutation response or failed refresh does not unlock unknown backend work", async () => {
  let current = state(), failRead = false;
  const h = harness(async () => { if (failRead) throw "snapshot unavailable"; return current; });
  const stop = h.client.subscribe(() => {}); await flush();
  failRead = true;
  await assert.rejects(h.client.mutate(async () => { throw new Error("IPC disconnected"); }), /IPC disconnected/);
  assert.equal(h.client.snapshot.pending, false); assert.equal(h.client.snapshot.uncertain, true); assert.equal(qualityLocked(h.client.snapshot), true);
  failRead = false; current = state({ busy: true, phase: "starting" }); await h.tick();
  assert.equal(h.client.snapshot.uncertain, false); assert.equal(qualityLocked(h.client.snapshot), true);
  stop(); assert.equal(h.timers.size, 1, "busy service keeps polling without mounted UI");
  current = state(); await h.tick(); assert.equal(qualityLocked(h.client.snapshot), false); assert.equal(h.timers.size, 0);
});

test("late polls cannot overwrite a newer request or a completed mutation generation", async () => {
  const reads = [];
  const h = harness(() => { const request = deferred(); reads.push(request); return request.promise; });
  const stop = h.client.subscribe(() => {});
  const second = h.client.refresh(); reads[1].resolve(state({ revision: 2 })); await second;
  reads[0].resolve(state({ revision: 1 })); await flush(); assert.equal(h.client.snapshot.state.revision, 2);
  const oldPoll = h.client.refresh();
  const mutation = h.client.mutate(async () => {}); await flush();
  reads.at(-1).resolve(state({ revision: 3 })); await mutation;
  reads[2].resolve(state({ revision: 2, busy: true })); await oldPoll;
  assert.equal(h.client.snapshot.state.revision, 3); assert.equal(qualityLocked(h.client.snapshot), false);
  stop();
});

test("cancel can await active scan work but a normal selection cannot bypass busy", async () => {
  let current = state({ busy: true, phase: "scanning", activeScanId: "scan-a" });
  const h = harness(async () => current); const stop = h.client.subscribe(() => {}); await flush();
  await assert.rejects(h.client.mutate(async () => {}), /Code quality is busy/);
  const cancellation = deferred();
  const pending = h.client.mutate(async () => { await cancellation.promise; current = state(); }, true);
  assert.equal(qualityLocked(h.client.snapshot), true);
  await assert.rejects(h.client.mutate(async () => {}, true), /Code quality is busy/);
  cancellation.resolve(); await pending; assert.equal(qualityLocked(h.client.snapshot), false); stop();
});

test("dedup attribution remains filterable by either grep engine, never unrelated rules", () => {
  const merged = mergeFindingAttribution([finding("same", ["Opengrep"]), finding("same", ["Semgrep CE"]), finding("other", ["MegaLinter"], { ruleId: "other-rule", severity: "medium" })]);
  assert.equal(merged.length, 2); assert.deepEqual(merged[0].tools, ["Opengrep", "Semgrep CE"]);
  for (const tool of ["Opengrep", "Semgrep CE"]) assert.deepEqual(filterQualityFindings(merged, { tool, severity: "high", text: "SRC/A" }).map(value => value.id), ["same"]);
  assert.equal(filterQualityFindings(merged, { tool: "", severity: "medium", text: "other-rule" }).length, 1);
  assert.deepEqual(findingTools(merged), ["MegaLinter", "Opengrep", "Semgrep CE"]);
});

test("100-row pagination clamps negative, empty and past-end pages", () => {
  const values = Array.from({ length: 201 }, (_, i) => i);
  assert.deepEqual(qualityPage(values, 0).items, values.slice(0, 100));
  assert.deepEqual(qualityPage(values, 1).items, values.slice(100, 200));
  assert.deepEqual(qualityPage(values, 500), { items: [200], page: 2, pages: 3, total: 201 });
  assert.equal(qualityPage(values, -4).page, 0);
  assert.deepEqual(qualityPage([], 9), { items: [], page: 0, pages: 1, total: 0 });
});

test("partial, interrupted, cancelled and failed results never present as complete-zero", () => {
  const base = { findings: [], qualityGate: null };
  assert.match(scanPresentation({ ...base, status: "complete" }).label, /Completed · zero findings/);
  for (const status of ["partial", "interrupted", "cancelled", "error", "cancelling"]) { const result = scanPresentation({ ...base, status }); assert.notEqual(result.tone, "ok"); assert.doesNotMatch(result.label, /Completed/); }
  assert.equal(scanPresentation({ ...base, status: "complete", qualityGate: "failed" }).tone, "warn");
});

test("folder generations reject stale results after navigation, revisits and unmount", () => {
  const guard = new QualityGeneration(), first = guard.begin("C:\\Repo\\One\\");
  assert.equal(guard.accepts(first, "c:/repo/one"), true);
  const second = guard.begin("C:/Repo/Two"); assert.equal(guard.accepts(first, "C:/Repo/One"), false);
  guard.begin("C:/Repo/One"); assert.equal(guard.accepts(first, "C:/Repo/One"), false); assert.equal(guard.accepts(second, "C:/Repo/Two"), false);
  const active = guard.begin("C:/Repo/One"); guard.invalidate(); assert.equal(guard.accepts(active, "C:/Repo/One"), false);
});

test("only nonsecret folder and filter preferences are restored", () => {
  assert.deepEqual(parseQualityPreferences('{"folder":"C:/repo","tool":"Semgrep CE","token":"secret","config":{"mode":"alternate"}}'), { folder: "C:/repo", tool: "Semgrep CE", severity: "", text: "" });
  assert.deepEqual(parseQualityPreferences("invalid"), { folder: "", tool: "", severity: "", text: "" });
});
