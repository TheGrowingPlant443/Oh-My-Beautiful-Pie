import test from "node:test";
import assert from "node:assert/strict";
import { importTs } from "./test-loader.mjs";

const { TicketBook } = await importTs("visualization-tickets");

function harness(options = {}) {
  let now = 1000;
  let nextTimer = 1;
  const timers = new Map();
  const clock = {
    now: () => now,
    setTimeout: (fn, ms) => { const id = nextTimer++; timers.set(id, { fn, at: now + ms }); return id; },
    clearTimeout: id => { timers.delete(id); },
  };
  async function advance(ms) {
    const until = now + ms;
    for (;;) {
      const due = [...timers.entries()].filter(([, timer]) => timer.at <= until).sort((a, b) => a[1].at - b[1].at)[0];
      if (!due) break;
      timers.delete(due[0]);
      now = Math.max(now, due[1].at);
      due[1].fn();
      for (let i = 0; i < 6; i++) await Promise.resolve();
    }
    now = until;
    for (let i = 0; i < 6; i++) await Promise.resolve();
  }
  const log = { request: [], read: [], opened: [], returned: [], received: [], changes: [] };
  const status = (state, extra = {}) => ({ requestId: "req-1", folder: "C:/repo", kind: "systemOverview", state, message: null, result: null, snapshotId: "snap", producer: { instanceId: "arch", generation: 1, launchId: "l", sessionId: "s" }, createdAt: 5, ...extra });
  const env = { epoch: 0, following: true, quiescent: true, reads: [status("pending")], admit: status(options.admitState ?? "pending") };
  const hooks = {
    api: {
      requestSystemVisualization: async (...args) => { log.request.push(args); if (options.reject) throw new Error("rejected"); return env.admit; },
      readProjectInsight: async id => { log.read.push(id); const next = env.reads.length > 1 ? env.reads.shift() : env.reads[0]; return next; },
    },
    architectId: () => "arch",
    openArchitect: async id => { log.opened.push(id); return options.openOk ?? true; },
    epoch: () => env.epoch,
    following: () => env.following,
    quiescent: () => env.quiescent,
    returnToOverview: ticket => log.returned.push(ticket.key),
    received: s => log.received.push(s.state),
    changed: list => log.changes.push(list.length),
    clock,
  };
  const book = new TicketBook(hooks, folder => folder.toLowerCase());
  return { book, env, log, advance, status, request: { kind: "systemOverview", folder: "C:/repo", snapshotId: "snap", area: "system" } };
}
const done = status => status("completed", { result: { kind: "systemOverview" }, resultHash: "h" });

test("explicit generation admits once, opens the Architect once and watches by request id", async () => {
  const h = harness();
  const [first, second] = await Promise.all([h.book.generate(h.request), h.book.generate(h.request)]);
  assert.equal(h.log.request.length, 1, "a double click joins the admission in flight");
  assert.deepEqual(first, second);
  assert.deepEqual(h.log.opened, ["arch"]);
  assert.equal(h.book.list()[0].state, "waiting");
  assert.equal(h.book.list()[0].requestId, "req-1");
  await h.advance(3500);
  assert.ok(h.log.read.length >= 2 && h.log.read.every(id => id === "req-1"), "read by request id, never by latest (folder, kind)");
  assert.equal(h.log.request.length, 1, "polling never resends");
});

test("a refused admission stays in the overview with the error and no ticket", async () => {
  const h = harness({ reject: true });
  const outcome = await h.book.generate(h.request);
  assert.equal(outcome.admitted, false);
  assert.match(outcome.error, /rejected/);
  assert.equal(h.book.list().length, 0);
  assert.deepEqual(h.log.opened, []);
});

test("saved results open passively: no Architect focus and no watcher", async () => {
  const h = harness({ admitState: "completed" });
  h.env.admit = done(h.status);
  const outcome = await h.book.generate(h.request);
  assert.equal(outcome.admitted, true);
  assert.deepEqual(h.log.opened, []);
  assert.equal(h.book.list()[0].state, "completed");
  await h.advance(5000);
  assert.equal(h.log.read.length, 0);
});

test("uncertain delivery is visible waiting and is never resent", async () => {
  const h = harness({ admitState: "uncertain" });
  h.env.admit = h.status("uncertain", { message: "unknown" });
  h.env.reads = [h.status("uncertain", { message: "unknown" })];
  const outcome = await h.book.generate(h.request);
  assert.equal(outcome.uncertain, true);
  await h.advance(5000);
  assert.equal(h.book.list()[0].state, "uncertain");
  assert.equal(h.log.request.length, 1);
});

test("the matching typed result returns to the overview only while everything still matches", async () => {
  const h = harness();
  await h.book.generate(h.request);
  h.env.reads = [h.status("pending"), done(h.status)];
  await h.advance(3000);
  assert.equal(h.book.list()[0].state, "completed");
  assert.deepEqual(h.log.returned, [h.book.list()[0].key]);
  assert.equal(h.book.list()[0].lateResult, false);
});

test("manual navigation, another project or a lost Architect view make the result a late action instead of stealing the screen", async () => {
  for (const change of [env => { env.epoch++; }, env => { env.following = false; }]) {
    const h = harness();
    await h.book.generate(h.request);
    change(h.env);
    h.env.reads = [done(h.status)];
    await h.advance(3000);
    assert.deepEqual(h.log.returned, []);
    assert.equal(h.book.list()[0].lateResult, true);
    assert.equal(h.book.openResult(h.book.list()[0].key), true);
    assert.equal(h.log.returned.length, 1);
    assert.equal(h.book.list()[0].lateResult, false);
  }
  const h = harness();
  await h.book.generate(h.request);
  h.book.invalidateAutoReturn();
  h.env.reads = [done(h.status)];
  await h.advance(3000);
  assert.deepEqual(h.log.returned, []);
  assert.equal(h.book.list()[0].lateResult, true);
});

