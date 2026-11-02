//! On-demand, evidence-preserving repository source index.
//!
//! A job copies the admitted files of one folder into an immutable app-owned snapshot, hashes the copied
//! bytes, runs the trusted local parser over that copy and validates the facts it returns. Snapshots live
//! under `<config>/system-overview/<repo-key>/<snapshot-uuid>/`, never inside the selected repository, and
//! nothing from the repository is executed. There is no watcher: refresh is always an explicit `start`.
//!
//! Layout of one job directory: `status.json` (final [`SourceIndexStatus`]), `inventory.json` (the immutable
//! file inventory), `index.json` (validated facts), `source/` (copied files); `work/` is scratch for the parser.

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

use super::{inventory, model, query, runner};
use super::{
    FileStatus, SourceGraphIndex, SourceIndexCapabilities, SourceIndexPage, SourceIndexQuery,
    SourceIndexSelection, SourceIndexState, SourceIndexStatus,
};
use crate::code_quality::runtime::RunControl;
use inventory::InventoryRecord;
use model::{Expected, Result, MAX_OUTPUT_BYTES, MAX_SAVED_BYTES};
use parking_lot::Mutex;
use runner::{ExtractRun, Parser, ParserOutcome};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const JOB_DEADLINE: Duration = Duration::from_secs(15 * 60);
const MAX_ACTIVE_JOBS: usize = 4;
const CACHED_INDEXES: usize = 3;
const MAX_STATUS_BYTES: u64 = 1024 * 1024;
const MAX_INVENTORY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_WARNING_LINES: usize = 50;
/// Interrupted attempts younger than this may still belong to another running app instance.
const ORPHAN_AGE: Duration = Duration::from_secs(30 * 60);

struct Job {
    id: String,
    dir: PathBuf,
    cancel: Arc<AtomicBool>,
    status: Mutex<SourceIndexStatus>,
}

