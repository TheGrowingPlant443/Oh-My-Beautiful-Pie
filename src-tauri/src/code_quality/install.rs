//! Explicit, consent-gated setup actions. Probes never install, pull, start a VM, or mutate
//! defaults. Scripts are fixed include_str assets, materialized and digest-recorded before use.
//! Explicit shutdown requires a ready receipt and matching live machine identity;
//! both rootless/rootful stores must be empty. No workload, volume, or machine is deleted.
use super::runtime::*;
use super::types::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const MACHINE: &str = "omp-manager-quality";
const WSL_INSTALL: &str = include_str!("../../../bridge/code-quality/install-docker-wsl.sh");
const WINDOWS_FEATURES: &str = include_str!("../../../bridge/code-quality/enable-windows-features.ps1");
const VM_LIMITS: &str = include_str!("../../../bridge/code-quality/raise-vm-limits.sh");
const JSON_LIMIT: u64 = 1024 * 1024;

pub struct InstallOutcome {
    pub needs_restart: bool,
    pub message: String,
    pub podman_connection: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PodmanReceipt {
    version: u32,
    machine: String,
    provider: String,
    stage: String,
    connection: Option<String>,
    created: Option<String>,
}

fn read_receipt(root: &Path) -> Result<Option<PodmanReceipt>> {
    let receipt: Option<PodmanReceipt> = crate::dependency_security::bounded_json(&root.join("setup/podman.json"), JSON_LIMIT)?;
    if let Some(r) = &receipt {
        if r.version != 1 || r.machine != MACHINE || r.provider != "wsl"
            || !matches!(r.stage.as_str(), "creating" | "ready") {
            return Err("Invalid Podman setup receipt. Restore the app-owned setup record; an existing same-named machine will not be adopted automatically.".into());
        }
    }
    Ok(receipt)
}

fn save_receipt(root: &Path, receipt: &PodmanReceipt) -> Result<()> {
    fs::create_dir_all(root.join("setup")).map_err(|e| format!("Create runtime setup directory: {e}"))?;
    crate::dependency_security::atomic_json(&root.join("setup/podman.json"), receipt, JSON_LIMIT)
}

pub fn podman_app_connection(config_root: &Path) -> Option<String> {
    read_receipt(config_root).ok().flatten().filter(|r| r.stage == "ready").and_then(|r| r.connection)
}

pub(crate) fn podman_connections(ctl: &Ctl) -> Result<Vec<serde_json::Value>> {
    let program = podman_program().ok_or("Podman is not installed. Choose Install runtime (https://podman.io/docs/installation).")?;
    let mut cmd = Command::new(program);
    cmd.args(["system", "connection", "list", "--format", "json"]);
    let out = run_bounded(cmd, None, PROBE_TIMEOUT, ctl)?;
    out.require_success("List Podman connections")?;
    serde_json::from_slice(&out.stdout).map_err(|_| "Invalid or oversized Podman connection list.".into())
}

fn machines(ctl: &Ctl) -> Result<Vec<serde_json::Value>> {
    let mut cmd = Command::new(podman_program().ok_or("Podman is not installed.")?);
    cmd.args(["machine", "list", "--format", "json"]);
    let out = run_bounded(cmd, None, PROBE_TIMEOUT, ctl)?;
    out.require_success("List Podman machines")?;
    serde_json::from_slice(&out.stdout).map_err(|_| "Invalid Podman machine list.".into())
}

fn inspect_machine(name: &str, ctl: &Ctl) -> Result<serde_json::Value> {
    validate_selector(name, "Podman machine")?;
    let mut cmd = Command::new(podman_program().ok_or("Podman is not installed.")?);
    cmd.args(["machine", "inspect", name]);
    let out = run_bounded(cmd, None, PROBE_TIMEOUT, ctl)?;
    out.require_success("Inspect selected Podman machine")?;
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| "Invalid Podman machine inspection.")?;
    value.as_array().and_then(|a| a.first()).cloned().ok_or("Podman machine inspection was empty.".into())
}

fn connection_machine(connection: &serde_json::Value, candidates: &[serde_json::Value], ctl: &Ctl) -> Option<String> {
    let uri = reqwest::Url::parse(connection["URI"].as_str()?).ok()?;
    if uri.scheme() != "ssh" || !matches!(uri.host_str(), Some("127.0.0.1" | "localhost" | "[::1]" | "::1")) { return None; }
    let port = u64::from(uri.port()?);
    let identity = connection["Identity"].as_str()?;
    for item in candidates {
        let Some(name) = item["Name"].as_str() else { continue; };
        let Ok(machine) = inspect_machine(name, ctl) else { continue; };
        if machine.pointer("/SSHConfig/Port").and_then(|v| v.as_u64()) == Some(port)
            && machine.pointer("/SSHConfig/IdentityPath").and_then(|v| v.as_str()) == Some(identity) {
            return Some(name.into());
        }
    }
    None
}

pub(crate) fn podman_machine_for_connection(config_root: &Path, runtime: &QualityRuntime, ctl: &Ctl) -> Option<String> {
    let QualityRuntime::Podman { connection } = runtime else { return None; };
    let connection = if connection.is_empty() { podman_app_connection(config_root)? } else { connection.clone() };
    let entries = podman_connections(ctl).ok()?;
    let entry = entries.iter().find(|c| c["Name"].as_str() == Some(connection.as_str()))?;
    connection_machine(entry, &machines(ctl).ok()?, ctl)
}

pub(crate) fn podman_machine_identity(machine: &str, ctl: &Ctl) -> Option<String> {
    inspect_machine(machine, ctl).ok()?["Created"].as_str().map(str::to_owned)
}


const EXTERNAL_MACHINE_STOP: &str = "Code quality containers stopped; this Podman machine is not owned by OMP Manager and was left running.";

fn verified_stop_identity<'a>(receipt: &'a PodmanReceipt, machine: &serde_json::Value) -> Result<&'a str> {
    let created = receipt.created.as_deref().filter(|value| !value.trim().is_empty())
        .ok_or("The Podman setup receipt has no creation identity. The machine was left running; repair its setup record before retrying shutdown.")?;
    if receipt.stage != "ready" || receipt.version != 1 || receipt.machine != MACHINE || receipt.provider != "wsl" {
        return Err(EXTERNAL_MACHINE_STOP.into());
    }
    if machine["Name"].as_str() != Some(receipt.machine.as_str())
        || machine["Created"].as_str() != Some(created) {
        return Err("The app-owned Podman machine was replaced or its identity cannot be verified. It was left running; restore the matching setup receipt before retrying shutdown.".into());
    }
    Ok(created)
}

