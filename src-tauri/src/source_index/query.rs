//! Bounded, paged queries over a validated [`SourceGraphIndex`]. Aggregations never drop facts: group rows carry
//! totals, truncated member/link lists say so, and every aggregated arrow keeps the ids of its underlying edges.
use super::model::{Node, Resolution, Result, SourceGraphIndex, SourceSite};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};

pub(crate) const MAX_PAGE_BYTES: usize = 1024 * 1024;
const MAX_LIMIT: usize = 200;
const MAX_TEXT: usize = 512;
const MEMBER_SAMPLE: usize = 200;
const LINK_SAMPLE: usize = 500;
const EDGE_SAMPLE: usize = 20;
const SITE_SAMPLE: usize = 10;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Level { Files, Functions }

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Direction { In, Out, Both }

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum GroupBy { Directory, Scc }

/// The strict query union of the System overview page.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum SourceIndexQuery {
    Search { level: Level, text: String, offset: usize, limit: usize },
    Neighbors { level: Level, id: String, direction: Direction, offset: usize, limit: usize },
    Files { offset: usize, limit: usize },
    Symbols { offset: usize, limit: usize },
    Coverage { offset: usize, limit: usize },
    Overview { level: Level, #[serde(rename = "groupBy")] group_by: GroupBy, offset: usize, limit: usize },
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SourceIndexPage {
    pub total: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub items: Vec<Value>,
}

fn val<T: Serialize>(value: &T) -> Value { serde_json::to_value(value).unwrap_or(Value::Null) }

fn window(offset: usize, limit: usize) -> Result<()> {
    if !(1..=MAX_LIMIT).contains(&limit) { return Err(format!("Query limit must be between 1 and {MAX_LIMIT}")); }
    if offset > 100_000_000 { return Err("Query offset is out of range".into()); }
    Ok(())
}

fn page(total: usize, offset: usize, limit: usize, make: impl Fn(usize) -> Value) -> Result<SourceIndexPage> {
    window(offset, limit)?;
    let start = offset.min(total);
    let end = (start + limit).min(total);
    let items = (start..end).map(make).collect();
    finish(SourceIndexPage { total, offset, next_offset: (end < total).then_some(end), items })
}

fn finish(page: SourceIndexPage) -> Result<SourceIndexPage> {
    let size = serde_json::to_vec(&page).map_err(|e| e.to_string())?.len();
    if size > MAX_PAGE_BYTES { return Err(format!("Query result is {size} bytes, over the 1 MiB page limit; request a smaller limit.")); }
    Ok(page)
}

fn rows(items: Vec<Value>, offset: usize, limit: usize) -> Result<SourceIndexPage> {
    let total = items.len();
    page(total, offset, limit, |i| items[i].clone())
}

pub(crate) fn run(index: &SourceGraphIndex, query: &SourceIndexQuery) -> Result<SourceIndexPage> {
    match query {
        SourceIndexQuery::Files { offset, limit } => page(index.files.len(), *offset, *limit, |i| val(&index.files[i])),
        SourceIndexQuery::Symbols { offset, limit } => page(index.symbols.len(), *offset, *limit, |i| val(&index.symbols[i])),
        SourceIndexQuery::Coverage { offset, limit } => { window(*offset, *limit)?; rows(coverage_rows(index), *offset, *limit) }
        SourceIndexQuery::Search { level, text, offset, limit } => { window(*offset, *limit)?; search(index, *level, text, *offset, *limit) }
        SourceIndexQuery::Neighbors { level, id, direction, offset, limit } => { window(*offset, *limit)?; rows(neighbors(index, *level, id, *direction)?, *offset, *limit) }
        SourceIndexQuery::Overview { level, group_by, offset, limit } => {
            window(*offset, *limit)?;
            let overview = overview(index, *level, *group_by);
            page(overview.groups.len(), *offset, *limit, |i| group_json(index, &overview, i))
        }
    }
}

// ---- coverage --------------------------------------------------------------------------------------------------

fn coverage_rows(index: &SourceGraphIndex) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut summary = index.coverage.as_object().cloned().unwrap_or_default();
    let languages = summary.remove("languages");
    summary.insert("kind".into(), json!("summary"));
    rows.push(Value::Object(summary));
    for language in languages.and_then(|l| l.as_array().cloned()).unwrap_or_default() {
        let mut row = language.as_object().cloned().unwrap_or_default();
        row.insert("kind".into(), json!("language"));
        rows.push(Value::Object(row));
    }
    for file in &index.files {
        if file.status != super::model::FileStatus::Parsed || !file.diagnostics.is_empty() {
            rows.push(json!({ "kind": "file", "fileId": file.id, "path": file.path, "language": file.language, "status": file.status, "diagnostics": file.diagnostics }));
        }
    }
    for text in &index.limitations { rows.push(json!({ "kind": "limitation", "text": text })); }
    rows
}

