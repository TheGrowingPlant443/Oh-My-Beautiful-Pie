"""Go syntax recipe (pure Tree-sitter AST facts; no resolution, no repository code execution).

Records function/method/func-literal/interface-method declarations, types, import specs,
every ``call_expression`` plus ambiguous generic instantiations, and lexical declarations
(params, locals, imports, package-level names) so the resolver can see shadowing.
"""
from __future__ import annotations

import common
import kit_walk
from kit_walk import Visitor, collect_identifiers, fields_of, strip_quotes

PROFILES = {
    "go": {
        "extensions": [".go"],
        "grammar": "tree_sitter_go",
        "capsule": "language",
        "node_kinds": [
            "function_declaration", "method_declaration", "func_literal", "method_elem",
            "import_spec", "call_expression", "type_spec", "selector_expression",
            "short_var_declaration", "var_spec", "parameter_declaration", "package_clause",
        ],
    }
}

_IDENT = frozenset({"identifier"})
_BLOCKS = ("block", "func_literal", "function_declaration", "method_declaration", "source_file")


def _exported(name: str) -> bool:
    return bool(name) and name[0].isupper()


class GoVisitor(Visitor):
    closure_types = frozenset({"func_literal"})

    # -- package / imports -----------------------------------------------------
    def visit_package_clause(self, node):
        name = kit_walk.child_of_type(node, "package_identifier")
        if name is not None:
            kit_walk.set_module_bindings(self.ctx, package=self.text(name))
        return None

    def visit_import_spec(self, node):
        path = node.child_by_field_name("path")
        if path is None:
            return None
        literal = self.text(path)
        spec = strip_quotes(literal)
        alias_node = node.child_by_field_name("name")
        alias = self.text(alias_node) if alias_node is not None else None
        last = spec.rsplit("/", 1)[-1]
        implicit = alias is None
        local = None
        if alias is None:
            local = last
        elif alias not in (".", "_"):
            local = alias
        binding = {
            "kind": "import", "specifier": spec, "imported": None, "alias": alias,
            "default": False, "namespace": alias == ".", "reexport": False, "typeOnly": False,
            "typeKind": None, "local": local, "sideEffect": alias == "_", "literal": literal,
        }
        if implicit:
            binding["implicitLocal"] = True
        site = self.site("imports", node, literal, binding, node_kind="import_spec")
        if local:
            self.ctx.declare(local, "import", 0, len(self.ctx.source), site_id=site["id"], owner=self._module_owner())
        return None

    def _module_owner(self):
        sid = self.ctx._module_symbols.get(self.ctx.region)  # noqa: SLF001
        return sid

    # -- declarations ---------------------------------------------------------------
    def _declare_params(self, plist, body_range):
        if plist is None:
            return []
        names = []
        for p in plist.children:
            if p.type not in ("parameter_declaration", "variadic_parameter_declaration"):
                continue
            for n in fields_of(p, "name"):
                names.append(self.text(n))
                self.ctx.declare(self.text(n), "parameter", body_range[0], body_range[1], position=n.start_byte)
        return names

    def _callable_common(self, node, kind, name, qualified, bindings, receiver_list=None):
        body = node.child_by_field_name("body")
        rng = (body.start_byte, body.end_byte) if body is not None else (node.end_byte, node.end_byte)
        bindings["body"] = body is not None
        sym = self.open_symbol(node, kind, name, qualified=qualified, bindings=bindings, exported=_exported(name),
                               scope_name=qualified)
        names = []
        if receiver_list is not None:
            names += self._declare_params(receiver_list, rng)
        names += self._declare_params(node.child_by_field_name("parameters"), rng)
        sym["bindings"]["parameters"] = names
        return sym

    def visit_function_declaration(self, node):
        name = self.field_text(node, "name") or ""
        tp = node.child_by_field_name("type_parameters")
        b = {"generic": tp is not None}
        sym = self._callable_common(node, "function", name, name, b)
        if node.parent is not None and node.parent.type == "source_file":
            self.ctx.declare(name, "function", 0, len(self.ctx.source), symbol_id=sym["id"], owner=self._module_owner())
        return None

    def visit_method_declaration(self, node):
        name = self.field_text(node, "name") or ""
        recv = node.child_by_field_name("receiver")
        recv_type = None
        pointer = False
        if recv is not None:
            for p in recv.children:
                if p.type == "parameter_declaration":
                    t = p.child_by_field_name("type")
                    recv_type, pointer = self._receiver_type(t)
                    break
        qualified = f"{recv_type}.{name}" if recv_type else name
        b = {"receiver": recv_type, "pointerReceiver": pointer}
        self._callable_common(node, "method", name, qualified, b, receiver_list=recv)
        return None

    def _receiver_type(self, t):
        if t is None:
            return None, False
        pointer = False
        while t is not None and t.type in ("pointer_type", "parenthesized_type"):
            if t.type == "pointer_type":
                pointer = True
            inner = [c for c in t.children if c.is_named]
            t = inner[0] if inner else None
        if t is None:
            return None, pointer
        if t.type == "generic_type":
            base = t.child_by_field_name("type")
            return (self.text(base) if base is not None else self.text(t)), pointer
        return self.text(t), pointer

    def visit_func_literal(self, node):
        body = node.child_by_field_name("body")
        rng = (body.start_byte, body.end_byte) if body is not None else (node.end_byte, node.end_byte)
        sym = self.open_lambda(node, bindings={"body": body is not None}, node_kind="func_literal")
        names = self._declare_params(node.child_by_field_name("parameters"), rng)
        sym["bindings"]["parameters"] = names
        return None

    def visit_type_spec(self, node):
        self._type_symbol(node)
        return None

    def visit_type_alias(self, node):
        return self.visit_type_spec(node)

    def _type_symbol(self, node):
        name_node = node.child_by_field_name("name")
        if name_node is None:
            return
        name = self.text(name_node)
        t = node.child_by_field_name("type")
        kind = "type"
        if t is not None:
            kind = {"struct_type": "struct", "interface_type": "interface"}.get(t.type, "type")
        if node.type == "type_alias":
            kind = "type"
        b = {"alias": node.type == "type_alias"}
        if t is not None and kind == "type":
            b["underlying"] = self.text(t)
        sym = self.add_symbol(node, kind, name, bindings=b, exported=_exported(name))
        decl_parent = node.parent
        top = decl_parent is not None and decl_parent.type == "type_declaration" and decl_parent.parent is not None and decl_parent.parent.type == "source_file"
        if top:
            self.ctx.declare(name, "type", 0, len(self.ctx.source), symbol_id=sym["id"], owner=self._module_owner())
        else:
            end = self.end_of_ancestor(node, _BLOCKS)
            self.ctx.declare(name, "type", node.start_byte, end, symbol_id=sym["id"])
        if kind == "interface":
            self.ctx.push_scope(sym["id"], name, kind)
            self._open.append(t.id)  # popped when the interface_type node is left
        return None

    def visit_method_elem(self, node):
        name = self.field_text(node, "name")
        if not name:
            return None
        self.add_symbol(node, "signature", name, bindings={"body": False, "interfaceMethod": True}, exported=_exported(name))
        return None

    # -- variables ----------------------------------------------------------------
    def _scope_for_local(self, node):
        end = self.end_of_ancestor(node, _BLOCKS)
        top = node.parent is not None and node.parent.type == "source_file"
        return node.end_byte, end, top

    def _declare_names(self, node, name_nodes, values, kind="variable", mutable=True):
        parent = node.parent
        at_top = False
        p = parent
        while p is not None and p.type in ("var_declaration", "const_declaration", "var_spec_list"):
            p = p.parent
        if p is not None and p.type == "source_file":
            at_top = True
        end = len(self.ctx.source) if at_top else self.end_of_ancestor(node, _BLOCKS)
        owner = self._module_owner() if at_top else ...
        start = 0 if at_top else node.end_byte
        for i, n in enumerate(name_nodes):
            if n.type != "identifier" or self.text(n) == "_":
                continue
            value = values[i] if values is not None and len(values) == len(name_nodes) and i < len(values) else None
            self.declare(self.text(n), kind, start, end, mutable=mutable, value=value, position=n.start_byte, owner=owner)

    def visit_var_spec(self, node):
        names = fields_of(node, "name")
        value = node.child_by_field_name("value")
        values = [c for c in value.children if c.is_named] if value is not None else None
        self._declare_names(node, names, values, "variable", True)
        return None

    def visit_const_spec(self, node):
        names = fields_of(node, "name")
        value = node.child_by_field_name("value")
        values = [c for c in value.children if c.is_named] if value is not None else None
        self._declare_names(node, names, values, "variable", False)
        return None

    def visit_short_var_declaration(self, node):
        left = node.child_by_field_name("left")
        right = node.child_by_field_name("right")
        if left is None:
            return None
        names = [c for c in left.children if c.is_named]
        values = [c for c in right.children if c.is_named] if right is not None else None
        self._declare_names(node, names, values, "variable", True)
        return None

    def visit_range_clause(self, node):
        left = node.child_by_field_name("left")
        if left is None:
            return None
        if not any(c.type == ":=" for c in node.children):
            return None
        end = self.end_of_ancestor(node, _BLOCKS)
        for n in left.children:
            if n.type == "identifier" and self.text(n) != "_":
                self.ctx.declare(self.text(n), "variable", node.end_byte, end, mutable=True, position=n.start_byte)
        return None

    def visit_type_switch_statement(self, node):
        alias = node.child_by_field_name("alias")
        if alias is not None:
            for n in alias.children:
                if n.type == "identifier":
                    self.ctx.declare(self.text(n), "variable", node.start_byte, node.end_byte, position=n.start_byte)
        return None

    def visit_assignment_statement(self, node):
        left = node.child_by_field_name("left")
        if left is None:
            return None
        end = self.end_of_ancestor(node, ("func_literal", "function_declaration", "method_declaration", "source_file"))
        for n in left.children:
            if n.type == "identifier" and self.text(n) != "_":
                self.ctx.declare(self.text(n), "assignment", node.start_byte, end, mutable=True, position=node.start_byte)
        return None

    # -- calls ----------------------------------------------------------------------
    def _callee_binding(self, fn):
        """Return (raw, binding, dynamic) for a call's function expression."""
        raw = self.text(fn)
        t = fn.type
        if t == "identifier":
            return raw, {"kind": "identifier", "name": raw, "receiver": None, "receiverKind": None}, False
        if t == "selector_expression":
            return raw, self._selector_binding(fn), False
        if t in ("generic_type", "type_instantiation_expression"):
            base = fn.child_by_field_name("type") or fn.child_by_field_name("operand")
            if base is not None and base.type in ("identifier", "type_identifier"):
                name = self.text(base)
                return raw, {"kind": "identifier", "name": name, "receiver": None, "receiverKind": None, "generic": True}, False
            if base is not None and base.type in ("selector_expression", "qualified_type"):
                q = self.text(base)
                recv, _, name = q.rpartition(".")
                return raw, {"kind": "member", "name": name, "receiver": recv or None,
                             "receiverKind": "identifier", "generic": True}, False
        # parenthesized / call results / index expressions / literals: computed callee
        return raw, {"kind": "dynamic", "name": None, "receiver": None, "receiverKind": None, "dynamic": True}, True

    def _selector_binding(self, fn):
        operand = fn.child_by_field_name("operand")
        field = fn.child_by_field_name("field")
        recv = self.text(operand) if operand is not None else None
        ot = operand.type if operand is not None else None
        rk = {"identifier": "identifier", "selector_expression": "path"}.get(ot, "expression")
        return {"kind": "member", "name": self.text(field) if field is not None else None,
                "receiver": recv, "receiverKind": rk}

    def visit_call_expression(self, node):
        fn = node.child_by_field_name("function")
        if fn is None:
            return None
        raw, binding, dynamic = self._callee_binding(fn)
        parent = node.parent
        if parent is not None and parent.type in ("go_statement", "defer_statement"):
            binding["deferred" if parent.type == "defer_statement" else "async"] = True
        self.site("calls", node, raw, binding, dynamic=dynamic, node_kind="call_expression")
        return None

    def visit_type_conversion_expression(self, node):
        # `G[int](x)` is ambiguous between generic instantiation+call and conversion.
        t = node.child_by_field_name("type")
        if t is None:
            return None
        _, binding, dynamic = self._callee_binding(t)
        binding = dict(binding)
        binding["ambiguousConversion"] = True
        self.site("calls", node, self.text(t), binding, dynamic=dynamic, node_kind="type_conversion_expression")
        return None


def extract(_language: str, _source: bytes, ctx: common.Context) -> dict:
    return kit_walk.run_extract(ctx, GoVisitor)
