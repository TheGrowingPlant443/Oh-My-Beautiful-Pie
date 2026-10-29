//! macOS process metadata and numeric lsof field decoding. The decoder is also compiled by Linux tests.
//! lsof sees descriptors, not a guaranteed complete kernel socket table: every successful socket section is
//! deliberately partial, even as root (SIP/TCC and process races can hide evidence). No elevation is attempted.
use std::cmp::Ordering;
use std::net::{Ipv4Addr, Ipv6Addr};

use crate::host_monitor::tables::RawSocket;
use crate::host_monitor::types::{Endpoint, Family, Protocol};

#[derive(Default)]
struct FileRecord<'input> {
    family: Option<Family>,
    protocol: Option<Protocol>,
    name: Option<&'input str>,
    state: Option<&'input str>,
    invalid: bool,
}

#[derive(Default)]
pub(super) struct ParsedLsof {
    pub(super) tcp: Vec<RawSocket>,
    pub(super) udp: Vec<RawSocket>,
    pub(super) malformed: usize,
}

fn endpoint(
    text: &str,
    family: Family,
    resolve_scope: fn(&str) -> Option<u32>,
) -> Option<Endpoint> {
    let (address, port) = text.rsplit_once(':')?;
    let port = port.parse::<u16>().ok()?;
    let address = address
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(address);
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
    Some(Endpoint {
        address,
        port,
        scope_id,
    })
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

fn finish_record(
    parsed: &mut ParsedLsof,
    pid: Option<u32>,
    record: FileRecord<'_>,
    resolve_scope: fn(&str) -> Option<u32>,
) {
    let row = (|| {
        if record.invalid {
            return None;
        }
        let pid = pid?;
        let family = record.family?;
        let protocol = record.protocol?;
        let name = record.name?;
        let (local, remote) = match name.split_once("->") {
            Some((local, remote)) => (
                endpoint(local, family, resolve_scope)?,
                Some(endpoint(remote, family, resolve_scope)?),
            ),
            None => (endpoint(name, family, resolve_scope)?, None),
        };
        let tcp_state = if protocol == Protocol::Tcp {
            record.state.and_then(tcp_state)
        } else {
            None
        };
        Some(RawSocket {
            family,
            protocol,
            pid,
            local,
            remote,
            tcp_state,
        })
    })();
    match row {
        Some(row) if row.protocol == Protocol::Tcp => parsed.tcp.push(row),
        Some(row) => parsed.udp.push(row),
        None => parsed.malformed += 1,
    }
}

/// A record boundary: `p` starts a process (and ends the open file record), `f` starts a file record.
fn start_section(
    parsed: &mut ParsedLsof,
    pid: &mut Option<u32>,
    record: &mut Option<FileRecord<'_>>,
    tag: u8,
    value: Result<&str, std::str::Utf8Error>,
    resolve_scope: fn(&str) -> Option<u32>,
) {
    if let Some(previous) = record.take() {
        finish_record(parsed, *pid, previous, resolve_scope);
    }
    if tag == b'f' {
        *record = Some(FileRecord::default());
        return;
    }
    *pid = value
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid > 0);
    if pid.is_none() {
        parsed.malformed += 1;
    }
}

fn apply_file_field<'a>(
    record: &mut FileRecord<'a>,
    tag: u8,
    value: Result<&'a str, std::str::Utf8Error>,
) {
    let Ok(value) = value else {
        record.invalid = true;
        return;
    };
    match tag {
        b't' => {
            record.family = match value {
                "IPv4" => Some(Family::Ipv4),
                "IPv6" => Some(Family::Ipv6),
                _ => None,
            }
        }
        b'P' => {
            record.protocol = match value {
                "TCP" => Some(Protocol::Tcp),
                "UDP" => Some(Protocol::Udp),
                _ => None,
            }
        }
        b'n' => record.name = Some(value),
        b'T' => {
            if let Some(state) = value.strip_prefix("ST=") {
                record.state = Some(state);
            }
        }
        _ => {}
    }
}

