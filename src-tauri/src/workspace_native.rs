use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};

type Result<T, E = String> = std::result::Result<T, E>;
const FRAME_LIMIT: u64 = 64 * 1024;
const DEADLINE: Duration = Duration::from_secs(120);

pub(crate) fn materialize(root: &Path) -> Result<()> {
    fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let target = root.join("omp-workspace.mjs");
    let source = include_str!("../../bridge/omp-workspace.mjs");
    if fs::read_to_string(&target).ok().as_deref() != Some(source) {
        fs::write(target, source).map_err(|error| error.to_string())?;
    }
    let target = root.join("omp-native-ui.mjs");
    let source = include_str!("../../bridge/omp-native-ui.mjs");
    if fs::read_to_string(&target).ok().as_deref() != Some(source) {
        fs::write(target, source).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    crate::code_diff::no_console(&mut command);
    command
}

fn check_omp_version(version: &str) -> Result<()> {
    let output = command(crate::toolchain::omp_program())
        .arg("--version")
        .output()
        .map_err(|error| format!("Read OMP version: {error}"))?;
    if !output.status.success()
        || String::from_utf8_lossy(&output.stdout).trim() != format!("omp/{version}")
    {
        return Err(format!(
            "OMP SDK {version} does not match the executable; set OMP_SDK_DIR to its installed SDK"
        ));
    }
    Ok(())
}
/// Verifies the SDK matches the executable and returns the encoded helper request with its working directory.
fn helper_request(root: &Path, home: &Path, request: &mut Value) -> Result<(Vec<u8>, PathBuf)> {
    materialize(root)?;
    let sdk = crate::config::sdk_dir(home)?;
    let package: Value = serde_json::from_slice(
        &fs::read(sdk.join("package.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let version = package["version"]
        .as_str()
        .ok_or("OMP SDK has no version")?;
    check_omp_version(version)?;
    request["sdkDir"] = json!(sdk);
    let cwd = request["cwd"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.to_path_buf());
    let encoded = serde_json::to_vec(&*request).map_err(|error| error.to_string())?;
    if encoded.len() as u64 > FRAME_LIMIT {
        return Err("Workspace helper request exceeds transport limit".into());
    }
    Ok((encoded, cwd))
}
/// Forwards newline-delimited JSON frames from the helper until EOF, a bad frame or a closed receiver.
fn read_frames(stdout: ChildStdout, send: mpsc::SyncSender<Result<Value>>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut bytes = Vec::new();
        let result = (&mut reader)
            .take(FRAME_LIMIT + 1)
            .read_until(b'\n', &mut bytes);
        let result = match result {
            Ok(0) => break,
            Ok(_) if bytes.len() as u64 > FRAME_LIMIT => {
                Err("Workspace helper frame exceeds transport limit".to_string())
            }
            Ok(_) => serde_json::from_slice::<Value>(&bytes)
                .map_err(|error| format!("Invalid native workspace response: {error}")),
            Err(error) => Err(format!("Read native workspace response: {error}")),
        };
        let failed = result.is_err();
        if send.send(result).is_err() || failed {
            break;
        }
    }
}
/// Sends the request, runs the persistence callback once the helper is ready, then reports the helper's verdict.
fn exchange<T>(
    input: &mut ChildStdin,
    receive: &mpsc::Receiver<Result<Value>>,
    encoded: &[u8],
    commit: impl FnOnce(&Value) -> Result<T>,
) -> Result<T> {
    input
        .write_all(encoded)
        .and_then(|_| input.write_all(b"\n"))
        .and_then(|_| input.flush())
        .map_err(|error| format!("Write workspace request: {error}"))?;
    let ready = receive.recv_timeout(DEADLINE).map_err(|error| {
        format!("Native workspace helper did not acknowledge relocation: {error}")
    })??;
    if ready["phase"] != "ready" {
        return Err(ready["error"]
            .as_str()
            .unwrap_or("Native workspace relocation failed")
            .to_string());
    }
    let committed = commit(&ready["identity"]);
    let decision = if committed.is_ok() {
        b"{\"commit\":true}\n".as_slice()
    } else {
        b"{\"commit\":false}\n".as_slice()
    };
    input
        .write_all(decision)
        .and_then(|_| input.flush())
        .map_err(|error| {
            format!(
                "Native workspace acknowledgement uncertain; actual identity {}: {error}",
                ready["identity"]
            )
        })?;
    let complete = receive.recv_timeout(DEADLINE).map_err(|error| {
        format!(
            "Native workspace completion uncertain; actual identity {}: {error}",
            ready["identity"]
        )
    })??;
    if complete["phase"] != "complete" {
        return Err(format!(
            "{}; {}",
            committed
                .err()
                .unwrap_or_else(|| "Native workspace finalization failed".into()),
            complete["error"]
                .as_str()
                .unwrap_or("Native workspace helper failed")
        ));
    }
    match committed {
        Ok(value) if complete["committed"] == true => Ok(value),
        Ok(_) => Err("Native workspace helper unexpectedly rolled back a committed change".into()),
        Err(error) => Err(error),
    }
}
/// Runs once the helper's stdin is closed: drains a failed exchange, reaps the child and checks its exit status.
fn reap_helper<T>(
    result: Result<T>,
    receive: &mpsc::Receiver<Result<Value>>,
    mut child: Child,
    reader: std::thread::JoinHandle<()>,
) -> Result<T> {
    // EOF tells an undecided helper to restore its captured source state. Drain
    // its terminal response before reaping, so a failed Registry save never
    // releases the lifecycle reservation while rollback is still mutating files.
    if result.is_err() {
        while let Ok(frame) = receive.recv_timeout(DEADLINE) {
            if frame
                .as_ref()
                .is_ok_and(|value| value["phase"] == "complete" || value["phase"] == "failed")
            {
                break;
            }
        }
    }
    let status = child
        .wait()
        .map_err(|error| format!("Wait for native workspace helper: {error}"));
    let _ = reader.join();
    match result {
        Err(error) => Err(error),
        Ok(value) => {
            if !status?.success() {
                return Err(
                    "Native workspace helper could not finalize the relocated session".into(),
                );
            }
            Ok(value)
        }
    }
}
/// The child owns the native writer lease until the persistence callback decides
/// commit or rollback. No transcript/snapshot is serialized through this channel.
/// The caller must already hold the coordinator's lifecycle reservation, but no
/// Registry records/queue/hub lock may be held while invoking this function.
pub(crate) fn relocate<T>(
    root: &Path,
    home: &Path,
    mut request: Value,
    commit: impl FnOnce(&Value) -> Result<T>,
) -> Result<T> {
    let (encoded, cwd) = helper_request(root, home, &mut request)?;
    let mut child = command(crate::toolchain::bun_program())
        .arg(root.join("omp-workspace.mjs"))
        .current_dir(cwd)
        .env_remove("PI_COMPILED")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Start native workspace helper: {error}"))?;
    let mut input = child
        .stdin
        .take()
        .ok_or("Workspace helper stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("Workspace helper stdout unavailable")?;
    let (send, receive) = mpsc::sync_channel(4);
    let reader = std::thread::spawn(move || read_frames(stdout, send));
    let result = exchange(&mut input, &receive, &encoded, commit);
    drop(input);
    reap_helper(result, &receive, child, reader)
}
