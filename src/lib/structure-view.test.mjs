import test from "node:test";
import assert from "node:assert/strict";
import {
  approvalNotice, buildCurrentTree, canApprove, citationTexts, conflictList, conventionRows, differentDocument, entryForGroup, entryForPath, entryLookup,
  evidenceText, excerptLine, generalize, generalPaths, indexChanged, mappingIndex, mappingsForProposed, mappingsFromCurrent, mappingText, proposedTree,
  reconcileGeneral, shortHash, sizeText, sourceKindLabel, treePaths,
} from "./structure-view.ts";

const row = (path, extra = {}) => ({ id: `f:${path}`, path, sha256: "0".repeat(64), bytes: 10, language: null, status: "indexed", diagnostics: [], ...extra });
const find = (node, path) => {
  if (node.path === path && node.kind !== "group") return node;
  for (const child of node.children ?? node.nodes ?? []) { const hit = find(child, path); if (hit) return hit; }
  return null;
};

test("complete inventory: every file and directory, empty ones, exact case and Unicode, stable order", () => {
  const files = [row("src/b.ts"), row("src/A.ts"), row("src/a.ts"), row("README.md"), row("日本/ファイル.txt"), row("src/b.ts")];
  const tree = buildCurrentTree(files, ["empty", "src/deep/empty", "日本", ""]);
  const paths = treePaths(tree);
  assert.deepEqual(paths.files, ["README.md", "src/A.ts", "src/a.ts", "src/b.ts", "日本/ファイル.txt"]);
  assert.deepEqual(paths.directories, ["empty", "src", "src/deep", "src/deep/empty", "日本"]);
  assert.equal(tree.fileCount, 5);
  assert.equal(tree.dirCount, 5);
  const src = tree.dirs.find(dir => dir.path === "src");
  assert.deepEqual(src.files.map(file => file.name), ["A.ts", "a.ts", "b.ts"]);
  assert.equal(src.fileCount, 3);
  assert.equal(src.dirCount, 2);
  assert.deepEqual(buildCurrentTree([...files].reverse(), ["日本", "src/deep/empty", "empty"]), tree);
});

test("sibling directories with the same child shape collapse to one group that keeps exact members and counts", () => {
  const files = [];
  for (let i = 0; i < 12; i++) files.push(row(`components/c${i}/index.ts`), row(`components/c${i}/c${i}.test.ts`));
  files.push(row("components/odd/other.ts"), row("main.ts"));
  const tree = buildCurrentTree(files, ["components/emptyA"]);
  const general = generalize(tree);
  const components = find(general, "components");
  const group = components.children.find(child => child.kind === "group");
  assert.ok(group);
  assert.equal(group.label, "components/*/ (12 similar)");
  assert.equal(group.memberKind, "dir");
  assert.equal(group.pattern, "*/{*.test.ts, index.ts}");
  assert.deepEqual(group.members, Array.from({ length: 12 }, (_, i) => `components/c${i}`).sort());
  assert.equal(group.fileCount, 24);
  assert.equal(group.dirCount, 12);
  assert.equal(group.nodes.length, 12);
  assert.ok(components.children.some(child => child.kind === "dir" && child.path === "components/odd"));
  assert.ok(components.children.some(child => child.kind === "dir" && child.path === "components/emptyA"));
  assert.ok(reconcileGeneral(tree, general));
  assert.equal(generalPaths(general).files.length, files.length);
});

