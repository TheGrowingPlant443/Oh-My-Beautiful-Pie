//! Shared Markdown publication for admitted vault roots and backend-derived native deployment roots.
//!
//! Guarantees (Windows: operating-system sharing locks; Linux/macOS: see the last paragraph; a platform with neither
//! returns an explicit "unsupported" error and writes nothing):
//!
//! * **Create** (`expected_hash == None`): the note is staged and published with a create-only rename. An existing
//!   note, or one created concurrently, is never replaced.
//! * **Replace** (`Some(hash)`): the destination is opened READ|DELETE with sharing READ only, so from that moment no
//!   other process can write, rename or delete it. Its hash is verified on that locked handle. The *exact locked
//!   object* is then renamed to a reserved sibling (create-only) and the staged note is published into the freed name
//!   (create-only). Neither rename can overwrite anything, so a note a foreign process creates in the short window
//!   where the name is empty always wins: our publication fails, the original is restored create-only, and when even
//!   that is impossible the original bytes stay in an immutable private backup (`<journal_dir>/<id>.base.md`, outside
//!   the vault) which the error names. The two renames are individually atomic; the name is *missing* for the instant
//!   between them (readers can briefly see "not found"), and a crash in that gap is repaired by
//!   [`recover_publications`] from a durable private journal that is written *before* the first move.
//! * **Delete**: the exact locked, hash-verified object is deleted through its handle; a different file that carries
//!   the name afterwards is never touched.
//!
//! **Linux/macOS** have no mandatory locks, so "locked" there means: the destination is opened without following links
//! and hashed, then the *name* is moved with an exclusive rename (`renameat2(RENAME_NOREPLACE)` / `renameatx_np`) and the
//! moved object is verified by device+inode and by re-hashing the bytes behind the handle. A different object or a note
//! another program edited meanwhile is put back untouched and the replacement fails; nothing is ever overwritten. Only a
//! writer that already held the note open and writes after that check lands in the superseded original (see `rooted_unix`).
//!
//! App writers of one note are additionally serialized in-process. Journals/backups never live in a vault.
use super::publication_journal::{self, InFlight, PublicationJournal};
use super::{fsio, Result};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, LazyLock, Weak};

static NOTES: LazyLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Marks a simulated process death: the replacement stops exactly where it is, with nothing cleaned up.
const CRASH_MARKER: &str = "simulated crash";

/// Test-only fault injection, keyed by the normalised parent folder so parallel tests never see each other's hook.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Stage {
    /// The destination is locked and its hash verified; nothing has moved.
    Locked,
    /// The original was moved to its reservation; the name is empty; nothing was published yet.
    Reserved,
}
#[cfg(test)]
pub(crate) enum HookAction {
    Continue,
    Crash,
}
#[cfg(test)]
pub(crate) type Hook = Box<dyn Fn(Stage, &Path, &str) -> HookAction + Send>;
#[cfg(test)]
pub(crate) static HOOKS: LazyLock<Mutex<HashMap<String, Hook>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(any(windows, unix))]
fn crash_requested(stage: u8, parent: &Path, name: &str) -> bool {
    #[cfg(test)]
    {
        let stage = if stage == 0 {
            Stage::Locked
        } else {
            Stage::Reserved
        };
        if let Some(hook) = HOOKS.lock().get(&fsio::norm(parent)) {
            return matches!(hook(stage, parent, name), HookAction::Crash);
        }
    }
    let _ = (stage, parent, name);
    false
}

fn note_lock(root: &Path, relative: &str) -> Arc<Mutex<()>> {
    let key = format!("{}\0{}", fsio::norm(root), relative.to_lowercase());
    let mut notes = NOTES.lock();
    notes.retain(|_, value| value.strong_count() > 0);
    if let Some(lock) = notes.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    notes.insert(key, Arc::downgrade(&lock));
    lock
}

