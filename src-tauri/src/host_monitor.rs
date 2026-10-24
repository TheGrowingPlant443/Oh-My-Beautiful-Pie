//! Request-driven whole-computer process and socket monitor.
//!
//! Nothing here runs on a timer: a native pass happens only inside `collect`, which the visible Terminal tab asks
//! for. A *view* belongs to one open monitor pane; it keeps at most one acknowledged display snapshot and one
//! replacement candidate. Explanation requests pin a snapshot independently of any view (see `explanation`), so a
//! view closing never releases an admitted explanation. Snapshots live in memory only.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;
use uuid::Uuid;

mod explanation;
mod native;
mod snapshot;
mod tables;
#[cfg(test)]
mod tests;
mod types;
pub use explanation::{BlockedBy, ExplainError, ExplainResult};

pub use native::capture_identity;
pub use types::*;

use crate::instance::Registry;
use snapshot::{HostSnapshot, RawCollection};

/// Open views kept at once. A reloaded WebView that never closed its view cannot grow this without bound: the least
/// recently used view is cancelled instead.
const MAX_VIEWS: usize = 8;

/// Effective context of one app-managed instance, as the monitor view captured it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InstanceContext {
    pub generation: u64,
    pub project_path: String,
}

/// Everything the service needs from the outside world; production reads the Registry and Windows, tests inject.
pub(crate) trait Environment: Send + Sync {
    fn supported(&self) -> bool;
    fn instance(&self, instance_id: &str) -> Option<InstanceContext>;
    /// Launched roots whose descendants are highlighted as owned. Read-only.
    fn roots(&self) -> Vec<ManagedRoot>;
    fn collect(&self, cancel: &AtomicBool) -> Result<RawCollection, String>;
}

struct SystemEnvironment {
    registry: Arc<Registry>,
}

impl Environment for SystemEnvironment {
    fn supported(&self) -> bool {
        cfg!(any(windows, target_os = "linux"))
    }

    fn instance(&self, instance_id: &str) -> Option<InstanceContext> {
        self.registry
            .terminal_sources()
            .into_iter()
            .find(|source| source.id == instance_id)
            .map(|source| InstanceContext { generation: source.generation, project_path: source.project_path })
    }

    fn roots(&self) -> Vec<ManagedRoot> {
        #[allow(unused_mut)]
        let mut roots = self.registry.process_roots();
        #[cfg(any(windows, unix))]
        {
            if let Some(browser) = self.registry.browser_supervisor() {
                roots.extend(browser.owned_roots().into_iter().map(|root| ManagedRoot {
                    instance_id: root.instance_id,
                    generation: root.generation,
                    kind: RootKind::Browser,
                    identity: ProcessIdentity { pid: root.pid, creation_time_100ns: root.creation_time_100ns },
                }));
            }
        }
        roots
    }

    fn collect(&self, cancel: &AtomicBool) -> Result<RawCollection, String> {
        native::collect(cancel)
    }
}

/// What an admitted explanation request keeps of its frozen snapshot.
#[derive(Debug)]
pub(crate) struct PinnedSnapshot {
    pub snapshot_id: String,
    pub hash: String,
    pub started_at: u64,
    pub completed_at: u64,
    pub duration_ms: u64,
    pub summary: Value,
}

impl PinnedSnapshot {
    fn of(snapshot: &HostSnapshot) -> Self {
        Self {
            snapshot_id: snapshot.id.clone(),
            hash: snapshot.hash.clone(),
            started_at: snapshot.started_at,
            completed_at: snapshot.completed_at,
            duration_ms: snapshot.duration_ms,
            summary: serde_json::to_value(snapshot.summary()).unwrap_or(Value::Null),
        }
    }
}

/// State a collection reads without taking the store lock, so closing a view reaches an in-flight pass at once.
struct ViewShared {
    id: String,
    instance_id: String,
    generation: u64,
    project_path: String,
    cancel: AtomicBool,
    busy: AtomicBool,
}

