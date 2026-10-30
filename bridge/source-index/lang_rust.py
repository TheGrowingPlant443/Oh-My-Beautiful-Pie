"""Rust syntax recipe (pure Tree-sitter AST facts).

Function items/signatures, impl/trait/mod containers, closures, ``use`` trees flattened
to one site per leaf (FULL path in ``specifier``), ``mod`` declarations, every call-like
expression, and ``macro_invocation`` as separate unexpanded ``macros`` facts.  Macro
token trees are not expanded, so calls hidden in macro arguments are not claimed.
"""
from __future__ import annotations

import re

import common
import kit_walk
from kit_walk import Visitor, child_of_type, collect_identifiers

PROFILES = {
    "rust": {
        "extensions": [".rs"],
        "grammar": "tree_sitter_rust",
        "capsule": "language",
        "node_kinds": [
            "function_item", "function_signature_item", "closure_expression", "use_declaration",
            "scoped_use_list", "use_as_clause", "use_wildcard", "mod_item", "impl_item", "trait_item",
            "call_expression", "macro_invocation", "field_expression", "scoped_identifier",
            "let_declaration", "extern_crate_declaration", "macro_definition",
        ],
    }
}

_ITEMS = frozenset({
    "function_item", "function_signature_item", "mod_item", "use_declaration", "impl_item", "trait_item",
    "struct_item", "enum_item", "union_item", "type_item", "const_item", "static_item",
    "extern_crate_declaration", "macro_definition",
})
_BLOCKS = ("block", "declaration_list", "source_file")
_IDENT = frozenset({"identifier"})
_CFG = re.compile(r"^#\s*\[\s*cfg\s*\(")
_RECEIVER_KINDS = {"self": "self", "identifier": "identifier", "scoped_identifier": "path"}


