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
use crate::knowledge::dto::{
    CogneeOpenResult, CogneeOpenState, CogneeState, CogneeUiCapability, CogneeUiState,
};
use crate::knowledge::fsio;
use crate::knowledge::KnowledgeService;
use std::fs;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The only Next.js version whose internal startup flags were inspected.
pub(crate) const SUPPORTED_NEXT: &str = "16.3.4";
pub(super) const FRONTEND_PORT: u16 = 3000;
const BACKEND_PORT: u16 = 8000;
pub(super) const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
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
    Blocked {
        state,
        message: message.into(),
    }
}

/// The verified installed pieces a start needs.
#[derive(Clone, Debug)]
pub(crate) struct Components {
    pub frontend: PathBuf,
    pub node: PathBuf,
    pub next_entry: PathBuf,
    pub swc_binding: PathBuf,
    /// The installed venv's interpreter that a verified backend must be running.
    pub venv_python: PathBuf,
}

fn forward(path: &Path) -> String {
    fsio::shown(&path.to_string_lossy().replace('\\', "/"))
}

fn unreadable(path: &Path, error: impl std::fmt::Display) -> Blocked {
    blocked(
        CogneeUiState::Unreadable,
        format!("Cannot inspect {}: {error}", forward(path)),
    )
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
    (&mut file)
        .take(MAX_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| unreadable(path, error))?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return Err(blocked(
            CogneeUiState::Unreadable,
            format!("{} is unexpectedly large", forward(path)),
        ));
    }
    serde_json::from_slice(&bytes).map(Some).map_err(|_| {
        blocked(
            CogneeUiState::Unreadable,
            format!("{} is not valid JSON", forward(path)),
        )
    })
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.trim().trim_start_matches('v').split('.');
    let mut next = || parts.next().and_then(|part| part.parse::<u32>().ok());
    Some((next()?, next()?, next().unwrap_or(0)))
}

/// `>=X.Y.Z` is the only engines form understood; anything else keeps the default floor.
fn node_floor(next: &serde_json::Value) -> (u32, u32, u32) {
    next.pointer("/engines/node")
        .and_then(|value| value.as_str())
        .and_then(|text| text.trim().strip_prefix(">="))
        .and_then(parse_version)
        .unwrap_or(DEFAULT_NODE_FLOOR)
}

/// The Node.js executable: an explicit absolute override (isolated runs) or the first `node.exe` on an absolute `PATH` entry.
fn find_node(node_override: Option<&Path>) -> Result<Option<PathBuf>, Blocked> {
    if let Some(path) = node_override {
        return Ok(if path.is_absolute() && is_file(path)? {
            Some(path.to_path_buf())
        } else {
            None
        });
    }
    let name = if cfg!(windows) { "node.exe" } else { "node" };
    let Some(paths) = std::env::var_os("PATH") else {
        return Ok(None);
    };
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
    crate::knowledge::integrations::executable_version(path)
        .as_deref()
        .and_then(parse_version)
}

/// Inspects the installed frontend, its Node packages and Node.js. Reads metadata only; never runs, imports or writes.
pub(crate) fn inspect_components(
    cognee_folder: &Path,
    home: &Path,
    node_override: Option<&Path>,
) -> Result<Components, Blocked> {
    if !cfg!(any(
        all(windows, target_arch = "x86_64"),
        all(unix, any(target_arch = "x86_64", target_arch = "aarch64"))
    )) {
        return Err(blocked(
            CogneeUiState::Unsupported,
            "This platform has no supported Next.js native binding.",
        ));
    }
    let venv = cognee_folder.join("venv");
    let frontend = locate_frontend(&python_site(&venv)?, home)?;
    let modules = frontend_modules(&frontend)?;
    let next_json = read_next(&modules)?;
    let next_entry = modules.join("next").join("dist").join("bin").join("next");
    let binding = if cfg!(windows) {
        "win32-x64-msvc"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-arm64"
    } else if cfg!(target_os = "macos") {
        "darwin-x64"
    } else if cfg!(target_arch = "aarch64") {
        "linux-arm64-gnu"
    } else {
        "linux-x64-gnu"
    };
    let swc_package = modules.join("@next").join(format!("swc-{binding}"));
    let swc_binding = swc_package.join(format!("next-swc.{binding}.node"));
    require_installed(&modules, &next_entry, &swc_binding)?;
    check_swc_version(&swc_package)?;
    let node = find_node(node_override)?.ok_or_else(|| blocked(CogneeUiState::MissingDependencies, "Node.js was not found on this computer's PATH; the Cognee web interface needs it. OMP Manager never installs it."))?;
    check_node(&node, &next_json)?;
    Ok(Components {
        frontend,
        node,
        next_entry,
        swc_binding,
        venv_python: if cfg!(windows) {
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        },
    })
}

