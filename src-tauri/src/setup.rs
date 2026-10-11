use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config;
use crate::instance::{CreateInput, Instance, InstanceKind, Origin, Registry};

pub type Result<T, E = String> = std::result::Result<T, E>;

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum SetupScope {
    Global,
    Manager,
}
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum SetupKind {
    Agents,
    Rules,
    Skill,
    Tool,
    Workflow,
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
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupPaths {
    pub global_omp_folder: PathBuf,
    pub global_agent_folder: PathBuf,
    pub manager_folder: PathBuf,
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
    admission: Option<bool>,
    sending: bool,
}
pub struct SetupService {
    root: PathBuf,
    jobs: Mutex<HashMap<String, Job>>,
}

impl SetupRequest {
    pub fn normalize(mut self) -> Result<Self> {
        if self.items.is_empty() {
            return Err("Describe at least one setup item".into());
        }
        let mut targets = HashSet::new();
        for item in &mut self.items {
            item.description = item.description.trim().into();
            if item.description.is_empty() {
                return Err("Every selected item needs a description".into());
            }
            if matches!(item.kind, SetupKind::Agents | SetupKind::Rules) {
                item.name.clear();
            } else {
                let name = item.name.trim();
                if name.contains(['/', '\\']) || name.contains("..") {
                    return Err("Setup names cannot contain paths".into());
                }
                item.name = name
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join("-")
                    .to_lowercase();
                if item.name.is_empty()
                    || item.name.starts_with('-')
                    || item.name.ends_with('-')
                    || item.name.contains("--")
                    || !item
                        .name
                        .bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                {
                    return Err("Use letters, numbers, and single hyphens for a setup name".into());
                }
                let reserved = matches!(item.name.as_str(), "con" | "prn" | "aux" | "nul")
                    || (item.name.len() == 4
                        && (item.name.starts_with("com") || item.name.starts_with("lpt"))
                        && matches!(item.name.as_bytes()[3], b'1'..=b'9'));
                if reserved {
                    return Err("That name is reserved by Windows".into());
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
    let folder = root.join("manager-setup");
    fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let file = folder.join("index.ts");
    let source = include_str!("../../bridge/manager-setup.ts");
    if fs::read_to_string(&file).ok().as_deref() != Some(source) {
        fs::write(file, source).map_err(|e| e.to_string())?;
    }
    Ok(())
}
pub fn paths(registry: &Registry) -> Result<SetupPaths> {
    let native = config::native_request(
        registry.config_root(),
        &registry.home,
        json!({"action":"paths","cwd":registry.home,"profile":"default"}),
    )?;
    let global_omp_folder = native["globalOmpFolder"]
        .as_str()
        .ok_or("OMP global folder unavailable")?;
    let global_agent_folder = native["globalAgentFolder"]
        .as_str()
        .ok_or("OMP agent folder unavailable")?;
    Ok(SetupPaths {
        global_omp_folder: global_omp_folder.into(),
        global_agent_folder: global_agent_folder.into(),
        manager_folder: registry.config_root().join("manager-setup"),
    })
}

pub fn open_folder(registry: &Registry, scope: SetupScope) -> Result<()> {
    let folder = match scope {
        SetupScope::Global => paths(registry)?.global_omp_folder,
        SetupScope::Manager => registry.config_root().join("manager-setup"),
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

fn prompt(request: &SetupRequest, paths: &SetupPaths) -> String {
    let target = match request.scope {
        SetupScope::Global => &paths.global_agent_folder,
        SetupScope::Manager => &paths.manager_folder,
    };
    let mut text = format!("Create the user-requested OMP Agent setup below in {}. Scope: {:?}. Platform: {}.\n\nREVIEW FIRST: You are in native plan mode. Read existing assets, preserve unrelated content, and write only your native plan until the human selects a native execution option. Propose the complete plan using write to xd://propose. Do this setup inline; do not use subagents or other models to generate it. The workflows being authored may describe future native delegation, but do not execute those workflows now.\n\nAfter human approval, implement real files and validate their native loading and behavior. Never modify config.yml/config.yaml, auth or credential databases, app default snapshots, bridge/index.ts entrypoints, installed OMP, or unrelated files. Never download or install dependencies/interpreters, add telemetry/updaters/autostart, or run unreviewed external code. Read existing targets before proposing changes. Report actual changed paths and verification; do not claim completion from merely writing files.\n\n", target.display(), request.scope, std::env::consts::OS);
    for item in &request.items {
        let file = match item.kind {
            SetupKind::Agents => "AGENTS.md".into(),
            SetupKind::Rules => "RULES.md".into(),
            SetupKind::Skill => format!("skills/{}/SKILL.md and SKILLS.md", item.name),
            SetupKind::Tool => format!("tools/{}/index.ts", item.name),
            SetupKind::Workflow => format!("rules/workflow-{}.md", item.name),
        };
        text.push_str(&format!(
            "Requested {:?}: {file}\nDescription:\n{}\n",
            item.kind, item.description
        ));
        match item.kind {
            SetupKind::Agents | SetupKind::Rules => text.push_str("Surgically merge requested instructions; preserve existing unrelated policy. RULES.md is sticky.\n"),
            SetupKind::Skill => text.push_str("Create native SKILL.md with YAML name/description and concrete reusable steps. Update SKILLS.md as a readable catalog pointing to the real skill, preserving other entries. The catalog itself is not a native skill.\n"),
            SetupKind::Workflow => text.push_str("Use native rule frontmatter description, alwaysApply: true, agents: [main]. Describe semantic trigger, native delegation/reviewer steps, required evidence, completion/decision handling, and stop conditions. Use installed native agents; if a required reviewer is unavailable, request human guidance. No scheduler or automatic hooks.\n"),
            SetupKind::Tool => {
                text.push_str(&format!("Language: {:?}. Native default factory in index.ts uses injected api.zod, approval: 'exec', execute(toolCallId, params, onUpdate, ctx, signal). Validate arguments; return native content/details with true stdout/stderr/error/abort state. No import-time execution. Python/Bash code lives beside it as run.py/run.sh, invoked via api.exec(interpreter, argumentArray, {{cwd:ctx.sessionManager.getCwd(),signal}}). Missing interpreter must fail clearly, never install it. On Windows use installed Git Bash rather than System32/WindowsApps WSL launchers. TypeScript logic stays in the native tool module. Inspect local interpreter locations before selecting one. Validate using a harmless fixture operation after approval.\n", item.language));
            }
        }
        text.push('\n');
    }
    text.push_str("Global native assets apply to the default OMP profile. Manager-only assets are loaded by a private directory extension, not by worker project .omp folders. Instruction text reloads next Manager turn; skill/tool/rule catalogs need native reload or an explicit user reopen. Do not restart The Manager automatically. This session is human-controlled: wait for human plan review.");
    text
}

impl SetupService {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            jobs: Mutex::new(HashMap::new()),
        }
    }
    pub fn start(
        &self,
        registry: &Arc<Registry>,
        paths: &SetupPaths,
        request: SetupRequest,
    ) -> Result<SetupAdmission> {
        let request = request.normalize()?;
        if request.scope == SetupScope::Manager
            && !registry
                .snapshot()
                .iter()
                .any(|row| row.kind == InstanceKind::Manager)
        {
            return Err("Set up The Manager before creating Manager-only assets".into());
        }
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
        let title = match request.scope {
            SetupScope::Global => "Global",
            SetupScope::Manager => "The Manager",
        };
        let row = registry.create(CreateInput {
            name: format!("Agent setup — {title}"),
            project_path: folder.to_string_lossy().into(),
            configs: vec![overlay],
            ..Default::default()
        })?;
        self.jobs.lock().map_err(|e| e.to_string())?.insert(
            row.id.clone(),
            Job {
                request: request.clone(),
                paths: paths.clone(),
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
        let paths = {
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
            job.sending = true;
            job.paths.clone()
        };
        let (sent, error) = match hub.wait_ready(id) {
            Err(error) => (Some(false), Some(error)),
            Ok(()) => match hub.prompt_human(id, &prompt(&request, &paths)) {
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
        };
        assert!(empty.normalize().is_err());
        let mut no_description = request(SetupKind::Rules, "");
        no_description.items[0].description = "  ".into();
        assert!(no_description.normalize().is_err());
    }
    #[test]
    fn setup_permissions_and_admission_state_prevent_replay() {
        let root = std::env::temp_dir().join(format!("omp-setup-policy-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("agent-setup/jobs/job")).unwrap();
        let registry = Arc::new(Registry::load(root.join("instances.json"), root.clone()).unwrap());
        let service = SetupService::new(root.clone());
        let paths = SetupPaths {
            global_omp_folder: root.join("global"),
            global_agent_folder: root.join("global/agent"),
            manager_folder: root.join("manager-setup"),
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
        registry.shutdown();
        fs::remove_dir_all(root).unwrap();
    }
}
