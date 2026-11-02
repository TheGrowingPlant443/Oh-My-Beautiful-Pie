//! libproc and descriptor fcntl calls have no standard-library equivalent; unsafe stays inside read-only probes.
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io::{self, Read};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::native_macos_parser::{self, ParsedLsof};
use crate::host_monitor::snapshot::{RawCollection, RawProcess, Recheck};
use crate::host_monitor::types::{Availability, ProcessIdentity, SectionState, Sections};

const CANCELLED: &str = "monitor_cancelled";
const MAX_PROCESSES: usize = 262_144;
const MAX_OUTPUT: usize = 16 * 1024 * 1024;
const MAX_STDERR: usize = 64 * 1024;
const LSOF_TIMEOUT: Duration = Duration::from_secs(5);

fn cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err(CANCELLED.into())
    } else {
        Ok(())
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0)
}

fn creation(info: &libc::proc_bsdinfo) -> u64 {
    // Identical to process_tree_unix::process_creation_time: epoch seconds plus microseconds in 100 ns units.
    info.pbi_start_tvsec
        .saturating_mul(10_000_000)
        .saturating_add(info.pbi_start_tvusec.saturating_mul(10))
}

enum Probe {
    Running(libc::proc_bsdinfo),
    Exited(libc::proc_bsdinfo),
    Limited,
    Gone,
}

fn probe(pid: u32) -> Probe {
    let Some(pid) = i32::try_from(pid).ok().filter(|pid| *pid > 0) else {
        return Probe::Limited;
    };
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: the buffer is writable for exactly `size` bytes, and only read after a full successful result.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return if read <= 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            Probe::Gone
        } else {
            Probe::Limited
        };
    }
    // SAFETY: proc_pidinfo returned the exact struct size and therefore initialized every field.
    let info = unsafe { info.assume_init() };
    if info.pbi_status == libc::SZOMB {
        Probe::Exited(info)
    } else {
        Probe::Running(info)
    }
}

pub fn capture_identity(pid: u32) -> Option<ProcessIdentity> {
    match probe(pid) {
        Probe::Running(info) => Some(ProcessIdentity {
            pid,
            creation_time_100ns: creation(&info),
        }),
        _ => None,
    }
}

fn process_ids() -> Result<(Vec<u32>, bool), String> {
    // SAFETY: a null buffer queries the current process count; subsequent buffers are sized in bytes.
    let estimate = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if estimate <= 0 {
        return Err(format!(
            "Process enumeration failed: {}",
            io::Error::last_os_error()
        ));
    }
    let mut ids = vec![0i32; (estimate as usize).saturating_add(64).min(MAX_PROCESSES)];
    for attempt in 0..3 {
        // SAFETY: ids owns initialized writable i32 storage; the supplied byte count is exactly its length.
        let count = unsafe {
            libc::proc_listallpids(
                ids.as_mut_ptr().cast(),
                (ids.len() * std::mem::size_of::<i32>()) as i32,
            )
        };
        if count <= 0 {
            return Err(format!(
                "Process enumeration failed: {}",
                io::Error::last_os_error()
            ));
        }
        let full = count as usize >= ids.len();
        if full && attempt < 2 && ids.len() < MAX_PROCESSES {
            ids.resize(ids.len().saturating_mul(2).min(MAX_PROCESSES), 0);
            continue;
        }
        ids.truncate((count as usize).min(ids.len()));
        let mut ids: Vec<u32> = ids
            .into_iter()
            .filter_map(|pid| u32::try_from(pid).ok().filter(|pid| *pid > 0))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        return Ok((ids, full));
    }
    unreachable!("bounded process enumeration returns on its final attempt")
}

fn process_name(info: &libc::proc_bsdinfo) -> String {
    let bytes = if info.pbi_name[0] != 0 {
        &info.pbi_name[..]
    } else {
        &info.pbi_comm[..]
    };
    let length = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    // SAFETY: c_char and u8 have identical size/alignment; this borrows only the initialised name bytes.
    let bytes = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), length) };
    String::from_utf8_lossy(bytes).into_owned()
}

fn thread_count(pid: u32) -> Option<u32> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
    // SAFETY: same full-sized-buffer contract as `probe`; PIDs came from the kernel and fit pid_t.
    if unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    } != size
    {
        return None;
    }
    // SAFETY: the full-size successful proc_pidinfo call initialized every field.
    u32::try_from(unsafe { info.assume_init() }.pti_threadnum).ok()
}

