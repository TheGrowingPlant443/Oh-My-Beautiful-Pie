"""Live, fail-closed host-folder and Nix-store views for a rootless container.

Mount with raw_fi=True and zero attribute/entry/negative cache timeouts. Host
files use direct IO; immutable store files retain executable mmap support. Host
symlinks, multiply-linked files and nested mounts are intentionally unavailable;
Nix symlinks may only resolve within the currently approved closure roots.
"""

import errno
import os
import posixpath
import stat
import threading
from collections import Counter, OrderedDict
from contextlib import contextmanager
from dataclasses import dataclass

from fuse import FuseOSError, Operations


STORE = "/nix/store"
_STORE_VIEW = "/store"
_HOST_VIEW = "/host"
_DIRECTORY = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
_PATH = os.O_PATH | os.O_NOFOLLOW | os.O_CLOEXEC
_SYSTEM = ("/proc", "/sys", "/dev", "/run", "/nix")
# Lookups of files that never exist, not user actions; Python's startup probes beside its executable included.
# A leading "-" is a login shell's argv[0] (bash looks up "-bash"), never a program name.
_PROBES = {".DS_Store", ".directory", ".hidden", "desktop.ini", "autorun.inf", "pyvenv.cfg"}


def _within(path, root):
    return path == root or path.startswith(root.rstrip("/") + "/")


def _absolute(path):
    return (isinstance(path, str) and path.startswith("/") and "\0" not in path
            and path == posixpath.normpath(path) and not path.startswith("//"))


def _store_root(path):
    if path.startswith(STORE + "/"):
        return STORE + "/" + path[len(STORE) + 1:].split("/", 1)[0]


@dataclass(frozen=True)
class _Entry:
    kind: str
    target: str = ""
    anchor: str = "/"
    writable: bool = False


@dataclass
class _Handle:
    fd: int
    path: str


