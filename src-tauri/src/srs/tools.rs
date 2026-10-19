//! Strict native tool handlers. Every call is bound to the exact producer, attempt, stage and persisted admission;
//! no model value selects a path, root, vault, process or authority. Replies are bounded transport frames only.
use super::*;
use crate::knowledge::{WritePrecondition, WriteReceipt};
use serde_json::Map;
use std::fs;

pub(super) fn err(code: &str, message: impl AsRef<str>) -> String { format!("{code}: {}", message.as_ref()) }
pub(super) fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value.get(key).and_then(Value::as_str).ok_or_else(|| err("invalid_request", format!("{key} is required")))
}
fn only<'a>(value: &'a Value, allowed: &[&str]) -> Result<&'a Map<String, Value>> {
    let object = value.as_object().ok_or_else(|| err("invalid_request", "A JSON object is required"))?;
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) { return Err(err("invalid_request", format!("Unknown field {key}"))); }
    }
    Ok(object)
}
fn number(object: &Map<String, Value>, key: &str, default: u64) -> Result<u64> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value.as_u64().ok_or_else(|| err("invalid_request", format!("{key} must be a non-negative integer"))),
    }
}
fn rank(phase: Phase) -> u8 { match phase { Phase::Survey => 0, Phase::Specify => 1, Phase::GapReview => 2, Phase::Verify => 3 } }
fn reply(value: Value) -> Result<Value> {
    if serde_json::to_vec(&value).map_err(|e| e.to_string())?.len() > MAX_RECORD as usize {
        return Err(err("invalid_request", "The result exceeds 1 MiB; request a smaller page"));
    }
    Ok(value)
}
fn page_value(section: &str, revision: u64, items: Vec<Value>, total: u64, offset: u64, next: Option<u64>) -> Value {
    json!({"section":section,"revision":revision,"total":total,"offset":offset,"nextOffset":next,"items":items})
}
fn hex_limbs(hex: &str) -> [u64; 4] {
    let mut limbs = [0u64; 4];
    for (index, limb) in limbs.iter_mut().enumerate() { *limb = u64::from_str_radix(&hex[index * 16..index * 16 + 16], 16).unwrap_or(0); }
    limbs
}
fn pausing(error: &str) -> Option<&'static str> {
    ["storage_error", "vault_conflict", "vault_unavailable"].into_iter().find(|code| error.starts_with(*code))
}
fn write_link(store: &Store, job: &str, writes: &mut Vec<store::Mutation>, owner: &str, kind: &str, target: &str) -> Result<()> {
    if let Some(mutation) = store.relation(job, owner, kind, target)? {
        if !writes.iter().any(|w| w.table == mutation.table && w.id == mutation.id) { writes.push(mutation); }
    }
    Ok(())
}

/// Marks the active stage interrupted so no later reconciliation can treat it as live.
pub(super) fn interrupt_stage(job: &mut Job) -> Result<Vec<store::Mutation>> {
    let mut writes = Vec::new();
    if let Some(stage) = job.stage.as_mut() {
        if !matches!(stage.state.as_str(), "committed" | "interrupted") {
            stage.state = "interrupted".into();
            writes.push(mutation("stages", &stage.stage_id, &*stage)?);
        }
    }
    Ok(writes)
}

impl SrsService {
    pub(crate) fn handle_bridge(&self, producer: &DependencyProducer, action: &str, p: Value) -> Result<Value> {
        match action {
            "srs_tool" => self.handle_tool(producer, &p),
            "srs_stage_admitted" => self.stage_admitted(producer, &p),
            "srs_stage_validate" => self.stage_validate(producer, &p),
            "srs_stage_yielded" => self.stage_yielded(producer, &p),
            "srs_ask_map" | "srs_ask_receipt" | "srs_ask_retired" => self.ask_bridge(producer, action, p),
            _ => Err("invalid_action".into()),
        }
    }

    fn current_stage(job: &Job, producer: &DependencyProducer, p: &Value) -> Result<Stage> {
        if job.producer.as_ref() != Some(producer) { return Err(err("stale_attempt", "This job is bound to a different Architect launch or session")); }
        if job.attempt_id.as_deref() != Some(text(p, "attemptId")?) { return Err(err("stale_attempt", "The attempt is no longer current")); }
        let stage = job.stage.clone().ok_or_else(|| err("stale_stage", "The job has no current stage"))?;
        if stage.stage_id != text(p, "stageId")? || stage.attempt_id != text(p, "attemptId")? { return Err(err("stale_stage", "The stage is no longer current")); }
        Ok(stage)
    }

    fn bound(&self, producer: &DependencyProducer, p: &Value, allow_committed: bool) -> Result<(Job, Stage)> {
        let job = self.store.job(text(p, "jobId")?).map_err(|_| err("not_found", "Unknown SRS job"))?;
        let stage = Self::current_stage(&job, producer, p)?;
        let live = matches!(stage.state.as_str(), "queued" | "active") && stage.admitted;
        if !(live || (allow_committed && stage.state == "committed")) { return Err(err("stale_stage", "The stage has no persisted native admission or already ended")); }
        let state_ok = matches!(job.state, JobState::Running | JobState::Reviewing) || (allow_committed && job.state == JobState::Completed);
        if !state_ok { return Err(err("precondition_failed", format!("The job is {:?}; no analysis tool is authorized", job.state))); }
        Ok((job, stage))
    }

