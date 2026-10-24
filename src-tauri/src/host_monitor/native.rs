//! Native collection. Windows: ToolHelp process enumeration, limited-query process times, and the IPv4/IPv6 TCP and
//! UDP owner-PID tables. Linux: `/proc/<pid>/stat`, `/proc/net/{tcp,tcp6,udp,udp6}` and the socket inodes found under
//! `/proc/<pid>/fd`. Read-only: no privileges are requested, nothing is opened for write or termination, no
//! subprocess runs, and no DNS or network traffic is generated. Other platforms report `unsupported_platform`.
#[cfg(not(any(windows, target_os = "linux")))]
use super::snapshot::RawCollection;
#[cfg(not(any(windows, target_os = "linux")))]
use super::types::ProcessIdentity;

#[cfg(windows)]
pub use imp::{capture_identity, collect};
#[cfg(target_os = "linux")]
pub use linux::{capture_identity, collect};

#[cfg(not(any(windows, target_os = "linux")))]
pub fn capture_identity(_pid: u32) -> Option<ProcessIdentity> {
    None
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn collect(_cancel: &std::sync::atomic::AtomicBool) -> Result<RawCollection, String> {
    Err("unsupported_platform".into())
}

#[cfg(windows)]
mod imp {
    use std::collections::{HashMap, HashSet};
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use windows_sys::Win32::Foundation::{
        ERROR_INSUFFICIENT_BUFFER, FILETIME, INVALID_HANDLE_VALUE, NO_ERROR,
    };
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    use super::super::snapshot::{RawCollection, RawProcess, Recheck};
    use super::super::tables::{self, ParsedTable, RawSocket};
    use super::super::types::*;

    const AF_INET: u32 = 2;
    const AF_INET6: u32 = 23;
    const ERROR_NO_MORE_FILES: i32 = 18;
    const ERROR_INVALID_PARAMETER: i32 = 87;
    /// Initial table buffer; native size growth is retried at most three times.
    const INITIAL_WORDS: usize = 512;
    const GROWTH_RETRIES: usize = 3;
    const CANCEL_STRIDE: usize = 32;

    pub const CANCELLED: &str = "monitor_cancelled";

    enum Probe {
        Running(u64),
        Exited(Option<u64>),
        /// Exists but cannot be opened for a limited query (or the query failed).
        Limited,
        Gone,
    }

    fn filetime(value: FILETIME) -> u64 {
        (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
    }

    fn now_ms() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    }

    fn probe(pid: u32) -> Probe {
        if pid == 0 {
            return Probe::Limited;
        }
        // SAFETY: the handle is owned immediately and closed on drop; the out-parameters are plain stack FILETIMEs.
        unsafe {
            let raw = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if raw.is_null() {
                return match std::io::Error::last_os_error().raw_os_error() {
                    Some(ERROR_INVALID_PARAMETER) => Probe::Gone,
                    _ => Probe::Limited,
                };
            }
            let handle = OwnedHandle::from_raw_handle(raw);
            let (mut created, mut exited, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
            let read = GetProcessTimes(raw, &mut created, &mut exited, &mut kernel, &mut user);
            drop(handle);
            if read == 0 {
                return Probe::Limited;
            }
            let created = filetime(created);
            if filetime(exited) != 0 {
                Probe::Exited(Some(created))
            } else {
                Probe::Running(created)
            }
        }
    }

    /// PID plus creation time of a running process. Callers use it right after a launch, while they still hold the
    /// child handle, so the PID cannot have been recycled.
    pub fn capture_identity(pid: u32) -> Option<ProcessIdentity> {
        match probe(pid) {
            Probe::Running(creation_time_100ns) => Some(ProcessIdentity { pid, creation_time_100ns }),
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

    fn processes(cancel: &AtomicBool) -> Result<(Vec<RawProcess>, SectionState), String> {
        // SAFETY: the snapshot handle is wrapped before use; PROCESSENTRY32W is initialised with its size.
        unsafe {
            let raw = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if raw == INVALID_HANDLE_VALUE {
                let error = std::io::Error::last_os_error();
                return Ok((Vec::new(), SectionState::unavailable(format!("Process snapshot failed: {error}"))));
            }
            let snapshot = OwnedHandle::from_raw_handle(raw);
            let mut entry = PROCESSENTRY32W::default();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut rows = Vec::new();
            if Process32FirstW(raw, &mut entry) == 0 {
                let error = std::io::Error::last_os_error();
                drop(snapshot);
                return Ok((rows, SectionState::unavailable(format!("Process enumeration failed: {error}"))));
            }
            let mut state = SectionState::ok();
            loop {
                if rows.len() % CANCEL_STRIDE == 0 {
                    cancelled(cancel)?;
                }
                let length = entry.szExeFile.iter().position(|unit| *unit == 0).unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..length]);
                let (creation, availability) = match probe(entry.th32ProcessID) {
                    Probe::Running(created) => (Some(created), Availability::Available),
                    Probe::Exited(created) => (created, Availability::Exited),
                    Probe::Limited => (None, Availability::Limited),
                    Probe::Gone => (None, Availability::Exited),
                };
                rows.push(RawProcess {
                    pid: entry.th32ProcessID,
                    parent_pid: entry.th32ParentProcessID,
                    threads: entry.cntThreads,
                    name,
                    creation,
                    availability,
                });
                if Process32NextW(raw, &mut entry) == 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(ERROR_NO_MORE_FILES) {
                        state = SectionState::partial(format!("Process enumeration stopped early: {error}"));
                    }
                    break;
                }
            }
            drop(snapshot);
            Ok((rows, state))
        }
    }

    /// One reusable, 8-byte aligned buffer for every socket table of a pass.
    struct Buffer {
        words: Vec<u64>,
    }

    impl Buffer {
        fn fetch(&mut self, call: impl Fn(*mut core::ffi::c_void, *mut u32) -> u32) -> Result<&[u8], String> {
            for attempt in 0..=GROWTH_RETRIES {
                let capacity = self.words.len() * 8;
                let mut size = u32::try_from(capacity).map_err(|_| "socket table buffer too large".to_string())?;
                let status = call(self.words.as_mut_ptr().cast(), &mut size);
                if status == NO_ERROR {
                    let used = (size as usize).min(capacity);
                    // SAFETY: `used <= capacity` bytes of an initialised `Vec<u64>` allocation.
                    return Ok(unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), used) });
                }
                if status != ERROR_INSUFFICIENT_BUFFER || attempt == GROWTH_RETRIES {
                    return Err(if status == ERROR_INSUFFICIENT_BUFFER {
                        "the table kept growing while it was read".to_string()
                    } else {
                        format!("Windows error {status}")
                    });
                }
                // The table can grow between the size query and the read: ask for headroom.
                let wanted = (size as usize).checked_add(8 * 64).ok_or_else(|| "socket table size overflow".to_string())?;
                self.words.resize(wanted.div_ceil(8), 0);
            }
            Err("socket table could not be read".into())
        }
    }

    fn table(
        label: &str,
        parse: fn(&[u8]) -> Result<ParsedTable, String>,
        buffer: &mut Buffer,
        call: impl Fn(*mut core::ffi::c_void, *mut u32) -> u32,
    ) -> (Vec<RawSocket>, SectionState) {
        match buffer.fetch(call).and_then(parse) {
            Ok(parsed) if parsed.truncated => {
                let shown = parsed.rows.len();
                (parsed.rows, SectionState::partial(format!("{label} changed while it was read; showing the first {shown} rows")))
            }
            Ok(parsed) => (parsed.rows, SectionState::ok()),
            Err(error) => (Vec::new(), SectionState::unavailable(format!("{label} unavailable: {error}"))),
        }
    }

    pub fn collect(cancel: &AtomicBool) -> Result<RawCollection, String> {
        let timer = Instant::now();
        let started_at = now_ms();
        cancelled(cancel)?;
        let (processes, process_state) = processes(cancel)?;
        cancelled(cancel)?;
        let mut buffer = Buffer { words: vec![0; INITIAL_WORDS] };
        // SAFETY (all four calls): the pointer/size pair always describes `buffer`'s live allocation.
        let (tcp4, tcp4_state) = table("IPv4 TCP table", tables::parse_tcp4, &mut buffer, |ptr, size| unsafe {
            GetExtendedTcpTable(ptr, size, 0, AF_INET, TCP_TABLE_OWNER_PID_ALL, 0)
        });
        cancelled(cancel)?;
        let (tcp6, tcp6_state) = table("IPv6 TCP table", tables::parse_tcp6, &mut buffer, |ptr, size| unsafe {
            GetExtendedTcpTable(ptr, size, 0, AF_INET6, TCP_TABLE_OWNER_PID_ALL, 0)
        });
        cancelled(cancel)?;
        let (udp4, udp4_state) = table("IPv4 UDP table", tables::parse_udp4, &mut buffer, |ptr, size| unsafe {
            GetExtendedUdpTable(ptr, size, 0, AF_INET, UDP_TABLE_OWNER_PID, 0)
        });
        cancelled(cancel)?;
        let (udp6, udp6_state) = table("IPv6 UDP table", tables::parse_udp6, &mut buffer, |ptr, size| unsafe {
            GetExtendedUdpTable(ptr, size, 0, AF_INET6, UDP_TABLE_OWNER_PID, 0)
        });
        cancelled(cancel)?;

        let tcp: Vec<RawSocket> = tcp4.into_iter().chain(tcp6).collect();
        let udp: Vec<RawSocket> = udp4.into_iter().chain(udp6).collect();
        // Read creation times again after the tables: a socket is attributed only if the identity did not change.
        let pids: HashSet<u32> = tcp.iter().chain(&udp).map(|socket| socket.pid).filter(|pid| *pid != 0).collect();
        let mut rechecks = HashMap::with_capacity(pids.len());
        for (index, pid) in pids.into_iter().enumerate() {
            if index % CANCEL_STRIDE == 0 {
                cancelled(cancel)?;
            }
            rechecks.insert(
                pid,
                match probe(pid) {
                    Probe::Running(created) => Recheck::Creation(created),
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
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::io::{BufReader, ErrorKind, Read};
    use std::os::unix::ffi::OsStrExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use super::super::snapshot::{RawCollection, RawProcess, Recheck};
    use super::super::tables::{self, ProcStat, RawSocket};
    use super::super::types::*;

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
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
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
        match fs::File::open(format!("/proc/{pid}/stat")).and_then(|file| file.take(STAT_LIMIT).read_to_string(&mut text)) {
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
            Probe::Running(stat) => Some(ProcessIdentity { pid, creation_time_100ns: creation(&stat) }),
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
                (!bytes.is_empty() && bytes.len() <= 10 && bytes.iter().all(u8::is_ascii_digit)).then(|| std::str::from_utf8(bytes).ok()?.parse().ok())?
            })
            .filter(|pid| *pid != 0)
            .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    fn processes(cancel: &AtomicBool) -> Result<(Vec<RawProcess>, SectionState), String> {
        let ids = match process_ids() {
            Ok(ids) => ids,
            Err(error) => return Ok((Vec::new(), SectionState::unavailable(format!("Process enumeration failed: {error}")))),
        };
        let mut state = SectionState::ok();
        let mut rows = Vec::with_capacity(ids.len().min(MAX_PROCESSES));
        for (index, pid) in ids.into_iter().enumerate() {
            if index % CANCEL_STRIDE == 0 {
                cancelled(cancel)?;
            }
            if rows.len() >= MAX_PROCESSES {
                state = SectionState::partial(format!("More than {MAX_PROCESSES} processes; showing the first {MAX_PROCESSES}"));
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
                None => RawProcess { pid, parent_pid: 0, threads: 0, name: "(unreadable)".into(), creation: None, availability },
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
            Err(error) if error.kind() == ErrorKind::NotFound && ipv6_optional => return (Vec::new(), SectionState::ok()),
            Err(error) => return (Vec::new(), SectionState::unavailable(format!("{label} unavailable: {error}"))),
        };
        match tables::parse_proc_net(BufReader::new(opened), family, protocol) {
            Ok(parsed) if parsed.truncated => {
                let shown = parsed.rows.len();
                (parsed.rows, SectionState::partial(format!("{label} is larger than the limit; showing the first {shown} rows")))
            }
            Ok(parsed) if parsed.malformed > 0 => {
                let skipped = parsed.malformed;
                (parsed.rows, SectionState::partial(format!("{label}: {skipped} lines could not be read and were skipped")))
            }
            Ok(parsed) => (parsed.rows, SectionState::ok()),
            Err(error) => (Vec::new(), SectionState::unavailable(format!("{label} unavailable: {error}"))),
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

    /// Owners of the wanted socket inodes. A socket shared by several processes (a forked listener) is attributed to
    /// the lowest PID, whatever order `pids` is in. Processes whose descriptors cannot be listed are returned too:
    /// their sockets are invisible to this scan.
    fn socket_owners(wanted: &HashSet<u64>, pids: &[u32], cancel: &AtomicBool) -> Result<(HashMap<u64, u32>, HashSet<u32>), String> {
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
            for (count, entry) in entries.enumerate() {
                if count >= MAX_FDS_PER_PROCESS {
                    break;
                }
                if count % FD_CANCEL_STRIDE == FD_CANCEL_STRIDE - 1 {
                    cancelled(cancel)?;
                }
                let Ok(entry) = entry else { continue };
                // A descriptor closed since the listing, or a non-socket link: nothing to record.
                let Ok(target) = fs::read_link(entry.path()) else { continue };
                if let Some(inode) = socket_inode(&target).filter(|inode| wanted.contains(inode)) {
                    owners.entry(inode).and_modify(|owner: &mut u32| *owner = (*owner).min(*pid)).or_insert(*pid);
                }
            }
        }
        Ok((owners, unreadable))
    }

    pub fn collect(cancel: &AtomicBool) -> Result<RawCollection, String> {
        let timer = Instant::now();
        let started_at = now_ms();
        cancelled(cancel)?;
        let (mut processes, process_state) = processes(cancel)?;
        cancelled(cancel)?;
        let (tcp4, tcp4_state) = table("IPv4 TCP table", "/proc/net/tcp", Family::Ipv4, Protocol::Tcp, false);
        let (tcp6, tcp6_state) = table("IPv6 TCP table", "/proc/net/tcp6", Family::Ipv6, Protocol::Tcp, true);
        let (udp4, udp4_state) = table("IPv4 UDP table", "/proc/net/udp", Family::Ipv4, Protocol::Udp, false);
        let (udp6, udp6_state) = table("IPv6 UDP table", "/proc/net/udp6", Family::Ipv6, Protocol::Udp, true);
        cancelled(cancel)?;

        // Join sockets with processes through the socket inode, after the tables were read.
        let wanted: HashSet<u64> = tcp4.iter().chain(&tcp6).chain(&udp4).chain(&udp6).map(|row| row.inode).filter(|inode| *inode != 0).collect();
        let pids: Vec<u32> = processes.iter().filter(|process| process.availability != Availability::Exited).map(|process| process.pid).collect();
        let (owners, unreadable) = socket_owners(&wanted, &pids, cancel)?;
        for process in &mut processes {
            if unreadable.contains(&process.pid) && process.availability == Availability::Available {
                process.availability = Availability::Limited;
            }
        }
        let attach = |rows: Vec<tables::ProcSocket>| -> Vec<RawSocket> {
            rows.into_iter()
                .map(|row| RawSocket { pid: owners.get(&row.inode).copied().unwrap_or(0), ..row.socket })
                .collect()
        };
        let tcp: Vec<RawSocket> = attach(tcp4).into_iter().chain(attach(tcp6)).collect();
        let udp: Vec<RawSocket> = attach(udp4).into_iter().chain(attach(udp6)).collect();

        // Read start times again after the tables: a socket is attributed only if the identity did not change.
        let attributed: HashSet<u32> = tcp.iter().chain(&udp).map(|socket| socket.pid).filter(|pid| *pid != 0).collect();
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
            sections: Sections { processes: process_state, tcp4: tcp4_state, tcp6: tcp6_state, udp4: udp4_state, udp6: udp6_state },
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::net::TcpListener;
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        fn inode_of(fd: i32) -> u64 {
            socket_inode(&fs::read_link(format!("/proc/self/fd/{fd}")).unwrap()).expect("a socket descriptor")
        }

        #[test]
        fn socket_link_targets_are_parsed_strictly() {
            let parse = |text: &str| socket_inode(std::path::Path::new(text));
            assert_eq!(parse("socket:[12345]"), Some(12345));
            for bad in ["socket:[]", "socket:[12a]", "socket:[12345", "pipe:[12345]", "anon_inode:[eventfd]", "/dev/null", "socket:[-1]", "socket:[123456789012345678901]"] {
                assert_eq!(parse(bad), None, "{bad}");
            }
        }

        #[test]
        fn a_shared_socket_goes_to_the_lowest_pid_a_vanished_pid_is_skipped_and_unreadable_ones_are_flagged() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let inode = inode_of(listener.as_raw_fd());
            let fd = listener.as_raw_fd();
            // The child inherits the same socket as descriptor 9 (a forked listener), so two processes hold the inode.
            let mut command = Command::new("sleep");
            command.arg("30").stdout(Stdio::null()).stderr(Stdio::null());
            // SAFETY: only async-signal-safe `dup2` runs between fork and exec.
            unsafe {
                command.pre_exec(move || if libc::dup2(fd, 9) < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
            }
            let mut child = command.spawn().unwrap();
            let (own, other) = (std::process::id(), child.id());
            let vanished = 4_194_303; // pid_max is at most 2^22; nothing is expected to live here
            let wanted = HashSet::from([inode, u64::MAX]);
            let cancel = AtomicBool::new(false);
            let (owners, unreadable) = socket_owners(&wanted, &[vanished, own.max(other), own.min(other)], &cancel).unwrap();
            assert_eq!(owners.get(&inode), Some(&own.min(other)), "ascending order is not required of the caller's list: the lowest pid wins");
            assert_eq!(owners.len(), 1, "only wanted inodes are recorded");
            assert!(!unreadable.contains(&vanished) && !unreadable.contains(&own), "a vanished or own pid is not 'unreadable'");
            if unsafe { libc::geteuid() } != 0 {
                let (_, unreadable) = socket_owners(&HashSet::new(), &[1], &cancel).unwrap();
                assert!(unreadable.contains(&1), "another user's descriptors cannot be listed: pid 1 is flagged, not skipped silently");
            }
            let stopped = AtomicBool::new(true);
            assert_eq!(socket_owners(&wanted, &[own], &stopped).err().as_deref(), Some(CANCELLED));
            child.kill().ok();
            child.wait().ok();
        }
    }
}
