import errno
import json
import os
from pathlib import Path
import secrets
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
from unittest.mock import patch
import unittest

from access_fs import AccessFS
from policy import Policy
import browser
from runtime import KEYRING_UNLOCK, BrowserDisplay, Runtime


def _until(condition, timeout=5):
    deadline = time.monotonic() + timeout
    while not condition():
        if time.monotonic() >= deadline:
            raise AssertionError("Condition not reached in time")
        time.sleep(0.01)


class SourceBuildTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "checkout"
        self.source.mkdir()
        (self.source / "flake.nix").write_text("{}")
        self.runtime = Runtime(Policy(self.root / "state"))
        self.addCleanup(self.runtime.stop)

    def test_rootfs_preparation_does_not_delete_existing_temporary_data(self):
        temporary = self.runtime.policy.directory / "rootfs/var/tmp"
        temporary.mkdir(parents=True)
        saved = temporary / "saved"
        saved.write_text("keep this data")
        with self.assertRaises(OSError):
            self.runtime._rootfs("/bin/sh", "/bin")
        self.assertEqual(saved.read_text(), "keep this data")

    def test_failed_build_never_launches_an_installed_fallback(self):
        with patch("runtime.subprocess.Popen") as build:
            build.return_value.communicate.return_value = ("", None)
            build.return_value.returncode = 1
            with self.assertRaisesRegex(RuntimeError, "OMBP build failed"):
                self.runtime.start(str(self.source), "normal", False, "none")
        self.assertFalse(self.runtime.status()["running"])
        self.assertIsNone(self.runtime.launch)
        self.assertIsNone(self.runtime.log)

    def test_rejects_folder_without_flake_before_building(self):
        (self.source / "flake.nix").unlink()
        with patch("runtime.subprocess.Popen") as build:
            with self.assertRaisesRegex(ValueError, "source folder"):
                self.runtime.start(str(self.source), "container", True, "none")
            build.assert_not_called()

    def test_ambiguous_build_output_never_launches(self):
        with patch("runtime.subprocess.Popen") as build:
            build.return_value.communicate.return_value = ("/nix/store/first\n/nix/store/second\n", None)
            build.return_value.returncode = 0
            with self.assertRaisesRegex(RuntimeError, "one Nix store output"):
                self.runtime.start(str(self.source), "normal", False, "none")
        self.assertFalse(self.runtime.status()["running"])

    def test_close_during_build_reaps_child_and_prevents_launch(self):
        spawned = threading.Event()
        children, errors = [], []
        popen = subprocess.Popen

        def build(*_args, **_kwargs):
            child = popen([sys.executable, "-c", "import time; time.sleep(60)"], stdout=subprocess.PIPE, text=True)
            children.append(child)
            spawned.set()
            return child

        def start():
            try:
                self.runtime.start(str(self.source), "normal", False, "none")
            except RuntimeError as error:
                errors.append(str(error))

        worker = threading.Thread(target=start, daemon=True)
        with patch("runtime.subprocess.Popen", side_effect=build):
            worker.start()
            try:
                self.assertTrue(spawned.wait(5), "Build did not start")
                self.runtime.close()
                worker.join(5)
                self.assertFalse(worker.is_alive(), "Shutdown left start blocked")
                self.assertIsNotNone(children[0].poll(), "Build child survived shutdown")
                self.assertEqual(errors, ["Launch cancelled: launcher is closing"])
                self.assertFalse(self.runtime.status()["running"])
                self.assertIsNone(self.runtime.launch)
            finally:
                for child in children:
                    if child.poll() is None:
                        child.kill()
                    child.wait()
                worker.join(5)

    def test_closing_desktop_window_is_a_stop_but_app_crash_is_reported(self):
        def child(code, delay=0):
            return subprocess.Popen([sys.executable, "-c", f"import sys, time; time.sleep({delay}); sys.exit({code})"])

        # (desktop exit code or None while still running, expected error prefix; "" means no error)
        for desktop_code, expected in ((0, ""), (None, "Process exited 1"), (1, "Process exited 1")):
            with self.subTest(desktop_code=desktop_code):
                app = child(1)
                self.runtime.process = app
                self.runtime.desktop_process = child(0, 30) if desktop_code is None else child(desktop_code)
                self.runtime.last_error = ""
                self.runtime._watch(app)
                if expected:
                    self.assertTrue(self.runtime.last_error.startswith(expected), self.runtime.last_error)
                else:
                    self.assertEqual(self.runtime.last_error, "")
                self.assertIsNone(self.runtime.process)
                self.assertIsNone(self.runtime.desktop_process)

    def test_host_omp_bind_never_exposes_launcher_state(self):
        home = self.root / "home"
        (home / ".omp").mkdir(parents=True)
        alias = self.root / "alias"
        alias.symlink_to(home / ".omp", target_is_directory=True)
        with patch.dict(os.environ, {"HOME": str(home)}):
            self.assertEqual(self.runtime._host_omp([]), (home / ".omp").resolve())
            self.assertIsNone(self.runtime._host_omp(["host-omp"]))
            for state in (home / ".omp" / "ombp-start", alias / "state"):
                with self.subTest(state=state):
                    runtime = Runtime(Policy(state))
                    with self.assertRaisesRegex(ValueError, "overlaps protected launcher path"):
                        runtime._host_omp([])


class PrivateKeyringTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.policy = Policy(self.root / "state")
        self.runtime = Runtime(self.policy)
        self.addCleanup(self.runtime.stop)
        self.key = self.policy.directory / KEYRING_UNLOCK

    def test_unlock_key_is_stable_private_and_separate_from_container_home(self):
        first = self.runtime._keyring_password()
        home = self.policy.directory / "home"
        home.mkdir()
        (home / "saved-data").write_text("persistent private data")
        # Boolean comparisons keep generated unlock keys out of assertion output.
        self.assertTrue(secrets.compare_digest(first, Runtime(self.policy)._keyring_password()), "Private keyring key rotated")
        other = Runtime(Policy(self.root / "other-state"))
        self.assertFalse(secrets.compare_digest(first, other._keyring_password()), "Different states share an unlock key")
        self.assertEqual(stat.S_IMODE(self.key.stat().st_mode), 0o600)
        self.assertFalse(self.key.is_relative_to(home))

    def test_broad_host_grant_cannot_read_unlock_key(self):
        self.runtime._keyring_password()
        value = self.policy.snapshot()
        value["folders"] = [{"path": str(self.root), "write": True}]
        self.policy.replace(value)
        filesystem = AccessFS(self.policy, {}, lambda _: [])
        self.addCleanup(filesystem.destroy, "/")
        with self.assertRaises(OSError) as denied:
            filesystem.open("/host" + str(self.key), os.O_RDONLY)
        self.assertEqual(denied.exception.errno, errno.EACCES)

    def test_symlinked_state_parent_cannot_expose_unlock_key(self):
        real = self.root / "real"
        real.mkdir()
        alias = self.root / "alias"
        alias.symlink_to(real, target_is_directory=True)
        policy = Policy(alias / "state")
        runtime = Runtime(policy)
        runtime._keyring_password()
        (real / "shared").write_bytes(b"allowed data")
        value = policy.snapshot()
        value["folders"] = [{"path": str(real), "write": True}]
        policy.replace(value)
        filesystem = AccessFS(policy, {}, lambda _: [])
        self.addCleanup(filesystem.destroy, "/")
        handle = filesystem.open("/host" + str(real / "shared"), os.O_RDONLY)
        try:
            self.assertEqual(filesystem.read("", 32, 0, handle), b"allowed data")
        finally:
            filesystem.release("", handle)
        with self.assertRaises(OSError) as denied:
            filesystem.open("/host" + str(real / "state" / KEYRING_UNLOCK), os.O_RDONLY)
        self.assertEqual(denied.exception.errno, errno.EACCES)

    def test_existing_private_home_is_never_rekeyed_without_its_key(self):
        home = self.policy.directory / "home"
        home.mkdir()
        saved = home / "encrypted-keyring"
        saved.write_bytes(b"existing encrypted data")
        with self.assertRaises(ValueError):
            self.runtime._keyring_password()
        self.assertEqual(saved.read_bytes(), b"existing encrypted data")
        self.assertFalse(self.key.exists())

    def test_symlink_and_public_unlock_keys_are_rejected(self):
        outside = self.root / "outside-key"
        outside.write_text(json.dumps("f" * 64))
        outside.chmod(0o600)
        self.key.symlink_to(outside)
        with self.assertRaises((OSError, ValueError)):
            self.runtime._keyring_password()
        self.key.unlink()
        self.key.write_text(json.dumps("f" * 64))
        self.key.chmod(0o644)
        with self.assertRaises(ValueError):
            self.runtime._keyring_password()
        self.key.chmod(0o600)
        self.assertEqual(self.runtime._keyring_password(), "f" * 64)

    def test_malformed_unlock_key_is_not_replaced(self):
        self.key.write_text(json.dumps("invalid"))
        self.key.chmod(0o600)
        with self.assertRaises(ValueError):
            self.runtime._keyring_password()
        self.assertEqual(json.loads(self.key.read_text()), "invalid")


class PrivateBrowserTrustTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.policy = Policy(self.root / "state")
        self.runtime = Runtime(self.policy)
        self.addCleanup(self.runtime.stop)
        self.home = self.policy.directory / "home"
        self.home.mkdir()
        self.ca = self.policy.directory / "proxy-ca.pem"
        self.ca.write_text("first launcher CA")
        self.runtime.gateway = SimpleNamespace(ca_path=self.ca, socket_path=self.root / "proxy.sock", stop=lambda: None)
        environment = patch.dict(os.environ, {"START_APP_CERTUTIL": "/nix/store/test-nss/bin/certutil",
                                             "LD_PRELOAD": "/untrusted.so"})
        environment.start()
        self.addCleanup(environment.stop)

    def certutil(self, command, **kwargs):
        database = Path(command[command.index("-d") + 1].removeprefix("sql:"))
        self.assertTrue(database.is_relative_to(self.policy.directory))
        self.assertFalse(database.is_relative_to(self.home))
        self.assertEqual(kwargs["env"], {"HOME": str(database.parent.parent), "PATH": os.defpath})
        self.assertEqual(Path(kwargs["cwd"]), database.parent.parent)
        self.assertTrue(kwargs["check"])
        if "-N" in command:
            self.assertIn("--empty-password", command)
            (database / "cert9.db").write_bytes(b"")
            (database / "key4.db").write_bytes(b"fresh key database")
            (database / "pkcs11.txt").write_text(f"parameters=configdir='sql:{database}'\n")
        else:
            self.assertEqual(command[1:6], ["-A", "-n", "OMBP sandbox proxy CA", "-t", "C,,"])
            self.assertEqual(command[command.index("-i") + 1], str(self.ca))
            (database / "cert9.db").write_bytes(self.ca.read_bytes())

    def test_only_fresh_protected_database_is_opened_and_mounted_for_chrome(self):
        stored = self.home / ".pki/nssdb"
        stored.mkdir(parents=True)
        outside = self.root / "untrusted-module"
        outside.write_text("sandbox-authored module configuration")
        for name in ("cert9.db", "key4.db", "pkcs11.txt"):
            (stored / name).symlink_to(outside)
        with patch("runtime.subprocess.run", side_effect=self.certutil) as commands:
            self.runtime._browser_trust_start(self.home)
        self.assertEqual(commands.call_count, 2)
        private = Path(self.runtime.browser_trust.name) / ".pki"
        self.assertEqual((private / "nssdb/cert9.db").read_bytes(), self.ca.read_bytes())
        self.assertEqual((private / "nssdb/pkcs11.txt").read_text(),
                         "parameters=configdir='sql:/home/sandbox/.pki/nssdb'\n")
        self.assertEqual(outside.read_text(), "sandbox-authored module configuration")
        self.assertTrue(all((stored / name).is_symlink() for name in ("cert9.db", "key4.db", "pkcs11.txt")))
        args = []
        self.runtime._mount_resources(args, self.home, ["host-omp", "tool-chrome"])
        self.assertIn(f"{private}:/home/sandbox/.pki:rw", args)
        self.assertNotIn("/nix/store/test-nss/bin/certutil", args)

    def test_restart_regenerates_trust_without_stale_ca_or_changing_saved_database(self):
        stored = self.home / ".pki/nssdb"
        stored.mkdir(parents=True)
        (stored / "cert9.db").write_bytes(b"user's saved NSS database")
        with patch("runtime.subprocess.run", side_effect=self.certutil):
            self.runtime._browser_trust_start(self.home)
            first = Path(self.runtime.browser_trust.name)
            gateway = self.runtime.gateway
            self.runtime.stop()
            self.assertFalse(first.exists())
            self.runtime.gateway = gateway
            self.ca.write_text("rotated launcher CA")
            self.runtime._browser_trust_start(self.home)
        database = Path(self.runtime.browser_trust.name) / ".pki/nssdb"
        self.assertEqual((database / "cert9.db").read_bytes(), b"rotated launcher CA")
        self.assertEqual((stored / "cert9.db").read_bytes(), b"user's saved NSS database")

    def test_symlinked_mount_target_is_rejected_before_host_certutil_runs(self):
        for destination in (self.root, self.root / "missing"):
            with self.subTest(destination=destination):
                (self.home / ".pki").symlink_to(destination, target_is_directory=True)
                with patch("runtime.subprocess.run") as commands:
                    with self.assertRaises(OSError):
                        self.runtime._browser_trust_start(self.home)
                commands.assert_not_called()
                self.assertIsNone(self.runtime.browser_trust)
                (self.home / ".pki").unlink()

    def test_symlinked_home_is_rejected_before_host_certutil_runs(self):
        alias = self.policy.directory / "home-alias"
        alias.symlink_to(self.home, target_is_directory=True)
        with patch("runtime.subprocess.run") as commands:
            with self.assertRaises(OSError):
                self.runtime._browser_trust_start(alias)
        commands.assert_not_called()
        self.assertFalse((self.home / ".pki").exists())

    def test_cleanup_never_reopens_or_follows_sandbox_mutated_database(self):
        outside = self.root / "outside"
        outside.mkdir()
        marker = outside / "keep"
        marker.write_text("host data")
        with patch("runtime.subprocess.run", side_effect=self.certutil) as commands:
            self.runtime._browser_trust_start(self.home)
            scratch = Path(self.runtime.browser_trust.name)
            database = scratch / ".pki/nssdb"
            (database / "pkcs11.txt").write_text("library=/untrusted-host-module.so")
            (database / "cert9.db").unlink()
            (database / "cert9.db").symlink_to(marker)
            (database / "outside").symlink_to(outside, target_is_directory=True)
            self.runtime.stop()
        self.assertEqual(commands.call_count, 2)
        self.assertFalse(scratch.exists())
        self.assertEqual(marker.read_text(), "host data")

    def test_certutil_failure_fails_closed_and_stop_cleans_scratch(self):
        with patch("runtime.subprocess.run", side_effect=subprocess.CalledProcessError(1, "certutil")):
            with self.assertRaisesRegex(RuntimeError, "private browser CA trust"):
                self.runtime._browser_trust_start(self.home)
        scratch = Path(self.runtime.browser_trust.name)
        self.runtime.stop()
        self.assertFalse(scratch.exists())
        self.assertIsNone(self.runtime.browser_trust)


