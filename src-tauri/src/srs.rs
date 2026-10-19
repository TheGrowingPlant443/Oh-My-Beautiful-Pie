//! Durable, human-admitted SRS jobs. The existing Architect is the only producer; reads never resume generation.
//! Bounded records/chunks are transport limits, never aggregate job budgets. Private journals and evidence stay
//! outside the vault. App close checkpoints and pauses; there is no daemon or detached provider loop.
mod types;
mod store;
mod inventory;
mod documents;
mod ledger;
mod query;
mod questions;
mod scheduler;
mod tools;
#[cfg(test)] mod tests;
pub use types::{Answer,JobState,Phase};
use tools::text;
use types::*;
use store::{Store,mutation};
use parking_lot::Mutex;
use serde_json::{json,Value};
use std::{collections::HashMap,path::{Path,PathBuf},sync::{Arc,OnceLock,Weak,atomic::{AtomicBool,Ordering}},time::Duration};
use crate::{instance::Registry,knowledge::{KnowledgeService,KnowledgeScope},manager::DependencyProducer};
pub type Result<T,E=String> = std::result::Result<T,E>;
pub const MAX_RECORD:u64=1024*1024;
const TOKEN_TTL:u64=600_000;
pub trait Clock:Send+Sync {fn now(&self)->u64;}
struct WallClock;
impl Clock for WallClock {fn now(&self)->u64{crate::dependency_security::now_ms()}}
struct PreparedScope {scope:Scope,expires:u64}
struct PreparedResume {job_id:String,revision:u64,producer:DependencyProducer,mode:String,expires:u64}
pub struct SrsService {
 store:Store, config_root:PathBuf, knowledge:Arc<KnowledgeService>, clock:Arc<dyn Clock>,
 gate:Mutex<()>, scopes:Mutex<HashMap<String,PreparedScope>>, resumes:Mutex<HashMap<String,PreparedResume>>,
 registry:OnceLock<Weak<Registry>>, notifier:OnceLock<Box<dyn Fn(Value)+Send+Sync>>, closing:AtomicBool,
}
#[derive(Clone,Debug)]
pub struct SrsLifecycle {pub kind:String,pub payload:Value}
impl SrsService {
 pub fn new(config_root:PathBuf,knowledge:Arc<KnowledgeService>)->Arc<Self>{Self::with_clock(config_root,knowledge,Arc::new(WallClock))}
 pub fn with_clock(config_root:PathBuf,knowledge:Arc<KnowledgeService>,clock:Arc<dyn Clock>)->Arc<Self>{
  let s=Arc::new(Self{store:Store::new(config_root.join("srs")),config_root,knowledge,clock,gate:Mutex::new(()),scopes:Mutex::new(HashMap::new()),resumes:Mutex::new(HashMap::new()),registry:OnceLock::new(),notifier:OnceLock::new(),closing:AtomicBool::new(false)});
  if let Err(e)=s.recover(){eprintln!("SRS recovery: {e}");}s
 }
 pub fn bind_registry(self:&Arc<Self>,registry:Weak<Registry>)->Result<()> {self.registry.set(registry).map_err(|_|"SRS registry already installed".to_string())?;let weak=Arc::downgrade(self);std::thread::spawn(move||loop{std::thread::sleep(Duration::from_secs(2));let Some(s)=weak.upgrade()else{break};if s.closing.load(Ordering::Acquire){break}if let Err(e)=s.schedule(){eprintln!("SRS scheduler: {e}");}});Ok(())}
 pub fn set_notifier(&self,notifier:Box<dyn Fn(Value)+Send+Sync>)->Result<()> {self.notifier.set(notifier).map_err(|_|"SRS notifier already installed".into())}
 fn registry(&self)->Result<Arc<Registry>>{self.registry.get().and_then(Weak::upgrade).ok_or_else(||"SRS registry unavailable".into())}
 fn now(&self)->u64{self.clock.now()}
 fn recover(&self)->Result<()> {let _g=self.gate.lock();self.store.recover()?;self.store.jobs(|mut j|{if !matches!(j.state,JobState::Completed|JobState::Cancelled|JobState::Paused){j.state=JobState::Interrupted;j.error=Some(Diagnostic{code:"app_closed".into(),message:"The previous app session ended. Review the saved checkpoint and explicitly Resume.".into()});self.save(&mut j,vec![],"Recovered interrupted job")?;}self.store.for_each(&j.job_id,"questions",|v|{let mut q:Question=serde_json::from_value(v).map_err(|e|e.to_string())?;if q.live{q.live=false;q.invocation_id=None;if q.state=="asked"{q.state="open".into()}store::atomic(&self.store.path(&j.job_id,"questions",&q.question_id)?,&q)?;}Ok(())})})}
 fn save(&self,j:&mut Job,writes:Vec<store::Mutation>,summary:&str)->Result<()> {j.revision=j.revision.checked_add(1).ok_or("storage_error: Revision counter exhausted")?;j.updated_at=self.now();self.store.commit(j,writes,json!({"revision":j.revision,"at":j.updated_at,"summary":summary}))?;self.emit(j);Ok(())}
 fn emit(&self,j:&Job){if let Some(notify)=self.notifier.get(){if let Ok(v)=self.status_value(j){notify(v)}}}
 fn status_value(&self,j:&Job)->Result<Value>{let vault=self.knowledge.vault_root(&j.scope.vault_id);let same= vault.as_ref().is_ok_and(|v|same_path(&v.root,Path::new(&j.scope.vault_root)));let resume=if matches!(j.state,JobState::Running|JobState::Reviewing|JobState::Inventory|JobState::Completed|JobState::Cancelled){"none"}else{let current=self.registry().ok().and_then(|r|r.manager_hub().and_then(|h|j.producer.as_ref().map(|p|h.dependency_producer_live(p))));if current==Some(false){"currentConversationFromCheckpoints"}else{"sameConversation"}};
 Ok(json!({"jobId":j.job_id,"folder":j.scope.folder,"repositoryRoot":j.scope.repository_root,"revision":j.revision,"state":j.state,"phase":j.phase,"vaultId":j.scope.vault_id,"vaultMode":j.scope.vault_mode,"vaultAvailable":same,"outputPath":format!("SRS/{}",j.job_id),"masterPath":format!("SRS/{}/SRS.md",j.job_id),"template":j.scope.template,"scopeHash":j.scope_hash,"sourceHash":j.source_hash,"attemptId":j.attempt_id,"stageId":j.stage.as_ref().map(|s|&s.stage_id),"stage":j.stage,"producer":j.producer,"counts":j.counts,"checkpoint":j.checkpoint,"error":j.error,"usage":{"tokens":null,"costUsd":null},"createdAt":j.created_at,"updatedAt":j.updated_at,"completion":j.completion,"resumeRequired":resume,"humanExclusions":j.human_exclusions,"architectModel":null}))}
 pub fn get_srs_job(&self,job_id:&str)->Result<Value>{let _g=self.gate.lock();self.status_value(&self.store.job(job_id)?)}
 pub fn list_srs_jobs(&self,folder:&str,offset:usize,limit:usize)->Result<Value>{if !(1..=200).contains(&limit){return Err("Page size must be 1..200".into())}let root=crate::setup::find_repository_root(Path::new(folder))?;let _g=self.gate.lock();let mut items=Vec::<Job>::new();let mut total=0;self.store.jobs(|j|{if same_path(Path::new(&j.scope.repository_root),&root){total+=1;items.push(j);items.sort_by(|a,b|(b.created_at,&b.job_id).cmp(&(a.created_at,&a.job_id)));if items.len()>offset.saturating_add(limit){items.pop();}}Ok(())})?;let mut values=Vec::new();for j in items.into_iter().skip(offset){values.push(self.status_value(&j)?)}let next=offset+values.len();Ok(json!({"items":values,"offset":offset,"total":total,"nextOffset":if next<total{Some(next)}else{None}}))}
 pub fn prepare_srs_scope(&self,folder:&str,vault_id:&str,evidence_folders:Vec<String>,exclusions:Vec<String>)->Result<Value>{
  if evidence_folders.len()>200||exclusions.len()>200{return Err("A scope preparation admits at most 200 explicit inputs per message".into())}let repository=crate::setup::find_repository_root(Path::new(folder))?;safe_root(&repository)?;let vault=self.knowledge.vault_root(vault_id)?;match &vault.scope{KnowledgeScope::Project{repository_root}if same_path(Path::new(repository_root),&repository)=>{},_=>return Err("Select this project's explicitly bound vault".into())}
  let mut roots=vec![Root{path:repository.display().to_string(),kind:"repository".into(),rationale:"Human-selected project repository".into()}];for value in evidence_folders{let p=crate::setup::canonical(Path::new(&value)).map_err(|e|e.to_string())?;safe_root(&p)?;if same_path(&p,&self.config_root)||p.starts_with(&self.config_root)||same_path(&p,&vault.root)||p.starts_with(&vault.root){return Err("App metadata and managed output cannot be evidence roots".into())}if roots.iter().any(|r|p.starts_with(&r.path)||Path::new(&r.path).starts_with(&p)){return Err("Evidence roots may not overlap".into())}roots.push(Root{path:p.display().to_string(),kind:"evidence".into(),rationale:"Supplementary folder explicitly admitted by the human".into()});}
  let mut excluded=Vec::new();for value in exclusions{let requested=Path::new(&value);let p=if requested.is_absolute(){requested.to_path_buf()}else{repository.join(requested)};let p=crate::setup::canonical(&p).map_err(|e|format!("Cannot resolve exclusion: {e}"))?;if !roots.iter().any(|r|p.starts_with(&r.path)&&!same_path(&p,Path::new(&r.path))){return Err("Exclusions must be inside an admitted root, not the entire root".into())}excluded.push(Exclusion{path:p.display().to_string(),reason:"Explicit human scope exclusion".into(),source:"human".into()});}
  excluded.push(Exclusion{path:vault.root.display().to_string(),reason:"Vault notes and generated SRS output are not primary evidence".into(),source:"app".into()});excluded.push(Exclusion{path:self.config_root.display().to_string(),reason:"Private application state, sessions and credentials".into(),source:"app".into()});
  let template=serde_json::to_value(documents::template_manifest()).map_err(|e|e.to_string())?;let scope=Scope{folder:repository.display().to_string(),repository_root:repository.display().to_string(),vault_id:vault_id.into(),vault_root:vault.root.display().to_string(),vault_mode:vault.mode,roots,exclusions:excluded,template};let token=store::id();let expires=self.now()+TOKEN_TTL;let result=json!({"token":token,"expiresAt":expires,"folder":scope.folder,"repositoryRoot":scope.repository_root,"vaultId":scope.vault_id,"vaultRoot":scope.vault_root,"vaultMode":scope.vault_mode,"roots":scope.roots,"exclusions":scope.exclusions,"template":scope.template,"warnings":["No total document/token/time/iteration limit. Provider charges may be very large. Local models still require time and compute.","Only the recorded evidence scope is covered; inaccessible production behavior is not proven.","Closing OMP Manager checkpoints and pauses; there is no background service."]});let mut pending=self.scopes.lock();pending.retain(|_,s|s.expires>self.now());pending.insert(token,PreparedScope{scope,expires});Ok(result)
 }
 pub fn start_srs(self:&Arc<Self>,scope_token:&str)->Result<Value>{let pending=self.scopes.lock().remove(scope_token).ok_or("Scope token expired or consumed; review scope again")?;if self.now()>pending.expires{return Err("Scope token expired; review scope again".into())}let scope=pending.scope;let vault=self.knowledge.vault_root(&scope.vault_id)?;if !same_path(&vault.root,Path::new(&scope.vault_root)){return Err("Vault binding changed; review destination again".into())}for r in &scope.roots{safe_root(Path::new(&r.path))?;}
  let registry=self.registry()?;
  let producer=crate::dependency_security::ensure_producer(&registry)?;
  let gate=self.gate.lock();
  self.ensure_available(&producer,None)?;
  let now=self.now();
  let scope_hash=store::hash(&serde_json::to_vec(&scope).map_err(|e|e.to_string())?);
  let human_exclusions=scope.exclusions.iter().filter(|e|e.source=="human").count() as u64;
  let mut j=Job{job_id:store::id(),scope,revision:0,state:JobState::Inventory,phase:Phase::Survey,scope_hash,source_hash:None,attempt_id:Some(store::id()),producer:Some(producer),stage:None,next_stage:1,next_requirement:0,counts:Counts::default(),checkpoint:None,error:None,created_at:now,updated_at:now,completion:None,inventory_complete:false,human_exclusions,document_revision:0,work_revision:0};
  documents::materialize_templates(&self.store.dir(&j.job_id)?)?;
  self.save(&mut j,vec![],"Human started SRS; inventory admitted")?;
  let result=self.status_value(&j)?;
  let id=j.job_id.clone();
  drop(gate);
  self.spawn_inventory(id);
  Ok(result)
 }
 fn ensure_available(&self,producer:&DependencyProducer,except:Option<&str>)->Result<()> {self.store.jobs(|j|{if Some(j.job_id.as_str())!=except&&matches!(j.state,JobState::Inventory|JobState::Running|JobState::Reviewing|JobState::WaitingForHuman|JobState::Uncertain)&&j.producer.as_ref().is_some_and(|p|p.instance_id==producer.instance_id){return Err("Pause the existing SRS job before starting another analysis in the Architect".into())}Ok(())})}
 fn inventory_args(&self,j:&Job)->(Vec<inventory::InventoryRoot>,Vec<inventory::InventoryExclusion>,Vec<PathBuf>){(j.scope.roots.iter().map(|r|inventory::InventoryRoot{path:r.path.clone().into(),kind:r.kind.clone(),rationale:r.rationale.clone()}).collect(),j.scope.exclusions.iter().map(|e|inventory::InventoryExclusion{path:e.path.clone().into(),reason:e.reason.clone(),source:e.source.clone()}).collect(),vec![self.config_root.clone(),PathBuf::from(&j.scope.vault_root)])}
 fn spawn_inventory(self:&Arc<Self>,id:String){let s=self.clone();std::thread::spawn(move||{if let Err(e)=s.build_inventory(&id){let _g=s.gate.lock();if let Ok(mut j)=s.store.job(&id){if matches!(j.state,JobState::Inventory){j.state=JobState::Paused;j.error=Some(Diagnostic{code:"storage_error".into(),message:e});let _=s.save(&mut j,vec![],"Inventory paused");}}}});}
 fn build_inventory(&self,id:&str)->Result<()> {
  let j={let _g=self.gate.lock();self.store.job(id)?};
  let(roots,exclusions,protected)=self.inventory_args(&j);let dir=self.store.dir(id)?;
  // A paused or interrupted inventory is discarded and rebuilt from the live scope; immutable evidence is never mixed.
  if dir.join("inventory").exists(){std::fs::remove_dir_all(dir.join("inventory")).map_err(|e|format!("storage_error: {e}"))?;}
  let summary=inventory::inventory(&dir,&roots,&exclusions,&protected,&||self.closing.load(Ordering::Acquire)||self.store.job(id).is_ok_and(|j|j.state!=JobState::Inventory))?;
  let _g=self.gate.lock();let mut j=self.store.job(id)?;if j.state!=JobState::Inventory{return Ok(())}
  j.source_hash=Some(summary.source_hash.clone());j.counts.areas=summary.areas;j.counts.evidence_files=summary.files;j.counts.excluded=summary.excluded;
  // One survey obligation per admitted area and per blocking record; individual files are covered by read receipts.
  // Reconcile records even if the previous process died between writing a work record and its relation/counters.
  inventory::for_each(&dir,&mut |item|{
   if item.state=="excluded"||!(item.kind=="area"||item.state=="blocked"){return Ok(())}
   let work_id=format!("source-{}",item.id);
   if !self.store.path(id,"work",&work_id)?.exists(){
    let status=if item.state=="blocked"{"blocked"}else{"pending"};
    let w=Work{id:work_id.clone(),parent_id:None,phase:Phase::Survey,kind:"sourceArea".into(),status:status.into(),rationale:format!("Recorded evidence: {}",item.path),revision:0};
    store::atomic(&self.store.path(id,"work",&work_id)?,&w)?;
   }
   // A prior write may have reached the work record but not this link; always repair it.
   if let Some(r)=self.store.relation(id,&work_id,"evidence",&item.id)?{store::atomic(&self.store.path(id,&r.table,&r.id)?,&r.value)?;}
   Ok(())})?;
  for section in ["Introduction","Product Overview","External Interfaces","Functional","QoS","Compliance","Design/Implementation","AI/ML","Verification","Appendixes"]{
   let wid=format!("template-{}",store::hash(section.as_bytes()));
   if !self.store.path(id,"work",&wid)?.exists(){let w=Work{id:wid.clone(),parent_id:None,phase:Phase::Verify,kind:"templateSection".into(),status:"pending".into(),rationale:section.into(),revision:0};store::atomic(&self.store.path(id,"work",&wid)?,&w)?;}
  }
  // No model work can be admitted before inventoryComplete. If the source scope changed during an incomplete
  // inventory, retire obsolete generated source obligations rather than retain dangling evidence references.
  self.store.for_each(id,"work",|value|{
   let w:Work=serde_json::from_value(value).map_err(|e|e.to_string())?;
   if let Some(evidence)=w.id.strip_prefix("source-"){
    if w.kind=="sourceArea"&&inventory::read_item(&dir,evidence).is_err(){
     std::fs::remove_file(self.store.path(id,"work",&w.id)?).map_err(|e|e.to_string())?;
     let relations=dir.join(format!("rel-{}-evidence",w.id));
     store::plain(&relations)?;
     if relations.exists(){std::fs::remove_dir_all(relations).map_err(|e|e.to_string())?;}
    }
   }
   Ok(())
  })?;
  j.counts.work=0;j.counts.work_open=0;j.counts.blocked=0;
  let mut work_exclusions=0;
  self.store.for_each(id,"work",|value|{
   let w:Work=serde_json::from_value(value).map_err(|e|e.to_string())?;
   j.counts.work+=1;
   if !matches!(w.status.as_str(),"reviewed"|"notApplicable"|"excluded"){j.counts.work_open+=1;}
   if w.status=="blocked"{j.counts.blocked+=1;}
   if w.status=="excluded"{work_exclusions+=1;}
   Ok(())
  })?;
  j.counts.excluded=summary.excluded+work_exclusions;
  let blocked=j.counts.blocked;
  j.inventory_complete=true;
  j.state=if blocked>0{JobState::WaitingForHuman}else{JobState::Running};
  j.error=if blocked>0{Some(Diagnostic{code:"needs_evidence".into(),message:"Unreadable or unsupported evidence needs a readable supply or explicit human exclusion. It is not proof of no behavior.".into()})}else{None};
  self.save(&mut j,vec![],"Streaming evidence inventory committed")
 }
 pub fn pause_srs(&self,job_id:&str)->Result<Value>{self.stop(job_id,false,"pause")}
 pub fn cancel_srs(&self,job_id:&str)->Result<Value>{self.stop(job_id,true,"cancel")}
 fn stop(&self,id:&str,cancel:bool,reason:&str)->Result<Value>{
  let(producer,stage,result)={
   let _g=self.gate.lock();let mut j=self.store.job(id)?;
   if matches!(j.state,JobState::Completed|JobState::Cancelled){return self.status_value(&j)}
   let live=j.stage.clone().filter(|s|!matches!(s.state.as_str(),"committed"|"interrupted"));
   let writes=tools::interrupt_stage(&mut j)?;
   j.state=if cancel{JobState::Cancelled}else{JobState::Paused};
   self.save(&mut j,writes,if cancel{"Human cancelled SRS"}else{"Human paused SRS"})?;
   (j.producer.clone(),live,self.status_value(&j)?)
  };
  if let(Some(p),Some(stage))=(producer,stage){if let Ok(r)=self.registry(){if let Some(h)=r.manager_hub(){let _=h.srs_control(&p,"srs_pause",json!({"jobId":id,"attemptId":stage.attempt_id,"stageId":stage.stage_id,"reason":reason}));}}}
  Ok(result)
 }
 pub fn shutdown(&self){self.closing.store(true,Ordering::Release);let mut ids=Vec::new();{let _g=self.gate.lock();let _=self.store.jobs(|j|{if matches!(j.state,JobState::Inventory|JobState::Running|JobState::Reviewing|JobState::WaitingForHuman|JobState::Uncertain){ids.push(j.job_id)}Ok(())});}for id in ids{let _=self.stop(&id,false,"close");}}
 pub fn prepare_srs_resume(&self,id:&str,mode:&str)->Result<Value>{if !["sameConversation","currentConversationFromCheckpoints"].contains(&mode){return Err("Invalid resume mode".into())}let registry=self.registry()?;let producer=crate::dependency_security::ensure_producer(&registry)?;let _g=self.gate.lock();let j=self.store.job(id)?;if !matches!(j.state,JobState::Paused|JobState::Interrupted|JobState::Uncertain|JobState::WaitingForHuman){return Err("This job does not require Resume".into())}if mode=="sameConversation"&&j.producer.as_ref().is_some_and(|p|p.session_id!=producer.session_id||p.instance_id!=producer.instance_id){return Err("The native conversation was replaced. Explicitly Continue from saved checkpoints in the current Architect.".into())}self.ensure_available(&producer,Some(id))?;let token=store::id();let expires=self.now()+TOKEN_TTL;self.resumes.lock().insert(token.clone(),PreparedResume{job_id:id.into(),revision:j.revision,producer,mode:mode.into(),expires});Ok(json!({"token":token,"expiresAt":expires,"jobId":id,"mode":mode,"summary":j.checkpoint.as_ref().map(|c|&c.summary),"warnings":["Resume revalidates source, vault, template and conversation identity. Committed documents are not replayed."]}))}
 pub fn resume_srs(self:&Arc<Self>,token:&str)->Result<Value>{let prep=self.resumes.lock().remove(token).ok_or("Resume token expired or consumed")?;if self.now()>prep.expires{return Err("Resume token expired".into())}let j={let _g=self.gate.lock();let j=self.store.job(&prep.job_id)?;if j.revision!=prep.revision{return Err("revision_conflict: Review the newer checkpoint".into())}j};let vault=self.knowledge.vault_root(&j.scope.vault_id)?;if !same_path(&vault.root,Path::new(&j.scope.vault_root)){return Err("vault_unavailable: Locate the original vault first".into())}self.validate_publications(&j)?;if j.inventory_complete{let(r,e,p)=self.inventory_args(&j);inventory::validate_sources(&self.store.dir(&j.job_id)?,&r,&e,&p,&||self.closing.load(Ordering::Acquire))?;}let registry=self.registry()?;let hub=registry.manager_hub().ok_or("Bridge unavailable")?;if !hub.dependency_producer_live(&prep.producer){return Err("producer_replaced: Resume preparation is stale".into())}
 let _g=self.gate.lock();let mut j=self.store.job(&prep.job_id)?;if j.revision!=prep.revision{return Err("revision_conflict: Checkpoint changed during validation".into())}self.ensure_available(&prep.producer,Some(&j.job_id))?;if prep.mode=="sameConversation"&&j.producer.as_ref().is_some_and(|p|p.session_id!=prep.producer.session_id){return Err("producer_replaced".into())}j.attempt_id=Some(store::id());j.producer=Some(prep.producer);j.stage=None;j.error=None;j.state=if j.inventory_complete{JobState::Running}else{JobState::Inventory};self.save(&mut j,vec![],"Human explicitly resumed from saved checkpoints")?;let result=self.status_value(&j)?;if !j.inventory_complete{let id=j.job_id.clone();drop(_g);self.spawn_inventory(id);}Ok(result)}
 pub fn open_srs_documents(&self,id:&str)->Result<Value>{let _g=self.gate.lock();let j=self.store.job(id)?;let vault=self.knowledge.vault_root(&j.scope.vault_id)?;if !same_path(&vault.root,Path::new(&j.scope.vault_root)){return Err("Locate the recorded vault first".into())}let master=format!("SRS/{id}/SRS.md");let path=if vault.read_markdown(&master).is_ok(){master}else{format!("SRS/{id}")};Ok(json!({"vaultId":j.scope.vault_id,"vaultMode":j.scope.vault_mode,"path":path,"completed":j.state==JobState::Completed,"draft":j.state!=JobState::Completed}))}
 pub fn rebind_srs_vault(&self,id:&str,vault_id:&str)->Result<Value>{let _g=self.gate.lock();let mut j=self.store.job(id)?;if matches!(j.state,JobState::Running|JobState::Inventory|JobState::Reviewing){return Err("Pause before locating a vault".into())}let v=self.knowledge.vault_root(vault_id)?;match &v.scope{KnowledgeScope::Project{repository_root}if same_path(Path::new(repository_root),Path::new(&j.scope.repository_root))=>{},_=>return Err("Replacement must be this project's human-registered vault".into())}self.check_publications(&j,&v)?;j.scope.vault_id=vault_id.into();j.scope.vault_root=v.root.display().to_string();j.scope.vault_mode=v.mode;self.save(&mut j,vec![],"Human located a hash-matching vault")?;self.status_value(&j)}
 fn validate_publications(&self,j:&Job)->Result<()> {let v=self.knowledge.vault_root(&j.scope.vault_id)?;self.check_publications(j,&v)}
 fn check_publications(&self,j:&Job,v:&crate::knowledge::VaultRoot)->Result<()> {self.store.for_each(&j.job_id,"documents",|value|{let d:Document=serde_json::from_value(value).map_err(|e|e.to_string())?;if let Some(h)=d.hash {let bytes=v.read_markdown(&format!("SRS/{}/{}",j.job_id,d.path))?;if store::hash(&bytes)!=h{return Err(format!("vault_conflict: {} was changed or moved; review the note before continuing",d.title))}}Ok(())})}
 pub fn on_architect_lifecycle(&self,producer:&DependencyProducer,event:SrsLifecycle){
  let _g=self.gate.lock();
  let _=self.store.jobs(|mut j|{
   if j.producer.as_ref()!=Some(producer)||!matches!(j.state,JobState::Running|JobState::Reviewing|JobState::Inventory|JobState::WaitingForHuman|JobState::Uncertain){return Ok(())}
   let live=j.stage.as_ref().is_some_and(|s|!matches!(s.state.as_str(),"committed"|"interrupted"));
   match event.kind.as_str(){
    "disconnect"|"exit"|"session_changed"=>{let writes=tools::interrupt_stage(&mut j)?;j.state=JobState::Interrupted;j.error=Some(Diagnostic{code:"producer_ended".into(),message:"Architect connection/session changed. Inspect the saved checkpoint and explicitly Resume; no stage was resent.".into()});self.save(&mut j,writes,"Architect lifecycle interrupted the attempt")?}
    "turn_error"|"turn_aborted" if live=>{let writes=tools::interrupt_stage(&mut j)?;j.state=JobState::Paused;j.error=Some(Diagnostic{code:"provider_error".into(),message:event.payload.to_string()});self.save(&mut j,writes,"Architect turn stopped; checkpoint retained")?}
    _=>{}
   }
   Ok(())
  });
 }
}
fn same_path(a:&Path,b:&Path)->bool{let a=a.to_string_lossy().replace('\\',"/").trim_start_matches("//?/").trim_end_matches('/').to_string();let b=b.to_string_lossy().replace('\\',"/").trim_start_matches("//?/").trim_end_matches('/').to_string();if cfg!(windows){a.eq_ignore_ascii_case(&b)}else{a==b}}
fn safe_root(p:&Path)->Result<()> {store::plain(p)?;let m=std::fs::symlink_metadata(p).map_err(|e|e.to_string())?;if !p.is_absolute()||p.parent().is_none()||!m.is_dir(){return Err("Choose a regular project/evidence folder, not a filesystem root".into())}Ok(())}