// Connection names are not authority: match the loopback SSH endpoint and identity
// recorded by the live machine inspection, then distinguish both container stores.
fn machine_store(connection: &serde_json::Value, machine: &serde_json::Value) -> Option<bool> {
    let uri = reqwest::Url::parse(connection["URI"].as_str()?).ok()?;
    let port = machine.pointer("/SSHConfig/Port")?.as_u64()?;
    let identity = machine.pointer("/SSHConfig/IdentityPath")?.as_str()?;
    if identity.is_empty() || uri.scheme() != "ssh"
        || !matches!(uri.host_str(), Some("127.0.0.1" | "localhost" | "[::1]" | "::1"))
        || uri.port().map(u64::from) != Some(port)
        || connection["Identity"].as_str() != Some(identity) {
        return None;
    }
    if uri.username() == "root" && uri.path() == "/run/podman/podman.sock" {
        Some(true)
    } else if !uri.username().is_empty() && uri.username() != "root" {
        let uid = uri.path().strip_prefix("/run/user/")?.strip_suffix("/podman/podman.sock")?;
        if !uid.is_empty() && uid.bytes().all(|value| value.is_ascii_digit()) && uid != "0" { Some(false) } else { None }
    } else { None }
}

fn shutdown_store_connections<'a>(entries: &'a [serde_json::Value], machine: &serde_json::Value, selected: &str) -> Result<[&'a str; 2]> {
    if !entries.iter().any(|entry| entry["Name"].as_str() == Some(selected) && machine_store(entry, machine).is_some()) {
        return Err(EXTERNAL_MACHINE_STOP.into());
    }
    let find = |rootful| entries.iter().find_map(|entry| {
        if machine_store(entry, machine) == Some(rootful) {
            entry["Name"].as_str().filter(|name| !name.is_empty())
        } else { None }
    });
    let rootless = find(false).ok_or("Cannot check the Podman machine's rootless container store. Repair its rootless SSH connection and retry shutdown; the VM was left running.")?;
    let rootful = find(true).ok_or("Cannot check the Podman machine's rootful container store. Repair its root SSH connection and retry shutdown; the VM was left running.")?;
    validate_selector(rootless, "Podman rootless connection")?;
    validate_selector(rootful, "Podman rootful connection")?;
    Ok([rootless, rootful])
}

fn remaining_workload_names(containers: &[ContainerInfo]) -> Result<()> {
    let mut names: Vec<&str> = containers.iter().filter(|info| info.running).map(|info| info.name.as_str()).collect();
    if names.is_empty() { return Ok(()); }
    names.sort_unstable();
    names.dedup();
    Err(format!("Code quality containers stopped; the Podman machine was left running because containers are still running: {}. Stop these workloads explicitly, then retry shutdown.", names.join(", ")))
}

#[cfg(windows)]
fn selected_stop_connection<'a>(receipt: &'a PodmanReceipt, connection: &'a str) -> Result<&'a str> {
    if !connection.is_empty() { return Ok(connection); }
    receipt.connection.as_deref().filter(|name| !name.is_empty()).ok_or_else(|| EXTERNAL_MACHINE_STOP.to_owned())
}

/// Whether the verified machine is running; a stopped machine needs no shutdown at all.
#[cfg(windows)]
fn machine_is_running(machine: &serde_json::Value) -> Result<bool> {
    match machine["State"].as_str() {
        Some("stopped") => Ok(false),
        Some("running") => Ok(true),
        _ => Err("The Podman machine is in an unknown or transitional state. It was left unchanged; wait for its current operation to finish and retry shutdown.".into()),
    }
}

#[cfg(windows)]
fn store_containers(connection: &str, config_root: &Path, ctl: &Ctl) -> Result<Vec<ContainerInfo>> {
    let mut cmd = Command::new(podman_program().ok_or("Podman is not installed.")?);
    cmd.args(["--connection", connection, "ps", "--no-trunc", "--format", "{{.ID}}"]);
    let out = run_bounded(cmd, None, PROBE_TIMEOUT, ctl)?;
    out.require_success("Check remaining Podman workloads")?;
    let engine = CliEngine::new(QualityRuntime::Podman { connection: connection.into() }, config_root)?;
    let mut containers = Vec::new();
    for id in out.text().lines().map(str::trim).filter(|id| !id.is_empty()) {
        // Disappeared containers are safe; an inspection failure is not.
        if let Some(info) = engine.inspect(id, ctl)? { containers.push(info); }
    }
    Ok(containers)
}

#[cfg(windows)]
fn remaining_store_containers(connections: [&str; 2], config_root: &Path, ctl: &Ctl) -> Result<Vec<ContainerInfo>> {
    let mut remaining = Vec::new();
    for (connection, store) in connections.into_iter().zip(["rootless", "rootful"]) {
        remaining.extend(store_containers(connection, config_root, ctl).map_err(|_| format!("Cannot verify the Podman machine's {store} container store. Check that connection's engine is reachable and retry shutdown; the VM was left running."))?);
    }
    Ok(remaining)
}

