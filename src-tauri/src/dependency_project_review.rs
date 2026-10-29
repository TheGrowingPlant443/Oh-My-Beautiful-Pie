//! Project-wide security review: every DIRECT dependency of a scanned project, built from the per-package security
//! reviews of `DependencySecurityService` so already saved or in-flight work is joined, never repeated. Newly needed work
//! is dispatched with ONE Architect prompt; the aggregate is compiled locally from saved reports when every child is
//! terminal, so no separate summary-model call exists. Nothing in this module issues a metadata or network request.
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::dependencies::{self, DepNode, DependencyGraph};
use crate::dependency_context::{self, DependencyContextService, DependencyContextSummary};
use crate::dependency_security::{
    self, DependencyJobAdmission, DependencyJobKind, DependencySecurityService, JobDisposition,
    SecurityReviewState, SecurityReviewStatus,
};
use crate::inhouse::{InhouseService, InhouseStatus};
use crate::instance::{Instance, InstanceKind, Registry};
use crate::manager::DependencyProducer;
use crate::package_insights::PackageInsightsService;

pub type Result<T, E = String> = std::result::Result<T, E>;
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;
const MAX_RESULT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_WORK_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const COMPILED_NOTE: &str = "Compiled from saved package reviews; no separate summary-model call.";
const BLOCKED: &str =
    "Public lookups are blocked until The Architect has identified in-house packages.";