struct ViewState {
    shared: Arc<ViewShared>,
    /// The snapshot the pane acknowledged as shown.
    display: Option<String>,
    /// The most recent completed sample, kept apart until the pane acknowledges it on its next collect.
    candidate: Option<String>,
    /// An explanation holds this view's display: no collection may replace it.
    frozen: bool,
    last_used: u64,
}

struct Held {
    snapshot: Arc<HostSnapshot>,
    view_refs: usize,
    pins: usize,
}

#[derive(Default)]
struct Store {
    views: HashMap<String, ViewState>,
    snapshots: HashMap<String, Held>,
    /// explanation request id -> pinned snapshot id
    pins: HashMap<String, String>,
    clock: u64,
}

impl Store {
    fn collect_garbage(&mut self, snapshot_id: &str) {
        if self.snapshots.get(snapshot_id).is_some_and(|held| held.view_refs == 0 && held.pins == 0) {
            self.snapshots.remove(snapshot_id);
        }
    }

    fn release_view_ref(&mut self, snapshot_id: &str) {
        if let Some(held) = self.snapshots.get_mut(snapshot_id) {
            held.view_refs = held.view_refs.saturating_sub(1);
        }
        self.collect_garbage(snapshot_id);
    }

    /// Cancel and remove a view, releasing only its own display/candidate references.
    fn drop_view(&mut self, view_id: &str) {
        if let Some(view) = self.views.remove(view_id) {
            view.shared.cancel.store(true, Ordering::Release);
            for id in [view.display, view.candidate].into_iter().flatten() {
                self.release_view_ref(&id);
            }
        }
    }

    /// The candidate becomes the acknowledged display and the previous display is released.
    fn promote_candidate(&mut self, view_id: &str) {
        let Some(view) = self.views.get_mut(view_id) else { return };
        let Some(candidate) = view.candidate.take() else { return };
        let previous = view.display.replace(candidate);
        if let Some(previous) = previous {
            self.release_view_ref(&previous);
        }
    }

    fn promote_candidate_if(&mut self, view_id: &str, snapshot_id: &str) {
        if self.views.get(view_id).is_some_and(|view| view.candidate.as_deref() == Some(snapshot_id)) {
            self.promote_candidate(view_id);
        }
    }

    fn live_view_showing(&self, snapshot_id: &str, include_candidate: bool) -> Option<String> {
        self.views
            .iter()
            .find(|(_, view)| {
                !view.shared.cancel.load(Ordering::Acquire)
                    && (view.display.as_deref() == Some(snapshot_id)
                        || (include_candidate && view.candidate.as_deref() == Some(snapshot_id)))
            })
            .map(|(id, _)| id.clone())
    }
}

/// Clears a view's one-collection-at-a-time flag however the pass ends.
struct BusyGuard(Arc<ViewShared>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}

pub struct HostMonitorService {
    env: Arc<dyn Environment>,
    store: Mutex<Store>,
    closing: AtomicBool,
    explanations: explanation::Explanations,
}

impl HostMonitorService {
    pub fn new(registry: Arc<Registry>) -> Arc<Self> {
        Self::with_environment(Arc::new(SystemEnvironment { registry }))
    }

    pub(crate) fn with_environment(env: Arc<dyn Environment>) -> Arc<Self> {
        Arc::new(Self {
            env,
            store: Mutex::new(Store::default()),
            closing: AtomicBool::new(false),
            explanations: Default::default(),
        })
    }

