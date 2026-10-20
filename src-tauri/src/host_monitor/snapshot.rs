//! Frozen whole-computer snapshots: joining sequentially enumerated processes and sockets conservatively, ownership
//! attribution by creation identity and verified ancestry, deterministic ordering, hashing, and filter-then-page reads.
use std::collections::HashMap;

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::tables::{self, RawSocket};
use super::types::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawProcess {
    pub pid: u32,
    pub parent_pid: u32,
    pub threads: u32,
    pub name: String,
    /// FILETIME creation time; `None` when the process could not be opened for a limited query.
    pub creation: Option<u64>,
    pub availability: Availability,
}

/// Creation time of a PID read again after the socket tables were enumerated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recheck {
    Creation(u64),
    Gone,
    Unreadable,
}

/// What one native pass observed; everything is interpreted by `HostSnapshot::build`.
pub struct RawCollection {
    pub started_at: u64,
    pub completed_at: u64,
    pub duration_ms: u64,
    pub processes: Vec<RawProcess>,
    pub tcp: Vec<RawSocket>,
    pub udp: Vec<RawSocket>,
    pub rechecks: HashMap<u32, Recheck>,
    pub sections: Sections,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    processes: Option<usize>,
    limited: Option<usize>,
    external_tcp: Option<usize>,
    confirmed_outbound: Option<usize>,
    unknown_direction: Option<usize>,
    udp: Option<usize>,
}

pub struct HostSnapshot {
    pub id: String,
    pub view_id: String,
    pub instance_id: String,
    pub generation: u64,
    pub started_at: u64,
    pub completed_at: u64,
    pub duration_ms: u64,
    pub sections: Sections,
    /// SHA-256 over the canonical content (rows, sections and timing), never over random identifiers.
    pub hash: String,
    processes: Vec<ProcessRow>,
    tcp: Vec<SocketRow>,
    udp: Vec<SocketRow>,
    counts: Counts,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Canonical<'a> {
    scope: &'static str,
    started_at: u64,
    completed_at: u64,
    sections: &'a Sections,
    processes: &'a [ProcessRow],
    tcp: &'a [SocketRow],
    udp: &'a [SocketRow],
}

fn process_key(pid: u32, creation: Option<u64>, ordinal: usize) -> String {
    match creation {
        Some(creation) => format!("{pid}:{creation}"),
        // Idle and System have a fixed, system-wide identity even though their start time is not readable.
        None if pid == 0 || pid == 4 => format!("{pid}:boot"),
        None => format!("{pid}:local-{ordinal}"),
    }
}

fn clean(name: &str) -> String {
    name.chars().map(|c| if c.is_control() { '\u{fffd}' } else { c }).collect()
}

