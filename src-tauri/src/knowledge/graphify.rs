//! Graphify output: passive presence/provenance probing, immutable app-owned admission, and paged faithful reads.
//!
//! A probe only stats and sniffs files. Admission is explicit (the user clicked Graphify): `graph.json` is read
//! through the hardlink-safe opener, streamed record by record (never loaded whole), hashed completely and normalised
//! into an immutable snapshot under `<app-config>/knowledge/graphify/<artifactId>/`:
//!
//! * `graph.json` — the byte-exact original, tee'd from the same read that is parsed and hashed,
//! * `metadata.json` — all graph/top-level metadata, including fields too large for an IPC response,
//! * `nodes|links|hyperedges|communities.jsonl` — one DTO per line, `*.idx` — little-endian `u64` line offsets,
//!   `links.ends` — `(source, target, flags)` node ordinals per link (24 bytes each), `meta.json`, `source.json`.
//!
//! Admitted snapshots are never pruned or invalidated by changes to live output. Large metadata has an explicit
//! private-file receipt rather than a lossy placeholder; metadata page zero remains a `GraphifyArtifact`.
//!
//! Records keep their raw fields (`attributes`); numeric and text ids stay distinct; parallel edges, self-loops,
//! directedness, communities, provenance and hyperedges are kept as found, never merged or invented. The repository's
//! `graph.html` is never read. The artifact hash proves byte identity of `graph.json`, not freshness against source.
use super::dto::*;
use super::fsio::{self, err, now_ms, sha256_hex, shown};
use super::{KnowledgeService, Result};
use crate::code_quality::snapshot::is_link;
use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

const MAX_GRAPH_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 512 * 1024;
const MAX_INLINE_METADATA_BYTES: usize = 128 * 1024;
const MAX_ISSUES: usize = 50;
const MAX_LIMIT: usize = 200;
const MAX_PAGE_BYTES: usize = 1024 * 1024;
const NO_NODE: u64 = u64::MAX;
const CACHED_ARTIFACTS: usize = 4;

