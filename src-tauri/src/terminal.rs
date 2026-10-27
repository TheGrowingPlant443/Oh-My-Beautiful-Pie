//! Command history, live AI command output and user shells behind the Terminal pane.
//!
//! The bridge observer inside each managed OMP process captures real Bash/Eval execution and writes a ledger
//! (`script.txt` + `meta.json` per execution). This service authenticates its live events by actor/generation,
//! streams replacement output tails to subscribed channels and reads the ledger for history. It also owns the
//! optional interactive user shell (Tab 0), which never touches an OMP process.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter};

use crate::instance::{Registry, TerminalSource};
use crate::pty::Pty;

pub type Result<T, E = String> = std::result::Result<T, E>;

const HISTORY_PAGE: usize = 100;
const MAX_SCRIPT: u64 = 64 * 1024 * 1024;
const MAX_META: u64 = 1024 * 1024;
const MAX_OUTPUT: usize = 2 * 1024 * 1024;
const MAX_FAILURE_TEXT: usize = 256 * 1024;
const PREVIEW_CHARS: usize = 256;
const STATUSES: [&str; 6] = ["running", "stopping", "completed", "failed", "cancelled", "interrupted"];
const UNAVAILABLE: &str = "Command capture is not active for this session. Restart it to enable capture; it also needs the installed OMP SDK (set OMP_SDK_DIR if it is not found)";

pub fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn is_execution_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.hyphenated().to_string() == id)
        || id
            .strip_prefix("import-")
            .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn flatten(script: &str) -> String {
    script.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(PREVIEW_CHARS).collect()
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CommandSummary {
    pub execution_id: String,
    pub session_id: String,
    pub source_session_id: String,
    pub tool_call_id: String,
    pub agent_id: String,
    pub kind: String,
    pub description: Option<String>,
    pub preview: String,
    pub cwd: String,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub status: String,
    pub exit_code: Option<i64>,
    pub tab: Option<u32>,
    #[serde(default)]
    pub historical: bool,
    #[serde(default)]
    pub has_output: bool,
}

impl CommandSummary {
    fn validated(mut self) -> Result<Self> {
        if !is_execution_id(&self.execution_id) {
            return Err("invalid execution id".into());
        }
        if !["bash", "js", "py"].contains(&self.kind.as_str()) || !STATUSES.contains(&self.status.as_str()) {
            return Err("invalid command kind or status".into());
        }
        if self.session_id.is_empty() {
            return Err("command has no native session".into());
        }
        self.preview = self.preview.chars().take(PREVIEW_CHARS).collect();
        self.description = self.description.map(|d| d.chars().take(500).collect()).filter(|d: &String| !d.is_empty());
        self.historical = false;
        Ok(self)
    }
    fn live(&self) -> bool {
        self.ended_at.is_none() && matches!(self.status.as_str(), "running" | "stopping")
    }
}

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CommandOutput {
    pub execution_id: String,
    pub revision: u64,
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct UserShellInfo {
    pub shell_id: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub exited: bool,
    pub exit_code: Option<u32>,
}

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOwner {
    pub instance_id: String,
    pub name: String,
    pub generation: u64,
    pub session_id: Option<String>,
    pub supported: bool,
    pub error: Option<String>,
    pub held: bool,
    pub revision: u64,
    pub active: Vec<CommandSummary>,
    pub user_shell: Option<UserShellInfo>,
}

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub instance_id: String,
    pub instance_name: String,
    pub command: CommandSummary,
}

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    s: u64,
    i: String,
    e: String,
}

struct Live {
    summary: CommandSummary,
    output: CommandOutput,
    subscribers: Vec<Channel<CommandOutput>>,
}

#[derive(Default)]
struct OwnerState {
    generation: u64,
    session_id: Option<String>,
    supported: bool,
    error: Option<String>,
    held: bool,
    revision: u64,
    active: BTreeMap<String, Live>,
}

struct UserShell {
    id: String,
    cwd: String,
    pty: Mutex<Pty>,
    #[cfg(windows)]
    _containment: std::os::windows::io::OwnedHandle,
}

impl UserShell {
    fn info(&self) -> UserShellInfo {
        let mut pty = self.pty.lock();
        let exit = pty.poll().ok().flatten();
        UserShellInfo {
            shell_id: self.id.clone(),
            cwd: self.cwd.clone(),
            cols: pty.cols,
            rows: pty.rows,
            exited: exit.is_some(),
            exit_code: exit,
        }
    }
    fn close(&self) {
        let _ = self.pty.lock().kill();
    }
}

struct Imported {
    stamp: (u64, Option<std::time::SystemTime>),
    rows: Arc<Vec<CommandSummary>>,
}

struct Journaled {
    summary: CommandSummary,
    script: String,
    output: Option<String>,
}

