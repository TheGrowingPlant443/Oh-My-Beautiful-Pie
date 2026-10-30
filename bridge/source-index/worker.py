"""Trusted source-index worker (run as ``python -I -B worker.py <command>``).

Commands
  capabilities                         print runtime/profile capability JSON
  extract --manifest M --output O      parse every pending manifest file, resolve, write SourceGraphIndex JSON
  child --root R                       internal per-file parser process (spawned by ``extract``)

``extract`` is a supervised coordinator: files are parsed one at a time in a single
child process that is killed and restarted after a hard per-file timeout or a crash.
Repository code is never imported, executed or used to select modules/grammars.
"""
from __future__ import annotations

import os
import sys

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
if SCRIPT_DIR not in sys.path:
    sys.path.insert(0, SCRIPT_DIR)

import hashlib  # noqa: E402
import json  # noqa: E402
import queue  # noqa: E402
from pathlib import Path  # noqa: E402
import stat  # noqa: E402
import subprocess  # noqa: E402
import threading  # noqa: E402
import time  # noqa: E402

import common  # noqa: E402

MAX_OUTPUT_BYTES = 128 * 1024 * 1024
PARSE_BUDGET_FRACTION = 0.75
READY_TIMEOUT_SECONDS = 120.0
PROGRESS_INTERVAL_SECONDS = 0.25
MAX_LANGUAGE_DIAGNOSTICS = 10

STATIC_LIMITATION = (
    "Static syntax analysis only: dynamic receivers, callbacks, virtual dispatch, reflection, shell commands, "
    "macros, build-time generation and cross-language calls are not promoted to edges by name."
)


class WorkerFailure(Exception):
    """Controlled extraction failure reported on stderr with exit code 2."""


def dumps(value) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


# ---------------------------------------------------------------------------
# child process: parse one file per request
# ---------------------------------------------------------------------------

def _is_reparse_or_link(st: os.stat_result) -> bool:
    if stat.S_ISLNK(st.st_mode):
        return True
    attrs = getattr(st, "st_file_attributes", 0)
    return bool(attrs & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400))


def read_snapshot_file(root: str, rel: str, expect_bytes: int, expect_sha: str) -> tuple[bytes | None, str | None]:
    """Contained, link-refusing read of one snapshot file with hash/size verification."""
    if not rel or rel.startswith("/") or "\\" in rel or ":" in rel.split("/")[0] and os.name == "nt":
        return None, "unsafe manifest path"
    parts = rel.split("/")
    if any(p in ("", ".", "..") for p in parts):
        return None, "unsafe manifest path"
    root_real = os.path.realpath(common.extended_path(root))
    cur = root_real
    try:
        for p in parts:
            cur = os.path.join(cur, p)
            st = os.lstat(common.extended_path(cur))
            if _is_reparse_or_link(st):
                return None, "path component is a link/reparse point; refused"
        if not stat.S_ISREG(st.st_mode):
            return None, "not a regular file"
        real = os.path.realpath(common.extended_path(cur))
        plain_root = common.plain_path(root_real)
        if os.path.commonpath([plain_root, common.plain_path(real)]) != plain_root:
            return None, "path escapes the snapshot root"
        with open(common.extended_path(cur), "rb") as fh:
            data = fh.read(expect_bytes + 1)
    except (OSError, ValueError) as exc:
        return None, f"cannot read snapshot file: {getattr(exc, 'strerror', None) or exc}"
    if len(data) != expect_bytes:
        return None, f"snapshot file size changed (manifest {expect_bytes}, read {len(data)})"
    if hashlib.sha256(data).hexdigest() != expect_sha:
        return None, "snapshot file hash differs from manifest"
    return data, None