const SUMMARY_UNITS: usize = 20_000;
const LIMITATION_UNITS: usize = 2_000;
const MAX_LIMITATIONS: usize = 50;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CoverageStatus {
    Pending,
    Complete,
    Error,
    SkippedInhouse,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCoverage {
    pub package_id: String,
    pub request_id: Option<String>,
    pub status: CoverageStatus,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectReviewState {
    pub request_id: String,
    pub folder: String,
    pub context_id: Option<String>,
    pub status: SecurityReviewStatus,
    pub coverage: Vec<ProjectCoverage>,
    pub summary: Option<String>,
    pub limitations: Vec<String>,
    pub completed_at: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectRequest {
    schema_version: u32,
    kind: String,
    request_id: String,
    folder: String,
    folder_key: String,
    context_id: Option<String>,
    created_at: u64,
    rerun: bool,
    // Present only when this project dispatched new work; the bridge validates the full tuple on inspection.
    architect_instance_id: Option<String>,
    architect_generation: Option<u64>,
    architect_launch_id: Option<String>,
    architect_session_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum WorkState {
    Reused,
    New,
    Skipped,
    Failed,
}

/// One line of `work.jsonl`: what this project did with one direct dependency when it started.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkEntry {
    package_id: String,
    request_id: Option<String>,
    state: WorkState,
    reason: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SavedAdmission {
    prompt_sent: Option<bool>,
    error: Option<String>,
}

/// What the aggregate needs from a terminal child; parsed once, then cached.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ChildSummary {
    status: SecurityReviewStatus,
    error: Option<String>,
    /// `(title, severity, applicability status)` of each saved finding.
    findings: Vec<(String, String, String)>,
}

#[derive(Default)]
struct Index {
    requests: HashMap<String, Arc<ProjectRequest>>,
    latest: HashMap<String, String>,
    // Project requests with no saved result: the one-second tick reconciles only these.
    pending: BTreeSet<String>,
    done: HashMap<String, Arc<ProjectReviewState>>,
    children: HashMap<String, ChildSummary>,
}

pub struct DependencyProjectReviewService {
    root: PathBuf,
    index: Mutex<Index>,
    // Serializes project starts so concurrent clicks join one request; never taken by read/reconcile.
    start_gate: Mutex<()>,
    finalize_gate: Mutex<()>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn validate_id(id: &str) -> Result<()> {
    let parsed = Uuid::parse_str(id).map_err(|_| "Invalid project review request id")?;
    if parsed.to_string() != id {
        return Err("Invalid project review request id".into());
    }
    Ok(())
}

/// Truncate to at most `maximum` UTF-16 units without splitting a character.
fn clip(text: &str, maximum: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for character in text.chars() {
        units += character.len_utf16();
        if units > maximum {
            break;
        }
        out.push(character);
    }
    out
}

fn bounded_json<T: DeserializeOwned>(path: &Path, maximum: u64) -> Result<Option<T>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > maximum {
        return Err("Invalid or oversized project review file".into());
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|error| error.to_string())?
        .take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > maximum {
        return Err("Oversized project review file".into());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    Ok(())
}

fn atomic_json(path: &Path, value: &impl Serialize, maximum: u64) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > maximum {
        return Err("Oversized project review file".into());
    }
    atomic_write(path, &bytes)
}

fn child_summary(state: &SecurityReviewState) -> ChildSummary {
    let report = serde_json::to_value(&state.report).unwrap_or(serde_json::Value::Null);
    let text = |value: &serde_json::Value, key: &str| {
        value
            .get(key)
            .and_then(|field| field.as_str())
            .unwrap_or("unknown")
            .to_string()
    };
    let findings = report
        .get("findings")
        .and_then(|findings| findings.as_array())
        .map(|items| {
            items
                .iter()
                .map(|finding| {
                    (
                        text(finding, "title"),
                        text(finding, "severity"),
                        finding.get("applicability").map_or_else(
                            || "unknown".to_string(),
                            |applicability| text(applicability, "status"),
                        ),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    ChildSummary {
        status: state.status,
        error: state.error.clone(),
        findings,
    }
}

fn severity_rank(severity: &str) -> u8 {
    match severity {
        "critical" => 4,
        "high" => 3,
        "moderate" => 2,
        "low" => 1,
        _ => 0,
    }
}

/// Exact coverage for one direct dependency; `child` is its current terminal/pending summary when it has a request.
fn coverage_for(entry: &WorkEntry, child: Option<&ChildSummary>) -> ProjectCoverage {
    let package_id = entry.package_id.clone();
    let request_id = entry.request_id.clone();
    match (entry.state, child) {
        (WorkState::Skipped, _) => ProjectCoverage {
            package_id,
            request_id: None,
            status: CoverageStatus::SkippedInhouse,
            reason: entry.reason.clone(),
        },
        (WorkState::Failed, _) | (_, None) => ProjectCoverage {
            package_id,
            request_id,
            status: CoverageStatus::Error,
            reason: Some(
                entry
                    .reason
                    .clone()
                    .unwrap_or_else(|| "The package review could not be read.".into()),
            ),
        },
        (_, Some(child)) => match child.status {
            SecurityReviewStatus::Complete => ProjectCoverage {
                package_id,
                request_id,
                status: CoverageStatus::Complete,
                reason: None,
            },
            SecurityReviewStatus::Error => ProjectCoverage {
                package_id,
                request_id,
                status: CoverageStatus::Error,
                reason: Some(
                    child
                        .error
                        .clone()
                        .unwrap_or_else(|| "The package review failed.".into()),
                ),
            },
            SecurityReviewStatus::Pending => ProjectCoverage {
                package_id,
                request_id,
                status: CoverageStatus::Pending,
                reason: child.error.clone(),
            },
        },
    }
}

/// Compose the aggregate summary and limitations from saved reviews only. Never claims "all safe".
fn compile(
    coverage: &[ProjectCoverage],
    children: &HashMap<String, ChildSummary>,
) -> (String, Vec<String>) {
    let count =
        |status: CoverageStatus| coverage.iter().filter(|item| item.status == status).count();
    let mut summary = format!(
        "{COMPILED_NOTE}\nDirect dependencies: {} — reviewed {}, failed {}, skipped as in-house {}.\n",
        coverage.len(), count(CoverageStatus::Complete), count(CoverageStatus::Error), count(CoverageStatus::SkippedInhouse));
    let mut with_findings = 0;
    let completed = coverage
        .iter()
        .filter(|item| item.status == CoverageStatus::Complete);
    for item in completed {
        let Some(child) = item.request_id.as_ref().and_then(|id| children.get(id)) else {
            continue;
        };
        if child.findings.is_empty() {
            summary.push_str(&format!(
                "- {}: no verified findings reported (not a clean bill of health).\n",
                item.package_id
            ));
            continue;
        }
        with_findings += 1;
        let highest = child
            .findings
            .iter()
            .map(|finding| finding.1.as_str())
            .max_by_key(|severity| severity_rank(severity))
            .unwrap_or("unknown");
        let applicable = |status: &str| {
            child
                .findings
                .iter()
                .filter(|finding| finding.2 == status)
                .count()
        };
        let titles: Vec<String> = child
            .findings
            .iter()
            .take(5)
            .map(|finding| format!("{} [{}]", finding.0, finding.1))
            .collect();
        summary.push_str(&format!(
            "- {}: {} finding(s), highest {}; {} relevant to this project, {} not relevant, {} unknown applicability — {}{}\n",
            item.package_id, child.findings.len(), highest, applicable("relevant"), applicable("notRelevant"), applicable("unknown"),
            titles.join("; "), if child.findings.len() > 5 { "; …" } else { "" }));
    }
    let mut limitations = vec![
        "These are sourced model reports, not independently verified vulnerability verdicts; no findings is not a clean bill of health.".to_string(),
        "Only direct dependencies were reviewed as separate jobs; transitive packages were consulted only where relevant to a finding.".to_string(),
    ];
    let failed: Vec<&ProjectCoverage> = coverage
        .iter()
        .filter(|item| item.status == CoverageStatus::Error)
        .collect();
    let shown_failures = failed.iter().take(20);
    for item in shown_failures {
        limitations.push(clip(
            &format!(
                "{}: review failed — {}",
                item.package_id,
                item.reason.as_deref().unwrap_or("unknown error")
            ),
            LIMITATION_UNITS,
        ));
    }
    if failed.len() > 20 {
        limitations.push(format!(
            "{} more packages failed; see their coverage entries.",
            failed.len() - 20
        ));
    }
    let skipped: Vec<&str> = coverage
        .iter()
        .filter(|item| item.status == CoverageStatus::SkippedInhouse)
        .map(|item| item.package_id.as_str())
        .collect();
    if !skipped.is_empty() {
        let shown = skipped
            .iter()
            .take(20)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        limitations.push(clip(&format!("{} package(s) marked in-house were not looked up publicly and were not reviewed: {shown}{}", skipped.len(), if skipped.len() > 20 { ", …" } else { "" }), LIMITATION_UNITS));
    }
    limitations.truncate(MAX_LIMITATIONS);
    if with_findings == 0 && count(CoverageStatus::Complete) == 0 {
        summary.push_str("No package review completed.\n");
    }
    (clip(&summary, SUMMARY_UNITS), limitations)
}

impl DependencyProjectReviewService {
    pub fn new(config_root: PathBuf) -> Self {
        let service = Self {
            root: config_root.join("architect").join("project-security"),
            index: Mutex::new(Index::default()),
            start_gate: Mutex::new(()),
            finalize_gate: Mutex::new(()),
        };
        service.restore();
        service
    }

    fn directory(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn safe_directory(&self, id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        let directory = self.directory(id);
        for path in [&self.root, &directory] {
            let meta = fs::symlink_metadata(path)
                .map_err(|error| format!("Unknown project review request: {error}"))?;
            if !meta.is_dir() || meta.file_type().is_symlink() {
                return Err("Invalid project review directory".into());
            }
        }
        Ok(directory)
    }

    fn load_request(&self, id: &str) -> Result<Arc<ProjectRequest>> {
        if let Some(request) = self.index.lock().requests.get(id) {
            return Ok(request.clone());
        }
        let directory = self.safe_directory(id)?;
        let request: ProjectRequest =
            bounded_json(&directory.join("request.json"), MAX_REQUEST_BYTES)?
                .ok_or("Unknown project review request")?;
        if request.schema_version != 2
            || request.kind != "project"
            || request.request_id != id
            || !Path::new(&request.folder).is_absolute()
            || request.created_at > MAX_SAFE_INTEGER
            || request
                .context_id
                .as_deref()
                .is_some_and(|context| !dependency_context::valid_context_id(context))
        {
            return Err("Invalid project review request".into());
        }
        let request = Arc::new(request);
        self.index
            .lock()
            .requests
            .insert(id.to_string(), request.clone());
        Ok(request)
    }

    /// Restore the latest request per project and the unfinished ones; reads local files only.
    fn restore(&self) {
        let mut found: Vec<(u64, Option<SystemTime>, String, Arc<ProjectRequest>)> = Vec::new();
        let entries = fs::read_dir(&self.root).into_iter().flatten().flatten();
        for item in entries {
            let id = item.file_name().to_string_lossy().into_owned();
            let Ok(request) = self.load_request(&id) else {
                continue;
            };
            let modified = item.metadata().and_then(|meta| meta.modified()).ok();
            found.push((request.created_at, modified, id, request));
        }
        found.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
        let mut index = self.index.lock();
        for (_, _, id, request) in found {
            index.latest.insert(request.folder_key.clone(), id.clone());
            if !self.directory(&id).join("result.json").exists() {
                index.pending.insert(id);
            }
        }
    }

    fn architect_row(registry: &Registry) -> Option<Instance> {
        registry
            .snapshot()
            .into_iter()
            .find(|row| row.kind == InstanceKind::Architect)
    }

    fn architect_id(registry: &Registry) -> Option<String> {
        Self::architect_row(registry).map(|row| row.id)
    }

    fn work(&self, id: &str) -> Result<Vec<WorkEntry>> {
        let directory = self.safe_directory(id)?;
        let path = directory.join("work.jsonl");
        let meta = fs::symlink_metadata(&path)
            .map_err(|error| format!("Project review manifest is missing: {error}"))?;
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_WORK_BYTES {
            return Err("Invalid project review manifest".into());
        }
        let reader = BufReader::new(File::open(&path).map_err(|error| error.to_string())?);
        let mut entries = Vec::new();
        for line in reader.lines() {
            let line = line.map_err(|error| error.to_string())?;
            if line.trim().is_empty() {
                continue;
            }
            entries.push(
                serde_json::from_str::<WorkEntry>(&line)
                    .map_err(|error| format!("Invalid project review manifest: {error}"))?,
            );
        }
        Ok(entries)
    }

    /// The latest security request of a direct package that this project may join instead of creating new work.
    fn reusable(
        &self,
        registry: &Registry,
        security: &DependencySecurityService,
        key: &str,
        package_id: &str,
        rerun: bool,
    ) -> Option<String> {
        let id = security.latest_request(key, DependencyJobKind::Security, package_id)?;
        match security.read(registry, &id) {
            Ok(state) if state.status == SecurityReviewStatus::Pending || !rerun => Some(id),
            _ => None,
        }
    }

    fn child(
        &self,
        registry: &Registry,
        security: &DependencySecurityService,
        id: &str,
    ) -> Option<ChildSummary> {
        if let Some(child) = self.index.lock().children.get(id) {
            return Some(child.clone());
        }
        let state = security.read(registry, id).ok()?;
        let child = child_summary(&state);
        if child.status != SecurityReviewStatus::Pending {
            self.index
                .lock()
                .children
                .insert(id.to_string(), child.clone());
        }
        Some(child)
    }

    /// A saved or running review of this project that satisfies the request, or nothing when fresh work is needed.
    fn saved_admission(
        &self,
        registry: &Registry,
        security: &DependencySecurityService,
        key: &str,
        rerun: bool,
    ) -> Result<Option<DependencyJobAdmission>> {
        let previous = self.index.lock().latest.get(key).cloned();
        let Some(previous) = previous else {
            return Ok(None);
        };
        match self.read(registry, security, &previous) {
            Ok(state) if state.status == SecurityReviewStatus::Pending => Ok(Some(self.admission(
                registry,
                &previous,
                JobDisposition::Pending,
                None,
            ))),
            Ok(state) if !rerun => {
                let disposition = if state.status == SecurityReviewStatus::Error {
                    JobDisposition::Error
                } else {
                    JobDisposition::Complete
                };
                Ok(Some(self.admission(
                    registry,
                    &previous,
                    disposition,
                    state.error,
                )))
            }
            Err(error) if !rerun => {
                Err(format!("The saved project review cannot be read: {error}"))
            }
            _ => Ok(None),
        }
    }

    fn inhouse_skip(policy: &InhouseStatus, node: &DepNode) -> Option<WorkEntry> {
        if !policy.gate {
            return None;
        }
        let reason = policy.inhouse.get(&node.id)?;
        let reason = if reason.trim().is_empty() {
            "Marked in-house; never looked up publicly.".to_string()
        } else {
            format!("Marked in-house: {reason}")
        };
        Some(WorkEntry {
            package_id: node.id.clone(),
            request_id: None,
            state: WorkState::Skipped,
            reason: Some(clip(&reason, LIMITATION_UNITS)),
        })
    }

    /// Skipped and reused entries, plus the packages that still need a review.
    fn plan_work(
        &self,
        registry: &Registry,
        security: &DependencySecurityService,
        key: &str,
        rerun: bool,
        policy: &InhouseStatus,
        directs: &[&DepNode],
    ) -> (Vec<WorkEntry>, Vec<String>) {
        let mut entries: Vec<WorkEntry> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        for node in directs {
            if let Some(entry) = Self::inhouse_skip(policy, node) {
                entries.push(entry);
                continue;
            }
            match self.reusable(registry, security, key, &node.id, rerun) {
                Some(id) => entries.push(WorkEntry {
                    package_id: node.id.clone(),
                    request_id: Some(id),
                    state: WorkState::Reused,
                    reason: None,
                }),
                None => missing.push(node.id.clone()),
            }
        }
        (entries, missing)
    }

    /// Claim a security request per missing package; returns the entries and the ids of newly created requests.
    fn claim_missing(
        registry: &Registry,
        security: &DependencySecurityService,
        graph: &DependencyGraph,
        context_id: &str,
        missing: &[String],
        rerun: bool,
        bound: &DependencyProducer,
    ) -> (Vec<WorkEntry>, Vec<String>) {
        let mut entries: Vec<WorkEntry> = Vec::new();
        let mut new_ids: Vec<String> = Vec::new();
        for package_id in missing {
            match security.claim_selected(registry, graph, context_id, package_id, rerun, bound) {
                Ok(claim) => {
                    let state = if matches!(claim.disposition, JobDisposition::New) {
                        WorkState::New
                    } else {
                        WorkState::Reused
                    };
                    if state == WorkState::New {
                        new_ids.push(claim.request_id.clone());
                    }
                    entries.push(WorkEntry {
                        package_id: package_id.clone(),
                        request_id: Some(claim.request_id),
                        state,
                        reason: None,
                    });
                }
                Err(error) => entries.push(WorkEntry {
                    package_id: package_id.clone(),
                    request_id: None,
                    state: WorkState::Failed,
                    reason: Some(clip(&error, LIMITATION_UNITS)),
                }),
            }
        }
        (entries, new_ids)
    }

    /// Send the one Architect prompt and record its admission outcome; returns `(prompt_sent, error)`.
    fn dispatch_prompt(
        &self,
        registry: &Registry,
        security: &DependencySecurityService,
        request_id: &str,
        message: &str,
        new_ids: &[String],
        producer: &DependencyProducer,
    ) -> (Option<bool>, Option<String>) {
        let outcome = match registry.manager_hub() {
            Some(hub) => hub
                .prompt_human_generation(&producer.instance_id, producer.generation, message)
                .map(|_| ()),
            None => Err("manager bridge not initialized".into()),
        };
        let (prompt_sent, error) = match outcome {
            Ok(()) => (Some(true), None),
            Err(failure) => (None, Some(format!("Prompt admission outcome unknown: {failure}. Inspect The Architect's conversation before sending anything else; a late report can still arrive."))),
        };
        security.finish_claims(new_ids, prompt_sent, error.clone());
        let _ = atomic_json(
            &self.directory(request_id).join("admission.json"),
            &SavedAdmission {
                prompt_sent,
                error: error.clone(),
            },
            MAX_REQUEST_BYTES,
        );
        (prompt_sent, error)
    }

    pub fn start(
        &self,
        registry: &Arc<Registry>,
        security: &DependencySecurityService,
        insights: &PackageInsightsService,
        inhouse: &InhouseService,
        context: &DependencyContextService,
        folder: &Path,
        rerun: bool,
    ) -> Result<DependencyJobAdmission> {
        let _start = self.start_gate.lock();
        let folder =
            fs::canonicalize(folder).map_err(|error| format!("Invalid project folder: {error}"))?;
        let graph = dependencies::scan(&folder, &registry.home, /* transitive */ true)?;
        let key = graph.key.clone();
        if let Some(admission) = self.saved_admission(registry, security, &key, rerun)? {
            return Ok(admission);
        }
        // Fresh work: refresh the privacy snapshot from this authoritative scan before any public lookup or prompt.
        let policy = inhouse.review_policy(&graph)?;
        if policy.gate && !policy.complete {
            return Err(BLOCKED.into());
        }
        let mut directs: Vec<&DepNode> = graph
            .nodes
            .iter()
            .filter(|node| node.direct && node.id != "project" && node.ecosystem != "project")
            .collect();
        directs.sort_by(|a, b| a.id.cmp(&b.id));
        let request_id = Uuid::new_v4().to_string();
        if directs.is_empty() {
            let request = self.request(&request_id, &graph, None, rerun, None);
            self.persist(&request, &[])?;
            self.finalize_with(&request, Vec::new(), &HashMap::new(), false)?;
            self.register(&request, false);
            return Ok(self.admission(registry, &request_id, JobDisposition::Complete, None));
        }
        let summary = context.publish(registry, insights, inhouse, &graph)?;
        let _ = context.activate(&summary);
        let (mut entries, missing) =
            self.plan_work(registry, security, &key, rerun, &policy, &directs);
        let mut producer: Option<DependencyProducer> = None;
        let mut new_ids: Vec<String> = Vec::new();
        if !missing.is_empty() {
            // The Architect is needed only when some package has no saved or in-flight review; failure claims nothing.
            let bound = dependency_security::ensure_producer(registry)?;
            let (claimed, ids) = Self::claim_missing(
                registry,
                security,
                &graph,
                &summary.id,
                &missing,
                rerun,
                &bound,
            );
            entries.extend(claimed);
            new_ids = ids;
            if !new_ids.is_empty() {
                producer = Some(bound);
            }
        }
        entries.sort_by(|a, b| a.package_id.cmp(&b.package_id));
        let request = self.request(
            &request_id,
            &graph,
            Some(summary.id.clone()),
            rerun,
            producer.as_ref(),
        );
        self.persist_claimed(&request, &entries, security, &new_ids)?;
        let (prompt_sent, error) = match &producer {
            Some(bound) => {
                let message = self.prompt(&request, &graph, &summary, new_ids.len(), entries.len());
                self.dispatch_prompt(registry, security, &request_id, &message, &new_ids, bound)
            }
            None => (None, None),
        };
        self.register(&request, true);
        let state = self.refresh(registry, security, &request_id)?;
        let disposition = if !new_ids.is_empty() {
            JobDisposition::New
        } else if state.status == SecurityReviewStatus::Pending {
            JobDisposition::Pending
        } else if state.status == SecurityReviewStatus::Error {
            JobDisposition::Error
        } else {
            JobDisposition::Complete
        };
        let architect_id = producer
            .map(|bound| bound.instance_id)
            .or_else(|| Self::architect_id(registry));
        Ok(DependencyJobAdmission {
            request_id,
            architect_id,
            prompt_sent,
            error,
            disposition,
        })
    }

    fn persist_claimed(
        &self,
        request: &ProjectRequest,
        entries: &[WorkEntry],
        security: &DependencySecurityService,
        new_ids: &[String],
    ) -> Result<()> {
        if let Err(error) = self.persist(request, entries) {
            if !new_ids.is_empty() {
                security.finish_claims(
                    new_ids,
                    Some(false),
                    Some(format!("The project review could not be saved: {error}")),
                );
            }
            return Err(error);
        }
        Ok(())
    }

    fn admission(
        &self,
        registry: &Registry,
        request_id: &str,
        disposition: JobDisposition,
        error: Option<String>,
    ) -> DependencyJobAdmission {
        let saved = self.safe_directory(request_id).ok().and_then(|directory| {
            bounded_json::<SavedAdmission>(&directory.join("admission.json"), MAX_REQUEST_BYTES)
                .ok()
                .flatten()
        });
        let request = self.load_request(request_id).ok();
        // A bound project reports the Architect that was given the work; reused or complete ones the configured Architect.
        let architect_id = request
            .and_then(|request| request.architect_instance_id.clone())
            .or_else(|| Self::architect_id(registry));
        DependencyJobAdmission {
            request_id: request_id.to_string(),
            architect_id,
            prompt_sent: saved.as_ref().and_then(|saved| saved.prompt_sent),
            error: error.or_else(|| saved.and_then(|saved| saved.error)),
            disposition,
        }
    }

    fn request(
        &self,
        id: &str,
        graph: &DependencyGraph,
        context_id: Option<String>,
        rerun: bool,
        producer: Option<&DependencyProducer>,
    ) -> ProjectRequest {
        ProjectRequest {
            schema_version: 2,
            kind: "project".into(),
            request_id: id.into(),
            folder: graph.folder.clone(),
            folder_key: graph.key.clone(),
            context_id,
            created_at: now_ms(),
            rerun,
            architect_instance_id: producer.map(|bound| bound.instance_id.clone()),
            architect_generation: producer.map(|bound| bound.generation),
            architect_launch_id: producer.map(|bound| bound.launch_id.clone()),
            architect_session_id: producer.map(|bound| bound.session_id.clone()),
        }
    }

    /// Manifest first, request.json last: a directory without request.json is never restored or joined.
    fn persist(&self, request: &ProjectRequest, entries: &[WorkEntry]) -> Result<()> {
        fs::create_dir_all(&self.root).map_err(|error| error.to_string())?;
        let directory = self.directory(&request.request_id);
        fs::create_dir(&directory).map_err(|error| error.to_string())?;
        let mut lines = Vec::new();
        for entry in entries {
            serde_json::to_writer(&mut lines, entry).map_err(|error| error.to_string())?;
            lines.push(b'\n');
        }
        if lines.len() as u64 > MAX_WORK_BYTES {
            return Err("The project review manifest is too large".into());
        }
        atomic_write(&directory.join("work.jsonl"), &lines)?;
        atomic_json(&directory.join("request.json"), request, MAX_REQUEST_BYTES)
    }

    fn register(&self, request: &ProjectRequest, pending: bool) {
        let mut index = self.index.lock();
        index
            .requests
            .insert(request.request_id.clone(), Arc::new(request.clone()));
        index
            .latest
            .insert(request.folder_key.clone(), request.request_id.clone());
        if pending {
            index.pending.insert(request.request_id.clone());
        }
    }

    fn prompt(
        &self,
        request: &ProjectRequest,
        graph: &DependencyGraph,
        summary: &DependencyContextSummary,
        new_children: usize,
        total: usize,
    ) -> String {
        format!(
            "Project dependency security review. Request id: {id}. Project folder: {folder}.\n{context}\n\n\
This is ONE request for every direct dependency of the project ({total} in scope, {new_children} newly requested). The work list is app-owned and paged: call {tool} with requestId \"{id}\" and section \"work\" (offset/limit; follow nextOffset until it is absent). \
Each work entry has its own requestId and a current status computed from saved files. For EVERY entry whose state is \"new\" and whose current status is \"pending\", perform an individual security review as specified below and submit it by calling submit_dependency_security_review with THAT entry's own requestId (never this project request id). \
Do NOT review again any entry that is already complete, errored, skipped, \"reused\" or \"failed\": that work is saved or joined elsewhere. Do not review transitive-only packages as separate jobs, and do not send per-package questions to the user; consult transitive dependencies (section package/edges) only where they matter to a finding. \
Do not submit a project summary: the app compiles the aggregate from your saved package reviews. Project key: {key}.\n\n\
Per-package requirements:\n{requirements}",
            id = request.request_id, folder = graph.folder, context = dependency_context::prompt_block(summary), total = total, new_children = new_children,
            tool = dependency_context::TOOL_NAME, key = graph.key, requirements = dependency_security::SECURITY_REQUIREMENTS)
    }

    /// Read a project review. A finished result is returned as saved; otherwise live child statuses are reconciled and,
    /// when every child is terminal, the aggregate is persisted. Local files only.
    pub fn read(
        &self,
        registry: &Registry,
        security: &DependencySecurityService,
        request_id: &str,
    ) -> Result<ProjectReviewState> {
        self.refresh(registry, security, request_id)
    }

    fn refresh(
        &self,
        registry: &Registry,
        security: &DependencySecurityService,
        request_id: &str,
    ) -> Result<ProjectReviewState> {
        validate_id(request_id)?;
        if let Some(done) = self.index.lock().done.get(request_id) {
            return Ok((**done).clone());
        }
        let request = self.load_request(request_id)?;
        let directory = self.safe_directory(request_id)?;
        if let Some(saved) =
            bounded_json::<ProjectReviewState>(&directory.join("result.json"), MAX_RESULT_BYTES)?
        {
            if saved.request_id != request_id {
                return Err("Project review result does not match its request".into());
            }
            let mut index = self.index.lock();
            index.pending.remove(request_id);
            index
                .done
                .insert(request_id.to_string(), Arc::new(saved.clone()));
            return Ok(saved);
        }
        let entries = self.work(request_id)?;
        let mut children = HashMap::new();
        let mut coverage = Vec::with_capacity(entries.len());
        for entry in &entries {
            let summary = entry
                .request_id
                .as_ref()
                .filter(|_| entry.state != WorkState::Failed && entry.state != WorkState::Skipped)
                .and_then(|id| {
                    let child = self.child(registry, security, id);
                    if let Some(child) = &child {
                        children.insert(id.clone(), child.clone());
                    }
                    child
                });
            coverage.push(coverage_for(entry, summary.as_ref()));
        }
        if coverage
            .iter()
            .any(|item| item.status == CoverageStatus::Pending)
        {
            return Ok(ProjectReviewState {
                request_id: request_id.into(),
                folder: request.folder.clone(),
                context_id: request.context_id.clone(),
                status: SecurityReviewStatus::Pending,
                coverage,
                summary: None,
                limitations: Vec::new(),
                completed_at: None,
                error: None,
            });
        }
        let displayed = self.finalize_with(&request, coverage, &children, true)?;
        if let Some(state) = self.index.lock().done.get(request_id).cloned() {
            if displayed {
                self.display(registry, &request);
            }
            return Ok((*state).clone());
        }
        Err("Project review finalization failed".into())
    }

    /// Persist the final aggregate exactly once. Returns true only for the call that wrote it.
    fn finalize_with(
        &self,
        request: &ProjectRequest,
        coverage: Vec<ProjectCoverage>,
        children: &HashMap<String, ChildSummary>,
        from_work: bool,
    ) -> Result<bool> {
        let _final = self.finalize_gate.lock();
        let id = &request.request_id;
        if self.index.lock().done.contains_key(id)
            || self.directory(id).join("result.json").exists()
        {
            return Ok(false);
        }
        let (summary, limitations) = if coverage.is_empty() && !from_work {
            (
                "This project has no direct dependencies to review; no model was prompted."
                    .to_string(),
                Vec::new(),
            )
        } else {
            compile(&coverage, children)
        };
        let mut state = ProjectReviewState {
            request_id: id.clone(),
            folder: request.folder.clone(),
            context_id: request.context_id.clone(),
            status: SecurityReviewStatus::Complete,
            coverage,
            summary: Some(summary),
            limitations,
            completed_at: Some(now_ms()),
            error: None,
        };
        let path = self.directory(id).join("result.json");
        if atomic_json(&path, &state, MAX_RESULT_BYTES).is_err() {
            state = ProjectReviewState {
                coverage: Vec::new(),
                summary: None,
                limitations: Vec::new(),
                status: SecurityReviewStatus::Error,
                error: Some(
                    "The project review result exceeded its size limit and could not be saved."
                        .into(),
                ),
                ..state
            };
            atomic_json(&path, &state, MAX_RESULT_BYTES)?;
        }
        let mut index = self.index.lock();
        index.pending.remove(id);
        index.done.insert(id.clone(), Arc::new(state));
        Ok(true)
    }

    /// First aggregate completion: ask the originating live Architect session to print the saved result (display only).
    /// A stopped or replaced Architect keeps the saved result for an explicit replay; no model is ever started here.
    fn display(&self, registry: &Registry, request: &ProjectRequest) {
        let (Some(instance), Some(generation), Some(launch), Some(session)) = (
            &request.architect_instance_id,
            request.architect_generation,
            &request.architect_launch_id,
            &request.architect_session_id,
        ) else {
            return;
        };
        let Some(hub) = registry.manager_hub() else {
            return;
        };
        let producer = DependencyProducer {
            instance_id: instance.clone(),
            generation,
            launch_id: launch.clone(),
            session_id: session.clone(),
        };
        if !hub.dependency_producer_live(&producer) {
            return;
        }
        if let Err(error) =
            hub.show_dependency_result(instance, generation, &request.request_id, "project", false)
        {
            eprintln!("project review display: {error}");
        }
    }

    /// True when `request_id` names a saved project review of this service (a request file exists under its fixed root).
    pub fn owns(&self, request_id: &str) -> bool {
        validate_id(request_id).is_ok()
            && fs::symlink_metadata(self.directory(request_id).join("request.json"))
                .is_ok_and(|meta| meta.is_file())
    }

    /// Ask the configured Architect to print an already saved project result (display only, never an analysis prompt).
    pub fn show_in_architect(
        &self,
        registry: &Registry,
        security: &DependencySecurityService,
        request_id: &str,
    ) -> Result<()> {
        if self.read(registry, security, request_id)?.status == SecurityReviewStatus::Pending {
            return Err("There is no saved result to show yet".into());
        }
        let architect = Self::architect_row(registry)
            .ok_or("Create The Architect first using its toolbar button.")?;
        let hub = registry
            .manager_hub()
            .ok_or("manager bridge not initialized")?;
        hub.show_dependency_result(
            &architect.id,
            architect.generation,
            request_id,
            "project",
            true,
        )
    }

    /// One-second lifecycle hook: reconcile unfinished project reviews from saved child state. No model or metadata calls.
    pub fn reconcile(&self, registry: &Registry, security: &DependencySecurityService) {
        let pending: Vec<String> = self.index.lock().pending.iter().cloned().collect();
        for id in pending {
            if let Err(error) = self.refresh(registry, security, &id) {
                // An unreadable project is reported by its next explicit read; the tick must not log it every second.
                eprintln!("project review {id}: {error}");
                self.index.lock().pending.remove(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("omp-project-review-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn child(
        status: SecurityReviewStatus,
        findings: &[(&str, &str, &str)],
        error: Option<&str>,
    ) -> ChildSummary {
        ChildSummary {
            status,
            error: error.map(str::to_string),
            findings: findings
                .iter()
                .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
                .collect(),
        }
    }

    fn entry(
        package: &str,
        request: Option<&str>,
        state: WorkState,
        reason: Option<&str>,
    ) -> WorkEntry {
        WorkEntry {
            package_id: package.into(),
            request_id: request.map(str::to_string),
            state,
            reason: reason.map(str::to_string),
        }
    }

    struct Fixture {
        root: PathBuf,
        registry: Arc<Registry>,
        security: DependencySecurityService,
        insights: PackageInsightsService,
        inhouse: InhouseService,
        context: DependencyContextService,
    }

    fn fixture() -> Fixture {
        let root = temp();
        Fixture {
            registry: Arc::new(Registry::load(root.join("instances.json"), root.clone()).unwrap()),
            security: DependencySecurityService::new(root.clone()),
            insights: PackageInsightsService::try_new(root.clone()).unwrap(),
            inhouse: InhouseService::new(root.clone()),
            context: DependencyContextService::new(root.clone()),
            root,
        }
    }

    #[test]
    fn coverage_is_exact_and_pending_is_never_presented_as_complete() {
        let work = [
            entry("npm:a", Some("A"), WorkState::Reused, None),
            entry("npm:b", Some("B"), WorkState::New, None),
            entry("npm:c", Some("C"), WorkState::New, None),
            entry(
                "npm:in",
                None,
                WorkState::Skipped,
                Some("Marked in-house: private"),
            ),
            entry("npm:bad", None, WorkState::Failed, Some("not a dependency")),
        ];
        let summaries = HashMap::from([
            (
                "A".to_string(),
                child(
                    SecurityReviewStatus::Complete,
                    &[
                        ("Prototype pollution", "high", "relevant"),
                        ("Old bug", "low", "notRelevant"),
                    ],
                    None,
                ),
            ),
            (
                "B".to_string(),
                child(
                    SecurityReviewStatus::Pending,
                    &[],
                    Some("Prompt admission outcome unknown"),
                ),
            ),
            (
                "C".to_string(),
                child(SecurityReviewStatus::Error, &[], Some("producerEnded")),
            ),
        ]);
        let coverage: Vec<_> = work
            .iter()
            .map(|item| {
                coverage_for(
                    item,
                    item.request_id.as_ref().and_then(|id| summaries.get(id)),
                )
            })
            .collect();
        assert_eq!(
            coverage.iter().map(|item| item.status).collect::<Vec<_>>(),
            [
                CoverageStatus::Complete,
                CoverageStatus::Pending,
                CoverageStatus::Error,
                CoverageStatus::SkippedInhouse,
                CoverageStatus::Error
            ]
        );
        assert_eq!(
            coverage[1].reason.as_deref(),
            Some("Prompt admission outcome unknown")
        );
        assert_eq!(coverage[3].request_id, None);
        let (summary, limitations) = compile(&coverage, &summaries);
        assert!(summary.starts_with(COMPILED_NOTE));
        assert!(summary.contains("npm:a: 2 finding(s), highest high; 1 relevant to this project, 1 not relevant, 0 unknown"));
        assert!(limitations
            .iter()
            .any(|item| item.contains("npm:c: review failed — producerEnded")));
        assert!(limitations.iter().any(|item| item.contains("npm:in")));
        assert!(limitations
            .iter()
            .any(|item| item.contains("not a clean bill of health")));
    }

    #[test]
    fn summary_and_limitations_respect_their_bounds() {
        let many: Vec<ProjectCoverage> = (0..300)
            .map(|n| ProjectCoverage {
                package_id: format!("npm:p{n}"),
                request_id: None,
                status: CoverageStatus::Error,
                reason: Some("x".repeat(5000)),
            })
            .collect();
        let (summary, limitations) = compile(&many, &HashMap::new());
        assert!(summary.encode_utf16().count() <= SUMMARY_UNITS);
        assert!(
            limitations.len() <= MAX_LIMITATIONS
                && limitations
                    .iter()
                    .all(|item| item.encode_utf16().count() <= LIMITATION_UNITS)
        );
        assert_eq!(clip("a😀b", 3), "a😀");
    }

    #[test]
    fn an_empty_direct_set_completes_without_a_model_and_survives_a_restart() {
        let f = fixture();
        let project = f.root.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("package.json"), r#"{"dependencies":{}}"#).unwrap();
        let service = DependencyProjectReviewService::new(f.root.clone());
        let admission = service
            .start(
                &f.registry,
                &f.security,
                &f.insights,
                &f.inhouse,
                &f.context,
                &project,
                false,
            )
            .unwrap();
        assert_eq!(admission.disposition, JobDisposition::Complete);
        assert!(admission.prompt_sent.is_none());
        let state = service
            .read(&f.registry, &f.security, &admission.request_id)
            .unwrap();
        assert_eq!(state.status, SecurityReviewStatus::Complete);
        assert!(
            state.coverage.is_empty() && state.completed_at.is_some() && state.context_id.is_none()
        );
        // Reuse: same request id, then a second service on the same directory returns the same saved result.
        assert_eq!(
            service
                .start(
                    &f.registry,
                    &f.security,
                    &f.insights,
                    &f.inhouse,
                    &f.context,
                    &project,
                    false
                )
                .unwrap()
                .request_id,
            admission.request_id
        );
        let again = DependencyProjectReviewService::new(f.root.clone());
        assert_eq!(
            again
                .start(
                    &f.registry,
                    &f.security,
                    &f.insights,
                    &f.inhouse,
                    &f.context,
                    &project,
                    false
                )
                .unwrap()
                .request_id,
            admission.request_id
        );
        assert_eq!(
            again
                .read(&f.registry, &f.security, &admission.request_id)
                .unwrap(),
            state
        );
        // Only an explicit rerun replaces a completed project.
        let replaced = again
            .start(
                &f.registry,
                &f.security,
                &f.insights,
                &f.inhouse,
                &f.context,
                &project,
                true,
            )
            .unwrap();
        assert_ne!(replaced.request_id, admission.request_id);
        fs::remove_dir_all(f.root).unwrap();
    }

    #[test]
    fn an_incomplete_privacy_sweep_blocks_fresh_work_before_anything_is_saved() {
        let f = fixture();
        let project = f.root.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("package.json"),
            r#"{"dependencies":{"a":"1"}}"#,
        )
        .unwrap();
        f.inhouse.status(&f.registry, &project, true, true).unwrap();
        let service = DependencyProjectReviewService::new(f.root.clone());
        let error = service
            .start(
                &f.registry,
                &f.security,
                &f.insights,
                &f.inhouse,
                &f.context,
                &project,
                false,
            )
            .unwrap_err();
        assert!(error.contains("blocked until"), "{error}");
        assert!(
            !f.root.join("architect/project-security").exists()
                && !f.root.join("architect/dependency-context").exists()
        );
        fs::remove_dir_all(f.root).unwrap();
    }

    #[test]
    fn in_house_directs_are_skipped_without_an_architect_and_pending_projects_are_restored() {
        let f = fixture();
        let project = f.root.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("package.json"),
            r#"{"dependencies":{"secret":"1"}}"#,
        )
        .unwrap();
        let key = dependencies::folder_key(&project);
        let judgements = f.root.join("architect/inhouse").join(format!("{key}.json"));
        fs::create_dir_all(judgements.parent().unwrap()).unwrap();
        fs::write(
            &judgements,
            r#"{"items":{"npm:secret":{"inhouse":true,"reason":"company internal"}}}"#,
        )
        .unwrap();
        f.inhouse.status(&f.registry, &project, true, true).unwrap();
        let service = DependencyProjectReviewService::new(f.root.clone());
        let admission = service
            .start(
                &f.registry,
                &f.security,
                &f.insights,
                &f.inhouse,
                &f.context,
                &project,
                false,
            )
            .unwrap();
        assert_eq!(admission.disposition, JobDisposition::Complete);
        let state = service
            .read(&f.registry, &f.security, &admission.request_id)
            .unwrap();
        assert_eq!(state.coverage.len(), 1);
        assert_eq!(state.coverage[0].status, CoverageStatus::SkippedInhouse);
        assert!(state.coverage[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("company internal"));
        assert!(state.summary.unwrap().contains("skipped as in-house 1"));
        assert!(
            state.context_id.is_some(),
            "a project with directs binds the published graph context"
        );

        // An unfinished project (request + manifest, no result) is restored as pending after a restart.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let unfinished = Uuid::new_v4().to_string();
        let request = service.request(
            &unfinished,
            &dependencies::scan(&project, &f.registry.home, true).unwrap(),
            None,
            false,
            None,
        );
        service
            .persist(
                &request,
                &[entry(
                    "npm:secret",
                    Some(&Uuid::new_v4().to_string()),
                    WorkState::Reused,
                    None,
                )],
            )
            .unwrap();
        let restored = DependencyProjectReviewService::new(f.root.clone());
        assert!(restored.index.lock().pending.contains(&unfinished));
        assert_eq!(restored.index.lock().latest.get(&key), Some(&unfinished));
        fs::remove_dir_all(f.root).unwrap();
    }
}
