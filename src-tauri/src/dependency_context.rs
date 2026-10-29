//! Immutable, project-bound snapshots of the complete scanned dependency graph for The Architect. A snapshot is a
//! directory `<config>/architect/dependency-context/<sha256>/` holding `summary.json`, `packages.jsonl` and
//! `edges.jsonl`. Publishing never resolves tokens, runs `gh` or issues HTTP: saved metadata is read from the cache only.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::architect::Classification;
use crate::dependencies::{DepNode, DependencyGraph};
use crate::inhouse::{InhouseService, Judgements};
use crate::instance::Registry;
use crate::package_insights::{PackageInsights, PackageInsightsService};
use crate::{architect, known_standard};

pub type Result<T, E = String> = std::result::Result<T, E>;
const MAX_SUMMARY_BYTES: u64 = 1024 * 1024;
const ROOT_ID: &str = "project";
pub const TOOL_NAME: &str = "inspect_dependency_context";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DependencyContextSummary {
    pub id: String,
    pub content_hash: String,
    pub folder: String,
    pub folder_key: String,
    pub captured_at: u64,
    /// Scanned packages, excluding the synthetic project root.
    pub package_count: usize,
    pub edge_count: usize,
    pub direct_count: usize,
    pub root_id: String,
    pub notes: Vec<String>,
    pub truncated: bool,
    pub packages_bytes: u64,
    pub edges_bytes: u64,
}

#[derive(Serialize)]
struct ProjectJudgement<'judgement> {
    basic: bool,
    reason: &'judgement str,
}
#[derive(Serialize)]
struct StandardJudgement<'judgement> {
    reason: &'judgement str,
}
#[derive(Serialize)]
struct InhouseJudgement<'judgement> {
    inhouse: bool,
    reason: &'judgement str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PackageRecord<'graph> {
    id: &'graph str,
    name: &'graph str,
    version: Option<&'graph str>,
    resolved_versions: &'graph [String],
    declared_versions: &'graph [String],
    ecosystem: &'graph str,
    direct: bool,
    dev: bool,
    /// The project's own Architect classification, or null when it never judged this package.
    project_judgement: Option<ProjectJudgement<'graph>>,
    /// The globally remembered standard (built-in) judgement, or null.
    remembered_standard: Option<StandardJudgement<'graph>>,
    /// The in-house privacy judgement, or null when unknown.
    inhouse: Option<InhouseJudgement<'graph>>,
    /// Saved public metadata only; null when none was ever saved.
    insights: Option<PackageInsights>,
}

pub struct DependencyContextService {
    root: PathBuf,
}

/// Writes through to a file while hashing exactly the bytes written.
struct HashWriter {
    inner: BufWriter<File>,
    hash: Sha256,
    bytes: u64,
}

impl HashWriter {
    fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            inner: BufWriter::new(file),
            hash: Sha256::new(),
            bytes: 0,
        })
    }
    fn finish(mut self) -> Result<([u8; 32], u64)> {
        self.inner.flush().map_err(|error| error.to_string())?;
        self.inner
            .get_ref()
            .sync_all()
            .map_err(|error| error.to_string())?;
        Ok((self.hash.finalize().into(), self.bytes))
    }
}

impl Write for HashWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.hash.update(&buffer[..written]);
        self.bytes += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn is_root(node: &DepNode) -> bool {
    node.id == ROOT_ID || node.ecosystem == "project"
}

