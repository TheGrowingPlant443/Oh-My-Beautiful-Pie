use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::manager::DependencyProducer;

#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Root { pub path:String, pub kind:String, pub rationale:String }
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Exclusion { pub path:String, pub reason:String, pub source:String }
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Scope { pub folder:String, pub repository_root:String, pub vault_id:String, pub vault_root:String, pub vault_mode:crate::knowledge::VaultMode, pub roots:Vec<Root>, pub exclusions:Vec<Exclusion>, pub template:Value }
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(rename_all="camelCase")]
pub enum JobState { Inventory, Running, WaitingForHuman, Paused, Interrupted, Uncertain, Reviewing, Completed, Cancelled }
#[derive(Clone,Copy,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(rename_all="camelCase")]
pub enum Phase { Survey, Specify, GapReview, Verify }
impl Phase { pub fn next(self)->Option<Self>{ match self {Self::Survey=>Some(Self::Specify),Self::Specify=>Some(Self::GapReview),Self::GapReview=>Some(Self::Verify),Self::Verify=>None} } }
#[derive(Clone,Debug,Default,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Counts {pub areas:u64,pub evidence_files:u64,pub work:u64,pub work_open:u64,pub documents:u64,pub requirements:u64,pub questions:u64,pub questions_open:u64,pub blocked:u64,pub excluded:u64}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Checkpoint { pub revision:u64,pub at:u64,pub summary:String }
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Diagnostic {pub code:String,pub message:String}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Completion {pub manifest_hash:String,pub scope_hash:String,pub at:u64,pub label:String}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Stage { pub stage_id:String,pub number:u64,pub phase:Phase,pub state:String,pub attempt_id:String,pub nonce:String,pub started_at:u64,pub ended_at:Option<u64>,pub summary:Option<String>,pub admitted:bool }
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Job {pub job_id:String,pub scope:Scope,pub revision:u64,pub state:JobState,pub phase:Phase,pub scope_hash:String,pub source_hash:Option<String>,pub attempt_id:Option<String>,pub producer:Option<DependencyProducer>,pub stage:Option<Stage>,pub next_stage:u64,pub next_requirement:u64,pub counts:Counts,pub checkpoint:Option<Checkpoint>,pub error:Option<Diagnostic>,pub created_at:u64,pub updated_at:u64,pub completion:Option<Completion>,pub inventory_complete:bool,pub human_exclusions:u64,pub document_revision:u64,pub work_revision:u64}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Work {pub id:String,pub parent_id:Option<String>,pub phase:Phase,pub kind:String,pub status:String,pub rationale:String,pub revision:u64}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Document {pub document_id:String,pub r#type:String,pub title:String,pub path:String,pub state:String,pub revision_id:String,pub hash:Option<String>,pub bytes:u64,pub requirement_id:Option<String>,pub specificity:Option<String>,pub reviewed_revision:Option<u64>,pub parent_document_id:Option<String>}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Requirement {pub id:String,pub area:String,pub title:String,pub document_id:String,pub revision:u64}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct QuestionOption {pub label:String,#[serde(default,skip_serializing_if="Option::is_none")]pub description:Option<String>}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Question {pub question_id:String,pub prompt:String,pub options:Vec<QuestionOption>,pub multi:bool,pub required:bool,pub state:String,pub answer_receipt_id:Option<String>,pub live:bool,pub invocation_id:Option<String>}
#[derive(Clone,Debug,Deserialize,Serialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Answer { pub selected_options:Vec<String>,#[serde(default)]pub custom_input:Option<String>,#[serde(default)]pub note:Option<String> }
#[derive(Clone,Debug,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct AddWork {#[serde(default)]pub r#ref:Option<String>,#[serde(default)]pub parent_id:Option<String>,pub phase:Phase,pub kind:String,#[serde(default)]pub status:Option<String>,#[serde(default)]pub evidence_ids:Vec<String>,#[serde(default)]pub requirement_ids:Vec<String>,#[serde(default)]pub question_ids:Vec<String>,pub rationale:String}
#[derive(Clone,Debug,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct UpdateWork {pub id:String,#[serde(default)]pub status:Option<String>,#[serde(default)]pub evidence_ids:Vec<String>,#[serde(default)]pub requirement_ids:Vec<String>,#[serde(default)]pub question_ids:Vec<String>,#[serde(default)]pub rationale:Option<String>}
#[derive(Clone,Debug,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct AddDocument {#[serde(default)]pub r#ref:Option<String>,pub r#type:String,pub title:String,#[serde(default)]pub parent_document_id:Option<String>,#[serde(default)]pub requirement_id:Option<String>}
#[derive(Clone,Debug,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct AddRequirement {#[serde(default)]pub r#ref:Option<String>,pub area:String,pub title:String,#[serde(default)]pub work_ids:Vec<String>,#[serde(default)]pub evidence_ids:Vec<String>}
#[derive(Clone,Debug,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct AddQuestion {#[serde(default)]pub r#ref:Option<String>,#[serde(default)]pub work_ids:Vec<String>,pub prompt:String,#[serde(default)]pub options:Vec<QuestionOption>,#[serde(default)]pub multi:bool,#[serde(default="yes")]pub required:bool}
fn yes()->bool{true}
#[derive(Clone,Debug,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Updates {#[serde(default)]pub add_work:Vec<AddWork>,#[serde(default)]pub update_work:Vec<UpdateWork>,#[serde(default)]pub add_documents:Vec<AddDocument>,#[serde(default)]pub add_requirements:Vec<AddRequirement>,#[serde(default)]pub add_questions:Vec<AddQuestion>,#[serde(default)]pub summary:Option<String>}
#[derive(Clone,Debug,Deserialize,Serialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Citation {pub evidence_id:String,#[serde(default)]pub start_offset:Option<u64>,#[serde(default)]pub end_offset:Option<u64>}
#[derive(Clone,Debug,Deserialize,Serialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Metadata {pub r#type:String,#[serde(default)]pub title:Option<String>,#[serde(default)]pub requirement_id:Option<String>,#[serde(default)]pub related_document_ids:Vec<String>,pub specificity:String,#[serde(default)]pub reviewed_work_ids:Vec<String>,#[serde(default)]pub citations:Vec<Citation>}
#[derive(Clone,Debug,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Review {pub phase:Phase,#[serde(default)]pub document_revision:Option<u64>,#[serde(default)]pub work_revision:Option<u64>,#[serde(default)]pub gap_ids:Vec<String>,#[serde(default)]pub unresolved_question_ids:Vec<String>,#[serde(default)]pub final_declaration:Option<String>,pub summary:String}
