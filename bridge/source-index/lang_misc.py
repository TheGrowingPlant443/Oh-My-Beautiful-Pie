"""Zig, Elixir, Julia, Fortran and Verilog syntax recipes (pure Tree-sitter; no repository code runs).

Shared rules: declarations become symbols; every recognized call/import/include/instantiation
is an unresolved site (never promoted by name).  Notes per language:

* Zig: literal ``@import``; builtins (``@as`` ...) are ``external`` call sites; no closures exist.
* Elixir: ``def*`` heads are declarations (never call sites), ``alias``/``import``/``require``/
  ``use`` are import facts, local calls carry arity; ``a.b`` without parentheses on a variable is an
  ambiguous ``references`` site (map access or zero-arity call).  Typespec attributes are skipped.
* Julia: methods are open-ended (multiple dispatch) so function declarations are mutable;
  ``include("lit")`` is an include fact; macros are ``macros`` sites.
* Fortran: preprocessing is NOT expanded (preprocessor regions are conditional); names are
  case-insensitive (``caseInsensitive`` binding flag, lower-cased ``fqn``); ``f(x)`` is ambiguous
  between call and array element and is recorded as such (declared arrays become ``references``).
* Verilog: ``module_instantiation`` is ``instantiates`` (NOT a function call); the grammar parses
  some procedural calls and positional instantiations as checker instantiations/ERROR regions:
  recognized ones are recorded with ``ambiguousSyntax`` and the file is partial.
"""
from __future__ import annotations

import common

PROFILES = {
    "zig": {
        "extensions": [".zig"],
        "grammar": "tree_sitter_zig",
        "capsule": "language",
        "node_kinds": [
            "source_file", "function_declaration", "variable_declaration", "call_expression", "builtin_function",
            "builtin_identifier", "field_expression", "struct_declaration", "enum_declaration", "union_declaration",
            "test_declaration", "parameter", "string_content", "if_statement",
        ],
    },
    "elixir": {
        "extensions": [".ex", ".exs"],
        "grammar": "tree_sitter_elixir",
        "capsule": "language",
        "node_kinds": [
            "source", "call", "arguments", "do_block", "dot", "alias", "identifier", "anonymous_function",
            "stab_clause", "binary_operator", "unary_operator", "keywords", "pair",
        ],
    },
    "julia": {
        "extensions": [".jl"],
        "grammar": "tree_sitter_julia",
        "capsule": "language",
        "node_kinds": [
            "source_file", "module_definition", "function_definition", "macro_definition", "struct_definition",
            "abstract_definition", "signature", "call_expression", "broadcast_call_expression", "field_expression",
            "macrocall_expression", "macro_identifier", "using_statement", "import_statement", "selected_import",
            "import_path", "import_alias", "arrow_function_expression", "assignment", "string_literal", "do_clause",
        ],
    },
    "fortran": {
        "extensions": [".f", ".F", ".f90", ".F90", ".f95", ".F95", ".f03", ".F03", ".f08", ".F08"],
        "grammar": "tree_sitter_fortran",
        "capsule": "language",
        "node_kinds": [
            "translation_unit", "module", "module_statement", "program", "program_statement", "subroutine",
            "subroutine_statement", "function", "function_statement", "use_statement", "subroutine_call",
            "call_expression", "variable_declaration", "include_statement", "derived_type_definition",
            "interface", "internal_procedures", "preproc_include",
        ],
    },
    "verilog": {
        "extensions": [".v", ".sv", ".svh", ".vh"],
        "grammar": "tree_sitter_verilog",
        "capsule": "language",
        "node_kinds": [
            "source_file", "module_declaration", "module_header", "module_instantiation", "hierarchical_instance",
            "name_of_instance", "function_declaration", "task_declaration", "tf_call", "system_tf_call",
            "package_declaration", "package_import_declaration", "package_import_item", "class_declaration",
            "interface_declaration", "include_compiler_directive", "text_macro_definition", "checker_instantiation",
        ],
    },
}

RAW_CAP = 4096


def _cap(text, binding=None):
    if len(text) <= RAW_CAP:
        return text
    if binding is not None:
        binding["rawTruncated"] = True
    return text[:RAW_CAP]


def _import_binding(kind, specifier, **extra):
    b = {
        "kind": kind, "specifier": specifier, "imported": None, "alias": None, "default": False,
        "namespace": False, "reexport": False, "typeOnly": False, "typeKind": None, "local": None,
        "relativeTo": None, "system": False,
    }
    b.update(extra)
    if specifier is None:
        b["dynamic"] = True
    return b


class _Base:
    def __init__(self, ctx):
        self.ctx = ctx
        self.pushed: dict[int, int] = {}
        self.cond = 0
        self.pp = 0
        self.cond_nodes: set[int] = set()
        self.pp_nodes: set[int] = set()
        self.skip_nodes: set[int] = set()
        self.module_id = None
        self.names: dict = {}

    def scope_range(self):
        ctx = self.ctx
        sym = ctx.symbol_by_id(ctx.owner) if ctx.owner else None
        if sym is None or sym["kind"] == "module":
            return 0, len(ctx.source)
        sp = sym["span"]
        return sp["byteStart"], sp["byteEnd"]

    def push(self, node, sym, name, kind):
        self.ctx.push_scope(sym["id"], name, kind)
        self.pushed[node.id] = self.cond
        # a nested scope is not conditional on enclosing statements, but is on preprocessor regions
        self.cond = self.pp

    def mark_cond(self, node):
        self.cond += 1
        self.cond_nodes.add(node.id)

    def mark_pp(self, node=None):
        """Open a preprocessor region (conditional compilation); node=None for flat directive pairs."""
        self.pp += 1
        self.cond += 1
        if node is not None:
            self.pp_nodes.add(node.id)

    def close_pp(self):
        if self.pp > 0:
            self.pp -= 1
            self.cond -= 1

    def declare(self, name, kind, start, end, *, sym=None, site=None, node=None, mutable=False, owner=...):
        if not name or name == "_":
            return
        ctx = self.ctx
        own = ctx.owner if owner is ... else owner
        key = name
        if kind in ("variable", "assignment", "parameter"):
            seen = self.names.setdefault(own, set())
            if kind in ("variable", "assignment"):
                kind = "assignment" if key in seen else "variable"
            seen.add(key)
        ctx.declare(name, kind, start, end, symbol_id=sym["id"] if sym else None, site_id=site["id"] if site else None,
                    mutable=mutable, position=node.start_byte if node is not None else None, owner=own)

    def leave(self, node):
        if node.id in self.cond_nodes:
            self.cond_nodes.discard(node.id)
            self.cond -= 1
        if node.id in self.pp_nodes:
            self.pp_nodes.discard(node.id)
            self.pp -= 1
            self.cond -= 1
        if node.id in self.pushed:
            self.ctx.pop_scope()
            self.cond = self.pushed.pop(node.id)

    def slice(self, start, end):
        return self.ctx.source[start:end].decode("utf-8", "replace")

    def text(self, node):
        return self.ctx.text(node)


# ---------------------------------------------------------------------------
# Zig
# ---------------------------------------------------------------------------

ZIG_CONDITIONAL = {
    "if_statement", "if_expression", "switch_expression", "switch_statement", "while_statement", "while_expression",
    "for_statement", "for_expression", "catch_expression", "orelse_expression",
}
ZIG_CONTAINERS = {"struct_declaration": "struct", "enum_declaration": "enum", "union_declaration": "struct", "opaque_declaration": "struct"}
ZIG_DYNAMIC_BUILTINS = {"@call", "@field", "@fieldParentPtr"}


