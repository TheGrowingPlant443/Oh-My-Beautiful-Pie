//! Linux and macOS container runtimes. There is no WSL and no app-provisioned VM here:
//!
//! * Linux: native (rootless) Podman, which is daemonless, and native Docker Engine.
//! * macOS: Podman through the user's own `podman machine` and Docker Desktop. (Written from the
//!   documented CLIs; this repository's CI cannot run macOS, so the macOS-only code is unverified.)
//!
//! The app never installs a container engine and never creates a Podman machine. It detects, probes
//! and explains. A missing or stopped engine is an actionable `Err`, never a silent fallback to
//! another runtime.
use super::*;
use crate::code_quality::types::{Prerequisite, QualityOptions};
use serde_json::Value;

const MIN_MAP_COUNT: u64 = 524_288;
const MIN_FILE_MAX: u64 = 131_072;

/// Where engine clients usually live outside a login shell's PATH.
fn standard_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".nix-profile/bin"));
        dirs.push(home.join(".local/bin"));
    }
    dirs.extend(["/run/current-system/sw/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin", "/opt/podman/bin"]
        .map(PathBuf::from));
    dirs
}

fn program(name: &str) -> Option<PathBuf> {
    discover_program(name, &standard_dirs().iter().map(|dir| dir.join(name)).collect::<Vec<_>>())
}

pub(super) fn podman_program() -> Option<PathBuf> { program("podman") }
pub(super) fn docker_program() -> Option<PathBuf> { program("docker") }

pub(crate) fn macos() -> bool { cfg!(target_os = "macos") }

pub(crate) fn missing_podman() -> String {
    if macos() {
        "Podman is not installed. Install it with Homebrew (`brew install podman`), then create and start a machine yourself (`podman machine init`, `podman machine start`) and Recheck. OMP Manager never installs software or creates machines on macOS.".into()
    } else {
        "Podman is not installed. Install it (Nix: `nix profile add nixpkgs#podman`; Debian/Ubuntu: `apt install podman`; Fedora: `dnf install podman`; Arch: `pacman -S podman`) and make sure rootless containers work (`podman info`), then Recheck. OMP Manager never installs container engines.".into()
    }
}

pub(crate) fn missing_docker() -> String {
    if macos() {
        "Docker Desktop is not installed. Install it from https://www.docker.com/products/docker-desktop/ (or Homebrew: `brew install --cask docker`), start it and Recheck. OMP Manager never installs software on macOS.".into()
    } else {
        "Docker is not installed. Install Docker Engine for your distribution (https://docs.docker.com/engine/install/), start its service, add your user to the `docker` group and Recheck. OMP Manager never installs container engines.".into()
    }
}

/// Maps a selection to the concrete client invocation. Only the runtimes of this platform resolve.
pub(super) fn resolve(runtime: QualityRuntime) -> Result<(QualityRuntime, PathBuf, Vec<String>)> {
    match runtime {
        QualityRuntime::Podman { connection } => {
            let prefix = if connection.is_empty() { Vec::new() } else {
                validate_selector(&connection, "Podman connection")?;
                vec!["--connection".into(), connection.clone()]
            };
            Ok((QualityRuntime::Podman { connection }, podman_program().ok_or_else(missing_podman)?, prefix))
        }
        QualityRuntime::DockerNative if !macos() => {
            Ok((QualityRuntime::DockerNative, docker_program().ok_or_else(missing_docker)?, Vec::new()))
        }
        QualityRuntime::DockerNative => Err("Native Docker Engine is only available on Linux; on macOS choose Docker Desktop or Podman.".into()),
        QualityRuntime::DockerDesktop if macos() => {
            Ok((QualityRuntime::DockerDesktop, docker_program().ok_or_else(missing_docker)?,
                vec!["--context".into(), "desktop-linux".into()]))
        }
        QualityRuntime::DockerDesktop => Err("Docker Desktop is not offered on Linux; choose Docker (native Engine) or Podman.".into()),
        QualityRuntime::DockerWsl { .. } => Err("Docker in WSL is only available on Windows.".into()),
    }
}

/// What an engine's `info` JSON says, reduced to the facts the lifecycle needs.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Info {
    pub os: String,
    pub id: String,
    pub graph: String,
    pub memory_bytes: u64,
    pub remote: bool,
    pub operating_system: String,
    /// Why `--cpus` cannot be honoured by this engine (`None`: it can, or nothing says it cannot).
    pub cpu_limits_unsupported: Option<String>,
}

