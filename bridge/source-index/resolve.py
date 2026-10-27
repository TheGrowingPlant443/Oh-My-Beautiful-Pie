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
JSON_EXT = ".json"
PACKAGE_JSON = "package.json"
INIT_PY = "__init__.py"
GO_TEST_SUFFIX = "_test.go"
GO_MODULE_CHARS = frozenset("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_.-")
JS_EXTS = (".ts", ".tsx", ".d.ts", ".mts", ".cts", ".d.mts", ".d.cts", ".js", ".jsx", ".mjs", ".cjs", JSON_EXT)
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


def _static_member_head(spec: str) -> str:
    """Drop a final qualified name and its separator run; leave unmatched inputs unchanged."""
    i = len(spec)
    while i > 0 and spec[i - 1] not in ".:\\/":
        i -= 1
    if i == 0 or i == len(spec):
        return spec
    while i > 0 and spec[i - 1] in ".:\\/":
        i -= 1
    return spec[:i]


def _jsonc_string_end(text: str, i: int) -> int:
    """Index of the closing quote of the string opening at ``i`` (past the end if unterminated)."""
    j, n = i + 1, len(text)
    while j < n:
        if text[j] == "\\":
            j += 2
            continue
        if text[j] == '"':
            break
        j += 1
    return j


def _jsonc_line_end(text: str, i: int) -> int:
    n = len(text)
    while i < n and text[i] not in "\r\n":
        i += 1
    return i


def _strip_jsonc_comments(text: str) -> str:
    out: list[str] = []
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if c == '"':
            j = _jsonc_string_end(text, i)
            out.append(text[i : j + 1])
            i = j + 1
        elif c == "/" and text.startswith("//", i):
            i = _jsonc_line_end(text, i)
        elif c == "/" and text.startswith("/*", i):
            j = text.find("*/", i + 2)
            i = n if j < 0 else j + 2
        else:
            out.append(c)
            i += 1
    return "".join(out)


def _jsonc_comma_is_trailing(text: str, j: int) -> bool:
    """``True`` if only whitespace separates the comma before ``j`` from a closing ``}`` or ``]``."""
    n = len(text)
    while j < n and text[j] in " \t\r\n":
        j += 1
    return j < n and text[j] in "}]"


