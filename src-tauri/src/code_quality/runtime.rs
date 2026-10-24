//! Container runtime contract. Concrete adapters (Docker Desktop, Docker in WSL, Podman) implement
//! `Engine` with direct argv `Command`s; the service and scan jobs only ever see this trait, which is
//! also the seam for deterministic fake engines in tests. No adapter may fall back to another runtime.
use super::types::{QualityRuntime, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[cfg(unix)]
pub(crate) mod native;

pub const OWNER_LABEL: &str = "dev.udvikler.omp-manager.owner";
pub const ROLE_LABEL: &str = "dev.udvikler.omp-manager.role";
pub const ROLE_SONAR: &str = "quality-sonar";
pub const ROLE_DB: &str = "quality-db";
pub const ROLE_SCAN: &str = "quality-scan";

/// Cooperative cancellation + bounded progress sink handed to every long engine call.
#[derive(Clone)]
pub struct Ctl {
    pub cancel: Arc<AtomicBool>,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
}

impl Ctl {
    pub fn new(log: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        Ctl { cancel: Arc::new(AtomicBool::new(false)), log }
    }
    pub fn silent() -> Self {
        Ctl::new(Arc::new(|_| {}))
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn say(&self, line: &str) {
        (self.log)(line)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publish {
    pub host_ip: String,
    pub host_port: u16,
    pub container_port: u16,
}

#[derive(Clone, Default)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub labels: Vec<(String, String)>,
    /// Non-secret `NAME=value` environment.
    pub env: Vec<(String, String)>,
    /// Secret environment: passed through the engine CLI's process environment as `-e NAME`
    /// (never as `NAME=value` argv, never logged).
    pub secret_env: Vec<(String, String)>,
    /// Engine network name, or `Some("none")` for no networking.
    pub network: Option<String>,
    pub network_aliases: Vec<String>,
    pub publish: Option<Publish>,
    pub cpus: Option<f32>,
    pub memory: Option<String>,
    /// (named volume, absolute container path)
    pub volumes: Vec<(String, String)>,
    pub tmpfs: Vec<String>,
    /// e.g. `nofile=131072:131072`
    pub ulimits: Vec<String>,
    pub user: Option<String>,
    pub workdir: Option<String>,
    pub entrypoint: Option<Vec<String>>,
    pub command: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub image: String,
    pub running: bool,
    pub status: String,
    pub exit_code: Option<i64>,
    pub labels: HashMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineProbe {
    /// Stable identity of the daemon (server ID / machine identity); changes when the engine is replaced.
    pub fingerprint: String,
    pub os: String,
    pub memory_bytes: u64,
    pub max_map_count: Option<u64>,
    pub file_max: Option<u64>,
    #[cfg(unix)]
    pub cpu_limits_unsupported: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecOutput {
    pub exit_code: Option<i64>,
    /// Bounded (<= 1 MiB each) tails; the rest is drained and discarded.
    pub stdout: String,
    pub stderr: String,
}

pub trait Engine: Send + Sync {
    fn runtime(&self) -> &QualityRuntime;
    /// Read-only: daemon reachable and usable. Error text is the actionable setup message.
    fn probe(&self, ctl: &Ctl) -> Result<EngineProbe>;
    /// Start the selected engine itself when stopped (Desktop app, distro docker service, podman machine)
    /// and wait until `probe` succeeds. Never stops it, never changes global defaults.
    fn ensure_started(&self, ctl: &Ctl) -> Result<()>;
    fn image_exists(&self, image: &str, ctl: &Ctl) -> Result<bool>;
    fn pull(&self, image: &str, ctl: &Ctl) -> Result<()>;
    /// Build `tag` from an app-owned context directory (never a user project).
    fn build_image(&self, tag: &str, context_dir: &Path, ctl: &Ctl) -> Result<()>;
    fn network_create(&self, name: &str, labels: &[(String, String)], ctl: &Ctl) -> Result<()>;
    fn volume_create(&self, name: &str, labels: &[(String, String)], ctl: &Ctl) -> Result<()>;
    fn create(&self, spec: &ContainerSpec, ctl: &Ctl) -> Result<String>;
    /// `Ok(None)` when no such container exists.
    fn inspect(&self, id: &str, ctl: &Ctl) -> Result<Option<ContainerInfo>>;
    /// All containers (running or not) carrying `OWNER_LABEL=<owner>`.
    fn list_owned(&self, owner: &str, ctl: &Ctl) -> Result<Vec<ContainerInfo>>;
    fn start(&self, id: &str, ctl: &Ctl) -> Result<()>;
    /// Graceful stop with `grace_secs`, then verify it is not running.
    fn stop(&self, id: &str, grace_secs: u32, ctl: &Ctl) -> Result<()>;
    /// Block until exit (or timeout/cancel) and return the exit code. Cancel stops nothing by itself.
    fn wait(&self, id: &str, timeout_secs: u64, ctl: &Ctl) -> Result<i64>;
    /// Stream `tar_file` into `dest_dir` inside the (possibly stopped) container via `cp - id:dest_dir`.
    fn copy_in(&self, id: &str, dest_dir: &str, tar_file: &Path, ctl: &Ctl) -> Result<()>;
    /// Stream `src` (file or directory) out of the container into `tar_file` via `cp id:src -`.
    fn copy_out(&self, id: &str, src: &str, tar_file: &Path, ctl: &Ctl) -> Result<()>;
    fn remove(&self, id: &str, force: bool, ctl: &Ctl) -> Result<()>;
    fn exec(&self, id: &str, argv: &[String], secret_env: &[(String, String)], timeout_secs: u64, ctl: &Ctl) -> Result<ExecOutput>;
    /// Last `tail` lines of combined container output (bounded; for diagnostics only).
    fn logs(&self, id: &str, tail: u32, ctl: &Ctl) -> Result<String>;
}

/// Builds the concrete engine for a runtime selection. The service holds one factory so tests can
/// substitute a fake without touching the lifecycle code.
pub trait EngineFactory: Send + Sync {
    fn engine(&self, runtime: &QualityRuntime) -> Result<Arc<dyn Engine>>;
}

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const TAIL_BYTES: usize = 1024 * 1024;
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const PREPARE_TIMEOUT: Duration = Duration::from_secs(20 * 60);

pub(crate) struct RunOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub cancelled: bool,
}

fn process_text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.iter().skip(1).step_by(2).any(|b| *b == 0) {
        let bytes = bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes);
        let words: Vec<u16> = bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        String::from_utf16_lossy(&words)
    } else { String::from_utf8_lossy(bytes).into_owned() }
}

impl RunOutput {
    pub(crate) fn text(&self) -> String { process_text(&self.stdout).trim().to_owned() }
    pub(crate) fn diagnostic(&self) -> String {
        let text = process_text(&self.stderr);
        if !text.trim().is_empty() { return text.trim().to_owned(); }
        // WSL writes its setup error to UTF-16 stdout on some releases. Return only the
        // recognized code, never arbitrary inspect stdout containing container environments.
        let stdout = process_text(&self.stdout).to_uppercase();
        for code in ["HCS_E_SERVICE_NOT_AVAILABLE", "0X80370114", "0X80370102", "WSL_E_WSL_OPTIONAL_COMPONENT_REQUIRED"] {
            if stdout.contains(code) { return format!("WSL prerequisite failure: {code}"); }
        }
        format!("engine command exited with {}", self.status)
    }
    pub(crate) fn require_success(&self, subject: &str) -> Result<()> {
        if self.cancelled { return Err(format!("{subject} cancelled.")); }
        if self.timed_out { return Err(format!("{subject} timed out.")); }
        if !self.status.success() { return Err(format!("{subject}: {}", self.diagnostic())); }
        Ok(())
    }
}

/// Preserve OS/tool discovery variables, but never inherit model/cloud/registry credentials,
/// DOCKER_HOST, DOCKER_CONTEXT, CONTAINER_HOST, WSLENV or an ambient engine selector.
fn sanitize_environment(cmd: &mut Command) -> Vec<String> {
    let explicit: Vec<(OsString, Option<OsString>)> = cmd.get_envs()
        .map(|(k, v)| (k.to_owned(), v.map(OsStr::to_owned))).collect();
    cmd.env_clear();
    for key in ["SystemRoot", "WINDIR", "PATH", "PATHEXT", "TEMP", "TMP", "USERPROFILE", "USERNAME",
        "HOMEDRIVE", "HOMEPATH", "ALLUSERSPROFILE", "LOCALAPPDATA", "APPDATA", "PROGRAMFILES",
        "PROGRAMFILES(X86)", "PROGRAMW6432", "COMMONPROGRAMFILES", "PROGRAMDATA", "COMSPEC",
        "HOME", "LANG", "LC_ALL"] {
        if let Some(value) = std::env::var_os(key) { cmd.env(key, value); }
    }
    let mut redact = Vec::new();
    for (key, value) in explicit {
        if let Some(value) = value {
            // Values supplied through explicit process env are credential material. Short values
            // (flags such as BuildKit=1) and WSLENV name lists are structural, not secrets.
            if key != "WSLENV" && value.len() >= 8 { redact.push(value.to_string_lossy().into_owned()); }
            cmd.env(key, value);
        } else { cmd.env_remove(key); }
    }
    redact
}

fn drain(mut input: impl Read, mut output: Option<File>, maximum: u64) -> std::io::Result<Vec<u8>> {
    let mut tail = std::collections::VecDeque::with_capacity(TAIL_BYTES);
    let mut buf = [0u8; 32 * 1024];
    let mut total = 0u64;
    let mut failure = None;
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => { failure = Some(e); break; }
        };
        total = total.saturating_add(n as u64);
        if let Some(file) = output.as_mut() {
            if total > maximum {
                failure = Some(std::io::Error::other("Container archive exceeds the 4 GiB transfer limit."));
                output = None; // continue draining to avoid a pipe deadlock
            } else if let Err(e) = file.write_all(&buf[..n]) {
                failure = Some(e);
                output = None;
            }
        }
        if tail.len() + n > TAIL_BYTES {
            let discard = tail.len() + n - TAIL_BYTES;
            tail.drain(..discard);
        }
        tail.extend(buf[..n].iter().copied());
    }
    if let Some(e) = failure { Err(e) } else { Ok(Vec::from(tail)) }
}

