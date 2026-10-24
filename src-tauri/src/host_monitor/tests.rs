use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Value};

use super::snapshot::{HostSnapshot, RawCollection, RawProcess, Recheck};
use super::tables::{self, RawSocket};
use super::*;

// ---------------------------------------------------------------- raw table encoding

fn put_port(row: &mut Vec<u8>, port: u16) {
    // Network byte order in the first two bytes of the DWORD.
    row.extend_from_slice(&[(port >> 8) as u8, port as u8, 0, 0]);
}

fn tcp4_row(state: u32, local: [u8; 4], local_port: u16, remote: [u8; 4], remote_port: u16, pid: u32) -> Vec<u8> {
    let mut row = state.to_le_bytes().to_vec();
    row.extend_from_slice(&local);
    put_port(&mut row, local_port);
    row.extend_from_slice(&remote);
    put_port(&mut row, remote_port);
    row.extend_from_slice(&pid.to_le_bytes());
    row
}

fn tcp6_row(state: u32, local: [u8; 16], scope: u32, local_port: u16, remote: [u8; 16], remote_scope: u32, remote_port: u16, pid: u32) -> Vec<u8> {
    let mut row = local.to_vec();
    row.extend_from_slice(&scope.to_le_bytes());
    put_port(&mut row, local_port);
    row.extend_from_slice(&remote);
    row.extend_from_slice(&remote_scope.to_le_bytes());
    put_port(&mut row, remote_port);
    row.extend_from_slice(&state.to_le_bytes());
    row.extend_from_slice(&pid.to_le_bytes());
    row
}

fn table_of(announced: u32, rows: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = announced.to_le_bytes().to_vec();
    for row in rows {
        bytes.extend_from_slice(row);
    }
    bytes
}

fn ip6(text: &str) -> [u8; 16] {
    text.parse::<std::net::Ipv6Addr>().unwrap().octets()
}

#[test]
fn tcp4_ports_use_network_byte_order_and_listeners_have_no_peer() {
    let buffer = table_of(
        2,
        &[
            tcp4_row(2, [0, 0, 0, 0], 8080, [0, 0, 0, 0], 0, 100),
            tcp4_row(5, [192, 168, 1, 5], 50000, [93, 184, 216, 34], 443, 200),
        ],
    );
    let parsed = tables::parse_tcp4(&buffer).unwrap();
    assert!(!parsed.truncated);
    let [listener, connection] = &parsed.rows[..] else { panic!("two rows") };
    assert_eq!((listener.local.port, listener.pid, listener.tcp_state), (8080, 100, Some(2)));
    assert_eq!(listener.remote, None, "a wildcard peer with port zero is no peer");
    assert_eq!(connection.local.address, "192.168.1.5");
    assert_eq!(connection.local.port, 50000);
    let peer = connection.remote.as_ref().unwrap();
    assert_eq!((peer.address.as_str(), peer.port, peer.scope_id), ("93.184.216.34", 443, None));
    assert_eq!(connection.pid, 200);
}

#[test]
fn tcp_tables_with_lying_or_short_headers_never_read_out_of_bounds() {
    let row = tcp4_row(5, [10, 0, 0, 1], 1, [10, 0, 0, 2], 2, 3);
    let truncated = tables::parse_tcp4(&table_of(5, &[row.clone()])).unwrap();
    assert!(truncated.truncated, "the header announced more rows than the buffer holds");
    assert_eq!(truncated.rows.len(), 1);

    let partial_row = &row[..row.len() - 1];
    let shorter = tables::parse_tcp4(&table_of(1, &[partial_row.to_vec()])).unwrap();
    assert!(shorter.truncated && shorter.rows.is_empty(), "a row cut short is not decoded");

    assert!(tables::parse_tcp4(&[1, 0]).is_err());
    assert!(tables::parse_tcp6(&[]).is_err());
    assert!(tables::parse_udp4(&table_of(u32::MAX, &[])).unwrap().rows.is_empty());
    assert!(tables::parse_udp6(&table_of(0, &[])).unwrap().rows.is_empty());
}

#[test]
fn tcp6_keeps_scope_ids_and_compresses_addresses() {
    let buffer = table_of(
        1,
        &[tcp6_row(3, ip6("fe80::1"), 7, 51000, ip6("2001:db8::1"), 0, 443, 9)],
    );
    let parsed = tables::parse_tcp6(&buffer).unwrap();
    let row = &parsed.rows[0];
    assert_eq!(row.family, Family::Ipv6);
    assert_eq!((row.local.address.as_str(), row.local.port, row.local.scope_id), ("fe80::1", 51000, Some(7)));
    let peer = row.remote.as_ref().unwrap();
    assert_eq!((peer.address.as_str(), peer.port, peer.scope_id), ("2001:db8::1", 443, None));
    assert_eq!(row.tcp_state, Some(3));
}

#[test]
fn udp_rows_decode_ports_and_have_no_peer() {
    let mut v4 = vec![127, 0, 0, 1];
    put_port(&mut v4, 5353);
    v4.extend_from_slice(&77u32.to_le_bytes());
    let parsed = tables::parse_udp4(&table_of(1, &[v4])).unwrap();
    assert_eq!((parsed.rows[0].local.port, parsed.rows[0].pid, parsed.rows[0].protocol), (5353, 77, Protocol::Udp));
    assert!(parsed.rows[0].remote.is_none() && parsed.rows[0].tcp_state.is_none());

    let mut v6 = ip6("::1").to_vec();
    v6.extend_from_slice(&3u32.to_le_bytes());
    put_port(&mut v6, 1900);
    v6.extend_from_slice(&88u32.to_le_bytes());
    let parsed = tables::parse_udp6(&table_of(1, &[v6])).unwrap();
    assert_eq!(
        (parsed.rows[0].local.address.as_str(), parsed.rows[0].local.port, parsed.rows[0].local.scope_id, parsed.rows[0].pid),
        ("::1", 1900, Some(3), 88)
    );
}

#[test]
fn only_handshake_states_reveal_direction() {
    use tables::direction_of;
    assert_eq!(direction_of(2), (Direction::Listener, DirectionEvidence::Listen));
    assert_eq!(direction_of(3), (Direction::Outbound, DirectionEvidence::SynSent));
    assert_eq!(direction_of(4), (Direction::Inbound, DirectionEvidence::SynReceived));
    for state in [1, 5, 6, 7, 8, 9, 10, 11, 12, 99] {
        assert_eq!(direction_of(state), (Direction::Unknown, DirectionEvidence::Unavailable), "state {state}");
    }
    assert_eq!(tables::tcp_state_name(5), "ESTABLISHED");
    assert_eq!(tables::tcp_state_name(77), "UNKNOWN(77)");
}

#[test]
fn address_scopes_and_external_peers() {
    use AddressScope::*;
    for (address, expected) in [
        ("127.0.0.1", Loopback),
        ("0.0.0.0", Unspecified),
        ("10.1.2.3", Private),
        ("172.16.0.1", Private),
        ("172.32.0.1", Public),
        ("192.168.0.9", Private),
        ("169.254.1.1", LinkLocal),
        ("100.64.0.1", Other),
        ("224.0.0.1", Other),
        ("8.8.8.8", Public),
        ("::1", Loopback),
        ("::", Unspecified),
        ("fe80::1", LinkLocal),
        ("fd00::1", Private),
        ("2606:4700::1111", Public),
        ("::ffff:127.0.0.1", Loopback),
        ("::ffff:8.8.8.8", Public),
        ("ff02::1", Other),
    ] {
        assert_eq!(tables::address_scope(address), expected, "{address}");
    }
    let peer = |address: &str| Some(Endpoint { address: address.into(), port: 443, scope_id: None });
    assert!(tables::is_external(peer("192.168.0.9").as_ref()), "private LAN peers are external");
    assert!(tables::is_external(peer("8.8.8.8").as_ref()));
    assert!(!tables::is_external(peer("127.0.0.1").as_ref()));
    assert!(!tables::is_external(peer("0.0.0.0").as_ref()));
    assert!(!tables::is_external(None));
}

// ---------------------------------------------------------------- snapshot construction

fn process(pid: u32, parent: u32, name: &str, creation: Option<u64>) -> RawProcess {
    RawProcess {
        pid,
        parent_pid: parent,
        threads: 1,
        name: name.into(),
        creation,
        availability: if creation.is_some() { Availability::Available } else { Availability::Limited },
    }
}

