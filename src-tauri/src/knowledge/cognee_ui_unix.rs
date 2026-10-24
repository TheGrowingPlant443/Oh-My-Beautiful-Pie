use super::{Listener, Row};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

pub(crate) fn tcp_listeners() -> Result<Vec<Listener>, String> {
    let inventory = crate::host_monitor::native::collect(&AtomicBool::new(false))?;
    Ok(inventory.tcp.into_iter().filter(|row| row.tcp_state == Some(2)).filter_map(|row| {
        Some(Listener { address: row.local.address.parse().ok()?, port: row.local.port, pid: row.pid })
    }).collect())
}
pub(crate) fn creation_time(pid: u32) -> Option<u64> { crate::process_tree::process_creation_time(pid) }
pub(crate) fn parent_map() -> Result<HashMap<u32, u32>, String> {
    let inventory = crate::host_monitor::native::collect(&AtomicBool::new(false))?;
    Ok(inventory.processes.into_iter().map(|p| (p.pid, p.parent_pid)).collect())
}
pub(crate) fn in_tree(parents: &HashMap<u32, u32>, root: u32, root_created: u64, pid: u32) -> bool {
    if creation_time(root) != Some(root_created) { return false; }
    let Some(mut child_created) = creation_time(pid) else { return false };
    let mut current = pid;
    for _ in 0..64 {
        if current == root { return child_created == root_created; }
        let Some(&parent) = parents.get(&current) else { return false };
        let Some(parent_created) = creation_time(parent) else { return false };
        if parent_created > child_created { return false; }
        current = parent;
        child_created = parent_created;
    }
    false
}

#[cfg(target_os = "linux")]
pub(super) fn query_processes() -> Result<Vec<Row>, String> {
    use std::io::Read;
    let mut rows = Vec::new();
    for entry in std::fs::read_dir("/proc").map_err(|e| e.to_string())?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else { continue };
        let Ok(exe) = std::fs::read_link(entry.path().join("exe")) else { continue };
        let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.starts_with("python") && name != "node" { continue; }
        let Some(created) = creation_time(pid) else { continue };
        let Ok(file) = std::fs::File::open(entry.path().join("cmdline")) else { continue };
        let mut bytes = Vec::new();
        if file.take(32769).read_to_end(&mut bytes).is_err() || bytes.len() > 32768 { continue; }
        let Ok(text) = std::str::from_utf8(&bytes) else { continue };
        let cmd = text.split('\0').filter(|s| !s.is_empty()).map(|arg| format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))).collect::<Vec<_>>().join(" ");
        rows.push(Row { pid, created, exe: exe.to_string_lossy().into_owned(), cmd });
    }
    Ok(rows)
}

