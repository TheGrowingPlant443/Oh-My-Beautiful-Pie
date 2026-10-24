//! Opened-handle containment and directory pins. On Windows every ancestor is held without write/delete sharing
//! from the volume root through the destination. A directory cannot be replaced, renamed or converted to a reparse
//! point during traversal/publication. Final files are validated by their handles, never by re-resolving a pathname.
use super::fsio::handle_identity;
#[cfg(windows)]
use super::fsio::{self, err, same_path};
use super::Result;
#[cfg(windows)]
use crate::code_quality::snapshot::is_link;
#[cfg(windows)]
use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::fs::File;
#[cfg(windows)]
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct DirectoryPins {
    files: Vec<File>,
    path: PathBuf,
}

impl DirectoryPins {
    pub(crate) fn identity(&self) -> Result<String> {
        handle_identity(self.files.last().ok_or("No pinned directory")?)?.id.ok_or_else(|| "Filesystem does not expose a stable directory identity".to_string())
    }

    pub(crate) fn path(&self) -> &Path { &self.path }
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetFinalPathNameByHandleW(file: *mut c_void, path: *mut u16, length: u32, flags: u32) -> u32;
        pub fn SetFileInformationByHandle(file: *mut c_void, class: u32, info: *const c_void, size: u32) -> i32;
    }
}

/// The final location of the opened object, resolved by the kernel from its handle.
#[cfg(windows)]
pub(crate) fn handle_path(file: &File) -> Result<PathBuf> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::ffi::OsStringExt;
    let handle = file.as_raw_handle() as *mut std::ffi::c_void;
    let mut buffer = vec![0u16; 1024];
    loop {
        // SAFETY: buffer is writable for its length; the handle remains owned by file.
        let n = unsafe { win::GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0) };
        if n == 0 { return Err(err("Cannot resolve opened handle", std::io::Error::last_os_error())); }
        if n as usize >= buffer.len() {
            if n > 32768 { return Err("Opened handle path is too long".into()); }
            buffer.resize(n as usize + 1, 0);
            continue;
        }
        let raw = std::ffi::OsString::from_wide(&buffer[..n as usize]);
        let text = raw.to_string_lossy();
        return Ok(PathBuf::from(if let Some(unc) = text.strip_prefix("\\\\?\\UNC\\") { format!("\\\\{unc}") } else { text.strip_prefix("\\\\?\\").unwrap_or(&text).to_string() }));
    }
}

/// Pins an absolute directory and all ancestors; checks both the handle metadata and its kernel-resolved path.
/// The caller keeps this guard alive until its path-based operation has finished.
pub(crate) fn pin_directory(path: &Path) -> Result<DirectoryPins> {
    pin_directory_leaf(path, false)
}

/// `writable_leaf` additionally shares FILE_SHARE_WRITE on the deepest directory only. Windows opens the rename target
/// directory for write when publishing; every ancestor still denies write/delete sharing, so the directory itself can
/// not be renamed, deleted or replaced, and it is never empty at that point (the staged note lives in it).
#[cfg(windows)]
pub(crate) fn pin_directory_leaf(path: &Path, writable_leaf: bool) -> Result<DirectoryPins> {
    if !path.is_absolute() { return Err("A rooted operation requires an absolute directory".into()); }
    let mut files = Vec::new();
    let mut ancestors: Vec<&Path> = path.ancestors().collect();
    ancestors.reverse();
    for ancestor in ancestors {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)] {
            use std::os::windows::fs::OpenOptionsExt;
            // Prevent write/delete opens of each directory while pinned. Child-file creation remains possible.
            let leaf = writable_leaf && ancestor == path;
            options.share_mode(if leaf { 1 | 2 } else { 1 }).custom_flags(0x0200_0000 | 0x0020_0000);
        }
        let file = options.open(ancestor).map_err(|e| err("Cannot pin directory safely", e))?;
        let meta = file.metadata().map_err(|e| err("Cannot inspect pinned directory", e))?;
        if !meta.is_dir() || is_link(&meta) { return Err("Refusing linked/reparse-point directory".into()); }
        if !same_path(&handle_path(&file)?, ancestor) { return Err("Directory changed or resolves through a link; choose its real location".into()); }
        files.push(file);
    }
    Ok(DirectoryPins { files, path: path.to_path_buf() })
}

