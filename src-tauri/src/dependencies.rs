//! Dependency scanner: reads a project's manifests (npm, PyPI, Maven/Gradle, Cargo) and whatever is installed or cached
//! locally, and returns one graph of packages. Nothing is executed, nothing is downloaded.
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use roxmltree::{Document, Node};
use serde::Serialize;
use sha2::{Digest, Sha256};
use toml::Table;

pub type Result<T, E = String> = std::result::Result<T, E>;

const MAX_NODES: usize = 30_000;
const MAX_FILE: u64 = 1024 * 1024;
const MODULE_MAX_DEPTH: usize = 5;
const MAX_LINKS: usize = 200_000;
const EXCLUDED: [&str; 9] = [
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    ".venv",
    "venv",
    "env",
    "__pycache__",
];
const NO_MANIFEST: &str = "(looked for package.json, pom.xml, build.gradle(.kts), pyproject.toml, requirements*.txt, Cargo.toml)";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DepNode {
    pub id: String,
    pub name: String,
    pub version: Option<String>,
    pub resolved_versions: Vec<String>,
    pub declared_versions: Vec<String>,
    pub ecosystem: &'static str,
    pub direct: bool,
    pub dev: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyGraph {
    pub folder: String,
    pub key: String,
    pub nodes: Vec<DepNode>,
    pub edges: Vec<(String, String)>,
    pub notes: Vec<String>,
    pub truncated: bool,
    #[serde(skip)]
    pub resolution: Resolution,
}

/// One installed copy of a package: `version` is `Some` only when it was proven locally from an installed manifest,
/// `.dist-info` or lock entry (never a declared range or a cache guess).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Occurrence {
    pub id: String,
    pub version: Option<String>,
}

impl Occurrence {
    fn new(id: &str, version: Option<String>) -> Self {
        Self {
            id: id.to_string(),
            version: version
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
        }
    }
}

/// Version-aware dependency links; `None` as the source is the project root. Not part of the frontend payload.
#[derive(Clone, Debug, Default)]
pub struct Resolution {
    pub complete: bool,
    pub links: Vec<(Option<Occurrence>, Occurrence)>,
}

/// `\\?\C:\x` → `C:\x`.
fn plain(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().trim_start_matches("\\\\?\\"))
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path)
        .map(|p| plain(&p))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// The canonical, verbatim-prefix-free path of an existing folder, spelled the way scan reports it.
pub(crate) fn canonical_folder(folder: &Path) -> Result<PathBuf> {
    if !folder.is_dir() {
        return Err(format!("Not a folder: {}", folder.display()));
    }
    Ok(canonical(folder))
}