#[cfg(windows)]
fn python_site(venv: &Path) -> Result<PathBuf, Blocked> {
    Ok(venv.join("Lib").join("site-packages"))
}

#[cfg(unix)]
fn python_site(venv: &Path) -> Result<PathBuf, Blocked> {
    let lib = venv.join("lib");
    let mut sites = match fs::read_dir(&lib) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("python"))
            })
            .map(|path| path.join("site-packages"))
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(unreadable(&lib, error)),
    };
    sites.sort();
    Ok(sites
        .into_iter()
        .find(|path| path.join("cognee").is_dir())
        .unwrap_or_else(|| lib.join("site-packages")))
}

/// The same candidates, in the same order, as Cognee's own frontend lookup: three development locations next to the
/// package, then the download cache. Looking is read-only; the cache folder is never created.
fn locate_frontend(site: &Path, home: &Path) -> Result<PathBuf, Blocked> {
    let candidates = [
        site.join("cognee-frontend"),
        site.join("cognee").join("cognee-frontend"),
        site.join("cognee").join("api").join("cognee-frontend"),
        home.join(".cognee").join("ui-cache").join("frontend"),
    ];
    for candidate in candidates {
        if is_file(&candidate.join("package.json"))? {
            return Ok(candidate);
        }
    }
    Err(blocked(
        CogneeUiState::MissingFrontend,
        "Cognee is installed, but its web interface (the cognee-frontend package) is not installed on this computer. OMP Manager never downloads or installs it; install it yourself, then check again.",
    ))
}

/// Confirms the folder is the `cognee-frontend` package with a `node_modules` directory; returns that directory.
fn frontend_modules(frontend: &Path) -> Result<PathBuf, Blocked> {
    let package = read_json(&frontend.join("package.json"))?
        .ok_or_else(|| unreadable(&frontend.join("package.json"), "it disappeared"))?;
    if package.get("name").and_then(|name| name.as_str()) != Some("cognee-frontend") {
        return Err(blocked(
            CogneeUiState::Unsupported,
            format!(
                "{} is not the cognee-frontend package, so it is not started.",
                forward(frontend)
            ),
        ));
    }
    let modules = frontend.join("node_modules");
    if stat(&modules)?.is_none_or(|meta| !meta.is_dir()) {
        return Err(blocked(
            CogneeUiState::MissingDependencies,
            format!("The Cognee web interface is present in {}, but its Node packages are not installed (no node_modules). OMP Manager never installs them; run the install yourself, then try again.", forward(frontend)),
        ));
    }
    Ok(modules)
}

/// The installed `next` package metadata, which must be the one supported Next.js version.
fn read_next(modules: &Path) -> Result<serde_json::Value, Blocked> {
    let next_json = match read_json(&modules.join("next").join("package.json"))? {
        Some(json) if json.get("name").and_then(|name| name.as_str()) == Some("next") => json,
        Some(_) => return Err(blocked(CogneeUiState::Unsupported, "node_modules/next is not the Next.js package, so the interface is not started.")),
        None => {
            return Err(blocked(
                CogneeUiState::MissingDependencies,
                "The Cognee web interface needs Next.js, which is not installed in its node_modules. OMP Manager never installs it.",
            ))
        }
    };
    let version = next_json
        .get("version")
        .and_then(|version| version.as_str())
        .unwrap_or("unknown");
    if version != SUPPORTED_NEXT {
        return Err(blocked(
            CogneeUiState::Unsupported,
            format!("Next.js {version} is installed for the Cognee web interface; only Next.js {SUPPORTED_NEXT} was inspected and is supported, so it is not started."),
        ));
    }
    Ok(next_json)
}

