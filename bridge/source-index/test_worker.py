"""Registry, capabilities and the supervised extract coordinator (real subprocesses, hermetic temp dirs)."""
from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

import common
import profiles

SRC = Path(__file__).resolve().parent
PLAN_EXTENSIONS = {
    "python": [".py"],
    "javascript": [".js", ".jsx", ".mjs", ".cjs"],
    "typescript": [".ts", ".mts", ".cts"],
    "tsx": [".tsx"],
    "go": [".go"],
    "rust": [".rs"],
    "java": [".java"],
    "groovy": [".groovy", ".gradle"],
    "c": [".c", ".h"],
    "cpp": [".cpp", ".cc", ".cxx", ".hpp", ".hh", ".hxx", ".cu", ".cuh", ".metal"],
    "ruby": [".rb", ".rake"],
    "csharp": [".cs"],
    "kotlin": [".kt", ".kts"],
    "scala": [".scala"],
    "php": [".php"],
    "lua": [".lua", ".luau"],
    "swift": [".swift"],
    "bash": [".sh", ".bash"],
    "powershell": [".ps1", ".psm1"],
    "zig": [".zig"],
    "elixir": [".ex", ".exs"],
    "objc": [".m", ".mm"],
    "julia": [".jl"],
    "fortran": [".f", ".F", ".f90", ".F90", ".f95", ".F95", ".f03", ".F03", ".f08", ".F08"],
    "verilog": [".v", ".sv", ".svh", ".vh"],
    "svelte": [".svelte"],
    "vue": [".vue"],
    "astro": [".astro"],
    "ejs": [".ejs"],
}
IDENTITY_RESOLVER = "def resolve(index, root):\n    return index\n"
STUB_PYTHON = '''
import os, time
import common

PROFILES = {"python": {"extensions": [".py"], "grammar": "tree_sitter_python", "capsule": "language", "node_kinds": ["function_definition"]}}


def extract(language, source, ctx):
    pid_file = os.environ.get("OMP_TEST_PIDFILE")
    if b"HANG" in source:
        if pid_file:
            with open(pid_file, "w") as fh:
                fh.write(str(os.getpid()))
        time.sleep(120)
    if b"CRASH" in source:
        os._exit(3)
    ctx.module_symbol((0, len(source)))
    try:
        ctx.add_symbol("function", "ok", (0, len(source)), node_kind="stub")
    finally:
        ctx.pop_scope()
    return ctx.result()
'''


def runtime_files():
    return [p for p in SRC.glob("*.py") if not p.name.startswith("test_") and not p.name.startswith("testkit_")]


class Fixture:
    """A temp scripts dir (copy of the trusted runtime) plus a snapshot root and manifest builder."""

    def __init__(self, test: unittest.TestCase, *, resolver: str | None = IDENTITY_RESOLVER, stub_python: bool = False, real: bool = False):
        self.tmp = Path(tempfile.mkdtemp(prefix="omp-srcidx-test-"))
        test.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        if real:
            self.scripts = SRC
        else:
            self.scripts = self.tmp / "scripts"
            self.scripts.mkdir()
            for p in runtime_files():
                if p.name == "resolve.py":
                    continue
                shutil.copy(p, self.scripts / p.name)
            if resolver is not None:
                (self.scripts / "resolve.py").write_text(resolver, encoding="utf-8")
            if stub_python:
                (self.scripts / "lang_python.py").write_text(STUB_PYTHON, encoding="utf-8")
        self.root = self.tmp / "snapshot"
        self.root.mkdir()
        self.out = self.tmp / "out"
        self.out.mkdir()
        self.entries: list[dict] = []

    def add(self, path: str, data: bytes | str, *, status: str = "pending", diagnostics=None, sha: str | None = None, size: int | None = None, write: bool = True):
        raw = data.encode("utf-8") if isinstance(data, str) else data
        if write:
            target = self.root.joinpath(*path.split("/"))
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(raw)
        self.entries.append(
            {
                "id": common.file_id(path),
                "path": path,
                "sha256": sha or hashlib.sha256(raw).hexdigest(),
                "bytes": len(raw) if size is None else size,
                "language": None,
                "status": status,
                "diagnostics": list(diagnostics or []),
            }
        )

    def manifest(self, directories=None) -> Path:
        m = {
            "schemaVersion": 1,
            "snapshotId": "snap-1",
            "folder": "C:/repo",
            "sourceHash": "abc",
            "root": str(self.root),
            "files": self.entries,
            "directories": directories or [],
            "limitations": ["manifest limitation"],
        }
        path = self.tmp / "manifest.json"
        path.write_text(json.dumps(m), encoding="utf-8")
        return path

    def run(self, *extra: str, timeout: float = 120.0, env: dict | None = None):
        output = self.out / "index.json"
        cmd = [sys.executable, "-I", "-B", str(self.scripts / "worker.py"), "extract", "--manifest", str(self.manifest()), "--output", str(output), *extra]
        full_env = dict(os.environ)
        if env:
            full_env.update(env)
        proc = subprocess.run(cmd, capture_output=True, timeout=timeout, cwd=str(self.tmp), env=full_env)
        index = json.loads(output.read_text(encoding="utf-8")) if output.exists() else None
        return proc, index


