//! Passive "Project requirements": what a repository itself declares it needs (runtimes, build tools,
//! package managers, container engines, OS features, system settings, executables), whether this computer
//! appears to have it, and whether OMP Manager can install it.
//!
//! Everything here is read-only. Files come from the ignore-filtered, link-free enumeration shared with
//! Code quality snapshots (bounded depth and count, secret-looking files removed before anything is read).
//! Tools are only located and asked for `--version` (fixed allowlist, 5 s limit, run without a shell). Project
//! scripts, builds, installers and container engines are never started; `install::probe_prerequisites` (via the
//! Code quality service's probe seam) is the only machine probe and is itself read-only. A probe that fails or
//! times out yields `unknown`, never `missing`.
use crate::code_quality::runtime::{discover_program, docker_program, podman_program, program_files, run_bounded, wsl_program, Ctl};
use crate::code_quality::scanners::{display_folder, plain_path, read_config};
use crate::code_quality::snapshot::list_source_files;
use crate::code_quality::types::{Prerequisite, QualityOptions};
use crate::code_quality::CodeQualityService;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

type Result<T, E = String> = std::result::Result<T, E>;

/// The only program names this module ever launches (always with a version flag and nothing else).
const ALLOWED_TOOLS: &[&str] = &["cargo", "rustc", "node", "npm", "pnpm", "yarn", "python", "pip", "java", "mvn", "gradle", "dotnet", "docker", "podman", "wsl", "git", "bun"];
const PACKAGE_MANAGERS: &[&str] = &["npm", "pnpm", "yarn", "bun"];
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_EVIDENCE: usize = 8;
const MAX_SCRIPTS: usize = 40;
const MAX_IMAGES: usize = 8;
const MAX_TEXT: usize = 200;
const MAX_DEPTH: usize = 3;
const MAX_FILES: u64 = 20_000;
const MAX_DOCS: usize = 12;
const MENTIONS: [&str; 6] = ["podman", "docker", "wsl", "winget", "vm.max_map_count", "fs.file-max"];
const SETUP_VIA: &str = "Code quality setup";
const NO_INSTALLER: &str = "No installer in OMP Manager.";
const EXE: &str = if cfg!(windows) { ".exe" } else { "" };

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Category { Runtime, BuildTool, PackageManager, ContainerEngine, OsFeature, SystemSetting, Executable }

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Need { Required, Optional, Unknown }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Environment { Host, Wsl, Container }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DetectedState { Found, Missing, Unknown }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Availability { Automatic, Manual, Unknown }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source { Static, Architect }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SetupLink { CodeQuality }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Detected {
    pub state: DetectedState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Install {
    pub availability: Availability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequirementItem {
    pub id: String,
    pub name: String,
    pub category: Category,
    pub need: Need,
    pub environment: Environment,
    pub evidence: Vec<Evidence>,
    pub detected: Detected,
    pub install: Install,
    pub explanation: String,
    pub source: Source,
    #[serde(default)]
    pub setup_link: Option<SetupLink>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRequirements {
    pub folder: String,
    pub items: Vec<RequirementItem>,
    pub limitations: Vec<String>,
}

/// Outcome of locating one allowlisted tool and asking it for its version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolProbe {
    Found { path: String, version: Option<String> },
    /// Not on `PATH` nor in its standard install folders.
    Missing,
    /// Located (or not checkable) but the version check failed or timed out.
    Unknown { path: Option<String> },
}

/// Passive container/WSL facts from the Code quality prerequisite probe.
pub struct Prereqs {
    pub prerequisites: Vec<Prerequisite>,
    pub options: QualityOptions,
}

/// The only machine access this analysis has. Deliberately read-only: there is no start/install method.
pub trait Host: Sync {
    fn tool(&self, name: &str) -> ToolProbe;
    fn prerequisites(&self) -> Prereqs;
}

pub struct SystemHost {
    tool: Box<dyn Fn(&str) -> ToolProbe + Send + Sync>,
    prerequisites: Box<dyn Fn() -> Prereqs + Send + Sync>,
}

impl SystemHost {
    /// Real tools; prerequisites come from the Code quality service's read-only probe (it never starts an engine).
    pub fn new(service: Arc<CodeQualityService>) -> Self {
        SystemHost {
            tool: Box::new(probe_tool),
            prerequisites: Box::new(move || {
                let runtime = service.inner.lock().file.config.runtime.clone();
                let (prerequisites, options) = (service.probe)(&service.config_root, &runtime);
                Prereqs { prerequisites, options }
            }),
        }
    }
    #[cfg(test)]
    pub(crate) fn with_tool(mut self, tool: impl Fn(&str) -> ToolProbe + Send + Sync + 'static) -> Self {
        self.tool = Box::new(tool);
        self
    }
}

impl Host for SystemHost {
    fn tool(&self, name: &str) -> ToolProbe { (self.tool)(name) }
    fn prerequisites(&self) -> Prereqs { (self.prerequisites)() }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from)
}

fn locate(name: &str) -> Option<PathBuf> {
    if cfg!(windows) {
        match name {
            "docker" => return docker_program(),
            "podman" => return podman_program(),
            "wsl" => return wsl_program(),
            _ => {}
        }
    }
    let files: Vec<String> = if cfg!(windows) { ["exe", "cmd", "bat"].iter().map(|ext| format!("{name}.{ext}")).collect() } else { unix_names(name) };
    let mut dirs: Vec<PathBuf> = Vec::new();
    match name {
        "cargo" | "rustc" => dirs.extend(home_dir().map(|home| home.join(".cargo/bin"))),
        "node" | "npm" | "pnpm" | "yarn" if cfg!(windows) => dirs.push(program_files().join("nodejs")),
        "dotnet" if cfg!(windows) => dirs.push(program_files().join("dotnet")),
        "dotnet" => {
            dirs.extend(home_dir().map(|home| home.join(".dotnet")));
            dirs.extend(["/usr/share/dotnet", "/usr/local/share/dotnet", "/usr/lib/dotnet"].map(PathBuf::from));
        }
        "git" if cfg!(windows) => dirs.push(program_files().join("Git/cmd")),
        "java" => dirs.extend(std::env::var_os("JAVA_HOME").map(|home| PathBuf::from(home).join("bin"))),
        _ => {}
    }
    for file in files {
        let standard: Vec<PathBuf> = dirs.iter().map(|dir| dir.join(&file)).collect();
        if let Some(path) = discover_program(&file, &standard) { return Some(path); }
    }
    None
}

/// Executable names tried on Unix, in order: distributions and Homebrew ship `python3`/`pip3` and often no bare `python`/`pip`.
fn unix_names(name: &str) -> Vec<String> {
    match name {
        "python" => vec!["python3".into(), "python".into()],
        "pip" => vec!["pip3".into(), "pip".into()],
        other => vec![other.into()],
    }
}

fn probe_tool(name: &str) -> ToolProbe {
    if !ALLOWED_TOOLS.contains(&name) { return ToolProbe::Unknown { path: None }; }
    // WSL is a Windows feature: on other platforms its absence says nothing about the project, so it is never "not found".
    if name == "wsl" && !cfg!(windows) { return ToolProbe::Unknown { path: None }; }
    let Some(program) = locate(name) else { return ToolProbe::Missing; };
    let shown = display_folder(&program);
    let mut command = Command::new(&program);
    command.arg(if name == "java" { "-version" } else { "--version" });
    let unknown = ToolProbe::Unknown { path: Some(shown.clone()) };
    let Ok(out) = run_bounded(command, None, VERSION_TIMEOUT, &Ctl::silent()) else { return unknown; };
    if out.timed_out || out.cancelled || !out.status.success() { return unknown; }
    // `java -version` writes to stderr; everything else to stdout.
    let stdout = out.text();
    let text = if stdout.is_empty() { String::from_utf8_lossy(&out.stderr).trim().to_owned() } else { stdout };
    let version = parse_version(&text).or_else(|| text.lines().next().map(|line| clip(line.trim(), 80))).filter(|v| !v.is_empty());
    ToolProbe::Found { path: shown, version }
}

fn parse_version(text: &str) -> Option<String> {
    for token in text.split_whitespace() {
        let token = token.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';' | '(' | ')' | 'v' | 'V'));
        if token.chars().next().is_some_and(|c| c.is_ascii_digit()) && token.contains('.')
            && token.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_')) {
            return Some(clip(token, 40));
        }
    }
    None
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max { return text.to_string(); }
    let mut shortened: String = text.chars().take(max).collect();
    shortened.push('…');
    shortened
}

