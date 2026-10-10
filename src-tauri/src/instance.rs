use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::{pty, session};

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
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Definition {
    pub id: String,
    #[serde(flatten)]
    pub input: CreateInput,
    pub session_path: Option<PathBuf>,
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
}

struct Record {
    definition: Definition,
    process: Option<pty::Pty>,
    reader: session::TailReader,
    started_at: u64,
    generation: u64,
    error: Option<String>,
}

impl Record {
    fn new(definition: Definition) -> Self {
        Self {
            definition,
            process: None,
            reader: session::TailReader::default(),
            started_at: 0,
            generation: 0,
            error: None,
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
                Ok(Some(code)) => exit_code = Some(code),
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
            records: Mutex::new(definitions.into_iter().map(Record::new).collect()),
            home,
            file,
        })
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

    pub fn create(&self, mut input: CreateInput) -> Result<Instance, String> {
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
        for config in &mut input.configs {
            if !config.is_absolute() {
                *config = cwd.join(&*config);
            }
            if !config.is_file() {
                return Err(format!("config not found: {}", config.display()));
            }
        }
        let path = match &input.resume_session_id {
            Some(target) => {
                let sessions = session::list(&self.home, &cwd, &input.profile);
                let found = sessions
                    .into_iter()
                    .find(|s| s.id == *target || s.path == Path::new(target))
                    .ok_or("session not found for this project/profile")?;
                Some(found.path)
            }
            None => None,
        };
        let definition = Definition {
            id: uuid::Uuid::new_v4().to_string(),
            input,
            session_path: path,
        };
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        records.push(Record::new(definition));
        if let Err(error) = self.save(&records) {
            records.pop();
            return Err(error);
        }
        let record = records.last_mut().ok_or("instance missing")?;
        if let Some(path) = &record.definition.session_path {
            record.reader.update(path)?;
        }
        Ok(record.view())
    }