// ---- search ----------------------------------------------------------------------------------------------------

fn rank(haystacks: &[&str], exact: &[&str], needle: &str, lowered: &str) -> Option<u8> {
    if needle.is_empty() { return Some(2); }
    if exact.iter().any(|h| *h == needle) { return Some(0); }
    if haystacks.iter().any(|h| h.contains(needle)) { return Some(1); }
    haystacks.iter().any(|h| h.to_lowercase().contains(lowered)).then_some(2)
}

fn search(index: &SourceGraphIndex, level: Level, text: &str, offset: usize, limit: usize) -> Result<SourceIndexPage> {
    if text.len() > MAX_TEXT { return Err(format!("Search text exceeds {MAX_TEXT} bytes")); }
    let lowered = text.to_lowercase();
    // Matching is case-insensitive for finding, but exact-case hits rank first and every result carries its exact path.
    let mut hits: Vec<(u8, usize)> = match level {
        Level::Files => index.files.iter().enumerate().filter_map(|(i, f)| rank(&[f.path.as_str()], &[f.path.as_str()], text, &lowered).map(|r| (r, i))).collect(),
        Level::Functions => index.symbols.iter().enumerate().filter_map(|(i, s)| rank(&[s.name.as_str(), s.qualified_name.as_str(), s.path.as_str()], &[s.name.as_str(), s.qualified_name.as_str()], text, &lowered).map(|r| (r, i))).collect(),
    };
    if level == Level::Functions {
        hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| {
            let (x, y) = (&index.symbols[a.1], &index.symbols[b.1]);
            (&x.path, x.span.byte_start, &x.id).cmp(&(&y.path, y.span.byte_start, &y.id))
        }));
    } else {
        hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| index.files[a.1].path.cmp(&index.files[b.1].path)));
    }
    page(hits.len(), offset, limit, |i| match level { Level::Files => val(&index.files[hits[i].1]), Level::Functions => val(&index.symbols[hits[i].1]) })
}

// ---- neighbors -------------------------------------------------------------------------------------------------

fn node_label(index: &SourceGraphIndex, id: &str) -> Value {
    match index.lookup.node(id) {
        Some(Node::File(i)) => json!(index.files[i as usize].path),
        Some(Node::Symbol(i)) => { let s = &index.symbols[i as usize]; json!(if s.qualified_name.is_empty() { &s.name } else { &s.qualified_name }) }
        None => Value::Null,
    }
}

fn site_values(index: &SourceGraphIndex, ids: &[String]) -> (Vec<Value>, bool) {
    (ids.iter().take(SITE_SAMPLE).filter_map(|id| index.site(id)).map(val).collect(), ids.len() > SITE_SAMPLE)
}

fn file_of_node(index: &SourceGraphIndex, node: Node) -> Option<u32> {
    match node { Node::File(i) => Some(i), Node::Symbol(i) => index.lookup.files.get(&index.symbols[i as usize].file_id).copied() }
}

type FileGroups = BTreeMap<(String, String, &'static str, &'static str), Vec<u32>>;

fn neighbors(index: &SourceGraphIndex, level: Level, id: &str, direction: Direction) -> Result<Vec<Value>> {
    let wanted = match (level, index.lookup.node(id)) {
        (Level::Files, Some(node @ Node::File(_))) | (Level::Functions, Some(node @ Node::Symbol(_))) => node,
        _ => return Err(format!("Unknown {} id", if level == Level::Files { "file" } else { "function" })),
    };
    let (outgoing, incoming) = (direction != Direction::In, direction != Direction::Out);
    let mut items = Vec::new();
    push_direct_edges(index, id, outgoing, incoming, &mut items);
    if let Node::File(file) = wanted {
        push_file_aggregates(index, id, file, outgoing, incoming, &mut items);
        if outgoing { push_file_sites(index, id, &mut items); }
    } else if outgoing {
        push_caller_sites(index, id, &mut items);
    }
    Ok(items)
}

