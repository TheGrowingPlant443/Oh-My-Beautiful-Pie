"""Trusted, frozen language-profile registry for the source-index worker.

The extension mapping below is frozen app code (never a repository plugin) and is
looked up with the *exact* extension case.  Language modules (``lang_*.py`` and
``containers.py`` in this directory) supply ``PROFILES`` + ``extract``; they are
discovered only from this directory.
"""
from __future__ import annotations

import importlib
import importlib.metadata
import os
import sys
import time
from typing import Any

import common

_PLAN_EXTENSIONS: dict[str, tuple[str, ...]] = {
    "python": (".py",),
    "javascript": (".js", ".jsx", ".mjs", ".cjs"),
    "typescript": (".ts", ".mts", ".cts"),
    "tsx": (".tsx",),
    "go": (".go",),
    "rust": (".rs",),
    "java": (".java",),
    "groovy": (".groovy", ".gradle"),
    "c": (".c", ".h"),
    "cpp": (".cpp", ".cc", ".cxx", ".hpp", ".hh", ".hxx", ".cu", ".cuh", ".metal"),
    "ruby": (".rb", ".rake"),
    "csharp": (".cs",),
    "kotlin": (".kt", ".kts"),
    "scala": (".scala",),
    "php": (".php",),
    "lua": (".lua", ".luau"),
    "swift": (".swift",),
    "bash": (".sh", ".bash"),
    "powershell": (".ps1", ".psm1"),
    "zig": (".zig",),
    "elixir": (".ex", ".exs"),
    "objc": (".m", ".mm"),
    "julia": (".jl",),
    "fortran": (".f", ".F", ".f90", ".F90", ".f95", ".F95", ".f03", ".F03", ".f08", ".F08"),
    "verilog": (".v", ".sv", ".svh", ".vh"),
    # Containers: embedded-script regions only (see containers.py).
    "svelte": (".svelte",),
    "vue": (".vue",),
    "astro": (".astro",),
    "ejs": (".ejs",),
}

EXTENSION_MAP: dict[str, str] = {ext: lang for lang, exts in _PLAN_EXTENSIONS.items() for ext in exts}
CONTAINER_LANGUAGES = frozenset({"svelte", "vue", "astro", "ejs"})
LANGUAGE_IDS = frozenset(_PLAN_EXTENSIONS)

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
_MODULE_SKIP = {"lang_.py"}

_registry: dict[str, Any] | None = None


def language_for_path(path: str) -> str | None:
    """Exact-case frozen mapping from the file's final extension to a profile id."""
    name = path.replace("\\", "/").rsplit("/", 1)[-1]
    dot = name.rfind(".")
    if dot <= 0:
        return None
    return EXTENSION_MAP.get(name[dot:])


def _module_names() -> list[str]:
    names = ["containers"]
    try:
        for entry in sorted(os.listdir(SCRIPT_DIR)):
            if entry.startswith("lang_") and entry.endswith(".py") and entry not in _MODULE_SKIP:
                names.append(entry[:-3])
    except OSError:
        pass
    return names


def registry() -> dict[str, Any]:
    """Load (once) ``{'profiles': {lang: {module, profile}}, 'moduleErrors': {mod: str}}``."""
    global _registry
    if _registry is not None:
        return _registry
    if SCRIPT_DIR not in sys.path:
        sys.path.insert(0, SCRIPT_DIR)
    profiles: dict[str, dict] = {}
    errors: dict[str, str] = {}
    for name in _module_names():
        try:
            mod = importlib.import_module(name)
            declared = getattr(mod, "PROFILES", None)
            extract = getattr(mod, "extract", None)
            if not isinstance(declared, dict) or not callable(extract):
                raise ImportError("module lacks PROFILES/extract")
        except Exception as exc:  # explicit, never silent
            errors[name] = f"{type(exc).__name__}: {exc}"
            continue
        for lang, profile in declared.items():
            if lang not in LANGUAGE_IDS:
                errors[f"{name}:{lang}"] = "profile language id is not in the frozen allowlist"
                continue
            if lang in profiles:
                errors[f"{name}:{lang}"] = f"duplicate profile (already provided by {profiles[lang]['module']})"
                continue
            profiles[lang] = {"module": mod, "moduleName": name, "profile": profile}
    _registry = {"profiles": profiles, "moduleErrors": errors}
    return _registry


def _required_languages(lang: str, profile: dict) -> list[str]:
    if lang in CONTAINER_LANGUAGES:
        return list(profile.get("requires", ()))
    return [lang]