/// Every file the frontend needs must exist; all missing ones are named together.
fn require_installed(modules: &Path, next_entry: &Path, swc_binding: &Path) -> Result<(), Blocked> {
    let mut missing = Vec::new();
    for (path, label) in [
        (next_entry, "next/dist/bin/next"),
        (
            &modules.join("typescript").join("lib").join("typescript.js"),
            "typescript",
        ),
        (
            &modules.join("@types").join("react").join("index.d.ts"),
            "@types/react",
        ),
        (
            &modules.join("@types").join("node").join("index.d.ts"),
            "@types/node",
        ),
        (
            swc_binding,
            "the platform-specific @next/swc native binding",
        ),
    ] {
        if !is_file(path)? {
            missing.push(label);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(blocked(
        CogneeUiState::MissingDependencies,
        format!("These installed packages of the Cognee web interface are missing: {}. OMP Manager never installs or downloads them; install them yourself, then try again.", missing.join(", ")),
    ))
}

fn check_swc_version(swc_package: &Path) -> Result<(), Blocked> {
    match read_json(&swc_package.join("package.json"))?.as_ref().and_then(|json| json.get("version")).and_then(|version| version.as_str()) {
        Some(SUPPORTED_NEXT) => Ok(()),
        Some(other) => Err(blocked(CogneeUiState::Unsupported, format!("The installed Next.js native binding is version {other}, not {SUPPORTED_NEXT}, so the interface is not started."))),
        None => Err(blocked(CogneeUiState::MissingDependencies, "The installed Next.js native binding has no readable package.json.")),
    }
}

#[cfg(windows)]
fn check_node(node: &Path, next_json: &serde_json::Value) -> Result<(), Blocked> {
    let floor = node_floor(next_json);
    match node_version(node) {
        Some(found) if found >= floor => Ok(()),
        Some(found) => Err(blocked(
            CogneeUiState::Unsupported,
            format!(
                "Node.js {}.{}.{} is installed; Next.js {SUPPORTED_NEXT} needs {}.{}.{} or later.",
                found.0, found.1, found.2, floor.0, floor.1, floor.2
            ),
        )),
        None => Err(blocked(
            CogneeUiState::Unreadable,
            format!(
                "The version of Node.js at {} could not be read.",
                forward(node)
            ),
        )),
    }
}

/// Only Windows identifies the Node.js version; elsewhere the executable is accepted as found.
#[cfg(not(windows))]
fn check_node(_node: &Path, _next_json: &serde_json::Value) -> Result<(), Blocked> {
    Ok(())
}

/// The passive capability shown on page entry: the same inspection as a click, nothing more.
pub(crate) fn capability(
    cognee_folder: &Path,
    home: &Path,
    node_override: Option<&Path>,
) -> CogneeUiCapability {
    match inspect_components(cognee_folder, home, node_override) {
        Ok(_) => CogneeUiCapability {
            state: CogneeUiState::Ready,
            message: None,
        },
        Err(blocked) => CogneeUiCapability {
            state: blocked.state,
            message: Some(blocked.message),
        },
    }
}

// ---------- backend identification (pure parts) ----------

/// A backend this app verified: its process owns a numeric loopback listener on `port`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Backend {
    pub port: u16,
    pub pid: u32,
    pub address: IpAddr,
}

/// Windows command-line splitting (`CommandLineToArgvW` rules; the program name follows the simpler first-token rule).
pub(crate) fn split_command_line(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();
    let mut i = skip_blanks(&chars, 0);
    if i < chars.len() {
        tokens.push(read_program(&chars, &mut i));
    }
    loop {
        i = skip_blanks(&chars, i);
        if i >= chars.len() {
            break;
        }
        tokens.push(read_token(&chars, &mut i));
    }
    tokens
}

fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

fn skip_blanks(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && is_blank(chars[i]) {
        i += 1;
    }
    i
}

/// Characters from `*i` up to (not including) the first one for which `stop` holds.
fn take_until(chars: &[char], i: &mut usize, stop: impl Fn(char) -> bool) -> String {
    let mut taken = String::new();
    while *i < chars.len() && !stop(chars[*i]) {
        taken.push(chars[*i]);
        *i += 1;
    }
    taken
}

/// The program name: a quoted name runs to the next quote, otherwise to the next blank; no backslash rules.
fn read_program(chars: &[char], i: &mut usize) -> String {
    if chars[*i] != '"' {
        return take_until(chars, i, is_blank);
    }
    *i += 1;
    let program = take_until(chars, i, |c| c == '"');
    *i += 1;
    program
}

/// One argument under `CommandLineToArgvW` rules; `*i` is on its first character.
fn read_token(chars: &[char], i: &mut usize) -> String {
    let mut token = String::new();
    let mut quoted = false;
    while *i < chars.len() {
        let start = *i;
        while *i < chars.len() && chars[*i] == '\\' {
            *i += 1;
        }
        let slashes = *i - start;
        if *i < chars.len() && chars[*i] == '"' {
            apply_quote(chars, i, slashes, &mut quoted, &mut token);
            continue;
        }
        token.extend(std::iter::repeat_n('\\', slashes));
        if *i >= chars.len() || (!quoted && is_blank(chars[*i])) {
            break;
        }
        token.push(chars[*i]);
        *i += 1;
    }
    token
}

/// A `"` after `slashes` backslashes: half of them survive; an odd count makes the quote literal, `""` inside quotes
/// is one literal quote, anything else toggles quoting.
fn apply_quote(
    chars: &[char],
    i: &mut usize,
    slashes: usize,
    quoted: &mut bool,
    token: &mut String,
) {
    token.extend(std::iter::repeat_n('\\', slashes / 2));
    if slashes % 2 == 1 {
        token.push('"');
        *i += 1;
    } else if *quoted && chars.get(*i + 1) == Some(&'"') {
        token.push('"');
        *i += 2;
    } else {
        *quoted = !*quoted;
        *i += 1;
    }
}

/// The port of a command line that is exactly `<python> -m uvicorn cognee.api.client:app [--host <loopback>] [--port N] ...`
/// with only benign options. `None` for anything else, so a lookalike is never trusted.
pub(crate) fn uvicorn_port(tokens: &[String]) -> Option<u16> {
    if tokens.len() < 4
        || tokens[1] != "-m"
        || tokens[2] != "uvicorn"
        || tokens[3] != "cognee.api.client:app"
    {
        return None;
    }
    let mut host = "127.0.0.1";
    let mut port = BACKEND_PORT;
    let mut i = 4;
    while i < tokens.len() {
        let token = tokens[i].as_str();
        let (name, inline) = match token.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (token, None),
        };
        match name {
            "--host" | "--port" | "--log-level" => {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        i += 1;
                        tokens.get(i)?.as_str()
                    }
                };
                match name {
                    "--host" => host = value,
                    "--port" => port = value.parse().ok().filter(|port| *port != 0)?,
                    _ => {}
                }
            }
            "--no-access-log" | "--access-log" | "--use-colors" | "--no-use-colors"
                if inline.is_none() => {}
            _ => return None,
        }
        i += 1;
    }
    let loopback = matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1" | "[::1]"
    );
    loopback.then_some(port)
}

