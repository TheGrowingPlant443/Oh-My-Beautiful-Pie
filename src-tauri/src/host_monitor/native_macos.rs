//! macOS process metadata and numeric lsof field decoding. The decoder is also compiled by Linux tests.
//! lsof sees descriptors, not a guaranteed complete kernel socket table: every successful socket section is
//! deliberately partial, even as root (SIP/TCC and process races can hide evidence). No elevation is attempted.
use std::net::{Ipv4Addr, Ipv6Addr};

use crate::host_monitor::tables::RawSocket;
use crate::host_monitor::types::{Endpoint, Family, Protocol};

#[derive(Default)]
struct FileRecord<'a> {
    family: Option<Family>,
    protocol: Option<Protocol>,
    name: Option<&'a str>,
    state: Option<&'a str>,
    invalid: bool,
}

#[derive(Default)]
struct ParsedLsof {
    tcp: Vec<RawSocket>,
    udp: Vec<RawSocket>,
    malformed: usize,
}

fn endpoint(text: &str, family: Family, resolve_scope: fn(&str) -> Option<u32>) -> Option<Endpoint> {
    let (address, port) = text.rsplit_once(':')?;
    let port = port.parse::<u16>().ok()?;
    let address = address.strip_prefix('[').and_then(|value| value.strip_suffix(']')).unwrap_or(address);
    let (address, scope_id) = match address.rsplit_once('%') {
        Some((address, zone)) if family == Family::Ipv6 => {
            let scope = zone.parse::<u32>().ok().or_else(|| resolve_scope(zone))?;
            (address, Some(scope).filter(|scope| *scope != 0))
        }
        _ => (address, None),
    };
    let address = match (family, address) {
        (Family::Ipv4, "*") => Ipv4Addr::UNSPECIFIED.to_string(),
        (Family::Ipv6, "*") => Ipv6Addr::UNSPECIFIED.to_string(),
        (Family::Ipv4, value) => value.parse::<Ipv4Addr>().ok()?.to_string(),
        (Family::Ipv6, value) => value.parse::<Ipv6Addr>().ok()?.to_string(),
    };
    Some(Endpoint { address, port, scope_id })
}

/// lsof names are not kernel/MIB state numbers; only these observed states become direction evidence.
fn tcp_state(state: &str) -> Option<u32> {
    Some(match state {
        "CLOSED" => 1,
        "LISTEN" => 2,
        "SYN_SENT" => 3,
        "SYN_RCVD" | "SYN_RECEIVED" => 4,
        "ESTABLISHED" => 5,
        "FIN_WAIT_1" => 6,
        "FIN_WAIT_2" => 7,
        "CLOSE_WAIT" => 8,
        "CLOSING" => 9,
        "LAST_ACK" => 10,
        "TIME_WAIT" => 11,
        _ => return None,
    })
}

fn finish_record(parsed: &mut ParsedLsof, pid: Option<u32>, record: FileRecord<'_>, resolve_scope: fn(&str) -> Option<u32>) {
    let row = (|| {
        if record.invalid { return None; }
        let pid = pid?;
        let family = record.family?;
        let protocol = record.protocol?;
        let name = record.name?;
        let (local, remote) = match name.split_once("->") {
            Some((local, remote)) => (endpoint(local, family, resolve_scope)?, Some(endpoint(remote, family, resolve_scope)?)),
            None => (endpoint(name, family, resolve_scope)?, None),
        };
        let tcp_state = if protocol == Protocol::Tcp { record.state.and_then(tcp_state) } else { None };
        Some(RawSocket { family, protocol, pid, local, remote, tcp_state })
    })();
    match row {
        Some(row) if row.protocol == Protocol::Tcp => parsed.tcp.push(row),
        Some(row) => parsed.udp.push(row),
        None => parsed.malformed += 1,
    }
}