const CREDENTIAL_WORDS: [&str; 8] = ["token", "secret", "password", "passwd", "apikey", "api_key", "api-key", "authorization"];
/// Evidence lines are repository text; a line that could carry a credential value is replaced, never shown.
fn redact(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    if CREDENTIAL_WORDS.iter().any(|word| lower.contains(word)) && (line.contains('=') || line.contains(':')) {
        "[line omitted: it may contain a credential]".into()
    } else { line.to_string() }
}

fn ev(path: &str, line: Option<usize>, text: &str) -> Evidence {
    Evidence { path: path.into(), line: line.and_then(|l| u32::try_from(l).ok()), text: clip(&redact(text.trim()), MAX_TEXT) }
}
fn find_line<'a>(text: &'a str, needle: &str) -> Option<(usize, &'a str)> {
    text.lines().enumerate().find(|(_, line)| line.contains(needle)).map(|(i, line)| (i + 1, line))
}
fn ev_at(path: &str, text: &str, needle: &str, fallback: &str) -> Evidence {
    match find_line(text, needle) {
        Some((number, line)) => ev(path, Some(number), line),
        None => ev(path, None, fallback),
    }
}
fn first_line(text: &str) -> Option<(usize, &str)> {
    text.lines().enumerate().map(|(i, line)| (i + 1, line.trim())).find(|(_, line)| !line.is_empty())
}
fn dir_of(rel: &str) -> &str { rel.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("") }
fn join(dir: &str, name: &str) -> String { if dir.is_empty() { name.into() } else { format!("{dir}/{name}") } }
fn file_name(rel: &str) -> &str { rel.rsplit('/').next().unwrap_or(rel) }

fn is_secret(rel: &str) -> bool {
    let name = file_name(rel).to_ascii_lowercase();
    (name.starts_with(".env") && !(name.ends_with(".example") || name.ends_with(".sample")))
        || name.ends_with(".pem") || name.ends_with(".key") || name.ends_with(".pfx") || name.ends_with(".p12")
        || name.starts_with("id_") || name == ".npmrc" || name == ".netrc" || name == "credentials" || name == "credentials.json"
}

fn category_key(category: Category) -> &'static str {
    match category {
        Category::Runtime => "runtime",
        Category::BuildTool => "buildTool",
        Category::PackageManager => "packageManager",
        Category::ContainerEngine => "containerEngine",
        Category::OsFeature => "osFeature",
        Category::SystemSetting => "systemSetting",
        Category::Executable => "executable",
    }
}

/// How an item's `detected` state is decided.
#[derive(Clone)]
enum Probe {
    Tool(&'static str),
    Engine,
    Wsl,
    Winget,
    /// The declaring file exists, so the declared thing exists.
    Declared(PathBuf),
    /// Found when any listed file exists; otherwise unknown (not built / not installed there).
    Local(Vec<PathBuf>),
    None,
}

struct Spec { id: String, name: String, category: Category, need: Need, environment: Environment, probe: Probe, explanation: String }
struct Draft { name: String, category: Category, need: Need, environment: Environment, probe: Probe, explanation: String, evidence: Vec<Evidence> }
#[derive(Default)]
struct Drafts { map: BTreeMap<String, Draft> }

impl Drafts {
    /// Same id merges: the stronger need wins, evidence accumulates (bounded), the first explanation stays.
    fn add(&mut self, spec: Spec, evidence: impl IntoIterator<Item = Evidence>) {
        let draft = self.map.entry(spec.id).or_insert_with(|| Draft {
            name: spec.name, category: spec.category, need: spec.need, environment: spec.environment,
            probe: spec.probe, explanation: spec.explanation, evidence: Vec::new(),
        });
        draft.need = draft.need.min(spec.need);
        for item in evidence {
            if draft.evidence.len() < MAX_EVIDENCE && !draft.evidence.contains(&item) { draft.evidence.push(item); }
        }
    }
    fn count_prefix(&self, prefix: &str) -> usize { self.map.keys().filter(|id| id.starts_with(prefix)).count() }
}

fn tool_spec(tool: &'static str, need: Need, explanation: &str) -> Spec {
    let (category, name, probe) = match tool {
        "cargo" => (Category::BuildTool, "Cargo", Probe::Tool("cargo")),
        "rustc" => (Category::Runtime, "Rust compiler (rustc)", Probe::Tool("rustc")),
        "node" => (Category::Runtime, "Node.js", Probe::Tool("node")),
        "npm" => (Category::PackageManager, "npm", Probe::Tool("npm")),
        "pnpm" => (Category::PackageManager, "pnpm", Probe::Tool("pnpm")),
        "yarn" => (Category::PackageManager, "Yarn", Probe::Tool("yarn")),
        "bun" => (Category::PackageManager, "Bun", Probe::Tool("bun")),
        "python" => (Category::Runtime, "Python", Probe::Tool("python")),
        "pip" => (Category::PackageManager, "pip", Probe::Tool("pip")),
        "java" => (Category::Runtime, "Java (JDK)", Probe::Tool("java")),
        "mvn" => (Category::BuildTool, "Apache Maven", Probe::Tool("mvn")),
        "gradle" => (Category::BuildTool, "Gradle", Probe::Tool("gradle")),
        "dotnet" => (Category::Runtime, ".NET SDK", Probe::Tool("dotnet")),
        "docker" => (Category::ContainerEngine, "Docker", Probe::Tool("docker")),
        "podman" => (Category::ContainerEngine, "Podman", Probe::Tool("podman")),
        "wsl" => (Category::OsFeature, "Windows Subsystem for Linux (WSL)", Probe::Wsl),
        "git" => (Category::BuildTool, "Git", Probe::Tool("git")),
        other => (Category::BuildTool, other, Probe::Tool(other)),
    };
    Spec { id: format!("{}:{tool}", category_key(category)), name: name.into(), category, need, environment: Environment::Host, probe, explanation: explanation.into() }
}

fn quality_setting(key: &'static str, minimum: u64) -> Spec {
    Spec {
        id: format!("systemSetting:{key}"), name: format!("{key} >= {minimum}"), category: Category::SystemSetting, need: Need::Optional,
        environment: Environment::Wsl, probe: Probe::None,
        explanation: format!("Required by Code quality analysis, not by the project: the Linux VM that runs the analyzer needs {key} of at least {minimum}. OMP Manager does not read or change it when this view opens; Code quality setup can raise it for its own Podman machine."),
    }
}

fn quality_evidence(key: &str, minimum: u64) -> Evidence {
    Evidence { path: "OMP Manager Code quality requirement".into(), line: None, text: format!("{key}>={minimum}") }
}

fn tauri_spec(repo: &Repo, dir: &str) -> Spec {
    let mut candidates = Vec::new();
    for base in [dir.to_string(), String::new()] {
        for name in ["tauri.cmd", "tauri"] { candidates.push(repo.abs(&join(&join(&base, "node_modules/.bin"), name))); }
    }
    candidates.extend(home_dir().map(|home| home.join(format!(".cargo/bin/cargo-tauri{EXE}"))));
    Spec {
        id: "buildTool:tauri".into(), name: "Tauri CLI".into(), category: Category::BuildTool, need: Need::Required, environment: Environment::Host,
        probe: Probe::Local(candidates),
        explanation: "The Tauri CLI (`cargo tauri` or the @tauri-apps/cli package) builds and runs this Tauri desktop app. Only the project's node_modules and ~/.cargo/bin are checked; it is not run.".into(),
    }
}

/// Read-only view of the repository: ignore-filtered relative file list plus bounded text reads.
struct Repo { root: PathBuf, files: Vec<String>, notes: RefCell<Vec<String>> }

impl Repo {
    fn open(root: &Path) -> Repo {
        let notes = RefCell::new(Vec::new());
        let files: Vec<String> = match list_source_files(root, &[], Some(MAX_DEPTH), MAX_FILES, &Ctl::silent()) {
            Ok(listing) => {
                if !listing.warnings.is_empty() { notes.borrow_mut().push(format!("{} linked file(s) or folder(s) were skipped.", listing.warnings.len())); }
                listing.paths.iter().filter_map(|path| path.strip_prefix(root).ok())
                    .map(|path| path.to_string_lossy().replace('\\', "/")).filter(|rel| !is_secret(rel)).collect()
            }
            Err(error) => { notes.borrow_mut().push(format!("The repository file list could not be read completely: {error}")); Vec::new() }
        };
        Repo { root: root.to_path_buf(), files, notes }
    }
    fn note(&self, text: impl Into<String>) {
        let text = text.into();
        let mut notes = self.notes.borrow_mut();
        if notes.len() < 20 && !notes.contains(&text) { notes.push(text); }
    }
    fn has(&self, rel: &str) -> bool { self.files.iter().any(|file| file.eq_ignore_ascii_case(rel)) }
    fn abs(&self, rel: &str) -> PathBuf { self.root.join(rel) }
    fn read(&self, rel: &str) -> Option<String> {
        match read_config(&self.abs(rel)) {
            Ok(text) => text,
            Err(error) => { self.note(error); None }
        }
    }
    /// Files in the project root or an immediate non-hidden sub-folder (the scope `dependencies::scan` uses).
    fn find(&self, matches: impl Fn(&str) -> bool) -> Vec<String> {
        self.files.iter().filter(|rel| {
            let depth = rel.matches('/').count();
            (depth == 0 || (depth == 1 && !rel.starts_with('.'))) && matches(file_name(rel))
        }).cloned().collect()
    }
}

fn first_tool(command: &str) -> Option<&'static str> {
    let token = command.split_whitespace().find(|t| !t.contains('=') && *t != "cross-env" && !t.starts_with('-'))?;
    let stem = token.trim_matches(|c: char| c == '"' || c == '\'');
    let stem = stem.strip_suffix(".exe").unwrap_or(stem);
    ALLOWED_TOOLS.iter().copied().find(|tool| *tool == stem)
}

