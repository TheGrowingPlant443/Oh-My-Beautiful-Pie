//! The clock-tick query requires sysconf; process and socket evidence otherwise uses safe /proc reads.
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufReader, ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::snapshot::{RawCollection, RawProcess, Recheck};
use super::tables::{self, ProcStat, RawSocket};
use super::types::{Availability, Family, ProcessIdentity, Protocol, SectionState, Sections};

pub const CANCELLED: &str = "monitor_cancelled";
const CANCEL_STRIDE: usize = 32;
/// Process rows kept per snapshot; `pid_max` can be in the millions but a real host has a few hundred.
const MAX_PROCESSES: usize = 262_144;
/// Descriptors read from one process's `fd` directory.
const MAX_FDS_PER_PROCESS: usize = 1 << 20;
const FD_CANCEL_STRIDE: usize = 4096;
const STAT_LIMIT: u64 = 4096;

/// What reading one `/proc/<pid>` entry can say about the process.
enum Probe {
    Running(ProcStat),
    /// Zombie or dead: the entry still exists but the process has exited.
    Exited(ProcStat),
    /// The entry exists but cannot be read (permissions, `hidepid`) or did not parse.
    Limited,
    Gone,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn ticks_per_second() -> u64 {
    static HZ: OnceLock<u64> = OnceLock::new();
    *HZ.get_or_init(|| {
        // SAFETY: `sysconf` has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        u64::try_from(hz).ok().filter(|hz| *hz > 0).unwrap_or(100)
    })
}

fn creation(stat: &ProcStat) -> u64 {
    tables::ticks_to_100ns(stat.start_ticks, ticks_per_second())
}

fn gone(error: &std::io::Error) -> bool {
    error.kind() == ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

fn probe(pid: u32) -> Probe {
    if pid == 0 {
        return Probe::Limited;
    }
    let mut text = String::new();
    match fs::File::open(format!("/proc/{pid}/stat"))
        .and_then(|file| file.take(STAT_LIMIT).read_to_string(&mut text))
    {
        Ok(_) => {}
        Err(error) if gone(&error) => return Probe::Gone,
        Err(_) => return Probe::Limited,
    }
    match tables::parse_proc_stat(&text) {
        Some(stat) if matches!(stat.state, 'Z' | 'X' | 'x') => Probe::Exited(stat),
        Some(stat) => Probe::Running(stat),
        None => Probe::Limited,
    }
}

/// PID plus start time of a running process. Callers use it right after a launch, while they still hold the child,
/// so the PID cannot have been recycled.
pub fn capture_identity(pid: u32) -> Option<ProcessIdentity> {
    match probe(pid) {
        Probe::Running(stat) => Some(ProcessIdentity {
            pid,
            creation_time_100ns: creation(&stat),
        }),
        _ => None,
    }
}

fn cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err(CANCELLED.into())
    } else {
        Ok(())
    }
}

fn process_ids() -> std::io::Result<Vec<u32>> {
    let mut ids: Vec<u32> = fs::read_dir("/proc")?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            let bytes = name.as_bytes();
            (!bytes.is_empty() && bytes.len() <= 10 && bytes.iter().all(u8::is_ascii_digit))
                .then(|| std::str::from_utf8(bytes).ok()?.parse().ok())?
        })
        .filter(|pid| *pid != 0)
        .collect();
    ids.sort_unstable();
    Ok(ids)
}

fn processes(cancel: &AtomicBool) -> Result<(Vec<RawProcess>, SectionState), String> {
    let ids = match process_ids() {
        Ok(ids) => ids,
        Err(error) => {
            return Ok((
                Vec::new(),
                SectionState::unavailable(format!("Process enumeration failed: {error}")),
            ))
        }
    };
    let mut state = SectionState::ok();
    let mut rows = Vec::with_capacity(ids.len().min(MAX_PROCESSES));
    for (index, pid) in ids.into_iter().enumerate() {
        if index % CANCEL_STRIDE == 0 {
            cancelled(cancel)?;
        }
        if rows.len() >= MAX_PROCESSES {
            state = SectionState::partial(format!(
                "More than {MAX_PROCESSES} processes; showing the first {MAX_PROCESSES}"
            ));
            break;
        }
        let (stat, availability) = match probe(pid) {
            Probe::Running(stat) => (Some(stat), Availability::Available),
            Probe::Exited(stat) => (Some(stat), Availability::Exited),
            Probe::Limited => (None, Availability::Limited),
            // Ended between the directory read and the stat read: it is no longer a process, so it is not listed.
            Probe::Gone => continue,
        };
        rows.push(match stat {
            Some(stat) => RawProcess {
                pid,
                parent_pid: stat.parent_pid,
                threads: stat.threads,
                creation: Some(creation(&stat)),
                name: stat.name,
                availability,
            },
            None => RawProcess {
                pid,
                parent_pid: 0,
                threads: 0,
                name: "(unreadable)".into(),
                creation: None,
                availability,
            },
        });
    }
    Ok((rows, state))
}

