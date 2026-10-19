import test from "node:test";
import assert from "node:assert/strict";
import { importTs } from "./test-loader.mjs";

const helicopter = await importTs("helicopter-graph");
const graph = await importTs("system-graph");
const { modelFromHelicopter, helicopterDefaultExpanded, revealNodes, journeyRows, capabilityRows, evidenceFor, helicopterSummary, needsClarification, HELICOPTER_KIND_WORDS } = helicopter;
const { buildIndex, projectGraph } = graph;

const evidenceRecord = (id, path = "src/app.ts") => ({ id, path, lineStart: 1, lineEnd: 3, contentHash: "h".repeat(64) });
const base = (nodes, edges = [], capabilities = []) => ({
  kind: "helicopter", schemaVersion: 1, snapshotId: "snap", purpose: "Lets customers book visits.", nodes, edges, capabilities,
  evidence: [evidenceRecord("e1"), evidenceRecord("e2", "docker-compose.yml")], coverage: [], limitations: [],
});
const node = (id, kind, label, extra = {}) => ({ id, kind, label, parentId: null, description: "", evidenceIds: kind === "businessGroup" ? [] : ["e1"], ...extra });
const edge = (id, sourceId, targetId, extra = {}) => ({ id, sourceId, targetId, label: "", evidenceIds: ["e1"], certainty: "inferred", ...extra });
const project = (model, expanded) => projectGraph(buildIndex(model), { mode: "system", expanded: expanded === null ? null : new Set(expanded) });
const visible = projection => projection.nodes.map(item => item.id);

const appointment = () => base(
  [
    node("customer", "actor", "Customer"),
    node("asks", "trigger", "Customer asks for an appointment", { parentId: "booking" }),
    node("booking", "businessGroup", "Booking an appointment"),
    node("free", "decision", "Is a slot free?", { parentId: "booking" }),
    node("confirm", "activity", "Clinic confirms the visit", { parentId: "booking" }),
    node("cancelled", "outcome", "Appointment is cancelled", { description: "Needs clarification: whether a fee applies", evidenceIds: ["e2"] }),
    node("pay", "externalParty", "Payment provider", { evidenceIds: ["e2"] }),
  ],
  [
    edge("h1", "customer", "asks"), edge("h2", "asks", "free"), edge("h3", "free", "confirm", { label: "Yes" }),
    edge("h4", "free", "cancelled", { label: "No", certainty: "unknown", evidenceIds: [] }), edge("h5", "confirm", "pay", { label: "Deposit is taken", evidenceIds: ["e2"] }),
  ],
  [
    { id: "cap-book", label: "Book a visit", description: "", nodeIds: ["asks", "free", "confirm"], evidenceIds: ["e1"] },
    { id: "cap-cancel", label: "Cancel a visit", description: "Needs clarification: fees", nodeIds: ["cancelled", "ghost"], evidenceIds: ["e2", "missing"] },
  ],
);

test("the business model is a directed flow in plain words: every arrow directed, roles by business meaning, uncertainty visible", () => {
  const model = modelFromHelicopter(appointment());
  assert.deepEqual(model.nodes.map(item => item.kind), ["Who", "Starts when", "Journey", "Decision", "Step", "Result", "Outside party"]);
  assert.ok(model.edges.every(item => item.direction === "directed" && item.derived === false && item.count === 1));
  assert.deepEqual(Object.fromEntries(model.nodes.map(item => [item.id, item.role])), {
    customer: "input", asks: "input", booking: "app", free: "app", confirm: "app", cancelled: "output", pay: "output",
  });
  // Arrow words are the business labels; an unsettled arrow says so on the arrow itself; the structured label and evidence stay on the record.
  assert.deepEqual(model.edges.map(item => item.relation), ["then", "then", "Yes", "No (needs clarification)", "Deposit is taken"]);
  assert.equal(model.edges[3].grade, "unknown");
  assert.equal(model.edges[3].label, "No");
  assert.deepEqual(model.edges[4].evidenceIds, ["e2"]);
  assert.equal(model.nodes.find(item => item.id === "cancelled").subtitle, "Needs clarification");
  assert.equal(model.nodes.find(item => item.id === "confirm").subtitle, "");
  assert.ok(model.nodes.every(item => item.evidence === "analysis" && item.synthetic === false));
  assert.equal(HELICOPTER_KIND_WORDS.businessGroup, "Journey");
  assert.equal(model.nodes.find(item => item.id === "booking").memberCount, 0, "a journey is a container, not an item");
});

test("an outside party that starts a journey is an input; one that only receives is an output", () => {
  const payload = base(
    [node("webhook", "externalParty", "Booking website"), node("step", "activity", "Clinic records the request"), node("bank", "externalParty", "Bank")],
    [edge("a", "webhook", "step"), edge("b", "step", "bank")],
  );
  const roles = Object.fromEntries(modelFromHelicopter(payload).nodes.map(item => [item.id, item.role]));
  assert.deepEqual(roles, { webhook: "input", step: "app", bank: "output" });
});

