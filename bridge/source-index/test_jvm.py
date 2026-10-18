import unittest

import common
import lang_jvm
from testkit_syntax import (assert_node_kinds, by_id, caller_name, declarations, requires_grammar, run, sites, syms)

JAVA = '''package p.q;
import java.util.List;
import static java.lang.Math.max;
import a.b.*;
public class A<T> extends B implements I {
  A() { this(1); super.x(); }
  A(int x) { }
  static { init(); }
  void m() { n(); this.n(); new Foo(1); Runnable r = () -> n(); list.stream().map(x -> x); Outer.Inner.f(); new Object() { void anon() {} }; Foo::bar; }
  int n() { return m2(); }
  int rec(int k) { return rec(k - 1); }
  int Größe() { return 1; }
  int größe() { return 2; }
  void ping() { pong(); }
  void pong() { ping(); }
  void unused() {}
  interface J { void sig(); default void d() {} }
  enum E { X { void f(){} }, Y; void g() {} }
  record R(int a) { R { x(); } }
}
'''

GROOVY = '''package x
import foo.Bar
import static a.B.c
import d.*
import e.F as G
class A extends B {
  A() { super() }
  def m() { n(); this.n(); new Foo(); def c = { it -> n() }; list.each { println it } }
}
def top() { helper() }
println "hi"
task build { doLast { println 1 } }
dependencies { implementation 'x:y:1' }
'''

KOTLIN = '''package a.b
import x.y.Z
import x.y.*
import x.w as W
class A(val p: Int) : B() {
    constructor(a: String) : this(1)
    init {
        setup()
    }
    fun m() {
        n()
        this.n()
        val l = { x: Int -> x }
        l(1)
        list.map { it + 1 }
        fun local() {
        }
        local()
        val o = object : Runnable {
            override fun run() {
            }
        }
    }
    companion object {
        fun f() {
        }
    }
    val prop: Int
        get() = 1
}
object O {
    fun o() = A(1).m()
}
fun top() = helper(1)
fun String.ext() = length
fun rec(n: Int): Int = rec(n - 1)
fun shadow(rec: () -> Unit) {
    rec()
}
interface I {
    fun sig()
    fun d() {
    }
}
'''

SCALA = '''package a.b
import x.y.Z
import x.y.{A, B => C, _}
import x.w._
class A(p: Int) extends B with T {
  def this(a: String) = this(1)
  def m(): Unit = { n(); this.n(); val l = (x: Int) => x; l(1); List(1).map(_ + 1); def local() = 1; local() }
  def sig: Double
  def rec(i: Int): Int = rec(i - 1)
  val g = curry(1)(2)
  val x = a max b
}
object O { def o = new A(1).m() }
trait T { def t(): Unit }
enum Color { case Red; def f = 1 }
given G: Ordering[Int] with { def compare(a: Int, b: Int) = 0 }
extension (s: String) def ext = s
'''


def _by_name(items, name):
    return [s for s in items if s["name"] == name]


