"""Hermetic fixtures for the Python syntax recipes (real Tree-sitter parsing, no repo code executed)."""
from __future__ import annotations

import unittest

import common
import lang_python
import profiles
import testkit_syntax as kit

PY = "python"


@kit.requires_grammar(PY)
class PythonRecipes(unittest.TestCase):
    def run_py(self, source, path="pkg/mod.py"):
        return kit.run(lang_python, PY, source, path)

    def test_declared_node_kinds_exist(self):
        kit.assert_node_kinds(self, lang_python, PY)

    def test_declarations_nesting_and_kinds(self):
        r = self.run_py(
            "def top():\n"
            "    def inner():\n"
            "        pass\n"
            "    return lambda: inner()\n"
            "class K:\n"
            "    def __init__(self): pass\n"
            "    def m(self): pass\n"
            "    @property\n"
            "    def p(self): return 1\n"
            "    @staticmethod\n"
            "    def s(): pass\n"
            "async def co(): pass\n"
        )
        top = kit.syms(r, "function", "top")[0]
        inner = kit.syms(r, "function", "inner")[0]
        self.assertEqual(inner["ownerId"], top["id"])
        self.assertEqual(inner["qualifiedName"], "top.inner")
        lam = kit.syms(r, "lambda")[0]
        self.assertEqual(lam["ownerId"], top["id"])
        self.assertEqual(lam["name"], "")
        k = kit.syms(r, "class", "K")[0]
        self.assertEqual(kit.syms(r, "constructor", "__init__")[0]["ownerId"], k["id"])
        self.assertEqual(kit.syms(r, "method", "m")[0]["qualifiedName"], "K.m")
        self.assertEqual(kit.syms(r, "accessor", "p")[0]["ownerId"], k["id"])
        self.assertTrue(kit.syms(r, "method", "s")[0]["bindings"]["static"])
        self.assertTrue(kit.syms(r, "function", "co")[0]["bindings"]["async"])
        mod = kit.syms(r, "module")[0]
        self.assertIsNone(mod["ownerId"])
        self.assertEqual(top["ownerId"], mod["id"])
        exports = {e["exported"] for e in mod["bindings"]["exports"]}
        self.assertEqual(exports, {"top", "K", "co"})

    def test_uncalled_isolated_and_recursive_are_kept(self):
        r = self.run_py(
            "def lonely(): pass\n"
            "def rec(n): return rec(n - 1)\n"
            "def ping(): pong()\n"
            "def pong(): ping()\n"
        )
        self.assertEqual({s["name"] for s in r["symbols"] if s["kind"] == "function"}, {"lonely", "rec", "ping", "pong"})
        callers = {kit.caller_name(r, s): s["binding"]["name"] for s in r["sites"] if s["relation"] == "calls"}
        self.assertEqual(callers, {"rec": "rec", "ping": "pong", "pong": "ping"})
        # facts never claim a target without the resolver
        self.assertTrue(all(s["targetId"] is None and s["resolution"] == "unresolved" for s in r["sites"]))

    def test_same_name_case_and_unicode_ids_do_not_merge(self):
        r = self.run_py(
            "def run(): pass\n"
            "def Run(): pass\n"
            "def run_(): pass\n"
            "def é(): pass\n"
            "class A:\n"
            "    def run(self): pass\n"
            "class B:\n"
            "    def run(self): pass\n"
        )
        ids = [s["id"] for s in r["symbols"]]
        self.assertEqual(len(ids), len(set(ids)))
        names = [s["name"] for s in r["symbols"] if s["kind"] in ("function", "method")]
        self.assertEqual(sorted(names), sorted(["run", "Run", "run_", "é", "run", "run"]))
        e = kit.syms(r, "function", "é")[0]
        data = "def run(): pass\ndef Run(): pass\ndef run_(): pass\ndef é(): pass\n".encode()
        self.assertEqual(data[e["span"]["byteStart"]:e["span"]["byteEnd"]], "def é(): pass".encode())
        self.assertEqual(e["span"]["lineStart"], 4)
        self.assertEqual(e["span"]["columnStart"], 0)

    def test_symbol_ids_are_the_documented_length_delimited_hash_and_deterministic(self):
        src = "def f():\n    return 1\n"
        a = self.run_py(src)
        b = self.run_py(src)
        self.assertEqual([s["id"] for s in a["symbols"]], [s["id"] for s in b["symbols"]])
        f = kit.syms(a, "function", "f")[0]
        sp = f["span"]
        self.assertEqual(f["id"], common.symbol_id("pkg/mod.py", PY, "main", "function_definition", sp["byteStart"], sp["byteEnd"]))
        other_path = self.run_py(src, path="pkg/Mod.py")
        self.assertNotEqual(kit.syms(other_path, "function", "f")[0]["id"], f["id"])
        self.assertNotEqual(common.stable_id("ab", "c"), common.stable_id("a", "bc"))

    def test_call_binding_receiver_kinds(self):
        r = self.run_py(
            "class K(Base):\n"
            "    def m(self, other):\n"
            "        self.a()\n"
            "        other.b()\n"
            "        super().c()\n"
            "        pkg.sub.d()\n"
            "        make().e()\n"
            "        free()\n"
        )
        got = {s["binding"]["name"]: (s["binding"]["receiver"], s["binding"]["receiverKind"], s["binding"]["kind"]) for s in kit.sites(r, "calls") if s["binding"]["name"] != "super"}
        self.assertEqual(got["a"], ("self", "self", "member"))
        self.assertEqual(got["b"], ("other", "identifier", "member"))
        self.assertEqual(got["c"], ("super()", "super", "member"))
        self.assertEqual(got["d"], ("pkg.sub", "path", "member"))
        self.assertEqual(got["e"], ("make()", "expression", "member"))
        self.assertEqual(got["free"], (None, None, "identifier"))
        ext = kit.sites(r, "references")[0]
        self.assertEqual(ext["binding"]["role"], "extends")
        self.assertEqual(kit.syms(r, "class", "K")[0]["bindings"]["extends"], ["Base"])

    def test_dynamic_callees_are_dynamic_never_named(self):
        r = self.run_py("handlers['x']()\nf()()\ngetattr(o, 'n')()\n")
        dyn = [s for s in r["sites"] if s["resolution"] == "dynamic"]
        self.assertEqual(len(dyn), 3)
        self.assertTrue(all(s["binding"]["name"] is None for s in dyn))

    def test_imports_recipes(self):
        r = self.run_py(
            "from ..pkg import a as b, c\n"
            "from . import x\n"
            "from m import *\n"
            "import a.b.c, d as e\n"
            "from __future__ import annotations\n"
            "from typing import TYPE_CHECKING\n"
            "if TYPE_CHECKING:\n"
            "    from t import T\n"
        )
        imps = [s["binding"] for s in kit.sites(r, "imports")]
        spec = lambda sp, imp: next(b for b in imps if b["specifier"] == sp and b["imported"] == imp)  # noqa: E731
        self.assertEqual(spec("..pkg", "a")["alias"], "b")
        self.assertEqual(spec("..pkg", "c")["local"], "c")
        self.assertEqual(spec(".", "x")["specifier"], ".")
        wild = next(b for b in imps if b["specifier"] == "m")
        self.assertTrue(wild["namespace"] and wild["imported"] is None)
        dotted = next(b for b in imps if b["specifier"] == "a.b.c")
        self.assertEqual((dotted["alias"], dotted["local"]), (None, "a"))
        aliased = next(b for b in imps if b["specifier"] == "d")
        self.assertEqual(aliased["alias"], "e")
        self.assertTrue(any(b["specifier"] == "__future__" for b in imps))
        t = next(s for s in kit.sites(r, "imports") if s["binding"]["specifier"] == "t")
        self.assertTrue(t["typeOnly"] and t["binding"]["typeKind"] == "type" and t["conditional"])
        decl_names = {d["name"] for d in kit.declarations(r, None)}
        self.assertTrue({"b", "c", "x", "a", "e", "T"} <= decl_names)
        imp_decl = next(d for d in kit.declarations(r, None, "b"))
        self.assertTrue(imp_decl["siteId"] in {s["id"] for s in r["sites"]})

    def test_dynamic_import_calls(self):
        r = self.run_py(
            "import importlib\n"
            "importlib.import_module('lit.mod')\n"
            "importlib.import_module(name)\n"
            "__import__('other')\n"
            "importlib.import_module('.rel')\n"
        )
        dyn = [s for s in kit.sites(r, "imports") if s["binding"]["kind"] == "dynamicImport"]
        self.assertEqual(len(dyn), 4)
        by_raw = {s["raw"]: s for s in dyn}
        self.assertEqual(by_raw["lit.mod"]["binding"]["specifier"], "lit.mod")
        self.assertEqual(by_raw["lit.mod"]["resolution"], "unresolved")
        self.assertIsNone(by_raw["name"]["binding"]["specifier"])
        self.assertEqual(by_raw["name"]["resolution"], "dynamic")
        self.assertEqual(by_raw["other"]["binding"]["specifier"], "other")
        self.assertEqual(by_raw[".rel"]["resolution"], "dynamic")  # relative needs package context

    def test_shadowing_and_rebinding_evidence(self):
        r = self.run_py(
            "def helper(): pass\n"
            "def caller(helper):\n"
            "    helper()\n"
            "def other():\n"
            "    helper = 1\n"
            "    helper()\n"
            "def gl():\n"
            "    global helper\n"
            "    helper = lambda: 2\n"
        )
        params = kit.declarations(r, "caller", "helper")
        self.assertEqual([d["kind"] for d in params], ["parameter"])
        local = kit.declarations(r, "other", "helper")
        self.assertEqual([d["kind"] for d in local], ["variable"])
        gl = {d["kind"] for d in kit.declarations(r, "gl", "helper")}
        self.assertIn("global", gl)
        self.assertIn("assignment", gl)
        self.assertNotIn("variable", gl)

    def test_class_body_names_are_not_lexically_declared(self):
        r = self.run_py("class K:\n    x = 1\n    def meth(self): pass\n")
        k = kit.syms(r, "class", "K")[0]
        self.assertFalse((k.get("bindings") or {}).get("declarations"))

    def test_conditional_flags(self):
        r = self.run_py(
            "def f(a):\n"
            "    always()\n"
            "    if a:\n"
            "        branch()\n"
            "    x = other() if a else third()\n"
            "    a and short()\n"
            "    try:\n"
            "        tried()\n"
            "    except E:\n"
            "        handled()\n"
            "    for i in a:\n"
            "        looped()\n"
            "    return [item() for item in a]\n"
        )
        cond = {s["binding"]["name"]: s["conditional"] for s in kit.sites(r, "calls")}
        self.assertFalse(cond["always"])
        self.assertTrue(cond["branch"])
        self.assertTrue(cond["third"])
        self.assertTrue(cond["other"])  # consequence branch of the conditional expression
        self.assertTrue(cond["short"])
        self.assertFalse(cond["tried"])
        self.assertTrue(cond["handled"])
        self.assertTrue(cond["looped"])
        self.assertTrue(cond["item"])

    def test_defaults_and_decorators_belong_to_enclosing_scope(self):
        r = self.run_py("def outer():\n    @deco()\n    def f(x=default()):\n        return body()\n")
        outer = kit.syms(r, "function", "outer")[0]
        f = kit.syms(r, "function", "f")[0]
        owners = {s["binding"]["name"]: s["callerId"] for s in kit.sites(r, "calls")}
        self.assertEqual(owners["deco"], outer["id"])
        self.assertEqual(owners["default"], outer["id"])
        self.assertEqual(owners["body"], f["id"])

    def test_iife_lambda_is_structurally_resolved(self):
        r = self.run_py("(lambda: 1)()\nf = lambda: g()\n")
        iife = next(s for s in kit.sites(r, "calls") if s["binding"].get("iife"))
        lam = next(s for s in r["symbols"] if s["name"] == "" and s["kind"] == "lambda")
        self.assertEqual((iife["resolution"], iife["targetId"]), ("resolvedStatic", lam["id"]))
        named = kit.syms(r, "function", "f")[0]
        self.assertEqual(named["bindings"]["body"], True)
        inner = kit.sites(r, "calls", "g")[0]
        self.assertEqual(inner["callerId"], named["id"])

    def test_all_names_recorded(self):
        r = self.run_py("__all__ = ['a', 'b']\n__all__ += ['c']\ndef a(): pass\n")
        mod = kit.syms(r, "module")[0]
        self.assertEqual(mod["bindings"]["all"], ["a", "b", "c"])