def strip_jsonc(text: str) -> str:
    """Remove comments and trailing commas from JSONC without touching strings."""
    cleaned = _strip_jsonc_comments(text)
    # trailing commas (outside strings): rescan cheaply with a small state machine
    res: list[str] = []
    i, n = 0, len(cleaned)
    while i < n:
        c = cleaned[i]
        if c == '"':
            j = _jsonc_string_end(cleaned, i)
            res.append(cleaned[i : j + 1])
            i = j + 1
            continue
        if c == "," and _jsonc_comma_is_trailing(cleaned, i + 1):
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
        self.fpath: dict[str, dict] = {f["path"]: f for f in self.files}
        self.fid: dict[str, dict] = {f["id"]: f for f in self.files}
        self.sym: dict[str, dict] = {s["id"]: s for s in self.symbols}
        self.children: dict[str, list[dict]] = defaultdict(list)
        self.file_symbols: dict[str, list[dict]] = defaultdict(list)
        self.file_modsym: dict[str, dict] = {}
        self.mod_by_region: dict[tuple[str, str], dict] = {}
        self._index_symbols()
        self.dir_files: dict[str, list[str]] = defaultdict(list)
        self.lower: dict[str, list[str]] = defaultdict(list)
        self.all_dirs: set[str] = {""}
        self.by_basename: dict[str, list[str]] = defaultdict(list)
        self.c_names: dict[str, dict[str, list[dict]]] = {}
        self._pkg_members: dict[str, set[str]] | None = None
        self._cfg_by_path: dict[str, dict] | None = None
        self._changed: set = set()
        self._index_paths(index.get("directories") or [])
        # languages whose files are not all fully parsed (external is not provable there)
        self.lang_incomplete: set[str] = {f["language"] for f in self.files if f.get("language") and f.get("status") != "parsed"}
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

    def _index_symbols(self) -> None:
        for s in self.symbols:
            if s.get("ownerId"):
                self.children[s["ownerId"]].append(s)
            self.file_symbols[s["fileId"]].append(s)
            if s["kind"] == "module":
                self._index_module(s)

    def _index_module(self, s: dict) -> None:
        self.mod_by_region.setdefault((s["fileId"], s.get("region", "")), s)
        if not s.get("ownerId") or self.sym.get(s["ownerId"], {}).get("fileId") != s["fileId"]:
            self.file_modsym.setdefault(s["fileId"], s)

    def _index_paths(self, directories: Iterable) -> None:
        for f in self.files:
            path = f["path"]
            self.dir_files[dirname(path)].append(path)
            self.lower[path.lower()].append(path)
            self.by_basename[basename(path)].append(path)
            for a in ancestors(dirname(path)):
                if a in self.all_dirs:
                    break
                self.all_dirs.add(a)
        for d in directories:
            if isinstance(d, str):
                self.all_dirs.update(ancestors(d.strip("/")))

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
        except (OSError, ValueError):
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
            except ValueError:
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
        if scope["kind"] not in CLASS_KINDS:
            for child in self.children.get(sid, ()):
                decl = self._implicit_decl(child, seen_syms)
                if decl is not None:
                    by_name[child["name"]].append(decl)
        self._decl_index[sid] = by_name
        return by_name

    @staticmethod
    def _implicit_decl(child: dict, seen_syms: set[str]) -> dict | None:
        if child["id"] in seen_syms or not child.get("name"):
            return None
        kind = child["kind"]
        if kind not in DECL_SYMBOL_KINDS and kind not in ("signature", "macro"):
            return None
        decl_kind = "function" if kind in CALLABLE_KINDS or kind == "signature" else kind
        return {"name": child["name"], "kind": decl_kind, "symbolId": child["id"], "siteId": None, "mutable": False, "scopeStart": 0, "scopeEnd": 1 << 62, "implicit": True}

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

    def scope_hit(self, scope: dict, name: str, pos: int) -> Lex | None:
        decls = self.decls_of(scope).get(name)
        if not decls:
            return None
        group = self._innermost_decls(decls, pos)
        if not group:
            return None
        syms, imports, local, rebinding = self._classify_decls(group)
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

    @staticmethod
    def _innermost_decls(decls: list[dict], pos: int) -> list[dict]:
        """Non-global declarations of the narrowest scope range covering ``pos`` (empty if none)."""
        inside = [d for d in decls if d["scopeStart"] <= pos < d["scopeEnd"]]
        if not inside:
            return []
        narrow = min(d["scopeEnd"] - d["scopeStart"] for d in inside)
        return [d for d in inside if d["scopeEnd"] - d["scopeStart"] == narrow and d["kind"] != "global"]

    def _classify_decls(self, group: list[dict]) -> tuple[dict[str, dict], list[dict], bool, bool]:
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
        return syms, imports, local, rebinding

    def lex_lookup(self, scope_id: str | None, name: str, pos: int, lang: str, file_id: str) -> Lex:
        sid = scope_id
        first = True
        hops = 0
        while sid and hops < 100000:
            hops += 1
            s = self.sym.get(sid)
            if s is None:
                break
            lex = self._lex_scope_step(s, name, pos, lang, file_id, first)
            if lex is not None:
                return lex
            first = False
            sid = s.get("ownerId")
        return Lex("none")

    def _lex_scope_step(self, s: dict, name: str, pos: int, lang: str, file_id: str, first: bool) -> Lex | None:
        """One ``lex_lookup`` hop: a result ends the walk, ``None`` continues to the owner scope."""
        if lang == "rust" and self.is_rmod(s):
            # a Rust module is a hard scope boundary: nothing outside it is visible without `use`
            hit = self.ns_get(s["id"], name)
            return Lex("found", set(hit), False, s["id"]) if hit else Lex("none")
        if lang == "python" and s["kind"] == "module":
            hit = self.scope_hit(s, name, pos)
            if hit:
                return hit
            star = self.glob.get(file_id, {}).get(name)
            return Lex("found", set(star), False, s["id"]) if star else None
        if lang == "python" and s["kind"] == "class" and not first:
            return None
        return self.scope_hit(s, name, pos)

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
        js_local = self._js_module_export_rows()
        js_local.extend(self._js_symbol_export_rows())
        js_sites = self._js_reexport_sites(js_local)
        py_stars = [s for s in self.sites if self._is_py_star_site(s)]
        rust_mods, rust_uses = self._rust_use_sites()
        py_imports = self._seed_static_namespaces()
        # fixed point: specs are re-evaluated only when a namespace they read changed in the previous round
        specs = self._js_local_specs(js_local)
        specs.extend((self._modres_deps(site), functools.partial(self._js_reexport, site)) for site in js_sites)
        specs.extend(self._py_import_specs(py_imports))
        specs.extend((self._modres_deps(site), functools.partial(self._py_star, site)) for site in py_stars)
        if rust_mods or rust_uses:
            specs.append(("rust", functools.partial(self._rust_round, rust_mods, rust_uses)))
        if not self._run_fixed_point(specs):
            self.limit(f"Module export/namespace resolution did not converge within {MAX_ROUNDS} rounds; some re-exports may be missing.")
        self.finalized = True
        self._import_cache.clear()

    def _js_module_export_rows(self) -> list[tuple]:
        rows: list[tuple] = []
        for m in self.symbols:
            if m["kind"] != "module" or m["language"] not in JS_LANGS:
                continue
            for e in (m.get("bindings") or {}).get("exports") or []:
                name = "default" if e.get("default") else e.get("exported")
                rows.append((m["fileId"], name, e.get("symbolId"), e.get("local"), m))
        return rows

    def _js_symbol_export_rows(self) -> list[tuple]:
        rows: list[tuple] = []
        for s in self.symbols:
            if s["language"] not in JS_LANGS or not s.get("exported") or not s.get("ownerId"):
                continue
            if self.sym.get(s["ownerId"], {}).get("kind") != "module":
                continue
            names = (s.get("bindings") or {}).get("exportNames") or ([s["name"]] if s.get("name") else [])
            rows.extend((s["fileId"], n, s["id"], None, None) for n in names)
        return rows

    def _js_reexport_sites(self, js_local: list[tuple]) -> list[dict]:
        """Reexport sites with a specifier; local reexports (no specifier) are appended to ``js_local``."""
        js_sites: list[dict] = []
        for site in self.sites:
            if site["language"] not in JS_LANGS or site["relation"] != "reexports":
                continue
            b = site.get("binding") or {}
            if b.get("specifier") is not None:
                js_sites.append(site)
            else:
                mod = self.mod_by_region.get((site["fileId"], site.get("region", "")))
                js_local.append((site["fileId"], b.get("alias") or b.get("imported"), None, b.get("imported") or b.get("local"), mod))
        return js_sites

    @staticmethod
    def _is_py_star_site(site: dict) -> bool:
        if site["language"] != "python" or site["relation"] not in ("imports", "reexports"):
            return False
        b = site.get("binding") or {}
        return bool(b.get("namespace")) and b.get("specifier") is not None and not b.get("alias") and not b.get("local")

    def _rust_use_sites(self) -> tuple[list[dict], list[dict]]:
        rust_mods: list[dict] = []
        rust_uses: list[dict] = []
        for site in self.sites:
            if site["language"] != "rust":
                continue
            kind = (site.get("binding") or {}).get("kind")
            if kind == "mod":
                rust_mods.append(site)
            elif kind == "use" or site["relation"] == "reexports":
                rust_uses.append(site)
        return rust_mods, rust_uses

    def _seed_static_namespaces(self) -> list[tuple]:
        """Seed python/rust module namespaces with declared items; return python import rows still to resolve."""
        py_imports: list[tuple] = []
        for m in self.symbols:
            if m["kind"] != "module" and not self.is_rmod(m):
                continue
            if m["language"] == "python":
                py_imports.extend(self._seed_python_module(m))
            elif m["language"] == "rust":
                self._seed_rust_module(m)
        return py_imports

    def _seed_python_module(self, m: dict) -> list[tuple]:
        fid = m["fileId"]
        if m.get("ownerId") and self.sym.get(m["ownerId"], {}).get("fileId") == fid:
            return []
        py_imports: list[tuple] = []
        for name, ds in self.decls_of(m).items():
            for d in ds:
                if d.get("symbolId") and d["symbolId"] in self.sym:
                    self.exp[fid][name].add(("sym", d["symbolId"]))
                elif d["kind"] == "variable":
                    self.exp[fid][name].add(("val", name))
                elif d["kind"] == "import" and d.get("siteId"):
                    py_imports.append((fid, name, d["siteId"]))
        return py_imports

    def _seed_rust_module(self, m: dict) -> None:
        key = m["id"]
        for child in self.children.get(key, ()):
            if self._is_rust_namespace_item(child):
                self.exp[key][child["name"]].add(("sym", child["id"]))

    @staticmethod
    def _is_rust_namespace_item(child: dict) -> bool:
        if not child.get("name") or child["kind"] not in ITEM_KINDS or child["kind"] == "impl":
            return False
        # `mod x;` is supplied by the mod-declaration rule (file entity)
        return not (child["kind"] in ("namespace", "module") and (child.get("bindings") or {}).get("body") is False)

    def _js_local_specs(self, js_local: list[tuple]) -> list[tuple[Any, Any]]:
        specs: list[tuple[Any, Any]] = []
        for fid, name, symid, local, m in js_local:
            if not name:
                continue
            if symid and symid in self.sym:
                specs.append((frozenset(), functools.partial(self._add, self.exp, fid, name, {("sym", symid)})))
            elif local and m is not None:
                specs.append((self._local_import_deps(m, local), functools.partial(self._js_local_export, fid, name, m, local)))
        return specs

    def _py_import_specs(self, py_imports: list[tuple]) -> list[tuple[Any, Any]]:
        specs: list[tuple[Any, Any]] = []
        for fid, name, siteid in py_imports:
            site = self._site_by_id[siteid]
            specs.append((self._modres_deps(site), functools.partial(self._py_import_export, fid, name, site)))
        return specs

    def _run_fixed_point(self, specs: list[tuple[Any, Any]]) -> bool:
        """Run specs round by round; ``False`` if ``MAX_ROUNDS`` was exhausted without convergence."""
        dirty: set | None = None
        for _ in range(MAX_ROUNDS):
            self._changed = set()
            for deps, fn in specs:
                if self._spec_is_dirty(deps, dirty):
                    fn()
            if not self._changed:
                return True
            dirty = self._changed
        return False

    def _spec_is_dirty(self, deps: Any, dirty: set | None) -> bool:
        if dirty is None:
            return True
        if deps == "rust":
            return any(self.sym.get(k, {}).get("language") == "rust" for k in dirty)
        return bool(deps & dirty)

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
        if mr.kind not in ("file", "candidates"):
            return False
        fid = site["fileId"]
        targets = [self.fid_of(p) for p in mr.files]
        exported = b.get("alias")
        imported = b.get("imported")
        if b.get("namespace") and not exported and not imported:
            return self._js_reexport_star(fid, targets)
        if b.get("namespace") and exported:
            return self._add(self.exp, fid, exported, {("file", t) for t in targets})
        if imported is None:
            return False
        return self._js_reexport_named(fid, exported or imported, imported, targets)

    def _js_reexport_star(self, fid: str, targets: list[str]) -> bool:
        changed = False
        for t in targets:
            for n in self.ns_names(t):
                if n == "default":
                    continue
                changed |= self._add(self.glob, fid, n, self.ns_get(t, n) or ())
        return changed

    def _js_reexport_named(self, fid: str, out_name: str, imported: str, targets: list[str]) -> bool:
        changed = False
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
        out = frozenset(self._import_ents_uncached(site))
        if self.finalized:
            self._import_cache[site["id"]] = out
        return out

    def _import_ents_uncached(self, site: dict) -> Iterable:
        b = site.get("binding") or {}
        mr = self.modres(site)
        if mr.kind == "external":
            return {("ext", mr.ext or str(b.get("specifier")))}
        if mr.kind in ("unresolved", "dynamic", "unsupported"):
            return {("unres", mr.note or mr.kind)}
        return self._import_ents_by_language(site, b, mr)

    def _import_ents_by_language(self, site: dict, b: dict, mr: ModRes) -> Iterable:
        lang = site["language"]
        if lang in JS_LANGS or self.path_is_container(site["path"]):
            return self._js_import_ents(b, mr)
        if lang == "python":
            return self._py_import_ents(site, b)
        if lang == "go":
            return self._go_import_ents(mr)
        if lang == "rust":
            return self.rust_use_ents(site)
        return self._generic_import_ents(site, b, mr)

    @staticmethod
    def _go_import_ents(mr: ModRes) -> set:
        if mr.kind in ("dir", "candidates", "file"):
            return {("dir", mr.dir or dirname(mr.files[0]))}
        return set()

    def path_is_container(self, path: str) -> bool:
        return path.endswith(CONTAINER_EXTS)

    def _js_import_ents(self, b: dict, mr: ModRes) -> set:
        ents: set = set()
        if mr.kind not in ("file", "candidates"):
            return ents
        for p in mr.files:
            ents |= self._js_import_file_ents(b, p)
        return ents

    def _js_import_file_ents(self, b: dict, p: str) -> set:
        t = self.fid_of(p)
        imported = b.get("imported")
        if b.get("namespace") and not imported:
            return {("file", t)}
        if b.get("default") or imported == "default":
            return self._js_import_member_ents(t, "default", "default-export-not-found", p.endswith(CONTAINER_EXTS))
        if imported:
            return self._js_import_member_ents(t, imported, f"export-not-found:{imported}", False)
        return {("file", t)}

    def _js_import_member_ents(self, t: str, name: str, missing_note: str, container: bool) -> set:
        hit = self.ns_get(t, name)
        if hit:
            return hit
        if container or not self.file_has_exports(t):
            return {("file", t)}
        return {("unres", missing_note)}

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
                res = self._modres_by_language(site, spec, lang)
            except RecursionError:
                res = ModRes("unresolved", note="resolution recursion limit")
        if lang != "rust" or self.finalized:
            self._modres_cache[site["id"]] = res
        return res

    def _modres_by_language(self, site: dict, spec: str, lang: str) -> ModRes:
        if lang in JS_LANGS or self.path_is_container(site["path"]):
            return self.js_modres(site, spec)
        if lang == "python":
            return self.py_modres(site, spec)
        if lang == "rust":
            return self.rust_modres(site, spec)
        if lang == "go":
            return self.go_modres(spec)
        if lang in C_LANGS:
            return self.c_modres(site, spec)
        return self.generic_modres(site, spec)

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
        hits = self._js_ext_swap_hits(target)
        if hits:
            return hits
        hits = [target + e for e in JS_EXTS if target + e in self.fpath]
        if hits:
            return hits
        hits = self._js_package_field_hits(target)
        if hits:
            return hits
        idx = [posixpath.join(target, "index" + e) for e in JS_EXTS]
        return [i for i in idx if i in self.fpath]

    def _js_ext_swap_hits(self, target: str) -> list[str]:
        for ext, swaps in JS_EXT_SWAP.items():
            if target.endswith(ext):
                stem = target[: -len(ext)]
                hits = [stem + s for s in swaps if stem + s in self.fpath]
                if hits:
                    return hits
        return []

    def _js_package_field_hits(self, target: str) -> list[str]:
        pkg = posixpath.join(target, PACKAGE_JSON)
        if pkg not in self.fpath:
            return []
        data = self.read_json(pkg)
        if not isinstance(data, dict):
            return []
        for field in ("types", "typings", "module", "main"):
            v = data.get(field)
            if not isinstance(v, str):
                continue
            sub = norm_join(target, v)
            if sub is None:
                continue
            hits = self.js_probe_file_only(sub)
            if hits:
                return hits
        return []

    def js_probe_file_only(self, target: str) -> list[str]:
        if target in self.fpath:
            return [target]
        hits = self._js_ext_swap_hits(target)
        if hits:
            return hits
        return [target + e for e in JS_EXTS if target + e in self.fpath]

    def js_modres(self, site: dict, spec: str) -> ModRes:
        if "\x00" in spec or "\\" in spec:
            return ModRes("unresolved", note="hostile-specifier")
        importer = site["path"]
        if spec in (".", "..") or spec.startswith(("./", "../")):
            return self._js_relative_modres(dirname(importer), spec)
        if spec.startswith("/") or WINDOWS_DRIVE_RE.match(spec):
            return ModRes("unresolved", note="absolute-path-specifier")
        if spec.startswith("node:") or spec.split("/")[0] in NODE_BUILTINS:
            return ModRes("external", ext=spec)
        if SCHEME_RE.match(spec) and not spec.startswith("@"):
            return self._js_scheme_modres(spec)
        if spec.startswith("#"):
            return self.js_imports_map(site, spec)
        return self._js_bare_modres(importer, spec)

    def _js_relative_modres(self, idir: str, spec: str) -> ModRes:
        target = norm_join(idir, spec)
        if target is None:
            return ModRes("unresolved", note="specifier-escapes-snapshot")
        hits = self.js_probe(target)
        if not hits:
            return ModRes("unresolved", note=self.case_hint(target) or "no-file-in-snapshot")
        return self.file_res(hits)

    @staticmethod
    def _js_scheme_modres(spec: str) -> ModRes:
        if spec.split(":", 1)[0] in ("http", "https", "data", "npm", "jsr", "bun", "deno"):
            return ModRes("external", ext=spec)
        return ModRes("unresolved", note="unsupported-url-scheme")

    def _js_bare_modres(self, importer: str, spec: str) -> ModRes:
        # tsconfig/jsconfig paths + baseUrl
        results = self.js_tsconfig_hits(importer, spec)
        if results:
            return self._js_tsconfig_modres(results)
        pkg = self.js_package(importer, spec)
        if pkg is not None:
            return pkg
        name = self.pkg_name(spec)
        if self.js_declared(importer, name):
            return ModRes("external", ext=name)
        return ModRes("unresolved", note="bare-specifier-not-found")

    def _js_tsconfig_modres(self, results: list[list[str]]) -> ModRes:
        distinct = {tuple(sorted(r)) for r in results}
        if len(distinct) == 1:
            return self.file_res(list(results[0]), note="tsconfig-paths")
        allp: list[str] = sorted({p for r in results for p in r})
        return ModRes("candidates", allp, note="tsconfig-paths-disagree")

    @staticmethod
    def pkg_name(spec: str) -> str:
        parts = spec.split("/")
        if spec.startswith("@") and len(parts) >= 2:
            return parts[0] + "/" + parts[1]
        return parts[0]

    def js_declared(self, importer: str, name: str) -> bool:
        for d in ancestors(dirname(importer)):
            p = posixpath.join(d, PACKAGE_JSON)
            if p not in self.fpath:
                continue
            data = self.read_json(p)
            if isinstance(data, dict) and self._js_pkg_declares(data, name):
                return True
        return False

    @staticmethod
    def _js_pkg_declares(data: dict, name: str) -> bool:
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
        self._tsconfig_extends(cfg, data, path, cdir, stack)
        self._tsconfig_options(cfg, data, path, cdir)
        self._tsconfig_references(cfg, data, cdir)
        return cfg

    def _tsconfig_extends(self, cfg: dict, data: dict, path: str, cdir: str, stack: set[str]) -> None:
        ext = data.get("extends")
        parents = [ext] if isinstance(ext, str) else [e for e in (ext or []) if isinstance(e, str)]
        if path in stack:
            self.limit(f"tsconfig extends cycle at {path}.")
            return
        for p in parents:
            self._tsconfig_extend_one(cfg, path, cdir, p, stack)

    def _tsconfig_extend_one(self, cfg: dict, path: str, cdir: str, p: str, stack: set[str]) -> None:
        if not p.startswith(("./", "../")):
            self.limit(f"tsconfig {path} extends package/absolute configuration {p}; its path mapping was not read.")
            return
        tp = norm_join(cdir, p)
        parent = None
        for c in (tp, (tp + JSON_EXT) if tp is not None else None):
            if c and c in self.fpath:
                parent = self.load_tsconfig(c, stack | {path})
                break
        if parent is None:
            self.limit(f"tsconfig {path} extends {p}, which is not in the inventory.")
            return
        for k in ("baseUrl", "paths", "pathsDir", "files", "include", "exclude", "filesDir", "includeDir", "excludeDir"):
            if parent[k] is not None:
                cfg[k] = parent[k]

    def _tsconfig_options(self, cfg: dict, data: dict, path: str, cdir: str) -> None:
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

    def _tsconfig_references(self, cfg: dict, data: dict, cdir: str) -> None:
        refs = data.get("references")
        if not isinstance(refs, list):
            return
        for r in refs:
            if not (isinstance(r, dict) and isinstance(r.get("path"), str)):
                continue
            rp = norm_join(cdir, r["path"])
            if rp is None:
                continue
            for cand in (rp, rp + JSON_EXT, posixpath.join(rp, "tsconfig.json")):
                if cand in self.fpath:
                    cfg["references"].append(cand)
                    break

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
            covering = self._tsconfig_covering(by_path, roots, importer)
            if not covering:
                continue
            root_paths = {r["path"] for r in roots}
            referenced = [c for c in covering if c["path"] not in root_paths]
            return self._tsconfig_spec_results(referenced or covering, spec)
        return []

    def _tsconfig_covering(self, by_path: dict, roots: list[dict], importer: str) -> list[dict]:
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
        return covering

    def _tsconfig_spec_results(self, top: list[dict], spec: str) -> list[list[str]]:
        results: list[list[str]] = []
        for c in top:
            hits = self.cfg_resolve(c, spec)
            if hits:
                results.append(hits)
        return results

    def cfg_resolve(self, cfg: dict, spec: str) -> list[str]:
        paths = cfg["paths"]
        if paths:
            best = self._cfg_best_paths_match(paths, spec)
            if best is not None:
                hits = self._cfg_paths_hits(cfg, best)
                if hits:
                    return hits
        if cfg["baseUrl"] is not None:
            tp = norm_join(cfg["baseUrl"], spec)
            if tp is not None:
                hits = self.js_probe(tp)
                if hits:
                    return hits
        return []

    @staticmethod
    def _cfg_best_paths_match(paths: dict, spec: str) -> tuple | None:
        best = None
        for pat, targets in paths.items():
            if "*" not in pat:
                if pat == spec:
                    return (1 << 30, "", targets)
                continue
            pre, _, suf = pat.partition("*")
            if not (spec.startswith(pre) and spec.endswith(suf) and len(spec) >= len(pre) + len(suf)):
                continue
            if best is None or len(pre) > best[0]:
                best = (len(pre), spec[len(pre) : len(spec) - len(suf)], targets)
        return best

    def _cfg_paths_hits(self, cfg: dict, best: tuple) -> list[str]:
        base = cfg["baseUrl"] if cfg["baseUrl"] is not None else cfg["pathsDir"]
        for t in best[2]:
            tp = norm_join(base, t.replace("*", best[1], 1))
            if tp is None:
                continue
            hits = self.js_probe(tp)
            if hits:
                return hits
        return []

    # -- packages / exports ---------------------------------------------------
    def pkg_index(self) -> dict[str, str]:
        if self._pkg_index is None:
            idx: dict[str, str] = {}
            dup: set[str] = set()
            paths = [p for p in self.fpath if basename(p) == PACKAGE_JSON]
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
        pkg_json_path = posixpath.join(pdir, PACKAGE_JSON)
        data = self.read_json(pkg_json_path)
        if not isinstance(data, dict):
            return None
        in_self = in_dir(importer, pdir)
        if not (self.js_declared(importer, name) or self.workspace_member(pdir) or (in_self and data.get("exports") is not None)):
            return None
        sub = "." + spec[len(name):]
        return self.pkg_entry(pdir, data, sub)

    def workspace_member(self, pdir: str) -> bool:
        for d in ancestors(dirname(pdir) if pdir else ""):
            p = posixpath.join(d, PACKAGE_JSON)
            if p not in self.fpath or d == pdir:
                continue
            if any(isinstance(pat, str) and self.glob_match_dir(d, pat.rstrip("/"), pdir + "/x") for pat in self._workspace_patterns(p)):
                return True
        return False

    def _workspace_patterns(self, path: str) -> Iterable:
        data = self.read_json(path)
        ws = data.get("workspaces") if isinstance(data, dict) else None
        if isinstance(ws, dict):
            ws = ws.get("packages")
        return ws if isinstance(ws, list) else ()

    def pkg_entry(self, pdir: str, data: dict, subpath: str) -> ModRes:
        exports = data.get("exports")
        if exports is not None:
            return self.exports_res(pdir, exports, subpath)
        if subpath == ".":
            return self._pkg_main_entry(pdir, data)
        t = norm_join(pdir, subpath[2:])
        if t is None:
            return ModRes("unresolved", note="specifier-escapes-package")
        hits = self.js_probe(t)
        return self.file_res(hits, note="package-subpath") if hits else ModRes("unresolved", note=self.case_hint(t) or "package-subpath-not-in-snapshot")

    def _pkg_main_entry(self, pdir: str, data: dict) -> ModRes:
        for field in ("types", "typings", "module", "main"):
            v = data.get(field)
            if not isinstance(v, str):
                continue
            t = norm_join(pdir, v)
            if t is None:
                continue
            hits = self.js_probe_file_only(t) or self.js_probe(t)
            if hits:
                return self.file_res(hits, note="package-main")
        hits = self.js_probe(posixpath.join(pdir, "index"))
        if hits:
            return self.file_res(hits, note="package-index")
        return ModRes("unresolved", note="package-entry-not-in-snapshot")

    def exports_res(self, pdir: str, exports: Any, subpath: str) -> ModRes:
        if isinstance(exports, (str, list)) or (isinstance(exports, dict) and exports and not any(k.startswith((".", "#")) for k in exports)):
            exports = {".": exports}
        if not isinstance(exports, dict):
            return ModRes("unresolved", note="invalid-exports")
        target, star = self._exports_target(exports, subpath)
        if target is None:
            return ModRes("unresolved", note="subpath-not-exported")
        leaves: list[str] = []
        self.collect_leaves(target, leaves, 0)
        relative = [leaf for leaf in leaves if leaf.startswith("./")]
        found = self._exports_files(pdir, relative, star)
        if not found:
            return ModRes("unresolved", note="export-target-not-in-snapshot")
        distinct = sorted(set(found))
        if len(distinct) == 1 and (len(relative) <= 1 or len(found) == len(relative)):
            return ModRes("file", distinct, note="package-exports")
        return ModRes("candidates", distinct, note="package-exports-conditions")

    @staticmethod
    def _exports_target(exports: dict, subpath: str) -> tuple[Any, str]:
        if subpath in exports:
            return exports[subpath], ""
        best = None
        for k in exports:
            if "*" not in k:
                continue
            pre, _, suf = k.partition("*")
            if not (subpath.startswith(pre) and subpath.endswith(suf) and len(subpath) >= len(pre) + len(suf)):
                continue
            if best is None or len(pre) > best[0]:
                best = (len(pre), k, subpath[len(pre) : len(subpath) - len(suf)])
        if best is None:
            return None, ""
        return exports[best[1]], best[2]

    def _exports_files(self, pdir: str, leaves: list[str], star: str) -> list[str]:
        found: list[str] = []
        for leaf in leaves:
            tp = norm_join(pdir, (leaf.replace("*", star) if "*" in leaf else leaf)[2:] or ".")
            if tp is not None and tp in self.fpath:
                found.append(tp)
        return found

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
            p = posixpath.join(d, PACKAGE_JSON)
            if p in self.fpath:
                data = self.read_json(p)
                if isinstance(data, dict) and isinstance(data.get("imports"), dict):
                    return self.exports_res(d, data["imports"], spec)
        return ModRes("unresolved", note="package-imports-not-found")

    # ======================================================================
    # Python
    # ======================================================================
    def declared_py(self) -> set[str]:
        if self._py_deps is None:
            deps: set[str] = set()
            for p in self.fpath:
                b = basename(p)
                if b == "pyproject.toml":
                    deps.update(self._pyproject_deps(p))
                elif re.fullmatch(r"requirements[^/]*\.txt", b):
                    deps.update(self._requirements_deps(p))
            self._py_deps = deps
        return self._py_deps

    @staticmethod
    def _normalize_py_dependency(name: str) -> str:
        return re.sub(r"[-_.]+", "_", name).lower()

    def _py_requirement_name(self, line: str) -> str | None:
        m = re.match(r"\s*([A-Za-z0-9_.-]+)", line)
        return self._normalize_py_dependency(m.group(1)) if m else None

    def _pyproject_deps(self, path: str) -> set[str]:
        data = self.read_toml(path)
        if not isinstance(data, dict):
            return set()
        deps: set[str] = set()
        proj = data.get("project") or {}
        lines = list(proj.get("dependencies") or []) + [x for v in (proj.get("optional-dependencies") or {}).values() for x in v]
        for line in lines:
            name = self._py_requirement_name(str(line))
            if name is not None:
                deps.add(name)
        poetry = ((data.get("tool") or {}).get("poetry") or {})
        for k in list((poetry.get("dependencies") or {}).keys()) + list((poetry.get("dev-dependencies") or {}).keys()):
            deps.add(self._normalize_py_dependency(k))
        return deps

    def _requirements_deps(self, path: str) -> set[str]:
        deps: set[str] = set()
        for line in (self.read_text(path) or "").splitlines():
            name = self._py_requirement_name(line)
            if name is not None and not line.lstrip().startswith(("#", "-")):
                deps.add(name)
        return deps

    def py_find(self, parts: list[str], base_dir: str) -> tuple[ModRes | None, str | None]:
        """Find dotted module below ``base_dir``: package > module; returns (ModRes, root)."""
        if not parts:
            return None, None
        rel = "/".join(parts)
        t = norm_join(base_dir, rel)
        if t is None:
            return ModRes("unresolved", note="escapes-snapshot"), None
        init = posixpath.join(t, INIT_PY)
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
        parts = rest.split(".") if rest else []
        if any(not p for p in parts):
            return ModRes("unresolved", note="malformed-module-name")
        idir = dirname(site["path"])
        if level:
            return self._py_relative_modres(idir, level, parts)
        found = self._py_source_roots(idir, parts)
        if len(found) == 1:
            return next(iter(found.values()))
        if len(found) > 1:
            files = sorted({f for r in found.values() for f in r.files})
            return ModRes("candidates", files, note="several-source-roots")
        top = parts[0]
        if top in PY_STDLIB:
            return ModRes("external", ext=top, note="python-stdlib")
        for d in ancestors(idir):
            res, _ = self.py_find(parts, d)
            if res is not None and res.kind == "dir":
                return res
        if self._normalize_py_dependency(top) in self.declared_py():
            return ModRes("external", ext=top, note="declared-dependency")
        return ModRes("unresolved", note="module-not-in-snapshot")

    def _py_relative_modres(self, base: str, level: int, parts: list[str]) -> ModRes:
        for _ in range(level - 1):
            if base == "":
                return ModRes("unresolved", note="relative-import-beyond-root")
            base = dirname(base)
        if not parts:
            init = posixpath.join(base, INIT_PY)
            if init in self.fpath:
                return ModRes("file", [init], dir=base, note="package")
            return ModRes("dir", [], dir=base, note="package-directory")
        res, _ = self.py_find(parts, base)
        return res or ModRes("unresolved", note=self.case_hint(posixpath.join(base, *parts)) or "relative-module-not-in-snapshot")

    def _py_source_roots(self, idir: str, parts: list[str]) -> dict[tuple, ModRes]:
        found: dict[tuple, ModRes] = {}
        for d in ancestors(idir):
            for base in (d, posixpath.join(d, "src") if d else "src"):
                res, _ = self.py_find(parts, base)
                if res is not None and res.kind != "dir":
                    found[(res.kind, tuple(res.files), res.dir)] = res
        return found

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

    def _py_import_ents(self, site: dict, b: dict) -> set:
        imported = b.get("imported")
        mods = self.py_module_ents(site)
        if imported is None or imported == "":
            return self._py_bare_import_ents(site, b, mods)
        out: set = set()
        for ent in mods:
            out |= self._py_import_member(ent, imported)
        return out

    def _py_bare_import_ents(self, site: dict, b: dict, mods: set) -> set:
        spec = b.get("specifier") or ""
        local = b.get("local") or b.get("alias")
        bare = spec.lstrip(".")
        head = bare.split(".")[0]
        # `import a.b.c` binds `a`; an alias binds the imported module itself.
        if local and not b.get("alias") and local == head and "." in bare and spec == bare:
            res, _ = self.py_find([head], self._py_root_for(site, spec))
            if res is not None:
                return self.py_module_ents_from(res)
        return mods

    def _py_import_member(self, ent: tuple, imported: str) -> set:
        if ent[0] == "file":
            hit = self.ns_get(ent[1], imported)
            if hit:
                return hit
            base = self.fid[ent[1]]["path"]
            if basename(base) not in (INIT_PY, "__init__.pyi"):
                return {("unres", f"name-not-found:{imported}")}
            pdir = dirname(base)
        elif ent[0] == "dir":
            pdir = ent[1]
        else:
            return {ent}
        sub, _ = self.py_find([imported], pdir)
        if sub is not None:
            return self.py_module_ents_from(sub)
        return {("unres", f"name-not-found:{imported}")}

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
        # Include search paths are unknown: exact literal suffix matches are candidates only.
        matches = self._c_include_matches(spec)
        if matches:
            return ModRes("candidates", sorted(matches)[:MAX_CANDIDATES], note="include-path-unknown")
        if system:
            return ModRes("external", ext=spec, note="system-include")
        if direct is not None and (case := self.case_hint(direct)):
            return ModRes("unresolved", note=case)
        return ModRes("unresolved", note="include-not-in-snapshot")

    def _c_include_matches(self, spec: str) -> list[str]:
        norm = norm_join("", spec)
        if not norm:
            return []
        return [p for p in self.by_basename.get(basename(norm), ()) if p == norm or p.endswith("/" + norm)]

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
                self._go_manifest(p)
        self.go_mods.sort(key=lambda t: -len(t[0]))
        self.go_replace.sort(key=lambda t: -len(t[0]))

    def _go_manifest(self, path: str) -> None:
        text = self.read_text(path)
        if text is None:
            return
        d = dirname(path)
        m = re.search(r"^\s*module\s+(\S+)", text, re.M)
        if m:
            self.go_mods.append((m.group(1).strip('"'), d))
        self.go_requires.update(self._go_require_paths(text))
        for r in re.finditer(r"^\s*(?:replace\s+)?(\S+)(?:\s+v\S+)?\s*=>\s*(\.{1,2}/\S*)", text, re.M):
            tp = norm_join(d, r.group(2))
            if tp is not None:
                self.go_replace.append((r.group(1), tp))

    @staticmethod
    def _is_go_dependency_path(path: str) -> bool:
        dot = path.find(".", 1)
        if dot < 0 or dot == len(path) - 1:
            return False
        return all(c in GO_MODULE_CHARS or (c == "/" and i > dot) for i, c in enumerate(path))

    def _go_require_paths(self, text: str) -> Iterable[str]:
        """Read line-anchored dependency tokens; whitespace may cross lines, versions start with v + ASCII digit."""
        pending = None
        after_require = False
        for line in text.split("\n"):
            for i, token in enumerate(line.split()):
                if pending is not None and len(token) > 1 and token[0] == "v" and "0" <= token[1] <= "9":
                    yield pending
                    pending = None
                    after_require = False
                    continue
                pending = None
                if (i == 0 or after_require) and self._is_go_dependency_path(token):
                    pending = token
                after_require = i == 0 and token == "require"

    def go_pkg_files(self, directory: str) -> list[str]:
        return sorted(p for p in self.dir_files.get(directory, ()) if p.endswith(".go") and not p.endswith(GO_TEST_SUFFIX))

    def go_modres(self, spec: str) -> ModRes:
        self.go_init()
        if "\x00" in spec or "\\" in spec or ".." in spec.split("/"):
            return ModRes("unresolved", note="hostile-specifier")
        local = self._go_local_modres(spec)
        if local is not None:
            return local
        replaced = self._go_replaced_modres(spec)
        if replaced is not None:
            return replaced
        first = spec.split("/")[0]
        if "." not in first:
            return ModRes("external", ext=spec, note="go-standard-library")
        if any(spec == r or spec.startswith(r + "/") for r in self.go_requires):
            return ModRes("external", ext=spec, note="go-module-dependency")
        return ModRes("unresolved", note="package-not-in-snapshot")

    def _go_local_modres(self, spec: str) -> ModRes | None:
        for mod, d in self.go_mods:
            if spec != mod and not spec.startswith(mod + "/"):
                continue
            sub = spec[len(mod) :].lstrip("/")
            target = norm_join(d, sub) if sub else d
            if target is None:
                return ModRes("unresolved", note="specifier-escapes-snapshot")
            files = self.go_pkg_files(target)
            if not files:
                return ModRes("unresolved", note="package-directory-not-in-snapshot")
            return ModRes("dir", files, dir=target, note="go-package")
        return None

    def _go_replaced_modres(self, spec: str) -> ModRes | None:
        for mod, d in self.go_replace:
            if spec != mod and not spec.startswith(mod + "/"):
                continue
            sub = spec[len(mod) :].lstrip("/")
            target = norm_join(d, sub) if sub else d
            files = self.go_pkg_files(target) if target is not None else []
            if files:
                return ModRes("dir", files, dir=target, note="go-replace")
        return None

    # ======================================================================
    # Rust
    # ======================================================================
    def rust_init(self) -> None:
        if self._rust_ready:
            return
        self._rust_ready = True
        self.crates: list[dict] = []
        self.rust_roots: dict[str, dict] = {}
        manifests = sorted(p for p in self.fpath if basename(p) == "Cargo.toml")
        datas = {p: self.read_toml(p) for p in manifests}
        ws_deps = self._rust_workspace_deps(datas)
        for p, data in datas.items():
            crate = self._rust_crate_of_manifest(p, data, ws_deps)
            if crate is not None:
                self.crates.append(crate)
        self._rust_index_crates()
        self._rust_index_mod_sites()

    @staticmethod
    def _rust_workspace_deps(datas: dict) -> dict[str, dict]:
        ws_deps: dict[str, dict] = {}
        for p, data in datas.items():
            if not isinstance(data, dict):
                continue
            wd = ((data.get("workspace") or {}).get("dependencies") or {})
            if isinstance(wd, dict):
                ws_deps[dirname(p)] = wd
        return ws_deps

    def _rust_crate_of_manifest(self, p: str, data: Any, ws_deps: dict[str, dict]) -> dict | None:
        if not isinstance(data, dict):
            return None
        pkg = data.get("package")
        if not isinstance(pkg, dict) or not isinstance(pkg.get("name"), str):
            return None
        d = dirname(p)
        crate = {"name": pkg["name"].replace("-", "_"), "pkg": pkg["name"], "dir": d, "manifest": p, "lib": None, "roots": [], "deps": {}}
        lib = data.get("lib") if isinstance(data.get("lib"), dict) else {}
        lp = norm_join(d, lib.get("path") or "src/lib.rs")
        if lib.get("name"):
            crate["name"] = str(lib["name"]).replace("-", "_")
        if lp and lp in self.fpath:
            crate["lib"] = lp
            crate["roots"].append(lp)
        crate["roots"].extend(self._rust_bin_roots(d, data))
        crate["roots"].extend(self._rust_dir_roots(d))
        crate["deps"].update(self._rust_manifest_deps(data, d, ws_deps))
        crate["roots"] = sorted(set(crate["roots"]))
        return crate

    def _rust_bin_roots(self, d: str, data: dict) -> list[str]:
        roots: list[str] = []
        bins = data.get("bin") if isinstance(data.get("bin"), list) else []
        for bn in bins:
            if not isinstance(bn, dict) or not isinstance(bn.get("path"), str):
                continue
            bp = norm_join(d, bn["path"])
            if bp and bp in self.fpath:
                roots.append(bp)
        return roots

    def _rust_dir_roots(self, d: str) -> list[str]:
        roots: list[str] = []
        for fixed in ("src/main.rs", "build.rs"):
            f = norm_join(d, fixed)
            if f in self.fpath:
                roots.append(f)
        for sub in ("src/bin", "tests", "benches", "examples"):
            roots.extend(q for q in self.dir_files.get(posixpath.join(d, sub) if d else sub, ()) if q.endswith(".rs"))
        return roots

    def _rust_manifest_deps(self, data: dict, d: str, ws_deps: dict[str, dict]) -> dict[str, dict | None]:
        deps: dict[str, dict | None] = {}
        target = data.get("target")
        scopes = [data]
        if isinstance(target, dict):
            scopes.extend(tv for tv in target.values() if isinstance(tv, dict))
        for scope in scopes:
            for table in ("dependencies", "dev-dependencies", "build-dependencies"):
                deps.update(self._rust_deps(scope.get(table), d, ws_deps))
        return deps

    def _rust_index_crates(self) -> None:
        for c in self.crates:
            for r in c["roots"]:
                self.rust_roots.setdefault(r, c)
        by_name: dict[str, list[dict]] = defaultdict(list)
        for c in self.crates:
            if c["lib"]:
                by_name[c["name"]].append(c)
        self.crate_by_name = {n: cs[0] for n, cs in by_name.items() if len(cs) == 1}
        self.crate_by_pkg = {c["pkg"]: c for c in self.crates if c["lib"] and by_name[c["name"]] == [c]}

    def _rust_index_mod_sites(self) -> None:
        # file -> roots (reachability through `mod name;` edges)
        self.rust_file_roots: dict[str, set[str]] = defaultdict(set)
        self.rust_parent: dict[str, set[str]] = defaultdict(set)  # file module symbol -> containing module keys
        self.rust_mod_child: dict[str, str | None] = {}
        mod_sites = [s for s in self.sites if s["language"] == "rust" and (s.get("binding") or {}).get("kind") == "mod"]
        for s in mod_sites:
            self.rust_mod_child[s["id"]] = self.rust_mod_target(s)
        files_by_parent: dict[str, list[str]] = defaultdict(list)
        for s in mod_sites:
            child = self.rust_mod_child[s["id"]]
            if not child:
                continue
            m = self.file_modsym.get(self.fid_of(child))
            scope = self.site_scope(s)
            if m and scope:
                self.rust_parent[m["id"]].add(scope)
            files_by_parent[s["path"]].append(child)
        self._rust_walk_roots(files_by_parent)

    def _rust_walk_roots(self, files_by_parent: dict[str, list[str]]) -> None:
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

    @staticmethod
    def _rust_workspace_dep(key: str, val: dict, cdir: str, ws_deps: dict[str, dict]) -> tuple[Any, str | None]:
        """Inherit `key` from the nearest ancestor workspace table; (val, None) when none declares it."""
        for d in ancestors(cdir):
            wd = ws_deps.get(d)
            if wd is None or key not in wd:
                continue
            inherited = wd[key]
            if isinstance(inherited, dict) and isinstance(inherited.get("path"), str):
                return inherited, norm_join(d, inherited["path"])
            return inherited, None
        return val, None

    def _rust_deps(self, table: Any, cdir: str, ws_deps: dict[str, dict]) -> dict[str, dict | None]:
        out: dict[str, dict | None] = {}
        if not isinstance(table, dict):
            return out
        for key, val in table.items():
            real = key
            path = None
            if isinstance(val, dict) and val.get("workspace") is True:
                val, path = self._rust_workspace_dep(key, val, cdir, ws_deps)
            if isinstance(val, dict):
                real = val.get("package", key)
                if path is None and isinstance(val.get("path"), str):
                    path = norm_join(cdir, val["path"])
            out[key.replace("-", "_")] = {"pkg": real, "path": path}
        return out

    def _rust_mod_chain(self, site: dict) -> list[str]:
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
        return chain

    def _rust_mod_base(self, path: str, chain: list[str]) -> str:
        base = dirname(path)
        if basename(path) not in ("mod.rs", "lib.rs", "main.rs") and path not in getattr(self, "rust_roots", {}):
            base = posixpath.join(base, basename(path)[:-3]) if base else basename(path)[:-3]
        for part in chain:
            base = posixpath.join(base, part) if base else part
        return base

    def rust_mod_target(self, site: dict) -> str | None:
        b = site["binding"]
        name = b.get("specifier")
        if not name or "/" in name or "\\" in name or ".." in name:
            return None
        path = site["path"]
        chain = self._rust_mod_chain(site)
        d = dirname(path)
        base = self._rust_mod_base(path, chain)
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

    def _rust_dep_crate(self, dep: dict) -> dict | None:
        target = None
        if dep.get("path"):
            for c in self.crates:
                if c["dir"] == dep["path"] and c["lib"]:
                    target = c
        if target is None:
            target = self.crate_by_pkg.get(dep["pkg"])
        return target

    def rust_lookup_first(self, key: str, seg: str) -> set:
        hit = self.ns_get(key, seg)
        if hit:
            return set(hit)
        crate = self.rust_current_crate(key)
        if seg in RUST_STD_CRATES:
            return {("ext", seg)}
        if crate is None:
            return set()
        if seg == crate["name"] and crate["lib"]:
            return {("file", self.fid_of(crate["lib"]))}
        dep = crate["deps"].get(seg)
        if dep is None:
            return set()
        target = self._rust_dep_crate(dep)
        if target is not None and target["lib"]:
            return {("file", self.fid_of(target["lib"]))}
        return {("ext", seg)}

    def _rust_start_global(self, key: str, segs: list[str]) -> tuple[set, list[str], bool]:
        rest = segs[1:]
        if not rest:
            return set(), rest, True
        ents = self.rust_lookup_first(key, rest[0])
        if not ents:
            return {("ext", segs[1])}, rest[1:], True
        return ents, rest[1:], False

    def _rust_super_parents(self, cur: set) -> set:
        nxt: set = set()
        for k in cur:
            s = self.sym[k]
            if s.get("ownerId") and self.sym.get(s["ownerId"], {}).get("fileId") == s["fileId"]:
                nxt.add(s["ownerId"])
            else:
                nxt |= self.rust_parent.get(k, set())
        return nxt

    def _rust_start_super(self, key: str, segs: list[str]) -> tuple[set, list[str], bool]:
        cur = {key}
        segs2 = list(segs)
        while segs2 and segs2[0] == "super":
            cur = self._rust_super_parents(cur)
            segs2.pop(0)
        if not cur:
            return {("unres", "super-module-unknown")}, segs2, True
        return {("sym", k) for k in cur}, segs2, False

    def _rust_path_start(self, key: str, segs: list[str], impl_scope: str | None) -> tuple[set, list[str], bool]:
        """Resolve the first segment: (ents, remaining segments, final) where final means ents is already the path result."""
        first = segs[0]
        rest = segs[1:]
        if first == "":
            return self._rust_start_global(key, segs)
        if first == "super":
            return self._rust_start_super(key, segs)
        if first == "self":
            return {("sym", key)}, rest, False
        if first == "crate":
            ents = {("file", self.fid_of(r)) for r in self.rust_crate_roots(key)}
            miss = "crate-root-unknown"
        elif first == "Self":
            ents = self.rust_self_type(impl_scope)
            miss = "Self-type-unknown"
        else:
            ents = self.rust_lookup_first(key, first)
            if not ents:
                return ({("ext", first)} if first in RUST_PRELUDE_FUNCS else set()), rest, True
            return ents, rest, False
        if not ents:
            return {("unres", miss)}, rest, True
        return ents, rest, False

    def rust_path(self, key: str, segs: list[str], *, impl_scope: str | None = None) -> set:
        """Resolve a (possibly module-qualified) path starting from module ``key``."""
        if not segs:
            return set()
        ents, rest, final = self._rust_path_start(key, segs, impl_scope)
        if final:
            return ents
        for seg in rest:
            nxt: set = set()
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

    _RUST_TRAIT_MEMBER_KINDS = CALLABLE_KINDS | {"signature"}

    def _rust_named_members(self, owner_id: str, name: str, kinds: frozenset | set) -> set:
        return {("sym", ch["id"]) for ch in self.children.get(owner_id, ()) if ch.get("name") == name and ch["kind"] in kinds}

    def rust_assoc(self, type_sym: dict, name: str) -> set:
        inherent: set = set()
        traits: set = set()
        for impl in self.rust_impls_for(type_sym):
            is_trait = bool((impl.get("bindings") or {}).get("implTrait"))
            (traits if is_trait else inherent).update(self._rust_named_members(impl["id"], name, CALLABLE_KINDS))
        if type_sym["kind"] == "trait":
            traits |= self._rust_named_members(type_sym["id"], name, self._RUST_TRAIT_MEMBER_KINDS)
        if len(inherent) == 1 and not traits:
            return inherent
        allents = inherent | traits
        if not allents:
            return set()
        return {("multi",) + tuple(sorted(e[1] for e in allents))}

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

    def _rust_round_mod(self, s: dict) -> bool:
        child = self.rust_mod_child.get(s["id"])
        key = self.rust_self_key(self.site_scope(s))
        name = s["binding"].get("specifier")
        if child and key and name:
            return self._add(self.exp, key, name, {("file", self.fid_of(child))})
        return False

    def _rust_round_glob(self, key: str, ents: set) -> bool:
        changed = False
        for e in ents:
            nk = self.rust_ns_key(e)
            if nk is None:
                continue
            for n in self.ns_names(nk):
                changed |= self._add(self.glob, key, n, self.ns_get(nk, n) or ())
        return changed

    def _rust_round_use(self, s: dict) -> bool:
        b = s["binding"]
        scope = self.site_scope(s)
        key = self.rust_self_key(scope)
        holder = self.sym.get(scope or "")
        if holder is not None and not self.is_rmod(holder):
            return False  # function-local `use` is resolved lexically, not added to the module namespace
        if key is None or not b.get("specifier"):
            return False
        segs = self.rust_use_segments(b)
        ents = {e for e in self.rust_path(key, segs) if e[0] in ("sym", "file", "ext", "multi")}
        if not ents:
            return False
        if b.get("namespace"):
            return self._rust_round_glob(key, ents)
        name = b.get("alias") or b.get("local") or segs[-1]
        if name == "self" and len(segs) > 1:
            name = segs[-2]
        if name == "_":
            return False
        return self._add(self.exp, key, name, ents)

    def _rust_round(self, mods: list[dict], uses: list[dict]) -> bool:
        self.rust_init()
        changed = False
        for s in mods:
            changed |= self._rust_round_mod(s)
        for s in uses:
            changed |= self._rust_round_use(s)
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
            return self.fqn_modres(spec, lang, b)
        return ModRes("unsupported", note=f"{lang}-module-resolution-not-implemented")

    def path_literal_modres(self, site: dict, spec: str, lang: str, b: dict) -> ModRes:
        idir = dirname(site["path"])
        rel_marker = b.get("relativeTo") in ("dir", "__DIR__", "__dirname", "$PSScriptRoot", "script")
        if rel_marker and spec.startswith("/") and not spec.startswith("//"):
            spec = spec[1:]  # `__DIR__ . '/x.php'`: the parser keeps the literal tail
        if spec.startswith("/") or WINDOWS_DRIVE_RE.match(spec) or "\\" in spec:
            return ModRes("unresolved", note="absolute-or-hostile-path")
        if lang == "zig" and self._is_zig_module(spec):
            return ModRes("external", ext=spec, note="zig-module")
        exts = {"ruby": ("", ".rb"), "lua": ("", ".lua", "/init.lua", ".luau"), "bash": ("",), "powershell": ("", ".ps1", ".psm1"),
                "php": ("",), "julia": ("", ".jl"), "zig": ("",)}[lang]
        spec_path = spec.replace(".", "/") if lang == "lua" and "/" not in spec and not spec.endswith((".lua", ".luau")) else spec
        rel_to_dir = rel_marker or lang in ("zig", "julia")
        bases = self._path_literal_bases(idir, lang, rel_to_dir)
        hits = self._path_literal_hits(idir, spec_path, bases, exts)
        if hits is None:
            return ModRes("unresolved", note="specifier-escapes-snapshot")
        if not hits:
            return self._missing_literal_path(idir, spec_path, spec, lang, rel_to_dir)
        if rel_to_dir and len(hits) == 1:
            return ModRes("file", hits, note="relative-to-source-file")
        return ModRes("candidates", hits, note="search-path-unknown")

    @staticmethod
    def _is_zig_module(spec: str) -> bool:
        return spec in ("std", "builtin", "root") or ("/" not in spec and not spec.endswith((".zig", ".zon")))

    @staticmethod
    def _path_literal_bases(idir: str, lang: str, rel_to_dir: bool) -> list[str]:
        if rel_to_dir:
            return [idir]
        bases = [idir, ""]
        if lang == "ruby":
            bases.append("lib")
        return list(dict.fromkeys(bases))

    def _path_literal_hits(self, idir: str, spec_path: str, bases: list[str], exts: tuple[str, ...]) -> list[str] | None:
        hits: list[str] = []
        for base in bases:
            t = norm_join(base, spec_path)
            if t is None:
                if base == idir:
                    return None
                continue
            for e in exts:
                cand = t + e
                if cand in self.fpath:
                    hits.append(cand)
        return sorted(set(hits))

    def _missing_literal_path(self, idir: str, spec_path: str, spec: str, lang: str, rel_to_dir: bool) -> ModRes:
        first = norm_join(idir, spec_path) or spec
        hint = self.case_hint(first)
        if hint:
            return ModRes("unresolved", note=hint)
        if lang in ("ruby", "lua") and not rel_to_dir and lang not in self.lang_incomplete:
            return ModRes("external", ext=spec, note="not-in-snapshot")
        return ModRes("unresolved", note="file-not-in-snapshot")

    def fqn_modres(self, spec: str, lang: str, b: dict) -> ModRes:
        idx = self.fqn_index()
        # Normalize separators to what the parser stores in fqn (identity match first).
        cands = self.fqn_lookup(idx, spec, lang)
        if b.get("namespace"):
            wildcard = self._fqn_wildcard_modres(spec, lang)
            if wildcard is not None:
                return wildcard
        if cands:
            files = sorted({s["path"] for s in cands})
            return self.file_res(files, certain=len(cands) == 1, note="fqn-match")
        # Java static import: class fqn + member.
        if len(self._split_q(spec)) > 1:
            cls = self.fqn_lookup(idx, _static_member_head(spec), lang)
            if cls:
                files = sorted({s["path"] for s in cls})
                return self.file_res(files, certain=len(cls) == 1, note="static-member-import")
        if lang in self.lang_incomplete or any(f.get("language") == lang and f.get("status") != "parsed" for f in self.files):
            return ModRes("unresolved", note="not-found-and-some-source-files-unparsed")
        return ModRes("external", ext=spec, note="not-in-snapshot")

    def _fqn_wildcard_modres(self, spec: str, lang: str) -> ModRes | None:
        norm_spec = re.sub(r"::|\\|/", ".", spec)
        key = norm_spec.lower() if lang == "fortran" else norm_spec
        files = sorted(self.pkg_members().get(key, ()))
        if not files:
            return None
        note = "wildcard-import" + ("-truncated" if len(files) > MAX_CANDIDATES else "")
        return ModRes("candidates", files[:MAX_CANDIDATES], note=note)

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
        if mr.kind not in ("file", "candidates"):
            return set()
        spec = b.get("specifier") or ""
        idx = self.fqn_index()
        lang = site["language"]
        cands = self.fqn_lookup(idx, spec, lang) if not b.get("namespace") else []
        if cands:
            return {("sym", s["id"]) for s in cands}
        if mr.note == "static-member-import":
            ents = self._static_import_ents(idx, spec, lang)
            if ents:
                return ents
        return {("file", self.fid_of(p)) for p in mr.files}

    def _static_import_ents(self, idx: dict, spec: str, lang: str) -> set:
        head = _static_member_head(spec)
        name = self._split_q(spec)[-1]
        return {
            ("sym", ch["id"])
            for c in self.fqn_lookup(idx, head, lang)
            for ch in self.children.get(c["id"], ())
            if ch.get("name") == name and ch["kind"] in CALLABLE_KINDS
        }

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

    def class_members(self, cls: dict, name: str, *, with_inherited: bool, seen: set | None = None) -> list[dict]:
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
                out.extend(self.class_members(base, name, with_inherited=True, seen=seen))
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
            for e in self._class_base_ents(cls, segs):
                if e[0] == "sym" and e[1] in self.sym and self.sym[e[1]]["kind"] in TYPE_KINDS:
                    out.append(self.sym[e[1]])
        return out

    def _class_base_ents(self, cls: dict, segs: list[str]) -> set:
        lex = self.lex_lookup(cls.get("ownerId"), segs[0], cls["span"]["byteStart"], cls["language"], cls["fileId"])
        ents = set(lex.ents) if lex.kind == "found" else set()
        for seg in segs[1:]:
            nxt: set = set()
            for e in ents:
                if e[0] == "file":
                    nxt |= self.ns_get(e[1], seg) or set()
            ents = nxt
        return ents

    @staticmethod
    def is_ctor(sym: dict) -> bool:
        return sym["kind"] == "constructor" or (sym["kind"] in ("method", "function") and sym.get("name") in ("__init__",))

    def class_target(self, cls: dict) -> tuple[str, list[str], str | None]:
        ctors = [c for c in self.children.get(cls["id"], ()) if self.is_ctor(c) and not self.is_signature(c)]
        if not ctors:
            return RESOLVED, [cls["id"]], "implicit-constructor"
        if len(ctors) == 1:
            return RESOLVED, [ctors[0]["id"]], None
        return CANDIDATE, [c["id"] for c in ctors], "overloaded-constructors"

    def result_from_ents(self, site: dict, ents: set, conflict: bool, note: str | None = None) -> tuple[str, str | None, list[str], str | None]:
        syms, files, ext, unres, val, multi = self._result_entity_groups(ents)
        conflict = conflict or multi
        if syms:
            return self._symbol_result(site, syms, conflict, bool(ext or unres or val or files), note)
        rel = site["relation"]
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

    def _result_entity_groups(self, ents: set) -> tuple[list[dict], list[str], list[str], list[str], bool, bool]:
        syms: list[dict] = []
        multi: list[str] = []
        files: list[str] = []
        ext: list[str] = []
        unres: list[str] = []
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
        syms.extend(self.sym[i] for i in multi if i in self.sym)
        return syms, files, ext, unres, val, bool(multi)

    def _symbol_result(self, site: dict, syms: list[dict], conflict: bool, mixed: bool, note: str | None):
        impl_keys = {(s.get("ownerId"), s.get("name")) for s in syms if not self.is_signature(s)}
        syms = [s for s in syms if not (self.is_signature(s) and (s.get("ownerId"), s.get("name")) in impl_keys)]
        status, targets, tnote = self._symbol_targets(syms, site["relation"] in ("calls", "instantiates"), note)
        if not targets:
            return UNRESOLVED, None, [], "target-not-callable"
        if conflict or mixed or len(targets) > 1 or any(self.is_signature(self.sym[t]) for t in targets if t in self.sym):
            status = CANDIDATE
        if status == RESOLVED and len(targets) == 1:
            return RESOLVED, targets[0], [], tnote
        return CANDIDATE, None, targets, tnote or ("conflicting-binding" if conflict else "several-candidates")

    def _symbol_targets(self, syms: list[dict], callable_rel: bool, note: str | None) -> tuple[str, list[str], str | None]:
        targets: list[str] = []
        status = RESOLVED
        for s in syms:
            if callable_rel and s["kind"] in TYPE_KINDS and s["kind"] not in ("interface", "trait", "type"):
                st, ids, n = self.class_target(s)
                targets.extend(ids)
                if st == CANDIDATE:
                    status = CANDIDATE
                note = n or note
            elif callable_rel and s["kind"] not in CALLABLE_KINDS and s["kind"] != "signature":
                continue
            else:
                targets.append(s["id"])
        return status, sorted(dict.fromkeys(targets)), note

    def resolve_module_site(self, site: dict) -> tuple[str, str | None, list[str], str | None]:
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
        parser_result = self._parser_site_result(origin, b)
        if parser_result is not None:
            return parser_result
        local_export = rel == "reexports" and b.get("specifier") is None
        if (rel in MODULE_RELATIONS or kind in ("import", "require", "dynamicImport", "exportFrom", "include", "use", "mod", "source", "using")) and not local_export:
            if b.get("specifier") is None or b.get("dynamic"):
                return DYNAMIC, None, [], "non-literal-specifier"
            return self.resolve_module_site(site)
        if b.get("dynamic") or kind == "dynamic":
            return DYNAMIC, None, [], "computed-or-non-literal-callee"
        if origin and origin[0] == DYNAMIC:
            return DYNAMIC, None, [], "parser-marked-dynamic"
        return self._resolve_callee_site(site, b, local_export)

    @staticmethod
    def _parser_site_result(origin: tuple | None, b: dict):
        if origin is None:
            return None
        parser_res, target = origin
        if parser_res == UNSUPPORTED:
            return UNSUPPORTED, None, [], "parser-recognized-unanalyzable"
        if parser_res == EXTERNAL:
            return EXTERNAL, None, [], "parser-marked-external"
        if b.get("iife") and parser_res == RESOLVED and target:
            return RESOLVED, target, [], "iife"
        return None

    def _resolve_callee_site(self, site: dict, b: dict, local_export: bool):
        name = b.get("name")
        if local_export:
            name = b.get("imported") or b.get("local")
        if not name:
            return UNRESOLVED, None, [], "no-callee-name"
        receiver = b.get("receiver")
        kind = b.get("kind")
        rel = site["relation"]
        lang = site["language"]
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
        builtin = self._global_name_note(lang, name)
        if builtin:
            return EXTERNAL, None, [], builtin
        if lang == "go":
            if name in GO_BUILTINS:
                return EXTERNAL, None, [], "go-builtin"
            r = self.go_package_scope(site, name, file_id)
            if r:
                return r
        if lang == "rust" and name in RUST_PRELUDE_FUNCS:
            return EXTERNAL, None, [], "rust-prelude"
        if lang in C_LANGS:
            return self.c_unbound(name, file_id)
        if lang in self.FQN_LANGS:
            scoped = self.fqn_scope_ents(site, name, lang, scope)
            if scoped:
                return self.result_from_ents(site, scoped[0], not scoped[1], "package-scope" if scoped[1] else "wildcard-import")
        return UNRESOLVED, None, [], "name-not-bound-in-scope"

    @staticmethod
    def _global_name_note(lang: str, name: str) -> str | None:
        if lang in JS_LANGS and name in JS_GLOBALS:
            return "javascript-global"
        if lang == "python" and name in PY_BUILTINS:
            return "python-builtin"
        return None

    def go_package_scope(self, site: dict, name: str, file_id: str):
        path = site["path"]
        m = self.file_modsym.get(file_id)
        pkg = (m.get("bindings") or {}).get("package") if m else None
        out = self.go_dir_decls(dirname(path), name, path.endswith(GO_TEST_SUFFIX), pkg)
        out = self.prefer_non_signatures(out)
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
        out: list[str] = []
        seen = {file_id}
        q = deque([file_id])
        while q:
            f = q.popleft()
            for t in self._c_include_targets(f):
                if t not in seen:
                    seen.add(t)
                    out.append(t)
                    q.append(t)
        cache[file_id] = out
        return out

    def _c_include_targets(self, file_id: str) -> Iterable[str]:
        if not hasattr(self, "_c_inc"):
            self._c_inc = defaultdict(list)
            for s in self.sites:
                if s["relation"] == "includes" and s["language"] in C_LANGS:
                    self._c_inc[s["fileId"]].append(s)
        for s in self._c_inc.get(file_id, ()):
            mr = self.modres(s)
            if mr.kind in ("file", "candidates"):
                yield from (self.fid_of(p) for p in mr.files)

    def c_unbound(self, name: str, file_id: str):
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
        own = self._fqn_owner_namespaces(scope)
        own.update(self._fqn_file_namespaces(site["fileId"], sep))
        if lang == "csharp":
            parents: set[str] = set()
            for p in own:
                while sep in p:
                    p = p.rsplit(sep, 1)[0]
                    parents.add(p)
            own.update(parents)
            own.add("")
        found = [s for p in sorted(own) for s in self.fqn_lookup(idx, (p + sep + head) if p else head, lang)]
        if found:
            return ({("sym", s["id"]) for s in found}, True)
        found = [s for w in self.wildcards_of(site["fileId"]) for s in self.fqn_lookup(idx, w + sep + head, lang)]
        if found:
            return ({("sym", s["id"]) for s in found}, False)
        return None

    def _fqn_owner_namespaces(self, scope: str | None) -> set[str]:
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
        return own

    def _fqn_file_namespaces(self, file_id: str, sep: str) -> set[str]:
        own: set[str] = set()
        for s in self.file_symbols.get(file_id, ()):
            f = (s.get("bindings") or {}).get("fqn")
            if isinstance(f, str) and s["kind"] in TYPE_KINDS | {"function"} and self.sym.get(s.get("ownerId") or "", {}).get("kind") == "module":
                norm = re.sub(r"::|\\|\.", sep, f)
                own.add(norm.rsplit(sep, 1)[0] if sep in norm else "")
        return own

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
        rk = self._receiver_kind(receiver, b)
        if lang == "rust" and (b.get("kind") == "path" or rk == "path" or "::" in receiver):
            return self.rust_call(site, name, receiver, scope)
        if rk == "expression":
            return DYNAMIC, None, [], "receiver-is-an-expression"
        if rk in ("this", "self"):
            return self._self_member_result(site, name, scope, lang)
        if rk == "super":
            return self._super_member_result(name, scope)
        return self._qualified_member_result(site, name, receiver, b, pos, scope, lang, file_id)

    @staticmethod
    def _receiver_kind(receiver: str, b: dict) -> str:
        rk = b.get("receiverKind")
        if rk is not None:
            return rk
        base = receiver.split("(")[0].strip() if receiver.startswith("super(") else receiver
        if base in ("this", "self", "cls", "Self", "$this"):
            return "this" if base != "Self" else "path"
        if base == "super":
            return "super"
        if re.fullmatch(r"[A-Za-z_$][\w$]*", base):
            return "identifier"
        if re.fullmatch(r"[\w$:.\\]+", base) and ("::" in base or "." in base):
            return "path"
        return "expression"

    def _self_member_result(self, site: dict, name: str, scope: str | None, lang: str):
        if lang == "rust":
            return self.rust_self_method(site, name, scope)
        cls = self.enclosing_class(scope)
        if cls is None:
            return UNRESOLVED, None, [], "no-enclosing-class"
        ms = self.class_members(cls, name, with_inherited=True)
        if not ms:
            return UNRESOLVED, None, [], "member-not-found-in-class"
        return CANDIDATE, None, sorted({m["id"] for m in ms}), "virtual-dispatch"

    def _super_member_result(self, name: str, scope: str | None):
        cls = self.enclosing_class(scope)
        if cls is None:
            return UNRESOLVED, None, [], "no-enclosing-class"
        for base_cls in self.class_bases(cls):
            ms = self.class_members(base_cls, name, with_inherited=True)
            if ms:
                ids = sorted({m["id"] for m in ms})
                return (RESOLVED, ids[0], [], "super-call") if len(ids) == 1 and not self.is_signature(ms[0]) else (CANDIDATE, None, ids, "super-call-overloads")
        return UNRESOLVED, None, [], "member-not-found-in-base-classes"

    def _qualified_member_result(self, site: dict, name: str, receiver: str, b: dict, pos: int, scope: str | None, lang: str, file_id: str):
        segs = [x for x in re.split(r"\.|::|\\", receiver) if x]
        if not segs:
            return DYNAMIC, None, [], "empty-receiver"
        head, rest = segs[0], segs[1:]
        lex = self.lex_lookup(scope, head, pos, lang, file_id)
        if lex.kind == "local":
            return DYNAMIC, None, [], "receiver-is-a-local-value"
        if lex.kind == "none":
            builtin = self._global_name_note(lang, head)
            if builtin:
                return EXTERNAL, None, [], builtin
            scoped = self.fqn_scope_ents(site, head, lang, scope) if lang in self.FQN_LANGS else None
            if scoped is None:
                return UNRESOLVED, None, [], "receiver-not-bound-in-scope"
            lex = Lex("found", scoped[0], not scoped[1])
        via_path = b.get("kind") == "path" or "::" in receiver
        return self._member_path_result(site, name, rest, lex, lang, via_path)

    def _member_path_result(self, site: dict, name: str, rest: list[str], lex: Lex, lang: str, via_path: bool):
        ents = set(lex.ents)
        for seg in rest:
            ents = self.member_ents(ents, seg, lang, site)
            if not ents:
                return UNRESOLVED, None, [], f"path-segment-not-found:{seg}"
        out = self.member_ents(ents, name, lang, site, via_path=via_path)
        if not out:
            if any(e[0] == "val" for e in ents):
                return DYNAMIC, None, [], "receiver-is-a-local-value"
            return UNRESOLVED, None, [], "member-not-found"
        return self.result_from_ents(site, out, lex.conflict)

    def member_ents(self, ents: set, name: str, lang: str, site: dict, *, via_path: bool = False) -> set:
        out: set = set()
        for e in ents:
            out |= self._member_entity_ents(e, name, lang, site, via_path)
        return out

    def _member_entity_ents(self, ent: tuple, name: str, lang: str, site: dict, via_path: bool) -> set:
        k = ent[0]
        if k == "file":
            return self._file_member_ents(ent[1], name, lang)
        if k == "dir":
            if lang == "python":
                res, _ = self.py_find([name], ent[1])
                return self.py_module_ents_from(res) if res is not None else set()
            if lang == "go":
                return self.go_dir_member(ent[1], name, site)
            return set()
        if k == "sym":
            return self._symbol_member_ents(self.sym[ent[1]], name, via_path)
        if k in ("ext", "unres"):
            return {ent}
        return set()

    def _file_member_ents(self, file_id: str, name: str, lang: str) -> set:
        hit = self.ns_get(file_id, name)
        if hit:
            return hit
        if lang == "python":
            path = self.fid[file_id]["path"]
            if basename(path) in (INIT_PY, "__init__.pyi"):
                res, _ = self.py_find([name], dirname(path))
                if res is not None:
                    return self.py_module_ents_from(res)
            return set()
        if not self.file_has_exports(file_id) and not (self.file_modsym.get(file_id) and lang in JS_LANGS):
            return {("unres", "target-file-not-parsed" if lang in JS_LANGS else "module-members-not-modelled-for-language")}
        return set()

    def _symbol_member_ents(self, sym: dict, name: str, via_path: bool) -> set:
        if sym["kind"] in TYPE_KINDS | {"impl"}:
            return self._type_member_ents(sym, name, via_path)
        if sym["kind"] in ("namespace", "module"):
            hit = self.ns_get(sym["id"], name)
            if hit:
                return hit
            return {("sym", ch["id"]) for ch in self.children.get(sym["id"], ()) if ch.get("name") == name and ch["kind"] in ITEM_KINDS}
        return set()

    def _type_member_ents(self, sym: dict, name: str, via_path: bool) -> set:
        ms = self.prefer_non_signatures(self.class_members(sym, name, with_inherited=True))
        if not ms:
            return set()
        static_ok = all((m.get("bindings") or {}).get("static") for m in ms) or via_path
        if static_ok and len(ms) == 1:
            return {("sym", ms[0]["id"])}
        return {("multi",) + tuple(sorted(m["id"] for m in ms))}

    def go_file_decls(self, fm: dict, name: str) -> list[dict]:
        return [
            ch
            for ch in self.children.get(fm["id"], ())
            if ch.get("name") == name and (ch["kind"] in CALLABLE_KINDS or ch["kind"] in TYPE_KINDS) and ch["kind"] != "method"
        ]

    def go_dir_decls(self, directory: str, name: str, is_test: bool, pkg: str | None = None) -> list[dict]:
        """Named top-level Go declarations in the package files of ``directory`` (optionally one package only)."""
        out: list[dict] = []
        for p in self.dir_files.get(directory, ()):
            if not p.endswith(".go") or (p.endswith(GO_TEST_SUFFIX) and not is_test):
                continue
            fm = self.file_modsym.get(self.fid_of(p))
            if fm is None or (pkg is not None and (fm.get("bindings") or {}).get("package") != pkg):
                continue
            out.extend(self.go_file_decls(fm, name))
        return out

    def prefer_non_signatures(self, syms: list[dict]) -> list[dict]:
        return [s for s in syms if not self.is_signature(s)] or syms

    def go_dir_member(self, directory: str, name: str, site: dict) -> set:
        found = self.go_dir_decls(directory, name, site["path"].endswith(GO_TEST_SUFFIX))
        found = self.prefer_non_signatures(found)
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
        segs = list(receiver.split("::")) + [name]
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
    def note_parser_origin(self, s: dict) -> None:
        pr = s.get("resolution")
        b = s.get("binding") if isinstance(s.get("binding"), dict) else {}
        meta = b.get("resolved") if isinstance(b.get("resolved"), dict) else None
        if meta is not None:
            if meta.get("parser"):
                self.parser_origin[s["id"]] = (pr, s.get("targetId"))
        elif pr in (UNSUPPORTED, DYNAMIC, EXTERNAL) or (pr == RESOLVED and b.get("iife") and s.get("targetId")):
            self.parser_origin[s["id"]] = (pr, s.get("targetId"))

    def store_site_resolution(self, s: dict) -> None:
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
        if not note:
            return
        if not isinstance(binding, dict):
            binding = s["binding"] = {}
        binding["resolved"] = {"note": note}
        if note in PARSER_NOTES:
            binding["resolved"]["parser"] = True

    def run(self) -> dict:
        index = self.index
        # Parser-origin results (unsupported / parser-marked dynamic / IIFE) are tracked so repeated passes are idempotent:
        # results this resolver wrote carry binding.resolved; the parser-origin ones are tagged ``parser: true``.
        self.parser_origin: dict[str, tuple[str, str | None]] = {}
        for s in self.sites:
            self.note_parser_origin(s)
        self.rust_init_if_needed()
        self.build_tables()
        for s in self.sites:
            self.store_site_resolution(s)
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

    @staticmethod
    def edge_site_targets(s: dict) -> list[str]:
        res = s["resolution"]
        if res not in (RESOLVED, CANDIDATE):
            return []
        return [s["targetId"]] if res == RESOLVED and s["targetId"] else list(s["candidateIds"])

    def edge_site_source(self, s: dict) -> str:
        b = s.get("binding") or {}
        if s["relation"] in MODULE_RELATIONS and b.get("specifier") is not None:
            return s["fileId"]
        source = s.get("callerId") if s.get("callerId") in self.sym else None
        if source is not None:
            return source
        m = self.mod_by_region.get((s["fileId"], s.get("region", ""))) or self.file_modsym.get(s["fileId"])
        return m["id"] if m else s["fileId"]

    @staticmethod
    def add_edge_evidence(agg: dict[tuple, dict], order: list[tuple], s: dict, source: str, t: str) -> None:
        rel = s["relation"]
        res = s["resolution"]
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

    def collect_site_edges(self, s: dict, agg: dict[tuple, dict], order: list[tuple]) -> None:
        targets = self.edge_site_targets(s)
        if not targets:
            return
        source = self.edge_site_source(s)
        for t in targets:
            if t in self.sym or t in self.fid:
                self.add_edge_evidence(agg, order, s, source, t)

    def build_edges(self, previous: list[dict]) -> list[dict]:
        kept = [e for e in previous if not e.get("evidenceIds") and e.get("sourceId") and e.get("targetId")]
        agg: dict[tuple, dict] = {}
        order: list[tuple] = []
        for s in self.sites:
            self.collect_site_edges(s, agg, order)
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
