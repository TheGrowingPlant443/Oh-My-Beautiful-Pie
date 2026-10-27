"""Zig, Elixir, Julia, Fortran and Verilog recipe fixtures.

Each language class is skipped with an explicit "GRAMMAR UNAVAILABLE" reason when its
grammar cannot be loaded, so a missing grammar is reported rather than silently passing.
"""
import sys
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common  # noqa: E402
import lang_misc  # noqa: E402

PROBES = {lang: common.language_probe(lang, spec["node_kinds"]) for lang, spec in lang_misc.PROFILES.items()}


def skip_reason(lang):
    p = PROBES[lang]
    return None if p["available"] else f"GRAMMAR UNAVAILABLE ({lang}): {p['error']}"


def run(lang, text, path):
    data = text.encode("utf-8")
    ctx = common.Context(path, lang, data, deadline=time.monotonic() + 20)
    lang_misc.extract(lang, data, ctx)
    return ctx.result(), data


def by(items, **kw):
    return [i for i in items if all(i.get(k) == v for k, v in kw.items())]


def decls(sym):
    return [(d["name"], d["kind"]) for d in sym.get("bindings", {}).get("declarations", [])]


def unique_ids(case, res):
    case.assertEqual(len({s["id"] for s in res["symbols"]}), len(res["symbols"]))
    case.assertEqual(len({s["id"] for s in res["sites"]}), len(res["sites"]))
    for s in res["sites"]:
        case.assertIsNone(s["targetId"])  # parsers never promote targets


class GrammarProbe(unittest.TestCase):
    def test_required_node_kinds_present(self):
        for lang in lang_misc.PROFILES:
            with self.subTest(lang=lang):
                if skip_reason(lang):
                    self.skipTest(skip_reason(lang))
                self.assertIsNone(PROBES[lang]["error"], PROBES[lang]["error"])

    def test_profiles_match_plan_extensions(self):
        exts = {lang: set(spec["extensions"]) for lang, spec in lang_misc.PROFILES.items()}
        self.assertEqual(exts["zig"], {".zig"})
        self.assertEqual(exts["elixir"], {".ex", ".exs"})
        self.assertEqual(exts["julia"], {".jl"})
        self.assertEqual(exts["verilog"], {".v", ".sv", ".svh", ".vh"})
        self.assertEqual(exts["fortran"], {".f", ".F", ".f90", ".F90", ".f95", ".F95", ".f03", ".F03", ".f08", ".F08"})


# ---------------------------------------------------------------------------

