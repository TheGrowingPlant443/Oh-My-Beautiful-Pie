"""Lua/Luau syntax recipe (pure Tree-sitter; never executes repository code).

Facts: function declarations/definitions (``function f``, ``function M.f``, ``function M:m``,
``local function``, ``name = function``, table-field functions, anonymous closures), literal
``require``/``dofile``/``loadfile`` imports (non-literal = dynamic), and every call site.
Block-scoped ``local`` bindings are declared with the narrowest block range; later assignments
to a visible name are recorded as ``assignment`` (rebinding) so the resolver never promotes
a name that may have been reassigned.  ``load``/``loadstring``, computed indexing and
chained call results are dynamic; no name-only promotion.
"""
from __future__ import annotations

import common

PROFILES = {
    "lua": {
        "extensions": [".lua", ".luau"],
        "grammar": "tree_sitter_lua",
        "capsule": "language",
        "node_kinds": [
            "chunk", "function_declaration", "function_definition", "function_call", "arguments", "parameters",
            "variable_declaration", "assignment_statement", "dot_index_expression", "method_index_expression",
            "bracket_index_expression", "for_generic_clause", "for_numeric_clause", "if_statement",
            "string", "string_content", "table_constructor", "field", "block",
        ],
    }
}

RAW_CAP = 4096
LOADERS = {"require": False, "dofile": True, "loadfile": True}
DYNAMIC_FUNCS = {"load", "loadstring", "getfenv", "setfenv", "loadfile_dynamic"}
CONDITIONAL = {
    "if_statement", "elseif_statement", "else_statement", "while_statement", "repeat_statement", "for_statement",
}


def _cap(text, binding=None):
    if len(text) <= RAW_CAP:
        return text
    if binding is not None:
        binding["rawTruncated"] = True
    return text[:RAW_CAP]


