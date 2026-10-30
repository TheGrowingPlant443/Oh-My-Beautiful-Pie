"""Ruby syntax recipe (pure Tree-sitter; never executes repository code).

Facts: module/class/method/singleton_method/lambda/block declarations, literal
``require``/``require_relative``/``load``/``autoload`` imports, mixin and superclass
references, and every call/``super``/``yield`` site.  Non-literal requires and
reflective calls (``send``, ``define_method``, ``eval`` ...) are persisted as dynamic sites.
Methods are declared with ``mutable=True`` because Ruby classes can be reopened and
overridden: the resolver must not promote bare names to a unique target on name alone.
"""
from __future__ import annotations

import common

PROFILES = {
    "ruby": {
        "extensions": [".rb", ".rake"],
        "grammar": "tree_sitter_ruby",
        "capsule": "language",
        "node_kinds": [
            "program", "class", "module", "method", "singleton_method", "singleton_class",
            "call", "lambda", "block", "do_block", "string", "string_content", "argument_list",
            "method_parameters", "block_parameters", "scope_resolution", "superclass", "yield",
        ],
    }
}

RAW_CAP = 4096
REQUIRE_LOADERS = {"require", "require_relative", "load", "require_dependency", "autoload"}
MIXINS = {"include", "extend", "prepend"}
DYNAMIC_CALLS = {
    "send", "__send__", "public_send", "instance_variable_get", "instance_variable_set",
    "define_method", "define_singleton_method", "eval", "instance_eval", "class_eval",
    "module_eval", "instance_exec", "class_exec", "const_get", "method", "constantize", "safe_constantize",
}
CONDITIONAL = {
    "if", "unless", "if_modifier", "unless_modifier", "conditional", "case", "case_match", "when",
    "in_clause", "while", "until", "while_modifier", "until_modifier", "rescue", "rescue_modifier", "for",
}
PARAM_NAMED = {
    "optional_parameter", "keyword_parameter", "splat_parameter", "hash_splat_parameter", "block_parameter",
}


def _cap(text: str, binding: dict | None = None) -> str:
    if len(text) <= RAW_CAP:
        return text
    if binding is not None:
        binding["rawTruncated"] = True
    return text[:RAW_CAP]


def _literal(ctx, node):
    """Raw contents of a non-interpolated string literal, else None."""
    if node is None or node.type != "string":
        return None
    parts = list(node.named_children)
    if any(c.type not in ("string_content", "escape_sequence") for c in parts):
        return None
    if not parts:
        return ""
    return ctx.source[parts[0].start_byte:parts[-1].end_byte].decode("utf-8", "replace")


def _receiver_kind(node):
    if node is None:
        return None
    t = node.type
    if t == "identifier":
        return "identifier"
    if t == "self":
        return "self"
    if t == "super":
        return "super"
    if t in ("constant", "scope_resolution"):
        return "path"
    return "expression"


def _split_path(text):
    """``A::B::C`` -> (``A::B``, ``C``); no ``::`` -> (None, text)."""
    if "::" in text:
        return tuple(text.rsplit("::", 1))
    return None, text


def _path_binding(text, role):
    prefix, last = _split_path(text)
    return {"kind": "path" if prefix else "identifier", "name": last, "receiver": prefix,
            "receiverKind": "path" if prefix else None, "role": role}


def _fqn(parent_name, name):
    return ((parent_name + "::" if parent_name else "") + name).replace(".", "::")


def _call_raw_end(node, args, block):
    if args is not None:
        return args.start_byte
    if block is not None:
        return block.start_byte
    return node.end_byte


def _require_target(loader, items):
    if loader == "autoload" and len(items) > 1:
        return items[1]
    return items[0] if items else None