#[cfg(windows)]
fn stop_verified_machine(receipt: &PodmanReceipt, ctl: &Ctl) -> Result<()> {
    verified_stop_identity(receipt, &inspect_machine(&receipt.machine, ctl)?)?;
    if ctl.cancelled() { return Err("Podman server shutdown was cancelled; the machine was left running.".into()); }
    let mut cmd = Command::new(podman_program().ok_or("Podman is not installed.")?);
    cmd.args(["machine", "stop", &receipt.machine]);
    run_machine(cmd, Duration::from_secs(180), ctl)?.require_success("Stop app-owned Podman machine")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let current = inspect_machine(&receipt.machine, ctl)?;
        verified_stop_identity(receipt, &current)?;
        if current["State"].as_str() == Some("stopped") { return Ok(()); }
        if ctl.cancelled() || std::time::Instant::now() >= deadline {
            return Err("Podman machine stop could not be verified. The server remains suspended; Recheck and retry shutdown before starting it again.".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Stop only the receipt-proven app VM, after checking *both* stores for remaining
/// workloads. This never starts a daemon, changes defaults, or removes containers/data.
#[cfg(windows)]
pub fn stop_owned_podman_machine(config_root: &Path, runtime: &QualityRuntime, owner: &str, ctl: &Ctl) -> Result<()> {
    let QualityRuntime::Podman { connection } = runtime else { return Ok(()); };
    validate_selector(owner, "installation owner")?;
    if ctl.cancelled() { return Err("Podman server shutdown was cancelled; the machine was left unchanged.".into()); }
    let receipt = read_receipt(config_root)?.ok_or(EXTERNAL_MACHINE_STOP)?;
    if receipt.stage != "ready" { return Err(EXTERNAL_MACHINE_STOP.into()); }
    let selected = selected_stop_connection(&receipt, connection)?;
    validate_selector(selected, "Podman connection")?;
    let machine = inspect_machine(&receipt.machine, ctl)?;
    verified_stop_identity(&receipt, &machine)?;
    let entries = podman_connections(ctl)?;
    // A stopped machine needs no store probes (which would require starting it),
    // but the selected connection must still prove that it addresses this VM.
    if !entries.iter().any(|entry| entry["Name"].as_str() == Some(selected) && machine_store(entry, &machine).is_some()) {
        return Err(EXTERNAL_MACHINE_STOP.into());
    }
    if !machine_is_running(&machine)? { return Ok(()); }
    let connections = shutdown_store_connections(&entries, &machine, selected)?;
    // Every remaining container blocks shutdown, including unexpected owned roles.
    remaining_workload_names(&remaining_store_containers(connections, config_root, ctl)?)?;
    stop_verified_machine(&receipt, ctl)
}
fn winget_program() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default();
    discover_program("winget.exe", &[local.join("Microsoft/WindowsApps/winget.exe")])
}

fn powershell() -> PathBuf {
    std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| "C:/Windows".into())
        .join("System32/WindowsPowerShell/v1.0/powershell.exe")
}

fn explain_failure(error: &str) -> String {
    let upper = error.to_uppercase();
    if upper.contains("HCS_E_SERVICE_NOT_AVAILABLE") || upper.contains("0X80370114")
        || upper.contains("WSL_E_WSL_OPTIONAL_COMPONENT_REQUIRED") {
        format!("{error}\nWindows virtualization/WSL services are unavailable (HCS_E_SERVICE_NOT_AVAILABLE). Use Enable required Windows features, then restart Windows manually and Recheck. Firmware virtualization or administrator policy may also need attention.")
    } else if upper.contains("0X80370102") || upper.contains("VIRTUALIZATION") || upper.contains("HYPERVISOR") {
        format!("{error}\nVirtualization is unavailable. Enable CPU virtualization in firmware and the VirtualMachinePlatform Windows feature, or contact your administrator. OMP Manager does not change firmware/boot settings.")
    } else { error.into() }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeatureReceipt { version: u32, needs_restart: bool, boot_id: Option<String> }

fn boot_id(ctl: &Ctl) -> Option<String> {
    let mut cmd = Command::new(powershell());
    cmd.args(["-NoProfile", "-NonInteractive", "-Command",
        "(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().ToString('o')"]);
    let out = run_bounded(cmd, None, PROBE_TIMEOUT, ctl).ok()?;
    if out.status.success() && !out.text().is_empty() { Some(out.text()) } else { None }
}

fn restart_pending(root: &Path, ctl: &Ctl) -> bool {
    let receipts: Vec<FeatureReceipt> = ["windows-features.json", "runtime-restart.json"].iter()
        .filter_map(|name| crate::dependency_security::bounded_json::<FeatureReceipt>(
            &root.join("setup").join(name), JSON_LIMIT).ok().flatten())
        .filter(|r| r.version == 1 && r.needs_restart).collect();
    if receipts.is_empty() { return false; }
    let current = boot_id(ctl);
    receipts.iter().any(|r| match (&r.boot_id, &current) {
        (Some(previous), Some(current)) => previous == current,
        _ => true,
    })
}

fn windows_feature_exit(code: Option<i32>) -> Result<bool> {
    match code {
        Some(0) => Ok(false),
        Some(3010) => Ok(true),
        Some(5) => Err("Administrator elevation was cancelled or denied by policy. Required Windows features were not enabled.".into()),
        Some(6) => Err("CPU virtualization is unavailable in firmware. Enable virtualization in firmware or contact your administrator; OMP Manager will not change firmware or boot settings.".into()),
        other => Err(format!("Windows feature activation failed (servicing error or administrator policy), exit code {other:?}.")),
    }
}

/// Decides from `wsl.exe --status` text whether WSL cannot run for lack of Windows features.
/// The "Windows Subsystem for Linux" component only counts when the text is not merely the WSL1
/// note ("…optional component to use WSL1"), which also appears when WSL2 works.
fn features_missing_from(status_text: &str, exit_code: Option<i32>) -> bool {
    let lower = status_text.to_lowercase();
    let component = lower.contains("optional component");
    (component && lower.contains("virtual machine platform"))
        || (component && lower.contains("windows subsystem for linux") && !lower.contains("wsl1"))
        || (exit_code.is_some_and(|code| code != 0) && status_text.to_uppercase().contains("HCS_E_SERVICE_NOT_AVAILABLE"))
}

/// True when `wsl.exe --status` reports a missing Windows feature. Absent `wsl.exe`, a timeout or
/// any launch failure is not evidence of missing features, so those answer false.
#[cfg(windows)]
pub fn wsl_features_missing(ctl: &Ctl) -> bool {
    let Some(program) = wsl_program() else { return false; };
    let mut cmd = Command::new(program);
    cmd.arg("--status");
    let Ok(out) = run_bounded(cmd, None, PROBE_TIMEOUT, ctl) else { return false; };
    if out.timed_out || out.cancelled { return false; }
    // wsl.exe writes UTF-16LE; decode_wsl_list shares the NUL/BOM-aware decoder and falls back to UTF-8.
    let text = [decode_wsl_list(&out.stdout), decode_wsl_list(&out.stderr)].concat().join("\n");
    features_missing_from(&text, out.status.code())
}

#[cfg(windows)]
fn winget_usable(ctl: &Ctl) -> bool {
    winget_program().and_then(|program| {
        let mut cmd = Command::new(program); cmd.arg("--version");
        run_bounded(cmd, None, PROBE_TIMEOUT, ctl).ok()
    }).is_some_and(|o| o.status.success())
}

#[cfg(windows)]
fn runtime_installed(runtime: &QualityRuntime, running_distros: &[String], ctl: &Ctl) -> bool {
    match runtime {
        QualityRuntime::Podman { .. } => podman_program().is_some(),
        QualityRuntime::DockerDesktop => docker_program().is_some()
            && program_files().join("Docker/Docker/Docker Desktop.exe").is_file(),
        QualityRuntime::DockerWsl { distro } => !distro.is_empty() && running_distros.contains(distro)
            && wsl_command(distro, "/bin/sh", &["-c", "command -v docker >/dev/null 2>&1"])
                .and_then(|cmd| run_bounded(cmd, None, PROBE_TIMEOUT, ctl))
                .is_ok_and(|out| out.status.success()),
    }
}

#[cfg(windows)]
fn prerequisite_message(runtime: &QualityRuntime, probe_error: Option<String>, installed: bool, winget_available: bool, distro_error: Option<&String>) -> Option<String> {
    let message = probe_error.map(|e| explain_failure(&e));
    match runtime {
        QualityRuntime::DockerWsl { distro } if distro.is_empty() => Some(distro_error.map(|e| explain_failure(e))
            .unwrap_or_else(|| "Install and explicitly select a WSL Linux distribution. Automatic Docker installation supports Ubuntu/Debian; existing native engines on other Linux distros are supported.".into())),
        QualityRuntime::DockerWsl { .. } => message,
        _ if installed || winget_available => message,
        _ => {
            let link = if matches!(runtime, QualityRuntime::DockerDesktop) { "https://www.docker.com/products/docker-desktop/" }
                else { "https://podman.io/docs/installation" };
            let hint = format!("winget is unavailable. Install from {link}, then Recheck. No installer was downloaded.");
            Some(match message { Some(e) => format!("{e}\n{hint}"), None => hint })
        }
    }
}

#[cfg(windows)]
pub fn probe_prerequisites(config_root: &Path, selected: &QualityRuntime) -> (Vec<Prerequisite>, QualityOptions) {
    let ctl = Ctl::silent();
    let distro_result = wsl_distros(&ctl);
    let distros = distro_result.as_ref().cloned().unwrap_or_default();
    let running_distros = wsl_running_distros(&ctl).unwrap_or_default();
    let connections_result = podman_connections(&ctl);
    let connections: Vec<String> = connections_result.as_ref().map(|list| list.iter()
        .filter_map(|v| v["Name"].as_str().map(str::to_owned)).collect()).unwrap_or_default();
    let winget_available = winget_usable(&ctl);
    let options = QualityOptions { wsl_features_missing: wsl_features_missing(&ctl), wsl_distros: distros.clone(), podman_connections: connections,
        winget_available, podman_app_connection: podman_app_connection(config_root) };
    let podman = match selected {
        QualityRuntime::Podman { connection } => QualityRuntime::Podman { connection: connection.clone() },
        _ => QualityRuntime::Podman { connection: String::new() },
    };
    let distro = match selected { QualityRuntime::DockerWsl { distro } => distro.clone(),
        _ => distros.first().cloned().unwrap_or_default() };
    let wsl = QualityRuntime::DockerWsl { distro };
    let pending = restart_pending(config_root, &ctl);
    let runtimes = [podman, QualityRuntime::DockerDesktop, wsl];
    let mut result = Vec::new();
    for runtime in runtimes {
        let installed = runtime_installed(&runtime, &running_distros, &ctl);
        let probe = CliEngine::new(runtime.clone(), config_root).and_then(|engine| engine.probe(&ctl));
        let ready = probe.is_ok();
        let message = prerequisite_message(&runtime, probe.err(), installed, winget_available, distro_result.as_ref().err());
        let needs_restart = pending && !ready;
        let message = if needs_restart { Some(format!("{}\nA Windows restart is required; restart manually, then Recheck.", message.unwrap_or_default())) } else { message };
        result.push(Prerequisite { runtime, installed, ready, needs_restart, message });
    }
    (result, options)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AssetReceipt<'a> { version: u32, file: &'a str, sha256: String }

fn materialize(root: &Path, name: &str, content: &str) -> Result<PathBuf> {
    let dir = root.join("setup");
    fs::create_dir_all(&dir).map_err(|e| format!("Create setup asset directory: {e}"))?;
    let path = dir.join(name);
    let temp = dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&temp, content.as_bytes()).map_err(|e| format!("Write fixed setup helper: {e}"))?;
    // Replace only the fixed app-owned asset, never a frontend-supplied path.
    if path.exists() { fs::remove_file(&path).map_err(|e| format!("Replace setup helper: {e}"))?; }
    fs::rename(&temp, &path).map_err(|e| format!("Publish setup helper: {e}"))?;
    let receipt = AssetReceipt { version: 1, file: name, sha256: format!("{:x}", Sha256::digest(content.as_bytes())) };
    crate::dependency_security::atomic_json(&dir.join(format!("{name}.sha256.json")), &receipt, JSON_LIMIT)?;
    // Re-read before use to refuse a truncated/replaced helper.
    let bytes = fs::read(&path).map_err(|e| format!("Read fixed setup helper: {e}"))?;
    if Sha256::digest(&bytes) != Sha256::digest(content.as_bytes()) { return Err("Setup helper digest mismatch; no helper was executed.".into()); }
    Ok(path)
}

const PODMAN_PACKAGE: &str = "RedHat.Podman";

/// winget arguments and the progress line shown before it runs. Podman installs silently (Windows
/// still raises its own administrator prompt); Docker Desktop stays interactive because its license
/// must be accepted by the user, never automatically.
fn winget_command(package: &str) -> (Vec<&str>, &'static str) {
    if package == PODMAN_PACKAGE {
        (vec!["install", "--exact", "--id", package, "--source", "winget", "--silent",
            "--accept-package-agreements", "--accept-source-agreements"],
            "Installing Podman (Windows will ask for administrator approval).")
    } else {
        (vec!["install", "--exact", "--id", package, "--source", "winget", "--interactive"],
            "Awaiting the interactive runtime installer. Review its license/elevation prompts; no license is automatically accepted.")
    }
}

/// Fixed PowerShell that runs winget elevated (a hidden app process cannot raise winget's own UAC
/// prompt; `Start-Process -Verb RunAs` does) and exits with winget's exit code. Only constants and the
/// discovered winget path are interpolated, never frontend input.
fn elevated_winget_script(program: &Path, args: &[&str], hidden: bool) -> String {
    let quote = |text: &str| format!("'{}'", text.replace('\'', "''"));
    let list = args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(",");
    format!(
        "try {{ $p = Start-Process -FilePath {} -ArgumentList @({list}) -Verb RunAs -Wait -PassThru{}; exit $p.ExitCode }} catch {{ [Console]::Error.WriteLine($_.Exception.Message); exit 5 }}",
        quote(&program.to_string_lossy()),
        if hidden { " -WindowStyle Hidden" } else { "" }
    )
}

fn run_winget(package: &str, ctl: &Ctl) -> Result<bool> {
    let program = winget_program().ok_or_else(|| {
        let link = if package == "Docker.DockerDesktop" { "https://www.docker.com/products/docker-desktop/" } else { "https://podman.io/docs/installation" };
        format!("winget is unavailable. Use the official installer at {link}, then Recheck. No unverified mirror will be used.")
    })?;
    let (args, progress) = winget_command(package);
    let mut cmd = Command::new(powershell());
    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"]).arg(elevated_winget_script(&program, &args, package == PODMAN_PACKAGE));
    ctl.say(progress);
    let out = run_installer(cmd, None, ctl)?;
    if out.cancelled { return Err("Runtime installation was cancelled; the actual installer has now exited. Recheck installed runtime state before retrying.".into()); }
    match out.status.code().map(|c| c as u32) {
        Some(3010) => return Ok(true),
        Some(5) => return Err("Administrator approval was declined or never shown, so nothing was installed. Press Continue setup and approve the Windows administrator prompt.".into()),
        _ => {}
    }
    out.require_success("Runtime installation")?;
    Ok(false)
}

fn prepare_podman_receipt(root: &Path, receipt: Option<PodmanReceipt>, existing: bool) -> Result<PodmanReceipt> {
    if existing && receipt.is_none() {
        return Err("A Podman machine named omp-manager-quality already exists without this app's setup receipt. It will not be adopted or deleted. Select its existing connection explicitly, or resolve the name conflict manually.".into());
    }
    if let Some(receipt) = receipt { return Ok(receipt); }
    let receipt = PodmanReceipt { version: 1, machine: MACHINE.into(), provider: "wsl".into(),
        stage: "creating".into(), connection: None, created: None };
    // Ownership intent precedes creation; a crash after init remains safely recoverable.
    save_receipt(root, &receipt)?;
    Ok(receipt)
}

fn record_podman_setup_identity(root: &Path, receipt: &mut PodmanReceipt, inspected: &serde_json::Value) -> Result<()> {
    if let Some(previous) = &receipt.created {
        if inspected["Created"].as_str() != Some(previous.as_str()) {
            return Err("The app-owned Podman machine was replaced since setup. Restore its receipt or select the replacement connection explicitly; it will not be adopted automatically.".into());
        }
    }
    receipt.created = Some(inspected["Created"].as_str().ok_or("Podman omitted the app-machine creation identity; setup ownership cannot be recorded safely.")?.to_owned());
    save_receipt(root, receipt)
}

fn setup_rootless_connection<'a>(value: &'a serde_json::Value, port: u64, identity: &str) -> Option<&'a str> {
    let uri = reqwest::Url::parse(value["URI"].as_str()?).ok()?;
    let rootless = uri.path().starts_with("/run/user/") && uri.username() != "root";
    if rootless && uri.port().map(u64::from) == Some(port)
        && value["Identity"].as_str() == Some(identity)
        && matches!(uri.host_str(), Some("127.0.0.1" | "localhost" | "[::1]" | "::1")) {
        value["Name"].as_str()
    } else { None }
}