fn push_direct_edge(index: &SourceGraphIndex, items: &mut Vec<Value>, edge_index: u32, dir: &str) {
    let edge = &index.edges[edge_index as usize];
    let peer = if dir == "out" { &edge.target_id } else { &edge.source_id };
    let (sites, truncated) = site_values(index, &edge.evidence_ids);
    items.push(json!({ "kind": "edge", "direction": dir, "self": edge.source_id == edge.target_id, "peerId": peer, "peerLabel": node_label(index, peer), "edge": val(edge), "sites": sites, "sitesTruncated": truncated }));
}

fn push_direct_edges(index: &SourceGraphIndex, id: &str, outgoing: bool, incoming: bool, items: &mut Vec<Value>) {
    if outgoing { for e in index.lookup.edges_out.get(id).into_iter().flatten() { push_direct_edge(index, items, *e, "out"); } }
    if incoming {
        // A self edge was already reported as outgoing when both directions are wanted.
        for e in index.lookup.edges_in.get(id).into_iter().flatten() { if index.edges[*e as usize].source_id != id || !outgoing { push_direct_edge(index, items, *e, "in"); } }
    }
}

/// Cross-file call/reference aggregates derived from function-level edges, labelled separately.
fn push_file_aggregates(index: &SourceGraphIndex, id: &str, file: u32, outgoing: bool, incoming: bool, items: &mut Vec<Value>) {
    let mut groups = FileGroups::new();
    for symbol in index.lookup.symbols_by_file.get(id).into_iter().flatten() {
        let symbol_id = &index.symbols[*symbol as usize].id;
        if outgoing { collect_file_edges(index, &mut groups, file, index.lookup.edges_out.get(symbol_id), "out"); }
        if incoming { collect_file_edges(index, &mut groups, file, index.lookup.edges_in.get(symbol_id), "in"); }
    }
    for ((path, relation, resolution, dir), edges) in groups {
        let peer_id = index.lookup.files_by_path.get(&path).map(|i| index.files[*i as usize].id.clone());
        items.push(json!({ "kind": "aggregate", "direction": dir, "peerId": peer_id, "peerLabel": path, "relation": relation, "resolution": resolution, "count": edges.len(),
            "edgeIds": edges.iter().take(EDGE_SAMPLE).map(|e| &index.edges[*e as usize].id).collect::<Vec<_>>(), "edgeIdsTruncated": edges.len() > EDGE_SAMPLE }));
    }
}

fn collect_file_edges(index: &SourceGraphIndex, groups: &mut FileGroups, file: u32, edges: Option<&Vec<u32>>, dir: &'static str) {
    for e in edges.into_iter().flatten() {
        let edge = &index.edges[*e as usize];
        let peer = if dir == "out" { &edge.target_id } else { &edge.source_id };
        let Some(peer_node @ Node::Symbol(_)) = index.lookup.node(peer) else { continue };
        let Some(peer_file) = file_of_node(index, peer_node) else { continue };
        if peer_file == file { continue; }
        groups.entry((index.files[peer_file as usize].path.clone(), edge.relation.clone(), edge.resolution.as_str(), dir)).or_default().push(*e);
    }
}

fn push_file_sites(index: &SourceGraphIndex, id: &str, items: &mut Vec<Value>) {
    for site in index.lookup.sites_by_file.get(id).into_iter().flatten().map(|s| &index.sites[*s as usize]) {
        if site.caller_id.is_none() && site.target_id.is_none() { items.push(json!({ "kind": "site", "direction": "out", "site": val(site) })); }
    }
}

fn push_caller_sites(index: &SourceGraphIndex, id: &str, items: &mut Vec<Value>) {
    for site in index.lookup.sites_by_caller.get(id).into_iter().flatten().map(|s| &index.sites[*s as usize]) {
        if site.target_id.is_none() { items.push(json!({ "kind": "site", "direction": "out", "site": val::<SourceSite>(site) })); }
    }
}

// ---- overview --------------------------------------------------------------------------------------------------