type NodeKey = (bool, String);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphifyCounts {
    pub nodes: u64,
    pub links: u64,
    pub hyperedges: u64,
    pub communities: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphifyIssue {
    pub severity: String,
    pub section: String,
    pub ordinal: Option<u64>,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphifyArtifact {
    pub artifact_id: String,
    pub artifact_hash: String,
    pub folder: String,
    pub source_root: Option<String>,
    pub provenance: String,
    pub directed: bool,
    pub multigraph: bool,
    pub counts: GraphifyCounts,
    pub metadata: Map<String, Value>,
    /// Present when metadata is stored privately rather than inlined into this bounded DTO.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_receipt: Option<GraphifyMetadataReceipt>,
    pub bytes: u64,
    // ---- extras ----
    pub output_kind: String,
    pub schema: String,
    pub directed_declared: bool,
    pub multigraph_declared: bool,
    pub provenance_counts: Map<String, Value>,
    pub origin_counts: Map<String, Value>,
    pub self_loops: u64,
    pub dangling_links: u64,
    pub issue_count: u64,
    pub issues: Vec<GraphifyIssue>,
    pub admitted_ms: u64,
}

/// Relative, app-owned snapshot files; never repository paths or executable HTML.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphifyMetadataReceipt {
    pub path: String,
    pub raw_graph_path: String,
    pub bytes: u64,
    pub sha256: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphifyArtifactCheck {
    pub artifact_id: String,
    pub current: bool,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    graph_path: String,
    fingerprint: String,
    project_key: String,
    admitted_ms: u64,
}

pub(crate) struct Loaded {
    dir: PathBuf,
    meta: GraphifyArtifact,
    node_ordinals: OnceLock<std::result::Result<HashMap<NodeKey, u64>, String>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Provenance {
    Verified,
    Unverified,
}

#[derive(Clone)]
pub(crate) struct Located {
    graph_path: PathBuf,
    kind: &'static str,
    provenance: Provenance,
    claimed_root: Option<String>,
    fingerprint: String,
}

enum Eval {
    Absent,
    Found(Located),
    Bad { state: GraphifyState, message: String, source_root: Option<String> },
}

pub(crate) struct Probe {
    pub capability: GraphifyCapability,
    located: Option<Located>,
}

fn bad(state: GraphifyState, message: impl Into<String>) -> Eval {
    Eval::Bad { state, message: message.into(), source_root: None }
}

fn marker_matches(claimed: &str, repository: &Path) -> bool {
    let path = Path::new(claimed);
    if path.exists() {
        crate::setup::canonical(path).map(|canonical| fsio::same_path(&canonical, repository)).unwrap_or(false)
    } else {
        fsio::same_path(path, repository)
    }
}

fn fingerprint(meta: &fs::Metadata) -> String {
    format!("{}:{}", meta.len(), fsio::mtime_nanos(meta))
}

/// `Ok(None)` when there is no marker; the first line of the marker otherwise (it is a path, never executed).
fn read_marker(out_dir: &Path) -> std::result::Result<Option<String>, String> {
    let components = vec![".graphify_root".to_string()];
    let (mut file, _) = match fsio::open_components(out_dir, &components) {
        Ok(opened) => opened,
        Err(e) if e.starts_with("Not found") => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    (&mut file).take(4097).read_to_end(&mut bytes).map_err(|e| err("Cannot read .graphify_root", e))?;
    if bytes.len() > 4096 {
        return Err(".graphify_root is unexpectedly large".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| ".graphify_root is not valid UTF-8".to_string())?;
    let line = text.trim_start_matches('\u{feff}').lines().next().unwrap_or("").trim().to_string();
    Ok((!line.is_empty()).then_some(line))
}

fn evaluate_output(repository: &Path, out_dir: &Path, require_marker: bool, kind: &'static str) -> Eval {
    match fs::symlink_metadata(out_dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return if require_marker { bad(GraphifyState::Unavailable, "The registered Graphify output folder no longer exists") } else { Eval::Absent };
        }
        Err(e) => return bad(GraphifyState::Unavailable, format!("The Graphify output folder cannot be inspected: {e}")),
        Ok(meta) if is_link(&meta) => return bad(GraphifyState::Unavailable, "The Graphify output folder is a link or reparse point and is not read"),
        Ok(meta) if !meta.is_dir() => return bad(GraphifyState::Unavailable, "The Graphify output path is not a folder"),
        Ok(_) => {}
    }
    let components = vec!["graph.json".to_string()];
    let (mut file, graph_path) = match fsio::open_components(out_dir, &components) {
        Ok(opened) => opened,
        Err(e) if e.starts_with("Not found") => {
            return if require_marker { bad(GraphifyState::Unavailable, "The registered Graphify output has no graph.json any more") } else { Eval::Absent };
        }
        Err(e) => return bad(GraphifyState::Unavailable, format!("graph.json cannot be read safely: {e}")),
    };
    let meta = match file.metadata() {
        Ok(meta) => meta,
        Err(e) => return bad(GraphifyState::Unavailable, format!("graph.json cannot be inspected: {e}")),
    };
    if meta.len() == 0 {
        return bad(GraphifyState::Malformed, "graph.json is empty");
    }
    if meta.len() > MAX_GRAPH_BYTES {
        return bad(GraphifyState::Unavailable, format!("graph.json is larger than {MAX_GRAPH_BYTES} bytes and is not opened"));
    }
    let mut head = [0u8; 4096];
    let read = file.read(&mut head).unwrap_or(0);
    let head = head[..read].strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&head[..read]);
    if head.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return bad(GraphifyState::Malformed, "graph.json is not a JSON object; rebuild the Graphify output");
    }
    let claimed = match read_marker(out_dir) {
        Ok(claimed) => claimed,
        Err(e) => return bad(GraphifyState::Unavailable, format!("The Graphify provenance marker cannot be read: {e}")),
    };
    let provenance = match &claimed {
        Some(claimed) if marker_matches(claimed, repository) => Provenance::Verified,
        Some(claimed) => {
            return Eval::Bad {
                state: GraphifyState::Foreign,
                message: format!("Foreign Graphify output: it was built for {}, not for this project, so it is not shown.", shown(claimed)),
                source_root: Some(claimed.clone()),
            };
        }
        None if require_marker => return bad(GraphifyState::Unavailable, "The registered Graphify output has no .graphify_root, so it cannot be tied to this project"),
        None => Provenance::Unverified,
    };
    Eval::Found(Located { graph_path, kind, provenance, claimed_root: claimed, fingerprint: fingerprint(&meta) })
}

fn probe_graphify(repository: &Path, registered: Option<&Path>) -> Probe {
    let local = evaluate_output(repository, &repository.join("graphify-out"), false, "local");
    let external = registered.map(|dir| evaluate_output(repository, dir, true, "registered"));
    let chosen = match (local, external) {
        (Eval::Found(found), _) => Eval::Found(found),
        (_, Some(Eval::Found(found))) => Eval::Found(found),
        (Eval::Absent, Some(other)) => other,
        (local, _) => local,
    };
    match chosen {
        Eval::Absent => Probe { capability: GraphifyCapability { present: false, state: GraphifyState::Absent, source_root: None, artifact_id: None, artifact_hash: None, message: None }, located: None },
        Eval::Bad { state, message, source_root } => Probe { capability: GraphifyCapability { present: true, state, source_root, artifact_id: None, artifact_hash: None, message: Some(message) }, located: None },
        Eval::Found(found) => {
            let (state, message) = match found.provenance {
                Provenance::Verified => (GraphifyState::Present, None),
                Provenance::Unverified => (GraphifyState::Unverified, Some("This Graphify output has no .graphify_root, so it cannot be tied to this project; it is shown as unverified.".to_string())),
            };
            Probe { capability: GraphifyCapability { present: true, state, source_root: found.claimed_root.clone(), artifact_id: None, artifact_hash: None, message }, located: Some(found) }
        }
    }
}

// ---------- normalisation ----------

struct Spool {
    data: BufWriter<File>,
    index: BufWriter<File>,
    offset: u64,
    count: u64,
}

impl Spool {
    fn create(dir: &Path, name: &str) -> Result<Self> {
        let open = |file: String| File::create(dir.join(file)).map(BufWriter::new).map_err(|e| err("Cannot create Graphify snapshot", e));
        Ok(Self { data: open(format!("{name}.jsonl"))?, index: open(format!("{name}.idx"))?, offset: 0, count: 0 })
    }

    fn push(&mut self, line: &[u8]) -> Result<()> {
        let io = |e: std::io::Error| err("Cannot write Graphify snapshot", e);
        self.index.write_all(&self.offset.to_le_bytes()).map_err(io)?;
        self.data.write_all(line).map_err(io)?;
        self.data.write_all(b"\n").map_err(io)?;
        self.offset += line.len() as u64 + 1;
        self.count += 1;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let io = |e: std::io::Error| err("Cannot write Graphify snapshot", e);
        self.data.flush().map_err(io)?;
        self.index.flush().map_err(io)
    }

    fn finish(mut self) -> Result<u64> {
        self.flush()?;
        Ok(self.count)
    }
}

/// Hashes/counts exactly the bytes successfully written without a second serialized copy.
struct SnapshotWriter {
    file: BufWriter<File>,
    hash: Sha256,
    bytes: u64,
}

impl Write for SnapshotWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.file.write(bytes)?;
        self.hash.update(&bytes[..written]);
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

struct ByteCount(usize);

impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.checked_add(bytes.len()).ok_or_else(|| std::io::Error::other("Serialized Graphify response is too large"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encoded_len(value: &impl Serialize) -> Result<usize> {
    let mut count = ByteCount(0);
    serde_json::to_writer(&mut count, value).map_err(|e| e.to_string())?;
    Ok(count.0)
}

struct HashRead {
    file: File,
    shared: Rc<RefCell<(Sha256, u64, BufWriter<File>)>>,
}

impl Read for HashRead {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.file.read(buffer)?;
        let mut shared = self.shared.borrow_mut();
        shared.0.update(&buffer[..read]);
        shared.1 += read as u64;
        if shared.1 > MAX_GRAPH_BYTES {
            return Err(std::io::Error::other("graph.json grew beyond the supported size while it was read"));
        }
        shared.2.write_all(&buffer[..read])?;
        Ok(read)
    }
}

#[derive(Default)]
struct Issues {
    list: Vec<GraphifyIssue>,
    total: u64,
}

impl Issues {
    fn add(&mut self, severity: &str, section: &str, ordinal: Option<u64>, message: String) {
        self.total += 1;
        if self.list.len() < MAX_ISSUES {
            self.list.push(GraphifyIssue { severity: severity.into(), section: section.into(), ordinal, message });
        }
    }
}

struct Normalizer {

    nodes: Spool,
    links_raw: BufWriter<File>,
    hyper_raw: BufWriter<File>,
    node_refs: HashMap<NodeKey, u64>,
    communities: BTreeMap<i64, (u64, BTreeSet<String>)>,
    seen: [bool; 3],
    top_keys: HashSet<String>,
    legacy_edges: bool,
    directed: Option<bool>,
    multigraph: Option<bool>,
    graph_attributes: Option<Value>,
    extras: Map<String, Value>,
    link_count: u64,
    hyper_count: u64,
    issues: Issues,
    failure: Option<String>,
}

fn ref_of(value: &Value) -> Option<GraphifyRef> {
    match value {
        Value::String(text) if !text.is_empty() => Some(GraphifyRef { id: text.clone(), id_kind: GraphifyIdKind::Text }),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some(GraphifyRef { id: number.to_string(), id_kind: GraphifyIdKind::Number }),
        _ => None,
    }
}

fn key_of(reference: &GraphifyRef) -> NodeKey {
    (reference.id_kind == GraphifyIdKind::Number, reference.id.clone())
}

fn take_string(object: &mut Map<String, Value>, key: &str) -> Option<String> {
    match object.get(key) {
        Some(Value::String(_)) => match object.remove(key) {
            Some(Value::String(text)) => Some(text),
            _ => None,
        },
        _ => None,
    }
}

fn take_i64(object: &mut Map<String, Value>, key: &str) -> Option<i64> {
    let number = object.get(key).and_then(Value::as_i64)?;
    object.remove(key);
    Some(number)
}

fn take_confidence(object: &mut Map<String, Value>) -> Option<String> {
    match object.get("confidence") {
        Some(Value::String(text)) if matches!(text.as_str(), "EXTRACTED" | "INFERRED" | "AMBIGUOUS") => take_string(object, "confidence"),
        _ => None,
    }
}

/// A native id: strings are removed from the attributes (lossless), numbers are stringified and kept as found.
fn native_id(object: &mut Map<String, Value>, key: &str) -> Option<String> {
    match object.get(key) {
        Some(Value::String(text)) if !text.is_empty() => take_string(object, key),
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => None,
    }
}

fn line_of(value: &Value, what: &str) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(format!("{what} is larger than the {MAX_RECORD_BYTES}-byte record limit"));
    }
    Ok(bytes)
}

impl Normalizer {
    fn fail(&mut self, message: String) -> String {
        self.failure.get_or_insert_with(|| message.clone());
        message
    }

    fn begin_section(&mut self, key: &str) -> Result<()> {
        let slot = match key {
            "nodes" => 0,
            "links" | "edges" => 1,
            _ => 2,
        };
        if self.seen[slot] {
            let message = if slot == 1 { "graph.json has both \"links\" and \"edges\" (or repeats one); the edge list is ambiguous".to_string() } else { format!("graph.json has more than one \"{key}\" section") };
            return Err(self.fail(message));
        }
        self.seen[slot] = true;
        if key == "edges" {
            self.legacy_edges = true;
        }
        Ok(())
    }

    fn record(&mut self, key: &str, value: Value) -> Result<()> {
        let outcome = match key {
            "nodes" => self.node_record(value),
            "hyperedges" => {
                self.hyper_count += 1;
                line_of(&value, &format!("Hyperedge #{}", self.hyper_count - 1)).and_then(|line| self.hyper_raw.write_all(&line).and_then(|_| self.hyper_raw.write_all(b"\n")).map_err(|e| err("Cannot write Graphify snapshot", e)))
            }
            _ => {
                self.link_count += 1;
                line_of(&value, &format!("Link #{}", self.link_count - 1)).and_then(|line| self.links_raw.write_all(&line).and_then(|_| self.links_raw.write_all(b"\n")).map_err(|e| err("Cannot write Graphify snapshot", e)))
            }
        };
        outcome.map_err(|message| self.fail(message))
    }

    fn node_record(&mut self, value: Value) -> Result<()> {
        let ordinal = self.nodes.count;
        let Value::Object(mut object) = value else { return Err(format!("Node #{ordinal} is not an object")) };
        let id = object.remove("id").ok_or_else(|| format!("Node #{ordinal} has no id"))?;
        let reference = ref_of(&id).ok_or_else(|| format!("Node #{ordinal} has an unusable id (it must be a non-empty string or an integer)"))?;
        let key = key_of(&reference);
        if let Some(previous) = self.node_refs.get(&key) {
            return Err(format!("Duplicate node id {} (nodes #{previous} and #{ordinal}); node names are never merged", shown(&reference.id)));
        }
        self.node_refs.insert(key, ordinal);
        let label = take_string(&mut object, "label");
        let file_type = take_string(&mut object, "file_type");
        let source_file = take_string(&mut object, "source_file");
        let source_location = take_string(&mut object, "source_location");
        let origin = take_string(&mut object, "_origin");
        let community = take_i64(&mut object, "community");
        if let Some(community) = community {
            let entry = self.communities.entry(community).or_default();
            entry.0 += 1;
            if let Some(name) = object.get("community_name").and_then(Value::as_str) {
                entry.1.insert(name.to_string());
            }
        }
        let dto = json!({
            "ref": { "id": reference.id, "idKind": reference.id_kind },
            "label": label.unwrap_or_else(|| reference.id.clone()),
            "fileType": file_type,
            "sourceFile": source_file,
            "sourceLocation": source_location,
            "community": community,
            "origin": origin,
            "attributes": object,
        });
        let line = line_of(&dto, &format!("Node #{ordinal}"))?;
        self.nodes.push(&line)
    }

    fn top_level(&mut self, key: String, value: Value) -> Result<()> {
        match key.as_str() {
            "directed" | "multigraph" => match value {
                Value::Bool(flag) => {
                    if key == "directed" {
                        self.directed = Some(flag);
                    } else {
                        self.multigraph = Some(flag);
                    }
                    Ok(())
                }
                _ => Err(self.fail(format!("\"{key}\" must be true or false"))),
            },
            "graph" => {
                self.graph_attributes = Some(value);
                Ok(())
            }
            _ => {
                self.extras.insert(key, value);
                Ok(())
            }
        }
    }
}

struct Top<'a>(&'a mut Normalizer);
struct Section<'a> {
    sink: &'a mut Normalizer,
    key: String,
}

