//! Checkpoints mutate bounded records; references always bind already-durable IDs, never caller aliases.
use super::{documents, inventory, store::{self, Mutation, Store}, types::*, Result};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::{collections::HashSet, path::Path};

const AREAS: &[&str] = &["INT", "PO", "EI", "FUN", "QOS", "COM", "DES", "AI", "VER", "APP"];
const WORK_KINDS: &[&str] = &["sourceArea", "capability", "scenario", "requirementReview", "templateSection"];
const STATUSES: &[&str] = &["pending", "active", "blocked", "reviewed", "notApplicable"];
const FIXED_DOCUMENTS: &[&str] = &["master", "uiObservations", "structureObservations", "traceability", "coverage", "glossary", "questions"];
const TEMPLATE_SECTIONS: &[&str] = &["Introduction", "Product Overview", "External Interfaces", "Functional", "QoS", "Compliance", "Design/Implementation", "AI/ML", "Verification", "Appendixes"];

fn invalid(message: impl AsRef<str>) -> String { format!("invalid_request: {}", message.as_ref()) }
fn text(value: &str, name: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(|c| c == '\0' || (c.is_control() && c != '\n' && c != '\r' && c != '\t')) {
        return Err(invalid(format!("{name} must be nonblank text of at most {max} UTF-8 bytes")));
    }
    Ok(())
}
fn bounded<T>(values: &[T], name: &str) -> Result<()> {
    if values.len() > 200 { return Err(invalid(format!("{name} accepts at most 200 entries per checkpoint"))); }
    Ok(())
}
fn reference(value: &Option<String>) -> Result<()> {
    if let Some(value) = value { text(value, "ref", 160)?; }
    Ok(())
}
fn decode<T: DeserializeOwned>(value: Value) -> Result<T> { serde_json::from_value(value).map_err(|e| format!("storage_error: Invalid ledger record: {e}")) }
fn checked_add(value: &mut u64, name: &str) -> Result<()> { *value = value.checked_add(1).ok_or_else(|| format!("storage_error: {name} counter overflow"))?; Ok(()) }
fn is_open(status: &str) -> bool { !matches!(status, "reviewed" | "notApplicable" | "excluded") }
fn phase_number(phase: Phase) -> u8 { match phase { Phase::Survey => 0, Phase::Specify => 1, Phase::GapReview => 2, Phase::Verify => 3 } }