/// The first 16 hex characters of the SHA-256 of the canonical folder path (forward slashes, lower case).
#[must_use]
pub fn folder_key(folder: &Path) -> String {
    let text = canonical(folder)
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();
    let text = text.trim_end_matches('/');
    Sha256::digest(text.as_bytes())[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct Builder {
    nodes: BTreeMap<String, DepNode>,
    edges: BTreeSet<(String, String)>,
    links: BTreeSet<(Option<Occurrence>, Occurrence)>,
    max_links: usize,
    notes: Vec<String>,
    truncated: bool,
}

impl Builder {
    fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            edges: BTreeSet::new(),
            links: BTreeSet::new(),
            max_links: MAX_LINKS,
            notes: Vec::new(),
            truncated: false,
        }
    }

    fn note(&mut self, text: String) {
        if !self.notes.contains(&text) {
            self.notes.push(text);
        }
    }

    /// Adds or merges a node. `dev` only describes direct declarations; a package is dev only if every direct
    /// declaration of it is.
    fn node(
        &mut self,
        ecosystem: &'static str,
        name: &str,
        version: Option<String>,
        direct: bool,
        dev: bool,
    ) -> Option<String> {
        let id = format!("{ecosystem}:{name}");
        let version = version.filter(|v| !v.is_empty());
        if let Some(existing) = self.nodes.get_mut(&id) {
            if direct {
                existing.dev = if existing.direct {
                    existing.dev && dev
                } else {
                    dev
                };
                existing.direct = true;
            }
            if existing.version.is_none() {
                existing.version = version;
            }
            return Some(id);
        }
        if self.nodes.len() >= MAX_NODES {
            self.truncated = true;
            self.note(format!("The graph was cut at {MAX_NODES} packages."));
            return None;
        }
        let node = DepNode {
            id: id.clone(),
            name: name.to_string(),
            version,
            resolved_versions: Vec::new(),
            declared_versions: Vec::new(),
            ecosystem,
            direct,
            dev: direct && dev,
        };
        self.nodes.insert(id.clone(), node);
        Some(id)
    }

    fn record_resolved(&mut self, id: &str, version: Option<&str>) {
        if let Some(node) = self.nodes.get_mut(id) {
            Self::record_version(&mut node.resolved_versions, version);
        }
    }

    fn record_declared(&mut self, id: &str, version: Option<&str>) {
        if let Some(node) = self.nodes.get_mut(id) {
            Self::record_version(&mut node.declared_versions, version);
        }
    }

    fn record_version(versions: &mut Vec<String>, version: Option<&str>) {
        let Some(version) = version.map(str::trim).filter(|v| !v.is_empty()) else {
            return;
        };
        if let Err(index) = versions.binary_search_by(|existing| existing.as_str().cmp(version)) {
            versions.insert(index, version.to_string());
        }
    }

    fn edge(&mut self, from: &str, to: &str) {
        self.edges.insert((from.to_string(), to.to_string()));
    }

    /// A version-aware link; `from == None` is the project root.
    fn link(&mut self, from: Option<Occurrence>, to: Occurrence) {
        let link = (from, to);
        if self.links.len() >= self.max_links {
            if !self.links.contains(&link) {
                self.truncated = true;
                let max = self.max_links;
                self.note(format!("The version graph was cut at {max} links."));
            }
            return;
        }
        self.links.insert(link);
    }

    /// A dependency declared by the project itself: a node plus an edge from the root.
    fn direct(
        &mut self,
        ecosystem: &'static str,
        name: &str,
        version: Option<String>,
        dev: bool,
    ) -> Option<String> {
        let id = self.node(ecosystem, name, version, true, dev)?;
        self.edge("project", &id);
        Some(id)
    }
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn read_text(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 8 * MAX_FILE {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn subdirs(path: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    found.sort();
    found
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Whether a folder holds any manifest this scanner understands. `Missing` only for the no-manifest condition;
/// every other failure (unreadable or not a folder) propagates and never offers manifest assistance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ManifestStatus {
    Present,
    Missing,
}

enum ScanFailure {
    NoManifest(String),
    Other(String),
}

impl ScanFailure {
    fn message(self) -> String {
        match self {
            Self::NoManifest(message) | Self::Other(message) => message,
        }
    }
}

pub fn manifest_status(folder: &Path, home: &Path) -> Result<ManifestStatus> {
    match scan_checked(folder, home, /* transitive */ false) {
        Ok(_) => Ok(ManifestStatus::Present),
        Err(ScanFailure::NoManifest(_)) => Ok(ManifestStatus::Missing),
        Err(ScanFailure::Other(message)) => Err(message),
    }
}

/// Scan `folder` and its immediate sub-folders. `transitive == false` stops at the declared dependencies.
pub fn scan(folder: &Path, home: &Path, transitive: bool) -> Result<DependencyGraph> {
    scan_checked(folder, home, transitive).map_err(ScanFailure::message)
}

fn scan_checked(
    folder: &Path,
    home: &Path,
    transitive: bool,
) -> std::result::Result<DependencyGraph, ScanFailure> {
    if !folder.is_dir() {
        return Err(ScanFailure::Other(format!(
            "Not a folder: {}",
            folder.display()
        )));
    }
    let root = canonical(folder);
    let mut b = Builder::new();
    b.nodes.insert(
        "project".into(),
        DepNode {
            id: "project".into(),
            name: file_name(&root),
            version: None,
            resolved_versions: Vec::new(),
            declared_versions: Vec::new(),
            ecosystem: "project",
            direct: false,
            dev: false,
        },
    );
    let mut modules = vec![root.clone()];
    for dir in subdirs(&root) {
        let name = file_name(&dir);
        if !name.starts_with('.') && !EXCLUDED.contains(&name.as_str()) {
            modules.push(dir);
        }
    }
    let mut found = false;
    for module in &modules {
        found |= scan_npm(module, transitive, &mut b);
        found |= scan_pypi(module, &root, transitive, &mut b);
        found |= scan_java(module, home, transitive, &mut b);
        found |= scan_cargo(module, &root, transitive, &mut b);
    }
    if !found {
        return Err(ScanFailure::NoManifest(format!(
            "No dependency manifest found in {} {NO_MANIFEST}.",
            folder.display()
        )));
    }
    let edges = b
        .edges
        .iter()
        .filter(|(from, to)| b.nodes.contains_key(from) && b.nodes.contains_key(to))
        .cloned()
        .collect();
    let links = b
        .links
        .iter()
        .filter(|(from, to)| {
            b.nodes.contains_key(&to.id)
                && from
                    .as_ref()
                    .is_none_or(|from| b.nodes.contains_key(&from.id))
        })
        .cloned()
        .collect();
    Ok(DependencyGraph {
        folder: root.display().to_string(),
        key: folder_key(&root),
        nodes: b.nodes.into_values().collect(),
        edges,
        notes: b.notes,
        truncated: b.truncated,
        resolution: Resolution {
            complete: !b.truncated,
            links,
        },
    })
}

// ---- npm -------------------------------------------------------------------------------------------------------

fn json_keys(value: &serde_json::Value, key: &str) -> Vec<(String, String)> {
    value
        .get(key)
        .and_then(|v| v.as_object())
        .map(|map| {
            map.iter()
                .map(|(name, range)| (name.clone(), range.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// The first `A/node_modules/<name>` holding a package, for `A` from `from` up to and including `stop`.
fn resolve_npm(from: &Path, stop: &Path, name: &str) -> Option<PathBuf> {
    let mut current = from.to_path_buf();
    loop {
        let candidate = current.join("node_modules").join(name);
        if candidate.join("package.json").is_file() {
            return Some(candidate);
        }
        if current == stop || !current.pop() {
            return None;
        }
    }
}

fn npm_direct_dependencies(
    module: &Path,
    json: &serde_json::Value,
    transitive: bool,
    b: &mut Builder,
    queue: &mut VecDeque<(Occurrence, PathBuf)>,
    visited: &mut HashSet<PathBuf>,
) {
    for (key, dev) in [
        ("dependencies", false),
        ("optionalDependencies", false),
        ("devDependencies", true),
    ] {
        for (name, range) in json_keys(json, key) {
            let installed = resolve_npm(module, module, &name);
            let resolved = installed
                .as_ref()
                .and_then(|dir| read_json(&dir.join("package.json")))
                .and_then(|pkg| pkg["version"].as_str().map(String::from));
            let version = resolved.clone().or_else(|| Some(range.clone()));
            let Some(id) = b.direct("npm", &name, version, dev) else {
                continue;
            };
            b.link(None, Occurrence::new(&id, resolved.clone()));
            b.record_resolved(&id, resolved.as_deref());
            b.record_declared(&id, Some(&range));
            let Some(dir) = installed.filter(|_| transitive) else {
                continue;
            };
            let dir = canonical(&dir);
            if visited.insert(dir.clone()) {
                queue.push_back((Occurrence::new(&id, resolved), dir));
            }
        }
    }
}

fn npm_transitive_dependencies(
    stop: &Path,
    queue: &mut VecDeque<(Occurrence, PathBuf)>,
    visited: &mut HashSet<PathBuf>,
    b: &mut Builder,
) {
    while let Some((parent, dir)) = queue.pop_front() {
        let Some(pkg) = read_json(&dir.join("package.json")) else {
            continue;
        };
        let declarations = json_keys(&pkg, "dependencies")
            .into_iter()
            .chain(json_keys(&pkg, "optionalDependencies"));
        for (name, range) in declarations {
            let Some(found) = resolve_npm(&dir, stop, &name) else {
                continue;
            };
            let dep_dir = canonical(&found);
            let version = read_json(&dep_dir.join("package.json"))
                .and_then(|p| p["version"].as_str().map(String::from));
            let known = b
                .nodes
                .get(&format!("npm:{name}"))
                .and_then(|node| node.version.as_deref());
            if known.is_some() && version.is_some() && known != version.as_deref() {
                b.note("Packages installed in several versions are shown once.".into());
            }
            let Some(id) = b.node(
                "npm",
                &name,
                version.clone(),
                /* direct */ false,
                /* dev */ false,
            ) else {
                continue;
            };
            b.record_resolved(&id, version.as_deref());
            b.record_declared(&id, Some(&range));
            b.edge(&parent.id, &id);
            b.link(Some(parent.clone()), Occurrence::new(&id, version.clone()));
            if visited.insert(dep_dir.clone()) {
                queue.push_back((Occurrence::new(&id, version), dep_dir));
            }
        }
    }
}

fn scan_npm(module: &Path, transitive: bool, b: &mut Builder) -> bool {
    let manifest = module.join("package.json");
    if !manifest.is_file() {
        return false;
    }
    let Some(json) = read_json(&manifest) else {
        return true;
    };
    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();
    npm_direct_dependencies(module, &json, transitive, b, &mut queue, &mut visited);
    if !transitive {
        return true;
    }
    if !module.join("node_modules").is_dir() {
        b.note(format!(
            "node_modules was not found in {}: only direct npm dependencies are shown. Install the packages to see transitive dependencies.",
            module.display()
        ));
        return true;
    }
    npm_transitive_dependencies(&canonical(module), &mut queue, &mut visited, b);
    true
}

// ---- PyPI ------------------------------------------------------------------------------------------------------

/// Lower case, runs of `-`, `_` and `.` collapsed to one `-`.
fn normalize_python(name: &str) -> String {
    let mut out = String::new();
    let mut separator = false;
    for ch in name.chars() {
        if matches!(ch, '-' | '_' | '.') {
            separator = true;
        } else {
            if separator && !out.is_empty() {
                out.push('-');
            }
            separator = false;
            out.extend(ch.to_lowercase());
        }
    }
    out
}

/// The leading name of a PEP 508 requirement (`requests[security]>=2; python_version<'3.8'` → `requests`).
fn requirement_name(spec: &str) -> Option<String> {
    let spec = spec.trim();
    if !spec
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphanumeric())
    {
        return None;
    }
    let end = spec
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')))
        .unwrap_or(spec.len());
    Some(normalize_python(&spec[..end]))
}

fn find_site_packages(module: &Path, root: &Path) -> Option<PathBuf> {
    let mut dirs = vec![module];
    if module != root {
        dirs.push(root);
    }
    for dir in dirs {
        for venv in [".venv", "venv", "env"] {
            let base = dir.join(venv);
            let windows = base.join("Lib").join("site-packages");
            if windows.is_dir() {
                return Some(windows);
            }
            for lib in subdirs(&base.join("lib")) {
                if file_name(&lib).starts_with("python") && lib.join("site-packages").is_dir() {
                    return Some(lib.join("site-packages"));
                }
            }
        }
    }
    None
}

#[derive(Default)]
struct Installed {
    version: Option<String>,
    requires: Vec<String>,
    /// A second `.dist-info` for the same name carried a different `Version:`: no version is proven.
    ambiguous: bool,
}

/// The installed version of `name`, only when exactly one is installed and the metadata states it.
fn proven_python_version(installed: &HashMap<String, Installed>, name: &str) -> Option<String> {
    installed
        .get(name)
        .filter(|pkg| !pkg.ambiguous)
        .and_then(|pkg| pkg.version.clone())
}

/// The `Name`, `Version` and unconditional `Requires-Dist` names of a `METADATA` header.
fn parse_python_metadata(text: &str) -> (Option<String>, Option<String>, Vec<String>) {
    let (mut name, mut version, mut requires) = (None, None, Vec::new());
    for line in text.lines() {
        if line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Name:") {
            name = requirement_name(value);
        } else if let Some(value) = line.strip_prefix("Version:") {
            version = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Requires-Dist:") {
            if !value.contains("extra ==") && !value.contains("extra==") {
                requires.extend(requirement_name(value));
            }
        }
    }
    (name, version, requires)
}

fn installed_python(site: &Path) -> HashMap<String, Installed> {
    let mut map = HashMap::new();
    for dir in subdirs(site) {
        if !file_name(&dir).ends_with(".dist-info") {
            continue;
        }
        let Some(text) = read_text(&dir.join("METADATA")) else {
            continue;
        };
        let (name, version, requires) = parse_python_metadata(&text);
        if let Some(name) = name {
            let ambiguous = map.get(&name).is_some_and(|previous: &Installed| {
                previous.ambiguous || previous.version != version
            });
            map.insert(
                name,
                Installed {
                    version,
                    requires,
                    ambiguous,
                },
            );
        }
    }
    map
}

struct PythonDeclaration {
    dev: bool,
    versions: Vec<String>,
}

fn add_python_declaration(
    found: &mut BTreeMap<String, PythonDeclaration>,
    name: Option<String>,
    version: Option<&str>,
    dev: bool,
) {
    let Some(name) = name else { return };
    let declaration = found.entry(name).or_insert_with(|| PythonDeclaration {
        dev,
        versions: Vec::new(),
    });
    declaration.dev &= dev;
    Builder::record_version(&mut declaration.versions, version);
}

fn poetry_dependencies(table: &Table, found: &mut BTreeMap<String, PythonDeclaration>, dev: bool) {
    for (name, spec) in table {
        if name == "python" {
            continue;
        }
        let version = spec
            .as_str()
            .or_else(|| spec.get("version").and_then(|v| v.as_str()));
        add_python_declaration(found, Some(normalize_python(name)), version, dev);
    }
}

fn project_dependencies(project: &Table, found: &mut BTreeMap<String, PythonDeclaration>) {
    let dependencies = project
        .get("dependencies")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten();
    for spec in dependencies {
        add_python_declaration(
            found,
            spec.as_str().and_then(requirement_name),
            spec.as_str(),
            false,
        );
    }
    let groups = project
        .get("optional-dependencies")
        .and_then(|value| value.as_table())
        .into_iter()
        .flat_map(|table| table.values());
    for group in groups {
        let dependencies = group.as_array().into_iter().flatten();
        for spec in dependencies {
            add_python_declaration(
                found,
                spec.as_str().and_then(requirement_name),
                spec.as_str(),
                true,
            );
        }
    }
}

fn poetry_table_dependencies(poetry: &Table, found: &mut BTreeMap<String, PythonDeclaration>) {
    if let Some(deps) = poetry.get("dependencies").and_then(|v| v.as_table()) {
        poetry_dependencies(deps, found, false);
    }
    if let Some(deps) = poetry.get("dev-dependencies").and_then(|v| v.as_table()) {
        poetry_dependencies(deps, found, true);
    }
    let groups = poetry
        .get("group")
        .and_then(|value| value.as_table())
        .into_iter()
        .flat_map(|table| table.values());
    for group in groups {
        if let Some(deps) = group.get("dependencies").and_then(|v| v.as_table()) {
            poetry_dependencies(deps, found, true);
        }
    }
}

fn pyproject_dependencies(table: &Table, found: &mut BTreeMap<String, PythonDeclaration>) {
    if let Some(project) = table.get("project").and_then(|v| v.as_table()) {
        project_dependencies(project, found);
    }
    if let Some(poetry) = table
        .get("tool")
        .and_then(|v| v.get("poetry"))
        .and_then(|v| v.as_table())
    {
        poetry_table_dependencies(poetry, found);
    }
}

fn requirement_files(module: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(module)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = file_name(path).to_lowercase();
            path.is_file() && name.starts_with("requirements") && name.ends_with(".txt")
        })
        .collect();
    files.sort();
    files
}

fn requirements_declarations(path: &Path, direct: &mut BTreeMap<String, PythonDeclaration>) {
    let name = file_name(path).to_lowercase();
    let dev = name.contains("dev") || name.contains("test");
    for line in read_text(path).unwrap_or_default().lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') || line.contains("://")
        {
            continue;
        }
        add_python_declaration(direct, requirement_name(line), Some(line), dev);
    }
}

fn python_direct_nodes(
    direct: &BTreeMap<String, PythonDeclaration>,
    installed: &HashMap<String, Installed>,
    transitive: bool,
    b: &mut Builder,
    queue: &mut VecDeque<String>,
    visited: &mut HashSet<String>,
) {
    for (name, declaration) in direct {
        let version = installed.get(name).and_then(|pkg| pkg.version.as_deref());
        let legacy_version = if transitive {
            version.map(String::from)
        } else {
            None
        };
        let Some(id) = b.direct("pypi", name, legacy_version, declaration.dev) else {
            continue;
        };
        b.link(
            None,
            Occurrence::new(&id, proven_python_version(installed, name)),
        );
        b.record_resolved(&id, version);
        for version in &declaration.versions {
            b.record_declared(&id, Some(version));
        }
        if transitive && visited.insert(name.clone()) {
            queue.push_back(name.clone());
        }
    }
}

fn python_transitive_nodes(
    installed: &HashMap<String, Installed>,
    b: &mut Builder,
    queue: &mut VecDeque<String>,
    visited: &mut HashSet<String>,
) {
    while let Some(name) = queue.pop_front() {
        let Some(package) = installed.get(&name) else {
            continue;
        };
        let from = Occurrence::new(
            &format!("pypi:{name}"),
            proven_python_version(installed, &name),
        );
        for required in &package.requires {
            let version = installed.get(required).and_then(|pkg| pkg.version.clone());
            let Some(id) = b.node(
                "pypi",
                required,
                version.clone(),
                /* direct */ false,
                /* dev */ false,
            ) else {
                continue;
            };
            b.record_resolved(&id, version.as_deref());
            b.edge(&from.id, &id);
            b.link(
                Some(from.clone()),
                Occurrence::new(&id, proven_python_version(installed, required)),
            );
            if visited.insert(required.clone()) {
                queue.push_back(required.clone());
            }
        }
    }
}

fn scan_pypi(module: &Path, root: &Path, transitive: bool, b: &mut Builder) -> bool {
    let mut found = false;
    let mut direct: BTreeMap<String, PythonDeclaration> = BTreeMap::new();
    if let Some(text) = read_text(&module.join("pyproject.toml")) {
        found = true;
        if let Ok(table) = text.parse::<Table>() {
            pyproject_dependencies(&table, &mut direct);
        }
    }
    for path in requirement_files(module) {
        found = true;
        requirements_declarations(&path, &mut direct);
    }
    if !found {
        return false;
    }
    let site = find_site_packages(module, root);
    let installed = site.as_deref().map(installed_python).unwrap_or_default();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    python_direct_nodes(&direct, &installed, transitive, b, &mut queue, &mut visited);
    if transitive && site.is_none() {
        b.note("No virtual environment (.venv, venv, env) was found: only direct Python dependencies are shown.".into());
    }
    python_transitive_nodes(&installed, b, &mut queue, &mut visited);
    true
}

// ---- Maven and Gradle --------------------------------------------------------------------------------------------

#[derive(Clone)]
struct PomDependency {
    group: String,
    artifact: String,
    version: Option<String>,
    scope: Option<String>,
    optional: bool,
}

fn child<'document, 'input>(
    node: Node<'document, 'input>,
    name: &str,
) -> Option<Node<'document, 'input>> {
    node.children()
        .find(|candidate| candidate.is_element() && candidate.tag_name().name() == name)
}

fn child_text(node: Node<'_, '_>, name: &str) -> Option<String> {
    child(node, name)
        .and_then(|n| n.text())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// `${name}` references resolved from `properties`; `None` while anything stays unresolved.
fn interpolate(value: &str, properties: &HashMap<String, String>) -> Option<String> {
    let mut text = value.trim().to_string();
    for _ in 0..5 {
        let Some(start) = text.find("${") else {
            return Some(text).filter(|t| !t.is_empty());
        };
        let end = text[start..].find('}')? + start;
        let replacement = properties.get(&text[start + 2..end])?;
        text.replace_range(start..=end, replacement);
    }
    None
}

fn pom_properties(project: Node<'_, '_>) -> HashMap<String, String> {
    let mut properties = HashMap::new();
    if let Some(list) = child(project, "properties") {
        let elements = list.children().filter(|node| node.is_element());
        for property in elements {
            if let Some(text) = property.text() {
                properties.insert(
                    property.tag_name().name().to_string(),
                    text.trim().to_string(),
                );
            }
        }
    }
    let parent = child(project, "parent");
    let version =
        child_text(project, "version").or_else(|| parent.and_then(|p| child_text(p, "version")));
    let group =
        child_text(project, "groupId").or_else(|| parent.and_then(|p| child_text(p, "groupId")));
    for key in ["project.version", "pom.version", "version"] {
        if let Some(version) = &version {
            properties.insert(key.to_string(), version.clone());
        }
    }
    for key in ["project.groupId", "pom.groupId"] {
        if let Some(group) = &group {
            properties.insert(key.to_string(), group.clone());
        }
    }
    properties
}

fn pom_dependencies(text: &str) -> Vec<PomDependency> {
    let Ok(document) = Document::parse(text) else {
        return Vec::new();
    };
    let project = document.root_element();
    let properties = pom_properties(project);
    let managed: HashMap<String, String> = child(project, "dependencyManagement")
        .and_then(|m| child(m, "dependencies"))
        .into_iter()
        .flat_map(|list| list.children().filter(|n| n.is_element()))
        .filter_map(|dep| {
            let group = interpolate(&child_text(dep, "groupId")?, &properties)?;
            let artifact = interpolate(&child_text(dep, "artifactId")?, &properties)?;
            Some((
                format!("{group}:{artifact}"),
                interpolate(&child_text(dep, "version")?, &properties)?,
            ))
        })
        .collect();
    child(project, "dependencies")
        .into_iter()
        .flat_map(|list| {
            list.children()
                .filter(|n| n.is_element() && n.tag_name().name() == "dependency")
        })
        .filter_map(|dep| {
            let group = interpolate(&child_text(dep, "groupId")?, &properties)?;
            let artifact = interpolate(&child_text(dep, "artifactId")?, &properties)?;
            let version = child_text(dep, "version")
                .and_then(|v| interpolate(&v, &properties))
                .or_else(|| managed.get(&format!("{group}:{artifact}")).cloned());
            Some(PomDependency {
                group,
                artifact,
                version,
                scope: child_text(dep, "scope"),
                optional: child_text(dep, "optional").is_some_and(|v| v == "true"),
            })
        })
        .collect()
}

fn pom_modules(text: &str) -> Vec<String> {
    let Ok(document) = Document::parse(text) else {
        return Vec::new();
    };
    child(document.root_element(), "modules")
        .into_iter()
        .flat_map(|list| {
            list.children()
                .filter(|n| n.is_element() && n.tag_name().name() == "module")
        })
        .filter_map(|module| module.text().map(|t| t.trim().to_string()))
        .filter(|name| !name.is_empty())
        .collect()
}

const GRADLE_CONFIGURATIONS: [&str; 10] = [
    "api",
    "implementation",
    "compileOnly",
    "runtimeOnly",
    "annotationProcessor",
    "compile",
    "runtime",
    "testImplementation",
    "testRuntimeOnly",
    "testCompile",
];

/// Read literal properties only; Gradle expressions are never executed or guessed.
fn gradle_properties(text: &str, quoted: bool, properties: &mut HashMap<String, String>) {
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        let key = key
            .trim()
            .trim_start_matches("val ")
            .trim_start_matches("var ")
            .trim_start_matches("def ")
            .trim_start_matches("ext.");
        if key.is_empty()
            || !key
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.')
        {
            continue;
        }
        let value = value.trim();
        let value = if quoted {
            let Some(quote) = value.chars().next().filter(|ch| *ch == '"' || *ch == '\'') else {
                continue;
            };
            let Some(end) = value[1..].find(quote) else {
                continue;
            };
            &value[1..1 + end]
        } else {
            if key.starts_with('#') || key.starts_with('!') {
                continue;
            }
            value
        };
        properties.insert(key.to_string(), value.to_string());
    }
}

fn gradle_version(value: &str, properties: &HashMap<String, String>) -> Option<String> {
    let mut expanded = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        expanded.push(ch);
        if ch == '$'
            && chars
                .peek()
                .is_some_and(|ch| ch.is_ascii_alphabetic() || *ch == '_')
        {
            expanded.push('{');
            while chars
                .peek()
                .is_some_and(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
            {
                expanded.push(chars.next()?);
            }
            expanded.push('}');
        }
    }
    let version = interpolate(&expanded, properties)?;
    (!version.contains('$')).then_some(version)
}

/// `(group, artifact, version, dev)` of every string-notation dependency; reports whether a version-catalog
/// reference (`libs.x`) was seen.
fn gradle_dependencies(
    text: &str,
    properties: &HashMap<String, String>,
) -> (Vec<(String, String, Option<String>, bool)>, bool) {
    let mut found = Vec::new();
    let mut catalog = false;
    for line in text.lines() {
        let line = line.trim();
        let configuration = GRADLE_CONFIGURATIONS.iter().find(|keyword| {
            line.strip_prefix(**keyword)
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_whitespace() || c == '('))
        });
        let Some(configuration) = configuration else {
            continue;
        };
        let rest =
            line[configuration.len()..].trim_start_matches(|c: char| c == '(' || c.is_whitespace());
        if rest.starts_with("libs.") {
            catalog = true;
            continue;
        }
        let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            continue;
        };
        let Some(end) = rest[1..].find(quote) else {
            continue;
        };
        let mut parts = rest[1..1 + end].split(':');
        let (Some(group), Some(artifact)) = (parts.next(), parts.next()) else {
            continue;
        };
        if group.is_empty() || artifact.is_empty() {
            continue;
        }
        let version = parts.next().and_then(|version| {
            if quote == '"' {
                gradle_version(version, properties)
            } else {
                (!version.is_empty() && !version.contains('$')).then(|| version.to_string())
            }
        });
        found.push((
            group.to_string(),
            artifact.to_string(),
            version,
            configuration.starts_with("test"),
        ));
    }
    (found, catalog)
}