fn custom<E: serde::de::Error>(message: String) -> E {
    E::custom(message)
}

impl<'de, 'a> DeserializeSeed<'de> for Top<'a> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> std::result::Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de, 'a> Visitor<'de> for Top<'a> {
    type Value = ();
    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a Graphify graph object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            if !self.0.top_keys.insert(key.clone()) {
                return Err(custom(self.0.fail(format!("graph.json repeats the top-level field \"{}\"; metadata and graph semantics must be unambiguous", shown(&key)))));
            }
            match key.as_str() {
                "nodes" | "links" | "edges" | "hyperedges" => map.next_value_seed(Section { sink: &mut *self.0, key })?,
                _ => {
                    let value: Value = map.next_value()?;
                    self.0.top_level(key, value).map_err(custom::<A::Error>)?;
                }
            }
        }
        Ok(())
    }
}

impl<'de, 'a> DeserializeSeed<'de> for Section<'a> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> std::result::Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de, 'a> Visitor<'de> for Section<'a> {
    type Value = ();
    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "an array for \"{}\"", self.key)
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<(), E> {
        if self.key == "nodes" {
            let message = self.sink.fail("graph.json \"nodes\" must be an array".into());
            return Err(custom(message));
        }
        self.sink.begin_section(&self.key).map_err(custom::<E>)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        self.sink.begin_section(&self.key).map_err(custom::<A::Error>)?;
        while let Some(value) = seq.next_element::<Value>()? {
            self.sink.record(&self.key, value).map_err(custom::<A::Error>)?;
        }
        Ok(())
    }
}

struct Normalized {
    hash: String,
    bytes: u64,
    meta: GraphifyArtifact,
}

fn lines(path: &Path) -> Result<impl Iterator<Item = std::io::Result<String>>> {
    let file = File::open(path).map_err(|e| err("Cannot read Graphify snapshot", e))?;
    Ok(BufReader::new(file).lines())
}

fn write_ends(writer: &mut BufWriter<File>, source: u64, target: u64, flags: u64) -> Result<()> {
    let io = |e: std::io::Error| err("Cannot write Graphify snapshot", e);
    writer.write_all(&source.to_le_bytes()).map_err(io)?;
    writer.write_all(&target.to_le_bytes()).map_err(io)?;
    writer.write_all(&flags.to_le_bytes()).map_err(io)
}

fn count_of(counts: &mut Map<String, Value>, key: &str) {
    let next = counts.get(key).and_then(Value::as_u64).unwrap_or(0) + 1;
    counts.insert(key.to_string(), json!(next));
}

