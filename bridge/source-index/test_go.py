import unittest

import common
import lang_go
from testkit_syntax import (assert_node_kinds, by_id, caller_name, declarations, requires_grammar, run, sites, syms)

SRC = '''package main

import (
	"fmt"
	str "strings"
	. "os"
	_ "embed"
)

type T struct{}
type I interface {
	Do()
	Other(int)
}

func (t *T) M(x int) { t.N(); fmt.Println(x) }
func (t T) N() {}

func Top() {
	f := func() { Top() }
	f()
	go Top()
	defer Top()
}

func Ping() { Pong() }
func Pong() { Ping() }
func Unused() {}
func Größe() {}
func größe() {}
func Shadow(Top func()) { Top() }
func Dyn(m map[string]func()) { m["a"](); (Top)(); Top()() }
'''


@requires_grammar("go")
class GoRecipeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.r = run(lang_go, "go", SRC, "pkg/main.go")

    def test_profile_node_kinds_present_in_grammar(self):
        assert_node_kinds(self, lang_go, "go")

    def test_declarations_methods_literals_and_signatures(self):
        r = self.r
        self.assertEqual({s["name"] for s in syms(r, "function")},
                         {"Top", "Ping", "Pong", "Unused", "Größe", "größe", "Shadow", "Dyn"})
        methods = {s["qualifiedName"]: s for s in syms(r, "method")}
        self.assertEqual(set(methods), {"T.M", "T.N"})
        self.assertEqual(methods["T.M"]["bindings"]["receiver"], "T")
        self.assertTrue(methods["T.M"]["bindings"]["pointerReceiver"])
        self.assertFalse(methods["T.N"]["bindings"]["pointerReceiver"])
        self.assertEqual([s["name"] for s in syms(r, "signature")], ["Do", "Other"])
        self.assertEqual(len(syms(r, "lambda")), 1)
        self.assertEqual({s["name"] for s in syms(r, "struct")} | {s["name"] for s in syms(r, "interface")}, {"T", "I"})
        self.assertEqual(len(syms(r, "module")), 1)

    def test_unicode_and_case_variants_are_distinct_ids(self):
        r = self.r
        a, b = syms(r, "function", "Größe")[0], syms(r, "function", "größe")[0]
        self.assertNotEqual(a["id"], b["id"])
        self.assertTrue(a["exported"] and not b["exported"])
        self.assertEqual(len({s["id"] for s in r["symbols"]}), len(r["symbols"]))
        again = run(lang_go, "go", SRC, "pkg/main.go")
        self.assertEqual([s["id"] for s in r["symbols"]], [s["id"] for s in again["symbols"]])
        other_path = run(lang_go, "go", SRC, "pkg/other.go")
        self.assertNotEqual(r["symbols"][1]["id"], other_path["symbols"][1]["id"])

    def test_imports_keep_raw_alias_and_special_forms(self):
        imps = {s["raw"]: s["binding"] for s in sites(self.r, "imports")}
        self.assertEqual(imps['"fmt"']["specifier"], "fmt")
        self.assertEqual(imps['"fmt"']["local"], "fmt")
        self.assertTrue(imps['"fmt"']["implicitLocal"])
        self.assertEqual(imps['"strings"']["alias"], "str")
        self.assertEqual(imps['"strings"']["local"], "str")
        self.assertTrue(imps['"os"']["namespace"])
        self.assertIsNone(imps['"os"']["local"])
        self.assertTrue(imps['"embed"']["sideEffect"])
        decls = {d["name"] for d in declarations(self.r, None) if d["kind"] == "import"}
        self.assertEqual(decls, {"fmt", "str"})

    def test_calls_binding_and_nothing_promoted(self):
        r = self.r
        calls = sites(r, "calls")
        for s in calls:
            self.assertIsNone(s["targetId"])
            self.assertIn(s["resolution"], ("unresolved", "dynamic"))
        member = [s for s in calls if s["binding"]["name"] == "Println"][0]
        self.assertEqual(member["binding"]["receiver"], "fmt")
        self.assertEqual(member["binding"]["kind"], "member")
        self.assertEqual(caller_name(r, member), "M")
        go_call = [s for s in calls if s["binding"].get("async")]
        self.assertEqual(len(go_call), 1)
        self.assertEqual(len([s for s in calls if s["binding"].get("deferred")]), 1)

    def test_recursion_and_mutual_recursion_are_plain_sites(self):
        r = self.r
        in_lambda = [s for s in sites(r, "calls", "Top") if by_id(r)[s["callerId"]]["kind"] == "lambda"]
        self.assertEqual(len(in_lambda), 1)
        self.assertEqual(caller_name(r, sites(r, "calls", "Pong")[0]), "Ping")
        self.assertEqual(caller_name(r, sites(r, "calls", "Ping")[0]), "Pong")
        self.assertEqual(sites(r, "calls", "Unused"), [])  # uncalled function still a symbol
        self.assertTrue(syms(r, "function", "Unused"))

    def test_dynamic_callees_are_dynamic_not_named(self):
        r = self.r
        dyn = [s for s in sites(r, "calls") if caller_name(r, s) == "Dyn"]
        computed = [s for s in dyn if s["binding"]["kind"] == "dynamic"]
        self.assertEqual(len(computed), 3)  # m["a"](), (Top)(), Top()()
        self.assertTrue(all(s["resolution"] == "dynamic" and s["binding"]["name"] is None for s in computed))
        self.assertEqual(len(dyn) - len(computed), 1)  # the inner Top() of Top()() is its own site

    def test_shadowing_parameter_declared_in_function_scope(self):
        r = self.r
        top_decl = [d for d in declarations(r, None, "Top")]
        self.assertEqual(top_decl[0]["kind"], "function")
        shadow = declarations(r, "Shadow", "Top")
        self.assertEqual(shadow[0]["kind"], "parameter")
        site = [s for s in sites(r, "calls", "Top") if caller_name(r, s) == "Shadow"][0]
        self.assertTrue(shadow[0]["scopeStart"] <= site["span"]["byteStart"] < shadow[0]["scopeEnd"])

    def test_closure_bound_to_local_pairs_declaration_with_lambda_symbol(self):
        r = self.r
        decl = [d for d in declarations(r, "Top", "f")][0]
        self.assertEqual(decl["kind"], "variable")
        self.assertEqual(decl["symbolId"], syms(r, "lambda")[0]["id"])
        self.assertEqual(syms(r, "lambda")[0]["name"], "f")  # a closure bound to a name takes that spelling

    def test_spans_are_byte_based_and_unicode_safe(self):
        r = self.r
        sym = syms(r, "function", "Größe")[0]
        self.assertEqual(SRC.encode()[sym["span"]["byteStart"]:sym["span"]["byteEnd"]], "func Größe() {}".encode())

    def test_syntax_error_marks_partial_and_keeps_facts(self):
        r = run(lang_go, "go", "package p\nfunc Good() {}\nfunc Bad( {\n", "bad.go")
        self.assertTrue(r["partial"])
        self.assertTrue(syms(r, "function", "Good"))
        self.assertTrue(any("syntax" in d for d in r["diagnostics"]))

    def test_generic_instantiation_is_ambiguous_conversion_not_resolved(self):
        r = run(lang_go, "go", "package p\nfunc G[T any](x T) T { return x }\nfunc use() { G[int](1) }\n")
        site = sites(r, "calls", "G")[0]
        self.assertTrue(site["binding"]["ambiguousConversion"])
        self.assertIsNone(site["targetId"])

    def test_empty_file_is_empty_but_parsed(self):
        r = run(lang_go, "go", "package p\n")
        self.assertFalse(r["partial"])
        self.assertEqual(r["sites"], [])


class GoUnavailableTests(unittest.TestCase):
    def test_unknown_grammar_raises_instead_of_empty_success(self):
        ctx = common.Context("x.go", "not-a-language", b"package p\n")
        with self.assertRaises(common.GrammarUnavailable):
            lang_go.extract("go", b"package p\n", ctx)


if __name__ == "__main__":
    unittest.main()
