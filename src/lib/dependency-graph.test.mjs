import test from "node:test";
import assert from "node:assert/strict";
import { visibleGraph, pendingDirect, nodeColor, nodeSize } from "./dependency-graph.ts";
import { projectFolders } from "./repo-agents.ts";

const node = (id, direct = false) => ({ id, name: id, version: null, ecosystem: id === "project" ? "project" : "npm", direct, dev: false });
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
