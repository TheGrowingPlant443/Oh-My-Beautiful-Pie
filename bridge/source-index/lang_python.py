"""Python extractor (pure Tree-sitter syntax recipes; no Python code is imported or executed)."""
from __future__ import annotations

from typing import Any

import common

PROFILES: dict[str, dict] = {
    "python": {
        "extensions": [".py"],
        "grammar": "tree_sitter_python",
        "capsule": "language",
        "node_kinds": [
            "function_definition", "lambda", "class_definition", "decorated_definition", "decorator", "call",
            "import_statement", "import_from_statement", "future_import_statement", "aliased_import",
            "relative_import", "wildcard_import", "assignment", "global_statement", "nonlocal_statement",
            "for_in_clause", "list_comprehension", "as_pattern", "except_clause", "if_statement",
        ],
    }
}

_LAMBDA_PARAMS = "lambda_parameters"
_COMPREHENSIONS = {"list_comprehension", "set_comprehension", "dictionary_comprehension", "generator_expression"}
_PROPERTY_SUFFIXES = (".setter", ".getter", ".deleter")


def extract(language: str, source: bytes, ctx: common.Context) -> dict:
    tree = ctx.parse(language)
    ctx.set_region("main", language)
    rng = (0, len(source))
    ctx.module_symbol(rng, node_kind="module")
    walker = _Walker(ctx, rng)
    try:
        ctx.traverse(tree.root_node, walker.enter, walker.leave)
    finally:
        ctx.pop_scope()
    walker.finish()
    return ctx.result()


