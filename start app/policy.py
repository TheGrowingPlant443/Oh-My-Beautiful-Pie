"""Host-owned live permissions. Never mount this state into the sandbox."""
import copy
import ipaddress
import json
import os
from pathlib import Path
import re
import shutil
import threading
import time
from urllib.parse import urlsplit

from catalog import DEFAULT_WEBSITES
from network import allowed_by_policy, auto_approved, covering_site
from requirements import WORKSPACE, prepare_workspace

DENIED_HISTORY = "denied.json"
AUTO_APPROVE = {"enabled": True, "sites": ["anthropic.com", "chatgpt.com", "google.com"]}
SITE_OPTIONS = {"dependencies": {}, "all_dependencies": [], "header_dependencies": [], "exact_hosts": []}


def origin(value):
    parsed = urlsplit(value.strip())
    if (parsed.scheme not in ("http", "https") or not parsed.hostname
            or parsed.username is not None or parsed.password is not None):
        raise ValueError("Use an http:// or https:// base URL without credentials")
    host = parsed.hostname.encode("idna").decode("ascii").lower().rstrip(".")
    try:
        address = ipaddress.ip_address(host)
        if not address.is_global:
            raise ValueError("Private and local network addresses are not allowed")
    except ValueError as error:
        if ":" in host or host.replace(".", "").isdigit():
            raise ValueError("Invalid or private network address") from error
    if host == "localhost" or host.endswith(".localhost") or any(c.isspace() for c in host):
        raise ValueError("Local or invalid host name")
    port = parsed.port
    if port is not None and not 1 <= port <= 65535:
        raise ValueError("Invalid port")
    if ":" in host:
        host = f"[{host}]"
    suffix = f":{port}" if port and port != {"http": 80, "https": 443}[parsed.scheme] else ""
    return f"{parsed.scheme}://{host}{suffix}"


