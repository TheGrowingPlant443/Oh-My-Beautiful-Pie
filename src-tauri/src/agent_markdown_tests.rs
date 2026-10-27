use super::*;
use crate::knowledge::{ScopeRequest,VaultMode,WritePrecondition};

struct Fixture {root:PathBuf,service:Arc<AgentMarkdownService>,resolved:Resolved,paths:SetupPaths}
impl Fixture {
    fn new()->Self {
        let root=std::env::temp_dir().join(format!("omp-markdown-{}",uuid::Uuid::new_v4()));
        let home=root.join("home");let app=root.join("app");let vault=root.join("vault");
        for dir in [&home,&app,&vault]{fs::create_dir_all(dir).unwrap();}
        let knowledge=KnowledgeService::new(app.clone(),home);
        let token=knowledge.prepare_knowledge_vault(ScopeRequest::Global,None,vault.to_str().unwrap(),VaultMode::Markdown).unwrap();
        knowledge.confirm_knowledge_vault(&token.token).unwrap();
        let paths=SetupPaths{global_omp_folder:root.join("native"),global_agent_folder:root.join("native/agent"),manager_folder:app.join("manager-setup"),architect_folder:app.join("architect-setup"),local_ai_folder:app.join("local-ai-setup")};
        fs::create_dir_all(&paths.global_agent_folder).unwrap();
        let service=AgentMarkdownService::new(app,knowledge);
        service.install_boundary(Arc::new(IdleFixture)).unwrap();
        let resolved=service.resolve_paths(KnowledgeAssetTarget::Global,&paths,|_|Err("No repository in fixture".into())).unwrap();
        Self{root,service,resolved,paths}
    }
    fn native(&self,path:&str,text:&str){let full=self.resolved.native.join(path);fs::create_dir_all(full.parent().unwrap()).unwrap();fs::write(full,text).unwrap();}
    fn note(&self,path:&str,text:&str){let v=self.resolved.vault.as_ref().unwrap();let full=format!("{}/{path}",self.resolved.namespace);let expected=read(&v.root,&full).unwrap();v.write_markdown(&full,text,match hash(&expected){Some(h)=>WritePrecondition::Replace{expected_hash:h},None=>WritePrecondition::Create}).unwrap();}
    fn prepare(&self,import:bool,selected:Option<Vec<String>>)->MarkdownPreparation{let current=self.service.inspect(&self.resolved).unwrap();self.service.prepare(self.resolved.clone(),import,(!import).then_some(current.source_revision.as_str()),selected).unwrap()}
    fn commit(&self,prepared:&MarkdownPreparation,selection:Option<Vec<String>>)->Result<MarkdownStatus>{let p=self.service.pending.lock().remove(&prepared.token).unwrap();self.service.commit_prepared(self.resolved.clone(),p,selection)}
    fn import(&self)->MarkdownStatus{let p=self.prepare(true,None);self.commit(&p,None).unwrap()}
}
impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_dir_all(&self.root);}}
struct IdleFixture;
impl MarkdownBoundary for IdleFixture {fn hold(&self,_:&str,_:&KnowledgeAssetTarget)->Result<Box<dyn Send>>{Ok(Box::new(()))}}