class FrozenRegistry(unittest.TestCase):
    def test_extension_mapping_is_exactly_the_plan(self):
        expected = {ext: lang for lang, exts in PLAN_EXTENSIONS.items() for ext in exts}
        self.assertEqual(profiles.EXTENSION_MAP, expected)
        self.assertEqual(profiles.LANGUAGE_IDS, frozenset(PLAN_EXTENSIONS))

    def test_lookup_is_case_exact_and_uses_final_extension(self):
        f = profiles.language_for_path
        self.assertEqual(f("a/b.F90"), "fortran")
        self.assertEqual(f("a/b.f90"), "fortran")
        self.assertEqual(f("a/b.F"), "fortran")
        self.assertIsNone(f("a/b.PY"))
        self.assertIsNone(f("a/b.Js"))
        self.assertEqual(f("types/x.d.ts"), "typescript")
        self.assertEqual(f("m.m"), "objc")
        self.assertEqual(f("m.mm"), "objc")
        self.assertEqual(f("build.gradle"), "groovy")
        self.assertIsNone(f("data.json"))
        self.assertIsNone(f(".py"))
        self.assertIsNone(f("Makefile"))
        self.assertEqual(f("dir.with.dots/Comp.svelte"), "svelte")

    def test_module_declared_extensions_match_frozen_mapping(self):
        reg = profiles.registry()
        for lang, entry in reg["profiles"].items():
            declared = entry["profile"].get("extensions", [])
            if declared:
                self.assertEqual(sorted(declared), sorted(PLAN_EXTENSIONS[lang]), f"{entry['moduleName']} declares wrong extensions for {lang}")

    def test_grammar_capsule_mapping_follows_the_allowlist(self):
        for lang, (module, capsule) in common.LANGUAGE_GRAMMARS.items():
            self.assertIn(module, common.GRAMMAR_ALLOWLIST, lang)
            self.assertIn(capsule, common.GRAMMAR_ALLOWLIST[module][1], lang)
        self.assertEqual(common.LANGUAGE_GRAMMARS["tsx"], ("tree_sitter_typescript", "language_tsx"))
        self.assertEqual(common.LANGUAGE_GRAMMARS["typescript"], ("tree_sitter_typescript", "language_typescript"))
        self.assertEqual(common.LANGUAGE_GRAMMARS["php"], ("tree_sitter_php", "language_php"))
        self.assertEqual(len(common.GRAMMAR_ALLOWLIST), 24)

    def test_non_allowlisted_grammar_is_refused(self):
        old = common.LANGUAGE_GRAMMARS.get("python")
        common.LANGUAGE_GRAMMARS["python"] = ("os", "language")
        try:
            with self.assertRaises(common.GrammarUnavailable) as ctx:
                common.load_language("python")
            self.assertIn("allowlisted", str(ctx.exception))
        finally:
            common.LANGUAGE_GRAMMARS["python"] = old
            common._LANGUAGE_ERRORS.pop(("os", "language"), None)

    def test_stable_ids_are_length_delimited(self):
        self.assertNotEqual(common.stable_id("a", "bc"), common.stable_id("ab", "c"))
        self.assertNotEqual(common.stable_id("a", ""), common.stable_id("a"))
        self.assertEqual(common.file_id("é/Ünï.py"), common.stable_id("file", "é/Ünï.py"))
        self.assertNotEqual(common.file_id("a.py"), common.file_id("A.py"))

    def test_unavailable_grammar_is_unsupported_with_diagnostic_not_success(self):
        old = common.LANGUAGE_GRAMMARS["python"]
        common.LANGUAGE_GRAMMARS["python"] = ("tree_sitter_not_installed_anywhere", "language")
        try:
            row = profiles.extract_file("a.py", b"def f(): pass\n")
            self.assertEqual(row["status"], "unsupported")
            self.assertEqual(row["symbols"], [])
            self.assertTrue(any("grammar unavailable" in d for d in row["diagnostics"]))
            probe = profiles.probe_profile("python")
            self.assertFalse(probe["available"])
            self.assertTrue(probe["error"])
        finally:
            common.LANGUAGE_GRAMMARS["python"] = old
            common._LANGUAGE_ERRORS.pop(("tree_sitter_not_installed_anywhere", "language"), None)

    def test_unmapped_and_binary_inputs_are_not_false_empty_parses(self):
        row = profiles.extract_file("README.md", b"# hi")
        self.assertEqual((row["status"], row["language"]), ("unsupported", None))
        self.assertEqual(profiles.extract_file("x.js", b"a\x00b")["status"], "skipped")

    def test_capabilities_cli_reports_every_profile(self):
        proc = subprocess.run([sys.executable, "-I", "-B", str(SRC / "worker.py"), "capabilities"], capture_output=True, timeout=120, cwd=tempfile.gettempdir())
        self.assertEqual(proc.returncode, 0, proc.stderr.decode())
        caps = json.loads(proc.stdout.decode("utf-8"))
        self.assertIn("python", caps["runtime"])
        self.assertIn("version", caps["runtime"]["treeSitter"])
        by = {p["language"]: p for p in caps["profiles"]}
        self.assertEqual(set(by), set(PLAN_EXTENSIONS))
        for rec in by.values():
            self.assertEqual(set(rec), {"language", "version", "available", "error", "nodeKinds"})
        for lang in ("python", "javascript", "typescript", "tsx", "svelte", "vue", "astro", "ejs"):
            self.assertTrue(by[lang]["available"], f"{lang}: {by[lang]['error']}")
            self.assertIsNone(by[lang]["error"], f"{lang}: {by[lang]['error']}")
        self.assertTrue(by["python"]["nodeKinds"])
        self.assertTrue(by["python"]["version"])


