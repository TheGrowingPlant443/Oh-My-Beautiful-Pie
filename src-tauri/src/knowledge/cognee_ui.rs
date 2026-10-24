//! The explicit "Open Cognee UI" action: passive capability inspection and an app-owned, contained Cognee frontend.
//!
//! Page entry and capability probes only stat files (`capability`): no process is started, no network is touched, no
//! Cognee code is imported and Cognee's own cache helper (which creates folders) is never called. Only an explicit
//! click runs [`KnowledgeService::open_cognee_ui`]:
//!
//! 1. The installation is revalidated. Only the inspected development layout is supported: package `cognee-frontend`,
//!    installed Next **16.3.4**, a Node.js new enough for it, TypeScript, the React/Node typings and the matching
//!    Windows x64 SWC binding. A standalone `server.js` alone proves neither complete static assets nor a local-mode
//!    build, so it is never started. Anything missing yields a diagnostic with zero process spawns and zero network.
//! 2. An already running local Cognee backend is required and verified (installed-venv Python running
//!    `-m uvicorn cognee.api.client:app`, identified by creation time, owning a numeric loopback listener). The
//!    backend is never started, migrated or contacted; only the one-shot read-only process query and the OS listener
//!    tables are read.
//! 3. The frontend is started as `node <next> dev --turbopack --hostname localhost --port 3000` inside a job-contained
//!    child (no npm, no shell). The internal Next flags that forbid package/binding downloads are applied only to the
//!    exact verified Next version. Startup may compile and write Next's `.next` caches and generated TypeScript
//!    declarations in the frontend folder; it installs nothing and writes no Cognee data or configuration.
//! 4. Readiness needs every listener on the port to belong to this app's process tree, loopback only, and
//!    `GET /api/runtime-config` (proxy-free, no redirects) to name exactly the admitted backend origin. A foreign
//!    listener is `portBusy` and left untouched; unknown servers are never adopted.
//! 5. The browser is opened at `http://localhost:3000`, Cognee's default allowed origin, only after the listener and
//!    process were revalidated and only while the app is not shutting down.
use super::dto::*;
use super::fsio::{self, shown};
use super::integrations::open_loopback_url;
use super::KnowledgeService;
use std::fs;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The only Next.js version whose internal startup flags were inspected.
pub(crate) const SUPPORTED_NEXT: &str = "16.3.4";
const FRONTEND_PORT: u16 = 3000;
const BACKEND_PORT: u16 = 8000;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_JSON_BYTES: u64 = 1024 * 1024;
/// Next 16 requires Node 20.9 or later; used when its package declares no parseable `engines.node` floor.
const DEFAULT_NODE_FLOOR: (u32, u32, u32) = (20, 9, 0);

/// Why the UI cannot be started from what is installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Blocked {
    pub state: CogneeUiState,
    pub message: String,
}

fn blocked(state: CogneeUiState, message: impl Into<String>) -> Blocked {
    Blocked { state, message: message.into() }
}

/// The verified installed pieces a start needs.
#[derive(Clone, Debug)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct Components {
    pub frontend: PathBuf,
    pub node: PathBuf,
    pub next_entry: PathBuf,
    pub swc_binding: PathBuf,
    /// The installed venv's interpreter that a verified backend must be running.
    pub venv_python: PathBuf,
}

fn forward(path: &Path) -> String {
    shown(&path.to_string_lossy().replace('\\', "/"))
}

fn unreadable(path: &Path, error: impl std::fmt::Display) -> Blocked {
    blocked(CogneeUiState::Unreadable, format!("Cannot inspect {}: {error}", forward(path)))
}

/// `Ok(None)` only for a missing path; every other failure stays a diagnostic.
fn stat(path: &Path) -> Result<Option<fs::Metadata>, Blocked> {
    match fs::metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(unreadable(path, error)),
    }
}

fn is_file(path: &Path) -> Result<bool, Blocked> {
    Ok(stat(path)?.is_some_and(|meta| meta.is_file()))
}

fn read_json(path: &Path) -> Result<Option<serde_json::Value>, Blocked> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable(path, error)),
    };
    let mut bytes = Vec::new();
    (&mut file).take(MAX_JSON_BYTES + 1).read_to_end(&mut bytes).map_err(|error| unreadable(path, error))?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return Err(blocked(CogneeUiState::Unreadable, format!("{} is unexpectedly large", forward(path))));
    }
    serde_json::from_slice(&bytes).map(Some).map_err(|_| blocked(CogneeUiState::Unreadable, format!("{} is not valid JSON", forward(path))))
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.trim().trim_start_matches('v').split('.');
    let mut next = || parts.next().and_then(|part| part.parse::<u32>().ok());
    Some((next()?, next()?, next().unwrap_or(0)))
}

/// `>=X.Y.Z` is the only engines form understood; anything else keeps the default floor.
fn node_floor(next: &serde_json::Value) -> (u32, u32, u32) {
    next.pointer("/engines/node").and_then(|value| value.as_str()).and_then(|text| text.trim().strip_prefix(">=")).and_then(parse_version).unwrap_or(DEFAULT_NODE_FLOOR)
}

