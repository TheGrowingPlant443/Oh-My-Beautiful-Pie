"""Read-only host suggestions; choosing one still goes through policy validation."""
import collections
import configparser
import os
from pathlib import Path
import re
import shlex
import shutil
import time

from packages import PACKAGE_CHOICES


DEFAULT_WEBSITES = ["https://github.com", "https://www.google.com", "https://stackoverflow.com", "https://www.atlassian.com"]


def detect_gpu(sysfs_root=Path("/sys/class/drm")):
    vendors = {"0x1002": "amd", "0x10de": "nvidia"}
    detected, boot = set(), set()
    root = Path(sysfs_root)
    for device in sorted(root.glob("*")):
        if not re.fullmatch(r"(?:card\d+|renderD\d+)", device.name):
            continue
        try:
            vendor = vendors.get((device / "device/vendor").read_text().strip().lower())
        except OSError:
            continue
        if vendor is None:
            continue
        detected.add(vendor)
        try:
            if device.name.startswith("card") and (device / "device/boot_vga").read_text().strip() == "1":
                boot.add(vendor)
        except OSError:
            pass
    return next((vendor for vendor in ("amd", "nvidia") if vendor in (boot or detected)), "none")


def _xdg_directory(environ, key, fallback):
    value = Path(environ.get(key) or fallback)
    return value if value.is_absolute() else Path(fallback)


def _folder_value(value, home):
    try:
        lexer = shlex.shlex(value, posix=True, punctuation_chars=";&|<>()")
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        return None
    if len(tokens) != 1:
        return None
    value = re.sub(r"^\$(?:HOME|\{HOME\})(?=/|$)", lambda _: str(home), tokens[0])
    if any(character in value for character in ("$", "`", "\x00", "\n", "\r")):
        return None
    path = Path(value)
    return path if path.is_absolute() and ".." not in path.parts else None


def user_folders(home=None, environ=None):
    home = Path(home) if home is not None else Path.home()
    environ = os.environ if environ is None else environ
    names = {"DESKTOP": "Desktop", "DOCUMENTS": "Documents", "DOWNLOAD": "Downloads",
             "PICTURES": "Pictures", "MUSIC": "Music", "VIDEOS": "Videos",
             "PUBLICSHARE": "Public", "TEMPLATES": "Templates"}
    paths = {key: home / name for key, name in names.items()}
    config = _xdg_directory(environ, "XDG_CONFIG_HOME", home / ".config") / "user-dirs.dirs"
    try:
        lines = config.read_text().splitlines()
    except (OSError, UnicodeError):
        lines = []
    for line in lines:
        name, separator, value = line.strip().partition("=")
        match = re.fullmatch(r"XDG_([A-Z]+)_DIR", name.strip())
        if separator and match and match[1] in paths:
            path = _folder_value(value.strip(), home)
            if path is not None:
                paths[match[1]] = path
    result = [{"name": "Home", "path": str(home)}]
    seen = {str(home)}
    for key, name in names.items():
        path = str(paths[key])
        if path not in seen:
            result.append({"name": name, "path": path})
            seen.add(path)
    return result


# Dot-names (.git, .cache, …) are skipped as hidden.
HEAVY_FOLDERS = {"node_modules", "__pycache__", "target"}


def _entries(directory):
    """Directory entries, lazily; unreadable folders have none."""
    try:
        with os.scandir(directory) as items:
            yield from items
    except OSError:
        return


def _visible_folder(item, private):
    try:
        return (not item.name.startswith(".") and item.name not in HEAVY_FOLDERS
                and item.is_dir(follow_symlinks=False)
                and not any(Path(item.path).is_relative_to(root) for root in private))
    except OSError:
        return False


def _walk_folders(home, private, entries=50000, seconds=0.3, depth_limit=6):
    """Breadth-first visible folders under home; every entry (files too) counts against the budget."""
    deadline, queue = time.monotonic() + seconds, collections.deque([(home, 0)])
    while queue:
        directory, depth = queue.popleft()
        for item in _entries(directory):
            entries -= 1
            if entries < 0 or time.monotonic() > deadline:
                return
            if _visible_folder(item, private):
                yield Path(item.path)
                if depth < depth_limit:
                    queue.append((Path(item.path), depth + 1))


def find_folders(query, home=None, private_paths=()):
    """Home folders whose name contains `query`; bounded so a keystroke never stalls the server."""
    query = query.strip().lower()
    if not query or "/" in query:
        return []
    home = Path(home) if home is not None else Path.home()
    matches = [path for path in _walk_folders(home, [Path(path) for path in private_paths])
               if query in path.name.lower()]
    matches.sort(key=lambda path: (not path.name.lower().startswith(query), len(str(path)), str(path)))
    return [{"name": path.name, "path": str(path)} for path in matches[:20]]


