//! Unix process ownership for trusted app-owned helpers. Each root starts a new session before exec.
//! Retirement signals the session's processes and waits for live members to disappear before releasing a profile.
//! This is lifecycle management, not a sandbox: a deliberately escaping child can create another session.
use parking_lot::{Condvar, Mutex};
use std::{ffi::OsString, io::Read, path::PathBuf, process::{Child, Command, Stdio}, sync::Arc, time::{Duration, Instant}};
use std::os::unix::process::CommandExt;

pub const RETIRE_BOUND: Duration = Duration::from_secs(5);
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StdioMode { Null, Capture }
pub struct ContainedSpec {
    pub subject: String,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env_set: Vec<(OsString, OsString)>,
    pub env_remove: Vec<OsString>,
    pub stdout: StdioMode,
    pub stderr: StdioMode,
    pub capture_limit: usize,
}
struct CaptureState { bytes: Vec<u8>, eof: bool }
pub struct Capture { state: Mutex<CaptureState>, changed: Condvar, limit: usize }
impl Capture {
    fn new(limit: usize, eof: bool) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(CaptureState { bytes: Vec::new(), eof }), changed: Condvar::new(), limit: limit.max(1) })
    }
    pub fn tail(&self) -> String { String::from_utf8_lossy(&self.state.lock().bytes).into_owned() }
    pub fn wait_match_until<T>(&self, timeout: Duration, stop: &dyn Fn() -> bool, mut find: impl FnMut(&str) -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock();
        loop {
            if let Some(found) = find(&String::from_utf8_lossy(&state.bytes)) { return Some(found); }
            if state.eof || stop() || Instant::now() >= deadline { return None; }
            self.changed.wait_for(&mut state, deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(50)));
        }
    }
    pub fn wait_match<T>(&self, timeout: Duration, find: impl FnMut(&str) -> Option<T>) -> Option<T> {
        self.wait_match_until(timeout, &|| false, find)
    }
}
fn drain(mut reader: impl Read + Send + 'static, output: Arc<Capture>) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let mut state = output.state.lock();
                    state.bytes.extend_from_slice(&chunk[..n]);
                    let excess = state.bytes.len().saturating_sub(output.limit);
                    if excess > 0 { state.bytes.drain(..excess); }
                    output.changed.notify_all();
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        output.state.lock().eof = true;
        output.changed.notify_all();
    });
}

pub fn process_creation_time(pid: u32) -> Option<u64> {
    crate::host_monitor::capture_identity(pid).map(|p| p.creation_time_100ns)
}

/// Live members of our dedicated session, including child process groups; zombies are already dead.
#[cfg(target_os = "linux")]
fn members(session: u32) -> Result<Vec<i32>, String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").map_err(|e| format!("Read process membership: {e}"))?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        let stat = match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            // A process that vanished mid-scan is gone; any other failure must not look like an empty session.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => continue,
            Err(e) => return Err(format!("Read process membership: {e}")),
        };
        let Some((_, fields)) = stat.rsplit_once(") ") else { continue };
        let mut fields = fields.split_ascii_whitespace();
        let state = fields.next();
        let sid = fields.nth(2).and_then(|n| n.parse::<u32>().ok());
        if sid == Some(session) && !matches!(state, Some("Z" | "X" | "x")) { found.push(pid); }
    }
    Ok(found)
}
#[cfg(target_os = "macos")]
fn members(session: u32) -> Result<Vec<i32>, String> {
    // Job-control children may create another process group but remain in the PTY's session.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count < 0 { return Err(std::io::Error::last_os_error().to_string()); }
    let mut pids = vec![0i32; count as usize + 256];
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * std::mem::size_of::<i32>()) as i32) };
    if count < 0 || count as usize >= pids.len() { return Err("Cannot enumerate the complete process session".into()); }
    pids.truncate(count as usize);
    Ok(pids.into_iter().filter(|&pid| {
        if pid <= 1 || unsafe { libc::getsid(pid) } != session as i32 { return false; }
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let read = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
        // SZOMB means already dead; an unreadable live process stays a member so retirement fails closed.
        if read == size { unsafe { info.assume_init() }.pbi_status != 5 } else { std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) }
    }).collect())
}

