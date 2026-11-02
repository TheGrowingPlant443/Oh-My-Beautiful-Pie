"""JVM-family syntax recipes: Java, Groovy/Gradle, Kotlin and Scala (pure Tree-sitter AST facts).

Records named/anonymous types, methods, constructors, accessors, initializers, lambdas/closures,
imports (one site per imported name), every recognized call/new/method-reference site and the
lexical declarations a resolver needs to see shadowing.  Overloads, inheritance, virtual dispatch,
implicit conversions/given instances, reflection and Gradle DSL dynamics are *not* resolved here.
Java variables are intentionally not declared: Java keeps methods and variables in separate
namespaces, so a local named like a method does not shadow calls to it.
"""
from __future__ import annotations

import common
import kit_walk
from kit_walk import Visitor, child_of_type, children_of_type, collect_identifiers, fields_of

_JAVA_KINDS = [
    "class_declaration", "interface_declaration", "enum_declaration", "record_declaration",
    "annotation_type_declaration", "method_declaration", "constructor_declaration", "lambda_expression",
    "method_invocation", "object_creation_expression", "import_declaration", "package_declaration",
    "explicit_constructor_invocation", "method_reference", "static_initializer", "enum_constant",
    "compact_constructor_declaration", "formal_parameters",
]
_GROOVY_KINDS = [
    "class_declaration", "interface_declaration", "enum_declaration", "method_declaration",
    "constructor_declaration", "function_definition", "closure", "method_invocation", "juxt_function_call",
    "object_creation_expression", "import_declaration", "package_declaration", "lambda_expression",
    "local_variable_declaration",
]
_KOTLIN_KINDS = [
    "class_declaration", "object_declaration", "companion_object", "function_declaration", "secondary_constructor",
    "primary_constructor", "anonymous_initializer", "lambda_literal", "anonymous_function", "call_expression",
    "navigation_expression", "import", "package_header", "property_declaration", "getter", "setter",
    "object_literal", "variable_declaration",
]
_SCALA_KINDS = [
    "class_definition", "object_definition", "trait_definition", "enum_definition", "given_definition",
    "function_definition", "function_declaration", "lambda_expression", "call_expression", "instance_expression",
    "import_declaration", "package_clause", "val_definition", "var_definition", "field_expression",
    "extension_definition", "case_clause", "namespace_selectors", "namespace_wildcard", "infix_expression",
]

PROFILES = {
    "java": {"extensions": [".java"], "grammar": "tree_sitter_java", "capsule": "language", "node_kinds": _JAVA_KINDS},
    "groovy": {"extensions": [".groovy", ".gradle"], "grammar": "tree_sitter_groovy", "capsule": "language",
               "node_kinds": _GROOVY_KINDS},
    "kotlin": {"extensions": [".kt", ".kts"], "grammar": "tree_sitter_kotlin", "capsule": "language",
               "node_kinds": _KOTLIN_KINDS},
    "scala": {"extensions": [".scala"], "grammar": "tree_sitter_scala", "capsule": "language",
              "node_kinds": _SCALA_KINDS},
}

_IDENT = frozenset({"identifier"})
_INIT = "<init>"


def _last(dotted: str) -> str:
    return dotted.rpartition(".")[2]


def _strip_generics(t: str) -> str:
    return t.split("<", 1)[0].split("[", 1)[0].strip()


class _Base(Visitor):
    sep = "."
    #: node types delimiting a declaration/lexical container for declarations
    container_types: tuple[str, ...] = ()

    def __init__(self, ctx: common.Context):
        super().__init__(ctx)
        self.package: str | None = None

    # -- shared helpers ---------------------------------------------------------------
    def _module_owner(self):
        return self.ctx._module_symbols.get(self.ctx.region)  # noqa: SLF001

    def _outer_owner(self):
        scopes = self.ctx.scopes
        return scopes[-2].symbol_id if len(scopes) >= 2 else None

    def set_package(self, name: str) -> None:
        self.package = name
        kit_walk.set_module_bindings(self.ctx, package=name, namespace=name)

    def fqn(self, name: str):
        """Fully-qualified name for a top-level declaration (the module is the only open scope)."""
        inner = self.ctx.scopes[1:-1]  # between the module and the just-opened symbol
        for sc in inner:
            sym = self.ctx.symbol_by_id(sc.symbol_id)
            if sym is None or not (sym.get("bindings") or {}).get("package"):
                return None
        return f"{self.package}.{name}" if self.package and name else (name or None)

    def container_range(self, node) -> tuple[int, int]:
        a = kit_walk.ancestor(node, self.container_types)
        if a is None:
            return 0, len(self.ctx.source)
        return a.start_byte, a.end_byte

    def local_range(self, node) -> tuple[int, int]:
        a = kit_walk.ancestor(node, self.container_types)
        return node.end_byte, (a.end_byte if a is not None else len(self.ctx.source))

    def import_site(self, node, spec: str, imported, alias, namespace: bool, *, raw: str | None = None,
                    kind: str = "import", **extra) -> dict:
        local = alias or imported
        if namespace:
            local = None
        if local == "_":
            local = None
        binding = {"kind": kind, "specifier": spec, "imported": imported, "alias": alias, "default": False,
                   "namespace": namespace, "reexport": False, "typeOnly": False, "typeKind": None, "local": local}
        binding.update(extra)
        site = self.site("imports", node, raw if raw is not None else spec, binding, node_kind=node.type)
        if local:
            self.ctx.declare(local, "import", 0, len(self.ctx.source), site_id=site["id"], owner=self._module_owner())
        return site

    def type_symbol(self, node, kind: str, name: str, bindings: dict, exported, *, label: str | None = None) -> dict:
        label = label or name or f"<anonymous@{node.start_point[0] + 1}:{node.start_point[1]}>"
        sym = self.open_symbol(node, kind, name, qualified=self.qual(label), scope_name=label,
                               bindings=bindings, exported=exported)
        fq = self.fqn(name)
        if fq:
            sym["bindings"]["fqn"] = fq
        if name:
            start, end = self.container_range(node)
            self.ctx.declare(name, "class" if kind != "enum" else "enum", start, end, symbol_id=sym["id"],
                             owner=self._outer_owner())
        return sym

    def callee_site(self, node, raw, binding, dynamic=False, node_kind=None):
        return self.site("calls", node, raw, binding, dynamic=dynamic, node_kind=node_kind or node.type)

    @staticmethod
    def dyn_binding() -> dict:
        return {"kind": "dynamic", "name": None, "receiver": None, "receiverKind": None, "dynamic": True}