/// The Node.js executable: an explicit absolute override (isolated runs) or the first `node.exe` on an absolute `PATH` entry.
fn find_node(node_override: Option<&Path>) -> Result<Option<PathBuf>, Blocked> {
    if let Some(path) = node_override {
        return Ok(if path.is_absolute() && is_file(path)? { Some(path.to_path_buf()) } else { None });
    }
    let name = if cfg!(windows) { "node.exe" } else { "node" };
    let Some(paths) = std::env::var_os("PATH") else { return Ok(None) };
    for entry in std::env::split_paths(&paths) {
        if !entry.is_absolute() {
            continue;
        }
        let candidate = entry.join(name);
        if is_file(&candidate)? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

#[cfg(windows)]
fn node_version(path: &Path) -> Option<(u32, u32, u32)> {
    super::integrations::executable_version(path).as_deref().and_then(parse_version)
}

/// Inspects the installed frontend, its Node packages and Node.js. Reads metadata only; never runs, imports or writes.
pub(crate) fn inspect_components(cognee_folder: &Path, home: &Path, node_override: Option<&Path>) -> Result<Components, Blocked> {
    if !cfg!(any(all(windows, target_arch = "x86_64"), all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))) {
        return Err(blocked(CogneeUiState::Unsupported, "This platform has no supported Next.js native binding."));
    }
    let venv = cognee_folder.join("venv");
    #[cfg(windows)]
    let site = venv.join("Lib").join("site-packages");
    #[cfg(unix)]
    let site = {
        let lib = venv.join("lib");
        let mut sites = match fs::read_dir(&lib) {
            Ok(entries) => entries.filter_map(|entry| entry.ok()).map(|entry| entry.path())
                .filter(|path| path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("python")))
                .map(|path| path.join("site-packages")).collect::<Vec<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(unreadable(&lib, error)),
        };
        sites.sort();
        sites.into_iter().find(|path| path.join("cognee").is_dir()).unwrap_or_else(|| lib.join("site-packages"))
    };
    // The same candidates, in the same order, as Cognee's own frontend lookup: three development locations next to the
    // package, then the download cache. Looking is read-only; the cache folder is never created.
    let candidates = [
        site.join("cognee-frontend"),
        site.join("cognee").join("cognee-frontend"),
        site.join("cognee").join("api").join("cognee-frontend"),
        home.join(".cognee").join("ui-cache").join("frontend"),
    ];
    let mut frontend = None;
    for candidate in &candidates {
        if is_file(&candidate.join("package.json"))? {
            frontend = Some(candidate.clone());
            break;
        }
    }
    let Some(frontend) = frontend else {
        return Err(blocked(
            CogneeUiState::MissingFrontend,
            "Cognee is installed, but its web interface (the cognee-frontend package) is not installed on this computer. OMP Manager never downloads or installs it; install it yourself, then check again.",
        ));
    };
    let package = read_json(&frontend.join("package.json"))?.ok_or_else(|| unreadable(&frontend.join("package.json"), "it disappeared"))?;
    if package.get("name").and_then(|name| name.as_str()) != Some("cognee-frontend") {
        return Err(blocked(CogneeUiState::Unsupported, format!("{} is not the cognee-frontend package, so it is not started.", forward(&frontend))));
    }
    let modules = frontend.join("node_modules");
    if stat(&modules)?.is_none_or(|meta| !meta.is_dir()) {
        return Err(blocked(
            CogneeUiState::MissingDependencies,
            format!("The Cognee web interface is present in {}, but its Node packages are not installed (no node_modules). OMP Manager never installs them; run the install yourself, then try again.", forward(&frontend)),
        ));
    }
    let next_package = read_json(&modules.join("next").join("package.json"))?;
    let next_json = match &next_package {
        Some(json) if json.get("name").and_then(|name| name.as_str()) == Some("next") => json,
        Some(_) => return Err(blocked(CogneeUiState::Unsupported, "node_modules/next is not the Next.js package, so the interface is not started.")),
        None => {
            return Err(blocked(
                CogneeUiState::MissingDependencies,
                "The Cognee web interface needs Next.js, which is not installed in its node_modules. OMP Manager never installs it.",
            ))
        }
    };
    let version = next_json.get("version").and_then(|version| version.as_str()).unwrap_or("unknown");
    if version != SUPPORTED_NEXT {
        return Err(blocked(
            CogneeUiState::Unsupported,
            format!("Next.js {version} is installed for the Cognee web interface; only Next.js {SUPPORTED_NEXT} was inspected and is supported, so it is not started."),
        ));
    }
    let next_entry = modules.join("next").join("dist").join("bin").join("next");
    let binding = if cfg!(windows) { "win32-x64-msvc" }
        else if cfg!(all(target_os = "macos", target_arch = "aarch64")) { "darwin-arm64" }
        else if cfg!(target_os = "macos") { "darwin-x64" }
        else if cfg!(target_arch = "aarch64") { "linux-arm64-gnu" }
        else { "linux-x64-gnu" };
    let swc_package = modules.join("@next").join(format!("swc-{binding}"));
    let swc_binding = swc_package.join(format!("next-swc.{binding}.node"));
    let mut missing = Vec::new();
    for (path, label) in [
        (&next_entry, "next/dist/bin/next"),
        (&modules.join("typescript").join("lib").join("typescript.js"), "typescript"),
        (&modules.join("@types").join("react").join("index.d.ts"), "@types/react"),
        (&modules.join("@types").join("node").join("index.d.ts"), "@types/node"),
        (&swc_binding, "the platform-specific @next/swc native binding"),
    ] {
        if !is_file(path)? {
            missing.push(label);
        }
    }
    if !missing.is_empty() {
        return Err(blocked(
            CogneeUiState::MissingDependencies,
            format!("These installed packages of the Cognee web interface are missing: {}. OMP Manager never installs or downloads them; install them yourself, then try again.", missing.join(", ")),
        ));
    }
    match read_json(&swc_package.join("package.json"))?.as_ref().and_then(|json| json.get("version")).and_then(|version| version.as_str()) {
        Some(SUPPORTED_NEXT) => {}
        Some(other) => {
            return Err(blocked(CogneeUiState::Unsupported, format!("The installed Next.js native binding is version {other}, not {SUPPORTED_NEXT}, so the interface is not started.")))
        }
        None => return Err(blocked(CogneeUiState::MissingDependencies, "The installed Next.js native binding has no readable package.json.")),
    }
    let node = find_node(node_override)?.ok_or_else(|| blocked(CogneeUiState::MissingDependencies, "Node.js was not found on this computer's PATH; the Cognee web interface needs it. OMP Manager never installs it."))?;
    #[cfg(windows)]
    {
    let floor = node_floor(next_json);
    match node_version(&node) {
        Some(found) if found >= floor => {}
        Some(found) => {
            return Err(blocked(
                CogneeUiState::Unsupported,
                format!("Node.js {}.{}.{} is installed; Next.js {SUPPORTED_NEXT} needs {}.{}.{} or later.", found.0, found.1, found.2, floor.0, floor.1, floor.2),
            ))
        }
        None => return Err(blocked(CogneeUiState::Unreadable, format!("The version of Node.js at {} could not be read.", forward(&node)))),
    }
    }
    Ok(Components { frontend, node, next_entry, swc_binding, venv_python: if cfg!(windows) { venv.join("Scripts").join("python.exe") } else { venv.join("bin").join("python") } })
}

