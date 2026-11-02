//! Private, durable journal for the "reserve, then publish" note replacement (Windows, Linux and macOS).
//!
//! The vault never contains journal state: records and the immutable original-bytes backup live in an explicit,
//! app-private `journal_dir` (`<id>.json`, `<id>.base.md`). A record is written (and fsynced) *before* the locked
//! destination is renamed to its reserved sibling, so a crash between the two individual renames is recoverable:
//!
//! | observed at the pinned parent          | recovery                                                                  |
//! |----------------------------------------|---------------------------------------------------------------------------|
//! | name hash == new                       | committed: drop reservation (if base), staged (if new), backup, record     |
//! | name hash == base                      | never moved/restored: drop reservation (if base), staged (if new), backup, record |
//! | name present with any other hash/kind  | external edit: never touched; drop reservation (if base) and staged (if new); backup kept; record -> `conflicted` |
//! | name missing, reservation hash == base | roll back: create-only rename of the locked reservation handle to the name (a racing creator wins -> external edit above) |
//! | name missing, no usable reservation    | never resurrect: backup kept; record -> `conflicted`                       |
//!
//! Only objects whose hash equals the recorded base/new hash are ever deleted, and every lookup happens inside the
//! pinned parent. `conflicted` records are terminal: they are retained for diagnostics and recovery ignores them.
//! Messages never contain file content or vault paths.
use super::fsio;
use super::Result;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub(crate) const JOURNAL_VERSION: u32 = 1;
pub(crate) const STATE_PREPARED: &str = "prepared";
pub(crate) const STATE_CONFLICTED: &str = "conflicted";
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_MESSAGE_CHARS: usize = 1000;

/// One journaled publication. Everything except `state`/`message` is immutable after the first `save`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PublicationJournal {
    pub version: u32,
    pub id: String,
    pub root: PathBuf,
    pub root_identity: String,
    pub parents: Vec<String>,
    pub name: String,
    pub reservation: String,
    pub staged: String,
    pub base_hash: String,
    pub new_hash: String,
    pub state: String,
    pub backup: String,
    #[serde(default)]
    pub message: Option<String>,
}

pub(crate) fn reservation_name(id: &str) -> String {
    format!(".omp-reserved-{id}.md")
}
pub(crate) fn backup_name(id: &str) -> String {
    format!("{id}.base.md")
}

/// `.omp-<uuid>.tmp` with the staging file's own uuid (not necessarily the publication id).
fn valid_staged(name: &str) -> bool {
    name.strip_prefix(".omp-")
        .and_then(|n| n.strip_suffix(".tmp"))
        .is_some_and(valid_id)
}

static IN_FLIGHT: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
static RECOVERY: Mutex<()> = Mutex::new(());

/// Marks a publication id as owned by a live writer in this process; recovery never touches such records.
/// Begin it *before* writing the backup/`save` and drop it only *after* `finish`.
pub(crate) struct InFlight(String);

impl InFlight {
    pub(crate) fn begin(id: &str) -> Self {
        IN_FLIGHT.lock().insert(id.to_string());
        Self(id.to_string())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.lock().remove(&self.0);
    }
}

fn in_flight(id: &str) -> bool {
    IN_FLIGHT.lock().contains(id)
}

fn valid_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id)
        .map(|u| u.hyphenated().to_string() == id)
        .unwrap_or(false)
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Structural validation: nothing a record names may escape its pinned parent or deviate from the id-derived names.
pub(crate) fn validate(record: &PublicationJournal) -> Result<()> {
    if record.version != JOURNAL_VERSION {
        return Err("Unsupported publication journal version".into());
    }
    if !valid_id(&record.id) {
        return Err("Invalid publication journal id".into());
    }
    if !record.root.is_absolute() {
        return Err("Publication journal root must be absolute".into());
    }
    if record.root_identity.is_empty() || record.root_identity.len() > 200 {
        return Err("Invalid publication journal root identity".into());
    }
    let mut relative = record.parents.clone();
    relative.push(record.name.clone());
    if relative.iter().any(|c| c.contains(['/', '\\', ':', '\0'])) {
        return Err("Unsafe publication journal path".into());
    }
    let parsed = fsio::validate_relative(&relative.join("/"))
        .map_err(|_| "Unsafe publication journal path".to_string())?;
    if parsed != relative || !fsio::is_markdown_name(&record.name) {
        return Err("Unsafe publication journal path".into());
    }
    if record.reservation != reservation_name(&record.id)
        || record.backup != backup_name(&record.id)
        || !valid_staged(&record.staged)
    {
        return Err("Publication journal names do not match its id".into());
    }
    if !valid_hash(&record.base_hash) || !valid_hash(&record.new_hash) {
        return Err("Invalid publication journal hash".into());
    }
    if record.state != STATE_PREPARED && record.state != STATE_CONFLICTED {
        return Err("Invalid publication journal state".into());
    }
    if record
        .message
        .as_ref()
        .is_some_and(|m| m.chars().count() > MAX_MESSAGE_CHARS)
    {
        return Err("Oversized publication journal message".into());
    }
    Ok(())
}