# ============================================================================================
# Java (and the Groovy subset that shares its grammar nodes)
# ============================================================================================

_JAVA_TYPES = {
    "class_declaration": "class", "interface_declaration": "interface", "enum_declaration": "enum",
    "record_declaration": "class", "annotation_type_declaration": "interface",
}


class JavaVisitor(_Base):
    closure_types = frozenset({"lambda_expression"})
    container_types = ("class_body", "interface_body", "enum_body", "enum_body_declarations", "block",
                       "constructor_body", "program", "annotation_type_body")

    def modifiers(self, node) -> set[str]:
        m = child_of_type(node, "modifiers")
        out: set[str] = set()
        if m is None:
            return out
        for c in m.children:
            if c.type in ("marker_annotation", "annotation"):
                continue
            out.update(self.text(c).split())
        return out

    def _exp(self, mods: set[str], in_iface: bool) -> bool:
        return "public" in mods or in_iface

    # -- package/imports ---------------------------------------------------------------------
    def visit_package_declaration(self, node):
        n = next((c for c in node.children if c.type in ("scoped_identifier", "identifier")), None)
        if n is not None:
            self.set_package(self.text(n))
        return None

    def visit_import_declaration(self, node):
        name = next((c for c in node.children if c.type in ("scoped_identifier", "identifier")), None)
        if name is None:
            return None
        spec = self.text(name)
        wildcard = any(c.type == "asterisk" for c in node.children)
        static = any(c.type == "static" for c in node.children)
        alias = None
        named = [c for c in node.children if c.is_named and c.type not in ("asterisk",)]
        if not wildcard and len(named) >= 2 and named[1].type == "identifier":
            alias = self.text(named[1])  # Groovy `import a.B as C`
        self.import_site(node, spec, None if wildcard else _last(spec), alias, wildcard, raw=self.text(node).strip(),
                         static=static)
        return None

    # -- types ------------------------------------------------------------------------------------
    def _heritage(self, node) -> dict:
        b: dict = {}
        sup = node.child_by_field_name("superclass")
        if sup is not None:
            b["extends"] = [self.text(c) for c in sup.children if c.is_named]
        ifaces = node.child_by_field_name("interfaces")
        impl = []
        if ifaces is not None:
            for tl in ifaces.children:
                if tl.type == "type_list":
                    impl += [self.text(c) for c in tl.children if c.is_named]
        if impl:
            b["implements"] = impl
        ext_i = child_of_type(node, "extends_interfaces")
        if ext_i is not None:
            b.setdefault("extends", [])
            for tl in ext_i.children:
                if tl.type == "type_list":
                    b["extends"] += [self.text(c) for c in tl.children if c.is_named]
        return b

    def _type(self, node):
        kind = _JAVA_TYPES[node.type]
        name = self.field_text(node, "name") or ""
        mods = self.modifiers(node)
        b = self._heritage(node)
        b["body"] = True
        b["static"] = "static" in mods
        b["abstract"] = "abstract" in mods
        if node.type == "record_declaration":
            b["record"] = True
        if node.type == "annotation_type_declaration":
            b["annotation"] = True
        b["modifiers"] = sorted(mods)
        in_iface = node.parent is not None and node.parent.type == "interface_body"
        self.type_symbol(node, kind, name, b, self._exp(mods, in_iface))
        return None

    visit_class_declaration = _type
    visit_interface_declaration = _type
    visit_enum_declaration = _type
    visit_record_declaration = _type
    visit_annotation_type_declaration = _type

    def visit_enum_constant(self, node):
        name = self.field_text(node, "name") or ""
        body = node.child_by_field_name("body")
        self.open_symbol(node, "property", name, bindings={"enumConstant": True, "body": body is not None},
                         exported=True)
        return None

    # -- callables ---------------------------------------------------------------------------------
    def _params(self, plist) -> list[str]:
        names: list[str] = []
        if plist is None:
            return names
        for p in plist.children:
            if p.type not in ("formal_parameter", "spread_parameter", "receiver_parameter"):
                continue
            n = self._parameter_name(p)
            if n is not None:
                names.append(self.text(n))
        return names

    @staticmethod
    def _parameter_name(parameter):
        name = parameter.child_by_field_name("name")
        if name is not None:
            return name
        ids = [c for c in parameter.children if c.type in ("identifier", "variable_declarator")]
        if not ids:
            return None
        name = ids[-1]
        if name.type == "variable_declarator":
            return name.child_by_field_name("name")
        return name

    def _callable(self, node, kind: str, name: str, extra: dict | None = None):
        mods = self.modifiers(node)
        body = node.child_by_field_name("body")
        b = {"body": body is not None, "static": "static" in mods, "abstract": "abstract" in mods,
             "modifiers": sorted(mods)}
        if extra:
            b.update(extra)
        in_iface = node.parent is not None and node.parent.type == "interface_body"
        sym = self.open_symbol(node, kind, name, bindings=b, exported=self._exp(mods, in_iface))
        sym["bindings"]["parameters"] = self._params(node.child_by_field_name("parameters"))
        owner = self._outer_owner()
        if kind == "method":
            start, end = self.container_range(node)
            self.ctx.declare(name, "function", start, end, symbol_id=sym["id"], owner=owner)
        return sym

    def visit_method_declaration(self, node):
        self._callable(node, "method", self.field_text(node, "name") or "")
        return None

    def visit_constructor_declaration(self, node):
        self._callable(node, "constructor", self.field_text(node, "name") or "")
        return None

    def visit_compact_constructor_declaration(self, node):
        self._callable(node, "constructor", self.field_text(node, "name") or "", {"compact": True})
        return None

    def visit_static_initializer(self, node):
        self.open_symbol(node, "initializer", "<clinit>", bindings={"static": True, "body": True},
                         scope_name=f"<clinit@{node.start_point[0] + 1}>")
        return None

    def visit_block(self, node):
        if node.parent is not None and node.parent.type == "class_body":
            self.open_symbol(node, "initializer", _INIT, bindings={"static": False, "body": True},
                             scope_name=f"<init@{node.start_point[0] + 1}>")
        return None

    def visit_lambda_expression(self, node):
        body = node.child_by_field_name("body")
        sym = self.open_lambda(node, bindings={"body": body is not None}, node_kind="lambda_expression")
        params = node.child_by_field_name("parameters")
        names: list[str] = []
        if params is not None:
            if params.type == "identifier":
                names = [self.text(params)]
            else:
                names = [self.text(i) for i in collect_identifiers(params, _IDENT, skip_fields=frozenset({"type"}))]
        sym["bindings"]["parameters"] = names
        return None

    # -- calls --------------------------------------------------------------------------------------
    def _receiver_kind(self, obj) -> str | None:
        if obj is None:
            return None
        t = obj.type
        if t == "this":
            return "this"
        if t == "super":
            return "super"
        if t == "identifier":
            return "identifier"
        if t in ("field_access", "scoped_identifier", "scoped_type_identifier"):
            return "path" if self._plain_path(obj) else "expression"
        return "expression"

    def _plain_path(self, n) -> bool:
        stack = [n]
        while stack:
            c = stack.pop()
            if c.type in ("identifier", "type_identifier", "this", "super"):
                continue
            if c.type in ("field_access", "scoped_identifier", "scoped_type_identifier"):
                stack.extend(x for x in c.children if x.is_named)
                continue
            return False
        return True

    def visit_method_invocation(self, node):
        name = node.child_by_field_name("name")
        if name is None:
            return None
        obj = node.child_by_field_name("object")
        recv = self.text(obj) if obj is not None else None
        rk = self._receiver_kind(obj)
        kind = "path" if rk == "path" else "member"
        if obj is None:
            kind = "identifier"
        b = {"kind": kind, "name": self.text(name), "receiver": recv, "receiverKind": rk}
        raw = f"{recv}.{self.text(name)}" if recv else self.text(name)
        self.callee_site(node, raw, b)
        return None

    def visit_explicit_constructor_invocation(self, node):
        ctor = node.child_by_field_name("constructor")
        if ctor is None:
            return None
        self.callee_site(node, self.text(ctor), {"kind": "identifier", "name": self.text(ctor), "receiver": None,
                                                 "receiverKind": None, "constructor": True})
        return None

    def visit_object_creation_expression(self, node):
        t = node.child_by_field_name("type")
        if t is not None:
            raw = self.text(t)
            base = _strip_generics(raw)
            recv, _, simple = base.rpartition(".")
            self.callee_site(node, raw, {"kind": "new", "name": simple, "receiver": recv or None,
                                         "receiverKind": "path" if recv else None})
        body = child_of_type(node, "class_body")
        if body is not None:
            label = f"<anonymous {_strip_generics(self.text(t)) if t is not None else ''}@{node.start_point[0] + 1}>"
            self.open_symbol(node, "class", "", qualified=self.qual(label), scope_name=label,
                             bindings={"anonymous": True, "body": True,
                                       "extends": [self.text(t)] if t is not None else []},
                             node_kind="anonymous_class")
        return None

    def visit_method_reference(self, node):
        kids = [c for c in node.children if c.type != "::"]
        if len(kids) < 2:
            return None
        recv, name = kids[0], kids[-1]
        rk = self._receiver_kind(recv) if recv.type != "type_identifier" else "identifier"
        self.site("references", node, self.text(node),
                  {"kind": "member", "name": self.text(name), "receiver": self.text(recv), "receiverKind": rk,
                   "methodReference": True}, node_kind="method_reference")
        return None


