//! Private, full-depth evidence snapshots. Catalog records and the ordering index live on disk;
//! neither enumeration, source validation nor completion scans collect the job's catalog in memory.
use crate::code_quality::snapshot::{is_link, open_plain, relative_source_name};
use crate::knowledge::fsio::handle_identity;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, Metadata, OpenOptions, ReadDir};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

pub const TEXT_CHUNK_BYTES: usize = 64 * 1024;
const RECORD_BYTES: usize = 1024 * 1024;
const IMAGE_BYTES: u64 = 512 * 1024;
type Result<T, E = String> = std::result::Result<T, E>;

#[derive(Clone, Debug)]
pub struct InventoryRoot { pub path: PathBuf, pub kind: String, pub rationale: String }
#[derive(Clone, Debug)]
pub struct InventoryExclusion { pub path: PathBuf, pub reason: String, pub source: String }

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryItem {
    pub id: String,
    pub root: u64,
    pub path: String,
    pub kind: String,
    pub state: String,
    pub bytes: u64,
    pub sha256: Option<String>,
    pub rationale: Option<String>,
    pub exclusion_source: Option<String>,
    pub mime_type: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InventorySummary {
    pub total: u64, pub files: u64, pub areas: u64, pub blocked: u64,
    pub excluded: u64, pub bytes: u64, pub source_hash: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryPage {
    pub items: Vec<InventoryItem>, pub total: u64, pub offset: u64, pub next_offset: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceImage { pub mime_type: String, pub data: String }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceRead {
    pub evidence_id: String, pub kind: String, pub offset: u64, pub bytes: u64,
    pub total_bytes: u64, pub next_offset: Option<u64>, pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<EvidenceImage>,
}
// Mutable navigation is separate from immutable evidence records. The hash-keyed on-disk search
// tree gives deterministic source hashing without a whole-job sort or whole-job vector.
#[derive(Serialize, Deserialize)]
struct IndexNode { sequence: u64, left: Option<String>, right: Option<String> }

fn io(context: &str, error: impl std::fmt::Display) -> String { format!("storage_error: {context}: {error}") }
fn check_cancel(cancelled: &dyn Fn() -> bool) -> Result<()> {
    if cancelled() { Err("precondition_failed: Inventory cancelled".into()) } else { Ok(()) }
}
fn plain_chain(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) if is_link(&meta) => return Err("precondition_failed: Linked/reparse-point path refused".into()),
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(io("Cannot inspect path ancestry", e)),
        }
    }
    Ok(())
}
fn canonical(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() { return Err("invalid_request: Scope paths must be absolute".into()); }
    plain_chain(path)?;
    fs::canonicalize(path).map_err(|e| io("Cannot canonicalize scope root", e))
}
fn protected_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() || path.components().any(|p| matches!(p, Component::ParentDir)) {
        return Err("invalid_request: Protected/excluded paths must be absolute and normalized".into());
    }
    plain_chain(path)?;
    let mut existing = path;
    let mut tail = Vec::new();
    while !existing.exists() {
        tail.push(existing.file_name().ok_or("invalid_request: Invalid protected path")?.to_owned());
        existing = existing.parent().ok_or("invalid_request: Invalid protected path")?;
    }
    let mut resolved = fs::canonicalize(existing).map_err(|e| io("Cannot resolve protected path", e))?;
    for part in tail.into_iter().rev() { resolved.push(part); }
    Ok(resolved)
}
fn write_json(path: &Path, value: &impl Serialize, fresh: bool) -> Result<()> {
    plain_chain(path)?;
    let bytes = serde_json::to_vec(value).map_err(|e| io("Cannot encode inventory record", e))?;
    if bytes.len() > RECORD_BYTES { return Err("invalid_request: Inventory record exceeds 1 MiB".into()); }
    let mut options = OpenOptions::new();
    options.write(true);
    if fresh { options.create_new(true); } else { options.truncate(true); }
    let mut file = options.open(path).map_err(|e| io("Cannot write inventory record", e))?;
    file.write_all(&bytes).and_then(|_| file.sync_all()).map_err(|e| io("Cannot persist inventory record", e))
}
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    plain_chain(path)?;
    let file = open_plain(path)?;
    if handle_identity(&file)?.links != 1 { return Err("storage_error: Linked inventory record refused".into()); }
    let mut bytes = Vec::new();
    file.take(RECORD_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(|e| io("Cannot read inventory record", e))?;
    if bytes.len() > RECORD_BYTES { return Err("storage_error: Oversized inventory record".into()); }
    serde_json::from_slice(&bytes).map_err(|e| io("Invalid inventory record", e))
}
fn record_path(base: &Path, sequence: u64) -> PathBuf { base.join("records").join(format!("{sequence:020}.json")) }
fn valid_id(id: &str) -> Result<()> {
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err("invalid_request: Invalid evidence id".into());
    }
    Ok(())
}
fn node_path(base: &Path, id: &str) -> PathBuf { base.join("index").join(format!("{id}.json")) }
fn item_id(root: u64, relative: &str) -> String {
    let mut hash = Sha256::new(); hash.update(root.to_be_bytes()); hash.update(relative.as_bytes());
    format!("{:x}", hash.finalize())
}
fn insert(base: &Path, item: &InventoryItem, summary: &mut InventorySummary, first: &mut Option<String>) -> Result<()> {
    write_json(&record_path(base, summary.total), item, true)?;
    let mut cursor = first.clone();
    while let Some(id) = cursor {
        if id == item.id { return Err("storage_error: Duplicate evidence identity".into()); }
        let path = node_path(base, &id);
        let mut node: IndexNode = read_json(&path)?;
        let next = if item.id < id { &mut node.left } else { &mut node.right };
        if let Some(id) = next { cursor = Some(id.clone()); }
        else { *next = Some(item.id.clone()); write_json(&path, &node, false)?; break; }
    }
    write_json(&node_path(base, &item.id), &IndexNode { sequence: summary.total, left: None, right: None }, true)?;
    if first.is_none() { *first = Some(item.id.clone()); }
    summary.total = summary.total.checked_add(1).ok_or("storage_error: Inventory count overflow")?;
    if item.kind == "area" { summary.areas += 1; }
    else if item.state != "excluded" { summary.files += 1; }
    if item.state == "blocked" { summary.blocked += 1; }
    if item.state == "excluded" { summary.excluded += 1; }
    summary.bytes = summary.bytes.checked_add(item.bytes).ok_or("storage_error: Inventory byte count overflow")?;
    Ok(())
}
fn catalog_hash(base: &Path, first: Option<String>, cancelled: &dyn Fn() -> bool) -> Result<String> {
    let mut hash = Sha256::new(); hash.update(b"omp-srs-source-v1\0");
    let mut stack = Vec::new();
    let mut cursor = first;
    loop {
        check_cancel(cancelled)?;
        while let Some(id) = cursor {
            let node: IndexNode = read_json(&node_path(base, &id))?;
            cursor = node.left;
            stack.push((node.sequence, node.right));
        }
        let Some((sequence, right)) = stack.pop() else { break };
        let item: InventoryItem = read_json(&record_path(base, sequence))?;
        let bytes = serde_json::to_vec(&item).map_err(|e| io("Cannot hash inventory", e))?;
        hash.update((bytes.len() as u64).to_be_bytes()); hash.update(bytes);
        cursor = right;
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn policy(name: &str, directory: bool) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    if matches!(name.as_str(), ".git" | ".hg" | ".svn" | ".bzr") { return Some("Version-control internals are not product evidence"); }
    if directory && matches!(name.as_str(), "node_modules" | "vendor" | ".venv" | "venv" | "site-packages" | "bower_components" | "packages_cache") { return Some("Installed dependencies are excluded; dependency manifests remain evidence"); }
    if directory && matches!(name.as_str(), "target" | "dist" | "build" | "out" | "coverage" | ".next" | ".nuxt" | ".svelte-kit" | ".turbo" | ".cache" | "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache" | "bin" | "obj" | ".gradle" | ".idea" | ".vs") { return Some("Generated output or tool cache is excluded"); }
    if matches!(name.as_str(), ".env" | ".npmrc" | ".pypirc" | ".netrc" | "credentials" | "credentials.json" | "secrets.json" | "secrets.yaml" | "secrets.yml" | "id_rsa" | "id_dsa" | "id_ecdsa" | "id_ed25519")
        || (name.starts_with(".env.") && !matches!(name.as_str(), ".env.example" | ".env.sample" | ".env.template"))
        || name.ends_with(".pem") || name.ends_with(".key") || name.ends_with(".p12") || name.ends_with(".pfx") || name.ends_with(".keystore") {
        return Some("Secret-bearing credential material is never copied into evidence");
    }
    if matches!(name.as_str(), ".ssh" | ".aws" | ".azure" | ".gnupg" | ".omp" | ".pi" | ".obsidian") { return Some("Native application configuration or credential directory is excluded"); }
    if name.ends_with(".min.js") || name.ends_with(".min.css") || name.ends_with(".map") || name.ends_with(".generated.ts") || name.ends_with(".g.cs") {
        return Some("Generated/minified artifact is excluded; authored sources remain evidence");
    }
    None
}
fn modified(meta: &Metadata) -> Option<std::time::SystemTime> { meta.modified().ok() }
fn image_mime(probe: &[u8]) -> Option<&'static str> {
    if probe.starts_with(b"\x89PNG\r\n\x1a\n") { Some("image/png") }
    else if probe.starts_with(b"\xff\xd8\xff") { Some("image/jpeg") }
    else if probe.starts_with(b"GIF87a") || probe.starts_with(b"GIF89a") { Some("image/gif") }
    else if probe.len() >= 12 && &probe[..4] == b"RIFF" && &probe[8..12] == b"WEBP" { Some("image/webp") }
    else { None }
}
// Validates all bytes, including UTF-8 scalars crossing copy-buffer boundaries.
struct Utf8State { tail: Vec<u8>, valid: bool }
impl Utf8State {
    fn add(&mut self, bytes: &[u8]) {
        if !self.valid { return; }
        if bytes.contains(&0) { self.valid = false; return; }
        let mut start = 0;
        if !self.tail.is_empty() {
            while start < bytes.len() && self.tail.len() < 4 {
                self.tail.push(bytes[start]); start += 1;
                match std::str::from_utf8(&self.tail) {
                    Ok(_) => { self.tail.clear(); break; }
                    Err(e) if e.error_len().is_some() => { self.valid = false; return; }
                    Err(_) => (),
                }
            }
            if !self.tail.is_empty() { return; }
        }
        match std::str::from_utf8(&bytes[start..]) {
            Ok(_) => (),
            Err(e) if e.error_len().is_none() => self.tail.extend_from_slice(&bytes[start + e.valid_up_to()..]),
            Err(_) => self.valid = false,
        }
    }
}
/// Re-opens the source after copying and refuses any sign that it changed while being snapshotted.
fn verify_unchanged(source: &Path, input: &File, before: &Metadata, identity_id: Option<&str>, bytes: u64) -> Result<()> {
    let after = input.metadata().map_err(|e| io("Cannot inspect copied evidence", e))?;
    plain_chain(source)?;
    let current = open_plain(source)?;
    let current_identity = handle_identity(&current)?;
    let current_meta = current.metadata().map_err(|e| io("Cannot inspect source after snapshot", e))?;
    if bytes != before.len() || before.len() != after.len() || modified(before) != modified(&after)
        || identity_id != current_identity.id.as_deref() || current_identity.links != 1 || current_meta.len() != before.len()
        || modified(&current_meta) != modified(before) {
        return Err("Evidence changed during snapshot".into());
    }
    Ok(())
}
fn classify_content(item: &mut InventoryItem, bytes: u64, probe: &[u8], utf8: &Utf8State) {
    if let Some(mime) = image_mime(probe) {
        item.mime_type = Some(mime.into()); item.kind = "image".into();
        if bytes > IMAGE_BYTES { block(item, "Image exceeds the bounded native image transport; a human-supplied textual description is required"); }
    } else if utf8.valid && utf8.tail.is_empty() { item.kind = "text".into(); }
    else { item.kind = "unsupported".into(); block(item, "Binary or non-UTF-8 evidence requires a human-readable description"); }
}
fn copy_file(source: &Path, destination: &Path, item: &mut InventoryItem, cancelled: &dyn Fn() -> bool) -> Result<()> {
    plain_chain(source)?;
    let mut input = open_plain(source)?;
    let identity = handle_identity(&input)?;
    if identity.links != 1 { return Err("Hard-linked evidence is blocked and was not copied".into()); }
    let before = input.metadata().map_err(|e| io("Cannot inspect opened evidence", e))?;
    let mut output = OpenOptions::new().write(true).create_new(true).open(destination).map_err(|e| io("Cannot create evidence snapshot", e))?;
    let mut buffer = [0u8; TEXT_CHUNK_BYTES];
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut probe = Vec::new();
    let mut utf8 = Utf8State { tail: Vec::with_capacity(4), valid: true };
    loop {
        check_cancel(cancelled)?;
        let n = input.read(&mut buffer).map_err(|e| format!("Unreadable evidence: {e}"))?;
        if n == 0 { break; }
        bytes = bytes.checked_add(n as u64).ok_or("Evidence byte count overflow")?;
        if bytes > before.len() { return Err("Evidence changed during snapshot".into()); }
        if probe.len() < 16 { probe.extend_from_slice(&buffer[..n.min(16 - probe.len())]); }
        utf8.add(&buffer[..n]); hash.update(&buffer[..n]);
        output.write_all(&buffer[..n]).map_err(|e| io("Cannot copy evidence snapshot", e))?;
    }
    output.sync_all().map_err(|e| io("Cannot persist evidence snapshot", e))?;
    verify_unchanged(source, &input, &before, identity.id.as_deref(), bytes)?;
    item.bytes = bytes; item.sha256 = Some(format!("{:x}", hash.finalize()));
    classify_content(item, bytes, &probe, &utf8);
    Ok(())
}