fn socket(protocol: Protocol, pid: u32, local: (&str, u16), remote: Option<(&str, u16)>, state: Option<u32>) -> RawSocket {
    let endpoint = |(address, port): (&str, u16)| Endpoint { address: address.into(), port, scope_id: None };
    RawSocket {
        family: if local.0.contains(':') { Family::Ipv6 } else { Family::Ipv4 },
        protocol,
        pid,
        local: endpoint(local),
        remote: remote.map(endpoint),
        tcp_state: state,
    }
}

fn tcp(pid: u32, local: (&str, u16), remote: Option<(&str, u16)>, state: u32) -> RawSocket {
    socket(Protocol::Tcp, pid, local, remote, Some(state))
}

fn collection(processes: Vec<RawProcess>, tcp: Vec<RawSocket>, udp: Vec<RawSocket>, rechecks: &[(u32, Recheck)]) -> RawCollection {
    RawCollection {
        started_at: 1_000,
        completed_at: 1_040,
        duration_ms: 40,
        processes,
        tcp,
        udp,
        rechecks: rechecks.iter().copied().collect(),
        sections: Sections {
            processes: SectionState::ok(),
            tcp4: SectionState::ok(),
            tcp6: SectionState::ok(),
            udp4: SectionState::ok(),
            udp6: SectionState::ok(),
        },
    }
}

fn build(raw: RawCollection, roots: &[ManagedRoot]) -> HostSnapshot {
    HostSnapshot::build("snapshot".into(), "view".into(), "agent".into(), 1, raw, roots)
}

fn root(pid: u32, creation: u64, kind: RootKind) -> ManagedRoot {
    ManagedRoot {
        instance_id: "agent".into(),
        generation: 1,
        kind,
        identity: ProcessIdentity { pid, creation_time_100ns: creation },
    }
}

fn all_processes(snapshot: &HostSnapshot) -> Vec<Value> {
    snapshot.query(&HostMonitorQuery::Processes { text: String::new(), offset: 0, limit: 250 }).items
}

fn all_tcp(snapshot: &HostSnapshot, view: TcpView) -> Vec<Value> {
    snapshot.query(&HostMonitorQuery::Tcp { view, text: String::new(), offset: 0, limit: 250 }).items
}

fn owner_kind(rows: &[Value], pid: u32) -> Value {
    rows.iter().find(|row| row["pid"] == pid).unwrap_or_else(|| panic!("pid {pid}"))["owner"]["kind"].clone()
}

#[test]
fn ownership_requires_creation_identity_and_verified_ancestry() {
    let processes = vec![
        process(1, 0, "launcher.exe", Some(10)),
        process(10, 1, "omp.exe", Some(1_000)),
        process(11, 10, "child.exe", Some(1_500)),
        process(12, 11, "grandchild.exe", Some(1_600)),
        // Parent PID matches but the "child" is older than the parent: the parent PID was recycled.
        process(13, 10, "stranger.exe", Some(900)),
        // Identity cannot be read: no claim.
        process(14, 10, "locked.exe", None),
        // Same PID as a root but another creation time: a different process.
        process(20, 1, "reused.exe", Some(7_000)),
        process(21, 20, "reused-child.exe", Some(8_000)),
        process(30, 1, "chrome.exe", Some(2_000)),
        process(31, 30, "renderer.exe", Some(2_100)),
        // Parent already exited (not in the list): unattributed.
        process(40, 999, "orphan.exe", Some(3_000)),
    ];
    let roots = [root(10, 1_000, RootKind::Agent), root(20, 5_000, RootKind::Agent), root(30, 2_000, RootKind::Browser)];
    let snapshot = build(collection(processes, vec![], vec![], &[]), &roots);
    let rows = all_processes(&snapshot);
    assert_eq!(owner_kind(&rows, 10), "agentRoot");
    assert_eq!(owner_kind(&rows, 11), "descendant");
    assert_eq!(owner_kind(&rows, 12), "descendant");
    assert_eq!(owner_kind(&rows, 30), "browserRoot");
    assert_eq!(owner_kind(&rows, 31), "browserDescendant");
    for pid in [1, 13, 14, 20, 21, 40] {
        assert_eq!(owner_kind(&rows, pid), Value::Null, "pid {pid} must stay unattributed");
    }
    let owned = rows.iter().find(|row| row["pid"] == 11).unwrap();
    assert_eq!(owned["owner"]["instanceId"], "agent");
    assert_eq!(owned["owner"]["generation"], 1);
}

#[test]
fn process_rows_carry_identity_but_no_telemetry() {
    let snapshot = build(
        collection(
            vec![process(7, 0, "a.exe", Some(132_000_000_000_000_000)), process(8, 7, "b.exe", None), process(4, 0, "System", None)],
            vec![],
            vec![],
            &[],
        ),
        &[],
    );
    let rows = all_processes(&snapshot);
    let a = rows.iter().find(|row| row["pid"] == 7).unwrap();
    assert_eq!(a["creationTime100ns"], "132000000000000000", "FILETIME is a string, beyond a JS safe integer");
    assert_eq!(a["key"], "7:132000000000000000");
    assert_eq!(a["parentPid"], Value::Null, "parent 0 is no parent");
    assert_eq!(a["availability"], "available");
    let b = rows.iter().find(|row| row["pid"] == 8).unwrap();
    assert_eq!((b["creationTime100ns"].clone(), b["availability"].clone()), (Value::Null, json!("limited")));
    assert!(b["key"].as_str().unwrap().starts_with("8:local-"), "unknown start time uses a snapshot-local key");
    let system = rows.iter().find(|row| row["pid"] == 4).unwrap();
    assert_eq!(system["key"], "4:boot");
    let mut fields: Vec<_> = a.as_object().unwrap().keys().cloned().collect();
    fields.sort();
    assert_eq!(
        fields,
        ["availability", "creationTime100ns", "key", "name", "owner", "parentPid", "pid", "threadCount"],
        "no CPU, memory, arguments, environment, path or user fields"
    );
}

#[test]
fn sockets_are_attributed_only_when_identity_is_stable_around_the_table_read() {
    let processes = vec![
        process(30, 1, "stable.exe", Some(100)),
        process(31, 1, "recycled.exe", Some(100)),
        process(32, 1, "locked.exe", None),
        process(4, 0, "System", None),
        process(33, 1, "gone.exe", Some(100)),
    ];
    let tcp_rows = vec![
        tcp(30, ("10.0.0.2", 1000), Some(("8.8.8.8", 443)), 5),
        tcp(31, ("10.0.0.2", 1001), Some(("8.8.8.8", 443)), 5),
        tcp(32, ("10.0.0.2", 1002), Some(("8.8.8.8", 443)), 5),
        tcp(4, ("10.0.0.2", 445), None, 2),
        tcp(40, ("10.0.0.2", 1003), Some(("8.8.4.4", 443)), 5),
        tcp(0, ("10.0.0.2", 1004), Some(("8.8.4.4", 443)), 11),
        tcp(33, ("10.0.0.2", 1005), Some(("8.8.4.4", 443)), 5),
    ];
    let rechecks = [
        (30, Recheck::Creation(100)),
        (31, Recheck::Creation(999)),
        (32, Recheck::Unreadable),
        (33, Recheck::Gone),
        (40, Recheck::Creation(5)),
    ];
    let snapshot = build(collection(processes, tcp_rows, vec![], &rechecks), &[]);
    let rows = all_tcp(&snapshot, TcpView::All);
    let by_port = |port: u64| rows.iter().find(|row| row["local"]["port"] == port).unwrap().clone();
    assert_eq!(by_port(1000)["processName"], "stable.exe");
    assert_eq!(by_port(1000)["processKey"], "30:100");
    for port in [1001, 1002, 1003, 1005] {
        let row = by_port(port);
        assert_eq!((row["processKey"].clone(), row["processName"].clone()), (Value::Null, Value::Null), "port {port}");
        assert!(row["pid"].is_number(), "the PID is kept even when it is not attributed");
    }
    assert_eq!(by_port(445)["processName"], "System", "System has a fixed identity");
    assert_eq!(by_port(1004)["pid"], Value::Null, "owner PID 0 is no owner");
}

