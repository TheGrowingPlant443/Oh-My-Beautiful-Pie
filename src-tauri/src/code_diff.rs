//! Git comparison, guarded saves and explicit Git actions for the in-app Code diff page.
//!
//! Reads only ask Git questions (no fetch, textconv or external diff). Mutations (stage, unstage, revert, commit,
//! push, PR) happen only on an explicit user action. A snapshot pins the baseline commit and the exact changed-file
//! list, so file reads, saves and actions can never name an arbitrary path.
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub type Result<T, E = String> = std::result::Result<T, E>;

/// Largest text side the editor loads or saves.
const MAX_BYTES: u64 = 2 * 1024 * 1024;
const CHANGED: &str = "diff_view_changed: Project changed; refresh Code diff";
const CONFLICT: &str = "diff_conflict: File changed on disk; reload before saving";
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum DiffMode {
    Head,
    Branch,
    Uncommitted,
    Commits,
}
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum DiffGroup {
    Staged,
    Changes,
}
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum FileAction {
    Stage,
    Unstage,
    Revert,
}
#[derive(Clone, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DiffFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: String,
    /// Only the Uncommitted view groups entries; head/branch entries have none.
    pub group: Option<DiffGroup>,
}
#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DiffOverview {
    pub snapshot_id: String,
    pub root: String,
    pub mode: DiffMode,
    pub base_ref: Option<String>,
    pub base_commit: Option<String>,
    pub base_label: String,
    /// The branch "branch" mode compares with, when the repository has one.
    pub branch_ref: Option<String>,
    pub current_branch: Option<String>,
    pub upstream: Option<String>,
    pub files: Vec<DiffFile>,
}
#[derive(Clone, Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiffContent {
    pub path: String,
    pub original: Option<String>,
    pub modified: Option<String>,
    pub working_hash: Option<String>,
    /// "lf" or "crlf": the working file's line endings; texts are always LF-normalised.
    pub eol: String,
    pub bom: bool,
    pub read_only_reason: Option<String>,
    pub original_label: String,
    pub modified_label: String,
}
#[derive(Clone, Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommitResult {
    pub commit: String,
    pub summary: String,
}
#[derive(Clone, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PublishOptions {
    pub new_branch: Option<String>,
    pub create_pr: bool,
}
#[derive(Clone, Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PublishResult {
    pub branch: String,
    pub remote: String,
    pub pr_url: Option<String>,
    pub steps: Vec<String>,
}

struct Snapshot {
    id: String,
    root: PathBuf,
    mode: DiffMode,
    base_commit: Option<String>,
    /// The baseline (HEAD or branch) label shown on the original side.
    base_label: String,
    target_commit: Option<String>,
    target_label: String,
    files: HashMap<String, DiffFile>,
}
pub struct DiffService {
    snapshots: Mutex<HashMap<String, Arc<Snapshot>>>,
    /// Last pull request created per instance (the only URL `open_pull_request` will open).
    prs: Mutex<HashMap<String, String>>,
    /// GitHub CLI program ("gh"; replaceable in tests).
    gh: std::ffi::OsString,
}
// File saves, index changes and history operations share the same in-process writer lock.
pub(crate) static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn file_key(group: Option<DiffGroup>, path: &str) -> String {
    let tag = match group {
        None => "",
        Some(DiffGroup::Staged) => "staged",
        Some(DiffGroup::Changes) => "changes",
    };
    format!("{tag}:{path}")
}

// ---------------------------------------------------------------- Git

const GIT_MISSING: &str = "Git is not installed or is not on PATH";
const GH_MISSING: &str = "GitHub CLI (gh) is not installed or is not on PATH; install it and run \"gh auth login\"";

fn git_command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(["--no-pager", "--no-optional-locks", "--literal-pathspecs", "-c", "core.quotepath=false", "-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    no_console(&mut cmd);
    cmd
}
pub(crate) fn no_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}
pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    git_command(dir, args).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            GIT_MISSING.to_string()
        } else {
            format!("Git could not run: {e}")
        }
    })
}
/// Runs `cmd` with optional stdin and a deadline; the child is killed when the deadline passes.
pub(crate) fn run_timed(mut cmd: Command, stdin: Option<&[u8]>, limit: Duration, not_found: &str) -> Result<std::process::Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound { not_found.to_string() } else { format!("Could not start the command: {e}") }
    })?;
    let mut out = child.stdout.take().ok_or("stdout unavailable")?;
    let mut err = child.stderr.take().ok_or("stderr unavailable")?;
    let out_thread = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = out.read_to_end(&mut bytes);
        bytes
    });
    let err_thread = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = err.read_to_end(&mut bytes);
        bytes
    });
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A child that exits early closes the pipe; its status explains why, so a write error is not fatal.
        let _ = pipe.write_all(input);
    }
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Timed out after {} s", limit.as_secs()));
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    Ok(std::process::Output { status, stdout: out_thread.join().unwrap_or_default(), stderr: err_thread.join().unwrap_or_default() })
}
pub(crate) fn failure_text(out: &std::process::Output) -> String {
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match (err.is_empty(), stdout.is_empty()) {
        (false, false) => format!("{err}\n{stdout}"),
        (false, true) => err,
        (true, false) => stdout,
        (true, true) => format!("Command failed ({})", out.status),
    }
}
/// Git with a deadline and optional stdin; success returns stdout.
pub(crate) fn git_timed(dir: &Path, args: &[&str], stdin: Option<&[u8]>, secs: u64) -> Result<Vec<u8>> {
    let out = run_timed(git_command(dir, args), stdin, Duration::from_secs(secs), GIT_MISSING)?;
    if out.status.success() { Ok(out.stdout) } else { Err(failure_text(&out)) }
}
/// Opens a credential-free https URL (or http with a literal loopback host when `allow_loopback_http`)
/// with the platform handler (no shell).
pub(crate) fn open_url(url: &str, allow_loopback_http: bool) -> Result<()> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("Refusing to open a link with whitespace or control characters".into());
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| "Refusing to open an invalid link".to_string())?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("Refusing to open a link that contains credentials".into());
    }
    let loopback = matches!(parsed.host_str(), Some("127.0.0.1") | Some("[::1]"));
    let allowed = match parsed.scheme() {
        "https" => true,
        "http" => allow_loopback_http && loopback,
        _ => false,
    };
    if !allowed {
        return Err("Refusing to open a non-https link".into());
    }
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = Command::new("rundll32.exe");
        cmd.arg("url.dll,FileProtocolHandler").arg(url);
        cmd
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut cmd = Command::new("open");
        cmd.arg(url);
        cmd
    };
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let mut cmd = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };
    no_console(&mut cmd);
    cmd.spawn().map(|_| ()).map_err(|e| format!("Could not open the link: {e}"))
}
pub(crate) fn git_ok(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = git(dir, args)?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let message = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if message.contains("not a git repository") {
        return Err("This project is not inside a Git repository".into());
    }
    Err(if message.is_empty() { format!("Git failed ({})", out.status) } else { message })
}
/// `Ok(None)` when Git answers "no such object" (exit 1); real failures stay errors.
pub(crate) fn git_optional(dir: &Path, args: &[&str]) -> Result<Option<Vec<u8>>> {
    let out = git(dir, args)?;
    if out.status.success() {
        return Ok(Some(out.stdout));
    }
    if out.status.code() == Some(1) {
        return Ok(None);
    }
    let message = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if message.is_empty() { format!("Git failed ({})", out.status) } else { message })
}
pub(crate) fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim().to_string()
}
pub(crate) fn repository_root(project: &Path) -> Result<PathBuf> {
    let top = text(git_ok(project, &["rev-parse", "--show-toplevel"])?);
    if top.is_empty() {
        return Err("This project is not inside a Git repository".into());
    }
    crate::setup::canonical(Path::new(&top)).map_err(|e| format!("Repository root is unreadable: {e}"))
}
pub(crate) fn resolve_commit(root: &Path, reference: &str) -> Result<Option<String>> {
    let spec = format!("{reference}^{{commit}}");
    Ok(git_optional(root, &["rev-parse", "--verify", "--quiet", "--end-of-options", &spec])?.map(text))
}
pub(crate) fn valid_commit(root: &Path, hash: &str) -> Result<()> {
    if !matches!(hash.len(), 40 | 64) || !hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) || resolve_commit(root, hash)?.is_none() {
        return Err(format!("Not a commit: {hash}"));
    }
    Ok(())
}
fn short_name(reference: &str) -> String {
    reference.strip_prefix("refs/heads/").or_else(|| reference.strip_prefix("refs/remotes/")).unwrap_or(reference).to_string()
}
/// master first, then the repository's recorded default branch, then main. Only local refs are read.
pub(crate) fn default_branch(root: &Path) -> Result<Option<(String, String)>> {
    let found = |reference: String| -> Result<Option<(String, String)>> {
        Ok(resolve_commit(root, &reference)?.map(|commit| (short_name(&reference), commit)))
    };
    for candidate in ["refs/heads/master", "refs/remotes/origin/master"] {
        if let Some(hit) = found(candidate.into())? {
            return Ok(Some(hit));
        }
    }
    let listed = text(git_ok(root, &["for-each-ref", "--format=%(refname)", "refs/remotes/"])?);
    let mut remotes: Vec<&str> = listed
        .lines()
        .filter_map(|line| line.strip_prefix("refs/remotes/")?.strip_suffix("/HEAD"))
        .collect();
    remotes.sort_by_key(|remote| (*remote != "origin", *remote));
    for remote in remotes {
        let symbolic = git_optional(root, &["symbolic-ref", "--quiet", &format!("refs/remotes/{remote}/HEAD")])?.map(text);
        let Some(target) = symbolic.and_then(|t| t.strip_prefix(&format!("refs/remotes/{remote}/")).map(str::to_string)) else { continue };
        for candidate in [format!("refs/heads/{target}"), format!("refs/remotes/{remote}/{target}")] {
            if let Some(hit) = found(candidate)? {
                return Ok(Some(hit));
            }
        }
    }
    if let Some(name) = git_optional(root, &["config", "--get", "init.defaultBranch"])?.map(text).filter(|n| !n.is_empty()) {
        if let Some(hit) = found(format!("refs/heads/{name}"))? {
            return Ok(Some(hit));
        }
    }
    for candidate in ["refs/heads/main", "refs/remotes/origin/main"] {
        if let Some(hit) = found(candidate.into())? {
            return Ok(Some(hit));
        }
    }
    Ok(None)
}
fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes.split(|b| *b == 0).filter(|part| !part.is_empty()).map(|part| String::from_utf8_lossy(part).into_owned()).collect()
}
/// `git diff --name-status -z` records: status, path (and a second path for renames/copies).
fn parse_name_status(bytes: &[u8]) -> Vec<DiffFile> {
    let tokens = split_nul(bytes);
    let mut files: BTreeMap<String, DiffFile> = BTreeMap::new();
    let mut tokens = tokens.into_iter();
    while let Some(status) = tokens.next() {
        let letter = status.chars().next().unwrap_or('M');
        let (old_path, path) = if matches!(letter, 'R' | 'C') {
            let (Some(old), Some(new)) = (tokens.next(), tokens.next()) else { break };
            (Some(old), new)
        } else {
            let Some(path) = tokens.next() else { break };
            (None, path)
        };
        let keep_unmerged = files.get(&path).is_some_and(|f| f.status == "U");
        if !keep_unmerged {
            files.insert(path.clone(), DiffFile { path, old_path, status: letter.to_string(), group: None });
        }
    }
    files.into_values().collect()
}
fn changed_files(root: &Path, base: Option<&str>) -> Result<Vec<DiffFile>> {
    let mut files: BTreeMap<String, DiffFile> = BTreeMap::new();
    match base {
        Some(commit) => {
            let out = git_ok(root, &["diff", "--no-ext-diff", "--no-textconv", "--name-status", "-z", "--find-renames", commit, "--"])?;
            for file in parse_name_status(&out) {
                files.insert(file.path.clone(), file);
            }
        }
        None => {
            // No commit yet: everything tracked is new, as long as it still exists on disk.
            for path in split_nul(&git_ok(root, &["ls-files", "--cached", "-z"])?) {
                if fs::symlink_metadata(root.join(&path)).is_ok() {
                    files.insert(path.clone(), DiffFile { path, old_path: None, status: "A".into(), group: None });
                }
            }
        }
    }
    for path in split_nul(&git_ok(root, &["ls-files", "--others", "--exclude-standard", "-z"])?) {
        // A trailing slash marks an untracked nested repository: not a file we can show.
        if !path.ends_with('/') {
            files.entry(path.clone()).or_insert(DiffFile { path, old_path: None, status: "?".into(), group: None });
        }
    }
    Ok(files.into_values().collect())
}