/// A context id is the lower-case hex SHA-256 of its content; anything else is never joined to a path.
#[must_use]
pub fn valid_context_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl DependencyContextService {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root: root.join("architect").join("dependency-context"),
        }
    }

    pub fn directory(&self, id: &str) -> Result<PathBuf> {
        if !valid_context_id(id) {
            return Err("Invalid dependency context id".into());
        }
        Ok(self.root.join(id))
    }

    /// The file the Architect bridge reads to find the context of the newest scan.
    fn active_pointer(&self) -> PathBuf {
        self.root
            .parent()
            .expect("context root has a parent")
            .join("active-dependency-context.json")
    }

    /// Reads a complete snapshot's summary; a missing, partial or mismatched directory is an error.
    pub fn summary(&self, id: &str) -> Result<DependencyContextSummary> {
        let directory = self.directory(id)?;
        let summary: DependencyContextSummary = read_summary(&directory)?;
        if summary.id != id || summary.content_hash != id {
            return Err("Dependency context summary does not match its id".into());
        }
        for (name, expected) in [
            ("packages.jsonl", summary.packages_bytes),
            ("edges.jsonl", summary.edges_bytes),
        ] {
            let meta = fs::symlink_metadata(directory.join(name))
                .map_err(|error| format!("Dependency context is incomplete: {error}"))?;
            if !meta.is_file() || meta.file_type().is_symlink() || meta.len() != expected {
                return Err("Dependency context is incomplete".into());
            }
        }
        Ok(summary)
    }

    /// Publish the full scanned graph. Saved metadata comes from `PackageInsightsService::cached_only`; nothing is fetched.
    /// The user's in-house gate setting is read, never changed.
    pub fn publish(
        &self,
        registry: &Registry,
        insights: &PackageInsightsService,
        inhouse: &InhouseService,
        graph: &DependencyGraph,
    ) -> Result<DependencyContextSummary> {
        let config_root = registry.config_root();
        let classification = architect::read_classification(config_root, &graph.key);
        let standard = known_standard::read(config_root);
        let judgements = inhouse.judgements(&graph.key);
        fs::create_dir_all(&self.root)
            .map_err(|error| format!("Cannot create the dependency context folder: {error}"))?;
        let staging = self.root.join(format!(".staging-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&staging)
            .map_err(|error| format!("Cannot stage the dependency context: {error}"))?;
        let outcome = self.write_staging(
            &staging,
            graph,
            &classification,
            &standard.items,
            &judgements,
            insights,
        );
        let summary = match outcome {
            Ok(summary) => summary,
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };
        let target = self.root.join(&summary.id);
        if let Ok(existing) = self.summary(&summary.id) {
            // Identical project-bound content already exists and is complete: keep its immutable bytes and capture time.
            let _ = fs::remove_dir_all(&staging);
            return Ok(existing);
        }
        if fs::symlink_metadata(&target).is_ok() {
            // A directory without a valid complete snapshot was never addressable as complete; replace it.
            fs::remove_dir_all(&target).map_err(|error| {
                format!("Cannot replace an incomplete dependency context: {error}")
            })?;
        }
        if let Err(error) = fs::rename(&staging, &target) {
            let _ = fs::remove_dir_all(&staging);
            // A concurrent publisher of the same content may have won the rename.
            return self
                .summary(&summary.id)
                .map_err(|_| format!("Cannot publish the dependency context: {error}"));
        }
        Ok(summary)
    }

    /// Point the Architect's context tool at `summary` for requests that name no request id.
    pub fn activate(&self, summary: &DependencyContextSummary) -> Result<()> {
        let value = serde_json::json!({ "contextId": summary.id, "folder": summary.folder, "folderKey": summary.folder_key, "updatedAt": now_ms() });
        let bytes = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
        let path = self.active_pointer();
        fs::create_dir_all(path.parent().expect("pointer has a parent"))
            .map_err(|error| error.to_string())?;
        let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &path)
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary);
            return Err(format!(
                "Cannot save the active dependency context: {error}"
            ));
        }
        Ok(())
    }

    fn write_staging(
        &self,
        staging: &Path,
        graph: &DependencyGraph,
        classification: &Classification,
        standard: &BTreeMap<String, String>,
        judgements: &Judgements,
        insights: &PackageInsightsService,
    ) -> Result<DependencyContextSummary> {
        let mut nodes: Vec<&DepNode> = graph.nodes.iter().filter(|node| !is_root(node)).collect();
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        nodes.dedup_by(|a, b| a.id == b.id);
        let mut packages = HashWriter::create(&staging.join("packages.jsonl"))?;
        let mut direct_count = 0;
        for node in &nodes {
            if node.direct {
                direct_count += 1;
            }
            let record = PackageRecord {
                id: &node.id,
                name: &node.name,
                version: node.version.as_deref(),
                resolved_versions: &node.resolved_versions,
                declared_versions: &node.declared_versions,
                ecosystem: node.ecosystem,
                direct: node.direct,
                dev: node.dev,
                project_judgement: classification.items.get(&node.id).map(|item| {
                    ProjectJudgement {
                        basic: item.basic,
                        reason: &item.reason,
                    }
                }),
                remembered_standard: standard
                    .get(&node.id)
                    .map(|reason| StandardJudgement { reason }),
                inhouse: judgements.items.get(&node.id).map(|item| InhouseJudgement {
                    inhouse: item.inhouse,
                    reason: &item.reason,
                }),
                // Absent or unreadable saved metadata stays null: it is stated as a limitation, never fetched here.
                insights: insights.cached_only(&node.id).ok().flatten(),
            };
            serde_json::to_writer(&mut packages, &record).map_err(|error| error.to_string())?;
            packages
                .write_all(b"\n")
                .map_err(|error| error.to_string())?;
        }
        let (packages_hash, packages_bytes) = packages.finish()?;
        let edges: BTreeSet<(&str, &str)> = graph
            .edges
            .iter()
            .map(|(from, to)| (from.as_str(), to.as_str()))
            .collect();
        let mut writer = HashWriter::create(&staging.join("edges.jsonl"))?;
        for (from, to) in &edges {
            serde_json::to_writer(&mut writer, &[from, to]).map_err(|error| error.to_string())?;
            writer.write_all(b"\n").map_err(|error| error.to_string())?;
        }
        let (edges_hash, edges_bytes) = writer.finish()?;
        // Identity covers the project, the semantic summary fields and both canonical record streams; not the capture time.
        let mut head = Sha256::new();
        for part in [graph.folder.as_bytes(), graph.key.as_bytes()] {
            head.update((part.len() as u64).to_le_bytes());
            head.update(part);
        }
        let semantic = serde_json::to_vec(&(&graph.notes, graph.truncated))
            .map_err(|error| error.to_string())?;
        head.update((semantic.len() as u64).to_le_bytes());
        head.update(&semantic);
        head.update(packages_hash);
        head.update(edges_hash);
        let id: String = head
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let summary = DependencyContextSummary {
            content_hash: id.clone(),
            id,
            folder: graph.folder.clone(),
            folder_key: graph.key.clone(),
            captured_at: now_ms(),
            package_count: nodes.len(),
            edge_count: edges.len(),
            direct_count,
            root_id: ROOT_ID.into(),
            notes: graph.notes.clone(),
            truncated: graph.truncated,
            packages_bytes,
            edges_bytes,
        };
        let bytes = serde_json::to_vec(&summary).map_err(|error| error.to_string())?;
        fs::write(staging.join("summary.json"), bytes).map_err(|error| error.to_string())?;
        Ok(summary)
    }

    /// Stream `packages.jsonl` once; used by tests and Rust callers that need a package record without a second copy.
    pub fn for_each_line(
        &self,
        id: &str,
        file: &str,
        mut each: impl FnMut(&str) -> Result<bool>,
    ) -> Result<()> {
        self.summary(id)?;
        let reader = BufReader::new(
            File::open(self.directory(id)?.join(file)).map_err(|error| error.to_string())?,
        );
        for line in reader.lines() {
            if !each(&line.map_err(|error| error.to_string())?)? {
                break;
            }
        }
        Ok(())
    }
}