fn detect_cargo(repo: &Repo, d: &mut Drafts) {
    for rel in repo.find(|n| n.eq_ignore_ascii_case("Cargo.toml")) {
        let Some(text) = repo.read(&rel) else { continue };
        let dir = dir_of(&rel).to_string();
        let table = match toml::from_str::<toml::Table>(&text) {
            Ok(table) => Some(table),
            Err(error) => {
                let reason = error.to_string();
                repo.note(format!("{rel} is not valid TOML ({}); only its presence was recorded.", reason.lines().next().unwrap_or("parse error")));
                None
            }
        };
        let anchor = ["[package]", "[workspace]", "[dependencies]"].iter().find_map(|header| find_line(&text, header));
        let manifest = match anchor { Some((number, line)) => ev(&rel, Some(number), line), None => ev(&rel, None, "Cargo manifest") };
        d.add(tool_spec("cargo", Need::Required, "Cargo builds, tests and runs this Rust project."), [manifest.clone()]);
        let mut rustc = vec![manifest];
        let mut bins: Vec<(String, Evidence)> = Vec::new();
        let mut tauri = false;
        if let Some(table) = &table {
            let package = table.get("package");
            if let Some(version) = package.and_then(|p| p.get("rust-version")).and_then(|v| v.as_str()) {
                rustc.push(ev_at(&rel, &text, "rust-version", &format!("rust-version = \"{version}\"")));
            }
            for bin in table.get("bin").and_then(|b| b.as_array()).into_iter().flatten() {
                if let Some(name) = bin.get("name").and_then(|n| n.as_str()) {
                    bins.push((name.to_string(), ev_at(&rel, &text, &format!("name = \"{name}\""), "[[bin]] target")));
                }
            }
            let main = join(&dir, "src/main.rs");
            if let Some(name) = package.and_then(|p| p.get("name")).and_then(|n| n.as_str()) {
                if bins.is_empty() && repo.has(&main) { bins.push((name.to_string(), ev(&main, None, "binary entry point"))); }
            }
            tauri = ["dependencies", "build-dependencies"].iter().any(|section| table.get(*section).and_then(|v| v.get("tauri")).is_some());
        }
        d.add(tool_spec("rustc", Need::Required, "The Rust compiler compiles this crate."), rustc);
        if tauri { d.add(tauri_spec(repo, &dir), [ev_at(&rel, &text, "tauri", "tauri dependency")]); }
        for (name, evidence) in bins {
            let exe = format!("{name}{EXE}");
            let mut candidates = Vec::new();
            for base in [join(&dir, "target"), "target".to_string()] {
                for profile in ["debug", "release"] { candidates.push(repo.abs(&format!("{base}/{profile}/{exe}"))); }
            }
            d.add(Spec {
                id: format!("executable:cargo-bin:{rel}:{name}"), name: name.clone(), category: Category::Executable, need: Need::Unknown,
                environment: Environment::Host, probe: Probe::Local(candidates),
                explanation: format!("Binary target `{name}` of {rel}; `cargo build` produces it. OMP Manager does not build or run it."),
            }, [evidence]);
        }
    }
    for rel in repo.find(|n| n == "rust-toolchain" || n == "rust-toolchain.toml") {
        let Some(text) = repo.read(&rel) else { continue };
        let evidence = match find_line(&text, "channel") {
            Some((number, line)) => ev(&rel, Some(number), line),
            None => first_line(&text).map(|(number, line)| ev(&rel, Some(number), line)).unwrap_or_else(|| ev(&rel, None, "toolchain file")),
        };
        d.add(tool_spec("rustc", Need::Required, &format!("{rel} pins the Rust toolchain.")), [evidence]);
    }
    for rel in repo.find(|n| n == "tauri.conf.json") {
        d.add(tauri_spec(repo, dir_of(&rel)), [ev(&rel, None, "Tauri configuration")]);
    }
}

fn npm_bin(repo: &Repo, d: &mut Drafts, rel: &str, dir: &str, text: &str, name: &str, target: &str) {
    d.add(Spec {
        id: format!("executable:npm-bin:{rel}:{name}"), name: name.into(), category: Category::Executable, need: Need::Optional,
        environment: Environment::Host, probe: Probe::Local(vec![repo.abs(&join(dir, target))]),
        explanation: format!("Command `{name}` declared in {rel} (\"bin\"): it runs {target} with Node.js. OMP Manager does not run it."),
    }, [ev_at(rel, text, "\"bin\"", &format!("bin {name} -> {target}"))]);
}

