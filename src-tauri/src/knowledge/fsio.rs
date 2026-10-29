//! Rooted, link-free and hardlink-safe file primitives shared by the vault reader/writer and Graphify admission.
//!
//! Every read goes through [`open_components`]: each path component below the root is inspected without following
//! links, the file is opened without following a final reparse point, the opened *handle* is checked to be a regular
//! file with exactly one hard link, and the resolved location is verified to still be inside the root. A note that is
//! a hardlink to something else (for example a credential file) is therefore never read, previewed or copied.
// Windows file identity needs GetFileInformationByHandle; std does not expose its volume/file-index identity.
pub(crate) use super::note_publication::{
    publications_need_recovery, recover_publications, remove_relative_checked,
    write_relative_markdown_journaled,
};
pub(crate) use super::rooted::{pin_descendants, pin_directory, pin_directory_leaf, DirectoryPins};
use super::Result;
use crate::code_quality::snapshot;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

pub(crate) const MAX_COMPONENT_BYTES: usize = 255;
pub(crate) const MAX_RELATIVE_BYTES: usize = 1024;

#[must_use]
pub(crate) fn err(context: &str, error: impl std::fmt::Display) -> String {
    format!("{context}: {error}")
}

#[must_use]
pub(crate) fn hex(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(DIGITS[usize::from(byte >> 4)] as char);
        encoded.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

#[must_use]
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes))
}

/// A bounded, control-free rendering of untrusted text for error messages.
#[must_use]
pub(crate) fn shown(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(120).collect()
}

#[must_use]
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[must_use]
pub(crate) fn mtime_nanos(meta: &fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Case-folded (on Windows) forward-slash spelling used only for comparisons.
#[must_use]
pub(crate) fn norm(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    };
    text.trim_end_matches('/').to_string()
}

#[must_use]
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    norm(a) == norm(b)
}

/// `path == root` or `path` is below `root`, component-wise.
#[must_use]
pub(crate) fn same_or_inside(root: &Path, path: &Path) -> bool {
    let (root, path) = (norm(root), norm(path));
    path == root || path.starts_with(&format!("{root}/"))
}

#[must_use]
pub(crate) fn is_markdown_name(name: &str) -> bool {
    name.len() > 3
        && name.is_char_boundary(name.len() - 3)
        && name[name.len() - 3..].eq_ignore_ascii_case(".md")
}

/// Validates a vault-relative path written with forward slashes: no traversal, drive letters, alternate streams,
/// Windows device names, trailing dots/spaces, control characters or hidden (dot-prefixed) components.
pub(crate) fn validate_relative(path: &str) -> Result<Vec<String>> {
    if path.is_empty() {
        return Err("A relative path is required".into());
    }
    if path.len() > MAX_RELATIVE_BYTES {
        return Err("Vault path is too long".into());
    }
    if path.starts_with('/')
        || path.ends_with('/')
        || path.contains("//")
        || path.contains('\\')
        || path.chars().any(|c| c.is_control())
    {
        return Err(format!("Unsafe vault path: {}", shown(path)));
    }
    let components: Vec<&str> = path.split('/').collect();
    for part in &components {
        if part.is_empty()
            || part.len() > MAX_COMPONENT_BYTES
            || part.starts_with('.')
            || part.trim() != *part
        {
            return Err(format!("Unsafe vault path: {}", shown(path)));
        }
    }
    snapshot::relative_source_name(Path::new(path))
        .map_err(|_| format!("Unsafe vault path: {}", shown(path)))?;
    Ok(components.into_iter().map(String::from).collect())
}

/// Like [`validate_relative`] but the empty prefix (the vault root) is valid.
pub(crate) fn validate_prefix(prefix: &str) -> Result<Vec<String>> {
    if prefix.is_empty() {
        Ok(Vec::new())
    } else {
        validate_relative(prefix.trim_end_matches('/'))
    }
}

fn display_components(components: &[String]) -> String {
    shown(&components.join("/"))
}