fn read_summary(directory: &Path) -> Result<DependencyContextSummary> {
    let path = directory.join("summary.json");
    let meta = fs::symlink_metadata(&path)
        .map_err(|error| format!("Unknown dependency context: {error}"))?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_SUMMARY_BYTES {
        return Err("Invalid dependency context summary".into());
    }
    let bytes = fs::read(&path).map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("Invalid dependency context summary: {error}"))
}

/// The block every analysis prompt embeds so the Architect can read the complete graph through the app-owned tool.
#[must_use]
pub fn prompt_block(summary: &DependencyContextSummary) -> String {
    format!(
        "Complete dependency context. contextId: {id}. Source folder: {folder}. It holds {packages} scanned packages ({direct} direct) and {edges} edges, captured from the full scan, not from the displayed view.\n\
Read it with the {tool} tool (sections summary, packages, edges, package; pass requestId to resolve this request's context, plus offset/limit to page). Edges read `package -> its dependency`; the synthetic project root links to the direct dependencies. \
Consult every dependency path relevant to a finding, not only the selected package. Packages marked in-house are private identities: never search public registries, advisory databases or the web for them while the privacy gate excludes them. \
Packages with null insights have no saved public metadata; verify the newest release yourself with your configured tools and state it as a limitation when you cannot. Model-provider context is not a filesystem sandbox: this graph and project data are visible to you.",
        id = summary.id, folder = summary.folder, packages = summary.package_count, direct = summary.direct_count, edges = summary.edge_count, tool = TOOL_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let root = std::env::temp_dir().join(format!("omp-context-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn node(id: &str, direct: bool) -> DepNode {
        let (ecosystem, name) = id.split_once(':').unwrap();
        DepNode {
            id: id.into(),
            name: name.into(),
            version: Some("1.0.0".into()),
            resolved_versions: vec!["1.0.0".into(), "2.0.0".into()],
            declared_versions: vec!["^1".into()],
            ecosystem: match ecosystem {
                "npm" => "npm",
                "cargo" => "cargo",
                _ => "project",
            },
            direct,
            dev: false,
        }
    }

    /// A real scanner graph for a fixture folder, with its package/edge/note fields replaced by a known shape:
    /// a hidden depth-3 transitive, an edge cycle, a duplicate edge and a disconnected package.
    pub(crate) fn graph(root: &Path, name: &str) -> DependencyGraph {
        let folder = root.join(name);
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("package.json"),
            r#"{"dependencies":{"a":"^1"}}"#,
        )
        .unwrap();
        let mut graph = crate::dependencies::scan(&folder, root, true).unwrap();
        let project = graph
            .nodes
            .iter()
            .find(|node| node.id == ROOT_ID)
            .unwrap()
            .clone();
        graph.nodes = vec![
            node("npm:b", false),
            project,
            node("npm:a", true),
            node("npm:deep", false),
            node("cargo:iso", false),
        ];
        graph.edges = vec![
            (ROOT_ID.into(), "npm:a".into()),
            ("npm:a".into(), "npm:b".into()),
            ("npm:b".into(), "npm:deep".into()),
            ("npm:deep".into(), "npm:b".into()),
            ("npm:a".into(), "npm:b".into()),
        ];
        graph.notes = vec!["Truncated for the test.".into()];
        graph.truncated = true;
        graph
    }

    fn lines(service: &DependencyContextService, id: &str, file: &str) -> Vec<String> {
        let mut out = Vec::new();
        service
            .for_each_line(id, file, |line| {
                out.push(line.to_string());
                Ok(true)
            })
            .unwrap();
        out
    }

    #[test]
    fn the_snapshot_holds_every_node_and_edge_including_hidden_and_disconnected_ones() {
        let root = temp();
        let registry = Registry::load(root.join("instances.json"), root.clone()).unwrap();
        let insights = PackageInsightsService::try_new(root.clone()).unwrap();
        let inhouse = InhouseService::new(root.clone());
        let service = DependencyContextService::new(root.clone());
        let g = graph(&root, "project");
        let summary = service.publish(&registry, &insights, &inhouse, &g).unwrap();
        assert_eq!(
            (
                summary.package_count,
                summary.edge_count,
                summary.direct_count
            ),
            (4, 4, 1)
        );
        assert!(summary.truncated && summary.notes == vec!["Truncated for the test.".to_string()]);
        let packages = lines(&service, &summary.id, "packages.jsonl");
        let ids: Vec<_> = packages
            .iter()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(ids, ["cargo:iso", "npm:a", "npm:b", "npm:deep"]);
        let first: serde_json::Value = serde_json::from_str(&packages[0]).unwrap();
        assert_eq!(
            first["resolvedVersions"],
            serde_json::json!(["1.0.0", "2.0.0"])
        );
        assert!(
            first["insights"].is_null()
                && first["inhouse"].is_null()
                && first["projectJudgement"].is_null()
        );
        let edges = lines(&service, &summary.id, "edges.jsonl");
        assert_eq!(
            edges,
            [
                r#"["npm:a","npm:b"]"#,
                r#"["npm:b","npm:deep"]"#,
                r#"["npm:deep","npm:b"]"#,
                r#"["project","npm:a"]"#
            ]
        );
        assert_eq!(service.summary(&summary.id).unwrap(), summary);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn identical_content_reuses_bytes_but_different_projects_get_distinct_contexts() {
        let root = temp();
        let registry = Registry::load(root.join("instances.json"), root.clone()).unwrap();
        let insights = PackageInsightsService::try_new(root.clone()).unwrap();
        let inhouse = InhouseService::new(root.clone());
        let service = DependencyContextService::new(root.clone());
        let first = service
            .publish(&registry, &insights, &inhouse, &graph(&root, "one"))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let again = service
            .publish(&registry, &insights, &inhouse, &graph(&root, "one"))
            .unwrap();
        assert_eq!(
            first, again,
            "the existing complete snapshot, including its capture time, is reused"
        );
        let second = service
            .publish(&registry, &insights, &inhouse, &graph(&root, "two"))
            .unwrap();
        assert_ne!(first.id, second.id);
        // Same folder identity, one more package: a new content hash.
        let mut changed = graph(&root, "one");
        changed.nodes.push(node("npm:extra", false));
        assert_ne!(
            service
                .publish(&registry, &insights, &inhouse, &changed)
                .unwrap()
                .id,
            first.id
        );
        let leftovers: Vec<_> = fs::read_dir(root.join("architect/dependency-context"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".staging-"))
            .collect();
        assert!(leftovers.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_partial_directory_is_never_complete_and_ids_are_validated() {
        let root = temp();
        let service = DependencyContextService::new(root.clone());
        let id = "a".repeat(64);
        fs::create_dir_all(service.directory(&id).unwrap()).unwrap();
        assert!(service.summary(&id).is_err());
        for bad in ["", "../x", &"A".repeat(64), &"a".repeat(63)] {
            assert!(service.directory(bad).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn activation_writes_the_pointer_the_bridge_reads() {
        let root = temp();
        let registry = Registry::load(root.join("instances.json"), root.clone()).unwrap();
        let insights = PackageInsightsService::try_new(root.clone()).unwrap();
        let service = DependencyContextService::new(root.clone());
        let summary = service
            .publish(
                &registry,
                &insights,
                &InhouseService::new(root.clone()),
                &graph(&root, "p"),
            )
            .unwrap();
        service.activate(&summary).unwrap();
        let pointer: serde_json::Value = serde_json::from_slice(
            &fs::read(root.join("architect/active-dependency-context.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(pointer["contextId"], summary.id);
        assert!(
            prompt_block(&summary).contains(TOOL_NAME)
                && prompt_block(&summary).contains(&summary.id)
        );
        fs::remove_dir_all(root).unwrap();
    }
}