    fn stage_admitted(&self, producer: &DependencyProducer, p: &Value) -> Result<Value> {
        only(p, &["jobId", "attemptId", "stageId", "nonce"])?;
        let _g = self.gate.lock();
        let mut job = self.store.job(text(p, "jobId")?).map_err(|_| err("not_found", "Unknown SRS job"))?;
        let stage = Self::current_stage(&job, producer, p)?;
        if stage.nonce != text(p, "nonce")? { return Err(err("stale_stage", "The stage nonce is not the persisted claim")); }
        if !matches!(job.state, JobState::Running | JobState::Reviewing | JobState::Uncertain) { return Err(err("not_authorized", "The job is not running")); }
        if stage.admitted { return Ok(json!({"persisted":true,"duplicate":true})); }
        if !matches!(stage.state.as_str(), "claimed" | "queued") { return Err(err("stale_stage", "The stage cannot be admitted")); }
        let mut updated = stage;
        updated.admitted = true;
        updated.state = "active".into();
        let write = mutation("stages", &updated.stage_id, &updated)?;
        job.stage = Some(updated);
        if job.state == JobState::Uncertain { job.state = if job.phase == Phase::Verify { JobState::Reviewing } else { JobState::Running }; job.error = None; }
        self.save(&mut job, vec![write], "Native admission marker persisted")?;
        Ok(json!({"persisted":true,"duplicate":false}))
    }

    fn stage_validate(&self, producer: &DependencyProducer, p: &Value) -> Result<Value> {
        only(p, &["jobId", "attemptId", "stageId", "nonce", "preAdmission"])?;
        let pre = p.get("preAdmission").and_then(Value::as_bool).unwrap_or(false);
        let _g = self.gate.lock();
        let job = self.store.job(text(p, "jobId")?).map_err(|_| err("not_found", "Unknown SRS job"))?;
        let stage = Self::current_stage(&job, producer, p)?;
        if pre {
            // Before the native prompt exists the persisted claim is the only authority: claimed, unadmitted, same nonce.
            if stage.nonce != text(p, "nonce")? || stage.state != "claimed" || stage.admitted {
                return Err(err("not_authorized", "The stage is not an unadmitted claim"));
            }
            if !matches!(job.state, JobState::Running | JobState::Reviewing | JobState::Uncertain) {
                return Err(err("not_authorized", format!("The job is {:?}", job.state)));
            }
            return Ok(json!({"authorized":true}));
        }
        if stage.nonce != text(p, "nonce")? || !matches!(stage.state.as_str(), "queued" | "active" | "committed") || !stage.admitted {
            return Err(err("not_authorized", "No persisted admission for this stage"));
        }
        if !matches!(job.state, JobState::Running | JobState::Reviewing | JobState::Completed) {
            return Err(err("not_authorized", format!("The job is {:?}", job.state)));
        }
        Ok(json!({"authorized":true}))
    }

    fn stage_yielded(&self, producer: &DependencyProducer, p: &Value) -> Result<Value> {
        only(p, &["jobId", "attemptId", "stageId", "reason"])?;
        let reason = text(p, "reason")?;
        if !["unrelated_prompt", "continuation_uncertain", "guard_failure", "disconnected"].contains(&reason) { return Err(err("invalid_request", "Unknown yield reason")); }
        let _g = self.gate.lock();
        let mut job = self.store.job(text(p, "jobId")?).map_err(|_| err("not_found", "Unknown SRS job"))?;
        Self::current_stage(&job, producer, p)?;
        if !matches!(job.state, JobState::Running | JobState::Reviewing | JobState::Uncertain | JobState::WaitingForHuman) { return Ok(json!({"paused":true})); }
        let writes = interrupt_stage(&mut job)?;
        let (state, code, message) = match reason {
            "unrelated_prompt" => (JobState::Paused, "unrelated_prompt", "Another prompt took the Architect, so SRS paused at its last checkpoint. Resume explicitly."),
            "continuation_uncertain" => (JobState::Paused, "continuation_uncertain", "Native compaction continuation could not be proven analysis-only, so SRS paused before any provider call."),
            "guard_failure" => (JobState::Paused, "native_srs_guard_unavailable", "The native SRS guard could not authorize the next provider call."),
            _ => (JobState::Interrupted, "producer_ended", "The Architect bridge disconnected. Inspect the saved checkpoint and explicitly Resume."),
        };
        job.state = state;
        job.error = Some(Diagnostic { code: code.into(), message: message.into() });
        self.save(&mut job, writes, "Native guard yielded the SRS stage")?;
        Ok(json!({"paused":true}))
    }