#[test]
fn only_native_markdown_assets_are_imported_and_scripts_stay_native(){
    let f=Fixture::new();
    for (path,text) in [("AGENTS.md","policy"),("skills/review/SKILL.md","Run `scripts/check.py` with native approval."),("skills/review/guide.md","guide"),("tools/check/README.md","tool docs"),("extensions/p/README.md","plugin docs"),("tools/check/run.py","SECRET SCRIPT"),("extensions/p/package.json","SECRET CONFIG"),("config.yml","SECRET CONFIG"),("auth.json","SECRET CREDENTIAL"),("agent.db","SECRET SESSION"),("sessions/decoy.md","SECRET SESSION"),("skills/review/node_modules/sdk/README.md","SECRET SDK"),("index.ts","SECRET BOOTSTRAP")]{f.native(path,text);}
    let preview=f.prepare(true,None);
    assert_eq!(preview.selected_ids.len(),5);
    assert!(preview.items.iter().all(|i|i.id.ends_with(".md")&&!i.diff.contains("SECRET")));
    let done=f.commit(&preview,None).unwrap();assert!(done.enabled);assert_eq!(done.source_revision,done.applied_revision.unwrap());
    let vault=f.resolved.vault.as_ref().unwrap();
    assert!(!vault.root.join("Agent setup/Global/tools/check/run.py").exists());
    assert!(!vault.root.join("Agent setup/Global/config.yml").exists());
    assert_eq!(fs::read_to_string(f.resolved.native.join("tools/check/run.py")).unwrap(),"SECRET SCRIPT");
    assert!(done.items.iter().find(|i|i.id.ends_with("SKILL.md")).unwrap().native_dependencies.iter().any(|s|s.contains("scripts/check.py")));
}
#[test]
fn markdown_stays_inactive_until_apply_and_native_drift_requires_import(){
    let f=Fixture::new();f.native("AGENTS.md","old native");f.import();f.note("AGENTS.md","reviewed vault");
    assert_eq!(fs::read_to_string(f.resolved.native.join("AGENTS.md")).unwrap(),"old native");
    let p=f.prepare(false,None);assert_eq!(p.items[0].state,"changed");f.commit(&p,Some(vec!["AGENTS.md".into()])).unwrap();
    assert_eq!(fs::read_to_string(f.resolved.native.join("AGENTS.md")).unwrap(),"reviewed vault");
    f.native("AGENTS.md","external native edit");f.note("AGENTS.md","second vault edit");
    let p=f.prepare(false,None);assert_eq!(p.items[0].state,"nativeChanged");assert!(f.commit(&p,Some(vec!["AGENTS.md".into()])).is_err());
    let p=f.prepare(true,Some(vec!["AGENTS.md".into()]));assert!(p.items[0].diff.contains("external native edit"));f.commit(&p,None).unwrap();
    assert_eq!(String::from_utf8(f.resolved.vault.as_ref().unwrap().read_markdown("Agent setup/Global/AGENTS.md").unwrap()).unwrap(),"external native edit");
}
#[test]
fn arbitrary_notes_cannot_grant_runtime_destinations_and_deletions_are_explicit(){
    let f=Fixture::new();f.native("AGENTS.md","native");f.import();
    f.note("skills/unrequested/SKILL.md","arbitrary note cannot create runtime skill");
    let status=f.service.inspect(&f.resolved).unwrap();assert!(!status.items.iter().any(|i|i.id.contains("unrequested")));assert!(status.warnings.iter().any(|w|w.contains("Unmapped note")));
    fs::remove_file(f.resolved.vault.as_ref().unwrap().root.join("Agent setup/Global/AGENTS.md")).unwrap();
    let p=f.prepare(false,None);assert_eq!(p.items[0].state,"deleted");f.commit(&p,Some(vec![])).unwrap();assert!(f.resolved.native.join("AGENTS.md").is_file());
    let p=f.prepare(false,None);f.commit(&p,Some(vec!["AGENTS.md".into()])).unwrap();assert!(!f.resolved.native.join("AGENTS.md").exists());
}
#[test]
fn stale_hash_revision_binding_and_consumed_tokens_are_rejected(){
    let f=Fixture::new();f.native("AGENTS.md","old");let p=f.prepare(true,None);f.native("AGENTS.md","changed after preview");assert!(f.commit(&p,None).is_err());
    f.import();let old=f.service.inspect(&f.resolved).unwrap().source_revision;f.note("AGENTS.md","new");assert!(f.service.prepare(f.resolved.clone(),false,Some(&old),None).is_err());
    let registry=Registry::load(f.root.join("instances.json"),f.root.join("home")).unwrap();
    let p=f.prepare(true,None);f.service.pending.lock().get_mut(&p.token).unwrap().expires=Instant::now()-Duration::from_secs(1);
    assert!(f.service.confirm_import(&registry,&p.token).unwrap_err().contains("expired"));assert!(f.service.confirm_import(&registry,&p.token).unwrap_err().contains("consumed"));
    let p=f.prepare(true,None);let mut changed=f.resolved.clone();changed.native_identity=Some("replacement".into());let pending=f.service.pending.lock().remove(&p.token).unwrap();assert!(f.service.commit_prepared(changed,pending,None).is_err());
}
#[test]
fn hardlinked_secret_never_enters_preview_vault_or_backups(){
    let f=Fixture::new();let secret=f.root.join("credential.md");fs::write(&secret,"SECRET credential bytes").unwrap();fs::hard_link(&secret,f.resolved.native.join("AGENTS.md")).unwrap();
    let p=f.prepare(true,None);assert!(p.items.is_empty());assert!(p.warnings.iter().any(|w|w.contains("multiply linked")));f.commit(&p,None).unwrap();
    assert!(!f.resolved.vault.as_ref().unwrap().root.join("Agent setup/Global/AGENTS.md").exists());
    assert!(read(&f.resolved.native,"AGENTS.md").is_err());
}
#[test]
fn linked_markdown_and_parent_junctions_are_refused(){
    let f=Fixture::new();let outside=f.root.join("outside");fs::create_dir(&outside).unwrap();fs::write(outside.join("SKILL.md"),"SECRET outside").unwrap();fs::create_dir(f.resolved.native.join("skills")).unwrap();
    let link=f.resolved.native.join("skills/escape");
    #[cfg(unix)] let linked=std::os::unix::fs::symlink(&outside,&link).is_ok();
    #[cfg(windows)] let linked=std::process::Command::new("cmd.exe").args(["/C","mklink","/J"]).arg(&link).arg(&outside).output().unwrap().status.success();
    if linked{let p=f.prepare(true,None);assert!(p.items.is_empty());assert!(p.warnings.iter().any(|w|w.contains("linked")));assert!(read(&f.resolved.native,"skills/escape/SKILL.md").is_err());
        #[cfg(unix)] fs::remove_file(&link).unwrap();
        #[cfg(windows)] fs::remove_dir(&link).unwrap();
    }else{eprintln!("Link safety test unexercised: OS refused fixture link creation");}
}
fn interrupted(f:&Fixture,committed:bool)->(PathBuf,Journal){
    let id=uuid::Uuid::new_v4().to_string();let folder=f.service.dir.join("deployments").join(&id);fs::create_dir_all(&folder).unwrap();
    fsio::write_atomic(&folder.join("0.md"),b"original",false).unwrap();
    f.native("AGENTS.md","updated");f.native("RULES.md","new");
    let j=Journal{id,key:f.resolved.key.clone(),target:f.resolved.target.clone(),state:if committed{"committed".into()}else{"applying".into()},root:f.resolved.native.clone(),root_identity:fsio::dir_identity(&f.resolved.native),namespace:None,
        operations:vec![Operation{relative:"AGENTS.md".into(),before:Some(fsio::sha256_hex(b"original")),after:Some(fsio::sha256_hex(b"updated")),backup:Some("0.md".into())},Operation{relative:"RULES.md".into(),before:None,after:Some(fsio::sha256_hex(b"new")),backup:None}],baseline:Baseline{enabled:true,..Default::default()}};
    f.service.write_journal(&folder,&j).unwrap();(folder,j)
}
#[test]
fn crash_recovery_rolls_back_partial_batch_without_deleting_immutable_backups(){
    let f=Fixture::new();let (folder,_)=interrupted(&f,false);f.service.recover().unwrap();
    assert_eq!(fs::read_to_string(f.resolved.native.join("AGENTS.md")).unwrap(),"original");assert!(!f.resolved.native.join("RULES.md").exists());assert_eq!(fs::read_to_string(folder.join("0.md")).unwrap(),"original");
    f.service.recover().unwrap();assert_eq!(fs::read_to_string(f.resolved.native.join("AGENTS.md")).unwrap(),"original");
}
#[test]
fn crash_after_commit_rolls_forward_baseline_and_preserves_deployment(){
    let f=Fixture::new();interrupted(&f,true);f.service.recover().unwrap();assert_eq!(fs::read_to_string(f.resolved.native.join("AGENTS.md")).unwrap(),"updated");assert!(f.service.load(&f.resolved.key).unwrap().enabled);
}
#[test]
fn rollback_conflict_preserves_concurrent_edits_and_fails_instruction_recovery(){
    let f=Fixture::new();let (folder,_)=interrupted(&f,false);f.native("AGENTS.md","human edit after crash");assert!(f.service.recover().unwrap_err().contains("Recovery conflict"));assert_eq!(fs::read_to_string(f.resolved.native.join("AGENTS.md")).unwrap(),"human edit after crash");assert!(folder.join("0.md").is_file());
}
#[test]
fn default_and_named_profiles_have_disjoint_roots_and_markdown_only_authority(){
    let f=Fixture::new();let a=f.service.resolve_paths(KnowledgeAssetTarget::Profile{profile_name:"first".into()},&f.paths,|_|Err("unused".into())).unwrap();let b=f.service.resolve_paths(KnowledgeAssetTarget::Profile{profile_name:"second".into()},&f.paths,|_|Err("unused".into())).unwrap();
    assert_ne!(a.key,b.key);assert_ne!(a.native,f.resolved.native);assert_ne!(a.namespace,b.namespace);
    for target in ["default","../escape","CON"]{assert!(f.service.resolve_paths(KnowledgeAssetTarget::Profile{profile_name:target.into()},&f.paths,|_|Err("unused".into())).is_err());}
    for bad in ["config.yml","auth.json","sessions/secret.md","AGENTS.md:stream","../AGENTS.md","tools/x/index.ts","skills/x/run.py"]{assert!(!allowed(bad),"{bad}");}
}