# ============================================================================================
# Groovy / Gradle
# ============================================================================================

class GroovyVisitor(JavaVisitor):
    closure_types = frozenset({"lambda_expression", "closure"})
    container_types = JavaVisitor.container_types + ("closure",)

    def _exp(self, mods: set[str], in_iface: bool) -> bool:
        return not (mods & {"private", "protected"})  # Groovy members are public unless declared otherwise

    def visit_function_definition(self, node):
        name = self.field_text(node, "name") or ""
        body = node.child_by_field_name("body")
        b = {"body": body is not None, "script": True}
        sym = self.open_symbol(node, "function", name, bindings=b, exported=True)
        sym["bindings"]["parameters"] = self._params(node.child_by_field_name("parameters"))
        self.ctx.declare(name, "function", 0, len(self.ctx.source), symbol_id=sym["id"], owner=self._outer_owner())
        return None

    def visit_closure(self, node):
        parent = node.parent
        named = [c for c in node.children if c.is_named]
        if parent is not None and parent.type == "function_definition" and parent.child_by_field_name("body") is not None \
                and parent.child_by_field_name("body").id == node.id:
            return None  # the function's own body, not a separate callable
        if len(named) == 1 and named[0].type == "lambda_expression":
            pend = self._pending.pop(node.id, None)
            if pend is not None:
                self._pending[named[0].id] = pend
            return None  # the lambda_expression child is the callable
        sym = self.open_lambda(node, bindings={"body": True, "closure": True}, node_kind="closure")
        sym["bindings"]["parameters"] = []
        return None

    def visit_juxt_function_call(self, node):
        name = node.child_by_field_name("name")
        if name is None:
            return None
        self.callee_site(node, self.text(name), {"kind": "identifier", "name": self.text(name), "receiver": None,
                                                 "receiverKind": None, "command": True})
        return None

    def visit_method_invocation(self, node):
        super().visit_method_invocation(node)
        if node.child_by_field_name("body") is not None:
            self.ctx.sites[-1]["binding"]["closureBody"] = True
        return None

    def visit_local_variable_declaration(self, node):
        for d in children_of_type(node, "variable_declarator"):
            n = d.child_by_field_name("name")
            if n is None:
                continue
            start, end = self.local_range(node)
            self.declare(self.text(n), "variable", start, end, mutable=True,
                         value=d.child_by_field_name("value"), position=n.start_byte)
        return None

    def visit_formal_parameter(self, node):
        n = node.child_by_field_name("name")
        if n is not None:
            a = kit_walk.ancestor(node, ("method_declaration", "constructor_declaration", "function_definition"))
            if a is not None:
                body = a.child_by_field_name("body")
                if body is not None:
                    self.ctx.declare(self.text(n), "parameter", body.start_byte, body.end_byte, position=n.start_byte)
        return None


