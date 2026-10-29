//! Single-flight lifecycle of the app-owned Cognee frontend.
use super::components::{self, Backend, Components, FRONTEND_PORT, STARTUP_TIMEOUT};
use super::native;
use crate::knowledge::dto::{CogneeOpenResult, CogneeOpenState};
use crate::knowledge::fsio;
use crate::knowledge::integrations;
use crate::process_tree::{ContainedChild, ContainedSpec, StdioMode};
use parking_lot::{Condvar, Mutex, MutexGuard};
use std::fs;
use std::io::Read;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

const POLL: Duration = Duration::from_millis(250);
const CONFIG_TIMEOUT: Duration = Duration::from_secs(15);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CONFIG_BYTES: u64 = 16 * 1024;
const OUTPUT_TAIL: usize = 16 * 1024;
const SHUTDOWN_WAIT: Duration = Duration::from_secs(15);

/// A TCP listener from the operating system's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Listener {
    pub address: IpAddr,
    pub port: u16,
    pub pid: u32,
}

/// Everything environmental, so the lifecycle is exercised against isolated fixtures.
pub(crate) struct Hooks {
    pub frontend_port: u16,
    pub startup_timeout: Duration,
    /// Verified running backends for the given venv interpreter. Production: one bounded read-only CIM query
    /// of Python/Node candidates plus the listener tables.
    pub discover: Box<dyn Fn(&Path) -> Result<Vec<Backend>, String> + Send + Sync>,
    /// The system opener for the final loopback URL.
    pub opener: Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>,
}

impl Hooks {
    fn production() -> Self {
        Self {
            frontend_port: FRONTEND_PORT,
            startup_timeout: STARTUP_TIMEOUT,
            discover: Box::new(discover_backends),
            opener: Box::new(integrations::open_loopback_url),
        }
    }
}

/// The frontend this app started and still owns.
struct Owned {
    child: ContainedChild,
    backend_port: u16,
}

enum Phase {
    Idle,
    Starting,
    Ready(Arc<Owned>),
    Stopping,
    /// A frontend job that could not be proven empty. Never launch over it; retry retiring first.
    Blocked(Arc<Owned>),
}

enum Outcome {
    Idle,
    Ready(Arc<Owned>),
    Blocked(Arc<Owned>),
}

struct Inner {
    phase: Phase,
    finished: u64,
    last: Option<CogneeOpenResult>,
}

pub(crate) struct CogneeUi {
    inner: Mutex<Inner>,
    changed: Condvar,
    closing: AtomicBool,
    hooks: Mutex<Arc<Hooks>>,
}

enum Check {
    Ready,
    Pending(String),
    Busy(u32),
    Failed(String),
}

enum Config {
    Backend(Option<String>),
    Pending(String),
    Rejected(String),
}

