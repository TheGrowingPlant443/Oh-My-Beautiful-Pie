"""PHP recipe fixtures.  Skipped with an explicit reason when the grammar is unavailable."""
import sys
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common  # noqa: E402
import lang_php  # noqa: E402

LANG = "php"
PROBE = common.language_probe(LANG, lang_php.PROFILES[LANG]["node_kinds"])
SKIP = None if PROBE["available"] else f"GRAMMAR UNAVAILABLE ({LANG}): {PROBE['error']}"


def run(text, path="src/App.php"):
    data = text.encode("utf-8")
    ctx = common.Context(path, LANG, data, deadline=time.monotonic() + 20)
    lang_php.extract(LANG, data, ctx)
    return ctx.result(), data


def by(items, **kw):
    return [i for i in items if all(i.get(k) == v for k, v in kw.items())]


class GrammarProbe(unittest.TestCase):
    @unittest.skipIf(SKIP is not None, SKIP or "")
    def test_required_node_kinds_present(self):
        self.assertIsNone(PROBE["error"], PROBE["error"])


@unittest.skipIf(SKIP is not None, SKIP or "")
class PhpRecipe(unittest.TestCase):
    def test_declarations(self):
        res, _ = run(
            "<?php\nnamespace App\\Svc;\nfunction top($a) { return helper($a); }\n"
            "class K extends P implements I, J {\n  use T;\n  public function __construct() { parent::__construct(); }\n"
            "  public static function make() { return new static(); }\n  abstract function ab();\n}\n"
            "interface I { function im(); }\ntrait T { function tm() {} }\nenum E { case A; function em() {} }\n"
        )
        syms = res["symbols"]
        top = by(syms, kind="function", name="top")[0]
        self.assertEqual(top["bindings"]["fqn"], "App\\Svc\\top")
        k = by(syms, kind="class", name="K")[0]
        self.assertEqual(k["bindings"]["extends"], ["P"])
        self.assertEqual(k["bindings"]["implements"], ["I", "J"])
        ctor = by(syms, kind="constructor")[0]
        self.assertEqual(ctor["ownerId"], k["id"])
        make = by(syms, name="make")[0]
        self.assertTrue(make["bindings"]["static"])
        self.assertFalse(by(syms, name="ab")[0]["bindings"]["body"])
        self.assertTrue(by(syms, kind="interface", name="I"))
        self.assertTrue(by(syms, kind="trait", name="T"))
        self.assertTrue(by(syms, kind="enum", name="E"))
        self.assertEqual(by(syms, name="em")[0]["kind"], "method")
        roles = sorted(s["binding"]["role"] for s in by(res["sites"], relation="references"))
        self.assertEqual(roles, ["extends", "implements", "implements", "trait"])
        self.assertEqual(len({s["id"] for s in syms}), len(syms))

    def test_braced_namespace_scope(self):
        res, _ = run("<?php\nnamespace A { function f() {} }\nnamespace B { function f() {} }\n")
        fs = by(res["symbols"], name="f")
        self.assertEqual(sorted(f["bindings"]["fqn"] for f in fs), ["A\\f", "B\\f"])
        self.assertNotEqual(fs[0]["id"], fs[1]["id"])

    def test_use_imports_group_alias_function(self):
        res, _ = run("<?php\nuse Foo\\Bar as Baz;\nuse function Foo\\helper;\nuse Foo\\{A, B as C, function d};\nuse const X\\Y;\n")
        imps = by(res["sites"], relation="imports")
        got = {(s["binding"]["specifier"], s["binding"]["alias"], s["binding"]["typeKind"]) for s in imps}
        self.assertIn(("Foo\\Bar", "Baz", None), got)
        self.assertIn(("Foo\\helper", None, "function"), got)
        self.assertIn(("Foo\\A", None, None), got)
        self.assertIn(("Foo\\B", "C", None), got)
        self.assertIn(("Foo\\d", None, "function"), got)
        self.assertIn(("X\\Y", None, "const"), got)
        baz = by(imps, raw="Foo\\Bar as Baz")[0]
        self.assertEqual(baz["binding"]["local"], "Baz")
        mod = by(res["symbols"], kind="module")[0]
        names = {d["name"] for d in mod["bindings"]["declarations"] if d["kind"] == "import"}
        self.assertEqual(names, {"Baz", "helper", "A", "C", "d", "Y"})

    def test_includes_literal_dir_dynamic(self):
        res, _ = run(
            "<?php\nrequire_once 'a.php';\ninclude __DIR__ . '/b.php';\ninclude_once(dirname(__FILE__) . '/z.php');\n"
            "require dirname(__DIR__) . '/up.php';\nrequire $x;\nrequire \"pre$x.php\";\nrequire('x' . $y);\n"
        )
        inc = by(res["sites"], relation="includes")
        self.assertEqual(len(inc), 7)
        lit = {s["binding"]["specifier"]: s["binding"] for s in inc if s["binding"]["specifier"] is not None}
        self.assertEqual(lit["a.php"]["loader"], "require_once")
        self.assertIsNone(lit["a.php"]["relativeTo"])
        self.assertEqual(lit["/b.php"]["relativeTo"], "__DIR__")
        self.assertEqual(lit["/z.php"]["relativeTo"], "__DIR__")
        self.assertEqual(lit["../up.php"]["relativeTo"], "__DIR__")
        dyn = by(inc, resolution="dynamic")
        self.assertEqual(len(dyn), 3)
        self.assertTrue(all(d["binding"]["specifier"] is None for d in dyn))

    def test_calls_new_closures_dynamic(self):
        res, _ = run(
            "<?php\nclass K { function m($x) {\n  $this->n(); self::s(); K::t(); parent::p();\n"
            "  $f = function($a) use ($y) { return $a; };\n  $g = fn($z) => $z;\n  $h = new Foo(1); $i = new \\Ns\\Bar;\n"
            "  $v(); ($x->b)(); strlen('x'); $o?->go(); $x->$name(); $cls::make(); new $cls; call_user_func('f');\n  $this->m($x);\n} }\n"
        )
        m = by(res["symbols"], name="m")[0]
        sites = {s["raw"]: s for s in by(res["sites"], relation="calls")}
        self.assertEqual(sites["$this->n"]["binding"]["receiverKind"], "this")
        self.assertEqual(sites["self::s"]["binding"]["receiverKind"], "self")
        self.assertEqual(sites["K::t"]["binding"]["kind"], "path")
        self.assertEqual(sites["parent::p"]["binding"]["receiverKind"], "super")
        self.assertEqual(sites["new Foo"]["binding"]["kind"], "new")
        self.assertEqual(sites["new \\Ns\\Bar"]["binding"]["receiver"], "\\Ns")
        self.assertEqual(sites["strlen"]["binding"]["kind"], "identifier")
        self.assertEqual(sites["strlen"]["resolution"], "unresolved")
        self.assertEqual(sites["$v"]["resolution"], "dynamic")
        self.assertTrue(sites["$o?->go"]["binding"]["optional"])
        self.assertEqual(sites["$x->$name"]["resolution"], "dynamic")
        self.assertEqual(sites["$cls::make"]["resolution"], "dynamic")
        self.assertEqual(sites["new $cls"]["resolution"], "dynamic")
        self.assertEqual(sites["call_user_func"]["resolution"], "dynamic")
        # direct recursion through $this is a retained, unresolved site owned by the method
        rec = [s for s in by(res["sites"], relation="calls") if s["raw"] == "$this->m"][0]
        self.assertEqual(rec["callerId"], m["id"])
        self.assertIsNone(rec["targetId"])
        closures = by(res["symbols"], kind="lambda")
        self.assertEqual({c["name"] for c in closures}, {"<closure>", "<arrow>"})
        closure = by(closures, name="<closure>")[0]
        self.assertEqual(closure["bindings"]["assignedTo"], "$f")
        self.assertIn(("$y", "variable"), [(d["name"], d["kind"]) for d in closure["bindings"]["declarations"]])
        self.assertIn(("$a", "parameter"), [(d["name"], d["kind"]) for d in closure["bindings"]["declarations"]])
        for s in res["sites"]:
            self.assertIsNone(s["targetId"])

    def test_relative_and_dynamic_constructors_keep_argument_calls_and_spans(self):
        res, data = run(
            "<?php class K extends Base { function make($class) {\nif ($flag) {\n"
            "new self(first()); new parent(); new static(); new $class(last());\n} } }\n"
        )
        calls = by(res["sites"], relation="calls")
        self.assertEqual([s["raw"] for s in calls], ["new self", "first", "new parent", "new static", "new $class", "last"])
        constructors = [s for s in calls if s["binding"]["kind"] in ("new", "dynamic")]
        self.assertEqual(
            [(s["binding"]["name"], s["binding"]["receiverKind"], s["resolution"]) for s in constructors],
            [("self", "self", "unresolved"), ("parent", "super", "unresolved"), ("static", "self", "unresolved"), (None, None, "dynamic")],
        )
        self.assertTrue(constructors[-1]["binding"]["new"])
        self.assertEqual(
            [data[s["span"]["byteStart"]:s["span"]["byteEnd"]].decode() for s in constructors],
            ["new self(first())", "new parent()", "new static()", "new $class(last())"],
        )
        method = by(res["symbols"], name="make")[0]
        self.assertTrue(all(s["callerId"] == method["id"] and s["conditional"] for s in calls))

    def test_anonymous_class_unicode_case_conditional(self):
        res, data = run(
            "<?php\nfunction café() {}\nfunction Café() {}\nif (!function_exists('x')) { function x() { a(); } }\n"
            "b();\nif ($c) { d(); }\n$o = new class extends P { function m() {} };\n"
        )
        syms = res["symbols"]
        cafe = by(syms, name="café")[0]
        sp = cafe["span"]
        self.assertEqual(data[sp["byteStart"]:sp["byteEnd"]].decode(), "function café() {}")
        self.assertTrue(by(syms, name="Café"))
        self.assertNotEqual(cafe["id"], by(syms, name="Café")[0]["id"])
        self.assertFalse(by(res["sites"], raw="b")[0]["conditional"])
        self.assertTrue(by(res["sites"], raw="d")[0]["conditional"])
        mod = by(syms, kind="module")[0]
        x_decl = [d for d in mod["bindings"]["declarations"] if d["name"] == "x"][0]
        self.assertTrue(x_decl["mutable"])  # conditionally declared function
        anon = by(syms, name="<anonymous class>")[0]
        self.assertEqual(anon["bindings"]["extends"], ["P"])
        new = [s for s in res["sites"] if s["binding"].get("anonymous")][0]
        self.assertEqual(new["resolution"], "unsupported")

    def test_html_text_ignored_and_syntax_error_partial(self):
        res, _ = run("<html><?php foo(); ?><b>x</b>")
        self.assertEqual([s["raw"] for s in res["sites"]], ["foo"])
        bad, _ = run("<?php function broken( { foo(")
        self.assertTrue(bad["partial"])


@unittest.skipIf(SKIP is not None, SKIP or "")
class PhpRobustness(unittest.TestCase):
    def test_deep_concatenation_chain_is_iterative(self):
        res, _ = run("<?php include " + " . ".join(["'a'"] * 4000) + ";")
        inc = by(res["sites"], relation="includes")
        self.assertEqual(len(inc), 1)
        self.assertEqual(inc[0]["binding"]["specifier"], "a" * 4000)


if __name__ == "__main__":
    unittest.main()