fn remote_order(a: Option<&Endpoint>, b: Option<&Endpoint>) -> Ordering {
    let key = |endpoint: &Endpoint| (endpoint.port, endpoint.scope_id);
    match (a, b) {
        (Some(a), Some(b)) => key(a).cmp(&key(b)).then_with(|| a.address.cmp(&b.address)),
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn socket_order(a: &RawSocket, b: &RawSocket) -> Ordering {
    let key = |row: &RawSocket| {
        (
            row.pid,
            if row.family == Family::Ipv4 { 4 } else { 6 },
            row.local.port,
            row.local.scope_id,
            row.tcp_state,
        )
    };
    key(a)
        .cmp(&key(b))
        .then_with(|| a.local.address.cmp(&b.local.address))
        .then_with(|| remote_order(a.remote.as_ref(), b.remote.as_ref()))
}

/// `-F0pftPnT` emits NUL-terminated fields, with newlines between process/file sets. File boundaries, not
/// newlines, delimit records. Unknown T fields (queues, etc.) never become TCP state or direction evidence.
pub(super) fn parse_lsof(bytes: &[u8], resolve_scope: fn(&str) -> Option<u32>) -> ParsedLsof {
    let mut parsed = ParsedLsof::default();
    let mut pid = None;
    let mut record: Option<FileRecord<'_>> = None;
    for field in bytes.split_inclusive(|byte| *byte == 0) {
        let field = field.strip_prefix(b"\n").unwrap_or(field);
        if field.is_empty() {
            continue;
        }
        let Some(field) = field.strip_suffix(&[0]) else {
            parsed.malformed += 1;
            if let Some(record) = record.as_mut() {
                record.invalid = true;
            }
            break;
        };
        if field.is_empty() {
            continue;
        }
        let (tag, value) = (field[0], std::str::from_utf8(&field[1..]));
        if tag == b'p' || tag == b'f' {
            start_section(
                &mut parsed,
                &mut pid,
                &mut record,
                tag,
                value,
                resolve_scope,
            );
        } else if let Some(record) = record.as_mut() {
            apply_file_field(record, tag, value);
        }
    }
    if let Some(record) = record {
        finish_record(&mut parsed, pid, record, resolve_scope);
    }
    // Several descriptors in one process may refer to the same socket. Keep one row per observed owner/endpoint.
    parsed.tcp.sort_unstable_by(socket_order);
    parsed.tcp.dedup();
    parsed.udp.sort_unstable_by(socket_order);
    parsed.udp.dedup();
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(zone: &str) -> Option<u32> {
        if zone == "en0" {
            Some(7)
        } else {
            None
        }
    }

    #[test]
    fn parses_nul_fields_and_keeps_state_evidence_separate_from_peer_presence() {
        let parsed = parse_lsof(b"p123\0\nf9\0tIPv4\0PTCP\0n*:8080\0TST=LISTEN\0\nf10\0tIPv4\0PTCP\0n127.0.0.1:50123->127.0.0.1:8080\0TST=ESTABLISHED\0TQR=0\0\nf11\0tIPv4\0PTCP\0n10.0.0.1:1234->10.0.0.2:443\0TST=SYN_SENT\0\n", scope);
        assert_eq!(parsed.malformed, 0);
        assert_eq!(parsed.tcp.len(), 3);
        let listener = parsed
            .tcp
            .iter()
            .find(|row| row.local.port == 8080)
            .unwrap();
        assert_eq!(listener.pid, 123);
        assert_eq!(listener.local.address, "0.0.0.0");
        assert_eq!(listener.remote, None);
        assert_eq!(listener.tcp_state, Some(2));
        assert_eq!(
            parsed
                .tcp
                .iter()
                .find(|row| row.local.port == 50123)
                .unwrap()
                .tcp_state,
            Some(5)
        );
        assert_eq!(
            parsed
                .tcp
                .iter()
                .find(|row| row.local.port == 1234)
                .unwrap()
                .tcp_state,
            Some(3)
        );
    }

    #[test]
    fn parses_ipv6_scopes_udp_peers_and_process_boundaries() {
        let parsed = parse_lsof(b"p42\0\nf3\0tIPv6\0PUDP\0n[fe80::1%en0]:5353->[fe80::2%7]:5353\0\nf4\0tIPv6\0PUDP\0n*:123\0\np43\0\nf3\0tIPv6\0PTCP\0n[::1]:443->[::1]:60000\0TST=SYN_RCVD\0\n", scope);
        assert_eq!(parsed.malformed, 0);
        assert_eq!(parsed.udp.len(), 2);
        let udp = parsed
            .udp
            .iter()
            .find(|row| row.local.port == 5353)
            .unwrap();
        assert_eq!(udp.pid, 42);
        assert_eq!(udp.local.scope_id, Some(7));
        assert_eq!(udp.remote.as_ref().unwrap().scope_id, Some(7));
        assert_eq!(udp.tcp_state, None);
        assert_eq!(
            parsed
                .udp
                .iter()
                .find(|row| row.local.port == 123)
                .unwrap()
                .local
                .address,
            "::"
        );
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
        assert_eq!(
            endpoint("[2001:0db8::1]:443", Family::Ipv6, scope)
                .unwrap()
                .address,
            "2001:db8::1"
        );
    }
}
