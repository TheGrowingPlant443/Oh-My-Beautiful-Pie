import test from "node:test";
import assert from "node:assert/strict";
import { defaultQualityConfig, modeSelectors, selectorMode, QualityClient, qualityLocked, QUALITY_BUSY, QualityGeneration, QualityViewEntry, QualityRevealLifetime, setupAction, filterQualityFindings, findingTools, mergeFindingAttribution, qualityPage, scanPresentation, parseQualityPreferences, pageScreen, scanAction, installVisible, featuresVisible, countBySeverity, STARTING_TEXT } from "./code-quality.ts";

const flush = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
function deferred() { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; }
function state(extra = {}) {
  return { revision: 0, config: defaultQualityConfig(), effectiveMode: null, phase: "idle", busy: false, operationId: null, requestedConfig: null, error: null,
    activated: false, community: { running: false, url: null, runtimeId: null }, credentials: { community: false, enterprise: false }, rules: { manifestHash: "pack", licenseHash: "license", acknowledged: false, licenseText: "terms" }, prerequisites: [], options: { wslDistros: [], podmanConnections: [], wingetAvailable: false, podmanAppConnection: null, wslFeaturesMissing: false }, diagnostics: [], unresolved: [], activeScanId: null,
    ownership: { status: "owned", ownerPid: 42 }, suspended: false, setup: { needed: false, installRequired: false, resume: false, steps: [] }, startAtLaunch: false, ...extra };
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

const ready = (extra = {}) => state({ activated: true, effectiveMode: "sonarqubeCommunity", ...extra });
const step = (id, value) => ({ id, label: id, state: value });

test("page screen follows the evaluation order from loading to scan", () => {
  assert.equal(pageScreen(null, true), "loading");
  assert.equal(pageScreen(ready(), false), "loading");
  assert.equal(pageScreen(ready({ busy: true, phase: "waitingForReady" }), false), "starting", "progress shows while the open-view start is still running");
  assert.equal(pageScreen(state(), true), "setup", "fresh install shows the setup button");
  assert.equal(pageScreen(ready({ busy: true, phase: "installing" }), true), "setup");
  assert.equal(pageScreen(ready({ busy: true, phase: "waitingForReady" }), true), "starting");
  assert.equal(pageScreen(ready({ busy: true, phase: "scanning" }), true), "scan");
  assert.equal(pageScreen(ready({ busy: true, phase: "cancelling" }), true), "scan");
  assert.equal(pageScreen(ready({ setup: { needed: true, installRequired: true, resume: false, steps: [step("runtime", "pending")] } }), true), "setup");
  assert.equal(pageScreen(ready({ setup: { needed: true, installRequired: true, resume: true, steps: [step("windowsFeatures", "blocked")] } }), true), "setup");
  assert.equal(pageScreen(ready({ phase: "needsRestart" }), true), "setup");
  assert.equal(pageScreen(ready({ phase: "needsSetup" }), true), "setup");
  assert.equal(pageScreen(ready({ setup: { needed: true, installRequired: false, resume: false, steps: [step("token", "pending"), step("server", "pending")] } }), true), "setup", "missing token/server offers Start and scan, not installation");
  assert.equal(pageScreen(ready({ phase: "error", error: "boom", effectiveMode: null }), true), "failed");
  assert.equal(pageScreen(ready(), true), "scan");
  assert.equal(pageScreen(ready({ effectiveMode: null }), true), "setup", "idle without an effective analyzer offers explicit start");
});

test("scan action blocks until the recipe is ready and labels approval", () => {
  const base = { quality: state(), preparationReady: true, selectionNeeded: false, entryNeeded: false, requiresConsent: false, activeScanId: null, locked: false };
  assert.deepEqual(scanAction(base), { label: "Scan project", kind: "scan", reason: null });
  assert.deepEqual(scanAction({ ...base, requiresConsent: true }), { label: "Approve recipe and scan", kind: "scan", reason: null });
  assert.equal(scanAction({ ...base, preparationReady: false }).reason, "Preparing…");
  assert.equal(scanAction({ ...base, preparationReady: false }).kind, "blocked");
  assert.equal(scanAction({ ...base, selectionNeeded: true }).kind, "blocked");
  assert.match(scanAction({ ...base, entryNeeded: true }).reason, /build family/);
  assert.deepEqual(scanAction({ ...base, activeScanId: "scan-1" }), { label: "Cancel scan", kind: "cancel", reason: null });
  assert.equal(scanAction({ ...base, locked: true }).kind, "blocked");
  assert.equal(scanAction({ ...base, quality: null }).kind, "blocked");
});

test("install and Windows feature controls appear only after a probe proves them necessary", () => {
  const podman = { kind: "podman", connection: "" };
  const entry = (runtime, installed) => ({ runtime, installed, ready: installed, needsRestart: false, message: null });
  assert.equal(installVisible(state()), false, "not probed");
  assert.equal(installVisible(state({ prerequisites: [entry(podman, false)] })), true);
  assert.equal(installVisible(state({ prerequisites: [entry(podman, true)] })), false);
  assert.equal(installVisible(state({ prerequisites: [entry({ kind: "dockerDesktop" }, false)] })), false, "another runtime's entry never counts");
  const missing = { wslDistros: [], podmanConnections: [], wingetAvailable: true, podmanAppConnection: null, wslFeaturesMissing: true };
  assert.equal(featuresVisible(state({ options: missing })), false, "not probed");
  assert.equal(featuresVisible(state({ options: missing, prerequisites: [entry(podman, false)] })), true);
  assert.equal(featuresVisible(state({ options: { ...missing, wslFeaturesMissing: false }, prerequisites: [entry(podman, false)] })), false);
  const desktop = { ...defaultQualityConfig(), runtime: { kind: "dockerDesktop" } };
  assert.equal(featuresVisible(state({ config: desktop, options: missing, prerequisites: [entry({ kind: "dockerDesktop" }, false)] })), false);
});

test("severity counts and starting phase text", () => {
  assert.deepEqual(countBySeverity([]), { critical: 0, high: 0, medium: 0, low: 0, info: 0 });
  const counts = countBySeverity([finding("1", ["a"], { severity: "critical" }), finding("2", ["a"]), finding("3", ["b"]), finding("4", ["b"], { severity: "info" })]);
  assert.deepEqual(counts, { critical: 1, high: 2, medium: 0, low: 0, info: 1 });
  assert.equal(STARTING_TEXT.waitingForReady, "Waiting for SonarQube to finish starting (1–3 minutes)");
  assert.equal(STARTING_TEXT.idle, undefined);
});

test("setup action distinguishes installed prerequisites from analyzer/token work and unknown probes", () => {
  assert.equal(setupAction(null).ready, false);
  assert.equal(setupAction(state()).ready, false);
  const installed = ready({ prerequisites: [{ runtime: defaultQualityConfig().runtime, installed: true, ready: false }],
    setup: { needed: true, installRequired: false, resume: false, steps: ["windowsFeatures", "runtime", "machine"].map(id => step(id, "done")).concat([step("server", "pending"), step("token", "pending")]) } });
  assert.deepEqual(setupAction(installed), { ready: true, install: false, label: "Start and scan" });
  assert.deepEqual(setupAction({ ...installed, suspended: true }), setupAction(installed));
  for (const id of ["windowsFeatures", "runtime", "machine"]) {
    for (const status of ["pending", "blocked"]) {
      const setup = { ...installed.setup, installRequired: true, steps: installed.setup.steps.map(item => item.id === id ? step(id, status) : item) };
      assert.equal(setupAction({ ...installed, setup }).label, "Install and setup");
      assert.equal(setupAction({ ...installed, setup: { ...setup, resume: true } }).label, "Continue setup");
    }
  }
  assert.equal(pageScreen({ ...installed, suspended: true }, true), "scan", "stopped reports stay accessible");
});

test("ownership blocks every ordinary mutation while one explicit takeover is serialized", async () => {
  let current = state({ ownership: { status: "otherProcess", ownerPid: 77 } });
  const h = harness(async () => current); const stop = h.client.subscribe(() => {}); await flush();
  assert.equal(pageScreen(current, true), "ownership"); assert.equal(qualityLocked(h.client.snapshot), true);
  await assert.rejects(h.client.mutate(async () => assert.fail("must not admit")), /Code quality is busy/);
  await assert.rejects(h.client.mutate(async () => assert.fail("cancel must not bypass ownership"), true), /Code quality is busy/);
  const transfer = deferred(); let requests = 0;
  const first = h.client.mutate(async () => { requests++; await transfer.promise; current = state(); }, false, true);
  const rest = await Promise.allSettled(Array.from({ length: 20 }, () => h.client.mutate(async () => { requests++; }, false, true)));
  assert.ok(rest.every(value => value.status === "rejected")); assert.equal(requests, 1);
  transfer.resolve(); await first; assert.equal(qualityLocked(h.client.snapshot), false); stop();
});

test("handoff and suspension survive read-only polling without any engine admission", async () => {
  let mutations = 0;
  const current = state({ suspended: true, ownership: { status: "handedOff", ownerPid: null } });
  const h = harness(async () => current); const stop = h.client.subscribe(() => {}); await flush();
  await h.tick(); await h.tick();
  assert.equal(pageScreen(h.client.snapshot.state, true), "ownership");
  await assert.rejects(h.client.mutate(async () => { mutations++; }), /Code quality is busy/);
  assert.equal(mutations, 0); stop();
});

test("reveal lifetime clears fake secrets and rejects late responses after close, blur or unmount", () => {
  let secret = null, masks = false, next = 0; const timers = new Map();
  const lifetime = new QualityRevealLifetime(() => { secret = null; masks = false; }, (fn, ms) => { assert.equal(ms, 60_000); timers.set(++next, fn); return next; }, id => timers.delete(id));
  for (const dismissal of ["close", "escape", "backdrop", "blur", "navigation", "unmount"]) {
    const generation = lifetime.begin(); secret = { token: "fake-token", password: "fake-password" }; masks = true;
    lifetime.opened(generation); lifetime.close();
    assert.equal(secret, null, dismissal); assert.equal(masks, false); assert.equal(timers.size, 0);
    assert.equal(lifetime.accepts(generation), false, "late fetch cannot resurrect dismissed details");
  }
  const generation = lifetime.begin(); secret = { token: "fake-token" }; lifetime.opened(generation);
  const expire = [...timers.values()][0]; expire();
  assert.equal(secret, null); assert.equal(lifetime.accepts(generation), false); assert.equal(timers.size, 0);
});

test("reveal event bindings clear on actual blur/pagehide and detach on unmount", () => {
  const target = new EventTarget(); let secret = null, clears = 0;
  const lifetime = new QualityRevealLifetime(() => { secret = null; clears++; }, () => 1, () => {});
  const detach = lifetime.attach(target);
  for (const event of ["blur", "pagehide"]) {
    const generation = lifetime.begin(); secret = { token: "fake-token" }; lifetime.opened(generation);
    target.dispatchEvent(new Event(event));
    assert.equal(secret, null); assert.equal(lifetime.accepts(generation), false);
  }
  secret = { token: "fake-token" }; detach(); assert.equal(secret, null);
  const afterDetach = clears; target.dispatchEvent(new Event("blur")); assert.equal(clears, afterDetach);
});

test("mounted view admits initial entry and recovered ownership exactly once without restarting suspended work", () => {
  const entry = new QualityViewEntry();
  const other = status => state({ ownership: { status, ownerPid: null } });
  assert.equal(entry.shouldEnter(null, true), false);
  assert.equal(entry.shouldEnter(other("otherProcess"), true), false);
  assert.equal(entry.shouldEnter(state(), true), false, "wait for the authoritative mutation gate");
  assert.equal(entry.shouldEnter(state(), false), true, "owner exits and this window acquires");
  for (let poll = 0; poll < 20; poll++) assert.equal(entry.shouldEnter(state(), false), false, "polling never restarts");
  assert.equal(entry.shouldEnter(other("transferring"), true), false);
  assert.equal(entry.shouldEnter(state(), false), false, "failed outgoing transfer does not restart");
  assert.equal(entry.shouldEnter(other("handedOff"), true), false);
  assert.equal(entry.shouldEnter(other("handedOff"), true), false);
  assert.equal(entry.shouldEnter(state(), false), true, "explicit reacquisition gets one entry");
  assert.equal(entry.shouldEnter(state({ suspended: true }), false), false);
  assert.equal(entry.shouldEnter(state(), false), false, "explicit resume already starts itself");
  const suspendedMount = new QualityViewEntry();
  assert.equal(suspendedMount.shouldEnter(state({ suspended: true }), false), false);
  assert.equal(suspendedMount.shouldEnter(state(), false), false);
  assert.equal(new QualityViewEntry().shouldEnter(state(), false), true, "fresh mounted owner enters once");
});
