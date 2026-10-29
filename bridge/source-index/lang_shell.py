"""Bash and PowerShell syntax recipes (pure Tree-sitter; never executes repository code).

Shells resolve commands at run time (functions, aliases, builtins, ``PATH``, modules), so
every command word is a recorded ``calls`` site that stays unresolved; dynamic command
names (``$cmd``, ``& $x``, ``eval``, ``Invoke-Expression``) are persisted as dynamic.
Only literal ``source``/``.`` (Bash) and dot-source/``Import-Module``/``using`` (PowerShell)
specifiers are import facts; anything computed is a dynamic import site with a null
specifier.  Function declarations are mutable (redefinable) and never name-promoted.
"""
from __future__ import annotations

import re

import common

PROFILES = {
    "bash": {
        "extensions": [".sh", ".bash"],
        "grammar": "tree_sitter_bash",
        "capsule": "language",
        "node_kinds": [
            "program", "function_definition", "command", "command_name", "word", "string", "raw_string",
            "list", "if_statement", "case_statement", "while_statement", "for_statement", "command_substitution",
        ],
    },
    "powershell": {
        "extensions": [".ps1", ".psm1"],
        "grammar": "tree_sitter_powershell",
        "capsule": "language",
        "node_kinds": [
            "program", "function_statement", "class_statement", "class_method_definition", "enum_statement",
            "command", "command_name", "command_name_expr", "command_elements", "command_invokation_operator",
            "invokation_expression", "script_block_expression", "script_parameter", "assignment_expression",
            "if_statement", "foreach_statement", "try_statement", "type_literal", "member_name",
        ],
    },
}

RAW_CAP = 4096

BASH_BUILTINS = frozenset("""
: . [ [[ alias bg bind break builtin caller cd command compgen complete compopt continue declare dirs disown
echo enable eval exec exit export false fc fg getopts hash help history jobs kill let local logout mapfile
popd printf pushd pwd read readarray readonly return set shift shopt source suspend test times trap true
type typeset ulimit umask unalias unset wait
""".split())
BASH_DYNAMIC = frozenset({"eval"})
BASH_CONDITIONAL = {
    "if_statement", "elif_clause", "else_clause", "case_statement", "case_item", "while_statement",
    "for_statement", "c_style_for_statement",
}

PS_KEYWORDS = frozenset(
    "begin process end dynamicparam clean param return if elseif else switch while do for foreach try catch finally "
    "throw break continue exit trap".split()
)
PS_DYNAMIC = frozenset({"invoke-expression", "iex", "invoke-command", "icm"})
PS_CONDITIONAL = {
    "if_statement", "elseif_clause", "else_clause", "switch_statement", "switch_clause", "while_statement",
    "do_statement", "for_statement", "foreach_statement", "catch_clause", "trap_statement",
}
PS_WRAPPERS = {
    "unary_expression", "array_literal_expression", "range_expression", "format_expression",
    "multiplicative_expression", "additive_expression", "comparison_expression", "bitwise_expression",
    "logical_expression", "pipeline_chain", "pipeline", "parenthesized_expression",
}
PS_SCOPE_PREFIX = re.compile(r"^(global|script|local|private):", re.IGNORECASE)
PS_MODULE_EXTENSION = re.compile(r"\.(?:psm1|psd1|ps1|dll)$", re.IGNORECASE)
PS_SCRIPT_ROOT = "$psscriptroot"


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
        self.cond_nodes: set[int] = set()
        self.cond_children: set[int] = set()
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
        self.cond = 0

    def enter_cond(self, node, conditional_types):
        if node.id in self.cond_children or node.type in conditional_types:
            self.cond += 1
            self.cond_nodes.add(node.id)

    def import_site(self, node, binding, spec, conditional):
        self.ctx.add_site("imports", _cap(self.ctx.text(node), binding), node, conditional=conditional,
                          dynamic=spec is None, binding=binding)

    def leave(self, node):
        if node.id in self.cond_nodes:
            self.cond_nodes.discard(node.id)
            self.cond -= 1
        if node.id in self.pushed:
            self.ctx.pop_scope()
            self.cond = self.pushed.pop(node.id)


# ---------------------------------------------------------------------------
# Bash
# ---------------------------------------------------------------------------