/// Walks `components` below `root`, refusing every link/reparse point. Missing components are reported as `Not found`.
pub(crate) fn plain_chain(root: &Path, components: &[String]) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    for (i, component) in components.iter().enumerate() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(meta) if snapshot::is_link(&meta) => {
                return Err(format!(
                    "Refusing linked/reparse-point path inside the vault: {}",
                    display_components(&components[..=i])
                ))
            }
            Ok(meta) if i + 1 < components.len() && !meta.is_dir() => {
                return Err(format!(
                    "Not a folder: {}",
                    display_components(&components[..=i])
                ))
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "Not found: {}",
                    display_components(&components[..=i])
                ))
            }
            Err(e) => return Err(err("Cannot inspect vault path", e)),
        }
    }
    Ok(path)
}

pub(crate) struct Identity {
    pub links: u64,
    pub id: Option<String>,
}

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    #[repr(C)]
    pub struct ByHandleFileInformation {
        pub file_attributes: u32,
        pub creation_time: [u32; 2],
        pub last_access_time: [u32; 2],
        pub last_write_time: [u32; 2],
        pub volume_serial_number: u32,
        pub file_size_high: u32,
        pub file_size_low: u32,
        pub number_of_links: u32,
        pub file_index_high: u32,
        pub file_index_low: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetFileInformationByHandle(
            file: *mut c_void,
            information: *mut ByHandleFileInformation,
        ) -> i32;
    }
}

/// Hard-link count and a volume/file identity read from the *opened handle* (Windows handle information, Unix `nlink`).
#[cfg(windows)]
pub(crate) fn handle_identity(file: &File) -> Result<Identity> {
    use std::os::windows::io::AsRawHandle;
    let mut information = std::mem::MaybeUninit::<sys::ByHandleFileInformation>::zeroed();
    // SAFETY: the handle is owned by `file` for the duration of the call and the structure is fully zero-initialised.
    let ok = unsafe {
        sys::GetFileInformationByHandle(
            file.as_raw_handle() as *mut std::ffi::c_void,
            information.as_mut_ptr(),
        )
    };
    if ok == 0 {
        return Err(err(
            "Cannot inspect the opened file",
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: the call succeeded, so the structure was filled in.
    let information = unsafe { information.assume_init() };
    Ok(Identity {
        links: u64::from(information.number_of_links),
        id: Some(format!(
            "{:08x}:{:08x}{:08x}",
            information.volume_serial_number,
            information.file_index_high,
            information.file_index_low
        )),
    })
}

/// Unix: `nlink` plus `dev:ino`, extended by the birth time where the filesystem reports one. Inode numbers are
/// reused quickly (a removed and recreated folder may get the same number); the birth time tells them apart.
#[cfg(unix)]
pub(crate) fn handle_identity(file: &File) -> Result<Identity> {
    use std::os::unix::fs::MetadataExt;
    let meta = file
        .metadata()
        .map_err(|e| err("Cannot inspect the opened file", e))?;
    let born = meta
        .created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos());
    let id = match born {
        Some(born) => format!("{:x}:{:x}:{born:x}", meta.dev(), meta.ino()),
        None => format!("{:x}:{:x}", meta.dev(), meta.ino()),
    };
    Ok(Identity {
        links: meta.nlink(),
        id: Some(id),
    })
}

#[cfg(not(any(windows, unix)))]
pub(crate) fn handle_identity(_file: &File) -> Result<Identity> {
    Ok(Identity { links: 1, id: None })
}

/// Identity of a directory (volume + file index), used to notice a vault root that was replaced or moved.
pub(crate) fn dir_identity(path: &Path) -> Option<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_BACKUP_SEMANTICS (open a directory) | FILE_FLAG_OPEN_REPARSE_POINT (never follow a link).
        options.custom_flags(0x0200_0000 | 0x0020_0000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Never follow a link at the root, and only a directory has a directory identity.
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    }
    let file = options.open(path).ok()?;
    handle_identity(&file).ok().and_then(|i| i.id)
}

/// Opens `components` below `root` for reading. Returns the handle and its path. See the module documentation.
pub(crate) fn open_components(root: &Path, components: &[String]) -> Result<(File, PathBuf)> {
    super::rooted::open_file(root, components)
}

pub(crate) fn open_relative(root: &Path, relative: &str) -> Result<(File, PathBuf)> {
    open_components(root, &validate_relative(relative)?)
}

/// Reads a small regular file completely (never following links), failing above `max` bytes.
pub(crate) fn read_regular_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    let file = snapshot::open_plain(path)?;
    let len = file
        .metadata()
        .map_err(|e| err("Cannot inspect file", e))?
        .len();
    if len > max {
        return Err(format!("{} is larger than {max} bytes", path.display()));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| err("Cannot read file", e))?;
    if bytes.len() as u64 > max {
        return Err(format!("{} is larger than {max} bytes", path.display()));
    }
    Ok(bytes)
}

