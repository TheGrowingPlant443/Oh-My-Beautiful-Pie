"""Ruby recipe fixtures.  Skipped with an explicit reason when the grammar is unavailable."""
import sys
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common  # noqa: E402
import lang_ruby  # noqa: E402

LANG = "ruby"
PROBE = common.language_probe(LANG, lang_ruby.PROFILES[LANG]["node_kinds"])
SKIP = None if PROBE["available"] else f"GRAMMAR UNAVAILABLE ({LANG}): {PROBE['error']}"


def run(text, path="lib/app.rb"):
    data = text.encode("utf-8")
    ctx = common.Context(path, LANG, data, deadline=time.monotonic() + 20)
    lang_ruby.extract(LANG, data, ctx)
    return ctx.result(), data


def by(items, **kw):
    return [i for i in items if all(i.get(k) == v for k, v in kw.items())]


class GrammarProbe(unittest.TestCase):
    @unittest.skipIf(SKIP is not None, SKIP or "")
    def test_required_node_kinds_present(self):
        self.assertTrue(PROBE["available"])
        self.assertIsNone(PROBE["error"], PROBE["error"])


@unittest.skipIf(SKIP is not None, SKIP or "")
class RubyRecipe(unittest.TestCase):
    def test_declarations_and_ownership(self):
        res, _ = run(
            "module A\n  class B < C\n    def initialize(a); super; end\n    def self.make; new; end\n"
            "    def run(x) = helper(x)\n    def helper(y); y; end\n  end\nend\ndef top; end\n"
        )
        syms = res["symbols"]
        mod = by(syms, kind="module")
        self.assertEqual(len(mod), 1)
        self.assertIsNone(mod[0]["ownerId"])
        ns = by(syms, kind="namespace", name="A")[0]
        cls = by(syms, kind="class", name="B")[0]
        self.assertEqual(cls["ownerId"], ns["id"])
        self.assertEqual(cls["bindings"]["extends"], ["C"])
        self.assertEqual(cls["bindings"]["fqn"], "A::B")
        ctor = by(syms, kind="constructor", name="initialize")[0]
        self.assertEqual(ctor["ownerId"], cls["id"])
        make = by(syms, kind="method", name="make")[0]
        self.assertTrue(make["bindings"]["static"])
        self.assertEqual(by(syms, kind="method", name="run")[0]["ownerId"], cls["id"])
        top = by(syms, kind="function", name="top")[0]
        self.assertEqual(top["ownerId"], mod[0]["id"])
        # uncalled declaration survives
        self.assertTrue(by(syms, name="helper"))
        self.assertEqual(len({s["id"] for s in syms}), len(syms))

    def test_call_sites_have_owner_and_bindings(self):
        res, _ = run("class K\n  def run(x)\n    helper(x)\n    x.go\n    Foo::Bar.new\n    run(1)\n  end\nend\n")
        run_sym = by(res["symbols"], name="run")[0]
        sites = by(res["sites"], relation="calls")
        raws = {s["raw"]: s for s in sites}
        self.assertEqual(raws["helper"]["callerId"], run_sym["id"])
        self.assertEqual(raws["helper"]["binding"]["kind"], "identifier")
        self.assertEqual(raws["x.go"]["binding"]["kind"], "member")
        self.assertEqual(raws["x.go"]["binding"]["receiver"], "x")
        new = raws["Foo::Bar.new"]
        self.assertEqual(new["binding"]["kind"], "new")
        self.assertEqual(new["binding"]["name"], "Bar")
        self.assertEqual(new["binding"]["receiver"], "Foo")
        # direct recursion is a retained site, never promoted
        self.assertEqual(raws["run"]["callerId"], run_sym["id"])
        self.assertIsNone(raws["run"]["targetId"])
        self.assertEqual(raws["run"]["resolution"], "unresolved")
        for s in res["sites"]:
            self.assertIsNone(s["targetId"])

    def test_requires_literal_relative_dynamic(self):
        res, _ = run(
            "require 'json'\nrequire_relative \"./foo\"\nrequire name\nrequire \"a#{x}\"\nload 'tasks/x.rake'\n"
            "autoload :Thing, 'thing'\nif ok\n  require 'cond'\nend\n"
        )
        imps = by(res["sites"], relation="imports")
        spec = {s["binding"]["specifier"]: s for s in imps}
        self.assertEqual(spec["json"]["binding"]["kind"], "require")
        self.assertIsNone(spec["json"]["binding"]["relativeTo"])
        self.assertEqual(spec["./foo"]["binding"]["relativeTo"], "dir")
        self.assertEqual(spec["tasks/x.rake"]["binding"]["loader"], "load")
        self.assertEqual(spec["thing"]["binding"]["loader"], "autoload")
        self.assertTrue(spec["cond"]["conditional"])
        dyn = by(imps, resolution="dynamic")
        self.assertEqual(len(dyn), 2)
        self.assertTrue(all(s["binding"]["specifier"] is None for s in dyn))
        # imports are not also recorded as plain calls
        self.assertFalse([s for s in res["sites"] if s["relation"] == "calls" and s["raw"].startswith("require")])

    def test_closures_blocks_and_declarations(self):
        res, _ = run("def m(items)\n  l = ->(a) { a.zed }\n  items.each { |v| puts v }\n  lambda do |q| q end\n  l = 1\nend\n")
        lam = by(res["symbols"], kind="lambda")
        self.assertEqual(len(lam), 3)
        named = by(lam, name="<lambda>")
        self.assertEqual(len(named), 1)
        self.assertEqual(named[0]["bindings"]["assignedTo"], "l")
        meth = by(res["symbols"], name="m")[0]
        decls = meth["bindings"]["declarations"]
        kinds = [(d["name"], d["kind"]) for d in decls]
        self.assertIn(("items", "parameter"), kinds)
        self.assertIn(("l", "variable"), kinds)
        self.assertIn(("l", "assignment"), kinds)  # rebinding is visible to the resolver
        puts = by(res["sites"], raw="puts")[0]
        block = [l for l in lam if l["bindings"].get("blockOf") == "each"][0]
        self.assertEqual(puts["callerId"], block["id"])

    def test_dynamic_super_yield_mixins(self):
        res, _ = run("class A < ::Base\n  include Mod::Inner\n  def x(o)\n    o.send(:go)\n    yield 1\n    super\n  end\nend\n")
        refs = by(res["sites"], relation="references")
        roles = {s["binding"]["role"]: s for s in refs}
        self.assertEqual(roles["extends"]["binding"]["name"], "Base")
        self.assertEqual(roles["mixin"]["binding"]["receiver"], "Mod")
        self.assertEqual(roles["mixin"]["binding"]["name"], "Inner")
        send = by(res["sites"], raw="o.send")[0]
        self.assertEqual(send["resolution"], "dynamic")
        self.assertEqual(by(res["sites"], raw="yield")[0]["resolution"], "dynamic")
        sup = by(res["sites"], raw="super")[0]
        self.assertEqual(sup["binding"]["receiverKind"], "super")
        self.assertEqual(sup["binding"]["name"], "x")

    def test_same_name_case_and_unicode_ids(self):
        res, data = run("class A; def go; end; end\nclass B; def go; end; end\ndef Go; end\ndef go; end\ndef café; end\nclass Café; end\n")
        goes = by(res["symbols"], name="go")
        self.assertEqual(len(goes), 3)
        self.assertEqual(len({g["id"] for g in goes}), 3)
        self.assertTrue(by(res["symbols"], name="Go"))
        cafe = by(res["symbols"], name="café")[0]
        sp = cafe["span"]
        self.assertEqual(data[sp["byteStart"]:sp["byteEnd"]].decode(), "def café; end")
        self.assertTrue(by(res["symbols"], kind="class", name="Café"))

    def test_conditional_flag_and_syntax_error_is_partial(self):
        res, _ = run("def m\n  a()\n  if x\n    b()\n  end\nend\n")
        self.assertFalse(by(res["sites"], raw="a")[0]["conditional"])
        self.assertTrue(by(res["sites"], raw="b")[0]["conditional"])
        bad, _ = run("def broken(\n  foo(1\n")
        self.assertTrue(bad["partial"])
        self.assertTrue(any("syntax" in d for d in bad["diagnostics"]))


@unittest.skipIf(SKIP is not None, SKIP or "")
class RubyRobustness(unittest.TestCase):
    def test_keyword_tokens_never_become_symbols_and_large_input(self):
        res, _ = run("class A\n  def m; end\nend\nmodule B; end\n")
        self.assertEqual([s for s in res["symbols"] if s["name"] == ""], [])
        big, _ = run("\n".join(f"def m{i}; n{i}(1); end" for i in range(3000)))
        self.assertEqual(len(by(big["symbols"], kind="function")), 3000)
        self.assertEqual(len(by(big["sites"], relation="calls")), 3000)
        self.assertFalse(big["partial"])


if __name__ == "__main__":
    unittest.main()
