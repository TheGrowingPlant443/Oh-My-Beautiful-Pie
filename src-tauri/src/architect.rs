//! The Architect: an ordinary agent that judges which direct dependencies of a project are basic (built in) and which
//! were chosen. The app sends it the packages (found by `dependencies::scan`) and it answers through the bridge tool
//! `classify_dependencies` (bridge/omp-architect.mjs), which saves a JSON file this module reads.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::dependencies;
use crate::instance::{Instance, InstanceKind, Origin, Registry};

pub type Result<T, E = String> = std::result::Result<T, E>;

/// At most this many packages are sent per request; the rest stay unclassified (visible) until asked for again.
const MAX_REQUEST: usize = 600;

/// Every bridge module the Architect extension imports; each is written next to the manager bridge when it changes.
const BRIDGE_MODULES: [(&str, &str); 12] = [
    (
        "omp-architect.mjs",
        include_str!("../../bridge/omp-architect.mjs"),
    ),
    (
        "omp-architect-context.mjs",
        include_str!("../../bridge/omp-architect-context.mjs"),
    ),
    (
        "omp-architect-console.mjs",
        include_str!("../../bridge/omp-architect-console.mjs"),
    ),
    (
        "omp-dependency-updates.mjs",
        include_str!("../../bridge/omp-dependency-updates.mjs"),
    ),
    (
        "omp-project-insight.mjs",
        include_str!("../../bridge/omp-project-insight.mjs"),
    ),
    (
        "omp-insight-base.mjs",
        include_str!("../../bridge/omp-insight-base.mjs"),
    ),
    (
        "omp-source-snapshot.mjs",
        include_str!("../../bridge/omp-source-snapshot.mjs"),
    ),
    (
        "omp-system-results.mjs",
        include_str!("../../bridge/omp-system-results.mjs"),
    ),
    (
        "omp-visualization-guard.mjs",
        include_str!("../../bridge/omp-visualization-guard.mjs"),
    ),
    (
        "omp-structure-sources.mjs",
        include_str!("../../bridge/omp-structure-sources.mjs"),
    ),
    ("omp-srs.mjs", include_str!("../../bridge/omp-srs.mjs")),
    (
        "omp-host-monitor.mjs",
        include_str!("../../bridge/omp-host-monitor.mjs"),
    ),
];