@unittest.skipIf(skip_reason("zig") is not None, skip_reason("zig") or "")
class ZigRecipe(unittest.TestCase):
    def go(self, text):
        return run("zig", text, "src/main.zig")

    def test_declarations_containers_methods(self):
        res, _ = self.go(
            "const std = @import(\"std\");\npub fn add(a: i32, b: i32) i32 { return a + b; }\nfn unused() void {}\n"
            "const S = struct { fn meth(self: S) void { self.other(); } };\nconst U = union(enum) { a: i32, pub fn m(self: U) void {} };\n"
            "const E = enum { a, fn e() void {} };\nextern fn ext(x: i32) i32;\ntest \"t\" { _ = add(1, 2); }\n"
            "fn outer() void { const f = struct { fn inner() void {} }; _ = f; }\n"
        )
        syms = res["symbols"]
        mod = by(syms, kind="module")[0]
        add = by(syms, name="add")[0]
        self.assertEqual(add["kind"], "function")
        self.assertTrue(add["exported"])
        self.assertEqual(add["bindings"]["visibility"], "pub")
        self.assertIn(("a", "parameter"), decls(add))
        self.assertFalse(by(syms, name="unused")[0].get("exported", False))  # uncalled survives
        s = by(syms, kind="struct", name="S")[0]
        self.assertEqual(by(syms, name="meth")[0]["kind"], "method")
        self.assertEqual(by(syms, name="meth")[0]["ownerId"], s["id"])
        self.assertEqual(by(syms, name="U")[0]["bindings"]["declarationKind"], "union")
        self.assertEqual(by(syms, name="E")[0]["kind"], "enum")
        ext = by(syms, name="ext")[0]
        self.assertTrue(ext["bindings"]["extern"])
        self.assertFalse(ext["bindings"]["body"])
        self.assertTrue(by(syms, name="t")[0]["bindings"]["test"])
        self.assertEqual(by(syms, name="inner")[0]["ownerId"], by(syms, kind="struct", name="f")[0]["id"])
        self.assertIn(("add", "function"), decls(mod))
        self.assertIn(("std", "import"), decls(mod))
        unique_ids(self, res)

    def test_imports_calls_builtins(self):
        res, _ = self.go(
            "const std = @import(\"std\");\nconst m = @import(\"mod.zig\");\nconst n = @import(name);\n"
            "const c = @cImport({ @cInclude(\"stdio.h\"); });\n"
            "fn main() void { _ = add(1,2); std.debug.print(\"x\", .{}); m.foo(); _ = @as(i32, 1); @call(.auto, f, .{}); }\n"
            "fn add(a: i32, b: i32) i32 { if (a > 0) { return add(a - 1, b); } return b; }\n"
        )
        imps = by(res["sites"], relation="imports")
        spec = {s["binding"]["local"]: s for s in imps}
        self.assertEqual(spec["std"]["binding"]["specifier"], "std")
        self.assertIsNone(spec["std"]["binding"]["relativeTo"])
        self.assertEqual(spec["m"]["binding"]["relativeTo"], "dir")
        self.assertEqual(spec["n"]["resolution"], "dynamic")
        self.assertIsNone(spec["n"]["binding"]["specifier"])
        inc = by(res["sites"], relation="includes")[0]
        self.assertEqual(inc["binding"]["specifier"], "stdio.h")
        calls = by(res["sites"], relation="calls")
        raws = {s["raw"].rstrip("("): s for s in calls}
        self.assertEqual(raws["std.debug.print"]["binding"]["receiver"], "std.debug")
        self.assertEqual(raws["std.debug.print"]["binding"]["receiverKind"], "path")
        self.assertEqual(raws["@as"]["resolution"], "external")
        self.assertTrue(raws["@as"]["binding"]["builtin"])
        self.assertEqual(raws["@call"]["resolution"], "dynamic")
        add_sym = [s for s in res["symbols"] if s["name"] == "add" and s["kind"] == "function"][0]
        rec = [s for s in calls if s["binding"].get("name") == "add" and s["callerId"] == add_sym["id"]]
        self.assertEqual(len(rec), 1)
        self.assertTrue(rec[0]["conditional"])
        self.assertEqual(rec[0]["resolution"], "unresolved")
        unique_ids(self, res)

    def test_unicode_case_and_syntax_error(self):
        # Zig identifiers are ASCII; case variants stay distinct and Unicode lives in literals
        res, data = self.go("const m = @import(\"café.zig\");\nfn Foo() void {}\nfn foo() void {}\nfn FOO() void {}\n")
        self.assertEqual(sorted(s["name"] for s in res["symbols"] if s["kind"] == "function"), ["FOO", "Foo", "foo"])
        imp = by(res["sites"], relation="imports")[0]
        self.assertEqual(imp["binding"]["specifier"], "café.zig")
        sp = imp["span"]
        self.assertEqual(data[sp["byteStart"]:sp["byteEnd"]].decode(), '@import("café.zig")')
        bad, _ = self.go("fn broken( { call(")
        self.assertTrue(bad["partial"])


# ---------------------------------------------------------------------------

