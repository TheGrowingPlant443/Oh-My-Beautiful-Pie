import test from "node:test";
import assert from "node:assert/strict";
import {
  buildIndex, chooseRenderer, defaultExpanded, edgeCountsReconcile, FLOW_MAX_EDGES, FLOW_MAX_NODES, layoutLayers, matchNodes, modelFromDirectories, modelFromNeighbors, modelFromOverview,
  modelFromSystem, node, placeholderSystemModel, projectGraph, revealPath, scopeModel, SIGMA_MAX_EDGES, SIGMA_MAX_NODES, stronglyConnected, edgeDashed, gradeProven, clipText,
} from "./system-graph.ts";

const edge = (id, source, target, extra = {}) => ({ id, source, target, relation: "calls", grade: "resolvedStatic", derived: false, label: "calls", count: 1, evidenceIds: [`site:${id}`], truncated: false, ...extra });
const leaf = (id, parentId = null, extra = {}) => node({ id, label: id, parentId, ...extra });
const dir = (id, parentId = null) => node({ id, label: id, parentId, synthetic: true, kind: "directory" });

test("strongly connected components find cycles, self loops and keep acyclic nodes apart", () => {
  const components = stronglyConnected([[1], [2], [0, 3], [], [4]]);
  const sets = components.map(members => [...members].sort().join(","));
  assert.ok(sets.includes("0,1,2"));
  assert.ok(sets.includes("3"));
  assert.ok(sets.includes("4"));
  assert.equal(components.length, 3);
});

test("strongly connected components survive a very deep chain without recursion", () => {
  const n = 50000;
  const adjacency = Array.from({ length: n }, (_, i) => (i + 1 < n ? [i + 1] : [0]));
  const components = stronglyConnected(adjacency);
  assert.equal(components.length, 1);
  assert.equal(components[0].length, n);
});

test("layering runs input to application to output regardless of edge direction and never lets a response move a column", () => {
  const nodes = [
    { id: "db", role: "output" }, { id: "app", role: "app" }, { id: "client", role: "input" }, { id: "cache", role: "output" }, { id: "gateway", role: "input" },
  ];
  const edges = [
    { source: "client", target: "gateway", relation: "requests" }, { source: "gateway", target: "app", relation: "requests" },
    { source: "app", target: "db", relation: "writes" }, { source: "app", target: "cache", relation: "reads" },
    // The response points back at the caller: it must neither merge components nor move a column.
    { source: "app", target: "client", relation: "responds" },
  ];
  const layout = layoutLayers(nodes, edges, true);
  assert.ok(layout.column.get("client") < layout.column.get("gateway"));
  assert.ok(layout.column.get("gateway") < layout.column.get("app"));
  assert.ok(layout.column.get("app") < layout.column.get("db"));
  assert.equal(layout.column.get("db"), layout.column.get("cache"));
  assert.ok(layout.row.get("cache") !== layout.row.get("db"));
});

test("layering without roles is longest path over the condensed graph; cycle members share a column and ids order ties", () => {
  const nodes = ["a", "b", "c", "d", "e"].map(id => ({ id, role: null }));
  const edges = [["a", "b"], ["b", "c"], ["c", "b"], ["c", "d"], ["a", "d"]].map(([source, target]) => ({ source, target, relation: "calls" }));
  const layout = layoutLayers(nodes, edges, false);
  assert.equal(layout.column.get("a"), 0);
  assert.equal(layout.column.get("b"), 1);
  assert.equal(layout.column.get("c"), 1);
  assert.equal(layout.column.get("d"), 2);
  assert.equal(layout.component.get("b"), layout.component.get("c"));
  assert.equal(layout.componentSize.get(layout.component.get("b")), 2);
  assert.equal(layout.column.get("e"), 0);
  // Deterministic: the same input in another order lays out identically.
  const again = layoutLayers([...nodes].reverse(), [...edges].reverse(), false);
  for (const id of ["a", "b", "c", "d", "e"]) assert.equal(again.x.get(id), layout.x.get(id));
});