class Policy:
    def __init__(self, directory):
        requested = Path(directory).expanduser().absolute()
        if requested.is_symlink():
            raise ValueError("State directory must not be a symlink")
        self.directory = requested.resolve()
        self.directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        if self.directory.stat().st_uid != os.getuid():
            raise ValueError("State directory must be owned by you")
        self.directory.chmod(0o700)
        self.private_paths = (str(self.directory), str(Path(__file__).parent.resolve()),
                              str(Path(os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}").resolve()))
        self.lock = threading.RLock()
        self.path = self.directory / "policy.json"
        self.data = {"folders": [], "programs": [], "websites": [], "credential_origins": [],
                     "internet": True, "allow_all_network": False, "disabled_requirements": [],
                     "folder_write_default": False, "auto_start": None,
                     **copy.deepcopy(SITE_OPTIONS), "auto_approve": copy.deepcopy(AUTO_APPROVE)}
        self.history = []
        self.history_error = ""
        self.history_generation = 0
        if self.path.exists():
            self.data = self._validated(json.loads(self.path.read_text()))
        if not (self.directory / "defaults.json").exists():
            defaults = self.snapshot()
            defaults["websites"] = sorted(set(defaults["websites"]) | set(DEFAULT_WEBSITES))
            if not any(item["path"] == WORKSPACE for item in defaults["folders"]):
                defaults["folders"].append({"path": WORKSPACE, "write": True})
            self.replace(defaults)
            self.save("defaults.json", 1)
        history_path = self.directory / DENIED_HISTORY
        if history_path.exists():
            self.history = json.loads(history_path.read_text())[-500:]
            self._prune_allowed()

    def snapshot(self):
        with self.lock:
            return copy.deepcopy(self.data)

    def save(self, name, value):
        temporary = self.directory / f".{name}.tmp"
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "w") as output:
            json.dump(value, output)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, self.directory / name)

    def _folder(self, item):
        if not isinstance(item, dict) or set(item) != {"path", "write"} or not isinstance(item["write"], bool):
            raise ValueError("Folder needs path and boolean write permission")
        path = Path(item["path"]).expanduser()
        if not path.is_absolute() or (path.exists() and not path.is_dir()) or path.resolve() != path:
            raise ValueError("Folder must be an absolute directory path without symlinks")
        if path == Path("/") or any(path.is_relative_to(p) for p in ("/proc", "/sys", "/dev", "/run", "/nix")):
            raise ValueError("System trees cannot be shared as folders")
        if any(path.is_relative_to(p) for p in self.private_paths):
            raise ValueError("Launcher state, code and runtime sockets cannot be shared")
        if str(path) == WORKSPACE:
            prepare_workspace(path)
        return {"path": str(path), "write": item["write"]}

    @staticmethod
    def _program(value_path):
        if not isinstance(value_path, str):
            raise ValueError("Program must be an executable name or path")
        if "/" not in value_path:
            installed = shutil.which(value_path)
            if installed is None:
                raise ValueError(f"Program is not installed on the host: {value_path}")
            value_path = installed
        path = Path(value_path).expanduser().resolve(strict=True)
        if not str(path).startswith("/nix/store/") or not path.is_file() or not os.access(path, os.X_OK):
            raise ValueError("Programs must be installed Nix executables (use their bin paths)")
        return str(path)

    def _validated(self, value):
        if not isinstance(value, dict):
            raise ValueError("Invalid permission fields")
        value = {"disabled_requirements": [], "allow_all_network": False, **copy.deepcopy(SITE_OPTIONS),
                 "auto_approve": copy.deepcopy(AUTO_APPROVE), "folder_write_default": False, "auto_start": None, **value}
        if set(value) != set(self.data):
            raise ValueError("Invalid permission fields")
        if not all(isinstance(value[key], bool) for key in ("internet", "allow_all_network", "folder_write_default")):
            raise ValueError("Switches must be booleans")
        if any(not isinstance(value[key], list) for key in ("folders", "programs", "websites", "credential_origins")):
            raise ValueError("Permission lists are required")
        folders = [self._folder(item) for item in value["folders"]]
        programs = {self._program(path) for path in value["programs"]}
        if len({Path(path).name for path in programs}) != len(programs):
            raise ValueError("Selected programs must have distinct executable names")
        websites = sorted({origin(url) for url in value["websites"]})
        credentials = sorted({origin(url) for url in value["credential_origins"]})
        if not set(credentials) <= set(websites):
            raise ValueError("Credential origins must also be allowed websites")
        if any(not site.startswith("https://") for site in credentials):
            raise ValueError("Host Authorization injection requires HTTPS")
        disabled = value["disabled_requirements"]
        if not isinstance(disabled, list) or any(
                not isinstance(key, str) or not re.fullmatch(r"[a-z][a-z0-9-]{0,79}", key) for key in disabled):
            raise ValueError("Invalid disabled runtime requirements")
        return {"folders": folders, "programs": sorted(programs),
                "websites": websites, "credential_origins": credentials, "internet": value["internet"],
                "allow_all_network": value["allow_all_network"], "folder_write_default": value["folder_write_default"],
                "auto_start": self._auto_start(value["auto_start"]),
                "disabled_requirements": sorted(set(disabled)), **self._site_options(value, websites),
                "auto_approve": self._auto_approve(value["auto_approve"])}

    @staticmethod
    def _auto_start(value):
        """None, or the launch settings to start with whenever the launcher starts."""
        if value is None:
            return None
        if isinstance(value, dict):
            value = {"agent": "omp", **value}  # Settings saved before the agent choice existed.
        if (not isinstance(value, dict) or set(value) != {"source", "mode", "desktop", "gpu", "agent"}
                or not isinstance(value["source"], str) or not value["source"].strip()
                or value["mode"] not in ("container", "normal") or not isinstance(value["desktop"], bool)
                or value["gpu"] not in ("none", "amd", "nvidia") or value["agent"] not in ("omp", "omsp")):
            raise ValueError("Invalid automatic start settings")
        # Same resolution as Runtime.start, so ~ and relative checkouts keep working at the next launch.
        return {**value, "source": str(Path(value["source"]).expanduser().resolve())}

    @staticmethod
    def _site_options(value, websites):
        lists = ("all_dependencies", "header_dependencies", "exact_hosts")
        dependencies = value["dependencies"]
        if (not isinstance(dependencies, dict) or any(not isinstance(origins, list) for origins in dependencies.values())
                or any(not isinstance(value[key], list) for key in lists)):
            raise ValueError("Invalid website options")
        options = {key: sorted({origin(site) for site in value[key]}) for key in lists}
        options["dependencies"] = {origin(site): sorted({origin(url) for url in origins})
                                   for site, origins in dependencies.items() if origins}
        if not set(options["dependencies"]).union(*(options[key] for key in lists)) <= set(websites):
            raise ValueError("Website options need their site to be allowed")
        return options

    @staticmethod
    def _auto_text(value):
        """A host fragment; full URLs from older settings keep only their host."""
        if not isinstance(value, str):
            raise ValueError("Auto-approve entries must be text")
        text = value.strip().lower()
        if "://" in text:
            text = urlsplit(text).hostname or ""
        if not re.fullmatch(r"[a-z0-9.-]{1,253}", text):
            raise ValueError("Auto-approve entries are host text such as chatgpt.com or openai")
        return text

    @classmethod
    def _auto_approve(cls, value):
        if (not isinstance(value, dict) or set(value) != {"enabled", "sites"} or not isinstance(value["enabled"], bool)
                or not isinstance(value["sites"], list)):
            raise ValueError("Invalid auto-approve settings")
        return {"enabled": value["enabled"], "sites": sorted({cls._auto_text(site) for site in value["sites"]})}

    def replace(self, value):
        data = self._validated(value)
        with self.lock:
            self.save("policy.json", data)
            self.data = data
            self._prune_allowed()

    def remember_website(self, origin_value):
        """List an auto-approved origin as an ordinary allowed website (subdomains on), once."""
        with self.lock:
            # Recheck under the lock: the user may have just turned auto-approve or internet off.
            if (not self.data["internet"] or covering_site(origin_value, self.data) is not None
                    or not auto_approved(origin_value, self.data)):
                return
            data = copy.deepcopy(self.data)
            data["websites"].append(origin_value)
            self.replace(data)

    def _prune_allowed(self):
        """Websites the current permissions allow are no longer "denied"; new denials still appear."""
        with self.lock:
            if not self.data["internet"]:
                return  # Offline, every website is still blocked.
            history = [row for row in self.history
                       if row["kind"] != "website" or not allowed_by_policy(self.data, row["target"])]
            if history == self.history:
                return
            self.history = history
            try:
                self.save(DENIED_HISTORY, history)
            except OSError as error:
                self.history_error = str(error)
            else:
                self.history_error = ""

    def deny(self, kind, target, reason, site=None):
        if kind not in ("folder", "program", "website"):
            return
        with self.lock:
            for row in self.history:
                if row["kind"] == kind and row["target"] == target and row.get("site") == site:
                    row.update(time=time.time(), count=row["count"] + 1, reason=str(reason)[:300])
                    break
            else:
                self.history.append({"kind": kind, "target": str(target)[:2048], "site": site,
                                     "reason": str(reason)[:300], "time": time.time(), "count": 1})
            self.history.sort(key=lambda row: row["time"])
            self.history = self.history[-500:]
            try:
                self.save(DENIED_HISTORY, self.history)
            except OSError as error:
                self.history_error = str(error)
            else:
                self.history_error = ""

    def events(self):
        with self.lock:
            return copy.deepcopy(list(reversed(self.history)))

    def clear_history(self):
        with self.lock:
            self.save(DENIED_HISTORY, [])
            self.history = []
            self.history_error = ""
            self.history_generation += 1

    def forget(self, items):
        """Drop history rows the user just approved; new attempts can appear again."""
        if not isinstance(items, list) or any(not isinstance(item, dict) for item in items):
            raise ValueError("Invalid history items")
        keys = {(item.get("kind"), item.get("target"), item.get("site")) for item in items}
        with self.lock:
            history = [row for row in self.history if (row["kind"], row["target"], row.get("site")) not in keys]
            self.save(DENIED_HISTORY, history)
            self.history = history
            self.history_error = ""
            self.history_generation += 1