@kit.requires_grammar(PY)
class PythonViaProfiles(unittest.TestCase):
    def test_syntax_error_is_partial_with_diagnostic(self):
        row = profiles.extract_file("bad.py", b"def ok(): pass\ndef broken(:\n")
        self.assertEqual(row["status"], "partial")
        self.assertTrue(any("syntax" in d for d in row["diagnostics"]))
        self.assertEqual([s["name"] for s in row["symbols"] if s["kind"] == "function"][:1], ["ok"])

    def test_invalid_utf8_is_partial_but_offsets_exact(self):
        data = b"# \xff\xfe latin\ndef f(): pass\n"
        row = profiles.extract_file("lat.py", data)
        self.assertEqual(row["status"], "partial")
        f = next(s for s in row["symbols"] if s["name"] == "f")
        self.assertEqual(data[f["span"]["byteStart"]:f["span"]["byteEnd"]], b"def f(): pass")

    def test_node_cap_keeps_collected_facts_as_partial(self):
        src = "\n".join(f"def f{i}(): pass" for i in range(200)).encode()
        row = profiles.extract_file("many.py", src, node_cap=60)
        self.assertEqual(row["status"], "partial")
        self.assertTrue(any("node cap" in d for d in row["diagnostics"]))
        self.assertTrue(0 < len([s for s in row["symbols"] if s["kind"] == "function"]) < 200)

    def test_expired_deadline_is_reported_not_swallowed(self):
        import time

        src = "\n".join(f"def f{i}(): g()" for i in range(500)).encode()
        row = profiles.extract_file("slow.py", src, deadline=time.monotonic() - 1)
        self.assertEqual(row["status"], "failed")
        self.assertTrue(row["diagnostics"])

    def test_binary_and_oversized_are_skipped(self):
        self.assertEqual(profiles.extract_file("x.py", b"a\x00b")["status"], "skipped")
        big = b"#" * (common.MAX_FILE_BYTES + 1)
        self.assertEqual(profiles.extract_file("big.py", big)["status"], "skipped")


if __name__ == "__main__":
    unittest.main()