impl CogneeUi {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                phase: Phase::Idle,
                finished: 0,
                last: None,
            }),
            changed: Condvar::new(),
            closing: AtomicBool::new(false),
            hooks: Mutex::new(Arc::new(Hooks::production())),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_hooks(&self, hooks: Hooks) {
        *self.hooks.lock() = Arc::new(hooks);
    }

    #[cfg(test)]
    pub(crate) fn owned_pid(&self) -> Option<u32> {
        match &self.inner.lock().phase {
            Phase::Ready(owned) | Phase::Blocked(owned) => Some(owned.child.pid()),
            _ => None,
        }
    }

    fn closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    /// Application shutdown: stops an in-flight start (no opener runs afterwards) and retires the owned frontend.
    pub(crate) fn shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
        let mut inner = self.inner.lock();
        self.changed.notify_all();
        let deadline = Instant::now() + SHUTDOWN_WAIT;
        while matches!(inner.phase, Phase::Starting | Phase::Stopping) {
            if self.changed.wait_until(&mut inner, deadline).timed_out() {
                break;
            }
        }
        match std::mem::replace(&mut inner.phase, Phase::Stopping) {
            Phase::Ready(owned) | Phase::Blocked(owned) => {
                drop(inner);
                let retired = owned.child.terminate();
                let mut inner = self.inner.lock();
                inner.phase = if retired.is_ok() {
                    Phase::Idle
                } else {
                    Phase::Blocked(owned)
                };
                self.changed.notify_all();
            }
            unfinished => inner.phase = unfinished,
        }
    }

    /// Single-flight: concurrent clicks join the one start and receive its result.
    pub(crate) fn open(&self, components: &Components) -> CogneeOpenResult {
        let hooks = self.hooks.lock().clone();
        let mut inner = self.inner.lock();
        let existing = match self.claim_start(&mut inner) {
            Ok(existing) => existing,
            Err(answer) => return answer,
        };
        drop(inner);
        let (outcome, answer) = self.drive(&hooks, components, existing);
        let mut inner = self.inner.lock();
        inner.phase = match outcome {
            Outcome::Idle => Phase::Idle,
            Outcome::Ready(owned) => Phase::Ready(owned),
            Outcome::Blocked(owned) => Phase::Blocked(owned),
        };
        inner.finished += 1;
        inner.last = Some(answer.clone());
        self.changed.notify_all();
        answer
    }

    /// Waits out other starts and stops until this click owns the `Starting` phase. `Err` is the click's final answer;
    /// `Ok` carries the frontend this app already owns, if any.
    fn claim_start(
        &self,
        inner: &mut MutexGuard<'_, Inner>,
    ) -> Result<Option<Arc<Owned>>, CogneeOpenResult> {
        loop {
            if self.closing() {
                return Err(components::result(
                    CogneeOpenState::StartingFailed,
                    None,
                    "OMP Manager is closing; the Cognee UI was not started.",
                ));
            }
            match &inner.phase {
                Phase::Starting => {
                    if let Some(answer) = self.join_start(inner) {
                        return Err(answer);
                    }
                }
                Phase::Stopping => self.changed.wait(inner),
                Phase::Blocked(owned) => {
                    let owned = owned.clone();
                    self.retire_blocked(inner, owned)?;
                }
                Phase::Ready(owned) => {
                    let owned = owned.clone();
                    inner.phase = Phase::Starting;
                    return Ok(Some(owned));
                }
                Phase::Idle => {
                    inner.phase = Phase::Starting;
                    return Ok(None);
                }
            }
        }
    }

    /// Waits for the one running start; its answer, unless shutdown began or no answer was recorded.
    fn join_start(&self, inner: &mut MutexGuard<'_, Inner>) -> Option<CogneeOpenResult> {
        let seen = inner.finished;
        while inner.finished == seen && matches!(inner.phase, Phase::Starting) && !self.closing() {
            self.changed.wait(inner);
        }
        if inner.finished != seen {
            return inner.last.clone();
        }
        None
    }

    /// Retires a frontend that could not be proven stopped; the lock is released while the process tree is terminated.
    fn retire_blocked(
        &self,
        inner: &mut MutexGuard<'_, Inner>,
        owned: Arc<Owned>,
    ) -> Result<(), CogneeOpenResult> {
        inner.phase = Phase::Stopping;
        let retired = MutexGuard::unlocked(inner, || owned.child.terminate());
        inner.phase = if retired.is_ok() {
            Phase::Idle
        } else {
            Phase::Blocked(owned)
        };
        self.changed.notify_all();
        retired.map_err(|error| components::result(CogneeOpenState::StartingFailed, None, format!("The Cognee UI process started earlier could not be stopped completely ({error}), so no new one is started.")))
    }

    fn abort(
        &self,
        owned: Arc<Owned>,
        state: CogneeOpenState,
        message: String,
    ) -> (Outcome, CogneeOpenResult) {
        match owned.child.terminate() {
            Ok(()) => (Outcome::Idle, components::result(state, None, message)),
            Err(error) => {
                let message = format!("{message} The frontend process could not be stopped completely ({error}); OMP Manager retries stopping it before the next start.");
                (
                    Outcome::Blocked(owned),
                    components::result(state, None, message),
                )
            }
        }
    }

    fn drive(
        &self,
        hooks: &Hooks,
        components: &Components,
        existing: Option<Arc<Owned>>,
    ) -> (Outcome, CogneeOpenResult) {
        let port = hooks.frontend_port;
        let (backend, resolved) = match admit(hooks, components) {
            Ok(admitted) => admitted,
            Err(answer) => return (existing.map_or(Outcome::Idle, Outcome::Ready), answer),
        };
        let origin = format!("http://localhost:{}", backend.port);
        if let Some(owned) = existing {
            if let std::ops::ControlFlow::Break(done) =
                self.reuse_existing(owned, hooks, backend.port, &origin, &resolved)
            {
                return done;
            }
        }
        if self.closing() {
            return (
                Outcome::Idle,
                components::result(
                    CogneeOpenState::StartingFailed,
                    None,
                    "OMP Manager is closing; the Cognee UI was not started.",
                ),
            );
        }
        if let Some(answer) = port_blocker(port) {
            return (Outcome::Idle, answer);
        }
        let child = match ContainedChild::spawn(spawn_spec(components, backend.port, port)) {
            Ok(child) => child,
            Err(error) => {
                return (
                    Outcome::Idle,
                    components::result(
                        CogneeOpenState::StartingFailed,
                        None,
                        format!("The Cognee UI could not be started: {error}"),
                    ),
                )
            }
        };
        self.await_ready(
            Arc::new(Owned {
                child,
                backend_port: backend.port,
            }),
            hooks,
            &origin,
            &resolved,
        )
    }

    /// `Break` ends the click with the reused frontend's answer or a stuck one; `Continue` means the old job is retired.
    fn reuse_existing(
        &self,
        owned: Arc<Owned>,
        hooks: &Hooks,
        backend_port: u16,
        origin: &str,
        resolved: &[IpAddr],
    ) -> std::ops::ControlFlow<(Outcome, CogneeOpenResult)> {
        let reusable = owned.backend_port == backend_port
            && matches!(self.check(&owned, hooks, origin, resolved), Check::Ready);
        if reusable {
            return std::ops::ControlFlow::Break(self.finish(owned, hooks));
        }
        // A different backend, a dead process or a replaced listener: retire this app's own job first.
        match owned.child.terminate() {
            Ok(()) => std::ops::ControlFlow::Continue(()),
            Err(error) => std::ops::ControlFlow::Break((Outcome::Blocked(owned), components::result(CogneeOpenState::StartingFailed, None, format!("The Cognee UI process started earlier could not be stopped completely ({error}), so no new one is started.")))),
        }
    }

    /// Polls the freshly spawned frontend until it is ready, failed, foreign-bound or out of time.
    fn await_ready(
        &self,
        owned: Arc<Owned>,
        hooks: &Hooks,
        origin: &str,
        resolved: &[IpAddr],
    ) -> (Outcome, CogneeOpenResult) {
        let deadline = Instant::now() + hooks.startup_timeout;
        let mut waiting;
        loop {
            if self.closing() {
                return self.abort(
                    owned,
                    CogneeOpenState::StartingFailed,
                    "OMP Manager is closing; the Cognee UI start was cancelled.".into(),
                );
            }
            match self.check(&owned, hooks, origin, resolved) {
                Check::Ready => return self.finish(owned, hooks),
                Check::Busy(pid) => {
                    let message = busy(hooks.frontend_port, pid).message.unwrap_or_default();
                    return self.abort(owned, CogneeOpenState::PortBusy, message);
                }
                Check::Failed(message) => {
                    return self.abort(owned, CogneeOpenState::StartingFailed, message)
                }
                Check::Pending(reason) => waiting = reason,
            }
            if Instant::now() >= deadline {
                let seconds = hooks.startup_timeout.as_secs();
                let last = if waiting.is_empty() {
                    String::new()
                } else {
                    format!(" Last status: {}.", fsio::shown(&waiting))
                };
                return self.abort(owned, CogneeOpenState::ReadinessTimeout, format!("The Cognee UI did not become ready within {seconds} seconds.{last} The process OMP Manager started was stopped."));
            }
            std::thread::sleep(POLL);
        }
    }

    /// One look at the owned frontend: alive, every listener on its port ours and loopback, and its own
    /// runtime configuration naming exactly the admitted backend origin.
    fn check(&self, owned: &Owned, hooks: &Hooks, origin: &str, resolved: &[IpAddr]) -> Check {
        match owned.child.try_wait() {
            Ok(Some(code)) => {
                return Check::Failed(format!(
                    "The Cognee UI process exited with code {code} before it was ready.{}",
                    output_tail(owned)
                ))
            }
            Err(error) => {
                return Check::Failed(format!(
                    "The Cognee UI process could not be inspected: {error}"
                ))
            }
            Ok(None) => {}
        }
        let listeners = match owned_listeners(owned, hooks.frontend_port) {
            Ok(Binding::None) => return Check::Pending("not listening yet".into()),
            Ok(Binding::Foreign(pid)) => return Check::Busy(pid),
            Ok(Binding::Ours(listeners)) => listeners,
            Err(error) => return Check::Pending(error),
        };
        if let Some(exposed) = listeners
            .iter()
            .find(|listener| !listener.address.is_loopback())
        {
            return Check::Failed(format!(
                "The Cognee UI listens on {}, which is not a loopback address, so it was stopped.",
                exposed.address
            ));
        }
        let Some(address) = listeners
            .iter()
            .map(|listener| listener.address)
            .find(|address| resolved.contains(address))
        else {
            return Check::Failed(
                "The Cognee UI is not listening on an address that \"localhost\" resolves to."
                    .into(),
            );
        };
        match runtime_config(address, hooks.frontend_port, CONFIG_TIMEOUT) {
            Config::Backend(Some(url))
                if url == origin || url.strip_suffix('/') == Some(origin) =>
            {
                Check::Ready
            }
            Config::Backend(other) => Check::Failed(format!(
                "The Cognee UI reports backend {} instead of {origin}, so it is not used.",
                other
                    .as_deref()
                    .map(fsio::shown)
                    .unwrap_or_else(|| "none".into())
            )),
            Config::Pending(message) => Check::Pending(message),
            Config::Rejected(message) => Check::Failed(message),
        }
    }

    /// Revalidates the listener and process, then opens the browser (never after shutdown began).
    fn finish(&self, owned: Arc<Owned>, hooks: &Hooks) -> (Outcome, CogneeOpenResult) {
        let port = hooks.frontend_port;
        let url = format!("http://localhost:{port}");
        let still = matches!(owned.child.try_wait(), Ok(None))
            && matches!(owned_listeners(&owned, port), Ok(Binding::Ours(listeners)) if !listeners.is_empty() && listeners.iter().all(|l| l.address.is_loopback()));
        if !still {
            return self.abort(
                owned,
                CogneeOpenState::StartingFailed,
                "The Cognee UI stopped or lost its port before the browser was opened.".into(),
            );
        }
        if self.closing() {
            return self.abort(
                owned,
                CogneeOpenState::StartingFailed,
                "OMP Manager is closing; the browser was not opened.".into(),
            );
        }
        match (hooks.opener)(&url) {
            Ok(()) => (
                Outcome::Ready(owned),
                CogneeOpenResult {
                    state: CogneeOpenState::Opened,
                    url: Some(url),
                    message: None,
                },
            ),
            // The child is known ready: keep it for the next explicit click.
            Err(error) => (
                Outcome::Ready(owned),
                components::result(
                    CogneeOpenState::OpenFailed,
                    Some(url),
                    format!("The Cognee UI is ready, but the browser could not be opened: {error}"),
                ),
            ),
        }
    }
}