/// A record boundary: `p` starts a process (and ends the open file record), `f` starts a file record.
fn start_section(parsed: &mut ParsedLsof, pid: &mut Option<u32>, record: &mut Option<FileRecord<'_>>, tag: u8, value: Result<&str, std::str::Utf8Error>, resolve_scope: fn(&str) -> Option<u32>) {
    if let Some(previous) = record.take() { finish_record(parsed, *pid, previous, resolve_scope); }
    if tag == b'f' {
        *record = Some(FileRecord::default());
        return;
    }
    *pid = value.ok().and_then(|value| value.parse::<u32>().ok()).filter(|pid| *pid > 0);
    if pid.is_none() { parsed.malformed += 1; }
}

fn apply_file_field<'a>(record: &mut FileRecord<'a>, tag: u8, value: Result<&'a str, std::str::Utf8Error>) {
    let Ok(value) = value else { record.invalid = true; return; };
    match tag {
        b't' => record.family = match value { "IPv4" => Some(Family::Ipv4), "IPv6" => Some(Family::Ipv6), _ => None },
        b'P' => record.protocol = match value { "TCP" => Some(Protocol::Tcp), "UDP" => Some(Protocol::Udp), _ => None },
        b'n' => record.name = Some(value),
        b'T' => if let Some(state) = value.strip_prefix("ST=") { record.state = Some(state); },
        _ => {}
    }
}