#[test]
fn tcp_direction_views_and_counts_do_not_infer_from_ports() {
    let tcp_rows = vec![
        tcp(1, ("0.0.0.0", 80), None, 2),
        tcp(1, ("10.0.0.2", 2000), Some(("8.8.8.8", 443)), 3),
        tcp(1, ("10.0.0.2", 2001), Some(("8.8.8.8", 8443)), 5),
        tcp(1, ("10.0.0.2", 80), Some(("8.8.4.4", 5555)), 4),
        tcp(1, ("127.0.0.1", 2002), Some(("127.0.0.1", 9000)), 5),
        tcp(1, ("10.0.0.2", 2003), Some(("192.168.0.9", 22)), 5),
    ];
    let snapshot = build(collection(vec![process(1, 0, "a.exe", Some(5))], tcp_rows, vec![], &[(1, Recheck::Creation(5))]), &[]);
    let summary = snapshot.summary();
    assert_eq!(summary.external_tcp_count, Some(4), "private LAN peers count as external; loopback and listeners do not");
    assert_eq!(summary.confirmed_outbound_count, Some(1), "only SYN_SENT is confirmed outbound");
    assert_eq!(summary.unknown_direction_count, Some(2), "established external rows stay unknown");
    let ports = |view| -> Vec<u64> { all_tcp(&snapshot, view).iter().map(|row| row["local"]["port"].as_u64().unwrap()).collect() };
    assert_eq!(ports(TcpView::Listeners), [80]);
    assert_eq!(ports(TcpView::ConfirmedOutbound), [2000]);
    assert_eq!(ports(TcpView::UnknownDirection).len(), 2);
    assert_eq!(ports(TcpView::External).len(), 4);
    assert_eq!(ports(TcpView::All).len(), 6);
    let outbound = all_tcp(&snapshot, TcpView::ConfirmedOutbound);
    assert_eq!((outbound[0]["direction"].clone(), outbound[0]["directionEvidence"].clone()), (json!("outbound"), json!("synSent")));
    let inbound = all_tcp(&snapshot, TcpView::All).into_iter().find(|row| row["state"] == "SYN_RCVD").unwrap();
    assert_eq!((inbound["direction"].clone(), inbound["directionEvidence"].clone()), (json!("inbound"), json!("synReceived")));
    let established = all_tcp(&snapshot, TcpView::UnknownDirection);
    assert!(established.iter().all(|row| row["directionEvidence"] == "unavailable"));
}

#[test]
fn udp_is_a_separate_section_without_direction_or_peer() {
    let udp = vec![socket(Protocol::Udp, 1, ("0.0.0.0", 5353), None, None)];
    let snapshot = build(collection(vec![process(1, 0, "a.exe", Some(5))], vec![], udp, &[(1, Recheck::Creation(5))]), &[]);
    assert_eq!(snapshot.summary().udp_endpoint_count, Some(1));
    assert_eq!(snapshot.summary().external_tcp_count, Some(0));
    let page = snapshot.query(&HostMonitorQuery::Udp { text: String::new(), offset: 0, limit: 10 });
    assert_eq!((page.section, page.total), ("udp", 1));
    let row = &page.items[0];
    assert_eq!((row["protocol"].clone(), row["direction"].clone(), row["remote"].clone(), row["state"].clone()), (json!("udp"), json!("unknown"), Value::Null, Value::Null));
    assert!(all_tcp(&snapshot, TcpView::All).is_empty());
}

#[test]
fn unavailable_sections_are_null_and_partial_sections_stay_labelled() {
    let mut raw = collection(vec![process(1, 0, "a.exe", Some(5))], vec![tcp(1, ("10.0.0.2", 1), Some(("8.8.8.8", 443)), 5)], vec![], &[(1, Recheck::Creation(5))]);
    raw.sections.tcp6 = SectionState::unavailable("IPv6 TCP table unavailable");
    raw.sections.udp4 = SectionState::unavailable("x");
    raw.sections.udp6 = SectionState::unavailable("y");
    raw.sections.processes = SectionState::partial("stopped early");
    let summary = build(raw, &[]).summary();
    assert_eq!(summary.external_tcp_count, Some(1), "one family is still readable");
    assert_eq!(summary.udp_endpoint_count, None, "both UDP families failed: unknown, not zero");
    assert_eq!(summary.process_count, Some(1));
    assert_eq!(summary.sections.processes.status, SectionStatus::Partial);
    assert_eq!(summary.sections.tcp6.status, SectionStatus::Unavailable);
    assert_eq!(summary.scope, "wholeComputer");

    let mut none = collection(vec![], vec![], vec![], &[]);
    none.sections.processes = SectionState::unavailable("denied");
    none.sections.tcp4 = SectionState::unavailable("a");
    none.sections.tcp6 = SectionState::unavailable("b");
    let summary = build(none, &[]).summary();
    assert_eq!((summary.process_count, summary.limited_process_count, summary.external_tcp_count, summary.confirmed_outbound_count, summary.unknown_direction_count), (None, None, None, None, None));
}

fn numbered(count: u32) -> HostSnapshot {
    let processes = (0..count).map(|i| process(100 + i, 0, &format!("app-{i:03}.exe"), Some(u64::from(i) + 1))).collect();
    build(collection(processes, vec![], vec![], &[]), &[])
}

fn processes_page(snapshot: &HostSnapshot, text: &str, offset: usize, limit: usize) -> HostMonitorPage {
    snapshot.query(&HostMonitorQuery::Processes { text: text.into(), offset, limit })
}

#[test]
fn queries_filter_the_whole_section_before_paging_and_clamp_to_the_last_page() {
    let snapshot = numbered(300);
    let first = processes_page(&snapshot, "", 0, 0);
    assert_eq!((first.total, first.items.len(), first.next_offset), (300, 100, Some(100)), "default page is 100");
    assert_eq!(processes_page(&snapshot, "", 0, 1_000).items.len(), 250, "pages are capped at 250");
    let last = processes_page(&snapshot, "", 10_000, 100);
    assert_eq!((last.offset, last.items.len(), last.next_offset), (200, 100, None));
    let uneven = processes_page(&snapshot, "app-00", 5_000, 4);
    assert_eq!((uneven.total, uneven.offset, uneven.items.len(), uneven.next_offset), (10, 8, 2, None), "totals are filtered totals");
    let empty = processes_page(&snapshot, "no-such-process", 40, 10);
    assert_eq!((empty.total, empty.offset, empty.items.len(), empty.next_offset), (0, 0, 0, None));
    // Filtering is applied to the full section, not to the page: a late match is found from offset 0.
    let late = processes_page(&snapshot, "app-299", 0, 5);
    assert_eq!((late.total, late.items.len()), (1, 1));
    let by_pid = processes_page(&snapshot, "  399 ", 0, 5);
    assert_eq!(by_pid.total, 1);
    assert_eq!(by_pid.items[0]["name"], "app-299.exe");
    let walked: Vec<usize> = (0..)
        .scan(Some(0usize), |next, _| {
            let offset = (*next)?;
            let page = processes_page(&snapshot, "app-1", offset, 17);
            *next = page.next_offset;
            Some(page.items.len())
        })
        .collect();
    assert_eq!(walked.iter().sum::<usize>(), 100, "paging by nextOffset visits every filtered row once");
}

#[test]
fn endpoint_and_process_text_search_covers_name_pid_and_endpoints() {
    let tcp_rows = vec![
        tcp(30, ("10.0.0.2", 1000), Some(("93.184.216.34", 443)), 5),
        tcp(30, ("10.0.0.2", 1001), Some(("8.8.8.8", 53)), 5),
    ];
    let snapshot = build(
        collection(vec![process(30, 1, "Browser.EXE", Some(100))], tcp_rows, vec![], &[(30, Recheck::Creation(100))]),
        &[],
    );
    let total = |text: &str| snapshot.query(&HostMonitorQuery::Tcp { view: TcpView::All, text: text.into(), offset: 0, limit: 10 }).total;
    assert_eq!(total("browser"), 2, "case-insensitive process name");
    assert_eq!(total("93.184.216.34:443"), 1);
    assert_eq!(total("8.8.8.8"), 1);
    assert_eq!(total("1001"), 1);
    assert_eq!(total("30"), 2);
    assert_eq!(total("nope"), 0);
}