def child_main(root: str) -> int:
    import profiles

    proto = sys.stdout.buffer
    sys.stdout = sys.stderr  # nothing but protocol lines may reach the pipe
    # Warm the registry so import errors show up before the handshake.
    profiles.registry()
    proto.write(b'{"ready":true}\n')
    proto.flush()
    inbox: queue.Queue = queue.Queue()

    def pump() -> None:
        for raw in sys.stdin.buffer:
            inbox.put(raw)
        os._exit(0)  # coordinator closed or was killed: stop at once, even in the middle of a parse

    threading.Thread(target=pump, daemon=True).start()
    while True:
        line = inbox.get().strip()
        if not line:
            continue
        req = json.loads(line.decode("utf-8"))
        seq = req["seq"]
        path = req["path"]
        soft = float(req.get("softDeadline", common.FILE_SOFT_DEADLINE_SECONDS))
        data, err = read_snapshot_file(root, path, int(req["bytes"]), str(req["sha256"]))
        if data is None:
            row = {"language": profiles.language_for_path(path), "status": "failed", "diagnostics": [err], "symbols": [], "sites": [], "edges": []}
        else:
            row = profiles.extract_file(path, data, deadline=time.monotonic() + soft)
        proto.write((dumps({"seq": seq, "row": row}) + "\n").encode("utf-8"))
        proto.flush()
    return 0


# ---------------------------------------------------------------------------
# coordinator
# ---------------------------------------------------------------------------

class Coordinator:
    """Supervises one restartable parser child process."""

    def __init__(self, root: str, *, file_timeout: float = common.FILE_HARD_TIMEOUT_SECONDS, child_argv: list[str] | None = None, ready_timeout: float = READY_TIMEOUT_SECONDS):
        self.root = root
        self.file_timeout = file_timeout
        self.ready_timeout = ready_timeout
        self.child_argv = child_argv or [sys.executable, "-I", "-B", os.path.join(SCRIPT_DIR, "worker.py"), "child", "--root", root]
        self.proc: subprocess.Popen | None = None
        self.lines: queue.Queue = queue.Queue()
        self.seq = 0
        self.restarts = 0

    def _reader(self, proc: subprocess.Popen, q: queue.Queue) -> None:
        try:
            for raw in proc.stdout:  # type: ignore[union-attr]
                q.put(raw)
        finally:
            q.put(None)

    def _start(self) -> None:
        env = {k: v for k, v in os.environ.items() if not k.upper().startswith("PYTHON")}
        self.proc = subprocess.Popen(
            self.child_argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            cwd=SCRIPT_DIR,
            env=env,
        )
        self.lines = queue.Queue()
        threading.Thread(target=self._reader, args=(self.proc, self.lines), daemon=True).start()
        try:
            first = self.lines.get(timeout=self.ready_timeout)
        except queue.Empty:
            self._kill()
            raise WorkerFailure("parser child did not become ready in time")
        if first is None or b'"ready"' not in first:
            code = self._kill()
            raise WorkerFailure(f"parser child failed to start (exit code {code})")

    def _kill(self):
        proc = self.proc
        self.proc = None
        if proc is None:
            return None
        try:
            if proc.poll() is None:
                proc.kill()
        except OSError:
            pass
        try:
            for stream in (proc.stdin, proc.stdout):
                if stream is not None:
                    try:
                        stream.close()
                    except OSError:
                        pass
            return proc.wait(timeout=10)
        except Exception:
            return None

    def close(self) -> None:
        proc = self.proc
        if proc is not None and proc.stdin is not None:
            try:
                proc.stdin.close()
            except OSError:
                pass
        self._kill()

    def parse(self, path: str, size: int, sha256: str) -> dict:
        """Parse one file; never raises for per-file failures (returns a failed row)."""
        failed = lambda msg: {"language": None, "status": "failed", "diagnostics": [msg], "symbols": [], "sites": [], "edges": []}  # noqa: E731
        if self.proc is None or self.proc.poll() is not None:
            if self.proc is not None:
                self._kill()
            self._start()
        self.seq += 1
        seq = self.seq
        soft = max(0.05, self.file_timeout * 0.8)
        request = dumps({"seq": seq, "path": path, "bytes": size, "sha256": sha256, "softDeadline": soft}) + "\n"
        proc = self.proc
        assert proc is not None and proc.stdin is not None
        try:
            proc.stdin.write(request.encode("utf-8"))
            proc.stdin.flush()
        except OSError:
            code = self._kill()
            self.restarts += 1
            return failed(f"parser process died before the request was delivered (exit code {code}); file not parsed")
        try:
            raw = self.lines.get(timeout=self.file_timeout)
        except queue.Empty:
            self._kill()
            self.restarts += 1
            return failed(f"parser hard timeout after {self.file_timeout:g}s; parser process was killed and restarted")
        if raw is None:
            code = self._kill()
            self.restarts += 1
            return failed(f"parser process crashed while processing this file (exit code {code})")
        try:
            msg = json.loads(raw.decode("utf-8"))
            if msg.get("seq") != seq:
                raise ValueError("sequence mismatch")
            row = msg["row"]
        except Exception as exc:
            self._kill()
            self.restarts += 1
            return failed(f"parser returned an unreadable result ({type(exc).__name__}); parser restarted")
        row["_bytes"] = len(raw)
        return row


