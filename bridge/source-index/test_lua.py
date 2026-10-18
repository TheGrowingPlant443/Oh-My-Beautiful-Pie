"""Lua recipe fixtures.  Skipped with an explicit reason when the grammar is unavailable."""
import sys
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common  # noqa: E402
import lang_lua  # noqa: E402

LANG = "lua"
PROBE = common.language_probe(LANG, lang_lua.PROFILES[LANG]["node_kinds"])
SKIP = None if PROBE["available"] else f"GRAMMAR UNAVAILABLE ({LANG}): {PROBE['error']}"


def run(text, path="src/mod.lua"):
    data = text.encode("utf-8")
    ctx = common.Context(path, LANG, data, deadline=time.monotonic() + 20)
    lang_lua.extract(LANG, data, ctx)
    return ctx.result(), data


def by(items, **kw):
    return [i for i in items if all(i.get(k) == v for k, v in kw.items())]


def decls(sym):
    return [(d["name"], d["kind"]) for d in sym.get("bindings", {}).get("declarations", [])]


class GrammarProbe(unittest.TestCase):
    @unittest.skipIf(SKIP is not None, SKIP or "")
    def test_required_node_kinds_present(self):
        self.assertIsNone(PROBE["error"], PROBE["error"])


@unittest.skipIf(SKIP is not None, SKIP or "")
class LuaRecipe(unittest.TestCase):
    def test_function_forms(self):
        res, _ = run(
            "local M = {}\nfunction top(a) return a end\nlocal function loc() end\nfunction M.f(a) end\nfunction M:meth() self:other() end\n"
            "M.g = function(a) return a end\nlocal t = { cb = function() end }\nlocal h = function() end\nlocal function unused() end\n"
        )
        syms = res["symbols"]
        mod = by(syms, kind="module")[0]
        self.assertIsNone(mod["ownerId"])
        self.assertTrue(by(syms, kind="function", name="top")[0]["bindings"]["global"])
        self.assertFalse(by(syms, name="loc")[0]["bindings"]["global"] if "global" in by(syms, name="loc")[0]["bindings"] else False)
        f = by(syms, name="f")[0]
        self.assertEqual(f["qualifiedName"], "M.f")
        meth = by(syms, kind="method", name="meth")[0]
        self.assertEqual(meth["qualifiedName"], "M:meth")
        self.assertIn(("self", "parameter"), decls(meth))
        g = by(syms, name="g")[0]
        self.assertEqual(g["kind"], "function")
        self.assertEqual(g["bindings"]["assignedTo"], "M.g")
        self.assertTrue(by(syms, name="cb")[0]["bindings"]["tableField"])
        self.assertEqual(by(syms, name="h")[0]["kind"], "function")
        self.assertTrue(by(syms, name="unused"))  # uncalled survives
        d = decls(mod)
        self.assertIn(("top", "function"), d)
        self.assertIn(("loc", "function"), d)
        self.assertIn(("M", "variable"), d)
        self.assertEqual(len({s["id"] for s in syms}), len(syms))

    def test_requires(self):
        res, _ = run(
            'local M = require("mod.sub")\nlocal x = require(name)\nrequire [[lib.core]]\ndofile("x.lua")\n'
            'local ok = pcall(require, "opt")\nif c then require("cond") end\n'
        )
        imps = by(res["sites"], relation="imports")
        spec = {s["binding"]["specifier"]: s for s in imps}
        self.assertEqual(spec["mod.sub"]["binding"]["local"], "M")
        self.assertEqual(spec["mod.sub"]["binding"]["kind"], "require")
        self.assertEqual(spec["lib.core"]["binding"]["loader"], "require")
        self.assertTrue(spec["x.lua"]["binding"]["pathLike"])
        self.assertTrue(spec["opt"]["conditional"])
        self.assertTrue(spec["cond"]["conditional"])
        dyn = by(imps, resolution="dynamic")
        self.assertEqual(len(dyn), 1)
        self.assertIsNone(dyn[0]["binding"]["specifier"])
        self.assertEqual(dyn[0]["binding"]["local"], "x")
        mod = by(res["symbols"], kind="module")[0]
        self.assertIn(("M", "import"), decls(mod))

    def test_calls_bindings_dynamic_and_recursion(self):
        res, _ = run(
            "function fact(n)\n  if n < 2 then return 1 end\n  return n * fact(n - 1)\nend\n"
            "obj.a.b(3); M.f(2); M:meth(); f()(); t[1](2); load('x')(); top(1)\n"
            "function M:m() self:other() end\n"
        )
        calls = {s["raw"]: s for s in by(res["sites"], relation="calls")}
        fact = by(res["symbols"], name="fact")[0]
        self.assertEqual(calls["fact"]["callerId"], fact["id"])
        self.assertTrue(calls["fact"]["conditional"] is False)
        self.assertIsNone(calls["fact"]["targetId"])
        self.assertEqual(calls["obj.a.b"]["binding"]["receiver"], "obj.a")
        self.assertEqual(calls["M.f"]["binding"]["kind"], "member")
        self.assertEqual(calls["M:meth"]["binding"]["method"], True)
        self.assertEqual(calls["self:other"]["binding"]["receiverKind"], "self")
        self.assertEqual(calls["t[1]"]["resolution"], "dynamic")
        self.assertTrue(any(s["resolution"] == "dynamic" for s in res["sites"] if s["raw"] == "f()"))
        self.assertEqual(calls["load"]["resolution"], "dynamic")
        self.assertEqual(calls["top"]["resolution"], "unresolved")

    def test_shadowing_and_rebinding_declared(self):
        res, _ = run(
            "local function helper() end\nlocal function outer()\n  local helper = 1\n  helper()\nend\n"
            "local v = 1\nv = 2\ng = 3\nhelper()\n"
        )
        mod = by(res["symbols"], kind="module")[0]
        outer = by(res["symbols"], name="outer")[0]
        d_mod = decls(mod)
        self.assertIn(("helper", "function"), d_mod)
        self.assertIn(("v", "variable"), d_mod)
        self.assertIn(("v", "assignment"), d_mod)   # rebinding evidence
        self.assertIn(("g", "assignment"), d_mod)   # global assignment
        self.assertIn(("helper", "variable"), decls(outer))
        inner = [s for s in res["sites"] if s["raw"] == "helper"]
        self.assertEqual(len(inner), 2)
        self.assertEqual(inner[0]["callerId"], outer["id"])
        # the shadowing declaration range contains the inner call but not the outer one
        sh = [d for d in outer["bindings"]["declarations"] if d["name"] == "helper"][0]
        self.assertTrue(sh["scopeStart"] <= inner[0]["span"]["byteStart"] < sh["scopeEnd"])
        self.assertFalse(sh["scopeStart"] <= inner[1]["span"]["byteStart"] < sh["scopeEnd"])

    def test_loops_conditional_unicode_case(self):
        res, data = run(
            "for i = 1, 3 do print(i) end\nfor k, v in pairs(t) do f(k) end\nif x then y() end\nz()\n"
            "function café() end\nfunction Café() end\nfunction cafe() end\n"
        )
        calls = {s["raw"]: s for s in by(res["sites"], relation="calls")}
        self.assertTrue(calls["y"]["conditional"])
        self.assertFalse(calls["z"]["conditional"])
        self.assertTrue(calls["print"]["conditional"])
        mod = by(res["symbols"], kind="module")[0]
        self.assertIn(("i", "variable"), [(d["name"], d["kind"]) for d in mod["bindings"]["declarations"]])
        names = [s["name"] for s in res["symbols"] if s["kind"] == "function"]
        self.assertEqual(sorted(names), ["Café", "cafe", "café"])
        cafe = by(res["symbols"], name="café")[0]
        sp = cafe["span"]
        self.assertEqual(data[sp["byteStart"]:sp["byteEnd"]].decode(), "function café() end")

    def test_syntax_error_is_partial(self):
        bad, _ = run("function (\nlocal = \n")
        self.assertTrue(bad["partial"])


@unittest.skipIf(SKIP is not None, SKIP or "")
class LuaRobustness(unittest.TestCase):
    def test_many_locals_and_rebindings(self):
        text = "\n".join(f"local x{i} = {i}\nx{i} = x{i} + 1\ng{i} = 2" for i in range(3000))
        res, _ = run(text)
        mod = by(res["symbols"], kind="module")[0]
        kinds = [d["kind"] for d in mod["bindings"]["declarations"]]
        self.assertEqual(kinds.count("variable"), 3000)
        self.assertEqual(kinds.count("assignment"), 6000)
        self.assertFalse(res["partial"])


if __name__ == "__main__":
    unittest.main()
