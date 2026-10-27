//! Admission, immutable copy and staleness comparison of a repository folder. Repository bytes are only
//! read and hashed here; nothing from the repository is executed, expanded or followed through links.
use super::model::{file_id, ManifestFile, Result};
use crate::code_quality::runtime::Ctl;
use crate::code_quality::snapshot::{is_link, list_source_tree, open_plain, relative_source_name, MAX_BYTES, MAX_FILES};
use crate::project_insight::evidence_excluded;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Parser admission: larger files stay in the inventory (hashed and copied) but are not parsed.
pub(crate) const MAX_PARSE_BYTES: u64 = 2 * 1024 * 1024;
const BINARY_PROBE: usize = 8 * 1024;
const EXCLUSION_SAMPLES: usize = 20;

pub(crate) struct AdmittedFile { pub path: PathBuf, pub rel: String, pub bytes: u64, pub modified_ns: u64 }

pub(crate) struct Admitted {
    pub files: Vec<AdmittedFile>,
    pub directories: Vec<String>,
    /// Sample of paths refused by the evidence policy (secrets, build output) plus the total count.
    pub excluded_samples: Vec<String>,
    pub excluded_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InventoryFile {
    pub id: String,
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub modified_ns: u64,
    pub status: String,
    pub diagnostics: Vec<String>,
}

/// Persisted next to the copied source; the immutable record every later query and staleness check uses.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InventoryRecord {
    pub version: u32,
    pub snapshot_id: String,
    pub folder: String,
    pub created_at: u64,
    pub source_hash: String,
    pub files: Vec<InventoryFile>,
    pub directories: Vec<String>,
    pub exclusions: Vec<String>,
    pub limitations: Vec<String>,
}

impl InventoryRecord {
    pub fn manifest_files(&self) -> Vec<ManifestFile> {
        self.files.iter().map(|f| ManifestFile {
            id: f.id.clone(), path: f.path.clone(), sha256: f.sha256.clone(), bytes: f.bytes, language: None,
            status: f.status.clone(), diagnostics: f.diagnostics.clone(),
        }).collect()
    }
}

fn modified_ns(meta: &fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos().min(u64::MAX as u128) as u64).unwrap_or(0)
}

fn policy_excluded(rel: &str, directory: bool) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    let last = parts.len() - 1;
    parts.iter().enumerate().any(|(i, part)| evidence_excluded(part, i < last || directory))
}

/// Lists, filters and names the admitted entries below the canonical `root`. Metadata only.
pub(crate) fn admit(root: &Path, ctl: &Ctl) -> Result<Admitted> {
    let listing = list_source_tree(root, &[], None, MAX_FILES, true, ctl)?;
    let mut warnings = listing.warnings;
    for path in &listing.special { warnings.push(format!("Skipped special filesystem entry: {}", path.display())); }
    let mut excluded = Exclusions::default();
    let mut directories = admit_directories(root, &listing.directories, &mut warnings, &mut excluded);
    let mut files = admit_files(root, listing.paths, ctl, &mut warnings, &mut excluded)?;
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    directories.sort();
    // Distinct paths must stay distinct in the snapshot; two names differing only by case would merge on
    // a case-insensitive filesystem, so the attempt reports the conflict instead.
    let conflicts = case_conflicts(files.iter().map(|f| f.rel.as_str()).chain(directories.iter().map(String::as_str)));
    if !conflicts.is_empty() {
        return Err(format!("Paths that differ only by letter case cannot be kept distinct in the snapshot: {}", conflicts.join(", ")));
    }
    Ok(Admitted { files, directories, excluded_samples: excluded.samples, excluded_count: excluded.count, warnings })
}

#[derive(Default)]
struct Exclusions { samples: Vec<String>, count: usize }

impl Exclusions {
    fn note(&mut self, rel: &str) {
        self.count += 1;
        if self.samples.len() < EXCLUSION_SAMPLES { self.samples.push(rel.to_string()); }
    }
}

fn relative_name(root: &Path, path: &Path) -> Result<String> {
    path.strip_prefix(root).map_err(|_| "Source path escaped the selected folder".to_string()).and_then(relative_source_name)
}

fn admit_directories(root: &Path, listed: &[PathBuf], warnings: &mut Vec<String>, excluded: &mut Exclusions) -> Vec<String> {
    let mut directories = Vec::new();
    for path in listed {
        let rel = match relative_name(root, path) {
            Ok(rel) => rel,
            Err(e) => { warnings.push(format!("Skipped directory {}: {e}", path.display())); continue; }
        };
        if policy_excluded(&rel, true) { excluded.note(&rel); continue; }
        directories.push(rel);
    }
    directories
}

fn admit_files(root: &Path, listed: Vec<PathBuf>, ctl: &Ctl, warnings: &mut Vec<String>, excluded: &mut Exclusions) -> Result<Vec<AdmittedFile>> {
    let mut files = Vec::new();
    for path in listed {
        if ctl.cancelled() { return Err("Source index cancelled".into()); }
        let rel = match relative_name(root, &path) {
            Ok(rel) => rel,
            Err(e) => { warnings.push(format!("Skipped file {}: {e}", path.display())); continue; }
        };
        if policy_excluded(&rel, false) { excluded.note(&rel); continue; }
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("Cannot inspect {rel}: {e}"))?;
        if is_link(&meta) || !meta.is_file() { warnings.push(format!("Skipped non-regular entry: {rel}")); continue; }
        files.push(AdmittedFile { path, rel, bytes: meta.len(), modified_ns: modified_ns(&meta) });
    }
    Ok(files)
}