test("five or more sibling files with the same suffix group; fewer stay individual; nothing is dropped", () => {
  const files = ["A", "B", "C", "D", "E", "F"].map(name => row(`lib/${name}.svelte`));
  files.push(row("lib/x.test.ts"), row("lib/y.test.ts"), row("lib/z.ts"), row("lib/.hidden"), row("lib/Makefile"));
  const tree = buildCurrentTree(files, []);
  const general = generalize(tree);
  const lib = find(general, "lib");
  const groups = lib.children.filter(child => child.kind === "group");
  assert.equal(groups.length, 1);
  assert.equal(groups[0].label, "lib/*.svelte (6 similar)");
  assert.equal(groups[0].pattern, "*.svelte");
  assert.deepEqual(groups[0].members, files.slice(0, 6).map(file => file.path));
  assert.deepEqual(lib.children.filter(child => child.kind === "file").map(child => child.name), [".hidden", "Makefile", "x.test.ts", "y.test.ts", "z.ts"]);
  assert.ok(reconcileGeneral(tree, general));
});

test("reconcile detects a generalized view that lost a path", () => {
  const files = Array.from({ length: 6 }, (_, i) => row(`a/F${i}.svelte`));
  const tree = buildCurrentTree(files, []);
  const general = generalize(tree);
  const group = find(general, "a").children.find(child => child.kind === "group");
  group.nodes.pop();
  assert.equal(reconcileGeneral(tree, general), false);
});

test("groups apply recursively and the empty root generalizes", () => {
  assert.deepEqual(generalize(buildCurrentTree([], [])).children, []);
  const files = [];
  for (let i = 0; i < 4; i++) for (let j = 0; j < 5; j++) files.push(row(`f${i}/g${j}/index.ts`));
  const tree = buildCurrentTree(files, []);
  const general = generalize(tree);
  assert.ok(reconcileGeneral(tree, general));
  const outer = general.children.find(child => child.kind === "group");
  assert.equal(outer.members.length, 4);
  assert.equal(outer.fileCount, 20);
  assert.equal(outer.dirCount, 4 + 20);
  const inner = outer.nodes[0].children.find(child => child.kind === "group");
  assert.equal(inner.members.length, 5);
});

const payload = (extra = {}) => ({
  schemaVersion: 1, snapshotId: "snap", sourceId: "src", sourceHash: "h", baseRequestId: null, supersedesRequestId: null,
  entries: [], conventions: [], mappings: [], conflicts: [], evidence: [], limitations: [], ...extra,
});
const entry = (id, parentId, extra = {}) => ({ id, parentId, label: id, path: null, pattern: null, members: [], purpose: "", evidenceIds: [], ...extra });

test("current analysis overlay: purposes by path/members/group, conventions and citations", () => {
  const p = payload({
    entries: [entry("e1", null, { path: "src/ui", purpose: "UI", evidenceIds: ["ev1", "ev9"] }), entry("e2", null, { members: ["lib/a.ts", "lib/b.ts"], purpose: "Helpers" })],
    evidence: [{ id: "ev1", path: "src/ui/index.ts", lineStart: 3, lineEnd: 9, contentHash: "x" }],
    conventions: [{ scope: "components", current: "PascalCase", proposed: "kebab-case", rationale: "r", evidenceIds: ["ev1"] }],
  });
  const lookup = entryLookup(p);
  assert.equal(entryForPath(lookup, "src/ui").purpose, "UI");
  assert.equal(entryForPath(lookup, "lib/b.ts").purpose, "Helpers");
  assert.equal(entryForPath(lookup, "nope"), null);
  assert.equal(entryForGroup(lookup, { members: ["x", "lib/a.ts", "lib/b.ts"] }).id, "e2");
  assert.equal(entryForGroup(lookup, { members: ["x"] }), null);
  assert.deepEqual(citationTexts(lookup, ["ev1", "ev9"]), ["src/ui/index.ts:3-9", "ev9 (evidence not included)"]);
  assert.equal(evidenceText(p.evidence[0]), "src/ui/index.ts:3-9");
  assert.deepEqual(conventionRows(p), [{ scope: "components", current: "PascalCase", proposed: "kebab-case", rationale: "r", citations: ["src/ui/index.ts:3-9"] }]);
  assert.deepEqual(conventionRows(null), []);
  assert.equal(entryForPath(entryLookup(null), "a"), null);
});

