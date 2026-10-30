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
_WS = b" \t\r\n"
_FENCE = re.compile(rb"---[ \t]*(\r?\n|$)")
_NOT_PARSED = "script region not parsed"


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


def _skip_token(src: bytes, j: int, template_depth: int) -> int:
    """Index after the string, template or comment starting at ``src[j]``; ``j + 1`` for any other byte."""
    c = src[j]
    if c == 0x22 or c == 0x27:
        return _skip_string(src, j, c)
    if c == 0x60:
        return _skip_template(src, j, template_depth)
    if c == 0x2F:
        nxt = src[j + 1:j + 2]
        if nxt == b"/":
            k = src.find(b"\n", j)
            return len(src) if k < 0 else k
        if nxt == b"*":
            return _skip_past(src, b"*/", j + 2)
    return j + 1


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
        else:
            j = _skip_token(src, j, depth + 1)
            continue
        j += 1
    return n


def _skip_ws(src: bytes, j: int) -> int:
    n = len(src)
    while j < n and src[j] in _WS:
        j += 1
    return j


def _skip_past(src: bytes, needle: bytes, pos: int) -> int:
    """Index after the next ``needle`` at or beyond ``pos`` (or len)."""
    k = src.find(needle, pos)
    return len(src) if k < 0 else k + len(needle)


def _skip_delimited(src: bytes, i: int, opener: bytes, closer: bytes) -> int | None:
    """Index after ``opener ... closer`` starting at ``src[i]`` (or len), ``None`` when ``src[i]`` does not start with ``opener``."""
    if not src.startswith(opener, i):
        return None
    return _skip_past(src, closer, i + len(opener))


def _find_close(src: bytes, name: bytes, pos: int):
    """Match of the next ``</name`` close tag at or beyond ``pos``, or ``None``."""
    return re.compile(rb"</" + re.escape(name) + rb"[\s/>]", re.I).search(src, pos)


def _skip_raw_text(src: bytes, name: bytes, pos: int) -> int | None:
    """Index after the close tag of a raw-text element, or ``None`` when it never closes."""
    m = _find_close(src, name, pos)
    return None if m is None else _skip_past(src, b">", m.start())


def _parse_attr_value(src: bytes, j: int, mustache: bool) -> tuple[str, int]:
    """Parse an attribute value at ``src[j]`` (after ``=`` and whitespace): ``(text, index_after)``."""
    n = len(src)
    if j < n and src[j] in b"\"'":
        q = src[j]
        v = j + 1
        p = v
        while p < n and src[p] != q:
            if mustache and src[p] == 0x7B:
                p = _skip_braces(src, p)
                continue
            p += 1
        return src[v:p].decode("utf-8", "replace"), min(p + 1, n)
    if mustache and j < n and src[j] == 0x7B:
        p = _skip_braces(src, j)
        return src[j:p].decode("utf-8", "replace"), p
    v = j
    while j < n and src[j] not in b" \t\r\n>":
        j += 1
    return src[v:j].decode("utf-8", "replace"), j


def _parse_attr(src: bytes, j: int, mustache: bool, attrs: dict[str, Any]) -> int:
    """Parse one attribute at ``src[j]`` into ``attrs``; return the index after it."""
    n = len(src)
    k = j
    while k < n and src[k] not in b" \t\r\n=>/" and not (mustache and src[k] == 0x7B):
        k += 1
    if k == j:
        return j + 1
    aname = src[j:k].decode("utf-8", "replace").lower()
    j = _skip_ws(src, k)
    if j >= n or src[j] != 0x3D:  # =
        attrs[aname] = True
        return j
    attrs[aname], j = _parse_attr_value(src, _skip_ws(src, j + 1), mustache)
    return j


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
        if c in _WS:
            j += 1
        elif c == 0x3E:  # >
            return name, attrs, j + 1, False
        elif c == 0x2F:
            if src[j + 1:j + 2] == b">":
                return name, attrs, j + 2, True
            j += 1
        elif mustache and c == 0x7B:
            j = _skip_braces(src, j)
        else:
            j = _parse_attr(src, j, mustache, attrs)
    return name, attrs, n, False


