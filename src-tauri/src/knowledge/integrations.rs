//! Passive discovery of Obsidian and Cognee and the explicit, narrow ways they are opened.
//!
//! Probing only inspects files: nothing is launched, no CLI or Python is run, no Obsidian or Cognee setting, registry
//! entry or store is written, and no note is copied. Obsidian is opened only through an `obsidian://` URI handed to its
//! own executable; the Cognee UI is started and opened by `cognee_ui` (explicit click only, loopback only).
use super::dto::*;
use super::fsio::{self, err, validate_relative};
use super::{KnowledgeService, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const OBSIDIAN_DOWNLOAD: &str = "https://obsidian.md/download";
const MAX_REGISTRY_BYTES: u64 = 1024 * 1024;
const MAX_REGISTERED_VAULTS: usize = 1000;

#[derive(Clone, Debug)]
pub(crate) enum ExeSetting {
    Auto,
    Fixed(PathBuf),
    Disabled,
}

/// Where discovery looks. [`Environment::from_process`] reads the optional `OMP_KNOWLEDGE_*` overrides used by isolated
/// smoke runs (`OMP_KNOWLEDGE_OBSIDIAN_EXE` may be `none` to simulate an absent Obsidian; `OMP_KNOWLEDGE_COGNEE_NODE`
/// names the Node.js executable for the Cognee UI instead of searching `PATH`).
#[derive(Clone, Debug)]
pub(crate) struct Environment {
    pub obsidian_exe: ExeSetting,
    pub obsidian_registry: Option<PathBuf>,
    pub cognee_home: Option<PathBuf>,
    pub cognee_node: Option<PathBuf>,
    pub protected: Vec<PathBuf>,
}

impl Environment {
    pub(crate) fn from_process() -> Self {
        let variable = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        let obsidian_exe = match variable("OMP_KNOWLEDGE_OBSIDIAN_EXE") {
            Some(value) if value.to_str().is_some_and(|v| v.eq_ignore_ascii_case("none")) => ExeSetting::Disabled,
            Some(value) => ExeSetting::Fixed(PathBuf::from(value)),
            None => ExeSetting::Auto,
        };
        Self {
            obsidian_exe,
            obsidian_registry: variable("OMP_KNOWLEDGE_OBSIDIAN_REGISTRY").map(PathBuf::from),
            cognee_home: variable("OMP_KNOWLEDGE_COGNEE_HOME").map(PathBuf::from),
            cognee_node: variable("OMP_KNOWLEDGE_COGNEE_NODE").map(PathBuf::from),
            protected: Vec::new(),
        }
    }

    /// Nothing is discovered anywhere (tests).
    #[cfg(test)]
    pub(crate) fn isolated() -> Self {
        Self { obsidian_exe: ExeSetting::Disabled, obsidian_registry: None, cognee_home: None, cognee_node: None, protected: Vec::new() }
    }
}

/// Old versions saved a UI address and an installation folder here. Both are ignored when read (unknown fields are
/// skipped by serde) and dropped on the next save: a saved address never authorizes a launch.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct CogneeSettings {
    /// Positive observations survive subsequent inspection errors during this service's lifetime.
    #[serde(skip)]
    pub(super) observed_installation: Option<CogneeEvidence>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CogneeEvidence {
    pub folder: String,
    pub version: Option<String>,
}

#[derive(Debug)]
pub(crate) struct CogneeProbeError {
    pub message: String,
    pub evidence: Option<CogneeEvidence>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct ObsidianSettings {
    pub executable: Option<String>,
    pub declined_projects: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct Integrations {
    pub version: u32,
    pub cognee: CogneeSettings,
    pub obsidian: ObsidianSettings,
    /// Registered external Graphify output folders by project key.
    pub graphify_outputs: std::collections::BTreeMap<String, String>,
}

#[cfg(windows)]
mod version {
    use std::ffi::c_void;
    #[repr(C)]
    pub struct FixedFileInfo {
        pub signature: u32,
        pub struct_version: u32,
        pub file_version_ms: u32,
        pub file_version_ls: u32,
        pub product_version_ms: u32,
        pub product_version_ls: u32,
        pub rest: [u32; 7],
    }
    #[link(name = "version")]
    extern "system" {
        pub fn GetFileVersionInfoSizeW(file: *const u16, handle: *mut u32) -> u32;
        pub fn GetFileVersionInfoW(file: *const u16, handle: u32, length: u32, data: *mut c_void) -> i32;
        pub fn VerQueryValueW(block: *const c_void, sub_block: *const u16, buffer: *mut *mut c_void, length: *mut u32) -> i32;
    }
}

/// The file version resource of an executable (read-only metadata; the program is not run).
#[cfg(windows)]
pub(crate) fn executable_version(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut handle = 0u32;
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call.
    let size = unsafe { version::GetFileVersionInfoSizeW(wide.as_ptr(), &mut handle) };
    if size == 0 || size > 16 * 1024 * 1024 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    // SAFETY: `data` has exactly `size` writable bytes.
    if unsafe { version::GetFileVersionInfoW(wide.as_ptr(), 0, size, data.as_mut_ptr().cast()) } == 0 {
        return None;
    }
    let root: Vec<u16> = "\\".encode_utf16().chain(std::iter::once(0)).collect();
    let mut buffer: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut length = 0u32;
    // SAFETY: `data` holds a version block; on success `buffer` points inside it for `length` bytes.
    if unsafe { version::VerQueryValueW(data.as_ptr().cast(), root.as_ptr(), &mut buffer, &mut length) } == 0 || buffer.is_null() || (length as usize) < std::mem::size_of::<version::FixedFileInfo>() {
        return None;
    }
    // SAFETY: bounds were checked above; the block is not aligned for u32 reads, so read unaligned.
    let info = unsafe { std::ptr::read_unaligned(buffer as *const version::FixedFileInfo) };
    if info.signature != 0xFEEF_04BD {
        return None;
    }
    Some(format!("{}.{}.{}", info.file_version_ms >> 16, info.file_version_ms & 0xFFFF, info.file_version_ls >> 16))
}

#[cfg(target_os = "macos")]
fn executable_version(path: &Path) -> Option<String> {
    let plist = path.parent()?.parent()?.join("Info.plist");
    let text = String::from_utf8(fsio::read_regular_bounded(&plist, 1024 * 1024).ok()?).ok()?;
    let after = &text[text.find("CFBundleShortVersionString")?..];
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")?;
    Some(after[start..start + end].trim().to_string())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn executable_version(_path: &Path) -> Option<String> {
    None
}

fn platform_obsidian_candidates(home: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        if let Some(base) = var("LOCALAPPDATA") {
            found.push(base.join("Programs").join("Obsidian").join("Obsidian.exe"));
        }
        for name in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
            if let Some(base) = var(name) {
                found.push(base.join("Obsidian").join("Obsidian.exe"));
            }
        }
        found.push(PathBuf::from("C:/Program Files/Obsidian/Obsidian.exe"));
    } else if cfg!(target_os = "macos") {
        found.push(PathBuf::from("/Applications/Obsidian.app/Contents/MacOS/Obsidian"));
        found.push(home.join("Applications/Obsidian.app/Contents/MacOS/Obsidian"));
    } else {
        found.push(PathBuf::from("/usr/bin/obsidian"));
        found.push(PathBuf::from("/usr/local/bin/obsidian"));
        found.push(PathBuf::from("/opt/Obsidian/obsidian"));
    }
    let mut seen = BTreeSet::new();
    found.retain(|p| seen.insert(fsio::norm(p)));
    found
}

enum Registry {
    Absent,
    Unreadable(String),
    Vaults(Vec<(String, PathBuf)>),
}

enum Lookup {
    Registered(String),
    NotRegistered,
    Unknown,
}

fn forward(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn launch_detached(program: &Path, argument: &str) -> Result<()> {
    let mut command = Command::new(program);
    command.arg(argument).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    crate::code_diff::no_console(&mut command);
    let child = command.spawn().map_err(|e| err("Could not start Obsidian", e))?;
    #[cfg(unix)]
    {
        let mut child = child;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    #[cfg(not(unix))]
    drop(child);
    Ok(())
}

/// The loopback URL rule of the system opener: credential-free http(s) on a loopback host, no query or fragment.
/// It is the last boundary before the browser is started; nothing user-supplied reaches it any more.
pub(crate) fn validate_cognee_url(text: &str) -> Result<String> {
    let text = text.trim();
    if text.is_empty() || text.len() > 512 || text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("The Cognee UI address is empty or contains whitespace".into());
    }
    let url = reqwest::Url::parse(text).map_err(|_| "That is not a valid address".to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Only http:// or https:// addresses are accepted".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("The address may not contain a user name or password".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("The address may not contain a query or fragment".into());
    }
    match url.host_str() {
        Some("localhost") | Some("127.0.0.1") | Some("[::1]") => Ok(url.to_string()),
        _ => Err("Only loopback addresses are accepted: localhost, 127.0.0.1 or ::1".into()),
    }
}

pub(crate) fn open_loopback_url(url: &str) -> Result<()> {
    let url = validate_cognee_url(url)?;
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler").arg(&url);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(&url);
        command
    };
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(&url);
        command
    };
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    crate::code_diff::no_console(&mut command);
    command.spawn().map(|_| ()).map_err(|e| err("Could not open the Cognee UI", e))
}

/// Only a missing directory is absence. Enumeration and entry errors must remain diagnostic.
fn optional_entries(path: &Path) -> Result<Option<fs::ReadDir>> {
    match fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(err("Cannot inspect Cognee package metadata", error)),
    }
}

