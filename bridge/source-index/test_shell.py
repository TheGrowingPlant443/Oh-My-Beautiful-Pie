"""Bash and PowerShell recipe fixtures.  Skipped with explicit reasons when a grammar is unavailable."""
import sys
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common  # noqa: E402
import lang_shell  # noqa: E402

PROBES = {lang: common.language_probe(lang, spec["node_kinds"]) for lang, spec in lang_shell.PROFILES.items()}


def skip_reason(lang):
    p = PROBES[lang]
    return None if p["available"] else f"GRAMMAR UNAVAILABLE ({lang}): {p['error']}"


def run(lang, text, path):
    data = text.encode("utf-8")
    ctx = common.Context(path, lang, data, deadline=time.monotonic() + 20)
    lang_shell.extract(lang, data, ctx)
    return ctx.result(), data


def by(items, **kw):
    return [i for i in items if all(i.get(k) == v for k, v in kw.items())]


def decls(sym):
    return [(d["name"], d["kind"]) for d in sym.get("bindings", {}).get("declarations", [])]


class GrammarProbe(unittest.TestCase):
    def test_required_node_kinds_present(self):
        for lang in lang_shell.PROFILES:
            with self.subTest(lang=lang):
                if skip_reason(lang):
                    self.skipTest(skip_reason(lang))
                self.assertIsNone(PROBES[lang]["error"], PROBES[lang]["error"])


@unittest.skipIf(skip_reason("bash") is not None, skip_reason("bash") or "")
class BashRecipe(unittest.TestCase):
    def go(self, text, path="scripts/run.sh"):
        return run("bash", text, path)

    def test_functions_and_commands(self):
        res, _ = self.go("foo() { echo hi; bar \"$1\"; }\nfunction baz { foo; }\nfoo\nls | grep x\nunused() { :; }\n")
        syms = res["symbols"]
        mod = by(syms, kind="module")[0]
        foo = by(syms, name="foo")[0]
        self.assertEqual(foo["kind"], "function")
        self.assertEqual(foo["ownerId"], mod["id"])
        d = [x for x in mod["bindings"]["declarations"] if x["name"] == "foo"][0]
        self.assertTrue(d["mutable"])  # shell functions are redefinable, never name-promoted
        self.assertTrue(by(syms, name="baz"))
        self.assertTrue(by(syms, name="unused"))
        calls = by(res["sites"], relation="calls")
        raws = [s["raw"] for s in calls]
        for expected in ("echo", "bar", "foo", "ls", "grep"):
            self.assertIn(expected, raws)
        echo = by(calls, raw="echo")[0]
        self.assertTrue(echo["binding"]["builtin"])
        self.assertEqual(echo["callerId"], foo["id"])
        self.assertEqual(echo["resolution"], "unresolved")
        top_foo = [s for s in calls if s["raw"] == "foo" and s["callerId"] == mod["id"]]
        self.assertEqual(len(top_foo), 1)
        self.assertIsNone(top_foo[0]["targetId"])
        self.assertEqual(len({s["id"] for s in syms}), len(syms))

    def test_source_literal_and_dynamic(self):
        res, _ = self.go(
            "source ./lib.sh\n. \"lib/x.sh\"\n. 'raw.sh'\nsource \"$DIR/x.sh\"\nsource \"$(dirname \"$0\")/lib.sh\"\nsource $x\nsource\n"
        )
        imps = by(res["sites"], relation="imports")
        self.assertEqual(len(imps), 7)
        lit = {s["binding"]["specifier"]: s["binding"] for s in imps if s["binding"]["specifier"] is not None}
        self.assertEqual(set(lit), {"./lib.sh", "lib/x.sh", "raw.sh"})
        self.assertEqual(lit["./lib.sh"]["kind"], "source")
        self.assertEqual(lit["./lib.sh"]["loader"], "source")
        self.assertEqual(lit["lib/x.sh"]["loader"], ".")
        self.assertTrue(lit["./lib.sh"]["cwdRelative"])
        dyn = by(imps, resolution="dynamic")
        self.assertEqual(len(dyn), 4)
        # source is an import fact, not also a command call
        self.assertFalse([s for s in res["sites"] if s["relation"] == "calls" and s["raw"] in ("source", ".")])

    def test_dynamic_command_names_eval_and_conditional(self):
        res, _ = self.go(
            "\"$cmd\" arg\n$dyn x\neval \"x\"\na && b || c\nif t; then u; else v; fi\nw\n./local.sh\nVAR=1 env cmd\nx=$(date)\n"
        )
        calls = by(res["sites"], relation="calls")
        dyn = by(calls, resolution="dynamic")
        self.assertGreaterEqual(len(dyn), 3)
        self.assertTrue(by(calls, raw="eval")[0]["binding"]["dynamic"])
        self.assertFalse(by(calls, raw="a")[0]["conditional"])
        self.assertTrue(by(calls, raw="b")[0]["conditional"])
        self.assertTrue(by(calls, raw="c")[0]["conditional"])
        self.assertTrue(by(calls, raw="u")[0]["conditional"])
        self.assertFalse(by(calls, raw="w")[0]["conditional"])
        self.assertTrue(by(calls, raw="./local.sh")[0]["binding"]["pathLike"])
        self.assertTrue(by(calls, raw="env"))
        self.assertTrue(by(calls, raw="date"))
        for s in res["sites"]:
            self.assertIsNone(s["targetId"])

    def test_unicode_case_and_syntax_error(self):
        res, data = self.go("café() { :; }\nCafé() { :; }\ncafe() { :; }\n")
        self.assertEqual(sorted(s["name"] for s in res["symbols"] if s["kind"] == "function"), ["Café", "cafe", "café"])
        cafe = by(res["symbols"], name="café")[0]
        sp = cafe["span"]
        self.assertEqual(data[sp["byteStart"]:sp["byteEnd"]].decode(), "café() { :; }")
        bad, _ = self.go("foo() { if then fi\n")
        self.assertTrue(bad["partial"])