# ============================================================================================
# Kotlin
# ============================================================================================

class KotlinVisitor(_Base):
    closure_types = frozenset({"lambda_literal", "anonymous_function"})
    container_types = ("function_body", "block", "class_body", "enum_class_body", "source_file", "lambda_literal",
                       "for_statement")

    def _mods(self, node) -> set[str]:
        m = child_of_type(node, "modifiers")
        out: set[str] = set()
        if m is not None:
            for c in m.children:
                if c.type != "annotation":
                    out.update(self.text(c).split())
        return out

    def _exported(self, mods: set[str]):
        return not (mods & {"private", "protected"})

    def visit_package_header(self, node):
        n = child_of_type(node, "qualified_identifier", "identifier")
        if n is not None:
            self.set_package(self.text(n))
        return None

    def visit_import(self, node):
        q = child_of_type(node, "qualified_identifier", "identifier")
        if q is None:
            return None
        spec = self.text(q)
        txt = self.text(node)
        wildcard = txt.rstrip().endswith("*")
        alias_node = next((c for c in node.children if c.type == "identifier" and c.id != q.id), None)
        alias = self.text(alias_node) if alias_node is not None else None
        self.import_site(node, spec, None if wildcard else _last(spec), alias, wildcard, raw=txt.strip())
        return None

    # -- types ------------------------------------------------------------------------------------
    def _delegations(self, node) -> list[str]:
        ds = child_of_type(node, "delegation_specifiers")
        if ds is None:
            return []
        return [self.text(c) for c in ds.children if c.type == "delegation_specifier"]

    def _class_like(self, node, kind: str, name: str, extra: dict | None = None, label: str | None = None):
        mods = self._mods(node)
        b = {"body": True, "modifiers": sorted(mods), "extends": self._delegations(node)}
        if extra:
            b.update(extra)
        return self.type_symbol(node, kind, name, b, self._exported(mods), label=label)

    def visit_class_declaration(self, node):
        name = self.field_text(node, "name") or ""
        is_iface = any(c.type == "interface" for c in node.children)
        mods = self._mods(node)
        kind = "enum" if "enum" in mods else "class"
        if is_iface:
            kind = "interface"
        self._class_like(node, kind, name, {"data": "data" in mods})
        return None

    def visit_object_declaration(self, node):
        self._class_like(node, "class", self.field_text(node, "name") or "", {"object": True})
        return None

    def visit_companion_object(self, node):
        name = self.field_text(node, "name") or "Companion"
        self._class_like(node, "class", name, {"object": True, "companion": True})
        return None

    def visit_object_literal(self, node):
        label = f"<anonymous@{node.start_point[0] + 1}:{node.start_point[1]}>"
        self.open_symbol(node, "class", "", qualified=self.qual(label), scope_name=label,
                         bindings={"anonymous": True, "object": True, "body": True, "extends": self._delegations(node)})
        return None

    def visit_enum_entry(self, node):
        n = child_of_type(node, "identifier")
        if n is not None:
            self.add_symbol(node, "property", self.text(n), bindings={"enumConstant": True}, exported=True)
        return None

    def visit_primary_constructor(self, node):
        cls = self.ctx.current_scope
        name = cls.name if cls is not None else ""
        sym = self.add_symbol(node, "constructor", name, bindings={"primary": True, "body": True})
        names = []
        cparams = child_of_type(node, "class_parameters")
        owner = cls.symbol_id if cls is not None else None
        if cparams is not None and cls is not None:
            a = kit_walk.ancestor(node, ("class_declaration", "object_declaration"))
            start, end = (a.start_byte, a.end_byte) if a is not None else (node.start_byte, node.end_byte)
            for p in children_of_type(cparams, "class_parameter"):
                n = child_of_type(p, "identifier")
                if n is not None:
                    names.append(self.text(n))
                    self.ctx.declare(self.text(n), "parameter", start, end, owner=owner, position=n.start_byte)
        sym["bindings"]["parameters"] = names
        return None

    # -- callables ---------------------------------------------------------------------------------
    def _params(self, node, rng) -> list[str]:
        names: list[str] = []
        plist = child_of_type(node, "function_value_parameters")
        if plist is None:
            return names
        for p in children_of_type(plist, "parameter", "class_parameter"):
            n = child_of_type(p, "identifier")
            if n is not None:
                names.append(self.text(n))
                self.ctx.declare(self.text(n), "parameter", rng[0], rng[1], position=n.start_byte)
        return names

    def _in_class_body(self, node) -> bool:
        p = node.parent
        return p is not None and p.type in ("class_body", "enum_class_body")

    def visit_function_declaration(self, node):
        name = self.field_text(node, "name") or ""
        mods = self._mods(node)
        body = child_of_type(node, "function_body")
        recv = None
        for c in node.children:
            if c.type == "user_type":
                recv = self.text(c)
                break
            if c.type == "identifier":
                break
        kind = "method" if self._in_class_body(node) else "function"
        b = {"body": body is not None, "modifiers": sorted(mods), "receiver": recv, "extension": recv is not None,
             "abstract": "abstract" in mods}
        sym = self.open_symbol(node, kind, name, bindings=b, exported=self._exported(mods))
        rng = (body.start_byte, body.end_byte) if body is not None else (node.end_byte, node.end_byte)
        sym["bindings"]["parameters"] = self._params(node, rng)
        start, end = self.container_range(node)
        self.ctx.declare(name, "function", start, end, symbol_id=sym["id"], owner=self._outer_owner())
        return None

    def visit_secondary_constructor(self, node):
        cls = self.ctx.current_scope
        cname = cls.name if cls is not None else ""
        sym = self.open_symbol(node, "constructor", "constructor", qualified=self.qual(_INIT),
                               scope_name=_INIT, bindings={"body": True, "className": cname})
        rng = (node.start_byte, node.end_byte)
        sym["bindings"]["parameters"] = self._params(node, rng)
        return None

    def visit_anonymous_initializer(self, node):
        self.open_symbol(node, "initializer", _INIT, bindings={"body": True},
                         scope_name=f"<init@{node.start_point[0] + 1}>")
        return None

    def _accessor(self, node, which: str):
        prop = ""
        p = node.parent
        if p is not None:
            vd = child_of_type(p, "variable_declaration")
            if vd is not None:
                n = child_of_type(vd, "identifier")
                prop = self.text(n) if n is not None else ""
        label = f"{which} {prop}".strip()
        self.open_symbol(node, "accessor", which, qualified=self.qual(label), scope_name=label,
                               bindings={"property": prop, "accessor": which, "body": True})
        if which == "set":
            n = child_of_type(node, "identifier")
            if n is not None:
                self.ctx.declare(self.text(n), "parameter", node.start_byte, node.end_byte, position=n.start_byte)
        return None

    def visit_getter(self, node):
        return self._accessor(node, "get")

    def visit_setter(self, node):
        return self._accessor(node, "set")

    def visit_lambda_literal(self, node):
        sym = self.open_lambda(node, bindings={"body": True}, node_kind="lambda_literal")
        names = []
        lp = child_of_type(node, "lambda_parameters")
        if lp is not None:
            for vd in children_of_type(lp, "variable_declaration"):
                n = child_of_type(vd, "identifier")
                if n is not None:
                    names.append(self.text(n))
                    self.ctx.declare(self.text(n), "parameter", node.start_byte, node.end_byte, position=n.start_byte)
        sym["bindings"]["parameters"] = names
        return None

    def visit_anonymous_function(self, node):
        sym = self.open_lambda(node, bindings={"body": True, "anonymousFunction": True}, node_kind="anonymous_function")
        body = child_of_type(node, "function_body")
        rng = (body.start_byte, body.end_byte) if body is not None else (node.end_byte, node.end_byte)
        sym["bindings"]["parameters"] = self._params(node, rng)
        return None

    # -- variables ---------------------------------------------------------------------------------------
    def visit_property_declaration(self, node):
        vds = self._property_variables(node)
        value = None
        for c in node.children:
            if c.is_named and c.type not in ("variable_declaration", "multi_variable_declaration", "modifiers",
                                              "getter", "setter", "type_constraints", "property_delegate",
                                              "type_parameters", "user_type"):
                value = c
                break
        if value is not None and value.type == "property_delegate":
            value = None
        range_for = self.container_range if self._in_class_body(node) else self.local_range
        for vd in vds:
            n = child_of_type(vd, "identifier")
            if n is None:
                continue
            start, end = range_for(node)
            self.declare(self.text(n), "variable", start, end, mutable="val" not in self.text(node)[:8],
                         value=value if len(vds) == 1 else None, position=n.start_byte)
        return None

    @staticmethod
    def _property_variables(node):
        variables = []
        for child in node.children:
            if child.type == "variable_declaration":
                variables.append(child)
            elif child.type == "multi_variable_declaration":
                variables += children_of_type(child, "variable_declaration")
        return variables

    def visit_for_statement(self, node):
        for vd in children_of_type(node, "variable_declaration"):
            n = child_of_type(vd, "identifier")
            if n is not None:
                self.ctx.declare(self.text(n), "variable", node.start_byte, node.end_byte, position=n.start_byte)
        return None

    def visit_assignment(self, node):
        left = node.child_by_field_name("left")
        if left is not None and left.type == "identifier":
            end = self.end_of_ancestor(node, ("function_declaration", "lambda_literal", "source_file"))
            self.ctx.declare(self.text(left), "assignment", node.start_byte, end, mutable=True, position=node.start_byte)
        return None

    # -- calls -------------------------------------------------------------------------------------------
    def _rk(self, n) -> str | None:
        if n is None:
            return None
        return {"this_expression": "this", "super_expression": "super", "identifier": "identifier",
                "navigation_expression": "path"}.get(n.type, "expression")

    def visit_constructor_delegation_call(self, node):
        first = node.children[0] if node.children else None
        if first is not None:
            nm = self.text(first)
            self.callee_site(node, nm, {"kind": "identifier", "name": nm, "receiver": None, "receiverKind": None,
                                        "constructor": True})
        return None

    def visit_call_expression(self, node):
        callee = next((c for c in node.children if c.is_named), None)
        if callee is None:
            return None
        t = callee.type
        raw = self.text(callee)
        if t == "identifier":
            self.callee_site(node, raw, {"kind": "identifier", "name": raw, "receiver": None, "receiverKind": None})
        elif t == "navigation_expression":
            kids = [c for c in callee.children if c.is_named]
            if len(kids) >= 2 and kids[-1].type == "identifier":
                recv = kids[0] if len(kids) == 2 else None
                recv_text = self.ctx.source[callee.start_byte:kids[-1].start_byte].decode("utf-8", "replace")
                recv_text = recv_text.rstrip().rstrip("?").rstrip(".").rstrip()
                rk = self._rk(recv)
                self.callee_site(node, raw, {"kind": "path" if rk == "path" else "member", "name": self.text(kids[-1]),
                                             "receiver": recv_text, "receiverKind": rk,
                                             "safeCall": "?." in raw})
            else:
                self.callee_site(node, raw, self.dyn_binding(), dynamic=True)
        else:
            self.callee_site(node, raw, self.dyn_binding(), dynamic=True)
        return None


