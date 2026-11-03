"""The Nix bundles and private mounts needed by this sandbox, without host execution."""
import os
from pathlib import Path
import re
import sys

from catalog import host_program

WORKSPACE = "/tmp/omp-manager"
DISPLAY_SOCKET = "/display/wayland"


def prepare_workspace(path=Path(WORKSPACE)):
    """Refuse foreign, symlinked or publicly writable predictable workspaces."""
    try:
        path.mkdir(mode=0o700)
    except FileExistsError:
        pass
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        metadata = os.fstat(fd)
        if metadata.st_uid != os.getuid() or metadata.st_mode & 0o022:
            raise ValueError("OMBP workspace must be owned by you and not group/world-writable")
    finally:
        os.close(fd)


FOLDERS = (
    ("sandbox-home", "Private OMBP home", "/home/sandbox"),
    ("sandbox-tmp", "Private temporary files", str(Path(WORKSPACE).parent)),
    ("sandbox-run", "Private session sockets", "/run"),
    ("launcher-files", "Sandbox entrypoint and OMP bridge", "/launcher"),
    ("network-socket", "Whitelist gateway socket", "/network"),
    ("proxy-ca", "Whitelist HTTPS certificate", "/trust/ca.pem"),
    ("nix-store", "Approved Nix runtime files", "/nix/store"),
    ("host-omp", "Your OMP folder (~/.omp), shared read-write with host OMP", "/home/sandbox/.omp"),
    ("sonar-engine", "SonarQube engine (filtered host Podman)", "/engine"),
)


def store_root(path):
    path = Path(path)
    if path.parts[:3] != ("/", "nix", "store") or len(path.parts) < 4:
        raise ValueError("Expected a Nix store path")
    return str(Path(*path.parts[:4]))


def _programs(path):
    if path.is_file():
        candidates = [path]
    else:
        directory = path if path.name == "bin" else path / "bin"
        candidates = sorted(directory.glob("*"))
    return [str(item) for item in candidates if item.is_file() and os.access(item, os.X_OK)]


def _optional_entries(selected):
    browser = ("sandbox-browser", "Sandbox browser window (Chrome/Chromium/Brave names)",
               os.environ.get("START_APP_BROWSER_NAMES"))
    if selected is not None:
        from packages import PACKAGE_CHOICES
        labels = {item["id"]: item["label"] for item in PACKAGE_CHOICES}
        entries = [browser] if "chromium" in selected else []
        # Flutter's Dart SDK also implements OMP's Flutter debugger defaults.
        entries.extend(("package-" + key, labels[key], selected[key])
                       for key in sorted(selected, key=lambda key: key != "flutter"))
        return entries
    entries = [
        ("sandbox-tools", "Basic tools (gdbus, ps/pgrep/pkill, grep, sed, find, curl, jq, rg…)",
         os.environ.get("START_APP_TOOLS")),
        ("tool-gh", "GitHub CLI (gh)", os.environ.get("START_APP_GH")),
        ("tool-ssh", "SSH client", os.environ.get("START_APP_OPENSSH")),
        browser,
    ]
    chrome = host_program(("google-chrome-stable", "google-chrome"))
    entries.append(("tool-chrome", "Google Chrome", str(Path(chrome).parent)) if chrome else
                   ("tool-chrome", "Chromium (Google Chrome not installed)", os.environ.get("START_APP_CHROMIUM")))
    obsidian = host_program(("obsidian",))
    if obsidian:
        entries.append(("tool-obsidian", "Obsidian", obsidian))
    return entries


def bundles(executable, gpu, resolve_closure, *, selected=None, agent_cli=None):
    entries = [("ombp", "OMBP", executable)]
    # Nix's script and binary wrappers both contain their literal bundled PATH directories.
    directories = dict.fromkeys(re.findall(rb"/nix/store/[a-z0-9]{32}-[^/\x00:\s\"']+/bin", Path(executable).read_bytes()))
    for directory in directories:
        path = os.fsdecode(directory)
        if store_root(path) != store_root(executable):
            package = re.split(r"-(?=\d)", Path(path).parent.name[33:], maxsplit=1)[0]
            entries.append(("bundled-" + package, "OMBP bundled " + package, path))
    python = os.environ.get("START_APP_PYTHON", sys.executable)
    dbus = os.environ.get("START_APP_DBUS")
    shell = os.environ.get("START_APP_SHELL")
    # Bin directories expose python/python3, dbus-daemon and sh/bash under /allowed-programs, not only the entrypoints.
    entries.extend((
        ("sandbox-python", "Sandbox Python", str(Path(python).parent)),
        ("sandbox-dbus", "Private D-Bus", dbus and str(Path(dbus).parent)),
        ("sandbox-keyring", "Private keyring", os.environ.get("START_APP_KEYRING")),
        ("sandbox-shell", "Sandbox shell (sh, bash)", shell and str(Path(shell).parent)),
        ("sandbox-coreutils", "Sandbox core utilities", os.environ.get("START_APP_COREUTILS")),
        ("sandbox-fonts", "Sandbox fonts", os.environ.get("START_APP_FONTCONFIG")),
        ("tool-podman", "Podman client (Code quality)", os.environ.get("START_APP_PODMAN")),
    ))
    optional = _optional_entries(selected)
    # Explicit tool environments (for example Python with debugpy) win command collisions.
    entries = entries + optional if selected is None else optional + entries
    if selected is not None:
        entries.extend((
            ("sandbox-dialog", "Sandbox file and folder picker", os.environ.get("START_APP_ZENITY")),
            ("agent-cli", "Selected agent command", agent_cli),
        ))
    if gpu == "amd":
        entries.append(("sandbox-mesa", "AMD Mesa drivers", os.environ.get("START_APP_MESA")))
    result = {}
    for key, label, value in entries:
        if not value:
            raise ValueError("Start through the supplied Nix environment for container runtime support")
        path = Path(value)
        programs = _programs(path)
        primary = {store_root(path), store_root(path.resolve(strict=True))}
        result[key] = {"label": label, "path": str(path), "roots": resolve_closure(str(path)),
                       "primary_roots": primary, "programs": programs}
    return result
