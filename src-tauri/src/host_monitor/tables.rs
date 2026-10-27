//! Pure decoding of the Windows `GetExtendedTcpTable` / `GetExtendedUdpTable` owner-PID tables, plus address and
//! TCP-state classification. Rows are read from raw bytes with checked arithmetic (no struct casts), so a short or
//! lying buffer yields a partial/error result instead of an out-of-bounds read.
use std::net::{Ipv4Addr, Ipv6Addr};

use super::types::*;

const TCP4_ROW: usize = 24;
const TCP6_ROW: usize = 56;
const UDP4_ROW: usize = 12;
const UDP6_ROW: usize = 28;

pub const TCP_LISTEN: u32 = 2;
pub const TCP_SYN_SENT: u32 = 3;
pub const TCP_SYN_RCVD: u32 = 4;

/// One decoded row, before it is joined with a verified process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawSocket {
    pub family: Family,
    pub protocol: Protocol,
    pub pid: u32,
    pub local: Endpoint,
    pub remote: Option<Endpoint>,
    pub tcp_state: Option<u32>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParsedTable {
    pub rows: Vec<RawSocket>,
    /// The header announced more rows than the buffer holds: the rows returned are a prefix.
    pub truncated: bool,
}

fn u32_le(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// `dwLocalPort`/`dwRemotePort` keep the port in network byte order in the first two bytes of the DWORD.
fn port_be(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

fn v4(bytes: &[u8], at: usize) -> String {
    Ipv4Addr::new(bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]).to_string()
}

fn v6(bytes: &[u8], at: usize) -> String {
    let mut octets = [0u8; 16];
    octets.copy_from_slice(&bytes[at..at + 16]);
    Ipv6Addr::from(octets).to_string()
}

fn scope(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32_le(bytes, at)).filter(|scope| *scope != 0)
}

fn table(buffer: &[u8], row_len: usize, decode: impl Fn(&[u8]) -> RawSocket) -> Result<ParsedTable, String> {
    if buffer.len() < 4 {
        return Err("socket table is shorter than its header".into());
    }
    let announced = u32_le(buffer, 0) as usize;
    let capacity = (buffer.len() - 4) / row_len;
    let truncated = announced > capacity;
    let count = announced.min(capacity);
    let end = count
        .checked_mul(row_len)
        .and_then(|bytes| bytes.checked_add(4))
        .ok_or_else(|| "socket table size overflow".to_string())?;
    let rows = buffer[4..end].chunks_exact(row_len).map(decode).collect();
    Ok(ParsedTable { rows, truncated })
}

/// A wildcard peer with port zero means "no peer" (listeners, unconnected sockets).
fn peer(address: String, port: u16, scope_id: Option<u32>) -> Option<Endpoint> {
    let wildcard = address == "0.0.0.0" || address == "::";
    if wildcard && port == 0 {
        None
    } else {
        Some(Endpoint { address, port, scope_id })
    }
}

pub fn parse_tcp4(buffer: &[u8]) -> Result<ParsedTable, String> {
    table(buffer, TCP4_ROW, |row| RawSocket {
        family: Family::Ipv4,
        protocol: Protocol::Tcp,
        pid: u32_le(row, 20),
        local: Endpoint { address: v4(row, 4), port: port_be(row, 8), scope_id: None },
        remote: peer(v4(row, 12), port_be(row, 16), None),
        tcp_state: Some(u32_le(row, 0)),
    })
}

pub fn parse_tcp6(buffer: &[u8]) -> Result<ParsedTable, String> {
    table(buffer, TCP6_ROW, |row| RawSocket {
        family: Family::Ipv6,
        protocol: Protocol::Tcp,
        pid: u32_le(row, 52),
        local: Endpoint { address: v6(row, 0), port: port_be(row, 20), scope_id: scope(row, 16) },
        remote: peer(v6(row, 24), port_be(row, 44), scope(row, 40)),
        tcp_state: Some(u32_le(row, 48)),
    })
}

pub fn parse_udp4(buffer: &[u8]) -> Result<ParsedTable, String> {
    table(buffer, UDP4_ROW, |row| RawSocket {
        family: Family::Ipv4,
        protocol: Protocol::Udp,
        pid: u32_le(row, 8),
        local: Endpoint { address: v4(row, 0), port: port_be(row, 4), scope_id: None },
        remote: None,
        tcp_state: None,
    })
}

