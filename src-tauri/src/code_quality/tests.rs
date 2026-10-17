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
        Ok(EngineProbe { fingerprint: format!("fp-{}", self.runtime.key()), os: "linux".into(), memory_bytes: 8 << 30, max_map_count: Some(1 << 20), file_max: Some(1 << 20) })
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

/// Loopback SonarQube: `UP` status, token validation (`bad` tokens are invalid).
fn fake_sonar() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                let n = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..n]).to_string();
                let body = if request.contains("/api/system/status") {
                    r#"{"id":"x","version":"26.9","status":"UP"}"#.to_string()
                } else if request.contains("/api/authentication/validate") {
                    let bad = request.contains("YmFkOg=="); // base64("bad:")
                    format!(r#"{{"valid":{}}}"#, !bad)
                } else {
                    "{}".to_string()
                };
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            });
        }
    });
    port
}

struct Harness {
    svc: Arc<CodeQualityService>,
    desktop: Arc<FakeEngine>,
    podman: Arc<FakeEngine>,
    dir: PathBuf,
    port: u16,
}

impl Harness {
    fn config(&self, runtime: QualityRuntime) -> QualityConfig {
        QualityConfig { runtime, community_port: self.port, ..QualityConfig::default() }
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
    let svc = CodeQualityService::with_parts(dir.clone(), Arc::new(FakeFactory(map)), Arc::new(MemorySecrets::default()), Ok(Some(ProcessLock::unowned())));
    Harness { svc, desktop, podman, dir, port: fake_sonar() }
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
fn a_second_process_is_read_only() {
    let dir = std::env::temp_dir().join(format!("omp-cq-ro-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let svc = CodeQualityService::with_parts(dir.clone(), Arc::new(FakeFactory(HashMap::new())), Arc::new(MemorySecrets::default()), Ok(None));
    assert!(svc.configure(QualityConfig::default(), 0, None).unwrap_err().contains("Another OMP Manager process"));
    assert!(svc.state().error.is_some());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unreadable_state_is_never_reset() {
    let dir = std::env::temp_dir().join(format!("omp-cq-bad-{}", uuid::Uuid::new_v4()));
    let root = dir.join("code-quality");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("resources.json"), b"{ nope").unwrap();
    let svc = CodeQualityService::with_parts(dir.clone(), Arc::new(FakeFactory(HashMap::new())), Arc::new(MemorySecrets::default()), Ok(Some(ProcessLock::unowned())));
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
