"""Shared parser API for the trusted source-index worker.

Everything here is app-owned and never executes repository code: parsing is
Tree-sitter over bytes, grammar packages come from a fixed allowlist, and no
repository-provided module/callback names are ever imported.

Language modules (``lang_*.py`` / ``containers.py``) expose::

    PROFILES: dict[str, dict]   # language id -> {extensions, grammar, capsule, node_kinds}
    def extract(language: str, source: bytes, ctx: Context) -> dict

and use the :class:`Context` sink builders (``add_symbol``/``add_site``/``declare``)
and iterative ``traverse`` below.  ``extract`` returns ``ctx.result()``.

ID rule (``stable_id``): SHA-256 over the UTF-8 encoding of ``str(part)`` for each
part, each prefixed by its 8-byte big-endian byte length (unambiguous framing).
"""
from __future__ import annotations

import bisect
import os
import hashlib
import importlib
import importlib.metadata
import time
import warnings
from typing import Any, Callable, Iterable

SCHEMA_VERSION = 1
MAX_FILE_BYTES = 2 * 1024 * 1024
MAX_NODES = 2_000_000
FILE_SOFT_DEADLINE_SECONDS = 4.0
FILE_HARD_TIMEOUT_SECONDS = 5.0
MAX_DIAGNOSTICS_PER_FILE = 50
MAX_SYNTAX_ERRORS_REPORTED = 10


def extended_path(path: str) -> str:
    """Windows extended-length form of ``path``: snapshot roots live deep under the app config directory, so a long repository path exceeds MAX_PATH."""
    if os.name != "nt" or path.startswith("\\\\?\\"):
        return path
    absolute = os.path.abspath(path)
    if absolute.startswith("\\\\"):
        return "\\\\?\\UNC\\" + absolute[2:]
    return "\\\\?\\" + absolute


def plain_path(path: str) -> str:
    """Inverse of :func:`extended_path` for comparisons (``\\\\?\\C:\\x`` -> ``C:\\x``; UNC forms are left alone)."""
    if path.startswith("\\\\?\\") and not path.startswith("\\\\?\\UNC\\"):
        return path[4:]
    return path


# ---------------------------------------------------------------------------
# Grammar allowlist
# ---------------------------------------------------------------------------

# module -> (distribution name, allowed capsule attribute names)
GRAMMAR_ALLOWLIST: dict[str, tuple[str, tuple[str, ...]]] = {
    "tree_sitter_python": ("tree-sitter-python", ("language",)),
    "tree_sitter_javascript": ("tree-sitter-javascript", ("language",)),
    "tree_sitter_typescript": ("tree-sitter-typescript", ("language_typescript", "language_tsx")),
    "tree_sitter_go": ("tree-sitter-go", ("language",)),
    "tree_sitter_rust": ("tree-sitter-rust", ("language",)),
    "tree_sitter_java": ("tree-sitter-java", ("language",)),
    "tree_sitter_groovy": ("tree-sitter-groovy", ("language",)),
    "tree_sitter_c": ("tree-sitter-c", ("language",)),
    "tree_sitter_cpp": ("tree-sitter-cpp", ("language",)),
    "tree_sitter_ruby": ("tree-sitter-ruby", ("language",)),
    "tree_sitter_c_sharp": ("tree-sitter-c-sharp", ("language",)),
    "tree_sitter_kotlin": ("tree-sitter-kotlin", ("language",)),
    "tree_sitter_scala": ("tree-sitter-scala", ("language",)),
    "tree_sitter_php": ("tree-sitter-php", ("language_php",)),
    "tree_sitter_lua": ("tree-sitter-lua", ("language",)),
    "tree_sitter_swift": ("tree-sitter-swift", ("language",)),
    "tree_sitter_bash": ("tree-sitter-bash", ("language",)),
    "tree_sitter_powershell": ("tree-sitter-powershell", ("language",)),
    "tree_sitter_zig": ("tree-sitter-zig", ("language",)),
    "tree_sitter_elixir": ("tree-sitter-elixir", ("language",)),
    "tree_sitter_objc": ("tree-sitter-objc", ("language",)),
    "tree_sitter_julia": ("tree-sitter-julia", ("language",)),
    "tree_sitter_fortran": ("tree-sitter-fortran", ("language",)),
    "tree_sitter_verilog": ("tree-sitter-verilog", ("language",)),
}