impl Job {
    fn status(&self) -> SourceIndexStatus {
        self.status.lock().clone()
    }
    fn update(&self, change: impl FnOnce(&mut SourceIndexStatus)) {
        change(&mut self.status.lock());
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct State {
    /// Running jobs keyed by repository key; `start` joins the one for its folder.
    active: HashMap<String, Arc<Job>>,
    capabilities: Option<SourceIndexCapabilities>,
    /// Most recently used validated indexes (newest last).
    indexes: Vec<(String, Arc<SourceGraphIndex>)>,
    directories: HashMap<String, PathBuf>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pointer {
    snapshot_id: String,
}

/// What a finished pipeline hands to [`SourceIndexService::finalize`].
enum Finish {
    Index {
        index: Box<SourceGraphIndex>,
        error: Option<String>,
    },
    Failed(String),
    Cancelled,
}

pub struct SourceIndexService {
    root: PathBuf,
    parser: Arc<dyn Parser>,
    state: Mutex<State>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Notes about what the inventory leaves out, shown with every index built from this snapshot.
fn inventory_limitations(admitted: &inventory::Admitted) -> Vec<String> {
    let mut limitations = vec!["Entries ignored by .gitignore or inside version-control, dependency or build-output folders (for example .git, node_modules, target, dist, build) are not inventoried.".to_string()];
    if admitted.excluded_count > 0 {
        limitations.push(format!(
            "{} path(s) were excluded as secrets or build output (for example {}).",
            admitted.excluded_count,
            admitted.excluded_samples.join(", ")
        ));
    }
    limitations.extend(admitted.warnings.iter().take(MAX_WARNING_LINES).cloned());
    if admitted.warnings.len() > MAX_WARNING_LINES {
        limitations.push(format!(
            "{} further inventory warning(s) were omitted.",
            admitted.warnings.len() - MAX_WARNING_LINES
        ));
    }
    limitations
}

/// Partial result for a snapshot the parser could not (fully) process: every file says why it has no facts.
fn inventory_only_finish(
    expected: &Expected,
    record: &InventoryRecord,
    status: FileStatus,
    file_reason: &str,
    reason: String,
) -> Finish {
    match SourceGraphIndex::inventory_only(
        expected,
        status,
        file_reason,
        &reason,
        record.limitations.clone(),
    ) {
        Ok(index) => Finish::Index {
            index: Box::new(index),
            error: Some(reason),
        },
        Err(e) => Finish::Failed(e),
    }
}

/// Identity of a repository folder: exact canonical spelling (case-folded only where the filesystem is).
fn folder_key(canonical: &Path) -> String {
    let text = canonical.to_string_lossy().replace('\\', "/");
    let text = if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    };
    Sha256::digest(text.trim_end_matches('/').as_bytes())[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn valid_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|u| u.to_string() == id)
}

fn write_json(path: &Path, value: &impl Serialize, max: u64) -> Result<()> {
    crate::dependency_security::atomic_json(path, value, max)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, max: u64) -> Result<T> {
    let file = File::open(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    if file.metadata().map_err(|e| e.to_string())?.len() > max {
        return Err(format!("{} is larger than {max} bytes", path.display()));
    }
    serde_json::from_reader(BufReader::new(file))
        .map_err(|e| format!("Invalid {}: {e}", path.display()))
}

fn read_pointer(path: &Path) -> Option<String> {
    read_json::<Pointer>(path, 4096)
        .ok()
        .map(|p| p.snapshot_id)
        .filter(|id| valid_id(id))
}

impl SourceIndexService {
    #[must_use]
    pub fn new(config: PathBuf) -> Arc<Self> {
        let root = config.join("system-overview");
        let parser: Arc<dyn Parser> = match runner::PythonParser::try_new(&root) {
            Ok(parser) => Arc::new(parser),
            Err(e) => Arc::new(runner::UnavailableParser(e)),
        };
        Self::build(root, parser)
    }

    fn build(root: PathBuf, parser: Arc<dyn Parser>) -> Arc<Self> {
        Arc::new(Self {
            root,
            parser,
            state: Mutex::new(State::default()),
        })
    }

    #[cfg(test)]
    fn with_parser(config: PathBuf, parser: Arc<dyn Parser>) -> Arc<Self> {
        Self::build(config.join("system-overview"), parser)
    }

    fn resolve(&self, folder: &str) -> Result<(PathBuf, String)> {
        if folder.trim().is_empty() {
            return Err("Choose a project folder first.".into());
        }
        let canonical = crate::dependencies::canonical_folder(Path::new(folder.trim()))?;
        let key = folder_key(&canonical);
        Ok((canonical, key))
    }

    /// Saved index state for a folder; metadata-only staleness comparison, never starts a job.
    pub fn get(&self, folder: &str) -> Result<SourceIndexSelection> {
        let (canonical, key) = self.resolve(folder)?;
        let repository = self.root.join(&key);
        let (active, cached) = {
            let state = self.state.lock();
            (state.active.get(&key).cloned(), state.capabilities.clone())
        };
        let load = |id: &str| -> Option<SourceIndexStatus> {
            let mut status = self.load_status(id).ok()?;
            if let Some(capabilities) = &cached {
                status.capabilities = capabilities.clone();
            }
            Some(status)
        };
        let mut selected = read_pointer(&repository.join("selected.json")).and_then(|id| load(&id));
        if let Some(status) = &mut selected {
            status.stale = status
                .snapshot_id
                .as_deref()
                .and_then(|id| self.load_inventory(id).ok())
                .and_then(|record| {
                    inventory::metadata_changed(&canonical, &record, &RunControl::silent()).ok()
                });
        }
        let latest_attempt = match active {
            Some(job) => Some(job.status()),
            None => read_pointer(&repository.join("latest.json")).and_then(|id| load(&id)),
        };
        Ok(SourceIndexSelection {
            selected,
            latest_attempt,
        })
    }

    /// Starts an index job for the folder, or joins the one already running for it.
    pub fn start(self: &Arc<Self>, folder: &str) -> Result<SourceIndexStatus> {
        let (canonical, key) = self.resolve(folder)?;
        let job = {
            let mut state = self.state.lock();
            if let Some(job) = state.active.get(&key) {
                return Ok(job.status());
            }
            if state.active.len() >= MAX_ACTIVE_JOBS {
                return Err(format!("{MAX_ACTIVE_JOBS} source index jobs are already running; wait for one to finish."));
            }
            let id = uuid::Uuid::new_v4().to_string();
            let dir = self.root.join(&key).join(&id);
            fs::create_dir_all(&dir)
                .map_err(|e| format!("Cannot create the source index job directory: {e}"))?;
            let status = SourceIndexStatus {
                job_id: id.clone(),
                snapshot_id: None,
                folder: canonical.to_string_lossy().into_owned(),
                state: SourceIndexState::Inventory,
                indexed_at: None,
                source_hash: None,
                completed_files: 0,
                total_files: 0,
                stale: None,
                error: None,
                capabilities: state.capabilities.clone().unwrap_or_default(),
                coverage: None,
            };
            let job = Arc::new(Job {
                id: id.clone(),
                dir: dir.clone(),
                cancel: Arc::new(AtomicBool::new(false)),
                status: Mutex::new(status),
            });
            state.directories.insert(id, dir);
            state.active.insert(key.clone(), job.clone());
            job
        };
        let initial = job.status();
        let service = self.clone();
        let spawned = std::thread::Builder::new()
            .name("source-index".into())
            .spawn({
                let job = job.clone();
                move || service.run(job, canonical, key)
            });
        if let Err(e) = spawned {
            let message = format!("Cannot start the source index worker thread: {e}");
            // The thread never ran, so nothing else will finalize this job.
            self.state
                .lock()
                .active
                .retain(|_, active| active.id != job.id);
            let _ = fs::remove_dir_all(&job.dir);
            return Err(message);
        }
        Ok(initial)
    }

    /// Current status of a job, running or finished.
    pub fn read_job(&self, job_id: &str) -> Result<SourceIndexStatus> {
        if let Some(job) = self.active_job(job_id) {
            return Ok(job.status());
        }
        let mut status = self.load_status(job_id)?;
        if let Some(capabilities) = self.state.lock().capabilities.clone() {
            status.capabilities = capabilities;
        }
        Ok(status)
    }

    /// Asks a running job to stop; only this job's parser process tree is terminated. Idempotent.
    pub fn cancel(&self, job_id: &str) -> Result<SourceIndexStatus> {
        if let Some(job) = self.active_job(job_id) {
            job.cancel.store(true, Ordering::Relaxed);
            return Ok(job.status());
        }
        self.read_job(job_id)
    }

    pub fn query(&self, snapshot_id: &str, query: SourceIndexQuery) -> Result<SourceIndexPage> {
        let index = self.index(snapshot_id)?;
        query::run(&index, &query)
    }

    /// Validated, immutable facts of a complete or partial snapshot. Cheap to clone and share.
    pub fn index(&self, snapshot_id: &str) -> Result<Arc<SourceGraphIndex>> {
        {
            let mut state = self.state.lock();
            if let Some(position) = state.indexes.iter().position(|(id, _)| id == snapshot_id) {
                let entry = state.indexes.remove(position);
                let index = entry.1.clone();
                state.indexes.push(entry);
                return Ok(index);
            }
        }
        let status = self.load_status(snapshot_id)?;
        if !status.state.has_index() {
            return Err("That source index attempt did not produce an index".into());
        }
        let dir = self.find_dir(snapshot_id)?;
        let record: InventoryRecord = read_json(&dir.join("inventory.json"), MAX_INVENTORY_BYTES)?;
        let manifest = record.manifest_files();
        let expected = Expected {
            snapshot_id: &record.snapshot_id,
            folder: &record.folder,
            source_hash: &record.source_hash,
            files: &manifest,
            directories: &record.directories,
        };
        let file = File::open(dir.join("index.json"))
            .map_err(|e| format!("Cannot read the saved source index: {e}"))?;
        if file.metadata().map_err(|e| e.to_string())?.len() > MAX_SAVED_BYTES {
            return Err("The saved source index exceeds its size limit".into());
        }
        let mut index: SourceGraphIndex = serde_json::from_reader(BufReader::new(file))
            .map_err(|e| format!("The saved source index is invalid: {e}"))?;
        index.finish(&expected)?;
        let index = Arc::new(index);
        let mut state = self.state.lock();
        state.indexes.retain(|(id, _)| id != snapshot_id);
        state.indexes.push((snapshot_id.to_string(), index.clone()));
        if state.indexes.len() > CACHED_INDEXES {
            state.indexes.remove(0);
        }
        Ok(index)
    }

    /// The immutable copied source tree of a snapshot, for trusted in-process readers only (never a UI path).
    pub fn snapshot_root(&self, snapshot_id: &str) -> Result<PathBuf> {
        let status = self.load_status(snapshot_id)?;
        if !status.state.has_index() {
            return Err("That source snapshot is not available".into());
        }
        let root = self.find_dir(snapshot_id)?.join("source");
        if !root.is_dir() {
            return Err("The source snapshot is missing from disk".into());
        }
        Ok(root)
    }

    // ---- directories and persisted records -----------------------------------------------------------------------

    fn active_job(&self, job_id: &str) -> Option<Arc<Job>> {
        self.state
            .lock()
            .active
            .values()
            .find(|job| job.id == job_id)
            .cloned()
    }

    fn find_dir(&self, id: &str) -> Result<PathBuf> {
        if !valid_id(id) {
            return Err("Invalid source index id".into());
        }
        if let Some(dir) = self.state.lock().directories.get(id) {
            if dir.is_dir() {
                return Ok(dir.clone());
            }
        }
        let entries =
            fs::read_dir(&self.root).map_err(|_| "Unknown source index id".to_string())?;
        let entries = entries.flatten();
        for entry in entries {
            let candidate = entry.path().join(id);
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) && candidate.is_dir() {
                self.state
                    .lock()
                    .directories
                    .insert(id.to_string(), candidate.clone());
                return Ok(candidate);
            }
        }
        Err("Unknown source index id".into())
    }

    fn load_status(&self, id: &str) -> Result<SourceIndexStatus> {
        let dir = self.find_dir(id)?;
        read_json(&dir.join("status.json"), MAX_STATUS_BYTES)
            .map_err(|_| "That source index attempt has no saved result".to_string())
    }

    fn load_inventory(&self, id: &str) -> Result<InventoryRecord> {
        read_json(
            &self.find_dir(id)?.join("inventory.json"),
            MAX_INVENTORY_BYTES,
        )
    }

    /// Probes the parser runtime (a short local process) and caches the answer for status display.
    fn capabilities(&self) -> SourceIndexCapabilities {
        let probed = self.parser.probe();
        self.state.lock().capabilities = Some(probed.clone());
        probed
    }

    // ---- the job pipeline ----------------------------------------------------------------------------------------

    fn run(self: Arc<Self>, job: Arc<Job>, folder: PathBuf, key: String) {
        // Defense in depth only: expected inventory/parser failures use Result and ParserOutcome.
        let finish = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.pipeline(&job, &folder)
        })) {
            Ok(finish) => finish,
            Err(_) => Finish::Failed("The source index worker stopped unexpectedly.".into()),
        };
        self.finalize(&job, &key, finish);
    }