/// The running, verified backend whose address "localhost" reaches, and where "localhost" resolves for the frontend port.
fn admit(
    hooks: &Hooks,
    components: &Components,
) -> Result<(Backend, Vec<IpAddr>), CogneeOpenResult> {
    let backend = identify_backend(hooks, components)?;
    let resolved = resolve_localhost(hooks.frontend_port)
        .map_err(|error| components::result(CogneeOpenState::Unsupported, None, error))?;
    Ok((backend, resolved))
}

/// The backend must already run: it is identified only from the process list and the listener tables.
fn identify_backend(hooks: &Hooks, components: &Components) -> Result<Backend, CogneeOpenResult> {
    let unavailable =
        |message: String| components::result(CogneeOpenState::BackendUnavailable, None, message);
    let backend = (hooks.discover)(&components.venv_python)
        .and_then(|found| components::choose_backend(&found))
        .map_err(unavailable)?;
    let resolved = resolve_localhost(backend.port);
    if resolved
        .as_ref()
        .is_ok_and(|addresses| addresses.contains(&backend.address))
    {
        return Ok(backend);
    }
    Err(unavailable(match resolved {
        Ok(_) => format!("The Cognee backend listens on {} but \"localhost\" does not resolve to that address, so the UI could not reach it.", backend.address),
        Err(error) => error,
    }))
}