#[test]
fn ordering_and_hash_are_deterministic_and_independent_of_random_ids() {
    let a = vec![process(9, 0, "zeta.exe", Some(3)), process(2, 0, "Alpha.exe", Some(2)), process(1, 0, "alpha.exe", Some(1))];
    let one = HostSnapshot::build("id-1".into(), "view-a".into(), "agent".into(), 1, collection(a.clone(), vec![], vec![], &[]), &[]);
    let two = HostSnapshot::build("id-2".into(), "view-b".into(), "agent".into(), 1, collection(a, vec![], vec![], &[]), &[]);
    assert_eq!(one.hash, two.hash);
    assert_eq!(one.hash.len(), 64);
    let names: Vec<_> = all_processes(&one).iter().map(|row| (row["name"].as_str().unwrap().to_lowercase(), row["pid"].as_u64().unwrap())).collect();
    let expected: Vec<(String, u64)> = vec![("alpha.exe".into(), 1), ("alpha.exe".into(), 2), ("zeta.exe".into(), 9)];
    assert_eq!(names, expected, "name, then PID");
    let changed = build(collection(vec![process(9, 0, "zeta.exe", Some(4))], vec![], vec![], &[]), &[]);
    assert_ne!(one.hash, changed.hash);
}

// ---------------------------------------------------------------- service lifecycle

type Behaviour = Arc<dyn Fn(&AtomicBool) -> Result<RawCollection, String> + Send + Sync>;

struct Fake {
    generation: AtomicU64,
    project: Mutex<String>,
    calls: AtomicUsize,
    behaviour: Mutex<Behaviour>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            generation: AtomicU64::new(1),
            project: Mutex::new("C:/work/project".into()),
            calls: AtomicUsize::new(0),
            behaviour: Mutex::new(Arc::new(|_: &AtomicBool| Ok(sample(1)))),
        })
    }
    fn behave(&self, behaviour: impl Fn(&AtomicBool) -> Result<RawCollection, String> + Send + Sync + 'static) {
        *self.behaviour.lock() = Arc::new(behaviour);
    }
}

impl Environment for Fake {
    fn supported(&self) -> bool {
        true
    }
    fn instance(&self, instance_id: &str) -> Option<InstanceContext> {
        (instance_id == "agent").then(|| InstanceContext { generation: self.generation.load(Ordering::SeqCst), project_path: self.project.lock().clone() })
    }
    fn roots(&self) -> Vec<ManagedRoot> {
        Vec::new()
    }
    fn collect(&self, cancel: &AtomicBool) -> Result<RawCollection, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let behaviour = self.behaviour.lock().clone();
        behaviour(cancel)
    }
}

fn sample(processes: u32) -> RawCollection {
    collection((0..processes).map(|i| process(100 + i, 0, &format!("p{i}.exe"), Some(u64::from(i) + 1))).collect(), vec![], vec![], &[])
}

fn service() -> (Arc<HostMonitorService>, Arc<Fake>, String) {
    let fake = Fake::new();
    let service = HostMonitorService::with_environment(fake.clone());
    let view = service.open("agent", 1).unwrap().view_id;
    (service, fake, view)
}

fn page(service: &HostMonitorService, snapshot: &str) -> Result<HostMonitorPage, String> {
    service.read(snapshot, &HostMonitorQuery::Processes { text: String::new(), offset: 0, limit: 10 })
}

fn held(service: &HostMonitorService) -> usize {
    service.store.lock().snapshots.len()
}

#[test]
fn open_validates_the_instance_without_starting_it_and_captures_context() {
    let fake = Fake::new();
    let service = HostMonitorService::with_environment(fake.clone());
    assert_eq!(service.open("missing", 1).unwrap_err(), "unknown_instance");
    assert_eq!(service.open("agent", 2).unwrap_err(), "generation_mismatch");
    let opened = service.open("agent", 1).unwrap();
    assert_eq!(opened.context, HostMonitorContext { instance_id: "agent".into(), generation: 1, project_path: "C:/work/project".into() });
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0, "opening never samples");
    let wire = serde_json::to_value(&opened).unwrap();
    assert_eq!(wire["context"]["projectPath"], "C:/work/project");
    assert!(wire["viewId"].is_string());
}

#[test]
fn display_survives_replacements_until_the_next_collect_acknowledges_the_candidate() {
    let (service, _fake, view) = service();
    let first = service.collect(&view, None).unwrap();
    assert!(page(&service, &first.snapshot_id).is_ok());
    let second = service.collect(&view, Some(&first.snapshot_id)).unwrap();
    assert!(page(&service, &first.snapshot_id).is_ok() && page(&service, &second.snapshot_id).is_ok(), "A stays while B is a candidate");
    // The pane failed to load B's pages and still shows A: the new candidate replaces B, A is kept.
    let third = service.collect(&view, Some(&first.snapshot_id)).unwrap();
    assert_eq!(page(&service, &second.snapshot_id).unwrap_err(), "snapshot_expired");
    assert!(page(&service, &first.snapshot_id).is_ok());
    // The pane now shows C: the next collect acknowledges it and A is released.
    let fourth = service.collect(&view, Some(&third.snapshot_id)).unwrap();
    assert_eq!(page(&service, &first.snapshot_id).unwrap_err(), "snapshot_expired");
    assert!(page(&service, &third.snapshot_id).is_ok() && page(&service, &fourth.snapshot_id).is_ok());
    assert_eq!(held(&service), 2, "at most one display and one candidate");
}

#[test]
fn a_failed_collection_keeps_the_last_good_display() {
    let (service, fake, view) = service();
    let good = service.collect(&view, None).unwrap();
    fake.behave(|_| Err("section failure".into()));
    assert_eq!(service.collect(&view, Some(&good.snapshot_id)).unwrap_err(), "section failure");
    assert!(page(&service, &good.snapshot_id).is_ok());
    fake.behave(|_| Ok(sample(2)));
    assert!(service.collect(&view, Some(&good.snapshot_id)).is_ok(), "the busy flag was released by the failed pass");
}

#[test]
fn one_collection_per_view_and_close_cancels_the_pass_without_waiting_for_it() {
    let (service, fake, view) = service();
    let (entered_tx, entered) = mpsc::channel();
    fake.behave(move |cancel| {
        entered_tx.send(()).ok();
        while !cancel.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        Err("monitor_cancelled".into())
    });
    let worker = {
        let service = service.clone();
        let view = view.clone();
        std::thread::spawn(move || service.collect(&view, None))
    };
    entered.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(service.collect(&view, None).unwrap_err(), "collection_in_progress");
    service.close(&view);
    assert_eq!(worker.join().unwrap().unwrap_err(), "monitor_cancelled");
    service.close(&view);
    assert_eq!(service.collect(&view, None).unwrap_err(), "view_closed");
    assert_eq!(held(&service), 0);
}

#[test]
fn a_result_finishing_after_close_is_never_published() {
    let (service, fake, view) = service();
    let (entered_tx, entered) = mpsc::channel();
    let (release_tx, release) = mpsc::channel::<()>();
    let release = Mutex::new(release);
    fake.behave(move |_| {
        entered_tx.send(()).ok();
        release.lock().recv_timeout(Duration::from_secs(10)).ok();
        Ok(sample(3))
    });
    let worker = {
        let service = service.clone();
        let view = view.clone();
        std::thread::spawn(move || service.collect(&view, None))
    };
    entered.recv_timeout(Duration::from_secs(10)).unwrap();
    service.close(&view);
    release_tx.send(()).unwrap();
    assert_eq!(worker.join().unwrap().unwrap_err(), "monitor_cancelled");
    assert_eq!(held(&service), 0, "no snapshot outlives its cancelled view");
}

#[test]
fn context_changes_reject_collection_and_publication() {
    let (service, fake, view) = service();
    fake.generation.store(2, Ordering::SeqCst);
    assert_eq!(service.collect(&view, None).unwrap_err(), "generation_mismatch");
    fake.generation.store(1, Ordering::SeqCst);
    let moved = fake.clone();
    fake.behave(move |_| {
        *moved.project.lock() = "C:/elsewhere".into();
        Ok(sample(1))
    });
    assert_eq!(service.collect(&view, None).unwrap_err(), "generation_mismatch", "the workspace moved during the pass");
    assert_eq!(held(&service), 0);
    assert_eq!(service.open("agent", 1).unwrap().context.project_path, "C:/elsewhere");
}