    fn fail(job: &Job, message: String) -> Finish {
        if job.cancelled() {
            Finish::Cancelled
        } else {
            Finish::Failed(message)
        }
    }

    fn pipeline(&self, job: &Job, folder: &Path) -> Finish {
        let started = Instant::now();
        let control = RunControl {
            cancel: job.cancel.clone(),
            log: Arc::new(|_| {}),
        };
        let capabilities = self.capabilities();
        job.update(|s| s.capabilities = capabilities.clone());
        if job.cancelled() {
            return Finish::Cancelled;
        }

        let (record, source_dir) = match Self::snapshot(job, folder, &control, started) {
            Ok(done) => done,
            Err(finish) => return finish,
        };
        if job.cancelled() {
            return Finish::Cancelled;
        }

        let manifest_files = record.manifest_files();
        let expected = Expected {
            snapshot_id: &record.snapshot_id,
            folder: &record.folder,
            source_hash: &record.source_hash,
            files: &manifest_files,
            directories: &record.directories,
        };
        if !capabilities.available {
            // Without a parser the inventory and structure stay viewable; files are marked skipped, not parsed.
            let reason = capabilities
                .hint
                .clone()
                .unwrap_or_else(|| "The parser runtime is unavailable".into());
            return inventory_only_finish(
                &expected,
                &record,
                FileStatus::Skipped,
                "Not parsed: the parser runtime is unavailable",
                reason,
            );
        }
        self.parse(job, &expected, &capabilities, &record, &source_dir, started)
    }

