//! The named OS mutex is authority; the loopback capability only requests its cooperative release.
use super::process_lock::ProcessLock;
use super::store::{self, Paths};
use super::types::*;
use super::CodeQualityService;
use crate::dependency_security::{atomic_json, bounded_json};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FRAME_LIMIT: usize = 16 * 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(600);
const CLOSE_OTHER: &str = "The other OMP Manager must finish closing before control can be transferred. Close that app and retry.";

pub(crate) struct Ownership {
    pub lock: Option<ProcessLock>,
    instance_id: String,
    pub handed_off: bool,
    pub transferring: bool,
    listener: Option<ListenerHandle>,
}

struct ListenerHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ListenerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

impl Ownership {
    pub fn new(lock: Option<ProcessLock>) -> Self {
        Self { lock, instance_id: uuid::Uuid::new_v4().to_string(), handed_off: false, transferring: false, listener: None }
    }

    pub fn snapshot(&self, paths: &Paths) -> QualityOwnership {
        let status = if self.transferring { OwnershipStatus::Transferring }
            else if self.handed_off { OwnershipStatus::HandedOff }
            else if self.lock.is_some() { OwnershipStatus::Owned }
            else { OwnershipStatus::OtherProcess };
        let owner_pid = if self.lock.is_some() { Some(std::process::id()) }
            else { read_descriptor(paths).ok().map(|d| d.pid) };
        QualityOwnership { status, owner_pid }
    }
}

impl Drop for Ownership {
    fn drop(&mut self) {
        // Close the listening socket before the lock field is automatically dropped.
        self.listener.take();
    }
}

// Deliberately no Debug: capability must never enter diagnostics or DTOs.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Descriptor {
    version: u32,
    instance_id: String,
    pid: u32,
    port: u16,
    capability: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    version: u32,
    instance_id: String,
    capability: String,
    request_id: String,
    action: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Response {
    version: u32,
    instance_id: String,
    request_id: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn read_descriptor(paths: &Paths) -> Result<Descriptor> {
    let descriptor = bounded_json::<Descriptor>(&paths.root.join("owner.json"), store::MAX_STATE_BYTES)?
        .ok_or_else(|| CLOSE_OTHER.to_string())?;
    if descriptor.version != 1 || descriptor.port == 0 || descriptor.pid == 0
        || uuid::Uuid::parse_str(&descriptor.instance_id).is_err()
        || descriptor.capability.len() != 64 || !descriptor.capability.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(CLOSE_OTHER.into());
    }
    Ok(descriptor)
}