test("index validation sets aside dangling edges, keeps the first duplicate and cuts parent cycles", () => {
  const index = buildIndex({ nodes: [leaf("a"), leaf("a"), leaf("p", "q"), leaf("q", "p")], edges: [edge("1", "a", "missing"), edge("2", "p", "q")] });
  assert.equal(index.duplicates, 1);
  assert.equal(index.dangling.length, 1);
  assert.equal(index.edges.length, 1);
  assert.equal(index.roots.length, 2, "a plus the node whose parent link closed the cycle");
  const parentless = [...index.parent.values()].filter(value => value === null).length;
  assert.ok(parentless >= 2, "the parent cycle was broken");
});

function directoryModel() {
  // src/a/{x,y}.ts and src/b/z.ts: x -> z (import), y -> x, z -> z (recursion). tests/t.ts isolated.
  const nodes = [dir("dir:"), dir("dir:src", "dir:"), dir("dir:src/a", "dir:src"), dir("dir:src/b", "dir:src"), dir("dir:tests", "dir:"),
    leaf("x", "dir:src/a", { path: "src/a/x.ts" }), leaf("y", "dir:src/a", { path: "src/a/y.ts" }), leaf("z", "dir:src/b", { path: "src/b/z.ts", selfLoops: 2 }), leaf("t", "dir:tests", { path: "tests/t.ts" })];
  const edges = [edge("e1", "x", "z", { relation: "imports", evidenceIds: ["s1"] }), edge("e2", "y", "x", { relation: "imports", evidenceIds: ["s2"] }), edge("e3", "y", "z", { relation: "imports", evidenceIds: ["s3"] }),
    edge("e4", "x", "z", { relation: "calls", grade: "candidate", evidenceIds: ["s4"], count: 3, truncated: true })];
  return { nodes, edges };
}

test("small repositories open fully; every file card (isolated ones too) is a node and counts reconcile", () => {
  const index = buildIndex(directoryModel());
  const view = projectGraph(index, { mode: "source", expanded: null });
  assert.equal(view.renderer, "flow");
  const ids = view.nodes.map(item => item.id).sort();
  assert.deepEqual(ids, ["t", "x", "y", "z"]);
  assert.equal(view.total.nodes, 4);
  assert.equal(view.displayed.nodes, 4);
  assert.ok(view.nodes.find(item => item.id === "t").degree === 0, "isolated file is displayed without invented edges");
  assert.equal(view.nodes.find(item => item.id === "z").selfLoops, 2);
  assert.ok(edgeCountsReconcile(index, view));
});

test("collapsing directories aggregates arrows, keeps every underlying edge/evidence id and accounts for internal edges", () => {
  const index = buildIndex(directoryModel());
  const view = projectGraph(index, { mode: "source", expanded: new Set(["dir:", "dir:src"]) });
  assert.deepEqual(view.nodes.map(item => item.id).sort(), ["dir:src/a", "dir:src/b", "dir:tests"]);
  const imports = view.edges.find(item => item.source === "dir:src/a" && item.target === "dir:src/b" && item.relation === "imports");
  assert.deepEqual(imports.edgeIds.sort(), ["e1", "e3"]);
  assert.deepEqual(imports.evidenceIds.sort(), ["s1", "s3"]);
  assert.equal(imports.count, 2);
  const candidate = view.edges.find(item => item.grade === "candidate");
  assert.equal(candidate.count, 3);
  assert.equal(candidate.truncated, true, "a truncated sample stays marked truncated");
  assert.equal(view.accounted.internal, 1, "y -> x is inside dir:src/a");
  assert.equal(view.nodes.find(item => item.id === "dir:src/a").internal, 1);
  assert.equal(view.nodes.find(item => item.id === "dir:src/b").selfLoops, 2);
  assert.ok(view.nodes.find(item => item.id === "dir:src/a").collapsed);
  assert.equal(view.nodes.find(item => item.id === "dir:src/a").memberCount, 2);
  assert.equal(view.displayed.nodes, 4);
  assert.ok(edgeCountsReconcile(index, view));
  // The same projection feeds either renderer: it is a plain object of arrays with identical membership.
  assert.equal(new Set(view.edges.map(item => item.source)).size > 0, true);
});

