"""Hermetic tests for resolve.py using hand-authored index facts plus temporary config files.

No parser is involved: every fixture states the facts a parser would emit
(symbols, ``bindings.declarations``/``exports``, site bindings), so the resolver's
rules are verified independently of Tree-sitter.
"""
from __future__ import annotations

import hashlib
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import resolve  # noqa: E402

BIG = 1 << 40
sid = resolve.stable_id


class Fx:
    """Builds a SourceGraphIndex-shaped dict and the matching on-disk snapshot."""

    def __init__(self, root: str):
        self.root = root
        self.files: list[dict] = []
        self.symbols: list[dict] = []
        self.sites: list[dict] = []
        self.modules: dict[tuple, str] = {}
        self.sym_by_id: dict[str, dict] = {}
        self.n = 0
        self.lang_of: dict[str, str] = {}

    # -- inventory ----------------------------------------------------------
    def file(self, path: str, content: str | bytes | None = "", language: str | None = None, status: str = "parsed") -> str:
        if content is not None:
            data = content.encode("utf-8") if isinstance(content, str) else content
            full = os.path.join(self.root, *path.split("/"))
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "wb") as fh:
                fh.write(data)
            digest = hashlib.sha256(data).hexdigest()
            size = len(data)
        else:
            digest, size = "0" * 64, 0
        fid = sid("file", path)
        self.files.append({"id": fid, "path": path, "sha256": digest, "bytes": size, "language": language, "status": status, "diagnostics": []})
        if language:
            self.lang_of[path] = language
        return fid

    def raw_file(self, path: str, sha: str, content: bytes) -> str:
        """File whose on-disk bytes differ from its inventory hash."""
        full = os.path.join(self.root, *path.split("/"))
        os.makedirs(os.path.dirname(full), exist_ok=True)
        Path(full).write_bytes(content)
        fid = sid("file", path)
        self.files.append({"id": fid, "path": path, "sha256": sha, "bytes": len(content), "language": None, "status": "unsupported", "diagnostics": []})
        return fid

    def pos(self) -> int:
        self.n += 10
        return self.n

    def span(self, start: int | None = None) -> dict:
        s = self.pos() if start is None else start
        return {"byteStart": s, "byteEnd": s + 5, "lineStart": 1, "lineEnd": 1, "columnStart": 0, "columnEnd": 5}

    # -- symbols --------------------------------------------------------------
    def module(self, path: str, lang: str, region: str = "main", owner: str | None = None) -> str:
        key = (path, region)
        if key in self.modules:
            return self.modules[key]
        s = self.sym(path, lang, region, "module", owner=owner, region=region, _mod=True)
        self.modules[key] = s
        return s

    def sym(self, path: str, lang: str, name: str, kind: str, owner: str | None = None, *, region: str = "main", exported: bool | None = None, _mod: bool = False, **bindings) -> str:
        if owner is None and not _mod:
            owner = self.module(path, lang, region)
        span = self.span()
        symid = sid(path, lang, region, kind, span["byteStart"], span["byteEnd"])
        d = {
            "id": symid, "fileId": sid("file", path), "path": path, "language": lang, "region": region, "ownerId": owner,
            "kind": kind, "name": name, "qualifiedName": name, "span": span,
        }
        if exported is not None:
            d["exported"] = exported
        if bindings:
            d["bindings"] = dict(bindings)
        self.symbols.append(d)
        self.sym_by_id[symid] = d
        return symid

    def decl(self, owner: str, name: str, kind: str, symbol: str | None = None, site: str | None = None, lo: int = 0, hi: int = BIG) -> None:
        o = self.sym_by_id[owner]
        o.setdefault("bindings", {}).setdefault("declarations", []).append(
            {"name": name, "kind": kind, "symbolId": symbol, "siteId": site, "mutable": False, "position": lo, "scopeStart": lo, "scopeEnd": hi}
        )

    def fn(self, path: str, lang: str, name: str, owner: str | None = None, kind: str = "function", declare: bool = True, **kw) -> str:
        s = self.sym(path, lang, name, kind, owner, **kw)
        if declare:
            o = owner or self.module(path, lang)
            self.decl(o, name, "function", symbol=s)
        return s

    def export(self, path: str, lang: str, exported: str, symbol: str | None = None, local: str | None = None, default: bool = False) -> None:
        m = self.sym_by_id[self.module(path, lang)]
        m.setdefault("bindings", {}).setdefault("exports", []).append(
            {"exported": exported, "local": local, "symbolId": symbol, "default": default, "typeOnly": False}
        )

    # -- sites ----------------------------------------------------------------
    def site(self, path: str, lang: str, relation: str, raw: str, binding: dict, caller: str | None = None, *, region: str = "main",
             type_only: bool = False, resolution: str = "unresolved", at: int | None = None) -> str:
        span = self.span(at)
        stid = sid("site", path, lang, region, relation, span["byteStart"], span["byteEnd"])
        self.sites.append({
            "id": stid, "fileId": sid("file", path), "path": path, "language": lang, "region": region,
            "callerId": caller, "span": span, "relation": relation, "raw": raw, "targetId": None, "resolution": resolution,
            "conditional": False, "typeOnly": type_only, "candidateIds": [], "binding": binding,
        })
        return stid

    def imp(self, path: str, lang: str, spec: str | None, imported: str | None = None, local: str | None = None, *, default: bool = False,
            namespace: bool = False, type_only: bool = False, kind: str = "import", relation: str = "imports", caller: str | None = None,
            declare: bool = True, **extra) -> str:
        b = {"kind": kind, "specifier": spec, "imported": imported, "alias": local, "local": local, "default": default,
             "namespace": namespace, "reexport": False, "typeOnly": type_only, "typeKind": "type" if type_only else None}
        b.update(extra)
        s = self.site(path, lang, relation, spec or "", b, caller, type_only=type_only)
        if declare and local:
            self.decl(caller or self.module(path, lang), local, "import", site=s)
        return s

    def reexport(self, path: str, lang: str, spec: str, imported: str | None = None, exported: str | None = None, namespace: bool = False) -> str:
        b = {"kind": "exportFrom", "specifier": spec, "imported": imported, "alias": exported, "default": False, "namespace": namespace, "reexport": True, "typeOnly": False}
        return self.site(path, lang, "reexports", spec, b)

    def call(self, path: str, lang: str, name: str, receiver: str | None = None, caller: str | None = None, *, kind: str | None = None,
             relation: str = "calls", **extra) -> str:
        b = {"kind": kind or ("member" if receiver else "identifier"), "name": name, "receiver": receiver}
        b.update(extra)
        return self.site(path, lang, relation, (receiver + "." if receiver else "") + name + "()", b, caller)

    def index(self) -> dict:
        return {
            "schemaVersion": 1, "snapshotId": "s", "folder": "f", "sourceHash": "h", "extractorVersions": {},
            "files": self.files, "directories": [], "symbols": self.symbols, "sites": self.sites, "edges": [],
            "coverage": {"admittedFiles": len(self.files)}, "limitations": [],
        }


class ResolveCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = os.path.realpath(self._tmp.name)
        self.fx = Fx(self.root)

    def run_idx(self, fx: Fx | None = None) -> dict:
        fx = fx or self.fx
        self.idx = resolve.resolve(fx.index(), self.root)
        self.sites_by_id = {s["id"]: s for s in self.idx["sites"]}
        return self.idx

    def site_res(self, site_id: str):
        s = self.sites_by_id[site_id]
        return s["resolution"], s["targetId"], s["candidateIds"]

    def note(self, site_id: str) -> str:
        return ((self.sites_by_id[site_id].get("binding") or {}).get("resolved") or {}).get("note", "")

    def assertResolved(self, site_id: str, target: str):
        res = self.site_res(site_id)
        self.assertEqual(res[0], "resolvedStatic", (res, self.note(site_id)))
        self.assertEqual(res[1], target)
        self.assertEqual(res[2], [])

    def assertRes(self, site_id: str, resolution: str):
        got = self.site_res(site_id)[0]
        self.assertEqual(got, resolution, (got, self.note(site_id)))

    def edge(self, source: str, target: str, relation: str | None = None):
        return [e for e in self.idx["edges"] if e["sourceId"] == source and e["targetId"] == target and (relation is None or e["relation"] == relation)]

    def fid(self, path: str) -> str:
        return sid("file", path)


