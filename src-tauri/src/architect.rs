//! The Architect: an ordinary agent that judges which direct dependencies of a project are basic (built in) and which
//! were chosen. The app sends it the packages (found by `dependencies::scan`) and it answers through the bridge tool
//! `classify_dependencies` (bridge/omp-architect.mjs), which saves a JSON file this module reads.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::dependencies::{folder_key, scan};
use crate::instance::{Instance, InstanceKind, Origin, Registry};

pub type Result<T, E = String> = std::result::Result<T, E>;

/// At most this many packages are sent per request; the rest stay unclassified (visible) until asked for again.
const MAX_REQUEST: usize = 600;

/// The agent imports this module next to the bridge extension.
pub fn materialize(folder: &Path) -> Result<()> {
    let file = folder.join("omp-architect.mjs");
    let source = include_str!("../../bridge/omp-architect.mjs");
    if fs::read_to_string(&file).ok().as_deref() != Some(source) {
        fs::write(file, source).map_err(|e| e.to_string())?;
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

pub fn classification_path(config_root: &Path, key: &str) -> PathBuf {
    config_root.join("architect").join("classifications").join(format!("{key}.json"))
}

/// A missing or unreadable file is an empty classification: nothing is hidden.
pub fn read_classification(config_root: &Path, key: &str) -> Classification {
    fs::read(classification_path(config_root, key))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// `packages` are `(id, version, dev)` of the direct dependencies to judge.
pub fn prompt(folder: &str, key: &str, packages: &[(String, Option<String>, bool)]) -> String {
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
        text.push_str(&format!("- {id} {} {}\n", version.as_deref().unwrap_or("-"), if *dev { "dev" } else { "runtime" }));
    }
    text
}

/// Ask The Architect to classify the project's direct dependencies it has not judged yet.
pub fn classify(registry: &Arc<Registry>, folder: &Path) -> Result<ClassifyAdmission> {
    let graph = scan(folder, &registry.home, false)?;
    let known = read_classification(registry.config_root(), &graph.key).items;
    let mut pending: Vec<&crate::dependencies::DepNode> = graph.nodes.iter().filter(|node| node.direct && !known.contains_key(&node.id)).collect();
    pending.sort_by(|a, b| a.id.cmp(&b.id));
    pending.truncate(MAX_REQUEST);
    let requested = pending.len();
    if requested == 0 {
        return Ok(ClassifyAdmission { architect: None, prompt_sent: None, requested: 0, error: None });
    }
    let packages: Vec<(String, Option<String>, bool)> = pending.iter().map(|node| (node.id.clone(), node.version.clone(), node.dev)).collect();
    let row = registry
        .snapshot()
        .into_iter()
        .find(|row| row.kind == InstanceKind::Architect)
        .ok_or("Create The Architect first (toolbar → The Architect).")?;
    if row.status != "running" && row.status != "starting" {
        registry.start(&row.id, Origin::Human)?;
    }
    let hub = registry.manager_hub().ok_or("manager bridge not initialized")?;
    let architect = registry.inspect(&row.id, Origin::Human)?;
    if let Err(error) = hub.wait_ready(&row.id) {
        return Ok(ClassifyAdmission { architect: Some(architect), prompt_sent: Some(false), requested, error: Some(error) });
    }
    let message = prompt(&folder.display().to_string(), &graph.key, &packages);
    Ok(match hub.prompt_human(&row.id, &message) {
        Ok(_) => ClassifyAdmission { architect: Some(architect), prompt_sent: Some(true), requested, error: None },
        Err(error) => ClassifyAdmission {
            architect: Some(architect),
            prompt_sent: None,
            requested,
            error: Some(format!("Prompt admission outcome unknown: {error}. Inspect The Architect's conversation before sending anything else.")),
        },
    })
}

/// The classification of a folder, by the same key the scan reports.
pub fn classification_for(registry: &Registry, folder: &Path) -> Result<Classification> {
    if !folder.is_dir() {
        return Err(format!("Not a folder: {}", folder.display()));
    }
    Ok(read_classification(registry.config_root(), &folder_key(folder)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(read.items["npm:a"], ClassificationItem { basic: true, reason: "core".into() });
        assert_eq!(read.items["npm:b"], ClassificationItem { basic: false, reason: String::new() });
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_request_names_the_key_the_tool_and_every_package() {
        let text = prompt("C:/p", "0123456789abcdef", &[("npm:a".into(), Some("1.0.0".into()), false), ("npm:b".into(), None, true)]);
        assert!(text.contains("0123456789abcdef"));
        assert!(text.contains("classify_dependencies"));
        assert!(text.contains("- npm:a 1.0.0 runtime"));
        assert!(text.contains("- npm:b - dev"));
    }
}
