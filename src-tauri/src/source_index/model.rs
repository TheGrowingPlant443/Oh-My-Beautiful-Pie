//! Wire types of the repository source index, their validation and the in-memory lookup tables.
//! Everything here is plain data: no repository code is ever interpreted.
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use parking_lot::Mutex;

pub(crate) type Result<T, E = String> = std::result::Result<T, E>;

/// Largest serialized fact file the service accepts from a parser or loads from disk.
pub(crate) const MAX_OUTPUT_BYTES: u64 = 128 * 1024 * 1024;
/// Persisted indexes carry a few service-added fields on top of the parser output.
pub(crate) const MAX_SAVED_BYTES: u64 = MAX_OUTPUT_BYTES + 64 * 1024 * 1024;

fn text_limit(value: &str, label: &str, max: usize) -> Result<()> {
    if value.len() > max { Err(format!("Source index {label} exceeds {max} bytes")) } else { Ok(()) }
}

/// Length-delimited SHA-256 of the parts; identical to `common.stable_id` in the worker scripts.
pub(crate) fn stable_id(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

pub(crate) fn file_id(path: &str) -> String { stable_id(&["file", path]) }

// ---- statuses --------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceIndexState { Inventory, Parsing, Resolving, Complete, Partial, Failed, Cancelled }

impl SourceIndexState {
    pub fn is_active(self) -> bool { matches!(self, Self::Inventory | Self::Parsing | Self::Resolving) }
    /// States whose job directory holds a loadable `index.json`.
    pub fn has_index(self) -> bool { matches!(self, Self::Complete | Self::Partial) }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeCapability {
    /// Where the interpreter came from: `environment`, `graphify`, `path` or `none`.
    pub source: String,
    pub python: Option<String>,
    pub version: Option<String>,
    pub tree_sitter: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileCapability {
    pub language: String,
    pub version: Option<String>,
    pub available: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SourceIndexCapabilities {
    /// True when the runtime runs and at least one grammar profile is usable.
    pub available: bool,
    pub probed: bool,
    pub runtime: RuntimeCapability,
    pub profiles: Vec<ProfileCapability>,
    /// Actionable text when the parser cannot be used (names the `OMP_SYSTEM_OVERVIEW_PYTHON` override).
    pub hint: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SourceIndexStatus {
    pub job_id: String,
    pub snapshot_id: Option<String>,
    pub folder: String,
    pub state: SourceIndexState,
    pub indexed_at: Option<u64>,
    pub source_hash: Option<String>,
    pub completed_files: u64,
    pub total_files: u64,
    pub stale: Option<bool>,
    pub error: Option<String>,
    pub capabilities: SourceIndexCapabilities,
    pub coverage: Option<Value>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SourceIndexSelection {
    pub selected: Option<SourceIndexStatus>,
    pub latest_attempt: Option<SourceIndexStatus>,
}

// ---- facts -----------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus { Parsed, Partial, Failed, Skipped, Unsupported }

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum Resolution { ResolvedStatic, Candidate, Dynamic, External, Unresolved, Unsupported }

impl Resolution {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ResolvedStatic => "resolvedStatic", Self::Candidate => "candidate", Self::Dynamic => "dynamic",
            Self::External => "external", Self::Unresolved => "unresolved", Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SourceFile {
    pub id: String,
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub language: Option<String>,
    pub status: FileStatus,
    #[serde(default)]
    pub diagnostics: Vec<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Span {
    pub byte_start: u64,
    pub byte_end: u64,
    pub line_start: u64,
    pub line_end: u64,
    pub column_start: u64,
    pub column_end: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SourceSymbol {
    pub id: String,
    pub file_id: String,
    pub path: String,
    pub language: String,
    pub region: Option<String>,
    pub owner_id: Option<String>,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub span: Span,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bindings: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exported: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SourceSite {
    pub id: String,
    pub file_id: String,
    pub path: String,
    pub language: String,
    pub region: Option<String>,
    pub caller_id: Option<String>,
    pub span: Span,
    pub relation: String,
    pub raw: Option<String>,
    pub target_id: Option<String>,
    pub resolution: Resolution,
    #[serde(default)]
    pub conditional: bool,
    #[serde(default)]
    pub type_only: bool,
    #[serde(default)]
    pub candidate_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SourceEdge {
    pub id: String,
    pub source_id: String,
    pub target_id: String,
    pub relation: String,
    pub resolution: Resolution,
    pub evidence_ids: Vec<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Position of a node in the file/symbol arrays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Node { File(u32), Symbol(u32) }

/// Derived tables; rebuilt on every load and never serialized.
#[derive(Default)]
pub(crate) struct Lookup {
    pub files: HashMap<String, u32>,
    pub files_by_path: HashMap<String, u32>,
    pub symbols: HashMap<String, u32>,
    pub sites: HashMap<String, u32>,
    pub edges_out: HashMap<String, Vec<u32>>,
    pub edges_in: HashMap<String, Vec<u32>>,
    pub symbols_by_file: HashMap<String, Vec<u32>>,
    pub sites_by_caller: HashMap<String, Vec<u32>>,
    pub sites_by_file: HashMap<String, Vec<u32>>,
    pub overviews: Mutex<HashMap<(super::query::Level, super::query::GroupBy), Arc<super::query::Overview>>>,
}

impl Lookup {
    pub fn node(&self, id: &str) -> Option<Node> {
        self.files.get(id).map(|i| Node::File(*i)).or_else(|| self.symbols.get(id).map(|i| Node::Symbol(*i)))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceGraphIndex {
    pub schema_version: u32,
    pub snapshot_id: String,
    pub folder: String,
    pub source_hash: String,
    #[serde(default)]
    pub extractor_versions: Map<String, Value>,
    pub files: Vec<SourceFile>,
    #[serde(default)]
    pub directories: Vec<String>,
    pub symbols: Vec<SourceSymbol>,
    pub sites: Vec<SourceSite>,
    pub edges: Vec<SourceEdge>,
    pub coverage: Value,
    #[serde(default)]
    pub limitations: Vec<String>,
    #[serde(skip)]
    pub(crate) lookup: Lookup,
}

/// One admitted file as recorded by the inventory (and handed to the parser).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ManifestFile {
    pub id: String,
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub language: Option<String>,
    /// `pending` (parser-eligible) or `skipped` (rejected by the service, with diagnostics).
    pub status: String,
    pub diagnostics: Vec<String>,
}

/// What an index must agree with: the immutable inventory of its snapshot.
pub(crate) struct Expected<'a> {
    pub snapshot_id: &'a str,
    pub folder: &'a str,
    pub source_hash: &'a str,
    pub files: &'a [ManifestFile],
    pub directories: &'a [String],
}

impl SourceGraphIndex {
    pub fn file(&self, id: &str) -> Option<&SourceFile> { self.lookup.files.get(id).map(|i| &self.files[*i as usize]) }
    pub fn symbol(&self, id: &str) -> Option<&SourceSymbol> { self.lookup.symbols.get(id).map(|i| &self.symbols[*i as usize]) }
    pub fn site(&self, id: &str) -> Option<&SourceSite> { self.lookup.sites.get(id).map(|i| &self.sites[*i as usize]) }

    /// Inventory-only index for snapshots the parser could not (fully) process; every row says why.
    pub fn inventory_only(expected: &Expected, status: FileStatus, file_reason: &str, limitation: &str, limitations: Vec<String>) -> Result<Self> {
        let files = expected.files.iter().map(|f| {
            let skipped = f.status == "skipped";
            SourceFile {
                id: f.id.clone(), path: f.path.clone(), sha256: f.sha256.clone(), bytes: f.bytes, language: None,
                status: if skipped { FileStatus::Skipped } else { status },
                diagnostics: if skipped { f.diagnostics.clone() } else { vec![file_reason.to_string()] },
                extra: Map::new(),
            }
        }).collect();
        let mut index = SourceGraphIndex {
            schema_version: 1, snapshot_id: expected.snapshot_id.into(), folder: expected.folder.into(), source_hash: expected.source_hash.into(),
            extractor_versions: Map::new(), files, directories: expected.directories.to_vec(), symbols: Vec::new(), sites: Vec::new(), edges: Vec::new(),
            coverage: json!({ "languages": [], "lastFullyProcessedFile": null, "partial": true }),
            limitations, lookup: Lookup::default(),
        };
        index.limitations.push(limitation.to_string());
        index.finish(expected)?;
        Ok(index)
    }

    /// Validates every fact against the inventory and builds the lookup tables. After success the index is
    /// internally consistent: every reference resolves, spans lie inside their file and the counts are derived.
    pub fn finish(&mut self, expected: &Expected) -> Result<()> {
        self.check_header(expected)?;
        let mut lookup = Lookup::default();
        let mut ids: HashSet<&str> = HashSet::new();
        self.index_files(expected, &mut lookup, &mut ids)?;
        self.index_symbols(&mut lookup, &mut ids)?;
        self.check_containment(&lookup)?;
        self.index_sites(&mut lookup, &mut ids)?;
        self.check_site_targets(&lookup)?;
        self.index_edges(&mut lookup, &ids)?;
        for limitation in &self.limitations { text_limit(limitation, "limitation", 8 * 1024)?; }
        self.derive_coverage()?;
        self.lookup = lookup;
        Ok(())
    }

    fn check_header(&self, expected: &Expected) -> Result<()> {
        if self.schema_version != 1 { return Err(format!("Unsupported source index schema version {}", self.schema_version)); }
        if self.snapshot_id != expected.snapshot_id || self.folder != expected.folder || self.source_hash != expected.source_hash {
            return Err("The source index does not belong to this snapshot".into());
        }
        if self.files.len() != expected.files.len() { return Err(format!("The source index lists {} files but the snapshot admitted {}", self.files.len(), expected.files.len())); }
        if self.directories != expected.directories { return Err("The source index directory list differs from the snapshot inventory".into()); }
        Ok(())
    }

    fn index_files<'a>(&'a self, expected: &Expected, lookup: &mut Lookup, ids: &mut HashSet<&'a str>) -> Result<()> {
        for (position, (file, wanted)) in self.files.iter().zip(expected.files).enumerate() {
            check_file(file, wanted)?;
            ids.insert(&file.id);
            lookup.files.insert(file.id.clone(), position as u32);
            lookup.files_by_path.insert(file.path.clone(), position as u32);
        }
        Ok(())
    }

    fn index_symbols<'a>(&'a self, lookup: &mut Lookup, ids: &mut HashSet<&'a str>) -> Result<()> {
        for (position, symbol) in self.symbols.iter().enumerate() {
            if !ids.insert(&symbol.id) { return Err(format!("Duplicate source index id {}", symbol.id)); }
            let Some(file_position) = lookup.files.get(&symbol.file_id) else { return Err(format!("Symbol {} names an unknown file", symbol.id)); };
            check_symbol(symbol, &self.files[*file_position as usize])?;
            lookup.symbols.insert(symbol.id.clone(), position as u32);
            lookup.symbols_by_file.entry(symbol.file_id.clone()).or_default().push(position as u32);
        }
        Ok(())
    }

    /// Containment must be a forest: every owner exists, lives in the same file and never loops.
    fn check_containment(&self, lookup: &Lookup) -> Result<()> {
        let mut state = vec![0u8; self.symbols.len()];
        for start in 0..self.symbols.len() { self.walk_owners(start, &mut state, lookup)?; }
        Ok(())
    }

    fn walk_owners(&self, start: usize, state: &mut [u8], lookup: &Lookup) -> Result<()> {
        let mut chain = Vec::new();
        let mut current = start;
        loop {
            match state[current] {
                2 => break,
                1 => return Err(format!("Symbol containment cycle at {}", self.symbols[current].id)),
                _ => {}
            }
            state[current] = 1;
            chain.push(current);
            match self.owner_of(current, lookup)? {
                Some(next) => current = next,
                None => break,
            }
        }
        for item in chain { state[item] = 2; }
        Ok(())
    }

    fn owner_of(&self, current: usize, lookup: &Lookup) -> Result<Option<usize>> {
        let symbol = &self.symbols[current];
        let Some(owner) = &symbol.owner_id else { return Ok(None) };
        let Some(next) = lookup.symbols.get(owner) else { return Err(format!("Symbol {} names an unknown owner", symbol.id)); };
        if self.symbols[*next as usize].file_id != symbol.file_id { return Err(format!("Symbol {} is owned across files", symbol.id)); }
        Ok(Some(*next as usize))
    }

    fn index_sites<'a>(&'a self, lookup: &mut Lookup, ids: &mut HashSet<&'a str>) -> Result<()> {
        for (position, site) in self.sites.iter().enumerate() {
            if !ids.insert(&site.id) { return Err(format!("Duplicate source index id {}", site.id)); }
            let Some(file_position) = lookup.files.get(&site.file_id) else { return Err(format!("Site {} names an unknown file", site.id)); };
            check_site(site, &self.files[*file_position as usize])?;
            if let Some(caller) = &site.caller_id {
                let Some(symbol) = lookup.symbols.get(caller) else { return Err(format!("Site {} names an unknown caller", site.id)); };
                if self.symbols[*symbol as usize].file_id != site.file_id { return Err(format!("Site {} caller is in another file", site.id)); }
                lookup.sites_by_caller.entry(caller.clone()).or_default().push(position as u32);
            }
            lookup.sites_by_file.entry(site.file_id.clone()).or_default().push(position as u32);
            // Only a proven static target may be set; name matches and candidates never carry one.
            if (site.resolution == Resolution::ResolvedStatic) != site.target_id.is_some() {
                return Err(format!("Site {} must carry a target exactly when it is resolvedStatic", site.id));
            }
            lookup.sites.insert(site.id.clone(), position as u32);
        }
        Ok(())
    }

    fn check_site_targets(&self, lookup: &Lookup) -> Result<()> {
        for site in &self.sites {
            for target in site.target_id.iter().chain(&site.candidate_ids) {
                if lookup.node(target).is_none() { return Err(format!("Site {} names an unknown target", site.id)); }
            }
        }
        Ok(())
    }

    fn index_edges(&self, lookup: &mut Lookup, ids: &HashSet<&str>) -> Result<()> {
        let mut edge_ids: HashSet<&str> = HashSet::new();
        for (position, edge) in self.edges.iter().enumerate() {
            if !edge_ids.insert(&edge.id) || ids.contains(edge.id.as_str()) { return Err(format!("Duplicate source index id {}", edge.id)); }
            self.check_edge(edge, lookup)?;
            lookup.edges_out.entry(edge.source_id.clone()).or_default().push(position as u32);
            lookup.edges_in.entry(edge.target_id.clone()).or_default().push(position as u32);
        }
        Ok(())
    }

    fn check_edge(&self, edge: &SourceEdge, lookup: &Lookup) -> Result<()> {
        let (Some(source), Some(_)) = (lookup.node(&edge.source_id), lookup.node(&edge.target_id)) else { return Err(format!("Edge {} names a missing endpoint", edge.id)); };
        if edge.relation.is_empty() || edge.relation.len() > 64 { return Err(format!("Edge {} has an invalid relation", edge.id)); }
        if edge.evidence_ids.is_empty() { return Err(format!("Edge {} has no site evidence", edge.id)); }
        let source_file = match source { Node::File(i) => i, Node::Symbol(i) => *lookup.files.get(&self.symbols[i as usize].file_id).unwrap_or(&u32::MAX) };
        for evidence in &edge.evidence_ids {
            let Some(site) = lookup.sites.get(evidence).map(|i| &self.sites[*i as usize]) else { return Err(format!("Edge {} cites unknown site {evidence}", edge.id)); };
            if lookup.files.get(&site.file_id) != Some(&source_file) { return Err(format!("Edge {} cites a site outside its source file", edge.id)); }
            check_evidence(edge, site)?;
        }
        Ok(())
    }

    /// Counts that follow directly from the validated rows replace whatever the parser reported; the parser's
    /// per-language rows and `lastFullyProcessedFile` are kept.
    fn derive_coverage(&mut self) -> Result<()> {
        let Value::Object(coverage) = &mut self.coverage else { return Err("Source index coverage must be an object".into()); };
        let count = |status: FileStatus| self.files.iter().filter(|f| f.status == status).count() as u64;
        let resolved = |wanted: Resolution| self.sites.iter().filter(|s| s.resolution == wanted).count() as u64;
        coverage.insert("admittedFiles".into(), json!(self.files.len()));
        coverage.insert("parsedFiles".into(), json!(count(FileStatus::Parsed)));
        coverage.insert("partialFiles".into(), json!(count(FileStatus::Partial)));
        coverage.insert("failedFiles".into(), json!(count(FileStatus::Failed)));
        coverage.insert("skippedFiles".into(), json!(count(FileStatus::Skipped)));
        coverage.insert("unsupportedFiles".into(), json!(count(FileStatus::Unsupported)));
        coverage.insert("encounteredSites".into(), json!(self.sites.len()));
        coverage.insert("resolvedSites".into(), json!(resolved(Resolution::ResolvedStatic)));
        coverage.insert("dynamicSites".into(), json!(resolved(Resolution::Dynamic)));
        coverage.insert("unresolvedSites".into(), json!(resolved(Resolution::Unresolved) + resolved(Resolution::Unsupported) + resolved(Resolution::Candidate)));
        coverage.insert("externalSites".into(), json!(resolved(Resolution::External)));
        coverage.entry("languages").or_insert_with(|| json!([]));
        coverage.entry("lastFullyProcessedFile").or_insert(Value::Null);
        match coverage.get("partial") {
            Some(Value::Bool(_)) => {}
            None => { coverage.insert("partial".into(), Value::Bool(false)); }
            Some(_) => return Err("Source index coverage.partial must be a boolean".into()),
        }
        Ok(())
    }

    pub fn is_partial(&self) -> bool { self.coverage.get("partial").and_then(Value::as_bool).unwrap_or(false) }
}

fn check_file(file: &SourceFile, wanted: &ManifestFile) -> Result<()> {
    if file.path != wanted.path || file.id != wanted.id || file.sha256 != wanted.sha256 || file.bytes != wanted.bytes {
        return Err(format!("The source index file {} differs from the snapshot inventory", file.path));
    }
    if wanted.status == "skipped" && file.status != FileStatus::Skipped { return Err(format!("The source index re-labelled skipped file {}", file.path)); }
    if file.language.as_ref().is_some_and(|l| l.len() > 64) { return Err("Source index language name is too long".into()); }
    for line in &file.diagnostics { text_limit(line, "file diagnostic", 8 * 1024)?; }
    Ok(())
}

fn check_symbol(symbol: &SourceSymbol, file: &SourceFile) -> Result<()> {
    if file.path != symbol.path { return Err(format!("Symbol {} path differs from its file", symbol.id)); }
    check_span(&symbol.span, file.bytes, &symbol.id)?;
    if symbol.kind.is_empty() || symbol.kind.len() > 64 { return Err(format!("Symbol {} has an invalid kind", symbol.id)); }
    text_limit(&symbol.name, "symbol name", 4 * 1024)?;
    text_limit(&symbol.qualified_name, "qualified name", 8 * 1024)
}

fn check_site(site: &SourceSite, file: &SourceFile) -> Result<()> {
    if file.path != site.path { return Err(format!("Site {} path differs from its file", site.id)); }
    check_span(&site.span, file.bytes, &site.id)?;
    if site.relation.is_empty() || site.relation.len() > 64 { return Err(format!("Site {} has an invalid relation", site.id)); }
    if let Some(raw) = &site.raw { text_limit(raw, "raw site text", 64 * 1024)?; }
    Ok(())
}

fn check_evidence(edge: &SourceEdge, site: &SourceSite) -> Result<()> {
    match edge.resolution {
        Resolution::ResolvedStatic if site.resolution != Resolution::ResolvedStatic || site.target_id.as_deref() != Some(edge.target_id.as_str()) => {
            Err(format!("Edge {} is not backed by a resolved site for its target", edge.id))
        }
        Resolution::Candidate if site.resolution != Resolution::Candidate || !site.candidate_ids.contains(&edge.target_id) => {
            Err(format!("Edge {} is not backed by a candidate site for its target", edge.id))
        }
        _ => Ok(()),
    }
}

fn check_span(span: &Span, file_bytes: u64, id: &str) -> Result<()> {
    if span.byte_start > span.byte_end || span.byte_end > file_bytes || span.line_start == 0 || span.line_end < span.line_start {
        return Err(format!("Source index record {id} has an invalid source span"));
    }
    Ok(())
}

impl std::fmt::Debug for SourceGraphIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceGraphIndex").field("snapshotId", &self.snapshot_id).field("files", &self.files.len()).field("symbols", &self.symbols.len())
            .field("sites", &self.sites.len()).field("edges", &self.edges.len()).finish()
    }
}
