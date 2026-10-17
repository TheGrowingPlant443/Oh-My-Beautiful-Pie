import test from "node:test";
import assert from "node:assert/strict";
import { KindProbe, languageLabel, projectEcosystems, projectFilterVerdict } from "./project-filter.ts";
const flush = async () => { for (let i = 0; i < 8; i++) await Promise.resolve(); };
const defaults = { manifestFilter: false, filterLanguages: [], aiFilter: false, filterRoles: [] };
const kind = (name, updatedAt = 10) => ({ kind: name, reason: "reason", updatedAt });
test("project filter verdict table honours independent filters, manifest precedence and override", () => {
  const cases = [
    [{}, ["maven"], null, false, "ok"],
    [{ manifestFilter: true, filterLanguages: ["maven"] }, ["maven", "npm"], null, false, "manifest"],
    [{ manifestFilter: true, filterLanguages: ["maven"] }, ["npm"], null, false, "ok"],
    [{ manifestFilter: true, filterLanguages: ["maven"] }, [], null, false, "ok"],
    [{ manifestFilter: true }, ["maven"], null, false, "ok"],
    [{ aiFilter: true, filterRoles: ["backend"] }, [], null, false, "ask"],
    [{ aiFilter: true, filterRoles: ["backend"] }, [], kind(null), false, "ask"],
    [{ aiFilter: true }, [], null, false, "ok"],
    [{ aiFilter: true, filterRoles: ["backend"] }, [], kind("backend"), false, "ai"],
    [{ aiFilter: true, filterRoles: ["frontend"] }, [], kind("fullstack"), false, "ai"],
    [{ aiFilter: true, filterRoles: ["frontend"] }, [], kind("backend"), false, "ok"],
    [{ aiFilter: true, filterRoles: ["backend", "frontend"] }, [], kind("library"), false, "ok"],
    [{ aiFilter: true, filterRoles: ["backend", "frontend"] }, [], kind("other"), false, "ok"],
    [{ manifestFilter: true, filterLanguages: ["maven"], aiFilter: true, filterRoles: ["backend"] }, ["maven"], null, false, "manifest"],
    [{ manifestFilter: true, filterLanguages: ["maven"], aiFilter: true, filterRoles: ["backend"] }, ["maven"], null, true, "ok"],
  ];
  for (const [settings, ecosystems, status, overridden, expected] of cases) assert.equal(projectFilterVerdict({ ...defaults, ...settings }, new Set(ecosystems), status, overridden).kind, expected);
  assert.deepEqual(projectFilterVerdict({ ...defaults, manifestFilter: true, filterLanguages: ["npm", "maven"] }, new Set(["maven", "npm"]), null, false), { kind: "manifest", languages: ["npm", "maven"] });
  assert.deepEqual(projectEcosystems([{ id: "project", ecosystem: "project" }, { id: "npm:a", ecosystem: "npm" }, { id: "npm:b", ecosystem: "npm" }]), new Set(["npm"]));
  assert.deepEqual(["maven", "npm", "pypi", "cargo"].map(languageLabel), ["Java (Maven/Gradle)", "JavaScript / TypeScript (npm)", "Python", "Rust (Cargo)"]);
});
function harness(extra = {}) {
  let now = 0, snapshot, starts = 0, answer = kind(null, null), timerId = 0;
  const timers = new Map(), admitted = [];
  const probe = new KindProbe({ start: async () => { starts++; return { architect: null, promptSent: true, error: null }; }, status: async () => answer,
    now: () => now, setTimer: fn => { timers.set(++timerId, fn); return timerId; }, clearTimer: id => timers.delete(id), changed: value => { snapshot = value; }, admitted: value => admitted.push(value), ...extra });
  return { probe, timers, admitted, get snapshot() { return snapshot; }, get starts() { return starts; }, set answer(value) { answer = value; }, async tick(ms = 2000) { now += ms; const current = [...timers.values()]; timers.clear(); for (const fn of current) fn(); await flush(); } };
}
test("kind probe ignores stale answers and duplicate runs, then completes on a newer answer", async () => {
  const h = harness(); h.probe.run("folder", 10); h.probe.run("ignored", null); await flush();
  h.answer = kind("backend", 10); await h.tick(); assert.equal(h.snapshot.running, true);
  await h.tick(); assert.equal(h.starts, 1);
  h.answer = kind("frontend", 11); await h.tick(); assert.equal(h.snapshot.running, false); assert.equal(h.snapshot.status.kind, "frontend"); assert.equal(h.timers.size, 0); assert.equal(h.admitted.length, 1);
});
test("probe timeout and IPC errors stop polling", async () => {
  const h = harness(); h.probe.run("folder", null); await flush(); await h.tick(600001); assert.match(h.snapshot.error, /not answered/); assert.equal(h.snapshot.running, false);
  for (const extra of [{ start: async () => { throw new Error("offline"); } }, { status: async () => { throw new Error("offline"); } }]) {
    const failed = harness(extra); failed.probe.run("folder", null); await flush(); if (failed.snapshot.running) await failed.tick(); assert.equal(failed.snapshot.error, "offline");
  }
});
test("definite rejection does not admit; uncertain admission warns, polls and does not resend", async () => {
  const failed = harness({ start: async () => ({ promptSent: false, error: "failed" }) }); failed.probe.run("folder", null); await flush();
  assert.equal(failed.snapshot.error, "failed"); assert.equal(failed.admitted.length, 0);
  const uncertain = harness({ start: async () => ({ promptSent: null, error: "uncertain" }) }); uncertain.probe.run("folder", null); await flush(); await uncertain.tick();
  assert.equal(uncertain.snapshot.warning, "uncertain"); assert.equal(uncertain.admitted.length, 1); assert.equal(uncertain.snapshot.running, true);
  uncertain.probe.stop(); assert.equal(uncertain.timers.size, 0);
});
test("stop cancels timers and late admission cannot restart polling", async () => {
  let resolve;
  const h = harness({ start: () => new Promise(done => { resolve = done; }) }); h.probe.run("folder", null); h.probe.stop(); resolve({ promptSent: true, error: null }); await flush(); assert.equal(h.timers.size, 0); assert.equal(h.admitted.length, 0);
});
