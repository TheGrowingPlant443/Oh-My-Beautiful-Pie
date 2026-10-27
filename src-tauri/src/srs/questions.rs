//! Authenticated Ask receipts and human item dispositions. Every durable change here runs under the service
//! gate (taken by `ask_bridge`, held by the caller of `dispose_srs_item` internals) and commits through
//! `SrsService::save`, so revisions and events stay monotonic. A live native Ask is never answered here:
//! the human answer is routed to the existing HumanDecision authority and the bridge's authenticated receipt
//! is the only path that records it.
use super::store::Mutation;
use super::*;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::HashSet;

const MAX_ASK_QUESTIONS: usize = 50;
const MAX_TEXT: usize = 16_384;

fn invalid(message: impl AsRef<str>) -> String { format!("invalid_request: {}", message.as_ref()) }
fn precondition(message: impl AsRef<str>) -> String { format!("precondition_failed: {}", message.as_ref()) }
fn not_found(message: impl AsRef<str>) -> String { format!("not_found: {}", message.as_ref()) }
fn storage(message: impl std::fmt::Display) -> String { format!("storage_error: {message}") }
/// Keeps an existing `code: message` error intact; anything else is a storage failure.
fn coded(error: String) -> String {
    if error.split_once(": ").is_some_and(|(code, _)| !code.is_empty() && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')) { error } else { storage(error) }
}
fn parse<T: DeserializeOwned>(value: Value) -> Result<T> { serde_json::from_value(value).map_err(|e| invalid(e.to_string())) }
fn plain_text(value: &str, name: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(|c| c == '\0' || (c.is_control() && !matches!(c, '\n' | '\r' | '\t'))) {
        return Err(invalid(format!("{name} must be nonblank text of at most {max} UTF-8 bytes")));
    }
    Ok(())
}
fn ask_id(value: &str, name: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) { return Err(invalid(format!("{name} must be a nonblank identifier of at most 512 bytes"))); }
    Ok(())
}
fn optional_text(value: Option<String>, name: &str) -> Result<Option<String>> {
    match value {
        Some(v) if v.trim().is_empty() => Ok(None),
        Some(v) => { plain_text(&v, name, MAX_TEXT)?; Ok(Some(v)) }
        None => Ok(None),
    }
}
fn inc(value: &mut u64, name: &str) -> Result<()> { *value = value.checked_add(1).ok_or_else(|| storage(format!("{name} counter overflow")))?; Ok(()) }
fn dec(value: &mut u64, name: &str) -> Result<()> { *value = value.checked_sub(1).ok_or_else(|| storage(format!("{name} counter underflow")))?; Ok(()) }
fn inv_key(invocation: &str) -> String { store::hash(invocation.as_bytes()) }
fn is_open(status: &str) -> bool { !matches!(status, "reviewed" | "notApplicable" | "excluded") }
/// Waiting jobs unstick only when no question and no blocked entry remains; they pause for an explicit Resume.
fn settle(job: &mut Job) {
    if job.counts.blocked == 0 && job.error.as_ref().is_some_and(|e| e.code == "needs_evidence") { job.error = None; }
    if job.state == JobState::WaitingForHuman && job.counts.questions_open == 0 && job.counts.blocked == 0 { job.state = JobState::Paused; job.error = None; }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NativeOption { Label(String), Object(OptionObject) }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionObject { label: String }
impl NativeOption { fn label(&self) -> &str { match self { Self::Label(l) => l, Self::Object(o) => &o.label } } }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeQuestion { id: String, question: String, #[serde(default)] options: Vec<NativeOption>, #[serde(default)] multi: bool }
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MapRequest { invocation_id: String, #[serde(default)] tool_call_id: Option<String>, questions: Vec<NativeQuestion> }
#[derive(Deserialize, Clone, Copy)]
enum Provenance { #[serde(rename = "human")] Human, #[serde(rename = "nativeHuman")] NativeHuman }
impl Provenance { fn as_str(self) -> &'static str { match self { Self::Human => "human", Self::NativeHuman => "nativeHuman" } } }
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AskResult { id: String, selected_options: Vec<String>, #[serde(default)] custom_input: Option<String>, #[serde(default)] note: Option<String> }
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReceiptRequest { invocation_id: String, provenance: Provenance, results: Vec<AskResult> }
#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum RetireReason { Cancelled, Chat, Aborted, Timeout, Retired, Failed }
impl RetireReason {
    fn as_str(self) -> &'static str { match self { Self::Cancelled => "cancelled", Self::Chat => "chat", Self::Aborted => "aborted", Self::Timeout => "timeout", Self::Retired => "retired", Self::Failed => "failed" } }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetiredRequest { invocation_id: String, reason: RetireReason }

/// A validated, normalized answer. Blank custom input/note collapse to `None`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Norm {
    id: String,
    selected_options: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")] custom_input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] note: Option<String>,
}
fn normalize_answer(id: &str, selected: Vec<String>, custom: Option<String>, note: Option<String>) -> Result<Norm> {
    if selected.len() > 200 { return Err(invalid("At most 200 options may be selected")); }
    let mut seen = HashSet::new();
    for s in &selected {
        if s.is_empty() || s.len() > 1024 || !seen.insert(s.as_str()) { return Err(invalid("Selected options must be unique, nonblank labels")); }
    }
    Ok(Norm { id: id.into(), selected_options: selected, custom_input: optional_text(custom, "customInput")?, note: optional_text(note, "note")? })
}
/// Each answer is validated against the actual durable ledger question, never against model-supplied data.
fn check_answer(question: &Question, answer: &Norm) -> Result<()> {
    let labels: HashSet<&str> = question.options.iter().map(|o| o.label.as_str()).collect();
    if let Some(bad) = answer.selected_options.iter().find(|s| !labels.contains(s.as_str())) {
        return Err(invalid(format!("'{bad}' is not an option of question {}", question.question_id)));
    }
    if !question.multi && answer.selected_options.len() > 1 { return Err(invalid(format!("Question {} accepts one selected option", question.question_id))); }
    if question.required && answer.selected_options.is_empty() && answer.custom_input.is_none() {
        return Err(invalid(format!("Required question {} needs a selected option or custom input", question.question_id)));
    }
    Ok(())
}
fn answer_value(answer: &Norm) -> Value {
    json!({"questionId":answer.id,"selectedOptions":answer.selected_options,"customInput":answer.custom_input,"note":answer.note})
}

/// Immutable mapping of one native Ask invocation to ledger questions, bound to the exact producer/attempt/stage.
/// Only `state`, receipt and retirement fields ever change, and only once.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Invocation {
    invocation_id: String,
    tool_call_id: Option<String>,
    job_id: String,
    attempt_id: String,
    stage_id: String,
    producer: DependencyProducer,
    fingerprint: String,
    mapped_ids: Vec<String>,
    unmapped_ids: Vec<String>,
    native_count: usize,
    /// live | answered | retired
    state: String,
    receipt_id: Option<String>,
    satisfied_ids: Vec<String>,
    results_hash: Option<String>,
    retired_reason: Option<String>,
    created_at: u64,
    resolved_at: Option<u64>,
}
fn mapping_value(inv: &Invocation) -> Value {
    json!({"mapped":true,"jobId":inv.job_id,"attemptId":inv.attempt_id,"stageId":inv.stage_id,"mappedIds":inv.mapped_ids,"unmappedIds":inv.unmapped_ids})
}
fn bound(question: &Question, native: &NativeQuestion) -> bool {
    question.prompt == native.question && question.multi == native.multi && question.options.len() == native.options.len()
        && question.options.iter().zip(&native.options).all(|(l, n)| l.label == n.label())
}

/// Projects a durable authenticated human answer for allowed model inspection, including after a conversation
/// replacement. Historical producer IDs bind the receipt; they need not equal the newly resumed producer.
pub(super) fn authenticated_answer(store: &super::store::Store, job_id: &str, question: &Question) -> Result<Option<Value>> {
    if question.state != "answered" { return Ok(None); }
    let receipt_id = question.answer_receipt_id.as_deref().ok_or_else(|| storage("Answered question has no receipt"))?;
    let receipt: Value = store.get(job_id, "receipts", receipt_id)?;
    if receipt["receiptId"] != receipt_id || receipt["jobId"] != job_id || receipt["kind"] != "humanAnswer" ||
        !matches!(receipt["provenance"].as_str(), Some("human" | "nativeHuman")) {
        return Err(storage("Question answer receipt has no authenticated human/job binding"));
    }
    match receipt["source"].as_str() {
        Some("mainWindow") if receipt["provenance"] == "human" && receipt["invocationId"].is_null() => {}
        Some("nativeAsk") => {
            let invocation_id = receipt["invocationId"].as_str().ok_or_else(|| storage("Native human receipt has no invocation"))?;
            let invocation: Invocation = store.get(job_id, "askInvocations", &inv_key(invocation_id))?;
            if invocation.job_id != job_id || invocation.invocation_id != invocation_id || invocation.state != "answered" ||
                invocation.receipt_id.as_deref() != Some(receipt_id) || !invocation.satisfied_ids.contains(&question.question_id) ||
                receipt["attemptId"] != invocation.attempt_id || receipt["stageId"] != invocation.stage_id ||
                receipt["producer"] != serde_json::to_value(&invocation.producer).map_err(storage)? {
                return Err(storage("Native human answer receipt does not match its settled invocation"));
            }
        }
        _ => return Err(storage("Question answer receipt has an untrusted source")),
    }
    let answers = receipt["answers"].as_array().ok_or_else(|| storage("Human receipt has no answers"))?;
    let mut matching = answers.iter().filter(|answer| answer["questionId"] == question.question_id);
    let answer = matching.next().ok_or_else(|| storage("Receipt does not answer this question"))?;
    if matching.next().is_some() { return Err(storage("Receipt duplicates this question")); }
    let parsed: Answer = serde_json::from_value(json!({"selectedOptions":answer["selectedOptions"],"customInput":answer["customInput"],"note":answer["note"]})).map_err(storage)?;
    let normalized = normalize_answer(&question.question_id, parsed.selected_options, parsed.custom_input, parsed.note)?;
    check_answer(question, &normalized).map_err(storage)?;
    Ok(Some(json!({"receiptId":receipt_id,"provenance":receipt["provenance"],"source":receipt["source"],"at":receipt["at"],
        "selectedOptions":normalized.selected_options,"customInput":normalized.custom_input,"note":normalized.note})))
}

/// Native pause to request after the gate is released: the hub re-enters the service for authorization.
struct PauseNotice { producer: DependencyProducer, job_id: String, attempt_id: String, stage_id: String }
enum Outcome {
    Done(Value),
    Live { producer: DependencyProducer, invocation: String, response: Value, question_id: String },
}

impl SrsService {
    /// Repository attribution comes from the persisted native-invocation mapping, never a model-supplied cwd.
    pub(crate) fn native_question_repository(&self, producer: &DependencyProducer, invocation_id: &str) -> Result<Option<String>> {
        let _guard = self.gate.lock();
        for job in self.bound_jobs(producer)? {
            if let Some(invocation) = self.invocation(&job.job_id, invocation_id)? {
                if self.is_current(&job, &invocation) { return Ok(Some(job.scope.repository_root.clone())); }
            }
        }
        Ok(None)
    }
    pub(super) fn ask_bridge(&self, producer: &DependencyProducer, action: &str, p: Value) -> Result<Value> {
        let mut notices = Vec::new();
        let result = {
            let _g = self.gate.lock();
            match action {
                "srs_ask_map" => parse(p).and_then(|r| self.ask_map(producer, r, &mut notices)),
                "srs_ask_receipt" => parse(p).and_then(|r| self.ask_receipt(producer, r, &mut notices)),
                "srs_ask_retired" => parse(p).and_then(|r| self.ask_retired(producer, r, &mut notices)),
                _ => Err(invalid("Unknown Ask action")),
            }
        };
        self.pause_native(notices);
        result
    }

    /// Human disposition of one ledger item. Live Ask answers are routed to the native HumanDecision authority.
    pub fn dispose_srs_item(&self, job_id: &str, expected_revision: u64, item_id: &str, disposition: &str, rationale: &str, answer: Option<Answer>) -> Result<Value> {
        store::safe_id(item_id)?;
        plain_text(rationale, "rationale", MAX_TEXT)?;
        let mut notices = Vec::new();
        let outcome = {
            let _g = self.gate.lock();
            let mut job = self.store.job(job_id)?;
            if job.revision != expected_revision {
                return Err(format!("revision_conflict: Job is at revision {}; review it before disposing an item", job.revision));
            }
            match (disposition, answer) {
                ("excludeFromScope", None) => self.exclude_locked(&mut job, item_id, rationale, &mut notices).map(Outcome::Done),
                ("provideHumanAnswer", Some(a)) => self.answer_locked(&mut job, item_id, rationale, a, &mut notices),
                ("excludeFromScope", Some(_)) => Err(invalid("excludeFromScope takes no answer")),
                ("provideHumanAnswer", None) => Err(invalid("provideHumanAnswer requires an answer")),
                _ => Err(invalid("Unknown disposition")),
            }
        };
        self.pause_native(notices);
        match outcome? {
            Outcome::Done(value) => Ok(value),
            Outcome::Live { producer, invocation, response, question_id } => {
                // The gate is released: the hub's reply path re-enters `ask_bridge` through the bridge receipt.
                let hub = self.registry().ok().and_then(|r| r.manager_hub().cloned())
                    .ok_or_else(|| precondition("The Manager bridge is unavailable; the live Ask cannot be answered"))?;
                hub.respond_srs_ask(&producer, &invocation, response).map_err(|e| precondition(format!("The native Ask did not accept the answer: {e}")))?;
                for _ in 0..60 {
                    std::thread::sleep(Duration::from_millis(50));
                    let _g = self.gate.lock();
                    let question: Question = self.store.get(job_id, "questions", &question_id)?;
                    if question.state != "asked" || !question.live { break; }
                }
                self.get_srs_job(job_id)
            }
        }
    }

    fn bound_jobs(&self, producer: &DependencyProducer) -> Result<Vec<Job>> {
        let mut jobs = Vec::new();
        self.store.jobs(|j| { if j.producer.as_ref() == Some(producer) { jobs.push(j); } Ok(()) })?;
        Ok(jobs)
    }
    fn invocation(&self, job_id: &str, id: &str) -> Result<Option<Invocation>> {
        let key = inv_key(id);
        if !self.store.path(job_id, "askInvocations", &key)?.exists() { return Ok(None); }
        let inv: Invocation = self.store.get(job_id, "askInvocations", &key)?;
        if inv.invocation_id != id || inv.job_id != job_id { return Err(storage("Ask invocation identity mismatch")); }
        Ok(Some(inv))
    }
    fn invocation_write(&self, inv: &Invocation) -> Result<Mutation> { store::mutation("askInvocations", &inv_key(&inv.invocation_id), inv) }
    /// The invocation still belongs to the exact producer/attempt/stage this job is bound to.
    fn is_current(&self, job: &Job, inv: &Invocation) -> bool {
        inv.state == "live" && job.attempt_id.as_deref() == Some(inv.attempt_id.as_str()) && job.producer.as_ref() == Some(&inv.producer)
            && job.stage.as_ref().is_some_and(|s| s.stage_id == inv.stage_id && s.attempt_id == inv.attempt_id)
    }
    fn live_invocation(&self, job: &Job, question: &Question) -> Result<Option<Invocation>> {
        if !question.live || question.state != "asked" { return Ok(None); }
        let Some(id) = question.invocation_id.as_deref() else { return Ok(None); };
        Ok(self.invocation(&job.job_id, id)?.filter(|inv| self.is_current(job, inv)))
    }
    /// A live-looking invocation that no longer belongs to the current attempt is retired with the dead mapping.
    fn retire_stale(&self, job: &Job, question: &Question, writes: &mut Vec<Mutation>) -> Result<()> {
        let Some(id) = question.invocation_id.as_deref() else { return Ok(()); };
        if let Some(mut inv) = self.invocation(&job.job_id, id)? {
            if inv.state == "live" && !self.is_current(job, &inv) {
                inv.state = "retired".into();
                inv.retired_reason = Some("retired".into());
                inv.resolved_at = Some(self.now());
                writes.push(self.invocation_write(&inv)?);
            }
        }
        Ok(())
    }

    fn bounded(job: &Job, writes: &[Mutation]) -> Result<()> {
        let size = serde_json::to_vec(&json!({"job":job,"writes":writes,"activity":{}})).map_err(storage)?.len() as u64;
        if size.saturating_add(4096) > MAX_RECORD { return Err(invalid("This change exceeds the 1 MiB transaction bound")); }
        Ok(())
    }
    /// Commit through the parent revision/event path. A persistence failure replays any pending redo log,
    /// then pauses the job (never a silent half-applied Ask state) and asks the native side to stop.
    fn commit(&self, job: &mut Job, writes: Vec<Mutation>, summary: &str, notices: &mut Vec<PauseNotice>) -> Result<()> {
        Self::bounded(job, &writes)?;
        let before = job.clone();
        match self.save(job, writes, summary) {
            Ok(()) => Ok(()),
            Err(error) => {
                *job = before;
                self.fail_closed(&job.job_id, &error, notices);
                Err(coded(error))
            }
        }
    }
    fn fail_closed(&self, job_id: &str, message: &str, notices: &mut Vec<PauseNotice>) {
        let _ = self.store.recover();
        let Ok(mut job) = self.store.job(job_id) else { return; };
        if !matches!(job.state, JobState::Running | JobState::Reviewing | JobState::WaitingForHuman) { return; }
        job.state = JobState::Paused;
        job.error = Some(Diagnostic { code: "storage_error".into(), message: message.chars().take(1024).collect() });
        if self.save(&mut job, Vec::new(), "Paused after Ask persistence failure").is_ok() {
            if let (Some(producer), Some(stage), Some(attempt)) = (job.producer.clone(), job.stage.clone(), job.attempt_id.clone()) {
                notices.push(PauseNotice { producer, job_id: job.job_id.clone(), attempt_id: attempt, stage_id: stage.stage_id });
            }
        } else {
            eprintln!("SRS: could not pause {job_id} after an Ask persistence failure");
        }
    }
    fn pause_native(&self, notices: Vec<PauseNotice>) {
        if notices.is_empty() { return; }
        let Some(hub) = self.registry().ok().and_then(|r| r.manager_hub().cloned()) else { return; };
        for n in notices {
            let hub = hub.clone();
            std::thread::spawn(move || {
                let _ = hub.srs_control(&n.producer, "srs_pause", json!({"jobId":n.job_id,"attemptId":n.attempt_id,"stageId":n.stage_id,"reason":"pause"}));
            });
        }
    }

    // ----- bridge: srs_ask_map -----
    fn validate_map(req: &MapRequest) -> Result<()> {
        ask_id(&req.invocation_id, "invocationId")?;
        if let Some(t) = &req.tool_call_id { ask_id(t, "toolCallId")?; }
        if req.questions.is_empty() || req.questions.len() > MAX_ASK_QUESTIONS { return Err(invalid("An Ask maps 1..50 questions")); }
        let mut ids = HashSet::new();
        for q in &req.questions {
            ask_id(&q.id, "question id")?;
            if !ids.insert(q.id.as_str()) { return Err(invalid("Duplicate native question ID")); }
            if q.question.len() > 65_536 || q.options.len() > 200 || q.options.iter().any(|o| o.label().len() > 4096) { return Err(invalid("Native question exceeds the supported bounds")); }
        }
        Ok(())
    }
    fn map_fingerprint(req: &MapRequest) -> Result<String> {
        Ok(store::hash(&serde_json::to_vec(&json!({
            "toolCallId":req.tool_call_id,
            "questions":req.questions.iter().map(|q| json!({"id":q.id,"question":q.question,"options":q.options.iter().map(|o| o.label()).collect::<Vec<_>>(),"multi":q.multi})).collect::<Vec<_>>(),
        })).map_err(storage)?))
    }
    /// An invocation ID maps once: an identical replay returns the stored mapping, anything else is rejected.
    fn replay_mapping(&self, jobs: &[Job], invocation_id: &str, fingerprint: &str) -> Result<Option<Value>> {
        for job in jobs {
            let Some(inv) = self.invocation(&job.job_id, invocation_id)? else { continue; };
            if inv.fingerprint != fingerprint { return Err(precondition("The invocation mapping is immutable and this request differs from it")); }
            if job.attempt_id.as_deref() != Some(inv.attempt_id.as_str()) || job.stage.as_ref().map(|s| s.stage_id.as_str()) != Some(inv.stage_id.as_str()) {
                return Err("stale_stage: The invocation belongs to an earlier attempt or stage".into());
            }
            return Ok(Some(mapping_value(&inv)));
        }
        Ok(None)
    }
    fn accepts_ask(job: &Job) -> bool {
        matches!(job.state, JobState::Running | JobState::Reviewing)
            && job.stage.as_ref().is_some_and(|s| Some(&s.attempt_id) == job.attempt_id.as_ref() && matches!(s.state.as_str(), "queued" | "active"))
    }
    /// The durable question a native question is bound to, when it may be asked now.
    fn map_candidate(&self, job: &Job, native: &NativeQuestion, retired: &mut Vec<Invocation>) -> Result<Option<Question>> {
        if store::safe_id(&native.id).is_err() || !self.store.path(&job.job_id, "questions", &native.id)?.exists() { return Ok(None); }
        let q: Question = self.store.get(&job.job_id, "questions", &native.id)?;
        Ok(if bound(&q, native) && self.ask_eligible(job, &q, retired)? { Some(q) } else { None })
    }
    fn map_questions(&self, job: &Job, req: &MapRequest, retired: &mut Vec<Invocation>) -> Result<(Vec<String>, Vec<String>, Vec<Mutation>)> {
        let (mut mapped, mut unmapped, mut writes) = (Vec::new(), Vec::new(), Vec::new());
        for native in &req.questions {
            let Some(mut q) = self.map_candidate(job, native, retired)? else { unmapped.push(native.id.clone()); continue; };
            q.state = "asked".into();
            q.live = true;
            q.invocation_id = Some(req.invocation_id.clone());
            writes.push(store::mutation("questions", &q.question_id, &q)?);
            mapped.push(native.id.clone());
        }
        Ok((mapped, unmapped, writes))
    }
    fn ask_map(&self, producer: &DependencyProducer, req: MapRequest, notices: &mut Vec<PauseNotice>) -> Result<Value> {
        Self::validate_map(&req)?;
        let fingerprint = Self::map_fingerprint(&req)?;
        let jobs = self.bound_jobs(producer)?;
        if let Some(mapping) = self.replay_mapping(&jobs, &req.invocation_id, &fingerprint)? { return Ok(mapping); }
        let all_ids: Vec<&str> = req.questions.iter().map(|q| q.id.as_str()).collect();
        let Some(mut job) = jobs.into_iter().find(Self::accepts_ask) else {
            return Ok(json!({"mapped":false,"mappedIds":[],"unmappedIds":all_ids}));
        };
        let stage_id = job.stage.as_ref().map(|s| s.stage_id.clone()).ok_or_else(|| "stale_stage: No current stage".to_string())?;
        let attempt_id = job.attempt_id.clone().ok_or_else(|| "stale_attempt: No bound attempt".to_string())?;
        let mut retired: Vec<Invocation> = Vec::new();
        let (mapped, unmapped, question_writes) = self.map_questions(&job, &req, &mut retired)?;
        if mapped.is_empty() { return Ok(json!({"mapped":false,"mappedIds":[],"unmappedIds":unmapped})); }
        let inv = Invocation {
            invocation_id: req.invocation_id, tool_call_id: req.tool_call_id, job_id: job.job_id.clone(), attempt_id, stage_id, producer: producer.clone(),
            fingerprint, mapped_ids: mapped, unmapped_ids: unmapped, native_count: req.questions.len(), state: "live".into(),
            receipt_id: None, satisfied_ids: Vec::new(), results_hash: None, retired_reason: None, created_at: self.now(), resolved_at: None,
        };
        let mut writes = vec![self.invocation_write(&inv)?];
        for r in &retired { writes.push(self.invocation_write(r)?); }
        writes.extend(question_writes);
        self.commit(&mut job, writes, "Native Ask mapped to ledger questions", notices)?;
        Ok(mapping_value(&inv))
    }
    /// A question may be asked when open, or when its previous invocation is dead (stale attempt/stage or retired).
    fn ask_eligible(&self, job: &Job, q: &Question, retired: &mut Vec<Invocation>) -> Result<bool> {
        match q.state.as_str() {
            "open" => Ok(true),
            "asked" => {
                let Some(old) = q.invocation_id.as_deref() else { return Ok(true); };
                match self.invocation(&job.job_id, old)? {
                    Some(inv) if inv.state == "live" => {
                        if self.is_current(job, &inv) { return Ok(false); }
                        if !retired.iter().any(|r| r.invocation_id == inv.invocation_id) {
                            let mut r = inv;
                            r.state = "retired".into();
                            r.retired_reason = Some("retired".into());
                            r.resolved_at = Some(self.now());
                            retired.push(r);
                        }
                        Ok(true)
                    }
                    _ => Ok(true),
                }
            }
            _ => Ok(false),
        }
    }

    // ----- bridge: srs_ask_receipt -----
    fn normalize_results(results: Vec<AskResult>) -> Result<Vec<Norm>> {
        let mut norm = Vec::new();
        for r in results { ask_id(&r.id, "result id")?; norm.push(normalize_answer(&r.id, r.selected_options, r.custom_input, r.note)?); }
        norm.sort_by(|a, b| a.id.cmp(&b.id));
        if norm.windows(2).any(|w| w[0].id == w[1].id) { return Err(invalid("Duplicate result ID")); }
        Ok(norm)
    }
    fn find_invocation(&self, jobs: Vec<Job>, id: &str) -> Result<Option<(Job, Invocation)>> {
        for job in jobs {
            if let Some(inv) = self.invocation(&job.job_id, id)? { return Ok(Some((job, inv))); }
        }
        Ok(None)
    }
    /// A settled invocation replays only its identical receipt; otherwise it must still be live for this job.
    fn check_receipt_target(&self, job: &Job, inv: &Invocation, digest: &str) -> Result<Option<Value>> {
        match inv.state.as_str() {
            "answered" => {
                if inv.results_hash.as_deref() == Some(digest) { return Ok(Some(json!({"receiptId":inv.receipt_id,"satisfiedIds":inv.satisfied_ids}))); }
                return Err(precondition("This invocation already has a different durable receipt"));
            }
            "retired" => return Err(precondition("This invocation was retired; its answer cannot be recorded")),
            _ => {}
        }
        if !matches!(job.state, JobState::Running | JobState::Reviewing | JobState::WaitingForHuman) {
            return Err(precondition(format!("A {:?} job does not accept Ask receipts", job.state)));
        }
        if !self.is_current(job, inv) { return Err("stale_attempt: The invocation no longer belongs to this producer, attempt and stage".into()); }
        Ok(None)
    }
    /// Marks each answered result's question answered; returns the question writes, satisfied IDs and receipt answers.
    fn answer_mapped(&self, job: &mut Job, inv: &Invocation, norm: &[Norm], receipt_id: &str) -> Result<(Vec<Mutation>, Vec<String>, Vec<Value>)> {
        let (mut writes, mut satisfied, mut answers) = (Vec::new(), Vec::new(), Vec::new());
        for n in norm {
            if inv.unmapped_ids.contains(&n.id) { continue; }
            if !inv.mapped_ids.contains(&n.id) { return Err(invalid(format!("Result {} does not belong to this invocation", n.id))); }
            let mut q: Question = self.store.get(&job.job_id, "questions", &n.id)?;
            if q.state != "asked" || !q.live || q.invocation_id.as_deref() != Some(inv.invocation_id.as_str()) {
                return Err(precondition(format!("Question {} is not awaiting this invocation", n.id)));
            }
            check_answer(&q, n)?;
            q.state = "answered".into();
            q.answer_receipt_id = Some(receipt_id.to_string());
            q.live = false;
            q.invocation_id = None;
            dec(&mut job.counts.questions_open, "open questions")?;
            writes.push(store::mutation("questions", &q.question_id, &q)?);
            satisfied.push(q.question_id.clone());
            answers.push(answer_value(n));
        }
        if satisfied.is_empty() { return Err(invalid("The receipt answers no mapped ledger question")); }
        Ok((writes, satisfied, answers))
    }
    /// Reopens this invocation's still-asked questions that are not in `skip`.
    fn release_asked(&self, job_id: &str, inv: &Invocation, skip: &[String], writes: &mut Vec<Mutation>) -> Result<()> {
        for id in inv.mapped_ids.iter().filter(|id| !skip.contains(id)) {
            let mut q: Question = self.store.get(job_id, "questions", id)?;
            if q.state == "asked" && q.invocation_id.as_deref() == Some(inv.invocation_id.as_str()) {
                q.state = "open".into();
                q.live = false;
                q.invocation_id = None;
                writes.push(store::mutation("questions", &q.question_id, &q)?);
            }
        }
        Ok(())
    }
    fn ask_receipt(&self, producer: &DependencyProducer, req: ReceiptRequest, notices: &mut Vec<PauseNotice>) -> Result<Value> {
        ask_id(&req.invocation_id, "invocationId")?;
        if req.results.is_empty() || req.results.len() > MAX_ASK_QUESTIONS { return Err(invalid("A receipt carries 1..50 results")); }
        let norm = Self::normalize_results(req.results)?;
        let digest = store::hash(&serde_json::to_vec(&json!({"provenance":req.provenance.as_str(),"answers":norm})).map_err(storage)?);
        let jobs = self.bound_jobs(producer)?;
        if jobs.is_empty() { return Err("stale_attempt: No SRS job is bound to this Architect producer".into()); }
        let Some((mut job, mut inv)) = self.find_invocation(jobs, &req.invocation_id)? else { return Err(not_found("Unknown or unmapped Ask invocation")); };
        if let Some(replay) = self.check_receipt_target(&job, &inv, &digest)? { return Ok(replay); }
        let receipt_id = store::id();
        let (mut question_writes, satisfied, answers) = self.answer_mapped(&mut job, &inv, &norm, &receipt_id)?;
        self.release_asked(&job.job_id, &inv, &satisfied, &mut question_writes)?;
        let at = self.now();
        let receipt = json!({
            "receiptId":receipt_id,"kind":"humanAnswer","provenance":req.provenance.as_str(),"source":"nativeAsk","jobId":job.job_id,
            "attemptId":inv.attempt_id,"stageId":inv.stage_id,"producer":inv.producer,"invocationId":inv.invocation_id,"toolCallId":inv.tool_call_id,
            "answers":answers,"at":at,
        });
        inv.state = "answered".into();
        inv.receipt_id = Some(receipt_id.clone());
        inv.satisfied_ids = satisfied.clone();
        inv.results_hash = Some(digest);
        inv.resolved_at = Some(at);
        // Receipt and invocation precede the question flips so an early storage failure applies nothing.
        let mut writes = vec![store::mutation("receipts", &receipt_id, &receipt)?, self.invocation_write(&inv)?];
        writes.extend(question_writes);
        settle(&mut job);
        self.commit(&mut job, writes, "Human answer receipt recorded", notices)?;
        Ok(json!({"receiptId":receipt_id,"satisfiedIds":satisfied}))
    }

    // ----- bridge: srs_ask_retired -----
    fn ask_retired(&self, producer: &DependencyProducer, req: RetiredRequest, notices: &mut Vec<PauseNotice>) -> Result<Value> {
        ask_id(&req.invocation_id, "invocationId")?;
        for mut job in self.bound_jobs(producer)? {
            let Some(mut inv) = self.invocation(&job.job_id, &req.invocation_id)? else { continue; };
            // Answered and already-retired invocations are final; retirement never undoes an answer.
            if inv.state != "live" { return Ok(json!({"ok":true})); }
            let mut writes = Vec::new();
            self.release_asked(&job.job_id, &inv, &[], &mut writes)?;
            inv.state = "retired".into();
            inv.retired_reason = Some(req.reason.as_str().into());
            inv.resolved_at = Some(self.now());
            writes.insert(0, self.invocation_write(&inv)?);
            self.commit(&mut job, writes, "Native Ask retired; ledger questions remain open", notices)?;
            return Ok(json!({"ok":true}));
        }
        Ok(json!({"ok":true}))
    }

    // ----- human: provideHumanAnswer -----
    fn answer_locked(&self, job: &mut Job, item: &str, rationale: &str, answer: Answer, notices: &mut Vec<PauseNotice>) -> Result<Outcome> {
        if !self.store.path(&job.job_id, "questions", item)?.exists() { return Err(not_found("provideHumanAnswer requires a ledger question ID")); }
        let mut q: Question = self.store.get(&job.job_id, "questions", item)?;
        if !matches!(q.state.as_str(), "open" | "asked") { return Err(precondition(format!("Question is already {}; answers are immutable", q.state))); }
        let norm = normalize_answer(&q.question_id, answer.selected_options, answer.custom_input, answer.note)?;
        check_answer(&q, &norm)?;
        if let Some(inv) = self.live_invocation(job, &q)? {
            if !matches!(job.state, JobState::Running | JobState::Reviewing | JobState::WaitingForHuman | JobState::Paused) {
                return Err(precondition(format!("A {:?} job cannot answer a live Ask", job.state)));
            }
            if inv.native_count > 1 {
                return Err(precondition("This native Ask carries several questions; answer it from the Decisions card"));
            }
            let response = json!({"kind":"submit","results":[serde_json::to_value(&norm).map_err(storage)?]});
            return Ok(Outcome::Live { producer: inv.producer, invocation: inv.invocation_id, response, question_id: q.question_id });
        }
        if !matches!(job.state, JobState::Paused | JobState::WaitingForHuman) {
            return Err(precondition("Pause the job before answering outside a live Ask"));
        }
        let mut writes = Vec::new();
        self.retire_stale(job, &q, &mut writes)?;
        let receipt_id = store::id();
        let receipt = json!({
            "receiptId":receipt_id,"kind":"humanAnswer","provenance":"human","source":"mainWindow","jobId":job.job_id,"attemptId":job.attempt_id,
            "stageId":job.stage.as_ref().map(|s| &s.stage_id),"producer":job.producer,"invocationId":null,"toolCallId":null,
            "answers":[answer_value(&norm)],"rationale":rationale,"at":self.now(),
        });
        q.state = "answered".into();
        q.answer_receipt_id = Some(receipt_id.clone());
        q.live = false;
        q.invocation_id = None;
        dec(&mut job.counts.questions_open, "open questions")?;
        writes.insert(0, store::mutation("receipts", &receipt_id, &receipt)?);
        writes.push(store::mutation("questions", &q.question_id, &q)?);
        settle(job);
        self.commit(job, writes, "Human answered question", notices)?;
        Ok(Outcome::Done(self.status_value(job)?))
    }

    // ----- human: excludeFromScope -----
    /// Review validity is checked lazily against each reviewed work revision; a human disposition never rewrites
    /// an unbounded fan-out of dependent documents in one transport-sized transaction.
    fn revise(&self, job: &mut Job, affected: &HashSet<String>, reopen_reviewed: bool, writes: &mut Vec<Mutation>) -> Result<()> {
        if reopen_reviewed {
            for id in affected {
                if !self.store.path(&job.job_id, "work", id)?.exists() { continue; }
                let mut work: Work = self.store.get(&job.job_id, "work", id)?;
                if work.status == "reviewed" {
                    work.status = "pending".into();
                    inc(&mut job.counts.work_open, "open work")?;
                }
                work.revision = job.work_revision;
                writes.push(store::mutation("work", id, &work)?);
            }
        }
        Ok(())
    }

    fn exclude_question(&self, job: &mut Job, item: &str, record: &mut Value, writes: &mut Vec<Mutation>) -> Result<(String, String)> {
        let job_id = job.job_id.clone();
        let mut q: Question = self.store.get(&job_id, "questions", item)?;
        if !matches!(q.state.as_str(), "open" | "asked") { return Err(precondition(format!("Question is already {}", q.state))); }
        if self.live_invocation(job, &q)?.is_some() {
            return Err(precondition("A live native Ask holds this question; answer it from the Decisions card or wait for its retirement"));
        }
        self.retire_stale(job, &q, writes)?;
        let mut affected = HashSet::new();
        self.store.related(&job_id, &q.question_id, "work", |id| { affected.insert(id.to_string()); Ok(()) })?;
        record["kind"] = json!("question");
        record["previousState"] = json!(q.state);
        record["workIds"] = json!(affected.iter().collect::<Vec<_>>());
        q.state = "excluded".into();
        q.live = false;
        q.invocation_id = None;
        q.answer_receipt_id = None;
        dec(&mut job.counts.questions_open, "open questions")?;
        if !affected.is_empty() { inc(&mut job.work_revision, "work revision")?; }
        self.revise(job, &affected, true, writes)?;
        writes.push(store::mutation("questions", &q.question_id, &q)?);
        Ok((q.question_id.clone(), format!("Human excluded question {}", q.question_id)))
    }
    /// The durable work an excluded item maps to: the work item itself, or the work allocated for source evidence.
    fn exclusion_target(&self, job_id: &str, item: &str) -> Result<(String, &'static str, Option<inventory::InventoryItem>)> {
        if self.store.path(job_id, "work", item)?.exists() { return Ok((item.to_string(), "work", None)); }
        let dir = self.store.dir(job_id)?;
        let ev = inventory::read_item(&dir, item).map_err(|_| not_found("No question, work or evidence item has this ID"))?;
        if ev.state == "excluded" { return Err(precondition("This evidence is already excluded from the recorded scope")); }
        Ok((format!("source-{item}"), "evidence", Some(ev)))
    }
    fn exclude_work(&self, job: &mut Job, item: &str, rationale: &str, record: &mut Value, writes: &mut Vec<Mutation>) -> Result<(String, String)> {
        let job_id = job.job_id.clone();
        let (work_id, kind, evidence) = self.exclusion_target(&job_id, item)?;
        if !self.store.path(&job_id, "work", &work_id)?.exists() { return Err(not_found("This item has no durable work to exclude")); }
        let mut work: Work = self.store.get(&job_id, "work", &work_id)?;
        if work.status == "excluded" { return Err(precondition("This work is already excluded by a human")); }
        if work.kind == "templateSection" { return Err(precondition("Template sections cannot be excluded; review them or record evidence-backed notApplicable")); }
        record["kind"] = json!(kind);
        record["workId"] = json!(work_id);
        record["previousStatus"] = json!(work.status);
        record["previousRationale"] = json!(work.rationale);
        if let Some(ev) = &evidence { record["evidence"] = json!({"id":ev.id,"path":ev.path,"kind":ev.kind,"state":ev.state,"sha256":ev.sha256}); }
        if is_open(&work.status) { dec(&mut job.counts.work_open, "open work")?; }
        if work.status == "blocked" { dec(&mut job.counts.blocked, "blocked work")?; }
        inc(&mut job.counts.excluded, "excluded")?;
        inc(&mut job.work_revision, "work revision")?;
        work.status = "excluded".into();
        work.rationale = format!("Human exclusion: {rationale}");
        work.revision = job.work_revision;
        let affected = HashSet::from([work_id.clone()]);
        self.revise(job, &affected, false, writes)?;
        writes.push(store::mutation("work", &work_id, &work)?);
        Ok((work_id, format!("Human excluded {kind} {item}")))
    }
    fn exclude_locked(&self, job: &mut Job, item: &str, rationale: &str, notices: &mut Vec<PauseNotice>) -> Result<Value> {
        if !matches!(job.state, JobState::Paused | JobState::WaitingForHuman) { return Err(precondition("Human exclusions require a paused or waiting job")); }
        let job_id = job.job_id.clone();
        let disposition_id = store::id();
        let at = self.now();
        let mut writes = Vec::new();
        let mut record = json!({"id":disposition_id,"jobId":job_id,"itemId":item,"disposition":"excludeFromScope","rationale":rationale,"at":at,"actor":"human","revision":job.revision.saturating_add(1)});
        let (owner, summary) = if self.store.path(&job_id, "questions", item)?.exists() {
            self.exclude_question(job, item, &mut record, &mut writes)?
        } else {
            self.exclude_work(job, item, rationale, &mut record, &mut writes)?
        };
        writes.insert(0, store::mutation("dispositions", &disposition_id, &record)?);
        if let Some(relation) = self.store.relation(&job_id, &owner, "humanDisposition", &disposition_id)? { writes.insert(1, relation); }
        inc(&mut job.human_exclusions, "human exclusions")?;
        job.completion = None;
        settle(job);
        self.commit(job, writes, &summary, notices)?;
        self.status_value(job)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    struct TickClock(AtomicU64);
    impl Clock for TickClock { fn now(&self) -> u64 { self.0.fetch_add(1, Ordering::SeqCst) + 1000 } }

    fn producer(generation: u64) -> DependencyProducer {
        DependencyProducer { instance_id: "architect".into(), generation, launch_id: store::hash(b"launch"), session_id: "session".into() }
    }
    struct Fx { svc: Arc<SrsService>, root: PathBuf, job_id: String, producer: DependencyProducer }
    impl Drop for Fx { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.root); } }

    /// `(id, required, multi, options)` ledger questions; prompts are `Prompt <id>`.
    type Spec<'a> = (&'a str, bool, bool, &'a [&'a str]);
    impl Fx {
        fn new(state: &str, questions: &[Spec<'_>], blocked: u64) -> Self {
            let root = std::env::temp_dir().join(format!("omp-srs-questions-{}", store::id()));
            std::fs::create_dir_all(&root).unwrap();
            let svc = SrsService::with_clock(root.clone(), KnowledgeService::new(root.clone(), root.join("home")), Arc::new(TickClock(AtomicU64::new(0))));
            let producer = producer(1);
            let job_id = store::id();
            let attempt = store::id();
            let job: Job = serde_json::from_value(json!({
                "jobId":job_id,"scope":{"folder":"fixture","repositoryRoot":"fixture","vaultId":store::id(),"vaultRoot":"fixture","vaultMode":"markdown","roots":[],"exclusions":[],"template":{}},
                "revision":1,"state":state,"phase":"survey","scopeHash":"fixture","sourceHash":null,"attemptId":attempt,"producer":producer,
                "stage":{"stageId":store::id(),"number":1,"phase":"survey","state":"active","attemptId":attempt,"nonce":store::id(),"startedAt":1,"endedAt":null,"summary":null,"admitted":true},
                "nextStage":2,"nextRequirement":0,
                "counts":{"areas":0,"evidenceFiles":0,"work":0,"workOpen":0,"documents":0,"requirements":0,"questions":questions.len(),"questionsOpen":questions.len(),"blocked":blocked,"excluded":0},
                "checkpoint":null,"error":null,"createdAt":1,"updatedAt":1,"completion":null,"inventoryComplete":true,"humanExclusions":0,"documentRevision":0,"workRevision":0
            })).unwrap();
            let mut writes = Vec::new();
            for (id, required, multi, options) in questions {
                let q = Question { question_id: (*id).into(), prompt: format!("Prompt {id}"), options: options.iter().map(|l| QuestionOption { label: (*l).into(), description: None }).collect(), multi: *multi, required: *required, state: "open".into(), answer_receipt_id: None, live: false, invocation_id: None };
                writes.push(store::mutation("questions", id, &q).unwrap());
            }
            svc.store.commit(&job, writes, json!({"fixture":true})).unwrap();
            Self { svc, root, job_id, producer }
        }
        fn job(&self) -> Job { self.svc.store.job(&self.job_id).unwrap() }
        fn question(&self, id: &str) -> Question { self.svc.store.get(&self.job_id, "questions", id).unwrap() }
        fn map(&self, invocation: &str, ids: &[&str]) -> Result<Value> {
            let questions: Vec<Value> = ids.iter().map(|id| {
                let q = self.question(id);
                json!({"id":id,"question":q.prompt,"options":q.options.iter().map(|o| o.label.clone()).collect::<Vec<_>>(),"multi":q.multi})
            }).collect();
            self.svc.ask_bridge(&self.producer, "srs_ask_map", json!({"invocationId":invocation,"toolCallId":"call-1","questions":questions}))
        }
        fn receipt(&self, invocation: &str, results: Value) -> Result<Value> {
            self.svc.ask_bridge(&self.producer, "srs_ask_receipt", json!({"invocationId":invocation,"provenance":"human","results":results}))
        }
        fn retire(&self, invocation: &str, reason: &str) -> Result<Value> {
            self.svc.ask_bridge(&self.producer, "srs_ask_retired", json!({"invocationId":invocation,"reason":reason}))
        }
        fn add(&self, writes: Vec<Mutation>) {
            let mut job = self.job();
            job.revision += 1;
            self.svc.store.commit(&job, writes, json!({"fixture":true})).unwrap();
        }
    }
    const YN: &[&str] = &["Yes", "No"];
    const AB: &[&str] = &["A", "B"];

    #[test]
    fn receipt_is_atomic_idempotent_and_counts_once() {
        let fx = Fx::new("running", &[("q1", true, false, YN), ("q2", false, true, AB)], 0);
        let mapped = fx.map("inv-1", &["q1", "q2"]).unwrap();
        assert_eq!(mapped["mapped"], true);
        assert_eq!(mapped["mappedIds"].as_array().unwrap().len(), 2);
        assert_eq!(fx.map("inv-1", &["q1", "q2"]).unwrap(), mapped);
        assert!(fx.question("q1").live && fx.question("q1").state == "asked");
        assert_eq!(fx.job().counts.questions_open, 2);
        let results = json!([{"id":"q1","selectedOptions":["Yes"]},{"id":"q2","selectedOptions":[],"note":"skip"}]);
        let first = fx.receipt("inv-1", results.clone()).unwrap();
        assert_eq!(first["satisfiedIds"].as_array().unwrap().len(), 2);
        let revision = fx.job().revision;
        assert_eq!(fx.job().counts.questions_open, 0);
        assert_eq!(fx.receipt("inv-1", results).unwrap(), first);
        assert_eq!(fx.job().revision, revision, "replay must not write");
        assert_eq!(fx.job().counts.questions_open, 0);
        let q1 = fx.question("q1");
        assert_eq!(q1.state, "answered");
        assert!(!q1.live && q1.invocation_id.is_none());
        assert_eq!(q1.answer_receipt_id.as_deref(), first["receiptId"].as_str());
        let receipt: Value = fx.svc.store.get(&fx.job_id, "receipts", first["receiptId"].as_str().unwrap()).unwrap();
        assert_eq!(receipt["provenance"], "human");
        assert_eq!(receipt["producer"]["sessionId"], "session");
        assert_eq!(receipt["stageId"], fx.job().stage.unwrap().stage_id);
        let err = fx.receipt("inv-1", json!([{"id":"q1","selectedOptions":["No"]}])).unwrap_err();
        assert!(err.starts_with("precondition_failed"), "{err}");
        assert_eq!(fx.job().counts.questions_open, 0);
    }

    #[test]
    fn strict_validation_changes_nothing() {
        let fx = Fx::new("running", &[("q1", true, false, YN), ("q2", false, true, AB)], 0);
        for bad in [json!({"invocationId":"i","questions":[],"extra":1}), json!({"invocationId":"i","questions":[{"id":"q1","question":"Prompt q1","options":[],"multi":false,"x":1}]}), json!({"invocationId":"i","questions":[]})] {
            assert!(fx.svc.ask_bridge(&fx.producer, "srs_ask_map", bad).unwrap_err().starts_with("invalid_request"));
        }
        // Prompt, options and multi are bound to the durable ledger question.
        let wrong = fx.svc.ask_bridge(&fx.producer, "srs_ask_map", json!({"invocationId":"i","questions":[{"id":"q1","question":"Prompt q1","options":[{"label":"Yes"},{"label":"Maybe"}],"multi":false}]})).unwrap();
        assert_eq!(wrong["mapped"], false);
        let wrong = fx.svc.ask_bridge(&fx.producer, "srs_ask_map", json!({"invocationId":"i","questions":[{"id":"q1","question":"Prompt q1","options":["Yes","No"],"multi":true},{"id":"unknown","question":"x"}]})).unwrap();
        assert_eq!(wrong["mapped"], false);
        assert_eq!(wrong["unmappedIds"].as_array().unwrap().len(), 2);
        assert!(fx.question("q1").state == "open");
        let mapped = fx.svc.ask_bridge(&fx.producer, "srs_ask_map", json!({"invocationId":"inv","questions":[{"id":"q1","question":"Prompt q1","options":[{"label":"Yes"},"No"],"multi":false},{"id":"q2","question":"Prompt q2","options":["A","B"],"multi":true}]})).unwrap();
        assert_eq!(mapped["mappedIds"].as_array().unwrap().len(), 2);
        let revision = fx.job().revision;
        for bad in [
            json!({"invocationId":"inv","provenance":"manager","results":[{"id":"q1","selectedOptions":["Yes"]}]}),
            json!({"invocationId":"inv","provenance":"human","results":[{"id":"q1","selectedOptions":["Yes"],"x":1}]}),
            json!({"invocationId":"inv","provenance":"human","extra":1,"results":[{"id":"q1","selectedOptions":["Yes"]}]}),
        ] {
            assert!(fx.svc.ask_bridge(&fx.producer, "srs_ask_receipt", bad).unwrap_err().starts_with("invalid_request"));
        }
        for results in [
            json!([{"id":"q1","selectedOptions":[]}]),
            json!([{"id":"q1","selectedOptions":[],"customInput":"   "}]),
            json!([{"id":"q1","selectedOptions":["Maybe"]}]),
            json!([{"id":"q1","selectedOptions":["Yes","No"]}]),
            json!([{"id":"q1","selectedOptions":["Yes"]},{"id":"q1","selectedOptions":["No"]}]),
            json!([{"id":"q2","selectedOptions":["A","A"]}]),
            json!([{"id":"stranger","selectedOptions":[]}]),
            json!([{"id":"q2","selectedOptions":[]}, {"id":"q1","selectedOptions":["Yes"],"customInput":"a\u{0}b"}]),
        ] {
            let err = fx.receipt("inv", results.clone()).unwrap_err();
            assert!(err.starts_with("invalid_request"), "{results}: {err}");
        }
        assert_eq!(fx.job().revision, revision);
        assert_eq!(fx.job().counts.questions_open, 2);
        assert!(fx.question("q1").live);
        // Optional empty is accepted; custom input satisfies a required question.
        let ok = fx.receipt("inv", json!([{"id":"q1","selectedOptions":[],"customInput":"Other"},{"id":"q2","selectedOptions":["A","B"]}])).unwrap();
        assert_eq!(ok["satisfiedIds"].as_array().unwrap().len(), 2);
        let native = fx.svc.ask_bridge(&fx.producer, "srs_ask_receipt", json!({"invocationId":"inv","provenance":"nativeHuman","results":[{"id":"q1","selectedOptions":[],"customInput":"Other"},{"id":"q2","selectedOptions":["A","B"]}]})).unwrap_err();
        assert!(native.starts_with("precondition_failed"), "a different provenance is a different receipt: {native}");
    }

    #[test]
    fn native_human_receipt_and_unmapped_results() {
        let fx = Fx::new("reviewing", &[("q1", true, false, YN)], 0);
        let mapped = fx.svc.ask_bridge(&fx.producer, "srs_ask_map", json!({"invocationId":"n","questions":[{"id":"q1","question":"Prompt q1","options":["Yes","No"],"multi":false},{"id":"free","question":"Free?"}]})).unwrap();
        assert_eq!(mapped["unmappedIds"], json!(["free"]));
        let receipt = fx.svc.ask_bridge(&fx.producer, "srs_ask_receipt", json!({"invocationId":"n","provenance":"nativeHuman","results":[{"id":"free","selectedOptions":[],"customInput":"x"},{"id":"q1","selectedOptions":["No"]}]})).unwrap();
        assert_eq!(receipt["satisfiedIds"], json!(["q1"]));
        let stored: Value = fx.svc.store.get(&fx.job_id, "receipts", receipt["receiptId"].as_str().unwrap()).unwrap();
        assert_eq!(stored["provenance"], "nativeHuman");
        assert_eq!(stored["answers"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn stale_producer_unknown_invocation_and_inactive_jobs() {
        let fx = Fx::new("running", &[("q1", true, false, YN)], 0);
        let stale = producer(2);
        let q = json!({"invocationId":"s","questions":[{"id":"q1","question":"Prompt q1","options":["Yes","No"],"multi":false}]});
        let unmapped = fx.svc.ask_bridge(&stale, "srs_ask_map", q).unwrap();
        assert_eq!(unmapped["mapped"], false);
        assert_eq!(fx.question("q1").state, "open");
        let err = fx.svc.ask_bridge(&stale, "srs_ask_receipt", json!({"invocationId":"s","provenance":"human","results":[{"id":"q1","selectedOptions":["Yes"]}]})).unwrap_err();
        assert!(err.starts_with("stale_attempt"), "{err}");
        let err = fx.receipt("never-mapped", json!([{"id":"q1","selectedOptions":["Yes"]}])).unwrap_err();
        assert!(err.starts_with("not_found"), "{err}");
        assert_eq!(fx.retire("never-mapped", "failed").unwrap(), json!({"ok":true}));
        assert_eq!(fx.svc.ask_bridge(&fx.producer, "srs_unknown", json!({})).unwrap_err().split(':').next(), Some("invalid_request"));
        // Legitimate invocation: a producer that later becomes stale cannot answer or retire it.
        fx.map("live", &["q1"]).unwrap();
        let err = fx.svc.ask_bridge(&stale, "srs_ask_receipt", json!({"invocationId":"live","provenance":"human","results":[{"id":"q1","selectedOptions":["Yes"]}]})).unwrap_err();
        assert!(err.starts_with("stale_attempt"), "{err}");
        fx.svc.ask_bridge(&stale, "srs_ask_retired", json!({"invocationId":"live","reason":"cancelled"})).unwrap();
        assert!(fx.question("q1").live, "a stale producer cannot retire another producer's invocation");
        // A paused job does not accept new maps or receipts.
        let mut job = fx.job();
        job.state = JobState::Paused;
        job.revision += 1;
        fx.svc.store.commit(&job, vec![], json!({})).unwrap();
        assert!(fx.receipt("live", json!([{"id":"q1","selectedOptions":["Yes"]}])).unwrap_err().starts_with("precondition_failed"));
        assert_eq!(fx.map("later", &["q1"]).unwrap()["mapped"], false);
        // But a waiting job still accepts the active invocation's receipt.
        let mut job = fx.job();
        job.state = JobState::WaitingForHuman;
        job.revision += 1;
        fx.svc.store.commit(&job, vec![], json!({})).unwrap();
        fx.receipt("live", json!([{"id":"q1","selectedOptions":["Yes"]}])).unwrap();
        let job = fx.job();
        assert_eq!(job.state, JobState::Paused, "answering the last open question unsticks waiting jobs to an explicit Resume");
        assert_eq!(job.counts.questions_open, 0);
    }

    #[test]
    fn retirement_reopens_but_never_answered() {
        let fx = Fx::new("running", &[("q1", true, false, YN), ("q2", true, false, YN)], 0);
        fx.map("one", &["q1", "q2"]).unwrap();
        let open_before = fx.job().counts.questions_open;
        for reason in ["cancelled", "chat", "aborted", "timeout", "retired", "failed"] {
            fx.map(reason, &["q1"]).ok();
        }
        assert_eq!(fx.retire("one", "timeout").unwrap(), json!({"ok":true}));
        assert_eq!(fx.job().counts.questions_open, open_before);
        for id in ["q1", "q2"] { let q = fx.question(id); assert!(q.state == "open" && !q.live && q.invocation_id.is_none(), "{id}"); }
        assert!(fx.receipt("one", json!([{"id":"q1","selectedOptions":["Yes"]}])).unwrap_err().starts_with("precondition_failed"));
        assert_eq!(fx.retire("one", "failed").unwrap(), json!({"ok":true}));
        assert_eq!(fx.map("two", &["q1"]).unwrap()["mapped"], true);
        fx.receipt("two", json!([{"id":"q1","selectedOptions":["Yes"]}])).unwrap();
        assert_eq!(fx.retire("two", "aborted").unwrap(), json!({"ok":true}));
        let q1 = fx.question("q1");
        assert_eq!(q1.state, "answered");
        assert!(q1.answer_receipt_id.is_some());
        assert_eq!(fx.job().counts.questions_open, 1);
        assert_eq!(fx.map("three", &["q1"]).unwrap()["mapped"], false, "answered questions are never asked again");
    }

    #[test]
    fn dead_invocation_is_superseded_by_a_new_attempt() {
        let fx = Fx::new("running", &[("q1", true, false, YN)], 0);
        fx.map("old", &["q1"]).unwrap();
        let mut job = fx.job();
        let attempt = store::id();
        let mut stage = job.stage.clone().unwrap();
        stage.attempt_id = attempt.clone();
        stage.stage_id = store::id();
        job.attempt_id = Some(attempt);
        job.stage = Some(stage);
        job.revision += 1;
        fx.svc.store.commit(&job, vec![], json!({})).unwrap();
        assert!(fx.receipt("old", json!([{"id":"q1","selectedOptions":["Yes"]}])).unwrap_err().starts_with("stale_attempt"));
        assert_eq!(fx.map("new", &["q1"]).unwrap()["mapped"], true);
        assert_eq!(fx.question("q1").invocation_id.as_deref(), Some("new"));
        let old = fx.svc.invocation(&fx.job_id, "old").unwrap().unwrap();
        assert_eq!(old.state, "retired");
        // Same invocation, same attempt, but a different payload: immutable mapping.
        let err = fx.svc.ask_bridge(&fx.producer, "srs_ask_map", json!({"invocationId":"new","questions":[{"id":"q1","question":"Prompt q1","options":["Yes"],"multi":false}]})).unwrap_err();
        assert!(err.starts_with("precondition_failed"), "{err}");
    }

    #[test]
    fn concurrent_receipts_and_retirement_settle_once() {
        for _ in 0..8 {
            let fx = Fx::new("running", &[("q1", true, false, YN)], 0);
            fx.map("race", &["q1"]).unwrap();
            let svc = fx.svc.clone();
            let producer = fx.producer.clone();
            let mut handles = Vec::new();
            for index in 0..6 {
                let (svc, producer) = (svc.clone(), producer.clone());
                handles.push(std::thread::spawn(move || {
                    if index % 3 == 2 {
                        svc.ask_bridge(&producer, "srs_ask_retired", json!({"invocationId":"race","reason":"timeout"})).map(|_| String::new())
                    } else {
                        svc.ask_bridge(&producer, "srs_ask_receipt", json!({"invocationId":"race","provenance":"human","results":[{"id":"q1","selectedOptions":["Yes"]}]}))
                            .map(|v| v["receiptId"].as_str().unwrap().to_string())
                    }
                }));
            }
            let outcomes: Vec<Result<String>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            let q = fx.question("q1");
            let job = fx.job();
            let receipts: HashSet<String> = outcomes.iter().filter_map(|o| o.as_ref().ok()).filter(|s| !s.is_empty()).cloned().collect();
            if q.state == "answered" {
                assert_eq!(job.counts.questions_open, 0);
                assert_eq!(receipts.len(), 1, "every accepted replay returns the single receipt");
                assert_eq!(q.answer_receipt_id.as_deref(), receipts.iter().next().map(String::as_str));
            } else {
                assert_eq!(q.state, "open");
                assert_eq!(job.counts.questions_open, 1);
                assert!(receipts.is_empty());
            }
        }
    }

    #[test]
    fn receipt_persistence_failure_pauses_the_job() {
        let fx = Fx::new("running", &[("q1", true, false, YN)], 0);
        fx.map("fail", &["q1"]).unwrap();
        // A file where the receipts table must be created makes the first durable write fail.
        std::fs::write(fx.svc.store.dir(&fx.job_id).unwrap().join("receipts"), b"blocked").unwrap();
        let err = fx.receipt("fail", json!([{"id":"q1","selectedOptions":["Yes"]}])).unwrap_err();
        assert!(err.starts_with("storage_error") || err.contains("storage"), "{err}");
        let job = fx.job();
        assert_eq!(job.state, JobState::Paused);
        assert_eq!(job.error.as_ref().unwrap().code, "storage_error");
        assert_eq!(job.counts.questions_open, 1);
        assert_eq!(fx.question("q1").state, "asked");
        fx.retire("fail", "failed").unwrap();
        assert_eq!(fx.question("q1").state, "open");
    }

    #[test]
    fn human_answers_need_fencing_state_and_manager_for_live_asks() {
        let fx = Fx::new("running", &[("q1", true, false, YN), ("q2", true, false, YN), ("q3", true, false, YN)], 0);
        let answer = || Some(Answer { selected_options: vec!["Yes".into()], custom_input: None, note: None });
        // Running without a live Ask: refused (pause first).
        assert!(fx.svc.dispose_srs_item(&fx.job_id, 2, "q1", "provideHumanAnswer", "r", answer()).unwrap_err().starts_with("revision_conflict"));
        assert!(fx.svc.dispose_srs_item(&fx.job_id, 1, "q1", "provideHumanAnswer", "r", answer()).unwrap_err().starts_with("precondition_failed"));
        // Live single-question Ask with the Manager absent: error, nothing changes.
        fx.map("single", &["q1"]).unwrap();
        let revision = fx.job().revision;
        let err = fx.svc.dispose_srs_item(&fx.job_id, revision, "q1", "provideHumanAnswer", "r", answer()).unwrap_err();
        assert!(err.contains("Manager bridge is unavailable"), "{err}");
        assert_eq!(fx.job().revision, revision);
        assert_eq!(fx.question("q1").state, "asked");
        // Invalid answers never reach the native authority.
        let err = fx.svc.dispose_srs_item(&fx.job_id, revision, "q1", "provideHumanAnswer", "r", Some(Answer { selected_options: vec![], custom_input: None, note: Some("n".into()) })).unwrap_err();
        assert!(err.starts_with("invalid_request"), "{err}");
        // Multi-question Ask: refuse a single answer, direct to the Decisions card.
        fx.map("multi", &["q2", "q3"]).unwrap();
        let revision = fx.job().revision;
        let err = fx.svc.dispose_srs_item(&fx.job_id, revision, "q2", "provideHumanAnswer", "r", answer()).unwrap_err();
        assert!(err.contains("Decisions card"), "{err}");
        // The live holder of a question cannot be excluded either.
        let mut job = fx.job();
        job.state = JobState::Paused;
        job.revision += 1;
        fx.svc.store.commit(&job, vec![], json!({})).unwrap();
        let revision = fx.job().revision;
        let err = fx.svc.dispose_srs_item(&fx.job_id, revision, "q2", "excludeFromScope", "out of scope", None).unwrap_err();
        assert!(err.contains("live native Ask"), "{err}");
        // After retirement a direct human answer is a main-window receipt with immutable provenance.
        fx.retire("multi", "aborted").unwrap();
        let revision = fx.job().revision;
        let status = fx.svc.dispose_srs_item(&fx.job_id, revision, "q2", "provideHumanAnswer", "Confirmed with the owner", answer()).unwrap();
        assert_eq!(status["counts"]["questionsOpen"], 2);
        let q2 = fx.question("q2");
        assert_eq!(q2.state, "answered");
        let receipt: Value = fx.svc.store.get(&fx.job_id, "receipts", q2.answer_receipt_id.as_deref().unwrap()).unwrap();
        assert_eq!(receipt["provenance"], "human");
        assert_eq!(receipt["source"], "mainWindow");
        assert_eq!(receipt["rationale"], "Confirmed with the owner");
        // Answers are immutable.
        let revision = fx.job().revision;
        assert!(fx.svc.dispose_srs_item(&fx.job_id, revision, "q2", "provideHumanAnswer", "again", answer()).unwrap_err().starts_with("precondition_failed"));
        assert!(fx.svc.dispose_srs_item(&fx.job_id, revision, "q3", "excludeFromScope", "x", answer()).unwrap_err().starts_with("invalid_request"));
    }

    #[test]
    fn waiting_job_unsticks_only_when_questions_and_blocked_entries_are_resolved() {
        let fx = Fx::new("waitingForHuman", &[("q1", true, false, YN)], 1);
        let answer = Some(Answer { selected_options: vec!["Yes".into()], custom_input: None, note: None });
        let revision = fx.job().revision;
        let status = fx.svc.dispose_srs_item(&fx.job_id, revision, "q1", "provideHumanAnswer", "ok", answer).unwrap();
        assert_eq!(status["state"], "waitingForHuman", "a blocked entry is still unresolved");
        assert_eq!(status["counts"]["questionsOpen"], 0);
        let ghost = fx.svc.dispose_srs_item(&fx.job_id, fx.job().revision, "no-such-item", "excludeFromScope", "x", None).unwrap_err();
        assert!(ghost.starts_with("not_found"), "{ghost}");
    }

    #[test]
    fn exclusions_keep_audit_revise_reviews_and_count_exactly() {
        let fx = Fx::new("waitingForHuman", &[("q1", true, false, YN)], 1);
        let rev = |s: &str| Work { id: s.into(), parent_id: None, phase: Phase::Survey, kind: "capability".into(), status: "reviewed".into(), rationale: "r".into(), revision: 0 };
        let mut blocked = rev("source-abc");
        blocked.status = "blocked".into();
        blocked.kind = "sourceArea".into();
        let mut template = rev("tmpl");
        template.kind = "templateSection".into();
        template.status = "pending".into();
        let doc = Document { document_id: store::id(), r#type: "domain".into(), title: "D".into(), path: "d.md".into(), state: "committed".into(), revision_id: store::id(), hash: Some(store::hash(b"d")), bytes: 1, requirement_id: None, specificity: Some("portable".into()), reviewed_revision: Some(1), parent_document_id: None };
        let mut writes = vec![store::mutation("work", "w1", &rev("w1")).unwrap(), store::mutation("work", "source-abc", &blocked).unwrap(), store::mutation("work", "tmpl", &template).unwrap(), store::mutation("documents", &doc.document_id, &doc).unwrap()];
        for (owner, kind, target) in [(doc.document_id.as_str(), "reviewedWork", "w1"), ("q1", "work", "w1"), ("w1", "question", "q1")] {
            writes.push(fx.svc.store.relation(&fx.job_id, owner, kind, target).unwrap().unwrap());
        }
        let mut job = fx.job();
        job.counts.work = 3;
        job.counts.work_open = 2;
        job.revision += 1;
        job.completion = Some(Completion { manifest_hash: "h".into(), scope_hash: "s".into(), at: 1, label: "l".into() });
        fx.svc.store.commit(&job, writes, json!({})).unwrap();
        let review = super::super::ledger::review_mutations(&fx.svc.store, &job, &doc.document_id, &["w1".into()]).unwrap();
        fx.svc.store.commit(&job, review, json!({})).unwrap();
        assert!(super::super::ledger::document_review_current(&fx.svc.store, &job, &doc).unwrap());
        let go = |item: &str| { let revision = fx.job().revision; fx.svc.dispose_srs_item(&fx.job_id, revision, item, "excludeFromScope", "Human decided this is out of scope", None) };
        assert!(go("tmpl").unwrap_err().starts_with("precondition_failed"));
        // Question exclusion: counts once, reopens reviewed dependent work, revises its document review.
        let status = go("q1").unwrap();
        assert_eq!(status["counts"]["questionsOpen"], 0);
        assert_eq!(status["humanExclusions"], 1);
        assert_eq!(status["counts"]["workOpen"], 3);
        let w1: Work = fx.svc.store.get(&fx.job_id, "work", "w1").unwrap();
        assert_eq!(w1.status, "pending");
        let d: Document = fx.svc.store.get(&fx.job_id, "documents", &doc.document_id).unwrap();
        assert!(d.reviewed_revision.is_some(), "fan-out document records are not rewritten by human dispositions");
        assert!(!super::super::ledger::document_review_current(&fx.svc.store, &fx.job(), &d).unwrap(), "the changed work revision invalidates its dependent document lazily");
        assert!(fx.job().completion.is_none());
        assert_eq!(fx.question("q1").state, "excluded");
        assert!(go("q1").unwrap_err().starts_with("precondition_failed"));
        // Work exclusion of the blocked source: blocked/open/excluded move together and the waiting job unsticks.
        let before = fx.job();
        let status = go("source-abc").unwrap();
        assert_eq!(status["counts"]["blocked"], 0);
        assert_eq!(status["counts"]["workOpen"], before.counts.work_open - 1);
        assert_eq!(status["counts"]["excluded"], 1);
        assert_eq!(status["humanExclusions"], 2);
        assert_eq!(status["state"], "paused", "no blocked entry or question remains: explicit Resume, never an automatic model call");
        let work: Work = fx.svc.store.get(&fx.job_id, "work", "source-abc").unwrap();
        assert!(work.status == "excluded" && work.rationale.contains("out of scope"));
        let mut dispositions = 0;
        fx.svc.store.for_each(&fx.job_id, "dispositions", |v| { dispositions += 1; assert_eq!(v["actor"], "human"); assert!(v["rationale"].as_str().unwrap().contains("out of scope")); Ok(()) }).unwrap();
        assert_eq!(dispositions, 2);
        let mut linked = Vec::new();
        fx.svc.store.related(&fx.job_id, "source-abc", "humanDisposition", |t| { linked.push(t.to_string()); Ok(()) }).unwrap();
        assert_eq!(linked.len(), 1);
        let record: Value = fx.svc.store.get(&fx.job_id, "dispositions", &linked[0]).unwrap();
        assert_eq!(record["kind"], "work");
        assert_eq!(record["previousStatus"], "blocked");
        assert!(go("source-abc").unwrap_err().starts_with("precondition_failed"));
        // Only paused/waiting jobs accept human exclusions.
        let mut job = fx.job();
        job.state = JobState::Running;
        job.revision += 1;
        fx.svc.store.commit(&job, vec![], json!({})).unwrap();
        assert!(go("w1").unwrap_err().starts_with("precondition_failed"));
    }
}
