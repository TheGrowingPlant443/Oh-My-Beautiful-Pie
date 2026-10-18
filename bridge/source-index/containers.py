"""Embedded-script extraction for Svelte / Vue / Astro / EJS files.

Only script regions are parsed (with the JS/TS grammars, via Tree-sitter
``included_ranges`` over the *original* buffer so every byte offset, line and
column is in original-file coordinates).  Template markup, styles, directives
and framework behavior are explicitly separate coverage and are never claimed
as resolved here.  The lexer is a small stateful scanner (comments, quoted
attribute values, ``{...}`` mustache expressions with JS strings/templates/
comments, raw-text elements); it is not a regex-only ``<script>`` matcher.
"""
from __future__ import annotations

import re
from typing import Any

import common
import lang_js

_CONTAINER = {
    "extensions": [],
    "grammar": None,
    "capsule": None,
    "requires": ["javascript", "typescript"],
    "node_kinds": ["program"],
}

PROFILES: dict[str, dict] = {
    "svelte": {**_CONTAINER, "extensions": [".svelte"]},
    "vue": {**_CONTAINER, "extensions": [".vue"]},
    "astro": {**_CONTAINER, "extensions": [".astro"]},
    "ejs": {**_CONTAINER, "extensions": [".ejs"]},
}

_VOID = {b"area", b"base", b"br", b"col", b"embed", b"hr", b"img", b"input", b"link", b"meta", b"param", b"source", b"track", b"wbr"}
_RAW_TEXT = {b"style", b"textarea", b"title"}
_NAME_RE = re.compile(rb"[A-Za-z][A-Za-z0-9:_.\-]*")
_SCRIPT_CLOSE = re.compile(rb"</script[\s/>]", re.I)
_JS_TYPES = {
    "", "module", "text/javascript", "application/javascript", "text/ecmascript", "application/ecmascript",
    "text/jsx", "text/babel", "text/typescript", "application/typescript",
}
_MAX_DEPTH = 200


# ---------------------------------------------------------------------------
# Lexer helpers (all offsets are original-buffer byte offsets)
# ---------------------------------------------------------------------------

def _skip_string(src: bytes, i: int, quote: int) -> int:
    """``src[i]`` is a quote; return the index after the closing quote (or len)."""
    n = len(src)
    j = i + 1
    while j < n:
        c = src[j]
        if c == 0x5C:  # backslash
            j += 2
            continue
        if c == quote:
            return j + 1
        if c == 0x0A and quote != 0x60:
            return j  # unterminated single-line string: stop at newline
        j += 1
    return n


def _skip_template(src: bytes, i: int, depth: int = 0) -> int:
    n = len(src)
    j = i + 1
    while j < n:
        c = src[j]
        if c == 0x5C:
            j += 2
            continue
        if c == 0x60:
            return j + 1
        if c == 0x24 and src[j + 1:j + 2] == b"{":
            j = _skip_braces(src, j + 1, depth + 1)
            continue
        j += 1
    return n


def _skip_braces(src: bytes, i: int, depth: int = 0) -> int:
    """``src[i] == '{'``: return the index after the matching ``}`` (JS string/template/comment aware)."""
    n = len(src)
    if depth > _MAX_DEPTH:
        return n
    level = 0
    j = i
    while j < n:
        c = src[j]
        if c == 0x7B:
            level += 1
        elif c == 0x7D:
            level -= 1
            if level == 0:
                return j + 1
        elif c == 0x22 or c == 0x27:
            j = _skip_string(src, j, c)
            continue
        elif c == 0x60:
            j = _skip_template(src, j, depth + 1)
            continue
        elif c == 0x2F:
            nxt = src[j + 1:j + 2]
            if nxt == b"/":
                k = src.find(b"\n", j)
                j = n if k < 0 else k
                continue
            if nxt == b"*":
                k = src.find(b"*/", j + 2)
                j = n if k < 0 else k + 2
                continue
        j += 1
    return n