fn site_packages_dirs(folder: &Path) -> Result<Vec<PathBuf>> {
    if cfg!(windows) {
        return Ok(vec![folder.join("venv").join("Lib").join("site-packages")]);
    }
    let mut site_packages = Vec::new();
    let Some(entries) = optional_entries(&folder.join("venv").join("lib"))? else { return Ok(site_packages) };
    for entry in entries {
        let entry = entry.map_err(|error| err("Cannot enumerate Cognee Python environments", error))?;
        if entry.file_name().to_str().is_some_and(|name| name.starts_with("python")) {
            site_packages.push(entry.path().join("site-packages"));
        }
    }
    Ok(site_packages)
}

/// The version of one `cognee-<version>.dist-info` entry; `None` for any other entry.
fn dist_info_version(entry: &fs::DirEntry) -> Result<Option<String>> {
    let name = entry.file_name();
    let Some(name) = name.to_str() else { return Ok(None) };
    let Some(version) = name.strip_prefix("cognee-").and_then(|name| name.strip_suffix(".dist-info")) else { return Ok(None) };
    if !version.starts_with(|c: char| c.is_ascii_digit()) || !version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '!' | '_' | '-')) {
        return Ok(None);
    }
    #[cfg(windows)]
    let directory = entry.file_type().map_err(|error| err("Cannot inspect Cognee package metadata entry", error))?.is_dir();
    // Nix virtual environments link each installed dist-info directory to the immutable store.
    #[cfg(unix)]
    let directory = fs::metadata(entry.path()).map_err(|error| err("Cannot inspect Cognee package metadata entry", error))?.is_dir();
    if !directory {
        return Err("Cognee package metadata is not a folder".into());
    }
    Ok(Some(version.to_string()))
}

