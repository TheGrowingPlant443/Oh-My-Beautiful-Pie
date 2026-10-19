import test from "node:test";
import assert from "node:assert/strict";
import { get } from "svelte/store";
import { importTs } from "./test-loader.mjs";

const { createKnowledgeStore, validateCogneeUrl, graphifyVisible, cogneeVisible, obsidianInstalled } = await importTs("knowledge");

const caps = (folder, extra = {}) => ({
  folder,
  obsidian: { state: "absent", version: null, executable: null, registeredVaults: [], message: null, declinedForProject: false },
  graphify: { present: false, state: "absent", sourceRoot: null, artifactId: null, artifactHash: null, message: null },
  cognee: { installed: false, version: null, uiUrl: null, installationFolder: null, message: null },
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

test("a failed probe keeps the last capabilities and reports the failure instead of claiming absence", async () => {
  let fail = false;
  const store = createKnowledgeStore({ getCapabilities: async folder => { if (fail) throw new Error("probe failed"); return caps(folder, { graphify: { present: true, state: "present", sourceRoot: "C:/p", artifactId: null, artifactHash: null, message: null } }); } });
  await store.probe("C:/p");
  fail = true;
  assert.equal(await store.probe("C:/p"), null);
  const state = get(store);
  assert.equal(state.error, "probe failed");
  assert.equal(graphifyVisible(state.capabilities), true, "an installation or probe error never hides what was found before");
});

test("buttons follow the capabilities: Graphify only for a project output (broken ones stay visible), Cognee only when installed", () => {
  assert.equal(graphifyVisible(null), false);
  assert.equal(graphifyVisible(caps(null)), false);
  for (const state of ["present", "malformed", "foreign", "unverified", "unavailable"]) {
    assert.equal(graphifyVisible(caps("C:/p", { graphify: { present: true, state, sourceRoot: null, artifactId: null, artifactHash: null, message: null } })), true, state);
  }
  assert.equal(cogneeVisible(caps(null)), false);
  assert.equal(cogneeVisible(caps(null, { cognee: { installed: true, version: "1.6.4", uiUrl: null, installationFolder: null, message: null } })), true);
  assert.equal(cogneeVisible(caps(null, { cognee: { state: "unreadable", installed: null, version: null, uiUrl: null, installationFolder: null, message: "denied" } })), false, "unknown installation is not proof Cognee is installed; diagnostics remain in Settings");
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

test("the Cognee address must be a credential-free loopback http(s) address", () => {
  const ok = text => validateCogneeUrl(text).ok;
  assert.equal(ok("http://127.0.0.1:3000"), true);
  assert.equal(ok("http://localhost:3000/ui"), true);
  assert.equal(ok("https://[::1]:8443"), true);
  assert.equal(validateCogneeUrl(" http://localhost:3000 ").url, "http://localhost:3000/");
  for (const bad of ["", "   ", "javascript:alert(1)", "file:///c:/x", "data:text/html,x", "ftp://127.0.0.1", "http://example.com", "http://192.168.1.5:3000", "http://user:pass@127.0.0.1:3000",
    "http://127.0.0.1:3000/?token=1", "http://127.0.0.1:3000/#frag", "http://127.0.0.1:3000/a b", "not a url", "http://localhost.evil.com", "http://127.0.0.1.evil.com"]) {
    assert.equal(ok(bad), false, `rejects ${JSON.stringify(bad)}`);
  }
  assert.match(validateCogneeUrl("http://example.com").message, /this computer/);
});