fn setup_podman(root: &Path, ctl: &Ctl) -> Result<String> {
    let receipt = read_receipt(root)?;
    let existing = machines(ctl)?.iter().any(|v| v["Name"].as_str() == Some(MACHINE));
    let mut receipt = prepare_podman_receipt(root, receipt, existing)?;
    if !existing {
        let mut cmd = Command::new(podman_program().ok_or("Podman was not found after installation. Recheck or repair the official installation.")?);
        // Podman 5.8 has no --provider/--update-connection flags: the provider is chosen by this variable.
        cmd.env("CONTAINERS_MACHINE_PROVIDER", "wsl");
        cmd.args(["machine", "init", "--rootful=false", MACHINE]);
        crate::code_quality::runtime::run_machine(cmd, Duration::from_secs(30 * 60), ctl)?.require_success("Initialize app-owned WSL Podman machine")?;
    }
    if !machines(ctl)?.iter().any(|v| v["Name"].as_str() == Some(MACHINE) && v["VMType"].as_str() == Some("wsl")) {
        return Err("The app-owned Podman machine is not verified as WSL-backed. It will not be adopted or reconfigured; choose an existing connection explicitly.".into());
    }
    let inspected = inspect_machine(MACHINE, ctl)?;
    record_podman_setup_identity(root, &mut receipt, &inspected)?;
    if inspected["State"].as_str() != Some("running") {
        let mut cmd = Command::new(podman_program().ok_or("Podman is not installed.")?);
        cmd.args(["machine", "start", MACHINE]);
        crate::code_quality::runtime::run_machine(cmd, Duration::from_secs(10 * 60), ctl)?.require_success("Start app-owned Podman machine")?;
    }
    let inspected = inspect_machine(MACHINE, ctl)?;
    let port = inspected.pointer("/SSHConfig/Port").and_then(|v| v.as_u64()).ok_or("Podman machine omitted its SSH port.")?;
    let identity = inspected.pointer("/SSHConfig/IdentityPath").and_then(|v| v.as_str()).ok_or("Podman machine omitted its SSH identity path.")?;
    let connections = podman_connections(ctl)?;
    let connection = connections.iter().find_map(|value| setup_rootless_connection(value, port, identity))
        .map(str::to_owned).ok_or("The app-owned Podman machine has no verified rootless connection. Repair Podman machine setup, then retry; no ambient/default connection will be used.")?;
    receipt.connection = Some(connection.clone());
    receipt.stage = "ready".into();
    save_receipt(root, &receipt)?;
    CliEngine::new(QualityRuntime::Podman { connection: connection.clone() }, root)?.probe(ctl)?;
    Ok(connection)
}

