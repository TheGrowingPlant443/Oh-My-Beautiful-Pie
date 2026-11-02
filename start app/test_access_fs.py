"""Deterministic gateway tests; no mount, root privileges, or Nix installation."""

import copy
import errno
import os
from pathlib import Path
import socket
import stat
import tempfile
import threading
from contextlib import contextmanager
from types import SimpleNamespace
import unittest
from unittest.mock import patch


import access_fs


class Policy:
    def __init__(self, folders=(), programs=(), private_paths=()):
        self.data = {"folders": list(folders), "programs": list(programs),
                     "websites": [], "internet": False}
        self.private_paths = tuple(str(path) for path in private_paths)
        self.denied = []

    def snapshot(self):
        return copy.deepcopy(self.data)

    def deny(self, kind, target, reason):
        self.denied.append((kind, target, reason))


class ObservedRLock:
    """Announce contenders without replacing the lock or filesystem operations."""

    def __init__(self, contender, attempted):
        self.lock = threading.RLock()
        self.contender = contender
        self.attempted = attempted

    def __enter__(self):
        if threading.current_thread().name == self.contender:
            self.attempted.set()
        self.lock.acquire()
        return self

    def __exit__(self, *exc):
        self.lock.release()


class AccessFSTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.allowed = self.root / "allowed"
        self.allowed.mkdir()
        self.secret = self.root / "secret"
        self.secret.mkdir()
        (self.secret / "credential").write_bytes(b"outside-secret")
        self.store = self.root / "nix" / "store"
        self.store.mkdir(parents=True)
        self.base = self.store / "base-runtime"
        self.base.mkdir()
        (self.base / "runtime").write_bytes(b"base")
        self.package = self.store / "selected-package"
        (self.package / "bin").mkdir(parents=True)
        self.program = self.package / "bin" / "tool"
        self.program.write_bytes(b"#!/bin/sh\nprintf tool\\n\n")
        self.program.chmod(0o755)
        self.dependency = self.store / "selected-dependency"
        self.dependency.mkdir()
        (self.dependency / "library").write_bytes(b"library")
        self.hidden = self.store / "unapproved-package"
        (self.hidden / "bin").mkdir(parents=True)
        (self.hidden / "bin" / "other").write_bytes(b"unapproved")
        (self.hidden / "bin" / "other").chmod(0o755)
        self.policy = Policy([{"path": str(self.allowed), "write": True}])
        self.resolved = []

        def closure(program):
            self.resolved.append(program)
            return [str(self.package), str(self.dependency)]

        self.store_patch = patch.object(access_fs, "STORE", str(self.store))
        self.store_patch.start()
        self.addCleanup(self.store_patch.stop)
        self.fs = access_fs.AccessFS(self.policy, {"runtime": {
            "roots": [str(self.base)], "primary_roots": [str(self.base)], "programs": []}}, closure)
        self.addCleanup(self.fs.destroy, "/")

    @staticmethod
    def host(path):
        return "/host" + str(path)

    def read(self, path):
        handle = self.fs.open(path, os.O_RDONLY)
        try:
            return self.fs.read(path, 65536, 0, handle)
        finally:
            self.fs.release(path, handle)

    def assertDenied(self, callback, *args):
        with self.assertRaises(OSError):
            callback(*args)

    def test_virtual_ancestors_list_only_granted_descendants(self):
        path = self.host(self.root)
        self.assertEqual(self.fs.readdir(path, None), [".", "..", "allowed"])
        self.assertDenied(self.fs.readdir, self.host(self.secret), None)
        self.assertDenied(self.fs.getattr, self.host(self.secret / "credential"))
        self.assertEqual(self.fs.readdir("/host", None), [".", "..", "tmp"])

    def test_grant_and_revoke_are_live_for_existing_handles(self):
        file = self.allowed / "data"
        file.write_bytes(b"allowed")
        path = self.host(file)
        handle = self.fs.open(path, os.O_RDWR)
        self.assertEqual(self.fs.read(path, 20, 0, handle), b"allowed")
        self.policy.data["folders"] = []
        self.assertDenied(self.fs.read, path, 20, 0, handle)
        self.assertDenied(self.fs.write, path, b"X", 0, handle)
        self.assertEqual(self.fs.flush(path, handle), 0)
        self.policy.data["folders"] = [{"path": str(self.allowed), "write": False}]
        self.assertEqual(self.fs.read(path, 20, 0, handle), b"allowed")
        self.assertDenied(self.fs.write, path, b"X", 0, handle)
        self.fs.release(path, handle)
        self.assertEqual(file.read_bytes(), b"allowed")

    def test_normal_writable_directory_and_file_operations(self):
        directory = self.host(self.allowed / "directory")
        self.fs.mkdir(directory, 0o755)
        path, renamed = directory + "/file", directory + "/renamed"
        handle = self.fs.create(path, 0o640)
        self.assertEqual(self.fs.write(path, b"abcdef", 0, handle), 6)
        self.fs.rename(path, renamed)
        self.assertEqual(self.fs.read(renamed, 3, 1, handle), b"bcd")
        self.fs.truncate(renamed, 3, handle)
        self.fs.fsync(renamed, False, handle)
        self.fs.flush(renamed, handle)
        self.fs.release(renamed, handle)
        self.fs.chmod(renamed, 0o600)
        self.fs.utimens(renamed, (1000, 2000))
        self.assertEqual(self.read(renamed), b"abc")
        self.assertGreater(self.fs.statfs(directory)["f_blocks"], 0)
        self.fs.unlink(renamed)
        self.fs.rmdir(directory)
        self.assertFalse((self.allowed / "directory").exists())

    def test_chmod_restores_permissions_after_mode_zero(self):
        file = self.allowed / "data"
        file.write_bytes(b"owner-data")
        self.addCleanup(file.chmod, 0o600)
        path = self.host(file)
        self.fs.chmod(path, 0)
        self.assertEqual(stat.S_IMODE(file.stat().st_mode), 0)
        self.fs.chmod(path, 0o600)
        self.assertEqual(stat.S_IMODE(file.stat().st_mode), 0o600)
        self.assertEqual(file.read_bytes(), b"owner-data")

    def test_metadata_updates_nonreadable_directories_and_grant_root(self):
        child = self.allowed / "directory"
        child.mkdir()
        for directory in (child, self.allowed):
            with self.subTest(directory=directory):
                self.addCleanup(directory.chmod, 0o700)
                path = self.host(directory)
                self.fs.chmod(path, 0)
                self.fs.utimens(path, (1000, 2000))
                self.assertEqual(directory.stat().st_mtime_ns, 2000_000_000_000)
                self.fs.chmod(path, 0o700)
                self.assertEqual(stat.S_IMODE(directory.stat().st_mode), 0o700)

    def test_utimens_updates_owner_writable_nonreadable_file(self):
        file = self.allowed / "data"
        file.write_bytes(b"owner-data")
        self.addCleanup(file.chmod, 0o600)
        file.chmod(0o200)
        self.fs.utimens(self.host(file), (1000, 2000))
        self.assertEqual(file.stat().st_atime_ns, 1000_000_000_000)
        self.assertEqual(file.stat().st_mtime_ns, 2000_000_000_000)
        self.assertEqual(stat.S_IMODE(file.stat().st_mode), 0o200)

    def test_metadata_traverses_execute_only_grant_root_and_parent(self):
        parent = self.allowed / "parent"
        parent.mkdir()
        file = parent / "data"
        file.write_bytes(b"owner-data")
        self.addCleanup(file.chmod, 0o600)
        for directory in (self.allowed, parent):
            with self.subTest(directory=directory):
                self.addCleanup(directory.chmod, 0o700)
                directory.chmod(0o100)
                path = self.host(file)
                self.fs.chmod(path, 0o200)
                self.fs.utimens(path, (1000, 2000))
                self.assertEqual(file.stat().st_mtime_ns, 2000_000_000_000)
                self.fs.chmod(path, 0o600)
                self.assertEqual(self.read(path), b"owner-data")
                self.assertDenied(self.fs.readdir, self.host(directory), None)
                directory.chmod(0o700)

    def test_handle_validation_serializes_concurrent_file_and_directory_rename(self):
        for directory, write in ((False, False), (False, True),
                                 (True, False), (True, True)):
            with self.subTest(directory=directory, write=write):
                parent = self.allowed / f"parent-{directory}-{write}"
                parent.mkdir()
                file = parent / "data"
                file.write_bytes(b"original")
                path = self.host(file)
                source = self.host(parent if directory else file)
                target = source + "-moved"
                entered, resume, attempted = (threading.Event() for _ in range(3))

                class PausedValidationFS(access_fs.AccessFS):
                    def _entry(fs, candidate, *args, **kwargs):
                        if (threading.current_thread().name == "held-io"
                                and candidate == path):
                            entered.set()
                            if not resume.wait(5):
                                raise TimeoutError("Validation was not resumed")
                        return super()._entry(candidate, *args, **kwargs)

                fs = PausedValidationFS(self.policy, {}, lambda _: [])
                fs._lock = ObservedRLock("rename", attempted)
                self.addCleanup(fs.destroy, "/")
                handle = fs.open(path, os.O_RDWR)
                results, failures = [], []

                def io():
                    try:
                        results.append(fs.write(path, b"X", 0, handle) if write
                                       else fs.read(path, 20, 0, handle))
                    except BaseException as error:
                        failures.append(error)

                def rename():
                    try:
                        fs.rename(source, target)
                    except BaseException as error:
                        failures.append(error)

                reader = threading.Thread(target=io, name="held-io")
                renamer = threading.Thread(target=rename, name="rename")
                reader.start()
                try:
                    self.assertTrue(entered.wait(5), "IO did not reach validation")
                    renamer.start()
                    self.assertTrue(attempted.wait(5), "Rename did not contend")
                finally:
                    resume.set()
                    reader.join(5)
                    if renamer.ident is not None:
                        renamer.join(5)
                self.assertFalse(reader.is_alive())
                self.assertFalse(renamer.is_alive())
                self.assertEqual(failures, [])
                self.assertEqual(results, [1 if write else b"original"])
                self.assertEqual(fs.read(target, 20, 0, handle),
                                 b"Xriginal" if write else b"original")
                fs.release(target, handle)

    def test_open_and_create_register_before_concurrent_file_and_parent_rename(self):
        for create in (False, True):
            for directory in (False, True):
                with self.subTest(create=create, directory=directory):
                    parent = self.allowed / f"opening-{create}-{directory}"
                    parent.mkdir()
                    file = parent / "data"
                    if not create:
                        file.write_bytes(b"original")
                    path = self.host(file)
                    source = self.host(parent if directory else file)
                    target = source + "-moved"
                    opened, resume, attempted, renamed = (
                        threading.Event() for _ in range(4))

                    class PausedRegistrationFS(access_fs.AccessFS):
                        def _register(fs, candidate, fd, fi):
                            os.fstat(fd)
                            opened.set()
                            if not resume.wait(5):
                                raise TimeoutError("Registration was not resumed")
                            return super()._register(candidate, fd, fi)

                    fs = PausedRegistrationFS(self.policy, {}, lambda _: [])
                    fs._lock = ObservedRLock("opening-rename", attempted)
                    self.addCleanup(fs.destroy, "/")
                    handles, failures = [], []

                    def opening():
                        try:
                            handles.append(fs.create(path, 0o600) if create
                                           else fs.open(path, os.O_RDWR))
                        except BaseException as error:
                            failures.append(error)

                    def rename():
                        try:
                            fs.rename(source, target)
                        except BaseException as error:
                            failures.append(error)
                        finally:
                            renamed.set()

                    opener = threading.Thread(target=opening, name="opening")
                    renamer = threading.Thread(target=rename, name="opening-rename")
                    opener.start()
                    try:
                        self.assertTrue(opened.wait(5), "Descriptor was not opened")
                        unlocked = fs._lock.lock.acquire(blocking=False)
                        if unlocked:
                            fs._lock.lock.release()
                        renamer.start()
                        self.assertTrue(attempted.wait(5), "Rename did not contend")
                        if unlocked:
                            self.assertTrue(renamed.wait(5), "Rename did not finish")
                    finally:
                        resume.set()
                        opener.join(5)
                        if renamer.ident is not None:
                            renamer.join(5)
                    self.assertFalse(opener.is_alive())
                    self.assertFalse(renamer.is_alive())
                    self.assertEqual(failures, [])
                    self.assertEqual(len(handles), 1)
                    moved = target + ("/data" if directory else "")
                    handle = handles[0]
                    self.assertEqual(fs.read(moved, 20, 0, handle),
                                     b"" if create else b"original")
                    self.assertEqual(fs.write(moved, b"X", 0, handle), 1)
                    self.assertEqual(fs.read(moved, 20, 0, handle),
                                     b"X" if create else b"Xriginal")
                    fs.release(moved, handle)

    def test_concurrent_parent_rename_cannot_invalidate_pinned_logical_parent(self):
        parent = self.allowed / "parent"
        parent.mkdir()
        file = parent / "data"
        file.write_bytes(b"original")
        path = self.host(file)
        destination = self.host(self.allowed / "result")
        moved = self.host(self.allowed / "moved")
        pinned, resume, attempted = (threading.Event() for _ in range(3))

        class PausedParentFS(access_fs.AccessFS):
            @contextmanager
            def _parent(fs, entry):
                with super()._parent(entry) as value:
                    if (threading.current_thread().name == "child-rename"
                            and entry.target == str(file) and not pinned.is_set()):
                        pinned.set()
                        if not resume.wait(5):
                            raise TimeoutError("Pinned parent was not resumed")
                    yield value

        fs = PausedParentFS(self.policy, {}, lambda _: [])
        fs._lock = ObservedRLock("parent-rename", attempted)
        self.addCleanup(fs.destroy, "/")
        handle = fs.open(path, os.O_RDWR)
        failures = []

        def rename(old, new):
            try:
                fs.rename(old, new)
            except BaseException as error:
                failures.append(error)

        child = threading.Thread(target=rename, args=(path, destination),
                                 name="child-rename")
        ancestor = threading.Thread(target=rename, args=(self.host(parent), moved),
                                    name="parent-rename")
        child.start()
        try:
            self.assertTrue(pinned.wait(5), "Child rename did not pin its parent")
            ancestor.start()
            self.assertTrue(attempted.wait(5), "Parent rename did not contend")
        finally:
            resume.set()
            child.join(5)
            if ancestor.ident is not None:
                ancestor.join(5)
        self.assertFalse(child.is_alive())
        self.assertFalse(ancestor.is_alive())
        self.assertEqual(failures, [])
        self.assertEqual(fs.read(destination, 20, 0, handle), b"original")
        self.assertEqual(fs.write(destination, b"X", 0, handle), 1)
        self.assertEqual((self.allowed / "result").read_bytes(), b"Xriginal")
        self.assertTrue((self.allowed / "moved").is_dir())
        fs.release(destination, handle)

    def test_readonly_blocks_every_mutation(self):
        file = self.allowed / "file"
        file.write_bytes(b"unchanged")
        path = self.host(file)
        self.policy.data["folders"][0]["write"] = False
        for callback, args in (
            (self.fs.open, (path, os.O_WRONLY | os.O_TRUNC)),
            (self.fs.create, (self.host(self.allowed / "new"), 0o600)),
            (self.fs.mkdir, (self.host(self.allowed / "dir"), 0o700)),
            (self.fs.unlink, (path,)), (self.fs.rmdir, (self.host(self.allowed),)),
            (self.fs.rename, (path, path + "2")), (self.fs.chmod, (path, 0o777)),
            (self.fs.truncate, (path, 0)), (self.fs.utimens, (path, None)),
        ):
            with self.subTest(operation=callback.__name__):
                self.assertDenied(callback, *args)
        self.assertEqual(file.read_bytes(), b"unchanged")
        self.assertFalse(self.fs.getattr(path)["st_mode"] & 0o222)

    def test_host_symlinks_hardlinks_fifos_and_sockets_are_hidden(self):
        (self.allowed / "symlink").symlink_to(self.secret / "credential")
        (self.allowed / "directory-link").symlink_to(self.secret, target_is_directory=True)
        os.link(self.secret / "credential", self.allowed / "hardlink")
        os.mkfifo(self.allowed / "fifo")
        with socket.socket(socket.AF_UNIX) as sock:
            sock.bind(str(self.allowed / "socket"))
            self.assertEqual(self.fs.readdir(self.host(self.allowed), None), [".", ".."])
            for name in ("symlink", "directory-link/credential", "hardlink", "fifo", "socket"):
                with self.subTest(name=name):
                    path = self.host(self.allowed / name)
                    self.assertDenied(self.fs.open, path, os.O_RDONLY)
                    self.assertDenied(self.fs.open, path, os.O_WRONLY | os.O_TRUNC)
                    self.assertDenied(self.fs.chmod, path, 0o600)
                    self.assertDenied(self.fs.utimens, path, (1000, 2000))
        self.assertEqual((self.secret / "credential").read_bytes(), b"outside-secret")
        self.assertTrue(any(kind == "folder" and target == str(self.allowed)
                            for kind, target, _ in self.policy.denied))

    def test_symlink_grant_root_and_path_traversal_are_denied(self):
        alias = self.root / "alias"
        alias.symlink_to(self.secret, target_is_directory=True)
        self.policy.data["folders"].append({"path": str(alias), "write": True})
        self.assertDenied(self.read, self.host(alias / "credential"))
        for path in (self.host(self.allowed) + "/../secret/credential",
                     "/host//etc/passwd", "/host/./etc", "/host/etc\0/passwd"):
            self.assertDenied(self.fs.getattr, path)

    def test_checked_inode_cannot_be_swapped_for_symlink_before_open(self):
        file = self.allowed / "data"
        file.write_bytes(b"original")
        real_open = os.open
        swapped = False

        def swapping_open(path, flags, *args, **kwargs):
            nonlocal swapped
            if isinstance(path, str) and path.startswith("/proc/self/fd/") and not swapped:
                swapped = True
                file.rename(self.allowed / "old")
                file.symlink_to(self.secret / "credential")
            return real_open(path, flags, *args, **kwargs)

        with patch.object(access_fs.os, "open", side_effect=swapping_open):
            handle = self.fs.open(self.host(file), os.O_WRONLY | os.O_TRUNC)
        self.fs.release(self.host(file), handle)
        self.assertTrue(swapped)
        self.assertEqual((self.secret / "credential").read_bytes(), b"outside-secret")
        self.assertEqual((self.allowed / "old").read_bytes(), b"")

    def test_replaced_or_newly_hardlinked_open_inode_is_denied(self):
        file = self.allowed / "data"
        file.write_bytes(b"original")
        path = self.host(file)
        handle = self.fs.open(path, os.O_RDWR)
        os.link(file, self.root / "outside-alias")
        self.assertDenied(self.fs.read, path, 20, 0, handle)
        (self.root / "outside-alias").unlink()
        file.rename(self.root / "moved-outside")
        file.write_bytes(b"replacement")
        self.assertDenied(self.fs.write, path, b"X", 0, handle)
        self.fs.release(path, handle)
        self.assertEqual((self.root / "moved-outside").read_bytes(), b"original")

    def test_private_subtrees_and_ancestor_renames_stay_inaccessible(self):
        parent = self.allowed / "app"
        private = parent / "state"
        private.mkdir(parents=True)
        (private / "policy.json").write_bytes(b"host-owned")
        self.policy.private_paths = (str(private),)
        self.assertEqual(self.fs.readdir(self.host(parent), None), [".", ".."])
        self.assertFalse(self.policy.denied)
        self.assertDenied(self.read, self.host(private / "policy.json"))
        self.assertDenied(self.fs.rename, self.host(parent), self.host(self.allowed / "moved"))
        self.assertEqual((private / "policy.json").read_bytes(), b"host-owned")

    def test_nested_mount_crossing_is_rejected_without_mount_privileges(self):
        file = self.allowed / "file"
        file.write_bytes(b"file")
        original_fstat = os.fstat
        file_inode = file.stat().st_ino

        def foreign_device(fd):
            value = original_fstat(fd)
            if value.st_ino == file_inode:
                fields = list(value)
                fields[2] += 1
                return os.stat_result(fields)
            return value

        with patch.object(access_fs.os, "fstat", side_effect=foreign_device):
            self.assertDenied(self.read, self.host(file))

    def test_store_only_lists_approved_closures_and_revokes_programs(self):
        self.assertEqual(self.fs.readdir("/store", None), [".", "..", self.base.name])
        self.assertEqual(self.read("/store/" + self.base.name + "/runtime"), b"base")
        self.assertDenied(self.read, "/store/" + self.hidden.name + "/bin/other")
        self.policy.data["programs"] = [str(self.program)]
        self.assertEqual(self.fs.readdir("/programs", None), [".", "..", "tool"])
        self.assertEqual(self.fs.readlink("/programs/tool"), str(self.program))
        self.assertEqual(set(self.fs.readdir("/store", None)),
                         {".", "..", self.base.name, self.package.name, self.dependency.name})
        path = "/store/" + self.package.name + "/bin/tool"
        handle = self.fs.open(path, os.O_RDONLY)
        self.assertTrue(self.fs.read(path, 100, 0, handle).startswith(b"#!/bin/sh"))
        self.policy.data["programs"] = []
        self.assertDenied(self.fs.read, path, 100, 0, handle)
        self.assertDenied(self.fs.readlink, "/programs/tool")
        self.fs.release(path, handle)
        self.assertEqual(self.read("/store/" + self.base.name + "/runtime"), b"base")
        self.assertEqual(self.resolved, [str(self.program)])
        self.assertTrue(any(kind == "program" and target == str(self.hidden / "bin" / "other")
                            for kind, target, _ in self.policy.denied))

    def test_cached_roots_revoke_symlink_targets_for_existing_handles(self):
        self.policy.data["programs"] = [str(self.program)]
        (self.base / "selected-library").symlink_to(self.dependency / "library")
        path = "/store/" + self.base.name + "/selected-library"
        handle = self.fs.open(path, os.O_RDONLY)
        try:
            self.assertEqual(self.fs.read(path, 32, 0, handle), b"library")
            approved = self.policy.snapshot()
            self.policy.data["programs"] = []
            self.assertDenied(self.fs.read, path, 32, 0, handle)
            # An older concurrent snapshot must not authorize a later operation.
            self.fs._roots(approved)
            self.assertDenied(self.fs.read, path, 32, 0, handle)
            self.assertDenied(self.fs.getattr, path)
            self.policy.data["programs"] = [str(self.program)]
            self.assertEqual(self.fs.read(path, 32, 0, handle), b"library")
        finally:
            self.fs.release(path, handle)

    def test_removing_required_bundle_blocks_its_store_root_and_restores_live(self):
        path = "/store/" + self.base.name + "/runtime"
        handle = self.fs.open(path, os.O_RDONLY)
        self.fs._resolve_closure = lambda _: [
            str(self.package), str(self.dependency), str(self.base)]
        self.policy.data["programs"] = [str(self.program)]
        self.policy.data["disabled_requirements"] = ["runtime"]
        try:
            self.assertDenied(self.read, path)
            self.assertDenied(self.fs.read, path, 32, 0, handle)
            self.assertNotIn(self.base.name, self.fs.readdir("/store", None))
            self.assertTrue(self.read("/store/" + self.package.name + "/bin/tool").startswith(b"#!/bin/sh"))
            self.policy.data["disabled_requirements"] = []
            self.assertEqual(self.fs.read(path, 32, 0, handle), b"base")
        finally:
            self.fs.release(path, handle)

    def test_cleared_history_can_record_the_same_denial_again(self):
        path = self.host(self.secret / "credential")
        self.assertDenied(self.read, path)
        self.assertDenied(self.read, path)
        self.assertEqual(len(self.policy.denied), 1)
        self.policy.denied.clear()
        self.policy.history_generation = 1
        self.assertDenied(self.read, path)
        self.assertEqual(len(self.policy.denied), 1)

    def test_store_symlinks_are_rewritten_and_must_stay_in_closure(self):
        self.policy.data["programs"] = [str(self.program)]
        link = self.package / "library-link"
        link.symlink_to("../" + self.dependency.name + "/library")
        path = "/store/" + self.package.name + "/library-link"
        self.assertEqual(self.read(path), b"library")
        self.assertEqual(self.fs.readlink(path), str(self.dependency / "library"))
        self.assertTrue(stat.S_ISLNK(self.fs.getattr(path)["st_mode"]))
        for name, target in (("host-escape", self.secret / "credential"),
                             ("store-escape", self.hidden / "bin" / "other")):
            (self.package / name).symlink_to(target)
            self.assertDenied(self.read, "/store/" + self.package.name + "/" + name)
            self.assertDenied(self.fs.readlink, "/store/" + self.package.name + "/" + name)
        self.assertNotIn("host-escape", self.fs.readdir("/store/" + self.package.name, None))

    def test_store_hardlinks_are_permitted_but_writes_are_not(self):
        os.link(self.base / "runtime", self.base / "optimised")
        path = "/store/" + self.base.name + "/optimised"
        self.assertEqual(self.read(path), b"base")
        self.assertDenied(self.fs.open, path, os.O_WRONLY | os.O_TRUNC)
        self.assertEqual((self.base / "runtime").read_bytes(), b"base")

    def test_duplicate_program_names_fail_closed(self):
        self.policy.data["programs"] = [str(self.program), str(self.hidden / "bin" / "tool")]
        self.assertEqual(self.fs.readdir("/programs", None), [".", ".."])
        self.assertDenied(self.fs.readlink, "/programs/tool")
        self.assertEqual(self.fs.readdir("/store", None), [".", "..", self.base.name])

    def test_failed_closure_never_grants_partial_results(self):
        self.policy.data["programs"] = [str(self.program)]
        self.fs._resolve_closure = lambda _: [str(self.package), str(self.secret)]
        self.assertEqual(self.fs.readdir("/store", None), [".", "..", self.base.name])
        self.assertEqual(self.fs.readdir("/programs", None), [".", ".."])
        self.assertDenied(self.read, "/store/" + self.package.name + "/bin/tool")
        self.assertTrue(any(kind == "program" and target == str(self.program)
                            for kind, target, _ in self.policy.denied))

    def test_failed_closure_is_retried_without_a_policy_change(self):
        self.policy.data["programs"] = [str(self.program)]
        resolver = self.fs._resolve_closure
        self.fs._resolve_closure = lambda _: [str(self.package), str(self.secret)]
        path = "/store/" + self.package.name + "/bin/tool"
        self.assertDenied(self.read, path)
        self.fs._resolve_closure = resolver
        self.assertTrue(self.read(path).startswith(b"#!/bin/sh"))

    def test_store_root_membership_does_not_match_a_prefix(self):
        other = self.store / (self.base.name + "-other")
        other.mkdir()
        (other / "runtime").write_bytes(b"unapproved")
        with self.assertRaises(OSError) as error:
            self.read("/store/" + other.name + "/runtime")
        self.assertEqual(error.exception.errno, errno.EACCES)

    def test_store_symlink_loop_and_special_files_are_denied(self):
        (self.base / "loop").symlink_to("loop")
        os.mkfifo(self.base / "fifo")
        for name in ("loop", "fifo"):
            with self.subTest(name=name):
                path = "/store/" + self.base.name + "/" + name
                self.assertDenied(self.read, path)
                self.assertDenied(self.fs.getattr, path)
        self.assertEqual(self.fs.readdir("/store/" + self.base.name, None),
                         [".", "..", "runtime"])

    def test_raw_fuse_file_info_uses_direct_io_only_for_host(self):
        file = self.allowed / "data"
        file.write_bytes(b"host")
        path = self.host(file)
        fi = SimpleNamespace(flags=os.O_RDWR)
        self.assertEqual(self.fs.open(path, fi), 0)
        self.assertEqual(fi.direct_io, 1)
        self.assertEqual(fi.keep_cache, 0)
        self.assertEqual(self.fs.read(path, 10, 0, fi), b"host")
        self.fs.write(path, b"H", 0, fi)
        self.fs.truncate(path, 2, fi)
        self.fs.release(path, fi)
        store_path = "/store/" + self.base.name + "/runtime"
        store_fi = SimpleNamespace(flags=os.O_RDONLY)
        self.assertEqual(self.fs.open(store_path, store_fi), 0)
        self.assertEqual(store_fi.direct_io, 0)
        self.fs.release(store_path, store_fi)
        new_path = self.host(self.allowed / "new")
        created = SimpleNamespace(flags=os.O_WRONLY)
        self.assertEqual(self.fs.create(new_path, 0o600, created), 0)
        self.assertEqual(created.direct_io, 1)
        self.fs.write(new_path, b"new", 0, created)
        self.fs.release(new_path, created)
        self.assertEqual((self.allowed / "new").read_bytes(), b"new")

    def test_denial_history_is_bounded_and_deduplicated(self):
        path = self.host(self.secret)
        for _ in range(10):
            self.assertDenied(self.fs.getattr, path)
        self.assertEqual(len(self.policy.denied), 1)
        self.assertEqual(self.policy.denied[0][:2], ("folder", str(self.secret)))
        for index in range(300):
            self.assertDenied(self.fs.getattr, path + str(index))
        self.assertLessEqual(len(self.fs._denials), 256)

    def test_no_link_device_or_host_ownership_creation(self):
        for callback, args in (
            (self.fs.symlink, (self.host(self.allowed / "link"), str(self.secret))),
            (self.fs.link, (self.host(self.allowed / "link"), str(self.secret))),
            (self.fs.mknod, (self.host(self.allowed / "device"), stat.S_IFCHR | 0o600, 0)),
            (self.fs.chown, (self.host(self.allowed), 0, 0)),
        ):
            with self.subTest(operation=callback.__name__):
                with self.assertRaises(OSError) as failure:
                    callback(*args)
                self.assertEqual(failure.exception.errno, errno.EPERM)


if __name__ == "__main__":
    unittest.main()
