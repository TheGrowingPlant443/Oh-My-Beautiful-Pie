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
    parts = [c for c in node.named_children]
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
    def declare_params(self, params, kind="parameter"):
        if params is None:
            return
        for p in params.named_children:
            name = None
            if p.type == "identifier":
                name = self.ctx.text(p)
            elif p.type in PARAM_NAMED:
                n = p.child_by_field_name("name")
                name = self.ctx.text(n) if n is not None else None
            elif p.type == "destructured_parameter":
                for q in p.named_children:
                    if q.type == "identifier":
                        self.declare_here(self.ctx.text(q), kind, node=q, mutable=True)
                continue
            self.declare_here(name, kind, node=p, mutable=True)

    # -- sites ---------------------------------------------------------------
    def call_site(self, node):
        ctx = self.ctx
        method = node.child_by_field_name("method")
        recv = node.child_by_field_name("receiver")
        args = node.child_by_field_name("arguments")
        block = node.child_by_field_name("block")
        name = ctx.text(method) if method is not None else None
        if recv is None and name in REQUIRE_LOADERS and args is not None:
            self.require_site(node, name, args)
            return
        if recv is None and name in MIXINS and args is not None:
            for a in args.named_children:
                if a.type in ("constant", "scope_resolution"):
                    text = ctx.text(a)
                    last = text.rsplit("::", 1)[-1]
                    recv_text = text.rsplit("::", 1)[0] if "::" in text else None
                    ctx.add_site(
                        "references", _cap(text), a, conditional=self.cond > 0,
                        binding={"kind": "path" if recv_text else "identifier", "name": last, "receiver": recv_text,
                                 "receiverKind": "path" if recv_text else None, "role": "mixin", "mixin": name},
                    )
            return
        end = args.start_byte if args is not None else (block.start_byte if block is not None else node.end_byte)
        raw = ctx.source[node.start_byte:end].decode("utf-8", "replace").rstrip()
        binding: dict = {"name": name, "receiver": None, "receiverKind": None}
        dynamic = name in DYNAMIC_CALLS or method is None
        if recv is not None:
            rtext = ctx.text(recv)
            binding["receiver"] = _cap(rtext, binding)
            binding["receiverKind"] = _receiver_kind(recv)
            if name == "new" and binding["receiverKind"] in ("path", "identifier"):
                binding["kind"] = "new"
                binding["method"] = "new"
                if "::" in rtext:
                    binding["receiver"], binding["name"] = rtext.rsplit("::", 1)
                else:
                    binding["receiver"], binding["name"] = None, rtext
                binding["receiverKind"] = "path" if binding["receiver"] else None
            else:
                binding["kind"] = "member"
        else:
            binding["kind"] = "identifier"
        if dynamic:
            binding["dynamic"] = True
            if method is None:
                binding["name"] = None  # `callable.(args)`
        ctx.add_site("calls", _cap(raw, binding), node, conditional=self.cond > 0, dynamic=dynamic, binding=binding)

    def require_site(self, node, loader, args):
        ctx = self.ctx
        items = args.named_children
        # autoload :Const, "path" -> path is the second argument
        target = items[1] if loader == "autoload" and len(items) > 1 else (items[0] if items else None)
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
            return True  # keyword tokens share names with node types (`class`, `module`, `yield`)
        ctx = self.ctx
        t = node.type
        if t in CONDITIONAL:
            self.cond += 1
            self.cond_nodes.add(node.id)
        if t == "class":
            name_node = node.child_by_field_name("name")
            name = ctx.text(name_node)
            sup = node.child_by_field_name("superclass")
            bindings = {"extends": []}
            sup_text = None
            if sup is not None:
                inner = sup.named_children[0] if sup.named_children else None
                sup_text = ctx.text(inner) if inner is not None else None
                if sup_text:
                    bindings["extends"] = [sup_text]
            if not bindings["extends"]:
                del bindings["extends"]
            outer = ctx.owner
            parent_name = ctx.qualified(None)
            sym = self.push_declared(node, "class", name, bindings, name_node)
            bindings_fqn = (parent_name + "::" if parent_name else "") + name
            sym.setdefault("bindings", {})["fqn"] = bindings_fqn.replace(".", "::")
            if sup is not None and sup_text:
                last = sup_text.rsplit("::", 1)[-1]
                prefix = sup_text.rsplit("::", 1)[0] if "::" in sup_text else None
                ctx.add_site(
                    "references", _cap(sup_text), sup, caller=outer,
                    binding={"kind": "path" if prefix else "identifier", "name": last, "receiver": prefix,
                             "receiverKind": "path" if prefix else None, "role": "extends"},
                )
            return True
        if t == "module":
            name_node = node.child_by_field_name("name")
            name = ctx.text(name_node)
            parent_name = ctx.qualified(None)
            sym = self.push_declared(node, "namespace", name, {}, name_node)
            sym["bindings"]["fqn"] = ((parent_name + "::" if parent_name else "") + name).replace(".", "::")
            return True
        if t == "singleton_class":
            self.push(node, "namespace", "<<self", node_kind="singleton_class", bindings={"singleton": True})
            return True
        if t in ("method", "singleton_method"):
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
            return True
        if t in ("block", "do_block") and node.parent is not None and node.parent.type == "lambda":
            return True  # the lambda symbol already represents this body
        if t in ("lambda", "block", "do_block"):
            parent = node.parent
            label = "<lambda>" if t == "lambda" else "<block>"
            bindings: dict = {}
            if t != "lambda" and parent is not None and parent.type == "call":
                m = parent.child_by_field_name("method")
                if m is not None:
                    bindings["blockOf"] = ctx.text(m)
            outer = ctx.owner
            outer_range = self.scope_range()
            sym = self.push(node, "lambda", label, node_kind=t, bindings=bindings)
            self.declare_params(node.child_by_field_name("parameters"))
            # `name = ->(x) { }`: the variable holds this closure
            if t == "lambda" and parent is not None and parent.type == "assignment":
                left = parent.child_by_field_name("left")
                if left is not None and left.type == "identifier":
                    name = ctx.text(left)
                    sym["bindings"]["assignedTo"] = name
                    self.declare_variable(name, left, sym=sym, owner=outer, scope=outer_range)
            return True
        if t == "assignment" or t == "operator_assignment":
            left = node.child_by_field_name("left")
            right = node.child_by_field_name("right")
            if left is not None and left.type == "identifier":
                if not (t == "assignment" and right is not None and right.type == "lambda"):
                    self.declare_variable(ctx.text(left), left)
            elif left is not None and left.type in ("constant", "scope_resolution"):
                self.declare_here(ctx.text(left), "variable", node=left, mutable=False)
            return True
        if t == "call":
            self.call_site(node)
            return True
        if t == "super":
            sc = self.enclosing_method_name()
            ctx.add_site(
                "calls", "super", node, conditional=self.cond > 0,
                binding={"kind": "identifier", "name": sc, "receiver": "super", "receiverKind": "super", "super": True},
            )
            return True
        if t == "yield":
            ctx.add_site(
                "calls", "yield", node, conditional=self.cond > 0, dynamic=True,
                binding={"kind": "dynamic", "name": "yield", "receiver": None, "receiverKind": None, "dynamic": True},
            )
            return True
        return True

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


def extract(language: str, source: bytes, ctx: "common.Context") -> dict:
    tree = ctx.parse()
    root = tree.root_node
    ctx.module_symbol(root)
    walker = _Walker(ctx)
    ctx.traverse(root, walker.enter, walker.leave)
    ctx.pop_scope()
    # Dynamic dispatch, reflection and metaprogramming are never resolved (see DYNAMIC_CALLS).
    return ctx.result()
