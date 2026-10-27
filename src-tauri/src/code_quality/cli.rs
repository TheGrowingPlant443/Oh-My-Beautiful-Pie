//! Headless entry point using the desktop service, its persisted ownership and OS secrets.
use super::runtime::{Ctl, ROLE_LABEL};
use super::types::*;
use super::{store, CodeQualityService};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

const HELP: &str = "OMP Manager native code-quality CLI (Linux/macOS)

Usage: omp-manager --code-quality <status|up|scan|down> [projectdir]
                                 [--port PORT] [--state-dir DIR]
       omp-manager --code-quality --help

  status  Read prerequisites and app-owned container status; never start an engine.
  up      Start the saved analyzer with the same service used by the GUI.
  scan    Detect the directory (default: current directory), start the saved
          analyzer, approve its detected recipe and wait for the final scan.
  down    Stop only this state directory's app-owned resources; retain data.

--state-dir is the app configuration root (code-quality/ is created inside it).
Default: the GUI's platform configuration directory. Use the SAME directory for
all commands. A GUI or CLI already managing it blocks mutations; no takeover.
--port overrides the saved Community port on up/scan; status/down require it to
match the saved port. Without --port, use the saved port (initially 9000).

up/scan explicitly consent to service startup and pinned container image pulls,
NOT installing software, host configuration changes, or runtime/VM shutdown.
Install and prepare Podman/Docker separately. Linux uses the native engine;
macOS requires an existing Podman machine/connection or Docker Desktop.
Secret Service (Linux) / Keychain (macOS) must be available in this login session.
scan also consents to the detected recipe: supported Maven/Gradle/.NET builds can
execute project code and download dependencies in a disposable snapshot. TS/JS,
Python and other direct-analysis languages need no project-specific settings.
Ambiguous build families/.NET entries must first be selected in the GUI.
No working-tree bind mounts or changes. No credentials accepted on the CLI.

stdout: one JSON result (no credentials, raw logs, finding messages or build args).
stderr: errors. Exit: 0 success; 1 infrastructure/incomplete analysis; 2 completed
analysis with findings or a failed quality gate; 64 invalid arguments. scan waits
for the existing service's bounded scanner/server jobs (one-hour scan watchdog).
up and scan leave managed analyzer containers running for the next command;
down stops them, never the shared engine daemon or VM.
An interrupted scan (Ctrl-C, kill) can leave its scanner container running; run `down` to retire it.

Example (9001 avoids an independent analyzer already using 9000):
  omp-manager --code-quality up --port 9001 --state-dir /tmp/omp-quality
  omp-manager --code-quality scan ./project --state-dir /tmp/omp-quality
  omp-manager --code-quality status --state-dir /tmp/omp-quality
  omp-manager --code-quality down --state-dir /tmp/omp-quality
";

struct Args {
    command: String,
    project: Option<PathBuf>,
    port: Option<u16>,
    root: PathBuf,
    /// An explicit --state-dir is canonicalized; the default must stay byte-identical to the GUI's config path,
    /// because credential names are derived from that path text.
    explicit_root: bool,
}

/// `None` preserves ordinary desktop startup, including all Windows startup behavior.
pub(crate) fn dispatch() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--code-quality")) {
        return None;
    }
    let raw: Vec<OsString> = args.collect();
    if raw.len() == 1 && matches!(raw[0].to_str(), Some("--help" | "-h")) {
        print!("{HELP}");
        return Some(0);
    }
    let args = match parse(raw) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}\nUse --code-quality --help for usage.");
            let _ = emit(json!({"ok": false, "exitCode": 64, "error": "invalidArguments"}));
            return Some(64);
        }
    };
    Some(run(args))
}

fn parse(raw: Vec<OsString>) -> Result<Args> {
    let mut raw = raw.into_iter();
    let command = raw.next().and_then(|s| s.into_string().ok()).ok_or("Choose status, up, scan or down.")?;
    if !matches!(command.as_str(), "status" | "up" | "scan" | "down") {
        return Err("Choose status, up, scan or down.".into());
    }
    let mut project = None;
    let mut port = None;
    let mut root = None;
    let mut positional = false;
    while let Some(arg) = raw.next() {
        if !positional && arg == "--" {
            positional = true;
        } else if !positional && (arg == "--port" || arg.to_str().is_some_and(|s| s.starts_with("--port="))) {
            if port.is_some() { return Err("Pass --port only once.".into()); }
            let value = if arg == "--port" { raw.next().ok_or("--port needs a value.")? } else { OsString::from(arg.to_str().unwrap().trim_start_matches("--port=")) };
            let value = value.to_str().ok_or("--port must be a number.")?;
            port = Some(value.parse::<u16>().ok().filter(|p| *p >= 1024).ok_or("--port must be between 1024 and 65535.")?);
        } else if !positional && (arg == "--state-dir" || arg.to_str().is_some_and(|s| s.starts_with("--state-dir="))) {
            if root.is_some() { return Err("Pass --state-dir only once.".into()); }
            let value = if arg == "--state-dir" { raw.next().ok_or("--state-dir needs a directory.")? } else { OsString::from(arg.to_str().unwrap().trim_start_matches("--state-dir=")) };
            if value.is_empty() { return Err("--state-dir needs a directory.".into()); }
            root = Some(PathBuf::from(value));
        } else if !positional && arg.to_string_lossy().starts_with('-') {
            return Err("Unknown option; use --code-quality --help.".into());
        } else if project.replace(PathBuf::from(arg)).is_some() {
            return Err("Pass at most one project directory.".into());
        }
    }
    let explicit_root = root.is_some();
    let root = root.map(Ok).unwrap_or_else(default_root)?;
    Ok(Args { command, project, port, root, explicit_root })
}

