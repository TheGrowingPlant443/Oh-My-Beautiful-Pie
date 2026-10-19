//! Passive, paged reads for the human UI. Nothing here starts, resumes or schedules model work.
use super::*;

impl SrsService {
    pub fn query_srs_job(&self, job_id: &str, section: &str, offset: u64, limit: u64) -> Result<Value> {
        if !(1..=200).contains(&limit) { return Err("invalid_request: limit must be 1..200".into()); }
        let _g = self.gate.lock();
        let job = self.store.job(job_id).map_err(|_| "not_found: Unknown SRS job".to_string())?;
        let wrap = |items: Vec<Value>, total: u64, next: Option<u64>| json!({"jobId":job.job_id,"revision":job.revision,"section":section,"total":total,"offset":offset,"items":items,"nextOffset":next});
        match section {
            "receipts" => {
                let page = self.store.page(job_id, "receipts", offset as usize, limit as usize)?;
                Ok(wrap(page["items"].as_array().cloned().unwrap_or_default(), page["total"].as_u64().unwrap_or(0), page["nextOffset"].as_u64()))
            }
            "stages" | "work" => {
                let table = if section == "stages" { "stages" } else { "work" };
                let page = self.store.page(job_id, table, offset as usize, limit as usize)?;
                let items = page["items"].as_array().cloned().unwrap_or_default().into_iter().map(|mut v| {
                    if section == "stages" {
                        v["number"] = v["number"].clone();
                        if let Some(object) = v.as_object_mut() { object.remove("nonce"); }
                    }
                    v
                }).collect();
                Ok(wrap(items, page["total"].as_u64().unwrap_or(0), page["nextOffset"].as_u64()))
            }
            "questions" => {
                let page = self.store.page(job_id, "questions", offset as usize, limit as usize)?;
                let mut items = Vec::new();
                for value in page["items"].as_array().cloned().unwrap_or_default() {
                    let question: Question = serde_json::from_value(value).map_err(|e| format!("storage_error: {e}"))?;
                    let mut work_ids = Vec::new();
                    self.store.related(job_id, &question.question_id, "work", |id| { if work_ids.len() < 200 { work_ids.push(id.to_string()); } Ok(()) })?;
                    items.push(json!({"questionId":question.question_id,"prompt":question.prompt,"options":question.options,"multi":question.multi,"required":question.required,"state":question.state,"answerReceiptId":question.answer_receipt_id,"live":question.live,"workIds":work_ids}));
                }
                Ok(wrap(items, page["total"].as_u64().unwrap_or(0), page["nextOffset"].as_u64()))
            }
            "documents" => {
                let page = self.store.page(job_id, "documents", offset as usize, limit as usize)?;
                let mut items = Vec::new();
                for value in page["items"].as_array().cloned().unwrap_or_default() {
                    let doc: Document = serde_json::from_value(value).map_err(|e| format!("storage_error: {e}"))?;
                    let committed = doc.state == "committed";
                    items.push(json!({"documentId":doc.document_id,"type":doc.r#type,"title":doc.title,"path":if committed { Some(&doc.path) } else { None },"state":doc.state,"revisionId":doc.revision_id,"hash":doc.hash,"bytes":doc.bytes,"requirementId":doc.requirement_id,"specificity":doc.specificity}));
                }
                Ok(wrap(items, page["total"].as_u64().unwrap_or(0), page["nextOffset"].as_u64()))
            }
            "coverage" => {
                if !job.inventory_complete { return Ok(wrap(Vec::new(), 0, None)); }
                let dir = self.store.dir(job_id)?;
                let page = inventory::page(&dir, offset, limit as usize)?;
                let mut items = Vec::new();
                for item in &page.items {
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
                    items.push(json!({"evidenceId":item.id,"path":item.path,"kind":item.kind,"state":item.state,"bytes":item.bytes,"coveredBytes":covered,"receipts":receipts,"complete":ready && covered >= item.bytes,"rationale":item.rationale,"exclusionSource":item.exclusion_source}));
                }
                Ok(wrap(items, page.total, page.next_offset))
            }
            "activity" => {
                // Newest first. Records are numbered by job revision, so no directory ordering is relied on.
                let total = job.revision;
                let mut items = Vec::new();
                let mut revision = total.saturating_sub(offset);
                while revision > 0 && (items.len() as u64) < limit {
                    if let Ok(record) = self.store.get::<Value>(job_id, "activity", &format!("{revision:020}")) { items.push(record); }
                    revision -= 1;
                }
                let next = offset + (total.saturating_sub(offset) - revision);
                Ok(wrap(items, total, if revision > 0 { Some(next) } else { None }))
            }
            _ => Err("invalid_request: Unknown section".into()),
        }
    }
}
