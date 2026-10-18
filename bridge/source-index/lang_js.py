"""JavaScript / TypeScript / TSX extractor (pure Tree-sitter syntax recipes).

Also exposes :func:`run_region` so ``containers.py`` can extract embedded script
regions (Svelte/Vue/Astro/EJS) with original-file byte offsets.
"""
from __future__ import annotations

from typing import Any

import common

_JS_KINDS = [
    "function_declaration", "generator_function_declaration", "function_expression", "generator_function",
    "arrow_function", "method_definition", "class_declaration", "class", "class_static_block",
    "call_expression", "new_expression", "import_statement", "export_statement", "variable_declarator",
    "lexical_declaration", "variable_declaration",
    "assignment_expression", "decorator", "namespace_import", "import_specifier", "export_specifier",
]
_JSX_KINDS = ["jsx_opening_element", "jsx_self_closing_element"]
_TS_KINDS = _JS_KINDS + [
    "abstract_class_declaration", "interface_declaration", "enum_declaration", "function_signature",
    "method_signature", "abstract_method_signature", "type_alias_declaration", "internal_module", "module",
    "ambient_declaration", "import_require_clause", "public_field_definition", "construct_signature",
    "call_signature",
]

PROFILES: dict[str, dict] = {
    "javascript": {
        "extensions": [".js", ".jsx", ".mjs", ".cjs"],
        "grammar": "tree_sitter_javascript",
        "capsule": "language",
        "node_kinds": _JS_KINDS + _JSX_KINDS,
    },
    "typescript": {
        "extensions": [".ts", ".mts", ".cts"],
        "grammar": "tree_sitter_typescript",
        "capsule": "language_typescript",
        "node_kinds": _TS_KINDS,
    },
    "tsx": {
        "extensions": [".tsx"],
        "grammar": "tree_sitter_typescript",
        "capsule": "language_tsx",
        "node_kinds": _TS_KINDS + _JSX_KINDS,
    },
}

_FUNC_EXPR = {"function_expression", "function", "generator_function", "arrow_function"}
_FUNC_DECL = {"function_declaration", "generator_function_declaration"}
_SIGNATURES = {"function_signature", "method_signature", "abstract_method_signature", "construct_signature", "call_signature"}
_FUNCS = _FUNC_EXPR | _FUNC_DECL | _SIGNATURES | {"method_definition", "class_static_block"}
_CLASSES = {"class_declaration", "class", "abstract_class_declaration"}
_WRAPPERS = {"parenthesized_expression", "as_expression", "satisfies_expression", "non_null_expression", "type_assertion"}
_BLOCKS = {"statement_block", "for_statement", "for_in_statement", "switch_body", "catch_clause", "class_static_block"}
_TYPE_CONTEXT = {
    "type_annotation", "type_query", "type_alias_declaration", "type_arguments", "type_assertion", "as_expression",
    "satisfies_expression", "extends_type_clause", "implements_clause", "constraint", "default_type", "index_type_query",
}
_NAME_TYPES = {"identifier", "property_identifier", "type_identifier", "private_property_identifier", "shorthand_property_identifier"}
_LOGICAL = {"&&", "||", "??"}
_LOGICAL_ASSIGN = {"&&=", "||=", "??="}


def extract(language: str, source: bytes, ctx: common.Context) -> dict:
    tree = ctx.parse(language)
    run_region(ctx, language, tree, region="main", module_range=(0, len(source)))
    return ctx.result()


def run_region(ctx: common.Context, language: str, tree: Any, *, region: str, module_range: tuple[int, int], owner: str | None = None) -> str:
    """Extract one parsed region.  Returns the region's module symbol id."""
    ctx.set_region(region, language)
    module = ctx.module_symbol(module_range, owner=owner, node_kind="program")
    walker = _Walker(ctx, module_range)
    try:
        ctx.traverse(tree.root_node, walker.enter, walker.leave)
    finally:
        ctx.pop_scope()
    return module["id"]


def _unwrap(node):
    while node is not None and node.type in _WRAPPERS:
        inner = None
        if node.type == "parenthesized_expression" or node.type == "non_null_expression":
            kids = node.named_children
            inner = kids[0] if kids else None
        elif node.type == "type_assertion":
            kids = node.named_children
            inner = kids[-1] if kids else None
        else:
            kids = node.named_children
            inner = kids[0] if kids else None
        if inner is None:
            return node
        node = inner
    return node


class _CtxView:
    """Delegating view of Context that records ES exports only at module scope
    (an `export` inside a namespace/declare-module body is a namespace member)."""

    __slots__ = ("_c",)

    def __init__(self, ctx: common.Context):
        self._c = ctx

    def __getattr__(self, name: str):
        return getattr(self._c, name)

    def add_export(self, *args, **kwargs) -> None:
        scope = self._c.current_scope
        if scope is None or scope.kind == "module":
            self._c.add_export(*args, **kwargs)


