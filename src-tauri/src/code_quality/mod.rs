//! Code quality service: one authoritative configuration, one operation gate, one scan at a time.
//! Module map: `types` (wire DTOs), `store` (versioned state), `secrets` (Credential Manager),
//! `runtime`/`install`/`job` (container engines), `sonar`/`scanners`/`snapshot` (SonarQube path),
//! `alternate`/`reports` (MegaLinter + Opengrep + Semgrep CE path), `lifecycle` (configuration and
//! Community server transitions), `scan` (scan jobs).
pub mod alternate;
pub mod install;
pub mod job;
mod lifecycle;
mod process_lock;
pub mod reports;
pub mod runtime;
mod scan;
pub mod scanners;
pub mod secrets;
pub mod snapshot;
pub mod sonar;
pub mod store;
pub mod types;

use parking_lot::Mutex;
use runtime::{Ctl, EngineFactory};
use secrets::SecretStore;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use store::{ConfigFile, Paths};
use types::*;

const MAX_DIAGNOSTICS: usize = 200;

/// A scan currently owning the gate; `scan` is the live copy served to pollers.
pub(crate) struct ActiveScan {
    pub id: String,
    pub cancel: Arc<AtomicBool>,
    pub scan: QualityScan,
}

pub(crate) struct Inner {
    pub file: ConfigFile,
    pub installation: String,
    /// Unreadable/unsupported persisted state: every mutation is refused with this text.
    pub fatal: Option<String>,
    pub phase: QualityPhase,
    pub busy: bool,
    pub operation_id: Option<String>,
    pub requested: Option<QualityConfig>,
    pub effective: Option<QualityMode>,
    pub error: Option<String>,
    pub community: CommunityState,
    pub credentials: CredentialPresence,
    pub rules: RulesState,
    pub prerequisites: Vec<Prerequisite>,
    pub options: QualityOptions,
    pub active_scan: Option<ActiveScan>,
    pub diagnostics: VecDeque<String>,
    pub unresolved: Vec<String>,
    pub op_cancel: Option<Arc<AtomicBool>>,
}

pub struct CodeQualityService {
    pub(crate) config_root: PathBuf,
    pub(crate) paths: Paths,
    pub(crate) factory: Arc<dyn EngineFactory>,
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) inner: Mutex<Inner>,
    /// True when another OMP Manager process owns the managed resources: this process only reads.
    pub(crate) read_only: bool,
    process_lock: Mutex<Option<process_lock::ProcessLock>>,
    pub(crate) closing: AtomicBool,
    pub(crate) workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

/// Admission to the single operation gate. Dropping without `finish` restores the previous
/// phase/error, so a rejected request never disturbs what the user sees.
pub(crate) struct Gate {
    svc: Arc<CodeQualityService>,
    pub ctl: Ctl,
    previous: (QualityPhase, Option<String>),
    finished: bool,
}

impl Gate {
    pub fn phase(&self, phase: QualityPhase) {
        self.svc.inner.lock().phase = phase;
    }
    pub fn say(&self, line: &str) {
        self.ctl.say(line);
    }
    pub fn cancelled(&self) -> bool {
        self.ctl.cancelled()
    }
    /// Publish the final outcome and release the gate.
    pub fn finish(mut self, phase: QualityPhase, error: Option<String>) {
        self.finished = true;
        let mut inner = self.svc.inner.lock();
        inner.phase = phase;
        inner.error = error;
        inner.requested = None;
        inner.busy = false;
        inner.operation_id = None;
        inner.op_cancel = None;
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut inner = self.svc.inner.lock();
        inner.phase = self.previous.0;
        inner.error = self.previous.1.clone();
        inner.requested = None;
        inner.busy = false;
        inner.operation_id = None;
        inner.op_cancel = None;
    }
}

pub(crate) fn redact_line(line: &str) -> String {
    let mut text: String = line.chars().filter(|c| !c.is_control() || *c == '\t').collect();
    if text.len() > 600 {
        let mut cut = 600;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push('…');
    }
    text
}