/// The agent imports these modules next to the bridge extension.
pub fn materialize(folder: &Path) -> Result<()> {
    for (name, source) in BRIDGE_MODULES {
        let file = folder.join(name);
        if fs::read_to_string(&file).ok().as_deref() != Some(source) {
            fs::write(file, source).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ClassificationItem {
    pub basic: bool,
    #[serde(default)]
    pub reason: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Classification {
    #[serde(default)]
    pub items: BTreeMap<String, ClassificationItem>,
    #[serde(default)]
    pub updated_at: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifyAdmission {
    pub architect: Option<Instance>,
    pub prompt_sent: Option<bool>,
    pub requested: usize,
    pub error: Option<String>,
}

#[must_use]
pub fn classification_path(config_root: &Path, key: &str) -> PathBuf {
    config_root
        .join("architect")
        .join("classifications")
        .join(format!("{key}.json"))
}

/// A missing or unreadable file is an empty classification: nothing is hidden.
#[must_use]
pub fn read_classification(config_root: &Path, key: &str) -> Classification {
    fs::read(classification_path(config_root, key))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// `packages` are `(id, version, dev)` of the direct dependencies to judge.
#[must_use]
pub fn prompt(folder: &str, key: &str, packages: &[(&str, Option<&str>, bool)]) -> String {
    let mut text = format!(
    "Dependency classification request.\nProject folder: {folder}\nKey: {key}\n\n\
For each direct dependency below decide whether it is BASIC or CHOSEN for this project. \
BASIC = built in / foundational: the framework's own core packages (for example @angular/core, react, spring-boot-starter), \
language or runtime foundations and shims (tslib, typescript, javascript standard-library polyfills), type-definition packages, \
and anything practically every project of this kind has. CHOSEN = a deliberate library choice for this project \
(for example sigma, a chart library, an ORM, a state-management library). You MAY read the project's files to judge. \
Then call the classify_dependencies tool with key \"{key}\" and one item {{id, basic, reason (max 12 words)}} per package, \
using the ids exactly as written. Do not do anything else.\n\nPackages (id, version, dev):\n"
);
    for (id, version, dev) in packages {
        text.push_str(&format!(
            "- {id} {} {}\n",
            version.as_deref().unwrap_or("-"),
            if *dev { "dev" } else { "runtime" }
        ));
    }
    text
}

/// Ask The Architect to classify the project's direct dependencies it has not judged yet.
pub fn classify(registry: &Registry, folder: &Path) -> Result<ClassifyAdmission> {
    let graph = dependencies::scan(folder, &registry.home, /* include_transitive */ false)?;
    let known = read_classification(registry.config_root(), &graph.key).items;
    let standard = crate::known_standard::read(registry.config_root());
    let mut pending = pending_packages(&graph.nodes, &known, &standard.items);
    pending.truncate(MAX_REQUEST);
    let requested = pending.len();
    if requested == 0 {
        return Ok(ClassifyAdmission {
            architect: None,
            prompt_sent: None,
            requested: 0,
            error: None,
        });
    }
    let packages: Vec<(&str, Option<&str>, bool)> = pending
        .iter()
        .map(|node| (node.id.as_str(), node.version.as_deref(), node.dev))
        .collect();
    let message = prompt(&folder.display().to_string(), &graph.key, &packages);
    let (architect, prompt_sent, error) = ask_architect(registry, &message)?;
    Ok(ClassifyAdmission {
        architect: Some(architect),
        prompt_sent,
        requested,
        error,
    })
}

fn pending_packages<'a>(
    nodes: &'a [crate::dependencies::DepNode],
    judged: &BTreeMap<String, ClassificationItem>,
    standard: &BTreeMap<String, String>,
) -> Vec<&'a crate::dependencies::DepNode> {
    let mut pending: Vec<_> = nodes
        .iter()
        .filter(|n| n.direct && !judged.contains_key(&n.id) && !standard.contains_key(&n.id))
        .collect();
    pending.sort_by(|a, b| a.id.cmp(&b.id));
    pending
}

fn ask_architect(
    registry: &Registry,
    message: &str,
) -> Result<(Instance, Option<bool>, Option<String>)> {
    let row = registry
        .snapshot()
        .into_iter()
        .find(|row| row.kind == InstanceKind::Architect)
        .ok_or("Create The Architect first (toolbar → The Architect).")?;
    if row.status != "running" && row.status != "starting" {
        registry.start(&row.id, Origin::Human)?;
    }
    let hub = registry
        .manager_hub()
        .ok_or("manager bridge not initialized")?;
    let architect = registry.inspect(&row.id, Origin::Human)?;
    if let Err(error) = hub.wait_ready(&row.id) {
        return Ok((architect, Some(false), Some(error)));
    }
    Ok(match hub.prompt_human(&row.id, message) {
        Ok(_) => (architect, Some(true), None),
        Err(error) => (architect, None, Some(format!("Prompt admission outcome unknown: {error}. Inspect The Architect's conversation before sending anything else."))),
    })
}

/// The classification of a folder, by the same key the scan reports.
pub fn classification_for(registry: &Registry, folder: &Path) -> Result<Classification> {
    if !folder.is_dir() {
        return Err(format!("Not a folder: {}", folder.display()));
    }
    Ok(read_classification(
        registry.config_root(),
        &dependencies::folder_key(folder),
    ))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InhouseAdmission {
    pub architect: Option<Instance>,
    pub prompt_sent: Option<bool>,
    pub requested: usize,
    pub remaining: usize,
    pub error: Option<String>,
}
const MAX_INHOUSE_REQUEST: usize = 500;
fn pending_chunk(ids: &[String], judged: &BTreeSet<String>, max: usize) -> (Vec<String>, usize) {
    let mut pending: Vec<_> = ids
        .iter()
        .filter(|id| !judged.contains(*id))
        .cloned()
        .collect();
    pending.sort();
    pending.dedup();
    let remaining = pending.len().saturating_sub(max);
    pending.truncate(max);
    (pending, remaining)
}
fn inhouse_prompt(folder: &str, key: &str, ids: &[(String, bool)]) -> String {
    let mut text = format!("In-house package classification request.\nProject folder: {folder}\nKey: {key}\n\
IN-HOUSE = company-internal packages: internal/private registry scopes or groups, workspace/path/file/link dependencies and monorepo members, names that contain a company, product, customer or project name or could otherwise reveal confidential information, packages served from private registries configured in .npmrc/pip.conf/Cargo registries/Maven repositories.\n\
PUBLIC = general-purpose libraries published on npm, PyPI, crates.io or Maven Central.\n\
When unsure, answer inhouse: true. You MAY read the project's files (read-only) to decide. Do NOT search the web, package registries or any online service for these names.\n\
Call classify_inhouse_packages with key \"{key}\" and one item {{id, inhouse, reason (max 12 words)}} per package with ids exactly as written. You may use any number of tool calls. Do not do anything else.\n");
    for (id, direct) in ids {
        text.push_str(&format!(
            "- {id} {}\n",
            if *direct { "direct" } else { "transitive" }
        ));
    }
    text
}
pub fn inhouse_sweep(registry: &Registry, folder: &Path) -> Result<InhouseAdmission> {
    let graph = dependencies::scan(folder, &registry.home, /* include_transitive */ true)?;
    let judged = crate::inhouse::read(registry.config_root(), &graph.key)
        .items
        .into_keys()
        .collect();
    let ids = graph
        .nodes
        .iter()
        .filter(|n| n.id != "project")
        .map(|n| n.id.clone())
        .collect::<Vec<_>>();
    let (chunk, remaining) = pending_chunk(&ids, &judged, MAX_INHOUSE_REQUEST);
    let requested = chunk.len();
    if requested == 0 {
        return Ok(InhouseAdmission {
            architect: None,
            prompt_sent: None,
            requested,
            remaining,
            error: None,
        });
    }
    let direct: BTreeSet<_> = graph
        .nodes
        .iter()
        .filter(|n| n.direct)
        .map(|n| n.id.as_str())
        .collect();
    let packages = chunk
        .into_iter()
        .map(|id| {
            let is_direct = direct.contains(id.as_str());
            (id, is_direct)
        })
        .collect::<Vec<_>>();
    let (architect, prompt_sent, error) = ask_architect(
        registry,
        &inhouse_prompt(&folder.display().to_string(), &graph.key, &packages),
    )?;
    Ok(InhouseAdmission {
        architect: Some(architect),
        prompt_sent,
        requested,
        remaining,
        error,
    })
}
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectKind {
    pub kind: Option<String>,
    #[serde(default)]
    pub reason: String,
    pub updated_at: Option<u64>,
}
const KINDS: [&str; 5] = ["backend", "frontend", "fullstack", "library", "other"];
fn project_kind_path(root: &Path, key: &str) -> PathBuf {
    root.join("architect/project-kind")
        .join(format!("{key}.json"))
}
#[must_use]
pub fn read_project_kind(root: &Path, key: &str) -> ProjectKind {
    let path = project_kind_path(root, key);
    let saved: ProjectKind = fs::metadata(&path)
        .ok()
        .filter(|m| m.len() <= 5 * 1024 * 1024)
        .and_then(|_| fs::read(path).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if saved.kind.as_deref().is_some_and(|k| KINDS.contains(&k)) {
        saved
    } else {
        ProjectKind::default()
    }
}
pub fn project_kind_for(registry: &Registry, folder: &Path) -> Result<ProjectKind> {
    if !folder.is_dir() {
        return Err(format!("Not a folder: {}", folder.display()));
    }
    Ok(read_project_kind(
        registry.config_root(),
        &dependencies::folder_key(folder),
    ))
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KindAdmission {
    pub architect: Option<Instance>,
    pub prompt_sent: Option<bool>,
    pub error: Option<String>,
}
fn kind_prompt(folder: &str, key: &str) -> String {
    format!("Project kind request.\nProject folder: {folder}\nKey: {key}\n\
backend = server-side service, API, batch job or command-line tool without a user interface; frontend = browser, mobile or desktop user-interface application; fullstack = both in this repository; library = reusable package or SDK; other = none of these.\n\
You MAY read the project's files (read-only) to decide. Do NOT search the web or any online service.\n\
Call classify_project_kind exactly once with the key \"{key}\", kind (backend|frontend|fullstack|library|other) and reason (max 12 words). Do not do anything else.")
}
pub fn ask_project_kind(registry: &Registry, folder: &Path) -> Result<KindAdmission> {
    if !folder.is_dir() {
        return Err(format!("Not a folder: {}", folder.display()));
    }
    let (architect, prompt_sent, error) = ask_architect(
        registry,
        &kind_prompt(
            &folder.display().to_string(),
            &dependencies::folder_key(folder),
        ),
    )?;
    Ok(KindAdmission {
        architect: Some(architect),
        prompt_sent,
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn temp() -> PathBuf {
        let folder = std::env::temp_dir().join(format!("omp-architect-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[test]
    fn a_missing_or_corrupt_classification_is_empty_and_the_saved_shape_is_read() {
        let root = temp();
        let key = "0123456789abcdef";
        assert_eq!(read_classification(&root, key), Classification::default());
        let path = classification_path(&root, key);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();
        assert_eq!(read_classification(&root, key), Classification::default());
        fs::write(&path, r#"{"items":{"npm:a":{"basic":true,"reason":"core"},"npm:b":{"basic":false}},"updatedAt":1760000000000}"#).unwrap();
        let read = read_classification(&root, key);
        assert_eq!(read.updated_at, Some(1760000000000));
        assert_eq!(
            read.items["npm:a"],
            ClassificationItem {
                basic: true,
                reason: "core".into()
            }
        );
        assert_eq!(
            read.items["npm:b"],
            ClassificationItem {
                basic: false,
                reason: String::new()
            }
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_request_names_the_key_the_tool_and_every_package() {
        let text = prompt(
            "C:/p",
            "0123456789abcdef",
            &[("npm:a", Some("1.0.0"), false), ("npm:b", None, true)],
        );
        assert!(text.contains("0123456789abcdef"));
        assert!(text.contains("classify_dependencies"));
        assert!(text.contains("- npm:a 1.0.0 runtime"));
        assert!(text.contains("- npm:b - dev"));
    }

    #[test]
    fn chunks_and_prompts_are_sorted_and_local() {
        let ids = (0..1200)
            .rev()
            .map(|n| format!("npm:p{n:04}"))
            .collect::<Vec<_>>();
        let (chunk, remaining) = pending_chunk(&ids, &BTreeSet::new(), 500);
        assert_eq!((chunk.len(), remaining), (500, 700));
        assert_eq!(chunk[0], "npm:p0000");
        assert_eq!(
            pending_chunk(&ids, &BTreeSet::from([chunk[0].clone()]), 500).0[0],
            "npm:p0001"
        );
        let text = inhouse_prompt(
            "C:/project",
            "key123",
            &[("npm:a".into(), true), ("cargo:b".into(), false)],
        );
        for needle in [
            "key123",
            "classify_inhouse_packages",
            "When unsure, answer inhouse: true",
            "Do NOT search the web",
            "- npm:a direct",
            "- cargo:b transitive",
        ] {
            assert!(text.contains(needle));
        }
        assert_eq!(text.matches("npm:a").count(), 1);
        let text = kind_prompt("C:/project", "key123");
        for needle in [
            "key123",
            "classify_project_kind",
            "backend",
            "frontend",
            "fullstack",
            "library",
            "other",
            "Do NOT search the web or any online service",
            "Do not do anything else.",
        ] {
            assert!(text.contains(needle));
        }
    }
    #[test]
    fn kind_storage_and_missing_architect_admission() {
        let root = temp();
        assert_eq!(read_project_kind(&root, "key"), ProjectKind::default());
        let path = project_kind_path(&root, "key");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for bad in ["{bad", r#"{"kind":"unknown"}"#] {
            fs::write(&path, bad).unwrap();
            assert_eq!(read_project_kind(&root, "key"), ProjectKind::default());
        }
        fs::write(
            &path,
            r#"{"kind":"frontend","reason":"desktop","updatedAt":123}"#,
        )
        .unwrap();
        assert_eq!(
            read_project_kind(&root, "key").kind.as_deref(),
            Some("frontend")
        );
        assert_eq!(read_project_kind(&root, "key").updated_at, Some(123));
        fs::write(&path, vec![b' '; 5 * 1024 * 1024 + 1]).unwrap();
        assert_eq!(read_project_kind(&root, "key"), ProjectKind::default());
        let registry = Arc::new(Registry::load(root.join("instances.json"), root.clone()).unwrap());
        assert!(ask_project_kind(&registry, &root)
            .err()
            .unwrap()
            .contains("Create The Architect first"));
        assert!(project_kind_for(&registry, &root.join("missing")).is_err());
        fs::write(root.join("package.json"), r#"{"dependencies":{}}"#).unwrap();
        assert_eq!(inhouse_sweep(&registry, &root).unwrap().requested, 0);
        fs::write(
            root.join("package.json"),
            r#"{"dependencies":{"a":"1","z":"1","b":"1"}}"#,
        )
        .unwrap();
        assert!(inhouse_sweep(&registry, &root)
            .err()
            .unwrap()
            .contains("Create The Architect first"));
        let graph =
            dependencies::scan(&root, &registry.home, /* include_transitive */ false).unwrap();
        let judged = BTreeMap::from([("npm:a".into(), ClassificationItem::default())]);
        let standard = BTreeMap::from([("npm:z".into(), "core".into())]);
        let pending = pending_packages(&graph.nodes, &judged, &standard);
        assert_eq!(
            pending.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            vec!["npm:b"]
        );
        let judgement_file = root
            .join("architect/inhouse")
            .join(format!("{}.json", graph.key));
        fs::create_dir_all(judgement_file.parent().unwrap()).unwrap();
        fs::write(judgement_file, r#"{"items":{"npm:a":{"inhouse":false},"npm:b":{"inhouse":true},"npm:z":{"inhouse":false}}}"#).unwrap();
        assert_eq!(inhouse_sweep(&registry, &root).unwrap().requested, 0);
        fs::remove_dir_all(root).unwrap();
    }
}
