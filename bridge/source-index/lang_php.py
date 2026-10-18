"""PHP syntax recipe (pure Tree-sitter; never executes repository code).

Facts: namespaces, functions, classes/interfaces/traits/enums, methods/constructors,
closures and arrow functions; ``use`` imports (one site per imported name, group use
flattened), ``include``/``require`` (literal or ``__DIR__``-relative specifiers only),
trait/extends/implements references, and every function, method, static, ``new``
call site.  Variable callees, computed member names and reflection helpers
(``call_user_func`` ...) are persisted as dynamic sites.  Template ``text`` outside
``<?php`` tags is not code.  PHP class/function names are case-insensitive at run time;
IDs and bindings keep the exact spelling and never merge case variants.
"""
from __future__ import annotations

import common

PROFILES = {
    "php": {
        "extensions": [".php"],
        "grammar": "tree_sitter_php",
        "capsule": "language_php",
        "node_kinds": [
            "program", "namespace_definition", "namespace_use_declaration", "namespace_use_clause",
            "function_definition", "method_declaration", "class_declaration", "interface_declaration",
            "trait_declaration", "enum_declaration", "anonymous_function", "arrow_function",
            "function_call_expression", "member_call_expression", "scoped_call_expression",
            "object_creation_expression", "include_expression", "include_once_expression",
            "require_expression", "require_once_expression", "base_clause", "class_interface_clause",
            "use_declaration", "formal_parameters", "qualified_name", "encapsed_string",
        ],
    }
}

RAW_CAP = 4096
DYNAMIC_FUNCS = {
    "call_user_func", "call_user_func_array", "forward_static_call", "forward_static_call_array",
    "create_function", "eval", "assert", "extract", "compact", "func_get_args",
}
INCLUDE_NODES = {
    "include_expression": "include", "include_once_expression": "include_once",
    "require_expression": "require", "require_once_expression": "require_once",
}
CONDITIONAL = {
    "if_statement", "else_if_clause", "else_clause", "switch_statement", "case_statement", "default_statement",
    "match_expression", "match_conditional_expression", "while_statement", "do_statement", "for_statement",
    "foreach_statement", "catch_clause", "conditional_expression",
}
CLASS_LIKE = {
    "class_declaration": "class", "interface_declaration": "interface",
    "trait_declaration": "trait", "enum_declaration": "enum",
}


def _cap(text, binding=None):
    if len(text) <= RAW_CAP:
        return text
    if binding is not None:
        binding["rawTruncated"] = True
    return text[:RAW_CAP]


def _split(name):
    """Split a possibly qualified PHP name into (prefix|None, last)."""
    if "\\" in name:
        prefix, last = name.rsplit("\\", 1)
        return (prefix if prefix else "\\"), last
    return None, name


