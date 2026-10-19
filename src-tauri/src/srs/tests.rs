//! Service-level behavior with an injected clock, scratch storage and no Architect, provider or network.
use super::*;
use crate::knowledge::{ScopeRequest, VaultMode};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

const DAY: u64 = 24 * 60 * 60 * 1000;

struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn now(&self) -> u64 { self.0.load(AtomicOrdering::SeqCst) }
}
impl TestClock {
    fn advance(&self, by: u64) { self.0.fetch_add(by, AtomicOrdering::SeqCst); }
}

struct Fix {
    root: PathBuf,
    repo: PathBuf,
    knowledge: Arc<KnowledgeService>,
    clock: Arc<TestClock>,
    service: Arc<SrsService>,
    vault_id: String,
    job_id: String,
    producer: DependencyProducer,
}

impl Drop for Fix {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.root); }
}

fn producer() -> DependencyProducer {
    DependencyProducer { instance_id: "architect".into(), generation: 1, launch_id: "launch".into(), session_id: "session".into() }
}

impl Fix {
    fn new(state: &str, phase: &str, stage_state: &str) -> Self {
        let root = crate::setup::canonical(&{
            let path = std::env::temp_dir().join(format!("omp srs ø {}", store::id()));
            std::fs::create_dir_all(&path).unwrap();
            path
        }).unwrap();
        let home = root.join("home");
        let repo = root.join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src").join("a.txt"), "Visitors can book an appointment.\n").unwrap();
        let repo = crate::setup::canonical(&repo).unwrap();
        let knowledge = KnowledgeService::new(root.join("config"), home);
        let prepared = knowledge.prepare_knowledge_vault(ScopeRequest::Project { repository_root: None }, Some(repo.to_string_lossy().as_ref()), "", VaultMode::Markdown).unwrap();
        let vault = knowledge.confirm_knowledge_vault(&prepared.token).unwrap();
        let clock = Arc::new(TestClock(AtomicU64::new(1_000)));
        let service = SrsService::with_clock(root.join("config"), knowledge.clone(), clock.clone());
        let mut fix = Self { root, repo, knowledge, clock, service, vault_id: vault.vault_id, job_id: store::id(), producer: producer() };
        fix.write_job(state, phase, stage_state);
        fix
    }

    fn write_job(&mut self, state: &str, phase: &str, stage_state: &str) {
        let vault = self.knowledge.vault_root(&self.vault_id).unwrap();
        let admitted = matches!(stage_state, "queued" | "active" | "committed");
        let job: Job = serde_json::from_value(json!({
            "jobId": self.job_id,
            "scope": {"folder": self.repo, "repositoryRoot": self.repo, "vaultId": self.vault_id, "vaultRoot": vault.root, "vaultMode": "markdown",
                "roots": [{"path": self.repo, "kind": "repository", "rationale": "fixture"}], "exclusions": [], "template": {"revision": "fixture"}},
            "revision": 1, "state": state, "phase": phase, "scopeHash": "scope", "sourceHash": null, "attemptId": "attempt",
            "producer": self.producer, "nextStage": 2, "nextRequirement": 0,
            "stage": {"stageId": "stage", "number": 1, "phase": phase, "state": stage_state, "attemptId": "attempt", "nonce": "nonce", "startedAt": 1, "endedAt": null, "summary": null, "admitted": admitted},
            "counts": {"areas": 0, "evidenceFiles": 0, "work": 0, "workOpen": 0, "documents": 0, "requirements": 0, "questions": 0, "questionsOpen": 0, "blocked": 0, "excluded": 0},
            "checkpoint": null, "error": null, "createdAt": 1, "updatedAt": 1, "completion": null, "inventoryComplete": true,
            "humanExclusions": 0, "documentRevision": 0, "workRevision": 0
        })).unwrap();
        self.service.store.commit(&job, vec![], json!({"fixture": true})).unwrap();
    }

    fn job(&self) -> Job { self.service.store.job(&self.job_id).unwrap() }

    fn call(&self, action: &str, extra: Value) -> Result<Value> {
        let stage = self.job().stage.as_ref().map(|s| s.stage_id.clone()).unwrap_or_default();
        let mut payload = json!({"action": action, "jobId": self.job_id, "attemptId": "attempt", "stageId": stage});
        for (key, value) in extra.as_object().unwrap() { payload[key] = value.clone(); }
        self.service.handle_tool(&self.producer, &payload)
    }

    /// What the scheduler would do after a committed stage: claim, send and have the native bridge admit the next one.
    fn next_stage(&self, number: u64) {
        let mut job = self.job();
        let phase = job.phase;
        job.stage = Some(Stage { stage_id: format!("stage-{number}"), number, phase, state: "active".into(), attempt_id: "attempt".into(), nonce: format!("nonce-{number}"), started_at: 1, ended_at: None, summary: None, admitted: true });
        job.state = if phase == Phase::Verify { JobState::Reviewing } else { JobState::Running };
        self.service.save(&mut job, vec![], "test: next stage").unwrap();
    }

    fn checkpoint(&self, updates: Value) -> Result<Value> {
        self.call("srs_checkpoint", json!({"expectedRevision": self.job().revision, "updates": updates}))
    }

    fn restart(&mut self) {
        self.service = SrsService::with_clock(self.root.join("config"), self.knowledge.clone(), self.clock.clone());
    }

    /// Simulates the human's explicit Resume after a restart: a new running attempt over the saved ledger.
    fn resume(&mut self) {
        let mut job = self.job();
        job.state = JobState::Running;
        job.error = None;
        job.stage.as_mut().unwrap().state = "active".into();
        job.stage.as_mut().unwrap().admitted = true;
        self.service.save(&mut job, vec![], "test resume").unwrap();
    }
}