class _Zig(_Base):
    def string_value(self, node):
        if node is None or node.type != "string":
            return None
        parts = node.named_children
        if any(c.type not in ("string_content", "escape_sequence") for c in parts):
            return None
        if not parts:
            return ""
        return self.slice(parts[0].start_byte, parts[-1].end_byte)

    def builtin_name(self, node):
        ident = next((c for c in node.named_children if c.type == "builtin_identifier"), None)
        return self.text(ident) if ident is not None else None

    def builtin_args(self, node):
        args = next((c for c in node.named_children if c.type == "arguments"), None)
        return list(args.named_children) if args is not None else []

    def import_site(self, node, local=None):
        args = self.builtin_args(node)
        spec = self.string_value(args[0]) if args else None
        binding = _import_binding("import", spec, local=local, alias=local, loader="@import",
                                  relativeTo="dir" if spec and spec.endswith(".zig") else None)
        site = self.ctx.add_site("imports", _cap(self.text(node), binding), node, conditional=self.cond > 0,
                                 dynamic=spec is None, binding=binding)
        return site

    def include_site(self, node):
        args = self.builtin_args(node)
        spec = self.string_value(args[0]) if args else None
        binding = _import_binding("include", spec, loader="@cInclude", cImport=True)
        self.ctx.add_site("includes", _cap(self.text(node), binding), node, conditional=self.cond > 0,
                          dynamic=spec is None, binding=binding)

    def builtin(self, node):
        ctx = self.ctx
        name = self.builtin_name(node)
        if node.id in self.skip_nodes:
            return
        if name == "@import":
            self.import_site(node)
            return
        if name == "@cInclude":
            self.include_site(node)
            return
        if name in ("@cImport", None):
            return
        binding = {"kind": "identifier", "name": name, "receiver": None, "receiverKind": None, "builtin": True}
        dynamic = name in ZIG_DYNAMIC_BUILTINS
        if dynamic:
            binding["dynamic"] = True
        ctx.add_site("calls", name, node, conditional=self.cond > 0, dynamic=dynamic,
                     resolution=None if dynamic else "external", binding=binding)

    def call(self, node):
        ctx = self.ctx
        fn = node.child_by_field_name("function")
        binding = {"name": None, "receiver": None, "receiverKind": None}
        dynamic = False
        first_arg = next((c for c in node.named_children if fn is None or c.id != fn.id), None)
        end = first_arg.start_byte if first_arg is not None else node.end_byte
        raw = self.slice(node.start_byte, end).rstrip()
        if fn is not None and fn.type == "identifier":
            binding.update(kind="identifier", name=self.text(fn))
        elif fn is not None and fn.type == "field_expression":
            obj = fn.child_by_field_name("object")
            member = fn.child_by_field_name("member")
            otext = self.text(obj) if obj is not None else None
            rk = "expression"
            if obj is not None:
                if obj.type == "identifier":
                    rk = "identifier"
                elif obj.type == "field_expression" and all(c.type in ("identifier", "field_expression") for c in self.descend(obj)):
                    rk = "path"
            binding.update(kind="member", name=self.text(member) if member is not None else None,
                           receiver=_cap(otext, binding) if otext is not None else None, receiverKind=rk)
            if member is None:
                binding.update(kind="dynamic", dynamic=True)
                dynamic = True
        else:
            binding.update(kind="dynamic", dynamic=True)
            dynamic = True
        ctx.add_site("calls", _cap(raw, binding), node, conditional=self.cond > 0, dynamic=dynamic, binding=binding)

    @staticmethod
    def descend(node):
        out = []
        stack = list(node.named_children)
        while stack:
            n = stack.pop()
            out.append(n)
            stack.extend(n.named_children)
        return out

    def block_end(self, node):
        p = node.parent
        while p is not None and p.type not in ("block", "source_file", "struct_declaration", "enum_declaration", "union_declaration"):
            p = p.parent
        return p.end_byte if p is not None else node.end_byte

    def enter(self, node):
        if not node.is_named:
            return True
        ctx = self.ctx
        t = node.type
        if t in ZIG_CONDITIONAL:
            self.mark_cond(node)
        if t == "function_declaration":
            name_node = node.child_by_field_name("name")
            name = self.text(name_node)
            sc = ctx.current_scope
            member = sc is not None and sc.kind in ("struct", "enum")
            kids = [c.type for c in node.children if not c.is_named]
            body = node.child_by_field_name("body")
            bindings = {
                "visibility": "pub" if "pub" in kids else "", "export": "export" in kids, "extern": "extern" in kids,
                "inline": "inline" in kids, "body": body is not None,
            }
            sym = ctx.add_symbol("method" if member else "function", name, node, bindings=bindings,
                                 exported=("pub" in kids) or ("export" in kids))
            if sc is not None and sc.kind != "module":
                s, e = self.scope_range()
            else:
                s, e = 0, len(ctx.source)
            self.declare(name, "function", s, e, sym=sym, node=name_node, mutable=False)
            self.push(node, sym, name, "method" if member else "function")
            for c in node.children:
                if c.type == "parameters":
                    for p in c.named_children:
                        pn = p.child_by_field_name("name")
                        if p.type == "parameter" and pn is not None:
                            self.declare(self.text(pn), "parameter", node.start_byte, node.end_byte, node=p)
            return True
        if t == "test_declaration":
            label = next((c for c in node.named_children if c.type in ("string", "identifier")), None)
            name = (self.string_value(label) if label is not None and label.type == "string" else (self.text(label) if label is not None else "")) or "test"
            sym = ctx.add_symbol("function", name, node, bindings={"test": True, "body": True})
            self.push(node, sym, name, "function")
            return True
        if t == "variable_declaration":
            self.variable(node)
            return True
        if t in ZIG_CONTAINERS and (node.parent is None or node.parent.type != "variable_declaration"):
            sym = ctx.add_symbol(ZIG_CONTAINERS[t], "<anonymous>", node, bindings={"anonymous": True, "declarationKind": t.split("_")[0]})
            self.push(node, sym, None, ZIG_CONTAINERS[t])
            return True
        if t == "builtin_function":
            self.builtin(node)
            return True
        if t == "call_expression":
            self.call(node)
            return True
        return True

    def variable(self, node):
        ctx = self.ctx
        ident = next((c for c in node.named_children if c.type == "identifier"), None)
        if ident is None:
            return
        name = self.text(ident)
        kids = [c.type for c in node.children if not c.is_named]
        mutable = "var" in kids
        value = node.named_children[-1] if len(node.named_children) > 1 else None
        top = ctx.current_scope is None or ctx.current_scope.kind in ("module", "struct", "enum")
        if top:
            s, e = self.scope_range()
        else:
            s, e = node.end_byte, self.block_end(node)
        if value is not None and value.type in ZIG_CONTAINERS:
            kind = ZIG_CONTAINERS[value.type]
            sym = ctx.add_symbol(kind, name, node, bindings={"declarationKind": value.type.split("_")[0],
                                                              "visibility": "pub" if "pub" in kids else ""},
                                 exported="pub" in kids)
            self.declare(name, "type", s, e, sym=sym, node=ident)
            self.push(node, sym, name, kind)
            return
        if value is not None and value.type == "builtin_function" and self.builtin_name(value) == "@import":
            site = self.import_site(value, local=name)
            self.skip_nodes.add(value.id)
            self.declare(name, "import", s, e, site=site, node=ident)
            return
        self.declare(name, "variable", s, e, node=ident, mutable=mutable)


# ---------------------------------------------------------------------------
# Elixir
# ---------------------------------------------------------------------------

EX_DEF = {"def": ("function", "public"), "defp": ("function", "private"), "defmacro": ("macro", "public"),
          "defmacrop": ("macro", "private"), "defguard": ("macro", "public"), "defguardp": ("macro", "private"),
          "defdelegate": ("function", "public")}
EX_DIRECTIVES = {"alias", "import", "require", "use"}
EX_SPECIAL = {"if", "unless", "case", "cond", "with", "for", "try", "receive", "quote", "unquote", "unquote_splicing",
              "__block__", "__aliases__", "defmodule", "defprotocol", "defimpl", "defstruct", "defexception", "defoverridable"}
EX_COND = {"if", "unless", "case", "cond", "with", "try", "receive"}
EX_DYNAMIC = {"apply", "eval_string", "eval_quoted", "eval_file", "compile_string", "compile_quoted", "spawn_apply"}
EX_SKIP_ATTRS = {"spec", "type", "typep", "opaque", "callback", "macrocallback", "doc", "moduledoc", "typedoc",
                 "impl", "derive", "enforce_keys", "vsn", "on_load", "compile", "dialyzer", "optional_callbacks"}