#[derive(Clone, Copy)]
enum Layout {
    Maven,
    Gradle,
}

fn compare_versions(a: &str, b: &str) -> Ordering {
    let (mut left, mut right) = (a.split(['.', '-']), b.split(['.', '-']));
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let order = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    (Ok(_), Err(_)) => Ordering::Greater,
                    (Err(_), Ok(_)) => Ordering::Less,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

fn pom_file(
    root: &Path,
    layout: Layout,
    group: &str,
    artifact: &str,
    version: &str,
) -> Option<PathBuf> {
    let file = format!("{artifact}-{version}.pom");
    match layout {
        Layout::Maven => {
            let path = root
                .join(group.replace('.', "/"))
                .join(artifact)
                .join(version)
                .join(&file);
            path.is_file().then_some(path)
        }
        Layout::Gradle => subdirs(&root.join(group).join(artifact).join(version))
            .into_iter()
            .map(|hash| hash.join(&file))
            .find(|p| p.is_file()),
    }
}

fn pom_versions(root: &Path, layout: Layout, group: &str, artifact: &str) -> Vec<String> {
    let dir = match layout {
        Layout::Maven => root.join(group.replace('.', "/")).join(artifact),
        Layout::Gradle => root.join(group).join(artifact),
    };
    subdirs(&dir).iter().map(|path| file_name(path)).collect()
}

/// The POM and version for `group:artifact`: the exact version when known, else the highest one with a POM.
fn find_pom(
    roots: &[(PathBuf, Layout)],
    group: &str,
    artifact: &str,
    version: Option<&str>,
) -> Option<(PathBuf, String)> {
    if let Some(version) = version {
        return roots.iter().find_map(|(root, layout)| {
            pom_file(root, *layout, group, artifact, version)
                .map(|path| (path, version.to_string()))
        });
    }
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    for (root, layout) in roots {
        for version in pom_versions(root, *layout, group, artifact) {
            if let Some(path) = pom_file(root, *layout, group, artifact, &version) {
                candidates.push((version, path));
            }
        }
    }
    candidates
        .into_iter()
        .max_by(|a, b| compare_versions(&a.0, &b.0))
        .map(|(version, path)| (path, version))
}

fn collect_poms(
    dir: &Path,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    seeds: &mut Vec<(String, String, Option<String>, bool)>,
    found: &mut bool,
) {
    let dir = canonical(dir);
    if depth > MODULE_MAX_DEPTH || !visited.insert(dir.clone()) {
        return;
    }
    let Some(text) = read_text(&dir.join("pom.xml")) else {
        return;
    };
    *found = true;
    for dep in pom_dependencies(&text) {
        seeds.push((
            dep.group,
            dep.artifact,
            dep.version,
            dep.scope.as_deref() == Some("test"),
        ));
    }
    for module in pom_modules(&text) {
        collect_poms(&dir.join(module), depth + 1, visited, seeds, found);
    }
}

type JavaSeed = (String, String, Option<String>, bool);

/// POM and Gradle dependency seeds, whether any manifest was found, and whether a Gradle version catalog was seen.
fn java_seeds(module: &Path) -> (Vec<JavaSeed>, bool, bool) {
    let mut seeds: Vec<JavaSeed> = Vec::new();
    let mut found = false;
    collect_poms(module, 0, &mut HashSet::new(), &mut seeds, &mut found);
    let mut catalog = false;
    for name in ["build.gradle", "build.gradle.kts"] {
        let Some(text) = read_text(&module.join(name)) else {
            continue;
        };
        found = true;
        let mut properties = HashMap::new();
        if let Some(text) = read_text(&module.join("gradle.properties")) {
            gradle_properties(&text, false, &mut properties);
        }
        gradle_properties(&text, true, &mut properties);
        let (list, saw_catalog) = gradle_dependencies(&text, &properties);
        seeds.extend(list);
        catalog |= saw_catalog;
    }
    (seeds, found, catalog)
}

fn java_transitive(
    roots: &[(PathBuf, Layout)],
    queue: &mut VecDeque<(String, String, Option<String>)>,
    visited: &mut HashSet<String>,
    b: &mut Builder,
) {
    while let Some((group, artifact, version)) = queue.pop_front() {
        let Some((pom, used)) = find_pom(roots, &group, &artifact, version.as_deref()) else {
            continue;
        };
        let from = format!("maven:{group}:{artifact}");
        // The cached version only feeds the legacy `version`; it is neither resolved nor declared evidence.
        b.node(
            "maven",
            &format!("{group}:{artifact}"),
            Some(used),
            /* direct */ false,
            /* dev */ false,
        );
        let Some(text) = read_text(&pom) else {
            continue;
        };
        for dep in pom_dependencies(&text) {
            if dep.optional || matches!(dep.scope.as_deref(), Some("test" | "provided" | "system"))
            {
                continue;
            }
            let name = format!("{}:{}", dep.group, dep.artifact);
            let Some(id) = b.node(
                "maven",
                &name,
                dep.version.clone(),
                /* direct */ false,
                /* dev */ false,
            ) else {
                continue;
            };
            b.record_declared(&id, dep.version.as_deref());
            b.edge(&from, &id);
            if visited.insert(name) {
                queue.push_back((dep.group, dep.artifact, dep.version));
            }
        }
    }
}

fn scan_java(module: &Path, home: &Path, transitive: bool, b: &mut Builder) -> bool {
    let (seeds, found, catalog) = java_seeds(module);
    if !found {
        return false;
    }
    if catalog {
        b.note("Gradle version-catalog dependencies (libs.*) are not read.".into());
    }
    b.note("Dependencies inherited from parent POMs are not followed.".into());
    let mut queue: VecDeque<(String, String, Option<String>)> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    for (group, artifact, version, dev) in seeds {
        let name = format!("{group}:{artifact}");
        let Some(id) = b.direct("maven", &name, version.clone(), dev) else {
            continue;
        };
        b.record_declared(&id, version.as_deref());
        if visited.insert(name) {
            queue.push_back((group, artifact, version));
        }
    }
    if !transitive {
        return true;
    }
    let roots: Vec<(PathBuf, Layout)> = [
        (home.join(".m2").join("repository"), Layout::Maven),
        (
            home.join(".gradle")
                .join("caches")
                .join("modules-2")
                .join("files-2.1"),
            Layout::Gradle,
        ),
    ]
    .into_iter()
    .filter(|(path, _)| path.is_dir())
    .collect();
    if roots.is_empty() {
        b.note("No local Maven or Gradle repository was found: only direct Java dependencies are shown.".into());
        return true;
    }
    java_transitive(&roots, &mut queue, &mut visited, b);
    true
}

// ---- Cargo -----------------------------------------------------------------------------------------------------

/// Declared Cargo dependencies, with `package = "…"` renames resolved to the real crate name.
fn cargo_direct_dependencies(manifest: &Table, b: &mut Builder) -> Vec<String> {
    let mut direct: Vec<String> = Vec::new();
    for (key, dev) in [
        ("dependencies", false),
        ("build-dependencies", false),
        ("dev-dependencies", true),
    ] {
        let dependencies = manifest
            .get(key)
            .and_then(|value| value.as_table())
            .into_iter()
            .flatten();
        for (name, spec) in dependencies {
            let name = spec
                .get("package")
                .and_then(|v| v.as_str())
                .unwrap_or(name)
                .to_string();
            let Some(id) = b.direct("cargo", &name, None, dev) else {
                continue;
            };
            let version = spec
                .as_str()
                .or_else(|| spec.get("version").and_then(|v| v.as_str()));
            b.record_declared(&id, version);
            direct.push(name);
        }
    }
    direct
}

struct CargoLock {
    packages: HashMap<String, (String, Vec<String>)>,
    versions: HashMap<String, Vec<String>>,
    /// Every lock entry by (name, version): the dependency strings are `name[ version[ (source)]]`.
    by_version: HashMap<(String, String), BTreeSet<(String, Option<String>)>>,
}

impl CargoLock {
    fn parse(lock: &Table) -> Self {
        let mut parsed = CargoLock {
            packages: HashMap::new(),
            versions: HashMap::new(),
            by_version: HashMap::new(),
        };
        let packages = lock
            .get("package")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten();
        for package in packages {
            let Some(name) = package.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let version = package
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Builder::record_version(
                parsed.versions.entry(name.to_string()).or_default(),
                Some(&version),
            );
            let entries: Vec<&str> = package
                .get("dependencies")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|dep| dep.as_str())
                .collect();
            if !version.is_empty() {
                let pairs = entries
                    .iter()
                    .filter_map(|text| cargo_dependency_pair(text));
                parsed
                    .by_version
                    .entry((name.to_string(), version.clone()))
                    .or_default()
                    .extend(pairs);
            }
            let dependencies: Vec<String> = entries
                .iter()
                .filter_map(|text| text.split(' ').next())
                .map(String::from)
                .collect();
            parsed
                .packages
                .entry(name.to_string())
                .or_insert((version, dependencies));
        }
        parsed
    }

    fn sole_version(&self, name: &str) -> Option<String> {
        self.versions
            .get(name)
            .filter(|found| found.len() == 1)
            .map(|found| found[0].clone())
    }
}

