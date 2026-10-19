//! Reviewed Markdown sources and recoverable native deployments. No executable/config asset is admitted.
//! Private mappings, immutable backups and journals live outside the human-selected vault.
use crate::instance::Registry;
use crate::knowledge::{fsio, KnowledgeScope, KnowledgeService, VaultRoot};
use crate::setup::{self, SetupPaths, SetupRequest, SetupScope};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use parking_lot::{Mutex, RwLock, RwLockReadGuard};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
type Result<T, E = String> = std::result::Result<T, E>;
const MAX_FILE: u64 = 8 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(600);
static ADMISSION: RwLock<()> = RwLock::new(());
thread_local! { static READ_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) }; }
/// Shared instruction-read gate. Re-entrant on one thread (`read_recursive`), so a queued Apply writer cannot deadlock a
/// nested admission; `nested()` tells callers to skip recovery (which needs the exclusive gate) while already inside one.
pub struct ReadAdmission { _guard: RwLockReadGuard<'static, ()> }
impl Drop for ReadAdmission { fn drop(&mut self) { READ_DEPTH.with(|d| d.set(d.get().saturating_sub(1))); } }
pub fn nested() -> bool { READ_DEPTH.with(|d| d.get() > 0) }
pub fn read_admission() -> Result<ReadAdmission> {
    let guard = ADMISSION.read_recursive();
    READ_DEPTH.with(|d| d.set(d.get() + 1));
    Ok(ReadAdmission { _guard: guard })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag="kind", rename_all="camelCase", rename_all_fields="camelCase", deny_unknown_fields)]
pub enum KnowledgeAssetTarget { Global, Manager, Architect, Repository { project_instance_id: String }, Profile { profile_name: String } }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct MarkdownItem {
    pub id: String, pub native_path: String, pub vault_path: String,
    pub source_hash: Option<String>, pub native_hash: Option<String>, pub applied_hash: Option<String>,
    pub state: String, pub diff: String, pub native_dependencies: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all="camelCase")]
pub struct MarkdownStatus {
    pub target: KnowledgeAssetTarget, pub vault_id: Option<String>, pub namespace: String, pub native_root: String,
    pub source_root: Option<String>, pub enabled: bool, pub source_revision: String, pub applied_revision: Option<String>,
    pub items: Vec<MarkdownItem>, pub warnings: Vec<String>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all="camelCase")]
pub struct MarkdownPreparation { pub token: String, pub target: KnowledgeAssetTarget, pub source_revision: String, pub items: Vec<MarkdownItem>, pub selected_ids: Vec<String>, pub warnings: Vec<String> }
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
struct Baseline {
    enabled: bool, vault_id: String, native_root: String, applied_revision: Option<String>, hashes: BTreeMap<String, String>,
    #[serde(default)] allowed_paths: BTreeSet<String>,
    #[serde(default)] allowed_prefixes: BTreeSet<String>,
}
#[derive(Clone)]
struct Resolved { target: KnowledgeAssetTarget, key: String, native: PathBuf, native_identity: Option<String>, namespace: String, vault: Option<VaultRoot> }
struct Pending { resolved: Resolved, expires: Instant, import: bool, revision: String, selected: Vec<String>, items: Vec<MarkdownItem> }
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all="camelCase", deny_unknown_fields)]
struct Operation { relative: String, before: Option<String>, after: Option<String>, backup: Option<String> }
#[derive(Serialize, Deserialize)]
#[serde(rename_all="camelCase", deny_unknown_fields)]
struct Journal { id: String, state: String, key: String, target: KnowledgeAssetTarget, root: PathBuf, root_identity: Option<String>, namespace: Option<String>, operations: Vec<Operation>, baseline: Baseline }
/// The host acquires all affected captured-native idle holds before returning this guard. Drop releases them.
/// Missing native capability must fail closed, never fall back to an idle-status observation.
pub trait MarkdownBoundary: Send + Sync { fn hold(&self, transaction_id: &str, target: &KnowledgeAssetTarget) -> Result<Box<dyn Send>>; }
pub struct AgentMarkdownService {
    dir: PathBuf, knowledge: Arc<KnowledgeService>, pending: Mutex<HashMap<String, Pending>>, serial: Mutex<()>,
    boundary: OnceLock<Arc<dyn MarkdownBoundary>>,
}