test("proposed tree follows parent links and survives cycles, self parents, missing parents and duplicate ids", () => {
  const tree = proposedTree(payload({ entries: [entry("root", null), entry("a", "root"), entry("b", "root"), entry("a1", "a")] }));
  assert.deepEqual(tree.roots.map(node => node.id), ["root"]);
  assert.deepEqual(tree.roots[0].children.map(node => node.id), ["a", "b"]);
  assert.deepEqual(tree.roots[0].children[0].children.map(node => node.id), ["a1"]);
  assert.deepEqual(tree.issues, []);

  const cyclic = proposedTree(payload({ entries: [entry("x", "y"), entry("y", "x"), entry("z", "z"), entry("m", "ghost"), entry("w", "x"), entry("x", null)] }));
  const seen = new Set();
  const walk = node => { assert.ok(!seen.has(node.id), `${node.id} visited twice`); seen.add(node.id); node.children.forEach(walk); };
  cyclic.roots.forEach(walk);
  assert.deepEqual([...seen].sort(), ["m", "w", "x", "y", "z"]);
  assert.ok(cyclic.roots.some(node => node.cycleBroken));
  assert.ok(cyclic.issues.some(issue => issue.includes("Duplicate entry id x")));
  assert.ok(cyclic.issues.some(issue => issue.includes("missing parent ghost")));
  assert.deepEqual(proposedTree(null), { roots: [], issues: [] });
});

test("mapping lookups in both directions; splits and merges are explicit", () => {
  const p = payload({ mappings: [
    { fromPaths: ["a.ts"], toPaths: ["b.ts"], operation: "rename", rationale: "" },
    { fromPaths: ["big.ts"], toPaths: ["p1.ts", "p2.ts"], operation: "split", rationale: "" },
    { fromPaths: ["m1.ts", "m2.ts"], toPaths: ["m.ts"], operation: "merge", rationale: "" },
    { fromPaths: [], toPaths: ["new.ts"], operation: "create", rationale: "" },
    { fromPaths: ["old.ts"], toPaths: [], operation: "remove", rationale: "" },
    { fromPaths: ["same.ts"], toPaths: ["same.ts"], operation: "unchanged", rationale: "" },
  ] });
  const index = mappingIndex(p);
  assert.equal(mappingsFromCurrent(index, "big.ts")[0].operation, "split");
  assert.deepEqual(mappingsFromCurrent(index, "nowhere"), []);
  assert.equal(mappingsForProposed(index, { path: "p2.ts", members: [] })[0].operation, "split");
  assert.equal(mappingsForProposed(index, { path: null, members: ["m.ts"] })[0].operation, "merge");
  assert.equal(mappingsForProposed(index, { path: "m.ts", members: ["m.ts"] }).length, 1);
  assert.deepEqual(p.mappings.map(mappingText), [
    "Rename: a.ts → b.ts", "Split: big.ts → p1.ts, p2.ts", "Merge: m1.ts, m2.ts → m.ts", "Create: new.ts", "Remove: old.ts", "Unchanged: same.ts",
  ]);
  assert.deepEqual(conflictList(payload({ conflicts: [" a ", "", "  "] })), ["a"]);
});

const proposed = (extra = {}) => ({
  requestId: "r2", folder: "C:/p", kind: "structureProposed", state: "completed", message: null,
  result: { kind: "structureProposed", ...payload() }, createdAt: 2, resultHash: "rh", snapshotId: "snap", baseRequestId: null,
  supersedesRequestId: null, supersededBy: null, sourceId: "src", sourceHash: "h", producer: null, approval: null, ...extra,
});
const ctx = { snapshotId: "snap", source: { sourceId: "src", sha256: "h" } };

