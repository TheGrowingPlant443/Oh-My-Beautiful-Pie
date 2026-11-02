//! Native leases need flock on Unix; std has no advisory lock operation that explicitly releases inherited leases.
use super::browser_supervisor::{self, BrowserRoot, NativeError};
use crate::process_tree::{ContainedChild, ContainedSpec, StdioMode};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

pub(super) const ENDPOINT_DEADLINE: Duration = Duration::from_secs(20);
const TOMBSTONES: usize = 512;
#[cfg(windows)]
const ERROR_SHARING_VIOLATION: i32 = 32;
#[cfg(windows)]
const ERROR_LOCK_VIOLATION: i32 = 33;

type NativeResult<T> = std::result::Result<T, NativeError>;

pub(super) fn busy(message: &str) -> NativeError {
    NativeError::new(
        "browser_profile_busy",
        format!("browser_profile_busy: {message}. If a Chrome window is using this terminal's browser profile, close it normally and try again."),
    )
}

/// Exclusive no-sharing handle on `<instance>/owner.lock`. The file's existence is not authority; the open handle is.
#[derive(Debug)]
pub(super) struct Lease {
    _file: fs::File,
}
#[cfg(unix)]
impl Drop for Lease {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // A concurrent fork can briefly inherit the open-file description before exec closes it.
        // Explicit unlock releases our lease now, not when that unrelated child eventually execs.
        // SAFETY: _file owns the live descriptor until after this explicit unlock.
        unsafe {
            libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
#[cfg(windows)]
pub(super) fn acquire_lease(path: &Path) -> NativeResult<Lease> {
    use std::os::windows::fs::OpenOptionsExt;
    let opened = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path);
    match opened {
        Ok(file) => {
            let meta = file.metadata().map_err(|e| {
                NativeError::new(
                    "browser_lease_failed",
                    format!("inspect profile lease: {e}"),
                )
            })?;
            if !meta.is_file() {
                return Err(NativeError::new(
                    "browser_lease_failed",
                    "profile lease is not a regular file",
                ));
            }
            Ok(Lease { _file: file })
        }
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
            ) =>
        {
            Err(busy("another process owns this terminal's browser profile"))
        }
        Err(error) => Err(NativeError::new(
            "browser_lease_failed",
            format!("profile lease: {error}"),
        )),
    }
}

#[cfg(unix)]
pub(super) fn acquire_lease(path: &Path) -> NativeResult<Lease> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| NativeError::new("browser_lease_failed", e.to_string()))?;
    let meta = file
        .metadata()
        .map_err(|e| NativeError::new("browser_lease_failed", e.to_string()))?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(NativeError::new(
            "browser_lease_failed",
            "Profile lock must be a single-linked regular file",
        ));
    }
    // SAFETY: file is an owned descriptor. flock is released when the last descriptor closes.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(busy("another process owns this terminal's browser profile"));
    }
    Ok(Lease { _file: file })
}

pub(super) struct SlotInner {
    child: Option<Arc<ContainedChild>>,
    lease: Option<Lease>,
    retired: bool,
}
pub(super) struct Slot {
    pub instance_id: String,
    pub generation: u64,
    pub operation_id: String,
    pub helper_session: String,
    pub lease_id: String,
    cancelled: AtomicBool,
    ready: AtomicBool,
    identity: OnceLock<(u32, u64)>,
    inner: Mutex<SlotInner>,
}

#[derive(Default)]
struct NativeState {
    slots: HashMap<String, Arc<Slot>>,
    /// Cancelled operation ids: a spawn that arrives after its retirement must not start anything.
    tombstones: VecDeque<String>,
}
#[derive(Default)]
pub(super) struct Natives {
    state: Mutex<NativeState>,
}

impl Slot {
    pub(super) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    pub(super) fn identity(&self) -> Option<(u32, u64)> {
        self.identity.get().copied()
    }
    pub(super) fn mark_ready(&self) -> bool {
        if self.cancelled() {
            return false;
        }
        self.ready.store(true, Ordering::SeqCst);
        true
    }
    pub(super) fn child(&self) -> Option<Arc<ContainedChild>> {
        self.inner.lock().child.clone()
    }
    #[cfg(test)]
    pub(super) fn test_install(&self, child: Arc<ContainedChild>, lease: Lease) {
        let _ = self
            .identity
            .set((child.pid(), child.creation_time_100ns()));
        let mut inner = self.inner.lock();
        inner.child = Some(child);
        inner.lease = Some(lease);
    }
    /// Joins any in-flight spawn, then terminates the tree (after an optional grace period for a clean root exit),
    /// verifies the job is empty and only then releases the lease. Failure keeps lease and child for a retry.
    pub(super) fn retire(&self, natives: &Natives, grace: Duration) -> NativeResult<()> {
        self.cancelled.store(true, Ordering::SeqCst);
        self.ready.store(false, Ordering::SeqCst);
        let mut inner = self.inner.lock();
        self.retire_locked(&mut inner, grace)?;
        drop(inner);
        natives.forget(self);
        Ok(())
    }
    fn retire_locked(&self, inner: &mut SlotInner, grace: Duration) -> NativeResult<()> {
        if inner.retired {
            return Ok(());
        }
        if let Some(child) = inner.child.as_ref() {
            if !grace.is_zero() {
                let _ = child.wait_timeout(grace);
            }
            child.terminate().map_err(|e| {
                NativeError::new(
                    "browser_cleanup_blocked",
                    format!("browser_cleanup_blocked: {e}"),
                )
            })?;
        }
        inner.child = None;
        inner.lease = None;
        inner.retired = true;
        Ok(())
    }
}

