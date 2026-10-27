//! Configuration transitions and the managed Community (SonarQube + PostgreSQL) lifecycle.
//! Everything here runs under the single operation `Gate`; nothing is queued or speculative.
use super::runtime::{ContainerSpec, Engine, Publish, OWNER_LABEL, ROLE_DB, ROLE_LABEL, ROLE_SONAR};
use super::secrets::{random_password, random_sonar_password, sha256_hex};
use super::sonar::{self, SonarClient, SonarKind};
use super::store::{self, ResourceRecord, RuntimeRecord};
use super::types::*;
use super::{scanners, CodeQualityService, Gate};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) const SPEC_LABEL: &str = "dev.udvikler.omp-manager.spec";
pub(crate) const DB_ALIAS: &str = "omp-quality-db";
pub(crate) const SONAR_ALIAS: &str = "omp-quality-sonar";
const MIN_MAP_COUNT: u64 = 524_288;
const MIN_FILE_MAX: u64 = 131_072;
const MIN_ENGINE_MEMORY: u64 = 5 * 1024 * 1024 * 1024;
const READY_TIMEOUT: Duration = Duration::from_secs(600);
const DB_READY_TIMEOUT: Duration = Duration::from_secs(180);

pub(crate) enum Outcome {
    Ready,
    Setup(String),
}

pub(crate) struct Names {
    pub network: String,
    pub db: String,
    pub sonar: String,
    pub vol_db: String,
    pub vol_data: String,
    pub vol_logs: String,
    pub vol_ext: String,
    pub vol_temp: String,
}

pub(crate) fn names(installation: &str) -> Names {
    let prefix = format!("omp-q-{}", &sha256_hex(installation)[..8]);
    Names {
        network: format!("{prefix}-net"),
        db: format!("{prefix}-db"),
        sonar: format!("{prefix}-sonar"),
        vol_db: format!("{prefix}-db-data"),
        vol_data: format!("{prefix}-sonar-data"),
        vol_logs: format!("{prefix}-sonar-logs"),
        vol_ext: format!("{prefix}-sonar-extensions"),
        vol_temp: format!("{prefix}-sonar-temp"),
    }
}

pub(crate) fn validate_config(config: &QualityConfig) -> Result<QualityConfig> {
    if config.version != 1 {
        return Err("Unsupported code quality settings version.".into());
    }
    if config.community_port < 1024 {
        return Err("Choose a SonarQube port between 1024 and 65535.".into());
    }
    let mut config = config.clone();
    match &config.runtime {
        QualityRuntime::DockerWsl { distro } => {
            if distro.trim().is_empty() || distro.chars().any(|c| c.is_control() || c == '"') {
                return Err("Choose the WSL distribution that runs Docker.".into());
            }
        }
        QualityRuntime::Podman { connection } => {
            if connection.chars().any(|c| !(c.is_ascii_alphanumeric() || "._-@:/".contains(c))) {
                return Err("The Podman connection name contains unsupported characters.".into());
            }
        }
        QualityRuntime::DockerDesktop => {}
        #[cfg(unix)]
        QualityRuntime::DockerNative => {}
    }
    if config.mode == QualityMode::SonarqubeEnterprise || !config.enterprise_url.trim().is_empty() {
        config.enterprise_url = sonar::normalize_enterprise_url(config.enterprise_url.trim())?;
    } else {
        config.enterprise_url = String::new();
    }
    Ok(config)
}

fn valid_token(token: &str) -> Result<()> {
    if token.is_empty() || token.len() > 512 || token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("The token is empty or contains whitespace.".into());
    }
    Ok(())
}

fn spec_hash(parts: &[&str]) -> String {
    sha256_hex(&parts.join("|"))[..16].to_string()
}

fn port_conflict(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("address already in use") || lower.contains("port is already allocated") || lower.contains("only one usage of each socket")
}

impl CodeQualityService {
    fn engine(&self, runtime: &QualityRuntime) -> Result<Arc<dyn Engine>> {
        self.factory.engine(runtime)
    }

    // ---- unresolved ownership bookkeeping -------------------------------------------------

    fn mark_unresolved(&self, runtime: &QualityRuntime, message: String) {
        let prefix = format!("[{}] ", runtime.key());
        let mut inner = self.inner.lock();
        inner.unresolved.retain(|u| !u.starts_with(&prefix));
        inner.unresolved.push(format!("{prefix}{message}"));
        if &inner.file.config.runtime == runtime { inner.community.running = None; }
    }

    fn clear_unresolved(&self, runtime: &QualityRuntime) {
        let prefix = format!("[{}] ", runtime.key());
        self.inner.lock().unresolved.retain(|u| !u.starts_with(&prefix));
    }

    pub(crate) fn has_unresolved(&self) -> bool {
        !self.inner.lock().unresolved.is_empty()
    }

    fn is_unresolved(&self, runtime: &QualityRuntime) -> bool {
        let prefix = format!("[{}] ", runtime.key());
        self.inner.lock().unresolved.iter().any(|u| u.starts_with(&prefix))
    }

    pub(crate) fn set_possibly_running(&self, runtime: &QualityRuntime, fingerprint: Option<&str>, port: u16, running: bool) -> Result<()> {
        let mut resources = store::load_resources(&self.paths)?;
        match resources.runtime_mut(runtime) {
            Some(record) => {
                record.possibly_running = running;
                record.port = port;
                if let Some(fp) = fingerprint {
                    record.fingerprint = fp.to_string();
                }
            }
            None => resources.runtimes.push(RuntimeRecord {
                runtime: runtime.clone(),
                fingerprint: fingerprint.unwrap_or_default().to_string(),
                possibly_running: running,
                port,
            }),
        }
        store::save_resources(&self.paths, &resources)
    }

    // ---- public operations ----------------------------------------------------------------