class _Walker:
    def __init__(self, ctx, module_id):
        self.ctx = ctx
        self.module_id = module_id
        self.pushed: dict[int, int] = {}
        self.cond = 0
        self.cond_nodes: set[int] = set()
        self.visible: dict[str, list[tuple]] = {}  # name -> [(start, end, owner)]
        self.require_bind: dict[int, dict] = {}
        self.globals_seen: set[str] = set()

    # -- helpers ----------------------------------------------------------
    @staticmethod
    def block_end(node):
        p = node.parent
        while p is not None and p.type not in ("block", "chunk"):
            p = p.parent
        return p.end_byte if p is not None else node.end_byte

    def lookup_local(self, name, pos):
        for s, e, owner in reversed(self.visible.get(name, ())):
            if s <= pos < e:
                return name, s, e, owner
        return None

    def declare(self, name, kind, start, end, *, sym=None, site=None, mutable=False, node=None, owner=...):
        ctx = self.ctx
        own = ctx.owner if owner is ... else owner
        ctx.declare(name, kind, start, end, symbol_id=sym["id"] if sym else None,
                    site_id=site["id"] if site else None, mutable=mutable,
                    position=node.start_byte if node is not None else None, owner=own)
        if kind in ("function", "variable", "parameter", "import", "catch"):
            self.visible.setdefault(name, []).append((start, end, own))

    def bind_name(self, name, name_node, stmt, *, is_local, sym=None, site=None):
        """Record the binding or rebinding of identifier ``name`` by statement ``stmt``."""
        ctx = self.ctx
        kind = "variable"
        if site is not None:
            kind = "import"
        elif sym is not None:
            kind = "function"
        if is_local:
            start = stmt.start_byte if (sym is not None and sym["kind"] == "function" and stmt.type == "function_declaration") else stmt.end_byte
            self.declare(name, kind, start, self.block_end(stmt), sym=sym, site=site, mutable=False, node=name_node)
            return
        found = self.lookup_local(name, stmt.end_byte)
        if found is not None:
            _, _, scope_end, _ = found
            ctx.declare(name, "assignment", stmt.end_byte, max(scope_end, stmt.end_byte), symbol_id=sym["id"] if sym else None,
                        site_id=site["id"] if site else None, mutable=True, position=name_node.start_byte)
            return
        # global binding: visible across the whole file, reassignable from anywhere
        first = name not in self.globals_seen
        self.globals_seen.add(name)
        gkind = "function" if (sym is not None and first) else "assignment"
        ctx.declare(name, gkind, 0, len(ctx.source), symbol_id=sym["id"] if sym else None,
                    site_id=site["id"] if site else None, mutable=True, position=name_node.start_byte, owner=self.module_id)

    def declare_params(self, node):
        params = node.child_by_field_name("parameters")
        if params is None:
            return
        for p in params.named_children:
            if p.type == "identifier":
                self.declare(self.ctx.text(p), "parameter", node.start_byte, node.end_byte, node=p, mutable=False)

    def string_value(self, node):
        if node is None or node.type != "string":
            return None
        parts = list(node.named_children)
        if any(c.type not in ("string_content", "escape_sequence") for c in parts):
            return None
        if not parts:
            return ""
        return self.ctx.source[parts[0].start_byte:parts[-1].end_byte].decode("utf-8", "replace")

    @staticmethod
    def arg_nodes(args):
        if args is None:
            return []
        if args.type == "arguments":
            return list(args.named_children)
        return [args]

    # -- sites -------------------------------------------------------------
    def import_site(self, node, loader, first_arg, *, conditional, bind=None):
        ctx = self.ctx
        lit = self.string_value(first_arg)
        dynamic = lit is None
        binding = {
            "kind": "require", "specifier": lit, "imported": None, "alias": bind["name"] if bind else None,
            "default": False, "namespace": False, "reexport": False, "typeOnly": False, "typeKind": None,
            "local": bind["name"] if bind else None, "relativeTo": None, "loader": loader,
            "pathLike": LOADERS[loader], "system": False,
        }
        if dynamic:
            binding["dynamic"] = True
        site = ctx.add_site("imports", _cap(ctx.text(node), binding), node, conditional=conditional, dynamic=dynamic, binding=binding)
        if bind is not None:
            self.bind_name(bind["name"], bind["node"], bind["stmt"], is_local=bind["local"], site=site)
        return site

    def call_site(self, node):
        ctx = self.ctx
        fn = node.child_by_field_name("name")
        args = node.child_by_field_name("arguments")
        argv = self.arg_nodes(args)
        cond = self.cond > 0
        if self.loader_call(node, fn, argv, cond):
            return
        end = args.start_byte if args is not None else node.end_byte
        raw = ctx.source[node.start_byte:end].decode("utf-8", "replace").rstrip()
        binding, dynamic = self.call_binding(fn)
        if dynamic:
            binding["dynamic"] = True
        ctx.add_site("calls", _cap(raw, binding), node, conditional=cond, dynamic=dynamic, binding=binding)

    def loader_call(self, node, fn, argv, conditional):
        if fn is None or fn.type != "identifier":
            return False
        fname = self.ctx.text(fn)
        if fname in LOADERS:
            self.import_site(node, fname, argv[0] if argv else None, conditional=conditional,
                             bind=self.require_bind.pop(node.id, None))
            return True
        if fname in ("pcall", "xpcall") and argv and argv[0].type == "identifier" and self.ctx.text(argv[0]) == "require":
            self.import_site(node, "require", argv[1] if len(argv) > 1 else None, conditional=True)
        return False

    def call_binding(self, fn):
        binding = {"name": None, "receiver": None, "receiverKind": None}
        t = fn.type if fn is not None else None
        if t == "identifier":
            binding.update(kind="identifier", name=self.ctx.text(fn))
            return binding, binding["name"] in DYNAMIC_FUNCS
        if t in ("dot_index_expression", "method_index_expression"):
            table = fn.child_by_field_name("table")
            field_name = "method" if t == "method_index_expression" else "field"
            field = fn.child_by_field_name(field_name)
            rtext = self.ctx.text(table)
            rk = "identifier" if table.type == "identifier" else "expression"
            if rtext == "self":
                rk = "self"
            binding.update(kind="member", name=self.ctx.text(field), receiver=_cap(rtext, binding), receiverKind=rk)
            if t == "method_index_expression":
                binding["method"] = True
            return binding, False
        binding.update(kind="dynamic", dynamic=True)
        return binding, True

    # -- declarations ---------------------------------------------------------
    def function_symbol(self, node):
        """Create the symbol for a function_declaration / function_definition and bind its name."""
        ctx = self.ctx
        bindings: dict = {}
        if node.type == "function_declaration":
            kind, name, qualified, bind = self.declared_function(node, bindings)
        else:
            kind, name, qualified, bind = self.defined_function(node, bindings)
        bindings["body"] = True
        if bind is not None and not bind[3] and node.type == "function_declaration":
            bindings["global"] = True
        sym = ctx.add_symbol(kind, name, node, bindings=bindings, qualified=qualified)
        if bind is not None:
            nm, nnode, stmt, is_local = bind
            # a function_declaration binds before its body (recursion); an assignment binds after it
            self.bind_name(nm, nnode, stmt, is_local=is_local, sym=sym)
        ctx.push_scope(sym["id"], name, kind)
        self.pushed[node.id] = self.cond
        self.cond = 0
        if node.type == "function_declaration":
            name_node = node.child_by_field_name("name")
            if name_node.type == "method_index_expression":
                self.declare("self", "parameter", node.start_byte, node.end_byte, node=name_node, mutable=False)
        self.declare_params(node)
        return sym

    def declared_function(self, node, bindings):
        name_node = node.child_by_field_name("name")
        is_local = any((not c.is_named) and c.type == "local" for c in node.children)
        nt = name_node.type
        text = self.ctx.text(name_node)
        if nt == "identifier":
            bindings["local"] = is_local
            return "function", text, None, (text, name_node, node, is_local)
        if nt == "dot_index_expression":
            bindings.update(assignedTo=text, receiver=self.ctx.text(name_node.child_by_field_name("table")), static=True)
            return "function", self.ctx.text(name_node.child_by_field_name("field")), text, None
        if nt == "method_index_expression":
            bindings.update(assignedTo=text, receiver=self.ctx.text(name_node.child_by_field_name("table")), static=False, method=True)
            return "method", self.ctx.text(name_node.child_by_field_name("method")), text, None
        bindings["assignedTo"] = text
        return "function", text, None, None

    def defined_function(self, node, bindings):
        parent = node.parent
        if parent is not None and parent.type == "expression_list" and parent.parent is not None and parent.parent.type == "assignment_statement":
            return self.assigned_function(node, parent, bindings)
        if parent is not None and parent.type == "field":
            name_node = parent.child_by_field_name("name")
            if name_node is not None and name_node.type == "identifier":
                name = self.ctx.text(name_node)
                bindings.update(assignedTo=name, tableField=True)
                return "function", name, None, None
        return "lambda", "<anonymous>", None, None

    def assigned_function(self, node, parent, bindings):
        stmt = parent.parent
        vars_ = stmt.named_children[0] if stmt.named_children else None
        vlist = [c for c in vars_.named_children if c.type != "attribute"] if vars_ is not None and vars_.type == "variable_list" else []
        idx = list(parent.named_children).index(node) if node in parent.named_children else -1
        if not 0 <= idx < len(vlist):
            return "lambda", "<anonymous>", None, None
        target = vlist[idx]
        full = self.ctx.text(target)
        if target.type == "identifier":
            outer = stmt.parent
            is_local = outer is not None and outer.type == "variable_declaration"
            bindings["assignedTo"] = full
            return "function", full, None, (full, target, outer if is_local else stmt, is_local)
        if target.type == "dot_index_expression":
            bindings.update(assignedTo=full, receiver=self.ctx.text(target.child_by_field_name("table")), static=True)
            return "function", self.ctx.text(target.child_by_field_name("field")), full, None
        return "lambda", "<anonymous>", None, None

    def assignment(self, node):
        """Declarations introduced by `local a, b = ...` / `a = ...` (functions/requires are bound at their value)."""
        parent = node.parent
        is_local = parent is not None and parent.type == "variable_declaration"
        stmt = parent if is_local else node
        vars_ = next((c for c in node.named_children if c.type == "variable_list"), None)
        values = next((c for c in node.named_children if c.type == "expression_list"), None)
        if vars_ is None:
            return
        vals = list(values.named_children) if values is not None else []
        targets = [c for c in vars_.named_children if c.type != "attribute"]
        for i, target in enumerate(targets):
            value = vals[i] if i < len(vals) else None
            self.assignment_target(target, value, stmt, is_local)

    def assignment_target(self, target, value, stmt, is_local):
        if target.type != "identifier":
            return
        name = self.ctx.text(target)
        if value is not None and value.type == "function_definition":
            return  # bound when the closure symbol is created
        if value is not None and value.type == "function_call":
            fn = value.child_by_field_name("name")
            if fn is not None and fn.type == "identifier" and self.ctx.text(fn) in LOADERS:
                self.require_bind[value.id] = {"name": name, "node": target, "stmt": stmt, "local": is_local}
                return
        self.bind_name(name, target, stmt, is_local=is_local)

    # -- traversal ------------------------------------------------------------
    def enter(self, node):
        if not node.is_named:
            return  # keyword/punctuation tokens share names with node types (e.g. `function`)
        t = node.type
        if t in CONDITIONAL:
            self.cond += 1
            self.cond_nodes.add(node.id)
        if t in ("function_declaration", "function_definition"):
            self.function_symbol(node)
        elif t == "assignment_statement":
            self.assignment(node)
        elif t in ("for_numeric_clause", "for_generic_clause"):
            self.for_variables(node)
        elif t == "function_call":
            self.call_site(node)

    def for_variables(self, node):
        body = node.parent.child_by_field_name("body") if node.parent is not None else None
        rng = (body.start_byte, body.end_byte) if body is not None else (node.end_byte, node.end_byte)
        if node.type == "for_numeric_clause":
            name = node.child_by_field_name("name")
            if name is not None:
                self.declare(self.ctx.text(name), "variable", rng[0], rng[1], node=name, mutable=False)
            return
        for vl in node.named_children:
            if vl.type != "variable_list":
                continue
            for name in vl.named_children:
                if name.type == "identifier":
                    self.declare(self.ctx.text(name), "variable", rng[0], rng[1], node=name, mutable=False)

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
    module = ctx.module_symbol(root)
    walker = _Walker(ctx, module["id"])
    ctx.traverse(root, walker.enter, walker.leave)
    ctx.pop_scope()
    return ctx.result()