pub(crate) struct Link { target: usize, relation: u32, resolution: Resolution, derived: bool, count: u64, edges: Vec<u32> }

pub(crate) struct Group {
    id: String,
    kind: &'static str,
    label: String,
    path: Option<String>,
    parent: Option<usize>,
    layer: Option<u32>,
    cyclic: bool,
    members: Vec<u32>,
    internal_edges: u64,
    links: Vec<Link>,
}

pub(crate) struct Overview { groups: Vec<Group>, relations: Vec<String>, level: Level }

fn node_count(index: &SourceGraphIndex, level: Level) -> usize { match level { Level::Files => index.files.len(), Level::Functions => index.symbols.len() } }
fn node_id(index: &SourceGraphIndex, level: Level, node: u32) -> &str { match level { Level::Files => &index.files[node as usize].id, Level::Functions => &index.symbols[node as usize].id } }
fn label_of(index: &SourceGraphIndex, level: Level, node: u32) -> String {
    match level {
        Level::Files => index.files[node as usize].path.clone(),
        Level::Functions => { let s = &index.symbols[node as usize]; if s.qualified_name.is_empty() { s.name.clone() } else { s.qualified_name.clone() } }
    }
}
fn file_path_of(index: &SourceGraphIndex, level: Level, node: u32) -> &str {
    match level { Level::Files => &index.files[node as usize].path, Level::Functions => &index.symbols[node as usize].path }
}
fn directory_of(path: &str) -> &str { path.rfind('/').map_or("", |i| &path[..i]) }

struct Projected { a: u32, b: u32, edge: u32, derived: bool }

fn project(index: &SourceGraphIndex, level: Level) -> Vec<Projected> {
    let mut out = Vec::new();
    for (i, edge) in index.edges.iter().enumerate() {
        let (Some(source), Some(target)) = (index.lookup.node(&edge.source_id), index.lookup.node(&edge.target_id)) else { continue };
        match level {
            Level::Files => {
                let (Some(a), Some(b)) = (file_of_node(index, source), file_of_node(index, target)) else { continue };
                out.push(Projected { a, b, edge: i as u32, derived: !matches!((source, target), (Node::File(_), Node::File(_))) });
            }
            Level::Functions => if let (Node::Symbol(a), Node::Symbol(b)) = (source, target) { out.push(Projected { a, b, edge: i as u32, derived: false }); },
        }
    }
    out
}

/// Strongly connected components, iteratively (repositories can nest call chains far deeper than the stack).
/// Components are returned sinks first.
pub(super) fn tarjan(adjacency: &[Vec<u32>]) -> Vec<Vec<u32>> {
    let n = adjacency.len();
    let mut scc = Scc { adjacency, order: vec![u32::MAX; n], low: vec![0u32; n], on: vec![false; n], stack: Vec::new(), components: Vec::new(), counter: 0 };
    for root in 0..n {
        if scc.order[root] == u32::MAX { scc.search(root); }
    }
    scc.components
}

struct Scc<'a> { adjacency: &'a [Vec<u32>], order: Vec<u32>, low: Vec<u32>, on: Vec<bool>, stack: Vec<u32>, components: Vec<Vec<u32>>, counter: u32 }

impl Scc<'_> {
    fn enter(&mut self, v: usize, calls: &mut Vec<(u32, usize)>) {
        self.order[v] = self.counter; self.low[v] = self.counter; self.counter += 1;
        self.stack.push(v as u32); self.on[v] = true;
        calls.push((v as u32, 0));
    }

    /// Depth-first search from `root` with an explicit call stack.
    fn search(&mut self, root: usize) {
        let mut calls: Vec<(u32, usize)> = Vec::new();
        self.enter(root, &mut calls);
        while let Some(&(v, position)) = calls.last() {
            let v = v as usize;
            if position < self.adjacency[v].len() {
                calls.last_mut().unwrap().1 += 1;
                let w = self.adjacency[v][position] as usize;
                if self.order[w] == u32::MAX { self.enter(w, &mut calls); } else if self.on[w] { self.low[v] = self.low[v].min(self.order[w]); }
            } else {
                calls.pop();
                self.leave(v);
                if let Some(&(parent, _)) = calls.last() { self.low[parent as usize] = self.low[parent as usize].min(self.low[v]); }
            }
        }
    }

    fn leave(&mut self, v: usize) {
        if self.low[v] != self.order[v] { return; }
        let mut component = Vec::new();
        loop {
            let w = self.stack.pop().unwrap();
            self.on[w as usize] = false;
            component.push(w);
            if w as usize == v { break; }
        }
        self.components.push(component);
    }
}

