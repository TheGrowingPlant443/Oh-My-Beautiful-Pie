import test from "node:test";
import assert from "node:assert/strict";
import { visibleGraph, pendingDirect, nodeColor, nodeSize, pathToNode, focusCamera, packageTotals, buildPackageSearchIndex, searchPackages, exactNameMatch, parseDependencyPrefs, ROOT } from "./dependency-graph.ts";
import { projectFolders } from "./repo-agents.ts";

const node = (id, direct = false) => ({ id, name: id, version: null, resolvedVersions: [], declaredVersions: [], ecosystem: id === "project" ? "project" : "npm", direct, dev: false });
const graph = {
  folder: "C:/p", key: "0123456789abcdef", notes: [], truncated: false,
  nodes: [node("project"), node("A", true), node("B", true), node("C", true), node("T1"), node("T2"), node("T3")],
  edges: [["project", "A"], ["project", "B"], ["project", "C"], ["A", "T1"], ["B", "T2"], ["B", "T1"], ["T2", "T3"], ["A", "C"]],
};
const classification = { items: { A: { basic: false, reason: "" }, B: { basic: true, reason: "core" } }, updatedAt: 1 };
const ids = view => view.nodes.map(n => n.id).sort();
const pairs = view => view.edges.map(([a, b]) => `${a}>${b}`).sort();

test("transitive view without Show all drops basic packages and whatever only they reach", () => {
  const view = visibleGraph(graph, classification, "transitive", false);
  assert.deepEqual(ids(view), ["A", "C", "T1", "project"]);
  assert.equal(view.hidden, 3);
  assert.deepEqual(pairs(view), ["A>C", "A>T1", "project>A", "project>C"]);
});

test("Show all keeps everything", () => {
  const view = visibleGraph(graph, classification, "transitive", true);
  assert.equal(view.nodes.length, graph.nodes.length);
  assert.equal(view.edges.length, graph.edges.length);
  assert.equal(view.hidden, 0);
});

test("direct view shows only the project and its direct packages, with the edges among them", () => {
  const hidden = visibleGraph(graph, classification, "direct", false);
  assert.deepEqual(ids(hidden), ["A", "C", "project"]);
  assert.equal(hidden.hidden, 1);
  const all = visibleGraph(graph, classification, "direct", true);
  assert.deepEqual(ids(all), ["A", "B", "C", "project"]);
  assert.deepEqual(pairs(all), ["A>C", "project>A", "project>B", "project>C"]);
});

test("an unclassified graph hides nothing", () => {
  const view = visibleGraph(graph, { items: {}, updatedAt: null }, "transitive", false);
  assert.equal(view.hidden, 0);
});

test("pending packages are the direct ones without a judgement", () => {
  assert.deepEqual(pendingDirect(graph, classification), ["C"]);
  assert.deepEqual(pendingDirect(graph, { items: { ...classification.items, C: { basic: false, reason: "" } }, updatedAt: 2 }), []);
});

test("project folders list each saved session folder once", () => {
  const rows = [
    { kind: "instance", projectPath: "C:\\Work\\Repo\\" },
    { kind: "instance", projectPath: "c:/work/repo" },
    { kind: "manager", projectPath: "C:/Manager" },
    { kind: "architect", projectPath: "C:/Architect" },
    { kind: "instance", projectPath: "C:/another" },
  ];
  assert.deepEqual(projectFolders(rows), ["C:/another", "C:\\Work\\Repo\\"]);
});

test("colours and sizes follow role and popularity", () => {
  assert.equal(nodeColor(node("project"), false), "#4ea1ff");
  assert.equal(nodeColor(node("A", true), false), "#3fb950");
  assert.equal(nodeColor(node("B", true), true), "#d29922");
  assert.equal(nodeColor(node("T1"), false), "#61708a");
  assert.equal(nodeSize(0), 4);
  assert.equal(nodeSize(100000), 14);
});