fn default_root() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"));
    #[cfg(not(target_os = "macos"))]
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    let identifier = serde_json::from_str::<Value>(include_str!("../../tauri.conf.json"))
        .map_err(|e| e.to_string())?.get("identifier").and_then(Value::as_str)
        .ok_or("The application configuration has no identifier.")?.to_owned();
    Ok(base.ok_or("Cannot find the app configuration directory; pass --state-dir.")?.join(identifier))
}

fn run(args: Args) -> i32 {
    let root = (|| -> std::io::Result<PathBuf> {
        use std::os::unix::fs::DirBuilderExt;
        // The state root holds the ownership handover capability and scan results: owner-only.
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&args.root)?;
        if args.explicit_root { std::fs::canonicalize(&args.root) } else { Ok(args.root.clone()) }
    })();
    let root = match root {
        Ok(root) => root,
        Err(error) => {
            eprintln!("Cannot prepare the state directory: {error}");
            let _ = emit(json!({ "command": args.command, "ok": false, "exitCode": 1, "error": "stateDirectoryUnavailable" }));
            return 1;
        }
    };
    // Explicit aliases of one state directory must share the credential targets; the default matches the GUI.
    let svc = CodeQualityService::new(root);
    let mut scan = None;
    let mut containers = None;
    let mut result = execute(&svc, &args.command, args.project, args.port, &mut scan, &mut containers);
    // All operations except start_scan are synchronous. Do not release the process lock while a
    // scanner still owns the gate, including when loading a result failed after admission.
    for worker in std::mem::take(&mut *svc.workers.lock()) {
        if worker.join().is_err() {
            result = Err("A code-quality worker terminated unexpectedly.".into());
        }
    }
    let state = svc.state();
    let mut exit = match result {
        Ok(()) => scan.as_ref().map(scan_exit).unwrap_or(0),
        Err(error) => { eprintln!("{error}"); 1 }
    };
    if let Some(scan) = &scan {
        if exit == 1 {
            eprintln!("{}", scan.error.as_deref().unwrap_or("The scan did not complete with full analysis coverage."));
        } else if exit == 2 {
            eprintln!("Analysis completed with findings or a failing quality gate.");
        }
    }
    // This is a command client, not a desktop exit: leave owned containers for the next CLI
    // invocation. Use the existing ownership release, not shutdown(), which stops the analyzer.
    svc.release_owner(false);
    let report = json!({
        "command": args.command, "ok": exit == 0, "exitCode": exit,
        "state": state_report(&state), "managedContainers": containers,
        "scan": scan.as_ref().map(scan_report),
    });
    if let Err(error) = emit(report) {
        eprintln!("Cannot write JSON report: {error}");
        exit = 1;
    }
    exit
}

