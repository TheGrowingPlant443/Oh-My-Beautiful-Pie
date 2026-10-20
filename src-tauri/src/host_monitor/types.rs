//! Wire and snapshot types of the whole-computer monitor. Process rows carry no CPU/memory, arguments,
//! environment, full paths, user names or script previews: only what the Windows process snapshot and socket tables
//! expose by themselves.
use serde::{Deserialize, Serialize};

pub const DEFAULT_PAGE: usize = 100;
pub const MAX_PAGE: usize = 250;
pub const MAX_PAGE_BYTES: usize = 1024 * 1024;
pub const MAX_SEARCH_CHARS: usize = 256;

/// PID plus creation time (FILETIME ticks of 100 ns). A PID alone never identifies a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub creation_time_100ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootKind {
    Agent,
    Browser,
}

/// A launched process this app owns, used only to highlight ownership in whole-computer rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedRoot {
    pub instance_id: String,
    pub generation: u64,
    pub kind: RootKind,
    pub identity: ProcessIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SectionStatus {
    Ok,
    Partial,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SectionState {
    pub status: SectionStatus,
    pub error: Option<String>,
}

impl SectionState {
    pub fn ok() -> Self {
        Self { status: SectionStatus::Ok, error: None }
    }
    pub fn partial(error: impl Into<String>) -> Self {
        Self { status: SectionStatus::Partial, error: Some(error.into()) }
    }
    pub fn unavailable(error: impl Into<String>) -> Self {
        Self { status: SectionStatus::Unavailable, error: Some(error.into()) }
    }
    pub fn is_unavailable(&self) -> bool {
        self.status == SectionStatus::Unavailable
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sections {
    pub processes: SectionState,
    pub tcp4: SectionState,
    pub tcp6: SectionState,
    pub udp4: SectionState,
    pub udp6: SectionState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Availability {
    Available,
    Limited,
    Exited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OwnerKind {
    AgentRoot,
    Descendant,
    BrowserRoot,
    BrowserDescendant,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Owner {
    pub instance_id: String,
    pub generation: u64,
    pub kind: OwnerKind,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessRow {
    pub key: String,
    pub pid: u32,
    /// FILETIME ticks as a decimal string: the value exceeds a JavaScript safe integer.
    pub creation_time_100ns: Option<String>,
    pub parent_pid: Option<u32>,
    pub name: String,
    pub thread_count: u32,
    pub availability: Availability,
    pub owner: Option<Owner>,
    #[serde(skip)]
    pub search: String,
    #[serde(skip)]
    pub creation: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Family {
    Ipv4,
    Ipv6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Protocol {
    Tcp,
    Udp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    pub address: String,
    pub port: u16,
    pub scope_id: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    Outbound,
    Inbound,
    Listener,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DirectionEvidence {
    SynSent,
    SynReceived,
    Listen,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AddressScope {
    Loopback,
    Private,
    Public,
    LinkLocal,
    Unspecified,
    Other,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketRow {
    pub key: String,
    pub family: Family,
    pub protocol: Protocol,
    pub pid: Option<u32>,
    pub process_key: Option<String>,
    pub process_name: Option<String>,
    pub local: Endpoint,
    pub remote: Option<Endpoint>,
    pub state: Option<String>,
    pub direction: Direction,
    pub direction_evidence: DirectionEvidence,
    pub address_scope: AddressScope,
    /// Remote peer is neither a wildcard nor loopback (private LAN peers included). Not "proven outbound".
    #[serde(skip)]
    pub external: bool,
    #[serde(skip)]
    pub search: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TcpView {
    External,
    ConfirmedOutbound,
    UnknownDirection,
    Listeners,
    All,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum HostMonitorQuery {
    Processes {
        #[serde(default)]
        text: String,
        offset: usize,
        limit: usize,
    },
    Tcp {
        view: TcpView,
        #[serde(default)]
        text: String,
        offset: usize,
        limit: usize,
    },
    Udp {
        #[serde(default)]
        text: String,
        offset: usize,
        limit: usize,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostMonitorPage {
    pub snapshot_id: String,
    pub section: &'static str,
    pub total: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub items: Vec<serde_json::Value>,
}

/// `null` counts are unavailable; a partial section makes its count a lower bound (see `sections`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSnapshotSummary {
    pub view_id: String,
    pub snapshot_id: String,
    pub instance_id: String,
    pub generation: u64,
    pub scope: &'static str,
    pub started_at: u64,
    pub completed_at: u64,
    pub duration_ms: u64,
    pub process_count: Option<usize>,
    pub external_tcp_count: Option<usize>,
    pub confirmed_outbound_count: Option<usize>,
    pub unknown_direction_count: Option<usize>,
    pub udp_endpoint_count: Option<usize>,
    pub limited_process_count: Option<usize>,
    pub sections: Sections,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostMonitorContext {
    pub instance_id: String,
    pub generation: u64,
    pub project_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostMonitorOpen {
    pub view_id: String,
    pub context: HostMonitorContext,
}
