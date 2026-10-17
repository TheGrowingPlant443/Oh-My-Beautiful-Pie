//! Built-in transitive packages: the transitive packages of Architect-classified built-in direct packages are removed
//! from the dependency view, version by version. A copy of a package stays visible when a non-built-in package uses a
//! different, locally proven version of it. `derive` and `closure` are pure functions over the scanner's version-aware
//! links; `resolve` is the only entry that touches disk (scan, classification and the learned memory).
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;

use serde::Serialize;

use crate::dependencies::{DependencyGraph, Occurrence};

/// What is known about the versions a built-in package pulls in: proven versions and whether any copy had none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryEntry {
    pub versions: BTreeSet<String>,
    pub unversioned: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DerivedPackage {
    pub id: String,
    pub versions: Vec<String>,
    pub standard_versions: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DerivedStandard {
    pub complete: bool,
    pub hide: Vec<String>,
    pub different: Vec<DerivedPackage>,
    pub unproven: Vec<DerivedPackage>,
    pub learned: usize,
}

const ROOT: &str = "project";

struct Links<'a> {
    from: HashMap<&'a Occurrence, Vec<&'a Occurrence>>,
    root: HashMap<&'a str, Vec<&'a Occurrence>>,
    edges: HashMap<&'a str, Vec<&'a str>>,
}

impl<'a> Links<'a> {
    fn new(graph: &'a DependencyGraph) -> Self {
        let mut from: HashMap<&Occurrence, Vec<&Occurrence>> = HashMap::new();
        let mut root: HashMap<&str, Vec<&Occurrence>> = HashMap::new();
        for (source, target) in &graph.resolution.links {
            match source {
                Some(source) => from.entry(source).or_default().push(target),
                None => root.entry(target.id.as_str()).or_default().push(target),
            }
        }
        let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
        for (source, target) in &graph.edges {
            if source != ROOT {
                edges.entry(source.as_str()).or_default().push(target.as_str());
            }
        }
        Self { from, root, edges }
    }

    /// The occurrences of `id` the project root links to, or one unversioned occurrence when it has none.
    fn root_occurrences(&self, id: &str) -> Vec<Occurrence> {
        match self.root.get(id) {
            Some(found) => found.iter().map(|occurrence| (*occurrence).clone()).collect(),
            None => vec![Occurrence { id: id.to_string(), version: None }],
        }
    }

    /// Linked dependencies of one installed copy. Package-level edges are the fallback only for a copy whose version
    /// is unknown (Maven/Gradle, ambiguous installs): a proven copy without links is a leaf, not a copy of its siblings.
    fn successors(&self, occurrence: &Occurrence) -> Vec<Occurrence> {
        if let Some(found) = self.from.get(occurrence) {
            return found.iter().map(|target| (*target).clone()).collect();
        }
        if occurrence.version.is_some() {
            return Vec::new();
        }
        self.edges
            .get(occurrence.id.as_str())
            .into_iter()
            .flatten()
            .map(|id| Occurrence { id: (*id).to_string(), version: None })
            .collect()
    }
}

fn walk(links: &Links, start: Vec<Occurrence>, mut visit: impl FnMut(&Occurrence) -> bool) {
    let mut seen: HashSet<Occurrence> = HashSet::new();
    let mut queue: VecDeque<Occurrence> = VecDeque::new();
    for occurrence in start {
        if seen.insert(occurrence.clone()) {
            queue.push_back(occurrence);
        }
    }
    while let Some(occurrence) = queue.pop_front() {
        if !visit(&occurrence) {
            continue;
        }
        for next in links.successors(&occurrence) {
            if seen.insert(next.clone()) {
                queue.push_back(next);
            }
        }
    }
}