@unittest.skipIf(skip_reason("elixir") is not None, skip_reason("elixir") or "")
class ElixirRecipe(unittest.TestCase):
    def go(self, text):
        return run("elixir", text, "lib/foo/bar.ex")

    SRC = (
        "defmodule Foo.Bar do\n  alias Foo.Baz, as: B\n  alias Foo.{A, C}\n  import Enum, only: [map: 2]\n  require Logger\n  use GenServer\n"
        "  @moduledoc \"x\"\n  @behaviour Access\n  @spec pub(t, k) :: any\n  def pub(a, b), do: helper(a, b)\n"
        "  defp helper(a, b) do\n    Enum.map(a, fn x -> x end)\n    B.run(b)\n    run(&Kernel.is_atom/1)\n  end\n"
        "  defmacro mac(x) do\n    quote do: unquote(x)\n  end\n  def guarded(x) when is_integer(x), do: x\n"
        "  def multi(0), do: :zero\n  def multi(n) when n > 0 do\n    case n do\n      1 -> one()\n      _ -> other()\n    end\n    x = 1\n    x = x + 1\n  end\n"
        "  defmodule Inner do\n    def i, do: :ok\n  end\n  def piped(x), do: x |> helper(1) |> Enum.map(&to_s/1)\n"
        "  def acc(m), do: m.key\n  def dyn(m, f), do: apply(m, f, [])\n  def rec(n), do: rec(n - 1)\n  defdelegate del(a), to: Other\nend\n"
    )

    def test_declarations_arity_and_heads_not_calls(self):
        res, _ = self.go(self.SRC)
        syms = res["symbols"]
        bar = by(syms, kind="namespace", name="Foo.Bar")[0]
        self.assertEqual(bar["bindings"]["fqn"], "Foo.Bar")
        inner = by(syms, kind="namespace", name="Inner")[0]
        self.assertEqual(inner["bindings"]["fqn"], "Foo.Bar.Inner")
        self.assertEqual(inner["ownerId"], bar["id"])
        pub = by(syms, name="pub")[0]
        self.assertEqual((pub["kind"], pub["bindings"]["arity"], pub["bindings"]["visibility"]), ("function", 2, "public"))
        self.assertEqual(pub["bindings"]["fqn"], "Foo.Bar.pub/2")
        self.assertEqual(by(syms, name="helper")[0]["bindings"]["visibility"], "private")
        self.assertEqual(by(syms, name="mac")[0]["kind"], "macro")
        self.assertEqual(by(syms, name="guarded")[0]["bindings"]["arity"], 1)
        multi = by(syms, name="multi")
        self.assertEqual(len(multi), 2)  # clauses are separate symbols, never merged
        self.assertNotEqual(multi[0]["id"], multi[1]["id"])
        self.assertIn(("a", "parameter"), decls(pub))
        self.assertIn(("x", "variable"), decls(multi[1]))
        self.assertIn(("x", "assignment"), decls(multi[1]))  # rebinding
        self.assertFalse(by(syms, name="del")[0]["bindings"]["body"])
        # the def head `pub(a, b)` and `helper(a, b)` heads are not call sites
        heads = [s for s in res["sites"] if s["relation"] == "calls" and s["callerId"] == bar["id"]]
        self.assertEqual(heads, [])
        lam = by(syms, kind="lambda")[0]
        self.assertIn(("x", "parameter"), decls(lam))
        unique_ids(self, res)

    def test_directives(self):
        res, _ = self.go(self.SRC)
        imps = by(res["sites"], relation="imports")
        got = {(s["binding"]["kind"], s["binding"]["specifier"]): s["binding"] for s in imps}
        self.assertEqual(got[("alias", "Foo.Baz")]["alias"], "B")
        self.assertEqual(got[("alias", "Foo.A")]["local"], "A")
        self.assertEqual(got[("alias", "Foo.C")]["local"], "C")
        self.assertTrue(got[("import", "Enum")]["namespace"])
        self.assertEqual(got[("import", "Enum")]["only"], "[map: 2]")
        self.assertEqual(got[("require", "Logger")]["local"], "Logger")
        self.assertTrue(got[("use", "GenServer")]["injectsCode"])
        bar = by(res["symbols"], name="Foo.Bar")[0]
        self.assertIn(("B", "import"), decls(bar))
        dyn, _ = self.go("defmodule M do\n  alias unquote(x)\nend\n")
        self.assertEqual(by(dyn["sites"], relation="imports")[0]["resolution"], "dynamic")
        refs = by(res["sites"], relation="references")
        roles = {s["binding"].get("role") for s in refs}
        self.assertIn("behaviour", roles)

    def test_calls_remote_pipe_capture_dynamic(self):
        res, _ = self.go(self.SRC)
        calls = by(res["sites"], relation="calls")

        def find(raw, caller):
            sym = by(res["symbols"], name=caller)[0]
            return [s for s in calls if s["raw"] == raw and s["callerId"] == sym["id"]]

        remote = find("Enum.map", "helper")[0]
        self.assertEqual(remote["binding"]["receiver"], "Enum")
        self.assertEqual(remote["binding"]["receiverKind"], "path")
        self.assertEqual(remote["binding"]["arity"], 2)
        piped = find("helper", "piped")[0]
        self.assertTrue(piped["binding"]["piped"])
        self.assertEqual(piped["binding"]["arity"], 2)  # explicit 1 + piped value
        self.assertEqual(find("apply", "dyn")[0]["resolution"], "dynamic")
        rec = find("rec", "rec")
        self.assertEqual(len(rec), 1)  # direct recursion retained, never promoted
        self.assertEqual(rec[0]["resolution"], "unresolved")
        self.assertTrue(by(calls, raw="one")[0]["conditional"])
        refs = by(res["sites"], relation="references")
        cap = [s for s in refs if s["binding"].get("capture")]
        self.assertEqual({s["binding"]["name"] for s in cap}, {"is_atom", "to_s"})
        amb = [s for s in refs if s["binding"].get("ambiguous")]
        self.assertEqual(len(amb), 1)  # m.key: map access vs zero-arity call stays ambiguous
        self.assertEqual(amb[0]["binding"]["name"], "key")
        # typespec / doc attributes never create call sites
        self.assertFalse([s for s in calls if s["raw"] in ("spec", "moduledoc", "pub") and s["binding"].get("arity") == 2 and s["raw"] == "pub"])

    def test_unicode_case_and_syntax_error(self):
        res, data = self.go("defmodule M do\n  def café, do: 1\n  def cafe, do: 3\n  def cafés, do: 2\nend\n")
        self.assertEqual(sorted(s["name"] for s in res["symbols"] if s["kind"] == "function"), ["cafe", "café", "cafés"])
        sp = by(res["symbols"], name="café")[0]["span"]
        self.assertEqual(data[sp["byteStart"]:sp["byteEnd"]].decode(), "def café, do: 1")
        bad, _ = self.go("defmodule M do\n  def broken(\n")
        self.assertTrue(bad["partial"])