test("the projection draws journeys as containers and never turns containment into an arrow", () => {
  const model = modelFromHelicopter(appointment());
  const closed = project(model, []);
  assert.deepEqual(visible(closed).sort(), ["booking", "cancelled", "customer", "pay"]);
  const journey = closed.nodes.find(item => item.id === "booking");
  assert.equal(journey.collapsed, true);
  assert.equal(journey.memberCount, 3);
  assert.ok(closed.edges.every(item => item.direction === "directed"));
  assert.ok(closed.edges.every(item => item.source !== item.target));
  const opened = project(model, ["booking"]);
  assert.deepEqual(visible(opened).sort(), ["asks", "cancelled", "confirm", "customer", "free", "pay"]);
  assert.equal(opened.total.edges, 5, "every flow is accounted for however the journeys are opened");
});

test("a small picture opens its journeys; a large one shows a few journey cards and every step stays one click away", () => {
  const small = modelFromHelicopter(appointment());
  assert.deepEqual(helicopterDefaultExpanded(small), ["booking"]);
  const nodes = [node("customer", "actor", "Customer")];
  const edges = [];
  for (const journey of ["a", "b", "c"]) {
    nodes.push(node(`g-${journey}`, "businessGroup", `Journey ${journey}`));
    for (let step = 0; step < 8; step++) {
      nodes.push(node(`s-${journey}-${step}`, "activity", `Step ${step} of ${journey}`, { parentId: `g-${journey}` }));
      if (step > 0) edges.push(edge(`e-${journey}-${step}`, `s-${journey}-${step - 1}`, `s-${journey}-${step}`));
    }
    edges.push(edge(`start-${journey}`, "customer", `s-${journey}-0`));
  }
  const large = modelFromHelicopter(base(nodes, edges));
  assert.deepEqual(helicopterDefaultExpanded(large), []);
  const first = project(large, helicopterDefaultExpanded(large));
  assert.deepEqual(visible(first).sort(), ["customer", "g-a", "g-b", "g-c"], "a handful of cards by default");
  assert.equal(first.nodes.filter(item => item.collapsed).reduce((sum, item) => sum + item.memberCount, 0), 24, "all 24 steps are counted inside the journeys");
  assert.equal(first.total.edges, 24, "three chains of seven flows and one start for each journey, all accounted for while collapsed");
  assert.equal(first.total.nodes, 25, "items are the person and the 24 steps; the three journeys are containers, not items");
  // Every step can be made a visible card by opening the journeys that hold it.
  const index = buildIndex(large);
  for (const item of nodes.filter(entry => entry.kind === "activity")) {
    const open = revealNodes(index, [item.id], new Set());
    assert.ok(visible(projectGraph(index, { mode: "system", expanded: open })).includes(item.id), item.id);
  }
  const several = revealNodes(index, ["s-a-3", "s-c-7"], new Set(["g-b"]));
  assert.deepEqual([...several].sort(), ["g-a", "g-b", "g-c"], "existing choices are kept and unknown ids are ignored");
  assert.deepEqual([...revealNodes(index, ["ghost"], new Set())], []);
});

test("every capability is listed; its steps resolve only to known nodes and nothing is dropped silently", () => {
  const payload = appointment();
  const rows = capabilityRows(payload);
  assert.deepEqual(rows.map(item => item.id), ["cap-book", "cap-cancel"]);
  assert.deepEqual(rows[0].steps.map(item => item.label), ["Customer asks for an appointment", "Is a slot free?", "Clinic confirms the visit"]);
  assert.deepEqual(rows[1].steps, [{ id: "cancelled", label: "Appointment is cancelled" }]);
  assert.equal(rows[1].needsClarification, true);
  assert.equal(rows[0].needsClarification, false);
  assert.deepEqual(capabilityRows(base([], [], [{ id: "orphan", label: "Orphan", description: "", nodeIds: ["x"], evidenceIds: [] }]))[0].steps, []);
});

test("technical evidence resolves only to recorded evidence and the summary counts what needs clarification", () => {
  const payload = appointment();
  assert.deepEqual(evidenceFor(payload, ["e2", "nope", "e1"]).map(item => item.path), ["docker-compose.yml", "src/app.ts"]);
  assert.deepEqual(helicopterSummary(payload), { journeys: 1, steps: 6, capabilities: 2, unclear: 3 });
  assert.equal(needsClarification("  Needs clarification: price"), true);
  assert.equal(needsClarification("The customer needs clarification"), false);
});

test("journeys list in reading order with depth, openness and the people, steps and results they hold", () => {
  const payload = base([
    node("outer", "businessGroup", "Visits"),
    node("inner", "businessGroup", "Booking", { parentId: "outer" }),
    node("a", "activity", "Clinic books", { parentId: "inner" }),
    node("b", "activity", "Clinic reminds", { parentId: "outer" }),
    node("other", "businessGroup", "Billing"),
    node("c", "activity", "Clinic invoices", { parentId: "other" }),
    node("loose", "outcome", "Visit happens"),
  ]);
  assert.deepEqual(journeyRows(payload, ["inner"]), [
    { id: "outer", label: "Visits", depth: 0, open: false, steps: 2 },
    { id: "inner", label: "Booking", depth: 1, open: true, steps: 1 },
    { id: "other", label: "Billing", depth: 0, open: false, steps: 1 },
  ]);
  assert.deepEqual(journeyRows(base([node("only", "actor", "Customer")]), []), [], "no journey: nothing to open");
  // Opening the rows' ids reveals exactly the steps they hold.
  const index = buildIndex(modelFromHelicopter(payload));
  assert.deepEqual(visible(projectGraph(index, { mode: "system", expanded: new Set(["outer", "inner"]) })).sort(), ["a", "b", "loose", "other"]);
});