class _Elixir(_Base):
    def __init__(self, ctx):
        super().__init__(ctx)
        self.fqn_stack: list[str] = []
        self.fqn_nodes: set[int] = set()

    def call_parts(self, node):
        target = node.child_by_field_name("target")
        args = next((c for c in node.named_children if c.type == "arguments"), None)
        block = next((c for c in node.named_children if c.type == "do_block"), None)
        return target, args, block

    def arg_list(self, args):
        return list(args.named_children) if args is not None else []

    def target_name(self, target):
        return self.text(target) if target is not None and target.type == "identifier" else None

    def idents(self, node, out):
        """Pattern-bound identifiers (skips call targets, pinned vars and underscore)."""
        stack = [node]
        while stack:
            n = stack.pop()
            if n.type == "identifier":
                p = n.parent
                tgt = p.child_by_field_name("target") if p is not None and p.type == "call" else None
                if tgt is not None and tgt.id == n.id:
                    continue
                if p is not None and p.type == "unary_operator" and p.children and p.children[0].type == "^":
                    continue
                txt = self.text(n)
                if txt != "_":
                    out.append(n)
                continue
            if n.type in ("anonymous_function",):
                continue
            stack.extend(n.named_children)
        out.sort(key=lambda c: c.start_byte)

    # -- definitions ------------------------------------------------------------
    def def_head(self, args):
        """(name, [param nodes], arity, dynamic, head_node, guard) from the first argument of a def."""
        items = self.arg_list(args)
        if not items:
            return None
        head = items[0]
        guard = None
        if head.type == "binary_operator":
            op = head.child_by_field_name("operator")
            if op is not None and self.text(op) == "when":
                guard = head.child_by_field_name("right")
                head = head.child_by_field_name("left")
        if head is None:
            return None
        if head.type == "call":
            target, hargs, _ = self.call_parts(head)
            name = self.target_name(target)
            params = self.arg_list(hargs)
            dyn = name is None
            return (name if name is not None else "<dynamic>", params, len(params), dyn, head, guard)
        if head.type == "identifier":
            return (self.text(head), [], 0, False, head, guard)
        if head.type == "binary_operator":
            op = head.child_by_field_name("operator")
            return (self.text(op) if op is not None else "<operator>", [head.child_by_field_name("left"), head.child_by_field_name("right")], 2, False, head, guard)
        return (self.text(head), [], 0, True, head, guard)

    def definition(self, node, name_kw, target):
        ctx = self.ctx
        kind, visibility = EX_DEF[name_kw]
        _, args, _ = self.call_parts(node)
        parsed = self.def_head(args)
        if parsed is None:
            ctx.mark_partial(f"{name_kw} without a head")
            return
        name, params, arity, dyn, head, guard = parsed
        if head.type == "call":
            self.skip_nodes.add(head.id)
        owner = ctx.current_scope
        bindings = {"arity": arity, "visibility": visibility, "kind": name_kw, "body": True, "caseInsensitive": False}
        if dyn:
            bindings["dynamicName"] = True
        if name_kw == "defdelegate":
            kw = [c for c in self.arg_list(args)[1:]]
            bindings["delegate"] = [self.text(k) for k in kw]
            bindings["body"] = False
        if owner is not None and owner.kind in ("namespace",) and self.fqn_stack:
            bindings["fqn"] = f"{self.fqn_stack[-1]}.{name}/{arity}"
        sym = ctx.add_symbol(kind, name, node, bindings=bindings, exported=visibility == "public",
                             qualified=(f"{self.fqn_stack[-1]}.{name}" if self.fqn_stack else name))
        s, e = self.scope_range()
        self.declare(name, "function", s, e, sym=sym, node=node, mutable=False)
        self.push(node, sym, name, kind)
        ps: list = []
        for p in params:
            if p is not None:
                self.idents(p, ps)
        for p in ps:
            self.declare(self.text(p), "parameter", node.start_byte, node.end_byte, node=p, mutable=False)

    def module_def(self, node, kw, target):
        ctx = self.ctx
        _, args, _ = self.call_parts(node)
        items = self.arg_list(args)
        first = items[0] if items else None
        name = self.text(first) if first is not None else ""
        bindings: dict = {"declarationKind": kw}
        kind = {"defmodule": "namespace", "defprotocol": "interface", "defimpl": "impl"}[kw]
        if kw == "defimpl":
            bindings["implTrait"] = name
            for it in items[1:]:
                if it.type == "keywords":
                    for pair in it.named_children:
                        k = pair.child_by_field_name("key")
                        v = pair.child_by_field_name("value")
                        if k is not None and v is not None and self.text(k).strip().rstrip(":") == "for":
                            bindings["implFor"] = self.text(v)
        parent = self.fqn_stack[-1] if self.fqn_stack else None
        full = name if (parent is None or kw == "defimpl") else f"{parent}.{name}"
        bindings["fqn"] = full
        sym = ctx.add_symbol(kind, name, node, bindings=bindings, qualified=full)
        if kind != "impl" and name:
            s, e = self.scope_range()
            self.declare(name.rsplit(".", 1)[-1] if parent else name, "namespace", s, e, sym=sym, node=node)
        self.push(node, sym, name, kind)
        self.fqn_stack.append(full)
        self.fqn_nodes.add(node.id)



    def directive(self, node, kw):
        ctx = self.ctx
        _, args, _ = self.call_parts(node)
        items = self.arg_list(args)
        if not items:
            return
        first = items[0]
        opts = {}
        for it in items[1:]:
            if it.type == "keywords":
                for pair in it.named_children:
                    k = pair.child_by_field_name("key")
                    v = pair.child_by_field_name("value")
                    if k is not None and v is not None:
                        opts[self.text(k).strip().rstrip(":").strip()] = v
        cond = self.cond > 0
        kind = {"alias": "alias", "import": "import", "require": "require", "use": "use"}[kw]

        def emit(spec, node_for_span, imported=None, alias=None, namespace=False, raw=None):
            lit = spec if spec is not None else None
            extra = {"loader": kw}
            if kw == "import":
                for key in ("only", "except", "type"):
                    if key in opts:
                        extra[key] = self.text(opts[key])
            if kw == "use":
                extra["injectsCode"] = True
                if len(items) > 1:
                    extra["options"] = _cap(self.text(items[1]))
            if kw == "require" and "as" in opts:
                alias = self.text(opts["as"])
            local = alias if alias else (spec.rsplit(".", 1)[-1] if (spec and kw in ("alias", "require")) else None)
            binding = _import_binding(kind, lit, imported=imported, alias=alias, namespace=namespace, local=local, **extra)
            site = ctx.add_site("imports", _cap(raw if raw is not None else self.text(node_for_span), binding), node_for_span,
                                conditional=cond, dynamic=lit is None, binding=binding)
            if local and kw in ("alias", "require"):
                s, e = self.scope_range()
                self.declare(local, "import", node.end_byte if kw == "alias" else s, e, site=site, node=node)

        if first.type == "alias":
            spec = self.text(first)
            if kw == "alias" and "as" in opts:
                emit(spec, node, alias=self.text(opts["as"]), raw=self.text(node))
            else:
                emit(spec, node, namespace=(kw in ("import", "use")), raw=self.text(node))
        elif first.type == "dot" and kw == "alias":
            left = first.child_by_field_name("left")
            right = first.child_by_field_name("right")
            if left is not None and right is not None and right.type == "tuple":
                for m in right.named_children:
                    if m.type == "alias":
                        emit(f"{self.text(left)}.{self.text(m)}", m, raw=self.text(node))
            else:
                emit(None, node)
        elif first.type == "atom":
            emit(self.text(first), node, namespace=True, raw=self.text(node))
        else:
            emit(None, node)

    # -- calls ----------------------------------------------------------------------
    def is_capture(self, node):
        p = node.parent
        if p is not None and p.type == "binary_operator":
            op = p.child_by_field_name("operator")
            if op is not None and self.text(op) == "/":
                gp = p.parent
                if gp is not None and gp.type == "unary_operator" and gp.children and gp.children[0].type == "&":
                    return True
        return False

    def piped(self, node):
        p = node.parent
        if p is not None and p.type == "binary_operator":
            op = p.child_by_field_name("operator")
            right = p.child_by_field_name("right")
            return op is not None and self.text(op) == "|>" and right is not None and right.id == node.id
        return False

    def call(self, node):
        ctx = self.ctx
        target, args, block = self.call_parts(node)
        if target is None:
            return
        items = self.arg_list(args)
        arity = len(items) + (1 if self.piped(node) else 0)
        end = args.start_byte if args is not None else (block.start_byte if block is not None else node.end_byte)
        raw = self.slice(node.start_byte, end).rstrip()
        cond = self.cond > 0
        binding = {"name": None, "receiver": None, "receiverKind": None, "arity": arity}
        if self.piped(node):
            binding["piped"] = True
        relation = "calls"
        dynamic = False
        if target.type == "identifier":
            name = self.text(target)
            binding.update(kind="identifier", name=name)
            dynamic = name in EX_DYNAMIC
        elif target.type == "dot":
            left = target.child_by_field_name("left")
            right = target.child_by_field_name("right")
            ltext = self.text(left) if left is not None else None
            if left is None:
                rk = None
            elif left.type == "alias":
                rk = "path"
            elif left.type == "atom":
                rk = "atom"
            elif left.type == "identifier":
                rk = "identifier"
            else:
                rk = "expression"
            binding.update(kind="member", receiver=_cap(ltext, binding) if ltext is not None else None, receiverKind=rk)
            if right is None or right.type != "identifier":
                binding.update(kind="dynamic", dynamic=True)
                dynamic = True
            else:
                binding["name"] = self.text(right)
                dynamic = binding["name"] in EX_DYNAMIC
                if args is None and block is None:
                    if self.is_capture(node):
                        relation = "references"
                        binding["capture"] = True
                    elif rk != "path" and rk != "atom":
                        relation = "references"
                        binding["ambiguous"] = "mapAccessOrZeroArityCall"
                        binding["noParens"] = True
        else:
            binding.update(kind="dynamic", dynamic=True)
            dynamic = True
        if dynamic:
            binding["dynamic"] = True
        ctx.add_site(relation, _cap(raw, binding), node, conditional=cond, dynamic=dynamic, binding=binding)

    # -- traversal ---------------------------------------------------------------------
    def enter(self, node):
        if not node.is_named:
            return True
        ctx = self.ctx
        t = node.type
        if t == "call":
            if node.id in self.skip_nodes:
                return True
            target, args, block = self.call_parts(node)
            name = self.target_name(target)
            if name in EX_COND:
                self.mark_cond(node)
            if name in EX_DEF and args is not None:
                self.definition(node, name, target)
                return True
            if name in ("defmodule", "defprotocol", "defimpl") and args is not None:
                self.module_def(node, name, target)
                return True
            if name in EX_DIRECTIVES and args is not None:
                self.directive(node, name)
                return True
            if name in EX_SPECIAL:
                return True
            self.call(node)
            return True
        if t == "unary_operator":
            op = node.children[0] if node.children else None
            operand = node.child_by_field_name("operand")
            if op is not None and op.type == "@" and operand is not None and operand.type == "call":
                tgt, aargs, _ = self.call_parts(operand)
                an = self.target_name(tgt)
                if an == "behaviour" and aargs is not None:
                    for a in aargs.named_children:
                        txt = self.text(a)
                        ctx.add_site("references", txt, a, binding={"kind": "path" if "." in txt else "identifier",
                                     "name": txt.rsplit(".", 1)[-1], "receiver": txt.rsplit(".", 1)[0] if "." in txt else None,
                                     "receiverKind": "path" if "." in txt else None, "role": "behaviour"})
                    return False
                if an in EX_SKIP_ATTRS:
                    return False
            if op is not None and op.type == "&" and operand is not None and operand.type == "binary_operator":
                oper = operand.child_by_field_name("operator")
                left = operand.child_by_field_name("left")
                right = operand.child_by_field_name("right")
                if oper is not None and self.text(oper) == "/" and left is not None and left.type == "identifier":
                    arity = self.text(right) if right is not None else None
                    ctx.add_site("references", self.text(left), left, conditional=self.cond > 0,
                                 binding={"kind": "identifier", "name": self.text(left), "receiver": None, "receiverKind": None,
                                          "capture": True, "arity": int(arity) if arity and arity.isdigit() else None})
            return True
        if t == "anonymous_function":
            sym = ctx.add_symbol("lambda", "<fn>", node, bindings={})
            self.push(node, sym, None, "lambda")
            return True
        if t == "stab_clause":
            left = node.child_by_field_name("left")
            if left is not None:
                ps: list = []
                self.idents(left, ps)
                is_fn = node.parent is not None and node.parent.type == "anonymous_function"
                for p in ps:
                    self.declare(self.text(p), "parameter" if is_fn else "variable", node.start_byte, node.end_byte, node=p, mutable=False)
            return True
        if t == "binary_operator":
            op = node.child_by_field_name("operator")
            if op is not None and self.text(op) == "=":
                left = node.child_by_field_name("left")
                if left is not None:
                    ps = []
                    self.idents(left, ps)
                    s, e = node.end_byte, self.scope_range()[1]
                    for p in ps:
                        self.declare(self.text(p), "variable", s, e, node=p, mutable=False)
            return True
        return True

    def leave(self, node):
        if node.id in self.fqn_nodes:
            self.fqn_nodes.discard(node.id)
            self.fqn_stack.pop()
        super().leave(node)


