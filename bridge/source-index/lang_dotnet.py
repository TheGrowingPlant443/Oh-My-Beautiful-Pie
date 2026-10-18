"""C# syntax recipe (pure Tree-sitter AST facts).

Types, methods, constructors, finalizers, operators, accessors, local functions, lambdas and
anonymous methods; ``using`` directives (namespace, static, alias, global) and every invocation /
object-creation / constructor-initializer site.  Overload resolution, extension methods, dynamic,
reflection, partial-type merging and delegate invocation targets are not resolved here.
"""
from __future__ import annotations

import common
import kit_walk
from kit_walk import Visitor, child_of_type, children_of_type, collect_identifiers

PROFILES = {
    "csharp": {
        "extensions": [".cs"],
        "grammar": "tree_sitter_c_sharp",
        "capsule": "language",
        "node_kinds": [
            "class_declaration", "interface_declaration", "struct_declaration", "record_declaration",
            "enum_declaration", "namespace_declaration", "method_declaration", "constructor_declaration",
            "destructor_declaration", "operator_declaration", "property_declaration", "accessor_declaration",
            "local_function_statement", "lambda_expression", "anonymous_method_expression",
            "invocation_expression", "object_creation_expression", "using_directive",
            "member_access_expression", "variable_declarator", "constructor_initializer",
            "conditional_access_expression", "parameter_list",
        ],
    }
}

_IDENT = frozenset({"identifier"})
_TYPES = {
    "class_declaration": "class", "interface_declaration": "interface", "struct_declaration": "struct",
    "record_declaration": "class", "record_struct_declaration": "struct", "enum_declaration": "enum",
}
_BLOCKS = ("block", "declaration_list", "compilation_unit", "switch_section", "for_statement",
           "foreach_statement", "using_statement", "arrow_expression_clause", "enum_member_declaration_list")