test("layers are inclusive shortest surviving paths, with independent hidden counts", () => {
  const view = visibleGraph(graph, classification, "transitive", false, 1);
  assert.deepEqual(ids(view), ["A", "C", "project"]);
  assert.equal(view.hidden, 3);
  assert.equal(view.depthHidden, 1);
  assert.equal(view.maxDepth, 2);
  const all = visibleGraph(graph, classification, "transitive", true, 2);
  assert.deepEqual(ids(all), ["A", "B", "C", "T1", "T2", "project"]);
  assert.equal(all.hidden, 0);
  assert.equal(all.depthHidden, 1);
  assert.equal(all.maxDepth, 3);
});

test("a removed shortcut cannot lower the surviving depth; cycles terminate", () => {
  const withShortcut = {
    ...graph,
    edges: [...graph.edges, ["B", "T3"], ["T3", "T1"], ["T1", "T2"], ["T2", "T1"]],
  };
  const capped = visibleGraph(withShortcut, classification, "transitive", false, 2);
  assert.deepEqual(ids(capped), ["A", "C", "T1", "project"]);
  assert.equal(capped.hidden, 1);
  assert.equal(capped.depthHidden, 2);
  assert.equal(capped.maxDepth, 4);
  const complete = visibleGraph(withShortcut, classification, "transitive", false);
  assert.ok(complete.nodes.some(node => node.id === "T3"));
  assert.equal(complete.depthHidden, 0);
});

test("shorter alternate paths win even when encountered after long branches", () => {
  const alternate = { ...graph, edges: [...graph.edges, ["C", "T3"]] };
  const view = visibleGraph(alternate, classification, "transitive", true, 2);
  assert.ok(view.nodes.some(node => node.id === "T3"));
  assert.equal(view.maxDepth, 2);
  assert.ok(view.edges.every(([from, to]) => view.nodes.some(node => node.id === from) && view.nodes.some(node => node.id === to)));
  assert.deepEqual(ids(visibleGraph(alternate, classification, "direct", false, 1)), ["A", "C", "project"]);
});

test("version lag takes precedence over package role, but never recolours root", () => {
  assert.equal(nodeColor(node("A", true), false, 2), "#f85149");
  assert.equal(nodeColor(node("B", true), true, 3), "#f85149");
  assert.equal(nodeColor(node("T"), false, 2), "#f85149");
  assert.equal(nodeColor(node("A", true), false, 1), "#3fb950");
  assert.equal(nodeColor(node("project"), false, 8), "#4ea1ff");
});

test("path view keeps all root-to-target paths and induced edges without mutating inputs", () => {
  const view = { nodes: ["project", "a", "b", "t", "c", "isolated"].map(id => node(id)), edges: [["project", "a"], ["project", "b"], ["a", "t"], ["b", "t"], ["project", "c"], ["a", "b"], ["b", "a"]], hidden: 3, standardHidden: 2, depthHidden: 4, maxDepth: 7 };
  const before = structuredClone(view);
  const result = pathToNode(view, "t");
  assert.deepEqual(result.nodes.map(n => n.id), ["project", "a", "b", "t"]);
  assert.deepEqual(result.edges, view.edges.filter(([from, to]) => from !== "c" && to !== "c"));
  for (const key of ["hidden", "standardHidden", "depthHidden", "maxDepth"]) assert.equal(result[key], view[key]);
  assert.deepEqual(view, before);
  for (const target of ["project", "missing", "isolated"]) assert.equal(pathToNode(view, target), null);
  assert.equal(pathToNode({ ...view, nodes: view.nodes.filter(n => n.id !== "project") }, "t"), null);
});

test("focus camera preserves centre and frames wide and tall neighbours with bounded ratios", () => {
  const center = { x: .3, y: .7 };
  assert.deepEqual(focusCamera(center, [], { width: 1, height: 1 }), { ...center, ratio: .25 });
  assert.deepEqual(focusCamera(center, [center], { width: 1, height: 1 }), { ...center, ratio: .1 });
  assert.equal(focusCamera(center, [{ x: 20, y: 20 }], { width: 1, height: 1 }).ratio, 1);
  const wide = focusCamera(center, [{ x: .5, y: .7 }], { width: 2, height: 1 });
  const tall = focusCamera(center, [{ x: .3, y: .9 }], { width: 2, height: 1 });
  assert.equal(wide.x, center.x); assert.equal(wide.y, center.y);
  assert.ok(Math.abs(wide.ratio - .26) < 1e-10);
  assert.ok(Math.abs(tall.ratio - .52) < 1e-10);
});