/// Streams `graph.json` into the snapshot directory `dir`. Returns the finished metadata (artifact id not yet set).
fn normalize(source: File, expected_len: u64, dir: &Path, folder: &str, located: &Located) -> Result<Normalized> {
    let create = |name: &str| File::create(dir.join(name)).map(BufWriter::new).map_err(|e| err("Cannot create Graphify snapshot", e));
    let mut normalizer = Normalizer {

        nodes: Spool::create(dir, "nodes")?,
        links_raw: create("links.raw")?,
        hyper_raw: create("hyperedges.raw")?,
        node_refs: HashMap::new(),
        communities: BTreeMap::new(),
        seen: [false; 3],
        top_keys: HashSet::new(),
        legacy_edges: false,
        directed: None,
        multigraph: None,
        graph_attributes: None,
        extras: Map::new(),
        link_count: 0,
        hyper_count: 0,
        issues: Issues::default(),
        failure: None,
    };
    let check = source.try_clone().map_err(|e| err("Cannot inspect graph.json", e))?;
    let shared = Rc::new(RefCell::new((Sha256::new(), 0u64, create("graph.json")?)));
    let reader = BufReader::with_capacity(256 * 1024, HashRead { file: source, shared: shared.clone() });
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let parsed = Top(&mut normalizer).deserialize(&mut deserializer).and_then(|()| deserializer.end());
    if let Err(e) = parsed {
        return Err(normalizer.failure.take().unwrap_or_else(|| format!("graph.json is not a valid Graphify graph: {e}")));
    }
    drop(deserializer);
    let (hasher, bytes) = {
        let mut shared = shared.borrow_mut();
        shared.2.flush().map_err(|e| err("Cannot retain the original graph.json", e))?;
        (std::mem::replace(&mut shared.0, Sha256::new()), shared.1)
    };
    if bytes != expected_len || check.metadata().map_err(|e| err("Cannot inspect graph.json", e))?.len() != expected_len {
        return Err("graph.json changed while it was being read; try again".into());
    }
    let hash = fsio::hex(hasher.finalize());
    if !normalizer.seen[0] {
        return Err("graph.json has no \"nodes\" array".into());
    }
    if !normalizer.seen[1] {
        normalizer.issues.add("warning", "links", None, "graph.json has no \"links\" (or legacy \"edges\") array; the graph has no links".into());
    }
    let directed = normalizer.directed.unwrap_or(false);
    let multigraph = normalizer.multigraph.unwrap_or(false);
    let io = |e: std::io::Error| err("Cannot write Graphify snapshot", e);

    // Pass 2: links (need the node table and the artifact hash).
    normalizer.links_raw.flush().map_err(io)?;
    normalizer.hyper_raw.flush().map_err(io)?;
    let mut links = Spool::create(dir, "links")?;
    let mut ends = create("links.ends")?;
    let mut used: HashSet<String> = HashSet::new();
    let mut provenance_counts = Map::new();
    let mut origin_counts = Map::new();
    let (mut self_loops, mut dangling_links) = (0u64, 0u64);
    for (ordinal, text) in lines(&dir.join("links.raw"))?.enumerate() {
        let ordinal = ordinal as u64;
        let text = text.map_err(|e| err("Cannot read Graphify snapshot", e))?;
        let value: Value = serde_json::from_str(&text).map_err(|e| err("Cannot read Graphify snapshot", e))?;
        let Value::Object(mut object) = value else { return Err(format!("Link #{ordinal} is not an object")) };
        let endpoint = |object: &mut Map<String, Value>, name: &str| -> Result<GraphifyRef> {
            let value = object.remove(name).ok_or_else(|| format!("Link #{ordinal} has no {name}"))?;
            ref_of(&value).ok_or_else(|| format!("Link #{ordinal} has an unusable {name} id (it must be a non-empty string or an integer)"))
        };
        let source_ref = endpoint(&mut object, "source")?;
        let target_ref = endpoint(&mut object, "target")?;
        let relation = take_string(&mut object, "relation");
        if relation.is_none() {
            normalizer.issues.add("warning", "links", Some(ordinal), "The link has no text \"relation\"".into());
        }
        let link_directed = match object.get("directed") {
            Some(Value::Bool(flag)) => {
                let flag = *flag;
                object.remove("directed");
                flag
            }
            _ => directed,
        };
        let key = match object.get("key") {
            Some(Value::String(_)) => take_string(&mut object, "key"),
            Some(Value::Number(number)) => Some(number.to_string()),
            _ => None,
        };
        let native = native_id(&mut object, "id");
        let confidence = take_confidence(&mut object);
        let score = match object.get("confidence_score") {
            Some(Value::Number(_)) => object.remove("confidence_score"),
            _ => None,
        };
        let source_file = take_string(&mut object, "source_file");
        let source_location = take_string(&mut object, "source_location");
        let origin = take_string(&mut object, "_origin");
        let source_node = normalizer.node_refs.get(&key_of(&source_ref)).copied();
        let target_node = normalizer.node_refs.get(&key_of(&target_ref)).copied();
        let dangling = source_node.is_none() || target_node.is_none();
        if dangling {
            dangling_links += 1;
            normalizer.issues.add("warning", "links", Some(ordinal), format!("The link references a node that does not exist ({} → {})", shown(&source_ref.id), shown(&target_ref.id)));
        }
        if source_node.is_some() && source_node == target_node {
            self_loops += 1;
        }
        let internal = format!("graphify:{hash}:link:{ordinal}");
        let id = match &native {
            Some(native) if !native.starts_with("graphify:") && used.insert(native.clone()) => native.clone(),
            Some(native) => {
                normalizer.issues.add("warning", "links", Some(ordinal), format!("The link id {} is reused or reserved; its internal id is used", shown(native)));
                used.insert(internal.clone());
                internal
            }
            None => {
                used.insert(internal.clone());
                internal
            }
        };
        count_of(&mut provenance_counts, confidence.as_deref().unwrap_or("none"));
        count_of(&mut origin_counts, origin.as_deref().unwrap_or("none"));
        let dto = json!({
            "ordinal": ordinal,
            "id": id,
            "nativeId": native,
            "source": source_ref,
            "target": target_ref,
            "key": key,
            "relation": relation.unwrap_or_default(),
            "directed": link_directed,
            "confidence": confidence,
            "confidenceScore": score,
            "sourceFile": source_file,
            "sourceLocation": source_location,
            "origin": origin,
            "attributes": object,
            "dangling": dangling,
        });
        links.push(&line_of(&dto, &format!("Link #{ordinal}"))?)?;
        write_ends(&mut ends, source_node.unwrap_or(NO_NODE), target_node.unwrap_or(NO_NODE), u64::from(link_directed) | (u64::from(dangling) << 1))?;
    }
    let link_total = links.finish()?;
    ends.flush().map_err(io)?;

    // Hyperedges stay separate group records.
    let mut hyperedges = Spool::create(dir, "hyperedges")?;
    let mut hyper_used = HashSet::new();
    for (ordinal, text) in lines(&dir.join("hyperedges.raw"))?.enumerate() {
        let ordinal = ordinal as u64;
        let text = text.map_err(|e| err("Cannot read Graphify snapshot", e))?;
        let value: Value = serde_json::from_str(&text).map_err(|e| err("Cannot read Graphify snapshot", e))?;
        let Value::Object(mut object) = value else { return Err(format!("Hyperedge #{ordinal} is not an object")) };
        let native = native_id(&mut object, "id");
        let list_key = if object.get("nodes").is_some_and(Value::is_array) { "nodes" } else { "members" };
        let members = match object.remove(list_key) {
            Some(Value::Array(members)) => members,
            _ => return Err(format!("Hyperedge #{ordinal} has no node list")),
        };
        let mut node_ids = Vec::with_capacity(members.len());
        let mut unresolved = 0u64;
        for member in &members {
            let reference = ref_of(member).ok_or_else(|| format!("Hyperedge #{ordinal} has a member that is not a node id"))?;
            if !normalizer.node_refs.contains_key(&key_of(&reference)) {
                unresolved += 1;
            }
            node_ids.push(reference);
        }
        if unresolved > 0 {
            normalizer.issues.add("warning", "hyperedges", Some(ordinal), format!("The hyperedge lists {unresolved} nodes that do not exist"));
        }
        let label = take_string(&mut object, "label");
        let relation = take_string(&mut object, "relation");
        let confidence = take_confidence(&mut object);
        let source_file = take_string(&mut object, "source_file");
        let internal = format!("graphify:{hash}:hyperedge:{ordinal}");
        let id = match &native {
            Some(native) if !native.starts_with("graphify:") && hyper_used.insert(native.clone()) => native.clone(),
            Some(native) => {
                normalizer.issues.add("warning", "hyperedges", Some(ordinal), format!("The hyperedge id {} is reused or reserved; its internal id is used", shown(native)));
                internal
            }
            None => internal,
        };
        let dto = json!({
            "id": id,
            "ordinal": ordinal,
            "nativeId": native,
            "label": label.or_else(|| native.clone()).unwrap_or_else(|| "(unlabeled hyperedge)".to_string()),
            "relation": relation,
            "nodeIds": node_ids,
            "confidence": confidence,
            "sourceFile": source_file,
            "attributes": object,
            "unresolvedMembers": unresolved,
        });
        hyperedges.push(&line_of(&dto, &format!("Hyperedge #{ordinal}"))?)?;
    }
    let hyper_total = hyperedges.finish()?;

    let node_total = normalizer.nodes.count;
    let mut communities = Spool::create(dir, "communities")?;
    let community_total = normalizer.communities.len() as u64;
    for (id, (size, names)) in &normalizer.communities {
        let label = if names.len() == 1 { names.iter().next().cloned() } else { None };
        communities.push(&serde_json::to_vec(&json!({ "id": id, "label": label, "size": size })).map_err(|e| e.to_string())?)?;
    }
    communities.finish()?;
    normalizer.nodes.flush()?;
    let _ = fs::remove_file(dir.join("links.raw"));
    let _ = fs::remove_file(dir.join("hyperedges.raw"));

    let mut metadata = std::mem::take(&mut normalizer.extras);
    if let Some(graph) = normalizer.graph_attributes.take() {
        metadata.insert("graph".into(), graph);
    }
    let mut metadata_file = SnapshotWriter { file: create("metadata.json")?, hash: Sha256::new(), bytes: 0 };
    serde_json::to_writer(&mut metadata_file, &metadata).map_err(|e| err("Cannot retain Graphify metadata", e))?;
    metadata_file.file.flush().map_err(|e| err("Cannot retain Graphify metadata", e))?;
    let metadata_receipt = if metadata_file.bytes > MAX_INLINE_METADATA_BYTES as u64 {
        metadata.clear();
        Some(GraphifyMetadataReceipt {
            path: "metadata.json".into(),
            raw_graph_path: "graph.json".into(),
            bytes: metadata_file.bytes,
            sha256: fsio::hex(metadata_file.hash.finalize()),
            message: "Metadata exceeds the inline response budget. All original metadata is retained in the app-owned immutable metadata.json; graph.json retains the full byte-exact artifact. The inline metadata map is intentionally empty, not a replacement for the original.".into(),
        })
    } else {
        None
    };
    let meta = GraphifyArtifact {
        artifact_id: String::new(),
        artifact_hash: hash.clone(),
        folder: folder.to_string(),
        source_root: located.claimed_root.clone(),
        provenance: if located.provenance == Provenance::Verified { "verified" } else { "unverified" }.into(),
        directed,
        multigraph,
        counts: GraphifyCounts { nodes: node_total, links: link_total, hyperedges: hyper_total, communities: community_total },
        metadata,
        metadata_receipt,
        bytes,
        output_kind: located.kind.into(),
        schema: if normalizer.legacy_edges { "legacyEdges" } else { "nodeLink" }.into(),
        directed_declared: normalizer.directed.is_some(),
        multigraph_declared: normalizer.multigraph.is_some(),
        provenance_counts,
        origin_counts,
        self_loops,
        dangling_links,
        issue_count: normalizer.issues.total,
        issues: normalizer.issues.list.clone(),
        admitted_ms: now_ms(),
    };
    if encoded_len(&meta)? > MAX_RECORD_BYTES - 32 {
        return Err(format!("Graphify artifact descriptors exceed the {MAX_RECORD_BYTES}-byte response limit; shorten unusually large provenance/origin labels or metadata descriptors before admission"));
    }

    Ok(Normalized { hash, bytes, meta })
}

