#[cfg(any(windows, unix))]
use super::native;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Weak};
use std::time::Duration;

pub type Result<T, E = String> = std::result::Result<T, E>;
const MAX_FRAME: usize = 64 * 1024 * 1024;
const MAX_NATIVE_WORKERS: usize = 16;
#[cfg(windows)]
// Cross-process WS_CHILD hosting painted Chrome but broke its omnibox and shortcuts in the
// Windows probe. Docking the real top-level window is used instead; failures stay explicit.
pub const EMBEDDING_ERROR: &str = "Browser could not be embedded";
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(not(windows), derive(Default))]
pub struct BrowserSettings {
    pub headless: bool,
    pub embedded: bool,
}
#[cfg(windows)]
impl Default for BrowserSettings {
    fn default() -> Self {
        Self {
            headless: false,
            embedded: cfg!(windows),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserInfo {
    pub instance_id: String,
    pub generation: u64,
    pub epoch: u64,
    pub state: String,
    pub headless: bool,
    pub embedded: bool,
    pub pid: Option<u32>,
    pub error: Option<String>,
}
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
/// A live, native-verified Chrome root process owned by one terminal's browser. Descendants join by ancestry; the
/// identity is `pid` + `creation_time_100ns` (FILETIME), never the PID alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserRoot {
    pub instance_id: String,
    pub generation: u64,
    pub pid: u32,
    pub creation_time_100ns: u64,
}
struct Owner {
    generation: u64,
    token: String,
    info: BrowserInfo,
    /// Helper run that produced `info`; a retired helper's late frames must not overwrite its successor's.
    session: Option<String>,
}
struct State {
    settings: BrowserSettings,
    owners: HashMap<String, Owner>,
    closed: bool,
}
struct Helper {
    session: String,
    child: Mutex<Child>,
    input: Mutex<ChildStdin>,
    pending: Mutex<HashMap<String, mpsc::SyncSender<Result<Value>>>>,
    /// Pending launch-capable requests (request id -> instance, generation): the only requests that may ask the
    /// native side to start a browser.
    launches: Mutex<HashMap<String, (String, u64)>>,
    registered: Mutex<HashMap<String, u64>>,
    /// Set once retirement begins; no request, frame or native action of this run is honoured afterwards.
    invalid: AtomicBool,
    /// Joined retirement: the first caller does the work under this lock, later callers wait and share the outcome.
    retired: Mutex<bool>,
    native_workers: AtomicUsize,
    #[cfg(windows)]
    job: Mutex<Option<std::os::windows::io::OwnedHandle>>,
}
pub struct BrowserSupervisor {
    root: PathBuf,
    home: PathBuf,
    state: Mutex<State>,
    helper: Mutex<Option<Arc<Helper>>>,
    /// One turn per instance for owner transitions (register / revoke); never held across a browser action.
    transitions: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    #[cfg(any(windows, unix))]
    natives: native::Natives,
    dirty: AtomicBool,
}

/// Failure of a private native action; `code` is the machine-readable reason the helper surfaces to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeError {
    pub code: &'static str,
    pub message: String,
}
impl NativeError {
    #[must_use]
    pub(super) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub(super) fn stale() -> Self {
        Self::new("stale_generation", "stale_generation")
    }
}
impl std::fmt::Display for NativeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Pure admission helpers for the private spawn protocol.
// ---------------------------------------------------------------------------------------------------------------------

/// `(terminal-browsers, <hash>, <hash>/profile)` for one terminal. The hash matches the helper's `profileDirectory`.
pub(crate) fn profile_layout(root: &Path, instance_id: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = root.join("terminal-browsers");
    let hash = format!("{:x}", Sha256::digest(instance_id.as_bytes()));
    let instance = base.join(hash);
    let profile = instance.join("profile");
    (base, instance, profile)
}

fn normalized_path_text(text: &str) -> String {
    #[cfg(windows)]
    {
        text.replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    }
    #[cfg(unix)]
    {
        text.trim_end_matches('/').to_string()
    }
}
pub(crate) fn same_path_text(a: &str, b: &str) -> bool {
    normalized_path_text(a) == normalized_path_text(b)
}

const PLAIN_CHROME_FLAGS: &[&str] = &[
    "allow-pre-commit-input",
    "disable-background-networking",
    "disable-background-timer-throttling",
    "disable-backgrounding-occluded-windows",
    "disable-breakpad",
    "disable-client-side-phishing-detection",
    "disable-component-extensions-with-background-pages",
    "disable-crash-reporter",
    "disable-default-apps",
    "disable-dev-shm-usage",
    "disable-extensions",
    "disable-hang-monitor",
    "disable-infobars",
    "disable-ipc-flooding-protection",
    "disable-popup-blocking",
    "disable-prompt-on-repost",
    "disable-renderer-backgrounding",
    "disable-search-engine-choice-screen",
    "disable-sync",
    "enable-automation",
    "export-tagged-pdf",
    "generate-pdf-document-outline",
    "headless",
    "hide-crash-restore-bubble",
    "hide-scrollbars",
    "metrics-recording-only",
    "mute-audio",
    "no-first-run",
    "use-mock-keychain",
];

/// Accepts exactly the Chromium switches the helper sends (Puppeteer's default set plus this app's launch switches).
/// Startup URLs, extension loading, sandbox removal, proxies, wrapper processes and any debugging endpoint other than
/// numeric loopback with an OS-assigned port are refused; the fixed switches must each be present exactly once.
pub(crate) fn validate_chrome_arguments(
    args: &[String],
    profile: &str,
) -> std::result::Result<(), NativeError> {
    let refused = |message: String| NativeError::new("browser_arguments_refused", message);
    if args.is_empty() || args.len() > 64 {
        return Err(refused("unexpected argument count".into()));
    }
    let (mut user_data, mut address, mut port, mut no_window) = (0, 0, 0, 0);
    for argument in args {
        let Some(flag) = argument.strip_prefix("--") else {
            return Err(refused(format!(
                "`{}` is not a browser switch",
                argument.chars().take(40).collect::<String>()
            )));
        };
        if flag.chars().any(char::is_control) {
            return Err(refused("switch contains control characters".into()));
        }
        let (name, value) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (flag, None),
        };
        let feature_list = |v: &str| {
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ',')
        };
        let allowed = match (name, value) {
            ("user-data-dir", Some(value)) => {
                user_data += 1;
                same_path_text(value, profile)
            }
            ("remote-debugging-address", Some("127.0.0.1")) => {
                address += 1;
                true
            }
            ("remote-debugging-port", Some("0")) => {
                port += 1;
                true
            }
            ("no-startup-window", None) => {
                no_window += 1;
                true
            }
            ("disable-features" | "enable-features", Some(value)) => feature_list(value),
            ("force-color-profile", Some("srgb"))
            | ("password-store", Some("basic"))
            | ("headless", Some("new")) => true,
            ("window-position", Some("-32000,-32000")) => true,
            (name, None) => PLAIN_CHROME_FLAGS.contains(&name),
            _ => false,
        };
        if !allowed {
            return Err(refused(format!(
                "switch --{} is not permitted",
                name.chars().take(60).collect::<String>()
            )));
        }
    }
    if (user_data, address, port, no_window) != (1, 1, 1, 1) {
        return Err(refused(
            "required fixed switches are missing or repeated".into(),
        ));
    }
    Ok(())
}

