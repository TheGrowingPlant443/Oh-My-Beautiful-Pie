//! Dependency scanner: reads a project's manifests (npm, PyPI, Maven/Gradle, Cargo) and whatever is installed or cached
//! locally, and returns one graph of packages. Nothing is executed, nothing is downloaded.
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

pub type Result<T, E = String> = std::result::Result<T, E>;

const MAX_NODES: usize = 30_000;
const MAX_FILE: u64 = 1024 * 1024;
const MODULE_MAX_DEPTH: usize = 5;
const EXCLUDED: [&str; 9] = ["node_modules", "target", "dist", "build", "out", ".venv", "venv", "env", "__pycache__"];
const NO_MANIFEST: &str = "(looked for package.json, pom.xml, build.gradle(.kts), pyproject.toml, requirements*.txt, Cargo.toml)";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DepNode {
    pub id: String,
    pub name: String,
    pub version: Option<String>,
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
}

/// `\\?\C:\x` → `C:\x`.
fn plain(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().trim_start_matches("\\\\?\\"))
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).map(|p| plain(&p)).unwrap_or_else(|_| path.to_path_buf())
}

/// The first 16 hex characters of the SHA-256 of the canonical folder path (forward slashes, lower case).
pub fn folder_key(folder: &Path) -> String {
    let text = canonical(folder).to_string_lossy().replace('\\', "/").to_lowercase();
    let text = text.trim_end_matches('/');
    Sha256::digest(text.as_bytes())[..8].iter().map(|byte| format!("{byte:02x}")).collect()
}

struct Builder {
    nodes: BTreeMap<String, DepNode>,
    edges: BTreeSet<(String, String)>,
    notes: Vec<String>,
    truncated: bool,
}

impl Builder {
    fn note(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !self.notes.contains(&text) {
            self.notes.push(text);
        }
    }

