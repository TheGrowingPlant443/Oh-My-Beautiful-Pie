import test from "node:test";
import assert from "node:assert/strict";
import { get } from "svelte/store";
import { importTs } from "./test-loader.mjs";

const { agentSetupPreview, agentMarkdownTargetKey, canApplyMarkdown, unappliedMarkdown, createAgentMarkdownStore } = await importTs("agent-markdown");
const globalTarget = { kind: "global" };
const managerTarget = { kind: "manager" };
function item(id, state, extra = {}) {
  return { id, state, nativePath: `/native/${id}`, vaultPath: `Agent setup/Global/${id}`, sourceHash: "source", nativeHash: "native", appliedHash: "applied", diff: "-before\n+after", nativeDependencies: [], ...extra };
}
function status(target = globalTarget, extra = {}) {
  return { target, vaultId: "vault", namespace: "Agent setup/Global", nativeRoot: "/native", sourceRoot: "/vault/Agent setup/Global", enabled: true, sourceRevision: "revision-1", appliedRevision: "revision-0", items: [], warnings: [], ...extra };
}
function backend(overrides = {}) {
  const calls = [];
  return {
    calls,
    async status(target) { calls.push(["status", target]); return status(target); },
    async prepareImport(target, selectedNativeIds) { calls.push(["prepareImport", target, selectedNativeIds]); return { token: "import-token", target, items: [item("AGENTS.md", "nativeOnly")], selectedIds: selectedNativeIds ?? ["AGENTS.md"], warnings: [] }; },
    async confirmImport(token) { calls.push(["confirmImport", token]); return status(); },
    async prepareApply(target, sourceRevision) { calls.push(["prepareApply", target, sourceRevision]); return { token: "apply-token", target, sourceRevision, items: [], selectedIds: [], warnings: [] }; },
    async apply(token, selectedIds) { calls.push(["apply", token, selectedIds]); return status(); },
    ...overrides,
  };
}

test("only new, changed and deleted Markdown can be applied", () => {
  for (const state of ["nativeOnly", "new", "changed", "deleted", "unchanged", "nativeChanged", "conflict"]) {
    assert.equal(canApplyMarkdown(item("x.md", state)), ["new", "changed", "deleted"].includes(state), state);
  }
  assert.deepEqual(unappliedMarkdown(status(globalTarget, { items: [item("a", "changed"), item("b", "conflict"), item("c", "nativeChanged"), item("d", "deleted")] })).map(item => item.id), ["a", "b", "d"]);
});

test("a vault binding without explicit migration keeps every setup preview native", () => {
  const bound = status(globalTarget, { enabled: false });
  for (const source of [null, bound]) {
    assert.deepEqual(agentSetupPreview({ kind: "agents", name: "", description: "test" }, "/native/", source, "/omp"), [{ nativePath: "/native/AGENTS.md", sourcePath: null }]);
  }
});

test("enabled Markdown previews show both source and runtime for each supported asset", () => {
  const source = status();
  const cases = [
    [{ kind: "agents", name: "Review Agent" }, ["agent-instructions/review-agent/AGENTS.md"]],
    [{ kind: "rules", name: "" }, ["RULES.md"]],
    [{ kind: "skill", name: "audit" }, ["skills/audit/SKILL.md", "SKILLS.md"]],
    [{ kind: "workflow", name: "review" }, ["rules/workflow-review.md"]],
  ];
  for (const [input, expected] of cases) {
    assert.deepEqual(agentSetupPreview({ description: "test", ...input }, "/repository/.omp", source, "/omp"), expected.map(path => ({ nativePath: `/repository/.omp/${path}`, sourcePath: `/vault/Agent setup/Global/${path}` })));
  }
});

