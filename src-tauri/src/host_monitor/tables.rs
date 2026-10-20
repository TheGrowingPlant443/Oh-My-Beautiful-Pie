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