    pub(crate) fn handle_tool(&self, producer: &DependencyProducer, p: &Value) -> Result<Value> {
        let action = text(p, "action")?.to_string();
        let mut abort: Option<Value> = None;
        let result = {
            let _g = self.gate.lock();
            let result = self.run_tool(producer, &action, p);
            match &result {
                Err(error) if pausing(error).is_some() => {
                    abort = self.pause_for_error(p, pausing(error).unwrap_or("storage_error"), error);
                }
                _ => {}
            }
            result
        };
        if let Some(payload) = abort {
            if let Some(hub) = self.registry().ok().and_then(|r| r.manager_hub().cloned()) { let _ = hub.srs_control(producer, "srs_pause", payload); }
        }
        result
    }

    fn pause_for_error(&self, p: &Value, code: &str, message: &str) -> Option<Value> {
        let mut job = self.store.job(text(p, "jobId").ok()?).ok()?;
        if !matches!(job.state, JobState::Running | JobState::Reviewing) { return None; }
        let stage = job.stage.clone();
        let writes = interrupt_stage(&mut job).ok()?;
        job.state = JobState::Paused;
        job.error = Some(Diagnostic { code: code.into(), message: message.into() });
        self.save(&mut job, writes, "Tool failure paused SRS with diagnostics").ok()?;
        stage.map(|s| json!({"jobId":job.job_id,"attemptId":s.attempt_id,"stageId":s.stage_id,"reason":"pause"}))
    }

    fn run_tool(&self, producer: &DependencyProducer, action: &str, p: &Value) -> Result<Value> {
        let finishing = action == "srs_finish_stage";
        let (mut job, stage) = self.bound(producer, p, finishing)?;
        match action {
            "srs_inspect" => reply(self.tool_inspect(&job, p)?),
            "srs_read_evidence" => reply(self.tool_read_evidence(&job, p)?),
            "srs_write_document_chunk" => self.tool_chunk(&job, p),
            "srs_commit_document" => self.tool_commit(&mut job, p),
            "srs_checkpoint" => self.tool_checkpoint(&mut job, p),
            "srs_finish_stage" => self.tool_finish(&mut job, stage, p),
            _ => Err(err("invalid_request", "Unknown SRS tool")),
        }
    }

    fn relations(&self, job: &str, owner: &str, kind: &str, offset: u64, limit: u64) -> Result<Value> {
        let mut items = Vec::new();
        let mut total = 0u64;
        self.store.related(job, owner, kind, |id| {
            if total >= offset && (items.len() as u64) < limit { items.push(json!(id)); }
            total += 1;
            Ok(())
        })?;
        let next = offset + items.len() as u64;
        Ok(json!({"items":items,"total":total,"nextOffset":if next < total { Some(next) } else { None }}))
    }