/// Why no new frontend may start: a foreign listener on `port` or an unreadable listener table.
fn port_blocker(port: u16) -> Option<CogneeOpenResult> {
    match native::tcp_listeners() {
        Ok(listeners) => listeners
            .iter()
            .find(|listener| listener.port == port)
            .map(|foreign| busy(port, foreign.pid)),
        Err(error) => Some(components::result(
            CogneeOpenState::StartingFailed,
            None,
            format!("The listening ports could not be read: {error}"),
        )),
    }
}

fn busy(port: u16, pid: u32) -> CogneeOpenResult {
    components::result(
        CogneeOpenState::PortBusy,
        None,
        format!("Port {port} is already in use by another program (process {pid}). OMP Manager did not touch it and does not adopt servers it did not start. Stop that program yourself, then try again."),
    )
}

fn output_tail(owned: &Owned) -> String {
    let tail = owned.child.stderr().tail();
    let tail: String = tail
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect();
    let tail = tail.trim();
    if tail.is_empty() {
        return String::new();
    }
    let start = tail
        .char_indices()
        .rev()
        .nth(1200)
        .map_or(0, |(index, _)| index);
    format!(" Output: {}", tail[start..].trim())
}

fn resolve_localhost(port: u16) -> Result<Vec<IpAddr>, String> {
    let addresses: Vec<IpAddr> = ("localhost", port)
        .to_socket_addrs()
        .map_err(|error| format!("\"localhost\" could not be resolved: {error}"))?
        .map(|address| address.ip())
        .collect();
    if components::all_loopback(&addresses) {
        Ok(addresses)
    } else {
        Err("\"localhost\" does not resolve only to loopback addresses on this computer, so the Cognee UI is not started.".into())
    }
}