fn private_tail(bytes: &mut Vec<u8>, secrets: &[String]) {
    if secrets.is_empty() { return; }
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    for secret in secrets { text = text.replace(secret, "[REDACTED]"); }
    if text.len() > TAIL_BYTES {
        let mut start = text.len() - TAIL_BYTES;
        while !text.is_char_boundary(start) { start += 1; }
        text.drain(..start);
    }
    *bytes = text.into_bytes();
}

pub(crate) fn run_bounded(cmd: Command, stdin_file: Option<&Path>, timeout: Duration, ctl: &Ctl) -> Result<RunOutput> {
    run_process(cmd, stdin_file, None, timeout, ctl, false)
}

/// Installers are deliberately not killed at the presentation timeout: an elevated helper or
/// package installer can still be mutating the machine. The caller retains its gate until exit.
pub(crate) fn run_installer(cmd: Command, stdin_file: Option<&Path>, ctl: &Ctl) -> Result<RunOutput> {
    run_process(cmd, stdin_file, None, Duration::from_secs(30 * 60), ctl, true)
}

#[cfg(windows)]
fn installer_processes(job: &std::os::windows::io::OwnedHandle) -> Result<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::*;
    let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    let ok = unsafe { QueryInformationJobObject(job.as_raw_handle(), JobObjectBasicAccountingInformation,
        &mut accounting as *mut _ as *mut _, std::mem::size_of_val(&accounting) as u32, std::ptr::null_mut()) };
    if ok == 0 { Err(format!("Inspect installer process tree: {}", std::io::Error::last_os_error())) }
    else { Ok(accounting.ActiveProcesses) }
}