/// The numeric-loopback DevTools endpoint Chrome prints on stderr. Anything else (other hosts, other schemes, a
/// zero port, a malformed path) is not an owned endpoint.
pub(crate) fn parse_devtools_endpoint(text: &str) -> Option<String> {
    const MARKER: &str = "DevTools listening on ";
    for line in text.lines() {
        let Some(start) = line.find(MARKER) else {
            continue;
        };
        let url = line[start + MARKER.len()..].trim();
        let rest = url.strip_prefix("ws://127.0.0.1:")?;
        let (port, path) = rest.split_once('/')?;
        let id = path.strip_prefix("devtools/browser/")?;
        if port.is_empty()
            || port.len() > 5
            || !port.bytes().all(|b| b.is_ascii_digit())
            || port
                .parse::<u32>()
                .ok()
                .filter(|p| (1..=65535).contains(p))
                .is_none()
        {
            return None;
        }
        if id.is_empty()
            || id.len() > 128
            || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return None;
        }
        return Some(url.to_string());
    }
    None
}

/// Removes endpoint URLs and token-like secrets before text reaches the private log or an error message.
pub(crate) fn redact(text: &str) -> String {
    text.split_inclusive(char::is_whitespace)
        .map(|part| {
            let word = part.trim_end();
            let tail = &part[word.len()..];
            let lower = word.to_ascii_lowercase();
            if ["ws://", "wss://", "http://", "https://"]
                .iter()
                .any(|scheme| lower.contains(scheme))
            {
                format!("[browser endpoint]{tail}")
            } else if word.len() >= 32 && word.bytes().all(|b| b.is_ascii_hexdigit()) {
                format!("[token]{tail}")
            } else {
                part.to_string()
            }
        })
        .collect()
}

/// A browser executable the helper may ask the native side to start: an explicit `PUPPETEER_EXECUTABLE_PATH`, one of
/// the installed Windows locations the SDK probes, or an already present Puppeteer cache inside the helper's private
/// runtime. No binary is ever resolved from renderer input.
pub(crate) fn executable_allowed(
    executable: &Path,
    root: &Path,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> std::result::Result<(), NativeError> {
    let refused = |message: &str| NativeError::new("browser_executable_refused", message);
    if !executable.is_absolute() {
        return Err(refused("browser executable must be an absolute path"));
    }
    let Ok(canonical) = fs::canonicalize(executable) else {
        return Err(NativeError::new(
            "browser_executable_missing",
            "browser executable was not found",
        ));
    };
    if !canonical.is_file() {
        return Err(refused("browser executable is not a file"));
    }
    let directory =
        |name: &str, default: &str| PathBuf::from(env(name).unwrap_or_else(|| default.into()));
    let (program_files, program_files_x86) = (
        directory("ProgramFiles", "C:\\Program Files"),
        directory("ProgramFiles(x86)", "C:\\Program Files (x86)"),
    );
    let local = env("LOCALAPPDATA").map(PathBuf::from);
    let mut allowed: Vec<PathBuf> = env("PUPPETEER_EXECUTABLE_PATH")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    allowed.extend([
        program_files.join("Google\\Chrome\\Application\\chrome.exe"),
        program_files_x86.join("Google\\Chrome\\Application\\chrome.exe"),
        program_files.join("Chromium\\Application\\chrome.exe"),
        program_files.join("Microsoft\\Edge\\Application\\msedge.exe"),
        program_files_x86.join("Microsoft\\Edge\\Application\\msedge.exe"),
    ]);
    if let Some(local) = local {
        allowed.extend([
            local.join("Google\\Chrome\\Application\\chrome.exe"),
            local.join("Chromium\\Application\\chrome.exe"),
        ]);
    }
    #[cfg(unix)]
    {
        for name in [
            "google-chrome-stable",
            "google-chrome",
            "chromium",
            "chromium-browser",
            "chrome",
            "brave-browser",
        ] {
            if let Some(path) = crate::toolchain::find_program(name) {
                allowed.push(path);
            }
        }
        for path in [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            "/opt/google/chrome/chrome",
        ] {
            allowed.push(path.into());
        }
    }
    if allowed
        .iter()
        .any(|candidate| fs::canonicalize(candidate).is_ok_and(|candidate| candidate == canonical))
    {
        return Ok(());
    }
    let cached = fs::canonicalize(root.join("terminal-browsers").join("runtime"))
        .is_ok_and(|runtime| canonical.starts_with(runtime));
    let name = canonical
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if cached
        && matches!(
            name.as_str(),
            "chrome.exe" | "chromium.exe" | "msedge.exe" | "chrome" | "chromium"
        )
    {
        return Ok(());
    }
    Err(refused("browser executable is not an installed Chrome/Chromium/Edge or an existing Puppeteer cache"))
}

fn native_text<'a>(
    payload: &'a Value,
    key: &str,
    max: usize,
) -> std::result::Result<&'a str, NativeError> {
    payload[key]
        .as_str()
        .filter(|text| !text.is_empty() && text.len() <= max && !text.contains('\0'))
        .ok_or_else(|| NativeError::new("invalid_request", format!("invalid {key}")))
}
fn native_identifier<'a>(
    payload: &'a Value,
    key: &str,
) -> std::result::Result<&'a str, NativeError> {
    let text = native_text(payload, key, 128)?;
    text.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        .then_some(text)
        .ok_or_else(|| NativeError::new("invalid_request", format!("invalid {key}")))
}
fn native_generation(payload: &Value) -> std::result::Result<u64, NativeError> {
    payload["generation"]
        .as_u64()
        .ok_or_else(|| NativeError::new("invalid_request", "invalid generation"))
}

// ---------------------------------------------------------------------------------------------------------------------
// Native browser ownership: per-profile OS lease + contained Chrome process tree.
// ---------------------------------------------------------------------------------------------------------------------

