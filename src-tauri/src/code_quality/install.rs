//! Explicit, consent-gated setup actions. Probes never install, pull, start a VM, or mutate
//! defaults. Scripts are fixed include_str assets, materialized and digest-recorded before use.
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

pub fn probe_prerequisites(config_root: &Path, selected: &QualityRuntime) -> (Vec<Prerequisite>, QualityOptions) {
    let ctl = Ctl::silent();
    let distro_result = wsl_distros(&ctl);
    let distros = distro_result.as_ref().cloned().unwrap_or_default();
    let running_distros = wsl_running_distros(&ctl).unwrap_or_default();
    let connections_result = podman_connections(&ctl);
    let connections: Vec<String> = connections_result.as_ref().map(|list| list.iter()
        .filter_map(|v| v["Name"].as_str().map(str::to_owned)).collect()).unwrap_or_default();
    let winget_available = winget_program().and_then(|program| {
        let mut cmd = Command::new(program); cmd.arg("--version");
        run_bounded(cmd, None, PROBE_TIMEOUT, &ctl).ok()
    }).map(|o| o.status.success()).unwrap_or(false);
    let options = QualityOptions { wsl_distros: distros.clone(), podman_connections: connections,
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
        let installed = match &runtime {
            QualityRuntime::Podman { .. } => podman_program().is_some(),
            QualityRuntime::DockerDesktop => docker_program().is_some()
                && program_files().join("Docker/Docker/Docker Desktop.exe").is_file(),
            QualityRuntime::DockerWsl { distro } => {
                if distro.is_empty() || !running_distros.contains(distro) { false } else {
                    wsl_command(distro, "/bin/sh", &["-c", "command -v docker >/dev/null 2>&1"])
                        .and_then(|cmd| run_bounded(cmd, None, PROBE_TIMEOUT, &ctl))
                        .map(|out| out.status.success()).unwrap_or(false)
                }
            }
        };
        let probe = CliEngine::new(runtime.clone(), config_root).and_then(|engine| engine.probe(&ctl));
        let ready = probe.is_ok();
        let mut message = probe.err().map(|e| explain_failure(&e));
        if let QualityRuntime::DockerWsl { distro } = &runtime {
            if distro.is_empty() {
                message = Some(distro_result.as_ref().err().map(|e| explain_failure(e))
                    .unwrap_or_else(|| "Install and explicitly select a WSL Linux distribution. Automatic Docker installation supports Ubuntu/Debian; existing native engines on other Linux distros are supported.".into()));
            }
        }
        if !installed && !winget_available && !matches!(runtime, QualityRuntime::DockerWsl { .. }) {
            let link = if matches!(runtime, QualityRuntime::DockerDesktop) { "https://www.docker.com/products/docker-desktop/" }
                else { "https://podman.io/docs/installation" };
            let hint = format!("winget is unavailable. Install from {link}, then Recheck. No installer was downloaded.");
            message = Some(match message { Some(e) => format!("{e}\n{hint}"), None => hint });
        }
        if pending && !ready { message = Some(format!("{}\nA Windows restart is required; restart manually, then Recheck.", message.unwrap_or_default())); }
        result.push(Prerequisite { runtime, installed, ready, needs_restart: pending && !ready, message });
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

fn run_winget(package: &str, ctl: &Ctl) -> Result<bool> {
    let program = winget_program().ok_or_else(|| {
        let link = if package == "Docker.DockerDesktop" { "https://www.docker.com/products/docker-desktop/" } else { "https://podman.io/docs/installation" };
        format!("winget is unavailable. Use the official installer at {link}, then Recheck. No unverified mirror will be used.")
    })?;
    let mut cmd = Command::new(program);
    cmd.args(["install", "--exact", "--id", package, "--source", "winget", "--interactive"]);
    ctl.say("Awaiting the interactive runtime installer. Review its license/elevation prompts; no license is automatically accepted.");
    let out = run_installer(cmd, None, ctl)?;
    if out.cancelled { return Err("Runtime installation was cancelled; the actual installer has now exited. Recheck installed runtime state before retrying.".into()); }
    if out.status.code() == Some(3010) { return Ok(true); }
    out.require_success("Runtime installation (elevation cancellation is an error)")?;
    Ok(false)
}

fn setup_podman(root: &Path, ctl: &Ctl) -> Result<String> {
    let mut receipt = read_receipt(root)?;
    let existing = machines(ctl)?.iter().any(|v| v["Name"].as_str() == Some(MACHINE));
    if existing && receipt.is_none() {
        return Err("A Podman machine named omp-manager-quality already exists without this app's setup receipt. It will not be adopted or deleted. Select its existing connection explicitly, or resolve the name conflict manually.".into());
    }
    if receipt.is_none() {
        let planned = PodmanReceipt { version: 1, machine: MACHINE.into(), provider: "wsl".into(),
            stage: "creating".into(), connection: None, created: None };
        // Ownership intent precedes creation; a crash after init remains safely recoverable.
        save_receipt(root, &planned)?;
        receipt = Some(planned);
    }
    let mut receipt = receipt.ok_or("Missing Podman setup receipt.")?;
    if !existing {
        let mut cmd = Command::new(podman_program().ok_or("Podman was not found after installation. Recheck or repair the official installation.")?);
        cmd.args(["machine", "init", "--provider", "wsl", "--rootful=false", "--update-connection=false", MACHINE]);
        run_installer(cmd, None, ctl)?.require_success("Initialize app-owned WSL Podman machine")?;
    }
    if !machines(ctl)?.iter().any(|v| v["Name"].as_str() == Some(MACHINE) && v["VMType"].as_str() == Some("wsl")) {
        return Err("The app-owned Podman machine is not verified as WSL-backed. It will not be adopted or reconfigured; choose an existing connection explicitly.".into());
    }
    let inspected = inspect_machine(MACHINE, ctl)?;
    if let Some(previous) = &receipt.created {
        if inspected["Created"].as_str() != Some(previous.as_str()) {
            return Err("The app-owned Podman machine was replaced since setup. Restore its receipt or select the replacement connection explicitly; it will not be adopted automatically.".into());
        }
    }
    let created = Some(inspected["Created"].as_str().ok_or("Podman omitted the app-machine creation identity; setup ownership cannot be recorded safely.")?.to_owned());
    receipt.created = created;
    save_receipt(root, &receipt)?;
    if inspected["State"].as_str() != Some("running") {
        let mut cmd = Command::new(podman_program().ok_or("Podman is not installed.")?);
        cmd.args(["machine", "start", "--update-connection=false", MACHINE]);
        run_installer(cmd, None, ctl)?.require_success("Start app-owned Podman machine")?;
    }
    let inspected = inspect_machine(MACHINE, ctl)?;
    let port = inspected.pointer("/SSHConfig/Port").and_then(|v| v.as_u64()).ok_or("Podman machine omitted its SSH port.")?;
    let identity = inspected.pointer("/SSHConfig/IdentityPath").and_then(|v| v.as_str()).ok_or("Podman machine omitted its SSH identity path.")?;
    let connections = podman_connections(ctl)?;
    let connection = connections.iter().find_map(|value| {
        let uri = reqwest::Url::parse(value["URI"].as_str()?).ok()?;
        let rootless = uri.path().starts_with("/run/user/") && uri.username() != "root";
        if rootless && uri.port().map(u64::from) == Some(port)
            && value["Identity"].as_str() == Some(identity)
            && matches!(uri.host_str(), Some("127.0.0.1" | "localhost" | "[::1]" | "::1")) {
            value["Name"].as_str().map(str::to_owned)
        } else { None }
    }).ok_or("The app-owned Podman machine has no verified rootless connection. Repair Podman machine setup, then retry; no ambient/default connection will be used.")?;
    receipt.connection = Some(connection.clone());
    receipt.stage = "ready".into();
    save_receipt(root, &receipt)?;
    CliEngine::new(QualityRuntime::Podman { connection: connection.clone() }, root)?.probe(ctl)?;
    Ok(connection)
}

pub fn install_runtime(config_root: &Path, runtime: &QualityRuntime, ctl: &Ctl) -> Result<InstallOutcome> {
    if ctl.cancelled() { return Err("Runtime installation cancelled before it started.".into()); }
    let mut connection = None;
    let restart = match runtime {
        QualityRuntime::DockerDesktop => {
            let restart = if docker_program().is_none() || !program_files().join("Docker/Docker/Docker Desktop.exe").is_file() {
                run_winget("Docker.DockerDesktop", ctl)?
            } else { false };
            if !restart { CliEngine::new(runtime.clone(), config_root)?.ensure_started(ctl).map_err(|e| explain_failure(&e))?; }
            restart
        }
        QualityRuntime::Podman { connection: selected } => {
            let restart = if podman_program().is_none() { run_winget("RedHat.Podman", ctl)? } else { false };
            if !restart {
                if podman_program().is_none() { return Err("Podman installer exited but podman.exe was not found. Repair the official installation and Recheck.".into()); }
                if selected.is_empty() { connection = Some(setup_podman(config_root, ctl).map_err(|e| explain_failure(&e))?); }
                else {
                    validate_selector(selected, "Podman connection")?;
                    if !podman_connections(ctl)?.iter().any(|v| v["Name"].as_str() == Some(selected)) {
                        return Err("Selected Podman connection no longer exists; Recheck and explicitly choose an existing connection.".into());
                    }
                    CliEngine::new(runtime.clone(), config_root)?.ensure_started(ctl).map_err(|e| explain_failure(&e))?;
                    connection = Some(selected.clone());
                }
            }
            restart
        }
        QualityRuntime::DockerWsl { distro } => {
            validate_selector(distro, "WSL distribution")?;
            if !wsl_distros(ctl)?.contains(distro) { return Err("The explicitly selected WSL distribution is not registered. Recheck and select an installed distro.".into()); }
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
            engine.ensure_started(ctl).map_err(|e| explain_failure(&e))?;
            false
        }
    };
    if restart {
        fs::create_dir_all(config_root.join("setup")).map_err(|e| format!("Record runtime restart requirement: {e}"))?;
        let receipt = FeatureReceipt { version: 1, needs_restart: true, boot_id: boot_id(ctl) };
        crate::dependency_security::atomic_json(&config_root.join("setup/runtime-restart.json"), &receipt, JSON_LIMIT)?;
    }
    Ok(InstallOutcome { needs_restart: restart, message: if restart {
        "Runtime installation requires a Windows restart. Restart Windows manually, then Recheck; OMP Manager did not reboot or start another installer.".into()
    } else { "Selected runtime is installed and its Linux daemon is ready. No global engine context/default was changed.".into() }, podman_connection: connection })
}

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

#[cfg(test)]
mod tests {
    use super::*;
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
}