fn block(item: &mut InventoryItem, rationale: &str) {
    item.state = "blocked".into();
    item.rationale = Some(rationale.into());
}
fn exclude(item: &mut InventoryItem, source: &str, reason: &str) {
    item.kind = "excluded".into(); item.state = "excluded".into();
    item.exclusion_source = Some(source.into()); item.rationale = Some(reason.into());
}
fn new_item(root: u64, relative: String) -> InventoryItem {
    InventoryItem { id: item_id(root, &relative), root, path: relative, kind: "unsupported".into(), state: "ready".into(),
        bytes: 0, sha256: None, rationale: None, exclusion_source: None, mime_type: None }
}
/// ReadDir cannot identify the failed child. A separate blocking record preserves the
/// already-immutable area record and prevents false coverage.
fn directory_error_item(parent: &Path, root: &Path, number: u64) -> Result<InventoryItem> {
    let relative = parent.strip_prefix(root).map_err(|_| "Source escaped root")?;
    let relative = if relative.as_os_str().is_empty() { ".".into() } else { relative_source_name(relative)? };
    let mut item = new_item(number, relative.clone());
    item.id = item_id(number, &format!("{relative}\0directory-error"));
    block(&mut item, "Evidence area enumeration failed; coverage is incomplete");
    Ok(item)
}
/// Do not expose absolute source paths or OS error strings in catalog DTOs.
fn snapshot_failure(error: &str) -> &'static str {
    if error.contains("changed during") { "Evidence changed during snapshot" }
    else if error.contains("Hard-linked") { "Hard-linked evidence is blocked and was not copied" }
    else { "Evidence is unreadable or unsafe and was not copied" }
}

