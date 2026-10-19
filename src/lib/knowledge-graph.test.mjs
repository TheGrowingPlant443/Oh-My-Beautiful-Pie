import test from "node:test";
import assert from "node:assert/strict";
import { importTs } from "./test-loader.mjs";

const graph = await importTs("system-graph");
const kg = await importTs("knowledge-graph");
const indexLib = await importTs("knowledge-index");
const { buildIndex, projectGraph, edgeCountsReconcile } = graph;
const { VaultModelBuilder, GraphifyModelBuilder, noteNodeId, graphifyNodeId } = kg;
const { IndexClient, IndexHost, INDEX_PROTOCOL_VERSION } = indexLib;

const note = (path, extra = {}) => ({ path, title: path.replace(/^.*\//, "").replace(/\.md$/, ""), bytes: 10, hash: "h", outgoing: 0, incoming: 0, ...extra });
const link = (id, source, target, extra = {}) => ({ id, source, target, kind: "wiki", status: target === null ? "missing" : "resolved", raw: target ?? "Missing", label: null, fragment: null, line: 1, candidates: [], ...extra });

test("note graph: every note is a node (isolated ones too), folders only contain, links are undirected and self-links stay on their note", () => {
  const builder = new VaultModelBuilder();
  builder.addNotes([note("a.md"), note("docs/b.md"), note("docs/deep/c.md"), note("lonely.md")]);
  builder.addLinks([
    link("l1", "a.md", "docs/b.md"), link("l2", "docs/b.md", "a.md"), link("l3", "a.md", "docs/b.md", { kind: "embed" }), link("l4", "a.md", "a.md"),
    link("l5", "a.md", null), link("l6", "a.md", null, { status: "ambiguous" }), link("l7", "a.md", null, { status: "external" }), link("l8", "a.md", "ghost.md"),
  ]);
  const model = builder.build();
  const notes = model.nodes.filter(item => !item.synthetic);
  assert.equal(notes.length, 4);
  assert.ok(notes.some(item => item.id === noteNodeId("lonely.md")), "an isolated note is still a node");
  assert.deepEqual(model.nodes.filter(item => item.synthetic).map(item => item.kind), ["folder", "folder"]);
  assert.ok(model.edges.every(item => item.direction === "undirected"), "notes relate mutually: no arrowheads");
  assert.deepEqual(model.edges.map(item => item.relation), ["link", "link", "embed", "link", "link"], "unresolved occurrences are not edges");
  assert.equal(builder.stats.links.missing, 1);
  assert.equal(builder.stats.links.ambiguous, 1);
  assert.equal(builder.stats.links.external, 1);
  assert.equal(builder.stats.selfLinks, 1);

  const index = buildIndex(model);
  assert.equal(index.totalNodes, 4);
  assert.equal(index.dangling.length, 1, "a link to a note that is not listed is set aside and counted, never invented as a node");
  const view = projectGraph(index, { mode: "source", expanded: null, loopKind: "selfLink" });
  assert.equal(view.loopKind, "selfLink");
  const a = view.nodes.find(item => item.id === noteNodeId("a.md"));
  assert.equal(a.selfLoops, 1);
  assert.deepEqual(a.selfLoopIds, ["l4"]);
  // l1 and l3 (parallel, different relation) and l2 (opposite) between a and b: one undirected line per relation, all ids kept.
  const between = view.edges.filter(item => item.source.endsWith("a.md") || item.target.endsWith("a.md"));
  assert.ok(between.every(item => item.direction === "undirected"));
  const ids = between.flatMap(item => item.edgeIds).sort();
  assert.deepEqual(ids, ["l1", "l2", "l3"]);
  assert.ok(edgeCountsReconcile(index, view));
});

test("a large note graph groups by folder and still reconciles every link", () => {
  const builder = new VaultModelBuilder();
  const notes = [];
  for (let f = 0; f < 30; f++) for (let n = 0; n < 30; n++) notes.push(note(`f${f}/n${n}.md`));
  builder.addNotes(notes);
  const links = [];
  for (let i = 0; i < notes.length - 1; i++) links.push(link(`k${i}`, notes[i].path, notes[i + 1].path));
  builder.addLinks(links);
  const index = buildIndex(builder.build());
  const view = projectGraph(index, { mode: "source", expanded: null });
  assert.ok(view.nodes.length <= 200, "the automatic cut fits the Flow bound");
  assert.equal(view.total.nodes, 900);
  assert.ok(edgeCountsReconcile(index, view));
  assert.ok(view.nodes.some(item => item.collapsed), "folders collapsed into cards");
});

const ref = (id, idKind = "text") => ({ id, idKind });
const gnode = (id, extra = {}) => ({ ref: ref(id), label: `Node ${id}`, fileType: "code", sourceFile: `src/${id}.py`, sourceLocation: "L1", community: null, origin: null, attributes: {}, ...extra });
const glink = (ordinal, source, target, extra = {}) => ({
  ordinal, id: `graphify:abc:link:${ordinal}`, nativeId: null, source, target, key: null, relation: "calls", directed: true, confidence: "EXTRACTED", confidenceScore: 1,
  sourceFile: "src/a.py", sourceLocation: "L3", origin: null, attributes: {}, dangling: false, ...extra,
});

test("Graphify: numeric and text ids stay distinct, directedness and confidence are per record, parallel edges and self-loops survive, hyperedges draw nothing", () => {
  const builder = new GraphifyModelBuilder();
  builder.addCommunities([{ id: 1, label: "Core", size: 2 }]);
  builder.addNodes([gnode("1", { community: 1 }), { ...gnode("1"), ref: ref("1", "number"), label: "Numeric one", community: 1 }, gnode("b"), gnode("dup"), gnode("dup")]);
  builder.addLinks([
    glink(0, ref("1"), ref("b")), glink(1, ref("1"), ref("b"), { key: "k2", confidence: "INFERRED" }), glink(2, ref("b"), ref("1"), { directed: false, confidence: "AMBIGUOUS" }),
    glink(3, ref("b"), ref("b")), glink(4, ref("1", "number"), ref("b"), { confidence: null }), glink(5, ref("b"), ref("nowhere"), { dangling: true }),
  ]);
  builder.addHyperedges([{ id: "h1", label: "Group", relation: null, nodeIds: [ref("1"), ref("b"), ref("dup")], confidence: "INFERRED", sourceFile: null, attributes: {} }]);
  const { model, stats } = builder.build();
  assert.equal(stats.nodes, 4, "a repeated id is one node; text '1' and numeric 1 are two");
  assert.equal(stats.links, 6);
  assert.equal(stats.dangling, 1);
  assert.deepEqual(stats.confidence, { EXTRACTED: 3, INFERRED: 1, AMBIGUOUS: 1, none: 1 });
  assert.equal(stats.hyperedges, 1);
  assert.equal(model.edges.length, 6, "a hyperedge adds no edges between its members");
  assert.ok(model.nodes.some(item => item.id === graphifyNodeId(ref("1", "number"))));
  assert.ok(model.nodes.some(item => item.id === graphifyNodeId(ref("1"))));
  assert.notEqual(graphifyNodeId(ref("1")), graphifyNodeId(ref("1", "number")));
  assert.equal(model.nodes.find(item => item.id === "community:1").synthetic, true);

  const index = buildIndex(model);
  assert.equal(index.dangling.length, 1);
  const view = projectGraph(index, { mode: "source", expanded: new Set(["community:1"]), loopKind: "selfLink" });
  const arrows = view.edges.filter(item => item.direction === "directed").flatMap(item => item.edgeIds);
  const plain = view.edges.filter(item => item.direction === "undirected").flatMap(item => item.edgeIds);
  assert.ok(arrows.includes("graphify:abc:link:0") && arrows.includes("graphify:abc:link:1"), "parallel directed edges with different confidence stay separate records");
  assert.deepEqual(plain, ["graphify:abc:link:2"]);
  assert.equal(view.nodes.find(item => item.id === graphifyNodeId(ref("b"))).selfLoops, 1);
  assert.ok(edgeCountsReconcile(index, view));
});

test("link-index host builds one job at a time, drops cancelled jobs and rejects the wrong protocol", () => {
  const replies = [];
  const host = new IndexHost(message => replies.push(message));
  host.handle({ type: "begin", version: INDEX_PROTOCOL_VERSION, jobId: 1, kind: "vault" });
  host.handle({ type: "chunk", version: INDEX_PROTOCOL_VERSION, jobId: 1, section: "notes", items: [note("a.md")] });
  host.handle({ type: "chunk", version: INDEX_PROTOCOL_VERSION, jobId: 1, section: "graphifyNodes", items: [] });
  assert.equal(replies.at(-1).type, "error", "a section of another kind is refused");
  host.handle({ type: "begin", version: INDEX_PROTOCOL_VERSION, jobId: 2, kind: "vault" });
  host.handle({ type: "chunk", version: INDEX_PROTOCOL_VERSION, jobId: 2, section: "notes", items: [note("a.md"), note("b.md")] });
  host.handle({ type: "cancel", version: INDEX_PROTOCOL_VERSION, jobId: 2 });
  host.handle({ type: "finish", version: INDEX_PROTOCOL_VERSION, jobId: 2 });
  assert.equal(replies.filter(item => item.type === "done").length, 0, "a cancelled job never produces a result");
  host.handle({ type: "begin", version: 99, jobId: 3, kind: "vault" });
  assert.match(replies.at(-1).message, /protocol 99/);
});

function connected() {
  const host = { current: null };
  const worker = {
    onmessage: null, onerror: null, terminated: false, posted: [],
    postMessage(message) {
      this.posted.push(message.type);
      queueMicrotask(() => host.current.handle(message));
    },
    terminate() { this.terminated = true; },
  };
  host.current = new IndexHost(message => queueMicrotask(() => worker.onmessage?.({ data: message })));
  return { worker, client: new IndexClient(() => worker) };
}

test("client: a finished job resolves with the model; a superseded job resolves null and is cancelled in the worker", async () => {
  const { worker, client } = connected();
  const first = client.begin("vault");
  first.push({ section: "notes", items: [note("a.md")] });
  const second = client.begin("vault");
  assert.equal(await first.finish(), null, "the first job was superseded by the second");
  assert.ok(worker.posted.includes("cancel"), "the worker was told to drop the first job");
  second.push({ section: "notes", items: [note("a.md"), note("b.md")] });
  second.push({ section: "links", items: [link("l", "a.md", "b.md")] });
  const result = await second.finish();
  assert.equal(result.kind, "vault");
  assert.equal(result.stats.notes, 2);
  assert.equal(result.model.edges.length, 1);
  const third = client.begin("graphify");
  third.cancel();
  assert.equal(await third.finish(), null);
});

test("client: a worker error rejects the job and a later job gets a fresh worker", async () => {
  let made = 0;
  let broken = null;
  const client = new IndexClient(() => {
    made++;
    const worker = { onmessage: null, onerror: null, postMessage() {}, terminate() {} };
    if (made === 1) broken = worker;
    return worker;
  });
  const job = client.begin("vault");
  const pending = job.finish();
  broken.onerror({ message: "boom" });
  await assert.rejects(pending, /boom/);
  client.begin("vault");
  assert.equal(made, 2);
});

test("exact neighborhood pins a node that a 5,000-node flat window or an oversized group would hide, in bounded pages", async () => {
  const { knowledgeNeighborhood, NEIGHBORHOOD_PAGE_RECORDS } = await importTs("knowledge-neighborhood");
  const nodes = Array.from({ length: 6000 }, (_, i) => ({ ...graph.node({ id: `n${String(i).padStart(5, "0")}`, label: `n${i}` }) }));
  const edges = [];
  for (let i = 1; i <= 450; i++) edges.push({ id: `e${i}`, source: "n05999", target: `n${String(i).padStart(5, "0")}`, direction: "directed", relation: "calls", grade: "EXTRACTED", derived: false, label: "calls", count: 1, evidenceIds: [], truncated: false });
  edges.push({ id: "loop", source: "n05999", target: "n05999", direction: "undirected", relation: "link", grade: "EXTRACTED", derived: false, label: "", count: 1, evidenceIds: [], truncated: false });
  const index = buildIndex({ nodes, edges });
  const flat = projectGraph(index, { mode: "source", expanded: null });
  assert.ok(!flat.nodes.some(item => item.id === "n05999"), "the full projection windows the node out");
  const first = knowledgeNeighborhood(index, "n05999", 0, "selfLink");
  assert.ok(first.projection.nodes.some(item => item.id === "n05999"), "the requested node is always displayed");
  assert.equal(first.records, 451);
  assert.equal(first.pages, 3);
  assert.ok(first.projection.edges.reduce((sum, item) => sum + item.count, 0) <= NEIGHBORHOOD_PAGE_RECORDS);
  const seen = new Set();
  for (let page = 0; page < first.pages; page++) for (const edge of knowledgeNeighborhood(index, "n05999", page, "selfLink").projection.edges.flatMap(item => item.edgeIds)) seen.add(edge);
  const loops = knowledgeNeighborhood(index, "n05999", 2, "selfLink").projection.nodes.find(item => item.id === "n05999").selfLoopIds;
  assert.equal(seen.size + loops.length, 451, "every original record is reachable exactly once");
  assert.equal(knowledgeNeighborhood(index, "missing", 0, "selfLink"), null);
});