fn run_process(mut cmd: Command, stdin_file: Option<&Path>, stdout_file: Option<&Path>,
    timeout: Duration, ctl: &Ctl, installer: bool) -> Result<RunOutput> {
    if ctl.cancelled() { return Err("Code quality operation cancelled.".into()); }
    let secrets = sanitize_environment(&mut cmd);
    crate::code_diff::no_console(&mut cmd);
    cmd.stdin(match stdin_file {
        Some(path) => Stdio::from(File::open(path).map_err(|e| format!("Open input archive: {e}"))?),
        None => Stdio::null(),
    }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let target = stdout_file.map(File::create).transpose().map_err(|e| format!("Create output archive: {e}"))?;
    let mut child = cmd.spawn().map_err(|e| format!("Start code quality process: {e}"))?;
    #[cfg(windows)]
    let mut containment = match crate::process_tree::contain(child.id(), "code quality process") {
        Ok(job) => Some(job),
        Err(e) => { let _ = child.kill(); let _ = child.wait(); return Err(e); }
    };
    let stdout = child.stdout.take().ok_or("Missing code quality process stdout.")?;
    let stderr = child.stderr.take().ok_or("Missing code quality process stderr.")?;
    let out_thread = thread::spawn(move || drain(stdout, target, 4 * 1024 * 1024 * 1024));
    let err_thread = thread::spawn(move || drain(stderr, None, u64::MAX));
    let started = Instant::now();
    let mut timed_out = false;
    let mut cancelled = false;
    let mut wait_error = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(e) => { wait_error = Some(format!("Wait for code quality process: {e}")); break None; }
        }
        if started.elapsed() >= timeout || ctl.cancelled() {
            if installer {
                if !timed_out && !cancelled {
                    ctl.say("Installer still running; waiting for it to exit before unlocking code quality settings.");
                }
                timed_out |= started.elapsed() >= timeout;
                cancelled |= ctl.cancelled();
            } else {
                timed_out = started.elapsed() >= timeout;
                cancelled = ctl.cancelled();
                break None;
            }
        }
        thread::sleep(Duration::from_millis(40));
    };
    let status = match status {
        Some(status) => status,
        None => {
            // Terminate the direct child first so its exit status is a failure (closing a
            // kill-on-close job reports exit code 0), then the whole contained tree.
            let _ = child.kill();
            #[cfg(windows)]
            drop(containment.take());
            child.wait().map_err(|e| format!("Reap code quality process: {e}"))?
        }
    };
    #[cfg(windows)]
    if installer {
        if let Some(job) = &containment {
            let mut notified = timed_out || cancelled;
            loop {
                match installer_processes(job) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => {
                        // Never release/kill an unconfirmed installer tree just because its
                        // tracking probe failed. The service remains locked, visibly.
                        if !notified { ctl.say("Cannot yet confirm installer exit; waiting before unlocking code quality settings."); notified = true; }
                    }
                }
                timed_out |= started.elapsed() >= timeout;
                cancelled |= ctl.cancelled();
                if (timed_out || cancelled) && !notified {
                    ctl.say("Installer still running; waiting for all installer processes to exit.");
                    notified = true;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    #[cfg(windows)]
    drop(containment); // descendants must not retain inherited pipe handles
    let mut stdout = out_thread.join().map_err(|_| "Code quality stdout reader failed.")?
        .map_err(|e| format!("Read code quality stdout: {e}"))?;
    let mut stderr = err_thread.join().map_err(|_| "Code quality stderr reader failed.")?
        .map_err(|e| format!("Read code quality stderr: {e}"))?;
    private_tail(&mut stdout, &secrets);
    private_tail(&mut stderr, &secrets);
    if let Some(e) = wait_error { return Err(e); }
    Ok(RunOutput { status, stdout, stderr, timed_out, cancelled })
}

/// Runs a Podman machine command (`init`, `start`). `podman machine start` leaves the VM's keep-alive
/// processes running: they must neither be killed with a job object nor hold our output pipes open, so
/// output goes to temp files and only the direct child is waited on (and killed on timeout/cancel).
pub(crate) fn run_machine(mut cmd: Command, timeout: Duration, ctl: &Ctl) -> Result<RunOutput> {
    if ctl.cancelled() { return Err("Code quality operation cancelled.".into()); }
    let secrets = sanitize_environment(&mut cmd);
    crate::code_diff::no_console(&mut cmd);
    let base = std::env::temp_dir().join(format!("omp-machine-{}", uuid::Uuid::new_v4()));
    let (out_path, err_path) = (base.with_extension("out"), base.with_extension("err"));
    let result = (|| {
        let out = File::create(&out_path).map_err(|e| format!("Create machine output file: {e}"))?;
        let err = File::create(&err_path).map_err(|e| format!("Create machine output file: {e}"))?;
        cmd.stdin(Stdio::null()).stdout(Stdio::from(out)).stderr(Stdio::from(err));
        let mut child = cmd.spawn().map_err(|e| format!("Start Podman machine command: {e}"))?;
        let started = Instant::now();
        let (mut timed_out, mut cancelled) = (false, false);
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|e| format!("Wait for Podman machine command: {e}"))? {
                break status;
            }
            if started.elapsed() >= timeout || ctl.cancelled() {
                timed_out = started.elapsed() >= timeout;
                cancelled = ctl.cancelled();
                let _ = child.kill();
                break child.wait().map_err(|e| format!("Reap Podman machine command: {e}"))?;
            }
            thread::sleep(Duration::from_millis(100));
        };
        let tail = |path: &Path| {
            let mut bytes = fs::read(path).unwrap_or_default();
            if bytes.len() > TAIL_BYTES { bytes.drain(..bytes.len() - TAIL_BYTES); }
            bytes
        };
        let (mut stdout, mut stderr) = (tail(&out_path), tail(&err_path));
        private_tail(&mut stdout, &secrets);
        private_tail(&mut stderr, &secrets);
        Ok(RunOutput { status, stdout, stderr, timed_out, cancelled })
    })();
    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&err_path);
    result
}

pub(crate) fn discover_program(name: &str, standard: &[PathBuf]) -> Option<PathBuf> {
    std::env::var_os("PATH").into_iter().flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|p| p.join(name)).chain(standard.iter().cloned()).find(|p| p.is_file())
}

#[cfg(windows)]
pub(crate) fn docker_program() -> Option<PathBuf> {
    discover_program("docker.exe", &[program_files().join("Docker/Docker/resources/bin/docker.exe")])
}
#[cfg(windows)]
pub(crate) fn podman_program() -> Option<PathBuf> {
    discover_program("podman.exe", &[
        program_files().join("RedHat/Podman/podman.exe"),
        program_files().join("RedHat/Podman/resources/bin/podman.exe"),
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default().join("Programs/Podman/podman.exe"),
    ])
}
#[cfg(unix)]
pub(crate) fn docker_program() -> Option<PathBuf> { native::docker_program() }
#[cfg(unix)]
pub(crate) fn podman_program() -> Option<PathBuf> { native::podman_program() }
pub(crate) fn program_files() -> PathBuf {
    std::env::var_os("PROGRAMFILES").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:/Program Files"))
}
pub(crate) fn wsl_program() -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:/Windows"));
    discover_program("wsl.exe", &[root.join("System32/wsl.exe")])
}

pub(crate) fn validate_selector(value: &str, kind: &str) -> Result<()> {
    if value.is_empty() || value.len() > 256 || value.starts_with('-')
        || value.chars().any(|c| c.is_control()) {
        Err(format!("Choose a valid explicit {kind}."))
    } else { Ok(()) }
}

pub(crate) fn decode_wsl_list(bytes: &[u8]) -> Vec<String> {
    process_text(bytes).lines().map(|s| s.trim().trim_start_matches('\u{feff}').to_owned())
        .filter(|s| !s.is_empty()).collect()
}

pub(crate) fn wsl_distros(ctl: &Ctl) -> Result<Vec<String>> {
    let program = wsl_program().ok_or("WSL is not installed. Enable required Windows features, restart Windows, and install a Linux distro.")?;
    let mut cmd = Command::new(program);
    cmd.args(["--list", "--quiet"]);
    let out = run_bounded(cmd, None, PROBE_TIMEOUT, ctl)?;
    out.require_success("List WSL distributions")?;
    Ok(decode_wsl_list(&out.stdout))
}

pub(crate) fn wsl_running_distros(ctl: &Ctl) -> Result<Vec<String>> {
    let program = wsl_program().ok_or("WSL is not installed.")?;
    let mut cmd = Command::new(program);
    cmd.args(["--list", "--running", "--quiet"]);
    let out = run_bounded(cmd, None, PROBE_TIMEOUT, ctl)?;
    out.require_success("List running WSL distributions")?;
    Ok(decode_wsl_list(&out.stdout))
}

pub(crate) fn wsl_command(distro: &str, program: &str, args: &[&str]) -> Result<Command> {
    validate_selector(distro, "WSL distribution")?;
    let wsl = wsl_program().ok_or("WSL is not installed. Enable required Windows features and restart Windows.")?;
    let mut cmd = Command::new(wsl);
    cmd.args(["--distribution", distro, "--user", "root", "--exec", program]).args(args);
    Ok(cmd)
}

fn runtime_prefix(runtime: &QualityRuntime) -> Result<Vec<String>> {
    match runtime {
        #[cfg(unix)]
        QualityRuntime::DockerNative => Ok(Vec::new()),
        QualityRuntime::DockerDesktop => Ok(vec!["--context".into(), "desktop-linux".into()]),
        QualityRuntime::DockerWsl { distro } => {
            validate_selector(distro, "WSL distribution")?;
            Ok(vec!["--distribution".into(), distro.clone(), "--user".into(), "root".into(),
                "--exec".into(), "docker".into(), "--host".into(), "unix:///var/run/docker.sock".into()])
        }
        QualityRuntime::Podman { connection } => {
            validate_selector(connection, "Podman connection")?;
            Ok(vec!["--connection".into(), connection.clone()])
        }
    }
}

#[derive(Clone)]
pub struct CliEngine {
    runtime: QualityRuntime,
    program: PathBuf,
    prefix: Vec<String>,
    config_root: PathBuf,
    #[cfg(unix)]
    cpu_limits: Arc<std::sync::OnceLock<Option<String>>>,
}