fn read_frame<T: for<'de> Deserialize<'de>>(stream: &mut TcpStream, deadline: Instant) -> Result<T> {
    let mut bytes = Vec::with_capacity(512);
    let mut buffer = [0; 1024];
    loop {
        let remaining = deadline.checked_duration_since(Instant::now()).ok_or("Code quality handover timed out.")?;
        stream.set_read_timeout(Some(remaining)).map_err(|e| e.to_string())?;
        let count = stream.read(&mut buffer).map_err(|_| CLOSE_OTHER.to_string())?;
        if count == 0 { return Err(CLOSE_OTHER.into()); }
        if let Some(end) = buffer[..count].iter().position(|byte| *byte == b'\n') {
            if bytes.len() + end + 1 > FRAME_LIMIT { return Err("Code quality handover frame is too large.".into()); }
            bytes.extend_from_slice(&buffer[..end]);
            return serde_json::from_slice(&bytes).map_err(|_| "Invalid code quality handover frame.".into());
        }
        if bytes.len() + count >= FRAME_LIMIT { return Err("Code quality handover frame is too large.".into()); }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

fn write_frame(stream: &mut TcpStream, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| "Could not encode code quality handover.".to_string())?;
    if bytes.len() + 1 > FRAME_LIMIT { return Err("Code quality handover frame is too large.".into()); }
    bytes.push(b'\n');
    stream.set_write_timeout(Some(HEADER_TIMEOUT)).map_err(|e| e.to_string())?;
    stream.write_all(&bytes).map_err(|_| CLOSE_OTHER.to_string())
}

impl CodeQualityService {
    pub(crate) fn owns_resources(&self) -> bool { self.ownership.lock().lock.is_some() }

    pub(crate) fn publish_owner(self: &Arc<Self>) -> Result<()> {
        let mut ownership = self.ownership.lock();
        self.publish_owner_locked(&mut ownership)
    }

    fn publish_owner_locked(self: &Arc<Self>, ownership: &mut Ownership) -> Result<()> {
        if ownership.lock.is_none() || ownership.listener.is_some() { return Ok(()); }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("Could not listen for code quality handover: {e}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let descriptor = Descriptor {
            version: 1, instance_id: ownership.instance_id.clone(), pid: std::process::id(),
            port: listener.local_addr().map_err(|e| e.to_string())?.port(),
            capability: format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple()),
        };
        std::fs::create_dir_all(&self.paths.root).map_err(|e| e.to_string())?;
        atomic_json(&self.paths.root.join("owner.json"), &descriptor, store::MAX_STATE_BYTES)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let weak = Arc::downgrade(self);
        let active = Arc::new(AtomicUsize::new(0));
        let generation = descriptor.instance_id;
        let capability = descriptor.capability;
        let thread = std::thread::Builder::new().name("code-quality-handover".into()).spawn(move || {
            while !stopping.load(Ordering::Acquire) && weak.strong_count() != 0 {
                match listener.accept() {
                    Ok((mut stream, address)) => {
                        if !address.ip().is_loopback() { continue; }
                        if active.fetch_add(1, Ordering::AcqRel) >= 8 {
                            active.fetch_sub(1, Ordering::AcqRel);
                            continue;
                        }
                        let weak = weak.clone();
                        let generation = generation.clone();
                        let capability = capability.clone();
                        let active = active.clone();
                        let failed_active = active.clone();
                        let handler = move || {
                            // Accepted sockets can inherit nonblocking mode on Windows.
                            let result = stream.set_nonblocking(false).map_err(|e| e.to_string()).and_then(|_| {
                                read_frame::<Request>(&mut stream, Instant::now() + HEADER_TIMEOUT)
                            });
                            if let Ok(request) = result {
                                if request.version == 1 && request.instance_id == generation && request.capability == capability
                                    && request.action == "handover" && uuid::Uuid::parse_str(&request.request_id).is_ok() {
                                    if let Some(service) = weak.upgrade() {
                                        let result = service.handover(&generation, Instant::now() + TRANSFER_TIMEOUT);
                                        let (status, message) = match result {
                                            Ok(()) => ("released", None),
                                            Err(error) if error == BUSY_MESSAGE => ("busy", Some("Code quality is busy. Retry after the current operation finishes.".into())),
                                            Err(error) => ("error", Some(error)),
                                        };
                                        let _ = write_frame(&mut stream, &Response { version: 1, instance_id: generation, request_id: request.request_id, status: status.into(), message });
                                    }
                                }
                            }
                            active.fetch_sub(1, Ordering::AcqRel);
                        };
                        if std::thread::Builder::new().name("code-quality-handover-request".into()).spawn(handler).is_err() {
                            failed_active.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
        }).map_err(|e| e.to_string())?;
        ownership.listener = Some(ListenerHandle { stop, thread: Some(thread) });
        Ok(())
    }

    /// File-only acquisition: never starts an engine, including when called by polling.
    pub fn refresh_ownership(self: &Arc<Self>) -> Result<()> {
        if self.closing.load(Ordering::Relaxed) { return Ok(()); }
        let mut ownership = self.ownership.lock();
        if ownership.lock.is_some() || ownership.handed_off || ownership.transferring { return Ok(()); }
        self.acquire_owner_locked(&mut ownership)
    }

    fn acquire_owner_locked(self: &Arc<Self>, ownership: &mut Ownership) -> Result<()> {
        let Some(lock) = ProcessLock::acquire(&super::process_lock::name_for(&self.paths.root))? else { return Ok(()); };
        // Retain the mutex even if persisted data is corrupt: no mutation can pass the fatal gate.
        ownership.lock = Some(lock);
        ownership.handed_off = false;
        ownership.instance_id = uuid::Uuid::new_v4().to_string();
        let loaded: Result<(store::ConfigFile, String)> = (|| {
            let file = store::load_config(&self.paths)?;
            store::load_resources(&self.paths)?;
            let installation = store::load_installation(&self.paths)?;
            Ok((file, installation))
        })();
        match loaded {
            Ok((file, installation)) => {
                let mut inner = self.inner.lock();
                inner.file = file;
                inner.installation = installation;
                inner.fatal = None;
                inner.error = None;
                inner.phase = QualityPhase::Idle;
                inner.effective = None;
                inner.community = CommunityState::default();
                inner.prerequisites.clear();
                inner.options = QualityOptions::default();
                inner.unresolved.clear();
                inner.setup_resume = self.pending_setup_path().is_file();
            }
            Err(error) => {
                let mut inner = self.inner.lock();
                inner.fatal = Some(error.clone());
                inner.error = Some(error.clone());
                return Err(error);
            }
        }
        ownership.handed_off = false;
        self.refresh_rules();
        self.refresh_credentials();
        self.recover_interrupted_jobs();
        if let Err(error) = self.publish_owner_locked(ownership) {
            let mut inner = self.inner.lock();
            inner.fatal = Some(error.clone());
            inner.error = Some(error.clone());
            return Err(error);
        }
        Ok(())
    }

    pub fn take_over(self: &Arc<Self>, consent: bool) -> Result<QualityState> {
        if !consent { return Err("Confirm the code quality handover before taking control.".into()); }
        {
            let mut ownership = self.ownership.lock();
            if ownership.transferring { return Err(BUSY_MESSAGE.into()); }
            if self.closing.load(Ordering::Relaxed) { return Err("OMP Manager is closing.".into()); }
            if ownership.lock.is_some() { drop(ownership); return Ok(self.state()); }
            ownership.transferring = true;
        }
        let result = self.request_handover();
        self.ownership.lock().transferring = false;
        result?;
        Ok(self.state())
    }

    fn request_handover(self: &Arc<Self>) -> Result<()> {
        {
            let mut ownership = self.ownership.lock();
            self.acquire_owner_locked(&mut ownership)?;
            if ownership.lock.is_some() { return Ok(()); }
        }
        let descriptor = read_descriptor(&self.paths).map_err(|_| CLOSE_OTHER.to_string())?;
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, descriptor.port));
        let mut stream = TcpStream::connect_timeout(&address, HEADER_TIMEOUT).map_err(|_| CLOSE_OTHER.to_string())?;
        let request_id = uuid::Uuid::new_v4().to_string();
        write_frame(&mut stream, &Request { version: 1, instance_id: descriptor.instance_id.clone(), capability: descriptor.capability, request_id: request_id.clone(), action: "handover".into() })?;
        let response: Response = read_frame(&mut stream, Instant::now() + TRANSFER_TIMEOUT).map_err(|_| CLOSE_OTHER.to_string())?;
        if response.version != 1 || response.instance_id != descriptor.instance_id || response.request_id != request_id { return Err(CLOSE_OTHER.into()); }
        match response.status.as_str() {
            "released" => {}
            "busy" => return Err("Code quality is busy in the other OMP Manager. Retry after its current operation finishes.".into()),
            "error" => return Err(response.message.unwrap_or_else(|| CLOSE_OTHER.into())),
            _ => return Err(CLOSE_OTHER.into()),
        }
        self.ownership.lock().handed_off = false;
        let deadline = Instant::now() + HEADER_TIMEOUT;
        loop {
            let mut ownership = self.ownership.lock();
            self.acquire_owner_locked(&mut ownership)?;
            if ownership.lock.is_some() { return Ok(()); }
            if Instant::now() >= deadline { return Ok(()); } // A third contender may legitimately win.
            drop(ownership);
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn handover(self: &Arc<Self>, generation: &str, deadline: Instant) -> Result<()> {
        {
            let mut ownership = self.ownership.lock();
            if ownership.lock.is_none() || ownership.instance_id != generation || ownership.handed_off { return Err(CLOSE_OTHER.into()); }
            if ownership.transferring || self.closing.load(Ordering::Relaxed) { return Err(BUSY_MESSAGE.into()); }
            let inner = self.inner.lock();
            if inner.busy && inner.active_scan.is_none() { return Err(BUSY_MESSAGE.into()); }
            ownership.transferring = true;
            if let Some(scan) = &inner.active_scan { scan.cancel.store(true, Ordering::Relaxed); }
        }
        let result: Result<()> = (|| {
            while self.inner.lock().busy {
                if Instant::now() >= deadline { return Err("The active scan did not finish cancelling; control was not released.".into()); }
                std::thread::sleep(Duration::from_millis(100));
            }
            if Instant::now() >= deadline { return Err("Code quality handover timed out; control was not released.".into()); }
            let gate = self.admit_internal(QualityPhase::Stopping, None, true, false)?;
            let (done, completed) = std::sync::mpsc::channel::<()>();
            let cancel = gate.ctl.cancel.clone();
            let remaining = deadline.saturating_duration_since(Instant::now());
            std::thread::Builder::new().name("code-quality-handover-deadline".into()).spawn(move || {
                if matches!(completed.recv_timeout(remaining), Err(std::sync::mpsc::RecvTimeoutError::Timeout)) {
                    cancel.store(true, Ordering::Relaxed);
                }
            }).map_err(|e| e.to_string())?;
            let _done = done;
            self.verify_pending_tasks_stopped()?;
            self.stop_recorded_servers(&gate)?;
            if self.has_unresolved() || Instant::now() >= deadline { return Err("Managed work could not be confirmed stopped; control was not released.".into()); }
            {
                let mut inner = self.inner.lock();
                inner.effective = None;
                inner.community.running = Some(false);
                inner.error = None;
            }
            gate.finish(QualityPhase::Idle, None);
            self.release_owner(true);
            Ok(())
        })();
        if let Err(error) = &result { self.inner.lock().error = Some(error.clone()); }
        self.ownership.lock().transferring = false;
        result
    }

    pub(crate) fn verify_pending_tasks_stopped(&self) -> Result<()> {
        let resources = store::load_resources(&self.paths)?;
        for task in resources.tasks {
            let target = match task.config.mode {
                QualityMode::SonarqubeCommunity => self.community_token_target(&task.config.runtime),
                QualityMode::SonarqubeEnterprise => Some(self.enterprise_token_target(&task.config.enterprise_url)),
                QualityMode::Alternate => None,
            };
            let token = target.map(|target| self.secrets.read(&target)).transpose()?.flatten();
            if token.is_none() { return Err("A previous SonarQube task cannot be verified without its stored token.".into()); }
            self.reconcile_pending_tasks(&task.config, token.as_deref())?;
        }
        if !store::load_resources(&self.paths)?.tasks.is_empty() { return Err("SonarQube tasks remain unresolved.".into()); }
        Ok(())
    }

    pub(crate) fn release_owner(&self, handed_off: bool) {
        let mut ownership = self.ownership.lock();
        if ownership.lock.is_none() { return; }
        ownership.listener.take();
        if read_descriptor(&self.paths).is_ok_and(|d| d.instance_id == ownership.instance_id) {
            let _ = std::fs::remove_file(self.paths.root.join("owner.json"));
        }
        ownership.handed_off = handed_off;
        // This is LAST: only a new mutex holder may now publish a descriptor or mutate resources.
        ownership.lock.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (receiver, _) = listener.accept().unwrap();
        (sender, receiver)
    }

    #[test]
    fn oversized_and_malformed_frames_are_rejected() {
        let (mut sender, mut receiver) = pair();
        sender.write_all(&vec![b'x'; FRAME_LIMIT + 1]).unwrap();
        let result: Result<serde_json::Value> = read_frame(&mut receiver, Instant::now() + HEADER_TIMEOUT);
        assert!(result.unwrap_err().contains("too large"));
        let (mut sender, mut receiver) = pair();
        sender.write_all(b"{invalid}\n").unwrap();
        let result: Result<serde_json::Value> = read_frame(&mut receiver, Instant::now() + HEADER_TIMEOUT);
        assert!(result.unwrap_err().contains("Invalid"));
    }

    #[test]
    fn header_deadline_bounds_an_incomplete_request() {
        let (_sender, mut receiver) = pair();
        let began = Instant::now();
        let result: Result<serde_json::Value> = read_frame(&mut receiver, began + Duration::from_millis(20));
        assert!(result.is_err());
        assert!(began.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn expired_transfer_keeps_authority_and_does_not_touch_an_engine() {
        struct NoEngine;
        impl super::super::runtime::EngineFactory for NoEngine {
            fn engine(&self, _: &QualityRuntime) -> Result<Arc<dyn super::super::runtime::Engine>> {
                panic!("a timed out handover must not reach an engine")
            }
        }
        let dir = std::env::temp_dir().join(format!("omp-cq-expired-{}", uuid::Uuid::new_v4()));
        let service = CodeQualityService::with_parts(dir.clone(), Arc::new(NoEngine),
            Arc::new(super::super::secrets::MemorySecrets::default()), Ok(Some(ProcessLock::unowned())),
            Arc::new(|_, _| (Vec::new(), QualityOptions::default())));
        let generation = service.ownership.lock().instance_id.clone();
        assert!(service.handover(&generation, Instant::now()).unwrap_err().contains("timed out"));
        assert_eq!(service.state().ownership.status, OwnershipStatus::Owned);
        assert!(!service.ownership.lock().transferring);
        service.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }
}