    fn tool_inspect(&self, job: &Job, p: &Value) -> Result<Value> {
        let object = only(p, &["action", "jobId", "attemptId", "stageId", "section", "id", "offset", "limit"])?;
        let section = text(p, "section")?;
        let offset = number(object, "offset", 0)?;
        let limit = number(object, "limit", 50)?;
        if !(1..=200).contains(&limit) { return Err(err("invalid_request", "limit must be 1..200")); }
        let id = match object.get("id") { None | Some(Value::Null) => None, Some(v) => Some(v.as_str().ok_or_else(|| err("invalid_request", "id must be text"))?) };
        let dir = self.store.dir(&job.job_id)?;
        let revision = job.revision;
        match section {
            "request" => {
                let roots: Vec<_> = job.scope.roots.iter().map(|r| json!({"path":r.path,"kind":r.kind,"rationale":r.rationale})).collect();
                let exclusions: Vec<_> = job.scope.exclusions.iter().map(|e| json!({"path":e.path,"reason":e.reason,"source":e.source})).collect();
                Ok(page_value(section, revision, vec![json!({"jobId":job.job_id,"phase":job.phase,"scopeHash":job.scope_hash,"sourceHash":job.source_hash,"roots":roots,"exclusions":exclusions,"vaultMode":job.scope.vault_mode,"outputRoot":format!("SRS/{}",job.job_id),"template":job.scope.template,"counts":job.counts})], 1, 0, None))
            }
            "template" => match id {
                None => {
                    let manifest = documents::template_manifest();
                    let items: Vec<Value> = manifest.files.iter().map(|f| json!({"name":f.name,"sha256":f.sha256,"revision":manifest.revision})).collect();
                    let total = items.len() as u64;
                    Ok(page_value(section, revision, items, total, 0, None))
                }
                Some(name) => {
                    let chunk = documents::template_chunk(name, offset as usize).map_err(|e| err("invalid_request", e))?;
                    Ok(page_value(section, revision, vec![serde_json::to_value(&chunk).map_err(|e| e.to_string())?], 1, 0, None))
                }
            },
            "inventory" => match id {
                Some(id) => {
                    store::safe_id(id)?;
                    let item = inventory::read_item(&dir, id).map_err(|e| err("not_found", e))?;
                    Ok(page_value(section, revision, vec![serde_json::to_value(&item).map_err(|e| e.to_string())?], 1, 0, None))
                }
                None => {
                    let page = inventory::page(&dir, offset, limit as usize)?;
                    let items = page.items.iter().map(|i| serde_json::to_value(i).map_err(|e| e.to_string())).collect::<Result<Vec<_>>>()?;
                    Ok(page_value(section, revision, items, page.total, page.offset, page.next_offset))
                }
            },
            "work" | "documents" | "questions" => match id {
                Some(id) => {
                    store::safe_id(id)?;
                    let mut record: Value = self.store.get(&job.job_id, section, id).map_err(|e| err("not_found", e))?;
                    self.enrich_inspection_record(job, section, &mut record)?;
                    let kinds: &[&str] = match section { "work" => &["evidence", "requirement", "question"], "documents" => &["parent", "reviewedWork", "related"], _ => &["work"] };
                    let mut relations = Map::new();
                    for kind in kinds { relations.insert((*kind).into(), self.relations(&job.job_id, id, kind, offset, limit)?); }
                    Ok(page_value(section, revision, vec![json!({"record":record,"relations":relations})], 1, 0, None))
                }
                None => {
                    let page = self.store.page(&job.job_id, section, offset as usize, limit as usize)?;
                    let mut items = page["items"].as_array().cloned().unwrap_or_default();
                    for record in &mut items { self.enrich_inspection_record(job, section, record)?; }
                    Ok(page_value(section, revision, items, page["total"].as_u64().unwrap_or(0), offset, page["nextOffset"].as_u64()))
                }
            },
            "coverage" => {
                let page = inventory::page(&dir, offset, limit as usize)?;
                let mut items = Vec::new();
                for item in &page.items {
                    let covered = match (item.state.as_str(), item.kind.as_str(), item.sha256.as_deref()) {
                        ("ready", "text" | "image", Some(hash)) => self.covered(&job.job_id, &item.id, item.bytes, hash)?.0,
                        _ => 0,
                    };
                    items.push(json!({"id":item.id,"path":item.path,"kind":item.kind,"state":item.state,"bytes":item.bytes,"coveredBytes":covered,"rationale":item.rationale,"exclusionSource":item.exclusion_source}));
                }
                Ok(page_value(section, revision, items, page.total, page.offset, page.next_offset))
            }
            "checkpoint" => Ok(page_value(section, revision, vec![json!({"revision":job.revision,"workRevision":job.work_revision,"documentRevision":job.document_revision,"nextRequirementOrdinal":job.next_requirement,"phase":job.phase,"counts":job.counts,"checkpoint":job.checkpoint,"stage":job.stage.as_ref().map(|s| json!({"stageId":s.stage_id,"number":s.number,"phase":s.phase}))})], 1, 0, None)),
            _ => Err(err("invalid_request", "Unknown inspect section")),
        }
    }
    fn enrich_inspection_record(&self, job: &Job, section: &str, value: &mut Value) -> Result<()> {
        if section == "questions" {
            let question: Question = serde_json::from_value(value.clone()).map_err(|e| err("storage_error", e.to_string()))?;
            value["answer"] = serde_json::to_value(questions::authenticated_answer(&self.store, &job.job_id, &question)?).map_err(|e| e.to_string())?;
        }
        if section == "documents" {
            let doc: Document = serde_json::from_value(value.clone()).map_err(|e| err("storage_error", e.to_string()))?;
            value["reviewCurrent"] = json!(ledger::document_review_current(&self.store, job, &doc)?);
        }
        Ok(())
    }

    /// Contiguous byte coverage from the durable per-evidence receipts (union of overlapping or out-of-order reads).
    pub(super) fn covered(&self, job: &str, id: &str, total: u64, hash: &str) -> Result<(u64, bool)> {
        let table = format!("receipts-{id}");
        let mut covered = 0u64;
        let mut saw = false;
        loop {
            let previous = covered;
            self.store.for_each(job, &table, |receipt| {
                if receipt["sha256"] != hash || receipt["totalBytes"].as_u64() != Some(total) { return Ok(()); }
                let (Some(offset), Some(bytes)) = (receipt["offset"].as_u64(), receipt["bytes"].as_u64()) else { return Ok(()); };
                let Some(end) = offset.checked_add(bytes) else { return Ok(()); };
                if end > total { return Ok(()); }
                saw = true;
                if offset <= covered { covered = covered.max(end); }
                Ok(())
            })?;
            if covered >= total || covered == previous { break; }
        }
        Ok((covered, saw))
    }

    fn tool_read_evidence(&self, job: &Job, p: &Value) -> Result<Value> {
        let object = only(p, &["action", "jobId", "attemptId", "stageId", "evidenceId", "offset"])?;
        let id = text(p, "evidenceId")?;
        store::safe_id(id)?;
        let offset = number(object, "offset", 0)?;
        let dir = self.store.dir(&job.job_id)?;
        let item = inventory::read_item(&dir, id).map_err(|e| err("not_found", e))?;
        let read = inventory::read_evidence(&dir, id, offset).map_err(|e| err("invalid_request", e))?;
        let mut value = serde_json::to_value(&read).map_err(|e| e.to_string())?;
        value["receiptId"] = Value::Null;
        if item.state == "ready" && matches!(read.kind.as_str(), "text" | "image") {
            let hash = item.sha256.clone().ok_or_else(|| err("storage_error", "Ready evidence has no immutable hash"))?;
            if read.total_bytes != item.bytes { return Err(err("storage_error", "Evidence size changed from its immutable record")); }
            let receipt_id = store::hash(format!("{id}:{}:{}", read.offset, read.bytes).as_bytes());
            let record = json!({"receiptId":receipt_id,"evidenceId":id,"offset":read.offset,"bytes":read.bytes,"totalBytes":read.total_bytes,"sha256":hash,"at":self.now()});
            store::atomic(&self.store.path(&job.job_id, &format!("receipts-{id}"), &receipt_id)?, &record).map_err(|e| err("storage_error", e))?;
            value["receiptId"] = json!(receipt_id);
        }
        reply(value)
    }