/// The passive capability shown on page entry: the same inspection as a click, nothing more.
pub(crate) fn capability(cognee_folder: &Path, home: &Path, node_override: Option<&Path>) -> CogneeUiCapability {
    match inspect_components(cognee_folder, home, node_override) {
        Ok(_) => CogneeUiCapability { state: CogneeUiState::Ready, message: None },
        Err(blocked) => CogneeUiCapability { state: blocked.state, message: Some(blocked.message) },
    }
}

// ---------- backend identification (pure parts) ----------

/// A backend this app verified: its process owns a numeric loopback listener on `port`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Backend {
    pub port: u16,
    pub pid: u32,
    pub address: IpAddr,
}

/// Windows command-line splitting (`CommandLineToArgvW` rules; the program name follows the simpler first-token rule).
pub(crate) fn split_command_line(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    let skip = |i: &mut usize| {
        while *i < chars.len() && (chars[*i] == ' ' || chars[*i] == '\t') {
            *i += 1;
        }
    };
    skip(&mut i);
    if i < chars.len() {
        let mut program = String::new();
        if chars[i] == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                program.push(chars[i]);
                i += 1;
            }
            i += 1;
        } else {
            while i < chars.len() && chars[i] != ' ' && chars[i] != '\t' {
                program.push(chars[i]);
                i += 1;
            }
        }
        tokens.push(program);
    }
    loop {
        skip(&mut i);
        if i >= chars.len() {
            break;
        }
        let mut token = String::new();
        let mut quoted = false;
        while i < chars.len() {
            let mut slashes = 0;
            while i < chars.len() && chars[i] == '\\' {
                slashes += 1;
                i += 1;
            }
            if i < chars.len() && chars[i] == '"' {
                token.extend(std::iter::repeat('\\').take(slashes / 2));
                if slashes % 2 == 1 {
                    token.push('"');
                    i += 1;
                } else if quoted && chars.get(i + 1) == Some(&'"') {
                    token.push('"');
                    i += 2;
                } else {
                    quoted = !quoted;
                    i += 1;
                }
                continue;
            }
            token.extend(std::iter::repeat('\\').take(slashes));
            if i >= chars.len() || (!quoted && (chars[i] == ' ' || chars[i] == '\t')) {
                break;
            }
            token.push(chars[i]);
            i += 1;
        }
        tokens.push(token);
    }
    tokens
}

/// The port of a command line that is exactly `<python> -m uvicorn cognee.api.client:app [--host <loopback>] [--port N] ...`
/// with only benign options. `None` for anything else, so a lookalike is never trusted.
pub(crate) fn uvicorn_port(tokens: &[String]) -> Option<u16> {
    if tokens.len() < 4 || tokens[1] != "-m" || tokens[2] != "uvicorn" || tokens[3] != "cognee.api.client:app" {
        return None;
    }
    let mut host = "127.0.0.1".to_string();
    let mut port = BACKEND_PORT;
    let mut i = 4;
    while i < tokens.len() {
        let token = tokens[i].as_str();
        let (name, inline) = match token.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (token, None),
        };
        match name {
            "--host" | "--port" | "--log-level" => {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        i += 1;
                        tokens.get(i)?.clone()
                    }
                };
                match name {
                    "--host" => host = value,
                    "--port" => port = value.parse().ok().filter(|port| *port != 0)?,
                    _ => {}
                }
            }
            "--no-access-log" | "--access-log" | "--use-colors" | "--no-use-colors" if inline.is_none() => {}
            _ => return None,
        }
        i += 1;
    }
    let loopback = matches!(host.to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]");
    loopback.then_some(port)
}

/// Port 8000 when verified, otherwise exactly one verified backend.
pub(crate) fn choose_backend(verified: &[Backend]) -> Result<Backend, String> {
    let mut ordered: Vec<&Backend> = verified.iter().collect();
    ordered.sort_by(|a, b| (a.port, a.pid, a.address).cmp(&(b.port, b.pid, b.address)));
    if let Some(found) = ordered.iter().find(|backend| backend.port == BACKEND_PORT) {
        return Ok((*found).clone());
    }
    let mut ports: Vec<u16> = ordered.iter().map(|backend| backend.port).collect();
    ports.dedup();
    match ports.as_slice() {
        [] => Err("No running local Cognee backend was found. Start your Cognee backend yourself (OMP Manager never starts it or runs migrations), then try again.".into()),
        [port] => Ok(ordered.iter().find(|backend| backend.port == *port).map(|backend| (*backend).clone()).expect("port came from this list")),
        many => Err(format!("Several Cognee backends are running (ports {}), so none is chosen. Leave one running, or use port {BACKEND_PORT}.", many.iter().map(u16::to_string).collect::<Vec<_>>().join(", "))),
    }
}

/// Every address a name resolved to must be loopback (and there must be one).
pub(crate) fn all_loopback(addresses: &[IpAddr]) -> bool {
    !addresses.is_empty() && addresses.iter().all(IpAddr::is_loopback)
}

fn result(state: CogneeOpenState, url: Option<String>, message: impl Into<String>) -> CogneeOpenResult {
    CogneeOpenResult { state, url, message: Some(message.into()) }
}

/// Unix has no Windows executable-version resource. Only an explicit Open executes Node; passive discovery remains read-only.
#[cfg(unix)]
fn verify_unix_node(components: &Components) -> Result<(), String> {
    let next = read_json(&components.frontend.join("node_modules/next/package.json")).map_err(|e| e.message)?
        .ok_or_else(|| "The installed Next.js package disappeared.".to_string())?;
    let floor = node_floor(&next);
    let mut command = std::process::Command::new(&components.node);
    command.arg("--version");
    let output = crate::code_diff::run_timed(command, None, Duration::from_secs(15), "Node.js version inspection timed out.")?;
    let version = output.status.success().then(|| parse_version(&String::from_utf8_lossy(&output.stdout))).flatten()
        .ok_or_else(|| "The installed Node.js version could not be read.".to_string())?;
    if version < floor {
        return Err(format!("Node.js {}.{}.{} is too old; this Next.js requires {}.{}.{}.", version.0, version.1, version.2, floor.0, floor.1, floor.2));
    }
    Ok(())
}