pub(super) fn parse_podman_info(v: &Value) -> Result<Info> {
    let host = v.get("host").ok_or("Invalid Podman info response.")?;
    let rootless = host.pointer("/security/rootless").and_then(Value::as_bool).unwrap_or(false);
    let controllers: Option<Vec<&str>> = host.get("cgroupControllers").and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).collect());
    // Rootless cgroup v2 only gets the controllers systemd delegated to the user. `--cpus` needs `cpu`.
    let cpu_limits_unsupported = match (&controllers, rootless) {
        (Some(list), true) if !list.contains(&"cpu") => Some(format!(
            "rootless Podman has no delegated `cpu` cgroup controller (available: {}); delegate it with a systemd `Delegate=cpu` drop-in for user@.service to enforce CPU limits",
            if list.is_empty() { "none".to_string() } else { list.join(", ") })),
        _ => None,
    };
    Ok(Info {
        os: host.get("os").and_then(Value::as_str).unwrap_or("").into(),
        id: host.get("hostname").and_then(Value::as_str).unwrap_or("").into(),
        graph: v.pointer("/store/graphRoot").and_then(Value::as_str).unwrap_or("").into(),
        memory_bytes: host.get("memTotal").and_then(Value::as_u64).ok_or("Engine probe omitted available VM memory.")?,
        remote: host.get("serviceIsRemote").and_then(Value::as_bool).unwrap_or(false),
        operating_system: String::new(),
        cpu_limits_unsupported,
    })
}

pub(super) fn parse_docker_info(v: &Value) -> Result<Info> {
    Ok(Info {
        os: v["OSType"].as_str().unwrap_or("").into(),
        id: v["ID"].as_str().unwrap_or("").into(),
        graph: String::new(),
        memory_bytes: v["MemTotal"].as_u64().ok_or("Engine probe omitted available VM memory.")?,
        remote: false,
        operating_system: v["OperatingSystem"].as_str().unwrap_or("").into(),
        cpu_limits_unsupported: (v.get("CpuCfsQuota").and_then(Value::as_bool) == Some(false))
            .then(|| "the Docker daemon reports no CPU CFS quota support".to_string()),
    })
}

/// Reads the two kernel settings SonarQube's search engine checks. Absent/unreadable → `None`.
pub(super) fn read_limits_at(proc_sys: &Path) -> (Option<u64>, Option<u64>) {
    let read = |name: &str| fs::read_to_string(proc_sys.join(name)).ok().and_then(|t| t.trim().parse::<u64>().ok());
    (read("vm/max_map_count"), read("fs/file-max"))
}

pub(crate) fn limits_low(max_map_count: Option<u64>, file_max: Option<u64>) -> bool {
    max_map_count.is_some_and(|v| v < MIN_MAP_COUNT) || file_max.is_some_and(|v| v < MIN_FILE_MAX)
}

/// The Podman machine a macOS client talks to: the explicit connection's machine, else the default one.
/// The app never creates or starts a machine; a stopped/missing one is reported.
#[cfg(target_os = "macos")]
fn running_machine(engine: &CliEngine, ctl: &Ctl) -> Result<String> {
    let out = engine.success(&["machine", "list", "--format", "json"], PROBE_TIMEOUT, ctl, "List Podman machines")?;
    let machines: Vec<Value> = serde_json::from_slice(&out.stdout).map_err(|_| "Invalid Podman machine list.".to_string())?;
    let wanted = match &engine.runtime { QualityRuntime::Podman { connection } if !connection.is_empty() => Some(connection.as_str()), _ => None };
    let running: Vec<&Value> = machines.iter().filter(|m| m["Running"].as_bool() == Some(true)).collect();
    let chosen = running.iter().find(|m| wanted.is_some_and(|w| m["Name"].as_str().map(|n| n.trim_end_matches('*')) == Some(w)))
        .or_else(|| running.iter().find(|m| m["Default"].as_bool() == Some(true)))
        .or_else(|| if running.len() == 1 { running.first() } else { None });
    match chosen.and_then(|m| m["Name"].as_str()) {
        Some(name) => Ok(name.trim_end_matches('*').to_owned()),
        None if machines.is_empty() => Err("No Podman machine exists. Create and start one yourself (`podman machine init` then `podman machine start`), then Recheck. OMP Manager does not create machines on macOS.".into()),
        None => Err("No Podman machine is running. Start yours (`podman machine start`) and Recheck. OMP Manager does not start machines on macOS.".into()),
    }
}