#[test]
fn work_continues_across_days_and_restarts_with_more_than_500_stable_requirements() {
    let mut fix = Fix::new("running", "specify", "active");
    let mut ids = std::collections::HashSet::new();
    for batch in 0..6 {
        fix.clock.advance(DAY);
        let requirements: Vec<Value> = (0..100).map(|n| json!({"area": "FUN", "title": format!("Behavior {batch}-{n}")})).collect();
        let result = fix.checkpoint(json!({"addRequirements": requirements, "summary": format!("batch {batch}")})).unwrap();
        for item in result["allocated"]["requirements"].as_array().unwrap() { assert!(ids.insert(item["id"].as_str().unwrap().to_string())); }
    }
    let job = fix.job();
    assert_eq!((job.counts.requirements, job.counts.documents, job.next_requirement), (600, 600, 600));
    assert!(job.updated_at >= 1_000 + 6 * DAY, "the injected clock advanced six days with no deadline");

    fix.restart();
    let recovered = fix.job();
    assert_eq!(recovered.state, JobState::Interrupted, "restart never resumes model work by itself");
    assert_eq!(recovered.error.as_ref().unwrap().code, "app_closed");
    assert_eq!(recovered.counts.requirements, 600);
    assert!(fix.call("srs_checkpoint", json!({"expectedRevision": recovered.revision, "updates": {}})).is_err(), "an interrupted job accepts no tool call");

    fix.resume();
    let result = fix.call("srs_checkpoint", json!({"expectedRevision": fix.job().revision, "idempotencyKey": "next", "updates": {"addRequirements": [{"area": "QOS", "title": "One more"}]}})).unwrap();
    assert_eq!(result["allocated"]["requirements"][0]["id"], "REQ-QOS-601");
    let replay = fix.call("srs_checkpoint", json!({"expectedRevision": 1, "idempotencyKey": "next", "updates": {"addRequirements": [{"area": "QOS", "title": "One more"}]}})).unwrap();
    assert_eq!(replay, result, "a replayed checkpoint is answered, never allocated twice");
    assert_eq!(fix.job().counts.requirements, 601);

    let page = fix.call("srs_inspect", json!({"section": "documents", "offset": 400, "limit": 200})).unwrap();
    assert_eq!((page["total"].as_u64(), page["items"].as_array().unwrap().len()), (Some(601), 200));
    assert!(fix.call("srs_inspect", json!({"section": "documents", "limit": 201})).is_err());
    assert!(fix.call("srs_inspect", json!({"section": "documents", "surprise": true})).is_err(), "unknown fields are rejected");
}

#[test]
fn stale_producers_attempts_stages_and_paused_jobs_cannot_use_tools() {
    let mut fix = Fix::new("running", "specify", "claimed");
    // Before the native admission marker is persisted no analysis tool is authorized.
    assert!(fix.call("srs_inspect", json!({"section": "request"})).unwrap_err().starts_with("stale_stage"));
    let admit = json!({"jobId": fix.job_id, "attemptId": "attempt", "stageId": "stage", "nonce": "wrong"});
    assert!(fix.service.handle_bridge(&fix.producer, "srs_stage_admitted", admit).is_err());
    let pre = json!({"jobId": fix.job_id, "attemptId": "attempt", "stageId": "stage", "nonce": "nonce", "preAdmission": true});
    let wrong_pre = json!({"jobId": fix.job_id, "attemptId": "attempt", "stageId": "stage", "nonce": "wrong", "preAdmission": true});
    assert!(fix.service.handle_bridge(&fix.producer, "srs_stage_validate", wrong_pre).is_err(), "a different nonce has no claim to validate");
    assert_eq!(fix.service.handle_bridge(&fix.producer, "srs_stage_validate", pre.clone()).unwrap()["authorized"], true, "an unadmitted claim is valid only before admission");
    let admit = json!({"jobId": fix.job_id, "attemptId": "attempt", "stageId": "stage", "nonce": "nonce"});
    assert_eq!(fix.service.handle_bridge(&fix.producer, "srs_stage_admitted", admit.clone()).unwrap()["persisted"], true);
    assert_eq!(fix.service.handle_bridge(&fix.producer, "srs_stage_admitted", admit.clone()).unwrap()["duplicate"], true);
    assert_eq!(fix.service.handle_bridge(&fix.producer, "srs_stage_validate", admit.clone()).unwrap()["authorized"], true);
    assert!(fix.call("srs_inspect", json!({"section": "request"})).is_ok());

    let mut other = fix.producer.clone();
    other.session_id = "replaced".into();
    let payload = json!({"action": "srs_inspect", "jobId": fix.job_id, "attemptId": "attempt", "stageId": "stage", "section": "request"});
    assert!(fix.service.handle_tool(&other, &payload).unwrap_err().starts_with("stale_attempt"));
    let mut stale = payload.clone();
    stale["attemptId"] = json!("old");
    assert!(fix.service.handle_tool(&fix.producer, &stale).unwrap_err().starts_with("stale_attempt"));
    stale["attemptId"] = json!("attempt");
    stale["stageId"] = json!("older");
    assert!(fix.service.handle_tool(&fix.producer, &stale).unwrap_err().starts_with("stale_stage"));

    let yielded = json!({"jobId": fix.job_id, "attemptId": "attempt", "stageId": "stage", "reason": "unrelated_prompt"});
    assert_eq!(fix.service.handle_bridge(&fix.producer, "srs_stage_yielded", yielded).unwrap()["paused"], true);
    assert_eq!(fix.job().state, JobState::Paused);
    assert!(fix.service.handle_bridge(&fix.producer, "srs_stage_validate", admit).is_err(), "a paused job authorizes no provider call");
    assert!(fix.call("srs_inspect", json!({"section": "request"})).is_err());
    fix.restart();
    assert_eq!(fix.job().state, JobState::Paused, "a deliberate pause survives restart unchanged");
}

fn staged_text(requirement: &str) -> String {
    format!("# {requirement} Book an appointment\n\n## Statement\nThe system shall let a visitor book an appointment.\n\n## Rationale\nVisitors need a confirmed time.\n\n## Acceptance Criteria\nA booked appointment is shown in the visitor's list.\n\n## Verification Method\nTest: book, reload, compare the list.\n\n## More Information\nSee the [overview](../SRS.md).\n")
}

fn stage_and_commit(fix: &Fix, document: &Value, text: &str, work: &str) -> Result<Value> {
    let bytes = text.as_bytes();
    let (first, second) = bytes.split_at(60);
    let first = std::str::from_utf8(first).unwrap();
    let second = std::str::from_utf8(second).unwrap();
    let (document_id, revision_id) = (document["documentId"].as_str().unwrap(), document["revisionId"].as_str().unwrap());
    let chunk = |offset: u64, part: &str, prefix: String| fix.call("srs_write_document_chunk", json!({"documentId": document_id, "revisionId": revision_id, "offset": offset, "text": part, "expectedPrefixHash": prefix}));
    chunk(0, first, store::hash(b"")).unwrap();
    chunk(0, first, store::hash(b"")).unwrap();
    assert!(chunk(5, second, store::hash(first.as_bytes())).is_err(), "a chunk at the wrong offset is refused");
    chunk(60, second, store::hash(first.as_bytes())).unwrap();
    fix.call("srs_commit_document", json!({"documentId": document_id, "revisionId": revision_id, "expectedHash": store::hash(bytes),
        "metadata": {"type": "requirement", "requirementId": "REQ-FUN-001", "specificity": "portable", "reviewedWorkIds": [work]}}))
}