// ---------- service ----------

fn valid_artifact_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn artifact_id(project_key: &str, hash: &str) -> String {
    sha256_hex(format!("omp-graphify-artifact\0{project_key}\0{hash}").as_bytes())[..32].to_string()
}

fn unavailable(id: &str) -> String {
    format!("Graphify artifact {id} is no longer available in the app-owned snapshot store. Open Graphify again to admit the current output.")
}

fn read_json_file<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fsio::read_regular_bounded(path, 8 * 1024 * 1024)?;
    serde_json::from_slice(&bytes).map_err(|e| err(&format!("Invalid {}", path.display()), e))
}

/// Removes abandoned `.incoming-*` snapshot folders (crashed admissions) older than an hour.
pub(crate) fn cleanup_incoming(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let stale = entry.file_name().to_str().is_some_and(|n| n.starts_with(".incoming-"))
            && entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() > 3600);
        if stale {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn read_index(dir: &Path, name: &str, ordinal: u64) -> Result<u64> {
    let mut file = File::open(dir.join(format!("{name}.idx"))).map_err(|e| err("Cannot read Graphify snapshot", e))?;
    file.seek(SeekFrom::Start(ordinal * 8)).map_err(|e| err("Cannot read Graphify snapshot", e))?;
    let mut bytes = [0u8; 8];
    file.read_exact(&mut bytes).map_err(|_| "Graphify record is out of range".to_string())?;
    Ok(u64::from_le_bytes(bytes))
}

fn record_count(dir: &Path, name: &str) -> Result<u64> {
    Ok(fs::metadata(dir.join(format!("{name}.idx"))).map_err(|e| err("Cannot read Graphify snapshot", e))?.len() / 8)
}

fn section_name(section: GraphifySection) -> &'static str {
    match section {
        GraphifySection::Metadata => "metadata",
        GraphifySection::Nodes => "nodes",
        GraphifySection::Links => "links",
        GraphifySection::Hyperedges => "hyperedges",
        GraphifySection::Communities => "communities",
    }
}

/// `count` consecutive records starting at `start`.
fn read_run(dir: &Path, name: &str, start: u64, count: usize) -> Result<Vec<Value>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let offset = read_index(dir, name, start)?;
    let mut file = File::open(dir.join(format!("{name}.jsonl"))).map_err(|e| err("Cannot read Graphify snapshot", e))?;
    file.seek(SeekFrom::Start(offset)).map_err(|e| err("Cannot read Graphify snapshot", e))?;
    let mut out = Vec::with_capacity(count);
    for line in BufReader::new(file).lines().take(count) {
        let line = line.map_err(|e| err("Cannot read Graphify snapshot", e))?;
        out.push(serde_json::from_str(&line).map_err(|e| err("Corrupt Graphify snapshot", e))?);
    }
    Ok(out)
}