#[test]
fn pinning_freezes_the_view_and_survives_view_close_until_released() {
    let (service, _fake, view) = service();
    let shown = service.collect(&view, None).unwrap();
    let pinned = service.pin_displayed(&shown.snapshot_id, "request-1").unwrap();
    assert_eq!(pinned.snapshot_id, shown.snapshot_id);
    assert_eq!(pinned.hash.len(), 64);
    assert_eq!(pinned.summary["snapshotId"], shown.snapshot_id.as_str());
    assert_eq!((pinned.started_at, pinned.completed_at, pinned.duration_ms), (1_000, 1_040, 40));
    assert!(service.view_frozen(&shown.snapshot_id));
    assert_eq!(service.collect(&view, Some(&shown.snapshot_id)).unwrap_err(), "monitor_frozen");
    assert!(!service.unfreeze_view(&shown.snapshot_id), "a pin still holds it");
    assert_eq!(service.pin_displayed(&shown.snapshot_id, "request-1").unwrap().hash, pinned.hash, "idempotent per request id");
    assert_eq!(service.pin_displayed("other", "request-1").unwrap_err(), "request_pin_conflict");
    assert_eq!(service.pin_displayed("other", "request-2").unwrap_err(), "snapshot_not_displayed");

    service.close(&view);
    assert_eq!(page(&service, &shown.snapshot_id).unwrap_err(), "snapshot_expired", "the closed view's UI can no longer read it");
    let query = || HostMonitorQuery::Processes { text: String::new(), offset: 0, limit: 10 };
    let read = service.read_pinned("request-1", &shown.snapshot_id, query()).unwrap();
    assert_eq!(read["total"], 1);
    assert_eq!(read["section"], "processes");
    assert_eq!(service.read_pinned("forged", &shown.snapshot_id, query()).unwrap_err(), "snapshot_expired");
    assert!(!service.view_frozen(&shown.snapshot_id), "a closed view is no longer frozen");

    service.release_pin("request-1");
    assert_eq!(service.read_pinned("request-1", &shown.snapshot_id, query()).unwrap_err(), "snapshot_expired");
    assert_eq!(held(&service), 0, "the last reference released the snapshot");
}

#[test]
fn release_and_unfreeze_resume_sampling_and_pinning_a_candidate_acknowledges_it() {
    let (service, _fake, view) = service();
    let first = service.collect(&view, None).unwrap();
    let second = service.collect(&view, Some(&first.snapshot_id)).unwrap();
    // The pane already shows B: pinning B acknowledges it and releases A.
    service.pin_displayed(&second.snapshot_id, "r").unwrap();
    assert_eq!(page(&service, &first.snapshot_id).unwrap_err(), "snapshot_expired");
    assert!(service.view_frozen(&second.snapshot_id));
    service.release_pin("r");
    assert!(service.view_frozen(&second.snapshot_id), "releasing a pin alone does not thaw the view");
    assert!(service.unfreeze_view(&second.snapshot_id));
    assert!(!service.view_frozen(&second.snapshot_id));
    assert!(service.collect(&view, Some(&second.snapshot_id)).is_ok());
    assert!(!service.unfreeze_view("expired"));
}

#[test]
fn views_are_bounded_and_the_least_recently_used_is_cancelled() {
    let fake = Fake::new();
    let service = HostMonitorService::with_environment(fake);
    let views: Vec<String> = (0..MAX_VIEWS + 1).map(|_| service.open("agent", 1).unwrap().view_id).collect();
    assert_eq!(service.collect(&views[0], None).unwrap_err(), "view_closed");
    assert!(service.collect(&views[MAX_VIEWS], None).is_ok());
}

#[test]
fn shutdown_cancels_views_and_refuses_new_work() {
    let (service, _fake, view) = service();
    let shown = service.collect(&view, None).unwrap();
    service.pin_displayed(&shown.snapshot_id, "r").unwrap();
    service.shutdown();
    assert_eq!(service.collect(&view, None).unwrap_err(), "monitor_closed");
    assert_eq!(service.open("agent", 1).unwrap_err(), "monitor_closed");
    assert_eq!(held(&service), 1, "only the explanation pin survives view cancellation");
}

#[test]
fn queries_deserialize_from_the_wire_shape_and_reject_unknown_fields() {
    let query: HostMonitorQuery = serde_json::from_value(json!({"kind":"tcp","view":"confirmedOutbound","text":"x","offset":3,"limit":10})).unwrap();
    assert_eq!(query, HostMonitorQuery::Tcp { view: TcpView::ConfirmedOutbound, text: "x".into(), offset: 3, limit: 10 });
    let query: HostMonitorQuery = serde_json::from_value(json!({"kind":"processes","offset":0,"limit":100})).unwrap();
    assert_eq!(query, HostMonitorQuery::Processes { text: String::new(), offset: 0, limit: 100 });
    assert!(serde_json::from_value::<HostMonitorQuery>(json!({"kind":"udp","text":"","offset":0,"limit":1,"pid":4})).is_err());
    assert!(serde_json::from_value::<HostMonitorQuery>(json!({"kind":"tcp","view":"everything","offset":0,"limit":1})).is_err());
    assert!(serde_json::from_value::<HostMonitorQuery>(json!({"kind":"udp","offset":-1,"limit":1})).is_err());
}

// ---------------------------------------------------------------- real native pass (opt-in)