    /// Validate an existing app-managed instance (never starting it) and capture its effective context.
    pub fn open(&self, instance_id: &str, expected_generation: u64) -> Result<HostMonitorOpen, String> {
        if self.closing.load(Ordering::Acquire) {
            return Err("monitor_closed".into());
        }
        if !self.env.supported() {
            return Err("unsupported_platform".into());
        }
        let context = self.env.instance(instance_id).ok_or("unknown_instance")?;
        if context.generation != expected_generation {
            return Err("generation_mismatch".into());
        }
        let view_id = Uuid::new_v4().to_string();
        let shared = Arc::new(ViewShared {
            id: view_id.clone(),
            instance_id: instance_id.to_string(),
            generation: context.generation,
            project_path: context.project_path.clone(),
            cancel: AtomicBool::new(false),
            busy: AtomicBool::new(false),
        });
        let mut store = self.store.lock();
        if self.closing.load(Ordering::Acquire) {
            return Err("monitor_closed".into());
        }
        if store.views.len() >= MAX_VIEWS {
            let oldest = store.views.iter().min_by_key(|(_, view)| view.last_used).map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                store.drop_view(&oldest);
            }
        }
        store.clock += 1;
        let last_used = store.clock;
        store.views.insert(
            view_id.clone(),
            ViewState { shared, display: None, candidate: None, frozen: false, last_used },
        );
        Ok(HostMonitorOpen {
            view_id,
            context: HostMonitorContext {
                instance_id: instance_id.to_string(),
                generation: context.generation,
                project_path: context.project_path,
            },
        })
    }

    fn revalidate(&self, shared: &ViewShared) -> Result<(), String> {
        let context = self.env.instance(&shared.instance_id).ok_or("unknown_instance")?;
        if context.generation != shared.generation || context.project_path != shared.project_path {
            return Err("generation_mismatch".into());
        }
        Ok(())
    }

    /// One native pass for this view. `displayed_snapshot_id` is what the pane currently shows: if it is the pending
    /// candidate it becomes the acknowledged display, and the previous display is released. The display survives
    /// every failed or cancelled replacement.
    pub fn collect(&self, view_id: &str, displayed_snapshot_id: Option<&str>) -> Result<HostSnapshotSummary, String> {
        let shared = {
            let mut store = self.store.lock();
            if self.closing.load(Ordering::Acquire) {
                return Err("monitor_closed".into());
            }
            store.clock += 1;
            let clock = store.clock;
            let view = store.views.get_mut(view_id).ok_or("view_closed")?;
            if view.shared.cancel.load(Ordering::Acquire) {
                return Err("monitor_cancelled".into());
            }
            if view.frozen {
                return Err("monitor_frozen".into());
            }
            if view.shared.busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
                return Err("collection_in_progress".into());
            }
            view.last_used = clock;
            let shared = view.shared.clone();
            if displayed_snapshot_id.is_some() && view.candidate.as_deref() == displayed_snapshot_id {
                store.promote_candidate(view_id);
            }
            shared
        };
        let _busy = BusyGuard(shared.clone());
        let cancelled = || shared.cancel.load(Ordering::Acquire);
        self.revalidate(&shared)?;
        let roots = self.env.roots();
        let raw = self.env.collect(&shared.cancel)?;
        if cancelled() {
            return Err("monitor_cancelled".into());
        }
        let snapshot = Arc::new(HostSnapshot::build(
            Uuid::new_v4().to_string(),
            shared.id.clone(),
            shared.instance_id.clone(),
            shared.generation,
            raw,
            &roots,
        ));
        self.revalidate(&shared)?;
        let mut store = self.store.lock();
        if self.closing.load(Ordering::Acquire) || cancelled() {
            return Err("monitor_cancelled".into());
        }
        let store = &mut *store;
        let view = store.views.get_mut(&shared.id).ok_or("view_closed")?;
        if view.frozen {
            return Err("monitor_frozen".into());
        }
        let summary = snapshot.summary();
        let id = snapshot.id.clone();
        let replaced = if view.display.is_none() {
            view.display = Some(id.clone());
            None
        } else {
            view.candidate.replace(id.clone())
        };
        store.snapshots.insert(id, Held { snapshot, view_refs: 1, pins: 0 });
        if let Some(replaced) = replaced {
            store.release_view_ref(&replaced);
        }
        Ok(summary)
    }

    /// Filter the whole snapshot, sort, then page. Only snapshots held by a live view are readable; it never samples.
    pub fn read(&self, snapshot_id: &str, query: &HostMonitorQuery) -> Result<HostMonitorPage, String> {
        let snapshot = {
            let store = self.store.lock();
            store
                .snapshots
                .get(snapshot_id)
                .filter(|held| held.view_refs > 0)
                .map(|held| held.snapshot.clone())
                .ok_or("snapshot_expired")?
        };
        Ok(snapshot.query(query))
    }

    /// Idempotently cancel a view. Its in-flight pass is never published; explanation pins are untouched.
    pub fn close(&self, view_id: &str) {
        self.store.lock().drop_view(view_id);
    }

    /// Cancel every view and refuse new work. Owned explanation state is released last.
    pub fn shutdown(&self) {
        self.closing.store(true, Ordering::Release);
        {
            let mut store = self.store.lock();
            let ids: Vec<String> = store.views.keys().cloned().collect();
            for id in ids {
                store.drop_view(&id);
            }
        }
        self.explanations.shutdown();
    }

    /// One lock: `snapshot_id` must be shown by a live view (its acknowledged display, or the pending candidate the
    /// pane is showing, which is acknowledged now). Pins it under `request_id` and freezes that view so no collection
    /// can replace it. Idempotent per request id.
    fn pin_displayed(&self, snapshot_id: &str, request_id: &str) -> Result<PinnedSnapshot, String> {
        let mut guard = self.store.lock();
        let store = &mut *guard;
        if let Some(existing) = store.pins.get(request_id) {
            if existing != snapshot_id {
                return Err("request_pin_conflict".into());
            }
            let held = store.snapshots.get(snapshot_id).ok_or("snapshot_expired")?;
            return Ok(PinnedSnapshot::of(&held.snapshot));
        }
        let view_id = store.live_view_showing(snapshot_id, true).ok_or("snapshot_not_displayed")?;
        store.promote_candidate_if(&view_id, snapshot_id);
        if let Some(view) = store.views.get_mut(&view_id) {
            view.frozen = true;
        }
        let held = store.snapshots.get_mut(snapshot_id).ok_or("snapshot_expired")?;
        held.pins += 1;
        let pinned = PinnedSnapshot::of(&held.snapshot);
        store.pins.insert(request_id.to_string(), snapshot_id.to_string());
        Ok(pinned)
    }

    /// Drop only this request's pin; the view stays frozen until `unfreeze_view`.
    fn release_pin(&self, request_id: &str) {
        let mut store = self.store.lock();
        if let Some(snapshot_id) = store.pins.remove(request_id) {
            if let Some(held) = store.snapshots.get_mut(&snapshot_id) {
                held.pins = held.pins.saturating_sub(1);
            }
            store.collect_garbage(&snapshot_id);
        }
    }

    /// A live view still displays `snapshot_id` and is frozen, so a suspended continuation is still valid.
    fn view_frozen(&self, snapshot_id: &str) -> bool {
        let store = self.store.lock();
        store.views.values().any(|view| {
            !view.shared.cancel.load(Ordering::Acquire) && view.frozen && view.display.as_deref() == Some(snapshot_id)
        })
    }

    /// Thaw the live view displaying `snapshot_id` unless a pin still holds it.
    fn unfreeze_view(&self, snapshot_id: &str) -> bool {
        let mut store = self.store.lock();
        if store.snapshots.get(snapshot_id).is_some_and(|held| held.pins > 0) {
            return false;
        }
        let found = store.views.values_mut().find(|view| {
            !view.shared.cancel.load(Ordering::Acquire) && view.display.as_deref() == Some(snapshot_id)
        });
        match found {
            Some(view) => {
                view.frozen = false;
                true
            }
            None => false,
        }
    }

    /// Read a pinned snapshot with the same filter/sort/page semantics as `read`; works after the view closed.
    fn read_pinned(&self, request_id: &str, snapshot_id: &str, query: HostMonitorQuery) -> Result<Value, String> {
        let snapshot = {
            let store = self.store.lock();
            if store.pins.get(request_id).map(String::as_str) != Some(snapshot_id) {
                return Err("snapshot_expired".into());
            }
            store.snapshots.get(snapshot_id).map(|held| held.snapshot.clone()).ok_or("snapshot_expired")?
        };
        serde_json::to_value(snapshot.query(&query)).map_err(|error| error.to_string())
    }
}