/// The records at arbitrary `ordinals`, in that order.
fn read_records(dir: &Path, name: &str, ordinals: &[u64]) -> Result<Vec<Value>> {
    if ordinals.is_empty() {
        return Ok(Vec::new());
    }
    let mut index = File::open(dir.join(format!("{name}.idx"))).map_err(|e| err("Cannot read Graphify snapshot", e))?;
    let mut data = BufReader::new(File::open(dir.join(format!("{name}.jsonl"))).map_err(|e| err("Cannot read Graphify snapshot", e))?);
    let mut out = Vec::with_capacity(ordinals.len());
    for ordinal in ordinals {
        index.seek(SeekFrom::Start(ordinal * 8)).map_err(|e| err("Cannot read Graphify snapshot", e))?;
        let mut bytes = [0u8; 8];
        index.read_exact(&mut bytes).map_err(|_| "Graphify record is out of range".to_string())?;
        data.seek(SeekFrom::Start(u64::from_le_bytes(bytes))).map_err(|e| err("Cannot read Graphify snapshot", e))?;
        let mut line = String::new();
        data.read_line(&mut line).map_err(|e| err("Cannot read Graphify snapshot", e))?;
        out.push(serde_json::from_str(line.trim_end()).map_err(|e| err("Corrupt Graphify snapshot", e))?);
    }
    Ok(out)
}

/// Keeps the longest prefix that fits including the envelope, commas, wrappers and hydrated records.
/// The caller sizes an empty page with the longest possible next-offset before adding items.
fn within_budget(mut items: Vec<Value>, envelope_bytes: usize) -> Result<Vec<Value>> {
    let mut used = envelope_bytes;
    let mut keep = 0usize;
    for item in &items {
        let bytes = encoded_len(item)?;
        let next = used + bytes + usize::from(keep > 0);
        if next > MAX_PAGE_BYTES {
            if keep == 0 {
                return Err(format!("A Graphify response record ({bytes} bytes) with its envelope exceeds the {MAX_PAGE_BYTES}-byte page limit. For hydrated neighbors, read the links section and query each endpoint with kind \"node\" separately; the full original remains in the app-owned immutable graph.json."));
            }
            break;
        }
        used = next;
        keep += 1;
    }
    items.truncate(keep);
    Ok(items)
}

fn window(offset: usize, limit: usize) -> Result<()> {
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(format!("Query limit must be between 1 and {MAX_LIMIT}"));
    }
    if offset > 100_000_000 {
        return Err("Query offset is out of range".into());
    }
    Ok(())
}

