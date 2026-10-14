//! Plan changer: a human-controlled OMP session that interrupts another session, rewrites its plan and hands the
//! plan back. The plan agent's tool calls reach the target only through `ManagerHub::plan_action`.
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;

use crate::instance::{Instance, InstanceKind, Origin, Registry};
use crate::session;

pub type Result<T, E = String> = std::result::Result<T, E>;

pub const NOT_RUNNING: &str =
    "The target session is not running; ask the user to start it from the dashboard";
const HANDOFF_LIMIT: usize = 60_000;

pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    fs::create_dir_all(path.parent().ok_or("invalid plan path")?).map_err(|e| e.to_string())?;
    let temporary = path.with_extension("md.tmp");
    fs::write(&temporary, text).map_err(|e| e.to_string())?;
    fs::rename(&temporary, path).map_err(|e| e.to_string())
}

#[derive(Clone, serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Revision {
    pub id: String,
    pub session_path: Option<PathBuf>,
    pub path: PathBuf,
    pub text: String,
    pub base_live: Option<String>,
    pub handed_off: bool,
}

#[derive(serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanBackup {
    pub id: String,
    pub saved_at: u64,
    pub text: String,
}

pub fn read_revision(folder: &Path, session_path: &Option<PathBuf>, live: &Option<String>) -> Result<Option<Revision>> {
    let file = folder.join("revision.json");
    let bytes = match fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let revision: Revision = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if revision.session_path != *session_path || (revision.handed_off && revision.base_live != *live) {
        return Ok(None);
    }
    Ok(Some(revision))
}

pub fn save_revision(folder: &Path, revision: &Revision) -> Result<()> {
    write_atomic(&folder.join("revision.json"), &serde_json::to_string(revision).map_err(|e| e.to_string())?)
}

