import test from "node:test";
import assert from "node:assert/strict";
import { repoKey, sameRepoAgents } from "./repo-agents.ts";

const row = (id, patch = {}) => ({ id, name: id, projectPath: "C:\\Work\\Repo", status: "running", planTarget: null, helper: null, ...patch });
const helper = (id, target, patch = {}) => row(id, { projectPath: `C:\\cfg\\helpers\\${target}\\git`, helper: { target, purpose: "git", repo: "x" }, ...patch });
const ids = rows => rows.map(r => r.id);

test("a lone session has no same-repo agents", () => {
  const a = row("a");
  assert.deepEqual(sameRepoAgents([a], a), []);
});

test("sessions match on folder ignoring slash style, case and trailing slash; stopped and other folders do not", () => {
  const a = row("a", { projectPath: "C:\\Work\\Repo\\" });
  const all = [a, row("b", { projectPath: "c:/work/repo" }), row("c", { projectPath: "C:/Other" }), row("d", { projectPath: "c:/work/repo", status: "stopped" })];
  assert.deepEqual(ids(sameRepoAgents(all, a)), ["b"]);
});

test("repoKey normalizes extended-length, UNC and trailing separators", () => {
  assert.equal(repoKey("  \\\\?\\C:\\Work\\Repo\\\\ "), "c:/work/repo");
  assert.equal(repoKey("\\\\?\\UNC\\Server\\Share\\"), "//server/share");
  assert.equal(repoKey("/"), "");
  assert.equal(repoKey("/Ünï/Dir//"), "/ünï/dir");
});

test("helpers of the session are listed after sessions, sorted by name; other folders' helpers are not", () => {
  const a = row("a"), g = row("g", { name: "Gamma" }), z = row("z", { projectPath: "C:/Other" });
  const all = [a, helper("h2", "a", { name: "Git helper" }), helper("h1", "a", { name: "Diff helper" }), g, helper("h3", "z", { name: "Diff helper z" }), z];
  assert.deepEqual(ids(sameRepoAgents(all, a)), ["g", "h1", "h2"]);
});

test("viewing a helper lists its session, sibling helper and same-folder sessions but not itself", () => {
  const a = row("a"), g = row("g"), h1 = helper("h1", "a", { name: "Diff" }), h2 = helper("h2", "a", { name: "Git" });
  assert.deepEqual(ids(sameRepoAgents([a, g, h1, h2], h1)), ["a", "g", "h2"]);
});

test("plan agents are never listed", () => {
  const a = row("a");
  assert.deepEqual(sameRepoAgents([a, row("p", { planTarget: "a" })], a), []);
});

test("a helper whose target row is missing yields nothing and is never a candidate", () => {
  const a = row("a"), orphan = helper("o", "missing");
  assert.deepEqual(sameRepoAgents([a, orphan], orphan), []);
  assert.deepEqual(sameRepoAgents([a, orphan], a), []);
});
