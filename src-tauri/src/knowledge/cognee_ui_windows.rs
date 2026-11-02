//! Windows listener/process inventory: std lacks TCP owner tables and Toolhelp snapshots.
use super::live::Listener;
use std::collections::HashMap;
use std::ffi::c_void;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};

#[link(name = "iphlpapi")]
extern "system" {
    fn GetExtendedTcpTable(
        table: *mut c_void,
        size: *mut u32,
        order: i32,
        family: u32,
        class: i32,
        reserved: u32,
    ) -> u32;
}

#[repr(C)]
struct ProcessEntry {
    size: u32,
    usage: u32,
    pid: u32,
    heap: usize,
    module: u32,
    threads: u32,
    parent: u32,
    priority: i32,
    flags: u32,
    exe: [u16; 260],
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> HANDLE;
    fn Process32FirstW(snapshot: HANDLE, entry: *mut ProcessEntry) -> i32;
    fn Process32NextW(snapshot: HANDLE, entry: *mut ProcessEntry) -> i32;
}

const AF_INET: u32 = 2;
const AF_INET6: u32 = 23;
const TCP_TABLE_OWNER_PID_LISTENER: i32 = 3;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const TH32CS_SNAPPROCESS: u32 = 2;

/// The raw table bytes; the buffer grows at most three times.
fn table(family: u32) -> Result<Vec<u8>, String> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut size = 0u32;
    for _ in 0..4 {
        let pointer = if buffer.is_empty() {
            std::ptr::null_mut()
        } else {
            buffer.as_mut_ptr().cast()
        };
        // SAFETY: `pointer` is null or points to `buffer`'s `size` writable bytes.
        let status = unsafe {
            GetExtendedTcpTable(
                pointer,
                &mut size,
                0,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        match status {
            0 => {
                buffer.truncate(size as usize);
                return Ok(buffer);
            }
            ERROR_INSUFFICIENT_BUFFER => buffer = vec![0u8; size as usize],
            other => {
                return Err(format!(
                    "GetExtendedTcpTable failed with Windows error {other}"
                ))
            }
        }
    }
    Err("The TCP listener table kept changing size.".into())
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

/// The port is the low 16 bits of a DWORD in network byte order.
fn port_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

pub(crate) fn parse_v4(bytes: &[u8]) -> Result<Vec<Listener>, String> {
    const ROW: usize = 24;
    if bytes.len() < 4 {
        return Err("The IPv4 listener table is truncated.".into());
    }
    let count = u32_at(bytes, 0) as usize;
    if count
        .checked_mul(ROW)
        .and_then(|rows| rows.checked_add(4))
        .is_none_or(|end| end > bytes.len())
    {
        return Err("The IPv4 listener table is inconsistent.".into());
    }
    Ok((0..count)
        .map(|index| {
            let row = &bytes[4 + index * ROW..4 + (index + 1) * ROW];
            Listener {
                address: IpAddr::V4(Ipv4Addr::new(row[4], row[5], row[6], row[7])),
                port: port_at(row, 8),
                pid: u32_at(row, 20),
            }
        })
        .collect())
}

pub(crate) fn parse_v6(bytes: &[u8]) -> Result<Vec<Listener>, String> {
    const ROW: usize = 56;
    if bytes.len() < 4 {
        return Err("The IPv6 listener table is truncated.".into());
    }
    let count = u32_at(bytes, 0) as usize;
    if count
        .checked_mul(ROW)
        .and_then(|rows| rows.checked_add(4))
        .is_none_or(|end| end > bytes.len())
    {
        return Err("The IPv6 listener table is inconsistent.".into());
    }
    Ok((0..count)
        .map(|index| {
            let row = &bytes[4 + index * ROW..4 + (index + 1) * ROW];
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&row[0..16]);
            Listener {
                address: IpAddr::V6(Ipv6Addr::from(octets)),
                port: port_at(row, 20),
                pid: u32_at(row, 52),
            }
        })
        .collect())
}

/// Every TCP listener of this computer (IPv4 and IPv6) with its owning process id.
pub(crate) fn tcp_listeners() -> Result<Vec<Listener>, String> {
    let mut all = parse_v4(&table(AF_INET)?)?;
    all.extend(parse_v6(&table(AF_INET6)?)?);
    Ok(all)
}

/// The creation time (100 ns FILETIME ticks) of a still running process; `None` when it cannot be inspected.
pub(crate) fn creation_time(pid: u32) -> Option<u64> {
    crate::process_tree::process_creation_time(pid)
}

/// Parent process ids of the running processes (one snapshot).
pub(crate) fn parent_map() -> Result<HashMap<u32, u32>, String> {
    // SAFETY: the snapshot handle is closed on every path; `entry` is a correctly sized, initialised struct.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            return Err(format!(
                "The process snapshot failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut entry: ProcessEntry = std::mem::zeroed();
        entry.size = std::mem::size_of::<ProcessEntry>() as u32;
        let mut map = HashMap::new();
        let mut more = Process32FirstW(snapshot, &mut entry);
        while more != 0 {
            map.insert(entry.pid, entry.parent);
            more = Process32NextW(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
        Ok(map)
    }
}

/// `pid` is the root or a verified descendant: every ancestor up to the root exists, is older than its child and
/// the root still has the creation time it was started with. Unknown ancestry is not ownership.
pub(crate) fn in_tree(parents: &HashMap<u32, u32>, root: u32, root_created: u64, pid: u32) -> bool {
    let Some(mut child_created) = creation_time(pid) else {
        return false;
    };
    if pid == root {
        return child_created == root_created;
    }
    let mut current = pid;
    for _ in 0..64 {
        let Some(&parent) = parents.get(&current) else {
            return false;
        };
        if parent == 0 {
            return false;
        }
        let Some(parent_created) = creation_time(parent) else {
            return false;
        };
        if parent_created > child_created {
            return false;
        }
        if parent == root {
            return parent_created == root_created;
        }
        current = parent;
        child_created = parent_created;
    }
    false
}
