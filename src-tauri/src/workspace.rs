use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use parking_lot::{Condvar, Mutex};

use serde::Serialize;
use serde_json::{json, Value};
use tauri::Emitter;

use crate::instance::{self, ControlLease, InstanceKind, Origin, Registry, WorkspaceCommit, WorkspaceSourceBinding};
use crate::manager::ManagerHub;

type Result<T, E = String> = std::result::Result<T, E>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkspaceKind { Manual, ManagerHandover, ManagerReturn, ArchitectRepo, DependencyPlan }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkspaceStatus { Pending, Applying, Applied, Failed, Cancelled }
impl WorkspaceStatus { fn terminal(self) -> bool { matches!(self, Self::Applied | Self::Failed | Self::Cancelled) } }
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceChange {
    pub request_id: String,
    pub coordinator_id: String,
    pub source_instance_id: Option<String>,
    pub target_folder: String,
    pub kind: WorkspaceKind,
    pub status: WorkspaceStatus,
    pub error: Option<String>,
}
#[derive(Clone)]
struct Job {
    change: WorkspaceChange,
    expected: ControlLease,
    source: Option<WorkspaceSourceBinding>,
    dependency: Option<String>,
    repositories: Vec<String>,
    committed: bool,
    ready_claimed: bool,
    connection: Option<String>,
}
#[derive(Default)]
struct QueueState { jobs: HashMap<String, Job>, queues: HashMap<String, VecDeque<String>>, order: VecDeque<String>, workers: usize, closing: bool }
pub struct DependencyWorkspaceContext { pub manager_id: String, pub manager_working_directory: String, pub repository_folders: Vec<String> }
struct OwnershipContext { target: String, repositories: Vec<String>, computed: bool }
#[derive(Serialize, serde::Deserialize)]
struct ComputedFolders { version: u32, folders: Vec<String> }
const COMPUTED_FOLDERS_LIMIT: u64 = 1024 * 1024;
const COMPUTED_FOLDERS_MAX: usize = 1000;
fn write_computed_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("Invalid computed workspace folders path")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("Writing computed workspace folders: {e}"))?;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if let Err(error) = result { let _ = std::fs::remove_file(&temporary); return Err(format!("Writing computed workspace folders: {error}")); }
    Ok(())
}
pub struct WorkspaceService {
    registry: Arc<Registry>, hub: Arc<ManagerHub>, root: PathBuf, state: Mutex<QueueState>, changed: Condvar, computed_lock: Mutex<()>,
    app: OnceLock<tauri::AppHandle>, dependency_updates: OnceLock<Weak<crate::dependency_updates::DependencyUpdateService>>,
}
impl WorkspaceService {
    pub fn new(registry: Arc<Registry>, hub: Arc<ManagerHub>, root: PathBuf) -> Arc<Self> { Arc::new(Self { registry, hub, root, state: Mutex::new(QueueState::default()), changed: Condvar::new(), computed_lock: Mutex::new(()), app: OnceLock::new(), dependency_updates: OnceLock::new() }) }
    pub fn bind_app(&self, app: tauri::AppHandle) -> Result<()> { self.app.set(app).map_err(|_| "workspace app already bound".into()) }
    pub fn bind_dependency_updates(&self, service: Weak<crate::dependency_updates::DependencyUpdateService>) -> Result<()> { self.dependency_updates.set(service).map_err(|_| "workspace dependency service already bound".into()) }
    pub fn has_active_work(&self)->bool {self.state.lock().workers>0}
    pub fn list(&self) -> Vec<WorkspaceChange> { let state = self.state.lock(); state.order.iter().filter_map(|id| state.jobs.get(id).map(|j| j.change.clone())).collect() }
    fn publish(&self) { if let Some(app) = self.app.get() { let _ = app.emit("workspace_changes", self.list()); } }
    fn canonical(folder: &str) -> Result<String> { let path = crate::setup::canonical(Path::new(folder)).map_err(|e| format!("Invalid project folder: {e}"))?; if !path.is_dir() { return Err("Working directory must be an existing directory".into()); } Ok(path.to_string_lossy().into_owned()) }
    fn coordinator(&self, kind: InstanceKind) -> Result<String> { self.registry.coordinator_id(kind) }
    fn source(&self, source_id: &str) -> Result<crate::instance::WorkspaceMetadata> { let data = self.registry.workspace_metadata(source_id)?; if data.definition.kind != InstanceKind::Instance || data.definition.helper.is_some() || data.definition.plan_target.is_some() || !self.registry.is_real_project_folder(&data.definition.input.project_path) { return Err("Only ordinary project workers can supply repository context".into()); } Ok(data) }
    fn root(&self, folder: &str) -> Result<String> { Ok(crate::setup::find_repository_root(Path::new(&Self::canonical(folder)?))?.to_string_lossy().into_owned()) }
    fn repository_context(&self, manager: &str, incoming: &str) -> Result<(String, Vec<String>)> {
        let mut roots = self.registry.managed_repository_roots(manager)?;
        roots.push(self.root(incoming)?);
        roots.sort_by_key(|p| p.to_lowercase().replace('\\', "/"));
        roots.dedup_by(|a,b| a.to_lowercase().replace('\\', "/") == b.to_lowercase().replace('\\', "/"));
        let target = instance::common_ancestor_folder(&roots).map(Ok)
            .unwrap_or_else(|| self.registry.coordinator_neutral_folder(InstanceKind::Manager))?;
        // Component arithmetic uses portable separators; admissions expose the
        // canonical native spelling used by repository roots and committed cwd.
        Ok((Self::canonical(&target)?, roots))
    }
    /// Where The Manager itself goes when `source` is handed over or returned. Exactly one connected ordinary
    /// agent definition (counted by definition id before any Git-root deduplication; the incoming source counts
    /// once; helpers, plan agents and native subagents never count; The Manager's old folder is never an input)
    /// sends The Manager to that agent's actual project folder. Several connected agents share a computed folder.
    fn ownership_context(&self, manager: &str, source_id: &str, source_path: &str) -> Result<OwnershipContext> {
        let mut connected = self.registry.managed_project_folders(manager)?;
        if !connected.iter().any(|(id, _)| id == source_id) { connected.push((source_id.to_owned(), Self::canonical(source_path)?)); }
        if let [(_, folder)] = connected.as_slice() {
            return Ok(OwnershipContext { target: folder.clone(), repositories: vec![self.root(folder)?], computed: false });
        }
        let (target, repositories) = self.repository_context(manager, source_path)?;
        // A shared target that is itself a connected agent's real project folder is a project, not a computed folder.
        let computed = !connected.iter().any(|(_, folder)| instance::same_folder(folder, &target));
        Ok(OwnershipContext { target, repositories, computed })
    }
    fn prepare_ownership(&self, manager: &str, source_id: &str, source_path: &str) -> Result<OwnershipContext> {
        let context = self.ownership_context(manager, source_id, source_path)?;
        if context.computed { self.record_computed_folder(&context.target)?; }
        Ok(context)
    }
    fn computed_file(&self) -> PathBuf { self.root.join("workspace").join("computed-folders.json") }
    fn read_computed_folders(&self) -> Result<Vec<String>> {
        let file = self.computed_file();
        let size = match std::fs::metadata(&file) {
            Ok(meta) => meta.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => return Err(format!("Reading computed workspace folders: {error}")),
        };
        // Derived hint data: an oversized, unreadable or foreign-version file restarts empty instead of blocking
        // ownership transfers; startup reconciliation and later transfers rebuild it.
        if size > COMPUTED_FOLDERS_LIMIT { return Ok(vec![]); }
        let bytes = std::fs::read(&file).map_err(|e| format!("Reading computed workspace folders: {e}"))?;
        Ok(serde_json::from_slice::<ComputedFolders>(&bytes).ok().filter(|saved| saved.version == 1).map(|saved| saved.folders).unwrap_or_default())
    }
    pub fn computed_folders(&self) -> Result<Vec<String>> { let _guard = self.computed_lock.lock(); self.read_computed_folders() }
    fn record_computed_folder(&self, folder: &str) -> Result<()> {
        let folder = Self::canonical(folder)?;
        let folders = {
            let _guard = self.computed_lock.lock();
            let mut folders = self.read_computed_folders()?;
            if folders.iter().any(|known| instance::same_folder(known, &folder)) { return Ok(()); }
            folders.push(folder);
            if folders.len() > COMPUTED_FOLDERS_MAX { folders.drain(..folders.len() - COMPUTED_FOLDERS_MAX); }
            let mut bytes = serde_json::to_vec(&ComputedFolders { version: 1, folders: folders.clone() }).map_err(|e| e.to_string())?;
            while bytes.len() as u64 > COMPUTED_FOLDERS_LIMIT && folders.len() > 1 {
                folders.remove(0);
                bytes = serde_json::to_vec(&ComputedFolders { version: 1, folders: folders.clone() }).map_err(|e| e.to_string())?;
            }
            write_computed_atomic(&self.computed_file(), &bytes)?;
            folders
        };
        if let Some(app) = self.app.get() { let _ = app.emit("computed_workspace_folders", &folders); }
        Ok(())
    }
    /// Startup reconciliation for state written before computed folders were recorded: when The Manager owns
    /// several connected agents and sits in a real folder that is none of the ordinary agents' own project
    /// folders, that folder was computed for the group.
    pub fn reconcile_computed_folders(&self) -> Result<()> {
        let Ok(manager) = self.coordinator(InstanceKind::Manager) else { return Ok(()); };
        let cwd = self.registry.workspace_metadata(&manager)?.definition.input.project_path;
        if !self.registry.is_real_project_folder(&cwd) { return Ok(()); }
        if self.registry.managed_project_folders(&manager)?.len() < 2 { return Ok(()); }
        let cwd = Self::canonical(&cwd)?;
        if self.registry.ordinary_project_folders().iter().any(|folder| instance::same_folder(folder, &cwd)) { return Ok(()); }
        self.record_computed_folder(&cwd)
    }
    pub fn dependency_plan_context(&self, project_folder: &str) -> Result<DependencyWorkspaceContext> {
        if !self.registry.is_real_project_folder(project_folder) {return Err("Dependency handoff requires a real project repository".into());}
        let manager_id = self.coordinator(InstanceKind::Manager)?;
        let (manager_working_directory, repository_folders) = self.repository_context(&manager_id, project_folder)?;
        Ok(DependencyWorkspaceContext { manager_id, manager_working_directory, repository_folders })
    }
    pub fn change_directory(self: &Arc<Self>, id: String, folder: String, expected_generation: u64) -> Result<WorkspaceChange> {
        let metadata = self.registry.workspace_metadata(&id)?;
        if !instance::may_change_directory(&metadata.definition) { return Err("Only agent working directories can be changed".into()); }
        if metadata.lease.generation != expected_generation { return Err("stale_generation".into()); }
        let target = Self::canonical(&folder)?;
        if metadata.definition.kind == InstanceKind::Instance && !self.registry.is_real_project_folder(&target) { return Err("An agent working directory cannot be an app-managed workspace".into()); }
        self.enqueue(id, target, WorkspaceKind::Manual, metadata.lease, None, None, vec![])
    }
    pub fn give_repo_to_architect(self: &Arc<Self>, source_id: String) -> Result<WorkspaceChange> {
        let binding=self.registry.workspace_source_binding(&source_id)?;
        let WorkspaceSourceBinding::Path {canonical_path,..}=&binding else {return Err("Invalid source path binding".into())};
        let target=self.root(canonical_path)?;
        let id=self.coordinator(InstanceKind::Architect)?;
        let expected=self.registry.workspace_metadata(&id)?.lease;
        self.enqueue(id,target,WorkspaceKind::ArchitectRepo,expected,Some(binding),None,vec![])
    }
    pub fn hand_over_to_manager(self: &Arc<Self>, source_id: String) -> Result<WorkspaceChange> { let source = self.source(&source_id)?; let id = self.coordinator(InstanceKind::Manager)?; let expected = self.registry.workspace_metadata(&id)?.lease; let context = self.prepare_ownership(&id, &source_id, &source.definition.input.project_path)?; self.enqueue(id, context.target, WorkspaceKind::ManagerHandover, expected, Some(WorkspaceSourceBinding::Grant { source_id, expected: source.lease, canonical_path: Self::canonical(&source.definition.input.project_path)? }), None, context.repositories) }
    pub fn return_to_manager(self: &Arc<Self>, source_id: String, human_request_id: String) -> Result<WorkspaceChange> { let source = self.source(&source_id)?; let id = source.lease.manager_id.clone().ok_or("Worker is not controlled by The Manager")?; if source.human_request_id.as_deref() != Some(&human_request_id) { return Err("Human request changed; return control again".into()); } let expected = self.registry.workspace_metadata(&id)?.lease; let context = self.prepare_ownership(&id, &source_id, &source.definition.input.project_path)?; self.enqueue(id, context.target, WorkspaceKind::ManagerReturn, expected, Some(WorkspaceSourceBinding::Return { source_id, expected: source.lease, human_request_id, canonical_path: Self::canonical(&source.definition.input.project_path)? }), None, context.repositories) }
    pub fn enqueue_dependency_plan(self: &Arc<Self>, handoff_id: String, manager_id: String, expected: ControlLease, target: String, repositories: Vec<String>) -> Result<WorkspaceChange> { self.enqueue(manager_id, target, WorkspaceKind::DependencyPlan, expected, None, Some(handoff_id), repositories) }
    fn enqueue(self: &Arc<Self>, id: String, target: String, kind: WorkspaceKind, expected: ControlLease, source: Option<WorkspaceSourceBinding>, dependency: Option<String>, repositories: Vec<String>) -> Result<WorkspaceChange> {
        let source_instance_id = source.as_ref().map(|s| match s { WorkspaceSourceBinding::Path {source_id,..} | WorkspaceSourceBinding::Grant {source_id,..} | WorkspaceSourceBinding::Return {source_id,..} => source_id.clone() });
        let change = WorkspaceChange { request_id: uuid::Uuid::new_v4().to_string(), coordinator_id: id.clone(), source_instance_id, target_folder: target, kind, status: WorkspaceStatus::Pending, error: None };
        let spawn = { let mut state = self.state.lock(); if state.closing { return Err("Application is shutting down".into()); } let spawn = !state.queues.contains_key(&id); state.queues.entry(id.clone()).or_default().push_back(change.request_id.clone()); state.order.push_back(change.request_id.clone()); state.jobs.insert(change.request_id.clone(), Job {change: change.clone(), expected, source, dependency, repositories, committed:false,ready_claimed:false,connection:None}); if spawn { state.workers += 1; } spawn };
        self.publish(); if spawn { let service = self.clone(); std::thread::spawn(move || service.worker(id)); } Ok(change)
    }
    fn job(&self, request: &str) -> Result<Job> { self.state.lock().jobs.get(request).cloned().ok_or_else(|| "Unknown workspace request".into()) }
    fn terminal(&self, request: &str, status: WorkspaceStatus, error: Option<String>) {
        let dependency = { let mut state = self.state.lock(); let Some(job) = state.jobs.get_mut(request) else {return}; if job.change.status.terminal() {return} job.change.status = status; job.change.error = error.clone(); let dependency = job.dependency.clone(); let coordinator = job.change.coordinator_id.clone(); let terminal:Vec<String> = state.order.iter().filter(|id| state.jobs.get(*id).is_some_and(|j| j.change.coordinator_id == coordinator && j.change.status.terminal())).cloned().collect(); for id in terminal.iter().take(terminal.len().saturating_sub(32)) {state.jobs.remove(id); state.order.retain(|key| key != id);} self.changed.notify_all(); dependency };
        if status != WorkspaceStatus::Applied { if let (Some(id), Some(service)) = (dependency,self.dependency_updates.get().and_then(Weak::upgrade)) { if let Err(error)=service.fail_workspace_handoff(&id, error.as_deref().unwrap_or("Workspace request cancelled")) {eprintln!("Persisting cancelled dependency workspace handoff: {error}");} } } self.publish();
    }
    fn worker(self: Arc<Self>, coordinator: String) {
        loop {
            let request = { let mut state = self.state.lock(); match state.queues.get(&coordinator).and_then(|q|q.front()).cloned() {Some(request)=>request,None=>{state.queues.remove(&coordinator);state.workers-=1;self.changed.notify_all();return;}} };
            if let Err(error) = self.dispatch(&request) { self.terminal(&request, WorkspaceStatus::Failed, Some(error)); }
            let mut state = self.state.lock(); while state.jobs.get(&request).is_some_and(|j|!j.change.status.terminal()) {self.changed.wait(&mut state);} if let Some(queue) = state.queues.get_mut(&coordinator) {queue.pop_front();}
        }
    }
    fn dispatch(&self, request: &str) -> Result<()> {
        let mut job = self.job(request)?;
        if job.change.status.terminal() {return Ok(())}
        let metadata = self.registry.workspace_metadata(&job.change.coordinator_id)?;
        if let (Some(handoff),Some(service))=(&job.dependency,self.dependency_updates.get().and_then(Weak::upgrade)) {service.validate_workspace_handoff(handoff)?;}
        if metadata.lease != job.expected { return Err("Coordinator session changed; submit workspace request again".into()); }
        if matches!(job.change.kind,WorkspaceKind::ManagerHandover|WorkspaceKind::ManagerReturn) { self.refresh_ownership(request, &mut job)?; }
        if !metadata.running { return self.dispatch_offline(request, job); }
        self.send_workspace_change(request, &job)
    }
    fn refresh_ownership(&self, request: &str, job: &mut Job) -> Result<()> {
        let source_id = job.change.source_instance_id.as_deref().ok_or("Missing source")?;
        let source = self.source(source_id)?;
        let context=self.prepare_ownership(&job.change.coordinator_id,source_id,&source.definition.input.project_path)?;
        job.change.target_folder=context.target;job.repositories=context.repositories;
        self.state.lock().jobs.insert(request.into(),job.clone());
        self.publish();
        Ok(())
    }
    fn send_workspace_change(&self, request: &str, job: &Job) -> Result<()> {
        self.hub.workspace_control(&job.change.coordinator_id,&job.expected,"workspace_change",json!({"requestId":request,"targetFolder":job.change.target_folder,"kind":job.change.kind,"repositoryFolders":job.repositories}))?;
        Ok(())
    }
    fn dispatch_offline(&self, request: &str, mut job: Job) -> Result<()> {
        self.offline(&job)?;
        if self.state.lock().closing {
            self.cancel_closed_handoff(&job);
            self.terminal(request,WorkspaceStatus::Applied,None);
            return Ok(());
        }
        if self.restart_coordinator(request, &mut job)? { return Ok(()); }
        self.terminal(request, WorkspaceStatus::Applied,None);
        Ok(())
    }
    fn cancel_closed_handoff(&self, job: &Job) {
        if let (Some(handoff),Some(service))=(&job.dependency,self.dependency_updates.get().and_then(Weak::upgrade)) {
            if let Err(error)=service.fail_workspace_handoff(handoff,"Application closed before Manager startup") {eprintln!("Persisting cancelled handoff: {error}");}
        }
    }
    /// Rebases queued requests onto the offline-applied lease; true when a dependency handoff was re-sent and still awaits the native ready.
    fn restart_coordinator(&self, request: &str, job: &mut Job) -> Result<bool> {
        if !matches!(job.change.kind,WorkspaceKind::ManagerHandover|WorkspaceKind::ManagerReturn|WorkspaceKind::DependencyPlan) {
            let resulting=self.registry.workspace_metadata(&job.change.coordinator_id)?.lease;
            self.rebase(job,&resulting);
            return Ok(false);
        }
        self.registry.start(&job.change.coordinator_id,Origin::Human)?;
        self.hub.wait_ready(&job.change.coordinator_id)?;
        let resulting = self.registry.workspace_metadata(&job.change.coordinator_id)?.lease;
        self.rebase(job, &resulting);
        if job.dependency.is_none() { return Ok(false); }
        job.expected = resulting;
        job.source = None;
        job.change.status=WorkspaceStatus::Pending;
        self.state.lock().jobs.insert(request.into(),job.clone());
        self.send_workspace_change(request, job)?;
        Ok(true)
    }
    fn offline(&self, job:&Job)->Result<()> {
        {
            let mut state=self.state.lock();
            if state.closing {return Err("Application is shutting down".into());}
            let active=state.jobs.get_mut(&job.change.request_id).ok_or("Unknown workspace request")?;
            if active.change.status!=WorkspaceStatus::Pending {return Err("Workspace request cancelled".into());}
            self.registry.begin_workspace_change(&job.change.coordinator_id,&job.change.request_id,&job.expected)?;
            active.change.status=WorkspaceStatus::Applying;
        }
        self.publish();
        let result=(|| {
            let metadata=self.registry.workspace_metadata(&job.change.coordinator_id)?;
            let commit = |identity: &Value| -> Result<()> {
                if self.state.lock().closing {return Err("Application is shutting down; offline workspace move must roll back".into());}
                self.registry.commit_workspace_change(&WorkspaceCommit {request_id:job.change.request_id.clone(),coordinator_id:job.change.coordinator_id.clone(),expected:job.expected.clone(),target_folder:job.change.target_folder.clone(),session_id:identity["sessionId"].as_str().map(String::from),session_file:identity["sessionFile"].as_str().map(PathBuf::from),workspace_epoch:job.expected.workspace_epoch,source:job.source.clone()})?;
                Ok(())
            };
            if let Some(file)=metadata.definition.session_path {
                crate::workspace_native::relocate(&self.root,&self.registry.home,json!({"sessionFile":file,"targetFolder":job.change.target_folder,"profile":metadata.definition.input.profile,"cwd":metadata.definition.input.project_path}),commit)
            } else { commit(&json!({"sessionId":metadata.lease.session_id,"sessionFile":null})) }
        })();
        self.registry.finish_workspace_change(&job.change.coordinator_id,&job.change.request_id); result
    }
    fn rebase(&self, completed:&Job, resulting:&ControlLease) {
        let mut state=self.state.lock();
        for job in state.jobs.values_mut() {
            if job.change.coordinator_id==completed.change.coordinator_id && job.change.request_id!=completed.change.request_id && job.change.status==WorkspaceStatus::Pending && job.expected==completed.expected { job.expected=resulting.clone(); }
        }
    }
    pub fn begin(&self,id:&str,generation:u64,connection:&str,request:&str)->Result<Value> {
        {
            let mut state=self.state.lock();
            if state.closing {return Err("Application is shutting down".into());}
            if state.queues.get(id).and_then(|q|q.front()).map(String::as_str)!=Some(request) {return Err("Workspace request is not the FIFO head".into());}
            let job=state.jobs.get_mut(request).ok_or("Unknown workspace request")?;
            if job.change.coordinator_id!=id || job.expected.generation!=generation || job.change.status!=WorkspaceStatus::Pending {return Err("Invalid workspace request binding".into());}
            self.registry.begin_workspace_change(id,request,&job.expected)?;
            job.change.status=WorkspaceStatus::Applying;job.connection=Some(connection.into());
        }
        self.publish();Ok(json!({"reserved":true}))
    }
    pub fn ready(&self,id:&str,generation:u64,connection:&str,payload:&Value)->Result<Value> {
        let request=payload["requestId"].as_str().ok_or("requestId is required")?;
        let job={
            let mut state=self.state.lock();
            if state.closing {return Err("Application is shutting down; workspace move must roll back".into());}
            let job=state.jobs.get_mut(request).ok_or("Unknown workspace request")?;
            if job.change.coordinator_id!=id || job.expected.generation!=generation || job.connection.as_deref()!=Some(connection) || job.change.status!=WorkspaceStatus::Applying {return Err("Invalid workspace ready binding".into());}
            if job.committed {return Ok(json!({"committed":true}));}
            if job.ready_claimed {return Err("Workspace ready is already being committed".into());}
            job.ready_claimed=true;job.clone()
        };
        let actual=Self::canonical(payload["cwd"].as_str().ok_or("Native cwd is required")?)?; if actual.to_lowercase().replace('\\',"/")!=job.change.target_folder.to_lowercase().replace('\\',"/") {return Err("Native workspace destination does not match request".into());}
        let lease=self.registry.commit_workspace_change(&WorkspaceCommit {request_id:request.into(),coordinator_id:id.into(),expected:job.expected.clone(),target_folder:actual,session_id:payload["sessionId"].as_str().map(String::from),session_file:payload["sessionFile"].as_str().map(PathBuf::from),workspace_epoch:payload["workspaceEpoch"].as_u64().ok_or("workspaceEpoch is required")?,source:job.source.clone()})?;
        {let mut state=self.state.lock();if let Some(job)=state.jobs.get_mut(request){job.committed=true;}}
        self.rebase(&job,&lease);
        self.publish();
        if let Some(handoff)=job.dependency { if let Some(service)=self.dependency_updates.get().and_then(Weak::upgrade) { if let Err(error)=service.complete_workspace_handoff(&handoff,request,&lease) { eprintln!("Dependency workspace delivery: {error:?}"); } } }
        Ok(json!({"committed":true}))
    }
    pub fn complete(&self,id:&str,generation:u64,connection:&str,payload:&Value,failed:bool)->Result<Value> {
        let request=payload["requestId"].as_str().ok_or("requestId is required")?;
        let job=self.job(request)?;
        if job.change.coordinator_id!=id || job.expected.generation!=generation {return Err("Invalid workspace completion binding".into());}
        if job.change.status.terminal() {return Ok(json!({"completed":true}));}
        let pending_failure=failed&&job.change.status==WorkspaceStatus::Pending&&job.connection.is_none();
        if !pending_failure&&job.connection.as_deref()!=Some(connection) {return Err("Invalid workspace completion connection".into());}
        if !failed&&!job.committed {return Err("Workspace completion precedes commit".into());}
        self.registry.finish_workspace_change(id,request);
        if failed&&!job.committed {
            if payload["identity"].is_object() {self.registry.apply_identity(id,generation,&payload["identity"])?;}
            self.terminal(request,WorkspaceStatus::Failed,Some(payload["error"].as_str().unwrap_or("Native workspace move failed").into()));
        } else {self.terminal(request,WorkspaceStatus::Applied,None);}
        Ok(json!({"completed":true}))
    }
    pub fn take_back_control(&self,id:&str)->Result<()> {
        self.registry.take_back_control(id)?;
        let requests={let state=self.state.lock();state.jobs.values().filter(|j|j.change.source_instance_id.as_deref()==Some(id)&&matches!(j.change.kind,WorkspaceKind::ManagerHandover|WorkspaceKind::ManagerReturn)&&!j.change.status.terminal()).map(|j|j.change.request_id.clone()).collect::<Vec<_>>()};
        for request in requests {
            let job=self.job(&request)?;
            if job.change.status==WorkspaceStatus::Pending {
                self.terminal(&request,WorkspaceStatus::Cancelled,None);
                let hub=self.hub.clone();
                std::thread::spawn(move || {let _=hub.workspace_control(&job.change.coordinator_id,&job.expected,"cancel_workspace_change",json!({"requestId":request}));});
            }
        }
        self.publish();Ok(())
    }
    pub fn allows_plan_prompt(&self,id:&str,request:&str)->bool {
        let state=self.state.lock();
        !state.closing&&state.jobs.get(request).is_some_and(|j|j.change.coordinator_id==id&&j.change.kind==WorkspaceKind::DependencyPlan&&j.committed&&j.change.status==WorkspaceStatus::Applying)
    }
    pub fn disconnected(&self,id:&str,generation:u64,connection:&str) {
        let jobs={let state=self.state.lock();state.jobs.values().filter(|j|j.change.coordinator_id==id&&j.expected.generation==generation&&!j.change.status.terminal()&&j.connection.as_deref().is_none_or(|c|c==connection)).cloned().collect::<Vec<_>>()};
        for job in jobs {
            if job.change.status==WorkspaceStatus::Pending {self.terminal(&job.change.request_id,WorkspaceStatus::Cancelled,Some("Native connection closed before workspace transition".into()));}
            else {let mut state=self.state.lock();if let Some(active)=state.jobs.get_mut(&job.change.request_id){active.change.error=Some("Workspace acknowledgement disconnected; waiting for actual native state reconciliation".into());}}
        }
        self.publish();
    }
    pub fn reconcile(&self,id:&str,generation:u64,payload:&Value)->Result<Value> {
        let request=payload["requestId"].as_str().ok_or("requestId is required")?;
        let job=self.job(request)?;
        if job.change.coordinator_id!=id || job.expected.generation!=generation || job.change.status!=WorkspaceStatus::Applying {return Err("Invalid workspace reconciliation binding".into());}
        // A new authenticated connection reports only after the original native transaction has unwound.
        self.registry.finish_workspace_change(id,request);
        self.registry.apply_identity(id,generation,&payload["identity"])?;
        if job.committed {self.terminal(request,WorkspaceStatus::Applied,None);}
        else {self.terminal(request,WorkspaceStatus::Failed,Some(payload["error"].as_str().unwrap_or("Workspace acknowledgement was lost; native state reconciled").into()));}
        Ok(json!({"reconciled":true,"committed":job.committed}))
    }
    pub fn process_exited(&self,id:&str,generation:u64) {
        let jobs={let state=self.state.lock();state.jobs.values().filter(|j|j.change.coordinator_id==id&&j.expected.generation==generation&&!j.change.status.terminal()).cloned().collect::<Vec<_>>()};
        for job in jobs {
            self.registry.finish_workspace_change(id,&job.change.request_id);
            self.terminal(&job.change.request_id,if job.committed {WorkspaceStatus::Applied}else{WorkspaceStatus::Failed},Some("Coordinator exited before workspace acknowledgement".into()));
        }
    }
    pub fn shutdown(&self) {
        let pending={let mut state=self.state.lock();state.closing=true;state.jobs.values().filter(|j|!j.change.status.terminal()).cloned().collect::<Vec<_>>()};
        for job in pending {
            let lease=self.registry.workspace_metadata(&job.change.coordinator_id).map(|m|m.lease).unwrap_or_else(|_|job.expected.clone());
            let _=self.hub.workspace_control(&job.change.coordinator_id,&lease,"cancel_workspace_change",json!({"requestId":job.change.request_id,"shutdown":true}));
            if job.change.status==WorkspaceStatus::Pending {self.terminal(&job.change.request_id,WorkspaceStatus::Cancelled,None);}
        }
        let mut state=self.state.lock();
        while state.workers>0 {self.changed.wait(&mut state);}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture { root:PathBuf, registry:Arc<Registry>, hub:Arc<ManagerHub>, workspace:Arc<WorkspaceService>, project:PathBuf }
    impl Fixture {
        fn new()->Self {
            let root=std::env::temp_dir().join(format!("omp-workspace-test-{}",uuid::Uuid::new_v4()));
            let config=root.join("config");let project=root.join("project");
            std::fs::create_dir_all(&config).unwrap();std::fs::create_dir_all(&project).unwrap();
            let registry=Arc::new(Registry::load(config.join("instances.json"),root.clone()).unwrap());
            registry.ensure_coordinators(&[project.to_string_lossy().into_owned()]).unwrap();
            let hub=ManagerHub::bind(config.join("omp-manager-bridge.mjs")).unwrap();
            hub.bind_registry(Arc::downgrade(&registry)).unwrap();
            registry.install_manager_hub(hub.clone()).unwrap();
            let workspace=WorkspaceService::new(registry.clone(),hub.clone(),config);
            hub.install_workspace_service(Arc::downgrade(&workspace)).unwrap();
            Self {root,registry,hub,workspace,project}
        }
        fn wait(&self,request:&str)->WorkspaceChange {
            let mut state=self.workspace.state.lock();
            let result=self.workspace.changed.wait_while_for(&mut state,|s|s.jobs.get(request).is_some_and(|j|!j.change.status.terminal()),std::time::Duration::from_secs(5));
            assert!(!result.timed_out(),"workspace transition did not settle");
            state.jobs[request].change.clone()
        }
    }
    impl Drop for Fixture {fn drop(&mut self){self.workspace.shutdown();self.hub.shutdown();let _=std::fs::remove_dir_all(&self.root);}}
    #[test]
    fn stopped_definition_changes_are_fifo_and_do_not_launch() {
        let fixture=Fixture::new();let id=fixture.registry.coordinator_id(InstanceKind::Manager).unwrap();
        let second=fixture.root.join("second");let third=fixture.root.join("third");
        std::fs::create_dir_all(&second).unwrap();std::fs::create_dir_all(&third).unwrap();
        let first=fixture.workspace.change_directory(id.clone(),second.to_string_lossy().into_owned(),0).unwrap();
        let next=fixture.workspace.change_directory(id.clone(),third.to_string_lossy().into_owned(),0).unwrap();
        assert_eq!(fixture.wait(&first.request_id).status,WorkspaceStatus::Applied);
        assert_eq!(fixture.wait(&next.request_id).status,WorkspaceStatus::Applied);
        let metadata=fixture.registry.workspace_metadata(&id).unwrap();
        assert!(!metadata.running);assert_eq!(metadata.lease.generation,0);
        assert_eq!(metadata.definition.input.project_path,WorkspaceService::canonical(&third.to_string_lossy()).unwrap());
    }
    #[test]
    fn architect_repo_transfer_uses_metadata_and_keeps_both_agents_stopped() {
        let fixture=Fixture::new();
        let repo=fixture.root.join("source");std::fs::create_dir_all(repo.join(".git")).unwrap();
        let worker=fixture.registry.create(crate::instance::CreateInput {project_path:repo.to_string_lossy().into_owned(),..Default::default()}).unwrap();
        let before=fixture.registry.workspace_metadata(&worker.id).unwrap();
        let change=fixture.workspace.give_repo_to_architect(worker.id.clone()).unwrap();
        assert_eq!(fixture.wait(&change.request_id).status,WorkspaceStatus::Applied);
        let after=fixture.registry.workspace_metadata(&worker.id).unwrap();
        assert_eq!(after.lease,before.lease);assert_eq!(after.definition.input.project_path,before.definition.input.project_path);
        assert!(!after.running);
        let architect=fixture.registry.workspace_metadata(&change.coordinator_id).unwrap();
        assert!(!architect.running);assert_eq!(architect.definition.input.project_path,WorkspaceService::canonical(&repo.to_string_lossy()).unwrap());
    }
    #[test]
    fn manual_generation_and_non_worker_sources_are_rejected_before_admission() {
        let fixture=Fixture::new();let id=fixture.registry.coordinator_id(InstanceKind::Manager).unwrap();
        assert!(fixture.workspace.change_directory(id.clone(),fixture.project.to_string_lossy().into_owned(),99).is_err());
        assert!(fixture.workspace.give_repo_to_architect(id).is_err());
        assert!(fixture.workspace.list().is_empty());
    }
    fn input(path:&Path)->crate::instance::CreateInput { crate::instance::CreateInput {project_path:path.to_string_lossy().into_owned(),..Default::default()} }
    fn repo(fixture:&Fixture,name:&str,children:&[&str])->PathBuf {
        let repo=fixture.root.join(name);std::fs::create_dir_all(repo.join(".git")).unwrap();
        for child in children {std::fs::create_dir_all(repo.join(child)).unwrap();}
        repo
    }
    fn canon(path:&Path)->String { WorkspaceService::canonical(&path.to_string_lossy()).unwrap() }
    fn same(left:&str,right:&str)->bool { instance::same_folder(left,right) }
    #[test]
    fn one_connected_agent_moves_the_manager_to_its_actual_folder_and_several_share_a_computed_one() {
        let fixture=Fixture::new();let manager=fixture.registry.coordinator_id(InstanceKind::Manager).unwrap();
        let b=repo(&fixture,"B",&["src","lib"]);let c=repo(&fixture,"C",&["x"]);
        let incoming=fixture.registry.create(input(&b.join("src"))).unwrap();
        let lease=fixture.registry.workspace_metadata(&incoming.id).unwrap();
        let single=fixture.workspace.ownership_context(&manager,&incoming.id,&incoming.project_path).unwrap();
        // The agent's actual folder, not its Git root and never the Manager's old folder.
        assert!(same(&single.target,&canon(&b.join("src"))));assert!(!single.computed);
        assert_eq!(single.repositories.len(),1);assert!(same(&single.repositories[0],&canon(&b)));
        // Helpers and plan agents never count.
        fixture.registry.create_helper(&incoming.id,crate::instance::HelperPurpose::Diff,incoming.project_path.clone()).unwrap();
        fixture.registry.create_planner(&incoming.id).unwrap();
        assert!(same(&fixture.workspace.ownership_context(&manager,&incoming.id,&incoming.project_path).unwrap().target,&canon(&b.join("src"))));
        // An already owned (even stopped) agent counts once.
        let owned=fixture.registry.create_owned(&manager,input(&b.join("src"))).unwrap();
        let again=fixture.workspace.ownership_context(&manager,&owned.id,&owned.project_path).unwrap();
        assert!(same(&again.target,&canon(&b.join("src"))));assert!(!again.computed);
        // A second definition in the same repository at another subfolder: two agents, shared branch.
        let lib=fixture.registry.create(input(&b.join("lib"))).unwrap();
        let shared=fixture.workspace.ownership_context(&manager,&lib.id,&lib.project_path).unwrap();
        assert!(same(&shared.target,&canon(&b)));assert!(shared.computed);assert_eq!(shared.repositories.len(),1);
        // Two separate repositories: the common ancestor of their Git roots.
        let other=fixture.registry.create(input(&c.join("x"))).unwrap();
        let both=fixture.workspace.ownership_context(&manager,&other.id,&other.project_path).unwrap();
        assert!(same(&both.target,&canon(&fixture.root)));assert!(both.computed);assert_eq!(both.repositories.len(),2);
        // Computing a context never touches the incoming agent.
        assert_eq!(fixture.registry.workspace_metadata(&incoming.id).unwrap().lease,lease.lease);
        assert!(same(&fixture.registry.workspace_metadata(&incoming.id).unwrap().definition.input.project_path,&canon(&b.join("src"))));
    }
    #[test]
    fn handover_commit_moves_only_the_manager_to_the_single_agents_folder() {
        let fixture=Fixture::new();let manager=fixture.registry.coordinator_id(InstanceKind::Manager).unwrap();
        let b=repo(&fixture,"B",&["src"]);
        let worker=fixture.registry.create(input(&b.join("src"))).unwrap();
        let context=fixture.workspace.ownership_context(&manager,&worker.id,&worker.project_path).unwrap();
        let before=fixture.registry.workspace_metadata(&worker.id).unwrap();
        let lease=fixture.registry.workspace_metadata(&manager).unwrap().lease;
        fixture.registry.begin_workspace_change(&manager,"request",&lease).unwrap();
        fixture.registry.commit_workspace_change(&WorkspaceCommit {request_id:"request".into(),coordinator_id:manager.clone(),expected:lease.clone(),target_folder:context.target,session_id:None,session_file:None,workspace_epoch:lease.workspace_epoch,source:Some(WorkspaceSourceBinding::Grant {source_id:worker.id.clone(),expected:before.lease.clone(),canonical_path:canon(&b.join("src"))})}).unwrap();
        fixture.registry.finish_workspace_change(&manager,"request");
        assert!(same(&fixture.registry.workspace_metadata(&manager).unwrap().definition.input.project_path,&canon(&b.join("src"))));
        let after=fixture.registry.workspace_metadata(&worker.id).unwrap();
        assert!(same(&after.definition.input.project_path,&before.definition.input.project_path));
        assert_eq!(after.definition.session_path,before.definition.session_path);
        assert_eq!(after.lease.session_id,before.lease.session_id);
        assert_eq!(after.definition.manager_id.as_deref(),Some(manager.as_str()));
    }
    #[test]
    fn dependency_plan_context_keeps_single_project_git_root_semantics() {
        let fixture=Fixture::new();let manager=fixture.registry.coordinator_id(InstanceKind::Manager).unwrap();
        let b=repo(&fixture,"B",&["src"]);
        fixture.registry.create_owned(&manager,input(&b.join("src"))).unwrap();
        let context=fixture.workspace.dependency_plan_context(&canon(&b.join("src"))).unwrap();
        assert!(same(&context.manager_working_directory,&canon(&b)));
        assert_eq!(context.repository_folders.len(),1);assert!(same(&context.repository_folders[0],&canon(&b)));
    }
    #[test]
    fn computed_folders_record_only_shared_targets_once_and_survive_a_damaged_file() {
        let fixture=Fixture::new();let manager=fixture.registry.coordinator_id(InstanceKind::Manager).unwrap();
        let b=repo(&fixture,"B",&["src"]);let c=repo(&fixture,"C",&["x"]);
        let first=fixture.registry.create_owned(&manager,input(&b.join("src"))).unwrap();
        assert!(!fixture.workspace.prepare_ownership(&manager,&first.id,&first.project_path).unwrap().computed);
        assert!(fixture.workspace.computed_folders().unwrap().is_empty());
        assert!(!fixture.workspace.computed_file().exists());
        let second=fixture.registry.create_owned(&manager,input(&c.join("x"))).unwrap();
        assert!(fixture.workspace.prepare_ownership(&manager,&second.id,&second.project_path).unwrap().computed);
        assert!(fixture.workspace.prepare_ownership(&manager,&second.id,&second.project_path).unwrap().computed);
        let recorded=fixture.workspace.computed_folders().unwrap();
        assert_eq!(recorded.len(),1);assert!(same(&recorded[0],&canon(&fixture.root)));
        let saved:Value=serde_json::from_slice(&std::fs::read(fixture.workspace.computed_file()).unwrap()).unwrap();
        assert_eq!(saved["version"],1);assert_eq!(saved["folders"].as_array().unwrap().len(),1);
        std::fs::write(fixture.workspace.computed_file(),b"{ not json").unwrap();
        assert!(fixture.workspace.computed_folders().unwrap().is_empty());
        fixture.workspace.prepare_ownership(&manager,&second.id,&second.project_path).unwrap();
        assert_eq!(fixture.workspace.computed_folders().unwrap().len(),1);
    }
    #[test]
    fn a_shared_target_that_is_an_agents_own_project_is_not_computed() {
        let fixture=Fixture::new();let manager=fixture.registry.coordinator_id(InstanceKind::Manager).unwrap();
        let d=repo(&fixture,"D",&["src"]);
        fixture.registry.create_owned(&manager,input(&d)).unwrap();
        let nested=fixture.registry.create_owned(&manager,input(&d.join("src"))).unwrap();
        let context=fixture.workspace.prepare_ownership(&manager,&nested.id,&nested.project_path).unwrap();
        assert!(same(&context.target,&canon(&d)));assert!(!context.computed);
        assert!(fixture.workspace.computed_folders().unwrap().is_empty());
    }
    #[test]
    fn startup_reconciliation_marks_an_existing_group_cwd_but_not_a_real_agent_folder() {
        let fixture=Fixture::new();let manager=fixture.registry.coordinator_id(InstanceKind::Manager).unwrap();
        let b=repo(&fixture,"B",&["src"]);let c=repo(&fixture,"C",&["x"]);
        fixture.registry.create_owned(&manager,input(&b.join("src"))).unwrap();
        fixture.workspace.reconcile_computed_folders().unwrap();
        assert!(fixture.workspace.computed_folders().unwrap().is_empty(),"one connected agent never computes a folder");
        fixture.registry.create_owned(&manager,input(&c.join("x"))).unwrap();
        let shared=fixture.root.join("shared");std::fs::create_dir_all(&shared).unwrap();
        let change=fixture.workspace.change_directory(manager.clone(),shared.to_string_lossy().into_owned(),0).unwrap();
        assert_eq!(fixture.wait(&change.request_id).status,WorkspaceStatus::Applied);
        fixture.workspace.reconcile_computed_folders().unwrap();fixture.workspace.reconcile_computed_folders().unwrap();
        let recorded=fixture.workspace.computed_folders().unwrap();
        assert_eq!(recorded.len(),1);assert!(same(&recorded[0],&canon(&shared)));
        let own=fixture.workspace.change_directory(manager.clone(),b.join("src").to_string_lossy().into_owned(),0).unwrap();
        assert_eq!(fixture.wait(&own.request_id).status,WorkspaceStatus::Applied);
        fixture.workspace.reconcile_computed_folders().unwrap();
        assert_eq!(fixture.workspace.computed_folders().unwrap().len(),1,"an agent's own folder is a project, not a computed folder");
    }
    #[test]
    fn ordinary_agent_directory_changes_keep_owner_and_session_while_helpers_and_plans_are_rejected() {
        let fixture=Fixture::new();let manager=fixture.registry.coordinator_id(InstanceKind::Manager).unwrap();
        let a=repo(&fixture,"A",&[]);let b=repo(&fixture,"B",&[]);
        let worker=fixture.registry.create_owned(&manager,input(&a)).unwrap();
        let before=fixture.registry.workspace_metadata(&worker.id).unwrap();
        let manager_before=fixture.registry.workspace_metadata(&manager).unwrap();
        assert_eq!(fixture.workspace.change_directory(worker.id.clone(),b.to_string_lossy().into_owned(),99).unwrap_err(),"stale_generation");
        let helper=fixture.registry.create_helper(&worker.id,crate::instance::HelperPurpose::Diff,worker.project_path.clone()).unwrap();
        let planner=fixture.registry.create_planner(&worker.id).unwrap();
        for id in [&helper.id,&planner.id] {
            assert_eq!(fixture.workspace.change_directory(id.clone(),b.to_string_lossy().into_owned(),0).unwrap_err(),"Only agent working directories can be changed");
        }
        assert!(fixture.workspace.change_directory(worker.id.clone(),fixture.registry.config_root().to_string_lossy().into_owned(),0).unwrap_err().contains("app-managed"));
        assert!(fixture.workspace.list().is_empty());
        let change=fixture.workspace.change_directory(worker.id.clone(),b.to_string_lossy().into_owned(),0).unwrap();
        assert_eq!(change.kind,WorkspaceKind::Manual);
        assert_eq!(fixture.wait(&change.request_id).status,WorkspaceStatus::Applied);
        let after=fixture.registry.workspace_metadata(&worker.id).unwrap();
        assert!(same(&after.definition.input.project_path,&canon(&b)));
        assert_eq!(after.lease,before.lease);assert_eq!(after.definition.manager_id.as_deref(),Some(manager.as_str()));
        assert!(!after.running);
        // Moving an owned worker does not move The Manager; a later handover/return recomputes it.
        assert!(same(&fixture.registry.workspace_metadata(&manager).unwrap().definition.input.project_path,&manager_before.definition.input.project_path));
    }
}
