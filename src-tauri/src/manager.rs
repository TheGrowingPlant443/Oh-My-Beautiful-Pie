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
use serde_json::{json, Value};

const MAX_FRAME: usize = 64 * 1024 * 1024;
type Connection = Arc<Mutex<TcpStream>>;
pub type Result<T, E = String> = std::result::Result<T, E>;

pub struct BridgeLaunch {
    pub endpoint: String,
    pub token: String,
    pub extension_path: PathBuf,
    pub manager_setup_dir: PathBuf,
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
        Ok(BridgeLaunch {
            endpoint: self.endpoint.clone(),
            token,
            extension_path: self.extension_path.clone(),
            manager_setup_dir: self
                .extension_path
                .parent()
                .ok_or("invalid bridge directory")?
                .join("manager-setup"),
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
                Some("request") if actor.role == InstanceKind::Manager => {
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
        if registry.inspect(id, Origin::Human)?.kind != InstanceKind::Instance {
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
    fn execute(&self, actor: &Actor, frame: &Value) -> Result<Value> {
        if actor.role != InstanceKind::Manager {
            return Err("unauthorized".into());
        }
        let registry = self.registry()?;
        if registry.lease(&actor.id, Origin::Human, false)?.generation != actor.generation {
            return Err("stale_generation".into());
        }
        let action = text(frame, "action")?;
        let p = &frame["payload"];
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
            let input: CreateInput =
                serde_json::from_value(p["config"].clone()).map_err(|e| e.to_string())?;
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
        let definition = Definition {
            id: manager.id.clone(),
            input: CreateInput::default(),
            session_path: None,
            kind: InstanceKind::Manager,
            manager_id: None,
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
}