class ExtractCoordinator(unittest.TestCase):
    def test_mixed_repository_inventory_and_coverage(self):
        fx = Fixture(self)
        fx.add("src/app.py", "from .util import helper\n\ndef main():\n    helper()\n")
        fx.add("src/util.py", "def helper():\n    pass\n\ndef uncalled():\n    pass\n")
        fx.add("web/ünï/Menü.js", "export function menu() { return Item(); }\n")
        fx.add("web/page.ts", "export const n: number = 1;\n")
        fx.add("web/Comp.svelte", "<script>function s() { t(); }</script>\n<p>x</p>\n")
        fx.add("notes/README.md", "# notes\n")
        fx.add("img/logo.png", b"\x89PNG\x00\x00", status="skipped", diagnostics=["binary file; not parsed"])
        fx.add("src/broken.py", "def x(:\n")
        proc, index = fx.run()
        self.assertEqual(proc.returncode, 0, proc.stderr.decode())
        status = {f["path"]: f for f in index["files"]}
        self.assertEqual([f["path"] for f in index["files"]], [e["path"] for e in fx.entries])
        self.assertEqual(status["src/app.py"]["status"], "parsed")
        self.assertEqual(status["src/app.py"]["language"], "python")
        self.assertEqual(status["web/ünï/Menü.js"]["status"], "parsed")
        self.assertEqual(status["web/Comp.svelte"]["language"], "svelte")
        self.assertEqual(status["notes/README.md"]["status"], "unsupported")
        self.assertIsNone(status["notes/README.md"]["language"])
        self.assertEqual(status["img/logo.png"]["status"], "skipped")
        self.assertEqual(status["img/logo.png"]["diagnostics"], ["binary file; not parsed"])
        self.assertEqual(status["src/broken.py"]["status"], "partial")
        for entry in fx.entries:
            self.assertEqual(status[entry["path"]]["id"], entry["id"])
            self.assertEqual(status[entry["path"]]["sha256"], entry["sha256"])
        ids = {f["id"] for f in index["files"]}
        for s in index["symbols"] + index["sites"]:
            self.assertIn(s["fileId"], ids)
        names = {s["name"] for s in index["symbols"] if s["kind"] == "function"}
        self.assertTrue({"main", "helper", "uncalled", "menu", "s"} <= names)
        cov = index["coverage"]
        self.assertEqual(cov["admittedFiles"], 8)
        self.assertEqual(cov["parsedFiles"], 5)
        self.assertEqual(cov["partialFiles"], 1)
        self.assertEqual(cov["skippedFiles"], 1)
        self.assertEqual(cov["unsupportedFiles"], 1)
        self.assertEqual(cov["failedFiles"], 0)
        self.assertEqual(cov["encounteredSites"], len(index["sites"]))
        self.assertFalse(cov["partial"])
        self.assertEqual(cov["lastFullyProcessedFile"], "src/broken.py")
        langs = {row["language"]: row for row in cov["languages"]}
        self.assertEqual(langs["python"]["files"], 3)
        self.assertEqual(langs["python"]["partial"], 1)
        self.assertIn("unmapped", langs)
        self.assertEqual(index["schemaVersion"], 1)
        self.assertEqual((index["snapshotId"], index["folder"], index["sourceHash"]), ("snap-1", "C:/repo", "abc"))
        self.assertIn("python", index["extractorVersions"]["profiles"])
        self.assertTrue(any("Static syntax analysis" in x for x in index["limitations"]))
        self.assertIn("manifest limitation", index["limitations"])
        progress = json.loads((fx.out / "progress.json").read_text(encoding="utf-8"))
        self.assertEqual(progress["completedFiles"], 8)
        self.assertEqual(progress["totalFiles"], 8)
        self.assertEqual(progress["lastFullyProcessedFile"], "src/broken.py")

    def test_empty_manifest_and_directories_pass_through(self):
        fx = Fixture(self)
        proc, index = fx.run()
        self.assertEqual(proc.returncode, 0, proc.stderr.decode())
        self.assertEqual(index["files"], [])
        self.assertEqual(index["coverage"]["admittedFiles"], 0)
        self.assertIsNone(index["coverage"]["lastFullyProcessedFile"])
        fx2 = Fixture(self)
        fx2.add("a.py", "x = 1\n")
        proc, index = fx2.run()
        self.assertEqual(proc.returncode, 0)

    def test_changed_escaping_and_linked_files_fail_without_mixing_versions(self):
        fx = Fixture(self)
        fx.add("ok.py", "def ok(): pass\n")
        fx.add("changed.py", "def a(): pass\n", sha="0" * 64)
        fx.add("short.py", "def a(): pass\n", size=999)
        fx.add("../escape.py", "def e(): pass\n", write=False)
        outside = fx.tmp / "outside.py"
        outside.write_text("def secret(): pass\n", encoding="utf-8")
        link_made = False
        try:
            os.symlink(outside, fx.root / "linked.py")
            link_made = True
        except (OSError, NotImplementedError):
            pass
        if link_made:
            raw = outside.read_bytes()
            fx.entries.append({"id": common.file_id("linked.py"), "path": "linked.py", "sha256": hashlib.sha256(raw).hexdigest(), "bytes": len(raw), "language": None, "status": "pending", "diagnostics": []})
        proc, index = fx.run()
        self.assertEqual(proc.returncode, 0, proc.stderr.decode())
        st = {f["path"]: f for f in index["files"]}
        self.assertEqual(st["ok.py"]["status"], "parsed")
        self.assertEqual(st["changed.py"]["status"], "failed")
        self.assertTrue(any("hash" in d for d in st["changed.py"]["diagnostics"]))
        self.assertEqual(st["short.py"]["status"], "failed")
        self.assertEqual(st["../escape.py"]["status"], "failed")
        self.assertTrue(any("unsafe" in d for d in st["../escape.py"]["diagnostics"]))
        if link_made:
            self.assertEqual(st["linked.py"]["status"], "failed")
            self.assertTrue(any("link" in d for d in st["linked.py"]["diagnostics"]))
        self.assertNotIn("secret", {s["name"] for s in index["symbols"]})
        self.assertEqual({s["name"] for s in index["symbols"] if s["kind"] == "function"}, {"ok"})

    def test_snapshot_files_beyond_max_path_are_read_with_their_hash(self):
        fx = Fixture(self)
        rel = "/".join(["segment-" + "x" * 30] * 7 + ["deep.py"])
        raw = b"def deep_function():\n    return 1\n"
        target = common.extended_path(str(fx.root.joinpath(*rel.split("/"))))
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "wb") as fh:
            fh.write(raw)
        self.assertGreater(len(str(fx.root)) + len(rel) + 1, 260, "the fixture really exceeds MAX_PATH")
        fx.add(rel, raw, write=False)
        proc, index = fx.run()
        self.assertEqual(proc.returncode, 0, proc.stderr.decode())
        record = {f["path"]: f for f in index["files"]}[rel]
        self.assertEqual(record["status"], "parsed", record["diagnostics"])
        self.assertIn("deep_function", {s["name"] for s in index["symbols"]})

    def test_hard_timeout_and_crash_restart_the_parser_and_continue(self):
        fx = Fixture(self, stub_python=True)
        fx.add("a_ok.py", "x\n")
        fx.add("b_hang.py", "HANG\n")
        fx.add("c_ok.py", "x\n")
        fx.add("d_crash.py", "CRASH\n")
        fx.add("e_ok.py", "x\n")
        started = time.monotonic()
        proc, index = fx.run("--file-timeout", "1.5")
        self.assertEqual(proc.returncode, 0, proc.stderr.decode())
        self.assertLess(time.monotonic() - started, 60)
        st = {f["path"]: f for f in index["files"]}
        self.assertEqual([st[p]["status"] for p in ("a_ok.py", "b_hang.py", "c_ok.py", "d_crash.py", "e_ok.py")], ["parsed", "failed", "parsed", "failed", "parsed"])
        self.assertTrue(any("hard timeout" in d for d in st["b_hang.py"]["diagnostics"]))
        self.assertTrue(any("crashed" in d for d in st["d_crash.py"]["diagnostics"]))
        self.assertEqual(index["coverage"]["failedFiles"], 2)
        self.assertTrue(any("restarted 2" in x for x in index["limitations"]))
        self.assertEqual(sum(1 for s in index["symbols"] if s["kind"] == "function"), 3)

    def test_killing_the_coordinator_stops_its_parser_child(self):
        fx = Fixture(self, stub_python=True)
        fx.add("hang.py", "HANG\n")
        pid_file = fx.tmp / "child.pid"
        output = fx.out / "index.json"
        cmd = [sys.executable, "-I", "-B", str(fx.scripts / "worker.py"), "extract", "--manifest", str(fx.manifest()), "--output", str(output), "--file-timeout", "90"]
        env = dict(os.environ)
        env["OMP_TEST_PIDFILE"] = str(pid_file)
        coordinator = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=str(fx.tmp), env=env)
        try:
            deadline = time.monotonic() + 60
            while not pid_file.exists() or not pid_file.read_text().strip():
                self.assertLess(time.monotonic(), deadline, "child never started parsing")
                time.sleep(0.1)
            child_pid = int(pid_file.read_text().strip())
            self.assertTrue(_alive(child_pid))
        finally:
            coordinator.kill()
            coordinator.wait(timeout=30)
        deadline = time.monotonic() + 30
        while _alive(child_pid) and time.monotonic() < deadline:
            time.sleep(0.2)
        self.assertFalse(_alive(child_pid), "parser child outlived its killed coordinator")
        self.assertFalse(output.exists())

    def test_fact_cap_marks_partial_and_last_processed_file(self):
        fx = Fixture(self)
        for i in range(30):
            fx.add(f"m{i:02d}.py", "".join(f"def f{j}():\n    g{j}()\n" for j in range(5)))
        proc, index = fx.run("--max-output-bytes", "60000")
        self.assertEqual(proc.returncode, 0, proc.stderr.decode())
        st = [f for f in index["files"]]
        parsed = [f["path"] for f in st if f["status"] == "parsed"]
        skipped = [f for f in st if f["status"] == "skipped"]
        self.assertTrue(parsed and skipped)
        self.assertEqual(index["coverage"]["lastFullyProcessedFile"], parsed[-1])
        self.assertTrue(index["coverage"]["partial"])
        self.assertTrue(all(any("limit" in d for d in f["diagnostics"]) for f in skipped))
        self.assertEqual([f["path"] for f in st], sorted(f["path"] for f in st))
        self.assertEqual(max(parsed), parsed[-1])
        self.assertTrue(any("Serialized fact limit" in x for x in index["limitations"]))
        parsed_ids = {f["id"] for f in st if f["status"] == "parsed"}
        self.assertTrue(all(s["fileId"] in parsed_ids for s in index["symbols"]))

    def test_missing_resolver_is_an_explicit_failure(self):
        fx = Fixture(self, resolver=None)
        fx.add("a.py", "def f(): pass\n")
        proc, index = fx.run()
        self.assertEqual(proc.returncode, 2)
        self.assertIsNone(index)
        self.assertIn("resolver unavailable", proc.stderr.decode())

    def test_resolver_exception_is_an_explicit_failure(self):
        fx = Fixture(self, resolver="def resolve(index, root):\n    raise RuntimeError('boom')\n")
        fx.add("a.py", "def f(): pass\n")
        proc, index = fx.run()
        self.assertEqual(proc.returncode, 2)
        self.assertIsNone(index)
        self.assertIn("resolver failed", proc.stderr.decode())

    def test_resolver_edge_with_missing_endpoint_is_rejected(self):
        bad = "def resolve(index, root):\n    index['edges'].append({'id':'e','sourceId':'nope','targetId':'nope','relation':'calls','resolution':'resolvedStatic','evidenceIds':[]})\n    return index\n"
        fx = Fixture(self, resolver=bad)
        fx.add("a.py", "def f(): pass\n")
        proc, index = fx.run()
        self.assertEqual(proc.returncode, 2)
        self.assertIsNone(index)

    def test_manifest_id_must_match_stable_file_id(self):
        fx = Fixture(self)
        fx.add("a.py", "x = 1\n")
        fx.entries[0]["id"] = "deadbeef"
        proc, index = fx.run()
        self.assertEqual(proc.returncode, 2)
        self.assertIsNone(index)

    def test_real_resolver_runs_on_the_whole_index_after_parsing(self):
        if not (SRC / "resolve.py").exists():
            self.skipTest("resolve.py is not present yet")
        fx = Fixture(self, real=True)
        fx.add("pkg/__init__.py", "")
        fx.add("pkg/a.py", "from .b import helper\n\ndef main():\n    helper()\n")
        fx.add("pkg/b.py", "def helper():\n    pass\n")
        fx.add("web/x.ts", "import { y } from './y';\nexport function x() { y(); }\n")
        fx.add("web/y.ts", "export function y() {}\n")
        proc, index = fx.run()
        self.assertEqual(proc.returncode, 0, proc.stderr.decode())
        ids = {f["id"] for f in index["files"]} | {s["id"] for s in index["symbols"]}
        for e in index["edges"]:
            self.assertIn(e["sourceId"], ids)
            self.assertIn(e["targetId"], ids)
        cov = index["coverage"]
        self.assertEqual(cov["encounteredSites"], len(index["sites"]))
        self.assertEqual(cov["resolvedSites"], sum(1 for s in index["sites"] if s["resolution"] == "resolvedStatic"))


def _alive(pid: int) -> bool:
    if os.name == "nt":
        out = subprocess.run(["tasklist", "/FI", f"PID eq {pid}", "/NH"], capture_output=True, text=True).stdout
        return str(pid) in out
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


if __name__ == "__main__":
    unittest.main()