impl KnowledgeService {
    /// Explicit click: revalidates the installation and opens (starting this app's own frontend if needed) the Cognee UI
    /// for an already running local backend. Always an answer; see the module documentation for what is never done.
    pub fn open_cognee_ui(&self) -> CogneeOpenResult {
        let capability = self.probe_cognee();
        let missing = |message: String| result(CogneeOpenState::MissingComponent, None, message);
        match capability.state {
            CogneeState::Unreadable => return missing(capability.message.unwrap_or_else(|| "Cognee could not be inspected.".into())),
            CogneeState::Absent => return missing("Cognee is not installed on this computer.".into()),
            CogneeState::Installed => {}
        }
        let Some(folder) = capability.installation_folder else { return missing("The Cognee installation folder is unknown.".into()) };
        match inspect_components(Path::new(&folder), &self.home, self.env.cognee_node.as_deref()) {
            Ok(components) => {
                #[cfg(unix)]
                if let Err(message) = verify_unix_node(&components) {
                    return result(CogneeOpenState::MissingComponent, None, message);
                }
                self.cognee_ui.open(&components)
            },
            Err(blocked) => result(if blocked.state == CogneeUiState::Unsupported { CogneeOpenState::Unsupported } else { CogneeOpenState::MissingComponent }, None, blocked.message),
        }
    }
}

#[cfg(any(windows, unix))]
pub(crate) use live::CogneeUi;
#[cfg(all(windows, test))]
pub(crate) use live::{native, verify_backends, Hooks, Listener, Row};

#[cfg(not(any(windows, unix)))]
pub(crate) struct CogneeUi;

#[cfg(not(any(windows, unix)))]
impl CogneeUi {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn shutdown(&self) {}

    pub(crate) fn open(&self, _components: &Components) -> CogneeOpenResult {
        result(CogneeOpenState::Unsupported, None, "OMP Manager can start the Cognee UI only on Windows (x64).")
    }
}

#[cfg(any(windows, unix))]
mod live {
    use super::*;
    use crate::process_tree::{ContainedChild, ContainedSpec, StdioMode};
    use parking_lot::{Condvar, Mutex};
    use std::net::SocketAddr;
    use std::net::ToSocketAddrs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    const POLL: Duration = Duration::from_millis(250);
    const CONFIG_TIMEOUT: Duration = Duration::from_secs(15);
    const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
    const MAX_CONFIG_BYTES: u64 = 16 * 1024;
    const OUTPUT_TAIL: usize = 16 * 1024;
    const SHUTDOWN_WAIT: Duration = Duration::from_secs(15);