test("tools, plugin implementation and named profile settings never move to Markdown", () => {
  const cases = [
    [{ kind: "tool", name: "audit", language: "python" }, ["tools/audit/index.ts", "tools/audit/run.py"]],
    [{ kind: "tool", name: "audit", language: "bash" }, ["tools/audit/index.ts", "tools/audit/run.sh"]],
    [{ kind: "tool", name: "audit", language: "typescript" }, ["tools/audit/index.ts"]],
    [{ kind: "plugin", name: "audit" }, ["extensions/audit/package.json", "extensions/audit/index.ts"]],
  ];
  for (const [input, expected] of cases) assert.deepEqual(agentSetupPreview({ description: "test", ...input }, "/native", status(), "/omp"), expected.map(path => ({ nativePath: `/native/${path}`, sourcePath: null })));
  assert.deepEqual(agentSetupPreview({ kind: "profile", name: "plan-mode", description: "test" }, "/native", status(), "/omp"), [{ nativePath: "/omp/profiles/plan-mode/agent/config.yml", sourcePath: null }]);
});

test("target identities distinguish repository instances and named profiles", () => {
  const targets = [globalTarget, managerTarget, { kind: "architect" }, { kind: "repository", projectInstanceId: "a" }, { kind: "repository", projectInstanceId: "b" }, { kind: "profile", profileName: "a" }];
  assert.equal(new Set(targets.map(agentMarkdownTargetKey)).size, targets.length);
});

test("late old-target status and errors cannot replace the current target", async () => {
  for (const reject of [false, true]) {
    const pending = Promise.withResolvers();
    const api = backend({ status: target => target.kind === "global" ? pending.promise : Promise.resolve(status(target)) });
    const store = createAgentMarkdownStore(api);
    const first = store.selectTarget(globalTarget);
    await store.selectTarget(managerTarget);
    if (reject) pending.reject(new Error("old target failed")); else pending.resolve(status());
    await first;
    assert.deepEqual(get(store).target, managerTarget);
    assert.deepEqual(get(store).status.target, managerTarget);
    assert.equal(get(store).error, null);
    assert.equal(get(store).busy, false);
  }
});

test("the newest same-target refresh wins", async () => {
  const old = Promise.withResolvers();
  let reads = 0;
  const store = createAgentMarkdownStore(backend({ status: async target => ++reads === 2 ? old.promise : status(target, { sourceRevision: `r${reads}` }) }));
  await store.selectTarget(globalTarget);
  const pending = store.refresh();
  await store.refresh();
  old.resolve(status(globalTarget, { sourceRevision: "stale" }));
  await pending;
  assert.equal(get(store).status.sourceRevision, "r3");
});

test("late preparations do not leak tokens or busy state to a changed target", async () => {
  const pending = Promise.withResolvers();
  const store = createAgentMarkdownStore(backend({ prepareImport: () => pending.promise }));
  await store.selectTarget(globalTarget);
  const old = store.prepare("import");
  await store.selectTarget(managerTarget);
  pending.resolve({ token: "old-token", target: globalTarget, items: [], selectedIds: [], warnings: [] });
  await old;
  assert.equal(get(store).review, null);
  assert.deepEqual(get(store).status.target, managerTarget);
  assert.equal(get(store).busy, false);
});

test("default import uses null; an explicit empty selection confirms and enables without copying", async () => {
  const api = backend();
  const store = createAgentMarkdownStore(api);
  await store.selectTarget(globalTarget);
  await store.prepare("import");
  assert.equal(api.calls.at(-1)[2], null);
  await store.prepare("import", []);
  assert.deepEqual(api.calls.at(-1)[2], []);
  assert.deepEqual(get(store).review.selectedIds, []);
  await store.confirm();
  assert.deepEqual(api.calls.at(-1), ["confirmImport", "import-token"]);
  assert.equal(get(store).status.enabled, true);
  assert.equal(get(store).review, null);
});

