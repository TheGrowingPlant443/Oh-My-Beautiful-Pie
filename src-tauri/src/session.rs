use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub modified_ms: u64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Subagent {
    pub id: String,
    pub name: String,
    pub task: Option<String>,
    pub status: String,
    pub agent: Option<String>,
    pub path: PathBuf,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanTask {
    pub content: String,
    pub status: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Phase {
    pub name: String,
    pub tasks: Vec<PlanTask>,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    pub title: Option<String>,
    pub task: Option<String>,
    pub todo_done: usize,
    pub todo_total: usize,
    pub plan_path: Option<PathBuf>,
    pub subagents: Vec<Subagent>,
    pub phases: Vec<Phase>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn agent_dir(home: &Path, profile: &str) -> PathBuf {
    let root = home.join(
        std::env::var_os("PI_CONFIG_DIR")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ".omp".into()),
    );
    if profile.is_empty() || profile == "default" {
        let inherited = std::env::var("OMP_PROFILE")
            .or_else(|_| std::env::var("PI_PROFILE"))
            .unwrap_or_default();
        if let Some(path) = std::env::var_os("PI_CODING_AGENT_DIR").map(PathBuf::from) {
            let inherited_path = root.join("profiles").join(&inherited).join("agent");
            if inherited.is_empty() || !same_path(&path, &inherited_path) {
                return path;
            }
        }
        root.join("agent")
    } else {
        root.join("profiles").join(profile).join("agent")
    }
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    let normalize = |p: &Path| {
        let p = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let text = p.to_string_lossy().replace('\\', "/");
        let text = text
            .trim_start_matches("//?/")
            .trim_end_matches('/')
            .to_string();
        if cfg!(windows) {
            text.to_lowercase()
        } else {
            text
        }
    };
    normalize(a) == normalize(b)
}

pub fn header(path: &Path) -> Option<(Value, Option<String>)> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(8192)
        .read_to_end(&mut bytes)
        .ok()?;
    let mut title = None;
    for line in bytes.split(|b| *b == b'\n') {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        match value["type"].as_str() {
            Some("title") => {
                title = value["title"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(String::from)
            }
            Some("session") => return Some((value, title)),
            _ => {}
        }
    }
    None
}

pub fn list(home: &Path, project: &Path, profile: &str) -> Vec<SessionInfo> {
    let mut sessions = Vec::new();
    let root = agent_dir(home, profile).join("sessions");
    let Ok(buckets) = fs::read_dir(root) else {
        return sessions;
    };
    for bucket in buckets.flatten() {
        let Ok(files) = fs::read_dir(bucket.path()) else {
            continue;
        };
        for entry in files.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Some((value, title)) = header(&path) else {
                continue;
            };
            let Some(cwd) = value["cwd"].as_str() else {
                continue;
            };
            if !same_path(Path::new(cwd), project) {
                continue;
            }
            let Some(id) = value["id"].as_str() else {
                continue;
            };
            let modified_ms = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            sessions.push(SessionInfo {
                id: id.into(),
                title: title
                    .or_else(|| value["title"].as_str().map(String::from))
                    .unwrap_or_else(|| "Untitled session".into()),
                path,
                modified_ms,
            });
        }
    }
    sessions.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms));
    sessions
}

pub fn local_path(session: &Path, uri: &str) -> Option<PathBuf> {
    if let Some(relative) = uri.strip_prefix("local://") {
        let relative = Path::new(relative);
        if relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return None;
        }
        Some(session.with_extension("").join("local").join(relative))
    } else {
        let path = PathBuf::from(uri);
        path.is_absolute().then_some(path)
    }
}