/// Port 8000 when verified, otherwise exactly one verified backend.
pub(crate) fn choose_backend(verified: &[Backend]) -> Result<Backend, String> {
    let mut ordered: Vec<&Backend> = verified.iter().collect();
    ordered.sort_by_key(|backend| (backend.port, backend.pid, backend.address));
    if let Some(found) = ordered.iter().find(|backend| backend.port == BACKEND_PORT) {
        return Ok(**found);
    }
    match ordered.as_slice() {
        [] => Err("No running local Cognee backend was found. Start your Cognee backend yourself (OMP Manager never starts it or runs migrations), then try again.".into()),
        [first, rest @ ..] if rest.iter().all(|backend| backend.port == first.port) => Ok(**first),
        many => {
            let mut ports: Vec<u16> = many.iter().map(|backend| backend.port).collect();
            ports.dedup();
            Err(format!("Several Cognee backends are running (ports {}), so none is chosen. Leave one running, or use port {BACKEND_PORT}.", ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ")))
        }
    }
}
/// Every address a name resolved to must be loopback (and there must be one).
#[must_use]
pub(crate) fn all_loopback(addresses: &[IpAddr]) -> bool {
    !addresses.is_empty() && addresses.iter().all(IpAddr::is_loopback)
}

pub(super) fn result(
    state: CogneeOpenState,
    url: Option<String>,
    message: impl Into<String>,
) -> CogneeOpenResult {
    CogneeOpenResult {
        state,
        url,
        message: Some(message.into()),
    }
}

/// Unix has no Windows executable-version resource. Only an explicit Open executes Node; passive discovery remains read-only.
#[cfg(unix)]
fn verify_unix_node(components: &Components) -> Result<(), String> {
    let next = read_json(&components.frontend.join("node_modules/next/package.json"))
        .map_err(|e| e.message)?
        .ok_or_else(|| "The installed Next.js package disappeared.".to_string())?;
    let floor = node_floor(&next);
    let mut command = std::process::Command::new(&components.node);
    command.arg("--version");
    let output = crate::code_diff::run_timed(
        command,
        None,
        Duration::from_secs(15),
        "Node.js version inspection timed out.",
    )?;
    let version = output
        .status
        .success()
        .then(|| parse_version(&String::from_utf8_lossy(&output.stdout)))
        .flatten()
        .ok_or_else(|| "The installed Node.js version could not be read.".to_string())?;
    if version < floor {
        return Err(format!(
            "Node.js {}.{}.{} is too old; this Next.js requires {}.{}.{}.",
            version.0, version.1, version.2, floor.0, floor.1, floor.2
        ));
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
            CogneeState::Unreadable => {
                return missing(
                    capability
                        .message
                        .unwrap_or_else(|| "Cognee could not be inspected.".into()),
                )
            }
            CogneeState::Absent => {
                return missing("Cognee is not installed on this computer.".into())
            }
            CogneeState::Installed => {}
        }
        let Some(folder) = capability.installation_folder else {
            return missing("The Cognee installation folder is unknown.".into());
        };
        match inspect_components(
            Path::new(&folder),
            &self.home,
            self.env.cognee_node.as_deref(),
        ) {
            Ok(components) => {
                #[cfg(unix)]
                if let Err(message) = verify_unix_node(&components) {
                    return result(CogneeOpenState::MissingComponent, None, message);
                }
                self.cognee_ui.open(&components)
            }
            Err(blocked) => result(
                if blocked.state == CogneeUiState::Unsupported {
                    CogneeOpenState::Unsupported
                } else {
                    CogneeOpenState::MissingComponent
                },
                None,
                blocked.message,
            ),
        }
    }
}

#[cfg(not(any(windows, unix)))]
pub(crate) struct CogneeUi;

#[cfg(not(any(windows, unix)))]
impl CogneeUi {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn shutdown(&self) {}

    pub(crate) fn open(&self, _components: &Components) -> CogneeOpenResult {
        result(
            CogneeOpenState::Unsupported,
            None,
            "OMP Manager can start the Cognee UI only on Windows (x64).",
        )
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
            tokens(
                r#""C:\Program Files\Py\python.exe" -m uvicorn cognee.api.client:app --host localhost --port 8000"#
            ),
            [
                "C:\\Program Files\\Py\\python.exe",
                "-m",
                "uvicorn",
                "cognee.api.client:app",
                "--host",
                "localhost",
                "--port",
                "8000"
            ]
        );
        assert_eq!(
            tokens(r#"p "a b" c\ "d\"e" "" f"#),
            ["p", "a b", "c\\", "d\"e", "", "f"]
        );
        assert_eq!(
            tokens(r#"p a\\"b c" d"#),
            ["p", "a\\b c", "d"],
            "an even run of backslashes before a quote halves and toggles quoting"
        );
        assert_eq!(tokens("  \t "), Vec::<String>::new());
        assert_eq!(tokens("C:\\x\\python.exe"), ["C:\\x\\python.exe"]);
    }

    fn port(line: &str) -> Option<u16> {
        uvicorn_port(&tokens(line))
    }

    #[test]
    fn only_the_exact_cognee_app_on_a_loopback_host_is_a_backend_command() {
        let base = "py.exe -m uvicorn cognee.api.client:app";
        assert_eq!(
            port(base),
            Some(8000),
            "uvicorn's defaults are 127.0.0.1:8000"
        );
        assert_eq!(
            port(&format!("{base} --host localhost --port 8123")),
            Some(8123)
        );
        assert_eq!(
            port(&format!(
                "{base} --host=127.0.0.1 --port=9000 --log-level info --no-access-log"
            )),
            Some(9000)
        );
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
        Backend {
            port,
            pid,
            address: IpAddr::from([127, 0, 0, 1]),
        }
    }

    #[test]
    fn port_8000_wins_otherwise_exactly_one_backend_must_be_verified() {
        assert!(choose_backend(&[])
            .unwrap_err()
            .contains("No running local Cognee backend"));
        assert_eq!(choose_backend(&[backend(9000, 3)]).unwrap().port, 9000);
        assert_eq!(
            choose_backend(&[backend(9000, 3), backend(9000, 4)])
                .unwrap()
                .port,
            9000,
            "two processes of one backend are one port"
        );
        assert!(choose_backend(&[backend(9000, 3), backend(9001, 4)])
            .unwrap_err()
            .contains("9000, 9001"));
        assert_eq!(
            choose_backend(&[backend(9001, 4), backend(8000, 5), backend(9000, 3)])
                .unwrap()
                .pid,
            5
        );
    }

    #[test]
    fn localhost_must_resolve_only_to_loopback() {
        assert!(all_loopback(&[
            IpAddr::from([127, 0, 0, 1]),
            IpAddr::from(std::net::Ipv6Addr::LOCALHOST)
        ]));
        assert!(!all_loopback(&[]));
        assert!(!all_loopback(&[
            IpAddr::from([127, 0, 0, 1]),
            IpAddr::from([10, 0, 0, 5])
        ]));
        assert!(!all_loopback(&[IpAddr::from([0, 0, 0, 0])]));
    }

    #[test]
    fn node_floor_and_version_parsing_fall_back_to_the_documented_floor() {
        assert_eq!(parse_version("22.15.0"), Some((22, 15, 0)));
        assert_eq!(parse_version("v20.9"), Some((20, 9, 0)));
        assert_eq!(parse_version("nope"), None);
        assert_eq!(
            node_floor(&serde_json::json!({"engines": {"node": ">=20.9.0"}})),
            (20, 9, 0)
        );
        assert_eq!(
            node_floor(&serde_json::json!({"engines": {"node": ">=22.1.4"}})),
            (22, 1, 4)
        );
        assert_eq!(
            node_floor(&serde_json::json!({"engines": {"node": "^20 || ^22"}})),
            DEFAULT_NODE_FLOOR
        );
        assert_eq!(node_floor(&serde_json::json!({})), DEFAULT_NODE_FLOOR);
    }
}
