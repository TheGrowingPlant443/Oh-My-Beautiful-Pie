import test from "node:test";
import assert from "node:assert/strict";
import { get } from "svelte/store";
import { importTs } from "./test-loader.mjs";

const { createGraphifyAccess } = await importTs("graphify-access");

const A = { folder: "C:/a", projectKey: "c:/a" };
const B = { folder: "C:/b", projectKey: "c:/b" };
const caps = (project, graphify = {}) => ({
  folder: project.folder, projectKey: project.projectKey,
  graphify: { present: true, state: "present", sourceRoot: project.folder, artifactId: null, artifactHash: null, message: null, ...graphify },
});
const artifact = { artifactId: "id1", artifactHash: "h1" };
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };

function rig(overrides = {}) {
  const log = [];
  let shown = A;
  const gates = { pick: deferred(), register: deferred(), open: deferred() };
  const access = createGraphifyAccess({
    api: {
      registerGraphifyOutput: async (folder, directory) => { log.push(["register", folder, directory]); return overrides.register ? overrides.register(folder, directory) : gates.register.promise; },
      openGraphifyArtifact: async folder => { log.push(["open", folder]); return overrides.open ? overrides.open(folder) : gates.open.promise; },
    },
    pick: async () => { log.push(["pick"]); return overrides.pick ? overrides.pick() : gates.pick.promise; },
    current: () => shown,
    adopt: value => log.push(["adopt", value.folder]),
    navigate: (folder, opened) => log.push(["navigate", folder, opened.artifactId]),
  });
  return { access, log, gates, show: project => { shown = project; } };
}
const tick = () => new Promise(resolve => setImmediate(resolve));
const changed = log => log.some(entry => entry[0] === "adopt" || entry[0] === "navigate");

test("choosing an output registers it, admits the immutable artifact, then adopts and navigates once", async () => {
  const { access, log, gates } = rig();
  const running = access.choose("C:/a", "c:/a");
  assert.equal(get(access).busy, true);
  gates.pick.resolve("D:/graphs/a/graphify-out");
  await tick();
  assert.deepEqual(log, [["pick"], ["register", "C:/a", "D:/graphs/a/graphify-out"]]);
  gates.register.resolve(caps(A));
  await tick();
  assert.deepEqual(log.at(-1), ["open", "C:/a"], "the artifact is admitted before the view opens");
  assert.equal(changed(log), false);
  gates.open.resolve(artifact);
  assert.equal(await running, "opened");
  assert.deepEqual(log.slice(-2), [["adopt", "C:/a"], ["navigate", "C:/a", "id1"]]);
  assert.deepEqual(get(access), { busy: false, error: null });
});

test("cancelling the picker changes nothing and shows no error", async () => {
  const { access, log } = rig({ pick: async () => null });
  assert.equal(await access.choose("C:/a", "c:/a"), "cancelled");
  assert.deepEqual(log, [["pick"]]);
  assert.deepEqual(get(access), { busy: false, error: null });
});

test("a refused registration stays visible, adopts and navigates nowhere, and can be retried", async () => {
  let attempt = 0;
  const { access, log } = rig({
    pick: async () => "D:/copy/graphify-out",
    register: async () => { if (attempt++ === 0) throw "Foreign Graphify output: it was built for C:/original, not for this project, so it is not shown."; return caps(A); },
    open: async () => artifact,
  });
  assert.equal(await access.choose("C:/a", "c:/a"), "failed");
  assert.match(get(access).error, /built for C:\/original/);
  assert.equal(changed(log) || log.some(entry => entry[0] === "open"), false);
  assert.equal(await access.choose("C:/a", "c:/a"), "opened", "failure never leaves the control stuck");
  assert.equal(get(access).error, null);
});

test("an answer that is not a usable output or is for another project is a visible failure", async () => {
  for (const [answer, pattern] of [
    [caps(A, { present: false, state: "absent", message: "No graph.json was found" }), /No graph\.json/],
    [caps(A, { state: "foreign", message: "Foreign Graphify output" }), /Foreign/],
    [caps(B), /different project/],
    [{ ...caps(A), projectKey: null }, /different project/],
  ]) {
    const { access, log } = rig({ pick: async () => "D:/x", register: async () => answer });
    assert.equal(await access.choose("C:/a", "c:/a"), "failed");
    assert.match(get(access).error, pattern);
    assert.equal(changed(log) || log.some(entry => entry[0] === "open"), false);
  }
});

test("switching project during the picker, even A to B to A, makes the late result change nothing", async () => {
  const { access, log, gates, show } = rig();
  const running = access.choose("C:/a", "c:/a");
  show(B);
  access.invalidate();
  show(A);
  gates.pick.resolve("D:/graphs/a");
  assert.equal(await running, "stale");
  assert.deepEqual(log, [["pick"]], "nothing is registered for a result of an invalidated operation");
  assert.deepEqual(get(access), { busy: false, error: null });
});

test("a project that differs when the picker returns is stale even without an explicit invalidation", async () => {
  const { access, log, gates, show } = rig();
  const running = access.choose("C:/a", "c:/a");
  show(B);
  gates.pick.resolve("D:/graphs/a");
  assert.equal(await running, "stale");
  assert.deepEqual(log, [["pick"]]);
  assert.equal(get(access).busy, false, "the control is released");
});

test("a project switch during registration or admission never adopts or navigates", async () => {
  for (const phase of ["register", "open"]) {
    const { access, log, gates, show } = rig();
    const running = access.choose("C:/a", "c:/a");
    gates.pick.resolve("D:/graphs/a");
    await tick();
    if (phase === "register") {
      show(B);
      gates.register.resolve(caps(A));
    } else {
      gates.register.resolve(caps(A));
      await tick();
      show(B);
      gates.open.resolve(artifact);
    }
    assert.equal(await running, "stale", phase);
    assert.equal(changed(log), false, phase);
    assert.equal(log.some(entry => entry[0] === "open"), phase === "open", "no admission is requested for a stale registration");
  }
});

test("a second click while one operation runs does not open another picker", async () => {
  const { access, log, gates } = rig();
  const first = access.choose("C:/a", "c:/a");
  assert.equal(await access.choose("C:/a", "c:/a"), "stale");
  assert.deepEqual(log, [["pick"]]);
  gates.pick.resolve(null);
  assert.equal(await first, "cancelled");
});
