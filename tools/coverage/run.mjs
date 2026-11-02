import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, rmSync, statSync, symlinkSync, writeFileSync } from "node:fs";
import { isAbsolute, join } from "node:path";
import { homedir } from "node:os";
import { fileURLToPath } from "node:url";
import { lcovTotals, normalizeLcov } from "./report.mjs";

const root = fileURLToPath(new URL("../../", import.meta.url));
process.chdir(root);
const output = join(root, ".quality-coverage");
const raw = join(output, "raw");
const reports = ["javascript.lcov", "python.xml", "rust.lcov"];
const configuredCache = process.env.XDG_CACHE_HOME;
const cacheRoot = configuredCache && isAbsolute(configuredCache) ? configuredCache : join(homedir(), ".cache");
const projectKey = createHash("sha256").update(root).digest("hex").slice(0, 16);
const rustTarget = join(cacheRoot, "omp-manager", "coverage", projectKey, "target");

function run(command, args, environment = process.env) {
  if (!command || !isAbsolute(command)) throw new Error("Coverage tool paths must come from the Nix development shell");
  const result = spawnSync(command, args, { cwd: root, env: environment, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    const outcome = result.signal ?? `exit ${result.status}`;
    throw new Error(`${command} failed: ${outcome}`);
  }
}
function readReport(path) {
  if (!statSync(path).isFile()) throw new Error(`Coverage report is not a regular file: ${path}`);
  const text = readFileSync(path, "utf8");
  if (!text.trim()) throw new Error(`Coverage report is empty: ${path}`);
  return text;
}
function writeLcov(input, filename) {
  const text = normalizeLcov(readReport(input), {
    root,
    lineCount(path) {
      const source = join(root, path);
      if (!existsSync(source)) return null;
      return readFileSync(source, "utf8").split(/\r?\n/u).length;
    },
  });
  if (!(lcovTotals(text).found > 0)) throw new Error(`${filename} contains no instrumented source lines`);
  writeFileSync(join(output, filename), text);
  return text;
}
function removeReports() {
  for (const name of reports) rmSync(join(output, name), { force: true });
}
function verifyPythonReport() {
  const xml = readReport(join(output, "python.xml"));
  const validLines = /lines-valid="([0-9]+)"/u.exec(xml);
  if (!validLines || Number(validLines[1]) === 0 || !xml.includes("<class ") || !xml.includes("<source>bridge/source-index</source>")) {
    throw new Error("Python coverage has no repository-relative source data");
  }
}
function rustEnvironment() {
  const cargoHome = join(output, "cargo-home");
  mkdirSync(cargoHome, { recursive: true });
  const vendor = process.env.OMP_COVERAGE_CARGO_VENDOR;
  if (!vendor || !isAbsolute(vendor)) throw new Error("The Nix Cargo vendor directory is unavailable");
  writeFileSync(join(cargoHome, "config.toml"), `[source.crates-io]\nreplace-with = "omp-vendored"\n[source.omp-vendored]\ndirectory = ${JSON.stringify(vendor)}\n[net]\noffline = true\n`);
  return { ...process.env, CARGO_HOME: cargoHome, CARGO_NET_OFFLINE: "true", CARGO_LLVM_COV_TARGET_DIR: rustTarget };
}

if (!process.env.OMP_COVERAGE_PYTHON || !process.env.OMP_TEST_NODE_MODULES || !process.env.OMP_COVERAGE_CARGO) {
  throw new Error("Run with: nix develop -c npm run test:coverage");
}
const modules = join(root, "node_modules");
if (!existsSync(modules)) symlinkSync(process.env.OMP_TEST_NODE_MODULES, modules, "dir");
removeReports();
rmSync(raw, { recursive: true, force: true });
mkdirSync(raw, { recursive: true });

try {
  run(process.execPath, [
    "--experimental-strip-types", "--test", "--experimental-test-coverage",
    "--test-reporter=spec", "--test-reporter-destination=stdout",
    "--test-reporter=lcov", `--test-reporter-destination=${join(raw, "javascript.lcov")}`,
    "src/lib/*.test.mjs", "bridge/*.test.mjs", "tools/coverage/*.test.mjs",
  ]);
  const javascript = writeLcov(join(raw, "javascript.lcov"), "javascript.lcov");
  if (!/^SF:src\/lib\/.+\.ts$/mu.test(javascript)) throw new Error("JavaScript coverage contains no original library TypeScript files");
  const python = process.env.OMP_COVERAGE_PYTHON;
  const pythonConfig = "--rcfile=tools/coverage/python.ini";
  run(python, ["-B", "-m", "coverage", "erase", pythonConfig]);
  run(python, ["-B", "-m", "coverage", "run", pythonConfig, "-m", "unittest", "discover", "-s", "bridge/source-index", "-p", "test_*.py"]);
  run(python, ["-B", "-m", "coverage", "combine", pythonConfig]);
  run(python, ["-B", "-m", "coverage", "xml", pythonConfig]);
  verifyPythonReport();
  mkdirSync(rustTarget, { recursive: true, mode: 0o700 });
  run(process.env.OMP_COVERAGE_CARGO, [
    "llvm-cov", "--manifest-path", "src-tauri/Cargo.toml", "--locked", "--offline", "--lcov",
    "--output-path", join(raw, "rust.lcov"),
  ], rustEnvironment());
  writeLcov(join(raw, "rust.lcov"), "rust.lcov");
  for (const name of reports) {
    readReport(join(output, name));
    console.log(`Coverage report: .quality-coverage/${name}`);
  }
} catch (error) {
  removeReports();
  throw error;
}