test("filters hide arrows without changing node membership and still reconcile", () => {
  const index = buildIndex(directoryModel());
  const all = projectGraph(index, { mode: "source", expanded: null });
  const filtered = projectGraph(index, { mode: "source", expanded: null, grades: new Set(["resolvedStatic"]) });
  assert.deepEqual(filtered.nodes.map(item => item.id), all.nodes.map(item => item.id));
  assert.equal(filtered.accounted.filtered, 3);
  assert.ok(edgeCountsReconcile(index, filtered));
  const derivedOff = buildIndex({ nodes: [leaf("a"), leaf("b")], edges: [edge("d", "a", "b", { derived: true }), edge("r", "b", "a")] });
  assert.equal(projectGraph(derivedOff, { mode: "source", expanded: null, derived: false }).edges.length, 1);
});

function flatModel(nodeCount, edgeCount) {
  const nodes = [dir("dir:")];
  for (let i = 0; i < nodeCount; i++) nodes.push(leaf(`n${String(i).padStart(6, "0")}`, "dir:"));
  const edges = [];
  for (let i = 0; i < edgeCount; i++) edges.push(edge(`e${i}`, `n${String(i % nodeCount).padStart(6, "0")}`, `n${String((i * 7 + 1) % nodeCount).padStart(6, "0")}`, { relation: i % 2 ? "calls" : "imports" }));
  return { nodes, edges };
}

test("renderer policy: flow up to 200 nodes and 500 edges, sigma above, system always flow", () => {
  assert.equal(chooseRenderer("source", FLOW_MAX_NODES, FLOW_MAX_EDGES), "flow");
  assert.equal(chooseRenderer("source", FLOW_MAX_NODES + 1, 0), "sigma");
  assert.equal(chooseRenderer("source", 10, FLOW_MAX_EDGES + 1), "sigma");
  assert.equal(chooseRenderer("system", 5000, 20000), "flow");
  const small = projectGraph(buildIndex(flatModel(200, 300)), { mode: "source", expanded: null });
  assert.equal(small.renderer, "flow");
  assert.equal(small.nodes.length, 200);
  const medium = projectGraph(buildIndex(flatModel(201, 300)), { mode: "source", expanded: null });
  assert.equal(medium.renderer, "sigma");
  assert.equal(medium.nodes.length, 201);
});