fn cargo_dependency_pair(text: &str) -> Option<(String, Option<String>)> {
    let mut parts = text.split(' ');
    let name = parts.next().filter(|name| !name.is_empty())?;
    Some((
        name.to_string(),
        parts
            .next()
            .filter(|version| !version.starts_with('('))
            .map(String::from),
    ))
}

fn cargo_collapsed_graph(direct: Vec<String>, lock: &CargoLock, transitive: bool, b: &mut Builder) {
    let mut queue: VecDeque<String> = direct.iter().cloned().collect();
    let mut visited: HashSet<String> = direct.into_iter().collect();
    while let Some(name) = queue.pop_front() {
        let from = format!("cargo:{name}");
        let versions = lock.versions.get(&name).into_iter().flatten();
        for version in versions {
            b.record_resolved(&from, Some(version));
        }
        if !transitive {
            continue;
        }
        let Some((version, dependencies)) = lock.packages.get(&name) else {
            continue;
        };
        b.node(
            "cargo",
            &name,
            Some(version.clone()),
            /* direct */ false,
            /* dev */ false,
        );
        for dependency in dependencies {
            let version = lock
                .packages
                .get(dependency)
                .map(|(version, _)| version.clone());
            let Some(id) = b.node(
                "cargo", dependency, version, /* direct */ false, /* dev */ false,
            ) else {
                continue;
            };
            b.edge(&from, &id);
            if visited.insert(dependency.clone()) {
                queue.push_back(dependency.clone());
            }
        }
    }
}