    pub fn configure(
        self: &Arc<Self>,
        config: QualityConfig,
        expected: u64,
        credential: Option<QualityCredentialUpdate>,
    ) -> Result<QualityState> {
        let gate = self.admit(QualityPhase::Checking, Some(config.clone()))?;
        self.check_revision(expected)?;
        let target = validate_config(&config)?;
        if let Some(update) = &credential {
            match (update.action, update.token.as_deref()) {
                (CredentialAction::Replace, Some(token)) => valid_token(token)?,
                (CredentialAction::Replace, None) => return Err("Enter the token to save.".into()),
                (CredentialAction::Clear, _) => {}
            }
        }
        if credential.as_ref().is_some_and(|c| c.target == CredentialTarget::Community) && target.mode != QualityMode::SonarqubeCommunity {
            return Err("Select SonarQube Community to save or clear its token.".into());
        }
        if target.mode == QualityMode::Alternate {
            super::alternate::require_acknowledged(&self.config_root)?;
        }
        if self.has_unresolved() {
            return Err("A managed SonarQube server could not be confirmed stopped; use Start / retry to reconcile first.".into());
        }
        let committed = self.inner.lock().file.config.clone();
        let same = committed == target;
        if same && credential.is_none() && self.is_healthy(&target) {
            return Ok(self.state_after(gate));
        }
        if same && credential.is_some() && self.is_healthy(&target) {
            return self.credential_only(gate, &target, credential.as_ref().expect("checked"));
        }
        self.run_transition(gate, target, credential)
    }

    pub fn start_service(self: &Arc<Self>, expected: u64) -> Result<QualityState> {
        let gate = self.admit(QualityPhase::Starting, None)?;
        self.check_revision(expected)?;
        self.inner.lock().suspended = false;
        let target = self.inner.lock().file.config.clone();
        if target.mode == QualityMode::Alternate {
            super::alternate::require_acknowledged(&self.config_root)?;
        }
        self.run_transition(gate, target, None)
    }

    pub fn check_prerequisites(self: &Arc<Self>) -> Result<QualityState> {
        let gate = match self.admit(QualityPhase::Checking, None) {
            Ok(gate) => gate,
            Err(_) => return Ok(self.state()),
        };
        let runtime = self.inner.lock().file.config.runtime.clone();
        let (prerequisites, options) = (self.probe)(&self.config_root, &runtime);
        {
            let mut inner = self.inner.lock();
            inner.prerequisites = prerequisites;
            inner.options = options;
        }
        Ok(self.state_after(gate))
    }