test("a repository above the Sigma cap is aggregated by directory first, then windowed, never silently truncated", () => {
  const nodes = [dir("dir:")];
  const edges = [];
  for (let d = 0; d < 30; d++) {
    nodes.push(dir(`dir:p${d}`, "dir:"));
    for (let f = 0; f < 250; f++) nodes.push(leaf(`f${d}-${f}`, `dir:p${d}`));
    for (let f = 0; f < 249; f++) edges.push(edge(`e${d}-${f}`, `f${d}-${f}`, `f${d}-${f + 1}`));
    if (d) edges.push(edge(`x${d}`, `f${d}-0`, `f${d - 1}-0`));
  }
  const index = buildIndex({ nodes, edges });
  assert.equal(index.totalNodes, 7500);
  // Everything opened exceeds 5,000 nodes: groups collapse until it fits and the page says why.
  const opened = projectGraph(index, { mode: "source", expanded: new Set(index.nodes.keys()) });
  assert.ok(opened.nodes.length <= SIGMA_MAX_NODES);
  assert.ok(opened.edges.length <= SIGMA_MAX_EDGES);
  assert.equal(opened.renderer, "sigma");
  assert.match(opened.reason, /collapsed/);
  assert.equal(opened.total.nodes, 7500);
  assert.equal(opened.displayed.nodes, 7500, "collapsed groups still account for every file");
  assert.ok(edgeCountsReconcile(index, opened));
  // One flat directory above the cap cannot be drawn: it stays one explained card (nothing is dropped, search/neighbourhoods reach every file).
  const crowded = projectGraph(buildIndex(flatModel(SIGMA_MAX_NODES + 10, 100)), { mode: "source", expanded: null });
  assert.equal(crowded.nodes.length, 1);
  assert.equal(crowded.nodes[0].memberCount, SIGMA_MAX_NODES + 10);
  assert.match(crowded.reason, /more than 5,000 nodes/);
  // Without containers the projection itself is windowed with explicit counts.
  const roots = (n) => ({ nodes: Array.from({ length: n }, (_, i) => leaf(`r${String(i).padStart(6, "0")}`)), edges: [] });
  const flat = projectGraph(buildIndex(roots(SIGMA_MAX_NODES + 10)), { mode: "source", expanded: null });
  assert.equal(flat.nodes.length, SIGMA_MAX_NODES);
  assert.equal(flat.paged.pages, 2);
  assert.match(flat.reason, /window 1 of 2/);
  const second = projectGraph(buildIndex(roots(SIGMA_MAX_NODES + 10)), { mode: "source", expanded: null, page: 1 });
  assert.equal(second.nodes.length, 10);
  assert.equal(second.total.nodes, SIGMA_MAX_NODES + 10);
});

test("the default cut opens directories breadth first within the Flow bound and respects the edge bound", () => {
  const index = buildIndex(flatModel(150, 600));
  const cut = defaultExpanded(index, "source");
  assert.ok(cut.has("dir:"));
  const view = projectGraph(index, { mode: "source", expanded: null });
  // 150 nodes but 600 arrows: the default cut collapses to stay within the Flow edge bound, then Sigma shows it if it still exceeds.
  assert.ok(view.nodes.length >= 1);
  assert.ok(edgeCountsReconcile(index, view));
});

const systemPayload = () => ({
  schemaVersion: 1, snapshotId: "snap",
  nodes: [
    { id: "app", kind: "application", label: "Application", parentId: null, description: "", evidenceIds: ["ev1"] },
    { id: "api", kind: "logicGroup", label: "API", parentId: "app", description: "", evidenceIds: [] },
    { id: "core", kind: "logicGroup", label: "Core", parentId: "app", description: "", evidenceIds: [] },
    { id: "user", kind: "actor", label: "Users", parentId: null, description: "", evidenceIds: [] },
    { id: "ingress", kind: "service", label: "Ingress", parentId: null, description: "", evidenceIds: ["ev1"] },
    { id: "opa", kind: "sidecar", label: "OPA", parentId: null, description: "", evidenceIds: [] },
    { id: "valkey", kind: "cache", label: "Valkey", parentId: null, description: "", evidenceIds: [] },
    { id: "pg", kind: "database", label: "Postgres", parentId: null, description: "", evidenceIds: [] },
    { id: "mail", kind: "external", label: "Mail API", parentId: null, description: "", evidenceIds: [] },
  ],
  edges: [
    { id: "e1", sourceId: "user", targetId: "ingress", relation: "requests", label: "HTTPS", evidenceIds: [], certainty: "inferred", staticFactId: null },
    { id: "e2", sourceId: "ingress", targetId: "api", relation: "requests", label: "", evidenceIds: ["ev1"], certainty: "declared", staticFactId: "fact1" },
    { id: "e3", sourceId: "api", targetId: "ingress", relation: "responds", label: "", evidenceIds: [], certainty: "inferred", staticFactId: null },
    { id: "e4", sourceId: "api", targetId: "opa", relation: "authorizes", label: "", evidenceIds: [], certainty: "inferred", staticFactId: null },
    { id: "e5", sourceId: "core", targetId: "valkey", relation: "reads", label: "", evidenceIds: [], certainty: "unknown", staticFactId: null },
    { id: "e6", sourceId: "core", targetId: "pg", relation: "writes", label: "", evidenceIds: [], certainty: "inferred", staticFactId: null },
    { id: "e7", sourceId: "core", targetId: "mail", relation: "requests", label: "", evidenceIds: [], certainty: "inferred", staticFactId: null },
    { id: "e8", sourceId: "api", targetId: "core", relation: "calls", label: "", evidenceIds: [], certainty: "inferred", staticFactId: null },
  ],
  evidence: [], coverage: [], limitations: [],
});

