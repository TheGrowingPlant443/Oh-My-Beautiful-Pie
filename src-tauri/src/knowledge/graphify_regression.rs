//! Deterministic Graphify regressions: immutable admitted snapshots, byte-exact retention, complete metadata,
//! passive freshness, tagged search, response budgets and hostile graph shapes. Fixtures live in throwaway folders.
use super::fsio::sha256_hex;
use super::integrations::Environment;
use super::{
    GraphifyArtifact, GraphifyDirection, GraphifyIdKind, GraphifyQuery, GraphifyRef,
    GraphifySection, KnowledgeService,
};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "omp graphify regression ø {}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(crate::setup::canonical(&path).unwrap())
    }
    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> (Temp, Arc<KnowledgeService>, PathBuf, String) {
    let t = Temp::new();
    fs::create_dir_all(t.join("home")).unwrap();
    let service = KnowledgeService::with_environment(
        t.join("config"),
        t.join("home"),
        Environment::isolated(),
    );
    let repository = t.join("repo");
    fs::create_dir_all(repository.join(".git")).unwrap();
    let repository = crate::setup::canonical(&repository).unwrap();
    let folder = repository.to_string_lossy().into_owned();
    (t, service, repository, folder)
}

fn write_bytes(repository: &Path, bytes: &[u8]) {
    let out = repository.join("graphify-out");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("graph.json"), bytes).unwrap();
    fs::write(
        out.join(".graphify_root"),
        repository.to_string_lossy().as_bytes(),
    )
    .unwrap();
}

fn write_graph(repository: &Path, graph: &Value) -> Vec<u8> {
    // Deliberately non-canonical bytes: indentation, key order and a trailing newline must survive byte for byte.
    let mut bytes = serde_json::to_vec_pretty(graph).unwrap();
    bytes.extend_from_slice(b"\n");
    write_bytes(repository, &bytes);
    bytes
}

fn text(id: &str) -> GraphifyRef {
    GraphifyRef {
        id: id.into(),
        id_kind: GraphifyIdKind::Text,
    }
}

fn snapshot_dir(t: &Temp, id: &str) -> PathBuf {
    t.join("config").join("knowledge").join("graphify").join(id)
}

fn small_graph(name: &str) -> Value {
    json!({
        "directed": true,
        "multigraph": true,
        "nodes": [{"id": "a", "label": format!("A {name}"), "community": 1}, {"id": "b", "label": "B"}],
        "links": [{"source": "a", "target": "b", "relation": "calls"}],
    })
}

#[test]
fn admitted_snapshots_keep_serving_when_live_output_changes_or_vanishes_and_are_never_pruned() {
    let (t, service, repo, folder) = fixture();
    let mut ids = Vec::new();
    for i in 0..7 {
        write_graph(&repo, &small_graph(&format!("build {i}")));
        ids.push(service.open_graphify_artifact(&folder).unwrap().artifact_id);
    }
    let distinct: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(distinct.len(), 7);
    for id in &ids {
        assert!(
            snapshot_dir(&t, id).join("meta.json").is_file(),
            "no artifact snapshot is pruned"
        );
    }
    // The live output is replaced and then deleted: every admitted artifact still answers, with its own hash.
    write_graph(&repo, &json!({"nodes": [{"id": "z"}], "links": []}));
    fs::remove_dir_all(repo.join("graphify-out")).unwrap();
    for (i, id) in ids.iter().enumerate() {
        let page = service
            .read_graphify_graph(id, GraphifySection::Nodes, 0, 10)
            .unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.items[0]["label"], format!("A build {i}"));
        let node = service
            .query_graphify_graph(id, &GraphifyQuery::Node { id: text("b") })
            .unwrap();
        assert_eq!(node.artifact_hash, page.artifact_hash);
        assert!(service
            .read_graphify_graph(id, GraphifySection::Metadata, 0, 1)
            .is_ok());
    }
}

