//! Service-level behavior with a deterministic fake container engine and a loopback fake SonarQube.
use super::process_lock::ProcessLock;
use super::runtime::*;
use super::secrets::MemorySecrets;
use super::*;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

#[derive(Default)]
struct FakeState {
    containers: Vec<ContainerInfo>,
    ops: Vec<String>,
    next: u32,
}

pub struct FakeEngine {
    runtime: QualityRuntime,
    st: Mutex<FakeState>,
    reachable: AtomicBool,
    fail_stop: AtomicBool,
    start_delay_ms: AtomicU64,
}

impl FakeEngine {
    fn new(runtime: QualityRuntime) -> Arc<Self> {
        Arc::new(FakeEngine {
            runtime,
            st: Mutex::new(FakeState::default()),
            reachable: AtomicBool::new(true),
            fail_stop: AtomicBool::new(false),
            start_delay_ms: AtomicU64::new(0),
        })
    }
    fn ops(&self) -> Vec<String> {
        self.st.lock().ops.clone()
    }
    fn count(&self, prefix: &str) -> usize {
        self.st.lock().ops.iter().filter(|o| o.starts_with(prefix)).count()
    }
    fn running(&self) -> usize {
        self.st.lock().containers.iter().filter(|c| c.running).count()
    }
}

impl Engine for FakeEngine {
    fn runtime(&self) -> &QualityRuntime {
        &self.runtime
    }
    fn probe(&self, _: &Ctl) -> Result<EngineProbe> {
        if !self.reachable.load(Ordering::Relaxed) {
            return Err("engine is not running".into());
        }
        Ok(EngineProbe { fingerprint: format!("fp-{}", self.runtime.key()), os: "linux".into(), memory_bytes: 8 << 30, max_map_count: Some(1 << 20), file_max: Some(1 << 20),
            #[cfg(unix)]
            cpu_limits_unsupported: None,
        })
    }
    fn ensure_started(&self, ctl: &Ctl) -> Result<()> {
        self.probe(ctl).map(|_| ())
    }
    fn image_exists(&self, _: &str, _: &Ctl) -> Result<bool> {
        Ok(true)
    }
    fn pull(&self, image: &str, _: &Ctl) -> Result<()> {
        self.st.lock().ops.push(format!("pull {image}"));
        Ok(())
    }
    fn build_image(&self, tag: &str, _: &Path, _: &Ctl) -> Result<()> {
        self.st.lock().ops.push(format!("build {tag}"));
        Ok(())
    }
    fn network_create(&self, name: &str, _: &[(String, String)], _: &Ctl) -> Result<()> {
        self.st.lock().ops.push(format!("network {name}"));
        Ok(())
    }
    fn volume_create(&self, name: &str, _: &[(String, String)], _: &Ctl) -> Result<()> {
        self.st.lock().ops.push(format!("volume {name}"));
        Ok(())
    }
    fn create(&self, spec: &ContainerSpec, _: &Ctl) -> Result<String> {
        let mut st = self.st.lock();
        st.next += 1;
        let id = format!("c{:04}", st.next);
        st.ops.push(format!("create {}", spec.name));
        st.containers.push(ContainerInfo {
            id: id.clone(),
            name: spec.name.clone(),
            image: spec.image.clone(),
            running: false,
            status: "created".into(),
            exit_code: None,
            labels: spec.labels.iter().cloned().collect::<HashMap<_, _>>(),
        });
        Ok(id)
    }
    fn inspect(&self, id: &str, _: &Ctl) -> Result<Option<ContainerInfo>> {
        Ok(self.st.lock().containers.iter().find(|c| c.id == id).cloned())
    }
    fn list_owned(&self, owner: &str, _: &Ctl) -> Result<Vec<ContainerInfo>> {
        Ok(self.st.lock().containers.iter().filter(|c| c.labels.get(OWNER_LABEL).map(String::as_str) == Some(owner)).cloned().collect())
    }
    fn start(&self, id: &str, _: &Ctl) -> Result<()> {
        std::thread::sleep(Duration::from_millis(self.start_delay_ms.load(Ordering::Relaxed)));
        let mut st = self.st.lock();
        let name = st.containers.iter().find(|c| c.id == id).map(|c| c.name.clone()).ok_or("no such container")?;
        st.ops.push(format!("start {name}"));
        st.containers.iter_mut().find(|c| c.id == id).expect("found").running = true;
        Ok(())
    }
    fn stop(&self, id: &str, _: u32, _: &Ctl) -> Result<()> {
        if self.fail_stop.load(Ordering::Relaxed) {
            return Err("daemon refused to stop".into());
        }
        let mut st = self.st.lock();
        let name = st.containers.iter().find(|c| c.id == id).map(|c| c.name.clone()).ok_or("no such container")?;
        st.ops.push(format!("stop {name}"));
        st.containers.iter_mut().find(|c| c.id == id).expect("found").running = false;
        Ok(())
    }
    fn wait(&self, _: &str, _: u64, _: &Ctl) -> Result<i64> {
        Ok(0)
    }
    fn copy_in(&self, _: &str, _: &str, _: &Path, _: &Ctl) -> Result<()> {
        Ok(())
    }
    fn copy_out(&self, _: &str, _: &str, _: &Path, _: &Ctl) -> Result<()> {
        Ok(())
    }
    fn remove(&self, id: &str, _: bool, _: &Ctl) -> Result<()> {
        let mut st = self.st.lock();
        st.ops.push(format!("remove {id}"));
        st.containers.retain(|c| c.id != id);
        Ok(())
    }
    fn exec(&self, _: &str, _: &[String], _: &[(String, String)], _: u64, _: &Ctl) -> Result<ExecOutput> {
        Ok(ExecOutput { exit_code: Some(0), ..Default::default() })
    }
    fn logs(&self, _: &str, _: u32, _: &Ctl) -> Result<String> {
        Ok(String::new())
    }
}

struct FakeFactory(HashMap<String, Arc<FakeEngine>>);

impl EngineFactory for FakeFactory {
    fn engine(&self, runtime: &QualityRuntime) -> Result<Arc<dyn Engine>> {
        self.0.get(&runtime.key()).cloned().map(|e| e as Arc<dyn Engine>).ok_or_else(|| "unknown runtime".to_string())
    }
}

/// What the loopback SonarQube saw and how it behaves (default admin account, token generation).
#[derive(Default)]
struct FakeSonar {
    password_changed: AtomicBool,
    fail_change: AtomicBool,
    calls: Mutex<Vec<String>>,
}

impl FakeSonar {
    fn count(&self, needle: &str) -> usize {
        self.calls.lock().iter().filter(|c| c.contains(needle)).count()
    }
}

/// Loopback SonarQube: `UP` status, `bad` tokens invalid, the default `admin:admin` login valid until its
/// password is changed, then token generation for the admin login.
fn fake_sonar() -> (u16, Arc<FakeSonar>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(FakeSonar::default());
    let shared = state.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let state = shared.clone();
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 2048];
                loop {
                    let n = stream.read(&mut buffer).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end].lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8_lossy(&bytes).to_string();
                state.calls.lock().push(request.lines().next().unwrap_or_default().to_string());
                let admin = request.contains("Basic YWRtaW46"); // base64("admin:")
                let default_admin = request.contains("YWRtaW46YWRtaW4="); // base64("admin:admin")
                let changed = state.password_changed.load(Ordering::Relaxed);
                let (status, body) = if request.contains("/api/system/status") {
                    (200, r#"{"id":"x","version":"26.9","status":"UP"}"#.to_string())
                } else if request.contains("/api/authentication/validate") {
                    let valid = if default_admin { !changed } else { !request.contains("YmFkOg==") }; // base64("bad:")
                    (200, format!(r#"{{"valid":{valid}}}"#))
                } else if request.contains("/api/users/change_password") {
                    if state.fail_change.load(Ordering::Relaxed) || !request.contains("previousPassword=admin") || changed {
                        (400, r#"{"errors":[{"msg":"rejected"}]}"#.to_string())
                    } else {
                        state.password_changed.store(true, Ordering::Relaxed);
                        (200, "{}".to_string())
                    }
                } else if request.contains("/api/user_tokens/generate") {
                    if admin && changed && !default_admin {
                        (200, r#"{"login":"admin","name":"omp","token":"squ_boot","type":"USER_TOKEN"}"#.to_string())
                    } else {
                        (401, r#"{"errors":[{"msg":"unauthorized"}]}"#.to_string())
                    }
                } else {
                    (200, "{}".to_string())
                };
                let reason = match status { 200 => "OK", 400 => "Bad Request", _ => "Unauthorized" };
                let _ = write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            });
        }
    });
    (port, state)
}