/// One `/proc/net` table. A kernel without IPv6 has no `tcp6`/`udp6`: that is an empty table, not a failure.
fn table(
    label: &str,
    file: &str,
    family: Family,
    protocol: Protocol,
    ipv6_optional: bool,
) -> (Vec<tables::ProcSocket>, SectionState) {
    let opened = match fs::File::open(file) {
        Ok(opened) => opened,
        Err(error) if error.kind() == ErrorKind::NotFound && ipv6_optional => {
            return (Vec::new(), SectionState::ok())
        }
        Err(error) => {
            return (
                Vec::new(),
                SectionState::unavailable(format!("{label} unavailable: {error}")),
            )
        }
    };
    match tables::parse_proc_net(BufReader::new(opened), family, protocol) {
        Ok(parsed) if parsed.truncated => {
            let shown = parsed.rows.len();
            (
                parsed.rows,
                SectionState::partial(format!(
                    "{label} is larger than the limit; showing the first {shown} rows"
                )),
            )
        }
        Ok(parsed) if parsed.malformed > 0 => {
            let skipped = parsed.malformed;
            (
                parsed.rows,
                SectionState::partial(format!(
                    "{label}: {skipped} lines could not be read and were skipped"
                )),
            )
        }
        Ok(parsed) => (parsed.rows, SectionState::ok()),
        Err(error) => (
            Vec::new(),
            SectionState::unavailable(format!("{label} unavailable: {error}")),
        ),
    }
}

