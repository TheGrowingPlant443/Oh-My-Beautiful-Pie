import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";

const core = await import("./overview-layout-core.ts");
const clientSource = (await readFile(new URL("./overview-layout.ts", import.meta.url), "utf8")).replace('"./overview-layout-core"', JSON.stringify(new URL("./overview-layout-core.ts", import.meta.url).href));
const { LayoutClient, positionMap } = await import(`data:text/javascript,${encodeURIComponent(stripTypeScriptTypes(clientSource))}`);

/** Same protocol as the real worker, run in-process with a controllable event loop. */
class FakeWorker {
  constructor(options = {}) {
    this.options = options;
    this.cancelled = new Set();
    this.onmessage = null;
    this.onerror = null;
    this.terminated = false;
    this.sent = [];
  }
  postMessage(message) {
    this.sent.push(message);
    if (message.type === "cancel") { this.cancelled.add(message.jobId); return; }
    const version = this.options.version ?? message.version;
    void core.runLayout({ seeds: message.seeds, edges: message.edges, iterations: message.iterations }, {
      cancelled: () => this.cancelled.has(message.jobId),
      yieldNow: () => new Promise(resolve => setTimeout(resolve, 0)),
      progress: (positions, iteration) => this.onmessage?.({ data: { type: "progress", version, jobId: message.jobId, iteration, positions } }),
    }).then(positions => this.onmessage?.({ data: positions === null ? { type: "cancelled", version, jobId: message.jobId } : { type: "done", version, jobId: message.jobId, positions } }),
      error => this.onmessage?.({ data: { type: "error", version, jobId: message.jobId, message: error.message } }));
  }
  terminate() { this.terminated = true; }
}

const ids = ["a", "b", "c", "d"];
const edges = [["a", "b"], ["b", "c"], ["c", "d"]];

test("layout finishes with finite positions for every node, in id order, and reports progress", async () => {
  const worker = new FakeWorker();
  const client = new LayoutClient(() => worker);
  const progress = [];
  const result = await client.run({ ids, edges, iterations: 60, onProgress: positions => progress.push(positions.ids.length) });
  assert.deepEqual(result.ids, ids);
  for (let i = 0; i < ids.length; i++) assert.ok(Number.isFinite(result.x[i]) && Number.isFinite(result.y[i]));
  assert.ok(progress.length >= 2 && progress.every(count => count === ids.length));
  assert.equal(positionMap(result).size, 4);
  assert.equal(worker.sent[0].version, core.LAYOUT_PROTOCOL_VERSION);
});

test("a new run cancels the previous one (null) and only the newest result is delivered", async () => {
  const worker = new FakeWorker();
  const client = new LayoutClient(() => worker);
  const first = client.run({ ids, edges, iterations: 500 });
  const second = client.run({ ids: ["x", "y"], edges: [["x", "y"]], iterations: 30 });
  assert.equal(await first, null);
  const result = await second;
  assert.deepEqual(result.ids, ["x", "y"]);
  assert.ok(worker.sent.some(message => message.type === "cancel" && message.jobId === 1));
});

test("cancel stops the running job without terminating the worker; dispose releases it", async () => {
  const worker = new FakeWorker();
  const client = new LayoutClient(() => worker);
  const pending = client.run({ ids, edges, iterations: 100000 });
  client.cancel();
  assert.equal(await pending, null);
  assert.equal(worker.terminated, false);
  client.dispose();
  assert.equal(worker.terminated, true);
});

test("a worker speaking another protocol version is refused, a crashed worker is replaced on the next run", async () => {
  const stale = new FakeWorker({ version: 99 });
  const workers = [stale];
  const client = new LayoutClient(() => workers.shift() ?? new FakeWorker());
  await assert.rejects(client.run({ ids, edges, iterations: 10 }), /protocol 99/);
  const crashing = new FakeWorker();
  workers.push(crashing);
  const pending = client.run({ ids, edges, iterations: 100000 });
  crashing.onerror({ message: "boom" });
  await assert.rejects(pending, /boom/);
  assert.equal(crashing.terminated, true);
  const recovered = await client.run({ ids, edges, iterations: 10 });
  assert.equal(recovered.ids.length, 4);
});

test("late messages from an earlier job are ignored", async () => {
  const worker = new FakeWorker();
  const client = new LayoutClient(() => worker);
  const first = client.run({ ids, edges, iterations: 10 });
  const result = await first;
  assert.equal(result.ids.length, 4);
  // A stray message for an already finished job id changes nothing.
  worker.onmessage({ data: { type: "done", version: core.LAYOUT_PROTOCOL_VERSION, jobId: 1, positions: new Float32Array(8) } });
  const next = await client.run({ ids: ["q"], edges: [], iterations: 5 });
  assert.deepEqual(next.ids, ["q"]);
});

test("seeded positions are deterministic and distinct, and invalid jobs are rejected", async () => {
  const a = core.seededPosition("src/a.ts", 0), again = core.seededPosition("src/a.ts", 0), b = core.seededPosition("src/b.ts", 1);
  assert.deepEqual(a, again);
  assert.notDeepEqual(a, b);
  await assert.rejects(core.runLayout({ seeds: new Float32Array(3), edges: new Uint32Array(0), iterations: 1 }, { cancelled: () => false, yieldNow: async () => {} }), /seeds/);
  await assert.rejects(core.runLayout({ seeds: new Float32Array(4), edges: Uint32Array.from([0, 5]), iterations: 1 }, { cancelled: () => false, yieldNow: async () => {} }), /does not exist/);
  assert.equal((await core.runLayout({ seeds: new Float32Array(0), edges: new Uint32Array(0), iterations: 5 }, { cancelled: () => false, yieldNow: async () => {} })).length, 0);
});