class _Walker:
    def __init__(self, ctx):
        self.ctx = ctx
        self.pushed: dict[int, int] = {}
        self.cond = 0
        self.cond_nodes: set[int] = set()
        self.names: dict = {}  # scope symbol id -> names already bound (rebinding detection)

    # -- scope helpers -----------------------------------------------------
    def scope_range(self):
        ctx = self.ctx
        sid = ctx.owner
        sym = ctx.symbol_by_id(sid) if sid else None
        if sym is None or sym["kind"] == "module":
            return 0, len(ctx.source)
        sp = sym["span"]
        return sp["byteStart"], sp["byteEnd"]

    def declare_here(self, name, kind, sym=None, node=None, mutable=False):
        if not name:
            return
        start, end = self.scope_range()
        pos = node.start_byte if node is not None else None
        if kind == "parameter":
            self.names.setdefault(self.ctx.owner, set()).add(name)
        self.ctx.declare(name, kind, start, end, symbol_id=sym["id"] if sym else None, mutable=mutable, position=pos)

    def push(self, node, kind, name, **kw):
        ctx = self.ctx
        sym = ctx.add_symbol(kind, name, node, push=True, **kw)
        self.pushed[node.id] = self.cond
        self.cond = 0
        return sym

    def in_class_scope(self):
        sc = self.ctx.current_scope
        return sc is not None and sc.kind in ("class", "namespace")

    # -- params -------------------------------------------------------------
    def param_name(self, p):
        if p.type == "identifier":
            return self.ctx.text(p)
        if p.type in PARAM_NAMED:
            n = p.child_by_field_name("name")
            return self.ctx.text(n) if n is not None else None
        return None

    def declare_params(self, params, kind="parameter"):
        if params is None:
            return
        for p in params.named_children:
            if p.type == "destructured_parameter":
                for q in p.named_children:
                    if q.type == "identifier":
                        self.declare_here(self.ctx.text(q), kind, node=q, mutable=True)
            else:
                self.declare_here(self.param_name(p), kind, node=p, mutable=True)

    # -- sites ---------------------------------------------------------------
    def call_site(self, node):
        ctx = self.ctx
        method = node.child_by_field_name("method")
        recv = node.child_by_field_name("receiver")
        args = node.child_by_field_name("arguments")
        block = node.child_by_field_name("block")
        name = ctx.text(method) if method is not None else None
        if recv is None and args is not None:
            if name in REQUIRE_LOADERS:
                self.require_site(node, name, args)
                return
            if name in MIXINS:
                self.mixin_sites(args, name)
                return
        end = _call_raw_end(node, args, block)
        raw = ctx.source[node.start_byte:end].decode("utf-8", "replace").rstrip()
        binding = self.call_binding(recv, name)
        dynamic = name in DYNAMIC_CALLS or method is None
        if dynamic:
            binding["dynamic"] = True
            if method is None:
                binding["name"] = None  # `callable.(args)`
        ctx.add_site("calls", _cap(raw, binding), node, conditional=self.cond > 0, dynamic=dynamic, binding=binding)

    def mixin_sites(self, args, mixin):
        ctx = self.ctx
        for a in args.named_children:
            if a.type in ("constant", "scope_resolution"):
                text = ctx.text(a)
                binding = _path_binding(text, "mixin")
                binding["mixin"] = mixin
                ctx.add_site("references", _cap(text), a, conditional=self.cond > 0, binding=binding)

    def call_binding(self, recv, name):
        binding: dict = {"name": name, "receiver": None, "receiverKind": None}
        if recv is None:
            binding["kind"] = "identifier"
            return binding
        rtext = self.ctx.text(recv)
        binding["receiver"] = _cap(rtext, binding)
        binding["receiverKind"] = _receiver_kind(recv)
        if name == "new" and binding["receiverKind"] in ("path", "identifier"):
            binding["kind"] = "new"
            binding["method"] = "new"
            binding["receiver"], binding["name"] = _split_path(rtext)
            binding["receiverKind"] = "path" if binding["receiver"] else None
        else:
            binding["kind"] = "member"
        return binding

    def require_site(self, node, loader, args):
        ctx = self.ctx
        items = args.named_children
        # autoload :Const, "path" -> path is the second argument
        target = _require_target(loader, items)
        raw = ctx.source[node.start_byte:args.end_byte].decode("utf-8", "replace")
        literal = _literal(ctx, target)
        binding = {
            "kind": "require", "specifier": literal, "imported": None, "alias": None, "default": False,
            "namespace": False, "reexport": False, "typeOnly": False, "typeKind": None, "local": None,
            "relativeTo": "dir" if loader == "require_relative" else None, "loader": loader, "system": False,
        }
        dynamic = literal is None
        if dynamic:
            binding["dynamic"] = True
        ctx.add_site("imports", _cap(raw, binding), node, conditional=self.cond > 0, dynamic=dynamic, binding=binding)

    # -- traversal -------------------------------------------------------------
    def enter(self, node):
        if not node.is_named:
            return  # keyword tokens share names with node types (`class`, `module`, `yield`)
        t = node.type
        if t in CONDITIONAL:
            self.cond += 1
            self.cond_nodes.add(node.id)
        if t == "class":
            self.enter_class(node)
        elif t == "module":
            self.enter_module(node)
        elif t == "singleton_class":
            self.push(node, "namespace", "<<self", node_kind="singleton_class", bindings={"singleton": True})
        elif t in ("method", "singleton_method"):
            self.enter_method(node)
        elif t in ("lambda", "block", "do_block"):
            self.enter_closure(node)
        elif t in ("assignment", "operator_assignment"):
            self.enter_assignment(node)
        elif t == "call":
            self.call_site(node)
        elif t == "super":
            self.super_site(node)
        elif t == "yield":
            self.yield_site(node)

    def superclass_text(self, sup):
        if sup is None or not sup.named_children:
            return None
        return self.ctx.text(sup.named_children[0])

    def enter_class(self, node):
        ctx = self.ctx
        name_node = node.child_by_field_name("name")
        name = ctx.text(name_node)
        sup = node.child_by_field_name("superclass")
        sup_text = self.superclass_text(sup)
        bindings = {"extends": [sup_text]} if sup_text else {}
        outer = ctx.owner
        parent_name = ctx.qualified(None)
        sym = self.push_declared(node, "class", name, bindings, name_node)
        sym.setdefault("bindings", {})["fqn"] = _fqn(parent_name, name)
        if sup_text:
            ctx.add_site("references", _cap(sup_text), sup, caller=outer, binding=_path_binding(sup_text, "extends"))

    def enter_module(self, node):
        ctx = self.ctx
        name_node = node.child_by_field_name("name")
        name = ctx.text(name_node)
        parent_name = ctx.qualified(None)
        sym = self.push_declared(node, "namespace", name, {}, name_node)
        sym["bindings"]["fqn"] = _fqn(parent_name, name)

    def enter_method(self, node):
        ctx = self.ctx
        t = node.type
        name_node = node.child_by_field_name("name")
        name = ctx.text(name_node)
        static = t == "singleton_method" or self.singleton_scope()
        kind = "method" if (self.in_class_scope() or t == "singleton_method") else "function"
        if name == "initialize" and kind == "method":
            kind = "constructor"
        bindings = {"static": bool(static)}
        if t == "singleton_method":
            obj = node.child_by_field_name("object")
            bindings["receiver"] = ctx.text(obj) if obj is not None else None
        sym = ctx.add_symbol(kind, name, node, bindings=bindings, push=False, node_kind=t)
        self.declare_here(name, "function", sym, name_node, mutable=True)
        ctx.push_scope(sym["id"], name, kind)
        self.pushed[node.id] = self.cond
        self.cond = 0
        self.declare_params(node.child_by_field_name("parameters"))

    def block_bindings(self, node):
        parent = node.parent
        if node.type == "lambda" or parent is None or parent.type != "call":
            return {}
        m = parent.child_by_field_name("method")
        return {"blockOf": self.ctx.text(m)} if m is not None else {}

    def enter_closure(self, node):
        t = node.type
        parent = node.parent
        if t != "lambda" and parent is not None and parent.type == "lambda":
            return  # the lambda symbol already represents this body
        label = "<lambda>" if t == "lambda" else "<block>"
        outer = self.ctx.owner
        outer_range = self.scope_range()
        sym = self.push(node, "lambda", label, node_kind=t, bindings=self.block_bindings(node))
        self.declare_params(node.child_by_field_name("parameters"))
        # `name = ->(x) { }`: the variable holds this closure
        if t == "lambda" and parent is not None and parent.type == "assignment":
            self.bind_lambda(sym, parent, outer, outer_range)

    def bind_lambda(self, sym, assignment, outer, outer_range):
        left = assignment.child_by_field_name("left")
        if left is not None and left.type == "identifier":
            name = self.ctx.text(left)
            sym["bindings"]["assignedTo"] = name
            self.declare_variable(name, left, sym=sym, owner=outer, scope=outer_range)

    def enter_assignment(self, node):
        left = node.child_by_field_name("left")
        right = node.child_by_field_name("right")
        if left is None:
            return
        if left.type == "identifier":
            lambda_assignment = node.type == "assignment" and right is not None and right.type == "lambda"
            if not lambda_assignment:
                self.declare_variable(self.ctx.text(left), left)
        elif left.type in ("constant", "scope_resolution"):
            self.declare_here(self.ctx.text(left), "variable", node=left, mutable=False)

    def super_site(self, node):
        self.ctx.add_site(
            "calls", "super", node, conditional=self.cond > 0,
            binding={"kind": "identifier", "name": self.enclosing_method_name(), "receiver": "super",
                     "receiverKind": "super", "super": True},
        )

    def yield_site(self, node):
        self.ctx.add_site(
            "calls", "yield", node, conditional=self.cond > 0, dynamic=True,
            binding={"kind": "dynamic", "name": "yield", "receiver": None, "receiverKind": None, "dynamic": True},
        )

    def declare_variable(self, name, node, sym=None, owner=..., scope=None):
        """First binding in a scope is a ``variable``; later ones are ``assignment`` (rebinding)."""
        ctx = self.ctx
        own = ctx.owner if owner is ... else owner
        start, end = scope if scope is not None else self.scope_range()
        seen = self.names.setdefault(own, set())
        kind = "assignment" if name in seen else "variable"
        seen.add(name)
        ctx.declare(name, kind, start, end, symbol_id=sym["id"] if sym else None, mutable=True,
                    position=node.start_byte, owner=own)

    def singleton_scope(self):
        sc = self.ctx.current_scope
        return sc is not None and sc.name == "<<self"

    def enclosing_method_name(self):
        for sc in reversed(self.ctx.scopes):
            if sc.kind in ("method", "function", "constructor"):
                return sc.name
        return None

    def push_declared(self, node, kind, name, bindings, name_node):
        ctx = self.ctx
        sym = ctx.add_symbol(kind, name, node, bindings=dict(bindings), push=False)
        self.declare_here(name.rsplit("::", 1)[-1], kind if kind != "namespace" else "namespace", sym, name_node, mutable=True)
        ctx.push_scope(sym["id"], name, kind)
        self.pushed[node.id] = self.cond
        self.cond = 0
        return sym

    def leave(self, node):
        if node.id in self.cond_nodes:
            self.cond_nodes.discard(node.id)
            self.cond -= 1
        if node.id in self.pushed:
            self.ctx.pop_scope()
            self.cond = self.pushed.pop(node.id)


def extract(_language: str, _source: bytes, ctx: "common.Context") -> dict:
    tree = ctx.parse()
    root = tree.root_node
    ctx.module_symbol(root)
    walker = _Walker(ctx)
    ctx.traverse(root, walker.enter, walker.leave)
    ctx.pop_scope()
    # Dynamic dispatch, reflection and metaprogramming are never resolved (see DYNAMIC_CALLS).
    return ctx.result()