test("known standards hide direct and transitive packages with descendants but preserve alternate paths", () => {
  const known = new Set(["B", "T1", "project"]);
  const view = visibleGraph(graph, { items: {}, updatedAt: null }, "transitive", false, null, { known });
  assert.deepEqual(ids(view), ["A", "C", "project"]);
  assert.equal(view.hidden, 0); assert.equal(view.standardHidden, 4);
  assert.equal(view.maxDepth, 1);
  const all = visibleGraph(graph, classification, "transitive", true, null, { known: known });
  assert.equal(all.nodes.length, graph.nodes.length); assert.equal(all.standardHidden, 0);
  const alternate = visibleGraph({ ...graph, edges: [...graph.edges, ["C", "T3"]] }, classification, "transitive", false, null, { known: new Set(["T1"]) });
  assert.ok(ids(alternate).includes("T3"));
  assert.equal(alternate.hidden, 2); assert.equal(alternate.standardHidden, 1);
  assert.deepEqual(pendingDirect(graph, classification, new Set(["C"])), []);
});

test("pink beats age red and every role, but never recolours root", () => {
  assert.equal(nodeColor(node("T"), false, 3, true), "#ff69b4");
  assert.equal(nodeColor(node("A", true), true, null, true), "#ff69b4");
  assert.equal(nodeColor(node("project"), false, null, true), "#4ea1ff");
  assert.equal(nodeColor(node("T"), false, 3, false), "#f85149");
  assert.equal(nodeColor(node("T"), false, 1, false), "#61708a");
});

test("hiding different-version packages removes what only they reach, counted apart from known standards", () => {
  const none = { items: {}, updatedAt: null };
  const base = visibleGraph(graph, none, "transitive", false, null, { different: new Set(["T2"]) });
  assert.deepEqual(ids(base), ["A", "B", "C", "T1", "project"]);
  assert.equal(base.differentHidden, 2); assert.equal(base.standardHidden, 0); assert.equal(base.hidden, 0);
  const both = visibleGraph(graph, none, "transitive", false, null, { known: new Set(["T1"]), different: new Set(["T2"]) });
  assert.deepEqual(ids(both), ["A", "B", "C", "project"]);
  assert.equal(both.standardHidden, 1); assert.equal(both.differentHidden, 2);
  const root = visibleGraph(graph, none, "transitive", false, null, { different: new Set(["project"]) });
  assert.equal(root.nodes.length, graph.nodes.length); assert.equal(root.differentHidden, 0);
  const all = visibleGraph(graph, none, "transitive", true, null, { different: new Set(["T2"]) });
  assert.equal(all.nodes.length, graph.nodes.length); assert.equal(all.differentHidden, 0);
  assert.equal(visibleGraph(graph, none, "transitive", false, null, { different: new Set() }).differentHidden, 0);
  assert.equal(visibleGraph(graph, none, "transitive", false).differentHidden, 0);
});