/// One process row and whether it counts as limited; `None` when the process is gone.
fn process_row(pid: u32) -> Option<(RawProcess, bool)> {
    let (info, exited) = match probe(pid) {
        Probe::Running(info) => (info, false),
        Probe::Exited(info) => (info, true),
        Probe::Gone => return None,
        Probe::Limited => {
            return Some((
                RawProcess {
                    pid,
                    parent_pid: 0,
                    threads: 0,
                    name: "(unreadable)".into(),
                    creation: None,
                    availability: Availability::Limited,
                },
                true,
            ));
        }
    };
    let threads = if exited { Some(0) } else { thread_count(pid) };
    // Guard the separate task-info query against PID reuse before attaching its thread count.
    let stable = exited
        || matches!(&probe(pid), Probe::Running(after) if creation(after) == creation(&info));
    let readable = stable && threads.is_some();
    Some((
        RawProcess {
            pid,
            parent_pid: info.pbi_ppid,
            threads: if stable { threads.unwrap_or(0) } else { 0 },
            name: process_name(&info),
            creation: Some(creation(&info)),
            availability: if exited {
                Availability::Exited
            } else if readable {
                Availability::Available
            } else {
                Availability::Limited
            },
        },
        !readable,
    ))
}

fn processes(cancel: &AtomicBool) -> Result<(Vec<RawProcess>, SectionState), String> {
    let (ids, truncated) = match process_ids() {
        Ok(ids) => ids,
        Err(error) => return Ok((Vec::new(), SectionState::unavailable(error))),
    };
    let mut rows = Vec::with_capacity(ids.len());
    let mut limited = 0;
    for (index, pid) in ids.into_iter().enumerate() {
        if index % 32 == 0 {
            cancelled(cancel)?;
        }
        let Some((row, is_limited)) = process_row(pid) else {
            continue;
        };
        if is_limited {
            limited += 1;
        }
        rows.push(row);
    }
    let state = if truncated || limited > 0 {
        SectionState::partial(format!("macOS process scan: {limited} processes had inaccessible or changing metadata; enumeration truncated: {truncated}"))
    } else {
        SectionState::ok()
    };
    Ok((rows, state))
}

