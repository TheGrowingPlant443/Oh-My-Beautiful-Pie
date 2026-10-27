//! Scan jobs: one global scan at a time, owned by the operation gate. Snapshot the folder, run the
//! selected backend, persist provenance + results. Nothing here touches the original working tree.
use super::alternate::{self, AlternateRun};
use super::lifecycle::{names, SONAR_ALIAS};
use super::runtime::{ContainerSpec, Ctl, Engine, OWNER_LABEL, ROLE_LABEL, ROLE_SCAN};
use super::scanners::{self, SonarRunTarget};
use super::snapshot::{self, Snapshot};
use super::sonar::{self, SonarClient, SonarKind};
use super::store::{self, JobRequest, PendingTask};
use super::types::*;
use super::{ActiveScan, CodeQualityService, Gate};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SCAN_WALL_CLOCK: Duration = Duration::from_secs(60 * 60);
const MAX_LOG_LINES: usize = 300;
const MAX_FINDINGS: usize = 50_000;
const KEEP_RECENT: usize = 20;
const REPORT_PREVIEW_BYTES: usize = 2 * 1024 * 1024;
const SONAR_ISSUE_CAP: usize = 10_000;
const REPORT_SLICE_BYTES: usize = 64 * 1024;
/// Same ceiling as the Architect's `inspect_dependency_context` tool.
const AGENT_OUTPUT_CAP: usize = 256 * 1024;
const NOT_THIS_REPOSITORY: &str = "That scan does not belong to this repository";

/// Canonical existing directory; Windows verbatim prefixes removed so paths compare and hash stably.
pub(crate) fn canonical_folder(folder: &str) -> Result<PathBuf> {
    let path = std::fs::canonicalize(folder.trim()).map_err(|e| format!("The folder cannot be used: {e}"))?;
    if !path.is_dir() {
        return Err("Choose a folder.".into());
    }
    let text = path.to_string_lossy().to_string();
    let stripped = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text
    };
    Ok(PathBuf::from(stripped))
}

fn folder_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 400
        && key.chars().all(|c| c.is_ascii_alphanumeric() || "_.:-".contains(c))
        && key.chars().any(|c| !c.is_ascii_digit())
}

fn clean(text: &str) -> bool {
    !text.chars().any(|c| c.is_control())
}

fn valid_entry_file(entry: &str) -> bool {
    !entry.is_empty() && !Path::new(entry).is_absolute() && !entry.split(['/', '\\']).any(|part| part == "..") && clean(entry)
}
fn agent_report_section(req: &AgentReportRequest) -> Result<&'static str> {
    let (section, allowed): (&str, &[&str]) = match req.section {
        AgentReportSection::Scans => ("scans", &[]),
        AgentReportSection::Summary => ("summary", &["scanId"]),
        AgentReportSection::Findings => ("findings", &["scanId", "offset", "limit", "tool", "severity"]),
        AgentReportSection::Artifact => ("artifact", &["scanId", "reportName", "offset"]),
    };
    for (name, present) in [
        ("scanId", req.scan_id.is_some()), ("offset", req.offset.is_some()), ("limit", req.limit.is_some()),
        ("tool", req.tool.is_some()), ("severity", req.severity.is_some()), ("reportName", req.report_name.is_some()),
    ] {
        if present && !allowed.contains(&name) { return Err(format!("section {section} does not accept {name}")); }
    }
    Ok(section)
}
fn agent_summary(scan: &QualityScan) -> Result<serde_json::Value> {
    let mut by_severity = serde_json::Map::new();
    let mut by_tool = std::collections::BTreeMap::<&str, u64>::new();
    for severity in [Severity::Critical, Severity::High, Severity::Medium, Severity::Low, Severity::Info] {
        let key = serde_json::to_value(severity).map_err(|error| error.to_string())?;
        by_severity.insert(key.as_str().unwrap_or_default().to_string(), scan.findings.iter().filter(|finding| finding.severity == severity).count().into());
    }
    for finding in &scan.findings {
        for tool in &finding.tools { *by_tool.entry(tool.as_str()).or_default() += 1; }
    }
    Ok(serde_json::json!({
        "section": "summary", "scan": scan_summary(scan), "findingsBySeverity": by_severity,
        "findingsByTool": by_tool, "hotspots": scan.findings.iter().filter(|finding| finding.hotspot).count(),
    }))
}
fn agent_findings(scan: &QualityScan, req: &AgentReportRequest) -> Result<serde_json::Value> {
    let (offset, limit) = page(req)?;
    if req.tool.as_ref().is_some_and(|tool| tool.is_empty() || tool.len() > 200 || !clean(tool)) { return Err("tool must be 1..200 characters".into()); }
    let selected: Vec<&QualityFinding> = scan.findings.iter()
        .filter(|finding| req.tool.as_ref().is_none_or(|tool| finding.tools.iter().any(|candidate| candidate == tool)))
        .filter(|finding| req.severity.is_none_or(|severity| finding.severity == severity))
        .collect();
    let mut items = Vec::new();
    let mut size = 0usize;
    for (index, finding) in selected.iter().enumerate().skip(offset) {
        if items.len() >= limit { break; }
        let item = serde_json::to_value(finding).map_err(|error| error.to_string())?;
        let bytes = item.to_string().len();
        if bytes > AGENT_OUTPUT_CAP - 4096 { return Err(format!("finding {index} is {bytes} bytes, larger than the {AGENT_OUTPUT_CAP}-byte output cap; it cannot be returned whole (continue with offset {})", index + 1)); }
        if !items.is_empty() && size + bytes > AGENT_OUTPUT_CAP - 4096 { break; }
        size += bytes;
        items.push(item);
    }
    let next = offset + items.len();
    Ok(serde_json::json!({
        "section": "findings", "scanId": scan.id, "total": selected.len(), "offset": offset, "count": items.len(),
        "nextOffset": (next < selected.len()).then_some(next), "items": items,
    }))
}
pub(crate) fn validate_project(project: &QualityProject, folder: &Path) -> Result<QualityProject> {
    let mut project = project.clone();
    project.folder = folder_string(folder);
    if !valid_key(&project.project_key) {
        return Err("The SonarQube project key may contain letters, digits and _ . : - (and at least one non-digit).".into());
    }
    if let Some(entry) = &project.entry_file {
        if !valid_entry_file(entry) {
            return Err("The entry file must be a relative path inside the project.".into());
        }
    }
    if let Some(image) = &project.image {
        if image.is_empty() || image.len() > 300 || !image.chars().all(|c| c.is_ascii_alphanumeric() || "./:_@-".contains(c)) {
            return Err("The custom image reference is not valid.".into());
        }
    }
    if project.build_args.len() > 64 || project.build_args.iter().any(|a| a.len() > 500 || !clean(a)) {
        return Err("The build arguments are too long or contain control characters.".into());
    }
    if project.exclusions.len() > 100 || project.exclusions.iter().any(|e| e.is_empty() || e.len() > 300 || !clean(e) || Path::new(e).is_absolute() || e.contains("..")) {
        return Err("Exclusions must be relative glob patterns.".into());
    }
    Ok(project)
}

