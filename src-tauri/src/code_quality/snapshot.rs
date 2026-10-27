//! Source snapshots and untrusted engine tar streams. No source links are followed.
use super::runtime::Ctl;
use super::types::*;
use ignore::{gitignore::GitignoreBuilder, WalkBuilder};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use parking_lot::Mutex;

pub(crate) const MAX_FILES: u64 = 100_000;
pub(crate) const MAX_BYTES: u64 = 1024 * 1024 * 1024;
const EXCLUDED: &[&str] = &[".git", "node_modules", ".venv", "venv", "target", "dist", "build", "coverage", ".coverage", ".scannerwork", ".sonarqube", "megalinter-reports", "mega-linter-reports", "reports", "out", ".next", ".nuxt", "__pycache__", ".pytest_cache", ".mypy_cache"];

pub struct Snapshot {
    pub tar_path: PathBuf,
    pub hash: String,
    pub file_count: u64,
    pub total_bytes: u64,
    pub applied_exclusions: Vec<String>,
    pub warnings: Vec<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestEntry { path: String, bytes: u64, sha256: String }
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest { version: u32, hash: String, files: Vec<ManifestEntry> }

fn err(context: &str, e: impl std::fmt::Display) -> String { format!("{context}: {e}") }
pub(crate) fn is_link(meta: &Metadata) -> bool {
    if meta.file_type().is_symlink() { return true; }
    #[cfg(windows)]
    { use std::os::windows::fs::MetadataExt; return meta.file_attributes() & 0x400 != 0; }
    #[cfg(not(windows))]
    { false }
}
/// Rejects a link/reparse point at `path` itself. Callers that own a root check below it with `ensure_plain_below`.
pub(crate) fn ensure_plain_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if is_link(&m) => Err(format!("Refusing linked/reparse-point path: {}", path.display())),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(err("Cannot inspect path", e)),
    }
}
fn ensure_plain_below(root: &Path, path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        ensure_plain_path(ancestor)?;
        if ancestor == root { break; }
    }
    Ok(())
}
pub(crate) fn open_plain(path: &Path) -> Result<File> {
    ensure_plain_path(path)?;
    let mut options = OpenOptions::new(); options.read(true);
    #[cfg(windows)]
    { use std::os::windows::fs::OpenOptionsExt; options.custom_flags(0x00200000); }
    let file = options.open(path).map_err(|e| err(&format!("Cannot read {}", path.display()), e))?;
    let meta = file.metadata().map_err(|e| err("Cannot inspect opened file", e))?;
    if !meta.is_file() || is_link(&meta) { return Err(format!("Not a regular source file: {}", path.display())); }
    Ok(file)
}
fn safe_name(path: &Path) -> Result<String> {
    let raw = path.to_str().ok_or("Non-Unicode archive path is not supported")?;
    if raw.contains('\\') || raw.contains(':') || raw.chars().any(|c| c.is_control()) { return Err("Unsafe archive member name".into()); }
    if path.is_absolute() || path.components().any(|p| !matches!(p, Component::Normal(_) | Component::CurDir)) {
        return Err(format!("Unsafe archive member: {raw}"));
    }
    let parts: Vec<_> = path.components().filter_map(|p| if let Component::Normal(n) = p { n.to_str() } else { None }).collect();
    if parts.is_empty() { return Ok(String::new()); }
    // Windows device names/alternate streams cannot be represented as ordinary destination files.
    for part in &parts {
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if part.ends_with(['.', ' ']) || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && matches!(stem.as_bytes()[3], b'1'..=b'9')) {
            return Err(format!("Unsafe archive member: {raw}"));
        }
    }
    Ok(parts.join("/"))
}
pub(crate) fn relative_source_name(path: &Path) -> Result<String> {
    let text = path.to_str().ok_or("Non-Unicode source path is not supported")?;
    #[cfg(windows)]
    let text = text.replace('\\', "/");
    safe_name(Path::new(&text))
}
fn check_limits(files: u64, bytes: u64, max_files: u64, max_bytes: u64) -> Result<()> {
    if files > max_files { return Err(format!("Snapshot exceeds {max_files} files; narrow the project exclusions.")); }
    if bytes > max_bytes { return Err(format!("Snapshot exceeds {max_bytes} bytes; narrow the project exclusions.")); }
    Ok(())
}
struct HashReader<'a> { file: File, hash: Sha256, ctl: &'a Ctl }
impl Read for HashReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.ctl.cancelled() { return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "Scan cancelled")); }
        let n = self.file.read(buf)?; self.hash.update(&buf[..n]); Ok(n)
    }
}
fn header(size: u64, executable: bool) -> tar::Header {
    let mut h = tar::Header::new_gnu(); h.set_size(size); h.set_uid(1000); h.set_gid(1000); h.set_mtime(0); h.set_mode(if executable { 0o755 } else { 0o644 }); h.set_cksum(); h
}

