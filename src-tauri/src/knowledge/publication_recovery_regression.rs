//! Deterministic recovery regressions for the private publication journal. Every fixture is a throwaway folder; the
//! on-disk states a crash can leave behind are staged with plain file operations, no foreign processes involved.
use super::fsio::{self, sha256_hex};
use super::publication_journal::*;
use std::fs;
use std::path::{Path, PathBuf};

const BASE: &[u8] = b"original note\n";
const NEW: &[u8] = b"replacement note\n";
const FOREIGN: &[u8] = b"written by somebody else\n";

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("omp publication recovery ø {}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(crate::setup::canonical(&path).unwrap())
    }
}

impl Drop for Temp {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

struct Fixture {
    _temp: Temp,
    root: PathBuf,
    journal: PathBuf,
    dir: PathBuf,
    record: PublicationJournal,
}

fn fixture(parents: &[&str]) -> Fixture {
    let temp = Temp::new();
    let root = temp.0.join("vault");
    let journal = temp.0.join("private").join("journal");
    let parents: Vec<String> = parents.iter().map(|p| p.to_string()).collect();
    let mut dir = root.clone();
    for parent in &parents { dir.push(parent); }
    fs::create_dir_all(&dir).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    write_backup(&journal, &id, BASE);
    let identity = fsio::dir_identity(&root).unwrap();
    let record = prepared(&id, &root, &identity, &parents, "note.md", &sha256_hex(BASE), &sha256_hex(NEW));
    save(&journal, &record).unwrap();
    Fixture { _temp: temp, root, journal, dir, record }
}

fn put(dir: &Path, name: &str, bytes: &[u8]) { fs::write(dir.join(name), bytes).unwrap(); }
fn exists(dir: &Path, name: &str) -> bool { fs::symlink_metadata(dir.join(name)).is_ok() }
fn read(dir: &Path, name: &str) -> Vec<u8> { fs::read(dir.join(name)).unwrap() }

fn write_backup(journal: &Path, id: &str, bytes: &[u8]) {
    fs::create_dir_all(journal).unwrap();
    fs::write(journal.join(backup_name(id)), bytes).unwrap();
}

/// The staged name carries its own uuid, independent of the publication id.
fn prepared(id: &str, root: &Path, identity: &str, parents: &[String], name: &str, base: &str, new: &str) -> PublicationJournal {
    PublicationJournal {
        version: 1,
        id: id.to_string(),
        root: root.to_path_buf(),
        root_identity: identity.to_string(),
        parents: parents.to_vec(),
        name: name.to_string(),
        reservation: reservation_name(id),
        staged: format!(".omp-{}.tmp", uuid::Uuid::new_v4()),
        base_hash: base.to_string(),
        new_hash: new.to_string(),
        state: "prepared".to_string(),
        backup: backup_name(id),
        message: None,
    }
}

#[test]
fn record_serializes_camel_case_and_validates_names() {
    let f = fixture(&["notes"]);
    let text = fs::read_to_string(f.journal.join(format!("{}.json", f.record.id))).unwrap();
    for key in ["rootIdentity", "baseHash", "newHash", "reservation", "staged", "backup", "parents"] {
        assert!(text.contains(&format!("\"{key}\"")), "{key}");
    }
    let mut bad = f.record.clone();
    bad.reservation = reservation_name(&uuid::Uuid::new_v4().to_string());
    assert!(save(&f.journal, &bad).is_err());
    let mut bad = f.record.clone();
    bad.parents = vec!["..".into()];
    assert!(save(&f.journal, &bad).is_err());
    let mut bad = f.record.clone();
    bad.name = "sub/note.md".into();
    assert!(save(&f.journal, &bad).is_err());
    let mut bad = f.record.clone();
    bad.base_hash = "ABC".into();
    assert!(save(&f.journal, &bad).is_err());
}

#[test]
fn journal_must_stay_outside_the_vault_and_match_the_backup() {
    let f = fixture(&[]);
    let inside = f.root.join("journal");
    let id = uuid::Uuid::new_v4().to_string();
    write_backup(&inside, &id, BASE);
    let record = prepared(&id, &f.root, &f.record.root_identity, &[], "note.md", &sha256_hex(BASE), &sha256_hex(NEW));
    assert!(save(&inside, &record).is_err());
    // A backup holding other bytes than the recorded base hash is refused.
    let other = uuid::Uuid::new_v4().to_string();
    write_backup(&f.journal, &other, b"different");
    let record = prepared(&other, &f.root, &f.record.root_identity, &[], "note.md", &sha256_hex(BASE), &sha256_hex(NEW));
    assert!(save(&f.journal, &record).is_err());
    let mut bad = f.record.clone();
    bad.staged = ".omp-not-a-uuid.tmp".into();
    assert!(save(&f.journal, &bad).is_err());
    assert_eq!(fs::read(f.journal.join(&f.record.backup)).unwrap(), BASE);
}

#[test]
fn finish_removes_only_the_record_and_is_repeatable() {
    let f = fixture(&[]);
    assert!(needs_recovery(&f.journal).unwrap());
    finish(&f.journal, &f.record.id).unwrap();
    finish(&f.journal, &f.record.id).unwrap();
    assert!(!needs_recovery(&f.journal).unwrap());
    assert!(f.journal.join(&f.record.backup).exists(), "the caller owns the backup");
    assert!(finish(&f.journal, "../escape").is_err());
}

#[test]
fn needs_recovery_reads_only_and_skips_in_flight_ids() {
    let temp = Temp::new();
    let missing = temp.0.join("never created");
    assert!(!needs_recovery(&missing).unwrap());
    recover_publications(&missing).unwrap();
    assert!(!missing.exists(), "an empty journal must not create anything");
    let f = fixture(&[]);
    {
        let _guard = InFlight::begin(&f.record.id);
        assert!(!needs_recovery(&f.journal).unwrap());
        put(&f.dir, &f.record.reservation, BASE);
        put(&f.dir, &f.record.staged, NEW);
        recover_publications(&f.journal).unwrap();
        assert!(exists(&f.dir, &f.record.reservation), "a live writer's reservation must not be recovered");
        assert!(exists(&f.dir, &f.record.staged));
    }
    assert!(needs_recovery(&f.journal).unwrap());
}

#[cfg(not(any(windows, unix)))]
#[test]
fn unsupported_platform_refuses_pending_records_but_accepts_an_empty_journal() {
    let f = fixture(&[]);
    let error = recover_publications(&f.journal).unwrap_err();
    assert!(error.contains("this platform lacks"), "{error}");
    finish(&f.journal, &f.record.id).unwrap();
    recover_publications(&f.journal).unwrap();
}

// The on-disk recovery table is identical on Windows, Linux and macOS (only the way objects are held differs).
#[cfg(any(windows, unix))]
mod windows {
    use super::*;