pub(crate) fn overview(index: &SourceGraphIndex, level: Level, group_by: GroupBy) -> std::sync::Arc<Overview> {
    let cached = index.lookup.overviews.lock().get(&(level, group_by)).cloned();
    if let Some(found) = cached { return found; }
    let built = std::sync::Arc::new(build_overview(index, level, group_by));
    index.lookup.overviews.lock().insert((level, group_by), built.clone());
    built
}

fn build_overview(index: &SourceGraphIndex, level: Level, group_by: GroupBy) -> Overview {
    let n = node_count(index, level);
    let projected = project(index, level);
    let mut group_of = vec![0usize; n];
    let mut groups = match group_by {
        GroupBy::Directory => directory_groups(index, level, &mut group_of),
        GroupBy::Scc => scc_groups(index, level, &projected, &mut group_of),
    };
    let relations = aggregate_links(index, &projected, &group_of, &mut groups);
    Overview { groups, relations, level }
}

fn directory_groups(index: &SourceGraphIndex, level: Level, group_of: &mut [usize]) -> Vec<Group> {
    let n = group_of.len() as u32;
    let mut paths: Vec<&str> = vec![""];
    paths.extend(index.directories.iter().map(String::as_str));
    for node in 0..n { paths.push(directory_of(file_path_of(index, level, node))); }
    paths.sort_unstable();
    paths.dedup();
    let position: HashMap<&str, usize> = paths.iter().enumerate().map(|(i, p)| (*p, i)).collect();
    let mut groups: Vec<Group> = paths.iter().map(|path| Group {
        id: format!("dir:{path}"), kind: "directory", label: if path.is_empty() { ".".into() } else { (*path).into() }, path: Some((*path).into()),
        parent: if path.is_empty() { None } else { position.get(directory_of(path)).copied() },
        layer: None, cyclic: false, members: Vec::new(), internal_edges: 0, links: Vec::new(),
    }).collect();
    for node in 0..n {
        let group = position[directory_of(file_path_of(index, level, node))];
        group_of[node as usize] = group;
        groups[group].members.push(node);
    }
    groups
}

/// Resolved-static adjacency between nodes plus which nodes call themselves.
fn scc_adjacency(index: &SourceGraphIndex, projected: &[Projected], n: usize) -> (Vec<Vec<u32>>, Vec<bool>) {
    let mut adjacency: Vec<Vec<u32>> = vec![Vec::new(); n];
    let mut self_loops = vec![false; n];
    for edge in projected {
        if index.edges[edge.edge as usize].resolution != Resolution::ResolvedStatic { continue; }
        if edge.a == edge.b { self_loops[edge.a as usize] = true; } else { adjacency[edge.a as usize].push(edge.b); }
    }
    (adjacency, self_loops)
}

/// Longest-path layers per component. Tarjan emits sinks first, so walking backwards visits sources first.
fn scc_layers(components: &[Vec<u32>], adjacency: &[Vec<u32>]) -> Vec<u32> {
    let mut component_of = vec![0usize; adjacency.len()];
    for (c, members) in components.iter().enumerate() { for m in members { component_of[*m as usize] = c; } }
    let mut layer = vec![0u32; components.len()];
    for c in (0..components.len()).rev() {
        for m in &components[c] {
            for w in &adjacency[*m as usize] {
                let target = component_of[*w as usize];
                if target != c { layer[target] = layer[target].max(layer[c] + 1); }
            }
        }
    }
    layer
}