impl Helper {
    fn request(&self, action: &str, payload: Value, seconds: u64) -> Result<Value> {
        if self.invalid.load(Ordering::SeqCst) {
            return Err("Browser helper disconnected".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::sync_channel(1);
        let mut bytes = serde_json::to_vec(&json!({"id":id,"action":action,"payload":payload}))
            .map_err(|e| e.to_string())?;
        if bytes.len() >= MAX_FRAME {
            return Err("browser frame exceeds transport limit".into());
        }
        bytes.push(b'\n');
        self.pending.lock().insert(id.clone(), tx);
        if matches!(action, "ensureTab" | "openVisible") {
            let instance = payload["instanceId"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            self.launches.lock().insert(
                id.clone(),
                (instance, payload["generation"].as_u64().unwrap_or(u64::MAX)),
            );
        }
        let forget = |helper: &Self| {
            helper.pending.lock().remove(&id);
            helper.launches.lock().remove(&id);
        };
        if self.invalid.load(Ordering::SeqCst) {
            forget(self);
            return Err("Browser helper disconnected".into());
        }
        let written = self.input.lock().write_all(&bytes);
        if let Err(error) = written {
            forget(self);
            return Err(format!("Write browser request: {error}"));
        }
        let result = rx
            .recv_timeout(Duration::from_secs(seconds))
            .map_err(|_| "Browser service request timed out or disconnected".to_string());
        forget(self);
        result?
    }
    fn admits_launch(&self, request_id: &str, instance: &str, generation: u64) -> bool {
        self.launches
            .lock()
            .get(request_id)
            .is_some_and(|(i, g)| i == instance && *g == generation)
    }
    /// Private reply to a helper's `native_request`; dropped silently once the helper is retired.
    fn respond_native(&self, id: &str, outcome: std::result::Result<Value, NativeError>) {
        if self.invalid.load(Ordering::SeqCst) {
            return;
        }
        let frame = match outcome {
            Ok(result) => {
                json!({"type":"native_response","id":id,"result":result,"error":Value::Null})
            }
            Err(error) => {
                json!({"type":"native_response","id":id,"result":Value::Null,"error":{"code":error.code,"message":error.message}})
            }
        };
        if let Ok(mut bytes) = serde_json::to_vec(&frame) {
            bytes.push(b'\n');
            let _ = self.input.lock().write_all(&bytes);
        }
    }
    /// Stops the helper process and its job; the browser jobs it owned are retired by the supervisor afterwards.
    fn stop_process(&self) -> Result<()> {
        {
            let mut child = self.child.lock();
            let _ = child.kill();
            let _ = child.wait();
        }
        #[cfg(windows)]
        {
            let mut job = self.job.lock();
            if let Some(handle) = job.as_ref() {
                crate::process_tree::terminate_job(
                    handle,
                    "browser helper",
                    crate::process_tree::RETIRE_BOUND,
                )?;
            }
            job.take();
        }
        Ok(())
    }
}
impl BrowserSupervisor {
    #[must_use]
    pub fn new(root: PathBuf, home: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            root,
            home,
            state: Mutex::new(State {
                settings: BrowserSettings::default(),
                owners: HashMap::new(),
                closed: false,
            }),
            helper: Mutex::new(None),
            transitions: Mutex::new(HashMap::new()),
            #[cfg(any(windows, unix))]
            natives: native::Natives::default(),
            dirty: AtomicBool::new(true),
        })
    }
    pub fn materialize(&self) -> Result<()> {
        for (name, source) in [
            (
                "omp-browser-service.mjs",
                include_str!("../../../bridge/omp-browser-service.mjs"),
            ),
            (
                "omp-browser.mjs",
                include_str!("../../../bridge/omp-browser.mjs"),
            ),
        ] {
            let file = self.root.join(name);
            if fs::read_to_string(&file).ok().as_deref() != Some(source) {
                fs::write(file, source).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
    pub fn configure(&self, settings: BrowserSettings) -> Result<()> {
        #[cfg(unix)]
        let settings = BrowserSettings {
            embedded: false,
            ..settings
        };
        let helper = self.helper.lock().clone();
        if let Some(helper) = helper {
            helper.request(
                "configure",
                serde_json::to_value(settings).map_err(|e| e.to_string())?,
                30,
            )?;
        }
        let mut state = self.state.lock();
        if state.closed {
            return Err("Browser service closed".into());
        }
        state.settings = settings;
        // Owners the helper has not launched report the new default until its state frames take over.
        for owner in state.owners.values_mut() {
            if owner.info.pid.is_none() {
                owner.info.embedded = settings.embedded;
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }
    #[must_use]
    pub fn settings(&self) -> BrowserSettings {
        self.state.lock().settings
    }
    pub fn report_embedding_error(&self, id: &str, generation: u64, message: &str) {
        let mut state = self.state.lock();
        if let Some(owner) = state.owners.get_mut(id) {
            if owner.generation == generation && owner.info.error.as_deref() != Some(message) {
                owner.info.error = Some(message.into());
                owner.info.state = "error".into();
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
    }
    /// Live Chrome roots this supervisor owns, each verified against its recorded creation time. Takes short locks
    /// only and never talks to the helper, so a monitor can call it on every snapshot.
    #[must_use]
    pub fn owned_roots(&self) -> Vec<BrowserRoot> {
        #[cfg(any(windows, unix))]
        {
            self.natives.roots()
        }
        #[cfg(not(any(windows, unix)))]
        {
            Vec::new()
        }
    }
    pub fn register(&self, id: &str, generation: u64, token: &str) -> Result<()> {
        let mut state = self.state.lock();
        if state.closed {
            return Err("Browser service closed".into());
        }
        let (headless, embedded) = (state.settings.headless, state.settings.embedded);
        state.owners.insert(
            id.into(),
            Owner {
                generation,
                token: token.into(),
                info: BrowserInfo {
                    instance_id: id.into(),
                    generation,
                    epoch: 0,
                    state: "closed".into(),
                    headless,
                    embedded,
                    pid: None,
                    error: None,
                },
                session: None,
            },
        );
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }
    fn authenticate(&self, id: &str, generation: u64) -> Result<()> {
        let state = self.state.lock();
        if !state.closed
            && state
                .owners
                .get(id)
                .is_some_and(|o| o.generation == generation && !o.token.is_empty())
        {
            Ok(())
        } else {
            Err("stale_generation".into())
        }
    }
    fn transition_gate(&self, id: &str) -> Arc<Mutex<()>> {
        self.transitions
            .lock()
            .entry(id.into())
            .or_default()
            .clone()
    }
    fn native_log(&self, line: &str) {
        let directory = self.root.join("terminal-browsers");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("browser-native.log"))
        {
            let _ = writeln!(file, "{stamp} {}", redact(line));
        }
    }
    /// One joined retirement shared by EOF, shutdown and replacement: invalidate the run, stop its process and job,
    /// then retire every browser job it owned. Success is cached; failure is retried by the next caller and keeps
    /// everything still owned.
    fn retire_helper(&self, helper: &Arc<Helper>) -> Result<()> {
        let mut retired = helper.retired.lock();
        if *retired {
            return Ok(());
        }
        helper.invalid.store(true, Ordering::SeqCst);
        for (_, tx) in helper.pending.lock().drain() {
            let _ = tx.send(Err("Browser helper disconnected".into()));
        }
        helper.launches.lock().clear();
        helper.stop_process()?;
        #[cfg(any(windows, unix))]
        self.natives
            .retire_session(&helper.session)
            .map_err(|e| e.to_string())?;
        *retired = true;
        Ok(())
    }
    fn connect(self: &Arc<Self>) -> Result<Arc<Helper>> {
        let mut slot = self.helper.lock();
        if let Some(helper) = slot.clone() {
            let alive = !helper.invalid.load(Ordering::SeqCst)
                && helper
                    .child
                    .lock()
                    .try_wait()
                    .map_err(|e| e.to_string())?
                    .is_none();
            if alive {
                return Ok(helper);
            }
            // Never start a replacement before the old run's helper job and browser jobs are verified empty.
            self.retire_helper(&helper)
                .map_err(|e| format!("Previous browser service cleanup is blocked: {e}"))?;
            *slot = None;
        }
        if self.state.lock().closed {
            return Err("Browser service closed".into());
        }
        let sdk = crate::config::sdk_dir(&self.home)?;
        let session = uuid::Uuid::new_v4().simple().to_string();
        let mut cmd = Command::new(crate::toolchain::bun_program());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        let directory = self.root.join("terminal-browsers");
        // Per-run post-mortem logs; diagnostics must never stop the browser from starting.
        let log = fs::create_dir_all(&directory)
            .and_then(|_| fs::File::create(directory.join("browser-native.log")))
            .and_then(|_| fs::File::create(directory.join("browser-service.log")))
            .map_or_else(|_| Stdio::null(), Stdio::from);
        let mut child = cmd
            .arg(self.root.join("omp-browser-service.mjs"))
            .arg(&self.root)
            .arg(sdk)
            .arg(&session)
            .env_remove("PI_COMPILED")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .map_err(|e| format!("Browser requires Bun and the installed OMP SDK: {e}"))?;
        #[cfg(windows)]
        let job = match crate::process_tree::contain(child.id(), "browser helper") {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let input = child
            .stdin
            .take()
            .ok_or("Browser helper stdin unavailable")?;
        let output = child
            .stdout
            .take()
            .ok_or("Browser helper stdout unavailable")?;
        let helper = Arc::new(Helper {
            session,
            child: Mutex::new(child),
            input: Mutex::new(input),
            pending: Mutex::new(HashMap::new()),
            launches: Mutex::new(HashMap::new()),
            registered: Mutex::new(HashMap::new()),
            invalid: AtomicBool::new(false),
            retired: Mutex::new(false),
            native_workers: AtomicUsize::new(0),
            #[cfg(windows)]
            job: Mutex::new(Some(job)),
        });
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let reader_helper = helper.clone();
        let supervisor = Arc::downgrade(self);
        std::thread::spawn(move || Self::read_helper(output, reader_helper, supervisor, ready_tx));
        if ready_rx.recv_timeout(Duration::from_secs(30)).is_err() {
            let _ = self.retire_helper(&helper);
            return Err("Browser helper failed to start within 30 seconds".into());
        }
        let settings = self.state.lock().settings;
        if let Err(error) = helper.request(
            "configure",
            serde_json::to_value(settings).map_err(|e| e.to_string())?,
            30,
        ) {
            let _ = self.retire_helper(&helper);
            return Err(error);
        }
        *slot = Some(helper.clone());
        Ok(helper)
    }
    fn read_helper(
        output: std::process::ChildStdout,
        helper: Arc<Helper>,
        supervisor: Weak<Self>,
        ready: mpsc::SyncSender<()>,
    ) {
        let mut reader = BufReader::new(output);
        let mut observed = HashMap::<String, BrowserInfo>::new();
        while let Some(frame) = Self::read_frame(&mut reader) {
            if !Self::dispatch_frame(&frame, &helper, &supervisor, &ready, &mut observed) {
                break;
            }
        }
        Self::finish_helper(&helper, &supervisor, observed);
    }
    /// Next newline-terminated JSON frame; `None` ends the reader (I/O error, EOF, oversize, truncated or invalid frame).
    fn read_frame(reader: &mut BufReader<std::process::ChildStdout>) -> Option<Value> {
        let mut line = Vec::new();
        let count = Read::by_ref(reader)
            .take((MAX_FRAME + 1) as u64)
            .read_until(b'\n', &mut line)
            .ok()?;
        if count == 0 || count > MAX_FRAME || line.last() != Some(&b'\n') {
            return None;
        }
        serde_json::from_slice::<Value>(&line).ok()
    }
    /// Handles one frame; `false` stops the reader loop.
    fn dispatch_frame(
        frame: &Value,
        helper: &Arc<Helper>,
        supervisor: &Weak<Self>,
        ready: &mpsc::SyncSender<()>,
        observed: &mut HashMap<String, BrowserInfo>,
    ) -> bool {
        match frame["type"].as_str() {
            Some("ready") => Self::accept_ready(frame, helper, ready),
            Some("response") => {
                Self::deliver_response(frame, helper);
                true
            }
            Some("native_request") => Self::start_native_request(frame, helper, supervisor),
            Some("state") => {
                Self::record_state(frame, helper, supervisor, observed);
                true
            }
            _ => false,
        }
    }
    fn accept_ready(frame: &Value, helper: &Helper, ready: &mpsc::SyncSender<()>) -> bool {
        // A helper that does not echo this run's session is not the process this reader was started for.
        if frame["helperSession"].as_str() != Some(helper.session.as_str()) {
            return false;
        }
        let _ = ready.try_send(());
        true
    }
    fn deliver_response(frame: &Value, helper: &Helper) {
        let Some(id) = frame["id"].as_str() else {
            return;
        };
        let Some(tx) = helper.pending.lock().remove(id) else {
            return;
        };
        let result = if frame["ok"] == true {
            Ok(frame["data"].clone())
        } else {
            Err(frame["error"]
                .as_str()
                .unwrap_or("Browser helper failed")
                .into())
        };
        let _ = tx.send(result);
    }
    /// `false` (stop the reader) only for a missing or oversized request id.
    fn start_native_request(frame: &Value, helper: &Arc<Helper>, supervisor: &Weak<Self>) -> bool {
        let Some(id) = frame["id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 128)
            .map(str::to_string)
        else {
            return false;
        };
        let action = frame["action"].as_str().unwrap_or_default().to_string();
        let payload = frame["payload"].clone();
        if let Some(error) = Self::native_refusal(helper, &payload) {
            helper.respond_native(&id, Err(error));
            return true;
        }
        Self::spawn_native_worker(helper, supervisor, id, action, payload);
        true
    }
    /// Refuses stale or over-limit requests; an accepted request leaves `native_workers` incremented.
    fn native_refusal(helper: &Helper, payload: &Value) -> Option<NativeError> {
        if helper.invalid.load(Ordering::SeqCst)
            || payload["helperSession"].as_str() != Some(helper.session.as_str())
        {
            Some(NativeError::new(
                "stale_helper_session",
                "stale_helper_session",
            ))
        } else if helper.native_workers.fetch_add(1, Ordering::SeqCst) >= MAX_NATIVE_WORKERS {
            helper.native_workers.fetch_sub(1, Ordering::SeqCst);
            Some(NativeError::new(
                "native_busy",
                "too many concurrent native browser actions",
            ))
        } else {
            None
        }
    }
    // A worker per request: spawning waits for Chrome's endpoint while retire/ready requests must
    // keep flowing through the reader.
    fn spawn_native_worker(
        helper: &Arc<Helper>,
        supervisor: &Weak<Self>,
        id: String,
        action: String,
        payload: Value,
    ) {
        let (worker_helper, worker_supervisor) = (helper.clone(), supervisor.clone());
        std::thread::spawn(move || {
            let outcome = match worker_supervisor.upgrade() {
                Some(supervisor) => supervisor.native_action(&worker_helper, &action, &payload),
                None => Err(NativeError::new(
                    "browser_service_closed",
                    "Browser service closed",
                )),
            };
            worker_helper.native_workers.fetch_sub(1, Ordering::SeqCst);
            worker_helper.respond_native(&id, outcome);
        });
    }
    fn record_state(
        frame: &Value,
        helper: &Helper,
        supervisor: &Weak<Self>,
        observed: &mut HashMap<String, BrowserInfo>,
    ) {
        if helper.invalid.load(Ordering::SeqCst) {
            return;
        }
        if let (Some(supervisor), Ok(info)) = (
            supervisor.upgrade(),
            serde_json::from_value::<BrowserInfo>(frame["data"].clone()),
        ) {
            observed.insert(info.instance_id.clone(), info.clone());
            supervisor.apply_state(helper, info);
        }
    }
    fn apply_state(&self, helper: &Helper, info: BrowserInfo) {
        let mut state = self.state.lock();
        if let Some(owner) = state.owners.get_mut(&info.instance_id) {
            if owner.generation == info.generation
                && info.epoch >= owner.info.epoch
                && owner.info != info
            {
                owner.info = info;
                owner.session = Some(helper.session.clone());
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
    }
    fn finish_helper(
        helper: &Arc<Helper>,
        supervisor: &Weak<Self>,
        observed: HashMap<String, BrowserInfo>,
    ) {
        match supervisor.upgrade() {
            Some(supervisor) => {
                // Retirement runs before the state lock is taken.
                let outcome = supervisor.retire_helper(helper);
                supervisor.mark_disconnected(helper, observed, &outcome);
                if let Err(error) = outcome {
                    supervisor.native_log(&format!("helper retirement blocked: {error}"));
                }
            }
            None => {
                helper.invalid.store(true, Ordering::SeqCst);
                let _ = helper.stop_process();
            }
        }
    }
    fn mark_disconnected(
        &self,
        helper: &Helper,
        observed: HashMap<String, BrowserInfo>,
        outcome: &Result<()>,
    ) {
        let mut state = self.state.lock();
        for (id, last) in observed {
            let Some(owner) = state.owners.get_mut(&id) else {
                continue;
            };
            // Only the run that produced the current info may mark it disconnected.
            if owner.session.as_deref() == Some(helper.session.as_str())
                && owner.info.generation == last.generation
                && owner.info.epoch == last.epoch
                && owner.info.pid.is_some()
            {
                owner.info.pid = None;
                owner.info.state = "error".into();
                owner.info.error = Some(match outcome {
                    Ok(()) => "Browser helper disconnected".into(),
                    Err(error) => {
                        format!("Browser helper disconnected; cleanup is blocked: {error}")
                    }
                });
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
    }
    /// Private helper→native actions: `browser_spawn`, `browser_ready`, `browser_retire`.
    fn native_action(
        self: &Arc<Self>,
        helper: &Arc<Helper>,
        action: &str,
        payload: &Value,
    ) -> std::result::Result<Value, NativeError> {
        #[cfg(not(any(windows, unix)))]
        {
            let _ = (helper, action, payload);
            Err(NativeError::new(
                "unsupported_platform",
                "Owned browsers require Windows",
            ))
        }
        #[cfg(any(windows, unix))]
        {
            let result = match action {
                "browser_spawn" => self.browser_spawn(helper, payload),
                "browser_ready" => self.browser_ready(helper, payload),
                "browser_retire" => self.browser_retire(helper, payload),
                _ => Err(NativeError::new(
                    "unknown_action",
                    "unknown native browser action",
                )),
            };
            if let Err(error) = &result {
                self.native_log(&format!(
                    "{action} failed code={} message={}",
                    error.code, error.message
                ));
            }
            result
        }
    }
    #[cfg(any(windows, unix))]
    fn browser_spawn(
        self: &Arc<Self>,
        helper: &Arc<Helper>,
        p: &Value,
    ) -> std::result::Result<Value, NativeError> {
        let operation = native_identifier(p, "operationId")?;
        let source = native_text(p, "sourceRequestId", 128)?;
        let instance = native_text(p, "instanceId", 1024)?;
        let generation = native_generation(p)?;
        if !helper.admits_launch(source, instance, generation) {
            return Err(NativeError::new(
                "browser_spawn_refused",
                "no pending launch request admits this browser start",
            ));
        }
        self.authenticate(instance, generation)
            .map_err(|_| NativeError::stale())?;
        if helper.registered.lock().get(instance) != Some(&generation) {
            return Err(NativeError::stale());
        }
        let profile = native_text(p, "profilePath", 32768)?;
        let (_, _, expected) = profile_layout(&self.root, instance);
        if !same_path_text(profile, &expected.to_string_lossy()) {
            return Err(NativeError::new(
                "browser_arguments_refused",
                "profile path is not this terminal's profile",
            ));
        }
        let args = p["args"]
            .as_array()
            .ok_or_else(|| NativeError::new("invalid_request", "invalid args"))?
            .iter()
            .map(|arg| {
                arg.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| NativeError::new("invalid_request", "invalid args"))
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        validate_chrome_arguments(&args, profile)?;
        let executable = PathBuf::from(native_text(p, "executable", 32768)?);
        executable_allowed(&executable, &self.root, &|name| std::env::var_os(name))?;
        let slot = self
            .natives
            .begin(instance, generation, operation, &helper.session)?;
        let log = |line: &str| self.native_log(line);
        native::run_spawn(
            &slot,
            &self.natives,
            native::SpawnRequest {
                root: &self.root,
                instance_id: instance,
                executable,
                args,
                log: &log,
            },
        )
    }
    #[cfg(any(windows, unix))]
    fn browser_ready(
        self: &Arc<Self>,
        helper: &Arc<Helper>,
        p: &Value,
    ) -> std::result::Result<Value, NativeError> {
        let (operation, lease) = (
            native_identifier(p, "operationId")?,
            native_text(p, "leaseId", 128)?,
        );
        let (instance, generation) = (native_text(p, "instanceId", 1024)?, native_generation(p)?);
        self.authenticate(instance, generation)
            .map_err(|_| NativeError::stale())?;
        let slot = self
            .natives
            .find(instance, operation)
            .filter(|slot| {
                slot.lease_id == lease
                    && slot.generation == generation
                    && slot.helper_session == helper.session
            })
            .ok_or_else(NativeError::stale)?;
        let alive = slot
            .child()
            .is_some_and(|child| child.try_wait().is_ok_and(|code| code.is_none()));
        if !alive {
            return Err(NativeError::new(
                "browser_exited",
                "Chrome exited before initialization finished",
            ));
        }
        if !slot.mark_ready() {
            return Err(NativeError::stale());
        }
        Ok(json!({"ready": true}))
    }
    #[cfg(any(windows, unix))]
    fn browser_retire(
        self: &Arc<Self>,
        helper: &Arc<Helper>,
        p: &Value,
    ) -> std::result::Result<Value, NativeError> {
        let operation = native_identifier(p, "operationId")?;
        let instance = native_text(p, "instanceId", 1024)?;
        native_generation(p)?;
        let grace = Duration::from_millis(p["graceMs"].as_u64().unwrap_or(0).min(10_000));
        if !p["leaseId"].is_null() && native_text(p, "leaseId", 128).is_err() {
            return Err(NativeError::new("invalid_request", "invalid leaseId"));
        }
        // Cancelling by operation id also stops a spawn that has not produced a lease yet.
        let Some(slot) = self.natives.cancel_operation(instance, operation) else {
            return Ok(json!({"retired": true}));
        };
        if slot.helper_session != helper.session {
            return Err(NativeError::new(
                "stale_helper_session",
                "stale_helper_session",
            ));
        }
        if let Some(lease) = p["leaseId"].as_str() {
            if lease != slot.lease_id {
                return Err(NativeError::new(
                    "lease_mismatch",
                    "lease does not belong to this operation",
                ));
            }
        }
        slot.retire(&self.natives, grace)?;
        Ok(json!({"retired": true}))
    }
    fn request(
        self: &Arc<Self>,
        id: &str,
        generation: u64,
        action: &str,
        mut payload: Value,
    ) -> Result<Value> {
        self.authenticate(id, generation)?;
        let helper = self.connect()?;
        {
            // Registration is one turn of this instance's transition: it cannot interleave with a revoke.
            let gate = self.transition_gate(id);
            let _turn = gate.lock();
            self.authenticate(id, generation)?;
            let known = helper.registered.lock().get(id) == Some(&generation);
            if !known {
                helper.request(
                    "register",
                    json!({"instanceId":id,"generation":generation}),
                    30,
                )?;
                helper.registered.lock().insert(id.into(), generation);
            }
        }
        self.authenticate(id, generation)?;
        payload["instanceId"] = json!(id);
        payload["generation"] = json!(generation);
        let result = helper.request(action, payload, 30);
        self.authenticate(id, generation)?;
        result
    }
    pub fn prepare_tab(
        self: &Arc<Self>,
        id: &str,
        generation: u64,
        name: &str,
        target: Option<&str>,
    ) -> Result<Value> {
        self.authenticate(id, generation)?;
        {
            let mut state = self.state.lock();
            if let Some(owner) = state.owners.get_mut(id) {
                if owner.generation == generation
                    && owner.info.pid.is_none()
                    && owner.info.error.is_some()
                {
                    owner.info.error = None;
                    owner.info.state = "closed".into();
                    self.dirty.store(true, Ordering::Relaxed);
                }
            }
        }
        let mut payload = json!({"name":name});
        if let Some(target) = target {
            payload["target"] = json!(target);
        }
        let result = self.request(id, generation, "ensureTab", payload);
        // A target that matches no page is the caller's mistake, not a failed browser.
        if let (Err(error), None) = (&result, target) {
            let mut state = self.state.lock();
            if let Some(owner) = state.owners.get_mut(id) {
                if owner.generation == generation
                    && (owner.info.state != "error" || owner.info.error.as_ref() != Some(error))
                {
                    owner.info.state = "error".into();
                    owner.info.error = Some(error.clone());
                    self.dirty.store(true, Ordering::Relaxed);
                }
            }
        }
        result
    }
    /// Docks (or reuses) this terminal's own browser; a headless one is only restarted with consent.
    pub fn open_visible(
        self: &Arc<Self>,
        id: &str,
        generation: u64,
        restart_headless: bool,
    ) -> Result<BrowserInfo> {
        let value = self.request(
            id,
            generation,
            "openVisible",
            json!({"restartHeadless":restart_headless}),
        )?;
        let info: BrowserInfo = serde_json::from_value(value).map_err(|e| e.to_string())?;
        let mut state = self.state.lock();
        if let Some(owner) = state.owners.get_mut(id) {
            if owner.generation == generation
                && info.generation == generation
                && info.epoch >= owner.info.epoch
                && owner.info != info
            {
                owner.info = info.clone();
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
        Ok(info)
    }
    /// Tabs of this terminal's running browser (hand-opened ones are bound as tab-N); never starts anything.
    pub fn list_tabs(self: &Arc<Self>, id: &str, generation: u64) -> Result<Value> {
        self.authenticate(id, generation)?;
        if self.helper.lock().is_none() {
            return Ok(json!([]));
        }
        self.request(id, generation, "listTabs", json!({}))
    }
    pub fn close_tab(
        self: &Arc<Self>,
        id: &str,
        generation: u64,
        name: Option<&str>,
    ) -> Result<()> {
        self.request(id, generation, "closeTab", json!({"name":name}))
            .map(|_| ())
    }
    /// Removes the owner, then retires its browser through the helper and — independently of the helper answering —
    /// natively, so a hung or dead helper cannot leave a Chrome tree or the profile lease behind.
    pub fn revoke(&self, id: &str, generation: u64) -> Result<()> {
        {
            let mut state = self.state.lock();
            if !state
                .owners
                .get(id)
                .is_some_and(|o| o.generation == generation)
            {
                return Ok(());
            }
            state.owners.remove(id);
            self.dirty.store(true, Ordering::Relaxed);
        }
        let gate = self.transition_gate(id);
        let _turn = gate.lock();
        let helper = self.helper.lock().clone();
        if let Some(helper) = helper {
            let registered = helper.registered.lock().remove(id);
            if registered == Some(generation) {
                if let Err(error) = helper.request(
                    "revoke",
                    json!({"instanceId":id,"generation":generation}),
                    30,
                ) {
                    self.native_log(&format!("helper revoke failed: {error}"));
                }
            }
        }
        #[cfg(any(windows, unix))]
        self.natives
            .retire_instance(id, Some(generation))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    #[must_use]
    pub fn snapshot(&self) -> Vec<BrowserInfo> {
        let mut rows = self
            .state
            .lock()
            .owners
            .values()
            .map(|o| o.info.clone())
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        rows
    }
    pub fn take_update(&self) -> Option<Vec<BrowserInfo>> {
        if self.dirty.swap(false, Ordering::Relaxed) {
            Some(self.snapshot())
        } else {
            None
        }
    }
    pub fn shutdown(&self) {
        self.state.lock().closed = true;
        if let Some(helper) = self.helper.lock().take() {
            let _ = helper.request("shutdown", json!({}), 20);
            if let Err(error) = self.retire_helper(&helper) {
                self.native_log(&format!("shutdown helper retirement blocked: {error}"));
            }
        }
        #[cfg(any(windows, unix))]
        if let Err(error) = self.natives.retire_all() {
            self.native_log(&format!("shutdown browser retirement blocked: {error}"));
        }
        self.state.lock().owners.clear();
    }
}

#[cfg(all(test, any(windows, unix)))]
impl BrowserSupervisor {
    /// Kills only the helper process (no retirement), as a crash would; its EOF reader must retire the browsers.
    fn kill_helper_process_for_test(&self) {
        let helper = self.helper.lock().clone().expect("a helper is running");
        let _ = helper.child.lock().kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(profile: &str) -> Vec<String> {
        [
            "--disable-background-networking",
            "--disable-features=Translate,AcceptCHFrame",
            "--enable-features=PdfOopif",
            "--force-color-profile=srgb",
            "--no-first-run",
            &format!("--user-data-dir={profile}"),
            "--remote-debugging-address=127.0.0.1",
            "--remote-debugging-port=0",
            "--no-startup-window",
            "--hide-crash-restore-bubble",
            "--window-position=-32000,-32000",
        ]
        .map(str::to_string)
        .to_vec()
    }

    #[test]
    fn chrome_arguments_accept_only_the_fixed_launch_shape() {
        let profile = r"C:\app\terminal-browsers\abc\profile";
        assert_eq!(
            validate_chrome_arguments(&arguments(profile), profile),
            Ok(())
        );
        let mutate = |edit: &dyn Fn(&mut Vec<String>)| {
            let mut args = arguments(profile);
            edit(&mut args);
            validate_chrome_arguments(&args, profile).unwrap_err().code
        };
        assert_eq!(
            mutate(&|a| a.push("about:blank".into())),
            "browser_arguments_refused"
        );
        assert_eq!(
            mutate(&|a| a.push("https://example.test/".into())),
            "browser_arguments_refused"
        );
        assert_eq!(
            mutate(&|a| a.push("--no-sandbox".into())),
            "browser_arguments_refused"
        );
        assert_eq!(
            mutate(&|a| a.push("--load-extension=C:\\evil".into())),
            "browser_arguments_refused"
        );
        assert_eq!(
            mutate(&|a| a.push("--remote-debugging-pipe".into())),
            "browser_arguments_refused"
        );
        assert_eq!(
            mutate(&|a| a.push("--renderer-cmd-prefix=calc".into())),
            "browser_arguments_refused"
        );
        assert_eq!(
            mutate(&|a| a.push("--disable-features=A B".into())),
            "browser_arguments_refused"
        );
        assert_eq!(
            mutate(&|a| a.retain(|x| x != "--remote-debugging-port=0")),
            "browser_arguments_refused"
        );
        assert_eq!(
            mutate(&|a| a.push("--remote-debugging-port=0".into())),
            "browser_arguments_refused"
        );
        assert_eq!(
            mutate(&|a| a.retain(|x| x != "--remote-debugging-address=127.0.0.1")),
            "browser_arguments_refused"
        );
        assert_eq!(
            mutate(&|a| a[6] = "--remote-debugging-address=0.0.0.0".into()),
            "browser_arguments_refused"
        );
        assert_eq!(
            mutate(&|a| a[5] =
                r"--user-data-dir=C:\Users\someone\AppData\Local\Google\Chrome\User Data".into()),
            "browser_arguments_refused"
        );
        assert_eq!(
            mutate(&|a| a.retain(|x| x != "--no-startup-window")),
            "browser_arguments_refused"
        );
    }

    #[test]
    fn profile_paths_match_the_helper_layout_and_compare_loosely() {
        let (_, instance, profile) = profile_layout(Path::new(r"C:\cfg"), "terminal-a");
        assert_eq!(profile, instance.join("profile"));
        assert_ne!(
            profile_layout(Path::new(r"C:\cfg"), "terminal-b").2,
            profile
        );
        assert_ne!(
            profile_layout(Path::new(r"C:\cfg"), "../terminal-a").2,
            profile
        );
        assert_eq!(
            same_path_text(
                &profile.to_string_lossy().replace('\\', "/"),
                &profile.to_string_lossy().to_uppercase()
            ),
            cfg!(windows)
        );
        assert!(!same_path_text(
            r"C:\cfg\terminal-browsers\x\profile",
            &profile.to_string_lossy()
        ));
    }

    #[test]
    fn only_a_numeric_loopback_devtools_line_is_an_endpoint() {
        let good = "[1:2:ERROR] noise\nDevTools listening on ws://127.0.0.1:51734/devtools/browser/0a1b2c3d-4e5f-6789-abcd-ef0123456789\n";
        assert_eq!(
            parse_devtools_endpoint(good).as_deref(),
            Some("ws://127.0.0.1:51734/devtools/browser/0a1b2c3d-4e5f-6789-abcd-ef0123456789")
        );
        for bad in [
            "DevTools listening on ws://localhost:51734/devtools/browser/abc",
            "DevTools listening on ws://10.0.0.5:51734/devtools/browser/abc",
            "DevTools listening on ws://127.0.0.1:0/devtools/browser/abc",
            "DevTools listening on ws://127.0.0.1:99999/devtools/browser/abc",
            "DevTools listening on ws://127.0.0.1:123/other/abc",
            "DevTools listening on ws://127.0.0.1:123/devtools/browser/a;b",
            "DevTools listening on wss://127.0.0.1:123/devtools/browser/abc",
            "nothing here",
        ] {
            assert_eq!(parse_devtools_endpoint(bad), None, "{bad}");
        }
    }

    #[test]
    fn private_logs_drop_endpoints_and_tokens() {
        let text = "DevTools listening on ws://127.0.0.1:9/devtools/browser/x token 0123456789abcdef0123456789abcdef0123 ok";
        let clean = redact(text);
        assert!(
            !clean.contains("127.0.0.1") && !clean.contains("0123456789abcdef"),
            "{clean}"
        );
        assert!(
            clean.contains("[browser endpoint]")
                && clean.contains("[token]")
                && clean.ends_with("ok")
        );
    }

    #[test]
    fn executables_come_only_from_installed_locations() {
        let root = std::env::temp_dir().join(format!("omp-browser-exe-{}", uuid::Uuid::new_v4()));
        let runtime = root
            .join("terminal-browsers")
            .join("runtime")
            .join(".omp")
            .join("puppeteer")
            .join("chrome-win64");
        fs::create_dir_all(&runtime).unwrap();
        let cached = runtime.join("chrome.exe");
        let explicit = root.join("explicit-browser.exe");
        let stranger = root.join("stranger.exe");
        for file in [&cached, &explicit, &stranger] {
            fs::write(file, b"MZ").unwrap();
        }
        let env = |explicit: Option<&Path>| {
            let explicit = explicit.map(|p| p.as_os_str().to_owned());
            move |name: &str| match name {
                "PUPPETEER_EXECUTABLE_PATH" => explicit.clone(),
                "ProgramFiles" | "ProgramFiles(x86)" | "LOCALAPPDATA" => {
                    Some(std::ffi::OsString::from("Z:\\absent"))
                }
                _ => None,
            }
        };
        assert_eq!(
            executable_allowed(&cached, &root, &env(None)),
            Ok(()),
            "existing Puppeteer cache in the helper runtime"
        );
        assert_eq!(
            executable_allowed(&explicit, &root, &env(Some(&explicit))),
            Ok(()),
            "explicit absolute PUPPETEER_EXECUTABLE_PATH"
        );
        assert_eq!(
            executable_allowed(&stranger, &root, &env(None))
                .unwrap_err()
                .code,
            "browser_executable_refused"
        );
        assert_eq!(
            executable_allowed(&stranger, &root, &env(Some(&explicit)))
                .unwrap_err()
                .code,
            "browser_executable_refused"
        );
        assert_eq!(
            executable_allowed(&root.join("absent.exe"), &root, &env(None))
                .unwrap_err()
                .code,
            "browser_executable_missing"
        );
        assert_eq!(
            executable_allowed(Path::new("chrome.exe"), &root, &env(None))
                .unwrap_err()
                .code,
            "browser_executable_refused"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(any(windows, unix))]
    mod native_tests {
        use super::*;
        use crate::process_tree::{self, ContainedChild, ContainedSpec, StdioMode};
        use std::ffi::OsString;

        fn scratch() -> PathBuf {
            let root =
                std::env::temp_dir().join(format!("omp-browser-native-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            root
        }

        #[test]
        fn profile_lease_is_exclusive_until_released() {
            let root = scratch();
            let path = root.join("owner.lock");
            let first = native::acquire_lease(&path).unwrap();
            assert_eq!(
                native::acquire_lease(&path).unwrap_err().code,
                "browser_profile_busy"
            );
            // The file's existence is not authority: an existing lock file is acquirable once the holder is gone.
            drop(first);
            assert!(path.exists());
            drop(native::acquire_lease(&path).unwrap());
            let _ = fs::remove_dir_all(root);
        }

        #[cfg(unix)]
        #[test]
        fn releasing_a_profile_lease_does_not_wait_for_an_unrelated_fork_to_exec() {
            use std::os::fd::AsRawFd;
            use std::os::unix::net::UnixStream;
            let root = scratch();
            let path = root.join("owner.lock");
            let lease = native::acquire_lease(&path).unwrap();
            let (reader, writer) = UnixStream::pair().unwrap();
            // SAFETY: the forked child calls only async-signal-safe syscalls, never Rust allocation or locks.
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                // SAFETY: this is the fork child; live descriptors and a one-byte stack buffer are used only by
                // async-signal-safe syscalls, and _exit prevents Rust destructors from running.
                unsafe {
                    libc::close(writer.as_raw_fd());
                    let mut byte = 0u8;
                    libc::read(reader.as_raw_fd(), (&mut byte as *mut u8).cast(), 1);
                    libc::_exit(0);
                }
            }
            assert!(pid > 0);
            drop(reader);
            drop(lease);
            let replacement = native::acquire_lease(&path);
            drop(writer);
            // SAFETY: pid is our successfully forked child; null discards its exit status while waitpid reaps it.
            unsafe {
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
            assert!(
                replacement.is_ok(),
                "the unrelated child must not extend the retired profile lease"
            );
            drop(replacement);
            fs::remove_dir_all(root).unwrap();
        }

        fn shell_command(seconds: u32) -> (PathBuf, Vec<OsString>) {
            #[cfg(windows)]
            return (
                PathBuf::from(std::env::var_os("COMSPEC").unwrap()),
                vec![
                    "/s".into(),
                    "/c".into(),
                    format!("ping -n {seconds} 127.0.0.1 >nul").into(),
                ],
            );
            #[cfg(unix)]
            (
                PathBuf::from("/bin/sh"),
                vec!["-c".into(), format!("sleep {seconds}").into()],
            )
        }

        fn scratch_child() -> Arc<ContainedChild> {
            let (program, args) = shell_command(60);
            Arc::new(
                ContainedChild::spawn(ContainedSpec {
                    subject: "browser slot probe".into(),
                    program,
                    args,
                    cwd: None,
                    env_set: vec![],
                    env_remove: vec![],
                    stdout: StdioMode::Null,
                    stderr: StdioMode::Null,
                    capture_limit: 1024,
                })
                .unwrap(),
            )
        }

        #[test]
        fn retiring_a_slot_kills_its_tree_then_releases_lease_and_slot() {
            let root = scratch();
            let natives = native::Natives::default();
            let lock = root.join("owner.lock");
            let slot = natives.begin("a", 1, "op-1", "session").unwrap();
            assert_eq!(
                natives.begin("a", 2, "op-2", "session").err().unwrap().code,
                "browser_profile_busy",
                "one browser slot per terminal"
            );
            let child = scratch_child();
            let identity = (child.pid(), child.creation_time_100ns());
            slot.test_install(child, native::acquire_lease(&lock).unwrap());
            assert!(slot.mark_ready());
            assert_eq!(natives.roots().len(), 1);
            assert_eq!(
                (
                    natives.roots()[0].pid,
                    natives.roots()[0].creation_time_100ns
                ),
                identity
            );
            assert_eq!(
                native::acquire_lease(&lock).unwrap_err().code,
                "browser_profile_busy"
            );
            // Cancelling by operation id tombstones it and hands back the slot created for it.
            let found = natives.cancel_operation("a", "op-1").unwrap();
            found.retire(&natives, Duration::ZERO).unwrap();
            assert!(natives.is_empty() && natives.roots().is_empty());
            assert_eq!(
                process_tree::process_creation_time(identity.0),
                None,
                "the whole tree is gone before the lease is released"
            );
            drop(native::acquire_lease(&lock).unwrap());
            assert_eq!(
                natives.begin("a", 1, "op-1", "session").err().unwrap().code,
                "operation_cancelled",
                "a late spawn of a retired operation starts nothing"
            );
            natives.begin("a", 1, "op-3", "session").unwrap();
            natives.retire_all().unwrap();
            assert!(natives.is_empty());
            let _ = fs::remove_dir_all(root);
        }

        fn processes_alive(identity: (u32, u64)) -> bool {
            process_tree::process_creation_time(identity.0) == Some(identity.1)
        }

        /// Opt-in end-to-end proof with a real Bun helper, the installed SDK and a real Chrome, in a scratch app root:
        /// `OMP_BROWSER_NATIVE=1 cargo test native_owned_browser_lifecycle`. Requires `OMP_SDK_DIR`/`BUN_BIN` when they
        /// are not discoverable and `PUPPETEER_EXECUTABLE_PATH` pointing at an installed Chrome. It creates only
        /// test-owned scratch Chrome processes and never touches the user's browser profiles.
        #[test]
        fn native_owned_browser_lifecycle() {
            if std::env::var("OMP_BROWSER_NATIVE").as_deref() != Ok("1") {
                return;
            }
            let root = scratch();
            let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(PathBuf::from)
                .expect("home directory");
            let chrome = PathBuf::from(
                std::env::var_os("PUPPETEER_EXECUTABLE_PATH").expect("PUPPETEER_EXECUTABLE_PATH"),
            );
            fs::create_dir_all(root.join("terminal-browsers")).unwrap();
            let supervisor = BrowserSupervisor::new(root.clone(), home.clone());
            supervisor.materialize().unwrap();
            let wait = |what: &str, mut done: Box<dyn FnMut() -> bool + '_>| {
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                while !done() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "timed out waiting for {what}"
                    );
                    std::thread::sleep(Duration::from_millis(25));
                }
            };
            let only_root = |s: &Arc<BrowserSupervisor>, id: &str| {
                let roots: Vec<_> = s
                    .owned_roots()
                    .into_iter()
                    .filter(|r| r.instance_id == id)
                    .collect();
                assert_eq!(roots.len(), 1, "exactly one owned Chrome root for {id}");
                (roots[0].pid, roots[0].creation_time_100ns)
            };

            // Two terminals each own their own browser; the unrelated scratch process is never touched.
            let (program, args) = shell_command(120);
            let bystander = ContainedChild::spawn(ContainedSpec {
                subject: "bystander".into(),
                program,
                args,
                cwd: None,
                env_set: vec![],
                env_remove: vec![],
                stdout: StdioMode::Null,
                stderr: StdioMode::Null,
                capture_limit: 16,
            })
            .unwrap();
            supervisor.register("native-a", 1, "token-a").unwrap();
            supervisor.register("native-b", 1, "token-b").unwrap();
            let a = supervisor.prepare_tab("native-a", 1, "main", None).unwrap();
            supervisor.prepare_tab("native-b", 1, "main", None).unwrap();
            assert!(a["cdpUrl"].as_str().is_some());
            let first_a = only_root(&supervisor, "native-a");
            let first_b = only_root(&supervisor, "native-b");
            assert_ne!(first_a.0, first_b.0);
            assert!(processes_alive(first_a) && processes_alive(first_b));
            assert_eq!(
                supervisor
                    .snapshot()
                    .iter()
                    .find(|r| r.instance_id == "native-a")
                    .unwrap()
                    .pid,
                Some(first_a.0)
            );

            // A second supervisor on the same app root cannot take over a leased profile.
            let rival = BrowserSupervisor::new(root.clone(), home.clone());
            rival.register("native-a", 9, "token-rival").unwrap();
            let busy = rival.prepare_tab("native-a", 9, "main", None).unwrap_err();
            assert!(busy.contains("browser_profile_busy"), "{busy}");
            assert!(
                processes_alive(first_a),
                "a rejected rival must not touch the owner's Chrome"
            );
            rival.shutdown();
            assert!(processes_alive(first_a));

            // Killing only the helper retires every browser job it owned; the profile is immediately reusable.
            supervisor.kill_helper_process_for_test();
            wait(
                "helper-only kill to retire its browsers",
                Box::new(|| !processes_alive(first_a) && !processes_alive(first_b)),
            );
            assert!(supervisor.owned_roots().is_empty());
            supervisor.prepare_tab("native-a", 1, "main", None).unwrap();
            let second_a = only_root(&supervisor, "native-a");
            assert_ne!(second_a, first_a);

            // Revocation retires the browser natively and releases the lease.
            supervisor.revoke("native-a", 1).unwrap();
            assert!(!processes_alive(second_a));
            assert!(supervisor.owned_roots().is_empty());
            assert!(
                native::acquire_lease(&profile_layout(&root, "native-a").1.join("owner.lock"))
                    .is_ok_and(drop_lease)
            );

            // A foreign Chrome already running on a profile is never adopted or killed: the owned launch hands off
            // and exits, the user gets closure guidance, and the foreign process survives.
            let (_, _, foreign_profile) = profile_layout(&root, "native-foreign");
            fs::create_dir_all(&foreign_profile).unwrap();
            let foreign = ContainedChild::spawn(ContainedSpec {
                subject: "foreign chrome".into(),
                program: chrome.clone(),
                args: [
                    format!("--user-data-dir={}", foreign_profile.display()),
                    "--headless=new".into(),
                    "--remote-debugging-port=0".into(),
                    "--no-first-run".into(),
                    "about:blank".into(),
                ]
                .map(OsString::from)
                .to_vec(),
                cwd: None,
                env_set: vec![],
                env_remove: vec![],
                stdout: StdioMode::Null,
                stderr: StdioMode::Capture,
                capture_limit: 4096,
            })
            .unwrap();
            foreign
                .stderr()
                .wait_match(Duration::from_secs(20), |t| {
                    t.contains("DevTools listening").then_some(())
                })
                .expect("foreign chrome starts");
            supervisor
                .register("native-foreign", 1, "token-foreign")
                .unwrap();
            let refused = supervisor
                .prepare_tab("native-foreign", 1, "main", None)
                .unwrap_err();
            assert!(
                refused.contains("normally"),
                "closure guidance expected: {refused}"
            );
            assert_eq!(
                foreign.try_wait().unwrap(),
                None,
                "foreign Chrome must survive"
            );
            assert!(supervisor
                .owned_roots()
                .iter()
                .all(|r| r.instance_id != "native-foreign"));
            foreign.terminate().unwrap();

            // Shutdown retires everything that is left; the bystander is untouched until the test ends it.
            supervisor.shutdown();
            assert!(!processes_alive(first_b) && supervisor.owned_roots().is_empty());
            assert_eq!(bystander.try_wait().unwrap(), None);
            bystander.terminate().unwrap();
            let _ = fs::remove_dir_all(root);
        }

        fn drop_lease(lease: native::Lease) -> bool {
            drop(lease);
            true
        }
    }
}