/// Pins a root and descendants (creating missing directories without adopting existing links).
#[cfg(windows)]
pub(crate) fn pin_descendants(root: &Path, components: &[String], create: bool, writable_leaf: bool) -> Result<DirectoryPins> {
    let mut pins = pin_directory_leaf(root, writable_leaf && components.is_empty())?;
    for (i, component) in components.iter().enumerate() {
        if component.is_empty() || component == "." || component == ".." || component.contains(['/', '\\', ':', '\0']) { return Err("Unsafe directory component".into()); }
        let child = pins.path.join(component);
        if create {
            match fs::create_dir(&child) {
                Ok(()) => {},
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {},
                Err(e) => return Err(err("Cannot create note folder", e)),
            }
        }
        let mut next = pin_directory_leaf(&child, writable_leaf && i + 1 == components.len())?;
        pins.files.append(&mut next.files);
        pins.path = child;
    }
    Ok(pins)
}

/// Opens a single-linked regular file while the complete directory chain is pinned.
#[cfg(windows)]
pub(crate) fn open_file(root: &Path, components: &[String]) -> Result<(File, PathBuf)> {
    let (name, parents) = components.split_last().ok_or("A file path is required")?;
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', ':', '\0']) { return Err("Unsafe file component".into()); }
    let pins = pin_descendants(root, parents, false, false)?;
    let path = pins.path.join(name);
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)] {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(&path).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { format!("Not found: {}", components.join("/")) } else { err("Cannot safely open file", e) })?;
    validate_file(&file, &path)?;
    Ok((file, path))
}

#[cfg(windows)]
fn validate_file(file: &File, path: &Path) -> Result<()> {
    let meta = file.metadata().map_err(|e| err("Cannot inspect opened file", e))?;
    if !meta.is_file() || is_link(&meta) { return Err("Not a plain regular file".into()); }
    if handle_identity(file)?.links != 1 { return Err("Refusing a multiply linked file".into()); }
    if !same_path(&handle_path(file)?, path) { return Err("Opened file is outside its admitted location".into()); }
    Ok(())
}

/// Opens the destination so that, until the handle is dropped, no other process can write, rename or delete it:
/// access READ|DELETE (so this handle may itself rename/delete the object), sharing READ only.
#[cfg(windows)]
pub(crate) fn lock_destination(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    let file = OpenOptions::new().read(true).access_mode(0x8001_0000).share_mode(1).custom_flags(0x0020_0000).open(path)
        .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound {
            "The note no longer exists; review it before publishing".to_string()
        } else {
            err("Cannot lock note for publication; close external edits and review again", e)
        })?;
    validate_file(&file, path)?;
    Ok(file)
}

/// Renames the object behind `file` to `name` inside the pinned parent. `replace == false` is create-only: it fails
/// when `name` exists, so a file that appeared concurrently is never overwritten.
#[cfg(windows)]
pub(crate) fn rename_handle(file: &File, parent: &DirectoryPins, name: &str, replace: bool) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct RenameInfo { flags: u32, root: *mut std::ffi::c_void, name_len: u32, name: [u16; 1] }
    // Win32 requires RootDirectory == NULL (a non-NULL root is rejected with ERROR_INVALID_PARAMETER), so the target is
    // the absolute path below the *pinned* parent: its directory chain cannot be renamed or replaced while pinned.
    let target = handle_path(parent.files.last().ok_or("No pinned parent directory")?)?.join(name);
    // Like the standard library, hand over a verbatim (`\\?\`) path: the Win32 layer otherwise converts the DOS path
    // into a longer NT path in place, which misread the name (stray characters after it) and rejected others (os error 123).
    let dos = target.to_string_lossy().replace('/', "\\");
    let verbatim = match dos.strip_prefix("\\\\") {
        Some(unc) if !unc.starts_with("?\\") => format!("\\\\?\\UNC\\{unc}"),
        _ if dos.starts_with("\\\\?\\") => dos.clone(),
        _ => format!("\\\\?\\{dos}"),
    };
    let utf16: Vec<u16> = verbatim.encode_utf16().collect();
    let name_offset = std::mem::offset_of!(RenameInfo, name);
    let byte_count = name_offset + (utf16.len() + 1) * 2; // includes the terminating NUL
    // u64 allocation gives the native structure the required pointer alignment.
    let mut buffer = vec![0u64; byte_count.div_ceil(8)];
    let info = buffer.as_mut_ptr().cast::<RenameInfo>();
    // SAFETY: allocation is aligned and large enough for the variable length native FILE_RENAME_INFO structure.
    unsafe {
        (*info).flags = if replace { 1 | 2 } else { 0 }; // [REPLACE_IF_EXISTS | POSIX_SEMANTICS] or create-only
        (*info).root = std::ptr::null_mut();
        (*info).name_len = (utf16.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), (info as *mut u8).add(name_offset).cast::<u16>(), utf16.len());
        // FileRenameInfoEx (22).
        if win::SetFileInformationByHandle(file.as_raw_handle() as *mut _, 22, info.cast(), byte_count as u32) == 0 {
            let error = std::io::Error::last_os_error();
            let exists = error.raw_os_error() == Some(80) || error.raw_os_error() == Some(183);
            return Err(if exists { format!("{} already exists", shown_path(&target)) } else { err(&format!("Atomic publication failed ({})", shown_path(&target)), error) });
        }
    }
    Ok(())
}