fn record_path(journal_dir: &Path, id: &str) -> PathBuf {
    journal_dir.join(format!("{id}.json"))
}

fn sync_directory(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| fsio::err("Cannot flush journal folder", e))?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

fn ensure_outside_vault(journal_dir: &Path, root: &Path) -> Result<()> {
    if !journal_dir.is_absolute() {
        return Err("The publication journal folder must be absolute".into());
    }
    let inside = |inner: &Path, outer: &Path| fsio::same_or_inside(outer, inner);
    if inside(journal_dir, root) {
        return Err("The publication journal must stay outside the vault".into());
    }
    if let (Ok(j), Ok(r)) = (
        crate::setup::canonical(journal_dir),
        crate::setup::canonical(root),
    ) {
        if inside(&j, &r) {
            return Err("The publication journal must stay outside the vault".into());
        }
    }
    Ok(())
}

fn verify_backup(journal_dir: &Path, record: &PublicationJournal) -> Result<()> {
    let path = journal_dir.join(&record.backup);
    let meta =
        fs::symlink_metadata(&path).map_err(|e| fsio::err("Publication backup is missing", e))?;
    if !meta.is_file() {
        return Err("Publication backup is not a regular file".into());
    }
    let mut file =
        fs::File::open(&path).map_err(|e| fsio::err("Cannot read publication backup", e))?;
    let mut hasher = Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut chunk)
            .map_err(|e| fsio::err("Cannot read publication backup", e))?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
    }
    if fsio::hex(hasher.finalize()) != record.base_hash {
        return Err("Publication backup does not match the recorded base hash".into());
    }
    Ok(())
}

/// Durably stores (or atomically replaces) the record. The backup must already exist and match `base_hash`.
pub(crate) fn save(journal_dir: &Path, record: &PublicationJournal) -> Result<()> {
    validate(record)?;
    fs::create_dir_all(journal_dir)
        .map_err(|e| fsio::err("Cannot create publication journal folder", e))?;
    ensure_outside_vault(journal_dir, &record.root)?;
    verify_backup(journal_dir, record)?;
    let path = record_path(journal_dir, &record.id);
    match fs::symlink_metadata(&path) {
        Ok(meta) if !meta.is_file() => {
            return Err("Publication journal record is not a regular file".into())
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(fsio::err("Cannot inspect publication journal record", e)),
    }
    crate::dependency_security::atomic_json(&path, record, MAX_RECORD_BYTES)
        .map_err(|e| fsio::err("Cannot write publication journal record", e))?;
    // atomic_json fsynced the temporary file before the rename; flush the final name as well.
    OpenOptions::new()
        .write(true)
        .open(&path)
        .and_then(|f| f.sync_all())
        .map_err(|e| fsio::err("Cannot flush publication journal record", e))?;
    sync_directory(journal_dir)
}

/// Removes the record only (the caller owns the backup). A missing record is fine; non-regular objects are never followed.
pub(crate) fn finish(journal_dir: &Path, id: &str) -> Result<()> {
    remove_plain(journal_dir, &format!("{id}.json"), id)
}

/// Recovery's terminal success: the record, then its backup.
fn discard(journal_dir: &Path, id: &str) -> Result<()> {
    finish(journal_dir, id)?;
    remove_plain(journal_dir, &backup_name(id), id)
}

fn remove_plain(journal_dir: &Path, file_name: &str, id: &str) -> Result<()> {
    if !valid_id(id) {
        return Err("Invalid publication journal id".into());
    }
    let path = journal_dir.join(file_name);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => fs::remove_file(&path)
            .map_err(|e| fsio::err("Cannot remove publication journal file", e))?,
        Ok(_) => return Err("Publication journal entry is not a regular file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(fsio::err("Cannot inspect publication journal file", e)),
    }
    sync_directory(journal_dir)
}

enum Loaded {
    Valid(Box<PublicationJournal>),
    /// Structurally unusable (corrupt, tampered, oversized); can never become recoverable.
    Invalid,
}

fn record_ids(journal_dir: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(journal_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(fsio::err("Cannot read publication journal folder", e)),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| fsio::err("Cannot read publication journal folder", e))?;
        if let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.strip_suffix(".json"))
        {
            if valid_id(id) {
                ids.push(id.to_string());
            }
        }
    }
    ids.sort();
    Ok(ids)
}