# ---------------------------------------------------------------------------
# index assembly
# ---------------------------------------------------------------------------

def write_progress(path: str, completed: int, total: int, last: str | None) -> None:
    tmp = path + ".tmp"
    try:
        with open(tmp, "w", encoding="utf-8") as fh:
            fh.write(dumps({"completedFiles": completed, "totalFiles": total, "lastFullyProcessedFile": last}))
        os.replace(tmp, path)
    except OSError:
        pass  # progress is advisory; the output file is authoritative


def compute_coverage(index: dict, last_file: str | None, partial: bool) -> dict:
    files = index["files"]
    counts = {"parsed": 0, "partial": 0, "failed": 0, "skipped": 0, "unsupported": 0}
    langs: dict[str, dict] = {}
    for f in files:
        counts[f["status"]] = counts.get(f["status"], 0) + 1
        lang = f.get("language") or "unmapped"
        row = langs.setdefault(lang, {"language": lang, "files": 0, "parsed": 0, "partial": 0, "failed": 0, "unsupported": 0, "diagnostics": []})
        row["files"] += 1
        if f["status"] in ("parsed", "partial", "failed", "unsupported"):
            row[f["status"]] += 1
        for d in f.get("diagnostics", []):
            if d not in row["diagnostics"] and len(row["diagnostics"]) < MAX_LANGUAGE_DIAGNOSTICS:
                row["diagnostics"].append(d)
    res = {"resolvedStatic": 0, "dynamic": 0, "unresolved": 0}
    for s in index["sites"]:
        r = s.get("resolution")
        if r == "resolvedStatic":
            res["resolvedStatic"] += 1
        elif r == "dynamic":
            res["dynamic"] += 1
        elif r in ("unresolved", "candidate", "unsupported"):
            res["unresolved"] += 1
    return {
        "admittedFiles": len(files),
        "parsedFiles": counts["parsed"],
        "partialFiles": counts["partial"],
        "failedFiles": counts["failed"],
        "skippedFiles": counts["skipped"],
        "unsupportedFiles": counts["unsupported"],
        "encounteredSites": len(index["sites"]),
        "resolvedSites": res["resolvedStatic"],
        "unresolvedSites": res["unresolved"],
        "dynamicSites": res["dynamic"],
        "languages": sorted(langs.values(), key=lambda r: r["language"]),
        "lastFullyProcessedFile": last_file,
        "partial": bool(partial),
    }


def load_resolver():
    try:
        import resolve  # trusted sibling module (script dir is first on sys.path)
    except Exception as exc:
        raise WorkerFailure(f"resolver unavailable (resolve.py failed to import: {type(exc).__name__}: {exc}); extraction aborted") from exc
    fn = getattr(resolve, "resolve", None)
    if not callable(fn):
        raise WorkerFailure("resolver unavailable: resolve.py does not define resolve(index, root)")
    return fn


class _ExtractState:
    """Mutable accumulation shared by the per-file steps of ``extract_command``."""

    def __init__(self, budget: int) -> None:
        self.files: list[dict] = []
        self.symbols: list[dict] = []
        self.sites: list[dict] = []
        self.used_languages: set[str] = set()
        self.budget = budget
        self.used_bytes = 0
        self.last_file: str | None = None
        self.capped = False


def _load_manifest(manifest_path: str) -> tuple[dict, str]:
    try:
        with open(manifest_path, "rb") as fh:
            manifest = json.loads(fh.read().decode("utf-8"))
    except Exception as exc:
        raise WorkerFailure(f"cannot read manifest: {exc}") from exc
    if manifest.get("schemaVersion") != 1:
        raise WorkerFailure("unsupported manifest schemaVersion")
    root = manifest.get("root")
    if not isinstance(root, str) or not os.path.isdir(root):
        raise WorkerFailure("manifest root is not an existing directory")
    return manifest, root