def resolve_program(command, search_path, store_root=Path("/nix/store")):
    """Resolve only executable files in the store, never run desktop commands."""
    if not command or any(character in command for character in ("%", "$", "`", "\x00")):
        return None
    if not Path(command).is_absolute():
        if "/" in command:
            return None
        command = shutil.which(command, path=search_path)
        if command is None:
            return None
    try:
        path = Path(command).resolve(strict=True)
        if path.is_relative_to(Path(store_root).resolve()) and path.is_file() and os.access(path, os.X_OK):
            return str(path)
    except (OSError, RuntimeError, ValueError):
        pass
    return None


def _desktop_program(path, search_path, store_root):
    parser = configparser.ConfigParser(interpolation=None)
    try:
        parser.read_string(path.read_text())
        entry = parser["Desktop Entry"]
        if (entry.get("Type") != "Application" or entry.getboolean("Hidden", fallback=False)
                or entry.getboolean("NoDisplay", fallback=False)):
            return None
        name = entry.get("Name", "").strip()
        command = shlex.split(entry.get("Exec", ""))
        if not name or not command or Path(command[0]).name == "env":
            return None
        executable = resolve_program(command[0], search_path, store_root)
        return {"name": name, "path": executable} if executable else None
    except (OSError, configparser.Error, KeyError, ValueError):
        return None


def _path_programs(bins, search_path, store_root):
    programs, commands = {}, set()
    for directory in bins:
        try:
            entries = sorted(directory.iterdir())
        except OSError:
            continue
        for entry in entries:
            if entry.name in commands:
                continue
            executable = resolve_program(str(entry), search_path, store_root)
            if executable:
                commands.add(entry.name)
                programs.setdefault(executable, {"name": entry.name, "path": executable})
    return programs


def _label_programs(programs, application_dirs, search_path, store_root):
    labeled = set()
    for directory in dict.fromkeys(map(Path, application_dirs)):
        for desktop in sorted(directory.glob("**/*.desktop")):
            item = _desktop_program(desktop, search_path, store_root)
            if item and item["path"] not in labeled:
                programs[item["path"]] = item
                labeled.add(item["path"])


def _profile_bins(home, environ):
    user = home.name
    state = _xdg_directory(environ, "XDG_STATE_HOME", home / ".local/state")
    return [home / ".nix-profile/bin", state / "nix/profile/bin",
            Path("/etc/profiles/per-user") / user / "bin",
            Path("/nix/var/nix/profiles/per-user") / user / "profile/bin",
            Path("/nix/var/nix/profiles/default/bin"), Path("/run/current-system/sw/bin")]


def _bins(profile_bins, environ):
    # Profile bins precede runtimeInputs injected into the launcher's PATH.
    return list(dict.fromkeys(profile_bins + [Path(path) for path in environ.get("PATH", "").split(os.pathsep)
                                              if path and Path(path).is_absolute()]))


def host_program(names, home=None, environ=None):
    """First of `names` the user has installed as a Nix store executable, else None."""
    home = Path(home) if home is not None else Path.home()
    environ = os.environ if environ is None else environ
    search_path = os.pathsep.join(map(str, _bins(_profile_bins(home, environ), environ)))
    return next(filter(None, (resolve_program(name, search_path) for name in names)), None)


def installed_programs(home=None, environ=None, store_root=Path("/nix/store"),
                       profile_bins=None, application_dirs=None):
    home = Path(home) if home is not None else Path.home()
    environ = os.environ if environ is None else environ
    if profile_bins is None:
        profile_bins = _profile_bins(home, environ)
    profile_bins = [Path(path) for path in profile_bins]
    bins = _bins(profile_bins, environ)
    search_path = os.pathsep.join(map(str, bins))
    programs = _path_programs(bins, search_path, store_root)
    if application_dirs is None:
        data = _xdg_directory(environ, "XDG_DATA_HOME", home / ".local/share")
        data_dirs = [Path(path) for path in environ.get("XDG_DATA_DIRS", "/usr/local/share:/usr/share").split(os.pathsep)
                     if path and Path(path).is_absolute()]
        application_dirs = [data / "applications"] + [directory.parent / "share/applications" for directory in profile_bins]
        application_dirs += [directory / "applications" for directory in data_dirs]
    _label_programs(programs, application_dirs, search_path, store_root)
    result, names = [], set()
    for item in sorted(programs.values(), key=lambda item: (item["name"].casefold(), item["path"])):
        name = item["name"].casefold()
        if name not in names:
            result.append(item)
            names.add(name)
    return result


def catalog():
    return {"gpu": detect_gpu(), "folders": user_folders(), "programs": installed_programs(),
            "packages": PACKAGE_CHOICES}
