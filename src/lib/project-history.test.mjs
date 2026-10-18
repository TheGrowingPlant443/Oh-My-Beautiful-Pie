import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
const source = (await readFile(new URL("./project-history.ts", import.meta.url), "utf8")).replace('"./repo-agents"', JSON.stringify(new URL("./repo-agents.ts", import.meta.url).href));
const { folderOptions, rememberFolder, parseProjectHistory, persistProjectHistory, isHiddenComputed, isRememberable, coordinatorMoveFolders } = await import(`data:text/javascript,${encodeURIComponent(stripTypeScriptTypes(source))}`);

test("visits survive reload without saved sessions and selected folder remains first", () => {
  let raw = null;
  const storage = { getItem: () => raw, setItem: (_key, value) => { raw = value; } };
  let history = [];
  for (const folder of ["A", "B", "C"]) history = persistProjectHistory(storage, history, [], folder).folders;
  history = parseProjectHistory(raw).folders;
  assert.deepEqual(history, ["C", "B", "A"]);
  assert.deepEqual(folderOptions(rememberFolder(history, "D"), [], "A"), ["A", "D", "C", "B"]);
});
test("case slash extended Windows and UNC aliases deduplicate while display spelling survives", () => {
  assert.deepEqual(folderOptions(["C:/Repo/", "\\\\?\\C:\\repo", "\\\\?\\UNC\\Server\\Share\\", "//server/share"], ["D:/other"], "c:\\REPO"), ["c:\\REPO", "\\\\?\\UNC\\Server\\Share\\", "D:/other"]);
  assert.deepEqual(folderOptions(["/", "C:/", "C:\\"], [], ""), ["/", "C:/"]);
});
test("seeds and concurrent stored folders merge without truncation or switching selection", () => {
  let raw = JSON.stringify({ version: 1, folders: ["other-window", "old-deleted-folder"] });
  const result = persistProjectHistory({ getItem: () => raw, setItem: (_key, value) => { raw = value; } }, ["manual"], ["legacy-quality", "legacy-dependency", "saved-quality"], "selected");
  assert.equal(result.error, null);
  assert.deepEqual(result.folders, ["selected", "manual", "legacy-quality", "legacy-dependency", "saved-quality", "other-window", "old-deleted-folder"]);
  assert.equal(folderOptions(Array.from({ length: 500 }, (_, i) => `folder-${i}`), [], "").length, 500);
});
test("invalid future storage and failed writes retain in-memory choices and original bytes", () => {
  for (const raw of ["bad json", JSON.stringify({ version: 2, folders: [] }), JSON.stringify({ version: 1, folders: [1] })]) {
    let writes = 0;
    const result = persistProjectHistory({ getItem: () => raw, setItem: () => { writes++; } }, ["known"], ["seed"]);
    assert.ok(result.error);
    assert.deepEqual(result.folders, ["known", "seed"]);
    assert.equal(writes, 0);
  }
  const result = persistProjectHistory({ getItem: () => null, setItem: () => { throw new Error("quota"); } }, [], [], "chosen");
  assert.deepEqual(result.folders, ["chosen"]);
  assert.match(result.error, /quota/);
  assert.deepEqual(parseProjectHistory(null), { folders: [], error: null });
});
test("a computed shared folder is hidden from choices unless it is a live project or the selection", () => {
  const history = ["C:/Shared", "D:/B", "D:/A"];
  assert.deepEqual(folderOptions(history, [], "", ["c:\\shared\\"]), ["D:/B", "D:/A"]);
  assert.deepEqual(folderOptions(history, ["C:\\Shared"], "", ["C:/Shared"]), ["C:/Shared", "D:/B", "D:/A"], "a real ordinary project at the same path survives");
  assert.deepEqual(folderOptions(history, [], "c:/shared", ["C:/Shared"]), ["c:/shared", "D:/B", "D:/A"], "an explicit selection survives");
  assert.deepEqual(folderOptions(history, ["D:/Live"], "", []), ["C:/Shared", "D:/B", "D:/A", "D:/Live"], "without computed folders nothing is hidden");
  assert.equal(isHiddenComputed("C:/Shared", ["c:/shared"], []), true);
  assert.equal(isHiddenComputed("C:/Shared", ["c:/shared"], ["C:\\Shared\\"]), false);
  assert.equal(isHiddenComputed("C:/Other", ["c:/shared"], []), false);
});
test("a stored entry polluted before the computed list existed is hidden after reload", () => {
  const raw = JSON.stringify({ version: 1, folders: ["C:/Shared", "D:/A"] });
  const stored = parseProjectHistory(raw);
  assert.deepEqual(stored.folders, ["C:/Shared", "D:/A"], "stored data is never rewritten by reading");
  assert.deepEqual(folderOptions(stored.folders, [], "", ["C:/Shared"]), ["D:/A"]);
});
test("A to Shared to B never records Shared while A and B are remembered", () => {
  const computed = ["C:/Shared"];
  assert.deepEqual(coordinatorMoveFolders("D:/A", "C:/Shared", [], computed, []), ["D:/A"]);
  assert.deepEqual(coordinatorMoveFolders("C:/Shared", "D:/B", [], computed, []), ["D:/B"]);
  assert.deepEqual(coordinatorMoveFolders("D:/A", "C:/Shared", ["d:\\a"], computed, []), [], "bootstrap temporary folders stay unrecorded");
  assert.deepEqual(coordinatorMoveFolders("D:/A", "C:/Shared", [], computed, ["C:/Shared"]), ["D:/A", "C:/Shared"], "a live ordinary project at that path is recorded");
});
test("remembering respects computed folders unless the choice is explicit", () => {
  assert.equal(isRememberable("C:/Shared", ["C:/Shared"], [], false), false);
  assert.equal(isRememberable("C:/Shared", ["C:/Shared"], [], true), true);
  assert.equal(isRememberable("C:/Shared", ["C:/Shared"], ["c:/shared"], false), true);
  assert.equal(isRememberable("D:/A", ["C:/Shared"], [], false), true);
});
test("seeding skips computed folders that are not live projects or the selection and keeps stored entries", () => {
  let raw = JSON.stringify({ version: 1, folders: ["other-window"] });
  const storage = { getItem: () => raw, setItem: (_key, value) => { raw = value; } };
  const computed = ["C:/Shared"];
  let result = persistProjectHistory(storage, [], ["C:/Shared", "D:/A"], "", computed, []);
  assert.deepEqual(result.folders, ["D:/A", "other-window"]);
  result = persistProjectHistory(storage, result.folders, ["C:/Shared"], "", computed, ["C:\\Shared"]);
  assert.deepEqual(result.folders, ["D:/A", "other-window", "C:/Shared"], "a live project at the same path is seeded");
  result = persistProjectHistory(storage, [], ["C:/Shared"], "c:/shared", computed, []);
  assert.equal(result.folders[0], "c:/shared", "the selection is kept");
});