#[test]
fn documents_publish_idempotently_and_a_human_edit_is_a_conflict_never_an_overwrite() {
    let fix = Fix::new("running", "specify", "active");
    let created = fix.checkpoint(json!({
        "addWork": [{"phase": "specify", "kind": "capability", "rationale": "Appointment booking"}],
        "addDocuments": [{"type": "master", "title": "Software Requirements Specification"}],
        "addRequirements": [{"area": "FUN", "title": "Book an appointment"}]
    })).unwrap();
    let work = created["allocated"]["work"][0]["id"].as_str().unwrap().to_string();
    let doc = fix.service.store.get::<Document>(&fix.job_id, "documents", created["allocated"]["requirements"][0]["documentId"].as_str().unwrap()).unwrap();
    assert_eq!(doc.path, "Requirements/REQ-FUN-001.md");
    let document = json!({"documentId": doc.document_id, "revisionId": doc.revision_id});
    let text = staged_text("REQ-FUN-001");
    let committed = stage_and_commit(&fix, &document, &text, &work).unwrap();
    assert_eq!(committed["hash"], store::hash(text.as_bytes()));
    let vault = fix.knowledge.vault_root(&fix.vault_id).unwrap();
    let note = format!("SRS/{}/Requirements/REQ-FUN-001.md", fix.job_id);
    assert_eq!(vault.read_markdown(&note).unwrap(), text.as_bytes());
    assert!(fix.job().document_revision >= 1 && fix.job().completion.is_none());

    // Replaying the same commit (lost acknowledgement) neither republishes nor changes the revision chain.
    let replay = fix.call("srs_commit_document", json!({"documentId": doc.document_id, "revisionId": doc.revision_id, "expectedHash": store::hash(text.as_bytes()),
        "metadata": {"type": "requirement", "requirementId": "REQ-FUN-001", "specificity": "portable", "reviewedWorkIds": [work]}})).unwrap();
    assert_eq!(replay["replayed"], true);

    // A public note may not leak a private evidence path.
    let next = fix.service.store.get::<Document>(&fix.job_id, "documents", &doc.document_id).unwrap();
    assert_ne!(next.revision_id, doc.revision_id, "the next editable revision rotates after a commit");

    // A human edits the note in the vault; the next commit must stop instead of overwriting it.
    let on_disk = vault.root.join("SRS").join(&fix.job_id).join("Requirements").join("REQ-FUN-001.md");
    std::fs::write(&on_disk, "Human rewrite\n").unwrap();
    let revised = staged_text("REQ-FUN-001").replace("book an appointment", "book or cancel an appointment");
    let document = json!({"documentId": next.document_id, "revisionId": next.revision_id});
    let error = stage_and_commit(&fix, &document, &revised, &work).unwrap_err();
    assert!(error.starts_with("vault_conflict"), "{error}");
    assert_eq!(std::fs::read_to_string(&on_disk).unwrap(), "Human rewrite\n");
    assert_eq!(fix.job().state, JobState::Paused);
    assert_eq!(fix.job().error.unwrap().code, "vault_conflict");
}

#[test]
fn survey_cannot_advance_until_every_admitted_byte_has_a_server_read_receipt() {
    let fix = Fix::new("running", "survey", "active");
    let dir = fix.service.store.dir(&fix.job_id).unwrap();
    let vault = fix.knowledge.vault_root(&fix.vault_id).unwrap();
    let roots = [inventory::InventoryRoot { path: fix.repo.clone(), kind: "repository".into(), rationale: "fixture".into() }];
    inventory::inventory(&dir, &roots, &[], &[fix.root.join("config"), vault.root.clone()], &|| false).unwrap();
    let page = inventory::page(&dir, 0, 200).unwrap();
    let file = page.items.iter().find(|i| i.path == "src/a.txt").expect("the file is inventoried").clone();
    assert_eq!(file.kind, "text");

    let finish = |fix: &Fix| fix.call("srs_finish_stage", json!({"expectedRevision": fix.job().revision, "review": {"phase": "survey", "summary": "surveyed"}}));
    let first = finish(&fix).unwrap();
    assert_eq!(first["advanced"], false, "unread evidence cannot be skipped by prose");
    assert_eq!(fix.job().phase, Phase::Survey);

    // The unmodified fixture stage is committed, so a fresh stage is needed for the next review.
    let mut job = fix.job();
    job.stage.as_mut().unwrap().state = "active".into();
    fix.service.save(&mut job, vec![], "test: new stage").unwrap();
    let read = fix.call("srs_read_evidence", json!({"evidenceId": file.id, "offset": 0})).unwrap();
    assert_eq!(read["kind"], "text");
    assert!(read["receiptId"].is_string(), "the backend, not the model, records the receipt");
    assert!(fix.call("srs_read_evidence", json!({"evidenceId": "../escape"})).is_err(), "model ids cannot select paths");
    let second = finish(&fix).unwrap();
    assert_eq!((second["advanced"].clone(), second["nextPhase"].clone()), (json!(true), json!("specify")));
    assert_eq!(fix.job().phase, Phase::Specify);
}

#[test]
fn final_review_prose_cannot_complete_an_incomplete_ledger() {
    let fix = Fix::new("reviewing", "verify", "active");
    let dir = fix.service.store.dir(&fix.job_id).unwrap();
    let vault = fix.knowledge.vault_root(&fix.vault_id).unwrap();
    let roots = [inventory::InventoryRoot { path: fix.repo.clone(), kind: "repository".into(), rationale: "fixture".into() }];
    inventory::inventory(&dir, &roots, &[], &[fix.root.join("config"), vault.root.clone()], &|| false).unwrap();
    let result = fix.call("srs_finish_stage", json!({"expectedRevision": fix.job().revision, "review": {"phase": "verify", "finalDeclaration": "noKnownGap", "summary": "Everything is done."}})).unwrap();
    assert_eq!(result["completed"], false);
    assert!(!result["reasons"].as_array().unwrap().is_empty());
    let job = fix.job();
    assert!(job.completion.is_none() && job.state != JobState::Completed);
    assert!(fix.call("srs_finish_stage", json!({"expectedRevision": 1, "review": {"phase": "verify", "summary": "x"}})).is_err() || fix.job().stage.as_ref().unwrap().state == "committed");
}