def _parse_tag(src: bytes, i: int, mustache: bool):
    """Parse an opening tag at ``src[i] == '<'``.

    Returns ``(name_bytes, attrs, end_index, self_closing)`` or ``None`` when ``<`` does not start a tag.
    """
    m = _NAME_RE.match(src, i + 1)
    if m is None:
        return None
    name = m.group(0)
    n = len(src)
    j = m.end()
    attrs: dict[str, Any] = {}
    while j < n:
        c = src[j]
        if c in b" \t\r\n":
            j += 1
            continue
        if c == 0x3E:  # >
            return name, attrs, j + 1, False
        if c == 0x2F and src[j + 1:j + 2] == b">":
            return name, attrs, j + 2, True
        if c == 0x2F:
            j += 1
            continue
        if mustache and c == 0x7B:
            j = _skip_braces(src, j)
            continue
        # attribute name
        k = j
        while k < n and src[k] not in b" \t\r\n=>/" and not (mustache and src[k] == 0x7B):
            k += 1
        if k == j:
            j += 1
            continue
        aname = src[j:k].decode("utf-8", "replace").lower()
        j = k
        while j < n and src[j] in b" \t\r\n":
            j += 1
        if j < n and src[j] == 0x3D:  # =
            j += 1
            while j < n and src[j] in b" \t\r\n":
                j += 1
            if j < n and src[j] in b"\"'":
                q = src[j]
                v = j + 1
                p = v
                while p < n and src[p] != q:
                    if mustache and src[p] == 0x7B:
                        p = _skip_braces(src, p)
                        continue
                    p += 1
                attrs[aname] = src[v:p].decode("utf-8", "replace")
                j = min(p + 1, n)
            elif mustache and j < n and src[j] == 0x7B:
                p = _skip_braces(src, j)
                attrs[aname] = src[j:p].decode("utf-8", "replace")
                j = p
            else:
                v = j
                while j < n and src[j] not in b" \t\r\n>":
                    j += 1
                attrs[aname] = src[v:j].decode("utf-8", "replace")
        else:
            attrs[aname] = True
    return name, attrs, n, False


def _scan_html(src: bytes, *, mustache: bool, start: int = 0, all_scripts_top_level: bool = False):
    """Yield ``{attrs, start, end, top, closed}`` for every ``<script>`` element.

    ``top`` is True when no ancestor element is open.  Comments, ``<style>``/
    ``<textarea>``/``<title>`` raw text and ``{...}`` expressions are skipped.
    """
    n = len(src)
    i = start
    stack: list[bytes] = []
    while i < n:
        c = src[i]
        if c == 0x3C:  # <
            if src.startswith(b"<!--", i):
                k = src.find(b"-->", i + 4)
                i = n if k < 0 else k + 3
                continue
            if src.startswith(b"<![CDATA[", i):
                k = src.find(b"]]>", i)
                i = n if k < 0 else k + 3
                continue
            if src[i + 1:i + 2] == b"/":
                m = _NAME_RE.match(src, i + 2)
                k = src.find(b">", i)
                if m is not None:
                    nm = m.group(0).lower()
                    for idx in range(len(stack) - 1, -1, -1):
                        if stack[idx] == nm:
                            del stack[idx:]
                            break
                i = n if k < 0 else k + 1
                continue
            tag = _parse_tag(src, i, mustache)
            if tag is None:
                i += 1
                continue
            name, attrs, end, self_closing = tag
            lower = name.lower()
            if lower == b"script":
                if self_closing:
                    yield {"attrs": attrs, "start": end, "end": end, "top": not stack or all_scripts_top_level, "closed": True}
                    i = end
                    continue
                m = _SCRIPT_CLOSE.search(src, end)
                if m is None:
                    yield {"attrs": attrs, "start": end, "end": n, "top": not stack or all_scripts_top_level, "closed": False}
                    return
                yield {"attrs": attrs, "start": end, "end": m.start(), "top": not stack or all_scripts_top_level, "closed": True}
                k = src.find(b">", m.start())
                i = n if k < 0 else k + 1
                continue
            if lower in _RAW_TEXT and not self_closing:
                m = re.compile(rb"</" + re.escape(lower) + rb"[\s/>]", re.I).search(src, end)
                if m is None:
                    return
                k = src.find(b">", m.start())
                i = n if k < 0 else k + 1
                continue
            if not self_closing and lower not in _VOID:
                stack.append(lower)
                if len(stack) > 10_000:
                    del stack[:5_000]
            i = end
            continue
        if mustache and c == 0x7B:
            i = _skip_braces(src, i)
            continue
        i += 1


