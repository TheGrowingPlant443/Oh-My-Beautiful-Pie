import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
const source = await readFile(new URL("./requirements-view.ts", import.meta.url), "utf8");
const view = await import(`data:text/javascript,${encodeURIComponent(stripTypeScriptTypes(source))}`);
const { groupRequirements, staticItems, architectItems, needLabel, environmentLabel, detectedLabel, installLabel, sourceLabel, evidenceLocation, insightInFlight, CATEGORY_ORDER } = view;

function item(id, category, source = "static", extra = {}) {
  return {
    id, name: id, category, need: "unknown", environment: "host", evidence: [], detected: { state: "unknown" },
    install: { availability: "manual", note: "No installer in OMP Manager." }, explanation: "", source, setupLink: null, ...extra,
  };
}

test("groups follow the fixed category order and omit empty categories", () => {
  const groups = groupRequirements([item("e", "executable"), item("r2", "runtime"), item("c", "containerEngine"), item("r1", "runtime")]);
  assert.deepEqual(groups.map(group => group.category), ["runtime", "containerEngine", "executable"]);
  assert.deepEqual(groups[0].items.map(entry => entry.id), ["r2", "r1"], "backend order inside a group is kept");
  assert.equal(CATEGORY_ORDER.length, 7);
});

test("architect findings never join the static groups or change static items", () => {
  const stat = item("runtime:node", "runtime", "static", { detected: { state: "missing" } });
  const found = item("runtime:node", "runtime", "architect", { detected: { state: "found", version: "99" } });
  const all = [stat, found];
  assert.deepEqual(groupRequirements(all).flatMap(group => group.items), [stat]);
  assert.deepEqual(staticItems(all), [stat]);
  assert.deepEqual(architectItems(all), [found]);
  assert.equal(stat.detected.state, "missing");
});

test("badge labels state need, environment, detection, install and source plainly", () => {
  assert.deepEqual(["required", "optional", "unknown"].map(needLabel), ["Required", "Optional", "Need unknown"]);
  assert.deepEqual(["host", "wsl", "container"].map(environmentLabel), ["This computer", "WSL", "Container"]);
  assert.equal(detectedLabel({ state: "found", version: "1.2.3" }), "Found 1.2.3");
  assert.equal(detectedLabel({ state: "found" }), "Found");
  assert.equal(detectedLabel({ state: "missing" }), "Not found");
  assert.equal(detectedLabel({ state: "unknown", path: "C:/x" }), "Unknown");
  assert.equal(installLabel({ availability: "automatic", via: "Code quality setup", note: "" }), "Installable via Code quality setup");
  assert.equal(installLabel({ availability: "manual", note: "" }), "Manual install");
  assert.equal(installLabel({ availability: "unknown", note: "" }), "Install unknown");
  assert.equal(sourceLabel("architect"), "Architect finding (unverified)");
  assert.equal(sourceLabel("static"), "From project files");
});

test("evidence locations and in-flight states", () => {
  assert.equal(evidenceLocation({ path: "README.md", line: 3, text: "" }), "README.md:3");
  assert.equal(evidenceLocation({ path: "package.json", text: "" }), "package.json");
  assert.deepEqual(["pending", "submitted", "uncertain", "completed", "failed"].map(insightInFlight), [true, true, true, false, false]);
});
