"""Static resolution pass over a complete immutable source index.

``resolve(index, root)`` consumes parser facts (symbols, sites, binding metadata)
and the *inventory* (every admitted file path) and rewrites site resolutions,
``targetId``/``candidateIds``, derived edges and coverage site counts.

Guarantees
* Never executes repository code, never runs build tools or shells.  The only
  filesystem access is reading small configuration files (tsconfig/jsconfig,
  package.json, Cargo.toml, go.mod, pyproject.toml ...) that are listed in the
  inventory, lie below ``root`` without links/reparse points, and still match
  their inventory SHA-256.
* Specifiers never become filesystem paths outside the inventory: absolute
  paths, drive letters, backslashes, NUL and ``..`` escapes are rejected.
* No resolution by spelling alone.  A site is ``resolvedStatic`` only for a
  lexical binding without conflicting shadow/rebinding, or a literal
  file/module target under the language rules.  Conditions, ambiguous export
  maps, overloads, virtual dispatch and include-path-dependent lookups are
  ``candidate`` (even if only one candidate exists); dynamic receivers are
  ``dynamic``; unknown things stay ``unresolved``.  No path proximity or
  test/non-test preference.
* Every recognized site is retained, whatever its resolution.
"""
from __future__ import annotations

import functools
import hashlib
import json
import os
import posixpath
import re
import sys
import tomllib
from collections import defaultdict, deque
from typing import Any, Iterable

import common

RESOLVED = "resolvedStatic"
CANDIDATE = "candidate"
DYNAMIC = "dynamic"
EXTERNAL = "external"
UNRESOLVED = "unresolved"
UNSUPPORTED = "unsupported"

PARSER_NOTES = frozenset({"parser-recognized-unanalyzable", "parser-marked-dynamic", "parser-marked-external", "iife"})
MAX_CANDIDATES = 100
MAX_CONFIG_BYTES = 1024 * 1024
MAX_CONFIG_FILES = 4000
MAX_ROUNDS = 5000

JS_LANGS = frozenset({"javascript", "typescript", "tsx"})
C_LANGS = frozenset({"c", "cpp", "objc"})
MODULE_RELATIONS = frozenset({"imports", "includes", "reexports"})
CLASS_KINDS = frozenset({"class", "struct", "enum", "interface", "trait", "impl", "type"})
TYPE_KINDS = frozenset({"class", "struct", "enum", "interface", "trait", "type"})
CALLABLE_KINDS = frozenset({"function", "method", "constructor", "accessor", "lambda", "closure", "macro"})
ITEM_KINDS = CALLABLE_KINDS | TYPE_KINDS | {"module", "namespace", "signature"}
DECL_SYMBOL_KINDS = frozenset({"function", "class", "type", "enum", "namespace", "interface", "struct", "trait", "variable"})
JS_EXTS = (".ts", ".tsx", ".d.ts", ".mts", ".cts", ".d.mts", ".d.cts", ".js", ".jsx", ".mjs", ".cjs", ".json")
JS_EXT_SWAP = {
    ".js": (".ts", ".tsx", ".d.ts"),
    ".jsx": (".tsx",),
    ".mjs": (".mts", ".d.mts"),
    ".cjs": (".cts", ".d.cts"),
}
CONTAINER_EXTS = (".svelte", ".vue", ".astro")
NODE_BUILTINS = frozenset(
    "assert async_hooks buffer child_process cluster console constants crypto dgram diagnostics_channel dns domain events fs "
    "http http2 https inspector module net os path perf_hooks process punycode querystring readline repl stream string_decoder "
    "sys test timers tls trace_events tty url util v8 vm wasi worker_threads zlib".split()
)
JS_GLOBALS = frozenset(
    "console Object Array Promise JSON Math Number String Boolean Symbol Map Set WeakMap WeakSet Date RegExp Error TypeError "
    "RangeError SyntaxError parseInt parseFloat isNaN isFinite setTimeout setInterval clearTimeout clearInterval "
    "queueMicrotask structuredClone fetch require process Buffer URL URLSearchParams TextEncoder TextDecoder AbortController "
    "Reflect Proxy BigInt Intl globalThis window document navigator requestAnimationFrame cancelAnimationFrame alert "
    "encodeURIComponent decodeURIComponent encodeURI decodeURI atob btoa Uint8Array Int32Array Float64Array ArrayBuffer".split()
)
GO_BUILTINS = frozenset("append cap clear close complex copy delete imag len make max min new panic print println real recover".split())
RUST_STD_CRATES = frozenset({"std", "core", "alloc", "proc_macro", "test"})
RUST_PRELUDE_FUNCS = frozenset({"drop", "Some", "None", "Ok", "Err", "Box", "Vec", "String", "Option", "Result", "Default"})
GO_BUILD_SUFFIX = re.compile(
    r"_(aix|android|darwin|dragonfly|freebsd|hurd|illumos|ios|js|linux|nacl|netbsd|openbsd|plan9|solaris|wasip1|windows|"
    r"386|amd64|arm|arm64|loong64|mips|mipsle|mips64|mips64le|ppc64|ppc64le|riscv64|s390x|sparc64|wasm)(_[a-z0-9]+)?\.go$"
)
PY_STDLIB = frozenset(getattr(sys, "stdlib_module_names", ()))
try:  # builtin names are interpreter data, not repository data
    import builtins as _builtins

    PY_BUILTINS = frozenset(dir(_builtins))
except Exception:  # pragma: no cover
    PY_BUILTINS = frozenset()
SCHEME_RE = re.compile(r"^[A-Za-z][A-Za-z0-9+.-]*:")
WINDOWS_DRIVE_RE = re.compile(r"^[A-Za-z]:")


def stable_id(*parts: Any) -> str:
    """Length-delimited SHA-256 over ``str(part)`` (same framing as ``common.stable_id``)."""
    h = hashlib.sha256()
    for part in parts:
        data = str(part).encode("utf-8", "surrogatepass")
        h.update(len(data).to_bytes(8, "big"))
        h.update(data)
    return h.hexdigest()


# ---------------------------------------------------------------------------
# Path and text helpers
# ---------------------------------------------------------------------------

def norm_join(base_dir: str, rel: str) -> str | None:
    """Join and normalize ``rel`` below an inventory directory; ``None`` if hostile/escaping."""
    if not isinstance(rel, str) or not rel or "\x00" in rel or "\\" in rel:
        return None
    if rel.startswith("/") or WINDOWS_DRIVE_RE.match(rel):
        return None
    if ":" in rel:
        return None
    joined = posixpath.normpath(posixpath.join(base_dir, rel))
    if joined == ".":
        return ""
    if joined == ".." or joined.startswith("../"):
        return None
    return joined


def dirname(path: str) -> str:
    return path.rsplit("/", 1)[0] if "/" in path else ""


def basename(path: str) -> str:
    return path.rsplit("/", 1)[-1]


def ancestors(directory: str) -> Iterable[str]:
    """``a/b/c`` -> ``a/b/c``, ``a/b``, ``a``, ``''`` (root)."""
    d = directory
    while True:
        yield d
        if not d:
            return
        d = dirname(d)


def in_dir(path: str, directory: str) -> bool:
    return directory == "" or path == directory or path.startswith(directory + "/")


def strip_jsonc(text: str) -> str:
    """Remove comments and trailing commas from JSONC without touching strings."""
    out: list[str] = []
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if c == '"':
            j = i + 1
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == '"':
                    break
                j += 1
            out.append(text[i : j + 1])
            i = j + 1
        elif c == "/" and i + 1 < n and text[i + 1] == "/":
            while i < n and text[i] not in "\r\n":
                i += 1
        elif c == "/" and i + 1 < n and text[i + 1] == "*":
            j = text.find("*/", i + 2)
            i = n if j < 0 else j + 2
        else:
            out.append(c)
            i += 1
    cleaned = "".join(out)
    # trailing commas (outside strings): rescan cheaply with a small state machine
    res: list[str] = []
    i, n = 0, len(cleaned)
    while i < n:
        c = cleaned[i]
        if c == '"':
            j = i + 1
            while j < n:
                if cleaned[j] == "\\":
                    j += 2
                    continue
                if cleaned[j] == '"':
                    break
                j += 1
            res.append(cleaned[i : j + 1])
            i = j + 1
            continue
        if c == ",":
            j = i + 1
            while j < n and cleaned[j] in " \t\r\n":
                j += 1
            if j < n and cleaned[j] in "}]":
                i += 1
                continue
        res.append(c)
        i += 1
    return "".join(res)


@functools.lru_cache(maxsize=8192)
def glob_regex(pattern: str) -> re.Pattern:
    out = []
    i = 0
    while i < len(pattern):
        c = pattern[i]
        if c == "*":
            if pattern[i : i + 3] == "**/":
                out.append("(?:.*/)?")
                i += 3
                continue
            if pattern[i : i + 2] == "**":
                out.append(".*")
                i += 2
                continue
            out.append("[^/]*")
        elif c == "?":
            out.append("[^/]")
        else:
            out.append(re.escape(c))
        i += 1
    return re.compile("".join(out) + r"\Z")


def ent_id(ent: tuple) -> str | None:
    return ent[1] if ent[0] in ("sym", "file") else None


class ModRes:
    """Result of resolving a module/import/include specifier."""

    __slots__ = ("kind", "files", "dir", "note", "ext")

    def __init__(self, kind: str, files: list[str] | None = None, *, dir: str | None = None, note: str | None = None, ext: str | None = None):
        self.kind = kind  # file | candidates | dir | external | unresolved | dynamic | unsupported
        self.files = files or []
        self.dir = dir
        self.note = note
        self.ext = ext


class Lex:
    """Lexical lookup result: kind in found|local|none."""

    __slots__ = ("kind", "ents", "conflict", "scope")

    def __init__(self, kind: str, ents: set | None = None, conflict: bool = False, scope: str | None = None):
        self.kind = kind
        self.ents = ents if ents is not None else set()
        self.conflict = conflict
        self.scope = scope


# ---------------------------------------------------------------------------
# Resolver
# ---------------------------------------------------------------------------