/// Human-reviewed Markdown publication; `expected_hash == None` is create-only, `Some(hash)` a checked replacement.
/// `journal_dir` is a private application folder (never inside a vault) that holds the crash journal and the
/// original-bytes backup of a replacement.
pub(crate) fn write_relative_markdown_journaled(
    root: &Path,
    relative: &str,
    text: &str,
    expected_hash: Option<&str>,
    journal_dir: &Path,
) -> Result<()> {
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (root, relative, text, expected_hash, journal_dir);
        return Err("Conflict-safe Markdown publication requires handle-based rename support that this platform lacks".into());
    }
    #[cfg(any(windows, unix))]
    {
        let components = fsio::validate_relative(relative)?;
        let (name, parents) = components.split_last().ok_or("A note path is required")?;
        if !fsio::is_markdown_name(name) {
            return Err("Only Markdown documents may be published".into());
        }
        let lock = note_lock(root, relative);
        let _note = lock.lock();
        let parent = fsio::pin_descendants(
            root, parents, /* create */ true, /* writable_leaf */ true,
        )?;
        let folded_name = name.to_lowercase();
        for entry in fs::read_dir(parent.path())
            .map_err(|e| fsio::err("Cannot inspect note directory", e))?
        {
            let entry = entry.map_err(|e| fsio::err("Cannot inspect note directory", e))?;
            if let Some(existing) = entry.file_name().to_str() {
                if existing != name && existing.to_lowercase() == folded_name {
                    return Err("A document differing only by letter case already exists".into());
                }
            }
        }
        match expected_hash {
            None => create_only(&parent, name, text.as_bytes()),
            Some(expected) => replace_checked(
                root,
                parents,
                &parent,
                name,
                text.as_bytes(),
                expected,
                journal_dir,
            ),
        }
    }
}

#[cfg(any(windows, unix))]
fn create_only(parent: &fsio::DirectoryPins, name: &str, text: &[u8]) -> Result<()> {
    let staged = super::rooted::stage(parent, text, &format!(".omp-{}.tmp", uuid::Uuid::new_v4()))?;
    let published =
        super::rooted::rename_handle(&staged.file, parent, name, /* replace */ false);
    if published.is_err() {
        let _ = super::rooted::delete_handle(&staged.file);
    }
    published
}

/// Immutable private copy of the original bytes, made from the locked handle BEFORE anything moves.
#[cfg(any(windows, unix))]
fn backup_original<R: std::io::Read + std::io::Seek>(
    locked: &mut R,
    journal_dir: &Path,
    backup_name: &str,
) -> Result<()> {
    use std::io::{SeekFrom, Write};
    fs::create_dir_all(journal_dir)
        .map_err(|e| fsio::err("Cannot create the private publication journal", e))?;
    let mut backup = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(journal_dir.join(backup_name))
        .map_err(|e| fsio::err("Cannot back up the original note", e))?;
    std::io::copy(locked, &mut backup)
        .map_err(|e| fsio::err("Cannot back up the original note", e))?;
    backup
        .flush()
        .and_then(|_| backup.sync_all())
        .map_err(|e| fsio::err("Cannot back up the original note", e))?;
    locked
        .seek(SeekFrom::Start(0))
        .map_err(|e| fsio::err("Cannot rewind the locked note", e))?;
    Ok(())
}

/// The staged note could not take the name. A foreign process (or anything else) occupied it in the gap: never
/// overwrite it; put the original back only if the name is still free, otherwise keep the foreign note and the
/// original's backup. Always an error.
#[cfg(any(windows, unix))]
fn undo_failed_publication(
    locked: &super::rooted::Handle,
    parent: &fsio::DirectoryPins,
    journal_dir: &Path,
    record: &mut PublicationJournal,
    publish_error: String,
) -> Result<()> {
    match super::rooted::rename_handle(locked, parent, &record.name, /* replace */ false) {
        Ok(()) => {
            let _ = publication_journal::finish(journal_dir, &record.id);
            let _ = fs::remove_file(journal_dir.join(&record.backup));
            Err(format!("The note changed while it was being replaced; the original was restored and nothing was overwritten: {publish_error}"))
        }
        Err(restore_error) => {
            let message = format!(
                "The note was replaced by someone else while it was being published. Their version was kept untouched; the original is saved at {} ({publish_error}; restore: {restore_error})",
                journal_dir.join(&record.backup).display()
            );
            record.state = "conflicted".into();
            record.message = Some(message.clone());
            let _ = publication_journal::save(journal_dir, record);
            let _ = super::rooted::delete_handle(locked); // the reservation duplicates the immutable backup
            Err(message)
        }
    }
}