#[cfg(windows)]
fn install_docker_desktop(runtime: &QualityRuntime, config_root: &Path, ctl: &Ctl) -> Result<bool> {
    let installed = docker_program().is_some() && program_files().join("Docker/Docker/Docker Desktop.exe").is_file();
    let restart = if installed { false } else { run_winget("Docker.DockerDesktop", ctl)? };
    if !restart { CliEngine::new(runtime.clone(), config_root)?.ensure_started(ctl).map_err(|e| explain_failure(&e))?; }
    Ok(restart)
}

/// Returns whether a restart is pending and the Podman connection that was set up or verified.
#[cfg(windows)]
fn install_podman(runtime: &QualityRuntime, selected: &str, config_root: &Path, ctl: &Ctl) -> Result<(bool, Option<String>)> {
    if podman_program().is_none() && run_winget(PODMAN_PACKAGE, ctl)? { return Ok((true, None)); }
    if podman_program().is_none() { return Err("Podman installer exited but podman.exe was not found. Repair the official installation and Recheck.".into()); }
    if selected.is_empty() { return Ok((false, Some(setup_podman(config_root, ctl).map_err(|e| explain_failure(&e))?))); }
    validate_selector(selected, "Podman connection")?;
    if !podman_connections(ctl)?.iter().any(|v| v["Name"].as_str() == Some(selected)) {
        return Err("Selected Podman connection no longer exists; Recheck and explicitly choose an existing connection.".into());
    }
    CliEngine::new(runtime.clone(), config_root)?.ensure_started(ctl).map_err(|e| explain_failure(&e))?;
    Ok((false, Some(selected.to_owned())))
}