class RustVisitor(Visitor):
    closure_types = frozenset({"closure_expression"})
    sep = "::"

    # -- conditional compilation ----------------------------------------------
    def enter(self, node):
        if node.type in _ITEMS and self._has_cfg(node):
            self.open_conditional(node)
        return super().enter(node)

    def _attributes(self, node):
        prev = node.prev_sibling
        while prev is not None and prev.type in ("attribute_item", "line_comment", "block_comment", "inner_attribute_item"):
            if prev.type == "attribute_item":
                yield prev
            prev = prev.prev_sibling

    def _has_cfg(self, node) -> bool:
        return any(_CFG.match(self.text(a)) for a in self._attributes(node))

    def _path_attribute(self, node):
        for a in self._attributes(node):
            attr = child_of_type(a, "attribute")
            if attr is None:
                continue
            ident = child_of_type(attr, "identifier")
            value = attr.child_by_field_name("value")
            if ident is not None and self.text(ident) == "path" and value is not None:
                return kit_walk.strip_quotes(self.text(value))
        return None

    def _visibility(self, node) -> str:
        v = child_of_type(node, "visibility_modifier")
        return self.text(v) if v is not None else ""

    # -- use / extern crate / mod --------------------------------------------------
    def visit_use_declaration(self, node):
        arg = node.child_by_field_name("argument")
        if arg is None:
            return None
        vis = self._visibility(node)
        start, end = self.range_of_ancestor(node, _BLOCKS)
        for leaf, spec, alias, glob, self_import in self._use_leaves(arg, ""):
            self._declare_use_leaf(leaf, spec, alias, glob, self_import, vis, start, end)
        return False

    def _declare_use_leaf(self, leaf, spec, alias, glob, self_import, vis, start, end):
        imported = None if glob else (spec.rsplit("::", 1)[-1] or None)
        local = None
        if not glob:
            local = alias if alias is not None else imported
            if local == "_":
                local = None
        binding = {
            "kind": "use", "specifier": spec, "imported": imported, "alias": alias, "default": False,
            "namespace": glob, "reexport": bool(vis), "typeOnly": False, "typeKind": None,
            "local": local, "visibility": vis, "absolute": spec.startswith("::"),
        }
        if self_import:
            binding["selfImport"] = True
        site = self.site("reexports" if vis else "imports", leaf, spec, binding, node_kind=leaf.type)
        if local:
            self.ctx.declare(local, "import", start, end, site_id=site["id"])

    def _use_leaves(self, node, prefix: str, depth: int = 0):
        if depth > 64:
            self.ctx.mark_partial("use tree nesting exceeds 64 levels; deeper leaves not recorded")
            return
        t = node.type
        if t in ("identifier", "crate", "super", "self", "scoped_identifier", "metavariable"):
            if t == "self" and prefix:
                yield node, prefix, None, False, True
            else:
                yield node, _join_use(prefix, self.text(node)), None, False, False
            return
        if t == "use_as_clause":
            yield from self._aliased_use_leaves(node, prefix)
            return
        if t == "use_wildcard":
            kids = [c for c in node.children if c.is_named]
            path = _join_use(prefix, self.text(kids[0])) if kids else prefix
            yield node, path, None, True, False
            return
        if t == "scoped_use_list":
            path = node.child_by_field_name("path")
            new_prefix = _join_use(prefix, self.text(path)) if path is not None else prefix
            yield from self._use_list_leaves(node.child_by_field_name("list"), new_prefix, depth)
        elif t == "use_list":
            yield from self._use_list_leaves(node, prefix, depth)

    def _aliased_use_leaves(self, node, prefix: str):
        path = node.child_by_field_name("path")
        alias = node.child_by_field_name("alias")
        if path is None:
            return
        spec = prefix if (path.type == "self" and prefix) else _join_use(prefix, self.text(path))
        yield node, spec, self.text(alias) if alias is not None else None, False, path.type == "self"

    def _use_list_leaves(self, lst, prefix: str, depth: int):
        if lst is None:
            return
        for c in lst.children:
            if c.is_named:
                yield from self._use_leaves(c, prefix, depth + 1)

    def visit_extern_crate_declaration(self, node):
        name = node.child_by_field_name("name")
        if name is None:
            return None
        alias = node.child_by_field_name("alias")
        spec = self.text(name)
        a = self.text(alias) if alias is not None else None
        local = a if a is not None else spec
        binding = {"kind": "use", "specifier": spec, "imported": None, "alias": a, "default": False, "namespace": False,
                   "reexport": bool(self._visibility(node)), "typeOnly": False, "typeKind": None,
                   "local": None if local == "_" else local, "externCrate": True}
        site = self.site("imports", node, spec, binding)
        if binding["local"]:
            start, end = self.range_of_ancestor(node, _BLOCKS)
            self.ctx.declare(binding["local"], "import", start, end, site_id=site["id"])
        return None

    def visit_mod_item(self, node):
        name_node = node.child_by_field_name("name")
        if name_node is None:
            return None
        name = self.text(name_node)
        vis = self._visibility(node)
        body = node.child_by_field_name("body")
        start, end = self.range_of_ancestor(node, _BLOCKS)
        bindings = {"visibility": vis, "body": body is not None}
        if body is None:
            path = self._path_attribute(node)
            binding = {"kind": "mod", "specifier": name, "imported": None, "alias": None, "default": False,
                       "namespace": False, "reexport": bool(vis), "typeOnly": False, "typeKind": None,
                       "local": name, "path": path}
            site = self.site("imports", node, name, binding)
            self.add_symbol(node, "namespace", name, bindings=bindings, exported=bool(vis))
            self.ctx.declare(name, "namespace", start, end, site_id=site["id"])
            return None
        sym = self.open_symbol(node, "namespace", name, bindings=bindings, exported=bool(vis))
        self.ctx.declare(name, "namespace", start, end, symbol_id=sym["id"], owner=self._outer_owner())
        return None

    def _outer_owner(self):
        scopes = self.ctx.scopes
        return scopes[-2].symbol_id if len(scopes) >= 2 else None

    def visit_macro_definition(self, node):
        name = self.field_text(node, "name")
        if name:
            self.add_symbol(node, "macro", name, bindings={"body": True})
        return False  # macro rule bodies are token trees, not expanded

    # -- types ------------------------------------------------------------------------
    def _type_item(self, node, kind):
        name = self.field_text(node, "name")
        if not name:
            return
        vis = self._visibility(node)
        b = {"visibility": vis}
        if node.type == "union_item":
            b["union"] = True
        sym = self.add_symbol(node, kind, name, bindings=b, exported=bool(vis))
        start, end = self.range_of_ancestor(node, _BLOCKS)
        self.ctx.declare(name, "enum" if kind == "enum" else "type", start, end, symbol_id=sym["id"])

    def visit_struct_item(self, node):
        self._type_item(node, "struct")

    def visit_union_item(self, node):
        self._type_item(node, "struct")

    def visit_enum_item(self, node):
        self._type_item(node, "enum")

    def visit_type_item(self, node):
        self._type_item(node, "type")

    def visit_trait_item(self, node):
        name = self.field_text(node, "name")
        if not name:
            return None
        vis = self._visibility(node)
        sym = self.open_symbol(node, "trait", name, bindings={"visibility": vis}, exported=bool(vis))
        start, end = self.range_of_ancestor(node, _BLOCKS)
        self.ctx.declare(name, "type", start, end, symbol_id=sym["id"], owner=self._outer_owner())
        return None

    def _base_type_name(self, t) -> str:
        if t is None:
            return ""
        if t.type == "generic_type":
            inner = t.child_by_field_name("type")
            if inner is not None:
                return self._base_type_name(inner)
        return self.text(t)

    def visit_impl_item(self, node):
        t = node.child_by_field_name("type")
        trait = node.child_by_field_name("trait")
        type_raw = self.text(t) if t is not None else ""
        trait_raw = self.text(trait) if trait is not None else None
        name = self._base_type_name(t)
        label = f"impl {trait_raw} for {type_raw}" if trait_raw else f"impl {type_raw}"
        self.open_symbol(node, "impl", name, qualified=self.qual(label), scope_name=name or label,
                         bindings={"implFor": type_raw, "implTrait": trait_raw})
        return None

    # -- callables ----------------------------------------------------------------------
    def _container(self, node):
        p = node.parent
        if p is not None and p.type == "declaration_list" and p.parent is not None:
            return p.parent
        return None

    def _declare_params(self, plist, body):
        names: list[str] = []
        has_self = False
        if plist is None:
            return names, has_self
        rng = (body.start_byte, body.end_byte) if body is not None else (plist.end_byte, plist.end_byte)
        for p in plist.children:
            if p.type == "self_parameter":
                has_self = True
                continue
            if p.type != "parameter":
                continue
            pat = p.child_by_field_name("pattern")
            if pat is None:
                continue
            for ident in collect_identifiers(pat, _IDENT, skip_fields=frozenset({"type"})):
                n = self.text(ident)
                names.append(n)
                self.ctx.declare(n, "parameter", rng[0], rng[1], position=ident.start_byte)
        return names, has_self

    def _fn(self, node, *, signature: bool):
        name = self.field_text(node, "name") or ""
        cont = self._container(node)
        vis = self._visibility(node)
        body = node.child_by_field_name("body")
        mods = child_of_type(node, "function_modifiers")
        mod_text = self.text(mods) if mods is not None else ""
        bindings = {
            "visibility": vis, "body": body is not None,
            "async": "async" in mod_text.split(), "unsafe": "unsafe" in mod_text.split(),
            "const": "const" in mod_text.split(), "abi": None,
        }
        exported: bool | None = bool(vis)
        kind = "signature" if signature else "function"
        in_impl_trait = False
        if cont is not None and cont.type in ("impl_item", "trait_item"):
            if not signature:
                kind = "method"
            in_impl_trait = self._impl_bindings(cont, bindings)
            if in_impl_trait or cont.type == "trait_item":
                exported = None
        elif cont is not None and cont.type == "foreign_mod_item":
            bindings["foreign"] = True
            bindings["abi"] = self._foreign_abi(cont)
        sym = self.open_symbol(node, kind, name, bindings=bindings, exported=exported)
        names, has_self = self._declare_params(node.child_by_field_name("parameters"), body)
        sym["bindings"]["parameters"] = names
        sym["bindings"]["static"] = kind == "method" and not has_self
        sym["bindings"]["hasSelf"] = has_self
        target = self._declaring_node(kind, node, cont)
        if target is not None:
            start, end = self.range_of_ancestor(target, _BLOCKS)
            self.ctx.declare(name, "function", start, end, symbol_id=sym["id"], owner=self._outer_owner())
        return None

    def _foreign_abi(self, cont):
        ext = child_of_type(cont, "extern_modifier")
        if ext is None:
            return None
        return kit_walk.strip_quotes(self.text(ext).replace("extern", "", 1).strip())

    @staticmethod
    def _declaring_node(kind, node, cont):
        """Node whose enclosing block scopes the function name; None when it is not a free name."""
        if kind == "function":
            return node
        if kind == "signature" and cont is not None and cont.type == "foreign_mod_item":
            return cont
        return None

    def _impl_bindings(self, cont, bindings) -> bool:
        """Record impl/trait container facts; True when inside ``impl Trait for T``."""
        if cont.type != "impl_item":
            bindings["traitMethod"] = True
            return False
        trait = cont.child_by_field_name("trait")
        t = cont.child_by_field_name("type")
        bindings["implFor"] = self.text(t) if t is not None else None
        bindings["implTrait"] = self.text(trait) if trait is not None else None
        return trait is not None

    def visit_function_item(self, node):
        return self._fn(node, signature=False)

    def visit_function_signature_item(self, node):
        return self._fn(node, signature=True)

    def visit_closure_expression(self, node):
        body = node.child_by_field_name("body")
        sym = self.open_lambda(node, bindings={"body": body is not None}, node_kind="closure_expression")
        params = node.child_by_field_name("parameters")
        names = []
        if params is not None:
            rng = (node.start_byte, node.end_byte)
            for ident in collect_identifiers(params, _IDENT, skip_fields=frozenset({"type"})):
                n = self.text(ident)
                names.append(n)
                self.ctx.declare(n, "parameter", rng[0], rng[1], position=ident.start_byte)
        sym["bindings"]["parameters"] = names
        return None

    # -- bindings -------------------------------------------------------------------------
    def _pattern_names(self, pat):
        return collect_identifiers(pat, _IDENT, skip_fields=frozenset({"type"}))

    def visit_let_declaration(self, node):
        pat = node.child_by_field_name("pattern")
        value = node.child_by_field_name("value")
        if pat is None:
            return None
        end = self.end_of_ancestor(node, _BLOCKS)
        ids = self._pattern_names(pat)
        for ident in ids:
            n = self.text(ident)
            direct = pat.type == "identifier" or (pat.type == "mut_pattern" and len(ids) == 1)
            self.declare(n, "variable", node.end_byte, end, mutable=True, position=ident.start_byte,
                         value=value if direct else None)
        return None

    def _scoped_patterns(self, pat_node, start, end, kind="variable"):
        if pat_node is None:
            return
        for ident in self._pattern_names(pat_node):
            self.ctx.declare(self.text(ident), kind, start, end, mutable=True, position=ident.start_byte)

    def visit_match_arm(self, node):
        self._scoped_patterns(node.child_by_field_name("pattern"), node.start_byte, node.end_byte)
        return None

    def visit_let_condition(self, node):
        p = node.parent
        while p is not None and p.type not in ("if_expression", "while_expression", "block", "source_file"):
            p = p.parent
        end = p.end_byte if p is not None else node.end_byte
        self._scoped_patterns(node.child_by_field_name("pattern"), node.end_byte, end)
        return None

    def visit_for_expression(self, node):
        return self.visit_match_arm(node)

    def _const_like(self, node):
        name = self.field_text(node, "name")
        if not name:
            return
        start, end = self.range_of_ancestor(node, _BLOCKS)
        cont = self._container(node)
        if cont is not None and cont.type in ("impl_item", "trait_item"):
            return
        self.declare(name, "variable", start, end, value=node.child_by_field_name("value"))

    def visit_const_item(self, node):
        self._const_like(node)

    def visit_static_item(self, node):
        self._const_like(node)

    def visit_assignment_expression(self, node):
        left = node.child_by_field_name("left")
        if left is not None and left.type == "identifier":
            end = self.end_of_ancestor(node, ("function_item", "closure_expression", "source_file"))
            self.ctx.declare(self.text(left), "assignment", node.start_byte, end, mutable=True, position=node.start_byte)
        return None

    # -- calls / macros ---------------------------------------------------------------------
    def _callee(self, fn, depth: int = 0):
        """(binding, dynamic) for a call's function expression."""
        t = fn.type
        if t == "identifier":
            return {"kind": "identifier", "name": self.text(fn), "receiver": None, "receiverKind": None}, False
        if t == "scoped_identifier":
            return self._scoped_callee(fn), False
        if t == "field_expression":
            return self._field_callee(fn)
        if t == "generic_function":
            inner = fn.child_by_field_name("function")
            if inner is not None and depth < 8:
                b, d = self._callee(inner, depth + 1)
                b = dict(b)
                b["generic"] = True
                return b, d
        return _dynamic_callee()

    def _scoped_callee(self, fn):
        raw = self.text(fn)
        path = fn.child_by_field_name("path")
        name = fn.child_by_field_name("name")
        recv = self.text(path) if path is not None else ""
        if path is None and raw.startswith("::"):
            recv = "::"
        if path is not None and raw.startswith("::") and not recv.startswith("::"):
            recv = "::" + recv
        b = {"kind": "path", "name": self.text(name) if name is not None else None,
             "receiver": recv or None, "receiverKind": "path"}
        if path is not None and path.type == "generic_type":
            b["generic"] = True
        return b

    def _field_callee(self, fn):
        value = fn.child_by_field_name("value")
        field = fn.child_by_field_name("field")
        if field is None or field.type != "field_identifier":
            return _dynamic_callee()  # tuple field holding a callable: `x.0()`
        rk = _RECEIVER_KINDS.get(value.type if value is not None else None, "expression")
        return {"kind": "member", "name": self.text(field), "receiver": self.text(value) if value is not None else None,
                "receiverKind": rk}, False

    def visit_call_expression(self, node):
        fn = node.child_by_field_name("function")
        if fn is None:
            return None
        binding, dynamic = self._callee(fn)
        self.site("calls", node, self.text(fn), binding, dynamic=dynamic, node_kind="call_expression")
        return None

    def visit_macro_invocation(self, node):
        m = node.child_by_field_name("macro")
        if m is None:
            return None
        raw = self.text(m)
        recv, _, name = raw.rpartition("::")
        binding = {"kind": "macro", "name": name, "receiver": recv or None,
                   "receiverKind": "path" if recv else None, "expanded": False}
        self.site("macros", node, raw, binding, resolution="unsupported", node_kind="macro_invocation")
        return False  # token trees are not parsed as expressions


def _join_use(prefix: str, name: str) -> str:
    if not prefix:
        return name
    return prefix + name if name.startswith("::") else f"{prefix}::{name}"


def _dynamic_callee():
    return {"kind": "dynamic", "name": None, "receiver": None, "receiverKind": None, "dynamic": True}, True


def extract(_language: str, _source: bytes, ctx: common.Context) -> dict:
    return kit_walk.run_extract(ctx, RustVisitor)