impl Natives {
    pub(super) fn begin(
        &self,
        instance: &str,
        generation: u64,
        operation: &str,
        session: &str,
    ) -> NativeResult<Arc<Slot>> {
        let mut state = self.state.lock();
        if state.tombstones.iter().any(|t| t == operation) {
            return Err(NativeError::new(
                "operation_cancelled",
                "operation_cancelled",
            ));
        }
        if state.slots.contains_key(instance) {
            return Err(busy(
                "a previous browser of this terminal has not finished closing",
            ));
        }
        let slot = Arc::new(Slot {
            instance_id: instance.into(),
            generation,
            operation_id: operation.into(),
            helper_session: session.into(),
            lease_id: uuid::Uuid::new_v4().to_string(),
            cancelled: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            identity: OnceLock::new(),
            inner: Mutex::new(SlotInner {
                child: None,
                lease: None,
                retired: false,
            }),
        });
        state.slots.insert(instance.into(), slot.clone());
        Ok(slot)
    }
    fn forget(&self, slot: &Slot) {
        let mut state = self.state.lock();
        if state
            .slots
            .get(&slot.instance_id)
            .is_some_and(|current| std::ptr::eq(Arc::as_ptr(current), slot))
        {
            state.slots.remove(&slot.instance_id);
        }
    }
    /// Cancels `operation` for good (late spawns are refused) and returns the slot it already created, if any.
    pub(super) fn cancel_operation(&self, instance: &str, operation: &str) -> Option<Arc<Slot>> {
        let mut state = self.state.lock();
        if !state.tombstones.iter().any(|t| t == operation) {
            state.tombstones.push_back(operation.into());
            while state.tombstones.len() > TOMBSTONES {
                state.tombstones.pop_front();
            }
        }
        state
            .slots
            .get(instance)
            .filter(|slot| slot.operation_id == operation)
            .cloned()
    }
    pub(super) fn find(&self, instance: &str, operation: &str) -> Option<Arc<Slot>> {
        self.state
            .lock()
            .slots
            .get(instance)
            .filter(|slot| slot.operation_id == operation)
            .cloned()
    }
    fn matching(&self, keep: impl Fn(&Slot) -> bool) -> Vec<Arc<Slot>> {
        self.state
            .lock()
            .slots
            .values()
            .filter(|slot| keep(slot))
            .cloned()
            .collect()
    }
    fn retire_each(&self, slots: Vec<Arc<Slot>>, grace: Duration) -> NativeResult<()> {
        let mut first = None;
        for slot in slots {
            if let Err(error) = slot.retire(self, grace) {
                first.get_or_insert(error);
            }
        }
        first.map_or(Ok(()), Err)
    }
    pub(super) fn retire_instance(
        &self,
        instance: &str,
        generation: Option<u64>,
    ) -> NativeResult<()> {
        let slots = self.matching(|slot| {
            slot.instance_id == instance && generation.is_none_or(|g| slot.generation == g)
        });
        self.retire_each(slots, Duration::ZERO)
    }
    pub(super) fn retire_session(&self, session: &str) -> NativeResult<()> {
        let slots = self.matching(|slot| slot.helper_session == session);
        self.retire_each(slots, Duration::ZERO)
    }
    pub(super) fn retire_all(&self) -> NativeResult<()> {
        let slots = self.matching(|_| true);
        self.retire_each(slots, Duration::ZERO)
    }
    pub(super) fn roots(&self) -> Vec<BrowserRoot> {
        let slots = self.matching(|slot| slot.ready.load(Ordering::SeqCst) && !slot.cancelled());
        slots
            .into_iter()
            .filter_map(|slot| {
                let (pid, creation) = slot.identity()?;
                // A recycled PID has a different creation time; only a still-running exact match is reported.
                (crate::process_tree::process_creation_time(pid) == Some(creation)).then(|| {
                    BrowserRoot {
                        instance_id: slot.instance_id.clone(),
                        generation: slot.generation,
                        pid,
                        creation_time_100ns: creation,
                    }
                })
            })
            .collect()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.state.lock().slots.is_empty()
    }
}

pub(super) struct SpawnRequest<'a> {
    pub root: &'a Path,
    pub instance_id: &'a str,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub log: &'a dyn Fn(&str),
}

