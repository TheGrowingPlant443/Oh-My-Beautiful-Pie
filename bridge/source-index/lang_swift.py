"""Swift syntax recipe (pure Tree-sitter; never executes repository code).

Facts: classes/structs/enums/actors/protocols/extensions, functions, methods, ``init``/
``deinit``, subscripts, computed-property accessors and closures; ``import`` declarations
(kind-qualified imports keep the qualifier in ``typeKind``); every call, initializer
(``Type<Args>(...)``), message-style member call and ``#macro`` invocation.  Swift has
overloading, implicit ``self`` and protocol dispatch, so no site is promoted by name:
methods are not lexically declared and bare ``f(x)`` stays unresolved for the resolver.
``#if`` regions are parsed but their contents are flagged conditional.
"""
from __future__ import annotations

import common

PROFILES = {
    "swift": {
        "extensions": [".swift"],
        "grammar": "tree_sitter_swift",
        "capsule": "language",
        "node_kinds": [
            "source_file", "import_declaration", "class_declaration", "protocol_declaration",
            "function_declaration", "protocol_function_declaration", "init_declaration", "deinit_declaration",
            "subscript_declaration", "property_declaration", "computed_getter", "computed_setter",
            "computed_property", "lambda_literal", "call_expression", "call_suffix", "navigation_expression",
            "navigation_suffix", "constructor_expression", "macro_invocation", "inheritance_specifier",
            "parameter", "directive", "if_statement", "guard_statement", "function_body", "class_body",
        ],
    }
}

RAW_CAP = 4096
TYPE_KINDS = {"class": "class", "struct": "struct", "enum": "enum", "actor": "class", "extension": "impl"}
TYPE_SCOPES = {"class", "struct", "enum", "interface", "impl"}
FUNC_SCOPES = {"function", "method", "constructor", "accessor", "lambda"}
CONDITIONAL = {
    "if_statement", "guard_statement", "switch_statement", "switch_entry", "while_statement", "repeat_while_statement",
    "for_statement", "catch_block", "ternary_expression", "else_statement",
}
BLOCK_TYPES = {"statements", "function_body", "lambda_literal", "source_file", "computed_property", "class_body"}


def _cap(text, binding=None):
    if len(text) <= RAW_CAP:
        return text
    if binding is not None:
        binding["rawTruncated"] = True
    return text[:RAW_CAP]