class _Bash(_Base):
    def literal_word(self, node):
        """Literal text of a command argument, else None (expansions / concatenations are dynamic)."""
        ctx = self.ctx
        if node is None:
            return None
        t = node.type
        if t in ("word", "number"):
            return ctx.text(node)
        if t == "raw_string":
            return ctx.text(node)[1:-1]
        if t == "string":
            parts = node.named_children
            if any(c.type != "string_content" for c in parts):
                return None
            if not parts:
                return ""
            return ctx.source[parts[0].start_byte:parts[-1].end_byte].decode("utf-8", "replace")
        return None

    def command(self, node):
        ctx = self.ctx
        name_node = node.child_by_field_name("name")
        if name_node is None:
            return  # bare assignment: not a command
        args = [c for i, c in enumerate(node.children) if node.field_name_for_child(i) == "argument"]
        first = name_node.named_children[0] if name_node.named_children else None
        name = self.literal_word(first) if first is not None else None
        raw = ctx.source[name_node.start_byte:name_node.end_byte].decode("utf-8", "replace")
        cond = self.cond > 0
        if name in ("source", "."):
            spec = self.literal_word(args[0]) if args else None
            binding = _import_binding("source", spec, loader=name, cwdRelative=bool(spec) and not spec.startswith("/"))
            self.import_site(node, binding, spec, cond)
            return
        binding, dynamic = self.command_binding(name)
        ctx.add_site("calls", _cap(raw, binding), node, conditional=cond, dynamic=dynamic, binding=binding)

    @staticmethod
    def command_binding(name):
        binding = {"kind": "identifier", "name": name, "receiver": None, "receiverKind": None, "command": True}
        if name is None:
            binding.update(kind="dynamic", dynamic=True)
            return binding, True
        if name in BASH_BUILTINS:
            binding["builtin"] = True
        if "/" in name:
            binding["pathLike"] = True
        dynamic = name in BASH_DYNAMIC
        if dynamic:
            binding["dynamic"] = True
        return binding, dynamic

    def enter(self, node):
        if not node.is_named:
            return
        t = node.type
        self.enter_cond(node, BASH_CONDITIONAL)
        if t == "list":
            self.mark_after_logical(node)
        elif t == "function_definition":
            self.define_function(node)
        elif t == "command":
            self.command(node)

    def mark_after_logical(self, node):
        after = False
        for c in node.children:
            if not c.is_named:
                after = after or c.type in ("&&", "||")
            elif after:
                self.cond_children.add(c.id)

    def define_function(self, node):
        ctx = self.ctx
        name_node = node.child_by_field_name("name")
        name = ctx.text(name_node)
        sym = ctx.add_symbol("function", name, node, bindings={"body": True, "global": True})
        # Shell functions live in one global namespace once defined: reassignable, never name-promoted.
        ctx.declare(name, "function", node.start_byte, len(ctx.source), symbol_id=sym["id"], mutable=True,
                    position=name_node.start_byte, owner=self.module_id)
        self.push(node, sym, name, "function")


# ---------------------------------------------------------------------------
# PowerShell
# ---------------------------------------------------------------------------

