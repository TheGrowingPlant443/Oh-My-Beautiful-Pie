"""One rootless container; filtered quality engine, no host session bus or host network."""
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import subprocess
import socketserver
import stat
import sys
import tempfile
import threading
import time
import uuid

from access_fs import AccessFS
from engine import EngineGateway, pinned_images, seed_client
from installed import build_packages, install_profile
from network import NetworkGateway
from requirements import DISPLAY_SOCKET, FOLDERS, WORKSPACE, bundles, prepare_workspace

CONTAINER_RECORD = "container.json"
TRUST_CA = "/trust/ca.pem"
BROWSER_DISPLAY = "/browser-display"
PROXY_URL = "http://127.0.0.1:18080"
# The container has --network=none, so loopback names only reach processes inside the sandbox.
LOOPBACK = "localhost,.localhost,127.0.0.1,::1,[::1]"
KEYRING_UNLOCK = "keyring-unlock.json"


def _terminate(process, timeout):
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=timeout)


def closure(program):
    path = Path(program).resolve(strict=True)
    if not str(path).startswith("/nix/store/"):
        raise ValueError(f"Expected a Nix executable: {path}")
    root = "/" + "/".join(path.parts[1:4])
    result = subprocess.run(["nix-store", "--query", "--requisites", root],
                            check=True, text=True, capture_output=True, timeout=120)
    return result.stdout.splitlines()


class _BrowserRequest(socketserver.BaseRequestHandler):
    def handle(self):
        self.server.display.serve(self.request)


class BrowserDisplay:
    """A second kiosk desktop that runs only while some sandbox browser holds a request connection."""

    def __init__(self, directory, spawn):
        self.directory = directory
        self.spawn = spawn
        self.process = None
        self.clients = 0
        self.closed = False
        self.lock = threading.Lock()
        directory.mkdir(mode=0o700, exist_ok=True)
        path = directory / "request.sock"
        path.unlink(missing_ok=True)
        self.server = socketserver.ThreadingUnixStreamServer(str(path), _BrowserRequest)
        self.server.daemon_threads = True
        self.server.display = self
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def serve(self, connection):
        with self.lock:
            self.clients += 1
            ready = not self.closed
            try:
                if ready and (self.process is None or self.process.poll() is not None):
                    self.process = self.spawn(self.directory, "wayland-browser")
            except (OSError, RuntimeError, ValueError):
                ready = False
        try:
            if ready:
                connection.sendall(b"ready\n")
                # Any bytes are ignored; EOF means the browser (holding this socket) has exited.
                while connection.recv(4096):
                    continue
        except OSError:
            pass
        finally:
            with self.lock:
                self.clients -= 1
                if not self.clients:
                    _terminate(self.process, 5)
                    self.process = None

    def stop(self):
        self.server.shutdown()
        self.server.server_close()
        (self.directory / "request.sock").unlink(missing_ok=True)
        with self.lock:
            self.closed = True
            _terminate(self.process, 5)
            self.process = None