class FakeWeston:
    def __init__(self):
        self.code = None

    def poll(self):
        return self.code

    def terminate(self):
        self.code = -15

    def wait(self, timeout=None):
        return self.code


class BrowserDisplayTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.started = []
        self.display = BrowserDisplay(Path(self.temp.name) / "browser-display", self.spawn)
        self.addCleanup(self.display.stop)

    def spawn(self, directory, name):
        self.assertEqual((directory, name), (self.display.directory, "wayland-browser"))
        self.started.append(FakeWeston())
        return self.started[-1]

    def connect(self):
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.addCleanup(client.close)
        client.settimeout(5)
        client.connect(str(self.display.directory / "request.sock"))
        self.assertEqual(client.recv(16), b"ready\n")
        return client

    def test_window_runs_once_while_any_browser_holds_its_connection(self):
        first, second = self.connect(), self.connect()
        self.assertEqual(len(self.started), 1)
        first.close()
        _until(lambda: self.display.clients == 1)
        self.assertIsNone(self.started[0].code)
        second.close()
        _until(lambda: self.display.process is None)
        self.assertEqual(self.started[0].code, -15)
        self.connect()
        self.assertEqual(len(self.started), 2)

    def test_stop_retires_window_and_request_socket(self):
        self.connect()
        self.display.stop()
        self.assertEqual(self.started[0].code, -15)
        self.assertFalse((self.display.directory / "request.sock").exists())


class BrowserWrapperTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.request = str(Path(self.temp.name) / "request.sock")
        environment = patch.dict(os.environ, {"START_APP_BROWSER": "/real/chrome", "WAYLAND_DISPLAY": "host"})
        environment.start()
        self.addCleanup(environment.stop)

    def serve(self, reply):
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.addCleanup(server.close)
        server.bind(self.request)
        server.listen()

        def answer():
            connection, _ = server.accept()
            self.addCleanup(connection.close)
            connection.sendall(reply)
            if not reply:
                connection.close()
        threading.Thread(target=answer, daemon=True).start()

    def test_ready_window_hands_inheritable_connection_to_real_browser(self):
        self.serve(b"ready\n")
        with patch("browser.REQUEST", self.request), patch("browser.os.execv") as execv:
            client = browser.main(["google-chrome-stable", "--remote-debugging-port=0"])
        self.addCleanup(client.close)
        execv.assert_called_once_with("/real/chrome", ["/real/chrome", "--remote-debugging-port=0"])
        self.assertTrue(os.get_inheritable(client.fileno()))
        self.assertEqual(os.environ["WAYLAND_DISPLAY"], browser.DISPLAY)

    def test_failed_window_never_starts_browser(self):
        self.serve(b"")
        with patch("browser.REQUEST", self.request), patch("browser.os.execv") as execv:
            with self.assertRaises(SystemExit):
                browser.main(["chrome"])
        execv.assert_not_called()

    def test_without_window_service_browser_starts_unchanged(self):
        with patch("browser.REQUEST", self.request), patch("browser.os.execv") as execv:
            self.assertIsNone(browser.main(["chromium", "--headless"]))
        execv.assert_called_once_with("/real/chrome", ["/real/chrome", "--headless"])
        self.assertEqual(os.environ["WAYLAND_DISPLAY"], "host")



if __name__ == "__main__":
    unittest.main()