fn load(journal_dir: &Path, id: &str) -> Result<Option<Loaded>> {
    let path = record_path(journal_dir, id);
    match fs::symlink_metadata(&path) {
        Ok(meta) if !meta.is_file() || meta.len() > MAX_RECORD_BYTES => {
            return Ok(Some(Loaded::Invalid))
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(fsio::err("Cannot inspect publication journal record", e)),
    }
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(fsio::err("Cannot read publication journal record", e)),
    };
    Ok(Some(
        match serde_json::from_slice::<PublicationJournal>(&bytes) {
            Ok(record) if record.id == id && validate(&record).is_ok() => {
                Loaded::Valid(Box::new(record))
            }
            _ => Loaded::Invalid,
        },
    ))
}

/// True iff a record is `prepared` (or unusable) and not owned by a live writer. Reads only.
pub(crate) fn needs_recovery(journal_dir: &Path) -> Result<bool> {
    for id in record_ids(journal_dir)? {
        if in_flight(&id) {
            continue;
        }
        match load(journal_dir, &id)? {
            None => {}
            Some(Loaded::Invalid) => return Ok(true),
            Some(Loaded::Valid(record)) if record.state == STATE_PREPARED => return Ok(true),
            Some(Loaded::Valid(_)) => {}
        }
    }
    Ok(false)
}

/// Retained `conflicted` records (diagnostics for the human); reads only.
#[cfg(test)]
pub(crate) fn conflicted(journal_dir: &Path) -> Result<Vec<PublicationJournal>> {
    let mut found = Vec::new();
    for id in record_ids(journal_dir)? {
        if let Some(Loaded::Valid(record)) = load(journal_dir, &id)? {
            if record.state == STATE_CONFLICTED {
                found.push(*record);
            }
        }
    }
    Ok(found)
}