#[test]
fn scope_preparation_records_roots_app_and_human_exclusions_and_is_single_use() {
    let fix = Fix::new("paused", "survey", "interrupted");
    std::fs::create_dir_all(fix.repo.join("vendor")).unwrap();
    let prepared = fix.service.prepare_srs_scope(fix.repo.to_string_lossy().as_ref(), &fix.vault_id, vec![], vec!["vendor".into()]).unwrap();
    assert_eq!(prepared["roots"][0]["kind"], "repository");
    let exclusions = prepared["exclusions"].as_array().unwrap();
    assert!(exclusions.iter().any(|e| e["source"] == "human" && e["path"].as_str().unwrap().ends_with("vendor")));
    assert!(exclusions.iter().any(|e| e["source"] == "app"));
    assert!(prepared["token"].is_string() && prepared["warnings"].as_array().unwrap().len() >= 2);
    // A whole root cannot be excluded and an evidence folder may not overlap the repository.
    assert!(fix.service.prepare_srs_scope(fix.repo.to_string_lossy().as_ref(), &fix.vault_id, vec![], vec![fix.repo.to_string_lossy().into_owned()]).is_err());
    assert!(fix.service.prepare_srs_scope(fix.repo.to_string_lossy().as_ref(), &fix.vault_id, vec![fix.repo.join("src").to_string_lossy().into_owned()], vec![]).is_err());
    assert!(fix.service.prepare_srs_scope(fix.repo.to_string_lossy().as_ref(), &store::id(), vec![], vec![]).is_err(), "only a bound vault may be chosen");
}

// ---------------------------------------------------------------------------------------------------------------
// Positive end-to-end completion through the real tool API (what the model is allowed to call), one stage per phase.
// ---------------------------------------------------------------------------------------------------------------

fn ok(fix: &Fix, action: &str, extra: Value) -> Value {
    fix.call(action, extra).unwrap_or_else(|e| panic!("{action} failed: {e}"))
}

/// Revisions the model is given by `srs_inspect checkpoint`; nothing else is needed to satisfy the fences.
fn checkpoint_view(fix: &Fix) -> Value {
    ok(fix, "srs_inspect", json!({"section": "checkpoint"}))["items"][0].clone()
}

fn finish(fix: &Fix, phase: &str, declaration: Option<&str>) -> Value {
    let view = checkpoint_view(fix);
    let mut review = json!({"phase": phase, "documentRevision": view["documentRevision"], "workRevision": view["workRevision"], "summary": format!("{phase} reviewed against the ledger.")});
    if let Some(declaration) = declaration { review["finalDeclaration"] = json!(declaration); }
    ok(fix, "srs_finish_stage", json!({"expectedRevision": view["revision"], "review": review}))
}

fn encode(path: &str) -> String { path.replace(' ', "%20") }

/// A public relative link between two run-relative note paths.
fn link(label: &str, from: &str, to: &str) -> String {
    format!("[{label}]({}{})", "../".repeat(from.matches('/').count()), encode(to))
}

fn publish(fix: &Fix, doc: &Value, text: &str, meta: Value) -> Value {
    let (document_id, revision_id) = (doc["documentId"].as_str().unwrap(), doc["revisionId"].as_str().unwrap());
    ok(fix, "srs_write_document_chunk", json!({"documentId": document_id, "revisionId": revision_id, "offset": 0, "text": text, "expectedPrefixHash": store::hash(b"")}));
    ok(fix, "srs_commit_document", json!({"documentId": document_id, "revisionId": revision_id, "expectedHash": store::hash(text.as_bytes()), "metadata": meta}))
}

fn master_text(requirements: &[(String, String)]) -> String {
    let mut out = String::from("# Software Requirements Specification\n\n");
    let list: String = requirements.iter().map(|(id, path)| format!("- {}\n", link(id, "SRS.md", path))).collect();
    for title in documents::master_sections() {
        let numbers = title.split_whitespace().next().unwrap_or("");
        let level = if numbers.starts_with(|c: char| c.is_ascii_digit()) { numbers.trim_end_matches('.').split('.').count() + 1 } else { 2 };
        out.push_str(&format!("{} {title}\n", "#".repeat(level)));
        let body = match title {
            "Table of Contents" | "2.2 Product Functions" | "3.2 Functional" => list.clone(),
            "Revision History" => "Version one records the behaviour observed in the admitted evidence.\n".into(),
            "1. Introduction" => "This specification describes an appointment booking service for the recorded evidence scope.\n".into(),
            "1.1 Document Purpose" => "It gives a modernization team the externally observable behaviour and acceptance criteria.\n".into(),
            "1.2 Product Scope" => "Visitors book appointments, cancel them and receive refunds under the recorded policy.\n".into(),
            "3. Requirements" => "Requirements are kept in the linked requirement notes, one note per stable identifier.\n".into(),
            "4. Verification" => "Each requirement names its verification method and appears in the traceability matrix.\n".into(),
            _ => "N/A: The admitted evidence does not describe this topic.\n".into(),
        };
        out.push_str(&format!("\n{body}\n"));
    }
    out
}

fn requirement_text(id: &str, title: &str, statement: &str, criteria: &str, back: &str) -> String {
    format!("# {id} {title}\n\n## Statement\n{statement}\n\n## Rationale\nVisitors and staff need predictable, explainable appointment outcomes.\n\n## Acceptance Criteria\n{criteria}\n\n## Verification Method\nTest: exercise the normal case, the unavailable case and the missing input case, then compare the outcomes.\n\n## More Information\nSee the {back}.\n")
}