pub fn parse_udp6(buffer: &[u8]) -> Result<ParsedTable, String> {
    table(buffer, UDP6_ROW, |row| RawSocket {
        family: Family::Ipv6,
        protocol: Protocol::Udp,
        pid: u32_le(row, 24),
        local: Endpoint { address: v6(row, 0), port: port_be(row, 20), scope_id: scope(row, 16) },
        remote: None,
        tcp_state: None,
    })
}

pub fn tcp_state_name(state: u32) -> String {
    match state {
        1 => "CLOSED".into(),
        2 => "LISTEN".into(),
        3 => "SYN_SENT".into(),
        4 => "SYN_RCVD".into(),
        5 => "ESTABLISHED".into(),
        6 => "FIN_WAIT1".into(),
        7 => "FIN_WAIT2".into(),
        8 => "CLOSE_WAIT".into(),
        9 => "CLOSING".into(),
        10 => "LAST_ACK".into(),
        11 => "TIME_WAIT".into(),
        12 => "DELETE_TCB".into(),
        other => format!("UNKNOWN({other})"),
    }
}

/// Only handshake states reveal who initiated a connection; ESTABLISHED and closing states do not, and the table
/// cannot be used to carry a direction across snapshots because tuples are reused.
pub fn direction_of(state: u32) -> (Direction, DirectionEvidence) {
    match state {
        TCP_LISTEN => (Direction::Listener, DirectionEvidence::Listen),
        TCP_SYN_SENT => (Direction::Outbound, DirectionEvidence::SynSent),
        TCP_SYN_RCVD => (Direction::Inbound, DirectionEvidence::SynReceived),
        _ => (Direction::Unknown, DirectionEvidence::Unavailable),
    }
}

fn scope_v4(ip: Ipv4Addr) -> AddressScope {
    let [a, b, ..] = ip.octets();
    if ip.is_unspecified() {
        AddressScope::Unspecified
    } else if ip.is_loopback() {
        AddressScope::Loopback
    } else if ip.is_link_local() {
        AddressScope::LinkLocal
    } else if ip.is_private() {
        AddressScope::Private
    } else if ip.is_broadcast() || ip.is_multicast() || a >= 240 || (a == 100 && (64..128).contains(&b)) {
        AddressScope::Other
    } else {
        AddressScope::Public
    }
}

/// Classification of a textual address as produced by this module; unparsable text is `Other`.
pub fn address_scope(address: &str) -> AddressScope {
    if let Ok(ip) = address.parse::<Ipv4Addr>() {
        return scope_v4(ip);
    }
    let Ok(ip) = address.parse::<Ipv6Addr>() else { return AddressScope::Other };
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return scope_v4(mapped);
    }
    let first = ip.segments()[0];
    if ip.is_unspecified() {
        AddressScope::Unspecified
    } else if ip.is_loopback() {
        AddressScope::Loopback
    } else if first & 0xffc0 == 0xfe80 {
        AddressScope::LinkLocal
    } else if first & 0xfe00 == 0xfc00 {
        AddressScope::Private
    } else if first & 0xe000 == 0x2000 {
        AddressScope::Public
    } else {
        AddressScope::Other
    }
}

/// Remote peer that is neither a wildcard nor loopback. Private LAN peers count as external.
pub fn is_external(remote: Option<&Endpoint>) -> bool {
    remote.is_some_and(|peer| !matches!(address_scope(&peer.address), AddressScope::Loopback | AddressScope::Unspecified))
}

// ---------------------------------------------------------------------------------------------------------------
// Linux `/proc/net/{tcp,tcp6,udp,udp6}` text tables. Pure decoding, so it is testable on any host; only the Linux
// collector calls it. Lines are read with a hard length bound and the row count is capped.
// ---------------------------------------------------------------------------------------------------------------