#[cfg(any(windows, unix))]
fn replace_checked(
    root: &Path,
    parents: &[String],
    parent: &fsio::DirectoryPins,
    name: &str,
    text: &[u8],
    expected: &str,
    journal_dir: &Path,
) -> Result<()> {
    let id = uuid::Uuid::new_v4().to_string();
    // Recovery must never mistake this live writer for a crashed one: guard the id for the whole replacement.
    let _inflight = InFlight::begin(&id);
    let staged = super::rooted::stage(parent, text, &format!(".omp-{}.tmp", uuid::Uuid::new_v4()))?;
    let outcome = (|| -> Result<()> {
        let mut locked = super::rooted::lock_destination(&parent.path().join(name))?;
        let base_hash = super::rooted::hash_handle(&mut locked)?;
        if base_hash != expected {
            return Err("The note changed since it was read; review it before replacing".into());
        }
        if crash_requested(0, parent.path(), name) {
            return Err(CRASH_MARKER.into());
        }
        let backup_name = publication_journal::backup_name(&id);
        backup_original(&mut locked, journal_dir, &backup_name)?;
        let discard_backup = || {
            let _ = fs::remove_file(journal_dir.join(&backup_name));
        };
        let reservation = publication_journal::reservation_name(&id);
        let mut record = PublicationJournal {
            version: publication_journal::JOURNAL_VERSION,
            id: id.clone(),
            root: root.to_path_buf(),
            root_identity: parent_root_identity(root)?,
            parents: parents.to_vec(),
            name: name.to_string(),
            reservation,
            staged: staged.name.clone(),
            base_hash,
            new_hash: fsio::sha256_hex(text),
            state: publication_journal::STATE_PREPARED.to_string(),
            backup: backup_name.clone(),
            message: None,
        };
        if let Err(e) = publication_journal::save(journal_dir, &record) {
            discard_backup();
            return Err(e);
        }
        // Step 1: move the exact locked original out of the way (create-only). Nothing is lost if this fails.
        if let Err(e) = super::rooted::reserve_handle(&locked, parent, &record.reservation) {
            let _ = publication_journal::finish(journal_dir, &id);
            discard_backup();
            return Err(format!("The note could not be reserved for replacement; nothing was overwritten or lost: {e}"));
        }
        if crash_requested(1, parent.path(), name) {
            return Err(CRASH_MARKER.into());
        }
        // Step 2: publish the staged note into the now-empty name (create-only).
        match super::rooted::rename_handle(&staged.file, parent, name, /* replace */ false) {
            Ok(()) => {
                let _ = super::rooted::delete_handle(&locked); // the superseded original (we still hold its handle)
                let _ = publication_journal::finish(journal_dir, &id);
                discard_backup();
                Ok(())
            }
            Err(publish_error) => {
                undo_failed_publication(&locked, parent, journal_dir, &mut record, publish_error)
            }
        }
    })();
    // A simulated process death leaves everything exactly as it was, like a real one would.
    if outcome.as_ref().is_err_and(|e| e != CRASH_MARKER) {
        let _ = super::rooted::delete_handle(&staged.file);
    }
    outcome
}

#[cfg(any(windows, unix))]
fn parent_root_identity(root: &Path) -> Result<String> {
    fsio::dir_identity(root).ok_or_else(|| "The vault root has no stable identity".to_string())
}

/// Repairs publications that a crash interrupted. Idempotent; writes nothing when no record is pending and never
/// touches a record whose writer is still running in this process.
pub(crate) fn recover_publications(journal_dir: &Path) -> Result<()> {
    publication_journal::recover_publications(journal_dir)
}

/// Read-only: is there an unfinished publication record in `journal_dir`?
pub(crate) fn publications_need_recovery(journal_dir: &Path) -> Result<bool> {
    publication_journal::needs_recovery(journal_dir)
}

/// Deletes only an unchanged, singly linked Markdown file; the OS lock is held through deletion by handle.
pub(crate) fn remove_relative_checked(
    root: &Path,
    relative: &str,
    expected_hash: &str,
) -> Result<()> {
    let components = fsio::validate_relative(relative)?;
    let (name, parents) = components.split_last().ok_or("A note path is required")?;
    if !fsio::is_markdown_name(name) {
        return Err("Only Markdown documents may be removed".into());
    }
    let lock = note_lock(root, relative);
    let _note = lock.lock();
    let parent = fsio::pin_descendants(
        root, parents, /* create */ false, /* writable_leaf */ false,
    )?;
    super::rooted::remove_checked(&parent, name, expected_hash)
}
