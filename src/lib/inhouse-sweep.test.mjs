import test from "node:test";
import assert from "node:assert/strict";
import { InhouseSweep } from "./inhouse-sweep.ts";
const flush = async () => { for (let i = 0; i < 8; i++) await Promise.resolve(); };
function harness(extra = {}) {
  let now = 0, judged = 0, total = 1500, starts = 0, snapshot;
  const timers = new Map(), admitted = [];
  let next = 0;
  const deps = {
    start: async () => { starts++; return { architect: null, promptSent: true, requested: Math.min(500, total - judged), remaining: Math.max(0, total - judged - 500), error: null }; },
    status: async () => ({ gate: true, complete: judged === total, total, judged, inhouse: {}, updatedAt: now }),
    now: () => now, setTimer: fn => { timers.set(++next, fn); return next; }, clearTimer: id => timers.delete(id),
    changed: value => { snapshot = value; }, admitted: value => admitted.push(value), ...extra,
  };
  const sweep = new InhouseSweep(deps);
  return { sweep, timers, admitted, get snapshot() { return snapshot; }, get starts() { return starts; }, set judged(value) { judged = value; }, set total(value) { total = value; },
    async tick(ms = 2000) { now += ms; const current = [...timers.values()]; timers.clear(); for (const fn of current) fn(); await flush(); } };
}
test("three chunks run sequentially, progress gates the next prompt and completion stops polling", async () => {
  const h = harness(); h.sweep.run("folder"); h.sweep.run("ignored"); await flush();
  assert.equal(h.starts, 1); await h.tick(); assert.equal(h.starts, 1);
  h.judged = 499; await h.tick(); assert.equal(h.starts, 1);
  h.judged = 500; await h.tick(); assert.equal(h.starts, 2);
  h.judged = 1000; await h.tick(); assert.equal(h.starts, 3);
  h.judged = 1500; await h.tick(); assert.equal(h.snapshot.running, false); assert.equal(h.timers.size, 0);
  assert.equal(h.admitted.length, 3); assert.equal(h.admitted[2].remaining, 0);
});
test("progress resets idle timeout, then an idle sweep stops with an actionable error", async () => {
  const h = harness(); h.sweep.run("folder"); await flush();
  h.judged = 1; await h.tick(600001); assert.equal(h.snapshot.running, true);
  await h.tick(600001); assert.match(h.snapshot.error, /not finished judging/); assert.equal(h.timers.size, 0);
});
test("definite failure stops without admission; uncertain admission warns and never duplicates outstanding chunk", async () => {
  const failed = harness({ start: async () => ({ promptSent: false, requested: 500, remaining: 1000, error: "failed" }) });
  failed.sweep.run("folder"); await flush(); assert.equal(failed.snapshot.error, "failed"); assert.equal(failed.admitted.length, 0);
  let starts = 0;
  const uncertain = harness({ start: async () => { starts++; return { promptSent: null, requested: 500, remaining: 1000, error: "uncertain" }; } });
  uncertain.sweep.run("folder"); await flush(); await uncertain.tick(); await uncertain.tick();
  assert.equal(starts, 1); assert.equal(uncertain.snapshot.warning, "uncertain"); assert.equal(uncertain.admitted.length, 1);
  uncertain.sweep.stop(); assert.equal(uncertain.timers.size, 0);
});
test("already complete, empty admission and IPC failures terminate safely", async () => {
  const complete = harness(); complete.total = 0; complete.sweep.run("folder"); await flush(); assert.equal(complete.starts, 0); assert.equal(complete.snapshot.running, false);
  for (const extra of [{ start: async () => ({ promptSent: true, requested: 0, remaining: 0, error: null }) }, { status: async () => { throw new Error("offline"); } }, { start: async () => { throw new Error("offline"); } }]) {
    const h = harness(extra); h.sweep.run("folder"); await flush(); assert.equal(h.snapshot.running, false); assert.ok(h.snapshot.error);
  }
});
test("stop ignores late responses and never aborts the Architect", async () => {
  let resolve;
  const h = harness({ status: () => new Promise(done => { resolve = done; }) });
  h.sweep.run("folder"); h.sweep.stop(); resolve({ complete: false, judged: 0, total: 500 }); await flush();
  assert.equal(h.starts, 0); assert.equal(h.timers.size, 0);
});