/// The literal, shell-free command of the frontend. The internal Next flags are valid for the verified version only.
pub(crate) fn spawn_spec(components: &Components, backend_port: u16, port: u16) -> ContainedSpec {
    let set = |name: &str, value: String| (name.into(), value.into());
    ContainedSpec {
        subject: "Cognee UI".into(),
        program: components.node.clone(),
        args: vec![
            components.next_entry.clone().into_os_string(),
            "dev".into(),
            "--turbopack".into(),
            "--hostname".into(),
            "localhost".into(),
            "--port".into(),
            port.to_string().into(),
        ],
        cwd: Some(components.frontend.clone()),
        env_set: vec![
            set("CI", "1".into()),
            set("NEXT_TELEMETRY_DISABLED", "1".into()),
            set("NEXT_PUBLIC_IS_CLOUD_ENVIRONMENT", "false".into()),
            set(
                "COGNEE_BACKEND_URL",
                format!("http://localhost:{backend_port}"),
            ),
            set("NEXT_IGNORE_INCORRECT_LOCKFILE", "1".into()),
            set("NEXT_DISABLE_SWC_WASM", "1".into()),
            set(
                "__INTERNAL_CUSTOM_TURBOPACK_BINDINGS",
                components.swc_binding.to_string_lossy().into_owned(),
            ),
        ],
        env_remove: [
            "NEXT_TEST_WASM",
            "NEXT_TEST_NATIVE_DIR",
            "NEXT_TEST_NATIVE_IGNORE_LOCAL_INSTALL",
            "NEXT_PUBLIC_LOCAL_API_URL",
        ]
        .into_iter()
        .map(Into::into)
        .collect(),
        stdout: StdioMode::Capture,
        stderr: StdioMode::Capture,
        capture_limit: OUTPUT_TAIL,
    }
}