test("commonly used and known-owner sets hide direct and transitive packages and what only they reach, counted apart", () => {
  const none = { items: {}, updatedAt: null };
  const common = visibleGraph(graph, none, "transitive", false, null, { common: new Set(["B"]) });
  assert.deepEqual(ids(common), ["A", "C", "T1", "project"]);
  assert.equal(common.commonHidden, 3); assert.equal(common.ownersHidden, 0); assert.equal(common.standardHidden, 0); assert.equal(common.hidden, 0);
  const transitiveOnly = visibleGraph(graph, none, "transitive", false, null, { owners: new Set(["T2"]) });
  assert.deepEqual(ids(transitiveOnly), ["A", "B", "C", "T1", "project"]);
  assert.equal(transitiveOnly.ownersHidden, 2); assert.equal(transitiveOnly.commonHidden, 0);
  const both = visibleGraph(graph, none, "transitive", false, null, { known: new Set(["T1"]), common: new Set(["B"]), owners: new Set(["A"]) });
  assert.deepEqual(ids(both), ["C", "project"]);
  assert.deepEqual([both.standardHidden, both.commonHidden, both.ownersHidden], [1, 3, 1]);
  const direct = visibleGraph(graph, none, "direct", false, null, { common: new Set(["A"]), owners: new Set(["C"]) });
  assert.deepEqual(ids(direct), ["B", "project"]);
  assert.equal(visibleGraph(graph, none, "transitive", true, null, { common: new Set(["B"]), owners: new Set(["A"]) }).nodes.length, graph.nodes.length);
  const root = visibleGraph(graph, none, "transitive", false, null, { common: new Set(["project"]) });
  assert.equal(root.nodes.length, graph.nodes.length); assert.equal(root.commonHidden, 0);
  const alternate = visibleGraph({ ...graph, edges: [...graph.edges, ["C", "T3"]] }, none, "transitive", false, null, { common: new Set(["T2"]) });
  assert.ok(ids(alternate).includes("T3")); assert.equal(alternate.commonHidden, 1);
});

test("saved preferences default the two new filters off", () => {
  assert.equal(parseDependencyPrefs(null).hideCommon, false);
  assert.equal(parseDependencyPrefs(null).hideOwners, false);
  assert.equal(parseDependencyPrefs(JSON.stringify({ hideCommon: "yes", hideOwners: 1 })).hideCommon, false);
  const saved = parseDependencyPrefs(JSON.stringify({ hideCommon: true, hideOwners: true }));
  assert.deepEqual([saved.hideCommon, saved.hideOwners], [true, true]);
});

test("a path to a package can include its own descendants down to a layer, or all of them", () => {
  const chain = { nodes: ["project", "a", "b", "c", "d", "side"].map(id => node(id)), edges: [["project", "a"], ["a", "b"], ["b", "c"], ["c", "d"], ["project", "side"]], hidden: 0, standardHidden: 0, differentHidden: 0, commonHidden: 0, ownersHidden: 0, depthHidden: 0, maxDepth: 4 };
  const before = structuredClone(chain);
  const idsOf = view => view.nodes.map(n => n.id);
  assert.deepEqual(idsOf(pathToNode(chain, "a")), ["project", "a"]);
  assert.deepEqual(idsOf(pathToNode(chain, "a", undefined)), ["project", "a"]);
  assert.deepEqual(idsOf(pathToNode(chain, "a", 3)), ["project", "a", "b", "c"]);
  assert.deepEqual(idsOf(pathToNode(chain, "a", 2)), ["project", "a", "b"]);
  assert.deepEqual(idsOf(pathToNode(chain, "a", 1)), ["project", "a"]);
  assert.deepEqual(idsOf(pathToNode(chain, "a", null)), ["project", "a", "b", "c", "d"]);
  assert.deepEqual(pathToNode(chain, "a", null).edges.map(([f, t]) => `${f}>${t}`), ["project>a", "a>b", "b>c", "c>d"]);
  assert.equal(pathToNode(chain, "project", null), null);
  // A cycle back to the target neither loops forever nor drops the cycle members.
  assert.deepEqual(idsOf(pathToNode({ ...chain, edges: [...chain.edges, ["d", "a"]] }, "a", 1)), ["project", "a", "b", "c", "d"]);
  assert.deepEqual(chain, before);
});

