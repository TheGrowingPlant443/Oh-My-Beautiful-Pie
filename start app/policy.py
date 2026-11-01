"""Host-owned live permissions. Never mount this state into the sandbox."""
import copy
import ipaddress
import json
import os
from pathlib import Path
import shutil
import threading
import time
from urllib.parse import urlsplit


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
        self.data = {"folders": [], "programs": [], "websites": [], "credential_origins": [], "internet": False}
        self.history = []
        self.history_error = ""
        if self.path.exists():
            self.data = self._validated(json.loads(self.path.read_text()))
        history_path = self.directory / "denied.json"
        if history_path.exists():
            self.history = json.loads(history_path.read_text())[-500:]

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
        if not isinstance(value, dict) or set(value) != set(self.data):
            raise ValueError("Invalid permission fields")
        if not isinstance(value["internet"], bool):
            raise ValueError("Internet must be a boolean")
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
        return {"folders": folders, "programs": sorted(programs),
                "websites": websites, "credential_origins": credentials, "internet": value["internet"]}

    def replace(self, value):
        data = self._validated(value)
        with self.lock:
            self.save("policy.json", data)
            self.data = data

    def deny(self, kind, target, reason):
        if kind not in ("folder", "program", "website"):
            return
        with self.lock:
            for row in self.history:
                if row["kind"] == kind and row["target"] == target:
                    row.update(time=time.time(), count=row["count"] + 1, reason=str(reason)[:300])
                    break
            else:
                self.history.append({"kind": kind, "target": str(target)[:2048],
                                     "reason": str(reason)[:300], "time": time.time(), "count": 1})
            self.history.sort(key=lambda row: row["time"])
            self.history = self.history[-500:]
            try:
                self.save("denied.json", self.history)
            except OSError as error:
                self.history_error = str(error)
            else:
                self.history_error = ""

    def events(self):
        with self.lock:
            return copy.deepcopy(list(reversed(self.history)))
