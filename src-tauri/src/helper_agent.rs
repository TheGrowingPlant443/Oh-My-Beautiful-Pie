//! Helper agents: a read-only OMP session per project and purpose that explains or reviews the Code diff changes
//! (`diff`) or searches the repository's history (`git`). The helper has no built-in tools (`--no-tools`); its tools are
//! `repo_inspect` (bridge/omp-repo.mjs) and the read-only saved-results reader `code_quality_report`
//! (bridge/omp-code-quality.mjs, registered for every agent role). It is opened like a plan agent: create, start, wait for the bridge,
//! then send one request prompt.
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::code_diff::{comparison, current_branch, repository_root, Comparison, DiffMode};
use crate::instance::{HelperPurpose, Instance, Origin, Registry};

pub type Result<T, E = String> = std::result::Result<T, E>;

/// The agent imports this module next to the bridge extension.
pub fn materialize(folder: &Path) -> Result<()> {
    let file = folder.join("omp-repo.mjs");
    let source = include_str!("../../bridge/omp-repo.mjs");
    if fs::read_to_string(&file).ok().as_deref() != Some(source) {
        fs::write(file, source).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperRequest {
    Explain,
    Review,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperAdmission {
    pub helper: Instance,
    pub prompt_sent: Option<bool>,
    pub error: Option<String>,
}

fn base_line(base: Option<&str>) -> String {
    match base {
        Some(hash) => format!("Base commit: {hash}."),
        None => "The repository has no commits yet (omit \"base\").".into(),
    }
}

/// The `changes` call that lists the changed files.
fn call(base: Option<&str>) -> String {
    match base {
        Some(hash) => format!("{{\"action\":\"changes\",\"base\":\"{hash}\",\"stat\":true}}"),
        None => "{\"action\":\"changes\",\"stat\":true}".into(),
    }
}

pub enum Ask<'a> {
    Explain(&'a Comparison),
    Review(&'a Comparison),
    Git,
}

pub fn prompt(root: &str, name: &str, branch: Option<&str>, ask: &Ask) -> String {
    let branch = branch.unwrap_or("detached HEAD");
    let preamble = format!("You are a read-only Git helper inside OMP Manager for the repository at {root} (project \"{name}\", branch {branch}). You have no shell and cannot change anything. Your tools are repo_inspect (actions: changes, log, show, diff, blame, file) and the read-only code_quality_report (saved code quality results of this repository). Never claim to have modified, staged, committed or run anything. Keep answers short and in plain language; the user reads them in a terminal. Quote code only as short fenced diff blocks.");
    let rest = match ask {
        Ask::Explain(c) => format!("\n\nThe user opened Code diff and pressed Explain. Comparison: {}. {}\n1. Call repo_inspect {} to list the changed files, then read the patches you need (add \"path\" to read one file; untracked files are listed at the end of the result and can be read with action \"file\").\n2. Reply with a SHORT overall explanation of what this change does and why it matters: at most 5 sentences, no per-file detail, no code.\n3. End with this exact line: Type All for a short explanation of every file.\nIf the user later answers All (any case), give one short sentence per changed file instead and nothing else.", c.label, base_line(c.base_commit.as_deref()), call(c.base_commit.as_deref())),
        Ask::Review(c) => format!("\n\nThe user opened Code diff and pressed Review. Comparison: {}. {}\nRead the changes with repo_inspect (start with {}, then the patches; use action \"file\" to read surrounding code when a hunk is not enough). Then review them like a careful reviewer: list concrete problems ordered by severity — correctness bugs, unhandled edge cases, security issues, missing or weak tests, confusing code — each with the file and the line or hunk it concerns and a one-line fix suggestion. Skip praise and do not restate what the change does. If you find nothing serious, say so in one sentence. Finish with a one-line verdict.", c.label, base_line(c.base_commit.as_deref()), call(c.base_commit.as_deref())),
        Ask::Git => "\n\nThe user opened the Git tree and wants your help searching this repository's history. When they describe logic (\"where did we add X\", \"when did Y stop working\"): search with repo_inspect log (message query, code = text added or removed, regex, path, author, since/until, merges), narrow down candidate commits, inspect them with show / diff / blame, then answer with the short commit hashes, dates, authors, one line on what each commit did, and the relevant diff hunks as short fenced diff blocks. If several commits could match, list them best first and say how sure you are. Reply now with one short sentence saying you are ready.".to_string(),
    };
    preamble + &rest
}

pub struct HelperService {
    /// Per helper, the signature of the last prompt it was sent. The same request for the same change set is never
    /// sent twice; a failed-before-send admission is not recorded so the UI can retry.
    sent: Mutex<HashMap<String, String>>,
}

impl HelperService {
    pub fn new() -> Self {
        Self { sent: Mutex::new(HashMap::new()) }
    }

    /// Find or create the helper for `target_id` and `purpose`, start it, and send the request once.
    pub fn open(&self, registry: &Arc<Registry>, target_id: &str, purpose: HelperPurpose, request: Option<HelperRequest>, mode: Option<DiffMode>) -> Result<HelperAdmission> {
        let target = registry.inspect(target_id, Origin::Human)?;
        if target.plan_target.is_some() || target.helper.is_some() {
            return Err("A helper agent cannot have helpers".into());
        }
        let ask = match (purpose, request, mode) {
            (HelperPurpose::Diff, Some(request), Some(mode @ (DiffMode::Head | DiffMode::Uncommitted | DiffMode::Branch))) => Some((request, mode)),
            (HelperPurpose::Git, None, _) => None,
            _ => return Err("invalid helper request".into()),
        };
        let project = Path::new(&target.project_path);
        let root = repository_root(project)?;
        let repo = root.display().to_string();
        let compared = ask.map(|(_, mode)| comparison(project, mode)).transpose()?;
        let branch = match &compared {
            Some(c) => c.branch.clone(),
            None => current_branch(&root)?,
        };

        // Held to the end so a double click serializes and the second caller sees the claim.
        let mut sent = self.sent.lock();
        let id = match registry.helper_for(target_id, purpose) {
            Some(id) if registry.inspect(&id, Origin::Human)?.helper.is_some_and(|link| link.repo == repo) => id,
            Some(stale) => {
                // The project moved to another repository: its old helper's conversation no longer fits.
                registry.remove(&stale, Origin::Human)?;
                sent.remove(&stale);
                registry.create_helper(target_id, purpose, repo.clone())?.id
            }
            None => registry.create_helper(target_id, purpose, repo.clone())?.id,
        };
        let row = registry.inspect(&id, Origin::Human)?;
        if row.status != "running" && row.status != "starting" {
            registry.start(&id, Origin::Human)?;
        }
        let helper = registry.inspect(&id, Origin::Human)?;
        let signature = format!("{purpose:?}|{request:?}|{}", compared.as_ref().map_or(repo.as_str(), |c| c.fingerprint.as_str()));
        if sent.get(&id) == Some(&signature) {
            return Ok(HelperAdmission { helper, prompt_sent: Some(true), error: None });
        }
        let hub = registry.manager_hub().ok_or("manager bridge not initialized")?;
        if let Err(error) = hub.wait_ready(&id) {
            return Ok(HelperAdmission { helper, prompt_sent: Some(false), error: Some(error) });
        }
        let message = match (request, &compared) {
            (Some(HelperRequest::Explain), Some(c)) => prompt(&repo, &target.name, branch.as_deref(), &Ask::Explain(c)),
            (Some(HelperRequest::Review), Some(c)) => prompt(&repo, &target.name, branch.as_deref(), &Ask::Review(c)),
            _ => prompt(&repo, &target.name, branch.as_deref(), &Ask::Git),
        };
        let outcome = hub.prompt_human(&id, &message);
        sent.insert(id.clone(), signature);
        let helper = registry.inspect(&id, Origin::Human)?;
        Ok(match outcome {
            Ok(_) => HelperAdmission { helper, prompt_sent: Some(true), error: None },
            Err(error) => HelperAdmission {
                helper,
                prompt_sent: None,
                error: Some(format!("Prompt admission outcome unknown: {error}. Inspect the helper's conversation before sending anything else.")),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compared(base: Option<&str>) -> Comparison {
        Comparison { base_commit: base.map(String::from), label: "Uncommitted changes".into(), branch: Some("main".into()), fingerprint: "f".into() }
    }

    #[test]
    fn every_request_names_the_repository_the_only_tool_and_the_exact_first_call() {
        let c = compared(Some("abc123"));
        for ask in [Ask::Explain(&c), Ask::Review(&c), Ask::Git] {
            let text = prompt("C:/repo", "Alpha", Some("main"), &ask);
            assert!(text.starts_with("You are a read-only Git helper inside OMP Manager for the repository at C:/repo (project \"Alpha\", branch main)."));
            assert!(text.contains("Your tools are repo_inspect") && text.contains("code_quality_report"));
        }
        let explain = prompt("C:/repo", "Alpha", Some("main"), &Ask::Explain(&c));
        assert!(explain.contains("Base commit: abc123.") && explain.contains("{\"action\":\"changes\",\"base\":\"abc123\",\"stat\":true}"));
        assert!(explain.contains("End with this exact line: Type All for a short explanation of every file."));
        let review = prompt("C:/repo", "Alpha", None, &Ask::Review(&c));
        assert!(review.contains("branch detached HEAD") && review.contains("Finish with a one-line verdict."));
        let fresh = compared(None);
        let unborn = prompt("C:/repo", "Alpha", Some("main"), &Ask::Explain(&fresh));
        assert!(unborn.contains("{\"action\":\"changes\",\"stat\":true}") && unborn.contains("omit \"base\""));
        assert!(prompt("C:/repo", "Alpha", Some("main"), &Ask::Git).contains("searching this repository's history"));
    }
}
