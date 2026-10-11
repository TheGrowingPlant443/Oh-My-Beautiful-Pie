use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::config::GlobalConfigStore;
use crate::manager::ManagerHub;
use crate::{pty, session};

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum InstanceKind {
    #[default]
    Instance,
    Manager,
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
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HumanRequest {
    pub reason: String,
    pub decision_id: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub id: String,
    pub name: String,
    pub project_path: String,
    pub profile: String,
    pub configs: Vec<PathBuf>,
    pub model: String,
    pub status: String,
    pub activity: String,
    pub task: Option<String>,
    pub subagents: Vec<session::Subagent>,
    pub plan_path: Option<PathBuf>,
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
    pub bridge_ready: bool,
    pub pending_decision_count: usize,
    pub human_request: Option<HumanRequest>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlLease {
    pub generation: u64,
    pub session_id: Option<String>,
    pub ownership_epoch: u64,
    pub manager_id: Option<String>,
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
    pending_decision_count: usize,
    human_request: Option<HumanRequest>,
    ownership_epoch: u64,
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
            pending_decision_count: 0,
            human_request: None,
            ownership_epoch: 0,
        }
    }
    fn lease(&self) -> ControlLease {
        ControlLease {
            generation: self.generation,
            session_id: self.native_session_id.clone(),
            ownership_epoch: self.ownership_epoch,
            manager_id: self.definition.manager_id.clone(),
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
            configs: input.configs.clone(),
            model: input.model.clone(),
            status: status.into(),
            activity: activity.into(),
            task: state.task.clone(),
            subagents: state.subagents.clone(),
            plan_path: state.plan_path.clone(),
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
            bridge_ready: self.bridge_ready,
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

pub struct Registry {
    records: Mutex<Vec<Record>>,
    pub home: PathBuf,
    file: PathBuf,
    hub: OnceLock<Arc<ManagerHub>>,
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

impl Registry {
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
        self.create_definition(input, InstanceKind::Instance, None)
    }
    pub fn create_manager(&self, mut input: CreateInput) -> Result<Instance, String> {
        input.name = "The Manager".into();
        self.create_definition(input, InstanceKind::Manager, None)
    }
    pub fn create_owned(&self, manager_id: &str, input: CreateInput) -> Result<Instance, String> {
        self.create_definition(input, InstanceKind::Instance, Some(manager_id.into()))
    }
    fn create_definition(
        &self,
        mut input: CreateInput,
        kind: InstanceKind,
        manager_id: Option<String>,
    ) -> Result<Instance, String> {
        let cwd = fs::canonicalize(&input.project_path).map_err(|e| e.to_string())?;
        if !cwd.is_dir() {
            return Err("project folder is not a directory".into());
        }
        if input.profile.contains(['/', '\\']) || input.profile == "." || input.profile == ".." {
            return Err("invalid profile name".into());
        }
        input.project_path = cwd.to_string_lossy().trim_start_matches("\\\\?\\").into();
        if input.name.trim().is_empty() {
            input.name = cwd.file_name().unwrap_or_default().to_string_lossy().into();
        }
        if input.profile == "default" {
            input.profile.clear();
        }
        if input.use_global_config.unwrap_or(true) {
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
        };
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        if kind == InstanceKind::Manager && records.iter().any(|r| r.definition.kind == kind) {
            return Err("The Manager already exists".into());
        }
        if let Some(owner) = &definition.manager_id {
            if !records
                .iter()
                .any(|r| r.definition.id == *owner && r.definition.kind == InstanceKind::Manager)
            {
                return Err("not_controlled".into());
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
        Ok(())
    }
    pub fn apply_identity(&self, id: &str, generation: u64, payload: &Value) -> Result<(), String> {
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        if records[i].generation != generation {
            return Err("stale_generation".into());
        }
        let path = payload["sessionFile"].as_str().map(PathBuf::from);
        let dirty = records[i].definition.session_path != path;
        if dirty {
            records[i].reader = session::TailReader::default();
            records[i].definition.session_path = path;
        }
        records[i].native_session_id = payload["sessionId"].as_str().map(String::from);
        records[i].materialized = payload["materialized"].as_bool().unwrap_or(false);
        records[i].bridge_ready = payload["supported"].as_bool().unwrap_or(true);
        let cwd = payload["cwd"].as_str().filter(|s| Path::new(s).is_dir());
        let cwd_dirty = cwd.is_some_and(|s| s != records[i].definition.input.project_path);
        if let Some(cwd) = cwd {
            records[i].definition.input.project_path = cwd.into();
        }
        if dirty || cwd_dirty {
            self.save(&records)?;
        }
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
    pub fn set_manager_owner(&self, id: &str, manager_id: Option<&str>) -> Result<(), String> {
        let lease = {
            let mut records = self.records.lock().map_err(|e| e.to_string())?;
            let i = index(&records, id)?;
            if records[i].definition.kind == InstanceKind::Manager {
                return Err("cannot hand over The Manager".into());
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
                reason,
                decision_id,
            });
        }
        Ok(())
    }
    pub fn return_to_manager(&self, id: &str) -> Result<(), String> {
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        records[i].human_request = None;
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
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        authorize(&records, i, origin, true)?;
        let record = &mut records[i];
        if record
            .process
            .as_mut()
            .is_some_and(|p| matches!(p.poll(), Ok(None)))
        {
            return Err("instance already running".into());
        }
        let hub = self.hub.get().ok_or("manager bridge not initialized")?;
        record.generation += 1;
        record.error = None;
        record.bridge_ready = false;
        record.pending_decision_count = 0;
        let launch = hub.launch(&record.definition, record.generation)?;
        match pty::Pty::spawn(&record.definition, &launch) {
            Ok(process) => {
                record.process = Some(process);
                Ok(())
            }
            Err(error) => {
                record.error = Some(error.clone());
                hub.invalidate(id, record.generation);
                Err(error)
            }
        }
    }
    pub fn stop(&self, id: &str, origin: Origin<'_>) -> Result<(), String> {
        let generation = {
            let mut records = self.records.lock().map_err(|e| e.to_string())?;
            let i = index(&records, id)?;
            authorize(&records, i, origin, true)?;
            if let Some(process) = &mut records[i].process {
                process.stop()?;
            }
            records[i].bridge_ready = false;
            records[i].generation
        };
        if let Some(hub) = self.hub.get() {
            hub.invalidate(id, generation);
        }
        Ok(())
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
    }
    pub fn remove(&self, id: &str, origin: Origin<'_>) -> Result<(), String> {
        self.stop(id, origin)?;
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
    pub fn plan(&self, id: &str, origin: Origin<'_>) -> Result<Option<String>, String> {
        let records = self.records.lock().map_err(|e| e.to_string())?;
        let i = index(&records, id)?;
        authorize(&records, i, origin, false)?;
        records[i]
            .reader
            .state
            .plan_path
            .as_ref()
            .map(fs::read_to_string)
            .transpose()
            .map_err(|e| e.to_string())
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
            .request_human(&owned.id, &manager.id, "Review execution".into(), None)
            .unwrap();
        assert_eq!(
            registry
                .lease(&owned.id, Origin::Manager(&manager.id), true)
                .err()
                .as_deref(),
            Some("human_intervention_pending")
        );
        assert!(registry.lease(&owned.id, Origin::Human, true).is_ok());
        registry.return_to_manager(&owned.id).unwrap();
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
}
