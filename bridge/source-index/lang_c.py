"""C / C++ / Objective-C syntax recipes (pure Tree-sitter AST facts).

Function names come from *declarator descent*: the function is the innermost
``function_declarator`` whose declarator is directly a name; a function pointer
variable (``int (*fp)(int)``) is a variable, never a function.  Macros are recorded
but never expanded; preprocessing is not performed, so conditional-compilation
branches are all parsed and every fact inside is flagged ``conditional``.
C++ member calls and ObjC message sends are never promoted by name.
"""
from __future__ import annotations

import common
import kit_walk
from kit_walk import Visitor, child_of_type, fields_of, strip_quotes

_C_KINDS = [
    "function_definition", "function_declarator", "call_expression", "preproc_include", "preproc_def",
    "preproc_function_def", "field_expression", "declaration", "parameter_declaration",
    "struct_specifier", "type_definition", "pointer_declarator", "init_declarator", "preproc_if",
]
_CPP_KINDS = _C_KINDS + [
    "class_specifier", "namespace_definition", "lambda_expression", "qualified_identifier",
    "template_declaration", "using_declaration", "new_expression", "alias_declaration",
    "template_function", "destructor_name", "operator_name",
]
_OBJC_KINDS = _C_KINDS + [
    "class_interface", "class_implementation", "protocol_declaration", "method_definition",
    "method_declaration", "message_expression", "method_parameter", "module_import", "block_literal",
    "property_declaration",
]

PROFILES = {
    "c": {"extensions": [".c", ".h"], "grammar": "tree_sitter_c", "capsule": "language", "node_kinds": _C_KINDS},
    "cpp": {
        "extensions": [".cpp", ".cc", ".cxx", ".hpp", ".hh", ".hxx", ".cu", ".cuh", ".metal"],
        "grammar": "tree_sitter_cpp", "capsule": "language", "node_kinds": _CPP_KINDS,
    },
    "objc": {"extensions": [".m", ".mm"], "grammar": "tree_sitter_objc", "capsule": "language", "node_kinds": _OBJC_KINDS},
}

_NAME_TYPES = frozenset({"identifier", "field_identifier", "type_identifier", "qualified_identifier",
                         "destructor_name", "operator_name", "operator_cast", "template_function",
                         "namespace_identifier"})
_WRAPPERS = frozenset({"pointer_declarator", "reference_declarator", "array_declarator", "parenthesized_declarator",
                       "attributed_declarator", "block_pointer_declarator", "init_declarator",
                       "abstract_pointer_declarator", "abstract_parenthesized_declarator"})
_CONTAINER_END = ("compound_statement", "declaration_list", "field_declaration_list", "translation_unit",
                  "for_statement", "for_range_loop", "enumerator_list")
_CONDITIONAL_PREPROC = frozenset({"preproc_if", "preproc_ifdef", "preproc_elif", "preproc_elifdef", "preproc_else"})
_TAGGED = frozenset({"class_specifier", "struct_specifier", "union_specifier", "enum_specifier"})
_CLASS_LIKE = {"class_specifier": "class", "struct_specifier": "struct", "union_specifier": "struct"}


def _ext(path: str) -> str:
    base = path.rsplit("/", 1)[-1]
    return base[base.rfind("."):].lower() if "." in base else ""