/// The session's todo list as markdown: one `#` heading per phase, one checkbox line per task
/// (`[x]` completed, `[~]` in progress, `[ ]` otherwise).
pub fn todo_markdown(phases: &[Phase]) -> String {
    phases
        .iter()
        .map(|phase| {
            let title = if phase.name.trim().is_empty() { "Plan" } else { phase.name.trim() };
            let tasks = phase
                .tasks
                .iter()
                .map(|task| {
                    let mark = match task.status.as_str() {
                        "completed" => "x",
                        "in_progress" => "~",
                        _ => " ",
                    };
                    format!("- [{mark}] {}", task.content)
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("# {title}\n{tasks}\n")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The most recently saved native plan file among a project's sessions (newest 10 sessions that have
/// `local` artifacts). A session that was replaced after plan approval keeps its plan only here.
pub fn last_saved_plan(sessions: &[SessionInfo]) -> Option<PathBuf> {
    sessions
        .iter()
        .filter(|s| s.path.with_extension("").join("local").is_dir())
        .take(10)
        .filter_map(|s| {
            let mut reader = TailReader::default();
            reader.update(&s.path).ok()?;
            let plan = reader.state.plan_path?;
            let modified = fs::metadata(&plan).ok()?.modified().ok()?;
            Some((modified, plan))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, plan)| plan)
}

#[derive(Default)]
pub struct TailReader {
    pub state: SessionState,
    offset: u64,
    pending: Vec<u8>,
    checkpoint: Vec<u8>,
    created: Option<SystemTime>,
    plan_uri: Option<String>,
    progress: HashMap<String, Value>,
    local_writes: HashMap<String, String>,
    resolved_local: HashMap<String, PathBuf>,
}

/// A `write` tool call targeting a `local://` URI, as (call id, URI).
fn local_write_call(block: &Value) -> Option<(&str, &str)> {
    if block["type"] != "toolCall" || block["name"] != "write" {
        return None;
    }
    let uri = block["arguments"]["path"].as_str()?;
    uri.starts_with("local://").then_some((block["id"].as_str()?, uri))
}

impl TailReader {
    fn fold(&mut self, value: Value) {
        if value["type"] == "mode_change" {
            self.plan_uri = value["data"]["planFilePath"]
                .as_str()
                .or_else(|| value["data"]["planFile"].as_str())
                .map(String::from)
                .or(self.plan_uri.take());
        }
        if value["type"] == "custom" && value["customType"] == "user_todo_edit" {
            self.todos(&value["data"]["phases"]);
        }
        if value["type"] == "message" && value["message"]["role"] == "assistant" {
            self.record_local_writes(&value["message"]);
        }
        if value["type"] == "message" && value["message"]["role"] == "toolResult" {
            self.fold_tool_result(&value["message"]);
        }
    }

    fn record_local_writes(&mut self, message: &Value) {
        for block in message["content"].as_array().into_iter().flatten() {
            if let Some((id, uri)) = local_write_call(block) {
                self.local_writes.insert(id.into(), uri.into());
            }
        }
    }

    fn fold_tool_result(&mut self, message: &Value) {
        match message["toolName"].as_str() {
            Some("write") => self.resolve_local_write(message),
            Some("todo") => self.todos(&message["details"]["phases"]),
            Some("task") => {
                for item in message["details"]["progress"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    if let Some(id) = item["id"].as_str() {
                        self.progress.insert(id.into(), item.clone());
                    }
                }
            }
            _ => {}
        }
    }

    fn resolve_local_write(&mut self, message: &Value) {
        let Some(id) = message["toolCallId"].as_str() else {
            return;
        };
        let Some(uri) = self.local_writes.remove(id) else {
            return;
        };
        if message["isError"] == true {
            return;
        }
        if let Some(path) = message["details"]["resolvedPath"].as_str().map(PathBuf::from) {
            if path.is_absolute() {
                self.resolved_local.insert(uri, path);
            }
        }
    }

    fn todos(&mut self, phases: &Value) {
        let Some(phases) = phases.as_array() else {
            return;
        };
        self.state.task = None;
        self.state.todo_done = 0;
        self.state.todo_total = 0;
        self.state.phases.clear();
        for phase in phases {
            let mut tasks = Vec::new();
            for task in phase["tasks"].as_array().into_iter().flatten() {
                self.state.todo_total += 1;
                if task["status"] == "completed" {
                    self.state.todo_done += 1;
                }
                if task["status"] == "in_progress" && self.state.task.is_none() {
                    self.state.task = task["content"].as_str().map(String::from);
                }
                tasks.push(PlanTask {
                    content: task["content"].as_str().unwrap_or_default().into(),
                    status: task["status"].as_str().unwrap_or("pending").into(),
                });
            }
            self.state.phases.push(Phase { name: phase["name"].as_str().unwrap_or_default().into(), tasks });
        }
    }

    pub fn update(&mut self, path: &Path) -> Result<(), String> {
        let mut file = File::open(path).map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        let len = metadata.len();
        let created = metadata.created().ok();
        if self.is_replaced(&mut file, len, created)? {
            *self = Self::default();
        }
        self.created = created;
        if len > self.offset {
            self.read_appended(&mut file)?;
        }
        self.state.title = header(path).and_then(|(_, title)| title);
        self.state.plan_path = self
            .plan_uri
            .as_deref()
            .and_then(|uri| {
                local_path(path, uri)
                    .map(|fallback| self.resolved_local.get(uri).cloned().unwrap_or(fallback))
            })
            .filter(|p| p.is_file());
        self.refresh_subagents(path);
        Ok(())
    }

    fn is_replaced(&self, file: &mut File, len: u64, created: Option<SystemTime>) -> Result<bool, String> {
        if len < self.offset || (self.created.is_some() && created != self.created) {
            return Ok(true);
        }
        if self.offset == 0 {
            return Ok(false);
        }
        file.seek(SeekFrom::Start(self.offset - self.checkpoint.len() as u64))
            .map_err(|e| e.to_string())?;
        let mut previous = vec![0; self.checkpoint.len()];
        file.read_exact(&mut previous).map_err(|e| e.to_string())?;
        Ok(previous != self.checkpoint)
    }

    fn read_appended(&mut self, file: &mut File) -> Result<(), String> {
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|e| e.to_string())?;
        file.read_to_end(&mut self.pending)
            .map_err(|e| e.to_string())?;
        self.offset = file.stream_position().map_err(|e| e.to_string())?;
        let complete = self
            .pending
            .iter()
            .rposition(|b| *b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let bytes: Vec<u8> = self.pending.drain(..complete).collect();
        for line in bytes.split(|b| *b == b'\n') {
            if let Ok(value) = serde_json::from_slice(line) {
                self.fold(value);
            }
        }
        let size = self.offset.min(64) as usize;
        file.seek(SeekFrom::Start(self.offset - size as u64))
            .map_err(|e| e.to_string())?;
        self.checkpoint = vec![0; size];
        file.read_exact(&mut self.checkpoint)
            .map_err(|e| e.to_string())
    }

    fn refresh_subagents(&mut self, path: &Path) {
        self.state.subagents.clear();
        for entry in fs::read_dir(path.with_extension(""))
            .into_iter()
            .flatten()
            .flatten()
        {
            if let Some(subagent) = self.subagent(entry.path(), path) {
                self.state.subagents.push(subagent);
            }
        }
        self.state.subagents.sort_by(|a, b| a.name.cmp(&b.name));
    }

    fn subagent(&self, child: PathBuf, session: &Path) -> Option<Subagent> {
        if child.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            return None;
        }
        let (value, _) = header(&child)?;
        let parent = value["parentSession"].as_str()?;
        if !same_path(Path::new(parent), session) {
            return None;
        }
        let name = child
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let progress = self.progress.get(&name);
        Some(Subagent {
            id: name.clone(),
            name,
            path: child,
            task: progress
                .and_then(|p| p["task"].as_str())
                .map(|s| s.lines().next().unwrap_or("").into()),
            status: progress
                .and_then(|p| p["status"].as_str())
                .unwrap_or("unknown")
                .into(),
            agent: progress.and_then(|p| p["agent"].as_str()).map(String::from),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn native_tail_updates_and_rewrites() {
        let dir = std::env::temp_dir().join(format!(
            "omp-manager-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let plan = path.with_extension("").join("local/demo-plan.md");
        fs::create_dir_all(plan.parent().unwrap()).unwrap();
        fs::write(&plan, "# Actual plan").unwrap();
        let title =
            serde_json::json!({"type":"title","title":"Demo Session","pad":" ".repeat(200)});
        let head = serde_json::json!({"type":"session","id":"demo","cwd":dir});
        let todo = serde_json::json!({"type":"message","message":{"role":"toolResult","toolName":"todo","details":{"phases":[{"tasks":[{"content":"Done","status":"completed"},{"content":"Implement café","status":"in_progress"}]}]}}});
        let mode = serde_json::json!({"type":"mode_change","data":{"planFilePath":"local://demo-plan.md"}});
        fs::write(&path, format!("{title}\n{head}\n{todo}\n{mode}\n")).unwrap();
        let child = path.with_extension("").join("Scout1.jsonl");
        fs::write(
            &child,
            format!(
                "{}\n",
                serde_json::json!({"type":"session","id":"child","parentSession":path})
            ),
        )
        .unwrap();
        let mut reader = TailReader::default();
        reader.update(&path).unwrap();
        assert_eq!(reader.state.title.as_deref(), Some("Demo Session"));
        assert_eq!(reader.state.task.as_deref(), Some("Implement café"));
        assert_eq!((reader.state.todo_done, reader.state.todo_total), (1, 2));
        assert_eq!(reader.state.plan_path.as_ref(), Some(&plan));
        assert_eq!(reader.state.subagents[0].name, "Scout1");
        let next = serde_json::json!({"type":"message","message":{"role":"toolResult","toolName":"todo","details":{"phases":[]}}});
        writeln!(
            fs::OpenOptions::new().append(true).open(&path).unwrap(),
            "{next}"
        )
        .unwrap();
        reader.update(&path).unwrap();
        assert_eq!(reader.state.todo_total, 0);
        fs::write(&path, format!("{head}\n")).unwrap();
        reader.update(&path).unwrap();
        assert_eq!(reader.state.task, None);
        assert_eq!(reader.state.plan_path, None);
        assert!(local_path(&path, "local://../secret").is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn plans_use_correlated_native_local_write_paths_not_guessed_artifact_folders() {
        let root = std::env::temp_dir().join(format!("omp-plan-path-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("native-local")).unwrap();
        let journal = root.join("session.jsonl");
        let plan = root.join("native-local/review-plan.md");
        fs::write(&plan, "# Real native plan\n").unwrap();
        let call = serde_json::json!({"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","id":"write-1","name":"write","arguments":{"path":"local://review-plan.md"}}]}});
        let result = serde_json::json!({"type":"message","message":{"role":"toolResult","toolName":"write","toolCallId":"write-1","details":{"resolvedPath":plan}}});
        let mode = serde_json::json!({"type":"mode_change","mode":"plan","data":{"planFilePath":"local://review-plan.md"}});
        fs::write(&journal, format!("{call}\n{result}\n{mode}\n")).unwrap();
        let mut reader = TailReader::default();
        reader.update(&journal).unwrap();
        assert_eq!(reader.state.plan_path, Some(plan.clone()));
        let malicious = serde_json::json!({"type":"mode_change","data":{"planFilePath":"local://../review-plan.md"}});
        writeln!(
            fs::OpenOptions::new().append(true).open(&journal).unwrap(),
            "{malicious}"
        )
        .unwrap();
        reader.update(&journal).unwrap();
        assert_eq!(reader.state.plan_path, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partial_utf8_and_replaced_tail_are_not_mixed() {
        let dir = std::env::temp_dir().join(format!("omp-manager-tail-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let todo = serde_json::json!({"type":"message","message":{"role":"toolResult","toolName":"todo","details":{"phases":[{"tasks":[{"content":"café","status":"in_progress"}]}]}}});
        let line = format!("{todo}\n").into_bytes();
        let split = line.iter().position(|b| *b == 0xc3).unwrap() + 1;
        fs::write(&path, &line[..split]).unwrap();
        let mut reader = TailReader::default();
        reader.update(&path).unwrap();
        assert_eq!(reader.state.task, None);
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&line[split..])
            .unwrap();
        reader.update(&path).unwrap();
        assert_eq!(reader.state.task.as_deref(), Some("café"));
        let replacement = String::from_utf8(line).unwrap().replace("café", "tea!");
        fs::write(&path, replacement).unwrap();
        reader.update(&path).unwrap();
        assert_eq!(reader.state.task.as_deref(), Some("tea!"));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn the_todo_list_is_kept_by_phase_and_rendered_as_checkboxes() {
        let dir = std::env::temp_dir().join(format!("omp-todo-md-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let journal = dir.join("s.jsonl");
        let todo = serde_json::json!({"type":"message","message":{"role":"toolResult","toolName":"todo","details":{"phases":[
            {"name":"Build","tasks":[{"content":"one","status":"completed"},{"content":"two","status":"in_progress"}]},
            {"name":"","tasks":[{"content":"three","status":"pending"},{"content":"four","status":"abandoned"}]}]}}});
        fs::write(&journal, format!("{}\n{todo}\n", serde_json::json!({"type":"session","id":"s","cwd":dir}))).unwrap();
        let mut reader = TailReader::default();
        reader.update(&journal).unwrap();
        assert_eq!((reader.state.todo_done, reader.state.todo_total), (1, 4));
        assert_eq!(todo_markdown(&reader.state.phases), "# Build\n- [x] one\n- [~] two\n\n# Plan\n- [ ] three\n- [ ] four\n");
        assert_eq!(todo_markdown(&[]), "");
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn the_last_saved_plan_is_the_newest_native_plan_among_sessions_with_artifacts() {
        let dir = std::env::temp_dir().join(format!("omp-saved-plan-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let session = |name: &str, plan: Option<(&str, &str)>| {
            let path = dir.join(format!("{name}.jsonl"));
            let mut text = format!("{}\n", serde_json::json!({"type":"session","id":name,"cwd":dir}));
            if let Some((uri, body)) = plan {
                text += &format!("{}\n", serde_json::json!({"type":"mode_change","mode":"plan","data":{"planFilePath":uri}}));
                let file = local_path(&path, uri).unwrap();
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, body).unwrap();
            }
            fs::write(&path, text).unwrap();
            SessionInfo { id: name.into(), path, title: name.into(), modified_ms: 0 }
        };
        let old = session("old", Some(("local://old.md", "old")));
        std::thread::sleep(std::time::Duration::from_millis(30));
        let new = session("new", Some(("local://new.md", "new")));
        let bare = session("bare", None);
        let picked = last_saved_plan(&[bare.clone(), old.clone(), new.clone()]).unwrap();
        assert_eq!(fs::read_to_string(picked).unwrap(), "new");
        assert_eq!(last_saved_plan(&[bare.clone(), old]).map(|p| fs::read_to_string(p).unwrap()).as_deref(), Some("old"));
        assert_eq!(last_saved_plan(&[bare]), None);
        assert_eq!(last_saved_plan(&[]), None);
        fs::remove_dir_all(dir).unwrap();
    }
}

pub fn render_transcript_file(
    path: &Path,
    input: &crate::instance::CreateInput,
) -> Result<String, String> {
    let program = crate::toolchain::omp_program();
    let mut command = std::process::Command::new(program);
    command
        .arg("render")
        .arg(path)
        .args(["-w", "120", "--plain"]);
    command
        .current_dir(&input.project_path)
        .env("OMP_PROFILE", &input.profile);
    let overlays: Vec<PathBuf> = std::env::var_os("PI_CONFIG_FILES")
        .map(|s| std::env::split_paths(&s).collect())
        .unwrap_or_default();
    command.env(
        "PI_CONFIG_FILES",
        std::env::join_paths(overlays.iter().chain(input.configs.iter()))
            .map_err(|e| e.to_string())?,
    );
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command.output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