class _Ps(_Base):
    # -- literals ---------------------------------------------------------------
    def string_literal(self, node):
        """(value, quoted_single) for a string_literal; value None when it interpolates."""
        ctx = self.ctx
        text = ctx.text(node)
        if node.type != "string_literal" or len(text) < 2:
            return None, False
        single = text[0] == "'" or text.startswith("@'")
        if single:
            return text[1:-1] if text[0] == "'" else text[2:-2].strip("\r\n"), True
        has_var = any(d.type in ("variable", "sub_expression", "subexpression") for d in self.descendants(node))
        if has_var or "`" in text:
            return None, False
        return text[1:-1], False

    @staticmethod
    def descendants(node):
        out = []
        stack = list(node.children)
        while stack:
            n = stack.pop()
            out.append(n)
            stack.extend(n.children)
        return out

    def classify_path(self, text, interpolating):
        """(specifier|None, relativeTo) for a literal-ish path token."""
        low = text.lower()
        if low.startswith(PS_SCRIPT_ROOT) and len(text) > 13 and text[13] in "\\/":
            return text[14:], "dir"
        if interpolating and ("$" in text or "`" in text):
            return None, None
        return text, None

    def expr_literal(self, node):
        """Literal value of an argument node (generic token / string / wrapped string), else (None,None)."""
        n = node
        while n is not None and n.type in ("array_literal_expression", "unary_expression") and n.named_child_count == 1:
            n = n.named_children[0]
        if n is None:
            return None, None
        if n.type in ("generic_token", "command_name", "path_command_name", "simple_name"):
            return self.token_literal(n)
        if n.type == "string_literal":
            return self.string_arg_literal(n)
        if n.type == "command_name_expr":
            return self.expr_literal(n.named_children[0]) if n.named_children else (None, None)
        return None, None

    def token_literal(self, n):
        text = self.ctx.text(n)
        if n.type == "path_command_name" and any(d.type == "variable" for d in n.named_children) and not text.lower().startswith(PS_SCRIPT_ROOT):
            return None, None
        return self.classify_path(text, True)

    def string_arg_literal(self, n):
        val, single = self.string_literal(n)
        if val is not None:
            return self.classify_path(val, not single)
        text = self.ctx.text(n)
        inner = text[1:-1] if len(text) >= 2 else text
        if inner.lower().startswith(PS_SCRIPT_ROOT):
            return self.classify_path(inner, True)
        return None, None

    def collect_args(self, elements):
        args = []
        pending = None
        if elements is None:
            return args
        for c in elements.named_children:
            if c.type == "command_parameter":
                pending = self.ctx.text(c)
                continue
            if c.type in ("command_argument_sep",):
                continue
            args.append({"param": pending, "node": c})
            pending = None
        return args

    # -- commands -----------------------------------------------------------------
    def command(self, node):
        ctx = self.ctx
        nm = node.child_by_field_name("command_name")
        if nm is None:
            return
        op_node = next((c for c in node.children if c.type == "command_invokation_operator"), None)
        op = ctx.text(op_node) if op_node is not None else None
        elements = node.child_by_field_name("command_elements")
        args = self.collect_args(elements)
        cond = self.cond > 0
        raw_end = elements.start_byte if elements is not None else node.end_byte
        raw = ctx.source[node.start_byte:raw_end].decode("utf-8", "replace").rstrip()
        if op == ".":
            spec, rel = self.expr_literal(nm)
            binding = _import_binding("source", spec, loader="dot-source", relativeTo=rel, pathLike=True)
            self.import_site(node, binding, spec, cond)
            return
        if nm.type == "command_name_expr":
            self.expression_call(node, nm, op, raw, cond)
            return
        name = ctx.text(nm)
        low = name.lower()
        if low in PS_KEYWORDS:
            return
        if low == "using":
            self.using_import(node, args, cond)
        elif low in ("import-module", "ipmo"):
            self.module_import(node, args, cond)
        else:
            self.named_call(node, name, low, op, raw, cond)

    def expression_call(self, node, nm, op, raw, cond):
        lit, rel = self.expr_literal(nm)
        binding = {"kind": "identifier", "name": lit, "receiver": None, "receiverKind": None, "command": True, "callOperator": op or None}
        dynamic = lit is None
        if dynamic:
            binding.update(kind="dynamic", dynamic=True)
        else:
            binding["pathLike"] = True
            if rel:
                binding["relativeTo"] = rel
        self.ctx.add_site("calls", _cap(raw, binding), node, conditional=cond, dynamic=dynamic, binding=binding)

    def using_import(self, node, args, cond):
        kw = args[0] if args else None
        kwtext = self.ctx.text(kw["node"]).lower() if kw is not None else ""
        target = args[1] if len(args) > 1 else None
        spec = self.using_spec(target["node"], kwtext) if target is not None else None
        binding = _import_binding("using", spec, typeKind=kwtext or None, namespace=(kwtext == "namespace"), loader="using")
        self.import_site(node, binding, spec, cond)

    def using_spec(self, target, kwtext):
        if kwtext == "module":
            return self.expr_literal(target)[0]
        if any(d.type == "variable" for d in self.descendants(target)):
            return None
        return self.ctx.text(target)

    def module_import(self, node, args, cond):
        target = next((a for a in args if (a["param"] or "").lower() in ("-name", "-path", "-literalpath", "-fullyqualifiedname")), None)
        if target is None:
            target = next((a for a in args if a["param"] is None), None)
        spec, rel = self.expr_literal(target["node"]) if target is not None else (None, None)
        binding = _import_binding("import", spec, loader="Import-Module", relativeTo=rel,
                                  pathLike=bool(spec and ("/" in spec or "\\" in spec or PS_MODULE_EXTENSION.search(spec))))
        self.import_site(node, binding, spec, cond)

    def named_call(self, node, name, low, op, raw, cond):
        binding = {"kind": "identifier", "name": name, "receiver": None, "receiverKind": None, "command": True,
                   "caseInsensitive": True, "callOperator": op or None}
        dynamic = low in PS_DYNAMIC
        if dynamic:
            binding["dynamic"] = True
        if "\\" in name or "/" in name:
            binding["pathLike"] = True
        self.ctx.add_site("calls", _cap(raw, binding), node, conditional=cond, dynamic=dynamic, binding=binding)

    def invocation(self, node):
        ctx = self.ctx
        kids = node.named_children
        obj = kids[0] if kids else None
        member = next((c for c in kids if c.type == "member_name"), None)
        args = next((c for c in kids if c.type == "argument_list"), None)
        end = args.start_byte if args is not None else node.end_byte
        raw = ctx.source[node.start_byte:end].decode("utf-8", "replace").rstrip()
        binding: dict = {"name": None, "receiver": None, "receiverKind": None}
        dynamic = member is None or obj is None or obj is member
        if dynamic:
            binding.update(kind="dynamic", dynamic=True)
        elif obj.type == "type_literal":
            self.static_call_binding(binding, ctx.text(member), ctx.text(obj))
            binding["caseInsensitive"] = True
        else:
            otext = ctx.text(obj)
            rk = "identifier" if obj.type == "variable" else "expression"
            if otext.lower() == "$this":
                rk = "this"
            binding.update(kind="member", name=ctx.text(member), receiver=_cap(otext, binding), receiverKind=rk)
            binding["caseInsensitive"] = True
        ctx.add_site("calls", _cap(raw, binding), node, conditional=self.cond > 0, dynamic=dynamic, binding=binding)

    @staticmethod
    def static_call_binding(binding, mname, otext):
        tname = otext.strip()[1:-1] if otext.startswith("[") and otext.endswith("]") else otext
        if mname.lower() != "new":
            binding.update(kind="path", receiver=tname, receiverKind="path", name=mname)
        else:
            recv = tname.rsplit(".", 1)[0] if "." in tname else None
            binding.update(kind="new", receiver=recv, receiverKind="path" if recv else None,
                           name=tname.rsplit(".", 1)[-1])
        binding["static"] = True

    # -- declarations ------------------------------------------------------------------
    def declare_var(self, name, node, *, sym=None, owner=..., scope=None, mutable=True):
        ctx = self.ctx
        own = ctx.owner if owner is ... else owner
        start, end = scope if scope is not None else self.scope_range()
        seen = self.names.setdefault(own, set())
        kind = "assignment" if name.lower() in seen else "variable"
        seen.add(name.lower())
        ctx.declare(name, kind, start, end, symbol_id=sym["id"] if sym else None, mutable=mutable,
                    position=node.start_byte, owner=own)

    def declare_param(self, var_node):
        ctx = self.ctx
        name = ctx.text(var_node)
        self.names.setdefault(ctx.owner, set()).add(name.lower())
        start, end = self.scope_range()
        ctx.declare(name, "parameter", start, end, mutable=True, position=var_node.start_byte)

    def first_variable(self, node):
        """The lone variable of a (wrapped) assignment target."""
        n = node
        while n is not None and n.type != "variable":
            if n.named_child_count != 1:
                return None
            n = n.named_children[0]
        return n

    def assigned_name_of_scriptblock(self, node):
        p = node.parent
        while p is not None and p.type in PS_WRAPPERS:
            p = p.parent
        if p is not None and p.type == "assignment_expression" and p.child_by_field_name("value") is not None:
            val = p.child_by_field_name("value")
            if val.start_byte <= node.start_byte and node.end_byte <= val.end_byte:
                left = next((c for c in p.named_children if c.type == "left_assignment_expression"), None)
                var = self.first_variable(left) if left is not None else None
                return var
        return None

    # -- traversal ---------------------------------------------------------------------
    def enter(self, node):
        if not node.is_named:
            return
        self.enter_cond(node, PS_CONDITIONAL)
        handler = self.ENTER_HANDLERS.get(node.type)
        if handler is not None:
            handler(self, node)

    def enter_function(self, node):
        ctx = self.ctx
        fn = next((c for c in node.named_children if c.type == "function_name"), None)
        full = ctx.text(fn) if fn is not None else ""
        m = PS_SCOPE_PREFIX.match(full)
        name = full[m.end():] if m else full
        bindings = {"body": True, "caseInsensitive": True}
        if m:
            bindings["scope"] = m.group(1).lower()
        sym = ctx.add_symbol("function", name, node, bindings=bindings)
        _, end = self.scope_range()
        ctx.declare(name, "function", node.start_byte, end, symbol_id=sym["id"], mutable=True, position=node.start_byte)
        self.push(node, sym, name, "function")

    def enter_class(self, node):
        ctx = self.ctx
        names = [c for c in node.named_children if c.type == "simple_name"]
        name = ctx.text(names[0]) if names else ""
        bases = [ctx.text(n) for n in names[1:]]
        bindings = {"caseInsensitive": True}
        outer = ctx.owner
        if bases:
            bindings["extends"] = bases
        sym = ctx.add_symbol("class", name, node, bindings=bindings)
        ctx.declare(name, "class", *self.scope_range(), symbol_id=sym["id"], mutable=False, position=node.start_byte)
        self.push(node, sym, name, "class")
        for n in names[1:]:
            text = ctx.text(n)
            ctx.add_site("references", text, n, caller=outer,
                         binding={"kind": "identifier", "name": text, "receiver": None, "receiverKind": None, "role": "extends"})

    def enter_enum(self, node):
        ctx = self.ctx
        names = [c for c in node.named_children if c.type == "simple_name"]
        name = ctx.text(names[0]) if names else ""
        sym = ctx.add_symbol("enum", name, node, bindings={})
        ctx.declare(name, "enum", *self.scope_range(), symbol_id=sym["id"], mutable=False, position=node.start_byte)
        self.push(node, sym, name, "enum")

    def enter_method(self, node):
        ctx = self.ctx
        direct = [c for c in node.named_children if c.type == "simple_name"]
        name = ctx.text(direct[0]) if direct else ""
        attrs = [ctx.text(c).lower() for c in node.named_children if c.type == "class_attribute"]
        has_ret = any(c.type == "type_literal" for c in node.named_children)
        owner_name = ctx.current_scope.name if ctx.current_scope is not None else None
        kind = "constructor" if (not has_ret and owner_name is not None and name.lower() == owner_name.lower()) else "method"
        sym = ctx.add_symbol(kind, name, node, bindings={"static": "static" in attrs, "hidden": "hidden" in attrs, "caseInsensitive": True})
        self.push(node, sym, name, kind)

    def enter_scriptblock(self, node):
        ctx = self.ctx
        var = self.assigned_name_of_scriptblock(node)
        outer = ctx.owner
        outer_range = self.scope_range()
        sym = ctx.add_symbol("lambda", "<scriptblock>", node, bindings={})
        self.push(node, sym, None, "lambda")
        if var is not None:
            sym["bindings"]["assignedTo"] = ctx.text(var)
            self.declare_var(ctx.text(var), var, sym=sym, owner=outer, scope=outer_range)

    def enter_parameters(self, node):
        for c in node.children:
            if c.type == "variable":
                self.declare_param(c)

    def enter_assignment(self, node):
        left = next((c for c in node.named_children if c.type == "left_assignment_expression"), None)
        var = self.first_variable(left) if left is not None else None
        if var is not None and not self.binds_scriptblock(node.child_by_field_name("value")):
            self.declare_var(self.ctx.text(var), var)

    def binds_scriptblock(self, value):
        """True when a script block inside ``value`` is itself named by this assignment."""
        if value is None:
            return False
        blocks = [d for d in self.descendants(value) if d.type == "script_block_expression"]
        return bool(blocks) and self.assigned_name_of_scriptblock(blocks[0]) is not None

    def enter_foreach(self, node):
        var = next((c for c in node.named_children if c.type == "variable"), None)
        if var is not None:
            self.declare_var(self.ctx.text(var), var, scope=(node.start_byte, node.end_byte))

    ENTER_HANDLERS = {
        "function_statement": enter_function,
        "class_statement": enter_class,
        "enum_statement": enter_enum,
        "class_method_definition": enter_method,
        "script_block_expression": enter_scriptblock,
        "script_parameter": enter_parameters,
        "class_method_parameter": enter_parameters,
        "assignment_expression": enter_assignment,
        "foreach_statement": enter_foreach,
        "command": command,
        "invokation_expression": invocation,
    }


def extract(language: str, _source: bytes, ctx: "common.Context") -> dict:
    tree = ctx.parse()
    root = tree.root_node
    module = ctx.module_symbol(root)
    walker = _Bash(ctx) if language == "bash" else _Ps(ctx)
    walker.module_id = module["id"]
    ctx.traverse(root, walker.enter, walker.leave)
    ctx.pop_scope()
    return ctx.result()
