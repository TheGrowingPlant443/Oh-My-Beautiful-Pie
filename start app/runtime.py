"""One rootless container; no host OMP, engine socket, session bus or host network."""
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import subprocess
import stat
import sys
import threading
import time
import uuid

from access_fs import AccessFS
from network import NetworkGateway

CONTAINER_RECORD = "container.json"
TRUST_CA = "/trust/ca.pem"
PROXY_URL = "http://127.0.0.1:18080"
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


class Runtime:
    def __init__(self, policy):
        self.policy = policy
        self.process = None
        self.container = None
        self.gateway = None
        self.fs_thread = None
        self.mount = policy.directory / "access"
        self.last_error = ""
        self.mode = None
        self.launch = None
        self.lock = threading.RLock()
        self.log = None
        self.desktop_process = None
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
        with self.lock:
            return {"running": self.process is not None and self.process.poll() is None,
                    "mode": self.mode, "launch": self.launch, "container": self.container, "error": self.last_error}

    def start(self, executable, mode, desktop, gpu):
        with self.lock:
            if self.status()["running"]:
                raise ValueError("Already running")
            self.stop()
            executable = str(Path(executable).expanduser().resolve(strict=True))
            if not os.access(executable, os.X_OK) or not Path(executable).is_file():
                raise ValueError("Choose the installed omp-manager executable")
            if mode not in ("normal", "container") or gpu not in ("none", "amd", "nvidia") or not isinstance(desktop, bool):
                raise ValueError("Invalid launch settings")
            self.mode = mode
            self.launch = {"executable": executable, "desktop": desktop, "gpu": gpu}
            self.last_error = ""
            self.log = open(self.policy.directory / "launch.log", "ab", buffering=0)
            try:
                if mode == "normal":
                    self.process = subprocess.Popen([executable], stdout=self.log, stderr=self.log,
                                                    start_new_session=True)
                else:
                    self._container_start(executable, desktop, gpu)
                threading.Thread(target=self._watch, args=(self.process,), daemon=True).start()
            except Exception:
                self.stop()
                raise

    def _container_start(self, executable, desktop, gpu):
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
        base = set()
        for program in (executable, python, dbus, keyring, shell, coreutils, fonts):
            base.update(closure(program))
        mesa = os.environ.get("START_APP_MESA")
        if gpu == "amd" and mesa:
            base.update(closure(mesa))
        password = self._keyring_password()
        self._access_start(base)
        self.gateway = NetworkGateway(self.policy, self.policy.directory / "network")
        self.gateway.start()
        root = self._rootfs(shell, coreutils)
        home = self.policy.directory / "home"
        home.mkdir(exist_ok=True, mode=0o700)
        self.container = f"ombp-start-{uuid.uuid4().hex[:12]}"
        # Mapping container root to our host UID avoids global FUSE allow_other access.
        self.policy.save(CONTAINER_RECORD, self.container)
        args = ["podman", "run", "--rm", "--interactive", "--name", self.container, "--label", "app=ombp-start",
                "--http-proxy=false",
                "--network=none", "--userns=keep-id:uid=0,gid=0", "--user=0:0",
                "--cap-drop=ALL", "--security-opt=no-new-privileges", "--read-only",
                "--pid=private", "--ipc=private", "--pids-limit=4096", "--init",
                "--tmpfs=/tmp:rw,nosuid,nodev", "--tmpfs=/run:rw,nosuid,nodev,mode=755",
                "--workdir=/home/sandbox"]

        def mount(source, target, writable=False):
            # Podman --mount is comma-delimited; rejecting commas avoids option injection.
            if "," in str(source) or ":" in str(source):
                raise ValueError("Mount paths cannot contain commas or colons")
            args.extend(["--volume", f"{source}:{target}:{'rw' if writable else 'ro'}"])

        mount(self.mount / "store", "/nix/store")
        mount(self.mount / "host", "/host", True)
        mount(self.mount / "programs", "/allowed-programs")
        mount(home, "/home/sandbox", True)
        mount(Path(__file__).parent.resolve(), "/launcher")
        mount(Path(self.gateway.socket_path).parent, "/network")
        mount(self.gateway.ca_path, TRUST_CA)
        env = {"HOME": "/home/sandbox", "USER": "sandbox", "LOGNAME": "sandbox",
               "XDG_RUNTIME_DIR": "/run/ombp", "XDG_CONFIG_HOME": "/home/sandbox/.config",
               "PATH": ":".join(("/allowed-programs", str(Path(python).parent), str(Path(shell).parent),
                                 coreutils, str(Path(dbus).parent), str(Path(keyring).parent))),
               "HTTP_PROXY": PROXY_URL, "HTTPS_PROXY": PROXY_URL,
               "ALL_PROXY": PROXY_URL, "NO_PROXY": "",
               "http_proxy": PROXY_URL, "https_proxy": PROXY_URL,
               "all_proxy": PROXY_URL, "no_proxy": "", "NODE_USE_ENV_PROXY": "1",
               "SSL_CERT_FILE": TRUST_CA, "NODE_EXTRA_CA_CERTS": TRUST_CA,
               "REQUESTS_CA_BUNDLE": TRUST_CA, "CURL_CA_BUNDLE": TRUST_CA,
               "OMP_BIN": "/launcher/omp",
               "START_APP_KEYRING": keyring, "SHELL": shell, "FONTCONFIG_FILE": fonts, "GDK_BACKEND": "wayland"}
        if desktop:
            socket = self._desktop_start()
            mount(socket, "/display/wayland")
            env["WAYLAND_DISPLAY"] = "/display/wayland"
        self._gpu_access(args, env, gpu, mesa)
        for key, value in env.items():
            args.extend(["--env", f"{key}={value}"])
        args.extend(["--rootfs", str(root), dbus, "--config-file=/etc/session.conf", "--",
                     python, "/launcher/session.py", executable])
        self.process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=self.log, stderr=self.log,
                                        start_new_session=True)
        with self.process.stdin as stream:
            stream.write((password + "\n").encode())

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
        for directory in ("etc", "nix/store", "host", "allowed-programs", "launcher", "network", "trust", "home/sandbox", "tmp", "run", "proc", "dev"):
            (root / directory).mkdir(parents=True, exist_ok=True)
        (root / "etc/passwd").write_text("sandbox:x:0:0:Sandbox:/home/sandbox:/bin/sh\n")
        (root / "etc/group").write_text("sandbox:x:0:\n")
        (root / "etc/nsswitch.conf").write_text("hosts: files dns\n")
        (root / "etc/session.conf").write_text(
            '<busconfig><type>session</type><listen>unix:tmpdir=/tmp</listen><auth>EXTERNAL</auth>'
            '<policy context="default"><allow send_destination="*"/><allow receive_sender="*"/>'
            '<allow own="*"/></policy></busconfig>')
        for directory in ("bin", "usr/bin", "display"):
            (root / directory).mkdir(exist_ok=True, parents=True)
        for target, source in (("bin/sh", shell), ("bin/bash", shell), ("usr/bin/env", coreutils + "/env")):
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
        display = os.environ.get("WAYLAND_DISPLAY", "")
        host_runtime = Path(os.environ.get("XDG_RUNTIME_DIR", "/nonexistent"))
        host_socket = host_runtime / display
        weston = os.environ.get("START_APP_WESTON")
        if not weston or not display or not host_socket.is_socket():
            raise ValueError("Nested desktop requires the Nix launcher and a local Wayland session")
        directory = self.policy.directory / "display"
        directory.mkdir(mode=0o700, exist_ok=True)
        socket = directory / "wayland-ombp"
        socket.unlink(missing_ok=True)
        env = {key: os.environ[key] for key in ("PATH", "LANG", "LOCALE_ARCHIVE") if key in os.environ}
        env.update(HOME=str(directory), XDG_RUNTIME_DIR=str(directory), WAYLAND_DISPLAY=str(host_socket))
        self.desktop_process = subprocess.Popen(
            [sys.executable, str(Path(__file__).parent / "desktop.py"), str(os.getpid()), weston,
             "--backend=wayland", "--renderer=pixman", "--shell=kiosk-shell.so",
             "--socket=wayland-ombp", "--width=1440", "--height=900", "--idle-time=0", "--no-config"],
            env=env, stdout=self.log, stderr=self.log, start_new_session=True)
        deadline = time.monotonic() + 15
        while not socket.is_socket():
            if self.desktop_process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("Nested desktop did not start; inspect launch.log")
            time.sleep(0.05)
        return socket

    def _watch(self, process):
        code = process.wait()
        with self.lock:
            if self.process is process:
                if code:
                    self.last_error = f"Process exited {code}; see {self.policy.directory / 'launch.log'}"
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
            _terminate(self.desktop_process, 5)
            self.desktop_process = None
            if self.gateway:
                self.gateway.stop()
                self.gateway = None
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