@requires_grammar("java")
class JavaTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.r = run(lang_jvm, "java", JAVA, "src/p/q/A.java")

    def test_profile_node_kinds_present_in_grammar(self):
        assert_node_kinds(self, lang_jvm, "java")

    def test_types_methods_constructors_initializers_lambdas(self):
        r = self.r
        self.assertEqual({s["name"] for s in syms(r, "class") if s["name"]}, {"A", "R"})
        self.assertEqual(len([s for s in syms(r, "class") if not s["name"]]), 1)  # anonymous class
        self.assertEqual(syms(r, "class", "A")[0]["bindings"]["fqn"], "p.q.A")
        self.assertEqual(syms(r, "class", "A")[0]["bindings"]["extends"], ["B"])
        self.assertEqual(syms(r, "class", "A")[0]["bindings"]["implements"], ["I"])
        self.assertEqual(syms(r, "module")[0]["bindings"]["package"], "p.q")
        self.assertEqual(len(syms(r, "constructor")), 3)  # two ctors + compact record ctor
        self.assertEqual(len(syms(r, "lambda")), 2)
        self.assertEqual({s["name"] for s in syms(r, "initializer")}, {"<clinit>"})
        self.assertEqual({s["name"] for s in syms(r, "interface")}, {"J"})
        self.assertEqual({s["name"] for s in syms(r, "enum")}, {"E"})
        self.assertFalse(syms(r, "method", "sig")[0]["bindings"]["body"])
        self.assertTrue(syms(r, "method", "d")[0]["bindings"]["body"])
        self.assertEqual({s["name"] for s in syms(r, "property")}, {"X", "Y"})

    def test_imports_one_site_per_name_with_static_and_wildcard(self):
        imps = [s["binding"] for s in sites(self.r, "imports")]
        by = {b["specifier"]: b for b in imps}
        self.assertEqual(by["java.util.List"]["local"], "List")
        self.assertTrue(by["java.lang.Math.max"]["static"])
        self.assertEqual(by["java.lang.Math.max"]["imported"], "max")
        self.assertTrue(by["a.b"]["namespace"])
        self.assertIsNone(by["a.b"]["imported"])
        self.assertEqual({d["name"] for d in declarations(self.r, None) if d["kind"] == "import"}, {"List", "max"})

    def test_call_forms(self):
        r = self.r
        kinds = {(s["binding"]["name"], s["binding"]["kind"], s["binding"]["receiverKind"]) for s in sites(r, "calls")}
        self.assertIn(("n", "identifier", None), kinds)
        self.assertIn(("n", "member", "this"), kinds)
        self.assertIn(("x", "member", "super"), kinds)
        self.assertIn(("Foo", "new", None), kinds)
        self.assertIn(("f", "path", "path"), kinds)
        self.assertIn(("map", "member", "expression"), kinds)
        ctor = [s for s in sites(r, "calls") if s["binding"].get("constructor")]
        self.assertEqual(ctor[0]["binding"]["name"], "this")
        refs = sites(r, "references")
        self.assertEqual(refs[0]["binding"]["name"], "bar")
        self.assertTrue(refs[0]["binding"]["methodReference"])
        for s in sites(r, "calls"):
            self.assertIsNone(s["targetId"])
            self.assertEqual(s["resolution"], "unresolved")

    def test_recursion_mutual_recursion_isolated_and_same_spelling_case(self):
        r = self.r
        self.assertEqual(caller_name(r, sites(r, "calls", "rec")[0]), "rec")
        self.assertEqual(caller_name(r, sites(r, "calls", "pong")[0]), "ping")
        self.assertEqual(sites(r, "calls", "unused"), [])
        self.assertTrue(syms(r, "method", "unused"))
        a, b = syms(r, "method", "Größe")[0], syms(r, "method", "größe")[0]
        self.assertNotEqual(a["id"], b["id"])
        decl = declarations(r, "A", "n")
        self.assertEqual(decl[0]["kind"], "function")
        self.assertEqual(decl[0]["symbolId"], syms(r, "method", "n")[0]["id"])

    def test_variables_do_not_shadow_calls_in_javas_separate_namespace(self):
        r = run(lang_jvm, "java", "class Q { int size() { int size = size(); return size; } }\n")
        self.assertEqual([d for d in declarations(r, "size") if d["name"] == "size"], [])

    def test_anonymous_class_methods_and_calls_owner(self):
        r = self.r
        anon_method = syms(r, "method", "anon")[0]
        owner = by_id(r)[anon_method["ownerId"]]
        self.assertEqual((owner["kind"], owner["name"]), ("class", ""))

    def test_deterministic_ids_and_unicode_spans(self):
        again = run(lang_jvm, "java", JAVA, "src/p/q/A.java")
        self.assertEqual([s["id"] for s in self.r["symbols"]], [s["id"] for s in again["symbols"]])
        m = syms(self.r, "method", "Größe")[0]
        self.assertEqual(JAVA.encode()[m["span"]["byteStart"]:m["span"]["byteEnd"]], "int Größe() { return 1; }".encode())

    def test_syntax_error_partial_keeps_good_facts(self):
        r = run(lang_jvm, "java", "class A { void ok() {} void bad( { } }\n")
        self.assertTrue(r["partial"])
        self.assertTrue(syms(r, "method", "ok"))


@requires_grammar("groovy")
class GroovyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.r = run(lang_jvm, "groovy", GROOVY, "build.gradle")

    def test_profile_node_kinds_present_in_grammar(self):
        assert_node_kinds(self, lang_jvm, "groovy")

    def test_gradle_extension_is_groovy_profile(self):
        self.assertIn(".gradle", lang_jvm.PROFILES["groovy"]["extensions"])
        self.assertIn(".groovy", lang_jvm.PROFILES["groovy"]["extensions"])

    def test_script_functions_classes_closures_imports(self):
        r = self.r
        self.assertTrue(syms(r, "function", "top")[0]["bindings"]["script"])
        self.assertEqual(syms(r, "class", "A")[0]["bindings"]["fqn"], "x.A")
        self.assertTrue(syms(r, "class", "A")[0]["exported"])  # Groovy members are public by default
        self.assertEqual(syms(r, "lambda", "c")[0]["bindings"]["parameters"], ["it"])
        self.assertGreaterEqual(len([s for s in syms(r, "lambda") if not s["name"]]), 3)
        imps = {s["binding"]["specifier"]: s["binding"] for s in sites(r, "imports")}
        self.assertEqual(imps["e.F"]["alias"], "G")
        self.assertTrue(imps["a.B.c"]["static"])
        self.assertTrue(imps["d"]["namespace"])

    def test_dsl_calls_are_unresolved_command_or_closure_sites(self):
        r = self.r
        deps = sites(r, "calls", "dependencies")[0]
        self.assertTrue(deps["binding"]["closureBody"])
        impl = sites(r, "calls", "implementation")[0]
        self.assertTrue(impl["binding"]["command"])
        self.assertEqual(caller_name(r, sites(r, "calls", "doLast")[0]), "")
        for s in sites(r, "calls"):
            self.assertIsNone(s["targetId"])

    def test_untyped_parameters_make_file_partial_not_clean(self):
        self.assertTrue(self.r["partial"])
        self.assertTrue(self.r["diagnostics"])

    def test_closure_variable_is_lexical_declaration_with_symbol(self):
        r = self.r
        decl = declarations(r, "m", "c")[0]
        self.assertEqual(decl["kind"], "variable")
        self.assertEqual(decl["symbolId"], syms(r, "lambda", "c")[0]["id"])


