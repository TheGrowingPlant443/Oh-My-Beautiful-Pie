//! Serializable contracts of the knowledge backend. Field names mirror `src/lib/knowledge.ts` exactly (camelCase);
//! fields marked "extra" are additive and ignored by consumers that do not know them.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VaultMode {
    Obsidian,
    Markdown,
}

/// Where a vault applies. Only the backend derives `repository_root` (from `setup::find_repository_root`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum KnowledgeScope {
    Global,
    Project { repository_root: String },
}

/// The scope a human asks for. A project's repository root is derived from `folder`; one that is sent must agree.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ScopeRequest {
    Global,
    Project {
        #[serde(default)]
        repository_root: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VaultBinding {
    pub vault_id: String,
    pub root: String,
    pub mode: VaultMode,
    pub scope: KnowledgeScope,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ObsidianState {
    Absent,
    Unreadable,
    Installed,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObsidianCapability {
    pub state: ObsidianState,
    pub version: Option<String>,
    pub executable: Option<String>,
    pub registered_vaults: Vec<String>,
    pub message: Option<String>,
    /// Extra: the human declined Obsidian for the shown project, so the SRS flow must not ask again.
    pub declined_for_project: bool,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum GraphifyState {
    Absent,
    Present,
    Malformed,
    Foreign,
    Unverified,
    Unavailable,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphifyCapability {
    pub present: bool,
    pub state: GraphifyState,
    pub source_root: Option<String>,
    pub artifact_id: Option<String>,
    pub artifact_hash: Option<String>,
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CogneeState {
    Installed,
    Absent,
    Unreadable,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CogneeCapability {
    pub state: CogneeState,
    /// `None` means inspection failed without evidence; `true` may be advisory when state is unreadable.
    pub installed: Option<bool>,
    pub version: Option<String>,
    pub ui_url: Option<String>,
    pub message: Option<String>,
    /// The inspected folder, including advisory evidence when it could not be inspected again.
    pub installation_folder: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeCapabilities {
    pub folder: Option<String>,
    pub obsidian: ObsidianCapability,
    pub graphify: GraphifyCapability,
    pub cognee: CogneeCapability,
    pub project_vault: Option<VaultBinding>,
    pub global_vault: Option<VaultBinding>,
    pub suggested_project_root: Option<String>,
    pub suggested_global_root: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultPreparation {
    pub token: String,
    pub scope: KnowledgeScope,
    pub mode: VaultMode,
    pub root: String,
    pub exists: bool,
    pub existing_markdown_count: u64,
    pub conflicts: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenNoteResult {
    pub opened: bool,
    pub uri: String,
    pub needs_vault_registration: bool,
    pub vault_path: String,
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VaultEntryKind {
    Folder,
    Markdown,
    Other,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultEntry {
    pub path: String,
    pub name: String,
    pub kind: VaultEntryKind,
    pub bytes: Option<u64>,
    pub hash: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultEntryPage {
    pub vault_id: String,
    pub revision: u64,
    pub prefix: String,
    pub total: u64,
    pub offset: u64,
    pub items: Vec<VaultEntry>,
    pub next_offset: Option<u64>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FenceState {
    pub marker: String,
    pub length: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultDocumentPage {
    pub vault_id: String,
    pub revision: u64,
    pub path: String,
    pub hash: String,
    pub byte_length: u64,
    pub byte_offset: u64,
    pub length: u64,
    pub next_offset: Option<u64>,
    pub text: String,
    /// Extra: the page starts inside a line (only when a single line exceeds a page).
    pub starts_mid_line: bool,
    /// Extra: the code fence that is open where this page starts, so a renderer can continue the parser state.
    pub fence: Option<FenceState>,
    /// Extra: the page starts inside YAML front matter.
    pub frontmatter: bool,
    /// Extra: 1-based number of the line this page starts in.
    pub line_number: u64,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum VaultLinkKind {
    Wiki,
    Markdown,
    Embed,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum VaultLinkStatus {
    Resolved,
    Missing,
    Ambiguous,
    NonDocument,
    External,
    Anchor,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultLinkRecord {
    pub id: String,
    pub source: String,
    pub target: Option<String>,
    pub kind: VaultLinkKind,
    pub status: VaultLinkStatus,
    pub raw: String,
    pub label: Option<String>,
    pub fragment: Option<String>,
    pub line: u64,
    pub candidates: Vec<String>,
    /// Extra: resolved to a note whose heading/block named by `fragment` does not exist.
    pub anchor_missing: bool,
    /// Extra: display-only fields were shortened; the indexed link identity is still `id`.
    pub raw_truncated: bool,
    pub label_truncated: bool,
    pub fragment_truncated: bool,
    /// Extra: the full candidate count before the transport prefix (at most 200) was selected.
    pub candidate_total: u64,
    pub candidates_truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultNoteRecord {
    pub path: String,
    pub title: String,
    pub bytes: u64,
    pub hash: String,
    pub outgoing: u64,
    pub incoming: u64,
    /// Extra: the displayed title is a UTF-8 prefix of the exact indexed heading.
    pub title_truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultSearchHit {
    pub path: String,
    pub title: String,
    pub line: Option<u64>,
    pub snippet: String,
    /// Extra: display limits do not change the exact text used for matching.
    pub title_truncated: bool,
    pub snippet_truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultResolution {
    pub raw: String,
    pub status: VaultLinkStatus,
    pub path: Option<String>,
    pub candidates: Vec<String>,
    /// Extra: candidate paths are exact identities; only the list length is bounded.
    pub candidate_total: u64,
    pub candidates_truncated: bool,
}

/// Strict query union (unknown kinds/fields are rejected).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum VaultLinkQuery {
    Notes { offset: usize, limit: usize },
    Links { offset: usize, limit: usize },
    Search { text: String, offset: usize, limit: usize },
    Backlinks { path: String, offset: usize, limit: usize },
    Neighbors { path: String, offset: usize, limit: usize },
    Resolve { from_path: String, targets: Vec<String> },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultQueryPage<T> {
    pub vault_id: String,
    pub revision: u64,
    pub total: u64,
    pub offset: u64,
    pub items: Vec<T>,
    pub next_offset: Option<u64>,
}

/// The answer to one [`VaultLinkQuery`]; serialized flat (the page is the answer).
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum VaultQueryAnswer {
    Notes(VaultQueryPage<VaultNoteRecord>),
    Links(VaultQueryPage<VaultLinkRecord>),
    Search(VaultQueryPage<VaultSearchHit>),
    Resolve(VaultQueryPage<VaultResolution>),
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultRefresh {
    pub vault_id: String,
    pub revision: u64,
    pub notes: u64,
    pub links: u64,
    pub unreadable: u64,
    /// Extra: SHA-256 over the sorted `(path, size, hash)` of every note — the identity of this revision.
    pub hash: String,
    /// Extra: true when this refresh published a new revision.
    pub changed: bool,
    /// Extra: first problems found (unreadable, non-UTF-8, over-long lines), never silently dropped.
    pub warnings: Vec<String>,
}

// ---------- Graphify ----------

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum GraphifySection {
    Metadata,
    Nodes,
    Links,
    Hyperedges,
    Communities,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum GraphifyIdKind {
    Text,
    Number,
}

/// Numeric and text ids are different identities even when they print alike.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphifyRef {
    pub id: String,
    pub id_kind: GraphifyIdKind,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GraphifyDirection {
    In,
    Out,
    Both,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum GraphifyQuery {
    Search { text: String, offset: usize, limit: usize },
    Neighbors { id: GraphifyRef, direction: GraphifyDirection, offset: usize, limit: usize },
    Node { id: GraphifyRef },
    CommunityMembers { community: i64, offset: usize, limit: usize },
    HyperedgesOf { id: GraphifyRef, offset: usize, limit: usize },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphifyPage {
    pub artifact_id: String,
    pub artifact_hash: String,
    pub section: GraphifySection,
    pub total: u64,
    pub offset: u64,
    pub items: Vec<Value>,
    pub next_offset: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphifyQueryPage {
    pub artifact_id: String,
    pub artifact_hash: String,
    pub kind: &'static str,
    pub total: u64,
    pub offset: u64,
    pub items: Vec<Value>,
    pub next_offset: Option<u64>,
}