class CSharpVisitor(Visitor):
    closure_types = frozenset({"lambda_expression", "anonymous_method_expression"})

    def __init__(self, ctx: common.Context):
        super().__init__(ctx)
        self.namespace: str | None = None
        self._ns_stack: dict[int, str | None] = {}

    def leave(self, node):
        if node.id in self._ns_stack:
            self.namespace = self._ns_stack.pop(node.id)
        super().leave(node)

    # -- helpers --------------------------------------------------------------------------------
    def _module_owner(self):
        return self.ctx._module_symbols.get(self.ctx.region)  # noqa: SLF001

    def _outer_owner(self):
        scopes = self.ctx.scopes
        return scopes[-2].symbol_id if len(scopes) >= 2 else None

    def _mods(self, node) -> set[str]:
        out: set[str] = set()
        for c in node.children:
            if c.type == "modifier":
                out.update(self.text(c).split())
        return out

    def _container_range(self, node) -> tuple[int, int]:
        a = kit_walk.ancestor(node, ("declaration_list", "compilation_unit"))
        return (a.start_byte, a.end_byte) if a is not None else (0, len(self.ctx.source))

    def _local_range(self, node) -> tuple[int, int]:
        a = kit_walk.ancestor(node, _BLOCKS)
        return node.end_byte, (a.end_byte if a is not None else len(self.ctx.source))

    def _fqn(self, name: str):
        inner = self.ctx.scopes[1:-1]
        for sc in inner:
            sym = self.ctx.symbol_by_id(sc.symbol_id)
            if sym is None or not (sym.get("bindings") or {}).get("namespace"):
                return None
        return f"{self.namespace}.{name}" if self.namespace and name else (name or None)

    # -- namespaces / using ---------------------------------------------------------------------
    def visit_file_scoped_namespace_declaration(self, node):
        n = node.child_by_field_name("name")
        if n is not None:
            self.namespace = self.text(n)
            kit_walk.set_module_bindings(self.ctx, namespace=self.namespace, package=self.namespace)
        return None

    def visit_namespace_declaration(self, node):
        n = node.child_by_field_name("name")
        txt = self.text(n) if n is not None else ""
        old = self.namespace
        self._ns_stack[node.id] = old
        self.namespace = txt if not old else f"{old}.{txt}"
        sym = self.open_symbol(node, "namespace", txt, bindings={"namespace": self.namespace, "package": self.namespace,
                                                              "body": True})
        start, end = self._container_range(node)
        self.ctx.declare(txt.split(".")[0], "namespace", start, end, symbol_id=sym["id"], owner=self._outer_owner())
        return None

    def visit_using_directive(self, node):
        alias_node = node.child_by_field_name("name")
        target = next((c for c in node.children if c.is_named and (alias_node is None or c.id != alias_node.id)), None)
        if target is None:
            return None
        spec = self.text(target)
        static = any(c.type == "static" for c in node.children)
        global_ = any(c.type == "global" for c in node.children)
        alias = self.text(alias_node) if alias_node is not None else None
        if alias is not None:
            imported, namespace = spec.rpartition(".")[2], False
        else:
            imported, namespace = None, True
        binding = {"kind": "using", "specifier": spec, "imported": imported, "alias": alias, "default": False,
                   "namespace": namespace, "reexport": False, "typeOnly": False, "typeKind": None,
                   "local": alias, "static": static, "global": global_}
        site = self.site("imports", node, self.text(node).strip(), binding, node_kind="using_directive")
        if alias:
            self.ctx.declare(alias, "import", 0, len(self.ctx.source), site_id=site["id"], owner=self._module_owner())
        return None

    # -- types --------------------------------------------------------------------------------------
    def _type(self, node):
        kind = _TYPES[node.type]
        name = self.field_text(node, "name") or ""
        mods = self._mods(node)
        bl = child_of_type(node, "base_list")
        b = {"body": node.child_by_field_name("body") is not None, "modifiers": sorted(mods),
             "extends": [self.text(c) for c in bl.children if c.is_named] if bl is not None else [],
             "partial": "partial" in mods, "static": "static" in mods, "abstract": "abstract" in mods}
        if node.type.startswith("record"):
            b["record"] = True
        in_iface = node.parent is not None and node.parent.parent is not None and node.parent.parent.type == "interface_declaration"
        sym = self.open_symbol(node, kind, name, bindings=b, exported=("public" in mods) or in_iface)
        fq = self._fqn(name)
        if fq:
            sym["bindings"]["fqn"] = fq
        start, end = self._container_range(node)
        self.ctx.declare(name, "enum" if kind == "enum" else "class", start, end, symbol_id=sym["id"],
                         owner=self._outer_owner())
        pl = child_of_type(node, "parameter_list")
        if pl is not None:  # record primary constructor
            names = []
            for p in children_of_type(pl, "parameter"):
                n = p.child_by_field_name("name")
                if n is not None:
                    names.append(self.text(n))
                    self.ctx.declare(self.text(n), "parameter", node.start_byte, node.end_byte, position=n.start_byte)
            sym["bindings"]["parameters"] = names
        return None

    visit_class_declaration = _type
    visit_interface_declaration = _type
    visit_struct_declaration = _type
    visit_record_declaration = _type
    visit_record_struct_declaration = _type
    visit_enum_declaration = _type

    def visit_delegate_declaration(self, node):
        name = self.field_text(node, "name")
        if name:
            self.add_symbol(node, "type", name, bindings={"delegate": True, "body": False},
                            exported="public" in self._mods(node))
        return None

    def visit_enum_member_declaration(self, node):
        n = node.child_by_field_name("name")
        if n is not None:
            self.add_symbol(node, "property", self.text(n), bindings={"enumConstant": True}, exported=True)
        return None

    # -- callables -------------------------------------------------------------------------------------
    def _params(self, node, body) -> list[str]:
        names: list[str] = []
        pl = node.child_by_field_name("parameters")
        if pl is None:
            return names
        rng = (body.start_byte, body.end_byte) if body is not None else (node.end_byte, node.end_byte)
        for p in children_of_type(pl, "parameter"):
            n = p.child_by_field_name("name")
            if n is not None:
                names.append(self.text(n))
                self.ctx.declare(self.text(n), "parameter", rng[0], rng[1], position=n.start_byte)
        return names

    def _callable(self, node, kind: str, name: str, extra: dict | None = None, declare_name: bool = True):
        mods = self._mods(node)
        body = node.child_by_field_name("body")
        in_iface = node.parent is not None and node.parent.parent is not None \
            and node.parent.parent.type == "interface_declaration"
        b = {"body": body is not None, "modifiers": sorted(mods), "static": "static" in mods,
             "abstract": "abstract" in mods}
        if extra:
            b.update(extra)
        sym = self.open_symbol(node, kind, name, bindings=b, exported=("public" in mods) or in_iface)
        sym["bindings"]["parameters"] = self._params(node, body)
        if declare_name:
            if kind == "function":  # local function: visible throughout its block
                start, end = self._local_container(node)
            else:
                start, end = self._container_range(node)
            self.ctx.declare(name, "function", start, end, symbol_id=sym["id"], owner=self._outer_owner())
        return sym

    def _local_container(self, node) -> tuple[int, int]:
        a = kit_walk.ancestor(node, ("block", "declaration_list", "compilation_unit"))
        return (a.start_byte, a.end_byte) if a is not None else (0, len(self.ctx.source))

    def visit_method_declaration(self, node):
        self._callable(node, "method", self.field_text(node, "name") or "")
        return None

    def visit_local_function_statement(self, node):
        self._callable(node, "function", self.field_text(node, "name") or "")
        return None

    def visit_constructor_declaration(self, node):
        self._callable(node, "constructor", self.field_text(node, "name") or "", declare_name=False)
        return None

    def visit_destructor_declaration(self, node):
        self._callable(node, "method", "~" + (self.field_text(node, "name") or ""), {"destructor": True},
                       declare_name=False)
        return None

    def _operator_name(self, node) -> str:
        parts = []
        seen = False
        for c in node.children:
            if c.type == "operator" and not seen:
                seen = True
                continue
            if seen:
                if c.type == "parameter_list":
                    break
                if c.type not in ("explicit", "implicit"):
                    parts.append(self.text(c))
        return "operator " + " ".join(parts).strip() if parts else "operator"

    def visit_operator_declaration(self, node):
        self._callable(node, "method", self._operator_name(node), {"operator": True}, declare_name=False)
        return None

    def visit_conversion_operator_declaration(self, node):
        self._callable(node, "method", self._operator_name(node), {"operator": True, "conversion": True},
                       declare_name=False)
        return None

    def _member_with_accessors(self, node, bindings_extra: dict | None = None):
        name = self.field_text(node, "name") or ("this[]" if node.type == "indexer_declaration" else "")
        mods = self._mods(node)
        b = {"modifiers": sorted(mods), "static": "static" in mods, "abstract": "abstract" in mods, "body": True}
        if bindings_extra:
            b.update(bindings_extra)
        self.open_symbol(node, "property", name, bindings=b, exported="public" in mods)
        return None

    def visit_property_declaration(self, node):
        return self._member_with_accessors(node)

    def visit_indexer_declaration(self, node):
        return self._member_with_accessors(node, {"indexer": True})

    def visit_event_declaration(self, node):
        return self._member_with_accessors(node, {"event": True})

    def visit_accessor_declaration(self, node):
        which = "accessor"
        for c in node.children:  # attribute lists / modifiers can precede the keyword
            if c.type in ("get", "set", "init", "add", "remove"):
                which = c.type
                break
        owner = self.ctx.current_scope
        prop = owner.name if owner is not None and owner.kind == "property" else ""
        label = f"{which} {prop}".strip()
        self.open_symbol(node, "accessor", which, qualified=self.qual(label), scope_name=label,
                         bindings={"property": prop, "accessor": which,
                                   "body": node.child_by_field_name("body") is not None})
        return None

    def visit_arrow_expression_clause(self, node):
        p = node.parent
        if p is not None and p.type in ("property_declaration", "indexer_declaration"):
            owner = self.ctx.current_scope
            prop = owner.name if owner is not None else ""
            self.open_symbol(node, "accessor", "get", qualified=self.qual(f"get {prop}".strip()),
                             scope_name=f"get {prop}".strip(),
                             bindings={"property": prop, "accessor": "get", "body": True, "expressionBodied": True})
        return None

    def _lambda(self, node):
        body = node.child_by_field_name("body") or child_of_type(node, "block")
        sym = self.open_lambda(node, bindings={"body": body is not None}, node_kind=node.type)
        params = node.child_by_field_name("parameters")
        names: list[str] = []
        if params is not None:
            if params.type == "identifier":
                names = [self.text(params)]
            else:
                names = [self.text(i) for i in collect_identifiers(params, _IDENT, skip_fields=frozenset({"type"}))]
        for n in names:
            self.ctx.declare(n, "parameter", node.start_byte, node.end_byte)
        sym["bindings"]["parameters"] = names
        return None

    visit_lambda_expression = _lambda
    visit_anonymous_method_expression = _lambda

    # -- variables ----------------------------------------------------------------------------------------
    def visit_variable_declarator(self, node):
        n = node.child_by_field_name("name")
        if n is None:
            return None
        p = node.parent.parent if node.parent is not None else None
        member = p is not None and p.type in ("field_declaration", "event_field_declaration")
        value = next((c for c in reversed(node.children) if c.is_named and c.id != n.id), None)
        if member:
            start, end = self._container_range(node)
        else:
            start, end = self._local_range(node)
        self.declare(self.text(n), "variable", start, end, mutable=True, value=value, position=n.start_byte)
        return None

    def visit_foreach_statement(self, node):
        left = node.child_by_field_name("left")
        if left is not None and left.type == "identifier":
            self.ctx.declare(self.text(left), "variable", node.start_byte, node.end_byte, position=left.start_byte)
        return None

    def visit_catch_declaration(self, node):
        n = node.child_by_field_name("name")
        if n is not None:
            a = kit_walk.ancestor(node, ("catch_clause",))
            r = (a.start_byte, a.end_byte) if a is not None else (node.start_byte, node.end_byte)
            self.ctx.declare(self.text(n), "catch", r[0], r[1], position=n.start_byte)
        return None

    def visit_assignment_expression(self, node):
        left = node.child_by_field_name("left")
        if left is not None and left.type == "identifier":
            a = kit_walk.ancestor(node, ("method_declaration", "local_function_statement", "constructor_declaration",
                                         "lambda_expression", "accessor_declaration", "compilation_unit"))
            end = a.end_byte if a is not None else len(self.ctx.source)
            self.ctx.declare(self.text(left), "assignment", node.start_byte, end, mutable=True, position=node.start_byte)
        return None

    # -- calls ---------------------------------------------------------------------------------------------
    def _rk(self, n) -> str | None:
        if n is None:
            return None
        t = n.type
        if t in ("this_expression", "this"):
            return "this"
        if t in ("base_expression", "base"):
            return "super"
        if t == "identifier":
            return "identifier"
        if t in ("member_access_expression", "qualified_name"):
            return "path"
        return "expression"

    def _simple(self, n):
        """(name, generic) of an identifier / generic_name node, else (None, False)."""
        if n is None:
            return None, False
        if n.type == "identifier":
            return self.text(n), False
        if n.type == "generic_name":
            i = child_of_type(n, "identifier")
            return (self.text(i) if i is not None else None), True
        return None, False

    def _callee(self, fn):
        t = fn.type
        raw = self.text(fn)
        if t in ("identifier", "generic_name"):
            name, gen = self._simple(fn)
            b = {"kind": "identifier", "name": name, "receiver": None, "receiverKind": None}
            if gen:
                b["generic"] = True
            return b, False
        if t == "member_access_expression":
            expr = fn.child_by_field_name("expression")
            name, gen = self._simple(fn.child_by_field_name("name"))
            if expr is None:
                # `this`/`base` keyword receivers appear as anonymous tokens
                tok = next((c for c in fn.children if c.type in ("this", "base")), None)
                recv_text = self.text(tok) if tok is not None else None
                rk = "this" if tok is not None and tok.type == "this" else ("super" if tok is not None else None)
            else:
                recv_text, rk = self.text(expr), self._rk(expr)
            if name is not None:
                b = {"kind": "path" if rk == "path" else "member", "name": name, "receiver": recv_text, "receiverKind": rk}
                if gen:
                    b["generic"] = True
                return b, False
        if t == "conditional_access_expression":
            cond = fn.child_by_field_name("condition")
            mb = child_of_type(fn, "member_binding_expression")
            name, gen = self._simple(mb.child_by_field_name("name")) if mb is not None else (None, False)
            if name is not None:
                return {"kind": "member", "name": name, "receiver": self.text(cond) if cond is not None else None,
                        "receiverKind": self._rk(cond), "safeCall": True}, False
        return {"kind": "dynamic", "name": None, "receiver": None, "receiverKind": None, "dynamic": True}, True

    def visit_invocation_expression(self, node):
        fn = node.child_by_field_name("function")
        if fn is None:
            return None
        b, dyn = self._callee(fn)
        self.site("calls", node, self.text(fn), b, dynamic=dyn, node_kind="invocation_expression")
        return None

    def visit_object_creation_expression(self, node):
        t = node.child_by_field_name("type")
        if t is None:
            return None
        raw = self.text(t)
        base = raw.split("<", 1)[0].strip()
        recv, _, simple = base.rpartition(".")
        self.site("calls", node, raw, {"kind": "new", "name": simple, "receiver": recv or None,
                                       "receiverKind": "path" if recv else None}, node_kind="object_creation_expression")
        return None

    def visit_implicit_object_creation_expression(self, node):
        self.site("calls", node, self.text(node), {"kind": "new", "name": None, "receiver": None, "receiverKind": None,
                                                    "implicitType": True, "dynamic": True},
                  dynamic=True, node_kind="implicit_object_creation_expression")
        return None

    def visit_constructor_initializer(self, node):
        first = node.children[0] if node.children else None
        # the keyword precedes the argument list: `: base(...)` / `: this(...)`
        kw = next((c for c in node.children if c.type in ("base", "this")), None)
        name = self.text(kw) if kw is not None else (self.text(first) if first is not None else "base")
        self.site("calls", node, name, {"kind": "identifier", "name": name, "receiver": None, "receiverKind": None,
                                        "constructor": True}, node_kind="constructor_initializer")
        return None


def extract(language: str, source: bytes, ctx: common.Context) -> dict:
    return kit_walk.run_extract(ctx, CSharpVisitor)