struct Scan<'a> {
    base: &'a Path, protected: Vec<PathBuf>, exclusions: Vec<(PathBuf, String, String)>,
    cancelled: &'a dyn Fn() -> bool, summary: InventorySummary, first: Option<String>,
}
struct RootWalk<'a> { root: PathBuf, number: u64, selected: &'a InventoryRoot, stack: Vec<(PathBuf, ReadDir)> }

impl Scan<'_> {
    fn insert(&mut self, item: &InventoryItem) -> Result<()> { insert(self.base, item, &mut self.summary, &mut self.first) }

    fn scan_root(&mut self, number: u64, selected: &InventoryRoot) -> Result<()> {
        check_cancel(self.cancelled)?;
        let root = canonical(&selected.path)?;
        if !root.is_dir() { return Err("invalid_request: Evidence root is not a directory".into()); }
        let mut walk = RootWalk { root: root.clone(), number, selected, stack: Vec::new() };
        let mut pending = Some(root);
        loop {
            check_cancel(self.cancelled)?;
            let next = match pending.take() { Some(path) => Some(path), None => self.next_path(&mut walk)? };
            let Some(path) = next else { break };
            self.visit(&mut walk, &path)?;
        }
        Ok(())
    }

    fn next_path(&mut self, walk: &mut RootWalk) -> Result<Option<PathBuf>> {
        while let Some((parent, entries)) = walk.stack.last_mut() {
            match entries.next() {
                Some(Ok(entry)) => return Ok(Some(entry.path())),
                Some(Err(_)) => {
                    let item = directory_error_item(parent, &walk.root, walk.number)?;
                    self.insert(&item)?;
                    walk.stack.pop();
                }
                None => { walk.stack.pop(); }
            }
        }
        Ok(None)
    }

    fn visit(&mut self, walk: &mut RootWalk, path: &Path) -> Result<()> {
        let relative = path.strip_prefix(&walk.root).map_err(|_| "Source escaped root")?;
        let relative = if relative.as_os_str().is_empty() { ".".into() } else {
            // An unrepresentable name must block inventory, never disappear from coverage.
            relative_source_name(relative).map_err(|e| format!("precondition_failed: Unsafe evidence name: {e}"))?
        };
        let mut item = new_item(walk.number, relative);
        if plain_chain(path).is_err() {
            block(&mut item, "Linked, reparse-point or unreadable source ancestry is never followed");
            return self.insert(&item);
        }
        let Ok(metadata) = fs::symlink_metadata(path) else {
            block(&mut item, "Evidence metadata is unreadable");
            return self.insert(&item);
        };
        // Link safety takes precedence over exclusions, so links cannot be presented as read evidence.
        if is_link(&metadata) {
            block(&mut item, "Symlink, junction or reparse-point evidence is never followed");
            return self.insert(&item);
        }
        self.classify(walk, path, &metadata, &mut item)?;
        self.insert(&item)
    }

    fn classify(&self, walk: &mut RootWalk, path: &Path, metadata: &Metadata, item: &mut InventoryItem) -> Result<()> {
        if self.protected.iter().any(|p| path.starts_with(p)) {
            exclude(item, "app", "Private application configuration, job data or vault output is outside the evidence scope");
        } else if let Some((_, reason, source)) = self.exclusions.iter().find(|(p, _, _)| path.starts_with(p)) {
            exclude(item, source, reason);
        } else if let Some(reason) = policy(&path.file_name().unwrap_or_default().to_string_lossy(), metadata.is_dir()) {
            exclude(item, "app", reason);
        } else if metadata.is_dir() {
            Self::admit_directory(walk, path, item);
        } else if metadata.is_file() {
            self.snapshot(path, metadata.len(), item)?;
        } else {
            block(item, "Device, socket or other non-regular evidence is not readable");
        }
        Ok(())
    }

    fn admit_directory(walk: &mut RootWalk, path: &Path, item: &mut InventoryItem) {
        item.kind = "area".into();
        item.rationale = Some(if item.path == "." { format!("{}: {}", walk.selected.kind, walk.selected.rationale) } else { "Source evidence area".into() });
        match fs::read_dir(path) {
            Ok(entries) => walk.stack.push((path.to_path_buf(), entries)),
            Err(_) => block(item, "Evidence area is unreadable; coverage is incomplete"),
        }
    }

    fn snapshot(&self, path: &Path, length: u64, item: &mut InventoryItem) -> Result<()> {
        let destination = self.base.join("objects").join(&item.id);
        match copy_file(path, &destination, item, self.cancelled) {
            Ok(()) => Ok(()),
            Err(error) if error.starts_with("storage_error:") || (self.cancelled)() => Err(error),
            Err(error) => {
                if destination.exists() { fs::remove_file(&destination).map_err(|e| io("Cannot remove incomplete snapshot", e))?; }
                item.kind = "unsupported".into();
                block(item, snapshot_failure(&error));
                item.bytes = length; item.sha256 = None;
                Ok(())
            }
        }
    }
}