#[test]
fn a_complete_srs_is_reached_only_through_the_real_tool_api_and_publishes_clean_notes() {
    let fix = Fix::new("inventory", "survey", "active");
    std::fs::write(fix.repo.join("README.md"), "# Appointments\nA small booking service. See appointment-behaviour.md.\n").unwrap();
    std::fs::write(fix.repo.join("appointment-behaviour.md"), "# Behaviour\nA visitor books an available time and receives a confirmation. A taken time is refused.\nA visitor may cancel a booking. A cancellation made early is refunded in full; a late cancellation is refunded in part. The exact refund rule is not written down.\n").unwrap();
    std::fs::write(fix.repo.join("src").join("booking.txt"), "function computeRefundAmount(appointment) { return appointment.price; }\n").unwrap();
    fix.service.build_inventory(&fix.job_id).unwrap();
    assert_eq!(fix.job().state, JobState::Running, "no blocked evidence in this fixture");

    // ---- survey ------------------------------------------------------------------------------------------------
    let inventory = ok(&fix, "srs_inspect", json!({"section": "inventory", "limit": 200}));
    let items = inventory["items"].as_array().unwrap().clone();
    let id_of = |path: &str| items.iter().find(|i| i["path"] == path).unwrap_or_else(|| panic!("{path} is inventoried"))["id"].as_str().unwrap().to_string();
    let (behaviour, booking) = (id_of("appointment-behaviour.md"), id_of("src/booking.txt"));
    assert!(ok(&fix, "srs_inspect", json!({"section": "template"}))["items"].as_array().unwrap().len() >= 4);
    for item in items.iter().filter(|i| i["kind"] == "text" && i["state"] == "ready") {
        let mut offset = 0u64;
        loop {
            let read = ok(&fix, "srs_read_evidence", json!({"evidenceId": item["id"], "offset": offset}));
            assert!(read["receiptId"].is_string());
            match read["nextOffset"].as_u64() { Some(next) => offset = next, None => break }
        }
    }
    let work = ok(&fix, "srs_inspect", json!({"section": "work", "limit": 200}))["items"].as_array().unwrap().clone();
    let reviewed: Vec<Value> = work.iter().filter(|w| w["kind"] == "sourceArea").map(|w| json!({"id": w["id"], "status": "reviewed", "rationale": "All evidence in this area was read."})).collect();
    assert!(!reviewed.is_empty());
    let survey = fix.checkpoint(json!({
        "addWork": [
            {"phase": "specify", "kind": "capability", "rationale": "Booking an appointment", "evidenceIds": [behaviour]},
            {"phase": "specify", "kind": "capability", "rationale": "Cancelling and refunds", "evidenceIds": [behaviour, booking]}
        ],
        "updateWork": reviewed,
        "addQuestions": [{"prompt": "When a customer cancels late, how much is refunded?", "options": [{"label": "Everything"}, {"label": "Nothing"}, {"label": "Depends on timing"}], "required": true}],
        "summary": "Surveyed every admitted file; the refund rule needs a stakeholder."
    })).unwrap();
    let (book_work, cancel_work) = (survey["allocated"]["work"][0]["id"].as_str().unwrap().to_string(), survey["allocated"]["work"][1]["id"].as_str().unwrap().to_string());
    let question = survey["allocated"]["questions"][0]["questionId"].as_str().unwrap().to_string();

    // The question blocks the stage until a real human receipt exists.
    let blocked = finish(&fix, "survey", None);
    assert_eq!(blocked["advanced"], false);
    assert_eq!(fix.job().phase, Phase::Survey);
    fix.next_stage(2);
    let invocation_id = store::id();
    let native = fix.service.handle_bridge(&fix.producer, "srs_ask_map", json!({"invocationId": invocation_id, "questions": [{"id": question, "question": "When a customer cancels late, how much is refunded?", "options": ["Everything", "Nothing", "Depends on timing"], "multi": false}]})).unwrap();
    assert_eq!(native["mapped"], true);
    let receipt = fix.service.handle_bridge(&fix.producer, "srs_ask_receipt", json!({"invocationId": invocation_id, "provenance": "human", "results": [{"id": question, "selectedOptions": ["Depends on timing"]}]})).unwrap();
    assert_eq!(receipt["satisfiedIds"][0], question.as_str());
    let advanced = finish(&fix, "survey", None);
    assert_eq!((advanced["advanced"].clone(), advanced["nextPhase"].clone()), (json!(true), json!("specify")), "{advanced}");

    // ---- specify -----------------------------------------------------------------------------------------------
    fix.next_stage(3);
    let allocated = fix.checkpoint(json!({
        "addRequirements": [
            {"area": "FUN", "title": "Book an appointment", "workIds": [book_work], "evidenceIds": [behaviour]},
            {"area": "FUN", "title": "Cancel an appointment", "workIds": [cancel_work], "evidenceIds": [behaviour]},
            {"area": "FUN", "title": "Refund a late cancellation", "workIds": [cancel_work], "evidenceIds": [behaviour, booking]}
        ],
        "addDocuments": [
            {"type": "master", "title": "Software Requirements Specification"}, {"type": "glossary", "title": "Glossary"},
            {"type": "questions", "title": "Open questions"}, {"type": "uiObservations", "title": "Current UI observations"},
            {"type": "structureObservations", "title": "Current structure observations"}, {"type": "traceability", "title": "Traceability"},
            {"type": "coverage", "title": "Coverage"}
        ],
        "summary": "Requirements and documents allocated."
    })).unwrap()["allocated"].clone();
    let requirements: Vec<(String, String, Value)> = allocated["requirements"].as_array().unwrap().iter().map(|r| {
        let record = ok(&fix, "srs_inspect", json!({"section": "documents", "id": r["documentId"]}))["items"][0]["record"].clone();
        (r["id"].as_str().unwrap().to_string(), record["path"].as_str().unwrap().to_string(), json!({"documentId": record["documentId"], "revisionId": record["revisionId"]}))
    }).collect();
    let doc = |index: usize| allocated["documents"][index].clone();
    let path = |index: usize| allocated["documents"][index]["path"].as_str().unwrap().to_string();
    assert_eq!(requirements[0].0, "REQ-FUN-001");
    assert_eq!(requirements[2].0, "REQ-FUN-003");
    // Reviewed work is finished before the notes that cite it, so the notes' reviews are not invalidated afterwards.
    fix.checkpoint(json!({"updateWork": [
        {"id": book_work, "status": "reviewed", "requirementIds": [requirements[0].0], "evidenceIds": [behaviour]},
        {"id": cancel_work, "status": "reviewed", "requirementIds": [requirements[1].0, requirements[2].0], "evidenceIds": [behaviour]}
    ]})).unwrap();
    let cite = json!([{"evidenceId": behaviour}, {"evidenceId": booking}]);
    let master_path = path(0);
    let links: Vec<(String, String)> = requirements.iter().map(|(id, p, _)| (id.clone(), p.clone())).collect();
    let texts = [
        ("Book an appointment", "The system shall let a visitor reserve an available appointment time and shall confirm the booking.", "A visitor who books an available time receives a confirmation. A visitor who selects a time that is already taken is told it is unavailable and no booking is created. A request without a time is rejected with an explanation."),
        ("Cancel an appointment", "The system shall let a visitor cancel a booking that has not yet taken place.", "A cancelled booking no longer appears in the visitor's list and its time becomes available again. Cancelling a booking twice reports that it is already cancelled. Cancelling a past booking is refused."),
        ("Refund a late cancellation", "The system shall refund a cancellation according to how early it is made, and the exact refund rule is an open question for the stakeholder.", "An early cancellation is refunded in full. A late cancellation is refunded in part according to the stakeholder decision. A cancellation with no payment produces no refund and no error."),
    ];
    for (index, (title, statement, criteria)) in texts.iter().enumerate() {
        let (id, p, doc_ref) = &requirements[index];
        let text = requirement_text(id, title, statement, criteria, &format!("{} and the {}", link("specification", p, &master_path), link("glossary", p, &path(1))));
        publish(&fix, doc_ref, &text, json!({"type": "requirement", "requirementId": id, "specificity": "portable", "reviewedWorkIds": [if index == 0 { &book_work } else { &cancel_work }], "citations": cite}));
    }
    publish(&fix, &doc(1), "# Glossary\n\n- Appointment: a reserved time slot for one visitor.\n- Refund: money returned after a cancellation.\n", json!({"type": "glossary", "specificity": "portable", "reviewedWorkIds": [book_work]}));
    let open_questions = format!("# Open questions\n\nRefund rule: the stakeholder answered that the refund depends on timing. It is captured in {}.\n", link(&requirements[2].0, &path(2), &requirements[2].1));
    publish(&fix, &doc(2), &open_questions, json!({"type": "questions", "specificity": "portable", "reviewedWorkIds": [cancel_work]}));
    publish(&fix, &doc(3), "# Current UI observations\n\n**UI-only requirement**\n\nCurrent UI only — redesignable.\n\nThe legacy screens show a list of free time slots and a cancel button beside each booking. Keyboard access to every action is essential behaviour; the widgets and layout are incidental.\n", json!({"type": "uiObservations", "specificity": "uiSpecific", "reviewedWorkIds": [book_work]}));
    publish(&fix, &doc(4), "# Current structure observations\n\n**Implementation-only requirement**\n\nImplementation-specific — not a modernization requirement.\n\nThe legacy system separates booking rules from refund rules; a new design may organise this differently.\n", json!({"type": "structureObservations", "specificity": "implementationSpecific", "reviewedWorkIds": [cancel_work]}));
    let trace_rows: String = requirements.iter().map(|(id, p, _)| format!("| {} | Test |\n", link(id, &path(5), p))).collect();
    let related: Vec<Value> = requirements.iter().map(|(_, _, d)| d["documentId"].clone()).collect();
    publish(&fix, &doc(5), &format!("# Traceability\n\n| Requirement | Verification |\n| --- | --- |\n{trace_rows}"), json!({"type": "traceability", "specificity": "portable", "reviewedWorkIds": [book_work, cancel_work], "relatedDocumentIds": related}));
    publish(&fix, &doc(6), &format!("# Coverage\n\nEvery admitted evidence area was read and each requirement is listed in the {}.\n", link("specification", &path(6), &master_path)), json!({"type": "coverage", "specificity": "portable", "reviewedWorkIds": [book_work, cancel_work]}));
    publish(&fix, &doc(0), &master_text(&links), json!({"type": "master", "specificity": "portable", "reviewedWorkIds": [book_work, cancel_work], "citations": cite}));
    let specify = finish(&fix, "specify", None);
    assert_eq!((specify["advanced"].clone(), specify["nextPhase"].clone()), (json!(true), json!("gapReview")), "{specify}");

    // ---- gap review ---------------------------------------------------------------------------------------------
    fix.next_stage(4);
    let gaps = finish(&fix, "gapReview", Some("noKnownGap"));
    assert_eq!((gaps["advanced"].clone(), gaps["nextPhase"].clone()), (json!(true), json!("verify")), "{gaps}");

    // ---- verify -------------------------------------------------------------------------------------------------
    fix.next_stage(5);
    let work = ok(&fix, "srs_inspect", json!({"section": "work", "limit": 200}))["items"].as_array().unwrap().clone();
    let sections: Vec<Value> = work.iter().filter(|w| w["kind"] == "templateSection").map(|w| {
        let applicable = ["Introduction", "Product Overview", "Functional", "Verification"].contains(&w["rationale"].as_str().unwrap());
        if applicable { json!({"id": w["id"], "status": "reviewed"}) } else { json!({"id": w["id"], "status": "notApplicable", "evidenceIds": [behaviour]}) }
    }).collect();
    assert_eq!(sections.len(), 10);
    fix.checkpoint(json!({"updateWork": sections, "summary": "Every template section is worked or evidence-backed not applicable."})).unwrap();
    // A global requirement count cannot cover an omitted capability or scenario. Completion must name both
    // omissions, and a real human exclusion must be consumable rather than an unknown relation kind.
    let additions = fix.checkpoint(json!({"addWork":[
        {"phase":"verify","kind":"capability","status":"reviewed","rationale":"Availability checking within booking","evidenceIds":[behaviour]},
        {"phase":"verify","kind":"scenario","status":"reviewed","rationale":"An occupied appointment time is refused","evidenceIds":[behaviour]}],
        "addQuestions":[{"prompt":"Should the undocumented legacy hardware be in scope?","options":[{"label":"In scope"},{"label":"Out of scope"}],"required":true}]})).unwrap()["allocated"].clone();
    let incomplete = finish(&fix,"verify",Some("noKnownGap"));
    assert_eq!(incomplete["completed"],false,"reviewed obligations cannot be silently omitted");
    for kind in ["capability","scenario"] {
        assert!(incomplete["reasons"].as_array().unwrap().iter().any(|r|r.as_str().unwrap().contains(kind) && r.as_str().unwrap().contains("stable requirement")), "{incomplete}");
    }
    fix.service.pause_srs(&fix.job_id).unwrap();
    let excluded = additions["questions"][0]["questionId"].as_str().unwrap();
    fix.service.dispose_srs_item(&fix.job_id,fix.job().revision,excluded,"excludeFromScope","Stakeholder explicitly excludes undocumented legacy hardware from this evidence scope",None).unwrap();
    fix.next_stage(6);
    let coverage:Vec<Value> = additions["work"].as_array().unwrap().iter().map(|w|json!({"id":w["id"],"status":"reviewed","requirementIds":[requirements[0].0]})).collect();
    fix.checkpoint(json!({"updateWork":coverage})).unwrap();
    let done = finish(&fix, "verify", Some("noKnownGap"));
    assert_eq!(done["completed"], true, "{done}");

    let job = fix.job();
    assert_eq!(job.state, JobState::Completed);
    let completion = job.completion.expect("a completion record");
    assert_eq!(completion.label, "SRS complete for the recorded evidence scope");
    assert_eq!(completion.manifest_hash.len(), 64);
    assert_eq!(completion.scope_hash, job.scope_hash);
    let manifest: Value = store::read(&fix.service.store.dir(&fix.job_id).unwrap().join("manifest.json")).unwrap();
    assert_eq!(manifest["manifestHash"], completion.manifest_hash.as_str());
    assert_eq!(manifest["manifest"]["documents"], 10);
    let opened = fix.service.open_srs_documents(&fix.job_id).unwrap();
    assert_eq!((opened["completed"].clone(), opened["path"].clone()), (json!(true), json!(format!("SRS/{}/SRS.md", fix.job_id))));
    // After completion only the committed stage's validation remains; analysis tools are no longer authorized.
    assert!(fix.call("srs_inspect", json!({"section": "request"})).is_err());

    // Published notes read back from the vault and carry no private evidence terminology.
    let vault = fix.knowledge.vault_root(&fix.vault_id).unwrap();
    let mut published = 0;
    fix.service.store.for_each(&fix.job_id, "documents", |value| {
        let doc: Document = serde_json::from_value(value).unwrap();
        let bytes = vault.read_markdown(&format!("SRS/{}/{}", fix.job_id, doc.path)).unwrap();
        assert_eq!(store::hash(&bytes), doc.hash.clone().unwrap(), "{}", doc.path);
        let text = String::from_utf8(bytes).unwrap();
        for forbidden in ["booking.txt", "computeRefundAmount", "src/", "README", "appointment-behaviour", ".rs"] { assert!(!text.contains(forbidden), "{} leaks {forbidden}", doc.path); }
        published += 1;
        Ok(())
    }).unwrap();
    assert_eq!(published, 10);
    // The audit trail stays private: citations are stored with the job, never in a vault note.
    assert!(fix.service.store.page(&fix.job_id, "citations", 0, 200).unwrap()["total"].as_u64().unwrap() >= 4);
}