impl CliEngine {
    pub fn new(runtime: QualityRuntime, config_root: &Path) -> Result<Self> {
        #[cfg(unix)]
        let (runtime, program, prefix) = native::resolve(runtime)?;
        #[cfg(windows)]
        let (runtime, program, prefix) = match runtime {
            QualityRuntime::DockerDesktop => (QualityRuntime::DockerDesktop,
                docker_program().ok_or("Docker Desktop is not installed. Choose Install runtime (https://www.docker.com/products/docker-desktop/).")?,
                runtime_prefix(&QualityRuntime::DockerDesktop)?),
            QualityRuntime::DockerWsl { distro } => {
                validate_selector(&distro, "WSL distribution")?;
                if !wsl_distros(&Ctl::silent())?.contains(&distro) {
                    return Err(format!("WSL distribution '{distro}' is not registered; choose one from Recheck."));
                }
                let prefix = runtime_prefix(&QualityRuntime::DockerWsl { distro: distro.clone() })?;
                (QualityRuntime::DockerWsl { distro }, wsl_program().ok_or("WSL is not installed.")?, prefix)
            }
            QualityRuntime::Podman { connection } => {
                let connection = if connection.is_empty() {
                    super::install::podman_app_connection(config_root)
                        .ok_or("Podman app-machine setup is required. Choose Install runtime to create omp-manager-quality with the WSL provider.")?
                } else { connection };
                validate_selector(&connection, "Podman connection")?;
                (QualityRuntime::Podman { connection: connection.clone() },
                    podman_program().ok_or("Podman is not installed. Choose Install runtime (https://podman.io/docs/installation).")?,
                    runtime_prefix(&QualityRuntime::Podman { connection })?)
            }
        };
        Ok(Self { runtime, program, prefix, config_root: config_root.to_owned(),
            #[cfg(unix)]
            cpu_limits: Arc::new(std::sync::OnceLock::new()),
        })
    }

    fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.prefix).args(args);
        cmd
    }
    fn transfer_command(&self, id: &str, path: &str, into: bool) -> Command {
        let address = format!("{id}:{path}");
        if into { self.command(&["cp".into(), "-".into(), address]) }
        else { self.command(&["cp".into(), address, "-".into()]) }
    }
    fn call(&self, args: &[&str], timeout: Duration, ctl: &Ctl) -> Result<RunOutput> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.prefix).args(args);
        run_bounded(cmd, None, timeout, ctl)
    }
    fn success(&self, args: &[&str], timeout: Duration, ctl: &Ctl, subject: &str) -> Result<RunOutput> {
        let out = self.call(args, timeout, ctl)?;
        out.require_success(subject)?;
        Ok(out)
    }
    fn create_command(&self, spec: &ContainerSpec) -> Result<Command> {
        validate_selector(&spec.name, "container name")?;
        validate_selector(&spec.image, "container image")?;
        let mut args = vec!["create".into(), "--name".into(), spec.name.clone(), "--restart=no".into()];
        for (key, value) in &spec.labels { args.extend(["--label".into(), format!("{key}={value}")]); }
        for (key, value) in &spec.env { args.extend(["-e".into(), format!("{key}={value}")]); }
        for (key, _) in &spec.secret_env {
            validate_env_name(key)?;
            args.extend(["-e".into(), key.clone()]);
        }
        if let Some(network) = &spec.network { args.extend(["--network".into(), network.clone()]); }
        for alias in &spec.network_aliases { args.extend(["--network-alias".into(), alias.clone()]); }
        if let Some(p) = &spec.publish {
            if p.host_ip != "127.0.0.1" { return Err("Quality containers may only publish to 127.0.0.1.".into()); }
            args.extend(["--publish".into(), format!("{}:{}:{}", p.host_ip, p.host_port, p.container_port)]);
        }
        if let Some(cpus) = spec.cpus { args.extend(["--cpus".into(), cpus.to_string()]); }
        if let Some(memory) = &spec.memory { args.extend(["--memory".into(), memory.clone()]); }
        for (volume, dest) in &spec.volumes {
            if volume.contains(['/', '\\', ':']) || volume.is_empty() || volume.starts_with('-') {
                return Err("Quality storage must use named volumes, never host bind mounts.".into());
            }
            args.extend(["--volume".into(), format!("{volume}:{dest}")]);
        }
        for dest in &spec.tmpfs { args.extend(["--tmpfs".into(), dest.clone()]); }
        for limit in &spec.ulimits { args.extend(["--ulimit".into(), limit.clone()]); }
        if let Some(user) = &spec.user { args.extend(["--user".into(), user.clone()]); }
        if let Some(dir) = &spec.workdir { args.extend(["--workdir".into(), dir.clone()]); }
        let entry_args = if let Some(entry) = &spec.entrypoint {
            let first = entry.first().ok_or("Container entrypoint must not be empty.")?;
            args.extend(["--entrypoint".into(), first.clone()]);
            &entry[1..]
        } else { &[] };
        args.push(spec.image.clone());
        args.extend(entry_args.iter().cloned());
        args.extend(spec.command.iter().cloned());
        let mut cmd = self.command(&args);
        for (key, value) in &spec.secret_env { cmd.env(key, value); }
        // WSL only transfers explicitly enumerated secrets; ambient WSLENV is never inherited.
        if matches!(self.runtime, QualityRuntime::DockerWsl { .. }) && !spec.secret_env.is_empty() {
            cmd.env("WSLENV", spec.secret_env.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(":"));
        }
        Ok(cmd)
    }

    fn inspect_json(&self, kind: &str, id: &str, ctl: &Ctl) -> Result<Option<serde_json::Value>> {
        validate_selector(id, kind)?;
        let out = self.call(&[kind, "inspect", id], PROBE_TIMEOUT, ctl)?;
        if !out.status.success() {
            if out.cancelled || out.timed_out { out.require_success("Inspect container resource")?; }
            let text = out.diagnostic().to_lowercase();
            if text.contains("no such") || text.contains("not found") || text.contains("does not exist") || text.contains("not known") || text.contains("failed to find") { return Ok(None); }
            return Err(format!("Inspect container resource: {}", out.diagnostic()));
        }
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| "Invalid or oversized container inspect response.")?;
        Ok(json.as_array().and_then(|a| a.first()).cloned())
    }
}

fn validate_env_name(key: &str) -> Result<()> {
    if key.is_empty() || !key.bytes().enumerate().all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())) {
        return Err("Invalid managed environment variable name.".into());
    }
    Ok(())
}

fn verify_resource_labels(kind: &str, name: &str, value: &serde_json::Value, labels: &[(String, String)]) -> Result<()> {
    // Docker and `podman volume inspect` say "Labels"; `podman network inspect` says "labels".
    let actual = value.get("Labels").or_else(|| value.get("labels")).and_then(|v| v.as_object());
    if labels.iter().all(|(key, expected)| actual.and_then(|v| v.get(key)).and_then(|v| v.as_str()) == Some(expected.as_str())) {
        Ok(())
    } else {
        Err(format!("Existing {kind} '{name}' has different ownership labels; it will not be adopted or removed."))
    }
}