fn target_name(target: &KnowledgeAssetTarget) -> Result<String> { Ok(match target {
    KnowledgeAssetTarget::Global => "Global".into(), KnowledgeAssetTarget::Manager => "Manager".into(), KnowledgeAssetTarget::Architect => "Architect".into(),
    KnowledgeAssetTarget::Repository { .. } => "Repository".into(),
    KnowledgeAssetTarget::Profile { profile_name } => { if profile_name == "default" || !setup::valid_profile_name(profile_name) { return Err("Invalid named OMP profile".into()); } format!("Profiles/{profile_name}") }
}) }
/// Only native discovery namespaces are deployable. Notes/frontmatter can never grant another destination.
fn allowed(path: &str) -> bool {
    let Ok(parts) = fsio::validate_relative(path) else { return false };
    if !parts.last().is_some_and(|p| fsio::is_markdown_name(p)) { return false; }
    if parts.iter().any(|p| matches!(p.to_ascii_lowercase().as_str(), "node_modules"|"vendor"|"cache"|"sessions"|"history"|"artifacts"|"credentials"|"auth"|"sdk")) { return false; }
    match parts.first().map(String::as_str) {
        Some("AGENTS.md"|"RULES.md"|"SKILLS.md") => parts.len()==1,
        Some("agent-instructions"|"skills"|"tools"|"extensions") => parts.len()>=3,
        Some("rules") => parts.len()>=2,
        _ => false,
    }
}
/// A folder that is a plain directory whose canonical spelling equals its own (no link or alias anywhere on the way).
/// With `create`, missing folders are created one level at a time and re-checked.
fn plain_root(root: &Path, create: bool) -> Result<()> {
    match fs::symlink_metadata(root) {
        Ok(m) if crate::code_quality::snapshot::is_link(&m) || !m.is_dir() => Err(format!("Not a plain folder: {}",root.display())),
        Ok(_) => {
            let canonical=setup::canonical(root).map_err(|e|format!("Cannot resolve {}: {e}",root.display()))?;
            if fsio::same_path(&canonical,root) { Ok(()) } else { Err(format!("Refusing a folder reached through a link or alias: {}",root.display())) }
        }
        Err(e) if create && e.kind()==std::io::ErrorKind::NotFound => {
            if let Some(parent) = root.parent() { plain_root(parent, true)?; }
            fs::create_dir(root).map_err(|e|e.to_string())?;
            plain_root(root,false)
        }
        Err(e) => Err(format!("Cannot inspect {}: {e}",root.display())),
    }
}
fn read(root: &Path, relative: &str) -> Result<Option<Vec<u8>>> {
    fsio::validate_relative(relative)?;
    match fs::symlink_metadata(root.join(relative)) {
        Err(e) if e.kind()==std::io::ErrorKind::NotFound => {
            // An absent leaf must not hide a linked existing ancestor.
            let mut p=root.to_path_buf(); for part in relative.split('/').take(relative.split('/').count()-1) { p.push(part); match fs::symlink_metadata(&p) { Ok(m) if crate::code_quality::snapshot::is_link(&m) || !m.is_dir()=> return Err("Linked/non-directory parent refused".into()), Ok(_)=>{}, Err(e) if e.kind()==std::io::ErrorKind::NotFound=>break, Err(e)=>return Err(e.to_string()) } } Ok(None)
        }, Err(e)=>Err(e.to_string()), Ok(_)=>{
            plain_root(root,false)?;
            let (mut file,_)=fsio::open_relative(root,relative)?;
            let meta=file.metadata().map_err(|e|e.to_string())?;let size=meta.len();let modified=fsio::mtime_nanos(&meta);if size>MAX_FILE { return Err("Markdown asset exceeds the 8 MiB per-file review bound".into()); }
            let mut bytes=Vec::with_capacity(size as usize); Read::by_ref(&mut file).take(MAX_FILE+1).read_to_end(&mut bytes).map_err(|e|e.to_string())?;
            if bytes.len() as u64>MAX_FILE {return Err("Markdown changed size during read".into());}
            std::str::from_utf8(&bytes).map_err(|_|"Markdown asset is not UTF-8")?;
            let after=file.metadata().map_err(|e|e.to_string())?;if after.len()!=size||bytes.len() as u64!=size||fsio::mtime_nanos(&after)!=modified {return Err("Markdown changed during read".into());} Ok(Some(bytes))
        }
    }
}
fn hash(bytes: &Option<Vec<u8>>) -> Option<String> { bytes.as_ref().map(|b| fsio::sha256_hex(b)) }
fn revision(items: &[MarkdownItem]) -> String { hashes_revision(items.iter().filter_map(|i|i.source_hash.as_ref().map(|h|(&i.id,h)))) }
fn hashes_revision<'a>(hashes: impl Iterator<Item=(&'a String,&'a String)>) -> String {let bytes:Vec<u8>=hashes.flat_map(|(id,h)|format!("{id}\0{h}\n").into_bytes()).collect();fsio::sha256_hex(&bytes)}
fn walk(root: &Path, prefix: &str, native: bool, names: &mut BTreeSet<String>, warnings: &mut Vec<String>, depth: usize) -> Result<()> {
    if depth>32 {return Err("Markdown namespace nesting exceeds review safety bound".into());}
    let dir=root.join(prefix);
    match fs::symlink_metadata(&dir) { Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(()), Err(e)=>return Err(e.to_string()), Ok(m) if crate::code_quality::snapshot::is_link(&m)||!m.is_dir()=>return Err(format!("Refusing linked/non-directory namespace: {}",dir.display())), _=>{} }
    let mut case=BTreeSet::new();
    for entry in fs::read_dir(dir).map_err(|e|e.to_string())? {
        let entry=entry.map_err(|e|e.to_string())?; let name=entry.file_name().into_string().map_err(|_|"Non-Unicode asset path")?;
        if name.starts_with('.') || matches!(name.to_ascii_lowercase().as_str(),"node_modules"|"vendor"|"cache"|"sessions"|"history"|"artifacts"|"credentials"|"auth"|"sdk") {continue;}
        if !case.insert(name.to_lowercase()) {return Err("Case-colliding Markdown namespace entries".into());}
        let rel=if prefix.is_empty(){name.clone()}else{format!("{prefix}/{name}")};
        let meta=fs::symlink_metadata(entry.path()).map_err(|e|e.to_string())?;
        if crate::code_quality::snapshot::is_link(&meta) {warnings.push(format!("Refused linked asset: {rel}"));continue;}
        if meta.is_dir() {
            if !native || !prefix.is_empty() || matches!(name.as_str(),"agent-instructions"|"skills"|"rules"|"tools"|"extensions") {walk(root,&rel,native,names,warnings,depth+1)?;}
        } else if meta.is_file() && (!native || allowed(&rel)) && fsio::is_markdown_name(&name) {names.insert(rel); if names.len()>10000{return Err("More than 10,000 Markdown assets; review a smaller native target".into());}}
    } Ok(())
}
fn dependencies(bytes: &[u8]) -> Vec<String> {
    let mut found=BTreeSet::new();
    for word in String::from_utf8_lossy(bytes).split(|c:char| c.is_whitespace()||matches!(c,'`'|'"'|'\''|'('|')'|'['|']'|'<'|'>'|','|';')) {
        if [".py",".sh",".ts",".js",".mjs",".ps1",".bat",".exe"].iter().any(|ext|word.ends_with(ext)) {found.insert(format!("Native-only dependency (not copied): {word}"));}
    } found.into_iter().collect()
}
fn diff(before: &Option<Vec<u8>>, after: &Option<Vec<u8>>) -> String {
    if before==after {return String::new();}
    let old=before.as_ref().map(|b|String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
    let new=after.as_ref().map(|b|String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
    let mut out=String::from("--- current destination\n+++ reviewed source\n");
    for (prefix,text) in [("-",old.as_str()),("+",new.as_str())] {for line in text.lines().take(100) {out.push_str(prefix);out.extend(line.chars().take(500));out.push('\n');} if text.lines().count()>100 {out.push_str("[Preview shortened; full content hashes remain bound to review]\n");}}
    out
}
impl AgentMarkdownService {
    pub fn new(app_config: PathBuf, knowledge: Arc<KnowledgeService>) -> Arc<Self> { Arc::new(Self {dir:app_config.join("knowledge"),knowledge,pending:Mutex::new(HashMap::new()),serial:Mutex::new(()),boundary:OnceLock::new()}) }
    pub fn install_boundary(&self, boundary: Arc<dyn MarkdownBoundary>) -> Result<()> { self.boundary.set(boundary).map_err(|_|"Markdown boundary already installed".into()) }
    fn resolve(&self, registry: &Registry, target: KnowledgeAssetTarget) -> Result<Resolved> {
        let paths=setup::paths(registry)?; self.resolve_paths(target,&paths,|id|setup::repository_root(registry,id))
    }
    fn resolve_paths(&self, target: KnowledgeAssetTarget, paths: &SetupPaths, repository: impl Fn(&str)->Result<PathBuf>) -> Result<Resolved> {
        let name=target_name(&target)?;
        let (native,scope)=match &target {
            KnowledgeAssetTarget::Global=>(paths.global_agent_folder.clone(),KnowledgeScope::Global),
            KnowledgeAssetTarget::Manager=>(paths.manager_folder.clone(),KnowledgeScope::Global),
            KnowledgeAssetTarget::Architect=>(paths.architect_folder.clone(),KnowledgeScope::Global),
            KnowledgeAssetTarget::Profile{profile_name}=>(paths.global_omp_folder.join("profiles").join(profile_name).join("agent"),KnowledgeScope::Global),
            KnowledgeAssetTarget::Repository{project_instance_id}=>{let root=repository(project_instance_id)?;(root.join(".omp"),KnowledgeScope::Project{repository_root:root.to_string_lossy().into_owned()})},
        };
        // Resolve existing ancestors without allowing root aliases/reparse points, including an absent native root.
        let mut existing=native.as_path();while !existing.exists(){existing=existing.parent().ok_or("Native root has no existing ancestor")?;}plain_root(existing,false)?;
        let key=fsio::sha256_hex(format!("{}\0{}",name,fsio::norm(&native)).as_bytes());
        let same_scope=|a:&KnowledgeScope,b:&KnowledgeScope|match(a,b){(KnowledgeScope::Global,KnowledgeScope::Global)=>true,(KnowledgeScope::Project{repository_root:x},KnowledgeScope::Project{repository_root:y})=>fsio::same_path(Path::new(x),Path::new(y)),_=>false};
        let vault=self.knowledge.list_knowledge_vaults()?.into_iter().find(|v|same_scope(&v.scope,&scope)).map(|v|self.knowledge.vault_root(&v.vault_id)).transpose()?;
        Ok(Resolved{target,key,native_identity:fsio::dir_identity(&native),native,namespace:format!("Agent setup/{name}"),vault})
    }
    fn baseline_path(&self,key:&str)->PathBuf {self.dir.join("agent-markdown").join(format!("{key}.json"))}
    fn load(&self,key:&str)->Result<Baseline>{let path=self.baseline_path(key);if !path.exists(){return Ok(Baseline::default());} serde_json::from_slice(&fsio::read_regular_bounded(&path,16*1024*1024)?).map_err(|e|e.to_string())}
    fn save(&self,key:&str,value:&Baseline)->Result<()> {let path=self.baseline_path(key);plain_root(path.parent().unwrap(),true)?;crate::dependency_security::atomic_json(&path,value,16*1024*1024)}
    fn inspect(&self,r:&Resolved)->Result<MarkdownStatus>{
        let baseline=self.load(&r.key)?; let mut warnings=Vec::new(); let mut names=BTreeSet::new();
        walk(&r.native,"",true,&mut names,&mut warnings,0)?;
        if let Some(vault)=&r.vault {let mut source=BTreeSet::new();walk(&vault.root,&r.namespace,false,&mut source,&mut warnings,0)?;for path in source {let rel=path.strip_prefix(&format!("{}/",r.namespace)).ok_or("Invalid namespace")?;if allowed(rel)&&(baseline.allowed_paths.contains(rel)||baseline.hashes.contains_key(rel)||baseline.allowed_prefixes.iter().any(|p|rel.starts_with(p))){names.insert(rel.into());}else{warnings.push(format!("Unmapped note — no native deployment authority: {path}"));}}}
        names.extend(baseline.hashes.keys().cloned()); let mut items=Vec::new();
        for id in names {
            if !allowed(&id){return Err("Stored mapping is not an admitted Markdown destination".into());}
            let vault_path=format!("{}/{}",r.namespace,id);
            let native=match read(&r.native,&id){Ok(value)=>value,Err(e)=>{warnings.push(format!("Refused {id}: {e}"));continue;}};
            let source=match &r.vault{Some(v)=>read(&v.root,&vault_path)?,None=>None};
            let nh=hash(&native);let sh=hash(&source); let ah=baseline.hashes.get(&id).cloned();
            let state=if sh==nh {"unchanged"}else if ah.is_none(){if sh.is_none(){"nativeOnly"}else if nh.is_none(){"new"}else{"conflict"}}
                else if nh!=ah {"nativeChanged"}else if sh.is_none(){"deleted"}else{"changed"};
            if sh.is_none()&&nh.is_none(){continue;}
            items.push(MarkdownItem{id:id.clone(),native_path:r.native.join(&id).to_string_lossy().into_owned(),vault_path,source_hash:sh,native_hash:nh,applied_hash:ah,state:state.into(),diff:diff(&native,&source),native_dependencies:dependencies(source.as_deref().or(native.as_deref()).unwrap_or_default())});
        }
        let source_revision=revision(&items);
        let bound=r.vault.as_ref().is_some_and(|v|v.vault_id==baseline.vault_id)&&baseline.native_root==r.native.to_string_lossy();
        if baseline.enabled&&!bound {warnings.push("Vault/native binding changed; explicitly import/review before enabling this target again".into());}
        Ok(MarkdownStatus{target:r.target.clone(),vault_id:r.vault.as_ref().map(|v|v.vault_id.clone()),namespace:r.namespace.clone(),native_root:r.native.to_string_lossy().into_owned(),source_root:r.vault.as_ref().map(|v|v.root.join(&r.namespace).to_string_lossy().into_owned()),enabled:baseline.enabled&&bound,source_revision,applied_revision:baseline.applied_revision,items,warnings})
    }
    pub fn status(&self,registry:&Registry,target:KnowledgeAssetTarget)->Result<MarkdownStatus>{self.recover()?;let _serial=self.serial.lock();self.inspect(&self.resolve(registry,target)?)}
    pub fn prepare_import(&self,registry:&Registry,target:KnowledgeAssetTarget,selected_native_ids:Option<Vec<String>>)->Result<MarkdownPreparation>{
        self.recover()?;let _serial=self.serial.lock();let r=self.resolve(registry,target)?;self.prepare(r,true,None,selected_native_ids)
    }
    pub fn prepare_apply(&self,registry:&Registry,target:KnowledgeAssetTarget,source_revision:&str)->Result<MarkdownPreparation>{
        self.recover()?;let _serial=self.serial.lock();let r=self.resolve(registry,target)?;self.prepare(r,false,Some(source_revision),None)
    }
    fn prepare(&self,r:Resolved,import:bool,expected:Option<&str>,selected:Option<Vec<String>>)->Result<MarkdownPreparation>{
        r.vault.as_ref().ok_or("Choose a vault for this target first")?;let status=self.inspect(&r)?;
        if !import&&!status.enabled{return Err("Enable vault-authoritative Markdown through reviewed Import first".into());}
        if expected.is_some_and(|v|v!=status.source_revision){return Err("Markdown source revision changed; refresh and review again".into());}
        let ids=selected.unwrap_or_else(||status.items.iter().filter(|i|if import {i.native_hash.is_some()&&(i.source_hash.is_none()||i.source_hash==i.native_hash)}else{matches!(i.state.as_str(),"new"|"changed"|"deleted")}).map(|i|i.id.clone()).collect());
        let unique:BTreeSet<_>=ids.iter().collect();if unique.len()!=ids.len(){return Err("Duplicate Markdown selection".into());}
        for id in &ids {let item=status.items.iter().find(|i|&i.id==id).ok_or("Unknown Markdown selection")?;if import&&item.native_hash.is_none(){return Err("Import requires an admitted native Markdown source".into());}}
        let mut items=status.items.clone(); if import {for item in &mut items{let v=r.vault.as_ref().unwrap();item.diff=diff(&read(&v.root,&item.vault_path)?,&read(&r.native,&item.id)?);}}
        let token=uuid::Uuid::new_v4().to_string();let result=MarkdownPreparation{token:token.clone(),target:r.target.clone(),source_revision:status.source_revision.clone(),items,selected_ids:ids.clone(),warnings:status.warnings};
        let mut pending=self.pending.lock();pending.retain(|_,p|p.expires>Instant::now());pending.insert(token,Pending{resolved:r,expires:Instant::now()+TTL,import,revision:status.source_revision,selected:ids,items:status.items});Ok(result)
    }
    pub fn confirm_import(&self,registry:&Registry,token:&str)->Result<MarkdownStatus>{self.commit(registry,token,None)}
    pub fn apply(&self,registry:&Registry,token:&str,selected_ids:Vec<String>)->Result<MarkdownStatus>{self.commit(registry,token,Some(selected_ids))}
    fn commit(&self,registry:&Registry,token:&str,selection:Option<Vec<String>>)->Result<MarkdownStatus>{
        let _serial=self.serial.lock();let pending=self.pending.lock().remove(token).ok_or("Markdown review token expired or already consumed")?;
        if pending.expires<=Instant::now()||pending.import!=selection.is_none(){return Err("Invalid/expired Markdown review token".into());}
        let r=self.resolve(registry,pending.resolved.target.clone())?;
        self.commit_prepared(r,pending,selection)
    }
    fn commit_prepared(&self,r:Resolved,pending:Pending,selection:Option<Vec<String>>)->Result<MarkdownStatus>{
        let old_v=pending.resolved.vault.as_ref().ok_or("Missing reviewed vault")?;let v=r.vault.as_ref().ok_or("Vault no longer bound")?;
        if r.native!=pending.resolved.native||r.native_identity!=pending.resolved.native_identity||v.root!=old_v.root||v.binding_revision!=old_v.binding_revision||v.vault_id!=old_v.vault_id{return Err("Target binding changed since review".into());}
        let _admission=ADMISSION.write();self.recover_locked()?;let now=self.inspect(&r)?;
        if now.source_revision!=pending.revision{return Err("Markdown source revision changed since review".into());}
        let ids=selection.unwrap_or(pending.selected);let unique:BTreeSet<_>=ids.iter().collect();if unique.len()!=ids.len(){return Err("Duplicate Markdown selection".into());}
        let transaction=uuid::Uuid::new_v4().to_string();
        let _hold=if !pending.import {Some(self.boundary.get().ok_or("native_markdown_boundary_unavailable: captured-host admission guard not installed")?.hold(&transaction,&r.target)?)}else{None};
        let mut baseline=self.load(&r.key)?;if baseline.vault_id!=v.vault_id||baseline.native_root!=r.native.to_string_lossy(){baseline=Baseline::default();}baseline.enabled=true;baseline.vault_id=v.vault_id.clone();baseline.native_root=r.native.to_string_lossy().into_owned();
        let folder=self.dir.join("deployments").join(&transaction);plain_root(&folder,true)?;
        let mut operations=Vec::new();let mut contents=Vec::new();
        for id in &ids {
            let item=now.items.iter().find(|i|&i.id==id).ok_or("Unknown Markdown selection")?;
            let reviewed=pending.items.iter().find(|i|&i.id==id).ok_or("Item was not reviewed")?;
            if item.native_hash!=reviewed.native_hash||item.source_hash!=reviewed.source_hash{return Err(format!("{id} changed since review"));}
            if !pending.import&&!matches!(item.state.as_str(),"new"|"changed"|"deleted"){return Err(format!("{id}: import current native Markdown or cancel/review; runtime changed"));}
            let native=read(&r.native,id)?;let source=read(&v.root,&item.vault_path)?;
            let (before,after,relative)=if pending.import{(source,native,item.vault_path.clone())}else{(native,source,id.clone())};
            if pending.import&&after.is_none(){return Err("Import source disappeared".into());}
            let backup=before.as_ref().map(|_|format!("{}.md",operations.len()));
            if let (Some(bytes),Some(name))=(&before,&backup){fsio::write_atomic(&folder.join(name),bytes,false)?;}
            let next=hash(&after);baseline.allowed_paths.insert(id.clone());if let Some(h)=&next{baseline.hashes.insert(id.clone(),h.clone());}else{baseline.hashes.remove(id);}
            operations.push(Operation{relative,before:hash(&before),after:next,backup});contents.push(after);
        }
        baseline.applied_revision=Some(hashes_revision(baseline.hashes.iter()));
        let root=if pending.import{v.root.clone()}else{r.native.clone()};plain_root(&root,true)?;
        let mut journal=Journal{id:transaction,state:"applying".into(),key:r.key.clone(),target:r.target.clone(),root:root.clone(),root_identity:fsio::dir_identity(&root),namespace:pending.import.then(||r.namespace.clone()),operations,baseline};
        self.write_journal(&folder,&journal)?;
        let result=(||{for (op,after) in journal.operations.iter().zip(&contents){publish(&root,&op.relative,op.before.as_deref(),after.as_deref(),&self.dir.join("agent-publications"))?;}journal.state="committed".into();self.write_journal(&folder,&journal)?;self.save(&r.key,&journal.baseline)})();
        if let Err(error)=result{let recovery=fsio::recover_publications(&self.dir.join("agent-publications")).and_then(|_|self.recover_one(&folder,&mut journal));return Err(format!("Markdown batch failed: {error}; recovery: {}",recovery.err().unwrap_or_else(||journal.state.clone())));}
        journal.state="applied".into();self.write_journal(&folder,&journal)?;self.inspect(&r)
    }
    fn write_journal(&self,folder:&Path,journal:&Journal)->Result<()> {crate::dependency_security::atomic_json(&folder.join("journal.json"),journal,16*1024*1024)}
    fn recover_one(&self,folder:&Path,j:&mut Journal)->Result<()> {
        if matches!(j.state.as_str(),"applied"|"rolledBack"){return Ok(());}
        plain_root(&j.root,false)?;if fsio::dir_identity(&j.root)!=j.root_identity{return Err("Deployment root identity changed; recovery refuses another root".into());}
        if j.state=="committed" {self.save(&j.key,&j.baseline)?;j.state="applied".into();return self.write_journal(folder,j);}
        if j.state!="applying"{return Err("Unknown Markdown journal state".into());}
        for op in j.operations.iter().rev(){
            let rel=if let Some(namespace)=&j.namespace{op.relative.strip_prefix(&format!("{namespace}/")).ok_or("Journal escaped namespace")?}else{op.relative.as_str()};
            if !allowed(rel){return Err("Journal has an inadmissible Markdown path".into());}
            let current=read(&j.root,&op.relative)?;let current_hash=hash(&current);
            if current_hash==op.before{continue;}if current_hash!=op.after{return Err(format!("Recovery conflict at {}; retain backup and review manually",op.relative));}
            let before=match &op.backup {Some(name)=>{if !name.ends_with(".md")||name.contains(['/', '\\']){return Err("Invalid backup path".into());}Some(fsio::read_regular_bounded(&folder.join(name),MAX_FILE)?)},None=>None};
            if hash(&before)!=op.before{return Err("Immutable Markdown backup hash mismatch".into());}
            publish(&j.root,&op.relative,op.after.as_deref(),before.as_deref(),&self.dir.join("agent-publications"))?;
        }
        j.state="rolledBack".into();self.write_journal(folder,j)
    }
    fn recover_locked(&self)->Result<()> {
        let publications=self.dir.join("agent-publications");
        if fsio::publications_need_recovery(&publications)? {
            // An interrupted per-file publication may rewrite native Markdown: quiesce the captured hosts first when they exist.
            let _hold=self.boundary.get().map(|b|b.hold(&uuid::Uuid::new_v4().to_string(),&KnowledgeAssetTarget::Global)).transpose()?;
            fsio::recover_publications(&publications)?;
        }
        let dir=self.dir.join("deployments");if !dir.exists(){return Ok(());}plain_root(&dir,false)?;
        for entry in fs::read_dir(dir).map_err(|e|e.to_string())? {
            let entry=entry.map_err(|e|e.to_string())?;if uuid::Uuid::parse_str(&entry.file_name().to_string_lossy()).is_err(){continue;}
            let folder=entry.path();plain_root(&folder,false)?;let path=folder.join("journal.json");if !path.exists(){continue;}
            let mut j:Journal=serde_json::from_slice(&fsio::read_regular_bounded(&path,16*1024*1024)?).map_err(|e|e.to_string())?;
            let _hold=if j.state=="applying"&&j.namespace.is_none(){self.boundary.get().map(|b|b.hold(&j.id,&j.target)).transpose()?}else{None};
            self.recover_one(&folder,&mut j)?;
        }Ok(())
    }
    /// Call once before native prompt/Persona loading and on every explicit review; no unfinished batch is consumed.
    pub fn recover(&self)->Result<()> {let _serial=self.serial.lock();let _admission=ADMISSION.write();self.recover_locked()}
    pub fn routes(&self,registry:&Registry,request:&SetupRequest)->Result<MarkdownRoutes>{
        self.recover()?;let _serial=self.serial.lock();
        let target=match request.scope{SetupScope::Global=>KnowledgeAssetTarget::Global,SetupScope::Manager=>KnowledgeAssetTarget::Manager,SetupScope::Architect=>KnowledgeAssetTarget::Architect,SetupScope::Repository=>KnowledgeAssetTarget::Repository{project_instance_id:request.project_instance_id.clone().ok_or("Repository requires a project")?}};
        let mut profiles=BTreeMap::new();
        for item in &request.items {if item.kind==setup::SetupKind::Profile {if let Some(r)=self.reserve_route(self.resolve(registry,KnowledgeAssetTarget::Profile{profile_name:item.name.clone()})?,std::slice::from_ref(item),true)?{profiles.insert(item.name.clone(),r);}}}
        Ok(MarkdownRoutes{scope:self.reserve_route(self.resolve(registry,target)?,&request.items,false)?,profiles})
    }
    fn reserve_route(&self,r:Resolved,items:&[setup::SetupItem],profile:bool)->Result<Option<MarkdownRoute>>{
        let status=self.inspect(&r)?;if !status.enabled{return Ok(None);}
        let mut baseline=self.load(&r.key)?;
        if profile {
            // A normalized human Profile request grants Markdown discovery namespaces, never profile configuration.
            for path in ["AGENTS.md","RULES.md","SKILLS.md"] {baseline.allowed_paths.insert(path.into());}
            for prefix in ["rules/","skills/","agent-instructions/","tools/","extensions/"] {baseline.allowed_prefixes.insert(prefix.into());}
        } else {
            for item in items {match item.kind {
                setup::SetupKind::Agents=>{baseline.allowed_paths.insert(if item.name.is_empty(){"AGENTS.md".into()}else{format!("agent-instructions/{}/AGENTS.md",item.name)});},
                setup::SetupKind::Rules=>{baseline.allowed_paths.insert("RULES.md".into());},
                setup::SetupKind::Skill=>{baseline.allowed_paths.insert("SKILLS.md".into());baseline.allowed_prefixes.insert(format!("skills/{}/",item.name));},
                setup::SetupKind::Workflow=>{baseline.allowed_paths.insert(format!("rules/workflow-{}.md",item.name));},
                setup::SetupKind::Tool=>{baseline.allowed_prefixes.insert(format!("tools/{}/",item.name));},
                setup::SetupKind::Plugin=>{baseline.allowed_prefixes.insert(format!("extensions/{}/",item.name));},
                setup::SetupKind::Profile=>{},
            }}
        }
        self.save(&r.key,&baseline)?;
        Ok(Some(MarkdownRoute{source_root:PathBuf::from(status.source_root.ok_or("Enabled source missing")?),native_root:r.native}))
    }
}
#[derive(Clone, Default)]
pub struct MarkdownRoutes {pub scope:Option<MarkdownRoute>,pub profiles:BTreeMap<String,MarkdownRoute>}
#[derive(Clone)]
pub struct MarkdownRoute {pub source_root:PathBuf,pub native_root:PathBuf}
fn publish(root:&Path,relative:&str,expected:Option<&str>,bytes:Option<&[u8]>,journal_dir:&Path)->Result<()> {
    if !fsio::is_markdown_name(relative){return Err("Only Markdown may publish".into());}
    let current=read(root,relative)?;if hash(&current).as_deref()!=expected{return Err(format!("Concurrent Markdown change: {relative}"));}
    if let Some(bytes)=bytes{
        let text=std::str::from_utf8(bytes).map_err(|_|"Invalid Markdown UTF-8")?;
        fsio::write_relative_markdown_journaled(root,relative,text,expected,journal_dir)?;
    }else if current.is_some(){fsio::remove_relative_checked(root,relative,expected.ok_or("Missing deletion precondition")?)?;}
    Ok(())
}

#[cfg(test)]
#[path="agent_markdown_tests.rs"]
mod tests;