def probe_profile(lang: str) -> dict:
    """Capability record ``{language,version,available,error,nodeKinds}`` for one profile id."""
    reg = registry()
    entry = reg["profiles"].get(lang)
    if entry is None:
        modules = [m for m in reg["moduleErrors"] if not ":" in m and m.startswith("lang_") or m == "containers"]
        detail = "; ".join(f"{k}: {v}" for k, v in sorted(reg["moduleErrors"].items()) if k.split(":")[0] in modules)
        error = "no extractor module provides this profile" + (f" ({detail})" if detail else "")
        spec = common.LANGUAGE_GRAMMARS.get(lang)
        return {"language": lang, "version": common.grammar_version(spec[0]) if spec else None, "available": False, "error": error, "nodeKinds": []}
    profile = entry["profile"]
    if lang in CONTAINER_LANGUAGES:
        needed = _required_languages(lang, profile)
        bad = []
        versions = []
        kinds: list[str] = []
        degraded = []
        for dep in needed:
            rec = common.language_probe(dep, (profiles_kinds(dep)))
            if not rec["available"]:
                bad.append(f"{dep}: {rec['error']}")
            else:
                versions.append(f"{dep}={rec['version']}")
                if rec["error"]:
                    degraded.append(f"{dep}: {rec['error']}")
        if bad:
            return {"language": lang, "version": None, "available": False, "error": "; ".join(bad), "nodeKinds": []}
        return {
            "language": lang,
            "version": ",".join(versions) or None,
            "available": True,
            "error": "; ".join(degraded) or None,
            "nodeKinds": list(profile.get("node_kinds", [])),
        }
    return common.language_probe(lang, profile.get("node_kinds", []))


def profiles_kinds(lang: str) -> list[str]:
    entry = registry()["profiles"].get(lang)
    return list(entry["profile"].get("node_kinds", [])) if entry else []


def capabilities() -> dict:
    """Result of ``worker.py capabilities`` (probes every frozen profile id)."""
    try:
        ts_version = importlib.metadata.version("tree-sitter")
    except Exception:
        ts_version = None
    ts_info: dict[str, Any] = {"version": ts_version, "languageVersion": None, "minCompatibleLanguageVersion": None, "error": None}
    try:
        ts = common.tree_sitter_module()
        ts_info["languageVersion"] = getattr(ts, "LANGUAGE_VERSION", None)
        ts_info["minCompatibleLanguageVersion"] = getattr(ts, "MIN_COMPATIBLE_LANGUAGE_VERSION", None)
    except common.GrammarUnavailable as exc:
        ts_info["error"] = str(exc)
    reg = registry()
    records = [probe_profile(lang) for lang in sorted(LANGUAGE_IDS)]
    return {
        "runtime": {"python": sys.version.split()[0], "pythonFull": sys.version, "treeSitter": ts_info},
        "profiles": records,
        "moduleErrors": dict(reg["moduleErrors"]),
    }


def extract_file(path: str, data: bytes, *, deadline: float | None = None, node_cap: int = common.MAX_NODES) -> dict:
    """Parse one file with its frozen profile.  Always returns a row; never raises.

    Returns ``{language,status,diagnostics,symbols,sites,edges}`` where status is
    parsed|partial|failed|skipped|unsupported.  ``unsupported`` covers unknown
    extensions *and* unavailable grammars (explicit, never a false empty pass).
    """
    lang = language_for_path(path)
    row: dict[str, Any] = {"language": lang, "status": "unsupported", "diagnostics": [], "symbols": [], "sites": [], "edges": []}
    if lang is None:
        row["diagnostics"].append("no parser profile for this file extension; inventoried only")
        return row
    if len(data) > common.MAX_FILE_BYTES:
        row["status"] = "skipped"
        row["diagnostics"].append(f"file exceeds the {common.MAX_FILE_BYTES} byte parser admission limit")
        return row
    if b"\x00" in data:
        row["status"] = "skipped"
        row["diagnostics"].append("binary content (NUL byte); not parsed")
        return row
    reg = registry()
    entry = reg["profiles"].get(lang)
    if entry is None:
        row["diagnostics"].append(probe_profile(lang)["error"])
        return row
    rec = probe_profile(lang)
    if not rec["available"]:
        row["diagnostics"].append(f"grammar unavailable for {lang}: {rec['error']}")
        return row
    if deadline is None:
        deadline = time.monotonic() + common.FILE_SOFT_DEADLINE_SECONDS
    ctx = common.Context(path, lang, data, deadline=deadline, node_cap=node_cap)
    status = "parsed"
    diagnostics: list[str] = []
    try:
        try:
            data.decode("utf-8")
        except UnicodeDecodeError as exc:
            diagnostics.append(f"invalid UTF-8 at byte {exc.start}: raw strings are lossy (U+FFFD); byte offsets exact")
            status = "partial"
        try:
            entry["module"].extract(lang, data, ctx)
        except common.NodeCapExceeded as exc:
            ctx.mark_partial(f"{exc}; facts before the cap are retained")
        except common.DeadlineExceeded as exc:
            ctx.mark_partial(f"{exc}; facts before the deadline are retained")
        result = ctx.result()
    except common.ParseTimeout as exc:
        row.update({"status": "failed", "diagnostics": diagnostics + [str(exc)]})
        return row
    except common.GrammarUnavailable as exc:
        row["diagnostics"] = diagnostics + [f"grammar unavailable for {lang}: {exc}"]
        return row
    except RecursionError:
        row.update({"status": "failed", "diagnostics": diagnostics + ["extractor recursion limit reached"]})
        return row
    except Exception as exc:
        row.update({"status": "failed", "diagnostics": diagnostics + [f"extractor error: {type(exc).__name__}: {exc}"]})
        return row
    if result.get("partial"):
        status = "partial"
    row.update(
        {
            "language": lang,
            "status": status,
            "diagnostics": diagnostics + list(result.get("diagnostics", [])),
            "symbols": result.get("symbols", []),
            "sites": result.get("sites", []),
            "edges": result.get("edges", []),
        }
    )
    return row