class CVisitor(Visitor):
    closure_types = frozenset({"lambda_expression", "block_literal"})

    def __init__(self, ctx: common.Context):
        super().__init__(ctx)
        self.dialect = ctx.file_language  # 'c' | 'cpp' | 'objc'
        self.sep = "::" if self.dialect == "cpp" else "."
        self.objc_seen = False
        self._objc_class_name: str | None = None
        self._typedef_names: dict[int, str] = {}
        self._class_names: set[str] = set()
        self._namespace_names: set[str] = set()
        self._anon_ns_depth = 0
        self._anon_ns_nodes: list[int] = []

    # -- conditional compilation ------------------------------------------------------
    def enter(self, node):
        if node.type in _CONDITIONAL_PREPROC and not self._is_include_guard(node):
            self.open_conditional(node)
        return super().enter(node)

    def leave(self, node):
        if node.type in ("class_interface", "class_implementation", "protocol_declaration"):
            self._objc_class_name = None
        if self._anon_ns_nodes and self._anon_ns_nodes[-1] == node.id:
            self._anon_ns_nodes.pop()
            self._anon_ns_depth -= 1
        super().leave(node)

    def _is_include_guard(self, node) -> bool:
        """``#ifndef X / #define X ... #endif`` wrapping a whole header is not conditional logic."""
        if node.type != "preproc_ifdef" or node.parent is None or node.parent.type != "translation_unit":
            return False
        for sib in node.parent.children:
            if sib.id != node.id and sib.type not in ("comment",):
                return False
        name = node.child_by_field_name("name")
        first = next((c for c in node.children if c.is_named and c.type not in ("comment", "identifier")), None)
        if name is None or first is None or first.type != "preproc_def":
            return False
        defined = first.child_by_field_name("name")
        return defined is not None and self.text(defined) == self.text(name)

    # -- includes / imports --------------------------------------------------------------
    def visit_preproc_include(self, node):
        path = node.child_by_field_name("path")
        if path is None:
            return None
        directive = self.text(node.children[0]).strip() if node.children else "#include"
        raw = self.text(path)
        dynamic = False
        if path.type == "system_lib_string":
            spec, system = raw[1:-1] if raw.startswith("<") and raw.endswith(">") else raw, True
        elif path.type == "string_literal":
            spec, system = strip_quotes(raw), False
        else:
            spec, system, dynamic = None, False, True  # computed include: `#include MACRO`
        if directive == "#import":
            self.objc_seen = True
        binding = {
            "kind": "include", "specifier": spec, "imported": None, "alias": None, "default": False,
            "namespace": False, "reexport": False, "typeOnly": False, "typeKind": None, "local": None,
            "system": system, "directive": directive, "relativeTo": "dir" if (spec is not None and not system) else None,
        }
        if dynamic:
            binding["dynamic"] = True
        self.site("includes", node, raw, binding, dynamic=dynamic, node_kind="preproc_include")
        return None

    def visit_module_import(self, node):
        self.objc_seen = True
        path = node.child_by_field_name("path")
        raw = self.text(path) if path is not None else self.text(node)
        binding = {"kind": "import", "specifier": raw, "imported": None, "alias": None, "default": False,
                   "namespace": False, "reexport": False, "typeOnly": False, "typeKind": None, "local": None,
                   "module": True}
        self.site("imports", node, raw, binding, node_kind="module_import")
        return None

    def visit_using_declaration(self, node):
        is_namespace = any(c.type == "namespace" for c in node.children)
        target = next((c for c in node.children if c.is_named), None)
        if target is None:
            return None
        raw = self.text(target)
        name = raw.rpartition("::")[2]
        binding = {"kind": "using", "specifier": raw, "imported": None if is_namespace else name, "alias": None,
                   "default": False, "namespace": is_namespace, "reexport": False, "typeOnly": False,
                   "typeKind": None, "local": None if is_namespace else name}
        site = self.site("imports", node, raw, binding, node_kind="using_declaration")
        if not is_namespace:
            start, end = self._container_range(node)
            self.ctx.declare(name, "import", start, end, site_id=site["id"])
        return None

    def visit_alias_declaration(self, node):
        name = self.field_text(node, "name")
        if name:
            start, end = self._container_range(node)
            self.ctx.declare(name, "type", start, end)
            t = node.child_by_field_name("type")
            self.site("references", node, self.text(t) if t is not None else name,
                      {"kind": "alias", "name": name, "receiver": None, "receiverKind": None,
                       "specifier": self.text(t) if t is not None else None, "local": name},
                      node_kind="alias_declaration")
        return None

    def visit_namespace_alias_definition(self, node):
        name_node = node.child_by_field_name("name")
        target = [c for c in node.children if c.is_named and (name_node is None or c.id != name_node.id)]
        if name_node is None or not target:
            return None
        name, raw = self.text(name_node), self.text(target[-1])
        site = self.site("imports", node, raw, {
            "kind": "alias", "specifier": raw, "imported": None, "alias": name, "default": False,
            "namespace": True, "reexport": False, "typeOnly": False, "typeKind": None, "local": name},
            node_kind="namespace_alias_definition")
        start, end = self._container_range(node)
        self.ctx.declare(name, "namespace", start, end, site_id=site["id"])
        return None

    # -- ranges -----------------------------------------------------------------------------
    def _container_range(self, node) -> tuple[int, int]:
        a = kit_walk.ancestor(node, _CONTAINER_END)
        if a is None:
            return 0, len(self.ctx.source)
        if a.type in ("translation_unit", "declaration_list", "field_declaration_list"):
            return a.start_byte, a.end_byte
        return a.start_byte, a.end_byte

    def _local_range(self, node) -> tuple[int, int]:
        a = kit_walk.ancestor(node, _CONTAINER_END)
        end = a.end_byte if a is not None else len(self.ctx.source)
        return node.end_byte, end

    # -- macros -------------------------------------------------------------------------------
    def _macro(self, node):
        name = self.field_text(node, "name")
        if not name:
            return None
        params = node.child_by_field_name("parameters")
        names = [self.text(c) for c in params.children if c.is_named] if params is not None else []
        value = node.child_by_field_name("value")
        b = {"functionLike": node.type == "preproc_function_def", "parameters": names, "body": True,
             "value": kit_walk.clip(self.text(value))[:512] if value is not None else None, "expanded": False}
        self.add_symbol(node, "macro", name, bindings=b)
        return None

    def visit_preproc_def(self, node):
        return self._macro(node)

    def visit_preproc_function_def(self, node):
        return self._macro(node)

    # -- declarator descent ----------------------------------------------------------------------
    def descend(self, decl):
        """Return (name_node, function_declarator, direct) following the declarator chain."""
        fdecl = None
        direct = False
        cur = decl
        for _ in range(64):
            if cur is None:
                return None, fdecl, direct
            t = cur.type
            if t in _NAME_TYPES:
                return cur, fdecl, direct
            if t == "function_declarator":
                fdecl, direct = cur, True
                cur = cur.child_by_field_name("declarator")
                continue
            if t in _WRAPPERS or t.endswith("_declarator"):
                direct = False
                nxt = cur.child_by_field_name("declarator")
                if nxt is None:
                    nxt = next((c for c in cur.children if c.is_named and c.type != "attribute_declaration"
                                and c.type not in ("type_qualifier",)), None)
                cur = nxt
                continue
            return None, fdecl, direct
        return None, fdecl, direct

    def _split_name(self, name_node):
        """(qualifier|None, simple_name, flags) for a declarator name node."""
        t = name_node.type
        if t == "qualified_identifier":
            inner = name_node
            while inner.type == "qualified_identifier":
                nxt = inner.child_by_field_name("name")
                if nxt is None:
                    break
                inner = nxt
            full = self.ctx.source[name_node.start_byte:inner.start_byte].decode("utf-8", "replace")
            qualifier = full.rstrip().removesuffix("::") or None
            simple, flags = self._simple_name(inner)
            return qualifier, simple, flags
        simple, flags = self._simple_name(name_node)
        return None, simple, flags

    def _simple_name(self, name_node):
        t = name_node.type
        flags = {}
        if t == "destructor_name":
            flags["destructor"] = True
        if t == "template_function":
            nm = name_node.child_by_field_name("name")
            flags["generic"] = True
            return self.text(nm) if nm is not None else self.text(name_node), flags
        return self.text(name_node), flags

    # -- function definitions -----------------------------------------------------------------------
    def _is_static(self, node) -> bool:
        return any(c.type == "storage_class_specifier" and self.text(c) == "static" for c in node.children)

    def _enclosing_class(self):
        s = self.ctx.current_scope
        if s is not None and s.kind in ("class", "struct"):
            return s
        return None

    def _kind_for(self, qualifier, simple, flags):
        """(kind, bindings) for a function definition/declaration named ``simple``."""
        cls = self._enclosing_class()
        b: dict = {}
        if flags.get("destructor"):
            b["destructor"] = True
        if flags.get("generic"):
            b["generic"] = True
        if cls is not None and qualifier is None:
            b["className"] = cls.name
            if simple == cls.name:
                return "constructor", b
            return "method", b
        if qualifier is not None and self.dialect == "cpp":
            last = qualifier.rsplit("::", 1)[-1]
            base = last.split("<", 1)[0]
            b["qualifier"] = qualifier
            if base in self._namespace_names and base not in self._class_names:
                b["qualifierKind"] = "namespace"
                return "function", b
            b["qualifierKind"] = "class" if base in self._class_names else "unknown"
            b["className"] = base
            if simple == base:
                return "constructor", b
            return "method", b
        return "function", b

    def _declare_params(self, fdecl, rng):
        names = []
        plist = fdecl.child_by_field_name("parameters") if fdecl is not None else None
        if plist is None:
            return names
        for p in plist.children:
            if p.type not in ("parameter_declaration", "optional_parameter_declaration",
                              "variadic_parameter_declaration"):
                continue
            d = p.child_by_field_name("declarator")
            if d is None:
                continue
            n, _f, _direct = self.descend(d)
            if n is not None:
                nm = self.text(n)
                names.append(nm)
                self.ctx.declare(nm, "parameter", rng[0], rng[1], position=n.start_byte)
        return names

    def visit_function_definition(self, node):
        decl = node.child_by_field_name("declarator")
        name_node, fdecl, direct = self.descend(decl)
        body = node.child_by_field_name("body")
        if name_node is None or fdecl is None or not direct:
            # not a recognizable function name (e.g. macro-mangled declarator): keep the scope/facts, mark partial
            self.ctx.mark_partial(f"unrecognized function declarator at line {node.start_point[0] + 1}; "
                                  "function symbol not recorded")
            return None
        qualifier, simple, flags = self._split_name(name_node)
        kind, bindings = self._kind_for(qualifier, simple, flags)
        bindings["body"] = body is not None
        bindings["static"] = self._is_static(node)
        if node.parent is not None and node.parent.type == "template_declaration":
            bindings["templated"] = True
        exported: bool | None = None
        if kind == "function":
            exported = not bindings["static"] and self._anon_ns_depth == 0
        label = f"{qualifier}::{simple}" if qualifier else simple
        sym = self.open_symbol(node, kind, simple, qualified=self.qual(label), scope_name=label,
                               bindings=bindings, exported=exported)
        rng = (body.start_byte, body.end_byte) if body is not None else (node.end_byte, node.end_byte)
        sym["bindings"]["parameters"] = self._declare_params(fdecl, rng)
        if kind == "function" or (kind in ("method", "constructor") and qualifier is None):
            start, end = self._container_range(node)
            self.ctx.declare(simple, "function", start, end, symbol_id=sym["id"], owner=self._outer_owner())
        return None

    def _outer_owner(self):
        scopes = self.ctx.scopes
        return scopes[-2].symbol_id if len(scopes) >= 2 else None

    def visit_lambda_expression(self, node):
        body = node.child_by_field_name("body")
        sym = self.open_lambda(node, bindings={"body": body is not None}, node_kind="lambda_expression")
        decl = node.child_by_field_name("declarator")
        rng = (body.start_byte, body.end_byte) if body is not None else (node.end_byte, node.end_byte)
        sym["bindings"]["parameters"] = self._declare_params(decl, rng)
        return None

    def visit_block_literal(self, node):
        self.objc_seen = True
        body = child_of_type(node, "compound_statement")
        sym = self.open_lambda(node, bindings={"body": body is not None, "block": True}, node_kind="block_literal")
        sym["bindings"]["parameters"] = []
        return None

    # -- declarations -------------------------------------------------------------------------------------
    def _declaration(self, node):
        in_class = node.type == "field_declaration"
        cls = self._enclosing_class()
        for d in fields_of(node, "declarator"):
            self._declarator(node, d, in_class, cls)
        return None

    def _declarator(self, node, decl, in_class, cls):
        value = None
        target = decl
        if decl.type == "init_declarator":
            value = decl.child_by_field_name("value")
            target = decl.child_by_field_name("declarator")
        if target is None:
            return
        name_node, fdecl, direct = self.descend(target)
        if name_node is None:
            return
        qualifier, simple, flags = self._split_name(name_node)
        if fdecl is not None and direct:
            self._signature(node, name_node, qualifier, simple, flags, in_class, cls)
            return
        if name_node.type not in ("identifier", "field_identifier"):
            return
        if in_class and cls is not None:
            start, end = self._container_range(node)
            self.declare(simple, "variable", start, end, value=value, position=name_node.start_byte)
        else:
            start, end = self._local_range(node)
            self.declare(simple, "variable", start, end, mutable=True, value=value,
                         position=name_node.start_byte)

    def _signature(self, node, name_node, qualifier, simple, flags, in_class, cls):
        bindings = {"body": False, "declarationOnly": True, "static": self._is_static(node)}
        if flags.get("destructor"):
            bindings["destructor"] = True
        if in_class and cls is not None:
            bindings["method"] = True
            bindings["className"] = cls.name
        elif qualifier:
            bindings["qualifier"] = qualifier
        if self.dialect != "cpp" or qualifier is None:
            sym = self.add_symbol(node, "signature", simple, node_kind=node.type, bindings=bindings,
                                  span=name_node, exported=None if in_class else not bindings["static"])
            start, end = self._container_range(node)
            self.ctx.declare(simple, "function", start, end, symbol_id=sym["id"])

    def visit_declaration(self, node):
        return self._declaration(node)

    def visit_field_declaration(self, node):
        return self._declaration(node)

    def visit_type_definition(self, node):
        t = node.child_by_field_name("type")
        for d in fields_of(node, "declarator"):
            n, _f, _direct = self.descend(d)
            if n is None:
                continue
            name = self.text(n)
            if t is not None and t.type in _TAGGED \
                    and t.child_by_field_name("name") is None and t.child_by_field_name("body") is not None \
                    and t.id not in self._typedef_names:
                self._typedef_names[t.id] = name
            start, end = self._container_range(node)
            self.ctx.declare(name, "type", start, end, position=n.start_byte)
        return None

    def visit_assignment_expression(self, node):
        left = node.child_by_field_name("left")
        if left is not None and left.type == "identifier":
            end = self.end_of_ancestor(node, ("function_definition", "lambda_expression", "translation_unit"))
            self.ctx.declare(self.text(left), "assignment", node.start_byte, end, mutable=True,
                             position=node.start_byte)
        return None

    # -- type containers ------------------------------------------------------------------------------------
    def _heritage(self, node):
        extends = []
        for c in node.children:
            if c.type == "base_class_clause":
                extends += [self.text(x) for x in c.children if x.is_named and x.type != "access_specifier"]
        return extends

    def _class_like(self, node):
        body = node.child_by_field_name("body")
        if body is None:
            return None  # a type *reference* (`struct S *p`), not a declaration
        name_node = node.child_by_field_name("name")
        name = self.text(name_node) if name_node is not None else self._typedef_names.get(node.id, "")
        kind = _CLASS_LIKE[node.type]
        b = {"body": True}
        if node.type == "union_specifier":
            b["union"] = True
        ext = self._heritage(node)
        if ext:
            b["extends"] = ext
        label = name or f"<anonymous@{node.start_point[0] + 1}:{node.start_point[1]}>"
        if name:
            self._class_names.add(name.split("<", 1)[0])
        sym = self.open_symbol(node, kind, name, qualified=self.qual(label), scope_name=label, bindings=b)
        if name and self.dialect == "cpp":
            start, end = self._container_range(node)
            self.ctx.declare(name, "class", start, end, symbol_id=sym["id"], owner=self._outer_owner())
        return None

    def visit_class_specifier(self, node):
        return self._class_like(node)

    def visit_struct_specifier(self, node):
        return self._class_like(node)

    def visit_union_specifier(self, node):
        return self._class_like(node)

    def visit_enum_specifier(self, node):
        body = node.child_by_field_name("body")
        name_node = node.child_by_field_name("name")
        if body is None:
            return None
        name = self.text(name_node) if name_node is not None else self._typedef_names.get(node.id, "")
        self.add_symbol(node, "enum", name, bindings={"body": True})
        return None

    def visit_namespace_definition(self, node):
        name_node = node.child_by_field_name("name")
        name = self.text(name_node) if name_node is not None else ""
        if name:
            self._namespace_names.add(name.rsplit("::", 1)[-1])
        label = name or f"<anonymous@{node.start_point[0] + 1}:{node.start_point[1]}>"
        sym = self.open_symbol(node, "namespace", name, qualified=self.qual(label), scope_name=label,
                               bindings={"anonymous": not name})
        if not name:
            self._anon_ns_depth += 1
            self._anon_ns_nodes.append(node.id)
        else:
            start, end = self._container_range(node)
            self.ctx.declare(name, "namespace", start, end, symbol_id=sym["id"], owner=self._outer_owner())
        return None

    # -- calls ------------------------------------------------------------------------------------------------
    def _callee(self, fn):
        t = fn.type
        raw = self.text(fn)
        dyn = {"kind": "dynamic", "name": None, "receiver": None, "receiverKind": None, "dynamic": True}
        if t == "identifier":
            return {"kind": "identifier", "name": raw, "receiver": None, "receiverKind": None}, False
        if t == "field_expression":
            return self._field_callee(fn, dyn)
        if t == "qualified_identifier":
            qualifier, simple, flags = self._split_name(fn)
            b = {"kind": "path", "name": simple, "receiver": qualifier, "receiverKind": "path"}
            if flags.get("generic"):
                b["generic"] = True
            return b, False
        if t == "template_function":
            nm = fn.child_by_field_name("name")
            return {"kind": "identifier", "name": self.text(nm) if nm is not None else raw, "receiver": None,
                    "receiverKind": None, "generic": True}, False
        if t in ("dependent_name", "operator_name"):
            return {"kind": "identifier", "name": raw, "receiver": None, "receiverKind": None}, False
        return dyn, True

    def _field_callee(self, fn, dyn):
        arg = fn.child_by_field_name("argument")
        field = fn.child_by_field_name("field")
        if field is None:
            return dyn, True
        name = self.text(field)
        binding = {"kind": "member", "name": name, "receiver": self.text(arg) if arg is not None else None,
                   "receiverKind": self._receiver_kind(arg)}
        op = next((c for c in fn.children if c.type in (".", "->", ".*", "->*")), None)
        binding["operator"] = op.type if op is not None else "."
        if field.type == "template_method":
            nm = field.child_by_field_name("name")
            binding["name"] = self.text(nm) if nm is not None else name
            binding["generic"] = True
        # C: a call through a struct field is a call through a function pointer
        return binding, self.dialect != "cpp"

    def _receiver_kind(self, node):
        if node is None:
            return None
        if node.type == "this":
            return "this"
        if node.type == "super":
            return "super"
        if node.type == "identifier":
            return "self" if self.text(node) == "self" and self.dialect == "objc" else "identifier"
        if node.type in ("qualified_identifier",):
            return "path"
        return "expression"

    def visit_call_expression(self, node):
        fn = node.child_by_field_name("function")
        if fn is None:
            return None
        binding, dynamic = self._callee(fn)
        self.site("calls", node, self.text(fn), binding, dynamic=dynamic, node_kind="call_expression")
        return None

    def visit_new_expression(self, node):
        t = node.child_by_field_name("type")
        if t is None:
            return None
        raw = self.text(t)
        simple = raw.split("<", 1)[0].rpartition("::")[2].strip()
        recv = raw.split("<", 1)[0].rpartition("::")[0] or None
        self.site("calls", node, raw, {"kind": "new", "name": simple, "receiver": recv,
                                       "receiverKind": "path" if recv else None},
                  node_kind="new_expression")
        return None

    # -- Objective-C --------------------------------------------------------------------------------------------
    def _selector(self, node) -> str:
        parts = []
        for c in node.children:
            if c.type == "identifier":
                nxt = c.next_named_sibling
                parts.append(self.text(c) + (":" if nxt is not None and nxt.type == "method_parameter" else ""))
        return "".join(parts)

    def _objc_class(self, node, kind):
        self.objc_seen = True
        idents = [c for c in node.children if c.type == "identifier"]
        name = self.text(idents[0]) if idents else ""
        cat = node.child_by_field_name("category")
        sup = node.child_by_field_name("superclass")
        b = {"body": node.type != "class_interface"}
        if cat is not None:
            b["category"] = self.text(cat)
        if sup is not None:
            b["extends"] = [self.text(sup)]
        protos = child_of_type(node, "parameterized_arguments")
        if protos is not None:
            b["implements"] = [self.text(x) for x in protos.children if x.is_named]
        label = f"{name}({b['category']})" if "category" in b else name
        sym = self.open_symbol(node, kind, name, qualified=label, scope_name="", bindings=b)
        self._objc_class_name = name
        if node.type == "class_interface" or node.type == "protocol_declaration":
            start, end = self._container_range(node)
            self.ctx.declare(name, "class", start, end, symbol_id=sym["id"], owner=self._outer_owner())
        return None

    def visit_class_interface(self, node):
        return self._objc_class(node, "class")

    def visit_class_implementation(self, node):
        return self._objc_class(node, "class")

    def visit_protocol_declaration(self, node):
        return self._objc_class(node, "interface")

    def _objc_method(self, node, defined: bool):
        self.objc_seen = True
        selector = self._selector(node)
        sign = next((c.type for c in node.children if c.type in ("-", "+")), "-")
        cname = self._objc_class_name or ""
        label = f"{sign}[{cname} {selector}]"
        b = {"selector": selector, "instance": sign == "-", "className": cname, "body": defined, "static": sign == "+"}
        if not defined:
            self.add_symbol(node, "signature", selector, qualified=label, bindings={**b, "declarationOnly": True})
            return None
        sym = self.open_symbol(node, "method", selector, qualified=label, scope_name=label, bindings=b)
        body = child_of_type(node, "compound_statement")
        rng = (body.start_byte, body.end_byte) if body is not None else (node.end_byte, node.end_byte)
        names = []
        for p in node.children:
            if p.type == "method_parameter":
                ids = [c for c in p.children if c.type == "identifier"]
                if ids:
                    nm = self.text(ids[-1])
                    names.append(nm)
                    self.ctx.declare(nm, "parameter", rng[0], rng[1], position=ids[-1].start_byte)
        sym["bindings"]["parameters"] = names
        return None

    def visit_method_definition(self, node):
        return self._objc_method(node, True)

    def visit_method_declaration(self, node):
        return self._objc_method(node, False)

    def visit_property_declaration(self, node):
        self.objc_seen = True
        for d in kit_walk.collect_identifiers(node, frozenset({"identifier"})):
            parent = d.parent
            if parent is not None and parent.type in ("struct_declarator", "pointer_declarator"):
                self.add_symbol(node, "property", self.text(d), span=d, node_kind="property_declaration")
        return False

    def _message_selector(self, node):
        parts = []
        kids = node.children
        for i, c in enumerate(kids):
            if node.field_name_for_child(i) == "method":
                nxt = c.next_named_sibling
                has_arg = nxt is not None and nxt.id != c.id and not (
                    nxt.parent is not None and any(
                        kids[j].id == nxt.id and node.field_name_for_child(j) == "method" for j in range(len(kids))))
                parts.append(self.text(c) + (":" if has_arg else ""))
        return "".join(parts)

    def visit_message_expression(self, node):
        self.objc_seen = True
        recv = node.child_by_field_name("receiver")
        sel = self._message_selector(node)
        binding = {"kind": "message", "name": sel, "selector": sel,
                   "receiver": self.text(recv) if recv is not None else None,
                   "receiverKind": self._receiver_kind(recv), "dynamic": True}
        self.site("calls", node, self.text(recv) + " " + sel if recv is not None else sel, binding, dynamic=True,
                  node_kind="message_expression")
        return None

    def visit_selector_expression(self, node):
        self.objc_seen = True
        raw = self.text(node)
        self.site("references", node, raw, {"kind": "selector", "name": raw, "receiver": None, "receiverKind": None,
                                            "dynamic": True}, dynamic=True, node_kind="selector_expression")
        return None


def _post(ctx: common.Context, v: CVisitor, root) -> None:
    ext = _ext(ctx.path)
    if ctx.file_language == "objc":
        if ctx.partial:
            ctx.mark_partial(
                f"'{ext}' is ambiguous (Objective-C/Objective-C++/MATLAB/other) and syntax errors occurred under "
                "the Objective-C grammar; facts are partial and the file is not claimed as MATLAB or Objective-C++")
        elif not v.objc_seen:
            ctx.mark_partial(
                f"'{ext}' file shows no Objective-C constructs; the extension is ambiguous (could be MATLAB or "
                "another language), so facts are partial")
    elif ctx.file_language == "cpp" and ext in (".cu", ".cuh", ".metal") and ctx.partial:
        ctx.mark_partial(
            f"'{ext}' is parsed with the C++ grammar; vendor extensions (__global__, <<<>>>, address spaces) are "
            "not supported and appear as syntax errors")


def extract(_language: str, _source: bytes, ctx: common.Context) -> dict:
    return kit_walk.run_extract(ctx, CVisitor, _post)