class Runtime:
    def __init__(self, policy):
        self.policy = policy
        self.process = None
        self.container = None
        self.gateway = None
        self.engine = None
        self.browser_trust = None
        self.browser_display = None
        self.fs_thread = None
        self.mount = policy.directory / "access"
        self.last_error = ""
        self.mode = None
        self.launch = None
        self.pending_launch = None
        self.installed_profile = None
        self.lock = threading.RLock()
        self.build_lock = threading.Lock()
        self.build_process = None
        self.closing = False
        self.log = None
        self.desktop_process = None
        manifest = policy.directory / "requirements.json"
        self.requirements = json.loads(manifest.read_text()) if manifest.exists() else [
            {"id": key, "label": label, "path": path, "kind": "folder"} for key, label, path in FOLDERS]
        record = policy.directory / CONTAINER_RECORD
        if record.exists():
            name = json.loads(record.read_text())
            if not isinstance(name, str) or not name.startswith("ombp-start-") or not name[11:].isalnum():
                raise ValueError("Invalid owned-container record")
            inspected = subprocess.run(["podman", "container", "inspect", name], capture_output=True, text=True, timeout=30)
            if inspected.returncode == 0:
                labels = json.loads(inspected.stdout)[0]["Config"]["Labels"]
                if labels.get("app") != "ombp-start":
                    raise RuntimeError("Recorded container no longer belongs to this launcher")
            self.container = name
        self.stop()
        socket = policy.directory / "network/socket/proxy.sock"
        if socket.is_socket():
            socket.unlink()

    def status(self):
        # start() holds the lock for the whole build; report that instead of stalling the UI.
        if not self.lock.acquire(blocking=False):
            return {"running": False, "starting": True, "mode": self.mode,
                    "launch": self.pending_launch or self.launch, "container": self.container, "error": ""}
        try:
            return {"running": self.process is not None and self.process.poll() is None, "starting": False,
                    "mode": self.mode, "launch": self.launch, "container": self.container, "error": self.last_error}
        finally:
            self.lock.release()

    def requirements_status(self):
        snapshot = self.policy.snapshot()
        active = self.pending_launch or (self.launch if self.process is not None else None)
        installed = active["installed_mode"] if active else snapshot["installed_mode"]
        return [{**item, "enabled": (installed and (item["kind"] == "program" or item["id"] == "nix-store"))
                 or item["id"] not in snapshot["disabled_requirements"]} for item in self.requirements]

    @staticmethod
    def _launch_source(source, mode, desktop, gpu, agent):
        """The resolved OMBP checkout, once every launch setting is valid."""
        source = Path(source).expanduser().resolve(strict=True)
        if not source.is_dir() or not (source / "flake.nix").is_file():
            raise ValueError("Choose the OMBP source folder containing flake.nix")
        if (mode not in ("normal", "container") or gpu not in ("none", "amd", "nvidia")
                or not isinstance(desktop, bool) or agent not in ("omp", "omsp")):
            raise ValueError("Invalid launch settings")
        return source

    def _check_closing(self):
        if self.closing:
            raise RuntimeError("Launch cancelled: launcher is closing")

    def _run_prepare(self, command, cwd=None, label="OMBP build"):
        """All package builds/copies share the same cancellable child tracked by close()."""
        with self.build_lock:
            self._check_closing()
            child = self.build_process = subprocess.Popen(
                command, cwd=cwd, stdout=subprocess.PIPE, stderr=self.log, text=True)
        try:
            output, _ = child.communicate()
        finally:
            with self.build_lock:
                self.build_process = None
        self._check_closing()
        if child.returncode:
            raise RuntimeError(f"{label} failed; see {self.policy.directory / 'launch.log'}")
        return output

    def start(self, source, mode, desktop, gpu, agent):
        with self.lock:
            if self.status()["running"]:
                raise ValueError("Already running")
            self.stop()
            source = self._launch_source(source, mode, desktop, gpu, agent)
            self.mode = mode
            self.launch = None
            settings = self.policy.snapshot()
            self.pending_launch = {
                "source": str(source), "desktop": desktop, "gpu": gpu, "agent": agent,
                "installed_mode": mode == "container" and settings["installed_mode"],
                "selected_packages": settings["selected_packages"] if mode == "container" else [],
            }
            self.last_error = ""
            self.log = open(self.policy.directory / "launch.log", "ab", buffering=0)
            try:
                output = self._run_prepare(
                    ["nix", "build", f"path:.#{'default' if agent == 'omp' else 'omsp'}", "--no-link",
                     "--print-out-paths", "--no-write-lock-file"], cwd=source)
                outputs = output.splitlines()
                if len(outputs) != 1 or not outputs[0].startswith("/nix/store/"):
                    raise RuntimeError("OMBP build did not return one Nix store output")
                executable = str((Path(outputs[0]) / "bin/omp-manager").resolve(strict=True))
                if not Path(executable).is_file() or not os.access(executable, os.X_OK):
                    raise ValueError("OMBP build has no executable bin/omp-manager")
                self.launch = {**self.pending_launch, "executable": executable}
                if mode == "normal":
                    with self.build_lock:
                        self._check_closing()
                        self.process = subprocess.Popen([executable], stdout=self.log, stderr=self.log,
                                                        env={**os.environ, "OMBP_AGENT": agent},
                                                        start_new_session=True)
                else:
                    self._container_start(executable, desktop, gpu, source, agent, self.pending_launch)
                threading.Thread(target=self._watch, args=(self.process,), daemon=True).start()
            except Exception:
                self.stop()
                raise
            finally:
                self.pending_launch = None

    def _container_bundles(self, executable, gpu, source, agent, installation):
        selected = agent_cli = None
        prepare = lambda command, cwd=None: self._run_prepare(command, cwd, "Container installation")
        if installation["installed_mode"]:
            agent_cli, selected = build_packages(installation["selected_packages"], agent, source, prepare)
        base = bundles(executable, gpu, closure, selected=selected, agent_cli=agent_cli)
        if installation["installed_mode"]:
            self.installed_profile = install_profile(self.policy.directory, base, prepare, self._check_closing)
        return base

    def _record_requirements(self, base, desktop):
        self.requirements = [
            {"id": key, "label": item["label"], "path": item["path"], "kind": "program"}
            for key, item in base.items()
        ] + [{"id": key, "label": label, "path": path, "kind": "folder"} for key, label, path in FOLDERS]
        if desktop:
            self.requirements.append({"id": "desktop-socket", "label": "Isolated desktop socket",
                                      "path": DISPLAY_SOCKET, "kind": "folder"})
        self.policy.save("requirements.json", self.requirements)

    def _container_start(self, executable, desktop, gpu, source, agent, installation):
        info = json.loads(subprocess.run(["podman", "info", "--format", "json"],
                          check=True, text=True, capture_output=True, timeout=30).stdout)
        if not info["host"]["security"]["rootless"] or info["host"].get("serviceIsRemote"):
            raise ValueError("Only local rootless Podman is supported")
        python = os.environ.get("START_APP_PYTHON", sys.executable)
        dbus = os.environ.get("START_APP_DBUS", shutil.which("dbus-run-session"))
        keyring = os.environ.get("START_APP_KEYRING", shutil.which("gnome-keyring-daemon"))
        shell = os.environ.get("START_APP_SHELL")
        coreutils = os.environ.get("START_APP_COREUTILS")
        fonts = os.environ.get("START_APP_FONTCONFIG")
        if not all((dbus, keyring, shell, coreutils, fonts)):
            raise ValueError("Start through the supplied Nix environment for container runtime support")
        installed = installation["installed_mode"]
        base = self._container_bundles(executable, gpu, source, agent, installation)
        self._record_requirements(base, desktop)
        mesa = os.environ.get("START_APP_MESA")
        snapshot = self.policy.snapshot()
        disabled = snapshot["disabled_requirements"]
        workspace = any(item["path"] == WORKSPACE for item in snapshot["folders"])
        if workspace:
            prepare_workspace()
        password = self._keyring_password()
        self._access_start({} if installed else base)
        self.gateway = NetworkGateway(self.policy, self.policy.directory / "network")
        self.gateway.start()
        root = self._rootfs(shell, coreutils)
        home = self.policy.directory / "home"
        home.mkdir(exist_ok=True, mode=0o700)
        browser = base.get("package-chromium" if installed else "tool-chrome")
        if browser and "sandbox-home" not in disabled and "proxy-ca" not in disabled:
            self._browser_trust_start(home)
        self.container = f"ombp-start-{uuid.uuid4().hex[:12]}"
        # Leave container root unmapped: crun then sets up FUSE mounts as their host owner.
        self.policy.save(CONTAINER_RECORD, self.container)
        args = ["podman", "run", "--rm", "--interactive", "--name", self.container, "--label", "app=ombp-start",
                "--http-proxy=false", "--hostname=ombp-sandbox",
                "--network=none", f"--uidmap={os.getuid()}:0:1", f"--gidmap={os.getgid()}:0:1",
                f"--user={os.getuid()}:{os.getgid()}",
                "--cap-drop=ALL", "--security-opt=no-new-privileges", "--read-only", "--read-only-tmpfs=false",
                # Podman's default seccomp profile blocks Chrome's user-namespace zygote sandbox.
                "--security-opt=seccomp=unconfined",
                "--pid=private", "--ipc=private", "--pids-limit=4096", "--init",
                "--workdir=/home/sandbox"]
        if "sandbox-tmp" not in disabled:
            args.append("--tmpfs=/tmp:rw,nosuid,nodev,notmpcopyup")
        if "sandbox-run" not in disabled:
            args.append("--tmpfs=/run:rw,nosuid,nodev,mode=1777,notmpcopyup")

        self._mount_resources(args, home, disabled)
        if workspace:
            # A real mount keeps OMBP's canonical paths unchanged; access still passes the revocable gateway.
            self._mount(args, self.mount / "host" / WORKSPACE.lstrip("/"), WORKSPACE, True)
        env = {"HOME": "/home/sandbox", "USER": "sandbox", "LOGNAME": "sandbox",
               "XDG_RUNTIME_DIR": "/run/ombp", "XDG_CONFIG_HOME": "/home/sandbox/.config",
               "PATH": ":".join(("/installed-programs" if installed else "/allowed-programs",
                                 str(Path(python).parent), str(Path(shell).parent),
                                 coreutils, str(Path(dbus).parent), str(Path(keyring).parent))),
               "HTTP_PROXY": PROXY_URL, "HTTPS_PROXY": PROXY_URL,
               "ALL_PROXY": PROXY_URL, "NO_PROXY": LOOPBACK,
               "http_proxy": PROXY_URL, "https_proxy": PROXY_URL,
               "all_proxy": PROXY_URL, "no_proxy": LOOPBACK, "NODE_USE_ENV_PROXY": "1",
               "SSL_CERT_FILE": TRUST_CA, "NODE_EXTRA_CA_CERTS": TRUST_CA,
               "REQUESTS_CA_BUNDLE": TRUST_CA, "CURL_CA_BUNDLE": TRUST_CA,
               "OMBP_GIT_DISCOVERY_HOME": "/host" + str(Path.home()),
               "START_APP_KEYRING": keyring, "SHELL": shell, "FONTCONFIG_FILE": fonts,
               "GDK_BACKEND": "wayland", "NIXOS_OZONE_WL": "1"}
        env.update(self._program_environment(installed, agent, browser))
        if "sonar-engine" not in disabled:
            sonar_port = seed_client(home)
            self.engine = EngineGateway(self.policy, self.policy.directory / "engine",
                                        pinned_images(source), sonar_port, self.log).start()
            self._mount(args, self.engine.api_dir, "/engine")
            env["START_APP_SONAR_PORT"] = str(sonar_port)
        self._mount_displays(args, env, desktop, disabled, browser)
        self._gpu_access(args, env, gpu, mesa)
        for key, value in env.items():
            args.extend(["--env", f"{key}={value}"])
        args.extend(["--rootfs", str(root), dbus, "--config-file=/etc/session.conf", "--",
                     python, "/launcher/session.py", executable])
        with self.build_lock:
            self._check_closing()
            self.process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=self.log, stderr=self.log,
                                            start_new_session=True)
        with self.process.stdin as stream:
            stream.write((password + "\n").encode())

    @staticmethod
    def _program_environment(installed, agent, browser):
        env = {"OMP_BIN": f"/installed-programs/{agent}" if installed else "/launcher/omp", "OMBP_AGENT": agent}
        if installed and agent == "omsp":
            env["OMSP_CONFIG_DIR"] = ".omp"
        if browser:
            commands = browser["programs"]
            env["START_APP_BROWSER"] = next((path for path in commands if Path(path).name == "google-chrome-stable"),
                                            commands[0])
        return env

    def _mount_displays(self, args, env, desktop, disabled, browser):
        if not desktop or "desktop-socket" in disabled:
            return
        socket = self._desktop_start()
        self._mount(args, socket, DISPLAY_SOCKET)
        env["WAYLAND_DISPLAY"] = DISPLAY_SOCKET
        env["XDG_SESSION_TYPE"] = "wayland"
        if browser:
            self.browser_display = BrowserDisplay(self.policy.directory / "browser-display", self._weston)
            self._mount(args, self.browser_display.directory, BROWSER_DISPLAY)

    @staticmethod
    def _mount(args, source, target, writable=False):
        # Podman --mount is comma-delimited; rejecting commas avoids option injection.
        if "," in str(source) or ":" in str(source):
            raise ValueError("Mount paths cannot contain commas or colons")
        args.extend(["--volume", f"{source}:{target}:{'rw' if writable else 'ro'}"])

    def _mount_resources(self, args, home, disabled):
        if self.installed_profile:
            self._mount(args, self.installed_profile / "store", "/nix/store")
            self._mount(args, self.installed_profile / "programs", "/installed-programs")
        for key, source, target, writable in (
                ("nix-store", self.mount / "store", "/nix/store", False),
                ("sandbox-home", home, "/home/sandbox", True),
                ("launcher-files", Path(__file__).parent.resolve(), "/launcher", False),
                ("network-socket", Path(self.gateway.socket_path).parent, "/network", False),
                ("proxy-ca", self.gateway.ca_path, TRUST_CA, False)):
            if key not in disabled and not (key == "nix-store" and self.installed_profile):
                self._mount(args, source, target, writable)
        if self.browser_trust:
            # Chrome uses NSS_InitReadWrite and silently drops local trust on a read-only database.
            self._mount(args, Path(self.browser_trust.name) / ".pki", "/home/sandbox/.pki", True)
        host_omp = self._host_omp(disabled)
        if host_omp:
            # User-approved direct bind: SQLite locks, mmap and hardlinks behave exactly as for host OMP.
            self._mount(args, host_omp, "/home/sandbox/.omp", True)
        self._mount(args, self.mount / "host", "/host", True)
        if not self.installed_profile:
            self._mount(args, self.mount / "programs", "/allowed-programs")

    def _host_omp(self, disabled):
        path = Path.home() / ".omp"
        if "host-omp" in disabled or path.is_symlink() or not path.is_dir():
            return None
        path = path.resolve()
        for private in map(Path, self.policy.private_paths):
            if path.is_relative_to(private) or private.is_relative_to(path):
                # The direct bind bypasses the gateway, so it must never expose keys, policy or runtime sockets.
                raise ValueError(f"~/.omp overlaps protected launcher path {private}; move it outside ~/.omp "
                                 "or revoke 'Your OMP folder' under Folder access")
        return path

    def _browser_trust_start(self, home):
        certutil = os.environ.get("START_APP_CERTUTIL")
        if not certutil:
            raise ValueError("Start through the supplied Nix environment for private browser CA trust")
        descriptor = os.open(home, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            try:
                os.mkdir(".pki", mode=0o700, dir_fd=descriptor)
            except FileExistsError:
                pass
            target = os.open(".pki", os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            os.close(target)
        finally:
            os.close(descriptor)
        # Never let host NSS load sandbox-authored PKCS#11 modules or database files.
        self.browser_trust = tempfile.TemporaryDirectory(prefix="browser-trust-", dir=self.policy.directory)
        scratch = Path(self.browser_trust.name)
        database = scratch / ".pki/nssdb"
        database.mkdir(parents=True, mode=0o700)
        environment = {"HOME": str(scratch), "PATH": os.defpath}
        for command in (
                ["-N", "--empty-password", "-d", f"sql:{database}"],
                ["-A", "-n", "OMBP sandbox proxy CA", "-t", "C,,",
                 "-i", str(self.gateway.ca_path), "-d", f"sql:{database}"]):
            try:
                subprocess.run([certutil, *command], env=environment, cwd=scratch,
                               stdout=self.log, stderr=self.log, check=True, timeout=30)
            except (OSError, subprocess.SubprocessError) as error:
                raise RuntimeError("Could not initialize private browser CA trust; see launch.log") from error
        # NSS persists configdir in its module configuration; relocate only our fresh trusted file.
        modules = database / "pkcs11.txt"
        modules.write_text(modules.read_text().replace(str(database), "/home/sandbox/.pki/nssdb"))

    def _keyring_password(self):
        path = self.policy.directory / KEYRING_UNLOCK
        if not path.exists() and not path.is_symlink():
            home = self.policy.directory / "home"
            if home.is_symlink() or (home.exists() and (not home.is_dir() or any(home.iterdir()))):
                raise ValueError("Existing private home has no keyring unlock key; restore its matching key or use a new state directory")
            self.policy.save(KEYRING_UNLOCK, secrets.token_urlsafe(48))
        return self._read_keyring_password(path)

    @staticmethod
    def _read_keyring_password(path):
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        try:
            metadata = os.fstat(descriptor)
            if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
                    or metadata.st_mode & 0o077 or metadata.st_size > 256):
                raise ValueError("Private keyring unlock key must be a private, owner-only regular file")
            password = json.loads(os.read(descriptor, 257))
        finally:
            os.close(descriptor)
        if not isinstance(password, str) or re.fullmatch(r"[A-Za-z0-9_-]{64}", password) is None:
            raise ValueError("Invalid private keyring unlock key")
        return password

    def _access_start(self, base):
        from fuse import FUSE
        self.mount.mkdir(exist_ok=True)
        self.fs_error = None
        fs = AccessFS(self.policy, base, closure)

        def mount_fs():
            try:
                FUSE(fs, str(self.mount), foreground=True, nothreads=False, allow_other=False,
                     raw_fi=True, attr_timeout=0, entry_timeout=0, negative_timeout=0)
            except Exception as error:
                self.fs_error = error
        self.fs_thread = threading.Thread(target=mount_fs, daemon=True)
        self.fs_thread.start()
        deadline = time.monotonic() + 10
        while not os.path.ismount(self.mount):
            if self.fs_error:
                raise RuntimeError(f"FUSE mount failed: {self.fs_error}")
            if time.monotonic() >= deadline:
                raise RuntimeError("FUSE mount did not become ready; check /dev/fuse and fusermount")
            time.sleep(0.05)

    def _rootfs(self, shell, coreutils):
        root = self.policy.directory / "rootfs"
        for directory in ("etc", "nix/store", "host", "allowed-programs", "installed-programs", "launcher", "network", "engine", "trust", "home/sandbox", "tmp", "run", "proc", "dev"):
            (root / directory).mkdir(parents=True, exist_ok=True)
        (root / "etc/passwd").write_text(f"sandbox:x:{os.getuid()}:{os.getgid()}:Sandbox:/home/sandbox:/bin/sh\n")
        (root / "etc/group").write_text(f"sandbox:x:{os.getgid()}:\n")
        (root / "etc/nsswitch.conf").write_text("hosts: files dns\n")
        machine_id = root / "etc/machine-id"
        if not re.fullmatch(r"[0-9a-f]{32}\n?", machine_id.read_text() if machine_id.is_file() else ""):
            # D-Bus/GLib tools and Chrome need one; random per state, never the host's.
            machine_id.write_text(secrets.token_hex(16) + "\n")
        (root / "etc/session.conf").write_text(
            '<busconfig><type>session</type><listen>unix:tmpdir=/tmp</listen><auth>EXTERNAL</auth>'
            '<policy context="default"><allow send_destination="*"/><allow receive_sender="*"/>'
            '<allow own="*"/></policy></busconfig>')
        for directory in ("bin", "usr/bin", "display", "browser-display", "var"):
            (root / directory).mkdir(exist_ok=True, parents=True)
        # Engine subprocesses discard TMPDIR; their default /var/tmp must reach private writable scratch.
        temporary = root / "var/tmp"
        if temporary.is_dir() and not temporary.is_symlink():
            temporary.rmdir()
        for target, source in (("bin/sh", shell), ("bin/bash", shell),
                               ("usr/bin/env", coreutils + "/env"), ("var/tmp", "../tmp")):
            link = root / target
            link.unlink(missing_ok=True)
            link.symlink_to(source)
        return root

    @staticmethod
    def _gpu_access(args, env, gpu, mesa):
        if gpu == "amd":
            devices = sorted(Path("/dev/dri").glob("renderD*"))
            if not devices:
                raise ValueError("No AMD render device available")
            for device in devices:
                args.extend(["--device", str(device)])
            if Path("/dev/kfd").exists():
                args.extend(["--device", "/dev/kfd"])
            args.extend(["--group-add=keep-groups"])
            if mesa:
                env["LIBGL_DRIVERS_PATH"] = f"{mesa}/lib/dri"
        elif gpu == "nvidia":
            args.extend(["--device=nvidia.com/gpu=all", "--group-add=keep-groups"])

    def _desktop_start(self):
        directory = self.policy.directory / "display"
        self.desktop_process = self._weston(directory, "wayland-ombp")
        return directory / "wayland-ombp"

    def _weston(self, directory, name):
        """Start a nested kiosk desktop serving `directory/name`; return its process once the socket exists."""
        display = os.environ.get("WAYLAND_DISPLAY", "")
        host_runtime = Path(os.environ.get("XDG_RUNTIME_DIR", "/nonexistent"))
        host_socket = host_runtime / display
        weston = os.environ.get("START_APP_WESTON")
        if not weston or not display or not host_socket.is_socket():
            raise ValueError("Nested desktop requires the Nix launcher and a local Wayland session")
        directory.mkdir(mode=0o700, exist_ok=True)
        socket = directory / name
        socket.unlink(missing_ok=True)
        env = {key: os.environ[key] for key in ("PATH", "LANG", "LOCALE_ARCHIVE") if key in os.environ}
        env.update(HOME=str(directory), XDG_RUNTIME_DIR=str(directory), WAYLAND_DISPLAY=str(host_socket))
        if name == "wayland-browser":
            # OMBP stays borderless; the kiosk patch independently maximizes Zenity pickers.
            env["OMBP_KIOSK_MAXIMIZE"] = "1"
        process = subprocess.Popen(
            [sys.executable, str(Path(__file__).parent / "desktop.py"), str(os.getpid()), weston,
             "--backend=wayland", "--renderer=pixman", "--shell=kiosk-shell.so",
             f"--socket={name}", "--width=1440", "--height=900", "--idle-time=0", "--no-config"],
            env=env, stdout=self.log, stderr=self.log, start_new_session=True)
        deadline = time.monotonic() + 15
        while not socket.is_socket():
            if process.poll() is not None or time.monotonic() >= deadline:
                _terminate(process, 5)
                raise RuntimeError("Nested desktop did not start; inspect launch.log")
            time.sleep(0.05)
        return process

    def _watch(self, process):
        code = process.wait()
        # Closing the nested desktop window (for example Super+Q) ends OMBP's display: a stop, not a crash.
        closed = False
        desktop = self.desktop_process
        if code and desktop is not None:
            try:
                closed = desktop.wait(timeout=2) == 0
            except subprocess.TimeoutExpired:
                pass
        with self.lock:
            if self.process is process:
                if code and not closed:
                    self.last_error = f"Process exited {code}; see {self.policy.directory / 'launch.log'}"
                self.stop()

    def close(self):
        # Cancel before taking the lifecycle lock: start() holds it while Nix builds.
        with self.build_lock:
            self.closing = True
            build = self.build_process
        _terminate(build, 10)
        self.stop()

    def stop(self):
        with self.lock:
            container = self.container
            if container:
                self._remove_container(container, 10)
            if self.process and self.mode == "normal":
                try:
                    os.killpg(self.process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            _terminate(self.process, 10)
            if container:
                # podman run can create the container after the first remove; retire it after the client exits.
                self._remove_container(container, 0)
                self.container = None
                (self.policy.directory / CONTAINER_RECORD).unlink(missing_ok=True)
            self.process = None
            self.installed_profile = None
            _terminate(self.desktop_process, 5)
            self.desktop_process = None
            if self.browser_display:
                self.browser_display.stop()
                self.browser_display = None
            if self.gateway:
                self.gateway.stop()
                self.gateway = None
            if self.engine:
                self.engine.stop()
                self.engine = None
            if self.browser_trust:
                self.browser_trust.cleanup()
                self.browser_trust = None
            if os.path.ismount(self.mount):
                helper = next((p for p in ("/usr/bin/fusermount3", "/usr/bin/fusermount") if Path(p).exists()), "fusermount")
                subprocess.run([helper, "-u", str(self.mount)], check=True, timeout=10, capture_output=True)
            if self.fs_thread:
                self.fs_thread.join(timeout=3)
                self.fs_thread = None
            if self.log:
                self.log.close()
                self.log = None

    @staticmethod
    def _remove_container(name, timeout):
        result = subprocess.run(["podman", "rm", "--force", "--ignore", f"--time={timeout}", name],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout + 20)
        if result.returncode:
            raise RuntimeError("Failed to stop container: " + result.stderr.decode(errors="replace"))
