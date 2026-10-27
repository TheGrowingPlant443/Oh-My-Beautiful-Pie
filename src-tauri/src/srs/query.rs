//! Passive, paged reads for the human UI. Nothing here starts, resumes or schedules model work.
use super::*;

type Page = (Vec<Value>, u64, Option<u64>);

impl SrsService {
    pub fn query_srs_job(&self, job_id: &str, section: &str, offset: u64, limit: u64) -> Result<Value> {
        if !(1..=200).contains(&limit) { return Err("invalid_request: limit must be 1..200".into()); }
        let _g = self.gate.lock();
        let job = self.store.job(job_id).map_err(|_| "not_found: Unknown SRS job".to_string())?;
        let (items, total, next) = match section {
            "receipts" => self.table_page(job_id, "receipts", offset, limit, Ok)?,
            "stages" => self.table_page(job_id, "stages", offset, limit, |mut v| {
                v["number"] = v["number"].clone();
                if let Some(object) = v.as_object_mut() { object.remove("nonce"); }
                Ok(v)
            })?,
            "work" => self.table_page(job_id, "work", offset, limit, Ok)?,
            "questions" => self.table_page(job_id, "questions", offset, limit, |v| self.question_item(job_id, v))?,
            "documents" => self.table_page(job_id, "documents", offset, limit, document_item)?,
            "coverage" => self.coverage_page(&job, offset, limit)?,
            "activity" => self.activity_page(&job, offset, limit)?,
            _ => return Err("invalid_request: Unknown section".into()),
        };
        Ok(json!({"jobId":job.job_id,"revision":job.revision,"section":section,"total":total,"offset":offset,"items":items,"nextOffset":next}))
    }

    fn table_page(&self, job_id: &str, table: &str, offset: u64, limit: u64, map: impl FnMut(Value) -> Result<Value>) -> Result<Page> {
        let page = self.store.page(job_id, table, offset as usize, limit as usize)?;
        let items = page["items"].as_array().cloned().unwrap_or_default().into_iter().map(map).collect::<Result<Vec<_>>>()?;
        Ok((items, page["total"].as_u64().unwrap_or(0), page["nextOffset"].as_u64()))
    }

    fn question_item(&self, job_id: &str, value: Value) -> Result<Value> {
        let question: Question = serde_json::from_value(value).map_err(|e| format!("storage_error: {e}"))?;
        let mut work_ids = Vec::new();
        self.store.related(job_id, &question.question_id, "work", |id| { if work_ids.len() < 200 { work_ids.push(id.to_string()); } Ok(()) })?;
        Ok(json!({"questionId":question.question_id,"prompt":question.prompt,"options":question.options,"multi":question.multi,"required":question.required,"state":question.state,"answerReceiptId":question.answer_receipt_id,"live":question.live,"workIds":work_ids}))
    }

    fn coverage_page(&self, job: &Job, offset: u64, limit: u64) -> Result<Page> {
        if !job.inventory_complete { return Ok((Vec::new(), 0, None)); }
        let page = inventory::page(&self.store.dir(&job.job_id)?, offset, limit as usize)?;
        let mut items = Vec::new();
        for item in &page.items { items.push(self.coverage_item(&job.job_id, item)?); }
        Ok((items, page.total, page.next_offset))
    }

    fn coverage_item(&self, job_id: &str, item: &inventory::InventoryItem) -> Result<Value> {
        let ready = item.state == "ready" && matches!(item.kind.as_str(), "text" | "image");
        let (covered, receipts) = match (ready, item.sha256.as_deref()) {
            (true, Some(hash)) => {
                let (covered, _) = self.covered(job_id, &item.id, item.bytes, hash)?;
                let mut count = 0u64;
                self.store.for_each(job_id, &format!("receipts-{}", item.id), |_| { count += 1; Ok(()) })?;
                (covered, count)
            }
            _ => (0, 0),
        };
        Ok(json!({"evidenceId":item.id,"path":item.path,"kind":item.kind,"state":item.state,"bytes":item.bytes,"coveredBytes":covered,"receipts":receipts,"complete":ready && covered >= item.bytes,"rationale":item.rationale,"exclusionSource":item.exclusion_source}))
    }

    /// Newest first. Records are numbered by job revision, so no directory ordering is relied on.
    fn activity_page(&self, job: &Job, offset: u64, limit: u64) -> Result<Page> {
        let total = job.revision;
        let mut items = Vec::new();
        let mut revision = total.saturating_sub(offset);
        while revision > 0 && (items.len() as u64) < limit {
            if let Ok(record) = self.store.get::<Value>(&job.job_id, "activity", &format!("{revision:020}")) { items.push(record); }
            revision -= 1;
        }
        let next = offset + (total.saturating_sub(offset) - revision);
        Ok((items, total, if revision > 0 { Some(next) } else { None }))
    }
}

fn document_item(value: Value) -> Result<Value> {
    let doc: Document = serde_json::from_value(value).map_err(|e| format!("storage_error: {e}"))?;
    let committed = doc.state == "committed";
    Ok(json!({"documentId":doc.document_id,"type":doc.r#type,"title":doc.title,"path":if committed { Some(&doc.path) } else { None },"state":doc.state,"revisionId":doc.revision_id,"hash":doc.hash,"bytes":doc.bytes,"requirementId":doc.requirement_id,"specificity":doc.specificity}))
}
