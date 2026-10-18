import unittest

import common
import lang_dotnet
from testkit_syntax import (assert_node_kinds, by_id, caller_name, declarations, requires_grammar, run, sites, syms)

SRC = '''using System;
using static System.Math;
using A = System.Collections.Generic.List<int>;
global using X.Y;
namespace N {
  public class C : B, IThing {
    private Action cb;
    public C() : base() { }
    ~C() { }
    public int P { get { return F(); } set { G(value); } }
    void M() { F(); this.F(); new Foo(); Func<int> f = () => 1; Func<int,int> g = delegate(int x) { return x; }; int Local() { return 1; } Local(); a.b.C(); obj?.D(); Helper<int>(); cb(); }
    public static C operator +(C a, C b) => a;
    int Ex => Calc(1);
    event EventHandler E { add { H(); } remove { } }
    int Rec(int n) { return Rec(n - 1); }
    void Ping() { Pong(); }
    void Pong() { Ping(); }
    void Unused() { }
    void Größe() { }
    void größe() { }
    void Shadow(Action F) { F(); }
    public delegate void Del(int a);
  }
  interface I { void Sig(); }
  struct S { void T() { } }
  record R(int A) { void RM() { } }
  enum Color { Red, Green }
}
'''


@requires_grammar("csharp")
class CSharpTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.r = run(lang_dotnet, "csharp", SRC, "src/C.cs")

    def test_profile_node_kinds_present_in_grammar(self):
        assert_node_kinds(self, lang_dotnet, "csharp")

    def test_no_syntax_errors_in_fixture(self):
        self.assertFalse(self.r["partial"], self.r["diagnostics"])

    def test_types_and_namespace(self):
        r = self.r
        self.assertEqual({s["name"] for s in syms(r, "class")}, {"C", "R"})
        self.assertEqual({s["name"] for s in syms(r, "interface")}, {"I"})
        self.assertEqual({s["name"] for s in syms(r, "struct")}, {"S"})
        self.assertEqual({s["name"] for s in syms(r, "enum")}, {"Color"})
        c = syms(r, "class", "C")[0]
        self.assertEqual(c["bindings"]["fqn"], "N.C")
        self.assertEqual(c["bindings"]["extends"], ["B", "IThing"])
        self.assertTrue(c["exported"])
        self.assertEqual(syms(r, "namespace", "N")[0]["bindings"]["namespace"], "N")
        self.assertTrue(syms(r, "type", "Del")[0]["bindings"]["delegate"])

    def test_callables_constructors_accessors_operators_local_functions_lambdas(self):
        r = self.r
        self.assertEqual({s["name"] for s in syms(r, "constructor")}, {"C"})
        self.assertTrue(syms(r, "method", "~C")[0]["bindings"]["destructor"])
        acc = sorted((s["name"], s["bindings"]["property"]) for s in syms(r, "accessor"))
        self.assertEqual(acc, [("add", "E"), ("get", "Ex"), ("get", "P"), ("remove", "E"), ("set", "P")])
        self.assertTrue([s for s in syms(r, "accessor") if s["bindings"].get("expressionBodied")])
        self.assertTrue(syms(r, "method", "operator +")[0]["bindings"]["operator"])
        self.assertEqual(syms(r, "function", "Local")[0]["qualifiedName"], "N.C.M.Local")
        lam = {s["name"]: s for s in syms(r, "lambda")}
        self.assertEqual(set(lam), {"f", "g"})
        self.assertEqual(lam["g"]["bindings"]["parameters"], ["x"])
        self.assertTrue(lam["g"]["bindings"]["body"])  # anonymous method block body
        self.assertEqual({s["name"] for s in syms(r, "property") if s["bindings"].get("enumConstant")}, {"Red", "Green"})

    def test_using_forms(self):
        by = {s["binding"]["specifier"]: s["binding"] for s in sites(self.r, "imports")}
        self.assertTrue(by["System"]["namespace"])
        self.assertTrue(by["System.Math"]["static"])
        self.assertTrue(by["X.Y"]["global"])
        alias = by["System.Collections.Generic.List<int>"]
        self.assertEqual((alias["alias"], alias["local"], alias["namespace"]), ("A", "A", False))
        self.assertEqual({d["name"] for d in declarations(self.r, None) if d["kind"] == "import"}, {"A"})

    def test_invocations_forms_and_nothing_promoted(self):
        r = self.r
        kinds = {(s["binding"]["name"], s["binding"]["kind"], s["binding"]["receiverKind"]) for s in sites(r, "calls")}
        self.assertIn(("F", "identifier", None), kinds)
        self.assertIn(("F", "member", "this"), kinds)
        self.assertIn(("C", "path", "path"), kinds)
        self.assertIn(("D", "member", "identifier"), kinds)
        self.assertIn(("Foo", "new", None), kinds)
        self.assertIn(("base", "identifier", None), kinds)
        generic = sites(r, "calls", "Helper")[0]
        self.assertTrue(generic["binding"]["generic"])
        safe = sites(r, "calls", "D")[0]
        self.assertTrue(safe["binding"]["safeCall"])
        for s in sites(r, "calls"):
            self.assertIsNone(s["targetId"])
            self.assertIn(s["resolution"], ("unresolved", "dynamic"))

    def test_implicit_new_and_computed_callee_are_dynamic(self):
        r = run(lang_dotnet, "csharp", "class Q { void M() { Foo g = new(); fns[0](); (x)(); } }\n")
        self.assertEqual([s["resolution"] for s in sites(r, "calls") if s["binding"]["kind"] == "new"], ["dynamic"])
        self.assertEqual(len([s for s in sites(r, "calls") if s["binding"]["kind"] == "dynamic"]), 2)

    def test_recursion_mutual_recursion_isolated(self):
        r = self.r
        self.assertEqual(caller_name(r, sites(r, "calls", "Rec")[0]), "Rec")
        self.assertEqual(caller_name(r, sites(r, "calls", "Pong")[0]), "Ping")
        self.assertEqual(caller_name(r, sites(r, "calls", "Ping")[0]), "Pong")
        self.assertEqual(sites(r, "calls", "Unused"), [])
        self.assertTrue(syms(r, "method", "Unused"))

    def test_shadowing_parameter_and_delegate_field_declarations(self):
        r = self.r
        d = declarations(r, "Shadow", "F")[0]
        site = [s for s in sites(r, "calls", "F") if caller_name(r, s) == "Shadow"][0]
        self.assertEqual(d["kind"], "parameter")
        self.assertTrue(d["scopeStart"] <= site["span"]["byteStart"] < d["scopeEnd"])
        field = declarations(r, "C", "cb")[0]
        self.assertEqual(field["kind"], "variable")
        method_decl = declarations(r, "C", "F")
        self.assertEqual(method_decl, [])  # no method named F exists; the parameter is the only F

    def test_lambda_variable_pairing(self):
        r = self.r
        decl = declarations(r, "M", "f")[0]
        self.assertEqual(decl["symbolId"], [s for s in syms(r, "lambda", "f")][0]["id"])

    def test_case_and_unicode_names_distinct_and_deterministic(self):
        r = self.r
        a, b = syms(r, "method", "Größe")[0], syms(r, "method", "größe")[0]
        self.assertNotEqual(a["id"], b["id"])
        self.assertEqual(SRC.encode()[a["span"]["byteStart"]:a["span"]["byteEnd"]], "void Größe() { }".encode())
        again = run(lang_dotnet, "csharp", SRC, "src/C.cs")
        self.assertEqual([s["id"] for s in r["symbols"]], [s["id"] for s in again["symbols"]])

    def test_file_scoped_namespace_gives_fqn(self):
        r = run(lang_dotnet, "csharp", "namespace A.B;\npublic class Foo { }\n")
        self.assertEqual(syms(r, "class", "Foo")[0]["bindings"]["fqn"], "A.B.Foo")
        self.assertEqual(syms(r, "module")[0]["bindings"]["namespace"], "A.B")

    def test_syntax_error_partial_keeps_good_facts(self):
        r = run(lang_dotnet, "csharp", "class A { void Ok() { } void Bad( { } }\n")
        self.assertTrue(r["partial"])
        self.assertTrue(syms(r, "method", "Ok"))


class CSharpUnavailableTests(unittest.TestCase):
    def test_unknown_grammar_raises_instead_of_empty_success(self):
        ctx = common.Context("x.cs", "not-a-language", b"class A {}\n")
        with self.assertRaises(common.GrammarUnavailable):
            lang_dotnet.extract("csharp", b"class A {}\n", ctx)

    def test_extension_plan(self):
        self.assertEqual(lang_dotnet.PROFILES["csharp"]["extensions"], [".cs"])


if __name__ == "__main__":
    unittest.main()
