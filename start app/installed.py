"""Pinned packages copied into a private, read-only container store; never a host-store bind."""
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import tempfile

from packages import validate_packages
from requirements import store_root

_STORE_ROOT = re.compile(r"/nix/store/[a-z0-9]{32}-[^/\x00]+\Z")


def _output(run, attribute, source):
    output = run(["nix", "build", f"path:.#{attribute}^out", "--no-link", "--print-out-paths",
                  "--no-write-lock-file"], cwd=source)
    paths = output.splitlines()
    if len(paths) != 1 or not _STORE_ROOT.fullmatch(paths[0]):
        raise ValueError(f"Package {attribute} did not build one Nix store output")
    return paths[0]


def build_packages(selected, agent, source, run):
    """Realize only selected catalog outputs and the matching SDK-based agent CLI."""
    selected = validate_packages(selected)
    if agent not in ("omp", "omsp"):
        raise ValueError("Unknown container agent")
    agent_cli = _output(run, f"agent-{agent}", source)
    launcher = Path(__file__).parent
    outputs = {key: _output(run, f"package-{key}", launcher) for key in selected}
    return agent_cli, outputs


def _private_directory(path):
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError("Container installation cache must be an owner-only directory without symlinks")


def _profile_manifest(bundles, run):
    roots = set()
    programs = {}
    for bundle in bundles.values():
        roots.update(bundle["roots"])
        roots.update(bundle["primary_roots"])
        for program in bundle["programs"]:
            if not _STORE_ROOT.fullmatch(store_root(program)):
                raise ValueError("Installed commands must come from Nix store outputs")
            roots.add(store_root(program))
            programs.setdefault(Path(program).name, program)
    if not roots or any(not _STORE_ROOT.fullmatch(root) for root in roots):
        raise ValueError("Container package closure contains a non-store path")
    # Query lexical output roots too: resolving a buildEnv symlink alone would omit its wrapper/resources.
    all_roots = sorted(set(run(["nix-store", "--query", "--requisites", *sorted(roots)]).splitlines()))
    if not roots.issubset(all_roots) or any(not _STORE_ROOT.fullmatch(root) for root in all_roots):
        raise ValueError("Container package closure is incomplete or invalid")
    return {"version": 1, "roots": all_roots, "programs": programs}


def install_profile(directory, bundles, run, check_cancelled):
    """Atomically cache complete package closures and executable links, without host credentials."""
    manifest = _profile_manifest(bundles, run)
    encoded = json.dumps(manifest, sort_keys=True).encode()
    key = hashlib.sha256(encoded).hexdigest()
    cache = Path(directory) / "installed"
    _private_directory(cache)
    target = cache / key
    if target.exists() or target.is_symlink():
        _private_directory(target)
        record = target / "manifest.json"
        if (record.is_symlink() or record.read_bytes() != encoded
                or any((target / name).is_symlink() or not (target / name).is_dir()
                       for name in ("store", "programs"))):
            raise ValueError("Cached container installation is incomplete or changed")
        check_cancelled()
        return target
    with tempfile.TemporaryDirectory(prefix=".install-", dir=cache) as staging:
        stage = Path(staging)
        store = stage / "store"
        store.mkdir()
        commands = stage / "programs"
        commands.mkdir()
        copier = str(Path(os.environ["START_APP_COREUTILS"]) / "cp")
        # Reflinks are independent files; never hardlink private installations into the host store.
        run([copier, "--archive", "--reflink=auto", "--no-preserve=ownership", "--target-directory", str(store),
             "--", *manifest["roots"]])
        for name, program in manifest["programs"].items():
            (commands / name).symlink_to(program)
        (stage / "manifest.json").write_bytes(encoded)
        check_cancelled()
        os.replace(stage, target)
    return target
