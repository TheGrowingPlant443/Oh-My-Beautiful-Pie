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
}

impl Pty {
    pub fn spawn(definition: &Definition, bridge: &BridgeLaunch) -> Result<Self, String> {
        let input = &definition.input;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: DEFAULT_ROWS,
                cols: DEFAULT_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
        let program = std::env::var_os("OMP_BIN").unwrap_or_else(|| "omp".into());
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
        if let Some(path) = &definition.session_path {
            cmd.arg("--resume");
            cmd.arg(path);
        }
        cmd.arg("--extension");
        cmd.arg(&bridge.extension_path);
        if definition.kind == crate::instance::InstanceKind::Manager {
            cmd.arg("--extension");
            cmd.arg(&bridge.manager_setup_dir);
            cmd.env("OMP_MANAGER_SETUP_DIR", &bridge.manager_setup_dir);
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
        let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
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
        })
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
        self.child.kill().map_err(|e| e.to_string())?;
        self.exit = Some(self.child.wait().map_err(|e| e.to_string())?.exit_code());
        Ok(())
    }
}
impl Drop for Pty {
    fn drop(&mut self) {
        if self.exit.is_none() {
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
}