pub(super) fn probe(engine: &CliEngine, ctl: &Ctl) -> Result<EngineProbe> {
    let podman = matches!(engine.runtime, QualityRuntime::Podman { .. });
    if matches!(engine.runtime, QualityRuntime::DockerNative | QualityRuntime::DockerDesktop) {
        // The selected context must be this machine's own engine socket, never a TCP/ssh remote.
        let out = engine.success(&["context", "inspect"], PROBE_TIMEOUT, ctl, "Inspect the Docker context")?;
        let contexts: Value = serde_json::from_slice(&out.stdout).map_err(|_| "Invalid Docker context response.".to_string())?;
        let host = contexts.pointer("/0/Endpoints/docker/Host").and_then(Value::as_str).unwrap_or("");
        if !host.starts_with("unix://") {
            return Err(format!("The Docker context points at '{host}', not a local engine socket. Select a local context (`docker context use default`); OMP Manager does not use remote engines."));
        }
    }
    let out = engine.success(&["info", "--format", "{{json .}}"], PROBE_TIMEOUT, ctl, "Probe selected container engine")
        .map_err(|e| explain_probe_failure(&engine.runtime, e))?;
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|_| "Invalid or oversized engine info response.".to_string())?;
    let info = if podman { parse_podman_info(&v)? } else { parse_docker_info(&v)? };
    if info.os != "linux" { return Err("The selected quality runtime must provide a Linux container engine.".into()); }
    if info.id.is_empty() { return Err("The engine did not return a stable daemon identity.".into()); }
    match &engine.runtime {
        QualityRuntime::Podman { connection } if connection.is_empty() && !macos() && info.remote => {
            return Err("Podman is configured as a remote client. Select its connection explicitly instead of the local default.".into());
        }
        QualityRuntime::DockerNative if info.operating_system.to_lowercase().contains("docker desktop") => {
            return Err("The active Docker context is Docker Desktop, not the native Docker Engine. Switch with `docker context use default`.".into());
        }
        QualityRuntime::DockerDesktop if !info.operating_system.to_lowercase().contains("docker desktop") => {
            return Err("The desktop-linux context does not identify the local Docker Desktop daemon.".into());
        }
        _ => {}
    }
    #[cfg(target_os = "macos")]
    let machine = if podman { Some(running_machine(engine, ctl)?) } else { None };
    #[cfg(not(target_os = "macos"))]
    let machine: Option<String> = None;
    let identity = machine.as_deref().and_then(|m| machine_created(engine, m, ctl)).unwrap_or_default();
    let fingerprint = if podman { format!("podman:{}:{}:{identity}", info.id, info.graph) } else { format!("docker:{}", info.id) };
    let (max_map_count, file_max) = limits(engine, machine.as_deref(), ctl);
    Ok(EngineProbe { fingerprint, os: info.os, memory_bytes: info.memory_bytes, max_map_count, file_max,
        cpu_limits_unsupported: info.cpu_limits_unsupported })
}

fn explain_probe_failure(runtime: &QualityRuntime, error: String) -> String {
    let lower = error.to_lowercase();
    match runtime {
        QualityRuntime::DockerNative if lower.contains("permission denied") =>
            format!("{error}\nYour user may not use the Docker socket. Add it to the `docker` group (then log in again) or use Podman; OMP Manager never uses sudo for Docker."),
        QualityRuntime::DockerNative if lower.contains("cannot connect") || lower.contains("is the docker daemon running") =>
            format!("{error}\nThe Docker service is not running. Start it (for example `sudo systemctl start docker`); OMP Manager never starts system services."),
        QualityRuntime::Podman { .. } if macos() && (lower.contains("connection") || lower.contains("podman machine")) =>
            format!("{error}\nStart your Podman machine (`podman machine start`); OMP Manager does not start machines on macOS."),
        _ => error,
    }
}

