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
_DYNAMIC = "<dynamic>"


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
_ZIG_IMPORT = "@import"


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
        binding = _import_binding("import", spec, local=local, alias=local, loader=_ZIG_IMPORT,
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
        if name == _ZIG_IMPORT:
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
        fn = node.child_by_field_name("function")
        binding = {"name": None, "receiver": None, "receiverKind": None}
        dynamic = False
        first_arg = next((c for c in node.named_children if fn is None or c.id != fn.id), None)
        end = first_arg.start_byte if first_arg is not None else node.end_byte
        raw = self.slice(node.start_byte, end).rstrip()
        if fn is not None and fn.type == "identifier":
            binding.update(kind="identifier", name=self.text(fn))
        elif fn is not None and fn.type == "field_expression":
            dynamic = self.member_callee(fn, binding)
        else:
            binding.update(kind="dynamic", dynamic=True)
            dynamic = True
        self.ctx.add_site("calls", _cap(raw, binding), node, conditional=self.cond > 0, dynamic=dynamic, binding=binding)

    def member_callee(self, fn, binding):
        """Fill the member-call binding of a field-expression callee; return whether it is dynamic."""
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
            return True
        return False

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
        if node.type in ZIG_CONDITIONAL:
            self.mark_cond(node)
        handler = self.HANDLERS.get(node.type)
        return True if handler is None else handler(self, node)

    def enter_function(self, node):
        ctx = self.ctx
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
        self.declare_parameters(node)

    def declare_parameters(self, node):
        for c in node.children:
            if c.type != "parameters":
                continue
            for p in c.named_children:
                pn = p.child_by_field_name("name")
                if p.type == "parameter" and pn is not None:
                    self.declare(self.text(pn), "parameter", node.start_byte, node.end_byte, node=p)

    def enter_test(self, node):
        label = next((c for c in node.named_children if c.type in ("string", "identifier")), None)
        name = None
        if label is not None:
            name = self.string_value(label) if label.type == "string" else self.text(label)
        name = name or "test"
        sym = self.ctx.add_symbol("function", name, node, bindings={"test": True, "body": True})
        self.push(node, sym, name, "function")

    def enter_container(self, node):
        if node.parent is not None and node.parent.type == "variable_declaration":
            return
        kind = ZIG_CONTAINERS[node.type]
        sym = self.ctx.add_symbol(kind, "<anonymous>", node, bindings={"anonymous": True, "declarationKind": node.type.split("_")[0]})
        self.push(node, sym, None, kind)

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
        if value is not None and value.type == "builtin_function" and self.builtin_name(value) == _ZIG_IMPORT:
            site = self.import_site(value, local=name)
            self.skip_nodes.add(value.id)
            self.declare(name, "import", s, e, site=site, node=ident)
            return
        self.declare(name, "variable", s, e, node=ident, mutable=mutable)

    HANDLERS = {
        "function_declaration": enter_function,
        "test_declaration": enter_test,
        "variable_declaration": variable,
        "builtin_function": builtin,
        "call_expression": call,
        **dict.fromkeys(ZIG_CONTAINERS, enter_container),
    }


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
                if self.is_bound_identifier(n):
                    out.append(n)
            elif n.type != "anonymous_function":
                stack.extend(n.named_children)
        out.sort(key=lambda c: c.start_byte)

    def is_bound_identifier(self, n):
        p = n.parent
        tgt = p.child_by_field_name("target") if p is not None and p.type == "call" else None
        if tgt is not None and tgt.id == n.id:
            return False
        if p is not None and p.type == "unary_operator" and p.children and p.children[0].type == "^":
            return False
        return self.text(n) != "_"

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
            return (name if name is not None else _DYNAMIC, params, len(params), dyn, head, guard)
        if head.type == "identifier":
            return (self.text(head), [], 0, False, head, guard)
        if head.type == "binary_operator":
            op = head.child_by_field_name("operator")
            return (self.text(op) if op is not None else "<operator>", [head.child_by_field_name("left"), head.child_by_field_name("right")], 2, False, head, guard)
        return (self.text(head), [], 0, True, head, guard)

    def definition(self, node, name_kw):
        ctx = self.ctx
        kind, visibility = EX_DEF[name_kw]
        _, args, _ = self.call_parts(node)
        parsed = self.def_head(args)
        if parsed is None:
            ctx.mark_partial(f"{name_kw} without a head")
            return
        name, params, arity, dyn, head, _ = parsed
        if head.type == "call":
            self.skip_nodes.add(head.id)
        owner = ctx.current_scope
        bindings = {"arity": arity, "visibility": visibility, "kind": name_kw, "body": True, "caseInsensitive": False}
        if dyn:
            bindings["dynamicName"] = True
        if name_kw == "defdelegate":
            kw = self.arg_list(args)[1:]
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

    def module_def(self, node, kw):
        ctx = self.ctx
        _, args, _ = self.call_parts(node)
        items = self.arg_list(args)
        first = items[0] if items else None
        name = self.text(first) if first is not None else ""
        bindings: dict = {"declarationKind": kw}
        kind = {"defmodule": "namespace", "defprotocol": "interface", "defimpl": "impl"}[kw]
        if kw == "defimpl":
            bindings["implTrait"] = name
            impl_for = self.impl_for(items)
            if impl_for is not None:
                bindings["implFor"] = impl_for
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

    def impl_for(self, items):
        """Last `for:` target of a defimpl's keyword options, if any."""
        found = None
        for it in items[1:]:
            if it.type != "keywords":
                continue
            for pair in it.named_children:
                k = pair.child_by_field_name("key")
                v = pair.child_by_field_name("value")
                if k is not None and v is not None and self.text(k).strip().rstrip(":") == "for":
                    found = self.text(v)
        return found



    def directive_options(self, items):
        opts = {}
        for it in items[1:]:
            if it.type != "keywords":
                continue
            for pair in it.named_children:
                k = pair.child_by_field_name("key")
                v = pair.child_by_field_name("value")
                if k is not None and v is not None:
                    opts[self.text(k).strip().rstrip(":").strip()] = v
        return opts

    @staticmethod
    def directive_local(kw, spec, alias):
        if alias:
            return alias
        if spec and kw in ("alias", "require"):
            return spec.rsplit(".", 1)[-1]
        return None

    def emit_directive(self, node, info, span, spec, alias=None, namespace=False):
        """Record one import fact; info is (keyword, argument items, keyword options)."""
        kw, items, opts = info
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
        local = self.directive_local(kw, spec, alias)
        binding = _import_binding(kw, spec, alias=alias, namespace=namespace, local=local, **extra)
        site = self.ctx.add_site("imports", _cap(self.text(node), binding), span, conditional=self.cond > 0,
                                 dynamic=spec is None, binding=binding)
        if local and kw in ("alias", "require"):
            s, e = self.scope_range()
            self.declare(local, "import", node.end_byte if kw == "alias" else s, e, site=site, node=node)

    def directive(self, node, kw):
        items = self.arg_list(self.call_parts(node)[1])
        if not items:
            return
        first = items[0]
        opts = self.directive_options(items)
        info = (kw, items, opts)
        if first.type == "alias":
            spec = self.text(first)
            if kw == "alias" and "as" in opts:
                self.emit_directive(node, info, node, spec, alias=self.text(opts["as"]))
            else:
                self.emit_directive(node, info, node, spec, namespace=kw in ("import", "use"))
        elif first.type == "dot" and kw == "alias":
            self.alias_group(node, info, first)
        elif first.type == "atom":
            self.emit_directive(node, info, node, self.text(first), namespace=True)
        else:
            self.emit_directive(node, info, node, None)

    def alias_group(self, node, info, first):
        left = first.child_by_field_name("left")
        right = first.child_by_field_name("right")
        if left is not None and right is not None and right.type == "tuple":
            for m in right.named_children:
                if m.type == "alias":
                    self.emit_directive(node, info, m, f"{self.text(left)}.{self.text(m)}")
        else:
            self.emit_directive(node, info, node, None)

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
        target, args, block = self.call_parts(node)
        if target is None:
            return
        piped = self.piped(node)
        arity = len(self.arg_list(args)) + (1 if piped else 0)
        end = node.end_byte
        if block is not None:
            end = block.start_byte
        if args is not None:
            end = args.start_byte
        raw = self.slice(node.start_byte, end).rstrip()
        binding = {"name": None, "receiver": None, "receiverKind": None, "arity": arity}
        if piped:
            binding["piped"] = True
        relation = "calls"
        if target.type == "identifier":
            name = self.text(target)
            binding.update(kind="identifier", name=name)
            dynamic = name in EX_DYNAMIC
        elif target.type == "dot":
            relation, dynamic = self.dot_call(node, target, args, block, binding)
        else:
            binding.update(kind="dynamic", dynamic=True)
            dynamic = True
        if dynamic:
            binding["dynamic"] = True
        self.ctx.add_site(relation, _cap(raw, binding), node, conditional=self.cond > 0, dynamic=dynamic, binding=binding)

    def dot_call(self, node, target, args, block, binding):
        """Fill a `receiver.name` call binding; return (relation, dynamic)."""
        left = target.child_by_field_name("left")
        right = target.child_by_field_name("right")
        ltext = self.text(left) if left is not None else None
        rk = None
        if left is not None:
            rk = {"alias": "path", "atom": "atom", "identifier": "identifier"}.get(left.type, "expression")
        binding.update(kind="member", receiver=_cap(ltext, binding) if ltext is not None else None, receiverKind=rk)
        if right is None or right.type != "identifier":
            binding.update(kind="dynamic", dynamic=True)
            return "calls", True
        binding["name"] = self.text(right)
        relation = "calls"
        if args is None and block is None:
            relation = self.bare_dot_relation(node, rk, binding)
        return relation, binding["name"] in EX_DYNAMIC

    def bare_dot_relation(self, node, rk, binding):
        """`a.b` without parentheses: a capture, or an ambiguous map access / zero-arity call."""
        if self.is_capture(node):
            binding["capture"] = True
            return "references"
        if rk not in ("path", "atom"):
            binding["ambiguous"] = "mapAccessOrZeroArityCall"
            binding["noParens"] = True
            return "references"
        return "calls"

    # -- traversal ---------------------------------------------------------------------
    def enter(self, node):
        if not node.is_named:
            return True
        handler = self.HANDLERS.get(node.type)
        return True if handler is None else handler(self, node)

    def enter_call(self, node):
        if node.id in self.skip_nodes:
            return
        target, args, _ = self.call_parts(node)
        name = self.target_name(target)
        if name in EX_COND:
            self.mark_cond(node)
        if name in EX_DEF and args is not None:
            self.definition(node, name)
        elif name in ("defmodule", "defprotocol", "defimpl") and args is not None:
            self.module_def(node, name)
        elif name in EX_DIRECTIVES and args is not None:
            self.directive(node, name)
        elif name not in EX_SPECIAL:
            self.call(node)

    def enter_unary(self, node):
        op = node.children[0] if node.children else None
        operand = node.child_by_field_name("operand")
        if op is None or operand is None:
            return None
        if op.type == "@" and operand.type == "call":
            return self.attribute_skip(operand)
        if op.type == "&" and operand.type == "binary_operator":
            self.capture_reference(operand)
        return None

    def attribute_skip(self, operand):
        """Return False when a module attribute's subtree must not be walked (typespecs, docs, behaviours)."""
        tgt, aargs, _ = self.call_parts(operand)
        an = self.target_name(tgt)
        if an == "behaviour" and aargs is not None:
            for a in aargs.named_children:
                txt = self.text(a)
                self.ctx.add_site("references", txt, a, binding={
                    "kind": "path" if "." in txt else "identifier", "name": txt.rsplit(".", 1)[-1],
                    "receiver": txt.rsplit(".", 1)[0] if "." in txt else None,
                    "receiverKind": "path" if "." in txt else None, "role": "behaviour"})
            return False
        if an in EX_SKIP_ATTRS:
            return False
        return None

    def capture_reference(self, operand):
        oper = operand.child_by_field_name("operator")
        left = operand.child_by_field_name("left")
        right = operand.child_by_field_name("right")
        if oper is not None and self.text(oper) == "/" and left is not None and left.type == "identifier":
            arity = self.text(right) if right is not None else None
            self.ctx.add_site("references", self.text(left), left, conditional=self.cond > 0,
                              binding={"kind": "identifier", "name": self.text(left), "receiver": None, "receiverKind": None,
                                       "capture": True, "arity": int(arity) if arity and arity.isdigit() else None})

    def enter_lambda(self, node):
        sym = self.ctx.add_symbol("lambda", "<fn>", node, bindings={})
        self.push(node, sym, None, "lambda")

    def enter_stab(self, node):
        left = node.child_by_field_name("left")
        if left is None:
            return
        ps: list = []
        self.idents(left, ps)
        is_fn = node.parent is not None and node.parent.type == "anonymous_function"
        kind = "parameter" if is_fn else "variable"
        for p in ps:
            self.declare(self.text(p), kind, node.start_byte, node.end_byte, node=p, mutable=False)

    def enter_match(self, node):
        op = node.child_by_field_name("operator")
        left = node.child_by_field_name("left")
        if op is None or self.text(op) != "=" or left is None:
            return
        ps: list = []
        self.idents(left, ps)
        s, e = node.end_byte, self.scope_range()[1]
        for p in ps:
            self.declare(self.text(p), "variable", s, e, node=p, mutable=False)

    HANDLERS = {
        "call": enter_call,
        "unary_operator": enter_unary,
        "anonymous_function": enter_lambda,
        "stab_clause": enter_stab,
        "binary_operator": enter_match,
    }

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
        name, owner_type = self.callee_head(callee)
        return name, n, owner_type

    def callee_head(self, callee):
        """(name, owner_type) of a method head's callee expression."""
        if callee.type == "identifier":
            return self.text(callee), None
        if callee.type == "field_expression":
            return self.text(callee.named_children[-1]), None
        if callee.type == "parametrized_type_expression":
            return self.text(callee.named_children[0]), None
        if callee.type == "parenthesized_expression":
            inner = callee.named_children[0] if callee.named_children else None
            typ = None
            if inner is not None and inner.type == "typed_expression" and len(inner.named_children) > 1:
                typ = self.text(inner.named_children[-1])
            return "<callable>", typ
        return None, None

    def param_node(self, node, wrappers):
        while node is not None and node.type in wrappers:
            if node.type == "assignment":
                self.skip_nodes.add(node.id)  # defaults bind parameters, but their expressions are still traversed
            node = node.named_children[0] if node.named_children else None
        return node

    def params_of(self, call):
        arg_list = next((c for c in call.named_children if c.type == "argument_list"), None)
        out = []
        if arg_list is None:
            return out
        for p in arg_list.named_children:
            q = self.param_node(p, ("assignment", "named_argument", "typed_expression", "splat_expression", "optional_parameter", "typed_parameter"))
            if q is None:
                continue
            if q.type == "identifier":
                out.append(q)
            elif q.type == "argument_list":
                out.extend(self.nested_params(q))
        return out

    def nested_params(self, arg_list):
        out = []
        for r in arg_list.named_children:
            rr = self.param_node(r, ("assignment", "named_argument", "typed_expression"))
            if rr is not None and rr.type == "identifier":
                out.append(rr)
        return out

    def function_symbol(self, node, head):
        ctx = self.ctx
        name, call, owner_type = self.signature_call(head)
        if call is not None:
            self.skip_nodes.add(call.id)
        if name is None:
            name = _DYNAMIC
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
        if name not in ("<callable>", _DYNAMIC):
            # methods extend one generic function: never a unique lexical target
            self.declare(name, "function", s, e, sym=sym, node=node, mutable=True)
        self.push(node, sym, name, "function")
        if call is not None:
            for p in self.params_of(call):
                self.declare(self.text(p), "parameter", node.start_byte, node.end_byte, node=p, mutable=True)

    def dots(self, n):
        i = n.start_byte
        while i > 0 and self.ctx.source[i - 1:i] == b".":
            i -= 1
        return self.slice(i, n.start_byte)

    def path_text(self, n):
        return self.dots(n) + self.text(n)

    def emit_import(self, node, span_node, spec, **fields):
        loader = "using" if node.type == "using_statement" else "import"
        binding = _import_binding(loader, spec, loader=loader, **fields)
        site = self.ctx.add_site("imports", _cap(self.text(span_node), binding), span_node, conditional=self.cond > 0, binding=binding)
        local = fields.get("local")
        if local:
            s, e = self.scope_range()
            self.declare(local, "import", s, e, site=site, node=span_node)

    def import_statements(self, node):
        is_using = node.type == "using_statement"
        for c in node.named_children:
            if c.type in ("identifier", "import_path", "scoped_identifier", "field_expression"):
                spec = self.path_text(c)
                self.emit_import(node, c, spec, namespace=is_using, local=spec.rsplit(".", 1)[-1].lstrip("."))
            elif c.type == "import_alias":
                self.import_alias(node, c)
            elif c.type == "selected_import":
                self.selected_import(node, c)

    def import_alias(self, node, c):
        parts = c.named_children
        if len(parts) >= 2:
            alias = self.text(parts[-1])
            self.emit_import(node, c, self.path_text(parts[0]), alias=alias, local=alias)

    def selected_import(self, node, c):
        parts = c.named_children
        if not parts:
            return
        spec = self.path_text(parts[0])
        for m in parts[1:]:
            if m.type == "import_alias" and len(m.named_children) >= 2:
                alias = self.text(m.named_children[-1])
                self.emit_import(node, m, spec, imported=self.text(m.named_children[0]), alias=alias, local=alias)
            else:
                nm = self.text(m)
                self.emit_import(node, m, spec, imported=nm, local=nm)

    def include_site(self, node, arg_list):
        args = list(arg_list.named_children) if arg_list is not None else []
        lit = self.string_value(args[-1]) if args and (len(args) == 1 or args[-1].type == "string_literal") else None
        b = _import_binding("include", lit, loader="include", relativeTo="dir")
        self.ctx.add_site("includes", _cap(self.text(node), b), node, conditional=self.cond > 0, dynamic=lit is None, binding=b)

    def callee_binding(self, callee, binding):
        """Fill the call binding from the callee expression; return whether the call is dynamic."""
        kind = callee.type if callee is not None else None
        if kind == "identifier":
            name = self.text(callee)
            binding.update(kind="identifier", name=name)
            return name in JL_DYNAMIC
        if kind == "field_expression":
            return self.field_callee(callee, binding)
        if kind == "parametrized_type_expression":
            binding.update(kind="identifier", name=self.text(callee.named_children[0]), parametric=True)
            return False
        binding.update(kind="dynamic", dynamic=True)
        return True

    def field_callee(self, callee, binding):
        val = callee.child_by_field_name("value") or (callee.named_children[0] if callee.named_children else None)
        member = callee.named_children[-1] if callee.named_children else None
        vtext = self.text(val) if val is not None else None
        rk = "expression"
        if val is not None:
            rk = {"identifier": "identifier", "field_expression": "path"}.get(val.type, "expression")
        binding.update(kind="member", name=self.text(member) if member is not None else None, receiver=_cap(vtext, binding) if vtext else None, receiverKind=rk)
        return binding["name"] in JL_DYNAMIC

    def call(self, node):
        kids = node.named_children
        callee = kids[0] if kids else None
        arg_list = next((c for c in kids if c.type == "argument_list"), None)
        if callee is not None and callee.type == "identifier" and self.text(callee) == "include":
            self.include_site(node, arg_list)
            return
        end = arg_list.start_byte if arg_list is not None else node.end_byte
        raw = self.slice(node.start_byte, end).rstrip()
        binding = {"name": None, "receiver": None, "receiverKind": None}
        dynamic = self.callee_binding(callee, binding)
        if node.type == "broadcast_call_expression":
            binding["broadcast"] = True
        if dynamic:
            binding["dynamic"] = True
        self.ctx.add_site("calls", _cap(raw, binding), node, conditional=self.cond > 0, dynamic=dynamic, binding=binding)

    def macro_site(self, node):
        ctx = self.ctx
        head = node.named_children[0] if node.named_children else None
        if head is None:
            return
        mid = head.named_children[-1] if head.type == "field_expression" and head.named_children else head
        if mid.type != "macro_identifier":
            return
        name = self.text(mid).lstrip("@")
        recv = self.text(head.named_children[0]) if head.type == "field_expression" else None
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
        if node.type in JL_COND:
            self.mark_cond(node)
        handler = self.HANDLERS.get(node.type)
        return True if handler is None else handler(self, node)

    def enter_module(self, node):
        ctx = self.ctx
        name_node = node.child_by_field_name("name")
        name = self.text(name_node)
        sc = ctx.current_scope
        full = f"{sc.name}.{name}" if sc is not None and sc.kind == "namespace" and sc.name else name
        sym = ctx.add_symbol("namespace", name, node, bindings={"fqn": full, "declarationKind": "module"}, qualified=full)
        s, e = self.scope_range()
        self.declare(name, "namespace", s, e, sym=sym, node=name_node)
        self.push(node, sym, full, "namespace")

    def enter_function(self, node):
        head = next((c for c in node.named_children if c.type == "signature"), None)
        if head is not None:
            self.function_symbol(node, head)

    def enter_macro(self, node):
        ctx = self.ctx
        head = next((c for c in node.named_children if c.type == "signature"), None)
        name, call, _ = self.signature_call(head) if head is not None else (None, None, None)
        if call is not None:
            self.skip_nodes.add(call.id)
        name = name or _DYNAMIC
        sym = ctx.add_symbol("macro", name, node, bindings={"body": True})
        s, e = self.scope_range()
        self.declare("@" + name, "function", s, e, sym=sym, node=node, mutable=True)
        self.push(node, sym, name, "macro")
        if call is not None:
            for p in self.params_of(call):
                self.declare(self.text(p), "parameter", node.start_byte, node.end_byte, node=p, mutable=True)

    def enter_type(self, node):
        ctx = self.ctx
        t = node.type
        head = next((c for c in node.named_children if c.type == "type_head"), None)
        name = self.type_name(head) if head is not None else ""
        kind = "struct" if t == "struct_definition" else "type"
        bindings = {"declarationKind": t.split("_")[0], "mutable": any(c.type == "mutable" for c in node.children)}
        if head is not None:
            self.record_supertype(head, bindings)
        sym = ctx.add_symbol(kind, name, node, bindings=bindings)
        s, e = self.scope_range()
        self.declare(name, "class" if kind == "struct" else "type", s, e, sym=sym, node=node, mutable=False)
        if t == "struct_definition":
            self.push(node, sym, name, kind)

    def record_supertype(self, head, bindings):
        be = head.named_children[0] if head.named_children else None
        if be is None or be.type != "binary_expression" or len(be.named_children) < 3:
            return
        sup = self.text(be.named_children[-1])
        bindings["extends"] = [sup]
        self.ctx.add_site("references", sup, be.named_children[-1], binding={
            "kind": "identifier", "name": sup.split("{")[0].rsplit(".", 1)[-1], "receiver": None,
            "receiverKind": None, "role": "supertype"})

    def enter_import(self, node):
        self.import_statements(node)
        return False

    @staticmethod
    def is_method_head(left):
        if left.type in ("call_expression", "where_expression"):
            return True
        return left.type == "typed_expression" and bool(left.named_children) and left.named_children[0].type == "call_expression"

    def enter_assignment(self, node):
        if node.id in self.skip_nodes:
            return
        left = node.named_children[0] if node.named_children else None
        right = node.named_children[-1] if node.named_children else None
        if left is None:
            return
        if self.is_method_head(left):
            self.function_symbol(node, left)
        elif left.type == "identifier" and not (right is not None and right.type == "arrow_function_expression"):
            const = node.parent is not None and node.parent.type == "const_statement"
            s, e = node.end_byte, self.scope_range()[1]
            self.declare(self.text(left), "variable", s, e, node=left, mutable=not const)

    @staticmethod
    def arrow_params(node):
        first = node.named_children[0] if node.named_children else None
        ps = []
        stack = [first] if first is not None else []
        while stack:
            n = stack.pop()
            if n.type == "identifier":
                ps.append(n)
            elif n.type in ("tuple_expression", "parenthesized_expression", "typed_expression", "argument_list"):
                stack.extend(n.named_children[:1] if n.type == "typed_expression" else n.named_children)
        return ps

    def enter_arrow(self, node):
        ctx = self.ctx
        parent = node.parent
        outer = ctx.owner
        rng = self.scope_range()
        sym = ctx.add_symbol("lambda", "<arrow>", node, bindings={})
        self.push(node, sym, None, "lambda")
        for p in self.arrow_params(node):
            self.declare(self.text(p), "parameter", node.start_byte, node.end_byte, node=p)
        if parent is not None and parent.type == "assignment" and parent.id not in self.skip_nodes:
            lhs = parent.named_children[0] if parent.named_children else None
            if lhs is not None and lhs.type == "identifier":
                nm = self.text(lhs)
                sym["bindings"]["assignedTo"] = nm
                self.declare(nm, "variable", parent.end_byte, rng[1], sym=sym, node=lhs, mutable=True, owner=outer)

    def enter_do(self, node):
        sym = self.ctx.add_symbol("lambda", "<do>", node, bindings={})
        self.push(node, sym, None, "lambda")
        al = next((c for c in node.named_children if c.type == "argument_list"), None)
        if al is not None:
            for p in al.named_children:
                if p.type == "identifier":
                    self.declare(self.text(p), "parameter", node.start_byte, node.end_byte, node=p)

    def enter_binary(self, node):
        ops = [c for c in node.children if not c.is_named or c.type == "operator"]
        if not any(self.text(c) == "|>" for c in ops):
            return
        right = node.named_children[-1] if node.named_children else None
        if right is not None and right.type in ("identifier", "field_expression"):
            txt = self.text(right)
            self.ctx.add_site("references", txt, right, conditional=self.cond > 0, binding={
                "kind": "identifier" if right.type == "identifier" else "member",
                "name": txt.rsplit(".", 1)[-1], "receiver": txt.rsplit(".", 1)[0] if "." in txt else None,
                "receiverKind": None, "pipeTarget": True})

    def enter_call(self, node):
        if node.id not in self.skip_nodes:
            self.call(node)

    HANDLERS = {
        "module_definition": enter_module,
        "function_definition": enter_function,
        "macro_definition": enter_macro,
        "struct_definition": enter_type,
        "abstract_definition": enter_type,
        "primitive_definition": enter_type,
        "using_statement": enter_import,
        "import_statement": enter_import,
        "assignment": enter_assignment,
        "arrow_function_expression": enter_arrow,
        "do_clause": enter_do,
        "binary_expression": enter_binary,
        "call_expression": enter_call,
        "broadcast_call_expression": enter_call,
        "macrocall_expression": macro_site,
    }


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
            self.declare_unit_parameters(node, stmt, sym)
        return sym

    def declare_unit_parameters(self, node, stmt, sym):
        params = next((c for c in stmt.named_children if c.type == "parameters"), None)
        if params is None:
            return
        for p in params.named_children:
            if p.type == "identifier":
                self.declare(self.text(p), "parameter", node.start_byte, node.end_byte, node=p, mutable=False)
                self.vars[sym["id"]].add(self.text(p).lower())



    def use(self, node):
        ctx = self.ctx
        mn = next((c for c in node.named_children if c.type == "module_name"), None)
        if mn is None:
            return
        mod = self.text(mn)
        intrinsic = any((not c.is_named and c.type == "intrinsic") for c in node.children) or mod.lower() in FT_INTRINSIC_MODULES
        items = next((c for c in node.named_children if c.type in ("included_items",)), None)
        cond = self.cond > 0
        base = {"loader": "use", "caseInsensitive": True, "intrinsic": intrinsic}
        if items is None:
            b = _import_binding("use", mod, namespace=True, **base)
            ctx.add_site("imports", _cap(self.text(node), b), node, conditional=cond, binding=b)
            return
        for it in items.named_children:
            b = self.use_binding(it, mod, base)
            site = ctx.add_site("imports", _cap(self.text(it), b), it, conditional=cond, binding=b)
            s, e = self.scope_range()
            self.declare(b["local"], "import", s, e, site=site, node=it)

    def use_binding(self, it, mod, base):
        if it.type != "use_alias":
            nm = self.text(it)
            return _import_binding("use", mod, imported=nm, local=nm, **base)
        ln = next((c for c in it.named_children if c.type == "local_name"), None)
        rem = [c for c in it.named_children if c.type != "local_name"]
        imported = self.text(rem[0]) if rem else None
        local = self.text(ln) if ln is not None else imported
        return _import_binding("use", mod, imported=imported, alias=local if ln is not None else None, local=local, **base)

    @staticmethod
    def member_parts(node):
        parts = node.named_children
        member = next((c for c in parts if c.type == "type_member"), None)
        recv = next((c for c in parts if c.type != "type_member"), None)
        return member, recv

    def call_like(self, node):
        if node.type == "subroutine_call":
            self.subroutine_call(node)
        else:
            self.function_call(node)

    def subroutine_call(self, node):
        sub = node.child_by_field_name("subroutine")
        args = next((c for c in node.named_children if c.type == "argument_list"), None)
        end = args.start_byte if args is not None else node.end_byte
        raw = self.slice(node.start_byte, end).rstrip()
        binding = {"name": None, "receiver": None, "receiverKind": None, "caseInsensitive": True, "subroutine": True}
        if sub is not None and sub.type == "identifier":
            binding.update(kind="identifier", name=self.text(sub))
        elif sub is not None and sub.type == "derived_type_member_expression":
            member, recv = self.member_parts(sub)
            binding.update(kind="member", name=self.text(member) if member is not None else None,
                           receiver=self.text(recv) if recv is not None else None,
                           receiverKind="identifier" if recv is not None and recv.type == "identifier" else "expression")
        else:
            binding.update(kind="dynamic", dynamic=True)
        self.ctx.add_site("calls", _cap(raw, binding), node, conditional=self.cond > 0, dynamic=binding.get("dynamic", False), binding=binding)

    def function_call(self, node):
        kids = node.named_children
        callee = kids[0] if kids else None
        args = next((c for c in kids if c.type == "argument_list"), None)
        end = args.start_byte if args is not None else node.end_byte
        raw = self.slice(node.start_byte, end).rstrip()
        binding = {"name": None, "receiver": None, "receiverKind": None, "caseInsensitive": True,
                   "ambiguous": "callOrArrayElement"}
        relation = "calls"
        if callee is not None and callee.type == "identifier":
            relation = self.identifier_call(callee, binding)
        elif callee is not None and callee.type == "derived_type_member_expression":
            member, recv = self.member_parts(callee)
            binding.update(kind="member", name=self.text(member) if member is not None else None,
                           receiver=self.text(recv) if recv is not None else None, receiverKind="identifier")
        else:
            binding.update(kind="dynamic", dynamic=True)
        self.ctx.add_site(relation, _cap(raw, binding), node, conditional=self.cond > 0, dynamic=binding.get("dynamic", False), binding=binding)

    def identifier_call(self, callee, binding):
        """Fill an identifier callee: an array element of a declared variable, else a (maybe intrinsic) call."""
        name = self.text(callee)
        binding.update(kind="identifier", name=name)
        if name.lower() in self.vars.get(self.ctx.owner, set()):
            binding.update(kind="index")
            binding.pop("ambiguous", None)
            binding["arrayElement"] = True
            return "references"
        if name.lower() in FT_INTRINSICS:
            binding["intrinsic"] = True
        return "calls"

    def enter(self, node):
        if not node.is_named:
            return True  # keyword tokens share names with node types (`module`, `function`, `subroutine`)
        t = node.type
        if t in FT_PP:
            self.mark_pp(node)
            self.saw_preproc = True
        elif t in FT_COND:
            self.mark_cond(node)
        handler = self.HANDLERS.get(t)
        return True if handler is None else handler(self, node)

    def enter_module(self, node):
        self.enter_unit(node, "namespace", "module_statement", {"declarationKind": "module"})

    def enter_program(self, node):
        self.enter_unit(node, "function", "program_statement", {"program": True})

    def enter_procedure(self, node):
        self.enter_unit(node, "function", node.type + "_statement", {"declarationKind": node.type})

    def bound_procedures(self, node):
        bound = []
        for c in node.named_children:
            if c.type != "derived_type_procedures":
                continue
            for p in c.named_children:
                if p.type == "procedure_statement":
                    bound.extend(self.text(d) for d in p.named_children if d.type == "method_name")
        return bound

    def enter_derived_type(self, node):
        ctx = self.ctx
        stmt = next((c for c in node.named_children if c.type == "derived_type_statement"), None)
        tn = next((c for c in stmt.named_children if c.type == "type_name"), None) if stmt is not None else None
        name = self.text(tn) if tn is not None else ""
        sym = ctx.add_symbol("struct", name, node, bindings={"caseInsensitive": True, "typeBound": self.bound_procedures(node),
                                                              "fqn": (f"{self.module_stack[-1]}::{name}" if self.module_stack else name).lower()})
        s, e = self.scope_range()
        self.declare(name, "class", s, e, sym=sym, node=tn or node, mutable=False)

    def enter_interface(self, node):
        ctx = self.ctx
        stmt = next((c for c in node.named_children if c.type == "interface_statement"), None)
        nm = next((c for c in stmt.named_children if c.type == "name"), None) if stmt is not None else None
        name = self.text(nm) if nm is not None else ""
        sym = ctx.add_symbol("interface", name, node, bindings={"caseInsensitive": True, "generic": bool(name)})
        self.push(node, sym, name, "interface")

    def enter_procedure_statement(self, node):
        scope = self.ctx.current_scope
        if scope is None or scope.kind != "interface":
            return
        for d in node.named_children:
            if d.type == "method_name":
                nm = self.text(d)
                self.ctx.add_site("references", nm, d, binding={"kind": "identifier", "name": nm, "receiver": None, "receiverKind": None,
                                                                "role": "moduleProcedure", "caseInsensitive": True})

    def enter_variable(self, node):
        if node.parent is not None and node.parent.type == "derived_type_definition":
            return False  # components are not lexical names
        sid = self.ctx.owner
        for i, c in enumerate(node.children):
            if node.field_name_for_child(i) == "declarator":
                self.declare_declarator(c, sid)
        return None

    def declare_declarator(self, c, sid):
        ident = c if c.type == "identifier" else next((d for d in c.named_children if d.type == "identifier"), None)
        if ident is None:
            return
        nm = self.text(ident)
        known = self.vars.setdefault(sid, set())
        if nm.lower() not in known:  # otherwise a type declaration of a dummy argument, not a second binding
            known.add(nm.lower())
            s, e = self.scope_range()
            self.declare(nm, "variable", s, e, node=ident, mutable=True)

    def enter_use(self, node):
        self.use(node)
        return False

    def enter_include(self, node):
        path = node.child_by_field_name("path")
        lit = self.text(path).strip("'\"") if path is not None else None
        b = _import_binding("include", lit, loader="include", caseInsensitive=True)
        self.ctx.add_site("includes", _cap(self.text(node), b), node, conditional=self.cond > 0, dynamic=lit is None, binding=b)

    def enter_preproc_include(self, node):
        path = node.child_by_field_name("path")
        text = self.text(path) if path is not None else ""
        system = text.startswith("<")
        lit = text.strip('<>"') if text else None
        b = _import_binding("include", lit, loader="#include", system=system)
        self.ctx.add_site("includes", _cap(self.text(node), b), node, conditional=self.cond > 0, dynamic=lit is None, binding=b)
        self.saw_preproc = True

    HANDLERS = {
        "module": enter_module,
        "program": enter_program,
        "subroutine": enter_procedure,
        "function": enter_procedure,
        "derived_type_definition": enter_derived_type,
        "interface": enter_interface,
        "procedure_statement": enter_procedure_statement,
        "variable_declaration": enter_variable,
        "use_statement": enter_use,
        "include_statement": enter_include,
        "preproc_include": enter_preproc_include,
        "subroutine_call": call_like,
        "call_expression": call_like,
    }

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
        kids = node.named_children
        type_node = next((c for c in kids if c.type in ("simple_identifier", "checker_identifier")), None)
        if type_node is None:
            return
        mod = self.text(type_node) if type_node.type == "simple_identifier" else (self.text(self.first_id(type_node) or type_node))
        params = next((c for c in kids if c.type == "parameter_value_assignment"), None)
        for inst in kids:
            if inst.type == "hierarchical_instance":
                self.hierarchical_instance(inst, mod, params)

    @staticmethod
    def port_style(ports):
        if ports is None or not ports.named_children:
            return None
        return "named" if ports.named_children[0].type.startswith("named") else "ordered"

    def hierarchical_instance(self, inst, mod, params):
        noi = next((c for c in inst.named_children if c.type == "name_of_instance"), None)
        inst_id = self.first_id(noi) if noi is not None else None
        iname = self.text(inst_id) if inst_id is not None else None
        ports = next((c for c in inst.named_children if c.type == "list_of_port_connections"), None)
        binding = {"kind": "identifier", "name": mod, "receiver": None, "receiverKind": None, "instance": iname,
                   "portStyle": self.port_style(ports)}
        if params is not None:
            binding["parameters"] = _cap(self.text(params), binding)
        self.ctx.add_site("instantiates", _cap(f"{mod} {iname or ''}".strip(), binding), inst, node_kind="hierarchical_instance",
                          conditional=self.cond > 0, binding=binding)

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

    DECL_KINDS = {"module_declaration": "namespace", "interface_declaration": "interface", "program_declaration": "namespace",
                  "package_declaration": "namespace", "class_declaration": "class"}

    def enter(self, node):
        if not node.is_named:
            return True
        if node.type in VL_COND:
            self.mark_cond(node)
        handler = self.HANDLERS.get(node.type)
        return True if handler is None else handler(self, node)

    def enter_directive(self, node):
        text = self.text(node).lstrip("`").strip()
        if text.startswith(("ifdef", "ifndef")):
            self.mark_pp()
        elif text.startswith("endif"):
            self.close_pp()

    def enter_include(self, node):
        lit_node = next((c for c in node.named_children if c.type in ("double_quoted_string", "angle_bracket_literal")), None)
        txt = self.text(lit_node) if lit_node is not None else None
        system = bool(txt) and txt.startswith("<")
        spec = txt.strip('"<>') if txt else None
        b = _import_binding("include", spec, loader="`include", system=system)
        self.ctx.add_site("includes", _cap(self.text(node), b), node, conditional=self.cond > 0, dynamic=spec is None, binding=b)

    def enter_macro_definition(self, node):
        nm = self.first_id(node)
        if nm is not None:
            self.ctx.add_symbol("macro", self.text(nm), node, bindings={"preprocessor": True, "body": True})

    def enter_macro_usage(self, node):
        nm = self.first_id(node)
        if nm is not None:
            name = self.text(nm)
            b = {"kind": "macro", "name": name, "receiver": None, "receiverKind": None, "preprocessor": True}
            self.ctx.add_site("macros", "`" + name, node, conditional=self.cond > 0, binding=b)

    def declaration_name(self, node):
        """(name, identifier node) of a module/interface/program/package/class declaration."""
        t = node.type
        if t == "module_declaration":
            hdr = next((c for c in node.named_children if c.type == "module_header"), None)
            n = self.first_id(hdr) if hdr is not None else None
            return (self.text(n) if n is not None else ""), n
        if t == "class_declaration":
            return self.name_under(node, "class_identifier")
        if t == "package_declaration":
            return self.name_under(node, "package_identifier")
        name, n = None, None
        if t == "interface_declaration":
            for c in node.named_children:
                if c.type in ("interface_ansi_header", "interface_nonansi_header"):
                    name, n = self.name_under(c, "interface_identifier")
        else:
            name, n = self.name_under(node, "program_identifier")
        return name or "", n

    def enter_declaration(self, node):
        ctx = self.ctx
        kind = self.DECL_KINDS[node.type]
        name, n = self.declaration_name(node)
        outer = ctx.owner
        sym = ctx.add_symbol(kind, name or "", node, bindings={"declarationKind": node.type.split("_")[0], "fqn": name})
        s, e = self.scope_range()
        self.declare(name, "class" if kind == "class" else "namespace", s, e, sym=sym, node=n or node, mutable=False)
        self.push(node, sym, name, kind)
        if node.type == "class_declaration":
            self.record_extends(node, sym, outer)

    def record_extends(self, node, sym, outer):
        ext = next((c for c in node.named_children if c.type == "class_type"), None)
        sup = self.first_id(ext) if ext is not None else None
        if sup is not None:
            sym["bindings"]["extends"] = [self.text(sup)]
            self.ctx.add_site("references", self.text(sup), ext, caller=outer, binding={
                "kind": "identifier", "name": self.text(sup), "receiver": None, "receiverKind": None, "role": "extends"})

    def callable_identifier(self, body):
        if body is None:
            return None
        idc = next((c for c in body.named_children if c.type in ("function_identifier", "task_identifier")), None)
        return self.first_id(idc) if idc is not None else None

    def enter_callable(self, node):
        ctx = self.ctx
        body = next((c for c in node.named_children if c.type in ("function_body_declaration", "task_body_declaration")), None)
        idn = self.callable_identifier(body)
        name = self.text(idn) if idn is not None else ""
        sc = ctx.current_scope
        kind = "function"
        if sc is not None and sc.kind == "class":
            kind = "constructor" if name == "new" else "method"
        sym = ctx.add_symbol(kind, name, node, bindings={"task": node.type == "task_declaration", "body": True})
        s, e = self.scope_range()
        if kind == "function":
            self.declare(name, "function", s, e, sym=sym, node=idn or node, mutable=False)
        self.push(node, sym, name, kind)
        if body is not None:
            self.declare_tf_ports(node, body)

    def declare_tf_ports(self, node, body):
        for pnode in body.named_children:
            if pnode.type != "tf_port_list":
                continue
            for item in pnode.named_children:
                pid = next((c for c in item.named_children if c.type == "port_identifier"), None)
                ident = self.first_id(pid) if pid is not None else None
                if ident is not None:
                    self.declare(self.text(ident), "parameter", node.start_byte, node.end_byte, node=pid)

    def enter_constructor(self, node):
        sym = self.ctx.add_symbol("constructor", "new", node, bindings={"body": True})
        self.push(node, sym, "new", "constructor")

    def import_item(self, item):
        pid = next((c for c in item.named_children if c.type == "package_identifier"), None)
        pident = self.first_id(pid) if pid is not None else None
        pkg = self.text(pident) if pident is not None else None
        rest = [c for c in item.named_children if c.type == "simple_identifier"]
        imported = self.text(rest[0]) if rest else None
        b = _import_binding("import", pkg, imported=imported, namespace=imported is None, local=imported, loader="import")
        site = self.ctx.add_site("imports", _cap(self.text(item), b), item, conditional=self.cond > 0, dynamic=pkg is None, binding=b)
        if imported:
            s, e = self.scope_range()
            self.declare(imported, "import", s, e, site=site, node=item)

    def enter_package_import(self, node):
        for item in node.named_children:
            if item.type == "package_import_item":
                self.import_item(item)
        return False

    def enter_tf_call(self, node):
        ids = [c for c in node.named_children if c.type == "simple_identifier"]
        if ids:
            name = self.text(ids[0])
            b = {"kind": "identifier", "name": name, "receiver": None, "receiverKind": None}
            self.ctx.add_site("calls", name, node, conditional=self.cond > 0, binding=b)

    def enter_system_tf_call(self, node):
        ident = next((c for c in node.named_children if c.type == "system_tf_identifier"), None)
        if ident is not None:
            name = self.text(ident)
            b = {"kind": "identifier", "name": name, "receiver": None, "receiverKind": None, "systemTask": True}
            self.ctx.add_site("calls", name, node, conditional=self.cond > 0, resolution="external", binding=b)

    def enter_let(self, node):
        scope = next((c for c in node.named_children if c.type in ("package_scope", "class_scope")), None)
        ids = [c for c in node.named_children if c.type == "simple_identifier"]
        if scope is not None and ids:
            sid = self.first_id(scope)
            recv = self.text(sid) if sid is not None else None
            name = self.text(ids[-1])
            b = {"kind": "path", "name": name, "receiver": recv, "receiverKind": "path", "scope": "package" if scope.type == "package_scope" else "class"}
            self.ctx.add_site("calls", f"{recv}::{name}", node, conditional=self.cond > 0, binding=b)

    HANDLERS = {
        "id_directive": enter_directive,
        "zero_directive": enter_directive,
        "include_compiler_directive": enter_include,
        "text_macro_definition": enter_macro_definition,
        "text_macro_usage": enter_macro_usage,
        "simple_text_macro_usage": enter_macro_usage,
        **dict.fromkeys(DECL_KINDS, enter_declaration),
        "function_declaration": enter_callable,
        "task_declaration": enter_callable,
        "class_constructor_declaration": enter_constructor,
        "package_import_declaration": enter_package_import,
        "module_instantiation": instantiation,
        "checker_instantiation": checker,
        "tf_call": enter_tf_call,
        "system_tf_call": enter_system_tf_call,
        "let_expression": enter_let,
    }


def extract(language: str, _source: bytes, ctx: "common.Context") -> dict:
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
