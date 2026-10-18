import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
const source = (await readFile(new URL("./workspace.ts", import.meta.url), "utf8")).replace('"./repo-agents"', JSON.stringify(new URL("./repo-agents.ts", import.meta.url).href));
const { architectEntryFolder, workspacePending } = await import(`data:text/javascript,${encodeURIComponent(stripTypeScriptTypes(source))}`);

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