fn scc_groups(index: &SourceGraphIndex, level: Level, projected: &[Projected], group_of: &mut [usize]) -> Vec<Group> {
    let (adjacency, self_loops) = scc_adjacency(index, projected, group_of.len());
    let components = tarjan(&adjacency);
    let layer = scc_layers(&components, &adjacency);
    let mut built: Vec<(u32, String, usize)> = components.iter().enumerate().map(|(c, members)| {
        let smallest = members.iter().map(|m| node_id(index, level, *m)).min().unwrap_or("").to_string();
        (layer[c], smallest, c)
    }).collect();
    built.sort();
    let mut groups = Vec::with_capacity(built.len());
    for (rank, (_, smallest, c)) in built.iter().enumerate() {
        let mut members = components[*c].clone();
        members.sort_by(|a, b| node_id(index, level, *a).cmp(node_id(index, level, *b)));
        let cyclic = members.len() > 1 || self_loops[members[0] as usize];
        let label = if members.len() == 1 { label_of(index, level, members[0]) } else { format!("{} members in a cycle, including {}", members.len(), label_of(index, level, members[0])) };
        for m in &members { group_of[*m as usize] = rank; }
        groups.push(Group { id: format!("scc:{smallest}"), kind: "scc", label, path: None, parent: None, layer: Some(layer[*c]), cyclic, members, internal_edges: 0, links: Vec::new() });
    }
    groups
}

/// Aggregate arrows between groups, keeping the underlying edge ids. Returns the relation names the links index into.
fn aggregate_links(index: &SourceGraphIndex, projected: &[Projected], group_of: &[usize], groups: &mut [Group]) -> Vec<String> {
    let mut relations: Vec<String> = Vec::new();
    let mut relation_ids: HashMap<&str, u32> = HashMap::new();
    let mut links: HashMap<(usize, usize, u32, Resolution, bool), (u64, Vec<u32>)> = HashMap::new();
    for edge in projected {
        let source = &index.edges[edge.edge as usize];
        let (from, to) = (group_of[edge.a as usize], group_of[edge.b as usize]);
        if from == to { groups[from].internal_edges += 1; continue; }
        let relation = *relation_ids.entry(source.relation.as_str()).or_insert_with(|| { relations.push(source.relation.clone()); relations.len() as u32 - 1 });
        let entry = links.entry((from, to, relation, source.resolution, edge.derived)).or_default();
        entry.0 += 1;
        if entry.1.len() < EDGE_SAMPLE { entry.1.push(edge.edge); }
    }
    for ((from, to, relation, resolution, derived), (count, edges)) in links {
        groups[from].links.push(Link { target: to, relation, resolution, derived, count, edges });
    }
    for group in groups {
        group.links.sort_by(|a, b| b.count.cmp(&a.count).then(a.target.cmp(&b.target)).then(a.relation.cmp(&b.relation)).then((a.resolution as u8).cmp(&(b.resolution as u8))).then(a.derived.cmp(&b.derived)));
    }
    relations
}

fn group_json(index: &SourceGraphIndex, overview: &Overview, position: usize) -> Value {
    let group = &overview.groups[position];
    let mut item = Map::new();
    item.insert("id".into(), json!(group.id));
    item.insert("kind".into(), json!(group.kind));
    item.insert("level".into(), json!(overview.level));
    item.insert("label".into(), json!(group.label));
    if let Some(path) = &group.path { item.insert("path".into(), json!(path)); }
    if let Some(parent) = group.parent { item.insert("parentId".into(), json!(overview.groups[parent].id)); }
    if let Some(layer) = group.layer { item.insert("layer".into(), json!(layer)); }
    item.insert("cyclic".into(), json!(group.cyclic));
    item.insert("memberCount".into(), json!(group.members.len()));
    item.insert("members".into(), json!(group.members.iter().take(MEMBER_SAMPLE).map(|m| node_id(index, overview.level, *m)).collect::<Vec<_>>()));
    item.insert("membersTruncated".into(), json!(group.members.len() > MEMBER_SAMPLE));
    item.insert("internalEdges".into(), json!(group.internal_edges));
    item.insert("linksTotal".into(), json!(group.links.len()));
    item.insert("linksTruncated".into(), json!(group.links.len() > LINK_SAMPLE));
    item.insert("links".into(), Value::Array(group.links.iter().take(LINK_SAMPLE).map(|link| json!({
        "targetGroupId": overview.groups[link.target].id, "targetLabel": overview.groups[link.target].label,
        "relation": overview.relations[link.relation as usize], "resolution": link.resolution, "derived": link.derived, "count": link.count,
        "edgeIds": link.edges.iter().map(|e| &index.edges[*e as usize].id).collect::<Vec<_>>(), "edgeIdsTruncated": link.count as usize > link.edges.len(),
    })).collect()));
    Value::Object(item)
}