    pub fn snapshot(&self) -> Vec<Instance> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
            .map(Record::view)
            .collect()
    }

    pub fn refresh(&self) -> Result<(), String> {
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let mut claimed: HashSet<PathBuf> = records
            .iter()
            .filter_map(|r| r.definition.session_path.clone())
            .collect();
        let mut dirty = false;
        for record in records.iter_mut() {
            let base = session::agent_dir(&self.home, &record.definition.input.profile);
            let crumb = base
                .join("terminal-sessions")
                .join(format!("wt-{}", record.definition.id));
            let breadcrumb = fs::read_to_string(crumb).ok().and_then(|text| {
                let mut lines = text.lines();
                lines.next()?;
                let path = PathBuf::from(lines.next()?);
                let fresh = lines.next() == Some("fresh");
                (path.is_file() || fresh).then_some((path, fresh))
            });
            let fresh = breadcrumb.as_ref().is_some_and(|(_, fresh)| *fresh);
            if record.started_at > 0 && record.process.is_some() {
                // Each Windows ConPTY gets its own native OMP breadcrumb, not the parent's WT_SESSION.
                let found = breadcrumb.map(|(path, _)| path).or_else(|| {
                    if record.definition.session_path.is_some() {
                        return None;
                    }
                    session::list(
                        &self.home,
                        Path::new(&record.definition.input.project_path),
                        &record.definition.input.profile,
                    )
                    .into_iter()
                    .rev()
                    .find(|s| s.modified_ms >= record.started_at && !claimed.contains(&s.path))
                    .map(|s| s.path)
                });
                if let Some(path) = found {
                    if record.definition.session_path.as_ref() != Some(&path) {
                        record.reader = session::TailReader::default();
                        record.definition.session_path = Some(path.clone());
                        claimed.insert(path);
                        dirty = true;
                    }
                }
            }
            if let Some(path) = &record.definition.session_path {
                if fresh && !path.is_file() {
                    continue;
                }
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
        if dirty {
            self.save(&records)?;
        }
        Ok(())
    }

    pub fn start(&self, id: &str) -> Result<(), String> {
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let record = records
            .iter_mut()
            .find(|r| r.definition.id == id)
            .ok_or("unknown instance")?;
        if record
            .process
            .as_mut()
            .is_some_and(|p| matches!(p.poll(), Ok(None)))
        {
            return Err("instance already running".into());
        }
        record.started_at = session::now_ms();
        record.generation += 1;
        record.error = None;
        match pty::Pty::spawn(&record.definition) {
            Ok(process) => {
                record.process = Some(process);
                Ok(())
            }
            Err(error) => {
                record.error = Some(error.clone());
                Err(error)
            }
        }
    }

    pub fn stop(&self, id: &str) -> Result<(), String> {
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let record = records
            .iter_mut()
            .find(|r| r.definition.id == id)
            .ok_or("unknown instance")?;
        if let Some(process) = &mut record.process {
            process.stop()?;
        }
        Ok(())
    }

    pub fn shutdown(&self) {
        let ids: Vec<String> = self.snapshot().into_iter().map(|i| i.id).collect();
        for id in ids {
            if let Err(e) = self.stop(&id) {
                eprintln!("stop {id}: {e}");
            }
        }
    }

    pub fn remove(&self, id: &str) -> Result<(), String> {
        self.stop(id)?;
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let index = records
            .iter()
            .position(|r| r.definition.id == id)
            .ok_or("unknown instance")?;
        let record = records.remove(index);
        if let Err(error) = self.save(&records) {
            records.insert(index, record);
            return Err(error);
        }
        Ok(())
    }

    pub fn attach(&self, id: &str, channel: Channel<InvokeResponseBody>) -> Result<(), String> {
        let records = self.records.lock().map_err(|e| e.to_string())?;
        let record = records
            .iter()
            .find(|r| r.definition.id == id)
            .ok_or("unknown instance")?;
        record
            .process
            .as_ref()
            .ok_or("instance not started")?
            .attach(channel)
    }

    pub fn write(&self, id: &str, data: &str) -> Result<(), String> {
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let record = records
            .iter_mut()
            .find(|r| r.definition.id == id)
            .ok_or("unknown instance")?;
        record
            .process
            .as_mut()
            .ok_or("instance not running")?
            .write(data)
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), String> {
        let mut records = self.records.lock().map_err(|e| e.to_string())?;
        let record = records
            .iter_mut()
            .find(|r| r.definition.id == id)
            .ok_or("unknown instance")?;
        record
            .process
            .as_mut()
            .ok_or("instance not running")?
            .resize(cols, rows)
    }

    pub fn plan(&self, id: &str) -> Result<Option<String>, String> {
        let records = self.records.lock().map_err(|e| e.to_string())?;
        let record = records
            .iter()
            .find(|r| r.definition.id == id)
            .ok_or("unknown instance")?;
        record
            .reader
            .state
            .plan_path
            .as_ref()
            .map(fs::read_to_string)
            .transpose()
            .map_err(|e| e.to_string())
    }

    pub fn transcript_path(&self, id: &str, child: &str) -> Result<(PathBuf, CreateInput), String> {
        let records = self.records.lock().map_err(|e| e.to_string())?;
        let record = records
            .iter()
            .find(|r| r.definition.id == id)
            .ok_or("unknown instance")?;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_restores_configuration_without_launching() {
        let folder =
            std::env::temp_dir().join(format!("omp-manager-registry-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&folder).unwrap();
        let overlay = folder.join("config.yml");
        fs::write(&overlay, "theme: {}\n").unwrap();
        let file = folder.join("instances.json");
        let registry = Registry::load(file.clone(), folder.clone()).unwrap();
        let input = CreateInput {
            name: "Project".into(),
            project_path: folder.to_string_lossy().into(),
            profile: "isolated".into(),
            model: "@slow".into(),
            configs: vec![overlay.clone()],
            resume_session_id: None,
        };
        let created = registry.create(input).unwrap();
        let loaded = Registry::load(file, folder.clone()).unwrap();
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
        loaded.remove(&created.id).unwrap();
        fs::remove_dir_all(folder).unwrap();
    }
}