#[cfg(windows)]
#[test]
fn native_snapshot_smoke() {
    use std::net::{TcpListener, TcpStream, UdpSocket};
    use std::process::{Command, Stdio};

    if std::env::var_os("OMP_HOST_MONITOR_NATIVE").is_none() {
        eprintln!("native_snapshot_smoke skipped: set OMP_HOST_MONITOR_NATIVE=1");
        return;
    }
    // Test-owned loopback endpoints and one scratch child; nothing else is created or touched.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen_port = listener.local_addr().unwrap().port();
    let client = TcpStream::connect(("127.0.0.1", listen_port)).unwrap();
    let client_port = client.local_addr().unwrap().port();
    let (_server, _) = listener.accept().unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    let udp_port = udp.local_addr().unwrap().port();
    let listener6 = TcpListener::bind("[::1]:0").ok();
    let client6 = listener6.as_ref().and_then(|l| TcpStream::connect(l.local_addr().unwrap()).ok());
    let mut child = Command::new("ping").args(["-n", "30", "127.0.0.1"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();

    let own = capture_identity(std::process::id()).expect("own process identity");
    let child_identity = capture_identity(child.id()).expect("scratch child identity");
    assert_ne!(own.creation_time_100ns, 0);
    let roots = [ManagedRoot { instance_id: "smoke".into(), generation: 1, kind: RootKind::Agent, identity: own }];
    let cancel = AtomicBool::new(false);
    let raw = super::native::collect(&cancel).expect("native collection");
    let snapshot = HostSnapshot::build("s".into(), "v".into(), "smoke".into(), 1, raw, &roots);
    let summary = snapshot.summary();
    assert_eq!(summary.sections.processes.status, SectionStatus::Ok);
    assert_eq!(summary.sections.tcp4.status, SectionStatus::Ok);
    assert_eq!(summary.sections.udp4.status, SectionStatus::Ok);
    assert!(summary.process_count.unwrap() > 2);
    assert!(summary.duration_ms < 30_000);

    let process_rows = snapshot.query(&HostMonitorQuery::Processes { text: String::new(), offset: 0, limit: 250 });
    let mut every_process = process_rows.items;
    let mut offset = process_rows.next_offset;
    while let Some(next) = offset {
        let more = snapshot.query(&HostMonitorQuery::Processes { text: String::new(), offset: next, limit: 250 });
        every_process.extend(more.items);
        offset = more.next_offset;
    }
    let find = |pid: u32| every_process.iter().find(|row| row["pid"] == pid).cloned().unwrap_or_else(|| panic!("pid {pid} listed"));
    let me = find(std::process::id());
    assert_eq!(me["key"], format!("{}:{}", own.pid, own.creation_time_100ns).as_str());
    assert_eq!(me["owner"]["kind"], "agentRoot");
    let scratch = find(child.id());
    assert_eq!(scratch["creationTime100ns"], child_identity.creation_time_100ns.to_string().as_str());
    assert_eq!(scratch["owner"]["kind"], "descendant", "the scratch child is attributed through verified ancestry");
    assert_eq!(scratch["parentPid"], std::process::id());

    let tcp = snapshot.query(&HostMonitorQuery::Tcp { view: TcpView::All, text: format!("{}", std::process::id()), offset: 0, limit: 250 }).items;
    let on = |port: u16| -> Vec<&Value> { tcp.iter().filter(|row| row["local"]["port"] == port).collect() };
    let listening = on(listen_port).into_iter().find(|row| row["state"] == "LISTEN").expect("listener row");
    assert_eq!((listening["direction"].clone(), listening["pid"].clone(), listening["remote"].clone()), (json!("listener"), json!(std::process::id()), Value::Null));
    assert_eq!(listening["processKey"], me["key"], "attributed through creation identity");
    let established = on(client_port).into_iter().find(|row| row["state"] == "ESTABLISHED").expect("client row");
    assert_eq!(established["remote"]["port"], listen_port, "literal ports, not byte-swapped");
    assert_eq!((established["direction"].clone(), established["directionEvidence"].clone()), (json!("unknown"), json!("unavailable")));
    assert!(on(listen_port).iter().any(|row| row["state"] == "ESTABLISHED" && row["remote"]["port"] == client_port), "the accepted side is listed too");

    let udp_rows = snapshot.query(&HostMonitorQuery::Udp { text: udp_port.to_string(), offset: 0, limit: 50 }).items;
    let udp_row = udp_rows.iter().find(|row| row["local"]["port"] == udp_port).expect("udp row");
    assert_eq!((udp_row["pid"].clone(), udp_row["remote"].clone(), udp_row["direction"].clone()), (json!(std::process::id()), Value::Null, json!("unknown")));

    if let (Some(listener6), Some(_client6)) = (&listener6, &client6) {
        assert_eq!(summary.sections.tcp6.status, SectionStatus::Ok);
        let port6 = listener6.local_addr().unwrap().port();
        let rows6 = snapshot.query(&HostMonitorQuery::Tcp { view: TcpView::Listeners, text: port6.to_string(), offset: 0, limit: 50 }).items;
        let row6 = rows6.iter().find(|row| row["local"]["port"] == port6).expect("IPv6 listener");
        assert_eq!((row6["family"].clone(), row6["local"]["address"].clone(), row6["addressScope"].clone()), (json!("ipv6"), json!("::1"), json!("loopback")));
    }

    // Cancellation before the pass starts is honoured without any native call being published.
    let stopped = AtomicBool::new(true);
    assert_eq!(super::native::collect(&stopped).err().as_deref(), Some("monitor_cancelled"));

    child.kill().ok();
    child.wait().ok();
    drop(listener6);
}

#[cfg(not(any(windows, target_os = "linux")))]
#[test]
fn native_snapshot_smoke() {
    assert_eq!(super::native::collect(&AtomicBool::new(false)).err().as_deref(), Some("unsupported_platform"));
    assert!(capture_identity(1).is_none());
}

// ---------------------------------------------------------------- Linux /proc decoding (pure, runs on any host)

// Captured from a live Linux host (little-endian: the kernel prints each address word in host byte order).
const PROC_TCP4: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:22B8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 2252060 2 0000000000000000 100 0 0 10 0
   1: 3600007F:0035 00000000:0000 0A 00000000:00000000 00:00000000 00000000   974        0 15736 1 0000000000000000 100 0 0 10 5
   2: BE08A8C0:A2C3 1D12AE03:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 2236367 2 0000000000000000 92 4 28 10 -1
   3: BE08A8C0:A2C4 0101A8C0:0050 02 00000000:00000000 00:00000000 00000000  1000        0 2236368 2 0000000000000000 92 4 28 10 -1
   4: 0100007F:1F90 0100007F:C350 03 00000000:00000000 00:00000000 00000000  1000        0 2236369 2 0000000000000000 92 4 28 10 -1
   5: BE08A8C0:A2C5 7797FB8E:01BB 06 00000000:00000000 03:000001A2 00000000     0        0 0 3 0000000000000000
";
const PROC_TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:14EB 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000   974        0 15727 1 0000000000000000 100 0 0 10 5
   1: 00000000000000000000000001000000:0277 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 40560 1 0000000000000000 100 0 0 10 0
   2: C80601209D4F09FFF5AA25E203001DA7:DCAA 5014002A06080F400000000016200000:01BB 08 00000000:00000082 00:00000000 00000000  1000        0 1034553 1 0000000000000000 92 4 28 10 -1
   3: 0000000000000000FFFF00000100007F:1F90 0000000000000000FFFF00000200A8C0:C351 01 00000000:00000000 00:00000000 00000000  1000        0 77 1 0000000000000000 92 4 28 10 -1
";
const PROC_UDP4: &str = "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
 7407: BE08A8C0:A2C3 1D12AE03:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 2236367 2 0000000000000000 0
31329: 3600007F:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000   974        0 15735 2 0000000000000000 0
";
const PROC_UDP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
 2214: 00000000000000000000000000000000:8E7A 00000000000000000000000000000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 1294348 2 0000000000000000 0
31822: 000080FE00000000848D8FDE1F70569D:0222 00000000000000000000000000000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 17023 2 0000000000000000 0
";

fn proc_rows(text: &str, family: Family, protocol: Protocol) -> tables::ParsedProcTable {
    tables::parse_proc_net(text.as_bytes(), family, protocol).unwrap()
}

#[cfg(target_endian = "little")]
#[test]
fn proc_net_tcp4_decodes_host_order_addresses_ports_states_and_inodes() {
    let parsed = proc_rows(PROC_TCP4, Family::Ipv4, Protocol::Tcp);
    assert_eq!((parsed.rows.len(), parsed.truncated, parsed.malformed), (6, false, 0));
    let row = |index: usize| &parsed.rows[index];
    let listener = row(0);
    assert_eq!(listener.socket.local, Endpoint { address: "127.0.0.1".into(), port: 0x22B8, scope_id: None });
    assert_eq!((listener.socket.remote.clone(), listener.socket.tcp_state, listener.inode), (None, Some(tables::TCP_LISTEN), 2252060), "a listener has no peer");
    assert_eq!(row(1).socket.local.address, "127.0.0.54", "the byte order is host order: 3600007F is 127.0.0.54");
    let established = row(2);
    assert_eq!(established.socket.local.address, "192.168.8.190");
    assert_eq!(established.socket.remote, Some(Endpoint { address: "3.174.18.29".into(), port: 443, scope_id: None }));
    assert_eq!(established.socket.tcp_state.map(tables::tcp_state_name).as_deref(), Some("ESTABLISHED"));
    assert_eq!(row(3).socket.tcp_state, Some(3), "SYN_SENT keeps the Windows number so the shared direction rules apply");
    assert_eq!(row(4).socket.tcp_state, Some(4), "SYN_RECV");
    let waiting = row(5);
    assert_eq!((waiting.inode, waiting.socket.tcp_state.map(tables::tcp_state_name).as_deref()), (0, Some("TIME_WAIT")), "TIME_WAIT sockets have no owner inode");
    assert!(parsed.rows.iter().all(|row| row.socket.pid == 0 && row.socket.family == Family::Ipv4 && row.socket.protocol == Protocol::Tcp));
}

#[cfg(target_endian = "little")]
#[test]
fn proc_net_tcp6_decodes_word_swapped_addresses_including_mapped_and_link_local() {
    let parsed = proc_rows(PROC_TCP6, Family::Ipv6, Protocol::Tcp);
    assert_eq!((parsed.rows.len(), parsed.malformed), (4, 0));
    assert_eq!(parsed.rows[0].socket.local.address, "::");
    assert_eq!(parsed.rows[0].socket.remote, None, "a wildcard peer with port 0 is no peer");
    assert_eq!(parsed.rows[1].socket.local, Endpoint { address: "::1".into(), port: 0x0277, scope_id: None });
    let global = &parsed.rows[2].socket;
    assert_eq!(global.local.address, "2001:6c8:ff09:4f9d:e225:aaf5:a71d:3", "each 32-bit word is byte-swapped on its own, the words keep their order");
    assert_eq!(global.remote.as_ref().map(|peer| (peer.address.as_str(), peer.port)), Some(("2a00:1450:400f:806::2016", 443)));
    let mapped = &parsed.rows[3].socket;
    assert_eq!((mapped.local.address.as_str(), mapped.remote.as_ref().map(|p| p.address.as_str())), ("::ffff:127.0.0.1", Some("::ffff:192.168.0.2")));
    assert!(tables::is_external(mapped.remote.as_ref()), "a mapped private LAN peer is external");
    assert!(!tables::is_external(Some(&Endpoint { address: "::ffff:127.0.0.1".into(), port: 1, scope_id: None })), "a mapped loopback peer is not");
}

#[cfg(target_endian = "little")]
#[test]
fn proc_net_udp_rows_have_no_peer_or_state_and_link_local_is_classified() {
    let udp4 = proc_rows(PROC_UDP4, Family::Ipv4, Protocol::Udp);
    assert_eq!(udp4.rows.len(), 2);
    assert!(udp4.rows.iter().all(|row| row.socket.remote.is_none() && row.socket.tcp_state.is_none()), "a connected UDP socket still lists only its local side, like the Windows table");
    assert_eq!((udp4.rows[1].socket.local.port, udp4.rows[1].inode), (53, 15735));
    let udp6 = proc_rows(PROC_UDP6, Family::Ipv6, Protocol::Udp);
    assert_eq!(udp6.rows[1].socket.local.address, "fe80::de8f:8d84:9d56:701f");
    assert_eq!(tables::address_scope(&udp6.rows[1].socket.local.address), AddressScope::LinkLocal);
}

#[test]
fn proc_net_rejects_foreign_headers_and_counts_unreadable_lines_without_guessing() {
    assert!(tables::parse_proc_net(&b""[..], Family::Ipv4, Protocol::Tcp).is_err());
    assert!(tables::parse_proc_net(&b"not a table\n   0: 0100007F:22B8 00000000:0000 0A 0 0 0 0 0 0 1\n"[..], Family::Ipv4, Protocol::Tcp).is_err());
    let header = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
    let good = "   0: 0100007F:22B8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 5 2\n";
    for bad in [
        "   1: 0100007F:22B8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0\n", // missing inode
        "   1: 0100007F:22B8 00000000:0000 0A 0 0 0 0 0 abc\n",                                        // inode not a number
        "   1: 0100007:22B8 00000000:0000 0A 0 0 0 0 0 0 1\n",                                         // 7-digit address
        "   1: 0100007F:22B 00000000:0000 0A 0 0 0 0 0 0 1\n",                                         // 3-digit port
        "   1: 0100007F:22B8 00000000:0000 0 0 0 0 0 0 0 1\n",                                         // 1-digit state
        "   1: 0100007G:22B8 00000000:0000 0A 0 0 0 0 0 0 1\n",                                         // not hex
        "   1: 0100007F22B8 00000000:0000 0A 0 0 0 0 0 0 1\n",                                         // no colon
        "   1: ünïcödé:22B8 00000000:0000 0A 0 0 0 0 0 0 1\n",
    ] {
        let parsed = proc_rows(&format!("{header}{good}{bad}{good}"), Family::Ipv4, Protocol::Tcp);
        assert_eq!((parsed.rows.len(), parsed.malformed), (2, 1), "{bad}");
    }
    // Blank lines are not rows and not damage; a missing final newline still yields the last row.
    let parsed = proc_rows(&format!("{header}{good}\n\n{}", good.trim_end()), Family::Ipv4, Protocol::Tcp);
    assert_eq!((parsed.rows.len(), parsed.malformed), (2, 0));
    // A line longer than any socket row is skipped whole; the next row is still read.
    let parsed = proc_rows(&format!("{header}{}\n{good}", "9".repeat(5_000)), Family::Ipv4, Protocol::Tcp);
    assert_eq!((parsed.rows.len(), parsed.malformed), (1, 1));
    // The same long line at the very end of the input must not loop or panic.
    let parsed = proc_rows(&format!("{header}{good}{}", "9".repeat(5_000)), Family::Ipv4, Protocol::Tcp);
    assert_eq!((parsed.rows.len(), parsed.malformed), (1, 1));
}

#[test]
fn proc_net_tables_are_capped_and_say_so() {
    let header = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
    let row = "   0: 0100007F:22B8 00000000:0000 06 00000000:00000000 00:00000000 00000000  1000        0 0 2\n";
    let text = format!("{header}{}", row.repeat(tables::PROC_ROW_LIMIT + 7));
    let parsed = proc_rows(&text, Family::Ipv4, Protocol::Tcp);
    assert_eq!((parsed.rows.len(), parsed.truncated), (tables::PROC_ROW_LIMIT, true));
    let exact = proc_rows(&format!("{header}{}", row.repeat(tables::PROC_ROW_LIMIT)), Family::Ipv4, Protocol::Tcp);
    assert_eq!((exact.rows.len(), exact.truncated), (tables::PROC_ROW_LIMIT, false), "exactly the limit is not truncation");
}

#[test]
fn linux_tcp_states_map_onto_the_shared_direction_rules() {
    let direction = |state: u8| tables::direction_of(tables::tcp_state_from_linux(state));
    assert_eq!(direction(0x0a), (Direction::Listener, DirectionEvidence::Listen));
    assert_eq!(direction(0x02), (Direction::Outbound, DirectionEvidence::SynSent));
    assert_eq!(direction(0x03), (Direction::Inbound, DirectionEvidence::SynReceived));
    assert_eq!(direction(0x0c), (Direction::Inbound, DirectionEvidence::SynReceived), "a request socket is still in the handshake");
    for established_or_closing in [0x01, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0b] {
        assert_eq!(direction(established_or_closing), (Direction::Unknown, DirectionEvidence::Unavailable), "{established_or_closing:#x}");
    }
    let names: Vec<String> = (1..=0x0b).map(|state| tables::tcp_state_name(tables::tcp_state_from_linux(state))).collect();
    assert_eq!(names, ["ESTABLISHED", "SYN_SENT", "SYN_RCVD", "FIN_WAIT1", "FIN_WAIT2", "TIME_WAIT", "CLOSED", "CLOSE_WAIT", "LAST_ACK", "LISTEN", "CLOSING"]);
    assert_eq!(tables::tcp_state_name(tables::tcp_state_from_linux(0x7f)), "UNKNOWN(0)");
}

#[test]
fn proc_stat_names_may_contain_spaces_and_parentheses_and_the_fields_are_positional() {
    let stat = tables::parse_proc_stat("1727 (omp) R 1691 1727 1727 34816 1727 4194304 10544891 183077658 163541 40082 129617 9238 1193824 284183 20 0 151 0 1869 84651192320 140084 18446744073709551615 25435648").unwrap();
    assert_eq!(stat, tables::ProcStat { name: "omp".into(), state: 'R', parent_pid: 1691, threads: 151, start_ticks: 1869 });
    let tricky = tables::parse_proc_stat("42 (a) b (c)) S 7 42 42 0 -1 4194560 1 1 1 1 1 1 1 1 20 0 3 0 99 1 1").unwrap();
    assert_eq!((tricky.name.as_str(), tricky.state, tricky.parent_pid, tricky.threads, tricky.start_ticks), ("a) b (c)", 'S', 7, 3, 99));
    assert_eq!(tables::parse_proc_stat("42 (zombie) Z 7 42 42 0 -1 0 1 1 1 1 1 1 1 1 20 0 1 0 5 0").unwrap().state, 'Z');
    for broken in ["", "42", "42 (x", "42 )x(", "42 (x) S", "42 (x) S 7 42 42 0 -1 4194560 1 1 1 1 1 1 1 1 20 0 3 0", "42 (x) S notapid 42 42 0 -1 4194560 1 1 1 1 1 1 1 1 20 0 3 0 99"] {
        assert_eq!(tables::parse_proc_stat(broken), None, "{broken:?}");
    }
    assert_eq!(tables::ticks_to_100ns(1869, 100), 186_900_000);
    assert_eq!(tables::ticks_to_100ns(u64::MAX, 1), u64::MAX, "no overflow");
    assert_eq!(tables::ticks_to_100ns(5, 0), 50_000_000, "a bogus zero clock rate cannot divide by zero");
}

#[test]
fn linux_rows_flow_through_the_shared_snapshot_rules() {
    // A decoded private-LAN peer is external; loopback and wildcard are not; TIME_WAIT (no owner) has no pid.
    let mut sockets: Vec<RawSocket> = proc_rows(PROC_TCP4, Family::Ipv4, Protocol::Tcp).rows.into_iter().map(|row| row.socket).collect();
    sockets[2].pid = 200;
    sockets[3].pid = 200;
    let raw = collection(
        vec![process(200, 1, "curl", Some(5))],
        sockets,
        vec![],
        &[(200, Recheck::Creation(5))],
    );
    let snapshot = build(raw, &[]);
    let external = all_tcp(&snapshot, TcpView::External);
    let peers: Vec<&str> = external.iter().map(|row| row["remote"]["address"].as_str().unwrap()).collect();
    assert_eq!(peers.len(), 3, "{peers:?}");
    assert!(peers.contains(&"192.168.1.1") && peers.contains(&"3.174.18.29"), "{peers:?}");
    let outbound = all_tcp(&snapshot, TcpView::ConfirmedOutbound);
    assert_eq!(outbound.len(), 1);
    assert_eq!((outbound[0]["pid"].clone(), outbound[0]["processName"].clone()), (json!(200), json!("curl")));
    let waiting = all_tcp(&snapshot, TcpView::All).into_iter().find(|row| row["state"] == "TIME_WAIT").unwrap();
    assert_eq!((waiting["pid"].clone(), waiting["processKey"].clone()), (Value::Null, Value::Null));
}

// ---------------------------------------------------------------- real Linux native pass

/// The Linux twin of the Windows smoke test, against the live `/proc`. Read-only; it creates loopback endpoints and
/// one scratch `sleep` child of its own.
#[cfg(target_os = "linux")]
#[test]
fn native_snapshot_smoke() {
    use std::net::{TcpListener, TcpStream, UdpSocket};
    use std::process::{Command, Stdio};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen_port = listener.local_addr().unwrap().port();
    let client = TcpStream::connect(("127.0.0.1", listen_port)).unwrap();
    let client_port = client.local_addr().unwrap().port();
    let (_server, _) = listener.accept().unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    let udp_port = udp.local_addr().unwrap().port();
    let listener6 = TcpListener::bind("[::1]:0").ok();
    let client6 = listener6.as_ref().and_then(|l| TcpStream::connect(l.local_addr().unwrap()).ok());
    let mut child = Command::new("sleep").arg("30").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();

    let own = capture_identity(std::process::id()).expect("own process identity");
    let child_identity = capture_identity(child.id()).expect("scratch child identity");
    assert_ne!(own.creation_time_100ns, 0);
    assert!(child_identity.creation_time_100ns >= own.creation_time_100ns, "the child started after its parent");
    let roots = [ManagedRoot { instance_id: "smoke".into(), generation: 1, kind: RootKind::Agent, identity: own }];
    let cancel = AtomicBool::new(false);
    let raw = super::native::collect(&cancel).expect("native collection");
    let snapshot = HostSnapshot::build("s".into(), "v".into(), "smoke".into(), 1, raw, &roots);
    let summary = snapshot.summary();
    assert_eq!(summary.sections.processes.status, SectionStatus::Ok);
    assert_eq!(summary.sections.tcp4.status, SectionStatus::Ok);
    assert_eq!(summary.sections.udp4.status, SectionStatus::Ok);
    assert!(summary.process_count.unwrap() > 2);
    assert!(summary.duration_ms < 30_000);

    let process_rows = snapshot.query(&HostMonitorQuery::Processes { text: String::new(), offset: 0, limit: 250 });
    let mut every_process = process_rows.items;
    let mut offset = process_rows.next_offset;
    while let Some(next) = offset {
        let more = snapshot.query(&HostMonitorQuery::Processes { text: String::new(), offset: next, limit: 250 });
        every_process.extend(more.items);
        offset = more.next_offset;
    }
    let find = |pid: u32| every_process.iter().find(|row| row["pid"] == pid).cloned().unwrap_or_else(|| panic!("pid {pid} listed"));
    let me = find(std::process::id());
    assert_eq!(me["key"], format!("{}:{}", own.pid, own.creation_time_100ns).as_str());
    assert_eq!(me["owner"]["kind"], "agentRoot");
    assert_eq!(me["availability"], "available");
    let scratch = find(child.id());
    assert_eq!(scratch["creationTime100ns"], child_identity.creation_time_100ns.to_string().as_str());
    assert_eq!(scratch["name"], "sleep");
    assert_eq!(scratch["owner"]["kind"], "descendant", "the scratch child is attributed through verified ancestry");
    assert_eq!(scratch["parentPid"], std::process::id());
    assert!(me["threadCount"].as_u64().unwrap() >= 1);

    let tcp = snapshot.query(&HostMonitorQuery::Tcp { view: TcpView::All, text: format!("{}", std::process::id()), offset: 0, limit: 250 }).items;
    let on = |port: u16| -> Vec<&Value> { tcp.iter().filter(|row| row["local"]["port"] == port).collect() };
    let listening = on(listen_port).into_iter().find(|row| row["state"] == "LISTEN").expect("listener row");
    assert_eq!((listening["direction"].clone(), listening["pid"].clone(), listening["remote"].clone()), (json!("listener"), json!(std::process::id()), Value::Null));
    assert_eq!(listening["processKey"], me["key"], "attributed through the socket inode and creation identity");
    let established = on(client_port).into_iter().find(|row| row["state"] == "ESTABLISHED").expect("client row");
    assert_eq!(established["remote"]["port"], listen_port, "literal ports, not byte-swapped");
    assert_eq!(established["remote"]["address"], "127.0.0.1", "literal addresses, not byte-swapped");
    assert_eq!((established["direction"].clone(), established["directionEvidence"].clone()), (json!("unknown"), json!("unavailable")));
    assert!(on(listen_port).iter().any(|row| row["state"] == "ESTABLISHED" && row["remote"]["port"] == client_port), "the accepted side is listed too");

    let udp_rows = snapshot.query(&HostMonitorQuery::Udp { text: udp_port.to_string(), offset: 0, limit: 50 }).items;
    let udp_row = udp_rows.iter().find(|row| row["local"]["port"] == udp_port).expect("udp row");
    assert_eq!((udp_row["pid"].clone(), udp_row["remote"].clone(), udp_row["direction"].clone()), (json!(std::process::id()), Value::Null, json!("unknown")));

    if let (Some(listener6), Some(_client6)) = (&listener6, &client6) {
        assert_eq!(summary.sections.tcp6.status, SectionStatus::Ok);
        let port6 = listener6.local_addr().unwrap().port();
        let rows6 = snapshot.query(&HostMonitorQuery::Tcp { view: TcpView::Listeners, text: port6.to_string(), offset: 0, limit: 50 }).items;
        let row6 = rows6.iter().find(|row| row["local"]["port"] == port6).expect("IPv6 listener");
        assert_eq!((row6["family"].clone(), row6["local"]["address"].clone(), row6["addressScope"].clone()), (json!("ipv6"), json!("::1"), json!("loopback")));
        assert_eq!(row6["pid"], std::process::id());
    }

    // Cancellation before the pass starts is honoured without reading anything.
    let stopped = AtomicBool::new(true);
    assert_eq!(super::native::collect(&stopped).err().as_deref(), Some("monitor_cancelled"));

    child.kill().ok();
    child.wait().ok();
    drop(listener6);
}

#[cfg(target_os = "linux")]
#[test]
fn a_zombie_is_reported_as_exited_and_has_no_capturable_identity() {
    use std::process::{Command, Stdio};
    let mut child = Command::new("true").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let pid = child.id();
    // The child is not waited for, so once it has run it stays a zombie.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| stat.contains(") Z ")) {
        assert!(std::time::Instant::now() < deadline, "the child never became a zombie");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(capture_identity(pid), None);
    let raw = super::native::collect(&AtomicBool::new(false)).unwrap();
    let row = raw.processes.iter().find(|process| process.pid == pid).expect("zombie is still listed");
    assert_eq!((row.availability, row.parent_pid), (Availability::Exited, std::process::id()));
    child.wait().unwrap();
    assert_eq!(capture_identity(pid), None, "a reaped pid is gone");
    assert_eq!(capture_identity(0), None);
    assert_eq!(capture_identity(u32::MAX), None, "a pid beyond pid_max never existed");
}