#[test]
fn interrupted_inventory_reconciles_existing_work_links_and_all_counters_before_tools_resume() {
    let mut fix = Fix::new("inventory", "survey", "active");
    std::fs::write(fix.repo.join("unreadable.bin"), [0u8, 1, 2, 3]).unwrap();
    let mut initial = fix.job();
    initial.inventory_complete = false;
    fix.service.save(&mut initial, vec![], "Inventory begun").unwrap();
    let dir = fix.service.store.dir(&fix.job_id).unwrap();
    let (roots, exclusions, protected) = fix.service.inventory_args(&initial);
    inventory::inventory(&dir, &roots, &exclusions, &protected, &|| false).unwrap();
    let mut seeded = Vec::new();
    inventory::for_each(&dir, &mut |item| {
        if item.kind == "area" || item.state == "blocked" {
            let work = Work { id: format!("source-{}", item.id), parent_id: None, phase: Phase::Survey, kind: "sourceArea".into(),
                status: if item.state == "blocked" { "blocked".into() } else { "pending".into() }, rationale: item.path.clone(), revision: 0 };
            // Simulate a process crash after writing work but before its relation and job counters.
            store::atomic(&fix.service.store.path(&fix.job_id, "work", &work.id).unwrap(), &work).unwrap();
            seeded.push((work.id, item.id.clone()));
        }
        Ok(())
    }).unwrap();
    assert!(seeded.len() >= 2, "at least one area and the blocked attachment");
    fix.restart();
    assert_eq!(fix.job().state, JobState::Interrupted);
    let mut resumed = fix.job();
    resumed.state = JobState::Inventory;
    fix.service.save(&mut resumed, vec![], "Human resumes incomplete inventory").unwrap();
    fix.service.build_inventory(&fix.job_id).unwrap();
    let reconciled = fix.job();
    assert_eq!(reconciled.state, JobState::WaitingForHuman);
    assert_eq!(reconciled.counts.blocked, 1, "existing blocked work is counted on replay");
    let mut all = 0u64;
    let mut open = 0u64;
    fix.service.store.for_each(&fix.job_id, "work", |value| {
        let w: Work = serde_json::from_value(value).unwrap();
        all += 1;
        if !matches!(w.status.as_str(), "reviewed" | "notApplicable" | "excluded") { open += 1; }
        Ok(())
    }).unwrap();
    assert_eq!((reconciled.counts.work, reconciled.counts.work_open), (all, open));
    for (work, evidence) in &seeded {
        assert!(fix.service.store.relation_exists(&fix.job_id, work, "evidence", evidence).unwrap(), "missing evidence relation repaired");
    }
    let mut evidence_id = String::new();
    inventory::for_each(&dir, &mut |item| { if item.state == "blocked" { evidence_id = item.id.clone(); } Ok(()) }).unwrap();
    fix.service.dispose_srs_item(&fix.job_id, reconciled.revision, &evidence_id, "excludeFromScope", "Unreadable attachment excluded by the stakeholder", None).unwrap();
    fix.next_stage(2);
    let source: Vec<Value> = ok(&fix, "srs_inspect", json!({"section":"work","limit":200}))["items"].as_array().unwrap()
        .iter().filter(|w| w["kind"] == "sourceArea" && w["status"] == "pending")
        .map(|w| json!({"id":w["id"],"status":"reviewed"})).collect();
    fix.checkpoint(json!({"updateWork":source})).expect("reviewing reconstructed pending work never underflows counters");
    assert_eq!(fix.job().counts.blocked, 0);
    assert_eq!(fix.job().counts.work_open, 10, "only template sections remain open");
}