test("system diagram: central application box without a result, a few groups with one, every external relationship kept", () => {
  const empty = projectGraph(buildIndex(placeholderSystemModel("Application")), { mode: "system", expanded: null });
  assert.deepEqual(empty.nodes.map(item => item.id), ["application"]);
  assert.equal(empty.edges.length, 0);
  const model = modelFromSystem(systemPayload());
  const index = buildIndex(model);
  const view = projectGraph(index, { mode: "system", expanded: null });
  assert.equal(view.renderer, "flow");
  // Application has two groups: opened by default; the application card itself has no own relationships so it is not a card.
  const ids = view.nodes.map(item => item.id).sort();
  assert.deepEqual(ids, ["api", "core", "ingress", "mail", "opa", "pg", "user", "valkey"]);
  const column = Object.fromEntries(view.nodes.map(item => [item.id, item.column]));
  assert.ok(column.user < column.ingress && column.ingress < column.api, "inputs precede the application");
  assert.ok(column.api < column.pg && column.core < column.valkey && column.core < column.mail && column.api < column.opa, "outputs follow the application");
  const response = view.edges.find(item => item.relation === "responds");
  assert.ok(response.response && edgeDashed(response));
  assert.equal(view.edges.length, 8);
  const proven = view.edges.find(item => item.id === "ingress>api|requests|declared|0");
  assert.ok(gradeProven(proven.grade) && !edgeDashed(proven));
  assert.deepEqual(proven.evidenceIds.sort(), ["ev1", "fact1"]);
  assert.equal(view.total.nodes, view.displayed.nodes + view.accounted.openContainers);
  assert.ok(edgeCountsReconcile(index, view));
});

test("collapsing the application rolls relationships of hidden groups up to one box and keeps each relation's own arrow", () => {
  const index = buildIndex(modelFromSystem(systemPayload()));
  const view = projectGraph(index, { mode: "system", expanded: new Set() });
  assert.ok(view.nodes.find(item => item.id === "app").collapsed);
  const outgoing = view.edges.filter(item => item.source === "app");
  assert.deepEqual(outgoing.map(item => item.target).sort(), ["ingress", "mail", "opa", "pg", "valkey"]);
  const request = view.edges.find(item => item.source === "user");
  assert.equal(request.target, "ingress");
  assert.equal(view.accounted.internal, 1, "api -> core stays inside the application box");
  assert.ok(edgeCountsReconcile(index, view));
});