/// `socket:[12345]` link target to its inode.
fn socket_inode(target: &std::path::Path) -> Option<u64> {
    let bytes = target.as_os_str().as_bytes();
    let digits = bytes.strip_prefix(b"socket:[")?.strip_suffix(b"]")?;
    if digits.is_empty() || digits.len() > 20 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// Records the wanted socket inodes held by one process's descriptors; the lowest PID owns a shared socket.
fn record_fd_owners(
    entries: fs::ReadDir,
    pid: u32,
    wanted: &HashSet<u64>,
    owners: &mut HashMap<u64, u32>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    for (count, entry) in entries.enumerate() {
        if count >= MAX_FDS_PER_PROCESS {
            break;
        }
        if count % FD_CANCEL_STRIDE == FD_CANCEL_STRIDE - 1 {
            cancelled(cancel)?;
        }
        let Ok(entry) = entry else { continue };
        // A descriptor closed since the listing, or a non-socket link: nothing to record.
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        if let Some(inode) = socket_inode(&target).filter(|inode| wanted.contains(inode)) {
            owners
                .entry(inode)
                .and_modify(|owner: &mut u32| *owner = (*owner).min(pid))
                .or_insert(pid);
        }
    }
    Ok(())
}

/// Owners of the wanted socket inodes. A socket shared by several processes (a forked listener) is attributed to
/// the lowest PID, whatever order `pids` is in. Processes whose descriptors cannot be listed are returned too:
/// their sockets are invisible to this scan.
fn socket_owners(
    wanted: &HashSet<u64>,
    pids: &[u32],
    cancel: &AtomicBool,
) -> Result<(HashMap<u64, u32>, HashSet<u32>), String> {
    let mut owners = HashMap::with_capacity(wanted.len());
    let mut unreadable = HashSet::new();
    for (index, pid) in pids.iter().enumerate() {
        if index % CANCEL_STRIDE == 0 {
            cancelled(cancel)?;
        }
        let entries = match fs::read_dir(format!("/proc/{pid}/fd")) {
            Ok(entries) => entries,
            Err(error) if gone(&error) => continue,
            Err(_) => {
                unreadable.insert(*pid);
                continue;
            }
        };
        record_fd_owners(entries, *pid, wanted, &mut owners, cancel)?;
    }
    Ok((owners, unreadable))
}

pub fn collect(cancel: &AtomicBool) -> Result<RawCollection, String> {
    let timer = Instant::now();
    let started_at = now_ms();
    cancelled(cancel)?;
    let (mut processes, process_state) = processes(cancel)?;
    cancelled(cancel)?;
    let (tcp4, tcp4_state) = table(
        "IPv4 TCP table",
        "/proc/net/tcp",
        Family::Ipv4,
        Protocol::Tcp,
        /* ipv6_optional= */ false,
    );
    let (tcp6, tcp6_state) = table(
        "IPv6 TCP table",
        "/proc/net/tcp6",
        Family::Ipv6,
        Protocol::Tcp,
        /* ipv6_optional= */ true,
    );
    let (udp4, udp4_state) = table(
        "IPv4 UDP table",
        "/proc/net/udp",
        Family::Ipv4,
        Protocol::Udp,
        /* ipv6_optional= */ false,
    );
    let (udp6, udp6_state) = table(
        "IPv6 UDP table",
        "/proc/net/udp6",
        Family::Ipv6,
        Protocol::Udp,
        /* ipv6_optional= */ true,
    );
    cancelled(cancel)?;

    // Join sockets with processes through the socket inode, after the tables were read.
    let wanted: HashSet<u64> = tcp4
        .iter()
        .chain(&tcp6)
        .chain(&udp4)
        .chain(&udp6)
        .map(|row| row.inode)
        .filter(|inode| *inode != 0)
        .collect();
    let pids: Vec<u32> = processes
        .iter()
        .filter(|process| process.availability != Availability::Exited)
        .map(|process| process.pid)
        .collect();
    let (owners, unreadable) = socket_owners(&wanted, &pids, cancel)?;
    for process in &mut processes {
        if unreadable.contains(&process.pid) && process.availability == Availability::Available {
            process.availability = Availability::Limited;
        }
    }
    let attach = |rows: Vec<tables::ProcSocket>| -> Vec<RawSocket> {
        rows.into_iter()
            .map(|row| RawSocket {
                pid: owners.get(&row.inode).copied().unwrap_or(0),
                ..row.socket
            })
            .collect()
    };
    let tcp: Vec<RawSocket> = attach(tcp4).into_iter().chain(attach(tcp6)).collect();
    let udp: Vec<RawSocket> = attach(udp4).into_iter().chain(attach(udp6)).collect();

    // Read start times again after the tables: a socket is attributed only if the identity did not change.
    let attributed: HashSet<u32> = tcp
        .iter()
        .chain(&udp)
        .map(|socket| socket.pid)
        .filter(|pid| *pid != 0)
        .collect();
    let mut rechecks = HashMap::with_capacity(attributed.len());
    for (index, pid) in attributed.into_iter().enumerate() {
        if index % CANCEL_STRIDE == 0 {
            cancelled(cancel)?;
        }
        rechecks.insert(
            pid,
            match probe(pid) {
                Probe::Running(stat) => Recheck::Creation(creation(&stat)),
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
        tcp,
        udp,
        rechecks,
        sections: Sections {
            processes: process_state,
            tcp4: tcp4_state,
            tcp6: tcp6_state,
            udp4: udp4_state,
            udp6: udp6_state,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::os::fd::AsRawFd;
    use std::process::{Command, Stdio};

    fn inode_of(fd: i32) -> u64 {
        socket_inode(&fs::read_link(format!("/proc/self/fd/{fd}")).unwrap())
            .expect("a socket descriptor")
    }

    #[test]
    fn socket_link_targets_are_parsed_strictly() {
        let parse = |text: &str| socket_inode(std::path::Path::new(text));
        assert_eq!(parse("socket:[12345]"), Some(12345));
        for bad in [
            "socket:[]",
            "socket:[12a]",
            "socket:[12345",
            "pipe:[12345]",
            "anon_inode:[eventfd]",
            "/dev/null",
            "socket:[-1]",
            "socket:[123456789012345678901]",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_shared_socket_goes_to_the_lowest_pid_a_vanished_pid_is_skipped_and_unreadable_ones_are_flagged(
    ) {
        use std::os::unix::process::CommandExt;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let inode = inode_of(listener.as_raw_fd());
        let fd = listener.as_raw_fd();
        // The child inherits the same socket as descriptor 9 (a forked listener), so two processes hold the inode.
        let mut command = Command::new("sleep");
        command
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: only async-signal-safe `dup2` runs between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, 9) < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command.spawn().unwrap();
        let (own, other) = (std::process::id(), child.id());
        let vanished = 4_194_303; // pid_max is at most 2^22; nothing is expected to live here
        let wanted = HashSet::from([inode, u64::MAX]);
        let cancel = AtomicBool::new(false);
        let (owners, unreadable) = socket_owners(
            &wanted,
            &[vanished, own.max(other), own.min(other)],
            &cancel,
        )
        .unwrap();
        assert_eq!(
            owners.get(&inode),
            Some(&own.min(other)),
            "ascending order is not required of the caller's list: the lowest pid wins"
        );
        assert_eq!(owners.len(), 1, "only wanted inodes are recorded");
        assert!(
            !unreadable.contains(&vanished) && !unreadable.contains(&own),
            "a vanished or own pid is not 'unreadable'"
        );
        // SAFETY: geteuid has no preconditions and does not access Rust memory.
        if unsafe { libc::geteuid() } != 0 {
            let (_, unreadable) = socket_owners(&HashSet::new(), &[1], &cancel).unwrap();
            assert!(unreadable.contains(&1), "another user's descriptors cannot be listed: pid 1 is flagged, not skipped silently");
        }
        let stopped = AtomicBool::new(true);
        assert_eq!(
            socket_owners(&wanted, &[own], &stopped).err().as_deref(),
            Some(CANCELLED)
        );
        child.kill().ok();
        child.wait().ok();
    }
}