enum Binding {
    None,
    Foreign(u32),
    Ours(Vec<Listener>),
}

/// Every listener on `port`; one that is not in the owned process tree makes the port foreign.
fn owned_listeners(owned: &Owned, port: u16) -> Result<Binding, String> {
    let on_port: Vec<Listener> = native::tcp_listeners()?
        .into_iter()
        .filter(|listener| listener.port == port)
        .collect();
    if on_port.is_empty() {
        return Ok(Binding::None);
    }
    let parents = native::parent_map()?;
    let (root, created) = (owned.child.pid(), owned.child.creation_time_100ns());
    for listener in &on_port {
        if !native::in_tree(&parents, root, created, listener.pid) {
            return Ok(Binding::Foreign(listener.pid));
        }
    }
    Ok(Binding::Ours(on_port))
}

/// Proxy-free, redirect-free read of the frontend's own runtime configuration through the verified numeric address,
/// keeping `Host: localhost:<port>`. This reads frontend configuration, not the backend database.
fn runtime_config(address: IpAddr, port: u16, timeout: Duration) -> Config {
    let client = match reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return Config::Rejected(format!(
                "The readiness check could not be prepared: {error}"
            ))
        }
    };
    let url = format!(
        "http://{}/api/runtime-config",
        SocketAddr::new(address, port)
    );
    let response = match client
        .get(url)
        .header(reqwest::header::HOST, format!("localhost:{port}"))
        .send()
    {
        Ok(response) => response,
        Err(error) => return Config::Pending(error.to_string()),
    };
    let status = response.status();
    if status.is_redirection() {
        return Config::Rejected(
            "The Cognee UI answered with a redirect, which is refused.".into(),
        );
    }
    if !status.is_success() {
        return Config::Pending(format!("HTTP {status}"));
    }
    let json_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("json"));
    if !json_type {
        return Config::Rejected("The Cognee UI runtime configuration is not JSON.".into());
    }
    let mut body = Vec::new();
    if response
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut body)
        .is_err()
    {
        return Config::Pending("the runtime configuration could not be read".into());
    }
    if body.len() as u64 > MAX_CONFIG_BYTES {
        return Config::Rejected(
            "The Cognee UI runtime configuration is unexpectedly large.".into(),
        );
    }
    match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(value) => Config::Backend(
            value
                .get("backendUrl")
                .and_then(|url| url.as_str())
                .map(str::to_string),
        ),
        Err(_) => Config::Rejected("The Cognee UI runtime configuration is not valid JSON.".into()),
    }
}

// ---------- production backend discovery ----------

const CIM_SCRIPT: &str = "$ErrorActionPreference = 'Stop'; [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false); \
    $rows = @(Get-CimInstance -ClassName Win32_Process -Filter \"Name = 'python.exe' OR Name = 'pythonw.exe' OR Name = 'node.exe'\" | Where-Object { $_.CreationDate } | ForEach-Object { \
    [pscustomobject]@{ pid = [int64]$_.ProcessId; created = [int64]$_.CreationDate.ToFileTimeUtc(); exe = [string]$_.ExecutablePath; cmd = [string]$_.CommandLine } }); \
    ConvertTo-Json -InputObject $rows -Compress";

/// One Python/Node process of the fixed, read-only query.
pub(crate) struct Row {
    pub pid: u32,
    pub created: u64,
    pub exe: String,
    pub cmd: String,
}