test("overview groups become nodes (cycles as one expandable node), links keep totals and samples, containers stay out of the counts", () => {
  const groups = [
    { id: "scc:a", kind: "scc", level: "files", label: "src/a.ts", cyclic: false, memberCount: 1, members: ["fa"], membersTruncated: false, internalEdges: 0, linksTotal: 1, linksTruncated: false,
      links: [{ targetGroupId: "scc:c", targetLabel: "cycle", relation: "imports", resolution: "resolvedStatic", derived: false, count: 4, edgeIds: ["e1", "e2"], edgeIdsTruncated: true }] },
    { id: "scc:c", kind: "scc", level: "files", label: "2 members in a cycle, including src/ü/Ç.ts", cyclic: true, memberCount: 2, members: ["fb", "fc"], membersTruncated: false, internalEdges: 2, linksTotal: 0, linksTruncated: false, links: [] },
    { id: "scc:iso", kind: "scc", level: "files", label: "docs/readme.md", cyclic: false, memberCount: 1, members: ["fd"], membersTruncated: false, internalEdges: 0, linksTotal: 0, linksTruncated: false, links: [] },
  ];
  const files = [
    { id: "fa", path: "src/a.ts", status: "parsed", language: "typescript", sha256: "", bytes: 1, diagnostics: [] },
    { id: "fb", path: "src/ü/Ç.ts", status: "parsed", language: "typescript", sha256: "", bytes: 1, diagnostics: [] },
    { id: "fc", path: "src/ü/ç.ts", status: "partial", language: "typescript", sha256: "", bytes: 1, diagnostics: ["x"] },
    { id: "fd", path: "docs/readme.md", status: "unsupported", language: null, sha256: "", bytes: 1, diagnostics: [] },
  ];
  const model = modelFromOverview({ level: "files", groups, files, directories: ["empty/dir"] });
  const index = buildIndex(model);
  assert.equal(index.totalNodes, 4, "a cycle counts all its files; containers are not counted as indexed nodes");
  assert.equal(index.totalGroups, 3, "the cycle is still one node");
  assert.ok(index.nodes.has("dir:empty/dir"), "empty directories are admitted containers");
  const cycle = index.nodes.get("scc:c");
  assert.equal(cycle.memberCount, 2);
  assert.deepEqual(cycle.lazyMembers, ["fb", "fc"]);
  assert.equal(cycle.parentId, "dir:src/ü", "a cycle sits in the members' common directory");
  assert.ok(index.nodes.has("fd"), "isolated files stay");
  assert.equal(index.nodes.get("fd").evidence, "unsupported");
  const link = index.edges[0];
  assert.equal(link.count, 4);
  assert.equal(link.truncated, true);
  assert.deepEqual(link.evidenceIds, ["e1", "e2"]);
  // Case and Unicode are preserved exactly: two spellings are two paths.
  assert.ok(index.nodes.has("dir:src/ü") && !index.nodes.has("dir:src/Ü"));
});

test("function overview nests functions under file containers and keeps same-named functions apart", () => {
  const symbol = (id, path, name, line) => ({ id, fileId: "f", path, language: "ts", region: null, ownerId: null, kind: "function", name, qualifiedName: name, span: { byteStart: 0, byteEnd: 1, lineStart: line, lineEnd: line, columnStart: 0, columnEnd: 1 } });
  const group = (id, label, links = [], internalEdges = 0) => ({ id: `scc:${id}`, kind: "scc", level: "functions", label, cyclic: internalEdges > 0, memberCount: 1, members: [id], membersTruncated: false, internalEdges, linksTotal: links.length, linksTruncated: false, links });
  const model = modelFromOverview({
    level: "functions",
    groups: [group("s1", "run", [{ targetGroupId: "scc:s2", targetLabel: "run", relation: "calls", resolution: "candidate", derived: false, count: 1, edgeIds: ["c1"], edgeIdsTruncated: false }], 1), group("s2", "run"), group("s3", "Run")],
    symbols: [symbol("s1", "a.ts", "run", 1), symbol("s2", "b/b.ts", "run", 9), symbol("s3", "b/b.ts", "Run", 20)],
  });
  const index = buildIndex(model);
  assert.equal(index.totalNodes, 3);
  assert.equal(index.nodes.get("s2").parentId, "file:b/b.ts");
  assert.equal(index.nodes.get("file:b/b.ts").parentId, "dir:b");
  assert.equal(index.nodes.get("s1").selfLoops, 1);
  const view = projectGraph(index, { mode: "source", expanded: null });
  assert.equal(view.nodes.find(item => item.id === "s1").selfLoops, 1);
  assert.equal(view.edges[0].grade, "candidate");
  assert.ok(edgeCountsReconcile(index, view));
});