#[cfg(windows)]
fn install_docker_wsl(runtime: &QualityRuntime, distro: &str, config_root: &Path, ctl: &Ctl) -> Result<()> {
    validate_selector(distro, "WSL distribution")?;
    if !wsl_distros(ctl)?.iter().any(|name| name == distro) { return Err("The explicitly selected WSL distribution is not registered. Recheck and select an installed distro.".into()); }
    // Already usable engines on any distro are supported without trying apt.
    let engine = CliEngine::new(runtime.clone(), config_root)?;
    if engine.ensure_started(ctl).is_err() {
        if ctl.cancelled() { return Err("Runtime setup cancelled before Docker installation.".into()); }
        let script = materialize(config_root, "install-docker-wsl.sh", WSL_INSTALL)?;
        ctl.say("Installing Docker Engine as root in the selected Ubuntu/Debian distro using Docker's signed official apt repository. Conflicting packages are refused, never removed.");
        let cmd = wsl_command(distro, "/bin/sh", &["-s"])?;
        run_installer(cmd, Some(&script), ctl)?.require_success("Install native Docker Engine in WSL")
            .map_err(|e| explain_failure(&e))?;
    }
    engine.ensure_started(ctl).map_err(|e| explain_failure(&e))
}

#[cfg(windows)]
fn record_runtime_restart(config_root: &Path, ctl: &Ctl) -> Result<()> {
    fs::create_dir_all(config_root.join("setup")).map_err(|e| format!("Record runtime restart requirement: {e}"))?;
    let receipt = FeatureReceipt { version: 1, needs_restart: true, boot_id: boot_id(ctl) };
    crate::dependency_security::atomic_json(&config_root.join("setup/runtime-restart.json"), &receipt, JSON_LIMIT)
}

#[cfg(windows)]
pub fn install_runtime(config_root: &Path, runtime: &QualityRuntime, ctl: &Ctl) -> Result<InstallOutcome> {
    if ctl.cancelled() { return Err("Runtime installation cancelled before it started.".into()); }
    let (restart, connection) = match runtime {
        QualityRuntime::DockerDesktop => (install_docker_desktop(runtime, config_root, ctl)?, None),
        QualityRuntime::Podman { connection: selected } => install_podman(runtime, selected, config_root, ctl)?,
        QualityRuntime::DockerWsl { distro } => { install_docker_wsl(runtime, distro, config_root, ctl)?; (false, None) }
    };
    if restart { record_runtime_restart(config_root, ctl)?; }
    Ok(InstallOutcome { needs_restart: restart, message: if restart {
        "Runtime installation requires a Windows restart. Restart Windows manually, then Recheck; OMP Manager did not reboot or start another installer.".into()
    } else { "Selected runtime is installed and its Linux daemon is ready. No global engine context/default was changed.".into() }, podman_connection: connection })
}

#[cfg(windows)]
pub fn enable_windows_features(config_root: &Path, ctl: &Ctl) -> Result<InstallOutcome> {
    let script = materialize(config_root, "enable-windows-features.ps1", WINDOWS_FEATURES)?;
    let mut cmd = Command::new(powershell());
    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]).arg(&script);
    ctl.say("Awaiting administrator consent to enable Windows Subsystem for Linux and VirtualMachinePlatform with -NoRestart. No reboot, distro termination, firmware or boot changes will be performed.");
    let out = run_installer(cmd, None, ctl)?;
    if out.cancelled { return Err("Windows feature setup cancelled; the helper has exited. Recheck actual feature state before retrying.".into()); }
    let restart = windows_feature_exit(out.status.code()).map_err(|message| format!("{message} {}", out.diagnostic()))?;
    let receipt = FeatureReceipt { version: 1, needs_restart: restart, boot_id: boot_id(ctl) };
    crate::dependency_security::atomic_json(&config_root.join("setup/windows-features.json"), &receipt, JSON_LIMIT)?;
    Ok(InstallOutcome { needs_restart: restart, podman_connection: None, message: if restart {
        "Required Windows features are enabled; restart Windows manually before installing/starting the Linux runtime. OMP Manager did not reboot.".into()
    } else { "Required Windows features are enabled. Recheck the selected runtime; firmware or administrator restrictions can still prevent virtualization.".into() } })
}

/// Integration can use its actual (including harness-isolated) config root directly.
#[cfg(windows)]
pub fn raise_vm_limits_at(engine: &dyn Engine, config_root: &Path, ctl: &Ctl) -> Result<()> {
    let script = materialize(config_root, "raise-vm-limits.sh", VM_LIMITS)?;
    let cmd = match engine.runtime() {
        QualityRuntime::DockerWsl { distro } => wsl_command(distro, "/bin/sh", &["-s"] )?,
        QualityRuntime::Podman { .. } => {
            let machine = podman_machine_for_connection(config_root, engine.runtime(), ctl)
                .ok_or("Cannot raise limits on an unverified remote Podman VM. Ask its administrator to set vm.max_map_count>=524288 and fs.file-max>=131072, then Recheck.")?;
            let mut cmd = Command::new(podman_program().ok_or("Podman is not installed.")?);
            cmd.args(["machine", "ssh", &machine, "sudo", "-n", "/bin/sh", "-s"]);
            cmd
        }
        QualityRuntime::DockerDesktop => return Err("Docker Desktop does not expose a supported VM sysctl setup API to this app. Follow Docker Desktop's supported Linux VM configuration instructions to set vm.max_map_count>=524288 and fs.file-max>=131072, then Recheck. OMP Manager will not use a privileged container or disable Elasticsearch bootstrap checks.".into()),
    };
    run_bounded(cmd, Some(&script), Duration::from_secs(90), ctl)?.require_success("Raise and verify consented Linux VM limits")?;
    let probe = engine.probe(ctl)?;
    if probe.max_map_count.unwrap_or(0) < 524288 || probe.file_max.unwrap_or(0) < 131072 {
        return Err("Linux VM limits could not be verified after the helper. Required: vm.max_map_count>=524288, fs.file-max>=131072. No Community server was started.".into());
    }
    Ok(())
}

#[cfg(unix)]
pub fn probe_prerequisites(config_root: &Path, selected: &QualityRuntime) -> (Vec<Prerequisite>, QualityOptions) {
    native::probe_prerequisites(config_root, selected)
}

#[cfg(unix)]
pub fn wsl_features_missing(_: &Ctl) -> bool { false }