/// Pairs of distinct names that a case-insensitive filesystem would merge (at most five reported).
pub(super) fn case_conflicts<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen: HashMap<String, &str> = HashMap::new();
    let mut conflicts = Vec::new();
    for name in names {
        if let Some(first) = seen.insert(name.to_lowercase(), name) {
            if conflicts.len() < 5 { conflicts.push(format!("{first} / {name}")); }
        }
    }
    conflicts
}

pub(crate) struct Copied { pub files: Vec<InventoryFile>, pub source_hash: String }

/// Aggregate content identity: every path with its content hash, then every directory.
pub(crate) fn source_hash(files: &[InventoryFile], directories: &[String]) -> String {
    let mut hash = Sha256::new();
    for file in files {
        hash.update((file.path.len() as u64).to_be_bytes()); hash.update(file.path.as_bytes()); hash.update(file.sha256.as_bytes());
    }
    hash.update(b"\0directories\0");
    for directory in directories { hash.update((directory.len() as u64).to_be_bytes()); hash.update(directory.as_bytes()); }
    format!("{:x}", hash.finalize())
}

/// Copies every admitted file into `source_dir` (created fresh by the caller), hashing the bytes actually
/// copied. A file that changes while being read fails the whole attempt: versions are never mixed.
pub(crate) fn copy_snapshot(admitted: &Admitted, source_dir: &Path, ctl: &Ctl, progress: &dyn Fn(u64)) -> Result<Copied> {
    for directory in &admitted.directories {
        fs::create_dir_all(source_dir.join(directory)).map_err(|e| format!("Cannot create snapshot directory {directory}: {e}"))?;
    }
    let mut files = Vec::with_capacity(admitted.files.len());
    let mut total = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    for (done, item) in admitted.files.iter().enumerate() {
        if ctl.cancelled() { return Err("Source index cancelled".into()); }
        progress(done as u64);
        files.push(copy_file(item, source_dir, ctl, &mut buffer, &mut total)?);
    }
    progress(admitted.files.len() as u64);
    let source_hash = source_hash(&files, &admitted.directories);
    Ok(Copied { files, source_hash })
}

fn copy_file(item: &AdmittedFile, source_dir: &Path, ctl: &Ctl, buffer: &mut [u8], total: &mut u64) -> Result<InventoryFile> {
    let mut input = open_plain(&item.path)?;
    let before = input.metadata().map_err(|e| format!("Cannot inspect {}: {e}", item.rel))?;
    *total = total.checked_add(before.len()).ok_or("Snapshot size overflow")?;
    if *total > MAX_BYTES { return Err(format!("Snapshot exceeds {MAX_BYTES} bytes; narrow the project exclusions.")); }
    let destination = source_dir.join(&item.rel);
    if let Some(parent) = destination.parent() { fs::create_dir_all(parent).map_err(|e| format!("Cannot create snapshot directory for {}: {e}", item.rel))?; }
    let mut output = OpenOptions::new().write(true).create_new(true).open(&destination).map_err(|e| format!("Cannot write snapshot copy of {}: {e}", item.rel))?;
    let (hash, copied, binary) = stream_copy(&mut input, &mut output, before.len(), ctl, buffer, &item.rel)?;
    output.sync_all().map_err(|e| format!("Cannot finish snapshot copy of {}: {e}", item.rel))?;
    let after = input.metadata().map_err(|e| format!("Cannot inspect {}: {e}", item.rel))?;
    if copied != before.len() || after.len() != before.len() || modified_ns(&after) != modified_ns(&before) {
        return Err(format!("Source file changed during indexing: {}; retry.", item.rel));
    }
    let (status, diagnostics) = if copied > MAX_PARSE_BYTES {
        ("skipped", vec![format!("Not parsed: larger than the {} MiB parser limit", MAX_PARSE_BYTES / 1024 / 1024)])
    } else if binary {
        ("skipped", vec!["Not parsed: binary content".to_string()])
    } else { ("pending", Vec::new()) };
    Ok(InventoryFile {
        id: file_id(&item.rel), path: item.rel.clone(), bytes: copied, sha256: format!("{:x}", hash.finalize()),
        modified_ns: modified_ns(&before), status: status.into(), diagnostics,
    })
}

/// Copies `input` to `output`, returning the content hash, the byte count and whether a NUL appeared in the first probe window.
fn stream_copy(input: &mut impl Read, output: &mut impl Write, expected: u64, ctl: &Ctl, buffer: &mut [u8], rel: &str) -> Result<(Sha256, u64, bool)> {
    let (mut hash, mut copied, mut binary, mut probed) = (Sha256::new(), 0u64, false, 0usize);
    loop {
        if ctl.cancelled() { return Err("Source index cancelled".into()); }
        let n = input.read(buffer).map_err(|e| format!("Cannot read {rel}: {e}"))?;
        if n == 0 { break; }
        if probed < BINARY_PROBE {
            let take = (BINARY_PROBE - probed).min(n);
            binary |= buffer[..take].contains(&0);
            probed += take;
        }
        copied += n as u64;
        if copied > expected { return Err(format!("Source file changed during indexing: {rel}; retry.")); }
        hash.update(&buffer[..n]);
        output.write_all(&buffer[..n]).map_err(|e| format!("Cannot write snapshot copy of {rel}: {e}"))?;
    }
    Ok((hash, copied, binary))
}

/// Metadata-only comparison against the saved inventory. `true` means something changed since the snapshot.
pub(crate) fn metadata_changed(root: &Path, record: &InventoryRecord, ctl: &Ctl) -> Result<bool> {
    let admitted = admit(root, ctl)?;
    if admitted.directories != record.directories || admitted.files.len() != record.files.len() { return Ok(true); }
    Ok(admitted.files.iter().zip(&record.files).any(|(now, saved)| now.rel != saved.path || now.bytes != saved.bytes || now.modified_ns != saved.modified_ns))
}