# ---------------------------------------------------------------------------
# Julia
# ---------------------------------------------------------------------------

JL_COND = {"if_statement", "elseif_clause", "else_clause", "ternary_expression", "for_statement", "while_statement",
           "try_statement", "catch_clause"}
JL_DYNAMIC = {"eval", "invokelatest", "include_string", "evalfile", "getfield", "setfield!", "getproperty"}


class _Julia(_Base):
    def string_value(self, node):
        if node is None or node.type != "string_literal":
            return None
        parts = node.named_children
        if any(c.type not in ("content", "escape_sequence") for c in parts):
            return None
        if not parts:
            return ""
        return self.slice(parts[0].start_byte, parts[-1].end_byte)

    def signature_call(self, node):
        """(name_text, call_node, owner_type|None) of a function head."""
        n = node
        while n is not None and n.type in ("signature", "where_expression", "typed_expression", "parenthesized_expression"):
            n = n.named_children[0] if n.named_children else None
        if n is None or n.type != "call_expression":
            return None, None, None
        callee = n.named_children[0] if n.named_children else None
        if callee is None:
            return None, n, None
        if callee.type == "identifier":
            return self.text(callee), n, None
        if callee.type == "field_expression":
            return self.text(callee.named_children[-1]), n, None
        if callee.type == "parametrized_type_expression":
            return self.text(callee.named_children[0]), n, None
        if callee.type == "parenthesized_expression":
            inner = callee.named_children[0] if callee.named_children else None
            typ = None
            if inner is not None and inner.type == "typed_expression" and len(inner.named_children) > 1:
                typ = self.text(inner.named_children[-1])
            return "<callable>", n, typ
        return None, n, None

    def params_of(self, call):
        out = []
        arg_list = next((c for c in call.named_children if c.type == "argument_list"), None)
        if arg_list is None:
            return out
        stack = list(arg_list.named_children)
        for p in stack:
            q = p
            if q.type in ("named_argument", "typed_expression", "splat_expression", "optional_parameter", "typed_parameter"):
                q = q.named_children[0] if q.named_children else None
            if q is not None and q.type == "identifier":
                out.append(q)
            elif q is not None and q.type == "argument_list":
                for r in q.named_children:
                    rr = r
                    if rr.type in ("named_argument", "typed_expression"):
                        rr = rr.named_children[0] if rr.named_children else None
                    if rr is not None and rr.type == "identifier":
                        out.append(rr)
        return out

    def function_symbol(self, node, head, body_is_node=True):
        ctx = self.ctx
        name, call, owner_type = self.signature_call(head)
        if call is not None:
            self.skip_nodes.add(call.id)
        if name is None:
            name = "<dynamic>"
            dyn = True
        else:
            dyn = False
        bindings = {"body": True}
        if dyn:
            bindings["dynamicName"] = True
        if call is not None and call.named_children and call.named_children[0].type == "field_expression":
            bindings["methodOf"] = self.text(call.named_children[0].named_children[0])  # e.g. Base.show extends Base's function
        if owner_type:
            bindings["callableType"] = owner_type
        sc = ctx.current_scope
        if sc is not None and sc.kind == "namespace" and sc.name:
            bindings["fqn"] = f"{sc.name}.{name}"
        sym = ctx.add_symbol("function", name, node, bindings=bindings)
        s, e = self.scope_range()
        if name != "<callable>" and name != "<dynamic>":
            # methods extend one generic function: never a unique lexical target
            self.declare(name, "function", s, e, sym=sym, node=node, mutable=True)
        self.push(node, sym, name, "function")
        if call is not None:
            for p in self.params_of(call):
                self.declare(self.text(p), "parameter", node.start_byte, node.end_byte, node=p, mutable=True)

    def import_statements(self, node):
        ctx = self.ctx
        is_using = node.type == "using_statement"
        cond = self.cond > 0

        def dots(n):
            i = n.start_byte
            while i > 0 and ctx.source[i - 1:i] == b".":
                i -= 1
            return self.slice(i, n.start_byte)

        def path_text(n):
            return dots(n) + self.text(n)

        def emit(span_node, spec, imported=None, alias=None, namespace=False, local=None):
            binding = _import_binding("using" if is_using else "import", spec, imported=imported, alias=alias,
                                      namespace=namespace, local=local, loader="using" if is_using else "import")
            site = ctx.add_site("imports", _cap(self.text(span_node), binding), span_node, conditional=cond, binding=binding)
            if local:
                s, e = self.scope_range()
                self.declare(local, "import", s, e, site=site, node=span_node)

        for c in node.named_children:
            if c.type in ("identifier", "import_path", "scoped_identifier", "field_expression"):
                spec = path_text(c)
                emit(c, spec, namespace=is_using, local=spec.rsplit(".", 1)[-1].lstrip("."))
            elif c.type == "import_alias":
                parts = c.named_children
                if len(parts) >= 2:
                    spec = path_text(parts[0])
                    emit(c, spec, alias=self.text(parts[-1]), local=self.text(parts[-1]))
            elif c.type == "selected_import":
                parts = c.named_children
                if not parts:
                    continue
                spec = path_text(parts[0])
                for m in parts[1:]:
                    if m.type == "import_alias" and len(m.named_children) >= 2:
                        emit(m, spec, imported=self.text(m.named_children[0]), alias=self.text(m.named_children[-1]),
                             local=self.text(m.named_children[-1]))
                    else:
                        nm = self.text(m)
                        emit(m, spec, imported=nm, local=nm)

    def call(self, node):
        ctx = self.ctx
        kids = node.named_children
        callee = kids[0] if kids else None
        arg_list = next((c for c in kids if c.type == "argument_list"), None)
        end = arg_list.start_byte if arg_list is not None else node.end_byte
        raw = self.slice(node.start_byte, end).rstrip()
        cond = self.cond > 0
        binding = {"name": None, "receiver": None, "receiverKind": None}
        dynamic = False
        if callee is not None and callee.type == "identifier":
            name = self.text(callee)
            if name == "include":
                args = list(arg_list.named_children) if arg_list is not None else []
                lit = self.string_value(args[-1]) if args and (len(args) == 1 or args[-1].type == "string_literal") else None
                b = _import_binding("include", lit, loader="include", relativeTo="dir")
                ctx.add_site("includes", _cap(self.text(node), b), node, conditional=cond, dynamic=lit is None, binding=b)
                return
            binding.update(kind="identifier", name=name)
            dynamic = name in JL_DYNAMIC
        elif callee is not None and callee.type == "field_expression":
            val = callee.child_by_field_name("value") or (callee.named_children[0] if callee.named_children else None)
            member = callee.named_children[-1] if callee.named_children else None
            vtext = self.text(val) if val is not None else None
            rk = "identifier" if val is not None and val.type == "identifier" else ("path" if val is not None and val.type == "field_expression" else "expression")
            binding.update(kind="member", name=self.text(member) if member is not None else None, receiver=_cap(vtext, binding) if vtext else None, receiverKind=rk)
            dynamic = binding["name"] in JL_DYNAMIC
        elif callee is not None and callee.type == "parametrized_type_expression":
            inner = callee.named_children[0]
            binding.update(kind="identifier", name=self.text(inner), parametric=True)
        else:
            binding.update(kind="dynamic", dynamic=True)
            dynamic = True
        if node.type == "broadcast_call_expression":
            binding["broadcast"] = True
        if dynamic:
            binding["dynamic"] = True
        ctx.add_site("calls", _cap(raw, binding), node, conditional=cond, dynamic=dynamic, binding=binding)

    def macro_site(self, node):
        ctx = self.ctx
        mid = next((c for c in node.named_children if c.type == "macro_identifier"), None)
        if mid is None:
            return
        name = self.text(mid).lstrip("@")
        idx = node.named_children.index(mid)
        recv = self.text(node.named_children[idx - 1]) if idx > 0 and node.named_children[idx - 1].type in ("identifier", "field_expression") else None
        binding = {"kind": "macro", "name": name, "receiver": recv, "receiverKind": "path" if recv else None}
        dynamic = name in ("eval",)
        if dynamic:
            binding["dynamic"] = True
        raw = self.slice(node.start_byte, mid.end_byte)
        ctx.add_site("macros", _cap(raw, binding), node, conditional=self.cond > 0, dynamic=dynamic, binding=binding)

    def type_name(self, node):
        n = node
        while n is not None and n.type in ("type_head", "binary_expression", "parametrized_type_expression", "typed_expression"):
            n = n.named_children[0] if n.named_children else None
        return self.text(n) if n is not None else ""

    def enter(self, node):
        if not node.is_named:
            return True
        ctx = self.ctx
        t = node.type
        if t in JL_COND:
            self.mark_cond(node)
        if t == "module_definition":
            name_node = node.child_by_field_name("name")
            name = self.text(name_node)
            sc = ctx.current_scope
            full = f"{sc.name}.{name}" if sc is not None and sc.kind == "namespace" and sc.name else name
            sym = ctx.add_symbol("namespace", name, node, bindings={"fqn": full, "declarationKind": "module"}, qualified=full)
            s, e = self.scope_range()
            self.declare(name, "namespace", s, e, sym=sym, node=name_node)
            self.push(node, sym, full, "namespace")
            return True
        if t == "function_definition":
            head = next((c for c in node.named_children if c.type == "signature"), None)
            if head is None:
                return True
            self.function_symbol(node, head)
            return True
        if t == "macro_definition":
            head = next((c for c in node.named_children if c.type == "signature"), None)
            name, call, _ = self.signature_call(head) if head is not None else (None, None, None)
            if call is not None:
                self.skip_nodes.add(call.id)
            name = name or "<dynamic>"
            sym = ctx.add_symbol("macro", name, node, bindings={"body": True})
            s, e = self.scope_range()
            self.declare("@" + name, "function", s, e, sym=sym, node=node, mutable=True)
            self.push(node, sym, name, "macro")
            if call is not None:
                for p in self.params_of(call):
                    self.declare(self.text(p), "parameter", node.start_byte, node.end_byte, node=p, mutable=True)
            return True
        if t in ("struct_definition", "abstract_definition", "primitive_definition"):
            head = next((c for c in node.named_children if c.type == "type_head"), None)
            name = self.type_name(head) if head is not None else ""
            kind = "struct" if t == "struct_definition" else "type"
            bindings = {"declarationKind": t.split("_")[0], "mutable": any(c.type == "mutable" or (not c.is_named and c.type == "mutable") for c in node.children)}
            if head is not None and head.named_children and head.named_children[0].type == "binary_expression":
                be = head.named_children[0]
                if len(be.named_children) >= 3:
                    sup = self.text(be.named_children[-1])
                    bindings["extends"] = [sup]
                    ctx.add_site("references", sup, be.named_children[-1], binding={
                        "kind": "identifier", "name": sup.split("{")[0].rsplit(".", 1)[-1], "receiver": None,
                        "receiverKind": None, "role": "supertype"})
            sym = ctx.add_symbol(kind, name, node, bindings=bindings)
            s, e = self.scope_range()
            self.declare(name, "class" if kind == "struct" else "type", s, e, sym=sym, node=node, mutable=False)
            if t == "struct_definition":
                self.push(node, sym, name, kind)
            return True
        if t in ("using_statement", "import_statement"):
            self.import_statements(node)
            return False
        if t == "assignment":
            left = node.named_children[0] if node.named_children else None
            right = node.named_children[-1] if node.named_children else None
            if left is not None and (left.type in ("call_expression", "where_expression") or
                                     (left.type == "typed_expression" and left.named_children and left.named_children[0].type == "call_expression")):
                self.function_symbol(node, left)
                return True
            if left is not None and left.type == "identifier":
                if right is not None and right.type == "arrow_function_expression":
                    return True  # declared with the closure
                const = node.parent is not None and node.parent.type == "const_statement"
                s, e = node.end_byte, self.scope_range()[1]
                self.declare(self.text(left), "variable", s, e, node=left, mutable=not const)
            return True
        if t == "arrow_function_expression":
            parent = node.parent
            outer = ctx.owner
            rng = self.scope_range()
            sym = ctx.add_symbol("lambda", "<arrow>", node, bindings={})
            self.push(node, sym, None, "lambda")
            first = node.named_children[0] if node.named_children else None
            ps = []
            if first is not None:
                stack = [first]
                while stack:
                    n = stack.pop()
                    if n.type == "identifier":
                        ps.append(n)
                    elif n.type in ("tuple_expression", "parenthesized_expression", "typed_expression", "argument_list"):
                        stack.extend(n.named_children[:1] if n.type == "typed_expression" else n.named_children)
            for p in ps:
                self.declare(self.text(p), "parameter", node.start_byte, node.end_byte, node=p)
            if parent is not None and parent.type == "assignment":
                lhs = parent.named_children[0] if parent.named_children else None
                if lhs is not None and lhs.type == "identifier":
                    nm = self.text(lhs)
                    sym["bindings"]["assignedTo"] = nm
                    self.declare(nm, "variable", parent.end_byte, rng[1], sym=sym, node=lhs, mutable=True, owner=outer)
            return True
        if t == "do_clause":
            sym = ctx.add_symbol("lambda", "<do>", node, bindings={})
            self.push(node, sym, None, "lambda")
            al = next((c for c in node.named_children if c.type == "argument_list"), None)
            if al is not None:
                for p in al.named_children:
                    if p.type == "identifier":
                        self.declare(self.text(p), "parameter", node.start_byte, node.end_byte, node=p)
            return True
        if t == "binary_expression":
            kids = node.children
            ops = [c for c in kids if not c.is_named or c.type == "operator"]
            if any(self.text(c) == "|>" for c in ops):
                right = node.named_children[-1] if node.named_children else None
                if right is not None and right.type in ("identifier", "field_expression"):
                    txt = self.text(right)
                    ctx.add_site("references", txt, right, conditional=self.cond > 0, binding={
                        "kind": "identifier" if right.type == "identifier" else "member",
                        "name": txt.rsplit(".", 1)[-1], "receiver": txt.rsplit(".", 1)[0] if "." in txt else None,
                        "receiverKind": None, "pipeTarget": True})
            return True
        if t in ("call_expression", "broadcast_call_expression"):
            if node.id not in self.skip_nodes:
                self.call(node)
            return True
        if t == "macrocall_expression":
            self.macro_site(node)
            return True
        return True