#[test]
fn local_markdown_import_and_apply_are_isolated_from_other_scopes(){
    let mut f=Fixture::new();
    let global=f.resolved.clone();
    f.native("AGENTS.md","global policy");
    let local=f.service.resolve_paths(KnowledgeAssetTarget::LocalAi,&f.paths,|_|Err("unused".into())).unwrap();
    assert_eq!(local.namespace,"Agent setup/Local AI models");
    assert!(!local.native.exists());
    for target in [KnowledgeAssetTarget::Global,KnowledgeAssetTarget::Manager,KnowledgeAssetTarget::Architect,KnowledgeAssetTarget::Profile{profile_name:"local".into()},KnowledgeAssetTarget::Repository{project_instance_id:"repo".into()}]{
        let other=f.service.resolve_paths(target,&f.paths,|_|Ok(f.root.join("repository"))).unwrap();
        assert_ne!(local.key,other.key);assert_ne!(local.native,other.native);assert_ne!(local.namespace,other.namespace);
    }
    f.resolved=local;
    f.native("AGENTS.md","local policy");f.native("index.ts","loader");f.native("extensions/plugin/package.json","manifest");
    let imported=f.import();assert_eq!(imported.items.len(),1);
    let vault=f.resolved.vault.as_ref().unwrap();
    assert_eq!(String::from_utf8(vault.read_markdown("Agent setup/Local AI models/AGENTS.md").unwrap()).unwrap(),"local policy");
    assert!(!vault.root.join("Agent setup/Global/AGENTS.md").exists());
    f.note("AGENTS.md","reviewed local policy");
    let prepared=f.prepare(false,None);f.commit(&prepared,None).unwrap();
    assert_eq!(fs::read_to_string(f.resolved.native.join("AGENTS.md")).unwrap(),"reviewed local policy");
    assert_eq!(fs::read_to_string(global.native.join("AGENTS.md")).unwrap(),"global policy");
    assert_eq!(fs::read_to_string(f.resolved.native.join("index.ts")).unwrap(),"loader");
    assert_eq!(fs::read_to_string(f.resolved.native.join("extensions/plugin/package.json")).unwrap(),"manifest");
}