    pub fn install_runtime(self: &Arc<Self>, runtime: QualityRuntime, expected: u64, consent: bool) -> Result<QualityState> {
        let gate = self.admit(QualityPhase::Installing, None)?;
        self.check_revision(expected)?;
        if !consent {
            return Err("Review and accept the installation details before installing.".into());
        }
        validate_config(&QualityConfig { runtime: runtime.clone(), ..self.inner.lock().file.config.clone() })?;
        gate.say(&format!("Preparing {}", runtime.label()));
        let outcome = super::install::install_runtime(&self.config_root, &runtime, &gate.ctl);
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                self.refresh_prerequisites(&runtime);
                gate.finish(QualityPhase::Error, Some(error.clone()));
                return Err(error);
            }
        };
        gate.say(&outcome.message);
        if let Some(connection) = &outcome.podman_connection {
            gate.say(&format!("Podman connection: {connection}"));
        }
        if outcome.needs_restart {
            self.refresh_prerequisites(&runtime);
            gate.finish(QualityPhase::NeedsRestart, Some(outcome.message));
            return Ok(self.state());
        }
        // The Elasticsearch prerequisites are part of the explicitly consented setup.
        if let Some(message) = self.ensure_vm_limits(&gate, &runtime) {
            self.refresh_prerequisites(&runtime);
            gate.finish(QualityPhase::NeedsSetup, Some(message));
            return Ok(self.state());
        }
        self.refresh_prerequisites(&runtime);
        gate.finish(QualityPhase::Idle, None);
        Ok(self.state())
    }

    pub fn enable_windows_features(self: &Arc<Self>, expected: u64, consent: bool) -> Result<QualityState> {
        let gate = self.admit(QualityPhase::Installing, None)?;
        self.check_revision(expected)?;
        if !consent {
            return Err("Review and accept the Windows feature changes before continuing.".into());
        }
        match super::install::enable_windows_features(&self.config_root, &gate.ctl) {
            Ok(outcome) if outcome.needs_restart => {
                gate.finish(QualityPhase::NeedsRestart, Some(outcome.message));
                Ok(self.state())
            }
            Ok(outcome) => {
                gate.say(&outcome.message);
                gate.finish(QualityPhase::Idle, None);
                Ok(self.state())
            }
            Err(error) => {
                gate.finish(QualityPhase::Error, Some(error.clone()));
                Err(error)
            }
        }
    }

    pub fn acknowledge_rules(self: &Arc<Self>, manifest_hash: String, license_hash: String, expected: u64, consent: bool) -> Result<QualityState> {
        let gate = self.admit(QualityPhase::Checking, None)?;
        self.check_revision(expected)?;
        if !consent {
            return Err("Acknowledge the rules license to continue.".into());
        }
        super::alternate::acknowledge_rules(&self.config_root, &manifest_hash, &license_hash)?;
        self.refresh_rules();
        Ok(self.state_after(gate))
    }

    /// Raises the container VM limits SonarQube's search engine needs when they are too low.
    /// `Some(message)` = it could not be done (or is unsupported on this engine).
    fn ensure_vm_limits(&self, gate: &Gate, runtime: &QualityRuntime) -> Option<String> {
        let engine = self.engine(runtime).ok()?;
        let probe = engine.probe(&gate.ctl).ok()?;
        let low = probe.max_map_count.is_some_and(|v| v < MIN_MAP_COUNT) || probe.file_max.is_some_and(|v| v < MIN_FILE_MAX);
        if !low {
            return None;
        }
        gate.say("Raising the container VM limits SonarQube's search engine needs");
        super::install::raise_vm_limits_at(engine.as_ref(), &self.config_root, &gate.ctl).err()
    }

    fn write_pending_setup(&self) -> Result<()> {
        let path = self.pending_setup_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        crate::dependency_security::atomic_json(&path, &serde_json::json!({ "version": 1, "at": now_millis() }), store::MAX_STATE_BYTES)?;
        self.inner.lock().setup_resume = true;
        Ok(())
    }

    fn clear_pending_setup(&self) {
        let _ = std::fs::remove_file(self.pending_setup_path());
        self.inner.lock().setup_resume = false;
    }

    /// One-click "Install and setup": every missing prerequisite in order, then start the analyzer.
    pub fn setup(self: &Arc<Self>, expected: u64, consent: bool) -> Result<QualityState> {
        let gate = self.admit(QualityPhase::Installing, None)?;
        self.check_revision(expected)?;
        if !consent {
            return Err("Review and accept the setup details first.".into());
        }
        let target = self.inner.lock().file.config.clone();
        if target.mode == QualityMode::Alternate {
            super::alternate::require_acknowledged(&self.config_root).map_err(|_| RULES_MESSAGE.to_string())?;
        }
        let wsl_runtime = matches!(target.runtime, QualityRuntime::Podman { .. } | QualityRuntime::DockerWsl { .. });
        if wsl_runtime && super::install::wsl_features_missing(&gate.ctl) {
            gate.say("Enabling the Windows features WSL needs (administrator approval required)");
            match super::install::enable_windows_features(&self.config_root, &gate.ctl) {
                Err(error) => {
                    gate.finish(QualityPhase::Error, Some(error.clone()));
                    return Err(error);
                }
                Ok(outcome) if outcome.needs_restart => return self.setup_restart(gate, &target.runtime, outcome.message),
                Ok(outcome) => gate.say(&outcome.message),
            }
        }
        gate.say(&format!("Preparing {}", target.runtime.label()));
        match super::install::install_runtime(&self.config_root, &target.runtime, &gate.ctl) {
            Err(error) => {
                self.refresh_prerequisites(&target.runtime);
                gate.finish(QualityPhase::Error, Some(error.clone()));
                return Err(error);
            }
            Ok(outcome) if outcome.needs_restart => return self.setup_restart(gate, &target.runtime, outcome.message),
            Ok(outcome) => {
                gate.say(&outcome.message);
                if let Some(connection) = &outcome.podman_connection {
                    gate.say(&format!("Podman connection: {connection}"));
                }
            }
        }
        if let Some(message) = self.ensure_vm_limits(&gate, &target.runtime) {
            self.refresh_prerequisites(&target.runtime);
            gate.finish(QualityPhase::NeedsSetup, Some(message));
            return Ok(self.state());
        }
        self.refresh_prerequisites(&target.runtime);
        self.clear_pending_setup();
        if target.mode == QualityMode::SonarqubeEnterprise {
            gate.finish(QualityPhase::Idle, None);
            self.inner.lock().suspended = false;
            return Ok(self.state());
        }
        self.run_transition(gate, target, None)
    }

    fn setup_restart(self: &Arc<Self>, gate: Gate, runtime: &QualityRuntime, message: String) -> Result<QualityState> {
        self.refresh_prerequisites(runtime);
        if let Err(error) = self.write_pending_setup() {
            gate.finish(QualityPhase::Error, Some(error.clone()));
            return Err(error);
        }
        let text = format!("Windows must restart once to finish enabling WSL. Restart Windows, reopen OMP Manager and press Continue setup. ({message})");
        gate.finish(QualityPhase::NeedsRestart, Some(text));
        Ok(self.state())
    }

    /// The only automatic start path: opening the Code quality view (or launch-start when enabled).
    pub fn enter(self: &Arc<Self>) -> Result<QualityState> {
        self.refresh_ownership()?;
        if self.inner.lock().suspended { return Ok(self.state()); }
        let gate = match self.admit(QualityPhase::Checking, None) {
            Ok(gate) => gate,
            Err(_) => return Ok(self.state()),
        };
        let (config, activated) = {
            let inner = self.inner.lock();
            (inner.file.config.clone(), inner.file.activated)
        };
        let (prerequisites, options) = (self.probe)(&self.config_root, &config.runtime);
        let prerequisite = prerequisites.iter().find(|p| std::mem::discriminant(&p.runtime) == std::mem::discriminant(&config.runtime)).cloned();
        {
            let mut inner = self.inner.lock();
            inner.prerequisites = prerequisites;
            inner.options = options;
        }
        let healthy = activated && self.is_healthy(&config);
        match enter_decision(activated, prerequisite.as_ref(), healthy) {
            EnterDecision::Nothing => Ok(self.state_after(gate)),
            EnterDecision::NeedsSetup(message) => {
                gate.finish(QualityPhase::NeedsSetup, Some(message));
                Ok(self.state())
            }
            EnterDecision::Start => {
                if config.mode == QualityMode::Alternate && super::alternate::require_acknowledged(&self.config_root).is_err() {
                    gate.finish(QualityPhase::NeedsSetup, Some(RULES_MESSAGE.into()));
                    return Ok(self.state());
                }
                gate.phase(QualityPhase::Starting);
                self.reconcile_orphan_scans(&gate);
                self.run_transition(gate, config, None)
            }
        }
    }

    pub fn set_start_at_launch(self: &Arc<Self>, enabled: bool) -> Result<QualityState> {
        let gate = self.admit(QualityPhase::Checking, None)?;
        let mut file = self.inner.lock().file.clone();
        file.start_at_launch = enabled;
        store::save_config(&self.paths, &file)?;
        self.inner.lock().file = file;
        Ok(self.state_after(gate))
    }

    /// App start: only recovers interrupted jobs (files); the analyzer starts here only when the user
    /// enabled "Start code quality when OMP Manager starts", otherwise when Code quality is opened.
    pub fn reconcile_on_start(self: &Arc<Self>) {
        if self.refresh_ownership().is_err() || !self.owns_resources() { return; }
        let launch = {
            let inner = self.inner.lock();
            inner.fatal.is_none() && inner.file.activated && inner.file.start_at_launch
        };
        if !launch {
            return;
        }
        let svc = self.clone();
        let _ = self.spawn_worker("code-quality-launch", move || {
            let _ = svc.enter();
        });
    }

    // ---- transitions ---------------------------------------------------------------------

    fn state_after(self: &Arc<Self>, gate: Gate) -> QualityState {
        drop(gate);
        self.state()
    }

    pub(crate) fn is_healthy(&self, config: &QualityConfig) -> bool {
        let (effective, phase) = {
            let inner = self.inner.lock();
            (inner.effective, inner.phase)
        };
        if effective != Some(config.mode) || matches!(phase, QualityPhase::Error | QualityPhase::NeedsSetup | QualityPhase::NeedsRestart) {
            return false;
        }
        if config.mode == QualityMode::SonarqubeCommunity {
            return SonarClient::new(SonarKind::Community, &sonar::community_url(config.community_port), None)
                .and_then(|c| c.system_status().map_err(String::from))
                .map(|s| s == "UP")
                .unwrap_or(false);
        }
        true
    }

    fn run_transition(self: &Arc<Self>, gate: Gate, target: QualityConfig, credential: Option<QualityCredentialUpdate>) -> Result<QualityState> {
        {
            let mut inner = self.inner.lock();
            inner.requested = Some(target.clone());
        }
        match self.apply(&gate, &target, credential.as_ref()) {
            Ok(Outcome::Ready) => {
                self.inner.lock().suspended = false;
                gate.finish(QualityPhase::Idle, None);
                Ok(self.state())
            }
            Ok(Outcome::Setup(message)) => {
                self.refresh_prerequisites(&target.runtime);
                gate.finish(QualityPhase::NeedsSetup, Some(message));
                Ok(self.state())
            }
            Err(error) => {
                {
                    let mut inner = self.inner.lock();
                    inner.effective = None;
                }
                let started = store::load_resources(&self.paths).ok().and_then(|r| r.runtime(&target.runtime).map(|x| x.possibly_running)).unwrap_or(false);
                if target.mode == QualityMode::SonarqubeCommunity && started {
                    gate.phase(QualityPhase::Stopping);
                    if let Err(stop_error) = self.stop_runtime(&gate, &target.runtime) {
                        gate.finish(QualityPhase::Error, Some(format!("{error} ({stop_error})")));
                        return Err(error);
                    }
                }
                gate.finish(QualityPhase::Error, Some(error.clone()));
                Err(error)
            }
        }
    }

    fn apply(&self, gate: &Gate, target: &QualityConfig, credential: Option<&QualityCredentialUpdate>) -> Result<Outcome> {
        // 1. Validate everything that does not require touching the old server.
        gate.phase(QualityPhase::Validating);
        let enterprise = match credential.filter(|c| c.target == CredentialTarget::Enterprise) {
            Some(update) => Some(self.prepare_enterprise_credential(target, update)?),
            None => None,
        };
        if target.mode == QualityMode::SonarqubeEnterprise {
            self.validate_enterprise_connection(target, enterprise.as_ref())?;
        }
        // 2. Stop everything that must not keep running before another backend starts.
        gate.phase(QualityPhase::Stopping);
        self.inner.lock().effective = None;
        self.stop_for_target(gate, target)?;
        if gate.cancelled() {
            return Err("Cancelled.".into());
        }
        // 3. Start the target.
        let outcome = match target.mode {
            QualityMode::SonarqubeCommunity => self.start_community(gate, target, credential.filter(|c| c.target == CredentialTarget::Community))?,
            QualityMode::SonarqubeEnterprise => Outcome::Ready,
            QualityMode::Alternate => self.start_alternate(gate, target)?,
        };
        if !matches!(outcome, Outcome::Ready) {
            return Ok(outcome);
        }
        // 4. Credentials (validated above), then persist and publish.
        if let Some(prepared) = enterprise {
            self.commit_credential(&prepared)?;
        }
        self.commit(target)?;
        Ok(Outcome::Ready)
    }

    fn commit(&self, target: &QualityConfig) -> Result<()> {
        let mut file = self.inner.lock().file.clone();
        if file.config == *target && file.activated {
            self.inner.lock().suspended = false;
            // Re-confirming the committed selection (opening the view, Start / retry) is not a change.
            self.inner.lock().effective = Some(target.mode);
            self.refresh_credentials();
            return Ok(());
        }
        file.config = target.clone();
        file.revision += 1;
        file.activated = true;
        store::save_config(&self.paths, &file)?;
        {
            let mut inner = self.inner.lock();
            inner.file = file;
            inner.effective = Some(target.mode);
            inner.suspended = false;
        }
        self.refresh_credentials();
        Ok(())
    }

    // ---- credentials -----------------------------------------------------------------------

    fn prepare_enterprise_credential(&self, target: &QualityConfig, update: &QualityCredentialUpdate) -> Result<PreparedCredential> {
        if target.enterprise_url.is_empty() {
            return Err("Enter the Enterprise server URL before saving its token.".into());
        }
        let name = self.enterprise_token_target(&target.enterprise_url);
        match update.action {
            CredentialAction::Clear => Ok(PreparedCredential { target_name: name, token: None }),
            CredentialAction::Replace => {
                let token = update.token.clone().unwrap_or_default();
                valid_token(&token)?;
                SonarClient::new(SonarKind::Enterprise, &target.enterprise_url, Some(&token))?
                    .validate_token()
                    .map_err(String::from)?;
                Ok(PreparedCredential { target_name: name, token: Some(token) })
            }
        }
    }

    fn validate_enterprise_connection(&self, target: &QualityConfig, prepared: Option<&PreparedCredential>) -> Result<()> {
        if let Some(prepared) = prepared {
            if prepared.token.is_none() {
                return Ok(()); // explicit clear: connection stays selected, scans are blocked until a token exists
            }
            return Ok(()); // validated while preparing
        }
        let token = self
            .secrets
            .read(&self.enterprise_token_target(&target.enterprise_url))?
            .ok_or_else(|| "Enterprise needs a user token: log in through SSO in your browser, then My Account → Security → create a user token, and save it here.".to_string())?;
        SonarClient::new(SonarKind::Enterprise, &target.enterprise_url, Some(&token))?
            .validate_token()
            .map_err(String::from)
    }

    fn commit_credential(&self, prepared: &PreparedCredential) -> Result<()> {
        match &prepared.token {
            Some(token) => self.secrets.write(&prepared.target_name, token),
            None => self.secrets.delete(&prepared.target_name),
        }
    }

    /// Token-only change on an unchanged healthy configuration: no restart.
    fn credential_only(self: &Arc<Self>, gate: Gate, config: &QualityConfig, update: &QualityCredentialUpdate) -> Result<QualityState> {
        gate.phase(QualityPhase::Validating);
        let outcome: Result<()> = (|| match update.target {
            CredentialTarget::Enterprise => {
                let prepared = self.prepare_enterprise_credential(config, update)?;
                self.commit_credential(&prepared)
            }
            CredentialTarget::Community => {
                let name = self
                    .community_token_target(&config.runtime)
                    .ok_or_else(|| "Start the local SonarQube server once before saving its token.".to_string())?;
                match update.action {
                    CredentialAction::Clear => self.secrets.delete(&name),
                    CredentialAction::Replace => {
                        let token = update.token.clone().unwrap_or_default();
                        valid_token(&token)?;
                        if config.mode == QualityMode::SonarqubeCommunity {
                            SonarClient::new(SonarKind::Community, &sonar::community_url(config.community_port), Some(&token))?
                                .validate_token()
                                .map_err(String::from)?;
                        }
                        self.secrets.write(&name, &token)
                    }
                }
            }
        })();
        match outcome {
            Ok(()) => {
                self.refresh_credentials();
                Ok(self.state_after(gate))
            }
            Err(error) => {
                drop(gate);
                Err(error)
            }
        }
    }

    // ---- stopping --------------------------------------------------------------------------

    fn stop_for_target(&self, gate: &Gate, target: &QualityConfig) -> Result<()> {
        let resources = store::load_resources(&self.paths)?;
        let keep = (target.mode == QualityMode::SonarqubeCommunity).then(|| target.runtime.clone());
        for record in resources.runtimes.iter().filter(|r| r.possibly_running) {
            if Some(&record.runtime) == keep.as_ref() && !self.is_unresolved(&record.runtime) {
                continue;
            }
            self.stop_runtime(gate, &record.runtime)?;
        }
        Ok(())
    }

    /// Stops (and verifies) every app-owned server container on `runtime`: Sonar first, then PostgreSQL.
    pub(crate) fn stop_runtime(&self, gate: &Gate, runtime: &QualityRuntime) -> Result<()> {
        let engine = self.engine(runtime).map_err(|e| {
            self.mark_unresolved(runtime, e.clone());
            e
        })?;
        if let Err(e) = engine.probe(&gate.ctl) {
            let message = format!("Cannot reach {} to confirm the managed SonarQube server stopped: {e}", runtime.label());
            self.mark_unresolved(runtime, message.clone());
            return Err(message);
        }
        let owner = self.installation_id();
        if owner.is_empty() { return Err("The managed installation identity is missing; no containers were stopped.".into()); }
        let owned = engine.list_owned(&owner, &gate.ctl).map_err(|e| {
            let message = format!("Could not list managed containers on {}: {e}", runtime.label());
            self.mark_unresolved(runtime, message.clone());
            message
        })?;
        for role in [super::runtime::ROLE_SCAN, ROLE_SONAR, ROLE_DB] {
            for container in owned.iter().filter(|c| c.running && c.labels.get(ROLE_LABEL).map(String::as_str) == Some(role)) {
                let Some(current) = engine.inspect(&container.id, &gate.ctl).map_err(|error| {
                    let message = format!("{} could not be inspected before stopping: {error}", container.name);
                    self.mark_unresolved(runtime, message.clone());
                    message
                })? else { continue };
                if current.labels.get(OWNER_LABEL).map(String::as_str) != Some(owner.as_str())
                    || current.labels.get(ROLE_LABEL).map(String::as_str) != Some(role) {
                    let message = format!("{} no longer has the expected managed ownership labels; it was left running.", container.name);
                    self.mark_unresolved(runtime, message.clone());
                    return Err(message);
                }
                gate.say(&format!("Stopping {}", container.name));
                let stopped = engine.stop(&container.id, 30, &gate.ctl).and_then(|_| match engine.inspect(&container.id, &gate.ctl)? {
                    Some(info) if info.running => Err(format!("{} is still running", container.name)),
                    _ => Ok(()),
                });
                if let Err(e) = stopped {
                    let message = format!("{} could not be stopped on {}: {e}", container.name, runtime.label());
                    self.mark_unresolved(runtime, message.clone());
                    return Err(message);
                }
            }
        }
        let remaining = engine.list_owned(&owner, &gate.ctl).map_err(|error| {
            let message = format!("Managed containers could not be verified stopped: {error}");
            self.mark_unresolved(runtime, message.clone());
            message
        })?;
        if let Some(container) = remaining.iter().find(|container| container.running) {
            let message = format!("{} is still running with a managed ownership label; shutdown was not confirmed.", container.name);
            self.mark_unresolved(runtime, message.clone());
            return Err(message);
        }
        let port = store::load_resources(&self.paths)?.runtime(runtime).map(|r| r.port).unwrap_or(0);
        self.set_possibly_running(runtime, None, port, false)?;
        self.clear_unresolved(runtime);
        let mut inner = self.inner.lock();
        if &inner.file.config.runtime == runtime { inner.community.running = Some(false); }
        Ok(())
    }

    /// Stop all recorded active runtimes without starting any daemon to reach them.
    pub(crate) fn stop_recorded_servers(&self, gate: &Gate) -> Result<()> {
        let resources = store::load_resources(&self.paths)?;
        let mut errors = Vec::new();
        for record in resources.runtimes.iter().filter(|r| r.possibly_running || self.is_unresolved(&r.runtime)) {
            if let Err(error) = self.stop_runtime(gate, &record.runtime) { errors.push(error); }
        }
        if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
    }

    pub fn stop_server(self: &Arc<Self>) -> Result<QualityState> {
        let gate = self.admit(QualityPhase::Stopping, None)?;
        let runtime = {
            let mut inner = self.inner.lock();
            inner.suspended = true;
            inner.effective = None;
            inner.file.config.runtime.clone()
        };
        let outcome = self.verify_pending_tasks_stopped().and_then(|_| self.stop_recorded_servers(&gate)).and_then(|_| {
            super::install::stop_owned_podman_machine(&self.config_root, &runtime, &self.installation_id(), &gate.ctl)
        });
        {
            let mut inner = self.inner.lock();
            for prerequisite in &mut inner.prerequisites {
                if prerequisite.runtime == runtime { prerequisite.ready = false; }
            }
            if outcome.is_ok() { inner.community.running = Some(false); }
        }
        match outcome {
            Ok(()) => { gate.finish(QualityPhase::Idle, None); Ok(self.state()) }
            Err(error) => { gate.finish(QualityPhase::Error, Some(error.clone())); Err(error) }
        }
    }

    /// Exit path: best-effort stop of every owned server; unconfirmed stops stay recorded.
    pub(crate) fn stop_all_on_exit(self: &Arc<Self>) {
        let Ok(gate_phase) = self.admit_internal(QualityPhase::Stopping, None, false, true) else { return };
        let _ = self.stop_recorded_servers(&gate_phase);
    }


    // ---- alternate backend -----------------------------------------------------------------

    fn start_alternate(&self, gate: &Gate, target: &QualityConfig) -> Result<Outcome> {
        let engine = match self.engine(&target.runtime) {
            Ok(engine) => engine,
            Err(message) => return Ok(Outcome::Setup(message)),
        };
        gate.phase(QualityPhase::Starting);
        if let Err(message) = engine.ensure_started(&gate.ctl) {
            return Ok(Outcome::Setup(message));
        }
        if let Err(message) = engine.probe(&gate.ctl) {
            return Ok(Outcome::Setup(message));
        }
        self.record_ready_runtime(&target.runtime);
        gate.phase(QualityPhase::Pulling);
        super::alternate::prepare_alternate(engine.as_ref(), &self.config_root, &gate.ctl)?;
        Ok(Outcome::Ready)
    }

    // ---- community ------------------------------------------------------------------------

    fn start_community(&self, gate: &Gate, target: &QualityConfig, credential: Option<&QualityCredentialUpdate>) -> Result<Outcome> {
        let runtime = &target.runtime;
        let engine = match self.engine(runtime) {
            Ok(engine) => engine,
            Err(message) => return Ok(Outcome::Setup(message)),
        };
        gate.phase(QualityPhase::Starting);
        if let Err(message) = engine.ensure_started(&gate.ctl) {
            return Ok(Outcome::Setup(message));
        }
        let probe = match engine.probe(&gate.ctl) {
            Ok(probe) => probe,
            Err(message) => return Ok(Outcome::Setup(message)),
        };
        self.record_ready_runtime(runtime);
        if let Some(value) = probe.max_map_count.filter(|v| *v < MIN_MAP_COUNT) {
            return Ok(Outcome::Setup(format!(
                "SonarQube's search engine needs vm.max_map_count >= {MIN_MAP_COUNT} but {} has {value}. Use Install runtime to raise it (Podman/WSL), or follow Docker's Desktop instructions.",
                runtime.label()
            )));
        }
        if let Some(value) = probe.file_max.filter(|v| *v < MIN_FILE_MAX) {
            return Ok(Outcome::Setup(format!(
                "SonarQube's search engine needs fs.file-max >= {MIN_FILE_MAX} but {} has {value}. Use Install runtime to raise it.",
                runtime.label()
            )));
        }
        if probe.memory_bytes > 0 && probe.memory_bytes < MIN_ENGINE_MEMORY {
            return Ok(Outcome::Setup(format!(
                "SonarQube needs about 5 GiB of memory in the container engine but {} offers {:.1} GiB. Increase the engine/VM memory (the app never changes it).",
                runtime.label(),
                probe.memory_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
            )));
        }
        self.record_daemon(runtime, &probe.fingerprint, target.community_port, engine.as_ref(), gate)?;
        let images = scanners::images()?;
        for image in [&images.postgres, &images.sonarqube_community] {
            if !engine.image_exists(image, &gate.ctl)? {
                gate.phase(QualityPhase::Pulling);
                gate.say(&format!("Pulling {image}"));
                engine.pull(image, &gate.ctl)?;
            }
        }
        gate.phase(QualityPhase::Starting);
        self.set_possibly_running(runtime, Some(&probe.fingerprint), target.community_port, true)?;
        let set = self.resource_set_id(runtime).ok_or("The container engine identity could not be recorded.")?;
        self.ensure_servers(gate, engine.as_ref(), target, &set, &images.postgres, &images.sonarqube_community)?;
        let url = sonar::community_url(target.community_port);
        gate.phase(QualityPhase::WaitingForReady);
        self.wait_for_sonar(gate, &url)?;
        if let Some(update) = credential {
            let name = self.community_token_target(runtime).ok_or("The container engine identity was not recorded.")?;
            match update.action {
                CredentialAction::Clear => self.secrets.delete(&name)?,
                CredentialAction::Replace => {
                    let token = update.token.clone().unwrap_or_default();
                    valid_token(&token)?;
                    gate.phase(QualityPhase::Validating);
                    SonarClient::new(SonarKind::Community, &url, Some(&token))?.validate_token().map_err(String::from)?;
                    self.secrets.write(&name, &token)?;
                }
            }
        } else {
            self.bootstrap_sonar_token(gate, runtime, &url, &set);
        }
        {
            let mut inner = self.inner.lock();
            inner.community = CommunityState { running: Some(true), url: Some(url), runtime_id: Some(set) };
        }
        Ok(Outcome::Ready)
    }

    /// Fresh app-owned server only: change the default admin password to a random one and create a scan
    /// token, both kept in the operating system credential store. Never fails the start; problems become log lines.
    fn bootstrap_sonar_token(&self, gate: &Gate, runtime: &QualityRuntime, url: &str, set: &str) {
        let Some(token_target) = self.community_token_target(runtime) else { return };
        if matches!(self.secrets.read(&token_target), Ok(Some(_))) {
            return;
        }
        let admin_target = super::secrets::target_name(&self.config_root, &format!("community:{set}"), "adminpassword");
        let password = match self.secrets.read(&admin_target) {
            Ok(Some(password)) => password,
            Ok(None) => {
                let Ok(default_login) = SonarClient::with_login(url, "admin", "admin") else { return };
                if !default_login.login_valid() {
                    gate.say("SonarQube's admin account was already changed outside OMP Manager; create a token manually (Open SonarQube setup).");
                    return;
                }
                let new = random_sonar_password();
                // Persist first: a crash between the change and the save must not lose the only password.
                if let Err(error) = self.secrets.write(&admin_target, &new) {
                    gate.say(&format!("Could not save the SonarQube admin password, so it was left unchanged: {error}"));
                    return;
                }
                if let Err(error) = default_login.change_password("admin", "admin", &new) {
                    let _ = self.secrets.delete(&admin_target);
                    gate.say(&format!("Could not change the SonarQube admin password: {error}"));
                    return;
                }
                new
            }
            Err(error) => {
                gate.say(&format!("Could not read the saved SonarQube admin password: {error}"));
                return;
            }
        };
        let created: Result<String> = (|| {
            let admin = SonarClient::with_login(url, "admin", &password)?;
            let name = format!("omp-manager-{}", &set[..set.len().min(8)]);
            let token = match admin.generate_user_token(&name) {
                Ok(token) => token,
                Err(_) => {
                    let _ = admin.revoke_user_token(&name);
                    admin.generate_user_token(&name).map_err(String::from)?
                }
            };
            SonarClient::new(SonarKind::Community, url, Some(&token))?.validate_token().map_err(String::from)?;
            Ok(token)
        })();
        match created {
            Ok(token) => match self.secrets.write(&token_target, &token) {
                Ok(()) => {
                    self.refresh_credentials();
                    gate.say("Created the SonarQube scan token and saved it in the operating system credential store.");
                }
                Err(error) => gate.say(&format!("Could not save the SonarQube scan token: {error}")),
            },
            Err(error) => gate.say(&format!("Automatic SonarQube token setup failed: {error}. Create a token manually (Open SonarQube setup).")),
        }
    }

    /// Remember the daemon's identity; a replaced daemon invalidates its old container records.
    fn record_daemon(&self, runtime: &QualityRuntime, fingerprint: &str, port: u16, _engine: &dyn Engine, gate: &Gate) -> Result<()> {
        let mut resources = store::load_resources(&self.paths)?;
        match resources.runtime_mut(runtime) {
            Some(record) if record.fingerprint == fingerprint => {
                record.port = port;
            }
            Some(record) => {
                if !record.fingerprint.is_empty() {
                    gate.say("The container engine was replaced; its previous managed containers no longer exist.");
                }
                record.fingerprint = fingerprint.to_string();
                record.port = port;
                resources.containers.retain(|c| &c.runtime != runtime);
            }
            None => resources.runtimes.push(RuntimeRecord { runtime: runtime.clone(), fingerprint: fingerprint.to_string(), possibly_running: false, port }),
        }
        store::save_resources(&self.paths, &resources)
    }

    fn ensure_servers(&self, gate: &Gate, engine: &dyn Engine, target: &QualityConfig, set: &str, postgres: &str, sonarqube: &str) -> Result<()> {
        let installation = self.installation_id();
        let n = names(&installation);
        let owner = vec![(OWNER_LABEL.to_string(), installation.clone())];
        engine.network_create(&n.network, &owner, &gate.ctl)?;
        for volume in [&n.vol_db, &n.vol_data, &n.vol_logs, &n.vol_ext, &n.vol_temp] {
            engine.volume_create(volume, &owner, &gate.ctl)?;
        }
        let password_target = self.db_password_target(&target.runtime, set);
        let password = match self.secrets.read(&password_target)? {
            Some(p) => p,
            None => {
                let p = random_password();
                self.secrets.write(&password_target, &p)?;
                p
            }
        };
        let port = target.community_port;
        let db_hash = spec_hash(&[postgres, "db", "512m", "1"]);
        let sonar_hash = spec_hash(&[sonarqube, "sonar", &port.to_string(), "4g", "2", postgres]);
        let labels = |role: &str, hash: &str| {
            vec![
                (OWNER_LABEL.to_string(), installation.clone()),
                (ROLE_LABEL.to_string(), role.to_string()),
                (SPEC_LABEL.to_string(), hash.to_string()),
            ]
        };
        let db_spec = ContainerSpec {
            name: n.db.clone(),
            image: postgres.to_string(),
            labels: labels(ROLE_DB, &db_hash),
            env: vec![("POSTGRES_USER".into(), "sonar".into()), ("POSTGRES_DB".into(), "sonar".into())],
            secret_env: vec![("POSTGRES_PASSWORD".into(), password.clone())],
            network: Some(n.network.clone()),
            network_aliases: vec![DB_ALIAS.to_string()],
            cpus: Some(1.0),
            memory: Some("512m".into()),
            volumes: vec![(n.vol_db.clone(), "/var/lib/postgresql/data".into())],
            ..Default::default()
        };
        let sonar_spec = ContainerSpec {
            name: n.sonar.clone(),
            image: sonarqube.to_string(),
            labels: labels(ROLE_SONAR, &sonar_hash),
            env: vec![
                ("SONAR_JDBC_URL".into(), format!("jdbc:postgresql://{DB_ALIAS}:5432/sonar")),
                ("SONAR_JDBC_USERNAME".into(), "sonar".into()),
            ],
            secret_env: vec![("SONAR_JDBC_PASSWORD".into(), password)],
            network: Some(n.network.clone()),
            network_aliases: vec![SONAR_ALIAS.to_string()],
            publish: Some(Publish { host_ip: "127.0.0.1".into(), host_port: port, container_port: 9000 }),
            cpus: Some(2.0),
            memory: Some("4g".into()),
            volumes: vec![
                (n.vol_data.clone(), "/opt/sonarqube/data".into()),
                (n.vol_logs.clone(), "/opt/sonarqube/logs".into()),
                (n.vol_ext.clone(), "/opt/sonarqube/extensions".into()),
                (n.vol_temp.clone(), "/opt/sonarqube/temp".into()),
            ],
            ulimits: vec!["nofile=131072:131072".into(), "nproc=8192:8192".into()],
            ..Default::default()
        };
        let db_id = self.ensure_container(gate, engine, &target.runtime, &installation, &db_spec, &db_hash)?;
        self.wait_for_database(gate, engine, &db_id)?;
        self.ensure_container(gate, engine, &target.runtime, &installation, &sonar_spec, &sonar_hash)?;
        Ok(())
    }

    /// Reuse the recorded/labelled container when its spec matches and start it; otherwise replace it.
    fn ensure_container(&self, gate: &Gate, engine: &dyn Engine, runtime: &QualityRuntime, installation: &str, spec: &ContainerSpec, hash: &str) -> Result<String> {
        let ctl = &gate.ctl;
        let role = spec.labels.iter().find(|(k, _)| k == ROLE_LABEL).map(|(_, v)| v.clone()).unwrap_or_default();
        let existing = engine
            .list_owned(installation, ctl)?
            .into_iter()
            .find(|c| c.labels.get(ROLE_LABEL).map(String::as_str) == Some(role.as_str()));
        let mut id = None;
        if let Some(info) = existing {
            if info.labels.get(SPEC_LABEL).map(String::as_str) == Some(hash) {
                id = Some(info.id.clone());
                self.record_container(runtime, &info.id, &info.name, &role, hash)?;
                if !info.running {
                    self.start_container(gate, engine, &info.id, &info.name)?;
                }
            } else {
                gate.say(&format!("Replacing {} (settings changed; volumes are kept)", info.name));
                if info.running {
                    engine.stop(&info.id, 30, ctl)?;
                }
                engine.remove(&info.id, false, ctl)?;
                self.forget_container(&info.id)?;
            }
        }
        match id {
            Some(id) => Ok(id),
            None => {
                gate.say(&format!("Creating {}", spec.name));
                let created = engine.create(spec, ctl)?;
                self.record_container(runtime, &created, &spec.name, &role, hash)?;
                self.start_container(gate, engine, &created, &spec.name)?;
                Ok(created)
            }
        }
    }

    fn start_container(&self, gate: &Gate, engine: &dyn Engine, id: &str, name: &str) -> Result<()> {
        gate.say(&format!("Starting {name}"));
        engine.start(id, &gate.ctl).map_err(|e| {
            if port_conflict(&e) {
                "The SonarQube port is already in use on this PC. Choose another port in the code quality settings (the app never stops other programs).".to_string()
            } else {
                e
            }
        })
    }

    fn record_container(&self, runtime: &QualityRuntime, id: &str, name: &str, role: &str, hash: &str) -> Result<()> {
        let mut resources = store::load_resources(&self.paths)?;
        let fingerprint = resources.runtime(runtime).map(|r| r.fingerprint.clone()).unwrap_or_default();
        resources.containers.retain(|c| !(c.role == role && &c.runtime == runtime));
        resources.containers.push(ResourceRecord {
            id: id.to_string(),
            name: name.to_string(),
            role: role.to_string(),
            runtime: runtime.clone(),
            fingerprint,
            created_at: now_millis(),
            spec_hash: hash.to_string(),
        });
        store::save_resources(&self.paths, &resources)
    }

    fn forget_container(&self, id: &str) -> Result<()> {
        let mut resources = store::load_resources(&self.paths)?;
        resources.containers.retain(|c| c.id != id);
        store::save_resources(&self.paths, &resources)
    }

    fn wait_for_database(&self, gate: &Gate, engine: &dyn Engine, id: &str) -> Result<()> {
        gate.phase(QualityPhase::WaitingForReady);
        let deadline = Instant::now() + DB_READY_TIMEOUT;
        let argv: Vec<String> = ["pg_isready", "-U", "sonar", "-d", "sonar"].iter().map(|s| s.to_string()).collect();
        loop {
            if gate.cancelled() {
                return Err("Cancelled.".into());
            }
            if let Ok(output) = engine.exec(id, &argv, &[], 15, &gate.ctl) {
                if output.exit_code == Some(0) {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                let tail = engine.logs(id, 20, &gate.ctl).unwrap_or_default();
                return Err(format!("PostgreSQL did not become ready within {} s. {tail}", DB_READY_TIMEOUT.as_secs()));
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    fn wait_for_sonar(&self, gate: &Gate, url: &str) -> Result<()> {
        let client = SonarClient::new(SonarKind::Community, url, None)?;
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut last = String::new();
        loop {
            if gate.cancelled() {
                return Err("Cancelled.".into());
            }
            match client.system_status() {
                Ok(status) if status == "UP" => return Ok(()),
                Ok(status) => {
                    if status != last {
                        gate.say(&format!("SonarQube status: {status}"));
                        last = status;
                    }
                }
                Err(error) => {
                    let text = error.to_string();
                    if text != last {
                        gate.say(&format!("Waiting for SonarQube: {text}"));
                        last = text;
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "SonarQube did not report UP within {} minutes (last: {last}). Check the container logs and the engine's memory.",
                    READY_TIMEOUT.as_secs() / 60
                ));
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    }

    /// Reuse the successful engine probe rather than leaving shutdown's cached "not ready"
    /// status in place or probing every unrelated runtime again.
    fn record_ready_runtime(&self, runtime: &QualityRuntime) {
        let mut inner = self.inner.lock();
        if let Some(prerequisite) = inner.prerequisites.iter_mut().find(|item| &item.runtime == runtime) {
            prerequisite.installed = true;
            prerequisite.ready = true;
            prerequisite.needs_restart = false;
            prerequisite.message = None;
        } else {
            inner.prerequisites.push(Prerequisite {
                runtime: runtime.clone(), installed: true, ready: true,
                needs_restart: false, message: None,
            });
        }
    }

    fn refresh_prerequisites(&self, runtime: &QualityRuntime) {
        let (prerequisites, options) = (self.probe)(&self.config_root, runtime);
        let mut inner = self.inner.lock();
        inner.prerequisites = prerequisites;
        inner.options = options;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EnterDecision {
    Nothing,
    NeedsSetup(String),
    Start,
}

/// What opening the Code quality view must do (pure so it can be tested without engines).
pub(crate) fn enter_decision(activated: bool, prerequisite: Option<&Prerequisite>, healthy: bool) -> EnterDecision {
    if !activated {
        return EnterDecision::Nothing;
    }
    if let Some(p) = prerequisite.filter(|p| !p.installed) {
        return EnterDecision::NeedsSetup(p.message.clone().unwrap_or_else(|| "The container runtime is not installed.".into()));
    }
    if healthy {
        return EnterDecision::Nothing;
    }
    EnterDecision::Start
}

pub(crate) struct PreparedCredential {
    pub target_name: String,
    pub token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation_normalizes_and_rejects() {
        let mut config = QualityConfig::default();
        config.community_port = 80;
        assert!(validate_config(&config).is_err());
        config.community_port = 9000;
        config.mode = QualityMode::SonarqubeEnterprise;
        assert!(validate_config(&config).is_err(), "enterprise requires a URL");
        config.enterprise_url = "http://sonar.example".into();
        assert!(validate_config(&config).is_err(), "enterprise must be https");
        config.enterprise_url = "https://SONAR.example/team/".into();
        assert_eq!(validate_config(&config).unwrap().enterprise_url, "https://sonar.example/team");
        config.runtime = QualityRuntime::DockerWsl { distro: " ".into() };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn names_are_namespaced_by_installation() {
        let a = names("11111111-1111-1111-1111-111111111111");
        let b = names("22222222-2222-2222-2222-222222222222");
        assert_ne!(a.sonar, b.sonar);
        assert!(a.sonar.starts_with("omp-q-") && a.sonar.ends_with("-sonar"));
    }

    #[test]
    fn port_conflicts_are_recognized() {
        assert!(port_conflict("Error: bind: address already in use"));
        assert!(port_conflict("Bind for 127.0.0.1:9000 failed: port is already allocated"));
        assert!(!port_conflict("no such image"));
    }
}