def _close_tag(src: bytes, stack: list[bytes], i: int) -> int:
    """``src[i:i+2] == '</'``: pop the matching open element (if any); return the index after the tag."""
    m = _NAME_RE.match(src, i + 2)
    if m is not None:
        nm = m.group(0).lower()
        for idx in range(len(stack) - 1, -1, -1):
            if stack[idx] == nm:
                del stack[idx:]
                break
    return _skip_past(src, b">", i)


def _script_element(src: bytes, attrs: dict, end: int, top: bool, self_closing: bool) -> tuple[int | None, dict]:
    """Next scan index and block; a None index marks an unclosed script extending to EOF."""
    block = {"attrs": attrs, "start": end, "end": end, "top": top, "closed": True}
    if self_closing:
        return end, block
    m = _SCRIPT_CLOSE.search(src, end)
    if m is None:
        block["end"] = len(src)
        block["closed"] = False
        return None, block
    block["end"] = m.start()
    return _skip_past(src, b">", m.start()), block


def _html_markup(src: bytes, i: int, mustache: bool, stack: list[bytes], all_top: bool) -> tuple[int | None, dict | None]:
    """Handle ``src[i] == '<'``: ``(next_index, script_block_or_None)``; a ``None`` index ends the scan."""
    special = _skip_delimited(src, i, b"<!--", b"-->")
    if special is None:
        special = _skip_delimited(src, i, b"<![CDATA[", b"]]>")
    if special is not None:
        return special, None
    if src[i + 1:i + 2] == b"/":
        return _close_tag(src, stack, i), None
    tag = _parse_tag(src, i, mustache)
    if tag is None:
        return i + 1, None
    name, attrs, end, self_closing = tag
    lower = name.lower()
    if lower == b"script":
        return _script_element(src, attrs, end, not stack or all_top, self_closing)
    if lower in _RAW_TEXT and not self_closing:
        return _skip_raw_text(src, lower, end), None
    if not self_closing and lower not in _VOID:
        stack.append(lower)
        if len(stack) > 10_000:
            del stack[:5_000]
    return end, None


def _scan_html(src: bytes, *, mustache: bool, start: int = 0, all_scripts_top_level: bool = False):
    """Yield ``{attrs, start, end, top, closed}`` for every ``<script>`` element.

    ``top`` is True when no ancestor element is open.  Comments, ``<style>``/
    ``<textarea>``/``<title>`` raw text and ``{...}`` expressions are skipped.
    """
    n = len(src)
    i: int | None = start
    stack: list[bytes] = []
    while i is not None and i < n:
        c = src[i]
        if c == 0x3C:  # <
            i, block = _html_markup(src, i, mustache, stack, all_scripts_top_level)
            if block is not None:
                yield block
        elif mustache and c == 0x7B:
            i = _skip_braces(src, i)
        else:
            i += 1


def _skip_template_block(src: bytes, pos: int) -> int:
    """``pos`` is just after an opening ``<template>``: return the index after its matching close tag (or len)."""
    n = len(src)
    depth = 1
    opener = re.compile(rb"<template[\s/>]", re.I)
    closer = re.compile(rb"</template[\s/>]", re.I)
    while depth > 0:
        o = opener.search(src, pos)
        c = closer.search(src, pos)
        if c is None:
            return n
        if o is not None and o.start() < c.start():
            depth += 1
            pos = o.end()
        else:
            depth -= 1
            pos = _skip_past(src, b">", c.start())
    return pos


def _vue_block(src: bytes, i: int) -> tuple[int | None, dict | None]:
    """Handle ``src[i] == '<'`` at SFC top level: ``(next_index, script_block_or_None)``; a ``None`` index ends the scan."""
    skipped = _skip_delimited(src, i, b"<!--", b"-->")
    if skipped is not None:
        return skipped, None
    tag = _parse_tag(src, i, False)
    if tag is None:
        return i + 1, None
    name, attrs, end, self_closing = tag
    lower = name.lower()
    if self_closing:
        return end, None
    if lower == b"template":
        return _skip_template_block(src, end), None
    m = _find_close(src, lower, end)
    block = None
    if lower == b"script":
        block = {"attrs": attrs, "start": end, "end": len(src) if m is None else m.start(), "closed": m is not None}
    if m is None:
        return None, block
    return _skip_past(src, b">", m.start()), block