fn cargo_occurrence_links(roots: Vec<(String, Option<String>)>, lock: &CargoLock, b: &mut Builder) {
    let mut occurrences: VecDeque<(String, Option<String>)> = roots.iter().cloned().collect();
    let mut seen: HashSet<(String, Option<String>)> = roots.into_iter().collect();
    while let Some((name, version)) = occurrences.pop_front() {
        let Some(version) = version else { continue };
        let Some(dependencies) = lock.by_version.get(&(name.clone(), version.clone())) else {
            continue;
        };
        let from = Occurrence::new(&format!("cargo:{name}"), Some(version));
        for (dependency, given) in dependencies {
            let resolved = given.clone().or_else(|| lock.sole_version(dependency));
            b.link(
                Some(from.clone()),
                Occurrence::new(&format!("cargo:{dependency}"), resolved.clone()),
            );
            if seen.insert((dependency.clone(), resolved.clone())) {
                occurrences.push_back((dependency.clone(), resolved));
            }
        }
    }
}

fn scan_cargo(module: &Path, root: &Path, transitive: bool, b: &mut Builder) -> bool {
    let Some(text) = read_text(&module.join("Cargo.toml")) else {
        return false;
    };
    let Ok(manifest) = text.parse::<Table>() else {
        return true;
    };
    let direct = cargo_direct_dependencies(&manifest, b);
    let lock = [module, root]
        .iter()
        .find_map(|dir| read_text(&dir.join("Cargo.lock")));
    let Some(lock) = lock.and_then(|text| text.parse::<Table>().ok()) else {
        for name in &direct {
            b.link(None, Occurrence::new(&format!("cargo:{name}"), None));
        }
        if transitive {
            b.note("No Cargo.lock was found: only direct Rust dependencies are shown.".into());
        }
        return true;
    };
    let lock = CargoLock::parse(&lock);
    let roots: Vec<(String, Option<String>)> = direct
        .iter()
        .map(|name| (name.clone(), lock.sole_version(name)))
        .collect();
    for (name, version) in &roots {
        b.link(
            None,
            Occurrence::new(&format!("cargo:{name}"), version.clone()),
        );
    }
    cargo_collapsed_graph(direct, &lock, transitive, b);
    if transitive {
        cargo_occurrence_links(roots, &lock, b);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let folder = std::env::temp_dir().join(format!("omp-deps-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn package(root: &Path, name: &str, version: &str, dependencies: &[&str]) {
        let deps: Vec<String> = dependencies
            .iter()
            .map(|d| format!("\"{d}\": \"*\""))
            .collect();
        write(
            &root.join("node_modules").join(name).join("package.json"),
            &format!(
                "{{\"name\":\"{name}\",\"version\":\"{version}\",\"dependencies\":{{{}}}}}",
                deps.join(",")
            ),
        );
    }

    fn ids(graph: &DependencyGraph) -> Vec<&str> {
        graph.nodes.iter().map(|node| node.id.as_str()).collect()
    }

    fn has_edge(graph: &DependencyGraph, from: &str, to: &str) -> bool {
        graph.edges.iter().any(|(a, b)| a == from && b == to)
    }

    fn node<'a>(graph: &'a DependencyGraph, id: &str) -> &'a DepNode {
        graph.nodes.iter().find(|node| node.id == id).unwrap()
    }

    #[test]
    fn npm_dependencies_follow_node_resolution_and_installed_versions() {
        let root = temp();
        write(
            &root.join("package.json"),
            r#"{"dependencies":{"app-lib":"^1"},"devDependencies":{"tool":"^2"}}"#,
        );
        package(&root, "app-lib", "1.0.0", &["@angular/core", "rxjs"]);
        package(&root, "@angular/core", "17.0.0", &["rxjs"]);
        package(&root, "tool", "2.0.0", &[]);
        package(&root, "rxjs", "7.0.0", &[]);
        write(
            &root.join("node_modules/app-lib/node_modules/rxjs/package.json"),
            r#"{"name":"rxjs","version":"6.0.0"}"#,
        );
        let graph = scan(&root, &root, true).unwrap();
        assert_eq!(
            ids(&graph),
            [
                "npm:@angular/core",
                "npm:app-lib",
                "npm:rxjs",
                "npm:tool",
                "project"
            ]
        );
        for (from, to) in [
            ("project", "npm:app-lib"),
            ("project", "npm:tool"),
            ("npm:app-lib", "npm:@angular/core"),
            ("npm:app-lib", "npm:rxjs"),
            ("npm:@angular/core", "npm:rxjs"),
        ] {
            assert!(has_edge(&graph, from, to), "{from} -> {to}");
        }
        assert!(node(&graph, "npm:tool").dev && node(&graph, "npm:tool").direct);
        assert!(node(&graph, "npm:app-lib").direct && !node(&graph, "npm:app-lib").dev);
        assert!(!node(&graph, "npm:rxjs").direct);
        assert_eq!(
            node(&graph, "npm:app-lib").version.as_deref(),
            Some("1.0.0")
        );
        assert_eq!(node(&graph, "npm:app-lib").declared_versions, ["^1"]);
        assert_eq!(node(&graph, "npm:app-lib").resolved_versions, ["1.0.0"]);
        assert_eq!(
            node(&graph, "npm:rxjs").resolved_versions,
            ["6.0.0", "7.0.0"]
        );
        assert_eq!(node(&graph, "npm:rxjs").declared_versions, ["*"]);
        assert!(node(&graph, "project").resolved_versions.is_empty());
        assert!(node(&graph, "project").declared_versions.is_empty());
        assert!(graph
            .notes
            .iter()
            .any(|note| note == "Packages installed in several versions are shown once."));
        let direct_only = scan(&root, &root, false).unwrap();
        assert_eq!(
            direct_only.edges,
            vec![
                ("project".to_string(), "npm:app-lib".to_string()),
                ("project".to_string(), "npm:tool".to_string())
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn python_dependencies_come_from_manifests_and_the_virtual_environment() {
        let root = temp();
        write(
            &root.join("pyproject.toml"),
            "[project]\nname = \"x\"\ndependencies = [\"requests>=2\", \"Flask[async]==3\"]\n",
        );
        write(
            &root.join(".venv/Lib/site-packages/requests-2.0.dist-info/METADATA"),
            "Metadata-Version: 2.1\nName: requests\nVersion: 2.0\nRequires-Dist: urllib3>=1\nRequires-Dist: PySocks; extra == \"socks\"\n\nbody\n",
        );
        let graph = scan(&root, &root, true).unwrap();
        assert!(has_edge(&graph, "pypi:requests", "pypi:urllib3"));
        assert!(!ids(&graph).iter().any(|id| id.contains("pysocks")));
        assert!(ids(&graph).contains(&"pypi:flask"));
        assert_eq!(
            node(&graph, "pypi:requests").version.as_deref(),
            Some("2.0")
        );
        assert_eq!(node(&graph, "pypi:requests").resolved_versions, ["2.0"]);
        assert_eq!(
            node(&graph, "pypi:requests").declared_versions,
            ["requests>=2"]
        );
        assert!(node(&graph, "pypi:flask").resolved_versions.is_empty());
        assert_eq!(
            node(&graph, "pypi:flask").declared_versions,
            ["Flask[async]==3"]
        );
        assert!(node(&graph, "pypi:urllib3").resolved_versions.is_empty());
        let direct_only = scan(&root, &root, false).unwrap();
        assert_eq!(
            node(&direct_only, "pypi:requests").resolved_versions,
            ["2.0"]
        );
        assert!(!ids(&direct_only).contains(&"pypi:urllib3"));
        assert!(!graph
            .notes
            .iter()
            .any(|note| note.contains("virtual environment")));
        let without = temp();
        write(
            &without.join("requirements-dev.txt"),
            "# comment\n-r base.txt\npytest==8\n",
        );
        let graph = scan(&without, &without, true).unwrap();
        assert!(node(&graph, "pypi:pytest").dev);
        assert!(node(&graph, "pypi:pytest").resolved_versions.is_empty());
        assert_eq!(node(&graph, "pypi:pytest").declared_versions, ["pytest==8"]);
        assert!(graph
            .notes
            .iter()
            .any(|note| note.contains("only direct Python dependencies")));
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(without).unwrap();
    }

    #[test]
    fn maven_dependencies_resolve_properties_and_the_local_repository() {
        let root = temp();
        let home = temp();
        write(
            &root.join("pom.xml"),
            "<project><properties><lib.version>1.0</lib.version></properties><dependencies>\
             <dependency><groupId>org.x</groupId><artifactId>lib</artifactId><version>${lib.version}</version></dependency>\
             <dependency><groupId>junit</groupId><artifactId>junit</artifactId><version>4.13</version><scope>test</scope></dependency>\
             </dependencies></project>",
        );
        let repository = home.join(".m2/repository");
        write(
            &repository.join("org/x/lib/1.0/lib-1.0.pom"),
            "<project><dependencies>\
             <dependency><groupId>org.y</groupId><artifactId>util</artifactId></dependency>\
             <dependency><groupId>org.z</groupId><artifactId>skip</artifactId><version>1</version><optional>true</optional></dependency>\
             </dependencies></project>",
        );
        write(
            &repository.join("org/y/util/1.9/util-1.9.pom"),
            "<project/>",
        );
        write(
            &repository.join("org/y/util/1.10/util-1.10.pom"),
            "<project/>",
        );
        let graph = scan(&root, &home, true).unwrap();
        assert!(has_edge(&graph, "maven:org.x:lib", "maven:org.y:util"));
        assert_eq!(
            node(&graph, "maven:org.y:util").version.as_deref(),
            Some("1.10")
        );
        assert!(node(&graph, "maven:org.y:util")
            .resolved_versions
            .is_empty());
        assert!(node(&graph, "maven:org.y:util")
            .declared_versions
            .is_empty());
        assert_eq!(node(&graph, "maven:org.x:lib").declared_versions, ["1.0"]);
        assert!(node(&graph, "maven:org.x:lib").resolved_versions.is_empty());
        assert_eq!(
            node(&graph, "maven:org.x:lib").version.as_deref(),
            Some("1.0")
        );
        assert!(!ids(&graph).contains(&"maven:org.z:skip"));
        assert!(node(&graph, "maven:junit:junit").dev);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn gradle_string_dependencies_are_read_and_catalog_references_are_reported() {
        let root = temp();
        write(
            &root.join("build.gradle.kts"),
            "dependencies {\n  implementation(\"org.a:one:1.2\")\n  testImplementation 'org.b:two'\n  api(libs.three)\n}\n",
        );
        let graph = scan(&root, &root, false).unwrap();
        assert_eq!(
            node(&graph, "maven:org.a:one").version.as_deref(),
            Some("1.2")
        );
        assert_eq!(node(&graph, "maven:org.a:one").declared_versions, ["1.2"]);
        assert!(node(&graph, "maven:org.b:two").declared_versions.is_empty());
        assert!(node(&graph, "maven:org.b:two").dev);
        assert!(graph
            .notes
            .iter()
            .any(|note| note.contains("version-catalog")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cargo_dependencies_use_the_lock_file() {
        let root = temp();
        write(
            &root.join("Cargo.toml"),
            "[package]\nname = \"x\"\n[dependencies]\na = \"1\"\n[dev-dependencies]\nt = \"1\"\n",
        );
        write(
            &root.join("Cargo.lock"),
            "[[package]]\nname = \"a\"\nversion = \"1.0.0\"\ndependencies = [\"b 2.0.0 (registry+x)\"]\n[[package]]\nname = \"b\"\nversion = \"2.0.0\"\n",
        );
        let graph = scan(&root, &root, true).unwrap();
        assert!(has_edge(&graph, "cargo:a", "cargo:b"));
        assert_eq!(node(&graph, "cargo:b").version.as_deref(), Some("2.0.0"));
        assert!(node(&graph, "cargo:t").dev);
        assert_eq!(node(&graph, "cargo:a").declared_versions, ["1"]);
        assert_eq!(node(&graph, "cargo:a").resolved_versions, ["1.0.0"]);
        assert_eq!(node(&graph, "cargo:b").resolved_versions, ["2.0.0"]);
        assert!(node(&graph, "cargo:t").resolved_versions.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sub_folders_are_modules_and_an_empty_folder_is_an_error() {
        let root = temp();
        write(
            &root.join("web/package.json"),
            r#"{"dependencies":{"left-pad":"1"}}"#,
        );
        write(
            &root.join("node_modules/ignored/package.json"),
            r#"{"dependencies":{"x":"1"}}"#,
        );
        let graph = scan(&root, &root, false).unwrap();
        assert!(ids(&graph).contains(&"npm:left-pad"));
        fs::remove_dir_all(&root).unwrap();
        let empty = temp();
        let error = scan(&empty, &empty, true).unwrap_err();
        assert_eq!(
            error,
            format!(
                "No dependency manifest found in {} {NO_MANIFEST}.",
                empty.display()
            )
        );
        fs::remove_dir_all(empty).unwrap();
    }

    #[test]
    fn manifest_status_is_missing_only_for_the_no_manifest_condition() {
        let root = temp();
        assert_eq!(
            manifest_status(&root, &root).unwrap(),
            ManifestStatus::Missing
        );
        write(
            &root.join("web/package.json"),
            r#"{"dependencies":{"left-pad":"1"}}"#,
        );
        assert_eq!(
            manifest_status(&root, &root).unwrap(),
            ManifestStatus::Present
        );
        // A path that cannot be scanned is an ordinary error, never "missing".
        let file = root.join("web/package.json");
        assert!(manifest_status(&file, &root)
            .unwrap_err()
            .starts_with("Not a folder"));
        assert!(manifest_status(&root.join("absent"), &root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_folder_key_is_sixteen_hex_characters_whatever_the_spelling() {
        let root = temp();
        let key = folder_key(&root);
        assert_eq!(key.len(), 16);
        assert!(key
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        let spelled = PathBuf::from(format!(
            "{}{}",
            root.display().to_string().to_uppercase().replace('\\', "/"),
            "/"
        ));
        assert_eq!(folder_key(&spelled), key);
        assert_eq!(folder_key(&root.join("..").join(file_name(&root))), key);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn evidence_vectors_serialize_camel_case_and_stay_sorted_and_unique() {
        let mut b = Builder::new();
        let id = b.node("npm", "x", None, false, false).unwrap();
        for version in ["2.0.0", "10.0.0", "2.0.0", " ", "1.5.0"] {
            b.record_resolved(&id, Some(version));
        }
        b.record_resolved(&id, None);
        b.record_declared(&id, Some("^2"));
        b.record_declared(&id, Some("^1"));
        b.record_declared(&id, Some("^2"));
        b.record_resolved("npm:absent", Some("1.0.0"));
        let node = &b.nodes[&id];
        assert_eq!(node.resolved_versions, ["1.5.0", "10.0.0", "2.0.0"]);
        assert_eq!(node.declared_versions, ["^1", "^2"]);
        let json = serde_json::to_value(node).unwrap();
        assert_eq!(
            json["resolvedVersions"],
            serde_json::json!(["1.5.0", "10.0.0", "2.0.0"])
        );
        assert_eq!(json["declaredVersions"], serde_json::json!(["^1", "^2"]));
        assert!(json.get("resolved_versions").is_none() && json.get("declared_versions").is_none());
    }

    #[test]
    fn maven_declarations_from_several_modules_are_all_kept() {
        let root = temp();
        write(
            &root.join("pom.xml"),
            "<project><modules><module>a</module><module>b</module></modules></project>",
        );
        for (module, version) in [("a", "1.0"), ("b", "2.0")] {
            write(
                &root.join(module).join("pom.xml"),
                &format!("<project><dependencies><dependency><groupId>org.x</groupId><artifactId>lib</artifactId><version>{version}</version></dependency></dependencies></project>"),
            );
        }
        let graph = scan(&root, &root, false).unwrap();
        let lib = node(&graph, "maven:org.x:lib");
        assert_eq!(lib.declared_versions, ["1.0", "2.0"]);
        assert!(lib.resolved_versions.is_empty());
        assert_eq!(lib.version.as_deref(), Some("1.0"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn npm_missing_installation_keeps_only_deduplicated_declarations() {
        let root = temp();
        write(
            &root.join("package.json"),
            r#"{"dependencies":{"missing":"^1"},"optionalDependencies":{"missing":"^1"},"devDependencies":{"missing":"~2"}}"#,
        );
        for transitive in [false, true] {
            let graph = scan(&root, &root, transitive).unwrap();
            let missing = node(&graph, "npm:missing");
            assert!(missing.resolved_versions.is_empty());
            assert_eq!(missing.declared_versions, ["^1", "~2"]);
            assert!(!missing.dev);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn python_poetry_and_requirements_preserve_declarations_and_dev_merging() {
        let root = temp();
        write(
            &root.join("pyproject.toml"),
            r#"
[tool.poetry.dependencies]
python = "^3.11"
Some_Pkg = "^2"
table-pkg = { version = "~4", optional = true }
path-pkg = { path = "../local" }
[tool.poetry.group.test.dependencies]
some-pkg = { version = ">=1" }
only-dev = "*"
"#,
        );
        write(
            &root.join("requirements-dev.txt"),
            "Some.Pkg[extra]>=2; python_version >= '3.11'\n",
        );
        write(
            &root.join(".venv/Lib/site-packages/table_pkg-4.dist-info/METADATA"),
            "Name: table-pkg\n\nNo version header.\n",
        );
        let graph = scan(&root, &root, true).unwrap();
        let merged = node(&graph, "pypi:some-pkg");
        assert_eq!(
            merged.declared_versions,
            [">=1", "Some.Pkg[extra]>=2; python_version >= '3.11'", "^2"]
        );
        assert!(merged.resolved_versions.is_empty());
        assert!(!merged.dev);
        assert!(node(&graph, "pypi:only-dev").dev);
        assert_eq!(node(&graph, "pypi:table-pkg").declared_versions, ["~4"]);
        assert!(node(&graph, "pypi:table-pkg").resolved_versions.is_empty());
        assert!(node(&graph, "pypi:path-pkg").declared_versions.is_empty());
        assert!(!ids(&graph).contains(&"pypi:python"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cargo_all_matching_lock_versions_are_evidence_without_changing_collapsed_edges() {
        let root = temp();
        write(
            &root.join("Cargo.toml"),
            "[dependencies]\nalias = { package = \"a\", version = \">=1\" }\n",
        );
        write(
            &root.join("Cargo.lock"),
            r#"
[[package]]
name = "a"
version = "2.0.0"
dependencies = ["b 3.0.0"]
[[package]]
name = "a"
version = "1.0.0"
dependencies = ["other 1.0.0"]
[[package]]
name = "a"
version = "2.0.0"
[[package]]
name = "b"
version = "3.0.0"
[[package]]
name = "b"
version = "2.0.0"
[[package]]
name = "unreachable"
version = "9.0.0"
"#,
        );
        let graph = scan(&root, &root, true).unwrap();
        assert_eq!(
            node(&graph, "cargo:a").resolved_versions,
            ["1.0.0", "2.0.0"]
        );
        assert_eq!(node(&graph, "cargo:a").declared_versions, [">=1"]);
        assert_eq!(node(&graph, "cargo:a").version.as_deref(), Some("2.0.0"));
        assert_eq!(
            node(&graph, "cargo:b").resolved_versions,
            ["2.0.0", "3.0.0"]
        );
        assert!(has_edge(&graph, "cargo:a", "cargo:b"));
        assert!(!ids(&graph).contains(&"cargo:other"));
        assert!(!ids(&graph).contains(&"cargo:unreachable"));
        let direct_only = scan(&root, &root, false).unwrap();
        assert_eq!(
            node(&direct_only, "cargo:a").resolved_versions,
            ["1.0.0", "2.0.0"]
        );
        assert!(!ids(&direct_only).contains(&"cargo:b"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gradle_literal_properties_are_declarations_not_cached_resolution() {
        let root = temp();
        let home = temp();
        write(&root.join("gradle.properties"), "releaseVersion=1.2\n");
        write(&root.join("build.gradle.kts"), "val otherVersion = \"2.3\"\ndependencies {\n implementation(\"org.a:one:$releaseVersion\")\n api(\"org.a:two:${otherVersion}\")\n runtimeOnly(\"org.a:unknown\")\n}\n");
        let repository = home.join(".m2/repository");
        write(&repository.join("org/a/one/9.0/one-9.0.pom"), "<project/>");
        write(&repository.join("org/a/unknown/9.0/unknown-9.0.pom"), "<project><dependencies><dependency><groupId>org.a</groupId><artifactId>child</artifactId><version>5.0</version></dependency></dependencies></project>");
        let graph = scan(&root, &home, true).unwrap();
        assert_eq!(node(&graph, "maven:org.a:one").declared_versions, ["1.2"]);
        assert_eq!(node(&graph, "maven:org.a:two").declared_versions, ["2.3"]);
        let unknown = node(&graph, "maven:org.a:unknown");
        assert_eq!(unknown.version.as_deref(), Some("9.0"));
        assert!(unknown.declared_versions.is_empty());
        assert!(graph
            .nodes
            .iter()
            .all(|node| node.resolved_versions.is_empty()));
        assert!(has_edge(&graph, "maven:org.a:unknown", "maven:org.a:child"));
        assert_eq!(node(&graph, "maven:org.a:child").declared_versions, ["5.0"]);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn versions_compare_numerically_and_numbers_beat_text() {
        assert_eq!(compare_versions("1.10", "1.9"), Ordering::Greater);
        assert_eq!(compare_versions("1.0", "1.0-beta"), Ordering::Less);
        assert_eq!(compare_versions("2", "2"), Ordering::Equal);
    }

    fn occ(id: &str, version: Option<&str>) -> Occurrence {
        Occurrence {
            id: id.to_string(),
            version: version.map(String::from),
        }
    }

    fn has_link(graph: &DependencyGraph, from: Option<Occurrence>, to: Occurrence) -> bool {
        graph
            .resolution
            .links
            .iter()
            .any(|(a, b)| *a == from && *b == to)
    }

    #[test]
    fn npm_nested_copies_of_different_versions_are_distinct_occurrences() {
        let root = temp();
        write(
            &root.join("package.json"),
            r#"{"dependencies":{"a":"1","b":"1"}}"#,
        );
        package(&root, "a", "1.0.0", &["shared"]);
        package(&root, "b", "1.0.0", &["shared"]);
        package(&root, "shared", "1.0.0", &["leaf"]);
        package(&root, "leaf", "1.0.0", &[]);
        write(
            &root.join("node_modules/a/node_modules/shared/package.json"),
            r#"{"name":"shared","version":"2.0.0"}"#,
        );
        let graph = scan(&root, &root, true).unwrap();
        assert!(graph.resolution.complete);
        assert!(has_link(&graph, None, occ("npm:a", Some("1.0.0"))));
        assert!(has_link(
            &graph,
            Some(occ("npm:a", Some("1.0.0"))),
            occ("npm:shared", Some("2.0.0"))
        ));
        assert!(has_link(
            &graph,
            Some(occ("npm:b", Some("1.0.0"))),
            occ("npm:shared", Some("1.0.0"))
        ));
        assert!(has_link(
            &graph,
            Some(occ("npm:shared", Some("1.0.0"))),
            occ("npm:leaf", Some("1.0.0"))
        ));
        assert!(!has_link(
            &graph,
            Some(occ("npm:shared", Some("2.0.0"))),
            occ("npm:leaf", Some("1.0.0"))
        ));
        let json = serde_json::to_value(&graph).unwrap();
        assert!(json.get("resolution").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn python_duplicate_dist_info_with_different_versions_is_unproven() {
        let root = temp();
        write(
            &root.join("pyproject.toml"),
            "[project]\nname = \"x\"\ndependencies = [\"requests\", \"flask\"]\n",
        );
        let site = root.join(".venv/Lib/site-packages");
        write(
            &site.join("requests-2.0.dist-info/METADATA"),
            "Name: requests\nVersion: 2.0\nRequires-Dist: urllib3\n\n",
        );
        write(
            &site.join("requests-2.1.dist-info/METADATA"),
            "Name: requests\nVersion: 2.1\nRequires-Dist: urllib3\n\n",
        );
        write(
            &site.join("flask-3.0.dist-info/METADATA"),
            "Name: flask\nVersion: 3.0\n\n",
        );
        write(
            &site.join("urllib3-1.0.dist-info/METADATA"),
            "Name: urllib3\nVersion: 1.0\n\n",
        );
        write(
            &site.join("urllib3-1.0b.dist-info/METADATA"),
            "Name: urllib3\nVersion: 1.0\n\n",
        );
        let graph = scan(&root, &root, true).unwrap();
        assert!(has_link(&graph, None, occ("pypi:requests", None)));
        assert!(has_link(&graph, None, occ("pypi:flask", Some("3.0"))));
        assert!(has_link(
            &graph,
            Some(occ("pypi:requests", None)),
            occ("pypi:urllib3", Some("1.0"))
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cargo_lock_entries_resolve_exact_versions_and_leave_ambiguous_names_unproven() {
        let root = temp();
        write(
            &root.join("Cargo.toml"),
            "[dependencies]\na = \"1\"\nc = \"1\"\nb = \"*\"\n",
        );
        write(
            &root.join("Cargo.lock"),
            r#"
[[package]]
name = "a"
version = "1.0.0"
dependencies = ["b 2.0.0 (registry+x)", "d"]
[[package]]
name = "b"
version = "2.0.0"
[[package]]
name = "b"
version = "3.0.0"
[[package]]
name = "c"
version = "1.0.0"
dependencies = ["b 3.0.0"]
[[package]]
name = "d"
version = "1.0.0"
dependencies = ["b"]
"#,
        );
        let graph = scan(&root, &root, true).unwrap();
        assert!(has_link(&graph, None, occ("cargo:a", Some("1.0.0"))));
        assert!(has_link(&graph, None, occ("cargo:b", None)));
        assert!(has_link(
            &graph,
            Some(occ("cargo:a", Some("1.0.0"))),
            occ("cargo:b", Some("2.0.0"))
        ));
        assert!(has_link(
            &graph,
            Some(occ("cargo:a", Some("1.0.0"))),
            occ("cargo:d", Some("1.0.0"))
        ));
        assert!(has_link(
            &graph,
            Some(occ("cargo:c", Some("1.0.0"))),
            occ("cargo:b", Some("3.0.0"))
        ));
        assert!(has_link(
            &graph,
            Some(occ("cargo:d", Some("1.0.0"))),
            occ("cargo:b", None)
        ));
        assert!(!has_link(
            &graph,
            Some(occ("cargo:d", Some("1.0.0"))),
            occ("cargo:b", Some("2.0.0"))
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maven_never_proves_versions_and_emits_no_links() {
        let root = temp();
        let home = temp();
        write(
            &root.join("pom.xml"),
            "<project><dependencies><dependency><groupId>org.x</groupId><artifactId>lib</artifactId><version>1.0</version></dependency></dependencies></project>",
        );
        write(
            &home.join(".m2/repository/org/x/lib/1.0/lib-1.0.pom"),
            "<project/>",
        );
        let graph = scan(&root, &home, true).unwrap();
        assert!(graph.resolution.links.is_empty());
        assert!(graph.resolution.complete);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn the_link_cap_truncates_and_marks_the_resolution_incomplete() {
        let mut b = Builder::new();
        b.max_links = 2;
        b.link(None, occ("npm:a", Some("1")));
        b.link(None, occ("npm:b", Some("1")));
        b.link(None, occ("npm:a", Some("1")));
        assert!(!b.truncated);
        b.link(None, occ("npm:c", Some("1")));
        assert!(b.truncated);
        assert_eq!(b.links.len(), 2);
        assert!(b
            .notes
            .iter()
            .any(|note| note == "The version graph was cut at 2 links."));
    }
}