/// Everything reachable below the `roots` (built-in direct packages): proven versions per package id, and whether a
/// reachable copy had no proven version. The roots' own ids are excluded.
pub fn closure(graph: &DependencyGraph, roots: &BTreeSet<String>) -> BTreeMap<String, MemoryEntry> {
    let links = Links::new(graph);
    let start: Vec<Occurrence> = roots.iter().flat_map(|id| links.root_occurrences(id)).collect();
    let mut found: BTreeMap<String, MemoryEntry> = BTreeMap::new();
    // Root ids are expanded but never recorded, whether they are the start or reached below another root.
    walk(&links, start, |occurrence| {
        if !roots.contains(&occurrence.id) {
            let entry = found.entry(occurrence.id.clone()).or_default();
            match &occurrence.version {
                Some(version) => {
                    entry.versions.insert(version.clone());
                }
                None => entry.unversioned = true,
            }
        }
        true
    });
    found
}

fn merge(into: &mut BTreeMap<String, MemoryEntry>, from: &BTreeMap<String, MemoryEntry>) {
    for (id, entry) in from {
        let target = into.entry(id.clone()).or_default();
        target.versions.extend(entry.versions.iter().cloned());
        target.unversioned |= entry.unversioned;
    }
}

pub fn derive(graph: &DependencyGraph, roots: &BTreeSet<String>, memory: &BTreeMap<String, MemoryEntry>) -> DerivedStandard {
    let mut result = DerivedStandard { complete: graph.resolution.complete, hide: Vec::new(), different: Vec::new(), unproven: Vec::new(), learned: 0 };
    if !result.complete {
        return result;
    }
    let links = Links::new(graph);
    let mut standard = closure(graph, roots);
    merge(&mut standard, memory);
    let is_standard = |occurrence: &Occurrence| {
        occurrence
            .version
            .as_ref()
            .is_some_and(|version| standard.get(&occurrence.id).is_some_and(|entry| entry.versions.contains(version)))
    };
    let direct: BTreeSet<&str> = graph.nodes.iter().filter(|node| node.direct && node.id != ROOT).map(|node| node.id.as_str()).collect();
    let start: Vec<Occurrence> = direct.iter().flat_map(|id| links.root_occurrences(id)).collect();
    let mut visible: BTreeMap<String, BTreeSet<Option<String>>> = BTreeMap::new();
    walk(&links, start, |occurrence| {
        if roots.contains(&occurrence.id) || is_standard(occurrence) {
            return false;
        }
        visible.entry(occurrence.id.clone()).or_default().insert(occurrence.version.clone());
        true
    });
    let present: HashSet<&str> = graph.nodes.iter().map(|node| node.id.as_str()).collect();
    for (id, entry) in &standard {
        if id == ROOT || direct.contains(id.as_str()) || !present.contains(id.as_str()) {
            continue;
        }
        let Some(recorded) = visible.get(id).filter(|found| !found.is_empty()) else {
            result.hide.push(id.clone());
            continue;
        };
        let proven: Vec<String> = recorded.iter().flatten().cloned().collect();
        let package = || DerivedPackage { id: id.clone(), versions: proven.clone(), standard_versions: entry.versions.iter().cloned().collect() };
        if !entry.versions.is_empty() && proven.iter().any(|version| !entry.versions.contains(version)) {
            result.different.push(package());
        } else {
            result.unproven.push(package());
        }
    }
    result
}