fn create_storage(job_dir: &Path) -> Result<(PathBuf, PathBuf)> {
    plain_chain(job_dir)?;
    fs::create_dir_all(job_dir).map_err(|e| io("Cannot create private job storage", e))?;
    let job_dir = canonical(job_dir)?;
    let base = job_dir.join("inventory");
    fs::create_dir(&base).map_err(|e| io("Inventory already exists or cannot be created", e))?;
    for directory in ["records", "index", "objects"] { fs::create_dir(base.join(directory)).map_err(|e| io("Cannot create inventory storage", e))?; }
    Ok((job_dir, base))
}

/// The caller owns root selection. No model-provided absolute path is accepted by read helpers.
/// `protected_paths` must include vault output and application configuration; job storage is always
/// protected automatically. Exclusions are literal paths, not globs or .gitignore interpretations.
pub fn inventory(job_dir: &Path, roots: &[InventoryRoot], exclusions: &[InventoryExclusion], protected_paths: &[PathBuf], cancelled: &dyn Fn() -> bool) -> Result<InventorySummary> {
    if !job_dir.is_absolute() || roots.is_empty() { return Err("invalid_request: An absolute private job path and at least one root are required".into()); }
    let (job_dir, base) = create_storage(job_dir)?;
    let mut protected = protected_paths.iter().map(|p| protected_path(p)).collect::<Result<Vec<_>>>()?;
    protected.push(job_dir);
    let exclusions = exclusions.iter().map(|e| Ok((protected_path(&e.path)?, e.reason.clone(), e.source.clone()))).collect::<Result<Vec<_>>>()?;
    let mut scan = Scan { base: &base, protected, exclusions, cancelled, summary: InventorySummary::default(), first: None };
    for (number, selected) in roots.iter().enumerate() { scan.scan_root(number as u64, selected)?; }
    let Scan { mut summary, first, .. } = scan;
    summary.source_hash = catalog_hash(&base, first.clone(), cancelled)?;
    write_json(&base.join("index-root.json"), &first, true)?;
    write_json(&base.join("summary.json"), &summary, true)?;
    Ok(summary)
}

