use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::config::GlobalConfigStore;
use crate::manager::ManagerHub;
use crate::terminal::TerminalService;
use crate::{pty, session};

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum InstanceKind {
    #[default]
    Instance,
    Manager,
    Architect,
}

/// What a helper agent is for: the Code diff page (explain / review the changes) or the Git tree (search history).
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum HelperPurpose {
    Diff,
    Git,
}

impl HelperPurpose {
    pub fn name(self) -> &'static str {
        match self {
            Self::Diff => "diff",
            Self::Git => "git",
        }
    }
}

/// A read-only helper agent for `target`'s repository (`repo` is its root when the helper was created).
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct HelperLink {
    pub target: String,
    pub purpose: HelperPurpose,
    pub repo: String,
}

#[derive(Clone, Copy)]
pub enum Origin<'a> {
    Human,
    Manager(&'a str),
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateInput {
    pub name: String,
    pub project_path: String,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub configs: Vec<PathBuf>,
    #[serde(default)]
    pub model: String,
    pub resume_session_id: Option<String>,
    #[serde(default)]
    pub use_global_config: Option<bool>,
    #[serde(default)]
    pub agent_file: Option<PathBuf>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Definition {
    pub id: String,
    #[serde(flatten)]
    pub input: CreateInput,
    pub session_path: Option<PathBuf>,
    #[serde(default)]
    pub kind: InstanceKind,
    #[serde(default)]
    pub manager_id: Option<String>,
    #[serde(default)]
    pub plan_target: Option<String>,
    #[serde(default)]
    pub helper: Option<HelperLink>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HumanRequest {
    pub id: String,
    pub reason: String,
    pub decision_id: Option<String>,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeUiCapabilities {
    pub agent_hub: bool,
    pub native_decisions: bool,
    pub native_decisions_error: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub id: String,
    pub name: String,
    pub project_path: String,
    pub profile: String,
    pub agent_file: Option<PathBuf>,
    pub configs: Vec<PathBuf>,
    pub model: String,
    pub status: String,
    pub activity: String,
    pub task: Option<String>,
    pub subagents: Vec<session::Subagent>,
    pub plan_path: Option<PathBuf>,
    /// `"todo"` (live unfinished todo list), `"file"` (native plan file) or `"saved"` (last saved plan);
    /// `None` when there is no plan or it is completed.
    pub plan_kind: Option<String>,
    pub last_plan_handoff: Option<String>,
    pub session_path: Option<PathBuf>,
    pub last_error: Option<String>,
    pub todo_done: usize,
    pub todo_total: usize,
    pub cols: u16,
    pub rows: u16,
    pub generation: u64,
    pub exit_code: Option<u32>,
    pub kind: InstanceKind,
    pub manager_id: Option<String>,
    pub plan_target: Option<String>,
    pub helper: Option<HelperLink>,
    pub bridge_ready: bool,
    pub capabilities: NativeUiCapabilities,
    pub pending_decision_count: usize,
    pub human_request: Option<HumanRequest>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoordinatorBootstrap {
    pub manager: Instance,
    pub architect: Instance,
    pub temporary_folders: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlLease {
    pub generation: u64,
    pub session_id: Option<String>,
    pub ownership_epoch: u64,
    pub manager_id: Option<String>,
    pub workspace_epoch: u64,
}

/// Workspace admission uses only saved metadata, never transcript/task state.
#[derive(Clone)]
pub struct WorkspaceMetadata {
    pub definition: Definition,
    pub lease: ControlLease,
    pub running: bool,
    pub workspace_epoch: u64,
    pub human_request_id: Option<String>,
}

#[derive(Clone)]
pub enum WorkspaceSourceBinding {
    Path { source_id: String, canonical_path: String },
    Grant { source_id: String, canonical_path: String, expected: ControlLease },
    Return { source_id: String, canonical_path: String, expected: ControlLease, human_request_id: String },
}

#[derive(Clone)]
pub struct WorkspaceCommit {
    pub request_id: String,
    pub coordinator_id: String,
    pub expected: ControlLease,
    pub target_folder: String,
    pub session_id: Option<String>,
    pub session_file: Option<PathBuf>,
    pub workspace_epoch: u64,
    pub source: Option<WorkspaceSourceBinding>,
}

struct Record {
    definition: Definition,
    process: Option<pty::Pty>,
    reader: session::TailReader,
    generation: u64,
    error: Option<String>,
    native_session_id: Option<String>,
    materialized: bool,
    bridge_ready: bool,
    capabilities: NativeUiCapabilities,
    pending_decision_count: usize,
    human_request: Option<HumanRequest>,
    ownership_epoch: u64,
    workspace_epoch: u64,
    workspace_request: Option<String>,
    workspace_committed: Option<String>,
    saved_plan: Option<PathBuf>,
    saved_checked: Option<std::time::Instant>,
    saved_for: Option<PathBuf>,
    last_plan_handoff: Option<String>,
}

const SAVED_PLAN_RECHECK: std::time::Duration = std::time::Duration::from_secs(10);

enum PlanSource {
    Todo,
    File(PathBuf, &'static str),
    Completed,
    Missing,
}

impl Record {
    fn new(definition: Definition) -> Self {
        let materialized = definition
            .session_path
            .as_ref()
            .is_some_and(|p| p.is_file());
        let native_session_id = definition
            .session_path
            .as_ref()
            .and_then(|p| session::header(p))
            .and_then(|(h, _)| h["id"].as_str().map(String::from));
        Self {
            definition,
            process: None,
            reader: session::TailReader::default(),
            generation: 0,
            error: None,
            native_session_id,
            materialized,
            bridge_ready: false,
            capabilities: NativeUiCapabilities::default(),
            pending_decision_count: 0,
            human_request: None,
            ownership_epoch: 0,
            workspace_epoch: 0,
            workspace_request: None,
            workspace_committed: None,
            saved_plan: None,
            saved_checked: None,
            saved_for: None,
            last_plan_handoff: None,
        }
    }
    /// The next launch starts a new OMP session: nothing is resumed and the old session's state is dropped.
    fn forget_session(&mut self) {
        self.definition.session_path = None;
        self.reader = session::TailReader::default();
        self.native_session_id = None;
        self.materialized = false;
    }
    fn lease(&self) -> ControlLease {
        ControlLease {
            generation: self.generation,
            session_id: self.native_session_id.clone(),
            ownership_epoch: self.ownership_epoch,
            manager_id: self.definition.manager_id.clone(),
            workspace_epoch: self.workspace_epoch,
        }
    }
    /// What "the plan" of this session is: its live todo list while unfinished, else its native plan file, else
    /// the last plan saved for the agent. A finished todo list means the plan is completed (no plan).
    fn plan_source(&self) -> PlanSource {
        let state = &self.reader.state;
        if state.todo_total > 0 {
            return if state.todo_done >= state.todo_total { PlanSource::Completed } else { PlanSource::Todo };
        }
        if let Some(path) = &state.plan_path {
            return PlanSource::File(path.clone(), "file");
        }
        match self.saved_plan.as_ref().filter(|path| path.is_file()) {
            Some(path) => PlanSource::File(path.clone(), "saved"),
            None => PlanSource::Missing,
        }
    }
    fn live_plan(&self) -> Result<Option<String>, String> {
        match self.plan_source() {
            PlanSource::Todo => Ok(Some(session::todo_markdown(&self.reader.state.phases))),
            PlanSource::File(path, _) => fs::read_to_string(path).map(Some).map_err(|e| e.to_string()),
            PlanSource::Completed | PlanSource::Missing => Ok(None),
        }
    }
    fn view(&mut self) -> Instance {
        let input = &self.definition.input;
        let mut status = "stopped";
        let mut activity = "stopped";
        let mut cols = pty::DEFAULT_COLS;
        let mut rows = pty::DEFAULT_ROWS;
        let mut exit_code = None;
        if let Some(process) = self.process.as_mut() {
            cols = process.cols;
            rows = process.rows;
            match process.poll() {
                Ok(Some(code)) => {
                    exit_code = Some(code);
                    self.bridge_ready = false;
                }
                Ok(None) => {
                    let last = process.last_output();
                    status = if last == 0 { "starting" } else { "running" };
                    activity = if last == 0 {
                        "starting"
                    } else if session::now_ms().saturating_sub(last) < 2000 {
                        "working"
                    } else {
                        "idle"
                    };
                }
                Err(error) => self.error = Some(error),
            }
        }
        if self.error.is_some() {
            status = "error";
        }
        let state = &self.reader.state;
        Instance {
            id: self.definition.id.clone(),
            name: input.name.clone(),
            project_path: input.project_path.clone(),
            profile: input.profile.clone(),
            agent_file: input.agent_file.clone(),
            configs: input.configs.clone(),
            model: input.model.clone(),
            status: status.into(),
            activity: activity.into(),
            task: state.task.clone(),
            subagents: state.subagents.clone(),
            plan_path: state.plan_path.clone(),
            plan_kind: match self.plan_source() {
                PlanSource::Todo => Some("todo".into()),
                PlanSource::File(_, kind) => Some(kind.into()),
                PlanSource::Completed | PlanSource::Missing => None,
            },
            last_plan_handoff: self.last_plan_handoff.clone(),
            session_path: self.definition.session_path.clone(),
            last_error: self.error.clone(),
            todo_done: state.todo_done,
            todo_total: state.todo_total,
            cols,
            rows,
            generation: self.generation,
            exit_code,
            kind: self.definition.kind,
            manager_id: self.definition.manager_id.clone(),
            plan_target: self.definition.plan_target.clone(),
            helper: self.definition.helper.clone(),
            bridge_ready: self.bridge_ready,
            capabilities: if self.bridge_ready { self.capabilities.clone() } else { NativeUiCapabilities::default() },
            pending_decision_count: self.pending_decision_count,
            human_request: self.human_request.clone(),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct Saved {
    version: u32,
    instances: Vec<Definition>,
}

/// What the Terminal pane needs to know about one saved agent.
#[derive(Clone)]
pub struct TerminalSource {
    pub id: String,
    pub name: String,
    pub project_path: String,
    pub session_path: Option<PathBuf>,
    pub session_id: Option<String>,
    pub generation: u64,
}

pub struct Registry {
    records: Mutex<Vec<Record>>,
    pub home: PathBuf,
    file: PathBuf,
    hub: OnceLock<Arc<ManagerHub>>,
    browser: OnceLock<Arc<crate::browser::BrowserSupervisor>>,
    terminals: OnceLock<Arc<TerminalService>>,
    lifecycle: parking_lot::Mutex<()>,
    plan_updates: parking_lot::Mutex<()>,
    pub global_config: GlobalConfigStore,
}

fn authorize(
    records: &[Record],
    index: usize,
    origin: Origin<'_>,
    mutation: bool,
) -> Result<(), String> {
    if let Origin::Manager(manager) = origin {
        if !records
            .iter()
            .any(|r| r.definition.id == manager && r.definition.kind == InstanceKind::Manager)
        {
            return Err("not_controlled".into());
        }
        let record = &records[index];
        if record.definition.kind != InstanceKind::Instance
            || record.definition.manager_id.as_deref() != Some(manager)
        {
            return Err("not_controlled".into());
        }
        if mutation && record.human_request.is_some() {
            return Err("human_intervention_pending".into());
        }
    }
    Ok(())
}

fn index(records: &[Record], id: &str) -> Result<usize, String> {
    records
        .iter()
        .position(|r| r.definition.id == id)
        .ok_or_else(|| "unknown instance".into())
}

/// Component-wise path identity, including Windows drive and UNC-share roots.
fn folder_components(folder: &str) -> (String, Vec<String>, bool) {
    let text = folder.replace('\\', "/");
    let text = text.strip_prefix("//?/UNC/").map(|s| format!("//{s}"))
        .unwrap_or_else(|| text.strip_prefix("//?/").unwrap_or(&text).to_owned());
    if let Some(unc) = text.strip_prefix("//") {
        let mut parts = unc.split('/').filter(|s| !s.is_empty());
        let root = format!("//{}/{}", parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        return (root, parts.map(String::from).collect(), true);
    }
    if text.as_bytes().get(1) == Some(&b':') {
        return (text[..2].to_owned(), text[2..].split('/').filter(|s| !s.is_empty()).map(String::from).collect(), true);
    }
    (if text.starts_with('/') { "/".into() } else { String::new() },
        text.split('/').filter(|s| !s.is_empty()).map(String::from).collect(), false)
}

/// An ordinary agent: a project instance that is neither a helper nor a plan session.
fn is_agent_definition(definition: &Definition) -> bool {
    definition.kind == InstanceKind::Instance && definition.helper.is_none() && definition.plan_target.is_none()
}

/// Manager, Architect and ordinary agents may relocate their own working directory; helpers and plan sessions may not.
pub fn may_change_directory(definition: &Definition) -> bool {
    matches!(definition.kind, InstanceKind::Manager | InstanceKind::Architect) || is_agent_definition(definition)
}

pub(crate) fn same_folder(left: &str, right: &str) -> bool {
    fn identity(path: &str) -> (u8, &str) {
        if let Some(unc) = path.strip_prefix("\\\\?\\UNC\\").or_else(|| path.strip_prefix("//?/UNC/")) {
            return (3, unc.trim_end_matches(['/', '\\']));
        }
        let path = path.strip_prefix("\\\\?\\").or_else(|| path.strip_prefix("//?/")).unwrap_or(path);
        if let Some(unc) = path.strip_prefix("\\\\").or_else(|| path.strip_prefix("//")) {
            (3, unc.trim_end_matches(['/', '\\']))
        } else if path.as_bytes().get(1) == Some(&b':') {
            (2, path.trim_end_matches(['/', '\\']))
        } else {
            (u8::from(path.starts_with('/')), path.trim_end_matches('/'))
        }
    }
    let (left_root, left) = identity(left);
    let (right_root, right) = identity(right);
    if left_root != right_root { return false; }
    if left_root >= 2 {
        let normalized = |c: char| if c == '\\' { '/' } else { c }.to_lowercase();
        left.chars().flat_map(normalized).eq(right.chars().flat_map(normalized))
    } else {
        left == right
    }
}

pub fn common_ancestor_folder(roots: &[String]) -> Option<String> {
    let (root, mut common, windows) = folder_components(roots.first()?);
    if root.is_empty() { return None; }
    for folder in &roots[1..] {
        let (other_root, parts, other_windows) = folder_components(folder);
        if windows != other_windows || !(if windows { root.eq_ignore_ascii_case(&other_root) } else { root == other_root }) {
            return None;
        }
        let length = common.iter().zip(&parts).take_while(|(a,b)| {
            if windows { a.eq_ignore_ascii_case(b) } else { a == b }
        }).count();
        common.truncate(length);
    }
    let suffix = common.join("/");
    Some(if root == "/" { format!("/{suffix}") }
        else if suffix.is_empty() { format!("{root}/") }
        else { format!("{root}/{suffix}") })
}

impl Registry {
    pub fn coordinator_id(&self, kind: InstanceKind) -> Result<String, String> {
        if kind == InstanceKind::Instance { return Err("Workspace changes require a coordinator".into()); }
        let records = self.records.lock().map_err(|e| e.to_string())?;
        records.iter().find(|r| r.definition.kind == kind).map(|r| r.definition.id.clone())
            .ok_or_else(|| "Coordinator does not exist".into())
    }

    pub fn workspace_metadata(&self, id: &str) -> Result<WorkspaceMetadata, String> {
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        let record = &mut records[i];
        let running = match record.process.as_mut() {
            Some(process) => matches!(process.poll(), Ok(None)),
            None => record.bridge_ready,
        };
        Ok(WorkspaceMetadata {
            definition: record.definition.clone(), lease: record.lease(), running,
            workspace_epoch: record.workspace_epoch,
            human_request_id: record.human_request.as_ref().map(|r| r.id.clone()),
        })
    }

    pub fn coordinator_neutral_folder(&self, kind: InstanceKind) -> Result<String, String> {
        use sha2::{Digest, Sha256};
        let role = match kind {
            InstanceKind::Manager => "manager",
            InstanceKind::Architect => "architect",
            _ => return Err("Workspace changes require a coordinator".into()),
        };
        fs::create_dir_all(self.config_root()).map_err(|e| e.to_string())?;
        let root = fs::canonicalize(self.config_root()).map_err(|e| e.to_string())?;
        let normalized = root.to_string_lossy().replace('\\', "/");
        #[cfg(windows)]
        let normalized = normalized.to_lowercase();
        let folder = std::env::temp_dir().join("omp-manager")
            .join(format!("{:x}", Sha256::digest(normalized.as_bytes()))).join(role);
        fs::create_dir_all(&folder).map_err(|e| format!("Coordinator workspace: {e}"))?;
        crate::setup::canonical(&folder).map(|p| p.to_string_lossy().into_owned()).map_err(|e| e.to_string())
    }

    pub fn is_real_project_folder(&self, folder: &str) -> bool {
        let Ok(folder) = crate::setup::canonical(Path::new(folder)) else { return false; };
        if !folder.is_dir() { return false; }
        let folder = folder.to_string_lossy();
        [crate::setup::canonical(self.config_root()).ok(),
            crate::setup::canonical(&std::env::temp_dir()).ok().map(|p| p.join("omp-manager"))]
            .into_iter().flatten().all(|excluded| {
                let excluded = excluded.to_string_lossy().into_owned();
                common_ancestor_folder(&[folder.to_string(), excluded.clone()])
                    .is_none_or(|common| !same_folder(&common, &excluded))
            })
    }

    fn source_path(&self, definition: &Definition) -> Result<String, String> {
        if definition.kind != InstanceKind::Instance || definition.helper.is_some() || definition.plan_target.is_some() {
            return Err("Repository context requires an ordinary project instance".into());
        }
        let path = crate::setup::canonical(Path::new(&definition.input.project_path)).map_err(|e| e.to_string())?;
        let path = path.to_string_lossy().into_owned();
        if !self.is_real_project_folder(&path) {
            return Err("Repository context cannot use an app-managed workspace".into());
        }
        Ok(path)
    }

    pub fn workspace_source_binding(&self, id: &str) -> Result<WorkspaceSourceBinding, String> {
        let definition = {
            let records = self.records.lock().map_err(|e| e.to_string())?;
            records[index(&records, id)?].definition.clone()
        };
        Ok(WorkspaceSourceBinding::Path {
            source_id: id.into(), canonical_path: self.source_path(&definition)?,
        })
    }

    fn managed_definitions(&self, manager_id: &str) -> Result<Vec<Definition>, String> {
        let records = self.records.lock().map_err(|e| e.to_string())?;
        if records[index(&records, manager_id)?].definition.kind != InstanceKind::Manager {
            return Err("unknown manager".into());
        }
        Ok(records.iter().filter(|r| r.definition.manager_id.as_deref() == Some(manager_id)
            && is_agent_definition(&r.definition)).map(|r| r.definition.clone()).collect())
    }

    /// One entry per ordinary connected agent definition (never helpers, plan agents or native subagents):
    /// `(definition id, canonical actual project folder)`. Counted before any Git-root deduplication.
    pub fn managed_project_folders(&self, manager_id: &str) -> Result<Vec<(String, String)>, String> {
        let mut folders = Vec::new();
        for definition in self.managed_definitions(manager_id)? {
            let path = crate::setup::canonical(Path::new(&definition.input.project_path)).map_err(|e| e.to_string())?;
            let path = path.to_string_lossy().into_owned();
            if !self.is_real_project_folder(&path) { continue; }
            folders.push((definition.id, path));
        }
        folders.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(folders)
    }

    /// Canonical project folders of every ordinary agent definition (owned or not), best effort.
    pub fn ordinary_project_folders(&self) -> Vec<String> {
        self.snapshot().into_iter()
            .filter(|row| row.kind == InstanceKind::Instance && row.plan_target.is_none() && row.helper.is_none())
            .filter_map(|row| crate::setup::canonical(Path::new(&row.project_path)).ok())
            .map(|path| path.to_string_lossy().into_owned()).filter(|path| self.is_real_project_folder(path)).collect()
    }

    pub fn managed_repository_roots(&self, manager_id: &str) -> Result<Vec<String>, String> {
        let mut roots: Vec<String> = Vec::new();
        for definition in self.managed_definitions(manager_id)? {
            let path = crate::setup::canonical(Path::new(&definition.input.project_path)).map_err(|e| e.to_string())?;
            if !self.is_real_project_folder(&path.to_string_lossy()) { continue; }
            let root = crate::setup::find_repository_root(&path)?.to_string_lossy().into_owned();
            if !roots.iter().any(|old| same_folder(old, &root)) { roots.push(root); }
        }
        roots.sort_by_key(|p| p.to_lowercase());
        Ok(roots)
    }

    pub fn begin_workspace_change(&self, id: &str, request_id: &str, expected: &ControlLease) -> Result<(), String> {
        let _lifecycle = self.lifecycle.lock();
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        let record = &mut records[i];
        if record.definition.kind == InstanceKind::Instance && !is_agent_definition(&record.definition) { return Err("Only agent working directories can be changed".into()); }
        if record.lease() != *expected { return Err("stale_workspace_identity".into()); }
        if record.workspace_request.as_deref().is_some_and(|active| active != request_id) {
            return Err("workspace_change_busy".into());
        }
        record.workspace_request = Some(request_id.into());
        Ok(())
    }

    /// Only the native/offline transaction owner calls this after move/rollback has finished.
    pub fn finish_workspace_change(&self, id: &str, request_id: &str) {
        let _lifecycle = self.lifecycle.lock();
        if let Ok(mut records) = self.records.lock() {
            if let Some(record) = records.iter_mut().find(|r| r.definition.id == id) {
                if record.workspace_request.as_deref() == Some(request_id) { record.workspace_request = None; }
            }
        }
    }

    pub fn commit_workspace_change(&self, commit: &WorkspaceCommit) -> Result<ControlLease, String> {
        let target = crate::setup::canonical(Path::new(&commit.target_folder)).map_err(|e| e.to_string())?;
        if !target.is_dir() { return Err("project folder is not a directory".into()); }
        let target = target.to_string_lossy().into_owned();
        let lifecycle = self.lifecycle.lock();
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, &commit.coordinator_id)?;
        if records[i].workspace_committed.as_deref() == Some(&commit.request_id) {
            return Ok(records[i].lease());
        }
        if records[i].definition.kind == InstanceKind::Instance {
            // Ordinary agents move their own cwd only: no coordinator source binding, never an app-managed target.
            if !is_agent_definition(&records[i].definition) { return Err("Only agent working directories can be changed".into()); }
            if commit.source.is_some() { return Err("invalid_workspace_binding".into()); }
            if !self.is_real_project_folder(&target) { return Err("An agent working directory cannot be an app-managed workspace".into()); }
        }
        if records[i].workspace_request.as_deref() != Some(commit.request_id.as_str())
            || records[i].lease() != commit.expected || commit.workspace_epoch < records[i].workspace_epoch {
            return Err("stale_workspace_identity".into());
        }
        if commit.expected.session_id.is_some() && commit.session_id != commit.expected.session_id {
            return Err("stale_workspace_identity".into());
        }
        if records[i].definition.session_path.is_some() && commit.session_file.is_none() {
            return Err("Workspace relocation cannot discard a saved session".into());
        }
        let mut source_change = None;
        if let Some(binding) = &commit.source {
            let (id, path) = match binding {
                WorkspaceSourceBinding::Path { source_id, canonical_path }
                | WorkspaceSourceBinding::Grant { source_id, canonical_path, .. }
                | WorkspaceSourceBinding::Return { source_id, canonical_path, .. } => (source_id, canonical_path),
            };
            let source = index(&records, id)?;
            if !same_folder(&self.source_path(&records[source].definition)?, path) {
                return Err("Source repository changed; give repo again.".into());
            }
            match binding {
                WorkspaceSourceBinding::Path { .. } => {
                    if records[i].definition.kind != InstanceKind::Architect { return Err("invalid_workspace_binding".into()); }
                }
                WorkspaceSourceBinding::Grant { expected, .. }
                | WorkspaceSourceBinding::Return { expected, .. } => {
                    if records[i].definition.kind != InstanceKind::Manager || records[source].lease() != *expected {
                        return Err("stale_workspace_source".into());
                    }
                    if let WorkspaceSourceBinding::Return { human_request_id, .. } = binding {
                        if records[source].definition.manager_id.as_deref() != Some(commit.coordinator_id.as_str())
                            || records[source].human_request.as_ref().map(|h| &h.id) != Some(human_request_id) {
                            return Err("stale_human_request".into());
                        }
                    }
                    source_change = Some(source);
                }
            }
        }
        let old_destination = records[i].definition.clone();
        let old_source = source_change.map(|s| (s, records[s].definition.manager_id.clone()));
        records[i].definition.input.project_path = target;
        records[i].definition.session_path = commit.session_file.clone();
        if let Some(source) = source_change { records[source].definition.manager_id = Some(commit.coordinator_id.clone()); }
        if let Err(error) = self.save(&records) {
            records[i].definition = old_destination;
            if let Some((source, owner)) = old_source { records[source].definition.manager_id = owner; }
            return Err(error);
        }
        if old_destination.session_path != commit.session_file { records[i].reader = session::TailReader::default(); }
        records[i].native_session_id = commit.session_id.clone();
        records[i].materialized = commit.session_file.as_ref().is_some_and(|p| p.is_file());
        records[i].workspace_epoch = commit.workspace_epoch;
        records[i].workspace_committed = Some(commit.request_id.clone());
        let notification = source_change.map(|source| {
            records[source].ownership_epoch += 1;
            records[source].human_request = None;
            (records[source].definition.id.clone(), records[source].lease())
        });
        let lease = records[i].lease();
        drop(records);
        drop(lifecycle);
        if let (Some(hub), Some((id, lease))) = (self.hub.get(), notification) { hub.ownership_changed(&id, &lease); }
        Ok(lease)
    }

    pub fn take_back_control(&self, id: &str) -> Result<(), String> {
        self.set_manager_owner(id, None)
    }
    pub fn load(file: PathBuf, home: PathBuf) -> Result<Self, String> {
        let definitions = match fs::read(&file) {
            Ok(bytes) => {
                let saved: Saved = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("{}: {e}", file.display()))?;
                if saved.version != 1 {
                    return Err("unsupported instance registry version".into());
                }
                saved.instances
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(e.to_string()),
        };
        Ok(Self {
            global_config: GlobalConfigStore::load(
                file.parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf(),
            ),
            records: Mutex::new(definitions.into_iter().map(Record::new).collect()),
            home,
            file,
            hub: OnceLock::new(),
            browser: OnceLock::new(),
            terminals: OnceLock::new(),
            lifecycle: parking_lot::Mutex::new(()),
            plan_updates: parking_lot::Mutex::new(()),
        })
    }
    pub fn config_root(&self) -> &Path {
        self.file.parent().unwrap_or_else(|| Path::new("."))
    }
    pub fn install_manager_hub(&self, hub: Arc<ManagerHub>) -> Result<(), String> {
        self.hub
            .set(hub)
            .map_err(|_| "manager bridge already initialized".into())
    }
    pub fn manager_hub(&self) -> Option<&Arc<ManagerHub>> {
        self.hub.get()
    }
    pub fn install_browser_supervisor(&self, browser: Arc<crate::browser::BrowserSupervisor>) -> Result<(), String> {
        self.browser.set(browser).map_err(|_| "browser supervisor already initialized".into())
    }
    pub fn browser_supervisor(&self) -> Option<&Arc<crate::browser::BrowserSupervisor>> {
        self.browser.get()
    }
    pub fn install_terminal_service(&self, terminals: Arc<TerminalService>) -> Result<(), String> {
        self.terminals.set(terminals).map_err(|_| "terminal service already initialized".into())
    }
    /// Light per-instance facts for the Terminal pane; never starts or touches an agent.
    pub fn terminal_sources(&self) -> Vec<TerminalSource> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|record| TerminalSource {
                id: record.definition.id.clone(),
                name: record.definition.input.name.clone(),
                project_path: record.definition.input.project_path.clone(),
                session_path: record.definition.session_path.clone(),
                session_id: record.native_session_id.clone(),
                generation: record.generation,
            })
            .collect()
    }
    fn save(&self, records: &[Record]) -> Result<(), String> {
        let saved = Saved {
            version: 1,
            instances: records.iter().map(|r| r.definition.clone()).collect(),
        };
        fs::create_dir_all(self.file.parent().ok_or("invalid registry path")?)
            .map_err(|e| e.to_string())?;
        let temporary = self.file.with_extension("json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(&saved).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        fs::rename(temporary, &self.file).map_err(|e| e.to_string())
    }
    pub fn create(&self, input: CreateInput) -> Result<Instance, String> {
        self.create_definition(input, InstanceKind::Instance, None, None, None)
    }
    pub fn create_manager(&self, mut input: CreateInput) -> Result<Instance, String> {
        input.name = "The Manager".into();
        self.create_definition(input, InstanceKind::Manager, None, None, None)
    }
    pub fn create_architect(&self, mut input: CreateInput) -> Result<Instance, String> {
        input.name = "The Architect".into();
        self.create_definition(input, InstanceKind::Architect, None, None, None)
    }
    /// Definitions only: provisioning never starts a process or replaces saved coordinator state.
    pub fn ensure_coordinators(&self, preferred_folders: &[String]) -> Result<CoordinatorBootstrap, String> {
        use sha2::{Digest, Sha256};
        let root = fs::canonicalize(self.config_root()).or_else(|_| {
            fs::create_dir_all(self.config_root())?;
            fs::canonicalize(self.config_root())
        }).map_err(|e| e.to_string())?;
        let normalized = root.to_string_lossy().replace('\\', "/");
        #[cfg(windows)]
        let normalized = normalized.to_lowercase();
        let temporary_root = std::env::temp_dir().join("omp-manager")
            .join(format!("{:x}", Sha256::digest(normalized.as_bytes())));
        let temporary_paths = [temporary_root.join("manager"), temporary_root.join("architect")];
        let display = |path: &Path| {
            let value = path.to_string_lossy();
            if let Some(unc) = value.strip_prefix("\\\\?\\UNC\\") { format!("\\\\{unc}") }
            else { value.trim_start_matches("\\\\?\\").to_string() }
        };
        let temporary_folders: Vec<String> = temporary_paths.iter().map(|p| display(p)).collect();
        let candidates = preferred_folders.iter().cloned().chain(self.snapshot().into_iter()
            .filter(|row| row.kind == InstanceKind::Instance && row.plan_target.is_none() && row.helper.is_none())
            .map(|row| row.project_path));
        let excluded_temporary_root = fs::canonicalize(std::env::temp_dir()).map_err(|e| e.to_string())?.join("omp-manager");
        let real_folder = candidates.filter_map(|folder| fs::canonicalize(folder).ok())
            .find(|folder| folder.is_dir() && !folder.starts_with(&root)
                && !folder.starts_with(&excluded_temporary_root));
        let mut coordinators = Vec::new();
        for (offset, kind) in [InstanceKind::Manager, InstanceKind::Architect].into_iter().enumerate() {
            if let Some(existing) = self.snapshot().into_iter().find(|row| row.kind == kind) {
                // Only app-managed workspaces may be recreated. Missing real project folders remain untouched.
                if existing.project_path == temporary_folders[offset] {
                    fs::create_dir_all(&temporary_paths[offset]).map_err(|e| format!("Coordinator workspace: {e}"))?;
                }
                coordinators.push(existing);
                continue;
            }
            let folder = if let Some(folder) = &real_folder { folder.clone() } else {
                fs::create_dir_all(&temporary_paths[offset]).map_err(|e| format!("Coordinator workspace: {e}"))?;
                temporary_paths[offset].clone()
            };
            let input = CreateInput { project_path: display(&folder), ..CreateInput::default() };
            let created = match kind {
                InstanceKind::Manager => self.create_manager(input),
                _ => self.create_architect(input),
            };
            // A simultaneous provisioning/manual request may have won singleton creation.
            coordinators.push(match created {
                Ok(row) => row,
                Err(error) => self.snapshot().into_iter().find(|row| row.kind == kind).ok_or(error)?,
            });
        }
        let architect = coordinators.pop().ok_or("Architect provisioning failed")?;
        let manager = coordinators.pop().ok_or("Manager provisioning failed")?;
        Ok(CoordinatorBootstrap { manager, architect, temporary_folders })
    }
    pub fn create_owned(&self, manager_id: &str, input: CreateInput) -> Result<Instance, String> {
        self.create_definition(input, InstanceKind::Instance, Some(manager_id.into()), None, None)
    }
    pub fn plan_folder(&self, target_id: &str) -> PathBuf {
        self.config_root().join("plan-changes").join(target_id)
    }
    fn fallback_plan(&self, id: &str) -> PathBuf {
        self.plan_folder(id).join("target-plan.md")
    }
    /// A plan agent is an ordinary human-controlled instance linked to the session it plans. Its cwd is an
    /// app-owned folder so its sessions never appear in the target project's OMP session list.
    pub fn create_planner(&self, target_id: &str) -> Result<Instance, String> {
        let target = self.inspect(target_id, Origin::Human)?;
        let folder = self.plan_folder(target_id);
        fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        self.create_definition(
            CreateInput {
                name: format!("Plan — {}", target.name),
                project_path: folder.to_string_lossy().into(),
                profile: target.profile,
                configs: target.configs,
                model: target.model,
                use_global_config: Some(false),
                ..Default::default()
            },
            InstanceKind::Instance,
            None,
            Some(target_id.into()),
            None,
        )
    }
    pub fn helper_folder(&self, target_id: &str, purpose: HelperPurpose) -> PathBuf {
        self.config_root().join("helpers").join(target_id).join(purpose.name())
    }
    /// A helper agent is an ordinary human-controlled instance linked to a project. Like a plan agent its cwd is an
    /// app-owned folder, so its sessions never appear in the project's OMP session list.
    pub fn create_helper(&self, target_id: &str, purpose: HelperPurpose, repo: String) -> Result<Instance, String> {
        let target = self.inspect(target_id, Origin::Human)?;
        let folder = self.helper_folder(target_id, purpose);
        fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        let label = match purpose {
            HelperPurpose::Diff => "Diff",
            HelperPurpose::Git => "Git",
        };
        self.create_definition(
            CreateInput {
                name: format!("{label} helper — {}", target.name),
                project_path: folder.to_string_lossy().into(),
                profile: target.profile,
                configs: target.configs,
                model: target.model,
                use_global_config: Some(false),
                ..Default::default()
            },
            InstanceKind::Instance,
            None,
            None,
            Some(HelperLink { target: target_id.into(), purpose, repo }),
        )
    }
    pub fn helper_for(&self, target: &str, purpose: HelperPurpose) -> Option<String> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|r| r.definition.helper.as_ref().is_some_and(|h| h.target == target && h.purpose == purpose))
            .map(|r| r.definition.id.clone())
    }
    pub fn planner_for(&self, target: &str) -> Option<String> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|r| r.definition.plan_target.as_deref() == Some(target))
            .map(|r| r.definition.id.clone())
    }
    pub fn plan_target(&self, id: &str) -> Option<String> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|r| r.definition.id == id)
            .and_then(|r| r.definition.plan_target.clone())
    }
    /// The single place that decides where a session's plan is written: its plan file (native or saved), or an
    /// app-owned file for a session whose plan is its todo list.
    pub fn plan_file(&self, id: &str) -> Result<PathBuf, String> {
        let records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        let record = &records[i];
        if let Some(revision) = crate::plan::read_revision(&self.plan_folder(id), &record.definition.session_path, &record.live_plan()?)? {
            return Ok(revision.path);
        }
        Ok(match record.plan_source() {
            PlanSource::File(path, _) => path,
            _ => self.fallback_plan(id),
        })
    }
    fn create_definition(
        &self,
        mut input: CreateInput,
        kind: InstanceKind,
        manager_id: Option<String>,
        plan_target: Option<String>,
        helper: Option<HelperLink>,
    ) -> Result<Instance, String> {
        let cwd = crate::setup::canonical(Path::new(&input.project_path)).map_err(|e| e.to_string())?;
        if !cwd.is_dir() {
            return Err("project folder is not a directory".into());
        }
        if input.profile.contains(['/', '\\']) || input.profile == "." || input.profile == ".." {
            return Err("invalid profile name".into());
        }
        input.project_path = cwd.to_string_lossy().into();
        if input.name.trim().is_empty() {
            input.name = cwd.file_name().unwrap_or_default().to_string_lossy().into();
        }
        if input.profile == "default" {
            input.profile.clear();
        }
        if let Some(path) = input.agent_file.take() {
            let verified = crate::setup::verify_agent_file(self, &path, kind)?;
            input.agent_file = Some(verified);
        }
        if input.use_global_config.unwrap_or(input.profile.is_empty()) {
            if let Some(path) = self.global_config.current_path()? {
                if !input.configs.contains(&path) {
                    input.configs.insert(0, path);
                }
            }
        }
        for config in &mut input.configs {
            if !config.is_absolute() {
                *config = cwd.join(&*config);
            }
            if !config.is_file() {
                return Err(format!("config not found: {}", config.display()));
            }
        }
        let path = match &input.resume_session_id {
            Some(target) => Some(
                session::list(&self.home, &cwd, &input.profile)
                    .into_iter()
                    .find(|s| s.id == *target || s.path == Path::new(target))
                    .ok_or("session not found for this project/profile")?
                    .path,
            ),
            None => None,
        };
        let definition = Definition {
            id: uuid::Uuid::new_v4().to_string(),
            input,
            session_path: path,
            kind,
            manager_id,
            plan_target,
            helper,
        };
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        if kind != InstanceKind::Instance && records.iter().any(|r| r.definition.kind == kind) {
            return Err(format!("{} already exists", if kind == InstanceKind::Manager { "The Manager" } else { "The Architect" }));
        }
        if let Some(owner) = &definition.manager_id {
            if !records
                .iter()
                .any(|r| r.definition.id == *owner && r.definition.kind == InstanceKind::Manager)
            {
                return Err("not_controlled".into());
            }
        }
        if let Some(target) = &definition.plan_target {
            if !records.iter().any(|r| r.definition.id == *target && r.definition.plan_target.is_none() && r.definition.helper.is_none()) {
                return Err("unknown instance".into());
            }
            if records.iter().any(|r| r.definition.plan_target.as_deref() == Some(target.as_str())) {
                return Err("A plan agent already exists for this session".into());
            }
        }
        if let Some(link) = &definition.helper {
            if !records.iter().any(|r| r.definition.id == link.target && r.definition.plan_target.is_none() && r.definition.helper.is_none()) {
                return Err("unknown instance".into());
            }
            if records.iter().any(|r| r.definition.helper.as_ref().is_some_and(|h| h.target == link.target && h.purpose == link.purpose)) {
                return Err(format!("A {} helper already exists for this session", link.purpose.name()));
            }
        }
        let mut record = Record::new(definition);
        if let Some(path) = &record.definition.session_path {
            record.reader.update(path)?;
        }
        records.push(record);
        if let Err(error) = self.save(&records) {
            records.pop();
            return Err(error);
        }
        Ok(records.last_mut().ok_or("instance missing")?.view())
    }
    pub fn snapshot(&self) -> Vec<Instance> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
            .map(Record::view)
            .collect()
    }
    pub fn inspect(&self, id: &str, origin: Origin<'_>) -> Result<Instance, String> {
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        authorize(&records, i, origin, false)?;
        Ok(records[i].view())
    }
    pub fn lease(
        &self,
        id: &str,
        origin: Origin<'_>,
        mutation: bool,
    ) -> Result<ControlLease, String> {
        let records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        authorize(&records, i, origin, mutation)?;
        Ok(records[i].lease())
    }
    pub fn refresh(&self) -> Result<(), String> {
        let due: Vec<(String, String, String)> = {
            let mut records = self.records.lock().map_err(|e| e.to_string())?;
            for record in records.iter_mut() {
                if let Some(path) = &record.definition.session_path {
                    if !record.materialized && !path.is_file() {
                        continue;
                    }
                    record.materialized = true;
                    match record.reader.update(path) {
                        Ok(()) => {
                            if record
                                .error
                                .as_deref()
                                .is_some_and(|e| e.starts_with("session read:"))
                            {
                                record.error = None;
                            }
                        }
                        Err(error) => record.error = Some(format!("session read: {error}")),
                    }
                }
            }
            // The last saved plan is only needed while a session has neither a todo list nor a native plan; the
            // scan reads other sessions, so it runs outside the lock and at most every few seconds.
            records
                .iter_mut()
                .filter(|r| {
                    r.definition.plan_target.is_none()
                        && r.definition.helper.is_none()
                        && r.reader.state.todo_total == 0
                        && r.reader.state.plan_path.is_none()
                        && (r.saved_for != r.definition.session_path
                            || r.saved_checked.is_none_or(|at| at.elapsed() >= SAVED_PLAN_RECHECK))
                })
                .map(|r| {
                    r.saved_checked = Some(std::time::Instant::now());
                    r.saved_for = r.definition.session_path.clone();
                    (r.definition.id.clone(), r.definition.input.project_path.clone(), r.definition.input.profile.clone())
                })
                .collect()
        };
        for (id, project, profile) in due {
            let found = session::last_saved_plan(&session::list(&self.home, Path::new(&project), &profile));
            let mut records = self.records.lock().map_err(|e| e.to_string())?;
            if let Some(record) = records.iter_mut().find(|r| r.definition.id == id) {
                record.saved_plan = found;
            }
        }
        Ok(())
    }
    pub fn apply_identity(&self, id: &str, generation: u64, payload: &Value) -> Result<(), String> {
        let _lifecycle = self.lifecycle.lock();
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        if records[i].generation != generation { return Err("stale_generation".into()); }
        let epoch = payload["workspaceEpoch"].as_u64().unwrap_or(0);
        if records[i].workspace_request.is_some() || epoch < records[i].workspace_epoch {
            return Ok(());
        }
        let old_definition = records[i].definition.clone();
        let path = payload["sessionFile"].as_str().map(PathBuf::from);
        let dirty = old_definition.session_path != path;
        let cwd = payload["cwd"].as_str().filter(|s| Path::new(s).is_dir())
            .map(|cwd| crate::setup::canonical(Path::new(cwd)))
            .transpose().map_err(|e| e.to_string())?;
        records[i].definition.session_path = path;
        if let Some(cwd) = cwd { records[i].definition.input.project_path = cwd.to_string_lossy().into_owned(); }
        if dirty || old_definition.input.project_path != records[i].definition.input.project_path {
            if let Err(error) = self.save(&records) {
                records[i].definition = old_definition;
                return Err(error);
            }
        }
        if dirty { records[i].reader = session::TailReader::default(); }
        records[i].native_session_id = payload["sessionId"].as_str().map(String::from);
        records[i].materialized = payload["materialized"].as_bool().unwrap_or(false);
        records[i].bridge_ready = payload["supported"].as_bool().unwrap_or(true);
        records[i].capabilities.agent_hub = payload["capabilities"]["agentHub"].as_bool().unwrap_or(false);
        records[i].capabilities.native_decisions = payload["capabilities"]["nativeDecisions"].as_bool().unwrap_or(false);
        records[i].capabilities.native_decisions_error = payload["capabilities"]["nativeDecisionsError"].as_str().map(str::to_owned);
        records[i].workspace_epoch = epoch;
        Ok(())
    }
    pub fn set_bridge_ready(&self, id: &str, generation: u64, ready: bool) {
        if let Ok(mut records) = self.records.lock() {
            if let Some(record) = records
                .iter_mut()
                .find(|r| r.definition.id == id && r.generation == generation)
            {
                record.bridge_ready = ready;
            }
        }
    }
    pub fn set_decision_count(&self, id: &str, count: usize) {
        if let Ok(mut records) = self.records.lock() {
            if let Some(record) = records.iter_mut().find(|r| r.definition.id == id) {
                record.pending_decision_count = count;
            }
        }
    }
    pub(crate) fn set_manager_owner(&self, id: &str, manager_id: Option<&str>) -> Result<(), String> {
        let lifecycle = self.lifecycle.lock();
        let lease = {
            let mut records = self.records.lock().map_err(|e| e.to_string())?;
            let i = index(&records, id)?;
            if records[i].definition.kind != InstanceKind::Instance
                || records[i].definition.helper.is_some() || records[i].definition.plan_target.is_some() {
                return Err("Control transfer requires an ordinary project instance".into());
            }
            if let Some(owner) = manager_id {
                if !records
                    .iter()
                    .any(|r| r.definition.id == owner && r.definition.kind == InstanceKind::Manager)
                {
                    return Err("unknown manager".into());
                }
            }
            let previous = records[i].definition.manager_id.clone();
            records[i].definition.manager_id = manager_id.map(String::from);
            if let Err(e) = self.save(&records) {
                records[i].definition.manager_id = previous;
                return Err(e);
            }
            records[i].ownership_epoch += 1;
            records[i].human_request = None;
            records[i].lease()
        };
        drop(lifecycle);
        if let Some(hub) = self.hub.get() {
            hub.ownership_changed(id, &lease);
        }
        Ok(())
    }
    pub fn request_human(
        &self,
        id: &str,
        manager: &str,
        reason: String,
        decision_id: Option<String>,
    ) -> Result<(), String> {
        if reason.trim().is_empty() {
            return Err("human intervention reason is required".into());
        }
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        authorize(&records, i, Origin::Manager(manager), false)?;
        if records[i].human_request.is_none() {
            records[i].human_request = Some(HumanRequest {
                id: uuid::Uuid::new_v4().to_string(),
                reason,
                decision_id,
            });
        }
        Ok(())
    }
    pub fn resolve_human_decision(&self, id: &str, decision_id: &str) {
        if let Ok(mut records) = self.records.lock() {
            if let Some(record) = records.iter_mut().find(|r| r.definition.id == id) {
                if record
                    .human_request
                    .as_ref()
                    .and_then(|h| h.decision_id.as_deref())
                    == Some(decision_id)
                {
                    record.human_request = None;
                }
            }
        }
    }
    pub fn start(&self, id: &str, origin: Origin<'_>) -> Result<(), String> {
        self.start_with(id, origin, false)
    }
    /// `fresh` begins a new OMP session instead of resuming the saved one.
    pub fn start_with(&self, id: &str, origin: Origin<'_>, fresh: bool) -> Result<(), String> {
        let _lifecycle = self.lifecycle.lock();
        let (definition, generation, hub) = {
            let mut records = self.records.lock().map_err(|e| e.to_string())?;
            let i = index(&records, id)?;
            authorize(&records, i, origin, true)?;
            let record = &mut records[i];
            if record.workspace_request.is_some() { return Err("workspace_change_busy".into()); }
            if record.process.as_mut().is_some_and(|p| matches!(p.poll(), Ok(None))) {
                return Err("instance already running".into());
            }
            if let Some(path) = &record.definition.input.agent_file {
                if !path.is_file() {
                    let message = format!("Agent instructions file not found: {}", path.display());
                    record.error = Some(message.clone());
                    return Err(message);
                }
            }
            if fresh {
                record.forget_session();
            }
            let hub = self.hub.get().ok_or("manager bridge not initialized")?;
            record.process = None;
            record.generation += 1;
            record.workspace_epoch = 0;
            record.workspace_committed = None;
            record.error = None;
            record.bridge_ready = false;
            record.capabilities = NativeUiCapabilities::default();
            record.pending_decision_count = 0;
            let launch = (record.definition.clone(), record.generation, hub);
            if fresh {
                self.save(&records)?;
            }
            launch
        };
        // launch/invalidate can tear down a previous browser: never hold records.
        let result = (|| {
            let launch = hub.launch(&definition, generation)?;
            if let Some(browser) = self.browser.get() {
                browser.register(id, generation, &launch.token)?;
            }
            pty::Pty::spawn(&definition, &launch)
        })();
        if result.is_err() { hub.invalidate(id, generation); }
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        let record = &mut records[i];
        match result {
            Ok(process) => { record.process = Some(process); Ok(()) }
            Err(error) => { record.error = Some(error.clone()); Err(error) }
        }
    }
    pub fn stop(&self, id: &str, origin: Origin<'_>) -> Result<(), String> {
        let _lifecycle = self.lifecycle.lock();
        self.stop_locked(id, origin)
    }
    fn stop_locked(&self, id: &str, origin: Origin<'_>) -> Result<(), String> {
        let (generation, result) = {
            let mut records = self.records.lock().map_err(|e| e.to_string())?;
            let i = index(&records, id)?;
            authorize(&records, i, origin, true)?;
            if records[i].workspace_request.is_some() { return Err("workspace_change_busy".into()); }
            let result = records[i].process.as_mut().map_or(Ok(()), |process| process.stop());
            records[i].bridge_ready = false;
            (records[i].generation, result)
        };
        let browser_result = self.browser.get().map_or(Ok(()), |browser| browser.revoke(id, generation));
        if let Some(hub) = self.hub.get() {
            hub.invalidate(id, generation);
        }
        result.and(browser_result)
    }
    pub fn shutdown(&self) {
        let ids: Vec<String> = self.snapshot().into_iter().map(|r| r.id).collect();
        for id in ids {
            if let Err(e) = self.stop(&id, Origin::Human) {
                eprintln!("stop {id}: {e}");
            }
        }
        if let Some(hub) = self.hub.get() {
            hub.shutdown();
        }
        if let Some(browser) = self.browser.get() { browser.shutdown(); }
        if let Some(terminals) = self.terminals.get() { terminals.shutdown(); }
    }
    pub fn remove(&self, id: &str, origin: Origin<'_>) -> Result<(), String> {
        let _lifecycle = self.lifecycle.lock();
        // Plan agents and helper agents of this session go first so none can outlive it.
        let dependants: Vec<String> = {
            let records = self.records.lock().map_err(|e| e.to_string())?;
            let i = index(&records, id)?;
            authorize(&records, i, origin, true)?;
            if records[i].workspace_request.is_some() { return Err("workspace_change_busy".into()); }
            records
                .iter()
                .filter(|r| r.definition.plan_target.as_deref() == Some(id) || r.definition.helper.as_ref().is_some_and(|h| h.target == id))
                .map(|r| r.definition.id.clone())
                .collect()
        };
        for dependant in dependants {
            self.remove_locked(&dependant, Origin::Human)?;
        }
        self.remove_locked(id, origin)?;
        let _ = fs::remove_dir_all(self.plan_folder(id));
        let _ = fs::remove_dir_all(self.config_root().join("helpers").join(id));
        Ok(())
    }
    fn remove_locked(&self, id: &str, origin: Origin<'_>) -> Result<(), String> {
        self.stop_locked(id, origin)?;
        let released = {
            let mut records = self.records.lock().map_err(|e| e.to_string())?;
            let i = index(&records, id)?;
            authorize(&records, i, origin, true)?;
            let record = records.remove(i);
            let children: Vec<usize> = records
                .iter()
                .enumerate()
                .filter(|(_, r)| r.definition.manager_id.as_deref() == Some(id))
                .map(|(i, _)| i)
                .collect();
            for child in &children {
                records[*child].definition.manager_id = None;
            }
            if let Err(e) = self.save(&records) {
                for child in &children {
                    records[*child].definition.manager_id = Some(id.into());
                }
                records.insert(i, record);
                return Err(e);
            }
            children
                .into_iter()
                .map(|i| {
                    records[i].ownership_epoch += 1;
                    records[i].human_request = None;
                    (records[i].definition.id.clone(), records[i].lease())
                })
                .collect::<Vec<_>>()
        };
        if let Some(terminals) = self.terminals.get() {
            terminals.close_instance(id);
        }
        if let Some(hub) = self.hub.get() {
            for (child, lease) in released {
                hub.ownership_changed(&child, &lease);
            }
        }
        Ok(())
    }
    pub fn attach(&self, id: &str, channel: Channel<InvokeResponseBody>) -> Result<(), String> {
        let records = self.records.lock().map_err(|e| e.to_string())?;
        records[index(&records, id)?]
            .process
            .as_ref()
            .ok_or("instance not started")?
            .attach(channel)
    }
    pub fn write(&self, id: &str, data: &str, origin: Origin<'_>) -> Result<(), String> {
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        authorize(&records, i, origin, true)?;
        records[i]
            .process
            .as_mut()
            .ok_or("instance not running")?
            .write(data)
    }
    pub fn keys(
        &self,
        id: &str,
        generation: u64,
        session_id: Option<&str>,
        data: &str,
        origin: Origin<'_>,
    ) -> Result<(), String> {
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        authorize(&records, i, origin, true)?;
        if records[i].generation != generation
            || records[i].native_session_id.as_deref() != session_id
        {
            return Err("stale_generation".into());
        }
        records[i]
            .process
            .as_mut()
            .ok_or("instance not running")?
            .write(data)
    }
    pub fn resize(&self, id: &str, cols: u16, rows: u16, origin: Origin<'_>) -> Result<(), String> {
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        authorize(&records, i, origin, true)?;
        records[i]
            .process
            .as_mut()
            .ok_or("instance not running")?
            .resize(cols, rows)
    }
    pub fn screen(&self, id: &str, origin: Origin<'_>) -> Result<Value, String> {
        let records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        authorize(&records, i, origin, false)?;
        let screen = records[i]
            .process
            .as_ref()
            .ok_or("instance not started")?
            .screen()?;
        let mut value = serde_json::to_value(screen).map_err(|e| e.to_string())?;
        value["generation"] = json!(records[i].generation);
        value["sessionId"] = json!(records[i].native_session_id);
        Ok(value)
    }
    /// The session's current plan text: its live todo list rendered as markdown, its native plan file, or the last
    /// plan saved for the agent. `None` when there is no plan or it is completed.
    pub fn plan(&self, id: &str, origin: Origin<'_>) -> Result<Option<String>, String> {
        let records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        authorize(&records, i, origin, false)?;
        let live = records[i].live_plan()?;
        let revision = crate::plan::read_revision(&self.plan_folder(id), &records[i].definition.session_path, &live)?;
        Ok(revision.map(|revision| revision.text).or(live))
    }
    pub fn plan_revision(&self, id: &str) -> Result<Option<crate::plan::Revision>, String> {
        let records = self.records.lock().map_err(|e| e.to_string())?;
        let record = &records[index(&records, id)?];
        crate::plan::read_revision(&self.plan_folder(id), &record.definition.session_path, &record.live_plan()?)
    }
    /// Save the pre-change plan first, including live todos that do not yet exist in a plan file.
    pub fn revise_plan(&self, id: &str, text: &str) -> Result<String, String> {
        let _update = self.plan_updates.lock();
        let previous = self.plan(id, Origin::Human)?;
        let backup = previous.as_deref().map(|text| crate::plan::save_backup(&self.plan_folder(id), text)).transpose()?;
        let path = self.plan_file(id)?;
        let (session_path, base_live) = {
            let records = self.records.lock().map_err(|e| e.to_string())?;
            let record = &records[index(&records, id)?];
            (record.definition.session_path.clone(), record.live_plan()?)
        };
        crate::plan::write_atomic(&path, text)?;
        // For native files, capture the post-write live text; for todos it remains the pre-handoff worker list.
        let base_live = if matches!(self.inspect(id, Origin::Human)?.plan_kind.as_deref(), Some("file" | "saved")) {
            Some(text.to_string())
        } else { base_live };
        crate::plan::save_revision(&self.plan_folder(id), &crate::plan::Revision {
            id: uuid::Uuid::new_v4().to_string(), path, text: text.into(), session_path, base_live, handed_off: false,
        })?;
        Ok(backup.unwrap_or_default())
    }
    pub fn mark_plan_handoff(&self, id: &str, revision_id: &str) -> Result<(), String> {
        let mut revision = self.plan_revision(id)?.ok_or("No saved revision to hand off")?;
        if revision.id != revision_id { return Err("The plan changed; confirm the new revision before handoff".into()); }
        revision.handed_off = true;
        crate::plan::save_revision(&self.plan_folder(id), &revision)?;
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        // A fresh completion marker per handoff, even when the same revision is handed off again.
        records[i].last_plan_handoff = Some(uuid::Uuid::new_v4().to_string());
        Ok(())
    }
    pub fn transcript_path(
        &self,
        id: &str,
        child: &str,
        origin: Origin<'_>,
    ) -> Result<(PathBuf, CreateInput), String> {
        let records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        authorize(&records, i, origin, false)?;
        let record = &records[i];
        let path = record
            .reader
            .state
            .subagents
            .iter()
            .find(|s| s.id == child)
            .map(|s| s.path.clone())
            .ok_or("unknown subagent")?;
        Ok((path, record.definition.input.clone()))
    }
    pub fn main_transcript(
        &self,
        id: &str,
        origin: Origin<'_>,
    ) -> Result<(PathBuf, CreateInput), String> {
        let records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        authorize(&records, i, origin, false)?;
        let record = &records[i];
        Ok((
            record
                .definition
                .session_path
                .clone()
                .filter(|p| p.is_file())
                .ok_or("No saved transcript yet")?,
            record.definition.input.clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (PathBuf, Registry) {
        let folder =
            std::env::temp_dir().join(format!("omp-manager-registry-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&folder).unwrap();
        let registry = Registry::load(folder.join("instances.json"), folder.clone()).unwrap();
        (folder, registry)
    }
    fn input(folder: &Path) -> CreateInput {
        CreateInput {
            name: "Project".into(),
            project_path: folder.to_string_lossy().into(),
            ..Default::default()
        }
    }
    fn workspace_fixture() -> (PathBuf, Registry) {
        let (folder, _) = fixture();
        let registry = Registry::load(folder.join("config/instances.json"), folder.join("home")).unwrap();
        fs::create_dir_all(folder.join("a")).unwrap();
        fs::create_dir_all(folder.join("b")).unwrap();
        (folder, registry)
    }

    fn workspace_commit(registry: &Registry, id: &str, folder: &Path, source: Option<WorkspaceSourceBinding>) -> WorkspaceCommit {
        let expected = registry.lease(id, Origin::Human, false).unwrap();
        WorkspaceCommit {
            request_id: uuid::Uuid::new_v4().to_string(), coordinator_id: id.into(),
            target_folder: folder.to_string_lossy().into_owned(), workspace_epoch: expected.workspace_epoch + 1,
            session_id: expected.session_id.clone(), session_file: registry.workspace_metadata(id).unwrap().definition.session_path,
            expected, source,
        }
    }

    #[test]
    fn common_ancestor_uses_components_and_windows_roots() {
        let ancestor = |roots: &[&str]| common_ancestor_folder(&roots.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(ancestor(&["C:/work/repo", "c:\\WORK\\repository"]), Some("C:/work".into()));
        assert_eq!(ancestor(&["C:/work/repo", "c:/WORK/repo/"]), Some("C:/work/repo".into()));
        assert_eq!(ancestor(&["C:/repo", "D:/repo"]), None);
        assert_eq!(ancestor(&[r"\\server\share\a", r"\\SERVER\SHARE\b"]), Some("//server/share/".into()));
        assert_eq!(ancestor(&[r"\\server\share\a", r"\\server\other\b"]), None);
        assert_eq!(ancestor(&[r"\\?\UNC\server\share\a", r"\\server\share\a"]), Some("//server/share/a".into()));
        assert_eq!(ancestor(&["/work/repo", "/work/repository"]), Some("/work".into()));
    }

    #[test]
    fn workspace_commit_is_atomic_idempotent_and_fences_lifecycle_and_identity() {
        let (folder, registry) = workspace_fixture();
        let manager = registry.create_manager(input(&folder.join("a"))).unwrap();
        let source = registry.create(input(&folder.join("b"))).unwrap();
        let metadata = registry.workspace_metadata(&source.id).unwrap();
        let commit = workspace_commit(&registry, &manager.id, &folder.join("b"), Some(WorkspaceSourceBinding::Grant {
            source_id: source.id.clone(), canonical_path: source.project_path.clone(), expected: metadata.lease,
        }));
        registry.begin_workspace_change(&manager.id, &commit.request_id, &commit.expected).unwrap();
        for error in [
            registry.start(&manager.id, Origin::Human), registry.stop(&manager.id, Origin::Human),
            registry.remove(&manager.id, Origin::Human),
        ] { assert_eq!(error.unwrap_err(), "workspace_change_busy"); }
        registry.apply_identity(&manager.id, 0, &json!({"cwd":manager.project_path,"sessionId":"stale","workspaceEpoch":0})).unwrap();
        assert_eq!(registry.lease(&manager.id, Origin::Human, false).unwrap(), commit.expected);
        let result = registry.commit_workspace_change(&commit).unwrap();
        let owned = registry.lease(&source.id, Origin::Human, false).unwrap();
        assert_eq!(owned.manager_id.as_deref(), Some(manager.id.as_str()));
        assert_eq!(owned.ownership_epoch, 1);
        assert_eq!(registry.commit_workspace_change(&commit).unwrap(), result);
        assert_eq!(registry.lease(&source.id, Origin::Human, false).unwrap().ownership_epoch, 1);
        registry.finish_workspace_change(&manager.id, &commit.request_id);
        registry.apply_identity(&manager.id, 0, &json!({"cwd":manager.project_path,"sessionId":"stale","workspaceEpoch":0})).unwrap();
        assert_eq!(registry.lease(&manager.id, Origin::Human, false).unwrap(), result);
        let saved = Registry::load(registry.file.clone(), folder.join("home")).unwrap();
        assert!(same_folder(&saved.workspace_metadata(&manager.id).unwrap().definition.input.project_path, &source.project_path));
        registry.stop(&manager.id, Origin::Human).unwrap();
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn failed_workspace_save_restores_destination_and_source_and_keeps_reservation() {
        let (folder, registry) = workspace_fixture();
        let manager = registry.create_manager(input(&folder.join("a"))).unwrap();
        let source = registry.create(input(&folder.join("b"))).unwrap();
        let commit = workspace_commit(&registry, &manager.id, &folder.join("b"), Some(WorkspaceSourceBinding::Grant {
            source_id: source.id.clone(), canonical_path: source.project_path.clone(),
            expected: registry.lease(&source.id, Origin::Human, false).unwrap(),
        }));
        registry.begin_workspace_change(&manager.id, &commit.request_id, &commit.expected).unwrap();
        fs::create_dir(registry.file.with_extension("json.tmp")).unwrap();
        assert!(registry.commit_workspace_change(&commit).is_err());
        assert_eq!(registry.lease(&manager.id, Origin::Human, false).unwrap(), commit.expected);
        assert_eq!(registry.workspace_metadata(&manager.id).unwrap().definition.input.project_path, manager.project_path);
        assert!(registry.lease(&source.id, Origin::Human, false).unwrap().manager_id.is_none());
        assert_eq!(registry.stop(&manager.id, Origin::Human).unwrap_err(), "workspace_change_busy");
        registry.finish_workspace_change(&manager.id, &commit.request_id);
        registry.stop(&manager.id, Origin::Human).unwrap();
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn take_back_and_newer_human_request_prevent_queued_source_commit() {
        let (folder, registry) = workspace_fixture();
        let manager = registry.create_manager(input(&folder.join("a"))).unwrap();
        let source = registry.create_owned(&manager.id, input(&folder.join("b"))).unwrap();
        registry.request_human(&source.id, &manager.id, "old".into(), Some("old-decision".into())).unwrap();
        let metadata = registry.workspace_metadata(&source.id).unwrap();
        let commit = workspace_commit(&registry, &manager.id, &folder.join("b"), Some(WorkspaceSourceBinding::Return {
            source_id: source.id.clone(), canonical_path: source.project_path.clone(), expected: metadata.lease,
            human_request_id: metadata.human_request_id.unwrap(),
        }));
        registry.begin_workspace_change(&manager.id, &commit.request_id, &commit.expected).unwrap();
        registry.resolve_human_decision(&source.id, "old-decision");
        registry.request_human(&source.id, &manager.id, "new".into(), None).unwrap();
        let newer = registry.workspace_metadata(&source.id).unwrap().human_request_id;
        assert_eq!(registry.commit_workspace_change(&commit).unwrap_err(), "stale_human_request");
        assert_eq!(registry.workspace_metadata(&source.id).unwrap().human_request_id, newer);
        registry.take_back_control(&source.id).unwrap();
        assert_eq!(registry.commit_workspace_change(&commit).unwrap_err(), "stale_workspace_source");
        registry.finish_workspace_change(&manager.id, &commit.request_id);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn architect_source_binding_is_path_only_and_rejects_changed_repository() {
        let (folder, registry) = workspace_fixture();
        let manager = registry.create_manager(input(&folder.join("a"))).unwrap();
        let architect = registry.create_architect(input(&folder.join("a"))).unwrap();
        let source = registry.create(input(&folder.join("b"))).unwrap();
        let binding = registry.workspace_source_binding(&source.id).unwrap();
        registry.set_manager_owner(&source.id, Some(&manager.id)).unwrap();
        {
            let mut records = registry.records.lock().unwrap();
            let i = index(&records, &source.id).unwrap();
            records[i].generation += 1;
            // A broken transcript is irrelevant to this metadata-only action.
            records[i].definition.session_path = Some(folder.join("missing.jsonl"));
        }
        let commit = workspace_commit(&registry, &architect.id, &folder.join("b"), Some(binding.clone()));
        registry.begin_workspace_change(&architect.id, &commit.request_id, &commit.expected).unwrap();
        registry.commit_workspace_change(&commit).unwrap();
        registry.finish_workspace_change(&architect.id, &commit.request_id);
        let later = workspace_commit(&registry, &architect.id, &folder.join("b"), Some(binding));
        registry.begin_workspace_change(&architect.id, &later.request_id, &later.expected).unwrap();
        {
            let mut records = registry.records.lock().unwrap();
            let i = index(&records, &source.id).unwrap();
            records[i].definition.input.project_path = manager.project_path;
        }
        assert_eq!(registry.commit_workspace_change(&later).unwrap_err(), "Source repository changed; give repo again.");
        registry.finish_workspace_change(&architect.id, &later.request_id);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn managed_roots_resolve_git_markers_deduplicate_and_exclude_app_workspaces() {
        let (folder, registry) = workspace_fixture();
        fs::create_dir(folder.join("a/.git")).unwrap();
        fs::create_dir_all(folder.join("a/src/deep")).unwrap();
        fs::write(folder.join("b/.git"), "gitdir: elsewhere").unwrap();
        let manager = registry.create_manager(input(&folder.join("a"))).unwrap();
        let first = registry.create_owned(&manager.id, input(&folder.join("a/src/deep"))).unwrap();
        registry.create_owned(&manager.id, input(&folder.join("a"))).unwrap();
        registry.create_owned(&manager.id, input(&folder.join("b"))).unwrap();
        registry.create_owned(&manager.id, input(registry.config_root())).unwrap();
        let helper = registry.create_helper(&first.id, HelperPurpose::Diff, first.project_path.clone()).unwrap();
        let planner = registry.create_planner(&first.id).unwrap();
        assert!(registry.workspace_source_binding(&helper.id).is_err());
        assert!(registry.workspace_source_binding(&planner.id).is_err());
        assert!(registry.workspace_source_binding(&manager.id).is_err());
        assert!(!registry.is_real_project_folder(&registry.config_root().to_string_lossy()));
        let roots = registry.managed_repository_roots(&manager.id).unwrap();
        assert_eq!(roots.len(), 2);
        for expected in [folder.join("a"), folder.join("b")] {
            let expected = crate::setup::canonical(&expected).unwrap().to_string_lossy().into_owned();
            assert!(roots.iter().any(|root| same_folder(root, &expected)));
        }
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn managed_project_folders_count_definitions_before_root_deduplication() {
        let (folder, registry) = workspace_fixture();
        fs::create_dir(folder.join("a/.git")).unwrap();
        fs::create_dir_all(folder.join("a/src")).unwrap();
        fs::create_dir_all(folder.join("a/docs")).unwrap();
        let manager = registry.create_manager(input(&folder.join("b"))).unwrap();
        let first = registry.create_owned(&manager.id, input(&folder.join("a/src"))).unwrap();
        registry.create_helper(&first.id, HelperPurpose::Diff, first.project_path.clone()).unwrap();
        registry.create_planner(&first.id).unwrap();
        registry.create_owned(&manager.id, input(registry.config_root())).unwrap();
        registry.create(input(&folder.join("a/docs"))).unwrap();
        let canonical = |path: PathBuf| crate::setup::canonical(&path).unwrap().to_string_lossy().into_owned();
        let one = registry.managed_project_folders(&manager.id).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].0, first.id);
        assert!(same_folder(&one[0].1, &canonical(folder.join("a/src"))));
        let second = registry.create_owned(&manager.id, input(&folder.join("a/docs"))).unwrap();
        let two = registry.managed_project_folders(&manager.id).unwrap();
        assert_eq!(two.iter().map(|(id, _)| id.as_str()).collect::<std::collections::BTreeSet<_>>(),
            [first.id.as_str(), second.id.as_str()].into_iter().collect());
        assert_eq!(registry.managed_repository_roots(&manager.id).unwrap().len(), 1);
        assert_eq!(registry.managed_project_folders(&first.id).unwrap_err(), "unknown manager");
        assert_eq!(registry.ordinary_project_folders().len(), 3);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn ordinary_agents_commit_their_own_workspace_change_while_helpers_and_plans_cannot() {
        let (folder, registry) = workspace_fixture();
        let manager = registry.create_manager(input(&folder.join("a"))).unwrap();
        let worker = registry.create_owned(&manager.id, input(&folder.join("a"))).unwrap();
        let before = registry.workspace_metadata(&worker.id).unwrap();
        let commit = workspace_commit(&registry, &worker.id, &folder.join("b"), None);
        registry.begin_workspace_change(&worker.id, &commit.request_id, &commit.expected).unwrap();
        let mut bound = commit.clone();
        bound.source = Some(WorkspaceSourceBinding::Path { source_id: worker.id.clone(), canonical_path: worker.project_path.clone() });
        assert_eq!(registry.commit_workspace_change(&bound).unwrap_err(), "invalid_workspace_binding");
        let mut managed = commit.clone();
        managed.target_folder = registry.config_root().to_string_lossy().into_owned();
        assert!(registry.commit_workspace_change(&managed).unwrap_err().contains("app-managed"));
        let lease = registry.commit_workspace_change(&commit).unwrap();
        registry.finish_workspace_change(&worker.id, &commit.request_id);
        assert_eq!((lease.generation, lease.session_id.clone(), lease.ownership_epoch, lease.manager_id.clone()),
            (before.lease.generation, before.lease.session_id.clone(), before.lease.ownership_epoch, before.lease.manager_id.clone()));
        assert_eq!(lease.manager_id.as_deref(), Some(manager.id.as_str()));
        assert_eq!(lease.workspace_epoch, before.workspace_epoch + 1);
        let target = crate::setup::canonical(&folder.join("b")).unwrap().to_string_lossy().into_owned();
        assert!(same_folder(&registry.workspace_metadata(&worker.id).unwrap().definition.input.project_path, &target));
        // A late identity report from before the move cannot move the agent back.
        registry.apply_identity(&worker.id, 0, &json!({"cwd":folder.join("a"),"sessionId":null,"workspaceEpoch":0})).unwrap();
        assert!(same_folder(&registry.workspace_metadata(&worker.id).unwrap().definition.input.project_path, &target));
        let helper = registry.create_helper(&worker.id, HelperPurpose::Diff, worker.project_path.clone()).unwrap();
        let planner = registry.create_planner(&worker.id).unwrap();
        for id in [&helper.id, &planner.id] {
            let metadata = registry.workspace_metadata(id).unwrap();
            assert!(!may_change_directory(&metadata.definition));
            assert_eq!(registry.begin_workspace_change(id, "request", &metadata.lease).unwrap_err(), "Only agent working directories can be changed");
        }
        assert!(may_change_directory(&registry.workspace_metadata(&manager.id).unwrap().definition));
        assert!(may_change_directory(&registry.workspace_metadata(&worker.id).unwrap().definition));
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn bootstrap_creates_stopped_singletons_and_recreates_only_managed_folders() {
        let (folder, registry) = fixture();
        let first = registry.ensure_coordinators(&[]).unwrap();
        assert_eq!(registry.snapshot().len(), 2);
        assert_ne!(first.manager.project_path, first.architect.project_path);
        for row in [&first.manager, &first.architect] {
            assert_eq!(row.status, "stopped");
            assert_eq!(row.generation, 0);
            assert!(Path::new(&row.project_path).is_dir());
            assert!(row.session_path.is_none());
        }
        fs::remove_dir(&first.manager.project_path).unwrap();
        let second = registry.ensure_coordinators(&[folder.to_string_lossy().into()]).unwrap();
        assert_eq!(first.manager.id, second.manager.id);
        assert_eq!(first.architect.id, second.architect.id);
        assert_eq!(first.manager.project_path, second.manager.project_path);
        assert!(Path::new(&second.manager.project_path).is_dir());
        fs::remove_dir(&second.manager.project_path).unwrap();
        fs::write(&second.manager.project_path, "conflict").unwrap();
        assert!(registry.ensure_coordinators(&[]).is_err());
        fs::remove_file(&second.manager.project_path).unwrap();
        fs::remove_dir(&second.architect.project_path).unwrap();
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn bootstrap_uses_real_candidates_and_preserves_existing_configuration() {
        let (folder, registry) = fixture();
        let real = std::env::temp_dir().join(format!("omp-bootstrap-real-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&real).unwrap();
        let manager = registry.create_manager(CreateInput {
            profile: "isolated".into(), model: "@preserve".into(), ..input(&real)
        }).unwrap();
        let first = registry.ensure_coordinators(&[
            folder.to_string_lossy().into(), real.to_string_lossy().into(),
        ]).unwrap();
        assert_eq!(first.manager.id, manager.id);
        assert_eq!(first.manager.profile, "isolated");
        assert_eq!(first.manager.model, "@preserve");
        assert_eq!(first.architect.project_path, manager.project_path);
        fs::remove_dir(&real).unwrap();
        let second = registry.ensure_coordinators(&[]).unwrap();
        assert_eq!(second.manager.project_path, manager.project_path);
        assert!(!real.exists());
        assert_eq!(registry.snapshot().len(), 2);
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn registry_restores_configuration_without_launching() {
        let (folder, registry) = fixture();
        let overlay = folder.join("config.yml");
        fs::write(&overlay, "theme: {}\n").unwrap();
        let created = registry
            .create(CreateInput {
                profile: "isolated".into(),
                model: "@slow".into(),
                configs: vec![overlay.clone()],
                ..input(&folder)
            })
            .unwrap();
        let loaded = Registry::load(folder.join("instances.json"), folder.clone()).unwrap();
        let rows = loaded.snapshot();
        assert_eq!(rows[0].id, created.id);
        assert_eq!(rows[0].profile, "isolated");
        assert_eq!(rows[0].model, "@slow");
        assert_eq!(rows[0].configs, vec![overlay]);
        assert_eq!(rows[0].status, "stopped");
        assert_eq!(rows[0].generation, 0);
        assert!(loaded
            .create(CreateInput {
                project_path: folder.join("missing").to_string_lossy().into(),
                ..Default::default()
            })
            .is_err());
        assert_eq!(loaded.snapshot().len(), 1);
        loaded.remove(&created.id, Origin::Human).unwrap();
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn ownership_and_human_authority_persist_and_revoke() {
        let (folder, registry) = fixture();
        let manager = registry.create_manager(input(&folder)).unwrap();
        assert!(registry.create_manager(input(&folder)).is_err());
        let owned = registry.create_owned(&manager.id, input(&folder)).unwrap();
        let human = registry.create(input(&folder)).unwrap();
        assert!(registry
            .inspect(&human.id, Origin::Manager(&manager.id))
            .is_err());
        registry
            .set_manager_owner(&human.id, Some(&manager.id))
            .unwrap();
        registry
            .request_human(&owned.id, &manager.id, "Review execution".into(), Some("review".into()))
            .unwrap();
        assert_eq!(
            registry
                .lease(&owned.id, Origin::Manager(&manager.id), true)
                .err()
                .as_deref(),
            Some("human_intervention_pending")
        );
        assert!(registry.lease(&owned.id, Origin::Human, true).is_ok());
        registry.resolve_human_decision(&owned.id, "review");
        assert!(registry
            .lease(&owned.id, Origin::Manager(&manager.id), true)
            .is_ok());
        registry.set_manager_owner(&owned.id, None).unwrap();
        assert!(registry
            .inspect(&owned.id, Origin::Manager(&manager.id))
            .is_err());
        let loaded = Registry::load(folder.join("instances.json"), folder.clone()).unwrap();
        assert_eq!(
            loaded
                .inspect(&human.id, Origin::Human)
                .unwrap()
                .manager_id
                .as_deref(),
            Some(manager.id.as_str())
        );
        loaded.remove(&manager.id, Origin::Human).unwrap();
        assert!(loaded
            .snapshot()
            .iter()
            .all(|r| r.manager_id.is_none() && r.status == "stopped"));
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn legacy_definitions_are_unowned() {
        let (folder, registry) = fixture();
        let value = json!({"version":1,"instances":[{"id":"old","name":"Old","projectPath":folder,"profile":"","configs":[],"model":"","resumeSessionId":null,"sessionPath":null}]});
        fs::write(&registry.file, serde_json::to_vec(&value).unwrap()).unwrap();
        let loaded = Registry::load(registry.file.clone(), folder.clone()).unwrap();
        let row = loaded.snapshot().remove(0);
        assert_eq!(row.kind, InstanceKind::Instance);
        assert_eq!(row.manager_id, None);
        assert_eq!(row.status, "stopped");
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn new_rows_freeze_defaults_without_changing_existing_sessions() {
        let (folder, registry) = fixture();
        let old = registry.create(input(&folder)).unwrap();
        let a = registry
            .global_config
            .publish("modelRoles:\n  default: first\n")
            .unwrap();
        let manager = registry.create_manager(input(&folder)).unwrap();
        let overlay = folder.join("override.yml");
        fs::write(&overlay, "modelRoles:\n  default: override\n").unwrap();
        let worker = registry
            .create_owned(
                &manager.id,
                CreateInput {
                    configs: vec![overlay.clone()],
                    ..input(&folder)
                },
            )
            .unwrap();
        assert_eq!(worker.configs, vec![a.clone(), overlay]);
        assert_eq!(manager.configs, vec![a.clone()]);
        let b = registry
            .global_config
            .publish("modelRoles:\n  default: second\n")
            .unwrap();
        let fresh = registry.create(input(&folder)).unwrap();
        assert_eq!(fresh.configs, vec![b]);
        let skipped = registry
            .create(CreateInput {
                use_global_config: Some(false),
                ..input(&folder)
            })
            .unwrap();
        assert!(skipped.configs.is_empty());
        let loaded = Registry::load(folder.join("instances.json"), folder.clone()).unwrap();
        assert_eq!(
            loaded.inspect(&worker.id, Origin::Human).unwrap().configs,
            worker.configs
        );
        assert!(loaded
            .inspect(&old.id, Origin::Human)
            .unwrap()
            .configs
            .is_empty());
        assert!(loaded.snapshot().iter().all(|row| row.status == "stopped"));
        fs::remove_file(&a).unwrap();
        assert_eq!(
            loaded.inspect(&manager.id, Origin::Human).unwrap().configs,
            vec![a]
        );
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn start_reports_a_missing_agent_file_before_launching() {
        let (folder, registry) = fixture();
        let missing = folder.join("gone/AGENTS.md");
        let value = json!({"version":1,"instances":[{"id":"persona","name":"P","projectPath":folder,"profile":"","configs":[],"model":"","resumeSessionId":null,"sessionPath":null,"agentFile":missing}]});
        fs::write(&registry.file, serde_json::to_vec(&value).unwrap()).unwrap();
        let loaded = Registry::load(registry.file.clone(), folder.clone()).unwrap();
        let error = loaded.start("persona", Origin::Human).unwrap_err();
        assert!(error.starts_with("Agent instructions file not found"), "{error}");
        let row = loaded.snapshot().remove(0);
        assert_eq!(row.last_error.as_deref(), Some(error.as_str()));
        assert_eq!(row.generation, 0);
        assert_eq!(row.agent_file, Some(missing));
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn named_profiles_skip_the_app_default_unless_asked() {
        let (folder, registry) = fixture();
        let default = registry
            .global_config
            .publish("modelRoles:\n  default: first\n")
            .unwrap();
        let named = |use_global_config| CreateInput {
            profile: "plan".into(),
            use_global_config,
            ..input(&folder)
        };
        assert!(registry.create(named(None)).unwrap().configs.is_empty());
        assert_eq!(
            registry.create(named(Some(true))).unwrap().configs,
            vec![default.clone()]
        );
        assert_eq!(registry.create(input(&folder)).unwrap().configs, vec![default.clone()]);
        let explicit_default = CreateInput {
            profile: "default".into(),
            ..input(&folder)
        };
        assert_eq!(registry.create(explicit_default).unwrap().configs, vec![default]);
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn a_plan_agent_mirrors_its_target_and_is_unique() {
        let (folder, registry) = fixture();
        let overlay = folder.join("config.yml");
        fs::write(&overlay, "theme: {}\n").unwrap();
        let target = registry
            .create(CreateInput { profile: "isolated".into(), model: "@slow".into(), configs: vec![overlay.clone()], ..input(&folder) })
            .unwrap();
        let planner = registry.create_planner(&target.id).unwrap();
        assert_eq!(planner.plan_target.as_deref(), Some(target.id.as_str()));
        assert_eq!((planner.name.as_str(), planner.profile.as_str(), planner.model.as_str()), ("Plan — Project", "isolated", "@slow"));
        assert_eq!(planner.configs, vec![overlay]);
        assert_eq!(planner.manager_id, None);
        let expected = fs::canonicalize(registry.plan_folder(&target.id)).unwrap();
        assert_eq!(planner.project_path, expected.to_string_lossy().trim_start_matches("\\\\?\\"));
        assert_eq!(registry.planner_for(&target.id), Some(planner.id.clone()));
        assert_eq!(registry.plan_target(&planner.id).as_deref(), Some(target.id.as_str()));
        assert_eq!(registry.plan_target(&target.id), None);
        assert_eq!(registry.create_planner(&target.id).err().as_deref(), Some("A plan agent already exists for this session"));
        assert_eq!(registry.create_planner(&planner.id).err().as_deref(), Some("unknown instance"));
        let loaded = Registry::load(folder.join("instances.json"), folder.clone()).unwrap();
        assert_eq!(loaded.planner_for(&target.id), Some(planner.id));
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn a_plan_is_the_live_todo_list_then_the_native_plan_then_the_last_saved_one() {
        let (folder, registry) = fixture();
        let target = registry.create(input(&folder)).unwrap();
        let kind = || registry.inspect(&target.id, Origin::Human).unwrap().plan_kind;
        let plan = || registry.plan(&target.id, Origin::Human).unwrap();
        let fallback = registry.plan_folder(&target.id).join("target-plan.md");
        assert_eq!((kind(), plan()), (None, None));
        assert_eq!(registry.plan_file(&target.id).unwrap(), fallback);
        let saved = folder.join("saved.md");
        fs::write(&saved, "# Saved").unwrap();
        registry.records.lock().unwrap()[0].saved_plan = Some(saved.clone());
        assert_eq!((kind().as_deref(), plan().as_deref()), (Some("saved"), Some("# Saved")));
        assert_eq!(registry.plan_file(&target.id).unwrap(), saved);
        let native = folder.join("native-plan.md");
        fs::write(&native, "# Native").unwrap();
        registry.records.lock().unwrap()[0].reader.state.plan_path = Some(native.clone());
        assert_eq!((kind().as_deref(), plan().as_deref()), (Some("file"), Some("# Native")));
        assert_eq!(registry.plan_file(&target.id).unwrap(), native);
        {
            let mut records = registry.records.lock().unwrap();
            let state = &mut records[0].reader.state;
            state.todo_total = 2;
            state.todo_done = 1;
            state.phases = vec![session::Phase {
                name: "Work".into(),
                tasks: vec![
                    session::PlanTask { content: "a".into(), status: "completed".into() },
                    session::PlanTask { content: "b".into(), status: "in_progress".into() },
                ],
            }];
        }
        assert_eq!((kind().as_deref(), plan().as_deref()), (Some("todo"), Some("# Work\n- [x] a\n- [~] b\n")));
        assert_eq!(registry.plan_file(&target.id).unwrap(), fallback);
        registry.records.lock().unwrap()[0].reader.state.todo_done = 2;
        assert_eq!((kind(), plan()), (None, None));
        assert!(registry.plan_file("missing").is_err());
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn a_session_replaced_after_plan_approval_still_finds_the_plan_saved_before_it() {
        use std::io::Write;
        let (folder, registry) = fixture();
        let target = registry.create(input(&folder)).unwrap();
        let cwd = target.project_path.clone();
        let bucket = session::agent_dir(&folder, "").join("sessions").join("bucket");
        fs::create_dir_all(&bucket).unwrap();
        let journal = |name: &str, lines: Vec<Value>| {
            let path = bucket.join(format!("{name}.jsonl"));
            fs::write(&path, lines.iter().map(|line| format!("{line}\n")).collect::<String>()).unwrap();
            path
        };
        let old = journal("old", vec![json!({"type":"session","id":"old","cwd":cwd}), json!({"type":"mode_change","mode":"plan","data":{"planFilePath":"local://saved-plan.md"}})]);
        let plan_file = old.with_extension("").join("local/saved-plan.md");
        fs::create_dir_all(plan_file.parent().unwrap()).unwrap();
        fs::write(&plan_file, "# Saved plan").unwrap();
        let current = journal("current", vec![json!({"type":"session","id":"current","cwd":cwd})]);
        registry.records.lock().unwrap()[0].definition.session_path = Some(current.clone());
        let kind = || registry.inspect(&target.id, Origin::Human).unwrap().plan_kind;
        registry.refresh().unwrap();
        assert_eq!(kind().as_deref(), Some("saved"));
        assert_eq!(registry.plan(&target.id, Origin::Human).unwrap().as_deref(), Some("# Saved plan"));
        let todo = |status: &str| format!("{}\n", json!({"type":"message","message":{"role":"toolResult","toolName":"todo","details":{"phases":[{"name":"test","tasks":[{"content":"step","status":status}]}]}}}));
        let append = |text: String| fs::OpenOptions::new().append(true).open(&current).unwrap().write_all(text.as_bytes()).unwrap();
        append(todo("in_progress"));
        registry.refresh().unwrap();
        assert_eq!(kind().as_deref(), Some("todo"));
        append(todo("completed"));
        registry.refresh().unwrap();
        assert_eq!(kind(), None);
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn removing_a_session_removes_its_plan_agent_and_files() {
        let (folder, registry) = fixture();
        let kept = registry.create(input(&folder)).unwrap();
        let target = registry.create(input(&folder)).unwrap();
        let planner = registry.create_planner(&target.id).unwrap();
        crate::plan::write_atomic(&registry.plan_folder(&target.id).join("target-plan.md"), "x").unwrap();
        registry.remove(&target.id, Origin::Human).unwrap();
        let ids: Vec<String> = registry.snapshot().into_iter().map(|row| row.id).collect();
        assert_eq!(ids, vec![kept.id]);
        assert!(!ids.contains(&planner.id));
        assert!(!registry.plan_folder(&target.id).exists());
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn helper_agents_are_hidden_links_to_one_project_per_purpose_and_go_with_it() {
        let (folder, registry) = fixture();
        let target = registry.create(input(&folder)).unwrap();
        let diff = registry.create_helper(&target.id, HelperPurpose::Diff, "C:/repo".into()).unwrap();
        assert_eq!(diff.helper, Some(HelperLink { target: target.id.clone(), purpose: HelperPurpose::Diff, repo: "C:/repo".into() }));
        assert_eq!(diff.name, "Diff helper — Project");
        assert_eq!(diff.plan_target, None);
        let expected = fs::canonicalize(registry.helper_folder(&target.id, HelperPurpose::Diff)).unwrap();
        assert_eq!(diff.project_path, expected.to_string_lossy().trim_start_matches("\\\\?\\"), "its cwd is app-owned, not the project");
        assert_eq!(registry.create_helper(&target.id, HelperPurpose::Diff, "C:/repo".into()).err().as_deref(), Some("A diff helper already exists for this session"));
        let git = registry.create_helper(&target.id, HelperPurpose::Git, "C:/repo".into()).unwrap();
        assert_eq!(registry.helper_for(&target.id, HelperPurpose::Git), Some(git.id.clone()));
        assert_eq!(registry.helper_for(&target.id, HelperPurpose::Diff), Some(diff.id.clone()));
        // A helper is not a session: it can have neither helpers nor a plan agent.
        assert_eq!(registry.create_helper(&diff.id, HelperPurpose::Git, "C:/repo".into()).err().as_deref(), Some("unknown instance"));
        assert_eq!(registry.create_planner(&diff.id).err().as_deref(), Some("unknown instance"));
        let loaded = Registry::load(folder.join("instances.json"), folder.clone()).unwrap();
        assert_eq!(loaded.helper_for(&target.id, HelperPurpose::Git), Some(git.id.clone()));
        assert_eq!(loaded.inspect(&git.id, Origin::Human).unwrap().helper.map(|link| link.repo), Some("C:/repo".into()));
        registry.remove(&target.id, Origin::Human).unwrap();
        assert!(registry.snapshot().is_empty(), "removing the project removes its helpers");
        assert!(!folder.join("helpers").join(&target.id).exists());
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn saved_todo_revisions_are_visible_before_handoff_and_every_change_has_a_restorable_backup() {
        let (folder, registry) = fixture();
        let target = registry.create(input(&folder)).unwrap();
        let journal = folder.join("session.jsonl");
        let header = json!({"type":"session","id":"s","cwd":folder});
        let todo = |content: &str| json!({"type":"message","message":{"role":"toolResult","toolName":"todo","details":{
            "phases":[{"name":"test","tasks":[{"content":content,"status":"in_progress"}]}]}}});
        fs::write(&journal, format!("{header}\n{}\n", todo("wait 60 seconds"))).unwrap();
        registry.apply_identity(&target.id, 0, &json!({"sessionFile":journal,"materialized":true,"sessionId":"s"})).unwrap();
        registry.refresh().unwrap();
        let original = registry.plan(&target.id, Origin::Human).unwrap().unwrap();
        let backup = registry.revise_plan(&target.id, "# test\n- [ ] wait 10 seconds").unwrap();
        // read_plan's source is the pending revision, not the unchanged worker todos.
        assert_eq!(registry.plan(&target.id, Origin::Human).unwrap().as_deref(), Some("# test\n- [ ] wait 10 seconds"));
        let version = crate::plan::read_backup(&registry.plan_folder(&target.id), &backup).unwrap();
        assert_eq!(version.text, original);
        let loaded = Registry::load(folder.join("instances.json"), folder.clone()).unwrap();
        loaded.refresh().unwrap();
        assert_eq!(loaded.plan(&target.id, Origin::Human).unwrap(), registry.plan(&target.id, Origin::Human).unwrap());
        registry.revise_plan(&target.id, &version.text).unwrap();
        assert_eq!(registry.plan(&target.id, Origin::Human).unwrap().as_deref(), Some(original.as_str()));
        let backups = crate::plan::list_backups(&registry.plan_folder(&target.id)).unwrap();
        assert_eq!(backups.len(), 2);
        assert!(backups.iter().any(|b| b.text == "# test\n- [ ] wait 10 seconds"));
        assert_eq!(crate::plan::read_backup(&registry.plan_folder(&target.id), &backup).unwrap().text, original);
        assert!(crate::plan::read_backup(&registry.plan_folder(&target.id), "../session").is_err());
        // After an approved handoff, new worker todos become the live plan again.
        let revision = registry.plan_revision(&target.id).unwrap().unwrap();
        registry.mark_plan_handoff(&target.id, &revision.id).unwrap();
        use std::io::Write;
        fs::OpenOptions::new().append(true).open(&journal).unwrap().write_all(format!("{}\n", todo("new worker step")).as_bytes()).unwrap();
        registry.refresh().unwrap();
        assert!(registry.plan(&target.id, Origin::Human).unwrap().unwrap().contains("new worker step"));
        // A new OMP session must not inherit an old draft merely because its registry id is the same.
        let next = folder.join("next.jsonl");
        fs::write(&next, format!("{header}\n{}\n", todo("different session"))).unwrap();
        registry.apply_identity(&target.id, 0, &json!({"sessionFile":next,"materialized":true,"sessionId":"next"})).unwrap();
        registry.refresh().unwrap();
        assert!(registry.plan_revision(&target.id).unwrap().is_none());
        assert!(registry.plan(&target.id, Origin::Human).unwrap().unwrap().contains("different session"));
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn a_backup_failure_preserves_the_native_plan_and_restoration_preserves_both_versions() {
        let (folder, registry) = fixture();
        let target = registry.create(input(&folder)).unwrap();
        let native = folder.join("native.md");
        fs::write(&native, "# Original").unwrap();
        registry.records.lock().unwrap()[0].reader.state.plan_path = Some(native.clone());
        let backup_dir = registry.plan_folder(&target.id).join("backups");
        fs::create_dir_all(backup_dir.parent().unwrap()).unwrap();
        fs::write(&backup_dir, "not a directory").unwrap();
        assert!(registry.revise_plan(&target.id, "# New").is_err());
        assert_eq!(fs::read_to_string(&native).unwrap(), "# Original");
        assert!(registry.plan_revision(&target.id).unwrap().is_none());
        fs::remove_file(&backup_dir).unwrap();
        let backup_id = registry.revise_plan(&target.id, "# New").unwrap();
        let backup = crate::plan::read_backup(&registry.plan_folder(&target.id), &backup_id).unwrap();
        assert_eq!(backup.text, "# Original");
        registry.revise_plan(&target.id, &backup.text).unwrap();
        assert_eq!(fs::read_to_string(&native).unwrap(), "# Original");
        assert!(crate::plan::list_backups(&registry.plan_folder(&target.id)).unwrap().iter().any(|b| b.text == "# New"));
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn forgetting_the_session_makes_the_next_launch_a_new_one() {
        let (folder, registry) = fixture();
        registry.create(input(&folder)).unwrap();
        let file = folder.join("old.jsonl");
        fs::write(&file, "{}\n").unwrap();
        let mut records = registry.records.lock().unwrap();
        records[0].definition.session_path = Some(file);
        records[0].materialized = true;
        records[0].native_session_id = Some("old".into());
        records[0].forget_session();
        assert!(records[0].definition.session_path.is_none());
        assert!(!records[0].materialized);
        assert!(records[0].native_session_id.is_none());
        drop(records);
        fs::remove_dir_all(folder).unwrap();
    }
}