# ---------------------------------------------------------------------------
# Fortran
# ---------------------------------------------------------------------------

FT_COND = {"if_statement", "elseif_clause", "else_clause", "select_case_statement", "case_statement", "do_loop_statement",
           "where_statement", "forall_statement"}
FT_PP = {"preproc_if", "preproc_ifdef", "preproc_else", "preproc_elif", "preproc_elifdef"}
FT_INTRINSIC_MODULES = {"iso_c_binding", "iso_fortran_env", "ieee_arithmetic", "ieee_exceptions", "ieee_features"}
FT_INTRINSICS = frozenset("""
abs acos aimag aint all anint any asin associated atan atan2 bit_size ceiling char cmplx conjg cos cosh count cshift
dble digits dot_product dprod eoshift epsilon exp exponent floor fraction huge iachar iand ibclr ibits ibset ichar
ieor index int ior ishft ishftc kind lbound len len_trim lge lgt lle llt log log10 logical matmul max maxloc maxval
merge min minloc minval mod modulo nint not pack present product radix real repeat reshape rrspacing scale scan
selected_int_kind selected_real_kind set_exponent shape sign sin sinh size spacing spread sqrt sum tan tanh tiny
transpose trim ubound unpack verify allocated command_argument_count get_command_argument cpu_time date_and_time
execute_command_line system_clock random_number random_seed null c_loc c_f_pointer c_associated
""".split())