pub fn read_item(job_dir: &Path, id: &str) -> Result<InventoryItem> {
    valid_id(id)?;
    let base = job_dir.join("inventory");
    let _: InventorySummary = read_json(&base.join("summary.json"))?;
    let node: IndexNode = read_json(&node_path(&base, id))?;
    let item: InventoryItem = read_json(&record_path(&base, node.sequence))?;
    if item.id != id { return Err("storage_error: Inventory identity mismatch".into()); }
    Ok(item)
}
pub fn page(job_dir: &Path, offset: u64, limit: usize) -> Result<InventoryPage> {
    if !(1..=200).contains(&limit) { return Err("invalid_request: Inventory page limit must be 1..200".into()); }
    let base = job_dir.join("inventory");
    let summary: InventorySummary = read_json(&base.join("summary.json"))?;
    if offset > summary.total { return Err("invalid_request: Inventory offset exceeds total".into()); }
    let mut items = Vec::new(); let mut encoded = 256usize;
    for sequence in offset..summary.total.min(offset.saturating_add(limit as u64)) {
        let item: InventoryItem = read_json(&record_path(&base, sequence))?;
        let size = serde_json::to_vec(&item).map_err(|e| io("Cannot encode inventory page", e))?.len();
        if encoded + size + 1 > RECORD_BYTES - 1024 {
            if items.is_empty() { return Err("storage_error: Inventory item cannot fit a bounded reply".into()); }
            break;
        }
        encoded += size + 1; items.push(item);
    }
    let end = offset + items.len() as u64;
    Ok(InventoryPage { items, total: summary.total, offset, next_offset: (end < summary.total).then_some(end) })
}
pub fn for_each(job_dir: &Path, callback: &mut dyn FnMut(&InventoryItem) -> Result<()>) -> Result<()> {
    let base = job_dir.join("inventory");
    let summary: InventorySummary = read_json(&base.join("summary.json"))?;
    for sequence in 0..summary.total { callback(&read_json(&record_path(&base, sequence))?)?; }
    Ok(())
}
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for part in bytes.chunks(3) {
        let a = part[0] as usize; let b = part.get(1).copied().unwrap_or(0) as usize; let c = part.get(2).copied().unwrap_or(0) as usize;
        output.push(TABLE[a >> 2] as char); output.push(TABLE[((a & 3) << 4) | (b >> 4)] as char);
        output.push(if part.len() > 1 { TABLE[((b & 15) << 2) | (c >> 6)] as char } else { '=' });
        output.push(if part.len() > 2 { TABLE[c & 63] as char } else { '=' });
    }
    output
}
/// Returns snapshot bytes only. The service persists its own receipt after this succeeds.
pub fn read_evidence(job_dir: &Path, id: &str, offset: u64) -> Result<EvidenceRead> {
    let item = read_item(job_dir, id)?;
    let sha256 = item.sha256.clone().unwrap_or_default();
    if item.state != "ready" || !matches!(item.kind.as_str(), "text" | "image") {
        return Ok(EvidenceRead { evidence_id: id.into(), kind: "unsupported".into(), offset: 0, bytes: 0,
            total_bytes: item.bytes, next_offset: None, sha256, text: None, image: None });
    }
    if offset > item.bytes { return Err("invalid_request: Evidence offset exceeds file length".into()); }
    let path = job_dir.join("inventory").join("objects").join(id);
    plain_chain(&path)?;
    let mut file = open_plain(&path)?;
    if handle_identity(&file)?.links != 1 || file.metadata().map_err(|e| io("Cannot inspect evidence snapshot", e))?.len() != item.bytes {
        return Err("storage_error: Evidence snapshot identity/length changed".into());
    }
    if item.kind == "image" {
        if offset != 0 { return Err("invalid_request: Images must be read at offset zero".into()); }
        if item.bytes > IMAGE_BYTES { return Err("storage_error: Oversized image incorrectly admitted".into()); }
        let mut bytes = Vec::with_capacity(item.bytes as usize);
        file.take(IMAGE_BYTES + 1).read_to_end(&mut bytes).map_err(|e| io("Cannot read image snapshot", e))?;
        if bytes.len() as u64 != item.bytes || format!("{:x}", Sha256::digest(&bytes)) != sha256 { return Err("storage_error: Image snapshot hash mismatch".into()); }
        return Ok(EvidenceRead { evidence_id: id.into(), kind: "image".into(), offset: 0, bytes: item.bytes,
            total_bytes: item.bytes, next_offset: None, sha256, text: None,
            image: Some(EvidenceImage { mime_type: item.mime_type.ok_or("storage_error: Missing image MIME type")?, data: base64(&bytes) }) });
    }
    file.seek(SeekFrom::Start(offset)).map_err(|e| io("Cannot seek evidence snapshot", e))?;
    let wanted = (item.bytes - offset).min(TEXT_CHUNK_BYTES as u64) as usize;
    let mut bytes = vec![0u8; wanted];
    file.read_exact(&mut bytes).map_err(|e| io("Cannot read text snapshot", e))?;
    let valid = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.len(),
        Err(e) if e.error_len().is_none() && offset + (bytes.len() as u64) < item.bytes => e.valid_up_to(),
        Err(_) => return Err("invalid_request: Evidence offset is not a UTF-8 character boundary or snapshot is corrupt".into()),
    };
    bytes.truncate(valid);
    let next = offset + bytes.len() as u64;
    let text = String::from_utf8(bytes).map_err(|_| "storage_error: Invalid UTF-8 snapshot")?;
    Ok(EvidenceRead { evidence_id: id.into(), kind: "text".into(), offset, bytes: text.len() as u64,
        total_bytes: item.bytes, next_offset: (next < item.bytes).then_some(next), sha256, text: Some(text), image: None })
}
/// Re-enumerates every area and hashes every admitted file, detecting additions/deletions, policy
/// changes, root substitutions and content changes. Temporary data remains in private job storage.
pub fn validate_sources(job_dir: &Path, roots: &[InventoryRoot], exclusions: &[InventoryExclusion], protected_paths: &[PathBuf], cancelled: &dyn Fn() -> bool) -> Result<()> {
    let saved: InventorySummary = read_json(&job_dir.join("inventory").join("summary.json"))?;
    let validation = job_dir.join(format!("source-validation-{}", uuid::Uuid::new_v4()));
    let mut protected = protected_paths.to_vec(); protected.push(canonical(job_dir)?);
    let result = inventory(&validation, roots, exclusions, &protected, cancelled).and_then(|now| {
        if now.source_hash == saved.source_hash { Ok(()) } else { Err("source_changed: Evidence sources or scope changed since the immutable snapshot".into()) }
    });
    // This subtree was created by this function, contains only owned files, and is never a vault path.
    if validation.exists() { fs::remove_dir_all(&validation).map_err(|e| io("Cannot remove source-validation snapshot", e))?; }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture { base: PathBuf, source: PathBuf, job: PathBuf }
    impl Fixture {
        fn new() -> Self {
            let base = std::env::temp_dir().join(format!("omp-srs-inventory-{}", uuid::Uuid::new_v4()));
            let source = base.join("source"); let job = base.join("private").join("job");
            fs::create_dir_all(&source).unwrap(); fs::create_dir_all(&job).unwrap();
            Self { base, source, job }
        }
        fn roots(&self) -> Vec<InventoryRoot> { vec![InventoryRoot { path: self.source.clone(), kind: "repository".into(), rationale: "Human-approved repository".into() }] }
        fn snapshot(&self) -> InventorySummary { inventory(&self.job, &self.roots(), &[], &[], &|| false).unwrap() }
        fn items(&self) -> Vec<InventoryItem> {
            let mut items = Vec::new(); for_each(&self.job, &mut |item| { items.push(item.clone()); Ok(()) }).unwrap(); items
        }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.base); } }
    #[test]
    fn streams_full_depth_and_does_not_blindly_apply_gitignore() {
        let fixture = Fixture::new();
        fs::write(fixture.source.join(".gitignore"), "important.txt\n").unwrap();
        fs::write(fixture.source.join("important.txt"), "evidence").unwrap();
        let mut directory = fixture.source.clone();
        for _ in 0..40 { directory.push("deep"); fs::create_dir(&directory).unwrap(); }
        fs::write(directory.join("leaf.txt"), "deep evidence").unwrap();
        fixture.snapshot();
        let items = fixture.items();
        assert!(items.iter().any(|i| i.path == "important.txt" && i.kind == "text"));
        assert!(items.iter().any(|i| i.path.ends_with("leaf.txt") && i.kind == "text"));
        let first = page(&fixture.job, 0, 2).unwrap(); assert_eq!(first.items.len(), 2); assert_eq!(first.next_offset, Some(2));
    }
    #[test]
    fn records_exclusion_rationales_binary_and_hardlinks() {
        let fixture = Fixture::new();
        fs::write(fixture.source.join(".env"), "SECRET=never-copy").unwrap();
        fs::create_dir(fixture.source.join("node_modules")).unwrap();
        fs::write(fixture.source.join("node_modules").join("hidden.txt"), "dependency").unwrap();
        fs::write(fixture.source.join("binary.bin"), [0, 255, 1]).unwrap();
        fs::write(fixture.source.join("original.txt"), "linked secret").unwrap();
        fs::hard_link(fixture.source.join("original.txt"), fixture.source.join("hardlink.txt")).unwrap();
        let summary = fixture.snapshot(); assert_eq!(summary.excluded, 2); assert_eq!(summary.blocked, 3);
        for item in fixture.items() {
            if item.path == ".env" { assert_eq!(item.exclusion_source.as_deref(), Some("app")); assert!(item.sha256.is_none()); }
            if item.path == "hardlink.txt" || item.path == "original.txt" { assert_eq!(item.state, "blocked"); assert!(!fixture.job.join("inventory/objects").join(item.id).exists()); }
        }
    }
    #[test]
    fn text_chunks_preserve_utf8_boundaries_and_empty_evidence() {
        let fixture = Fixture::new();
        let text = format!("{}€tail", "a".repeat(TEXT_CHUNK_BYTES - 1));
        fs::write(fixture.source.join("text.txt"), &text).unwrap();
        fs::write(fixture.source.join("empty.txt"), "").unwrap(); fixture.snapshot();
        let items = fixture.items(); let item = items.iter().find(|i| i.path == "text.txt").unwrap();
        let first = read_evidence(&fixture.job, &item.id, 0).unwrap();
        assert_eq!(first.bytes as usize, TEXT_CHUNK_BYTES - 1);
        let second = read_evidence(&fixture.job, &item.id, first.next_offset.unwrap()).unwrap();
        assert_eq!(format!("{}{}", first.text.unwrap(), second.text.unwrap()), text); assert_eq!(second.next_offset, None);
        assert!(read_evidence(&fixture.job, &item.id, TEXT_CHUNK_BYTES as u64).is_err());
        let empty = items.iter().find(|i| i.path == "empty.txt").unwrap(); assert_eq!(read_evidence(&fixture.job, &empty.id, 0).unwrap().text.as_deref(), Some(""));
    }
    #[test]
    fn source_hash_is_order_independent_and_resume_detects_add_delete_modify() {
        let fixture = Fixture::new();
        fs::write(fixture.source.join("one.txt"), "one").unwrap(); fixture.snapshot();
        validate_sources(&fixture.job, &fixture.roots(), &[], &[], &|| false).unwrap();
        fs::write(fixture.source.join("two.txt"), "two").unwrap();
        assert!(validate_sources(&fixture.job, &fixture.roots(), &[], &[], &|| false).unwrap_err().starts_with("source_changed:"));
        fs::remove_file(fixture.source.join("two.txt")).unwrap();
        fs::write(fixture.source.join("one.txt"), "ONE").unwrap();
        assert!(validate_sources(&fixture.job, &fixture.roots(), &[], &[], &|| false).is_err());
        fs::remove_file(fixture.source.join("one.txt")).unwrap();
        assert!(validate_sources(&fixture.job, &fixture.roots(), &[], &[], &|| false).is_err());
    }
    #[test]
    fn human_and_protected_exclusions_are_separate() {
        let fixture = Fixture::new();
        fs::write(fixture.source.join("human.txt"), "human excluded").unwrap();
        fs::create_dir(fixture.source.join("SRS")).unwrap();
        fs::write(fixture.source.join("SRS").join("draft.md"), "output").unwrap();
        inventory(&fixture.job, &fixture.roots(), &[InventoryExclusion { path: fixture.source.join("human.txt"), reason: "Not this product".into(), source: "human".into() }], &[fixture.source.join("SRS")], &|| false).unwrap();
        let items = fixture.items(); assert_eq!(items.iter().find(|i| i.path == "human.txt").unwrap().exclusion_source.as_deref(), Some("human"));
        assert_eq!(items.iter().find(|i| i.path == "SRS").unwrap().exclusion_source.as_deref(), Some("app"));
        assert!(!items.iter().any(|i| i.path.ends_with("draft.md")));
    }
    #[test]
    fn bounded_images_are_base64_and_large_images_remain_blocking() {
        let fixture = Fixture::new();
        let png = b"\x89PNG\r\n\x1a\n"; fs::write(fixture.source.join("tiny.png"), png).unwrap();
        let mut large = vec![0u8; IMAGE_BYTES as usize + 1]; large[..png.len()].copy_from_slice(png);
        fs::write(fixture.source.join("large.png"), large).unwrap(); fixture.snapshot();
        let items = fixture.items(); let tiny = items.iter().find(|i| i.path == "tiny.png").unwrap();
        assert_eq!(read_evidence(&fixture.job, &tiny.id, 0).unwrap().image.unwrap().data, "iVBORw0KGgo=");
        assert_eq!(items.iter().find(|i| i.path == "large.png").unwrap().state, "blocked");
        assert_eq!(base64(b"f"), "Zg=="); assert_eq!(base64(b"fo"), "Zm8="); assert_eq!(base64(b"foo"), "Zm9v");
    }
    #[test]
    fn disk_index_hash_is_independent_of_discovery_order() {
        let fixture = Fixture::new();
        let left = fixture.base.join("left"); let right = fixture.base.join("right");
        for base in [&left, &right] {
            fs::create_dir_all(base.join("records")).unwrap(); fs::create_dir(base.join("index")).unwrap();
        }
        let items: Vec<_> = ["z.txt", "a.txt", "middle.txt"].iter().map(|path| InventoryItem {
            id: item_id(0, path), root: 0, path: (*path).into(), kind: "text".into(),
            state: "ready".into(), bytes: 0, sha256: Some(format!("{:x}", Sha256::digest(b""))),
            rationale: None, exclusion_source: None, mime_type: None,
        }).collect();
        let mut summary = InventorySummary::default(); let mut first = None;
        for item in &items { insert(&left, item, &mut summary, &mut first).unwrap(); }
        let left_hash = catalog_hash(&left, first, &|| false).unwrap();
        let mut summary = InventorySummary::default(); let mut first = None;
        for item in items.iter().rev() { insert(&right, item, &mut summary, &mut first).unwrap(); }
        assert_eq!(left_hash, catalog_hash(&right, first, &|| false).unwrap());
    }
    #[test]
    fn complete_large_text_is_hashed_and_growth_during_copy_is_rejected() {
        use std::cell::Cell;
        let fixture = Fixture::new(); let content = "evidence\n".repeat(300_000);
        let source = fixture.source.join("large.txt"); fs::write(&source, &content).unwrap();
        fixture.snapshot();
        let item = fixture.items().into_iter().find(|i| i.path == "large.txt").unwrap();
        assert_eq!(item.bytes, content.len() as u64);
        assert_eq!(item.sha256.as_deref(), Some(format!("{:x}", Sha256::digest(content.as_bytes())).as_str()));
        let mut changed = item.clone(); let mutated = Cell::new(false);
        let result = copy_file(&source, &fixture.base.join("changed-copy"), &mut changed, &|| {
            if !mutated.replace(true) {
                OpenOptions::new().append(true).open(&source).unwrap().write_all(b"changed").unwrap();
            }
            false
        });
        assert!(result.unwrap_err().contains("changed during"));
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_are_blocked_and_roots_cannot_be_links() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new(); let outside = fixture.base.join("outside"); fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "not evidence").unwrap(); symlink(&outside, fixture.source.join("linked")).unwrap();
        fixture.snapshot(); let items = fixture.items(); assert_eq!(items.iter().find(|i| i.path == "linked").unwrap().state, "blocked");
        assert!(!items.iter().any(|i| i.path.contains("secret.txt")));
        assert!(canonical(&fixture.source.join("linked")).is_err());
    }
    #[test]
    fn cancelled_inventory_never_publishes_a_summary() {
        let fixture = Fixture::new(); assert!(inventory(&fixture.job, &fixture.roots(), &[], &[], &|| true).is_err());
        assert!(!fixture.job.join("inventory/summary.json").exists());
    }
}
