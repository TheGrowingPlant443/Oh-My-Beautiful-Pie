"""Swift recipe fixtures.  Skipped with an explicit reason when the grammar is unavailable."""
import sys
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common  # noqa: E402
import lang_swift  # noqa: E402

LANG = "swift"
PROBE = common.language_probe(LANG, lang_swift.PROFILES[LANG]["node_kinds"])
SKIP = None if PROBE["available"] else f"GRAMMAR UNAVAILABLE ({LANG}): {PROBE['error']}"


def run(text, path="Sources/App/main.swift"):
    data = text.encode("utf-8")
    ctx = common.Context(path, LANG, data, deadline=time.monotonic() + 20)
    lang_swift.extract(LANG, data, ctx)
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
class SwiftRecipe(unittest.TestCase):
    def test_declarations(self):
        res, _ = run(
            "class A: B, P {\n  init(x: Int) { super.init() }\n  deinit { cleanup() }\n  func m(_ a: Int) -> Int { return helper(a) + self.other(a) }\n"
            "  subscript(i: Int) -> Int { get { return i } set { store(newValue) } }\n  var computed: Int { return 1 }\n  static func s() { A.make() }\n}\n"
            "func top() { let f = { (x: Int) in x + 1 }; }\nprotocol P { func req() }\nextension A { func ext() {} }\n"
            "actor Act { func a() {} }\nenum E { case a; func e() {} }\nstruct S { func sm() {} }\n"
        )
        syms = res["symbols"]
        a = by(syms, kind="class", name="A")[0]
        self.assertEqual(a["bindings"]["extends"], ["B", "P"])
        self.assertEqual(by(syms, kind="constructor")[0]["ownerId"], a["id"])
        self.assertEqual(by(syms, name="m")[0]["kind"], "method")
        self.assertTrue(by(syms, name="s")[0]["bindings"]["static"])
        self.assertEqual(by(syms, kind="function", name="top")[0]["ownerId"], by(syms, kind="module")[0]["id"])
        self.assertEqual(by(syms, kind="interface", name="P")[0]["bindings"]["declarationKind"], "protocol")
        self.assertFalse(by(syms, name="req")[0]["bindings"]["body"])
        ext = by(syms, kind="impl")[0]
        self.assertEqual(ext["bindings"]["implFor"], "A")
        self.assertEqual(by(syms, name="ext")[0]["ownerId"], ext["id"])
        self.assertTrue(by(syms, kind="class", name="Act"))
        self.assertTrue(by(syms, kind="enum", name="E"))
        self.assertTrue(by(syms, kind="struct", name="S"))
        self.assertTrue(by(syms, name="deinit"))
        self.assertTrue(by(syms, name="subscript"))
        accessors = by(syms, kind="accessor")
        self.assertEqual(sorted(x["name"] for x in accessors), ["computed", "get", "set"])
        self.assertIn(("newValue", "parameter"), decls(by(accessors, name="set")[0]))
        lam = by(syms, kind="lambda")[0]
        self.assertEqual(lam["bindings"]["assignedTo"], "f")
        self.assertIn(("x", "parameter"), decls(lam))
        self.assertEqual(len({s["id"] for s in syms}), len(syms))

    def test_imports(self):
        res, _ = run("import Foundation\nimport struct Foo.Bar\n@testable import Baz\n")
        imps = by(res["sites"], relation="imports")
        spec = {s["binding"]["specifier"]: s["binding"] for s in imps}
        self.assertTrue(spec["Foundation"]["namespace"])
        self.assertEqual(spec["Foo.Bar"]["typeKind"], "struct")
        self.assertEqual(spec["Foo.Bar"]["imported"], "Bar")
        self.assertIn("Baz", spec)
        self.assertEqual(spec["Baz"]["attributes"], ["@testable"])

    def test_calls_bindings_recursion_unresolved(self):
        res, _ = run(
            "class A {\n  func m(_ a: Int) -> Int { return m(a - 1) + self.other(a) + helper(a) + obj?.go() + super.base() }\n}\n"
            "let v = Foo<Int>(x: 1)\nlet w = A.make()\nfoo { $0 }\nlet r = #stringify(1)\nf()()\n"
        )
        m = by(res["symbols"], name="m")[0]
        calls = {s["raw"]: s for s in by(res["sites"], relation="calls")}
        self.assertEqual(calls["m"]["callerId"], m["id"])
        self.assertIsNone(calls["m"]["targetId"])
        self.assertEqual(calls["m"]["resolution"], "unresolved")
        self.assertEqual(calls["self.other"]["binding"]["receiverKind"], "self")
        self.assertEqual(calls["helper"]["binding"]["kind"], "identifier")
        self.assertTrue(calls["obj?.go"]["binding"].get("optional"))
        self.assertEqual(calls["super.base"]["binding"]["receiverKind"], "super")
        self.assertEqual(calls["Foo<Int>"]["binding"]["kind"], "new")
        self.assertEqual(calls["Foo<Int>"]["binding"]["name"], "Foo")
        self.assertEqual(calls["A.make"]["binding"]["receiver"], "A")
        self.assertEqual(calls["foo"]["binding"]["kind"], "identifier")  # trailing closure call
        macro = by(res["sites"], relation="macros")[0]
        self.assertEqual(macro["binding"]["name"], "stringify")
        self.assertTrue(any(s["resolution"] == "dynamic" for s in res["sites"]))
        for s in res["sites"]:
            self.assertIsNone(s["targetId"])

    def test_locals_shadowing_conditional_unicode(self):
        res, data = run(
            "func helper() {}\nfunc outer() {\n  let helper = { 1 }\n  helper()\n  if let q = opt { z(q) }\n}\nhelper()\n"
            "#if DEBUG\ndbg()\n#endif\nfunc café() {}\nfunc Café() {}\nfunc cafe() {}\n"
        )
        mod = by(res["symbols"], kind="module")[0]
        outer = by(res["symbols"], name="outer")[0]
        self.assertIn(("helper", "function"), decls(mod))
        self.assertIn(("helper", "variable"), decls(outer))
        self.assertIn(("q", "variable"), decls(outer))
        helpers = by(res["sites"], raw="helper")
        self.assertEqual(len(helpers), 2)
        sh = [d for d in outer["bindings"]["declarations"] if d["name"] == "helper"][0]
        self.assertTrue(sh["scopeStart"] <= helpers[0]["span"]["byteStart"] < sh["scopeEnd"])
        self.assertFalse(sh["scopeStart"] <= helpers[1]["span"]["byteStart"] < sh["scopeEnd"])
        self.assertTrue(by(res["sites"], raw="z")[0]["conditional"])
        self.assertTrue(by(res["sites"], raw="dbg")[0]["conditional"])
        self.assertFalse(by(res["sites"], raw="helper")[1]["conditional"])
        names = sorted(s["name"] for s in res["symbols"] if s["name"] in ("café", "Café", "cafe"))
        self.assertEqual(names, ["Café", "cafe", "café"])
        cafe = by(res["symbols"], name="café")[0]
        sp = cafe["span"]
        self.assertEqual(data[sp["byteStart"]:sp["byteEnd"]].decode(), "func café() {}")

    def test_syntax_error_is_partial(self):
        bad, _ = run("func broken( { call(")
        self.assertTrue(bad["partial"])


if __name__ == "__main__":
    unittest.main()