def _manifest_row(entry: dict) -> dict:
    path = entry["path"]
    if entry.get("id") != common.file_id(path):
        raise WorkerFailure(f"manifest file id does not match the stable file id for {path!r}")
    return {
        "id": entry["id"],
        "path": path,
        "sha256": entry["sha256"],
        "bytes": entry["bytes"],
        "language": entry.get("language"),
        "status": entry.get("status", "pending"),
        "diagnostics": list(entry.get("diagnostics") or []),
    }


def _parse_pending_entry(state: _ExtractState, coordinator: Coordinator, entry: dict, row: dict) -> None:
    import profiles

    path = row["path"]
    lang = profiles.language_for_path(path)
    if lang is None:
        result = profiles.extract_file(path, b"")
    else:
        result = coordinator.parse(path, int(entry["bytes"]), entry["sha256"])
    size = int(result.pop("_bytes", 0))
    if state.used_bytes + size > state.budget:
        state.capped = True
        row["status"] = "skipped"
        row["language"] = lang
        row["diagnostics"].append("serialized fact limit reached; this file's facts were not retained")
        return
    state.used_bytes += size
    row["language"] = result.get("language") or lang
    row["status"] = result.get("status", "failed")
    row["diagnostics"].extend(result.get("diagnostics", []))
    state.symbols.extend(result.get("symbols", []))
    state.sites.extend(result.get("sites", []))
    state.last_file = path
    if row["language"]:
        state.used_languages.add(row["language"])


def _process_entry(state: _ExtractState, coordinator: Coordinator, entry: dict) -> None:
    import profiles

    row = _manifest_row(entry)
    state.files.append(row)
    path = row["path"]
    if row["status"] != "pending":
        row["language"] = row["language"] or profiles.language_for_path(path)
        state.last_file = path
    elif state.capped:
        row["status"] = "skipped"
        row["language"] = profiles.language_for_path(path)
        row["diagnostics"].append("serialized fact limit reached earlier in this job; file not parsed")
    else:
        _parse_pending_entry(state, coordinator, entry, row)


def _parse_entries(state: _ExtractState, coordinator: Coordinator, entries: list, progress_path: str) -> None:
    total = len(entries)
    last_progress = 0.0
    for i, entry in enumerate(entries):
        _process_entry(state, coordinator, entry)
        now = time.monotonic()
        if now - last_progress >= PROGRESS_INTERVAL_SECONDS or i + 1 == total:
            write_progress(progress_path, i + 1, total, state.last_file)
            last_progress = now
    if total == 0:
        write_progress(progress_path, 0, 0, None)


def _build_limitations(state: _ExtractState, unavailable: dict, degraded: dict, restarts: int, max_output_bytes: int) -> list[str]:
    limitations = [STATIC_LIMITATION]
    for lang in sorted(state.used_languages):
        if lang in unavailable:
            limitations.append(f"{lang}: grammar unavailable ({unavailable[lang]})")
        elif lang in degraded:
            limitations.append(f"{lang}: {degraded[lang]}")
    unavailable_in_repo = sorted(
        {f["language"] for f in state.files if f["status"] == "unsupported" and f["language"] in unavailable}
    )
    for lang in unavailable_in_repo:
        limitations.append(f"{lang}: files present but grammar unavailable ({unavailable[lang]}); inventoried only")
    if state.capped:
        limitations.append(f"Serialized fact limit ({max_output_bytes // (1024 * 1024)} MiB) was reached; coverage is partial after {state.last_file!r}.")
    if restarts:
        limitations.append(f"The parser process was restarted {restarts} time(s) after hard timeouts or crashes; affected files are marked failed.")
    return limitations