@requires_grammar("kotlin")
class KotlinTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.r = run(lang_jvm, "kotlin", KOTLIN, "app/A.kt")

    def test_profile_node_kinds_present_in_grammar(self):
        assert_node_kinds(self, lang_jvm, "kotlin")

    def test_no_syntax_errors_in_fixture(self):
        self.assertFalse(self.r["partial"], self.r["diagnostics"])

    def test_declarations(self):
        r = self.r
        self.assertEqual({s["name"] for s in syms(r, "class")}, {"A", "O", "Companion", ""})
        self.assertEqual(syms(r, "class", "A")[0]["bindings"]["fqn"], "a.b.A")
        self.assertEqual(syms(r, "class", "A")[0]["bindings"]["extends"], ["B()"])
        self.assertTrue(syms(r, "class", "O")[0]["bindings"]["object"])
        ctors = syms(r, "constructor")
        self.assertEqual(len(ctors), 2)
        self.assertTrue(any(c["bindings"].get("primary") for c in ctors))
        self.assertEqual({s["name"] for s in syms(r, "accessor")}, {"get"})
        self.assertEqual(syms(r, "initializer")[0]["name"], "<init>")
        ext = syms(r, "function", "ext")[0]
        self.assertEqual(ext["bindings"]["receiver"], "String")
        self.assertTrue(ext["bindings"]["extension"])
        self.assertTrue(syms(r, "method", "m"))
        self.assertTrue(syms(r, "function", "local"))
        self.assertFalse(syms(r, "method", "sig")[0]["bindings"]["body"])
        self.assertEqual({s["name"] for s in syms(r, "lambda")}, {"l", ""})

    def test_imports_wildcard_and_alias(self):
        imps = {s["binding"]["specifier"]: s["binding"] for s in sites(self.r, "imports")}
        self.assertEqual(imps["x.y.Z"]["local"], "Z")
        self.assertTrue(imps["x.y"]["namespace"])
        self.assertEqual(imps["x.w"]["alias"], "W")

    def test_calls_receivers_and_lambda_pairing(self):
        r = self.r
        m = sites(r, "calls", "n")
        self.assertEqual({s["binding"]["receiverKind"] for s in m}, {None, "this"})
        chain = [s for s in sites(r, "calls", "m") if s["binding"]["receiverKind"] == "expression"]
        self.assertEqual(chain[0]["binding"]["receiver"], "A(1)")
        decl = declarations(r, "m", "l")[0]
        self.assertEqual(decl["symbolId"], syms(r, "lambda", "l")[0]["id"])
        self.assertTrue([s for s in sites(r, "calls") if s["binding"].get("constructor")])
        for s in sites(r, "calls"):
            self.assertIsNone(s["targetId"])

    def test_recursion_shadow_and_isolated(self):
        r = self.r
        self.assertEqual(caller_name(r, sites(r, "calls", "rec")[0]), "rec")
        d = declarations(r, "shadow", "rec")[0]
        s = [x for x in sites(r, "calls", "rec") if caller_name(r, x) == "shadow"][0]
        self.assertEqual(d["kind"], "parameter")
        self.assertTrue(d["scopeStart"] <= s["span"]["byteStart"] < d["scopeEnd"])

    def test_one_line_blocks_are_partial_with_diagnostics(self):
        # The grammar needs newline-separated statements; errors must never look like a clean parse.
        r = run(lang_jvm, "kotlin", "class A { init { setup() } }\n")
        self.assertTrue(r["partial"])
        self.assertTrue(r["diagnostics"])