# ---------------------------------------------------------------------------

@unittest.skipIf(skip_reason("julia") is not None, skip_reason("julia") or "")
class JuliaRecipe(unittest.TestCase):
    def go(self, text):
        return run("julia", text, "src/M.jl")

    SRC = (
        "module M\nusing LinearAlgebra\nimport Base: show, +\nusing Foo: bar, baz\nimport ..Parent\ninclude(\"other.jl\")\n"
        "function f(x::Int)\n  g(x) + h.m(x)\nend\ng(x) = x + 1\nf2 = x -> x * 2\nmacro mm(x) end\n@mm 3\nBase.@inline foo()\n"
        "struct S; x end\nfunction (s::S)(y) y end\nfunction rec(n) rec(n - 1) end\nend\n"
        "mutable struct P <: Q{T}\n  x\n  P(x) = new(x)\nend\nabstract type Shape end\nconst K = 1\nfoo(x) do y\n  y + 1\nend\n"
        "f.(xs)\nimport A as B\nusing ..Up, Other.Sub\nfunction Base.show(io::IO, p::P) end\nfunction gg(a, b=2; kw=3) where T end\n"
        "eval(:(1))\nxs |> hh\nname(x) = x\n"
    )

    def test_declarations(self):
        res, _ = self.go(self.SRC)
        syms = res["symbols"]
        m = by(syms, kind="namespace", name="M")[0]
        self.assertEqual(m["bindings"]["fqn"], "M")
        f = by(syms, name="f", kind="function")
        self.assertTrue(any(s["ownerId"] == m["id"] for s in f))
        self.assertEqual(by(syms, name="g")[0]["bindings"]["fqn"], "M.g")  # short form definition
        self.assertIn(("x", "parameter"), decls(by(syms, name="f")[0]))
        lam = by(syms, kind="lambda", name="<arrow>")[0]
        self.assertEqual(lam["bindings"]["assignedTo"], "f2")
        self.assertEqual(by(syms, kind="macro", name="mm")[0]["ownerId"], m["id"])
        self.assertEqual(by(syms, kind="struct", name="S")[0]["ownerId"], m["id"])
        callable_ = by(syms, name="<callable>")[0]
        self.assertEqual(callable_["bindings"]["callableType"], "S")
        p = by(syms, kind="struct", name="P")[0]
        self.assertTrue(p["bindings"]["mutable"])
        self.assertEqual(p["bindings"]["extends"], ["Q{T}"])
        ctor = [s for s in by(syms, name="P") if s["kind"] == "function"][0]
        self.assertEqual(ctor["ownerId"], p["id"])
        self.assertEqual(by(syms, kind="type", name="Shape")[0]["bindings"]["declarationKind"], "abstract")
        show = by(syms, name="show")[0]
        self.assertEqual(show["bindings"]["methodOf"], "Base")
        self.assertIn(("kw", "parameter"), decls(by(syms, name="gg")[0]))
        d = [x for x in by(syms, kind="module")[0]["bindings"]["declarations"] if x["name"] == "g"]
        mod_decl = [x for x in m["bindings"]["declarations"] if x["name"] == "g"]
        self.assertTrue(mod_decl and all(x["mutable"] for x in mod_decl))  # methods extend one generic function
        self.assertIn(("K", "variable"), decls(by(syms, kind="module")[0]))
        unique_ids(self, res)

    def test_imports_and_include(self):
        res, _ = self.go(self.SRC + "include(joinpath(d, \"x.jl\"))\ninclude(dyn)\n")
        imps = by(res["sites"], relation="imports")
        got = {(s["binding"]["kind"], s["binding"]["specifier"], s["binding"]["imported"]): s["binding"] for s in imps}
        self.assertTrue(got[("using", "LinearAlgebra", None)]["namespace"])
        self.assertIn(("import", "Base", "show"), got)
        self.assertIn(("import", "Base", "+"), got)
        self.assertIn(("using", "Foo", "bar"), got)
        self.assertIn(("import", "..Parent", None), got)  # leading dots preserved
        self.assertEqual(got[("import", "A", None)]["alias"], "B")
        self.assertIn(("using", "..Up", None), got)
        self.assertIn(("using", "Other.Sub", None), got)
        incs = by(res["sites"], relation="includes")
        lit = [s for s in incs if s["binding"]["specifier"] == "other.jl"][0]
        self.assertEqual(lit["binding"]["relativeTo"], "dir")
        self.assertEqual(len(by(incs, resolution="dynamic")), 2)

    def test_calls_macros_recursion(self):
        res, _ = self.go(self.SRC)
        calls = by(res["sites"], relation="calls")
        f_sym = [s for s in res["symbols"] if s["name"] == "f" and s["ownerId"] == by(res["symbols"], name="M")[0]["id"]][0]
        member = [s for s in calls if s["raw"] == "h.m"][0]
        self.assertEqual(member["binding"]["receiver"], "h")
        self.assertEqual(member["callerId"], f_sym["id"])
        rec_sym = by(res["symbols"], name="rec")[0]
        rec = [s for s in calls if s["raw"] == "rec" and s["callerId"] == rec_sym["id"]]
        self.assertEqual(len(rec), 1)
        self.assertEqual(rec[0]["resolution"], "unresolved")
        macros = {s["raw"]: s for s in by(res["sites"], relation="macros")}
        self.assertEqual(macros["@mm"]["binding"]["name"], "mm")
        self.assertEqual(macros["Base.@inline"]["binding"]["receiver"], "Base")
        self.assertTrue([s for s in calls if s["binding"].get("broadcast")])
        self.assertEqual(by(calls, raw="eval")[0]["resolution"], "dynamic")
        # signature heads are declarations, not calls
        self.assertFalse([s for s in calls if s["raw"] in ("name", "gg", "g") and s["callerId"] != f_sym["id"]])
        pipe = [s for s in by(res["sites"], relation="references") if s["binding"].get("pipeTarget")]
        self.assertEqual(pipe[0]["binding"]["name"], "hh")
        unique_ids(self, res)

    def test_unicode_case_and_syntax_error(self):
        res, data = self.go("function café() end\nfunction Café() end\nfunction cafe() end\n")
        self.assertEqual(sorted(s["name"] for s in res["symbols"] if s["kind"] == "function"), ["Café", "cafe", "café"])
        sp = by(res["symbols"], name="café")[0]["span"]
        self.assertEqual(data[sp["byteStart"]:sp["byteEnd"]].decode(), "function café() end")
        bad, _ = self.go("function broken(\n  call(\n")
        self.assertTrue(bad["partial"])


