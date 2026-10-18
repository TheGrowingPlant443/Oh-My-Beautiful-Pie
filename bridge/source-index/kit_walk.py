"""Shared walker scaffolding for the Go/Rust/C-family/JVM/.NET recipes.

App-owned and parser-only: it sits on top of :class:`common.Context` (which owns
parsing, caps, IDs and sinks) and adds a dispatching depth-first visitor with
lexical scope enter/exit, deferred lexical declarations for closures bound to a
name, conditional-compilation depth and small text helpers.  Nothing here
resolves a target: every call/import fact is stored with raw spelling plus
parser-origin binding metadata, and the resolver decides what is provable.
"""
from __future__ import annotations

from typing import Any, Callable, Iterable

import common

RAW_LIMIT = 4096  # raw strings are kept verbatim up to this many characters


def clip(text: str) -> str:
    return text if len(text) <= RAW_LIMIT else text[:RAW_LIMIT]


def strip_quotes(text: str) -> str:
    """Remove one matching pair of surrounding quote characters, nothing else."""
    if len(text) >= 2 and text[0] == text[-1] and text[0] in "\"'`":
        return text[1:-1]
    return text


def named_children(node) -> list:
    return [c for c in node.children if c.is_named]


def child_of_type(node, *types: str):
    for c in node.children:
        if c.type in types:
            return c
    return None


def children_of_type(node, *types: str) -> list:
    return [c for c in node.children if c.type in types]


def fields_of(node, field: str) -> list:
    """All children carrying ``field`` (tree-sitter fields may repeat)."""
    out = []
    for i, c in enumerate(node.children):
        if node.field_name_for_child(i) == field:
            out.append(c)
    return out


def ancestor(node, types: Iterable[str]):
    types = set(types)
    p = node.parent
    while p is not None:
        if p.type in types:
            return p
        p = p.parent
    return None


def collect_identifiers(node, ident_types: frozenset[str], *, skip_fields: frozenset[str] = frozenset(),
                        skip_types: frozenset[str] = frozenset()) -> list:
    """Identifier leaves inside ``node`` (iterative; skips named fields/types)."""
    out = []
    stack = [node]
    while stack:
        n = stack.pop()
        if n.type in ident_types:
            out.append(n)
            continue
        if n.type in skip_types and n is not node:
            continue
        kids = n.children
        for i in range(len(kids) - 1, -1, -1):
            if skip_fields and n.field_name_for_child(i) in skip_fields:
                continue
            stack.append(kids[i])
    return out


