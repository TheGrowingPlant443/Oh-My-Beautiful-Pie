//! Unix side of the rooted-directory capability (`rooted.rs`).
//!
//! There are no mandatory file locks on Unix, so the Windows technique (hold every ancestor and the destination open
//! without write/delete sharing) is replaced by two properties of the kernel interface:
//!
//! * **Directories are capabilities.** A root is opened component by component from `/` with
//!   `openat(O_DIRECTORY | O_NOFOLLOW)`. A symbolic link anywhere in the chain is refused, and every later operation
//!   (create, open, rename, unlink, fsync) is relative to the pinned directory descriptor, never to a pathname that a
//!   concurrent process could re-point. Renaming or replacing an ancestor after the pin cannot redirect a write.
//! * **Renames never overwrite.** Moves use `renameat2(RENAME_NOREPLACE)` (Linux) / `renameatx_np(RENAME_EXCL)`
//!   (macOS) and fall back to `linkat` + `unlinkat` on filesystems without them; the destination name is created
//!   atomically or the call fails. A rename moves a *name*, not an object, so every move is verified afterwards
//!   (`dev`+`ino` of the moved object against the handle that was hashed) and put back when another object was moved
//!   or when the bytes behind the handle changed between the hash check and the move.
//!
//! What this cannot prevent (Windows can, by sharing modes): a process that already has the note open for writing and
//! writes into the original object in the instants after the post-move check. Such a write lands in the superseded
//! original; the replacement itself never overwrites anything, and the hash check on the exact moved object makes
//! every earlier write visible.
use super::{shown_path, DirectoryPins};
use crate::knowledge::fsio::{self, err, handle_identity};
use crate::knowledge::Result;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

const DIRECTORY: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
/// `O_NONBLOCK`: opening a FIFO or device that is named like a note must neither block nor ever be read.
const PLAIN_FILE: libc::c_int = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;

fn name_bytes(name: &[u8]) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