def _scan_vue_blocks(src: bytes):
    """Top-level SFC blocks: yield ``{attrs, start, end, closed}`` for ``<script>`` blocks only."""
    n = len(src)
    i = 0
    while i < n:
        if src[i] != 0x3C:
            i += 1
            continue
        if src.startswith(b"<!--", i):
            k = src.find(b"-->", i + 4)
            i = n if k < 0 else k + 3
            continue
        tag = _parse_tag(src, i, False)
        if tag is None:
            i += 1
            continue
        name, attrs, end, self_closing = tag
        lower = name.lower()
        if self_closing:
            i = end
            continue
        if lower == b"template":
            depth = 1
            pos = end
            opener = re.compile(rb"<template[\s/>]", re.I)
            closer = re.compile(rb"</template[\s/>]", re.I)
            while depth > 0:
                o = opener.search(src, pos)
                c = closer.search(src, pos)
                if c is None:
                    pos = n
                    break
                if o is not None and o.start() < c.start():
                    depth += 1
                    pos = o.end()
                else:
                    depth -= 1
                    k = src.find(b">", c.start())
                    pos = n if k < 0 else k + 1
            i = pos
            continue
        closer = re.compile(rb"</" + re.escape(lower) + rb"[\s/>]", re.I)
        m = closer.search(src, end)
        if lower == b"script":
            if m is None:
                yield {"attrs": attrs, "start": end, "end": n, "closed": False}
                return
            yield {"attrs": attrs, "start": end, "end": m.start(), "closed": True}
        if m is None:
            return
        k = src.find(b">", m.start())
        i = n if k < 0 else k + 1


def _astro_frontmatter(src: bytes):
    """Return ``(start, end, after)`` of the frontmatter region or ``None`` (``after`` = index past the closing fence line)."""
    n = len(src)
    i = 3 if src.startswith(b"\xef\xbb\xbf") else 0
    while i < n and src[i] in b" \t\r\n":
        i += 1
    if not src.startswith(b"---", i):
        return None
    eol = src.find(b"\n", i)
    if eol < 0 or src[i + 3:eol].strip() != b"":
        return None
    start = eol + 1
    j = start
    at_line_start = True
    fence = re.compile(rb"---[ \t]*(\r?\n|$)")
    while j < n:
        if at_line_start:
            m = fence.match(src, j)
            if m is not None:
                return start, j, m.end()
            at_line_start = False
        c = src[j]
        if c == 0x0A:
            at_line_start = True
            j += 1
            continue
        if c == 0x22 or c == 0x27:
            k = _skip_string(src, j, c)
            at_line_start = at_line_start or False
            j = k
            continue
        if c == 0x60:
            j = _skip_template(src, j)
            # a template may span lines; the next char decides line start
            at_line_start = j > 0 and src[j - 1:j] == b"\n"
            continue
        if c == 0x2F:
            nxt = src[j + 1:j + 2]
            if nxt == b"/":
                k = src.find(b"\n", j)
                j = n if k < 0 else k
                continue
            if nxt == b"*":
                k = src.find(b"*/", j + 2)
                if k < 0:
                    return None
                at_line_start = False
                j = k + 2
                continue
        j += 1
    return None


# ---------------------------------------------------------------------------
# Extraction
# ---------------------------------------------------------------------------

