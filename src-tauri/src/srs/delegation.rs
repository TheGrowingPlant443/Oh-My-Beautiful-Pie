//! Native child authority is a durable subset of one admitted root stage, never a new producer.
use super::types::{Document, Job, Metadata, Stage, Updates, Work};
use super::{inventory, store, tools, Result, SrsService};
use crate::manager::DependencyProducer;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Delegation {
    assignment_id: String,
    child_id: String,
    attempt_id: String,
    stage_id: String,
    assignment: String,
    work_ids: Vec<String>,
    evidence_ids: Vec<String>,
    document_ids: Vec<String>,
    state: String,
    summary: Option<String>,
}
fn denied() -> String {
    tools::err(
        "not_authorized",
        "Native child request exceeds its durable assignment",
    )
}
fn ids(p: &Value, key: &str) -> Result<Vec<String>> {
    let values: Vec<String> = serde_json::from_value(p.get(key).cloned().unwrap_or(Value::Null))
        .map_err(|_| tools::err("invalid_request", format!("{key} must be an array of IDs")))?;
    if values.len() > 200 {
        return Err(tools::err(
            "invalid_request",
            "At most 200 IDs per assignment field",
        ));
    }
    for value in &values {
        store::safe_id(value)?;
    }
    Ok(values)
}
fn subset(values: &[String], allowed: &[String]) -> Result<()> {
    if values.iter().all(|value| allowed.contains(value)) {
        Ok(())
    } else {
        Err(denied())
    }
}
fn authorize_commit_metadata(record: &Delegation, p: &Value) -> Result<()> {
    let metadata: Metadata = serde_json::from_value(p["metadata"].clone())
        .map_err(|e| tools::err("invalid_request", e.to_string()))?;
    subset(&metadata.reviewed_work_ids, &record.work_ids)?;
    subset(&metadata.related_document_ids, &record.document_ids)?;
    for citation in metadata.citations {
        subset(&[citation.evidence_id], &record.evidence_ids)?;
    }
    Ok(())
}
fn authorize_checkpoint(record: &Delegation, p: &Value) -> Result<()> {
    if p.get("idempotencyKey").is_some() {
        return Err(denied());
    }
    let updates: Updates = serde_json::from_value(p["updates"].clone())
        .map_err(|e| tools::err("invalid_request", e.to_string()))?;
    if !updates.add_work.is_empty()
        || !updates.add_documents.is_empty()
        || !updates.add_requirements.is_empty()
        || !updates.add_questions.is_empty()
    {
        return Err(denied());
    }
    for update in updates.update_work {
        subset(&[update.id], &record.work_ids)?;
        subset(&update.evidence_ids, &record.evidence_ids)?;
        if !update.requirement_ids.is_empty() || !update.question_ids.is_empty() {
            return Err(denied());
        }
    }
    Ok(())
}
impl SrsService {
    pub(super) fn delegate_bridge(
        &self,
        producer: &DependencyProducer,
        action: &str,
        p: &Value,
    ) -> Result<Value> {
        let _g = self.gate.lock();
        let (mut job, stage) = if action == "srs_delegate_close" {
            // Ending an already admitted child records lifecycle only, never renewed analysis authority.
            let job = self.store.job(tools::text(p, "jobId")?)?;
            let stage = Self::current_stage(&job, producer, p)?.clone();
            (job, stage)
        } else {
            self.bound(producer, p, false)?
        };
        if p.get("nonce")
            .is_some_and(|nonce| nonce.as_str() != Some(stage.nonce.as_str()))
        {
            return Err(denied());
        }
        match action {
            "srs_delegate_open" => self.delegate_open(&mut job, stage, p),
            "srs_delegate_validate" => {
                tools::only(p, &["jobId", "attemptId", "stageId", "nonce", "delegation"])?;
                self.delegation(&job, p, false)?;
                Ok(json!({"authorized":true}))
            }
            "srs_delegate_close" => self.delegate_close(&mut job, p),
            _ => Err(tools::err("invalid_request", "Unknown delegation action")),
        }
    }
    fn delegate_open(&self, job: &mut Job, stage: Stage, p: &Value) -> Result<Value> {
        tools::only(
            p,
            &[
                "jobId",
                "attemptId",
                "stageId",
                "nonce",
                "childId",
                "assignment",
                "workIds",
                "evidenceIds",
                "documentIds",
            ],
        )?;
        let child_id = tools::text(p, "childId")?;
        store::safe_id(child_id)?;
        let assignment = tools::text(p, "assignment")?;
        if assignment.trim().is_empty() || assignment.len() > 16_384 || assignment.contains('\0') {
            return Err(tools::err(
                "invalid_request",
                "Assignment must be nonblank text of at most 16384 bytes",
            ));
        }
        let work_ids = ids(p, "workIds")?;
        let evidence_ids = ids(p, "evidenceIds")?;
        let document_ids = ids(p, "documentIds")?;
        if work_ids.is_empty() {
            return Err(tools::err(
                "invalid_request",
                "Preallocate durable work for the child's findings",
            ));
        }
        self.check_assigned_records(job, &work_ids, &document_ids, &evidence_ids)?;
        self.check_distinct_assignment(job, &stage, child_id, &work_ids, &document_ids)?;
        let record = Delegation {
            assignment_id: store::id(),
            child_id: child_id.into(),
            attempt_id: stage.attempt_id,
            stage_id: stage.stage_id,
            assignment: assignment.into(),
            work_ids,
            evidence_ids,
            document_ids,
            state: "active".into(),
            summary: None,
        };
        let mut result = serde_json::to_value(&record).map_err(|e| e.to_string())?;
        result["purpose"] = json!(job.scope.purpose);
        result["publicDocumentContracts"] = super::documents::public_contracts();
        let write = store::mutation("delegations", &record.assignment_id, &record)?;
        self.save(
            job,
            vec![write],
            "Native child assignment admitted with bounded authority",
        )?;
        Ok(result)
    }
    fn check_assigned_records(
        &self,
        job: &Job,
        work_ids: &[String],
        document_ids: &[String],
        evidence_ids: &[String],
    ) -> Result<()> {
        for id in work_ids {
            let _: Work = self.store.get(&job.job_id, "work", id)?;
        }
        for id in document_ids {
            let _: Document = self.store.get(&job.job_id, "documents", id)?;
        }
        for id in evidence_ids {
            inventory::read_item(&self.store.dir(&job.job_id)?, id)?;
        }
        Ok(())
    }
    fn check_distinct_assignment(
        &self,
        job: &Job,
        stage: &Stage,
        child_id: &str,
        work_ids: &[String],
        document_ids: &[String],
    ) -> Result<()> {
        self.store.for_each(&job.job_id, "delegations", |value| {
            let existing: Delegation = serde_json::from_value(value).map_err(|e| e.to_string())?;
            let same_stage = existing.state == "active"
                && existing.attempt_id == stage.attempt_id
                && existing.stage_id == stage.stage_id;
            let overlaps = existing.child_id == child_id
                || existing.work_ids.iter().any(|id| work_ids.contains(id))
                || existing
                    .document_ids
                    .iter()
                    .any(|id| document_ids.contains(id));
            if same_stage && overlaps {
                return Err(tools::err(
                    "precondition_failed",
                    "Active native assignments must have distinct children, work and documents",
                ));
            }
            Ok(())
        })
    }
    fn delegate_close(&self, job: &mut Job, p: &Value) -> Result<Value> {
        tools::only(
            p,
            &[
                "jobId",
                "attemptId",
                "stageId",
                "nonce",
                "delegation",
                "outcome",
                "summary",
            ],
        )?;
        let mut record = self.delegation(job, p, true)?;
        let outcome = tools::text(p, "outcome")?;
        if !["completed", "failed", "cancelled"].contains(&outcome) {
            return Err(tools::err("invalid_request", "Unknown child outcome"));
        }
        if record.state != "active" {
            return if record.state == outcome {
                Ok(json!({"persisted":true,"replayed":true}))
            } else {
                Err(denied())
            };
        }
        let summary = p.get("summary").and_then(Value::as_str);
        if summary.is_some_and(|s| s.len() > 16_384 || s.contains('\0')) {
            return Err(tools::err(
                "invalid_request",
                "Child summary exceeds 16384 bytes or contains NUL",
            ));
        }
        record.state = outcome.into();
        record.summary = summary.map(str::to_owned);
        let write = store::mutation("delegations", &record.assignment_id, &record)?;
        self.save(
            job,
            vec![write],
            "Native child ended; durable findings retained for root review",
        )?;
        Ok(json!({"persisted":true}))
    }
    fn delegation(&self, job: &Job, p: &Value, allow_closed: bool) -> Result<Delegation> {
        let actor = p.get("delegation").ok_or_else(denied)?;
        tools::only(actor, &["assignmentId", "childId"])?;
        let id = tools::text(actor, "assignmentId")?;
        store::safe_id(id)?;
        let record: Delegation = self.store.get(&job.job_id, "delegations", id)?;
        let stage = job.stage.as_ref().ok_or_else(denied)?;
        if record.assignment_id != id
            || record.child_id != tools::text(actor, "childId")?
            || record.attempt_id != stage.attempt_id
            || record.stage_id != stage.stage_id
            || (!allow_closed && record.state != "active")
        {
            return Err(denied());
        }
        Ok(record)
    }
    pub(super) fn authorize_delegated(&self, job: &Job, action: &str, p: &Value) -> Result<()> {
        let record = self.delegation(job, p, false)?;
        let contains = |key: &str, allowed: &[String]| -> Result<()> {
            if allowed
                .iter()
                .any(|id| Some(id.as_str()) == p.get(key).and_then(Value::as_str))
            {
                Ok(())
            } else {
                Err(denied())
            }
        };
        match action {
            "srs_read_evidence" => contains("evidenceId", &record.evidence_ids),
            "srs_inspect" => match tools::text(p, "section")? {
                "template" | "checkpoint" => Ok(()),
                "work" => contains("id", &record.work_ids),
                "documents" => contains("id", &record.document_ids),
                "inventory" => contains("id", &record.evidence_ids),
                _ => Err(denied()),
            },
            "srs_write_document_chunk" => contains("documentId", &record.document_ids),
            "srs_commit_document" => {
                contains("documentId", &record.document_ids)?;
                authorize_commit_metadata(&record, p)
            }
            "srs_checkpoint" => authorize_checkpoint(&record, p),
            _ => Err(denied()),
        }?;
        let audit_id = store::id();
        let payload_hash = store::hash(&serde_json::to_vec(p).map_err(|e| e.to_string())?);
        store::atomic(
            &self
                .store
                .path(&job.job_id, "delegation-actions", &audit_id)?,
            &json!({"id":audit_id,"assignmentId":record.assignment_id,"childId":record.child_id,"attemptId":record.attempt_id,"stageId":record.stage_id,"action":action,"jobRevision":job.revision,"payloadHash":payload_hash,"at":self.now(),"state":"authorized"}),
        )
    }
    pub(super) fn has_live_delegations(&self, job: &Job) -> Result<bool> {
        let mut active = false;
        self.store.for_each(&job.job_id, "delegations", |value| {
            let record: Delegation = serde_json::from_value(value).map_err(|e| e.to_string())?;
            active |= record.state == "active"
                && job.stage.as_ref().is_some_and(|stage| {
                    stage.attempt_id == record.attempt_id && stage.stage_id == record.stage_id
                });
            Ok(())
        })?;
        Ok(active)
    }
}