/// Writes `bytes` to a same-directory temporary file and renames it into place. With `replace == false` an existing
/// target is an error (hard-link based, so a concurrent creator is never clobbered).
pub(crate) fn write_atomic(target: &Path, bytes: &[u8], replace: bool) -> Result<()> {
    let parent = target.parent().ok_or("The target has no parent folder")?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("The target has no usable name")?;
    let temporary = parent.join(format!(".{}.omp-{}.tmp", name, uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| err("Cannot create temporary file", e))?;
        file.write_all(bytes)
            .map_err(|e| err("Cannot write temporary file", e))?;
        file.sync_all()
            .map_err(|e| err("Cannot flush temporary file", e))?;
        drop(file);
        if replace {
            return fs::rename(&temporary, target).map_err(|e| err("Cannot publish file", e));
        }
        match fs::hard_link(&temporary, target) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(format!("{name} already exists"))
            }
            Err(_) => {
                // Filesystems without hard links: best-effort exists check, then rename.
                if fs::symlink_metadata(target).is_ok() {
                    return Err(format!("{name} already exists"));
                }
                fs::rename(&temporary, target).map_err(|e| err("Cannot publish file", e))
            }
        }
    })();
    let _ = fs::remove_file(&temporary);
    result
}

#[must_use]
pub(crate) fn percent_encode(text: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(DIGITS[usize::from(byte >> 4)] as char);
            out.push(DIGITS[usize::from(byte & 0x0f)] as char);
        }
    }
    out
}