test("package totals count unique scanned packages, exclude root and treat pink/unproven as not built in", () => {
  const raw = {
    ...graph,
    nodes: [...graph.nodes, { ...node("T1"), version: "2" }, node("pink"), node("derived"), node("known")],
    edges: [...graph.edges, ["B", "derived"]],
  };
  const totals = packageTotals(raw, classification, new Set(["known", "C", "project"]), new Set(["derived", "T1"]));
  // unique non-root: A B C T1 T2 T3 pink derived known = 9; built in: B (project basic), C and known (global), derived and T1.
  assert.deepEqual(totals, { total: 9, nonBuiltIn: 4, unclassified: 3 });
  const before = structuredClone(raw);
  packageTotals(raw, classification, new Set(), new Set());
  assert.deepEqual(raw, before);
  assert.deepEqual(packageTotals({ ...raw, nodes: [node("project")] }, classification, new Set(), new Set()), { total: 0, nonBuiltIn: 0, unclassified: 0 });
  const base = packageTotals(raw, { items: {}, updatedAt: null }, new Set(), new Set());
  assert.deepEqual(base, { total: 9, nonBuiltIn: 9, unclassified: 9 });
});

const eco = (id, name, ecosystem = "npm") => ({ ...node(id), name, ecosystem });
const searchNodes = [
  node("project"), eco("npm:react-dom", "react-dom"), eco("npm:preact", "preact"), eco("npm:React", "React"),
  eco("pypi:react", "react", "pypi"), eco("npm:isolated", "isolated"), eco("npm:@scope/react-x", "@scope/react-x"),
];

test("search ranks exact, prefix and substring literally, case-insensitively, with deterministic ties", () => {
  const index = buildPackageSearchIndex(searchNodes);
  assert.equal(index.length, searchNodes.length - 1);
  assert.ok(index.every(entry => entry.node.id !== ROOT));
  const before = structuredClone(searchNodes);
  assert.deepEqual(searchPackages(index, " REACT ").map(n => n.id), ["npm:React", "pypi:react", "npm:react-dom", "npm:@scope/react-x", "npm:preact"]);
  assert.deepEqual(searchPackages(index, "react").map(n => n.id), searchPackages(index, "React").map(n => n.id));
  assert.deepEqual(searchPackages(index, "npm:iso").map(n => n.id), ["npm:isolated"]);
  assert.deepEqual(searchPackages(index, "re.ct"), []);
  assert.deepEqual(searchPackages(index, "   "), []);
  assert.deepEqual(searchPackages(index, ""), []);
  assert.deepEqual(searchPackages(index, "project"), []);
  assert.deepEqual(searchNodes, before);
  assert.equal(searchPackages(index, "react")[0], searchNodes[3]);
});

test("exact name match picks the lowest id and ignores partial names", () => {
  const index = buildPackageSearchIndex(searchNodes);
  assert.equal(exactNameMatch(index, "REACT").id, "npm:React");
  assert.equal(exactNameMatch(index, "react-dom").id, "npm:react-dom");
  assert.equal(exactNameMatch(index, "react-d"), null);
  assert.equal(exactNameMatch(index, "  "), null);
});

test("saved preferences: layer 2 fallback, explicit All and numbers kept, standard removal defaults on", () => {
  const missing = parseDependencyPrefs(null);
  assert.equal(missing.layers, 2); assert.equal(missing.hideStandard, true); assert.equal(missing.hidePink, false); assert.equal(missing.mode, "direct");
  for (const raw of ["{}", "not json", "[]", "7", '{"layers":0}', '{"layers":-1}', '{"layers":1.5}', '{"layers":"3"}', '{"layers":9007199254740993}']) {
    assert.equal(parseDependencyPrefs(raw).layers, 2, raw);
  }
  assert.equal(parseDependencyPrefs('{"layers":null}').layers, null);
  assert.equal(parseDependencyPrefs('{"layers":5}').layers, 5);
  assert.equal(parseDependencyPrefs('{"hideStandard":false}').hideStandard, false);
  assert.equal(parseDependencyPrefs('{"hideStandard":"yes"}').hideStandard, true);
  assert.equal(parseDependencyPrefs('{"hidePink":true}').hidePink, true);
  const saved = parseDependencyPrefs('{"folder":"C:/p","mode":"transitive","showAll":true,"presentation":"list"}');
  assert.deepEqual([saved.folder, saved.mode, saved.showAll, saved.presentation], ["C:/p", "transitive", true, "list"]);
});