test("canApprove matrix", () => {
  assert.deepEqual(canApprove(proposed(), ctx), { ok: true, reason: null });
  const cases = [
    [null, ctx, /No target/],
    [proposed({ kind: "structureCurrent" }), ctx, /Only a target/],
    [proposed({ state: "submitted", result: null }), ctx, /not completed/],
    [proposed({ state: "failed" }), ctx, /not completed/],
    [proposed({ resultHash: null }), ctx, /result hash/],
    [proposed({ resultHash: "" }), ctx, /result hash/],
    [proposed({ sourceHash: null }), ctx, /source hash/],
    [proposed({ supersededBy: "r3" }), ctx, /newer preview/],
    [proposed({ approval: { approvedAt: 1, resultHash: "rh", sourceHash: "h", hasConflicts: false, delivery: "sent", deliveryError: null } }), ctx, /already approved/],
    [proposed({ snapshotId: "old" }), ctx, /Index changed/],
    [proposed({ snapshotId: null }), ctx, /Index changed/],
    [proposed(), { ...ctx, snapshotId: null }, /No source index snapshot/],
    [proposed(), { ...ctx, snapshotId: "new" }, /Index changed/],
    [proposed(), { ...ctx, source: null }, /No target Markdown/],
    [proposed({ sourceId: "other" }), ctx, /different document/],
    [proposed(), { ...ctx, source: { sourceId: "src", sha256: "zz" } }, /different document|inconsistent/],
    [proposed({ result: { kind: "structureProposed", ...payload({ sourceHash: "other" }) } }), ctx, /inconsistent/],
  ];
  for (const [revision, context, pattern] of cases) {
    const verdict = canApprove(revision, context);
    assert.equal(verdict.ok, false, String(pattern));
    assert.match(verdict.reason, pattern);
  }
});

test("stale and different-document badges", () => {
  assert.equal(indexChanged({ snapshotId: "a" }, "b"), true);
  assert.equal(indexChanged({ snapshotId: "a" }, "a"), false);
  assert.equal(indexChanged({ snapshotId: null }, "a"), false);
  assert.equal(differentDocument({ sourceId: "s", sourceHash: "h" }, { sourceId: "t", sha256: "h" }), true);
  assert.equal(differentDocument({ sourceId: "s", sourceHash: "h" }, { sourceId: "s", sha256: "h" }), false);
  assert.equal(differentDocument({ sourceId: "s", sourceHash: "h" }, null), true);
  assert.equal(differentDocument({ sourceId: null, sourceHash: null }, null), false);
});

test("approval delivery notices never auto-resend and offer the Architect", () => {
  const notice = delivery => approvalNotice({ approvedAt: 1, resultHash: "r", sourceHash: "s", hasConflicts: false, delivery, deliveryError: "boom" });
  assert.equal(notice("sent").openArchitect, false);
  assert.equal(notice("claimed").openArchitect, false);
  assert.equal(notice("pending").openArchitect, false);
  assert.match(notice("pending").text, /pending/);
  for (const delivery of ["uncertain", "failed", "unavailable"]) {
    const value = notice(delivery);
    assert.equal(value.openArchitect, true);
    assert.match(value.text, /not be resent automatically/);
    assert.match(value.text, /boom/);
  }
  assert.doesNotMatch(notice("sent").text, /authoriz/i);
});

test("display helpers", () => {
  assert.equal(excerptLine("\n\n  # Title   here \nbody"), "# Title here");
  assert.equal(excerptLine("x".repeat(200), 10), `${"x".repeat(9)}…`);
  assert.equal(excerptLine(""), "");
  assert.equal(shortHash("abcdef0123456789"), "abcdef01");
  assert.equal(shortHash(null), "—");
  assert.equal(sourceKindLabel("interactiveMessage"), "Inline message");
  assert.equal(sourceKindLabel("interactiveLocalPaste"), "Long paste file");
  assert.equal(sizeText(12), "12 B");
  assert.equal(sizeText(2048), "2.0 KiB");
  assert.equal(sizeText(64 * 1024), "64 KiB");
  assert.equal(sizeText(3 * 1024 * 1024), "3.0 MiB");
});
