use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::instance::{Definition, InstanceKind};
use crate::manager::BridgeLaunch;

pub const DEFAULT_COLS: u16 = 120;
pub const DEFAULT_ROWS: u16 = 32;
const REPLAY_LIMIT: usize = 256 * 1024;
type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

#[derive(Default)]
struct CursorReplies {
    bytes: Vec<u8>,
}
impl vt100::Callbacks for CursorReplies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        action: char,
    ) {
        if action != 'n'
            || i2.is_some()
            || !matches!(i1, None | Some(b'?'))
            || params != [&[6u16][..]]
        {
            return;
        }
        let (row, col) = screen.cursor_position();
        let private = if i1 == Some(b'?') { "?" } else { "" };
        let _ = write!(self.bytes, "\x1b[{private}{};{}R", row + 1, col + 1);
    }
}

struct Output {
    bytes: VecDeque<u8>,
    channel: Option<Channel<InvokeResponseBody>>,
    parser: vt100::Parser<CursorReplies>,
}
impl Output {
    fn new() -> Self {
        Self {
            bytes: VecDeque::new(),
            channel: None,
            parser: vt100::Parser::new_with_callbacks(
                DEFAULT_ROWS,
                DEFAULT_COLS,
                0,
                CursorReplies::default(),
            ),
        }
    }
}
struct Shared {
    output: Mutex<Output>,
    last_output: AtomicU64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalScreen {
    pub text: String,
    pub cols: u16,
    pub rows: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Writer,
    child: Box<dyn Child + Send + Sync>,
    shared: Arc<Shared>,
    pub cols: u16,
    pub rows: u16,
    exit: Option<u32>,
    /// PID plus creation time captured while the PTY owns the child handle; read-only attribution for the host monitor.
    root: Option<crate::host_monitor::ProcessIdentity>,
}

impl Pty {
    pub fn spawn(definition: &Definition, bridge: &BridgeLaunch) -> Result<Self, String> {
        let input = &definition.input;

        let program = crate::toolchain::omp_program();
        let mut cmd = CommandBuilder::new(program);
        cmd.cwd(&input.project_path);
        cmd.args(["--cwd", &input.project_path]);
        cmd.args([
            "--profile",
            if input.profile.is_empty() {
                "default"
            } else {
                &input.profile
            },
        ]);
        for config in &input.configs {
            cmd.arg("--config");
            cmd.arg(config);
        }
        if !input.model.is_empty() {
            cmd.args(["--model", &input.model]);
        }
        // OMP creates sessions lazily: a path reported before the first message has no file to resume.
        if let Some(path) = definition.session_path.as_ref().filter(|p| p.is_file()) {
            cmd.arg("--resume");
            cmd.arg(path);
        }
        if let Some(path) = &input.agent_file {
            cmd.arg("--append-system-prompt");
            cmd.arg(path);
        }
        // A helper agent (Code diff / Git tree) has no built-in tools at all; its tools are the bridge's repo_inspect and code_quality_report.
        let helper = definition.helper.as_ref();
        if helper.is_some() {
            cmd.arg("--no-tools");
        }
        cmd.arg("--extension");
        cmd.arg(&bridge.extension_path);
        cmd.env_remove("OMP_MANAGER_PRIVATE_SETUP_DIR");
        cmd.env_remove("OMP_MANAGER_PRIVATE_SETUP_ROLE");
        let private_setup = match definition.kind {
            InstanceKind::Manager => Some((&bridge.manager_setup_dir, "manager")),
            InstanceKind::Architect => Some((&bridge.architect_setup_dir, "architect")),
            _ => None,
        };
        if let Some((root, role)) = private_setup {
            cmd.arg("--extension");
            cmd.arg(root);
            cmd.env("OMP_MANAGER_PRIVATE_SETUP_DIR", root);
            cmd.env("OMP_MANAGER_PRIVATE_SETUP_ROLE", role);
            // Private packages remain confined to their coordinator's directory.
            for package in crate::setup::private_plugin_paths(root, definition.kind)? {
                cmd.arg("--extension");
                cmd.arg(package);
            }
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("WT_SESSION", &definition.id);
        cmd.env("TERM_SESSION_ID", &definition.id);
        cmd.env("OMP_MANAGER_BRIDGE_ENDPOINT", &bridge.endpoint);
        cmd.env("OMP_MANAGER_BRIDGE_TOKEN", &bridge.token);
        cmd.env("OMP_MANAGER_BRIDGE_INSTANCE_ID", &bridge.instance_id);
        cmd.env(
            "OMP_MANAGER_BRIDGE_GENERATION",
            bridge.generation.to_string(),
        );
        cmd.env(
            "OMP_MANAGER_BRIDGE_ROLE",
            if bridge.role == InstanceKind::Manager {
                "manager"
            } else {
                "instance"
            },
        );
        if definition.kind == InstanceKind::Architect {
            cmd.env("OMP_MANAGER_ARCHITECT_DIR", &bridge.architect_dir);
        }
        if let Some(link) = helper {
            cmd.env("OMP_MANAGER_HELPER_REPO", &link.repo);
        } else {
            cmd.env("OMP_MANAGER_BROWSER_REQUIRED", "1");
            // Both are required for command capture; without the SDK the bridge reports the feature as unavailable.
            if let Some(sdk) = &bridge.sdk_dir {
                cmd.env("OMP_MANAGER_TERMINAL_DIR", &bridge.terminal_dir);
                cmd.env("OMP_MANAGER_SDK_DIR", sdk);
            }
        }
        if definition.plan_target.is_some() {
            cmd.env("OMP_MANAGER_PLAN_SESSION", "1");
        }
        cmd.env("PI_BROWSER_RELAY", "0");
        cmd.env("PI_BROWSER_TERN", "0");
        cmd.env("PI_BROWSER_CMUX", "0");
        Self::start(cmd)
    }
    /// The user's own shell: no OMP arguments, no bridge credentials, nothing from the manager environment.
    pub fn spawn_shell(cwd: &str) -> Result<Self, String> {
        let mut cmd = CommandBuilder::new_default_prog();
        cmd.cwd(cwd);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("OMP_MANAGER_") {
                cmd.env_remove(name);
            }
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        Self::start(cmd)
    }
    fn start(cmd: CommandBuilder) -> Result<Self, String> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: DEFAULT_ROWS,
                cols: DEFAULT_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
        let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let root = child.process_id().and_then(crate::host_monitor::capture_identity);
        let writer: Writer = Arc::new(Mutex::new(
            pair.master.take_writer().map_err(|e| e.to_string())?,
        ));
        let shared = Arc::new(Shared {
            output: Mutex::new(Output::new()),
            last_output: AtomicU64::new(0),
        });
        let pump = shared.clone();
        let reply_writer = writer.clone();
        std::thread::spawn(move || drain(reader, pump, reply_writer));
        Ok(Self {
            master: pair.master,
            writer,
            child,
            shared,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            exit: None,
            root,
        })
    }
    pub fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }
    /// Creation identity of the launched root, captured at launch; `None` when it could not be read.
    pub fn root_identity(&self) -> Option<crate::host_monitor::ProcessIdentity> {
        self.root
    }
    /// Immediate termination for a shell the user closed; OMP uses `stop` for a graceful Ctrl-C first.
    pub fn kill(&mut self) -> Result<(), String> {
        if self.poll()?.is_some() {
            return Ok(());
        }
        self.kill_process_group();
        self.child.kill().map_err(|e| e.to_string())?;
        self.exit = Some(self.child.wait().map_err(|e| e.to_string())?.exit_code());
        Ok(())
    }
    /// The child is a session and process-group leader (portable-pty calls `setsid`), so one `killpg` ends everything it
    /// started that stayed in the group, the Unix counterpart of the Windows job. Only called while the leader is still
    /// unreaped, so its pid cannot have been recycled.
    fn kill_process_group(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.process_id().and_then(|pid| libc::pid_t::try_from(pid).ok()).filter(|pid| *pid > 1) {
            // SAFETY: plain signal delivery to a process group id this PTY created; failure (ESRCH) means it is already gone.
            unsafe { libc::killpg(pid, libc::SIGKILL) };
        }
    }
    pub fn last_output(&self) -> u64 {
        self.shared.last_output.load(Ordering::Relaxed)
    }
    pub fn poll(&mut self) -> Result<Option<u32>, String> {
        if self.exit.is_none() {
            self.exit = self
                .child
                .try_wait()
                .map_err(|e| e.to_string())?
                .map(|s| s.exit_code());
        }
        Ok(self.exit)
    }
    pub fn write(&mut self, data: &str) -> Result<(), String> {
        if self.poll()?.is_some() {
            return Err("instance not running".into());
        }
        let mut writer = self.writer.lock().map_err(|e| e.to_string())?;
        writer
            .write_all(data.as_bytes())
            .and_then(|_| writer.flush())
            .map_err(|e| e.to_string())
    }
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        if !(2..=500).contains(&cols) || !(2..=200).contains(&rows) {
            return Err("invalid terminal dimensions".into());
        }
        if self.cols == cols && self.rows == rows {
            return Ok(());
        }
        self.master
            .resize(PtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
        self.shared
            .output
            .lock()
            .map_err(|e| e.to_string())?
            .parser
            .screen_mut()
            .set_size(rows, cols);
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }
    pub fn screen(&self) -> Result<TerminalScreen, String> {
        let output = self.shared.output.lock().map_err(|e| e.to_string())?;
        let screen = output.parser.screen();
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        Ok(TerminalScreen {
            text: screen.contents(),
            cols,
            rows,
            cursor_row,
            cursor_col,
        })
    }
    pub fn attach(&self, channel: Channel<InvokeResponseBody>) -> Result<(), String> {
        let mut output = self.shared.output.lock().map_err(|e| e.to_string())?;
        channel
            .send(InvokeResponseBody::Raw(
                output.bytes.iter().copied().collect(),
            ))
            .map_err(|e| e.to_string())?;
        output.channel = Some(channel);
        Ok(())
    }
    pub fn stop(&mut self) -> Result<(), String> {
        if self.poll()?.is_some() {
            return Ok(());
        }
        let _ = self.write("\x03");
        std::thread::sleep(Duration::from_millis(300));
        let _ = self.write("\x03");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.poll()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.kill_process_group();
        self.child.kill().map_err(|e| e.to_string())?;
        self.exit = Some(self.child.wait().map_err(|e| e.to_string())?.exit_code());
        Ok(())
    }
}
impl Drop for Pty {
    fn drop(&mut self) {
        if self.exit.is_none() {
            self.kill_process_group();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn drain(mut reader: Box<dyn Read + Send>, shared: Arc<Shared>, writer: Writer) {
    let mut buffer = [0u8; 8192];
    let mut replies = Vec::new();
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        {
            let mut output = match shared.output.lock() {
                Ok(output) => output,
                Err(e) => {
                    eprintln!("terminal output lock: {e}");
                    break;
                }
            };
            output.parser.process(&buffer[..read]);
            std::mem::swap(&mut replies, &mut output.parser.callbacks_mut().bytes);
            output.bytes.extend(&buffer[..read]);
            let excess = output.bytes.len().saturating_sub(REPLAY_LIMIT);
            output.bytes.drain(..excess);
            shared
                .last_output
                .store(crate::session::now_ms(), Ordering::Relaxed);
            if let Some(channel) = &output.channel {
                if channel
                    .send(InvokeResponseBody::Raw(buffer[..read].to_vec()))
                    .is_err()
                {
                    output.channel = None;
                }
            }
        }
        if !replies.is_empty() {
            if let Ok(mut writer) = writer.lock() {
                let _ = writer.write_all(&replies).and_then(|_| writer.flush());
            }
            replies.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn screen_and_cursor_queries_survive_split_input() {
        let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, CursorReplies::default());
        parser.process("café".as_bytes());
        parser.process(b"\x1b[");
        parser.process(b"6n");
        assert_eq!(parser.screen().contents(), "café");
        assert_eq!(parser.callbacks().bytes, b"\x1b[1;5R");
        parser.callbacks_mut().bytes.clear();
        parser.process(b"\x1b[?6n");
        assert_eq!(parser.callbacks().bytes, b"\x1b[?1;5R");
        parser.screen_mut().set_size(40, 100);
        assert_eq!(parser.screen().size(), (40, 100));
    }

    /// Everything the shell started that stayed in its process group ends with it (the Windows job's Unix counterpart).
    #[cfg(unix)]
    #[test]
    fn killing_a_shell_ends_the_background_processes_it_started() {
        let folder = std::env::temp_dir();
        let mut shell = Pty::spawn_shell(folder.to_str().unwrap()).unwrap();
        shell.write("sh -c 'sleep 600 & echo CHILD_PID=$!; wait'\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let child: libc::pid_t = loop {
            let text = shell.screen().unwrap().text;
            let found = text.lines().filter_map(|line| line.strip_prefix("CHILD_PID=")).find_map(|pid| pid.trim().parse().ok());
            if let Some(pid) = found {
                break pid;
            }
            assert!(Instant::now() < deadline, "the shell never printed the child pid: {text:?}");
            std::thread::sleep(Duration::from_millis(50));
        };
        // SAFETY: signal 0 only probes for existence.
        assert_eq!(unsafe { libc::kill(child, 0) }, 0, "the background process is alive before the kill");
        shell.kill().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        // The orphaned sleep is reparented and reaped by init/subreaper; wait for it to disappear.
        while unsafe { libc::kill(child, 0) } == 0 && !is_zombie(child) {
            assert!(Instant::now() < deadline, "the background process survived the shell's kill");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[cfg(unix)]
    fn is_zombie(pid: libc::pid_t) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| stat.rsplit(')').next().is_some_and(|rest| rest.trim_start().starts_with('Z')))
    }
}
