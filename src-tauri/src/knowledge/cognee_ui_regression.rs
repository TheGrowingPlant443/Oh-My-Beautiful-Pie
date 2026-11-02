//! Lifecycle of the explicit Cognee UI start against isolated installed-layout fixtures: a throwaway Cognee install, a
//! throwaway `cognee-frontend` whose `next` entrypoint is a small Node program that serves `/api/runtime-config`, and
//! loopback-only sockets. Nothing here touches the real Cognee, its stores, its ports or the network; the real Node.js
//! executable runs only the fixture. Windows x64 only (the supported layout).
use super::cognee_ui::{native, verify_backends, Backend, Hooks, Listener, Row};
use super::integrations::Environment;
use super::{
    fsio, CogneeOpenResult, CogneeOpenState, CogneeUiCapability, CogneeUiState, KnowledgeService,
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FIXTURE_NEXT: &str = r#"
const fs = require('fs');
const http = require('http');
const cp = require('child_process');
const argv = process.argv.slice(1);
const port = Number(argv[argv.indexOf('--port') + 1]);
const host = argv[argv.indexOf('--hostname') + 1];
const mode = (() => { try { return fs.readFileSync('.fixture-mode', 'utf8').trim(); } catch { return 'serve'; } })();
if (!process.env.FIXTURE_CHILD) {
  const names = ['CI', 'NEXT_TELEMETRY_DISABLED', 'NEXT_PUBLIC_IS_CLOUD_ENVIRONMENT', 'COGNEE_BACKEND_URL', 'NEXT_IGNORE_INCORRECT_LOCKFILE', 'NEXT_DISABLE_SWC_WASM',
    '__INTERNAL_CUSTOM_TURBOPACK_BINDINGS', 'NEXT_TEST_WASM', 'NEXT_TEST_NATIVE_DIR', 'NEXT_TEST_NATIVE_IGNORE_LOCAL_INSTALL', 'NEXT_PUBLIC_LOCAL_API_URL'];
  const env = {};
  for (const name of names) env[name] = process.env[name] === undefined ? null : process.env[name];
  fs.appendFileSync('.fixture-starts.jsonl', JSON.stringify({ argv, cwd: process.cwd(), pid: process.pid, env }) + '\n');
}
if (mode === 'exit') { console.error('fixture refuses to start'); process.exit(3); }
if (mode === 'never') { setInterval(() => {}, 1000); return; }
if (mode === 'fork' && !process.env.FIXTURE_CHILD) {
  cp.spawn(process.execPath, process.argv.slice(1), { env: { ...process.env, FIXTURE_CHILD: '1' }, stdio: 'ignore' });
  setInterval(() => {}, 1000);
  return;
}
const server = http.createServer((req, res) => {
  if (req.url !== '/api/runtime-config' || req.headers.host !== 'localhost:' + port) { res.statusCode = 404; res.end('no'); return; }
  if (mode === 'redirect') { res.statusCode = 302; res.setHeader('Location', '/elsewhere'); res.end(); return; }
  res.setHeader('Content-Type', 'application/json');
  res.end(JSON.stringify({ backendUrl: mode === 'wrong' ? 'http://localhost:1' : process.env.COGNEE_BACKEND_URL }));
});
server.listen(port, host);
"#;

struct Layout(PathBuf);

impl Layout {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("omp-cognee-ui-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(path.join("home")).unwrap();
        Self(crate::setup::canonical(&path).unwrap())
    }

    fn cognee(&self) -> PathBuf {
        self.0.join("cognee")
    }
    fn site(&self) -> PathBuf {
        self.cognee().join("venv/Lib/site-packages")
    }
    fn frontend(&self) -> PathBuf {
        self.site().join("cognee-frontend")
    }

    fn install_cognee(&self) {
        let cli = self.cognee().join("venv/Scripts/cognee-cli.exe");
        fs::create_dir_all(cli.parent().unwrap()).unwrap();
        fs::write(cli, b"not a real executable").unwrap();
        fs::create_dir_all(self.site().join("cognee-1.6.4.dist-info")).unwrap();
    }

    fn write(&self, folder: &Path, relative: &str, text: &str) {
        let target = folder.join(relative);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, text).unwrap();
    }

    /// The complete supported development layout at `folder`.
    fn install_frontend_at(&self, folder: &Path, next_version: &str) {
        self.write(
            folder,
            "package.json",
            r#"{"name":"cognee-frontend","version":"1.0.0"}"#,
        );
        self.write(
            folder,
            "node_modules/next/package.json",
            &format!(
                r#"{{"name":"next","version":"{next_version}","engines":{{"node":">=20.9.0"}}}}"#
            ),
        );
        self.write(folder, "node_modules/next/dist/bin/next", FIXTURE_NEXT);
        self.write(
            folder,
            "node_modules/typescript/lib/typescript.js",
            "// fixture",
        );
        self.write(folder, "node_modules/@types/react/index.d.ts", "// fixture");
        self.write(folder, "node_modules/@types/node/index.d.ts", "// fixture");
        self.write(
            folder,
            "node_modules/@next/swc-win32-x64-msvc/package.json",
            &format!(r#"{{"name":"@next/swc-win32-x64-msvc","version":"{next_version}"}}"#),
        );
        self.write(
            folder,
            "node_modules/@next/swc-win32-x64-msvc/next-swc.win32-x64-msvc.node",
            "fixture binding",
        );
    }

    fn install_frontend(&self) {
        self.install_frontend_at(&self.frontend(), "16.3.4");
    }

    fn service(&self, node: Option<PathBuf>) -> Arc<KnowledgeService> {
        KnowledgeService::with_environment(
            self.0.join("config"),
            self.0.join("home"),
            Environment {
                cognee_home: Some(self.cognee()),
                cognee_node: node,
                ..Environment::isolated()
            },
        )
    }

    fn starts(&self) -> Vec<Value> {
        fs::read_to_string(self.frontend().join(".fixture-starts.jsonl"))
            .map(|text| {
                text.lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn mode(&self, mode: &str) {
        fs::write(self.frontend().join(".fixture-mode"), mode).unwrap();
    }
}

impl Drop for Layout {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Retires whatever the service started even when an assertion fails.
struct Cleanup(Arc<KnowledgeService>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

fn node() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|entry| entry.is_absolute())
        .map(|entry| entry.join("node.exe"))
        .find(|candidate| candidate.is_file())
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn gone(pid: u64) -> bool {
    crate::process_tree::process_creation_time(pid as u32).is_none()
}

#[derive(Clone, Default)]
struct Probe {
    opened: Arc<Mutex<Vec<String>>>,
    discovered: Arc<AtomicUsize>,
    refuse_open: Arc<AtomicBool>,
}

impl Probe {
    fn hooks(
        &self,
        frontend_port: u16,
        backends: Vec<Backend>,
        startup_timeout: Duration,
    ) -> Hooks {
        let (opened, discovered, refuse) = (
            self.opened.clone(),
            self.discovered.clone(),
            self.refuse_open.clone(),
        );
        Hooks {
            frontend_port,
            startup_timeout,
            discover: Box::new(move |_| {
                discovered.fetch_add(1, Ordering::SeqCst);
                Ok(backends.clone())
            }),
            opener: Box::new(move |url| {
                if refuse.load(Ordering::SeqCst) {
                    return Err("no browser".into());
                }
                opened.lock().push(url.to_string());
                Ok(())
            }),
        }
    }

    fn opened(&self) -> Vec<String> {
        self.opened.lock().clone()
    }
}

fn backend(port: u16) -> Backend {
    Backend {
        port,
        pid: std::process::id(),
        address: IpAddr::V4(Ipv4Addr::LOCALHOST),
    }
}

/// A started fixture service: installed Cognee, a complete frontend, hooks pointing at a free port. The cleanup field
/// comes first so the frontend is retired before the fixture folder is removed.
struct Rig {
    _cleanup: Cleanup,
    layout: Layout,
    service: Arc<KnowledgeService>,
    probe: Probe,
    port: u16,
}

impl Rig {
    fn new(startup_timeout: Duration) -> Option<Self> {
        let Some(node) = node() else {
            eprintln!("skipped: Node.js is not on PATH");
            return None;
        };
        let layout = Layout::new();
        layout.install_cognee();
        layout.install_frontend();
        let service = layout.service(Some(node));
        let probe = Probe::default();
        let port = free_port();
        service
            .cognee_ui
            .set_hooks(probe.hooks(port, vec![backend(18_000)], startup_timeout));
        Some(Self {
            _cleanup: Cleanup(service.clone()),
            layout,
            service,
            probe,
            port,
        })
    }

    fn url(&self) -> String {
        format!("http://localhost:{}", self.port)
    }
}

// ---------- passive inspection ----------

#[test]
fn an_installed_sdk_without_a_frontend_is_reported_and_open_spawns_and_contacts_nothing() {
    let layout = Layout::new();
    layout.install_cognee();
    let service = layout.service(None);
    let probe = Probe::default();
    service.cognee_ui.set_hooks(probe.hooks(
        free_port(),
        vec![backend(18_000)],
        Duration::from_secs(1),
    ));
    let cognee = service.get_knowledge_capabilities(None).unwrap().cognee;
    assert_eq!(
        (cognee.installed, cognee.ui.state),
        (Some(true), CogneeUiState::MissingFrontend)
    );
    assert!(cognee
        .ui
        .message
        .unwrap()
        .contains("never downloads or installs"));
    let opened = service.open_cognee_ui();
    assert_eq!(
        (opened.state, opened.url),
        (CogneeOpenState::MissingComponent, None)
    );
    assert_eq!(
        (
            probe.discovered.load(Ordering::SeqCst),
            probe.opened().len()
        ),
        (0, 0),
        "no backend query and no browser"
    );
    assert!(
        !layout.0.join("home/.cognee").exists() && !layout.0.join("config").exists(),
        "no cache folder, no settings"
    );
}

#[test]
fn missing_dependencies_wrong_versions_and_wrong_packages_refuse_before_any_process() {
    let layout = Layout::new();
    layout.install_cognee();
    let service = layout.service(node());
    let probe = Probe::default();
    service.cognee_ui.set_hooks(probe.hooks(
        free_port(),
        vec![backend(18_000)],
        Duration::from_secs(1),
    ));
    let state = |service: &KnowledgeService| {
        let ui = service.probe_cognee().ui;
        (ui.state, ui.message.unwrap_or_default())
    };
    // Package present, no node_modules.
    layout.write(
        &layout.frontend(),
        "package.json",
        r#"{"name":"cognee-frontend"}"#,
    );
    let (kind, message) = state(&service);
    assert_eq!(kind, CogneeUiState::MissingDependencies, "{message}");
    // Not the cognee-frontend package.
    layout.write(
        &layout.frontend(),
        "package.json",
        r#"{"name":"something-else"}"#,
    );
    assert_eq!(state(&service).0, CogneeUiState::Unsupported);
    // Complete layout with a different Next version.
    layout.install_frontend_at(&layout.frontend(), "16.3.3");
    let (kind, message) = state(&service);
    assert_eq!(kind, CogneeUiState::Unsupported, "{message}");
    assert!(message.contains("16.3.3") && message.contains("16.3.4"));
    // Right Next, but the native binding is another version.
    layout.install_frontend();
    layout.write(
        &layout.frontend(),
        "node_modules/@next/swc-win32-x64-msvc/package.json",
        r#"{"version":"16.3.3"}"#,
    );
    assert_eq!(state(&service).0, CogneeUiState::Unsupported);
    // Each required package missing is named, and not downloaded.
    layout.install_frontend();
    for (file, label) in [
        ("node_modules/typescript/lib/typescript.js", "typescript"),
        ("node_modules/@types/react/index.d.ts", "@types/react"),
        ("node_modules/@types/node/index.d.ts", "@types/node"),
        (
            "node_modules/@next/swc-win32-x64-msvc/next-swc.win32-x64-msvc.node",
            "native binding",
        ),
        ("node_modules/next/dist/bin/next", "next/dist/bin/next"),
    ] {
        let path = layout.frontend().join(file);
        let kept = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let (kind, message) = state(&service);
        assert_eq!(
            kind,
            CogneeUiState::MissingDependencies,
            "{file}: {message}"
        );
        assert!(message.contains(label), "{file}: {message}");
        assert_eq!(
            service.open_cognee_ui().state,
            CogneeOpenState::MissingComponent,
            "{file}"
        );
        fs::write(path, kept).unwrap();
    }
    // A standalone server.js alone is never started.
    let standalone = Layout::new();
    standalone.install_cognee();
    standalone.write(
        &standalone.frontend(),
        "package.json",
        r#"{"name":"cognee-frontend"}"#,
    );
    standalone.write(
        &standalone.frontend(),
        ".next/standalone/server.js",
        "// not started",
    );
    assert_eq!(
        standalone.service(node()).probe_cognee().ui.state,
        CogneeUiState::MissingDependencies
    );
    assert_eq!(
        (
            probe.discovered.load(Ordering::SeqCst),
            probe.opened().len()
        ),
        (0, 0)
    );
    assert!(layout.starts().is_empty());
}

#[test]
fn a_complete_layout_is_ready_in_every_supported_location_and_a_node_without_version_is_unreadable()
{
    let Some(node) = node() else {
        return eprintln!("skipped: Node.js is not on PATH");
    };
    let layout = Layout::new();
    layout.install_cognee();
    layout.install_frontend();
    assert_eq!(
        layout.service(Some(node.clone())).probe_cognee().ui,
        CogneeUiCapability {
            state: CogneeUiState::Ready,
            message: None
        }
    );
    // The download-cache location Cognee itself uses, found read-only (it is not created by probing).
    let cached = Layout::new();
    cached.install_cognee();
    cached.install_frontend_at(&cached.0.join("home/.cognee/ui-cache/frontend"), "16.3.4");
    assert_eq!(
        cached.service(Some(node)).probe_cognee().ui.state,
        CogneeUiState::Ready
    );
    // Node.js that cannot be identified is a diagnostic, not a guess.
    let fake = layout.0.join("fake-node.exe");
    fs::write(&fake, b"not a real program").unwrap();
    let unreadable = layout.service(Some(fake)).probe_cognee().ui;
    assert_eq!(
        unreadable.state,
        CogneeUiState::Unreadable,
        "{:?}",
        unreadable.message
    );
    let absent = layout
        .service(Some(layout.0.join("no-node.exe")))
        .probe_cognee()
        .ui;
    assert_eq!(absent.state, CogneeUiState::MissingDependencies);
    assert!(absent.message.unwrap().contains("Node.js"));
}

// ---------- lifecycle ----------

#[test]
fn explicit_open_starts_the_exact_command_once_with_the_fail_closed_environment_and_reuses_it() {
    let Some(rig) = Rig::new(Duration::from_secs(60)) else {
        return;
    };
    // Inherited test variables must not reach the frontend.
    for name in [
        "NEXT_TEST_WASM",
        "NEXT_TEST_NATIVE_DIR",
        "NEXT_TEST_NATIVE_IGNORE_LOCAL_INSTALL",
        "NEXT_PUBLIC_LOCAL_API_URL",
    ] {
        std::env::set_var(name, "inherited");
    }
    let first = rig.service.open_cognee_ui();
    for name in [
        "NEXT_TEST_WASM",
        "NEXT_TEST_NATIVE_DIR",
        "NEXT_TEST_NATIVE_IGNORE_LOCAL_INSTALL",
        "NEXT_PUBLIC_LOCAL_API_URL",
    ] {
        std::env::remove_var(name);
    }
    assert_eq!(
        (first.state, first.url.clone()),
        (CogneeOpenState::Opened, Some(rig.url())),
        "{:?}",
        first.message
    );
    let starts = rig.layout.starts();
    assert_eq!(starts.len(), 1);
    let entry = rig
        .layout
        .frontend()
        .join("node_modules/next/dist/bin/next");
    let argv = starts[0]["argv"].as_array().unwrap();
    assert_eq!(
        fsio::norm(Path::new(argv[0].as_str().unwrap())),
        fsio::norm(&entry),
        "the installed next entrypoint is run directly"
    );
    assert_eq!(
        argv[1..],
        [
            json!("dev"),
            json!("--turbopack"),
            json!("--hostname"),
            json!("localhost"),
            json!("--port"),
            json!(rig.port.to_string())
        ],
        "literal arguments: no npm, no shell"
    );
    assert_eq!(
        fsio::norm(Path::new(starts[0]["cwd"].as_str().unwrap())),
        fsio::norm(&rig.layout.frontend())
    );
    let env = &starts[0]["env"];
    let binding = rig
        .layout
        .frontend()
        .join("node_modules/@next/swc-win32-x64-msvc/next-swc.win32-x64-msvc.node");
    assert_eq!(env["CI"], "1");
    assert_eq!(env["NEXT_TELEMETRY_DISABLED"], "1");
    assert_eq!(env["NEXT_PUBLIC_IS_CLOUD_ENVIRONMENT"], "false");
    assert_eq!(env["COGNEE_BACKEND_URL"], "http://localhost:18000");
    assert_eq!(env["NEXT_IGNORE_INCORRECT_LOCKFILE"], "1");
    assert_eq!(env["NEXT_DISABLE_SWC_WASM"], "1");
    assert_eq!(
        fsio::norm(Path::new(
            env["__INTERNAL_CUSTOM_TURBOPACK_BINDINGS"]
                .as_str()
                .unwrap()
        )),
        fsio::norm(&binding)
    );
    for cleared in [
        "NEXT_TEST_WASM",
        "NEXT_TEST_NATIVE_DIR",
        "NEXT_TEST_NATIVE_IGNORE_LOCAL_INSTALL",
        "NEXT_PUBLIC_LOCAL_API_URL",
    ] {
        assert!(env[cleared].is_null(), "{cleared} must be cleared");
    }
    let pid = rig.service.cognee_ui.owned_pid().unwrap();
    assert_eq!(starts[0]["pid"].as_u64(), Some(u64::from(pid)));
    let second = rig.service.open_cognee_ui();
    assert_eq!(second.state, CogneeOpenState::Opened);
    assert_eq!(
        (rig.layout.starts().len(), rig.service.cognee_ui.owned_pid()),
        (1, Some(pid)),
        "the app's own verified frontend is reused, not restarted"
    );
    assert_eq!(rig.probe.opened(), vec![rig.url(), rig.url()]);
}

#[test]
fn concurrent_clicks_start_one_frontend() {
    let Some(rig) = Rig::new(Duration::from_secs(60)) else {
        return;
    };
    let answers: Vec<CogneeOpenResult> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| rig.service.open_cognee_ui()))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    assert!(
        answers
            .iter()
            .all(|answer| answer.state == CogneeOpenState::Opened),
        "{answers:?}"
    );
    assert_eq!(
        rig.layout.starts().len(),
        1,
        "single-flight: one process for all clicks"
    );
}

#[test]
fn a_listener_in_a_descendant_of_the_owned_process_counts_as_owned() {
    let Some(rig) = Rig::new(Duration::from_secs(60)) else {
        return;
    };
    rig.layout.mode("fork");
    let opened = rig.service.open_cognee_ui();
    assert_eq!(
        opened.state,
        CogneeOpenState::Opened,
        "{:?}",
        opened.message
    );
    let root = rig.service.cognee_ui.owned_pid().unwrap();
    let owners: Vec<u32> = native::tcp_listeners()
        .unwrap()
        .into_iter()
        .filter(|listener| listener.port == rig.port)
        .map(|listener| listener.pid)
        .collect();
    assert!(
        !owners.is_empty() && owners.iter().all(|pid| *pid != root),
        "the socket belongs to the forked child, not the root: {owners:?} root {root}"
    );
}

#[test]
fn a_foreign_listener_on_the_port_is_port_busy_untouched_and_nothing_is_spawned() {
    let Some(rig) = Rig::new(Duration::from_secs(60)) else {
        return;
    };
    let foreign = TcpListener::bind(("127.0.0.1", rig.port)).unwrap();
    let opened = rig.service.open_cognee_ui();
    assert_eq!(
        (opened.state, opened.url),
        (CogneeOpenState::PortBusy, None),
        "{:?}",
        opened.message
    );
    assert!(opened
        .message
        .unwrap()
        .contains(&std::process::id().to_string()));
    assert!(rig.layout.starts().is_empty(), "no frontend was started");
    assert!(rig.probe.opened().is_empty());
    assert!(
        TcpStream::connect_timeout(&foreign.local_addr().unwrap(), Duration::from_secs(2)).is_ok(),
        "the foreign listener still accepts"
    );
}

#[test]
fn a_missing_or_ambiguous_backend_is_unavailable_without_spawning_or_contacting_anything() {
    let Some(rig) = Rig::new(Duration::from_secs(60)) else {
        return;
    };
    let port = rig.port;
    for (backends, wanted) in [
        (vec![], "No running local Cognee backend"),
        (
            vec![backend(18_001), backend(18_002)],
            "Several Cognee backends",
        ),
    ] {
        rig.service
            .cognee_ui
            .set_hooks(rig.probe.hooks(port, backends, Duration::from_secs(60)));
        let opened = rig.service.open_cognee_ui();
        assert_eq!(opened.state, CogneeOpenState::BackendUnavailable);
        assert!(opened.message.unwrap().contains(wanted));
    }
    let failing = Hooks {
        frontend_port: port,
        startup_timeout: Duration::from_secs(1),
        discover: Box::new(|_| Err("process list denied".into())),
        opener: Box::new(|_| Ok(())),
    };
    rig.service.cognee_ui.set_hooks(failing);
    assert_eq!(
        rig.service.open_cognee_ui().state,
        CogneeOpenState::BackendUnavailable
    );
    assert!(rig.layout.starts().is_empty() && rig.probe.opened().is_empty());
    // Port 8000 wins when several are verified.
    rig.service.cognee_ui.set_hooks(rig.probe.hooks(
        port,
        vec![backend(18_001), backend(8000)],
        Duration::from_secs(60),
    ));
    assert_eq!(rig.service.open_cognee_ui().state, CogneeOpenState::Opened);
    assert_eq!(
        rig.layout.starts()[0]["env"]["COGNEE_BACKEND_URL"],
        "http://localhost:8000"
    );
}

#[test]
fn a_frontend_that_names_another_backend_or_redirects_is_stopped_not_opened() {
    let Some(rig) = Rig::new(Duration::from_secs(60)) else {
        return;
    };
    for (mode, wanted) in [("wrong", "reports backend"), ("redirect", "redirect")] {
        rig.layout.mode(mode);
        let opened = rig.service.open_cognee_ui();
        assert_eq!(
            opened.state,
            CogneeOpenState::StartingFailed,
            "{mode}: {:?}",
            opened.message
        );
        assert!(opened.message.unwrap().contains(wanted), "{mode}");
        assert_eq!(rig.service.cognee_ui.owned_pid(), None);
        let pid = rig.layout.starts().last().unwrap()["pid"].as_u64().unwrap();
        assert!(gone(pid), "{mode}: the process this app started is retired");
    }
    assert!(rig.probe.opened().is_empty());
    // After failures the same app can start a clean frontend (no replacement over a live job was needed).
    rig.layout.mode("serve");
    assert_eq!(rig.service.open_cognee_ui().state, CogneeOpenState::Opened);
}

#[test]
fn an_early_exit_and_a_startup_timeout_are_reported_and_retire_only_the_started_process() {
    let Some(rig) = Rig::new(Duration::from_millis(1_500)) else {
        return;
    };
    rig.layout.mode("exit");
    let exited = rig.service.open_cognee_ui();
    assert_eq!(exited.state, CogneeOpenState::StartingFailed);
    assert!(exited.message.unwrap().contains("exited with code 3"));
    rig.layout.mode("never");
    let timed_out = rig.service.open_cognee_ui();
    assert_eq!(
        timed_out.state,
        CogneeOpenState::ReadinessTimeout,
        "{:?}",
        timed_out.message
    );
    let pid = rig.layout.starts().last().unwrap()["pid"].as_u64().unwrap();
    assert!(gone(pid) && rig.service.cognee_ui.owned_pid().is_none());
    assert!(rig.probe.opened().is_empty());
}

#[test]
fn a_browser_that_cannot_open_keeps_the_ready_frontend_for_the_next_click() {
    let Some(rig) = Rig::new(Duration::from_secs(60)) else {
        return;
    };
    rig.probe.refuse_open.store(true, Ordering::SeqCst);
    let failed = rig.service.open_cognee_ui();
    assert_eq!(
        (failed.state, failed.url),
        (CogneeOpenState::OpenFailed, Some(rig.url()))
    );
    let pid = rig
        .service
        .cognee_ui
        .owned_pid()
        .expect("the ready child is retained");
    rig.probe.refuse_open.store(false, Ordering::SeqCst);
    assert_eq!(rig.service.open_cognee_ui().state, CogneeOpenState::Opened);
    assert_eq!(
        (rig.service.cognee_ui.owned_pid(), rig.layout.starts().len()),
        (Some(pid), 1)
    );
}

#[test]
fn a_different_backend_replaces_only_the_frontend_this_app_started() {
    let Some(rig) = Rig::new(Duration::from_secs(60)) else {
        return;
    };
    assert_eq!(rig.service.open_cognee_ui().state, CogneeOpenState::Opened);
    let old = rig.service.cognee_ui.owned_pid().unwrap();
    rig.service.cognee_ui.set_hooks(rig.probe.hooks(
        rig.port,
        vec![backend(18_001)],
        Duration::from_secs(60),
    ));
    assert_eq!(rig.service.open_cognee_ui().state, CogneeOpenState::Opened);
    let new = rig.service.cognee_ui.owned_pid().unwrap();
    assert_ne!(old, new);
    assert!(gone(u64::from(old)));
    let starts = rig.layout.starts();
    assert_eq!(
        (
            starts.len(),
            starts[1]["env"]["COGNEE_BACKEND_URL"].as_str()
        ),
        (2, Some("http://localhost:18001"))
    );
}

#[test]
fn shutdown_during_start_cancels_without_a_late_browser_and_shutdown_retires_the_owned_frontend() {
    let Some(rig) = Rig::new(Duration::from_secs(60)) else {
        return;
    };
    rig.layout.mode("never");
    let answer = std::thread::scope(|scope| {
        let handle = scope.spawn(|| rig.service.open_cognee_ui());
        wait_for(
            "the frontend process to start",
            Duration::from_secs(30),
            || !rig.layout.starts().is_empty(),
        );
        rig.service.shutdown();
        handle.join().unwrap()
    });
    assert_eq!(
        answer.state,
        CogneeOpenState::StartingFailed,
        "{:?}",
        answer.message
    );
    assert!(rig.probe.opened().is_empty(), "no browser after shutdown");
    assert!(gone(rig.layout.starts()[0]["pid"].as_u64().unwrap()));
    assert_eq!(
        rig.service.open_cognee_ui().state,
        CogneeOpenState::StartingFailed,
        "nothing starts after shutdown"
    );

    // A ready frontend is retired by shutdown and its port is released.
    let Some(ready) = Rig::new(Duration::from_secs(60)) else {
        return;
    };
    assert_eq!(
        ready.service.open_cognee_ui().state,
        CogneeOpenState::Opened
    );
    let pid = ready.service.cognee_ui.owned_pid().unwrap();
    ready.service.shutdown();
    assert!(gone(u64::from(pid)));
    wait_for("the port to be released", Duration::from_secs(5), || {
        TcpStream::connect(("localhost", ready.port)).is_err()
    });
}

// ---------- operating-system tables and identification ----------

#[test]
fn the_listener_table_reports_literal_ports_pids_and_both_families() {
    let v4 = TcpListener::bind("127.0.0.1:0").unwrap();
    let v6 = TcpListener::bind("[::1]:0").ok();
    let listeners = native::tcp_listeners().unwrap();
    let me = std::process::id();
    assert!(listeners.contains(&Listener {
        address: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: v4.local_addr().unwrap().port(),
        pid: me
    }));
    if let Some(v6) = v6 {
        assert!(listeners.contains(&Listener {
            address: "::1".parse().unwrap(),
            port: v6.local_addr().unwrap().port(),
            pid: me
        }));
    }
    let unused = free_port();
    assert!(!listeners.iter().any(|listener| listener.port == unused));
}

#[test]
fn table_rows_use_network_byte_order_and_inconsistent_counts_are_rejected() {
    let mut v4 = 1u32.to_le_bytes().to_vec();
    v4.extend(2u32.to_le_bytes());
    v4.extend([127, 0, 0, 1]);
    v4.extend([0x0b, 0xb8, 0, 0]);
    v4.extend([0u8; 8]);
    v4.extend(4242u32.to_le_bytes());
    assert_eq!(
        native::parse_v4(&v4).unwrap(),
        vec![Listener {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 3000,
            pid: 4242
        }]
    );
    let mut v6 = 1u32.to_le_bytes().to_vec();
    let mut loopback = [0u8; 16];
    loopback[15] = 1;
    v6.extend(loopback);
    v6.extend(7u32.to_le_bytes());
    v6.extend([0x1f, 0x40, 0, 0]);
    v6.extend([0u8; 16 + 4 + 4]);
    v6.extend(2u32.to_le_bytes());
    v6.extend(99u32.to_le_bytes());
    assert_eq!(
        native::parse_v6(&v6).unwrap(),
        vec![Listener {
            address: "::1".parse().unwrap(),
            port: 8000,
            pid: 99
        }]
    );
    assert!(
        native::parse_v4(&u32::MAX.to_le_bytes()).is_err()
            && native::parse_v6(&v6[..v6.len() - 1]).is_err()
            && native::parse_v4(&[1]).is_err()
    );
}

#[test]
fn only_the_installed_uvicorn_app_with_an_unchanged_process_owning_a_loopback_listener_is_a_backend(
) {
    let python = Path::new("C:/Cognee/venv/Scripts/python.exe");
    let command =
        |python: &str, tail: &str| format!("\"{python}\" -m uvicorn cognee.api.client:app {tail}");
    let row = |pid: u32, created: u64, exe: &str, cmd: String| Row {
        pid,
        created,
        exe: exe.into(),
        cmd,
    };
    let installed = "C:\\Cognee\\venv\\Scripts\\python.exe";
    let rows = vec![
        // The venv launcher and the base interpreter it starts: the listener belongs to the child.
        row(
            10,
            1_000_000,
            installed,
            command(installed, "--host localhost --port 8000"),
        ),
        row(
            11,
            1_000_500,
            "C:\\Python\\python.exe",
            command(installed, "--host localhost --port 8000"),
        ),
        row(
            12,
            2_000_000,
            "C:\\Python\\python.exe",
            command(installed, "--host 0.0.0.0 --port 8001"),
        ),
        row(
            13,
            3_000_000,
            "C:\\Other\\python.exe",
            command("C:\\Other\\python.exe", "--port 8002"),
        ),
        row(
            14,
            4_000_000,
            "C:\\Python\\python.exe",
            command(installed, "--port 8003"),
        ),
        row(
            15,
            5_000_000,
            "C:\\Python\\python.exe",
            command(installed, "--port 8004"),
        ),
        row(
            16,
            6_000_000,
            "C:\\Python\\python.exe",
            "\"C:\\Cognee\\venv\\Scripts\\python.exe\" -m uvicorn other.app:app --port 8005".into(),
        ),
        row(
            17,
            7_000_000,
            "C:\\Python\\python.exe",
            command(installed, "--port 8006 --app-dir C:\\evil"),
        ),
    ];
    let listen = |address: &str, port: u16, pid: u32| Listener {
        address: address.parse().unwrap(),
        port,
        pid,
    };
    let listeners = vec![
        listen("127.0.0.1", 8000, 11),
        listen("127.0.0.1", 8001, 12),
        listen("127.0.0.1", 8002, 13),
        listen("127.0.0.1", 8003, 14),
        listen("0.0.0.0", 8004, 15),
        listen("127.0.0.1", 8005, 16),
        listen("127.0.0.1", 8006, 17),
    ];
    // Process 14's id was reused after the query: its creation time differs.
    let creation = |pid: u32| match pid {
        10 => Some(1_000_000),
        11 => Some(1_000_509),
        12 => Some(2_000_000),
        13 => Some(3_000_000),
        14 => Some(4_999_999),
        15 => Some(5_000_000),
        16 => Some(6_000_000),
        17 => Some(7_000_000),
        _ => None,
    };
    let found = verify_backends(&rows, &listeners, python, &creation);
    assert_eq!(
        found,
        vec![Backend {
            port: 8000,
            pid: 11,
            address: "127.0.0.1".parse().unwrap()
        }]
    );
}