fn container_info(v: &serde_json::Value) -> Result<ContainerInfo> {
    let id = v["Id"].as_str().or_else(|| v["ID"].as_str()).ok_or("Container inspect omitted its ID.")?;
    let labels = v.pointer("/Config/Labels").and_then(|v| v.as_object()).map(|labels|
        labels.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.into()))).collect()).unwrap_or_default();
    Ok(ContainerInfo {
        id: id.into(), name: v["Name"].as_str().unwrap_or("").trim_start_matches('/').into(),
        image: v.pointer("/Config/Image").and_then(|v| v.as_str()).unwrap_or("").into(),
        running: v.pointer("/State/Running").and_then(|v| v.as_bool()).unwrap_or(false),
        status: v.pointer("/State/Status").and_then(|v| v.as_str()).unwrap_or("unknown").into(),
        exit_code: v.pointer("/State/ExitCode").and_then(|v| v.as_i64()), labels,
    })
}

impl Engine for CliEngine {
    fn runtime(&self) -> &QualityRuntime { &self.runtime }
    #[cfg(unix)]
    fn probe(&self, ctl: &Ctl) -> Result<EngineProbe> { native::probe(self, ctl) }
    #[cfg(windows)]
    fn probe(&self, ctl: &Ctl) -> Result<EngineProbe> {
        if let QualityRuntime::DockerWsl { distro } = &self.runtime {
            if !wsl_running_distros(ctl)?.contains(distro) {
                return Err(format!("WSL distribution '{distro}' is stopped. Docker installation/readiness cannot be inspected without starting it. Choose Start / retry; read-only Recheck never starts a distro."));
            }
        }
        if matches!(self.runtime, QualityRuntime::DockerDesktop) {
            let out = self.success(&["context", "inspect", "desktop-linux"], PROBE_TIMEOUT, ctl, "Inspect Docker Desktop context")?;
            let contexts: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| "Invalid Docker context response.")?;
            let host = contexts.pointer("/0/Endpoints/docker/Host").and_then(|v| v.as_str()).unwrap_or("");
            if !host.eq_ignore_ascii_case("npipe:////./pipe/dockerDesktopLinuxEngine") {
                return Err("desktop-linux must point to the local Docker Desktop Linux named pipe, not a remote engine or Podman.".into());
            }
        }
        let out = self.success(&["info", "--format", "{{json .}}"], PROBE_TIMEOUT, ctl, "Probe selected container daemon")?;
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| "Invalid or oversized engine info response.")?;
        let podman = matches!(self.runtime, QualityRuntime::Podman { .. });
        let os = if podman { v.pointer("/host/os").and_then(|v| v.as_str()) }
            else { v["OSType"].as_str() }.unwrap_or("");
        if os != "linux" { return Err("The selected quality runtime must provide a Linux container engine.".into()); }
        let operating = v["OperatingSystem"].as_str().unwrap_or("");
        let name = v["Name"].as_str().unwrap_or("");
        if matches!(self.runtime, QualityRuntime::DockerWsl { .. }) &&
            (operating.to_lowercase().contains("docker desktop") || name.to_lowercase().contains("docker-desktop")) {
            return Err("Choose Docker Desktop for this distro's Desktop socket.".into());
        }
        if matches!(self.runtime, QualityRuntime::DockerDesktop) &&
            !operating.to_lowercase().contains("docker desktop") {
            return Err("The desktop-linux context does not identify the local Docker Desktop daemon.".into());
        }
        if let QualityRuntime::DockerWsl { distro } = &self.runtime {
            // A Desktop integration CLI symlink is not a distro-owned Docker installation.
            let cmd = wsl_command(distro, "/bin/sh", &["-c",
                "test -r /etc/os-release || exit 4; p=$(command -v docker) || exit 2; p=$(readlink -f \"$p\"); case \"$p\" in /mnt/wsl/docker-desktop/*|/mnt/c/*|*.exe) exit 3;; esac; test -S /var/run/docker.sock || exit 5; cat /etc/os-release"])?;
            let check = run_bounded(cmd, None, PROBE_TIMEOUT, ctl)?;
            if !check.status.success() { return Err("Choose Docker Desktop for this distro's Desktop socket, or install the distro's own Docker Engine.".into()); }
        }
        let id = if podman {
            v.pointer("/host/hostname").and_then(|v| v.as_str())
        } else { v["ID"].as_str() }.filter(|s| !s.is_empty()).ok_or("The engine did not return a stable daemon identity.")?;
        let graph = v.pointer("/store/graphRoot").and_then(|v| v.as_str()).unwrap_or("");
        let mut podman_machine_identity = String::new();
        if podman {
            // A rebuilt local machine can reuse hostname/storage paths; its creation identity
            // makes old resource ownership unreachable instead of silently adopted.
            if let Some(machine) = super::install::podman_machine_for_connection(&self.config_root, &self.runtime, ctl) {
                podman_machine_identity = super::install::podman_machine_identity(&machine, ctl).unwrap_or_default();
            }
        }
        let fingerprint = if podman { format!("podman:{id}:{graph}:{podman_machine_identity}") } else { format!("docker:{id}") };
        let memory_bytes = if podman { v.pointer("/host/memTotal").and_then(|v| v.as_u64()) }
            else { v["MemTotal"].as_u64() }.ok_or("Engine probe omitted available VM memory.")?;
        let mut max_map_count = None;
        let mut file_max = None;
        let limits = match &self.runtime {
            QualityRuntime::DockerWsl { distro } => Some(wsl_command(distro, "/bin/sh",
                &["-c", "cat /proc/sys/vm/max_map_count /proc/sys/fs/file-max"])?),
            QualityRuntime::Podman { .. } => {
                super::install::podman_machine_for_connection(&self.config_root, &self.runtime, ctl).map(|machine| {
                    let mut cmd = Command::new(&self.program);
                    cmd.args(["machine", "ssh", &machine, "cat", "/proc/sys/vm/max_map_count", "/proc/sys/fs/file-max"]);
                    cmd
                })
            }
            QualityRuntime::DockerDesktop => {
                // Reading settings must not boot Desktop's WSL VM. Hyper-V-backed Desktop
                // has no exposed distro; leave limits unknown with actionable setup guidance.
                if wsl_running_distros(ctl).unwrap_or_default().iter().any(|d| d == "docker-desktop") {
                    Some(wsl_command("docker-desktop", "cat", &[
                        "/proc/sys/vm/max_map_count", "/proc/sys/fs/file-max"])?)
                } else { None }
            }
        };
        if let Some(cmd) = limits {
            let out = run_bounded(cmd, None, PROBE_TIMEOUT, ctl)?;
            if out.status.success() {
                let numbers: Vec<u64> = out.text().split_whitespace().filter_map(|s| s.parse().ok()).collect();
                max_map_count = numbers.first().copied();
                file_max = numbers.get(1).copied();
            }
        }
        Ok(EngineProbe { fingerprint, os: os.into(), memory_bytes, max_map_count, file_max })
    }
    #[cfg(unix)]
    fn ensure_started(&self, ctl: &Ctl) -> Result<()> { native::ensure_started(self, ctl) }
    #[cfg(windows)]
    fn ensure_started(&self, ctl: &Ctl) -> Result<()> {
        if self.probe(ctl).is_ok() { return Ok(()); }
        match &self.runtime {
            QualityRuntime::DockerDesktop => {
                let app = program_files().join("Docker/Docker/Docker Desktop.exe");
                if !app.is_file() { return Err("Docker Desktop.exe is missing; reinstall Docker Desktop using Install runtime.".into()); }
                let mut cmd = Command::new(app);
                sanitize_environment(&mut cmd);
                crate::code_diff::no_console(&mut cmd);
                // The shared Desktop daemon is intentionally not owned by a kill-on-close job.
                cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
                let mut child = cmd.spawn().map_err(|e| format!("Start Docker Desktop: {e}"))?;
                thread::spawn(move || { let _ = child.wait(); });
            }
            QualityRuntime::DockerWsl { distro } => {
                let mut cmd = wsl_command(distro, "/bin/sh", &["-c",
                    "if command -v systemctl >/dev/null 2>&1 && test -d /run/systemd/system; then systemctl start docker; else service docker start; fi"])?;
                // Only start the distro service; never wsl --shutdown.
                crate::code_diff::no_console(&mut cmd);
                run_bounded(cmd, None, Duration::from_secs(90), ctl)?.require_success("Start distro Docker service")?;
            }
            QualityRuntime::Podman { .. } => {
                let machine = super::install::podman_machine_for_connection(&self.config_root, &self.runtime, ctl)
                    .ok_or("Selected Podman connection is unreachable. Start its remote machine, or select an installed local Podman machine connection.")?;
                let mut cmd = Command::new(&self.program);
                cmd.args(["machine", "start", &machine]);
                run_machine(cmd, Duration::from_secs(180), ctl)?.require_success("Start selected Podman machine")?;
            }
        }
        let deadline = Instant::now() + Duration::from_secs(180);
        let mut last = String::new();
        while Instant::now() < deadline {
            if ctl.cancelled() { return Err("Start runtime cancelled.".into()); }
            match self.probe(ctl) { Ok(_) => return Ok(()), Err(e) => last = e }
            thread::sleep(Duration::from_secs(1));
        }
        Err(format!("Selected runtime did not become ready: {last}"))
    }
    fn image_exists(&self, image: &str, ctl: &Ctl) -> Result<bool> {
        Ok(self.inspect_json("image", image, ctl)?.is_some())
    }
    fn pull(&self, image: &str, ctl: &Ctl) -> Result<()> {
        self.success(&["pull", image], PREPARE_TIMEOUT, ctl, "Pull pinned analysis image").map(|_| ())
    }
    fn build_image(&self, tag: &str, context_dir: &Path, ctl: &Ctl) -> Result<()> {
        let mut cmd = self.command(&["build".into(), "--tag".into(), tag.into(), "-".into()]);
        // WSL cannot consume Windows paths. Send a tar context on stdin on every adapter.
        let tar = self.config_root.join(format!("quality-build-{}.tar", uuid::Uuid::new_v4()));
        super::snapshot::pack_dir_tar(context_dir, &tar)?;
        cmd.env("DOCKER_BUILDKIT", "1");
        let result = run_bounded(cmd, Some(&tar), PREPARE_TIMEOUT, ctl)
            .and_then(|out| out.require_success("Build pinned analysis image"));
        let _ = fs::remove_file(tar);
        result
    }
    fn network_create(&self, name: &str, labels: &[(String, String)], ctl: &Ctl) -> Result<()> {
        if let Some(existing) = self.inspect_json("network", name, ctl)? {
            return verify_resource_labels("network", name, &existing, labels);
        }
        let mut args = vec!["network".into(), "create".into()];
        for (key, value) in labels { args.extend(["--label".into(), format!("{key}={value}")]); }
        args.push(name.into());
        run_bounded(self.command(&args), None, PROBE_TIMEOUT, ctl)?.require_success("Create owned network")
    }
    fn volume_create(&self, name: &str, labels: &[(String, String)], ctl: &Ctl) -> Result<()> {
        if let Some(existing) = self.inspect_json("volume", name, ctl)? {
            return verify_resource_labels("volume", name, &existing, labels);
        }
        let mut args = vec!["volume".into(), "create".into()];
        for (key, value) in labels { args.extend(["--label".into(), format!("{key}={value}")]); }
        args.push(name.into());
        run_bounded(self.command(&args), None, PROBE_TIMEOUT, ctl)?.require_success("Create persistent owned volume")
    }
    fn create(&self, spec: &ContainerSpec, ctl: &Ctl) -> Result<String> {
        // Creating from an image that is not local would pull inside the short create timeout.
        if !self.image_exists(&spec.image, ctl)? {
            ctl.say(&format!("Downloading {} (first use only)", spec.image));
            self.pull(&spec.image, ctl)?;
        }
        #[cfg(unix)]
        let adjusted = {
            let mut copy = spec.clone();
            if copy.cpus.is_some() {
                if let Some(reason) = native::cpu_limit_problem(self, ctl) {
                    ctl.say(&format!("CPU limit unavailable: {reason}. Memory limit retained."));
                    copy.cpus = None;
                }
            }
            copy
        };
        #[cfg(unix)]
        let spec = &adjusted;
        let result = run_bounded(self.create_command(spec)?, None, PROBE_TIMEOUT, ctl).and_then(|out| {
            out.require_success(&format!("Create analysis container '{}'", spec.name))?;
            let id = out.text();
            if id.len() < 12 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("Engine create for '{}' returned an invalid container ID; reconcile ownership before retrying.", spec.name));
            }
            Ok(id)
        });
        if result.is_ok() { return result; }
        // Creation can finish in the daemon just as the CLI times out/is cancelled. Discover
        // only the exact name AND full expected ownership/image, then let the caller journal
        // that ID immediately (and, for a cancelled job, remove it before returning).
        let recovery = Ctl::new(ctl.log.clone());
        if spec.labels.iter().any(|(key, value)| key == OWNER_LABEL && !value.is_empty())
            && spec.labels.iter().any(|(key, _)| key == ROLE_LABEL) {
            if let Ok(Some(info)) = self.inspect(&spec.name, &recovery) {
                if info.image == spec.image && spec.labels.iter().all(|(key, value)| info.labels.get(key) == Some(value)) {
                    return Ok(info.id);
                }
            }
        }
        result
    }
    fn inspect(&self, id: &str, ctl: &Ctl) -> Result<Option<ContainerInfo>> {
        self.inspect_json("container", id, ctl)?.as_ref().map(container_info).transpose()
    }
    fn list_owned(&self, owner: &str, ctl: &Ctl) -> Result<Vec<ContainerInfo>> {
        validate_selector(owner, "installation owner")?;
        let filter = format!("label={OWNER_LABEL}={owner}");
        let out = self.success(&["ps", "--all", "--no-trunc", "--filter", &filter, "--format", "{{.ID}}"],
            PROBE_TIMEOUT, ctl, "Find exactly owned containers")?;
        let mut result = Vec::new();
        for id in out.text().lines() {
            if let Some(info) = self.inspect(id.trim(), ctl)? {
                if info.labels.get(OWNER_LABEL).map(String::as_str) == Some(owner) { result.push(info); }
            }
        }
        Ok(result)
    }
    fn start(&self, id: &str, ctl: &Ctl) -> Result<()> {
        self.success(&["start", id], PROBE_TIMEOUT, ctl, "Start owned container").map(|_| ())
    }
    fn stop(&self, id: &str, grace_secs: u32, ctl: &Ctl) -> Result<()> {
        if !self.inspect(id, ctl)?.map(|c| c.running).unwrap_or(false) { return Ok(()); }
        let out = self.call(&["stop", "--time", &grace_secs.to_string(), id],
            Duration::from_secs(u64::from(grace_secs) + 30), ctl)?;
        out.require_success("Stop owned container")?;
        if self.inspect(id, ctl)?.map(|c| c.running).unwrap_or(false) {
            return Err("Owned container is still running after stop; no other backend may start.".into());
        }
        Ok(())
    }
    fn wait(&self, id: &str, timeout_secs: u64, ctl: &Ctl) -> Result<i64> {
        let started = Instant::now();
        loop {
            if ctl.cancelled() { return Err("Container wait cancelled.".into()); }
            let info = self.inspect(id, ctl)?.ok_or("Analysis container disappeared before its exit was observed.")?;
            if !info.running { return info.exit_code.ok_or("Stopped container did not report an exit code.".into()); }
            if started.elapsed() >= Duration::from_secs(timeout_secs) { return Err("Container wait timed out.".into()); }
            thread::sleep(Duration::from_millis(200));
        }
    }
    fn copy_in(&self, id: &str, dest_dir: &str, tar_file: &Path, ctl: &Ctl) -> Result<()> {
        super::snapshot::validate_tar(tar_file, 4 * 1024 * 1024 * 1024)?;
        let cmd = self.transfer_command(id, dest_dir, true);
        run_bounded(cmd, Some(tar_file), PREPARE_TIMEOUT, ctl)?.require_success("Copy source archive into container")
    }
    fn copy_out(&self, id: &str, src: &str, tar_file: &Path, ctl: &Ctl) -> Result<()> {
        if matches!(self.runtime, QualityRuntime::Podman { .. }) {
            // `podman cp <container>:<path> -` cannot stream to stdout through the Windows remote client
            // ("/dev/stdout could not be found on the host"): copy into a private temp directory instead.
            let staging = std::env::temp_dir().join(format!("omp-cpout-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&staging).map_err(|e| format!("Create report staging folder: {e}"))?;
            let result = (|| {
                let mut args: Vec<String> = vec!["cp".into(), format!("{id}:{src}")];
                args.push(staging.to_string_lossy().into_owned());
                run_bounded(self.command(&args), None, PREPARE_TIMEOUT, ctl)?.require_success("Copy report archive out of container")?;
                super::snapshot::pack_dir_tar(&staging, tar_file)?;
                super::snapshot::validate_tar(tar_file, 4 * 1024 * 1024 * 1024).map(|_| ())
            })();
            let _ = fs::remove_dir_all(&staging);
            if result.is_err() { let _ = fs::remove_file(tar_file); }
            return result;
        }
        let cmd = self.transfer_command(id, src, false);
        let result = run_process(cmd, None, Some(tar_file), PREPARE_TIMEOUT, ctl, false)
            .and_then(|out| out.require_success("Copy report archive out of container"))
            .and_then(|()| super::snapshot::validate_tar(tar_file, 4 * 1024 * 1024 * 1024).map(|_| ()));
        if result.is_err() { let _ = fs::remove_file(tar_file); }
        result
    }
    fn remove(&self, id: &str, force: bool, ctl: &Ctl) -> Result<()> {
        let mut args = vec!["rm", id];
        if force { args.insert(1, "--force"); }
        self.success(&args, PROBE_TIMEOUT, ctl, "Remove disposable owned container").map(|_| ())
    }
    fn exec(&self, id: &str, argv: &[String], secret_env: &[(String, String)], timeout_secs: u64, ctl: &Ctl) -> Result<ExecOutput> {
        let mut args = vec!["exec".into()];
        for (key, _) in secret_env {
            validate_env_name(key)?;
            args.extend(["-e".into(), key.clone()]);
        }
        args.push(id.into());
        args.extend(argv.iter().cloned());
        let mut cmd = self.command(&args);
        for (key, value) in secret_env { cmd.env(key, value); }
        if matches!(self.runtime, QualityRuntime::DockerWsl { .. }) && !secret_env.is_empty() {
            cmd.env("WSLENV", secret_env.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(":"));
        }
        let out = run_bounded(cmd, None, Duration::from_secs(timeout_secs), ctl)?;
        if out.cancelled { return Err("Container exec cancelled.".into()); }
        if out.timed_out { return Err("Container exec timed out.".into()); }
        Ok(ExecOutput { exit_code: out.status.code().map(i64::from), stdout: out.text(), stderr: String::from_utf8_lossy(&out.stderr).into_owned() })
    }
    fn logs(&self, id: &str, tail: u32, ctl: &Ctl) -> Result<String> {
        let out = self.success(&["logs", "--tail", &tail.to_string(), id], PROBE_TIMEOUT, ctl, "Read bounded container logs")?;
        let mut text = out.text();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        if text.len() > TAIL_BYTES {
            let mut boundary = text.len() - TAIL_BYTES;
            while !text.is_char_boundary(boundary) { boundary += 1; }
            text.drain(..boundary);
        }
        Ok(text)
    }
}

pub struct RuntimeFactory { pub config_root: PathBuf }
impl EngineFactory for RuntimeFactory {
    fn engine(&self, runtime: &QualityRuntime) -> Result<Arc<dyn Engine>> {
        Ok(Arc::new(CliEngine::new(runtime.clone(), &self.config_root)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn fixture(runtime: QualityRuntime) -> CliEngine {
        CliEngine { prefix: runtime_prefix(&runtime).unwrap(), runtime,
            program: PathBuf::from("explicit-runtime.exe"), config_root: PathBuf::from("isolated-config"),
            #[cfg(unix)]
            cpu_limits: Arc::new(std::sync::OnceLock::new()),
        }
    }
    fn args(cmd: &Command) -> Vec<String> {
        cmd.get_args().map(|s| s.to_string_lossy().into_owned()).collect()
    }
    #[test]
    fn every_engine_argv_has_an_explicit_selector() {
        let desktop = fixture(QualityRuntime::DockerDesktop);
        assert_eq!(args(&desktop.command(&["info".into()])), vec!["--context", "desktop-linux", "info"]);
        let wsl = fixture(QualityRuntime::DockerWsl { distro: "Ubuntu Å team".into() });
        assert_eq!(args(&wsl.command(&["info".into()])), vec![
            "--distribution", "Ubuntu Å team", "--user", "root", "--exec", "docker",
            "--host", "unix:///var/run/docker.sock", "info"]);
        let podman = fixture(QualityRuntime::Podman { connection: "chosen-rootless".into() });
        assert_eq!(args(&podman.command(&["info".into()])), vec!["--connection", "chosen-rootless", "info"]);
        assert!(runtime_prefix(&QualityRuntime::DockerWsl { distro: String::new() }).is_err());
        assert!(runtime_prefix(&QualityRuntime::Podman { connection: String::new() }).is_err());
        assert!(runtime_prefix(&QualityRuntime::Podman { connection: "--remote".into() }).is_err());
    }
    #[test]
    fn secrets_are_only_process_environment_not_argv() {
        for runtime in [QualityRuntime::DockerDesktop,
            QualityRuntime::DockerWsl { distro: "Ubuntu".into() },
            QualityRuntime::Podman { connection: "explicit".into() }] {
            let engine = fixture(runtime);
            let spec = ContainerSpec { name: "omp-fixture".into(), image: "pinned:1".into(),
                secret_env: vec![("SONAR_TOKEN".into(), "never-in-argv".into())],
                command: vec!["scan".into()], ..ContainerSpec::default() };
            let cmd = engine.create_command(&spec).unwrap();
            let argv = args(&cmd);
            assert!(!argv.iter().any(|s| s.contains("never-in-argv") || s.contains("SONAR_TOKEN=")));
            assert!(argv.windows(2).any(|w| w == ["-e", "SONAR_TOKEN"]));
            assert!(cmd.get_envs().any(|(key, val)| key == "SONAR_TOKEN" && val == Some(OsStr::new("never-in-argv"))));
            if matches!(engine.runtime, QualityRuntime::DockerWsl { .. }) {
                assert!(cmd.get_envs().any(|(key, val)| key == "WSLENV" && val == Some(OsStr::new("SONAR_TOKEN"))));
            }
        }
    }
    #[test]
    fn cp_preserves_space_unicode_and_refuses_host_bind_mounts() {
        for runtime in [QualityRuntime::DockerDesktop,
            QualityRuntime::DockerWsl { distro: "Debian".into() },
            QualityRuntime::Podman { connection: "quality".into() }] {
            let engine = fixture(runtime);
            let input = args(&engine.transfer_command("exact-id", "/workspace/Å project", true));
            assert_eq!(&input[input.len()-3..], &["cp", "-", "exact-id:/workspace/Å project"]);
            let output = args(&engine.transfer_command("exact-id", "/out/安全 report.json", false));
            assert_eq!(&output[output.len()-3..], &["cp", "exact-id:/out/安全 report.json", "-"]);
            let bad = ContainerSpec { name: "fixture".into(), image: "fixture:1".into(),
                volumes: vec![("C:/Users/secret".into(), "/workspace".into())], ..ContainerSpec::default() };
            assert!(engine.create_command(&bad).is_err());
        }
    }
    #[test]
    fn utf16_wsl_distro_list_is_decoded_without_nuls_or_bom() {
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend("Ubuntu\r\nDebian\r\nÅngström 开发\r\n".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode_wsl_list(&bytes), vec!["Ubuntu", "Debian", "Ångström 开发"]);
        assert_eq!(decode_wsl_list(&bytes[2..]), vec!["Ubuntu", "Debian", "Ångström 开发"]);
        assert_eq!(decode_wsl_list(b"Ubuntu\nDebian\n"), vec!["Ubuntu", "Debian"]);
    }
    #[test]
    fn ownership_matching_never_adopts_foreign_resources() {
        let labels = vec![(OWNER_LABEL.into(), "my-owner".into())];
        let mut owned = serde_json::json!({"Labels": {}});
        owned["Labels"] = serde_json::to_value(labels.iter().cloned().collect::<HashMap<_, _>>()).unwrap();
        assert!(verify_resource_labels("volume", "exact-volume", &owned, &labels).is_ok());
        assert!(verify_resource_labels("network", "exact-network", &serde_json::json!({"Labels": {}}), &labels).is_err());
    }
    fn child(mode: &str) -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "code_quality::runtime::tests::runner_child_helper", "--nocapture"]);
        cmd.env("OMP_QUALITY_RUNNER_FIXTURE", mode);
        cmd
    }
    #[test]
    fn runner_child_helper() {
        let Ok(mode) = std::env::var("OMP_QUALITY_RUNNER_FIXTURE") else { return; };
        match mode.as_str() {
            "drain" => {
                let stderr = thread::spawn(|| {
                    let mut err = std::io::stderr().lock();
                    for _ in 0..80 { err.write_all(&[b'e'; 32768]).unwrap(); }
                });
                let mut out = std::io::stdout().lock();
                for _ in 0..80 { out.write_all(&[b'o'; 32768]).unwrap(); }
                out.flush().unwrap();
                drop(out);
                stderr.join().unwrap();
                std::process::exit(0);
            }
            "sleep" => {
                thread::sleep(Duration::from_millis(600));
                if let Some(path) = std::env::var_os("OMP_QUALITY_FIXTURE_SENTINEL") { fs::write(path, b"alive").unwrap(); }
                std::process::exit(0);
            }
            "echo" => {
                std::io::copy(&mut std::io::stdin().lock(), &mut std::io::stdout().lock()).unwrap();
                std::io::stdout().flush().unwrap();
                std::process::exit(0);
            }
            "port-error" => {
                std::io::stderr().write_all(b"bind failed: address already in use; port is already allocated").unwrap();
                std::io::stderr().flush().unwrap();
                std::process::exit(1);
            }
            _ => panic!("unknown fixture mode"),
        }
    }
    #[test]
    fn runner_continuously_drains_both_streams_to_bounded_tails() {
        let out = run_bounded(child("drain"), None, Duration::from_secs(10), &Ctl::silent()).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), TAIL_BYTES);
        assert_eq!(out.stderr.len(), TAIL_BYTES);
        assert!(out.stdout.iter().all(|b| *b == b'o'));
        assert!(out.stderr.iter().all(|b| *b == b'e'));
    }
    #[test]
    fn runner_timeout_kills_and_reaps_before_returning() {
        let root = std::env::temp_dir().join(format!("quality-kill-{}", uuid::Uuid::new_v4()));
        let mut cmd = child("sleep");
        cmd.env("OMP_QUALITY_FIXTURE_SENTINEL", &root);
        let out = run_bounded(cmd, None, Duration::from_millis(150), &Ctl::silent()).unwrap();
        assert!(out.timed_out);
        assert!(!out.status.success());
        thread::sleep(Duration::from_millis(650));
        assert!(!root.exists(), "timed out child continued running");
    }
    #[test]
    fn runner_cancellation_kills_and_waits() {
        let ctl = Ctl::silent();
        let cancelling = ctl.clone();
        let task = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            cancelling.cancel.store(true, Ordering::Relaxed);
        });
        let out = run_bounded(child("sleep"), None, Duration::from_secs(5), &ctl).unwrap();
        task.join().unwrap();
        assert!(out.cancelled);
        assert!(!out.status.success());
    }
    #[test]
    fn stdin_stdout_transfer_files_support_unicode_and_space_paths() {
        let root = std::env::temp_dir().join(format!("quality Å 安全 {}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let input = root.join("source snapshot.tar");
        let output = root.join("report archive.tar");
        let bytes: Vec<u8> = (0..=255u8).cycle().take(65536).collect();
        fs::write(&input, &bytes).unwrap();
        let out = run_process(child("echo"), Some(&input), Some(&output), Duration::from_secs(10), &Ctl::silent(), false).unwrap();
        assert!(out.status.success());
        // The libtest harness writes a short prefix before entering the fixture.
        assert!(fs::read(&output).unwrap().ends_with(&bytes));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn installer_timeout_and_cancel_keep_waiting_for_actual_exit() {
        for cancel in [false, true] {
            let ctl = Ctl::silent();
            let cancellation = ctl.clone();
            let task = thread::spawn(move || {
                if cancel {
                    thread::sleep(Duration::from_millis(100));
                    cancellation.cancel.store(true, Ordering::Relaxed);
                }
            });
            let root = std::env::temp_dir().join(format!("quality-installer-{}", uuid::Uuid::new_v4()));
            let mut cmd = child("sleep");
            cmd.env("OMP_QUALITY_FIXTURE_SENTINEL", &root);
            let started = Instant::now();
            let out = run_process(cmd, None, None, Duration::from_millis(150), &ctl, true).unwrap();
            task.join().unwrap();
            assert!(out.timed_out);
            assert_eq!(out.cancelled, cancel);
            assert!(out.status.success(), "installer must be awaited, not killed");
            assert!(started.elapsed() >= Duration::from_millis(600));
            assert!(root.exists(), "installer did not finish its real mutation");
            fs::remove_file(root).unwrap();
        }
    }
    #[test]
    fn engine_failure_keeps_original_port_conflict_diagnostic() {
        let out = run_bounded(child("port-error"), None, Duration::from_secs(10), &Ctl::silent()).unwrap();
        let error = out.require_success("Start owned container").unwrap_err();
        assert!(error.contains("address already in use"));
        assert!(error.contains("port is already allocated"));
    }
}