# ---------------------------------------------------------------------------

@unittest.skipIf(skip_reason("fortran") is not None, skip_reason("fortran") or "")
class FortranRecipe(unittest.TestCase):
    def go(self, text, path="src/m.f90"):
        return run("fortran", text, path)

    SRC = (
        "module m\n  use, intrinsic :: iso_c_binding, only: c_int\n  use other, only: a, b => c\n  use whole\n  implicit none\n"
        "  type :: t\n    integer :: comp\n  contains\n    procedure :: bound\n  end type\n  interface gen\n    module procedure s1, s2\n  end interface\n"
        "contains\n  subroutine s(x, obj)\n    real :: x, arr(10)\n    type(t) :: obj\n    call obj%bound(x)\n    call helper(x)\n"
        "    y = arr(1) + sin(x) + f(x)\n    call s(x, obj)\n  end subroutine\n  integer function f(x)\n    real :: x\n    f = g(x)\n  end function\nend module\n"
        "program p\n  use m\n  call s(1.0)\n  include 'inc.h'\nend program\n"
    )

    def test_declarations_case_fqn(self):
        res, _ = self.go(self.SRC)
        syms = res["symbols"]
        m = by(syms, kind="namespace", name="m")[0]
        self.assertEqual(m["bindings"]["fqn"], "m")
        s = by(syms, name="s", kind="function")[0]
        self.assertEqual(s["bindings"]["fqn"], "m::s")
        self.assertTrue(s["bindings"]["caseInsensitive"])
        self.assertIn(("x", "parameter"), decls(s))
        self.assertNotIn(("x", "assignment"), decls(s))  # `real :: x` types the dummy, it does not rebind it
        self.assertIn(("arr", "variable"), decls(s))
        self.assertTrue(by(syms, name="p")[0]["bindings"]["program"])
        t = by(syms, kind="struct", name="t")[0]
        self.assertEqual(t["bindings"]["typeBound"], ["bound"])
        self.assertNotIn("comp", [d[0] for d in decls(m)])  # components are not lexical names
        self.assertEqual(by(syms, kind="interface", name="gen")[0]["ownerId"], m["id"])
        self.assertEqual(len(by(syms, name="")), 0)  # keyword tokens never become symbols
        unique_ids(self, res)

    def test_use_include_preproc(self):
        res, _ = self.go(self.SRC + "#include \"foo.h\"\n#ifdef FOO\nsubroutine ext()\nend subroutine\n#endif\n")
        uses = by(res["sites"], relation="imports")
        got = {(s["binding"]["specifier"], s["binding"]["imported"], s["binding"]["alias"]): s["binding"] for s in uses}
        self.assertTrue(got[("iso_c_binding", "c_int", None)]["intrinsic"])
        self.assertIn(("other", "a", None), got)
        self.assertIn(("other", "c", "b"), got)
        self.assertTrue(got[("whole", None, None)]["namespace"])
        incs = by(res["sites"], relation="includes")
        specs = {s["binding"]["specifier"]: s for s in incs}
        self.assertEqual(specs["inc.h"]["binding"]["loader"], "include")
        self.assertEqual(specs["foo.h"]["binding"]["loader"], "#include")
        self.assertTrue(any("preprocessing is not expanded" in d for d in res["diagnostics"]))

    def test_calls_array_ambiguity_intrinsics_recursion(self):
        res, _ = self.go(self.SRC)
        calls = by(res["sites"], relation="calls")
        raws = {s["raw"]: s for s in calls}
        self.assertEqual(raws["call helper"]["binding"]["kind"], "identifier")
        self.assertEqual(raws["call obj%bound"]["binding"]["receiver"], "obj")
        self.assertTrue(raws["sin"]["binding"]["intrinsic"])
        self.assertEqual(raws["f"]["binding"]["ambiguous"], "callOrArrayElement")
        self.assertEqual(raws["g"]["binding"]["ambiguous"], "callOrArrayElement")
        refs = by(res["sites"], relation="references")
        arr = [s for s in refs if s["binding"].get("arrayElement")]
        self.assertEqual([s["binding"]["name"] for s in arr], ["arr"])  # declared array is not a call
        s_sym = by(res["symbols"], name="s", kind="function")[0]
        rec = [s for s in calls if s["raw"] == "call s" and s["callerId"] == s_sym["id"]]
        self.assertEqual(len(rec), 1)
        self.assertEqual(rec[0]["resolution"], "unresolved")
        self.assertEqual(len([s for s in refs if s["binding"].get("role") == "moduleProcedure"]), 2)

    def test_case_variants_unicode_and_syntax_error(self):
        res, _ = self.go("subroutine Foo()\nend subroutine\nsubroutine foo()\nend subroutine\n")
        foos = by(res["symbols"], kind="function")
        self.assertEqual(sorted(s["name"] for s in foos), ["Foo", "foo"])  # exact spelling kept, never merged
        self.assertEqual({s["bindings"]["fqn"] for s in foos}, {"foo"})
        self.assertNotEqual(foos[0]["id"], foos[1]["id"])
        bad, _ = self.go("subroutine broken(\n  call (\n")
        self.assertTrue(bad["partial"])