/// Prerequisite probe that reports the selected runtime as installed (or not) without touching the machine.
fn ready_probe(installed: Arc<AtomicBool>) -> ProbeFn {
    Arc::new(move |_, selected| {
        let yes = installed.load(Ordering::Relaxed);
        (
            vec![Prerequisite { runtime: selected.clone(), installed: yes, ready: yes, needs_restart: false, message: (!yes).then(|| "Not installed in this test.".to_string()) }],
            QualityOptions::default(),
        )
    })
}

struct Harness {
    svc: Arc<CodeQualityService>,
    desktop: Arc<FakeEngine>,
    podman: Arc<FakeEngine>,
    sonar: Arc<FakeSonar>,
    secrets: Arc<MemorySecrets>,
    installed: Arc<AtomicBool>,
    dir: PathBuf,
    port: u16,
}

impl Harness {
    fn config(&self, runtime: QualityRuntime) -> QualityConfig {
        QualityConfig { runtime, community_port: self.port, ..QualityConfig::default() }
    }
    /// A new service over the same config folder and secrets with fresh fake engines (an app restart).
    fn restart(&self) -> (Arc<CodeQualityService>, Arc<FakeEngine>, Arc<FakeEngine>) {
        let desktop = FakeEngine::new(QualityRuntime::DockerDesktop);
        let podman = FakeEngine::new(QualityRuntime::Podman { connection: "fake".into() });
        let mut map = HashMap::new();
        map.insert(desktop.runtime.key(), desktop.clone());
        map.insert(podman.runtime.key(), podman.clone());
        let svc = CodeQualityService::with_parts(self.dir.clone(), Arc::new(FakeFactory(map)), self.secrets.clone(), Ok(Some(ProcessLock::unowned())), ready_probe(self.installed.clone()));
        (svc, desktop, podman)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn harness() -> Harness {
    let dir = std::env::temp_dir().join(format!("omp-cq-svc-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let desktop = FakeEngine::new(QualityRuntime::DockerDesktop);
    let podman = FakeEngine::new(QualityRuntime::Podman { connection: "fake".into() });
    let mut map = HashMap::new();
    map.insert(desktop.runtime.key(), desktop.clone());
    map.insert(podman.runtime.key(), podman.clone());
    let secrets = Arc::new(MemorySecrets::default());
    let installed = Arc::new(AtomicBool::new(true));
    let svc = CodeQualityService::with_parts(dir.clone(), Arc::new(FakeFactory(map)), secrets.clone(), Ok(Some(ProcessLock::unowned())), ready_probe(installed.clone()));
    let (port, sonar) = fake_sonar();
    Harness { svc, desktop, podman, sonar, secrets, installed, dir, port }
}

#[test]
fn fresh_state_defaults_without_side_effects() {
    let h = harness();
    let state = h.svc.state();
    assert_eq!(state.config.mode, QualityMode::SonarqubeCommunity);
    assert_eq!(state.config.runtime, QualityRuntime::Podman { connection: String::new() });
    assert!(!state.activated && state.effective_mode.is_none() && !state.busy);
    assert!(h.desktop.ops().is_empty() && h.podman.ops().is_empty());
}

#[test]
fn stale_revision_is_rejected() {
    let h = harness();
    let config = h.config(QualityRuntime::DockerDesktop);
    assert_eq!(h.svc.configure(config, 7, None).unwrap_err(), STALE_MESSAGE);
}

#[test]
fn configure_resolves_only_after_readiness_and_persists() {
    let h = harness();
    h.desktop.start_delay_ms.store(150, Ordering::Relaxed);
    let config = h.config(QualityRuntime::DockerDesktop);
    let started = std::time::Instant::now();
    let state = h.svc.configure(config.clone(), 0, None).unwrap();
    assert!(started.elapsed() >= Duration::from_millis(300), "waited for both containers to start");
    assert_eq!(state.effective_mode, Some(QualityMode::SonarqubeCommunity));
    assert_eq!(state.community.running, Some(true));
    assert!(state.activated && state.revision == 1 && !state.busy);
    assert_eq!(h.desktop.running(), 2);
    let reloaded = store::load_config(&store::Paths::new(h.dir.join("code-quality"))).unwrap();
    assert_eq!(reloaded.config, config);
    assert!(reloaded.activated);
}

#[test]
fn twenty_concurrent_changes_run_one_and_reject_the_rest() {
    let h = harness();
    h.desktop.start_delay_ms.store(600, Ordering::Relaxed);
    let barrier = Arc::new(std::sync::Barrier::new(20));
    let handles: Vec<_> = (0..20)
        .map(|_| {
            let (svc, barrier, config) = (h.svc.clone(), barrier.clone(), h.config(QualityRuntime::DockerDesktop));
            std::thread::spawn(move || {
                barrier.wait();
                svc.configure(config, 0, None)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(results.iter().filter_map(|r| r.as_ref().err()).all(|e| e == BUSY_MESSAGE || e == STALE_MESSAGE));
    assert_eq!(h.desktop.count("create "), 2, "exactly one start sequence ran: {:?}", h.desktop.ops());
}

#[test]
fn identical_healthy_selection_is_a_noop() {
    let h = harness();
    let config = h.config(QualityRuntime::DockerDesktop);
    h.svc.configure(config.clone(), 0, None).unwrap();
    let ops = h.desktop.ops().len();
    let state = h.svc.configure(config, 1, None).unwrap();
    assert_eq!(state.revision, 1);
    assert_eq!(h.desktop.ops().len(), ops, "no stop/start for an identical healthy configuration");
}

#[test]
fn the_gate_blocks_changes_while_another_operation_runs() {
    let h = harness();
    let gate = h.svc.admit(QualityPhase::Scanning, None).unwrap();
    assert_eq!(h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap_err(), BUSY_MESSAGE);
    assert_eq!(h.svc.start_service(0).unwrap_err(), BUSY_MESSAGE);
    drop(gate);
    assert!(!h.svc.state().busy);
}

#[test]
fn switching_runtime_stops_the_old_server_before_starting_the_new_one() {
    let h = harness();
    h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap();
    let state = h.svc.configure(h.config(QualityRuntime::Podman { connection: "fake".into() }), 1, None).unwrap();
    assert_eq!(h.desktop.running(), 0, "old runtime's server and database were stopped");
    assert_eq!(h.podman.running(), 2);
    assert_eq!(state.revision, 2);
    let ops = h.desktop.ops();
    let sonar_stop = ops.iter().position(|o| o.starts_with("stop") && o.ends_with("-sonar")).unwrap();
    let db_stop = ops.iter().position(|o| o.starts_with("stop") && o.ends_with("-db")).unwrap();
    assert!(sonar_stop < db_stop, "Sonar stops before PostgreSQL: {ops:?}");
    // volumes are never deleted
    assert!(ops.iter().all(|o| !o.contains("volume rm") && !o.starts_with("remove")));
}

#[test]
fn failed_stop_blocks_the_new_start_and_retry_reconciles() {
    let h = harness();
    h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap();
    h.desktop.fail_stop.store(true, Ordering::Relaxed);
    let target = h.config(QualityRuntime::Podman { connection: "fake".into() });
    assert!(h.svc.configure(target.clone(), 1, None).is_err());
    assert!(h.podman.ops().is_empty(), "nothing started on the new runtime: {:?}", h.podman.ops());
    let state = h.svc.state();
    assert!(!state.unresolved.is_empty() && state.phase == QualityPhase::Error);
    assert_eq!(state.config.runtime, QualityRuntime::DockerDesktop, "previous config retained");
    assert_eq!(state.requested_config, None);
    // further transitions are refused until reconciliation
    assert!(h.svc.configure(target, 1, None).unwrap_err().contains("Start / retry"));
    h.desktop.fail_stop.store(false, Ordering::Relaxed);
    let state = h.svc.start_service(1).unwrap();
    assert!(state.unresolved.is_empty());
    assert_eq!(state.effective_mode, Some(QualityMode::SonarqubeCommunity));
}

#[test]
fn unreachable_old_daemon_prevents_a_second_server() {
    let h = harness();
    h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap();
    h.desktop.reachable.store(false, Ordering::Relaxed);
    let error = h.svc.configure(h.config(QualityRuntime::Podman { connection: "fake".into() }), 1, None).unwrap_err();
    assert!(error.contains("confirm"), "{error}");
    assert!(h.podman.ops().is_empty());
    assert!(!h.svc.state().unresolved.is_empty());
}

#[test]
fn alternate_without_license_acknowledgement_leaves_the_old_server_running() {
    let h = harness();
    h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap();
    let mut alternate = h.config(QualityRuntime::DockerDesktop);
    alternate.mode = QualityMode::Alternate;
    assert_eq!(h.svc.configure(alternate, 1, None).unwrap_err(), RULES_MESSAGE);
    assert_eq!(h.desktop.running(), 2, "the Community server was not stopped");
    assert_eq!(h.desktop.count("stop"), 0);
    assert_eq!(h.svc.state().effective_mode, Some(QualityMode::SonarqubeCommunity));
}

#[test]
fn token_only_updates_do_not_restart_and_invalid_tokens_keep_the_old_one() {
    let h = harness();
    let config = h.config(QualityRuntime::DockerDesktop);
    h.svc.configure(config.clone(), 0, None).unwrap();
    let update = |token: &str| QualityCredentialUpdate { target: CredentialTarget::Community, action: CredentialAction::Replace, token: Some(token.into()) };
    let ops = h.desktop.ops().len();
    let state = h.svc.configure(config.clone(), 1, Some(update("good-token"))).unwrap();
    assert!(state.credentials.community);
    assert_eq!(h.desktop.ops().len(), ops, "no restart for a token-only change");
    assert!(h.svc.configure(config.clone(), 1, Some(update("bad"))).is_err());
    assert!(h.svc.state().credentials.community, "the working token survives an invalid replacement");
    let cleared = QualityCredentialUpdate { target: CredentialTarget::Community, action: CredentialAction::Clear, token: None };
    let state = h.svc.configure(config, 1, Some(cleared)).unwrap();
    assert!(!state.credentials.community);
    assert_eq!(h.desktop.ops().len(), ops);
}

#[test]
fn state_json_never_contains_secrets() {
    let h = harness();
    let config = h.config(QualityRuntime::DockerDesktop);
    h.svc.configure(config.clone(), 0, None).unwrap();
    let update = QualityCredentialUpdate { target: CredentialTarget::Community, action: CredentialAction::Replace, token: Some("squ_super_secret_token".into()) };
    h.svc.configure(config, 1, Some(update)).unwrap();
    let json = serde_json::to_string(&h.svc.state()).unwrap();
    assert!(!json.contains("squ_super_secret_token"));
    for entry in walk(&h.dir) {
        if let Ok(text) = std::fs::read_to_string(&entry) {
            assert!(!text.contains("squ_super_secret_token"), "secret leaked into {}", entry.display());
        }
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn shutdown_stops_only_owned_servers() {
    let h = harness();
    h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap();
    // A foreign container that must never be touched.
    let foreign = h
        .desktop
        .create(&ContainerSpec { name: "someone-elses".into(), image: "x".into(), ..Default::default() }, &Ctl::silent())
        .unwrap();
    h.desktop.start(&foreign, &Ctl::silent()).unwrap();
    h.svc.shutdown();
    assert_eq!(h.desktop.running(), 1, "only the foreign container keeps running");
    assert!(h.desktop.ops().iter().all(|o| !o.contains(&foreign) || o.starts_with("remove") == false));
}

#[test]
#[cfg(windows)]
fn a_second_process_is_read_only() {
    let dir = std::env::temp_dir().join(format!("omp-cq-ro-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let _owner = ProcessLock::acquire(&process_lock::name_for(&dir.join("code-quality"))).unwrap().unwrap();
    let svc = CodeQualityService::with_parts(dir.clone(), Arc::new(FakeFactory(HashMap::new())), Arc::new(MemorySecrets::default()), Ok(None), ready_probe(Arc::new(AtomicBool::new(true))));
    assert!(svc.configure(QualityConfig::default(), 0, None).unwrap_err().contains("Another OMP Manager process"));
    assert_eq!(svc.state().ownership.status, OwnershipStatus::OtherProcess);
    assert!(svc.state().error.is_none(), "another owner is state, not a failure");
    assert!(!dir.join("code-quality").exists(), "a non-owner never creates installation state");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unreadable_state_is_never_reset() {
    let dir = std::env::temp_dir().join(format!("omp-cq-bad-{}", uuid::Uuid::new_v4()));
    let root = dir.join("code-quality");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("resources.json"), b"{ nope").unwrap();
    let svc = CodeQualityService::with_parts(dir.clone(), Arc::new(FakeFactory(HashMap::new())), Arc::new(MemorySecrets::default()), Ok(Some(ProcessLock::unowned())), ready_probe(Arc::new(AtomicBool::new(true))));
    assert!(svc.start_service(0).unwrap_err().contains("ownership"));
    assert_eq!(std::fs::read(root.join("resources.json")).unwrap(), b"{ nope");
    let _ = std::fs::remove_dir_all(dir);
}

fn sample_scan(id: &str, folder: &str, status: QualityScanStatus) -> QualityScan {
    let config = QualityConfig::default();
    QualityScan {
        id: id.into(),
        folder: folder.into(),
        config,
        project: QualityProject {
            folder: folder.into(),
            project_key: "omp-test".into(),
            scanner: Scanner::Cli,
            entry_file: None,
            image: None,
            build_args: vec![],
            prepare_npm_dependencies: false,
            exclusions: vec![],
        },
        status,
        phase: "Running".into(),
        started_at: 1,
        finished_at: None,
        snapshot_hash: String::new(),
        rules_hash: String::new(),
        tools: vec![QualityToolOutcome {
            tool: "t".into(),
            version: "1".into(),
            status: ToolStatus::Complete,
            exit_code: Some(0),
            analyzed_files: None,
            finding_count: 0,
            error: None,
            report_names: vec!["t.json".into()],
        }],
        findings: vec![],
        warnings: vec![],
        error: None,
        analysis_id: None,
        dashboard_url: None,
        quality_gate: None,
        log: vec![],
    }
}

#[test]
fn nonterminal_jobs_are_restored_as_interrupted_and_finished_ones_are_kept() {
    let h = harness();
    let (running, done) = (uuid::Uuid::new_v4().to_string(), uuid::Uuid::new_v4().to_string());
    let paths = store::Paths::new(h.dir.join("code-quality"));
    store::save_scan(&paths, &sample_scan(&running, "C:/p", QualityScanStatus::Running)).unwrap();
    store::save_scan(&paths, &sample_scan(&done, "C:/p", QualityScanStatus::Complete)).unwrap();
    h.svc.recover_interrupted_jobs();
    let recovered = h.svc.read_scan(&running).unwrap();
    assert_eq!(recovered.status, QualityScanStatus::Interrupted);
    assert!(recovered.finished_at.is_some() && recovered.error.is_some());
    assert_eq!(h.svc.read_scan(&done).unwrap().status, QualityScanStatus::Complete);
}

#[test]
fn report_previews_are_limited_to_names_recorded_in_the_scan() {
    let h = harness();
    let id = uuid::Uuid::new_v4().to_string();
    let paths = store::Paths::new(h.dir.join("code-quality"));
    store::save_scan(&paths, &sample_scan(&id, "C:/p", QualityScanStatus::Complete)).unwrap();
    std::fs::create_dir_all(paths.job(&id).join("reports")).unwrap();
    std::fs::write(paths.job(&id).join("reports").join("t.json"), "{\"ok\":true}").unwrap();
    std::fs::write(paths.job(&id).join("reports").join("other.json"), "secret").unwrap();
    std::fs::write(paths.job(&id).join("request.json"), "private").unwrap();
    assert_eq!(h.svc.read_report(&id, "t.json").unwrap().text, "{\"ok\":true}");
    for name in ["other.json", "../request.json", "..\\request.json", "request.json", ""] {
        assert!(h.svc.read_report(&id, name).is_err(), "{name}");
    }
    assert!(h.svc.read_report("not-a-uuid", "t.json").is_err());
}

#[test]
fn scans_need_a_ready_backend_and_a_saved_token() {
    let h = harness();
    let folder = h.dir.join("project");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("main.py"), "print('x')\n").unwrap();
    let project = |key: &str| QualityProject {
        folder: folder.to_string_lossy().to_string(),
        project_key: key.into(),
        scanner: Scanner::Cli,
        entry_file: None,
        image: None,
        build_args: vec![],
        prepare_npm_dependencies: false,
        exclusions: vec![],
    };
    // Nothing activated yet.
    let err = h.svc.start_scan(project("omp-a"), "x".into(), false).unwrap_err();
    assert!(err.contains("not ready"), "{err}");
    h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap();
    // The server bootstraps its own token; clear it to prove scans need one.
    let cleared = QualityCredentialUpdate { target: CredentialTarget::Community, action: CredentialAction::Clear, token: None };
    h.svc.configure(h.config(QualityRuntime::DockerDesktop), 1, Some(cleared)).unwrap();
    let preparation = h.svc.prepare_scan(project("omp-a")).unwrap();
    assert!(!preparation.recipe.is_empty() && !preparation.requires_consent);
    assert!(!preparation.recipe.join(" ").contains("token"), "{:?}", preparation.recipe);
    let err = h.svc.start_scan(project("omp-a"), "stale-hash".into(), false).unwrap_err();
    assert!(err.contains("changed since"), "{err}");
    let err = h.svc.start_scan(project("omp-a"), preparation.recipe_hash, false).unwrap_err();
    assert!(err.contains("token"), "{err}");
    assert!(!h.svc.state().busy, "a rejected scan releases the gate");
    assert!(h.svc.list_scans(&folder.to_string_lossy()).unwrap().is_empty());
}

fn admin_password_target(h: &Harness, set: &str) -> String {
    secrets::target_name(&h.dir, &format!("community:{set}"), "adminpassword")
}

#[test]
fn a_fresh_server_gets_a_random_admin_password_and_a_scan_token() {
    let h = harness();
    let state = h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap();
    assert!(state.credentials.community, "token created automatically");
    assert_eq!(h.sonar.count("/api/users/change_password"), 1);
    assert_eq!(h.sonar.count("/api/user_tokens/generate"), 1);
    let set = state.community.runtime_id.clone().unwrap();
    let password = h.secrets.read(&admin_password_target(&h, &set)).unwrap().expect("admin password saved");
    assert!(password.len() > 40 && password != "admin");
    let token = h.secrets.read(&h.svc.community_token_target(&QualityRuntime::DockerDesktop).unwrap()).unwrap();
    assert_eq!(token.as_deref(), Some("squ_boot"));
    let json = serde_json::to_string(&state).unwrap();
    assert!(!json.contains("squ_boot") && !json.contains(&password));
    for entry in walk(&h.dir) {
        if let Ok(text) = std::fs::read_to_string(&entry) {
            assert!(!text.contains("squ_boot") && !text.contains(&password), "secret leaked into {}", entry.display());
        }
    }
}

#[test]
fn starting_again_does_not_touch_the_admin_account_or_create_another_token() {
    let h = harness();
    h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap();
    let (changes, tokens) = (h.sonar.count("/api/users/change_password"), h.sonar.count("/api/user_tokens/generate"));
    h.svc.start_service(1).unwrap();
    assert_eq!((h.sonar.count("/api/users/change_password"), h.sonar.count("/api/user_tokens/generate")), (changes, tokens));
}

#[test]
fn an_admin_password_changed_elsewhere_is_never_touched() {
    let h = harness();
    h.sonar.password_changed.store(true, Ordering::Relaxed);
    let state = h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap();
    assert_eq!(state.effective_mode, Some(QualityMode::SonarqubeCommunity), "the server still starts");
    assert!(!state.credentials.community);
    assert_eq!(h.sonar.count("/api/users/change_password"), 0);
    assert_eq!(h.sonar.count("/api/user_tokens/generate"), 0);
    let set = state.community.runtime_id.unwrap();
    assert_eq!(h.secrets.read(&admin_password_target(&h, &set)).unwrap(), None);
    assert!(state.diagnostics.iter().any(|l| l.contains("already changed")), "{:?}", state.diagnostics);
}

#[test]
fn a_rejected_password_change_leaves_no_saved_admin_password() {
    let h = harness();
    h.sonar.fail_change.store(true, Ordering::Relaxed);
    let state = h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap();
    assert!(!state.credentials.community);
    let set = state.community.runtime_id.unwrap();
    assert_eq!(h.secrets.read(&admin_password_target(&h, &set)).unwrap(), None);
    assert!(state.diagnostics.iter().any(|l| l.contains("Could not change")), "{:?}", state.diagnostics);
}

#[test]
fn setup_needs_the_consent_checkbox_before_anything_is_touched() {
    let h = harness();
    assert_eq!(h.svc.setup(0, false).unwrap_err(), "Review and accept the setup details first.");
    assert!(!h.svc.state().busy);
    assert!(h.desktop.ops().is_empty() && h.podman.ops().is_empty());
}

#[cfg(windows)]
#[test]
fn setup_steps_follow_what_is_installed_started_and_saved() {
    let h = harness();
    {
        let mut inner = h.svc.inner.lock();
        inner.prerequisites = vec![Prerequisite { runtime: QualityRuntime::Podman { connection: String::new() }, installed: false, ready: false, needs_restart: false, message: None }];
        inner.options.wsl_features_missing = true;
    }
    let setup = h.svc.state().setup;
    let state_of = |id: &str| setup.steps.iter().find(|s| s.id == id).map(|s| s.state);
    assert!(setup.needed);
    assert_eq!(state_of("windowsFeatures"), Some(SetupStepState::Pending));
    assert_eq!(state_of("runtime"), Some(SetupStepState::Pending));
    assert_eq!(state_of("machine"), Some(SetupStepState::Pending));
    assert_eq!(state_of("server"), Some(SetupStepState::Pending));
    assert_eq!(state_of("token"), Some(SetupStepState::Pending));
    {
        let mut inner = h.svc.inner.lock();
        inner.prerequisites[0].installed = true;
        inner.options.wsl_features_missing = false;
        inner.options.podman_app_connection = Some("omp-manager-quality".into());
        inner.effective = Some(QualityMode::SonarqubeCommunity);
        inner.credentials.community = true;
    }
    let setup = h.svc.state().setup;
    assert!(!setup.needed && setup.steps.iter().all(|s| s.state == SetupStepState::Done), "{setup:?}");
    h.svc.inner.lock().prerequisites[0].needs_restart = true;
    let blocked = h.svc.state().setup;
    assert_eq!(blocked.steps.iter().find(|s| s.id == "windowsFeatures").unwrap().state, SetupStepState::Blocked);
}

#[test]
fn opening_the_view_before_setup_touches_no_engine() {
    let h = harness();
    let state = h.svc.enter().unwrap();
    assert!(!state.activated && !state.busy && state.phase == QualityPhase::Idle);
    assert!(h.desktop.ops().is_empty() && h.podman.ops().is_empty());
}

#[test]
fn nothing_starts_at_app_launch_but_opening_the_view_starts_the_analyzer() {
    let h = harness();
    h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap();
    h.svc.shutdown();
    let (svc, desktop, _podman) = h.restart();
    svc.reconcile_on_start();
    std::thread::sleep(Duration::from_millis(300));
    assert!(desktop.ops().is_empty(), "launch started nothing: {:?}", desktop.ops());
    assert_eq!(svc.state().effective_mode, None);
    let state = svc.enter().unwrap();
    assert_eq!(state.effective_mode, Some(QualityMode::SonarqubeCommunity));
    assert_eq!(desktop.running(), 2);
    assert_eq!(state.revision, 1, "re-confirming the committed selection is not a change");
    assert!(!svc.state().busy);
}

#[test]
fn start_at_launch_is_a_persisted_setting_that_does_not_change_the_revision() {
    let h = harness();
    h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap();
    let state = h.svc.set_start_at_launch(true).unwrap();
    assert!(state.start_at_launch && state.revision == 1);
    h.svc.shutdown();
    let (svc, desktop, _podman) = h.restart();
    assert!(svc.state().start_at_launch);
    svc.reconcile_on_start();
    let started = std::time::Instant::now();
    while svc.state().effective_mode.is_none() && started.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(svc.state().effective_mode, Some(QualityMode::SonarqubeCommunity));
    assert_eq!(desktop.running(), 2);
    let state = svc.set_start_at_launch(false).unwrap();
    assert!(!state.start_at_launch);
}

#[test]
fn the_launch_setting_cannot_be_changed_while_an_operation_runs() {
    let h = harness();
    let gate = h.svc.admit(QualityPhase::Scanning, None).unwrap();
    assert_eq!(h.svc.set_start_at_launch(true).unwrap_err(), BUSY_MESSAGE);
    drop(gate);
    assert!(!h.svc.state().start_at_launch);
}

#[test]
fn a_missing_runtime_sends_the_view_to_setup_instead_of_starting_anything() {
    let h = harness();
    h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap();
    h.svc.shutdown();
    let (svc, desktop, _podman) = h.restart();
    h.installed.store(false, Ordering::Relaxed);
    let state = svc.enter().unwrap();
    assert_eq!(state.phase, QualityPhase::NeedsSetup);
    assert!(state.error.is_some() && state.effective_mode.is_none());
    assert!(desktop.ops().is_empty());
}

#[test]
fn enter_decisions_cover_every_branch() {
    use lifecycle::{enter_decision, EnterDecision};
    let missing = Prerequisite { runtime: QualityRuntime::DockerDesktop, installed: false, ready: false, needs_restart: false, message: Some("install it".into()) };
    let present = Prerequisite { installed: true, ..missing.clone() };
    assert_eq!(enter_decision(false, Some(&missing), false), EnterDecision::Nothing);
    assert_eq!(enter_decision(true, Some(&missing), false), EnterDecision::NeedsSetup("install it".into()));
    assert_eq!(enter_decision(true, Some(&present), true), EnterDecision::Nothing);
    assert_eq!(enter_decision(true, Some(&present), false), EnterDecision::Start);
    assert_eq!(enter_decision(true, None, false), EnterDecision::Start);
}

#[cfg(windows)]
#[test]
fn installed_prerequisites_do_not_require_installation_when_only_analyzer_and_token_are_missing() {
    let h = harness();
    assert!(h.svc.state().setup.install_required, "unprobed state is not installation-ready");
    {
        let mut inner = h.svc.inner.lock();
        inner.file.config.runtime = QualityRuntime::Podman { connection: "fake".into() };
        inner.prerequisites = vec![Prerequisite { runtime: inner.file.config.runtime.clone(), installed: true, ready: false, needs_restart: false, message: None }];
    }
    let setup = h.svc.state().setup;
    assert!(setup.needed);
    assert!(!setup.install_required);
    assert!(setup.steps.iter().filter(|s| matches!(s.id.as_str(), "server" | "token")).all(|s| s.state == SetupStepState::Pending));
    h.svc.inner.lock().options.wsl_features_missing = true;
    assert!(h.svc.state().setup.install_required);
}

#[test]
fn explicit_shutdown_suspends_navigation_and_polling_but_explicit_start_resumes() {
    let h = harness();
    h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap();
    h.svc.check_prerequisites().unwrap();
    let state = h.svc.stop_server().unwrap();
    assert!(state.suspended && state.effective_mode.is_none());
    assert_eq!(state.community.running, Some(false));
    assert!(!state.prerequisites.iter().find(|p| p.runtime == QualityRuntime::DockerDesktop).unwrap().ready);
    assert_eq!(h.desktop.running(), 0);
    let ops = h.desktop.ops();
    for _ in 0..3 {
        assert!(h.svc.enter().unwrap().suspended);
        h.svc.refresh_ownership().unwrap();
        assert!(h.svc.state().suspended);
    }
    assert_eq!(h.desktop.ops(), ops);
    assert!(!h.svc.state().setup.install_required);
    let state = h.svc.start_service(1).unwrap();
    assert!(!state.suspended && state.effective_mode.is_some());
    assert!(state.prerequisites.iter().find(|p| p.runtime == QualityRuntime::DockerDesktop).unwrap().ready,
        "successful resume must enable subsequent scan controls without requiring Recheck");
    assert_eq!(h.desktop.running(), 2);
}

#[test]
fn shutdown_is_busy_during_scan_admission_and_failure_stays_suspended() {
    let h = harness();
    h.svc.configure(h.config(QualityRuntime::DockerDesktop), 0, None).unwrap();
    let gate = h.svc.admit(QualityPhase::Scanning, None).unwrap();
    assert_eq!(h.svc.stop_server().unwrap_err(), BUSY_MESSAGE);
    assert!(!h.svc.state().suspended);
    drop(gate);
    h.desktop.fail_stop.store(true, Ordering::Relaxed);
    assert!(h.svc.stop_server().is_err());
    let state = h.svc.state();
    assert!(state.suspended && state.error.is_some() && !state.unresolved.is_empty());
    let ops = h.desktop.ops();
    h.svc.enter().unwrap();
    assert_eq!(h.desktop.ops(), ops);
    h.desktop.fail_stop.store(false, Ordering::Relaxed);
    assert!(h.svc.stop_server().unwrap().suspended);
    assert!(h.svc.state().unresolved.is_empty());
}

#[test]
fn preparation_is_read_only_and_does_not_remember_project_choices() {
    let h = harness();
    let folder = h.dir.join("project");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("index.js"), "const answer = 42;").unwrap();
    let project = h.svc.detect_project(folder.to_str().unwrap()).unwrap().project;
    let target = h.svc.paths.project(&project.folder);
    assert!(!target.exists());
    h.svc.prepare_scan(project).unwrap();
    assert!(!target.exists());
}

#[test]
fn ownership_capability_never_enters_normal_state() {
    let h = harness();
    let owner: serde_json::Value = serde_json::from_slice(&std::fs::read(h.svc.paths.root.join("owner.json")).unwrap()).unwrap();
    let capability = owner["capability"].as_str().unwrap();
    let normal = serde_json::to_string(&h.svc.state()).unwrap();
    assert!(!normal.contains(capability));
    assert_eq!(h.svc.state().ownership.status, OwnershipStatus::Owned);
}

fn owner_frame(svc: &CodeQualityService, frame: serde_json::Value) -> Option<serde_json::Value> {
    let descriptor: serde_json::Value = serde_json::from_slice(&std::fs::read(svc.paths.root.join("owner.json")).unwrap()).unwrap();
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", descriptor["port"].as_u64().unwrap() as u16)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.write_all(&serde_json::to_vec(&frame).unwrap()).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut frame = Vec::new();
    for byte in stream.bytes() {
        let byte = byte.unwrap();
        if byte == b'\n' { break; }
        frame.push(byte);
        assert!(frame.len() <= 16 * 1024);
    }
    if frame.is_empty() { None } else { Some(serde_json::from_slice(&frame).unwrap()) }
}

#[test]
fn handover_rejects_wrong_capability_epoch_and_busy_installer_without_cancellation() {
    let h = harness();
    let descriptor: serde_json::Value = serde_json::from_slice(&std::fs::read(h.svc.paths.root.join("owner.json")).unwrap()).unwrap();
    let request = serde_json::json!({ "version": 1, "instanceId": descriptor["instanceId"], "capability": descriptor["capability"], "requestId": uuid::Uuid::new_v4().to_string(), "action": "handover" });
    let mut wrong = request.clone();
    wrong["capability"] = serde_json::Value::String("0".repeat(64));
    assert!(owner_frame(&h.svc, wrong).is_none());
    let mut wrong = request.clone();
    wrong["instanceId"] = serde_json::Value::String(uuid::Uuid::new_v4().to_string());
    assert!(owner_frame(&h.svc, wrong).is_none());
    let gate = h.svc.admit(QualityPhase::Installing, None).unwrap();
    let reply = owner_frame(&h.svc, request).unwrap();
    assert_eq!(reply["status"], "busy");
    assert!(!gate.cancelled());
    assert_eq!(h.svc.state().ownership.status, OwnershipStatus::Owned);
    assert!(h.desktop.ops().is_empty() && h.podman.ops().is_empty());
}

#[cfg(windows)]
fn real_lock_service(dir: &Path, engine: Arc<FakeEngine>, secrets: Arc<MemorySecrets>) -> Arc<CodeQualityService> {
    let mut engines = HashMap::new();
    engines.insert(engine.runtime.key(), engine);
    CodeQualityService::with_parts(dir.to_path_buf(), Arc::new(FakeFactory(engines)), secrets,
        ProcessLock::acquire(&process_lock::name_for(&dir.join("code-quality"))), ready_probe(Arc::new(AtomicBool::new(true))))
}

#[cfg(windows)]
#[test]
fn cooperative_transfer_stops_before_release_and_old_shutdown_cannot_stop_new_owner() {
    let dir = std::env::temp_dir().join(format!("omp-cq-transfer-{}", uuid::Uuid::new_v4()));
    let engine = FakeEngine::new(QualityRuntime::DockerDesktop);
    let secrets = Arc::new(MemorySecrets::default());
    let owner = real_lock_service(&dir, engine.clone(), secrets.clone());
    let (port, _) = fake_sonar();
    owner.configure(QualityConfig { runtime: QualityRuntime::DockerDesktop, community_port: port, ..QualityConfig::default() }, 0, None).unwrap();
    let waiting = real_lock_service(&dir, engine.clone(), secrets);
    assert_eq!(waiting.state().ownership.status, OwnershipStatus::OtherProcess);
    let scan_id = uuid::Uuid::new_v4().to_string();
    let gate = owner.admit(QualityPhase::Scanning, None).unwrap();
    let cancel = gate.ctl.cancel.clone();
    owner.inner.lock().active_scan = Some(ActiveScan { id: scan_id.clone(), cancel: cancel.clone(), scan: sample_scan(&scan_id, "fixture", QualityScanStatus::Running) });
    let scanner = engine.create(&ContainerSpec { name: "delayed-scanner".into(), image: "fixture".into(), labels: vec![(OWNER_LABEL.into(), owner.installation_id()), (ROLE_LABEL.into(), ROLE_SCAN.into())], ..Default::default() }, &Ctl::silent()).unwrap();
    engine.start(&scanner, &Ctl::silent()).unwrap();
    let worker_engine = engine.clone();
    let worker_owner = owner.clone();
    owner.spawn_worker("handover-test-scan", move || {
        while !cancel.load(Ordering::Relaxed) { std::thread::sleep(Duration::from_millis(10)); }
        std::thread::sleep(Duration::from_millis(150));
        worker_engine.stop(&scanner, 30, &Ctl::silent()).unwrap();
        assert!(!worker_engine.inspect(&scanner, &Ctl::silent()).unwrap().unwrap().running);
        worker_owner.inner.lock().active_scan = None;
        gate.finish(QualityPhase::Idle, None);
    }).unwrap();
    assert!(waiting.take_over(false).is_err());
    assert_eq!(engine.running(), 3);
    let barrier = Arc::new(std::sync::Barrier::new(20));
    let handles: Vec<_> = (0..20).map(|_| {
        let (waiting, barrier) = (waiting.clone(), barrier.clone());
        std::thread::spawn(move || { barrier.wait(); waiting.take_over(true) })
    }).collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(results.iter().any(|r| r.is_ok()));
    assert!(results.iter().filter_map(|r| r.as_ref().err()).all(|e| e == BUSY_MESSAGE));
    assert_eq!(engine.count("stop "), 3, "concurrent requests never queue multiple shutdown/restart cycles");
    let stops: Vec<_> = engine.ops().into_iter().filter(|op| op.starts_with("stop ")).collect();
    assert_eq!(stops[0], "stop delayed-scanner");
    assert!(stops[1].ends_with("-sonar") && stops[2].ends_with("-db"));
    assert_eq!(waiting.state().ownership.status, OwnershipStatus::Owned);
    assert_eq!(owner.state().ownership.status, OwnershipStatus::HandedOff);
    assert_eq!(engine.running(), 0, "all servers stopped before successor could acquire");
    for _ in 0..3 { owner.refresh_ownership().unwrap(); }
    assert_eq!(owner.state().ownership.status, OwnershipStatus::HandedOff);
    waiting.enter().unwrap();
    assert_eq!(engine.running(), 2);
    owner.shutdown();
    assert_eq!(engine.running(), 2, "old app exit never touches successor resources");
    waiting.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(windows)]
#[test]
fn stop_failure_keeps_os_authority_and_can_be_retried() {
    let dir = std::env::temp_dir().join(format!("omp-cq-transfer-failure-{}", uuid::Uuid::new_v4()));
    let engine = FakeEngine::new(QualityRuntime::DockerDesktop);
    let secrets = Arc::new(MemorySecrets::default());
    let owner = real_lock_service(&dir, engine.clone(), secrets.clone());
    let (port, _) = fake_sonar();
    owner.configure(QualityConfig { runtime: QualityRuntime::DockerDesktop, community_port: port, ..QualityConfig::default() }, 0, None).unwrap();
    let waiting = real_lock_service(&dir, engine.clone(), secrets);
    engine.fail_stop.store(true, Ordering::Relaxed);
    assert!(waiting.take_over(true).is_err());
    assert_eq!(owner.state().ownership.status, OwnershipStatus::Owned);
    assert_eq!(waiting.state().ownership.status, OwnershipStatus::OtherProcess);
    assert!(ProcessLock::acquire(&process_lock::name_for(&owner.paths.root)).unwrap().is_none());
    engine.fail_stop.store(false, Ordering::Relaxed);
    waiting.take_over(true).unwrap();
    assert_eq!(waiting.state().ownership.status, OwnershipStatus::Owned);
    owner.shutdown();
    waiting.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(windows)]
#[test]
#[ignore = "subprocess fixture invoked with an isolated quality root"]
fn ownership_child_process() {
    let Some(root) = std::env::var_os("OMP_QUALITY_OWNERSHIP_TEST_ROOT") else { return };
    let dir = PathBuf::from(root);
    let engine = FakeEngine::new(QualityRuntime::DockerDesktop);
    let service = real_lock_service(&dir, engine, Arc::new(MemorySecrets::default()));
    assert_eq!(service.state().ownership.status, OwnershipStatus::Owned);
    let mut file = service.inner.lock().file.clone();
    file.revision = 41;
    store::save_config(&service.paths, &file).unwrap();
    service.inner.lock().file = file;
    let id = std::env::var("OMP_QUALITY_OWNERSHIP_SCAN_ID").unwrap();
    std::fs::create_dir_all(service.paths.job(&id)).unwrap();
    store::save_scan(&service.paths, &sample_scan(&id, "fixture", QualityScanStatus::Running)).unwrap();
    std::fs::write(service.paths.job(&id).join("source.tar"), b"live-snapshot").unwrap();
    std::fs::write(dir.join("child-ready"), b"ready").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !dir.join("child-exit").exists() && std::time::Instant::now() < deadline {
        if dir.join("child-update").exists() && !dir.join("child-updated").exists() {
            let mut file = service.inner.lock().file.clone();
            file.revision = 42;
            store::save_config(&service.paths, &file).unwrap();
            service.inner.lock().file = file;
            std::fs::write(dir.join("child-updated"), b"updated").unwrap();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if std::env::var_os("OMP_QUALITY_OWNERSHIP_ABANDON").is_some() { std::process::exit(0); }
    service.shutdown();
}

#[cfg(windows)]
fn subprocess_ownership_case(abandon: bool) {
    let dir = std::env::temp_dir().join(format!("omp-cq-process-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "code_quality::tests::ownership_child_process", "--ignored", "--nocapture"])
        .env("OMP_QUALITY_OWNERSHIP_TEST_ROOT", &dir).env("OMP_QUALITY_OWNERSHIP_SCAN_ID", &id)
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    if abandon { command.env("OMP_QUALITY_OWNERSHIP_ABANDON", "1"); }
    let mut child = command.spawn().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !dir.join("child-ready").exists() && std::time::Instant::now() < deadline {
        assert!(child.try_wait().unwrap().is_none(), "isolated ownership child exited before publishing");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(dir.join("child-ready").exists());
    let paths = Paths::new(dir.join("code-quality"));
    let result_bytes = std::fs::read(paths.result(&id)).unwrap();
    let snapshot_bytes = std::fs::read(paths.job(&id).join("source.tar")).unwrap();
    let engine = FakeEngine::new(QualityRuntime::DockerDesktop);
    let waiting = real_lock_service(&dir, engine.clone(), Arc::new(MemorySecrets::default()));
    waiting.reconcile_on_start();
    assert_eq!(waiting.state().ownership.status, OwnershipStatus::OtherProcess);
    assert_eq!(std::fs::read(paths.result(&id)).unwrap(), result_bytes);
    assert_eq!(std::fs::read(paths.job(&id).join("source.tar")).unwrap(), snapshot_bytes);
    assert_eq!(waiting.state().revision, 41);
    std::fs::write(dir.join("child-update"), b"update").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !dir.join("child-updated").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(dir.join("child-updated").exists());
    if abandon {
        std::fs::write(dir.join("child-exit"), b"exit").unwrap();
        assert!(child.wait().unwrap().success());
        waiting.refresh_ownership().unwrap();
    } else {
        waiting.take_over(true).unwrap();
        std::fs::write(dir.join("child-exit"), b"exit").unwrap();
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(waiting.state().ownership.status, OwnershipStatus::Owned);
    assert_eq!(waiting.state().revision, 42);
    assert!(engine.ops().is_empty(), "refresh and transfer never start an engine");
    assert_eq!(store::load_scan(&paths, &id).unwrap().unwrap().status, QualityScanStatus::Interrupted);
    assert!(!paths.job(&id).join("source.tar").exists());
    waiting.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(windows)]
#[test]
fn real_process_handover_acquires_os_authority_and_reloads_before_mutation() {
    subprocess_ownership_case(false);
}

#[cfg(windows)]
#[test]
fn waiting_window_recovers_when_actual_owner_process_exits() {
    subprocess_ownership_case(true);
}

#[cfg(windows)]
#[test]
fn legacy_owner_without_endpoint_requires_closing_without_creating_state() {
    let dir = std::env::temp_dir().join(format!("omp-cq-legacy-{}", uuid::Uuid::new_v4()));
    let lock = ProcessLock::acquire(&process_lock::name_for(&dir.join("code-quality"))).unwrap().unwrap();
    let waiting = real_lock_service(&dir, FakeEngine::new(QualityRuntime::DockerDesktop), Arc::new(MemorySecrets::default()));
    let error = waiting.take_over(true).unwrap_err();
    assert_eq!(error, "The other OMP Manager must finish closing before control can be transferred. Close that app and retry.");
    assert_eq!(waiting.state().ownership.status, OwnershipStatus::OtherProcess);
    assert!(!dir.exists());
    drop(lock);
    waiting.refresh_ownership().unwrap();
    assert_eq!(waiting.state().ownership.status, OwnershipStatus::Owned);
    waiting.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn pending_analysis_without_verifiable_token_prevents_release() {
    let h = harness();
    let mut resources = store::load_resources(&h.svc.paths).unwrap();
    resources.tasks.push(store::PendingTask { scan_id: uuid::Uuid::new_v4().to_string(), task_id: "pending-task".into(), config: QualityConfig::default(), project_key: "fixture".into() });
    store::save_resources(&h.svc.paths, &resources).unwrap();
    let descriptor: serde_json::Value = serde_json::from_slice(&std::fs::read(h.svc.paths.root.join("owner.json")).unwrap()).unwrap();
    let reply = owner_frame(&h.svc, serde_json::json!({ "version": 1, "instanceId": descriptor["instanceId"], "capability": descriptor["capability"], "requestId": uuid::Uuid::new_v4().to_string(), "action": "handover" })).unwrap();
    assert_eq!(reply["status"], "error");
    assert_eq!(h.svc.state().ownership.status, OwnershipStatus::Owned);
    assert_eq!(store::load_resources(&h.svc.paths).unwrap().tasks.len(), 1);
    assert!(h.desktop.ops().is_empty() && h.podman.ops().is_empty());
}

#[cfg(windows)]
#[test]
fn application_exit_retains_mutex_until_the_admitted_operation_has_finished() {
    let dir = std::env::temp_dir().join(format!("omp-cq-exit-gate-{}", uuid::Uuid::new_v4()));
    let owner = real_lock_service(&dir, FakeEngine::new(QualityRuntime::DockerDesktop), Arc::new(MemorySecrets::default()));
    let gate = owner.admit(QualityPhase::Checking, None).unwrap();
    let exiting = owner.clone();
    let exit = std::thread::spawn(move || exiting.shutdown());
    while !owner.closing.load(Ordering::Relaxed) { std::thread::yield_now(); }
    let repeating = owner.clone();
    let repeat = std::thread::spawn(move || repeating.shutdown());
    assert!(ProcessLock::acquire(&process_lock::name_for(&owner.paths.root)).unwrap().is_none());
    assert!(!exit.is_finished());
    gate.finish(QualityPhase::Idle, None);
    exit.join().unwrap();
    repeat.join().unwrap();
    assert!(ProcessLock::acquire(&process_lock::name_for(&owner.paths.root)).unwrap().is_some());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn start_on_installed_prerequisites_bootstraps_without_installation_consent() {
    let h = harness();
    {
        let mut inner = h.svc.inner.lock();
        inner.file.config = h.config(QualityRuntime::DockerDesktop);
        store::save_config(&h.svc.paths, &inner.file).unwrap();
    }
    let setup = h.svc.check_prerequisites().unwrap().setup;
    assert!(setup.needed && !setup.install_required);
    let state = h.svc.start_service(0).unwrap();
    assert!(state.activated && state.credentials.community);
    assert_eq!(state.effective_mode, Some(QualityMode::SonarqubeCommunity));
    assert!(!state.setup.needed && !state.setup.install_required);
    assert!(!h.svc.pending_setup_path().exists());
    assert_eq!(h.desktop.running(), 2);
}

#[test]
fn unresolved_compute_task_cannot_be_reported_as_a_successful_shutdown() {
    let h = harness();
    h.svc.inner.lock().file.config.runtime = QualityRuntime::DockerDesktop;
    let mut resources = store::load_resources(&h.svc.paths).unwrap();
    resources.tasks.push(store::PendingTask { scan_id: uuid::Uuid::new_v4().to_string(), task_id: "pending".into(), config: h.svc.state().config, project_key: "fixture".into() });
    store::save_resources(&h.svc.paths, &resources).unwrap();
    assert!(h.svc.stop_server().is_err());
    assert!(h.svc.state().suspended && h.svc.state().error.is_some());
    assert_eq!(store::load_resources(&h.svc.paths).unwrap().tasks.len(), 1);
    assert!(h.desktop.ops().is_empty());
}

// ---- agent reader (`agent_report`, `read_report_slice`) ----------------------------------------------------

fn quality_finding(n: usize, severity: Severity, tool: &str, message: String) -> QualityFinding {
    QualityFinding {
        id: format!("f{n}"), tools: vec![tool.into()], rule_id: format!("rule-{n}"), category: "bug".into(), severity,
        message, path: Some(format!("src/f{n}.rs")), line: Some(n as u32), column: None, end_line: None, help_url: None, hotspot: n % 2 == 0,
    }
}

/// A saved scan with a `t.json` report for a real folder `name`; returns (canonical folder, scan id).
fn saved_quality_scan(h: &Harness, name: &str, findings: Vec<QualityFinding>, report: &str) -> (String, String) {
    let folder = h.dir.join(name);
    std::fs::create_dir_all(&folder).unwrap();
    let canonical = scan::canonical_folder(folder.to_str().unwrap()).unwrap().to_string_lossy().to_string();
    let id = uuid::Uuid::new_v4().to_string();
    let mut saved = sample_scan(&id, &canonical, QualityScanStatus::Complete);
    saved.findings = findings;
    let paths = store::Paths::new(h.dir.join("code-quality"));
    store::save_scan(&paths, &saved).unwrap();
    store::save_project(&paths, &canonical, &store::ProjectFile { version: 1, project: None, recent: vec![id.clone()] }).unwrap();
    std::fs::create_dir_all(paths.job(&id).join("reports")).unwrap();
    std::fs::write(paths.job(&id).join("reports").join("t.json"), report).unwrap();
    (canonical, id)
}

fn ask(h: &Harness, folder: &str, request: serde_json::Value) -> Result<serde_json::Value> {
    let request: AgentReportRequest = serde_json::from_value(request).map_err(|e| e.to_string())?;
    h.svc.agent_report(folder, &request)
}

#[test]
fn agent_reader_serves_only_the_callers_repository_without_any_engine_or_gate() {
    let h = harness();
    let findings: Vec<_> = (0..5)
        .map(|n| quality_finding(n, if n < 2 { Severity::High } else { Severity::Low }, if n % 2 == 0 { "opengrep" } else { "ruff" }, format!("problem {n}")))
        .collect();
    let (folder_a, scan_a) = saved_quality_scan(&h, "repo-a", findings, "{\"ok\":true}");
    let (folder_b, scan_b) = saved_quality_scan(&h, "repo-b", vec![quality_finding(9, Severity::Critical, "ruff", "other repo".into())], "secret of b");
    // A running operation owns the gate: reading must neither wait for it nor disturb it.
    let gate = h.svc.admit(QualityPhase::Checking, None).unwrap();

    let scans = ask(&h, &folder_a, serde_json::json!({"section": "scans"})).unwrap();
    assert_eq!(scans["total"], 1);
    assert_eq!(scans["scans"][0]["id"], scan_a.as_str());
    assert_eq!(scans["scans"][0]["findingCount"], 5);
    assert!(scans["scans"][0].get("log").is_none() && scans["scans"][0].get("folder").is_none() && scans["scans"][0].get("config").is_none());
    let summary = ask(&h, &folder_a, serde_json::json!({"section": "summary", "scanId": scan_a})).unwrap();
    assert_eq!(summary["findingsBySeverity"]["high"], 2);
    assert_eq!(summary["findingsBySeverity"]["low"], 3);
    assert_eq!(summary["findingsByTool"]["opengrep"], 3);
    assert_eq!(summary["hotspots"], 3);
    let first = ask(&h, &folder_a, serde_json::json!({"section": "findings", "scanId": scan_a, "limit": 2})).unwrap();
    assert_eq!((first["total"].as_u64(), first["count"].as_u64(), first["nextOffset"].as_u64()), (Some(5), Some(2), Some(2)));
    let last = ask(&h, &folder_a, serde_json::json!({"section": "findings", "scanId": scan_a, "offset": 4, "limit": 2})).unwrap();
    assert_eq!((last["count"].as_u64(), last["nextOffset"].is_null()), (Some(1), true));
    let tool = ask(&h, &folder_a, serde_json::json!({"section": "findings", "scanId": scan_a, "tool": "ruff"})).unwrap();
    assert_eq!(tool["total"], 2);
    let high = ask(&h, &folder_a, serde_json::json!({"section": "findings", "scanId": scan_a, "severity": "high", "tool": "ruff"})).unwrap();
    assert_eq!((high["total"].as_u64(), high["items"][0]["id"].as_str()), (Some(1), Some("f1")));
    let artifact = ask(&h, &folder_a, serde_json::json!({"section": "artifact", "scanId": scan_a, "reportName": "t.json"})).unwrap();
    assert_eq!((artifact["text"].as_str(), artifact["nextOffset"].is_null()), (Some("{\"ok\":true}"), true));

    // Another repository's scan is denied for every section, whichever way it is addressed.
    for (section, extra) in [("summary", serde_json::json!({})), ("findings", serde_json::json!({})), ("artifact", serde_json::json!({"reportName": "t.json"}))] {
        let mut request = serde_json::json!({"section": section, "scanId": scan_b});
        request.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        assert_eq!(ask(&h, &folder_a, request).unwrap_err(), "That scan does not belong to this repository", "{section}");
    }
    assert_eq!(ask(&h, &folder_b, serde_json::json!({"section": "artifact", "scanId": scan_a, "reportName": "t.json"})).unwrap_err(), "That scan does not belong to this repository");
    assert!(ask(&h, &folder_a, serde_json::json!({"section": "summary", "scanId": "not-a-uuid"})).is_err());
    assert!(ask(&h, &folder_a, serde_json::json!({"section": "summary"})).is_err());

    // Undeclared names, paths, unknown fields and fields foreign to the section are refused.
    for name in ["other.json", "../request.json", "..\\request.json", "request.json", ""] {
        assert!(ask(&h, &folder_a, serde_json::json!({"section": "artifact", "scanId": scan_a, "reportName": name})).is_err(), "{name}");
    }
    for request in [
        serde_json::json!({"section": "scans", "scanId": scan_a}),
        serde_json::json!({"section": "summary", "scanId": scan_a, "limit": 1}),
        serde_json::json!({"section": "findings", "scanId": scan_a, "reportName": "t.json"}),
        serde_json::json!({"section": "findings", "scanId": scan_a, "limit": 0}),
        serde_json::json!({"section": "findings", "scanId": scan_a, "limit": 101}),
        serde_json::json!({"section": "findings", "scanId": scan_a, "severity": "bogus"}),
        serde_json::json!({"section": "scans", "folder": folder_b}),
        serde_json::json!({"section": "scans", "path": "C:/x", "instanceId": "x", "export": true}),
        serde_json::json!({"section": "exports"}),
    ] {
        assert!(ask(&h, &folder_a, request.clone()).is_err(), "{request}");
    }

    // Nothing was admitted, probed, started or created, and the running operation was not disturbed.
    let state = h.svc.state();
    assert!(state.busy && state.phase == QualityPhase::Checking);
    gate.finish(QualityPhase::Idle, None);
    assert!(h.desktop.ops().is_empty() && h.podman.ops().is_empty());
    assert!(!h.svc.state().busy);
}

#[test]
fn agent_reader_pages_findings_under_the_output_cap_and_refuses_an_unreturnable_one() {
    let h = harness();
    let big: Vec<_> = (0..30).map(|n| quality_finding(n, Severity::Medium, "ruff", "x".repeat(20_000))).collect();
    let (folder, id) = saved_quality_scan(&h, "paged", big, "");
    let page = ask(&h, &folder, serde_json::json!({"section": "findings", "scanId": id, "limit": 100})).unwrap();
    let count = page["count"].as_u64().unwrap();
    assert!(count > 0 && count < 30 && page.to_string().len() <= 256 * 1024);
    assert_eq!(page["nextOffset"].as_u64(), Some(count));
    let mut seen = count;
    let mut next = page["nextOffset"].as_u64();
    while let Some(offset) = next {
        let more = ask(&h, &folder, serde_json::json!({"section": "findings", "scanId": id, "offset": offset, "limit": 100})).unwrap();
        seen += more["count"].as_u64().unwrap();
        next = more["nextOffset"].as_u64();
    }
    assert_eq!(seen, 30, "continuation reaches every finding exactly once");

    let (folder, id) = saved_quality_scan(&h, "oversized", vec![quality_finding(0, Severity::High, "ruff", "y".repeat(300_000))], "");
    let error = ask(&h, &folder, serde_json::json!({"section": "findings", "scanId": id})).unwrap_err();
    assert!(error.contains("cannot be returned whole"), "{error}");
}

#[test]
fn agent_reader_artifact_slices_are_utf8_safe_and_continue_to_the_tail() {
    let h = harness();
    let text = "é𝄞a".repeat(15_000);
    let (folder, id) = saved_quality_scan(&h, "slices", vec![], &text);
    let mut offset = 0u64;
    let (mut joined, mut calls) = (String::new(), 0);
    loop {
        let slice = ask(&h, &folder, serde_json::json!({"section": "artifact", "scanId": id, "reportName": "t.json", "offset": offset})).unwrap();
        assert_eq!(slice["totalBytes"].as_u64(), Some(text.len() as u64));
        assert!(!slice["text"].as_str().unwrap().contains('\u{fffd}'), "a cut never splits a character");
        joined.push_str(slice["text"].as_str().unwrap());
        calls += 1;
        match slice["nextOffset"].as_u64() {
            Some(next) => {
                assert!(next > offset);
                offset = next;
            }
            None => break,
        }
    }
    assert!(calls >= 2);
    assert_eq!(joined, text, "the tail is reachable");
    assert!(ask(&h, &folder, serde_json::json!({"section": "artifact", "scanId": id, "reportName": "t.json", "offset": 1})).unwrap_err().contains("character boundary"));
    assert!(ask(&h, &folder, serde_json::json!({"section": "artifact", "scanId": id, "reportName": "t.json", "offset": text.len() as u64 + 1})).is_err());
    let end = ask(&h, &folder, serde_json::json!({"section": "artifact", "scanId": id, "reportName": "t.json", "offset": text.len() as u64})).unwrap();
    assert_eq!((end["text"].as_str(), end["nextOffset"].is_null()), (Some(""), true));

    // Characters that JSON escapes expand a slice; it shrinks so the reply fits the cap and nothing is lost.
    let control = "\u{1}".repeat(70_000);
    let (folder, id) = saved_quality_scan(&h, "controls", vec![], &control);
    let (mut offset, mut joined) = (0u64, String::new());
    loop {
        let slice = ask(&h, &folder, serde_json::json!({"section": "artifact", "scanId": id, "reportName": "t.json", "offset": offset})).unwrap();
        assert!(slice.to_string().len() <= 256 * 1024);
        joined.push_str(slice["text"].as_str().unwrap());
        match slice["nextOffset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(joined, control);
    assert!(h.desktop.ops().is_empty() && h.podman.ops().is_empty());
}

#[test]
fn human_report_preview_is_unchanged_by_the_slice_reader() {
    let h = harness();
    let (_, id) = saved_quality_scan(&h, "preview", vec![], "{\"ok\":true}");
    assert_eq!(h.svc.read_report(&id, "t.json").unwrap().text, "{\"ok\":true}");
    let slice = h.svc.read_report_slice(&id, "t.json", 0, 1 << 20).unwrap();
    assert_eq!((slice.text.as_str(), slice.next_offset, slice.total), ("{\"ok\":true}", None, 11));
    assert!(h.svc.read_report_slice(&id, "other.json", 0, 10).is_err());
}

#[test]
fn project_requirements_use_only_the_read_only_probe_and_start_nothing() {
    use crate::project_requirements::{analyze, DetectedState, SystemHost, ToolProbe};
    let h = harness();
    let folder = h.dir.join("requirements-project");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(folder.join("README.md"), "Uses Podman and WSL.\n").unwrap();
    let before = serde_json::to_value(h.svc.state()).unwrap();
    let host = SystemHost::new(h.svc.clone()).with_tool(|_| ToolProbe::Missing);
    let result = analyze(&folder, &host).unwrap();
    // The service's probe seam answered (installed = true), so the engine is found without any engine call.
    let engine = result.items.iter().find(|i| i.id == "containerEngine:engine").unwrap();
    assert_eq!(engine.detected.state, DetectedState::Found);
    assert!(result.items.iter().any(|i| i.id == "containerEngine:podman") && result.items.iter().any(|i| i.id == "osFeature:wsl"));
    assert!(h.desktop.ops().is_empty() && h.podman.ops().is_empty(), "analysis must not touch any engine");
    assert_eq!(serde_json::to_value(h.svc.state()).unwrap(), before, "analysis must not change the service state");
    let _ = std::fs::remove_dir_all(&h.dir);
}