    fn tool_chunk(&self, job: &Job, p: &Value) -> Result<Value> {
        only(p, &["action", "jobId", "attemptId", "stageId", "documentId", "revisionId", "offset", "text", "expectedPrefixHash"])?;
        let document_id = text(p, "documentId")?;
        let revision_id = text(p, "revisionId")?;
        let offset = p.get("offset").and_then(Value::as_u64).ok_or_else(|| err("invalid_request", "offset is required"))?;
        store::safe_id(document_id)?;
        let doc: Document = self.store.get(&job.job_id, "documents", document_id).map_err(|e| err("not_found", e))?;
        if let Ok(record) = self.store.get::<Value>(&job.job_id, "revisions", revision_id) {
            if record["documentId"] == document_id {
                return Ok(json!({"documentId":document_id,"revisionId":revision_id,"stagedBytes":record["bytes"],"stagedHash":record["hash"],"committed":true}));
            }
        }
        if doc.revision_id != revision_id { return Err(err("stale_stage", "That revision is not the document's current editable revision")); }
        let mut result = self.store.append_chunk(&job.job_id, revision_id, offset, text(p, "text")?, text(p, "expectedPrefixHash")?)?;
        result["documentId"] = json!(document_id);
        Ok(result)
    }

    fn vault_for(&self, job: &Job) -> Result<crate::knowledge::VaultRoot> {
        let vault = self.knowledge.vault_root(&job.scope.vault_id).map_err(|e| err("vault_unavailable", e))?;
        if !same_path(&vault.root, Path::new(&job.scope.vault_root)) { return Err(err("vault_unavailable", "The recorded vault is no longer bound at its original location")); }
        Ok(vault)
    }

    /// Resolves an allocated public path to its document record without any whole-job path map.
    fn document_by_path(&self, job: &str, path: &str) -> Result<Option<Document>> {
        let load = |id: &str| -> Result<Option<Document>> {
            if store::safe_id(id).is_err() { return Ok(None); }
            Ok(self.store.get::<Document>(job, "documents", id).ok().filter(|d| d.path == path))
        };
        if let Some(rest) = path.strip_prefix("Requirements/") {
            let Some(id) = rest.strip_suffix(".md") else { return Ok(None) };
            if store::safe_id(id).is_err() { return Ok(None); }
            return match self.store.get::<Requirement>(job, "requirements", id) { Ok(r) => load(&r.document_id), Err(_) => Ok(None) };
        }
        for prefix in ["Domains/", "Flows/", "Interfaces/"] {
            if let Some(rest) = path.strip_prefix(prefix) { return match rest.strip_suffix(".md") { Some(id) => load(id), None => Ok(None) }; }
        }
        let nil = uuid::Uuid::nil().to_string();
        for kind in ["master", "uiObservations", "structureObservations", "traceability", "coverage", "glossary", "questions"] {
            if documents::document_path(kind, &nil, None).ok().as_deref() != Some(path) { continue; }
            if let Ok(fixed) = self.store.get::<Value>(job, "fixed", kind) {
                return Ok(fixed["documentId"].as_str().and_then(|id| load(id).ok().flatten()));
            }
            let mut found: Option<Document> = None;
            self.store.for_each(job, "documents", |v| {
                let d: Document = serde_json::from_value(v).map_err(|e| e.to_string())?;
                if d.r#type == kind { found = Some(d); }
                Ok(())
            })?;
            if let Some(d) = &found { store::atomic(&self.store.path(job, "fixed", kind)?, &json!({"documentId":d.document_id}))?; }
            return Ok(found.filter(|d| d.path == path));
        }
        Ok(None)
    }

