"""Test helpers shared by the Go/Rust/C-family/JVM/.NET syntax-recipe tests (not a test module)."""
from __future__ import annotations

import unittest

import common


def grammar_status(language: str) -> str | None:
    """None when the grammar loads; otherwise the explicit unavailability reason."""
    try:
        common.load_language(language)
        return None
    except common.ExtractionError as exc:
        return str(exc)


def requires_grammar(language: str):
    """Skip (with the real reason, never a silent pass) when a grammar is unavailable."""
    reason = grammar_status(language)
    return unittest.skipIf(reason is not None, f"grammar unavailable for {language}: {reason}")


def run(module, language: str, source, path: str = "fixture.src") -> dict:
    data = source.encode("utf-8") if isinstance(source, str) else source
    ctx = common.Context(path, language, data)
    return module.extract(language, data, ctx)


def syms(result: dict, kind: str | None = None, name: str | None = None) -> list[dict]:
    return [s for s in result["symbols"]
            if (kind is None or s["kind"] == kind) and (name is None or s["name"] == name)]


def sites(result: dict, relation: str | None = None, name: str | None = None) -> list[dict]:
    out = []
    for s in result["sites"]:
        if relation is not None and s["relation"] != relation:
            continue
        if name is not None and (s.get("binding") or {}).get("name") != name:
            continue
        out.append(s)
    return out


def by_id(result: dict) -> dict:
    return {s["id"]: s for s in result["symbols"]}


def caller_name(result: dict, site: dict) -> str | None:
    sym = by_id(result).get(site["callerId"])
    return None if sym is None else sym["name"]


def declarations(result: dict, owner_name: str | None, name: str | None = None) -> list[dict]:
    """Lexical declarations stored on the owning symbol (``None`` = module symbol)."""
    out = []
    for s in result["symbols"]:
        if (owner_name is None and s["kind"] == "module") or (owner_name is not None and s["name"] == owner_name):
            for d in (s.get("bindings") or {}).get("declarations", []):
                if name is None or d["name"] == name:
                    out.append(d)
    return out


def assert_node_kinds(case: unittest.TestCase, module, language: str) -> None:
    """The loaded grammar must expose every node kind the profile declares it relies on."""
    profile = module.PROFILES[language]
    probe = common.language_probe(language, profile["node_kinds"])
    case.assertTrue(probe["available"], probe["error"])
    case.assertIsNone(probe["error"], probe["error"])