#[cfg(target_os = "macos")]
fn machine_created(engine: &CliEngine, machine: &str, ctl: &Ctl) -> Option<String> {
    let out = engine.success(&["machine", "inspect", machine], PROBE_TIMEOUT, ctl, "Inspect Podman machine").ok()?;
    let value: Value = serde_json::from_slice(&out.stdout).ok()?;
    value.as_array()?.first()?["Created"].as_str().map(str::to_owned)
}
#[cfg(not(target_os = "macos"))]
fn machine_created(_: &CliEngine, _: &str, _: &Ctl) -> Option<String> { None }

/// Linux reads its own kernel settings (the engine shares them); a remote Podman connection cannot be
/// inspected from here. macOS reads them from inside the machine VM, which manages them itself.
fn limits(engine: &CliEngine, machine: Option<&str>, ctl: &Ctl) -> (Option<u64>, Option<u64>) {
    #[cfg(target_os = "macos")]
    if let Some(machine) = machine {
        let mut cmd = Command::new(&engine.program);
        cmd.args(["machine", "ssh", machine, "cat", "/proc/sys/vm/max_map_count", "/proc/sys/fs/file-max"]);
        return match run_bounded(cmd, None, PROBE_TIMEOUT, ctl) {
            Ok(out) if out.status.success() => {
                let numbers: Vec<u64> = out.text().split_whitespace().filter_map(|s| s.parse().ok()).collect();
                (numbers.first().copied(), numbers.get(1).copied())
            }
            _ => (None, None),
        };
    }
    let _ = (machine, ctl);
    if macos() { return (None, None); }
    match &engine.runtime {
        QualityRuntime::Podman { connection } if !connection.is_empty() => (None, None),
        _ => read_limits_at(Path::new("/proc/sys")),
    }
}

pub(super) fn ensure_started(engine: &CliEngine, ctl: &Ctl) -> Result<()> {
    let first = match engine.probe(ctl) { Ok(_) => return Ok(()), Err(e) => e };
    match &engine.runtime {
        QualityRuntime::DockerDesktop if macos() => {
            let mut cmd = Command::new("/usr/bin/open");
            cmd.args(["-a", "Docker"]);
            run_bounded(cmd, None, PROBE_TIMEOUT, ctl)?.require_success("Start Docker Desktop")?;
        }
        QualityRuntime::Podman { .. } if macos() => {
            return Err(format!("{first}\nNo Podman machine is running. Start yours (`podman machine start`); OMP Manager does not create or start machines on macOS."));
        }
        QualityRuntime::Podman { .. } => {
            return Err(format!("{first}\nPodman needs no daemon, so there is nothing to start: fix the problem above (`podman info` must work for your user)."));
        }
        _ => return Err(first),
    }
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut last = first;
    while Instant::now() < deadline {
        if ctl.cancelled() { return Err("Start runtime cancelled.".into()); }
        match engine.probe(ctl) { Ok(_) => return Ok(()), Err(e) => last = e }
        thread::sleep(Duration::from_secs(1));
    }
    Err(format!("Selected runtime did not become ready: {last}"))
}

/// Cached answer to "may `--cpus` be passed to this engine?" (`Some(reason)` = no).
pub(super) fn cpu_limit_problem(engine: &CliEngine, ctl: &Ctl) -> Option<String> {
    engine.cpu_limits.get_or_init(|| {
        let out = engine.success(&["info", "--format", "{{json .}}"], PROBE_TIMEOUT, ctl, "Probe CPU limit support").ok()?;
        let v: Value = serde_json::from_slice(&out.stdout).ok()?;
        if matches!(engine.runtime, QualityRuntime::Podman { .. }) { parse_podman_info(&v).ok()?.cpu_limits_unsupported }
        else { parse_docker_info(&v).ok()?.cpu_limits_unsupported }
    }).clone()
}

