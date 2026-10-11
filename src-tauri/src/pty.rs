use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::instance::Definition;

pub const DEFAULT_COLS: u16 = 120;
pub const DEFAULT_ROWS: u16 = 32;
const REPLAY_LIMIT: usize = 256 * 1024;

#[derive(Default)]
struct Output {
    bytes: VecDeque<u8>,
    channel: Option<Channel<InvokeResponseBody>>,
}

#[derive(Default)]
struct Shared {
    output: Mutex<Output>,
    last_output: AtomicU64,
}

pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    shared: Arc<Shared>,
    pub cols: u16,
    pub rows: u16,
    exit: Option<u32>,
}

impl Pty {
    pub fn spawn(definition: &Definition) -> Result<Self, String> {
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
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        // Preserve native breadcrumb isolation across ConPTY instances.
        cmd.env("WT_SESSION", &definition.id);
        cmd.env("TERM_SESSION_ID", &definition.id);
        let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        let shared = Arc::new(Shared::default());
        let pump = shared.clone();
        std::thread::spawn(move || drain(reader, pump));
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
        self.writer
            .write_all(data.as_bytes())
            .and_then(|_| self.writer.flush())
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
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    pub fn attach(&self, channel: Channel<InvokeResponseBody>) -> Result<(), String> {
        // Hold the stream lock across replay and binding; no byte can be lost or duplicated.
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

fn drain(mut reader: Box<dyn Read + Send>, shared: Arc<Shared>) {
    let mut buffer = [0u8; 8192];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let mut output = shared.output.lock().unwrap_or_else(|e| e.into_inner());
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
}