test("reviewing explicit conflict replacement obtains a new import token", async () => {
  let sequence = 0;
  const api = backend({ async prepareImport(target, ids) { return { token: `token-${++sequence}`, target, items: [item("AGENTS.md", "conflict")], selectedIds: ids ?? [], warnings: [] }; } });
  const store = createAgentMarkdownStore(api);
  await store.selectTarget(globalTarget);
  await store.prepare("import");
  assert.deepEqual(get(store).review.selectedIds, []);
  await store.prepare("import", ["AGENTS.md"]);
  assert.equal(get(store).review.preparation.token, "token-2");
  await store.confirm();
  assert.deepEqual(api.calls.at(-1), ["confirmImport", "token-2"]);
});

test("apply is pinned to source revision and only selected safe writes or deletions reach the API", async () => {
  const items = [item("new.md", "new"), item("changed.md", "changed"), item("deleted.md", "deleted", { sourceHash: null }), item("drift.md", "nativeChanged"), item("conflict.md", "conflict")];
  const api = backend();
  api.prepareApply = async (target, sourceRevision) => { api.calls.push(["prepareApply", target, sourceRevision]); return { token: "apply-token", target, sourceRevision, items, selectedIds: items.map(item => item.id), warnings: [] }; };
  const store = createAgentMarkdownStore(api);
  await store.selectTarget(globalTarget);
  await store.prepare("apply");
  assert.deepEqual(api.calls.at(-1), ["prepareApply", globalTarget, "revision-1"]);
  assert.deepEqual(get(store).review.selectedIds, ["new.md", "changed.md", "deleted.md"]);
  store.selectApply(["deleted.md", "drift.md", "conflict.md", "unknown", "deleted.md"]);
  assert.deepEqual(get(store).review.selectedIds, ["deleted.md"]);
  await store.confirm();
  assert.deepEqual(api.calls.at(-1), ["apply", "apply-token", ["deleted.md"]]);
});

test("refresh invalidates reviews and never silently reapplies a stale revision", async () => {
  const api = backend();
  const store = createAgentMarkdownStore(api);
  await store.selectTarget(globalTarget);
  await store.prepare("apply");
  await store.refresh();
  await store.confirm();
  assert.equal(api.calls.some(call => call[0] === "apply"), false);
  assert.equal(get(store).review, null);
});

test("ten-minute reviews expire before any confirm API call", async () => {
  let now = 10;
  const api = backend();
  const store = createAgentMarkdownStore(api, () => now);
  await store.selectTarget(globalTarget);
  await store.prepare("import");
  now += 10 * 60_000;
  await store.confirm();
  assert.match(get(store).error, /expired/);
  assert.equal(get(store).review, null);
  assert.equal(api.calls.some(call => call[0] === "confirmImport"), false);
});

test("a failed consumed confirmation cannot be replayed", async () => {
  let writes = 0;
  const store = createAgentMarkdownStore(backend({ async confirmImport() { writes++; throw new Error("native file changed"); } }));
  await store.selectTarget(globalTarget);
  await store.prepare("import");
  await store.confirm();
  await store.confirm();
  assert.equal(writes, 1);
  assert.equal(get(store).review, null);
  assert.match(get(store).error, /native file changed/);
});

test("a completed old-target write cannot replace the new target's status", async () => {
  const pending = Promise.withResolvers();
  const store = createAgentMarkdownStore(backend({ confirmImport: () => pending.promise }));
  await store.selectTarget(globalTarget);
  await store.prepare("import");
  const write = store.confirm();
  await store.selectTarget(managerTarget);
  pending.resolve(status());
  await write;
  assert.deepEqual(get(store).status.target, managerTarget);
  assert.equal(get(store).review, null);
});

test("disposal fences pending reads and preparation failures", async () => {
  const pending = Promise.withResolvers();
  const store = createAgentMarkdownStore(backend({ status: () => pending.promise }));
  const read = store.selectTarget(globalTarget);
  store.dispose();
  pending.resolve(status());
  await read;
  assert.equal(get(store).status, null);
});
