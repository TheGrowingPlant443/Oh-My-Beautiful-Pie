use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::instance::{
    ControlLease, CreateInput, Definition, Instance, InstanceKind, Origin, Registry,
};
use crate::session;
use crate::plan;
use crate::terminal::TerminalService;
use serde_json::{json, Value};

const MAX_FRAME: usize = 64 * 1024 * 1024;
type Connection = Arc<Mutex<TcpStream>>;
pub type Result<T, E = String> = std::result::Result<T, E>;

pub struct BridgeLaunch {
    pub endpoint: String,
    pub token: String,
    pub extension_path: PathBuf,
    pub manager_setup_dir: PathBuf,
    pub architect_dir: PathBuf,
    pub terminal_dir: PathBuf,
    pub sdk_dir: Option<PathBuf>,
    pub instance_id: String,
    pub generation: u64,
    pub role: InstanceKind,
}
#[derive(Clone)]
struct Actor {
    id: String,
    generation: u64,
    role: InstanceKind,
    connection_id: String,
}
struct Client {
    token: String,
    generation: u64,
    role: InstanceKind,
    connection_id: String,
    stream: Option<Connection>,
    ready: bool,
}
struct Inflight {
    target: String,
    generation: u64,
    sender: mpsc::SyncSender<Result<Value>>,
}
#[derive(Clone)]
struct Decision {
    public_id: String,
    worker: String,
    base_id: String,
    generation: u64,
    session_id: Option<String>,
    epoch: u64,
    payload: Value,
}
impl Decision {
    fn view(&self) -> Value {
        let mut payload = self.payload.clone();
        payload["requestId"] = json!(self.public_id);
        payload["generation"] = json!(self.generation);
        payload["sessionId"] = json!(self.session_id);
        payload
    }
}
#[derive(Default)]
struct HubState {
    clients: HashMap<String, Client>,
    inflight: HashMap<String, Inflight>,
    decisions: HashMap<String, Decision>,
    owners: HashMap<String, Option<String>>,
    subscriptions: HashMap<String, bool>,
    queues: HashMap<String, VecDeque<Value>>,
    exited: HashSet<(String, u64)>,
}
pub struct ManagerHub {
    endpoint: String,
    extension_path: PathBuf,
    registry: OnceLock<Weak<Registry>>,
    browser: OnceLock<Arc<crate::browser::BrowserSupervisor>>,
    terminals: OnceLock<Arc<TerminalService>>,
    state: Mutex<HubState>,
    closing: AtomicBool,
}

fn send(stream: &Connection, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() >= MAX_FRAME {
        return Err("bridge frame exceeds transport limit".into());
    }
    bytes.push(b'\n');
    stream
        .lock()
        .map_err(|e| e.to_string())?
        .write_all(&bytes)
        .map_err(|e| e.to_string())
}
fn read_frame(reader: &mut BufReader<TcpStream>) -> Result<Option<Value>> {
    let mut line = Vec::new();
    let count = Read::by_ref(reader)
        .take((MAX_FRAME + 1) as u64)
        .read_until(b'\n', &mut line)
        .map_err(|e| e.to_string())?;
    if count == 0 {
        return Ok(None);
    }
    if count > MAX_FRAME || line.last() != Some(&b'\n') {
        return Err("invalid or oversized bridge frame".into());
    }
    serde_json::from_slice(&line)
        .map(Some)
        .map_err(|_| "invalid bridge JSON".into())
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("{key} is required"))
}