#[cfg(unix)]
pub fn install_runtime(config_root: &Path, runtime: &QualityRuntime, ctl: &Ctl) -> Result<InstallOutcome> {
    let engine = CliEngine::new(runtime.clone(), config_root)?;
    engine.ensure_started(ctl)?;
    Ok(InstallOutcome { needs_restart: false, podman_connection: None,
        message: "Selected container engine is ready. No software was installed or machine created.".into() })
}

#[cfg(unix)]
pub fn enable_windows_features(_: &Path, _: &Ctl) -> Result<InstallOutcome> {
    Err("Windows feature setup is not applicable on this platform.".into())
}

#[cfg(unix)]
pub fn stop_owned_podman_machine(_: &Path, _: &QualityRuntime, _: &str, _: &Ctl) -> Result<()> {
    // Unix engines and macOS VMs belong to the user, not to the app. Only app-owned containers are stopped.
    Ok(())
}

#[cfg(unix)]
pub(crate) fn materialize_vm_limits_script(root: &Path) -> Result<PathBuf> {
    materialize(root, "raise-vm-limits.sh", VM_LIMITS)
}

#[cfg(unix)]
pub fn raise_vm_limits_at(engine: &dyn Engine, config_root: &Path, ctl: &Ctl) -> Result<()> {
    native::raise_vm_limits(engine, config_root, ctl)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop_receipt() -> PodmanReceipt {
        PodmanReceipt { version: 1, machine: MACHINE.into(), provider: "wsl".into(),
            stage: "ready".into(), connection: Some("quality-user".into()), created: Some("2026-10-17T12:00:00Z".into()) }
    }

    fn stop_machine() -> serde_json::Value {
        serde_json::json!({ "Name": MACHINE, "Created": "2026-10-17T12:00:00Z", "State": "running",
            "SSHConfig": { "Port": 54321, "IdentityPath": "C:/quality/identity" } })
    }

    fn stop_connections() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({ "Name": "quality-user", "URI": "ssh://user@127.0.0.1:54321/run/user/1000/podman/podman.sock", "Identity": "C:/quality/identity" }),
            serde_json::json!({ "Name": "quality-root", "URI": "ssh://root@127.0.0.1:54321/run/podman/podman.sock", "Identity": "C:/quality/identity" }),
        ]
    }

    #[test]
    fn machine_stop_requires_ready_receipt_and_exact_creation_identity() {
        let mut receipt = stop_receipt();
        let machine = stop_machine();
        assert!(verified_stop_identity(&receipt, &machine).is_ok());
        receipt.stage = "creating".into();
        assert_eq!(verified_stop_identity(&receipt, &machine).unwrap_err(), EXTERNAL_MACHINE_STOP);
        receipt.stage = "ready".into();
        for identity in [None, Some("".into()), Some("   ".into()), Some("replacement".into())] {
            receipt.created = identity;
            assert!(verified_stop_identity(&receipt, &machine).is_err());
        }
        receipt = stop_receipt();
        let mut replaced = machine;
        replaced["Name"] = "different-machine".into();
        assert!(verified_stop_identity(&receipt, &replaced).is_err());
    }

    #[test]
    fn machine_shutdown_checks_both_stores_and_selected_connection() {
        let machine = stop_machine();
        let mut entries = stop_connections();
        assert_eq!(shutdown_store_connections(&entries, &machine, "quality-user").unwrap(), ["quality-user", "quality-root"]);
        assert_eq!(shutdown_store_connections(&entries, &machine, "quality-root").unwrap(), ["quality-user", "quality-root"]);
        assert_eq!(shutdown_store_connections(&entries, &machine, "external").unwrap_err(), EXTERNAL_MACHINE_STOP);
        entries.pop();
        assert!(shutdown_store_connections(&entries, &machine, "quality-user").unwrap_err().contains("rootful"));
        let entries = vec![stop_connections().pop().unwrap()];
        assert!(shutdown_store_connections(&entries, &machine, "quality-root").unwrap_err().contains("rootless"));
    }

    #[test]
    fn machine_shutdown_rejects_unproven_connection_endpoints() {
        let machine = stop_machine();
        let original = stop_connections().remove(0);
        for uri in [
            "ssh://user@remote.example:54321/run/user/1000/podman/podman.sock",
            "ssh://user@127.0.0.1:12345/run/user/1000/podman/podman.sock",
            "http://127.0.0.1:54321/run/user/1000/podman/podman.sock",
            "ssh://root@127.0.0.1:54321/run/user/1000/podman/podman.sock",
            "ssh://user@127.0.0.1:54321/run/user/0/podman/podman.sock",
            "ssh://user@127.0.0.1:54321/run/user/invalid/podman/podman.sock",
        ] {
            let mut entry = original.clone();
            entry["URI"] = uri.into();
            assert_eq!(machine_store(&entry, &machine), None);
        }
        let mut entry = original;
        entry["Identity"] = "another-key".into();
        assert_eq!(machine_store(&entry, &machine), None);
    }

    #[test]
    fn any_remaining_workload_blocks_vm_shutdown_without_exposing_metadata() {
        let mut foreign = ContainerInfo { name: "foreign-sentinel".into(), running: true, ..Default::default() };
        foreign.labels.insert("secret".into(), "never-include-this".into());
        let owned = ContainerInfo { name: "unexpected-quality-role".into(), running: true,
            labels: std::collections::HashMap::from([(OWNER_LABEL.into(), "quality-owner".into())]), ..Default::default() };
        let error = remaining_workload_names(&[foreign.clone(), owned]).unwrap_err();
        assert!(error.contains("foreign-sentinel") && error.contains("unexpected-quality-role"));
        assert!(!error.contains("never-include-this") && !error.contains("quality-owner"));
        foreign.running = false;
        assert!(remaining_workload_names(&[foreign]).is_ok());
        assert!(remaining_workload_names(&[]).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn shutdown_without_receipt_never_discovers_or_adopts_a_machine() {
        let root = std::env::temp_dir().join(format!("omp-quality-stop-{}", uuid::Uuid::new_v4()));
        let runtime = QualityRuntime::Podman { connection: "external".into() };
        assert_eq!(stop_owned_podman_machine(&root, &runtime, "quality-owner", &Ctl::silent()).unwrap_err(), EXTERNAL_MACHINE_STOP);
        assert!(!root.exists(), "shutdown must not create a receipt or config directory");
        assert!(stop_owned_podman_machine(&root, &QualityRuntime::DockerDesktop, "quality-owner", &Ctl::silent()).is_ok());
    }
    #[test]
    fn bundled_helpers_have_fixed_safe_policies() {
        assert!(WSL_INSTALL.contains("docker-ce docker-ce-cli containerd.io"));
        assert!(WSL_INSTALL.contains("Signed-By: /etc/apt/keyrings/docker.asc"));
        assert!(WSL_INSTALL.contains("Conflicting package"));
        assert!(WINDOWS_FEATURES.contains("-Verb RunAs -Wait"));
        assert!(WINDOWS_FEATURES.contains("-NoRestart"));
        assert!(!WINDOWS_FEATURES.contains("Restart-Computer"));
        assert!(!WSL_INSTALL.contains("apt-get remove"));
        assert!(VM_LIMITS.contains("-ge 524288"));
        assert!(VM_LIMITS.contains("-ge 131072"));
    }
    #[test]
    fn materialized_helpers_and_digest_receipts_are_consistent() {
        let root = std::env::temp_dir().join(format!("omp-quality-install-{}", uuid::Uuid::new_v4()));
        let path = materialize(&root, "raise-vm-limits.sh", VM_LIMITS).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), VM_LIMITS);
        let receipt: serde_json::Value = serde_json::from_slice(&fs::read(root.join("setup/raise-vm-limits.sh.sha256.json")).unwrap()).unwrap();
        assert_eq!(receipt["sha256"].as_str().unwrap(), format!("{:x}", Sha256::digest(VM_LIMITS.as_bytes())));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn empty_connection_never_adopts_ambient_default() {
        let root = std::env::temp_dir().join(format!("omp-quality-receipt-{}", uuid::Uuid::new_v4()));
        assert!(podman_app_connection(&root).is_none());
        save_receipt(&root, &PodmanReceipt { version: 1, machine: MACHINE.into(), provider: "wsl".into(),
            stage: "creating".into(), connection: Some("unready".into()), created: None }).unwrap();
        assert!(podman_app_connection(&root).is_none());
        save_receipt(&root, &PodmanReceipt { version: 1, machine: MACHINE.into(), provider: "wsl".into(),
            stage: "ready".into(), connection: Some("owned-rootless".into()), created: None }).unwrap();
        assert_eq!(podman_app_connection(&root).as_deref(), Some("owned-rootless"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn virtualization_errors_are_actionable_and_distinct() {
        assert!(explain_failure("HCS_E_SERVICE_NOT_AVAILABLE").contains("Enable required Windows features"));
        assert!(explain_failure("0x80370102").contains("firmware"));
        assert_eq!(explain_failure("fixture failure"), "fixture failure");
    }
    #[test]
    fn feature_outcomes_distinguish_restart_elevation_and_firmware() {
        assert!(!windows_feature_exit(Some(0)).unwrap());
        assert!(windows_feature_exit(Some(3010)).unwrap());
        assert!(windows_feature_exit(Some(5)).unwrap_err().contains("cancelled or denied"));
        assert!(windows_feature_exit(Some(6)).unwrap_err().contains("firmware"));
        assert!(windows_feature_exit(Some(7)).unwrap_err().contains("servicing error"));
    }
    // Captured from `wsl.exe --status` (exit code 0) on a machine lacking Virtual Machine Platform.
    const WSL_STATUS_FEATURES_MISSING: &str = "Default Distribution: Ubuntu\r\nDefault Version: 2\r\nWSL1 is not supported with your current machine configuration.\r\nPlease enable the \"Windows Subsystem for Linux\" optional component to use WSL1.\r\nWSL2 is not supported with your current machine configuration.\r\nPlease enable the \"Virtual Machine Platform\" optional component and ensure virtualization is enabled in the BIOS.\r\nEnable \"Virtual Machine Platform\" by running: wsl.exe --install --no-distribution\r\nFor information please visit https://aka.ms/enablevirtualization\r\n";
    #[test]
    fn wsl_status_decides_missing_windows_features() {
        assert!(features_missing_from(WSL_STATUS_FEATURES_MISSING, Some(0)));
        assert!(features_missing_from(WSL_STATUS_FEATURES_MISSING, None));
        let utf16: Vec<u8> = std::iter::once(0xfeffu16).chain(WSL_STATUS_FEATURES_MISSING.encode_utf16())
            .flat_map(u16::to_le_bytes).collect();
        assert!(features_missing_from(&decode_wsl_list(&utf16).join("\n"), Some(0)), "UTF-16LE output decodes before matching");
        // Healthy WSL, and healthy WSL2 that only prints the WSL1 note.
        assert!(!features_missing_from("Default Distribution: Ubuntu\r\nDefault Version: 2", Some(0)));
        assert!(!features_missing_from("Default Distribution: Ubuntu\r\nDefault Version: 2\r\nWSL1 is not supported with your current machine configuration.\r\nPlease enable the \"Windows Subsystem for Linux\" optional component to use WSL1.", Some(0)));
        // Older inbox wsl.exe wording for a disabled WSL component.
        assert!(features_missing_from("The Windows Subsystem for Linux optional component is not enabled. Please enable it and try again.", Some(1)));
        // Service failure counts only with a failing exit code.
        assert!(features_missing_from("Wsl/Service/CreateInstance/CreateVm/HCS/HCS_E_SERVICE_NOT_AVAILABLE", Some(-1)));
        assert!(!features_missing_from("Wsl/Service/CreateInstance/CreateVm/HCS/HCS_E_SERVICE_NOT_AVAILABLE", Some(0)));
        assert!(!features_missing_from("", Some(1)));
    }
    #[test]
    fn winget_runs_elevated_with_quoted_constants_and_returns_its_exit_code() {
        let script = elevated_winget_script(Path::new("C:/Users/o'brien/winget.exe"), &["install", "--id", "RedHat.Podman"], true);
        assert!(script.contains("-Verb RunAs -Wait -PassThru -WindowStyle Hidden"));
        assert!(script.contains("'C:/Users/o''brien/winget.exe'") && script.contains("@('install','--id','RedHat.Podman')"));
        assert!(script.contains("exit $p.ExitCode") && script.contains("exit 5"));
        assert!(!elevated_winget_script(Path::new("w.exe"), &["x"], false).contains("Hidden"));
    }

    #[test]
    fn podman_installs_silently_and_docker_desktop_stays_interactive() {
        let (podman, progress) = winget_command("RedHat.Podman");
        assert_eq!(podman, ["install", "--exact", "--id", "RedHat.Podman", "--source", "winget", "--silent",
            "--accept-package-agreements", "--accept-source-agreements"]);
        assert_eq!(progress, "Installing Podman (Windows will ask for administrator approval).");
        let (docker, progress) = winget_command("Docker.DockerDesktop");
        assert_eq!(docker, ["install", "--exact", "--id", "Docker.DockerDesktop", "--source", "winget", "--interactive"]);
        assert!(!docker.contains(&"--silent") && !docker.contains(&"--accept-package-agreements"));
        assert!(progress.contains("no license is automatically accepted"));
    }
}
