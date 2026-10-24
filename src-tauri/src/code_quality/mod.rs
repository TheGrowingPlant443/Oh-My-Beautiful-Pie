//! Code quality service: one authoritative configuration, one operation gate, one scan at a time.
//! Module map: `types` (wire DTOs), `store` (versioned state), `secrets`/`secrets_unix` (OS credential
//! store), `runtime`/`install`/`job` (container engines; `runtime/native` is the Linux/macOS adapter), `sonar`/`scanners`/`snapshot` (SonarQube path),
//! `alternate`/`reports` (MegaLinter + Opengrep + Semgrep CE path), `lifecycle` (configuration and
//! Community server transitions), `scan` (scan jobs).
pub mod alternate;
#[cfg(unix)]
pub(crate) mod cli;
pub mod install;
pub mod job;
pub mod credentials;
mod lifecycle;
mod process_lock;
mod ownership;
pub mod reports;
mod report_html;
pub mod runtime;
mod scan;
pub mod scanners;
pub mod secrets;
#[cfg(unix)]
pub mod secrets_unix;
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
pub(crate) type ProbeFn = Arc<dyn Fn(&std::path::Path, &QualityRuntime) -> (Vec<Prerequisite>, QualityOptions) + Send + Sync>;


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
    /// A setup run stopped for a Windows restart; the next run is labelled "Continue setup".
    pub setup_resume: bool,
    pub suspended: bool,
}

pub struct CodeQualityService {
    pub(crate) config_root: PathBuf,
    pub(crate) paths: Paths,
    pub(crate) factory: Arc<dyn EngineFactory>,
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) inner: Mutex<Inner>,
    pub(crate) ownership: Mutex<ownership::Ownership>,
    /// Read-only prerequisite probe; a seam so tests never touch the real machine.
    pub(crate) probe: ProbeFn,
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

/// `windows`, `linux` or `macos` (the only platforms this module supports).
pub(crate) fn platform_name() -> &'static str {
    if cfg!(windows) { "windows" } else if cfg!(target_os = "macos") { "macos" } else { "linux" }
}