fn existing<T: DeserializeOwned>(store: &Store, job: &Job, table: &str, id: &str) -> Result<T> {
    store::safe_id(id)?;
    store.get(&job.job_id, table, id).map_err(|e| format!("invalid_request: {table} reference {id} must identify an existing durable record ({e}); use allocated IDs in a subsequent checkpoint"))
}
fn validate_evidence(dir: &Path, ids: &[String]) -> Result<()> {
    bounded(ids, "evidenceIds")?;
    for id in ids { store::safe_id(id)?; inventory::read_item(dir, id)?; }
    Ok(())
}
fn link(store: &Store, job: &Job, writes: &mut Vec<Mutation>, owner: &str, kind: &str, target: &str) -> Result<()> {
    if let Some(mutation) = store.relation(&job.job_id, owner, kind, target)? {
        if !writes.iter().any(|w| w.table == mutation.table && w.id == mutation.id) { writes.push(mutation); }
    }
    Ok(())
}
fn work_links(store: &Store, job: &Job, writes: &mut Vec<Mutation>, owner: &str, evidence: &[String], requirements: &[String], questions: &[String]) -> Result<()> {
    validate_evidence(&store.dir(&job.job_id)?, evidence)?;
    bounded(requirements, "requirementIds")?;
    bounded(questions, "questionIds")?;
    for id in requirements { let _: Requirement = existing(store, job, "requirements", id)?; }
    for id in questions { let _: Question = existing(store, job, "questions", id)?; }
    for id in evidence { link(store, job, writes, owner, "evidence", id)?; }
    for id in requirements {
        link(store, job, writes, owner, "requirement", id)?;
        link(store, job, writes, id, "work", owner)?;
    }
    for id in questions {
        link(store, job, writes, owner, "question", id)?;
        link(store, job, writes, id, "work", owner)?;
    }
    Ok(())
}
fn not_applicable(store: &Store, job: &Job, work: &Work, added_evidence: &[String]) -> Result<()> {
    if work.status != "notApplicable" { return Ok(()); }
    text(&work.rationale, "notApplicable rationale", 16_384)?;
    let mut evidence = !added_evidence.is_empty();
    store.related(&job.job_id, &work.id, "evidence", |id| { inventory::read_item(&store.dir(&job.job_id)?, id)?; evidence = true; Ok(()) })?;
    if !evidence { return Err(invalid("notApplicable work requires evidenceIds and a rationale")); }
    Ok(())
}
fn document(kind: &str, title: String, requirement_id: Option<String>, parent: Option<String>) -> Result<Document> {
    let document_id = store::id();
    let path = documents::document_path(kind, &document_id, requirement_id.as_deref())?;
    Ok(Document { document_id, r#type: kind.into(), title, path, state: "draft".into(), revision_id: store::id(), hash: None, bytes: 0, requirement_id, specificity: None, reviewed_revision: None, parent_document_id: parent })
}

/// A publication records at most one bounded page of exact work revisions. Historical
/// reviewedWork links remain append-only; omitting a stale dependency does not refresh it.
pub fn review_mutations(store: &Store, job: &Job, doc_id: &str, reviewed_work_ids: &[String]) -> Result<Vec<Mutation>> {
    let _: Document = existing(store, job, "documents", doc_id)?;
    bounded(reviewed_work_ids, "reviewedWorkIds")?;
    let mut writes = Vec::new();
    let mut seen = HashSet::new();
    for id in reviewed_work_ids {
        if !seen.insert(id) { continue; }
        let work: Work = existing(store, job, "work", id)?;
        link(store, job, &mut writes, doc_id, "reviewedWork", id)?;
        writes.push(store::mutation(&format!("reviews-{doc_id}"), id, &json!({
            "documentId":doc_id,"workId":id,"workRevision":work.revision
        }))?);
    }
    writes.push(store::mutation("document-reviews", doc_id, &json!({"documentId":doc_id,"version":1}))?);
    Ok(writes)
}

fn review_receipt_current(store: &Store, job: &Job, doc_id: &str, work_id: &str) -> Result<bool> {
    let path = store.path(&job.job_id, &format!("reviews-{doc_id}"), work_id)?;
    if !path.exists() { return Ok(false); }
    let receipt: Value = store::read(&path)?;
    let work: Work = existing(store, job, "work", work_id)?;
    Ok(review_receipt_matches(&receipt, doc_id, &work))
}

fn review_receipt_matches(receipt: &Value, doc_id: &str, work: &Work) -> bool {
    receipt["documentId"].as_str() == Some(doc_id)
        && receipt["workId"].as_str() == Some(work.id.as_str())
        && receipt["workRevision"].as_u64() == Some(work.revision)
}

/// Lazy, per-document check: work changes never rewrite their dependent documents.
/// The forward receipt shard avoids scanning all inverse reviewedWork links per note.
pub fn document_review_current(store: &Store, job: &Job, document: &Document) -> Result<bool> {
    if document.reviewed_revision.is_none() { return Ok(false); }
    let marker_path = store.path(&job.job_id, "document-reviews", &document.document_id)?;
    if !marker_path.exists() { return Ok(false); }
    let marker: Value = store::read(&marker_path)?;
    if marker["documentId"].as_str() != Some(document.document_id.as_str()) || marker["version"].as_u64() != Some(1) { return Ok(false); }
    let mut current = true;
    store.for_each(&job.job_id, &format!("reviews-{}", document.document_id), |receipt| {
        let Some(work_id) = receipt["workId"].as_str() else { current = false; return Ok(()); };
        let work: Work = existing(store, job, "work", work_id)?;
        current &= review_receipt_matches(&receipt, &document.document_id, &work)
            && store.relation_exists(&job.job_id, &document.document_id, "reviewedWork", work_id)?;
        Ok(())
    })?;
    Ok(current)
}

/// Prepare one atomic checkpoint. The caller alone increments/publishes the job revision and commits.
/// On validation failure neither the durable store nor the supplied job is changed.
pub fn checkpoint(store: &Store, job: &mut Job, updates: Updates, now: u64) -> Result<(Vec<Mutation>, Value)> {
    require_checkpointable(job)?;
    validate_update_bounds(&updates)?;
    let mut draft = Draft {
        next: job.clone(), writes: Vec::new(), touched: HashSet::new(), new_paths: HashSet::new(),
        work: Vec::new(), documents: Vec::new(), requirements: Vec::new(), questions: Vec::new(),
    };
    if changes_work(&updates) { checked_add(&mut draft.next.work_revision, "work revision")?; }
    for add in updates.add_work { add_work_item(store, job, &mut draft, add)?; }
    for update in updates.update_work { update_work_item(store, job, &mut draft, update)?; }
    for add in updates.add_documents { add_document_item(store, job, &mut draft, add)?; }
    for add in updates.add_requirements { add_requirement_item(store, job, &mut draft, add)?; }
    for add in updates.add_questions { add_question_item(store, job, &mut draft, add)?; }
    finish_checkpoint(job, draft, updates.summary, now)
}

/// Candidate checkpoint state; the caller's job is replaced only after every update validates.
struct Draft {
    next: Job,
    writes: Vec<Mutation>,
    touched: HashSet<String>,
    new_paths: HashSet<String>,
    work: Vec<Value>,
    documents: Vec<Value>,
    requirements: Vec<Value>,
    questions: Vec<Value>,
}

fn require_checkpointable(job: &Job) -> Result<()> {
    if !job.inventory_complete || !matches!(job.state, JobState::Running | JobState::Reviewing | JobState::WaitingForHuman) {
        return Err("precondition_failed: Checkpoints require a running job with a complete inventory".into());
    }
    if let Some(stage) = &job.stage {
        if stage.phase != job.phase || !matches!(stage.state.as_str(), "queued" | "active") {
            return Err("stale_stage: Checkpoint does not belong to the active backend phase".into());
        }
    }
    Ok(())
}

fn validate_update_bounds(updates: &Updates) -> Result<()> {
    bounded(&updates.add_work, "addWork")?;
    bounded(&updates.update_work, "updateWork")?;
    bounded(&updates.add_documents, "addDocuments")?;
    bounded(&updates.add_requirements, "addRequirements")?;
    bounded(&updates.add_questions, "addQuestions")?;
    if let Some(summary) = &updates.summary { text(summary, "summary", 16_384)?; }
    Ok(())
}

fn changes_work(updates: &Updates) -> bool {
    !updates.add_work.is_empty() || !updates.update_work.is_empty()
        || updates.add_requirements.iter().any(|add| !add.work_ids.is_empty())
}

fn valid_status(status: &str) -> Result<()> {
    if !STATUSES.contains(&status) { return Err(invalid("Invalid model work status; excluded is human-only")); }
    Ok(())
}

fn reject_future_review(work_phase: Phase, job_phase: Phase, status: &str) -> Result<()> {
    if phase_number(work_phase) > phase_number(job_phase) && matches!(status, "reviewed" | "notApplicable") {
        return Err("precondition_failed: Future-phase work cannot be reviewed before that backend phase".into());
    }
    Ok(())
}

fn add_work_item(store: &Store, job: &Job, draft: &mut Draft, add: AddWork) -> Result<()> {
    reference(&add.r#ref)?;
    if !WORK_KINDS.contains(&add.kind.as_str()) { return Err(invalid("Unknown work kind")); }
    let status = add.status.unwrap_or_else(|| "pending".into());
    valid_status(&status)?;
    text(&add.rationale, "rationale", 16_384)?;
    reject_future_review(add.phase, job.phase, &status)?;
    if let Some(parent) = &add.parent_id { let _: Work = existing(store, job, "work", parent)?; }
    let id = store::id();
    let work = Work { id: id.clone(), parent_id: add.parent_id, phase: add.phase, kind: add.kind, status, rationale: add.rationale, revision: draft.next.work_revision };
    work_links(store, job, &mut draft.writes, &id, &add.evidence_ids, &add.requirement_ids, &add.question_ids)?;
    not_applicable(store, job, &work, &add.evidence_ids)?;
    checked_add(&mut draft.next.counts.work, "work")?;
    if is_open(&work.status) { checked_add(&mut draft.next.counts.work_open, "open work")?; }
    if work.status == "blocked" { checked_add(&mut draft.next.counts.blocked, "blocked work")?; }
    draft.writes.push(store::mutation("work", &id, &work)?);
    draft.work.push(json!({"ref":add.r#ref,"id":id}));
    Ok(())
}

fn update_changes_review(update: &UpdateWork) -> bool {
    update.rationale.is_some() || !update.evidence_ids.is_empty() || !update.requirement_ids.is_empty() || !update.question_ids.is_empty()
}

fn apply_work_fields(work: &mut Work, update: &mut UpdateWork) -> Result<()> {
    if let Some(status) = update.status.take() {
        valid_status(&status)?;
        work.status = status;
    } else if work.status == "reviewed" && update_changes_review(update) {
        work.status = "pending".into();
    }
    if let Some(rationale) = update.rationale.take() { text(&rationale, "rationale", 16_384)?; work.rationale = rationale; }
    Ok(())
}

/// Moves `counter` by the open/blocked transition; `was`/`now` are the membership before and after.
fn shift_counter(counter: &mut u64, was: bool, now: bool, name: &str, underflow: &str) -> Result<()> {
    if was == now { return Ok(()); }
    if !was { return checked_add(counter, name); }
    *counter = counter.checked_sub(1).ok_or(underflow)?;
    Ok(())
}

fn update_work_item(store: &Store, job: &Job, draft: &mut Draft, mut update: UpdateWork) -> Result<()> {
    if !draft.touched.insert(update.id.clone()) { return Err(invalid("updateWork contains the same ID more than once")); }
    let mut work: Work = existing(store, job, "work", &update.id)?;
    if work.status == "excluded" { return Err(invalid("Human-excluded work cannot be modified by the model")); }
    let old_open = is_open(&work.status);
    let old_blocked = work.status == "blocked";
    apply_work_fields(&mut work, &mut update)?;
    reject_future_review(work.phase, job.phase, &work.status)?;
    work_links(store, job, &mut draft.writes, &work.id, &update.evidence_ids, &update.requirement_ids, &update.question_ids)?;
    not_applicable(store, job, &work, &update.evidence_ids)?;
    let counts = &mut draft.next.counts;
    shift_counter(&mut counts.work_open, old_open, is_open(&work.status), "open work", "storage_error: Open work counter underflow")?;
    shift_counter(&mut counts.blocked, old_blocked, work.status == "blocked", "blocked work", "storage_error: Blocked work counter underflow")?;
    work.revision = draft.next.work_revision;
    draft.writes.push(store::mutation("work", &work.id, &work)?);
    Ok(())
}

/// Fixed document types are singletons per job.
fn ensure_fixed_type_free(store: &Store, job: &Job, doc: &Document) -> Result<()> {
    if !FIXED_DOCUMENTS.contains(&doc.r#type.as_str()) { return Ok(()); }
    let mut collision = false;
    store.for_each(&job.job_id, "documents", |v| { let existing: Document = decode(v)?; collision |= existing.r#type == doc.r#type; Ok(()) })?;
    if collision { return Err(invalid(format!("Document type {} is already allocated", doc.r#type))); }
    Ok(())
}

fn add_document_item(store: &Store, job: &Job, draft: &mut Draft, add: AddDocument) -> Result<()> {
    reference(&add.r#ref)?;
    text(&add.title, "title", 1024)?;
    if let Some(parent) = &add.parent_document_id { let _: Document = existing(store, job, "documents", parent)?; }
    // Requirement notes are allocated together with the stable requirement; never create a second owner.
    if add.r#type == "requirement" { return Err(invalid("Requirement documents are allocated by addRequirements")); }
    if add.requirement_id.is_some() { return Err(invalid("Only auto-allocated requirement documents may have requirementId")); }
    let doc = document(&add.r#type, add.title, None, add.parent_document_id)?;
    if !draft.new_paths.insert(doc.path.clone()) { return Err(invalid("Duplicate document path in checkpoint")); }
    ensure_fixed_type_free(store, job, &doc)?;
    if let Some(parent) = &doc.parent_document_id { link(store, job, &mut draft.writes, &doc.document_id, "parent", parent)?; }
    checked_add(&mut draft.next.counts.documents, "documents")?;
    draft.documents.push(json!({"ref":add.r#ref,"documentId":doc.document_id,"revisionId":doc.revision_id,"path":doc.path}));
    draft.writes.push(store::mutation("documents", &doc.document_id, &doc)?);
    Ok(())
}

/// Links a new requirement to `target` work and restamps that work's revision; reviewed work stays reviewed and no dependent document is rewritten.
fn link_requirement_work(store: &Store, job: &Job, writes: &mut Vec<Mutation>, work_revision: u64, id: &str, target: &str) -> Result<()> {
    link(store, job, writes, id, "work", target)?;
    link(store, job, writes, target, "requirement", id)?;
    if let Some(write) = writes.iter_mut().find(|write| write.table == "work" && write.id == target) {
        write.value["revision"] = json!(work_revision);
        return Ok(());
    }
    let mut work: Work = existing(store, job, "work", target)?;
    work.revision = work_revision;
    writes.push(store::mutation("work", target, &work)?);
    Ok(())
}

fn add_requirement_item(store: &Store, job: &Job, draft: &mut Draft, add: AddRequirement) -> Result<()> {
    reference(&add.r#ref)?;
    if !AREAS.contains(&add.area.as_str()) { return Err(invalid("Unknown requirement AREA")); }
    text(&add.title, "title", 1024)?;
    bounded(&add.work_ids, "workIds")?;
    validate_evidence(&store.dir(&job.job_id)?, &add.evidence_ids)?;
    for id in &add.work_ids { let _: Work = existing(store, job, "work", id)?; }
    // next_requirement is the last allocated global ordinal, shared by every AREA.
    checked_add(&mut draft.next.next_requirement, "requirement ordinal")?;
    let id = documents::requirement_id(&add.area, draft.next.next_requirement)?;
    let doc = document("requirement", add.title.clone(), Some(id.clone()), None)?;
    let requirement = Requirement { id: id.clone(), area: add.area, title: add.title, document_id: doc.document_id.clone(), revision: job.revision.checked_add(1).ok_or("storage_error: Revision overflow")? };
    for target in &add.work_ids { link_requirement_work(store, job, &mut draft.writes, draft.next.work_revision, &id, target)?; }
    for target in &add.evidence_ids { link(store, job, &mut draft.writes, &id, "evidence", target)?; }
    checked_add(&mut draft.next.counts.requirements, "requirements")?;
    checked_add(&mut draft.next.counts.documents, "documents")?;
    draft.requirements.push(json!({"ref":add.r#ref,"id":id,"documentId":doc.document_id}));
    draft.writes.push(store::mutation("requirements", &id, &requirement)?);
    draft.writes.push(store::mutation("documents", &doc.document_id, &doc)?);
    Ok(())
}

fn validate_question_options(options: &[QuestionOption]) -> Result<()> {
    let mut labels = HashSet::new();
    for option in options {
        text(&option.label, "option label", 1024)?;
        if !labels.insert(option.label.as_str()) { return Err(invalid("Question options must have unique labels")); }
        if let Some(description) = &option.description { text(description, "option description", 4096)?; }
    }
    Ok(())
}

fn add_question_item(store: &Store, job: &Job, draft: &mut Draft, add: AddQuestion) -> Result<()> {
    reference(&add.r#ref)?;
    text(&add.prompt, "prompt", 16_384)?;
    bounded(&add.work_ids, "workIds")?;
    bounded(&add.options, "options")?;
    validate_question_options(&add.options)?;
    for id in &add.work_ids { let _: Work = existing(store, job, "work", id)?; }
    let id = store::id();
    let question = Question { question_id: id.clone(), prompt: add.prompt, options: add.options, multi: add.multi, required: add.required, state: "open".into(), answer_receipt_id: None, live: false, invocation_id: None };
    for target in &add.work_ids {
        link(store, job, &mut draft.writes, &id, "work", target)?;
        link(store, job, &mut draft.writes, target, "question", &id)?;
    }
    checked_add(&mut draft.next.counts.questions, "questions")?;
    checked_add(&mut draft.next.counts.questions_open, "open questions")?;
    draft.questions.push(json!({"ref":add.r#ref,"questionId":id}));
    draft.writes.push(store::mutation("questions", &id, &question)?);
    Ok(())
}

fn finish_checkpoint(job: &mut Job, draft: Draft, summary: Option<String>, now: u64) -> Result<(Vec<Mutation>, Value)> {
    let Draft { mut next, writes, work, documents, requirements, questions, .. } = draft;
    if !writes.is_empty() { next.completion = None; }
    next.updated_at = now;
    next.checkpoint = Some(Checkpoint { revision: job.revision.checked_add(1).ok_or("storage_error: Revision overflow")?, at: now, summary: summary.unwrap_or_else(|| "Checkpoint recorded".into()) });
    let allocated = json!({"work":work,"documents":documents,"requirements":requirements,"questions":questions});
    // Store enforces its transaction bound as well. Fail before exposing a mutated in-memory job.
    let encoded = serde_json::to_vec(&json!({"job":next,"writes":writes,"activity":allocated})).map_err(|e| e.to_string())?;
    if encoded.len() as u64 > super::MAX_RECORD { return Err(invalid("Checkpoint exceeds the 1 MiB transaction bound; split it into smaller checkpoints")); }
    *job = next;
    Ok((writes, allocated))
}

/// The private job shard and explicit jobId must agree with the owning human
/// disposition relation and its recorded question/work/evidence binding.
fn validate_disposition(store: &Store, job: &Job, owner: &str, id: &str) -> Result<()> {
    let record: Value = existing(store, job, "dispositions", id)?;
    let rationale = record["rationale"].as_str().ok_or_else(|| invalid("Human disposition has no rationale"))?;
    text(rationale, "human disposition rationale", 16_384)?;
    if !disposition_provenance_valid(job, id, &record) {
        return Err(invalid("Human disposition has invalid provenance or job/revision binding"));
    }
    let item = record["itemId"].as_str().ok_or_else(|| invalid("Human disposition has no itemId"))?;
    store::safe_id(item)?;
    match record["kind"].as_str() {
        Some("question") => validate_question_disposition(store, job, owner, item, &record),
        Some(kind @ ("work" | "evidence")) => validate_work_disposition(store, job, owner, item, rationale, kind, &record),
        _ => Err(invalid("Human disposition has an invalid kind")),
    }
}

fn disposition_provenance_valid(job: &Job, id: &str, record: &Value) -> bool {
    record["id"].as_str() == Some(id) && record["actor"].as_str() == Some("human")
        && record["disposition"].as_str() == Some("excludeFromScope")
        && record["revision"].as_u64().is_some_and(|revision| revision > 0 && revision <= job.revision)
        && record["at"].as_u64().is_some()
        && record["jobId"].as_str() == Some(job.job_id.as_str())
}

fn validate_question_disposition(store: &Store, job: &Job, owner: &str, item: &str, record: &Value) -> Result<()> {
    let question: Question = existing(store, job, "questions", owner)?;
    if item != owner || question.question_id != owner || question.state != "excluded"
        || !matches!(record["previousState"].as_str(), Some("open" | "asked")) {
        return Err(invalid("Human question disposition has an invalid owner/item/state binding"));
    }
    let work_ids = record["workIds"].as_array().ok_or_else(|| invalid("Human question disposition has no workIds"))?;
    for value in work_ids {
        let work_id = value.as_str().ok_or_else(|| invalid("Human disposition has an invalid workId"))?;
        let _: Work = existing(store, job, "work", work_id)?;
        if !store.relation_exists(&job.job_id, owner, "work", work_id)? {
            return Err(invalid("Human question disposition has an unrelated workId"));
        }
    }
    Ok(())
}

fn work_disposition_bound(work: &Work, owner: &str, rationale: &str, record: &Value) -> bool {
    record["workId"].as_str() == Some(owner) && work.id == owner && work.status == "excluded"
        && work.rationale == format!("Human exclusion: {rationale}")
        && record["previousStatus"].as_str().is_some_and(|status| STATUSES.contains(&status))
        && record["previousRationale"].as_str().is_some_and(|value| !value.trim().is_empty())
}

fn validate_work_disposition(store: &Store, job: &Job, owner: &str, item: &str, rationale: &str, kind: &str, record: &Value) -> Result<()> {
    let work: Work = existing(store, job, "work", owner)?;
    if !work_disposition_bound(&work, owner, rationale, record) {
        return Err(invalid("Human work disposition has an invalid owner/status/rationale binding"));
    }
    if kind != "work" { return validate_evidence_disposition(store, job, owner, item, record); }
    if item != owner { return Err(invalid("Human work disposition itemId does not match its owner")); }
    Ok(())
}

fn validate_evidence_disposition(store: &Store, job: &Job, owner: &str, item: &str, record: &Value) -> Result<()> {
    if owner != format!("source-{item}") { return Err(invalid("Human evidence disposition does not own its source obligation")); }
    let evidence = inventory::read_item(&store.dir(&job.job_id)?, item)?;
    let expected = json!({"id":evidence.id,"path":evidence.path,"kind":evidence.kind,"state":evidence.state,"sha256":evidence.sha256});
    if record["evidence"] != expected { return Err(invalid("Human evidence disposition does not match the immutable evidence")); }
    Ok(())
}

fn human_exclusion(store: &Store, job: &Job, owner: &str) -> Result<bool> {
    let mut valid = false;
    store.related(&job.job_id, owner, "humanDisposition", |id| {
        valid |= validate_disposition(store, job, owner, id).is_ok();
        Ok(())
    })?;
    Ok(valid)
}

fn reason(reasons: &mut Vec<String>, message: impl Into<String>) { if reasons.len() < 200 { reasons.push(message.into()); } }
fn valid_hash(hash: &str) -> bool { hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()) }

/// Streaming logical completion gate. The caller MUST separately validate the exact committed
/// document hashes through its bound VaultRoot before publishing the completion manifest.
pub fn validate_completion(store: &Store, job: &Job) -> Result<Vec<String>> {
    let mut reasons = Vec::new();
    if !job.inventory_complete { reason(&mut reasons, "Finish the durable evidence inventory before completion"); }
    if job.phase != Phase::Verify { reason(&mut reasons, "Complete the backend survey, specify and gapReview phases before final verification"); }
    let sections = check_work_records(store, job, &mut reasons)?;
    let dir = store.dir(&job.job_id)?;
    check_relations(store, job, &dir, &mut reasons)?;
    report_unfound(&mut reasons, &sections, TEMPLATE_SECTIONS, |title| format!("Create and review templateSection work for {title}"));
    check_questions(store, job, &mut reasons)?;
    let fixed = check_documents(store, job, &mut reasons)?;
    report_unfound(&mut reasons, &fixed, FIXED_DOCUMENTS, |kind| format!("Allocate and commit the required {kind} document"));
    check_requirements(store, job, &mut reasons)?;
    check_inventory(store, job, &dir, &mut reasons)?;
    Ok(reasons)
}

fn report_unfound(reasons: &mut Vec<String>, found: &[bool], names: &[&str], message: impl Fn(&str) -> String) {
    for (name, _) in names.iter().zip(found).filter(|(_, found)| !**found) { reason(reasons, message(*name)); }
}

fn mark_template_section(work: &Work, sections: &mut [bool; 10]) {
    if work.kind != "templateSection" { return; }
    if let Some(index) = TEMPLATE_SECTIONS.iter().position(|title| *title == work.rationale.trim()) { sections[index] = true; }
}

fn check_work_records(store: &Store, job: &Job, reasons: &mut Vec<String>) -> Result<[bool; 10]> {
    let mut sections = [false; 10];
    store.for_each(&job.job_id, "work", |value| {
        let work: Work = decode(value)?;
        mark_template_section(&work, &mut sections);
        check_work(store, job, &work, reasons)
    })?;
    Ok(sections)
}

fn check_work(store: &Store, job: &Job, work: &Work, reasons: &mut Vec<String>) -> Result<()> {
    if !WORK_KINDS.contains(&work.kind.as_str()) { reason(reasons, format!("Work {} has invalid kind {}", work.id, work.kind)); }
    if is_open(&work.status) { reason(reasons, format!("Review work {} ({}, status {})", work.id, work.rationale, work.status)); }
    if work.status == "notApplicable" {
        if let Err(e) = not_applicable(store, job, work, &[]) { reason(reasons, format!("Work {}: {e}", work.id)); }
    }
    if work.status == "reviewed" && matches!(work.kind.as_str(), "capability" | "scenario") && !requirement_covered(store, job, work)? {
        reason(reasons, format!("Reviewed {} work {} must map to an existing stable requirement, or be evidence-backed notApplicable or human-excluded", work.kind, work.id));
    }
    if work.status == "excluded" { check_excluded_work(store, job, work, reasons)?; }
    Ok(())
}

fn requirement_covered(store: &Store, job: &Job, work: &Work) -> Result<bool> {
    let mut covered = false;
    store.related(&job.job_id, &work.id, "requirement", |id| {
        if let Ok(requirement) = store.get::<Requirement>(&job.job_id, "requirements", id) {
            covered |= requirement.id == id;
        }
        Ok(())
    })?;
    Ok(covered)
}

fn check_excluded_work(store: &Store, job: &Job, work: &Work, reasons: &mut Vec<String>) -> Result<()> {
    if !human_exclusion(store, job, &work.id)? {
        reason(reasons, format!("Work {} has no authenticated human exclusion disposition", work.id));
    }
    if work.kind == "templateSection" { reason(reasons, format!("Template section {} must be reviewed or evidence-backed notApplicable, not excluded", work.rationale)); }
    Ok(())
}

fn check_relations(store: &Store, job: &Job, dir: &Path, reasons: &mut Vec<String>) -> Result<()> {
    // One pass validates every durable link target without per-record relation scans or an all-job map.
    store.for_each_relation(&job.job_id, |owner, kind, target| {
        if let Some(error) = relation_error(store, job, dir, owner, kind, target) {
            reason(reasons, format!("Relation {owner} -{kind}-> {target} is invalid: {error}"));
        }
        Ok(())
    })
}

fn relation_error(store: &Store, job: &Job, dir: &Path, owner: &str, kind: &str, target: &str) -> Option<String> {
    match kind {
        "evidence" => inventory::read_item(dir, target).err(),
        "requirement" => store.get::<Requirement>(&job.job_id, "requirements", target).err(),
        "question" => store.get::<Question>(&job.job_id, "questions", target).err(),
        "work" => store.get::<Work>(&job.job_id, "work", target).err(),
        "reviewedWork" => match review_receipt_current(store, job, owner, target) {
            Ok(true) => store.get::<Document>(&job.job_id, "documents", owner).err(),
            Ok(false) => Some("document review does not bind the current work revision".into()),
            Err(error) => Some(error),
        },
        "humanDisposition" => validate_disposition(store, job, owner, target).err(),
        "parent" | "related" => store.get::<Document>(&job.job_id, "documents", target).err(),
        _ => Some(format!("unknown relation kind {kind}")),
    }
}

fn check_questions(store: &Store, job: &Job, reasons: &mut Vec<String>) -> Result<()> {
    store.for_each(&job.job_id, "questions", |value| {
        let question: Question = decode(value)?;
        check_question(store, job, &question, reasons)
    })
}

fn check_question(store: &Store, job: &Job, question: &Question, reasons: &mut Vec<String>) -> Result<()> {
    if !matches!(question.state.as_str(), "answered" | "excluded") { reason(reasons, format!("Obtain a human answer or human exclusion for question {}: {}", question.question_id, question.prompt)); }
    if question.state == "answered" && !matches!(super::questions::authenticated_answer(store, &job.job_id, question), Ok(Some(_))) {
        reason(reasons, format!("Question {} has no authenticated durable human answer receipt", question.question_id));
    }
    if question.state == "excluded" && !human_exclusion(store, job, &question.question_id)? {
        reason(reasons, format!("Question {} has no authenticated human exclusion disposition", question.question_id));
    }
    if question.live { reason(reasons, format!("Retire the live human invocation for question {}", question.question_id)); }
    Ok(())
}

fn check_documents(store: &Store, job: &Job, reasons: &mut Vec<String>) -> Result<[bool; 7]> {
    let mut fixed = [false; 7];
    store.for_each(&job.job_id, "documents", |value| {
        let doc: Document = decode(value)?;
        if let Some(index) = FIXED_DOCUMENTS.iter().position(|kind| *kind == doc.r#type) { fixed[index] = true; }
        check_document(store, job, &doc, reasons);
        Ok(())
    })?;
    Ok(fixed)
}

fn check_document(store: &Store, job: &Job, doc: &Document, reasons: &mut Vec<String>) {
    if doc.state != "committed" || doc.bytes == 0 || !doc.hash.as_deref().map(valid_hash).unwrap_or(false) { reason(reasons, format!("Commit document {} ({}) with its exact content hash", doc.document_id, doc.path)); }
    match document_review_current(store, job, doc) {
        Ok(true) => {},
        Ok(false) => reason(reasons, format!("Review and recommit document {} ({}) against its affected work", doc.document_id, doc.path)),
        Err(error) => reason(reasons, format!("Document {} review is invalid: {error}", doc.document_id)),
    }
    match documents::document_path(&doc.r#type, &doc.document_id, doc.requirement_id.as_deref()) {
        Ok(path) if path == doc.path => {},
        _ => reason(reasons, format!("Document {} has an invalid allocated type/path binding", doc.document_id)),
    }
    if let Some(parent) = &doc.parent_document_id {
        if store.get::<Document>(&job.job_id, "documents", parent).is_err() { reason(reasons, format!("Document {} has missing parent {parent}", doc.document_id)); }
    }
}

fn check_requirements(store: &Store, job: &Job, reasons: &mut Vec<String>) -> Result<()> {
    store.for_each(&job.job_id, "requirements", |value| {
        let req: Requirement = decode(value)?;
        if !AREAS.contains(&req.area.as_str()) { reason(reasons, format!("Requirement {} has invalid AREA {}", req.id, req.area)); }
        match store.get::<Document>(&job.job_id, "documents", &req.document_id) {
            Ok(doc) if doc.r#type == "requirement" && doc.requirement_id.as_deref() == Some(req.id.as_str()) && doc.state == "committed" => {},
            _ => reason(reasons, format!("Requirement {} needs its committed linked requirement document {}", req.id, req.document_id)),
        }
        Ok(())
    })
}

/// A human scope exclusion is recorded on the item's survey obligation (`source-<evidenceId>`), never on the immutable inventory.
fn human_excluded_source(store: &Store, job: &Job, item: &inventory::InventoryItem) -> Result<bool> {
    if item.state == "excluded" { return Ok(false); }
    let Ok(work) = store.get::<Work>(&job.job_id, "work", &format!("source-{}", item.id)) else { return Ok(false); };
    if work.status != "excluded" { return Ok(false); }
    human_exclusion(store, job, &work.id)
}

fn check_inventory(store: &Store, job: &Job, dir: &Path, reasons: &mut Vec<String>) -> Result<()> {
    inventory::for_each(dir, &mut |item| {
        if human_excluded_source(store, job, item)? { return Ok(()); }
        if let Some(pending) = evidence_requirement(item, reasons) { check_coverage(store, job, pending, reasons)?; }
        Ok(())
    })
}

struct PendingEvidence { id: String, path: String, total: u64, hash: String }

/// Immediate inventory reasons; returns work only for admitted readable evidence needing receipt coverage.
fn evidence_requirement(item: &inventory::InventoryItem, reasons: &mut Vec<String>) -> Option<PendingEvidence> {
    let id = &item.id;
    match item.state.as_str() {
        "excluded" => {
            if !matches!(item.exclusion_source.as_deref(), Some("app" | "human")) || item.rationale.as_deref().unwrap_or("").trim().is_empty() {
                reason(reasons, format!("Evidence {id} exclusion lacks app/human provenance and rationale"));
            }
            return None;
        }
        "ready" => {}
        state => { reason(reasons, format!("Evidence {id} ({}) is {state}; fix it or obtain a human scope exclusion", item.path)); return None; }
    }
    match item.kind.as_str() {
        "area" => None,
        "text" | "image" => match item.sha256.as_deref() {
            Some(hash) if valid_hash(hash) => Some(PendingEvidence { id: id.clone(), path: item.path.clone(), total: item.bytes, hash: hash.to_owned() }),
            _ => { reason(reasons, format!("Evidence {id} ({}) has no immutable source hash", item.path)); None }
        },
        kind => { reason(reasons, format!("Evidence {id} ({}) is {kind}; obtain an explicit human scope disposition", item.path)); None }
    }
}

fn merge_ranges(ranges: &mut Vec<(u64, u64)>) {
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len().min(8));
    for &(start, end) in ranges.iter() {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    *ranges = merged;
}

/// Receipts are sharded per evidence (`receipts-<evidenceId>`), so this is O(receipts of one item).
/// Intervals are merged incrementally; memory is bounded by the merged range count of this item only.
fn check_coverage(store: &Store, job: &Job, pending: PendingEvidence, reasons: &mut Vec<String>) -> Result<()> {
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let mut receipts = false;
    store.for_each(&job.job_id, &format!("receipts-{}", pending.id), |receipt| {
        if receipt["evidenceId"].as_str() != Some(pending.id.as_str()) || receipt["sha256"].as_str() != Some(pending.hash.as_str()) || receipt["totalBytes"].as_u64() != Some(pending.total) { return Ok(()); }
        let (Some(offset), Some(bytes)) = (receipt["offset"].as_u64(), receipt["bytes"].as_u64()) else { return Ok(()); };
        let Some(end) = offset.checked_add(bytes) else { return Ok(()); };
        if end > pending.total { return Ok(()); }
        receipts = true;
        ranges.push((offset, end));
        if ranges.len() >= 256 { merge_ranges(&mut ranges); }
        Ok(())
    })?;
    merge_ranges(&mut ranges);
    let covered = match ranges.first() { Some(&(0, end)) => end, _ => 0 };
    if !receipts || covered < pending.total {
        reason(reasons, format!("Read all admitted evidence {} ({}) with native read receipts: contiguous coverage {covered}/{} bytes", pending.id, pending.path, pending.total));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use inventory::{InventoryItem, InventoryRoot};

    struct Fixture { store: Store, job: Job, root: std::path::PathBuf }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("omp-srs-ledger-{}", store::id()));
            std::fs::create_dir_all(root.join("repo")).unwrap();
            let root = crate::setup::canonical(&root).unwrap();
            std::fs::write(root.join("repo").join("a.txt"), "0123456789").unwrap();
            std::fs::write(root.join("repo").join("b.txt"), "abc").unwrap();
            let job: Job = serde_json::from_value(json!({
                "jobId":store::id(), "scope":{"folder":"fixture", "repositoryRoot":"fixture", "vaultId":store::id(), "vaultRoot":"fixture", "vaultMode":"markdown", "roots":[], "exclusions":[], "template":{}},
                "revision":1,"state":"running","phase":"specify","scopeHash":"fixture","sourceHash":null,"attemptId":null,"producer":null,"stage":null,"nextStage":1,"nextRequirement":0,
                "counts":{"areas":0,"evidenceFiles":0,"work":0,"workOpen":0,"documents":0,"requirements":0,"questions":0,"questionsOpen":0,"blocked":0,"excluded":0},
                "checkpoint":null,"error":null,"createdAt":1,"updatedAt":1,"completion":null,"inventoryComplete":true,"humanExclusions":0,"documentRevision":0,"workRevision":0
            })).unwrap();
            let store = Store::new(root.join("store"));
            store.commit(&job, Vec::new(), json!({"fixture":true})).unwrap();
            let roots = [InventoryRoot { path: root.join("repo"), kind: "repository".into(), rationale: "fixture".into() }];
            inventory::inventory(&store.dir(&job.job_id).unwrap(), &roots, &[], &[], &|| false).unwrap();
            Self { store, job, root }
        }
        fn try_apply(&mut self, value: Value) -> Result<Value> {
            let updates = serde_json::from_value(value).unwrap();
            let (writes, allocated) = checkpoint(&self.store, &mut self.job, updates, 2)?;
            self.job.revision += 1;
            self.store.commit(&self.job, writes, json!({"fixture":true})).unwrap();
            Ok(allocated)
        }
        fn apply(&mut self, value: Value) -> Value { self.try_apply(value).unwrap() }
        fn evidence(&self, path: &str) -> InventoryItem {
            inventory::page(&self.store.dir(&self.job.job_id).unwrap(), 0, 200).unwrap().items.into_iter().find(|i| i.path == path).unwrap()
        }
        fn commit(&mut self, writes: Vec<Mutation>) {
            self.job.revision += 1;
            self.store.commit(&self.job, writes, json!({"fixture":true})).unwrap();
        }
        fn receipt(&mut self, item: &InventoryItem, offset: u64, bytes: u64) {
            let id = store::id();
            let value = json!({"receiptId":id,"evidenceId":item.id,"offset":offset,"bytes":bytes,"totalBytes":item.bytes,"sha256":item.sha256,"at":1});
            self.commit(vec![store::mutation(&format!("receipts-{}", item.id), &id, &value).unwrap()]);
        }
        fn publish_ledger_documents(&mut self, work_ids: &[String]) {
            let mut docs = Vec::new();
            self.store.for_each(&self.job.job_id, "documents", |value| { docs.push(decode::<Document>(value)?); Ok(()) }).unwrap();
            for mut doc in docs {
                doc.state = "committed".into();
                doc.bytes = 1;
                doc.hash = Some(store::hash(b"x"));
                doc.reviewed_revision = Some(self.job.work_revision);
                let mut writes = review_mutations(&self.store, &self.job, &doc.document_id, work_ids).unwrap();
                writes.push(store::mutation("documents", &doc.document_id, &doc).unwrap());
                self.commit(writes);
            }
        }
        fn complete_ledger(&mut self, map_second: bool) -> Vec<String> {
            self.job.phase = Phase::Verify;
            let mut work: Vec<Value> = TEMPLATE_SECTIONS.iter().map(|title| json!({
                "phase":"verify","kind":"templateSection","status":"reviewed","rationale":title
            })).collect();
            for title in ["Reserve a booking", "Cancel a booking"] {
                work.push(json!({"phase":"specify","kind":"capability","status":"reviewed","rationale":title}));
            }
            let allocated = self.apply(json!({"addWork":work}));
            let ids: Vec<String> = allocated["work"].as_array().unwrap()[10..].iter().map(|item| item["id"].as_str().unwrap().to_owned()).collect();
            let linked = if map_second { ids.clone() } else { vec![ids[0].clone()] };
            let docs: Vec<Value> = FIXED_DOCUMENTS.iter().map(|kind| json!({"type":kind,"title":kind})).collect();
            self.apply(json!({"addDocuments":docs,"addRequirements":[{"area":"FUN","title":"Booking lifecycle","workIds":linked}]}));
            self.publish_ledger_documents(&ids);
            for name in ["a.txt", "b.txt"] {
                let item = self.evidence(name);
                self.receipt(&item, 0, item.bytes);
            }
            ids
        }
        fn exclude(&mut self, owner: &str, kind: &str, item: &str) -> (String, Value) {
            let id = store::id();
            let rationale = "Not in the agreed delivery scope";
            let mut record = json!({"id":id,"jobId":self.job.job_id,"itemId":item,"disposition":"excludeFromScope","rationale":rationale,
                "at":2,"actor":"human","revision":self.job.revision + 1,"kind":kind});
            let mut writes = Vec::new();
            if kind == "question" {
                let mut question: Question = self.store.get(&self.job.job_id, "questions", owner).unwrap();
                record["previousState"] = json!(question.state);
                record["workIds"] = json!([]);
                question.state = "excluded".into();
                writes.push(store::mutation("questions", owner, &question).unwrap());
            } else {
                let mut work: Work = self.store.get(&self.job.job_id, "work", owner).unwrap();
                record["workId"] = json!(owner);
                record["previousStatus"] = json!(work.status);
                record["previousRationale"] = json!(work.rationale);
                if kind == "evidence" {
                    let evidence = inventory::read_item(&self.store.dir(&self.job.job_id).unwrap(), item).unwrap();
                    record["evidence"] = json!({"id":evidence.id,"path":evidence.path,"kind":evidence.kind,"state":evidence.state,"sha256":evidence.sha256});
                }
                self.job.work_revision += 1;
                work.revision = self.job.work_revision;
                work.status = "excluded".into();
                work.rationale = format!("Human exclusion: {rationale}");
                writes.push(store::mutation("work", owner, &work).unwrap());
            }
            writes.push(store::mutation("dispositions", &id, &record).unwrap());
            writes.push(self.store.relation(&self.job.job_id, owner, "humanDisposition", &id).unwrap().unwrap());
            self.commit(writes);
            (id, record)
        }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.root); } }

    #[test]
    fn allocates_more_than_500_stable_global_requirement_ids_on_disk() {
        let mut f = Fixture::new();
        let mut allocated = Vec::new();
        for batch in 0..6 {
            let requirements: Vec<Value> = (0..100).map(|index| json!({"area":if (batch * 100 + index) % 2 == 0 {"FUN"} else {"QOS"},"title":format!("Requirement {batch}-{index}")})).collect();
            let response = f.apply(json!({"addRequirements":requirements}));
            for item in response["requirements"].as_array().unwrap() { allocated.push(item["id"].as_str().unwrap().to_owned()); }
        }
        assert_eq!(f.job.counts.requirements, 600);
        assert_eq!(f.job.counts.documents, 600);
        assert_eq!(f.job.next_requirement, 600);
        assert_eq!(allocated[0], "REQ-FUN-001");
        assert_eq!(allocated[599], "REQ-QOS-600");
        assert_eq!(allocated.iter().collect::<HashSet<_>>().len(), 600);
        let reopened = Store::new(f.store.root.clone());
        assert_eq!(reopened.job(&f.job.job_id).unwrap().next_requirement, 600);
        for id in allocated {
            let req: Requirement = reopened.get(&f.job.job_id, "requirements", &id).unwrap();
            let doc: Document = reopened.get(&f.job.job_id, "documents", &req.document_id).unwrap();
            assert_eq!(doc.requirement_id.as_deref(), Some(id.as_str()));
            assert_eq!(doc.state, "draft");
        }
    }

    #[test]
    fn invalid_area_and_oversized_reference_arrays_are_rejected() {
        let mut f = Fixture::new();
        assert!(f.try_apply(json!({"addRequirements":[{"area":"SEC","title":"Not an allowed AREA"}]})).is_err());
        let ids: Vec<String> = (0..201).map(|_| store::id()).collect();
        assert!(f.try_apply(json!({"addWork":[{"phase":"specify","kind":"capability","requirementIds":ids,"rationale":"too many"}]})).is_err());
        assert_eq!(f.job.counts.requirements, 0);
    }

    #[test]
    fn aliases_are_not_authority_and_validation_is_atomic() {
        let mut f = Fixture::new();
        let before = serde_json::to_value(&f.job).unwrap();
        let rejected = f.try_apply(json!({"addWork":[{"ref":"alias","phase":"specify","kind":"capability","rationale":"A capability"}],"addRequirements":[{"area":"FUN","title":"Bad alias link","workIds":["alias"]}]}));
        assert!(rejected.is_err());
        assert_eq!(serde_json::to_value(&f.job).unwrap(), before);
        assert_eq!(f.store.page(&f.job.job_id, "work", 0, 200).unwrap()["total"], 0);
        let work = f.apply(json!({"addWork":[{"ref":"alias","phase":"specify","kind":"capability","rationale":"A capability"}]}));
        assert_eq!(work["work"][0]["ref"], "alias");
        let work_id = work["work"][0]["id"].as_str().unwrap().to_owned();
        let response = f.apply(json!({"addRequirements":[{"area":"FUN","title":"Linked","workIds":[work_id.clone()]}]}));
        let requirement = response["requirements"][0]["id"].as_str().unwrap();
        assert!(f.store.relation_exists(&f.job.job_id, requirement, "work", &work_id).unwrap());
        assert!(f.store.relation_exists(&f.job.job_id, &work_id, "requirement", requirement).unwrap());
    }

    #[test]
    fn model_cannot_exclude_and_not_applicable_needs_valid_evidence_and_rationale() {
        let mut f = Fixture::new();
        let a = f.evidence("a.txt");
        for status in ["excluded", "done"] {
            assert!(f.try_apply(json!({"addWork":[{"phase":"specify","kind":"capability","status":status,"rationale":"Reason"}]})).is_err(), "{status}");
        }
        assert!(f.try_apply(json!({"addWork":[{"phase":"specify","kind":"capability","status":"notApplicable","rationale":"No evidence"}]})).is_err());
        assert!(f.try_apply(json!({"addWork":[{"phase":"specify","kind":"capability","status":"notApplicable","evidenceIds":["0".repeat(64)],"rationale":"Unknown evidence"}]})).is_err());
        assert!(f.try_apply(json!({"addWork":[{"phase":"specify","kind":"capability","status":"notApplicable","evidenceIds":[a.id.clone()],"rationale":"  "}]})).is_err());
        f.apply(json!({"addWork":[{"phase":"specify","kind":"capability","status":"notApplicable","evidenceIds":[a.id],"rationale":"Evidence shows not relevant"}]}));
        let human = Work { id: store::id(), parent_id: None, phase: Phase::Specify, kind: "capability".into(), status: "excluded".into(), rationale: "Human exclusion".into(), revision: 1 };
        f.commit(vec![store::mutation("work", &human.id, &human).unwrap()]);
        assert!(f.try_apply(json!({"updateWork":[{"id":human.id,"status":"pending"}]})).is_err());
        assert!(f.try_apply(json!({"addWork":[{"phase":"verify","kind":"templateSection","status":"reviewed","rationale":"Introduction"}]})).is_err());
    }

    #[test]
    fn work_updates_append_relations_and_invalidate_only_affected_reviews() {
        let mut f = Fixture::new();
        let (a, b) = (f.evidence("a.txt"), f.evidence("b.txt"));
        let created = f.apply(json!({
            "addWork":[{"phase":"specify","kind":"capability","status":"reviewed","evidenceIds":[a.id.clone()],"rationale":"Done"}],
            "addDocuments":[{"type":"glossary","title":"Glossary"},{"type":"traceability","title":"Trace"}]
        }));
        let work_id = created["work"][0]["id"].as_str().unwrap().to_owned();
        let glossary = created["documents"][0]["documentId"].as_str().unwrap().to_owned();
        let trace = created["documents"][1]["documentId"].as_str().unwrap().to_owned();
        assert_eq!(f.job.counts.work_open, 0);
        let mut writes = Vec::new();
        for id in [&glossary, &trace] {
            let mut doc: Document = f.store.get(&f.job.job_id, "documents", id).unwrap();
            doc.reviewed_revision = Some(f.job.work_revision);
            writes.push(store::mutation("documents", id, &doc).unwrap());
            writes.extend(review_mutations(&f.store, &f.job, id, if id == &glossary { std::slice::from_ref(&work_id) } else { &[] }).unwrap());
        }
        f.commit(writes);
        f.apply(json!({"updateWork":[{"id":work_id.clone(),"evidenceIds":[b.id.clone()],"rationale":"Changed after review"}]}));
        assert!(f.store.relation_exists(&f.job.job_id, &work_id, "evidence", &a.id).unwrap());
        assert!(f.store.relation_exists(&f.job.job_id, &work_id, "evidence", &b.id).unwrap());
        let work: Work = f.store.get(&f.job.job_id, "work", &work_id).unwrap();
        assert_eq!(work.status, "pending");
        assert_eq!(f.job.counts.work_open, 1);
        assert_eq!(f.job.work_revision, 2);
        let glossary: Document = f.store.get(&f.job.job_id, "documents", &glossary).unwrap();
        let trace: Document = f.store.get(&f.job.job_id, "documents", &trace).unwrap();
        assert_eq!(glossary.reviewed_revision, Some(1));
        assert_eq!(trace.reviewed_revision, Some(1));
        assert!(!document_review_current(&f.store, &f.job, &glossary).unwrap());
        assert!(document_review_current(&f.store, &f.job, &trace).unwrap());
        let writes = review_mutations(&f.store, &f.job, &glossary.document_id, &[]).unwrap();
        f.commit(writes);
        assert!(!document_review_current(&f.store, &f.job, &glossary).unwrap(), "omitting stale historical work cannot refresh its review");
        let writes = review_mutations(&f.store, &f.job, &glossary.document_id, &[work_id]).unwrap();
        f.commit(writes);
        assert!(document_review_current(&f.store, &f.job, &glossary).unwrap());
    }

    #[test]
    fn completion_requires_union_of_receipts_for_every_admitted_file() {
        let mut f = Fixture::new();
        f.job.phase = Phase::Verify;
        let (a, b) = (f.evidence("a.txt"), f.evidence("b.txt"));
        let missing = |f: &Fixture| validate_completion(&f.store, &f.job).unwrap().into_iter().filter(|r| r.contains("Read all admitted evidence")).collect::<Vec<_>>();
        assert_eq!(missing(&f).len(), 2);
        f.receipt(&a, 5, 5);
        f.receipt(&a, 0, 3);
        f.receipt(&b, 0, 3);
        let reasons = missing(&f);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("a.txt") && reasons[0].contains("3/10"));
        f.receipt(&a, 3, 2);
        assert!(missing(&f).is_empty());
    }

    #[test]
    fn completion_reports_bounded_reasons_after_streaming_all_records() {
        let mut f = Fixture::new();
        for _ in 0..3 {
            let questions: Vec<Value> = (0..100).map(|n| json!({"prompt":format!("Question {n}")})).collect();
            f.apply(json!({"addQuestions":questions}));
        }
        f.job.phase = Phase::Verify;
        let reasons = validate_completion(&f.store, &f.job).unwrap();
        assert_eq!(reasons.len(), 200);
        assert!(reasons.iter().any(|r| r.contains("Obtain a human answer")));
    }

    #[test]
    fn positive_ledger_rejects_an_omitted_second_capability_until_mapped() {
        let mut f = Fixture::new();
        let ids = f.complete_ledger(false);
        let reasons = validate_completion(&f.store, &f.job).unwrap();
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(reasons[0].contains(&ids[1]) && reasons[0].contains("stable requirement"));
        let requirement: Requirement = decode(f.store.page(&f.job.job_id, "requirements", 0, 1).unwrap()["items"][0].clone()).unwrap();
        f.apply(json!({"updateWork":[{"id":ids[1],"status":"reviewed","requirementIds":[requirement.id]}]}));
        assert!(!validate_completion(&f.store, &f.job).unwrap().is_empty(), "mapping changes invalidate earlier document reviews");
        f.publish_ledger_documents(&ids);
        assert!(validate_completion(&f.store, &f.job).unwrap().is_empty());
    }

    #[test]
    fn capability_and_scenario_nonapplicability_need_evidence_not_requirements() {
        let mut f = Fixture::new();
        f.complete_ledger(true);
        assert!(validate_completion(&f.store, &f.job).unwrap().is_empty());
        let evidence = f.evidence("a.txt");
        for kind in ["capability", "scenario"] {
            assert!(f.try_apply(json!({"addWork":[{"phase":"specify","kind":kind,"status":"notApplicable","rationale":"No support in the evidence"}]})).is_err());
            f.apply(json!({"addWork":[{"phase":"specify","kind":kind,"status":"notApplicable","rationale":"Evidence shows the feature is absent","evidenceIds":[evidence.id]}]}));
        }
        assert!(validate_completion(&f.store, &f.job).unwrap().is_empty());
        let scenario = f.apply(json!({"addWork":[{"phase":"specify","kind":"scenario","status":"reviewed","rationale":"Unmapped cancellation scenario"}]}));
        let id = scenario["work"][0]["id"].as_str().unwrap();
        assert!(validate_completion(&f.store, &f.job).unwrap().iter().any(|reason| reason.contains(id) && reason.contains("stable requirement")));
    }

    #[test]
    fn adding_requirements_keeps_reviewed_work_closed_but_stales_document_reviews() {
        let mut f = Fixture::new();
        let ids = f.complete_ledger(true);
        let before = f.job.work_revision;
        let updates = serde_json::from_value(json!({"addRequirements":[{"area":"FUN","title":"Another booking obligation","workIds":[ids[0]]}]})).unwrap();
        let (writes, _) = checkpoint(&f.store, &mut f.job, updates, 3).unwrap();
        assert!(writes.iter().filter(|write| write.table == "documents").all(|write| write.value["state"] == "draft"), "existing dependent documents must never be rewritten");
        f.commit(writes);
        let work: Work = f.store.get(&f.job.job_id, "work", &ids[0]).unwrap();
        assert_eq!(work.status, "reviewed");
        assert_eq!(work.revision, before + 1);
        assert_eq!(f.job.counts.work_open, 0);
        assert!(validate_completion(&f.store, &f.job).unwrap().iter().any(|reason| reason.contains("Review and recommit")));
        f.publish_ledger_documents(&ids);
        assert!(validate_completion(&f.store, &f.job).unwrap().is_empty());
    }

    #[test]
    fn authentic_human_dispositions_close_work_questions_and_evidence() {
        let mut f = Fixture::new();
        f.complete_ledger(true);
        let work = f.apply(json!({"addWork":[{"phase":"specify","kind":"capability","rationale":"Excluded feature"}]}));
        let work_id = work["work"][0]["id"].as_str().unwrap().to_owned();
        f.exclude(&work_id, "work", &work_id);
        let question = f.apply(json!({"addQuestions":[{"prompt":"Excluded scope decision"}]}));
        let question_id = question["questions"][0]["questionId"].as_str().unwrap().to_owned();
        f.exclude(&question_id, "question", &question_id);
        let evidence = f.evidence("a.txt");
        let owner = format!("source-{}", evidence.id);
        let source = Work { id: owner.clone(), parent_id: None, phase: Phase::Survey, kind: "sourceArea".into(), status: "blocked".into(), rationale: "Unreadable source".into(), revision: f.job.work_revision };
        f.commit(vec![store::mutation("work", &owner, &source).unwrap()]);
        f.exclude(&owner, "evidence", &evidence.id);
        assert!(validate_completion(&f.store, &f.job).unwrap().is_empty());
    }

    #[test]
    fn missing_and_tampered_human_dispositions_never_authorize_exclusions() {
        let mut f = Fixture::new();
        f.complete_ledger(true);
        let work = f.apply(json!({"addWork":[{"phase":"specify","kind":"capability","rationale":"Excluded feature"}]}));
        let owner = work["work"][0]["id"].as_str().unwrap().to_owned();
        let (id, original) = f.exclude(&owner, "work", &owner);
        assert!(validate_completion(&f.store, &f.job).unwrap().is_empty());
        for (field, value) in [
            ("actor", json!("model")), ("itemId", json!(store::id())), ("workId", json!(store::id())),
            ("id", json!(store::id())), ("jobId", json!(store::id())), ("revision", json!(u64::MAX)),
            ("rationale", json!("Changed scope")), ("rationale", json!(" ")), ("disposition", json!("provideHumanAnswer"))
        ] {
            let mut tampered = original.clone();
            tampered[field] = value;
            f.commit(vec![store::mutation("dispositions", &id, &tampered).unwrap()]);
            assert!(validate_completion(&f.store, &f.job).unwrap().iter().any(|reason| reason.contains("human exclusion") || reason.contains("humanDisposition")), "{field}");
        }
        f.commit(vec![store::mutation("dispositions", &id, &original).unwrap()]);
        assert!(validate_completion(&f.store, &f.job).unwrap().is_empty());
        let unproven = Work { id: store::id(), parent_id: None, phase: Phase::Survey, kind: "scenario".into(), status: "excluded".into(), rationale: "No human receipt".into(), revision: f.job.work_revision };
        f.commit(vec![store::mutation("work", &unproven.id, &unproven).unwrap()]);
        assert!(validate_completion(&f.store, &f.job).unwrap().iter().any(|reason| reason.contains(&unproven.id) && reason.contains("authenticated human exclusion")));
    }

    #[test]
    fn question_and_evidence_dispositions_reject_tampered_owner_and_snapshot_bindings() {
        let mut f = Fixture::new();
        let work_ids = f.complete_ledger(true);
        let allocated = f.apply(json!({"addQuestions":[{"prompt":"Scope exception"}]}));
        let question_id = allocated["questions"][0]["questionId"].as_str().unwrap().to_owned();
        let (question_disposition, question_record) = f.exclude(&question_id, "question", &question_id);
        assert!(validate_completion(&f.store, &f.job).unwrap().is_empty());
        for (field, value) in [
            ("itemId", json!(work_ids[0])), ("workIds", json!([work_ids[0]])),
            ("actor", json!("model")), ("rationale", json!(" "))
        ] {
            let mut tampered = question_record.clone();
            tampered[field] = value;
            f.commit(vec![store::mutation("dispositions", &question_disposition, &tampered).unwrap()]);
            assert!(validate_completion(&f.store, &f.job).unwrap().iter().any(|reason| reason.contains("human exclusion") || reason.contains("humanDisposition")), "{field}");
        }
        f.commit(vec![store::mutation("dispositions", &question_disposition, &question_record).unwrap()]);
        let evidence = f.evidence("a.txt");
        let owner = format!("source-{}", evidence.id);
        let source = Work { id: owner.clone(), parent_id: None, phase: Phase::Survey, kind: "sourceArea".into(), status: "blocked".into(), rationale: "Unsupported input".into(), revision: f.job.work_revision };
        f.commit(vec![store::mutation("work", &owner, &source).unwrap()]);
        let (evidence_disposition, evidence_record) = f.exclude(&owner, "evidence", &evidence.id);
        assert!(validate_completion(&f.store, &f.job).unwrap().is_empty());
        let mut tampered = evidence_record.clone();
        tampered["evidence"]["sha256"] = json!("0".repeat(64));
        f.commit(vec![store::mutation("dispositions", &evidence_disposition, &tampered).unwrap()]);
        assert!(validate_completion(&f.store, &f.job).unwrap().iter().any(|reason| reason.contains("immutable evidence")));
        f.commit(vec![store::mutation("dispositions", &evidence_disposition, &evidence_record).unwrap()]);
        assert!(validate_completion(&f.store, &f.job).unwrap().is_empty());
    }

    #[test]
    fn thousands_of_dependent_documents_do_not_expand_a_work_checkpoint() {
        let mut f = Fixture::new();
        let allocated = f.apply(json!({"addWork":[{"phase":"specify","kind":"capability","status":"reviewed","rationale":"Widely referenced feature"}]}));
        let work_id = allocated["work"][0]["id"].as_str().unwrap().to_owned();
        let mut documents = Vec::new();
        let mut old_invalidation_bytes = 0;
        for _ in 0..21 {
            let additions: Vec<Value> = (0..100).map(|_| json!({"type":"domain","title":"D".repeat(700)})).collect();
            let allocated = f.apply(json!({"addDocuments":additions}));
            let mut writes = Vec::new();
            for item in allocated["documents"].as_array().unwrap() {
                let id = item["documentId"].as_str().unwrap();
                let mut doc: Document = f.store.get(&f.job.job_id, "documents", id).unwrap();
                doc.reviewed_revision = Some(1);
                writes.extend(review_mutations(&f.store, &f.job, id, std::slice::from_ref(&work_id)).unwrap());
                writes.push(store::mutation("documents", id, &doc).unwrap());
                let mut invalidated = doc.clone();
                invalidated.reviewed_revision = None;
                old_invalidation_bytes += serde_json::to_vec(&store::mutation("documents", id, &invalidated).unwrap()).unwrap().len() as u64;
                documents.push(doc);
            }
            f.commit(writes);
        }
        assert!(old_invalidation_bytes > super::super::MAX_RECORD);
        assert_eq!(documents.len(), 2100);
        assert!(document_review_current(&f.store, &f.job, &documents[0]).unwrap());
        let updates = serde_json::from_value(json!({"updateWork":[{"id":work_id,"status":"reviewed","rationale":"Updated review input"}]})).unwrap();
        let (writes, _) = checkpoint(&f.store, &mut f.job, updates, 3).unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].table, "work");
        f.commit(writes);
        for doc in &documents {
            assert!(!document_review_current(&f.store, &f.job, doc).unwrap());
        }
        let first = &documents[0];
        let writes = review_mutations(&f.store, &f.job, &first.document_id, std::slice::from_ref(&work_id)).unwrap();
        f.commit(writes);
        assert!(document_review_current(&f.store, &f.job, first).unwrap());
        assert!(!document_review_current(&f.store, &f.job, &documents[1]).unwrap());
        let reopened = Store::new(f.store.root.clone());
        assert!(!document_review_current(&reopened, &f.job, &documents[2099]).unwrap());
        assert!(review_mutations(&f.store, &f.job, &first.document_id, &vec![work_id; 201]).is_err());
    }
}
