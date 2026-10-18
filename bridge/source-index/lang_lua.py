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
        kind = "import" if site is not None else ("function" if sym is not None else "variable")
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

    def declare_params(self, node, sym):
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
        if fn is not None and fn.type == "identifier":
            fname = ctx.text(fn)
            if fname in LOADERS:
                self.import_site(node, fname, argv[0] if argv else None, conditional=cond, bind=self.require_bind.pop(node.id, None))
                return
            if fname in ("pcall", "xpcall") and argv and argv[0].type == "identifier" and ctx.text(argv[0]) == "require":
                self.import_site(node, "require", argv[1] if len(argv) > 1 else None, conditional=True)
        end = args.start_byte if args is not None else node.end_byte
        raw = ctx.source[node.start_byte:end].decode("utf-8", "replace").rstrip()
        binding: dict = {"name": None, "receiver": None, "receiverKind": None}
        dynamic = False
        t = fn.type if fn is not None else None
        if t == "identifier":
            binding.update(kind="identifier", name=ctx.text(fn))
            dynamic = binding["name"] in DYNAMIC_FUNCS
        elif t == "dot_index_expression":
            table = fn.child_by_field_name("table")
            field = fn.child_by_field_name("field")
            rtext = ctx.text(table)
            rk = "self" if rtext == "self" else ("identifier" if table.type == "identifier" else "expression")
            binding.update(kind="member", name=ctx.text(field), receiver=_cap(rtext, binding), receiverKind=rk)
        elif t == "method_index_expression":
            table = fn.child_by_field_name("table")
            method = fn.child_by_field_name("method")
            rtext = ctx.text(table)
            rk = "self" if rtext == "self" else ("identifier" if table.type == "identifier" else "expression")
            binding.update(kind="member", name=ctx.text(method), receiver=_cap(rtext, binding), receiverKind=rk, method=True)
        else:
            binding.update(kind="dynamic", dynamic=True)
            dynamic = True
        if dynamic:
            binding["dynamic"] = True
        ctx.add_site("calls", _cap(raw, binding), node, conditional=cond, dynamic=dynamic, binding=binding)

    # -- declarations ---------------------------------------------------------
    def function_symbol(self, node):
        """Create the symbol for a function_declaration / function_definition and bind its name."""
        ctx = self.ctx
        t = node.type
        outer_owner = ctx.owner
        kind, name, qualified = "lambda", "<anonymous>", None
        bindings: dict = {}
        bind = None  # (name, name_node, stmt, is_local)
        if t == "function_declaration":
            name_node = node.child_by_field_name("name")
            is_local = any((not c.is_named) and c.type == "local" for c in node.children)
            nt = name_node.type
            text = ctx.text(name_node)
            if nt == "identifier":
                kind, name = "function", text
                bind = (text, name_node, node, is_local)
                bindings["local"] = is_local
            elif nt == "dot_index_expression":
                kind, name = "function", ctx.text(name_node.child_by_field_name("field"))
                qualified = text
                bindings.update(assignedTo=text, receiver=ctx.text(name_node.child_by_field_name("table")), static=True)
            elif nt == "method_index_expression":
                kind, name = "method", ctx.text(name_node.child_by_field_name("method"))
                qualified = text
                bindings.update(assignedTo=text, receiver=ctx.text(name_node.child_by_field_name("table")), static=False, method=True)
            else:
                kind, name = "function", text
                bindings["assignedTo"] = text
        else:  # function_definition: derive the name from the binding position
            parent = node.parent
            if parent is not None and parent.type == "expression_list" and parent.parent is not None and parent.parent.type == "assignment_statement":
                stmt = parent.parent
                vars_ = stmt.named_children[0] if stmt.named_children else None
                vlist = [c for c in vars_.named_children if c.type != "attribute"] if vars_ is not None and vars_.type == "variable_list" else []
                idx = [c for c in parent.named_children].index(node) if node in parent.named_children else -1
                if 0 <= idx < len(vlist):
                    target = vlist[idx]
                    full = ctx.text(target)
                    if target.type == "identifier":
                        kind, name = "function", full
                        outer = stmt.parent
                        is_local = outer is not None and outer.type == "variable_declaration"
                        bind = (full, target, outer if is_local else stmt, is_local)
                        bindings["assignedTo"] = full
                    elif target.type == "dot_index_expression":
                        kind, name = "function", ctx.text(target.child_by_field_name("field"))
                        qualified = full
                        bindings.update(assignedTo=full, receiver=ctx.text(target.child_by_field_name("table")), static=True)
            elif parent is not None and parent.type == "field":
                n = parent.child_by_field_name("name")
                if n is not None and n.type == "identifier":
                    kind, name = "function", ctx.text(n)
                    bindings.update(assignedTo=name, tableField=True)
        bindings["body"] = True
        if bind is not None and not bind[3] and t == "function_declaration":
            bindings["global"] = True
        sym = ctx.add_symbol(kind, name, node, bindings=bindings, qualified=qualified)
        if bind is not None:
            nm, nnode, stmt, is_local = bind
            # a function_declaration binds before its body (recursion); an assignment binds after it
            self.bind_name(nm, nnode, stmt, is_local=is_local, sym=sym)
        ctx.push_scope(sym["id"], name, kind)
        self.pushed[node.id] = self.cond
        self.cond = 0
        if t == "function_declaration":
            name_node = node.child_by_field_name("name")
            if name_node.type == "method_index_expression":
                self.declare("self", "parameter", node.start_byte, node.end_byte, node=name_node, mutable=False)
        self.declare_params(node, sym)
        return sym

    def assignment(self, node):
        """Declarations introduced by `local a, b = ...` / `a = ...` (functions/requires are bound at their value)."""
        ctx = self.ctx
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
            if target.type != "identifier":
                continue
            name = ctx.text(target)
            if value is not None and value.type == "function_definition":
                continue  # bound when the closure symbol is created
            if value is not None and value.type == "function_call":
                fn = value.child_by_field_name("name")
                if fn is not None and fn.type == "identifier" and ctx.text(fn) in LOADERS:
                    self.require_bind[value.id] = {"name": name, "node": target, "stmt": stmt, "local": is_local}
                    continue
            self.bind_name(name, target, stmt, is_local=is_local)

    # -- traversal ------------------------------------------------------------
    def enter(self, node):
        if not node.is_named:
            return True  # keyword/punctuation tokens share names with node types (e.g. `function`)
        ctx = self.ctx
        t = node.type
        if t in CONDITIONAL:
            self.cond += 1
            self.cond_nodes.add(node.id)
        if t in ("function_declaration", "function_definition"):
            self.function_symbol(node)
            return True
        if t == "assignment_statement":
            self.assignment(node)
            return True
        if t == "for_numeric_clause":
            n = node.child_by_field_name("name")
            body = node.parent.child_by_field_name("body") if node.parent is not None else None
            rng = (body.start_byte, body.end_byte) if body is not None else (node.end_byte, node.end_byte)
            if n is not None:
                self.declare(ctx.text(n), "variable", rng[0], rng[1], node=n, mutable=False)
            return True
        if t == "for_generic_clause":
            body = node.parent.child_by_field_name("body") if node.parent is not None else None
            rng = (body.start_byte, body.end_byte) if body is not None else (node.end_byte, node.end_byte)
            for vl in node.named_children:
                if vl.type == "variable_list":
                    for n in vl.named_children:
                        if n.type == "identifier":
                            self.declare(ctx.text(n), "variable", rng[0], rng[1], node=n, mutable=False)
            return True
        if t == "function_call":
            self.call_site(node)
            return True
        return True

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
    module = ctx.module_symbol(root)
    walker = _Walker(ctx, module["id"])
    ctx.traverse(root, walker.enter, walker.leave)
    ctx.pop_scope()
    return ctx.result()