/// Rows kept from one `/proc/net` table; a busy host can have many TIME_WAIT entries, a runaway one must not grow
/// the snapshot without bound.
#[cfg(any(target_os = "linux", test))]
pub const PROC_ROW_LIMIT: usize = 200_000;
/// A socket row is far shorter; a longer line is not a row.
#[cfg(any(target_os = "linux", test))]
const PROC_LINE_LIMIT: usize = 1024;

/// A decoded `/proc/net` row. `socket.pid` is `0` until the inode is joined with a process.
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcSocket {
    pub socket: RawSocket,
    /// Socket inode; `0` for sockets with no owner file (TIME_WAIT and other orphans).
    pub inode: u64,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedProcTable {
    pub rows: Vec<ProcSocket>,
    /// The table held more than `PROC_ROW_LIMIT` rows: the rows returned are a prefix.
    pub truncated: bool,
    /// Lines that were not a socket row (counted, never guessed at).
    pub malformed: usize,
}

/// The kernel prints each 32-bit address word with `%08X` of the raw network-order bytes, i.e. in host byte order,
/// so the bytes are recovered with the native-endian view of the parsed word. Ports are plain host-order hex.
#[cfg(any(target_os = "linux", test))]
fn proc_endpoint(text: &str, family: Family) -> Option<(String, u16)> {
    let (address, port) = text.split_once(':')?;
    if port.len() != 4 {
        return None;
    }
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |chunk: &str| -> Option<[u8; 4]> {
        (chunk.len() == 8 && chunk.bytes().all(|b| b.is_ascii_hexdigit())).then(|| u32::from_str_radix(chunk, 16).ok().map(u32::to_ne_bytes))?
    };
    let address = match family {
        Family::Ipv4 => Ipv4Addr::from(word(address)?).to_string(),
        Family::Ipv6 => {
            if address.len() != 32 || !address.is_ascii() {
                return None;
            }
            let mut octets = [0u8; 16];
            for (index, chunk) in address.as_bytes().chunks(8).enumerate() {
                octets[index * 4..index * 4 + 4].copy_from_slice(&word(std::str::from_utf8(chunk).ok()?)?);
            }
            Ipv6Addr::from(octets).to_string()
        }
    };
    Some((address, port))
}

/// Linux TCP state number (`include/net/tcp_states.h`) to the Windows MIB number the rest of the monitor uses.
#[cfg(any(target_os = "linux", test))]
pub fn tcp_state_from_linux(state: u8) -> u32 {
    match state {
        0x01 => 5,  // ESTABLISHED
        0x02 => 3,  // SYN_SENT
        0x03 => 4,  // SYN_RECV
        0x04 => 6,  // FIN_WAIT1
        0x05 => 7,  // FIN_WAIT2
        0x06 => 11, // TIME_WAIT
        0x07 => 1,  // CLOSE
        0x08 => 8,  // CLOSE_WAIT
        0x09 => 10, // LAST_ACK
        0x0a => TCP_LISTEN,
        0x0b => 9,  // CLOSING
        0x0c => 4,  // NEW_SYN_RECV: a request socket still in the handshake
        _ => 0,
    }
}

#[cfg(any(target_os = "linux", test))]
fn proc_row(line: &str, family: Family, protocol: Protocol) -> Option<ProcSocket> {
    let mut fields = line.split_ascii_whitespace();
    let (_slot, local, remote, state) = (fields.next()?, fields.next()?, fields.next()?, fields.next()?);
    // Skipped columns: tx_queue:rx_queue, tr:tm->when, retrnsmt, uid, timeout; then the inode.
    let inode: u64 = fields.nth(5)?.parse().ok()?;
    let (local_address, local_port) = proc_endpoint(local, family)?;
    let (remote_address, remote_port) = proc_endpoint(remote, family)?;
    let tcp_state = match protocol {
        Protocol::Tcp => Some(tcp_state_from_linux(u8::from_str_radix(state, 16).ok().filter(|_| state.len() == 2)?)),
        Protocol::Udp => None,
    };
    Some(ProcSocket {
        socket: RawSocket {
            family,
            protocol,
            pid: 0,
            local: Endpoint { address: local_address, port: local_port, scope_id: None },
            // UDP carries no peer here, exactly like the Windows UDP table: connected UDP sockets list their local side only.
            remote: match protocol {
                Protocol::Tcp => peer(remote_address, remote_port, None),
                Protocol::Udp => None,
            },
            tcp_state,
        },
        inode,
    })
}

/// `Ok(None)` is end of input; an over-long line yields `Some(None)` after its remainder was discarded.
#[cfg(any(target_os = "linux", test))]
fn next_line(reader: &mut impl std::io::BufRead, buffer: &mut Vec<u8>) -> Result<Option<Option<()>>, String> {
    use std::io::{BufRead, Read};
    buffer.clear();
    let read = reader.by_ref().take(PROC_LINE_LIMIT as u64 + 1).read_until(b'\n', buffer).map_err(|e| e.to_string())?;
    if read == 0 {
        return Ok(None);
    }
    if buffer.last() == Some(&b'\n') || buffer.len() <= PROC_LINE_LIMIT {
        return Ok(Some(Some(())));
    }
    // Discard the rest of an over-long line without keeping it.
    loop {
        let mut sink = Vec::new();
        let read = reader.by_ref().take(PROC_LINE_LIMIT as u64).read_until(b'\n', &mut sink).map_err(|e| e.to_string())?;
        if read == 0 || sink.last() == Some(&b'\n') {
            break;
        }
    }
    Ok(Some(None))
}

/// Reads one `/proc/net` table. The first line must be the column header; rows that do not decode are counted.
#[cfg(any(target_os = "linux", test))]
pub fn parse_proc_net(mut reader: impl std::io::BufRead, family: Family, protocol: Protocol) -> Result<ParsedProcTable, String> {
    let mut buffer = Vec::with_capacity(256);
    match next_line(&mut reader, &mut buffer)? {
        Some(Some(())) if buffer.trim_ascii_start().starts_with(b"sl") => {}
        None => return Err("table is empty".into()),
        _ => return Err("table header is not the expected one".into()),
    }
    let mut parsed = ParsedProcTable { rows: Vec::new(), truncated: false, malformed: 0 };
    while let Some(line) = next_line(&mut reader, &mut buffer)? {
        if parsed.rows.len() >= PROC_ROW_LIMIT {
            parsed.truncated = true;
            break;
        }
        let row = line.and_then(|_| std::str::from_utf8(&buffer).ok()).and_then(|text| proc_row(text, family, protocol));
        match row {
            Some(row) => parsed.rows.push(row),
            None if buffer.trim_ascii().is_empty() => {}
            None => parsed.malformed += 1,
        }
    }
    Ok(parsed)
}

/// The fields of `/proc/<pid>/stat` the monitor reads.
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcStat {
    /// `comm`: the kernel's process name, at most 15 bytes.
    pub name: String,
    /// Scheduler state letter; `Z` (zombie) and `X` (dead) mean the process has exited.
    pub state: char,
    pub parent_pid: u32,
    pub threads: u32,
    /// `starttime`: clock ticks since boot at which the process started.
    pub start_ticks: u64,
}

/// `comm` may itself contain spaces and parentheses, so the name is delimited by the first `(` and the *last* `)`;
/// everything after is whitespace-separated numbers (field 3 is the state, 4 the parent, 20 the thread count and 22
/// the start time).
#[cfg(any(target_os = "linux", test))]
pub fn parse_proc_stat(text: &str) -> Option<ProcStat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close < open {
        return None;
    }
    let mut rest = text[close + 1..].split_ascii_whitespace();
    let state = rest.next()?.chars().next()?;
    let parent_pid = rest.next()?.parse().ok()?;
    let threads = rest.nth(15)?.parse().ok()?;
    let start_ticks = rest.nth(1)?.parse().ok()?;
    Some(ProcStat { name: text[open + 1..close].to_string(), state, parent_pid, threads, start_ticks })
}

/// Start time in the monitor's 100 ns identity unit. Only equality and order matter, so ticks since boot are kept
/// instead of an absolute time that a clock adjustment could shift under a stored identity.
#[cfg(any(target_os = "linux", test))]
pub fn ticks_to_100ns(ticks: u64, ticks_per_second: u64) -> u64 {
    (u128::from(ticks) * 10_000_000 / u128::from(ticks_per_second.max(1))).min(u128::from(u64::MAX)) as u64
}