#[test]
fn resumed_architect_reads_saved_human_answer_through_both_allowed_question_inspections() {
    let mut fix = Fix::new("running", "survey", "active");
    let allocation = fix.checkpoint(json!({"addQuestions":[{"prompt":"Which format shall customers receive?","options":[{"label":"JSON"},{"label":"XML"}]}]})).unwrap();
    let qid = allocation["allocated"]["questions"][0]["questionId"].as_str().unwrap().to_string();
    fix.service.pause_srs(&fix.job_id).unwrap();
    fix.service.dispose_srs_item(&fix.job_id, fix.job().revision, &qid, "provideHumanAnswer", "Stakeholder confirmed export format",
        Some(Answer { selected_options:vec!["JSON".into()], custom_input:Some("Use orders.json".into()), note:Some("Keep existing customer integrations".into()) })).unwrap();
    let receipt = fix.service.store.get::<Question>(&fix.job_id,"questions",&qid).unwrap().answer_receipt_id.unwrap();
    fix.restart();
    fix.producer.session_id = "replacement-conversation".into();
    fix.producer.launch_id = "new-launch".into();
    let mut resumed = fix.job();
    resumed.producer = Some(fix.producer.clone());
    fix.service.save(&mut resumed, vec![], "Explicit continuation from checkpoints in replacement conversation").unwrap();
    fix.next_stage(2);
    for exact in [false, true] {
        let args = if exact { json!({"section":"questions","id":qid}) } else { json!({"section":"questions"}) };
        let inspected = ok(&fix,"srs_inspect",args);
        let record = if exact { &inspected["items"][0]["record"] } else { &inspected["items"][0] };
        assert_eq!(record["state"], "answered");
        assert_eq!(record["answer"]["receiptId"], receipt);
        assert_eq!(record["answer"]["selectedOptions"], json!(["JSON"]));
        assert_eq!(record["answer"]["customInput"], "Use orders.json");
        assert_eq!(record["answer"]["note"], "Keep existing customer integrations");
        assert_eq!((record["answer"]["provenance"].clone(),record["answer"]["source"].clone()), (json!("human"),json!("mainWindow")));
    }
    let mut forged: Value = fix.service.store.get(&fix.job_id,"receipts",&receipt).unwrap();
    forged["provenance"] = json!("model");
    store::atomic(&fix.service.store.path(&fix.job_id,"receipts",&receipt).unwrap(),&forged).unwrap();
    assert!(fix.call("srs_inspect",json!({"section":"questions","id":qid})).unwrap_err().contains("authenticated"), "unauthenticated receipt content never reaches model inspection");
}