    /// Adds or merges a node. `dev` only describes direct declarations; a package is dev only if every direct
    /// declaration of it is.
    fn node(&mut self, ecosystem: &'static str, name: &str, version: Option<String>, direct: bool, dev: bool) -> Option<String> {
        let id = format!("{ecosystem}:{name}");
        let version = version.filter(|v| !v.is_empty());
        if let Some(existing) = self.nodes.get_mut(&id) {
            if direct {
                existing.dev = if existing.direct { existing.dev && dev } else { dev };
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
        let node = DepNode { id: id.clone(), name: name.to_string(), version, ecosystem, direct, dev: direct && dev };
        self.nodes.insert(id.clone(), node);
        Some(id)
    }

    fn edge(&mut self, from: &str, to: &str) {
        self.edges.insert((from.to_string(), to.to_string()));
    }

    /// A dependency declared by the project itself: a node plus an edge from the root.
    fn direct(&mut self, ecosystem: &'static str, name: &str, version: Option<String>, dev: bool) -> Option<String> {
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
    path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Scan `folder` and its immediate sub-folders. `transitive == false` stops at the declared dependencies.
pub fn scan(folder: &Path, home: &Path, transitive: bool) -> Result<DependencyGraph> {
    if !folder.is_dir() {
        return Err(format!("Not a folder: {}", folder.display()));
    }
    let root = canonical(folder);
    let mut b = Builder { nodes: BTreeMap::new(), edges: BTreeSet::new(), notes: Vec::new(), truncated: false };
    b.nodes.insert(
        "project".into(),
        DepNode { id: "project".into(), name: file_name(&root), version: None, ecosystem: "project", direct: false, dev: false },
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
        return Err(format!("No dependency manifest found in {} {NO_MANIFEST}.", folder.display()));
    }
    let edges = b.edges.iter().filter(|(from, to)| b.nodes.contains_key(from) && b.nodes.contains_key(to)).cloned().collect();
    Ok(DependencyGraph {
        folder: root.display().to_string(),
        key: folder_key(&root),
        nodes: b.nodes.into_values().collect(),
        edges,
        notes: b.notes,
        truncated: b.truncated,
    })
}

// ---- npm -------------------------------------------------------------------------------------------------------

fn json_keys(value: &serde_json::Value, key: &str) -> Vec<(String, String)> {
    value
        .get(key)
        .and_then(|v| v.as_object())
        .map(|map| map.iter().map(|(name, range)| (name.clone(), range.as_str().unwrap_or("").to_string())).collect())
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

fn scan_npm(module: &Path, transitive: bool, b: &mut Builder) -> bool {
    let manifest = module.join("package.json");
    if !manifest.is_file() {
        return false;
    }
    let Some(json) = read_json(&manifest) else { return true };
    let modules_dir = module.join("node_modules");
    let mut queue: VecDeque<(String, PathBuf)> = VecDeque::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    for (key, dev) in [("dependencies", false), ("optionalDependencies", false), ("devDependencies", true)] {
        for (name, range) in json_keys(&json, key) {
            let installed = resolve_npm(module, module, &name);
            let version = installed
                .as_ref()
                .and_then(|dir| read_json(&dir.join("package.json")))
                .and_then(|pkg| pkg["version"].as_str().map(String::from))
                .or(Some(range));
            let Some(id) = b.direct("npm", &name, version, dev) else { continue };
            if transitive {
                if let Some(dir) = installed {
                    let dir = canonical(&dir);
                    if visited.insert(dir.clone()) {
                        queue.push_back((id, dir));
                    }
                }
            }
        }
    }
    if !transitive {
        return true;
    }
    if !modules_dir.is_dir() {
        b.note(format!(
            "node_modules was not found in {}: only direct npm dependencies are shown. Install the packages to see transitive dependencies.",
            module.display()
        ));
        return true;
    }
    let stop = canonical(module);
    while let Some((from, dir)) = queue.pop_front() {
        let Some(pkg) = read_json(&dir.join("package.json")) else { continue };
        let names: Vec<String> = json_keys(&pkg, "dependencies").into_iter().chain(json_keys(&pkg, "optionalDependencies")).map(|(name, _)| name).collect();
        for name in names {
            let Some(found) = resolve_npm(&dir, &stop, &name) else { continue };
            let dep_dir = canonical(&found);
            let version = read_json(&dep_dir.join("package.json")).and_then(|p| p["version"].as_str().map(String::from));
            let known = b.nodes.get(&format!("npm:{name}")).and_then(|node| node.version.clone());
            if known.is_some() && version.is_some() && known != version {
                b.note("Packages installed in several versions are shown once.");
            }
            let Some(id) = b.node("npm", &name, version, false, false) else { continue };
            b.edge(&from, &id);
            if visited.insert(dep_dir.clone()) {
                queue.push_back((id, dep_dir));
            }
        }
    }
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
    if !spec.chars().next().is_some_and(|ch| ch.is_ascii_alphanumeric()) {
        return None;
    }
    let end = spec.find(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))).unwrap_or(spec.len());
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
}

fn installed_python(site: &Path) -> HashMap<String, Installed> {
    let mut map = HashMap::new();
    for dir in subdirs(site) {
        if !file_name(&dir).ends_with(".dist-info") {
            continue;
        }
        let Some(text) = read_text(&dir.join("METADATA")) else { continue };
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
        if let Some(name) = name {
            map.insert(name, Installed { version, requires });
        }
    }
    map
}

fn pyproject_dependencies(table: &toml::Table, found: &mut BTreeMap<String, bool>) {
    let mut add = |name: Option<String>, dev: bool| {
        if let Some(name) = name {
            found.entry(name).and_modify(|existing| *existing = *existing && dev).or_insert(dev);
        }
    };
    if let Some(project) = table.get("project").and_then(|v| v.as_table()) {
        for spec in project.get("dependencies").and_then(|v| v.as_array()).into_iter().flatten() {
            add(spec.as_str().and_then(requirement_name), false);
        }
        for group in project.get("optional-dependencies").and_then(|v| v.as_table()).into_iter().flat_map(|t| t.values()) {
            for spec in group.as_array().into_iter().flatten() {
                add(spec.as_str().and_then(requirement_name), true);
            }
        }
    }
    if let Some(poetry) = table.get("tool").and_then(|v| v.get("poetry")).and_then(|v| v.as_table()) {
        let names = |t: &toml::Table| -> Vec<String> { t.keys().filter(|key| key.as_str() != "python").map(|key| normalize_python(key)).collect() };
        if let Some(deps) = poetry.get("dependencies").and_then(|v| v.as_table()) {
            for name in names(deps) {
                add(Some(name), false);
            }
        }
        if let Some(deps) = poetry.get("dev-dependencies").and_then(|v| v.as_table()) {
            for name in names(deps) {
                add(Some(name), true);
            }
        }
        for group in poetry.get("group").and_then(|v| v.as_table()).into_iter().flat_map(|t| t.values()) {
            if let Some(deps) = group.get("dependencies").and_then(|v| v.as_table()) {
                for name in names(deps) {
                    add(Some(name), true);
                }
            }
        }
    }
}

fn scan_pypi(module: &Path, root: &Path, transitive: bool, b: &mut Builder) -> bool {
    let mut found = false;
    let mut direct: BTreeMap<String, bool> = BTreeMap::new();
    if let Some(text) = read_text(&module.join("pyproject.toml")) {
        found = true;
        if let Ok(table) = text.parse::<toml::Table>() {
            pyproject_dependencies(&table, &mut direct);
        }
    }
    let mut requirement_files: Vec<PathBuf> = fs::read_dir(module)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = file_name(path).to_lowercase();
            path.is_file() && name.starts_with("requirements") && name.ends_with(".txt")
        })
        .collect();
    requirement_files.sort();
    for path in requirement_files {
        found = true;
        let name = file_name(&path).to_lowercase();
        let dev = name.contains("dev") || name.contains("test");
        for line in read_text(&path).unwrap_or_default().lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('-') || line.contains("://") {
                continue;
            }
            if let Some(name) = requirement_name(line) {
                direct.entry(name).and_modify(|existing| *existing = *existing && dev).or_insert(dev);
            }
        }
    }
    if !found {
        return false;
    }
    let site = if transitive { find_site_packages(module, root) } else { None };
    let installed = site.as_deref().map(installed_python).unwrap_or_default();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    for (name, dev) in &direct {
        let version = installed.get(name).and_then(|pkg| pkg.version.clone());
        if b.direct("pypi", name, version, *dev).is_some() && visited.insert(name.clone()) {
            queue.push_back(name.clone());
        }
    }
    if transitive && site.is_none() {
        b.note("No virtual environment (.venv, venv, env) was found: only direct Python dependencies are shown.");
    }
    while let Some(name) = queue.pop_front() {
        let Some(package) = installed.get(&name) else { continue };
        let from = format!("pypi:{name}");
        for required in &package.requires {
            let version = installed.get(required).and_then(|pkg| pkg.version.clone());
            let Some(id) = b.node("pypi", required, version, false, false) else { continue };
            b.edge(&from, &id);
            if visited.insert(required.clone()) {
                queue.push_back(required.clone());
            }
        }
    }
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

fn child<'a, 'input>(node: roxmltree::Node<'a, 'input>, name: &str) -> Option<roxmltree::Node<'a, 'input>> {
    node.children().find(|candidate| candidate.is_element() && candidate.tag_name().name() == name)
}

fn child_text(node: roxmltree::Node, name: &str) -> Option<String> {
    child(node, name).and_then(|n| n.text()).map(|text| text.trim().to_string()).filter(|text| !text.is_empty())
}

/// `${name}` references resolved from `properties`; `None` while anything stays unresolved.
fn interpolate(value: &str, properties: &HashMap<String, String>) -> Option<String> {
    let mut text = value.trim().to_string();
    for _ in 0..5 {
        let Some(start) = text.find("${") else { return Some(text).filter(|t| !t.is_empty()) };
        let end = text[start..].find('}')? + start;
        let replacement = properties.get(&text[start + 2..end])?.clone();
        text.replace_range(start..=end, &replacement);
    }
    None
}

fn pom_properties(project: roxmltree::Node) -> HashMap<String, String> {
    let mut properties = HashMap::new();
    if let Some(list) = child(project, "properties") {
        for property in list.children().filter(|n| n.is_element()) {
            if let Some(text) = property.text() {
                properties.insert(property.tag_name().name().to_string(), text.trim().to_string());
            }
        }
    }
    let parent = child(project, "parent");
    let version = child_text(project, "version").or_else(|| parent.and_then(|p| child_text(p, "version")));
    let group = child_text(project, "groupId").or_else(|| parent.and_then(|p| child_text(p, "groupId")));
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
    let Ok(document) = roxmltree::Document::parse(text) else { return Vec::new() };
    let project = document.root_element();
    let properties = pom_properties(project);
    let managed: HashMap<String, String> = child(project, "dependencyManagement")
        .and_then(|m| child(m, "dependencies"))
        .into_iter()
        .flat_map(|list| list.children().filter(|n| n.is_element()))
        .filter_map(|dep| {
            let group = interpolate(&child_text(dep, "groupId")?, &properties)?;
            let artifact = interpolate(&child_text(dep, "artifactId")?, &properties)?;
            Some((format!("{group}:{artifact}"), interpolate(&child_text(dep, "version")?, &properties)?))
        })
        .collect();
    child(project, "dependencies")
        .into_iter()
        .flat_map(|list| list.children().filter(|n| n.is_element() && n.tag_name().name() == "dependency"))
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
    let Ok(document) = roxmltree::Document::parse(text) else { return Vec::new() };
    child(document.root_element(), "modules")
        .into_iter()
        .flat_map(|list| list.children().filter(|n| n.is_element() && n.tag_name().name() == "module"))
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

/// `(group, artifact, version, dev)` of every string-notation dependency; reports whether a version-catalog
/// reference (`libs.x`) was seen.
fn gradle_dependencies(text: &str) -> (Vec<(String, String, Option<String>, bool)>, bool) {
    let mut found = Vec::new();
    let mut catalog = false;
    for line in text.lines() {
        let line = line.trim();
        let configuration = GRADLE_CONFIGURATIONS.iter().find(|keyword| {
            line.strip_prefix(**keyword).is_some_and(|rest| rest.starts_with(|c: char| c.is_whitespace() || c == '('))
        });
        let Some(configuration) = configuration else { continue };
        let rest = line[configuration.len()..].trim_start_matches(|c: char| c == '(' || c.is_whitespace());
        if rest.starts_with("libs.") {
            catalog = true;
            continue;
        }
        let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else { continue };
        let Some(end) = rest[1..].find(quote) else { continue };
        let mut parts = rest[1..1 + end].split(':');
        let (Some(group), Some(artifact)) = (parts.next(), parts.next()) else { continue };
        if group.is_empty() || artifact.is_empty() {
            continue;
        }
        let version = parts.next().filter(|v| !v.is_empty() && !v.contains('$')).map(String::from);
        found.push((group.to_string(), artifact.to_string(), version, configuration.starts_with("test")));
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

fn pom_file(root: &Path, layout: Layout, group: &str, artifact: &str, version: &str) -> Option<PathBuf> {
    let file = format!("{artifact}-{version}.pom");
    match layout {
        Layout::Maven => {
            let path = root.join(group.replace('.', "/")).join(artifact).join(version).join(&file);
            path.is_file().then_some(path)
        }
        Layout::Gradle => subdirs(&root.join(group).join(artifact).join(version)).into_iter().map(|hash| hash.join(&file)).find(|p| p.is_file()),
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
fn find_pom(roots: &[(PathBuf, Layout)], group: &str, artifact: &str, version: Option<&str>) -> Option<(PathBuf, String)> {
    if let Some(version) = version {
        return roots.iter().find_map(|(root, layout)| pom_file(root, *layout, group, artifact, version).map(|path| (path, version.to_string())));
    }
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    for (root, layout) in roots {
        for version in pom_versions(root, *layout, group, artifact) {
            if let Some(path) = pom_file(root, *layout, group, artifact, &version) {
                candidates.push((version, path));
            }
        }
    }
    candidates.into_iter().max_by(|a, b| compare_versions(&a.0, &b.0)).map(|(version, path)| (path, version))
}

fn collect_poms(dir: &Path, depth: usize, visited: &mut HashSet<PathBuf>, seeds: &mut Vec<(String, String, Option<String>, bool)>, found: &mut bool) {
    let dir = canonical(dir);
    if depth > MODULE_MAX_DEPTH || !visited.insert(dir.clone()) {
        return;
    }
    let Some(text) = read_text(&dir.join("pom.xml")) else { return };
    *found = true;
    for dep in pom_dependencies(&text) {
        seeds.push((dep.group, dep.artifact, dep.version, dep.scope.as_deref() == Some("test")));
    }
    for module in pom_modules(&text) {
        collect_poms(&dir.join(module), depth + 1, visited, seeds, found);
    }
}

fn scan_java(module: &Path, home: &Path, transitive: bool, b: &mut Builder) -> bool {
    let mut seeds: Vec<(String, String, Option<String>, bool)> = Vec::new();
    let mut found = false;
    collect_poms(module, 0, &mut HashSet::new(), &mut seeds, &mut found);
    let mut catalog = false;
    for name in ["build.gradle", "build.gradle.kts"] {
        if let Some(text) = read_text(&module.join(name)) {
            found = true;
            let (list, saw_catalog) = gradle_dependencies(&text);
            seeds.extend(list);
            catalog |= saw_catalog;
        }
    }
    if !found {
        return false;
    }
    if catalog {
        b.note("Gradle version-catalog dependencies (libs.*) are not read.");
    }
    b.note("Dependencies inherited from parent POMs are not followed.");
    let mut queue: VecDeque<(String, String, Option<String>)> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    for (group, artifact, version, dev) in seeds {
        let name = format!("{group}:{artifact}");
        if b.direct("maven", &name, version.clone(), dev).is_some() && visited.insert(name) {
            queue.push_back((group, artifact, version));
        }
    }
    if !transitive {
        return true;
    }
    let roots: Vec<(PathBuf, Layout)> = [
        (home.join(".m2").join("repository"), Layout::Maven),
        (home.join(".gradle").join("caches").join("modules-2").join("files-2.1"), Layout::Gradle),
    ]
    .into_iter()
    .filter(|(path, _)| path.is_dir())
    .collect();
    if roots.is_empty() {
        b.note("No local Maven or Gradle repository was found: only direct Java dependencies are shown.");
        return true;
    }
    while let Some((group, artifact, version)) = queue.pop_front() {
        let Some((pom, used)) = find_pom(&roots, &group, &artifact, version.as_deref()) else { continue };
        let from = format!("maven:{group}:{artifact}");
        b.node("maven", &format!("{group}:{artifact}"), Some(used), false, false);
        let Some(text) = read_text(&pom) else { continue };
        for dep in pom_dependencies(&text) {
            if dep.optional || matches!(dep.scope.as_deref(), Some("test" | "provided" | "system")) {
                continue;
            }
            let name = format!("{}:{}", dep.group, dep.artifact);
            let Some(id) = b.node("maven", &name, dep.version.clone(), false, false) else { continue };
            b.edge(&from, &id);
            if visited.insert(name) {
                queue.push_back((dep.group, dep.artifact, dep.version));
            }
        }
    }
    true
}

// ---- Cargo -----------------------------------------------------------------------------------------------------

fn scan_cargo(module: &Path, root: &Path, transitive: bool, b: &mut Builder) -> bool {
    let Some(text) = read_text(&module.join("Cargo.toml")) else { return false };
    let Ok(manifest) = text.parse::<toml::Table>() else { return true };
    let mut direct: Vec<String> = Vec::new();
    for (key, dev) in [("dependencies", false), ("build-dependencies", false), ("dev-dependencies", true)] {
        for (name, spec) in manifest.get(key).and_then(|v| v.as_table()).into_iter().flatten() {
            let name = spec.get("package").and_then(|v| v.as_str()).unwrap_or(name).to_string();
            if b.direct("cargo", &name, None, dev).is_some() {
                direct.push(name);
            }
        }
    }
    if !transitive {
        return true;
    }
    let lock = [module, root].iter().find_map(|dir| read_text(&dir.join("Cargo.lock")));
    let Some(lock) = lock.and_then(|text| text.parse::<toml::Table>().ok()) else {
        b.note("No Cargo.lock was found: only direct Rust dependencies are shown.");
        return true;
    };
    let mut packages: HashMap<String, (String, Vec<String>)> = HashMap::new();
    for package in lock.get("package").and_then(|v| v.as_array()).into_iter().flatten() {
        let Some(name) = package.get("name").and_then(|v| v.as_str()) else { continue };
        let version = package.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let dependencies = package
            .get("dependencies")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|dep| dep.as_str().and_then(|text| text.split(' ').next()).map(String::from))
            .collect();
        packages.entry(name.to_string()).or_insert((version, dependencies));
    }
    let mut queue: VecDeque<String> = direct.iter().cloned().collect();
    let mut visited: HashSet<String> = direct.into_iter().collect();
    while let Some(name) = queue.pop_front() {
        let Some((version, dependencies)) = packages.get(&name) else { continue };
        let from = format!("cargo:{name}");
        b.node("cargo", &name, Some(version.clone()), false, false);
        for dependency in dependencies {
            let version = packages.get(dependency).map(|(version, _)| version.clone());
            let Some(id) = b.node("cargo", dependency, version, false, false) else { continue };
            b.edge(&from, &id);
            if visited.insert(dependency.clone()) {
                queue.push_back(dependency.clone());
            }
        }
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
        let deps: Vec<String> = dependencies.iter().map(|d| format!("\"{d}\": \"*\"")).collect();
        write(
            &root.join("node_modules").join(name).join("package.json"),
            &format!("{{\"name\":\"{name}\",\"version\":\"{version}\",\"dependencies\":{{{}}}}}", deps.join(",")),
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
        write(&root.join("package.json"), r#"{"dependencies":{"app-lib":"^1"},"devDependencies":{"tool":"^2"}}"#);
        package(&root, "app-lib", "1.0.0", &["@angular/core", "rxjs"]);
        package(&root, "@angular/core", "17.0.0", &["rxjs"]);
        package(&root, "tool", "2.0.0", &[]);
        package(&root, "rxjs", "7.0.0", &[]);
        write(&root.join("node_modules/app-lib/node_modules/rxjs/package.json"), r#"{"name":"rxjs","version":"6.0.0"}"#);
        let graph = scan(&root, &root, true).unwrap();
        assert_eq!(ids(&graph), ["npm:@angular/core", "npm:app-lib", "npm:rxjs", "npm:tool", "project"]);
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
        assert_eq!(node(&graph, "npm:app-lib").version.as_deref(), Some("1.0.0"));
        assert!(graph.notes.iter().any(|note| note == "Packages installed in several versions are shown once."));
        let direct_only = scan(&root, &root, false).unwrap();
        assert_eq!(direct_only.edges, vec![("project".to_string(), "npm:app-lib".to_string()), ("project".to_string(), "npm:tool".to_string())]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn python_dependencies_come_from_manifests_and_the_virtual_environment() {
        let root = temp();
        write(&root.join("pyproject.toml"), "[project]\nname = \"x\"\ndependencies = [\"requests>=2\", \"Flask[async]==3\"]\n");
        write(
            &root.join(".venv/Lib/site-packages/requests-2.0.dist-info/METADATA"),
            "Metadata-Version: 2.1\nName: requests\nVersion: 2.0\nRequires-Dist: urllib3>=1\nRequires-Dist: PySocks; extra == \"socks\"\n\nbody\n",
        );
        let graph = scan(&root, &root, true).unwrap();
        assert!(has_edge(&graph, "pypi:requests", "pypi:urllib3"));
        assert!(!ids(&graph).iter().any(|id| id.contains("pysocks")));
        assert!(ids(&graph).contains(&"pypi:flask"));
        assert_eq!(node(&graph, "pypi:requests").version.as_deref(), Some("2.0"));
        assert!(!graph.notes.iter().any(|note| note.contains("virtual environment")));
        let without = temp();
        write(&without.join("requirements-dev.txt"), "# comment\n-r base.txt\npytest==8\n");
        let graph = scan(&without, &without, true).unwrap();
        assert!(node(&graph, "pypi:pytest").dev);
        assert!(graph.notes.iter().any(|note| note.contains("only direct Python dependencies")));
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
        write(&repository.join("org/y/util/1.9/util-1.9.pom"), "<project/>");
        write(&repository.join("org/y/util/1.10/util-1.10.pom"), "<project/>");
        let graph = scan(&root, &home, true).unwrap();
        assert!(has_edge(&graph, "maven:org.x:lib", "maven:org.y:util"));
        assert_eq!(node(&graph, "maven:org.y:util").version.as_deref(), Some("1.10"));
        assert_eq!(node(&graph, "maven:org.x:lib").version.as_deref(), Some("1.0"));
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
        assert_eq!(node(&graph, "maven:org.a:one").version.as_deref(), Some("1.2"));
        assert!(node(&graph, "maven:org.b:two").dev);
        assert!(graph.notes.iter().any(|note| note.contains("version-catalog")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cargo_dependencies_use_the_lock_file() {
        let root = temp();
        write(&root.join("Cargo.toml"), "[package]\nname = \"x\"\n[dependencies]\na = \"1\"\n[dev-dependencies]\nt = \"1\"\n");
        write(
            &root.join("Cargo.lock"),
            "[[package]]\nname = \"a\"\nversion = \"1.0.0\"\ndependencies = [\"b 2.0.0 (registry+x)\"]\n[[package]]\nname = \"b\"\nversion = \"2.0.0\"\n",
        );
        let graph = scan(&root, &root, true).unwrap();
        assert!(has_edge(&graph, "cargo:a", "cargo:b"));
        assert_eq!(node(&graph, "cargo:b").version.as_deref(), Some("2.0.0"));
        assert!(node(&graph, "cargo:t").dev);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sub_folders_are_modules_and_an_empty_folder_is_an_error() {
        let root = temp();
        write(&root.join("web/package.json"), r#"{"dependencies":{"left-pad":"1"}}"#);
        write(&root.join("node_modules/ignored/package.json"), r#"{"dependencies":{"x":"1"}}"#);
        let graph = scan(&root, &root, false).unwrap();
        assert!(ids(&graph).contains(&"npm:left-pad"));
        fs::remove_dir_all(&root).unwrap();
        let empty = temp();
        let error = scan(&empty, &empty, true).unwrap_err();
        assert_eq!(error, format!("No dependency manifest found in {} {NO_MANIFEST}.", empty.display()));
        fs::remove_dir_all(empty).unwrap();
    }

    #[test]
    fn the_folder_key_is_sixteen_hex_characters_whatever_the_spelling() {
        let root = temp();
        let key = folder_key(&root);
        assert_eq!(key.len(), 16);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        let spelled = PathBuf::from(format!("{}{}", root.display().to_string().to_uppercase().replace('\\', "/"), "/"));
        assert_eq!(folder_key(&spelled), key);
        assert_eq!(folder_key(&root.join("..").join(file_name(&root))), key);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn versions_compare_numerically_and_numbers_beat_text() {
        assert_eq!(compare_versions("1.10", "1.9"), Ordering::Greater);
        assert_eq!(compare_versions("1.0", "1.0-beta"), Ordering::Less);
        assert_eq!(compare_versions("2", "2"), Ordering::Equal);
    }
}