test("a busy Architect delays the return; it never happens while the turn is not quiescent, and gives up as a late action", async () => {
  const h = harness();
  await h.book.generate(h.request);
  h.env.quiescent = false;
  h.env.reads = [done(h.status)];
  await h.advance(5000);
  assert.deepEqual(h.log.returned, []);
  h.env.quiescent = true;
  await h.advance(1000);
  assert.equal(h.log.returned.length, 1);
  const g = harness();
  await g.book.generate(g.request);
  g.env.quiescent = false;
  g.env.reads = [done(g.status)];
  await g.advance(40_000);
  assert.deepEqual(g.log.returned, []);
  assert.equal(g.book.list()[0].lateResult, true);
});

test("failure and the ten minute local deadline report status without pretending completion and keep the request", async () => {
  const failing = harness();
  await failing.book.generate(failing.request);
  failing.env.reads = [failing.status("failed", { message: "bad payload" })];
  await failing.advance(3000);
  assert.equal(failing.book.list()[0].state, "failed");
  assert.match(failing.book.list()[0].message, /bad payload/);
  assert.deepEqual(failing.log.returned, []);
  const slow = harness();
  await slow.book.generate(slow.request);
  await slow.advance(10 * 60_000 + 2000);
  assert.equal(slow.book.list()[0].state, "timedOut");
  assert.equal(slow.book.list()[0].requestId, "req-1");
  slow.book.recheck(slow.book.list()[0].key);
  assert.equal(slow.book.list()[0].state, "waiting");
  assert.equal(slow.log.request.length, 1, "recheck only reads");
});

test("an explicit regeneration is a new request that names its predecessor; the old ticket is replaced, not resent", async () => {
  const h = harness();
  await h.book.generate(h.request);
  h.env.admit = h.status("pending", { requestId: "req-2" });
  await h.book.generate({ ...h.request, supersedesRequestId: "req-1" });
  assert.equal(h.log.request.length, 2);
  assert.equal(h.log.request[1][5], "req-1");
  assert.equal(h.book.list().length, 1);
  assert.equal(h.book.list()[0].requestId, "req-2");
});

const helicopterRequest = { kind: "helicopter", folder: "C:/repo", snapshotId: "snap", area: "helicopter" };

test("a Helicopter request has its own ticket, returns to the Helicopter tab and never resends", async () => {
  const h = harness();
  h.env.admit = h.status("pending", { requestId: "hel-1", kind: "helicopter" });
  h.env.reads = [h.status("pending", { requestId: "hel-1", kind: "helicopter" })];
  await h.book.generate(helicopterRequest);
  const [ticket] = h.book.list();
  assert.equal(ticket.kind, "helicopter");
  assert.equal(ticket.destinationTab, "helicopter");
  assert.equal(ticket.requestId, "hel-1");
  assert.equal(ticket.producer.sessionId, "s", "bound to the producer the backend admitted it with");
  assert.deepEqual(h.log.opened, ["arch"]);
  h.env.reads = [h.status("pending", { requestId: "hel-1", kind: "helicopter" }), h.status("completed", { requestId: "hel-1", kind: "helicopter", result: { kind: "helicopter" }, resultHash: "h" })];
  await h.advance(3000);
  assert.equal(h.book.list()[0].state, "completed");
  assert.deepEqual(h.log.returned, [ticket.key]);
  assert.equal(h.log.request.length, 1);
});

test("a Helicopter result that arrives after navigation is a late action and opens only on request", async () => {
  const h = harness();
  h.env.admit = h.status("pending", { requestId: "hel-1", kind: "helicopter" });
  h.env.reads = [h.status("pending", { requestId: "hel-1", kind: "helicopter" })];
  await h.book.generate(helicopterRequest);
  h.env.epoch++;
  h.env.reads = [h.status("completed", { requestId: "hel-1", kind: "helicopter", result: { kind: "helicopter" }, resultHash: "h" })];
  await h.advance(3000);
  assert.deepEqual(h.log.returned, []);
  const [ticket] = h.book.list();
  assert.equal(ticket.lateResult, true);
  assert.equal(h.book.openResult(ticket.key), true);
  assert.deepEqual(h.log.returned, [ticket.key]);
});

test("System and Helicopter requests for one snapshot are independent tickets on the same Architect; neither triggers the other", async () => {
  for (const order of [["systemOverview", "helicopter"], ["helicopter", "systemOverview"]]) {
    const h = harness({ admitState: "completed" });
    const saved = { systemOverview: { requestId: "sys-1", result: { kind: "systemOverview" } }, helicopter: { requestId: "hel-1", kind: "helicopter", result: { kind: "helicopter" } } };
    const requests = { systemOverview: h.request, helicopter: helicopterRequest };
    for (const kind of order) {
      h.env.admit = h.status("completed", { ...saved[kind], resultHash: "h" });
      await h.book.generate(requests[kind]);
    }
    assert.deepEqual(h.log.request.map(args => args[1]), order, "each explicit click sends exactly its own kind");
    const tickets = h.book.list();
    assert.equal(tickets.length, 2);
    assert.notEqual(tickets[0].key, tickets[1].key);
    assert.deepEqual(tickets.map(ticket => ticket.destinationTab).sort(), ["helicopter", "system"]);
    assert.deepEqual(new Set(tickets.map(ticket => ticket.producer.sessionId)).size, 1, "both are bound to the one existing Architect session");
    assert.deepEqual(h.log.opened, [], "saved results open passively without focusing the Architect");
  }
});