#[cfg(target_os = "macos")]
pub(super) fn query_processes() -> Result<Vec<Row>, String> {
    let mut command = std::process::Command::new("/bin/ps");
    command.args(["-axo", "pid=,command="]);
    let output = crate::code_diff::run_timed(command, None, std::time::Duration::from_secs(15), "macOS process inspection failed")?;
    if !output.status.success() { return Err("macOS process inspection failed".into()); }
    let mut rows = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((pid, cmd)) = line.trim().split_once(char::is_whitespace) else { continue };
        let Some(pid) = pid.parse::<u32>().ok() else { continue };
        let Some(created) = creation_time(pid) else { continue };
        let mut buffer = [0u8; 4096];
        // SAFETY: buffer is a valid writable byte array; proc_pidpath writes at most its length.
        let length = unsafe { libc::proc_pidpath(pid as i32, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        if length <= 0 { continue; }
        let length = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
        let exe = String::from_utf8_lossy(&buffer[..length]).into_owned();
        rows.push(Row { pid, created, exe, cmd: cmd.trim().to_string() });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn a_loopback_listener_is_attributed_to_the_process_that_bound_it() {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        let found = tcp_listeners().unwrap();
        assert!(found.contains(&Listener { address: "127.0.0.1".parse().unwrap(), port, pid: std::process::id() }), "{found:?}");
    }

    #[test]
    fn only_descendants_of_the_launched_root_are_in_its_tree() {
        let mut root = Command::new("/bin/sh").args(["-c", "sleep 30 & wait"]).spawn().unwrap();
        let created = creation_time(root.id()).expect("root identity");
        let deadline = Instant::now() + Duration::from_secs(10);
        let (parents, grandchild) = loop {
            let parents = parent_map().unwrap();
            if let Some((&pid, _)) = parents.iter().find(|(_, &parent)| parent == root.id()) { break (parents, pid); }
            assert!(Instant::now() < deadline, "the shell never started its background child");
            std::thread::sleep(Duration::from_millis(50));
        };
        let owned = in_tree(&parents, root.id(), created, grandchild);
        let unrelated = in_tree(&parents, root.id(), created, std::process::id());
        let stale_identity = in_tree(&parents, root.id(), created + 1, grandchild);
        // SAFETY: both pids belong to this test's own children.
        unsafe { libc::kill(grandchild as i32, libc::SIGKILL); }
        root.kill().unwrap();
        root.wait().unwrap();
        assert!(owned, "a grandchild belongs to the owned tree");
        assert!(!unrelated, "this test process is not a descendant of its own child");
        assert!(!stale_identity, "a root whose creation identity differs must not own anything");
    }

    #[test]
    fn a_backend_is_recognized_by_the_real_bin_directory_of_the_installed_venv() {
        use super::super::launched_from_venv;
        let root = std::env::temp_dir().join(format!("omp-venv-{}", std::process::id()));
        let store = root.join("store/bin");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("python3.13"), "").unwrap();
        // A Nix-style environment: the folder's venv is a link to the real directory, and the process reports the versioned name.
        std::os::unix::fs::symlink(root.join("store"), root.join("venv")).unwrap();
        let configured = root.join("venv/bin/python");
        let reported = store.join("python3.13");
        assert!(launched_from_venv(reported.to_str().unwrap(), "", &configured));
        assert!(launched_from_venv(configured.to_str().unwrap(), "", &configured));
        // The shared base interpreter and unrelated environments are not this venv.
        assert!(!launched_from_venv("/usr/bin/python3", "/usr/bin/python3", &configured));
        assert!(!launched_from_venv(root.join("other/bin/python3.13").to_str().unwrap(), "", &configured));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Opt-in end-to-end proof with the real installed frontend, Node.js and a running Cognee backend:
    /// `OMP_COGNEE_NATIVE=1 OMP_KNOWLEDGE_COGNEE_HOME=<cognee folder> cargo test real_frontend`.
    /// It starts only its own Next.js server (no browser is opened) and checks that shutdown frees the port.
    #[test]
    fn real_frontend_starts_on_loopback_and_is_retired_on_shutdown() {
        use super::super::{discover_backends, CogneeUi, Hooks, FRONTEND_PORT};
        use crate::knowledge::cognee_ui::{inspect_components, CogneeOpenState};
        let (Ok("1"), Some(folder)) = (std::env::var("OMP_COGNEE_NATIVE").as_deref(), std::env::var_os("OMP_KNOWLEDGE_COGNEE_HOME")) else { return };
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        let components = inspect_components(std::path::Path::new(&folder), &home, None).expect("installed frontend, Node.js and Next native binding");
        let opened = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let seen = opened.clone();
        let ui = CogneeUi::new();
        ui.set_hooks(Hooks {
            frontend_port: FRONTEND_PORT,
            startup_timeout: Duration::from_secs(120),
            discover: Box::new(discover_backends),
            opener: Box::new(move |url| { seen.lock().push(url.to_string()); Ok(()) }),
        });
        let result = ui.open(&components);
        assert_eq!(result.state, CogneeOpenState::Opened, "{result:?}");
        assert_eq!(*opened.lock(), vec![format!("http://localhost:{FRONTEND_PORT}")]);
        let root = ui.owned_pid().expect("the frontend is owned");
        let owners: Vec<u32> = tcp_listeners().unwrap().into_iter().filter(|l| l.port == FRONTEND_PORT && l.address.is_loopback()).map(|l| l.pid).collect();
        assert!(!owners.is_empty(), "the frontend listens on loopback");
        let parents = parent_map().unwrap();
        let created = creation_time(root).unwrap();
        assert!(owners.iter().all(|&pid| in_tree(&parents, root, created, pid)), "every listener belongs to the owned tree: {owners:?} root {root}");
        ui.shutdown();
        let deadline = Instant::now() + Duration::from_secs(15);
        while tcp_listeners().unwrap().iter().any(|l| l.port == FRONTEND_PORT) || creation_time(root).is_some() {
            assert!(Instant::now() < deadline, "the frontend survived shutdown");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}