fn project_name(folder: &Path) -> String {
    folder.file_name().map(|n| n.to_string_lossy().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "project".into())
}

/// A report destination must be an absolute `.html` file path in an existing directory; an existing target
/// must be a regular, non-linked file so the save can never be redirected elsewhere.
fn validate_report_path(path: &str) -> Result<PathBuf> {
    let target = PathBuf::from(path);
    if path.trim().is_empty() || !clean(path) || !target.is_absolute() {
        return Err("Choose an absolute file path for the report.".into());
    }
    if !target.extension().map(|e| e.to_string_lossy().eq_ignore_ascii_case("html")).unwrap_or(false) {
        return Err("The report file must have the .html extension.".into());
    }
    let parent = target.parent().ok_or("The report folder is missing.")?;
    if !parent.is_dir() {
        return Err("The report folder does not exist.".into());
    }
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if !meta.is_file() || snapshot::is_link(&meta) => return Err("The report target is not a regular file.".into()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("Cannot inspect the report target: {e}")),
    }
    Ok(target)
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Agent-facing summary of a scan. It names only results, never the configuration, server URL, project settings or
/// log, and never a path outside the repository.
fn scan_summary(scan: &QualityScan) -> serde_json::Value {
    serde_json::json!({
        "id": scan.id, "status": scan.status, "phase": clip(&scan.phase, 200), "startedAt": scan.started_at,
        "finishedAt": scan.finished_at, "qualityGate": scan.quality_gate, "mode": scan.config.mode,
        "snapshotHash": scan.snapshot_hash, "rulesHash": scan.rules_hash, "findingCount": scan.findings.len(),
        "tools": scan.tools, "warnings": scan.warnings.iter().take(20).map(|w| clip(w, 500)).collect::<Vec<_>>(),
        "error": scan.error.as_deref().map(|e| clip(e, 2000)),
    })
}

/// `offset` (default 0) and `limit` (1..=100, default 50) of a findings page.
fn page(req: &AgentReportRequest) -> Result<(usize, usize)> {
    let offset = usize::try_from(req.offset.unwrap_or(0)).map_err(|_| "offset is too large".to_string())?;
    let limit = req.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err("limit must be an integer from 1 to 100".into());
    }
    Ok((offset, limit as usize))
}

impl CodeQualityService {
    // ---- read-only helpers ---------------------------------------------------------------

    pub fn list_project_folders(&self) -> Result<Vec<String>> {
        store::list_project_folders(&self.paths)
    }

    pub fn detect_project(&self, folder: &str) -> Result<QualityProjectDetection> {
        let canonical = canonical_folder(folder)?;
        let saved = store::load_project(&self.paths, &folder_string(&canonical))?.project;
        scanners::detect(&canonical, saved)
    }

    fn server_origin(&self, config: &QualityConfig) -> String {
        match config.mode {
            QualityMode::SonarqubeEnterprise => config.enterprise_url.clone(),
            _ => sonar::community_url(config.community_port),
        }
    }

    pub fn prepare_scan(&self, project: QualityProject) -> Result<QualityScanPreparation> {
        let folder = canonical_folder(&project.folder)?;
        let project = validate_project(&project, &folder)?;
        let config = self.inner.lock().file.config.clone();
        let mut preparation = match config.mode {
            QualityMode::Alternate => alternate::prepare_scan(&project, &folder)?,
            _ => scanners::prepare_scan(&project, &folder, &self.server_origin(&config))?,
        };
        let consents = store::load_consents(&self.paths)?;
        if consents.approved.contains(&store::consent_key(&project.folder, &preparation.recipe_hash)) {
            preparation.requires_consent = false;
        }
        Ok(preparation)
    }

    pub fn read_scan(&self, id: &str) -> Result<QualityScan> {
        if let Some(active) = self.inner.lock().active_scan.as_ref().filter(|a| a.id == id) {
            return Ok(active.scan.clone());
        }
        store::load_scan(&self.paths, id)?.ok_or_else(|| "That scan no longer exists.".to_string())
    }

    /// Writes the self-contained HTML report of a stored (or active) scan to `path`.
    pub fn save_report(&self, id: &str, path: &str) -> Result<()> {
        let target = validate_report_path(path)?;
        let scan = self.read_scan(id)?;
        std::fs::write(&target, super::report_html::render_html(&scan)).map_err(|e| format!("Cannot write the report: {e}"))
    }