fn rank(exact: &[&str], haystacks: &[&str], needle: &str, lowered: &str) -> Option<u8> {
    if exact.iter().any(|h| *h == needle) {
        return Some(0);
    }
    if haystacks.iter().any(|h| h.contains(needle)) {
        return Some(1);
    }
    haystacks.iter().any(|h| h.to_lowercase().contains(lowered)).then_some(2)
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

impl Loaded {
    fn node_ordinal(&self, reference: &GraphifyRef) -> Result<u64> {
        let map = self.node_ordinals.get_or_init(|| {
            #[derive(Deserialize)]
            struct Line {
                #[serde(rename = "ref")]
                reference: GraphifyRef,
            }
            let mut map = HashMap::new();
            for (ordinal, line) in lines(&self.dir.join("nodes.jsonl"))?.enumerate() {
                let line = line.map_err(|e| err("Cannot read Graphify snapshot", e))?;
                let parsed: Line = serde_json::from_str(&line).map_err(|e| err("Corrupt Graphify snapshot", e))?;
                map.insert(key_of(&parsed.reference), ordinal as u64);
            }
            Ok(map)
        });
        match map {
            Ok(map) => map.get(&key_of(reference)).copied().ok_or_else(|| format!("Graphify node not found: {}", shown(&reference.id))),
            Err(e) => Err(e.clone()),
        }
    }
}

impl KnowledgeService {
    fn graphify_root(&self) -> PathBuf {
        self.dir.join("graphify")
    }

    fn registered_output(&self, repository: &Path) -> Option<PathBuf> {
        self.state.lock().integrations.graphify_outputs.get(&fsio::norm(repository)).map(PathBuf::from)
    }

    /// Passive: stats and sniffs files only. Never creates a snapshot.
    pub(crate) fn graphify_capability(&self, folder: Option<&str>) -> GraphifyCapability {
        let absent = |state: GraphifyState, message: Option<String>| GraphifyCapability { present: false, state, source_root: None, artifact_id: None, artifact_hash: None, message };
        let Some(folder) = folder else { return absent(GraphifyState::Absent, None) };
        match self.repository_root(folder) {
            Ok(repository) => probe_graphify(&repository, self.registered_output(&repository).as_deref()).capability,
            Err(message) => absent(GraphifyState::Unavailable, Some(message)),
        }
    }

    /// Admits the project's current Graphify output as an immutable snapshot (explicit user action).
    pub fn open_graphify_artifact(&self, folder: &str) -> Result<GraphifyArtifact> {
        let repository = self.repository_root(folder)?;
        let probe = probe_graphify(&repository, self.registered_output(&repository).as_deref());
        let Some(located) = probe.located else {
            return Err(probe.capability.message.unwrap_or_else(|| "This project has no Graphify output".to_string()));
        };
        let project_key = fsio::norm(&repository);
        let root = self.graphify_root();
        fs::create_dir_all(&root).map_err(|e| err("Cannot create the Graphify store", e))?;
        cleanup_incoming(&root);
        let (file, _) = {
            let parent = located.graph_path.parent().ok_or("graph.json has no folder")?;
            fsio::open_components(parent, &["graph.json".to_string()])?
        };
        let before = file.metadata().map_err(|e| err("Cannot inspect graph.json", e))?;
        if fingerprint(&before) != located.fingerprint {
            return Err("graph.json changed while it was being opened; try again".into());
        }
        let incoming = root.join(format!(".incoming-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&incoming).map_err(|e| err("Cannot create the Graphify snapshot", e))?;
        let outcome = normalize(file, before.len(), &incoming, &repository.to_string_lossy(), &located);
        let normalized = match outcome {
            Ok(normalized) => normalized,
            Err(message) => {
                let _ = fs::remove_dir_all(&incoming);
                return Err(format!("Graphify output rejected: {message}"));
            }
        };
        let id = artifact_id(&project_key, &normalized.hash);
        let final_dir = root.join(&id);
        let source = Source { graph_path: located.graph_path.to_string_lossy().into_owned(), fingerprint: located.fingerprint.clone(), project_key: project_key.clone(), admitted_ms: now_ms() };
        let mut meta = normalized.meta;
        meta.artifact_id = id.clone();
        let _ = normalized.bytes;
        let published = (|| -> Result<GraphifyArtifact> {
            if final_dir.join("meta.json").is_file() && final_dir.join("graph.json").is_file() && final_dir.join("metadata.json").is_file() {
                // Same bytes for the same project: the immutable snapshot already exists; only its live pointer moves.
                let existing: GraphifyArtifact = read_json_file(&final_dir.join("meta.json"))?;
                crate::dependency_security::atomic_json(&final_dir.join("source.json"), &source, 1024 * 1024)?;
                return Ok(existing);
            }
            if final_dir.join("meta.json").is_file() {
                // A legacy snapshot of the same bytes lacks the byte-exact original and complete metadata; this complete one replaces it.
                fs::remove_dir_all(&final_dir).map_err(|e| err("Cannot replace the legacy Graphify snapshot", e))?;
            }
            crate::dependency_security::atomic_json(&incoming.join("meta.json"), &meta, 8 * 1024 * 1024)?;
            crate::dependency_security::atomic_json(&incoming.join("source.json"), &source, 1024 * 1024)?;
            match fs::rename(&incoming, &final_dir) {
                Ok(()) => Ok(meta.clone()),
                Err(_) if final_dir.join("meta.json").is_file() && final_dir.join("graph.json").is_file() => {
                    crate::dependency_security::atomic_json(&final_dir.join("source.json"), &source, 1024 * 1024)?;
                    read_json_file(&final_dir.join("meta.json"))
                }
                Err(e) => Err(err("Cannot publish the Graphify snapshot", e)),
            }
        })();
        let _ = fs::remove_dir_all(&incoming);
        let published = published?;
        self.graphify_cache.lock().remove(&id);
        Ok(published)
    }

    /// Associates an external Graphify output folder with the project, but only when its `.graphify_root` names it.
    pub fn register_graphify_output(&self, folder: &str, directory: &str) -> Result<KnowledgeCapabilities> {
        let repository = self.repository_root(folder)?;
        let chosen = Path::new(directory.trim());
        if !chosen.is_absolute() {
            return Err("Choose an absolute Graphify output folder".into());
        }
        let meta = fs::symlink_metadata(chosen).map_err(|e| err("Cannot inspect the Graphify folder", e))?;
        if !meta.is_dir() || is_link(&meta) {
            return Err("The Graphify output must be a plain folder".into());
        }
        let canonical = crate::setup::canonical(chosen).map_err(|e| err("Cannot resolve the Graphify folder", e))?;
        let output = if canonical.join("graph.json").exists() { canonical.clone() } else { canonical.join("graphify-out") };
        match evaluate_output(&repository, &output, true, "registered") {
            Eval::Found(_) => {}
            Eval::Bad { message, .. } => return Err(message),
            Eval::Absent => return Err("No graph.json was found in that folder".into()),
        }
        {
            let mut state = self.state.lock();
            if let Some(error) = &state.integrations_error {
                return Err(format!("Integration settings are unreadable: {error}"));
            }
            let mut settings = state.integrations.clone();
            settings.graphify_outputs.insert(fsio::norm(&repository), output.to_string_lossy().into_owned());
            self.save_integrations(&settings)?;
            state.integrations = settings;
        }
        self.get_knowledge_capabilities(Some(folder))
    }

    /// Reads only the immutable app-owned snapshot; the live source may have changed or vanished.
    fn loaded_graphify(&self, id: &str) -> Result<Arc<Loaded>> {
        if !valid_artifact_id(id) {
            return Err("Unknown Graphify artifact".into());
        }
        let dir = self.graphify_root().join(id);
        if let Some(loaded) = self.graphify_cache.lock().get(id) {
            return Ok(loaded.clone());
        }
        let meta: GraphifyArtifact = read_json_file(&dir.join("meta.json")).map_err(|_| unavailable(id))?;
        if meta.artifact_id != id {
            return Err(unavailable(id));
        }
        let loaded = Arc::new(Loaded { dir, meta, node_ordinals: OnceLock::new() });
        let mut cache = self.graphify_cache.lock();
        if cache.len() >= CACHED_ARTIFACTS {
            cache.clear();
        }
        cache.insert(id.to_string(), loaded.clone());
        Ok(loaded)
    }

    /// Passive full-hash comparison. Does not admit, mutate, prune or invalidate any snapshot.
    pub fn check_graphify_artifact(&self, artifact_id: &str) -> Result<GraphifyArtifactCheck> {
        let loaded = self.loaded_graphify(artifact_id)?;
        let result = |current, message: Option<String>| GraphifyArtifactCheck { artifact_id: artifact_id.to_string(), current, message };
        let inspect = (|| -> Result<bool> {
            let source: Source = read_json_file(&loaded.dir.join("source.json"))?;
            let repository = self.repository_root(&loaded.meta.folder)?;
            if source.project_key != fsio::norm(&repository) || artifact_id != self::artifact_id(&source.project_key, &loaded.meta.artifact_hash) {
                return Err("The snapshot project identity no longer matches this repository".into());
            }
            let probe = probe_graphify(&repository, self.registered_output(&repository).as_deref());
            let located = probe.located.ok_or_else(|| probe.capability.message.unwrap_or_else(|| "The live Graphify output is absent".into()))?;
            let parent = located.graph_path.parent().ok_or("graph.json has no folder")?;
            let (mut file, _) = fsio::open_components(parent, &["graph.json".to_string()])?;
            let before = file.metadata().map_err(|e| err("Cannot inspect graph.json", e))?;
            if fingerprint(&before) != located.fingerprint {
                return Err("graph.json changed while it was being opened".into());
            }
            let mut hash = Sha256::new();
            let mut bytes = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let count = file.read(&mut buffer).map_err(|e| err("Cannot compare graph.json", e))?;
                if count == 0 {
                    break;
                }
                bytes += count as u64;
                if bytes > MAX_GRAPH_BYTES {
                    return Err("graph.json grew beyond the supported size during comparison".into());
                }
                hash.update(&buffer[..count]);
            }
            let after = file.metadata().map_err(|e| err("Cannot inspect graph.json", e))?;
            if bytes != before.len() || fingerprint(&before) != fingerprint(&after) {
                return Err("graph.json changed during comparison".into());
            }
            Ok(fsio::hex(hash.finalize()) == loaded.meta.artifact_hash
                && located.claimed_root == loaded.meta.source_root
                && (located.provenance == Provenance::Verified) == (loaded.meta.provenance == "verified"))
        })();
        Ok(match inspect {
            Ok(true) => result(true, None),
            Ok(false) => result(false, Some("New Graphify output available — Refresh. This immutable snapshot remains readable.".into())),
            Err(message) => result(false, Some(format!("Live Graphify output cannot be compared: {message}. This immutable snapshot remains readable."))),
        })
    }

    /// One strict page of a snapshot section.
    pub fn read_graphify_graph(&self, artifact_id: &str, section: GraphifySection, offset: usize, limit: usize) -> Result<GraphifyPage> {
        window(offset, limit)?;
        let loaded = self.loaded_graphify(artifact_id)?;
        let (total, items) = match section {
            GraphifySection::Metadata => (1u64, if offset == 0 { vec![serde_json::to_value(&loaded.meta).map_err(|e| e.to_string())?] } else { Vec::new() }),
            _ => {
                let name = section_name(section);
                let total = record_count(&loaded.dir, name)?;
                let start = (offset as u64).min(total);
                let count = (total - start).min(limit as u64) as usize;
                (total, read_run(&loaded.dir, name, start, count)?)
            }
        };
        let mut page = GraphifyPage {
            artifact_id: loaded.meta.artifact_id.clone(),
            artifact_hash: loaded.meta.artifact_hash.clone(),
            section,
            total,
            offset: offset as u64,
            items: Vec::new(),
            next_offset: Some(u64::MAX),
        };
        page.items = within_budget(items, encoded_len(&page)?)?;
        let end = (offset as u64).min(total) + page.items.len() as u64;
        page.next_offset = (end < total).then_some(end);
        Ok(page)
    }

    /// Exact search, neighbourhoods and lookups over a snapshot. Strict query union; nothing is guessed or merged.
    pub fn query_graphify_graph(&self, artifact_id: &str, query: &GraphifyQuery) -> Result<GraphifyQueryPage> {
        let loaded = self.loaded_graphify(artifact_id)?;
        let dir = &loaded.dir;
        let page = |kind: &'static str, total: u64, offset: usize, items: Vec<Value>| -> Result<GraphifyQueryPage> {
            let mut page = GraphifyQueryPage { artifact_id: loaded.meta.artifact_id.clone(), artifact_hash: loaded.meta.artifact_hash.clone(), kind, total, offset: offset as u64, items: Vec::new(), next_offset: Some(u64::MAX) };
            page.items = within_budget(items, encoded_len(&page)?)?;
            let end = (offset as u64).min(total) + page.items.len() as u64;
            page.next_offset = (end < total).then_some(end);
            Ok(page)
        };
        // Pages an ordered list of record ordinals of one section.
        let page_of = |kind: &'static str, section: &str, ordinals: &[u64], offset: usize, limit: usize| -> Result<GraphifyQueryPage> {
            let total = ordinals.len();
            let start = offset.min(total);
            let end = (start + limit).min(total);
            page(kind, total as u64, offset, read_records(dir, section, &ordinals[start..end])?)
        };
        match query {
            GraphifyQuery::Node { id } => {
                let ordinal = loaded.node_ordinal(id)?;
                page("node", 1, 0, read_records(dir, "nodes", &[ordinal])?)
            }
            GraphifyQuery::Search { text, offset, limit } => {
                window(*offset, *limit)?;
                let needle = text.trim();
                if needle.is_empty() {
                    return Err("Search text is required".into());
                }
                let lowered = needle.to_lowercase();
                let mut hits: Vec<(u8, u8, u64)> = Vec::new();
                for (ordinal, line) in lines(&dir.join("nodes.jsonl"))?.enumerate() {
                    let value: Value = serde_json::from_str(&line.map_err(|e| err("Cannot read Graphify snapshot", e))?).map_err(|e| err("Corrupt Graphify snapshot", e))?;
                    let reference = value.get("ref").cloned().unwrap_or(Value::Null);
                    let (id, label, file) = (str_of(&reference, "id"), str_of(&value, "label"), str_of(&value, "sourceFile"));
                    let community = value.get("attributes").map(|a| str_of(a, "community_name")).unwrap_or("");
                    if let Some(found) = rank(&[id, label], &[id, label, file, community], needle, &lowered) {
                        hits.push((found, 0, ordinal as u64));
                    }
                }
                for (ordinal, line) in lines(&dir.join("hyperedges.jsonl"))?.enumerate() {
                    let value: Value = serde_json::from_str(&line.map_err(|e| err("Cannot read Graphify snapshot", e))?).map_err(|e| err("Corrupt Graphify snapshot", e))?;
                    let (id, label) = (str_of(&value, "id"), str_of(&value, "label"));
                    if let Some(found) = rank(&[id, label], &[id, label], needle, &lowered) {
                        hits.push((found, 1, ordinal as u64));
                    }
                }
                for (ordinal, line) in lines(&dir.join("links.jsonl"))?.enumerate() {
                    let value: Value = serde_json::from_str(&line.map_err(|e| err("Cannot read Graphify snapshot", e))?).map_err(|e| err("Corrupt Graphify snapshot", e))?;
                    let (id, relation, file) = (str_of(&value, "id"), str_of(&value, "relation"), str_of(&value, "sourceFile"));
                    if let Some(found) = rank(&[id], &[id, relation, file], needle, &lowered) {
                        hits.push((found + 3, 2, ordinal as u64));
                    }
                }
                hits.sort();
                let total = hits.len();
                let start = (*offset).min(total);
                let end = (start + *limit).min(total);
                let mut items = Vec::new();
                for (_, kind, ordinal) in &hits[start..end] {
                    let section = ["nodes", "hyperedges", "links"][*kind as usize];
                    let value = read_records(dir, section, &[*ordinal])?.pop().ok_or("Graphify search record is missing")?;
                    items.push(match *kind {
                        0 => json!({ "kind": "node", "node": value }),
                        1 => json!({ "kind": "hyperedge", "hyperedge": value }),
                        _ => json!({ "kind": "link", "link": value }),
                    });
                }
                page("search", total as u64, *offset, items)
            }
            GraphifyQuery::Neighbors { id, direction, offset, limit } => {
                window(*offset, *limit)?;
                let node = loaded.node_ordinal(id)?;
                let mut matches: Vec<(u64, u64, u64)> = Vec::new();
                let mut ends = BufReader::new(File::open(dir.join("links.ends")).map_err(|e| err("Cannot read Graphify snapshot", e))?);
                let mut record = [0u8; 24];
                let mut ordinal = 0u64;
                loop {
                    match ends.read_exact(&mut record) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                        Err(e) => return Err(err("Cannot read Graphify snapshot", e)),
                    }
                    let word = |i: usize| u64::from_le_bytes(record[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
                    let (source, target, flags) = (word(0), word(1), word(2));
                    let directed = flags & 1 == 1;
                    let wanted = match direction {
                        GraphifyDirection::Both => source == node || target == node,
                        GraphifyDirection::Out => source == node || (!directed && target == node),
                        GraphifyDirection::In => target == node || (!directed && source == node),
                    };
                    if wanted {
                        matches.push((ordinal, source, target));
                    }
                    ordinal += 1;
                }
                let total = matches.len();
                let start = (*offset).min(total);
                let end = (start + *limit).min(total);
                let chosen = &matches[start..end];
                let mut links = read_records(dir, "links", &chosen.iter().map(|m| m.0).collect::<Vec<_>>())?;
                let mut wanted_nodes: Vec<u64> = chosen.iter().flat_map(|m| [m.1, m.2]).filter(|n| *n != NO_NODE).collect();
                wanted_nodes.sort_unstable();
                wanted_nodes.dedup();
                let nodes: HashMap<u64, Value> = wanted_nodes.iter().copied().zip(read_records(dir, "nodes", &wanted_nodes)?).collect();
                for (link, (_, source, target)) in links.iter_mut().zip(chosen) {
                    if let Value::Object(object) = link {
                        object.insert("sourceNode".into(), nodes.get(source).cloned().unwrap_or(Value::Null));
                        object.insert("targetNode".into(), nodes.get(target).cloned().unwrap_or(Value::Null));
                    }
                }
                page("neighbors", total as u64, *offset, links)
            }
            GraphifyQuery::CommunityMembers { community, offset, limit } => {
                window(*offset, *limit)?;
                #[derive(Deserialize)]
                struct Line {
                    community: Option<i64>,
                }
                let mut members = Vec::new();
                for (ordinal, line) in lines(&dir.join("nodes.jsonl"))?.enumerate() {
                    let parsed: Line = serde_json::from_str(&line.map_err(|e| err("Cannot read Graphify snapshot", e))?).map_err(|e| err("Corrupt Graphify snapshot", e))?;
                    if parsed.community == Some(*community) {
                        members.push(ordinal as u64);
                    }
                }
                page_of("communityMembers", "nodes", &members, *offset, *limit)
            }
            GraphifyQuery::HyperedgesOf { id, offset, limit } => {
                window(*offset, *limit)?;
                loaded.node_ordinal(id)?;
                #[derive(Deserialize)]
                struct Line {
                    #[serde(rename = "nodeIds")]
                    node_ids: Vec<GraphifyRef>,
                }
                let mut found = Vec::new();
                for (ordinal, line) in lines(&dir.join("hyperedges.jsonl"))?.enumerate() {
                    let parsed: Line = serde_json::from_str(&line.map_err(|e| err("Cannot read Graphify snapshot", e))?).map_err(|e| err("Corrupt Graphify snapshot", e))?;
                    if parsed.node_ids.contains(id) {
                        found.push(ordinal as u64);
                    }
                }
                page_of("hyperedgesOf", "hyperedges", &found, *offset, *limit)
            }
        }
    }
}
