use crate::dependencies::{self, DependencyGraph};
use crate::instance::Registry;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Clone, Default, Deserialize)]
pub struct Judgement {
    pub inhouse: bool,
    #[serde(default)]
    pub reason: String,
}
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Judgements {
    #[serde(default)]
    pub items: BTreeMap<String, Judgement>,
    pub updated_at: Option<u64>,
}
pub fn read(root: &Path, key: &str) -> Judgements {
    let path = root.join("architect/inhouse").join(format!("{key}.json"));
    fs::metadata(&path)
        .ok()
        .filter(|m| m.len() <= 5 * 1024 * 1024)
        .and_then(|_| fs::read(path).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InhouseStatus {
    pub gate: bool,
    pub complete: bool,
    pub total: usize,
    pub judged: usize,
    pub inhouse: BTreeMap<String, String>,
    pub updated_at: Option<u64>,
}
struct Snapshot {
    ids: BTreeSet<String>,
    stamp: Option<(SystemTime, u64)>,
    judgements: Judgements,
}
pub struct InhouseService {
    root: PathBuf,
    gate: AtomicBool,
    snapshots: Mutex<HashMap<String, Snapshot>>,
}
const BLOCKED: &str =
    "Public lookups are blocked until The Architect has identified in-house packages.";
impl InhouseService {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            gate: AtomicBool::new(false),
            snapshots: Mutex::new(HashMap::new()),
        }
    }
    fn stamp(&self, key: &str) -> Option<(SystemTime, u64)> {
        let m = fs::metadata(
            self.root
                .join("architect/inhouse")
                .join(format!("{key}.json")),
        )
        .ok()?;
        Some((m.modified().ok()?, m.len()))
    }
    pub fn status(
        &self,
        registry: &Registry,
        folder: &Path,
        rescan: bool,
        gate: bool,
    ) -> Result<InhouseStatus, String> {
        self.gate.store(gate, Ordering::Release);
        if !folder.is_dir() {
            return Err(format!("Not a folder: {}", folder.display()));
        }
        let key = dependencies::folder_key(folder);
        let mut snapshots = self.snapshots.lock().map_err(|e| e.to_string())?;
        if rescan || !snapshots.contains_key(&key) {
            let ids = match dependencies::scan(folder, &registry.home, /* transitive */ true) {
                Ok(graph) => graph
                    .nodes
                    .into_iter()
                    .filter(|n| n.id != "project")
                    .map(|n| n.id)
                    .collect(),
                // A real folder without a supported manifest has no package names to judge.
                Err(error) if error.starts_with("No dependency manifest found in ") => {
                    BTreeSet::new()
                }
                Err(error) => return Err(error),
            };
            snapshots.insert(
                key.clone(),
                Snapshot {
                    ids,
                    stamp: None,
                    judgements: Judgements::default(),
                },
            );
        }
        let snapshot = snapshots
            .get_mut(&key)
            .expect("the snapshot was retained or inserted above");
        snapshot.judgements = read(&self.root, &key);
        snapshot.stamp = self.stamp(&key);
        let judged = snapshot
            .ids
            .iter()
            .filter(|id| snapshot.judgements.items.contains_key(*id))
            .count();
        Ok(InhouseStatus {
            gate,
            complete: judged == snapshot.ids.len(),
            total: snapshot.ids.len(),
            judged,
            inhouse: snapshot
                .judgements
                .items
                .iter()
                .filter(|(id, j)| j.inhouse && snapshot.ids.contains(*id))
                .map(|(id, j)| (id.clone(), j.reason.clone()))
                .collect(),
            updated_at: snapshot.judgements.updated_at,
        })
    }
    pub fn allow(&self, folder: &Path, id: &str) -> Result<(), String> {
        if !self.gate.load(Ordering::Acquire) {
            return Ok(());
        }
        let key = dependencies::folder_key(folder);
        let mut snapshots = self.snapshots.lock().map_err(|e| e.to_string())?;
        let snapshot = snapshots.get_mut(&key).ok_or(BLOCKED)?;
        let stamp = self.stamp(&key);
        if stamp != snapshot.stamp {
            snapshot.judgements = read(&self.root, &key);
            snapshot.stamp = stamp;
        }
        if !snapshot
            .ids
            .iter()
            .all(|id| snapshot.judgements.items.contains_key(id))
        {
            return Err(BLOCKED.into());
        }
        if !snapshot.ids.contains(id) {
            return Err("Package is not part of the scanned project.".into());
        }
        if snapshot.judgements.items[id].inhouse {
            return Err(format!("{id} is marked in-house and is never looked up."));
        }
        Ok(())
    }
    /// The saved judgements of a project key, read from disk without touching the gate or any snapshot.
    pub fn judgements(&self, key: &str) -> Judgements {
        read(&self.root, key)
    }
    /// Admission policy for FRESH analysis: refreshes this project's snapshot from the already-scanned graph, so a package
    /// added since the last sweep cannot slip through an old complete snapshot. The gate setting is retained, never changed.
    pub fn review_policy(&self, graph: &DependencyGraph) -> Result<InhouseStatus, String> {
        let ids: BTreeSet<String> = graph
            .nodes
            .iter()
            .filter(|n| n.id != "project")
            .map(|n| n.id.clone())
            .collect();
        let mut snapshots = self.snapshots.lock().map_err(|e| e.to_string())?;
        let judgements = read(&self.root, &graph.key);
        let stamp = self.stamp(&graph.key);
        let judged = ids
            .iter()
            .filter(|id| judgements.items.contains_key(*id))
            .count();
        let status = InhouseStatus {
            gate: self.gate.load(Ordering::Acquire),
            complete: judged == ids.len(),
            total: ids.len(),
            judged,
            inhouse: judgements
                .items
                .iter()
                .filter(|(id, j)| j.inhouse && ids.contains(*id))
                .map(|(id, j)| (id.clone(), j.reason.clone()))
                .collect(),
            updated_at: judgements.updated_at,
        };
        snapshots.insert(
            graph.key.clone(),
            Snapshot {
                ids,
                stamp,
                judgements,
            },
        );
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn status_and_allow_are_fail_closed_and_notice_rewritten_judgements() {
        let root = std::env::temp_dir().join(format!("omp-inhouse-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let registry = Arc::new(Registry::load(root.join("instances.json"), root.clone()).unwrap());
        let service = InhouseService::new(root.clone());
        assert!(service.allow(&root, "anything").is_ok());
        service.gate.store(true, Ordering::Release);
        assert!(service
            .allow(&root, "npm:a")
            .unwrap_err()
            .contains("blocked until"));
        fs::write(
            root.join("package.json"),
            r#"{"dependencies":{"a":"1","b":"1"}}"#,
        )
        .unwrap();
        let status = service.status(&registry, &root, true, true).unwrap();
        assert_eq!(
            (status.total, status.judged, status.complete),
            (2, 0, false)
        );
        let file = root
            .join("architect/inhouse")
            .join(format!("{}.json", dependencies::folder_key(&root)));
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            r#"{"items":{"npm:a":{"inhouse":false},"npm:outside":{"inhouse":false}}}"#,
        )
        .unwrap();
        assert!(service.allow(&root, "npm:a").is_err());
        assert_eq!(
            service
                .status(&registry, &root, false, true)
                .unwrap()
                .judged,
            1
        );
        fs::write(&file, r#"{"items":{"npm:a":{"inhouse":false},"npm:b":{"inhouse":true,"reason":"private company"}},"updatedAt":1}"#).unwrap();
        assert!(service.allow(&root, "npm:a").is_ok());
        assert!(service
            .allow(&root, "npm:b")
            .unwrap_err()
            .contains("marked in-house"));
        assert!(service
            .allow(&root, "npm:outside")
            .unwrap_err()
            .contains("not part"));
        fs::write(&file, r#"{"items":{"npm:a":{"inhouse":true,"reason":"now private with a longer reason"},"npm:b":{"inhouse":false}}}"#).unwrap();
        assert!(service.allow(&root, "npm:a").is_err());
        assert!(service.allow(&root, "npm:b").is_ok());
        service.status(&registry, &root, false, false).unwrap();
        assert!(service.allow(&root, "npm:a").is_ok());
        assert!(service
            .status(&registry, &root.join("missing"), true, true)
            .is_err());
        let empty = root.join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(
            service
                .status(&registry, &empty, true, true)
                .unwrap()
                .complete
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn review_policy_refreshes_the_snapshot_from_the_scanned_graph_and_keeps_the_gate() {
        let root =
            std::env::temp_dir().join(format!("omp-inhouse-policy-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let registry = Arc::new(Registry::load(root.join("instances.json"), root.clone()).unwrap());
        let service = InhouseService::new(root.clone());
        fs::write(root.join("package.json"), r#"{"dependencies":{"a":"1"}}"#).unwrap();
        let file = root
            .join("architect/inhouse")
            .join(format!("{}.json", dependencies::folder_key(&root)));
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, r#"{"items":{"npm:a":{"inhouse":false}}}"#).unwrap();
        service.status(&registry, &root, true, true).unwrap();
        assert!(service.allow(&root, "npm:a").is_ok());
        // A package added after the complete sweep: the policy refresh makes it unjudged, and allow() fails closed.
        fs::write(
            root.join("package.json"),
            r#"{"dependencies":{"a":"1","z":"1"}}"#,
        )
        .unwrap();
        let graph = dependencies::scan(&root, &registry.home, true).unwrap();
        let policy = service.review_policy(&graph).unwrap();
        assert!(policy.gate && !policy.complete && (policy.total, policy.judged) == (2, 1));
        assert!(service
            .allow(&root, "npm:a")
            .unwrap_err()
            .contains("blocked until"));
        fs::write(
            &file,
            r#"{"items":{"npm:a":{"inhouse":false},"npm:z":{"inhouse":true,"reason":"private"}}}"#,
        )
        .unwrap();
        let policy = service.review_policy(&graph).unwrap();
        assert!(policy.complete && policy.inhouse["npm:z"] == "private");
        assert!(service.judgements(&graph.key).items.contains_key("npm:z"));
        fs::remove_dir_all(root).unwrap();
    }
}