@requires_grammar("scala")
class ScalaTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.r = run(lang_jvm, "scala", SCALA, "src/A.scala")

    def test_profile_node_kinds_present_in_grammar(self):
        assert_node_kinds(self, lang_jvm, "scala")

    def test_declarations(self):
        r = self.r
        self.assertEqual({s["name"] for s in syms(r, "class")}, {"A", "O", "G"})
        self.assertEqual({s["name"] for s in syms(r, "trait")}, {"T"})
        self.assertEqual({s["name"] for s in syms(r, "enum")}, {"Color"})
        self.assertEqual(syms(r, "class", "A")[0]["bindings"]["extends"], ["B", "T"])
        self.assertEqual(syms(r, "class", "A")[0]["bindings"]["fqn"], "a.b.A")
        self.assertEqual(len(syms(r, "constructor")), 1)
        sig = [s for s in syms(r, "method", "sig")][0]
        self.assertFalse(sig["bindings"]["body"])
        self.assertTrue(sig["bindings"]["abstract"])
        self.assertTrue(syms(r, "method", "ext")[0]["bindings"]["extension"])
        self.assertTrue(syms(r, "method", "compare"))
        self.assertTrue(syms(r, "function", "local"))
        self.assertEqual({s["name"] for s in syms(r, "property")}, {"Red"})

    def test_imports_one_site_per_leaf(self):
        imps = [s["binding"] for s in sites(self.r, "imports")]
        specs = sorted((b["specifier"], b["imported"], b["alias"], b["namespace"]) for b in imps)
        self.assertEqual(specs, sorted([
            ("x.y.Z", "Z", None, False),
            ("x.y.A", "A", None, False),
            ("x.y.B", "B", "C", False),
            ("x.y", None, None, True),
            ("x.w", None, None, True),
        ]))
        self.assertEqual({b["local"] for b in imps if b["local"]}, {"Z", "A", "C"})

    def test_calls_curried_infix_and_new(self):
        r = self.r
        new = [s for s in sites(r, "calls") if s["binding"]["kind"] == "new"]
        self.assertEqual(new[0]["binding"]["name"], "A")
        curry = [s for s in sites(r, "calls") if s["binding"]["kind"] == "dynamic"]
        self.assertEqual(len(curry), 1)  # `curry(1)(2)`: the applied result is a computed callee
        self.assertEqual(curry[0]["resolution"], "dynamic")
        self.assertEqual(sites(r, "calls", "curry")[0]["resolution"], "unresolved")
        infix = [s for s in sites(r, "calls") if s["binding"].get("infix")]
        self.assertEqual((infix[0]["binding"]["name"], infix[0]["binding"]["receiver"]), ("max", "a"))
        for s in sites(r, "calls"):
            self.assertIsNone(s["targetId"])

    def test_lambda_pairing_and_recursion(self):
        r = self.r
        decl = declarations(r, "m", "l")[0]
        self.assertEqual(decl["symbolId"], [s for s in syms(r, "lambda", "l")][0]["id"])
        self.assertEqual(caller_name(r, sites(r, "calls", "rec")[0]), "rec")

    def test_case_pattern_bindings_declared_without_constructors(self):
        r = run(lang_jvm, "scala", "object Q { def mm(i: Int) = i match { case Some(f) => f(1); case n: Int => n } }\n")
        names = {d["name"] for d in declarations(r, "mm") if d["kind"] == "variable"}
        self.assertEqual(names, {"f", "n"})

    def test_package_with_braces_scopes_fqn(self):
        r = run(lang_jvm, "scala", "package a.b {\n  object Q { def z = 1 }\n}\n")
        self.assertEqual(syms(r, "class", "Q")[0]["bindings"]["fqn"], "a.b.Q")
        self.assertEqual(syms(r, "namespace", "a.b")[0]["kind"], "namespace")


class JvmUnavailableTests(unittest.TestCase):
    def test_unknown_grammar_raises_instead_of_empty_success(self):
        ctx = common.Context("x.java", "not-a-language", b"class A {}\n")
        with self.assertRaises(common.GrammarUnavailable):
            lang_jvm.extract("java", b"class A {}\n", ctx)

    def test_language_not_owned_by_module_raises(self):
        ctx = common.Context("x.py", "python", b"x = 1\n")
        with self.assertRaises(common.GrammarUnavailable):
            lang_jvm.extract("python", b"x = 1\n", ctx)

    def test_extension_plan(self):
        self.assertEqual(lang_jvm.PROFILES["kotlin"]["extensions"], [".kt", ".kts"])
        self.assertEqual(lang_jvm.PROFILES["java"]["extensions"], [".java"])
        self.assertEqual(lang_jvm.PROFILES["scala"]["extensions"], [".scala"])


if __name__ == "__main__":
    unittest.main()