class _Fortran(_Base):
    def __init__(self, ctx):
        super().__init__(ctx)
        self.vars: dict[str, set] = {}
        self.module_stack: list[str] = []
        self.mod_nodes: set[int] = set()
        self.saw_preproc = False

    def name_of(self, node, *types):
        for c in node.named_children:
            if c.type in types:
                for d in c.named_children:
                    if d.type == "name":
                        return self.text(d), d
                return None, None
        return None, None

    def first_name(self, stmt):
        n = stmt.child_by_field_name("name") if stmt is not None else None
        if n is None and stmt is not None:
            n = next((c for c in stmt.named_children if c.type in ("name", "identifier")), None)
        return n

    def enter_unit(self, node, kind, stmt_type, extra):
        ctx = self.ctx
        stmt = next((c for c in node.named_children if c.type == stmt_type), None)
        name_node = self.first_name(stmt)
        name = self.text(name_node) if name_node is not None else ""
        parent = self.module_stack[-1] if self.module_stack else None
        fqn = (f"{parent}::{name}" if parent and node.type in ("subroutine", "function") else name).lower()
        bindings = {"fqn": fqn, "caseInsensitive": True, "body": True}
        bindings.update(extra)
        sym = ctx.add_symbol(kind, name, node, bindings=bindings)
        s, e = self.scope_range()
        self.declare(name, "namespace" if kind == "namespace" else "function", s, e, sym=sym, node=name_node or node, mutable=False)
        self.push(node, sym, name, kind)
        self.vars[sym["id"]] = set()
        if kind == "namespace":
            self.module_stack.append(name)
            self.mod_nodes.add(node.id)
        if stmt is not None:
            params = next((c for c in stmt.named_children if c.type == "parameters"), None)
            if params is not None:
                for p in params.named_children:
                    if p.type == "identifier":
                        self.declare(self.text(p), "parameter", node.start_byte, node.end_byte, node=p, mutable=False)
                        self.vars[sym["id"]].add(self.text(p).lower())
        return sym



    def use(self, node):
        ctx = self.ctx
        mn = next((c for c in node.named_children if c.type == "module_name"), None)
        if mn is None:
            return
        mod = self.text(mn)
        intrinsic = any((not c.is_named and c.type == "intrinsic") for c in node.children) or mod.lower() in FT_INTRINSIC_MODULES
        items = next((c for c in node.named_children if c.type in ("included_items",)), None)
        cond = self.cond > 0
        only = any(c.type == "only" or (not c.is_named and c.type == "only") for c in node.children) or items is not None
        base = {"loader": "use", "caseInsensitive": True, "intrinsic": intrinsic}
        if items is None:
            b = _import_binding("use", mod, namespace=True, **base)
            ctx.add_site("imports", _cap(self.text(node), b), node, conditional=cond, binding=b)
            return
        for it in items.named_children:
            if it.type == "use_alias":
                ln = next((c for c in it.named_children if c.type == "local_name"), None)
                rem = [c for c in it.named_children if c.type != "local_name"]
                imported = self.text(rem[0]) if rem else None
                local = self.text(ln) if ln is not None else imported
                b = _import_binding("use", mod, imported=imported, alias=local if ln is not None else None, local=local, **base)
            else:
                nm = self.text(it)
                b = _import_binding("use", mod, imported=nm, local=nm, **base)
            site = ctx.add_site("imports", _cap(self.text(it), b), it, conditional=cond, binding=b)
            s, e = self.scope_range()
            self.declare(b["local"], "import", s, e, site=site, node=it)

    def call_like(self, node):
        ctx = self.ctx
        cond = self.cond > 0
        scope_vars = self.vars.get(ctx.owner, set())
        if node.type == "subroutine_call":
            sub = node.child_by_field_name("subroutine")
            args = next((c for c in node.named_children if c.type == "argument_list"), None)
            end = args.start_byte if args is not None else node.end_byte
            raw = self.slice(node.start_byte, end).rstrip()
            binding = {"name": None, "receiver": None, "receiverKind": None, "caseInsensitive": True, "subroutine": True}
            if sub is not None and sub.type == "identifier":
                binding.update(kind="identifier", name=self.text(sub))
            elif sub is not None and sub.type == "derived_type_member_expression":
                parts = sub.named_children
                member = next((c for c in parts if c.type == "type_member"), None)
                recv = next((c for c in parts if c.type != "type_member"), None)
                binding.update(kind="member", name=self.text(member) if member is not None else None,
                               receiver=self.text(recv) if recv is not None else None,
                               receiverKind="identifier" if recv is not None and recv.type == "identifier" else "expression")
            else:
                binding.update(kind="dynamic", dynamic=True)
            ctx.add_site("calls", _cap(raw, binding), node, conditional=cond, dynamic=binding.get("dynamic", False), binding=binding)
            return
        kids = node.named_children
        callee = kids[0] if kids else None
        args = next((c for c in kids if c.type == "argument_list"), None)
        end = args.start_byte if args is not None else node.end_byte
        raw = self.slice(node.start_byte, end).rstrip()
        binding = {"name": None, "receiver": None, "receiverKind": None, "caseInsensitive": True,
                   "ambiguous": "callOrArrayElement"}
        relation = "calls"
        if callee is not None and callee.type == "identifier":
            name = self.text(callee)
            binding.update(kind="identifier", name=name)
            if name.lower() in scope_vars:
                relation = "references"
                binding.update(kind="index")
                binding.pop("ambiguous", None)
                binding["arrayElement"] = True
            elif name.lower() in FT_INTRINSICS:
                binding["intrinsic"] = True
        elif callee is not None and callee.type == "derived_type_member_expression":
            parts = callee.named_children
            member = next((c for c in parts if c.type == "type_member"), None)
            recv = next((c for c in parts if c.type != "type_member"), None)
            binding.update(kind="member", name=self.text(member) if member is not None else None,
                           receiver=self.text(recv) if recv is not None else None, receiverKind="identifier")
        else:
            binding.update(kind="dynamic", dynamic=True)
        ctx.add_site(relation, _cap(raw, binding), node, conditional=cond, dynamic=binding.get("dynamic", False), binding=binding)

    def enter(self, node):
        if not node.is_named:
            return True  # keyword tokens share names with node types (`module`, `function`, `subroutine`)
        ctx = self.ctx
        t = node.type
        if t in FT_PP:
            self.mark_pp(node)
            self.saw_preproc = True
        elif t in FT_COND:
            self.mark_cond(node)
        if t == "module":
            self.enter_unit(node, "namespace", "module_statement", {"declarationKind": "module"})
            return True
        if t == "program":
            self.enter_unit(node, "function", "program_statement", {"program": True})
            return True
        if t in ("subroutine", "function"):
            self.enter_unit(node, "function", t + "_statement", {"declarationKind": t})
            return True
        if t == "derived_type_definition":
            stmt = next((c for c in node.named_children if c.type == "derived_type_statement"), None)
            tn = next((c for c in stmt.named_children if c.type == "type_name"), None) if stmt is not None else None
            name = self.text(tn) if tn is not None else ""
            bound = []
            for c in node.named_children:
                if c.type == "derived_type_procedures":
                    for p in c.named_children:
                        if p.type == "procedure_statement":
                            bound.extend(self.text(d) for d in p.named_children if d.type == "method_name")
            sym = ctx.add_symbol("struct", name, node, bindings={"caseInsensitive": True, "typeBound": bound,
                                                                  "fqn": (f"{self.module_stack[-1]}::{name}" if self.module_stack else name).lower()})
            s, e = self.scope_range()
            self.declare(name, "class", s, e, sym=sym, node=tn or node, mutable=False)
            return True
        if t == "interface":
            stmt = next((c for c in node.named_children if c.type == "interface_statement"), None)
            nm = next((c for c in stmt.named_children if c.type == "name"), None) if stmt is not None else None
            name = self.text(nm) if nm is not None else ""
            sym = ctx.add_symbol("interface", name, node, bindings={"caseInsensitive": True, "generic": bool(name)})
            self.push(node, sym, name, "interface")
            return True
        if t == "procedure_statement" and ctx.current_scope is not None and ctx.current_scope.kind == "interface":
            for d in node.named_children:
                if d.type == "method_name":
                    nm = self.text(d)
                    ctx.add_site("references", nm, d, binding={"kind": "identifier", "name": nm, "receiver": None, "receiverKind": None,
                                                              "role": "moduleProcedure", "caseInsensitive": True})
            return True
        if t == "variable_declaration":
            if node.parent is not None and node.parent.type == "derived_type_definition":
                return False  # components are not lexical names
            sid = ctx.owner
            for i, c in enumerate(node.children):
                if node.field_name_for_child(i) != "declarator":
                    continue
                ident = c if c.type == "identifier" else next((d for d in c.named_children if d.type == "identifier"), None)
                if ident is None:
                    continue
                nm = self.text(ident)
                known = self.vars.setdefault(sid, set())
                if nm.lower() in known:
                    continue  # type declaration of a dummy argument, not a second binding
                known.add(nm.lower())
                s, e = self.scope_range()
                self.declare(nm, "variable", s, e, node=ident, mutable=True)
            return True
        if t == "use_statement":
            self.use(node)
            return False
        if t == "include_statement":
            path = node.child_by_field_name("path")
            lit = self.text(path).strip("'\"") if path is not None else None
            b = _import_binding("include", lit, loader="include", caseInsensitive=True)
            ctx.add_site("includes", _cap(self.text(node), b), node, conditional=self.cond > 0, dynamic=lit is None, binding=b)
            return True
        if t == "preproc_include":
            path = node.child_by_field_name("path")
            text = self.text(path) if path is not None else ""
            system = text.startswith("<")
            lit = text.strip('<>"') if text else None
            b = _import_binding("include", lit, loader="#include", system=system)
            ctx.add_site("includes", _cap(self.text(node), b), node, conditional=self.cond > 0, dynamic=lit is None, binding=b)
            self.saw_preproc = True
            return True
        if t in ("subroutine_call", "call_expression"):
            self.call_like(node)
            return True
        return True

    def leave(self, node):
        if node.id in self.mod_nodes:
            self.mod_nodes.discard(node.id)
            self.module_stack.pop()
        super().leave(node)