class _Walker:
    def __init__(self, ctx):
        self.ctx = ctx
        self.pushed: dict[int, int] = {}
        self.cond = 0
        self.pp = 0
        self.cond_nodes: set[int] = set()
        self.names: dict = {}

    # -- helpers -------------------------------------------------------------
    def scope_range(self):
        ctx = self.ctx
        sym = ctx.symbol_by_id(ctx.owner) if ctx.owner else None
        if sym is None or sym["kind"] == "module":
            return 0, len(ctx.source)
        sp = sym["span"]
        return sp["byteStart"], sp["byteEnd"]

    @staticmethod
    def block_range(node):
        p = node.parent
        while p is not None and p.type not in BLOCK_TYPES:
            p = p.parent
        return (p.start_byte, p.end_byte) if p is not None else (node.start_byte, node.end_byte)

    def declare(self, name, kind, start, end, *, sym=None, node=None, mutable=False, owner=...):
        if not name:
            return
        ctx = self.ctx
        own = ctx.owner if owner is ... else owner
        if kind in ("variable", "parameter"):
            seen = self.names.setdefault(own, set())
            seen.add(name)
        ctx.declare(name, kind, start, end, symbol_id=sym["id"] if sym else None, mutable=mutable,
                    position=node.start_byte if node is not None else None, owner=own)

    def push(self, node, sym, name, kind):
        self.ctx.push_scope(sym["id"], name, kind)
        self.pushed[node.id] = self.cond
        self.cond = self.pp  # nested scopes stay conditional on `#if` regions only

    def in_type_scope(self):
        sc = self.ctx.current_scope
        return sc is not None and sc.kind in TYPE_SCOPES

    def in_func_scope(self):
        sc = self.ctx.current_scope
        return sc is not None and sc.kind in FUNC_SCOPES

    def declare_params(self, node, params=None, start=None, end=None):
        ctx = self.ctx
        s, e = (node.start_byte, node.end_byte) if start is None else (start, end)
        for p in (params if params is not None else node.children):
            if p.type not in ("parameter", "lambda_parameter"):
                continue
            for i, c in enumerate(p.children):
                if p.field_name_for_child(i) == "name" and c.type == "simple_identifier":
                    self.declare(ctx.text(c), "parameter", s, e, node=p, mutable=False)
                    break

    def bound_names(self, node):
        out = []
        stack = [node]
        while stack:
            n = stack.pop()
            for i, c in enumerate(n.children):
                if n.field_name_for_child(i) == "bound_identifier" and c.type == "simple_identifier":
                    out.append(c)
                elif c.is_named and c.type in ("pattern", "tuple_pattern", "pattern_bound_identifier"):
                    stack.append(c)
        out.sort(key=lambda c: c.start_byte)
        return out

    # -- sites ---------------------------------------------------------------
    def import_site(self, node):
        ctx = self.ctx
        ident = next((c for c in node.named_children if c.type == "identifier"), None)
        if ident is None:
            ctx.mark_partial("import declaration without module path")
            return
        spec = ".".join(ctx.text(c) for c in ident.named_children)
        kind = next((c.type for c in node.children if not c.is_named and c.type in
                     ("typealias", "struct", "class", "enum", "protocol", "let", "var", "func")), None)
        attrs = [ctx.text(a) for m in node.named_children if m.type == "modifiers" for a in m.named_children if a.type == "attribute"]
        binding = {
            "kind": "import", "specifier": spec, "imported": spec.rsplit(".", 1)[-1] if kind else None,
            "alias": None, "default": False, "namespace": not kind, "reexport": any("_exported" in a for a in attrs),
            "typeOnly": False, "typeKind": kind, "local": None, "relativeTo": None, "system": False,
            "attributes": attrs,
        }
        ctx.add_site("imports", _cap(ctx.text(node), binding), node, conditional=self.cond > 0, binding=binding)

    @staticmethod
    def rightmost(expr):
        """Rightmost operand of a (mis-associated) binary expression; flag tells if it changed."""
        flag = False
        while expr is not None and expr.child_by_field_name("rhs") is not None:
            expr = expr.child_by_field_name("rhs")
            flag = True
        return expr, flag

    def callee_start(self, expr):
        cur = expr
        while cur is not None and cur.type == "navigation_expression":
            target = cur.child_by_field_name("target")
            if target is None:
                return cur.start_byte
            target, _ = self.rightmost(target)
            if target.type == "navigation_expression":
                cur = target
                continue
            return target.start_byte
        return cur.start_byte if cur is not None else 0

    def callee(self, expr):
        """Binding fields for the callee expression of a call_expression.

        tree-sitter-swift attaches a trailing call suffix or navigation to a whole binary expression
        (`a + b.f(x)` parses as call(additive(a, b.f), (x))), so operands are re-associated here.
        """
        ctx = self.ctx
        binding: dict = {"name": None, "receiver": None, "receiverKind": None}
        dynamic = False
        if expr is None:
            binding.update(kind="dynamic", dynamic=True)
            return binding, True
        if expr.type == "simple_identifier":
            binding.update(kind="identifier", name=ctx.text(expr))
        elif expr.type == "navigation_expression":
            target = expr.child_by_field_name("target")
            suffix = expr.child_by_field_name("suffix")
            sname = None
            if suffix is not None:
                inner = suffix.child_by_field_name("suffix")
                sname = ctx.text(inner) if inner is not None else None
            rtext = None
            rk = "expression"
            if target is not None:
                target, moved = self.rightmost(target)
                rtext = ctx.source[self.callee_start(target):target.end_byte].decode("utf-8", "replace")
                rk = {"self_expression": "self", "super_expression": "super", "simple_identifier": "identifier"}.get(target.type, "expression")
                if target.type in ("user_type", "type_identifier"):
                    rk = "path"
                if moved:
                    binding["reassociated"] = True
            binding.update(kind="member", name=sname, receiver=_cap(rtext, binding) if rtext is not None else None, receiverKind=rk)
            if target is not None and suffix is not None and b"?" in ctx.source[target.end_byte:suffix.start_byte]:
                binding["optional"] = True
            if target is None or sname is None:
                binding.update(kind="dynamic", dynamic=True)  # implicit member `.foo()` / unknown suffix
                dynamic = True
        else:
            binding.update(kind="dynamic", dynamic=True)
            dynamic = True
        return binding, dynamic

    def call_site(self, node):
        ctx = self.ctx
        kids = node.named_children
        expr = kids[0] if kids else None
        suffix = next((c for c in kids if c.type == "call_suffix"), None)
        reassociated = False
        if expr is suffix:
            expr = None
        else:
            expr, reassociated = self.rightmost(expr)
        end = suffix.start_byte if suffix is not None else node.end_byte
        start = self.callee_start(expr) if expr is not None else node.start_byte
        raw = ctx.source[start:end].decode("utf-8", "replace").rstrip()
        binding, dynamic = self.callee(expr)
        if reassociated:
            binding["reassociated"] = True
        site_range = (start, node.end_byte) if start != node.start_byte else node
        ctx.add_site("calls", _cap(raw, binding), site_range, node_kind="call_expression", conditional=self.cond > 0, dynamic=dynamic, binding=binding)

    def constructor_site(self, node):
        ctx = self.ctx
        ct = node.child_by_field_name("constructed_type")
        ctext = ""
        name = None
        binding: dict = {"kind": "new", "name": None, "receiver": None, "receiverKind": None, "initializer": True}
        if ct is not None:
            idents = [ctx.text(c) for c in ct.named_children if c.type == "type_identifier"]
            ctext = ".".join(idents) if idents else ctx.text(ct)
            if idents:
                binding["name"] = idents[-1]
                binding["receiver"] = ".".join(idents[:-1]) or None
                binding["receiverKind"] = "path" if binding["receiver"] else None
            else:
                binding.update(kind="dynamic", dynamic=True)
        suffix = next((c for c in node.named_children if c.type == "constructor_suffix"), None)
        end = suffix.start_byte if suffix is not None else node.end_byte
        raw = ctx.source[node.start_byte:end].decode("utf-8", "replace").rstrip()
        ctx.add_site("calls", _cap(raw, binding), node, conditional=self.cond > 0, dynamic=binding.get("dynamic", False), binding=binding)

    def macro_site(self, node):
        ctx = self.ctx
        ident = next((c for c in node.named_children if c.type == "simple_identifier"), None)
        name = ctx.text(ident) if ident is not None else None
        binding = {"kind": "macro", "name": name, "receiver": None, "receiverKind": None}
        suffix = next((c for c in node.named_children if c.type == "call_suffix"), None)
        end = suffix.start_byte if suffix is not None else node.end_byte
        raw = ctx.source[node.start_byte:end].decode("utf-8", "replace").rstrip()
        ctx.add_site("macros", _cap(raw, binding), node, conditional=self.cond > 0, binding=binding)

    def inheritance(self, node, bindings, outer):
        ctx = self.ctx
        bases = []
        for c in node.named_children:
            if c.type == "inheritance_specifier":
                inh = c.child_by_field_name("inherits_from")
                if inh is not None:
                    bases.append(ctx.text(inh))
                    parts = ctx.text(inh).rsplit(".", 1)
                    ctx.add_site(
                        "references", _cap(ctx.text(inh)), inh, caller=outer,
                        binding={"kind": "path" if len(parts) > 1 else "identifier", "name": parts[-1],
                                 "receiver": parts[0] if len(parts) > 1 else None,
                                 "receiverKind": "path" if len(parts) > 1 else None, "role": "inherits"},
                    )
        if bases:
            bindings["extends"] = bases

    # -- traversal -------------------------------------------------------------
    def enter(self, node):
        if not node.is_named:
            return True
        ctx = self.ctx
        t = node.type
        if t in CONDITIONAL:
            self.cond += 1
            self.cond_nodes.add(node.id)
        if t == "directive":
            text = ctx.text(node)
            if text.startswith("#if"):
                self.pp += 1
                self.cond += 1
            elif text.startswith("#endif") and self.pp > 0:
                self.pp -= 1
                self.cond -= 1
            return True
        if t == "import_declaration":
            self.import_site(node)
            return False
        if t == "class_declaration":
            dk = node.child_by_field_name("declaration_kind")
            dk_text = ctx.text(dk) if dk is not None else "class"
            name_node = node.child_by_field_name("name")
            name = ctx.text(name_node)
            kind = TYPE_KINDS.get(dk_text, "class")
            outer = ctx.owner
            bindings: dict = {"declarationKind": dk_text}
            if dk_text == "extension":
                bindings["implFor"] = name
                bindings["implTrait"] = None
            self.inheritance(node, bindings, outer)
            sym = ctx.add_symbol(kind, name, node, bindings=bindings)
            if dk_text != "extension":
                self.declare(name, "class" if kind in ("class", "struct") else "enum", *self.scope_range(), sym=sym, node=name_node, mutable=False)
            self.push(node, sym, name, kind)
            return True
        if t == "protocol_declaration":
            name_node = node.child_by_field_name("name")
            name = ctx.text(name_node)
            outer = ctx.owner
            bindings = {"declarationKind": "protocol"}
            self.inheritance(node, bindings, outer)
            sym = ctx.add_symbol("interface", name, node, bindings=bindings)
            self.declare(name, "type", *self.scope_range(), sym=sym, node=name_node)
            self.push(node, sym, name, "interface")
            return True
        if t in ("function_declaration", "protocol_function_declaration"):
            name_node = node.child_by_field_name("name")
            name = ctx.text(name_node)
            member = self.in_type_scope()
            kind = "method" if member else "function"
            mods = node.child_by_field_name("modifiers") or next((c for c in node.named_children if c.type == "modifiers"), None)
            modtext = ctx.text(mods) if mods is not None else ""
            bindings = {
                "static": member and any(w in modtext.split() for w in ("static", "class")),
                "body": node.child_by_field_name("body") is not None,
                "async": any(c.type == "async" for c in node.children),
                "throws": any(c.type in ("throws", "rethrows") for c in node.children),
            }
            sym = ctx.add_symbol(kind, name, node, bindings=bindings)
            if not member:
                rng = self.block_range(node) if self.in_func_scope() else self.scope_range()
                self.declare(name, "function", rng[0], rng[1], sym=sym, node=name_node, mutable=False)
            self.push(node, sym, name, kind)
            self.declare_params(node)
            return True
        if t == "init_declaration":
            sym = ctx.add_symbol("constructor", "init", node, bindings={"failable": any(c.type in ("?", "!") for c in node.children), "body": True})
            self.push(node, sym, "init", "constructor")
            self.declare_params(node)
            return True
        if t == "deinit_declaration":
            sym = ctx.add_symbol("method", "deinit", node, bindings={"destructor": True, "static": False})
            self.push(node, sym, "deinit", "method")
            return True
        if t == "subscript_declaration":
            sym = ctx.add_symbol("method", "subscript", node, bindings={"subscript": True, "static": False})
            self.push(node, sym, "subscript", "method")
            self.declare_params(node)
            return True
        if t in ("computed_getter", "computed_setter"):
            acc = "get" if t == "computed_getter" else "set"
            owner_name = ctx.current_scope.name if ctx.current_scope is not None else None
            sym = ctx.add_symbol("accessor", acc, node, bindings={"accessor": acc})
            self.push(node, sym, acc, "accessor")
            if t == "computed_setter":
                self.declare("newValue", "parameter", node.start_byte, node.end_byte, node=node, mutable=False)
            return True
        if t == "computed_property" and node.parent is not None and node.parent.type == "property_declaration":
            pat = node.parent.child_by_field_name("name")
            names = self.bound_names(pat) if pat is not None else []
            nm = ctx.text(names[0]) if names else ""
            sym = ctx.add_symbol("accessor", nm, node, bindings={"accessor": "get", "computedProperty": True})
            self.push(node, sym, nm, "accessor")
            return True
        if t in ("didset_clause", "willset_clause"):
            acc = "didSet" if t == "didset_clause" else "willSet"
            sym = ctx.add_symbol("accessor", acc, node, bindings={"accessor": acc})
            self.push(node, sym, acc, "accessor")
            self.declare("oldValue" if acc == "didSet" else "newValue", "parameter", node.start_byte, node.end_byte, node=node)
            return True
        if t == "lambda_literal":
            parent = node.parent
            outer_range = self.block_range(node) if parent is not None else self.scope_range()
            outer = ctx.owner
            sym = ctx.add_symbol("lambda", "<closure>", node, bindings={})
            self.push(node, sym, None, "lambda")
            for c in node.named_children:
                if c.type == "lambda_function_type":
                    for pp in c.named_children:
                        if pp.type == "lambda_function_type_parameters":
                            self.declare_params(node, params=pp.named_children)
            if parent is not None and parent.type == "property_declaration":
                pat = parent.child_by_field_name("name")
                names = self.bound_names(pat) if pat is not None else []
                if names:
                    name = ctx.text(names[0])
                    sym["bindings"]["assignedTo"] = name
                    self.declare(name, "variable", parent.end_byte, outer_range[1], sym=sym, node=names[0], mutable=self.is_var(parent), owner=outer)
            return True
        if t == "property_declaration":
            self.property_declaration(node)
            return True
        if t in ("if_statement", "while_statement", "guard_statement"):
            names = []
            for i, c in enumerate(node.children):
                if node.field_name_for_child(i) == "bound_identifier" and c.type == "simple_identifier":
                    names.append(c)
            for n in names:
                if t == "guard_statement":
                    rng = (node.end_byte, self.block_range(node)[1])
                else:
                    rng = (node.start_byte, node.end_byte)
                self.declare(ctx.text(n), "variable", rng[0], rng[1], node=n, mutable=False)
            return True
        if t == "constructor_expression":
            self.constructor_site(node)
            return True
        if t == "macro_invocation":
            self.macro_site(node)
            return True
        if t == "call_expression":
            self.call_site(node)
            return True
        return True

    @staticmethod
    def is_var(prop):
        for c in prop.children:
            if c.type == "value_binding_pattern":
                return "var" in c.text.decode("utf-8", "replace").split()
        return True

    def property_declaration(self, node):
        ctx = self.ctx
        if self.in_type_scope():
            return  # stored/computed members are not lexical names
        pat = node.child_by_field_name("name")
        if pat is None:
            return
        value = node.child_by_field_name("value")
        if value is not None and value.type == "lambda_literal":
            return  # declared with its closure symbol
        rng_end = self.block_range(node)[1]
        mutable = self.is_var(node)
        for n in self.bound_names(pat):
            self.declare(ctx.text(n), "variable", node.end_byte, rng_end, node=n, mutable=mutable)

    def leave(self, node):
        ctx = self.ctx
        if node.id in self.cond_nodes:
            self.cond_nodes.discard(node.id)
            self.cond -= 1
        if node.id in self.pushed:
            ctx.pop_scope()
            self.cond = self.pushed.pop(node.id)


def extract(language: str, source: bytes, ctx: "common.Context") -> dict:
    tree = ctx.parse()
    root = tree.root_node
    ctx.module_symbol(root)
    walker = _Walker(ctx)
    ctx.traverse(root, walker.enter, walker.leave)
    ctx.pop_scope()
    return ctx.result()