impl ManagerHub {
    pub fn bind(extension_path: PathBuf) -> Result<Arc<Self>> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let hub = Arc::new(Self {
            endpoint: listener
                .local_addr()
                .map_err(|e| e.to_string())?
                .to_string(),
            extension_path,
            registry: OnceLock::new(),
            browser: OnceLock::new(),
            terminals: OnceLock::new(),
            state: Mutex::new(HubState::default()),
            closing: AtomicBool::new(false),
        });
        let running = hub.clone();
        std::thread::spawn(move || {
            while !running.closing.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) if peer.ip().is_loopback() => {
                        let hub = running.clone();
                        std::thread::spawn(move || hub.serve(stream));
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50))
                    }
                    Err(e) => {
                        eprintln!("manager listener: {e}");
                        break;
                    }
                }
            }
        });
        Ok(hub)
    }
    pub fn bind_registry(&self, registry: Weak<Registry>) -> Result<()> {
        self.registry
            .set(registry)
            .map_err(|_| "manager registry already bound".into())
    }
    pub fn install_browser_supervisor(&self, browser: Arc<crate::browser::BrowserSupervisor>) -> Result<()> {
        self.browser.set(browser).map_err(|_| "browser supervisor already installed".into())
    }
    pub fn install_terminal_service(&self, terminals: Arc<TerminalService>) -> Result<()> {
        self.terminals.set(terminals).map_err(|_| "terminal service already installed".into())
    }
    /// Human-only: kills one command, interrupts its agent like Esc and holds new scripts until a new prompt.
    pub fn kill_terminal_command(&self, id: &str, generation: u64, session_id: &str, execution_id: &str) -> Result<()> {
        let lease = self.registry()?.lease(id, Origin::Human, false)?;
        if lease.generation != generation || lease.session_id.as_deref() != Some(session_id) {
            return Err("stale_generation".into());
        }
        self.control(id, &lease, "terminal_kill", json!({"executionId": execution_id}), Origin::Human)?;
        Ok(())
    }
    fn registry(&self) -> Result<Arc<Registry>> {
        self.registry
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| "manager registry unavailable".into())
    }
    pub fn launch(&self, definition: &Definition, generation: u64) -> Result<BridgeLaunch> {
        self.invalidate(&definition.id, generation.saturating_sub(1));
        let token = uuid::Uuid::new_v4().to_string();
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        state.clients.insert(
            definition.id.clone(),
            Client {
                token: token.clone(),
                generation,
                role: definition.kind,
                connection_id: String::new(),
                stream: None,
                ready: false,
            },
        );
        state
            .owners
            .insert(definition.id.clone(), definition.manager_id.clone());
        state
            .subscriptions
            .entry(definition.id.clone())
            .or_insert(true);
        let home = self.registry()?.home.clone();
        Ok(BridgeLaunch {
            endpoint: self.endpoint.clone(),
            token,
            extension_path: self.extension_path.clone(),
            manager_setup_dir: self
                .extension_path
                .parent()
                .ok_or("invalid bridge directory")?
                .join("manager-setup"),
            architect_dir: self
                .extension_path
                .parent()
                .ok_or("invalid bridge directory")?
                .join("architect"),
            terminal_dir: self
                .terminals
                .get()
                .ok_or("terminal service not initialized")?
                .instance_dir(&definition.id),
            sdk_dir: crate::config::sdk_dir(&home).ok(),
            instance_id: definition.id.clone(),
            generation,
            role: definition.kind,
        })
    }
    pub fn invalidate(&self, id: &str, generation: u64) {
        let stream = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            let stream = if state
                .clients
                .get(id)
                .is_some_and(|c| c.generation == generation)
            {
                state.clients.remove(id).and_then(|c| c.stream)
            } else {
                None
            };
            state
                .decisions
                .retain(|_, d| d.worker != id || d.generation != generation);
            let pending: Vec<String> = state
                .inflight
                .iter()
                .filter(|(_, p)| p.target == id && p.generation == generation)
                .map(|(id, _)| id.clone())
                .collect();
            for key in pending {
                if let Some(p) = state.inflight.remove(&key) {
                    let _ = p.sender.send(Err("stale_generation".into()));
                }
            }
            stream
        };
        if let Some(stream) = stream {
            if let Ok(stream) = stream.lock() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
        if let Some(browser) = self.browser.get() {
            if let Err(error) = browser.revoke(id, generation) { eprintln!("revoke terminal browser: {error}"); }
        }
        if let Some(terminals) = self.terminals.get() {
            terminals.invalidate(id, generation);
        }
    }
    pub fn shutdown(&self) {
        self.closing.store(true, Ordering::Relaxed);
        let streams = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            for (_, pending) in state.inflight.drain() {
                let _ = pending.sender.send(Err("manager bridge closed".into()));
            }
            state
                .clients
                .drain()
                .filter_map(|(_, c)| c.stream)
                .collect::<Vec<_>>()
        };
        for stream in streams {
            if let Ok(stream) = stream.lock() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }
    fn authenticate(&self, hello: &Value) -> Result<Actor> {
        if hello["type"] != "hello" || hello["protocol"] != 1 || hello["id"] != "hello" {
            return Err("unauthorized".into());
        }
        let id = text(hello, "instanceId")?;
        let generation = hello["generation"].as_u64().ok_or("unauthorized")?;
        let (role, valid) = {
            let state = self.state.lock().map_err(|e| e.to_string())?;
            let client = state.clients.get(id).ok_or("unauthorized")?;
            (
                client.role,
                client.generation == generation
                    && hello["token"].as_str() == Some(client.token.as_str()),
            )
        };
        if !valid {
            return Err("unauthorized".into());
        }
        if self.registry()?.lease(id, Origin::Human, false)?.generation != generation {
            return Err("stale_generation".into());
        }
        Ok(Actor {
            id: id.into(),
            generation,
            role,
            connection_id: uuid::Uuid::new_v4().to_string(),
        })
    }
    fn serve(self: Arc<Self>, stream: TcpStream) {
        // Windows accepts inherit the listener's non-blocking flag.
        if stream.set_nonblocking(false).is_err() {
            return;
        }
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let reader_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut reader = BufReader::new(reader_stream);
        let hello = match read_frame(&mut reader) {
            Ok(Some(v)) => v,
            _ => return,
        };
        let actor = match self.authenticate(&hello) {
            Ok(a) => a,
            Err(_) => return,
        };
        // The 10 s hello deadline must not outlive authentication: Windows keeps it on the cloned reader.
        let _ = reader.get_ref().set_read_timeout(None);
        let _ = stream.set_read_timeout(None);
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        let writer = Arc::new(Mutex::new(stream));
        let previous = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            let Some(client) = state.clients.get_mut(&actor.id) else {
                return;
            };
            if client.generation != actor.generation {
                return;
            }
            client.connection_id = actor.connection_id.clone();
            client.ready = false;
            client.stream.replace(writer.clone())
        };
        if let Some(previous) = previous {
            if let Ok(s) = previous.lock() {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
        if send(&writer,&json!({"type":"response","id":"hello","ok":true,"data":{"role":actor.role,"instanceId":actor.id,"generation":actor.generation}})).is_err(){return}
        while let Ok(Some(frame)) = read_frame(&mut reader) {
            let current = self.state.lock().ok().is_some_and(|s| {
                s.clients.get(&actor.id).is_some_and(|c| {
                    c.generation == actor.generation && c.connection_id == actor.connection_id
                })
            });
            if !current {
                break;
            }
            match frame["type"].as_str() {
                Some("request") => {
                    let hub = self.clone();
                    let actor = actor.clone();
                    let output = writer.clone();
                    std::thread::spawn(move || {
                        let id = frame["id"].clone();
                        let result = hub.execute(&actor, &frame);
                        let response = match result {
                            Ok(data) => json!({"type":"response","id":id,"ok":true,"data":data}),
                            Err(error) => {
                                json!({"type":"response","id":id,"ok":false,"error":error})
                            }
                        };
                        if let Err(error) = send(&output, &response) {
                            let _ = send(
                                &output,
                                &json!({"type":"response","id":id,"ok":false,"error":error}),
                            );
                        }
                    });
                }
                Some("event") => {
                    if let Err(e) = self.event(&actor, &frame) {
                        eprintln!("manager event: {e}");
                    }
                }
                Some("response") => self.control_response(&actor, &frame),
                Some("ack") if actor.role == InstanceKind::Manager => {
                    if let Ok(mut state) = self.state.lock() {
                        if let Some(queue) = state.queues.get_mut(&actor.id) {
                            queue.retain(|n| n["eventId"] != frame["eventId"]);
                        }
                    }
                }
                _ => {
                    let _ = send(
                        &writer,
                        &json!({"type":"response","id":frame["id"],"ok":false,"error":"invalid_action"}),
                    );
                }
            }
        }
        // A bridge must never keep writing into a connection nobody reads; closing makes it fail fast and reconnect.
        if let Ok(s) = writer.lock() {
            let _ = s.shutdown(Shutdown::Both);
        }
        let current = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            let current = state.clients.get(&actor.id).is_some_and(|c| {
                c.connection_id == actor.connection_id && c.generation == actor.generation
            });
            if current {
                if let Some(client) = state.clients.get_mut(&actor.id) {
                    client.stream = None;
                    client.ready = false;
                }
                let pending: Vec<String> = state
                    .inflight
                    .iter()
                    .filter(|(_, p)| p.target == actor.id && p.generation == actor.generation)
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in pending {
                    if let Some(p) = state.inflight.remove(&key) {
                        let _ = p
                            .sender
                            .send(Err("bridge disconnected; outcome may be unknown".into()));
                    }
                }
            }
            current
        };
        if current {
            if let Ok(registry) = self.registry() {
                registry.set_bridge_ready(&actor.id, actor.generation, false);
            }
        }
    }
    fn connection(&self, id: &str, ready: bool) -> Result<Connection> {
        let state = self.state.lock().map_err(|e| e.to_string())?;
        let client = state.clients.get(id).ok_or("bridge_not_ready")?;
        if ready && !client.ready {
            return Err("bridge_not_ready".into());
        }
        client
            .stream
            .clone()
            .ok_or_else(|| "bridge_not_ready".into())
    }
    fn control_response(&self, actor: &Actor, frame: &Value) {
        let Some(id) = frame["id"].as_str() else {
            return;
        };
        let pending = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if !state
                .inflight
                .get(id)
                .is_some_and(|p| p.target == actor.id && p.generation == actor.generation)
            {
                return;
            }
            state.inflight.remove(id)
        };
        if let Some(pending) = pending {
            let result = if frame["ok"] == true {
                Ok(frame["data"].clone())
            } else {
                Err(frame["error"]
                    .as_str()
                    .unwrap_or("native control failed")
                    .into())
            };
            let _ = pending.sender.send(result);
        }
    }
    fn control(
        &self,
        id: &str,
        lease: &ControlLease,
        action: &str,
        mut payload: Value,
        origin: Origin<'_>,
    ) -> Result<Value> {
        let stream = self.connection(id, true)?;
        let request_id = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = mpsc::sync_channel(1);
        payload["generation"] = json!(lease.generation);
        payload["sessionId"] = json!(lease.session_id);
        payload["ownershipEpoch"] = json!(lease.ownership_epoch);
        payload["source"] = json!(match origin {
            Origin::Human => "human",
            Origin::Manager(_) => "manager",
        });
        self.state
            .lock()
            .map_err(|e| e.to_string())?
            .inflight
            .insert(
                request_id.clone(),
                Inflight {
                    target: id.into(),
                    generation: lease.generation,
                    sender,
                },
            );
        if let Err(e) = send(
            &stream,
            &json!({"type":"control","id":request_id,"action":action,"payload":payload}),
        ) {
            if let Ok(mut state) = self.state.lock() {
                state.inflight.remove(&request_id);
            }
            return Err(e);
        }
        let result = receiver
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| "native control timeout; inspect state before retrying".to_string());
        if let Ok(mut state) = self.state.lock() {
            state.inflight.remove(&request_id);
        }
        result?
    }
    fn owner_notification(&self, id: &str, generation: u64, kind: &str, payload: Value) {
        let Ok(registry) = self.registry() else {
            return;
        };
        let Ok(lease) = registry.lease(id, Origin::Human, false) else {
            return;
        };
        if lease.generation != generation {
            return;
        }
        let Some(owner) = lease.manager_id else {
            return;
        };
        let subscribed = self
            .state
            .lock()
            .ok()
            .is_some_and(|s| s.subscriptions.get(id).copied().unwrap_or(true));
        if subscribed {
            self.notify(&owner, id, generation, kind, payload);
        }
    }
    fn notify(&self, manager: &str, id: &str, generation: u64, kind: &str, payload: Value) {
        let value = json!({"type":"notify","eventId":uuid::Uuid::new_v4().to_string(),"instanceId":id,"generation":generation,"kind":kind,"payload":payload});
        let stream = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state
                .queues
                .entry(manager.into())
                .or_default()
                .push_back(value.clone());
            state
                .clients
                .get(manager)
                .filter(|c| c.ready)
                .and_then(|c| c.stream.clone())
        };
        if let Some(stream) = stream {
            let _ = send(&stream, &value);
        }
    }
    fn replay(&self, manager: &str) {
        let (stream, queue) = {
            let Ok(state) = self.state.lock() else { return };
            let Some(stream) = state
                .clients
                .get(manager)
                .filter(|c| c.ready)
                .and_then(|c| c.stream.clone())
            else {
                return;
            };
            (
                stream,
                state.queues.get(manager).cloned().unwrap_or_default(),
            )
        };
        for value in queue {
            if send(&stream, &value).is_err() {
                break;
            }
        }
    }
    pub fn ownership_changed(&self, id: &str, lease: &ControlLease) {
        let (previous, decisions) = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            let previous = state
                .owners
                .insert(id.into(), lease.manager_id.clone())
                .flatten();
            state.subscriptions.insert(id.into(), true);
            for queue in state.queues.values_mut() {
                queue.retain(|n| n["instanceId"] != id);
            }
            let old: Vec<String> = state
                .decisions
                .iter()
                .filter(|(_, d)| d.worker == id)
                .map(|(k, _)| k.clone())
                .collect();
            let mut fresh = vec![];
            for key in old {
                if let Some(mut d) = state.decisions.remove(&key) {
                    d.public_id = uuid::Uuid::new_v4().to_string();
                    d.epoch = lease.ownership_epoch;
                    fresh.push(d.clone());
                    state.decisions.insert(d.public_id.clone(), d);
                }
            }
            (previous, fresh)
        };
        if let Ok(stream) = self.connection(id, false) {
            let _ = send(
                &stream,
                &json!({"type":"control","id":uuid::Uuid::new_v4().to_string(),"action":"ownership","payload":lease}),
            );
        }
        if let Some(old) = previous {
            self.notify(&old, id, lease.generation, "ownership_lost", json!({}));
        }
        if let Some(owner) = &lease.manager_id {
            self.notify(owner,id,lease.generation,"ownership_gained",json!({"pendingDecisions":decisions.iter().map(Decision::view).collect::<Vec<_>>()}));
            for decision in decisions {
                self.notify(
                    owner,
                    id,
                    lease.generation,
                    "needs_decision",
                    decision.view(),
                );
            }
        }
    }
    fn event(&self, actor: &Actor, frame: &Value) -> Result<()> {
        let kind = text(frame, "kind")?;
        let payload = &frame["payload"];
        let registry = self.registry()?;
        if registry.lease(&actor.id, Origin::Human, false)?.generation != actor.generation {
            return Err("stale_generation".into());
        }
        match kind {
            "ready" | "session_changed" => {
                registry.apply_identity(&actor.id, actor.generation, payload)?;
                if let Some(terminals) = self.terminals.get() {
                    if let Err(error) = terminals.apply_snapshot(&actor.id, actor.generation, payload) {
                        eprintln!("terminal snapshot: {error}");
                    }
                }
                if let Ok(mut state) = self.state.lock() {
                    if let Some(c) = state.clients.get_mut(&actor.id) {
                        c.ready = payload["supported"] != false;
                    }
                }
                self.clear_decisions(&actor.id);
                let lease = registry.lease(&actor.id, Origin::Human, false)?;
                if let Ok(stream) = self.connection(&actor.id, false) {
                    send(
                        &stream,
                        &json!({"type":"control","id":uuid::Uuid::new_v4().to_string(),"action":"ownership","payload":lease}),
                    )?;
                }
                if actor.role == InstanceKind::Manager {
                    self.replay(&actor.id);
                }
                for pending in payload["pendingDecisions"].as_array().into_iter().flatten() {
                    self.register_decision(actor, pending)?;
                }
            }
            "needs_decision" => self.register_decision(actor, payload)?,
            "decision_resolved" => {
                let base = text(payload, "requestId")?;
                let resolved = {
                    let mut state = self.state.lock().map_err(|e| e.to_string())?;
                    let key = state
                        .decisions
                        .iter()
                        .find(|(_, d)| {
                            d.worker == actor.id
                                && d.base_id == base
                                && d.generation == actor.generation
                        })
                        .map(|(k, _)| k.clone());
                    key.and_then(|k| state.decisions.remove(&k))
                };
                if let Some(decision) = resolved {
                    registry.resolve_human_decision(&actor.id, &decision.public_id);
                    self.retire_notification(&decision.public_id);
                    self.owner_notification(
                        &actor.id,
                        actor.generation,
                        kind,
                        json!({"requestId":decision.public_id,"source":payload["source"]}),
                    );
                }
                self.update_decision_count(&actor.id);
            }
            "agent_started" => {}
            "terminal_command" | "terminal_output" | "terminal_hold" => {
                let terminals = self.terminals.get().ok_or("terminal service unavailable")?;
                match kind {
                    "terminal_command" => terminals.apply_command(&actor.id, actor.generation, payload)?,
                    "terminal_output" => terminals.apply_output(&actor.id, actor.generation, payload)?,
                    _ => terminals.apply_hold(&actor.id, actor.generation, payload)?,
                }
            }
            "completed" | "turn_error" | "turn_aborted" => {
                if actor.role == InstanceKind::Instance {
                    self.owner_notification(&actor.id, actor.generation, kind, payload.clone());
                }
            }
            _ => return Err("invalid_event".into()),
        }
        Ok(())
    }
    fn register_decision(&self, actor: &Actor, payload: &Value) -> Result<()> {
        let registry = self.registry()?;
        let lease = registry.lease(&actor.id, Origin::Human, false)?;
        let base = text(payload, "requestId")?;
        let decision = {
            let mut state = self.state.lock().map_err(|e| e.to_string())?;
            if state.decisions.values().any(|d| {
                d.worker == actor.id && d.generation == actor.generation && d.base_id == base
            }) {
                return Ok(());
            }
            let decision = Decision {
                public_id: uuid::Uuid::new_v4().to_string(),
                worker: actor.id.clone(),
                base_id: base.into(),
                generation: actor.generation,
                session_id: lease.session_id,
                epoch: lease.ownership_epoch,
                payload: payload.clone(),
            };
            state
                .decisions
                .insert(decision.public_id.clone(), decision.clone());
            decision
        };
        self.update_decision_count(&actor.id);
        if actor.role == InstanceKind::Instance {
            self.owner_notification(
                &actor.id,
                actor.generation,
                "needs_decision",
                decision.view(),
            );
        }
        Ok(())
    }
    fn update_decision_count(&self, id: &str) {
        let count = self
            .state
            .lock()
            .ok()
            .map(|s| s.decisions.values().filter(|d| d.worker == id).count())
            .unwrap_or(0);
        if let Ok(registry) = self.registry() {
            registry.set_decision_count(id, count);
        }
    }
    fn retire_notification(&self, id: &str) {
        if let Ok(mut state) = self.state.lock() {
            for q in state.queues.values_mut() {
                q.retain(|n| n["payload"]["requestId"] != id);
            }
        }
    }
    fn clear_decisions(&self, id: &str) {
        let ids = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            let ids: Vec<String> = state
                .decisions
                .values()
                .filter(|d| d.worker == id)
                .map(|d| d.public_id.clone())
                .collect();
            state.decisions.retain(|_, d| d.worker != id);
            ids
        };
        for id in ids {
            self.retire_notification(&id);
        }
        self.update_decision_count(id);
    }
    pub fn observe_processes(&self, rows: &[Instance]) {
        for row in rows {
            if row.generation == 0 || row.exit_code.is_none() {
                continue;
            }
            let first = self
                .state
                .lock()
                .ok()
                .is_some_and(|mut s| s.exited.insert((row.id.clone(), row.generation)));
            if first {
                self.owner_notification(
                    &row.id,
                    row.generation,
                    "process_exited",
                    json!({"exitCode":row.exit_code}),
                );
                self.invalidate(&row.id, row.generation);
            }
        }
    }
    pub(crate) fn wait_ready(&self, id: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.connection(id, true).is_ok() {
                return Ok(());
            }
            let row = self.registry()?.inspect(id, Origin::Human)?;
            if row.exit_code.is_some() {
                return Err(row
                    .last_error
                    .unwrap_or_else(|| "process exited before bridge initialization".into()));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err("bridge_not_ready".into())
    }
    pub fn prompt_human(&self, id: &str, message: &str) -> Result<Value> {
        let registry = self.registry()?;
        let lease = registry.lease(id, Origin::Human, true)?;
        if registry.inspect(id, Origin::Human)?.kind == InstanceKind::Manager {
            return Err("Setup prompts require an ordinary human-controlled instance".into());
        }
        self.control(
            id,
            &lease,
            "prompt",
            json!({"message":message}),
            Origin::Human,
        )
    }
    /// App-originated control of a plan agent's target: `Origin::Human`, so any session (including The Manager)
    /// can be interrupted and prompted. The target is derived from the registry, never from the payload.
    fn plan_control(&self, registry: &Registry, target: &str, control: &str, payload: Value) -> Result<Value> {
        let lease = registry.lease(target, Origin::Human, true)?;
        self.control(target, &lease, control, payload, Origin::Human)
            .map_err(|e| if e == "bridge_not_ready" { plan::NOT_RUNNING.into() } else { e })
    }
    pub fn restore_plan_backup(&self, target: &str, backup_id: &str) -> Result<()> {
        let registry = self.registry()?;
        registry.inspect(target, Origin::Human)?;
        let backup = plan::read_backup(&registry.plan_folder(target), backup_id)?;
        match self.plan_control(&registry, target, "abort", json!({})) {
            Ok(_) => (),
            Err(e) if e == plan::NOT_RUNNING => (),
            Err(e) => return Err(e),
        }
        registry.revise_plan(target, &backup.text)?;
        Ok(())
    }
    fn plan_action(&self, registry: &Arc<Registry>, target: &str, action: &str, p: &Value) -> Result<Value> {
        let allowed: &[&str] = match action {
            "plan_status" | "plan_read_plan" | "plan_screen" | "plan_interrupt" | "plan_handoff_info" => &[],
            "plan_transcript" => &["tail"],
            "plan_write_plan" => &["text"],
            "plan_handoff" => &["summary", "revision", "startIfStopped"],
            _ => return Err("invalid_action".into()),
        };
        let object = p.as_object().ok_or_else(|| "invalid_plan_payload: payload must be a JSON object".to_string())?;
        if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(format!("invalid_plan_payload: {action} does not accept '{key}' (allowed: {})", if allowed.is_empty() { "none".into() } else { allowed.join(", ") }));
        }
        match action {
            "plan_status" => {
                let row = registry.inspect(target, Origin::Human)?;
                Ok(json!({
                    "name": row.name, "kind": row.kind, "projectPath": row.project_path, "status": row.status,
                    "activity": row.activity, "task": row.task, "todoDone": row.todo_done, "todoTotal": row.todo_total,
                    "planPath": row.plan_path, "planKind": row.plan_kind,
                    "subagents": row.subagents.iter().map(|s| json!({"name": s.name, "agent": s.agent, "status": s.status, "task": s.task})).collect::<Vec<_>>(),
                }))
            }
            "plan_read_plan" => Ok(json!({"kind": registry.inspect(target, Origin::Human)?.plan_kind, "path": registry.plan_file(target)?, "text": registry.plan(target, Origin::Human)?})),
            "plan_transcript" => {
                let tail = object.get("tail").map(|v| v.as_u64().ok_or("invalid_plan_payload")).transpose()?.unwrap_or(20_000).clamp(1_000, 60_000) as usize;
                let (path, input) = registry.main_transcript(target, Origin::Human)?;
                Ok(json!({"text": plan::tail(&session::render_transcript_file(&path, &input)?, tail)}))
            }
            "plan_screen" => {
                let screen = registry.screen(target, Origin::Human).map_err(|e| if e == "instance not started" { plan::NOT_RUNNING.to_string() } else { e })?;
                Ok(json!({"text": plan::tail(screen["text"].as_str().unwrap_or(""), 8_000)}))
            }
            "plan_interrupt" => {
                self.plan_control(registry, target, "abort", json!({}))?;
                Ok(json!({"interrupted": true}))
            }
            "plan_write_plan" => {
                let text = object.get("text").and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty() && t.chars().count() <= 262_144)
                    .ok_or("plan text must be 1..262144 characters")?;
                // The session must stop working before its plan changes; a stopped session has nothing to interrupt.
                let interrupted = match self.plan_control(registry, target, "abort", json!({})) {
                    Ok(_) => true,
                    Err(e) if e == plan::NOT_RUNNING => false,
                    Err(e) => return Err(e),
                };
                let backup = registry.revise_plan(target, text)?;
                Ok(json!({"path": registry.plan_file(target)?, "bytes": text.len(), "interrupted": interrupted, "backup": backup}))
            }
            "plan_handoff_info" => {
                let revision = registry.plan_revision(target)?
                    .ok_or("There is no plan to hand off; call write_plan first")?;
                let row = registry.inspect(target, Origin::Human)?;
                Ok(json!({"name": row.name, "revision": revision.id,
                    "needsStart": self.connection(target, true).is_err() && !matches!(row.status.as_str(), "running" | "starting")}))
            }
            "plan_handoff" => {
                let summary = object.get("summary").and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty() && s.chars().count() <= 4_000)
                    .ok_or("summary must be 1..4000 characters")?;
                let revision = registry.plan_revision(target)?
                    .ok_or("There is no plan to hand off; call write_plan first")?;
                if p["revision"].as_str() != Some(revision.id.as_str()) {
                    return Err("The plan changed; confirm the new revision before handoff".into());
                }
                let start = p.get("startIfStopped").map(|v| v.as_bool().ok_or("startIfStopped must be boolean")).transpose()?.unwrap_or(false);
                if self.connection(target, true).is_err() {
                    let row = registry.inspect(target, Origin::Human)?;
                    if !matches!(row.status.as_str(), "running" | "starting") {
                        if !start { return Err(plan::NOT_RUNNING.into()); }
                        registry.start(target, Origin::Human)?;
                    }
                    self.wait_ready(target)?;
                }
                // Starting/resuming may load a different native identity. Never send an unconfirmed revision.
                let current = registry.plan_revision(target)?.ok_or("The session changed; review the plan before handoff")?;
                if current.id != revision.id { return Err("The plan changed; confirm the new revision before handoff".into()); }
                let message = plan::handoff_message(&revision.path, &revision.text, summary);
                self.plan_control(registry, target, "prompt", json!({"message": message}))?;
                registry.mark_plan_handoff(target, &revision.id)?;
                Ok(json!({"accepted": true}))
            }
            _ => Err("invalid_action".into()),
        }
    }
    fn execute(&self, actor: &Actor, frame: &Value) -> Result<Value> {
        let current = self.state.lock().map_err(|e| e.to_string())?.clients.get(&actor.id).is_some_and(|client| client.generation == actor.generation && client.connection_id == actor.connection_id);
        if !current { return Err("stale_generation".into()); }
        let registry = self.registry()?;
        if registry.lease(&actor.id, Origin::Human, false)?.generation != actor.generation {
            return Err("stale_generation".into());
        }
        let action = text(frame, "action")?;
        let p = &frame["payload"];
        if matches!(action, "browser_prepare" | "browser_close_tab" | "browser_close_all" | "browser_tabs") {
            let fields: &[&str] = match action {
                "browser_close_all" | "browser_tabs" => &[],
                "browser_prepare" => &["name", "target"],
                _ => &["name"],
            };
            let object = p.as_object().ok_or("invalid_browser_payload")?;
            if object.keys().any(|key| !fields.contains(&key.as_str())) { return Err("invalid_browser_payload".into()); }
            let browser = self.browser.get().ok_or("browser supervisor unavailable")?;
            return match action {
                "browser_prepare" => {
                    let target = p.get("target").map(|v| v.as_str().ok_or("invalid_browser_payload")).transpose()?;
                    browser.prepare_tab(&actor.id, actor.generation, text(p, "name")?, target)
                }
                "browser_tabs" => browser.list_tabs(&actor.id, actor.generation),
                "browser_close_tab" => browser.close_tab(&actor.id, actor.generation, Some(text(p, "name")?)).map(|_| Value::Null),
                _ => browser.close_tab(&actor.id, actor.generation, None).map(|_| Value::Null),
            };
        }
        if action.starts_with("plan_") {
            if let Some(target) = registry.plan_target(&actor.id) {
                return self.plan_action(&registry, &target, action, p);
            }
        }
        if actor.role != InstanceKind::Manager { return Err("unauthorized".into()); }
        let origin = Origin::Manager(&actor.id);
        if action == "list" {
            let rows = registry
                .snapshot()
                .into_iter()
                .filter(|r| {
                    r.kind == InstanceKind::Instance
                        && r.manager_id.as_deref() == Some(actor.id.as_str())
                })
                .collect::<Vec<_>>();
            let state = self.state.lock().map_err(|e| e.to_string())?;
            return Ok(
                json!({"instances":rows,"pendingDecisions":state.decisions.values().filter(|d|rows.iter().any(|r|r.id==d.worker)).map(Decision::view).collect::<Vec<_>>()}),
            );
        }
        if action == "create" {
            let start = p["start"].as_bool().unwrap_or(true);
            let prompt = p["prompt"].as_str();
            if !start && prompt.is_some() {
                return Err("prompt_requires_start".into());
            }
            let mut input: CreateInput =
                serde_json::from_value(p["config"].clone()).map_err(|e| e.to_string())?;
            input.agent_file = None;
            let row = registry.create_owned(&actor.id, input)?;
            if start {
                if let Err(error) = registry.start(&row.id, origin) {
                    return Ok(
                        json!({"instance":registry.inspect(&row.id,origin)?,"error":error,"promptSent":false}),
                    );
                }
                if prompt.is_some() {
                    if let Err(error) = self.wait_ready(&row.id) {
                        return Ok(
                            json!({"instance":registry.inspect(&row.id,origin)?,"error":error,"promptSent":false}),
                        );
                    }
                    let lease = registry.lease(&row.id, origin, true)?;
                    self.control(
                        &row.id,
                        &lease,
                        "prompt",
                        json!({"message":prompt,"delivery":"follow_up"}),
                        origin,
                    )?;
                }
            }
            return Ok(
                json!({"instance":registry.inspect(&row.id,origin)?,"promptSent":prompt.is_some()}),
            );
        }
        let id = text(p, "instanceId")?;
        match action {
            "start" => {
                registry.start(id, origin)?;
                Ok(json!({"instance":registry.inspect(id,origin)?}))
            }
            "stop" => {
                registry.stop(id, origin)?;
                Ok(json!({"instance":registry.inspect(id,origin)?}))
            }
            "restart" => {
                registry.stop(id, origin)?;
                registry.refresh()?;
                registry.start(id, origin)?;
                Ok(json!({"instance":registry.inspect(id,origin)?}))
            }
            "remove" => {
                registry.remove(id, origin)?;
                Ok(json!({"removed":true}))
            }
            "screen" => registry.screen(id, origin),
            "plan" => Ok(json!({"text":registry.plan(id,origin)?})),
            "subagents" => Ok(json!({"subagents":registry.inspect(id,origin)?.subagents})),
            "transcript" => {
                let (path, input) = if let Some(child) = p["subagentId"].as_str() {
                    registry.transcript_path(id, child, origin)?
                } else {
                    registry.main_transcript(id, origin)?
                };
                Ok(json!({"text":session::render_transcript_file(&path,&input)?}))
            }
            "keys" => {
                registry.keys(
                    id,
                    p["generation"].as_u64().ok_or("generation is required")?,
                    p["sessionId"].as_str(),
                    text(p, "data")?,
                    origin,
                )?;
                Ok(json!({"accepted":true}))
            }
            "send" | "interrupt" => {
                let lease = registry.lease(id, origin, true)?;
                self.control(
                    id,
                    &lease,
                    if action == "send" { "prompt" } else { "abort" },
                    p.clone(),
                    origin,
                )
            }
            "subscribe" => {
                registry.lease(id, origin, false)?;
                let enabled = p["enabled"].as_bool().ok_or("enabled is required")?;
                let decisions = {
                    let mut state = self.state.lock().map_err(|e| e.to_string())?;
                    let was_enabled = state
                        .subscriptions
                        .insert(id.into(), enabled)
                        .unwrap_or(true);
                    if !enabled {
                        for queue in state.queues.values_mut() {
                            queue.retain(|n| n["instanceId"] != id);
                        }
                    }
                    state
                        .decisions
                        .values()
                        .filter(|d| enabled && !was_enabled && d.worker == id)
                        .cloned()
                        .collect::<Vec<_>>()
                };
                if enabled {
                    self.replay(&actor.id);
                }
                for decision in decisions {
                    self.owner_notification(
                        id,
                        decision.generation,
                        "needs_decision",
                        decision.view(),
                    );
                }
                Ok(json!({"subscribed":enabled}))
            }
            "request_human" => {
                registry.lease(id, origin, false)?;
                let decision_id = p["requestId"].as_str().map(String::from);
                if let Some(request) = &decision_id {
                    let state = self.state.lock().map_err(|e| e.to_string())?;
                    if !state.decisions.get(request).is_some_and(|d| d.worker == id) {
                        return Err("decision_already_resolved".into());
                    }
                }
                registry.request_human(id, &actor.id, text(p, "reason")?.into(), decision_id)?;
                self.owner_notification(
                    id,
                    registry.lease(id, origin, false)?.generation,
                    "human_requested",
                    p.clone(),
                );
                Ok(json!({"humanRequested":true}))
            }
            "respond" => {
                let lease = registry.lease(id, origin, true)?;
                let request = text(p, "requestId")?;
                if p["generation"].as_u64() != Some(lease.generation) {
                    return Err("stale_generation".into());
                }
                let decision = self
                    .state
                    .lock()
                    .map_err(|e| e.to_string())?
                    .decisions
                    .get(request)
                    .cloned()
                    .ok_or("decision_already_resolved")?;
                if decision.worker != id
                    || decision.generation != lease.generation
                    || decision.epoch != lease.ownership_epoch
                    || decision.session_id != lease.session_id
                {
                    return Err("stale_generation".into());
                }
                if matches!(
                    decision.payload["method"].as_str(),
                    Some("terminal" | "custom")
                ) {
                    return Err("native_terminal_decision".into());
                }
                self.control(
                    id,
                    &lease,
                    "reply_ui",
                    json!({"requestId":decision.base_id,"response":p["response"]}),
                    origin,
                )
            }
            _ => Err("invalid_action".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_frames_reject_bad_json_and_wrong_capabilities() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        client.write_all(b"not-json\n").unwrap();
        assert!(read_frame(&mut BufReader::new(server)).is_err());
        let folder =
            std::env::temp_dir().join(format!("omp-manager-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let registry =
            Arc::new(Registry::load(folder.join("instances.json"), folder.clone()).unwrap());
        let manager = registry
            .create_manager(CreateInput {
                project_path: folder.to_string_lossy().into(),
                ..Default::default()
            })
            .unwrap();
        let hub = ManagerHub::bind(folder.join("bridge.mjs")).unwrap();
        hub.bind_registry(Arc::downgrade(&registry)).unwrap();
        hub.install_terminal_service(TerminalService::new(&folder)).unwrap();
        let definition = Definition {
            id: manager.id.clone(),
            input: CreateInput::default(),
            session_path: None,
            kind: InstanceKind::Manager,
            manager_id: None,
            plan_target: None,
            helper: None,
        };
        let launch = hub.launch(&definition, 0).unwrap();
        assert!(hub.authenticate(&json!({"type":"hello","id":"hello","protocol":1,"instanceId":manager.id,"generation":0,"token":"wrong"})).is_err());
        let actor=hub.authenticate(&json!({"type":"hello","id":"hello","protocol":1,"instanceId":manager.id,"generation":0,"token":launch.token})).unwrap();
        let mut socket = TcpStream::connect(&hub.endpoint).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        std::thread::sleep(Duration::from_millis(250));
        let hello = json!({"type":"hello","id":"hello","protocol":1,"instanceId":manager.id,"generation":0,"token":launch.token});
        socket.write_all(format!("{hello}\n").as_bytes()).unwrap();
        let welcome = read_frame(&mut BufReader::new(socket)).unwrap().unwrap();
        assert_eq!(welcome["ok"], true);
        let actor = Actor {
            connection_id: hub.state.lock().unwrap_or_else(|e| e.into_inner()).clients[&manager.id].connection_id.clone(),
            ..actor
        };
        let human = registry
            .create(CreateInput {
                project_path: folder.to_string_lossy().into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            hub.execute(
                &actor,
                &json!({"action":"remove","payload":{"instanceId":human.id}})
            )
            .err()
            .as_deref(),
            Some("not_controlled")
        );
        assert_eq!(
            hub.execute(&actor, &json!({"action":"send","payload":{"instanceId":human.id,"source":"human","message":"unauthorized"}}))
                .err().as_deref(),
            Some("not_controlled")
        );
        assert_eq!(registry.snapshot().len(), 2);
        hub.invalidate(&manager.id, 0);
        assert!(hub.authenticate(&json!({"type":"hello","id":"hello","protocol":1,"instanceId":manager.id,"generation":0,"token":launch.token})).is_err());
        hub.shutdown();
        std::fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn decisions_retire_on_resolution_revocation_and_unsubscribe() {
        let folder =
            std::env::temp_dir().join(format!("omp-manager-decisions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let registry =
            Arc::new(Registry::load(folder.join("instances.json"), folder.clone()).unwrap());
        let config = CreateInput {
            project_path: folder.to_string_lossy().into(),
            ..Default::default()
        };
        let manager = registry.create_manager(config.clone()).unwrap();
        let worker = registry.create_owned(&manager.id, config).unwrap();
        let hub = ManagerHub::bind(folder.join("bridge.mjs")).unwrap();
        hub.bind_registry(Arc::downgrade(&registry)).unwrap();
        hub.install_terminal_service(TerminalService::new(&folder)).unwrap();
        registry.install_manager_hub(hub.clone()).unwrap();
        let manager_actor = Actor {
            id: manager.id.clone(),
            generation: 0,
            role: InstanceKind::Manager,
            connection_id: String::new(),
        };
        let worker_actor = Actor {
            id: worker.id.clone(),
            generation: 0,
            role: InstanceKind::Instance,
            connection_id: String::new(),
        };
        hub.launch(&Definition { id:manager.id.clone(), input:CreateInput::default(), session_path:None, kind:InstanceKind::Manager, manager_id:None, plan_target:None, helper:None }, 0).unwrap();
        hub.launch(&Definition { id:worker.id.clone(), input:CreateInput::default(), session_path:None, kind:InstanceKind::Instance, manager_id:Some(manager.id.clone()), plan_target:None, helper:None }, 0).unwrap();
        let event = json!({"kind":"needs_decision","payload":{"requestId":"native-1","method":"select","title":"Review operation","options":["Approve","Deny"]}});
        hub.event(&worker_actor, &event).unwrap();
        let first_id = {
            let state = hub.state.lock().unwrap_or_else(|e| e.into_inner());
            state.decisions.values().next().unwrap().public_id.clone()
        };
        let subscribe = |enabled| json!({"action":"subscribe","payload":{"instanceId":worker.id,"enabled":enabled}});
        hub.event(
            &worker_actor,
            &json!({"kind":"completed","payload":{"text":"DONE"}}),
        )
        .unwrap();
        hub.execute(&manager_actor, &subscribe(true)).unwrap();
        {
            let state = hub.state.lock().unwrap_or_else(|e| e.into_inner());
            let queue = state.queues.get(&manager.id).unwrap();
            assert_eq!(
                queue
                    .iter()
                    .map(|n| n["kind"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                vec!["needs_decision", "completed"]
            );
        }
        hub.execute(&manager_actor, &subscribe(false)).unwrap();
        {
            let state = hub.state.lock().unwrap_or_else(|e| e.into_inner());
            assert!(state.queues.get(&manager.id).unwrap().is_empty());
        }
        hub.execute(&manager_actor, &subscribe(true)).unwrap();
        {
            let state = hub.state.lock().unwrap_or_else(|e| e.into_inner());
            let queue = state.queues.get(&manager.id).unwrap();
            assert_eq!(queue.len(), 1);
            assert_eq!(queue[0]["payload"]["requestId"], first_id);
        }
        let reply = json!({"action":"respond","payload":{"instanceId":worker.id,"requestId":first_id,"generation":0,"response":"Approve"}});
        registry.set_manager_owner(&worker.id, None).unwrap();
        assert_eq!(
            hub.execute(&manager_actor, &reply).err().as_deref(),
            Some("not_controlled")
        );
        registry
            .set_manager_owner(&worker.id, Some(&manager.id))
            .unwrap();
        assert_eq!(
            hub.execute(&manager_actor, &reply).err().as_deref(),
            Some("decision_already_resolved")
        );
        let current_id = {
            let state = hub.state.lock().unwrap_or_else(|e| e.into_inner());
            state.decisions.values().next().unwrap().public_id.clone()
        };
        assert_ne!(first_id, current_id);
        hub.event(&worker_actor, &json!({"kind":"decision_resolved","payload":{"requestId":"native-1","source":"human"}})).unwrap();
        assert_eq!(
            registry
                .inspect(&worker.id, Origin::Human)
                .unwrap()
                .pending_decision_count,
            0
        );
        let resolved = json!({"action":"respond","payload":{"instanceId":worker.id,"requestId":current_id,"generation":0,"response":"Approve"}});
        assert_eq!(
            hub.execute(&manager_actor, &resolved).err().as_deref(),
            Some("decision_already_resolved")
        );
        assert_eq!(
            hub.execute(&worker_actor, &resolved).err().as_deref(),
            Some("unauthorized")
        );
        let stale = Actor {
            generation: 1,
            ..worker_actor
        };
        assert_eq!(
            hub.event(&stale, &event).err().as_deref(),
            Some("stale_generation")
        );
        hub.shutdown();
        std::fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn manager_created_workers_cannot_attach_agent_files() {
        let folder =
            std::env::temp_dir().join(format!("omp-manager-persona-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let persona = folder.join("AGENTS.md");
        std::fs::write(&persona, "be terse").unwrap();
        let registry =
            Arc::new(Registry::load(folder.join("instances.json"), folder.clone()).unwrap());
        let manager = registry
            .create_manager(CreateInput {
                project_path: folder.to_string_lossy().into(),
                ..Default::default()
            })
            .unwrap();
        let hub = ManagerHub::bind(folder.join("bridge.mjs")).unwrap();
        hub.bind_registry(Arc::downgrade(&registry)).unwrap();
        hub.install_terminal_service(TerminalService::new(&folder)).unwrap();
        registry.install_manager_hub(hub.clone()).unwrap();
        let actor = Actor {
            id: manager.id.clone(),
            generation: 0,
            role: InstanceKind::Manager,
            connection_id: String::new(),
        };
        hub.launch(&Definition { id:manager.id.clone(), input:CreateInput::default(), session_path:None, kind:InstanceKind::Manager, manager_id:None, plan_target:None, helper:None }, 0).unwrap();
        let create = json!({"action":"create","payload":{"start":false,"config":{"name":"W","projectPath":folder,"agentFile":persona}}});
        hub.execute(&actor, &create).unwrap();
        let rows = registry.snapshot();
        let worker = rows.iter().find(|r| r.kind == InstanceKind::Instance).unwrap();
        assert_eq!(worker.agent_file, None);
        hub.shutdown();
        std::fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn worker_browser_actions_are_authenticated_and_cannot_choose_a_peer_owner() {
        let folder = std::env::temp_dir().join(format!("omp-browser-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let registry = Arc::new(Registry::load(folder.join("instances.json"), folder.clone()).unwrap());
        let worker = registry.create(CreateInput { project_path:folder.to_string_lossy().into(), ..Default::default() }).unwrap();
        let hub = ManagerHub::bind(folder.join("bridge.mjs")).unwrap();
        hub.bind_registry(Arc::downgrade(&registry)).unwrap();
        hub.install_terminal_service(TerminalService::new(&folder)).unwrap();
        let browser = crate::browser::BrowserSupervisor::new(folder.clone(), folder.clone());
        hub.install_browser_supervisor(browser.clone()).unwrap();
        let launch = hub.launch(&Definition {id:worker.id.clone(),input:CreateInput::default(),session_path:None,kind:InstanceKind::Instance,manager_id:None,plan_target:None,helper:None},0).unwrap();
        browser.register(&worker.id,0,&launch.token).unwrap();
        let actor = Actor {id:worker.id.clone(),generation:0,role:InstanceKind::Instance,connection_id:String::new()};
        assert_eq!(hub.execute(&actor,&json!({"action":"browser_prepare","payload":{"name":"main","instanceId":"peer"}})).unwrap_err(),"invalid_browser_payload");
        assert_eq!(hub.execute(&actor,&json!({"action":"browser_prepare","payload":{"name":"main","target":5}})).unwrap_err(),"invalid_browser_payload");
        assert_eq!(hub.execute(&actor,&json!({"action":"list","payload":{}})).unwrap_err(),"unauthorized");
        assert_eq!(hub.execute(&actor,&json!({"action":"browser_tabs","payload":{"instanceId":"peer"}})).unwrap_err(),"invalid_browser_payload");
        assert_eq!(hub.execute(&actor,&json!({"action":"browser_tabs","payload":{}})).unwrap(),json!([]), "no helper means no tabs, and none is started");
        let replaced = Actor {connection_id:"replaced".into(),..actor.clone()};
        assert_eq!(hub.execute(&replaced,&json!({"action":"browser_close_all","payload":{}})).unwrap_err(),"stale_generation");
        hub.invalidate(&worker.id,0);
        assert!(browser.snapshot().is_empty());
        assert_eq!(hub.execute(&actor,&json!({"action":"browser_prepare","payload":{"name":"main"}})).unwrap_err(),"stale_generation");
        browser.shutdown();hub.shutdown();std::fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn an_authenticated_bridge_connection_stays_readable_after_the_hello_deadline() {
        let folder = std::env::temp_dir().join(format!("omp-bridge-idle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let registry = Arc::new(Registry::load(folder.join("instances.json"), folder.clone()).unwrap());
        let worker = registry.create(CreateInput { project_path: folder.to_string_lossy().into(), ..Default::default() }).unwrap();
        let hub = ManagerHub::bind(folder.join("bridge.mjs")).unwrap();
        hub.bind_registry(Arc::downgrade(&registry)).unwrap();
        hub.install_terminal_service(TerminalService::new(&folder)).unwrap();
        let launch = hub.launch(&Definition {id:worker.id.clone(),input:CreateInput::default(),session_path:None,kind:InstanceKind::Instance,manager_id:None,plan_target:None,helper:None},0).unwrap();
        let mut socket = TcpStream::connect(&hub.endpoint).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let hello = json!({"type":"hello","id":"hello","protocol":1,"instanceId":worker.id,"generation":0,"token":launch.token});
        socket.write_all(format!("{hello}\n").as_bytes()).unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        assert_eq!(read_frame(&mut reader).unwrap().unwrap()["ok"], true);
        // A quiet bridge (no events for longer than the 10 s hello deadline) must still get answers.
        std::thread::sleep(Duration::from_secs(11));
        socket.write_all(format!("{}\n", json!({"type":"request","id":"after-idle","action":"list","payload":{}})).as_bytes()).unwrap();
        let response = read_frame(&mut reader).unwrap().unwrap();
        assert_eq!(response["id"], "after-idle");
        assert_eq!(response["error"], "unauthorized");
        hub.shutdown();
        std::fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn planner_actions_are_confined_to_their_target() {
        let folder = std::env::temp_dir().join(format!("omp-planner-hub-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let registry = Arc::new(Registry::load(folder.join("instances.json"), folder.clone()).unwrap());
        let config = CreateInput { project_path: folder.to_string_lossy().into(), ..Default::default() };
        let target = registry.create(CreateInput { name: "Target".into(), ..config.clone() }).unwrap();
        let other = registry.create(config.clone()).unwrap();
        let planner = registry.create_planner(&target.id).unwrap();
        let hub = ManagerHub::bind(folder.join("bridge.mjs")).unwrap();
        hub.bind_registry(Arc::downgrade(&registry)).unwrap();
        hub.install_terminal_service(TerminalService::new(&folder)).unwrap();
        registry.install_manager_hub(hub.clone()).unwrap();
        let definition = |id: &str, plan_target: Option<String>| Definition { id: id.into(), input: CreateInput::default(), session_path: None, kind: InstanceKind::Instance, manager_id: None, plan_target, helper: None };
        let target_launch = hub.launch(&definition(&target.id, None), 0).unwrap();
        hub.launch(&definition(&planner.id, Some(target.id.clone())), 0).unwrap();
        hub.launch(&definition(&other.id, None), 0).unwrap();
        let actor = |id: &str| Actor { id: id.into(), generation: 0, role: InstanceKind::Instance, connection_id: String::new() };
        let ask = |who: &str, action: &str, payload: Value| hub.execute(&actor(who), &json!({"action": action, "payload": payload}));

        // The target's bridge: records every control frame and accepts it.
        let mut socket = TcpStream::connect(&hub.endpoint).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        socket.write_all(format!("{}\n", json!({"type":"hello","id":"hello","protocol":1,"instanceId":target.id,"generation":0,"token":target_launch.token})).as_bytes()).unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        assert_eq!(read_frame(&mut reader).unwrap().unwrap()["ok"], true);
        reader.get_ref().set_read_timeout(None).unwrap();
        socket.write_all(format!("{}\n", json!({"type":"event","eventId":"1","kind":"ready","payload":{"sessionId":"s","supported":true,"materialized":false}})).as_bytes()).unwrap();
        let recorded = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let log = recorded.clone();
        let mut replies = socket.try_clone().unwrap();
        std::thread::spawn(move || {
            while let Ok(Some(frame)) = read_frame(&mut reader) {
                if frame["type"] == "control" {
                    log.lock().unwrap().push((frame["action"].as_str().unwrap_or_default().into(), frame["payload"].clone()));
                    let _ = replies.write_all(format!("{}\n", json!({"type":"response","id":frame["id"],"ok":true,"data":{"accepted":true}})).as_bytes());
                }
            }
        });
        hub.wait_ready(&target.id).unwrap();
        let controls = |name: &str| recorded.lock().unwrap().iter().filter(|(action, _)| action == name).map(|(_, payload)| payload.clone()).collect::<Vec<_>>();

        assert_eq!(ask(&planner.id, "plan_handoff", json!({"summary": "x"})).unwrap_err(), "There is no plan to hand off; call write_plan first");
        let written = ask(&planner.id, "plan_write_plan", json!({"text": "# New\nbody"})).unwrap();
        assert_eq!(written["interrupted"], true);
        let plan_file = registry.plan_file(&target.id).unwrap();
        assert_eq!(std::fs::read_to_string(&plan_file).unwrap(), "# New\nbody");
        assert_eq!(controls("abort").len(), 1);
        let info = ask(&planner.id, "plan_handoff_info", json!({})).unwrap();
        assert_eq!(info["needsStart"], false);
        assert!(ask(&planner.id, "plan_handoff", json!({"summary":"s", "revision":"stale"})).unwrap_err().contains("confirm the new revision"));
        assert_eq!(controls("prompt").len(), 0);
        assert_eq!(registry.inspect(&target.id, Origin::Human).unwrap().last_plan_handoff, None);
        assert_eq!(ask(&planner.id, "plan_handoff", json!({"summary": "dropped the migration", "revision":info["revision"]})).unwrap()["accepted"], true);
        let handoff_marker = registry.inspect(&target.id, Origin::Human).unwrap().last_plan_handoff.unwrap();
        let prompts = controls("prompt");
        assert_eq!(prompts.len(), 1);
        let message = prompts[0]["message"].as_str().unwrap();
        assert!(message.starts_with("CHANGE OF PLANS") && message.contains("dropped the migration") && message.contains("# New\nbody"));
        assert_eq!(prompts[0]["source"], "human");
        assert_eq!(ask(&planner.id, "plan_interrupt", json!({})).unwrap()["interrupted"], true);
        assert_eq!(controls("abort").len(), 2);
        assert_eq!(ask(&planner.id, "plan_status", json!({})).unwrap()["name"], "Target");
        // The saved revision is visible before the target updates its own todo list.
        let read = ask(&planner.id, "plan_read_plan", json!({})).unwrap();
        assert_eq!(read["text"], "# New\nbody");
        assert_eq!(read["path"], json!(plan_file));

        // Only the listed fields are accepted and a plan agent can never name another session.
        assert!(ask(&planner.id, "plan_write_plan", json!({"text": "a", "instanceId": other.id})).unwrap_err().starts_with("invalid_plan_payload: plan_write_plan does not accept 'instanceId'"));
        assert!(ask(&planner.id, "plan_status", json!({"instanceId": other.id})).unwrap_err().starts_with("invalid_plan_payload"));
        assert_eq!(ask(&planner.id, "plan_write_plan", json!("{\"text\":\"x\"}")).unwrap_err(), "invalid_plan_payload: payload must be a JSON object");
        assert_eq!(ask(&planner.id, "plan_write_plan", json!({"text": "  "})).unwrap_err(), "plan text must be 1..262144 characters");
        assert_eq!(ask(&planner.id, "plan_handoff", json!({"summary": ""})).unwrap_err(), "summary must be 1..4000 characters");
        assert_eq!(ask(&planner.id, "plan_nope", json!({})).unwrap_err(), "invalid_action");
        assert_eq!(ask(&other.id, "plan_status", json!({})).unwrap_err(), "unauthorized");
        assert_eq!(std::fs::read_to_string(&plan_file).unwrap(), "# New\nbody");
        let replacement = ask(&planner.id, "plan_write_plan", json!({"text":"# Another revision"})).unwrap();
        hub.restore_plan_backup(&target.id, replacement["backup"].as_str().unwrap()).unwrap();
        assert_eq!(registry.plan(&target.id, Origin::Human).unwrap().as_deref(), Some("# New\nbody"));
        assert_eq!(controls("prompt").len(), 1, "restoration must not hand off");
        assert_eq!(registry.inspect(&target.id, Origin::Human).unwrap().last_plan_handoff.as_deref(), Some(handoff_marker.as_str()), "restoration must not navigate away");
        assert!(plan::list_backups(&registry.plan_folder(&target.id)).unwrap().iter().any(|b| b.text == "# Another revision"));
        let current_info = ask(&planner.id, "plan_handoff_info", json!({})).unwrap();
        let handoff = json!({"summary":"send again", "revision":current_info["revision"]});
        ask(&planner.id, "plan_handoff", handoff.clone()).unwrap();
        let first = registry.inspect(&target.id, Origin::Human).unwrap().last_plan_handoff.unwrap();
        ask(&planner.id, "plan_handoff", handoff).unwrap();
        assert_ne!(registry.inspect(&target.id, Origin::Human).unwrap().last_plan_handoff.as_deref(), Some(first.as_str()), "each successful handoff must trigger navigation, even for the same plan revision");

        // A target whose bridge is not connected cannot be interrupted, but its plan can still be written.
        let idle = registry.create(config).unwrap();
        let idle_planner = registry.create_planner(&idle.id).unwrap();
        hub.launch(&definition(&idle.id, None), 0).unwrap();
        hub.launch(&definition(&idle_planner.id, Some(idle.id.clone())), 0).unwrap();
        assert_eq!(ask(&idle_planner.id, "plan_interrupt", json!({})).unwrap_err(), plan::NOT_RUNNING);
        let offline = ask(&idle_planner.id, "plan_write_plan", json!({"text": "# Offline"})).unwrap();
        assert_eq!(offline["interrupted"], false);
        assert_eq!(std::fs::read_to_string(registry.plan_file(&idle.id).unwrap()).unwrap(), "# Offline");
        let info = ask(&idle_planner.id, "plan_handoff_info", json!({})).unwrap();
        assert_eq!(info["needsStart"], true);
        assert_eq!(ask(&idle_planner.id, "plan_handoff", json!({"summary": "s", "revision":info["revision"]})).unwrap_err(), plan::NOT_RUNNING);
        assert_eq!(registry.inspect(&idle.id, Origin::Human).unwrap().last_plan_handoff, None);
        hub.shutdown();
        std::fs::remove_dir_all(folder).unwrap();
    }
}
