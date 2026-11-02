//! Service, inventory, validation and query tests. The parser is replaced by an in-process fake that derives
//! facts from the manifest, so no Python is needed except in the single optional end-to-end test.
use super::super::inventory;
use super::super::model::{self, Expected, ManifestFile, Result, MAX_OUTPUT_BYTES};
use super::super::query;
use super::super::runner::{self, ExtractRun, Parser, ParserOutcome};
use super::super::{
    Direction, FileStatus, GroupBy, Level, ProfileCapability, RuntimeCapability, SourceGraphIndex,
    SourceIndexCapabilities, SourceIndexPage, SourceIndexQuery, SourceIndexService,
    SourceIndexState, SourceIndexStatus,
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("omp source index ø {}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(root: &Path, relative: &str, content: impl AsRef<[u8]>) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

// ---- fake parser -----------------------------------------------------------------------------------------------

#[derive(Clone)]
enum Mode {
    Ok,
    Fail(String),
    TimedOut,
    Garbage,
    Oversize,
    WrongHash,
    Wait,
    CapPartial,
}

struct FakeParser {
    mode: Mutex<Mode>,
    caps: Mutex<SourceIndexCapabilities>,
}

impl FakeParser {
    fn new() -> Arc<Self> {
        let caps = SourceIndexCapabilities {
            available: true,
            probed: true,
            runtime: RuntimeCapability {
                source: "environment".into(),
                python: Some("fake".into()),
                version: Some("3.12".into()),
                tree_sitter: Some("0.25.2".into()),
                error: None,
            },
            profiles: vec![ProfileCapability {
                language: "text".into(),
                version: Some("1".into()),
                available: true,
                error: None,
            }],
            hint: None,
        };
        Arc::new(Self {
            mode: Mutex::new(Mode::Ok),
            caps: Mutex::new(caps),
        })
    }
    fn set(&self, mode: Mode) {
        *self.mode.lock() = mode;
    }
}

fn span(start: u64) -> Value {
    json!({ "byteStart": start, "byteEnd": start + 1, "lineStart": 1, "lineEnd": 1, "columnStart": start, "columnEnd": start + 1 })
}

/// Facts for the first four parser-eligible files: alpha<->beta mutual recursion across two files, a
/// self-recursive beta, a same-name `Beta`, an isolated gamma, an import, an unresolved and a candidate site.
fn fake_index(manifest: &Value) -> Value {
    let files = manifest["files"].as_array().unwrap();
    let pending: Vec<&Value> = files.iter().filter(|f| f["status"] == "pending").collect();
    let out_files: Vec<Value> = files.iter().map(|f| {
        let parsed = f["status"] == "pending";
        json!({ "id": f["id"], "path": f["path"], "sha256": f["sha256"], "bytes": f["bytes"], "language": if parsed { json!("text") } else { Value::Null },
            "status": if parsed { "parsed" } else { "skipped" }, "diagnostics": f["diagnostics"] })
    }).collect();
    let (mut symbols, mut sites, mut edges) = (Vec::new(), Vec::new(), Vec::new());
    if pending.len() >= 4 {
        let (a, b, c) = (pending[0], pending[2], pending[3]);
        let symbol = |file: &Value, name: &str, start: u64| -> Value {
            let path = file["path"].as_str().unwrap();
            json!({ "id": model::stable_id(&[path, "text", "main", "function", &start.to_string(), &(start + 1).to_string()]), "fileId": file["id"], "path": path, "language": "text",
                "region": "main", "ownerId": null, "kind": "function", "name": name, "qualifiedName": name, "span": span(start) })
        };
        let site = |file: &Value,
                    caller: Option<&Value>,
                    relation: &str,
                    start: u64,
                    resolution: &str,
                    target: Option<&Value>,
                    candidates: Vec<Value>,
                    raw: &str|
         -> Value {
            let path = file["path"].as_str().unwrap();
            json!({ "id": model::stable_id(&["site", path, relation, &start.to_string()]), "fileId": file["id"], "path": path, "language": "text", "region": "main",
                "callerId": caller.map(|c| c["id"].clone()), "span": span(start), "relation": relation, "raw": raw, "targetId": target.cloned(),
                "resolution": resolution, "conditional": false, "typeOnly": false, "candidateIds": candidates })
        };
        let edge = |source: &Value, target: &Value, relation: &str, evidence: &Value| -> Value {
            json!({ "id": model::stable_id(&["edge", source.as_str().unwrap(), target.as_str().unwrap(), relation]), "sourceId": source, "targetId": target,
                "relation": relation, "resolution": "resolvedStatic", "evidenceIds": [evidence] })
        };
        let alpha = symbol(a, "alpha", 0);
        let beta = symbol(b, "beta", 0);
        let big_beta = symbol(b, "Beta", 1);
        let gamma = symbol(c, "gamma", 0);
        let (alpha_id, beta_id) = (&alpha["id"], &beta["id"]);
        let s1 = site(
            a,
            Some(&alpha),
            "calls",
            1,
            "resolvedStatic",
            Some(beta_id),
            vec![],
            "beta()",
        );
        let s2 = site(
            b,
            Some(&beta),
            "calls",
            5,
            "resolvedStatic",
            Some(alpha_id),
            vec![],
            "alpha()",
        );
        let s3 = site(
            b,
            Some(&beta),
            "calls",
            6,
            "resolvedStatic",
            Some(beta_id),
            vec![],
            "beta()",
        );
        let s4 = site(
            a,
            None,
            "imports",
            2,
            "resolvedStatic",
            Some(&b["id"]),
            vec![],
            "./b",
        );
        let s5 = site(
            a,
            Some(&alpha),
            "calls",
            3,
            "unresolved",
            None,
            vec![],
            "dynamic()",
        );
        let s6 = site(
            a,
            Some(&alpha),
            "calls",
            4,
            "candidate",
            None,
            vec![big_beta["id"].clone()],
            "Beta()",
        );
        edges.push(edge(alpha_id, beta_id, "calls", &s1["id"]));
        edges.push(edge(beta_id, alpha_id, "calls", &s2["id"]));
        edges.push(edge(beta_id, beta_id, "calls", &s3["id"]));
        edges.push(edge(&a["id"], &b["id"], "imports", &s4["id"]));
        symbols.extend([alpha, beta, big_beta, gamma]);
        sites.extend([s1, s2, s3, s4, s5, s6]);
    }
    json!({
        "schemaVersion": 1, "snapshotId": manifest["snapshotId"], "folder": manifest["folder"], "sourceHash": manifest["sourceHash"],
        "extractorVersions": { "fake": "1" }, "files": out_files, "directories": manifest["directories"], "symbols": symbols, "sites": sites, "edges": edges,
        "coverage": { "languages": [{ "language": "text", "files": pending.len(), "parsed": pending.len(), "partial": 0, "failed": 0, "unsupported": 0, "diagnostics": [] }], "lastFullyProcessedFile": null, "partial": false },
        "limitations": ["fake parser"],
    })
}

impl Parser for FakeParser {
    fn probe(&self) -> SourceIndexCapabilities {
        self.caps.lock().clone()
    }
    fn scripts_hash(&self) -> String {
        "fake".into()
    }
    fn extract(&self, run: &ExtractRun) -> ParserOutcome {
        let manifest: Value = serde_json::from_slice(&fs::read(run.manifest).unwrap()).unwrap();
        assert_eq!(manifest["schemaVersion"], 1);
        let root = PathBuf::from(manifest["root"].as_str().unwrap());
        assert!(
            root.is_dir(),
            "manifest root must be the immutable snapshot copy"
        );
        for file in manifest["files"].as_array().unwrap() {
            assert!(root.join(file["path"].as_str().unwrap()).is_file());
            assert!(file["language"].is_null());
        }
        assert!(
            run.output.parent().unwrap() == run.work_dir
                && run.manifest.parent().unwrap() == run.work_dir
        );
        let mode = self.mode.lock().clone();
        match mode {
            Mode::Ok => {
                fs::write(
                    run.output,
                    serde_json::to_vec(&fake_index(&manifest)).unwrap(),
                )
                .unwrap();
                ParserOutcome::Finished
            }
            Mode::CapPartial => {
                let mut index = fake_index(&manifest);
                index["coverage"]["partial"] = json!(true);
                index["coverage"]["lastFullyProcessedFile"] = manifest["files"][0]["path"].clone();
                fs::write(run.output, serde_json::to_vec(&index).unwrap()).unwrap();
                ParserOutcome::Finished
            }
            Mode::WrongHash => {
                let mut index = fake_index(&manifest);
                index["sourceHash"] = json!("0".repeat(64));
                fs::write(run.output, serde_json::to_vec(&index).unwrap()).unwrap();
                ParserOutcome::Finished
            }
            Mode::Garbage => {
                fs::write(run.output, b"{ not json").unwrap();
                ParserOutcome::Finished
            }
            Mode::Oversize => {
                let file = File::create(run.output).unwrap();
                file.set_len(MAX_OUTPUT_BYTES + 1).unwrap();
                ParserOutcome::Finished
            }
            Mode::Fail(message) => ParserOutcome::Failed(message),
            Mode::TimedOut => ParserOutcome::TimedOut,
            Mode::Wait => {
                fs::write(
                    run.work_dir.join("progress.json"),
                    r#"{"completedFiles":2,"totalFiles":5,"lastFullyProcessedFile":"x"}"#,
                )
                .unwrap();
                let started = Instant::now();
                while !run.cancel.load(Ordering::Relaxed) {
                    if started.elapsed() > Duration::from_secs(30) {
                        return ParserOutcome::Failed("test parser was never cancelled".into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                ParserOutcome::Cancelled
            }
        }
    }
}

// ---- fixtures --------------------------------------------------------------------------------------------------

struct Fixture {
    temp: Temp,
    repo: PathBuf,
    parser: Arc<FakeParser>,
    service: Arc<SourceIndexService>,
}

impl Fixture {
    fn new() -> Self {
        let temp = Temp::new();
        let repo = temp.0.join("repo æ");
        fs::create_dir_all(&repo).unwrap();
        let parser = FakeParser::new();
        let service = SourceIndexService::with_parser(temp.0.join("config"), parser.clone());
        Self {
            temp,
            repo,
            parser,
            service,
        }
    }
    fn folder(&self) -> String {
        self.repo.to_string_lossy().into_owned()
    }
    fn standard_repo(&self) {
        write(&self.repo, "src/alpha.ts", "export function alpha() {}\n");
        write(&self.repo, "src/beta.ts", "export function beta() {}\n");
        write(&self.repo, "Æble/ø gamma.py", "def gamma(): pass\n");
        write(&self.repo, "README.md", "# readme\n");
        write(&self.repo, ".gitignore", "ignored.txt\n");
        write(&self.repo, "ignored.txt", "ignored");
        write(&self.repo, ".env", "SECRET=1");
        write(&self.repo, "keys/server.pem", "private");
        write(
            &self.repo,
            "node_modules/pkg/index.js",
            "module.exports = 1;",
        );
        write(&self.repo, ".git/config", "[core]");
        fs::create_dir_all(self.repo.join("empty")).unwrap();
    }
    fn wait(&self, id: &str) -> SourceIndexStatus {
        wait_for(&self.service, id)
    }
    fn run(&self) -> SourceIndexStatus {
        let started = self.service.start(&self.folder()).unwrap();
        self.wait(&started.job_id)
    }
}

fn paths(index: &SourceGraphIndex) -> Vec<&str> {
    index.files.iter().map(|f| f.path.as_str()).collect()
}

fn wait_for(service: &SourceIndexService, id: &str) -> SourceIndexStatus {
    let started = Instant::now();
    loop {
        let status = service.read_job(id).unwrap();
        if !status.state.is_active() {
            return status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "job did not finish: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}

// ---- inventory, snapshot copy, service -------------------------------------------------------------------------

#[test]
fn complete_index_keeps_exact_inventory_and_an_immutable_copy() {
    let f = Fixture::new();
    f.standard_repo();
    let started = f.service.start(&f.folder()).unwrap();
    assert_eq!(started.state, SourceIndexState::Inventory);
    let done = f.wait(&started.job_id);
    assert_eq!(done.state, SourceIndexState::Complete, "{:?}", done.error);
    let id = done.snapshot_id.clone().unwrap();
    assert_eq!(id, done.job_id);
    let index = f.service.index(&id).unwrap();
    assert_eq!(
        paths(&index),
        [
            ".gitignore",
            "README.md",
            "src/alpha.ts",
            "src/beta.ts",
            "Æble/ø gamma.py"
        ]
    );
    assert_eq!(index.directories, ["empty", "keys", "src", "Æble"]);
    assert!(index
        .files
        .iter()
        .all(|file| file.status == FileStatus::Parsed));
    assert_eq!(index.coverage["admittedFiles"], 5);
    assert_eq!(index.coverage["parsedFiles"], 5);
    assert_eq!(index.coverage["encounteredSites"], 6);
    assert_eq!(
        index.coverage["unresolvedSites"], 2,
        "unresolved and candidate sites stay visible"
    );
    assert!(index
        .limitations
        .iter()
        .any(|l| l.contains("excluded as secrets")));
    assert!(
        index
            .extractor_versions
            .contains_key("ompWorkerScriptsSha256")
            && index.extractor_versions.contains_key("ompGrammars")
    );
    let hash = model::stable_id(&["file", "Æble/ø gamma.py"]);
    assert_eq!(index.file(&hash).unwrap().path, "Æble/ø gamma.py");
    assert_eq!(
        model::file_id("README.md"),
        index.file(&model::file_id("README.md")).unwrap().id
    );

    // The copy is independent of later repository edits and carries exact bytes.
    let root = f.service.snapshot_root(&id).unwrap();
    fs::write(f.repo.join("src/alpha.ts"), "changed").unwrap();
    assert_eq!(
        fs::read(root.join("src/alpha.ts")).unwrap(),
        b"export function alpha() {}\n"
    );
    assert_eq!(
        fs::read(root.join("Æble/ø gamma.py")).unwrap(),
        b"def gamma(): pass\n"
    );
    for excluded in [
        ".env",
        "keys/server.pem",
        "ignored.txt",
        "node_modules",
        ".git",
    ] {
        assert!(!root.join(excluded).exists(), "{excluded}");
    }
    assert!(root.join("empty").is_dir());

    // Content hash is a function of content: identical bytes give the same hash with a new snapshot id.
    fs::write(f.repo.join("src/alpha.ts"), "export function alpha() {}\n").unwrap();
    let again = f.run();
    assert_eq!(again.state, SourceIndexState::Complete);
    assert_eq!(again.source_hash, done.source_hash);
    assert_ne!(again.snapshot_id, done.snapshot_id);
    let selection = f.service.get(&f.folder()).unwrap();
    assert_eq!(selection.selected.unwrap().job_id, again.job_id);
}

#[test]
fn failed_or_partial_refresh_keeps_the_last_complete_snapshot_selected() {
    let f = Fixture::new();
    f.standard_repo();
    let good = f.run();
    assert_eq!(good.state, SourceIndexState::Complete);

    f.parser.set(Mode::Fail("worker exploded".into()));
    let bad = f.run();
    assert_eq!(bad.state, SourceIndexState::Partial);
    assert!(bad.error.as_deref().unwrap().contains("worker exploded"));
    let selection = f.service.get(&f.folder()).unwrap();
    assert_eq!(selection.selected.as_ref().unwrap().job_id, good.job_id);
    assert_eq!(
        selection.selected.as_ref().unwrap().state,
        SourceIndexState::Complete
    );
    assert_eq!(
        selection.latest_attempt.as_ref().unwrap().job_id,
        bad.job_id
    );
    // The partial attempt is inspectable, and every file says it was not parsed.
    let partial = f
        .service
        .index(bad.snapshot_id.as_deref().unwrap())
        .unwrap();
    assert!(partial
        .files
        .iter()
        .all(|file| file.status == FileStatus::Failed && !file.diagnostics.is_empty()));
    assert!(partial.symbols.is_empty() && partial.is_partial());
    assert!(partial
        .limitations
        .iter()
        .any(|l| l.contains("worker exploded")));

    // A parser-reported partial (output cap reached) is partial too and never replaces the selection.
    f.parser.set(Mode::CapPartial);
    let capped = f.run();
    assert_eq!(capped.state, SourceIndexState::Partial);
    assert_eq!(capped.coverage.as_ref().unwrap()["partial"], true);
    assert!(capped.coverage.as_ref().unwrap()["lastFullyProcessedFile"].is_string());
    assert_eq!(
        f.service.get(&f.folder()).unwrap().selected.unwrap().job_id,
        good.job_id
    );

    // Superseded partial attempts are pruned; the complete snapshot and the newest attempt remain.
    assert!(f.service.read_job(&bad.job_id).is_err());
    assert!(f.service.read_job(&good.job_id).is_ok() && f.service.read_job(&capped.job_id).is_ok());
}

#[test]
fn rejected_parser_output_degrades_to_an_inventory_only_partial_index() {
    let f = Fixture::new();
    f.standard_repo();
    for (mode, expected) in [
        (Mode::Garbage, "not a valid source index"),
        (Mode::WrongHash, "does not belong to this snapshot"),
        (Mode::Oversize, "128 MiB limit"),
        (Mode::TimedOut, "deadline"),
    ] {
        f.parser.set(mode);
        let status = f.run();
        assert_eq!(status.state, SourceIndexState::Partial);
        assert!(
            status.error.as_deref().unwrap().contains(expected),
            "{expected}: {:?}",
            status.error
        );
        let index = f
            .service
            .index(status.snapshot_id.as_deref().unwrap())
            .unwrap();
        assert!(index.symbols.is_empty() && index.sites.is_empty());
        assert_eq!(index.coverage["parsedFiles"], 0);
    }
    assert!(f.service.get(&f.folder()).unwrap().selected.is_none());
}

#[test]
fn missing_runtime_keeps_inventory_viewable_with_an_actionable_error() {
    let f = Fixture::new();
    f.standard_repo();
    *f.parser.caps.lock() = runner::unavailable(
        "graphify",
        "The Graphify Python environment was not found".into(),
    );
    let status = f.run();
    assert_eq!(status.state, SourceIndexState::Partial);
    assert!(status
        .error
        .as_deref()
        .unwrap()
        .contains(runner::PYTHON_OVERRIDE));
    assert!(!status.capabilities.available);
    let index = f
        .service
        .index(status.snapshot_id.as_deref().unwrap())
        .unwrap();
    assert_eq!(index.files.len(), 5);
    assert!(index
        .files
        .iter()
        .all(|file| file.status == FileStatus::Skipped));
}

#[test]
fn cancel_stops_only_this_job_reports_progress_and_discards_its_copy() {
    let f = Fixture::new();
    f.standard_repo();
    let good = f.run();
    f.parser.set(Mode::Wait);
    let started = f.service.start(&f.folder()).unwrap();
    let joined = f.service.start(&f.folder()).unwrap();
    assert_eq!(
        started.job_id, joined.job_id,
        "a second start joins the active job"
    );
    let begin = Instant::now();
    let progress = loop {
        let status = f.service.read_job(&started.job_id).unwrap();
        if status.completed_files == 2 && status.state == SourceIndexState::Parsing {
            break status;
        }
        assert!(
            begin.elapsed() < Duration::from_secs(20),
            "no progress observed: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(progress.total_files, 5);
    assert!(progress.snapshot_id.is_some() && progress.source_hash.is_some());
    let during = f.service.get(&f.folder()).unwrap();
    assert_eq!(during.latest_attempt.unwrap().job_id, started.job_id);
    f.service.cancel(&started.job_id).unwrap();
    let done = f.wait(&started.job_id);
    assert_eq!(done.state, SourceIndexState::Cancelled);
    assert!(done.snapshot_id.is_none() && done.error.is_none());
    let dir = f.service.find_dir(&started.job_id).unwrap();
    assert!(!dir.join("source").exists() && !dir.join("work").exists());
    assert_eq!(
        f.service.get(&f.folder()).unwrap().selected.unwrap().job_id,
        good.job_id
    );
    // Cancelling a finished job is a no-op that returns its terminal status.
    assert_eq!(
        f.service.cancel(&started.job_id).unwrap().state,
        SourceIndexState::Cancelled
    );
}

#[test]
fn staleness_is_metadata_only_and_a_refresh_establishes_a_new_snapshot() {
    let f = Fixture::new();
    f.standard_repo();
    assert!(f.service.get(&f.folder()).unwrap().selected.is_none());
    f.run();
    assert_eq!(
        f.service.get(&f.folder()).unwrap().selected.unwrap().stale,
        Some(false)
    );
    write(&f.repo, "src/new.ts", "export const x = 1;\n");
    assert_eq!(
        f.service.get(&f.folder()).unwrap().selected.unwrap().stale,
        Some(true)
    );
    let refreshed = f.run();
    assert_eq!(
        f.service.get(&f.folder()).unwrap().selected.unwrap().stale,
        Some(false)
    );
    assert_eq!(refreshed.total_files, 6);
    fs::create_dir_all(f.repo.join("brand-new-empty")).unwrap();
    assert_eq!(
        f.service.get(&f.folder()).unwrap().selected.unwrap().stale,
        Some(true),
        "a new empty directory changes the inventory"
    );
}

#[test]
fn oversized_and_binary_files_are_inventoried_but_never_handed_to_the_parser() {
    let f = Fixture::new();
    f.standard_repo();
    write(&f.repo, "big.txt", vec![b'a'; 2 * 1024 * 1024 + 1]);
    write(&f.repo, "blob.bin", b"ab\0cd");
    let done = f.run();
    assert_eq!(done.state, SourceIndexState::Complete, "{:?}", done.error);
    let index = f
        .service
        .index(done.snapshot_id.as_deref().unwrap())
        .unwrap();
    let big = index
        .files
        .iter()
        .find(|file| file.path == "big.txt")
        .unwrap();
    assert_eq!(big.status, FileStatus::Skipped);
    assert!(big.diagnostics[0].contains("parser limit"));
    let blob = index
        .files
        .iter()
        .find(|file| file.path == "blob.bin")
        .unwrap();
    assert_eq!((blob.status, blob.bytes), (FileStatus::Skipped, 5));
    assert!(blob.diagnostics[0].contains("binary"));
    assert_eq!(index.coverage["skippedFiles"], 2);
}

#[test]
fn linked_entries_are_not_followed_into_the_snapshot() {
    let f = Fixture::new();
    f.standard_repo();
    let outside = f.temp.0.join("outside");
    write(&outside, "secret.txt", "outside secret");
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&outside, f.repo.join("link-dir")).and_then(|_| {
        std::os::unix::fs::symlink(outside.join("secret.txt"), f.repo.join("link.txt"))
    });
    #[cfg(windows)]
    let linked =
        std::os::windows::fs::symlink_dir(&outside, f.repo.join("link-dir")).and_then(|_| {
            std::os::windows::fs::symlink_file(outside.join("secret.txt"), f.repo.join("link.txt"))
        });
    if let Err(e) = linked {
        eprintln!("SKIPPED link assertions: this account cannot create symbolic links ({e})");
        return;
    }
    let done = f.run();
    assert_eq!(done.state, SourceIndexState::Complete, "{:?}", done.error);
    let index = f
        .service
        .index(done.snapshot_id.as_deref().unwrap())
        .unwrap();
    assert!(!paths(&index)
        .iter()
        .any(|p| p.contains("link") || p.contains("secret")));
    assert!(!index.directories.iter().any(|d| d.contains("link")));
    assert!(index
        .limitations
        .iter()
        .any(|l| l.contains("Skipped link/junction/reparse point")));
    let root = f
        .service
        .snapshot_root(done.snapshot_id.as_deref().unwrap())
        .unwrap();
    assert!(!root.join("link.txt").exists() && !root.join("link-dir").exists());
}

#[test]
fn case_colliding_paths_are_reported_not_merged() {
    assert_eq!(
        inventory::case_conflicts(["a/B.ts", "a/b.ts", "README", "readme", "x"].into_iter()),
        ["a/B.ts / a/b.ts", "README / readme"]
    );
    assert!(inventory::case_conflicts(["Ünï.ts", "other"].into_iter()).is_empty());
    // Different case in different directories is fine only when the directories themselves differ in more than case.
    assert_eq!(
        inventory::case_conflicts(["Src", "src"].into_iter()).len(),
        1
    );
}

#[test]
fn folder_and_id_errors_are_explicit() {
    let f = Fixture::new();
    assert!(f
        .service
        .start("")
        .unwrap_err()
        .contains("Choose a project folder"));
    let missing = f.temp.0.join("nowhere").to_string_lossy().into_owned();
    assert!(f.service.start(&missing).is_err() && f.service.get(&missing).is_err());
    for bad in [
        "../x",
        "not-a-uuid",
        "",
        "..\\..\\x",
        "00000000-0000-0000-0000-000000000000",
    ] {
        assert!(f.service.read_job(bad).is_err(), "{bad}");
        assert!(f.service.cancel(bad).is_err(), "{bad}");
        assert!(f.service.index(bad).is_err(), "{bad}");
        assert!(f.service.snapshot_root(bad).is_err(), "{bad}");
        assert!(
            f.service
                .query(
                    bad,
                    SourceIndexQuery::Files {
                        offset: 0,
                        limit: 1
                    }
                )
                .is_err(),
            "{bad}"
        );
    }
    // An empty repository is an empty inventory, not an error.
    let done = f.run();
    assert_eq!(done.state, SourceIndexState::Complete);
    let index = f
        .service
        .index(done.snapshot_id.as_deref().unwrap())
        .unwrap();
    assert!(index.files.is_empty() && index.directories.is_empty());
}

#[test]
fn saved_indexes_survive_a_restart_and_reload_through_validation() {
    let f = Fixture::new();
    f.standard_repo();
    let done = f.run();
    let id = done.snapshot_id.clone().unwrap();
    let restarted = SourceIndexService::with_parser(f.temp.0.join("config"), f.parser.clone());
    let selection = restarted.get(&f.folder()).unwrap();
    assert_eq!(
        selection.selected.as_ref().unwrap().snapshot_id.as_deref(),
        Some(id.as_str())
    );
    assert_eq!(
        selection.latest_attempt.unwrap().state,
        SourceIndexState::Complete
    );
    let page = restarted
        .query(
            &id,
            SourceIndexQuery::Symbols {
                offset: 0,
                limit: 10,
            },
        )
        .unwrap();
    assert_eq!(page.total, 4);
    // A tampered saved index is refused rather than served.
    let dir = restarted.find_dir(&id).unwrap();
    let mut saved: Value =
        serde_json::from_slice(&fs::read(dir.join("index.json")).unwrap()).unwrap();
    saved["edges"][0]["targetId"] = json!("missing");
    fs::write(dir.join("index.json"), serde_json::to_vec(&saved).unwrap()).unwrap();
    let again = SourceIndexService::with_parser(f.temp.0.join("config"), f.parser.clone());
    assert!(again.index(&id).unwrap_err().contains("missing endpoint"));
}

// ---- validation ------------------------------------------------------------------------------------------------

fn synthetic() -> (Value, Vec<ManifestFile>, Vec<String>) {
    let entries = [
        ("a.txt", "pending"),
        ("big.bin", "skipped"),
        ("dir/c.txt", "pending"),
        ("dir/d.txt", "pending"),
        ("e.txt", "pending"),
    ];
    let files: Vec<ManifestFile> = entries
        .iter()
        .map(|(path, status)| ManifestFile {
            id: model::file_id(path),
            path: path.to_string(),
            sha256: model::stable_id(&["content", path]),
            bytes: 100,
            language: None,
            status: status.to_string(),
            diagnostics: if *status == "skipped" {
                vec!["Not parsed: binary content".into()]
            } else {
                vec![]
            },
        })
        .collect();
    let directories = vec!["dir".to_string()];
    let manifest = json!({ "schemaVersion": 1, "snapshotId": "snap", "folder": "/repo", "sourceHash": "hash", "root": "/unused", "files": files, "directories": directories });
    (manifest, files, directories)
}

fn check(change: impl FnOnce(&mut Value)) -> Result<SourceGraphIndex, String> {
    let (manifest, files, directories) = synthetic();
    let mut index = fake_index(&manifest);
    change(&mut index);
    let expected = Expected {
        snapshot_id: "snap",
        folder: "/repo",
        source_hash: "hash",
        files: &files,
        directories: &directories,
    };
    let mut parsed: SourceGraphIndex = serde_json::from_value(index).map_err(|e| e.to_string())?;
    parsed.finish(&expected)?;
    Ok(parsed)
}

#[test]
fn valid_facts_pass_and_derived_counts_replace_parser_claims() {
    let index = check(|v| {
        v["coverage"]["parsedFiles"] = json!(999);
        v["coverage"]["encounteredSites"] = json!(0);
    })
    .unwrap();
    assert_eq!(
        (
            index.coverage["parsedFiles"].as_u64(),
            index.coverage["skippedFiles"].as_u64()
        ),
        (Some(4), Some(1))
    );
    assert_eq!(index.coverage["encounteredSites"], 6);
    assert_eq!(index.coverage["resolvedSites"], 4);
    assert_eq!(index.coverage["partial"], false);
    assert!(index.lookup.edges_in.contains_key(&index.symbols[0].id));
}

#[test]
fn contradictory_or_dangling_facts_are_rejected() {
    fn rejects(name: &str, expected: &str, change: impl FnOnce(&mut Value)) {
        let error = check(change)
            .err()
            .unwrap_or_else(|| panic!("{name} was accepted"));
        assert!(error.contains(expected), "{name}: {error}");
    }
    rejects("resolved without target", "resolvedStatic", |v| {
        v["sites"][0]["targetId"] = Value::Null
    });
    rejects("candidate with a target", "resolvedStatic", |v| {
        let t = v["symbols"][1]["id"].clone();
        v["sites"][5]["targetId"] = t;
    });
    rejects("unresolved with a target", "resolvedStatic", |v| {
        let t = v["symbols"][1]["id"].clone();
        v["sites"][4]["targetId"] = t;
    });
    rejects("dangling edge", "missing endpoint", |v| {
        v["edges"][0]["targetId"] = json!("nope")
    });
    rejects("edge without evidence", "no site evidence", |v| {
        v["edges"][0]["evidenceIds"] = json!([])
    });
    rejects("edge with unknown evidence", "unknown site", |v| {
        v["edges"][0]["evidenceIds"] = json!(["nope"])
    });
    rejects(
        "evidence from another file",
        "outside its source file",
        |v| {
            let s = v["sites"][1]["id"].clone();
            v["edges"][0]["evidenceIds"] = json!([s]);
        },
    );
    rejects("edge not backed by its site", "not backed", |v| {
        let t = v["symbols"][3]["id"].clone();
        v["edges"][0]["targetId"] = t;
    });
    rejects("duplicate symbol", "Duplicate", |v| {
        let s = v["symbols"][0].clone();
        v["symbols"].as_array_mut().unwrap().push(s);
    });
    rejects("span past end of file", "invalid source span", |v| {
        v["symbols"][0]["span"]["byteEnd"] = json!(101)
    });
    rejects("inverted span", "invalid source span", |v| {
        v["symbols"][0]["span"]["byteStart"] = json!(9);
        v["symbols"][0]["span"]["byteEnd"] = json!(2);
    });
    rejects("zero line", "invalid source span", |v| {
        v["sites"][0]["span"]["lineStart"] = json!(0)
    });
    rejects("unknown owner", "unknown owner", |v| {
        v["symbols"][0]["ownerId"] = json!("nope")
    });
    rejects("cross-file owner", "owned across files", |v| {
        let o = v["symbols"][1]["id"].clone();
        v["symbols"][0]["ownerId"] = o;
    });
    rejects("owner cycle", "containment cycle", |v| {
        let (a, b) = (v["symbols"][1]["id"].clone(), v["symbols"][2]["id"].clone());
        v["symbols"][1]["ownerId"] = b;
        v["symbols"][2]["ownerId"] = a;
    });
    rejects("self owner", "containment cycle", |v| {
        let a = v["symbols"][0]["id"].clone();
        v["symbols"][0]["ownerId"] = a;
    });
    rejects("site in unknown file", "unknown file", |v| {
        v["sites"][0]["fileId"] = json!("nope")
    });
    rejects("site caller in another file", "another file", |v| {
        let c = v["symbols"][1]["id"].clone();
        v["sites"][0]["callerId"] = c;
    });
    rejects("unknown candidate", "unknown target", |v| {
        v["sites"][5]["candidateIds"] = json!(["nope"])
    });
    rejects("other snapshot", "does not belong", |v| {
        v["snapshotId"] = json!("other")
    });
    rejects(
        "changed file hash",
        "differs from the snapshot inventory",
        |v| v["files"][0]["sha256"] = json!("changed"),
    );
    rejects("renamed file", "differs from the snapshot inventory", |v| {
        v["files"][0]["path"] = json!("renamed.txt")
    });
    rejects("relabelled skipped file", "re-labelled", |v| {
        v["files"][1]["status"] = json!("parsed")
    });
    rejects("missing file", "lists 4 files", |v| {
        v["files"].as_array_mut().unwrap().pop();
    });
    rejects("changed directories", "directory list", |v| {
        v["directories"] = json!(["dir", "extra"])
    });
    rejects("unknown resolution", "unknown variant", |v| {
        v["sites"][0]["resolution"] = json!("probably")
    });
    rejects("coverage not an object", "coverage", |v| {
        v["coverage"] = json!([])
    });
    rejects("schema version", "schema version", |v| {
        v["schemaVersion"] = json!(2)
    });
}

#[test]
fn unknown_fields_are_preserved_losslessly() {
    let index = check(|v| {
        v["symbols"][0]["futureField"] = json!({ "x": 1 });
        v["symbols"][0]["bindings"] = json!({ "declarations": [{ "name": "alpha" }] });
    })
    .unwrap();
    let saved = serde_json::to_value(&index.symbols[0]).unwrap();
    assert_eq!(saved["futureField"], json!({ "x": 1 }));
    assert_eq!(saved["bindings"]["declarations"][0]["name"], "alpha");
}

// ---- queries ---------------------------------------------------------------------------------------------------

fn indexed() -> (Fixture, String) {
    let f = Fixture::new();
    f.standard_repo();
    let done = f.run();
    assert_eq!(done.state, SourceIndexState::Complete, "{:?}", done.error);
    let id = done.snapshot_id.unwrap();
    (f, id)
}

fn names(page: &SourceIndexPage) -> Vec<&str> {
    page.items
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect()
}

#[test]
fn pages_are_bounded_and_the_query_union_is_strict() {
    let (f, id) = indexed();
    let page = f
        .service
        .query(
            &id,
            SourceIndexQuery::Files {
                offset: 0,
                limit: 2,
            },
        )
        .unwrap();
    assert_eq!(
        (page.total, page.offset, page.next_offset, page.items.len()),
        (5, 0, Some(2), 2)
    );
    let page = f
        .service
        .query(
            &id,
            SourceIndexQuery::Files {
                offset: 4,
                limit: 2,
            },
        )
        .unwrap();
    assert_eq!((page.items.len(), page.next_offset), (1, None));
    let page = f
        .service
        .query(
            &id,
            SourceIndexQuery::Files {
                offset: 99,
                limit: 2,
            },
        )
        .unwrap();
    assert!(page.items.is_empty() && page.next_offset.is_none());
    for limit in [0, 201] {
        assert!(
            f.service
                .query(&id, SourceIndexQuery::Files { offset: 0, limit })
                .is_err(),
            "{limit}"
        );
    }
    let wire = |v: Value| serde_json::from_value::<SourceIndexQuery>(v);
    assert!(wire(json!({ "kind": "files", "offset": 0, "limit": 5 })).is_ok());
    assert!(wire(
        json!({ "kind": "overview", "level": "files", "groupBy": "scc", "offset": 0, "limit": 5 })
    )
    .is_ok());
    assert!(wire(json!({ "kind": "files", "offset": 0, "limit": 5, "path": "/etc" })).is_err());
    assert!(wire(json!({ "kind": "readFile", "path": "x" })).is_err());
    assert!(wire(
        json!({ "kind": "search", "level": "classes", "text": "a", "offset": 0, "limit": 5 })
    )
    .is_err());
    assert!(wire(json!({ "kind": "files", "offset": -1, "limit": 5 })).is_err());
    let coverage = f
        .service
        .query(
            &id,
            SourceIndexQuery::Coverage {
                offset: 0,
                limit: 50,
            },
        )
        .unwrap();
    assert_eq!(coverage.items[0]["kind"], "summary");
    assert_eq!(coverage.items[0]["admittedFiles"], 5);
    assert!(
        coverage.items.iter().any(|i| i["kind"] == "language")
            && coverage.items.iter().any(|i| i["kind"] == "limitation")
    );
    let next = f
        .service
        .query(
            &id,
            SourceIndexQuery::Coverage {
                offset: 1,
                limit: 1,
            },
        )
        .unwrap();
    assert_eq!(next.total, coverage.total);
    assert_eq!(next.items, coverage.items[1..2]);
    assert_eq!(next.next_offset, Some(2));
    let beyond = f
        .service
        .query(
            &id,
            SourceIndexQuery::Coverage {
                offset: 99,
                limit: 5,
            },
        )
        .unwrap();
    assert_eq!(beyond.total, coverage.total);
    assert!(beyond.items.is_empty() && beyond.next_offset.is_none());
}

#[test]
fn an_oversized_page_is_an_explicit_error() {
    let (_, mut files, directories) = synthetic();
    let heavy = "x".repeat(8000);
    files.extend((0..300).map(|i| ManifestFile {
        id: model::file_id(&format!("f{i}")),
        path: format!("f{i}"),
        sha256: "s".into(),
        bytes: 1,
        language: None,
        status: "pending".into(),
        diagnostics: vec![],
    }));
    let expected = Expected {
        snapshot_id: "snap",
        folder: "/repo",
        source_hash: "hash",
        files: &files,
        directories: &directories,
    };
    let index =
        SourceGraphIndex::inventory_only(&expected, FileStatus::Failed, &heavy, "reason", vec![])
            .unwrap();
    let error = query::run(
        &index,
        &SourceIndexQuery::Files {
            offset: 0,
            limit: 200,
        },
    )
    .unwrap_err();
    assert!(error.contains("1 MiB"), "{error}");
    assert_eq!(
        query::run(
            &index,
            &SourceIndexQuery::Files {
                offset: 0,
                limit: 20
            }
        )
        .unwrap()
        .items
        .len(),
        20
    );
    assert!(query::run(
        &index,
        &SourceIndexQuery::Files {
            offset: 280,
            limit: 200
        }
    )
    .unwrap()
    .next_offset
    .is_none());
}

#[test]
fn search_ranks_exact_case_first_and_never_merges_case_variants() {
    let (f, id) = indexed();
    let search = |level, text: &str| {
        f.service
            .query(
                &id,
                SourceIndexQuery::Search {
                    level,
                    text: text.into(),
                    offset: 0,
                    limit: 20,
                },
            )
            .unwrap()
    };
    // gamma lives in src/beta.ts, so a path match ranks it between the exact-case and case-folded hits.
    assert_eq!(
        names(&search(Level::Functions, "beta")),
        ["beta", "gamma", "Beta"]
    );
    assert_eq!(
        names(&search(Level::Functions, "Beta")),
        ["Beta", "beta", "gamma"]
    );
    assert_eq!(names(&search(Level::Functions, "gam")), ["gamma"]);
    assert!(search(Level::Functions, "nothing-like-this")
        .items
        .is_empty());
    let files = search(Level::Files, "ø GAMMA");
    assert_eq!(files.items.len(), 1);
    assert_eq!(files.items[0]["path"], "Æble/ø gamma.py");
    assert_eq!(search(Level::Files, "").total, 5);
    assert!(f
        .service
        .query(
            &id,
            SourceIndexQuery::Search {
                level: Level::Files,
                text: "x".repeat(600),
                offset: 0,
                limit: 5
            }
        )
        .is_err());
}

#[test]
fn neighbors_expose_edges_evidence_and_unresolved_sites() {
    let (f, id) = indexed();
    let index = f.service.index(&id).unwrap();
    let symbol = |name: &str| {
        index
            .symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap()
            .id
            .clone()
    };
    let neighbors = |level, node: &str, direction| {
        f.service
            .query(
                &id,
                SourceIndexQuery::Neighbors {
                    level,
                    id: node.into(),
                    direction,
                    offset: 0,
                    limit: 50,
                },
            )
            .unwrap()
    };
    let out = neighbors(Level::Functions, &symbol("alpha"), Direction::Out);
    let kinds: Vec<&str> = out
        .items
        .iter()
        .map(|i| i["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["edge", "site", "site"],
        "one resolved edge plus the dynamic and candidate sites with no target"
    );
    assert_eq!(out.items[0]["peerLabel"], "beta");
    assert_eq!(out.items[0]["sites"][0]["raw"], "beta()");
    let unresolved: Vec<&str> = out.items[1..]
        .iter()
        .map(|i| i["site"]["resolution"].as_str().unwrap())
        .collect();
    assert_eq!(unresolved, ["unresolved", "candidate"]);
    let incoming = neighbors(Level::Functions, &symbol("alpha"), Direction::In);
    assert_eq!(incoming.items.len(), 1);
    assert_eq!(incoming.items[0]["direction"], "in");
    assert_eq!(incoming.items[0]["peerLabel"], "beta");
    // Recursion: the self edge is reported once, not as both directions.
    let both = neighbors(Level::Functions, &symbol("beta"), Direction::Both);
    assert_eq!(both.items.iter().filter(|i| i["self"] == true).count(), 1);
    assert_eq!(
        both.items.len(),
        3,
        "two outgoing edges and one incoming edge; the self edge is not repeated"
    );
    // Isolated declarations answer with an empty page, not an error.
    assert!(
        neighbors(Level::Functions, &symbol("gamma"), Direction::Both)
            .items
            .is_empty()
    );
    // File level: the import edge, plus call aggregates derived from function edges and labelled as such.
    let a = index.symbol(&symbol("alpha")).unwrap().file_id.clone();
    let file_neighbors = neighbors(Level::Files, &a, Direction::Both);
    assert!(file_neighbors
        .items
        .iter()
        .any(|i| i["kind"] == "edge" && i["edge"]["relation"] == "imports"));
    let aggregates: Vec<&Value> = file_neighbors
        .items
        .iter()
        .filter(|i| i["kind"] == "aggregate")
        .collect();
    assert_eq!(aggregates.len(), 2);
    assert!(aggregates.iter().all(|i| i["relation"] == "calls"
        && i["count"] == 1
        && i["edgeIds"].as_array().unwrap().len() == 1));
    // Level/kind mismatches and unknown ids are rejected.
    assert!(f
        .service
        .query(
            &id,
            SourceIndexQuery::Neighbors {
                level: Level::Files,
                id: symbol("alpha"),
                direction: Direction::Out,
                offset: 0,
                limit: 5
            }
        )
        .is_err());
    assert!(f
        .service
        .query(
            &id,
            SourceIndexQuery::Neighbors {
                level: Level::Functions,
                id: "nope".into(),
                direction: Direction::Out,
                offset: 0,
                limit: 5
            }
        )
        .is_err());
}

#[test]
fn overview_groups_cover_every_node_and_keep_cycles_and_isolated_nodes() {
    let (f, id) = indexed();
    let overview = |level, group_by| {
        f.service
            .query(
                &id,
                SourceIndexQuery::Overview {
                    level,
                    group_by,
                    offset: 0,
                    limit: 200,
                },
            )
            .unwrap()
    };
    let scc = overview(Level::Functions, GroupBy::Scc);
    assert_eq!(
        scc.total, 3,
        "the alpha/beta cycle, Beta and the isolated gamma"
    );
    let members: usize = scc
        .items
        .iter()
        .map(|g| g["memberCount"].as_u64().unwrap() as usize)
        .sum();
    assert_eq!(members, 4);
    let cycle: Vec<&Value> = scc.items.iter().filter(|g| g["cyclic"] == true).collect();
    assert_eq!(cycle.len(), 1);
    assert_eq!(cycle[0]["memberCount"], 2);
    assert_eq!(cycle[0]["kind"], "scc");
    assert!(scc
        .items
        .iter()
        .any(|g| g["label"] == "gamma" && g["memberCount"] == 1 && g["linksTotal"] == 0));
    // Directory grouping includes empty directories and aggregates cross-directory arrows with their evidence ids.
    let directories = overview(Level::Files, GroupBy::Directory);
    let ids: Vec<&str> = directories
        .items
        .iter()
        .map(|g| g["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["dir:", "dir:empty", "dir:keys", "dir:src", "dir:Æble"]
    );
    let root = &directories.items[0];
    assert_eq!(root["memberCount"], 2, ".gitignore and README.md");
    assert_eq!(directories.items[1]["memberCount"], 0);
    let links = root["links"].as_array().unwrap();
    let import = links.iter().find(|l| l["relation"] == "imports").unwrap();
    assert_eq!(
        (
            import["targetGroupId"].as_str(),
            import["derived"].as_bool(),
            import["count"].as_u64()
        ),
        (Some("dir:src"), Some(false), Some(1))
    );
    assert_eq!(import["edgeIds"].as_array().unwrap().len(), 1);
    let call = links.iter().find(|l| l["relation"] == "calls").unwrap();
    assert_eq!(
        (call["derived"].as_bool(), call["resolution"].as_str()),
        (Some(true), Some("resolvedStatic")),
        "function edges lifted to files are labelled as derived"
    );
    assert_eq!(
        directories.items[3]["parentId"], "dir:",
        "src hangs off the root group"
    );
    // Strongly connected files: the two files that import and call each other collapse into one cyclic group.
    let files = overview(Level::Files, GroupBy::Scc);
    assert_eq!(files.total, 4);
    assert_eq!(
        files.items.iter().filter(|g| g["cyclic"] == true).count(),
        1
    );
    // Function directory grouping keeps every function as a member of exactly one group.
    let functions = overview(Level::Functions, GroupBy::Directory);
    assert_eq!(
        functions
            .items
            .iter()
            .map(|g| g["memberCount"].as_u64().unwrap())
            .sum::<u64>(),
        4
    );
    // Pages of groups continue where the previous page ended.
    let page = f
        .service
        .query(
            &id,
            SourceIndexQuery::Overview {
                level: Level::Files,
                group_by: GroupBy::Directory,
                offset: 1,
                limit: 2,
            },
        )
        .unwrap();
    assert_eq!(
        (page.total, page.items.len(), page.next_offset),
        (5, 2, Some(3))
    );
}

#[test]
fn components_are_computed_without_recursion_and_sinks_come_first() {
    let n = 200_000u32;
    let chain: Vec<Vec<u32>> = (0..n)
        .map(|i| if i + 1 < n { vec![i + 1] } else { Vec::new() })
        .collect();
    let components = query::tarjan(&chain);
    assert_eq!(components.len(), n as usize);
    assert_eq!(components[0], [n - 1], "the sink completes first");
    let mut ring = chain.clone();
    ring[n as usize - 1].push(0);
    let components = query::tarjan(&ring);
    assert_eq!(components.len(), 1);
    assert_eq!(components[0].len(), n as usize);
    assert_eq!(
        query::tarjan(&[vec![0]]).len(),
        1,
        "a self loop is still one component"
    );
}

// ---- runner ----------------------------------------------------------------------------------------------------

#[test]
fn interpreter_discovery_prefers_the_explicit_override_and_never_guesses() {
    let t = Temp::new();
    let exe = std::env::current_exe().unwrap();
    let found = runner::discover_interpreter(Some(exe.clone().into_os_string()), None).unwrap();
    assert_eq!((found.path, found.source), (exe, "environment"));
    let (source, message) =
        runner::discover_interpreter(Some(t.0.join("missing").into_os_string()), None).unwrap_err();
    assert!(
        source == "environment" && message.contains(runner::PYTHON_OVERRIDE),
        "{message}"
    );
    if cfg!(windows) {
        // An empty override is ignored; the Graphify environment is looked up only at its documented place.
        let (source, message) =
            runner::discover_interpreter(Some(OsString::new()), Some(t.0.clone().into_os_string()))
                .unwrap_err();
        assert!(
            source == "graphify" && message.contains("Graphify"),
            "{message}"
        );
        assert!(runner::discover_interpreter(None, None).is_err());
        write(&t.0, "Graphify/venv/Scripts/python.exe", "stub");
        let found = runner::discover_interpreter(None, Some(t.0.clone().into_os_string())).unwrap();
        assert_eq!(found.source, "graphify");
    }
}

#[test]
fn capability_reports_must_show_usable_grammars() {
    let interpreter = runner::Interpreter {
        path: PathBuf::from("python"),
        source: "graphify",
    };
    let report = br#"{"runtime":{"python":"3.12.1","treeSitter":"0.25.2"},"profiles":[
        {"language":"python","version":"0.25.0","available":true,"error":null,"nodeKinds":["function_definition"]},
        {"language":"go","version":null,"available":false,"error":"No module named tree_sitter_go","nodeKinds":[]}]}"#;
    let ok = runner::parse_probe(&interpreter, report);
    assert!(ok.available && ok.probed && ok.hint.is_none());
    assert_eq!(
        (
            ok.runtime.version.as_deref(),
            ok.runtime.tree_sitter.as_deref(),
            ok.runtime.source.as_str()
        ),
        (Some("3.12.1"), Some("0.25.2"), "graphify")
    );
    assert_eq!(ok.profiles.len(), 2);
    assert_eq!(
        ok.profiles[1].error.as_deref(),
        Some("No module named tree_sitter_go"),
        "an unavailable grammar is reported, not dropped"
    );
    let none = runner::parse_probe(&interpreter, br#"{"runtime":{"python":"3.12","treeSitter":null},"profiles":[{"language":"go","version":null,"available":false,"error":"missing"}]}"#);
    assert!(
        !none.available
            && none
                .hint
                .as_deref()
                .unwrap()
                .contains(runner::PYTHON_OVERRIDE)
    );
    assert_eq!(none.profiles.len(), 1);
    let detailed = runner::parse_probe(&interpreter, br#"{"runtime":{"python":"3.11.1","pythonFull":"3.11.1 (x)","treeSitter":{"version":"0.25.2","languageVersion":15,"minCompatibleLanguageVersion":13,"error":null}},"profiles":[{"language":"python","version":"0.25.0","available":true,"error":null,"nodeKinds":["function_definition"]}]}"#);
    assert!(
        detailed.available && detailed.runtime.error.is_none(),
        "the worker's real report shape is accepted: {:?}",
        detailed.runtime.error
    );
    assert_eq!(detailed.runtime.tree_sitter.as_deref(), Some("0.25.2"));
    let malformed = runner::parse_probe(&interpreter, b"Traceback (most recent call last)");
    assert!(
        !malformed.available
            && malformed
                .runtime
                .error
                .as_deref()
                .unwrap()
                .contains("not valid")
    );
}

#[test]
fn worker_scripts_are_materialized_from_the_build_and_repaired_when_altered() {
    let t = Temp::new();
    let directory = runner::materialize_scripts(&t.0).unwrap();
    assert!(directory.join("worker.py").is_file());
    for (name, body) in runner::SCRIPTS {
        assert_eq!(
            fs::read_to_string(directory.join(name)).unwrap(),
            *body,
            "{name}"
        );
    }
    assert!(
        runner::SCRIPTS
            .iter()
            .all(|(name, _)| !name.starts_with("test")),
        "test files are never shipped"
    );
    fs::write(directory.join("worker.py"), "tampered").unwrap();
    let again = runner::materialize_scripts(&t.0).unwrap();
    assert_eq!(again, directory);
    assert_ne!(
        fs::read_to_string(again.join("worker.py")).unwrap(),
        "tampered"
    );
}

#[test]
fn real_worker_end_to_end_when_a_runtime_is_available() {
    let t = Temp::new();
    let parser = Arc::new(
        runner::PythonParser::try_new(&t.0.join("config").join("system-overview")).unwrap(),
    );
    let capabilities = parser.probe();
    if !capabilities.available
        || !capabilities
            .profiles
            .iter()
            .any(|p| p.language == "python" && p.available)
    {
        eprintln!(
            "SKIPPED real worker test: no usable Python/Tree-sitter runtime ({:?})",
            capabilities.runtime.error
        );
        return;
    }
    let service = SourceIndexService::with_parser(t.0.join("config"), parser);
    let repo = t.0.join("repo");
    write(&repo, "pkg/__init__.py", "");
    write(&repo, "pkg/a.py", "from .b import helper\n\n\ndef caller():\n    return helper()\n\n\ndef lonely():\n    pass\n");
    write(
        &repo,
        "pkg/b.py",
        "def helper():\n    return 1\n\n\ndef helper2():\n    return helper()\n",
    );
    write(&repo, "notes.txt", "plain text");
    let started = service.start(&repo.to_string_lossy()).unwrap();
    let done = wait_for(&service, &started.job_id);
    assert_eq!(done.state, SourceIndexState::Complete, "{:?}", done.error);
    let index = service.index(done.snapshot_id.as_deref().unwrap()).unwrap();
    for name in ["caller", "helper", "helper2", "lonely"] {
        assert!(index.symbols.iter().any(|s| s.name == name), "{name}");
    }
    assert!(
        index
            .sites
            .iter()
            .any(|s| s.raw.as_deref().is_some_and(|r| r.contains("helper"))),
        "call sites are retained even when unresolved"
    );
    assert_eq!(
        index
            .files
            .iter()
            .find(|f| f.path == "notes.txt")
            .unwrap()
            .status,
        FileStatus::Unsupported
    );
    assert_eq!(
        index
            .files
            .iter()
            .find(|f| f.path == "pkg/a.py")
            .unwrap()
            .status,
        FileStatus::Parsed
    );
}