/// Ignore-filtered, link-free enumeration of regular files below a canonical `root`, sorted. Shared by
/// snapshots and read-only repository inspection; it never writes anything. `max_depth` limits how deep
/// below `root` entries are visited (files directly in `root` have depth 1). `directories` lists every admitted
/// directory (empty ones included, `root` excluded), sorted; `special` lists sockets/devices that were skipped
/// when the caller asked to tolerate them (otherwise such an entry fails the listing).
pub(crate) struct SourceListing { pub paths: Vec<PathBuf>, pub directories: Vec<PathBuf>, pub special: Vec<PathBuf>, pub warnings: Vec<String> }
pub(crate) fn list_source_files(root: &Path, extra_exclusions: &[String], max_depth: Option<usize>, max_files: u64, ctl: &Ctl) -> Result<SourceListing> {
    list_source_tree(root, extra_exclusions, max_depth, max_files, false, ctl)
}
fn invalid_exclusion(pattern: &str) -> bool {
    pattern.starts_with(['!', '/']) || pattern.contains('\\') || pattern.split('/').any(|p| p == "..")
}
fn exclusion_matcher(root: &Path, patterns: &[String]) -> Result<ignore::gitignore::Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        if invalid_exclusion(pattern) { return Err(format!("Exclusions must be relative exclusion globs: {pattern}")); }
        builder.add_line(None, pattern).map_err(|e| err("Invalid project exclusion", e))?;
    }
    builder.build().map_err(|e| err("Invalid project exclusions", e))
}
fn admit_entry(entry: &ignore::DirEntry, extra: &ignore::gitignore::Gitignore, warnings: &Mutex<Vec<String>>) -> bool {
    if entry.depth() == 0 { return true; }
    if fs::symlink_metadata(entry.path()).is_ok_and(|meta| is_link(&meta)) {
        warnings.lock().push(format!("Skipped link/junction/reparse point: {}", entry.path().display()));
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
    !EXCLUDED.iter().any(|n| name.eq_ignore_ascii_case(n)) && !extra.matched_path_or_any_parents(entry.path(), is_dir).is_ignore()
}
/// Sorts one non-link entry into `listing`; sockets and devices are an error unless the caller tolerates them.
fn record_entry(listing: &mut SourceListing, path: PathBuf, meta: &Metadata, max_files: u64, tolerate_special: bool) -> Result<()> {
    if meta.is_file() {
        listing.paths.push(path);
        check_limits(listing.paths.len() as u64, 0, max_files, MAX_BYTES)
    } else if meta.is_dir() {
        listing.directories.push(path);
        if listing.directories.len() as u64 > max_files { return Err(format!("Snapshot exceeds {max_files} directories; narrow the project exclusions.")); }
        Ok(())
    } else if tolerate_special {
        listing.special.push(path);
        Ok(())
    } else {
        Err("Snapshot contains an unsupported non-regular filesystem entry".into())
    }
}
pub(crate) fn list_source_tree(root: &Path, extra_exclusions: &[String], max_depth: Option<usize>, max_files: u64, tolerate_special: bool, ctl: &Ctl) -> Result<SourceListing> {
    let extra = exclusion_matcher(root, extra_exclusions)?;
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let sink = warnings.clone();
    let mut walk = WalkBuilder::new(root);
    walk.hidden(false).follow_links(false).git_ignore(true).git_global(false).git_exclude(true).require_git(false).max_depth(max_depth);
    walk.filter_entry(move |entry| admit_entry(entry, &extra, &sink));
    let mut listing = SourceListing { paths: Vec::new(), directories: Vec::new(), special: Vec::new(), warnings: Vec::new() };
    for entry in walk.build() {
        if ctl.cancelled() { return Err("Scan cancelled while creating source snapshot".into()); }
        let entry = entry.map_err(|e| err("Cannot enumerate complete project snapshot", e))?;
        if entry.depth() == 0 { continue; }
        let meta = fs::symlink_metadata(entry.path()).map_err(|e| err("Cannot inspect source file", e))?;
        if is_link(&meta) { continue; }
        record_entry(&mut listing, entry.into_path(), &meta, max_files, tolerate_special)?;
    }
    listing.paths.sort();
    listing.directories.sort();
    listing.warnings = warnings.lock().clone();
    Ok(listing)
}

// Explicit coverage inputs may be gitignored, but never bypass protected metadata paths or project exclusions.
fn declared_report(root: &Path, name: &str) -> Result<Option<PathBuf>> {
    let relative = Path::new(name);
    if name.is_empty() || name.contains(['*', '?', '\\', ':']) || name.chars().any(|c| c.is_control()) { return Ok(None); }
    if relative.is_absolute() || relative.components().any(|c| !matches!(c, Component::Normal(_))) { return Ok(None); }
    if relative.components().any(|part| part.as_os_str().to_str().is_some_and(|name| [".git", ".omp", ".ssh"].iter().any(|protected| name.eq_ignore_ascii_case(protected)))) { return Ok(None); }
    if !matches!(relative.extension().and_then(|ext| ext.to_str()), Some("xml" | "lcov" | "info")) { return Ok(None); }
    let path = root.join(relative);
    ensure_plain_below(root, &path)?;
    Ok(fs::symlink_metadata(&path).ok().filter(Metadata::is_file).map(|_| path))
}
fn declared_reports(root: &Path, listed: &[PathBuf], exclusions: &[String]) -> Result<Vec<PathBuf>> {
    let Some(text) = super::scanners::read_config(&root.join("sonar-project.properties"))? else { return Ok(Vec::new()); };
    let extra = exclusion_matcher(root, exclusions)?;
    let mut found: Vec<PathBuf> = Vec::new();
    super::scanners::visit_properties(&text, |key, value| {
        if !matches!(key, "sonar.coverageReportPaths" | "sonar.javascript.lcov.reportPaths" | "sonar.python.coverage.reportPaths" | "sonar.rust.lcov.reportPaths" | "sonar.rust.cobertura.reportPaths") { return Ok(()); }
        for name in super::scanners::unescape_property(value)?.split(',') {
            let Some(path) = declared_report(root, name.trim())? else { continue; };
            if extra.matched_path_or_any_parents(&path, false).is_ignore() { continue; }
            if listed.binary_search(&path).is_err() && !found.contains(&path) { found.push(path); }
        }
        Ok(())
    })?;
    Ok(found)
}

pub fn create_snapshot(folder: &Path, extra_exclusions: &[String], job_dir: &Path, ctl: &Ctl) -> Result<Snapshot> {
    ensure_plain_path(folder)?;
    let root = fs::canonicalize(folder).map_err(|e| err("Cannot resolve project folder", e))?;
    if !root.is_dir() { return Err("Project folder is not a directory".into()); }
    fs::create_dir_all(job_dir).map_err(|e| err("Cannot create job directory", e))?;
    ensure_plain_path(job_dir)?;
    let canonical_job = fs::canonicalize(job_dir).map_err(|e| err("Cannot resolve job directory", e))?;
    if canonical_job.starts_with(&root) { return Err("Job storage must be outside the source folder".into()); }
    let SourceListing { mut paths, warnings, .. } = list_source_files(&root, extra_exclusions, None, MAX_FILES, ctl)?;
    paths.extend(declared_reports(&root, &paths, extra_exclusions)?);
    paths.sort();
    let tar_path = job_dir.join("source.tar");
    let manifest_path = job_dir.join("manifest.json");
    let mut tar = tar::Builder::new(File::create(&tar_path).map_err(|e| err("Cannot create source archive", e))?);
    let mut files = Vec::with_capacity(paths.len()); let mut total = 0u64; let mut aggregate = Sha256::new();
    for path in paths {
        let file = open_plain(&path)?;
        let meta = file.metadata().map_err(|e| err("Cannot inspect source file", e))?;
        total = total.checked_add(meta.len()).ok_or("Snapshot size overflow")?;
        check_limits(files.len() as u64 + 1, total, MAX_FILES, MAX_BYTES)?;
        let relative = relative_source_name(path.strip_prefix(&root).map_err(|_| "Source path escaped the selected folder")?)?;
        #[cfg(unix)]
        let executable = { use std::os::unix::fs::PermissionsExt; meta.permissions().mode() & 0o111 != 0 };
        #[cfg(not(unix))]
        let executable = relative.ends_with(".sh") || relative.ends_with("gradlew") || relative.ends_with("mvnw");
        let mut reader = HashReader { file, hash: Sha256::new(), ctl };
        tar.append_data(&mut header(meta.len(), executable), &relative, (&mut reader).take(meta.len())).map_err(|e| err("Cannot archive complete source file", e))?;
        let mut changed = [0u8; 1];
        if reader.read(&mut changed).map_err(|e| err("Cannot read source file", e))? != 0 || reader.file.metadata().map_err(|e| err("Cannot inspect source file", e))?.len() != meta.len() { return Err(format!("Source file changed during snapshot: {relative}; retry the scan.")); }
        let content_hash = format!("{:x}", reader.hash.finalize());
        aggregate.update((relative.len() as u64).to_be_bytes()); aggregate.update(relative.as_bytes()); aggregate.update(content_hash.as_bytes());
        files.push(ManifestEntry { path: relative, bytes: meta.len(), sha256: content_hash });
    }
    tar.finish().map_err(|e| err("Cannot finish source archive", e))?;
    let hash = format!("{:x}", aggregate.finalize());
    let file_count = files.len() as u64;
    crate::dependency_security::atomic_json(&manifest_path, &Manifest { version: 1, hash: hash.clone(), files }, 64 * 1024 * 1024)?;
    let mut applied_exclusions: Vec<String> = EXCLUDED.iter().map(|s| s.to_string()).collect(); applied_exclusions.extend_from_slice(extra_exclusions);
    Ok(Snapshot { tar_path, hash, file_count, total_bytes: total, applied_exclusions, warnings })
}

pub struct TarStats { pub files: u64, pub bytes: u64 }
fn count_member(kind: tar::EntryType, size: u64, stats: &mut TarStats, max_total_bytes: u64) -> Result<()> {
    if kind.is_file() {
        stats.files += 1;
        stats.bytes = stats.bytes.checked_add(size).ok_or("Archive size overflow")?;
        if stats.bytes > max_total_bytes { return Err(format!("Archive exceeds {max_total_bytes} bytes")); }
    } else if size != 0 {
        return Err("Archive directory has a nonzero payload".into());
    }
    Ok(())
}
fn walk_tar(tar_file: &Path, max_total_bytes: u64, mut accept: impl FnMut(&str, &mut tar::Entry<'_, File>) -> Result<()>) -> Result<TarStats> {
    let mut archive = tar::Archive::new(open_plain(tar_file)?);
    let mut stats = TarStats { files: 0, bytes: 0 }; let mut seen = HashSet::new();
    for entry in archive.entries().map_err(|e| err("Invalid archive", e))? {
        let mut entry = entry.map_err(|e| err("Invalid archive entry", e))?;
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() { return Err("Archive contains a link, device or unsupported member".into()); }
        let name = safe_name(&entry.path().map_err(|e| err("Invalid archive path", e))?)?;
        if name.is_empty() { if kind.is_dir() { continue; } return Err("Archive contains an empty file name".into()); }
        if !seen.insert(name.to_lowercase()) { return Err(format!("Archive contains duplicate/case-colliding member: {name}")); }
        count_member(kind, entry.size(), &mut stats, max_total_bytes)?;
        accept(&name, &mut entry)?;
        // Consume every payload to detect truncated streams even for validation-only calls.
        std::io::copy(&mut entry, &mut std::io::sink()).map_err(|e| err("Truncated archive", e))?;
    }
    Ok(stats)
}
pub fn validate_tar(tar_file: &Path, max_total_bytes: u64) -> Result<TarStats> { walk_tar(tar_file, max_total_bytes, |_, _| Ok(())) }
pub fn extract_tar_safely(tar_file: &Path, dest: &Path, max_total_bytes: u64) -> Result<Vec<PathBuf>> {
    validate_tar(tar_file, max_total_bytes)?;
    ensure_plain_path(dest)?;
    fs::create_dir_all(dest).map_err(|e| err("Cannot create extraction directory", e))?;
    let mut paths = Vec::new();
    walk_tar(tar_file, max_total_bytes, |name, entry| {
        let relative = PathBuf::from(name); let target = dest.join(&relative);
        ensure_plain_below(dest, &target)?;
        if entry.header().entry_type().is_dir() { fs::create_dir_all(&target).map_err(|e| err("Cannot create report directory", e))?; }
        else {
            if let Some(parent) = target.parent() { fs::create_dir_all(parent).map_err(|e| err("Cannot create report parent", e))?; }
            // Refuse existing files as well as symlinks: an archive must not overwrite host state.
            let mut file = OpenOptions::new().write(true).create_new(true).open(&target).map_err(|e| err("Cannot create archive output safely", e))?;
            let bytes = std::io::copy(entry, &mut file).map_err(|e| err("Cannot extract archive member", e))?;
            file.flush().map_err(|e| err("Cannot flush extracted file", e))?;
            if bytes != file.metadata().map_err(|e| err("Cannot inspect extracted file", e))?.len() { return Err("Incomplete archive output".into()); }
        }
        paths.push(relative); Ok(())
    })?;
    Ok(paths)
}
pub fn pack_dir_tar(dir: &Path, tar_file: &Path) -> Result<()> {
    ensure_plain_path(dir)?;
    let root = fs::canonicalize(dir).map_err(|e| err("Cannot resolve managed directory", e))?;
    let mut paths = Vec::new();
    fn collect(root: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
        for child in fs::read_dir(root).map_err(|e| err("Cannot read managed directory", e))? {
            let child = child.map_err(|e| err("Cannot enumerate managed directory", e))?; let path = child.path();
            let meta = fs::symlink_metadata(&path).map_err(|e| err("Cannot inspect managed file", e))?;
            if is_link(&meta) { return Err("Managed directory contains a link/reparse point".into()); }
            if meta.is_dir() { collect(&path, paths)?; } else if meta.is_file() { paths.push(path); } else { return Err("Managed directory contains a non-regular file".into()); }
        } Ok(())
    }
    collect(&root, &mut paths)?; paths.sort();
    ensure_plain_path(tar_file)?;
    let mut archive = tar::Builder::new(File::create(tar_file).map_err(|e| err("Cannot create managed archive", e))?);
    for path in paths {
        let name = relative_source_name(path.strip_prefix(&root).map_err(|_| "Managed path escaped root")?)?;
        let file = open_plain(&path)?; let size = file.metadata().map_err(|e| err("Cannot inspect managed file", e))?.len();
        archive.append_data(&mut header(size, name.ends_with(".sh")), name, file).map_err(|e| err("Cannot archive managed file", e))?;
    }
    archive.finish().map_err(|e| err("Cannot finish managed archive", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp { fn new() -> Self { let p = std::env::temp_dir().join(format!("omp snapshot ø {}", uuid::Uuid::new_v4())); fs::create_dir_all(&p).unwrap(); Self(p) } }
    impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    #[test]
    fn snapshot_honors_ignores_untracked_unicode_and_is_deterministic() {
        let t = Temp::new(); let source = t.0.join("source æ folder"); fs::create_dir_all(source.join("target")).unwrap();
        fs::write(source.join(".gitignore"), "ignored.txt\n").unwrap(); fs::write(source.join("ignored.txt"), "ignored").unwrap();
        fs::write(source.join("untracked ø.py"), "eval(user_input)\n").unwrap(); fs::write(source.join("target/generated"), "generated").unwrap();
        fs::write(source.join("extra.txt"), "exclude").unwrap();
        let first = create_snapshot(&source, &["extra.txt".into()], &t.0.join("job1"), &Ctl::silent()).unwrap();
        let second = create_snapshot(&source, &["extra.txt".into()], &t.0.join("job2"), &Ctl::silent()).unwrap();
        assert_eq!(first.hash, second.hash); assert_eq!(first.file_count, 2);
        let paths = extract_tar_safely(&first.tar_path, &t.0.join("extracted"), MAX_BYTES).unwrap();
        assert!(paths.contains(&PathBuf::from("untracked ø.py"))); assert!(!paths.contains(&PathBuf::from("ignored.txt")));
        fs::write(source.join("untracked ø.py"), "changed dirty source").unwrap();
        assert_ne!(first.hash, create_snapshot(&source, &[], &t.0.join("job3"), &Ctl::silent()).unwrap().hash);
    }
    #[test]
    fn declared_coverage_is_captured_without_other_ignored_data() {
        let t = Temp::new(); let source = t.0.join("source");
        fs::create_dir_all(source.join("coverage")).unwrap();
        fs::write(source.join(".gitignore"), "coverage/\n").unwrap();
        fs::write(source.join("sonar-project.properties"), "sonar.python.coverage.reportPaths=coverage/coverage.xml\nsonar.javascript.lcov.reportPaths=coverage/\\\n js.lcov\n").unwrap();
        fs::write(source.join("coverage/coverage.xml"), "<coverage/>").unwrap();
        fs::write(source.join("coverage/js.lcov"), "TN:\nSF:src/main.js\nDA:1,1\nend_of_record\n").unwrap();
        fs::write(source.join("coverage/private.txt"), "not a report").unwrap();
        let first = create_snapshot(&source, &[], &t.0.join("job1"), &Ctl::silent()).unwrap();
        let destination = t.0.join("extracted");
        let paths = extract_tar_safely(&first.tar_path, &destination, MAX_BYTES).unwrap();
        assert!(paths.contains(&PathBuf::from("coverage/coverage.xml")));
        assert!(paths.contains(&PathBuf::from("coverage/js.lcov")));
        assert!(!paths.contains(&PathBuf::from("coverage/private.txt")));
        assert_eq!(fs::read_to_string(destination.join("coverage/coverage.xml")).unwrap(), "<coverage/>");
        fs::write(source.join("coverage/coverage.xml"), "<coverage changed='true'/>").unwrap();
        assert_ne!(first.hash, create_snapshot(&source, &[], &t.0.join("job2"), &Ctl::silent()).unwrap().hash);
        let excluded = create_snapshot(&source, &["coverage/**".into()], &t.0.join("job3"), &Ctl::silent()).unwrap();
        assert_eq!(excluded.file_count, 2);
    }
    #[test]
    fn declared_reports_do_not_escape_or_capture_metadata() {
        let t = Temp::new(); let source = t.0.join("source");
        fs::create_dir_all(source.join(".git")).unwrap();
        fs::write(source.join(".git/token.xml"), "<private/>").unwrap();
        for name in ["../outside.xml", "/outside.xml", ".git/token.xml", ".omp/private.xml", "coverage/auth.json", "coverage/*.xml"] {
            assert!(declared_report(&source, name).unwrap().is_none(), "{name}");
        }
    }
    #[cfg(unix)]
    #[test]
    fn declared_reports_reject_linked_parent_directories() {
        let t = Temp::new(); let source = t.0.join("source"); let outside = t.0.join("outside");
        fs::create_dir(&source).unwrap(); fs::create_dir(&outside).unwrap();
        fs::write(outside.join("private.xml"), "<private/>").unwrap();
        std::os::unix::fs::symlink(&outside, source.join("coverage")).unwrap();
        assert!(declared_report(&source, "coverage/private.xml").unwrap_err().contains("linked"));
    }
    #[test]
    fn limits_and_archive_size_are_explicit() {
        assert!(check_limits(MAX_FILES + 1, 0, MAX_FILES, MAX_BYTES).is_err());
        assert!(check_limits(1, MAX_BYTES + 1, MAX_FILES, MAX_BYTES).is_err());
        let t = Temp::new(); let d = t.0.join("data"); fs::create_dir(&d).unwrap(); fs::write(d.join("file"), "1234").unwrap();
        pack_dir_tar(&d, &t.0.join("data.tar")).unwrap(); assert!(validate_tar(&t.0.join("data.tar"), 3).is_err());
        let sparse = d.join("oversized");
        File::create(&sparse).unwrap().set_len(MAX_BYTES + 1).unwrap();
        assert!(create_snapshot(&d, &[], &t.0.join("oversized-job"), &Ctl::silent()).err().unwrap().contains("exceeds"));
    }
    #[test]
    fn refuses_traversal_links_devices_and_destination_links() {
        for p in ["../outside", "/absolute", "a/../../outside", "C:/drive", "a\\escape", "NUL.txt"] { assert!(safe_name(Path::new(p)).is_err(), "{p}"); }
        let t = Temp::new(); let p = t.0.join("link.tar"); let mut b = tar::Builder::new(File::create(&p).unwrap());
        let mut h = header(0, false); h.set_entry_type(tar::EntryType::Symlink); h.set_link_name("outside").unwrap(); h.set_cksum(); b.append_data(&mut h, "link", std::io::empty()).unwrap(); b.finish().unwrap(); drop(b);
        assert!(validate_tar(&p, 100).is_err());
    }
    #[test]
    fn skips_real_filesystem_links() {
        let t = Temp::new(); let source = t.0.join("source"); let outside = t.0.join("outside"); fs::create_dir(&source).unwrap(); fs::create_dir(&outside).unwrap(); fs::write(outside.join("secret"), "never copy").unwrap();
        #[cfg(windows)]
        let created = { let mut cmd = std::process::Command::new("cmd.exe"); cmd.args(["/c", "mklink", "/J"]).arg(source.join("junction")).arg(&outside); crate::code_diff::no_console(&mut cmd); cmd.status().map(|s| s.success()).unwrap_or(false) };
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(&outside, source.join("junction")).is_ok();
        #[cfg(not(any(windows, unix)))]
        let created = false;
        if !created { eprintln!("Skipping real link fixture: platform/permissions do not permit creation"); return; }
        let snapshot = create_snapshot(&source, &[], &t.0.join("job"), &Ctl::silent()).unwrap(); assert_eq!(snapshot.file_count, 0); assert!(!snapshot.warnings.is_empty());
    }
}