#[cfg(unix)]
fn query_processes() -> Result<Vec<Row>, String> {
    native::query_processes()
}
#[cfg(windows)]
/// One fixed, read-only query of Python/Node processes (id, creation time, path, command line), bounded to 15 seconds.
fn query_processes() -> Result<Vec<Row>, String> {
    let shell = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| "C:/Windows".into())
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut command = std::process::Command::new(shell);
    command.args(["-NoProfile", "-NonInteractive", "-Command", CIM_SCRIPT]);
    crate::code_diff::no_console(&mut command);
    let output = crate::code_diff::run_timed(
        command,
        None,
        DISCOVERY_TIMEOUT,
        "PowerShell was not found, so no running Cognee backend could be identified.",
    )
    .map_err(|error| format!("The process list could not be read: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "The process list could not be read: {}",
            fsio::shown(&String::from_utf8_lossy(&output.stderr))
        ));
    }
    if output.stdout.len() > 4 * 1024 * 1024 {
        return Err("The process list is unexpectedly large.".into());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim().trim_start_matches('\u{feff}');
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|_| "The process list was not understood.".to_string())?;
    let items = match value {
        serde_json::Value::Array(items) => items,
        single @ serde_json::Value::Object(_) => vec![single],
        _ => return Err("The process list was not understood.".into()),
    };
    Ok(items
        .iter()
        .filter_map(|item| {
            Some(Row {
                pid: u32::try_from(item.get("pid")?.as_i64()?).ok()?,
                created: u64::try_from(item.get("created")?.as_i64()?).ok()?,
                exe: item
                    .get("exe")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                cmd: item
                    .get("cmd")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect())
}

/// Whether this process was started from the installed environment's interpreter. Windows compares the interpreter
/// path itself. A Unix venv (and a Nix environment) is a directory of links, so the process reports a sibling such as
/// `python3.13` in the same real `bin` directory: the comparison is on that directory, never on the shared base interpreter.
pub(super) fn launched_from_venv(program: &str, exe: &str, venv_python: &Path) -> bool {
    #[cfg(unix)]
    {
        let _ = exe;
        let bin = |path: &Path| path.parent().and_then(|dir| fs::canonicalize(dir).ok());
        matches!((bin(Path::new(program)), bin(venv_python)), (Some(a), Some(b)) if a == b)
    }
    #[cfg(not(unix))]
    {
        fsio::same_path(Path::new(program), venv_python)
            || (!exe.is_empty() && fsio::same_path(Path::new(exe), venv_python))
    }
}

/// Backends among `rows` whose command line is exactly the installed Cognee uvicorn app and whose process, still
/// the same (`creation` is read after the listener table), owns a numeric loopback listener on its port. WMI
/// creation times have microsecond resolution, hence the tick comparison at that granularity.
pub(crate) fn verify_backends(
    rows: &[Row],
    listeners: &[Listener],
    venv_python: &Path,
    creation: &dyn Fn(u32) -> Option<u64>,
) -> Vec<Backend> {
    let mut verified = Vec::new();
    for row in rows {
        let tokens = components::split_command_line(&row.cmd);
        let Some(program) = tokens.first() else {
            continue;
        };
        let installed = launched_from_venv(program, &row.exe, venv_python);
        let Some(port) = installed
            .then(|| components::uvicorn_port(&tokens))
            .flatten()
        else {
            continue;
        };
        let owned_listeners = listeners.iter().filter(|listener| {
            listener.pid == row.pid && listener.port == port && listener.address.is_loopback()
        });
        for listener in owned_listeners {
            if creation(row.pid).is_some_and(|created| created / 10 == row.created / 10) {
                verified.push(Backend {
                    port,
                    pid: row.pid,
                    address: listener.address,
                });
            }
        }
    }
    verified
}

pub(super) fn discover_backends(venv_python: &Path) -> Result<Vec<Backend>, String> {
    let rows = query_processes()?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let listeners = native::tcp_listeners()?;
    Ok(verify_backends(
        &rows,
        &listeners,
        venv_python,
        &native::creation_time,
    ))
}