class Visitor:
    """Dispatching visitor: ``visit_<node.type>(node)`` returns False to skip children."""

    #: node types that are closure literals (lexical declarations bound to a name pair with them)
    closure_types: frozenset[str] = frozenset()
    #: separator used for qualified names
    sep = "."

    def __init__(self, ctx: common.Context):
        self.ctx = ctx
        self._dispatch: dict[str, Any] = {}
        self._open: list[int] = []
        self._cond_open: list[int] = []
        self._pending: dict[int, tuple] = {}
        self.cond_depth = 0

    # -- driving ---------------------------------------------------------------
    def run(self, root) -> None:
        self.ctx.module_symbol(root)
        try:
            self.ctx.traverse(root, self.enter, self.leave)
        finally:
            self.flush_pending()
            while self._open:
                self._open.pop()
                self.ctx.pop_scope()
            self.ctx.pop_scope()  # module scope

    def enter(self, node):
        h = self._dispatch.get(node.type)
        if h is None:
            h = getattr(self, "visit_" + node.type, False)
            self._dispatch[node.type] = h
        if h is False:
            return None
        return h(node)

    def leave(self, node) -> None:
        if self._open and self._open[-1] == node.id:
            self._open.pop()
            self.ctx.pop_scope()
        if self._cond_open and self._cond_open[-1] == node.id:
            self._cond_open.pop()
            self.cond_depth -= 1

    # -- text -----------------------------------------------------------------
    def text(self, node) -> str:
        return self.ctx.text(node)

    def field(self, node, name: str):
        return node.child_by_field_name(name)

    def field_text(self, node, name: str) -> str | None:
        c = node.child_by_field_name(name)
        return None if c is None else self.ctx.text(c)

    # -- conditional compilation ------------------------------------------------
    def open_conditional(self, node) -> None:
        self.cond_depth += 1
        self._cond_open.append(node.id)

    # -- scopes / symbols -------------------------------------------------------
    def qual(self, name: str | None) -> str:
        parts = [s.name for s in self.ctx.scopes if s.name and s.kind != "module"]
        if name:
            parts.append(name)
        return self.sep.join(parts)

    def add_symbol(self, node, kind: str, name: str, *, node_kind: str | None = None,
                   qualified: str | None = None, bindings: dict | None = None,
                   exported: bool | None = None, owner=...,
                   span=None) -> dict:
        b = dict(bindings) if bindings else {}
        if self.cond_depth > 0:
            b["conditional"] = True
        sym = self.ctx.add_symbol(
            kind, name, span if span is not None else node,
            node_kind=node_kind if node_kind is not None else node.type,
            qualified=qualified if qualified is not None else self.qual(name),
            owner=owner, bindings=b or None, exported=exported,
        )
        pend = self._pending.pop(node.id, None)
        if pend is not None:
            pname, pkind, s, e, mutable, powner = pend
            self.ctx.declare(pname, pkind, s, e, symbol_id=sym["id"], mutable=mutable, owner=powner)
        return sym

    def open_symbol(self, node, kind: str, name: str, *, scope_name: str | None | type(...) = ...,
                    **kw) -> dict:
        """Create a symbol and make it the lexical owner until ``leave(node)``."""
        sym = self.add_symbol(node, kind, name, **kw)
        self.ctx.push_scope(sym["id"], name if scope_name is ... else scope_name, kind)
        self._open.append(node.id)
        return sym

    def open_lambda(self, node, *, bindings: dict | None = None, node_kind: str | None = None,
                    kind: str = "lambda") -> dict:
        """Open an anonymous callable scope.

        A closure directly bound to a declared name takes that local spelling as its
        ``name``; otherwise ``name`` is '' and only the qualified label carries the position.
        """
        line, col = node.start_point
        pend = self._pending.get(node.id)
        name = pend[0] if pend else ""
        scope_name = name or f"<anonymous@{line + 1}:{col}>"
        return self.open_symbol(node, kind, name, qualified=self.qual(scope_name), scope_name=scope_name,
                                bindings=bindings, node_kind=node_kind)

    # -- lexical declarations ----------------------------------------------------
    def declare(self, name: str, kind: str, start: int, end: int, *, symbol_id: str | None = None,
                site_id: str | None = None, mutable: bool = False, position: int | None = None,
                value=None, owner=...) -> None:
        """Declare ``name``; a closure-literal ``value`` pairs it with the closure symbol."""
        if not name:
            return
        if value is not None and value.type in self.closure_types and symbol_id is None:
            self._pending[value.id] = (name, kind, start, end, mutable, self.ctx.owner if owner is ... else owner)
            return
        self.ctx.declare(name, kind, start, end, symbol_id=symbol_id, site_id=site_id, mutable=mutable,
                         position=position, owner=owner)

    def flush_pending(self) -> None:
        for pname, pkind, s, e, mutable, powner in self._pending.values():
            self.ctx.declare(pname, pkind, s, e, mutable=mutable, owner=powner)
        self._pending.clear()

    # -- sites --------------------------------------------------------------------
    def site(self, relation: str, node, raw: str, binding: dict, *, dynamic: bool = False,
             resolution: str | None = None, type_only: bool = False, node_kind: str | None = None,
             caller=...) -> dict:
        raw_clipped = clip(raw)
        if raw_clipped != raw:
            binding = dict(binding)
            binding["rawTruncated"] = True
        return self.ctx.add_site(
            relation, raw_clipped, node, node_kind=node_kind, caller=caller,
            conditional=self.cond_depth > 0, type_only=type_only, dynamic=dynamic,
            resolution=resolution, binding=binding,
        )

    # -- ranges ---------------------------------------------------------------------
    def range_of_ancestor(self, node, types: Iterable[str]) -> tuple[int, int]:
        a = ancestor(node, types)
        if a is None:
            return 0, len(self.ctx.source)
        return a.start_byte, a.end_byte

    def end_of_ancestor(self, node, types: Iterable[str]) -> int:
        return self.range_of_ancestor(node, types)[1]


def set_module_bindings(ctx: common.Context, **values: Any) -> None:
    """Attach parser-origin facts (e.g. package name) to the region's module symbol."""
    sid = ctx._module_symbols.get(ctx.region)  # noqa: SLF001 - same-package app-owned helper
    if sid is None:
        return
    sym = ctx.symbol_by_id(sid)
    if sym is not None:
        sym.setdefault("bindings", {}).update(values)


def run_extract(ctx: common.Context, make_visitor: Callable[[common.Context], Visitor],
                post: Callable[[common.Context, Visitor, Any], None] | None = None) -> dict:
    """Parse with the file language, walk, and return ``ctx.result()``.

    Deadline / node-cap overruns keep the facts collected so far and mark the file partial.
    """
    tree = ctx.parse()
    root = tree.root_node
    visitor = make_visitor(ctx)
    try:
        visitor.run(root)
    except (common.DeadlineExceeded, common.NodeCapExceeded) as exc:
        ctx.mark_partial(f"extraction stopped early: {exc}")
    if root.has_error and not ctx.partial:
        # grammars can report hidden MISSING tokens that no visible node carries
        ctx.mark_partial("Tree-sitter reported a syntax error (missing token) that is not attached to a visible node")
    if post is not None:
        post(ctx, visitor, root)
    return ctx.result()
