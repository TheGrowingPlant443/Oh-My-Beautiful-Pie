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


def _dir_specifier(levels, tail):
    """Join `levels` parent hops ("..") onto a literal tail relative to __DIR__."""
    ups = "/".join([".."] * levels)
    if not ups:
        return tail
    sep = "" if tail.startswith("/") else "/"
    return ups + sep + tail


class _Walker:
    def __init__(self, ctx):
        self.ctx = ctx
        self.pushed: dict[int, int] = {}
        self.cond = 0
        self.cond_nodes: set[int] = set()
        self.ns: str | None = None
        self.ns_braced: dict[int, str | None] = {}
        self.names: dict = {}
        self.handlers = {
            "namespace_definition": self.enter_namespace,
            "namespace_use_declaration": self.enter_use,
            "function_definition": self.enter_function,
            "anonymous_class": self.enter_anonymous_class,
            "use_declaration": self.enter_trait_use,
            "method_declaration": self.enter_method,
            "anonymous_function": self.enter_lambda,
            "arrow_function": self.enter_lambda,
            "assignment_expression": self.enter_assignment,
            "augmented_assignment_expression": self.enter_assignment,
            "catch_clause": self.enter_catch,
            "object_creation_expression": self.enter_new,
            **dict.fromkeys(CLASS_LIKE, self.enter_class),
            **dict.fromkeys(INCLUDE_NODES, self.enter_include),
            **dict.fromkeys(
                ("function_call_expression", "member_call_expression", "nullsafe_member_call_expression", "scoped_call_expression"),
                self.enter_call,
            ),
        }

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

    def push_node(self, node):
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

    def dirname_prefix(self, node):
        """Directory levels for a dirname(__FILE__ | <dir expression>) call, else None."""
        ctx = self.ctx
        fn = node.child_by_field_name("function")
        args = node.child_by_field_name("arguments")
        if fn is None or args is None or ctx.text(fn).lstrip("\\") != "dirname":
            return None
        items = [a for a in args.named_children if a.type == "argument"]
        if len(items) != 1 or not items[0].named_children:
            return None
        inner = items[0].named_children[0]
        if inner.type == "name" and ctx.text(inner) == "__FILE__":
            return 0
        sub = self.dir_prefix(inner)
        return None if sub is None else sub + 1

    def dir_prefix(self, node):
        """Return number of directory levels above the including file if node is __DIR__/dirname(...)."""
        node = self.unwrap(node)
        if node.type == "name" and self.ctx.text(node) == "__DIR__":
            return 0
        if node.type == "function_call_expression":
            return self.dirname_prefix(node)
        return None

    def include_specifier(self, arg):
        """(specifier, relativeTo, dynamic) for an include argument; only literals and __DIR__ chains resolve."""
        lit = self.string_value(arg)
        if lit is not None:
            return lit, None, False
        ops: list = []
        self.concat_operands(arg, ops)
        head = self.dir_prefix(ops[0]) if ops else None
        lits = [self.string_value(o) for o in (ops[1:] if head is not None else ops)]
        if not lits or any(v is None for v in lits):
            return None, None, True
        tail = "".join(lits)
        if head is None:
            return tail, None, False
        return _dir_specifier(head, tail), "__DIR__", False

    def include_site(self, node):
        ctx = self.ctx
        loader = INCLUDE_NODES[node.type]
        arg = self.unwrap(node.named_children[0]) if node.named_children else None
        specifier, relative_to, dynamic = (None, None, True) if arg is None else self.include_specifier(arg)
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

    def _use_group(self, node):
        """(group node | None, name prefix written before the group)."""
        prefix = None
        for c in node.children:
            if c.type == "namespace_use_group":
                return c, prefix
            if c.type == "namespace_name":
                prefix = self.ctx.text(c)
        return None, prefix

    def use_clause_site(self, clause, prefix, decl_kind):
        ctx = self.ctx
        name_node = next((c for c in clause.named_children if c.type in ("qualified_name", "name", "namespace_name")), None)
        if name_node is None:
            return
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

    def use_sites(self, node):
        decl_kind = self._keyword(node)
        group, prefix = self._use_group(node)
        for clause in (group if group is not None else node).named_children:
            if clause.type == "namespace_use_clause":
                self.use_clause_site(clause, prefix, decl_kind)

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
        if target is None:
            return
        raw, dynamic, resolution = self._new_target_binding(node, target, args, binding)
        ctx.add_site("calls", _cap(raw, binding), node, conditional=self.cond > 0, dynamic=dynamic, resolution=resolution, binding=binding)

    def _new_target_binding(self, node, target, args, binding):
        ctx = self.ctx
        if target.type == "anonymous_class":
            binding["name"] = "<anonymous class>"
            binding["anonymous"] = True
            return "new class", False, "unsupported"
        if target.type in ("name", "qualified_name"):
            text = ctx.text(target)
            prefix, last = _split(text)
            binding["name"], binding["receiver"] = last, prefix
            binding["receiverKind"] = "path" if prefix else None
            return "new " + text, False, None
        if target.type == "relative_scope":
            text = ctx.text(target)
            binding["name"] = text
            binding["receiverKind"] = "super" if text == "parent" else "self"
            return "new " + text, False, None
        binding["kind"] = "dynamic"
        binding["dynamic"] = True
        binding["new"] = True
        end = args.start_byte if args is not None else node.end_byte
        raw = _cap(ctx.source[node.start_byte:end].decode("utf-8", "replace").rstrip(), binding)
        return raw, True, None

    def _call_name(self, name, binding):
        """Set the called member name; True when the name is computed (dynamic)."""
        if name is not None and name.type == "name":
            binding["name"] = self.ctx.text(name)
            return False
        binding.update(kind="dynamic", dynamic=True, name=None)
        return True

    def function_call_binding(self, node, binding):
        ctx = self.ctx
        fn = node.child_by_field_name("function")
        if fn is not None and fn.type in ("name", "qualified_name"):
            text = ctx.text(fn)
            prefix, last = _split(text)
            binding.update(kind="path" if prefix else "identifier", name=last, receiver=prefix,
                           receiverKind="path" if prefix else None)
            return last.lower() in DYNAMIC_FUNCS
        binding.update(kind="dynamic", dynamic=True)
        if fn is not None and fn.type == "variable_name":
            binding["name"] = ctx.text(fn)
            binding["variableCallee"] = True
        return True

    def member_call_binding(self, node, binding):
        ctx = self.ctx
        obj = node.child_by_field_name("object")
        rtext = ctx.text(obj) if obj is not None else None
        kind = "expression"
        if obj is not None and obj.type == "variable_name":
            kind = "this" if rtext == "$this" else "identifier"
        binding.update(kind="member", receiver=_cap(rtext or "", binding), receiverKind=kind)
        if node.type == "nullsafe_member_call_expression":
            binding["optional"] = True
        return self._call_name(node.child_by_field_name("name"), binding)

    @staticmethod
    def _scope_kind(scope, stext):
        if scope is not None and scope.type == "relative_scope":
            return "super" if stext == "parent" else "self"
        if scope is not None and scope.type in ("name", "qualified_name"):
            return "path"
        return "expression"

    def scoped_call_binding(self, node, binding):
        scope = node.child_by_field_name("scope")
        stext = self.ctx.text(scope) if scope is not None else ""
        rk = self._scope_kind(scope, stext)
        binding.update(kind="member" if rk != "path" else "path", receiver=_cap(stext, binding), receiverKind=rk)
        dynamic = self._call_name(node.child_by_field_name("name"), binding)
        if rk == "expression":
            binding["dynamic"] = True
            dynamic = True
        return dynamic

    def call_site(self, node):
        ctx = self.ctx
        t = node.type
        args = node.child_by_field_name("arguments")
        end = args.start_byte if args is not None else node.end_byte
        raw = ctx.source[node.start_byte:end].decode("utf-8", "replace").rstrip()
        binding: dict = {"name": None, "receiver": None, "receiverKind": None}
        if t == "function_call_expression":
            dynamic = self.function_call_binding(node, binding)
        elif t in ("member_call_expression", "nullsafe_member_call_expression"):
            dynamic = self.member_call_binding(node, binding)
        else:  # scoped_call_expression
            dynamic = self.scoped_call_binding(node, binding)
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
        t = node.type
        if t in CONDITIONAL:
            self.cond += 1
            self.cond_nodes.add(node.id)
        handler = self.handlers.get(t)
        return True if handler is None else handler(node)

    def enter_namespace(self, node):
        ctx = self.ctx
        name = node.child_by_field_name("name")
        body = node.child_by_field_name("body")
        text = ctx.text(name) if name is not None else None
        if body is not None:
            self.ns_braced[node.id] = self.ns
            self.ns = text
            ctx.add_symbol("namespace", text or "", node, push=True, bindings={"namespace": text, "braced": True})
            self.push_node(node)
        else:
            self.ns = text
        return True

    def enter_use(self, node):
        self.use_sites(node)
        return False

    def enter_include(self, node):
        self.include_site(node)
        return True

    def enter_function(self, node):
        ctx = self.ctx
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
        self.push_node(node)
        self.declare_params(node.child_by_field_name("parameters"))
        return True

    @staticmethod
    def heritage(node):
        """(role, name nodes) per extends/implements clause of a class-like node."""
        roles = {"base_clause": "extends", "class_interface_clause": "implements"}
        return [
            (roles[c.type], [n for n in c.named_children if n.type in ("name", "qualified_name")])
            for c in node.named_children if c.type in roles
        ]

    def heritage_references(self, heritage, outer):
        for role, names in heritage:
            for n in names:
                self.reference(n, role, outer)

    def enter_class(self, node):
        ctx = self.ctx
        t = node.type
        name_node = node.child_by_field_name("name")
        name = ctx.text(name_node)
        outer = ctx.owner
        bindings: dict = {"namespace": self.ns}
        fqn = self.fqn(name)
        if fqn is not None:
            bindings["fqn"] = fqn
        heritage = self.heritage(node)
        for role, names in heritage:
            if names:
                bindings[role] = [ctx.text(n) for n in names]
        sym = ctx.add_symbol(CLASS_LIKE[t], name, node, bindings=bindings)
        self.declare_here(name, "class", sym=sym, node=name_node, mutable=self.cond > 0)
        ctx.push_scope(sym["id"], name, CLASS_LIKE[t])
        self.push_node(node)
        self.heritage_references(heritage, outer)
        return True

    def enter_anonymous_class(self, node):
        ctx = self.ctx
        outer = ctx.owner
        sym = ctx.add_symbol("class", "<anonymous class>", node, bindings={"namespace": self.ns, "anonymous": True})
        ctx.push_scope(sym["id"], None, "class")
        self.push_node(node)
        heritage = self.heritage(node)
        for role, names in heritage:
            sym["bindings"][role] = [ctx.text(n) for n in names]
        self.heritage_references(heritage, outer)
        return True

    def enter_trait_use(self, node):
        ctx = self.ctx
        if ctx.current_scope is not None and ctx.current_scope.kind in ("class", "trait", "enum"):
            for n in node.named_children:
                if n.type in ("name", "qualified_name"):
                    self.reference(n, "trait", ctx.owner)
        return True

    def enter_method(self, node):
        ctx = self.ctx
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
        self.push_node(node)
        self.declare_params(node.child_by_field_name("parameters"))
        return True

    def declare_use_vars(self, node):
        for c in node.named_children:
            if c.type == "anonymous_function_use_clause":
                for v in c.named_children:
                    if v.type == "variable_name":
                        self.declare_here(self.ctx.text(v), "variable", node=v, mutable=True)

    def declare_assigned_lambda(self, parent, sym, outer, outer_range):
        if parent is None or parent.type != "assignment_expression":
            return
        left = parent.child_by_field_name("left")
        if left is None or left.type != "variable_name":
            return
        name = self.ctx.text(left)
        sym["bindings"]["assignedTo"] = name
        self.declare_here(name, "variable", sym=sym, node=left, mutable=True, owner=outer, scope=outer_range)

    def enter_lambda(self, node):
        ctx = self.ctx
        parent = node.parent
        outer = ctx.owner
        outer_range = self.scope_range()
        sym = ctx.add_symbol("lambda", "<closure>" if node.type == "anonymous_function" else "<arrow>", node,
                             node_kind=node.type, bindings={"static": any(c.type == "static_modifier" for c in node.children)})
        ctx.push_scope(sym["id"], None, "lambda")
        self.push_node(node)
        self.declare_params(node.child_by_field_name("parameters"))
        self.declare_use_vars(node)
        self.declare_assigned_lambda(parent, sym, outer, outer_range)
        return True

    def enter_assignment(self, node):
        left = node.child_by_field_name("left")
        right = node.child_by_field_name("right")
        is_lambda = right is not None and right.type in ("anonymous_function", "arrow_function")
        if left is not None and left.type == "variable_name" and not is_lambda:
            self.declare_here(self.ctx.text(left), "variable", node=left, mutable=True)
        return True

    def enter_catch(self, node):
        n = node.child_by_field_name("name")
        if n is not None and n.type == "variable_name":
            self.declare_here(self.ctx.text(n), "catch", node=n, mutable=True)
        return True

    def enter_call(self, node):
        self.call_site(node)
        return True

    def enter_new(self, node):
        self.new_site(node)
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


def extract(_language: str, _source: bytes, ctx: "common.Context") -> dict:
    tree = ctx.parse()
    root = tree.root_node
    ctx.module_symbol(root)
    walker = _Walker(ctx)
    ctx.traverse(root, walker.enter, walker.leave)
    ctx.pop_scope()
    return ctx.result()