class _Walker:
    def __init__(self, ctx: common.Context, module_range: tuple[int, int]):
        self.ctx = _CtxView(ctx)
        self.pending: dict[int, dict] = {}
        self.undo: dict[int, list] = {}
        self.blocks: list[tuple[int, int]] = [module_range]
        self.funcs: list[tuple[int, int]] = [module_range]
        self.cond_ids: set[int] = set()
        self.cond = 0
        self.handled: set[int] = set()
        self.export_decl: dict[int, list[tuple[str, bool]]] = {}
        self.dispatch = self._build_dispatch()

    # -- helpers ----------------------------------------------------------
    def _build_dispatch(self):
        d = {}
        for t in _FUNC_DECL | _FUNC_EXPR | {"method_definition", "function_signature", "method_signature", "abstract_method_signature", "construct_signature", "call_signature", "class_static_block"}:
            d[t] = self._function
        for t in _CLASSES:
            d[t] = self._class
        d["interface_declaration"] = self._interface
        d["enum_declaration"] = self._enum
        d["type_alias_declaration"] = self._type_alias
        d["internal_module"] = self._namespace
        d["module"] = self._namespace
        d["import_statement"] = self._import
        d["import_alias"] = self._import_alias
        d["export_statement"] = self._export
        d["variable_declarator"] = self._declarator
        d["call_expression"] = self._call
        d["new_expression"] = self._new
        d["assignment_expression"] = self._assignment
        d["augmented_assignment_expression"] = self._augmented
        d["update_expression"] = self._update
        d["jsx_opening_element"] = self._jsx
        d["jsx_self_closing_element"] = self._jsx
        d["decorator"] = self._decorator
        d["for_in_statement"] = self._for_in
        d["catch_clause"] = self._catch
        d["field_definition"] = self._field
        d["public_field_definition"] = self._field
        d["pair"] = self._pair
        return d

    def _on_leave(self, node, fn) -> None:
        self.undo.setdefault(node.id, []).append(fn)

    def text(self, node) -> str:
        return self.ctx.text(node)

    def string_value(self, node) -> str | None:
        """Literal string body (raw, no escape processing) or ``None`` when non-literal."""
        if node is None:
            return None
        if node.type == "string":
            raw = self.ctx.source[node.start_byte + 1:node.end_byte - 1]
            return raw.decode("utf-8", "replace")
        if node.type == "template_string":
            for ch in node.children:
                if ch.type == "template_substitution":
                    return None
            raw = self.ctx.source[node.start_byte + 1:node.end_byte - 1]
            return raw.decode("utf-8", "replace")
        return None

    def prop_name(self, node) -> tuple[str, bool]:
        if node is None:
            return "", False
        if node.type in _NAME_TYPES:
            return self.text(node), False
        if node.type == "string":
            return self.string_value(node) or "", False
        if node.type == "number":
            return self.text(node), False
        if node.type == "computed_property_name":
            return self.text(node), True
        return self.text(node), True

    def pattern_names(self, node) -> list[tuple[str, Any]]:
        out: list[tuple[str, Any]] = []
        if node is None:
            return out
        stack = [node]
        while stack:
            n = stack.pop()
            t = n.type
            if t in ("identifier", "shorthand_property_identifier_pattern", "shorthand_property_identifier"):
                name = self.text(n)
                if name != "this":
                    out.append((name, n))
            elif t == "pair_pattern":
                v = n.child_by_field_name("value")
                if v is not None:
                    stack.append(v)
            elif t == "assignment_pattern":
                v = n.child_by_field_name("left")
                if v is not None:
                    stack.append(v)
            elif t == "object_assignment_pattern":
                v = n.child_by_field_name("left")
                if v is not None:
                    stack.append(v)
            elif t in ("rest_pattern", "object_pattern", "array_pattern"):
                stack.extend(reversed(n.named_children))
            elif t in ("required_parameter", "optional_parameter"):
                v = n.child_by_field_name("pattern")
                if v is not None:
                    stack.append(v)
            # member/subscript assignment targets declare nothing
        out.sort(key=lambda p: p[1].start_byte)
        return out

    def module_site(self, relation: str, raw: str, node, binding: dict, *, type_only: bool = False, conditional: bool = False, dynamic: bool = False):
        return self.ctx.add_site(relation, raw, node, conditional=conditional, type_only=type_only, dynamic=dynamic, binding=binding)

    @staticmethod
    def module_binding(kind: str, specifier: str | None, **kw) -> dict:
        b = {
            "kind": kind,
            "specifier": specifier,
            "imported": None,
            "alias": None,
            "local": None,
            "default": False,
            "namespace": False,
            "reexport": False,
            "typeOnly": False,
            "typeKind": None,
            "sideEffect": False,
        }
        b.update(kw)
        return b

    # -- traversal ----------------------------------------------------------
    def enter(self, node):
        nid = node.id
        if nid in self.cond_ids:
            self.cond += 1
            self._on_leave(node, self._dec_cond)
            self.cond_ids.discard(nid)
        t = node.type
        if t in _BLOCKS and t != "class_static_block":
            self.blocks.append((node.start_byte, node.end_byte))
            self._on_leave(node, self.blocks.pop)
        self._mark_conditional(node, t)
        if not node.is_named:
            return None
        handler = self.dispatch.get(t)
        if handler is not None:
            return handler(node)
        return None

    def _dec_cond(self) -> None:
        self.cond -= 1

    def leave(self, node) -> None:
        fns = self.undo.pop(node.id, None)
        if fns:
            for fn in reversed(fns):
                fn()

    def _mark(self, child) -> None:
        if child is not None:
            self.cond_ids.add(child.id)

    def _mark_conditional(self, node, t: str) -> None:
        if t == "if_statement":
            self._mark(node.child_by_field_name("consequence"))
            self._mark(node.child_by_field_name("alternative"))
        elif t == "ternary_expression":
            self._mark(node.child_by_field_name("consequence"))
            self._mark(node.child_by_field_name("alternative"))
        elif t == "binary_expression":
            op = node.child_by_field_name("operator")
            if op is not None and op.type in _LOGICAL:
                self._mark(node.child_by_field_name("right"))
        elif t == "augmented_assignment_expression":
            op = node.child_by_field_name("operator")
            if op is not None and op.type in _LOGICAL_ASSIGN:
                self._mark(node.child_by_field_name("right"))
        elif t in ("switch_case", "switch_default"):
            for ch in node.named_children:
                self._mark(ch)
        elif t in ("for_statement", "for_in_statement", "while_statement", "do_statement"):
            self._mark(node.child_by_field_name("body"))
        elif t == "catch_clause":
            self._mark(node.child_by_field_name("body"))

    # -- functions / classes ------------------------------------------------
    def _pending_for(self, node) -> dict:
        return self.pending.pop(node.id, None) or {}

    def _function(self, node):
        ctx = self.ctx
        t = node.type
        pend = self._pending_for(node)
        name_node = node.child_by_field_name("name")
        name, computed = ("", False)
        if t in _FUNC_DECL or t == "function_signature":
            name = self.text(name_node) if name_node is not None else ""
        elif t == "method_definition" or t in ("method_signature", "abstract_method_signature"):
            name, computed = self.prop_name(name_node)
        elif t == "construct_signature":
            name = "new"
        elif t in _FUNC_EXPR:
            name = self.text(name_node) if name_node is not None else ""
        has_body = node.child_by_field_name("body") is not None or t in _FUNC_EXPR or t == "class_static_block"
        anon_name = name or pend.get("name") or ""
        # kind
        kind = pend.get("kind")
        if t == "method_definition":
            kind = "method"
            if name == "constructor" and not computed:
                kind = "constructor"
            else:
                for ch in node.children:
                    if ch.type in ("get", "set") and not ch.is_named:
                        kind = "accessor"
                        break
                    if ch.id == (name_node.id if name_node is not None else -1):
                        break
        elif t in ("method_signature", "abstract_method_signature"):
            kind = "method"
        elif t in ("function_signature", "construct_signature", "call_signature"):
            kind = "signature"
        elif t in _FUNC_DECL:
            kind = "function"
        elif t in _FUNC_EXPR:
            kind = kind or ("function" if (name_node is not None or pend.get("name")) else "lambda")
        elif t == "class_static_block":
            kind = "function"
        label = anon_name
        row, col = node.start_point
        scope_label = label or f"<anonymous@{row + 1}:{col}>"
        exports = list(self.export_decl.pop(node.id, ())) + list(pend.get("exports", ()))
        bindings: dict[str, Any] = {"body": bool(has_body)}
        if exports:
            bindings["exportNames"] = [e for e, _ in exports]
        if t == "method_definition" or t in ("method_signature", "abstract_method_signature"):
            bindings["static"] = any(ch.type == "static" and not ch.is_named for ch in node.children)
            if computed:
                bindings["computedName"] = True
        if pend.get("receiver"):
            bindings["receiver"] = pend["receiver"]
        if t == "arrow_function":
            bindings["arrow"] = True
        sym = ctx.add_symbol(
            kind or "function",
            label,
            node,
            node_kind=t,
            qualified=ctx.qualified(scope_label),
            bindings=bindings,
            exported=bool(exports) or None,
        )
        sid = sym["id"]
        iife = pend.get("iife")
        if iife is not None:
            iife["targetId"] = sid
            iife["resolution"] = "resolvedStatic"
        # declarations in the *enclosing* scope
        if t in _FUNC_DECL or t == "function_signature":
            if name:
                ctx.declare(name, "function", *self.blocks[-1], symbol_id=sid)
        declare = pend.get("declare")
        if declare is not None:
            ctx.declare(declare["name"], "variable", declare["start"], declare["end"], symbol_id=sid, mutable=declare["mutable"])
        local = name or pend.get("name")
        for exported, is_default in exports:
            ctx.add_export(exported, local=local or None, symbol_id=sid, default=is_default)
        # enter the function scope
        ctx.push_scope(sid, scope_label, kind or "function")
        self._on_leave(node, ctx.pop_scope)
        rng = (node.start_byte, node.end_byte)
        self.funcs.append(rng)
        self._on_leave(node, self.funcs.pop)
        self.blocks.append(rng)
        self._on_leave(node, self.blocks.pop)
        saved = self.cond
        self.cond = 0
        self._on_leave(node, lambda s=saved: setattr(self, "cond", s))
        if t in _FUNC_EXPR and name_node is not None:
            ctx.declare(name, "function", *rng, symbol_id=sid)
        # parameters
        params = node.child_by_field_name("parameters")
        if params is not None:
            for ch in params.named_children:
                for pname, pnode in self.pattern_names(ch):
                    ctx.declare(pname, "parameter", *rng, mutable=True, position=pnode.start_byte)
        else:
            single = node.child_by_field_name("parameter")
            if single is not None:
                for pname, pnode in self.pattern_names(single):
                    ctx.declare(pname, "parameter", *rng, mutable=True, position=pnode.start_byte)
        return None

    def _class(self, node):
        ctx = self.ctx
        pend = self._pending_for(node)
        name_node = node.child_by_field_name("name")
        name = self.text(name_node) if name_node is not None else ""
        label = name or pend.get("name") or ""
        row, col = node.start_point
        scope_label = label or f"<anonymous@{row + 1}:{col}>"
        exports = list(self.export_decl.pop(node.id, ())) + list(pend.get("exports", ()))
        extends: list[str] = []
        implements: list[str] = []
        ext_nodes = []
        for ch in node.children:
            if ch.type == "class_heritage":
                saw_ext = False
                for h in ch.children:
                    if h.type == "extends_clause":
                        for v in h.named_children:
                            if v.type != "type_arguments":
                                extends.append(self.text(v))
                                ext_nodes.append(v)
                    elif h.type == "implements_clause":
                        implements.extend(self.text(v) for v in h.named_children)
                    elif h.type == "extends" and not h.is_named:
                        saw_ext = True
                    elif saw_ext and h.is_named:
                        extends.append(self.text(h))
                        ext_nodes.append(h)
                        saw_ext = False
        bindings: dict[str, Any] = {"extends": extends, "implements": implements}
        if exports:
            bindings["exportNames"] = [e for e, _ in exports]
        if node.type == "abstract_class_declaration":
            bindings["abstract"] = True
        sym = ctx.add_symbol("class", label, node, node_kind=node.type, qualified=ctx.qualified(scope_label), bindings=bindings, exported=bool(exports) or None)
        sid = sym["id"]
        if node.type != "class":
            if name:
                ctx.declare(name, "class", *self.blocks[-1], symbol_id=sid)
        declare = pend.get("declare")
        if declare is not None:
            ctx.declare(declare["name"], "variable", declare["start"], declare["end"], symbol_id=sid, mutable=declare["mutable"])
        local = name or pend.get("name")
        for exported, is_default in exports:
            ctx.add_export(exported, local=local or None, symbol_id=sid, default=is_default)
        for ext in ext_nodes:
            self._reference_site(ext, "extends")
        ctx.push_scope(sid, scope_label, "class")
        self._on_leave(node, ctx.pop_scope)
        rng = (node.start_byte, node.end_byte)
        self.blocks.append(rng)
        self._on_leave(node, self.blocks.pop)
        if node.type == "class" and name:
            ctx.declare(name, "class", *rng, symbol_id=sid)
        return None

    def _reference_site(self, node, role: str):
        name, receiver, rkind, kind = self._callee_parts(node)
        raw = self.text(node)
        binding = {"kind": kind, "name": name, "receiver": receiver, "receiverKind": rkind, "role": role}
        self.ctx.add_site("references", raw, node, binding=binding, dynamic=kind == "dynamic")

    def _container(self, node, kind: str, node_kind: str | None = None, push: bool = True, declare_kind: str | None = None):
        ctx = self.ctx
        name_node = node.child_by_field_name("name")
        name = self.text(name_node) if name_node is not None else ""
        row, col = node.start_point
        exports = list(self.export_decl.pop(node.id, ()))
        bindings: dict[str, Any] = {}
        if exports:
            bindings["exportNames"] = [e for e, _ in exports]
        label = name or f"<anonymous@{row + 1}:{col}>"
        sym = ctx.add_symbol(kind, name, node, node_kind=node_kind or node.type, qualified=ctx.qualified(label), bindings=bindings or None, exported=bool(exports) or None)
        if declare_kind and name:
            ctx.declare(name, declare_kind, *self.blocks[-1], symbol_id=sym["id"])
        for exported, is_default in exports:
            ctx.add_export(exported, local=name or None, symbol_id=sym["id"], default=is_default, type_only=kind in ("interface", "type"))
        if push:
            ctx.push_scope(sym["id"], label, kind)
            self._on_leave(node, ctx.pop_scope)
        return sym

    def _interface(self, node):
        sym = self._container(node, "interface", declare_kind="type")
        extends = []
        for ch in node.children:
            if ch.type == "extends_type_clause":
                extends.extend(self.text(v) for v in ch.named_children if v.type != "type_arguments")
        if extends:
            sym.setdefault("bindings", {})["extends"] = extends
        return None

    def _enum(self, node):
        self._container(node, "enum", declare_kind="enum")
        return None

    def _type_alias(self, node):
        self._container(node, "type", push=False, declare_kind="type")
        return None

    def _namespace(self, node):
        ctx = self.ctx
        before = len(ctx.symbols)
        self._container(node, "namespace", declare_kind="namespace")
        # a dotted namespace name (N.M) keeps its spelling in the symbol name
        sym = ctx.symbols[before]
        name_node = node.child_by_field_name("name")
        if name_node is not None:
            sym["name"] = self.text(name_node) if name_node.type != "string" else (self.string_value(name_node) or "")
        rng = (node.start_byte, node.end_byte)
        self.blocks.append(rng)
        self._on_leave(node, self.blocks.pop)
        return None

    # -- imports / exports ----------------------------------------------------
    def _is_type_only(self, node) -> bool:
        return any(ch.type == "type" and not ch.is_named for ch in node.children)

    def _import(self, node):
        ctx = self.ctx
        source = node.child_by_field_name("source")
        require_clause = None
        clause = None
        for ch in node.children:
            if ch.type == "import_clause":
                clause = ch
            elif ch.type == "import_require_clause":
                require_clause = ch
        stmt_type_only = self._is_type_only(node)
        scope = self.blocks[-1]
        if require_clause is not None:
            src = require_clause.child_by_field_name("source")
            spec = self.string_value(src)
            ident = next((c for c in require_clause.children if c.type == "identifier"), None)
            alias = self.text(ident) if ident is not None else None
            b = self.module_binding("require", spec, alias=alias, local=alias, namespace=False, typeOnly=stmt_type_only, typeKind="type" if stmt_type_only else None)
            site = self.module_site("imports", spec if spec is not None else self.text(src), node, b, type_only=stmt_type_only, dynamic=spec is None)
            if alias:
                ctx.declare(alias, "import", *scope, site_id=site["id"])
            return False
        spec = self.string_value(source)
        raw = spec if spec is not None else self.text(source)
        tk = "type" if stmt_type_only else None
        if clause is None:
            b = self.module_binding("import", spec, sideEffect=True)
            self.module_site("imports", raw, node, b, dynamic=spec is None)
            return False
        for ch in clause.named_children:
            if ch.type == "identifier":
                local = self.text(ch)
                b = self.module_binding("import", spec, alias=local, local=local, default=True, typeOnly=stmt_type_only, typeKind=tk)
                site = self.module_site("imports", raw, ch, b, type_only=stmt_type_only, dynamic=spec is None)
                ctx.declare(local, "import", *scope, site_id=site["id"])
            elif ch.type == "namespace_import":
                ident = next((c for c in ch.named_children if c.type == "identifier"), None)
                local = self.text(ident) if ident is not None else None
                b = self.module_binding("import", spec, alias=local, local=local, namespace=True, typeOnly=stmt_type_only, typeKind=tk)
                site = self.module_site("imports", raw, ch, b, type_only=stmt_type_only, dynamic=spec is None)
                if local:
                    ctx.declare(local, "import", *scope, site_id=site["id"])
            elif ch.type == "named_imports":
                for sp in ch.named_children:
                    if sp.type != "import_specifier":
                        continue
                    nm = sp.child_by_field_name("name")
                    al = sp.child_by_field_name("alias")
                    imported = self.text(nm) if nm is not None else None
                    if nm is not None and nm.type == "string":
                        imported = self.string_value(nm)
                    alias = self.text(al) if al is not None else None
                    local = alias or imported
                    sp_type = stmt_type_only or self._is_type_only(sp)
                    b = self.module_binding("import", spec, imported=imported, alias=alias, local=local, default=imported == "default", typeOnly=sp_type, typeKind="type" if sp_type else None)
                    site = self.module_site("imports", raw, sp, b, type_only=sp_type, dynamic=spec is None)
                    if local:
                        ctx.declare(local, "import", *scope, site_id=site["id"])
        return False

    def _import_alias(self, node):
        # TS `import a = N.b.c`
        ctx = self.ctx
        kids = node.named_children
        if len(kids) < 2:
            return None
        ident = kids[0]
        target = kids[-1]
        local = self.text(ident)
        name, receiver, rkind, kind = self._callee_parts(target)
        b = {"kind": "alias", "name": name, "receiver": receiver, "receiverKind": rkind, "alias": local, "local": local, "specifier": None}
        site = ctx.add_site("references", self.text(target), node, binding=b)
        ctx.declare(local, "import", *self.blocks[-1], site_id=site["id"])
        return False

    def _export(self, node):
        ctx = self.ctx
        source = node.child_by_field_name("source")
        spec = self.string_value(source) if source is not None else None
        type_only = self._is_type_only(node)
        tk = "type" if type_only else None
        raw_spec = spec if spec is not None else (self.text(source) if source is not None else None)
        decl = node.child_by_field_name("declaration")
        value = node.child_by_field_name("value")
        is_default = any(ch.type == "default" and not ch.is_named for ch in node.children)
        if decl is not None:
            dt = decl.type
            if dt in _CLASSES | _FUNC_DECL | _SIGNATURES | {"interface_declaration", "enum_declaration", "type_alias_declaration", "internal_module", "module"}:
                nm = decl.child_by_field_name("name")
                nm_text = self.text(nm) if nm is not None else None
                if is_default:
                    self.export_decl[decl.id] = [("default", True)]
                elif nm_text:
                    self.export_decl[decl.id] = [(nm_text, False)]
            elif dt in ("lexical_declaration", "variable_declaration"):
                for d in decl.named_children:
                    if d.type == "variable_declarator":
                        names = self.pattern_names(d.child_by_field_name("name"))
                        self.export_decl[d.id] = [(n, False) for n, _ in names]
            return None
        if value is not None:
            inner = _unwrap(value)
            if inner is not None and inner.type in _FUNC_EXPR | {"class"}:
                self.pending[inner.id] = {"exports": [("default", True)], "name": None}
            elif inner is not None and inner.type == "identifier":
                local = self.text(inner)
                ctx.add_export("default", local=local, default=True)
                b = self.module_binding("exportLocal", None, imported=local, alias="default", default=True, reexport=True)
                self.module_site("reexports", local, node, b)
            else:
                ctx.add_export("default", local=None, default=True)
            return None
        # export = foo
        eq = any(ch.type == "=" for ch in node.children)
        if eq:
            target = next((c for c in node.named_children), None)
            if target is not None:
                inner = _unwrap(target)
                if inner.type == "identifier":
                    local = self.text(inner)
                    ctx.add_export("default", local=local, default=True, exportEquals=True)
                    b = self.module_binding("exportLocal", None, imported=local, alias="default", default=True, reexport=True, exportEquals=True)
                    self.module_site("reexports", local, node, b)
                elif inner.type in _FUNC_EXPR | {"class"}:
                    self.pending[inner.id] = {"exports": [("default", True)], "name": None}
                else:
                    ctx.add_export("default", local=None, default=True, exportEquals=True)
            return None
        for ch in node.children:
            if ch.type == "export_clause":
                for sp in ch.named_children:
                    if sp.type != "export_specifier":
                        continue
                    nm = sp.child_by_field_name("name")
                    al = sp.child_by_field_name("alias")
                    imported = self.text(nm) if nm is not None else None
                    if nm is not None and nm.type == "string":
                        imported = self.string_value(nm)
                    exported = self.text(al) if al is not None else imported
                    if al is not None and al.type == "string":
                        exported = self.string_value(al)
                    sp_type = type_only or self._is_type_only(sp)
                    if source is not None:
                        b = self.module_binding("exportFrom", spec, imported=imported, alias=exported, reexport=True, default=exported == "default", typeOnly=sp_type, typeKind="type" if sp_type else None)
                        self.module_site("reexports", raw_spec or "", sp, b, type_only=sp_type, dynamic=spec is None)
                    else:
                        ctx.add_export(exported or "", local=imported, default=exported == "default", type_only=sp_type)
                        b = self.module_binding("exportLocal", None, imported=imported, alias=exported, reexport=True, default=exported == "default", typeOnly=sp_type, typeKind="type" if sp_type else None)
                        self.module_site("reexports", imported or "", sp, b, type_only=sp_type)
            elif ch.type == "namespace_export":
                ident = next((c for c in ch.named_children), None)
                alias = self.text(ident) if ident is not None else None
                b = self.module_binding("exportFrom", spec, alias=alias, namespace=True, reexport=True, typeOnly=type_only, typeKind=tk)
                self.module_site("reexports", raw_spec or "", ch, b, type_only=type_only, dynamic=spec is None)
            elif ch.type == "*" and not ch.is_named and source is not None:
                b = self.module_binding("exportFrom", spec, namespace=True, reexport=True, typeOnly=type_only, typeKind=tk)
                self.module_site("reexports", raw_spec or "", node, b, type_only=type_only, dynamic=spec is None)
        return None

    # -- declarations -------------------------------------------------------------
    def _decl_kind(self, declarator) -> str:
        parent = declarator.parent
        if parent is None:
            return "var"
        if parent.type == "variable_declaration":
            return "var"
        k = parent.child_by_field_name("kind")
        return self.text(k) if k is not None else "let"

    def _function_value(self, value):
        inner = _unwrap(value)
        if inner is not None and (inner.type in _FUNC_EXPR or inner.type == "class"):
            return inner
        return None

    def _require_call(self, value):
        """If ``value`` is ``require('x')``/``import('x')`` (optionally awaited / member-accessed),
        return ``(call_node, member_name_or_None, kind)``."""
        node = _unwrap(value)
        member = None
        if node is not None and node.type == "await_expression":
            kids = node.named_children
            node = _unwrap(kids[0]) if kids else None
        if node is not None and node.type == "member_expression":
            prop = node.child_by_field_name("property")
            obj = _unwrap(node.child_by_field_name("object"))
            if obj is not None and obj.type == "call_expression" and prop is not None:
                member = self.text(prop)
                node = obj
        if node is None or node.type != "call_expression":
            return None
        fn = _unwrap(node.child_by_field_name("function"))
        args = node.child_by_field_name("arguments")
        if fn is None or args is None or not args.named_children:
            return None
        if fn.type == "import":
            return node, member, "dynamicImport"
        if fn.type == "identifier" and self.text(fn) == "require":
            return node, member, "require"
        return None

    def _declarator(self, node):
        ctx = self.ctx
        name_node = node.child_by_field_name("name")
        value = node.child_by_field_name("value")
        kind = self._decl_kind(node)
        mutable = kind != "const"
        scope = self.blocks[-1] if kind != "var" else self.funcs[-1]
        names = self.pattern_names(name_node)
        exports = list(self.export_decl.pop(node.id, ()))
        fn_value = self._function_value(value) if value is not None else None
        req = self._require_call(value) if value is not None else None
        if req is not None and name_node is not None:
            call, member, rkind = req
            self.handled.add(call.id)
            args = call.child_by_field_name("arguments")
            first = args.named_children[0]
            spec = self.string_value(first)
            conditional = self.cond > 0 or rkind == "dynamicImport"
            raw = spec if spec is not None else self.text(first)
            dyn = spec is None
            if name_node.type == "identifier":
                local = self.text(name_node)
                b = self.module_binding(rkind, spec, imported=member, alias=local, local=local, namespace=member is None)
                site = ctx.add_site("imports", raw, node, conditional=conditional, dynamic=dyn, binding=b)
                ctx.declare(local, "import", *scope, site_id=site["id"], mutable=mutable)
                for e, d in exports:
                    ctx.add_export(e, local=local)
            elif name_node.type == "object_pattern":
                self._bind_object_pattern(name_node, spec, raw, dyn, rkind, conditional, scope, mutable, member, exports)
            else:
                for pname, pnode in names:
                    b = self.module_binding(rkind, spec, imported=None, alias=pname, local=pname, namespace=False)
                    b["destructured"] = True
                    site = ctx.add_site("imports", raw, pnode, conditional=conditional, dynamic=dyn, binding=b)
                    ctx.declare(pname, "import", *scope, site_id=site["id"], mutable=mutable)
                    for e, d in exports:
                        if e == pname:
                            ctx.add_export(e, local=pname)
            return None
        if name_node is not None and name_node.type == "identifier" and fn_value is not None:
            local = self.text(name_node)
            pend = self.pending.setdefault(fn_value.id, {})
            pend["name"] = local
            pend["declare"] = {"name": local, "mutable": mutable, "start": scope[0], "end": scope[1]}
            pend["exports"] = [(e, d) for e, d in exports]
            return None
        for pname, pnode in names:
            ctx.declare(pname, "variable", *scope, mutable=mutable, position=pnode.start_byte)
        for e, d in exports:
            ctx.add_export(e, local=e)
        return None

    def _bind_object_pattern(self, pattern, spec, raw, dyn, rkind, conditional, scope, mutable, member, exports):
        ctx = self.ctx
        for prop in pattern.named_children:
            t = prop.type
            imported = alias = None
            if t == "shorthand_property_identifier_pattern":
                imported = alias = self.text(prop)
            elif t == "object_assignment_pattern":
                left = prop.child_by_field_name("left")
                imported = alias = self.text(left) if left is not None else None
            elif t == "pair_pattern":
                key = prop.child_by_field_name("key")
                val = prop.child_by_field_name("value")
                imported = self.prop_name(key)[0]
                names = self.pattern_names(val)
                alias = names[0][0] if names else None
            elif t == "rest_pattern":
                names = self.pattern_names(prop)
                b = self.module_binding(rkind, spec, imported=None, alias=names[0][0] if names else None, local=names[0][0] if names else None, namespace=True)
                b["rest"] = True
                site = ctx.add_site("imports", raw, prop, conditional=conditional, dynamic=dyn, binding=b)
                for pname, _ in names:
                    ctx.declare(pname, "import", *scope, site_id=site["id"], mutable=mutable)
                continue
            else:
                continue
            if imported is None or alias is None:
                continue
            # `const {a} = require('x').y` selects member y then destructures a: not a direct export of x
            b = self.module_binding(rkind, spec, imported=imported if member is None else member, alias=alias if alias != imported else None, local=alias)
            if member is not None:
                b["memberPath"] = [member, imported]
            site = ctx.add_site("imports", raw, prop, conditional=conditional, dynamic=dyn, binding=b)
            ctx.declare(alias, "import", *scope, site_id=site["id"], mutable=mutable)
            for e, d in exports:
                if e == alias:
                    ctx.add_export(e, local=alias)

    def _for_in(self, node):
        left = node.child_by_field_name("left")
        kind_node = node.child_by_field_name("kind")
        if left is not None:
            kind = self.text(kind_node) if kind_node is not None else None
            rng = (node.start_byte, node.end_byte)
            if kind in ("let", "const"):
                for pname, pnode in self.pattern_names(left):
                    self.ctx.declare(pname, "variable", *rng, mutable=kind != "const", position=pnode.start_byte)
            elif kind == "var":
                for pname, pnode in self.pattern_names(left):
                    self.ctx.declare(pname, "variable", *self.funcs[-1], mutable=True, position=pnode.start_byte)
            else:
                for pname, pnode in self.pattern_names(left):
                    self.ctx.declare(pname, "assignment", pnode.start_byte, pnode.end_byte, mutable=True)
        return None

    def _catch(self, node):
        param = node.child_by_field_name("parameter")
        if param is not None:
            rng = (node.start_byte, node.end_byte)
            for pname, pnode in self.pattern_names(param):
                self.ctx.declare(pname, "catch", *rng, mutable=True, position=pnode.start_byte)
        return None

    def _field(self, node):
        name_node = node.child_by_field_name("name") or node.child_by_field_name("property")
        value = node.child_by_field_name("value")
        if value is not None:
            inner = self._function_value(value)
            if inner is not None:
                name, _computed = self.prop_name(name_node)
                self.pending.setdefault(inner.id, {}).update({"name": name, "kind": "method"})
        return None

    def _pair(self, node):
        value = node.child_by_field_name("value")
        if value is not None:
            inner = self._function_value(value)
            if inner is not None:
                name, _computed = self.prop_name(node.child_by_field_name("key"))
                self.pending.setdefault(inner.id, {}).setdefault("name", name)
        return None

    # -- assignments ---------------------------------------------------------------
    def _member_target(self, left) -> tuple[str | None, str | None]:
        """For `a.b` return (object_text, property_name)."""
        if left.type == "member_expression":
            obj = left.child_by_field_name("object")
            prop = left.child_by_field_name("property")
            if obj is not None and prop is not None:
                return self.text(obj), self.text(prop)
        return None, None

    def _assignment(self, node):
        ctx = self.ctx
        left = node.child_by_field_name("left")
        right = node.child_by_field_name("right")
        if left is None or right is None:
            return None
        lt = left.type
        if lt == "identifier":
            local = self.text(left)
            ctx.declare(local, "assignment", node.start_byte, node.end_byte, mutable=True)
            inner = self._function_value(right)
            if inner is not None:
                self.pending.setdefault(inner.id, {})["name"] = local
            return None
        if lt in ("object_pattern", "array_pattern"):
            for pname, pnode in self.pattern_names(left):
                ctx.declare(pname, "assignment", node.start_byte, node.end_byte, mutable=True)
            return None
        obj, prop = self._member_target(left)
        if prop is None:
            return None
        inner = self._function_value(right)
        is_module_exports = obj == "module" and prop == "exports"
        is_exports_prop = obj in ("exports", "module.exports")
        if is_module_exports:
            self._commonjs_module_exports(node, right)
            return None
        if is_exports_prop:
            if inner is not None:
                self.pending[inner.id] = {"name": prop, "exports": [(prop, False)], "receiver": obj, "kind": "function"}
                return None
            rt = _unwrap(right)
            local = self.text(rt) if rt is not None and rt.type == "identifier" else None
            ctx.add_export(prop, local=local, commonjs=True)
            b = self.module_binding("exportLocal", None, imported=local, alias=prop, reexport=True, commonjs=True)
            self.module_site("reexports", local or prop, node, b)
            return None
        if inner is not None:
            self.pending[inner.id] = {"name": prop, "receiver": obj, "kind": "function"}
        return None

    def _commonjs_module_exports(self, node, right) -> None:
        ctx = self.ctx
        rt = _unwrap(right)
        if rt is None:
            return
        if rt.type in _FUNC_EXPR | {"class"}:
            self.pending[rt.id] = {"exports": [("default", True)], "name": None, "kind": "function"}
            return
        if rt.type == "identifier":
            local = self.text(rt)
            ctx.add_export("default", local=local, default=True, commonjs=True)
            b = self.module_binding("exportLocal", None, imported=local, alias="default", default=True, reexport=True, commonjs=True)
            self.module_site("reexports", local, node, b)
            return
        req = self._require_call(rt)
        if req is not None and req[2] == "require" and req[1] is None:
            call = req[0]
            self.handled.add(call.id)
            first = call.child_by_field_name("arguments").named_children[0]
            spec = self.string_value(first)
            b = self.module_binding("exportFrom", spec, namespace=True, reexport=True, commonjs=True, alias="default")
            self.module_site("reexports", spec if spec is not None else self.text(first), node, b, dynamic=spec is None)
            return
        if rt.type == "object":
            for member in rt.named_children:
                mt = member.type
                if mt == "shorthand_property_identifier":
                    name = self.text(member)
                    ctx.add_export(name, local=name, commonjs=True)
                    b = self.module_binding("exportLocal", None, imported=name, alias=name, reexport=True, commonjs=True)
                    self.module_site("reexports", name, member, b)
                elif mt == "pair":
                    key = member.child_by_field_name("key")
                    val = member.child_by_field_name("value")
                    name, computed = self.prop_name(key)
                    inner = self._function_value(val) if val is not None else None
                    if computed:
                        continue
                    if inner is not None:
                        self.pending[inner.id] = {"name": name, "exports": [(name, False)], "kind": "function"}
                    else:
                        v = _unwrap(val) if val is not None else None
                        local = self.text(v) if v is not None and v.type == "identifier" else None
                        ctx.add_export(name, local=local, commonjs=True)
                        b = self.module_binding("exportLocal", None, imported=local, alias=name, reexport=True, commonjs=True)
                        self.module_site("reexports", local or name, member, b)
                elif mt == "method_definition":
                    name, computed = self.prop_name(member.child_by_field_name("name"))
                    if not computed:
                        self.pending[member.id] = {"exports": [(name, False)]}
            return
        ctx.add_export("default", local=None, default=True, commonjs=True)

    def _augmented(self, node):
        left = node.child_by_field_name("left")
        if left is not None and left.type == "identifier":
            self.ctx.declare(self.text(left), "assignment", node.start_byte, node.end_byte, mutable=True)
        return None

    def _update(self, node):
        arg = node.child_by_field_name("argument")
        if arg is not None and arg.type == "identifier":
            self.ctx.declare(self.text(arg), "assignment", node.start_byte, node.end_byte, mutable=True)
        return None

    # -- call-like sites -------------------------------------------------------------
    def _callee_parts(self, fn) -> tuple[str | None, str | None, str | None, str]:
        """(name, receiver, receiverKind, kind) for a callee expression node."""
        fn = _unwrap(fn)
        if fn is None:
            return None, None, None, "dynamic"
        t = fn.type
        if t == "identifier":
            return self.text(fn), None, None, "identifier"
        if t == "super":
            return "super", None, "super", "identifier"
        if t == "this":
            return "this", None, "this", "identifier"
        if t == "member_expression":
            obj = fn.child_by_field_name("object")
            prop = fn.child_by_field_name("property")
            name = self.text(prop) if prop is not None else None
            return name, (self.text(obj) if obj is not None else None), self._receiver_kind(obj), "member"
        if t == "subscript_expression":
            obj = fn.child_by_field_name("object")
            idx = fn.child_by_field_name("index")
            lit = self.string_value(idx) if idx is not None else None
            if lit is not None:
                return lit, (self.text(obj) if obj is not None else None), self._receiver_kind(obj), "member"
            return None, (self.text(obj) if obj is not None else None), self._receiver_kind(obj), "dynamic"
        if t == "nested_identifier":
            parts = self.text(fn).rsplit(".", 1)
            return parts[-1], (parts[0] if len(parts) > 1 else None), "path" if len(parts) > 1 else None, "member" if len(parts) > 1 else "identifier"
        if t == "type_identifier":
            return self.text(fn), None, None, "identifier"
        return None, None, None, "dynamic"

    def _receiver_kind(self, obj) -> str | None:
        obj = _unwrap(obj)
        if obj is None:
            return None
        if obj.type == "this":
            return "this"
        if obj.type == "super":
            return "super"
        if obj.type == "identifier":
            return "identifier"
        if obj.type == "member_expression":
            cur = obj
            while cur is not None and cur.type == "member_expression":
                cur = _unwrap(cur.child_by_field_name("object"))
            if cur is not None and cur.type in ("identifier", "this"):
                return "path"
        return "expression"

    def _chain_optional(self, node) -> bool:
        cur = node
        for _ in range(64):
            if cur is None:
                return False
            if any(ch.type == "optional_chain" or (ch.type == "?." and not ch.is_named) for ch in cur.children):
                return True
            if cur.type == "call_expression":
                cur = cur.child_by_field_name("function")
            elif cur.type in ("member_expression", "subscript_expression"):
                cur = cur.child_by_field_name("object")
            elif cur.type in ("non_null_expression", "parenthesized_expression"):
                kids = cur.named_children
                cur = kids[0] if kids else None
            else:
                return False
        return False

    def _in_type_position(self, node) -> str | None:
        cur = node.parent
        kind = None
        for _ in range(24):
            if cur is None:
                return None
            if cur.type == "type_query":
                kind = "typeof"
            if cur.type in _TYPE_CONTEXT:
                return kind or "type"
            if cur.type in ("statement_block", "program", "class_body"):
                return None
            cur = cur.parent
        return None

    def _call(self, node):
        ctx = self.ctx
        if node.id in self.handled:
            return None
        fn_raw = node.child_by_field_name("function")
        args = node.child_by_field_name("arguments")
        fn = _unwrap(fn_raw)
        optional = self._chain_optional(node)
        conditional = self.cond > 0 or optional
        if fn is not None and fn.type == "import":
            first = args.named_children[0] if args is not None and args.named_children else None
            spec = self.string_value(first) if first is not None else None
            raw = spec if spec is not None else (self.text(first) if first is not None else "")
            tpos = self._in_type_position(node)
            if tpos is not None:
                b = self.module_binding("import", spec, namespace=True, typeOnly=True, typeKind=tpos)
                self.module_site("imports", raw, node, b, type_only=True, dynamic=spec is None)
            else:
                b = self.module_binding("dynamicImport", spec, namespace=True)
                b["dynamic"] = spec is None
                self.module_site("imports", raw, node, b, conditional=True, dynamic=spec is None)
            return None
        if fn is not None and fn.type == "identifier" and args is not None and args.named_children and self.text(fn) == "require":
            first = args.named_children[0]
            spec = self.string_value(first)
            raw = spec if spec is not None else self.text(first)
            b = self.module_binding("require", spec, namespace=True)
            b["dynamic"] = spec is None
            self.module_site("imports", raw, node, b, conditional=conditional, dynamic=spec is None)
            return None
        raw = self.text(fn_raw)
        iife = None
        if fn is not None and fn.type in _FUNC_EXPR:
            iife = fn
        name, receiver, rkind, kind = self._callee_parts(fn_raw)
        binding = {"kind": kind, "name": name, "receiver": receiver, "receiverKind": rkind}
        if optional:
            binding["optional"] = True
        dynamic = kind == "dynamic"
        if dynamic:
            binding["dynamic"] = True
        if args is not None and args.type == "template_string":
            binding["tagged"] = True
        if iife is not None:
            binding = {"kind": "iife", "name": None, "receiver": None, "receiverKind": None, "iife": True}
            dynamic = False
        site = ctx.add_site("calls", raw, node, conditional=conditional, dynamic=dynamic, binding=binding)
        if iife is not None:
            self.pending.setdefault(iife.id, {})["iife"] = site
        return None

    def _new(self, node):
        ctx = self.ctx
        ctor = node.child_by_field_name("constructor")
        name, receiver, rkind, kind = self._callee_parts(ctor)
        binding = {"kind": "new", "name": name, "receiver": receiver, "receiverKind": rkind}
        dynamic = kind == "dynamic"
        if dynamic:
            binding["dynamic"] = True
        ctx.add_site("calls", self.text(ctor), node, conditional=self.cond > 0, dynamic=dynamic, binding=binding)
        return None

    def _jsx(self, node):
        ctx = self.ctx
        name_node = node.child_by_field_name("name")
        if name_node is None:
            return None
        t = name_node.type
        text = self.text(name_node)
        if t == "identifier" or t == "jsx_identifier":
            if not text[:1].isupper():
                return None  # intrinsic element: template coverage, not a function use
            binding = {"kind": "jsx", "name": text, "receiver": None, "receiverKind": None}
        elif t == "member_expression" or t == "nested_identifier":
            parts = text.rsplit(".", 1)
            binding = {"kind": "jsx", "name": parts[-1], "receiver": parts[0] if len(parts) > 1 else None, "receiverKind": "path"}
        else:
            return None  # namespaced intrinsic tags like svg:rect
        ctx.add_site("renders", text, node, conditional=self.cond > 0, binding=binding)
        return None

    def _decorator(self, node):
        kids = node.named_children
        if not kids:
            return None
        inner = kids[0]
        if inner.type == "call_expression":
            return None  # recorded by the call handler
        name, receiver, rkind, kind = self._callee_parts(inner)
        binding = {"kind": kind, "name": name, "receiver": receiver, "receiverKind": rkind, "decorator": True}
        self.ctx.add_site("calls", self.text(inner), node, binding=binding, dynamic=kind == "dynamic")
        return None