    #[test]
    fn crash_after_reserving_target_before_source_published_restores_the_original() {
        let f = fixture(&["notes", "deep"]);
        // Target renamed to its reservation; the staged note exists; the name is empty.
        put(&f.dir, &f.record.reservation, BASE);
        put(&f.dir, &f.record.staged, NEW);
        recover_publications(&f.journal).unwrap();
        assert_eq!(read(&f.dir, "note.md"), BASE);
        assert!(!exists(&f.dir, &f.record.reservation));
        assert!(!exists(&f.dir, &f.record.staged));
        assert!(!f.journal.join(format!("{}.json", f.record.id)).exists());
        assert!(!f.journal.join(&f.record.backup).exists());
        recover_publications(&f.journal).unwrap();
        assert_eq!(read(&f.dir, "note.md"), BASE, "recovery is idempotent");
    }

    #[test]
    fn foreign_file_created_in_the_gap_is_never_overwritten() {
        let f = fixture(&[]);
        put(&f.dir, &f.record.reservation, BASE);
        put(&f.dir, &f.record.staged, NEW);
        put(&f.dir, "note.md", FOREIGN);
        recover_publications(&f.journal).unwrap();
        assert_eq!(read(&f.dir, "note.md"), FOREIGN);
        assert!(!exists(&f.dir, &f.record.reservation));
        assert!(!exists(&f.dir, &f.record.staged));
        // The original stays recoverable from the private backup; the record is a terminal diagnostic.
        assert_eq!(fs::read(f.journal.join(&f.record.backup)).unwrap(), BASE);
        let conflicts = conflicted(&f.journal).unwrap();
        assert_eq!(conflicts.len(), 1);
        let message = conflicts[0].message.as_deref().unwrap();
        assert!(message.contains(&f.record.backup));
        assert!(!message.contains("note.md") && !message.contains("written by somebody"));
        // Terminal: neither pending nor reprocessed, no loop.
        assert!(!needs_recovery(&f.journal).unwrap());
        recover_publications(&f.journal).unwrap();
        assert_eq!(read(&f.dir, "note.md"), FOREIGN);
        assert_eq!(conflicted(&f.journal).unwrap().len(), 1);
    }

    #[test]
    fn committed_publication_drops_the_reservation_and_the_journal() {
        let f = fixture(&[]);
        put(&f.dir, "note.md", NEW);
        put(&f.dir, &f.record.reservation, BASE);
        recover_publications(&f.journal).unwrap();
        assert_eq!(read(&f.dir, "note.md"), NEW);
        assert!(!exists(&f.dir, &f.record.reservation));
        assert!(!f.journal.join(&f.record.backup).exists());
        assert!(!needs_recovery(&f.journal).unwrap());
    }