# language id -> (grammar module, capsule).  Frozen; never repository-provided.
LANGUAGE_GRAMMARS: dict[str, tuple[str, str]] = {
    "python": ("tree_sitter_python", "language"),
    "javascript": ("tree_sitter_javascript", "language"),
    "typescript": ("tree_sitter_typescript", "language_typescript"),
    "tsx": ("tree_sitter_typescript", "language_tsx"),
    "go": ("tree_sitter_go", "language"),
    "rust": ("tree_sitter_rust", "language"),
    "java": ("tree_sitter_java", "language"),
    "groovy": ("tree_sitter_groovy", "language"),
    "c": ("tree_sitter_c", "language"),
    "cpp": ("tree_sitter_cpp", "language"),
    "ruby": ("tree_sitter_ruby", "language"),
    "csharp": ("tree_sitter_c_sharp", "language"),
    "kotlin": ("tree_sitter_kotlin", "language"),
    "scala": ("tree_sitter_scala", "language"),
    "php": ("tree_sitter_php", "language_php"),
    "lua": ("tree_sitter_lua", "language"),
    "swift": ("tree_sitter_swift", "language"),
    "bash": ("tree_sitter_bash", "language"),
    "powershell": ("tree_sitter_powershell", "language"),
    "zig": ("tree_sitter_zig", "language"),
    "elixir": ("tree_sitter_elixir", "language"),
    "objc": ("tree_sitter_objc", "language"),
    "julia": ("tree_sitter_julia", "language"),
    "fortran": ("tree_sitter_fortran", "language"),
    "verilog": ("tree_sitter_verilog", "language"),
}


class ExtractionError(Exception):
    """Base class for controlled extraction failures."""


class GrammarUnavailable(ExtractionError):
    """The grammar/runtime for a language is missing, not allowlisted or ABI-incompatible."""


class ParseTimeout(ExtractionError):
    """Tree-sitter did not return a tree before the deadline."""


class NodeCapExceeded(ExtractionError):
    """The syntax node cap was reached (facts collected so far remain valid)."""


class DeadlineExceeded(ExtractionError):
    """The cooperative per-file deadline passed (facts collected so far remain valid)."""


_LANGUAGE_CACHE: dict[tuple[str, str], Any] = {}
_LANGUAGE_ERRORS: dict[tuple[str, str], str] = {}


def tree_sitter_module():
    try:
        import tree_sitter  # type: ignore
    except Exception as exc:  # pragma: no cover - depends on runtime
        raise GrammarUnavailable(f"tree-sitter runtime unavailable: {exc}") from exc
    return tree_sitter


def grammar_version(module: str) -> str | None:
    spec = GRAMMAR_ALLOWLIST.get(module)
    if not spec:
        return None
    try:
        return importlib.metadata.version(spec[0])
    except Exception:
        return None


def load_language(language_id: str):
    """Load a Tree-sitter ``Language`` for a frozen language id (cached)."""
    spec = LANGUAGE_GRAMMARS.get(language_id)
    if spec is None:
        raise GrammarUnavailable(f"no grammar profile for language '{language_id}'")
    module, capsule = spec
    key = (module, capsule)
    if key in _LANGUAGE_CACHE:
        return _LANGUAGE_CACHE[key]
    if key in _LANGUAGE_ERRORS:
        raise GrammarUnavailable(_LANGUAGE_ERRORS[key])
    try:
        allowed = GRAMMAR_ALLOWLIST.get(module)
        if allowed is None or capsule not in allowed[1]:
            raise GrammarUnavailable(f"grammar {module}.{capsule} is not allowlisted")
        ts = tree_sitter_module()
        try:
            mod = importlib.import_module(module)
        except Exception as exc:
            raise GrammarUnavailable(f"grammar module {module} unavailable: {exc}") from exc
        fn = getattr(mod, capsule, None)
        if not callable(fn):
            raise GrammarUnavailable(f"grammar module {module} has no capsule {capsule}")
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            try:
                lang = ts.Language(fn())
            except Exception as exc:
                raise GrammarUnavailable(f"grammar {module}.{capsule} failed to load (ABI?): {exc}") from exc
            abi = getattr(lang, "abi_version", None)
        lo = getattr(ts, "MIN_COMPATIBLE_LANGUAGE_VERSION", 0)
        hi = getattr(ts, "LANGUAGE_VERSION", 10**9)
        if isinstance(abi, int) and not (lo <= abi <= hi):
            raise GrammarUnavailable(f"grammar {module} ABI {abi} outside runtime range {lo}..{hi}")
    except GrammarUnavailable as exc:
        _LANGUAGE_ERRORS[key] = str(exc)
        raise
    _LANGUAGE_CACHE[key] = lang
    return lang