class _Walker:
    def __init__(self, ctx: common.Context, module_range: tuple[int, int]):
        self.ctx = ctx
        self.module_range = module_range
        self.module_id = ctx.owner
        self.funcs: list[tuple[int, int]] = [module_range]
        self.undo: dict[int, list] = {}
        self.push_on_enter: dict[int, tuple] = {}
        self.pending: dict[int, dict] = {}
        self.cond_ids: set[int] = set()
        self.cond = 0
        self.type_only_ids: set[int] = set()
        self.type_only = 0
        self.handled: set[int] = set()
        self.scope_info: list[dict] = []  # per function scope: globals/nonlocals/first param
        self.all_names: list[str] | None = None
        self.dispatch = {
            "function_definition": self._function,
            "lambda": self._lambda,
            "class_definition": self._class,
            "decorator": self._decorator,
            "import_statement": self._import,
            "import_from_statement": self._import_from,
            "future_import_statement": self._future_import,
            "call": self._call,
            "assignment": self._assignment,
            "augmented_assignment": self._augmented,
            "named_expression": self._walrus,
            "for_statement": self._for,
            "for_in_clause": self._for_in_clause,
            "as_pattern": self._as_pattern,
            "global_statement": self._global,
            "nonlocal_statement": self._global,
            "if_statement": self._if,
        }

    # -- helpers ---------------------------------------------------------------
    def text(self, node) -> str:
        return self.ctx.text(node)

    def _on_leave(self, node, fn) -> None:
        self.undo.setdefault(node.id, []).append(fn)

    def _mark(self, child) -> None:
        if child is not None:
            self.cond_ids.add(child.id)

    def string_value(self, node) -> str | None:
        if node is None or node.type != "string":
            return None
        start = end = None
        prefix = ""
        for ch in node.children:
            if ch.type == "interpolation":
                return None
            if ch.type == "string_start":
                start = ch.end_byte
                prefix = self.text(ch)
            elif ch.type == "string_end":
                end = ch.start_byte
        if start is None or end is None:
            return None
        if any(c in prefix for c in "fF"):
            return None
        return self.ctx.source[start:end].decode("utf-8", "replace")

    def pattern_names(self, node) -> list[tuple[str, Any]]:
        out: list[tuple[str, Any]] = []
        if node is None:
            return out
        stack = [node]
        while stack:
            n = stack.pop()
            t = n.type
            if t == "identifier":
                out.append((self.text(n), n))
            elif t in ("pattern_list", "tuple_pattern", "list_pattern", "tuple", "list", "list_splat_pattern",
                       "parenthesized_pattern", "as_pattern_target", "list_splat", "parenthesized_expression", "dictionary_splat_pattern"):
                stack.extend(reversed(n.named_children))
            # attribute / subscript targets bind no local name
        out.sort(key=lambda p: p[1].start_byte)
        return out

    def param_names(self, params) -> list[tuple[str, Any]]:
        out: list[tuple[str, Any]] = []
        if params is None:
            return out
        for ch in params.named_children:
            t = ch.type
            if t == "identifier":
                out.append((self.text(ch), ch))
            elif t in ("default_parameter", "typed_default_parameter"):
                nm = ch.child_by_field_name("name")
                out.extend(self.pattern_names(nm))
            elif t == "typed_parameter":
                for sub in ch.named_children:
                    if sub.type in ("identifier", "list_splat_pattern", "dictionary_splat_pattern"):
                        out.extend(self.pattern_names(sub))
                        break
            elif t == "list_splat_pattern":
                out.extend(self.pattern_names(ch))
            elif t == "dictionary_splat_pattern":
                if ch.named_children:
                    out.extend(self.pattern_names(ch.named_children[0]))
            elif t == "tuple_pattern":
                out.extend(self.pattern_names(ch))
        return out

    def in_class_body(self) -> bool:
        scope = self.ctx.current_scope
        return scope is not None and scope.kind == "class"

    def declare(self, name: str, kind: str, *, symbol_id: str | None = None, site_id: str | None = None, mutable: bool = True, position: int | None = None, rng: tuple[int, int] | None = None) -> None:
        """Declare a name in the current function/module scope (class-body names are never lexically visible to nested scopes)."""
        if self.in_class_body():
            return
        info = self.scope_info[-1] if self.scope_info else None
        if info is not None and name in info["nonlocal"] | info["global"] and kind in ("variable", "import", "function", "class"):
            kind = "assignment"
            symbol_id = None
        r = rng or self.funcs[-1]
        self.ctx.declare(name, kind, r[0], r[1], symbol_id=symbol_id, site_id=site_id, mutable=mutable, position=position)

    # -- traversal ------------------------------------------------------------------
    def enter(self, node):
        nid = node.id
        if nid in self.cond_ids:
            self.cond += 1
            self._on_leave(node, self._dec_cond)
            self.cond_ids.discard(nid)
        if nid in self.type_only_ids:
            self.type_only += 1
            self._on_leave(node, self._dec_type)
            self.type_only_ids.discard(nid)
        push = self.push_on_enter.pop(nid, None)
        if push is not None:
            sid, label, kind, rng, info = push
            self.ctx.push_scope(sid, label, kind)
            self._on_leave(node, self.ctx.pop_scope)
            if kind != "class":
                self.funcs.append(rng)
                self._on_leave(node, self.funcs.pop)
                self.scope_info.append(info)
                self._on_leave(node, self.scope_info.pop)
                saved = self.cond
                self.cond = 0
                self._on_leave(node, lambda s=saved: setattr(self, "cond", s))
            else:
                self.scope_info.append(info)
                self._on_leave(node, self.scope_info.pop)
        t = node.type
        self._mark_conditional(node, t)
        if not node.is_named:
            return None
        handler = self.dispatch.get(t)
        if handler is not None:
            return handler(node)
        if t in _COMPREHENSIONS:
            self._mark(node.child_by_field_name("body"))
            self._comprehension_scope(node)
        elif t == "conditional_expression":
            kids = node.named_children
            if len(kids) == 3:
                self._mark(kids[0])
                self._mark(kids[2])
        elif t == "boolean_operator":
            self._mark(node.child_by_field_name("right"))
        return None

    def _dec_cond(self) -> None:
        self.cond -= 1

    def _dec_type(self) -> None:
        self.type_only -= 1

    def leave(self, node) -> None:
        fns = self.undo.pop(node.id, None)
        if fns:
            for fn in reversed(fns):
                fn()

    def _mark_conditional(self, node, t: str) -> None:
        if t == "if_statement":
            self._mark(node.child_by_field_name("consequence"))
            self._mark(node.child_by_field_name("alternative"))
            for ch in node.children:
                if ch.type in ("elif_clause", "else_clause"):
                    self._mark(ch)
        elif t in ("while_statement", "for_statement"):
            self._mark(node.child_by_field_name("body"))
        elif t in ("except_clause", "except_group_clause", "case_clause"):
            for ch in node.named_children:
                self._mark(ch)
        elif t == "try_statement":
            for ch in node.children:
                if ch.type == "else_clause":
                    self._mark(ch)
        elif t == "match_statement":
            pass

    def finish(self) -> None:
        if self.all_names is not None and self.module_id is not None:
            sym = self.ctx.symbol_by_id(self.module_id)
            if sym is not None:
                sym.setdefault("bindings", {})["all"] = list(self.all_names)

    # -- control flow helpers --------------------------------------------------------
    def _if(self, node):
        cond = node.child_by_field_name("condition")
        if cond is not None:
            txt = self.text(cond).strip()
            if txt == "TYPE_CHECKING" or txt.endswith(".TYPE_CHECKING"):
                cons = node.child_by_field_name("consequence")
                if cons is not None:
                    self.type_only_ids.add(cons.id)
        return None

    def _comprehension_scope(self, node) -> None:
        rng = (node.start_byte, node.end_byte)
        for ch in node.children:
            if ch.type == "for_in_clause":
                left = ch.child_by_field_name("left")
                for name, n in self.pattern_names(left):
                    self.ctx.declare(name, "variable", rng[0], rng[1], mutable=True, position=n.start_byte)

    def _for_in_clause(self, node):
        return None  # declarations are registered by the enclosing comprehension

    def _for(self, node):
        for name, n in self.pattern_names(node.child_by_field_name("left")):
            self.declare(name, "variable", position=n.start_byte)
        return None

    def _as_pattern(self, node):
        alias = node.child_by_field_name("alias")
        if alias is None:
            return None
        kind = "catch" if node.parent is not None and node.parent.type in ("except_clause", "except_group_clause") else "variable"
        for name, n in self.pattern_names(alias):
            self.declare(name, kind if kind == "catch" else "variable", position=n.start_byte)
        return None

    def _global(self, node):
        # Names were collected by the function prescan; record them as rebinding evidence.
        for ch in node.named_children:
            if ch.type == "identifier":
                kind = "global" if node.type == "global_statement" else "assignment"
                if self.scope_info:
                    r = self.funcs[-1]
                    self.ctx.declare(self.text(ch), kind, r[0], r[1], mutable=True, position=ch.start_byte)
        return None

    def _walrus(self, node):
        name = node.child_by_field_name("name")
        if name is not None and name.type == "identifier":
            self.declare(self.text(name), "variable", position=name.start_byte)
            val = node.child_by_field_name("value")
            if val is not None and val.type == "lambda":
                self.pending.setdefault(val.id, {})["name"] = self.text(name)
        return None

    # -- definitions -----------------------------------------------------------------
    def _prescan_scope(self, body) -> dict:
        info = {"global": set(), "nonlocal": set(), "first_param": None, "class_method": False}
        if body is None:
            return info
        stack = [body]
        while stack:
            n = stack.pop()
            t = n.type
            if t == "global_statement" or t == "nonlocal_statement":
                key = "global" if t == "global_statement" else "nonlocal"
                for ch in n.named_children:
                    if ch.type == "identifier":
                        info[key].add(self.text(ch))
                continue
            if t in ("function_definition", "class_definition", "lambda"):
                continue
            stack.extend(n.named_children)
        return info

    def _decorators_of(self, node) -> list[str]:
        parent = node.parent
        if parent is not None and parent.type == "decorated_definition":
            return [self.text(d).lstrip("@").strip() for d in parent.children if d.type == "decorator"]
        return []

    def _function(self, node):
        ctx = self.ctx
        name_node = node.child_by_field_name("name")
        name = self.text(name_node) if name_node is not None else ""
        body = node.child_by_field_name("body")
        params = node.child_by_field_name("parameters")
        owner_scope = ctx.current_scope
        in_class = owner_scope is not None and owner_scope.kind == "class"
        decorators = self._decorators_of(node)
        kind = "method" if in_class else "function"
        if in_class and name in ("__init__", "__new__"):
            kind = "constructor"
        if any(d == "property" or d.endswith("cached_property") or d.endswith(_PROPERTY_SUFFIXES) for d in decorators):
            kind = "accessor"
        if any(d == "overload" or d.endswith(".overload") for d in decorators):
            kind = "signature"
        is_async = any(ch.type == "async" and not ch.is_named for ch in node.children)
        info = self._prescan_scope(body)
        names = self.param_names(params)
        info["first_param"] = names[0][0] if names else None
        info["class_method"] = in_class
        bindings: dict[str, Any] = {"body": kind != "signature", "static": any(d in ("staticmethod",) for d in decorators)}
        if decorators:
            bindings["decorators"] = decorators
        if any(d == "classmethod" for d in decorators):
            bindings["classmethod"] = True
        if is_async:
            bindings["async"] = True
        at_module = owner_scope is not None and owner_scope.kind == "module"
        exported = None
        if at_module:
            exported = not name.startswith("_")
            bindings["exportNames"] = [name]
        sym = ctx.add_symbol(kind, name, node, node_kind=node.type, bindings=bindings, exported=exported)
        sid = sym["id"]
        if not in_class:
            self.declare(name, "function", symbol_id=sid, mutable=False, position=node.start_byte)
        if at_module:
            ctx.add_export(name, local=name, symbol_id=sid)
        rng = (node.start_byte, node.end_byte)
        for pname, pnode in names:
            ctx.declare(pname, "parameter", rng[0], rng[1], mutable=True, position=pnode.start_byte, owner=sid)
        if body is not None:
            self.push_on_enter[body.id] = (sid, name, kind, rng, info)
        return None

    def _lambda(self, node):
        ctx = self.ctx
        pend = self.pending.pop(node.id, None) or {}
        body = node.child_by_field_name("body")
        params = node.child_by_field_name("parameters")
        row, col = node.start_point
        name = pend.get("name") or ""
        scope_label = name or f"<anonymous@{row + 1}:{col}>"
        sym = ctx.add_symbol(
            "function" if name else "lambda",
            name,
            node,
            node_kind="lambda",
            qualified=ctx.qualified(scope_label),
            bindings={"body": True},
        )
        sid = sym["id"]
        iife = pend.get("iife")
        if iife is not None:
            iife["targetId"] = sid
            iife["resolution"] = "resolvedStatic"
        decl = pend.get("declare")
        if decl is not None:
            self.declare(decl["name"], "variable", symbol_id=sid, mutable=True, position=decl["pos"])
        rng = (node.start_byte, node.end_byte)
        names = self.param_names(params)
        for pname, pnode in names:
            ctx.declare(pname, "parameter", rng[0], rng[1], mutable=True, position=pnode.start_byte, owner=sid)
        info = {"global": set(), "nonlocal": set(), "first_param": None, "class_method": False}
        if body is not None:
            self.push_on_enter[body.id] = (sid, scope_label, "function", rng, info)
        return None

    def _class(self, node):
        ctx = self.ctx
        name_node = node.child_by_field_name("name")
        name = self.text(name_node) if name_node is not None else ""
        supers = node.child_by_field_name("superclasses")
        extends: list[str] = []
        super_nodes = []
        if supers is not None:
            for ch in supers.named_children:
                if ch.type in ("keyword_argument", "comment"):
                    continue
                extends.append(self.text(ch))
                super_nodes.append(ch)
        decorators = self._decorators_of(node)
        owner_scope = ctx.current_scope
        at_module = owner_scope is not None and owner_scope.kind == "module"
        bindings: dict[str, Any] = {"extends": extends, "implements": []}
        if decorators:
            bindings["decorators"] = decorators
        exported = None
        if at_module:
            exported = not name.startswith("_")
            bindings["exportNames"] = [name]
        sym = ctx.add_symbol("class", name, node, node_kind=node.type, bindings=bindings, exported=exported)
        sid = sym["id"]
        if not self.in_class_body():
            self.declare(name, "class", symbol_id=sid, mutable=False, position=node.start_byte)
        if at_module:
            ctx.add_export(name, local=name, symbol_id=sid)
        for sn in super_nodes:
            self._reference(sn, "extends")
        body = node.child_by_field_name("body")
        if body is not None:
            info = {"global": set(), "nonlocal": set(), "first_param": None, "class_method": False}
            self.push_on_enter[body.id] = (sid, name, "class", (node.start_byte, node.end_byte), info)
        return None

    def _reference(self, node, role: str) -> None:
        name, receiver, rkind, kind = self._callee_parts(node)
        binding = {"kind": kind, "name": name, "receiver": receiver, "receiverKind": rkind, "role": role}
        self.ctx.add_site("references", self.text(node), node, binding=binding, dynamic=kind == "dynamic")

    def _decorator(self, node):
        kids = node.named_children
        if not kids:
            return None
        inner = kids[0]
        if inner.type == "call":
            return None
        name, receiver, rkind, kind = self._callee_parts(inner)
        binding = {"kind": kind, "name": name, "receiver": receiver, "receiverKind": rkind, "decorator": True}
        self.ctx.add_site("calls", self.text(inner), node, conditional=self.cond > 0, binding=binding, dynamic=kind == "dynamic")
        return None

    # -- assignments ----------------------------------------------------------------
    def _assignment(self, node):
        left = node.child_by_field_name("left")
        right = node.child_by_field_name("right")
        if left is None:
            return None
        if left.type == "identifier":
            name = self.text(left)
            if right is not None and right.type == "lambda":
                self.pending[right.id] = {"name": name, "declare": {"name": name, "pos": left.start_byte}}
            else:
                self.declare(name, "variable", position=left.start_byte)
            if name == "__all__" and self.scope_info == [] and right is not None:
                self._record_all(right, replace=True)
        else:
            for name, n in self.pattern_names(left):
                self.declare(name, "variable", position=n.start_byte)
        return None

    def _record_all(self, right, *, replace: bool) -> None:
        if right.type not in ("list", "tuple"):
            return
        names = []
        for el in right.named_children:
            v = self.string_value(el)
            if v is not None:
                names.append(v)
        if replace or self.all_names is None:
            self.all_names = names
        else:
            self.all_names.extend(names)

    def _augmented(self, node):
        left = node.child_by_field_name("left")
        right = node.child_by_field_name("right")
        if left is not None and left.type == "identifier":
            name = self.text(left)
            if self.scope_info:
                r = self.funcs[-1]
                self.ctx.declare(name, "assignment", r[0], r[1], mutable=True, position=left.start_byte)
            else:
                self.ctx.declare(name, "assignment", self.module_range[0], self.module_range[1], mutable=True, position=left.start_byte)
            if name == "__all__" and not self.scope_info and right is not None:
                self._record_all(right, replace=False)
        return None

    # -- imports ---------------------------------------------------------------------
    def _binding(self, specifier, **kw) -> dict:
        b = {
            "kind": "import",
            "specifier": specifier,
            "imported": None,
            "alias": None,
            "local": None,
            "default": False,
            "namespace": False,
            "reexport": False,
            "typeOnly": self.type_only > 0,
            "typeKind": "type" if self.type_only > 0 else None,
            "sideEffect": False,
        }
        b.update(kw)
        return b

    def _site(self, node, specifier, binding, local: str | None, *, dynamic: bool = False):
        site = self.ctx.add_site(
            "imports",
            specifier if specifier is not None else "",
            node,
            conditional=self.cond > 0,
            type_only=self.type_only > 0,
            dynamic=dynamic,
            binding=binding,
        )
        if local:
            self.declare(local, "import", site_id=site["id"], mutable=True, position=node.start_byte)
        return site

    def _import(self, node):
        for ch in node.children_by_field_name("name"):
            if ch.type == "aliased_import":
                nm = ch.child_by_field_name("name")
                al = ch.child_by_field_name("alias")
                spec = self.text(nm)
                alias = self.text(al) if al is not None else None
                b = self._binding(spec, alias=alias, local=alias or spec.split(".")[0], namespace=True)
                self._site(ch, spec, b, alias)
            elif ch.type == "dotted_name":
                spec = self.text(ch)
                local = spec.split(".")[0]
                b = self._binding(spec, local=local, namespace=True)
                self._site(ch, spec, b, local)
        return False

    def _module_name(self, node) -> str | None:
        mod = node.child_by_field_name("module_name")
        if mod is None:
            return None
        return self.text(mod)

    def _import_from(self, node):
        spec = self._module_name(node)
        names = node.children_by_field_name("name")
        wildcard = next((c for c in node.children if c.type == "wildcard_import"), None)
        if wildcard is not None:
            b = self._binding(spec, namespace=True)
            self._site(wildcard, spec, b, None)
        for ch in names:
            if ch.type == "aliased_import":
                nm = ch.child_by_field_name("name")
                al = ch.child_by_field_name("alias")
                imported = self.text(nm)
                alias = self.text(al) if al is not None else None
                b = self._binding(spec, imported=imported, alias=alias, local=alias or imported, default=imported == "default")
                self._site(ch, spec, b, alias or imported)
            elif ch.type == "dotted_name":
                imported = self.text(ch)
                b = self._binding(spec, imported=imported, local=imported)
                self._site(ch, spec, b, imported)
        return False

    def _future_import(self, node):
        for ch in node.children_by_field_name("name"):
            imported = self.text(ch.child_by_field_name("name")) if ch.type == "aliased_import" else self.text(ch)
            b = self._binding("__future__", imported=imported, local=None)
            self.ctx.add_site("imports", "__future__", ch, conditional=False, binding=b)
        return False

    # -- calls ---------------------------------------------------------------------
    def _receiver_kind(self, obj) -> str | None:
        if obj is None:
            return None
        if obj.type == "identifier":
            text = self.text(obj)
            info = self.scope_info[-1] if self.scope_info else None
            if info is not None and info.get("class_method") and info.get("first_param") == text:
                return "self"
            return "identifier"
        if obj.type == "call":
            fn = obj.child_by_field_name("function")
            if fn is not None and fn.type == "identifier" and self.text(fn) == "super":
                return "super"
            return "expression"
        if obj.type == "attribute":
            cur = obj
            while cur is not None and cur.type == "attribute":
                cur = cur.child_by_field_name("object")
            if cur is not None and cur.type == "identifier":
                return "path"
        return "expression"

    def _callee_parts(self, fn) -> tuple[str | None, str | None, str | None, str]:
        while fn is not None and fn.type == "parenthesized_expression" and fn.named_children:
            fn = fn.named_children[0]
        if fn is None:
            return None, None, None, "dynamic"
        t = fn.type
        if t == "identifier":
            return self.text(fn), None, None, "identifier"
        if t == "attribute":
            obj = fn.child_by_field_name("object")
            attr = fn.child_by_field_name("attribute")
            return (self.text(attr) if attr is not None else None), (self.text(obj) if obj is not None else None), self._receiver_kind(obj), "member"
        if t == "subscript":
            val = fn.child_by_field_name("value")
            return None, (self.text(val) if val is not None else None), self._receiver_kind(val), "dynamic"
        return None, None, None, "dynamic"

    def _call(self, node):
        ctx = self.ctx
        fn = node.child_by_field_name("function")
        args = node.child_by_field_name("arguments")
        inner = fn
        while inner is not None and inner.type == "parenthesized_expression" and inner.named_children:
            inner = inner.named_children[0]
        name, receiver, rkind, kind = self._callee_parts(fn)
        # dynamic import recipes: __import__("x") / importlib.import_module("x")
        if name in ("__import__", "import_module") and (kind == "identifier" or (kind == "member" and receiver in ("importlib",))):
            first = None
            if args is not None:
                for a in args.named_children:
                    if a.type != "keyword_argument":
                        first = a
                        break
            spec = self.string_value(first) if first is not None else None
            relative = spec is not None and spec.startswith(".")
            raw = spec if spec is not None else (self.text(first) if first is not None else "")
            dynamic = spec is None or relative
            b = self._binding(spec if not dynamic else (spec if relative else None), namespace=True)
            b["kind"] = "dynamicImport"
            b["dynamic"] = dynamic
            if relative:
                b["relativeTo"] = "package"
            ctx.add_site("imports", raw, node, conditional=True, dynamic=dynamic, binding=b)
            return None
        binding = {"kind": kind, "name": name, "receiver": receiver, "receiverKind": rkind}
        dynamic = kind == "dynamic"
        iife = None
        if inner is not None and inner.type == "lambda":
            iife = inner
            binding = {"kind": "iife", "name": None, "receiver": None, "receiverKind": None, "iife": True}
            dynamic = False
        elif dynamic:
            binding["dynamic"] = True
        site = ctx.add_site("calls", self.text(fn), node, conditional=self.cond > 0, dynamic=dynamic, binding=binding)
        if iife is not None:
            self.pending.setdefault(iife.id, {})["iife"] = site
        return None
