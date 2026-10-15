use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{mpsc, Arc, Weak},
    time::Duration,
};

pub type Result<T, E = String> = std::result::Result<T, E>;
const MAX_FRAME: usize = 64 * 1024 * 1024;
// Cross-process WS_CHILD hosting painted Chrome but broke its omnibox and shortcuts in the
// Windows probe. Docking the real top-level window is used instead; failures stay explicit.
pub const EMBEDDING_ERROR:&str="Browser could not be embedded";
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserSettings {
    pub headless: bool,
    pub embedded: bool,
}
impl Default for BrowserSettings {
    fn default() -> Self {
        Self {
            headless: false,
            embedded: true,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserInfo {
    pub instance_id: String,
    pub generation: u64,
    pub epoch: u64,
    pub state: String,
    pub headless: bool,
    pub embedded: bool,
    pub pid: Option<u32>,
    pub error: Option<String>,
}
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
struct Owner {
    generation: u64,
    token: String,
    info: BrowserInfo,
}
struct State {
    settings: BrowserSettings,
    owners: HashMap<String, Owner>,
    closed: bool,
}
struct Helper {
    child: Mutex<Child>,
    input: Mutex<ChildStdin>,
    pending: Mutex<HashMap<String, mpsc::SyncSender<Result<Value>>>>,
    registered: Mutex<HashMap<String, u64>>,
    #[cfg(windows)]
    job: Mutex<Option<std::os::windows::io::OwnedHandle>>,
}
pub struct BrowserSupervisor {
    root: PathBuf,
    home: PathBuf,
    state: Mutex<State>,
    helper: Mutex<Option<Arc<Helper>>>,
    dirty: AtomicBool,
}
impl Helper {
    fn request(&self, action: &str, payload: Value, seconds: u64) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::sync_channel(1);
        let mut bytes = serde_json::to_vec(&json!({"id":id,"action":action,"payload":payload}))
            .map_err(|e| e.to_string())?;
        if bytes.len() >= MAX_FRAME {
            return Err("browser frame exceeds transport limit".into());
        }
        bytes.push(b'\n');
        self.pending.lock().insert(id.clone(), tx);
        let written = self.input.lock().write_all(&bytes);
        if let Err(error) = written {
            self.pending.lock().remove(&id);
            return Err(format!("Write browser request: {error}"));
        }
        let result = rx
            .recv_timeout(Duration::from_secs(seconds))
            .map_err(|_| "Browser service request timed out or disconnected".to_string());
        self.pending.lock().remove(&id);
        result?
    }
    fn kill(&self) {
        let mut child = self.child.lock();
        let _ = child.kill();
        let _ = child.wait();
        #[cfg(windows)]
        {
            self.job.lock().take();
        }
    }
}
impl BrowserSupervisor {
    pub fn new(root: PathBuf, home: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            root,
            home,
            state: Mutex::new(State {
                settings: BrowserSettings::default(),
                owners: HashMap::new(),
                closed: false,
            }),
            helper: Mutex::new(None),
            dirty: AtomicBool::new(true),
        })
    }
    pub fn materialize(&self) -> Result<()> {
        for (name, source) in [
            (
                "omp-browser-service.mjs",
                include_str!("../../bridge/omp-browser-service.mjs"),
            ),
            (
                "omp-browser.mjs",
                include_str!("../../bridge/omp-browser.mjs"),
            ),
        ] {
            let file = self.root.join(name);
            if fs::read_to_string(&file).ok().as_deref() != Some(source) {
                fs::write(file, source).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
    pub fn configure(&self, settings: BrowserSettings) -> Result<()> {
        let helper = self.helper.lock().clone();
        if let Some(helper) = helper {
            helper.request(
                "configure",
                serde_json::to_value(settings).map_err(|e| e.to_string())?,
                30,
            )?;
        }
        let mut state = self.state.lock();
        if state.closed {
            return Err("Browser service closed".into());
        }
        state.settings = settings;
        // Owners the helper has not launched report the new default until its state frames take over.
        for owner in state.owners.values_mut() {
            if owner.info.pid.is_none() {
                owner.info.embedded = settings.embedded;
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }
    pub fn settings(&self) -> BrowserSettings {
        self.state.lock().settings
    }
    pub fn report_embedding_error(&self, id: &str, generation: u64, message: &str) {
        let mut state = self.state.lock();
        if let Some(owner) = state.owners.get_mut(id) {
            if owner.generation == generation && owner.info.error.as_deref() != Some(message) {
                owner.info.error = Some(message.into());
                owner.info.state = "error".into();
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
    }
    pub fn register(&self, id: &str, generation: u64, token: &str) -> Result<()> {
        let mut state = self.state.lock();
        if state.closed {
            return Err("Browser service closed".into());
        }
        let (headless, embedded) = (state.settings.headless, state.settings.embedded);
        state.owners.insert(
            id.into(),
            Owner {
                generation,
                token: token.into(),
                info: BrowserInfo {
                    instance_id: id.into(),
                    generation,
                    epoch: 0,
                    state: "closed".into(),
                    headless,
                    embedded,
                    pid: None,
                    error: None,
                },
            },
        );
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }
    fn authenticate(&self, id: &str, generation: u64) -> Result<()> {
        let state = self.state.lock();
        if !state.closed
            && state
                .owners
                .get(id)
                .is_some_and(|o| o.generation == generation && !o.token.is_empty())
        {
            Ok(())
        } else {
            Err("stale_generation".into())
        }
    }
    fn connect(self: &Arc<Self>) -> Result<Arc<Helper>> {
        let mut slot = self.helper.lock();
        if let Some(helper) = slot.as_ref() {
            if helper
                .child
                .lock()
                .try_wait()
                .map_err(|e| e.to_string())?
                .is_none()
            {
                return Ok(helper.clone());
            }
        }
        if self.state.lock().closed {
            return Err("Browser service closed".into());
        }
        let sdk = crate::config::sdk_dir(&self.home)?;
        let mut cmd = Command::new(std::env::var_os("BUN_BIN").unwrap_or_else(|| "bun".into()));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        let directory = self.root.join("terminal-browsers");
        // Per-run post-mortem log; diagnostics must never stop the browser from starting.
        let log = fs::create_dir_all(&directory)
            .and_then(|_| fs::File::create(directory.join("browser-service.log")))
            .map_or_else(|_| Stdio::null(), Stdio::from);
        let mut child = cmd
            .arg(self.root.join("omp-browser-service.mjs"))
            .arg(&self.root)
            .arg(sdk)
            .env_remove("PI_COMPILED")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .map_err(|e| format!("Browser requires Bun and the installed OMP SDK: {e}"))?;
        #[cfg(windows)]
        let job = match crate::process_tree::contain(child.id(), "browser helper") {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let input = child
            .stdin
            .take()
            .ok_or("Browser helper stdin unavailable")?;
        let output = child
            .stdout
            .take()
            .ok_or("Browser helper stdout unavailable")?;
        let helper = Arc::new(Helper {
            child: Mutex::new(child),
            input: Mutex::new(input),
            pending: Mutex::new(HashMap::new()),
            registered: Mutex::new(HashMap::new()),
            #[cfg(windows)]
            job: Mutex::new(Some(job)),
        });
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let reader_helper = helper.clone();
        let supervisor = Arc::downgrade(self);
        std::thread::spawn(move || Self::read_helper(output, reader_helper, supervisor, ready_tx));
        if ready_rx.recv_timeout(Duration::from_secs(30)).is_err() {
            helper.kill();
            return Err("Browser helper failed to start within 30 seconds".into());
        }
        let settings = self.state.lock().settings;
        if let Err(error) = helper.request(
            "configure",
            serde_json::to_value(settings).map_err(|e| e.to_string())?,
            30,
        ) {
            helper.kill();
            return Err(error);
        }
        *slot = Some(helper.clone());
        Ok(helper)
    }
    fn read_helper(
        output: std::process::ChildStdout,
        helper: Arc<Helper>,
        supervisor: Weak<Self>,
        ready: mpsc::SyncSender<()>,
    ) {
        let mut reader = BufReader::new(output);
        let mut observed = HashMap::<String, BrowserInfo>::new();
        loop {
            let mut line = Vec::new();
            let count = Read::by_ref(&mut reader)
                .take((MAX_FRAME + 1) as u64)
                .read_until(b'\n', &mut line);
            let Ok(count) = count else { break };
            if count == 0 || count > MAX_FRAME || line.last() != Some(&b'\n') {
                break;
            }
            let Ok(frame) = serde_json::from_slice::<Value>(&line) else {
                break;
            };
            match frame["type"].as_str() {
                Some("ready") => {
                    let _ = ready.try_send(());
                }
                Some("response") => {
                    if let Some(id) = frame["id"].as_str() {
                        if let Some(tx) = helper.pending.lock().remove(id) {
                            let result = if frame["ok"] == true {
                                Ok(frame["data"].clone())
                            } else {
                                Err(frame["error"]
                                    .as_str()
                                    .unwrap_or("Browser helper failed")
                                    .into())
                            };
                            let _ = tx.send(result);
                        }
                    }
                }
                Some("state") => {
                    if let (Some(supervisor), Ok(info)) = (
                        supervisor.upgrade(),
                        serde_json::from_value::<BrowserInfo>(frame["data"].clone()),
                    ) {
                        observed.insert(info.instance_id.clone(), info.clone());
                        let mut state = supervisor.state.lock();
                        if let Some(owner) = state.owners.get_mut(&info.instance_id) {
                            if owner.generation == info.generation && info.epoch >= owner.info.epoch && owner.info != info
                            {
                                owner.info = info;
                                supervisor.dirty.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                }
                _ => break,
            }
        }
        for (_, tx) in helper.pending.lock().drain() {
            let _ = tx.send(Err("Browser helper disconnected".into()));
        }
        helper.kill();
        if let Some(supervisor) = supervisor.upgrade() {
            let mut state = supervisor.state.lock();
            for (id, last) in observed {
                if let Some(owner) = state.owners.get_mut(&id) {
                    if owner.info.generation == last.generation
                        && owner.info.epoch == last.epoch
                        && owner.info.pid.is_some()
                    {
                        owner.info.pid = None;
                        owner.info.state = "error".into();
                        owner.info.error = Some("Browser helper disconnected".into());
                        supervisor.dirty.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
    }
    fn request(
        self: &Arc<Self>,
        id: &str,
        generation: u64,
        action: &str,
        mut payload: Value,
    ) -> Result<Value> {
        self.authenticate(id, generation)?;
        let helper = self.connect()?;
        {
            let mut registered = helper.registered.lock();
            if registered.get(id) != Some(&generation) {
                helper.request(
                    "register",
                    json!({"instanceId":id,"generation":generation}),
                    30,
                )?;
                registered.insert(id.into(), generation);
            }
        }
        self.authenticate(id, generation)?;
        payload["instanceId"] = json!(id);
        payload["generation"] = json!(generation);
        let result = helper.request(action, payload, 30);
        self.authenticate(id, generation)?;
        result
    }
    pub fn prepare_tab(self: &Arc<Self>, id: &str, generation: u64, name: &str, target: Option<&str>) -> Result<Value> {
        self.authenticate(id, generation)?;
        {
            let mut state = self.state.lock();
            if let Some(owner) = state.owners.get_mut(id) {
                if owner.generation == generation && owner.info.pid.is_none() && owner.info.error.is_some() {
                    owner.info.error = None;
                    owner.info.state = "closed".into();
                    self.dirty.store(true, Ordering::Relaxed);
                }
            }
        }
        let mut payload = json!({"name":name});
        if let Some(target) = target {
            payload["target"] = json!(target);
        }
        let result = self.request(id, generation, "ensureTab", payload);
        // A target that matches no page is the caller's mistake, not a failed browser.
        if let (Err(error), None) = (&result, target) {
            let mut state = self.state.lock();
            if let Some(owner) = state.owners.get_mut(id) {
                if owner.generation == generation && (owner.info.state != "error" || owner.info.error.as_ref() != Some(error)) {
                    owner.info.state = "error".into();
                    owner.info.error = Some(error.clone());
                    self.dirty.store(true, Ordering::Relaxed);
                }
            }
        }
        result
    }
    /// Docks (or reuses) this terminal's own browser; a headless one is only restarted with consent.
    pub fn open_visible(self: &Arc<Self>, id: &str, generation: u64, restart_headless: bool) -> Result<BrowserInfo> {
        let value = self.request(id, generation, "openVisible", json!({"restartHeadless":restart_headless}))?;
        let info: BrowserInfo = serde_json::from_value(value).map_err(|e| e.to_string())?;
        let mut state = self.state.lock();
        if let Some(owner) = state.owners.get_mut(id) {
            if owner.generation == generation && info.generation == generation && info.epoch >= owner.info.epoch && owner.info != info {
                owner.info = info.clone();
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
        Ok(info)
    }
    /// Tabs of this terminal's running browser (hand-opened ones are bound as tab-N); never starts anything.
    pub fn list_tabs(self: &Arc<Self>, id: &str, generation: u64) -> Result<Value> {
        self.authenticate(id, generation)?;
        if self.helper.lock().is_none() {
            return Ok(json!([]));
        }
        self.request(id, generation, "listTabs", json!({}))
    }
    pub fn close_tab(
        self: &Arc<Self>,
        id: &str,
        generation: u64,
        name: Option<&str>,
    ) -> Result<()> {
        self.request(id, generation, "closeTab", json!({"name":name}))
            .map(|_| ())
    }
    pub fn revoke(&self, id: &str, generation: u64) -> Result<()> {
        {
            let mut state = self.state.lock();
            if !state
                .owners
                .get(id)
                .is_some_and(|o| o.generation == generation)
            {
                return Ok(());
            }
            state.owners.remove(id);
            self.dirty.store(true, Ordering::Relaxed);
        }
        let helper = self.helper.lock().clone();
        if let Some(helper) = helper {
            let registered = helper.registered.lock().remove(id);
            if registered == Some(generation) {
                helper.request(
                    "revoke",
                    json!({"instanceId":id,"generation":generation}),
                    6,
                )?;
            }
        }
        Ok(())
    }
    pub fn snapshot(&self) -> Vec<BrowserInfo> {
        let mut rows = self
            .state
            .lock()
            .owners
            .values()
            .map(|o| o.info.clone())
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        rows
    }
    pub fn take_update(&self) -> Option<Vec<BrowserInfo>> {
        if self.dirty.swap(false, Ordering::Relaxed) { Some(self.snapshot()) } else { None }
    }
    pub fn shutdown(&self) {
        self.state.lock().closed = true;
        if let Some(helper) = self.helper.lock().take() {
            let _ = helper.request("shutdown", json!({}), 6);
            helper.kill();
        }
        self.state.lock().owners.clear();
    }
}