fn detect_node(repo: &Repo, d: &mut Drafts) {
    for rel in repo.find(|n| n == "package.json") {
        let Some(text) = repo.read(&rel) else { continue };
        let json: serde_json::Value = match serde_json::from_str(&text) {
            Ok(json) => json,
            Err(error) => { repo.note(format!("{rel} is not valid JSON ({error}); its requirements were skipped.")); continue; }
        };
        let dir = dir_of(&rel).to_string();
        let engines = json.get("engines");
        let engine = |key: &str| engines.and_then(|e| e.get(key)).and_then(|v| v.as_str()).map(str::to_string);
        match engine("node") {
            Some(range) => d.add(
                tool_spec("node", Need::Required, &format!("{rel} requires Node.js {range}.")),
                [ev_at(&rel, &text, "\"node\"", &format!("engines.node {range}"))]),
            None => d.add(tool_spec("node", Need::Required, "A package.json project runs on Node.js."), [ev(&rel, None, "package.json present")]),
        }
        let mut managers: Vec<(&'static str, Evidence)> = Vec::new();
        if let Some(field) = json.get("packageManager").and_then(|v| v.as_str()) {
            let name = field.split('@').next().unwrap_or("");
            if let Some(manager) = PACKAGE_MANAGERS.iter().copied().find(|m| *m == name) {
                managers.push((manager, ev_at(&rel, &text, "\"packageManager\"", field)));
            }
        }
        let locks: [(&'static str, &[&str]); 4] = [
            ("npm", &["package-lock.json", "npm-shrinkwrap.json"][..]), ("pnpm", &["pnpm-lock.yaml"][..]), ("yarn", &["yarn.lock"][..]), ("bun", &["bun.lockb", "bun.lock"][..]),
        ];
        for (manager, files) in locks {
            for file in files {
                let lock = join(&dir, file);
                if repo.has(&lock) { managers.push((manager, ev(&lock, None, "lockfile present"))); }
            }
        }
        for manager in ["npm", "pnpm", "yarn"] {
            if let Some(range) = engine(manager) { managers.push((manager, ev_at(&rel, &text, &format!("\"{manager}\""), &format!("engines.{manager} {range}")))); }
        }
        let default_manager = managers.first().map(|(manager, _)| *manager).unwrap_or("npm");
        if managers.is_empty() {
            d.add(tool_spec("npm", Need::Optional, "No lockfile or packageManager field was found; npm is Node.js's default package manager."), [ev(&rel, None, "package.json without a lockfile")]);
        }
        for (manager, evidence) in managers {
            d.add(tool_spec(manager, Need::Required, &format!("{manager} installs this project's packages (its lockfile or a package.json field names it).")), [evidence]);
        }
        if ["dependencies", "devDependencies"].iter().any(|section| json.get(*section).and_then(|v| v.get("@tauri-apps/cli")).is_some()) {
            d.add(tauri_spec(repo, &dir), [ev_at(&rel, &text, "@tauri-apps/cli", "@tauri-apps/cli dependency")]);
        }
        match json.get("bin") {
            Some(serde_json::Value::String(target)) => {
                let package = json.get("name").and_then(|n| n.as_str()).unwrap_or("bin");
                npm_bin(repo, d, &rel, &dir, &text, package.rsplit('/').next().unwrap_or(package), target);
            }
            Some(serde_json::Value::Object(map)) => {
                for (name, target) in map {
                    if let Some(target) = target.as_str() { npm_bin(repo, d, &rel, &dir, &text, name, target); }
                }
            }
            _ => {}
        }
        if let Some(scripts) = json.get("scripts").and_then(|s| s.as_object()) {
            if scripts.len() > MAX_SCRIPTS { repo.note(format!("{rel} declares {} scripts; only the first {MAX_SCRIPTS} are listed.", scripts.len())); }
            for (name, command) in scripts.iter().take(MAX_SCRIPTS) {
                let Some(command) = command.as_str() else { continue };
                let line = ev_at(&rel, &text, &format!("\"{name}\""), &format!("{name}: {command}"));
                d.add(Spec {
                    id: format!("executable:npm-script:{rel}:{name}"), name: format!("{default_manager} run {name}"), category: Category::Executable,
                    need: Need::Optional, environment: Environment::Host, probe: Probe::Declared(repo.abs(&rel)),
                    explanation: format!("Script \"{name}\" declared in {rel}. OMP Manager lists scripts; it never runs them."),
                }, [line.clone()]);
                if let Some(tool) = first_tool(command) {
                    d.add(tool_spec(tool, Need::Required, &format!("The \"{name}\" script in {rel} invokes {tool}.")), [line]);
                }
            }
        }
    }
}

fn detect_python(repo: &Repo, d: &mut Drafts) {
    let projects = repo.find(|n| n == "pyproject.toml");
    let requirements = repo.find(|n| { let lower = n.to_ascii_lowercase(); lower.starts_with("requirements") && lower.ends_with(".txt") });
    if projects.is_empty() && requirements.is_empty() { return; }
    let mut python = Vec::new();
    for rel in &projects {
        let Some(text) = repo.read(rel) else { continue };
        python.push(ev_at(rel, &text, "requires-python", "pyproject.toml present"));
    }
    for rel in &requirements { python.push(ev(rel, None, "requirements file")); }
    d.add(tool_spec("python", Need::Required, "Python runs this project's code and tools."), python);
    let (need, why, evidence) = if requirements.is_empty() {
        (Need::Optional, "pyproject.toml may name a different installer; pip is the standard Python package installer.", projects.iter().map(|rel| ev(rel, None, "pyproject.toml")).collect::<Vec<_>>())
    } else {
        (Need::Required, "pip installs the packages listed in the requirements file(s).", requirements.iter().map(|rel| ev(rel, None, "requirements file")).collect::<Vec<_>>())
    };
    d.add(tool_spec("pip", need, why), evidence);
}

fn detect_java(repo: &Repo, d: &mut Drafts) {
    for rel in repo.find(|n| n == "pom.xml") {
        let Some(text) = repo.read(&rel) else { continue };
        let dir = dir_of(&rel).to_string();
        let hint = ["<maven.compiler.release>", "<maven.compiler.source>", "<java.version>", "<release>", "<source>"].iter().find_map(|h| find_line(&text, h));
        let java = match hint { Some((number, line)) => ev(&rel, Some(number), line), None => ev(&rel, None, "Maven project") };
        d.add(tool_spec("java", Need::Required, "Maven compiles this project with a Java JDK."), [java]);
        let wrapper = repo.has(&join(&dir, "mvnw")) || repo.has(&join(&dir, "mvnw.cmd"));
        let (need, why) = if wrapper { (Need::Optional, "The repository ships the Maven wrapper (mvnw), so a system Maven is optional.") } else { (Need::Required, "Maven builds this project.") };
        d.add(tool_spec("mvn", need, why), [ev(&rel, None, "pom.xml")]);
    }
    for rel in repo.find(|n| matches!(n, "build.gradle" | "build.gradle.kts" | "settings.gradle" | "settings.gradle.kts")) {
        let Some(text) = repo.read(&rel) else { continue };
        let dir = dir_of(&rel).to_string();
        let hint = ["sourceCompatibility", "jvmToolchain", "JavaVersion"].iter().find_map(|h| find_line(&text, h));
        let java = match hint { Some((number, line)) => ev(&rel, Some(number), line), None => ev(&rel, None, "Gradle build script") };
        d.add(tool_spec("java", Need::Required, "Gradle compiles this project with a Java JDK."), [java]);
        let wrapper = repo.has(&join(&dir, "gradlew")) || repo.has(&join(&dir, "gradlew.bat"));
        let (need, why) = if wrapper { (Need::Optional, "The repository ships the Gradle wrapper (gradlew), so a system Gradle is optional.") } else { (Need::Required, "Gradle builds this project.") };
        d.add(tool_spec("gradle", need, why), [ev(&rel, None, "Gradle build script")]);
    }
}

fn detect_dotnet(repo: &Repo, d: &mut Drafts) {
    for rel in repo.find(|n| n == "global.json") {
        let Some(text) = repo.read(&rel) else { continue };
        let version = serde_json::from_str::<serde_json::Value>(&text).ok()
            .and_then(|json| json.get("sdk").and_then(|sdk| sdk.get("version")).and_then(|v| v.as_str()).map(str::to_string));
        match version {
            Some(version) => d.add(tool_spec("dotnet", Need::Required, &format!("{rel} pins .NET SDK {version}.")), [ev_at(&rel, &text, "\"version\"", &format!("sdk.version {version}"))]),
            None => d.add(tool_spec("dotnet", Need::Required, &format!("{rel} configures the .NET SDK.")), [ev(&rel, None, "global.json")]),
        }
    }
    for rel in repo.find(|n| { let lower = n.to_ascii_lowercase(); [".sln", ".slnx", ".csproj", ".vbproj", ".fsproj"].iter().any(|ext| lower.ends_with(ext)) }) {
        d.add(tool_spec("dotnet", Need::Required, "A .NET solution or project file needs the .NET SDK to build."), [ev(&rel, None, "project file")]);
    }
}

fn detect_pins(repo: &Repo, d: &mut Drafts) {
    for (name, tool) in [(".nvmrc", "node"), (".node-version", "node"), (".python-version", "python")] {
        for rel in repo.find(|n| n == name) {
            let Some(text) = repo.read(&rel) else { continue };
            let Some((number, line)) = first_line(&text) else { continue };
            d.add(tool_spec(tool, Need::Required, &format!("{rel} pins {tool} {line}.")), [ev(&rel, Some(number), line)]);
        }
    }
    for rel in repo.find(|n| n == ".gitmodules") {
        d.add(tool_spec("git", Need::Required, "Git fetches the submodules declared in .gitmodules."), [ev(&rel, None, "git submodules")]);
    }
}

fn detect_containers(repo: &Repo, d: &mut Drafts) {
    let is_dockerfile = |name: &str| { let lower = name.to_ascii_lowercase(); lower == "dockerfile" || lower.starts_with("dockerfile.") || lower.ends_with(".dockerfile") };
    let files = repo.find(|n| {
        let lower = n.to_ascii_lowercase();
        is_dockerfile(n) || ((lower.starts_with("docker-compose") || lower.starts_with("compose.")) && (lower.ends_with(".yml") || lower.ends_with(".yaml")))
    });
    if files.is_empty() { return; }
    d.add(Spec {
        id: "containerEngine:engine".into(), name: "Docker-compatible container engine".into(), category: Category::ContainerEngine, need: Need::Optional,
        environment: Environment::Host, probe: Probe::Engine,
        explanation: "A Dockerfile or compose file builds or runs container images; Docker Desktop, Podman or Docker in WSL can do that.".into(),
    }, files.iter().map(|rel| ev(rel, None, "container build or compose file")));
    for rel in files.iter().filter(|rel| is_dockerfile(file_name(rel))) {
        let Some(text) = repo.read(rel) else { continue };
        let mut stages: BTreeSet<String> = BTreeSet::new();
        for (index, line) in text.lines().enumerate() {
            let mut words = line.trim().split_whitespace();
            if !words.next().is_some_and(|w| w.eq_ignore_ascii_case("from")) { continue; }
            let rest: Vec<&str> = words.filter(|w| !w.starts_with("--")).collect();
            let Some(image) = rest.first().copied() else { continue };
            let stage = stages.contains(&image.to_ascii_lowercase());
            if let Some(position) = rest.iter().position(|w| w.eq_ignore_ascii_case("as")) {
                if let Some(alias) = rest.get(position + 1) { stages.insert(alias.to_ascii_lowercase()); }
            }
            if stage || image.eq_ignore_ascii_case("scratch") || image.contains('$') || d.count_prefix("runtime:image:") >= MAX_IMAGES { continue; }
            d.add(Spec {
                id: format!("runtime:image:{image}"), name: image.into(), category: Category::Runtime, need: Need::Required, environment: Environment::Container,
                probe: Probe::None,
                explanation: format!("Base image used by {rel}; the container engine pulls it during the build. It is not checked on this computer."),
            }, [ev(rel, Some(index + 1), line)]);
        }
    }
}

fn header_comment(text: &str) -> Option<String> {
    text.lines().take(20).map(str::trim).find_map(|line| {
        if line.starts_with("#!") { return None; }
        let rem = line.get(..4).filter(|prefix| prefix.eq_ignore_ascii_case("rem ")).map(|_| &line[4..]);
        let body = line.strip_prefix("::").or_else(|| line.strip_prefix('#')).or(rem)?.trim();
        if body.is_empty() { None } else { Some(clip(&redact(body), 120)) }
    })
}

fn detect_executables(repo: &Repo, d: &mut Drafts) {
    let environment = |rel: &str| -> Option<Environment> {
        let in_scope = match rel.matches('/').count() { 0 => true, 1 => rel.starts_with("bin/") || rel.starts_with("scripts/"), _ => false };
        if !in_scope { return None; }
        let name = file_name(rel).to_ascii_lowercase();
        match name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("") {
            "sh" => Some(Environment::Wsl),
            "exe" | "bat" | "cmd" | "ps1" => Some(Environment::Host),
            _ if name == "gradlew" || name == "mvnw" => Some(Environment::Host),
            _ => None,
        }
    };
    let executables: Vec<(String, Environment)> = repo.files.iter().filter_map(|rel| environment(rel).map(|env| (rel.clone(), env))).collect();
    if executables.is_empty() { return; }
    let declaring: Vec<(String, String)> = repo.files.iter().filter(|rel| {
        if rel.contains('/') { return false; }
        let lower = rel.to_ascii_lowercase();
        matches!(lower.as_str(), "package.json" | "cargo.toml" | "makefile" | "justfile") || lower.starts_with("readme") || lower.starts_with("dockerfile") || lower.starts_with("docker-compose") || lower.starts_with("compose.")
    }).take(20).filter_map(|rel| repo.read(rel).map(|text| (rel.clone(), text))).collect();
    for (rel, env) in executables {
        let name = file_name(&rel).to_string();
        let reference = declaring.iter().filter(|(declaring_rel, _)| *declaring_rel != rel).find_map(|(declaring_rel, text)| {
            find_line(text, &rel).or_else(|| find_line(text, &name)).map(|(number, line)| (declaring_rel.clone(), number, line.to_string()))
        });
        let header = if name.to_ascii_lowercase().ends_with(".exe") { None } else { repo.read(&rel).and_then(|text| header_comment(&text)) };
        let mut explanation = match &reference {
            Some((declaring_rel, number, _)) => format!("Declared in {declaring_rel}:{number}."),
            None => "Executable file in the repository; no recognized project file references it.".to_string(),
        };
        if let Some(comment) = header { explanation.push_str(&format!(" Header comment: {comment}")); }
        if env == Environment::Wsl { explanation.push_str(" On Windows, run it inside WSL."); }
        let mut evidence = vec![ev(&rel, None, "file present")];
        if let Some((declaring_rel, number, line)) = reference { evidence.push(ev(&declaring_rel, Some(number), &line)); }
        d.add(Spec {
            id: format!("executable:{rel}"), name, category: Category::Executable, need: Need::Unknown, environment: env,
            probe: Probe::Declared(repo.abs(&rel)), explanation,
        }, evidence);
    }
}

fn mentions(line: &str, term: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    let mut start = 0;
    while let Some(position) = lower[start..].find(term) {
        let at = start + position;
        let end = at + term.len();
        if !word(lower[..at].chars().next_back()) && !word(lower[end..].chars().next()) { return true; }
        start = end;
    }
    false
}

fn mention_spec(term: &'static str) -> Spec {
    match term {
        "vm.max_map_count" => quality_setting(term, 524_288),
        "fs.file-max" => quality_setting(term, 131_072),
        "winget" => Spec {
            id: "packageManager:winget".into(), name: "winget (Windows Package Manager)".into(), category: Category::PackageManager, need: Need::Unknown,
            environment: Environment::Host, probe: Probe::Winget,
            explanation: "Project documentation mentions winget; OMP Manager cannot tell whether it is required.".into(),
        },
        _ => tool_spec(term, Need::Unknown, &format!("Project documentation mentions {term}; OMP Manager cannot tell whether it is required.")),
    }
}

fn detect_mentions(repo: &Repo, d: &mut Drafts) {
    let docs: Vec<&String> = repo.files.iter().filter(|rel| {
        let lower = rel.to_ascii_lowercase();
        let (depth, name) = (rel.matches('/').count(), file_name(&lower).to_string());
        let ext = name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
        let text_ext = matches!(ext, "md" | "txt" | "rst" | "");
        (depth == 0 && name.starts_with("readme") && text_ext) || (depth == 1 && lower.starts_with("docs/") && matches!(ext, "md" | "txt" | "rst"))
    }).take(MAX_DOCS).collect();
    let mut hits: BTreeMap<&'static str, Vec<Evidence>> = BTreeMap::new();
    for rel in docs {
        let Some(text) = repo.read(rel) else { continue };
        for (index, line) in text.lines().enumerate() {
            for term in MENTIONS {
                let found = hits.entry(term).or_default();
                if found.len() < 4 && mentions(line, term) { found.push(ev(rel, Some(index + 1), line)); }
            }
        }
    }
    for term in MENTIONS {
        if let Some(evidence) = hits.remove(term).filter(|e| !e.is_empty()) { d.add(mention_spec(term), evidence); }
    }
}

fn install_for(id: &str) -> (Install, Option<SetupLink>) {
    let setup = |note: &str| (Install { availability: Availability::Automatic, via: Some(SETUP_VIA.into()), note: note.into() }, Some(SetupLink::CodeQuality));
    #[cfg(not(windows))]
    {
        // The installers, WSL features and the app-managed Podman machine exist only on Windows; nothing is offered here.
        let manual = |note: &str| (Install { availability: Availability::Manual, via: None, note: note.into() }, None);
        match id {
            "containerEngine:podman" | "containerEngine:docker" | "containerEngine:engine" =>
                return manual("OMP Manager does not install a container engine on this platform; install Podman or Docker with your package manager (for example `nix profile add nixpkgs#podman`)."),
            "osFeature:wsl" => return manual("Windows only; there is nothing to enable on this platform."),
            _ if id.starts_with("systemSetting:") =>
                return manual("A host kernel setting of the container engine's machine; OMP Manager never changes it on this platform. Raise it yourself with `sysctl` if your analyzer needs it."),
            _ => {}
        }
    }
    match id {
        "containerEngine:podman" => setup("Windows only: Code quality setup installs Podman with winget (RedHat.Podman) after you confirm and creates the app's own WSL-backed Podman machine. Windows asks for administrator approval."),
        "containerEngine:docker" => setup("Windows only: Code quality setup can install Docker Desktop with winget (Docker.DockerDesktop; you accept its license interactively) or Docker Engine inside an explicitly chosen Ubuntu/Debian WSL distribution."),
        "containerEngine:engine" => setup("Windows only: Code quality setup can install Podman (winget RedHat.Podman), Docker Desktop (winget Docker.DockerDesktop) or Docker Engine in an explicitly chosen Ubuntu/Debian WSL distribution. Nothing is installed without your confirmation."),
        "osFeature:wsl" => setup("Windows only: Code quality setup can enable the Microsoft-Windows-Subsystem-Linux and VirtualMachinePlatform Windows features after you confirm (administrator approval). Restarting Windows stays manual."),
        _ if id.starts_with("systemSetting:") => setup("Windows only: Code quality setup applies this limit to the app-managed Podman machine (omp-manager-quality). Other machines and engines are never changed."),
        _ if id.starts_with("runtime:image:") => (Install { availability: Availability::Unknown, via: None, note: "Pulled by the container engine during builds; OMP Manager does not install images.".into() }, None),
        _ => (Install { availability: Availability::Manual, via: None, note: NO_INSTALLER.into() }, None),
    }
}

fn unknown_state(path: Option<String>) -> Detected { Detected { state: DetectedState::Unknown, path, version: None } }

fn tool_state(probe: Option<&ToolProbe>) -> Detected {
    match probe {
        Some(ToolProbe::Found { path, version }) => Detected { state: DetectedState::Found, path: Some(path.clone()), version: version.clone() },
        Some(ToolProbe::Missing) => Detected { state: DetectedState::Missing, path: None, version: None },
        Some(ToolProbe::Unknown { path }) => unknown_state(path.clone()),
        None => unknown_state(None),
    }
}

/// `missing` needs both clients absent and no installed prerequisite, and no WSL distro that could hold Docker
/// (probing inside a stopped distro would start it, which this view never does).
fn engine_state(tools: &BTreeMap<&'static str, ToolProbe>, prereq: Option<&Prereqs>) -> Detected {
    for name in ["docker", "podman"] {
        if let Some(ToolProbe::Found { path, version }) = tools.get(name) {
            return Detected { state: DetectedState::Found, path: Some(path.clone()), version: version.clone() };
        }
    }
    let installed = prereq.map(|p| p.prerequisites.iter().any(|entry| entry.installed));
    if installed == Some(true) { return Detected { state: DetectedState::Found, path: None, version: None }; }
    let clients_missing = ["docker", "podman"].iter().all(|name| matches!(tools.get(*name), Some(ToolProbe::Missing)));
    let no_distros = prereq.is_some_and(|p| p.options.wsl_distros.is_empty());
    if clients_missing && installed == Some(false) && no_distros { Detected { state: DetectedState::Missing, path: None, version: None } } else { unknown_state(None) }
}

fn detect(probe: &Probe, tools: &BTreeMap<&'static str, ToolProbe>, prereq: Option<&Prereqs>) -> Detected {
    match probe {
        Probe::Tool(tool) => tool_state(tools.get(*tool)),
        Probe::Declared(path) => Detected { state: DetectedState::Found, path: Some(display_folder(path)), version: None },
        Probe::Local(paths) => paths.iter().find(|path| path.is_file())
            .map(|path| Detected { state: DetectedState::Found, path: Some(display_folder(path)), version: None })
            .unwrap_or_else(|| unknown_state(None)),
        Probe::Engine => engine_state(tools, prereq),
        Probe::Wsl => {
            // Only a positive `wsl --status` report of missing Windows features counts; a failed launch stays unknown.
            if prereq.is_some_and(|p| p.options.wsl_features_missing) { Detected { state: DetectedState::Missing, path: None, version: None } }
            else { tool_state(tools.get("wsl")) }
        }
        Probe::Winget => if prereq.is_some_and(|p| p.options.winget_available) { Detected { state: DetectedState::Found, path: None, version: None } } else { unknown_state(None) },
        Probe::None => unknown_state(None),
    }
}

fn resolve(drafts: Drafts, host: &dyn Host) -> Vec<RequirementItem> {
    let mut needed: BTreeSet<&'static str> = BTreeSet::new();
    let mut need_prereq = false;
    for draft in drafts.map.values() {
        match &draft.probe {
            Probe::Tool(tool) => { needed.insert(*tool); }
            Probe::Engine => { needed.insert("docker"); needed.insert("podman"); need_prereq = true; }
            Probe::Wsl => { needed.insert("wsl"); need_prereq = true; }
            Probe::Winget => need_prereq = true,
            _ => {}
        }
    }
    let mut tools: BTreeMap<&'static str, ToolProbe> = BTreeMap::new();
    let mut prereq: Option<Prereqs> = None;
    std::thread::scope(|scope| {
        let probes: Vec<_> = needed.iter().map(|&name| (name, scope.spawn(move || host.tool(name)))).collect();
        let prerequisites = if need_prereq { Some(scope.spawn(|| host.prerequisites())) } else { None };
        for (name, handle) in probes { tools.insert(name, handle.join().unwrap_or(ToolProbe::Unknown { path: None })); }
        if let Some(handle) = prerequisites { prereq = handle.join().ok(); }
    });
    let mut items: Vec<RequirementItem> = drafts.map.into_iter().map(|(id, draft)| {
        let detected = detect(&draft.probe, &tools, prereq.as_ref());
        let (install, setup_link) = install_for(&id);
        RequirementItem {
            id, name: draft.name, category: draft.category, need: draft.need, environment: draft.environment, evidence: draft.evidence,
            detected, install, explanation: draft.explanation, source: Source::Static, setup_link,
        }
    }).collect();
    items.sort_by(|a, b| a.category.cmp(&b.category).then(a.need.cmp(&b.need))
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())).then_with(|| a.id.cmp(&b.id)));
    items
}