fn execute(svc: &Arc<CodeQualityService>, command: &str, project: Option<PathBuf>, port: Option<u16>, scan: &mut Option<QualityScan>, containers: &mut Option<Value>) -> Result<()> {
    let state = svc.state();
    if let Some(error) = state.error { return Err(error); }
    if command != "status" && !svc.owns_resources() {
        return Err("Another OMP Manager process manages this state directory; close it before using this CLI.".into());
    }
    if matches!(command, "status" | "down") && port.is_some_and(|p| p != state.config.community_port) {
        return Err("--port does not match the stored port; change it with up or scan.".into());
    }
    match command {
        "status" => {
            svc.check_prerequisites()?;
            let engine = svc.factory.engine(&state.config.runtime)?;
            let ctl = Ctl::silent();
            let probe = engine.probe(&ctl)?; // read-only: never ensure_started()
            let resources = store::load_resources(&svc.paths)?;
            if resources.runtime(&state.config.runtime).is_some_and(|r| r.fingerprint != probe.fingerprint) {
                return Err("The container daemon identity changed; use up to reconcile the recorded resources.".into());
            }
            *containers = Some(Value::Array(engine.list_owned(&svc.installation_id(), &ctl)?.iter().map(|c| json!({
                "id": c.id, "name": c.name, "role": c.labels.get(ROLE_LABEL), "running": c.running,
            })).collect()));
            Ok(())
        }
        "down" => { svc.stop_server()?; Ok(()) }
        "up" | "scan" => {
            // Detection/preparation uses the exact saved-project/build-source route as the GUI.
            // Resolve ambiguous projects before touching the analyzer.
            let preparation = if command == "scan" {
                let folder = match project {
                    Some(folder) => folder,
                    None => std::env::current_dir().map_err(|e| e.to_string())?,
                };
                let folder = folder.to_str().ok_or("The project path must be valid UTF-8.")?;
                let detection = svc.detect_project(folder)?;
                if detection.needs_selection {
                    return Err("The project has ambiguous build families or .NET entries. Select and save the scanner/entry in the GUI first.".into());
                }
                Some(detection.project)
            } else { None };
            let config_changed = port.is_some_and(|port| port != state.config.community_port);
            let mut config = state.config;
            if let Some(port) = port { config.community_port = port; }
            let started = if config_changed {
                svc.configure(config, state.revision, None)?
            } else {
                svc.start_service(state.revision)?
            };
            if started.effective_mode != Some(started.config.mode) || started.error.is_some() {
                return Err(started.error.unwrap_or_else(|| "The analyzer is not ready; check its prerequisites in the GUI.".into()));
            }
            if let Some(project) = preparation {
                let prepared = svc.prepare_scan(project.clone())?;
                let admission = svc.start_scan(project, prepared.recipe_hash, true)?;
                // The service owns the one-hour watchdog and per-engine/server deadlines. Join
                // its actual jobs, not an unrelated HTTP submission or a second timeout policy.
                for worker in std::mem::take(&mut *svc.workers.lock()) {
                    worker.join().map_err(|_| "The scan worker terminated unexpectedly.")?;
                }
                let completed = svc.read_scan(&admission.scan_id)?;
                if !completed.status.is_terminal() {
                    return Err("The scan worker stopped without a terminal result.".into());
                }
                *scan = Some(completed);
            }
            Ok(())
        }
        _ => unreachable!("validated command"),
    }
}

fn scan_exit(scan: &QualityScan) -> i32 {
    if scan.status != QualityScanStatus::Complete || scan.error.is_some()
        || !scan.tools.iter().any(|t| t.status == ToolStatus::Complete)
        || scan.tools.iter().any(|t| matches!(t.status, ToolStatus::Error | ToolStatus::Partial))
        || (scan.config.mode != QualityMode::Alternate && !matches!(scan.quality_gate, Some(QualityGate::Passed | QualityGate::Failed))) {
        1
    } else if !scan.findings.is_empty() || scan.quality_gate == Some(QualityGate::Failed) {
        2
    } else { 0 }
}

fn state_report(state: &QualityState) -> Value {
    // Deliberate allowlist: never serialize stored URLs, recipes, diagnostic tails, credentials,
    // ownership capabilities or user build arguments into a shell/CI report.
    json!({
        "platform": state.platform, "revision": state.revision, "activated": state.activated,
        "mode": state.config.mode, "runtime": state.config.runtime,
        "port": state.config.community_port, "phase": state.phase,
        "effectiveMode": state.effective_mode, "ownership": state.ownership.status,
        "busy": state.busy, "hasError": state.error.is_some(),
        "credentialStore": { "name": state.credential_store.name, "available": state.credential_store.available },
        "community": state.community, "prerequisites": state.prerequisites.iter().map(|p| json!({
            "runtime": p.runtime, "installed": p.installed, "ready": p.ready, "needsRestart": p.needs_restart,
        })).collect::<Vec<_>>(),
    })
}

fn scan_report(scan: &QualityScan) -> Value {
    json!({
        "id": scan.id, "status": scan.status, "startedAt": scan.started_at,
        "finishedAt": scan.finished_at, "snapshotHash": scan.snapshot_hash,
        "qualityGate": scan.quality_gate, "findingCount": scan.findings.len(),
        "hasError": scan.error.is_some(), "warningCount": scan.warnings.len(),
        "tools": scan.tools.iter().map(|t| json!({
            "tool": t.tool, "status": t.status, "exitCode": t.exit_code,
            "analyzedFiles": t.analyzed_files, "findingCount": t.finding_count,
            "reportNames": t.report_names, "hasError": t.error.is_some(),
        })).collect::<Vec<_>>(),
        "findings": scan.findings.iter().map(|f| json!({
            "id": f.id, "tools": f.tools, "ruleId": f.rule_id, "category": f.category,
            "severity": f.severity, "path": f.path, "line": f.line, "column": f.column,
            "endLine": f.end_line, "hotspot": f.hotspot,
        })).collect::<Vec<_>>(),
    })
}

fn emit(report: Value) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &report).map_err(|e| e.to_string())?;
    stdout.write_all(b"\n").map_err(|e| e.to_string())
}