def language_probe(language_id: str, required_kinds: Iterable[str]) -> dict:
    """Capability record for one profile: version, availability, supported node kinds."""
    spec = LANGUAGE_GRAMMARS.get(language_id)
    version = grammar_version(spec[0]) if spec else None
    try:
        lang = load_language(language_id)
    except GrammarUnavailable as exc:
        return {"language": language_id, "version": version, "available": False, "error": str(exc), "nodeKinds": []}
    present: list[str] = []
    missing: list[str] = []
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        for kind in required_kinds:
            found = lang.id_for_node_kind(kind, True) or lang.id_for_node_kind(kind, False)
            (present if found else missing).append(kind)
    error = None
    if missing:
        error = "degraded: grammar lacks required node kinds: " + ", ".join(missing)
    return {"language": language_id, "version": version, "available": True, "error": error, "nodeKinds": present}


# ---------------------------------------------------------------------------
# Stable IDs
# ---------------------------------------------------------------------------

def stable_id(*parts: Any) -> str:
    h = hashlib.sha256()
    for part in parts:
        data = str(part).encode("utf-8", "surrogatepass")
        h.update(len(data).to_bytes(8, "big"))
        h.update(data)
    return h.hexdigest()


def file_id(path: str) -> str:
    return stable_id("file", path)


def symbol_id(path: str, language: str, region: str, node_kind: str, start: int, end: int, ordinal: int = 0) -> str:
    if ordinal:
        return stable_id(path, language, region, node_kind, start, end, ordinal)
    return stable_id(path, language, region, node_kind, start, end)


def site_id(path: str, language: str, region: str, relation: str, node_kind: str, start: int, end: int, ordinal: int = 0) -> str:
    return stable_id("site", path, language, region, relation, node_kind, start, end, ordinal)


# ---------------------------------------------------------------------------
# Context
# ---------------------------------------------------------------------------

CALLABLE_KINDS = frozenset({"function", "method", "constructor", "accessor", "lambda"})


class Scope:
    __slots__ = ("symbol_id", "name", "kind", "region")

    def __init__(self, symbol_id: str, name: str | None, kind: str, region: str):
        self.symbol_id = symbol_id
        self.name = name
        self.kind = kind
        self.region = region