class _Walker:
    def __init__(self, ctx):
        self.ctx = ctx
        self.pushed: dict[int, int] = {}
        self.cond = 0
        self.cond_nodes: set[int] = set()
        self.ns: str | None = None
        self.ns_braced: dict[int, str | None] = {}
        self.names: dict = {}

    # -- helpers -------------------------------------------------------------
    def scope_range(self):
        ctx = self.ctx
        sym = ctx.symbol_by_id(ctx.owner) if ctx.owner else None
        if sym is None or sym["kind"] == "module":
            return 0, len(ctx.source)
        sp = sym["span"]
        return sp["byteStart"], sp["byteEnd"]

    def declare_here(self, name, kind, sym=None, site=None, node=None, mutable=False, owner=..., scope=None):
        if not name:
            return
        ctx = self.ctx
        own = ctx.owner if owner is ... else owner
        start, end = scope if scope is not None else self.scope_range()
        if kind in ("variable", "assignment", "parameter", "catch"):
            seen = self.names.setdefault(own, set())
            if kind in ("variable", "assignment"):
                kind = "assignment" if name in seen else "variable"
            seen.add(name)
        ctx.declare(
            name, kind, start, end, symbol_id=sym["id"] if sym else None, site_id=site["id"] if site else None,
            mutable=mutable, position=node.start_byte if node is not None else None, owner=own,
        )

    def push_node(self, node, sym):
        self.pushed[node.id] = self.cond
        self.cond = 0

    def top_level(self):
        sc = self.ctx.current_scope
        return sc is None or sc.kind in ("module", "namespace")

    def fqn(self, name):
        if not self.top_level():
            return None
        return (self.ns + "\\" + name) if self.ns else name

    def string_value(self, node):
        """(literal, ok) for a string literal without interpolation."""
        ctx = self.ctx
        if node is None:
            return None
        if node.type in ("string", "encapsed_string", "nowdoc"):
            parts = list(node.named_children)
            if any(c.type not in ("string_content", "escape_sequence", "string_value", "nowdoc_string") for c in parts):
                return None
            if not parts:
                return ""
            return ctx.source[parts[0].start_byte:parts[-1].end_byte].decode("utf-8", "replace")
        return None

    # -- parameters -----------------------------------------------------------
    def declare_params(self, params):
        if params is None:
            return
        for p in params.named_children:
            n = p.child_by_field_name("name")
            if n is not None and n.type == "variable_name":
                self.declare_here(self.ctx.text(n), "parameter", node=p, mutable=True)

    # -- include/require ------------------------------------------------------
    def concat_operands(self, node, out):
        """Flatten a `.` concatenation chain left-to-right (iterative: chains can be thousands deep)."""
        stack = [node]
        while stack:
            n = self.unwrap(stack.pop())
            if n.type == "binary_expression":
                op = n.child_by_field_name("operator")
                if op is not None and self.ctx.text(op) == ".":
                    stack.append(n.child_by_field_name("right"))
                    stack.append(n.child_by_field_name("left"))
                    continue
            out.append(n)

    @staticmethod
    def unwrap(node):
        while node is not None and node.type == "parenthesized_expression" and node.named_child_count == 1:
            node = node.named_children[0]
        return node

    def dir_prefix(self, node):
        """Return number of directory levels above the including file if node is __DIR__/dirname(...)."""
        ctx = self.ctx
        node = self.unwrap(node)
        if node.type == "name" and ctx.text(node) == "__DIR__":
            return 0
        if node.type == "function_call_expression":
            fn = node.child_by_field_name("function")
            args = node.child_by_field_name("arguments")
            if fn is not None and ctx.text(fn).lstrip("\\") == "dirname" and args is not None:
                items = [a for a in args.named_children if a.type == "argument"]
                if len(items) == 1 and items[0].named_children:
                    inner = items[0].named_children[0]
                    if inner.type == "name" and ctx.text(inner) == "__FILE__":
                        return 0
                    sub = self.dir_prefix(inner)
                    if sub is not None:
                        return sub + 1
        return None

    def include_site(self, node):
        ctx = self.ctx
        loader = INCLUDE_NODES[node.type]
        arg = self.unwrap(node.named_children[0]) if node.named_children else None
        specifier, relative_to, dynamic = None, None, True
        if arg is not None:
            lit = self.string_value(arg)
            if lit is not None:
                specifier, dynamic = lit, False
            else:
                ops: list = []
                self.concat_operands(arg, ops)
                head = self.dir_prefix(ops[0]) if ops else None
                rest = ops[1:] if head is not None else ops
                lits = [self.string_value(o) for o in rest]
                if lits and all(v is not None for v in lits):
                    tail = "".join(lits)
                    if head is None:
                        specifier = tail
                    else:
                        ups = "/".join([".."] * head)
                        specifier = tail if not ups else ups + ("" if tail.startswith("/") else "/") + tail
                        relative_to = "__DIR__"
                    dynamic = False
        binding = {
            "kind": "include", "specifier": specifier, "imported": None, "alias": None, "default": False,
            "namespace": False, "reexport": False, "typeOnly": False, "typeKind": None, "local": None,
            "relativeTo": relative_to, "loader": loader, "system": False,
        }
        if dynamic:
            binding["dynamic"] = True
        raw = _cap(ctx.text(node), binding)
        ctx.add_site("includes", raw, node, conditional=self.cond > 0, dynamic=dynamic, binding=binding)

    # -- use imports -----------------------------------------------------------
    @staticmethod
    def _keyword(node):
        for c in node.children:
            if not c.is_named and c.type in ("function", "const"):
                return c.type
        return None

    def use_sites(self, node):
        ctx = self.ctx
        decl_kind = self._keyword(node)
        group = None
        prefix = None
        for c in node.children:
            if c.type == "namespace_use_group":
                group = c
            elif c.type == "namespace_name" and group is None:
                prefix = ctx.text(c)
        clauses = []
        if group is not None:
            clauses = [c for c in group.named_children if c.type == "namespace_use_clause"]
        else:
            clauses = [c for c in node.named_children if c.type == "namespace_use_clause"]
        for clause in clauses:
            name_node = next((c for c in clause.named_children if c.type in ("qualified_name", "name", "namespace_name")), None)
            if name_node is None:
                continue
            written = ctx.text(name_node)
            spec = (prefix.rstrip("\\") + "\\" + written) if prefix is not None else written
            imported = spec.rsplit("\\", 1)[-1]
            alias_node = clause.child_by_field_name("alias")
            alias = ctx.text(alias_node) if alias_node is not None else None
            kw = self._keyword(clause) or decl_kind
            type_kind = {"function": "function", "const": "const"}.get(kw)
            binding = {
                "kind": "use", "specifier": spec, "imported": imported, "alias": alias, "default": False,
                "namespace": False, "reexport": False, "typeOnly": False, "typeKind": type_kind,
                "local": alias or imported, "relativeTo": None, "system": False,
            }
            site = ctx.add_site("imports", _cap(ctx.text(clause), binding), clause, conditional=self.cond > 0, binding=binding)
            self.declare_here(alias or imported, "import", site=site, node=clause, mutable=False)

    # -- calls -----------------------------------------------------------------
    def new_site(self, node):
        ctx = self.ctx
        target = None
        for c in node.named_children:
            if c.type in ("arguments", "comment", "attribute_list"):
                continue
            target = c
            break
        args = next((c for c in node.named_children if c.type == "arguments"), None)
        binding: dict = {"kind": "new", "name": None, "receiver": None, "receiverKind": None}
        dynamic = False
        resolution = None
        if target is None:
            return
        if target.type == "anonymous_class":
            binding["name"] = "<anonymous class>"
            binding["anonymous"] = True
            resolution = "unsupported"
            raw = "new class"
        elif target.type in ("name", "qualified_name"):
            text = ctx.text(target)
            prefix, last = _split(text)
            binding["name"], binding["receiver"] = last, prefix
            binding["receiverKind"] = "path" if prefix else None
            raw = "new " + text
        elif target.type == "relative_scope":
            text = ctx.text(target)
            binding["name"] = text
            binding["receiverKind"] = "super" if text == "parent" else "self"
            raw = "new " + text
        else:
            dynamic = True
            binding["kind"] = "dynamic"
            binding["dynamic"] = True
            binding["new"] = True
            raw = _cap(ctx.source[node.start_byte:(args.start_byte if args is not None else node.end_byte)].decode("utf-8", "replace").rstrip(), binding)
        ctx.add_site("calls", _cap(raw, binding), node, conditional=self.cond > 0, dynamic=dynamic, resolution=resolution, binding=binding)

    def call_site(self, node):
        ctx = self.ctx
        t = node.type
        args = node.child_by_field_name("arguments")
        end = args.start_byte if args is not None else node.end_byte
        raw = ctx.source[node.start_byte:end].decode("utf-8", "replace").rstrip()
        binding: dict = {"name": None, "receiver": None, "receiverKind": None}
        dynamic = False
        if t == "function_call_expression":
            fn = node.child_by_field_name("function")
            if fn is not None and fn.type in ("name", "qualified_name"):
                text = ctx.text(fn)
                prefix, last = _split(text)
                binding.update(kind="path" if prefix else "identifier", name=last, receiver=prefix,
                               receiverKind="path" if prefix else None)
                dynamic = last.lower() in DYNAMIC_FUNCS
            else:
                binding.update(kind="dynamic", dynamic=True)
                dynamic = True
                if fn is not None and fn.type == "variable_name":
                    binding["name"] = ctx.text(fn)
                    binding["variableCallee"] = True
        elif t in ("member_call_expression", "nullsafe_member_call_expression"):
            obj = node.child_by_field_name("object")
            name = node.child_by_field_name("name")
            rtext = ctx.text(obj) if obj is not None else None
            kind = "expression"
            if obj is not None and obj.type == "variable_name":
                kind = "this" if rtext == "$this" else "identifier"
            binding.update(kind="member", receiver=_cap(rtext or "", binding), receiverKind=kind)
            if t == "nullsafe_member_call_expression":
                binding["optional"] = True
            if name is not None and name.type == "name":
                binding["name"] = ctx.text(name)
            else:
                binding.update(kind="dynamic", dynamic=True, name=None)
                dynamic = True
        else:  # scoped_call_expression
            scope = node.child_by_field_name("scope")
            name = node.child_by_field_name("name")
            stext = ctx.text(scope) if scope is not None else ""
            if scope is not None and scope.type == "relative_scope":
                rk = "super" if stext == "parent" else "self"
            elif scope is not None and scope.type in ("name", "qualified_name"):
                rk = "path"
            else:
                rk = "expression"
            binding.update(kind="member" if rk != "path" else "path", receiver=_cap(stext, binding), receiverKind=rk)
            if name is not None and name.type == "name":
                binding["name"] = ctx.text(name)
            else:
                binding.update(kind="dynamic", dynamic=True, name=None)
                dynamic = True
            if rk == "expression":
                binding["dynamic"] = True
                dynamic = True
        ctx.add_site("calls", _cap(raw, binding), node, conditional=self.cond > 0, dynamic=dynamic, binding=binding)

    def reference(self, name_node, role, caller):
        ctx = self.ctx
        text = ctx.text(name_node)
        prefix, last = _split(text)
        ctx.add_site(
            "references", _cap(text), name_node, caller=caller, conditional=False,
            binding={"kind": "path" if prefix else "identifier", "name": last, "receiver": prefix,
                     "receiverKind": "path" if prefix else None, "role": role},
        )

    # -- traversal ---------------------------------------------------------------
    def enter(self, node):
        if not node.is_named:
            return True
        ctx = self.ctx
        t = node.type
        if t in CONDITIONAL:
            self.cond += 1
            self.cond_nodes.add(node.id)
        if t == "namespace_definition":
            name = node.child_by_field_name("name")
            body = node.child_by_field_name("body")
            text = ctx.text(name) if name is not None else None
            if body is not None:
                self.ns_braced[node.id] = self.ns
                self.ns = text
                sym = ctx.add_symbol("namespace", text or "", node, push=True, bindings={"namespace": text, "braced": True})
                self.push_node(node, sym)
            else:
                self.ns = text
            return True
        if t == "namespace_use_declaration":
            self.use_sites(node)
            return False
        if t in INCLUDE_NODES:
            self.include_site(node)
            return True
        if t == "function_definition":
            name_node = node.child_by_field_name("name")
            name = ctx.text(name_node)
            body = node.child_by_field_name("body")
            bindings = {"namespace": self.ns, "body": body is not None}
            fqn = self.fqn(name)
            if fqn is not None:
                bindings["fqn"] = fqn
            sym = ctx.add_symbol("function", name, node, bindings=bindings)
            self.declare_here(name, "function", sym=sym, node=name_node, mutable=self.cond > 0)
            ctx.push_scope(sym["id"], name, "function")
            self.push_node(node, sym)
            self.declare_params(node.child_by_field_name("parameters"))
            return True
        if t in CLASS_LIKE:
            name_node = node.child_by_field_name("name")
            name = ctx.text(name_node)
            outer = ctx.owner
            bindings: dict = {"namespace": self.ns}
            fqn = self.fqn(name)
            if fqn is not None:
                bindings["fqn"] = fqn
            extends, implements = [], []
            for c in node.named_children:
                if c.type == "base_clause":
                    extends = [ctx.text(n) for n in c.named_children if n.type in ("name", "qualified_name")]
                elif c.type == "class_interface_clause":
                    implements = [ctx.text(n) for n in c.named_children if n.type in ("name", "qualified_name")]
            if extends:
                bindings["extends"] = extends
            if implements:
                bindings["implements"] = implements
            sym = ctx.add_symbol(CLASS_LIKE[t], name, node, bindings=bindings)
            self.declare_here(name, "class", sym=sym, node=name_node, mutable=self.cond > 0)
            ctx.push_scope(sym["id"], name, CLASS_LIKE[t])
            self.push_node(node, sym)
            for c in node.named_children:
                if c.type == "base_clause":
                    for n in c.named_children:
                        if n.type in ("name", "qualified_name"):
                            self.reference(n, "extends", outer)
                elif c.type == "class_interface_clause":
                    for n in c.named_children:
                        if n.type in ("name", "qualified_name"):
                            self.reference(n, "implements", outer)
            return True
        if t == "anonymous_class":
            bindings = {"namespace": self.ns, "anonymous": True}
            outer = ctx.owner
            sym = ctx.add_symbol("class", "<anonymous class>", node, bindings=bindings)
            ctx.push_scope(sym["id"], None, "class")
            self.push_node(node, sym)
            for c in node.named_children:
                if c.type == "base_clause":
                    sym["bindings"]["extends"] = [ctx.text(n) for n in c.named_children if n.type in ("name", "qualified_name")]
                    for n in c.named_children:
                        if n.type in ("name", "qualified_name"):
                            self.reference(n, "extends", outer)
                elif c.type == "class_interface_clause":
                    sym["bindings"]["implements"] = [ctx.text(n) for n in c.named_children if n.type in ("name", "qualified_name")]
                    for n in c.named_children:
                        if n.type in ("name", "qualified_name"):
                            self.reference(n, "implements", outer)
            return True
        if t == "use_declaration" and ctx.current_scope is not None and ctx.current_scope.kind in ("class", "trait", "enum"):
            for n in node.named_children:
                if n.type in ("name", "qualified_name"):
                    self.reference(n, "trait", ctx.owner)
            return True
        if t == "method_declaration":
            name_node = node.child_by_field_name("name")
            name = ctx.text(name_node)
            body = node.child_by_field_name("body")
            mods = [c.type for c in node.named_children if c.type.endswith("_modifier")]
            vis = next((ctx.text(c) for c in node.named_children if c.type == "visibility_modifier"), "")
            bindings = {
                "static": "static_modifier" in mods, "visibility": vis, "abstract": "abstract_modifier" in mods,
                "body": body is not None,
            }
            kind = "constructor" if name.lower() == "__construct" else "method"
            sym = ctx.add_symbol(kind, name, node, bindings=bindings)
            ctx.push_scope(sym["id"], name, kind)
            self.push_node(node, sym)
            self.declare_params(node.child_by_field_name("parameters"))
            return True
        if t in ("anonymous_function", "arrow_function"):
            parent = node.parent
            outer = ctx.owner
            outer_range = self.scope_range()
            sym = ctx.add_symbol("lambda", "<closure>" if t == "anonymous_function" else "<arrow>", node,
                                 node_kind=t, bindings={"static": any(c.type == "static_modifier" for c in node.children)})
            ctx.push_scope(sym["id"], None, "lambda")
            self.push_node(node, sym)
            self.declare_params(node.child_by_field_name("parameters"))
            for c in node.named_children:
                if c.type == "anonymous_function_use_clause":
                    for v in c.named_children:
                        if v.type == "variable_name":
                            self.declare_here(ctx.text(v), "variable", node=v, mutable=True)
            if parent is not None and parent.type == "assignment_expression":
                left = parent.child_by_field_name("left")
                if left is not None and left.type == "variable_name":
                    name = ctx.text(left)
                    sym["bindings"]["assignedTo"] = name
                    self.declare_here(name, "variable", sym=sym, node=left, mutable=True, owner=outer, scope=outer_range)
            return True
        if t == "assignment_expression" or t == "augmented_assignment_expression":
            left = node.child_by_field_name("left")
            right = node.child_by_field_name("right")
            if left is not None and left.type == "variable_name":
                if not (right is not None and right.type in ("anonymous_function", "arrow_function")):
                    self.declare_here(ctx.text(left), "variable", node=left, mutable=True)
            return True
        if t == "catch_clause":
            n = node.child_by_field_name("name")
            if n is not None and n.type == "variable_name":
                self.declare_here(ctx.text(n), "catch", node=n, mutable=True)
            return True
        if t in ("function_call_expression", "member_call_expression", "nullsafe_member_call_expression", "scoped_call_expression"):
            self.call_site(node)
            return True
        if t == "object_creation_expression":
            self.new_site(node)
            return True
        return True

    def leave(self, node):
        if node.id in self.cond_nodes:
            self.cond_nodes.discard(node.id)
            self.cond -= 1
        if node.id in self.pushed:
            self.ctx.pop_scope()
            self.cond = self.pushed.pop(node.id)
        if node.id in self.ns_braced:
            self.ns = self.ns_braced.pop(node.id)


def extract(language: str, source: bytes, ctx: "common.Context") -> dict:
    tree = ctx.parse()
    root = tree.root_node
    ctx.module_symbol(root)
    walker = _Walker(ctx)
    ctx.traverse(root, walker.enter, walker.leave)
    ctx.pop_scope()
    return ctx.result()