/// Resolves every `prepared` record that no live writer owns. Idempotent; writes nothing when none are pending.
/// Retryable problems (locked files, replaced vault root, I/O) leave the record `prepared` and are reported in the
/// returned error. Unusable records are renamed to `<id>.rejected` (evidence kept) and reported once.
pub(crate) fn recover_publications(journal_dir: &Path) -> Result<()> {
    let _serialized = RECOVERY.lock();
    #[cfg(not(any(windows, unix)))]
    {
        if needs_recovery(journal_dir)? {
            return Err("Publication journal recovery requires handle-based rename support that this platform lacks".into());
        }
        return Ok(());
    }
    #[cfg(any(windows, unix))]
    {
        let mut problems = Vec::new();
        for id in record_ids(journal_dir)? {
            if in_flight(&id) {
                continue;
            }
            match load(journal_dir, &id) {
                Ok(None) => {}
                Ok(Some(Loaded::Invalid)) => {
                    problems.push(format!("{id}: {}", quarantine(journal_dir, &id)))
                }
                Ok(Some(Loaded::Valid(record))) if record.state != STATE_PREPARED => {}
                Ok(Some(Loaded::Valid(record))) => {
                    // A writer may have begun and finished between listing and now; its record is gone then.
                    if in_flight(&id)
                        || !matches!(load(journal_dir, &id), Ok(Some(Loaded::Valid(_))))
                    {
                        continue;
                    }
                    if let Err(e) = recover_record(journal_dir, &record) {
                        problems.push(format!("{id}: {e}"));
                    }
                }
                Err(e) => problems.push(format!("{id}: {e}")),
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }
}

#[cfg(any(windows, unix))]
fn quarantine(journal_dir: &Path, id: &str) -> String {
    let from = record_path(journal_dir, id);
    let mut to = journal_dir.join(format!("{id}.rejected"));
    if fs::symlink_metadata(&to).is_ok() {
        to = journal_dir.join(format!("{id}.{}.rejected", uuid::Uuid::new_v4()));
    }
    match fs::rename(&from, &to) {
        Ok(()) => "unusable publication journal record was set aside as .rejected".into(),
        Err(e) => fsio::err("Cannot set aside unusable publication journal record", e),
    }
}

#[cfg(any(windows, unix))]
enum Object {
    Absent,
    /// Present but not a plain regular file (directory, link): never opened, never touched.
    Foreign,
    File {
        file: super::rooted::Handle,
        hash: String,
    },
}

#[cfg(any(windows, unix))]
impl Object {
    fn hash(&self) -> Option<&str> {
        match self {
            Object::File { hash, .. } => Some(hash),
            _ => None,
        }
    }
    fn present(&self) -> bool {
        !matches!(self, Object::Absent)
    }
    /// Deletes the object through its locked handle, only when its hash is `expected`.
    fn delete_if(&self, expected: &str) -> Result<()> {
        match self {
            Object::File { file, hash } if hash == expected => super::rooted::delete_handle(file),
            _ => Ok(()),
        }
    }
}

#[cfg(any(windows, unix))]
fn probe(pins: &super::rooted::DirectoryPins, name: &str) -> Result<Object> {
    use crate::code_quality::snapshot;
    let path = pins.path().join(name);
    let missing = |path: &Path| matches!(fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Object::Absent),
        Err(e) => return Err(fsio::err("Cannot inspect journaled note entry", e)),
        Ok(meta) if !meta.is_file() || snapshot::is_link(&meta) => return Ok(Object::Foreign),
        Ok(_) => {}
    }
    match super::rooted::lock_destination(&path) {
        Ok(mut file) => {
            let hash = super::rooted::hash_handle(&mut file)?;
            Ok(Object::File { file, hash })
        }
        Err(_) if missing(&path) => Ok(Object::Absent),
        Err(e) => Err(e),
    }
}

#[cfg(any(windows, unix))]
fn recover_record(journal_dir: &Path, record: &PublicationJournal) -> Result<()> {
    use super::rooted;
    // Pin the whole chain once (the deepest folder shares write so the OS may open it for the rename). A second,
    // read-only pin of the same folder would itself deny that write open. The root is then pinned and cannot be swapped
    // between this identity check and the work below.
    let pins = rooted::pin_descendants(
        &record.root,
        &record.parents,
        /* create */ false,
        /* writable_leaf */ true,
    )?;
    if fsio::dir_identity(&record.root).as_deref() != Some(record.root_identity.as_str()) {
        return Err("The vault root was replaced or moved since this publication was journaled; recovery refused".into());
    }
    let target = probe(&pins, &record.name)?;
    let reservation = probe(&pins, &record.reservation)?;
    let staged = probe(&pins, &record.staged)?;
    let (base, new) = (record.base_hash.as_str(), record.new_hash.as_str());
    let conflict = |reservation: &Object, staged: &Object, message: &str| -> Result<()> {
        reservation.delete_if(base)?;
        staged.delete_if(new)?;
        let mut marked = record.clone();
        marked.state = STATE_CONFLICTED.to_string();
        marked.message = Some(format!("{message} The original bytes are retained in {} inside the private publication journal.", record.backup));
        save(journal_dir, &marked)
    };
    const EXTERNAL: &str = "The note was changed outside the app while its replacement was being published; that note was left untouched.";
    match target.hash() {
        Some(hash) if hash == new || hash == base => {
            reservation.delete_if(base)?;
            staged.delete_if(new)?;
            return discard(journal_dir, &record.id);
        }
        _ => {}
    }
    if target.present() {
        return conflict(&reservation, &staged, EXTERNAL);
    }
    match &reservation {
        Object::File { file, hash } if hash == base => {
            if let Err(error) = rooted::rename_handle(file, &pins, &record.name, /* replace */ false) {
                // Only a creator that won the empty name changes the outcome; anything else is retried later.
                let now = probe(&pins, &record.name)?;
                if !now.present() { return Err(error); }
                return conflict(&reservation, &staged, EXTERNAL);
            }
            staged.delete_if(new)?;
            discard(journal_dir, &record.id)
        }
        Object::Absent => conflict(&reservation, &staged, "The note is missing and its reservation is gone; the original was not recreated."),
        _ => conflict(&reservation, &staged, "The note is missing and its reservation no longer holds the original bytes; it was left untouched."),
    }
}