fn tail(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

/// When a journaled tool call ended, whether it failed, its exit code and (failed only) its result text.
type JournalResult = (u64, bool, Option<i64>, Option<String>);

fn journal_result(message: &Value, stamp: u64, results: &mut HashMap<String, JournalResult>) {
    let Some(call_id) = message["toolCallId"].as_str() else { return };
    let failed = message["isError"].as_bool() == Some(true);
    let text = message["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
    let output = (failed && !text.is_empty()).then(|| tail(text, MAX_FAILURE_TEXT));
    results.insert(call_id.into(), (stamp, failed, message["details"]["exitCode"].as_i64(), output));
}

fn journal_call(block: &Value, stamp: u64, default_cwd: &str, session_id: &str) -> Option<Journaled> {
    let name = block["name"].as_str().unwrap_or("");
    if block["type"] != "toolCall" || !["bash", "eval"].contains(&name) {
        return None;
    }
    let call_id = block["id"].as_str()?;
    let arguments = &block["arguments"];
    let script = arguments[if name == "bash" { "command" } else { "code" }].as_str()?;
    let kind = if name == "bash" {
        "bash"
    } else if matches!(arguments["language"].as_str(), Some("py" | "python")) {
        "py"
    } else {
        "js"
    };
    let description = block["intent"]
        .as_str()
        .or_else(|| arguments["i"].as_str())
        .or_else(|| arguments["title"].as_str())
        .map(|d| d.chars().take(500).collect::<String>())
        .filter(|d| !d.is_empty());
    let cwd = match arguments["cwd"].as_str() {
        Some(cwd) if !cwd.is_empty() => Path::new(default_cwd).join(cwd).to_string_lossy().into_owned(),
        _ => default_cwd.to_string(),
    };
    Some(Journaled {
        summary: CommandSummary {
            execution_id: format!("import-{}", digest(&format!("{session_id}:{call_id}"))),
            session_id: session_id.into(),
            source_session_id: session_id.into(),
            tool_call_id: call_id.into(),
            agent_id: "main".into(),
            kind: kind.into(),
            description,
            preview: flatten(script),
            cwd,
            started_at: stamp,
            ended_at: None,
            status: "interrupted".into(),
            exit_code: None,
            tab: None,
            historical: true,
            has_output: false,
        },
        script: script.into(),
        output: None,
    })
}

pub struct TerminalService {
    root: PathBuf,
    registry: OnceLock<Weak<Registry>>,
    app: OnceLock<AppHandle>,
    state: Mutex<HashMap<String, OwnerState>>,
    shells: Mutex<HashMap<String, Arc<UserShell>>>,
    exited: Mutex<HashSet<String>>,
    imports: Mutex<HashMap<PathBuf, Imported>>,
}

impl TerminalService {
    pub fn new(folder: &Path) -> Arc<Self> {
        Arc::new(Self {
            root: folder.join("terminal-sessions"),
            registry: OnceLock::new(),
            app: OnceLock::new(),
            state: Mutex::new(HashMap::new()),
            shells: Mutex::new(HashMap::new()),
            exited: Mutex::new(HashSet::new()),
            imports: Mutex::new(HashMap::new()),
        })
    }
    /// The observer module is imported by the bridge extension next to it.
    pub fn materialize(folder: &Path) -> Result<()> {
        let file = folder.join("omp-terminal.mjs");
        let source = include_str!("../../bridge/omp-terminal.mjs");
        if fs::read_to_string(&file).ok().as_deref() != Some(source) {
            fs::write(file, source).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    pub fn bind_registry(&self, registry: Weak<Registry>) -> Result<()> {
        self.registry.set(registry).map_err(|_| "terminal registry already bound".into())
    }
    pub fn bind_app(&self, app: AppHandle) -> Result<()> {
        self.app.set(app).map_err(|_| "terminal app already bound".into())
    }
    pub fn instance_dir(&self, id: &str) -> PathBuf {
        self.root.join(digest(id))
    }
    fn registry(&self) -> Result<Arc<Registry>> {
        self.registry.get().and_then(Weak::upgrade).ok_or_else(|| "terminal registry unavailable".into())
    }
    fn source(&self, id: &str) -> Result<TerminalSource> {
        self.registry()?
            .terminal_sources()
            .into_iter()
            .find(|source| source.id == id)
            .ok_or_else(|| "unknown instance".into())
    }

    fn publish(&self, id: &str) {
        let Some(app) = self.app.get() else { return };
        if let Ok(owner) = self.owner(id) {
            let _ = app.emit("terminal_state", owner);
        }
    }

    fn with_owner<R>(&self, id: &str, generation: u64, work: impl FnOnce(&mut OwnerState) -> Result<R>) -> Result<R> {
        let mut state = self.state.lock();
        let owner = state.entry(id.into()).or_default();
        if generation < owner.generation {
            return Err("stale_generation".into());
        }
        if generation > owner.generation {
            *owner = OwnerState { generation, ..OwnerState::default() };
        }
        work(owner)
    }

    // ---- live events from the authenticated bridge ----

    pub fn apply_snapshot(&self, id: &str, generation: u64, payload: &Value) -> Result<()> {
        let terminal = &payload["terminal"];
        let rows = if terminal.is_null() {
            vec![]
        } else {
            let items = terminal["active"].as_array().ok_or("invalid terminal snapshot")?;
            items
                .iter()
                .map(|item| {
                    serde_json::from_value::<CommandSummary>(item.clone())
                        .map_err(|e| format!("invalid terminal snapshot: {e}"))
                        .and_then(CommandSummary::validated)
                })
                .collect::<Result<Vec<_>>>()?
        };
        self.with_owner(id, generation, |owner| {
            owner.session_id = payload["sessionId"].as_str().map(String::from);
            owner.supported = terminal["supported"].as_bool() == Some(true);
            owner.error = if terminal.is_null() {
                Some(UNAVAILABLE.into())
            } else {
                terminal["error"].as_str().map(String::from)
            };
            owner.held = terminal["held"].as_bool() == Some(true);
            let keep: HashSet<&str> = rows.iter().map(|row| row.execution_id.as_str()).collect();
            owner.active.retain(|key, _| keep.contains(key.as_str()));
            for row in rows.iter().filter(|row| row.live()) {
                Self::upsert(owner, row.clone());
            }
            owner.revision += 1;
            Ok(())
        })?;
        self.publish(id);
        Ok(())
    }

    fn upsert(owner: &mut OwnerState, summary: CommandSummary) {
        match owner.active.get_mut(&summary.execution_id) {
            Some(live) => live.summary = summary,
            None => {
                let output = CommandOutput { execution_id: summary.execution_id.clone(), revision: 0, text: String::new(), truncated: false };
                owner.active.insert(summary.execution_id.clone(), Live { summary, output, subscribers: vec![] });
            }
        }
    }

    pub fn apply_command(&self, id: &str, generation: u64, payload: &Value) -> Result<()> {
        let summary = serde_json::from_value::<CommandSummary>(payload["command"].clone())
            .map_err(|e| format!("invalid terminal command: {e}"))?
            .validated()?;
        self.with_owner(id, generation, |owner| {
            if summary.live() {
                Self::upsert(owner, summary);
            } else {
                owner.active.remove(&summary.execution_id);
            }
            owner.revision += 1;
            Ok(())
        })?;
        self.publish(id);
        Ok(())
    }

    pub fn apply_output(&self, id: &str, generation: u64, payload: &Value) -> Result<()> {
        let execution_id = payload["executionId"].as_str().filter(|e| is_execution_id(e)).ok_or("invalid execution id")?;
        let revision = payload["revision"].as_u64().ok_or("invalid output revision")?;
        let text = payload["text"].as_str().ok_or("invalid output text")?;
        if text.len() > MAX_OUTPUT {
            return Err("terminal output exceeds the transport limit".into());
        }
        let truncated = payload["truncated"].as_bool() == Some(true);
        self.with_owner(id, generation, |owner| {
            let Some(live) = owner.active.get_mut(execution_id) else { return Ok(()) };
            if revision <= live.output.revision {
                return Ok(());
            }
            live.output = CommandOutput { execution_id: execution_id.into(), revision, text: text.into(), truncated };
            let output = live.output.clone();
            live.subscribers.retain(|channel| channel.send(output.clone()).is_ok());
            Ok(())
        })
    }

    pub fn apply_hold(&self, id: &str, generation: u64, payload: &Value) -> Result<()> {
        let held = payload["held"].as_bool().ok_or("invalid hold state")?;
        self.with_owner(id, generation, |owner| {
            owner.held = held;
            owner.revision += 1;
            Ok(())
        })?;
        self.publish(id);
        Ok(())
    }

    /// The agent process ended or restarted: its observations can no longer complete, history keeps them as interrupted.
    pub fn invalidate(&self, id: &str, generation: u64) {
        {
            let mut state = self.state.lock();
            let Some(owner) = state.get_mut(id) else { return };
            if owner.generation != generation {
                return;
            }
            let revision = owner.revision + 1;
            *owner = OwnerState { generation, revision, ..OwnerState::default() };
        }
        self.publish(id);
    }

    pub fn attach_output(&self, id: &str, generation: u64, session_id: &str, execution_id: &str, channel: Channel<CommandOutput>) -> Result<()> {
        self.with_owner(id, generation, |owner| {
            let live = owner.active.get_mut(execution_id).ok_or("This command has finished")?;
            if live.summary.session_id != session_id {
                return Err("stale_generation".into());
            }
            channel.send(live.output.clone()).map_err(|e| e.to_string())?;
            live.subscribers.push(channel);
            Ok(())
        })
    }

    pub fn kill(&self, id: &str, generation: u64, session_id: &str, execution_id: &str) -> Result<()> {
        self.with_owner(id, generation, |owner| {
            let live = owner.active.get(execution_id).ok_or("This command has already finished")?;
            if live.summary.session_id != session_id {
                return Err("stale_generation".into());
            }
            Ok(())
        })?;
        let registry = self.registry()?;
        let hub = registry.manager_hub().ok_or("manager bridge not initialized")?;
        hub.kill_terminal_command(id, generation, session_id, execution_id)
    }

    // ---- owners ----

    fn build_owner(&self, source: &TerminalSource) -> TerminalOwner {
        let state = self.state.lock();
        let shell = self.shells.lock().get(&source.id).map(|shell| shell.info());
        match state.get(&source.id) {
            Some(owner) => TerminalOwner {
                instance_id: source.id.clone(),
                name: source.name.clone(),
                generation: owner.generation,
                session_id: owner.session_id.clone(),
                supported: owner.supported,
                error: owner.error.clone(),
                held: owner.held,
                revision: owner.revision,
                active: owner.active.values().map(|live| live.summary.clone()).collect(),
                user_shell: shell,
            },
            None => TerminalOwner {
                instance_id: source.id.clone(),
                name: source.name.clone(),
                generation: source.generation,
                session_id: source.session_id.clone(),
                supported: false,
                error: None,
                held: false,
                revision: 0,
                active: vec![],
                user_shell: shell,
            },
        }
    }
    pub fn owner(&self, id: &str) -> Result<TerminalOwner> {
        Ok(self.build_owner(&self.source(id)?))
    }
    pub fn owners(&self) -> Result<Vec<TerminalOwner>> {
        Ok(self.registry()?.terminal_sources().iter().map(|source| self.build_owner(source)).collect())
    }

    // ---- history ----

    pub fn history(&self, id: Option<&str>, cursor: Option<&str>) -> Result<HistoryPage> {
        let sources: Vec<TerminalSource> = self.registry()?.terminal_sources().into_iter().filter(|s| id.is_none_or(|id| id == s.id)).collect();
        if let Some(id) = id {
            if sources.is_empty() {
                return Err(format!("unknown instance {id}"));
            }
        }
        self.history_for(&sources, cursor)
    }

    fn history_for(&self, sources: &[TerminalSource], cursor: Option<&str>) -> Result<HistoryPage> {
        let after = cursor.map(|c| serde_json::from_str::<Cursor>(c).map_err(|_| "invalid history cursor".to_string())).transpose()?;
        let mut rows = Vec::new();
        for source in sources {
            for command in self.owner_rows(source)? {
                rows.push(HistoryEntry { instance_id: source.id.clone(), instance_name: source.name.clone(), command });
            }
        }
        let key = |entry: &HistoryEntry| (entry.command.started_at, entry.instance_id.clone(), entry.command.execution_id.clone());
        rows.sort_by_key(|entry| std::cmp::Reverse(key(entry)));
        if let Some(after) = &after {
            let limit = (after.s, after.i.clone(), after.e.clone());
            rows.retain(|entry| key(entry) < limit);
        }
        let more = rows.len() > HISTORY_PAGE;
        rows.truncate(HISTORY_PAGE);
        let next_cursor = if more {
            rows.last().map(|last| {
                serde_json::to_string(&Cursor { s: last.command.started_at, i: last.instance_id.clone(), e: last.command.execution_id.clone() })
                    .unwrap_or_default()
            })
        } else {
            None
        };
        Ok(HistoryPage { entries: rows, next_cursor })
    }

    fn native_session(source: &TerminalSource) -> Option<String> {
        source.session_id.clone().or_else(|| {
            let (header, _) = crate::session::header(source.session_path.as_deref()?)?;
            header["id"].as_str().map(String::from)
        })
    }

    fn owner_rows(&self, source: &TerminalSource) -> Result<Vec<CommandSummary>> {
        let Some(session_id) = Self::native_session(source) else { return Ok(vec![]) };
        let live: HashMap<String, CommandSummary> = self
            .state
            .lock()
            .get(&source.id)
            .map(|owner| owner.active.iter().map(|(key, live)| (key.clone(), live.summary.clone())).collect())
            .unwrap_or_default();
        let mut rows = self.ledger_rows(&source.id, &session_id, &live)?;
        if let Some(path) = &source.session_path {
            let known: HashSet<(String, String)> = rows.iter().map(|r| (r.source_session_id.clone(), r.tool_call_id.clone())).collect();
            for row in self.imported(path, &session_id)?.iter() {
                if !known.contains(&(row.source_session_id.clone(), row.tool_call_id.clone())) {
                    rows.push(row.clone());
                }
            }
        }
        Ok(rows)
    }

    fn ledger_rows(&self, instance: &str, session_id: &str, live: &HashMap<String, CommandSummary>) -> Result<Vec<CommandSummary>> {
        let directory = self.instance_dir(instance).join(digest(session_id));
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(format!("Terminal history could not be read: {e}")),
        };
        let mut rows = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| format!("Terminal history could not be read: {e}"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_execution_id(&name) || name.starts_with("import-") || entry.file_type().map_or(true, |t| !t.is_dir()) {
                continue;
            }
            let meta = entry.path().join("meta.json");
            let Ok(info) = fs::metadata(&meta) else { continue };
            if info.len() > MAX_META {
                continue;
            }
            let bytes = fs::read(&meta).map_err(|e| format!("Terminal history could not be read: {e}"))?;
            let Ok(summary) = serde_json::from_slice::<CommandSummary>(&bytes).map_err(|e| e.to_string()).and_then(CommandSummary::validated) else {
                continue;
            };
            if summary.execution_id != name {
                continue;
            }
            rows.push(match live.get(&name) {
                Some(live) => live.clone(),
                None if summary.live() => CommandSummary { status: "interrupted".into(), tab: None, ..summary },
                None => CommandSummary { tab: None, ..summary },
            });
        }
        Ok(rows)
    }

    // ---- scripts ----

    pub fn read_script(&self, id: &str, session_id: &str, execution_id: &str) -> Result<String> {
        if let Some(hash) = execution_id.strip_prefix("import-") {
            if !is_execution_id(execution_id) {
                return Err("invalid execution id".into());
            }
            let source = self.source(id)?;
            let path = source.session_path.as_ref().ok_or("This conversation has no saved journal")?;
            return self.imported_script(path, session_id, hash);
        }
        self.read_ledger_file(id, session_id, execution_id, "script.txt", "Script", MAX_SCRIPT)
    }

    /// Output saved for a failed command; commands that did not fail keep none.
    pub fn read_output(&self, id: &str, session_id: &str, execution_id: &str) -> Result<String> {
        if let Some(hash) = execution_id.strip_prefix("import-") {
            if !is_execution_id(execution_id) {
                return Err("invalid execution id".into());
            }
            let source = self.source(id)?;
            let path = source.session_path.as_ref().ok_or("This conversation has no saved journal")?;
            return self.imported_output(path, session_id, hash);
        }
        self.read_ledger_file(id, session_id, execution_id, "output.txt", "Output", MAX_OUTPUT as u64)
    }

    fn read_ledger_file(&self, id: &str, session_id: &str, execution_id: &str, file: &str, what: &str, limit: u64) -> Result<String> {
        if !is_execution_id(execution_id) {
            return Err("invalid execution id".into());
        }
        let directory = self.instance_dir(id).join(digest(session_id)).join(execution_id);
        let path = directory.join(file);
        // The ledger is app-owned; refuse anything that was redirected elsewhere.
        for part in [&self.root, &self.instance_dir(id), &self.instance_dir(id).join(digest(session_id)), &directory, &path] {
            let info = fs::symlink_metadata(part).map_err(|e| format!("{what} unavailable: {e}"))?;
            if info.file_type().is_symlink() {
                return Err(format!("{what} unavailable: ledger entry was redirected"));
            }
        }
        let length = fs::metadata(&path).map_err(|e| e.to_string())?.len();
        if length > limit {
            return Err(format!("{what} exceeds the {} MiB transport limit", limit / (1024 * 1024)));
        }
        let bytes = fs::read(&path).map_err(|e| format!("{what} unavailable: {e}"))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    // ---- read-only journal import ----

    fn journal(path: &Path, session_id: &str) -> Result<Vec<Journaled>> {
        let file = fs::File::open(path).map_err(|e| format!("Conversation journal unavailable: {e}"))?;
        let default_cwd = crate::session::header(path).and_then(|(h, _)| h["cwd"].as_str().map(String::from)).unwrap_or_default();
        let mut calls: Vec<Journaled> = Vec::new();
        // Per tool call: when it ended, whether it failed, its exit code and (failed only) its result text.
        let mut results: HashMap<String, JournalResult> = HashMap::new();
        for line in BufReader::new(file).split(b'\n') {
            let line = line.map_err(|e| format!("Conversation journal unavailable: {e}"))?;
            let Ok(text) = std::str::from_utf8(&line) else { continue };
            Self::journal_line(text, &default_cwd, session_id, &mut calls, &mut results);
        }
        for call in &mut calls {
            if let Some((ended, failed, code, output)) = results.remove(&call.summary.tool_call_id) {
                call.summary.ended_at = Some(ended);
                call.summary.status = if failed { "failed" } else { "completed" }.into();
                call.summary.exit_code = code;
                call.summary.has_output = output.is_some();
                call.output = output;
            }
        }
        Ok(calls)
    }

    fn journal_line(text: &str, default_cwd: &str, session_id: &str, calls: &mut Vec<Journaled>, results: &mut HashMap<String, JournalResult>) {
        let tool = text.contains("\"bash\"") || text.contains("\"eval\"");
        if !tool || !(text.contains("\"toolCall\"") || text.contains("\"toolResult\"")) {
            return;
        }
        let Ok(entry) = serde_json::from_str::<Value>(text) else { return };
        if entry["type"] != "message" {
            return;
        }
        let message = &entry["message"];
        let stamp = message["timestamp"].as_u64().or_else(|| entry["timestamp"].as_str().and_then(iso_ms)).unwrap_or(0);
        if message["role"] == "toolResult" && ["bash", "eval"].contains(&message["toolName"].as_str().unwrap_or("")) {
            journal_result(message, stamp, results);
        } else if message["role"] == "assistant" {
            let blocks = message["content"].as_array().into_iter().flatten();
            calls.extend(blocks.filter_map(|block| journal_call(block, stamp, default_cwd, session_id)));
        }
    }

    fn imported(&self, path: &Path, session_id: &str) -> Result<Arc<Vec<CommandSummary>>> {
        // OMP creates the journal with the first message: until then there is simply nothing to import.
        let info = match fs::metadata(path) {
            Ok(info) => info,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Arc::new(vec![])),
            Err(e) => return Err(format!("Conversation journal unavailable: {e}")),
        };
        let stamp = (info.len(), info.modified().ok());
        if let Some(cached) = self.imports.lock().get(path).filter(|c| c.stamp == stamp) {
            return Ok(cached.rows.clone());
        }
        let rows = Arc::new(Self::journal(path, session_id)?.into_iter().map(|call| call.summary).collect::<Vec<_>>());
        self.imports.lock().insert(path.into(), Imported { stamp, rows: rows.clone() });
        Ok(rows)
    }

    fn imported_script(&self, path: &Path, session_id: &str, hash: &str) -> Result<String> {
        let wanted = format!("import-{hash}");
        Self::journal(path, session_id)?
            .into_iter()
            .find(|call| call.summary.execution_id == wanted)
            .map(|call| call.script)
            .ok_or_else(|| "Script not found in the conversation journal".into())
    }

    fn imported_output(&self, path: &Path, session_id: &str, hash: &str) -> Result<String> {
        let wanted = format!("import-{hash}");
        Self::journal(path, session_id)?
            .into_iter()
            .find(|call| call.summary.execution_id == wanted)
            .and_then(|call| call.output)
            .ok_or_else(|| "No output was recorded for this command".into())
    }

    // ---- user shell (Tab 0) ----

    pub fn open_user_shell(&self, id: &str) -> Result<UserShellInfo> {
        let project = self.source(id)?.project_path;
        self.open_shell_in(id, &project)
    }

    fn open_shell_in(&self, id: &str, cwd: &str) -> Result<UserShellInfo> {
        let info = {
            let mut shells = self.shells.lock();
            if let Some(shell) = shells.get(id) {
                return Ok(shell.info());
            }
            let mut pty = Pty::spawn_shell(cwd)?;
            #[cfg(windows)]
            let containment = match pty.process_id().ok_or_else(|| "terminal shell process id unavailable".to_string()).and_then(|pid| crate::process_tree::contain(pid, "terminal shell")) {
                Ok(job) => job,
                Err(error) => {
                    let _ = pty.kill();
                    return Err(error);
                }
            };
            let shell = Arc::new(UserShell {
                id: uuid::Uuid::new_v4().to_string(),
                cwd: cwd.into(),
                pty: Mutex::new(pty),
                #[cfg(windows)]
                _containment: containment,
            });
            let info = shell.info();
            shells.insert(id.into(), shell);
            info
        };
        self.publish(id);
        Ok(info)
    }

    fn shell(&self, id: &str, shell_id: &str) -> Result<Arc<UserShell>> {
        self.shells
            .lock()
            .get(id)
            .filter(|shell| shell.id == shell_id)
            .cloned()
            .ok_or_else(|| "This terminal is closed".into())
    }
    pub fn attach_user_shell(&self, id: &str, shell_id: &str, channel: Channel<InvokeResponseBody>) -> Result<()> {
        self.shell(id, shell_id)?.pty.lock().attach(channel)
    }
    pub fn write_user_shell(&self, id: &str, shell_id: &str, data: &str) -> Result<()> {
        self.shell(id, shell_id)?.pty.lock().write(data).map_err(|e| if e == "instance not running" { "The terminal process has exited".into() } else { e })
    }
    pub fn resize_user_shell(&self, id: &str, shell_id: &str, cols: u16, rows: u16) -> Result<()> {
        self.shell(id, shell_id)?.pty.lock().resize(cols, rows)
    }
    pub fn close_user_shell(&self, id: &str, shell_id: &str) -> Result<()> {
        let shell = {
            let mut shells = self.shells.lock();
            if shells.get(id).is_none_or(|shell| shell.id != shell_id) {
                return Err("This terminal is closed".into());
            }
            shells.remove(id)
        };
        if let Some(shell) = shell {
            shell.close();
        }
        self.exited.lock().remove(id);
        self.publish(id);
        Ok(())
    }
    /// The instance was removed: nothing may keep running for it.
    pub fn close_instance(&self, id: &str) {
        if let Some(shell) = self.shells.lock().remove(id) {
            shell.close();
        }
        self.state.lock().remove(id);
        self.exited.lock().remove(id);
    }
    pub fn shutdown(&self) {
        let shells: Vec<Arc<UserShell>> = self.shells.lock().drain().map(|(_, shell)| shell).collect();
        for shell in shells {
            shell.close();
        }
    }
    /// A shell that exited by itself keeps its tab until closed; tell the UI once.
    pub fn poll_shells(&self) {
        let ids: Vec<(String, bool)> = self.shells.lock().iter().map(|(id, shell)| (id.clone(), shell.info().exited)).collect();
        for (id, exited) in ids {
            let changed = {
                let mut seen = self.exited.lock();
                if exited { seen.insert(id.clone()) } else { seen.remove(&id) }
            };
            if changed {
                self.publish(&id);
            }
        }
    }
}

/// `2026-10-11T18:41:00.753Z` to Unix milliseconds.
fn iso_ms(text: &str) -> Option<u64> {
    let (date, time) = text.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (d.next()??, d.next()??, d.next()??);
    let (clock, millis) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = clock.split(':').map(|p| p.parse::<i64>().ok());
    let (hour, minute, second) = (t.next()??, t.next()??, t.next()??);
    let fraction: String = millis.chars().take(3).collect();
    let millis = format!("{fraction:0<3}").parse::<i64>().ok()?;
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000 + millis).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("omp-terminal-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }
    fn source(id: &str, session: Option<&str>, path: Option<PathBuf>) -> TerminalSource {
        TerminalSource { id: id.into(), name: format!("Agent {id}"), project_path: ".".into(), session_path: path, session_id: session.map(String::from), generation: 1 }
    }
    fn summary(id: &str, session: &str, started: u64, ended: Option<u64>, status: &str) -> CommandSummary {
        CommandSummary {
            execution_id: id.into(),
            session_id: session.into(),
            source_session_id: session.into(),
            tool_call_id: format!("call-{id}"),
            agent_id: "main".into(),
            kind: "bash".into(),
            description: Some("Test".into()),
            preview: "echo hi".into(),
            cwd: ".".into(),
            started_at: started,
            ended_at: ended,
            status: status.into(),
            exit_code: None,
            tab: None,
            historical: false,
            has_output: false,
        }
    }
    fn write_row(service: &TerminalService, instance: &str, session: &str, row: &CommandSummary, script: &str) {
        let directory = service.instance_dir(instance).join(digest(session)).join(&row.execution_id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("meta.json"), serde_json::to_vec(row).unwrap()).unwrap();
        fs::write(directory.join("script.txt"), script).unwrap();
    }
    fn exec(n: u32) -> String {
        uuid::Uuid::from_u128(n as u128 + 1).to_string()
    }

    #[test]
    fn merged_history_pages_every_row_once_in_global_order() {
        let folder = scratch("paging");
        let service = TerminalService::new(&folder);
        // 130 rows per owner, half with identical timestamps so the tuple tiebreak decides the order.
        for owner in ["a", "b"] {
            for n in 0..130u32 {
                let offset = if owner == "a" { 0 } else { 1000 };
                write_row(&service, owner, &format!("session-{owner}"), &summary(&exec(n + offset), &format!("session-{owner}"), (n / 2) as u64 * 10, Some(5), "completed"), "echo hi");
            }
        }
        let sources = vec![source("a", Some("session-a"), None), source("b", Some("session-b"), None)];
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = service.history_for(&sources, cursor.as_deref()).unwrap();
            assert!(page.entries.len() <= HISTORY_PAGE);
            seen.extend(page.entries.iter().map(|e| (e.command.started_at, e.instance_id.clone(), e.command.execution_id.clone())));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(seen.len(), 260);
        let unique: HashSet<_> = seen.iter().cloned().collect();
        assert_eq!(unique.len(), 260, "no row is repeated or skipped across pages");
        let mut sorted = seen.clone();
        sorted.sort_by(|x, y| y.cmp(x));
        assert_eq!(seen, sorted, "rows follow (startedAt, instance, execution) descending");
        assert!(service.history_for(&sources, Some("not json")).is_err());
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn unfinished_ledger_rows_without_a_live_process_are_interrupted_and_removed_owners_leave() {
        let folder = scratch("interrupted");
        let service = TerminalService::new(&folder);
        write_row(&service, "a", "s", &summary(&exec(1), "s", 10, None, "running"), "sleep 100");
        write_row(&service, "a", "s", &summary(&exec(2), "s", 20, Some(30), "completed"), "true");
        write_row(&service, "other", "s", &summary(&exec(3), "s", 40, Some(50), "completed"), "true");
        let page = service.history_for(&[source("a", Some("s"), None)], None).unwrap();
        let rows: Vec<_> = page.entries.iter().map(|e| (e.command.execution_id.clone(), e.command.status.clone())).collect();
        assert_eq!(rows, vec![(exec(2), "completed".into()), (exec(1), "interrupted".into())]);
        assert!(page.entries.iter().all(|e| e.instance_id == "a"), "another owner's rows never leak in");
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn a_live_command_keeps_its_tab_and_ignores_stale_output() {
        let folder = scratch("live");
        let service = TerminalService::new(&folder);
        let id = exec(7);
        let command = |status: &str, ended: Value, tab: Value| json!({"command": {"executionId": id, "sessionId": "s", "sourceSessionId": "s", "toolCallId": "c", "agentId": "main", "kind": "bash", "description": null, "preview": "x", "cwd": ".", "startedAt": 1, "endedAt": ended, "status": status, "exitCode": null, "tab": tab}});
        service.apply_command("a", 1, &command("running", Value::Null, json!(2))).unwrap();
        service.apply_output("a", 1, &json!({"executionId": id, "revision": 2, "text": "two", "truncated": false})).unwrap();
        service.apply_output("a", 1, &json!({"executionId": id, "revision": 1, "text": "one", "truncated": false})).unwrap();
        let output = service.state.lock().get("a").unwrap().active.get(&id).unwrap().output.clone();
        assert_eq!((output.revision, output.text.as_str()), (2, "two"));
        assert!(service.apply_command("a", 0, &command("running", Value::Null, json!(2))).is_err(), "a stale generation is rejected");
        assert!(service.apply_output("a", 1, &json!({"executionId": "../x", "revision": 3, "text": "", "truncated": false})).is_err());
        service.apply_command("a", 1, &command("completed", json!(5), Value::Null)).unwrap();
        assert!(service.state.lock().get("a").unwrap().active.is_empty());
        service.apply_command("a", 2, &command("running", Value::Null, json!(2))).unwrap();
        service.invalidate("a", 2);
        assert!(service.state.lock().get("a").unwrap().active.is_empty(), "a restart ends every observation");
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn journal_import_uses_only_recorded_facts_and_never_duplicates_captured_rows() {
        let folder = scratch("import");
        let service = TerminalService::new(&folder);
        let journal = folder.join("session.jsonl");
        let script = format!("{}\n{}", "echo start", "x".repeat(400));
        let lines = [
            json!({"type":"session","id":"native","cwd":"C:/project","timestamp":"2026-10-11T18:36:15.017Z"}),
            json!({"type":"message","timestamp":"2026-10-11T18:41:00.753Z","message":{"role":"assistant","timestamp":1791744053135u64,"content":[
                {"type":"toolCall","id":"call-1","name":"bash","arguments":{"command":script,"i":"Run it"},"intent":"Run it"},
                {"type":"toolCall","id":"call-2","name":"eval","arguments":{"language":"py","code":"print(1)","title":"Py cell"}},
                {"type":"toolCall","id":"call-3","name":"bash","arguments":{"command":"never finished"}},
                {"type":"toolCall","id":"call-4","name":"read","arguments":{"path":"x"}}]}}),
            json!({"type":"message","message":{"role":"toolResult","toolCallId":"call-1","toolName":"bash","isError":false,"timestamp":1791744055000u64,"details":{}}}),
            json!({"type":"message","message":{"role":"toolResult","toolCallId":"call-2","toolName":"eval","isError":true,"timestamp":1791744056000u64,"content":[{"type":"text","text":"Traceback (most recent call last):\n  File \"x\", line 1\nZeroDivisionError"}]}}),
        ];
        fs::write(&journal, lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        let rows = service.imported(&journal, "native").unwrap();
        assert_eq!(rows.len(), 3);
        let first = rows.iter().find(|r| r.tool_call_id == "call-1").unwrap();
        assert_eq!((first.status.as_str(), first.ended_at, first.started_at), ("completed", Some(1791744055000), 1791744053135));
        assert_eq!(first.description.as_deref(), Some("Run it"));
        assert!(first.historical && first.preview.chars().count() <= 256 && !first.preview.contains('\n'));
        let python = rows.iter().find(|r| r.tool_call_id == "call-2").unwrap();
        assert_eq!((python.kind.as_str(), python.status.as_str(), python.description.as_deref()), ("py", "failed", Some("Py cell")));
        let unfinished = rows.iter().find(|r| r.tool_call_id == "call-3").unwrap();
        assert_eq!((unfinished.status.as_str(), unfinished.ended_at), ("interrupted", None));
        assert!(unfinished.cwd.replace('\\', "/").ends_with("C:/project") || unfinished.cwd == "C:/project");
        let full = service.imported_script(&journal, "native", first.execution_id.strip_prefix("import-").unwrap()).unwrap();
        assert_eq!(full, script, "the full original code is recoverable");
        let hash = |row: &CommandSummary| row.execution_id.strip_prefix("import-").unwrap().to_string();
        assert!(python.has_output && !first.has_output && !unfinished.has_output);
        assert_eq!(service.imported_output(&journal, "native", &hash(python)).unwrap(), "Traceback (most recent call last):\n  File \"x\", line 1\nZeroDivisionError");
        assert!(service.imported_output(&journal, "native", &hash(first)).is_err(), "only failed commands carry output");
        // A row captured live for the same tool call suppresses the imported twin.
        let mut captured = summary(&exec(9), "native", 1791744053135, Some(1791744055000), "completed");
        captured.tool_call_id = "call-1".into();
        write_row(&service, "a", "native", &captured, &script);
        let page = service.history_for(&[source("a", None, Some(journal.clone()))], None).unwrap();
        assert_eq!(page.entries.iter().filter(|e| e.command.tool_call_id == "call-1").count(), 1);
        assert_eq!(page.entries.len(), 3);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn a_session_whose_journal_does_not_exist_yet_still_lists_its_recorded_commands() {
        let folder = scratch("lazy-journal");
        let service = TerminalService::new(&folder);
        write_row(&service, "a", "s", &summary(&exec(1), "s", 10, Some(20), "completed"), "true");
        let page = service.history_for(&[source("a", Some("s"), Some(folder.join("not-created-yet.jsonl")))], None).unwrap();
        assert_eq!(page.entries.len(), 1, "OMP creates the journal with the first message");
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn iso_timestamps_convert_to_unix_milliseconds() {
        assert_eq!(iso_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(iso_ms("2026-10-11T18:41:00.753Z"), Some(1791744060753));
        assert_eq!(iso_ms("garbage"), None);
    }

    #[test]
    fn scripts_are_read_only_from_the_authenticated_ledger() {
        let folder = scratch("script");
        let service = TerminalService::new(&folder);
        let row = summary(&exec(4), "s", 1, Some(2), "completed");
        let long = "line\n".repeat(200_000);
        write_row(&service, "a", "s", &row, &long);
        let directory = service.instance_dir("a").join(digest("s")).join(&row.execution_id);
        assert_eq!(fs::read_to_string(directory.join("script.txt")).unwrap(), long, "ledger keeps the untruncated script");
        assert!(service.read_ledger_file("a", "../escape", &row.execution_id, "script.txt", "Script", MAX_SCRIPT).is_err());
        assert!(service.read_ledger_file("a", "s", "..", "script.txt", "Script", MAX_SCRIPT).is_err());
        assert_eq!(service.read_script("a", "s", &row.execution_id).unwrap(), long);
        assert!(service.read_output("a", "s", &row.execution_id).is_err(), "no output is saved unless the command failed");
        fs::write(directory.join("output.txt"), "boom\n  at x").unwrap();
        assert_eq!(service.read_output("a", "s", &row.execution_id).unwrap(), "boom\n  at x");
        assert!(service.read_output("a", "s", "..").is_err());
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn a_default_user_shell_runs_commands_streams_output_and_dies_with_its_tab() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex as StdMutex;
        let folder = scratch("shell");
        let service = TerminalService::new(&folder);
        let first = service.open_shell_in("a", folder.to_str().unwrap()).unwrap();
        assert_eq!(service.open_shell_in("a", folder.to_str().unwrap()).unwrap().shell_id, first.shell_id, "one shell per owner");
        let received = Arc::new(StdMutex::new(Vec::<u8>::new()));
        let chunks = Arc::new(AtomicUsize::new(0));
        let sink = received.clone();
        let count = chunks.clone();
        let channel = Channel::new(move |body| {
            if let InvokeResponseBody::Raw(bytes) = body {
                sink.lock().unwrap().extend(bytes);
                count.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        });
        service.attach_user_shell("a", &first.shell_id, channel).unwrap();
        service.resize_user_shell("a", &first.shell_id, 100, 30).unwrap();
        service.write_user_shell("a", &first.shell_id, "echo USER_MARKER_42\r\n").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let text = String::from_utf8_lossy(&received.lock().unwrap()).into_owned();
            if text.matches("USER_MARKER_42").count() >= 2 {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "shell never echoed its marker: {text:?}");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(service.shells.lock().get("a").unwrap().info().cols, 100);
        assert!(service.write_user_shell("a", "stale-id", "x").is_err(), "a stale shell id cannot write");
        let pid = service.shells.lock().get("a").unwrap().pty.lock().process_id().unwrap();
        service.close_user_shell("a", &first.shell_id).unwrap();
        assert!(service.close_user_shell("a", &first.shell_id).is_err());
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
            unsafe {
                let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
                if !handle.is_null() {
                    assert_eq!(WaitForSingleObject(handle, 5000), 0, "closing Tab 0 ends the shell process");
                    CloseHandle(handle);
                }
            }
        }
        #[cfg(unix)]
        {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            // SAFETY: signal 0 only probes for existence of the (already reaped) shell pid.
            while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
                assert!(std::time::Instant::now() < deadline, "closing Tab 0 ends the shell process");
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        let second = service.open_shell_in("a", folder.to_str().unwrap()).unwrap();
        assert_ne!(second.shell_id, first.shell_id, "a recreated shell gets a new id");
        service.close_instance("a");
        assert!(service.close_user_shell("a", &second.shell_id).is_err(), "removal closes the shell");
        let _ = fs::remove_dir_all(folder);
    }
}