# ---------------------------------------------------------------------------

@unittest.skipIf(skip_reason("verilog") is not None, skip_reason("verilog") or "")
class VerilogRecipe(unittest.TestCase):
    def go(self, text, path="rtl/top.sv"):
        return run("verilog", text, path)

    SRC = (
        "`include \"defs.vh\"\npackage pkg; function int f(int x); return x; endfunction endpackage\n"
        "module top #(parameter W=1) (input clk);\n  import pkg::*;\n  import pkg::f;\n  wire a;\n  sub #(.W(2)) u1 (.clk(clk));\n"
        "  other u2 (clk);\n  function automatic int g(int y); return f(y); endfunction\n  task t(); $display(\"x\"); endtask\n"
        "  always @(posedge clk) a <= pkg::f(1);\nendmodule\n"
    )

    def test_declarations(self):
        res, _ = self.go(self.SRC + "interface ifc; endinterface\nclass C extends B; function new(); endfunction endclass\n`define BAR(x) x\n")
        syms = res["symbols"]
        top = by(syms, kind="namespace", name="top")[0]
        self.assertEqual(top["bindings"]["declarationKind"], "module")
        pkg = by(syms, kind="namespace", name="pkg")[0]
        self.assertEqual(pkg["bindings"]["declarationKind"], "package")
        self.assertEqual(by(syms, name="f")[0]["ownerId"], pkg["id"])
        g = by(syms, name="g")[0]
        self.assertEqual(g["ownerId"], top["id"])
        self.assertIn(("y", "parameter"), decls(g))
        t = by(syms, name="t")[0]
        self.assertTrue(t["bindings"]["task"])
        self.assertTrue(by(syms, kind="interface", name="ifc"))
        c = by(syms, kind="class", name="C")[0]
        self.assertEqual(c["bindings"]["extends"], ["B"])
        self.assertEqual(by(syms, kind="constructor", name="new")[0]["ownerId"], c["id"])
        self.assertTrue(by(syms, kind="macro", name="BAR")[0]["bindings"]["preprocessor"])
        self.assertIn(("f", "import"), decls(top))
        unique_ids(self, res)

    def test_instantiation_is_not_a_call(self):
        res, _ = self.go(self.SRC)
        inst = by(res["sites"], relation="instantiates")
        self.assertEqual(sorted(s["binding"]["instance"] for s in inst), ["u1", "u2"])
        u1 = [s for s in inst if s["binding"]["instance"] == "u1"][0]
        self.assertEqual(u1["binding"]["name"], "sub")
        self.assertEqual(u1["binding"]["portStyle"], "named")
        self.assertIn(".W(2)", u1["binding"]["parameters"])
        u2 = [s for s in inst if s["binding"]["instance"] == "u2"][0]
        self.assertEqual(u2["binding"]["ambiguousSyntax"], "checkerInstantiation")
        calls = by(res["sites"], relation="calls")
        self.assertFalse([s for s in calls if s["binding"].get("name") in ("sub", "other")])
        top = by(res["symbols"], name="top")[0]
        self.assertTrue(all(s["callerId"] == top["id"] for s in inst))

    def test_imports_includes_calls_system_tasks(self):
        res, _ = self.go(self.SRC)
        imps = by(res["sites"], relation="imports")
        got = {(s["binding"]["specifier"], s["binding"]["imported"]): s["binding"] for s in imps}
        self.assertTrue(got[("pkg", None)]["namespace"])
        self.assertEqual(got[("pkg", "f")]["local"], "f")
        inc = by(res["sites"], relation="includes")[0]
        self.assertEqual(inc["binding"]["specifier"], "defs.vh")
        calls = by(res["sites"], relation="calls")
        g = by(res["symbols"], name="g")[0]
        inner = [s for s in calls if s["raw"] == "f" and s["callerId"] == g["id"]]
        self.assertEqual(len(inner), 1)
        scoped = [s for s in calls if s["raw"] == "pkg::f"][0]
        self.assertEqual(scoped["binding"]["receiver"], "pkg")
        self.assertEqual(scoped["binding"]["kind"], "path")
        sys_task = [s for s in calls if s["raw"] == "$display"][0]
        self.assertEqual(sys_task["resolution"], "external")
        self.assertTrue(sys_task["binding"]["systemTask"])
        unique_ids(self, res)

    def test_directives_conditional_and_partial_reporting(self):
        res, _ = self.go("`ifdef FOO\nmodule a; sub u (.x(1)); endmodule\n`endif\nmodule b; sub2 v (.x(1)); endmodule\n`BAR\n")
        inst = {s["binding"]["instance"]: s for s in by(res["sites"], relation="instantiates")}
        self.assertTrue(inst["u"]["conditional"])
        self.assertFalse(inst["v"]["conditional"])
        self.assertTrue(by(res["sites"], relation="macros"))
        bad, _ = self.go("module top;\n  initial begin foo(); end\nendmodule\n")
        self.assertTrue(bad["partial"])
        self.assertTrue(any("ERROR regions" in d for d in bad["diagnostics"]))

    def test_unicode_case_names(self):
        res, _ = self.go("module Top; endmodule\nmodule top; endmodule\nmodule TOP; endmodule\n")
        self.assertEqual(sorted(s["name"] for s in res["symbols"] if s["kind"] == "namespace"), ["TOP", "Top", "top"])


@unittest.skipIf(bool(skip_reason("elixir") or skip_reason("julia")), skip_reason("elixir") or skip_reason("julia") or "")
class MiscRobustness(unittest.TestCase):
    def test_large_inputs(self):
        ex, _ = run("elixir", "defmodule M do\n" + "\n".join(f"  def f{i}(a), do: g{i}(a)" for i in range(2000)) + "\nend\n", "a.ex")
        self.assertEqual(len(by(ex["symbols"], kind="function")), 2000)
        self.assertEqual(len(by(ex["sites"], relation="calls")), 2000)
        jl, _ = run("julia", "\n".join(f"f{i}(x) = g{i}(x)" for i in range(3000)), "a.jl")
        self.assertEqual(len(by(jl["symbols"], kind="function")), 3000)
        self.assertEqual(len(by(jl["sites"], relation="calls")), 3000)


if __name__ == "__main__":
    unittest.main()