def _scan_vue_blocks(src: bytes):
    """Top-level SFC blocks: yield ``{attrs, start, end, closed}`` for ``<script>`` blocks only."""
    n = len(src)
    i: int | None = 0
    while i is not None and i < n:
        if src[i] != 0x3C:
            i += 1
            continue
        i, block = _vue_block(src, i)
        if block is not None:
            yield block


def _astro_frontmatter_open(src: bytes) -> int | None:
    """Index after the opening ``---`` fence line (BOM and leading whitespace allowed), or ``None``."""
    i = _skip_ws(src, 3 if src.startswith(b"\xef\xbb\xbf") else 0)
    if not src.startswith(b"---", i):
        return None
    eol = src.find(b"\n", i)
    if eol < 0 or src[i + 3:eol].strip() != b"":
        return None
    return eol + 1


def _astro_advance(src: bytes, j: int) -> int | None:
    """Index after the string, template or comment at ``src[j]`` (``j + 1`` otherwise); ``None`` for an unterminated block comment."""
    if src.startswith(b"/*", j) and src.find(b"*/", j + 2) < 0:
        return None
    return _skip_token(src, j, 0)


def _astro_frontmatter(src: bytes):
    """Return ``(start, end, after)`` of the frontmatter region or ``None`` (``after`` = index past the closing fence line)."""
    start = _astro_frontmatter_open(src)
    if start is None:
        return None
    n = len(src)
    j = start
    at_line_start = True
    while j < n:
        if at_line_start:
            m = _FENCE.match(src, j)
            if m is not None:
                return start, j, m.end()
        at_line_start = src[j] == 0x0A
        j = j + 1 if at_line_start else _astro_advance(src, j)
        if j is None:
            return None
    return None


# ---------------------------------------------------------------------------
# Extraction
# ---------------------------------------------------------------------------

def _region_language(attrs: dict, default: str) -> tuple[str | None, str | None]:
    """Return ``(language, skipReason)`` for a script element's attributes."""
    typ = attrs.get("type")
    if typ is None:
        typ = ""
    elif isinstance(typ, str):
        typ = typ.strip().lower()
    else:
        typ = str(typ)
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


def _usable_language(plan: _Plan, block: dict, default: str, noun: str) -> str | None:
    """Record diagnostics for a script block; return its language, or ``None`` when it is skipped or empty."""
    lang, skip = _region_language(block["attrs"], default)
    if not block["closed"]:
        plan.partial = True
        plan.diagnostics.append(f"unterminated <script> {noun}: region extends to end of file")
    if lang is None:
        if skip and "non-JavaScript" in skip:
            plan.diagnostics.append(skip)
        else:
            plan.partial = True
            plan.diagnostics.append(skip or _NOT_PARSED)
        return None
    if block["start"] >= block["end"]:
        return None
    return lang


def _plan_svelte(src: bytes) -> _Plan:
    plan = _Plan()
    names: dict[str, int] = {}
    for block in _scan_html(src, mustache=True):
        lang = _usable_language(plan, block, "javascript", "element")
        if lang is None:
            continue
        attrs = block["attrs"]
        if block["top"]:
            is_module = ("module" in attrs) or str(attrs.get("context", "")).lower() == "module"
            kind = "module" if is_module else "instance"
        else:
            kind = "embedded"
            plan.diagnostics.append("script nested inside an element is a DOM script, not component scope; parsed as an independent 'embedded' region")
        plan.regions.append({"name": _unique(names, kind), "kind": kind, "language": lang, "ranges": [(block["start"], block["end"])]})
    return plan


def _plan_vue(src: bytes) -> _Plan:
    plan = _Plan()
    names: dict[str, int] = {}
    for block in _scan_vue_blocks(src):
        lang = _usable_language(plan, block, "javascript", "block")
        if lang is None:
            continue
        kind = "instance" if "setup" in block["attrs"] else "module"
        plan.regions.append({"name": _unique(names, kind), "kind": kind, "language": lang, "ranges": [(block["start"], block["end"])]})
    return plan