#[test]
fn passive_check_reports_new_output_without_admitting_or_invalidating_anything() {
    let (t, service, repo, folder) = fixture();
    let bytes = write_graph(&repo, &small_graph("one"));
    let artifact = service.open_graphify_artifact(&folder).unwrap();
    let listing = || {
        let mut names: Vec<_> = fs::read_dir(t.join("config").join("knowledge").join("graphify"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        names.sort();
        names
    };
    let before = listing();

    let current = service
        .check_graphify_artifact(&artifact.artifact_id)
        .unwrap();
    assert_eq!(
        (
            current.current,
            current.message.is_none(),
            current.artifact_id.as_str()
        ),
        (true, true, artifact.artifact_id.as_str())
    );

    // Identical length but different bytes: the comparison is a full hash, not a size/mtime guess.
    let mut changed = bytes.clone();
    let at = changed.iter().position(|b| *b == b'o').unwrap();
    changed[at] = b'0';
    write_bytes(&repo, &changed);
    let stale = service
        .check_graphify_artifact(&artifact.artifact_id)
        .unwrap();
    assert!(!stale.current);
    assert!(stale
        .message
        .unwrap()
        .contains("New Graphify output available"));

    fs::remove_dir_all(repo.join("graphify-out")).unwrap();
    let gone = service
        .check_graphify_artifact(&artifact.artifact_id)
        .unwrap();
    assert!(!gone.current && gone.message.is_some());

    let after = listing();
    assert_eq!(before, after, "a passive check never creates a snapshot");
    assert_eq!(
        service
            .read_graphify_graph(&artifact.artifact_id, GraphifySection::Nodes, 0, 10)
            .unwrap()
            .total,
        2
    );
    assert!(service.check_graphify_artifact(&"0".repeat(32)).is_err());
    assert!(service.check_graphify_artifact("../x").is_err());
}

#[test]
fn the_original_graph_bytes_and_all_oversized_metadata_are_retained_privately() {
    let (t, service, repo, folder) = fixture();
    let huge_graph_attrs = "g".repeat(300_000);
    let huge_extra = "e".repeat(200_000);
    let bytes = write_graph(
        &repo,
        &json!({
            "zeta": {"kept": true},
            "graph": {"blob": huge_graph_attrs, "version": "1"},
            "big_extra": huge_extra,
            "small_extra": [1, 2, 3],
            "nodes": [{"id": "a"}],
            "links": [],
        }),
    );
    let artifact = service.open_graphify_artifact(&folder).unwrap();
    let dir = snapshot_dir(&t, &artifact.artifact_id);

    let retained = fs::read(dir.join("graph.json")).unwrap();
    assert_eq!(
        retained, bytes,
        "the immutable snapshot holds the original bytes, not a re-serialisation"
    );
    assert_eq!(
        sha256_hex(&retained),
        artifact.artifact_hash,
        "the retained bytes are exactly the hashed bytes"
    );
    assert_eq!(artifact.bytes, bytes.len() as u64);

    let metadata: Value =
        serde_json::from_slice(&fs::read(dir.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(metadata["graph"]["blob"].as_str().unwrap().len(), 300_000);
    assert_eq!(metadata["big_extra"].as_str().unwrap().len(), 200_000);
    assert_eq!(metadata["small_extra"], json!([1, 2, 3]));
    assert_eq!(metadata["zeta"], json!({"kept": true}));

    let receipt = artifact
        .metadata_receipt
        .as_ref()
        .expect("oversized metadata has an explicit receipt");
    assert_eq!(
        (receipt.path.as_str(), receipt.raw_graph_path.as_str()),
        ("metadata.json", "graph.json")
    );
    assert_eq!(
        receipt.bytes,
        fs::metadata(dir.join("metadata.json")).unwrap().len()
    );
    assert_eq!(
        receipt.sha256,
        sha256_hex(&fs::read(dir.join("metadata.json")).unwrap())
    );
    assert!(
        artifact.metadata.is_empty(),
        "no lossy placeholder metadata is invented"
    );

    // Metadata page 0 stays a GraphifyArtifact and within the page budget.
    let page = service
        .read_graphify_graph(&artifact.artifact_id, GraphifySection::Metadata, 0, 1)
        .unwrap();
    assert!(serde_json::to_vec(&page).unwrap().len() <= 1024 * 1024);
    let echoed: GraphifyArtifact = serde_json::from_value(page.items[0].clone()).unwrap();
    assert_eq!(echoed.artifact_hash, artifact.artifact_hash);
    assert!(echoed.metadata_receipt.is_some());

    // Small metadata stays inline with no receipt.
    write_graph(
        &repo,
        &json!({"graph": {"v": 1}, "extra": "x", "nodes": [{"id": "a"}], "links": []}),
    );
    let small = service.open_graphify_artifact(&folder).unwrap();
    assert!(small.metadata_receipt.is_none());
    assert_eq!(
        (
            small.metadata["graph"]["v"].clone(),
            small.metadata["extra"].clone()
        ),
        (json!(1), json!("x"))
    );
}

#[test]
fn search_returns_tagged_node_link_and_hyperedge_records_counted_together() {
    let (_t, service, repo, folder) = fixture();
    write_graph(
        &repo,
        &json!({
            "nodes": [{"id": "auth", "label": "Auth"}, {"id": "db", "label": "Database"}],
            "links": [{"id": "auth-link", "source": "auth", "target": "db", "relation": "auth uses db"}],
            "hyperedges": [{"id": "auth-group", "label": "Auth group", "nodes": ["auth", "db"]}],
        }),
    );
    let id = service.open_graphify_artifact(&folder).unwrap().artifact_id;
    let page = service
        .query_graphify_graph(
            &id,
            &GraphifyQuery::Search {
                text: "auth".into(),
                offset: 0,
                limit: 10,
            },
        )
        .unwrap();
    assert_eq!(page.total, 3);
    let kinds: Vec<_> = page
        .items
        .iter()
        .map(|i| i["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds, vec!["node", "hyperedge", "link"]);
    assert_eq!(page.items[0]["node"]["ref"]["id"], "auth");
    assert_eq!(page.items[1]["hyperedge"]["id"], "auth-group");
    assert_eq!(page.items[2]["link"]["id"], "auth-link");
    for item in &page.items {
        assert_eq!(
            item.as_object().unwrap().len(),
            2,
            "only the discriminator and the typed record: {item}"
        );
    }
    // Pagination counts all three kinds.
    let second = service
        .query_graphify_graph(
            &id,
            &GraphifyQuery::Search {
                text: "auth".into(),
                offset: 2,
                limit: 1,
            },
        )
        .unwrap();
    assert_eq!(
        (second.total, second.items.len(), second.next_offset),
        (3, 1, None)
    );
    assert_eq!(second.items[0]["kind"], "link");
}

#[test]
fn pages_are_bounded_including_the_envelope_and_oversized_hydration_is_rejected_actionably() {
    let (_t, service, repo, folder) = fixture();
    // Each node is below the 512 KiB record limit; hydrated into one link they exceed the 1 MiB page.
    let heavy = "h".repeat(500_000);
    let mut nodes: Vec<Value> = vec![
        json!({"id": "a", "blob": heavy}),
        json!({"id": "b", "blob": heavy}),
    ];
    for i in 0..6 {
        nodes.push(json!({"id": format!("n{i}"), "blob": "p".repeat(300_000)}));
    }
    write_graph(
        &repo,
        &json!({"nodes": nodes, "links": [{"source": "a", "target": "b", "relation": "heavy", "note": "l".repeat(100_000)}]}),
    );
    let id = service.open_graphify_artifact(&folder).unwrap().artifact_id;

    let page = service
        .read_graphify_graph(&id, GraphifySection::Nodes, 0, 200)
        .unwrap();
    assert!(
        serde_json::to_vec(&page).unwrap().len() <= 1024 * 1024,
        "the whole serialised page, envelope included, is bounded"
    );
    assert!(
        page.items.len() < 8 && page.next_offset == Some(page.items.len() as u64),
        "the page is cut honestly, not silently"
    );

    let error = service
        .query_graphify_graph(
            &id,
            &GraphifyQuery::Neighbors {
                id: text("a"),
                direction: GraphifyDirection::Both,
                offset: 0,
                limit: 10,
            },
        )
        .unwrap_err();
    assert!(
        error.contains("kind \"node\"") && error.contains("page limit"),
        "{error}"
    );
    // The nodes remain individually readable, and the plain link is paged unhydrated by the section reader.
    assert!(
        serde_json::to_vec(
            &service
                .query_graphify_graph(&id, &GraphifyQuery::Node { id: text("a") })
                .unwrap()
        )
        .unwrap()
        .len()
            <= 1024 * 1024
    );
    assert_eq!(
        service
            .read_graphify_graph(&id, GraphifySection::Links, 0, 10)
            .unwrap()
            .total,
        1
    );
}

#[test]
fn hostile_graphs_keep_parallel_loops_dangling_and_reused_ids_visible_and_reject_ambiguity() {
    let (_t, service, repo, folder) = fixture();
    write_graph(
        &repo,
        &json!({
            "directed": true,
            "nodes": [{"id": "a"}, {"id": "b"}],
            "links": [
                {"id": "dup", "source": "a", "target": "b", "relation": "r"},
                {"id": "dup", "source": "a", "target": "b", "relation": "r"},
                {"id": "graphify:reserved", "source": "a", "target": "b", "relation": "r"},
                {"source": "a", "target": "a", "relation": "loop"},
                {"source": "a", "target": "missing", "relation": "dangling"},
            ],
            "hyperedges": [
                {"id": "h", "nodes": ["a", "ghost"]},
                {"id": "h", "nodes": ["a", "b"]},
            ],
        }),
    );
    let artifact = service.open_graphify_artifact(&folder).unwrap();
    assert_eq!(
        (
            artifact.counts.links,
            artifact.self_loops,
            artifact.dangling_links
        ),
        (5, 1, 1)
    );
    let links = service
        .read_graphify_graph(&artifact.artifact_id, GraphifySection::Links, 0, 10)
        .unwrap()
        .items;
    let ids: std::collections::HashSet<_> = links
        .iter()
        .map(|l| l["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 5, "no two links share a response id");
    assert_eq!(links[0]["id"], "dup");
    assert_eq!(
        links[1]["nativeId"], "dup",
        "the original native id is still shown"
    );
    assert_ne!(links[1]["id"], "dup");
    assert_eq!(links[4]["dangling"], true);
    let hyperedges = service
        .read_graphify_graph(&artifact.artifact_id, GraphifySection::Hyperedges, 0, 10)
        .unwrap()
        .items;
    assert_ne!(hyperedges[0]["id"], hyperedges[1]["id"]);
    assert_eq!(hyperedges[1]["nativeId"], "h");
    assert_eq!(hyperedges[0]["unresolvedMembers"], 1);

    let reject = |bytes: &str, needle: &str| {
        write_bytes(&repo, bytes.as_bytes());
        let error = service.open_graphify_artifact(&folder).unwrap_err();
        assert!(error.contains(needle), "{needle}: {error}");
    };
    reject(
        r#"{"nodes":[{"id":"a"},{"id":"a"}],"links":[]}"#,
        "Duplicate node id",
    );
    reject(
        r#"{"nodes":[{"id":"a"}],"links":[],"extra":1,"extra":2}"#,
        "repeats the top-level field",
    );
    reject(
        r#"{"nodes":[{"id":"a"}],"links":[],"links":[]}"#,
        "repeats the top-level field",
    );
    reject(
        r#"{"nodes":[{"id":"a"}],"links":[],"edges":[]}"#,
        "ambiguous",
    );
}