impl CodeQualityService {
    /// Production constructor: real engines, Windows Credential Manager, cross-process lock.
    pub fn new(config_root: PathBuf) -> Arc<Self> {
        let factory: Arc<dyn EngineFactory> = Arc::new(runtime::RuntimeFactory { config_root: config_root.clone() });
        #[cfg(windows)]
        let secrets: Arc<dyn SecretStore> = Arc::new(secrets::WindowsSecrets);
        #[cfg(not(windows))]
        let secrets: Arc<dyn SecretStore> = Arc::new(secrets::MemorySecrets::default());
        let root = config_root.join("code-quality");
        let name = process_lock::name_for(&root);
        let lock = process_lock::ProcessLock::acquire(&name);
        Self::with_parts(config_root, factory, secrets, lock)
    }

    pub(crate) fn with_parts(
        config_root: PathBuf,
        factory: Arc<dyn EngineFactory>,
        secrets: Arc<dyn SecretStore>,
        lock: Result<Option<process_lock::ProcessLock>>,
    ) -> Arc<Self> {
        let paths = Paths::new(config_root.join("code-quality"));
        let mut fatal = None;
        let file = store::load_config(&paths).unwrap_or_else(|e| {
            fatal = Some(e);
            ConfigFile::default()
        });
        let installation = store::load_installation(&paths).unwrap_or_else(|e| {
            fatal.get_or_insert(e);
            String::new()
        });
        if let Err(e) = store::load_resources(&paths) {
            fatal.get_or_insert(e);
        }
        let (read_only, held, lock_error) = match lock {
            Ok(Some(lock)) => (false, Some(lock), None),
            Ok(None) => (true, None, None),
            Err(e) => (true, None, Some(e)),
        };
        let svc = Arc::new(CodeQualityService {
            config_root: config_root.clone(),
            paths,
            factory,
            secrets,
            inner: Mutex::new(Inner {
                file,
                installation,
                fatal: fatal.clone(),
                phase: QualityPhase::Idle,
                busy: false,
                operation_id: None,
                requested: None,
                effective: None,
                error: fatal.or(lock_error),
                community: CommunityState::default(),
                credentials: CredentialPresence::default(),
                rules: RulesState::default(),
                prerequisites: Vec::new(),
                options: QualityOptions::default(),
                active_scan: None,
                diagnostics: VecDeque::new(),
                unresolved: Vec::new(),
                op_cancel: None,
            }),
            read_only,
            process_lock: Mutex::new(held),
            closing: AtomicBool::new(false),
            workers: Mutex::new(Vec::new()),
        });
        svc.refresh_rules();
        svc.refresh_credentials();
        svc
    }

    pub fn state(&self) -> QualityState {
        let inner = self.inner.lock();
        let config = inner.file.config.clone();
        QualityState {
            revision: inner.file.revision,
            config,
            activated: inner.file.activated,
            effective_mode: inner.effective,
            phase: inner.phase,
            busy: inner.busy,
            operation_id: inner.operation_id.clone(),
            requested_config: inner.requested.clone(),
            error: inner.error.clone().or_else(|| {
                if self.read_only {
                    Some("Another OMP Manager process manages code quality; this one can only read saved results.".into())
                } else {
                    None
                }
            }),
            community: inner.community.clone(),
            credentials: inner.credentials.clone(),
            rules: inner.rules.clone(),
            prerequisites: inner.prerequisites.clone(),
            options: inner.options.clone(),
            active_scan_id: inner.active_scan.as_ref().map(|s| s.id.clone()),
            diagnostics: inner.diagnostics.iter().cloned().collect(),
            unresolved: inner.unresolved.clone(),
        }
    }

    pub(crate) fn refresh_rules(&self) {
        let rules = alternate::rules_state(&self.config_root);
        self.inner.lock().rules = rules;
    }

    pub(crate) fn push_diagnostic(&self, line: &str) {
        let mut inner = self.inner.lock();
        if inner.diagnostics.len() >= MAX_DIAGNOSTICS {
            inner.diagnostics.pop_front();
        }
        inner.diagnostics.push_back(redact_line(line));
    }