def _plan_astro_frontmatter(plan: _Plan, names: dict[str, int], src: bytes) -> int:
    """Record the frontmatter region (if any); return the offset where ``<script>`` scanning starts."""
    front = _astro_frontmatter(src)
    if front is None:
        if src.lstrip().startswith(b"---"):
            plan.partial = True
            plan.diagnostics.append("Astro frontmatter fence is not properly closed; frontmatter not parsed")
        return 0
    s, e, after = front
    if s < e:
        plan.regions.append({"name": "instance", "kind": "instance", "language": "typescript", "ranges": [(s, e)]})
        names["instance"] = 1
    return after


def _plan_astro(src: bytes) -> _Plan:
    plan = _Plan()
    names: dict[str, int] = {}
    start = _plan_astro_frontmatter(plan, names, src)
    for block in _scan_html(src, mustache=True, start=start, all_scripts_top_level=True):
        lang = _usable_language(plan, block, "typescript", "element")
        if lang is None:
            continue
        plan.regions.append({"name": _unique(names, "module"), "kind": "module", "language": lang, "ranges": [(block["start"], block["end"])]})
    return plan


def _ejs_tag(src: bytes, j: int) -> tuple[bytes, int, int, int] | None:
    """Parse the EJS tag at ``src[j:j+2] == '<%'``: ``(modifier, code_start, code_end, tag_end)``, or ``None`` when unterminated."""
    k = j + 2
    mod = src[k:k + 1]
    if mod in (b"=", b"-", b"_", b"#"):
        k += 1
    end = src.find(b"%>", k)
    if end < 0:
        return None
    code_end = end
    if code_end > k and src[code_end - 1:code_end] in (b"-", b"_"):
        code_end -= 1
    return mod, k, code_end, end


def _ejs_record(mod: bytes, rng: tuple[int, int], scriptlets: list[tuple[int, int]], exprs: list[tuple[int, int]]) -> None:
    if mod == b"#" or rng[1] <= rng[0]:
        return
    if mod in (b"=", b"-"):
        exprs.append(rng)
    else:
        scriptlets.append(rng)


def _plan_ejs(src: bytes) -> _Plan:
    plan = _Plan()
    scriptlets: list[tuple[int, int]] = []
    exprs: list[tuple[int, int]] = []
    i = 0
    while True:
        j = src.find(b"<%", i)
        if j < 0:
            break
        if src.startswith(b"<%%", j):
            i = j + 3
            continue
        tag = _ejs_tag(src, j)
        if tag is None:
            plan.partial = True
            plan.diagnostics.append(f"unterminated EJS tag at byte {j}; remainder not parsed")
            break
        mod, k, code_end, end = tag
        _ejs_record(mod, (k, code_end), scriptlets, exprs)
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


def _region_order(region: dict) -> int:
    # module-scope regions first so that instance regions can chain to them
    return {"module": 0, "instance": 1}.get(region["kind"], 2)


def _region_owner(region: dict, module_ids: dict[str, str], instance_id: str | None) -> str | None:
    if region["kind"] == "instance" and module_ids:
        return next(iter(module_ids.values()))
    if region["kind"] == "expression":
        return instance_id
    return None


def _extract_region(ctx: common.Context, region: dict, owner: str | None) -> str | None:
    """Parse and extract one region; return its module symbol id, or ``None`` when its grammar is unavailable."""
    ranges = region["ranges"]
    ctx.set_region(region["name"], region["language"])
    try:
        tree = ctx.parse(region["language"], ranges)
    except common.GrammarUnavailable as exc:
        ctx.mark_partial(f"region '{region['name']}' not parsed: grammar unavailable ({exc})")
        return None
    return lang_js.run_region(
        ctx,
        region["language"],
        tree,
        region=region["name"],
        module_range=(ranges[0][0], ranges[-1][1]),
        owner=owner,
    )


def extract(language: str, source: bytes, ctx: common.Context) -> dict:
    plan = _PLANNERS[language](source)
    if plan.partial:
        ctx.partial = True
    for note in plan.diagnostics:
        ctx.diag(note)
    ctx.diag(_NOTES[language])
    module_ids: dict[str, str] = {}
    instance_id: str | None = None
    for region in sorted(plan.regions, key=_region_order):
        mid = _extract_region(ctx, region, _region_owner(region, module_ids, instance_id))
        if mid is None:
            continue
        if region["kind"] == "module":
            module_ids[region["name"]] = mid
        elif region["kind"] == "instance":
            instance_id = mid
    ctx.set_region("main", language)
    return ctx.result()