fn remote_order(a: &Option<Endpoint>, b: &Option<Endpoint>) -> std::cmp::Ordering {
    let key = |endpoint: &Endpoint| (endpoint.port, endpoint.scope_id);
    match (a, b) {
        (Some(a), Some(b)) => key(a).cmp(&key(b)).then_with(|| a.address.cmp(&b.address)),
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn socket_order(a: &RawSocket, b: &RawSocket) -> std::cmp::Ordering {
    let key = |row: &RawSocket| (row.pid, if row.family == Family::Ipv4 { 4 } else { 6 }, row.local.port, row.local.scope_id, row.tcp_state);
    key(a).cmp(&key(b)).then_with(|| a.local.address.cmp(&b.local.address)).then_with(|| remote_order(&a.remote, &b.remote))
}

/// `-F0pftPnT` emits NUL-terminated fields, with newlines between process/file sets. File boundaries, not
/// newlines, delimit records. Unknown T fields (queues, etc.) never become TCP state or direction evidence.
fn parse_lsof(bytes: &[u8], resolve_scope: fn(&str) -> Option<u32>) -> ParsedLsof {
    let mut parsed = ParsedLsof::default();
    let mut pid = None;
    let mut record: Option<FileRecord<'_>> = None;
    for field in bytes.split_inclusive(|byte| *byte == 0) {
        let field = field.strip_prefix(b"\n").unwrap_or(field);
        if field.is_empty() { continue; }
        let Some(field) = field.strip_suffix(&[0]) else {
            parsed.malformed += 1;
            if let Some(record) = record.as_mut() { record.invalid = true; }
            break;
        };
        if field.is_empty() { continue; }
        let (tag, value) = (field[0], std::str::from_utf8(&field[1..]));
        if tag == b'p' || tag == b'f' {
            start_section(&mut parsed, &mut pid, &mut record, tag, value, resolve_scope);
        } else if let Some(record) = record.as_mut() {
            apply_file_field(record, tag, value);
        }
    }
    if let Some(record) = record { finish_record(&mut parsed, pid, record, resolve_scope); }
    // Several descriptors in one process may refer to the same socket. Keep one row per observed owner/endpoint.
    parsed.tcp.sort_unstable_by(socket_order);
    parsed.tcp.dedup();
    parsed.udp.sort_unstable_by(socket_order);
    parsed.udp.dedup();
    parsed
}

#[cfg(target_os = "macos")]
mod platform {
    use std::collections::{HashMap, HashSet};
    use std::ffi::CString;
    use std::io::{self, Read};
    use std::os::fd::AsRawFd;
    use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use crate::host_monitor::snapshot::{RawCollection, RawProcess, Recheck};
    use crate::host_monitor::types::*;
    use super::{parse_lsof, ParsedLsof};

    const CANCELLED: &str = "monitor_cancelled";
    const MAX_PROCESSES: usize = 262_144;
    const MAX_OUTPUT: usize = 16 * 1024 * 1024;
    const MAX_STDERR: usize = 64 * 1024;
    const LSOF_TIMEOUT: Duration = Duration::from_secs(5);

    fn cancelled(cancel: &AtomicBool) -> Result<(), String> {
        if cancel.load(Ordering::Acquire) { Err(CANCELLED.into()) } else { Ok(()) }
    }

    fn now_ms() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|value| value.as_millis() as u64).unwrap_or(0)
    }

    fn creation(info: &libc::proc_bsdinfo) -> u64 {
        // Identical to process_tree_unix::process_creation_time: epoch seconds plus microseconds in 100 ns units.
        info.pbi_start_tvsec.saturating_mul(10_000_000).saturating_add(info.pbi_start_tvusec.saturating_mul(10))
    }

    enum Probe { Running(libc::proc_bsdinfo), Exited(libc::proc_bsdinfo), Limited, Gone }

    fn probe(pid: u32) -> Probe {
        let Some(pid) = i32::try_from(pid).ok().filter(|pid| *pid > 0) else { return Probe::Limited; };
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        // SAFETY: the buffer is writable for exactly `size` bytes, and only read after a full successful result.
        let read = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
        if read != size {
            return if read <= 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) { Probe::Gone } else { Probe::Limited };
        }
        let info = unsafe { info.assume_init() };
        if info.pbi_status == libc::SZOMB { Probe::Exited(info) } else { Probe::Running(info) }
    }

    pub fn capture_identity(pid: u32) -> Option<ProcessIdentity> {
        match probe(pid) {
            Probe::Running(info) => Some(ProcessIdentity { pid, creation_time_100ns: creation(&info) }),
            _ => None,
        }
    }

    fn process_ids() -> Result<(Vec<u32>, bool), String> {
        // SAFETY: a null buffer queries the current process count; subsequent buffers are sized in bytes.
        let estimate = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if estimate <= 0 { return Err(format!("Process enumeration failed: {}", io::Error::last_os_error())); }
        let mut ids = vec![0i32; (estimate as usize).saturating_add(64).min(MAX_PROCESSES)];
        for attempt in 0..3 {
            let count = unsafe { libc::proc_listallpids(ids.as_mut_ptr().cast(), (ids.len() * std::mem::size_of::<i32>()) as i32) };
            if count <= 0 { return Err(format!("Process enumeration failed: {}", io::Error::last_os_error())); }
            let full = count as usize >= ids.len();
            if full && attempt < 2 && ids.len() < MAX_PROCESSES {
                ids.resize(ids.len().saturating_mul(2).min(MAX_PROCESSES), 0);
                continue;
            }
            ids.truncate((count as usize).min(ids.len()));
            let mut ids: Vec<u32> = ids.into_iter().filter_map(|pid| u32::try_from(pid).ok().filter(|pid| *pid > 0)).collect();
            ids.sort_unstable();
            ids.dedup();
            return Ok((ids, full));
        }
        unreachable!("bounded process enumeration returns on its final attempt")
    }

    fn process_name(info: &libc::proc_bsdinfo) -> String {
        let bytes = if info.pbi_name[0] != 0 { &info.pbi_name[..] } else { &info.pbi_comm[..] };
        let length = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
        // SAFETY: c_char and u8 have identical size/alignment; this borrows only the initialised name bytes.
        let bytes = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), length) };
        String::from_utf8_lossy(bytes).into_owned()
    }

    fn thread_count(pid: u32) -> Option<u32> {
        let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
        // SAFETY: same full-sized-buffer contract as `probe`; PIDs came from the kernel and fit pid_t.
        if unsafe { libc::proc_pidinfo(pid as i32, libc::PROC_PIDTASKINFO, 0, info.as_mut_ptr().cast(), size) } != size { return None; }
        u32::try_from(unsafe { info.assume_init() }.pti_threadnum).ok()
    }

    /// One process row and whether it counts as limited; `None` when the process is gone.
    fn process_row(pid: u32) -> Option<(RawProcess, bool)> {
        let (info, exited) = match probe(pid) {
            Probe::Running(info) => (info, false),
            Probe::Exited(info) => (info, true),
            Probe::Gone => return None,
            Probe::Limited => {
                return Some((RawProcess { pid, parent_pid: 0, threads: 0, name: "(unreadable)".into(), creation: None, availability: Availability::Limited }, true));
            }
        };
        let threads = if exited { Some(0) } else { thread_count(pid) };
        // Guard the separate task-info query against PID reuse before attaching its thread count.
        let stable = exited || matches!(&probe(pid), Probe::Running(after) if creation(after) == creation(&info));
        let readable = stable && threads.is_some();
        Some((RawProcess {
            pid, parent_pid: info.pbi_ppid, threads: if stable { threads.unwrap_or(0) } else { 0 },
            name: process_name(&info), creation: Some(creation(&info)),
            availability: if exited { Availability::Exited } else if readable { Availability::Available } else { Availability::Limited },
        }, !readable))
    }

    fn processes(cancel: &AtomicBool) -> Result<(Vec<RawProcess>, SectionState), String> {
        let (ids, truncated) = match process_ids() {
            Ok(ids) => ids,
            Err(error) => return Ok((Vec::new(), SectionState::unavailable(error))),
        };
        let mut rows = Vec::with_capacity(ids.len());
        let mut limited = 0;
        for (index, pid) in ids.into_iter().enumerate() {
            if index % 32 == 0 { cancelled(cancel)?; }
            let Some((row, is_limited)) = process_row(pid) else { continue };
            if is_limited { limited += 1; }
            rows.push(row);
        }
        let state = if truncated || limited > 0 {
            SectionState::partial(format!("macOS process scan: {limited} processes had inaccessible or changing metadata; enumeration truncated: {truncated}"))
        } else { SectionState::ok() };
        Ok((rows, state))
    }

    /// Own the child until reaped, including on cancellation, pipe errors and output/time bounds.
    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
    }

    fn nonblocking(fd: i32) -> io::Result<()> {
        // SAFETY: fcntl only changes flags on an owned, live pipe descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
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
                    if keep < count || bytes.len() == limit { return Ok((false, true)); }
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
    fn drain_limit(reader: &mut impl Read, bytes: &mut Vec<u8>, limit: usize, done: &mut bool, reason: &'static str) -> io::Result<Option<&'static str>> {
        let (finished, full) = drain(reader, bytes, limit)?;
        *done |= finished;
        Ok(full.then_some(reason))
    }

    fn poll_exit(child: &mut Child, status: &mut Option<ExitStatus>) -> io::Result<()> {
        if status.is_none() { *status = child.try_wait()?; }
        Ok(())
    }

    type LsofRun = (Vec<u8>, bool, Option<ExitStatus>, Option<&'static str>);

    /// Polls lsof's pipes and exit until it ends or a cancellation/output/time bound stops it.
    fn read_lsof(cancel: &AtomicBool, child: &mut Child, stdout: &mut ChildStdout, stderr: &mut ChildStderr) -> io::Result<LsofRun> {
        nonblocking(stdout.as_raw_fd())?;
        nonblocking(stderr.as_raw_fd())?;
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        let (mut out_done, mut err_done) = (false, false);
        let mut status = None;
        let timer = Instant::now();
        let reason = loop {
            if cancel.load(Ordering::Acquire) { break Some("cancelled"); }
            if let Some(reason) = drain_limit(stdout, &mut output, MAX_OUTPUT, &mut out_done, "output limit reached")? { break Some(reason); }
            if let Some(reason) = drain_limit(stderr, &mut errors, MAX_STDERR, &mut err_done, "diagnostic limit reached")? { break Some(reason); }
            poll_exit(child, &mut status)?;
            if status.is_some() && out_done && err_done { break None; }
            if timer.elapsed() >= LSOF_TIMEOUT { break Some("five-second time limit reached"); }
            std::thread::sleep(Duration::from_millis(10));
        };
        Ok((output, !errors.is_empty(), status, reason))
    }

    fn sockets(cancel: &AtomicBool) -> Result<(ParsedLsof, SectionState), String> {
        cancelled(cancel)?;
        // Absolute executable and fixed arguments: no shell, DNS, service lookup, network request or privilege prompt.
        let child = Command::new("/usr/sbin/lsof")
            .args(["-nP", "-b", "-i", "-F0pftPnT", "-Ts"])
            .env("LC_ALL", "C").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
        let child = match child {
            Ok(child) => child,
            Err(error) => return Ok((ParsedLsof::default(), SectionState::unavailable(format!("macOS socket scan could not start lsof: {error}")))),
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
            Err(error) => return Ok((ParsedLsof::default(), SectionState::unavailable(format!("macOS socket scan pipe failed: {error}")))),
        };
        let parsed = parse_lsof(&output, resolve_scope);
        let failed = status.map(|status| !status.success()).unwrap_or(true);
        let detail = format!(
            "macOS lsof exposes only visible descriptors; protected/other-user sockets may be omitted. Skipped malformed records: {}; diagnostics: {}; nonzero/incomplete exit: {}{}",
            parsed.malformed, diagnostics, failed, reason.map(|reason| format!("; {reason}")).unwrap_or_default()
        );
        // Exit 1 with no output can mean no matching sockets, but cannot prove an empty whole-host table.
        let state = if output.is_empty() && (reason.is_some() || failed && diagnostics) {
            SectionState::unavailable(detail)
        } else { SectionState::partial(detail) };
        Ok((parsed, state))
    }

    pub fn collect(cancel: &AtomicBool) -> Result<RawCollection, String> {
        let timer = Instant::now();
        let started_at = now_ms();
        cancelled(cancel)?;
        let (processes, process_state) = processes(cancel)?;
        let (sockets, socket_state) = sockets(cancel)?;
        let attributed: HashSet<u32> = sockets.tcp.iter().chain(&sockets.udp).map(|socket| socket.pid).collect();
        let mut rechecks = HashMap::with_capacity(attributed.len());
        for (index, pid) in attributed.into_iter().enumerate() {
            if index % 32 == 0 { cancelled(cancel)?; }
            rechecks.insert(pid, match probe(pid) {
                Probe::Running(info) => Recheck::Creation(creation(&info)),
                Probe::Exited(_) | Probe::Gone => Recheck::Gone,
                Probe::Limited => Recheck::Unreadable,
            });
        }
        cancelled(cancel)?;
        Ok(RawCollection {
            started_at, completed_at: now_ms(), duration_ms: timer.elapsed().as_millis() as u64,
            processes, tcp: sockets.tcp, udp: sockets.udp, rechecks,
            sections: Sections {
                processes: process_state, tcp4: socket_state.clone(), tcp6: socket_state.clone(),
                udp4: socket_state.clone(), udp6: socket_state,
            },
        })
    }
}

#[cfg(target_os = "macos")]
pub use platform::{capture_identity, collect};

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(zone: &str) -> Option<u32> { if zone == "en0" { Some(7) } else { None } }

    #[test]
    fn parses_nul_fields_and_keeps_state_evidence_separate_from_peer_presence() {
        let parsed = parse_lsof(b"p123\0\nf9\0tIPv4\0PTCP\0n*:8080\0TST=LISTEN\0\nf10\0tIPv4\0PTCP\0n127.0.0.1:50123->127.0.0.1:8080\0TST=ESTABLISHED\0TQR=0\0\nf11\0tIPv4\0PTCP\0n10.0.0.1:1234->10.0.0.2:443\0TST=SYN_SENT\0\n", scope);
        assert_eq!(parsed.malformed, 0);
        assert_eq!(parsed.tcp.len(), 3);
        let listener = parsed.tcp.iter().find(|row| row.local.port == 8080).unwrap();
        assert_eq!(listener.pid, 123);
        assert_eq!(listener.local.address, "0.0.0.0");
        assert_eq!(listener.remote, None);
        assert_eq!(listener.tcp_state, Some(2));
        assert_eq!(parsed.tcp.iter().find(|row| row.local.port == 50123).unwrap().tcp_state, Some(5));
        assert_eq!(parsed.tcp.iter().find(|row| row.local.port == 1234).unwrap().tcp_state, Some(3));
    }

    #[test]
    fn parses_ipv6_scopes_udp_peers_and_process_boundaries() {
        let parsed = parse_lsof(b"p42\0\nf3\0tIPv6\0PUDP\0n[fe80::1%en0]:5353->[fe80::2%7]:5353\0\nf4\0tIPv6\0PUDP\0n*:123\0\np43\0\nf3\0tIPv6\0PTCP\0n[::1]:443->[::1]:60000\0TST=SYN_RCVD\0\n", scope);
        assert_eq!(parsed.malformed, 0);
        assert_eq!(parsed.udp.len(), 2);
        let udp = parsed.udp.iter().find(|row| row.local.port == 5353).unwrap();
        assert_eq!(udp.pid, 42);
        assert_eq!(udp.local.scope_id, Some(7));
        assert_eq!(udp.remote.as_ref().unwrap().scope_id, Some(7));
        assert_eq!(udp.tcp_state, None);
        assert_eq!(parsed.udp.iter().find(|row| row.local.port == 123).unwrap().local.address, "::");
        assert_eq!(parsed.tcp[0].pid, 43);
        assert_eq!(parsed.tcp[0].tcp_state, Some(4));
    }

    #[test]
    fn malformed_and_truncated_records_do_not_inherit_a_previous_owner_or_endpoint() {
        let parsed = parse_lsof(b"p12\0\nf1\0tIPv4\0PTCP\0n127.0.0.1:80\0TST=LISTEN\0\nf2\0tIPv4\0PTCP\0nlocalhost:80\0\npbad\0\nf3\0tIPv4\0PTCP\0n127.0.0.1:81\0\np13\0\nf4\0tIPv4\0PUDP\0n127.0.0.1:70000\0\nf5\0tIPv4\0PTCP\0n127.0.0.1:82\0TST=LIST", scope);
        assert_eq!(parsed.tcp.len(), 1);
        assert_eq!(parsed.tcp[0].pid, 12);
        assert!(parsed.udp.is_empty());
        assert!(parsed.malformed >= 4);
    }

    #[test]
    fn unknown_or_absent_states_are_not_fabricated_and_duplicate_descriptors_collapse() {
        let parsed = parse_lsof(b"p9\0\nf1\0tIPv4\0PTCP\0n127.0.0.1:80\0TST=NEW_STATE\0\nf2\0tIPv4\0PTCP\0n127.0.0.1:80\0\nf3\0tIPv4\0PUDP\0n127.0.0.1:53\0TST=LISTEN\0\n", scope);
        assert_eq!(parsed.malformed, 0);
        assert_eq!(parsed.tcp.len(), 1);
        assert_eq!(parsed.tcp[0].tcp_state, None);
        assert_eq!(parsed.udp[0].tcp_state, None);
    }

    #[test]
    fn endpoint_parser_rejects_dns_invalid_ports_and_unresolved_scopes() {
        assert!(endpoint("localhost:80", Family::Ipv4, scope).is_none());
        assert!(endpoint("127.0.0.1:*", Family::Ipv4, scope).is_none());
        assert!(endpoint("[::1]:65536", Family::Ipv6, scope).is_none());
        assert!(endpoint("[fe80::1%missing]:80", Family::Ipv6, scope).is_none());
        assert!(endpoint("[::1]:80", Family::Ipv4, scope).is_none());
        assert_eq!(endpoint("[2001:0db8::1]:443", Family::Ipv6, scope).unwrap().address, "2001:db8::1");
    }
}