/// Moves the locked original to its reservation name. Windows: the locked handle cannot change, so this is the
/// create-only handle rename.
#[cfg(windows)]
pub(crate) fn reserve_handle(file: &File, parent: &DirectoryPins, name: &str) -> Result<()> {
    rename_handle(file, parent, name, false)
}

/// What the locked destination is called in the shared publication code.
#[cfg(windows)]
pub(crate) type Handle = File;

/// Checked removal by the locked destination handle, never by a re-resolved path. The name stays locked (no other
/// process can rename, delete or write it) from the hash check until this handle's own deletion completes.
pub(crate) fn remove_checked(parent: &DirectoryPins, name: &str, expected_hash: &str) -> Result<()> {
    #[cfg(not(any(windows, unix)))] {
        let _ = (parent, name, expected_hash);
        return Err("Conflict-safe Markdown deletion requires handle support that this platform lacks".into());
    }
    #[cfg(windows)] {
        let mut locked = lock_destination(&parent.path.join(name))?;
        if hash_handle(&mut locked)? != expected_hash { return Err("The note changed since it was reviewed; deletion refused".into()); }
        delete_handle(&locked)
    }
    #[cfg(unix)] {
        unix_impl::remove_checked(parent, name, expected_hash)
    }
}

/// Deletes the exact object behind `file` (FILE_DISPOSITION_DELETE | POSIX_SEMANTICS). A different file that now
/// carries the same name is never touched.
#[cfg(windows)]
pub(crate) fn delete_handle(file: &File) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    let flags = 1u32 | 2u32;
    // SAFETY: `file` owns a valid handle with DELETE access and `flags` is a FILE_DISPOSITION_INFO_EX.
    if unsafe { win::SetFileInformationByHandle(file.as_raw_handle() as *mut _, 21, (&flags as *const u32).cast(), 4) } == 0 {
        return Err(err("Cannot delete the locked note", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// SHA-256 of the whole file behind `file`, read from the start; the position is rewound afterwards.
#[cfg(windows)]
pub(crate) fn hash_handle(file: &mut File) -> Result<String> {
    use sha2::Digest;
    file.seek(SeekFrom::Start(0)).map_err(|e| err("Cannot verify locked note", e))?;
    let mut hasher = sha2::Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut chunk).map_err(|e| err("Cannot verify locked note", e))?;
        if n == 0 { break; }
        hasher.update(&chunk[..n]);
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| err("Cannot verify locked note", e))?;
    Ok(fsio::hex(hasher.finalize()))
}

/// A fully written, flushed note waiting under a private name in the pinned parent. Its handle denies all sharing.
#[cfg(windows)]
pub(crate) struct Staged {
    pub file: File,
    pub name: String,
}

#[cfg(windows)]
pub(crate) fn stage(parent: &DirectoryPins, text: &[u8], name: &str) -> Result<Staged> {
    use std::os::windows::fs::OpenOptionsExt;
    let name = name.to_string();
    let path = parent.path.join(&name);
    let mut file = OpenOptions::new().read(true).write(true).create_new(true).access_mode(0xC001_0000).share_mode(0)
        .custom_flags(0x0020_0000).open(&path).map_err(|e| err("Cannot stage note", e))?;
    let written = (|| -> Result<()> {
        validate_file(&file, &path)?;
        file.write_all(text).map_err(|e| err("Cannot write staged note", e))?;
        file.sync_all().map_err(|e| err("Cannot flush staged note", e))
    })();
    if let Err(e) = written {
        let _ = delete_handle(&file);
        return Err(e);
    }
    Ok(Staged { file, name })
}

fn shown_path(path: &Path) -> String {
    path.to_string_lossy().chars().filter(|c| !c.is_control()).take(200).collect()
}

#[cfg(unix)]
#[path = "rooted_unix.rs"]
mod unix_impl;
#[cfg(unix)]
pub(crate) use unix_impl::{
    delete_handle, hash_handle, lock_destination, open_file, pin_descendants, pin_directory_leaf, rename_handle, reserve_handle, stage, Handle, Staged,
};