    /// Admits and copies the folder, then persists the immutable inventory. A failure is returned as the job's final state.
    fn snapshot(
        job: &Job,
        folder: &Path,
        control: &RunControl,
        started: Instant,
    ) -> std::result::Result<(InventoryRecord, PathBuf), Finish> {
        let admitted = inventory::admit(folder, control).map_err(|e| Self::fail(job, e))?;
        job.update(|s| s.total_files = admitted.files.len() as u64);
        let source_dir = job.dir.join("source");
        fs::create_dir_all(&source_dir)
            .map_err(|e| Self::fail(job, format!("Cannot create the snapshot directory: {e}")))?;
        let copied = inventory::copy_snapshot(&admitted, &source_dir, control, &|done| {
            job.update(|s| s.completed_files = done)
        })
        .map_err(|e| Self::fail(job, e))?;
        if started.elapsed() >= JOB_DEADLINE {
            return Err(Finish::Failed(
                "The source index job exceeded its fifteen-minute deadline.".into(),
            ));
        }

        let limitations = inventory_limitations(&admitted);
        let record = InventoryRecord {
            version: 1,
            snapshot_id: job.id.clone(),
            folder: folder.to_string_lossy().into_owned(),
            created_at: now_ms(),
            source_hash: copied.source_hash,
            files: copied.files,
            directories: admitted.directories,
            exclusions: admitted.excluded_samples,
            limitations,
        };
        write_json(
            &job.dir.join("inventory.json"),
            &record,
            MAX_INVENTORY_BYTES,
        )
        .map_err(|e| Self::fail(job, format!("Cannot save the snapshot inventory: {e}")))?;
        job.update(|s| {
            s.snapshot_id = Some(job.id.clone());
            s.source_hash = Some(record.source_hash.clone());
            s.state = SourceIndexState::Parsing;
            s.completed_files = 0;
        });
        Ok((record, source_dir))
    }