/// Lease, canonical profile validation, suspended+contained launch and endpoint capture for an already registered
/// slot. Every failure after the child exists retires it through the same path as a normal close.
pub(super) fn run_spawn(
    slot: &Slot,
    natives: &Natives,
    request: SpawnRequest<'_>,
) -> NativeResult<Value> {
    let mut inner = slot.inner.lock();
    let outcome = spawn_locked(slot, &mut inner, request);
    match outcome {
        Ok(value) => Ok(value),
        Err(error) => {
            slot.ready.store(false, Ordering::SeqCst);
            match slot.retire_locked(&mut inner, Duration::ZERO) {
                Ok(()) => {
                    drop(inner);
                    natives.forget(slot);
                    Err(error)
                }
                // Lease and child stay owned by the slot until a later retirement verifies an empty job.
                Err(blocked) => Err(blocked),
            }
        }
    }
}

fn spawn_locked(
    slot: &Slot,
    inner: &mut SlotInner,
    request: SpawnRequest<'_>,
) -> NativeResult<Value> {
    let cancelled = || NativeError::new("operation_cancelled", "operation_cancelled");
    if slot.cancelled() {
        return Err(cancelled());
    }
    let (base, instance_dir, profile_dir) =
        browser_supervisor::profile_layout(request.root, request.instance_id);
    fs::create_dir_all(&instance_dir).map_err(|e| {
        NativeError::new(
            "browser_profile_unavailable",
            format!("create profile folder: {e}"),
        )
    })?;
    let (canonical_base, canonical_instance) =
        match (fs::canonicalize(&base), fs::canonicalize(&instance_dir)) {
            (Ok(base), Ok(instance)) => (base, instance),
            _ => {
                return Err(NativeError::new(
                    "browser_profile_unavailable",
                    "profile folder cannot be resolved",
                ))
            }
        };
    if canonical_instance.parent() != Some(canonical_base.as_path())
        || canonical_instance.file_name() != instance_dir.file_name()
    {
        return Err(NativeError::new(
            "browser_profile_unavailable",
            "profile folder is not the expected canonical location",
        ));
    }
    inner.lease = Some(acquire_lease(&canonical_instance.join("owner.lock"))?);
    if slot.cancelled() {
        return Err(cancelled());
    }
    fs::create_dir_all(&profile_dir).map_err(|e| {
        NativeError::new(
            "browser_profile_unavailable",
            format!("create profile: {e}"),
        )
    })?;
    if fs::canonicalize(&profile_dir).ok().as_deref()
        != Some(canonical_instance.join("profile").as_path())
    {
        return Err(NativeError::new(
            "browser_profile_unavailable",
            "profile is not the expected canonical location",
        ));
    }
    let executable_name = request
        .executable
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    (request.log)(&format!("spawn phase=start executable={executable_name}"));
    let child = Arc::new(
        ContainedChild::spawn(ContainedSpec {
            subject: "terminal browser".into(),
            program: request.executable,
            args: request.args.into_iter().map(OsString::from).collect(),
            cwd: None,
            env_set: vec![],
            env_remove: vec![],
            stdout: StdioMode::Null,
            stderr: StdioMode::Capture,
            capture_limit: 64 * 1024,
        })
        .map_err(|e| {
            NativeError::new("browser_spawn_failed", format!("browser_spawn_failed: {e}"))
        })?,
    );
    inner.child = Some(child.clone());
    let _ = slot
        .identity
        .set((child.pid(), child.creation_time_100ns()));
    (request.log)(&format!(
        "spawn phase=started pid={} creation={}",
        child.pid(),
        child.creation_time_100ns()
    ));
    let endpoint = child.stderr().wait_match_until(
        ENDPOINT_DEADLINE,
        &|| slot.cancelled() || child.try_wait().map_or(true, |code| code.is_some()),
        browser_supervisor::parse_devtools_endpoint,
    );
    let Some(endpoint) = endpoint else {
        if slot.cancelled() {
            return Err(cancelled());
        }
        let exit = child.try_wait().ok().flatten();
        (request.log)(&format!(
            "spawn phase=endpoint failed exit={exit:?} stderr={}",
            browser_supervisor::redact(&child.stderr().tail())
        ));
        return Err(match exit {
            Some(code) => NativeError::new(
                "browser_exited_before_ready",
                format!("Chrome exited (code {code}) before opening its debugging endpoint. If another Chrome window is using this terminal's browser profile, close it normally and try again."),
            ),
            None => NativeError::new("browser_launch_timeout", "Chrome did not open its debugging endpoint in time."),
        });
    };
    if slot.cancelled() {
        return Err(cancelled());
    }
    (request.log)("spawn phase=endpoint ok");
    Ok(json!({
        "leaseId": slot.lease_id,
        "pid": child.pid(),
        "creationTime100ns": child.creation_time_100ns().to_string(),
        "webSocketUrl": endpoint,
    }))
}