class Resolver:
    def __init__(self, index: dict, root: Any):
        self.index = index
        self.root = os.fspath(root) if root is not None else None
        self.real_root = os.path.realpath(self.root) if self.root else None
        self.limitations: list[str] = []
        self._limit_seen: set[str] = set()
        self.files: list[dict] = index.get("files") or []
        self.symbols: list[dict] = index.get("symbols") or []
        self.sites: list[dict] = index.get("sites") or []
        self.fpath: dict[str, dict] = {}
        self.fid: dict[str, dict] = {}
        for f in self.files:
            self.fpath[f["path"]] = f
            self.fid[f["id"]] = f
        self.sym: dict[str, dict] = {s["id"]: s for s in self.symbols}
        self.children: dict[str, list[dict]] = defaultdict(list)
        self.file_symbols: dict[str, list[dict]] = defaultdict(list)
        for s in self.symbols:
            if s.get("ownerId"):
                self.children[s["ownerId"]].append(s)
            self.file_symbols[s["fileId"]].append(s)
        self.file_modsym: dict[str, dict] = {}
        self.mod_by_region: dict[tuple[str, str], dict] = {}
        for s in self.symbols:
            if s["kind"] == "module":
                key = (s["fileId"], s.get("region", ""))
                self.mod_by_region.setdefault(key, s)
                if not s.get("ownerId") or self.sym.get(s["ownerId"], {}).get("fileId") != s["fileId"]:
                    self.file_modsym.setdefault(s["fileId"], s)
        self.dir_files: dict[str, list[str]] = defaultdict(list)
        self.lower: dict[str, list[str]] = defaultdict(list)
        self.all_dirs: set[str] = {""}
        self.by_basename: dict[str, list[str]] = defaultdict(list)
        self.c_names: dict[str, dict[str, list[dict]]] = {}
        self._pkg_members: dict[str, set[str]] | None = None
        self._cfg_by_path: dict[str, dict] | None = None
        self._changed: set = set()
        for f in self.files:
            self.dir_files[dirname(f["path"])].append(f["path"])
            self.lower[f["path"].lower()].append(f["path"])
            self.by_basename[basename(f["path"])].append(f["path"])
            for a in ancestors(dirname(f["path"])):
                if a in self.all_dirs:
                    break
                self.all_dirs.add(a)
        for d in index.get("directories") or []:
            if isinstance(d, str):
                self.all_dirs.update(ancestors(d.strip("/")))
        # languages whose files are not all fully parsed (external is not provable there)
        self.lang_incomplete: set[str] = set()
        for f in self.files:
            lang = f.get("language")
            if lang and f.get("status") != "parsed":
                self.lang_incomplete.add(lang)
        self._decl_index: dict[str, dict[str, list[dict]]] = {}
        self._json_cache: dict[str, Any] = {}
        self._modres_cache: dict[str, ModRes] = {}
        self._import_cache: dict[str, frozenset] = {}
        self.finalized = False
        self.exp: dict[str, dict[str, set]] = defaultdict(lambda: defaultdict(set))
        self.glob: dict[str, dict[str, set]] = defaultdict(lambda: defaultdict(set))
        self._site_by_id: dict[str, dict] = {s["id"]: s for s in self.sites}
        self._js_cfg_cache: dict[str, Any] = {}
        self._pkg_index: dict[str, str] | None = None
        self._tsconfigs: list[dict] | None = None
        self._rust_ready = False
        self._go_ready = False
        self._py_deps: set[str] | None = None

    # -- bookkeeping ----------------------------------------------------------
    def limit(self, message: str) -> None:
        if message not in self._limit_seen:
            self._limit_seen.add(message)
            self.limitations.append(message)

    def has(self, path: str) -> bool:
        return path in self.fpath

    def fid_of(self, path: str) -> str:
        return self.fpath[path]["id"]

    # -- safe configuration reads ---------------------------------------------
    def read_text(self, path: str) -> str | None:
        f = self.fpath.get(path)
        if f is None or self.root is None:
            return None
        try:
            if int(f.get("bytes") or 0) > MAX_CONFIG_BYTES:
                self.limit(f"Configuration file {path} exceeds {MAX_CONFIG_BYTES} bytes and was not read for resolution.")
                return None
            if "\x00" in path or ".." in path.split("/") or path.startswith("/"):
                return None
            full = os.path.join(self.root, *path.split("/"))
            real = common.plain_path(os.path.realpath(common.extended_path(full)))
            expected = common.plain_path(os.path.join(self.real_root, *path.split("/")))
            if os.path.normcase(real) != os.path.normcase(expected):
                self.limit(f"Configuration file {path} resolves through a link or reparse point and was not read.")
                return None
            st = os.lstat(common.extended_path(full))
            import stat as _stat

            if not _stat.S_ISREG(st.st_mode) or st.st_size > MAX_CONFIG_BYTES:
                return None
            with open(common.extended_path(full), "rb") as fh:
                data = fh.read(MAX_CONFIG_BYTES + 1)
            if len(data) > MAX_CONFIG_BYTES:
                return None
            if f.get("sha256") and hashlib.sha256(data).hexdigest() != f["sha256"]:
                self.limit(f"Configuration file {path} no longer matches its inventory hash and was ignored.")
                return None
            if data.startswith(b"\xef\xbb\xbf"):
                data = data[3:]
            return data.decode("utf-8")
        except (OSError, UnicodeDecodeError, ValueError):
            self.limit(f"Configuration file {path} could not be read as UTF-8 and was ignored.")
            return None

    def read_json(self, path: str) -> Any:
        if path in self._json_cache:
            return self._json_cache[path]
        text = self.read_text(path)
        data = None
        if text is not None:
            try:
                data = json.loads(strip_jsonc(text))
            except ValueError:
                self.limit(f"Configuration file {path} is not valid JSON/JSONC and was ignored.")
        self._json_cache[path] = data
        return data

    def read_toml(self, path: str) -> Any:
        key = "toml:" + path
        if key in self._json_cache:
            return self._json_cache[key]
        text = self.read_text(path)
        data = None
        if text is not None:
            try:
                data = tomllib.loads(text)
            except (tomllib.TOMLDecodeError, ValueError):
                self.limit(f"Configuration file {path} is not valid TOML and was ignored.")
        self._json_cache[key] = data
        return data

    # ======================================================================
    # Declarations / lexical lookup
    # ======================================================================
    def decls_of(self, scope: dict) -> dict[str, list[dict]]:
        sid = scope["id"]
        cached = self._decl_index.get(sid)
        if cached is not None:
            return cached
        by_name: dict[str, list[dict]] = defaultdict(list)
        seen_syms: set[str] = set()
        for d in (scope.get("bindings") or {}).get("declarations") or []:
            by_name[d["name"]].append(d)
            if d.get("symbolId"):
                seen_syms.add(d["symbolId"])
        # Symbols a parser did not declare explicitly are visible in the whole scope, except class members
        # (members are only lexically visible where the parser declared them, e.g. Java/C# method names).
        for child in self.children.get(sid, ()) if scope["kind"] not in CLASS_KINDS else ():
            if child["id"] in seen_syms or not child.get("name"):
                continue
            if child["kind"] in DECL_SYMBOL_KINDS or child["kind"] in ("signature", "macro"):
                by_name[child["name"]].append(
                    {"name": child["name"], "kind": "function" if child["kind"] in CALLABLE_KINDS or child["kind"] == "signature" else child["kind"], "symbolId": child["id"], "siteId": None, "mutable": False, "scopeStart": 0, "scopeEnd": 1 << 62, "implicit": True}
                )
        self._decl_index[sid] = by_name
        return by_name

    @staticmethod
    def is_rmod(sym: dict) -> bool:
        """Rust module scope: the file's synthetic module or an inline `mod x { .. }` (parser kind namespace, body true)."""
        if sym["language"] != "rust":
            return False
        if sym["kind"] == "module":
            return True
        return sym["kind"] == "namespace" and (sym.get("bindings") or {}).get("body") is not False

    @staticmethod
    def is_signature(sym: dict) -> bool:
        if sym["kind"] == "signature":
            return True
        b = sym.get("bindings") or {}
        return b.get("body") is False

    def scope_hit(self, scope: dict, name: str, pos: int, lang: str) -> Lex | None:
        decls = self.decls_of(scope).get(name)
        if not decls:
            return None
        inside = [d for d in decls if d["scopeStart"] <= pos < d["scopeEnd"]]
        if not inside:
            return None
        narrow = min(d["scopeEnd"] - d["scopeStart"] for d in inside)
        group = [d for d in inside if d["scopeEnd"] - d["scopeStart"] == narrow]
        if all(d["kind"] == "global" for d in group):
            return None
        group = [d for d in group if d["kind"] != "global"]
        syms: dict[str, dict] = {}
        imports: list[dict] = []
        local = False
        rebinding = False
        for d in group:
            k = d["kind"]
            if k == "assignment":
                rebinding = True
            elif k in ("variable", "parameter", "catch") and not d.get("symbolId"):
                local = True
            elif d.get("symbolId") and d["symbolId"] in self.sym:
                syms[d["symbolId"]] = self.sym[d["symbolId"]]
            elif k == "import" and d.get("siteId"):
                imports.append(d)
            elif k in ("variable", "parameter", "catch"):
                local = True
        impls = {i: s for i, s in syms.items() if not self.is_signature(s)}
        chosen = impls if impls else syms
        ents: set = {("sym", i) for i in chosen}
        for d in imports:
            ents |= self.import_ents(self._site_by_id.get(d["siteId"]))
        definitions = len(chosen) + len(imports)
        conflict = rebinding or local and definitions > 0 or definitions > 1 or (bool(syms) and not impls)
        if not ents:
            return Lex("local", set(), False, scope["id"])
        return Lex("found", ents, conflict, scope["id"])

    def lex_lookup(self, scope_id: str | None, name: str, pos: int, lang: str, file_id: str) -> Lex:
        sid = scope_id
        first = True
        hops = 0
        while sid and hops < 100000:
            hops += 1
            s = self.sym.get(sid)
            if s is None:
                break
            if lang == "rust" and self.is_rmod(s):
                # a Rust module is a hard scope boundary: nothing outside it is visible without `use`
                hit = self.ns_get(s["id"], name)
                return Lex("found", set(hit), False, sid) if hit else Lex("none")
            elif lang == "python" and s["kind"] == "module":
                hit = self.scope_hit(s, name, pos, lang)
                if hit:
                    return hit
                star = self.glob.get(file_id, {}).get(name)
                if star:
                    return Lex("found", set(star), False, sid)
            elif not (lang == "python" and s["kind"] == "class" and not first):
                hit = self.scope_hit(s, name, pos, lang)
                if hit:
                    return hit
            first = False
            sid = s.get("ownerId")
        return Lex("none")

    def site_scope(self, site: dict) -> str | None:
        cid = site.get("callerId")
        if cid and cid in self.sym:
            return cid
        m = self.mod_by_region.get((site["fileId"], site.get("region", "")))
        if m is None:
            m = self.file_modsym.get(site["fileId"])
        return m["id"] if m else None

    # ======================================================================
    # Namespace tables (fixed point)
    # ======================================================================
    def ns_get(self, key: str, name: str) -> set | None:
        e = self.exp.get(key)
        if e is not None:
            hit = e.get(name)
            if hit:
                return hit
        g = self.glob.get(key)
        if g is not None:
            hit = g.get(name)
            if hit:
                return hit
        return None

    def ns_names(self, key: str) -> set[str]:
        names = set()
        if key in self.exp:
            names.update(n for n, v in self.exp[key].items() if v)
        if key in self.glob:
            names.update(n for n, v in self.glob[key].items() if v)
        return names

    def _add(self, table: dict, key: str, name: str, ents: Iterable) -> bool:
        cur = table[key][name]
        before = len(cur)
        cur.update(ents)
        if len(cur) != before:
            self._changed.add(key)
            return True
        return False

    _changed: set  # keys whose namespace tables grew during the current fixed-point round

    def _modres_deps(self, site: dict) -> frozenset:
        mr = self.modres(site)
        return frozenset(self.fid_of(p) for p in mr.files) if mr.kind in ("file", "candidates") else frozenset()

    def _local_import_deps(self, mod: dict, name: str) -> frozenset:
        deps: set = set()
        for d in self.decls_of(mod).get(name, ()):
            if d["kind"] == "import" and d.get("siteId") in self._site_by_id:
                deps |= self._modres_deps(self._site_by_id[d["siteId"]])
        return frozenset(deps)

    def _js_local_export(self, fid: str, name: str, mod: dict, local: str) -> bool:
        return self._add(self.exp, fid, name, self.module_local_ents(mod, local))

    def _py_import_export(self, fid: str, name: str, site: dict) -> bool:
        return self._add(self.exp, fid, name, self.import_ents(site))

    def build_tables(self) -> None:
        self.finalized = False
        js_local: list[tuple] = []
        js_sites: list[dict] = []
        py_imports: list[tuple] = []
        py_stars: list[dict] = []
        rust_uses: list[dict] = []
        rust_mods: list[dict] = []
        # static content
        for m in self.symbols:
            if m["kind"] != "module":
                continue
            lang = m["language"]
            fid = m["fileId"]
            b = m.get("bindings") or {}
            if lang in JS_LANGS:
                for e in b.get("exports") or []:
                    name = "default" if e.get("default") else e.get("exported")
                    js_local.append((fid, name, e.get("symbolId"), e.get("local"), m))
        for s in self.symbols:
            if s["language"] in JS_LANGS and s.get("exported") and s.get("ownerId") and self.sym.get(s["ownerId"], {}).get("kind") == "module":
                names = (s.get("bindings") or {}).get("exportNames") or ([s["name"]] if s.get("name") else [])
                for n in names:
                    js_local.append((s["fileId"], n, s["id"], None, None))
        for site in self.sites:
            lang = site["language"]
            b = site.get("binding") or {}
            rel = site["relation"]
            if lang in JS_LANGS and rel == "reexports":
                if b.get("specifier") is not None:
                    js_sites.append(site)
                else:
                    js_local.append((site["fileId"], b.get("alias") or b.get("imported"), None, b.get("imported") or b.get("local"), self.mod_by_region.get((site["fileId"], site.get("region", "")))))
            elif lang == "python":
                if rel in ("imports", "reexports") and b.get("namespace") and b.get("specifier") is not None and not b.get("alias") and not b.get("local"):
                    py_stars.append(site)
            elif lang == "rust":
                kind = b.get("kind")
                if kind == "mod":
                    rust_mods.append(site)
                elif kind == "use" or rel == "reexports":
                    rust_uses.append(site)
        # python / rust initial items
        for m in self.symbols:
            if m["kind"] != "module" and not self.is_rmod(m):
                continue
            fid = m["fileId"]
            if m["language"] == "python":
                if m.get("ownerId") and self.sym.get(m["ownerId"], {}).get("fileId") == fid:
                    continue
                for name, ds in self.decls_of(m).items():
                    for d in ds:
                        k = d["kind"]
                        if d.get("symbolId") and d["symbolId"] in self.sym:
                            self.exp[fid][name].add(("sym", d["symbolId"]))
                        elif k in ("variable",):
                            self.exp[fid][name].add(("val", name))
                        elif k == "import" and d.get("siteId"):
                            py_imports.append((fid, name, d["siteId"]))
            elif m["language"] == "rust":
                key = m["id"]
                for child in self.children.get(key, ()):
                    if not child.get("name") or child["kind"] not in ITEM_KINDS or child["kind"] == "impl":
                        continue
                    if child["kind"] in ("namespace", "module") and (child.get("bindings") or {}).get("body") is False:
                        continue  # `mod x;` is supplied by the mod-declaration rule (file entity)
                    self.exp[key][child["name"]].add(("sym", child["id"]))
        # fixed point: specs are re-evaluated only when a namespace they read changed in the previous round
        specs: list[tuple[Any, Any]] = []
        for fid, name, symid, local, m in js_local:
            if not name:
                continue
            if symid and symid in self.sym:
                specs.append((frozenset(), functools.partial(self._add, self.exp, fid, name, {("sym", symid)})))
            elif local and m is not None:
                specs.append((self._local_import_deps(m, local), functools.partial(self._js_local_export, fid, name, m, local)))
        for site in js_sites:
            specs.append((self._modres_deps(site), functools.partial(self._js_reexport, site)))
        for fid, name, siteid in py_imports:
            site = self._site_by_id[siteid]
            specs.append((self._modres_deps(site), functools.partial(self._py_import_export, fid, name, site)))
        for site in py_stars:
            specs.append((self._modres_deps(site), functools.partial(self._py_star, site)))
        if rust_mods or rust_uses:
            specs.append(("rust", functools.partial(self._rust_round, rust_mods, rust_uses)))
        converged = False
        dirty: set | None = None
        for _ in range(MAX_ROUNDS):
            self._changed = set()
            for deps, fn in specs:
                if dirty is not None:
                    if deps == "rust":
                        if not any(self.sym.get(k, {}).get("language") == "rust" for k in dirty):
                            continue
                    elif not (deps & dirty):
                        continue
                fn()
            if not self._changed:
                converged = True
                break
            dirty = self._changed
        if not converged:
            self.limit(f"Module export/namespace resolution did not converge within {MAX_ROUNDS} rounds; some re-exports may be missing.")
        self.finalized = True
        self._import_cache.clear()

    def module_local_ents(self, mod: dict, name: str) -> set:
        ents: set = set()
        for d in self.decls_of(mod).get(name, ()):
            if d.get("symbolId") in self.sym:
                ents.add(("sym", d["symbolId"]))
            elif d["kind"] == "import" and d.get("siteId"):
                ents |= self.import_ents(self._site_by_id.get(d["siteId"]))
            elif d["kind"] in ("variable", "parameter"):
                ents.add(("val", name))
        return ents

    # -- JS re-exports --------------------------------------------------------
    def _js_reexport(self, site: dict) -> bool:
        b = site["binding"]
        mr = self.modres(site)
        fid = site["fileId"]
        changed = False
        if mr.kind not in ("file", "candidates"):
            return False
        targets = [self.fid_of(p) for p in mr.files]
        exported = b.get("alias")
        imported = b.get("imported")
        if b.get("namespace") and not exported and not imported:
            for t in targets:
                for n in self.ns_names(t):
                    if n == "default":
                        continue
                    changed |= self._add(self.glob, fid, n, self.ns_get(t, n) or ())
            return changed
        if b.get("namespace") and exported:
            return self._add(self.exp, fid, exported, {("file", t) for t in targets})
        if imported is None:
            return False
        out_name = exported or imported
        for t in targets:
            ents = self.ns_get(t, imported)
            if not ents and not self.file_has_exports(t):
                ents = {("file", t)}
            if ents:
                changed |= self._add(self.exp, fid, out_name, ents)
        return changed

    def file_has_exports(self, fid: str) -> bool:
        f = self.fid[fid]
        return f.get("status") in ("parsed", "partial") and (f.get("language") in JS_LANGS or f["path"].endswith(CONTAINER_EXTS))

    # -- Python star ----------------------------------------------------------
    def _py_star(self, site: dict) -> bool:
        changed = False
        for ent in self.py_module_ents(site):
            if ent[0] != "file":
                continue
            for n in self.ns_names(ent[1]):
                if n.startswith("_"):
                    continue
                changed |= self._add(self.glob, site["fileId"], n, self.ns_get(ent[1], n) or ())
        return changed

    # ======================================================================
    # Import entity resolution
    # ======================================================================
    def import_ents(self, site: dict | None) -> frozenset:
        if site is None:
            return frozenset()
        cached = self._import_cache.get(site["id"])
        if cached is not None:
            return cached
        lang = site["language"]
        b = site.get("binding") or {}
        mr = self.modres(site)
        ents: set = set()
        if mr.kind == "external":
            ents.add(("ext", mr.ext or str(b.get("specifier"))))
        elif mr.kind in ("unresolved", "dynamic", "unsupported"):
            ents.add(("unres", mr.note or mr.kind))
        elif lang in JS_LANGS or self.path_is_container(site["path"]):
            ents |= self._js_import_ents(site, b, mr)
        elif lang == "python":
            ents |= self._py_import_ents(site, b, mr)
        elif lang == "go":
            if mr.kind in ("dir", "candidates", "file"):
                ents.add(("dir", mr.dir or dirname(mr.files[0])))
        elif lang == "rust":
            ents |= self.rust_use_ents(site)
        else:
            ents |= self._generic_import_ents(site, b, mr)
        out = frozenset(ents)
        if self.finalized:
            self._import_cache[site["id"]] = out
        return out

    def path_is_container(self, path: str) -> bool:
        return path.endswith(CONTAINER_EXTS)

    def _js_import_ents(self, site: dict, b: dict, mr: ModRes) -> set:
        ents: set = set()
        if mr.kind not in ("file", "candidates"):
            return ents
        imported = b.get("imported")
        for p in mr.files:
            t = self.fid_of(p)
            if b.get("namespace") and not imported:
                ents.add(("file", t))
            elif b.get("default") or imported == "default":
                hit = self.ns_get(t, "default")
                if hit:
                    ents |= hit
                elif p.endswith(CONTAINER_EXTS) or not self.file_has_exports(t):
                    ents.add(("file", t))
                else:
                    ents.add(("unres", "default-export-not-found"))
            elif imported:
                hit = self.ns_get(t, imported)
                if hit:
                    ents |= hit
                elif not self.file_has_exports(t):
                    ents.add(("file", t))
                else:
                    ents.add(("unres", f"export-not-found:{imported}"))
            else:
                ents.add(("file", t))
        return ents

    # ======================================================================
    # Module resolution dispatch
    # ======================================================================
    def modres(self, site: dict) -> ModRes:
        cached = self._modres_cache.get(site["id"])
        if cached is not None:
            return cached
        b = site.get("binding") or {}
        spec = b.get("specifier")
        lang = site["language"]
        if spec is None or b.get("dynamic"):
            res = ModRes("dynamic", note="non-literal specifier")
        else:
            try:
                if lang in JS_LANGS or self.path_is_container(site["path"]):
                    res = self.js_modres(site, spec)
                elif lang == "python":
                    res = self.py_modres(site, spec)
                elif lang == "rust":
                    res = self.rust_modres(site, spec)
                elif lang == "go":
                    res = self.go_modres(site, spec)
                elif lang in C_LANGS:
                    res = self.c_modres(site, spec)
                else:
                    res = self.generic_modres(site, spec)
            except RecursionError:
                res = ModRes("unresolved", note="resolution recursion limit")
        if lang != "rust" or self.finalized:
            self._modres_cache[site["id"]] = res
        return res

    def case_hint(self, path: str) -> str | None:
        lp = path.lower()
        for e in ("",) + JS_EXTS + (".py", ".pyi", ".rb", ".lua", ".rs", ".go", ".h", ".c", ".sh", ".zig"):
            alts = [p for p in self.lower.get(lp + e, ()) if p != path + e]
            if alts and (path + e) not in self.fpath:
                return f"case-mismatch (inventory has {alts[0]})"
        return None

    def file_res(self, paths: list[str], *, note: str | None = None, certain: bool = True) -> ModRes:
        paths = sorted(set(paths))
        if len(paths) == 1 and certain:
            return ModRes("file", paths, note=note)
        return ModRes("candidates", paths, note=note)

    # ======================================================================
    # JS/TS resolution
    # ======================================================================
    def js_probe(self, target: str) -> list[str]:
        """All inventory files TS/Node-style resolution could pick for ``target`` (first stage with hits)."""
        if target in self.fpath:
            return [target]
        for ext, swaps in JS_EXT_SWAP.items():
            if target.endswith(ext):
                stem = target[: -len(ext)]
                hits = [stem + s for s in swaps if stem + s in self.fpath]
                if hits:
                    return hits
        hits = [target + e for e in JS_EXTS if target + e in self.fpath]
        if hits:
            return hits
        pkg = posixpath.join(target, "package.json")
        if pkg in self.fpath:
            data = self.read_json(pkg)
            if isinstance(data, dict):
                for field in ("types", "typings", "module", "main"):
                    v = data.get(field)
                    if isinstance(v, str):
                        sub = norm_join(target, v)
                        if sub is not None:
                            hits = self.js_probe_file_only(sub)
                            if hits:
                                return hits
        base = target
        idx = [posixpath.join(base, "index" + e) for e in JS_EXTS]
        return [i for i in idx if i in self.fpath]

    def js_probe_file_only(self, target: str) -> list[str]:
        if target in self.fpath:
            return [target]
        for ext, swaps in JS_EXT_SWAP.items():
            if target.endswith(ext):
                stem = target[: -len(ext)]
                hits = [stem + s for s in swaps if stem + s in self.fpath]
                if hits:
                    return hits
        return [target + e for e in JS_EXTS if target + e in self.fpath]

    def js_modres(self, site: dict, spec: str) -> ModRes:
        if "\x00" in spec or "\\" in spec:
            return ModRes("unresolved", note="hostile-specifier")
        importer = site["path"]
        idir = dirname(importer)
        if spec in (".", "..") or spec.startswith("./") or spec.startswith("../"):
            target = norm_join(idir, spec)
            if target is None:
                return ModRes("unresolved", note="specifier-escapes-snapshot")
            hits = self.js_probe(target)
            if not hits:
                return ModRes("unresolved", note=self.case_hint(target) or "no-file-in-snapshot")
            return self.file_res(hits)
        if spec.startswith("/") or WINDOWS_DRIVE_RE.match(spec):
            return ModRes("unresolved", note="absolute-path-specifier")
        if spec.startswith("node:") or spec.split("/")[0] in NODE_BUILTINS:
            return ModRes("external", ext=spec)
        if SCHEME_RE.match(spec) and not spec.startswith("@"):
            if spec.split(":", 1)[0] in ("http", "https", "data", "npm", "jsr", "bun", "deno"):
                return ModRes("external", ext=spec)
            return ModRes("unresolved", note="unsupported-url-scheme")
        if spec.startswith("#"):
            return self.js_imports_map(site, spec)
        # tsconfig/jsconfig paths + baseUrl
        results = self.js_tsconfig_hits(importer, spec)
        if results:
            distinct = {tuple(sorted(r)) for r in results}
            if len(distinct) == 1:
                return self.file_res(list(results[0]), note="tsconfig-paths")
            allp: list[str] = sorted({p for r in results for p in r})
            return ModRes("candidates", allp, note="tsconfig-paths-disagree")
        pkg = self.js_package(importer, spec)
        if pkg is not None:
            return pkg
        name = self.pkg_name(spec)
        if self.js_declared(importer, name):
            return ModRes("external", ext=name)
        return ModRes("unresolved", note="bare-specifier-not-found")

    @staticmethod
    def pkg_name(spec: str) -> str:
        parts = spec.split("/")
        if spec.startswith("@") and len(parts) >= 2:
            return parts[0] + "/" + parts[1]
        return parts[0]

    def js_declared(self, importer: str, name: str) -> bool:
        for d in ancestors(dirname(importer)):
            p = posixpath.join(d, "package.json")
            if p in self.fpath:
                data = self.read_json(p)
                if isinstance(data, dict):
                    for field in ("dependencies", "devDependencies", "peerDependencies", "optionalDependencies"):
                        deps = data.get(field)
                        if isinstance(deps, dict) and (name in deps or "@types/" + name.lstrip("@").replace("/", "__") in deps):
                            return True
        return False

    # -- tsconfig -------------------------------------------------------------
    def tsconfigs(self) -> list[dict]:
        if self._tsconfigs is not None:
            return self._tsconfigs
        cfgs: list[dict] = []
        names = [p for p in self.fpath if re.fullmatch(r"(ts|js)config(\.[^/]+)?\.json", basename(p))]
        if len(names) > MAX_CONFIG_FILES:
            self.limit(f"More than {MAX_CONFIG_FILES} tsconfig/jsconfig files; extras ignored for path mapping.")
            names = sorted(names)[:MAX_CONFIG_FILES]
        for p in sorted(names):
            cfg = self.load_tsconfig(p, set())
            if cfg is not None:
                cfgs.append(cfg)
        # references -> mark referenced configs (more specific than the referencing config)
        by_path = {c["path"]: c for c in cfgs}
        for c in cfgs:
            for ref in c["references"]:
                if ref in by_path:
                    by_path[ref]["referencedBy"].add(c["path"])
        self._tsconfigs = cfgs
        return cfgs

    def load_tsconfig(self, path: str, stack: set[str]) -> dict | None:
        data = self.read_json(path)
        if not isinstance(data, dict):
            return None
        cdir = dirname(path)
        cfg = {
            "path": path, "dir": cdir, "baseUrl": None, "paths": None, "pathsDir": cdir, "files": None, "include": None, "exclude": None,
            "references": [], "referencedBy": set(), "filesDir": cdir, "includeDir": cdir, "excludeDir": cdir,
        }
        ext = data.get("extends")
        parents = [ext] if isinstance(ext, str) else [e for e in (ext or []) if isinstance(e, str)]
        if path in stack:
            self.limit(f"tsconfig extends cycle at {path}.")
            parents = []
        for p in parents:
            if p.startswith(("./", "../")):
                tp = norm_join(cdir, p)
                cand = [tp, (tp + ".json") if tp is not None else None]
                parent = None
                for c in cand:
                    if c and c in self.fpath:
                        parent = self.load_tsconfig(c, stack | {path})
                        break
                if parent is None:
                    self.limit(f"tsconfig {path} extends {p}, which is not in the inventory.")
                    continue
                for k in ("baseUrl", "paths", "pathsDir", "files", "include", "exclude", "filesDir", "includeDir", "excludeDir"):
                    if parent[k] is not None:
                        cfg[k] = parent[k]
            else:
                self.limit(f"tsconfig {path} extends package/absolute configuration {p}; its path mapping was not read.")
        co = data.get("compilerOptions") if isinstance(data.get("compilerOptions"), dict) else {}
        if isinstance(co.get("baseUrl"), str):
            bu = norm_join(cdir, co["baseUrl"])
            if bu is not None:
                cfg["baseUrl"] = bu
        if isinstance(co.get("paths"), dict):
            cfg["paths"] = {k: [t for t in v if isinstance(t, str)] for k, v in co["paths"].items() if isinstance(v, list)}
            cfg["pathsDir"] = cdir
        if "rootDirs" in co:
            self.limit(f"tsconfig {path} uses rootDirs, which is not modelled.")
        for key in ("files", "include", "exclude"):
            v = data.get(key)
            if isinstance(v, list):
                cfg[key] = [x for x in v if isinstance(x, str)]
                cfg[key + "Dir"] = cdir
        refs = data.get("references")
        if isinstance(refs, list):
            for r in refs:
                if isinstance(r, dict) and isinstance(r.get("path"), str):
                    rp = norm_join(cdir, r["path"])
                    if rp is None:
                        continue
                    for cand in (rp, rp + ".json", posixpath.join(rp, "tsconfig.json")):
                        if cand in self.fpath:
                            cfg["references"].append(cand)
                            break
        return cfg

    def cfg_covers(self, cfg: dict, path: str) -> bool:
        if cfg["files"] is not None:
            for f in cfg["files"]:
                np = norm_join(cfg["filesDir"], f)
                if np == path:
                    return not self.cfg_excluded(cfg, path)
        include = cfg["include"]
        if include is None:
            if cfg["files"] is not None:
                return False
            include = ["**/*"]
            idir = cfg["dir"]
        else:
            idir = cfg["includeDir"]
        ok = False
        for pat in include:
            if self.glob_match_dir(idir, pat, path):
                ok = True
                break
        return ok and not self.cfg_excluded(cfg, path)

    def cfg_excluded(self, cfg: dict, path: str) -> bool:
        exclude = cfg["exclude"]
        if exclude is None:
            exclude = ["node_modules", "bower_components", "jspm_packages"]
        edir = cfg["excludeDir"] if cfg["exclude"] is not None else cfg["dir"]
        return any(self.glob_match_dir(edir, pat, path) for pat in exclude)

    @staticmethod
    def glob_match_dir(base: str, pattern: str, path: str) -> bool:
        pat = pattern[2:] if pattern.startswith("./") else pattern
        if pat.startswith("/") or ".." in pat.split("/") and not pat.startswith("../"):
            return False
        full = posixpath.normpath(posixpath.join(base, pat)) if base else posixpath.normpath(pat)
        if full == ".":
            full = ""
        if full.startswith("../"):
            return False
        if not any(ch in full for ch in "*?"):
            return path == full or path.startswith(full + "/") or full == ""
        rx = glob_regex(full)
        if rx.match(path):
            return True
        parts = path.split("/")
        for i in range(1, len(parts)):
            if rx.match("/".join(parts[:i])):
                return True
        return False

    def js_tsconfig_hits(self, importer: str, spec: str) -> list[list[str]]:
        if self._cfg_by_path is None:
            self._cfg_by_path = {c["path"]: c for c in self.tsconfigs()}
        by_path = self._cfg_by_path
        # tsserver semantics: the nearest tsconfig.json/jsconfig.json whose project (or a referenced project) covers the file
        for d in ancestors(dirname(importer)):
            roots = [by_path[p] for p in (posixpath.join(d, "tsconfig.json"), posixpath.join(d, "jsconfig.json")) if p in by_path]
            if not roots:
                continue
            covering: list[dict] = []
            seen: set[str] = set()
            stack = list(roots)
            while stack:
                c = stack.pop()
                if c["path"] in seen:
                    continue
                seen.add(c["path"])
                if self.cfg_covers(c, importer):
                    covering.append(c)
                stack.extend(by_path[r] for r in c["references"] if r in by_path)
            if not covering:
                continue
            referenced = [c for c in covering if c["path"] not in {r["path"] for r in roots}]
            top = referenced or covering
            results: list[list[str]] = []
            for c in top:
                hits = self.cfg_resolve(c, spec)
                if hits:
                    results.append(hits)
            return results
        return []

    def cfg_resolve(self, cfg: dict, spec: str) -> list[str]:
        paths = cfg["paths"]
        if paths:
            best = None
            for pat, targets in paths.items():
                if "*" in pat:
                    pre, _, suf = pat.partition("*")
                    if spec.startswith(pre) and spec.endswith(suf) and len(spec) >= len(pre) + len(suf):
                        if best is None or len(pre) > best[0]:
                            best = (len(pre), spec[len(pre) : len(spec) - len(suf)], targets)
                elif pat == spec:
                    best = (1 << 30, "", targets)
                    break
            if best is not None:
                for t in best[2]:
                    sub = t.replace("*", best[1], 1) if "*" in t else t
                    tp = norm_join(cfg["baseUrl"] if cfg["baseUrl"] is not None else cfg["pathsDir"], sub)
                    if tp is None:
                        continue
                    hits = self.js_probe(tp)
                    if hits:
                        return hits
        if cfg["baseUrl"] is not None:
            tp = norm_join(cfg["baseUrl"], spec)
            if tp is not None:
                hits = self.js_probe(tp)
                if hits:
                    return hits
        return []

    # -- packages / exports ---------------------------------------------------
    def pkg_index(self) -> dict[str, str]:
        if self._pkg_index is None:
            idx: dict[str, str] = {}
            dup: set[str] = set()
            paths = [p for p in self.fpath if basename(p) == "package.json"]
            if len(paths) > MAX_CONFIG_FILES:
                self.limit(f"More than {MAX_CONFIG_FILES} package.json files; extras ignored for workspace resolution.")
                paths = sorted(paths)[:MAX_CONFIG_FILES]
            for p in sorted(paths):
                data = self.read_json(p)
                if isinstance(data, dict) and isinstance(data.get("name"), str):
                    n = data["name"]
                    if n in idx:
                        dup.add(n)
                    idx[n] = dirname(p)
            for n in dup:
                idx.pop(n, None)
                self.limit(f"Package name {n} is declared by several package.json files; workspace resolution skipped it.")
            self._pkg_index = idx
        return self._pkg_index

    def js_package(self, importer: str, spec: str) -> ModRes | None:
        name = self.pkg_name(spec)
        pdir = self.pkg_index().get(name)
        if pdir is None:
            return None
        pkg_json_path = posixpath.join(pdir, "package.json")
        data = self.read_json(pkg_json_path)
        if not isinstance(data, dict):
            return None
        in_self = in_dir(importer, pdir)
        if not (self.js_declared(importer, name) or self.workspace_member(pdir) or (in_self and data.get("exports") is not None)):
            return None
        sub = "." + spec[len(name):]
        return self.pkg_entry(pdir, data, sub, spec)

    def workspace_member(self, pdir: str) -> bool:
        for d in ancestors(dirname(pdir) if pdir else ""):
            p = posixpath.join(d, "package.json")
            if p in self.fpath and d != pdir:
                data = self.read_json(p)
                ws = data.get("workspaces") if isinstance(data, dict) else None
                if isinstance(ws, dict):
                    ws = ws.get("packages")
                if isinstance(ws, list):
                    for pat in ws:
                        if isinstance(pat, str) and self.glob_match_dir(d, pat.rstrip("/"), pdir + "/x"):
                            return True
        return False

    def pkg_entry(self, pdir: str, data: dict, subpath: str, spec: str) -> ModRes:
        exports = data.get("exports")
        if exports is None:
            if subpath == ".":
                for field in ("types", "typings", "module", "main"):
                    v = data.get(field)
                    if isinstance(v, str):
                        t = norm_join(pdir, v)
                        if t is not None:
                            hits = self.js_probe_file_only(t) or self.js_probe(t)
                            if hits:
                                return self.file_res(hits, note="package-main")
                hits = self.js_probe(posixpath.join(pdir, "index"))
                if hits:
                    return self.file_res(hits, note="package-index")
                return ModRes("unresolved", note="package-entry-not-in-snapshot")
            t = norm_join(pdir, subpath[2:])
            if t is None:
                return ModRes("unresolved", note="specifier-escapes-package")
            hits = self.js_probe(t)
            return self.file_res(hits, note="package-subpath") if hits else ModRes("unresolved", note=self.case_hint(t) or "package-subpath-not-in-snapshot")
        return self.exports_res(pdir, exports, subpath)

    def exports_res(self, pdir: str, exports: Any, subpath: str) -> ModRes:
        if isinstance(exports, (str, list)) or (isinstance(exports, dict) and exports and not any(k.startswith((".", "#")) for k in exports)):
            exports = {".": exports}
        if not isinstance(exports, dict):
            return ModRes("unresolved", note="invalid-exports")
        target = None
        star = ""
        if subpath in exports:
            target = exports[subpath]
        else:
            best = None
            for k in exports:
                if "*" in k:
                    pre, _, suf = k.partition("*")
                    if subpath.startswith(pre) and subpath.endswith(suf) and len(subpath) >= len(pre) + len(suf):
                        if best is None or len(pre) > best[0]:
                            best = (len(pre), k, subpath[len(pre) : len(subpath) - len(suf)])
            if best is not None:
                target, star = exports[best[1]], best[2]
        if target is None:
            return ModRes("unresolved", note="subpath-not-exported")
        leaves: list[str] = []
        self.collect_leaves(target, leaves, 0)
        leaves = [l for l in leaves]
        found: list[str] = []
        for leaf in leaves:
            if not leaf.startswith("./"):
                continue
            tp = norm_join(pdir, (leaf.replace("*", star) if "*" in leaf else leaf)[2:] or ".")
            if tp is None:
                continue
            if tp in self.fpath:
                found.append(tp)
        if not found:
            return ModRes("unresolved", note="export-target-not-in-snapshot")
        distinct = sorted(set(found))
        multi_leaf = len([l for l in leaves if l.startswith("./")]) > 1
        if len(distinct) == 1 and (not multi_leaf or len(found) == len([l for l in leaves if l.startswith("./")])):
            return ModRes("file", distinct, note="package-exports")
        return ModRes("candidates", distinct, note="package-exports-conditions")

    def collect_leaves(self, node: Any, out: list[str], depth: int) -> None:
        if depth > 16:
            return
        if isinstance(node, str):
            out.append(node)
        elif isinstance(node, list):
            for x in node:
                self.collect_leaves(x, out, depth + 1)
        elif isinstance(node, dict):
            for v in node.values():
                self.collect_leaves(v, out, depth + 1)

    def js_imports_map(self, site: dict, spec: str) -> ModRes:
        for d in ancestors(dirname(site["path"])):
            p = posixpath.join(d, "package.json")
            if p in self.fpath:
                data = self.read_json(p)
                if isinstance(data, dict) and isinstance(data.get("imports"), dict):
                    imports = data["imports"]
                    res = self.exports_res(d, {k: v for k, v in imports.items()}, spec)
                    if res.kind != "unresolved":
                        return res
                    return res
        return ModRes("unresolved", note="package-imports-not-found")

    # ======================================================================
    # Python
    # ======================================================================
    def declared_py(self) -> set[str]:
        if self._py_deps is None:
            deps: set[str] = set()

            def norm(n: str) -> str:
                return re.sub(r"[-_.]+", "_", n).lower()

            for p in self.fpath:
                b = basename(p)
                if b == "pyproject.toml":
                    data = self.read_toml(p)
                    if isinstance(data, dict):
                        proj = data.get("project") or {}
                        for line in list(proj.get("dependencies") or []) + [x for v in (proj.get("optional-dependencies") or {}).values() for x in v]:
                            m = re.match(r"\s*([A-Za-z0-9_.-]+)", str(line))
                            if m:
                                deps.add(norm(m.group(1)))
                        poetry = ((data.get("tool") or {}).get("poetry") or {})
                        for k in list((poetry.get("dependencies") or {}).keys()) + list((poetry.get("dev-dependencies") or {}).keys()):
                            deps.add(norm(k))
                elif re.fullmatch(r"requirements[^/]*\.txt", b):
                    text = self.read_text(p)
                    for line in (text or "").splitlines():
                        m = re.match(r"\s*([A-Za-z0-9_.-]+)", line)
                        if m and not line.lstrip().startswith(("#", "-")):
                            deps.add(norm(m.group(1)))
            self._py_deps = deps
        return self._py_deps

    def py_find(self, parts: list[str], base_dir: str) -> tuple[ModRes | None, str | None]:
        """Find dotted module below ``base_dir``: package > module; returns (ModRes, root)."""
        if not parts:
            return None, None
        rel = "/".join(parts)
        t = norm_join(base_dir, rel)
        if t is None:
            return ModRes("unresolved", note="escapes-snapshot"), None
        init = posixpath.join(t, "__init__.py")
        if init in self.fpath:
            return ModRes("file", [init], dir=t, note="package"), base_dir
        if t + ".py" in self.fpath:
            return ModRes("file", [t + ".py"], note="module"), base_dir
        if t + ".pyi" in self.fpath:
            return ModRes("file", [t + ".pyi"], note="stub"), base_dir
        if t in self.all_dirs:
            return ModRes("dir", [], dir=t, note="namespace-package"), base_dir
        return None, None

    def py_modres(self, site: dict, spec: str) -> ModRes:
        if "\x00" in spec or "/" in spec or "\\" in spec:
            return ModRes("unresolved", note="hostile-specifier")
        level = len(spec) - len(spec.lstrip("."))
        rest = spec[level:]
        parts = [p for p in rest.split(".")] if rest else []
        if any(not p for p in parts):
            return ModRes("unresolved", note="malformed-module-name")
        importer = site["path"]
        idir = dirname(importer)
        if level:
            base = idir
            for _ in range(level - 1):
                if base == "":
                    return ModRes("unresolved", note="relative-import-beyond-root")
                base = dirname(base)
            if not parts:
                init = posixpath.join(base, "__init__.py")
                if init in self.fpath:
                    return ModRes("file", [init], dir=base, note="package")
                return ModRes("dir", [], dir=base, note="package-directory")
            res, _ = self.py_find(parts, base)
            return res or ModRes("unresolved", note=self.case_hint(posixpath.join(base, *parts)) or "relative-module-not-in-snapshot")
        # absolute: distinct source roots among the importer's ancestors where the first segment exists
        found: dict[tuple, ModRes] = {}
        for d in ancestors(idir):
            for base in (d, posixpath.join(d, "src") if d else "src"):
                res, root = self.py_find(parts, base)
                if res is not None and res.kind != "dir":
                    found[(res.kind, tuple(res.files), res.dir)] = res
        if len(found) == 1:
            res = next(iter(found.values()))
            return res
        if len(found) > 1:
            files = sorted({f for r in found.values() for f in r.files})
            return ModRes("candidates", files, note="several-source-roots")
        top = parts[0]
        if top in PY_STDLIB:
            return ModRes("external", ext=top, note="python-stdlib")
        # namespace package without __init__ (directory only)
        for d in ancestors(idir):
            res, _ = self.py_find(parts, d)
            if res is not None and res.kind == "dir":
                return res
        if re.sub(r"[-_.]+", "_", top).lower() in self.declared_py():
            return ModRes("external", ext=top, note="declared-dependency")
        return ModRes("unresolved", note="module-not-in-snapshot")

    def py_module_ents(self, site: dict) -> set:
        """Entities of the imported *module* (not a name inside it)."""
        mr = self.modres(site)
        if mr.kind in ("file", "candidates"):
            return {("file", self.fid_of(p)) for p in mr.files}
        if mr.kind == "dir":
            return {("dir", mr.dir or "")}
        if mr.kind == "external":
            return {("ext", mr.ext or "")}
        return {("unres", mr.note or mr.kind)}

    def _py_import_ents(self, site: dict, b: dict, mr: ModRes) -> set:
        spec = b.get("specifier") or ""
        imported = b.get("imported")
        mods = self.py_module_ents(site)
        if imported is None or imported == "":
            local = b.get("local") or b.get("alias")
            bare = spec.lstrip(".")
            head = bare.split(".")[0]
            # `import a.b.c` binds the top package `a`; `import a.b.c as x` (or `import os`) binds the module itself
            if local and not b.get("alias") and local == head and "." in bare and spec == bare:
                res, _root = self.py_find([head], self._py_root_for(site, spec))
                if res is not None:
                    return self.py_module_ents_from(res)
            return mods
        out: set = set()
        for ent in mods:
            if ent[0] == "file":
                hit = self.ns_get(ent[1], imported)
                if hit:
                    out |= hit
                    continue
            if ent[0] in ("file", "dir"):
                base = self.fid[ent[1]]["path"] if ent[0] == "file" else ent[1]
                pdir = dirname(base) if ent[0] == "file" else base
                if ent[0] == "file" and basename(base) not in ("__init__.py", "__init__.pyi"):
                    out.add(("unres", f"name-not-found:{imported}"))
                    continue
                sub, _ = self.py_find([imported], pdir)
                if sub is not None:
                    out |= self.py_module_ents_from(sub)
                else:
                    out.add(("unres", f"name-not-found:{imported}"))
            else:
                out.add(ent)
        return out

    def _py_root_for(self, site: dict, spec: str) -> str:
        parts = spec.split(".")
        for d in ancestors(dirname(site["path"])):
            for base in (d, posixpath.join(d, "src") if d else "src"):
                res, _ = self.py_find(parts, base)
                if res is not None and res.kind != "dir":
                    return base
        return ""

    def py_module_ents_from(self, res: ModRes) -> set:
        if res.kind == "file":
            return {("file", self.fid_of(p)) for p in res.files}
        if res.kind == "dir":
            return {("dir", res.dir or "")}
        return {("unres", res.note or res.kind)}

    # ======================================================================
    # C / C++
    # ======================================================================
    def c_modres(self, site: dict, spec: str) -> ModRes:
        if "\x00" in spec or "\\" in spec:
            return ModRes("unresolved", note="hostile-specifier")
        b = site.get("binding") or {}
        idir = dirname(site["path"])
        system = bool(b.get("system"))
        direct = norm_join(idir, spec) if not spec.startswith("/") else None
        if spec.startswith("/") or WINDOWS_DRIVE_RE.match(spec):
            return ModRes("unresolved", note="absolute-include")
        if ".." in spec.split("/") and direct is None:
            return ModRes("unresolved", note="include-escapes-snapshot")
        if not system and direct is not None and direct in self.fpath:
            return ModRes("file", [direct], note="quoted-include-relative")
        # include search paths are unknown (no build discovery): exact literal path suffix matches are candidates only
        norm = norm_join("", spec)
        matches: list[str] = []
        if norm:
            for p in self.by_basename.get(basename(norm), ()):
                if p == norm or p.endswith("/" + norm):
                    matches.append(p)
        if matches:
            return ModRes("candidates", sorted(matches)[:MAX_CANDIDATES], note="include-path-unknown")
        if system:
            return ModRes("external", ext=spec, note="system-include")
        if direct is not None and (case := self.case_hint(direct)):
            return ModRes("unresolved", note=case)
        return ModRes("unresolved", note="include-not-in-snapshot")

    # ======================================================================
    # Go
    # ======================================================================
    def go_init(self) -> None:
        if self._go_ready:
            return
        self._go_ready = True
        self.go_mods: list[tuple[str, str]] = []  # (module path, dir)
        self.go_replace: list[tuple[str, str]] = []
        self.go_requires: set[str] = set()
        for p in sorted(self.fpath):
            if basename(p) == "go.mod":
                text = self.read_text(p)
                if text is None:
                    continue
                d = dirname(p)
                m = re.search(r"^\s*module\s+(\S+)", text, re.M)
                if m:
                    self.go_mods.append((m.group(1).strip('"'), d))
                for r in re.finditer(r"^\s*(?:require\s+)?([A-Za-z0-9_.\-]+\.[A-Za-z0-9_.\-/]+)\s+v[0-9]", text, re.M):
                    self.go_requires.add(r.group(1))
                for r in re.finditer(r"^\s*(?:replace\s+)?(\S+)(?:\s+v\S+)?\s*=>\s*(\.{1,2}/\S*)", text, re.M):
                    tp = norm_join(d, r.group(2))
                    if tp is not None:
                        self.go_replace.append((r.group(1), tp))
        self.go_mods.sort(key=lambda t: -len(t[0]))
        self.go_replace.sort(key=lambda t: -len(t[0]))

    def go_pkg_files(self, directory: str) -> list[str]:
        return sorted(p for p in self.dir_files.get(directory, ()) if p.endswith(".go") and not p.endswith("_test.go"))

    def go_modres(self, site: dict, spec: str) -> ModRes:
        self.go_init()
        if "\x00" in spec or "\\" in spec or ".." in spec.split("/"):
            return ModRes("unresolved", note="hostile-specifier")
        for mod, d in self.go_mods:
            if spec == mod or spec.startswith(mod + "/"):
                sub = spec[len(mod) :].lstrip("/")
                target = norm_join(d, sub) if sub else d
                if target is None:
                    return ModRes("unresolved", note="specifier-escapes-snapshot")
                files = self.go_pkg_files(target)
                if not files:
                    return ModRes("unresolved", note="package-directory-not-in-snapshot")
                return ModRes("dir", files, dir=target, note="go-package")
        for mod, d in self.go_replace:
            if spec == mod or spec.startswith(mod + "/"):
                sub = spec[len(mod) :].lstrip("/")
                target = norm_join(d, sub) if sub else d
                files = self.go_pkg_files(target) if target is not None else []
                if files:
                    return ModRes("dir", files, dir=target, note="go-replace")
        first = spec.split("/")[0]
        if "." not in first:
            return ModRes("external", ext=spec, note="go-standard-library")
        if any(spec == r or spec.startswith(r + "/") for r in self.go_requires):
            return ModRes("external", ext=spec, note="go-module-dependency")
        return ModRes("unresolved", note="package-not-in-snapshot")

    # ======================================================================
    # Rust
    # ======================================================================
    def rust_init(self) -> None:
        if self._rust_ready:
            return
        self._rust_ready = True
        self.crates: list[dict] = []
        self.rust_roots: dict[str, dict] = {}
        ws_deps: dict[str, dict] = {}
        manifests = sorted(p for p in self.fpath if basename(p) == "Cargo.toml")
        datas = {p: self.read_toml(p) for p in manifests}
        for p, data in datas.items():
            if isinstance(data, dict):
                wd = ((data.get("workspace") or {}).get("dependencies") or {})
                if isinstance(wd, dict):
                    ws_deps[dirname(p)] = wd
        for p, data in datas.items():
            if not isinstance(data, dict):
                continue
            pkg = data.get("package")
            if not isinstance(pkg, dict) or not isinstance(pkg.get("name"), str):
                continue
            d = dirname(p)
            name = pkg["name"].replace("-", "_")
            crate = {"name": name, "pkg": pkg["name"], "dir": d, "manifest": p, "lib": None, "roots": [], "deps": {}}
            lib = data.get("lib") if isinstance(data.get("lib"), dict) else {}
            lp = norm_join(d, lib.get("path") or "src/lib.rs")
            if lib.get("name"):
                crate["name"] = str(lib["name"]).replace("-", "_")
            if lp and lp in self.fpath:
                crate["lib"] = lp
                crate["roots"].append(lp)
            bins = data.get("bin") if isinstance(data.get("bin"), list) else []
            for bn in bins:
                if isinstance(bn, dict) and isinstance(bn.get("path"), str):
                    bp = norm_join(d, bn["path"])
                    if bp and bp in self.fpath:
                        crate["roots"].append(bp)
            main = norm_join(d, "src/main.rs")
            if main in self.fpath:
                crate["roots"].append(main)
            for q in self.dir_files.get(posixpath.join(d, "src/bin") if d else "src/bin", ()):
                if q.endswith(".rs"):
                    crate["roots"].append(q)
            for sub in ("tests", "benches", "examples"):
                for q in self.dir_files.get(posixpath.join(d, sub) if d else sub, ()):
                    if q.endswith(".rs"):
                        crate["roots"].append(q)
            bs = norm_join(d, "build.rs")
            if bs in self.fpath:
                crate["roots"].append(bs)
            for table in ("dependencies", "dev-dependencies", "build-dependencies"):
                crate["deps"].update(self._rust_deps(data.get(table), d, ws_deps))
            for tv in (data.get("target") or {}).values() if isinstance(data.get("target"), dict) else ():
                if isinstance(tv, dict):
                    for table in ("dependencies", "dev-dependencies", "build-dependencies"):
                        crate["deps"].update(self._rust_deps(tv.get(table), d, ws_deps))
            crate["roots"] = sorted(set(crate["roots"]))
            self.crates.append(crate)
        for c in self.crates:
            for r in c["roots"]:
                self.rust_roots.setdefault(r, c)
        by_name: dict[str, list[dict]] = defaultdict(list)
        for c in self.crates:
            if c["lib"]:
                by_name[c["name"]].append(c)
        self.crate_by_name = {n: cs[0] for n, cs in by_name.items() if len(cs) == 1}
        self.crate_by_pkg = {c["pkg"]: c for c in self.crates if c["lib"] and list(by_name[c["name"]]) == [c]}
        # file -> roots (reachability through `mod name;` edges)
        self.rust_file_roots: dict[str, set[str]] = defaultdict(set)
        self.rust_parent: dict[str, set[str]] = defaultdict(set)  # file module symbol -> containing module keys
        self.rust_mod_child: dict[str, str | None] = {}
        mod_sites = [s for s in self.sites if s["language"] == "rust" and (s.get("binding") or {}).get("kind") == "mod"]
        by_caller: dict[str, list[dict]] = defaultdict(list)
        for s in mod_sites:
            child = self.rust_mod_target(s)
            self.rust_mod_child[s["id"]] = child
        for s in mod_sites:
            child = self.rust_mod_child[s["id"]]
            if child:
                m = self.file_modsym.get(self.fid_of(child))
                scope = self.site_scope(s)
                if m and scope:
                    self.rust_parent[m["id"]].add(scope)
        files_by_parent: dict[str, list[str]] = defaultdict(list)
        for s in mod_sites:
            child = self.rust_mod_child[s["id"]]
            if child:
                files_by_parent[s["path"]].append(child)
        for root in self.rust_roots:
            q = deque([root])
            seen = {root}
            while q:
                f = q.popleft()
                self.rust_file_roots[f].add(root)
                for ch in files_by_parent.get(f, ()):
                    if ch not in seen:
                        seen.add(ch)
                        q.append(ch)

    def _rust_deps(self, table: Any, cdir: str, ws_deps: dict[str, dict]) -> dict[str, dict | None]:
        out: dict[str, dict | None] = {}
        if not isinstance(table, dict):
            return out
        for key, val in table.items():
            real = key
            path = None
            if isinstance(val, dict):
                if val.get("workspace") is True:
                    for d in ancestors(cdir):
                        wd = ws_deps.get(d)
                        if wd is not None and key in wd:
                            val = wd[key]
                            if isinstance(val, dict) and isinstance(val.get("path"), str):
                                path = norm_join(d, val["path"])
                            break
                if isinstance(val, dict):
                    real = val.get("package", key)
                    if path is None and isinstance(val.get("path"), str):
                        path = norm_join(cdir, val["path"])
            out[key.replace("-", "_")] = {"pkg": real, "path": path}
        return out

    def rust_mod_target(self, site: dict) -> str | None:
        b = site["binding"]
        name = b.get("specifier")
        if not name or "/" in name or "\\" in name or ".." in name:
            return None
        path = site["path"]
        chain = []
        sid = site.get("callerId")
        hops = 0
        while sid and hops < 1000:
            hops += 1
            s = self.sym.get(sid)
            if s is None or (s["kind"] == "module" and not s.get("ownerId")):
                break
            if self.is_rmod(s) and s.get("name"):
                chain.append(s["name"])
            sid = s.get("ownerId")
        chain.reverse()
        d = dirname(path)
        stem_dir = d
        if basename(path) not in ("mod.rs", "lib.rs", "main.rs") and path not in getattr(self, "rust_roots", {}):
            stem_dir = posixpath.join(d, basename(path)[:-3]) if d else basename(path)[:-3]
        base = stem_dir
        for part in chain:
            base = posixpath.join(base, part) if base else part
        attr = b.get("path")
        if isinstance(attr, str) and attr:
            t = norm_join(d if not chain else base, attr)
            return t if t in self.fpath else None
        a = norm_join(base, name + ".rs")
        c = norm_join(base, posixpath.join(name, "mod.rs"))
        hits = [x for x in (a, c) if x and x in self.fpath]
        return hits[0] if len(hits) == 1 else None

    def rust_modres(self, site: dict, spec: str) -> ModRes:
        self.rust_init()
        b = site["binding"]
        if b.get("kind") == "mod":
            t = self.rust_mod_child.get(site["id"])
            if t:
                return ModRes("file", [t], note="rust-mod")
            return ModRes("unresolved", note="module-file-not-in-snapshot")
        ents = self.rust_use_ents(site)
        files = sorted({self.fid[e[1]]["path"] for e in ents if e[0] == "file"} | {self.sym[e[1]]["path"] for e in ents if e[0] == "sym"})
        if any(e[0] == "ext" for e in ents) and not files:
            return ModRes("external", ext=spec)
        if files:
            return self.file_res(files, certain=len(files) == 1)
        return ModRes("unresolved", note="use-path-not-resolved")

    def rust_self_key(self, site_or_sym_scope: str | None) -> str | None:
        sid = site_or_sym_scope
        hops = 0
        while sid and hops < 100000:
            hops += 1
            s = self.sym.get(sid)
            if s is None:
                return None
            if self.is_rmod(s):
                return s["id"]
            sid = s.get("ownerId")
        return None

    def rust_file_of_key(self, key: str) -> str:
        return self.sym[key]["path"]

    def rust_crate_roots(self, key: str) -> list[str]:
        sym = self.sym[key]
        while sym.get("ownerId") and self.sym.get(sym["ownerId"], {}).get("fileId") == sym["fileId"]:
            sym = self.sym[sym["ownerId"]]
        fpath = sym["path"]
        return sorted(self.rust_file_roots.get(fpath, ()))

    def rust_current_crate(self, key: str) -> dict | None:
        roots = self.rust_crate_roots(key)
        if roots:
            return self.rust_roots.get(roots[0])
        d = self.sym[key]["path"]
        best = None
        for c in self.crates:
            if in_dir(d, c["dir"]) and (best is None or len(c["dir"]) > len(best["dir"])):
                best = c
        return best

    def rust_ns_key(self, ent: tuple) -> str | None:
        if ent[0] == "file":
            m = self.file_modsym.get(ent[1])
            return m["id"] if m else None
        if ent[0] == "sym" and self.is_rmod(self.sym[ent[1]]):
            return ent[1]
        return None

    def rust_lookup_first(self, key: str, seg: str, scope: str | None = None) -> set:
        hit = self.ns_get(key, seg)
        if hit:
            return set(hit)
        crate = self.rust_current_crate(key)
        if seg in RUST_STD_CRATES:
            return {("ext", seg)}
        if crate is not None:
            if seg == crate["name"] and crate["lib"]:
                return {("file", self.fid_of(crate["lib"]))}
            dep = crate["deps"].get(seg)
            if dep is not None:
                target = None
                if dep.get("path"):
                    for c in self.crates:
                        if c["dir"] == dep["path"] and c["lib"]:
                            target = c
                if target is None:
                    target = self.crate_by_pkg.get(dep["pkg"])
                if target is not None and target["lib"]:
                    return {("file", self.fid_of(target["lib"]))}
                return {("ext", seg)}
        return set()

    def rust_path(self, key: str, segs: list[str], *, impl_scope: str | None = None) -> set:
        """Resolve a (possibly module-qualified) path starting from module ``key``."""
        if not segs:
            return set()
        ents: set
        first = segs[0]
        rest = segs[1:]
        if first == "":
            if not rest:
                return set()
            ents = self.rust_lookup_first(key, rest[0])
            rest = rest[1:]
            if not ents:
                return {("ext", segs[1])}
        elif first == "crate":
            ents = {("file", self.fid_of(r)) for r in self.rust_crate_roots(key)}
            if not ents:
                return {("unres", "crate-root-unknown")}
        elif first == "self":
            ents = {("sym", key)}
        elif first == "super":
            cur = {key}
            segs2 = list(segs)
            while segs2 and segs2[0] == "super":
                nxt: set = set()
                for k in cur:
                    s = self.sym[k]
                    if s.get("ownerId") and self.sym.get(s["ownerId"], {}).get("fileId") == s["fileId"]:
                        nxt.add(s["ownerId"])
                    else:
                        nxt |= self.rust_parent.get(k, set())
                cur = nxt
                segs2.pop(0)
            if not cur:
                return {("unres", "super-module-unknown")}
            ents = {("sym", k) for k in cur}
            rest = segs2
        elif first == "Self":
            ents = self.rust_self_type(impl_scope)
            if not ents:
                return {("unres", "Self-type-unknown")}
        else:
            ents = self.rust_lookup_first(key, first)
            if not ents:
                return {("ext", first)} if first in RUST_PRELUDE_FUNCS else set()
        for seg in rest:
            nxt = set()
            for e in ents:
                nxt |= self.rust_member(e, seg)
            ents = nxt
            if not ents:
                return {("unres", f"path-segment-not-found:{seg}")}
        return ents

    def rust_member(self, ent: tuple, name: str) -> set:
        if ent[0] == "ext":
            return {ent}
        nk = self.rust_ns_key(ent)
        if nk is not None:
            hit = self.ns_get(nk, name)
            return set(hit) if hit else set()
        if ent[0] == "sym":
            s = self.sym[ent[1]]
            if s["kind"] in TYPE_KINDS:
                return self.rust_assoc(s, name)
        return set()

    def rust_self_type(self, scope_id: str | None) -> set:
        sid = scope_id
        hops = 0
        while sid and hops < 10000:
            hops += 1
            s = self.sym.get(sid)
            if s is None:
                break
            if s["kind"] == "impl":
                return self.rust_impl_type(s)
            if s["kind"] in ("trait", "struct", "enum", "class"):
                return {("sym", s["id"])}
            sid = s.get("ownerId")
        return set()

    @staticmethod
    def _rust_type_segments(raw: str) -> list[str]:
        t = re.sub(r"<[^<>]*>", "", raw)
        for _ in range(4):
            t = re.sub(r"<[^<>]*>", "", t)
        t = re.sub(r"^[&*\s]+", "", t)
        t = re.sub(r"^(mut|dyn|const)\s+", "", t).strip()
        return [p for p in re.split(r"::", t) if p] if t else []

    def rust_impl_type(self, impl: dict) -> set:
        cache = self._impl_types
        if impl["id"] in cache:
            return cache[impl["id"]]
        raw = (impl.get("bindings") or {}).get("implFor")
        out: set = set()
        if isinstance(raw, str):
            segs = self._rust_type_segments(raw)
            key = self.rust_self_key(impl["id"])
            if segs and key:
                out = {e for e in self.rust_path(key, segs) if e[0] == "sym"}
        cache[impl["id"]] = out
        return out

    def rust_impls_for(self, type_sym: dict) -> list[dict]:
        if self._impls_by_type is None:
            idx: dict[str, list[dict]] = defaultdict(list)
            self._impls_by_type = idx  # visible while being filled: path resolution may re-enter
            for s in self.symbols:
                if s["kind"] == "impl":
                    for e in self.rust_impl_type(s):
                        idx[e[1]].append(s)
        return self._impls_by_type.get(type_sym["id"], [])

    def rust_assoc(self, type_sym: dict, name: str) -> set:
        inherent: set = set()
        traits: set = set()
        for impl in self.rust_impls_for(type_sym):
            is_trait = bool((impl.get("bindings") or {}).get("implTrait"))
            for ch in self.children.get(impl["id"], ()):
                if ch.get("name") == name and ch["kind"] in CALLABLE_KINDS:
                    (traits if is_trait else inherent).add(("sym", ch["id"]))
        if type_sym["kind"] == "trait":
            for ch in self.children.get(type_sym["id"], ()):
                if ch.get("name") == name and (ch["kind"] in CALLABLE_KINDS or ch["kind"] == "signature"):
                    traits.add(("sym", ch["id"]))
        if len(inherent) == 1 and not traits:
            return inherent
        if inherent and not traits:
            return {("multi",) + tuple(sorted(e[1] for e in inherent))}
        allents = inherent | traits
        return {("multi",) + tuple(sorted(e[1] for e in allents))} if allents else set()

    _impl_types: dict[str, set] = {}
    _impls_by_type: dict | None = None

    @staticmethod
    def rust_use_segments(b: dict) -> list[str]:
        """`specifier` is the full leaf path; tolerate parsers that give the module path plus `imported`."""
        spec = b["specifier"]
        imported = b.get("imported")
        if imported and not (spec == imported or spec.endswith("::" + imported)):
            spec = spec + "::" + imported
        return spec.split("::")

    def rust_use_ents(self, site: dict) -> frozenset:
        self.rust_init()
        b = site["binding"]
        spec = b.get("specifier")
        if not spec:
            return frozenset()
        key = self.rust_self_key(self.site_scope(site))
        if key is None:
            return frozenset({("unres", "module-scope-unknown")})
        segs = self.rust_use_segments(b)
        ents = self.rust_path(key, segs)
        if not ents:
            return frozenset({("unres", "use-path-not-resolved")})
        return frozenset(ents)

    def _rust_round(self, mods: list[dict], uses: list[dict]) -> bool:
        self.rust_init()
        changed = False
        for s in mods:
            child = self.rust_mod_child.get(s["id"])
            key = self.rust_self_key(self.site_scope(s))
            name = s["binding"].get("specifier")
            if child and key and name:
                changed |= self._add(self.exp, key, name, {("file", self.fid_of(child))})
        for s in uses:
            b = s["binding"]
            key = self.rust_self_key(self.site_scope(s))
            holder = self.sym.get(self.site_scope(s) or "")
            if holder is not None and not self.is_rmod(holder):
                continue  # function-local `use` is resolved lexically, not added to the module namespace
            if key is None or not b.get("specifier"):
                continue
            segs = self.rust_use_segments(b)
            ents = self.rust_path(key, segs)
            ents = {e for e in ents if e[0] in ("sym", "file", "ext", "multi")}
            if not ents:
                continue
            if b.get("namespace"):
                for e in ents:
                    nk = self.rust_ns_key(e)
                    if nk is None:
                        continue
                    for n in self.ns_names(nk):
                        changed |= self._add(self.glob, key, n, self.ns_get(nk, n) or ())
            else:
                name = b.get("alias") or b.get("local") or segs[-1]
                if name == "self" and len(segs) > 1:
                    name = segs[-2]
                if name == "_":
                    continue
                changed |= self._add(self.exp, key, name, ents)
        return changed

    # ======================================================================
    # Generic (other languages)
    # ======================================================================
    def pkg_members(self) -> dict[str, set[str]]:
        """Normalized package/namespace -> files declaring a top-level fqn symbol directly inside it."""
        if self._pkg_members is None:
            out: dict[str, set[str]] = defaultdict(set)
            for f, syms in self.fqn_index().items():
                norm = re.sub(r"::|\\|/", ".", f)
                if "." in norm:
                    out[norm.rsplit(".", 1)[0]].update(s["path"] for s in syms)
            self._pkg_members = out
        return self._pkg_members

    def fqn_index(self) -> dict[str, list[dict]]:
        if getattr(self, "_fqn", None) is None:
            idx: dict[str, list[dict]] = defaultdict(list)
            for s in self.symbols:
                f = (s.get("bindings") or {}).get("fqn")
                if isinstance(f, str) and f:
                    idx[f].append(s)
            self._fqn = idx
        return self._fqn

    _fqn: dict | None = None

    @staticmethod
    def _split_q(spec: str) -> list[str]:
        return [p for p in re.split(r"::|\\|\.|/", spec) if p]

    PATH_ONLY_LANGS = frozenset({"ruby", "lua", "bash", "powershell", "zig"})
    PATH_KINDS = frozenset({"require", "source", "include"})
    FQN_LANGS = frozenset({"java", "kotlin", "scala", "csharp", "php", "elixir", "julia", "verilog", "fortran", "groovy"})

    def generic_modres(self, site: dict, spec: str) -> ModRes:
        lang = site["language"]
        b = site.get("binding") or {}
        if "\x00" in spec:
            return ModRes("unresolved", note="hostile-specifier")
        kind = b.get("kind")
        if lang in self.PATH_ONLY_LANGS or (lang in ("php", "julia") and (kind in self.PATH_KINDS or site["relation"] == "includes")):
            return self.path_literal_modres(site, spec, lang, b)
        if lang in self.FQN_LANGS:
            return self.fqn_modres(site, spec, lang, b)
        return ModRes("unsupported", note=f"{lang}-module-resolution-not-implemented")

    def path_literal_modres(self, site: dict, spec: str, lang: str, b: dict) -> ModRes:
        idir = dirname(site["path"])
        rel_marker = b.get("relativeTo") in ("dir", "__DIR__", "__dirname", "$PSScriptRoot", "script")
        if rel_marker and spec.startswith("/") and not spec.startswith("//"):
            spec = spec[1:]  # `__DIR__ . '/x.php'`: the parser keeps the literal tail
        if spec.startswith("/") or WINDOWS_DRIVE_RE.match(spec) or "\\" in spec:
            return ModRes("unresolved", note="absolute-or-hostile-path")
        if lang == "zig" and (spec in ("std", "builtin", "root") or ("/" not in spec and not spec.endswith((".zig", ".zon")))):
            return ModRes("external", ext=spec, note="zig-module")
        exts = {"ruby": ("", ".rb"), "lua": ("", ".lua", "/init.lua", ".luau"), "bash": ("",), "powershell": ("", ".ps1", ".psm1"),
                "php": ("",), "julia": ("", ".jl"), "zig": ("",)}[lang]
        spec_path = spec.replace(".", "/") if lang == "lua" and "/" not in spec and not spec.endswith((".lua", ".luau")) else spec
        rel_to_dir = rel_marker or lang in ("zig", "julia")
        bases = [idir] if rel_to_dir else list(dict.fromkeys([idir, ""] + (["lib"] if lang == "ruby" else [])))
        hits: list[str] = []
        for base in bases:
            t = norm_join(base, spec_path)
            if t is None:
                if base == idir:
                    return ModRes("unresolved", note="specifier-escapes-snapshot")
                continue
            for e in exts:
                cand = t + e
                if cand in self.fpath:
                    hits.append(cand)
        hits = sorted(set(hits))
        if not hits:
            first = norm_join(idir, spec_path) or spec
            hint = self.case_hint(first)
            if hint:
                return ModRes("unresolved", note=hint)
            if lang in ("ruby", "lua") and not rel_to_dir and lang not in self.lang_incomplete:
                return ModRes("external", ext=spec, note="not-in-snapshot")
            return ModRes("unresolved", note="file-not-in-snapshot")
        if rel_to_dir and len(hits) == 1:
            return ModRes("file", hits, note="relative-to-source-file")
        return ModRes("candidates", hits, note="search-path-unknown")

    def fqn_modres(self, site: dict, spec: str, lang: str, b: dict) -> ModRes:
        idx = self.fqn_index()
        wildcard = bool(b.get("namespace"))
        sep_spec = spec
        # normalize separators to what the parser stores in fqn (identity match first)
        cands = self.fqn_lookup(idx, sep_spec, lang)
        if wildcard:
            files: set[str] = set()
            norm_spec = re.sub(r"::|\\|/", ".", sep_spec)
            files.update(self.pkg_members().get(norm_spec.lower() if lang == "fortran" else norm_spec, ()))
            files = sorted(files)
            if files:
                return ModRes("candidates", files[:MAX_CANDIDATES], note="wildcard-import" + ("-truncated" if len(files) > MAX_CANDIDATES else ""))
            cands = cands or []
        if cands:
            files = sorted({s["path"] for s in cands})
            return self.file_res(files, certain=len(cands) == 1, note="fqn-match")
        # Java static import: class fqn + member
        parts = self._split_q(sep_spec)
        if len(parts) > 1:
            head = re.sub(r"[.:\\/]+[^.:\\/]+$", "", sep_spec)
            cls = self.fqn_lookup(idx, head, lang)
            if cls:
                files = sorted({s["path"] for s in cls})
                return self.file_res(files, certain=len(cls) == 1, note="static-member-import")
        if lang in self.lang_incomplete or any(f.get("language") == lang and f.get("status") != "parsed" for f in self.files):
            return ModRes("unresolved", note="not-found-and-some-source-files-unparsed")
        return ModRes("external", ext=spec, note="not-in-snapshot")

    def fqn_lookup(self, idx: dict, spec: str, lang: str) -> list[dict]:
        if lang == "fortran":
            spec = spec.lower()
        out = list(idx.get(spec, ()))
        if not out:
            for sep in (".", "::", "\\"):
                alt = re.sub(r"\.|::|\\", lambda _m, sep=sep: sep, spec)
                if alt != spec and alt in idx:
                    out = list(idx[alt])
                    break
        return out

    def _generic_import_ents(self, site: dict, b: dict, mr: ModRes) -> set:
        ents: set = set()
        if mr.kind not in ("file", "candidates"):
            return ents
        spec = b.get("specifier") or ""
        idx = self.fqn_index()
        lang = site["language"]
        cands = self.fqn_lookup(idx, spec, lang) if not b.get("namespace") else []
        if cands:
            return {("sym", s["id"]) for s in cands}
        if mr.note == "static-member-import":
            head = re.sub(r"[.:\\/]+[^.:\\/]+$", "", spec)
            name = self._split_q(spec)[-1]
            for c in self.fqn_lookup(idx, head, lang):
                for ch in self.children.get(c["id"], ()):
                    if ch.get("name") == name and ch["kind"] in CALLABLE_KINDS:
                        ents.add(("sym", ch["id"]))
            if ents:
                return ents
        for p in mr.files:
            ents.add(("file", self.fid_of(p)))
        return ents

    # ======================================================================
    # Site resolution
    # ======================================================================
    def enclosing_class(self, scope_id: str | None) -> dict | None:
        sid = scope_id
        hops = 0
        while sid and hops < 100000:
            hops += 1
            s = self.sym.get(sid)
            if s is None:
                return None
            if s["kind"] in ("class", "struct", "enum", "interface", "trait", "impl"):
                return s
            sid = s.get("ownerId")
        return None

    def class_members(self, cls: dict, name: str, *, with_inherited: bool, lang: str, seen: set | None = None) -> list[dict]:
        out: list[dict] = []
        seen = seen or set()
        if cls["id"] in seen:
            return out
        seen.add(cls["id"])
        for ch in self.children.get(cls["id"], ()):
            if ch.get("name") == name and ch["kind"] in CALLABLE_KINDS | {"signature"}:
                out.append(ch)
        if out and not with_inherited:
            return out
        if not out and with_inherited:
            for base in self.class_bases(cls):
                out.extend(self.class_members(base, name, with_inherited=True, lang=lang, seen=seen))
                if out:
                    break
        return out

    def class_bases(self, cls: dict) -> list[dict]:
        b = cls.get("bindings") or {}
        names = list(b.get("extends") or []) + list(b.get("implements") or [])
        out: list[dict] = []
        for raw in names:
            if not isinstance(raw, str):
                continue
            segs = [x for x in re.split(r"\.|::", re.sub(r"<.*>", "", raw)) if x]
            if not segs:
                continue
            lex = self.lex_lookup(cls.get("ownerId"), segs[0], cls["span"]["byteStart"], cls["language"], cls["fileId"])
            ents = set(lex.ents) if lex.kind == "found" else set()
            for seg in segs[1:]:
                nxt: set = set()
                for e in ents:
                    if e[0] == "file":
                        nxt |= self.ns_get(e[1], seg) or set()
                ents = nxt
            for e in ents:
                if e[0] == "sym" and e[1] in self.sym and self.sym[e[1]]["kind"] in TYPE_KINDS:
                    out.append(self.sym[e[1]])
        return out

    @staticmethod
    def is_ctor(sym: dict) -> bool:
        return sym["kind"] == "constructor" or (sym["kind"] in ("method", "function") and sym.get("name") in ("__init__",))

    def class_target(self, cls: dict, site: dict) -> tuple[str, list[str], str | None]:
        ctors = [c for c in self.children.get(cls["id"], ()) if self.is_ctor(c) and not self.is_signature(c)]
        if not ctors:
            return RESOLVED, [cls["id"]], "implicit-constructor"
        if len(ctors) == 1:
            return RESOLVED, [ctors[0]["id"]], None
        return CANDIDATE, [c["id"] for c in ctors], "overloaded-constructors"

    def result_from_ents(self, site: dict, ents: set, conflict: bool, note: str | None = None) -> tuple[str, str | None, list[str], str | None]:
        rel = site["relation"]
        syms: list[dict] = []
        multi: list[str] = []
        files = []
        ext = []
        unres = []
        val = False
        for e in sorted(ents, key=repr):
            k = e[0]
            if k == "sym":
                syms.append(self.sym[e[1]])
            elif k == "multi":
                multi.extend(e[1:])
            elif k == "file":
                files.append(e[1])
            elif k == "ext":
                ext.append(e[1])
            elif k == "unres":
                unres.append(e[1])
            elif k == "val":
                val = True
        if multi:
            conflict = True
            syms.extend(self.sym[i] for i in multi if i in self.sym)
        if syms:
            impl_keys = {(s.get("ownerId"), s.get("name")) for s in syms if not self.is_signature(s)}
            syms = [s for s in syms if not (self.is_signature(s) and (s.get("ownerId"), s.get("name")) in impl_keys)]
            callable_rel = rel in ("calls", "instantiates")
            targets: list[str] = []
            status = RESOLVED
            tnote = note
            for s in syms:
                if callable_rel and s["kind"] in TYPE_KINDS and s["kind"] != "interface" and s["kind"] != "trait" and s["kind"] != "type":
                    st, ids, n = self.class_target(s, site)
                    targets.extend(ids)
                    if st == CANDIDATE:
                        status = CANDIDATE
                    tnote = n or tnote
                elif callable_rel and s["kind"] not in CALLABLE_KINDS and s["kind"] != "signature":
                    continue
                else:
                    targets.append(s["id"])
            targets = sorted(dict.fromkeys(targets))
            if not targets:
                return UNRESOLVED, None, [], "target-not-callable"
            if conflict or ext or unres or val or len(targets) > 1 or any(self.is_signature(self.sym[t]) for t in targets if t in self.sym) or files:
                status = CANDIDATE
            if status == RESOLVED and len(targets) == 1:
                return RESOLVED, targets[0], [], tnote
            return CANDIDATE, None, targets, tnote or ("conflicting-binding" if conflict else "several-candidates")
        if files:
            if rel in ("renders", "references") and len(files) == 1 and not conflict and not ext:
                return RESOLVED, files[0], [], "component-file"
            if rel in ("renders", "references"):
                return CANDIDATE, None, sorted(files), "several-candidates"
            return UNRESOLVED, None, [], "module-not-callable"
        if val:
            return DYNAMIC, None, [], "callee-is-a-local-value"
        if ext and not unres:
            return EXTERNAL, None, [], f"external:{ext[0]}"
        if unres:
            return UNRESOLVED, None, [], unres[0]
        return UNRESOLVED, None, [], note or "no-binding"

    def resolve_module_site(self, site: dict) -> tuple[str, str | None, list[str], str | None]:
        b = site.get("binding") or {}
        mr = self.modres(site)
        k = mr.kind
        if k == "file":
            return RESOLVED, self.fid_of(mr.files[0]), [], mr.note
        if k == "candidates":
            ids = [self.fid_of(p) for p in mr.files[:MAX_CANDIDATES]]
            note = mr.note
            if len(mr.files) > MAX_CANDIDATES:
                note = (note or "") + f";candidateCount={len(mr.files)}"
            return CANDIDATE, None, ids, note
        if k == "dir":
            if mr.files:
                ids = [self.fid_of(p) for p in mr.files[:MAX_CANDIDATES]]
                return CANDIDATE, None, ids, (mr.note or "package") + f";package={mr.dir}"
            return UNRESOLVED, None, [], mr.note or "namespace-package-without-files"
        if k == "external":
            return EXTERNAL, None, [], mr.note or f"external:{mr.ext}"
        if k == "dynamic":
            return DYNAMIC, None, [], mr.note
        if k == "unsupported":
            return UNSUPPORTED, None, [], mr.note
        return UNRESOLVED, None, [], mr.note

    def resolve_site(self, site: dict) -> tuple[str, str | None, list[str], str | None]:
        b = site.get("binding") or {}
        rel = site["relation"]
        kind = b.get("kind")
        origin = self.parser_origin.get(site["id"])
        parser_res = origin[0] if origin else None
        if parser_res == UNSUPPORTED:
            return UNSUPPORTED, None, [], "parser-recognized-unanalyzable"
        if parser_res == EXTERNAL:
            return EXTERNAL, None, [], "parser-marked-external"
        if b.get("iife") and parser_res == RESOLVED and origin[1]:
            return RESOLVED, origin[1], [], "iife"
        lang = site["language"]
        # module-level sites
        local_export = rel == "reexports" and b.get("specifier") is None
        if (rel in MODULE_RELATIONS or kind in ("import", "require", "dynamicImport", "exportFrom", "include", "use", "mod", "source", "using")) and not local_export:
            if b.get("specifier") is None or b.get("dynamic"):
                return DYNAMIC, None, [], "non-literal-specifier"
            return self.resolve_module_site(site)
        if b.get("dynamic") or kind == "dynamic":
            return DYNAMIC, None, [], "computed-or-non-literal-callee"
        if parser_res == DYNAMIC:
            return DYNAMIC, None, [], "parser-marked-dynamic"
        name = b.get("name")
        if local_export:
            name = b.get("imported") or b.get("local")
        if not name:
            return UNRESOLVED, None, [], "no-callee-name"
        receiver = b.get("receiver")
        if kind == "message":
            return DYNAMIC, None, [], "message-send"
        if rel == "instantiates":
            return self.resolve_instantiation(site, name)
        if kind == "macro" or rel == "macros":
            return UNSUPPORTED, None, [], "macro-expansion-not-modelled"
        pos = site["span"]["byteStart"]
        scope = self.site_scope(site)
        file_id = site["fileId"]
        if kind == "jsx" and name[:1].islower() and not receiver and lang in JS_LANGS:
            return EXTERNAL, None, [], "intrinsic-element"
        if receiver is None or receiver == "":
            return self.resolve_name(site, name, pos, scope, lang, file_id)
        return self.resolve_member(site, name, str(receiver), b, pos, scope, lang, file_id)

    # -- bare names -----------------------------------------------------------
    def resolve_name(self, site: dict, name: str, pos: int, scope: str | None, lang: str, file_id: str):
        lex = self.lex_lookup(scope, name, pos, lang, file_id)
        if lex.kind == "found":
            res = self.result_from_ents(site, lex.ents, lex.conflict)
            if res[0] == RESOLVED and res[1] in self.sym:
                target = self.sym[res[1]]
                owner = self.sym.get(target.get("ownerId") or "")
                if (
                    target["kind"] in ("method", "accessor")
                    and owner is not None and owner["kind"] in CLASS_KINDS
                    and not (target.get("bindings") or {}).get("static")
                    and site["relation"] in ("calls", "references")
                ):
                    return CANDIDATE, None, [target["id"]], "virtual-dispatch"
            return res
        if lex.kind == "local":
            return DYNAMIC, None, [], "callee-is-a-local-value"
        return self.resolve_unbound(site, name, lang, file_id, scope)

    def resolve_unbound(self, site: dict, name: str, lang: str, file_id: str, scope: str | None):
        if lang in JS_LANGS and name in JS_GLOBALS:
            return EXTERNAL, None, [], "javascript-global"
        if lang == "python" and name in PY_BUILTINS:
            return EXTERNAL, None, [], "python-builtin"
        if lang == "go":
            if name in GO_BUILTINS:
                return EXTERNAL, None, [], "go-builtin"
            r = self.go_package_scope(site, name, file_id)
            if r:
                return r
        if lang == "rust" and name in RUST_PRELUDE_FUNCS:
            return EXTERNAL, None, [], "rust-prelude"
        if lang in C_LANGS:
            return self.c_unbound(site, name, file_id)
        if lang in self.FQN_LANGS:
            scoped = self.fqn_scope_ents(site, name, lang, scope)
            if scoped:
                return self.result_from_ents(site, scoped[0], not scoped[1], "package-scope" if scoped[1] else "wildcard-import")
        return UNRESOLVED, None, [], "name-not-bound-in-scope"

    def go_package_scope(self, site: dict, name: str, file_id: str):
        path = site["path"]
        d = dirname(path)
        is_test = path.endswith("_test.go")
        pkg = None
        m = self.file_modsym.get(file_id)
        if m:
            pkg = (m.get("bindings") or {}).get("package")
        out: list[dict] = []
        for p in self.dir_files.get(d, ()):
            if not p.endswith(".go") or (p.endswith("_test.go") and not is_test):
                continue
            fm = self.file_modsym.get(self.fid_of(p))
            if fm is None:
                continue
            if pkg is not None and (fm.get("bindings") or {}).get("package") != pkg:
                continue
            for ch in self.children.get(fm["id"], ()):
                if ch.get("name") == name and ch["kind"] in CALLABLE_KINDS | TYPE_KINDS and not (ch["kind"] == "method"):
                    out.append(ch)
        out = [s for s in out if not self.is_signature(s)] or out
        if not out:
            return None
        ents = {("sym", s["id"]) for s in out}
        conflict = any(GO_BUILD_SUFFIX.search(s["path"]) for s in out) or len(out) > 1
        return self.result_from_ents(site, ents, conflict, "go-package-scope")

    def c_closure(self, file_id: str) -> list[str]:
        """Files reachable through includes with literal in-snapshot targets (incl. candidates)."""
        cache = getattr(self, "_c_closure", None)
        if cache is None:
            cache = self._c_closure = {}
        if file_id in cache:
            return cache[file_id]
        by_file: dict[str, list[dict]] = defaultdict(list)
        if not hasattr(self, "_c_inc"):
            self._c_inc = defaultdict(list)
            for s in self.sites:
                if s["relation"] == "includes" and s["language"] in C_LANGS:
                    self._c_inc[s["fileId"]].append(s)
        out: list[str] = []
        seen = {file_id}
        q = deque([file_id])
        while q:
            f = q.popleft()
            for s in self._c_inc.get(f, ()):
                mr = self.modres(s)
                if mr.kind in ("file", "candidates"):
                    for p in mr.files:
                        t = self.fid_of(p)
                        if t not in seen:
                            seen.add(t)
                            out.append(t)
                            q.append(t)
        cache[file_id] = out
        return out

    def c_unbound(self, site: dict, name: str, file_id: str):
        found: list[dict] = []
        names = self.c_names
        for fid in [file_id] + self.c_closure(file_id):
            if fid not in names:
                table: dict[str, list[dict]] = defaultdict(list)
                for s in self.file_symbols.get(fid, ()):
                    if s.get("name") and s["kind"] in CALLABLE_KINDS | {"signature"} and self.sym.get(s.get("ownerId") or "", {}).get("kind") in ("module", "namespace"):
                        table[s["name"]].append(s)
                names[fid] = table
            found.extend(names[fid].get(name, ()))
        defs = [s for s in found if not self.is_signature(s)]
        found = defs or found
        if not found:
            return UNRESOLVED, None, [], "name-not-bound-in-scope"
        ids = sorted({s["id"] for s in found})[:MAX_CANDIDATES]
        return CANDIDATE, None, ids, "include-closure-name-match"

    def wildcards_of(self, file_id: str) -> list[str]:
        if self._wild is None:
            wild: dict[str, list[str]] = defaultdict(list)
            for s in self.sites:
                b = s.get("binding") or {}
                if s["language"] in self.FQN_LANGS and s["relation"] == "imports" and b.get("namespace") and isinstance(b.get("specifier"), str):
                    wild[s["fileId"]].append(b["specifier"])
            self._wild = wild
        return self._wild.get(file_id, [])

    _wild: dict | None = None

    def fqn_scope_ents(self, site: dict, head: str, lang: str, scope: str | None):
        """Names visible through the enclosing package/namespace (certain) or wildcard imports (uncertain)."""
        idx = self.fqn_index()
        if not idx:
            return None
        sep = "\\" if lang == "php" else "."
        own: set[str] = set()
        sid = scope
        hops = 0
        while sid and hops < 1000:
            hops += 1
            s = self.sym.get(sid)
            if s is None:
                break
            b = s.get("bindings") or {}
            for key in ("package", "namespace"):
                if isinstance(b.get(key), str) and b[key]:
                    own.add(b[key])
            if s["kind"] == "namespace" and isinstance(b.get("fqn"), str):
                own.add(b["fqn"])
            sid = s.get("ownerId")
        for s in self.file_symbols.get(site["fileId"], ()):
            f = (s.get("bindings") or {}).get("fqn")
            if isinstance(f, str) and s["kind"] in TYPE_KINDS | {"function"} and self.sym.get(s.get("ownerId") or "", {}).get("kind") == "module":
                norm = re.sub(r"::|\\|\.", sep, f)
                own.add(norm.rsplit(sep, 1)[0] if sep in norm else "")
        if lang == "csharp":
            for p in list(own):
                while sep in p:
                    p = p.rsplit(sep, 1)[0]
                    own.add(p)
            own.add("")
        found = [s for p in sorted(own) for s in self.fqn_lookup(idx, (p + sep + head) if p else head, lang)]
        if found:
            return ({("sym", s["id"]) for s in found}, True)
        found = [s for w in self.wildcards_of(site["fileId"]) for s in self.fqn_lookup(idx, w + sep + head, lang)]
        if found:
            return ({("sym", s["id"]) for s in found}, False)
        return None

    def resolve_instantiation(self, site: dict, name: str):
        """Hardware-style instantiation (Verilog module use): name-only, hence always a candidate, never a call."""
        if self._decl_by_name is None:
            idx: dict[tuple[str, str], list[str]] = defaultdict(list)
            for s in self.symbols:
                if s.get("ownerId") and s.get("name") and s["kind"] in ("module", "namespace", "class", "struct", "interface", "function"):
                    idx[(s["language"], s["name"])].append(s["id"])
            self._decl_by_name = idx
        found = self._decl_by_name.get((site["language"], name))
        if not found:
            return UNRESOLVED, None, [], "instantiated-unit-not-declared-in-snapshot"
        return CANDIDATE, None, sorted(found)[:MAX_CANDIDATES], "declaration-name-match"

    _decl_by_name: dict | None = None
    # -- member / qualified calls ---------------------------------------------
    def resolve_member(self, site: dict, name: str, receiver: str, b: dict, pos: int, scope: str | None, lang: str, file_id: str):
        rk = b.get("receiverKind")
        base = receiver.split("(")[0].strip() if receiver.startswith("super(") else receiver
        if rk is None:
            if base in ("this", "self", "cls", "Self", "$this"):
                rk = "this" if base != "Self" else "path"
            elif base == "super":
                rk = "super"
            elif re.fullmatch(r"[A-Za-z_$][\w$]*", base):
                rk = "identifier"
            elif re.fullmatch(r"[\w$:.\\]+", base) and ("::" in base or "." in base):
                rk = "path"
            else:
                rk = "expression"
        if lang == "rust" and (b.get("kind") == "path" or rk == "path" or "::" in receiver):
            return self.rust_call(site, name, receiver, scope)
        if rk == "expression":
            return DYNAMIC, None, [], "receiver-is-an-expression"
        if rk in ("this", "self"):
            if lang == "rust":
                return self.rust_self_method(site, name, scope)
            cls = self.enclosing_class(scope)
            if cls is None:
                return UNRESOLVED, None, [], "no-enclosing-class"
            ms = self.class_members(cls, name, with_inherited=True, lang=lang)
            if not ms:
                return UNRESOLVED, None, [], "member-not-found-in-class"
            return CANDIDATE, None, sorted({m["id"] for m in ms}), "virtual-dispatch"
        if rk == "super":
            cls = self.enclosing_class(scope)
            if cls is None:
                return UNRESOLVED, None, [], "no-enclosing-class"
            for base_cls in self.class_bases(cls):
                ms = self.class_members(base_cls, name, with_inherited=True, lang=lang)
                if ms:
                    ids = sorted({m["id"] for m in ms})
                    return (RESOLVED, ids[0], [], "super-call") if len(ids) == 1 and not self.is_signature(ms[0]) else (CANDIDATE, None, ids, "super-call-overloads")
            return UNRESOLVED, None, [], "member-not-found-in-base-classes"
        segs = [x for x in re.split(r"\.|::|\\", receiver) if x]
        if not segs:
            return DYNAMIC, None, [], "empty-receiver"
        head, rest = segs[0], segs[1:]
        lex = self.lex_lookup(scope, head, pos, lang, file_id)
        if lex.kind == "local":
            return DYNAMIC, None, [], "receiver-is-a-local-value"
        if lex.kind == "none":
            if lang in JS_LANGS and head in JS_GLOBALS:
                return EXTERNAL, None, [], "javascript-global"
            if lang == "python" and head in PY_BUILTINS:
                return EXTERNAL, None, [], "python-builtin"
            scoped = self.fqn_scope_ents(site, head, lang, scope) if lang in self.FQN_LANGS else None
            if scoped is None:
                return UNRESOLVED, None, [], "receiver-not-bound-in-scope"
            lex = Lex("found", scoped[0], not scoped[1])
        ents = set(lex.ents)
        conflict = lex.conflict
        for seg in rest:
            ents = self.member_ents(ents, seg, lang, site)
            if not ents:
                return UNRESOLVED, None, [], f"path-segment-not-found:{seg}"
        out = self.member_ents(ents, name, lang, site, final=True, via_path=(b.get("kind") == "path" or "::" in receiver))
        if not out:
            if any(e[0] == "val" for e in ents):
                return DYNAMIC, None, [], "receiver-is-a-local-value"
            return UNRESOLVED, None, [], "member-not-found"
        return self.result_from_ents(site, out, conflict)

    def member_ents(self, ents: set, name: str, lang: str, site: dict, *, final: bool = False, via_path: bool = False) -> set:
        out: set = set()
        for e in ents:
            k = e[0]
            if k == "file":
                hit = self.ns_get(e[1], name)
                if hit:
                    out |= hit
                elif lang == "python":
                    f = self.fid[e[1]]
                    if basename(f["path"]) in ("__init__.py", "__init__.pyi"):
                        res, _ = self.py_find([name], dirname(f["path"]))
                        if res is not None:
                            out |= self.py_module_ents_from(res)
                elif not self.file_has_exports(e[1]) and not (self.file_modsym.get(e[1]) and lang in JS_LANGS):
                    out.add(("unres", "target-file-not-parsed" if lang in JS_LANGS else "module-members-not-modelled-for-language"))
            elif k == "dir":
                if lang == "python":
                    res, _ = self.py_find([name], e[1])
                    if res is not None:
                        out |= self.py_module_ents_from(res)
                elif lang == "go":
                    out |= self.go_dir_member(e[1], name, site)
            elif k == "sym":
                s = self.sym[e[1]]
                if s["kind"] in TYPE_KINDS | {"impl"}:
                    ms = self.class_members(s, name, with_inherited=True, lang=lang)
                    ms = [m for m in ms if not self.is_signature(m)] or ms
                    if ms:
                        static_ok = all((m.get("bindings") or {}).get("static") for m in ms) or via_path
                        if static_ok and len(ms) == 1:
                            out.add(("sym", ms[0]["id"]))
                        else:
                            out.add(("multi",) + tuple(sorted(m["id"] for m in ms)) if len(ms) > 1 else ("multi", ms[0]["id"]))
                elif s["kind"] in ("namespace", "module"):
                    nk = e[1]
                    hit = self.ns_get(nk, name)
                    if hit:
                        out |= hit
                    else:
                        for ch in self.children.get(nk, ()):
                            if ch.get("name") == name and ch["kind"] in ITEM_KINDS:
                                out.add(("sym", ch["id"]))
            elif k == "ext":
                out.add(e)
            elif k == "unres":
                out.add(e)
        return out

    def go_dir_member(self, directory: str, name: str, site: dict) -> set:
        is_test = site["path"].endswith("_test.go")
        found: list[dict] = []
        for p in self.dir_files.get(directory, ()):
            if not p.endswith(".go") or (p.endswith("_test.go") and not is_test):
                continue
            fm = self.file_modsym.get(self.fid_of(p))
            if fm is None:
                continue
            for ch in self.children.get(fm["id"], ()):
                if ch.get("name") == name and (ch["kind"] in CALLABLE_KINDS or ch["kind"] in TYPE_KINDS) and ch["kind"] != "method":
                    found.append(ch)
        found = [s for s in found if not self.is_signature(s)] or found
        if not found:
            return {("unres", f"package-member-not-found:{name}")}
        if len(found) > 1 or any(GO_BUILD_SUFFIX.search(s["path"]) for s in found):
            return {("multi",) + tuple(sorted(s["id"] for s in found))}
        return {("sym", found[0]["id"])}

    # -- Rust calls ------------------------------------------------------------
    def rust_call(self, site: dict, name: str, receiver: str, scope: str | None):
        self.rust_init()
        key = self.rust_self_key(scope)
        if key is None:
            return UNRESOLVED, None, [], "module-scope-unknown"
        segs = [x for x in receiver.split("::")] + [name]
        if segs and segs[0] == "":
            pass
        first = segs[0]
        # a local variable/closure named like the first segment shadows items
        if first not in ("crate", "self", "super", "Self", ""):
            lex = self.lex_lookup(scope, first, site["span"]["byteStart"], "rust", site["fileId"])
            if lex.kind == "local":
                return DYNAMIC, None, [], "receiver-is-a-local-value"
        ents = self.rust_path(key, segs, impl_scope=scope)
        if not ents:
            return UNRESOLVED, None, [], "path-not-resolved"
        return self.result_from_ents(site, ents, False)

    def rust_self_method(self, site: dict, name: str, scope: str | None):
        self.rust_init()
        types = self.rust_self_type(scope)
        if not types:
            return UNRESOLVED, None, [], "Self-type-unknown"
        out: set = set()
        for t in types:
            out |= self.rust_assoc(self.sym[t[1]], name)
        if not out:
            return UNRESOLVED, None, [], "method-not-found-on-Self"
        return self.result_from_ents(site, out, False)

    # ======================================================================
    # Driver
    # ======================================================================
    def run(self) -> dict:
        index = self.index
        # Parser-origin results (unsupported / parser-marked dynamic / IIFE) are tracked so repeated passes are idempotent:
        # results this resolver wrote carry binding.resolved; the parser-origin ones are tagged ``parser: true``.
        self.parser_origin: dict[str, tuple[str, str | None]] = {}
        for s in self.sites:
            pr = s.get("resolution")
            b = s.get("binding") if isinstance(s.get("binding"), dict) else {}
            meta = b.get("resolved") if isinstance(b.get("resolved"), dict) else None
            if meta is not None:
                if meta.get("parser"):
                    self.parser_origin[s["id"]] = (pr, s.get("targetId"))
            elif pr in (UNSUPPORTED, DYNAMIC, EXTERNAL) or (pr == RESOLVED and b.get("iife") and s.get("targetId")):
                self.parser_origin[s["id"]] = (pr, s.get("targetId"))
        self.rust_init_if_needed()
        self.build_tables()
        for s in self.sites:
            res, target, cands, note = self.safe_resolve(s)
            if res == CANDIDATE and len(cands) > MAX_CANDIDATES:
                note = (note or "") + f";candidateCount={len(cands)}"
                cands = cands[:MAX_CANDIDATES]
            s["resolution"] = res
            s["targetId"] = target
            s["candidateIds"] = cands if res == CANDIDATE else []
            binding = s.get("binding")
            if isinstance(binding, dict):
                binding.pop("resolved", None)
            if note:
                if not isinstance(binding, dict):
                    binding = s["binding"] = {}
                binding["resolved"] = {"note": note}
                if note in PARSER_NOTES:
                    binding["resolved"]["parser"] = True
        index["edges"] = self.build_edges(index.get("edges") or [])
        self.update_coverage()
        lim = list(index.get("limitations") or [])
        for m in self.limitations + self.static_limitations():
            if m not in lim:
                lim.append(m)
        index["limitations"] = lim
        return index

    def rust_init_if_needed(self) -> None:
        self._impl_types = {}
        self._impls_by_type = None
        if any(s["language"] == "rust" for s in self.sites) or any(f.get("language") == "rust" for f in self.files):
            self.rust_init()

    def safe_resolve(self, site: dict):
        try:
            return self.resolve_site(site)
        except RecursionError:
            return UNRESOLVED, None, [], "resolution-recursion-limit"

    def static_limitations(self) -> list[str]:
        return [
            "Resolution is static: dynamic dispatch, callbacks, reflection, macros, shell commands and cross-language calls are not resolved; candidate edges are not proof of a call.",
            "Module resolution uses only inventoried files; build-tool aliases, generated files and environment-dependent search paths are not evaluated.",
        ]

    def build_edges(self, previous: list[dict]) -> list[dict]:
        kept = [e for e in previous if not e.get("evidenceIds") and e.get("sourceId") and e.get("targetId")]
        agg: dict[tuple, dict] = {}
        order: list[tuple] = []
        for s in self.sites:
            res = s["resolution"]
            if res not in (RESOLVED, CANDIDATE):
                continue
            targets = [s["targetId"]] if res == RESOLVED and s["targetId"] else list(s["candidateIds"])
            if not targets:
                continue
            rel = s["relation"]
            b = s.get("binding") or {}
            module_site = rel in MODULE_RELATIONS and b.get("specifier") is not None
            if module_site:
                source = s["fileId"]
            else:
                source = s.get("callerId") if s.get("callerId") in self.sym else None
                if source is None:
                    m = self.mod_by_region.get((s["fileId"], s.get("region", ""))) or self.file_modsym.get(s["fileId"])
                    source = m["id"] if m else s["fileId"]
            for t in targets:
                if t not in self.sym and t not in self.fid:
                    continue
                key = (source, t, rel, res, bool(s.get("typeOnly")))
                e = agg.get(key)
                if e is None:
                    e = agg[key] = {
                        "id": stable_id("edge", source, t, rel, res, int(bool(s.get("typeOnly")))),
                        "sourceId": source,
                        "targetId": t,
                        "relation": rel,
                        "resolution": res,
                        "evidenceIds": [],
                    }
                    if s.get("typeOnly"):
                        e["typeOnly"] = True
                    order.append(key)
                e["evidenceIds"].append(s["id"])
        edges = [agg[k] for k in order]
        ids = {e["id"] for e in edges}
        for e in kept:
            if e["id"] not in ids:
                edges.append(e)
        return edges

    def update_coverage(self) -> None:
        cov = self.index.get("coverage")
        if not isinstance(cov, dict):
            cov = self.index["coverage"] = {}
        counts: dict[str, int] = defaultdict(int)
        for s in self.sites:
            counts[s["resolution"]] += 1
        total = len(self.sites)
        cov["encounteredSites"] = total
        cov["resolvedSites"] = counts[RESOLVED]
        cov["dynamicSites"] = counts[DYNAMIC]
        cov["candidateSites"] = counts[CANDIDATE]
        cov["externalSites"] = counts[EXTERNAL]
        cov["unsupportedSites"] = counts[UNSUPPORTED]
        cov["unresolvedSites"] = counts[UNRESOLVED] + counts[CANDIDATE] + counts[UNSUPPORTED]
        cov["resolutionCounts"] = {k: counts[k] for k in (RESOLVED, CANDIDATE, DYNAMIC, EXTERNAL, UNRESOLVED, UNSUPPORTED)}


def resolve(index: dict, root: Any) -> dict:
    """Resolve ``index`` in place against the immutable inventory (config files read from ``root``)."""
    return Resolver(index, root).run()