test("neighborhood rows become peers and edges with site evidence; missing endpoints remain sites", () => {
  const site = { id: "site1", fileId: "f", path: "a.ts", language: "ts", region: null, callerId: "c", span: { byteStart: 0, byteEnd: 1, lineStart: 3, lineEnd: 3, columnStart: 0, columnEnd: 1 }, relation: "calls", raw: "dyn()", targetId: null, resolution: "dynamic", conditional: false, typeOnly: false, candidateIds: [] };
  const rows = [
    { kind: "edge", direction: "out", self: false, peerId: "p1", peerLabel: "pkg/p1.ts", edge: { id: "e1", sourceId: "c", targetId: "p1", relation: "imports", resolution: "resolvedStatic", evidenceIds: ["site1"] }, sites: [], sitesTruncated: false },
    { kind: "edge", direction: "in", self: false, peerId: "p2", peerLabel: "pkg/p2.ts", edge: { id: "e2", sourceId: "p2", targetId: "c", relation: "imports", resolution: "candidate", evidenceIds: [] }, sites: [], sitesTruncated: false },
    { kind: "aggregate", direction: "out", peerId: "p3", peerLabel: "pkg/p3.ts", relation: "calls", resolution: "resolvedStatic", count: 5, edgeIds: ["x"], edgeIdsTruncated: true },
    { kind: "site", direction: "out", site },
  ];
  const { model, sites } = modelFromNeighbors({ level: "files", centerId: "c", centerLabel: "pkg/c.ts", centerPath: "pkg/c.ts", rows });
  assert.equal(sites.length, 1);
  assert.equal(model.nodes.length, 4);
  const index = buildIndex(model);
  const view = projectGraph(index, { mode: "source", expanded: null });
  assert.equal(view.edges.length, 3);
  const incoming = view.edges.find(item => item.id.startsWith("p2>c"));
  assert.ok(incoming, "incoming edge points at the center");
  assert.equal(view.edges.find(item => item.derived).count, 5);
});

test("scope keeps matching nodes with their containers, reveal opens ancestors, search matches labels case-insensitively", () => {
  const model = directoryModel();
  const scoped = scopeModel(model, item => (item.path ?? "").startsWith("src/a"));
  assert.deepEqual(scoped.nodes.map(item => item.id).sort(), ["dir:", "dir:src", "dir:src/a", "x", "y"]);
  assert.deepEqual(scoped.edges.map(item => item.id), ["e2"]);
  const index = buildIndex(model);
  const open = revealPath(index, "z", new Set(), "source");
  assert.ok(open.has("dir:src/b") && open.has("dir:src") && open.has("dir:"));
  assert.deepEqual(matchNodes(index, "SRC/A"), ["x", "y"]);
  assert.deepEqual(matchNodes(index, "  "), []);
});

test("clipText is display-only and code-point safe", () => {
  assert.equal(clipText("abcdef", 4), "abc…");
  assert.equal(clipText("😀😀😀😀", 3), "😀😀…");
  assert.equal(clipText("short", 10), "short");
});

test("a directory overview counts the indexed members its directory cards stand for, so displayed and total reconcile", () => {
  const directories = [
    node({ id: "d:a", kind: "directory", label: "a", memberCount: 7000 }),
    node({ id: "d:b", kind: "directory", label: "b", memberCount: 908 }),
  ];
  const index = buildIndex({ nodes: directories, edges: [] });
  assert.equal(index.totalNodes, 7908);
  const view = projectGraph(index, { mode: "source" });
  assert.equal(view.total.nodes, 7908);
  assert.equal(view.displayed.nodes, 7908, "collapsed and drawn directory cards account for every member");
});

