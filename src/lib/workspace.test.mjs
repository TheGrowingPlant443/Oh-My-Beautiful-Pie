import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
const source = (await readFile(new URL("./workspace.ts", import.meta.url), "utf8")).replace('"./repo-agents"', JSON.stringify(new URL("./repo-agents.ts", import.meta.url).href));
const { architectEntryFolder, workspaceEntryFolder, workspacePending, qualityEntryFolder } = await import(`data:text/javascript,${encodeURIComponent(stripTypeScriptTypes(source))}`);

test("analyzer entry uses the committed Architect directory", () => {
  assert.equal(architectEntryFolder("C:/architect/repo", []), "C:/architect/repo");
});

test("temporary coordinator workspaces defer to analyzer manual preferences", () => {
  assert.equal(architectEntryFolder("c:\\temp\\architect\\", ["C:/Temp/Architect"]), undefined);
  assert.equal(architectEntryFolder(undefined, []), undefined);
  assert.equal(architectEntryFolder("C:/real/repo", ["C:/temp/architect"]), "C:/real/repo");
});

test("captured analyzer entry stays unchanged when Architect context changes later", () => {
  let committed = "C:/repo-a";
  const entry = architectEntryFolder(committed, []);
  committed = "C:/repo-b";
  assert.equal(entry, "C:/repo-a");
  assert.equal(architectEntryFolder(committed, []), "C:/repo-b");
});

test("only pending and applying workspace changes block context-dependent entry", () => {
  for (const status of ["pending", "applying"]) assert.equal(workspacePending({ status }), true);
  for (const status of ["applied", "failed", "cancelled"]) assert.equal(workspacePending({ status }), false);
});

test("selector entry captures the original project, including an authorized pending folder", () => {
  const instance = { id: "architect", projectPath: "/managed/worktree", projectFolder: "/original/repo" };
  assert.equal(workspaceEntryFolder(instance, [], []), "/original/repo");
  const pending = { coordinatorId: "architect", targetFolder: "/next/repo", status: "pending" };
  assert.equal(workspaceEntryFolder(instance, [pending], []), "/next/repo");
  assert.equal(workspaceEntryFolder(instance, [pending, { ...pending, coordinatorId: "manager", targetFolder: "/unrelated" }], []), "/next/repo");
  assert.equal(workspaceEntryFolder(instance, [pending, { ...pending, status: "applying", targetFolder: "/latest" }], []), "/latest");
  assert.equal(workspaceEntryFolder(instance, [{ ...pending, status: "failed" }], []), "/original/repo");
  assert.equal(workspaceEntryFolder(instance, [pending], ["/next/repo"]), undefined);
  assert.equal(workspaceEntryFolder(undefined, [], []), undefined);
});

test("Code quality opened by a coordinator plan session uses the target's logical project", () => {
  const manager = { id: "manager", kind: "manager", projectPath: "/private/manager-worktree", projectFolder: "/original/project" };
  const planner = { id: "planner", kind: "instance", planTarget: manager.id, helper: null, projectPath: "/private/plan-context" };
  const instances = new Map([[manager.id, manager], [planner.id, planner]]);
  assert.equal(qualityEntryFolder(planner, instances, [], [], undefined), "/original/project");
  assert.equal(qualityEntryFolder(planner, instances, [{ coordinatorId: manager.id, status: "pending", targetFolder: "/next/project" }], [], undefined), "/next/project");
  assert.equal(qualityEntryFolder(planner, instances, [], ["/original/project"], "/remembered/project"), "/remembered/project");
  assert.equal(qualityEntryFolder(planner, new Map(), [], [], undefined), undefined);
});

test("Code quality keeps ordinary sessions local and a missing helper target uses its bound repository", () => {
  const worker = { id: "worker", kind: "instance", projectPath: "/worker/project", projectFolder: null, helper: null, planTarget: null };
  assert.equal(qualityEntryFolder(worker, new Map(), [], [], "/remembered"), "/worker/project");
  const helper = { ...worker, helper: { target: "missing", repo: "/bound/repository" } };
  assert.equal(qualityEntryFolder(helper, new Map(), [], [], "/remembered"), "/bound/repository");
});