/// Immutable snapshots. Failure to back up aborts the change; never overwrite an existing backup.
pub fn save_backup(folder: &Path, text: &str) -> Result<String> {
    use std::io::Write;
    let directory = folder.join("backups");
    fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let backup = PlanBackup { id: uuid::Uuid::new_v4().to_string(), saved_at: session::now_ms(), text: text.into() };
    let mut file = fs::OpenOptions::new().write(true).create_new(true)
        .open(directory.join(format!("{}.json", backup.id))).map_err(|e| e.to_string())?;
    file.write_all(&serde_json::to_vec(&backup).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    Ok(backup.id)
}

pub fn list_backups(folder: &Path) -> Result<Vec<PlanBackup>> {
    let entries = match fs::read_dir(folder.join("backups")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut backups = Vec::new();
    for entry in entries {
        let file = entry.map_err(|e| e.to_string())?.path();
        if file.extension().is_some_and(|e| e == "json") {
            backups.push(serde_json::from_slice::<PlanBackup>(&fs::read(file).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?);
        }
    }
    backups.sort_by(|a, b| b.saved_at.cmp(&a.saved_at).then_with(|| b.id.cmp(&a.id)));
    Ok(backups)
}

pub fn read_backup(folder: &Path, id: &str) -> Result<PlanBackup> {
    let parsed = uuid::Uuid::parse_str(id).map_err(|_| "invalid backup id")?;
    if parsed.to_string() != id { return Err("invalid backup id".into()); }
    serde_json::from_slice(&fs::read(folder.join("backups").join(format!("{id}.json"))).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

/// The last `max` chars of `text` (never splitting a character).
pub fn tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let start = text
        .char_indices()
        .nth(count - max)
        .map_or(text.len(), |(index, _)| index);
    format!("[…earlier output omitted]\n{}", &text[start..])
}

pub fn handoff_message(path: &Path, text: &str, summary: &str) -> String {
    let body = match text.char_indices().nth(HANDOFF_LIMIT) {
        Some((index, _)) => format!("{}\n[truncated — read the file for the rest]", &text[..index]),
        None => text.to_string(),
    };
    format!(
        "CHANGE OF PLANS (from the user, via the plan changer). Stop what you were doing and continue under the updated plan.\nWhat changed: {summary}\nThe updated plan is saved at {}. Its full text follows.\n\n--- UPDATED PLAN ---\n{body}\n--- END PLAN ---\nAcknowledge the change briefly, adjust your todo list to match, and continue from where you were interrupted.",
        path.display()
    )
}

pub struct ContextParts {
    pub name: String,
    pub kind: &'static str,
    pub project: String,
    pub status: String,
    pub activity: String,
    pub profile: String,
    pub model: String,
    pub task: Option<String>,
    pub todo_done: usize,
    pub todo_total: usize,
    pub subagents: Vec<String>,
    pub plan_path: PathBuf,
    pub plan: Option<String>,
    pub transcript: Result<String>,
    pub screen: Option<String>,
    pub plan_kind: Option<String>,
}

pub fn context_prompt(p: &ContextParts) -> String {
    let profile = if p.profile.is_empty() { "default" } else { &p.profile };
    let model = if p.model.is_empty() { "OMP default" } else { &p.model };
    let subagents = if p.subagents.is_empty() { "none".to_string() } else { p.subagents.join("; ") };
    let plan = p
        .plan
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or("(no plan yet — you may write one with write_plan)");
    let transcript = match &p.transcript {
        Ok(text) => text.clone(),
        Err(error) => format!("(unavailable: {error})"),
    };
    let plan_label = match p.plan_kind.as_deref() {
        Some("todo") => "the session's live todo list, as markdown",
        Some("file") => "its native plan file",
        Some("saved") => "the last plan saved for this agent, from an earlier session",
        _ => "none",
    };
    format!(
        "You are the PLAN CHANGER for another running OMP session: \"{name}\" ({kind}). The user wants to change that session's plan midway through its work. You do not do the session's work yourself.\n\nHow you work:\n1. The user chats with you. Understand the change they want. Use the plan_session tool to inspect the live session: actions status, read_plan, transcript, screen.\n2. To change the plan call plan_session with action write_plan and payload {{\"text\": \"<the COMPLETE new plan, markdown, keeping headings>\"}}. It replaces the whole plan and first interrupts the running session so it stops working before the plan changes. When the plan is the session's live todo list, keep its `# phase` / `- [ ] task` layout; the session is told to adjust its todo list to match your text.\n3. Then call plan_session with action handoff and payload {{\"summary\": \"<what changed and why>\"}}. This sends the updated plan to the session as a \"change of plans\" message. Only tell the user the session was updated after handoff reports accepted.\n4. If a call says the session is not running, tell the user to start it from the dashboard. Do not retry in a loop.\nNever edit the plan file by hand and never perform the session's own tasks. Plan file: {plan_path}\n\nCURRENT STATE of \"{name}\"\nProject: {project} · status: {status}/{activity} · profile: {profile} · model: {model}\nCurrent task: {task} · todo progress: {done}/{total}\nSubagents: {subagents}\n\nCURRENT PLAN ({plan_label})\n{plan}\n\nRECENT CONVERSATION (tail)\n{transcript}\n\nCURRENT SCREEN (tail)\n{screen}",
        name = p.name,
        kind = p.kind,
        plan_path = p.plan_path.display(),
        project = p.project,
        status = p.status,
        activity = p.activity,
        task = p.task.as_deref().unwrap_or("none"),
        done = p.todo_done,
        total = p.todo_total,
        screen = p.screen.as_deref().unwrap_or("(not available)"),
    ) + "\n\nHANDOFF RULE: Always ask the user before handing off. The handoff tool opens a mandatory confirmation. If the target is stopped it offers to start it and hand off; never claim it was started without approval. Saving a revision backs up the previous plan automatically, and the split pane shows that revision before handoff. The pane's Backups control can restore an earlier version without handing it off."
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanAdmission {
    pub planner: Instance,
    pub prompt_sent: Option<bool>,
    pub error: Option<String>,
}

pub struct PlanService {
    /// Per plan agent, the plan identity its last context prompt described. An unchanged identity is never resent;
    /// a failed-before-send admission is not recorded so the UI can retry.
    sent: Mutex<HashMap<String, String>>,
}

impl PlanService {
    pub fn new() -> Self {
        Self { sent: Mutex::new(HashMap::new()) }
    }

    /// Find or create the plan agent for `target_id`, start it, and send its context prompt once.
    pub fn open(&self, registry: &Arc<Registry>, target_id: &str) -> Result<PlanAdmission> {
        let target = registry.inspect(target_id, Origin::Human)?;
        if target.plan_target.is_some() {
            return Err("A plan agent cannot be planned".into());
        }
        if target.plan_kind.is_none() {
            return Err("This session has no active plan to change (none yet, or it is completed)".into());
        }
        // Held to the end so a double click serializes and the second caller sees the claim.
        let mut sent = self.sent.lock();
        let id = match registry.planner_for(target_id) {
            Some(id) => id,
            None => registry.create_planner(target_id)?.id,
        };
        let row = registry.inspect(&id, Origin::Human)?;
        if row.status != "running" && row.status != "starting" {
            registry.start(&id, Origin::Human)?;
        }
        let planner = registry.inspect(&id, Origin::Human)?;
        let materialized = planner.session_path.as_ref().is_some_and(|path| path.is_file());
        let signature = format!("{:?}|{:?}|{:?}", target.session_path, target.plan_kind, registry.plan_file(target_id)?);
        // Context is (re)sent whenever the session's plan identity changed since the last send: a new session after
        // plan approval, a different plan kind or file.
        if sent.get(&id) == Some(&signature) {
            return Ok(PlanAdmission { planner, prompt_sent: Some(true), error: None });
        }
        let first = !materialized && !sent.contains_key(&id);
        let parts = ContextParts {
            name: target.name.clone(),
            kind: if target.kind == InstanceKind::Manager { "The Manager" } else { "session" },
            project: target.project_path.clone(),
            status: target.status.clone(),
            activity: target.activity.clone(),
            profile: target.profile.clone(),
            model: target.model.clone(),
            task: target.task.clone(),
            todo_done: target.todo_done,
            todo_total: target.todo_total,
            subagents: target
                .subagents
                .iter()
                .map(|s| format!("{} · {} · {}", s.name, s.agent.as_deref().unwrap_or("subagent"), s.status))
                .collect(),
            plan_path: registry.plan_file(target_id)?,
            plan: registry.plan(target_id, Origin::Human).ok().flatten(),
            transcript: registry
                .main_transcript(target_id, Origin::Human)
                .and_then(|(path, input)| session::render_transcript_file(&path, &input))
                .map(|text| tail(&text, 30_000)),
            screen: registry
                .screen(target_id, Origin::Human)
                .ok()
                .and_then(|value| value["text"].as_str().map(|text| tail(text, 6_000))),
            plan_kind: target.plan_kind.clone(),
        };
        let hub = registry.manager_hub().ok_or("manager bridge not initialized")?;
        if let Err(error) = hub.wait_ready(&id) {
            return Ok(PlanAdmission { planner, prompt_sent: Some(false), error: Some(error) });
        }
        let message = context_prompt(&parts);
        let message = if first {
            message
        } else {
            format!("UPDATED CONTEXT — the user reopened the plan view for this session; use this instead of any earlier picture of it.\n\n{message}")
        };
        let outcome = hub.prompt_human(&id, &message);
        sent.insert(id.clone(), signature);
        let planner = registry.inspect(&id, Origin::Human)?;
        Ok(match outcome {
            Ok(_) => PlanAdmission { planner, prompt_sent: Some(true), error: None },
            Err(error) => PlanAdmission {
                planner,
                prompt_sent: None,
                error: Some(format!("Prompt admission outcome unknown: {error}. Inspect the plan agent's conversation before sending anything else.")),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_never_splits_a_character_and_marks_the_cut() {
        assert_eq!(tail("héllo", 10), "héllo");
        let cut = tail("aéé😀z", 3);
        assert_eq!(cut, "[…earlier output omitted]\né😀z");
        assert!(tail("x".repeat(100).as_str(), 5).ends_with("xxxxx"));
    }

    #[test]
    fn handoff_names_the_change_and_carries_the_whole_plan() {
        let message = handoff_message(Path::new("C:/plans/p.md"), "# New\nstep", "dropped the migration");
        assert!(message.starts_with("CHANGE OF PLANS"));
        assert!(message.contains("What changed: dropped the migration"));
        assert!(message.contains("C:/plans/p.md"));
        assert!(message.contains("--- UPDATED PLAN ---\n# New\nstep\n--- END PLAN ---"));
        let long = handoff_message(Path::new("p.md"), &"é".repeat(HANDOFF_LIMIT + 5), "s");
        assert!(long.contains("[truncated — read the file for the rest]"));
        assert_eq!(long.matches('é').count(), HANDOFF_LIMIT);
    }

    #[test]
    fn context_prompt_reports_state_and_every_fallback() {
        let mut parts = ContextParts {
            name: "api-worker".into(),
            kind: "session",
            project: "C:/repo".into(),
            status: "running".into(),
            activity: "working".into(),
            profile: String::new(),
            model: String::new(),
            task: Some("Wire the endpoint".into()),
            todo_done: 2,
            todo_total: 5,
            subagents: vec!["Scout1 · explore · done".into()],
            plan_path: PathBuf::from("C:/plans/p.md"),
            plan: Some("# Plan\nstep one".into()),
            transcript: Ok("user: go\nassistant: ok".into()),
            screen: Some("prompt>".into()),
            plan_kind: Some("file".into()),
        };
        let text = context_prompt(&parts);
        assert!(text.contains("PLAN CHANGER for another running OMP session: \"api-worker\" (session)"));
        assert!(text.contains("Current task: Wire the endpoint · todo progress: 2/5"));
        assert!(text.contains("profile: default · model: OMP default"));
        assert!(text.contains("Subagents: Scout1 · explore · done"));
        assert!(text.contains("CURRENT PLAN (its native plan file)\n# Plan\nstep one"));
        assert!(text.contains("RECENT CONVERSATION (tail)\nuser: go"));
        parts.plan_kind = Some("todo".into());
        assert!(context_prompt(&parts).contains("CURRENT PLAN (the session's live todo list, as markdown)\n# Plan"));
        parts.plan_kind = Some("saved".into());
        assert!(context_prompt(&parts).contains("CURRENT PLAN (the last plan saved for this agent, from an earlier session)\n# Plan"));
        parts.plan = None;
        parts.task = None;
        parts.subagents.clear();
        parts.transcript = Err("omp render failed".into());
        parts.screen = None;
        let text = context_prompt(&parts);
        assert!(text.contains("(no plan yet — you may write one with write_plan)"));
        assert!(text.contains("Current task: none"));
        assert!(text.contains("Subagents: none"));
        assert!(text.contains("(unavailable: omp render failed)"));
        assert!(text.contains("CURRENT SCREEN (tail)\n(not available)"));
    }

    #[test]
    fn plans_are_written_atomically_over_existing_files() {
        let folder = std::env::temp_dir().join(format!("omp-plan-write-{}", uuid::Uuid::new_v4()));
        let file = folder.join("nested/plan.md");
        write_atomic(&file, "one").unwrap();
        write_atomic(&file, "two").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "two");
        assert!(!file.with_extension("md.tmp").exists());
        fs::remove_dir_all(folder).unwrap();
    }
}