    /// Runs the parser over the copied source and validates what it returns; any parser failure keeps the inventory viewable.
    fn parse(
        &self,
        job: &Job,
        expected: &Expected,
        capabilities: &SourceIndexCapabilities,
        record: &InventoryRecord,
        source_dir: &Path,
        started: Instant,
    ) -> Finish {
        let work = job.dir.join("work");
        if let Err(e) = fs::create_dir_all(&work) {
            return Self::fail(
                job,
                format!("Cannot create the parser scratch directory: {e}"),
            );
        }
        let (manifest_path, output_path) = (work.join("manifest.json"), work.join("output.json"));
        let manifest = json!({
            "schemaVersion": 1, "snapshotId": record.snapshot_id, "folder": record.folder, "sourceHash": record.source_hash,
            "root": source_dir.to_string_lossy(), "files": expected.files, "directories": record.directories, "limitations": record.limitations,
        });
        if let Err(e) = write_json(&manifest_path, &manifest, MAX_INVENTORY_BYTES) {
            return Self::fail(job, format!("Cannot write the parser manifest: {e}"));
        }
        let remaining = JOB_DEADLINE
            .saturating_sub(started.elapsed())
            .max(Duration::from_secs(1));
        let run = ExtractRun {
            manifest: &manifest_path,
            output: &output_path,
            work_dir: &work,
            timeout: remaining,
            cancel: job.cancel.clone(),
        };
        let outcome = self.extract_with_progress(job, &run, &work);
        let rejected = |reason: String| {
            inventory_only_finish(
                expected,
                record,
                FileStatus::Failed,
                "Not parsed: the parser run did not produce accepted facts",
                reason,
            )
        };
        match outcome {
            ParserOutcome::Cancelled => Finish::Cancelled,
            _ if job.cancelled() => Finish::Cancelled,
            ParserOutcome::TimedOut => rejected(format!(
                "The parser stopped at the {} minute deadline; no facts were accepted.",
                JOB_DEADLINE.as_secs() / 60
            )),
            ParserOutcome::Failed(message) => rejected(message),
            ParserOutcome::Finished => {
                job.update(|s| s.state = SourceIndexState::Resolving);
                match self.accept_output(&output_path, expected, capabilities, record) {
                    Ok(index) => Finish::Index {
                        index: Box::new(index),
                        error: None,
                    },
                    Err(e) => rejected(format!("The parser output was rejected: {e}")),
                }
            }
        }
    }