/// Prerequisite rows for this platform's runtimes. Never installs, starts or pulls anything.
pub(crate) fn probe_prerequisites(config_root: &Path, selected: &QualityRuntime) -> (Vec<Prerequisite>, QualityOptions) {
    let ctl = Ctl::silent();
    let connections = super::super::install::podman_connections(&ctl).map(|list| list.iter()
        .filter_map(|v| v["Name"].as_str().map(str::to_owned)).collect()).unwrap_or_default();
    let options = QualityOptions { podman_connections: connections, ..QualityOptions::default() };
    let podman = match selected {
        QualityRuntime::Podman { connection } => QualityRuntime::Podman { connection: connection.clone() },
        _ => QualityRuntime::Podman { connection: String::new() },
    };
    let docker = if macos() { QualityRuntime::DockerDesktop } else { QualityRuntime::DockerNative };
    let mut result = Vec::new();
    for runtime in [podman, docker] {
        let installed = match &runtime {
            QualityRuntime::Podman { .. } => podman_program().is_some(),
            QualityRuntime::DockerDesktop => docker_program().is_some() && Path::new("/Applications/Docker.app").exists(),
            _ => docker_program().is_some(),
        };
        let probe = CliEngine::new(runtime.clone(), config_root).and_then(|engine| engine.probe(&ctl));
        let message = match &probe {
            Err(error) => Some(error.clone()),
            Ok(p) => runtime_notes(&runtime, p),
        };
        result.push(Prerequisite { runtime, installed, ready: probe.is_ok(), needs_restart: false, message });
    }
    (result, options)
}

/// Facts about a *working* engine the user should still see (never errors).
pub(crate) fn runtime_notes(runtime: &QualityRuntime, probe: &EngineProbe) -> Option<String> {
    let mut notes = Vec::new();
    if limits_low(probe.max_map_count, probe.file_max) {
        notes.push(if macos() {
            "The Podman machine VM's vm.max_map_count/fs.file-max are below what SonarQube needs; raise them inside the VM (`podman machine ssh`).".to_string()
        } else {
            format!("vm.max_map_count ({}) or fs.file-max ({}) is below SonarQube's minimum ({MIN_MAP_COUNT}/{MIN_FILE_MAX}); use Prepare runtime to raise it with administrator approval (polkit).",
                probe.max_map_count.map_or("unknown".into(), |v| v.to_string()), probe.file_max.map_or("unknown".into(), |v| v.to_string()))
        });
    }
    if let Some(reason) = &probe.cpu_limits_unsupported {
        notes.push(format!("CPU limits are not applied on {}: {reason}.", runtime.label()));
    }
    (!notes.is_empty()).then(|| notes.join("\n"))
}

pub(crate) fn pkexec_program() -> Option<PathBuf> {
    let mut dirs = standard_dirs();
    dirs.insert(0, PathBuf::from("/run/wrappers/bin")); // NixOS setuid wrappers
    dirs.iter().map(|d| d.join("pkexec")).find(|p| p.is_file())
}

/// Raises `vm.max_map_count`/`fs.file-max` after explicit consent, never lowering a value. Linux: the
/// fixed script is piped into `pkexec /bin/sh -s` (no script path is ever executed as root, so the
/// user-writable config folder cannot be raced). macOS: inside the Podman machine VM.
pub(crate) fn raise_vm_limits(engine: &dyn Engine, config_root: &Path, ctl: &Ctl) -> Result<()> {
    let script = super::super::install::materialize_vm_limits_script(config_root)?;
    let read = || -> (Option<u64>, Option<u64>) {
        engine.probe(ctl).map(|p| (p.max_map_count, p.file_max)).unwrap_or((None, None))
    };
    let cmd = match engine.runtime() {
        QualityRuntime::Podman { connection } if macos() => {
            // The machine is resolved by the engine probe on every call; here it is addressed by name.
            let mut cmd = Command::new(podman_program().ok_or_else(missing_podman)?);
            cmd.args(["machine", "ssh"]);
            if !connection.is_empty() { cmd.arg(connection); }
            cmd.args(["sudo", "-n", "/bin/sh", "-s"]);
            cmd
        }
        QualityRuntime::DockerDesktop if macos() => return Err("Docker Desktop does not expose a supported VM sysctl setup API to this app. Follow Docker Desktop's supported Linux VM configuration instructions to set vm.max_map_count>=524288 and fs.file-max>=131072, then Recheck.".into()),
        QualityRuntime::Podman { connection } if !connection.is_empty() => return Err("Cannot raise limits on an unverified remote Podman engine. Ask its administrator to set vm.max_map_count>=524288 and fs.file-max>=131072, then Recheck.".into()),
        QualityRuntime::Podman { .. } | QualityRuntime::DockerNative => pkexec_command(pkexec_program())?,
        _ => return Err("This runtime has no Linux VM limits to raise from this platform.".into()),
    };
    raise_with(cmd, &script, &read, ctl)
}

