import test from "node:test";
import assert from "node:assert/strict";
import { get } from "svelte/store";
import { importTs } from "./test-loader.mjs";

const { createKnowledgeStore, capabilitiesFor, graphifyVisible, graphifyAction, cogneeVisible, obsidianInstalled, projectFolder } = await importTs("knowledge");

const graphify = (state, extra = {}) => ({ present: state !== "absent", state, sourceRoot: null, artifactId: null, artifactHash: null, message: null, ...extra });
const cognee = (extra = {}) => ({ state: "absent", installed: false, version: null, installationFolder: null, message: null, ui: { state: "missingFrontend", message: null }, ...extra });
const caps = (folder, extra = {}) => ({
  folder,
  projectKey: folder ? folder.trim().toLowerCase() : null,
  obsidian: { state: "absent", version: null, executable: null, registeredVaults: [], message: null, declinedForProject: false },
  graphify: graphify("absent"),
  cognee: cognee(),
  projectVault: null, globalVault: null, suggestedProjectRoot: null, suggestedGlobalRoot: null, ...extra,
});

test("capability probes are fenced: a late answer for an earlier folder never replaces the current one", async () => {
  const gates = new Map();
  const store = createKnowledgeStore({ getCapabilities: folder => new Promise(resolve => gates.set(folder, () => resolve(caps(folder)))) });
  const first = store.probe("C:/one");
  const second = store.probe("C:/two");
  gates.get("C:/two")();
  assert.equal((await second).folder, "C:/two");
  gates.get("C:/one")();
  assert.equal(await first, null, "the older probe is dropped");
  assert.equal(get(store).capabilities.folder, "C:/two");
  assert.equal(get(store).loading, false);
});

test("a failed probe of the same project keeps its answer and reports the failure instead of claiming absence", async () => {
  let fail = false;
  const store = createKnowledgeStore({ getCapabilities: async folder => { if (fail) throw new Error("probe failed"); return caps(folder, { graphify: graphify("present", { sourceRoot: "C:/p" }) }); } });
  await store.probe("C:/p");
  fail = true;
  assert.equal(await store.probe("C:/p"), null);
  const state = get(store);
  assert.equal(state.error, "probe failed");
  assert.equal(graphifyAction(capabilitiesFor(state, "C:/p")), "view", "an error never hides what was found before for the same project");
});

test("capabilities retained from another project are never used for the project now shown (in flight, failed, A to B to A)", async () => {
  const gates = new Map();
  let failB = false;
  const store = createKnowledgeStore({
    getCapabilities: folder => failB && folder === "C:/b" ? Promise.reject(new Error("denied")) : new Promise(resolve => gates.set(folder, () => resolve(caps(folder, { graphify: graphify("present") })))),
  });
  const a = store.probe("C:/a");
  gates.get("C:/a")();
  await a;
  assert.equal(graphifyAction(capabilitiesFor(get(store), "C:/a")), "view");
  const b = store.probe("C:/b");
  assert.equal(capabilitiesFor(get(store), "C:/b"), null, "A's answer is not B's while B is probing");
  gates.get("C:/b")();
  await b;
  assert.equal(capabilitiesFor(get(store), "C:/b")?.folder, "C:/b");
  assert.equal(capabilitiesFor(get(store), "C:/a"), null, "and B's answer is not A's after returning to A");
  failB = true;
  await store.probe("C:/b");
  assert.equal(get(store).error, "denied");
  assert.equal(capabilitiesFor(get(store), "C:/b")?.folder, "C:/b", "a failed re-probe of the same project keeps it");
  const back = store.probe("C:/a");
  assert.equal(capabilitiesFor(get(store), "C:/a"), null, "A again is unknown until its own probe answers");
  gates.get("C:/a")();
  await back;
  assert.equal(capabilitiesFor(get(store), " C:/a ")?.folder, "C:/a", "the echoed folder is the trimmed one");
  assert.equal(capabilitiesFor(get(store), null), null);
  assert.equal(capabilitiesFor(get(store), "   "), null);
});

test("Graphify is reachable for every shown project and opens the viewer only for a valid admitted output", () => {
  assert.equal(graphifyVisible(null), false);
  assert.equal(graphifyVisible("  "), false);
  assert.equal(graphifyVisible("C:/p"), true, "even with absent, foreign or unreadable output");
  assert.equal(projectFolder("  C:/p "), "C:/p");
  assert.equal(graphifyAction(null), "access", "a project that has not been probed (yet) opens the dialog with its diagnostic");
  for (const state of ["present", "unverified"]) assert.equal(graphifyAction(caps("C:/p", { graphify: graphify(state) })), "view", state);
  for (const state of ["absent", "foreign", "malformed", "unavailable"]) assert.equal(graphifyAction(caps("C:/p", { graphify: graphify(state) })), "access", state);
  assert.equal(graphifyAction(caps("C:/p", { graphify: graphify("present", { present: false }) })), "access", "present state without presence is not a graph");
});

test("Cognee shows only on positive installed evidence, and the UI capability is separate from installation", () => {
  assert.equal(cogneeVisible(caps(null)), false);
  assert.equal(cogneeVisible(caps(null, { cognee: cognee({ state: "installed", installed: true, version: "1.6.4", ui: { state: "missingFrontend", message: "no frontend" } }) })), true, "SDK installed, frontend missing is still installed");
  assert.equal(cogneeVisible(caps(null, { cognee: cognee({ state: "unreadable", installed: null, message: "denied", ui: { state: "unreadable", message: null } }) })), false, "unknown installation is not proof Cognee is installed");
  assert.equal(obsidianInstalled(caps(null, { obsidian: { state: "installed", version: "1.14.4", executable: "C:/o.exe", registeredVaults: [], message: null, declinedForProject: false } })), true);
  assert.equal(obsidianInstalled(caps(null, { obsidian: { state: "unreadable", version: null, executable: null, registeredVaults: [], message: "denied", declinedForProject: false } })), false);
});

test("adopting a backend answer replaces the capabilities and invalidates probes still in flight", async () => {
  let release;
  const store = createKnowledgeStore({ getCapabilities: () => new Promise(resolve => { release = () => resolve(caps("C:/old")); }) });
  const pending = store.probe("C:/old");
  store.adopt(caps("C:/new"));
  release();
  assert.equal(await pending, null);
  assert.equal(get(store).capabilities.folder, "C:/new");
});