class AccessFS(Operations):
    """policy supplies snapshot(), deny(), and optional absolute private_paths.

    closure_resolver(program) returns absolute /nix/store/<root> closure roots.
    It must fail rather than return an incomplete closure. Results are cached;
    a fresh policy snapshot determines which cached closures remain accessible.
    """

    def __init__(self, policy, base_store_roots, closure_resolver):
        self.policy = policy
        self._requirements = base_store_roots
        self._base_roots = {key: frozenset(self._root(root) for root in item["roots"])
                            for key, item in base_store_roots.items()}
        self._resolve_closure = closure_resolver
        self._closures = {}
        self._roots_key = None
        self._approved_roots = frozenset()
        self._handles = {}
        self._next_handle = 1
        self._lock = threading.RLock()
        self._denials = OrderedDict()
        self._history_generation = getattr(policy, "history_generation", 0)

    @staticmethod
    def _root(path):
        if not _absolute(path) or posixpath.dirname(path) != STORE:
            raise ValueError("Expected an absolute Nix store root")
        return path

    def _deny(self, kind, target, reason):
        name = posixpath.basename(target)
        if name in _PROBES or name.endswith("._pth") or (kind == "program" and name.startswith("-")):
            return
        key = kind, target, reason
        with self._lock:
            generation = getattr(self.policy, "history_generation", 0)
            if generation != self._history_generation:
                self._denials.clear()
                self._history_generation = generation
            if key in self._denials:
                return
            self._denials[key] = None
            if len(self._denials) > 256:
                self._denials.popitem(last=False)
        self.policy.deny(kind, target, reason)

    def _blocked(self, path):
        return any(_within(path, root) for root in (
            *_SYSTEM, *getattr(self.policy, "private_paths", ())))

    def _grants(self, snapshot):
        return [(item["path"], bool(item.get("write")))
                for item in snapshot.get("folders", ())
                if isinstance(item, dict) and _absolute(item.get("path"))
                and not self._blocked(item["path"])]

    def _blocked_roots(self, snapshot):
        return {self._root(root) for key in snapshot.get("disabled_requirements", ())
                for root in self._requirements.get(key, {}).get("primary_roots", ())}

    def _programs(self, snapshot):
        blocked = self._blocked_roots(snapshot)
        disabled = snapshot.get("disabled_requirements", ())
        programs = {}
        for key, item in self._requirements.items():
            if key not in disabled:
                for path in item["programs"]:
                    if not any(_within(path, root) for root in blocked):
                        programs.setdefault(posixpath.basename(path), path)
        paths = [path for path in snapshot.get("programs", ())
                 if _absolute(path) and _within(path, STORE) and path != STORE
                 and not any(_within(path, root) for root in blocked)]
        counts = Counter(posixpath.basename(path) for path in set(paths))
        for path in paths:
            name = posixpath.basename(path)
            if counts[name] == 1:
                programs[name] = path
            else:
                programs.pop(name, None)
        return programs

    def _roots(self, snapshot):
        key = (tuple(snapshot.get("programs", ())),
               tuple(snapshot.get("disabled_requirements", ())))
        with self._lock:
            if key == self._roots_key:
                return self._approved_roots
            disabled = snapshot.get("disabled_requirements", ())
            roots = set()
            for requirement, paths in self._base_roots.items():
                if requirement not in disabled:
                    roots.update(paths)
            complete = True
            for program in set(snapshot.get("programs", ())) & set(self._programs(snapshot).values()):
                try:
                    if program not in self._closures:
                        self._closures[program] = frozenset(
                            self._root(root) for root in self._resolve_closure(program))
                    roots.update(self._closures[program])
                except (OSError, ValueError, RuntimeError):
                    complete = False
                    self._deny("program", program, "Cannot resolve approved Nix closure")
            roots = frozenset(roots - self._blocked_roots(snapshot))
            # Failed closures stay retryable; only complete policy results are cached.
            if complete:
                self._roots_key, self._approved_roots = key, roots
            return roots

    def _entry(self, path, write=False, snapshot=None, file=False):
        if not _absolute(path):
            raise FuseOSError(errno.EINVAL)
        snapshot = self.policy.snapshot() if snapshot is None else snapshot
        if path in ("/", _STORE_VIEW, "/programs"):
            if write:
                raise FuseOSError(errno.EROFS)
            return _Entry("virtual")
        if _within(path, _HOST_VIEW):
            return self._host_entry(path[len(_HOST_VIEW):] or "/", write, snapshot, file)
        if path.startswith("/programs/"):
            name = path[len("/programs/"):]
            program = self._programs(snapshot).get(name)
            if program is None:
                self._deny("program", name, "Program is not selected")
                raise FuseOSError(errno.ENOENT)
            if write:
                raise FuseOSError(errno.EROFS)
            self._store_resolve(program, self._roots(snapshot))
            return _Entry("program", program)
        if path.startswith(_STORE_VIEW + "/"):
            return self._store_entry(STORE + path[len(_STORE_VIEW):], write, snapshot)
        raise FuseOSError(errno.ENOENT)

    def _host_entry(self, target, write, snapshot, file):
        grants = self._grants(snapshot)
        if not self._blocked(target):
            matches = [(root, writable) for root, writable in grants
                       if _within(target, root) and (writable or not write)]
            if matches:
                root, writable = max(matches, key=lambda item: len(item[0]))
                return _Entry("host", target, root, writable)
            if not write and (target == "/" or any(
                    _within(root, target) for root, _ in grants)):
                return _Entry("virtual")
        self._deny("folder", posixpath.dirname(target) if file else target,
                   "Folder is not granted for " + ("writing" if write else "reading"))
        raise FuseOSError(errno.EACCES)

    def _store_entry(self, target, write, snapshot):
        if _store_root(target) not in self._roots(snapshot):
            if "/bin/" in target and self._unapproved_executable(target):
                self._deny("program", target, "Nix executable is not approved")
            raise FuseOSError(errno.EACCES)
        if write:
            raise FuseOSError(errno.EROFS)
        return _Entry("store", target)

    def _unapproved_executable(self, target):
        root = _store_root(target)
        try:
            target = self._store_resolve(target, {root})
            parent = self._directory(posixpath.dirname(target))
            try:
                st = os.stat(posixpath.basename(target), dir_fd=parent,
                             follow_symlinks=False)
                return stat.S_ISREG(st.st_mode) and bool(st.st_mode & 0o111)
            finally:
                os.close(parent)
        except OSError:
            return False

    @staticmethod
    def _directory(path):
        fd = os.open("/", _PATH | os.O_DIRECTORY)
        try:
            for part in path.strip("/").split("/") if path != "/" else ():
                child = os.open(part, _PATH | os.O_DIRECTORY, dir_fd=fd)
                os.close(fd)
                fd = child
            return fd
        except BaseException:
            os.close(fd)
            raise

    @contextmanager
    def _parent(self, entry):
        parent, name = posixpath.split(entry.target)
        fd = self._directory(entry.anchor)
        try:
            device = os.fstat(fd).st_dev
            relative = posixpath.relpath(parent, entry.anchor)
            # The grant root itself is never a mutable directory entry.
            if relative == ".." or relative.startswith("../"):
                raise FuseOSError(errno.EACCES)
            for part in relative.split("/") if relative != "." else ():
                child = os.open(part, _PATH | os.O_DIRECTORY, dir_fd=fd)
                if os.fstat(child).st_dev != device:
                    os.close(child)
                    raise FuseOSError(errno.EXDEV)
                os.close(fd)
                fd = child
            yield fd, name, device
        finally:
            os.close(fd)

    @staticmethod
    def _safe_host(st, device):
        if st.st_dev != device:
            raise FuseOSError(errno.EXDEV)
        if not (stat.S_ISDIR(st.st_mode) or stat.S_ISREG(st.st_mode)):
            raise FuseOSError(errno.EACCES)
        if stat.S_ISREG(st.st_mode) and st.st_nlink > 1:
            raise FuseOSError(errno.EACCES)

    @staticmethod
    def _reopen_host(inspected, flags):
        if flags & os.O_CREAT and flags & os.O_EXCL:
            raise FuseOSError(errno.EEXIST)
        safe_flags = flags & ~(os.O_TRUNC | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW)
        # This trusted proc link names the pinned, checked inode.
        return os.open(f"/proc/self/fd/{inspected}", safe_flags
                       | os.O_CLOEXEC | os.O_NONBLOCK)

    def _host_open(self, entry, flags, mode=0o666):
        if entry.target == entry.anchor:
            pinned = self._directory(entry.anchor)
            if flags == _PATH:
                return pinned
            try:
                return self._reopen_host(pinned, flags)
            finally:
                os.close(pinned)
        with self._parent(entry) as (parent, name, device):
            # Pin the inode before opening it: a replaced device must never open.
            try:
                inspected = os.open(name, _PATH, dir_fd=parent)
            except FileNotFoundError:
                if not flags & os.O_CREAT:
                    raise
                fd = os.open(name, flags | os.O_EXCL | os.O_NOFOLLOW
                             | os.O_CLOEXEC, mode, dir_fd=parent)
            else:
                try:
                    self._safe_host(os.fstat(inspected), device)
                    if flags == _PATH:
                        fd, inspected = inspected, None
                    else:
                        fd = self._reopen_host(inspected, flags)
                finally:
                    if inspected is not None:
                        os.close(inspected)
            try:
                self._safe_host(os.fstat(fd), device)
                if flags & os.O_TRUNC:
                    os.ftruncate(fd, 0)
                return fd
            except BaseException:
                os.close(fd)
                raise

    @staticmethod
    def _checked_store_link(fd, final, follow_final, links):
        mode = os.fstat(fd).st_mode
        if stat.S_ISLNK(mode) and (not final or follow_final):
            if links >= 40:
                raise FuseOSError(errno.ELOOP)
            return os.readlink("", dir_fd=fd)
        if not (stat.S_ISREG(mode) or stat.S_ISDIR(mode)
                or (final and stat.S_ISLNK(mode))):
            raise FuseOSError(errno.EACCES)
        if not final and not stat.S_ISDIR(mode):
            raise FuseOSError(errno.ENOTDIR)
        return None

    def _store_resolve(self, target, roots, follow_final=True):
        links = 0
        while True:
            if _store_root(target) not in roots:
                raise FuseOSError(errno.EACCES)
            parts = target[len(STORE) + 1:].split("/")
            fd = self._directory(STORE)
            resolved = STORE
            try:
                for index, name in enumerate(parts):
                    child = os.open(name, _PATH, dir_fd=fd)
                    try:
                        final = index == len(parts) - 1
                        link = self._checked_store_link(child, final, follow_final, links)
                        if link is not None:
                            links += 1
                            target = posixpath.normpath(posixpath.join(
                                resolved, link, *parts[index + 1:]))
                            break
                        resolved += "/" + name
                        if final:
                            return resolved
                        os.close(fd)
                        fd, child = child, None
                    finally:
                        if child is not None:
                            os.close(child)
            finally:
                os.close(fd)

    def _open_entry(self, entry, flags):
        if entry.kind == "host":
            return self._host_open(entry, flags)
        if entry.kind in ("store", "program"):
            target = self._store_resolve(entry.target, self._roots(self.policy.snapshot()))
            parent = self._directory(posixpath.dirname(target))
            try:
                pinned = os.open(posixpath.basename(target), _PATH, dir_fd=parent)
                try:
                    mode = os.fstat(pinned).st_mode
                    if not (stat.S_ISREG(mode) or stat.S_ISDIR(mode)):
                        raise FuseOSError(errno.EACCES)
                    if flags == _PATH:
                        fd, pinned = pinned, None
                        return fd
                    return os.open(f"/proc/self/fd/{pinned}",
                                   (flags & ~os.O_NOFOLLOW) | os.O_CLOEXEC
                                   | os.O_NONBLOCK)
                finally:
                    if pinned is not None:
                        os.close(pinned)
            finally:
                os.close(parent)
        raise FuseOSError(errno.EISDIR)

    @staticmethod
    def _attributes(st, writable=False):
        if not (stat.S_ISREG(st.st_mode) or stat.S_ISDIR(st.st_mode)
                or stat.S_ISLNK(st.st_mode)):
            raise FuseOSError(errno.EACCES)
        attrs = {key: getattr(st, key) for key in (
            "st_mode", "st_nlink", "st_size", "st_atime", "st_mtime", "st_ctime",
            "st_uid", "st_gid", "st_ino")}
        attrs["st_mode"] &= ~0o6000  # Never advertise host set-id privileges.
        if not writable:
            attrs["st_mode"] &= ~0o222
        return attrs

    def getattr(self, path, fh=None):
        entry = self._entry(path)
        if entry.kind == "virtual":
            return {"st_mode": stat.S_IFDIR | 0o555, "st_nlink": 2,
                    "st_uid": os.getuid(), "st_gid": os.getgid(), "st_size": 0}
        if entry.kind == "program":
            return {"st_mode": stat.S_IFLNK | 0o777, "st_nlink": 1,
                    "st_uid": os.getuid(), "st_gid": os.getgid(),
                    "st_size": len(entry.target)}
        if entry.kind == "store":
            roots = self._roots(self.policy.snapshot())
            target = self._store_resolve(entry.target, roots, follow_final=False)
            self._store_resolve(entry.target, roots)
            parent = self._directory(posixpath.dirname(target))
            try:
                return self._attributes(os.stat(posixpath.basename(target),
                                                dir_fd=parent, follow_symlinks=False))
            finally:
                os.close(parent)
        fd = self._host_open(entry, _PATH)
        try:
            return self._attributes(os.fstat(fd), entry.writable)
        finally:
            os.close(fd)

    def readdir(self, path, fh):
        snapshot = self.policy.snapshot()
        entry = self._entry(path, snapshot=snapshot)
        if path == "/":
            names = {"host", "store", "programs"}
        elif path == _STORE_VIEW:
            names = {posixpath.basename(root) for root in self._roots(snapshot)}
        elif path == "/programs":
            names = set(self._programs(snapshot))
        elif entry.kind == "virtual":
            target = path[len(_HOST_VIEW):] or "/"
            names = {posixpath.relpath(root, target).split("/")[0]
                     for root, _ in self._grants(snapshot) if _within(root, target)
                     and root != target}
        else:
            fd = self._open_entry(entry, _DIRECTORY)
            try:
                names = set(os.listdir(fd))
            finally:
                os.close(fd)
        visible = []
        for name in sorted(names):
            if entry.kind == "host" and self._blocked(
                    posixpath.join(entry.target, name)):
                continue
            try:
                self.getattr(path.rstrip("/") + "/" + name)
            except OSError:
                continue
            visible.append(name)
        return [".", "..", *visible]

    def readlink(self, path):
        entry = self._entry(path)
        if entry.kind == "program":
            return entry.target
        if entry.kind != "store":
            raise FuseOSError(errno.EACCES)
        target = self._store_resolve(entry.target, self._roots(self.policy.snapshot()),
                                     follow_final=False)
        parent = self._directory(posixpath.dirname(target))
        try:
            if not stat.S_ISLNK(os.stat(posixpath.basename(target), dir_fd=parent,
                                       follow_symlinks=False).st_mode):
                raise FuseOSError(errno.EINVAL)
        finally:
            os.close(parent)
        return self._store_resolve(entry.target, self._roots(self.policy.snapshot()))

    def _register(self, path, fd, fi):
        with self._lock:
            handle = self._next_handle
            self._next_handle += 1
            if fi is not None:
                fi.fh = handle
                host = _within(path, _HOST_VIEW)
                fi.direct_io = int(host)
                # Store files are immutable; every open is still authorized, only their page cache survives.
                fi.keep_cache = int(not host)
            self._handles[handle] = _Handle(fd, path)
        return handle if fi is None else 0

    def open(self, path, flags):
        fi = None if isinstance(flags, int) else flags
        flags = flags if fi is None else fi.flags
        write = bool(flags & (os.O_WRONLY | os.O_RDWR | os.O_TRUNC | os.O_APPEND))
        with self._lock:
            entry = self._entry(path, write=write, file=True)
            try:
                fd = self._open_entry(entry, flags)
            except OSError:
                if entry.kind == "host":
                    self._deny("folder", posixpath.dirname(entry.target),
                               "Host path is unsafe or unavailable")
                raise
            try:
                if not stat.S_ISREG(os.fstat(fd).st_mode):
                    raise FuseOSError(errno.EISDIR)
                return self._register(path, fd, fi)
            except BaseException:
                os.close(fd)
                raise

    def create(self, path, mode, fi=None):
        with self._lock:
            self._entry(posixpath.dirname(path), write=True)
            entry = self._entry(path, write=True, file=True)
            flags = os.O_RDWR if fi is None else fi.flags
            fd = self._host_open(entry, flags | os.O_CREAT | os.O_EXCL, mode & 0o777)
            try:
                return self._register(path, fd, fi)
            except BaseException:
                os.close(fd)
                raise

    @contextmanager
    def _handle(self, fh, write=False):
        fh = fh if isinstance(fh, int) else fh.fh
        fd = None
        try:
            with self._lock:
                handle = self._handles.get(fh)
                if handle is None:
                    raise FuseOSError(errno.EBADF)
                fd, path = os.dup(handle.fd), handle.path
                entry = self._entry(path, write=write, file=True)
                current = self._open_entry(entry, _PATH)
                try:
                    actual, now = os.fstat(fd), os.fstat(current)
                    if (actual.st_dev, actual.st_ino) != (now.st_dev, now.st_ino):
                        raise FuseOSError(errno.EACCES)
                    if entry.kind == "host":
                        self._safe_host(actual, now.st_dev)
                finally:
                    os.close(current)
            yield fd
        finally:
            if fd is not None:
                os.close(fd)

    def read(self, path, size, offset, fh):
        with self._handle(fh) as fd:
            return os.pread(fd, size, offset)

    def write(self, path, data, offset, fh):
        with self._handle(fh, write=True) as fd:
            return os.pwrite(fd, data, offset)

    def truncate(self, path, length, fh=None):
        if fh is not None:
            with self._handle(fh, write=True) as fd:
                os.ftruncate(fd, length)
        else:
            fd = self._open_entry(self._entry(path, write=True, file=True), os.O_WRONLY)
            try:
                os.ftruncate(fd, length)
            finally:
                os.close(fd)
        return 0

    def flush(self, path, fh):
        fh = fh if isinstance(fh, int) else fh.fh
        with self._lock:
            if fh not in self._handles:
                raise FuseOSError(errno.EBADF)
        return 0

    def fsync(self, path, datasync, fh):
        with self._handle(fh) as fd:
            (os.fdatasync if datasync else os.fsync)(fd)
        return 0

    def release(self, path, fh):
        fh = fh if isinstance(fh, int) else fh.fh
        with self._lock:
            handle = self._handles.pop(fh, None)
        if handle is not None:
            os.close(handle.fd)
        return 0

    def _mutable_parent(self, path):
        self._entry(posixpath.dirname(path), write=True)
        entry = self._entry(path, write=True)
        if entry.kind != "host":
            raise FuseOSError(errno.EROFS)
        if any(_within(private, entry.target)
               for private in getattr(self.policy, "private_paths", ())):
            raise FuseOSError(errno.EACCES)
        return entry

    def mkdir(self, path, mode):
        entry = self._mutable_parent(path)
        with self._parent(entry) as (parent, name, _):
            os.mkdir(name, mode & 0o777, dir_fd=parent)
        return 0

    def _remove(self, path, directory):
        entry = self._mutable_parent(path)
        checked = self._host_open(entry, _PATH)
        try:
            with self._parent(entry) as (parent, name, _):
                (os.rmdir if directory else os.unlink)(name, dir_fd=parent)
        finally:
            os.close(checked)
        return 0

    def unlink(self, path):
        return self._remove(path, False)

    def rmdir(self, path):
        return self._remove(path, True)

    def rename(self, old, new):
        with self._lock:
            source, destination = self._mutable_parent(old), self._mutable_parent(new)
            checked = self._host_open(source, _PATH)
            try:
                try:
                    existing = self._host_open(destination, _PATH)
                except FileNotFoundError:
                    pass
                else:
                    os.close(existing)
                with self._parent(source) as (src, src_name, _):
                    with self._parent(destination) as (dst, dst_name, _):
                        os.rename(src_name, dst_name, src_dir_fd=src, dst_dir_fd=dst)
                for handle in self._handles.values():
                    if _within(handle.path, old):
                        handle.path = new + handle.path[len(old):]
            finally:
                os.close(checked)
        return 0

    def chmod(self, path, mode):
        entry = self._entry(path, write=True)
        fd = self._open_entry(entry, _PATH)
        try:
            os.chmod(f"/proc/self/fd/{fd}", mode & 0o777)
        finally:
            os.close(fd)
        return 0

    def utimens(self, path, times=None):
        entry = self._entry(path, write=True)
        fd = self._open_entry(entry, _PATH)
        try:
            os.utime(f"/proc/self/fd/{fd}", times)
        finally:
            os.close(fd)
        return 0

    def access(self, path, mode):
        entry = self._entry(path, write=bool(mode & os.W_OK))
        attrs = self.getattr(path)
        if mode & os.X_OK and not attrs["st_mode"] & 0o111:
            raise FuseOSError(errno.EACCES)
        if entry.kind == "virtual" and mode & os.W_OK:
            raise FuseOSError(errno.EROFS)
        return 0

    def statfs(self, path):
        entry = self._entry(path)
        if entry.kind == "virtual":
            return {"f_bsize": 4096, "f_frsize": 4096, "f_blocks": 0,
                    "f_bfree": 0, "f_bavail": 0, "f_files": 0, "f_ffree": 0,
                    "f_namemax": 255}
        fd = self._open_entry(entry, os.O_RDONLY)
        try:
            st = os.fstatvfs(fd)
            return {key: getattr(st, key) for key in (
                "f_bsize", "f_frsize", "f_blocks", "f_bfree", "f_bavail",
                "f_files", "f_ffree", "f_favail", "f_flag", "f_namemax")}
        finally:
            os.close(fd)

    def symlink(self, target, source):
        raise FuseOSError(errno.EPERM)

    def link(self, target, source):
        raise FuseOSError(errno.EPERM)

    def mknod(self, path, mode, dev):
        raise FuseOSError(errno.EPERM)

    def chown(self, path, uid, gid):
        raise FuseOSError(errno.EPERM)

    def destroy(self, path):
        with self._lock:
            handles, self._handles = self._handles, {}
        for handle in handles.values():
            os.close(handle.fd)