/// Scan `folder`, derive what built-in packages hide, and remember what they pull in. Nothing is hidden or learned
/// when the scan could not resolve every version link.
pub fn resolve(folder: &Path, home: &Path, config_root: &Path) -> Result<DerivedStandard, String> {
    let graph = crate::dependencies::scan(folder, home, true)?;
    let classification = crate::architect::read_classification(config_root, &graph.key);
    let known = crate::known_standard::read(config_root).items;
    let direct = graph.nodes.iter().filter(|node| node.direct && node.id != ROOT);
    let roots: BTreeSet<String> = direct.clone().filter(|node| classification.items.get(&node.id).is_some_and(|item| item.basic) || known.contains_key(&node.id)).map(|node| node.id.clone()).collect();
    // Only globally remembered packages are learning roots: a project's own basic flag survives a clear, memory must not.
    let learning: BTreeSet<String> = direct.filter(|node| known.contains_key(&node.id)).map(|node| node.id.clone()).collect();
    let memory = crate::known_standard::read_derived(config_root);
    let mut derived = derive(&graph, &roots, &memory);
    if derived.complete && !learning.is_empty() {
        derived.learned = crate::known_standard::learn(config_root, &learning, &closure(&graph, &learning))?;
    }
    Ok(derived)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use crate::dependencies::{DepNode, Resolution};

    fn node(id: &str, direct: bool) -> DepNode {
        let (ecosystem, name) = id.split_once(':').unwrap_or(("project", id));
        DepNode {
            id: id.into(), name: name.into(), version: None, resolved_versions: Vec::new(), declared_versions: Vec::new(),
            ecosystem: match ecosystem { "npm" => "npm", "maven" => "maven", _ => "project" }, direct, dev: false,
        }
    }

    fn occ(id: &str, version: Option<&str>) -> Occurrence {
        Occurrence { id: format!("npm:{id}"), version: version.map(String::from) }
    }

    fn graph(direct: &[&str], others: &[&str], edges: &[(&str, &str)], links: &[(Option<Occurrence>, Occurrence)], complete: bool) -> DependencyGraph {
        let mut nodes = vec![node("project", false)];
        nodes.extend(direct.iter().map(|id| node(&format!("npm:{id}"), true)));
        nodes.extend(others.iter().map(|id| node(&format!("npm:{id}"), false)));
        let mut all: Vec<(String, String)> = direct.iter().map(|id| ("project".to_string(), format!("npm:{id}"))).collect();
        all.extend(edges.iter().map(|(a, b)| (format!("npm:{a}"), format!("npm:{b}"))));
        DependencyGraph {
            folder: "f".into(), key: "k".into(), nodes, edges: all, notes: Vec::new(), truncated: !complete,
            resolution: Resolution { complete, links: links.to_vec() },
        }
    }

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| format!("npm:{id}")).collect()
    }

    fn versions(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    /// B (built-in) -> P@1 -> Q@1; N -> P@2; M -> P@1.
    fn shared() -> DependencyGraph {
        graph(
            &["b", "n", "m"],
            &["p", "q"],
            &[("b", "p"), ("p", "q"), ("n", "p"), ("m", "p")],
            &[
                (None, occ("b", Some("1"))), (None, occ("n", Some("1"))), (None, occ("m", Some("1"))),
                (Some(occ("b", Some("1"))), occ("p", Some("1"))),
                (Some(occ("p", Some("1"))), occ("q", Some("1"))),
                (Some(occ("n", Some("1"))), occ("p", Some("2"))),
                (Some(occ("m", Some("1"))), occ("p", Some("1"))),
            ],
            true,
        )
    }

    #[test]
    fn a_different_proven_version_stays_visible_and_the_built_in_subtree_is_hidden() {
        let graph = shared();
        let roots = set(&["b"]);
        let derived = derive(&graph, &roots, &BTreeMap::new());
        assert!(derived.complete);
        assert_eq!(derived.hide, ["npm:q"]);
        assert!(derived.unproven.is_empty());
        assert_eq!(derived.different, [DerivedPackage { id: "npm:p".into(), versions: versions(&["2"]), standard_versions: versions(&["1"]) }]);
        let found = closure(&graph, &roots);
        assert_eq!(found.keys().collect::<Vec<_>>(), ["npm:p", "npm:q"]);
        assert_eq!(found["npm:p"].versions, BTreeSet::from(["1".to_string()]));
        assert!(!found["npm:p"].unversioned);
    }

    #[test]
    fn a_package_only_under_the_built_in_one_is_hidden_and_direct_nodes_are_never_classified() {
        let graph = graph(
            &["b", "n"], &["only"], &[("b", "only"), ("b", "n")],
            &[
                (None, occ("b", Some("1"))), (None, occ("n", Some("1"))),
                (Some(occ("b", Some("1"))), occ("only", Some("1"))), (Some(occ("b", Some("1"))), occ("n", Some("1"))),
            ],
            true,
        );
        let derived = derive(&graph, &set(&["b"]), &BTreeMap::new());
        assert_eq!(derived.hide, ["npm:only"]);
        assert!(derived.different.is_empty() && derived.unproven.is_empty());
    }

    #[test]
    fn an_unproven_occurrence_is_neither_hidden_nor_different() {
        // The scanner could not prove the version of the copy the non-built-in package uses.
        let graph = graph(
            &["b", "n"], &["p"], &[("b", "p"), ("n", "p")],
            &[
                (None, occ("b", Some("1"))), (None, occ("n", Some("1"))),
                (Some(occ("b", Some("1"))), occ("p", Some("1"))), (Some(occ("n", Some("1"))), occ("p", None)),
            ],
            true,
        );
        let derived = derive(&graph, &set(&["b"]), &BTreeMap::new());
        assert!(derived.hide.is_empty() && derived.different.is_empty());
        assert_eq!(derived.unproven, [DerivedPackage { id: "npm:p".into(), versions: Vec::new(), standard_versions: versions(&["1"]) }]);
    }

    #[test]
    fn a_graph_without_links_falls_back_to_package_edges() {
        // Maven-style: no links. b -> only, b -> shared, n -> shared.
        let graph = graph(&["b", "n"], &["only", "shared"], &[("b", "only"), ("b", "shared"), ("n", "shared")], &[], true);
        let derived = derive(&graph, &set(&["b"]), &BTreeMap::new());
        assert_eq!(derived.hide, ["npm:only"]);
        assert!(derived.different.is_empty());
        assert_eq!(derived.unproven.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["npm:shared"]);
        let found = closure(&graph, &set(&["b"]));
        assert!(found["npm:shared"].unversioned && found["npm:shared"].versions.is_empty());
    }

    #[test]
    fn an_incomplete_scan_hides_nothing() {
        let mut graph = shared();
        graph.resolution.complete = false;
        let derived = derive(&graph, &set(&["b"]), &BTreeMap::new());
        assert_eq!(derived, DerivedStandard { complete: false, hide: Vec::new(), different: Vec::new(), unproven: Vec::new(), learned: 0 });
    }

    #[test]
    fn remembered_versions_hide_and_expose_without_a_live_root() {
        let graph = graph(
            &["n"], &["p"], &[("n", "p")],
            &[(None, occ("n", Some("1"))), (Some(occ("n", Some("1"))), occ("p", Some("1")))],
            true,
        );
        let memory = BTreeMap::from([("npm:p".to_string(), MemoryEntry { versions: BTreeSet::from(["1".to_string()]), unversioned: false })]);
        let derived = derive(&graph, &BTreeSet::new(), &memory);
        assert_eq!(derived.hide, ["npm:p"]);
        let other = graph_with_p("3");
        let derived = derive(&other, &BTreeSet::new(), &memory);
        assert!(derived.hide.is_empty());
        assert_eq!(derived.different, [DerivedPackage { id: "npm:p".into(), versions: versions(&["3"]), standard_versions: versions(&["1"]) }]);
        let unversioned = BTreeMap::from([("npm:p".to_string(), MemoryEntry { versions: BTreeSet::new(), unversioned: true })]);
        let derived = derive(&other, &BTreeSet::new(), &unversioned);
        assert!(derived.different.is_empty() && derived.hide.is_empty());
        assert_eq!(derived.unproven.len(), 1);
        // Memory about packages that are not in this graph is never reported.
        let absent = BTreeMap::from([("npm:gone".to_string(), MemoryEntry { versions: BTreeSet::from(["1".to_string()]), unversioned: false })]);
        assert!(derive(&other, &BTreeSet::new(), &absent).hide.is_empty());
    }

    fn graph_with_p(version: &str) -> DependencyGraph {
        graph(
            &["n"], &["p"], &[("n", "p")],
            &[(None, occ("n", Some("1"))), (Some(occ("n", Some("1"))), occ("p", Some(version)))],
            true,
        )
    }

    #[test]
    fn cycles_terminate() {
        let graph = graph(
            &["b", "n"], &["x", "y"], &[("b", "x"), ("x", "y"), ("y", "x"), ("n", "y")],
            &[
                (None, occ("b", Some("1"))), (None, occ("n", Some("1"))),
                (Some(occ("b", Some("1"))), occ("x", Some("1"))), (Some(occ("x", Some("1"))), occ("y", Some("1"))),
                (Some(occ("y", Some("1"))), occ("x", Some("1"))), (Some(occ("n", Some("1"))), occ("y", Some("1"))),
            ],
            true,
        );
        let derived = derive(&graph, &set(&["b"]), &BTreeMap::new());
        assert_eq!(derived.hide, ["npm:x", "npm:y"]);
        assert_eq!(closure(&graph, &set(&["b"])).len(), 2);
    }

    fn manifest(folder: &Path, name: &str, version: &str, dependencies: &[&str]) {
        let dir = folder.join("node_modules").join(name);
        fs::create_dir_all(&dir).unwrap();
        let deps: Vec<String> = dependencies.iter().map(|d| format!("\"{d}\":\"*\"")).collect();
        fs::write(dir.join("package.json"), format!("{{\"name\":\"{name}\",\"version\":\"{version}\",\"dependencies\":{{{}}}}}", deps.join(","))).unwrap();
    }

    #[test]
    fn resolving_a_project_hides_learns_once_and_forgets_nothing_after_a_clear() {
        let base = std::env::temp_dir().join(format!("omp-derived-{}", uuid::Uuid::new_v4()));
        let (project, config) = (base.join("project"), base.join("config"));
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("package.json"), r#"{"dependencies":{"std-b":"1","chosen-n":"1","chosen-m":"1"}}"#).unwrap();
        manifest(&project, "std-b", "1.0.0", &["shared"]);
        manifest(&project, "shared", "1.0.0", &["leaf"]);
        manifest(&project, "leaf", "1.0.0", &[]);
        manifest(&project, "chosen-m", "1.0.0", &["shared"]);
        manifest(&project, "chosen-n", "1.0.0", &["shared"]);
        let nested = project.join("node_modules/chosen-n");
        manifest(&nested, "shared", "2.0.0", &[]);
        let key = crate::dependencies::folder_key(&project);
        let path = crate::architect::classification_path(&config, &key);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"items":{"npm:std-b":{"basic":true,"reason":"core"}},"updatedAt":1}"#).unwrap();
        let derived = resolve(&project, &base, &config).unwrap();
        assert!(derived.complete);
        assert_eq!(derived.hide, ["npm:leaf"]);
        assert_eq!(derived.different, [DerivedPackage { id: "npm:shared".into(), versions: versions(&["2.0.0"]), standard_versions: versions(&["1.0.0"]) }]);
        assert!(derived.unproven.is_empty());
        assert_eq!(derived.learned, 2);
        let memory = crate::known_standard::read_derived(&config);
        assert_eq!(memory.keys().collect::<Vec<_>>(), ["npm:leaf", "npm:shared"]);
        assert_eq!(memory["npm:shared"].versions, BTreeSet::from(["1.0.0".to_string()]));
        assert_eq!(resolve(&project, &base, &config).unwrap().learned, 0);
        assert_eq!(crate::known_standard::clear(&config, Some("npm")).unwrap(), 1 + 2);
        let after = resolve(&project, &base, &config).unwrap();
        // The project's own classification still marks std-b built in, so it still hides; nothing is learned again.
        assert_eq!(after.hide, ["npm:leaf"]);
        assert_eq!(after.learned, 0);
        assert!(crate::known_standard::read_derived(&config).is_empty());
        fs::remove_dir_all(base).unwrap();
    }
}