struct BaseBlob {
    mode: String,
    kind: String,
    oid: String,
}
fn base_blob(root: &Path, commit: &str, path: &str) -> Result<Option<BaseBlob>> {
    let out = git_ok(root, &["ls-tree", "-z", commit, "--", path])?;
    for entry in out.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let entry = String::from_utf8_lossy(entry);
        let Some((meta, name)) = entry.split_once('\t') else { continue };
        if name != path {
            continue;
        }
        let mut parts = meta.split(' ');
        if let (Some(mode), Some(kind), Some(oid)) = (parts.next(), parts.next(), parts.next()) {
            return Ok(Some(BaseBlob { mode: mode.into(), kind: kind.into(), oid: oid.into() }));
        }
    }
    Ok(None)
}
/// The stage-0 index entry for `path`, in the same shape as a tree entry.
fn index_blob(root: &Path, path: &str) -> Result<Option<BaseBlob>> {
    let out = git_ok(root, &["ls-files", "--stage", "-z", "--", path])?;
    for entry in out.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let entry = String::from_utf8_lossy(entry);
        let Some((meta, name)) = entry.split_once('\t') else { continue };
        if name != path {
            continue;
        }
        let mut parts = meta.split(' ');
        if let (Some(mode), Some(oid), Some("0")) = (parts.next(), parts.next(), parts.next()) {
            let kind = if mode == "160000" { "commit" } else { "blob" };
            return Ok(Some(BaseBlob { mode: mode.into(), kind: kind.into(), oid: oid.into() }));
        }
    }
    Ok(None)
}
/// One blob as editor text, or the reason it cannot be shown.
fn blob_side(root: &Path, blob: &BaseBlob) -> Result<(Option<String>, Option<&'static str>)> {
    if blob.mode == "160000" || blob.kind == "commit" {
        return Ok((None, Some("Submodules are shown read-only")));
    }
    if blob.mode == "120000" {
        return Ok((None, Some("Symbolic links are shown read-only")));
    }
    if blob.kind != "blob" {
        return Ok((None, Some("Not a regular file")));
    }
    let size: u64 = text(git_ok(root, &["cat-file", "-s", &blob.oid])?).parse().map_err(|_| "Git returned an invalid object size".to_string())?;
    if size > MAX_BYTES {
        return Ok((None, Some("Files larger than 2 MiB are shown read-only")));
    }
    Ok(match decode(&git_ok(root, &["cat-file", "blob", &blob.oid])?, false) {
        Ok(decoded) => (Some(decoded.text), None),
        Err(why) => (None, Some(why)),
    })
}
/// `git status --porcelain=v2 -z` records mapped to Staged and Changes entries (each group sorted by path).
fn parse_status_v2(bytes: &[u8]) -> Vec<DiffFile> {
    let (mut staged, mut changes): (Vec<DiffFile>, Vec<DiffFile>) = (Vec::new(), Vec::new());
    let mut tokens = split_nul(bytes).into_iter();
    while let Some(record) = tokens.next() {
        let kind = record.chars().next();
        match kind {
            Some('1') | Some('2') => {
                let rename = kind == Some('2');
                // A rename record is followed by its original path as a separate token.
                let original = if rename { tokens.next() } else { None };
                let fields: Vec<&str> = record.splitn(if rename { 10 } else { 9 }, ' ').collect();
                let xy: Vec<char> = fields.get(1).map(|f| f.chars().collect()).unwrap_or_default();
                if fields.len() < 9 || xy.len() != 2 {
                    continue;
                }
                let path = fields[fields.len() - 1].to_string();
                if xy[0] != '.' {
                    staged.push(DiffFile { path: path.clone(), old_path: original, status: xy[0].to_string(), group: Some(DiffGroup::Staged) });
                }
                if xy[1] != '.' {
                    changes.push(DiffFile { path, old_path: None, status: xy[1].to_string(), group: Some(DiffGroup::Changes) });
                }
            }
            Some('u') => {
                let fields: Vec<&str> = record.splitn(11, ' ').collect();
                if fields.len() == 11 {
                    changes.push(DiffFile { path: fields[10].to_string(), old_path: None, status: "U".into(), group: Some(DiffGroup::Changes) });
                }
            }
            Some('?') => {
                let path = record[2..].to_string();
                // A trailing slash marks an untracked nested repository: not a file we can show.
                if !path.ends_with('/') {
                    changes.push(DiffFile { path, old_path: None, status: "?".into(), group: Some(DiffGroup::Changes) });
                }
            }
            _ => {}
        }
    }
    staged.sort_by(|a, b| a.path.cmp(&b.path));
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    staged.extend(changes);
    staged
}
fn status_files(root: &Path) -> Result<Vec<DiffFile>> {
    Ok(parse_status_v2(&git_ok(root, &["status", "--porcelain=v2", "-z", "--untracked-files=all"])?))
}

// ------------------------------------------------------ paths & text

fn unsafe_path() -> String {
    "Unsafe file path".into()
}
fn relative_parts(path: &str) -> Result<Vec<&str>> {
    if path.is_empty() || path.starts_with('/') || path.contains(['\\', '\0']) {
        return Err(unsafe_path());
    }
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts {
        let lower = part.to_ascii_lowercase();
        let short_git = lower.strip_prefix("git~").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if part.is_empty() || *part == "." || *part == ".." || part.contains(':') || part.ends_with(['.', ' ']) || lower == ".git" || short_git {
            return Err(unsafe_path());
        }
    }
    Ok(parts)
}
fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}
enum Working {
    Missing,
    File(PathBuf, fs::Metadata),
    Blocked(&'static str),
}
/// Walks every existing component without following links, so a link or junction anywhere is refused.
fn inspect_working(root: &Path, path: &str) -> Result<Working> {
    let parts = relative_parts(path)?;
    let mut current = root.to_path_buf();
    for (index, part) in parts.iter().enumerate() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(meta) if is_link(&meta) => return Ok(Working::Blocked("Symbolic links and junctions are shown read-only")),
            Ok(meta) if index + 1 < parts.len() => {
                if !meta.is_dir() {
                    return Ok(Working::Missing);
                }
            }
            Ok(meta) => {
                return Ok(if meta.is_file() { Working::File(current, meta) } else { Working::Blocked("Not a regular file") });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Working::Missing),
            Err(e) => return Err(format!("{path}: {e}")),
        }
    }
    Ok(Working::Missing)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
struct Decoded {
    text: String,
    crlf: bool,
    bom: bool,
}
fn decode(bytes: &[u8], strict_eol: bool) -> std::result::Result<Decoded, &'static str> {
    let (body, bom) = match bytes.strip_prefix(&BOM) {
        Some(rest) => (rest, true),
        None => (bytes, false),
    };
    if body.contains(&0) {
        return Err("Binary files are shown read-only");
    }
    let text = std::str::from_utf8(body).map_err(|_| "Files that are not valid UTF-8 are shown read-only")?;
    let (mut crlf, mut lf, mut lone_cr) = (0usize, 0usize, 0usize);
    let raw = body;
    for (i, byte) in raw.iter().enumerate() {
        match byte {
            b'\n' if i > 0 && raw[i - 1] == b'\r' => crlf += 1,
            b'\n' => lf += 1,
            b'\r' if raw.get(i + 1) != Some(&b'\n') => lone_cr += 1,
            _ => {}
        }
    }
    if strict_eol && lone_cr > 0 {
        return Err("Files with lone carriage returns are shown read-only");
    }
    if strict_eol && crlf > 0 && lf > 0 {
        return Err("Files with mixed line endings are shown read-only");
    }
    let text = if crlf > 0 { text.replace("\r\n", "\n") } else { text.to_string() };
    Ok(Decoded { text, crlf: crlf > 0 && lf == 0, bom })
}
fn read_limited(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(bytes)
}

// ------------------------------------------------------------ service

impl Default for DiffService {
    fn default() -> Self {
        Self::with_gh("gh".into())
    }
}

pub(crate) fn current_branch(root: &Path) -> Result<Option<String>> {
    Ok(git_optional(root, &["symbolic-ref", "--short", "-q", "HEAD"])?.map(text).filter(|b| !b.is_empty()))
}

/// What a helper agent is told about the comparison the Code diff page shows.
#[derive(Debug)]
pub struct Comparison {
    pub base_commit: Option<String>,
    pub label: String,
    pub branch: Option<String>,
    /// Changes exactly when the change set does: the same request is not sent to a helper twice.
    pub fingerprint: String,
}

/// Unlike `DiffService::overview` this keeps no snapshot, so asking never invalidates the open page's own view.
pub fn comparison(project: &Path, mode: DiffMode) -> Result<Comparison> {
    let root = repository_root(project)?;
    let head = resolve_commit(&root, "HEAD")?;
    let (base_commit, label) = match mode {
        DiffMode::Head => (head, "Since the last commit (HEAD)".to_string()),
        DiffMode::Uncommitted => (head, "Uncommitted changes (staged and unstaged, against HEAD)".to_string()),
        DiffMode::Branch => {
            let (name, commit) = default_branch(&root)?.ok_or("No master or default branch was found in this repository")?;
            (Some(commit), format!("Compared with {name}"))
        }
        DiffMode::Commits => return Err("Use the commit comparison command for historical diffs".into()),
    };
    let patch = match &base_commit {
        Some(base) => git_timed(&root, &["diff", "--no-ext-diff", "--no-textconv", base, "--"], None, 60)?,
        None => git_timed(&root, &["diff", "--no-ext-diff", "--no-textconv", "--cached", "--"], None, 60)?,
    };
    let untracked = git_timed(&root, &["ls-files", "--others", "--exclude-standard", "-z"], None, 30)?;
    // Untracked files are not in the patch: their names, sizes and modification times stand in for their content.
    let mut seen = String::new();
    for name in untracked.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let name = String::from_utf8_lossy(name);
        let (len, modified) = fs::metadata(root.join(&*name)).map_or((0, 0), |m| (m.len(), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis())));
        seen.push_str(&format!("{name}:{len}:{modified}\n"));
    }
    let fingerprint = crate::terminal::digest(&format!("{mode:?}|{}|{}|{seen}", base_commit.as_deref().unwrap_or("-"), String::from_utf8_lossy(&patch)));
    let branch = current_branch(&root)?;
    Ok(Comparison { base_commit, label, branch, fingerprint })
}