    /// Single atomic admission. A second request is rejected immediately, never queued.
    pub(crate) fn admit(self: &Arc<Self>, phase: QualityPhase, requested: Option<QualityConfig>) -> Result<Gate> {
        let mut inner = self.inner.lock();
        if let Some(fatal) = &inner.fatal {
            return Err(fatal.clone());
        }
        if self.read_only {
            return Err("Another OMP Manager process manages code quality; close it to change settings.".into());
        }
        if self.closing.load(Ordering::Relaxed) {
            return Err("OMP Manager is closing.".into());
        }
        if inner.busy {
            return Err(BUSY_MESSAGE.into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let previous = (inner.phase, inner.error.clone());
        inner.busy = true;
        inner.phase = phase;
        inner.operation_id = Some(id.clone());
        inner.requested = requested;
        inner.error = None;
        let cancel = Arc::new(AtomicBool::new(false));
        inner.op_cancel = Some(cancel.clone());
        drop(inner);
        let weak = Arc::downgrade(self);
        let log: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |line| {
            if let Some(svc) = weak.upgrade() {
                svc.push_diagnostic(line);
            }
        });
        Ok(Gate { svc: self.clone(), ctl: Ctl { cancel, log }, previous, finished: false })
    }

    pub(crate) fn check_revision(&self, expected: u64) -> Result<()> {
        if self.inner.lock().file.revision != expected {
            return Err(STALE_MESSAGE.into());
        }
        Ok(())
    }

    /// Stable per-resource-set identity: installation + runtime + daemon fingerprint. `None` until a
    /// daemon fingerprint has been recorded for that runtime.
    pub(crate) fn resource_set_id(&self, runtime: &QualityRuntime) -> Option<String> {
        let resources = store::load_resources(&self.paths).ok()?;
        let record = resources.runtime(runtime)?;
        let installation = self.inner.lock().installation.clone();
        Some(secrets::sha256_hex(&format!("{installation}|{}|{}", runtime.key(), record.fingerprint))[..16].to_string())
    }

    pub(crate) fn community_token_target(&self, runtime: &QualityRuntime) -> Option<String> {
        let set = self.resource_set_id(runtime)?;
        Some(secrets::target_name(&self.config_root, &format!("community:{set}"), "token"))
    }

    pub(crate) fn db_password_target(&self, runtime: &QualityRuntime, set: &str) -> String {
        let _ = runtime;
        secrets::target_name(&self.config_root, &format!("community:{set}"), "dbpassword")
    }

    pub(crate) fn enterprise_token_target(&self, url: &str) -> String {
        secrets::target_name(&self.config_root, &format!("enterprise:{}", sonar::identity_of(url)), "token")
    }

    pub(crate) fn refresh_credentials(&self) {
        let (config, _) = {
            let inner = self.inner.lock();
            (inner.file.config.clone(), ())
        };
        let community = self
            .community_token_target(&config.runtime)
            .map(|t| matches!(self.secrets.read(&t), Ok(Some(_))))
            .unwrap_or(false);
        let enterprise = !config.enterprise_url.is_empty()
            && matches!(self.secrets.read(&self.enterprise_token_target(&config.enterprise_url)), Ok(Some(_)));
        self.inner.lock().credentials = CredentialPresence { community, enterprise };
    }

    pub(crate) fn installation_id(&self) -> String {
        self.inner.lock().installation.clone()
    }

    /// Cancels the running operation or scan without waiting; callers join via the gate.
    pub(crate) fn request_cancel_all(&self) {
        let inner = self.inner.lock();
        if let Some(cancel) = &inner.op_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(scan) = &inner.active_scan {
            scan.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn spawn_worker(self: &Arc<Self>, name: &str, work: impl FnOnce() + Send + 'static) -> Result<()> {
        let handle = std::thread::Builder::new().name(name.to_string()).spawn(work).map_err(|e| e.to_string())?;
        let mut workers = self.workers.lock();
        workers.retain(|h| !h.is_finished());
        workers.push(handle);
        Ok(())
    }

    /// Exit path: cancel work, join workers, stop only app-owned Community containers, release the lock.
    pub fn shutdown(self: &Arc<Self>) {
        self.closing.store(true, Ordering::Relaxed);
        self.request_cancel_all();
        let handles: Vec<_> = std::mem::take(&mut *self.workers.lock());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        for handle in handles {
            while !handle.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
        if !self.read_only {
            self.stop_all_on_exit();
        }
        self.process_lock.lock().take();
    }

}

#[cfg(test)]
mod tests;