def _region_language(attrs: dict, default: str) -> tuple[str | None, str | None]:
    """Return ``(language, skipReason)`` for a script element's attributes."""
    typ = attrs.get("type")
    typ = typ.strip().lower() if isinstance(typ, str) else ("" if typ is None else str(typ))
    if typ not in _JS_TYPES:
        return None, f"non-JavaScript script type '{typ}' (data block) not parsed"
    lang = attrs.get("lang")
    if lang is True:
        lang = ""
    lang = lang.strip().lower() if isinstance(lang, str) else ""
    if typ in ("text/typescript", "application/typescript"):
        lang = lang or "ts"
    if lang == "":
        return default, None
    if lang in ("ts", "typescript"):
        return "typescript", None
    if lang == "tsx":
        return "tsx", None
    if lang in ("js", "javascript", "jsx", "mjs", "cjs"):
        return "javascript", None
    return None, f"script lang '{lang}' is not supported; region not parsed"


class _Plan:
    def __init__(self) -> None:
        self.regions: list[dict] = []  # {name, language, ranges, module_range, order, owner_module(bool)}
        self.diagnostics: list[str] = []
        self.partial = False


def _unique(names: dict[str, int], base: str) -> str:
    count = names.get(base, 0) + 1
    names[base] = count
    return base if count == 1 else f"{base}#{count}"


def _plan_svelte(src: bytes) -> _Plan:
    plan = _Plan()
    names: dict[str, int] = {}
    for block in _scan_html(src, mustache=True):
        lang, skip = _region_language(block["attrs"], "javascript")
        if not block["closed"]:
            plan.partial = True
            plan.diagnostics.append("unterminated <script> element: region extends to end of file")
        if lang is None:
            if skip and "non-JavaScript" in skip:
                plan.diagnostics.append(skip)
            else:
                plan.partial = True
                plan.diagnostics.append(skip or "script region not parsed")
            continue
        if block["start"] >= block["end"]:
            continue
        attrs = block["attrs"]
        if block["top"]:
            is_module = ("module" in attrs) or str(attrs.get("context", "")).lower() == "module"
            name = _unique(names, "module" if is_module else "instance")
            kind = "module" if is_module else "instance"
        else:
            name = _unique(names, "embedded")
            kind = "embedded"
            plan.diagnostics.append("script nested inside an element is a DOM script, not component scope; parsed as an independent 'embedded' region")
        plan.regions.append({"name": name, "kind": kind, "language": lang, "ranges": [(block["start"], block["end"])]})
    return plan


def _plan_vue(src: bytes) -> _Plan:
    plan = _Plan()
    names: dict[str, int] = {}
    for block in _scan_vue_blocks(src):
        lang, skip = _region_language(block["attrs"], "javascript")
        if not block["closed"]:
            plan.partial = True
            plan.diagnostics.append("unterminated <script> block: region extends to end of file")
        if lang is None:
            if skip and "non-JavaScript" in skip:
                plan.diagnostics.append(skip)
            else:
                plan.partial = True
                plan.diagnostics.append(skip or "script region not parsed")
            continue
        if block["start"] >= block["end"]:
            continue
        is_setup = "setup" in block["attrs"]
        kind = "instance" if is_setup else "module"
        name = _unique(names, kind)
        plan.regions.append({"name": name, "kind": kind, "language": lang, "ranges": [(block["start"], block["end"])]})
    return plan


def _plan_astro(src: bytes) -> _Plan:
    plan = _Plan()
    names: dict[str, int] = {}
    start = 0
    front = _astro_frontmatter(src)
    if front is not None:
        s, e, after = front
        if s < e:
            plan.regions.append({"name": "instance", "kind": "instance", "language": "typescript", "ranges": [(s, e)]})
            names["instance"] = 1
        start = after
    elif src.lstrip().startswith(b"---"):
        plan.partial = True
        plan.diagnostics.append("Astro frontmatter fence is not properly closed; frontmatter not parsed")
    for block in _scan_html(src, mustache=True, start=start, all_scripts_top_level=True):
        lang, skip = _region_language(block["attrs"], "typescript")
        if not block["closed"]:
            plan.partial = True
            plan.diagnostics.append("unterminated <script> element: region extends to end of file")
        if lang is None:
            if skip and "non-JavaScript" in skip:
                plan.diagnostics.append(skip)
            else:
                plan.partial = True
                plan.diagnostics.append(skip or "script region not parsed")
            continue
        if block["start"] >= block["end"]:
            continue
        plan.regions.append({"name": _unique(names, "module"), "kind": "module", "language": lang, "ranges": [(block["start"], block["end"])]})
    return plan


