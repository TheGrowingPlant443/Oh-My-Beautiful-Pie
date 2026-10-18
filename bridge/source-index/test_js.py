"""Hermetic fixtures for the JavaScript / TypeScript / TSX syntax recipes."""
from __future__ import annotations

import unittest

import common
import lang_js
import profiles
import testkit_syntax as kit


def run(lang, source, path=None):
    ext = {"javascript": "js", "typescript": "ts", "tsx": "tsx"}[lang]
    return kit.run(lang_js, lang, source, path or f"src/mod.{ext}")


def bindings(r, relation, **match):
    out = []
    for s in kit.sites(r, relation):
        b = s["binding"]
        if all(b.get(k) == v for k, v in match.items()):
            out.append(s)
    return out


class JsRecipes(unittest.TestCase):
    def test_node_kinds_exist_for_every_profile(self):
        for lang in ("javascript", "typescript", "tsx"):
            with self.subTest(lang=lang):
                if kit.grammar_status(lang):
                    self.skipTest(f"grammar unavailable for {lang}: {kit.grammar_status(lang)}")
                kit.assert_node_kinds(self, lang_js, lang)

    @kit.requires_grammar("javascript")
    def test_functions_methods_closures_and_ownership(self):
        r = run(
            "javascript",
            "function top() {\n"
            "  function inner() {}\n"
            "  return () => inner();\n"
            "}\n"
            "const named = function () {};\n"
            "const arrow = async (a, b = dflt()) => a;\n"
            "function* gen() { yield 1; }\n"
            "class K extends Base {\n"
            "  constructor() { super(); }\n"
            "  m() { this.n(); }\n"
            "  get g() { return 1; }\n"
            "  static s = () => 1;\n"
            "  #p() {}\n"
            "  static { init(); }\n"
            "}\n"
            "const o = { f() {}, g: function () {}, h: () => 1 };\n"
            "[1].map(x => x);\n",
        )
        top = kit.syms(r, "function", "top")[0]
        self.assertEqual(kit.syms(r, "function", "inner")[0]["ownerId"], top["id"])
        self.assertEqual(kit.syms(r, "function", "inner")[0]["qualifiedName"], "top.inner")
        arrow_in_top = [s for s in kit.syms(r, "lambda") if s["ownerId"] == top["id"]]
        self.assertEqual(len(arrow_in_top), 1)
        self.assertEqual(kit.syms(r, "function", "named")[0]["bindings"]["body"], True)
        self.assertTrue(kit.syms(r, "function", "arrow")[0]["bindings"]["arrow"])
        self.assertEqual(len(kit.syms(r, "function", "gen")), 1)
        k = kit.syms(r, "class", "K")[0]
        self.assertEqual(k["bindings"]["extends"], ["Base"])
        self.assertEqual(kit.syms(r, "constructor", "constructor")[0]["ownerId"], k["id"])
        self.assertEqual(kit.syms(r, "method", "m")[0]["qualifiedName"], "K.m")
        self.assertEqual(kit.syms(r, "accessor", "g")[0]["ownerId"], k["id"])
        field = kit.syms(r, "method", "s")[0]
        self.assertTrue(field["bindings"]["arrow"])
        self.assertEqual(kit.syms(r, "method", "#p")[0]["ownerId"], k["id"])
        static_block = [s for s in r["symbols"] if s["span"]["lineStart"] == 14 and s["kind"] == "function"]
        self.assertEqual(len(static_block), 1)
        self.assertEqual(kit.sites(r, "calls", "init")[0]["callerId"], static_block[0]["id"])
        names = {s["name"] for s in r["symbols"] if s["kind"] in ("method", "function")}
        self.assertTrue({"f", "g", "h"} <= names)
        self.assertEqual(kit.sites(r, "calls", "dflt")[0]["callerId"], kit.syms(r, "function", "arrow")[0]["id"])
        # no duplicate symbols from keyword tokens
        ids = [s["id"] for s in r["symbols"]]
        self.assertEqual(len(ids), len(set(ids)))

    @kit.requires_grammar("javascript")
    def test_call_new_member_optional_and_dynamic_callees(self):
        r = run(
            "javascript",
            "function f(o, k) {\n"
            "  plain();\n"
            "  o.m();\n"
            "  this.t();\n"
            "  a.b.c();\n"
            "  new Foo.Bar();\n"
            "  o?.opt();\n"
            "  o[k]();\n"
            "  o['lit']();\n"
            "  g()();\n"
            "  tag`x${1}`;\n"
            "}\n",
        )
        by = {(s["binding"]["name"], s["binding"]["kind"]): s for s in kit.sites(r, "calls")}
        self.assertEqual(by[("plain", "identifier")]["binding"]["receiver"], None)
        m = by[("m", "member")]["binding"]
        self.assertEqual((m["receiver"], m["receiverKind"]), ("o", "identifier"))
        self.assertEqual(by[("t", "member")]["binding"]["receiverKind"], "this")
        c = by[("c", "member")]["binding"]
        self.assertEqual((c["receiver"], c["receiverKind"]), ("a.b", "path"))
        new = by[("Bar", "new")]["binding"]
        self.assertEqual(new["receiver"], "Foo")
        opt = by[("opt", "member")]
        self.assertTrue(opt["conditional"] and opt["binding"]["optional"])
        self.assertEqual(by[("lit", "member")]["resolution"], "unresolved")
        dyn = [s for s in kit.sites(r, "calls") if s["binding"]["kind"] == "dynamic"]
        self.assertEqual({s["resolution"] for s in dyn}, {"dynamic"})
        self.assertEqual(len(dyn), 2)  # o[k]() and g()()
        tagged = by[("tag", "identifier")]["binding"]
        self.assertTrue(tagged.get("tagged"))

    @kit.requires_grammar("javascript")
    def test_recursion_uncalled_and_shadowing_facts(self):
        r = run(
            "javascript",
            "function rec(n) { return rec(n - 1); }\n"
            "function a() { b(); } function b() { a(); }\n"
            "function lonely() {}\n"
            "function shadow(rec) { rec(); }\n"
            "function reassigned() { helper = other; helper(); }\n"
            "function helper() {}\n",
        )
        self.assertEqual({s["name"] for s in r["symbols"] if s["kind"] == "function"}, {"rec", "a", "b", "lonely", "shadow", "reassigned", "helper"})
        self.assertEqual([d["kind"] for d in kit.declarations(r, "shadow", "rec")], ["parameter"])
        self.assertIn("assignment", {d["kind"] for d in kit.declarations(r, "reassigned", "helper")})
        mod_decl = {d["name"]: d for d in kit.declarations(r, None) if d["kind"] == "function"}
        self.assertEqual(mod_decl["helper"]["symbolId"], kit.syms(r, "function", "helper")[0]["id"])
        self.assertTrue(all(s["targetId"] is None for s in r["sites"]))

    @kit.requires_grammar("javascript")
    def test_block_scoping_of_let_const_versus_var(self):
        r = run("javascript", "function f() {\n  { const x = 1; let y = 2; var z = 3; }\n}\n")
        decls = {d["name"]: d for d in kit.declarations(r, "f")}
        f = kit.syms(r, "function", "f")[0]
        body_inner = (decls["x"]["scopeStart"], decls["x"]["scopeEnd"])
        self.assertEqual(body_inner, (decls["y"]["scopeStart"], decls["y"]["scopeEnd"]))
        self.assertEqual((decls["z"]["scopeStart"], decls["z"]["scopeEnd"]), (f["span"]["byteStart"], f["span"]["byteEnd"]))
        self.assertLess(decls["x"]["scopeEnd"] - decls["x"]["scopeStart"], decls["z"]["scopeEnd"] - decls["z"]["scopeStart"])
        self.assertFalse(decls["x"]["mutable"])
        self.assertTrue(decls["y"]["mutable"])

    @kit.requires_grammar("javascript")
    def test_esm_imports_and_exports(self):
        r = run(
            "javascript",
            "import d, { a as b, c } from './x.js';\n"
            "import * as ns from 'pkg';\n"
            "import './side.js';\n"
            "export { b as z, c };\n"
            "export { q as r } from './y.js';\n"
            "export * from './w.js';\n"
            "export * as nsx from './v.js';\n"
            "export default function foo() {}\n"
            "export const k = () => 1, j = 2;\n"
            "export class Cls {}\n",
        )
        imports = {(s["binding"]["specifier"], s["binding"]["imported"], s["binding"]["alias"], s["binding"]["default"], s["binding"]["namespace"], s["binding"]["sideEffect"]) for s in kit.sites(r, "imports")}
        self.assertEqual(
            imports,
            {
                ("./x.js", None, "d", True, False, False),
                ("./x.js", "a", "b", False, False, False),
                ("./x.js", "c", None, False, False, False),
                ("pkg", None, "ns", False, True, False),
                ("./side.js", None, None, False, False, True),
            },
        )
        re = {(s["binding"]["kind"], s["binding"]["specifier"], s["binding"]["imported"], s["binding"]["alias"], s["binding"]["namespace"]) for s in kit.sites(r, "reexports")}
        self.assertIn(("exportLocal", None, "b", "z", False), re)
        self.assertIn(("exportLocal", None, "c", "c", False), re)
        self.assertIn(("exportFrom", "./y.js", "q", "r", False), re)
        self.assertIn(("exportFrom", "./w.js", None, None, True), re)
        self.assertIn(("exportFrom", "./v.js", None, "nsx", True), re)
        mod = kit.syms(r, "module")[0]
        exports = {(e["exported"], e["default"], e["symbolId"] is not None) for e in mod["bindings"]["exports"]}
        self.assertIn(("default", True, True), exports)
        self.assertIn(("k", False, True), exports)
        self.assertIn(("j", False, False), exports)
        self.assertIn(("Cls", False, True), exports)
        self.assertIn(("z", False, False), exports)
        foo = kit.syms(r, "function", "foo")[0]
        self.assertTrue(foo["exported"] and foo["bindings"]["exportNames"] == ["default"])
        imp_sites = {s["id"] for s in kit.sites(r, "imports")}
        for d in kit.declarations(r, None):
            if d["kind"] == "import":
                self.assertIn(d["siteId"], imp_sites)
        self.assertEqual({d["name"] for d in kit.declarations(r, None) if d["kind"] == "import"}, {"d", "b", "c", "ns"})

    @kit.requires_grammar("javascript")
    def test_commonjs_require_recipes_and_exports(self):
        r = run(
            "javascript",
            "const fs = require('fs');\n"
            "const { a, b: c, ...rest } = require('./m');\n"
            "const w = require('./z').pick;\n"
            "const dyn = require(name);\n"
            "require('./side');\n"
            "const lazy = () => import('./lazy');\n"
            "const viaAwait = async () => { const { q } = await import('./aw'); };\n"
            "module.exports = { a, helper, inline() {}, k: function () {} };\n"
            "exports.e = () => 1;\n",
        )
        req = {(s["binding"]["kind"], s["binding"]["specifier"], s["binding"]["imported"], s["binding"]["alias"]) for s in kit.sites(r, "imports")}
        self.assertIn(("require", "fs", None, "fs"), req)
        self.assertIn(("require", "./m", "a", None), req)
        self.assertIn(("require", "./m", "b", "c"), req)
        self.assertIn(("require", "./z", "pick", "w"), req)
        self.assertIn(("require", None, None, "dyn"), req)
        self.assertIn(("require", "./side", None, None), req)
        self.assertIn(("dynamicImport", "./lazy", None, None), req)
        self.assertIn(("dynamicImport", "./aw", "q", None), req)
        nonlit = next(s for s in kit.sites(r, "imports") if s["binding"]["alias"] == "dyn")
        self.assertEqual(nonlit["resolution"], "dynamic")
        rest = next(s for s in kit.sites(r, "imports") if s["binding"].get("rest"))
        self.assertTrue(rest["binding"]["namespace"])
        lazy = next(s for s in kit.sites(r, "imports") if s["binding"]["specifier"] == "./lazy")
        self.assertTrue(lazy["conditional"])
        mod = kit.syms(r, "module")[0]
        exported = {e["exported"] for e in mod["bindings"]["exports"] if e.get("commonjs") or e["symbolId"]}
        self.assertTrue({"a", "helper", "inline", "k", "e"} <= exported)
        inline = kit.syms(r, "method", "inline")[0]
        self.assertTrue(inline["exported"])
        # require call itself is not double-counted as a call site
        self.assertEqual(kit.sites(r, "calls", "require"), [])

    @kit.requires_grammar("javascript")
    def test_iife_is_resolved_structurally(self):
        r = run("javascript", "(function () { inner(); })();\n(() => 1)();\n")
        iifes = [s for s in kit.sites(r, "calls") if s["binding"].get("iife")]
        self.assertEqual(len(iifes), 2)
        lam = {s["id"] for s in r["symbols"] if s["kind"] in ("lambda", "function")}
        for s in iifes:
            self.assertEqual(s["resolution"], "resolvedStatic")
            self.assertIn(s["targetId"], lam)
        fn = next(s for s in r["symbols"] if s["span"]["lineStart"] == 1 and s["kind"] == "lambda")
        self.assertEqual(kit.sites(r, "calls", "inner")[0]["callerId"], fn["id"])

    @kit.requires_grammar("javascript")
    def test_conditional_flags(self):
        r = run(
            "javascript",
            "function f(a) {\n"
            "  always();\n"
            "  if (a) { branch(); } else { other(); }\n"
            "  a && short();\n"
            "  a ? yes() : no();\n"
            "  for (const x of a) { looped(); }\n"
            "  switch (a) { case 1: sw(); }\n"
            "  try { tried(); } catch (e) { caught(); }\n"
            "  const cb = () => lazy();\n"
            "}\n",
        )
        cond = {s["binding"]["name"]: s["conditional"] for s in kit.sites(r, "calls")}
        self.assertFalse(cond["always"])
        for n in ("branch", "other", "short", "yes", "no", "looped", "sw", "caught"):
            self.assertTrue(cond[n], n)
        self.assertFalse(cond["tried"])
        self.assertFalse(cond["lazy"])  # inside its own function boundary

    @kit.requires_grammar("javascript")
    def test_jsx_renders_not_calls(self):
        r = run("javascript", "const A = () => <div><Foo /><Lib.Item/><b>x</b></div>;\n")
        rend = kit.sites(r, "renders")
        self.assertEqual({(s["binding"]["name"], s["binding"]["receiver"]) for s in rend}, {("Foo", None), ("Item", "Lib")})
        self.assertEqual([s for s in kit.sites(r, "calls") if s["binding"]["name"] in ("Foo", "Item")], [])

    @kit.requires_grammar("typescript")
    def test_typescript_type_only_imports_and_exports(self):
        r = run(
            "typescript",
            "import type { A } from './a';\n"
            "import { type B, C } from './bc';\n"
            "import type D from './d';\n"
            "import e = require('./e');\n"
            "export type { A };\n"
            "export type { T } from './t';\n"
            "let x: typeof import('./m');\n"
            "type Y = import('./n').Foo;\n"
            "export = zed;\n",
        )
        imp = {(s["binding"]["specifier"], s["binding"]["imported"]): s for s in kit.sites(r, "imports")}
        self.assertTrue(imp[("./a", "A")]["typeOnly"])
        self.assertTrue(imp[("./bc", "B")]["typeOnly"] and imp[("./bc", "B")]["binding"]["typeKind"] == "type")
        self.assertFalse(imp[("./bc", "C")]["typeOnly"])
        self.assertTrue(imp[("./d", None)]["typeOnly"])
        self.assertEqual(imp[("./e", None)]["binding"]["kind"], "require")
        m = imp[("./m", None)]
        self.assertTrue(m["typeOnly"] and m["binding"]["typeKind"] == "typeof" and not m["conditional"])
        n = imp[("./n", None)]
        self.assertTrue(n["typeOnly"] and n["binding"]["typeKind"] == "type")
        reexp = {(s["binding"]["kind"], s["binding"]["specifier"], s["binding"]["imported"]): s for s in kit.sites(r, "reexports")}
        self.assertTrue(reexp[("exportLocal", None, "A")]["typeOnly"])
        self.assertTrue(reexp[("exportFrom", "./t", "T")]["typeOnly"])
        self.assertTrue(reexp[("exportLocal", None, "zed")]["binding"]["exportEquals"])
        mod = kit.syms(r, "module")[0]
        self.assertTrue(any(e["typeOnly"] and e["exported"] == "A" for e in mod["bindings"]["exports"]))

    @kit.requires_grammar("typescript")
    def test_typescript_declarations_overloads_and_namespaces(self):
        r = run(
            "typescript",
            "interface I extends J { m(): void; new (): I; (): void }\n"
            "enum E { A = f() }\n"
            "type T = string;\n"
            "namespace N.M { export function q() {} }\n"
            "declare module 'mod' { function w(): void }\n"
            "function ov(a: string): void;\n"
            "function ov(a: any) {}\n"
            "abstract class Q implements I { abstract n(): void; get g() { return 1 } @dec() z() {} }\n"
            "export default class {}\n",
        )
        iface = kit.syms(r, "interface", "I")[0]
        self.assertEqual(iface["bindings"]["extends"], ["J"])
        self.assertFalse(kit.syms(r, "method", "m")[0]["bindings"]["body"])
        self.assertEqual(kit.sites(r, "calls", "f")[0]["callerId"], kit.syms(r, "enum", "E")[0]["id"])
        self.assertEqual(kit.syms(r, "type", "T")[0]["ownerId"], kit.syms(r, "module")[0]["id"])
        ns = kit.syms(r, "namespace", "N.M")[0]
        self.assertEqual(kit.syms(r, "function", "q")[0]["ownerId"], ns["id"])
        mod = kit.syms(r, "module")[0]
        self.assertNotIn("q", {e["exported"] for e in mod["bindings"].get("exports", [])})  # namespace member, not module export
        self.assertEqual(len(kit.syms(r, "signature", "ov")), 1)
        self.assertEqual(len(kit.syms(r, "function", "ov")), 1)
        ov_decls = [d for d in kit.declarations(r, None, "ov") if d["kind"] == "function"]
        self.assertEqual(len(ov_decls), 2)  # both overload and implementation declared: resolver must not pick one by name
        q = kit.syms(r, "class", "Q")[0]
        self.assertEqual(q["bindings"]["implements"], ["I"])
        self.assertTrue(q["bindings"]["abstract"])
        self.assertEqual(kit.syms(r, "accessor", "g")[0]["ownerId"], q["id"])
        dec = kit.sites(r, "calls", "dec")[0]
        self.assertEqual(dec["callerId"], q["id"])
        anon_default = [s for s in kit.syms(r, "class") if s["name"] == ""]
        self.assertTrue(anon_default[0]["exported"])

    @kit.requires_grammar("tsx")
    def test_tsx_parses_jsx_with_types(self):
        r = run("tsx", "export const A = ({ n }: { n: number }) => <Item n={n} onClick={() => go(n)} />;\n")
        self.assertEqual([s["binding"]["name"] for s in kit.sites(r, "renders")], ["Item"])
        self.assertEqual(len(kit.sites(r, "calls", "go")), 1)
        self.assertEqual(kit.syms(r, "function", "A")[0]["language"], "tsx")

    @kit.requires_grammar("javascript")
    def test_unicode_case_and_punctuation_names_stay_distinct(self):
        src = "function café() {}\nfunction Café() {}\nfunction $f() {}\nfunction _f() {}\nfunction 日本() {}\n"
        r = run("javascript", src)
        names = [s["name"] for s in r["symbols"] if s["kind"] == "function"]
        self.assertEqual(names, ["café", "Café", "$f", "_f", "日本"])
        self.assertEqual(len({s["id"] for s in r["symbols"]}), len(r["symbols"]))
        data = src.encode()
        j = kit.syms(r, "function", "日本")[0]
        self.assertEqual(data[j["span"]["byteStart"]:j["span"]["byteEnd"]], "function 日本() {}".encode())

    @kit.requires_grammar("javascript")
    def test_ids_deterministic_and_documented_hash(self):
        src = "function f() { g(); }\n"
        a, b = run("javascript", src), run("javascript", src)
        self.assertEqual([s["id"] for s in a["symbols"]], [s["id"] for s in b["symbols"]])
        self.assertEqual([s["id"] for s in a["sites"]], [s["id"] for s in b["sites"]])
        f = kit.syms(a, "function", "f")[0]
        sp = f["span"]
        self.assertEqual(f["id"], common.symbol_id("src/mod.js", "javascript", "main", "function_declaration", sp["byteStart"], sp["byteEnd"]))
        self.assertEqual(f["fileId"], common.file_id("src/mod.js"))
        self.assertNotEqual(kit.syms(run("javascript", src, "src/Mod.js"), "function", "f")[0]["id"], f["id"])

    @kit.requires_grammar("javascript")
    def test_syntax_errors_and_caps_report_partial(self):
        row = profiles.extract_file("bad.js", b"function ok() {}\nfunction broken( {\n")
        self.assertEqual(row["status"], "partial")
        self.assertTrue(any("syntax" in d for d in row["diagnostics"]))
        many = "\n".join(f"function f{i}() {{ g(); }}" for i in range(300)).encode()
        capped = profiles.extract_file("many.js", many, node_cap=100)
        self.assertEqual(capped["status"], "partial")
        self.assertTrue(any("node cap" in d for d in capped["diagnostics"]))


if __name__ == "__main__":
    unittest.main()