impl DiffService {
    pub fn new() -> Self {
        Self::default()
    }
    /// `gh` is a parameter so tests can substitute a fake program.
    pub fn with_gh(gh: std::ffi::OsString) -> Self {
        Self { snapshots: Mutex::new(HashMap::new()), prs: Mutex::new(HashMap::new()), gh }
    }
    pub fn overview(&self, id: &str, project: &Path, mode: DiffMode) -> Result<DiffOverview> {
        let root = repository_root(project)?;
        let head = resolve_commit(&root, "HEAD")?;
        let branch = default_branch(&root)?;
        let head_label = head.as_ref().map_or("No commits yet".to_string(), |c| format!("Last commit {}", &c[..c.len().min(7)]));
        let head_ref = head.as_ref().map(|_| "HEAD".to_string());
        // (base ref, base commit, label shown for the whole comparison, label of the original side)
        let (base_ref, base_commit, base_label, side_label) = match mode {
            DiffMode::Head => (head_ref, head.clone(), head_label.clone(), head_label),
            DiffMode::Uncommitted => (head_ref, head.clone(), "Uncommitted changes".to_string(), head_label),
            DiffMode::Commits => return Err("Use the commit comparison command for historical diffs".into()),
            DiffMode::Branch => {
                let (name, commit) = branch.clone().ok_or("No master or default branch was found in this repository")?;
                let label = format!("Compared with {name}");
                (Some(name), Some(commit), label.clone(), label)
            }
        };
        let files = if mode == DiffMode::Uncommitted { status_files(&root)? } else { changed_files(&root, base_commit.as_deref())? };
        let current_branch = git_optional(&root, &["symbolic-ref", "--short", "-q", "HEAD"])?.map(text).filter(|b| !b.is_empty());
        let upstream = match &current_branch {
            Some(name) => Some(text(git_ok(&root, &["for-each-ref", "--format=%(upstream:short)", &format!("refs/heads/{name}")])?)).filter(|u| !u.is_empty()),
            None => None,
        };
        let snapshot = Arc::new(Snapshot {
            id: uuid::Uuid::new_v4().to_string(),
            root: root.clone(),
            mode,
            base_commit: base_commit.clone(),
            base_label: side_label,
            target_commit: None,
            target_label: "Working tree".into(),
            files: files.iter().map(|f| (file_key(f.group, &f.path), f.clone())).collect(),
        });
        let overview = DiffOverview {
            snapshot_id: snapshot.id.clone(),
            root: root.display().to_string(),
            mode,
            base_ref,
            base_commit,
            base_label,
            branch_ref: branch.map(|(name, _)| name),
            current_branch,
            upstream,
            files,
        };
        self.snapshots.lock().insert(id.into(), snapshot);
        Ok(overview)
    }
    pub fn commit_overview(&self, id: &str, project: &Path, base: Option<&str>, target: &str) -> Result<DiffOverview> {
        let root = repository_root(project)?;
        valid_commit(&root, target)?;
        let base = if let Some(base) = base {
            valid_commit(&root, base)?;
            Some(base.to_string())
        } else {
            resolve_commit(&root, &format!("{target}^1"))?
        };
        let left = match &base {
            Some(hash) => hash.clone(),
            None => text(git_timed(&root, &["hash-object", "-t", "tree", "--stdin"], Some(b""), 10)?),
        };
        let files = parse_name_status(&git_ok(&root, &["diff", "--no-ext-diff", "--no-textconv", "--name-status", "-z", "--find-renames", &left, target, "--"])?);
        let base_label = match &base {
            Some(hash) => text(git_ok(&root, &["log", "-1", "--format=%h %s", hash])?),
            None => "Empty tree".into(),
        };
        let target_label = text(git_ok(&root, &["log", "-1", "--format=%h %s", target])?);
        let snapshot = Arc::new(Snapshot {
            id: uuid::Uuid::new_v4().to_string(), root: root.clone(), mode: DiffMode::Commits,
            base_commit: base.clone(), base_label: base_label.clone(),
            target_commit: Some(target.into()), target_label,
            files: files.iter().map(|f| (file_key(None, &f.path), f.clone())).collect(),
        });
        let current_branch = git_optional(&root, &["symbolic-ref", "--short", "-q", "HEAD"])?.map(text);
        let upstream = match &current_branch {
            Some(name) => Some(text(git_ok(&root, &["for-each-ref", "--format=%(upstream:short)", &format!("refs/heads/{name}")])?)).filter(|u| !u.is_empty()),
            None => None,
        };
        let overview = DiffOverview {
            snapshot_id: snapshot.id.clone(), root: root.display().to_string(), mode: DiffMode::Commits,
            base_ref: base.clone(), base_commit: base, base_label, branch_ref: None, current_branch, upstream, files,
        };
        self.snapshots.lock().insert(id.into(), snapshot);
        Ok(overview)
    }
    fn snapshot(&self, id: &str, project: &Path, snapshot_id: &str) -> Result<Arc<Snapshot>> {
        let snapshot = self.snapshots.lock().get(id).cloned().filter(|s| s.id == snapshot_id).ok_or(CHANGED)?;
        if repository_root(project).map_err(|_| CHANGED.to_string())? != snapshot.root {
            return Err(CHANGED.into());
        }
        Ok(snapshot)
    }
    /// A snapshot usable for Git actions: only the Uncommitted view mutates anything.
    fn action_snapshot(&self, id: &str, project: &Path, snapshot_id: &str) -> Result<Arc<Snapshot>> {
        let snapshot = self.snapshot(id, project, snapshot_id)?;
        if snapshot.mode != DiffMode::Uncommitted {
            return Err("Git actions are only available in Uncommitted changes".into());
        }
        Ok(snapshot)
    }
    pub fn read_file(&self, id: &str, project: &Path, snapshot_id: &str, path: &str, group: Option<DiffGroup>) -> Result<DiffContent> {
        let snapshot = self.snapshot(id, project, snapshot_id)?;
        let file = entry(&snapshot, group, path)?;
        Ok(load(&snapshot, file)?.content)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn save_file(
        &self,
        id: &str,
        project: &Path,
        snapshot_id: &str,
        path: &str,
        group: Option<DiffGroup>,
        expected_hash: Option<&str>,
        text: &str,
    ) -> Result<DiffContent> {
        let snapshot = self.snapshot(id, project, snapshot_id)?;
        let file = entry(&snapshot, group, path)?;
        let _serial = WRITE_LOCK.lock();
        let loaded = load(&snapshot, file)?;
        if let Some(reason) = &loaded.content.read_only_reason {
            return Err(reason.clone());
        }
        if loaded.content.working_hash.as_deref() != expected_hash {
            return Err(CONFLICT.into());
        }
        let normalized = text.replace("\r\n", "\n");
        if normalized.contains('\r') {
            return Err("Text contains unsupported line endings".into());
        }
        let mut bytes = Vec::with_capacity(normalized.len() + 3);
        if loaded.content.bom {
            bytes.extend_from_slice(&BOM);
        }
        if loaded.content.eol == "crlf" {
            bytes.extend_from_slice(normalized.replace('\n', "\r\n").as_bytes());
        } else {
            bytes.extend_from_slice(normalized.as_bytes());
        }
        if bytes.len() as u64 > MAX_BYTES {
            return Err("The file is larger than 2 MiB and cannot be saved here".into());
        }
        write_guarded(&snapshot.root, path, &bytes, expected_hash)?;
        Ok(load(&snapshot, file)?.content)
    }
    pub fn change_file(&self, id: &str, project: &Path, snapshot_id: &str, path: &str, group: Option<DiffGroup>, action: FileAction) -> Result<()> {
        let snapshot = self.action_snapshot(id, project, snapshot_id)?;
        let file = entry(&snapshot, group, path)?;
        let _serial = WRITE_LOCK.lock();
        let result = apply_file_action(&snapshot, file, action);
        // The index or working tree changed, so every entry of this snapshot may be stale.
        self.snapshots.lock().remove(id);
        result
    }
    pub fn commit(&self, id: &str, project: &Path, snapshot_id: &str, message: &str) -> Result<CommitResult> {
        let snapshot = self.action_snapshot(id, project, snapshot_id)?;
        let message = message.trim();
        if message.is_empty() {
            return Err("Enter a commit message".into());
        }
        if message.len() > 100_000 {
            return Err("Commit message is too long".into());
        }
        let _serial = WRITE_LOCK.lock();
        let root = &snapshot.root;
        let result = (|| {
            let staged = git(root, &["diff", "--cached", "--quiet"])?;
            match staged.status.code() {
                Some(0) => return Err("Nothing is staged to commit".to_string()),
                Some(1) => {}
                _ => return Err(failure_text(&staged)),
            }
            git_timed(root, &["commit", "--quiet", "-F", "-"], Some(format!("{message}\n").as_bytes()), 300)?;
            let commit = text(git_ok(root, &["rev-parse", "--short", "HEAD"])?);
            Ok(CommitResult { commit, summary: message.lines().next().unwrap_or_default().to_string() })
        })();
        self.snapshots.lock().remove(id);
        result
    }
    pub fn publish(&self, id: &str, project: &Path, snapshot_id: &str, options: &PublishOptions) -> Result<PublishResult> {
        let snapshot = self.action_snapshot(id, project, snapshot_id)?;
        let _serial = WRITE_LOCK.lock();
        let result = self.publish_steps(id, &snapshot.root, options);
        // The branch may have changed even when a later step failed.
        self.snapshots.lock().remove(id);
        result
    }
    fn publish_steps(&self, id: &str, root: &Path, options: &PublishOptions) -> Result<PublishResult> {
        let mut steps: Vec<String> = Vec::new();
        // Completed steps stay visible when a later one fails, so the user knows what already happened.
        let fail = |steps: &[String], message: String| if steps.is_empty() { message } else { format!("{}. Then: {message}", steps.join("; ")) };
        let current = git_optional(root, &["symbolic-ref", "--short", "-q", "HEAD"])?.map(text).filter(|b| !b.is_empty());
        let (branch, is_new) = match options.new_branch.as_deref().map(str::trim) {
            Some("") => return Err("Enter a name for the new branch".into()),
            Some(name) => {
                validate_new_branch(root, name)?;
                git_timed(root, &["checkout", "-b", name], None, 60)?;
                steps.push(format!("Created branch {name}"));
                (name.to_string(), true)
            }
            None => (current.ok_or("HEAD is detached; enter a new branch name to publish")?, false),
        };
        let remote = publish_remote(root, &branch, is_new).map_err(|e| fail(&steps, e))?;
        let has_upstream = !is_new
            && !text(git_ok(root, &["for-each-ref", "--format=%(upstream:short)", &format!("refs/heads/{branch}")]).map_err(|e| fail(&steps, e))?).is_empty();
        let push: Vec<&str> = if has_upstream {
            vec!["-c", "push.default=upstream", "push"]
        } else {
            vec!["push", "--set-upstream", &remote, &branch]
        };
        git_timed(root, &push, None, 180).map_err(|e| fail(&steps, e))?;
        steps.push(format!("Pushed {branch} to {remote}"));
        let mut pr_url = None;
        if options.create_pr {
            let url = create_pull_request(&self.gh, root, &branch).map_err(|e| fail(&steps, e))?;
            self.prs.lock().insert(id.into(), url.clone());
            steps.push(format!("Created pull request {url}"));
            pr_url = Some(url);
        }
        Ok(PublishResult { branch, remote, pr_url, steps })
    }
    pub fn open_pull_request(&self, id: &str) -> Result<()> {
        let url = self.prs.lock().get(id).cloned().ok_or("No pull request was created in this session")?;
        open_url(&url, false)
    }
}

fn entry<'a>(snapshot: &'a Snapshot, group: Option<DiffGroup>, path: &str) -> Result<&'a DiffFile> {
    snapshot.files.get(&file_key(group, path)).ok_or_else(|| "That file is not part of this comparison".to_string())
}
pub(crate) fn validate_new_branch(root: &Path, name: &str) -> Result<()> {
    let valid = !name.starts_with('-')
        && !name.chars().any(char::is_whitespace)
        && git(root, &["check-ref-format", "--branch", name])?.status.success();
    if !valid {
        return Err(format!("Invalid branch name {name}"));
    }
    if git_optional(root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{name}")])?.is_some() {
        return Err(format!("Branch {name} already exists"));
    }
    Ok(())
}
fn publish_remote(root: &Path, branch: &str, is_new: bool) -> Result<String> {
    if !is_new {
        let configured = git_optional(root, &["config", "--get", &format!("branch.{branch}.remote")])?.map(text);
        if let Some(remote) = configured.filter(|r| !r.is_empty() && r != ".") {
            return Ok(remote);
        }
    }
    let listed = text(git_ok(root, &["remote"])?);
    let remotes: Vec<&str> = listed.lines().map(str::trim).filter(|r| !r.is_empty()).collect();
    match remotes.as_slice() {
        [] => Err("No Git remote is configured".into()),
        list if list.contains(&"origin") => Ok("origin".into()),
        [only] => Ok((*only).into()),
        _ => Err("Several remotes exist and none is named origin; set the branch upstream with Git first".into()),
    }
}
/// `gh pr create --fill`; the pull request URL is the last https line gh prints.
fn create_pull_request(program: &OsStr, root: &Path, branch: &str) -> Result<String> {
    let mut cmd = Command::new(program);
    cmd.current_dir(root)
        .args(["pr", "create", "--fill", "--head", branch])
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1");
    no_console(&mut cmd);
    let out = run_timed(cmd, None, Duration::from_secs(120), GH_MISSING)?;
    if !out.status.success() {
        return Err(failure_text(&out));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with("https://"))
        .map(str::to_string)
        .ok_or_else(|| format!("gh did not print a pull request URL: {}", stdout.trim()))
}
fn apply_file_action(snapshot: &Snapshot, file: &DiffFile, action: FileAction) -> Result<()> {
    let root = &snapshot.root;
    relative_parts(&file.path)?;
    if let Some(old) = &file.old_path {
        relative_parts(old)?;
    }
    match (action, file.group) {
        (FileAction::Stage, Some(DiffGroup::Changes)) => {
            git_timed(root, &["add", "--", &file.path], None, 60)?;
        }
        (FileAction::Unstage, Some(DiffGroup::Staged)) => {
            let mut args: Vec<&str> = if snapshot.base_commit.is_some() { vec!["restore", "--staged", "--"] } else { vec!["rm", "--cached", "-f", "-q", "--"] };
            args.push(&file.path);
            if let Some(old) = &file.old_path {
                args.push(old);
            }
            git_timed(root, &args, None, 60)?;
        }
        (FileAction::Revert, Some(DiffGroup::Changes)) => match file.status.as_str() {
            "M" | "D" | "T" => {
                git_timed(root, &["restore", "--worktree", "--", &file.path], None, 60)?;
            }
            "?" => match inspect_working(root, &file.path)? {
                Working::File(path, _) => fs::remove_file(&path).map_err(|e| format!("Could not delete {}: {e}", file.path))?,
                Working::Missing => {}
                Working::Blocked(why) => return Err(why.into()),
            },
            "U" => return Err("Resolve merge conflicts with Git first".into()),
            "A" => return Err("Intent-to-add files can only be unstaged with Git".into()),
            other => return Err(format!("Cannot revert a file with status {other}")),
        },
        _ => return Err("That action does not apply to this entry".into()),
    }
    Ok(())
}