fn pkexec_command(pkexec: Option<PathBuf>) -> Result<Command> {
    let pkexec = pkexec.ok_or("pkexec (polkit) was not found. Run this as root yourself, then Recheck: sysctl -w vm.max_map_count=524288 fs.file-max=131072")?;
    let mut cmd = Command::new(pkexec);
    cmd.args(["/bin/sh", "-s"]);
    Ok(cmd)
}

/// Runs the privileged helper and requires the limits to hold afterwards.
fn raise_with(cmd: Command, script: &Path, read: &dyn Fn() -> (Option<u64>, Option<u64>), ctl: &Ctl) -> Result<()> {
    let out = run_bounded(cmd, Some(script), Duration::from_secs(300), ctl)?;
    if out.cancelled { return Err("Raising the VM limits was cancelled; nothing was changed.".into()); }
    match out.status.code() {
        Some(126) => return Err("Administrator approval was declined or dismissed, so the VM limits were not changed.".into()),
        Some(127) if !macos() => return Err("pkexec could not ask for administrator approval (no polkit authentication agent is running in this session). Run as root instead: sysctl -w vm.max_map_count=524288 fs.file-max=131072".into()),
        _ => {}
    }
    out.require_success("Raise and verify consented Linux VM limits")?;
    let (map, files) = read();
    if map.unwrap_or(0) < MIN_MAP_COUNT || files.unwrap_or(0) < MIN_FILE_MAX {
        return Err("Linux VM limits could not be verified after the helper. Required: vm.max_map_count>=524288, fs.file-max>=131072. No Community server was started.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering;

    // Trimmed from `podman info --format json` of rootless Podman 5.8.9 on this machine.
    const PODMAN_ROOTLESS: &str = r#"{"host":{"os":"linux","hostname":"cachyos-martin","memTotal":133622087680,"cgroupControllers":["memory","pids"],"cgroupManager":"systemd","serviceIsRemote":false,"security":{"rootless":true}},"store":{"graphRoot":"/home/martin/.local/share/containers/storage"}}"#;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("omp-native-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn script(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn parses_rootless_podman_info_and_flags_the_missing_cpu_controller() {
        let info = parse_podman_info(&serde_json::from_str(PODMAN_ROOTLESS).unwrap()).unwrap();
        assert_eq!((info.os.as_str(), info.id.as_str()), ("linux", "cachyos-martin"));
        assert_eq!(info.graph, "/home/martin/.local/share/containers/storage");
        assert_eq!(info.memory_bytes, 133_622_087_680);
        assert!(!info.remote);
        let reason = info.cpu_limits_unsupported.unwrap();
        assert!(reason.contains("memory, pids") && reason.contains("Delegate=cpu"), "{reason}");
    }

    #[test]
    fn cpu_limits_stay_enabled_when_the_controller_is_delegated_or_the_engine_is_rootful() {
        let delegated = PODMAN_ROOTLESS.replace(r#"["memory","pids"]"#, r#"["cpu","memory","pids"]"#);
        assert_eq!(parse_podman_info(&serde_json::from_str(&delegated).unwrap()).unwrap().cpu_limits_unsupported, None);
        let rootful = PODMAN_ROOTLESS.replace(r#""rootless":true"#, r#""rootless":false"#);
        assert_eq!(parse_podman_info(&serde_json::from_str(&rootful).unwrap()).unwrap().cpu_limits_unsupported, None);
        // An engine that does not report controllers at all is not assumed to lack them.
        let silent = PODMAN_ROOTLESS.replace(r#""cgroupControllers":["memory","pids"],"#, "");
        assert_eq!(parse_podman_info(&serde_json::from_str(&silent).unwrap()).unwrap().cpu_limits_unsupported, None);
        let empty = PODMAN_ROOTLESS.replace(r#"["memory","pids"]"#, "[]");
        assert!(parse_podman_info(&serde_json::from_str(&empty).unwrap()).unwrap().cpu_limits_unsupported.unwrap().contains("none"));
    }

    #[test]
    fn podman_info_without_memory_is_refused() {
        let v: Value = serde_json::from_str(r#"{"host":{"os":"linux","hostname":"h"},"store":{}}"#).unwrap();
        assert!(parse_podman_info(&v).is_err());
        assert!(parse_podman_info(&serde_json::json!({})).is_err());
    }

    #[test]
    fn parses_docker_info_and_cpu_quota_support() {
        let v = serde_json::json!({"ID":"ABCD:1234","OSType":"linux","MemTotal":8589934592u64,"OperatingSystem":"Arch Linux","CpuCfsQuota":true});
        let info = parse_docker_info(&v).unwrap();
        assert_eq!((info.os.as_str(), info.id.as_str(), info.memory_bytes), ("linux", "ABCD:1234", 8 << 30));
        assert_eq!(info.cpu_limits_unsupported, None);
        let no_quota = serde_json::json!({"ID":"x","OSType":"linux","MemTotal":1u64,"CpuCfsQuota":false});
        assert!(parse_docker_info(&no_quota).unwrap().cpu_limits_unsupported.unwrap().contains("CFS"));
        assert!(parse_docker_info(&serde_json::json!({"ID":"x"})).is_err());
    }

    #[test]
    fn limits_are_read_from_proc_sys_and_judged_against_sonars_minimums() {
        let root = scratch();
        fs::create_dir_all(root.join("vm")).unwrap();
        fs::create_dir_all(root.join("fs")).unwrap();
        assert_eq!(read_limits_at(&root), (None, None));
        fs::write(root.join("vm/max_map_count"), "262144\n").unwrap();
        fs::write(root.join("fs/file-max"), "9223372036854775807\n").unwrap();
        let (map, files) = read_limits_at(&root);
        assert_eq!((map, files), (Some(262_144), Some(9_223_372_036_854_775_807)));
        assert!(limits_low(map, files));
        assert!(!limits_low(Some(524_288), Some(131_072)));
        assert!(!limits_low(None, None), "unknown limits are not reported as low");
        assert!(limits_low(Some(524_287), Some(1 << 30)) && limits_low(Some(1 << 30), Some(131_071)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolution_is_explicit_per_platform_and_never_falls_back() {
        // Windows-only runtimes are typed errors here, whatever tools happen to be installed.
        assert!(resolve(QualityRuntime::DockerWsl { distro: "Ubuntu".into() }).unwrap_err().contains("only available on Windows"));
        if !macos() {
            assert!(resolve(QualityRuntime::DockerDesktop).unwrap_err().contains("not offered on Linux"));
        }
        assert!(resolve(QualityRuntime::Podman { connection: "--evil".into() }).is_err());
        if let Ok((runtime, _, prefix)) = resolve(QualityRuntime::Podman { connection: String::new() }) {
            assert_eq!(runtime, QualityRuntime::Podman { connection: String::new() });
            assert!(prefix.is_empty(), "an empty connection means the local default, with no --connection flag");
        }
        if let Ok((_, _, prefix)) = resolve(QualityRuntime::Podman { connection: "work-box".into() }) {
            assert_eq!(prefix, ["--connection", "work-box"]);
        }
    }

    #[test]
    fn missing_engine_messages_name_the_nix_and_homebrew_installs_and_promise_no_install() {
        if !macos() {
            assert!(missing_podman().contains("nix profile add nixpkgs#podman"));
        }
        for text in [missing_podman(), missing_docker()] {
            assert!(text.contains("never installs") || text.contains("never installs software"), "{text}");
        }
    }

    #[test]
    fn engine_notes_explain_cpu_degradation_and_low_vm_limits() {
        let runtime = QualityRuntime::Podman { connection: String::new() };
        let healthy = EngineProbe { max_map_count: Some(1 << 20), file_max: Some(1 << 20), ..EngineProbe::default() };
        assert_eq!(runtime_notes(&runtime, &healthy), None);
        let degraded = EngineProbe { cpu_limits_unsupported: Some("no cpu controller".into()), ..healthy.clone() };
        let note = runtime_notes(&runtime, &degraded).unwrap();
        assert!(note.contains("CPU limits are not applied on Podman"), "{note}");
        let low = EngineProbe { max_map_count: Some(65_530), ..healthy };
        let note = runtime_notes(&runtime, &low).unwrap();
        assert!(note.contains("65530") && (note.contains("polkit") || macos()), "{note}");
    }

    /// The raise path against a fake `pkexec` that records its argv and stdin and "applies" the change
    /// to a fake `/proc/sys`: proves the script travels on stdin to `/bin/sh -s`, and the result is verified.
    #[test]
    fn privileged_raise_pipes_the_fixed_script_to_pkexec_sh_and_verifies_the_result() {
        let dir = scratch();
        let proc = dir.join("proc");
        fs::create_dir_all(proc.join("vm")).unwrap();
        fs::create_dir_all(proc.join("fs")).unwrap();
        fs::write(proc.join("vm/max_map_count"), "65530\n").unwrap();
        fs::write(proc.join("fs/file-max"), "1000000\n").unwrap();
        let fake = dir.join("pkexec");
        script(&fake, &format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{d}/argv'\ncat > '{d}/stdin'\nprintf 524288 > '{p}/vm/max_map_count'\nexit 0\n",
            d = dir.display(), p = proc.display()));
        let script_file = dir.join("raise.sh");
        fs::write(&script_file, include_str!("../../../../bridge/code-quality/raise-vm-limits.sh")).unwrap();
        let read = || read_limits_at(&proc);
        assert!(limits_low(read().0, read().1));
        raise_with(pkexec_command(Some(fake.clone())).unwrap(), &script_file, &read, &Ctl::silent()).unwrap();
        assert_eq!(fs::read_to_string(dir.join("argv")).unwrap(), "/bin/sh\n-s\n");
        let piped = fs::read_to_string(dir.join("stdin")).unwrap();
        assert!(piped.contains("never lower") || piped.contains("Never lower"), "the fixed helper reached stdin");
        assert_eq!(read(), (Some(524_288), Some(1_000_000)), "an already higher value is retained");
        assert!(!limits_low(read().0, read().1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn privileged_raise_reports_declined_missing_agent_and_unverified_outcomes() {
        let dir = scratch();
        let script_file = dir.join("raise.sh");
        fs::write(&script_file, "exit 0\n").unwrap();
        let never_raised = || (Some(65_530u64), Some(1_000_000u64));
        for (code, expected) in [(126, "declined or dismissed"), (127, "no polkit authentication agent")] {
            let fake = dir.join(format!("pkexec-{code}"));
            script(&fake, &format!("#!/bin/sh\ncat >/dev/null\nexit {code}\n"));
            let error = raise_with(pkexec_command(Some(fake)).unwrap(), &script_file, &never_raised, &Ctl::silent()).unwrap_err();
            assert!(error.contains(expected), "{code}: {error}");
        }
        // The helper claims success but the kernel value did not change: never reported as raised.
        let liar = dir.join("pkexec-liar");
        script(&liar, "#!/bin/sh\ncat >/dev/null\nexit 0\n");
        let error = raise_with(pkexec_command(Some(liar)).unwrap(), &script_file, &never_raised, &Ctl::silent()).unwrap_err();
        assert!(error.contains("could not be verified"), "{error}");
        // A failing helper (not root) is an error with its diagnostic.
        let failing = dir.join("pkexec-fail");
        script(&failing, "#!/bin/sh\ncat >/dev/null\necho 'Raising SonarQube Linux VM limits requires root.' >&2\nexit 1\n");
        let error = raise_with(pkexec_command(Some(failing)).unwrap(), &script_file, &never_raised, &Ctl::silent()).unwrap_err();
        assert!(error.contains("requires root"), "{error}");
        // No pkexec at all: explicit manual instruction, never a silent skip.
        assert!(pkexec_command(None).err().unwrap().contains("sysctl -w vm.max_map_count=524288"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelling_the_privileged_helper_changes_nothing() {
        let dir = scratch();
        let fake = dir.join("pkexec");
        script(&fake, "#!/bin/sh\nsleep 30\n");
        let script_file = dir.join("raise.sh");
        fs::write(&script_file, "exit 0\n").unwrap();
        let ctl = Ctl::silent();
        let cancelling = ctl.clone();
        let task = thread::spawn(move || { thread::sleep(Duration::from_millis(150)); cancelling.cancel.store(true, Ordering::Relaxed); });
        let error = raise_with(pkexec_command(Some(fake)).unwrap(), &script_file, &|| (None, None), &ctl).unwrap_err();
        task.join().unwrap();
        assert!(error.contains("cancelled"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }
}