@unittest.skipIf(skip_reason("powershell") is not None, skip_reason("powershell") or "")
class PowerShellRecipe(unittest.TestCase):
    def go(self, text, path="scripts/tool.ps1"):
        return run("powershell", text, path)

    def test_declarations(self):
        res, _ = self.go(
            "function Get-Thing {\n    param($a)\n    Get-Content $a\n    Invoke-Other\n}\nfunction global:G { Get-Thing }\n"
            "function F($p1, [int]$p2) { $p1 }\nclass D : B {\n  static [int] S() { return 1 }\n  [void] M() { Get-Thing; $this.Other() }\n  D([int]$a) {}\n}\n"
            "enum Color { Red }\n$sb = { param($x) $x }\n"
        )
        syms = res["symbols"]
        mod = by(syms, kind="module")[0]
        gt = by(syms, name="Get-Thing")[0]
        self.assertEqual(gt["kind"], "function")
        self.assertIn(("$a", "parameter"), decls(gt))
        g = by(syms, name="G")[0]
        self.assertEqual(g["bindings"]["scope"], "global")
        self.assertIn(("$p1", "parameter"), decls(by(syms, name="F")[0]))
        d = by(syms, kind="class", name="D")[0]
        self.assertEqual(d["bindings"]["extends"], ["B"])
        self.assertTrue(by(syms, name="S")[0]["bindings"]["static"])
        self.assertEqual(by(syms, name="M")[0]["ownerId"], d["id"])
        self.assertEqual(by(syms, kind="constructor")[0]["name"], "D")
        self.assertTrue(by(syms, kind="enum", name="Color"))
        sb = by(syms, kind="lambda")[0]
        self.assertEqual(sb["bindings"]["assignedTo"], "$sb")
        self.assertIn(("$x", "parameter"), decls(sb))
        self.assertIn(("$sb", "variable"), decls(mod))
        self.assertEqual(len({s["id"] for s in syms}), len(syms))

    def test_commands_and_invocations(self):
        res, _ = self.go(
            "function Get-Thing { Write-Host \"hi\"; Get-Thing; & $x; & 'C:\\x\\y.ps1' -Arg; iex $cmd }\n"
            "$o.Method()\n$this.Other()\n[Foo]::Bar(1)\n$z = [Foo.Ns.Baz]::new(1)\n$p = New-Object System.Text.StringBuilder\n"
        )
        calls = by(res["sites"], relation="calls")
        gt = by(res["symbols"], name="Get-Thing")[0]
        wh = by(calls, raw="Write-Host")[0]
        self.assertEqual(wh["callerId"], gt["id"])
        self.assertTrue(wh["binding"]["caseInsensitive"])
        recursion = [s for s in calls if s["raw"] == "Get-Thing" and s["callerId"] == gt["id"]]
        self.assertEqual(len(recursion), 1)
        self.assertIsNone(recursion[0]["targetId"])
        self.assertEqual(recursion[0]["resolution"], "unresolved")
        dyn_amp = [s for s in calls if s["binding"].get("callOperator") == "&" and s["binding"]["name"] is None]
        self.assertEqual(len(dyn_amp), 1)
        self.assertEqual(dyn_amp[0]["resolution"], "dynamic")
        lit_amp = [s for s in calls if s["binding"].get("callOperator") == "&" and s["binding"]["name"]][0]
        self.assertEqual(lit_amp["binding"]["name"], "C:\\x\\y.ps1")
        self.assertEqual(by(calls, raw="iex")[0]["resolution"], "dynamic")
        m = by(calls, raw="$o.Method")[0]
        self.assertEqual(m["binding"]["kind"], "member")
        self.assertEqual(by(calls, raw="$this.Other")[0]["binding"]["receiverKind"], "this")
        static = by(calls, raw="[Foo]::Bar")[0]
        self.assertEqual(static["binding"]["kind"], "path")
        self.assertEqual(static["binding"]["receiver"], "Foo")
        new = by(calls, raw="[Foo.Ns.Baz]::new")[0]
        self.assertEqual(new["binding"]["kind"], "new")
        self.assertEqual(new["binding"]["name"], "Baz")
        self.assertEqual(new["binding"]["receiver"], "Foo.Ns")
        self.assertTrue(by(calls, raw="New-Object"))
        for s in res["sites"]:
            self.assertIsNone(s["targetId"])

    def test_imports_dot_source_module_using(self):
        res, _ = self.go(
            ". \"$PSScriptRoot\\lib.ps1\"\n. .\\other.ps1\n. $dyn\nImport-Module -Name Foo -Force\nImport-Module .\\Mod.psm1\n"
            "Import-Module $name\nusing module Bar\nusing namespace System.IO\nimport-module \"$PSScriptRoot\\m\\x.psd1\"\n"
        )
        imps = by(res["sites"], relation="imports")
        self.assertEqual(len(imps), 9)
        spec = {(s["binding"]["kind"], s["binding"]["specifier"]): s["binding"] for s in imps}
        self.assertEqual(spec[("source", "lib.ps1")]["relativeTo"], "dir")
        self.assertEqual(spec[("source", "lib.ps1")]["loader"], "dot-source")
        self.assertIn(("source", ".\\other.ps1"), spec)
        self.assertEqual(spec[("import", "Foo")]["loader"], "Import-Module")
        self.assertFalse(spec[("import", "Foo")]["pathLike"])
        self.assertTrue(spec[("import", ".\\Mod.psm1")]["pathLike"])
        self.assertEqual(spec[("using", "Bar")]["typeKind"], "module")
        self.assertTrue(spec[("using", "System.IO")]["namespace"])
        self.assertEqual(spec[("import", "m\\x.psd1")]["relativeTo"], "dir")
        dyn = by(imps, resolution="dynamic")
        self.assertEqual(len(dyn), 2)
        self.assertTrue(all(s["binding"]["specifier"] is None for s in dyn))
        # imports are not also calls
        self.assertFalse([s for s in res["sites"] if s["relation"] == "calls" and s["raw"].lower().startswith(("import-module", "using"))])

    def test_conditional_keywords_unicode_case(self):
        res, data = self.go(
            "if ($a) {\n  Invoke-A\n} else {\n  Invoke-B\n}\nInvoke-C\nfunction Café {}\nfunction cafe {}\nfunction CAFÉ {}\n"
        )
        calls = {s["raw"]: s for s in by(res["sites"], relation="calls")}
        self.assertTrue(calls["Invoke-A"]["conditional"])
        self.assertTrue(calls["Invoke-B"]["conditional"])
        self.assertFalse(calls["Invoke-C"]["conditional"])
        self.assertNotIn("if", calls)
        names = sorted(s["name"] for s in res["symbols"] if s["kind"] == "function")
        self.assertEqual(names, ["CAFÉ", "Café", "cafe"])
        cafe = by(res["symbols"], name="Café")[0]
        sp = cafe["span"]
        self.assertEqual(data[sp["byteStart"]:sp["byteEnd"]].decode(), "function Café {}")

    def test_syntax_error_is_partial(self):
        bad, _ = self.go("function ( { Get-Thing (\n")
        self.assertTrue(bad["partial"])


@unittest.skipIf(skip_reason("powershell") is not None, skip_reason("powershell") or "")
class PowerShellGrammarLimits(unittest.TestCase):
    def test_do_prefixed_command_is_reported_not_silently_dropped(self):
        # tree-sitter-powershell lexes the `Do` keyword before `-A`; the resulting ERROR must surface as partial
        res, _ = run("powershell", "if ($a) { Do-A }\n", "scripts/x.ps1")
        self.assertTrue(res["partial"])
        self.assertTrue(any("syntax" in d for d in res["diagnostics"]))


if __name__ == "__main__":
    unittest.main()