    pub fn list_scans(&self, folder: &str) -> Result<Vec<QualityScan>> {
        let canonical = folder_string(&canonical_folder(folder)?);
        let file = store::load_project(&self.paths, &canonical)?;
        let mut scans = Vec::new();
        for id in file.recent.iter().take(KEEP_RECENT) {
            if let Ok(scan) = self.read_scan(id) {
                if scan.folder.eq_ignore_ascii_case(&canonical) {
                    scans.push(scan);
                }
            }
        }
        Ok(scans)
    }

    /// The one validation of an artifact request: the name must be declared by this scan's tool outcomes, must not
    /// be a path, and must be a regular (non-linked) file inside the scan's own report folder.
    fn report_file(&self, id: &str, name: &str) -> Result<PathBuf> {
        let scan = self.read_scan(id)?;
        let allowed = scan.tools.iter().any(|t| t.report_names.iter().any(|n| n == name));
        if !allowed || name.is_empty() || name.contains(['/', '\\']) || name == "." || name == ".." || name.contains("..") {
            return Err("That report is not part of this scan.".into());
        }
        let path = self.paths.job(id).join("reports").join(name);
        let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("The report is not available: {e}"))?;
        if !meta.is_file() {
            return Err("The report is not available.".into());
        }
        Ok(path)
    }

    pub fn read_report(&self, id: &str, name: &str) -> Result<ReportPreview> {
        let path = self.report_file(id, name)?;
        let mut bytes = Vec::new();
        std::fs::File::open(&path)
            .map_err(|e| e.to_string())?
            .take(REPORT_PREVIEW_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        Ok(super::reports::preview_text(&String::from_utf8_lossy(&bytes), REPORT_PREVIEW_BYTES))
    }

    /// A continuation-friendly read of one declared report: at most `len` (<= 64 KiB) bytes from byte `offset`, ending on
    /// a UTF-8 character boundary so `next_offset` always starts a character.
    pub fn read_report_slice(&self, id: &str, name: &str, offset: u64, len: usize) -> Result<ReportSlice> {
        use std::io::{Seek, SeekFrom};
        let len = len.clamp(1, REPORT_SLICE_BYTES);
        let path = self.report_file(id, name)?;
        let mut file = std::fs::File::open(&path).map_err(|e| format!("The report is not available: {e}"))?;
        let total = file.metadata().map_err(|e| e.to_string())?.len();
        if offset > total {
            return Err("The offset is beyond the end of the report.".into());
        }
        file.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        // One extra byte shows whether the cut falls inside a multi-byte character.
        file.take(len as u64 + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        if offset > 0 && bytes.first().is_some_and(|b| b & 0xC0 == 0x80) {
            return Err("The offset is not on a UTF-8 character boundary; continue from nextOffset.".into());
        }
        let mut end = bytes.len().min(len);
        if bytes.len() > len {
            while end > 0 && bytes[end] & 0xC0 == 0x80 {
                end -= 1;
            }
            if end == 0 {
                end = len;
            }
        }
        let next = offset + end as u64;
        Ok(ReportSlice { text: String::from_utf8_lossy(&bytes[..end]).into_owned(), next_offset: (next < total).then_some(next), total })
    }

    /// Passive, read-only view of the saved results for `folder` (the caller's own repository, chosen by the hub from the
    /// authenticated agent, never by the agent). It never admits an operation, starts, probes or contacts an engine.
    pub fn agent_report(&self, folder: &str, req: &AgentReportRequest) -> Result<serde_json::Value> {
        let canonical = folder_string(&canonical_folder(folder)?);
        let scans: Vec<QualityScan> = self.list_scans(&canonical)?.into_iter()
            .filter(|scan| crate::instance::same_folder(&scan.folder, &canonical)).collect();
        let section = agent_report_section(req)?;
        let result = if req.section == AgentReportSection::Scans {
            serde_json::json!({ "section": section, "total": scans.len(), "scans": scans.iter().map(scan_summary).collect::<Vec<_>>() })
        } else {
            let id = req.scan_id.as_deref().ok_or_else(|| format!("section {section} requires scanId"))?;
            let id = uuid::Uuid::parse_str(id).map_err(|_| "scanId must be a scan id returned by section scans".to_string())?.to_string();
            let scan = scans.iter().find(|scan| scan.id == id).ok_or(NOT_THIS_REPOSITORY)?;
            match req.section {
                AgentReportSection::Summary => agent_summary(scan)?,
                AgentReportSection::Findings => agent_findings(scan, req)?,
                _ => self.agent_artifact(scan, req)?,
            }
        };
        if result.to_string().len() > AGENT_OUTPUT_CAP { return Err(format!("The result is larger than the {AGENT_OUTPUT_CAP}-byte output cap; request a smaller limit.")); }
        Ok(result)
    }
    fn agent_artifact(&self, scan: &QualityScan, req: &AgentReportRequest) -> Result<serde_json::Value> {
        let name = req.report_name.as_deref().ok_or("section artifact requires reportName")?;
        let offset = req.offset.unwrap_or(0);
        let mut len = REPORT_SLICE_BYTES;
        loop {
            let slice = self.read_report_slice(&scan.id, name, offset, len)?;
            let value = serde_json::json!({
                "section": "artifact", "scanId": scan.id, "reportName": name, "offset": offset,
                "text": slice.text, "nextOffset": slice.next_offset, "totalBytes": slice.total,
            });
            if value.to_string().len() <= AGENT_OUTPUT_CAP - 4096 || len <= 1024 { return Ok(value); }
            len /= 2;
        }
    }

    pub fn open_dashboard(&self, scan_id: Option<String>) -> Result<()> {
        let config = self.inner.lock().file.config.clone();
        let (url, loopback) = match &scan_id {
            Some(id) => {
                let scan = self.read_scan(id)?;
                let community = scan.config.mode == QualityMode::SonarqubeCommunity;
                match scan.dashboard_url {
                    Some(url) => (url, community),
                    None => (self.server_origin(&scan.config), community),
                }
            }
            None => match config.mode {
                QualityMode::Alternate => return Err("The selected analyzer has no dashboard.".into()),
                QualityMode::SonarqubeCommunity => (sonar::community_url(config.community_port), true),
                QualityMode::SonarqubeEnterprise => {
                    if config.enterprise_url.is_empty() {
                        return Err("Save the Enterprise server URL first.".into());
                    }
                    (config.enterprise_url.clone(), false)
                }
            },
        };
        crate::code_diff::open_url(&url, loopback)
    }

    pub fn open_sso_login(&self, url: &str) -> Result<()> {
        let normalized = sonar::normalize_enterprise_url(url.trim())?;
        crate::code_diff::open_url(&normalized, false)
    }

    // ---- start / cancel -------------------------------------------------------------------

    pub fn start_scan(self: &Arc<Self>, project: QualityProject, recipe_hash: String, consent: bool) -> Result<ScanAdmission> {
        let folder = canonical_folder(&project.folder)?;
        let project = validate_project(&project, &folder)?;
        let gate = self.admit(QualityPhase::Scanning, None)?;
        if self.has_unresolved() {
            return Err("A managed SonarQube server could not be confirmed stopped; use Start / retry first.".into());
        }
        let (config, effective) = {
            let inner = self.inner.lock();
            (inner.file.config.clone(), inner.effective)
        };
        if effective != Some(config.mode) {
            return Err("The selected analyzer is not ready. Apply the settings or use Start / retry first.".into());
        }
        let preparation = match config.mode {
            QualityMode::Alternate => {
                alternate::require_acknowledged(&self.config_root)?;
                alternate::prepare_scan(&project, &folder)?
            }
            _ => scanners::prepare_scan(&project, &folder, &self.server_origin(&config))?,
        };
        if preparation.recipe_hash != recipe_hash {
            return Err("The project or its build files changed since the recipe was shown; review it again.".into());
        }
        let consent_key = store::consent_key(&project.folder, &preparation.recipe_hash);
        let mut consents = store::load_consents(&self.paths)?;
        if preparation.requires_consent && !consent && !consents.approved.contains(&consent_key) {
            return Err("Approve the displayed recipe before this project can be scanned.".into());
        }
        let token = match config.mode {
            QualityMode::SonarqubeCommunity => {
                let target = self.community_token_target(&config.runtime).ok_or("Start the local SonarQube server first.")?;
                Some(self.secrets.read(&target)?.ok_or("Create a SonarQube user token (Browse + Execute Analysis + project creation) and save it in the settings.")?)
            }
            QualityMode::SonarqubeEnterprise => {
                Some(self.secrets.read(&self.enterprise_token_target(&config.enterprise_url))?.ok_or("Save an Enterprise user token in the settings first.")?)
            }
            QualityMode::Alternate => None,
        };
        self.reconcile_pending_tasks(&config, token.as_deref())?;

        let id = uuid::Uuid::new_v4().to_string();
        let started_at = now_millis();
        let job_dir = self.paths.job(&id);
        std::fs::create_dir_all(job_dir.join("reports")).map_err(|e| e.to_string())?;
        store::save_request(&self.paths, &JobRequest { version: 1, id: id.clone(), project: project.clone(), config: config.clone(), recipe_hash: recipe_hash.clone(), started_at })?;
        if preparation.requires_consent {
            consents.version = 1;
            consents.approved.push(consent_key);
            store::save_consents(&self.paths, &consents)?;
        }
        let mut project_file = store::load_project(&self.paths, &project.folder)?;
        project_file.version = 1;
        project_file.project = Some(project.clone());
        project_file.recent.insert(0, id.clone());
        let dropped: Vec<String> = project_file.recent.split_off(KEEP_RECENT.min(project_file.recent.len()));
        store::save_project(&self.paths, &project.folder, &project_file)?;
        for old in dropped {
            if store::valid_job_id(&old) {
                let _ = std::fs::remove_dir_all(self.paths.job(&old));
            }
        }
        let scan = QualityScan {
            id: id.clone(),
            folder: project.folder.clone(),
            config: config.clone(),
            project: project.clone(),
            status: QualityScanStatus::Running,
            phase: "Preparing".into(),
            started_at,
            finished_at: None,
            snapshot_hash: String::new(),
            rules_hash: String::new(),
            tools: Vec::new(),
            findings: Vec::new(),
            warnings: preparation.warnings.clone(),
            error: None,
            analysis_id: None,
            dashboard_url: None,
            quality_gate: None,
            log: Vec::new(),
        };
        store::save_scan(&self.paths, &scan)?;
        {
            let mut inner = self.inner.lock();
            inner.active_scan = Some(ActiveScan { id: id.clone(), cancel: gate.ctl.cancel.clone(), scan });
        }
        let svc = self.clone();
        let job = JobContext { id: id.clone(), folder, project, config, token, job_dir };
        let spawned = self.spawn_worker("code-quality-scan", move || svc.run_scan(gate, job));
        if let Err(error) = spawned {
            self.inner.lock().active_scan = None;
            return Err(error);
        }
        Ok(ScanAdmission { scan_id: id })
    }

    pub fn cancel_scan(&self, id: &str) -> Result<QualityScan> {
        let active = {
            let mut inner = self.inner.lock();
            match inner.active_scan.as_mut().filter(|a| a.id == id) {
                Some(active) => {
                    active.cancel.store(true, Ordering::Relaxed);
                    if active.scan.status == QualityScanStatus::Running {
                        active.scan.status = QualityScanStatus::Cancelling;
                        active.scan.phase = "Cancelling".into();
                    }
                    inner.phase = QualityPhase::Cancelling;
                    true
                }
                None => false,
            }
        };
        if active {
            // Cancellation is complete only when the scanner container and any server task have stopped.
            loop {
                let done = self.inner.lock().active_scan.as_ref().map(|a| a.id != id).unwrap_or(true);
                if done {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        self.read_scan(id)
    }

    // ---- job execution --------------------------------------------------------------------

    fn scan_log(&self, id: &str, line: &str) {
        let mut inner = self.inner.lock();
        if let Some(active) = inner.active_scan.as_mut().filter(|a| a.id == id) {
            if active.scan.log.len() >= MAX_LOG_LINES {
                active.scan.log.remove(0);
            }
            active.scan.log.push(super::redact_line(line));
        }
    }

    fn scan_phase(&self, id: &str, phase: &str) {
        self.scan_log(id, phase);
        let mut inner = self.inner.lock();
        if let Some(active) = inner.active_scan.as_mut().filter(|a| a.id == id) {
            if active.scan.status == QualityScanStatus::Running {
                active.scan.phase = phase.to_string();
            }
        }
    }

    fn mutate_scan(&self, id: &str, f: impl FnOnce(&mut QualityScan)) {
        let mut inner = self.inner.lock();
        if let Some(active) = inner.active_scan.as_mut().filter(|a| a.id == id) {
            f(&mut active.scan);
        }
    }

    fn run_scan(self: Arc<Self>, gate: Gate, job: JobContext) {
        let done = Arc::new(AtomicBool::new(false));
        let timed_out = Arc::new(AtomicBool::new(false));
        {
            let (done, timed_out, cancel) = (done.clone(), timed_out.clone(), gate.ctl.cancel.clone());
            let _ = std::thread::Builder::new().name("code-quality-scan-watchdog".into()).spawn(move || {
                let started = Instant::now();
                while !done.load(Ordering::Relaxed) {
                    if started.elapsed() >= SCAN_WALL_CLOCK {
                        timed_out.store(true, Ordering::Relaxed);
                        cancel.store(true, Ordering::Relaxed);
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            });
        }
        let outcome = self.execute_scan(&gate, &job);
        done.store(true, Ordering::Relaxed);
        let cancelled = gate.cancelled() && !timed_out.load(Ordering::Relaxed);
        let limit_hit = timed_out.load(Ordering::Relaxed);
        let (status, error) = match outcome {
            Ok(status) if cancelled => (if status == QualityScanStatus::Complete { QualityScanStatus::Cancelled } else { status.min_cancel() }, None),
            Ok(status) => (status, None),
            Err(message) if cancelled => (QualityScanStatus::Cancelled, Some(message).filter(|m| m != "Cancelled.")),
            Err(message) if limit_hit => (QualityScanStatus::Error, Some(format!("The scan exceeded the {} minute limit and was stopped. {message}", SCAN_WALL_CLOCK.as_secs() / 60))),
            Err(message) => (QualityScanStatus::Error, Some(message)),
        };
        let _ = std::fs::remove_file(job.job_dir.join("source.tar"));
        let _ = std::fs::remove_dir_all(job.job_dir.join("deps"));
        let final_scan = {
            let mut inner = self.inner.lock();
            let mut scan = match inner.active_scan.take() {
                Some(active) if active.id == job.id => active.scan,
                other => {
                    inner.active_scan = other;
                    return;
                }
            };
            scan.status = status;
            scan.error = error.clone().or(scan.error);
            scan.finished_at = Some(now_millis());
            scan.phase = match status {
                QualityScanStatus::Complete => "Complete",
                QualityScanStatus::Partial => "Finished with incomplete coverage",
                QualityScanStatus::Cancelled => "Cancelled",
                QualityScanStatus::Error => "Failed",
                _ => "Stopped",
            }
            .to_string();
            scan
        };
        if let Err(save_error) = store::save_scan(&self.paths, &final_scan) {
            self.push_diagnostic(&format!("The scan result could not be saved: {save_error}"));
        }
        gate.finish(QualityPhase::Idle, None);
    }

    fn execute_scan(self: &Arc<Self>, gate: &Gate, job: &JobContext) -> Result<QualityScanStatus> {
        let id = job.id.as_str();
        let engine = self.factory.engine(&job.config.runtime)?;
        let ctl = Ctl { cancel: gate.ctl.cancel.clone(), log: {
            let svc = Arc::downgrade(self);
            let id = id.to_string();
            Arc::new(move |line: &str| {
                if let Some(svc) = svc.upgrade() {
                    svc.scan_log(&id, line);
                }
            })
        } };
        self.scan_phase(id, "Checking the container engine");
        engine.ensure_started(&ctl)?;
        let probe = engine.probe(&ctl)?;
        // Alternate and Enterprise also run owned scanner containers. Persist the runtime before
        // creating one so shutdown/handover can verify cleanup even after a failed scan or crash.
        self.set_possibly_running(&job.config.runtime, Some(&probe.fingerprint), job.config.community_port, true)?;
        self.scan_phase(id, "Snapshotting the project");
        let snapshot = snapshot::create_snapshot(&job.folder, &job.project.exclusions, &job.job_dir, &ctl)?;
        self.scan_log(
            id,
            &format!(
                "Snapshot: {} files, {:.1} MiB; excluded: {}",
                snapshot.file_count,
                snapshot.total_bytes as f64 / (1024.0 * 1024.0),
                if snapshot.applied_exclusions.is_empty() { "defaults only".to_string() } else { snapshot.applied_exclusions.join(", ") }
            ),
        );
        for warning in &snapshot.warnings {
            self.mutate_scan(id, |s| s.warnings.push(warning.clone()));
        }
        let hash = snapshot.hash.clone();
        self.mutate_scan(id, |s| s.snapshot_hash = hash);
        if gate.cancelled() {
            return Ok(QualityScanStatus::Cancelled);
        }
        let owner = self.installation_id();
        let on_container = |container: &str| self.scan_log(id, &format!("Started container {}", &container[..container.len().min(12)]));
        match job.config.mode {
            QualityMode::Alternate => self.scan_alternate(id, engine.as_ref(), job, &snapshot, &owner, &on_container, &ctl),
            _ => self.scan_sonar(gate, id, engine.as_ref(), job, &snapshot, &owner, &on_container, &ctl),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_alternate(&self, id: &str, engine: &dyn Engine, job: &JobContext, snapshot: &Snapshot, owner: &str, on_container: &dyn Fn(&str), ctl: &Ctl) -> Result<QualityScanStatus> {
        self.scan_phase(id, "Preparing the analysis image");
        alternate::prepare_alternate(engine, &self.config_root, ctl)?;
        self.scan_phase(id, "Running MegaLinter, Opengrep and Semgrep CE");
        let run = AlternateRun { owner, job_id: id, snapshot_tar: &snapshot.tar_path, project: &job.project, job_dir: &job.job_dir, config_root: &self.config_root };
        let result = alternate::run_alternate(engine, &run, on_container, ctl)?;
        let any_ok = result.tools.iter().any(|t| matches!(t.status, ToolStatus::Complete | ToolStatus::Partial));
        let all_complete = result.tools.iter().all(|t| matches!(t.status, ToolStatus::Complete | ToolStatus::Skipped));
        let all_error = result.tools.iter().all(|t| t.status == ToolStatus::Error);
        let mut findings = result.findings;
        let mut warnings = result.warnings;
        let truncated = findings.len() > MAX_FINDINGS;
        if truncated {
            findings.truncate(MAX_FINDINGS);
            warnings.push(format!("Only the first {MAX_FINDINGS} findings are kept."));
        }
        self.mutate_scan(id, |s| {
            s.tools = result.tools;
            s.findings = findings;
            s.warnings.extend(warnings);
            s.rules_hash = result.rules_hash;
        });
        Ok(if all_error && !any_ok {
            QualityScanStatus::Error
        } else if all_complete && !truncated {
            QualityScanStatus::Complete
        } else {
            QualityScanStatus::Partial
        })
    }

    fn wait_scan_analysis(&self, gate: &Gate, id: &str, client: &SonarClient, task_id: &str, ctl: &Ctl) -> Result<Option<String>> {
        let finished = match client.submit_wait_task(task_id, ctl, Duration::from_secs(30 * 60)) {
            Ok(task) => task,
            Err(_) if gate.cancelled() => {
                self.cancel_remote_task(id, client, task_id);
                return Ok(None);
            }
            Err(error) => return Err(format!("{error} The SonarQube task {task_id} stays recorded and is checked before the next scan.")),
        };
        if gate.cancelled() {
            self.cancel_remote_task(id, client, task_id);
            return Ok(None);
        }
        self.remove_pending_task(task_id)?;
        if finished.status != "SUCCESS" {
            return Err(format!("SonarQube's analysis task {}: {}", finished.status.to_lowercase(), finished.error_message.unwrap_or_else(|| "no further detail".into())));
        }
        finished.analysis_id.map(Some).ok_or_else(|| "SonarQube finished the task without an analysis id.".into())
    }
    #[allow(clippy::too_many_arguments)]
    fn scan_sonar(&self, gate: &Gate, id: &str, engine: &dyn Engine, job: &JobContext, snapshot: &Snapshot, owner: &str, on_container: &dyn Fn(&str), ctl: &Ctl) -> Result<QualityScanStatus> {
        let config = &job.config;
        let token = job.token.clone().ok_or("The Sonar token is missing.")?;
        let community = config.mode == QualityMode::SonarqubeCommunity;
        let base = self.server_origin(config);
        let n = names(owner);
        let (scanner_url, network) = if community { (format!("http://{SONAR_ALIAS}:9000"), Some(n.network.clone())) } else { (base.clone(), None) };
        let kind = if community { SonarKind::Community } else { SonarKind::Enterprise };
        let client = SonarClient::new(kind, &base, Some(&token))?;
        let key = job.project.project_key.clone();
        self.scan_phase(id, "Checking the SonarQube connection");
        client.validate_token().map_err(String::from)?;
        client.ensure_project(&key, &project_name(&job.folder), community).map_err(String::from)?;
        let images = scanners::images()?;
        self.scan_phase(id, &format!("Source analysis is sent to {}", sonar::identity_of(&base)));
        self.check_scanner_reach(engine, owner, id, &images.scanner_cli, &scanner_url, network.as_deref(), on_container, ctl)?;
        let before = client.latest_analysis_id(&key).map_err(String::from)?;
        self.scan_phase(id, "Running the scanner");
        let target = SonarRunTarget { scanner_url: scanner_url.clone(), network, project_key: key.clone(), project_name: project_name(&job.folder) };
        let run = scanners::run_scanner(engine, owner, id, snapshot, &job.project, &target, &token, &job.job_dir, on_container, ctl)?;
        if run.cancelled {
            return Ok(QualityScanStatus::Cancelled);
        }
        if run.timed_out {
            return Err(format!("The scanner timed out. {}", run.log_tail));
        }
        if run.exit_code != 0 {
            return Err(format!("The scanner failed (exit {}). {}", run.exit_code, run.log_tail));
        }
        let report = run.report_task.ok_or("The scanner finished without a report-task.txt; nothing was submitted to SonarQube.")?;
        let task = sonar::parse_report_task(&report, &scanner_url, &key)?;
        self.record_pending_task(PendingTask { scan_id: id.to_string(), task_id: task.task_id.clone(), config: config.clone(), project_key: key.clone() })?;
        self.scan_phase(id, "Waiting for the SonarQube analysis task");
        let Some(analysis) = self.wait_scan_analysis(gate, id, &client, &task.task_id, ctl)? else { return Ok(QualityScanStatus::Cancelled); };
        self.scan_phase(id, "Reading the quality gate and findings");
        let gate_result = client.quality_gate(&analysis).map_err(String::from)?;
        let issues = client.issues(&key, SONAR_ISSUE_CAP).map_err(String::from)?;
        let hotspots = client.hotspots(&key, SONAR_ISSUE_CAP).map_err(String::from)?;
        let latest = client.latest_analysis_id(&key).map_err(String::from)?;
        let superseded = latest.as_deref() != Some(analysis.as_str());
        let mut warnings = Vec::new();
        let mut partial = false;
        if issues.truncated || hotspots.truncated {
            partial = true;
            warnings.push(format!("SonarQube reported {} issues and {} hotspots; only the first {SONAR_ISSUE_CAP} of each are shown.", issues.total, hotspots.total));
        }
        if superseded {
            partial = true;
            warnings.push("A newer analysis of this project superseded this one while results were collected; the findings may not match this source snapshot.".into());
        }
        if before.as_deref() == Some(analysis.as_str()) {
            warnings.push("SonarQube reused its previous analysis for this project.".into());
        }
        let total = issues.findings.len() + hotspots.findings.len();
        let mut findings = issues.findings;
        findings.extend(hotspots.findings);
        let version = if community { images.sonarqube_community.clone() } else { sonar::identity_of(&base) };
        let tool = QualityToolOutcome {
            tool: "SonarQube".into(),
            version,
            status: if partial { ToolStatus::Partial } else { ToolStatus::Complete },
            exit_code: Some(run.exit_code),
            analyzed_files: None,
            finding_count: total as u64,
            error: None,
            report_names: Vec::new(),
        };
        let dashboard = client.dashboard_url(&key);
        self.mutate_scan(id, |s| {
            s.tools = vec![tool];
            s.findings = findings;
            s.warnings.extend(warnings);
            s.analysis_id = Some(analysis);
            s.quality_gate = Some(gate_result);
            s.dashboard_url = Some(dashboard);
        });
        Ok(if partial { QualityScanStatus::Partial } else { QualityScanStatus::Complete })
    }

    /// After a cancel: ask the server to cancel this job's exact CE task and wait for a terminal state.
    fn cancel_remote_task(&self, id: &str, client: &SonarClient, task_id: &str) {
        self.scan_phase(id, "Waiting for the SonarQube task to finish");
        let _ = client.cancel_task(task_id);
        let deadline = Instant::now() + Duration::from_secs(600);
        while Instant::now() < deadline {
            match client.task_status(task_id) {
                Ok(task) if matches!(task.status.as_str(), "SUCCESS" | "FAILED" | "CANCELED") => {
                    let _ = self.remove_pending_task(task_id);
                    return;
                }
                _ => std::thread::sleep(Duration::from_secs(2)),
            }
        }
        self.mutate_scan(id, |s| s.warnings.push(format!("SonarQube task {task_id} did not reach a final state; it is rechecked before the next scan.")));
    }

    #[allow(clippy::too_many_arguments)]
    fn check_scanner_reach(&self, engine: &dyn Engine, owner: &str, id: &str, image: &str, url: &str, network: Option<&str>, on_container: &dyn Fn(&str), ctl: &Ctl) -> Result<()> {
        let spec = ContainerSpec {
            name: format!("omp-q-{}-scan-{}-reach", &super::secrets::sha256_hex(owner)[..8], &id[..8]),
            image: scanners::images()?.scanner_cli.clone(),
            labels: vec![(OWNER_LABEL.into(), owner.to_string()), (ROLE_LABEL.into(), ROLE_SCAN.into())],
            network: network.map(str::to_string),
            entrypoint: Some(vec!["sh".into(), "-c".into()]),
            command: vec![format!("curl -sS -f --max-time 15 -o /dev/null '{}/api/system/status'", url.trim_end_matches('/'))],
            cpus: Some(1.0),
            memory: Some("256m".into()),
            ..Default::default()
        };
        let _ = image;
        let step = super::job::JobStep { spec, copy_in: Vec::new(), copy_out: Vec::new(), timeout_secs: 60 };
        let result = super::job::run_job(engine, &step, on_container, ctl)?;
        if result.cancelled {
            return Err("Cancelled.".into());
        }
        if result.exit_code != 0 {
            return Err(format!("The scanner container cannot reach the SonarQube server at {url}. {}", result.log_tail));
        }
        Ok(())
    }

    // ---- pending compute-engine tasks ------------------------------------------------------

    fn record_pending_task(&self, task: PendingTask) -> Result<()> {
        let mut resources = store::load_resources(&self.paths)?;
        resources.tasks.retain(|t| t.task_id != task.task_id);
        resources.tasks.push(task);
        store::save_resources(&self.paths, &resources)
    }

    fn remove_pending_task(&self, task_id: &str) -> Result<()> {
        let mut resources = store::load_resources(&self.paths)?;
        resources.tasks.retain(|t| t.task_id != task_id);
        store::save_resources(&self.paths, &resources)
    }

    /// Unresolved tasks of the active connection block further scans until they reach a final state.
    pub(crate) fn reconcile_pending_tasks(&self, config: &QualityConfig, token: Option<&str>) -> Result<()> {
        let resources = store::load_resources(&self.paths)?;
        let identity = |c: &QualityConfig| match c.mode {
            QualityMode::SonarqubeEnterprise => sonar::identity_of(&c.enterprise_url),
            _ => format!("community:{}", c.runtime.key()),
        };
        for task in resources.tasks.iter().filter(|t| identity(&t.config) == identity(config)) {
            let Some(token) = token else { continue };
            let community = config.mode == QualityMode::SonarqubeCommunity;
            let client = SonarClient::new(if community { SonarKind::Community } else { SonarKind::Enterprise }, &self.server_origin(config), Some(token))?;
            match client.task_status(&task.task_id) {
                Ok(status) if matches!(status.status.as_str(), "SUCCESS" | "FAILED" | "CANCELED") => self.remove_pending_task(&task.task_id)?,
                Ok(_) => return Err(format!("A previous SonarQube analysis task ({}) is still running; wait for it to finish.", task.task_id)),
                Err(error) => return Err(format!("A previous SonarQube analysis task ({}) could not be checked: {error}", task.task_id)),
            }
        }
        Ok(())
    }

    // ---- restart recovery -------------------------------------------------------------------

    pub(crate) fn recover_interrupted_jobs(&self) {
        let Ok(entries) = std::fs::read_dir(self.paths.jobs()) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !store::valid_job_id(&name) {
                continue;
            }
            let Ok(Some(mut scan)) = store::load_scan(&self.paths, &name) else { continue };
            if scan.status.is_terminal() {
                continue;
            }
            scan.status = QualityScanStatus::Interrupted;
            scan.phase = "Interrupted".into();
            scan.finished_at = Some(now_millis());
            scan.error = Some("OMP Manager closed while this scan was running; results are incomplete.".into());
            let _ = store::save_scan(&self.paths, &scan);
            let _ = std::fs::remove_file(self.paths.job(&name).join("source.tar"));
        }
    }

    /// Stops scanner containers left behind by a crash (exact owner + role labels only).
    pub(crate) fn reconcile_orphan_scans(&self, gate: &Gate) {
        let config = self.inner.lock().file.config.clone();
        let Ok(engine) = self.factory.engine(&config.runtime) else { return };
        if engine.probe(&gate.ctl).is_err() {
            return;
        }
        let owner = self.installation_id();
        if let Ok(owned) = engine.list_owned(&owner, &gate.ctl) {
            for container in owned.iter().filter(|c| c.labels.get(ROLE_LABEL).map(String::as_str) == Some(ROLE_SCAN)) {
                gate.say(&format!("Removing leftover scanner container {}", container.name));
                if container.running {
                    let _ = engine.stop(&container.id, 10, &gate.ctl);
                }
                let _ = engine.remove(&container.id, true, &gate.ctl);
            }
        }
    }
}

pub(crate) struct JobContext {
    pub id: String,
    pub folder: PathBuf,
    pub project: QualityProject,
    pub config: QualityConfig,
    pub token: Option<String>,
    pub job_dir: PathBuf,
}

trait CancelStatus {
    fn min_cancel(self) -> Self;
}

impl CancelStatus for QualityScanStatus {
    /// A scan cancelled after partial results keeps `Cancelled`; failures stay failures.
    fn min_cancel(self) -> Self {
        match self {
            QualityScanStatus::Error => QualityScanStatus::Error,
            _ => QualityScanStatus::Cancelled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_keys_follow_sonar_rules() {
        assert!(valid_key("omp-0123abc"));
        assert!(valid_key("my.project:sub_1"));
        assert!(!valid_key(""));
        assert!(!valid_key("12345"));
        assert!(!valid_key("has space"));
        assert!(!valid_key("semi;colon"));
    }

    #[test]
    fn project_validation_rejects_escapes_and_control_characters() {
        let dir = std::env::temp_dir();
        let base = QualityProject {
            folder: dir.to_string_lossy().to_string(),
            project_key: "omp-abc".into(),
            scanner: Scanner::Cli,
            entry_file: None,
            image: None,
            build_args: vec![],
            prepare_npm_dependencies: false,
            exclusions: vec![],
        };
        assert!(validate_project(&base, &dir).is_ok());
        let mut bad = base.clone();
        bad.entry_file = Some("..\\x.sln".into());
        assert!(validate_project(&bad, &dir).is_err());
        let mut bad = base.clone();
        bad.build_args = vec!["-Dx\n=1".into()];
        assert!(validate_project(&bad, &dir).is_err());
        let mut bad = base.clone();
        bad.exclusions = vec!["../outside".into()];
        assert!(validate_project(&bad, &dir).is_err());
        let mut bad = base;
        bad.image = Some("repo/image; rm -rf".into());
        assert!(validate_project(&bad, &dir).is_err());
    }

    #[test]
    fn canonical_folder_strips_verbatim_prefix() {
        let dir = std::env::temp_dir();
        let canonical = canonical_folder(&dir.to_string_lossy()).unwrap();
        assert!(!canonical.to_string_lossy().starts_with(r"\\?\"));
        assert!(canonical_folder("C:/definitely/missing/folder-for-omp").is_err());
    }

    #[test]
    fn report_path_must_be_an_absolute_html_file_in_an_existing_folder() {
        let dir = std::env::temp_dir().join(format!("omp report ø {}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(validate_report_path("report.html").is_err());
        assert!(validate_report_path("").is_err());
        assert!(validate_report_path(&dir.join("report.txt").to_string_lossy()).is_err());
        assert!(validate_report_path(&dir.join("report").to_string_lossy()).is_err());
        assert!(validate_report_path(&dir.join("missing").join("report.html").to_string_lossy()).is_err());
        let as_dir = dir.join("folder.html");
        std::fs::create_dir_all(&as_dir).unwrap();
        assert!(validate_report_path(&as_dir.to_string_lossy()).is_err());
        let ok = dir.join("Report.HTML");
        assert_eq!(validate_report_path(&ok.to_string_lossy()).unwrap(), ok);
        std::fs::write(&ok, "old").unwrap();
        assert!(validate_report_path(&ok.to_string_lossy()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