# ============================================================================================
# Scala
# ============================================================================================

class ScalaVisitor(_Base):
    closure_types = frozenset({"lambda_expression"})
    container_types = ("block", "template_body", "with_template_body", "enum_body", "compilation_unit", "case_clause")
    _CLASS_BODIES = ("template_body", "with_template_body", "enum_body")

    def __init__(self, ctx: common.Context):
        super().__init__(ctx)
        self._package_stack: dict[int, str | None] = {}

    def leave(self, node):
        if node.id in self._package_stack:
            self.package = self._package_stack.pop(node.id)
        super().leave(node)

    def _mods(self, node) -> set[str]:
        m = child_of_type(node, "modifiers")
        return set(self.text(m).split()) if m is not None else set()

    def _exported(self, mods: set[str]):
        return not (mods & {"private", "protected"})

    # -- package/imports ------------------------------------------------------------------------------
    def visit_package_clause(self, node):
        name = node.child_by_field_name("name")
        body = node.child_by_field_name("body")
        txt = ".".join(self.text(c) for c in name.children if c.is_named) if name is not None else ""
        if body is None:
            if txt:
                self.set_package(txt)
            return None
        old = self.package
        self._package_stack[node.id] = old
        self.package = txt if not old else f"{old}.{txt}"
        self.open_symbol(node, "namespace", txt, bindings={"package": txt, "body": True})
        return None

    def visit_import_declaration(self, node):
        path = ".".join(self.text(c) for c in fields_of(node, "path") if c.type == "identifier")
        sel = child_of_type(node, "namespace_selectors")
        direct_wild = child_of_type(node, "namespace_wildcard")
        raw = self.text(node).strip()
        if sel is None and direct_wild is None:
            self.import_site(node, path, _last(path), None, False, raw=raw)
            return None
        if direct_wild is not None:
            self.import_site(node, path, None, None, True, raw=raw, wildcardKind=self.text(direct_wild))
        if sel is not None:
            for c in sel.children:
                if c.is_named:
                    self._import_selector(c, path, raw)
        return None

    def _import_selector(self, node, path, raw):
        if node.type == "identifier":
            self.import_site(node, f"{path}.{self.text(node)}", self.text(node), None, False, raw=raw)
        elif node.type == "arrow_renamed_identifier":
            name = node.child_by_field_name("name")
            alias = node.child_by_field_name("alias")
            name_text = self.text(name) if name is not None else ""
            alias_text = self.text(alias) if alias is not None else None
            self.import_site(node, f"{path}.{name_text}", name_text, alias_text, False,
                             raw=raw, hidden=alias_text == "_")
        elif node.type == "namespace_wildcard":
            self.import_site(node, path, None, None, True, raw=raw, wildcardKind=self.text(node))
        else:  # given selectors etc.: kept lossless, not analyzed
            self.import_site(node, path, None, None, True, raw=raw, selector=self.text(node), given=True)

    # -- types --------------------------------------------------------------------------------------------
    def _extends(self, node) -> list[str]:
        ext = node.child_by_field_name("extend")
        if ext is None:
            return []
        return [self.text(c) for c in ext.children if c.type in ("type_identifier", "generic_type", "stable_type_identifier",
                                                                  "compound_type", "projected_type", "annotated_type")]

    def _class_like(self, node, kind: str, extra: dict | None = None):
        name = self.field_text(node, "name") or ""
        mods = self._mods(node)
        b = {"body": node.child_by_field_name("body") is not None, "modifiers": sorted(mods),
             "extends": self._extends(node)}
        if extra:
            b.update(extra)
        sym = self.type_symbol(node, kind, name, b, self._exported(mods))
        cp = node.child_by_field_name("class_parameters")
        names = []
        if cp is not None:
            for p in cp.children:
                if p.type == "class_parameter":
                    n = p.child_by_field_name("name")
                    if n is not None:
                        names.append(self.text(n))
                        self.ctx.declare(self.text(n), "parameter", node.start_byte, node.end_byte,
                                         position=n.start_byte)
        sym["bindings"]["parameters"] = names
        return None

    def visit_class_definition(self, node):
        return self._class_like(node, "class", {"case": "case" in self._mods(node)})

    def visit_object_definition(self, node):
        return self._class_like(node, "class", {"object": True})

    def visit_trait_definition(self, node):
        return self._class_like(node, "trait")

    def visit_enum_definition(self, node):
        return self._class_like(node, "enum")

    def visit_given_definition(self, node):
        return self._class_like(node, "class", {"given": True})

    def visit_extension_definition(self, node):
        self.open_symbol(node, "namespace", "", qualified=self.qual(f"<extension@{node.start_point[0] + 1}>"),
                         scope_name=f"<extension@{node.start_point[0] + 1}>", bindings={"extension": True, "body": True})
        for p in fields_of(node, "parameters"):
            for q in p.children:
                if q.type == "parameter":
                    n = q.child_by_field_name("name")
                    if n is not None:
                        self.ctx.declare(self.text(n), "parameter", node.start_byte, node.end_byte, position=n.start_byte)
        return None

    def visit_simple_enum_case(self, node):
        n = node.child_by_field_name("name")
        if n is not None:
            self.add_symbol(node, "property", self.text(n), bindings={"enumConstant": True}, exported=True)
        return None

    # -- callables -----------------------------------------------------------------------------------------
    def _function(self, node):
        name = self.field_text(node, "name") or ""
        mods = self._mods(node)
        body = node.child_by_field_name("body")
        p = node.parent
        in_class = p is not None and p.type in self._CLASS_BODIES
        in_ext = p is not None and p.type == "extension_definition"
        kind = "function"
        if name == "this" and in_class:
            kind = "constructor"
        elif in_class or in_ext:
            kind = "method"
        b = {"body": body is not None, "modifiers": sorted(mods), "abstract": body is None,
             "extension": bool(in_ext)}
        sym = self.open_symbol(node, kind, name, bindings=b, exported=self._exported(mods))
        rng = (body.start_byte, body.end_byte) if body is not None else (node.end_byte, node.end_byte)
        sym["bindings"]["parameters"] = self._function_parameters(node, rng)
        if kind in ("function", "method"):
            start, end = self.container_range(node)
            self.ctx.declare(name, "function", start, end, symbol_id=sym["id"], owner=self._outer_owner())
        return None

    def _function_parameters(self, node, rng):
        names = []
        for plist in fields_of(node, "parameters"):
            for parameter in plist.children:
                if parameter.type != "parameter":
                    continue
                name = parameter.child_by_field_name("name")
                if name is not None:
                    names.append(self.text(name))
                    self.ctx.declare(self.text(name), "parameter", rng[0], rng[1], position=name.start_byte)
        return names

    visit_function_definition = _function
    visit_function_declaration = _function

    def visit_lambda_expression(self, node):
        sym = self.open_lambda(node, bindings={"body": True}, node_kind="lambda_expression")
        names = []
        params = node.child_by_field_name("parameters")
        if params is not None:
            if params.type == "identifier":
                names = [self.text(params)]
            else:
                for b in kit_walk.collect_identifiers(params, _IDENT, skip_fields=frozenset({"type"})):
                    names.append(self.text(b))
            for n in names:
                self.ctx.declare(n, "parameter", node.start_byte, node.end_byte)
        sym["bindings"]["parameters"] = names
        return None

    # -- variables ------------------------------------------------------------------------------------------
    def _val(self, node):
        pat = node.child_by_field_name("pattern")
        value = node.child_by_field_name("value")
        if pat is None:
            return None
        p = node.parent
        member = p is not None and p.type in self._CLASS_BODIES
        if member:
            start, end = self.container_range(node)
        else:
            start, end = self.local_range(node)
        ids = collect_identifiers(pat, _IDENT, skip_fields=frozenset({"type"}))
        for i in ids:
            self.declare(self.text(i), "variable", start, end, mutable=node.type == "var_definition",
                         value=value if (pat.type == "identifier" and len(ids) == 1) else None,
                         position=i.start_byte)
        return None

    def visit_val_definition(self, node):
        return self._val(node)

    def visit_var_definition(self, node):
        return self._val(node)

    def visit_case_clause(self, node):
        pat = node.child_by_field_name("pattern")
        if pat is not None:
            for i in collect_identifiers(pat, _IDENT, skip_fields=frozenset({"type"})):
                t = self.text(i)
                if t and not t[0].isupper():
                    self.ctx.declare(t, "variable", node.start_byte, node.end_byte, position=i.start_byte)
        return None

    def visit_assignment_expression(self, node):
        left = node.child_by_field_name("left")
        if left is not None and left.type == "identifier":
            end = self.end_of_ancestor(node, ("function_definition", "lambda_expression", "compilation_unit"))
            self.ctx.declare(self.text(left), "assignment", node.start_byte, end, mutable=True,
                             position=node.start_byte)
        return None

    # -- calls ---------------------------------------------------------------------------------------------
    def _rk(self, n) -> str | None:
        if n is None:
            return None
        if n.type == "identifier":
            t = self.text(n)
            return {"this": "this", "super": "super"}.get(t, "identifier")
        return {"field_expression": "path"}.get(n.type, "expression")

    def _callee(self, fn, depth=0):
        t = fn.type
        raw = self.text(fn)
        if t == "identifier":
            return {"kind": "identifier", "name": raw, "receiver": None, "receiverKind": None}, False
        if t == "field_expression":
            value = fn.child_by_field_name("value")
            field = fn.child_by_field_name("field")
            if field is not None:
                rk = self._rk(value)
                return {"kind": "path" if rk == "path" else "member", "name": self.text(field),
                        "receiver": self.text(value) if value is not None else None, "receiverKind": rk}, False
        if t == "generic_function" and depth < 8:
            inner = fn.child_by_field_name("function")
            if inner is not None:
                b, d = self._callee(inner, depth + 1)
                b = dict(b)
                b["generic"] = True
                return b, d
        return self.dyn_binding(), True

    def visit_call_expression(self, node):
        fn = node.child_by_field_name("function")
        if fn is None:
            return None
        b, dyn = self._callee(fn)
        self.callee_site(node, self.text(fn), b, dynamic=dyn)
        return None

    def visit_instance_expression(self, node):
        t = next((c for c in node.children if c.type in ("type_identifier", "generic_type", "stable_type_identifier")), None)
        if t is not None:
            raw = self.text(t)
            base = _strip_generics(raw)
            recv, _, simple = base.rpartition(".")
            self.callee_site(node, raw, {"kind": "new", "name": simple, "receiver": recv or None,
                                         "receiverKind": "path" if recv else None})
        return None

    def visit_infix_expression(self, node):
        op = node.child_by_field_name("operator")
        left = node.child_by_field_name("left")
        if op is not None and op.type == "identifier":  # alphabetic infix call `a max b`
            self.callee_site(node, self.text(op), {"kind": "member", "name": self.text(op),
                                                    "receiver": self.text(left) if left is not None else None,
                                                    "receiverKind": self._rk(left), "infix": True})
        return None


_VISITORS = {"java": JavaVisitor, "groovy": GroovyVisitor, "kotlin": KotlinVisitor, "scala": ScalaVisitor}


def extract(language: str, _source: bytes, ctx: common.Context) -> dict:
    cls = _VISITORS.get(language)
    if cls is None:
        raise common.GrammarUnavailable(f"lang_jvm does not handle language '{language}'")
    return kit_walk.run_extract(ctx, cls)