fn open_at(dir: RawFd, name: &[u8], flags: libc::c_int, mode: libc::mode_t) -> io::Result<File> {
    let name = name_bytes(name)?;
    loop {
        // SAFETY: `name` is a NUL-terminated string that outlives the call; `dir` is a descriptor or AT_FDCWD.
        let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC, libc::c_uint::from(mode)) };
        if fd >= 0 {
            // SAFETY: `openat` returned a new descriptor that nothing else owns.
            return Ok(unsafe { File::from_raw_fd(fd) });
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn stat_at(dir: RawFd, name: &[u8]) -> io::Result<libc::stat> {
    let name = name_bytes(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: `name` is NUL-terminated and `stat` is writable for one `libc::stat`.
    if unsafe { libc::fstatat(dir, name.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so the structure was filled in.
    Ok(unsafe { stat.assume_init() })
}

fn unlink_at(dir: RawFd, name: &[u8]) -> io::Result<()> {
    let name = name_bytes(name)?;
    // SAFETY: `name` is NUL-terminated and outlives the call.
    if unsafe { libc::unlinkat(dir, name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn directory_at(dir: RawFd, name: &[u8]) -> Result<File> {
    open_at(dir, name, DIRECTORY, 0).map_err(|error| {
        let link = stat_at(dir, name).is_ok_and(|stat| stat.st_mode & libc::S_IFMT == libc::S_IFLNK);
        if link { "Refusing linked/reparse-point directory".to_string() } else { err("Cannot pin directory safely", error) }
    })
}

fn leaf(pins: &DirectoryPins) -> Result<&File> {
    pins.files.last().ok_or_else(|| "No pinned directory".to_string())
}

/// `renameat2(RENAME_NOREPLACE)`: fails with `EEXIST` rather than replacing `to`.
#[cfg(target_os = "linux")]
fn rename_exclusive(from_dir: RawFd, from: &[u8], to_dir: RawFd, to: &[u8]) -> io::Result<()> {
    const RENAME_NOREPLACE: libc::c_uint = 1;
    let (from_name, to_name) = (name_bytes(from)?, name_bytes(to)?);
    // SAFETY: both names are NUL-terminated and outlive the call; the descriptors are valid directories.
    let status = unsafe { libc::syscall(libc::SYS_renameat2, from_dir, from_name.as_ptr(), to_dir, to_name.as_ptr(), RENAME_NOREPLACE) };
    if status == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP) => link_then_unlink(from_dir, from, to_dir, to),
        _ => Err(error),
    }
}

#[cfg(target_os = "macos")]
fn rename_exclusive(from_dir: RawFd, from: &[u8], to_dir: RawFd, to: &[u8]) -> io::Result<()> {
    let (from_name, to_name) = (name_bytes(from)?, name_bytes(to)?);
    // SAFETY: both names are NUL-terminated and outlive the call; the descriptors are valid directories.
    let status = unsafe { libc::renameatx_np(from_dir, from_name.as_ptr(), to_dir, to_name.as_ptr(), libc::RENAME_EXCL) };
    if status == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOTSUP | libc::EINVAL) => link_then_unlink(from_dir, from, to_dir, to),
        _ => Err(error),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_exclusive(from_dir: RawFd, from: &[u8], to_dir: RawFd, to: &[u8]) -> io::Result<()> {
    link_then_unlink(from_dir, from, to_dir, to)
}

/// Create-only publication without an exclusive rename: `linkat` fails with `EEXIST` when the name is taken, so a
/// concurrent creator is never clobbered; the source name is dropped afterwards.
fn link_then_unlink(from_dir: RawFd, from: &[u8], to_dir: RawFd, to: &[u8]) -> io::Result<()> {
    let (from_name, to_name) = (name_bytes(from)?, name_bytes(to)?);
    // SAFETY: both names are NUL-terminated and outlive the call; the descriptors are valid directories.
    if unsafe { libc::linkat(from_dir, from_name.as_ptr(), to_dir, to_name.as_ptr(), 0) } != 0 {
        let error = io::Error::last_os_error();
        return Err(if matches!(error.raw_os_error(), Some(libc::EPERM | libc::EMLINK | libc::ENOTSUP | libc::EXDEV)) {
            io::Error::other(format!("this filesystem supports neither exclusive renames nor hard links ({error})"))
        } else {
            error
        });
    }
    unlink_at(from_dir, from)
}

/// Pins an absolute directory by walking it from `/` without following a single link. The deepest descriptor is the
/// capability; `writable_leaf` only matters on Windows.
pub(crate) fn pin_directory_leaf(path: &Path, _writable_leaf: bool) -> Result<DirectoryPins> {
    if !path.is_absolute() {
        return Err("A rooted operation requires an absolute directory".into());
    }
    let mut current = open_at(libc::AT_FDCWD, b"/", DIRECTORY, 0).map_err(|e| err("Cannot pin directory safely", e))?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => current = directory_at(current.as_raw_fd(), name.as_bytes())?,
            _ => return Err("Refusing a directory path with relative components".into()),
        }
    }
    Ok(DirectoryPins { files: vec![current], path: path.to_path_buf() })
}

fn unsafe_component(component: &str) -> bool {
    component.is_empty() || component == "." || component == ".." || component.contains(['/', '\\', ':', '\0'])
}

/// Pins a root and descendants (creating missing directories without adopting existing links).
pub(crate) fn pin_descendants(root: &Path, components: &[String], create: bool, _writable_leaf: bool) -> Result<DirectoryPins> {
    let mut pins = pin_directory_leaf(root, false)?;
    for component in components {
        if unsafe_component(component) {
            return Err("Unsafe directory component".into());
        }
        let parent = leaf(&pins)?.as_raw_fd();
        if create {
            let name = name_bytes(component.as_bytes()).map_err(|e| err("Cannot create note folder", e))?;
            // SAFETY: `name` is NUL-terminated and outlives the call; `parent` is the pinned directory.
            if unsafe { libc::mkdirat(parent, name.as_ptr(), 0o777) } != 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(err("Cannot create note folder", error));
                }
            }
        }
        let next = directory_at(parent, component.as_bytes())?;
        pins.files = vec![next];
        pins.path = pins.path.join(component);
    }
    Ok(pins)
}

fn validate_file(file: &File) -> Result<()> {
    let meta = file.metadata().map_err(|e| err("Cannot inspect opened file", e))?;
    if !meta.is_file() {
        return Err("Not a plain regular file".into());
    }
    if handle_identity(file)?.links != 1 {
        return Err("Refusing a multiply linked file".into());
    }
    Ok(())
}

/// Opens a single-linked regular file inside the pinned directory chain; a link or special file is refused.
pub(crate) fn open_file(root: &Path, components: &[String]) -> Result<(File, PathBuf)> {
    let (name, parents) = components.split_last().ok_or("A file path is required")?;
    if unsafe_component(name) {
        return Err("Unsafe file component".into());
    }
    let pins = pin_descendants(root, parents, false, false)?;
    let file = open_at(leaf(&pins)?.as_raw_fd(), name.as_bytes(), PLAIN_FILE, 0).map_err(|e| match e.raw_os_error() {
        _ if e.kind() == io::ErrorKind::NotFound => format!("Not found: {}", components.join("/")),
        Some(libc::ELOOP) => "Not a plain regular file".to_string(),
        _ => err("Cannot safely open file", e),
    })?;
    validate_file(&file)?;
    Ok((file, pins.path.join(name)))
}

/// An opened object together with the directory it lives in and the name it currently carries. The name follows the
/// object through [`rename_handle`]; every name-based operation re-checks that the name still denotes this object.
pub(crate) struct Held {
    file: File,
    dir: File,
    name: Mutex<String>,
    /// The hash [`hash_handle`] verified; [`reserve_handle`] re-checks the bytes against it after the move.
    hashed: Mutex<Option<String>>,
}

/// What the locked destination is called in the shared publication code.
pub(crate) type Handle = Held;

impl Held {
    fn new(file: File, dir: File, name: &str) -> Self {
        Self { file, dir, name: Mutex::new(name.to_string()), hashed: Mutex::new(None) }
    }

    fn current(&self) -> String {
        self.name.lock().clone()
    }

    /// The name still denotes exactly this object (same device and inode, not a link).
    fn is_named(&self) -> io::Result<bool> {
        same_object(&self.file, &self.dir, &self.current())
    }
}

impl Read for Held {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Seek for Held {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

fn same_object(file: &File, dir: &File, name: &str) -> io::Result<bool> {
    let meta = file.metadata()?;
    match stat_at(dir.as_raw_fd(), name.as_bytes()) {
        Ok(stat) => Ok(stat.st_mode & libc::S_IFMT == libc::S_IFREG && stat.st_dev as u64 == meta.dev() && stat.st_ino as u64 == meta.ino()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn same_directory(a: &File, b: &File) -> io::Result<bool> {
    let (a, b) = (a.metadata()?, b.metadata()?);
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}

/// SHA-256 of the whole object, read positionally so the shared file offset is irrelevant.
fn digest(file: &File) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    let mut offset = 0u64;
    loop {
        let count = match file.read_at(&mut chunk, offset) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if count == 0 {
            return Ok(fsio::hex(hasher.finalize()));
        }
        hasher.update(&chunk[..count]);
        offset += count as u64;
    }
}

/// Opens the destination without following links and verifies it is a single-linked regular file. Unlike Windows the
/// object stays writable for other processes; [`hash_handle`] + [`reserve_handle`] detect what that could change.
pub(crate) fn lock_destination(path: &Path) -> Result<Held> {
    let parent = path.parent().ok_or("The note has no folder")?;
    let name = path.file_name().and_then(|n| n.to_str()).ok_or("The note has no usable name")?;
    let mut pins = super::pin_directory(parent)?;
    let dir = pins.files.pop().ok_or("No pinned directory")?;
    let file = open_at(dir.as_raw_fd(), name.as_bytes(), PLAIN_FILE, 0).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            "The note no longer exists; review it before publishing".to_string()
        } else {
            err("Cannot open note for publication; close external edits and review again", e)
        }
    })?;
    validate_file(&file)?;
    Ok(Held::new(file, dir, name))
}

/// SHA-256 of the whole file behind `file`, read from the start; the position is rewound afterwards. The digest is
/// remembered so [`reserve_handle`] can prove the bytes did not change while the name was being moved.
pub(crate) fn hash_handle(file: &mut Held) -> Result<String> {
    let verify = |e: io::Error| err("Cannot verify locked note", e);
    file.file.seek(SeekFrom::Start(0)).map_err(verify)?;
    let hash = digest(&file.file).map_err(verify)?;
    file.file.seek(SeekFrom::Start(0)).map_err(verify)?;
    *file.hashed.lock() = Some(hash.clone());
    Ok(hash)
}

/// Renames the object behind `file` to `name` inside the pinned parent. `replace == false` is create-only: it fails
/// when `name` exists, so a file that appeared concurrently is never overwritten. The moved object is verified to be
/// this handle's; if the name had been swapped for another file just before, that file is put back.
pub(crate) fn rename_handle(file: &Held, parent: &DirectoryPins, name: &str, replace: bool) -> Result<()> {
    let target = shown_path(&parent.path().join(name));
    if replace {
        return Err(format!("Replacing renames are not offered on this platform ({target})"));
    }
    let to_dir = leaf(parent)?;
    if !same_directory(&file.dir, to_dir).map_err(|e| err("Cannot compare note folders", e))? {
        return Err(format!("The note's folder changed while it was being published ({target})"));
    }
    let from = file.current();
    if let Err(error) = rename_exclusive(file.dir.as_raw_fd(), from.as_bytes(), to_dir.as_raw_fd(), name.as_bytes()) {
        return Err(if error.kind() == io::ErrorKind::AlreadyExists { format!("{target} already exists") } else { err(&format!("Atomic publication failed ({target})"), error) });
    }
    match same_object(&file.file, to_dir, name) {
        Ok(true) => {}
        outcome => {
            // The name was swapped for another file before the move: that other file is what moved. Put it back.
            let back = rename_exclusive(to_dir.as_raw_fd(), name.as_bytes(), file.dir.as_raw_fd(), from.as_bytes());
            let cause = outcome.err().map_or_else(|| "another file carried the name".to_string(), |e| e.to_string());
            return Err(match back {
                Ok(()) => format!("The note was replaced by another file while it was being moved; that file was put back untouched ({cause})"),
                Err(restore) => format!("The note was replaced by another file while it was being moved and could not be put back ({cause}; restore: {restore}); it is now named {target}"),
            });
        }
    }
    *file.name.lock() = name.to_string();
    let _ = to_dir.sync_all();
    Ok(())
}

/// Moves the locked original to its reservation name and proves, on the exact moved object, that the bytes still
/// match the hash that was verified. A note that another program changed in the meantime is put back untouched.
pub(crate) fn reserve_handle(file: &Held, parent: &DirectoryPins, name: &str) -> Result<()> {
    let original = file.current();
    rename_handle(file, parent, name, false)?;
    let Some(expected) = file.hashed.lock().clone() else { return Ok(()) };
    if matches!(digest(&file.file), Ok(now) if now == expected) {
        return Ok(());
    }
    Err(match rename_handle(file, parent, &original, false) {
        Ok(()) => "The note was changed by another program while it was being replaced; it was put back untouched".to_string(),
        Err(restore) => format!("The note was changed by another program while it was being replaced and could not be put back ({restore}); the changed note stays under its reservation name {name}"),
    })
}

/// Deletes the object behind `file` by its current name, but only while that name still denotes this very object.
/// A different file that now carries the name is never touched (reported); a name that is gone needs nothing.
pub(crate) fn delete_handle(file: &Held) -> Result<()> {
    let name = file.current();
    match file.is_named() {
        Ok(true) => {}
        Ok(false) if stat_at(file.dir.as_raw_fd(), name.as_bytes()).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) => return Ok(()),
        Ok(false) => return Err("The name now belongs to another file; it was left untouched".into()),
        Err(error) => return Err(err("Cannot delete the locked note", error)),
    }
    unlink_at(file.dir.as_raw_fd(), name.as_bytes()).map_err(|e| err("Cannot delete the locked note", e))?;
    let _ = file.dir.sync_all();
    Ok(())
}

/// A fully written, flushed note waiting under a private name in the pinned parent.
pub(crate) struct Staged {
    pub file: Held,
    pub name: String,
}

pub(crate) fn stage(parent: &DirectoryPins, text: &[u8], name: &str) -> Result<Staged> {
    let dir = leaf(parent)?.try_clone().map_err(|e| err("Cannot stage note", e))?;
    let created = open_at(dir.as_raw_fd(), name.as_bytes(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW, 0o666).map_err(|e| err("Cannot stage note", e))?;
    let staged = Held::new(created, dir, name);
    let written = (|| -> Result<()> {
        validate_file(&staged.file)?;
        (&staged.file).write_all(text).map_err(|e| err("Cannot write staged note", e))?;
        staged.file.sync_all().map_err(|e| err("Cannot flush staged note", e))
    })();
    if let Err(error) = written {
        let _ = delete_handle(&staged);
        return Err(error);
    }
    Ok(Staged { file: staged, name: name.to_string() })
}

/// Checked removal: the destination is hash-verified on its own handle, moved (create-only) to a private tombstone
/// name, re-verified there on the exact moved object and only then unlinked. A different file that carries the note's
/// name at any point is put back, never deleted.
pub(crate) fn remove_checked(parent: &DirectoryPins, name: &str, expected_hash: &str) -> Result<()> {
    let mut locked = lock_destination(&parent.path().join(name))?;
    if hash_handle(&mut locked)? != expected_hash {
        return Err("The note changed since it was reviewed; deletion refused".into());
    }
    let tombstone = format!(".omp-deleted-{}.tmp", uuid::Uuid::new_v4());
    reserve_handle(&locked, parent, &tombstone).map_err(|e| format!("The note changed since it was reviewed; deletion refused: {e}"))?;
    delete_handle(&locked)
}
