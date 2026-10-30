import unittest

import common
import lang_rust
from testkit_syntax import (assert_node_kinds, by_id, caller_name, declarations, requires_grammar, run, sites, syms)

SRC = '''use std::collections::{HashMap, hash_map::Entry as E};
use crate::a::*;
use self::b as bb;
pub use super::c::D;
use foo::{self, bar as _};
mod inner;
#[path = "other/x.rs"]
mod routed;
mod inline { pub fn q() {} }
extern crate alloc as al;
#[cfg(test)]
mod tests { fn t() { helper(); } }
trait Tr { fn sig(&self); fn dflt(&self) { self.sig() } }
struct S;
impl S {
    pub fn new() -> Self { S }
    fn m(&self) {
        let c = |x| x + 1;
        c(1);
        Self::new();
        println!("hi");
        inner::f();
        self.m()
    }
}
impl Tr for S { fn sig(&self) {} }
fn main() { S::new().m(); let v = vec![1]; fn nested() {} nested(); a::b::c(); (get())(); Vec::<u8>::new(); }
fn shadow(helper: fn()) { helper(); }
fn ping() { pong() }
fn pong() { ping() }
fn unused() {}
fn Größe() {}
fn größe() {}
extern "C" { fn ext(x: i32); }
macro_rules! mm { () => { foo() } }
'''