#[test]
fn native_question_repository_uses_only_exact_current_srs_mapping() {
    let fix = Fix::new("running","survey","active");
    let allocation=fix.checkpoint(json!({"addQuestions":[{"prompt":"Confirm the customer format","options":[{"label":"JSON"}]}]})).unwrap();
    let qid=allocation["allocated"]["questions"][0]["questionId"].as_str().unwrap();
    let invocation=store::id();
    fix.service.handle_bridge(&fix.producer,"srs_ask_map",json!({"invocationId":invocation,"questions":[{"id":qid,"question":"Confirm the customer format","options":["JSON"]}]})).unwrap();
    assert_eq!(fix.service.native_question_repository(&fix.producer,&invocation).unwrap(),Some(fix.repo.display().to_string()));
    assert_eq!(fix.service.native_question_repository(&fix.producer,&store::id()).unwrap(),None,"unrelated native invocation retains its normal repository attribution");
    let mut other=fix.producer.clone();other.session_id="other-session".into();
    assert_eq!(fix.service.native_question_repository(&other,&invocation).unwrap(),None,"stale session mapping does not select an SRS project");
    fix.next_stage(2);
    assert_eq!(fix.service.native_question_repository(&fix.producer,&invocation).unwrap(),None,"mapping is stage-bound");
}

#[test]
fn document_commit_preserves_public_export_filenames_but_rejects_actual_private_paths() {
    let fix=Fix::new("running","specify","active");
    let allocated=fix.checkpoint(json!({"addWork":[{"phase":"specify","kind":"capability","rationale":"Customer exports"}],
        "addRequirements":[{"area":"EI","title":"Export orders.json"}]})).unwrap();
    let work=allocated["allocated"]["work"][0]["id"].as_str().unwrap();
    let id=allocated["allocated"]["requirements"][0]["documentId"].as_str().unwrap();
    let record=ok(&fix,"srs_inspect",json!({"section":"documents","id":id}))["items"][0]["record"].clone();
    let body="# REQ-EI-001 Export orders.json\n\n## Statement\nThe system shall export orders.json, settings.yaml and report.xml to the customer.\n\n## Rationale\nCustomer integrations require the existing names and formats.\n\n## Acceptance Criteria\nEach export uses its documented filename; no internal location appears in its content.\n\n## Verification Method\nTest: request each export and inspect the downloaded name and content.\n\n## More Information\nThese externally visible filenames are product contracts, not private source locations.\n";
    publish(&fix,&record,body,json!({"type":"requirement","requirementId":"REQ-EI-001","specificity":"portable","reviewedWorkIds":[work]}));
    let updated=ok(&fix,"srs_inspect",json!({"section":"documents","id":id}))["items"][0]["record"].clone();
    let leaking=body.replace("These externally visible filenames are product contracts, not private source locations.","The implementation reads src/private/orders.json and calls exportOrders().");
    ok(&fix,"srs_write_document_chunk",json!({"documentId":id,"revisionId":updated["revisionId"],"offset":0,"text":leaking,"expectedPrefixHash":store::hash(b"")}));
    assert!(fix.call("srs_commit_document",json!({"documentId":id,"revisionId":updated["revisionId"],"expectedHash":store::hash(leaking.as_bytes()),
        "metadata":{"type":"requirement","requirementId":"REQ-EI-001","specificity":"portable","reviewedWorkIds":[work]}})).is_err());
}

#[test]
fn tool_document_review_becomes_stale_lazily_and_commit_replay_cannot_refresh_it() {
    let fix=Fix::new("running","specify","active");
    let work=fix.checkpoint(json!({"addWork":[{"phase":"specify","kind":"capability","rationale":"Customer export"}]})).unwrap()["allocated"]["work"][0]["id"].as_str().unwrap().to_string();
    let requirement=fix.checkpoint(json!({"addRequirements":[{"area":"FUN","title":"Customer export","workIds":[work]}]})).unwrap()["allocated"]["requirements"][0].clone();
    fix.checkpoint(json!({"updateWork":[{"id":work,"status":"reviewed"}]})).unwrap();
    let doc_id=requirement["documentId"].as_str().unwrap();
    let read_doc=||ok(&fix,"srs_inspect",json!({"section":"documents","id":doc_id}))["items"][0]["record"].clone();
    let record=read_doc();
    let body="# REQ-FUN-001 Customer export\n\n## Statement\nThe system shall deliver the requested customer export.\n\n## Rationale\nCustomers need their data.\n\n## Acceptance Criteria\nThe requested export is delivered; an unknown request is refused without changing data.\n\n## Verification Method\nTest: request valid and unknown exports and compare the result.\n\n## More Information\nNo additional interface constraints were confirmed.\n";
    let meta=json!({"type":"requirement","requirementId":"REQ-FUN-001","specificity":"portable","reviewedWorkIds":[work]});
    publish(&fix,&record,body,meta.clone());
    assert_eq!(read_doc()["reviewCurrent"],true);
    let before:Document=fix.service.store.get(&fix.job_id,"documents",doc_id).unwrap();
    fix.checkpoint(json!({"updateWork":[{"id":work,"rationale":"Customer export includes the confirmed boundary cases"}]})).unwrap();
    let after:Document=fix.service.store.get(&fix.job_id,"documents",doc_id).unwrap();
    assert_eq!(before.reviewed_revision,after.reviewed_revision,"checkpoint does not rewrite dependent document records");
    assert_eq!(read_doc()["reviewCurrent"],false,"inspection consumes exact work-revision review receipts");
    let replay=ok(&fix,"srs_commit_document",json!({"documentId":doc_id,"revisionId":record["revisionId"],"expectedHash":store::hash(body.as_bytes()),"metadata":meta}));
    assert_eq!(replay["replayed"],true);
    assert_eq!(replay["reviewCurrent"],false,"an old commit acknowledgement cannot approve a new work revision");
    fix.checkpoint(json!({"updateWork":[{"id":work,"status":"reviewed"}]})).unwrap();
    publish(&fix,&read_doc(),body,json!({"type":"requirement","requirementId":"REQ-FUN-001","specificity":"portable","reviewedWorkIds":[work]}));
    assert_eq!(read_doc()["reviewCurrent"],true,"explicitly reviewed new publication records the current work revision");
}