/// Passive analysis of `folder` (the project root and its immediate non-hidden sub-folders).
pub fn analyze(folder: &Path, host: &dyn Host) -> Result<ProjectRequirements> {
    plain_path(folder)?;
    let root = fs::canonicalize(folder).map_err(|e| format!("Cannot resolve project folder: {e}"))?;
    if !root.is_dir() { return Err(format!("Not a folder: {}", folder.display())); }
    let repo = Repo::open(&root);
    let mut drafts = Drafts::default();
    // The app's own Code quality requirement goes first so its label stays on these items.
    drafts.add(quality_setting("vm.max_map_count", 524_288), [quality_evidence("vm.max_map_count", 524_288)]);
    drafts.add(quality_setting("fs.file-max", 131_072), [quality_evidence("fs.file-max", 131_072)]);
    detect_cargo(&repo, &mut drafts);
    detect_node(&repo, &mut drafts);
    detect_python(&repo, &mut drafts);
    detect_java(&repo, &mut drafts);
    detect_dotnet(&repo, &mut drafts);
    detect_pins(&repo, &mut drafts);
    detect_containers(&repo, &mut drafts);
    detect_executables(&repo, &mut drafts);
    detect_mentions(&repo, &mut drafts);
    let items = resolve(drafts, host);
    let mut limitations = vec![
        "Static analysis of manifests, lockfiles, scripts and documentation in the project folder and its immediate sub-folders. No project script, build, installer or container engine is run.".to_string(),
        "Tool versions come from a 5-second `--version` check. Unknown means the check could not complete, not that the tool is missing.".to_string(),
        "Secret-looking files (.env, keys, certificates) are never read.".to_string(),
    ];
    limitations.extend(repo.notes.into_inner());
    Ok(ProjectRequirements { folder: folder.to_string_lossy().into_owned(), items, limitations })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use parking_lot::Mutex;

    struct FakeHost { tools: BTreeMap<String, ToolProbe>, prerequisites: Vec<Prerequisite>, options: QualityOptions, tool_calls: Mutex<Vec<String>>, prerequisite_calls: AtomicUsize }

    impl FakeHost {
        fn new(tools: &[(&str, ToolProbe)]) -> FakeHost {
            FakeHost {
                tools: tools.iter().map(|(name, probe)| (name.to_string(), probe.clone())).collect(), prerequisites: Vec::new(), options: QualityOptions::default(),
                tool_calls: Mutex::new(Vec::new()), prerequisite_calls: AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> Vec<String> { self.tool_calls.lock().clone() }
    }
    impl Host for FakeHost {
        fn tool(&self, name: &str) -> ToolProbe {
            self.tool_calls.lock().push(name.to_string());
            self.tools.get(name).cloned().unwrap_or(ToolProbe::Missing)
        }
        fn prerequisites(&self) -> Prereqs {
            self.prerequisite_calls.fetch_add(1, Ordering::SeqCst);
            Prereqs { prerequisites: self.prerequisites.clone(), options: self.options.clone() }
        }
    }

    fn found(version: &str) -> ToolProbe { ToolProbe::Found { path: "C:/tools/x.exe".into(), version: Some(version.into()) } }
    fn temp() -> PathBuf {
        let folder = std::env::temp_dir().join(format!("omp-req-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&folder).unwrap();
        folder
    }
    fn put(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn item<'a>(result: &'a ProjectRequirements, id: &str) -> &'a RequirementItem {
        result.items.iter().find(|item| item.id == id).unwrap_or_else(|| panic!("missing {id}; have {:?}", result.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>()))
    }
    fn has(result: &ProjectRequirements, id: &str) -> bool { result.items.iter().any(|item| item.id == id) }

    fn app_like_fixture(root: &Path) {
        put(root, "src-tauri/Cargo.toml", "[package]\nname = \"omp-manager\"\nversion = \"0.1.0\"\nrust-version = \"1.80\"\n\n[dependencies]\ntauri = \"2\"\n");
        put(root, "src-tauri/src/main.rs", "fn main() {}\n");
        put(root, "src-tauri/tauri.conf.json", "{}\n");
        put(root, "package.json", "{\n  \"name\": \"omp-manager\",\n  \"engines\": { \"node\": \">=20\" },\n  \"scripts\": { \"dev\": \"vite\", \"build\": \"vite build\", \"native\": \"cargo tauri dev\" }\n}\n");
        put(root, "package-lock.json", "{}\n");
        put(root, "README.md", "# App\nThe analyzer needs a Podman machine on WSL 2.\nRun scripts/run.ps1 to start.\n");
        put(root, "scripts/run.ps1", "# Starts the app in development mode\nWrite-Host hi\n");
        put(root, ".env", "PODMAN_TOKEN=supersecret\n");
        put(root, "deploy.pem", "-----BEGIN-----\n");
    }

    #[test]
    fn app_like_repository_lists_declared_requirements_with_states() {
        let root = temp();
        app_like_fixture(&root);
        let host = FakeHost::new(&[("cargo", found("1.89.0")), ("rustc", found("1.89.0")), ("node", ToolProbe::Unknown { path: Some("C:/node.exe".into()) }), ("npm", found("10.8.0"))]);
        let result = analyze(&root, &host).unwrap();
        assert_eq!(item(&result, "buildTool:cargo").detected.version.as_deref(), Some("1.89.0"));
        assert_eq!(item(&result, "runtime:rustc").need, Need::Required);
        assert!(item(&result, "runtime:rustc").evidence.iter().any(|e| e.path == "src-tauri/Cargo.toml" && e.text.contains("rust-version")));
        assert!(has(&result, "buildTool:tauri"));
        // A failed `--version` is unknown (path kept), never missing; an absent tool is missing.
        let node = item(&result, "runtime:node");
        assert_eq!((node.detected.state, node.detected.path.as_deref()), (DetectedState::Unknown, Some("C:/node.exe")));
        assert!(node.explanation.contains(">=20"));
        assert_eq!(item(&result, "packageManager:npm").evidence[0].path, "package-lock.json");
        assert_eq!(item(&result, "buildTool:cargo").install.availability, Availability::Manual);
        assert_eq!(item(&result, "buildTool:cargo").install.note, NO_INSTALLER);
        let script = item(&result, "executable:npm-script:package.json:dev");
        assert_eq!((script.name.as_str(), script.category, script.detected.state), ("npm run dev", Category::Executable, DetectedState::Found));
        let bin = item(&result, "executable:cargo-bin:src-tauri/Cargo.toml:omp-manager");
        assert_eq!(bin.detected.state, DetectedState::Unknown);
        let ps1 = item(&result, "executable:scripts/run.ps1");
        assert!(ps1.explanation.contains("Declared in README.md:3") && ps1.explanation.contains("Starts the app in development mode"));
        // The `cargo tauri dev` script names cargo as evidence too.
        assert!(item(&result, "buildTool:cargo").evidence.iter().any(|e| e.path == "package.json"));
        // README mentions: need unknown, evidence only, install via Code quality setup.
        let podman = item(&result, "containerEngine:podman");
        assert_eq!(podman.need, Need::Unknown);
        assert!(podman.evidence.iter().any(|e| e.path == "README.md" && e.line == Some(2)));
        let installable = if cfg!(windows) { (Availability::Automatic, Some(SetupLink::CodeQuality)) } else { (Availability::Manual, None) };
        assert_eq!((podman.install.availability, podman.setup_link), installable);
        assert_eq!(podman.detected.state, DetectedState::Missing);
        assert!(has(&result, "osFeature:wsl"));
        assert!(!has(&result, "packageManager:winget"));
        let setting = item(&result, "systemSetting:vm.max_map_count");
        assert!(setting.explanation.starts_with("Required by Code quality analysis, not by the project"));
        assert_eq!((setting.install.availability, setting.setup_link), installable);
        // Secret-looking files are never read or cited.
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("supersecret"));
        assert!(result.items.iter().flat_map(|i| &i.evidence).all(|e| !e.path.starts_with(".env") && e.path != "deploy.pem"));
        // The probe ran once, only for allowlisted names, once each.
        assert_eq!(host.prerequisite_calls.load(Ordering::SeqCst), 1);
        let calls = host.calls();
        assert!(calls.iter().all(|call| ALLOWED_TOOLS.contains(&call.as_str())));
        assert_eq!(calls.len(), calls.iter().collect::<BTreeSet<_>>().len());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn items_are_grouped_in_the_fixed_category_order() {
        let root = temp();
        app_like_fixture(&root);
        let result = analyze(&root, &FakeHost::new(&[])).unwrap();
        let categories: Vec<Category> = result.items.iter().map(|i| i.category).collect();
        let mut sorted = categories.clone();
        sorted.sort();
        assert_eq!(categories, sorted);
        assert_eq!(categories.first(), Some(&Category::Runtime));
        assert_eq!(categories.last(), Some(&Category::Executable));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn no_prerequisite_probe_without_container_or_wsl_items() {
        let root = temp();
        put(&root, "package.json", "{ \"name\": \"x\" }\n");
        let host = FakeHost::new(&[("node", found("22.0.0"))]);
        let result = analyze(&root, &host).unwrap();
        assert_eq!(host.prerequisite_calls.load(Ordering::SeqCst), 0);
        assert_eq!(item(&result, "packageManager:npm").need, Need::Optional);
        assert_eq!(host.calls().iter().map(String::as_str).collect::<BTreeSet<_>>(), BTreeSet::from(["node", "npm"]));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mixed_language_repository_reports_each_ecosystem() {
        let root = temp();
        put(&root, "requirements.txt", "flask==3\n");
        put(&root, "pyproject.toml", "[project]\nrequires-python = \">=3.10\"\n");
        put(&root, "service/pom.xml", "<project>\n<properties><java.version>21</java.version></properties>\n</project>\n");
        put(&root, "service/mvnw", "#!/bin/sh\n");
        put(&root, "global.json", "{ \"sdk\": { \"version\": \"8.0.100\" } }\n");
        put(&root, "Dockerfile", "FROM node:20 AS build\nFROM build AS test\nFROM --platform=linux/amd64 scratch\nFROM $BASE\nFROM python:3.12-slim\n");
        put(&root, ".nvmrc", "20.11.0\n");
        let host = FakeHost::new(&[]);
        let result = analyze(&root, &host).unwrap();
        assert_eq!(item(&result, "runtime:python").evidence.iter().filter(|e| e.text.contains("requires-python")).count(), 1);
        assert_eq!(item(&result, "packageManager:pip").need, Need::Required);
        assert_eq!(item(&result, "runtime:java").evidence[0].path, "service/pom.xml");
        assert_eq!(item(&result, "buildTool:mvn").need, Need::Optional);
        assert!(item(&result, "runtime:dotnet").explanation.contains("8.0.100"));
        assert!(item(&result, "runtime:node").explanation.contains("20.11.0"));
        let image = item(&result, "runtime:image:node:20");
        assert_eq!((image.environment, image.detected.state, image.install.availability), (Environment::Container, DetectedState::Unknown, Availability::Unknown));
        assert!(has(&result, "runtime:image:python:3.12-slim"));
        assert_eq!(result.items.iter().filter(|i| i.id.starts_with("runtime:image:")).count(), 2);
        let engine = item(&result, "containerEngine:engine");
        // No clients, no installed prerequisite, no distros: the only case that may say missing.
        assert_eq!(engine.detected.state, DetectedState::Missing);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn container_engine_is_never_called_missing_when_it_could_be_hidden_in_wsl() {
        let root = temp();
        put(&root, "Dockerfile", "FROM scratch\n");
        let mut host = FakeHost::new(&[]);
        host.options.wsl_distros = vec!["Ubuntu".into()];
        assert_eq!(item(&analyze(&root, &host).unwrap(), "containerEngine:engine").detected.state, DetectedState::Unknown);
        let mut host = FakeHost::new(&[("podman", found("5.0.0"))]);
        host.options.wsl_distros = vec!["Ubuntu".into()];
        let result = analyze(&root, &host).unwrap();
        assert_eq!(item(&result, "containerEngine:engine").detected.state, DetectedState::Found);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wsl_is_missing_only_on_a_positive_features_report() {
        let root = temp();
        put(&root, "README.md", "Needs WSL.\n");
        let mut host = FakeHost::new(&[("wsl", ToolProbe::Unknown { path: None })]);
        assert_eq!(item(&analyze(&root, &host).unwrap(), "osFeature:wsl").detected.state, DetectedState::Unknown);
        host.options.wsl_features_missing = true;
        assert_eq!(item(&analyze(&root, &host).unwrap(), "osFeature:wsl").detected.state, DetectedState::Missing);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wire_shape_is_camel_case_and_round_trips() {
        let root = temp();
        put(&root, "README.md", "Uses winget.\n");
        let result = analyze(&root, &FakeHost::new(&[])).unwrap();
        let json = serde_json::to_value(&result).unwrap();
        let winget = json["items"].as_array().unwrap().iter().find(|i| i["id"] == "packageManager:winget").unwrap();
        assert_eq!(winget["category"], "packageManager");
        assert_eq!(winget["need"], "unknown");
        assert_eq!(winget["source"], "static");
        assert!(winget["setupLink"].is_null());
        assert_eq!(winget["install"]["availability"], "manual");
        assert_eq!(winget["evidence"][0], serde_json::json!({ "path": "README.md", "line": 1, "text": "Uses winget." }));
        let setting = json["items"].as_array().unwrap().iter().find(|i| i["id"] == "systemSetting:fs.file-max").unwrap();
        assert_eq!(setting["setupLink"], "codeQuality");
        assert_eq!(setting["environment"], "wsl");
        assert_eq!(serde_json::from_value::<ProjectRequirements>(json).unwrap(), result);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn credential_looking_lines_are_never_quoted() {
        let root = temp();
        put(&root, "README.md", "Set DOCKER_TOKEN=abc123 before docker login.\nPlain docker mention.\n");
        let result = analyze(&root, &FakeHost::new(&[])).unwrap();
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("abc123"));
        assert!(item(&result, "containerEngine:docker").evidence.iter().any(|e| e.line == Some(2) && e.text == "Plain docker mention."));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mention_matching_uses_word_boundaries() {
        assert!(mentions("Install Docker Desktop", "docker"));
        assert!(mentions("`vm.max_map_count` must be raised", "vm.max_map_count"));
        assert!(!mentions("see Dockerfile", "docker"));
        assert!(!mentions("wslpath", "wsl"));
        assert!(mentions("wsl2 and wsl --status", "wsl"));
    }

    #[test]
    fn versions_are_parsed_from_common_tool_output() {
        assert_eq!(parse_version("cargo 1.89.0 (abc 2025-07-01)").as_deref(), Some("1.89.0"));
        assert_eq!(parse_version("v22.3.0").as_deref(), Some("22.3.0"));
        assert_eq!(parse_version("openjdk version \"21.0.2\" 2024-01-16").as_deref(), Some("21.0.2"));
        assert_eq!(parse_version("WSL version: 2.1.5.0").as_deref(), Some("2.1.5.0"));
        assert_eq!(parse_version("no digits here"), None);
    }

    #[test]
    fn unlisted_programs_are_never_launched() {
        assert_eq!(probe_tool("powershell"), ToolProbe::Unknown { path: None });
        assert_eq!(probe_tool("definitely-not-allowlisted"), ToolProbe::Unknown { path: None });
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_probes_know_python3_and_never_report_wsl_as_missing() {
        assert_eq!(unix_names("python"), ["python3", "python"]);
        assert_eq!(unix_names("pip"), ["pip3", "pip"]);
        assert_eq!(unix_names("node"), ["node"]);
        assert_eq!(probe_tool("wsl"), ToolProbe::Unknown { path: None });
        let (install, link) = install_for("containerEngine:podman");
        assert_eq!((install.availability, link), (Availability::Manual, None));
        assert!(install.note.contains("package manager"));
    }

    #[test]
    fn analysis_cannot_start_engines_or_installers() {
        let source = include_str!("project_requirements.rs");
        let production = source.split("mod tests {").next().unwrap();
        for forbidden in ["ensure_started", "install_runtime", "run_installer", "run_machine", "enable_windows_features", "CliEngine"] {
            assert!(!production.contains(forbidden), "production code must not reference {forbidden}");
        }
    }

    #[test]
    fn invalid_manifests_become_limitations_not_failures() {
        let root = temp();
        put(&root, "package.json", "{ nope");
        put(&root, "Cargo.toml", "[package\n");
        let result = analyze(&root, &FakeHost::new(&[])).unwrap();
        assert!(result.limitations.iter().any(|l| l.contains("package.json is not valid JSON")));
        assert!(result.limitations.iter().any(|l| l.contains("Cargo.toml is not valid TOML")));
        assert!(has(&result, "buildTool:cargo"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_folder_is_an_error() {
        let missing = std::env::temp_dir().join(format!("omp-req-missing-{}", uuid::Uuid::new_v4()));
        assert!(analyze(&missing, &FakeHost::new(&[])).is_err());
    }
}