    /// Runs the parser while a sibling thread mirrors its progress file into the job status.
    fn extract_with_progress(&self, job: &Job, run: &ExtractRun, work: &Path) -> ParserOutcome {
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    Self::read_progress(job, &work.join("progress.json"));
                    std::thread::sleep(Duration::from_millis(250));
                }
            });
            let outcome = self.parser.extract(run);
            stop.store(true, Ordering::Relaxed);
            outcome
        })
    }

    fn read_progress(job: &Job, path: &Path) {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Progress {
            completed_files: u64,
            total_files: u64,
        }
        let Ok(progress) = read_json::<Progress>(path, 64 * 1024) else {
            return;
        };
        job.update(|s| {
            s.completed_files = progress
                .completed_files
                .min(s.total_files.max(progress.total_files));
            s.state =
                if progress.total_files > 0 && progress.completed_files >= progress.total_files {
                    SourceIndexState::Resolving
                } else {
                    SourceIndexState::Parsing
                };
        });
    }

    /// Reads the parser's facts through a size cap and validates them against the immutable inventory.
    fn accept_output(
        &self,
        path: &Path,
        expected: &Expected,
        capabilities: &SourceIndexCapabilities,
        record: &InventoryRecord,
    ) -> Result<SourceGraphIndex> {
        let file =
            File::open(path).map_err(|e| format!("The parser produced no output file: {e}"))?;
        let size = file.metadata().map_err(|e| e.to_string())?.len();
        if size > MAX_OUTPUT_BYTES {
            return Err(format!(
                "The parser output is {size} bytes, over the {} MiB limit",
                MAX_OUTPUT_BYTES / 1024 / 1024
            ));
        }
        let mut index: SourceGraphIndex = serde_json::from_reader(BufReader::new(file))
            .map_err(|e| format!("The parser output is not a valid source index: {e}"))?;
        index
            .extractor_versions
            .entry("ompWorkerScriptsSha256")
            .or_insert_with(|| json!(self.parser.scripts_hash()));
        index
            .extractor_versions
            .entry("ompRuntime")
            .or_insert_with(|| json!(capabilities.runtime));
        let grammars: serde_json::Map<String, Value> = capabilities
            .profiles
            .iter()
            .map(|p| {
                (
                    p.language.clone(),
                    json!({ "version": p.version, "available": p.available, "error": p.error }),
                )
            })
            .collect();
        index
            .extractor_versions
            .entry("ompGrammars")
            .or_insert(Value::Object(grammars));
        for limitation in &record.limitations {
            if !index.limitations.contains(limitation) {
                index.limitations.push(limitation.clone());
            }
        }
        index.finish(expected)?;
        Ok(index)
    }

    // ---- terminal bookkeeping ------------------------------------------------------------------------------------

    fn finalize(&self, job: &Job, key: &str, finish: Finish) {
        let repository = self.root.join(key);
        let mut status = job.status();
        let mut keep_snapshot = false;
        match finish {
            Finish::Index { index, error } => {
                let complete = !index.is_partial() && error.is_none();
                status.state = if complete {
                    SourceIndexState::Complete
                } else {
                    SourceIndexState::Partial
                };
                status.error = error;
                status.coverage = Some(index.coverage.clone());
                status.completed_files = index.files.len() as u64;
                status.total_files = index.files.len() as u64;
                status.indexed_at = Some(now_ms());
                match self.save_index(&job.dir, &index) {
                    Ok(()) => {
                        keep_snapshot = true;
                        let mut state = self.state.lock();
                        state.indexes.retain(|(id, _)| *id != job.id);
                        // Parsed facts are served from memory right away; the lookup was built during validation.
                        state.indexes.push((job.id.clone(), Arc::new(*index)));
                        if state.indexes.len() > CACHED_INDEXES {
                            state.indexes.remove(0);
                        }
                    }
                    Err(e) => {
                        status.state = SourceIndexState::Failed;
                        status.error = Some(e);
                        status.coverage = None;
                    }
                }
            }
            Finish::Failed(message) => {
                status.state = SourceIndexState::Failed;
                status.error = Some(message);
            }
            Finish::Cancelled => {
                status.state = SourceIndexState::Cancelled;
                status.error = None;
            }
        }
        if !keep_snapshot {
            // No index was produced: the copied source would be an orphan, so the attempt keeps only its status.
            status.snapshot_id = None;
            status.source_hash = None;
            let _ = fs::remove_dir_all(job.dir.join("source"));
            let _ = fs::remove_file(job.dir.join("inventory.json"));
        }
        let _ = fs::remove_dir_all(job.dir.join("work"));
        status.stale = None;
        if let Err(e) = write_json(&job.dir.join("status.json"), &status, MAX_STATUS_BYTES) {
            status.state = SourceIndexState::Failed;
            status.error = Some(format!("Cannot save the source index result: {e}"));
        } else {
            let pointer = Pointer {
                snapshot_id: job.id.clone(),
            };
            let _ = write_json(&repository.join("latest.json"), &pointer, 4096);
            if status.state == SourceIndexState::Complete {
                let _ = write_json(&repository.join("selected.json"), &pointer, 4096);
            }
        }
        // Pruning and deregistration come first so that whoever sees the terminal status sees the final disk state.
        self.prune(&repository);
        self.state.lock().active.remove(key);
        job.update(|s| *s = status);
    }

    fn save_index(&self, dir: &Path, index: &SourceGraphIndex) -> Result<()> {
        let temporary = dir.join(format!("index.{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut writer = BufWriter::new(File::create(&temporary).map_err(|e| e.to_string())?);
            serde_json::to_writer(&mut writer, index).map_err(|e| e.to_string())?;
            let file = writer.into_inner().map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            drop(file);
            if fs::metadata(&temporary).map_err(|e| e.to_string())?.len() > MAX_SAVED_BYTES {
                return Err("The source index exceeds its size limit".to_string());
            }
            fs::rename(&temporary, dir.join("index.json")).map_err(|e| e.to_string())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(|e| format!("Cannot save the source index: {e}"))
    }

    /// Keeps complete snapshots (Architect results bind them), the selected and the latest attempt; removes
    /// superseded failed/cancelled/partial attempts and long-abandoned interrupted ones.
    fn prune(&self, repository: &Path) {
        let selected = read_pointer(&repository.join("selected.json"));
        let latest = read_pointer(&repository.join("latest.json"));
        let running: Vec<String> = self
            .state
            .lock()
            .active
            .values()
            .map(|job| job.id.clone())
            .collect();
        let Ok(entries) = fs::read_dir(repository) else {
            return;
        };
        let entries = entries.flatten();
        for entry in entries {
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !valid_id(&id)
                || !entry.path().is_dir()
                || running.contains(&id)
                || selected.as_deref() == Some(&id)
                || latest.as_deref() == Some(&id)
            {
                continue;
            }
            let removable = match read_json::<SourceIndexStatus>(
                &entry.path().join("status.json"),
                MAX_STATUS_BYTES,
            ) {
                Ok(status) => status.state != SourceIndexState::Complete,
                Err(_) => entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > ORPHAN_AGE),
            };
            if removable {
                let mut state = self.state.lock();
                state.directories.remove(&id);
                state.indexes.retain(|(cached, _)| *cached != id);
                drop(state);
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
}