def _plan_ejs(src: bytes) -> _Plan:
    plan = _Plan()
    scriptlets: list[tuple[int, int]] = []
    exprs: list[tuple[int, int]] = []
    n = len(src)
    i = 0
    while True:
        j = src.find(b"<%", i)
        if j < 0:
            break
        if src.startswith(b"<%%", j):
            i = j + 3
            continue
        k = j + 2
        mod = src[k:k + 1]
        if mod in (b"=", b"-", b"_", b"#"):
            k += 1
        end = src.find(b"%>", k)
        if end < 0:
            plan.partial = True
            plan.diagnostics.append(f"unterminated EJS tag at byte {j}; remainder not parsed")
            break
        code_end = end
        if code_end > k and src[code_end - 1:code_end] in (b"-", b"_"):
            code_end -= 1
        if mod == b"#":
            pass
        elif code_end > k:
            if mod in (b"=", b"-"):
                exprs.append((k, code_end))
            else:
                scriptlets.append((k, code_end))
        i = end + 2
    if scriptlets:
        plan.regions.append({"name": "instance", "kind": "instance", "language": "javascript", "ranges": scriptlets})
    for idx, rng in enumerate(exprs, 1):
        plan.regions.append({"name": f"expr#{idx}", "kind": "expression", "language": "javascript", "ranges": [rng]})
    return plan


_PLANNERS = {"svelte": _plan_svelte, "vue": _plan_vue, "astro": _plan_astro, "ejs": _plan_ejs}
_NOTES = {
    "svelte": "Svelte template markup, styles, directives and runes/stores are separate coverage and not analyzed; only <script> regions were parsed",
    "vue": "Vue template, styles and custom blocks are separate coverage and not analyzed; only <script> blocks were parsed",
    "astro": "Astro template markup, components in markup and styles are separate coverage and not analyzed; only frontmatter and <script> regions were parsed",
    "ejs": "EJS output markup and include()/partial resolution are separate coverage and not analyzed; only <% %> code regions were parsed",
}


def extract(language: str, source: bytes, ctx: common.Context) -> dict:
    plan = _PLANNERS[language](source)
    if plan.partial:
        ctx.partial = True
    for note in plan.diagnostics:
        ctx.diag(note)
    ctx.diag(_NOTES[language])
    # Region order: module-scope regions first so that instance regions can chain to them.
    module_ids: dict[str, str] = {}
    ordered = sorted(plan.regions, key=lambda r: 0 if r["kind"] == "module" else 1 if r["kind"] == "instance" else 2)
    instance_id: str | None = None
    for region in ordered:
        owner = None
        if region["kind"] in ("instance",) and module_ids:
            owner = next(iter(module_ids.values()))
        elif region["kind"] == "expression":
            owner = instance_id
        elif region["kind"] == "module" and language == "astro":
            owner = None
        ranges = region["ranges"]
        ctx.set_region(region["name"], region["language"])
        try:
            tree = ctx.parse(region["language"], ranges)
        except common.ParseTimeout:
            raise
        except common.GrammarUnavailable as exc:
            ctx.mark_partial(f"region '{region['name']}' not parsed: grammar unavailable ({exc})")
            continue
        mid = lang_js.run_region(
            ctx,
            region["language"],
            tree,
            region=region["name"],
            module_range=(ranges[0][0], ranges[-1][1]),
            owner=owner,
        )
        if region["kind"] == "module":
            module_ids[region["name"]] = mid
        if region["kind"] == "instance":
            instance_id = mid
    ctx.set_region("main", language)
    return ctx.result()