/// `None` when the escapes are malformed or the result is not UTF-8.
pub(crate) fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = (*bytes.get(i + 1)? as char).to_digit(16)?;
            let lo = (*bytes.get(i + 2)? as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub(crate) struct Line {
    /// Absolute byte offset of the first byte of the line.
    pub start: u64,
    /// The line exceeded the cap; `line` is empty and the content was consumed (hashed/validated) but not kept.
    pub too_long: bool,
}

/// A streaming line reader that hashes and UTF-8-validates every byte it consumes, so a whole file is verified in the
/// same pass that scans it. Lines have no total size limit; a single line above the cap is flagged, never truncated.
pub(crate) struct HashedReader {
    reader: BufReader<File>,
    hasher: Sha256,
    bytes: u64,
    carry: Vec<u8>,
    utf8_ok: bool,
    capture_start: u64,
    capture_len: usize,
    captured: Vec<u8>,
}

pub(crate) struct Digest256 {
    pub hash: String,
    pub bytes: u64,
    pub utf8_ok: bool,
    pub captured: Vec<u8>,
}

/// Takes the part of `available` up to the next `\n` into `line` (unless the line already passed `cap`, which is then
/// only flagged). Returns how many bytes to consume (the newline included) and whether the line ended.
fn buffer_line_part(
    line: &mut Vec<u8>,
    available: &[u8],
    cap: usize,
    too_long: &mut bool,
) -> (usize, bool) {
    let (take, done) = match available.iter().position(|b| *b == b'\n') {
        Some(i) => (i, true),
        None => (available.len(), false),
    };
    if !*too_long {
        if line.len() + take > cap {
            *too_long = true;
            line.clear();
        } else {
            line.extend_from_slice(&available[..take]);
        }
    }
    (if done { take + 1 } else { take }, done)
}

impl HashedReader {
    #[must_use]
    pub(crate) fn new(file: File) -> Self {
        Self {
            reader: BufReader::with_capacity(64 * 1024, file),
            hasher: Sha256::new(),
            bytes: 0,
            carry: Vec::new(),
            utf8_ok: true,
            capture_start: 0,
            capture_len: 0,
            captured: Vec::new(),
        }
    }

    /// Retains up to `len` bytes starting at absolute offset `start` from the same pass that hashes the file.
    pub(crate) fn capture(&mut self, start: u64, len: usize) {
        self.capture_start = start;
        self.capture_len = len;
        self.captured = Vec::with_capacity(len.min(1 << 20));
    }

    pub(crate) fn take_captured(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.captured)
    }

    #[must_use]
    pub(crate) fn position(&self) -> u64 {
        self.bytes
    }

    fn account(&mut self, amount: usize) {
        let chunk = &self.reader.buffer()[..amount];
        self.hasher.update(chunk);
        if self.capture_len > 0 {
            let (from, to) = (self.bytes, self.bytes + amount as u64);
            let (want_from, want_to) = (
                self.capture_start,
                self.capture_start + self.capture_len as u64,
            );
            let (lo, hi) = (from.max(want_from), to.min(want_to));
            if lo < hi {
                self.captured
                    .extend_from_slice(&chunk[(lo - from) as usize..(hi - from) as usize]);
            }
        }
        self.bytes += amount as u64;
        if self.utf8_ok {
            let owned: Vec<u8>;
            let slice: &[u8] = if self.carry.is_empty() {
                chunk
            } else {
                let mut joined = std::mem::take(&mut self.carry);
                joined.extend_from_slice(chunk);
                owned = joined;
                &owned[..]
            };
            match std::str::from_utf8(slice) {
                Ok(_) => {}
                Err(e) if e.error_len().is_none() => self.carry = slice[e.valid_up_to()..].to_vec(),
                Err(_) => self.utf8_ok = false,
            }
        }
        self.reader.consume(amount);
    }

    /// Reads the next line (without `\n`) into `line`. `Ok(None)` at end of file.
    pub(crate) fn next_line(
        &mut self,
        line: &mut Vec<u8>,
        cap: usize,
    ) -> std::io::Result<Option<Line>> {
        line.clear();
        let start = self.bytes;
        let mut too_long = false;
        let mut any = false;
        loop {
            let (amount, done) = {
                let available = self.reader.fill_buf()?;
                if available.is_empty() {
                    break;
                }
                any = true;
                buffer_line_part(line, available, cap, &mut too_long)
            };
            self.account(amount);
            if done {
                return Ok(Some(Line { start, too_long }));
            }
        }
        Ok(if any {
            Some(Line { start, too_long })
        } else {
            None
        })
    }

    /// Consumes the rest of the file (hashing/validating it) and returns the digest and the underlying handle.
    pub(crate) fn finish(mut self) -> std::io::Result<(Digest256, File)> {
        loop {
            let amount = self.reader.fill_buf()?.len();
            if amount == 0 {
                break;
            }
            self.account(amount);
        }
        let utf8_ok = self.utf8_ok && self.carry.is_empty();
        let captured = std::mem::take(&mut self.captured);
        let digest = Digest256 {
            hash: hex(self.hasher.finalize()),
            bytes: self.bytes,
            utf8_ok,
            captured,
        };
        Ok((digest, self.reader.into_inner()))
    }
}

/// Hashes a whole file by streaming it (used where only the digest is needed).
pub(crate) fn hash_file(file: File) -> Result<Digest256> {
    HashedReader::new(file)
        .finish()
        .map(|(digest, _)| digest)
        .map_err(|e| err("Cannot read file", e))
}

/// Skips `\r` at the end of a line and a UTF-8 byte-order mark at the start of the file.
#[must_use]
pub(crate) fn trim_line(bytes: &[u8], first: bool) -> &[u8] {
    let bytes = if first {
        bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes)
    } else {
        bytes
    };
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

#[cfg(test)]
mod tests {
    use super::{hex, percent_decode, percent_encode};

    #[test]
    fn byte_encodings_keep_their_wire_case_and_round_trip_unicode() {
        let bytes: Vec<u8> = (0..=255).collect();
        let expected: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(hex(&bytes), expected);
        assert_eq!(percent_encode("ø /%"), "%C3%B8%20%2F%25");
        assert_eq!(
            percent_decode(&percent_encode("ø /%")).as_deref(),
            Some("ø /%")
        );
    }
}