class Context:
    """Per-file parse context: parsing, deadlines, IDs, spans, scopes and sinks.

    ``language`` is the file's profile id.  ``region`` is the current region name
    (``"main"`` unless a container opens regions); symbols/sites record the
    grammar language of the region in effect (``set_region(..., language=...)``).
    """

    def __init__(
        self,
        path: str,
        language: str,
        source: bytes,
        *,
        deadline: float | None = None,
        node_cap: int = MAX_NODES,
    ):
        self.path = path
        self.file_id = file_id(path)
        self.file_language = language
        self.language = language  # grammar/effective language of current region
        self.source = source
        self.node_cap = node_cap
        self.deadline = deadline  # absolute time.monotonic() value
        self.region = "main"
        self.nodes_visited = 0
        self.symbols: list[dict] = []
        self.sites: list[dict] = []
        self.diagnostics: list[str] = []
        self.syntax_errors: list[str] = []
        self.partial = False
        self.scopes: list[Scope] = []
        self._symbol_index: dict[str, dict] = {}
        self._ordinals: dict[tuple, int] = {}
        self._line_starts: list[int] | None = None
        self._declarations: dict[str, list[dict]] = {}
        self._module_symbols: dict[str, str] = {}

    # -- time / caps --------------------------------------------------------
    def check_deadline(self) -> None:
        if self.deadline is not None and time.monotonic() > self.deadline:
            raise DeadlineExceeded("per-file cooperative deadline exceeded")

    def tick(self) -> None:
        self.nodes_visited += 1
        if self.nodes_visited > self.node_cap:
            raise NodeCapExceeded(f"syntax node cap {self.node_cap} exceeded")
        if (self.nodes_visited & 0x3FF) == 0:
            self.check_deadline()

    # -- parsing ------------------------------------------------------------
    def parse(self, language: str | None = None, ranges: list[tuple[int, int]] | None = None):
        """Parse ``self.source`` with an allowlisted grammar.

        ``ranges`` (original byte ``(start, end)`` pairs, ascending, non-overlapping)
        parse only those regions of the *original* buffer, so every node offset,
        row and column is in original-file coordinates.
        """
        ts = tree_sitter_module()
        lang = load_language(language or self.language)
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            parser = ts.Parser(lang)
            if self.deadline is not None:
                remaining = self.deadline - time.monotonic()
                if remaining <= 0:
                    raise ParseTimeout("deadline passed before parse")
                parser.timeout_micros = max(1, int(remaining * 1_000_000))
            if ranges is not None:
                parser.included_ranges = [
                    ts.Range(self.point_tuple(s), self.point_tuple(e), s, e) for s, e in ranges
                ]
            tree = parser.parse(self.source)
        if tree is None:
            raise ParseTimeout("Tree-sitter parse timed out")
        return tree

    def point_tuple(self, byte: int):
        ts = tree_sitter_module()
        row, col = self.row_col(byte)
        return ts.Point(row, col)

    # -- spans --------------------------------------------------------------
    def _lines(self) -> list[int]:
        if self._line_starts is None:
            starts = [0]
            src = self.source
            i = src.find(b"\n")
            while i != -1:
                starts.append(i + 1)
                i = src.find(b"\n", i + 1)
            self._line_starts = starts
        return self._line_starts

    def row_col(self, byte: int) -> tuple[int, int]:
        starts = self._lines()
        row = bisect.bisect_right(starts, byte) - 1
        return row, byte - starts[row]

    def span(self, node_or_range) -> dict:
        """Span dict for a Node or an original ``(startByte, endByte)`` pair."""
        if isinstance(node_or_range, tuple):
            s, e = node_or_range
            sr, sc = self.row_col(s)
            er, ec = self.row_col(e)
        else:
            s, e = node_or_range.start_byte, node_or_range.end_byte
            sr, sc = node_or_range.start_point
            er, ec = node_or_range.end_point
        return {
            "byteStart": s,
            "byteEnd": e,
            "lineStart": sr + 1,
            "lineEnd": er + 1,
            "columnStart": sc,
            "columnEnd": ec,
        }

    def text(self, node) -> str:
        """Lossless-as-possible text of a node (invalid UTF-8 becomes U+FFFD)."""
        if node is None:
            return ""
        return self.source[node.start_byte:node.end_byte].decode("utf-8", "replace")

    def field_text(self, node, field: str) -> str | None:
        child = node.child_by_field_name(field)
        return None if child is None else self.text(child)

    # -- traversal ----------------------------------------------------------
    def traverse(
        self,
        root,
        enter: Callable[[Any], Any],
        leave: Callable[[Any], None] | None = None,
    ) -> None:
        """Iterative visit of every node (named and anonymous), depth first.

        ``enter(node)`` may return ``False`` to skip the node's children (it has
        handled them itself, typically via nested ``traverse`` calls).  ``leave``
        is called after the node's children (or immediately after a skipped
        ``enter``).  Visiting counts against the node cap and deadline; ERROR /
        MISSING nodes are recorded as syntax errors.
        """
        stack: list[tuple[Any, bool]] = [(root, False)]
        while stack:
            node, done = stack.pop()
            if done:
                if leave is not None:
                    leave(node)
                continue
            self.tick()
            if node.is_error or node.is_missing:
                self.note_syntax_error(node)
            descend = enter(node)
            if leave is not None:
                stack.append((node, True))
            if descend is False:
                continue
            kids = node.children
            for i in range(len(kids) - 1, -1, -1):
                stack.append((kids[i], False))

    def note_syntax_error(self, node) -> None:
        self.partial = True
        if len(self.syntax_errors) < MAX_SYNTAX_ERRORS_REPORTED:
            what = "missing" if node.is_missing else "error"
            row, col = node.start_point
            self.syntax_errors.append(f"syntax {what} node '{node.type}' at line {row + 1}, column {col}")
        elif len(self.syntax_errors) == MAX_SYNTAX_ERRORS_REPORTED:
            self.syntax_errors.append("further syntax errors omitted")

    # -- regions / scopes ----------------------------------------------------
    def set_region(self, name: str, language: str | None = None) -> None:
        """Switch the active region (and effective grammar language) for new facts."""
        self.region = name
        if language is not None:
            self.language = language

    @property
    def owner(self) -> str | None:
        return self.scopes[-1].symbol_id if self.scopes else None

    @property
    def current_scope(self) -> Scope | None:
        return self.scopes[-1] if self.scopes else None

    def push_scope(self, symbol_id: str, name: str | None, kind: str) -> None:
        self.scopes.append(Scope(symbol_id, name, kind, self.region))

    def pop_scope(self) -> None:
        self.scopes.pop()

    def qualified(self, name: str | None) -> str:
        parts = [s.name for s in self.scopes if s.name and s.kind != "module"]
        if name:
            parts.append(name)
        return ".".join(parts)

    def module_symbol(self, node_or_range, *, name: str | None = None, owner: str | None = None, node_kind: str = "module") -> dict:
        """Create (once per region) the synthetic module owner and enter its scope.

        The module symbol is a containment node only: facts never link to it as a callee.
        Returns the symbol dict; the caller must ``pop_scope`` when finished with the region.
        """
        existing = self._module_symbols.get(self.region)
        if existing is not None:
            sym = self._symbol_index[existing]
            self.push_scope(sym["id"], None, "module")
            return sym
        sym = self.add_symbol(
            "module",
            name if name is not None else self.region,
            node_or_range,
            node_kind=node_kind,
            owner=owner,
            qualified="" if name is None else name,
            push=True,
        )
        self._module_symbols[self.region] = sym["id"]
        return sym

    # -- sinks ----------------------------------------------------------------
    def _ordinal(self, key: tuple) -> int:
        n = self._ordinals.get(key, 0)
        self._ordinals[key] = n + 1
        return n

    def add_symbol(
        self,
        kind: str,
        name: str,
        node_or_range,
        *,
        node_kind: str | None = None,
        qualified: str | None = None,
        owner: str | None = ...,  # type: ignore[assignment]
        bindings: dict | None = None,
        exported: bool | None = None,
        push: bool = False,
    ) -> dict:
        span = self.span(node_or_range)
        nk = node_kind if node_kind is not None else (getattr(node_or_range, "type", None) or kind)
        start, end = span["byteStart"], span["byteEnd"]
        ordinal = self._ordinal(("sym", self.language, self.region, nk, start, end))
        sid = symbol_id(self.path, self.language, self.region, nk, start, end, ordinal)
        owner_id = self.owner if owner is ... else owner
        sym: dict = {
            "id": sid,
            "fileId": self.file_id,
            "path": self.path,
            "language": self.language,
            "region": self.region,
            "ownerId": owner_id,
            "kind": kind,
            "name": name,
            "qualifiedName": qualified if qualified is not None else self.qualified(name),
            "span": span,
        }
        if bindings is not None:
            sym["bindings"] = bindings
        if exported is not None:
            sym["exported"] = exported
        self.symbols.append(sym)
        self._symbol_index[sid] = sym
        if push:
            self.push_scope(sid, name, kind)
        return sym

    def symbol_by_id(self, sid: str) -> dict | None:
        return self._symbol_index.get(sid)

    def add_site(
        self,
        relation: str,
        raw: str,
        node_or_range,
        *,
        node_kind: str | None = None,
        caller: str | None = ...,  # type: ignore[assignment]
        conditional: bool = False,
        type_only: bool = False,
        dynamic: bool = False,
        resolution: str | None = None,
        binding: dict | None = None,
    ) -> dict:
        """Record a recognized call/import/reference site.  Never promoted by name."""
        span = self.span(node_or_range)
        nk = node_kind if node_kind is not None else (getattr(node_or_range, "type", None) or relation)
        start, end = span["byteStart"], span["byteEnd"]
        ordinal = self._ordinal(("site", self.language, self.region, relation, nk, start, end))
        sid = site_id(self.path, self.language, self.region, relation, nk, start, end, ordinal)
        site: dict = {
            "id": sid,
            "fileId": self.file_id,
            "path": self.path,
            "language": self.language,
            "region": self.region,
            "callerId": self.owner if caller is ... else caller,
            "span": span,
            "relation": relation,
            "raw": raw,
            "targetId": None,
            "resolution": resolution or ("dynamic" if dynamic else "unresolved"),
            "conditional": bool(conditional),
            "typeOnly": bool(type_only),
            "candidateIds": [],
        }
        if binding is not None:
            site["binding"] = binding
        self.sites.append(site)
        return site

    def declare(
        self,
        name: str,
        kind: str,
        scope_start: int,
        scope_end: int,
        *,
        symbol_id: str | None = None,
        site_id: str | None = None,
        mutable: bool = False,
        position: int | None = None,
        owner: str | None = ...,  # type: ignore[assignment]
    ) -> None:
        """Record a lexical declaration visible in ``[scope_start, scope_end)``.

        Stored on the owning scope symbol's ``bindings.declarations`` (see
        ``finalize``).  ``kind`` is one of: function, class, import, variable,
        parameter, catch, type, enum, namespace, assignment, global.  The
        resolver finds the narrowest declaration range containing a site start,
        walking ``ownerId`` outward; ``assignment`` records mark rebinding.
        """
        owner_id = self.owner if owner is ... else owner
        if owner_id is None:
            return
        self._declarations.setdefault(owner_id, []).append(
            {
                "name": name,
                "kind": kind,
                "symbolId": symbol_id,
                "siteId": site_id,
                "mutable": bool(mutable),
                "position": scope_start if position is None else position,
                "scopeStart": scope_start,
                "scopeEnd": scope_end,
            }
        )

    def add_export(self, exported: str, *, local: str | None = None, symbol_id: str | None = None, default: bool = False, type_only: bool = False, module_id: str | None = None, **extra: Any) -> None:
        """Record a local export (not a re-export) on the region's module symbol."""
        mid = module_id or self._module_symbols.get(self.region)
        if mid is None:
            return
        sym = self._symbol_index[mid]
        bindings = sym.setdefault("bindings", {})
        entry = {"exported": exported, "local": local, "symbolId": symbol_id, "default": bool(default), "typeOnly": bool(type_only)}
        entry.update(extra)
        bindings.setdefault("exports", []).append(entry)

    def diag(self, message: str) -> None:
        if len(self.diagnostics) < MAX_DIAGNOSTICS_PER_FILE:
            self.diagnostics.append(message)
        elif len(self.diagnostics) == MAX_DIAGNOSTICS_PER_FILE:
            self.diagnostics.append("further diagnostics omitted")

    def mark_partial(self, reason: str) -> None:
        self.partial = True
        self.diag(reason)

    def finalize(self) -> None:
        for owner_id, decls in self._declarations.items():
            sym = self._symbol_index.get(owner_id)
            if sym is None:
                continue
            sym.setdefault("bindings", {}).setdefault("declarations", []).extend(decls)
        self._declarations = {}

    def result(self) -> dict:
        self.finalize()
        diags = list(self.diagnostics)
        diags.extend(self.syntax_errors)
        return {
            "symbols": self.symbols,
            "sites": self.sites,
            "diagnostics": diags,
            "partial": bool(self.partial),
        }