/// Retire every process still in a session we created, including shell job-control groups.
/// The caller holds the root child unreaped during signal delivery.
pub(crate) fn kill_session(session: u32) -> Result<(), String> {
    if session <= 1 { return Err("Refusing to signal an invalid session".into()); }
    let deadline = Instant::now() + RETIRE_BOUND;
    loop {
        let live = members(session)?;
        if live.is_empty() { return Ok(()); }
        for pid in live {
            // SAFETY: these are current members of the dedicated session, never arbitrary ancestry guesses.
            unsafe { libc::kill(pid, libc::SIGKILL); }
        }
        if Instant::now() >= deadline { return Err("Process-session retirement could not be verified".into()); }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub struct ContainedChild {
    subject: String, pid: u32, creation: u64, child: Mutex<Child>, retired: Mutex<bool>, stdout: Arc<Capture>, stderr: Arc<Capture>,
}
impl ContainedChild {
    pub fn spawn(spec: ContainedSpec) -> Result<Self, String> {
        if !spec.program.is_absolute() { return Err(format!("{} executable must be absolute", spec.subject)); }
        let mut command = Command::new(spec.program);
        command.args(spec.args).envs(spec.env_set).stdin(Stdio::null());
        for name in spec.env_remove { command.env_remove(name); }
        if let Some(cwd) = spec.cwd { command.current_dir(cwd); }
        command.stdout(if spec.stdout == StdioMode::Capture { Stdio::piped() } else { Stdio::null() });
        command.stderr(if spec.stderr == StdioMode::Capture { Stdio::piped() } else { Stdio::null() });
        // SAFETY: the child callback uses only async-signal-safe syscalls before exec, never allocates or locks.
        unsafe { command.pre_exec(|| {
            if libc::setsid() < 0 { return Err(std::io::Error::last_os_error()); }
            Ok(())
        }); }
        let mut child = command.spawn().map_err(|e| format!("Start {}: {e}", spec.subject))?;
        let pid = child.id();
        let creation = match process_creation_time(pid) {
            Some(value) => value,
            None => {
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
                let _ = child.wait();
                return Err(format!("{} exited before its identity could be captured", spec.subject));
            }
        };
        let stdout = Capture::new(spec.capture_limit, spec.stdout == StdioMode::Null);
        let stderr = Capture::new(spec.capture_limit, spec.stderr == StdioMode::Null);
        if let Some(reader) = child.stdout.take() { drain(reader, stdout.clone()); }
        if let Some(reader) = child.stderr.take() { drain(reader, stderr.clone()); }
        Ok(Self { subject: spec.subject, pid, creation, child: Mutex::new(child), retired: Mutex::new(false), stdout, stderr })
    }
    pub fn pid(&self) -> u32 { self.pid }
    pub fn creation_time_100ns(&self) -> u64 { self.creation }
    pub fn stdout(&self) -> &Capture { &self.stdout }
    pub fn stderr(&self) -> &Capture { &self.stderr }
    pub fn try_wait(&self) -> Result<Option<u32>, String> {
        self.child.lock().try_wait().map(|s| s.map(|s| s.code().unwrap_or(128) as u32)).map_err(|e| e.to_string())
    }
    pub fn wait_timeout(&self, timeout: Duration) -> Result<Option<u32>, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait()? { return Ok(Some(status)); }
            if Instant::now() >= deadline { return Ok(None); }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    pub fn active_processes(&self) -> Result<u32, String> { Ok(members(self.pid)?.len() as u32) }
    pub fn terminate(&self) -> Result<(), String> {
        let mut retired = self.retired.lock();
        if *retired { return Ok(()); }
        let deadline = Instant::now() + RETIRE_BOUND;
        // Hold the unreaped child PID until the first group signal, preventing PID reuse in the live-root case.
        unsafe { libc::kill(-(self.pid as i32), libc::SIGTERM); }
        loop {
            let live = members(self.pid)?;
            if live.is_empty() {
                // An empty list means the root is gone or a zombie; never block in wait() on a live process.
                self.wait_timeout(deadline.saturating_duration_since(Instant::now()))?
                    .ok_or_else(|| format!("{} process retirement could not be verified", self.subject))?;
                *retired = true;
                return Ok(());
            }
            let kill = Instant::now() + Duration::from_millis(100) >= deadline || deadline.saturating_duration_since(Instant::now()) < Duration::from_secs(4);
            if kill { for pid in live { unsafe { libc::kill(pid, libc::SIGKILL); } } }
            if Instant::now() >= deadline { return Err(format!("{} process retirement could not be verified", self.subject)); }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for ContainedChild { fn drop(&mut self) { let _ = self.terminate(); } }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retirement_ends_background_children_and_releases_the_captured_streams() {
        let child = ContainedChild::spawn(ContainedSpec { subject: "containment smoke".into(), program: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 300 & echo CHILD=$!; wait".into()], cwd: None, env_set: vec![], env_remove: vec![],
            stdout: StdioMode::Capture, stderr: StdioMode::Capture, capture_limit: 1024 }).unwrap();
        let descendant = child.stdout().wait_match(Duration::from_secs(5), |text| text.trim().strip_prefix("CHILD=")?.parse::<u32>().ok()).unwrap();
        assert!(process_creation_time(descendant).is_some());
        child.terminate().unwrap();
        assert_eq!(child.active_processes().unwrap(), 0);
        assert!(process_creation_time(descendant).is_none());
    }
}