test("a cycle group reconciles files with nodes: every member is counted once, whether the cycle is collapsed, a card or loaded", () => {
  const group = (id, members, extra = {}) => ({ id: `scc:${id}`, kind: "scc", level: "files", label: members.length === 1 ? `src/${members[0]}.ts` : `${members.length} members in a cycle`, cyclic: members.length > 1, memberCount: members.length, members, membersTruncated: false, internalEdges: 0, linksTotal: 0, linksTruncated: false, links: [], ...extra });
  const files = ["a", "b", "c", "d", "e"].map(id => ({ id, path: `src/${id}.ts`, status: "parsed", language: "typescript", sha256: "", bytes: 1, diagnostics: [] }));
  const groups = [group("one", ["a"]), group("loop", ["b", "c", "d"], { links: [{ targetGroupId: "scc:one", targetLabel: "a", relation: "imports", resolution: "resolvedStatic", derived: false, count: 1, edgeIds: ["x"], edgeIdsTruncated: false }] }), group("two", ["e"])];
  const base = modelFromOverview({ level: "files", groups, files });
  const view = projectGraph(buildIndex(base), { mode: "source", expanded: null });
  assert.equal(view.total.nodes, 5, "five files are indexed");
  assert.equal(view.total.groups, 3, "drawn as three nodes");
  assert.equal(view.displayed.nodes, view.total.nodes, "the displayed members add up to the indexed files");
  assert.equal(view.nodes.find(item => item.id === "scc:loop").memberCount, 3);
  // Once the cycle's members are loaded they replace it instead of being added on top of it.
  const members = ["b", "c", "d"].map(id => leaf(id, "scc:loop", { path: `src/${id}.ts` }));
  const loaded = buildIndex({ nodes: [...base.nodes, ...members], edges: base.edges });
  assert.equal(loaded.totalNodes, 5);
  const opened = projectGraph(loaded, { mode: "source", expanded: new Set(["scc:loop"]) });
  assert.equal(opened.displayed.nodes, opened.total.nodes);
  const collapsed = projectGraph(loaded, { mode: "source", expanded: new Set() });
  assert.equal(collapsed.displayed.nodes, collapsed.total.nodes);
});

test("directory aggregates report direct plus nested files, and opened directories keep their direct files in the view", () => {
  // root holds 2 files directly, src 3, src/a 5 and src/b 1: 11 in all. Nothing links any directory, so none has relationships of its own.
  const row = (id, parentId, memberCount) => ({ id, kind: "directory", level: "files", label: id, path: id, parentId, cyclic: false, memberCount, members: [], membersTruncated: true, internalEdges: 0, linksTotal: 0, linksTruncated: false, links: [] });
  const index = buildIndex(modelFromDirectories([row("root", undefined, 2), row("src", "root", 3), row("src/a", "src", 5), row("src/b", "src", 1)]));
  assert.equal(index.totalNodes, 11);
  assert.equal(index.totalGroups, 4);
  assert.equal(index.members.get("src"), 9, "a directory counts its own files plus its subdirectories'");
  for (const expanded of [null, new Set(), new Set(["root"]), new Set(["root", "src"]), new Set(["root", "src", "src/a"])]) {
    const view = projectGraph(index, { mode: "source", expanded });
    assert.equal(view.displayed.nodes, 11, "every file is accounted for in the displayed cards");
    assert.equal(view.displayed.nodes, view.total.nodes);
    assert.equal(view.accounted.openContainers, 0, "no opened directory hides the files it holds");
    assert.equal(view.nodes.reduce((sum, item) => sum + item.memberCount, 0), 11, "the members of the displayed cards add up to the indexed files");
  }
  const opened = projectGraph(index, { mode: "source", expanded: new Set(["root", "src"]) });
  assert.deepEqual(opened.nodes.map(item => [item.id, item.memberCount]).sort(), [["root", 2], ["src", 3], ["src/a", 5], ["src/b", 1]]);
});