def _build_index(manifest: dict, caps: dict, state: _ExtractState, limitations: list[str]) -> dict:
    versions = {
        "worker": 1,
        "python": caps["runtime"]["python"],
        "treeSitter": caps["runtime"]["treeSitter"],
        "profiles": {p["language"]: p["version"] for p in caps["profiles"]},
    }
    return {
        "schemaVersion": 1,
        "snapshotId": manifest.get("snapshotId"),
        "folder": manifest.get("folder"),
        "sourceHash": manifest.get("sourceHash"),
        "extractorVersions": versions,
        "files": state.files,
        "directories": list(manifest.get("directories") or []),
        "symbols": state.symbols,
        "sites": state.sites,
        "edges": [],
        "coverage": {},
        "limitations": limitations + [str(x) for x in (manifest.get("limitations") or [])],
    }


def _resolve_index(resolver, index: dict, root: str) -> dict:
    try:
        resolved = resolver(index, Path(root))
    except Exception as exc:
        raise WorkerFailure(f"resolver failed: {type(exc).__name__}: {exc}") from exc
    if isinstance(resolved, dict):
        index = resolved
    index.setdefault("edges", [])
    index.setdefault("limitations", [])
    ids = {f["id"] for f in index["files"]} | {s["id"] for s in index["symbols"]}
    for edge in index["edges"]:
        if edge.get("sourceId") not in ids or edge.get("targetId") not in ids:
            raise WorkerFailure("resolver produced an edge whose endpoint is not an indexed file or symbol")
    return index


def _write_index(index: dict, output_path: str, max_output_bytes: int) -> None:
    payload = dumps(index).encode("utf-8")
    if len(payload) > max_output_bytes:
        raise WorkerFailure(f"serialized index is {len(payload)} bytes, exceeding the {max_output_bytes} byte limit")
    tmp = output_path + ".tmp"
    with open(tmp, "wb") as fh:
        fh.write(payload)
    os.replace(tmp, output_path)


def extract_command(manifest_path: str, output_path: str, *, file_timeout: float, max_output_bytes: int) -> int:
    import profiles

    manifest, root = _load_manifest(manifest_path)
    resolver = load_resolver()  # fail fast before spending parse time
    entries = manifest.get("files") or []
    progress_path = os.path.join(os.path.dirname(os.path.abspath(output_path)), "progress.json")

    caps = profiles.capabilities()
    unavailable = {p["language"]: p["error"] for p in caps["profiles"] if not p["available"]}
    degraded = {p["language"]: p["error"] for p in caps["profiles"] if p["available"] and p["error"]}

    state = _ExtractState(int(max_output_bytes * PARSE_BUDGET_FRACTION))
    coordinator = Coordinator(root, file_timeout=file_timeout)
    try:
        _parse_entries(state, coordinator, entries, progress_path)
    finally:
        coordinator.close()

    limitations = _build_limitations(state, unavailable, degraded, coordinator.restarts, max_output_bytes)
    index = _build_index(manifest, caps, state, limitations)
    index = _resolve_index(resolver, index, root)
    index["coverage"] = compute_coverage(index, state.last_file, state.capped)
    _write_index(index, output_path, max_output_bytes)
    return 0


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print("usage: worker.py capabilities | extract --manifest M --output O | child --root R", file=sys.stderr)
        return 64
    cmd = argv[1]
    args = argv[2:]

    def opt(name: str, default=None):
        if name in args:
            idx = args.index(name)
            if idx + 1 < len(args):
                return args[idx + 1]
        return default

    try:
        if cmd == "capabilities":
            import profiles

            sys.stdout.write(dumps(profiles.capabilities()))
            sys.stdout.write("\n")
            return 0
        if cmd == "child":
            root = opt("--root")
            if not root:
                raise WorkerFailure("child requires --root")
            return child_main(root)
        if cmd == "extract":
            manifest = opt("--manifest")
            output = opt("--output")
            if not manifest or not output:
                raise WorkerFailure("extract requires --manifest and --output")
            return extract_command(
                manifest,
                output,
                file_timeout=float(opt("--file-timeout", common.FILE_HARD_TIMEOUT_SECONDS)),
                max_output_bytes=int(opt("--max-output-bytes", MAX_OUTPUT_BYTES)),
            )
        print(f"unknown command {cmd!r}", file=sys.stderr)
        return 64
    except WorkerFailure as exc:
        sys.stderr.write(dumps({"error": str(exc)}) + "\n")
        return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