struct Loaded {
    content: DiffContent,
}
/// Appends a blob's text to a side, recording the reason when it cannot be shown.
fn side_text(root: &Path, blob: &BaseBlob, note: &mut dyn FnMut(&str)) -> Result<Option<String>> {
    let (text, why) = blob_side(root, blob)?;
    if let Some(why) = why {
        note(why);
    }
    Ok(text)
}
const STAGED_READ_ONLY: &str = "Staged changes are read-only here; edit the file under Changes";
fn load(snapshot: &Snapshot, file: &DiffFile) -> Result<Loaded> {
    let mut reason: Option<String> = None;
    let mut note = |text: &str| {
        reason.get_or_insert_with(|| text.to_string());
    };
    relative_parts(&file.path)?;
    if file.status == "T" {
        note("File type changes are shown read-only");
    }
    let root = &snapshot.root;
    let commits = snapshot.mode == DiffMode::Commits;
    let staged = snapshot.mode == DiffMode::Uncommitted && file.group == Some(DiffGroup::Staged);
    let in_changes = snapshot.mode == DiffMode::Uncommitted && file.group == Some(DiffGroup::Changes);
    let (original_label, modified_label) = if commits {
        (snapshot.base_label.clone(), snapshot.target_label.clone())
    } else if in_changes {
        ("Staged (index)".to_string(), "Working tree".to_string())
    } else if staged {
        (snapshot.base_label.clone(), "Staged (index)".to_string())
    } else {
        (snapshot.base_label.clone(), "Working tree".to_string())
    };
    // Original side. Head/branch/staged compare against a commit; the Changes group against the index.
    let from_commit = match (in_changes, file.status.as_str()) {
        (true, "U") => snapshot.base_commit.is_some(),
        (true, _) => false,
        (false, "A" | "?") => false,
        (false, _) => snapshot.base_commit.is_some(),
    };
    let mut original = Some(String::new());
    if from_commit {
        let commit = snapshot.base_commit.as_deref().unwrap_or_default();
        let base_path = file.old_path.as_deref().unwrap_or(&file.path);
        relative_parts(base_path)?;
        match base_blob(root, commit, base_path)? {
            None if in_changes => {}
            None => return Err(format!("{base_path} is not in the baseline; refresh Code diff")),
            Some(blob) => original = side_text(root, &blob, &mut note)?,
        }
    } else if in_changes && file.status != "?" && file.status != "U" {
        if let Some(blob) = index_blob(root, &file.path)? {
            original = side_text(root, &blob, &mut note)?;
        }
    }
    let mut modified = Some(String::new());
    let (mut working_hash, mut crlf, mut bom) = (None, false, false);
    if commits {
        if file.status != "D" {
            let target = snapshot.target_commit.as_deref().ok_or("Missing target commit")?;
            let blob = base_blob(root, target, &file.path)?.ok_or_else(|| format!("{} is not in the commit; refresh", file.path))?;
            modified = side_text(root, &blob, &mut note)?;
        }
    } else if staged {
        if file.status != "D" {
            match index_blob(root, &file.path)? {
                None => return Err(format!("{} is not in the index; refresh Code diff", file.path)),
                Some(blob) => modified = side_text(root, &blob, &mut note)?,
            }
        }
    } else {
        match inspect_working(root, &file.path)? {
            Working::Missing => {}
            Working::Blocked(why) => {
                note(why);
                modified = None;
            }
            Working::File(path, meta) => {
                if meta.len() > MAX_BYTES {
                    note("Files larger than 2 MiB are shown read-only");
                    modified = None;
                } else {
                    let bytes = read_limited(&path)?;
                    if bytes.len() as u64 > MAX_BYTES {
                        note("Files larger than 2 MiB are shown read-only");
                        modified = None;
                    } else {
                        working_hash = Some(hash(&bytes));
                        match decode(&bytes, true) {
                            Ok(decoded) => {
                                (crlf, bom) = (decoded.crlf, decoded.bom);
                                modified = Some(decoded.text);
                            }
                            Err(why) => {
                                note(why);
                                modified = None;
                            }
                        }
                    }
                }
            }
        }
    }
    if staged {
        // Last, so a blob problem (binary, submodule, ...) is the reason shown when there is one.
        note(STAGED_READ_ONLY);
    }
    if commits {
        note("Commit diffs are read-only");
    }
    Ok(Loaded {
        content: DiffContent {
            path: file.path.clone(),
            original,
            modified,
            working_hash,
            eol: if crlf { "crlf" } else { "lf" }.into(),
            bom,
            read_only_reason: reason,
            original_label,
            modified_label,
        },
    })
}