    fn tool_commit(&self, job: &mut Job, p: &Value) -> Result<Value> {
        only(p, &["action", "jobId", "attemptId", "stageId", "documentId", "revisionId", "expectedHash", "metadata"])?;
        let document_id = text(p, "documentId")?;
        let revision_id = text(p, "revisionId")?;
        let expected = text(p, "expectedHash")?;
        store::safe_id(document_id)?;
        let meta: Metadata = serde_json::from_value(p.get("metadata").cloned().unwrap_or(Value::Null)).map_err(|e| err("invalid_request", format!("metadata: {e}")))?;
        let mut doc: Document = self.store.get(&job.job_id, "documents", document_id).map_err(|e| err("not_found", e))?;
        if let Ok(record) = self.store.get::<Value>(&job.job_id, "revisions", revision_id) {
            if record["documentId"] == document_id {
                if record["hash"] != expected { return Err(err("hash_mismatch", "This revision was committed with a different hash")); }
                return Ok(json!({"documentId":document_id,"revisionId":revision_id,"path":record["path"],"hash":record["hash"],"bytes":record["bytes"],"nextRevisionId":doc.revision_id,"replayed":true,"reviewCurrent":ledger::document_review_current(&self.store,job,&doc)?}));
            }
        }
        if doc.revision_id != revision_id { return Err(err("stale_stage", "That revision is not the document's current editable revision")); }
        if meta.r#type != doc.r#type || meta.requirement_id != doc.requirement_id || meta.related_document_ids.len() > 200 || meta.reviewed_work_ids.len() > 200 || meta.citations.len() > 200 {
            return Err(err("invalid_request", "Metadata does not match the allocated document or exceeds a 200-entry reference page"));
        }
        let required = match doc.r#type.as_str() {
            "uiObservations" => Some("uiSpecific"),
            "structureObservations" => Some("implementationSpecific"),
            "requirement" | "domain" | "flow" | "interface" => None,
            _ => Some("portable"),
        };
        if required.is_some_and(|r| r != meta.specificity) || !["portable", "uiSpecific", "implementationSpecific"].contains(&meta.specificity.as_str()) {
            return Err(err("invalid_request", format!("A {} note requires specificity {}", doc.r#type, required.unwrap_or("portable, uiSpecific or implementationSpecific"))));
        }
        let vault = self.vault_for(job)?;
        let staged = self.store.chunk_path(&job.job_id, revision_id)?;
        store::plain(&staged)?;
        if !staged.exists() { return Err(err("precondition_failed", "No chunks are staged for this revision")); }
        let bytes = fs::read(&staged).map_err(|e| err("storage_error", e.to_string()))?;
        if store::hash(&bytes) != expected { return Err(err("hash_mismatch", "The staged bytes do not match expectedHash")); }
        let body = String::from_utf8(bytes).map_err(|_| err("invalid_request", "The staged note is not valid UTF-8"))?;
        let dir = self.store.dir(&job.job_id)?;
        let mut forbidden = Vec::new();
        for citation in &meta.citations {
            store::safe_id(&citation.evidence_id)?;
            let item = inventory::read_item(&dir, &citation.evidence_id).map_err(|e| err("invalid_request", format!("Unknown citation evidence: {e}")))?;
            if let Some(name) = item.path.rsplit('/').next() { forbidden.push(name.to_string()); }
            forbidden.push(item.path);
        }
        let context = documents::ValidationContext { path: &doc.path, document_type: &doc.r#type, specificity: &meta.specificity, requirement_id: doc.requirement_id.as_deref(), forbidden_terms: &forbidden };
        let report = documents::validate_document(&body, &context, |path| self.document_by_path(&job.job_id, path).ok().flatten().is_some()).map_err(|e| err("invalid_request", e))?;
        if !report.not_applicable_sections.is_empty() && meta.citations.is_empty() {
            return Err(err("precondition_failed", "Not-applicable sections need private evidence citations in metadata"));
        }
        for related in &meta.related_document_ids { store::safe_id(related)?; let _: Document = self.store.get(&job.job_id, "documents", related).map_err(|_| err("invalid_request", "Unknown related document"))?; }
        let mut writes = ledger::review_mutations(&self.store, job, document_id, &meta.reviewed_work_ids)?;
        let relative = format!("SRS/{}/{}", job.job_id, doc.path);
        let precondition = match &doc.hash { None => WritePrecondition::Create, Some(hash) => WritePrecondition::Replace { expected_hash: hash.clone() } };
        let receipt: WriteReceipt = match vault.write_markdown(&relative, &body, precondition) {
            Ok(receipt) => receipt,
            Err(error) => match vault.read_markdown(&relative) {
                // A crash after publication but before the ledger commit replays idempotently.
                Ok(existing) if store::hash(&existing) == expected => WriteReceipt { path: relative.clone(), hash: expected.to_string(), bytes: existing.len() as u64 },
                _ => return Err(err("vault_conflict", format!("{error}. The note was not overwritten; review it in the vault"))),
            },
        };
        let next_revision = store::id();
        doc.state = "committed".into();
        doc.hash = Some(receipt.hash.clone());
        doc.bytes = receipt.bytes;
        doc.specificity = Some(meta.specificity.clone());
        doc.reviewed_revision = if meta.reviewed_work_ids.is_empty() { None } else { Some(job.work_revision) };
        doc.revision_id = next_revision.clone();
        for related in &meta.related_document_ids { write_link(&self.store, &job.job_id, &mut writes, document_id, "related", related)?; }
        writes.push(mutation("documents", document_id, &doc)?);
        writes.push(mutation("revisions", revision_id, &json!({"documentId":document_id,"hash":receipt.hash,"bytes":receipt.bytes,"path":doc.path,"at":self.now(),"nextRevisionId":next_revision}))?);
        writes.push(mutation("citations", revision_id, &json!({"documentId":document_id,"citations":meta.citations,"notApplicable":report.not_applicable_sections}))?);
        job.document_revision = job.document_revision.checked_add(1).ok_or_else(|| err("storage_error", "Document revision exhausted"))?;
        job.completion = None;
        self.save(job, writes, "Document revision committed")?;
        Ok(json!({"documentId":document_id,"revisionId":revision_id,"path":doc.path,"hash":receipt.hash,"bytes":receipt.bytes,"nextRevisionId":next_revision,"revision":job.revision}))
    }

    fn tool_checkpoint(&self, job: &mut Job, p: &Value) -> Result<Value> {
        let object = only(p, &["action", "jobId", "attemptId", "stageId", "expectedRevision", "idempotencyKey", "updates"])?;
        let key = match object.get("idempotencyKey") { None | Some(Value::Null) => None, Some(v) => Some(v.as_str().ok_or_else(|| err("invalid_request", "idempotencyKey must be text"))?) };
        if let Some(key) = key {
            store::safe_id(key)?;
            if let Ok(saved) = self.store.get::<Value>(&job.job_id, "idempotency", key) { return Ok(saved["result"].clone()); }
        }
        let expected = object.get("expectedRevision").and_then(Value::as_u64).ok_or_else(|| err("invalid_request", "expectedRevision is required"))?;
        if expected != job.revision { return Err(err("revision_conflict", format!("The current revision is {}; inspect the checkpoint section again", job.revision))); }
        let updates: Updates = serde_json::from_value(object.get("updates").cloned().unwrap_or(Value::Null)).map_err(|e| err("invalid_request", format!("updates: {e}")))?;
        let (mut writes, allocated) = ledger::checkpoint(&self.store, job, updates, self.now())?;
        let result = json!({"revision":job.revision + 1,"workRevision":job.work_revision,"allocated":allocated});
        if let Some(key) = key { writes.push(mutation("idempotency", key, &json!({"result":result}))?); }
        self.save(job, writes, "Checkpoint committed")?;
        Ok(result)
    }

    fn open_phase(&self, job: &str) -> Result<Option<Phase>> {
        let mut lowest: Option<Phase> = None;
        self.store.for_each(job, "work", |v| {
            let work: Work = serde_json::from_value(v).map_err(|e| e.to_string())?;
            if matches!(work.status.as_str(), "pending" | "active") && lowest.map_or(true, |l| rank(work.phase) < rank(l)) { lowest = Some(work.phase); }
            Ok(())
        })?;
        Ok(lowest)
    }

    fn uncovered(&self, job: &Job, blockers: &mut Vec<String>) -> Result<()> {
        let dir = self.store.dir(&job.job_id)?;
        inventory::for_each(&dir, &mut |item| {
            if blockers.len() >= 200 || item.state != "ready" || !matches!(item.kind.as_str(), "text" | "image") { return Ok(()); }
            let hash = item.sha256.as_deref().unwrap_or("");
            let (covered, saw) = self.covered(&job.job_id, &item.id, item.bytes, hash)?;
            if covered < item.bytes || !saw { blockers.push(format!("Read all admitted evidence {} with native receipts ({covered}/{} bytes)", item.id, item.bytes)); }
            Ok(())
        })
    }

    /// Order-independent, constant-memory multiset hash of every committed document record.
    fn manifest(&self, job: &Job) -> Result<(Value, String)> {
        let mut sum = [0u64; 4];
        let mut documents = 0u64;
        self.store.for_each(&job.job_id, "documents", |v| {
            let d: Document = serde_json::from_value(v).map_err(|e| e.to_string())?;
            let digest = store::hash(format!("{}|{}|{}|{}", d.document_id, d.path, d.hash.as_deref().unwrap_or(""), d.bytes).as_bytes());
            for (limb, add) in sum.iter_mut().zip(hex_limbs(&digest)) { *limb = limb.wrapping_add(add); }
            documents += 1;
            Ok(())
        })?;
        let manifest = json!({"jobId":job.job_id,"scopeHash":job.scope_hash,"sourceHash":job.source_hash,"template":job.scope.template["revision"],"documents":documents,"requirements":job.counts.requirements,"workRevision":job.work_revision,"documentRevision":job.document_revision,"documentSetHash":format!("{:016x}{:016x}{:016x}{:016x}", sum[0], sum[1], sum[2], sum[3])});
        let hash = store::hash(&serde_json::to_vec(&manifest).map_err(|e| e.to_string())?);
        Ok((manifest, hash))
    }

    fn tool_finish(&self, job: &mut Job, stage: Stage, p: &Value) -> Result<Value> {
        let object = only(p, &["action", "jobId", "attemptId", "stageId", "expectedRevision", "review"])?;
        if stage.state == "committed" {
            return Ok(json!({"advanced":true,"state":job.state,"nextPhase":job.phase,"completed":job.state == JobState::Completed,"reasons":[],"replayed":true}));
        }
        let expected = object.get("expectedRevision").and_then(Value::as_u64).ok_or_else(|| err("invalid_request", "expectedRevision is required"))?;
        if expected != job.revision { return Err(err("revision_conflict", format!("The current revision is {}; inspect the checkpoint section again", job.revision))); }
        let review: Review = serde_json::from_value(object.get("review").cloned().unwrap_or(Value::Null)).map_err(|e| err("invalid_request", format!("review: {e}")))?;
        if review.summary.trim().is_empty() || review.summary.len() > 16_384 { return Err(err("invalid_request", "summary must be 1..16384 bytes")); }
        if review.phase != job.phase || stage.phase != job.phase { return Err(err("stale_stage", "The review phase is not the backend phase")); }
        if review.document_revision.is_some_and(|r| r != job.document_revision) || review.work_revision.is_some_and(|r| r != job.work_revision) {
            return Err(err("revision_conflict", "The review names stale document/work revisions"));
        }
        if review.gap_ids.len() > 200 || review.unresolved_question_ids.len() > 200 { return Err(err("invalid_request", "gapIds and unresolvedQuestionIds accept at most 200 entries")); }
        for id in &review.gap_ids { store::safe_id(id)?; let _: Work = self.store.get(&job.job_id, "work", id).map_err(|_| err("invalid_request", "A gap ID must identify existing work"))?; }
        for id in &review.unresolved_question_ids { store::safe_id(id)?; let _: Question = self.store.get(&job.job_id, "questions", id).map_err(|_| err("invalid_request", "Unknown question ID"))?; }
        let declaration = match review.final_declaration.as_deref() { None => None, Some(d @ ("noKnownGap" | "moreWorkNeeded")) => Some(d), Some(_) => return Err(err("invalid_request", "finalDeclaration must be noKnownGap or moreWorkNeeded")) };
        let mut blockers: Vec<String> = Vec::new();
        if !review.gap_ids.is_empty() { blockers.push("The reviewer reported known gaps; queue and complete work for them".into()); }
        if declaration == Some("moreWorkNeeded") { blockers.push("The reviewer declared more work is needed".into()); }
        if matches!(job.phase, Phase::GapReview | Phase::Verify) && declaration != Some("noKnownGap") { blockers.push("This phase needs an explicit noKnownGap declaration".into()); }
        let lowest = self.open_phase(&job.job_id)?;
        if lowest.is_some_and(|l| rank(l) <= rank(job.phase)) { blockers.push("Pending or active work remains at or before this phase".into()); }
        if job.counts.blocked > 0 { blockers.push("Blocked work needs a human disposition or readable evidence".into()); }
        if job.counts.questions_open > 0 { blockers.push("Required stakeholder questions are unresolved".into()); }
        match job.phase {
            Phase::Survey => self.uncovered(job, &mut blockers)?,
            Phase::Specify => if job.counts.requirements == 0 { blockers.push("No stable requirements were recorded".into()); },
            Phase::GapReview => {}
            Phase::Verify => {
                for reason in ledger::validate_completion(&self.store, job)? { if blockers.len() < 200 { blockers.push(reason); } }
                match self.vault_for(job).and_then(|v| self.check_publications(job, &v).map_err(|e| err("vault_conflict", e))) {
                    Ok(()) => {}
                    Err(e) if e.starts_with("vault_unavailable") => return Err(e),
                    Err(e) => { if blockers.len() < 200 { blockers.push(e); } }
                }
            }
        }
        let advance = blockers.is_empty();
        let mut next = job.phase;
        let mut complete = false;
        if advance {
            match job.phase.next() { Some(n) => next = n, None => complete = true }
        } else if let Some(low) = lowest {
            if rank(low) < rank(job.phase) { next = low; }
        }
        let now = self.now();
        let mut finished = stage.clone();
        finished.state = "committed".into();
        finished.ended_at = Some(now);
        finished.summary = Some(review.summary.clone());
        let mut writes = vec![mutation("stages", &finished.stage_id, &finished)?];
        job.stage = Some(finished);
        job.phase = next;
        job.checkpoint = Some(Checkpoint { revision: job.revision + 1, at: now, summary: review.summary.clone() });
        if complete {
            let (manifest, hash) = self.manifest(job)?;
            store::atomic(&self.store.dir(&job.job_id)?.join("manifest.json"), &json!({"manifest":manifest,"manifestHash":hash})).map_err(|e| err("storage_error", e))?;
            job.state = JobState::Completed;
            job.error = None;
            job.completion = Some(Completion { manifest_hash: hash, scope_hash: job.scope_hash.clone(), at: now, label: "SRS complete for the recorded evidence scope".into() });
            writes.push(mutation("manifests", &job.job_id, &manifest)?);
        } else if job.counts.blocked > 0 {
            job.state = JobState::WaitingForHuman;
            job.error = Some(Diagnostic { code: "needs_evidence".into(), message: "Blocked work needs readable evidence or an explicit human scope exclusion. It is not proof of no behavior.".into() });
        } else {
            job.state = if next == Phase::Verify { JobState::Reviewing } else { JobState::Running };
        }
        self.save(job, writes, if complete { "Final review validated; SRS complete for the recorded evidence scope" } else { "Stage reviewed" })?;
        Ok(json!({"advanced":advance,"state":job.state,"nextPhase":job.phase,"completed":complete,"reasons":blockers,"revision":job.revision}))
    }
}