# ===========================================================================
# JavaScript / TypeScript
# ===========================================================================
class JsResolution(ResolveCase):
    TS = "typescript"

    def two_modules(self):
        fx = self.fx
        fx.file("src/a.ts", language="typescript")
        fx.file("src/b.ts", language="typescript")
        foo = fx.fn("src/b.ts", self.TS, "foo", exported=True)
        fx.export("src/b.ts", self.TS, "foo", symbol=foo)
        return foo

    def test_named_import_call_resolves_to_declaration_and_edges(self):
        fx = self.fx
        foo = self.two_modules()
        imp = fx.imp("src/a.ts", self.TS, "./b", "foo", "foo")
        call = fx.call("src/a.ts", self.TS, "foo")
        self.run_idx()
        self.assertResolved(imp, self.fid("src/b.ts"))
        self.assertResolved(call, foo)
        file_edges = self.edge(self.fid("src/a.ts"), self.fid("src/b.ts"), "imports")
        self.assertEqual(len(file_edges), 1)
        self.assertEqual(file_edges[0]["evidenceIds"], [imp])
        mod = fx.module("src/a.ts", self.TS)
        call_edges = self.edge(mod, foo, "calls")
        self.assertEqual(call_edges[0]["evidenceIds"], [call])
        self.assertEqual(call_edges[0]["resolution"], "resolvedStatic")

    def test_alias_and_default_and_namespace_imports(self):
        fx = self.fx
        fx.file("a.ts", language="typescript")
        fx.file("m.ts", language="typescript")
        foo = fx.fn("m.ts", self.TS, "foo", exported=True)
        dflt = fx.fn("m.ts", self.TS, "def", exported=True)
        fx.export("m.ts", self.TS, "foo", symbol=foo)
        fx.export("m.ts", self.TS, "default", symbol=dflt, default=True)
        fx.imp("a.ts", self.TS, "./m", "foo", "renamed")
        fx.imp("a.ts", self.TS, "./m", None, "D", default=True)
        fx.imp("a.ts", self.TS, "./m", None, "ns", namespace=True)
        c1 = fx.call("a.ts", self.TS, "renamed")
        c2 = fx.call("a.ts", self.TS, "D")
        c3 = fx.call("a.ts", self.TS, "foo", receiver="ns")
        c4 = fx.call("a.ts", self.TS, "foo")  # not imported under this name
        self.run_idx()
        self.assertResolved(c1, foo)
        self.assertResolved(c2, dflt)
        self.assertResolved(c3, foo)
        self.assertRes(c4, "unresolved")  # spelling alone never promotes

    def test_same_name_functions_resolve_by_binding_not_spelling(self):
        fx = self.fx
        for p in ("x.ts", "y.ts", "a.ts", "b.ts"):
            fx.file(p, language="typescript")
        fx_x = fx.fn("x.ts", self.TS, "run", exported=True)
        fx_y = fx.fn("y.ts", self.TS, "run", exported=True)
        fx.export("x.ts", self.TS, "run", symbol=fx_x)
        fx.export("y.ts", self.TS, "run", symbol=fx_y)
        fx.imp("a.ts", self.TS, "./x", "run", "run")
        fx.imp("b.ts", self.TS, "./y", "run", "run")
        ca = fx.call("a.ts", self.TS, "run")
        cb = fx.call("b.ts", self.TS, "run")
        self.run_idx()
        self.assertResolved(ca, fx_x)
        self.assertResolved(cb, fx_y)

    def test_extension_index_and_swap_resolution_and_case_sensitivity(self):
        fx = self.fx
        for p in ("src/a.ts", "src/b.ts", "src/dir/index.ts", "src/c.ts", "src/Case.ts", "src/ünï/ü.ts", "src/data.json"):
            fx.file(p, language="typescript" if p.endswith(".ts") else None)
        sites = {
            "b": fx.imp("src/a.ts", self.TS, "./b", "x", "x"),
            "dir": fx.imp("src/a.ts", self.TS, "./dir", "x", "y"),
            "swap": fx.imp("src/a.ts", self.TS, "./c.js", "x", "z"),
            "case": fx.imp("src/a.ts", self.TS, "./case", "x", "w"),
            "uni": fx.imp("src/a.ts", self.TS, "./ünï/ü", "x", "u"),
            "uni_case": fx.imp("src/a.ts", self.TS, "./Ünï/ü", "x", "v"),
            "json": fx.imp("src/a.ts", self.TS, "./data.json", None, "d", default=True),
            "missing": fx.imp("src/a.ts", self.TS, "./nope", "x", "n"),
        }
        self.run_idx()
        self.assertResolved(sites["b"], self.fid("src/b.ts"))
        self.assertResolved(sites["dir"], self.fid("src/dir/index.ts"))
        self.assertResolved(sites["swap"], self.fid("src/c.ts"))
        self.assertResolved(sites["uni"], self.fid("src/ünï/ü.ts"))
        self.assertResolved(sites["json"], self.fid("src/data.json"))
        self.assertRes(sites["case"], "unresolved")
        self.assertIn("case-mismatch", self.note(sites["case"]))
        self.assertRes(sites["uni_case"], "unresolved")
        self.assertRes(sites["missing"], "unresolved")

    def test_ts_and_js_with_same_stem_are_candidates(self):
        fx = self.fx
        for p in ("a.ts", "m.ts", "m.js"):
            fx.file(p, language="typescript")
        s = fx.imp("a.ts", self.TS, "./m", "x", "x")
        self.run_idx()
        res, target, cands = self.site_res(s)
        self.assertEqual((res, target), ("candidate", None))
        self.assertEqual(set(cands), {self.fid("m.ts"), self.fid("m.js")})

    def test_barrel_reexports_follow_chain_aliases_and_stars(self):
        fx = self.fx
        for p in ("app.ts", "index.ts", "impl/one.ts", "impl/two.ts", "impl/index.ts"):
            fx.file(p, language="typescript")
        one = fx.fn("impl/one.ts", self.TS, "one", exported=True)
        two = fx.fn("impl/two.ts", self.TS, "two", exported=True)
        fx.export("impl/one.ts", self.TS, "one", symbol=one)
        fx.export("impl/two.ts", self.TS, "two", symbol=two)
        fx.reexport("impl/index.ts", self.TS, "./one", namespace=True)  # export * from
        fx.reexport("impl/index.ts", self.TS, "./two", "two", "deux")  # export {two as deux} from
        fx.reexport("index.ts", self.TS, "./impl", namespace=True)
        fx.imp("app.ts", self.TS, "./index", "one", "one")
        fx.imp("app.ts", self.TS, "./index", "deux", "deux")
        fx.imp("app.ts", self.TS, "./index", "two", "two")  # not exported under that name
        c1 = fx.call("app.ts", self.TS, "one")
        c2 = fx.call("app.ts", self.TS, "deux")
        c3 = fx.call("app.ts", self.TS, "two")
        self.run_idx()
        self.assertResolved(c1, one)
        self.assertResolved(c2, two)
        self.assertRes(c3, "unresolved")

    def test_reexport_cycles_terminate_and_keep_real_exports(self):
        fx = self.fx
        for p in ("a.ts", "b.ts", "c.ts"):
            fx.file(p, language="typescript")
        fb = fx.fn("b.ts", self.TS, "fromB", exported=True)
        fx.export("b.ts", self.TS, "fromB", symbol=fb)
        fa = fx.fn("a.ts", self.TS, "fromA", exported=True)
        fx.export("a.ts", self.TS, "fromA", symbol=fa)
        fx.reexport("a.ts", self.TS, "./b", namespace=True)
        fx.reexport("b.ts", self.TS, "./a", namespace=True)
        fx.imp("c.ts", self.TS, "./a", "fromB", "fromB")
        fx.imp("c.ts", self.TS, "./b", "fromA", "fromA")
        fx.imp("c.ts", self.TS, "./a", "ghost", "ghost")
        c1, c2, c3 = (fx.call("c.ts", self.TS, n) for n in ("fromB", "fromA", "ghost"))
        self.run_idx()
        self.assertResolved(c1, fb)
        self.assertResolved(c2, fa)
        self.assertRes(c3, "unresolved")

    def test_conflicting_star_exports_stay_candidates(self):
        fx = self.fx
        for p in ("a.ts", "b.ts", "barrel.ts", "use.ts"):
            fx.file(p, language="typescript")
        fa = fx.fn("a.ts", self.TS, "dup", exported=True)
        fb = fx.fn("b.ts", self.TS, "dup", exported=True)
        fx.export("a.ts", self.TS, "dup", symbol=fa)
        fx.export("b.ts", self.TS, "dup", symbol=fb)
        fx.reexport("barrel.ts", self.TS, "./a", namespace=True)
        fx.reexport("barrel.ts", self.TS, "./b", namespace=True)
        fx.imp("use.ts", self.TS, "./barrel", "dup", "dup")
        c = fx.call("use.ts", self.TS, "dup")
        self.run_idx()
        res, target, cands = self.site_res(c)
        self.assertEqual((res, target), ("candidate", None))
        self.assertEqual(set(cands), {fa, fb})

    def test_local_export_alias_of_import_chain(self):
        fx = self.fx
        for p in ("a.ts", "mid.ts", "end.ts"):
            fx.file(p, language="typescript")
        end = fx.fn("end.ts", self.TS, "work", exported=True)
        fx.export("end.ts", self.TS, "work", symbol=end)
        fx.imp("mid.ts", self.TS, "./end", "work", "work")
        fx.export("mid.ts", self.TS, "publicWork", local="work")  # import {work}; export {work as publicWork}
        fx.imp("a.ts", self.TS, "./mid", "publicWork", "pw")
        c = fx.call("a.ts", self.TS, "pw")
        self.run_idx()
        self.assertResolved(c, end)

    def test_type_only_import_is_flagged_on_site_and_edge(self):
        fx = self.fx
        self.two_modules()
        imp = fx.imp("src/a.ts", self.TS, "./b", "foo", "foo", type_only=True)
        value = fx.imp("src/a.ts", self.TS, "./b", "foo", "foo2")
        self.run_idx()
        self.assertTrue(self.sites_by_id[imp]["typeOnly"])
        edges = self.edge(self.fid("src/a.ts"), self.fid("src/b.ts"), "imports")
        self.assertEqual(sorted((e.get("typeOnly", False), e["evidenceIds"]) for e in edges), sorted([(True, [imp]), (False, [value])]))

    def test_shadowing_and_rebinding_conflicts_are_not_promoted(self):
        fx = self.fx
        foo = self.two_modules()
        fx.imp("src/a.ts", self.TS, "./b", "foo", "foo")
        mod = fx.module("src/a.ts", self.TS)
        # parameter shadows the import inside f
        f = fx.fn("src/a.ts", self.TS, "f")
        fx.decl(f, "foo", "parameter")
        shadowed = fx.call("src/a.ts", self.TS, "foo", caller=f)
        # nested function declaration shadows the import: resolves to the inner one
        g = fx.fn("src/a.ts", self.TS, "g")
        inner = fx.fn("src/a.ts", self.TS, "foo", owner=g)
        inner_call = fx.call("src/a.ts", self.TS, "foo", caller=g)
        # module-level reassignment of the imported name makes the binding a conflict
        h = fx.fn("src/a.ts", self.TS, "h")
        fx.decl(mod, "foo", "assignment")
        rebound = fx.call("src/a.ts", self.TS, "foo", caller=h)
        self.run_idx()
        self.assertRes(shadowed, "dynamic")
        self.assertResolved(inner_call, inner)
        res, target, cands = self.site_res(rebound)
        self.assertEqual((res, target), ("candidate", None))
        self.assertEqual(cands, [foo])

    def test_block_scope_ranges_choose_narrowest_declaration(self):
        fx = self.fx
        fx.file("a.ts", language="typescript")
        outer = fx.fn("a.ts", self.TS, "work")
        f = fx.fn("a.ts", self.TS, "f")
        fx.decl(f, "work", "variable", lo=100, hi=200)  # block-local value
        inside = fx.call("a.ts", self.TS, "work", caller=f, ) 
        outside_site = fx.call("a.ts", self.TS, "work", caller=f)
        for s in fx.sites:
            if s["id"] == inside:
                s["span"]["byteStart"] = 150
            if s["id"] == outside_site:
                s["span"]["byteStart"] = 250
        self.run_idx()
        self.assertRes(inside, "dynamic")
        self.assertResolved(outside_site, outer)

    def test_recursion_and_uncalled_functions_are_kept(self):
        fx = self.fx
        fx.file("r.ts", language="typescript")
        a = fx.fn("r.ts", self.TS, "a")
        b = fx.fn("r.ts", self.TS, "b")
        fx.fn("r.ts", self.TS, "lonely")
        self_call = fx.call("r.ts", self.TS, "a", caller=a)
        ab = fx.call("r.ts", self.TS, "b", caller=a)
        ba = fx.call("r.ts", self.TS, "a", caller=b)
        self.run_idx()
        self.assertResolved(self_call, a)
        self.assertResolved(ab, b)
        self.assertResolved(ba, a)
        self.assertEqual(len(self.edge(a, a, "calls")), 1)  # self edge preserved
        self.assertEqual(len(self.idx["sites"]), 3)
        self.assertEqual(len(self.idx["symbols"]), 4)  # module + a + b + lonely

    def test_member_calls_this_super_static_and_dynamic_receivers(self):
        fx = self.fx
        fx.file("c.ts", language="typescript")
        base = fx.sym("c.ts", self.TS, "Base", "class")
        fx.decl(fx.module("c.ts", self.TS), "Base", "class", symbol=base)
        base_m = fx.sym("c.ts", self.TS, "greet", "method", base)
        child = fx.sym("c.ts", self.TS, "Child", "class", extends=["Base"])
        fx.decl(fx.module("c.ts", self.TS), "Child", "class", symbol=child)
        child_m = fx.sym("c.ts", self.TS, "run", "method", child)
        stat = fx.sym("c.ts", self.TS, "make", "method", child, static=True)
        this_call = fx.call("c.ts", self.TS, "greet", receiver="this", caller=child_m)
        super_call = fx.call("c.ts", self.TS, "greet", receiver="super", caller=child_m)
        static_call = fx.call("c.ts", self.TS, "make", receiver="Child")
        inst_call = fx.call("c.ts", self.TS, "run", receiver="Child")  # non-static method via class name
        local = fx.fn("c.ts", self.TS, "user")
        fx.decl(local, "obj", "variable")
        dyn_call = fx.call("c.ts", self.TS, "run", receiver="obj", caller=local)
        global_call = fx.call("c.ts", self.TS, "log", receiver="console")
        unknown = fx.call("c.ts", self.TS, "x", receiver="mystery")
        expr = fx.call("c.ts", self.TS, "x", receiver="a().b", receiverKind="expression")
        self.run_idx()
        res, target, cands = self.site_res(this_call)
        self.assertEqual((res, target, cands), ("candidate", None, [base_m]))  # virtual dispatch
        self.assertResolved(super_call, base_m)
        self.assertResolved(static_call, stat)
        self.assertRes(inst_call, "candidate")
        self.assertRes(dyn_call, "dynamic")
        self.assertRes(global_call, "external")
        self.assertRes(unknown, "unresolved")
        self.assertRes(expr, "dynamic")

    def test_overloads_prefer_single_implementation_else_candidates(self):
        fx = self.fx
        fx.file("o.ts", language="typescript")
        sig1 = fx.sym("o.ts", self.TS, "parse", "signature")
        sig2 = fx.sym("o.ts", self.TS, "parse", "signature")
        impl = fx.sym("o.ts", self.TS, "parse", "function")
        m = fx.module("o.ts", self.TS)
        for s in (sig1, sig2, impl):
            fx.decl(m, "parse", "function", symbol=s)
        dup1 = fx.sym("o.ts", self.TS, "twice", "function")
        dup2 = fx.sym("o.ts", self.TS, "twice", "function")
        fx.decl(m, "twice", "function", symbol=dup1)
        fx.decl(m, "twice", "function", symbol=dup2)
        c1 = fx.call("o.ts", self.TS, "parse")
        c2 = fx.call("o.ts", self.TS, "twice")
        self.run_idx()
        self.assertResolved(c1, impl)
        res, target, cands = self.site_res(c2)
        self.assertEqual((res, target, set(cands)), ("candidate", None, {dup1, dup2}))

    def test_class_construction_targets_constructor_or_class(self):
        fx = self.fx
        fx.file("k.ts", language="typescript")
        m = fx.module("k.ts", self.TS)
        plain = fx.sym("k.ts", self.TS, "Plain", "class")
        with_ctor = fx.sym("k.ts", self.TS, "WithCtor", "class")
        ctor = fx.sym("k.ts", self.TS, "constructor", "constructor", with_ctor)
        for n, s in (("Plain", plain), ("WithCtor", with_ctor)):
            fx.decl(m, n, "class", symbol=s)
        a = fx.call("k.ts", self.TS, "Plain", kind="new")
        b = fx.call("k.ts", self.TS, "WithCtor", kind="new")
        self.run_idx()
        self.assertResolved(a, plain)
        self.assertResolved(b, ctor)

    def test_dynamic_and_non_literal_specifiers(self):
        fx = self.fx
        fx.file("a.ts", language="typescript")
        dyn = fx.imp("a.ts", self.TS, None, None, None, kind="dynamicImport", dynamic=True)
        call = fx.call("a.ts", self.TS, "x", kind="dynamic")
        parser_dynamic = fx.site("a.ts", self.TS, "calls", "f[x]()", {"kind": "dynamic", "name": "x"}, resolution="dynamic")
        self.run_idx()
        self.assertRes(dyn, "dynamic")
        self.assertRes(call, "dynamic")
        self.assertRes(parser_dynamic, "dynamic")

    def test_iife_and_unsupported_parser_resolutions_are_preserved(self):
        fx = self.fx
        fx.file("a.ts", language="typescript")
        lam = fx.sym("a.ts", self.TS, "", "lambda")
        iife = fx.site("a.ts", self.TS, "calls", "(()=>1)()", {"kind": "call", "iife": True}, resolution="resolvedStatic")
        for s in fx.sites:
            if s["id"] == iife:
                s["targetId"] = lam
        macro = fx.site("a.ts", self.TS, "macros", "m!()", {"kind": "macro", "name": "m"}, resolution="unsupported")
        self.run_idx()
        self.assertResolved(iife, lam)
        self.assertRes(macro, "unsupported")
        self.run_idx_again = resolve.resolve(self.idx, self.root)
        self.assertEqual(self.idx["sites"][0]["targetId"], lam)

    def test_jsx_components_and_intrinsics(self):
        fx = self.fx
        fx.file("app.tsx", language="tsx")
        fx.file("btn.tsx", language="tsx")
        btn = fx.fn("btn.tsx", "tsx", "Button", exported=True)
        fx.export("btn.tsx", "tsx", "Button", symbol=btn)
        fx.imp("app.tsx", "tsx", "./btn", "Button", "Button")
        comp = fx.call("app.tsx", "tsx", "Button", kind="jsx", relation="renders")
        intrinsic = fx.call("app.tsx", "tsx", "div", kind="jsx", relation="renders")
        self.run_idx()
        self.assertResolved(comp, btn)
        self.assertRes(intrinsic, "external")
        self.assertEqual(self.sites_by_id[comp]["relation"], "renders")

    def test_svelte_component_import_resolves_to_file_and_named_exports(self):
        fx = self.fx
        fx.file("src/App.svelte", language="svelte")
        fx.file("src/Widget.svelte", language="svelte")
        fx.file("src/util.ts", language="typescript")
        helper = fx.fn("src/util.ts", self.TS, "help", exported=True)
        fx.export("src/util.ts", self.TS, "help", symbol=helper)
        # script region symbols carry the grammar language; the file profile is svelte
        w = fx.site("src/App.svelte", self.TS, "imports", "./Widget.svelte", {"kind": "import", "specifier": "./Widget.svelte", "imported": None, "local": "Widget", "default": True}, region="instance")
        fx.decl(fx.module("src/App.svelte", self.TS, region="instance"), "Widget", "import", site=w)
        h = fx.site("src/App.svelte", self.TS, "imports", "../src/util", {"kind": "import", "specifier": "../src/util", "imported": "help", "local": "help"}, region="instance")
        fx.decl(fx.module("src/App.svelte", self.TS, region="instance"), "help", "import", site=h)
        render = fx.site("src/App.svelte", self.TS, "renders", "<Widget/>", {"kind": "jsx", "name": "Widget", "receiver": None}, region="instance")
        self.run_idx()
        self.assertResolved(w, self.fid("src/Widget.svelte"))
        self.assertResolved(h, self.fid("src/util.ts"))
        self.assertResolved(render, self.fid("src/Widget.svelte"))

    def test_builtin_modules_and_declared_dependencies(self):
        fx = self.fx
        fx.file("package.json", json.dumps({"dependencies": {"left-pad": "1", "@scope/x": "1"}, "devDependencies": {"vitest": "1"}}))
        fx.file("a.ts", language="typescript")
        sites = {
            "fs": fx.imp("a.ts", self.TS, "node:fs", "x", "x1"),
            "fs2": fx.imp("a.ts", self.TS, "fs/promises", "x", "x2"),
            "dep": fx.imp("a.ts", self.TS, "left-pad", None, "lp", default=True),
            "scoped": fx.imp("a.ts", self.TS, "@scope/x/deep", "x", "x3"),
            "unknown": fx.imp("a.ts", self.TS, "mystery-pkg", None, "m", default=True),
            "url": fx.imp("a.ts", self.TS, "https://cdn.example/x.js", None, "u", default=True),
        }
        self.run_idx()
        for k in ("fs", "fs2", "dep", "scoped", "url"):
            self.assertRes(sites[k], "external")
        self.assertRes(sites["unknown"], "unresolved")

    def test_hostile_specifiers_never_escape_or_touch_the_filesystem(self):
        fx = self.fx
        fx.file("src/a.ts", language="typescript")
        outside = os.path.join(os.path.dirname(self.root), "secret.ts")
        specs = ["../../secret", "../../../etc/passwd", "/etc/passwd", "C:\\Windows\\win.ini", "..\\..\\x", "./a\x00.ts", "C:/x", "./x:y", "file:///etc/passwd"]
        ids = [fx.imp("src/a.ts", self.TS, s, "x", f"x{i}") for i, s in enumerate(specs)]
        self.run_idx()
        for i, s in zip(ids, specs):
            self.assertIn(self.site_res(i)[0], ("unresolved", "external"), s)
            self.assertIsNone(self.sites_by_id[i]["targetId"])
        self.assertFalse(os.path.exists(outside))

    def test_tsconfig_paths_baseurl_jsonc_extends_and_references(self):
        fx = self.fx
        fx.file("tsconfig.base.json", '{\n // comment\n "compilerOptions": { "baseUrl": ".", "paths": { "@lib/*": ["packages/lib/src/*"], "@exact": ["packages/lib/src/main.ts",], }, },\n}')
        fx.file("app/tsconfig.json", '/* app */ {"extends": "../tsconfig.base.json", "include": ["src/**/*"], "compilerOptions": {"strict": true},}')
        fx.file("app/src/a.ts", language="typescript")
        fx.file("packages/lib/src/util.ts", language="typescript")
        fx.file("packages/lib/src/main.ts", language="typescript")
        fx.file("app/src/local.ts", language="typescript")
        fx.file("other/x.ts", language="typescript")
        s1 = fx.imp("app/src/a.ts", self.TS, "@lib/util", "x", "x")
        s2 = fx.imp("app/src/a.ts", self.TS, "@exact", "x", "y")
        s3 = fx.imp("app/src/a.ts", self.TS, "app/src/local", "x", "z")  # baseUrl "." => repo-root relative
        s4 = fx.imp("app/src/a.ts", self.TS, "@lib/missing", "x", "w")
        outside = fx.imp("other/x.ts", self.TS, "@lib/util", "x", "x")  # not covered by app/tsconfig include
        self.run_idx()
        self.assertResolved(s1, self.fid("packages/lib/src/util.ts"))
        self.assertResolved(s2, self.fid("packages/lib/src/main.ts"))
        self.assertResolved(s3, self.fid("app/src/local.ts"))
        self.assertRes(s4, "unresolved")
        self.assertEqual(self.site_res(outside)[0], "unresolved")

    def test_tsconfig_paths_without_baseurl_are_relative_to_config(self):
        fx = self.fx
        fx.file("web/tsconfig.json", '{"compilerOptions": {"paths": {"~/*": ["./src/*"]}}}')
        fx.file("web/src/a.ts", language="typescript")
        fx.file("web/src/ui/button.ts", language="typescript")
        s = fx.imp("web/src/a.ts", self.TS, "~/ui/button", "x", "x")
        self.run_idx()
        self.assertResolved(s, self.fid("web/src/ui/button.ts"))

    def test_tsconfig_solution_references_select_the_covering_project(self):
        fx = self.fx
        fx.file("tsconfig.json", '{"files": [], "references": [{"path": "./app"}, {"path": "./lib"}]}')
        fx.file("app/tsconfig.json", '{"include": ["src"], "compilerOptions": {"paths": {"@l/*": ["../lib/src/*"]}}}')
        fx.file("lib/tsconfig.json", '{"include": ["src"]}')
        fx.file("app/src/a.ts", language="typescript")
        fx.file("lib/src/b.ts", language="typescript")
        s = fx.imp("app/src/a.ts", self.TS, "@l/b", "x", "x")
        self.run_idx()
        self.assertResolved(s, self.fid("lib/src/b.ts"))

    def test_workspace_package_exports_conditions_and_subpaths(self):
        fx = self.fx
        fx.file("package.json", json.dumps({"workspaces": ["packages/*"], "name": "root"}))
        fx.file("packages/core/package.json", json.dumps({
            "name": "@acme/core",
            "exports": {
                ".": {"types": "./src/index.d.ts", "import": "./src/index.mjs", "require": "./src/index.cjs"},
                "./util": "./src/util.ts",
                "./features/*": "./src/features/*.ts",
                "./private": None,
            },
        }))
        fx.file("packages/single/package.json", json.dumps({"name": "single", "main": "lib/main.ts"}))
        fx.file("packages/core/src/index.mjs")
        fx.file("packages/core/src/index.cjs")
        fx.file("packages/core/src/util.ts", language="typescript")
        fx.file("packages/core/src/features/login.ts", language="typescript")
        fx.file("packages/single/lib/main.ts", language="typescript")
        fx.file("packages/app/package.json", json.dumps({"name": "app", "dependencies": {"@acme/core": "workspace:*"}}))
        fx.file("packages/app/a.ts", language="typescript")
        s_root = fx.imp("packages/app/a.ts", self.TS, "@acme/core", "x", "x")
        s_util = fx.imp("packages/app/a.ts", self.TS, "@acme/core/util", "x", "y")
        s_feat = fx.imp("packages/app/a.ts", self.TS, "@acme/core/features/login", "x", "z")
        s_hidden = fx.imp("packages/app/a.ts", self.TS, "@acme/core/internal", "x", "w")
        s_single = fx.imp("packages/app/a.ts", self.TS, "single", None, "s", default=True)
        self.run_idx()
        res, target, cands = self.site_res(s_root)
        self.assertEqual((res, target), ("candidate", None))  # conditions stay candidates
        self.assertEqual(set(cands), {self.fid("packages/core/src/index.mjs"), self.fid("packages/core/src/index.cjs")})
        self.assertResolved(s_util, self.fid("packages/core/src/util.ts"))
        self.assertResolved(s_feat, self.fid("packages/core/src/features/login.ts"))
        self.assertRes(s_hidden, "unresolved")
        self.assertResolved(s_single, self.fid("packages/single/lib/main.ts"))

    def test_package_imports_map(self):
        fx = self.fx
        fx.file("package.json", json.dumps({"imports": {"#internal/*": "./src/internal/*.ts"}}))
        fx.file("src/a.ts", language="typescript")
        fx.file("src/internal/x.ts", language="typescript")
        s = fx.imp("src/a.ts", self.TS, "#internal/x", "x", "x")
        self.run_idx()
        self.assertResolved(s, self.fid("src/internal/x.ts"))

    def test_config_files_changed_after_inventory_or_linked_are_ignored(self):
        fx = self.fx
        good = json.dumps({"compilerOptions": {"paths": {"@a/*": ["./src/*"]}}})
        fx.raw_file("tsconfig.json", hashlib.sha256(b"something else").hexdigest(), good.encode())
        fx.file("src/a.ts", language="typescript")
        fx.file("src/b.ts", language="typescript")
        s = fx.imp("src/a.ts", self.TS, "@a/b", "x", "x")
        self.run_idx()
        self.assertRes(s, "unresolved")
        self.assertTrue(any("inventory hash" in m for m in self.idx["limitations"]))

    def test_symlinked_config_is_refused(self):
        target = os.path.join(self.root, "real.json")
        Path(target).write_text(json.dumps({"compilerOptions": {"paths": {"@a/*": ["./src/*"]}}}), encoding="utf-8")
        link = os.path.join(self.root, "tsconfig.json")
        try:
            os.symlink(target, link)
        except (OSError, NotImplementedError):
            self.skipTest("symlinks unavailable in this environment")
        fx = self.fx
        data = Path(target).read_bytes()
        fx.files.append({"id": sid("file", "tsconfig.json"), "path": "tsconfig.json", "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data), "language": None, "status": "unsupported", "diagnostics": []})
        fx.file("src/a.ts", language="typescript")
        fx.file("src/b.ts", language="typescript")
        s = fx.imp("src/a.ts", self.TS, "@a/b", "x", "x")
        self.run_idx()
        self.assertRes(s, "unresolved")
        self.assertTrue(any("link" in m for m in self.idx["limitations"]))


# ===========================================================================
# Python
# ===========================================================================
class PythonResolution(ResolveCase):
    PY = "python"

    def test_relative_package_and_reexport_imports(self):
        fx = self.fx
        for p in ("pkg/__init__.py", "pkg/impl.py", "pkg/sub/__init__.py", "pkg/sub/leaf.py", "app.py"):
            fx.file(p, language="python")
        work = fx.fn("pkg/impl.py", self.PY, "work")
        leaf = fx.fn("pkg/sub/leaf.py", self.PY, "leaf_fn")
        # pkg/__init__ re-exports impl.work
        fx.imp("pkg/__init__.py", self.PY, ".impl", "work", "work")
        # pkg/sub/leaf imports from the parent package with explicit relative level 2
        up = fx.imp("pkg/sub/leaf.py", self.PY, "..impl", "work", "w2")
        w2call = fx.call("pkg/sub/leaf.py", self.PY, "w2")
        fx.imp("app.py", self.PY, "pkg", "work", "work")
        fx.imp("app.py", self.PY, "pkg.sub.leaf", "leaf_fn", "lf")
        c1 = fx.call("app.py", self.PY, "work")
        c2 = fx.call("app.py", self.PY, "lf")
        self.run_idx()
        self.assertResolved(up, self.fid("pkg/impl.py"))
        self.assertResolved(w2call, work)
        self.assertResolved(c1, work)  # through the __init__ re-export
        self.assertResolved(c2, leaf)

    def test_module_alias_and_dotted_module_calls(self):
        fx = self.fx
        for p in ("lib/__init__.py", "lib/tools.py", "lib/deep/__init__.py", "lib/deep/mod.py", "main.py"):
            fx.file(p, language="python")
        tool = fx.fn("lib/tools.py", self.PY, "tool")
        deep = fx.fn("lib/deep/mod.py", self.PY, "deepfn")
        fx.imp("main.py", self.PY, "lib", "tools", "tools")  # from lib import tools (submodule)
        fx.imp("main.py", self.PY, "lib.deep.mod", None, "lib", alias=None)  # import lib.deep.mod -> binds `lib`
        c1 = fx.call("main.py", self.PY, "tool", receiver="tools")
        c2 = fx.call("main.py", self.PY, "deepfn", receiver="lib.deep.mod")
        self.run_idx()
        self.assertResolved(c1, tool)
        self.assertResolved(c2, deep)

    def test_stdlib_builtins_declared_deps_and_missing(self):
        fx = self.fx
        fx.file("requirements.txt", "requests==2\nNumPy>=1\n")
        fx.file("a.py", language="python")
        sites = {
            "os": fx.imp("a.py", self.PY, "os", None, "os"),
            "req": fx.imp("a.py", self.PY, "requests", None, "requests"),
            "np": fx.imp("a.py", self.PY, "numpy", None, "np"),
            "gone": fx.imp("a.py", self.PY, "mystery", None, "mystery"),
            "rel": fx.imp("a.py", self.PY, ".nothing", "x", "x"),
        }
        pb = fx.call("a.py", self.PY, "len")
        pj = fx.call("a.py", self.PY, "join", receiver="os")
        self.run_idx()
        for k in ("os", "req", "np"):
            self.assertRes(sites[k], "external")
        self.assertRes(sites["gone"], "unresolved")
        self.assertRes(sites["rel"], "unresolved")
        self.assertRes(pb, "external")
        self.assertRes(pj, "external")

    def test_star_import_self_methods_and_class_scope_rules(self):
        fx = self.fx
        for p in ("lib.py", "main.py"):
            fx.file(p, language="python")
        helper = fx.fn("lib.py", self.PY, "helper")
        fx.imp("main.py", self.PY, "lib", None, None, namespace=True)
        call = fx.call("main.py", self.PY, "helper")
        mod = fx.module("main.py", self.PY)
        cls = fx.sym("main.py", self.PY, "K", "class")
        fx.decl(mod, "K", "class", symbol=cls)
        m1 = fx.sym("main.py", self.PY, "one", "method", cls)
        m2 = fx.sym("main.py", self.PY, "two", "method", cls)
        fx.decl(cls, "one", "function", symbol=m1)
        fx.decl(cls, "two", "function", symbol=m2)
        selfcall = fx.call("main.py", self.PY, "two", receiver="self", caller=m1)
        # a bare `two()` inside a method does not see the class-scope name
        bare = fx.call("main.py", self.PY, "two", caller=m1)
        self.run_idx()
        self.assertResolved(call, helper)
        res, target, cands = self.site_res(selfcall)
        self.assertEqual((res, cands), ("candidate", [m2]))
        self.assertRes(bare, "unresolved")

    def test_try_except_import_fallback_and_redefinition_are_conflicts(self):
        fx = self.fx
        for p in ("a.py", "fast.py"):
            fx.file(p, language="python")
        fast = fx.fn("fast.py", self.PY, "speed")
        fx.imp("a.py", self.PY, "fast", "speed", "speed")
        fallback = fx.fn("a.py", self.PY, "speed")
        call = fx.call("a.py", self.PY, "speed")
        self.run_idx()
        res, target, cands = self.site_res(call)
        self.assertEqual((res, target, set(cands)), ("candidate", None, {fast, fallback}))

    def test_absolute_import_in_two_roots_is_candidates_and_namespace_packages(self):
        fx = self.fx
        for p in ("a/util.py", "b/util.py", "a/main.py", "ns/pkg/mod.py", "ns/user.py"):
            fx.file(p, language="python")
        fx.fn("a/util.py", self.PY, "x")
        fx.fn("b/util.py", self.PY, "x")
        fx.fn("ns/pkg/mod.py", self.PY, "y")
        ambiguous = fx.imp("a/main.py", self.PY, "util", "x", "x")  # a/util.py wins by importer root only if unique
        ns = fx.imp("ns/user.py", self.PY, "pkg.mod", "y", "y")
        self.run_idx()
        self.assertResolved(ambiguous, self.fid("a/util.py"))
        self.assertResolved(ns, self.fid("ns/pkg/mod.py"))


# ===========================================================================
# Rust
# ===========================================================================
class RustResolution(ResolveCase):
    RS = "rust"

    def crate(self):
        fx = self.fx
        fx.file("Cargo.toml", '[package]\nname = "my-crate"\nversion = "0.1.0"\n[dependencies]\nserde = "1"\nhelper-lib = { path = "../helper" }\n')
        fx.file("src/lib.rs", language="rust")
        fx.file("src/a.rs", language="rust")
        fx.file("src/b/mod.rs", language="rust")
        fx.file("src/b/c.rs", language="rust")
        fx.file("src/missing_decl.rs", language="rust")

    def mod_site(self, path, name, caller=None):
        return fx_site(self.fx, path, "imports", name, {"kind": "mod", "specifier": name, "imported": None, "alias": None}, caller)

    def use_site(self, path, spec, alias=None, glob=False, reexport=False, caller=None, rel="imports"):
        return fx_site(self.fx, path, "reexports" if reexport else rel, spec, {"kind": "use", "specifier": spec, "alias": alias, "namespace": glob, "reexport": reexport}, caller)

    def test_mod_use_crate_super_self_glob_and_reexports(self):
        fx = self.fx
        self.crate()
        lib = fx.module("src/lib.rs", self.RS)
        a = fx.module("src/a.rs", self.RS)
        b = fx.module("src/b/mod.rs", self.RS)
        c = fx.module("src/b/c.rs", self.RS)
        fa = fx.sym("src/a.rs", self.RS, "alpha", "function", a)
        fc = fx.sym("src/b/c.rs", self.RS, "gamma", "function", c)
        fl = fx.sym("src/lib.rs", self.RS, "top", "function", lib)
        m_a = self.mod_site("src/lib.rs", "a")
        m_b = self.mod_site("src/lib.rs", "b")
        m_c = self.mod_site("src/b/mod.rs", "c")
        m_gone = self.mod_site("src/lib.rs", "nonexistent")
        # lib.rs: `use crate::a::alpha;`  `use b::c::*;`  `pub use a::alpha as renamed;`
        u1 = self.use_site("src/lib.rs", "crate::a::alpha")
        u2 = self.use_site("src/lib.rs", "b::c", glob=True)
        u3 = self.use_site("src/lib.rs", "a::alpha", alias="renamed", reexport=True)
        # b/c.rs: `use super::super::top;`  `use crate::a;`
        u4 = self.use_site("src/b/c.rs", "super::super::top")
        u5 = self.use_site("src/b/c.rs", "crate::a")
        # calls
        call_alpha = fx.call("src/lib.rs", self.RS, "alpha", caller=fl)
        call_gamma = fx.call("src/lib.rs", self.RS, "gamma", caller=fl)
        call_renamed = fx.call("src/lib.rs", self.RS, "renamed", caller=fl)
        call_path = fx.call("src/b/c.rs", self.RS, "alpha", receiver="crate::a", caller=fc, kind="path")
        call_top = fx.call("src/b/c.rs", self.RS, "top", caller=fc)
        call_mod = fx.call("src/b/c.rs", self.RS, "alpha", receiver="a", caller=fc, kind="path")  # `use crate::a;` binds `a`
        call_unbound = fx.call("src/b/c.rs", self.RS, "alpha", receiver="never_imported", caller=fc, kind="path")
        self.run_idx()
        self.assertResolved(m_a, self.fid("src/a.rs"))
        self.assertResolved(m_b, self.fid("src/b/mod.rs"))
        self.assertResolved(m_c, self.fid("src/b/c.rs"))
        self.assertRes(m_gone, "unresolved")
        self.assertResolved(u1, self.fid("src/a.rs"))
        self.assertResolved(u4, self.fid("src/lib.rs"))
        self.assertResolved(u5, self.fid("src/a.rs"))
        self.assertResolved(call_alpha, fa)
        self.assertResolved(call_gamma, fc)  # via glob import
        self.assertResolved(call_renamed, fa)  # via `pub use ... as renamed`
        self.assertResolved(call_path, fa)
        self.assertResolved(call_top, fl)  # `use super::super::top` binds top
        self.assertResolved(call_mod, fa)
        self.assertRes(call_unbound, "unresolved")

    def test_external_crates_workspace_deps_and_unknown_paths(self):
        fx = self.fx
        self.crate()
        lib = fx.module("src/lib.rs", self.RS)
        u_std = self.use_site("src/lib.rs", "std::collections::HashMap")
        u_serde = self.use_site("src/lib.rs", "serde::Serialize")
        u_unknown = self.use_site("src/lib.rs", "unknown_crate::thing")
        vec_new = fx.call("src/lib.rs", self.RS, "new", receiver="Vec", kind="path")
        self.run_idx()
        self.assertRes(u_std, "external")
        self.assertRes(u_serde, "external")
        self.assertRes(u_unknown, "unresolved")
        self.assertRes(vec_new, "external")

    def test_assoc_functions_self_methods_and_trait_ambiguity(self):
        fx = self.fx
        self.crate()
        lib = fx.module("src/lib.rs", self.RS)
        st = fx.sym("src/lib.rs", self.RS, "Thing", "struct", lib)
        impl = fx.sym("src/lib.rs", self.RS, "", "impl", lib, implFor="Thing", implTrait=None)
        new = fx.sym("src/lib.rs", self.RS, "new", "function", impl)
        run = fx.sym("src/lib.rs", self.RS, "run", "method", impl)
        timpl = fx.sym("src/lib.rs", self.RS, "", "impl", lib, implFor="Thing", implTrait="Display")
        fmt = fx.sym("src/lib.rs", self.RS, "fmt", "method", timpl)
        timpl2 = fx.sym("src/lib.rs", self.RS, "", "impl", lib, implFor="Thing", implTrait="Debug")
        fmt2 = fx.sym("src/lib.rs", self.RS, "fmt", "method", timpl2)
        free = fx.sym("src/lib.rs", self.RS, "free", "function", lib)
        c_new = fx.call("src/lib.rs", self.RS, "new", receiver="Thing", kind="path", caller=free)
        c_self_run = fx.call("src/lib.rs", self.RS, "run", receiver="self", caller=run)
        c_self_fmt = fx.call("src/lib.rs", self.RS, "fmt", receiver="self", caller=run)
        c_selfpath = fx.call("src/lib.rs", self.RS, "new", receiver="Self", kind="path", caller=run)
        c_var = fx.call("src/lib.rs", self.RS, "run", receiver="t", caller=free)
        fx.decl(free, "t", "variable")
        self.run_idx()
        self.assertResolved(c_new, new)
        self.assertResolved(c_self_run, run)
        res, target, cands = self.site_res(c_self_fmt)
        self.assertEqual((res, target, set(cands)), ("candidate", None, {fmt, fmt2}))
        self.assertResolved(c_selfpath, new)
        self.assertRes(c_var, "dynamic")

    def test_macros_and_cfg_stay_unsupported(self):
        fx = self.fx
        self.crate()
        m = fx.site("src/lib.rs", self.RS, "macros", "println!()", {"kind": "macro", "name": "println"}, resolution="unsupported")
        self.run_idx()
        self.assertRes(m, "unsupported")


def fx_site(fx: Fx, path, relation, raw, binding, caller=None):
    return fx.site(path, "rust", relation, raw, binding, caller)


# ===========================================================================
# Go
# ===========================================================================
class GoResolution(ResolveCase):
    GO = "go"

    def test_package_import_is_candidate_set_and_member_calls_resolve_within_package(self):
        fx = self.fx
        fx.file("go.mod", "module example.com/app\n\ngo 1.21\nrequire github.com/x/y v1.0.0\n")
        for p in ("main.go", "util/a.go", "util/b.go", "util/b_test.go", "util/plat_windows.go", "util/plat_linux.go"):
            fx.file(p, language="go")
        for p in ("main.go", "util/a.go", "util/b.go", "util/b_test.go", "util/plat_windows.go", "util/plat_linux.go"):
            m = fx.module(p, self.GO)
            fx.sym_by_id[m].setdefault("bindings", {})["package"] = "main" if p == "main.go" else "util"
        helper = fx.sym("util/a.go", self.GO, "Helper", "function", fx.module("util/a.go", self.GO))
        other = fx.sym("util/b.go", self.GO, "Other", "function", fx.module("util/b.go", self.GO))
        test_only = fx.sym("util/b_test.go", self.GO, "OnlyInTest", "function", fx.module("util/b_test.go", self.GO))
        plat1 = fx.sym("util/plat_windows.go", self.GO, "Plat", "function", fx.module("util/plat_windows.go", self.GO))
        plat2 = fx.sym("util/plat_linux.go", self.GO, "Plat", "function", fx.module("util/plat_linux.go", self.GO))
        main_mod = fx.module("main.go", self.GO)
        imp = fx.imp("main.go", self.GO, "example.com/app/util", None, "util", implicitLocal=True)
        std = fx.imp("main.go", self.GO, "fmt", None, "fmt", implicitLocal=True)
        dep = fx.imp("main.go", self.GO, "github.com/x/y", None, "y", implicitLocal=True)
        missing = fx.imp("main.go", self.GO, "example.com/app/nodir", None, "nodir", implicitLocal=True)
        c_helper = fx.call("main.go", self.GO, "Helper", receiver="util")
        c_test = fx.call("main.go", self.GO, "OnlyInTest", receiver="util")
        c_plat = fx.call("main.go", self.GO, "Plat", receiver="util")
        c_fmt = fx.call("main.go", self.GO, "Println", receiver="fmt")
        c_len = fx.call("main.go", self.GO, "len")
        c_method = fx.call("main.go", self.GO, "Do", receiver="svc")
        fx.decl(main_mod, "svc", "variable")
        self.run_idx()
        res, target, cands = self.site_res(imp)
        self.assertEqual((res, target), ("candidate", None))  # a package is not one file
        self.assertEqual(set(cands), {self.fid("util/a.go"), self.fid("util/b.go"), self.fid("util/plat_windows.go"), self.fid("util/plat_linux.go")})
        self.assertRes(std, "external")
        self.assertRes(dep, "external")
        self.assertRes(missing, "unresolved")
        self.assertResolved(c_helper, helper)
        self.assertRes(c_test, "unresolved")  # test files are not part of the imported package
        res, target, cands = self.site_res(c_plat)
        self.assertEqual((res, set(cands)), ("candidate", {plat1, plat2}))
        self.assertRes(c_fmt, "external")
        self.assertRes(c_len, "external")
        self.assertRes(c_method, "dynamic")

    def test_same_package_function_across_files(self):
        fx = self.fx
        fx.file("go.mod", "module m\n")
        for p in ("a.go", "b.go"):
            fx.file(p, language="go")
            fx.sym_by_id[fx.module(p, self.GO)].setdefault("bindings", {})["package"] = "m"
        callee = fx.sym("b.go", self.GO, "helper", "function", fx.module("b.go", self.GO))
        caller = fx.sym("a.go", self.GO, "run", "function", fx.module("a.go", self.GO))
        call = fx.call("a.go", self.GO, "helper", caller=caller)
        self.run_idx()
        self.assertResolved(call, callee)


# ===========================================================================
# C / C++
# ===========================================================================
class CResolution(ResolveCase):
    def test_literal_includes_only_and_closure_calls(self):
        fx = self.fx
        for p in ("src/main.c", "src/util.h", "src/util.c", "include/api/api.h", "src/Case.h"):
            fx.file(p, language="c")
        quoted = fx.imp("src/main.c", "c", "util.h", kind="include", relation="includes", system=False)
        angled_local = fx.imp("src/main.c", "c", "api/api.h", kind="include", relation="includes", system=True)
        sys_inc = fx.imp("src/main.c", "c", "stdio.h", kind="include", relation="includes", system=True)
        gone = fx.imp("src/main.c", "c", "gone.h", kind="include", relation="includes", system=False)
        casey = fx.imp("src/main.c", "c", "case.h", kind="include", relation="includes", system=False)
        escape = fx.imp("src/main.c", "c", "../../x.h", kind="include", relation="includes", system=False)
        proto = fx.sym("src/util.h", "c", "helper", "signature", fx.module("src/util.h", "c"))
        defn = fx.sym("src/util.c", "c", "helper", "function", fx.module("src/util.c", "c"))
        local = fx.sym("src/main.c", "c", "local", "function", fx.module("src/main.c", "c"))
        fx.decl(fx.module("src/main.c", "c"), "local", "function", symbol=local)
        main_fn = fx.sym("src/main.c", "c", "main", "function", fx.module("src/main.c", "c"))
        c_local = fx.call("src/main.c", "c", "local", caller=main_fn)
        c_helper = fx.call("src/main.c", "c", "helper", caller=main_fn)
        c_nowhere = fx.call("src/main.c", "c", "nowhere", caller=main_fn)
        self.run_idx()
        self.assertResolved(quoted, self.fid("src/util.h"))
        res, target, cands = self.site_res(angled_local)
        self.assertEqual((res, cands), ("candidate", [self.fid("include/api/api.h")]))  # include path unknown
        self.assertRes(sys_inc, "external")
        self.assertRes(gone, "unresolved")
        self.assertRes(casey, "unresolved")
        self.assertRes(escape, "unresolved")
        self.assertResolved(c_local, local)
        res, target, cands = self.site_res(c_helper)
        self.assertEqual((res, target), ("candidate", None))  # prototypes in an included header; definition elsewhere
        self.assertEqual(cands, [proto])
        self.assertRes(c_nowhere, "unresolved")


# ===========================================================================
# Other languages (conservative)
# ===========================================================================
class OtherLanguages(ResolveCase):
    def test_ruby_lua_bash_zig_literal_paths(self):
        fx = self.fx
        for p, l in (("lib/a.rb", "ruby"), ("lib/b.rb", "ruby"), ("scripts/run.sh", "bash"), ("scripts/lib.sh", "bash"), ("game/main.lua", "lua"), ("game/util.lua", "lua"), ("zig/a.zig", "zig"), ("zig/b.zig", "zig")):
            fx.file(p, language=l)
        rr = fx.imp("lib/a.rb", "ruby", "b", kind="require", relativeTo="dir")
        req = fx.imp("lib/a.rb", "ruby", "json", kind="require")
        src = fx.imp("scripts/run.sh", "bash", "./lib.sh", kind="source", relation="includes")
        lua = fx.imp("game/main.lua", "lua", "game.util", kind="require")
        zig = fx.imp("zig/a.zig", "zig", "b.zig", kind="import")
        zstd = fx.imp("zig/a.zig", "zig", "std", kind="import")
        dyn = fx.imp("lib/a.rb", "ruby", None, kind="require")
        esc = fx.imp("lib/a.rb", "ruby", "../../../x", kind="require", relativeTo="dir")
        self.run_idx()
        self.assertResolved(rr, self.fid("lib/b.rb"))
        self.assertRes(req, "external")
        self.assertRes(src, "candidate")  # shell cwd is not known statically
        self.assertRes(lua, "candidate")  # package.path is not known statically
        self.assertResolved(zig, self.fid("zig/b.zig"))
        self.assertRes(zstd, "external")
        self.assertRes(dyn, "dynamic")
        self.assertRes(esc, "unresolved")

    def test_java_fqn_imports_static_members_and_wildcards(self):
        fx = self.fx
        fx.file("a/Main.java", language="java")
        fx.file("b/Util.java", language="java")
        fx.file("b/Other.java", language="java")
        util = fx.sym("b/Util.java", "java", "Util", "class", fx.module("b/Util.java", "java"), fqn="com.acme.b.Util")
        helper = fx.sym("b/Util.java", "java", "help", "method", util, static=True, fqn=None)
        fx.sym("b/Other.java", "java", "Other", "class", fx.module("b/Other.java", "java"), fqn="com.acme.b.Other")
        imp = fx.imp("a/Main.java", "java", "com.acme.b.Util", None, "Util")
        wild = fx.imp("a/Main.java", "java", "com.acme.b", None, None, namespace=True)
        ext = fx.imp("a/Main.java", "java", "java.util.List", None, "List")
        stat = fx.imp("a/Main.java", "java", "com.acme.b.Util.help", None, "help", static=True)
        c = fx.call("a/Main.java", "java", "help", receiver="Util")
        self.run_idx()
        self.assertResolved(imp, self.fid("b/Util.java"))
        res, target, cands = self.site_res(wild)
        self.assertEqual((res, set(cands)), ("candidate", {self.fid("b/Util.java"), self.fid("b/Other.java")}))
        self.assertRes(ext, "external")
        self.assertResolved(c, helper)

    def test_java_unresolved_when_some_sources_are_not_parsed(self):
        fx = self.fx
        fx.file("a/Main.java", language="java")
        fx.file("b/Broken.java", language="java", status="failed")
        imp = fx.imp("a/Main.java", "java", "com.acme.Maybe", None, "Maybe")
        self.run_idx()
        self.assertRes(imp, "unresolved")

    def test_unsupported_language_imports_are_explicit(self):
        fx = self.fx
        fx.file("a.swift", language="swift")
        imp = fx.imp("a.swift", "swift", "Foundation", None, None)
        self.run_idx()
        self.assertRes(imp, "unsupported")


# ===========================================================================
# Cross-cutting
# ===========================================================================
class Invariants(ResolveCase):
    def build(self):
        fx = self.fx
        for p in ("a.ts", "b.ts", "c.ts"):
            fx.file(p, language="typescript")
        fx.file("cfg.yaml", language=None, status="unsupported")
        foo = fx.fn("b.ts", "typescript", "foo", exported=True)
        fx.export("b.ts", "typescript", "foo", symbol=foo)
        fx.imp("a.ts", "typescript", "./b", "foo", "foo")
        fx.call("a.ts", "typescript", "foo")
        fx.call("a.ts", "typescript", "foo")
        fx.call("a.ts", "typescript", "missing")
        fx.call("a.ts", "typescript", "x", kind="dynamic")
        fx.imp("a.ts", "typescript", "react", None, "R", default=True)
        return foo

    def test_every_site_is_retained_and_counts_reconcile(self):
        foo = self.build()
        n = len(self.fx.sites)
        idx = self.run_idx()
        self.assertEqual(len(idx["sites"]), n)
        cov = idx["coverage"]
        self.assertEqual(cov["encounteredSites"], n)
        parts = cov["resolutionCounts"]
        self.assertEqual(sum(parts.values()), n)
        self.assertEqual(cov["resolvedSites"], parts["resolvedStatic"])
        self.assertEqual(cov["dynamicSites"], parts["dynamic"])
        self.assertEqual(cov["unresolvedSites"], parts["unresolved"] + parts["candidate"] + parts["unsupported"])
        self.assertEqual(cov["admittedFiles"], len(self.fx.files))  # untouched foreign coverage keys
        # parallel evidence aggregates into one edge
        edges = self.edge(self.fx.module("a.ts", "typescript"), foo, "calls")
        self.assertEqual(len(edges), 1)
        self.assertEqual(len(edges[0]["evidenceIds"]), 2)

    def test_resolution_is_idempotent(self):
        self.build()
        first = json.dumps(self.run_idx(), sort_keys=True)
        second = json.dumps(resolve.resolve(json.loads(first), self.root), sort_keys=True)
        self.assertEqual(first, second)

    def test_edges_reference_existing_nodes_and_candidate_edges_are_labelled(self):
        fx = self.fx
        for p in ("a.ts", "m.ts", "m.js"):
            fx.file(p, language="typescript")
        fx.imp("a.ts", "typescript", "./m", "x", "x")
        self.run_idx()
        ids = {f["id"] for f in self.idx["files"]} | {s["id"] for s in self.idx["symbols"]}
        self.assertEqual(len(self.idx["edges"]), 2)
        for e in self.idx["edges"]:
            self.assertIn(e["sourceId"], ids)
            self.assertIn(e["targetId"], ids)
            self.assertEqual(e["resolution"], "candidate")
            self.assertTrue(e["evidenceIds"])

    def test_existing_evidence_edges_are_rebuilt_and_plain_edges_kept(self):
        self.build()
        idx = self.fx.index()
        idx["edges"] = [{"id": "stale", "sourceId": "x", "targetId": "y", "relation": "calls", "resolution": "resolvedStatic", "evidenceIds": ["nope"]},
                        {"id": "contain", "sourceId": "p", "targetId": "q", "relation": "contains", "resolution": "resolvedStatic", "evidenceIds": []}]
        out = resolve.resolve(idx, self.root)
        ids = {e["id"] for e in out["edges"]}
        self.assertNotIn("stale", ids)
        self.assertIn("contain", ids)

    def test_missing_root_still_resolves_without_config(self):
        self.build()
        out = resolve.resolve(self.fx.index(), None)
        self.assertEqual(len(out["sites"]), len(self.fx.sites))

    def test_candidate_lists_are_bounded_and_truncation_is_reported(self):
        fx = self.fx
        fx.file("a.ts", language="typescript")
        mod = fx.module("a.ts", "typescript")
        cls = fx.sym("a.ts", "typescript", "Big", "class")
        for i in range(resolve.MAX_CANDIDATES + 20):
            fx.sym("a.ts", "typescript", "m", "method", cls)
        fn = fx.sym("a.ts", "typescript", "caller", "method", cls)
        s = fx.call("a.ts", "typescript", "m", receiver="this", caller=fn)
        self.run_idx()
        res, target, cands = self.site_res(s)
        self.assertEqual(res, "candidate")
        self.assertEqual(len(cands), resolve.MAX_CANDIDATES)
        self.assertIn("candidateCount=120", self.note(s))


class ExtraCases(ResolveCase):
    def test_deep_reexport_chain_converges_in_any_order(self):
        fx = self.fx
        depth = 300
        target = fx.fn("m0.ts", "typescript", "deep", exported=True)
        fx.file("m0.ts", language="typescript")
        fx.export("m0.ts", "typescript", "deep", symbol=target)
        # declared in reverse so a single forward pass cannot finish
        for i in range(depth, 0, -1):
            fx.file(f"m{i}.ts", language="typescript")
            fx.reexport(f"m{i}.ts", "typescript", f"./m{i - 1}", "deep", "deep")
        fx.file("use.ts", language="typescript")
        fx.imp("use.ts", "typescript", f"./m{depth}", "deep", "deep")
        call = fx.call("use.ts", "typescript", "deep")
        self.run_idx()
        self.assertResolved(call, target)
        self.assertFalse(any("did not converge" in m for m in self.idx["limitations"]))

    def test_ts_overload_signatures_exported_across_files_resolve_to_implementation(self):
        fx = self.fx
        fx.file("a.ts", language="typescript")
        fx.file("b.ts", language="typescript")
        s1 = fx.sym("b.ts", "typescript", "parse", "signature")
        s2 = fx.sym("b.ts", "typescript", "parse", "signature")
        impl = fx.sym("b.ts", "typescript", "parse", "function")
        for s in (s1, s2, impl):
            fx.export("b.ts", "typescript", "parse", symbol=s)
        fx.imp("a.ts", "typescript", "./b", "parse", "parse")
        c = fx.call("a.ts", "typescript", "parse")
        self.run_idx()
        self.assertResolved(c, impl)

    def test_rust_inline_modules_use_namespace_symbols_and_bodiless_mods(self):
        fx = self.fx
        fx.file("Cargo.toml", '[package]\nname = "x"\nversion = "0"\n')
        fx.file("src/lib.rs", language="rust")
        fx.file("src/ext.rs", language="rust")
        lib = fx.module("src/lib.rs", "rust")
        inner = fx.sym("src/lib.rs", "rust", "inner", "namespace", lib, body=True)
        g = fx.sym("src/lib.rs", "rust", "g", "function", inner)
        f = fx.sym("src/lib.rs", "rust", "f", "function", inner)
        deep = fx.sym("src/lib.rs", "rust", "deep", "namespace", inner, body=True)
        h = fx.sym("src/lib.rs", "rust", "h", "function", deep)
        mod_decl = fx.sym("src/lib.rs", "rust", "ext", "namespace", lib, body=False)  # `mod ext;`
        ext_fn = fx.sym("src/ext.rs", "rust", "e", "function", fx.module("src/ext.rs", "rust"))
        fx.site("src/lib.rs", "rust", "imports", "ext", {"kind": "mod", "specifier": "ext"})
        c_g = fx.site("src/lib.rs", "rust", "calls", "g", {"kind": "identifier", "name": "g", "receiver": None}, f)
        c_super = fx.site("src/lib.rs", "rust", "calls", "super::g", {"kind": "path", "name": "g", "receiver": "super"}, h)
        c_path = fx.site("src/lib.rs", "rust", "calls", "inner::deep::h", {"kind": "path", "name": "h", "receiver": "inner::deep"}, fx.sym("src/lib.rs", "rust", "top", "function", lib))
        c_ext = fx.site("src/lib.rs", "rust", "calls", "ext::e", {"kind": "path", "name": "e", "receiver": "ext"}, fx.sym("src/lib.rs", "rust", "top2", "function", lib))
        c_outer = fx.site("src/lib.rs", "rust", "calls", "top()", {"kind": "identifier", "name": "top", "receiver": None}, f)  # not visible without `use`
        self.run_idx()
        self.assertResolved(c_g, g)
        self.assertResolved(c_super, g)
        self.assertResolved(c_path, h)
        self.assertResolved(c_ext, ext_fn)
        self.assertRes(c_outer, "unresolved")

    def test_php_dir_relative_includes_strip_the_literal_slash(self):
        fx = self.fx
        fx.file("web/a.php", language="php")
        fx.file("web/b.php", language="php")
        rel = fx.imp("web/a.php", "php", "/b.php", kind="include", relation="includes", relativeTo="__DIR__")
        plain = fx.imp("web/a.php", "php", "b.php", kind="include", relation="includes", relativeTo=None)
        self.run_idx()
        self.assertResolved(rel, self.fid("web/b.php"))
        self.assertRes(plain, "candidate")

    def test_parser_origin_results_are_stable_across_repeated_passes(self):
        fx = self.fx
        fx.file("a.ts", language="typescript")
        lam = fx.sym("a.ts", "typescript", "", "lambda")
        iife = fx.site("a.ts", "typescript", "calls", "(()=>1)()", {"kind": "call", "iife": True}, resolution="resolvedStatic")
        next(s for s in fx.sites if s["id"] == iife)["targetId"] = lam
        fx.site("a.ts", "typescript", "calls", "x()", {"kind": "call", "name": "x"}, resolution="dynamic")
        fx.site("a.ts", "typescript", "macros", "m!()", {"kind": "macro", "name": "m"}, resolution="unsupported")
        fx.call("a.ts", "typescript", "nothing")
        first = json.dumps(self.run_idx(), sort_keys=True)
        second = json.dumps(resolve.resolve(json.loads(first), self.root), sort_keys=True)
        self.assertEqual(first, second)
        res = {s["raw"]: s["resolution"] for s in self.idx["sites"]}
        self.assertEqual(res["(()=>1)()"], "resolvedStatic")
        self.assertEqual(res["x()"], "dynamic")
        self.assertEqual(res["m!()"], "unsupported")

    def test_class_members_are_not_lexically_visible_and_function_valued_variables_resolve(self):
        fx = self.fx
        fx.file("a.ts", language="typescript")
        mod = fx.module("a.ts", "typescript")
        cls = fx.sym("a.ts", "typescript", "K", "class")
        meth = fx.sym("a.ts", "typescript", "m", "method", cls)
        caller = fx.sym("a.ts", "typescript", "other", "method", cls)
        bare = fx.call("a.ts", "typescript", "m", caller=caller)  # JS: `m()` is not `this.m()`
        arrow = fx.sym("a.ts", "typescript", "f", "lambda")
        fx.decl(mod, "f", "variable", symbol=arrow)
        use = fx.call("a.ts", "typescript", "f")
        arrow2 = fx.sym("a.ts", "typescript", "g", "lambda")
        fx.decl(mod, "g", "variable", symbol=arrow2)
        fx.decl(mod, "g", "assignment")
        reassigned = fx.call("a.ts", "typescript", "g")
        self.run_idx()
        self.assertRes(bare, "unresolved")
        self.assertResolved(use, arrow)
        self.assertEqual(self.site_res(reassigned), ("candidate", None, [arrow2]))

    def test_package_scope_wildcards_and_virtual_dispatch_in_fqn_languages(self):
        fx = self.fx
        for p in ("a/Main.java", "a/Sibling.java", "b/Util.java", "b/Other.java"):
            fx.file(p, language="java")
        a_mod = fx.module("a/Main.java", "java")
        fx.sym_by_id[a_mod].setdefault("bindings", {})["package"] = "com.a"
        main = fx.sym("a/Main.java", "java", "Main", "class", a_mod, fqn="com.a.Main")
        run = fx.sym("a/Main.java", "java", "run", "method", main, static=False)
        helper = fx.sym("a/Main.java", "java", "helper", "method", main, static=False)
        fx.decl(main, "helper", "function", symbol=helper)
        sib_mod = fx.module("a/Sibling.java", "java")
        sibling = fx.sym("a/Sibling.java", "java", "Sibling", "class", sib_mod, fqn="com.a.Sibling")
        util = fx.sym("b/Util.java", "java", "Util", "class", fx.module("b/Util.java", "java"), fqn="com.b.Util")
        fx.sym("b/Other.java", "java", "Other", "class", fx.module("b/Other.java", "java"), fqn="com.b.Other")
        fx.imp("a/Main.java", "java", "com.b", None, None, namespace=True)
        same_pkg = fx.call("a/Main.java", "java", "Sibling", kind="new", caller=run)
        wildcard = fx.call("a/Main.java", "java", "Util", kind="new", caller=run)
        virtual = fx.call("a/Main.java", "java", "helper", caller=run)
        missing = fx.call("a/Main.java", "java", "Ghost", kind="new", caller=run)
        self.run_idx()
        self.assertResolved(same_pkg, sibling)
        res, target, cands = self.site_res(wildcard)
        self.assertEqual((res, target, cands), ("candidate", None, [util]))  # on-demand import: shadowing by classpath types is possible
        self.assertEqual(self.site_res(virtual), ("candidate", None, [helper]))
        self.assertRes(missing, "unresolved")

    def test_verilog_instantiation_is_name_candidate_and_parser_external_is_kept(self):
        fx = self.fx
        fx.file("top.v", language="verilog")
        fx.file("leaf.v", language="verilog")
        leaf = fx.sym("leaf.v", "verilog", "leaf", "module", fx.module("leaf.v", "verilog"))
        inst = fx.site("top.v", "verilog", "instantiates", "leaf u1()", {"kind": "identifier", "name": "leaf", "receiver": None})
        absent = fx.site("top.v", "verilog", "instantiates", "ghost g()", {"kind": "identifier", "name": "ghost", "receiver": None})
        sysfn = fx.site("top.v", "verilog", "calls", "$display()", {"kind": "identifier", "name": "$display", "receiver": None}, resolution="external")
        self.run_idx()
        self.assertEqual(self.site_res(inst), ("candidate", None, [leaf]))
        self.assertRes(absent, "unresolved")
        self.assertRes(sysfn, "external")
        again = json.dumps(self.idx, sort_keys=True)
        self.assertEqual(again, json.dumps(resolve.resolve(json.loads(again), self.root), sort_keys=True))

    def test_junction_or_symlinked_directory_config_is_refused(self):
        real = os.path.join(self.root, "real")
        os.makedirs(real)
        body = json.dumps({"compilerOptions": {"paths": {"@a/*": ["./*"]}}}).encode()
        Path(real, "tsconfig.json").write_bytes(body)
        link = os.path.join(self.root, "linked")
        try:
            os.symlink(real, link, target_is_directory=True)
        except (OSError, NotImplementedError):
            import subprocess

            if os.name != "nt" or subprocess.run(["cmd", "/c", "mklink", "/J", link, real], capture_output=True).returncode != 0:
                self.skipTest("neither symlinks nor junctions can be created here")
        fx = self.fx
        fx.files.append({"id": sid("file", "linked/tsconfig.json"), "path": "linked/tsconfig.json", "sha256": hashlib.sha256(body).hexdigest(), "bytes": len(body), "language": None, "status": "unsupported", "diagnostics": []})
        fx.file("linked/a.ts", None, language="typescript")
        fx.file("linked/b.ts", None, language="typescript")
        s = fx.imp("linked/a.ts", "typescript", "@a/b", "x", "x")
        self.run_idx()
        self.assertRes(s, "unresolved")
        self.assertTrue(any("link" in m for m in self.idx["limitations"]))


if __name__ == "__main__":
    unittest.main()