    #[test]
    fn publication_that_never_moved_only_cleans_up_what_is_its_own() {
        let f = fixture(&[]);
        put(&f.dir, "note.md", BASE);
        put(&f.dir, &f.record.staged, NEW);
        recover_publications(&f.journal).unwrap();
        assert_eq!(read(&f.dir, "note.md"), BASE);
        assert!(!exists(&f.dir, &f.record.staged));
        assert!(!needs_recovery(&f.journal).unwrap());
    }

    #[test]
    fn external_edit_after_commit_or_before_move_is_preserved() {
        let f = fixture(&[]);
        put(&f.dir, "note.md", FOREIGN);
        put(&f.dir, &f.record.staged, NEW);
        recover_publications(&f.journal).unwrap();
        assert_eq!(read(&f.dir, "note.md"), FOREIGN);
        assert!(!exists(&f.dir, &f.record.staged));
        assert_eq!(conflicted(&f.journal).unwrap().len(), 1);
        assert!(f.journal.join(&f.record.backup).exists());
    }

    #[test]
    fn files_that_are_not_ours_are_never_deleted() {
        let f = fixture(&[]);
        // Missing name, reservation and staged objects carry other bytes than the recorded base/new hashes.
        put(&f.dir, &f.record.reservation, FOREIGN);
        put(&f.dir, &f.record.staged, FOREIGN);
        recover_publications(&f.journal).unwrap();
        assert!(!exists(&f.dir, "note.md"), "a foreign reservation is never promoted to the name");
        assert_eq!(read(&f.dir, &f.record.reservation), FOREIGN);
        assert_eq!(read(&f.dir, &f.record.staged), FOREIGN);
        assert_eq!(conflicted(&f.journal).unwrap().len(), 1);
    }

    #[test]
    fn missing_name_without_reservation_is_not_resurrected() {
        let f = fixture(&[]);
        put(&f.dir, &f.record.staged, NEW);
        recover_publications(&f.journal).unwrap();
        assert!(!exists(&f.dir, "note.md"));
        assert!(!exists(&f.dir, &f.record.staged));
        assert_eq!(fs::read(f.journal.join(&f.record.backup)).unwrap(), BASE);
        assert_eq!(conflicted(&f.journal).unwrap().len(), 1);
    }

    #[test]
    fn replaced_vault_root_refuses_recovery_and_touches_nothing() {
        let f = fixture(&["notes"]);
        put(&f.dir, &f.record.reservation, BASE);
        put(&f.dir, &f.record.staged, NEW);
        let moved = f.root.with_file_name("vault-moved");
        fs::rename(&f.root, &moved).unwrap();
        fs::create_dir_all(f.root.join("notes")).unwrap();
        put(&f.root.join("notes"), &f.record.reservation, BASE);
        let error = recover_publications(&f.journal).unwrap_err();
        assert!(error.contains("replaced or moved"), "{error}");
        assert!(!exists(&f.root.join("notes"), "note.md"));
        assert!(exists(&moved.join("notes"), &f.record.reservation));
        assert!(exists(&moved.join("notes"), &f.record.staged));
        assert!(needs_recovery(&f.journal).unwrap(), "the record stays pending for a later retry");
        // The identity string itself is verified, not only the path.
        fs::remove_dir_all(&f.root).unwrap();
        fs::rename(&moved, &f.root).unwrap();
        let mut forged = f.record.clone();
        forged.root_identity = "ffffffff:ffffffffffffffff".into();
        save(&f.journal, &forged).unwrap();
        assert!(recover_publications(&f.journal).is_err());
        assert!(exists(&f.dir, &f.record.reservation));
        save(&f.journal, &f.record).unwrap();
        recover_publications(&f.journal).unwrap();
        assert_eq!(read(&f.dir, "note.md"), BASE);
    }

    #[test]
    fn unusable_records_are_set_aside_not_trusted_or_looped_on() {
        let f = fixture(&[]);
        put(&f.dir, &f.record.reservation, BASE);
        // Tampered: reservation points at a sibling that does not carry this id.
        let path = f.journal.join(format!("{}.json", f.record.id));
        let text = fs::read_to_string(&path).unwrap().replace(&f.record.reservation, "note.md");
        fs::write(&path, text).unwrap();
        assert!(recover_publications(&f.journal).is_err());
        assert!(!path.exists());
        assert!(f.journal.join(format!("{}.rejected", f.record.id)).exists());
        assert!(exists(&f.dir, &f.record.reservation), "an untrusted record never causes file operations");
        recover_publications(&f.journal).unwrap();
    }
}
