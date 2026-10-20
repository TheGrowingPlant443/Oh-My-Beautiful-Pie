//! Native Windows collection: ToolHelp process enumeration, limited-query process times, and the IPv4/IPv6 TCP and
//! UDP owner-PID tables. Read-only: no privileges are requested, nothing is opened for write or termination, no
//! subprocess runs, and no DNS or network traffic is generated.
#[cfg(not(windows))]
use super::snapshot::RawCollection;
#[cfg(not(windows))]
use super::types::ProcessIdentity;

#[cfg(windows)]
pub use imp::{capture_identity, collect};

#[cfg(not(windows))]
pub fn capture_identity(_pid: u32) -> Option<ProcessIdentity> {
    None
}

#[cfg(not(windows))]
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