/// Replaces the working file through a sibling temporary file, rechecking its hash right before the swap.
fn write_guarded(root: &Path, path: &str, bytes: &[u8], expected: Option<&str>) -> Result<()> {
    let parts = relative_parts(path)?;
    let target = root.join(parts.iter().collect::<PathBuf>());
    let parent = target.parent().ok_or_else(unsafe_path)?;
    let permissions = match inspect_working(root, path)? {
        Working::File(_, meta) => {
            if meta.permissions().readonly() {
                return Err("The file is read-only on disk; clear its read-only attribute first".into());
            }
            Some(meta.permissions())
        }
        Working::Missing => {
            fs::create_dir_all(parent).map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
            // Directories created above (or raced in) must still be plain directories inside the repository.
            if !matches!(inspect_working(root, path)?, Working::Missing) {
                return Err(CONFLICT.into());
            }
            None
        }
        Working::Blocked(why) => return Err(why.into()),
    };
    let name = target.file_name().ok_or_else(unsafe_path)?.to_string_lossy().into_owned();
    let temp = parent.join(format!(".{name}.omp-diff-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temp).map_err(|e| format!("Could not write {}: {e}", temp.display()))?;
        file.write_all(bytes).and_then(|_| file.sync_all()).map_err(|e| format!("Could not write {}: {e}", temp.display()))?;
        drop(file);
        if let Some(permissions) = permissions {
            fs::set_permissions(&temp, permissions).map_err(|e| e.to_string())?;
        }
        let current = match inspect_working(root, path)? {
            Working::File(file, _) => Some(hash(&read_limited(&file)?)),
            Working::Missing => None,
            Working::Blocked(why) => return Err(why.into()),
        };
        if current.as_deref() != expected {
            return Err(CONFLICT.into());
        }
        fs::rename(&temp, &target).map_err(|e| format!("Could not replace {path}: {e}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Repo(PathBuf);
    impl Repo {
        fn new(branch: &str) -> Self {
            let root = std::env::temp_dir().join(format!("omp-code-diff-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let repo = Repo(crate::setup::canonical(&root).unwrap());
            repo.git(&["init", "--quiet", &format!("--initial-branch={branch}")]);
            // Service-run commits (not the fixture's own) need a repository-local identity.
            repo.git(&["config", "user.name", "Test"]);
            repo.git(&["config", "user.email", "test@example.invalid"]);
            repo.git(&["config", "commit.gpgsign", "false"]);
            repo.git(&["config", "core.autocrlf", "false"]);
            repo
        }
        fn git(&self, args: &[&str]) -> String {
            let out = Command::new("git")
                .arg("-C")
                .arg(&self.0)
                .args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
        fn write(&self, path: &str, bytes: &[u8]) {
            let file = self.0.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, bytes).unwrap();
        }
        fn read(&self, path: &str) -> Vec<u8> {
            fs::read(self.0.join(path)).unwrap()
        }
        fn commit(&self, message: &str) {
            self.git(&["add", "-A"]);
            self.git(&["commit", "--quiet", "-m", message]);
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn statuses(overview: &DiffOverview) -> Vec<(String, String)> {
        overview.files.iter().map(|f| (f.path.clone(), f.status.clone())).collect()
    }
    fn paths(overview: &DiffOverview) -> Vec<&str> {
        overview.files.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn head_mode_lists_staged_unstaged_and_untracked_changes_against_the_last_commit() {
        let repo = Repo::new("master");
        repo.write("kept.txt", b"one\n");
        repo.write("changed.txt", b"a\n");
        repo.write("gone.txt", b"bye\n");
        repo.write("moved name.txt", b"same content here\nline two\nline three\n");
        repo.commit("base");
        repo.write("changed.txt", b"a\nb\n");
        repo.git(&["add", "changed.txt"]);
        repo.write("kept.txt", b"one\ntwo\n");
        fs::remove_file(repo.0.join("gone.txt")).unwrap();
        fs::rename(repo.0.join("moved name.txt"), repo.0.join("moved é.txt")).unwrap();
        repo.git(&["add", "-A", "moved name.txt", "moved é.txt"]);
        repo.write("sub dir/new ünï.txt", b"fresh\n");
        let index_before = repo.git(&["ls-files", "--stage"]);

        let service = DiffService::new();
        let overview = service.overview("a", &repo.0, DiffMode::Head).unwrap();
        assert_eq!(overview.mode, DiffMode::Head);
        assert_eq!(overview.base_ref.as_deref(), Some("HEAD"));
        assert_eq!(overview.branch_ref.as_deref(), Some("master"));
        assert_eq!(
            statuses(&overview),
            [("changed.txt", "M"), ("gone.txt", "D"), ("kept.txt", "M"), ("moved é.txt", "R"), ("sub dir/new ünï.txt", "?")]
                .map(|(p, s)| (p.to_string(), s.to_string()))
        );
        let renamed = overview.files.iter().find(|f| f.status == "R").unwrap();
        assert_eq!(renamed.old_path.as_deref(), Some("moved name.txt"));
        let content = service.read_file("a", &repo.0, &overview.snapshot_id, "moved é.txt", None).unwrap();
        assert_eq!(content.original, content.modified, "renames compare the old baseline path with the new file");
        let deleted = service.read_file("a", &repo.0, &overview.snapshot_id, "gone.txt", None).unwrap();
        assert_eq!((deleted.original.as_deref(), deleted.modified.as_deref(), deleted.working_hash), (Some("bye\n"), Some(""), None));
        let added = service.read_file("a", &repo.0, &overview.snapshot_id, "sub dir/new ünï.txt", None).unwrap();
        assert_eq!((added.original.as_deref(), added.modified.as_deref()), (Some(""), Some("fresh\n")));
        assert_eq!(repo.git(&["ls-files", "--stage"]), index_before, "inspecting a diff must not touch the index");
    }

    #[test]
    fn branch_mode_compares_the_branch_tip_with_the_working_tree() {
        let repo = Repo::new("master");
        repo.write("shared.txt", b"base\n");
        repo.commit("base");
        repo.git(&["switch", "--quiet", "-c", "feature"]);
        repo.write("shared.txt", b"base\nfeature commit\n");
        repo.write("feature.txt", b"added on branch\n");
        repo.commit("feature work");
        repo.write("shared.txt", b"base\nfeature commit\nworking edit\n");

        let service = DiffService::new();
        let head = service.overview("a", &repo.0, DiffMode::Head).unwrap();
        assert_eq!(paths(&head), ["shared.txt"]);
        let branch = service.overview("a", &repo.0, DiffMode::Branch).unwrap();
        assert_eq!(branch.base_ref.as_deref(), Some("master"));
        assert_eq!(branch.base_label, "Compared with master");
        assert_eq!(paths(&branch), ["feature.txt", "shared.txt"], "committed branch changes appear with working edits");
        let shared = service.read_file("a", &repo.0, &branch.snapshot_id, "shared.txt", None).unwrap();
        assert_eq!(shared.original.as_deref(), Some("base\n"));
        assert_eq!(shared.modified.as_deref(), Some("base\nfeature commit\nworking edit\n"));
        assert!(service.read_file("a", &repo.0, &head.snapshot_id, "shared.txt", None).is_err(), "a new overview invalidates the older snapshot");
    }

    #[test]
    fn a_comparison_has_no_side_effects_and_its_fingerprint_follows_the_change_set() {
        let repo = Repo::new("master");
        repo.write("a.txt", b"one\n");
        repo.commit("base");
        repo.write("a.txt", b"one\nedit\n");
        let service = DiffService::new();
        let overview = service.overview("a", &repo.0, DiffMode::Head).unwrap();
        let first = comparison(&repo.0, DiffMode::Uncommitted).unwrap();
        assert_eq!(first.label, "Uncommitted changes (staged and unstaged, against HEAD)");
        assert_eq!((first.base_commit.as_deref(), first.branch.as_deref()), (overview.base_commit.as_deref(), Some("master")));
        assert!(service.read_file("a", &repo.0, &overview.snapshot_id, "a.txt", None).is_ok(), "asking for a comparison keeps the page's snapshot valid");
        assert_eq!(comparison(&repo.0, DiffMode::Uncommitted).unwrap().fingerprint, first.fingerprint);
        assert_ne!(comparison(&repo.0, DiffMode::Head).unwrap().fingerprint, first.fingerprint, "the mode is part of it");
        repo.write("a.txt", b"one\ntwo\n");
        let edited = comparison(&repo.0, DiffMode::Uncommitted).unwrap().fingerprint;
        assert_ne!(edited, first.fingerprint);
        repo.write("new.txt", b"x\n");
        let untracked = comparison(&repo.0, DiffMode::Uncommitted).unwrap().fingerprint;
        assert_ne!(untracked, edited);
        repo.write("new.txt", b"changed content\n");
        assert_ne!(comparison(&repo.0, DiffMode::Uncommitted).unwrap().fingerprint, untracked, "editing an untracked file counts");
        assert_eq!(comparison(&repo.0, DiffMode::Branch).unwrap().label, "Compared with master");
        assert_eq!(comparison(&repo.0, DiffMode::Commits).err().as_deref(), Some("Use the commit comparison command for historical diffs"));

        let trunk = Repo::new("trunk");
        trunk.write("a.txt", b"1\n");
        assert_eq!(comparison(&trunk.0, DiffMode::Head).unwrap().base_commit, None, "no commits yet");
        trunk.commit("base");
        assert!(comparison(&trunk.0, DiffMode::Branch).unwrap_err().contains("No master or default branch"));
        assert!(comparison(&trunk.0, DiffMode::Head).is_ok());
    }

    #[test]
    fn default_branch_prefers_master_then_the_recorded_default_then_main() {
        let repo = Repo::new("main");
        repo.write("a.txt", b"1\n");
        repo.commit("base");
        repo.write("a.txt", b"2\n");
        let service = DiffService::new();
        let overview = service.overview("a", &repo.0, DiffMode::Branch).unwrap();
        assert_eq!((overview.base_ref.as_deref(), overview.base_label.as_str()), (Some("main"), "Compared with main"));
        repo.git(&["branch", "master"]);
        assert_eq!(service.overview("a", &repo.0, DiffMode::Head).unwrap().branch_ref.as_deref(), Some("master"));

        let trunk = Repo::new("trunk");
        trunk.write("a.txt", b"1\n");
        trunk.commit("base");
        assert!(service.overview("t", &trunk.0, DiffMode::Branch).unwrap_err().contains("No master or default branch"));
        assert_eq!(service.overview("t", &trunk.0, DiffMode::Head).unwrap().branch_ref, None, "head mode stays usable");
        trunk.git(&["config", "init.defaultBranch", "trunk"]);
        assert_eq!(service.overview("t", &trunk.0, DiffMode::Branch).unwrap().base_ref.as_deref(), Some("trunk"));

        let clone_parent = Repo::new("develop");
        clone_parent.write("a.txt", b"1\n");
        clone_parent.commit("base");
        let clone = std::env::temp_dir().join(format!("omp-code-diff-clone-{}", uuid::Uuid::new_v4()));
        let out = Command::new("git").args(["clone", "--quiet"]).arg(&clone_parent.0).arg(&clone).output().unwrap();
        assert!(out.status.success());
        let clone = crate::setup::canonical(&clone).unwrap();
        let overview = service.overview("c", &clone, DiffMode::Branch).unwrap();
        assert_eq!(overview.base_ref.as_deref(), Some("develop"), "a clone's remote HEAD names its default branch");
        fs::remove_dir_all(clone).unwrap();
    }

    #[test]
    fn repositories_without_commits_show_everything_as_added() {
        let repo = Repo::new("master");
        repo.write("staged.txt", b"s\n");
        repo.git(&["add", "staged.txt"]);
        repo.write("loose.txt", b"l\n");
        let service = DiffService::new();
        let overview = service.overview("a", &repo.0, DiffMode::Head).unwrap();
        assert_eq!(overview.base_commit, None);
        assert_eq!(overview.base_label, "No commits yet");
        assert_eq!(statuses(&overview), [("loose.txt", "?"), ("staged.txt", "A")].map(|(p, s)| (p.to_string(), s.to_string())));
        let content = service.read_file("a", &repo.0, &overview.snapshot_id, "staged.txt", None).unwrap();
        assert_eq!((content.original.as_deref(), content.modified.as_deref()), (Some(""), Some("s\n")));
        assert!(service.overview("a", &repo.0, DiffMode::Branch).is_err());
    }

    #[test]
    fn non_repositories_and_stale_views_are_explained() {
        let plain = std::env::temp_dir().join(format!("omp-code-diff-plain-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&plain).unwrap();
        let service = DiffService::new();
        assert_eq!(service.overview("a", &plain, DiffMode::Head).unwrap_err(), "This project is not inside a Git repository");
        assert!(service.overview("a", &plain.join("missing"), DiffMode::Head).is_err());
        fs::remove_dir_all(plain).unwrap();

        let one = Repo::new("master");
        one.write("a.txt", b"1\n");
        one.commit("base");
        one.write("a.txt", b"2\n");
        let two = Repo::new("master");
        two.write("a.txt", b"1\n");
        two.commit("base");
        let snapshot = service.overview("a", &one.0, DiffMode::Head).unwrap();
        assert_eq!(service.read_file("a", &two.0, &snapshot.snapshot_id, "a.txt", None).unwrap_err(), CHANGED);
        assert_eq!(service.read_file("a", &one.0, "stale", "a.txt", None).unwrap_err(), CHANGED);
        assert_eq!(service.read_file("other", &one.0, &snapshot.snapshot_id, "a.txt", None).unwrap_err(), CHANGED);
        assert!(service.read_file("a", &one.0, &snapshot.snapshot_id, "never-listed.txt", None).is_err());
    }

    #[test]
    fn unsafe_paths_never_reach_the_filesystem() {
        for path in ["", "/abs", "C:/abs", "a/../b", "../b", "a//b", "./a", ".git/config", "a/.GIT/config", "GIT~1/config", "a:stream", "a\\b", "dir /x", "x.", "a/\0"] {
            assert!(relative_parts(path).is_err(), "{path:?} must be rejected");
        }
        assert_eq!(relative_parts("src/lib/ä b.rs").unwrap(), ["src", "lib", "ä b.rs"]);
        assert!(relative_parts(".github/workflows/ci.yml").is_ok());
    }

    #[test]
    fn unsupported_content_is_read_only_with_a_reason() {
        let repo = Repo::new("master");
        repo.write("binary.bin", &[1, 2, 0, 3]);
        repo.write("latin.txt", b"caf\xe9\n");
        repo.write("mixed.txt", b"a\r\nb\nc\n");
        repo.write("lone.txt", b"a\rb\n");
        repo.write("big.txt", b"x\n");
        repo.write("ok.txt", b"ok\n");
        repo.commit("base");
        repo.write("binary.bin", &[1, 2, 0, 4]);
        repo.write("latin.txt", b"caf\xe9!\n");
        repo.write("mixed.txt", b"a\r\nb\nc\nd\n");
        repo.write("lone.txt", b"a\rb\nc\n");
        repo.write("big.txt", &vec![b'y'; MAX_BYTES as usize + 1]);
        repo.write("ok.txt", b"ok\nmore\n");
        let service = DiffService::new();
        let overview = service.overview("a", &repo.0, DiffMode::Head).unwrap();
        let reasons: Vec<(String, Option<String>)> = overview
            .files
            .iter()
            .map(|f| (f.path.clone(), service.read_file("a", &repo.0, &overview.snapshot_id, &f.path, None).unwrap().read_only_reason))
            .collect();
        let reason = |name: &str| reasons.iter().find(|(p, _)| p == name).unwrap().1.clone();
        assert!(reason("binary.bin").unwrap().contains("Binary"));
        assert!(reason("latin.txt").unwrap().contains("UTF-8"));
        assert!(reason("mixed.txt").unwrap().contains("mixed line endings"));
        assert!(reason("lone.txt").unwrap().contains("lone carriage"));
        assert!(reason("big.txt").unwrap().contains("2 MiB"));
        assert_eq!(reason("ok.txt"), None);
        let blocked = service.save_file("a", &repo.0, &overview.snapshot_id, "mixed.txt", None, Some("x"), "replaced\n").unwrap_err();
        assert!(blocked.contains("mixed line endings"));
        assert_eq!(repo.read("mixed.txt"), b"a\r\nb\nc\nd\n", "read-only files are never rewritten");
    }

    #[test]
    fn saving_preserves_bom_and_line_endings_and_leaves_the_index_alone() {
        let repo = Repo::new("master");
        repo.write("crlf.txt", b"\xef\xbb\xbfone\r\ntwo\r\n");
        repo.write("lf.txt", b"one\ntwo");
        repo.commit("base");
        repo.write("crlf.txt", b"\xef\xbb\xbfone\r\ntwo\r\nthree\r\n");
        repo.write("lf.txt", b"one\ntwo\nthree");
        let index_before = repo.git(&["ls-files", "--stage"]);
        let service = DiffService::new();
        let overview = service.overview("a", &repo.0, DiffMode::Head).unwrap();

        let crlf = service.read_file("a", &repo.0, &overview.snapshot_id, "crlf.txt", None).unwrap();
        assert_eq!((crlf.eol.as_str(), crlf.bom, crlf.modified.as_deref()), ("crlf", true, Some("one\ntwo\nthree\n")));
        let saved = service.save_file("a", &repo.0, &overview.snapshot_id, "crlf.txt", None, crlf.working_hash.as_deref(), "one\ntwo\nthree\nfour\n").unwrap();
        assert_eq!(repo.read("crlf.txt"), b"\xef\xbb\xbfone\r\ntwo\r\nthree\r\nfour\r\n");
        assert_eq!(saved.working_hash.as_deref(), Some(hash(&repo.read("crlf.txt")).as_str()));

        let lf = service.read_file("a", &repo.0, &overview.snapshot_id, "lf.txt", None).unwrap();
        service.save_file("a", &repo.0, &overview.snapshot_id, "lf.txt", None, lf.working_hash.as_deref(), "one\ntwo\nthree\nno final newline").unwrap();
        assert_eq!(repo.read("lf.txt"), b"one\ntwo\nthree\nno final newline");
        assert_eq!(repo.git(&["ls-files", "--stage"]), index_before);
        assert!(repo.git(&["diff", "--stat"]).contains("crlf.txt"), "the saved change is visible to Git");
        let leftovers = fs::read_dir(&repo.0).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn concurrent_disk_changes_are_reported_instead_of_overwritten() {
        let repo = Repo::new("master");
        repo.write("a.txt", b"base\n");
        repo.commit("base");
        repo.write("a.txt", b"mine\n");
        let service = DiffService::new();
        let overview = service.overview("a", &repo.0, DiffMode::Head).unwrap();
        let loaded = service.read_file("a", &repo.0, &overview.snapshot_id, "a.txt", None).unwrap();
        repo.write("a.txt", b"agent wrote this\n");
        let error = service.save_file("a", &repo.0, &overview.snapshot_id, "a.txt", None, loaded.working_hash.as_deref(), "typed text\n").unwrap_err();
        assert_eq!(error, CONFLICT);
        assert_eq!(repo.read("a.txt"), b"agent wrote this\n", "no bytes are lost to a conflicting save");
        let fresh = service.read_file("a", &repo.0, &overview.snapshot_id, "a.txt", None).unwrap();
        service.save_file("a", &repo.0, &overview.snapshot_id, "a.txt", None, fresh.working_hash.as_deref(), "typed text\n").unwrap();
        assert_eq!(repo.read("a.txt"), b"typed text\n");
        assert_eq!(service.save_file("a", &repo.0, &overview.snapshot_id, "a.txt", None, None, "x\n").unwrap_err(), CONFLICT);
    }

    #[test]
    fn a_deleted_file_can_be_recreated_from_its_empty_side() {
        let repo = Repo::new("master");
        repo.write("deep/dir/file.txt", b"old\n");
        repo.write("keep.txt", b"k\n");
        repo.commit("base");
        fs::remove_dir_all(repo.0.join("deep")).unwrap();
        let service = DiffService::new();
        let overview = service.overview("a", &repo.0, DiffMode::Head).unwrap();
        let deleted = service.read_file("a", &repo.0, &overview.snapshot_id, "deep/dir/file.txt", None).unwrap();
        assert_eq!((deleted.working_hash.as_deref(), deleted.original.as_deref()), (None, Some("old\n")));
        service.save_file("a", &repo.0, &overview.snapshot_id, "deep/dir/file.txt", None, None, "restored\n").unwrap();
        assert_eq!(repo.read("deep/dir/file.txt"), b"restored\n");
    }

    #[test]
    fn links_and_junctions_are_never_written_through() {
        let repo = Repo::new("master");
        repo.write("real/file.txt", b"1\n");
        repo.commit("base");
        let outside = std::env::temp_dir().join(format!("omp-code-diff-outside-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("file.txt"), b"outside\n").unwrap();
        fs::remove_dir_all(repo.0.join("real")).unwrap();
        let link = repo.0.join("real");
        #[cfg(windows)]
        let linked = Command::new("cmd.exe").args(["/C", "mklink", "/J"]).arg(&link).arg(&outside).output().unwrap().status.success();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, &link).is_ok();
        if linked {
            let service = DiffService::new();
            let overview = service.overview("a", &repo.0, DiffMode::Head).unwrap();
            if let Some(file) = overview.files.iter().find(|f| f.path == "real/file.txt") {
                let content = service.read_file("a", &repo.0, &overview.snapshot_id, &file.path, None).unwrap();
                assert!(content.read_only_reason.is_some());
                assert!(service.save_file("a", &repo.0, &overview.snapshot_id, &file.path, None, content.working_hash.as_deref(), "pwn\n").is_err());
            }
            assert_eq!(fs::read(outside.join("file.txt")).unwrap(), b"outside\n");
            #[cfg(windows)]
            fs::remove_dir(&link).unwrap();
            #[cfg(unix)]
            fs::remove_file(&link).unwrap();
        } else {
            eprintln!("link containment unexercised: link creation refused");
        }
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn name_status_parser_handles_renames_copies_and_unmerged_entries() {
        let parsed = parse_name_status(b"M\0a b.txt\0R087\0old.txt\0new.txt\0C100\0src.txt\0copy.txt\0U\0conflict.txt\0M\0conflict.txt\0D\0gone.txt\0");
        let rows: Vec<(&str, &str, Option<&str>)> = parsed.iter().map(|f| (f.path.as_str(), f.status.as_str(), f.old_path.as_deref())).collect();
        assert_eq!(
            rows,
            [("a b.txt", "M", None), ("conflict.txt", "U", None), ("copy.txt", "C", Some("src.txt")), ("gone.txt", "D", None), ("new.txt", "R", Some("old.txt"))]
        );
    }

    // ------------------------------------------------ uncommitted view, actions, commit, publish

    fn lines(text: String) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }
    fn staged(repo: &Repo) -> Vec<String> {
        lines(repo.git(&["diff", "--cached", "--name-only"]))
    }
    fn unstaged(repo: &Repo) -> Vec<String> {
        lines(repo.git(&["diff", "--name-only"]))
    }
    fn rows(overview: &DiffOverview) -> Vec<(Option<DiffGroup>, String, String)> {
        overview.files.iter().map(|f| (f.group, f.path.clone(), f.status.clone())).collect()
    }
    fn row(group: DiffGroup, path: &str, status: &str) -> (Option<DiffGroup>, String, String) {
        (Some(group), path.into(), status.into())
    }
    /// Staged: a.txt (M), added.txt (A), new.txt (R from old.txt). Changes: a.txt (M again), b.txt (M), gone.txt (D), u.txt (?).
    fn dirty_repo() -> Repo {
        let repo = Repo::new("master");
        repo.write("a.txt", b"a1\n");
        repo.write("b.txt", b"b1\n");
        repo.write("old.txt", b"same content here\nline two\nline three\n");
        repo.write("gone.txt", b"bye\n");
        repo.commit("base");
        repo.write("a.txt", b"a2\n");
        repo.git(&["add", "a.txt"]);
        repo.write("a.txt", b"a3\n");
        repo.write("b.txt", b"b2\n");
        repo.git(&["mv", "old.txt", "new.txt"]);
        repo.write("added.txt", b"added\n");
        repo.git(&["add", "added.txt"]);
        repo.write("u.txt", b"untracked\n");
        fs::remove_file(repo.0.join("gone.txt")).unwrap();
        repo
    }
    fn fresh(service: &DiffService, repo: &Repo) -> String {
        service.overview("a", &repo.0, DiffMode::Uncommitted).unwrap().snapshot_id
    }

    #[test]
    fn status_v2_records_become_staged_and_changes_entries() {
        let bytes = b"1 MM N... 100644 100644 100644 aaa bbb dir/a b.txt\0\
            1 A. N... 000000 100644 100644 000 ccc new.txt\0\
            2 R. N... 100644 100644 100644 ddd eee R100 moved \xc3\xa9.txt\0moved name.txt\0\
            u UU N... 100644 100644 100644 100644 f1 f2 f3 conflict.txt\0\
            ? untracked file.txt\0? nested/\0! ignored.log\0\
            1 .D N... 100644 100644 000000 ggg ggg gone.txt\0";
        let parsed = parse_status_v2(bytes);
        let got: Vec<(Option<DiffGroup>, &str, &str, Option<&str>)> = parsed.iter().map(|f| (f.group, f.path.as_str(), f.status.as_str(), f.old_path.as_deref())).collect();
        assert_eq!(
            got,
            [
                (Some(DiffGroup::Staged), "dir/a b.txt", "M", None),
                (Some(DiffGroup::Staged), "moved é.txt", "R", Some("moved name.txt")),
                (Some(DiffGroup::Staged), "new.txt", "A", None),
                (Some(DiffGroup::Changes), "conflict.txt", "U", None),
                (Some(DiffGroup::Changes), "dir/a b.txt", "M", None),
                (Some(DiffGroup::Changes), "gone.txt", "D", None),
                (Some(DiffGroup::Changes), "untracked file.txt", "?", None),
            ]
        );
    }

    #[test]
    fn uncommitted_view_shows_vscode_style_sides_for_each_group() {
        let repo = dirty_repo();
        let service = DiffService::new();
        let ov = service.overview("a", &repo.0, DiffMode::Uncommitted).unwrap();
        assert_eq!(ov.base_label, "Uncommitted changes");
        assert_eq!((ov.current_branch.as_deref(), ov.upstream.as_deref()), (Some("master"), None));
        assert_eq!(
            rows(&ov),
            [
                row(DiffGroup::Staged, "a.txt", "M"),
                row(DiffGroup::Staged, "added.txt", "A"),
                row(DiffGroup::Staged, "new.txt", "R"),
                row(DiffGroup::Changes, "a.txt", "M"),
                row(DiffGroup::Changes, "b.txt", "M"),
                row(DiffGroup::Changes, "gone.txt", "D"),
                row(DiffGroup::Changes, "u.txt", "?"),
            ]
        );
        let read = |group, path: &str| service.read_file("a", &repo.0, &ov.snapshot_id, path, Some(group)).unwrap();
        let side = |c: &DiffContent| (c.original.clone().unwrap(), c.modified.clone().unwrap());
        let staged_a = read(DiffGroup::Staged, "a.txt");
        assert_eq!(side(&staged_a), ("a1\n".to_string(), "a2\n".to_string()), "staged = HEAD vs index");
        assert_eq!(staged_a.read_only_reason.as_deref(), Some(STAGED_READ_ONLY));
        assert_eq!(staged_a.working_hash, None);
        assert!(staged_a.original_label.starts_with("Last commit ") && staged_a.modified_label == "Staged (index)");
        let changes_a = read(DiffGroup::Changes, "a.txt");
        assert_eq!(side(&changes_a), ("a2\n".to_string(), "a3\n".to_string()), "changes = index vs working tree");
        assert_eq!(changes_a.read_only_reason, None);
        assert!(changes_a.working_hash.is_some());
        assert_eq!((changes_a.original_label.as_str(), changes_a.modified_label.as_str()), ("Staged (index)", "Working tree"));
        assert_eq!(side(&read(DiffGroup::Staged, "added.txt")), (String::new(), "added\n".to_string()));
        let renamed = side(&read(DiffGroup::Staged, "new.txt"));
        assert_eq!(renamed.0, "same content here\nline two\nline three\n");
        assert_eq!(renamed.0, renamed.1, "a staged rename compares the old HEAD path with the index");
        assert_eq!(side(&read(DiffGroup::Changes, "gone.txt")), ("bye\n".to_string(), String::new()));
        assert_eq!(side(&read(DiffGroup::Changes, "u.txt")), (String::new(), "untracked\n".to_string()));
        assert!(service.read_file("a", &repo.0, &ov.snapshot_id, "a.txt", None).is_err(), "entries are keyed by group");
        let head = service.overview("a", &repo.0, DiffMode::Head).unwrap();
        assert!(head.files.iter().all(|f| f.group.is_none()));
    }

    #[test]
    fn stage_unstage_and_revert_change_only_what_they_name() {
        let repo = dirty_repo();
        let service = DiffService::new();
        let act = |path: &str, group, action| {
            let id = fresh(&service, &repo);
            service.change_file("a", &repo.0, &id, path, Some(group), action)
        };
        act("b.txt", DiffGroup::Changes, FileAction::Stage).unwrap();
        assert!(staged(&repo).contains(&"b.txt".to_string()) && !unstaged(&repo).contains(&"b.txt".to_string()));
        let stale = fresh(&service, &repo);
        service.change_file("a", &repo.0, &stale, "b.txt", Some(DiffGroup::Staged), FileAction::Unstage).unwrap();
        assert!(!staged(&repo).contains(&"b.txt".to_string()) && unstaged(&repo).contains(&"b.txt".to_string()));
        assert_eq!(service.read_file("a", &repo.0, &stale, "b.txt", Some(DiffGroup::Changes)).unwrap_err(), CHANGED, "an action drops the snapshot");

        // Revert discards only the unstaged edit: the staged version survives.
        act("a.txt", DiffGroup::Changes, FileAction::Revert).unwrap();
        assert_eq!(repo.read("a.txt"), b"a2\n");
        assert!(staged(&repo).contains(&"a.txt".to_string()));
        act("b.txt", DiffGroup::Changes, FileAction::Revert).unwrap();
        assert_eq!(repo.read("b.txt"), b"b1\n");
        act("gone.txt", DiffGroup::Changes, FileAction::Revert).unwrap();
        assert_eq!(repo.read("gone.txt"), b"bye\n");
        act("u.txt", DiffGroup::Changes, FileAction::Revert).unwrap();
        assert!(!repo.0.join("u.txt").exists(), "reverting an untracked file deletes it");

        // A staged rename is unstaged through both of its paths.
        act("new.txt", DiffGroup::Staged, FileAction::Unstage).unwrap();
        assert!(!staged(&repo).iter().any(|p| p == "new.txt" || p == "old.txt"));
        assert!(lines(repo.git(&["ls-files", "--others", "--exclude-standard"])).contains(&"new.txt".to_string()));
        assert!(unstaged(&repo).contains(&"old.txt".to_string()));

        // Wrong group, unknown path and non-Uncommitted snapshots are rejected.
        assert_eq!(act("a.txt", DiffGroup::Staged, FileAction::Stage).unwrap_err(), "That action does not apply to this entry");
        assert_eq!(act("added.txt", DiffGroup::Staged, FileAction::Revert).unwrap_err(), "That action does not apply to this entry");
        let id = fresh(&service, &repo);
        assert_eq!(service.change_file("a", &repo.0, &id, "nope.txt", Some(DiffGroup::Changes), FileAction::Stage).unwrap_err(), "That file is not part of this comparison");
        let head = service.overview("a", &repo.0, DiffMode::Head).unwrap();
        assert_eq!(service.change_file("a", &repo.0, &head.snapshot_id, "b.txt", None, FileAction::Stage).unwrap_err(), "Git actions are only available in Uncommitted changes");
    }

    #[test]
    fn conflicts_and_intent_to_add_cannot_be_reverted_and_a_first_commit_can_be_unstaged() {
        let repo = Repo::new("master");
        repo.write("f.txt", b"first\n");
        repo.git(&["add", "f.txt"]);
        let service = DiffService::new();
        let id = fresh(&service, &repo);
        service.change_file("a", &repo.0, &id, "f.txt", Some(DiffGroup::Staged), FileAction::Unstage).unwrap();
        assert_eq!(repo.git(&["ls-files", "--cached"]).trim(), "", "with no commit, unstaging removes the index entry");
        assert_eq!(repo.read("f.txt"), b"first\n");

        let snapshot = Snapshot { id: "s".into(), root: repo.0.clone(), mode: DiffMode::Uncommitted, base_commit: None, base_label: "x".into(), target_commit: None, target_label: "Working tree".into(), files: HashMap::new() };
        for (status, message) in [("U", "Resolve merge conflicts with Git first"), ("A", "Intent-to-add files can only be unstaged with Git")] {
            let file = DiffFile { path: "f.txt".into(), old_path: None, status: status.into(), group: Some(DiffGroup::Changes) };
            assert_eq!(apply_file_action(&snapshot, &file, FileAction::Revert).unwrap_err(), message);
        }
        assert_eq!(repo.read("f.txt"), b"first\n");
    }

    #[test]
    fn commit_records_only_staged_changes_with_the_typed_message() {
        let repo = dirty_repo();
        let service = DiffService::new();
        let id = fresh(&service, &repo);
        assert_eq!(service.commit("a", &repo.0, &id, "   ").unwrap_err(), "Enter a commit message");
        let id = fresh(&service, &repo);
        let result = service.commit("a", &repo.0, &id, "  Subject line\n\nBody text\n  ").unwrap();
        assert_eq!(result.summary, "Subject line");
        assert_eq!(result.commit, repo.git(&["rev-parse", "--short", "HEAD"]).trim());
        assert_eq!(repo.git(&["log", "-1", "--format=%B"]).trim(), "Subject line\n\nBody text");
        assert!(staged(&repo).is_empty());
        assert!(unstaged(&repo).contains(&"b.txt".to_string()) && unstaged(&repo).contains(&"a.txt".to_string()), "unstaged edits stay uncommitted");
        assert_eq!(repo.read("a.txt"), b"a3\n");
        let id = fresh(&service, &repo);
        assert_eq!(service.commit("a", &repo.0, &id, "again").unwrap_err(), "Nothing is staged to commit");
        let head = service.overview("a", &repo.0, DiffMode::Head).unwrap();
        assert_eq!(service.commit("a", &repo.0, &head.snapshot_id, "x").unwrap_err(), "Git actions are only available in Uncommitted changes");
    }

    #[test]
    fn a_first_commit_works_and_a_failing_hook_is_reported_without_losing_the_stage() {
        let repo = Repo::new("master");
        repo.write("f.txt", b"first\n");
        repo.git(&["add", "f.txt"]);
        let hook = repo.0.join(".git").join("hooks").join("pre-commit");
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::write(&hook, "#!/bin/sh\necho blocked >&2\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let service = DiffService::new();
        let id = fresh(&service, &repo);
        let error = service.commit("a", &repo.0, &id, "initial").unwrap_err();
        assert!(error.contains("blocked"), "{error}");
        assert_eq!(staged(&repo), ["f.txt"]);
        fs::remove_file(&hook).unwrap();
        let id = fresh(&service, &repo);
        service.commit("a", &repo.0, &id, "initial").unwrap();
        assert_eq!(repo.git(&["log", "--format=%s"]).trim(), "initial");
    }

    fn bare_remote() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("omp-code-diff-remote-{}", uuid::Uuid::new_v4()));
        let out = Command::new("git").args(["init", "--bare", "--quiet", "--initial-branch=master"]).arg(&dir).output().unwrap();
        assert!(out.status.success());
        dir
    }
    fn committed_repo_with_remote(remote: &Path) -> Repo {
        let repo = Repo::new("master");
        repo.write("f.txt", b"one\n");
        repo.commit("base");
        repo.git(&["remote", "add", "origin", &remote.to_string_lossy()]);
        repo
    }
    fn remote_branches(remote: &Path) -> String {
        let out = Command::new("git").arg("-C").arg(remote).args(["branch", "--list"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    fn publish(service: &DiffService, repo: &Repo, new_branch: Option<&str>, create_pr: bool) -> Result<PublishResult> {
        let id = fresh(service, repo);
        service.publish("a", &repo.0, &id, &PublishOptions { new_branch: new_branch.map(str::to_string), create_pr })
    }

    #[test]
    fn publish_creates_a_branch_pushes_it_and_later_pushes_plainly() {
        let remote = bare_remote();
        let repo = committed_repo_with_remote(&remote);
        let service = DiffService::new();
        let first = publish(&service, &repo, Some("feature-x"), false).unwrap();
        assert_eq!((first.branch.as_str(), first.remote.as_str(), first.pr_url.as_deref()), ("feature-x", "origin", None));
        assert_eq!(first.steps, ["Created branch feature-x", "Pushed feature-x to origin"]);
        assert!(remote_branches(&remote).contains("feature-x"));
        assert_eq!(repo.git(&["rev-parse", "--abbrev-ref", "feature-x@{u}"]).trim(), "origin/feature-x");
        let ov = service.overview("a", &repo.0, DiffMode::Uncommitted).unwrap();
        assert_eq!((ov.current_branch.as_deref(), ov.upstream.as_deref()), (Some("feature-x"), Some("origin/feature-x")));

        repo.write("f.txt", b"two\n");
        repo.commit("second");
        let second = publish(&service, &repo, None, false).unwrap();
        assert_eq!(second.steps, ["Pushed feature-x to origin"]);
        let remote_tip = Command::new("git").arg("-C").arg(&remote).args(["rev-parse", "feature-x"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&remote_tip.stdout).trim(), repo.git(&["rev-parse", "HEAD"]).trim());
        fs::remove_dir_all(remote).unwrap();
    }

    #[test]
    fn publish_rejects_bad_branch_names_detached_heads_and_missing_remotes() {
        let remote = bare_remote();
        let repo = committed_repo_with_remote(&remote);
        let service = DiffService::new();
        for bad in ["bad name", "-x", "a..b", "x\ty", "master"] {
            let error = publish(&service, &repo, Some(bad), false).unwrap_err();
            assert!(error.contains("Invalid branch name") || error.contains("already exists"), "{bad}: {error}");
        }
        assert_eq!(publish(&service, &repo, Some("  "), false).unwrap_err(), "Enter a name for the new branch");
        assert_eq!(repo.git(&["branch", "--show-current"]).trim(), "master", "a rejected name must not switch branches");
        repo.git(&["checkout", "--detach"]);
        assert_eq!(publish(&service, &repo, None, false).unwrap_err(), "HEAD is detached; enter a new branch name to publish");
        repo.git(&["checkout", "master"]);
        fs::remove_dir_all(remote).unwrap();

        let lonely = Repo::new("master");
        lonely.write("f.txt", b"one\n");
        lonely.commit("base");
        let error = publish(&service, &lonely, Some("solo"), false).unwrap_err();
        assert_eq!(error, "Created branch solo. Then: No Git remote is configured", "completed steps are reported with the failure");
        assert_eq!(lonely.git(&["branch", "--show-current"]).trim(), "solo");
    }

    /// A fake `gh` that records its arguments and prints a pull request URL (or fails).
    fn fake_gh(dir: &Path, succeed: bool) -> (std::ffi::OsString, PathBuf) {
        let args = dir.join("gh-args.txt");
        #[cfg(windows)]
        let (script, body) = (
            dir.join("fake-gh.cmd"),
            if succeed {
                format!("@echo off\r\necho %*> \"{}\"\r\necho Creating pull request 1>&2\r\necho https://example.invalid/pull/7\r\n", args.display())
            } else {
                "@echo off\r\necho a pull request for this branch already exists 1>&2\r\nexit /b 1\r\n".to_string()
            },
        );
        #[cfg(not(windows))]
        let (script, body) = (
            dir.join("fake-gh"),
            if succeed {
                format!("#!/bin/sh\necho \"$@\" > \"{}\"\necho Creating pull request >&2\necho https://example.invalid/pull/7\n", args.display())
            } else {
                "#!/bin/sh\necho a pull request for this branch already exists >&2\nexit 1\n".to_string()
            },
        );
        fs::write(&script, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        (script.into_os_string(), args)
    }

    #[test]
    fn pull_requests_use_gh_after_the_push_and_report_its_failures() {
        let remote = bare_remote();
        let repo = committed_repo_with_remote(&remote);
        let tools = std::env::temp_dir().join(format!("omp-code-diff-gh-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&tools).unwrap();

        let (gh, args) = fake_gh(&tools, true);
        let service = DiffService::with_gh(gh);
        let result = publish(&service, &repo, Some("feature-pr"), true).unwrap();
        assert_eq!(result.pr_url.as_deref(), Some("https://example.invalid/pull/7"));
        assert_eq!(result.steps, ["Created branch feature-pr", "Pushed feature-pr to origin", "Created pull request https://example.invalid/pull/7"]);
        assert!(fs::read_to_string(&args).unwrap().trim().ends_with("pr create --fill --head feature-pr"));
        assert!(remote_branches(&remote).contains("feature-pr"), "the branch is pushed before the PR is created");

        let failing_dir = tools.join("failing");
        fs::create_dir_all(&failing_dir).unwrap();
        let (failing, _) = fake_gh(&failing_dir, false);
        let service = DiffService::with_gh(failing);
        repo.write("f.txt", b"two\n");
        repo.commit("second");
        let error = publish(&service, &repo, None, true).unwrap_err();
        assert_eq!(error, "Pushed feature-pr to origin. Then: a pull request for this branch already exists");

        let service = DiffService::with_gh("definitely-not-a-program-omp".into());
        let error = publish(&service, &repo, None, true).unwrap_err();
        assert!(error.ends_with(GH_MISSING), "{error}");
        assert_eq!(service.open_pull_request("a").unwrap_err(), "No pull request was created in this session");
        fs::remove_dir_all(remote).unwrap();
        fs::remove_dir_all(tools).unwrap();
    }

    #[test]
    fn commands_are_killed_when_they_exceed_their_deadline() {
        #[cfg(windows)]
        let cmd = {
            let mut cmd = Command::new("ping");
            cmd.args(["-n", "20", "127.0.0.1"]);
            cmd
        };
        #[cfg(not(windows))]
        let cmd = {
            let mut cmd = Command::new("sleep");
            cmd.arg("20");
            cmd
        };
        let started = Instant::now();
        assert_eq!(run_timed(cmd, None, Duration::from_secs(1), "missing").unwrap_err(), "Timed out after 1 s");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(run_timed(Command::new("definitely-not-a-program-omp"), None, Duration::from_secs(1), "missing").unwrap_err(), "missing");
    }

    #[test]
    fn historical_diffs_pin_blobs_and_never_read_or_save_the_working_tree() {
        let repo = Repo::new("master");
        repo.write("edit.txt", b"before\n");
        repo.write("delete.txt", b"gone\n");
        repo.write("old.txt", b"stable renamed content\nmore lines here\n");
        repo.commit("initial");
        let first = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.write("edit.txt", b"after\n");
        repo.write("add.txt", b"new\n");
        fs::remove_file(repo.0.join("delete.txt")).unwrap();
        repo.git(&["mv", "old.txt", "renamed.txt"]);
        repo.commit("changes");
        let second = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.write("edit.txt", b"unrelated disk edit\n");
        let service = DiffService::new();
        let overview = service.commit_overview("a", &repo.0, None, &second).unwrap();
        assert_eq!(overview.base_commit.as_deref(), Some(first.as_str()));
        assert_eq!(statuses(&overview), [("add.txt", "A"), ("delete.txt", "D"), ("edit.txt", "M"), ("renamed.txt", "R")].map(|(p,s)| (p.into(),s.into())));
        for (path, original, modified) in [("edit.txt", "before\n", "after\n"), ("add.txt", "", "new\n"), ("delete.txt", "gone\n", ""), ("renamed.txt", "stable renamed content\nmore lines here\n", "stable renamed content\nmore lines here\n")] {
            let content = service.read_file("a", &repo.0, &overview.snapshot_id, path, None).unwrap();
            assert_eq!((content.original.as_deref(), content.modified.as_deref()), (Some(original), Some(modified)));
            assert_eq!(content.read_only_reason.as_deref(), Some("Commit diffs are read-only"));
            assert!(content.original_label.ends_with(" initial") && content.modified_label.ends_with(" changes"));
            assert!(content.working_hash.is_none());
        }
        assert_eq!(service.save_file("a", &repo.0, &overview.snapshot_id, "edit.txt", None, None, "bad").unwrap_err(), "Commit diffs are read-only");
        assert_eq!(repo.read("edit.txt"), b"unrelated disk edit\n");
        let root = service.commit_overview("a", &repo.0, None, &first).unwrap();
        assert_eq!(root.base_label, "Empty tree");
        assert!(root.files.iter().all(|f| f.status == "A"));
        let content = service.read_file("a", &repo.0, &root.snapshot_id, "edit.txt", None).unwrap();
        assert_eq!((content.original.as_deref(), content.modified.as_deref()), (Some(""), Some("before\n")));
        repo.write("edit.txt", b"third\n");repo.commit("third");
        let third = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        let pair = service.commit_overview("a", &repo.0, Some(&first), &third).unwrap();
        let content = service.read_file("a", &repo.0, &pair.snapshot_id, "edit.txt", None).unwrap();
        assert_eq!((content.original.as_deref(), content.modified.as_deref()), (Some("before\n"), Some("third\n")));
        for bad in ["HEAD", "--all", "not-a-hash"] { assert!(service.commit_overview("a", &repo.0, None, bad).unwrap_err().starts_with("Not a commit:")); }
    }
}
