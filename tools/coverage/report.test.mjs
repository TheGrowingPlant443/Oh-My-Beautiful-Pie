import assert from "node:assert/strict";
import { test } from "node:test";
import { lcovTotals, normalizeLcov } from "./report.mjs";

const options = { root: "/repo", lineCount: path => path === "src/lib/example.ts" ? 2 : null };

test("merges repeated original files without counting the same line twice", () => {
  const output = normalizeLcov("SF:/repo/src/lib/example.ts\nDA:1,1\nend_of_record\nSF:/repo/src/lib/example.ts\nDA:1,2\nDA:2,0\nend_of_record\n", options);
  assert.equal(output.match(/^SF:/gmu)?.length, 1);
  assert.match(output, /^DA:1,3$/mu);
  assert.deepEqual(lcovTotals(output), { found: 2, hit: 1 });
});

test("rejects line hits outside the original source", () => {
  assert.throws(() => normalizeLcov("SF:/repo/src/lib/example.ts\nDA:3,1\nend_of_record\n", options), /counts line 3/u);
});

test("does not admit files outside the repository or generated dependency copies", () => {
  for (const path of ["/elsewhere/source.ts", "../outside.ts", "node_modules/cache/generated.mjs", "src-tauri/target/generated.rs"]) {
    assert.equal(normalizeLcov(`SF:${path}\nDA:1,1\nend_of_record\n`, options), "");
  }
});