/// What "Install and setup" still has to do, derived only from cached probes and state (never probes).
/// On Linux/macOS nothing is ever installed: the runtime step only asks whether the engine is usable.
pub(crate) fn setup_state(inner: &Inner, credential_store_ok: bool) -> SetupState {
    let config = &inner.file.config;
    let probed = !inner.prerequisites.is_empty();
    let prerequisite = inner
        .prerequisites
        .iter()
        .find(|p| std::mem::discriminant(&p.runtime) == std::mem::discriminant(&config.runtime));
    let mut steps = Vec::new();
    let mut push = |id: &str, label: String, done: bool, blocked: bool| {
        let state = if done {
            SetupStepState::Done
        } else if blocked {
            SetupStepState::Blocked
        } else {
            SetupStepState::Pending
        };
        steps.push(SetupStep { id: id.to_string(), label, state });
    };
    let windows = cfg!(windows);
    let wsl_runtime = windows && matches!(config.runtime, QualityRuntime::Podman { .. } | QualityRuntime::DockerWsl { .. });
    if wsl_runtime {
        let restart = prerequisite.is_some_and(|p| p.needs_restart);
        push("windowsFeatures", "Enable Windows features for WSL (needs one restart)".into(), probed && !restart && !inner.options.wsl_features_missing, restart);
    }
    let installed = prerequisite.is_some_and(|p| p.installed);
    if windows {
        push("runtime", format!("Install {}", config.runtime.label()), installed, false);
    } else {
        push("runtime", format!("{} is installed and reachable (detected, never installed by this app)", config.runtime.label()), prerequisite.is_some_and(|p| p.ready), false);
        push("credentials", "Credential store (Secret Service / Keychain) is usable".into(), credential_store_ok, false);
    }
    if let (true, QualityRuntime::Podman { connection }) = (windows, &config.runtime) {
        push("machine", "Create the app's Podman machine".into(), probed && prerequisite.is_some_and(|p| p.installed) && (inner.options.podman_app_connection.is_some() || !connection.is_empty()), false);
    }
    push("server", "Start the analyzer".into(), inner.effective == Some(config.mode), false);
    if config.mode == QualityMode::SonarqubeCommunity {
        push("token", "Create the scan token".into(), inner.credentials.community, false);
    }
    let needed = steps.iter().any(|s| s.state != SetupStepState::Done);
    let install_required = windows && steps.iter().any(|s| matches!(s.id.as_str(), "windowsFeatures" | "runtime" | "machine") && s.state != SetupStepState::Done);
    SetupState { needed, install_required, resume: inner.setup_resume, steps }
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

/// The agents' `code_quality_report` tool lives in this module next to the bridge extension, which imports it.
pub fn materialize_bridge(folder: &std::path::Path) -> Result<()> {
    let file = folder.join("omp-code-quality.mjs");
    let source = include_str!("../../../bridge/omp-code-quality.mjs");
    if std::fs::read_to_string(&file).ok().as_deref() != Some(source) {
        std::fs::write(file, source).map_err(|e| e.to_string())?;
    }
    Ok(())
}

impl CodeQualityService {
    /// Production constructor: real engines, the OS credential store, cross-process lock.
    pub fn new(config_root: PathBuf) -> Arc<Self> {
        let factory: Arc<dyn EngineFactory> = Arc::new(runtime::RuntimeFactory { config_root: config_root.clone() });
        #[cfg(windows)]
        let secrets: Arc<dyn SecretStore> = Arc::new(secrets::WindowsSecrets);
        #[cfg(unix)]
        let secrets: Arc<dyn SecretStore> = Arc::new(secrets_unix::ExternalSecrets::system());
        #[cfg(not(any(windows, unix)))]
        compile_error!("code quality has no credential store for this platform");
        let root = config_root.join("code-quality");
        let name = process_lock::name_for(&root);
        let lock = process_lock::ProcessLock::acquire(&name);
        Self::with_parts(config_root, factory, secrets, lock, Arc::new(install::probe_prerequisites))
    }

    pub(crate) fn with_parts(
        config_root: PathBuf,
        factory: Arc<dyn EngineFactory>,
        secrets: Arc<dyn SecretStore>,
        lock: Result<Option<process_lock::ProcessLock>>,
        probe: ProbeFn,
    ) -> Arc<Self> {
        let paths = Paths::new(config_root.join("code-quality"));
        let (held, lock_error) = match lock {
            Ok(held) => (held, None),
            Err(error) => (None, Some(error)),
        };
        let mut fatal = None;
        let file = store::load_config(&paths).unwrap_or_else(|e| {
            fatal = Some(e);
            ConfigFile::default()
        });
        let installation = (if held.is_some() { store::load_installation(&paths) } else { store::read_installation(&paths).map(|id| id.unwrap_or_default()) }).unwrap_or_else(|e| {
            fatal.get_or_insert(e);
            String::new()
        });
        if let Err(e) = store::load_resources(&paths) {
            fatal.get_or_insert(e);
        }
        let setup_resume = config_root.join("setup").join("pending-setup.json").is_file();
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
                setup_resume,
                suspended: false,
            }),
            ownership: Mutex::new(ownership::Ownership::new(held)),
            probe,
            closing: AtomicBool::new(false),
            workers: Mutex::new(Vec::new()),
        });
        svc.refresh_rules();
        svc.refresh_credentials();
        if svc.owns_resources() && svc.inner.lock().fatal.is_none() {
            svc.recover_interrupted_jobs();
            if let Err(error) = svc.publish_owner() {
                svc.inner.lock().fatal = Some(error.clone());
                svc.inner.lock().error = Some(error);
            }
        }
        svc
    }

    /// A read-only service (no process ownership, no credentials, a factory that must never be asked for an engine) so other
    /// modules can test saved-result reads without touching a container runtime.
    #[cfg(test)]
    pub(crate) fn passive_for_tests(config_root: PathBuf) -> Arc<Self> {
        struct NoEngine;
        impl EngineFactory for NoEngine {
            fn engine(&self, _: &QualityRuntime) -> Result<Arc<dyn runtime::Engine>> {
                panic!("a saved-result read must not reach a container engine")
            }
        }
        Self::with_parts(config_root, Arc::new(NoEngine), Arc::new(secrets::MemorySecrets::default()), Ok(None),
            Arc::new(|_, _| (Vec::new(), QualityOptions::default())))
    }

    pub(crate) fn pending_setup_path(&self) -> PathBuf {
        self.config_root.join("setup").join("pending-setup.json")
    }

    pub fn state(&self) -> QualityState {
        // Before the ownership/inner locks: the credential store answers from a cache and never blocks.
        let credential_store = self.secrets.status();
        let ownership = self.ownership.lock();
        let ownership_state = ownership.snapshot(&self.paths);
        let inner = self.inner.lock();
        let config = inner.file.config.clone();
        QualityState {
            revision: inner.file.revision,
            config,
            activated: inner.file.activated,
            ownership: ownership_state,
            suspended: inner.suspended,
            effective_mode: inner.effective,
            phase: inner.phase,
            busy: inner.busy,
            operation_id: inner.operation_id.clone(),
            requested_config: inner.requested.clone(),
            error: inner.error.clone(),
            community: inner.community.clone(),
            credentials: inner.credentials.clone(),
            rules: inner.rules.clone(),
            prerequisites: inner.prerequisites.clone(),
            options: inner.options.clone(),
            active_scan_id: inner.active_scan.as_ref().map(|s| s.id.clone()),
            diagnostics: inner.diagnostics.iter().cloned().collect(),
            unresolved: inner.unresolved.clone(),
            setup: setup_state(&inner, credential_store.available),
            start_at_launch: inner.file.start_at_launch,
            platform: platform_name().to_string(),
            credential_store,
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
        self.refresh_ownership()?;
        self.admit_internal(phase, requested, false, false)
    }

    pub(crate) fn admit_internal(self: &Arc<Self>, phase: QualityPhase, requested: Option<QualityConfig>, transfer: bool, exit: bool) -> Result<Gate> {
        let ownership = self.ownership.lock();
        let mut inner = self.inner.lock();
        if let Some(fatal) = &inner.fatal {
            return Err(fatal.clone());
        }
        if ownership.transferring && !transfer { return Err(BUSY_MESSAGE.into()); }
        if ownership.lock.is_none() || ownership.handed_off {
            return Err("Another OMP Manager process manages code quality; close it to change settings.".into());
        }
        if self.closing.load(Ordering::Relaxed) && !exit {
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

    /// Keep OS authority until every admitted operation and worker has actually stopped.
    pub fn shutdown(self: &Arc<Self>) {
        if self.closing.swap(true, Ordering::AcqRel) {
            // Concurrent close requests must not release the mutex underneath the first cleanup.
            while self.owns_resources() {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            return;
        }
        self.request_cancel_all();
        for handle in std::mem::take(&mut *self.workers.lock()) {
            let _ = handle.join();
        }
        while self.ownership.lock().transferring {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        while self.inner.lock().busy {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if self.owns_resources() {
            self.stop_all_on_exit();
            self.release_owner(false);
        }
    }

}

#[cfg(test)]
mod tests;