fn process_rows(raw: &[RawProcess]) -> Vec<ProcessRow> {
    raw.iter()
        .enumerate()
        .map(|(ordinal, process)| {
            let name = clean(&process.name);
            ProcessRow {
                key: process_key(process.pid, process.creation, ordinal),
                pid: process.pid,
                creation_time_100ns: process.creation.map(|c| c.to_string()),
                parent_pid: Some(process.parent_pid).filter(|parent| *parent != 0),
                search: format!("{} {}", name.to_lowercase(), process.pid),
                name,
                thread_count: process.threads,
                availability: process.availability,
                owner: None,
                creation: process.creation,
            }
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Memo {
    Pending,
    Visiting,
    Done(Option<(usize, bool)>),
}

/// Attribute processes to launched roots. A row matches a root only by PID *and* creation time; a descendant needs
/// a parent row that was created strictly earlier (so a recycled parent PID cannot adopt a stranger's child).
/// A child whose parent has already exited stays unattributed: no identity means no claim.
fn attribute(rows: &mut [ProcessRow], roots: &[ManagedRoot]) {
    if roots.is_empty() {
        return;
    }
    let by_pid: HashMap<u32, usize> = rows.iter().enumerate().map(|(index, row)| (row.pid, index)).collect();
    let mut memo = vec![Memo::Pending; rows.len()];
    fn resolve(
        index: usize,
        rows: &[ProcessRow],
        roots: &[ManagedRoot],
        by_pid: &HashMap<u32, usize>,
        memo: &mut [Memo],
        depth: usize,
    ) -> Option<(usize, bool)> {
        match memo[index] {
            Memo::Done(found) => return found,
            Memo::Visiting => return None,
            Memo::Pending => {}
        }
        memo[index] = Memo::Visiting;
        let row = &rows[index];
        let found = (|| {
            let creation = row.creation?;
            if let Some(root) = roots.iter().position(|r| r.identity.pid == row.pid && r.identity.creation_time_100ns == creation) {
                return Some((root, false));
            }
            if depth >= 64 {
                return None;
            }
            let parent = *by_pid.get(&row.parent_pid?)?;
            (rows[parent].creation? < creation).then_some(())?;
            resolve(parent, rows, roots, by_pid, memo, depth + 1).map(|(root, _)| (root, true))
        })();
        memo[index] = Memo::Done(found);
        found
    }
    let found: Vec<Option<(usize, bool)>> = (0..rows.len()).map(|i| resolve(i, rows, roots, &by_pid, &mut memo, 0)).collect();
    for (row, found) in rows.iter_mut().zip(found) {
        row.owner = found.map(|(root, descendant)| {
            let root = &roots[root];
            Owner {
                instance_id: root.instance_id.clone(),
                generation: root.generation,
                kind: match (root.kind, descendant) {
                    (RootKind::Agent, false) => OwnerKind::AgentRoot,
                    (RootKind::Agent, true) => OwnerKind::Descendant,
                    (RootKind::Browser, false) => OwnerKind::BrowserRoot,
                    (RootKind::Browser, true) => OwnerKind::BrowserDescendant,
                },
            }
        });
    }
}

fn endpoint_text(endpoint: &Endpoint) -> String {
    format!("{}:{}", endpoint.address, endpoint.port)
}

fn socket_rows(
    raw: &[RawSocket],
    processes: &[ProcessRow],
    rechecks: &HashMap<u32, Recheck>,
) -> Vec<SocketRow> {
    let by_pid: HashMap<u32, &ProcessRow> = processes.iter().map(|row| (row.pid, row)).collect();
    let mut seen: HashMap<String, usize> = HashMap::new();
    raw.iter()
        .map(|socket| {
            // A socket owner is named only if the same creation time was seen before and after the table read.
            let owner = by_pid.get(&socket.pid).copied().filter(|row| {
                socket.pid != 0
                    && match row.creation {
                        Some(creation) => rechecks.get(&socket.pid) == Some(&Recheck::Creation(creation)),
                        None => socket.pid == 4,
                    }
            });
            let (direction, direction_evidence) = match socket.tcp_state {
                Some(state) => tables::direction_of(state),
                None => (Direction::Unknown, DirectionEvidence::Unavailable),
            };
            let scope_source = socket.remote.as_ref().unwrap_or(&socket.local);
            let base = format!(
                "{}:{}:{}:{}",
                if socket.protocol == Protocol::Tcp { "tcp" } else { "udp" },
                endpoint_text(&socket.local),
                socket.remote.as_ref().map(endpoint_text).unwrap_or_default(),
                socket.pid
            );
            let duplicates = seen.entry(base.clone()).or_insert(0);
            *duplicates += 1;
            let key = if *duplicates == 1 { base } else { format!("{base}~{duplicates}") };
            let process_name = owner.map(|row| row.name.clone());
            let state = socket.tcp_state.map(tables::tcp_state_name);
            let search = format!(
                "{} {} {} {} {} {}",
                process_name.as_deref().unwrap_or("").to_lowercase(),
                socket.pid,
                endpoint_text(&socket.local),
                socket.remote.as_ref().map(endpoint_text).unwrap_or_default(),
                socket.local.address,
                socket.remote.as_ref().map(|peer| peer.address.as_str()).unwrap_or("")
            )
            .to_lowercase();
            SocketRow {
                key,
                family: socket.family,
                protocol: socket.protocol,
                pid: Some(socket.pid).filter(|pid| *pid != 0),
                process_key: owner.map(|row| row.key.clone()),
                process_name,
                address_scope: tables::address_scope(&scope_source.address),
                external: tables::is_external(socket.remote.as_ref()),
                local: socket.local.clone(),
                remote: socket.remote.clone(),
                state,
                direction,
                direction_evidence,
                search,
            }
        })
        .collect()
}

fn sort_sockets(rows: &mut [SocketRow]) {
    rows.sort_by_cached_key(|row| {
        (
            row.process_name.as_deref().unwrap_or("").to_lowercase(),
            row.pid,
            row.process_key.clone(),
            row.remote.as_ref().map(|peer| (peer.address.clone(), peer.port)),
            (row.local.address.clone(), row.local.port),
            row.state.clone(),
            row.key.clone(),
        )
    });
}

impl HostSnapshot {
    pub fn build(
        id: String,
        view_id: String,
        instance_id: String,
        generation: u64,
        raw: RawCollection,
        roots: &[ManagedRoot],
    ) -> Self {
        let mut processes = process_rows(&raw.processes);
        attribute(&mut processes, roots);
        let mut tcp = socket_rows(&raw.tcp, &processes, &raw.rechecks);
        let mut udp = socket_rows(&raw.udp, &processes, &raw.rechecks);
        processes.sort_by_cached_key(|row| (row.name.to_lowercase(), row.pid, row.creation, row.key.clone()));
        sort_sockets(&mut tcp);
        sort_sockets(&mut udp);
        let sections = raw.sections;
        let available = |a: &SectionState, b: &SectionState| !(a.is_unavailable() && b.is_unavailable());
        let tcp_known = available(&sections.tcp4, &sections.tcp6);
        let counts = Counts {
            processes: (!sections.processes.is_unavailable()).then_some(processes.len()),
            limited: (!sections.processes.is_unavailable())
                .then(|| processes.iter().filter(|row| row.availability == Availability::Limited).count()),
            external_tcp: tcp_known.then(|| tcp.iter().filter(|row| row.external).count()),
            confirmed_outbound: tcp_known
                .then(|| tcp.iter().filter(|row| row.external && row.direction == Direction::Outbound).count()),
            unknown_direction: tcp_known
                .then(|| tcp.iter().filter(|row| row.external && row.direction == Direction::Unknown).count()),
            udp: available(&sections.udp4, &sections.udp6).then_some(udp.len()),
        };
        let canonical = Canonical {
            scope: "wholeComputer",
            started_at: raw.started_at,
            completed_at: raw.completed_at,
            sections: &sections,
            processes: &processes,
            tcp: &tcp,
            udp: &udp,
        };
        let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
        let hash = Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
        Self {
            id,
            view_id,
            instance_id,
            generation,
            started_at: raw.started_at,
            completed_at: raw.completed_at,
            duration_ms: raw.duration_ms,
            sections,
            hash,
            processes,
            tcp,
            udp,
            counts,
        }
    }

    pub fn summary(&self) -> HostSnapshotSummary {
        HostSnapshotSummary {
            view_id: self.view_id.clone(),
            snapshot_id: self.id.clone(),
            instance_id: self.instance_id.clone(),
            generation: self.generation,
            scope: "wholeComputer",
            started_at: self.started_at,
            completed_at: self.completed_at,
            duration_ms: self.duration_ms,
            process_count: self.counts.processes,
            external_tcp_count: self.counts.external_tcp,
            confirmed_outbound_count: self.counts.confirmed_outbound,
            unknown_direction_count: self.counts.unknown_direction,
            udp_endpoint_count: self.counts.udp,
            limited_process_count: self.counts.limited,
            sections: self.sections.clone(),
        }
    }

    /// Filter the whole section, then page. Never samples.
    pub fn query(&self, query: &HostMonitorQuery) -> HostMonitorPage {
        match query {
            HostMonitorQuery::Processes { text, offset, limit } => {
                let needle = needle(text);
                self.page("processes", &self.processes, |row| row.search.contains(&needle), *offset, *limit)
            }
            HostMonitorQuery::Tcp { view, text, offset, limit } => {
                let needle = needle(text);
                let view = *view;
                self.page(
                    "tcp",
                    &self.tcp,
                    |row| {
                        let shown = match view {
                            TcpView::External => row.external,
                            TcpView::ConfirmedOutbound => row.external && row.direction == Direction::Outbound,
                            TcpView::UnknownDirection => row.external && row.direction == Direction::Unknown,
                            TcpView::Listeners => row.direction == Direction::Listener,
                            TcpView::All => true,
                        };
                        shown && row.search.contains(&needle)
                    },
                    *offset,
                    *limit,
                )
            }
            HostMonitorQuery::Udp { text, offset, limit } => {
                let needle = needle(text);
                self.page("udp", &self.udp, |row| row.search.contains(&needle), *offset, *limit)
            }
        }
    }

    fn page<T: Serialize>(
        &self,
        section: &'static str,
        rows: &[T],
        keep: impl Fn(&T) -> bool,
        offset: usize,
        limit: usize,
    ) -> HostMonitorPage {
        let matches: Vec<usize> = rows.iter().enumerate().filter(|(_, row)| keep(row)).map(|(index, _)| index).collect();
        let total = matches.len();
        let limit = if limit == 0 { DEFAULT_PAGE } else { limit.min(MAX_PAGE) };
        let start = if total == 0 {
            0
        } else if offset >= total {
            (total - 1) / limit * limit
        } else {
            offset
        };
        let mut items = Vec::new();
        let mut bytes = 0usize;
        for &index in matches[start..].iter().take(limit) {
            let value = serde_json::to_value(&rows[index]).unwrap_or(serde_json::Value::Null);
            let size = value.to_string().len();
            if !items.is_empty() && bytes + size > MAX_PAGE_BYTES {
                break;
            }
            bytes += size;
            items.push(value);
        }
        let end = start + items.len();
        HostMonitorPage {
            snapshot_id: self.id.clone(),
            section,
            total,
            offset: start,
            next_offset: (end < total).then_some(end),
            items,
        }
    }
}

fn needle(text: &str) -> String {
    text.trim().chars().take(MAX_SEARCH_CHARS).collect::<String>().to_lowercase()
}