pub(crate) fn cognee_version(folder: &Path) -> Result<Option<String>> {
    let mut versions = BTreeSet::new();
    for dir in site_packages_dirs(folder)? {
        let Some(entries) = optional_entries(&dir)? else { continue };
        for entry in entries {
            let entry = entry.map_err(|error| err("Cannot enumerate Cognee package metadata", error))?;
            versions.extend(dist_info_version(&entry)?);
        }
    }
    Ok(versions.into_iter().next())
}

/// Passive metadata discovery: a CLI file or a `cognee-<version>.dist-info` folder. Nothing is imported or run.
/// Positive CLI evidence is retained even if package metadata cannot be enumerated.
pub(crate) fn probe_cognee_folder(folder: &Path) -> Result<Option<CogneeEvidence>, CogneeProbeError> {
    let cli = if cfg!(windows) { folder.join("venv").join("Scripts").join("cognee-cli.exe") } else { folder.join("venv").join("bin").join("cognee-cli") };
    let cli_result = match fs::metadata(&cli) {
        Ok(meta) if meta.is_file() => Ok(true),
        Ok(_) => Err("The Cognee CLI path is not a file".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(err("Cannot inspect the Cognee CLI", error)),
    };
    let version_result = cognee_version(folder);
    let evidence = (matches!(&cli_result, Ok(true)) || matches!(&version_result, Ok(Some(_)))).then(|| CogneeEvidence {
        folder: folder.to_string_lossy().into_owned(),
        version: version_result.as_ref().ok().and_then(|version| version.clone()),
    });
    let mut problems = Vec::new();
    if let Err(problem) = cli_result {
        problems.push(problem);
    }
    if let Err(problem) = version_result {
        problems.push(problem);
    }
    if problems.is_empty() {
        Ok(evidence)
    } else {
        Err(CogneeProbeError { message: problems.join("; "), evidence })
    }
}

impl KnowledgeService {
    fn obsidian_executable(&self, configured: Option<&str>) -> std::result::Result<Option<PathBuf>, String> {
        let candidates: Vec<PathBuf> = match &self.env.obsidian_exe {
            ExeSetting::Disabled => Vec::new(),
            ExeSetting::Fixed(path) => vec![path.clone()],
            ExeSetting::Auto => {
                let mut list: Vec<PathBuf> = configured.map(PathBuf::from).into_iter().collect();
                list.extend(platform_obsidian_candidates(&self.home));
                list
            }
        };
        let mut problem = None;
        for candidate in candidates {
            match fs::metadata(&candidate) {
                Ok(meta) if meta.is_file() => return Ok(Some(candidate)),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => problem = Some(format!("{}: {e}", candidate.display())),
            }
        }
        match problem {
            Some(problem) => Err(problem),
            None => Ok(None),
        }
    }

    fn obsidian_registry_file(&self) -> Option<PathBuf> {
        if let Some(path) = &self.env.obsidian_registry {
            return Some(path.clone());
        }
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        if cfg!(windows) {
            var("APPDATA").map(|p| p.join("obsidian").join("obsidian.json"))
        } else if cfg!(target_os = "macos") {
            Some(self.home.join("Library/Application Support/obsidian/obsidian.json"))
        } else {
            Some(self.home.join(".config/obsidian/obsidian.json"))
        }
    }

    /// Reads Obsidian's list of known vaults (read-only; never modified).
    fn read_obsidian_registry(&self) -> Registry {
        let Some(file) = self.obsidian_registry_file() else { return Registry::Absent };
        match fs::symlink_metadata(&file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Registry::Absent,
            Err(e) => return Registry::Unreadable(e.to_string()),
            Ok(_) => {}
        }
        let bytes = match fsio::read_regular_bounded(&file, MAX_REGISTRY_BYTES) {
            Ok(bytes) => bytes,
            Err(e) => return Registry::Unreadable(e),
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return Registry::Unreadable("obsidian.json is not valid JSON".into()) };
        let mut vaults = Vec::new();
        if let Some(map) = value.get("vaults").and_then(|v| v.as_object()) {
            for (id, entry) in map.iter().take(MAX_REGISTERED_VAULTS) {
                if let Some(path) = entry.get("path").and_then(|p| p.as_str()).filter(|p| !p.is_empty()) {
                    vaults.push((id.clone(), PathBuf::from(path)));
                }
            }
        }
        Registry::Vaults(vaults)
    }

    fn lookup_registration(&self, root: &Path) -> Lookup {
        match self.read_obsidian_registry() {
            Registry::Vaults(vaults) => {
                let best = vaults.into_iter().filter(|(_, path)| fsio::same_or_inside(path, root)).max_by_key(|(_, path)| fsio::norm(path).len());
                match best {
                    Some((id, path)) if fsio::same_path(&path, root) => Lookup::Registered(id),
                    Some(_) => Lookup::Unknown,
                    None => Lookup::NotRegistered,
                }
            }
            Registry::Absent | Registry::Unreadable(_) => Lookup::Unknown,
        }
    }

    pub(crate) fn probe_obsidian(&self) -> ObsidianCapability {
        self.probe_obsidian_for(false)
    }

    pub(crate) fn probe_obsidian_for(&self, declined_for_project: bool) -> ObsidianCapability {
        let configured = self.state.lock().integrations.obsidian.executable.clone();
        let registered = || match self.read_obsidian_registry() {
            Registry::Vaults(vaults) => vaults.into_iter().map(|(_, p)| p).filter(|p| p.is_dir()).map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        match self.obsidian_executable(configured.as_deref()) {
            Ok(Some(path)) => {
                let version = executable_version(&path);
                let message = version.is_none().then(|| "Obsidian is installed; its version could not be read".to_string());
                ObsidianCapability { state: ObsidianState::Installed, version, executable: Some(path.to_string_lossy().into_owned()), registered_vaults: registered(), message, declined_for_project }
            }
            Ok(None) => ObsidianCapability { state: ObsidianState::Absent, version: None, executable: None, registered_vaults: Vec::new(), message: Some("Obsidian was not found".into()), declined_for_project },
            Err(problem) => ObsidianCapability { state: ObsidianState::Unreadable, version: None, executable: None, registered_vaults: Vec::new(), message: Some(format!("Obsidian could not be inspected: {problem}")), declined_for_project },
        }
    }

    pub(crate) fn probe_cognee(&self) -> CogneeCapability {
        self.probe_cognee_with(probe_cognee_folder)
    }

    /// The one place the Cognee installation folder comes from: the isolated-run override, otherwise
    /// `%LOCALAPPDATA%/Cognee` on Windows. There is no saved/configured location any more.
    pub(crate) fn cognee_folder(&self) -> Option<PathBuf> {
        self.env.cognee_home.clone().or_else(|| {
            std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()).map(|base| PathBuf::from(base).join("Cognee")).filter(|_| cfg!(windows))
        })
    }

    fn cognee_ui_for(&self, evidence: Option<&CogneeEvidence>, problem: bool) -> CogneeUiCapability {
        match evidence {
            Some(evidence) => super::cognee_ui::capability(Path::new(&evidence.folder), &self.home, self.env.cognee_node.as_deref()),
            None if problem => CogneeUiCapability { state: CogneeUiState::Unreadable, message: Some("Cognee could not be inspected, so its web interface was not looked for.".into()) },
            None => CogneeUiCapability { state: CogneeUiState::MissingFrontend, message: Some("Cognee is not installed.".into()) },
        }
    }

    /// The injectable file probe keeps error handling deterministic in regression tests.
    pub(crate) fn probe_cognee_with(&self, probe: impl FnOnce(&Path) -> Result<Option<CogneeEvidence>, CogneeProbeError>) -> CogneeCapability {
        let (settings, settings_error) = {
            let state = self.state.lock();
            (state.integrations.cognee.clone(), state.integrations_error.clone())
        };
        let folder = self.cognee_folder();
        let mut problems = Vec::new();
        if let Some(error) = settings_error {
            problems.push(format!("Integration settings are unreadable: {error}"));
        }
        let previous = folder.as_deref().and_then(|folder| settings.observed_installation.iter().find(|evidence| fsio::same_path(Path::new(&evidence.folder), folder)).cloned());
        let result = match folder.as_deref() {
            Some(folder) if !folder.is_absolute() => Err(CogneeProbeError { message: "The Cognee installation folder is not absolute".into(), evidence: None }),
            Some(folder) => probe(folder),
            None => Ok(None),
        };
        let found = match result {
            Ok(evidence) => evidence,
            Err(error) => {
                problems.push(error.message);
                error.evidence
            }
        };
        if !problems.is_empty() {
            let evidence = found.map(|mut evidence| {
                if evidence.version.is_none() {
                    evidence.version = previous.as_ref().and_then(|previous| previous.version.clone());
                }
                evidence
            }).or(previous);
            if let Some(evidence) = &evidence {
                self.state.lock().integrations.cognee.observed_installation = Some(evidence.clone());
            }
            let ui = self.cognee_ui_for(evidence.as_ref(), true);
            return CogneeCapability {
                state: CogneeState::Unreadable,
                installed: evidence.as_ref().map(|_| true),
                version: evidence.as_ref().and_then(|evidence| evidence.version.clone()),
                message: Some(format!("Cognee could not be fully inspected: {}. No application was started or changed.", problems.join("; "))),
                installation_folder: folder.map(|folder| folder.to_string_lossy().into_owned()),
                ui,
            };
        }
        match found {
            Some(evidence) => {
                // In-memory only: capability queries never write settings or any Cognee store.
                self.state.lock().integrations.cognee.observed_installation = Some(evidence.clone());
                let ui = self.cognee_ui_for(Some(&evidence), false);
                CogneeCapability { state: CogneeState::Installed, installed: Some(true), version: evidence.version, message: None, installation_folder: Some(evidence.folder), ui }
            }
            None => CogneeCapability {
                state: CogneeState::Absent,
                installed: Some(false),
                version: None,
                message: Some("Cognee was not found".into()),
                installation_folder: folder.map(|folder| folder.to_string_lossy().into_owned()),
                ui: self.cognee_ui_for(None, false),
            },
        }
    }

    /// Records that the human declined Obsidian for this project, so Markdown mode is offered without asking again.
    pub fn record_obsidian_decline(&self, folder: &str) -> Result<()> {
        let repository = self.repository_root(folder)?;
        let mut state = self.state.lock();
        if let Some(error) = &state.integrations_error {
            return Err(format!("Integration settings are unreadable: {error}"));
        }
        let mut settings = state.integrations.clone();
        settings.obsidian.declined_projects.insert(fsio::norm(&repository));
        self.save_integrations(&settings)?;
        state.integrations = settings;
        Ok(())
    }

    /// Saves (or clears) an Obsidian executable to use instead of the platform defaults.
    pub fn configure_obsidian_executable(&self, executable: Option<&str>) -> Result<ObsidianCapability> {
        let executable = match executable.map(str::trim).filter(|e| !e.is_empty()) {
            None => None,
            Some(path) => {
                let candidate = Path::new(path);
                let named = candidate.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.to_lowercase().contains("obsidian"));
                if !candidate.is_absolute() || !candidate.is_file() || !named {
                    return Err("Choose the Obsidian executable (an absolute path to Obsidian.exe)".into());
                }
                Some(path.to_string())
            }
        };
        {
            let mut state = self.state.lock();
            if let Some(error) = &state.integrations_error {
                return Err(format!("Integration settings are unreadable: {error}"));
            }
            let mut settings = state.integrations.clone();
            settings.obsidian.executable = executable;
            self.save_integrations(&settings)?;
            state.integrations = settings;
        }
        Ok(self.probe_obsidian())
    }

    fn obsidian_program(&self) -> Result<PathBuf> {
        let configured = self.state.lock().integrations.obsidian.executable.clone();
        self.obsidian_executable(configured.as_deref())
            .map_err(|problem| format!("Obsidian could not be inspected: {problem}"))?
            .ok_or_else(|| "Obsidian is not installed".to_string())
    }

    /// Opens a vault note (or the vault) in the real Obsidian application, separately from OMP Manager. A vault folder
    /// Obsidian does not know yet is reported (`needs_vault_registration`) instead of being registered for the user.
    pub fn open_knowledge_note(&self, vault_id: &str, path: Option<&str>) -> Result<OpenNoteResult> {
        let vault = self.vault_root(vault_id)?;
        if vault.mode != VaultMode::Obsidian {
            return Err("This vault uses Markdown mode; open it in the in-app documents view".into());
        }
        let note = match path.filter(|p| !p.is_empty()) {
            Some(path) => {
                let components = validate_relative(path)?;
                if !components.last().is_some_and(|n| fsio::is_markdown_name(n)) {
                    return Err("Not a Markdown vault document".into());
                }
                let (_file, absolute) = fsio::open_components(&vault.root, &components)?;
                Some(absolute)
            }
            None => None,
        };
        let program = self.obsidian_program()?;
        let lookup = self.lookup_registration(&vault.root);
        let vault_path = vault.root.to_string_lossy().into_owned();
        let uri = match (&note, &lookup) {
            (Some(note), _) => format!("obsidian://open?path={}", fsio::percent_encode(&forward(note))),
            (None, Lookup::Registered(id)) => format!("obsidian://open?vault={}", fsio::percent_encode(id)),
            (None, _) => format!("obsidian://open?path={}", fsio::percent_encode(&forward(&vault.root))),
        };
        if matches!(lookup, Lookup::NotRegistered) {
            return Ok(OpenNoteResult {
                opened: false,
                uri,
                needs_vault_registration: true,
                vault_path,
                message: Some("Obsidian does not know this folder yet. Register it with Obsidian's vault manager (Open folder as vault), then try again.".into()),
            });
        }
        launch_detached(&program, &uri)?;
        Ok(OpenNoteResult { opened: true, uri, needs_vault_registration: false, vault_path, message: None })
    }

    /// Opens Obsidian's own vault manager (`obsidian://choose-vault`). Obsidian's settings are never edited.
    pub fn open_obsidian_vault_manager(&self) -> Result<()> {
        let program = self.obsidian_program()?;
        launch_detached(&program, "obsidian://choose-vault")
    }

    /// Opens the official Obsidian download page; no installer is downloaded or run.
    pub fn open_obsidian_download(&self) -> Result<()> {
        crate::code_diff::open_url(OBSIDIAN_DOWNLOAD, false)
    }

    /// Opens a credential-free https link with the validated system opener.
    pub fn open_knowledge_link(&self, url: &str) -> Result<()> {
        crate::code_diff::open_url(url, false)
    }
}
