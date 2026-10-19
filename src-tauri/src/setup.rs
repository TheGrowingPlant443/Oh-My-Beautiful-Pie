use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config;
use crate::instance::{CreateInput, Instance, InstanceKind, Origin, Registry};
use crate::agent_markdown::{AgentMarkdownService, MarkdownRoutes};

static MARKDOWN_SERVICE: parking_lot::Mutex<Option<std::sync::Weak<AgentMarkdownService>>> = parking_lot::Mutex::new(None);
/// Hold through instruction loading/admission; Apply's exclusive gate protects the complete recoverable batch.
pub fn instruction_read_admission() -> Result<crate::agent_markdown::ReadAdmission> {
    // Recovery needs the exclusive gate; a thread already inside an admission was recovered when it entered.
    if !crate::agent_markdown::nested() {
        let service=MARKDOWN_SERVICE.lock().as_ref().and_then(std::sync::Weak::upgrade);
        if let Some(service)=service {service.recover()?;}
    }
    crate::agent_markdown::read_admission()
}

pub type Result<T, E = String> = std::result::Result<T, E>;

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum SetupScope {
    Global,
    Manager,
    Architect,
    Repository,
}
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum SetupKind {
    Agents,
    Rules,
    Skill,
    Tool,
    Workflow,
    Plugin,
    Profile,
}
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ScriptLanguage {
    Python,
    Bash,
    Typescript,
}
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq, Debug)]
pub struct SetupItem {
    pub kind: SetupKind,
    #[serde(default)]
    pub name: String,
    pub description: String,
    pub language: Option<ScriptLanguage>,
}
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq, Debug)]
pub struct SetupRequest {
    pub scope: SetupScope,
    pub items: Vec<SetupItem>,
    #[serde(default, rename = "projectInstanceId")]
    pub project_instance_id: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupPaths {
    pub global_omp_folder: PathBuf,
    pub global_agent_folder: PathBuf,
    pub manager_folder: PathBuf,
    pub architect_folder: PathBuf,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupAdmission {
    pub instance: Instance,
    pub prompt_sent: Option<bool>,
    pub error: Option<String>,
}
struct Job {
    request: SetupRequest,
    paths: SetupPaths,
    repository_root: Option<PathBuf>,
    admission: Option<bool>,
    sending: bool,
}
pub struct SetupService {
    root: PathBuf,
    jobs: Mutex<HashMap<String, Job>>,
    markdown: std::sync::OnceLock<Arc<AgentMarkdownService>>,
}

fn slug(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.contains(['/', '\\']) || name.contains("..") {
        return Err("Setup names cannot contain paths".into());
    }
    let name = name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    if name.is_empty()
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
        || !name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    {
        return Err("Use letters, numbers, and single hyphens for a setup name".into());
    }
    let reserved = matches!(name.as_str(), "con" | "prn" | "aux" | "nul")
        || (name.len() == 4
            && (name.starts_with("com") || name.starts_with("lpt"))
            && matches!(name.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        return Err("That name is reserved by Windows".into());
    }
    Ok(name)
}

impl SetupRequest {
    pub fn normalize(mut self) -> Result<Self> {
        if self.items.is_empty() {
            return Err("Describe at least one setup item".into());
        }
        if self.scope == SetupScope::Repository {
            if self.project_instance_id.as_ref().is_none_or(|id| id.trim().is_empty()) {
                return Err("Repository setup requires a project".into());
            }
        } else {
            self.project_instance_id = None;
        }
        if self.scope != SetupScope::Global && self.items.iter().any(|i| i.kind == SetupKind::Profile) {
            return Err("OMP profiles are global; choose the Global scope".into());
        }
        let mut targets = HashSet::new();
        for item in &mut self.items {
            item.description = item.description.trim().into();
            if item.description.is_empty() {
                return Err("Every selected item needs a description".into());
            }
            match item.kind {
                SetupKind::Rules => item.name.clear(),
                SetupKind::Agents if item.name.trim().is_empty() => item.name.clear(),
                _ => {
                    item.name = slug(&item.name)?;
                    if item.kind == SetupKind::Profile
                        && (item.name == "default" || !valid_profile_name(&item.name))
                    {
                        return Err("Use a valid OMP profile name other than default".into());
                    }
                }
            }
            if item.kind == SetupKind::Tool && item.language.is_none() {
                return Err("Select Python, Bash, or TypeScript for each tool".into());
            }
            if item.kind != SetupKind::Tool {
                item.language = None;
            }
            if !targets.insert(format!("{:?}:{}", item.kind, item.name)) {
                return Err("Setup items must have distinct target paths".into());
            }
        }
        Ok(self)
    }
}

pub fn materialize(root: &Path) -> Result<()> {
    let source = include_str!("../../bridge/coordinator-setup.ts");
    for role in ["manager", "architect"] {
        let folder = root.join(format!("{role}-setup"));
        fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        let file = folder.join("index.ts");
        if fs::read_to_string(&file).ok().as_deref() != Some(source) {
            fs::write(file, source).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
static GLOBAL_FOLDERS: std::sync::OnceLock<(PathBuf, PathBuf)> = std::sync::OnceLock::new();
pub fn paths(registry: &Registry) -> Result<SetupPaths> {
    let (global_omp_folder, global_agent_folder) = match GLOBAL_FOLDERS.get() {
        Some(folders) => folders.clone(),
        None => {
            let native = config::native_request(
                registry.config_root(),
                &registry.home,
                json!({"action":"paths","cwd":registry.home,"profile":"default"}),
            )?;
            let omp = native["globalOmpFolder"]
                .as_str()
                .ok_or("OMP global folder unavailable")?;
            let agent = native["globalAgentFolder"]
                .as_str()
                .ok_or("OMP agent folder unavailable")?;
            let folders = (PathBuf::from(omp), PathBuf::from(agent));
            let _ = GLOBAL_FOLDERS.set(folders.clone());
            folders
        }
    };
    Ok(SetupPaths {
        global_omp_folder,
        global_agent_folder,
        manager_folder: registry.config_root().join("manager-setup"),
        architect_folder: registry.config_root().join("architect-setup"),
    })
}

pub const AGENT_FILES_DIR: &str = "agent-instructions";
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AgentFile {
    pub name: String,
    pub scope: SetupScope,
    pub path: PathBuf,
}
#[derive(Serialize)]
pub struct AgentFiles {
    pub files: Vec<AgentFile>,
    pub error: Option<String>,
}
fn scan_roots(roots: &[(SetupScope, &Path)]) -> Vec<AgentFile> {
    let mut found = Vec::new();
    for (scope, root) in roots {
        let Ok(entries) = fs::read_dir(root.join(AGENT_FILES_DIR)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path().join("AGENTS.md");
            if entry.path().is_dir() && path.is_file() {
                found.push(AgentFile {
                    name: entry.file_name().to_string_lossy().into(),
                    scope: *scope,
                    path,
                });
            }
        }
    }
    found.sort_by_key(|f| (f.name.clone(), f.scope != SetupScope::Global));
    found
}
pub fn scan_agent_files(paths: &SetupPaths, kind: InstanceKind) -> Vec<AgentFile> {
    let mut roots = vec![
        (SetupScope::Global, paths.global_agent_folder.as_path()),
        (SetupScope::Manager, paths.manager_folder.as_path()),
    ];
    if kind == InstanceKind::Architect {
        roots.push((SetupScope::Architect, paths.architect_folder.as_path()));
    }
    scan_roots(&roots)
}
pub fn agent_files(registry: &Registry, kind: InstanceKind) -> AgentFiles {
    let _admission=match instruction_read_admission() {Ok(guard)=>guard,Err(error)=>return AgentFiles{files:Vec::new(),error:Some(error)}};
    match paths(registry) {
        Ok(paths) => AgentFiles {
            files: scan_agent_files(&paths, kind),
            error: None,
        },
        Err(error) => {
            let manager = registry.config_root().join("manager-setup");
            let architect = registry.config_root().join("architect-setup");
            let mut roots = vec![(SetupScope::Manager, manager.as_path())];
            if kind == InstanceKind::Architect {
                roots.push((SetupScope::Architect, architect.as_path()));
            }
            AgentFiles { files: scan_roots(&roots), error: Some(error) }
        }
    }
}
pub(crate) fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    let path = fs::canonicalize(path)?;
    let text = path.to_string_lossy();
    Ok(PathBuf::from(if let Some(unc) = text.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{unc}")
    } else {
        text.strip_prefix("\\\\?\\").unwrap_or(&text).to_owned()
    }))
}
pub fn verify_agent_file_in(files: &[AgentFile], path: &Path) -> Result<PathBuf> {
    let wanted = canonical(path)
        .map_err(|_| format!("Agent instructions file not found: {}", path.display()))?;
    if files
        .iter()
        .any(|f| canonical(&f.path).is_ok_and(|c| c == wanted))
    {
        Ok(wanted)
    } else {
        Err("Agent instructions file must be one created by Agent setup".into())
    }
}
pub fn verify_agent_file(registry: &Registry, path: &Path, kind: InstanceKind) -> Result<PathBuf> {
    let found = agent_files(registry, kind);
    let warning = found.error;
    verify_agent_file_in(&found.files, path).map_err(|e| match warning {
        Some(w) => format!("{e} ({w})"),
        None => e,
    })
}

/// A file native `--extension <package>` discovery loads from a package root: a direct `.ts`/`.js` child,
/// or `<child>/index.{ts,js}`.
fn discoverable_module(package: &Path, module: &Path) -> bool {
    let is_script = |file: &Path| file.extension().is_some_and(|e| e == "ts" || e == "js");
    let parent = module.parent();
    (parent == Some(package) && is_script(module))
        || (parent.and_then(Path::parent) == Some(package)
            && module.file_stem().is_some_and(|s| s == "index")
            && is_script(module))
}

/// Validated role-private packages loaded through native directory discovery.
pub fn private_plugin_paths(root: &Path, role: InstanceKind) -> Result<Vec<PathBuf>> {
    let role = match role {
        InstanceKind::Manager => "Manager",
        InstanceKind::Architect => "Architect",
        _ => return Err("Private plugins require a coordinator role".into()),
    };
    let folder = root.join("extensions");
    let entries = match fs::read_dir(&folder) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{role} plugins folder is unreadable: {e}")),
    };
    let base = canonical(&folder).map_err(|e| format!("{role} plugins folder is unreadable: {e}"))?;
    let mut names = Vec::new();
    for entry in entries {
        let name = entry.map_err(|e| e.to_string())?.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') {
            names.push(name);
        }
    }
    names.sort();
    let mut packages = Vec::new();
    for name in names {
        let package = canonical(&folder.join(&name)).map_err(|e| format!("{role} plugin {name} is unreadable: {e}"))?;
        if !package.starts_with(&base) {
            return Err(format!("{role} plugin {name} resolves outside the extensions folder"));
        }
        let manifest = package.join("package.json");
        if !package.is_dir() || !manifest.is_file() {
            continue;
        }
        let text = fs::read_to_string(&manifest).map_err(|e| format!("{role} plugin {name}: package.json is unreadable: {e}"))?;
        let json: Value = serde_json::from_str(&text).map_err(|e| format!("{role} plugin {name}: package.json is invalid: {e}"))?;
        let declared = ["omp", "pi"]
            .iter()
            .find_map(|key| json.get(*key).filter(|v| v.is_object()))
            .and_then(|manifest| manifest.get("extensions"))
            .and_then(Value::as_array);
        for entry in declared.into_iter().flatten().filter_map(Value::as_str) {
            let resolved = canonical(&package.join(entry))
                .map_err(|_| format!("{role} plugin {name} declares a missing extension module: {entry}"))?;
            if !resolved.starts_with(&package) {
                return Err(format!("{role} plugin {name} declares a module outside its package: {entry}"));
            }
            let module = if resolved.is_dir() {
                ["index.ts", "index.js"]
                    .iter()
                    .map(|file| resolved.join(file))
                    .find(|file| file.is_file())
                    .ok_or_else(|| format!("{role} plugin {name} declares {entry} without an index module"))?
            } else {
                resolved
            };
            if !discoverable_module(&package, &module) {
                return Err(format!(
                    "{role} plugin {name} declares {entry}, which The {role} cannot load; use index.ts at the package root"
                ));
            }
        }
        packages.push(package);
    }
    Ok(packages)
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OmpProfile {
    pub name: String,
    pub has_login: bool,
}
#[derive(Serialize)]
pub struct OmpProfiles {
    pub profiles: Vec<OmpProfile>,
    pub error: Option<String>,
}
pub fn valid_profile_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 || name.ends_with('.') {
        return false;
    }
    if !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()) {
        return false;
    }
    if !bytes
        .iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-'))
    {
        return false;
    }
    let stem = name.split('.').next().unwrap_or(name);
    let reserved = matches!(stem, "con" | "prn" | "aux" | "nul")
        || (stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.as_bytes()[3].is_ascii_digit());
    !reserved
}
pub fn scan_profiles(global_omp_folder: &Path) -> Vec<OmpProfile> {
    let Ok(entries) = fs::read_dir(global_omp_folder.join("profiles")) else {
        return Vec::new();
    };
    let mut found: Vec<OmpProfile> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            let agent = entry.path().join("agent");
            if name == "default" || !valid_profile_name(&name) || !entry.path().is_dir() || !agent.is_dir() {
                return None;
            }
            Some(OmpProfile {
                has_login: agent.join("agent.db").is_file(),
                name,
            })
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}
pub fn omp_profiles(registry: &Registry) -> OmpProfiles {
    match paths(registry) {
        Ok(paths) => OmpProfiles {
            profiles: scan_profiles(&paths.global_omp_folder),
            error: None,
        },
        Err(error) => OmpProfiles {
            profiles: Vec::new(),
            error: Some(error),
        },
    }
}

pub(crate) fn find_repository_root(cwd: &Path) -> Result<PathBuf> {
    let cwd = canonical(cwd)
        .map_err(|_| format!("Project working directory not found: {}", cwd.display()))?;
    if !cwd.is_dir() {
        return Err(format!("Project working directory not found: {}", cwd.display()));
    }
    for ancestor in cwd.ancestors() {
        let marker = ancestor.join(".git");
        match fs::metadata(&marker) {
            Ok(meta) if meta.is_dir() || meta.is_file() => return Ok(ancestor.to_path_buf()),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("Cannot inspect repository marker {}: {error}", marker.display())),
        }
    }
    Ok(cwd)
}
fn repository_folder(root: &Path) -> Result<PathBuf> {
    let root = canonical(root).map_err(|error| format!("Repository is unavailable: {error}"))?;
    if !root.is_dir() {
        return Err(format!("Repository is not a directory: {}", root.display()));
    }
    let folder = root.join(".omp");
    match fs::symlink_metadata(&folder) {
        Ok(_) => {
            let resolved = canonical(&folder).map_err(|error| format!("Repository setup folder is unavailable: {error}"))?;
            if !resolved.is_dir() {
                return Err(format!("Repository setup path is not a directory: {}", folder.display()));
            }
            if !resolved.starts_with(&root) {
                return Err("Repository setup folder must stay inside the repository".into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Repository setup folder is unavailable: {error}")),
    }
    Ok(folder)
}
pub fn repository_root(registry: &Registry, instance_id: &str) -> Result<PathBuf> {
    let row = registry.inspect(instance_id, Origin::Human)?;
    if row.kind != InstanceKind::Instance {
        return Err("Repository setup requires an ordinary project instance".into());
    }
    let root = find_repository_root(Path::new(&row.project_path))?;
    if canonical(&registry.home).is_ok_and(|home| home == root) {
        return Err("Repository setup cannot target the home directory".into());
    }
    repository_folder(&root)?;
    Ok(root)
}

pub fn open_folder(registry: &Registry, scope: SetupScope, project_instance_id: Option<&str>) -> Result<()> {
    let folder = match scope {
        SetupScope::Global => paths(registry)?.global_omp_folder,
        SetupScope::Manager => registry.config_root().join("manager-setup"),
        SetupScope::Architect => registry.config_root().join("architect-setup"),
        SetupScope::Repository => repository_root(registry, project_instance_id.ok_or("Repository setup requires a project")?)?,
    };
    open_editor_folder(&folder)
}

pub(crate) fn open_editor_folder(folder: &Path) -> Result<()> {
    if !folder.is_dir() {
        return Err(format!(
            "OMP setup folder does not exist: {}. Initialize OMP first.",
            folder.display()
        ));
    }
    #[cfg(windows)]
    let executable = {
        let mut candidates = vec![];
        if let Some(paths) = std::env::var_os("PATH") {
            for directory in std::env::split_paths(&paths) {
                if directory.join("code.cmd").is_file() || directory.join("code").is_file() {
                    if let Some(parent) = directory.parent() {
                        candidates.push(parent.join("Code.exe"));
                    }
                }
            }
        }
        if let Some(root) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(PathBuf::from(root).join("Programs/Microsoft VS Code/Code.exe"));
        }
        if let Some(root) = std::env::var_os("ProgramFiles") {
            candidates.push(PathBuf::from(root).join("Microsoft VS Code/Code.exe"));
        }
        candidates
            .into_iter()
            .find(|path| path.is_file())
            .ok_or("VS Code not found. Install VS Code or add its bin directory to PATH.")?
    };
    #[cfg(not(windows))]
    let executable = PathBuf::from("code");
    let mut command = std::process::Command::new(executable);
    command
        .arg("--new-window")
        .arg(folder)
        .env_remove("ELECTRON_RUN_AS_NODE")
        .env_remove("VSCODE_DEV")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().map_err(|e| {
        format!("Open VS Code: {e}. Install VS Code or add its bin directory to PATH.")
    })?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
fn prompt(request: &SetupRequest, paths: &SetupPaths, repository_root: Option<&Path>) -> Result<String> {
    prompt_with_routes(request,paths,repository_root,&MarkdownRoutes::default())
}
fn prompt_with_routes(request: &SetupRequest, paths: &SetupPaths, repository_root: Option<&Path>, routes: &MarkdownRoutes) -> Result<String> {
    let repository_target;
    let target = match request.scope {
        SetupScope::Global => &paths.global_agent_folder,
        SetupScope::Manager => &paths.manager_folder,
        SetupScope::Architect => &paths.architect_folder,
        SetupScope::Repository => {
            repository_target = repository_folder(repository_root.ok_or("Repository setup requires a project")?)?;
            &repository_target
        }
    };
    let markdown_source=routes.scope.as_ref().map(|r|r.source_root.as_path());
    let mut text = format!("Create the user-requested OMP Agent setup below in {}. Scope: {:?}. Platform: {}.\n\nREVIEW FIRST: You are in native plan mode. Read existing assets, preserve unrelated content, and write only your native plan until the human selects a native execution option. Propose the complete plan using write to xd://propose. Do this setup inline; do not use subagents or other models to generate it. The workflows being authored may describe future native delegation, but do not execute those workflows now.\n\nAfter human approval, implement real files and validate their native loading and behavior. Never modify config.yml/config.yaml, auth or credential databases, app default snapshots, bridge/index.ts entrypoints, installed OMP, or unrelated files. Never download or install dependencies/interpreters, add telemetry/updaters/autostart, or run unreviewed external code. Read existing targets before proposing changes. Report actual changed paths and verification; do not claim completion from merely writing files.\n\n", target.display(), request.scope, std::env::consts::OS);
    for item in &request.items {
        let source_file=match item.kind {
            SetupKind::Agents=>Some(if item.name.is_empty(){"AGENTS.md".into()}else{format!("{AGENT_FILES_DIR}/{}/AGENTS.md",item.name)}),
            SetupKind::Rules=>Some("RULES.md".into()),
            SetupKind::Skill=>Some(format!("skills/{}/SKILL.md",item.name)),
            SetupKind::Workflow=>Some(format!("rules/workflow-{}.md",item.name)),
            _=>None,
        };
        let file = match item.kind {
            SetupKind::Agents if item.name.is_empty() => "AGENTS.md".into(),
            SetupKind::Agents => format!("{AGENT_FILES_DIR}/{}/AGENTS.md", item.name),
            SetupKind::Rules => "RULES.md".into(),
            SetupKind::Skill => format!("skills/{}/SKILL.md and SKILLS.md", item.name),
            SetupKind::Tool => format!("tools/{}/index.ts", item.name),
            SetupKind::Workflow => format!("rules/workflow-{}.md", item.name),
            SetupKind::Plugin => format!("extensions/{0}/package.json and extensions/{0}/index.ts", item.name),
            SetupKind::Profile => paths
                .global_omp_folder
                .join("profiles")
                .join(&item.name)
                .join("agent")
                .join("config.yml")
                .display()
                .to_string(),
        };
        let file = if request.scope == SetupScope::Repository {
            match item.kind {
                SetupKind::Skill => format!("{} and {}", target.join(format!("skills/{}/SKILL.md", item.name)).display(), target.join("SKILLS.md").display()),
                SetupKind::Plugin => format!("{} and {}", target.join(format!("extensions/{}/package.json", item.name)).display(), target.join(format!("extensions/{}/index.ts", item.name)).display()),
                SetupKind::Tool => {
                    let mut file = target.join(&file).display().to_string();
                    let script = match item.language {
                        Some(ScriptLanguage::Python) => Some("run.py"),
                        Some(ScriptLanguage::Bash) => Some("run.sh"),
                        _ => None,
                    };
                    if let Some(script) = script {
                        file.push_str(&format!(" and {}", target.join("tools").join(&item.name).join(script).display()));
                    }
                    file
                }
                _ => target.join(&file).display().to_string(),
            }
        } else { file };
        let file=if let (Some(root),Some(relative))=(markdown_source,source_file.as_deref()) {
            let mut file=root.join(relative).display().to_string();
            if item.kind==SetupKind::Skill {file.push_str(&format!(" and {}",root.join("SKILLS.md").display()));}
            file
        }else{file};
        text.push_str(&format!(
            "Requested {:?}: {file}\nDescription:\n{}\n",
            item.kind, item.description
        ));
        if let (Some(root),true)=(markdown_source,matches!(item.kind,SetupKind::Agents|SetupKind::Rules|SetupKind::Skill|SetupKind::Workflow)) {
            text.push_str(&format!("Markdown source folder: {}. OMP runtime folder: {}. Author requested .md assets ONLY at the Markdown source folder, at their unchanged runtime-relative paths. Never update their native deployed copies from this setup session. They become active only after the human reviews Apply to OMP. Keep tools, every companion script, plugin modules/manifests, configuration and credentials native; never copy them into the vault. Preserve script-relative references in skill Markdown: the deployed Markdown will have its original runtime-relative location. Report native-only dependencies, do not relocate them. Optional tool/plugin documentation uses the corresponding tools/{}/ or extensions/{}/ Markdown namespace here; executable content stays native.\n",root.display(),target.display(),item.name,item.name));
        }
        if let Some(profile)=routes.profiles.get(&item.name).filter(|_|item.kind==SetupKind::Profile) {
            text.push_str(&format!("For this explicitly enabled profile, requested Markdown only goes to {} using original profile-runtime-relative paths, not {}. Its config.yml and all executable/tools/plugin files stay native. Only create profile Markdown actually requested in the description. Never copy credentials. Reviewed Apply to OMP is required before Markdown becomes active.\n",profile.source_root.display(),profile.native_root.display()));
        }
        match item.kind {
            SetupKind::Agents if item.name.is_empty() => text.push_str("Surgically merge requested instructions; preserve existing unrelated policy. RULES.md is sticky.\n"),
            SetupKind::Rules => text.push_str("Surgically merge requested instructions; preserve existing unrelated policy. RULES.md is sticky.\n"),
            SetupKind::Agents if request.scope == SetupScope::Repository => text.push_str("Create a standalone named instruction file, not merged into AGENTS.md. It is not automatically loaded and is not in this GUI's Persona catalog. Use it through an explicit --append-system-prompt file argument. No frontmatter is required.\n"),
            SetupKind::Agents => text.push_str("Create a standalone, named instruction file with plain Markdown instructions for an agent. Do not merge it into the scope's AGENTS.md. OMP does not load it automatically: the user selects it per dashboard instance, and OMP appends it to that session's system prompt. No frontmatter is required.\n"),
            SetupKind::Profile => text.push_str(&format!("Create the native OMP profile `{name}` at {root}/profiles/{name}/agent/. This overrides the general rule against modifying config.yml: write that profile's config.yml with native OMP settings that implement the description (for example plan.enabled: true and plan.defaultOnStartup: true start sessions in plan mode; modelRoles select models). Validate that it loads with native Settings.loadReadOnly using that agent directory. Add AGENTS.md, rules, skills or tools inside that profile only if the description asks. Never create or copy credentials or agent.db and never modify the default profile; tell the human the profile has its own login (run /login inside it). If the profile already exists read it first and make only surgical changes.\n", name = item.name, root = paths.global_omp_folder.display())),
            SetupKind::Skill if markdown_source.is_some() => text.push_str("Create SKILL.md at its requested Markdown source path with YAML name/description and concrete reusable steps. Update SKILLS.md in the same Markdown source root as a readable catalog pointing to the real deployed skill, preserving other entries. The catalog itself is not a native skill. Companion scripts stay in the OMP runtime folder; keep their original references unchanged and do not write them to the Markdown source folder.\n"),
            SetupKind::Skill => text.push_str("Create native SKILL.md with YAML name/description and concrete reusable steps. Update SKILLS.md as a readable catalog pointing to the real skill, preserving other entries. The catalog itself is not a native skill.\n"),
            SetupKind::Workflow => text.push_str("Use native rule frontmatter description, alwaysApply: true, agents: [main]. Describe semantic trigger, native delegation/reviewer steps, required evidence, completion/decision handling, and stop conditions. Use installed native agents; if a required reviewer is unavailable, request human guidance. No scheduler or automatic hooks.\n"),
            SetupKind::Plugin => text.push_str(&format!("Create the native OMP plugin package `{name}` in the `extensions/{name}/` folder of the target above. package.json must be {{\"name\":\"{name}\",\"version\":\"0.1.0\",\"private\":true,\"type\":\"module\",\"description\":<one line from the request>,\"omp\":{{\"extensions\":[\"index.ts\"]}}}}; index.ts must export a default factory that receives the injected native ExtensionAPI and registers real behavior for the description (api.registerTool, api.registerCommand, api.on hooks as needed). Read the installed OMP's examples/extensions and its ExtensionAPI types first. ExtensionAPI registered tools use execute(toolCallId, params, signal, onUpdate, ctx), unlike tools/ native CustomTool factories. This overrides the general rule against editing bridge/index.ts entrypoints only for this plugin's own package.json and index.ts; never touch either coordinator setup root index.ts. Add package-local skills/ tools/ or rules/ folders only when the description needs them. No import-time side effects, no placeholder or no-op tools, no dependency installation or publishing, no `omp plugin link`/install, and do not edit the installed OMP. If the package already exists read it first and change only the requested behavior; never overwrite a different package. After approval validate that the package loads natively and exercise one harmless behavior with a fixture, then tell the human that running sessions need a native reload or an explicit reopen to load it.\n", name = item.name)),
            SetupKind::Tool => {
                text.push_str(&format!("Language: {:?}. Native default factory in index.ts uses injected api.zod, approval: 'exec', execute(toolCallId, params, onUpdate, ctx, signal). Validate arguments; return native content/details with true stdout/stderr/error/abort state. No import-time execution. Python/Bash code lives beside it as run.py/run.sh, invoked via api.exec(interpreter, argumentArray, {{cwd:ctx.sessionManager.getCwd(),signal}}). Missing interpreter must fail clearly, never install it. On Windows use installed Git Bash rather than System32/WindowsApps WSL launchers. TypeScript logic stays in the native tool module. Inspect local interpreter locations before selecting one. Validate using a harmless fixture operation after approval.\n", item.language));
            }
        }
        text.push('\n');
    }
    if request.scope == SetupScope::Repository {
        text.push_str("After native plan approval, write only the requested assets under this repository's .omp folder. Read existing files first and preserve unrelated content. Validate native discovery with cwd equal to the repository root. Never modify Global/Manager/Architect setup or copy credentials. Existing project processes are not reloaded or restarted automatically. A nested cwd or nearer .omp can change discovery; use or reopen OMP from the repository root for root-scoped rules/tools. This session is human-controlled: wait for human plan review.");
    } else {
        let role = match request.scope {
            SetupScope::Manager => Some("Manager"),
            SetupScope::Architect => Some("Architect"),
            _ => None,
        };
        if let Some(role) = role {
            text.push_str(&format!("{role}-only assets are loaded by that coordinator's private directory extension, never by other coordinators or workers. Root instructions reload next {role} turn; skill/tool/rule catalogs need native reload or an explicit user reopen. Do not restart The {role} automatically. Never modify Global, other coordinator, repository, or analysis-result assets. This session is human-controlled: wait for human plan review."));
        } else {
            text.push_str("Global native assets apply to the default OMP profile. Existing sessions need native reload or an explicit user reopen for skill/tool/rule catalogs. Do not restart coordinators automatically. This session is human-controlled: wait for human plan review.");
        }
    }
    if markdown_source.is_some() || !routes.profiles.is_empty() {
        text.push_str("\n\nMARKDOWN EXCEPTION: every requested Markdown asset whose line above names a Markdown source folder is authored only there and is deployed after explicit human review (Apply to OMP). Do not edit its native deployed copy. Tools, companion scripts, plugin entrypoints/manifests and native configuration remain at their native destinations exactly as requested; never copy them into the Markdown source.");
    }
    Ok(text)
}

impl SetupService {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            jobs: Mutex::new(HashMap::new()),
            markdown: std::sync::OnceLock::new(),
        }
    }
    pub fn install_markdown(&self,service:Arc<AgentMarkdownService>)->Result<()> {
        service.recover()?;
        self.markdown.set(service.clone()).map_err(|_|"Markdown service already installed")?;
        *MARKDOWN_SERVICE.lock()=Some(Arc::downgrade(&service));
        Ok(())
    }
    pub fn start(
        &self,
        registry: &Arc<Registry>,
        paths: &SetupPaths,
        request: SetupRequest,
    ) -> Result<SetupAdmission> {
        let request = request.normalize()?;
        let coordinator = match request.scope {
            SetupScope::Manager => Some((InstanceKind::Manager, "Manager")),
            SetupScope::Architect => Some((InstanceKind::Architect, "Architect")),
            _ => None,
        };
        if let Some((kind, name)) = coordinator {
            if !registry.snapshot().iter().any(|row| row.kind == kind) {
                return Err(format!("Set up The {name} before creating {name}-only assets"));
            }
        }
        let repository_root = if request.scope == SetupScope::Repository {
            Some(repository_root(registry, request.project_instance_id.as_deref().ok_or("Repository setup requires a project")?)?)
        } else { None };
        let folder = self
            .root
            .join("agent-setup/jobs")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        let overlay = folder.join("setup-plan.yml");
        fs::write(
            &overlay,
            "plan:\n  enabled: true\n  defaultOnStartup: true\n",
        )
        .map_err(|e| e.to_string())?;
        let name = match request.scope {
            SetupScope::Global => "Agent setup — Global".into(),
            SetupScope::Manager => "Agent setup — The Manager".into(),
            SetupScope::Architect => "Agent setup — The Architect".into(),
            SetupScope::Repository => {
                let root = repository_root.as_ref().ok_or("Repository setup requires a project")?;
                let basename = root.file_name().map(|name| name.to_string_lossy()).unwrap_or_else(|| root.to_string_lossy());
                format!("Agent setup — Repository: {basename}")
            }
        };
        let row = registry.create(CreateInput {
            name,
            project_path: folder.to_string_lossy().into(),
            configs: vec![overlay],
            ..Default::default()
        })?;
        self.jobs.lock().map_err(|e| e.to_string())?.insert(
            row.id.clone(),
            Job {
                request: request.clone(),
                paths: paths.clone(),
                repository_root,
                admission: Some(false),
                sending: false,
            },
        );
        if let Err(error) = registry.start(&row.id, Origin::Human) {
            return Ok(SetupAdmission {
                instance: registry.inspect(&row.id, Origin::Human)?,
                prompt_sent: Some(false),
                error: Some(error),
            });
        }
        self.send(registry, &row.id, &request)
    }
    pub fn send(
        &self,
        registry: &Arc<Registry>,
        id: &str,
        request: &SetupRequest,
    ) -> Result<SetupAdmission> {
        let request = request.clone().normalize()?;
        if request.scope == SetupScope::Repository {
            let source = registry.inspect(request.project_instance_id.as_deref().ok_or("Repository setup requires a project")?, Origin::Human)
                .map_err(|_| "Repository project is no longer available".to_string())?;
            if source.kind != InstanceKind::Instance {
                return Err("Repository setup requires an ordinary project instance".into());
            }
        }
        let row = registry.inspect(id, Origin::Human)?;
        let jobs_root =
            fs::canonicalize(self.root.join("agent-setup/jobs")).map_err(|e| e.to_string())?;
        let cwd = fs::canonicalize(&row.project_path).map_err(|e| e.to_string())?;
        if !cwd.starts_with(jobs_root)
            || row.kind != InstanceKind::Instance
            || row.manager_id.is_some()
        {
            return Err("Setup retry requires its human-controlled setup session".into());
        }
        let hub = registry
            .manager_hub()
            .ok_or("manager bridge not initialized")?;
        let routes=self.markdown.get().map(|service|service.routes(registry,&request)).transpose()?.unwrap_or_default();
        let _admission=instruction_read_admission()?;
        let text = {
            let mut jobs = self.jobs.lock().map_err(|e| e.to_string())?;
            let job = jobs
                .get_mut(id)
                .ok_or("No live setup request; inspect or reopen the native session")?;
            if job.request != request || job.admission != Some(false) || job.sending {
                return Err(
                    "Setup prompt already admitted or outcome unknown; inspect the native session"
                        .into(),
                );
            }
            let text = prompt_with_routes(&request, &job.paths, job.repository_root.as_deref(), &routes)?;
            job.sending = true;
            text
        };
        let (sent, error) = match hub.wait_ready(id) {
            Err(error) => (Some(false), Some(error)),
            Ok(()) => match hub.prompt_human(id, &text) {
                Ok(_) => (Some(true), None),
                Err(error) => (None, Some(format!("Prompt admission outcome unknown: {error}. Inspect the native session before sending anything else."))),
            },
        };
        if let Some(job) = self.jobs.lock().map_err(|e| e.to_string())?.get_mut(id) {
            job.sending = false;
            job.admission = sent;
        }
        Ok(SetupAdmission {
            instance: registry.inspect(id, Origin::Human)?,
            prompt_sent: sent,
            error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(kind: SetupKind, name: &str) -> SetupRequest {
        SetupRequest {
            scope: SetupScope::Global,
            project_instance_id: None,
            items: vec![SetupItem {
                kind,
                name: name.into(),
                description: "Preserve local policy".into(),
                language: None,
            }],
        }
    }
    #[test]
    fn requests_reject_escaping_and_conflicting_targets() {
        for name in ["../outside", "a/b", "a\\b", "", "CON", "lpt1", "--bad"] {
            assert!(request(SetupKind::Skill, name).normalize().is_err());
        }
        let good = request(SetupKind::Skill, "Local Review")
            .normalize()
            .unwrap();
        assert_eq!(good.items[0].name, "local-review");
        let mut duplicate = good.clone();
        duplicate.items.push(good.items[0].clone());
        assert!(duplicate.normalize().is_err());
        assert!(request(SetupKind::Tool, "helper").normalize().is_err());
        let empty = SetupRequest {
            scope: SetupScope::Global,
            items: vec![],
            project_instance_id: None,
        };
        assert!(empty.normalize().is_err());
        let mut no_description = request(SetupKind::Rules, "");
        no_description.items[0].description = "  ".into();
        assert!(no_description.normalize().is_err());
    }
    fn item(kind: SetupKind, name: &str) -> SetupItem {
        request(kind, name).items.remove(0)
    }
    fn requests(scope: SetupScope, items: Vec<SetupItem>) -> SetupRequest {
        SetupRequest { scope, items, project_instance_id: None }
    }
    #[test]
    fn agents_items_may_be_unnamed_or_named_but_never_collide() {
        let ok = requests(
            SetupScope::Global,
            vec![
                item(SetupKind::Agents, "  "),
                item(SetupKind::Agents, "Code Reviewer"),
                item(SetupKind::Agents, "planner"),
            ],
        )
        .normalize()
        .unwrap();
        let names: Vec<_> = ok.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["", "code-reviewer", "planner"]);
        for bad in [
            vec![item(SetupKind::Agents, "../x")],
            vec![item(SetupKind::Agents, "a"), item(SetupKind::Agents, "A")],
            vec![item(SetupKind::Agents, ""), item(SetupKind::Agents, " ")],
        ] {
            assert!(requests(SetupScope::Global, bad).normalize().is_err());
        }
    }
    #[test]
    fn profile_items_need_valid_native_names_and_global_scope() {
        let ok = requests(SetupScope::Global, vec![item(SetupKind::Profile, "qa-plan")])
            .normalize()
            .unwrap();
        assert_eq!(ok.items[0].name, "qa-plan");
        for name in ["default", "con", "", "../x"] {
            assert!(
                requests(SetupScope::Global, vec![item(SetupKind::Profile, name)])
                    .normalize()
                    .is_err(),
                "{name}"
            );
        }
        let mut undescribed = item(SetupKind::Profile, "plan");
        undescribed.description = " ".into();
        assert!(requests(SetupScope::Global, vec![undescribed]).normalize().is_err());
        assert!(requests(SetupScope::Manager, vec![item(SetupKind::Profile, "plan")])
            .normalize()
            .is_err());
    }
    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }
    #[test]
    fn agent_files_are_listed_in_order_and_only_listed_files_verify() {
        let root = std::env::temp_dir().join(format!("omp-agent-files-{}", uuid::Uuid::new_v4()));
        let paths = SetupPaths {
            global_omp_folder: root.join("global"),
            global_agent_folder: root.join("global/agent"),
            manager_folder: root.join("manager-setup"),
            architect_folder: root.join("architect-setup"),
        };
        touch(&paths.global_agent_folder.join("agent-instructions/beta/AGENTS.md"));
        touch(&paths.global_agent_folder.join("agent-instructions/alpha/AGENTS.md"));
        touch(&paths.manager_folder.join("agent-instructions/alpha/AGENTS.md"));
        touch(&paths.global_agent_folder.join("agent-instructions/empty/other.md"));
        touch(&paths.global_agent_folder.join("agent-instructions/stray.md"));
        let outside = root.join("elsewhere/AGENTS.md");
        touch(&outside);
        let files = scan_agent_files(&paths, InstanceKind::Instance);
        let listed: Vec<_> = files.iter().map(|f| (f.name.as_str(), f.scope)).collect();
        assert_eq!(
            listed,
            [
                ("alpha", SetupScope::Global),
                ("alpha", SetupScope::Manager),
                ("beta", SetupScope::Global)
            ]
        );
        let architect_file = paths.architect_folder.join("agent-instructions/designer/AGENTS.md");
        touch(&architect_file);
        for kind in [InstanceKind::Instance, InstanceKind::Manager] {
            let files = scan_agent_files(&paths, kind);
            assert!(!files.iter().any(|file| file.scope == SetupScope::Architect));
            assert!(verify_agent_file_in(&files, &architect_file).is_err());
        }
        let architect_files = scan_agent_files(&paths, InstanceKind::Architect);
        assert!(architect_files.iter().any(|file| file.scope == SetupScope::Architect));
        assert_eq!(verify_agent_file_in(&architect_files, &architect_file).unwrap(), canonical(&architect_file).unwrap());
        let verified = verify_agent_file_in(&files, &files[2].path).unwrap();
        assert!(verified.is_file());
        assert!(verify_agent_file_in(&files, &outside).is_err());
        let missing = root.join("nothing/AGENTS.md");
        assert!(verify_agent_file_in(&files, &missing)
            .unwrap_err()
            .starts_with("Agent instructions file not found"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn profiles_listing_matches_native_naming_and_login_state() {
        let root = std::env::temp_dir().join(format!("omp-profiles-{}", uuid::Uuid::new_v4()));
        let profiles = root.join("profiles");
        touch(&profiles.join("plan/agent/config.yml"));
        touch(&profiles.join("tiny/agent/agent.db"));
        // Windows strips trailing dots from directory names, so "x." is checked on the rule itself.
        assert!(!valid_profile_name("x."));
        for skipped in ["UPPER", "bad name", "con"] {
            touch(&profiles.join(skipped).join("agent/config.yml"));
        }
        touch(&profiles.join("stray-file"));
        fs::create_dir_all(profiles.join("noagent")).unwrap();
        let found = scan_profiles(&root);
        let listed: Vec<_> = found.iter().map(|p| (p.name.as_str(), p.has_login)).collect();
        assert_eq!(listed, [("plan", false), ("tiny", true)]);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn repository_roots_use_nearest_git_markers_and_validate_saved_sources() {
        let root = std::env::temp_dir().join(format!("omp-repo-root-{}", uuid::Uuid::new_v4()));
        let repo = root.join("repo");
        let nested = repo.join("src/nested");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(repo.join(".git")).unwrap();
        assert_eq!(find_repository_root(&nested).unwrap(), canonical(&repo).unwrap());
        fs::write(repo.join("src/.git"), "gitdir: elsewhere").unwrap();
        assert_eq!(find_repository_root(&nested).unwrap(), canonical(&repo.join("src")).unwrap());
        fs::remove_file(repo.join("src/.git")).unwrap();
        let fallback = root.join("unversioned");
        fs::create_dir(&fallback).unwrap();
        assert_eq!(find_repository_root(&fallback).unwrap(), canonical(&fallback).unwrap());
        assert!(find_repository_root(&root.join("missing")).is_err());
        fs::write(root.join("file"), "not a directory").unwrap();
        assert!(find_repository_root(&root.join("file")).is_err());
        let registry = Registry::load(root.join("instances.json"), root.clone()).unwrap();
        let source = registry.create(CreateInput { project_path: nested.to_string_lossy().into(), ..Default::default() }).unwrap();
        assert_eq!(repository_root(&registry, &source.id).unwrap(), canonical(&repo).unwrap());
        assert!(!repo.join(".omp").exists());
        let manager = registry.create_manager(CreateInput { project_path: nested.to_string_lossy().into(), ..Default::default() }).unwrap();
        assert!(repository_root(&registry, &manager.id).is_err());
        assert!(repository_root(&registry, "unknown").is_err());
        let home = registry.create(CreateInput { project_path: root.to_string_lossy().into(), ..Default::default() }).unwrap();
        assert_eq!(repository_root(&registry, &home.id).unwrap_err(), "Repository setup cannot target the home directory");
        fs::write(repo.join(".omp"), "conflict").unwrap();
        assert!(repository_root(&registry, &source.id).unwrap_err().starts_with("Repository setup path is not a directory"));
        fs::remove_file(repo.join(".omp")).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn repository_folder_rejects_escape_through_a_link() {
        let root = std::env::temp_dir().join(format!("omp-repo-link-{}", uuid::Uuid::new_v4()));
        let repo = root.join("repo");
        let outside = root.join("outside");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let link = repo.join(".omp");
        #[cfg(windows)]
        let linked = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"]).arg(&link).arg(&outside)
            .output().unwrap().status.success();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, &link).is_ok();
        if linked {
            assert_eq!(repository_folder(&repo).unwrap_err(), "Repository setup folder must stay inside the repository");
            #[cfg(windows)]
            fs::remove_dir(&link).unwrap();
            #[cfg(unix)]
            fs::remove_file(&link).unwrap();
        } else {
            eprintln!("Repository junction/symlink containment check unexercised: link creation refused");
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn repository_scope_requires_an_identity_and_disallows_profiles() {
        let mut request = request(SetupKind::Rules, "");
        request.scope = SetupScope::Repository;
        assert_eq!(request.clone().normalize().unwrap_err(), "Repository setup requires a project");
        request.project_instance_id = Some("   ".into());
        assert!(request.clone().normalize().is_err());
        request.project_instance_id = Some("project-a".into());
        assert_eq!(request.clone().normalize().unwrap().project_instance_id.as_deref(), Some("project-a"));
        for scope in [SetupScope::Global, SetupScope::Manager, SetupScope::Architect] {
            let mut ordinary = request.clone();
            ordinary.scope = scope;
            assert_eq!(ordinary.normalize().unwrap().project_instance_id, None);
        }
        request.items = vec![item(SetupKind::Profile, "qa-plan")];
        assert_eq!(request.normalize().unwrap_err(), "OMP profiles are global; choose the Global scope");
    }
    #[test]
    fn repository_prompt_targets_every_asset_inside_the_bound_repo() {
        let root = std::env::temp_dir().join(format!("omp-repo-prompt-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let paths = SetupPaths { global_omp_folder: root.join("global"), global_agent_folder: root.join("global/agent"), manager_folder: root.join("manager"), architect_folder: root.join("architect-setup") };
        let mut request = requests(SetupScope::Repository, vec![
            item(SetupKind::Agents, ""), item(SetupKind::Agents, "reviewer"),
            item(SetupKind::Rules, ""), item(SetupKind::Skill, "review"),
            item(SetupKind::Workflow, "review"),
            SetupItem { language: Some(ScriptLanguage::Python), ..item(SetupKind::Tool, "helper") },
        ]);
        request.project_instance_id = Some("source".into());
        let request = request.normalize().unwrap();
        let text = prompt(&request, &paths, Some(&root)).unwrap().replace('\\', "/");
        for file in ["AGENTS.md", "agent-instructions/reviewer/AGENTS.md", "RULES.md", "skills/review/SKILL.md", "SKILLS.md", "rules/workflow-review.md", "tools/helper/index.ts", "tools/helper/run.py"] {
            assert!(text.contains(&canonical(&root).unwrap().join(".omp").join(file).display().to_string().replace('\\', "/")), "{file}");
        }
        assert!(!text.contains(&paths.global_agent_folder.display().to_string().replace('\\', "/")));
        assert!(!root.join(".omp").exists());
        assert!(prompt(&request, &paths, None).is_err());
        let manager = requests(SetupScope::Manager, vec![item(SetupKind::Rules, "")]);
        assert!(prompt(&manager, &paths, None).unwrap().contains(&paths.manager_folder.display().to_string()));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn prompts_target_named_agent_files_and_global_profile_folders() {
        let paths = SetupPaths {
            global_omp_folder: PathBuf::from("/g/.omp"),
            global_agent_folder: PathBuf::from("/g/.omp/agent"),
            manager_folder: PathBuf::from("/m/manager-setup"),
            architect_folder: PathBuf::from("/m/architect-setup"),
        };
        let text = prompt(
            &requests(
                SetupScope::Global,
                vec![
                    item(SetupKind::Agents, "reviewer"),
                    item(SetupKind::Agents, ""),
                    item(SetupKind::Profile, "qa-plan"),
                ],
            )
            .normalize()
            .unwrap(),
            &paths,
            None,
        ).unwrap();
        assert!(text.contains("Requested Agents: agent-instructions/reviewer/AGENTS.md"));
        assert!(text.contains("Requested Agents: AGENTS.md\n"));
        let profile = PathBuf::from("/g/.omp").join("profiles").join("qa-plan").join("agent").join("config.yml");
        assert!(text.contains(&format!("Requested Profile: {}", profile.display())));
    }
    #[test]
    fn setup_permissions_and_admission_state_prevent_replay() -> Result<()> {
        let root = std::env::temp_dir().join(format!("omp-setup-policy-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("agent-setup/jobs/job")).unwrap();
        let registry = Arc::new(Registry::load(root.join("instances.json"), root.clone()).unwrap());
        let service = SetupService::new(root.clone());
        let paths = SetupPaths {
            global_omp_folder: root.join("global"),
            global_agent_folder: root.join("global/agent"),
            manager_folder: root.join("manager-setup"),
            architect_folder: root.join("architect-setup"),
        };
        let mut manager_request = request(SetupKind::Rules, "");
        manager_request.scope = SetupScope::Manager;
        assert_eq!(
            service
                .start(&registry, &paths, manager_request)
                .err()
                .as_deref(),
            Some("Set up The Manager before creating Manager-only assets")
        );
        assert!(registry.snapshot().is_empty());
        let mut architect_request = request(SetupKind::Rules, "");
        architect_request.scope = SetupScope::Architect;
        assert_eq!(
            service.start(&registry, &paths, architect_request).err().as_deref(),
            Some("Set up The Architect before creating Architect-only assets")
        );
        let row = registry
            .create(CreateInput {
                project_path: root.join("agent-setup/jobs/job").to_string_lossy().into(),
                ..Default::default()
            })
            .unwrap();
        let hub = crate::manager::ManagerHub::bind(root.join("bridge.mjs")).unwrap();
        hub.bind_registry(Arc::downgrade(&registry)).unwrap();
        registry.install_manager_hub(hub).unwrap();
        let request = request(SetupKind::Rules, "").normalize().unwrap();
        for admission in [Some(true), None] {
            service
                .jobs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    row.id.clone(),
                    Job {
                        request: request.clone(),
                        paths: paths.clone(),
                        repository_root: None,
                        admission,
                        sending: false,
                    },
                );
            assert_eq!(
                service.send(&registry, &row.id, &request).err().as_deref(),
                Some(
                    "Setup prompt already admitted or outcome unknown; inspect the native session"
                )
            );
        }
        service
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                row.id.clone(),
                Job {
                    request: request.clone(),
                    paths,
                    repository_root: None,
                    admission: Some(false),
                    sending: true,
                },
            );
        assert!(service.send(&registry, &row.id, &request).is_err());
        assert!(registry
            .inspect(&row.id, Origin::Human)
            .unwrap()
            .manager_id
            .is_none());
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        fs::create_dir(&repo_a).unwrap();
        fs::create_dir(&repo_b).unwrap();
        let source_a = registry.create(CreateInput { project_path: repo_a.to_string_lossy().into(), ..Default::default() }).unwrap();
        let source_b = registry.create(CreateInput { project_path: repo_b.to_string_lossy().into(), ..Default::default() }).unwrap();
        let mut bound = request.clone();
        bound.scope = SetupScope::Repository;
        bound.project_instance_id = Some(source_a.id.clone());
        service.jobs.lock().map_err(|error| error.to_string())?.insert(row.id.clone(), Job {
            request: bound.clone(), paths: SetupPaths { global_omp_folder: root.join("global"), global_agent_folder: root.join("global/agent"), manager_folder: root.join("manager-setup"), architect_folder: root.join("architect-setup") },
            repository_root: Some(canonical(&repo_a).unwrap()), admission: Some(false), sending: false,
        });
        let mut changed = bound.clone();
        changed.project_instance_id = Some(source_b.id);
        assert!(service.send(&registry, &row.id, &changed).is_err());
        assert!(!service.jobs.lock().map_err(|error| error.to_string())?.get(&row.id).unwrap().sending);
        for admission in [Some(true), None] {
            service.jobs.lock().map_err(|error| error.to_string())?.get_mut(&row.id).unwrap().admission = admission;
            assert!(service.send(&registry, &row.id, &bound).is_err());
        }
        {
            let mut jobs = service.jobs.lock().map_err(|error| error.to_string())?;
            let job = jobs.get_mut(&row.id).unwrap();
            job.admission = Some(false);
            job.sending = true;
        }
        assert!(service.send(&registry, &row.id, &bound).is_err());
        registry.remove(&source_a.id, Origin::Human).unwrap();
        assert_eq!(service.send(&registry, &row.id, &bound).err().as_deref(), Some("Repository project is no longer available"));
        registry.shutdown();
        fs::remove_dir_all(root).unwrap();
        Ok(())
    }
    #[test]
    fn plugins_are_available_in_every_scope_while_profiles_stay_global() {
        let parsed: SetupRequest = serde_json::from_str(r#"{"scope":"manager","items":[{"kind":"plugin","name":"Release Notes","description":"Summarise changes"}]}"#).unwrap();
        assert_eq!(parsed.normalize().unwrap().items[0].name, "release-notes");
        for scope in [SetupScope::Global, SetupScope::Manager, SetupScope::Architect, SetupScope::Repository] {
            let mut plugin = requests(scope, vec![item(SetupKind::Plugin, "release-notes"), item(SetupKind::Skill, "release-notes")]);
            plugin.project_instance_id = Some("project".into());
            plugin.items[0].language = Some(ScriptLanguage::Python);
            let normalized = plugin.normalize().unwrap();
            assert_eq!(normalized.items[0].language, None, "only tools keep a language");
        }
        let mut duplicate = requests(SetupScope::Global, vec![item(SetupKind::Plugin, "a"), item(SetupKind::Plugin, "A")]);
        assert_eq!(duplicate.clone().normalize().unwrap_err(), "Setup items must have distinct target paths");
        duplicate.items.pop();
        duplicate.items.push(item(SetupKind::Profile, "plan-mode"));
        assert!(duplicate.clone().normalize().is_ok());
        duplicate.scope = SetupScope::Manager;
        assert_eq!(duplicate.normalize().unwrap_err(), "OMP profiles are global; choose the Global scope");
        assert!(request(SetupKind::Plugin, "../escape").normalize().is_err());
    }
    #[test]
    fn plugin_prompts_target_the_scope_extensions_folder() {
        let paths = SetupPaths { global_omp_folder: "G".into(), global_agent_folder: PathBuf::from("G").join("agent"), manager_folder: PathBuf::from("M"), architect_folder: PathBuf::from("A") };
        for (scope, target) in [(SetupScope::Global, PathBuf::from("G").join("agent")), (SetupScope::Manager, PathBuf::from("M")), (SetupScope::Architect, PathBuf::from("A"))] {
            let text = prompt(&requests(scope, vec![item(SetupKind::Plugin, "release-notes")]), &paths, None).unwrap();
            assert!(text.contains(&target.display().to_string()));
            assert!(text.contains("extensions/release-notes/package.json and extensions/release-notes/index.ts"));
            assert!(text.contains("`omp plugin link`"));
        }
    }
    #[test]
    fn manager_plugins_are_validated_and_ordered_without_executing_them() {
        let root = std::env::temp_dir().join(format!("omp-manager-plugins-{}", uuid::Uuid::new_v4()));
        assert!(private_plugin_paths(&root, InstanceKind::Manager).unwrap().is_empty());
        let extensions = root.join("extensions");
        let write = |relative: &str, text: &str| {
            let file = extensions.join(relative);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, text).unwrap();
        };
        let manifest = r#"{"name":"p","omp":{"extensions":["index.ts"]}}"#;
        write("zeta/package.json", manifest);
        write("zeta/index.ts", "throw new Error('must never run')");
        write("alpha/package.json", r#"{"name":"p","omp":{"extensions":["./src"]}}"#);
        write("alpha/src/index.js", "");
        write(".hidden/package.json", "not json");
        write("loose/index.ts", "");
        write("note.txt", "");
        let names = |paths: Vec<PathBuf>| paths.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(names(private_plugin_paths(&root, InstanceKind::Manager).unwrap()), ["alpha", "zeta"]);
        assert_eq!(names(private_plugin_paths(&root, InstanceKind::Architect).unwrap()), ["alpha", "zeta"]);
        assert!(private_plugin_paths(&root, InstanceKind::Instance).is_err());
        for (text, expected) in [
            ("{", "package.json is invalid"),
            (r#"{"omp":{"extensions":["missing.ts"]}}"#, "declares a missing extension module"),
            (r#"{"omp":{"extensions":["../zeta/index.ts"]}}"#, "outside its package"),
            (r#"{"omp":{"extensions":["src/main.ts"]}}"#, "cannot load; use index.ts"),
        ] {
            write("bad/package.json", text);
            write("bad/src/main.ts", "");
            let error = private_plugin_paths(&root, InstanceKind::Manager).unwrap_err();
            assert!(error.starts_with("Manager plugin bad") && error.contains(expected), "{error}");
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn architect_setup_materializes_separately_and_preserves_assets() {
        let root = std::env::temp_dir().join(format!("omp-private-setup-{}", uuid::Uuid::new_v4()));
        touch(&root.join("architect-setup/AGENTS.md"));
        touch(&root.join("architect/results.json"));
        materialize(&root).unwrap();
        let loader = include_str!("../../bridge/coordinator-setup.ts");
        for role in ["manager", "architect"] {
            assert_eq!(fs::read_to_string(root.join(format!("{role}-setup/index.ts"))).unwrap(), loader);
        }
        materialize(&root).unwrap();
        assert_eq!(fs::read_to_string(root.join("architect-setup/AGENTS.md")).unwrap(), "x");
        assert_eq!(fs::read_to_string(root.join("architect/results.json")).unwrap(), "x");
        let request = requests(SetupScope::Architect, vec![item(SetupKind::Profile, "private")]);
        assert_eq!(request.normalize().unwrap_err(), "OMP profiles are global; choose the Global scope");
        fs::remove_dir_all(root).unwrap();
    }
}