    /// A TCP listener from the operating system's table.
    #[derive(Clone, Debug, PartialEq, Eq)]
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
                opener: Box::new(|url| open_loopback_url(url)),
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
        pub(crate) fn new() -> Self {
            Self {
                inner: Mutex::new(Inner { phase: Phase::Idle, finished: 0, last: None }),
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
                    inner.phase = if retired.is_ok() { Phase::Idle } else { Phase::Blocked(owned) };
                    self.changed.notify_all();
                }
                unfinished => inner.phase = unfinished,
            }
        }

        /// Single-flight: concurrent clicks join the one start and receive its result.
        pub(crate) fn open(&self, components: &Components) -> CogneeOpenResult {
            let hooks = self.hooks.lock().clone();
            let mut inner = self.inner.lock();
            let existing = loop {
                if self.closing() {
                    return result(CogneeOpenState::StartingFailed, None, "OMP Manager is closing; the Cognee UI was not started.");
                }
                match &inner.phase {
                    Phase::Starting => {
                        let seen = inner.finished;
                        while inner.finished == seen && matches!(inner.phase, Phase::Starting) && !self.closing() {
                            self.changed.wait(&mut inner);
                        }
                        if inner.finished != seen {
                            if let Some(answer) = inner.last.clone() {
                                return answer;
                            }
                        }
                    }
                    Phase::Stopping => self.changed.wait(&mut inner),
                    Phase::Blocked(owned) => {
                        let owned = owned.clone();
                        inner.phase = Phase::Stopping;
                        drop(inner);
                        let retired = owned.child.terminate();
                        inner = self.inner.lock();
                        inner.phase = if retired.is_ok() { Phase::Idle } else { Phase::Blocked(owned) };
                        self.changed.notify_all();
                        if let Err(error) = retired {
                            return result(CogneeOpenState::StartingFailed, None, format!("The Cognee UI process started earlier could not be stopped completely ({error}), so no new one is started."));
                        }
                    }
                    Phase::Ready(owned) => {
                        let owned = owned.clone();
                        inner.phase = Phase::Starting;
                        break Some(owned);
                    }
                    Phase::Idle => {
                        inner.phase = Phase::Starting;
                        break None;
                    }
                }
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

        fn abort(&self, owned: Arc<Owned>, state: CogneeOpenState, message: String) -> (Outcome, CogneeOpenResult) {
            match owned.child.terminate() {
                Ok(()) => (Outcome::Idle, result(state, None, message)),
                Err(error) => {
                    let message = format!("{message} The frontend process could not be stopped completely ({error}); OMP Manager retries stopping it before the next start.");
                    (Outcome::Blocked(owned), result(state, None, message))
                }
            }
        }

        fn drive(&self, hooks: &Hooks, components: &Components, existing: Option<Arc<Owned>>) -> (Outcome, CogneeOpenResult) {
            let keep = |existing: Option<Arc<Owned>>| existing.map_or(Outcome::Idle, Outcome::Ready);
            let port = hooks.frontend_port;
            // The backend must already run: it is identified only from the process list and the listener tables.
            let backend = match (hooks.discover)(&components.venv_python).and_then(|found| choose_backend(&found)) {
                Ok(backend) => backend,
                Err(message) => return (keep(existing), result(CogneeOpenState::BackendUnavailable, None, message)),
            };
            let backend_resolved = resolve_localhost(backend.port);
            if !backend_resolved.as_ref().is_ok_and(|addresses| addresses.contains(&backend.address)) {
                let message = match backend_resolved {
                    Ok(_) => format!("The Cognee backend listens on {} but \"localhost\" does not resolve to that address, so the UI could not reach it.", backend.address),
                    Err(error) => error,
                };
                return (keep(existing), result(CogneeOpenState::BackendUnavailable, None, message));
            }
            let origin = format!("http://localhost:{}", backend.port);
            let resolved = match resolve_localhost(port) {
                Ok(addresses) => addresses,
                Err(error) => return (keep(existing), result(CogneeOpenState::Unsupported, None, error)),
            };
            if let Some(owned) = existing {
                let reusable = owned.backend_port == backend.port
                    && matches!(self.check(&owned, hooks, &origin, &resolved), Check::Ready);
                if reusable {
                    return self.finish(owned, hooks);
                }
                // A different backend, a dead process or a replaced listener: retire this app's own job first.
                if let Err(error) = owned.child.terminate() {
                    return (Outcome::Blocked(owned), result(CogneeOpenState::StartingFailed, None, format!("The Cognee UI process started earlier could not be stopped completely ({error}), so no new one is started.")));
                }
            }
            if self.closing() {
                return (Outcome::Idle, result(CogneeOpenState::StartingFailed, None, "OMP Manager is closing; the Cognee UI was not started."));
            }
            match native::tcp_listeners() {
                Ok(listeners) => {
                    if let Some(foreign) = listeners.iter().find(|listener| listener.port == port) {
                        return (Outcome::Idle, busy(port, foreign.pid));
                    }
                }
                Err(error) => return (Outcome::Idle, result(CogneeOpenState::StartingFailed, None, format!("The listening ports could not be read: {error}"))),
            }
            let child = match ContainedChild::spawn(spawn_spec(components, backend.port, port)) {
                Ok(child) => child,
                Err(error) => return (Outcome::Idle, result(CogneeOpenState::StartingFailed, None, format!("The Cognee UI could not be started: {error}"))),
            };
            let owned = Arc::new(Owned { child, backend_port: backend.port });
            let deadline = Instant::now() + hooks.startup_timeout;
            let mut waiting;
            loop {
                if self.closing() {
                    return self.abort(owned, CogneeOpenState::StartingFailed, "OMP Manager is closing; the Cognee UI start was cancelled.".into());
                }
                match self.check(&owned, hooks, &origin, &resolved) {
                    Check::Ready => return self.finish(owned, hooks),
                    Check::Busy(pid) => {
                        let message = busy(port, pid).message.unwrap_or_default();
                        return self.abort(owned, CogneeOpenState::PortBusy, message);
                    }
                    Check::Failed(message) => return self.abort(owned, CogneeOpenState::StartingFailed, message),
                    Check::Pending(reason) => waiting = reason,
                }
                if Instant::now() >= deadline {
                    let seconds = hooks.startup_timeout.as_secs();
                    let last = if waiting.is_empty() { String::new() } else { format!(" Last status: {}.", shown(&waiting)) };
                    return self.abort(owned, CogneeOpenState::ReadinessTimeout, format!("The Cognee UI did not become ready within {seconds} seconds.{last} The process OMP Manager started was stopped."));
                }
                std::thread::sleep(POLL);
            }
        }

        /// One look at the owned frontend: alive, every listener on its port ours and loopback, and its own
        /// runtime configuration naming exactly the admitted backend origin.
        fn check(&self, owned: &Owned, hooks: &Hooks, origin: &str, resolved: &[IpAddr]) -> Check {
            match owned.child.try_wait() {
                Ok(Some(code)) => return Check::Failed(format!("The Cognee UI process exited with code {code} before it was ready.{}", output_tail(owned))),
                Err(error) => return Check::Failed(format!("The Cognee UI process could not be inspected: {error}")),
                Ok(None) => {}
            }
            let listeners = match owned_listeners(owned, hooks.frontend_port) {
                Ok(Binding::None) => return Check::Pending("not listening yet".into()),
                Ok(Binding::Foreign(pid)) => return Check::Busy(pid),
                Ok(Binding::Ours(listeners)) => listeners,
                Err(error) => return Check::Pending(error),
            };
            if let Some(exposed) = listeners.iter().find(|listener| !listener.address.is_loopback()) {
                return Check::Failed(format!("The Cognee UI listens on {}, which is not a loopback address, so it was stopped.", exposed.address));
            }
            let Some(address) = listeners.iter().map(|listener| listener.address).find(|address| resolved.contains(address)) else {
                return Check::Failed("The Cognee UI is not listening on an address that \"localhost\" resolves to.".into());
            };
            match runtime_config(address, hooks.frontend_port, CONFIG_TIMEOUT) {
                Config::Backend(Some(url)) if url == origin || url.strip_suffix('/') == Some(origin) => Check::Ready,
                Config::Backend(other) => Check::Failed(format!("The Cognee UI reports backend {} instead of {origin}, so it is not used.", other.as_deref().map(shown).unwrap_or_else(|| "none".into()))),
                Config::Pending(message) => Check::Pending(message),
                Config::Rejected(message) => Check::Failed(message),
            }
        }

        /// Revalidates the listener and process, then opens the browser (never after shutdown began).
        fn finish(&self, owned: Arc<Owned>, hooks: &Hooks) -> (Outcome, CogneeOpenResult) {
            let port = hooks.frontend_port;
            let url = format!("http://localhost:{port}");
            let still = matches!(owned.child.try_wait(), Ok(None)) && matches!(owned_listeners(&owned, port), Ok(Binding::Ours(listeners)) if !listeners.is_empty() && listeners.iter().all(|l| l.address.is_loopback()));
            if !still {
                return self.abort(owned, CogneeOpenState::StartingFailed, "The Cognee UI stopped or lost its port before the browser was opened.".into());
            }
            if self.closing() {
                return self.abort(owned, CogneeOpenState::StartingFailed, "OMP Manager is closing; the browser was not opened.".into());
            }
            match (hooks.opener)(&url) {
                Ok(()) => (Outcome::Ready(owned), CogneeOpenResult { state: CogneeOpenState::Opened, url: Some(url), message: None }),
                // The child is known ready: keep it for the next explicit click.
                Err(error) => (Outcome::Ready(owned), result(CogneeOpenState::OpenFailed, Some(url), format!("The Cognee UI is ready, but the browser could not be opened: {error}"))),
            }
        }
    }

    fn busy(port: u16, pid: u32) -> CogneeOpenResult {
        result(
            CogneeOpenState::PortBusy,
            None,
            format!("Port {port} is already in use by another program (process {pid}). OMP Manager did not touch it and does not adopt servers it did not start. Stop that program yourself, then try again."),
        )
    }

    fn output_tail(owned: &Owned) -> String {
        let tail = owned.child.stderr().tail();
        let tail: String = tail.chars().filter(|c| !c.is_control() || *c == '\n').collect();
        let tail = tail.trim();
        if tail.is_empty() {
            return String::new();
        }
        let start = tail.char_indices().rev().nth(1200).map_or(0, |(index, _)| index);
        format!(" Output: {}", tail[start..].trim())
    }

    fn resolve_localhost(port: u16) -> Result<Vec<IpAddr>, String> {
        let addresses: Vec<IpAddr> = ("localhost", port).to_socket_addrs().map_err(|error| format!("\"localhost\" could not be resolved: {error}"))?.map(|address| address.ip()).collect();
        if all_loopback(&addresses) {
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
                set("COGNEE_BACKEND_URL", format!("http://localhost:{backend_port}")),
                set("NEXT_IGNORE_INCORRECT_LOCKFILE", "1".into()),
                set("NEXT_DISABLE_SWC_WASM", "1".into()),
                set("__INTERNAL_CUSTOM_TURBOPACK_BINDINGS", components.swc_binding.to_string_lossy().into_owned()),
            ],
            env_remove: ["NEXT_TEST_WASM", "NEXT_TEST_NATIVE_DIR", "NEXT_TEST_NATIVE_IGNORE_LOCAL_INSTALL", "NEXT_PUBLIC_LOCAL_API_URL"].into_iter().map(Into::into).collect(),
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
        let on_port: Vec<Listener> = native::tcp_listeners()?.into_iter().filter(|listener| listener.port == port).collect();
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
        let client = match reqwest::blocking::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(timeout).build() {
            Ok(client) => client,
            Err(error) => return Config::Rejected(format!("The readiness check could not be prepared: {error}")),
        };
        let url = format!("http://{}/api/runtime-config", SocketAddr::new(address, port));
        let response = match client.get(url).header(reqwest::header::HOST, format!("localhost:{port}")).send() {
            Ok(response) => response,
            Err(error) => return Config::Pending(error.to_string()),
        };
        let status = response.status();
        if status.is_redirection() {
            return Config::Rejected("The Cognee UI answered with a redirect, which is refused.".into());
        }
        if !status.is_success() {
            return Config::Pending(format!("HTTP {status}"));
        }
        let json_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).is_some_and(|value| value.contains("json"));
        if !json_type {
            return Config::Rejected("The Cognee UI runtime configuration is not JSON.".into());
        }
        let mut body = Vec::new();
        if response.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut body).is_err() {
            return Config::Pending("the runtime configuration could not be read".into());
        }
        if body.len() as u64 > MAX_CONFIG_BYTES {
            return Config::Rejected("The Cognee UI runtime configuration is unexpectedly large.".into());
        }
        match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(value) => Config::Backend(value.get("backendUrl").and_then(|url| url.as_str()).map(str::to_string)),
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
    fn query_processes() -> Result<Vec<Row>, String> { native::query_processes() }
    #[cfg(windows)]
    /// One fixed, read-only query of Python/Node processes (id, creation time, path, command line), bounded to 15 seconds.
    fn query_processes() -> Result<Vec<Row>, String> {
        let shell = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| "C:/Windows".into()).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut command = std::process::Command::new(shell);
        command.args(["-NoProfile", "-NonInteractive", "-Command", CIM_SCRIPT]);
        crate::code_diff::no_console(&mut command);
        let output = crate::code_diff::run_timed(command, None, DISCOVERY_TIMEOUT, "PowerShell was not found, so no running Cognee backend could be identified.")
            .map_err(|error| format!("The process list could not be read: {error}"))?;
        if !output.status.success() {
            return Err(format!("The process list could not be read: {}", shown(&String::from_utf8_lossy(&output.stderr))));
        }
        if output.stdout.len() > 4 * 1024 * 1024 {
            return Err("The process list is unexpectedly large.".into());
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let text = text.trim().trim_start_matches('\u{feff}');
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let value: serde_json::Value = serde_json::from_str(text).map_err(|_| "The process list was not understood.".to_string())?;
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
                    exe: item.get("exe").and_then(|value| value.as_str()).unwrap_or_default().to_string(),
                    cmd: item.get("cmd").and_then(|value| value.as_str()).unwrap_or_default().to_string(),
                })
            })
            .collect())
    }

    /// Whether this process was started from the installed environment's interpreter. Windows compares the interpreter
    /// path itself. A Unix venv (and a Nix environment) is a directory of links, so the process reports a sibling such as
    /// `python3.13` in the same real `bin` directory: the comparison is on that directory, never on the shared base interpreter.
    fn launched_from_venv(program: &str, exe: &str, venv_python: &Path) -> bool {
        #[cfg(unix)]
        {
            let _ = exe;
            let bin = |path: &Path| path.parent().and_then(|dir| fs::canonicalize(dir).ok());
            matches!((bin(Path::new(program)), bin(venv_python)), (Some(a), Some(b)) if a == b)
        }
        #[cfg(not(unix))]
        {
            fsio::same_path(Path::new(program), venv_python) || (!exe.is_empty() && fsio::same_path(Path::new(exe), venv_python))
        }
    }

    /// Backends among `rows` whose command line is exactly the installed Cognee uvicorn app and whose process, still
    /// the same (`creation` is read after the listener table), owns a numeric loopback listener on its port. WMI
    /// creation times have microsecond resolution, hence the tick comparison at that granularity.
    pub(crate) fn verify_backends(rows: &[Row], listeners: &[Listener], venv_python: &Path, creation: &dyn Fn(u32) -> Option<u64>) -> Vec<Backend> {
        let mut verified = Vec::new();
        for row in rows {
            let tokens = split_command_line(&row.cmd);
            let Some(program) = tokens.first() else { continue };
            let installed = launched_from_venv(program, &row.exe, venv_python);
            let Some(port) = installed.then(|| uvicorn_port(&tokens)).flatten() else { continue };
            for listener in listeners.iter().filter(|listener| listener.pid == row.pid && listener.port == port && listener.address.is_loopback()) {
                if creation(row.pid).is_some_and(|created| created / 10 == row.created / 10) {
                    verified.push(Backend { port, pid: row.pid, address: listener.address });
                }
            }
        }
        verified
    }

    fn discover_backends(venv_python: &Path) -> Result<Vec<Backend>, String> {
        let rows = query_processes()?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let listeners = native::tcp_listeners()?;
        Ok(verify_backends(&rows, &listeners, venv_python, &native::creation_time))
    }


    #[cfg(unix)]
    pub(crate) mod native { include!("cognee_ui_unix.rs"); }
    #[cfg(windows)]
    pub(crate) mod native {
        use super::Listener;
        use std::collections::HashMap;
        use std::ffi::c_void;
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};

        #[link(name = "iphlpapi")]
        extern "system" {
            fn GetExtendedTcpTable(table: *mut c_void, size: *mut u32, order: i32, family: u32, class: i32, reserved: u32) -> u32;
        }

        #[repr(C)]
        struct ProcessEntry {
            size: u32,
            usage: u32,
            pid: u32,
            heap: usize,
            module: u32,
            threads: u32,
            parent: u32,
            priority: i32,
            flags: u32,
            exe: [u16; 260],
        }

        #[link(name = "kernel32")]
        extern "system" {
            fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> HANDLE;
            fn Process32FirstW(snapshot: HANDLE, entry: *mut ProcessEntry) -> i32;
            fn Process32NextW(snapshot: HANDLE, entry: *mut ProcessEntry) -> i32;
        }

        const AF_INET: u32 = 2;
        const AF_INET6: u32 = 23;
        const TCP_TABLE_OWNER_PID_LISTENER: i32 = 3;
        const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
        const TH32CS_SNAPPROCESS: u32 = 2;

        /// The raw table bytes; the buffer grows at most three times.
        fn table(family: u32) -> Result<Vec<u8>, String> {
            let mut buffer: Vec<u8> = Vec::new();
            let mut size = 0u32;
            for _ in 0..4 {
                let pointer = if buffer.is_empty() { std::ptr::null_mut() } else { buffer.as_mut_ptr().cast() };
                // SAFETY: `pointer` is null or points to `buffer`'s `size` writable bytes.
                let status = unsafe { GetExtendedTcpTable(pointer, &mut size, 0, family, TCP_TABLE_OWNER_PID_LISTENER, 0) };
                match status {
                    0 => {
                        buffer.truncate(size as usize);
                        return Ok(buffer);
                    }
                    ERROR_INSUFFICIENT_BUFFER => buffer = vec![0u8; size as usize],
                    other => return Err(format!("GetExtendedTcpTable failed with Windows error {other}")),
                }
            }
            Err("The TCP listener table kept changing size.".into())
        }

        fn u32_at(bytes: &[u8], offset: usize) -> u32 {
            u32::from_le_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]])
        }

        /// The port is the low 16 bits of a DWORD in network byte order.
        fn port_at(bytes: &[u8], offset: usize) -> u16 {
            u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
        }

        pub(crate) fn parse_v4(bytes: &[u8]) -> Result<Vec<Listener>, String> {
            const ROW: usize = 24;
            if bytes.len() < 4 {
                return Err("The IPv4 listener table is truncated.".into());
            }
            let count = u32_at(bytes, 0) as usize;
            if count.checked_mul(ROW).and_then(|rows| rows.checked_add(4)).is_none_or(|end| end > bytes.len()) {
                return Err("The IPv4 listener table is inconsistent.".into());
            }
            Ok((0..count)
                .map(|index| {
                    let row = &bytes[4 + index * ROW..4 + (index + 1) * ROW];
                    Listener { address: IpAddr::V4(Ipv4Addr::new(row[4], row[5], row[6], row[7])), port: port_at(row, 8), pid: u32_at(row, 20) }
                })
                .collect())
        }

        pub(crate) fn parse_v6(bytes: &[u8]) -> Result<Vec<Listener>, String> {
            const ROW: usize = 56;
            if bytes.len() < 4 {
                return Err("The IPv6 listener table is truncated.".into());
            }
            let count = u32_at(bytes, 0) as usize;
            if count.checked_mul(ROW).and_then(|rows| rows.checked_add(4)).is_none_or(|end| end > bytes.len()) {
                return Err("The IPv6 listener table is inconsistent.".into());
            }
            Ok((0..count)
                .map(|index| {
                    let row = &bytes[4 + index * ROW..4 + (index + 1) * ROW];
                    let mut octets = [0u8; 16];
                    octets.copy_from_slice(&row[0..16]);
                    Listener { address: IpAddr::V6(Ipv6Addr::from(octets)), port: port_at(row, 20), pid: u32_at(row, 52) }
                })
                .collect())
        }

        /// Every TCP listener of this computer (IPv4 and IPv6) with its owning process id.
        pub(crate) fn tcp_listeners() -> Result<Vec<Listener>, String> {
            let mut all = parse_v4(&table(AF_INET)?)?;
            all.extend(parse_v6(&table(AF_INET6)?)?);
            Ok(all)
        }

        /// The creation time (100 ns FILETIME ticks) of a still running process; `None` when it cannot be inspected.
        pub(crate) fn creation_time(pid: u32) -> Option<u64> {
            crate::process_tree::process_creation_time(pid)
        }

        /// Parent process ids of the running processes (one snapshot).
        pub(crate) fn parent_map() -> Result<HashMap<u32, u32>, String> {
            // SAFETY: the snapshot handle is closed on every path; `entry` is a correctly sized, initialised struct.
            unsafe {
                let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
                if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
                    return Err(format!("The process snapshot failed: {}", std::io::Error::last_os_error()));
                }
                let mut entry: ProcessEntry = std::mem::zeroed();
                entry.size = std::mem::size_of::<ProcessEntry>() as u32;
                let mut map = HashMap::new();
                let mut more = Process32FirstW(snapshot, &mut entry);
                while more != 0 {
                    map.insert(entry.pid, entry.parent);
                    more = Process32NextW(snapshot, &mut entry);
                }
                CloseHandle(snapshot);
                Ok(map)
            }
        }

        /// `pid` is the root or a verified descendant: every ancestor up to the root exists, is older than its child and
        /// the root still has the creation time it was started with. Unknown ancestry is not ownership.
        pub(crate) fn in_tree(parents: &HashMap<u32, u32>, root: u32, root_created: u64, pid: u32) -> bool {
            let Some(mut child_created) = creation_time(pid) else { return false };
            if pid == root {
                return child_created == root_created;
            }
            let mut current = pid;
            for _ in 0..64 {
                let Some(&parent) = parents.get(&current) else { return false };
                if parent == 0 {
                    return false;
                }
                let Some(parent_created) = creation_time(parent) else { return false };
                if parent_created > child_created {
                    return false;
                }
                if parent == root {
                    return parent_created == root_created;
                }
                current = parent;
                child_created = parent_created;
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(line: &str) -> Vec<String> {
        split_command_line(line)
    }

    #[test]
    fn command_lines_split_like_the_windows_runtime() {
        assert_eq!(
            tokens(r#""C:\Program Files\Py\python.exe" -m uvicorn cognee.api.client:app --host localhost --port 8000"#),
            ["C:\\Program Files\\Py\\python.exe", "-m", "uvicorn", "cognee.api.client:app", "--host", "localhost", "--port", "8000"]
        );
        assert_eq!(tokens(r#"p "a b" c\ "d\"e" "" f"#), ["p", "a b", "c\\", "d\"e", "", "f"]);
        assert_eq!(tokens(r#"p a\\"b c" d"#), ["p", "a\\b c", "d"], "an even run of backslashes before a quote halves and toggles quoting");
        assert_eq!(tokens("  \t "), Vec::<String>::new());
        assert_eq!(tokens("C:\\x\\python.exe"), ["C:\\x\\python.exe"]);
    }

    fn port(line: &str) -> Option<u16> {
        uvicorn_port(&tokens(line))
    }

    #[test]
    fn only_the_exact_cognee_app_on_a_loopback_host_is_a_backend_command() {
        let base = "py.exe -m uvicorn cognee.api.client:app";
        assert_eq!(port(base), Some(8000), "uvicorn's defaults are 127.0.0.1:8000");
        assert_eq!(port(&format!("{base} --host localhost --port 8123")), Some(8123));
        assert_eq!(port(&format!("{base} --host=127.0.0.1 --port=9000 --log-level info --no-access-log")), Some(9000));
        assert_eq!(port(&format!("{base} --host ::1")), Some(8000));
        for bad in [
            "py.exe -m uvicorn other.module:app",
            "py.exe -m gunicorn cognee.api.client:app",
            "py.exe uvicorn cognee.api.client:app",
            "py.exe",
            "py.exe -m uvicorn cognee.api.client:app --host 0.0.0.0",
            "py.exe -m uvicorn cognee.api.client:app --host example.com",
            "py.exe -m uvicorn cognee.api.client:app --port 0",
            "py.exe -m uvicorn cognee.api.client:app --port nope",
            "py.exe -m uvicorn cognee.api.client:app --port",
            "py.exe -m uvicorn cognee.api.client:app --app-dir C:\\evil",
            "py.exe -m uvicorn cognee.api.client:app --ssl-keyfile k --port 8000",
            "py.exe -m uvicorn cognee.api.client:app --no-access-log=1",
        ] {
            assert_eq!(port(bad), None, "{bad}");
        }
    }

    fn backend(port: u16, pid: u32) -> Backend {
        Backend { port, pid, address: IpAddr::from([127, 0, 0, 1]) }
    }

    #[test]
    fn port_8000_wins_otherwise_exactly_one_backend_must_be_verified() {
        assert!(choose_backend(&[]).unwrap_err().contains("No running local Cognee backend"));
        assert_eq!(choose_backend(&[backend(9000, 3)]).unwrap().port, 9000);
        assert_eq!(choose_backend(&[backend(9000, 3), backend(9000, 4)]).unwrap().port, 9000, "two processes of one backend are one port");
        assert!(choose_backend(&[backend(9000, 3), backend(9001, 4)]).unwrap_err().contains("9000, 9001"));
        assert_eq!(choose_backend(&[backend(9001, 4), backend(8000, 5), backend(9000, 3)]).unwrap().pid, 5);
    }

    #[test]
    fn localhost_must_resolve_only_to_loopback() {
        assert!(all_loopback(&[IpAddr::from([127, 0, 0, 1]), IpAddr::from(std::net::Ipv6Addr::LOCALHOST)]));
        assert!(!all_loopback(&[]));
        assert!(!all_loopback(&[IpAddr::from([127, 0, 0, 1]), IpAddr::from([10, 0, 0, 5])]));
        assert!(!all_loopback(&[IpAddr::from([0, 0, 0, 0])]));
    }

    #[test]
    fn node_floor_and_version_parsing_fall_back_to_the_documented_floor() {
        assert_eq!(parse_version("22.15.0"), Some((22, 15, 0)));
        assert_eq!(parse_version("v20.9"), Some((20, 9, 0)));
        assert_eq!(parse_version("nope"), None);
        assert_eq!(node_floor(&serde_json::json!({"engines": {"node": ">=20.9.0"}})), (20, 9, 0));
        assert_eq!(node_floor(&serde_json::json!({"engines": {"node": ">=22.1.4"}})), (22, 1, 4));
        assert_eq!(node_floor(&serde_json::json!({"engines": {"node": "^20 || ^22"}})), DEFAULT_NODE_FLOOR);
        assert_eq!(node_floor(&serde_json::json!({})), DEFAULT_NODE_FLOOR);
    }
}