# ---------------------------------------------------------------------------
# Verilog / SystemVerilog
# ---------------------------------------------------------------------------

VL_COND = {"conditional_statement", "case_statement", "loop_statement", "if_generate_construct",
           "case_generate_construct", "conditional_generate_construct", "loop_generate_construct"}


class _Verilog(_Base):
    def ids(self, node):
        out = []
        stack = [node]
        while stack:
            n = stack.pop()
            if n.type == "simple_identifier":
                out.append(n)
            else:
                stack.extend(reversed(n.named_children))
        out.sort(key=lambda n: n.start_byte)
        return out

    def first_id(self, node):
        ids = self.ids(node) if node is not None else []
        return ids[0] if ids else None

    def name_under(self, node, *types):
        for c in node.named_children:
            if c.type in types:
                n = self.first_id(c)
                if n is not None:
                    return self.text(n), n
        return None, None

    def instantiation(self, node):
        ctx = self.ctx
        kids = node.named_children
        type_node = next((c for c in kids if c.type in ("simple_identifier", "checker_identifier")), None)
        if type_node is None:
            return
        mod = self.text(type_node) if type_node.type == "simple_identifier" else (self.text(self.first_id(type_node) or type_node))
        params = next((c for c in kids if c.type == "parameter_value_assignment"), None)
        cond = self.cond > 0
        for inst in kids:
            if inst.type == "hierarchical_instance":
                noi = next((c for c in inst.named_children if c.type == "name_of_instance"), None)
                iname = self.text(self.first_id(noi)) if noi is not None and self.first_id(noi) is not None else None
                ports = next((c for c in inst.named_children if c.type == "list_of_port_connections"), None)
                style = None
                if ports is not None and ports.named_children:
                    style = "named" if ports.named_children[0].type.startswith("named") else "ordered"
                binding = {"kind": "identifier", "name": mod, "receiver": None, "receiverKind": None, "instance": iname,
                           "portStyle": style}
                if params is not None:
                    binding["parameters"] = _cap(self.text(params), binding)
                ctx.add_site("instantiates", _cap(f"{mod} {iname or ''}".strip(), binding), inst, node_kind="hierarchical_instance",
                             conditional=cond, binding=binding)

    def checker(self, node):
        ctx = self.ctx
        cid = next((c for c in node.named_children if c.type == "checker_identifier"), None)
        noi = next((c for c in node.named_children if c.type == "name_of_instance"), None)
        if cid is None:
            return
        name = self.text(self.first_id(cid) or cid)
        inst_id = self.first_id(noi) if noi is not None else None
        iname = self.text(inst_id) if inst_id is not None else ""
        if iname:
            binding = {"kind": "identifier", "name": name, "receiver": None, "receiverKind": None, "instance": iname,
                       "ambiguousSyntax": "checkerInstantiation", "portStyle": "ordered"}
            ctx.add_site("instantiates", _cap(f"{name} {iname}", binding), node, conditional=self.cond > 0, binding=binding)
        else:
            binding = {"kind": "identifier", "name": name, "receiver": None, "receiverKind": None,
                       "ambiguousSyntax": "taskOrFunctionCall"}
            ctx.add_site("calls", _cap(name, binding), node, conditional=self.cond > 0, binding=binding)

    def enter(self, node):
        if not node.is_named:
            return True
        ctx = self.ctx
        t = node.type
        if t in VL_COND:
            self.mark_cond(node)
        if t in ("id_directive", "zero_directive"):
            text = self.text(node).lstrip("`").strip()
            if text.startswith(("ifdef", "ifndef")):
                self.mark_pp()
            elif text.startswith("endif"):
                self.close_pp()
            return True
        if t == "include_compiler_directive":
            lit_node = next((c for c in node.named_children if c.type in ("double_quoted_string", "angle_bracket_literal")), None)
            txt = self.text(lit_node) if lit_node is not None else None
            system = bool(txt) and txt.startswith("<")
            spec = txt.strip('"<>') if txt else None
            b = _import_binding("include", spec, loader="`include", system=system)
            ctx.add_site("includes", _cap(self.text(node), b), node, conditional=self.cond > 0, dynamic=spec is None, binding=b)
            return True
        if t == "text_macro_definition":
            nm = self.first_id(node)
            if nm is not None:
                ctx.add_symbol("macro", self.text(nm), node, bindings={"preprocessor": True, "body": True})
            return True
        if t in ("text_macro_usage", "simple_text_macro_usage"):
            nm = self.first_id(node)
            if nm is not None:
                name = self.text(nm)
                b = {"kind": "macro", "name": name, "receiver": None, "receiverKind": None, "preprocessor": True}
                ctx.add_site("macros", "`" + name, node, conditional=self.cond > 0, binding=b)
            return True
        if t in ("module_declaration", "interface_declaration", "program_declaration", "package_declaration", "class_declaration"):
            kind = {"module_declaration": "namespace", "interface_declaration": "interface", "program_declaration": "namespace",
                    "package_declaration": "namespace", "class_declaration": "class"}[t]
            if t == "module_declaration":
                hdr = next((c for c in node.named_children if c.type == "module_header"), None)
                n = self.first_id(hdr) if hdr is not None else None
                name = self.text(n) if n is not None else ""
            elif t == "class_declaration":
                name, n = self.name_under(node, "class_identifier")
            elif t == "package_declaration":
                name, n = self.name_under(node, "package_identifier")
            elif t == "interface_declaration":
                name, n = None, None
                for c in node.named_children:
                    if c.type in ("interface_ansi_header", "interface_nonansi_header"):
                        name, n = self.name_under(c, "interface_identifier")
                name = name or ""
            else:
                name, n = self.name_under(node, "program_identifier")
                name = name or ""
            bindings = {"declarationKind": t.split("_")[0], "fqn": name}
            outer = ctx.owner
            sym = ctx.add_symbol(kind, name or "", node, bindings=bindings)
            s, e = self.scope_range()
            self.declare(name, "class" if kind == "class" else "namespace", s, e, sym=sym, node=n or node, mutable=False)
            self.push(node, sym, name, kind)
            if t == "class_declaration":
                ext = next((c for c in node.named_children if c.type == "class_type"), None)
                if ext is not None:
                    sup = self.first_id(ext)
                    if sup is not None:
                        sym["bindings"]["extends"] = [self.text(sup)]
                        ctx.add_site("references", self.text(sup), ext, caller=outer, binding={
                            "kind": "identifier", "name": self.text(sup), "receiver": None, "receiverKind": None, "role": "extends"})
            return True
        if t in ("function_declaration", "task_declaration"):
            body = next((c for c in node.named_children if c.type in ("function_body_declaration", "task_body_declaration")), None)
            idn = None
            if body is not None:
                idc = next((c for c in body.named_children if c.type in ("function_identifier", "task_identifier")), None)
                idn = self.first_id(idc) if idc is not None else None
            name = self.text(idn) if idn is not None else ""
            sc = ctx.current_scope
            kind = "function"
            if sc is not None and sc.kind == "class":
                kind = "constructor" if name == "new" else "method"
            sym = ctx.add_symbol(kind, name, node, bindings={"task": t == "task_declaration", "body": True})
            s, e = self.scope_range()
            if kind == "function":
                self.declare(name, "function", s, e, sym=sym, node=idn or node, mutable=False)
            self.push(node, sym, name, kind)
            if body is not None:
                for pnode in body.named_children:
                    if pnode.type == "tf_port_list":
                        for item in pnode.named_children:
                            pid = next((c for c in item.named_children if c.type == "port_identifier"), None)
                            if pid is not None and self.first_id(pid) is not None:
                                self.declare(self.text(self.first_id(pid)), "parameter", node.start_byte, node.end_byte, node=pid)
            return True
        if t == "class_constructor_declaration":
            sym = ctx.add_symbol("constructor", "new", node, bindings={"body": True})
            self.push(node, sym, "new", "constructor")
            return True
        if t == "package_import_declaration":
            for item in node.named_children:
                if item.type != "package_import_item":
                    continue
                pid = next((c for c in item.named_children if c.type == "package_identifier"), None)
                pkg = self.text(self.first_id(pid)) if pid is not None and self.first_id(pid) is not None else None
                rest = [c for c in item.named_children if c.type == "simple_identifier"]
                imported = self.text(rest[0]) if rest else None
                b = _import_binding("import", pkg, imported=imported, namespace=imported is None, local=imported, loader="import")
                site = ctx.add_site("imports", _cap(self.text(item), b), item, conditional=self.cond > 0, dynamic=pkg is None, binding=b)
                if imported:
                    s, e = self.scope_range()
                    self.declare(imported, "import", s, e, site=site, node=item)
            return False
        if t == "module_instantiation":
            self.instantiation(node)
            return True
        if t == "checker_instantiation":
            self.checker(node)
            return True
        if t == "tf_call":
            ids = [c for c in node.named_children if c.type == "simple_identifier"]
            if ids:
                name = self.text(ids[0])
                b = {"kind": "identifier", "name": name, "receiver": None, "receiverKind": None}
                ctx.add_site("calls", name, node, conditional=self.cond > 0, binding=b)
            return True
        if t == "system_tf_call":
            ident = next((c for c in node.named_children if c.type == "system_tf_identifier"), None)
            if ident is not None:
                name = self.text(ident)
                b = {"kind": "identifier", "name": name, "receiver": None, "receiverKind": None, "systemTask": True}
                ctx.add_site("calls", name, node, conditional=self.cond > 0, resolution="external", binding=b)
            return True
        if t == "let_expression":
            scope = next((c for c in node.named_children if c.type in ("package_scope", "class_scope")), None)
            ids = [c for c in node.named_children if c.type == "simple_identifier"]
            if scope is not None and ids:
                sid = self.first_id(scope)
                recv = self.text(sid) if sid is not None else None
                name = self.text(ids[-1])
                b = {"kind": "path", "name": name, "receiver": recv, "receiverKind": "path", "scope": "package" if scope.type == "package_scope" else "class"}
                ctx.add_site("calls", f"{recv}::{name}", node, conditional=self.cond > 0, binding=b)
            return True
        return True


def extract(language: str, source: bytes, ctx: "common.Context") -> dict:
    tree = ctx.parse()
    root = tree.root_node
    module = ctx.module_symbol(root)
    walker = {"zig": _Zig, "elixir": _Elixir, "julia": _Julia, "fortran": _Fortran, "verilog": _Verilog}[language](ctx)
    walker.module_id = module["id"]
    ctx.traverse(root, walker.enter, walker.leave)
    ctx.pop_scope()
    if language == "fortran" and walker.saw_preproc:
        ctx.diag("Fortran preprocessing is not expanded; preprocessor regions are parsed as written and flagged conditional")
    if language == "verilog" and ctx.syntax_errors:
        ctx.diag("Verilog grammar: some procedural task/function calls and positional instantiations parse as ERROR regions; facts inside them are not recorded")
    return ctx.result()