/// Own the child until reaped, including on cancellation, pipe errors and output/time bounds.
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn nonblocking(fd: i32) -> io::Result<()> {
    // SAFETY: fcntl accepts an OS descriptor and scalar flags, and never accesses Rust memory.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: F_SETFL also receives only a descriptor and scalar flags; a failed F_GETFL skips this call.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn drain(reader: &mut impl Read, bytes: &mut Vec<u8>, limit: usize) -> io::Result<(bool, bool)> {
    let mut buffer = [0u8; 8192];
    // A bounded amount per tick keeps cancellation responsive even while lsof continuously writes.
    for _ in 0..32 {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok((true, false)),
            Ok(count) => {
                let keep = count.min(limit.saturating_sub(bytes.len()));
                bytes.extend_from_slice(&buffer[..keep]);
                if keep < count || bytes.len() == limit {
                    return Ok((false, true));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok((false, false)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok((false, false))
}

fn resolve_scope(zone: &str) -> Option<u32> {
    let zone = CString::new(zone).ok()?;
    // SAFETY: the interface name is a NUL-terminated string. This is a local interface lookup, not DNS.
    Some(unsafe { libc::if_nametoindex(zone.as_ptr()) }).filter(|index| *index != 0)
}

/// Reads one bounded chunk and reports `reason` when the pipe reached its limit.
fn drain_limit(
    reader: &mut impl Read,
    bytes: &mut Vec<u8>,
    limit: usize,
    done: &mut bool,
    reason: &'static str,
) -> io::Result<Option<&'static str>> {
    let (finished, full) = drain(reader, bytes, limit)?;
    *done |= finished;
    Ok(full.then_some(reason))
}

fn poll_exit(child: &mut Child, status: &mut Option<ExitStatus>) -> io::Result<()> {
    if status.is_none() {
        *status = child.try_wait()?;
    }
    Ok(())
}

type LsofRun = (Vec<u8>, bool, Option<ExitStatus>, Option<&'static str>);

/// Polls lsof's pipes and exit until it ends or a cancellation/output/time bound stops it.
fn read_lsof(
    cancel: &AtomicBool,
    child: &mut Child,
    stdout: &mut ChildStdout,
    stderr: &mut ChildStderr,
) -> io::Result<LsofRun> {
    use std::os::fd::AsRawFd;
    nonblocking(stdout.as_raw_fd())?;
    nonblocking(stderr.as_raw_fd())?;
    let (mut output, mut errors) = (Vec::new(), Vec::new());
    let (mut out_done, mut err_done) = (false, false);
    let mut status = None;
    let timer = Instant::now();
    let reason = loop {
        if cancel.load(Ordering::Acquire) {
            break Some("cancelled");
        }
        if let Some(reason) = drain_limit(
            stdout,
            &mut output,
            MAX_OUTPUT,
            &mut out_done,
            "output limit reached",
        )? {
            break Some(reason);
        }
        if let Some(reason) = drain_limit(
            stderr,
            &mut errors,
            MAX_STDERR,
            &mut err_done,
            "diagnostic limit reached",
        )? {
            break Some(reason);
        }
        poll_exit(child, &mut status)?;
        if status.is_some() && out_done && err_done {
            break None;
        }
        if timer.elapsed() >= LSOF_TIMEOUT {
            break Some("five-second time limit reached");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok((output, !errors.is_empty(), status, reason))
}

fn sockets(cancel: &AtomicBool) -> Result<(ParsedLsof, SectionState), String> {
    cancelled(cancel)?;
    // Absolute executable and fixed arguments: no shell, DNS, service lookup, network request or privilege prompt.
    let child = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-b", "-i", "-F0pftPnT", "-Ts"])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(error) => {
            return Ok((
                ParsedLsof::default(),
                SectionState::unavailable(format!(
                    "macOS socket scan could not start lsof: {error}"
                )),
            ))
        }
    };
    let mut child = ChildGuard(child);
    let mut stdout = child.0.stdout.take().expect("requested stdout pipe");
    let mut stderr = child.0.stderr.take().expect("requested stderr pipe");
    let result = read_lsof(cancel, &mut child.0, &mut stdout, &mut stderr);
    // Reap before parsing or returning; Drop covers every earlier error path too.
    drop(child);
    cancelled(cancel)?;
    let (output, diagnostics, status, reason) = match result {
        Ok(result) => result,
        Err(error) => {
            return Ok((
                ParsedLsof::default(),
                SectionState::unavailable(format!("macOS socket scan pipe failed: {error}")),
            ))
        }
    };
    let parsed = native_macos_parser::parse_lsof(&output, resolve_scope);
    let failed = status.map(|status| !status.success()).unwrap_or(true);
    let detail = format!(
        "macOS lsof exposes only visible descriptors; protected/other-user sockets may be omitted. Skipped malformed records: {}; diagnostics: {}; nonzero/incomplete exit: {}{}",
        parsed.malformed, diagnostics, failed, reason.map(|reason| format!("; {reason}")).unwrap_or_default()
    );
    // Exit 1 with no output can mean no matching sockets, but cannot prove an empty whole-host table.
    let state = if output.is_empty() && (reason.is_some() || failed && diagnostics) {
        SectionState::unavailable(detail)
    } else {
        SectionState::partial(detail)
    };
    Ok((parsed, state))
}

pub fn collect(cancel: &AtomicBool) -> Result<RawCollection, String> {
    let timer = Instant::now();
    let started_at = now_ms();
    cancelled(cancel)?;
    let (processes, process_state) = processes(cancel)?;
    let (sockets, socket_state) = sockets(cancel)?;
    let attributed: HashSet<u32> = sockets
        .tcp
        .iter()
        .chain(&sockets.udp)
        .map(|socket| socket.pid)
        .collect();
    let mut rechecks = HashMap::with_capacity(attributed.len());
    for (index, pid) in attributed.into_iter().enumerate() {
        if index % 32 == 0 {
            cancelled(cancel)?;
        }
        rechecks.insert(
            pid,
            match probe(pid) {
                Probe::Running(info) => Recheck::Creation(creation(&info)),
                Probe::Exited(_) | Probe::Gone => Recheck::Gone,
                Probe::Limited => Recheck::Unreadable,
            },
        );
    }
    cancelled(cancel)?;
    Ok(RawCollection {
        started_at,
        completed_at: now_ms(),
        duration_ms: timer.elapsed().as_millis() as u64,
        processes,
        tcp: sockets.tcp,
        udp: sockets.udp,
        rechecks,
        sections: Sections {
            processes: process_state,
            tcp4: socket_state.clone(),
            tcp6: socket_state.clone(),
            udp4: socket_state.clone(),
            udp6: socket_state,
        },
    })
}