@requires_grammar("rust")
class RustRecipeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.r = run(lang_rust, "rust", SRC, "src/lib.rs")

    def test_profile_node_kinds_present_in_grammar(self):
        assert_node_kinds(self, lang_rust, "rust")

    def test_declarations(self):
        r = self.r
        fns = {s["qualifiedName"] for s in syms(r, "function")}
        self.assertLessEqual({"inline::q", "main", "main::nested", "shadow", "ping", "pong", "unused", "Größe", "größe"}, fns)
        methods = {s["qualifiedName"]: s for s in syms(r, "method")}
        self.assertEqual(set(methods), {"Tr::dflt", "S::new", "S::m", "S::sig"})
        self.assertTrue(methods["S::new"]["bindings"]["static"])
        self.assertTrue(methods["S::m"]["bindings"]["hasSelf"])
        self.assertEqual(methods["S::sig"]["bindings"]["implTrait"], "Tr")
        self.assertEqual(methods["S::new"]["bindings"]["implFor"], "S")
        sigs = {s["qualifiedName"]: s for s in syms(r, "signature")}
        self.assertEqual(set(sigs), {"Tr::sig", "ext"})
        self.assertEqual(sigs["ext"]["bindings"]["abi"], "C")
        self.assertEqual(len(syms(r, "impl")), 2)
        self.assertEqual(syms(r, "macro")[0]["name"], "mm")
        self.assertEqual(len(syms(r, "module")), 1)
        self.assertTrue(syms(r, "function", "q")[0]["exported"])

    def test_closure_named_by_binding_and_params_declared(self):
        r = self.r
        lam = syms(r, "lambda")
        self.assertEqual(len(lam), 1)
        self.assertEqual(lam[0]["name"], "c")
        decl = declarations(r, "m", "c")[0]
        self.assertEqual(decl["symbolId"], lam[0]["id"])
        self.assertEqual(declarations(r, "c", "x")[0]["kind"], "parameter")

    def test_use_trees_flattened_one_site_per_leaf_with_full_path(self):
        imps = sites(self.r, "imports") + sites(self.r, "reexports")
        by_spec = {s["binding"]["specifier"]: s["binding"] for s in imps if s["binding"]["kind"] == "use"}
        self.assertEqual(by_spec["std::collections::HashMap"]["local"], "HashMap")
        entry = by_spec["std::collections::hash_map::Entry"]
        self.assertEqual((entry["alias"], entry["local"], entry["imported"]), ("E", "E", "Entry"))
        glob = by_spec["crate::a"]
        self.assertTrue(glob["namespace"])
        self.assertIsNone(glob["local"])
        self.assertEqual(by_spec["self::b"]["alias"], "bb")
        self.assertTrue(by_spec["foo"]["selfImport"])
        self.assertIsNone(by_spec["foo::bar"]["local"])  # `as _` binds nothing
        self.assertEqual(by_spec["alloc"]["alias"], "al")
        self.assertTrue(by_spec["alloc"]["externCrate"])

    def test_absolute_use_leaves_keep_aliases_order_and_spans(self):
        source = "pub use ::root::{self as Alias, branch::{self, leaf as _, *}};\n"
        result = run(lang_rust, "rust", source)
        imports = sites(result, "reexports")
        bindings = [site["binding"] for site in imports]
        self.assertEqual(
            [(b["specifier"], b["alias"], b["local"], b["namespace"], b.get("selfImport", False)) for b in bindings],
            [
                ("::root", "Alias", "Alias", False, True),
                ("::root::branch", None, "branch", False, True),
                ("::root::branch::leaf", "_", None, False, False),
                ("::root::branch", None, None, True, False),
            ],
        )
        self.assertTrue(all(b["absolute"] for b in bindings))
        data = source.encode("utf-8")
        self.assertEqual(
            [data[s["span"]["byteStart"]:s["span"]["byteEnd"]].decode() for s in imports],
            ["self as Alias", "self", "leaf as _", "*"],
        )
        self.assertEqual(
            [(d["name"], d["siteId"]) for d in declarations(result, None)],
            [("Alias", imports[0]["id"]), ("branch", imports[1]["id"])],
        )

    def test_pub_use_is_reexport_relation(self):
        re_ = sites(self.r, "reexports")
        self.assertEqual([s["binding"]["specifier"] for s in re_], ["super::c::D"])
        self.assertTrue(re_[0]["binding"]["reexport"])
        self.assertEqual(re_[0]["binding"]["visibility"], "pub")

    def test_mod_declarations_and_path_attribute(self):
        mods = {s["binding"]["specifier"]: s["binding"] for s in sites(self.r, "imports") if s["binding"]["kind"] == "mod"}
        self.assertEqual(set(mods), {"inner", "routed"})
        self.assertEqual(mods["routed"]["path"], "other/x.rs")
        self.assertIsNone(mods["inner"]["path"])
        self.assertTrue(syms(self.r, "namespace", "inline")[0]["bindings"]["body"])

    def test_macro_invocations_are_separate_unsupported_facts(self):
        r = self.r
        macros = sites(r, "macros")
        self.assertEqual({s["binding"]["name"] for s in macros}, {"println", "vec"})
        self.assertTrue(all(s["resolution"] == "unsupported" and s["binding"]["kind"] == "macro" for s in macros))
        self.assertEqual(sites(r, "calls", "println"), [])
        self.assertEqual(sites(r, "calls", "foo"), [])  # macro_rules bodies are not expanded or parsed as calls

    def test_call_kinds(self):
        r = self.r
        path = sites(r, "calls", "new")
        kinds = {(s["binding"]["receiver"], s["binding"]["kind"]) for s in path}
        self.assertIn(("Self", "path"), kinds)
        self.assertIn(("S", "path"), kinds)
        self.assertIn(("Vec::<u8>", "path"), kinds)
        m = [s for s in sites(r, "calls", "m") if caller_name(r, s) == "m"][0]
        self.assertEqual((m["binding"]["receiver"], m["binding"]["receiverKind"]), ("self", "self"))
        chain = [s for s in sites(r, "calls", "m") if caller_name(r, s) == "main"][0]
        self.assertEqual(chain["binding"]["receiverKind"], "expression")
        deep = sites(r, "calls", "c")
        self.assertIn("a::b", [s["binding"]["receiver"] for s in deep])
        dyn = [s for s in sites(r, "calls") if s["binding"]["kind"] == "dynamic"]
        self.assertEqual(len(dyn), 1)
        self.assertEqual(dyn[0]["resolution"], "dynamic")
        for s in sites(r, "calls"):
            self.assertIsNone(s["targetId"])

    def test_recursion_mutual_recursion_and_uncalled(self):
        r = self.r
        self.assertEqual(caller_name(r, sites(r, "calls", "pong")[0]), "ping")
        self.assertEqual(caller_name(r, sites(r, "calls", "ping")[0]), "pong")
        self.assertEqual([s for s in sites(r, "calls", "unused")], [])
        self.assertTrue(syms(r, "function", "unused"))
        self.assertEqual(caller_name(r, [s for s in sites(r, "calls", "m") if s["binding"]["receiver"] == "self"][0]), "m")

    def test_cfg_gated_items_are_conditional(self):
        r = self.r
        helper = sites(r, "calls", "helper")
        self.assertTrue(helper[0]["conditional"])
        self.assertTrue(syms(r, "function", "t")[0]["bindings"]["conditional"])
        self.assertFalse(sites(r, "calls", "nested")[0]["conditional"])

    def test_shadowing_parameter_visible_to_resolver(self):
        r = self.r
        d = declarations(r, "shadow", "helper")[0]
        site = sites(r, "calls", "helper")
        in_shadow = [s for s in site if caller_name(r, s) == "shadow"][0]
        self.assertEqual(d["kind"], "parameter")
        self.assertTrue(d["scopeStart"] <= in_shadow["span"]["byteStart"] < d["scopeEnd"])

    def test_unicode_case_ids_distinct_and_deterministic(self):
        r = self.r
        a, b = syms(r, "function", "Größe")[0], syms(r, "function", "größe")[0]
        self.assertNotEqual(a["id"], b["id"])
        again = run(lang_rust, "rust", SRC, "src/lib.rs")
        self.assertEqual([s["id"] for s in r["symbols"]], [s["id"] for s in again["symbols"]])
        self.assertEqual(len({s["id"] for s in r["sites"]}), len(r["sites"]))

    def test_pattern_bindings_shadow_without_binding_constructors(self):
        src = "fn pat() { let (f, g) = (1, 2); if let Some(h) = x { h(); } match v { Ok(k) => k(), _ => {} } }\n"
        r = run(lang_rust, "rust", src)
        names = {d["name"] for d in declarations(r, "pat")}
        self.assertEqual(names, {"f", "g", "h", "k"})

    def test_syntax_error_is_partial_with_diagnostic(self):
        r = run(lang_rust, "rust", "fn ok() {}\nfn bad( {\n")
        self.assertTrue(r["partial"])
        self.assertTrue(syms(r, "function", "ok"))
        self.assertTrue(any("syntax" in d for d in r["diagnostics"]))

    def test_deep_use_nesting_is_bounded_not_crashing(self):
        deep = "use " + "a::{" * 80 + "b" + "}" * 80 + ";\nuse kept::leaf;\n"
        r = run(lang_rust, "rust", deep)
        self.assertTrue(r["partial"])
        self.assertEqual([s["binding"]["specifier"] for s in sites(r, "imports")], ["kept::leaf"])


class RustUnavailableTests(unittest.TestCase):
    def test_unknown_grammar_raises_instead_of_empty_success(self):
        ctx = common.Context("x.rs", "not-a-language", b"fn a() {}\n")
        with self.assertRaises(common.GrammarUnavailable):
            lang_rust.extract("rust", b"fn a() {}\n", ctx)


if __name__ == "__main__":
    unittest.main()
