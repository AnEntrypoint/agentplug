use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use wasmtime::{Engine, Linker, Module, Store};

use crate::host_state::{HostState, SiblingHandle};
use crate::imports::{register_env_imports, register_wasi};

pub const PLUGIN_IDLE_EVICT_MS: u64 = 30 * 60 * 1000;

pub const EPOCH_TICK_INTERVAL_MS: u64 = 1_000;

pub fn epoch_ticks_for_seconds(secs: u64) -> u64 {
    (secs * 1000).div_ceil(EPOCH_TICK_INTERVAL_MS)
}

pub const DISPATCH_CALL_DEADLINE_SECS: u64 = 120;

pub const BERT_DISPATCH_CALL_DEADLINE_SECS: u64 = 1200;

fn dispatch_call_deadline_secs(plugin_name: &str) -> u64 {
    if plugin_name == "bert" {
        BERT_DISPATCH_CALL_DEADLINE_SECS
    } else {
        DISPATCH_CALL_DEADLINE_SECS
    }
}

const CALLER_SUPPLIED_DEADLINE_CEILING_SECS: u64 = 3600;

fn caller_supplied_deadline_secs(body: &str) -> Option<u64> {
    if !body.contains("deadline_secs") {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("deadline_secs").and_then(|d| d.as_u64()))
        .filter(|s| *s > 0)
        .map(|s| s.min(CALLER_SUPPLIED_DEADLINE_CEILING_SECS))
}

fn deadline_secs_for_call(plugin_name: &str, body: &str) -> u64 {
    caller_supplied_deadline_secs(body).unwrap_or_else(|| dispatch_call_deadline_secs(plugin_name))
}

pub const RELEASABLE_SHARED_PLUGINS: [&str; 3] = ["bert", "treesitter", "gm"];

const STATELESS_SHARED_PLUGIN_NAMES: [&str; 3] = ["bert", "treesitter", "gm"];

fn is_stateless_shared_plugin(plugin_name: &str) -> bool {
    STATELESS_SHARED_PLUGIN_NAMES.contains(&plugin_name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginFiberLifecycle {
    Inactive,
    Active,
    Unloading,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PluginFiberState {
    state: PluginFiberLifecycle,
    #[serde(default)]
    content_hash: Option<String>,
}

fn plugin_fiber_state_path(plugin_name: &str) -> PathBuf {
    crate::install::install_dir()
        .join("plugins")
        .join(format!("{plugin_name}.fiber-state.json"))
}

fn read_plugin_fiber_state(plugin_name: &str) -> PluginFiberState {
    std::fs::read_to_string(plugin_fiber_state_path(plugin_name))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(PluginFiberState {
            state: PluginFiberLifecycle::Inactive,
            content_hash: None,
        })
}

fn write_plugin_fiber_state(
    plugin_name: &str,
    state: PluginFiberLifecycle,
    content_hash: Option<String>,
) {
    let body = PluginFiberState {
        state,
        content_hash,
    };
    if let Ok(text) = serde_json::to_string(&body) {
        let _ = std::fs::write(plugin_fiber_state_path(plugin_name), text);
    }
}

pub fn advance_plugin_fiber(plugin_name: &str, load_succeeded: bool, content_hash: Option<&str>) {
    let current = read_plugin_fiber_state(plugin_name).state;
    let next = match (current, load_succeeded) {
        (PluginFiberLifecycle::Inactive, true) => PluginFiberLifecycle::Active,
        (PluginFiberLifecycle::Inactive, false) => PluginFiberLifecycle::Inactive,
        (PluginFiberLifecycle::Active, true) => PluginFiberLifecycle::Active,
        (PluginFiberLifecycle::Active, false) => PluginFiberLifecycle::Unloading,
        (PluginFiberLifecycle::Unloading, _) => PluginFiberLifecycle::Inactive,
    };
    write_plugin_fiber_state(plugin_name, next, content_hash.map(|s| s.to_string()));
}

pub fn read_plugin_lifecycle(plugin_name: &str) -> PluginFiberLifecycle {
    read_plugin_fiber_state(plugin_name).state
}

#[derive(Debug)]
pub enum PluginDispatchError {
    NotRegistered { plugin_name: String },
    EvictedOrPoisoned { plugin_name: String },
    AdmissionStarved { kind: &'static str, waited_ms: u64, limit: usize, in_flight: usize, slots: usize },
}

impl std::fmt::Display for PluginDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginDispatchError::NotRegistered { plugin_name } => {
                write!(f, "plugin {plugin_name} is not registered for this project (no plugin pool exists -- check .agentplug/plugins.txt and daemon startup logs for a compile/install failure)")
            }
            PluginDispatchError::EvictedOrPoisoned { plugin_name } => {
                write!(f, "plugin {plugin_name} slot was evicted after a prior dispatch error (poisoned Store) and could not be reinstantiated -- retry will attempt to reload it")
            }
            PluginDispatchError::AdmissionStarved { kind, waited_ms, limit, in_flight, slots } => {
                write!(f, "the {kind} admission gate was not entered within {waited_ms}ms -- {in_flight} dispatches already hold the {limit} admitted slots of {slots}; this dispatch was NOT executed and re-dispatching is safe")
            }
        }
    }
}

impl std::error::Error for PluginDispatchError {}

fn admission_starved_error(report: AdmissionWaitReport) -> PluginDispatchError {
    PluginDispatchError::AdmissionStarved {
        kind: report.kind,
        waited_ms: report.waited_ms,
        limit: report.limit,
        in_flight: report.in_flight,
        slots: report.slots,
    }
}

fn log_poisoned_store_eviction_event(
    root: &Path,
    plugin_name: &str,
    verb: &str,
    reinstantiation_succeeded: bool,
    prior_dispatch_error: &str,
) {
    let log_path = root.join(".gm").join("exec-spool").join(".watcher.log");
    let Some(parent) = log_path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    else {
        return;
    };
    use std::io::Write;
    let line = serde_json::json!({
        "event": "plugin_poisoned_store_evicted",
        "plugin": plugin_name,
        "verb": verb,
        "reinstantiation_succeeded": reinstantiation_succeeded,
        "prior_dispatch_error": prior_dispatch_error,
        "ts": crate::now_ms(),
    });
    let _ = writeln!(f, "evt: {line}");
}

static GM_POOL_SIZE: OnceLock<usize> = OnceLock::new();

pub fn set_gm_pool_size(n: usize) -> bool {
    GM_POOL_SIZE.set(n.max(1)).is_ok()
}

fn gm_pool_size() -> usize {
    *GM_POOL_SIZE.get_or_init(|| 4)
}

static SIDE_PLUGIN_POOL_SIZE: OnceLock<usize> = OnceLock::new();

pub fn set_side_plugin_pool_size(n: usize) -> bool {
    SIDE_PLUGIN_POOL_SIZE.set(n.max(1)).is_ok()
}

fn side_plugin_pool_size() -> usize {
    *SIDE_PLUGIN_POOL_SIZE.get_or_init(|| 1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchCostClass {
    Cheap,
    Heavy,
}

const HEAVY_DISPATCH_VERBS: &[&str] = &[
    "code_index",
    "embed",
    "health",
    "index",
    "memorize",
    "memorize-fire",
    "memorize-prune",
    "recall",
    "scan_deps",
    "background-convert",
    "bert",
    "libsql",
];

pub fn cost_class_for_verb(verb: &str) -> DispatchCostClass {
    if HEAVY_DISPATCH_VERBS.contains(&verb) {
        DispatchCostClass::Heavy
    } else {
        DispatchCostClass::Cheap
    }
}

pub fn cost_class_for_dispatch(verb: &str, body: &str) -> DispatchCostClass {
    if verb != "codesearch" {
        return cost_class_for_verb(verb);
    }
    let mode = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    match mode.as_deref() {
        None | Some("dual") => DispatchCostClass::Heavy,
        Some(_) => DispatchCostClass::Cheap,
    }
}

fn try_lock_slot_recovering_from_poison(
    slot: &Mutex<Option<SiblingHandle>>,
) -> Option<std::sync::MutexGuard<'_, Option<SiblingHandle>>> {
    match slot.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlotContentSnapshot {
    Empty,
    Loaded { content_hash: String },
    BusyWithDispatchInFlight,
}

pub struct SharedPluginPool {
    plugin_name: String,
    slots: Vec<Arc<Mutex<Option<SiblingHandle>>>>,
    last_observed_slot_hashes: Mutex<Vec<Option<String>>>,
    hashes_to_evict_when_their_in_flight_dispatch_completes:
        Mutex<std::collections::HashSet<String>>,
    ticket_queue: Mutex<TicketQueue>,
    slot_released: Condvar,
}

struct ClassTicketQueue {
    next_ticket: u64,
    now_serving: u64,
}

impl ClassTicketQueue {
    fn waiting(&self) -> u64 {
        self.next_ticket.saturating_sub(self.now_serving)
    }
}

struct TicketQueue {
    cheap: ClassTicketQueue,
    heavy: ClassTicketQueue,
    heavy_inflight: usize,
    blocking_inflight: usize,
}

impl TicketQueue {
    fn class(&mut self, class: DispatchCostClass) -> &mut ClassTicketQueue {
        match class {
            DispatchCostClass::Cheap => &mut self.cheap,
            DispatchCostClass::Heavy => &mut self.heavy,
        }
    }
}

pub struct HeavyDispatchAdmission {
    pool: Option<Arc<SharedPluginPool>>,
}

impl Drop for HeavyDispatchAdmission {
    fn drop(&mut self) {
        let Some(pool) = self.pool.take() else { return };
        {
            let mut q = pool.ticket_queue.lock().unwrap_or_else(|e| e.into_inner());
            q.heavy_inflight = q.heavy_inflight.saturating_sub(1);
        }
        pool.slot_released.notify_all();
    }
}

pub struct BlockingDispatchAdmission {
    pool: Option<Arc<SharedPluginPool>>,
}

impl Drop for BlockingDispatchAdmission {
    fn drop(&mut self) {
        let Some(pool) = self.pool.take() else { return };
        {
            let mut q = pool.ticket_queue.lock().unwrap_or_else(|e| e.into_inner());
            q.blocking_inflight = q.blocking_inflight.saturating_sub(1);
        }
        pool.slot_released.notify_all();
    }
}

const ADMISSION_WAIT_MAX_MS_DEFAULT: u64 = 120_000;

fn env_u64_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

pub fn admission_wait_max() -> Duration {
    Duration::from_millis(env_u64_or("AGENTPLUG_ADMISSION_WAIT_MAX_MS", ADMISSION_WAIT_MAX_MS_DEFAULT))
}

pub struct AdmissionWaitReport {
    pub kind: &'static str,
    pub waited_ms: u64,
    pub limit: usize,
    pub in_flight: usize,
    pub slots: usize,
}

#[derive(Clone, Copy)]
pub struct AdmissionWaitState {
    pub kind: &'static str,
    pub since_ms: u64,
    pub limit: usize,
    pub in_flight: usize,
}

fn admission_waits_by_thread() -> &'static Mutex<HashMap<std::thread::ThreadId, AdmissionWaitState>> {
    static WAITS: OnceLock<Mutex<HashMap<std::thread::ThreadId, AdmissionWaitState>>> = OnceLock::new();
    WAITS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn admission_wait_state_for_thread(thread: std::thread::ThreadId) -> Option<AdmissionWaitState> {
    admission_waits_by_thread()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&thread)
        .copied()
}

fn mark_admission_wait(kind: &'static str, limit: usize, in_flight: usize) {
    let mut waits = admission_waits_by_thread().lock().unwrap_or_else(|e| e.into_inner());
    let thread = std::thread::current().id();
    let since_ms = waits.get(&thread).map(|state| state.since_ms).unwrap_or_else(crate::now_ms);
    waits.insert(thread, AdmissionWaitState { kind, since_ms, limit, in_flight });
}

fn clear_admission_wait() {
    admission_waits_by_thread()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&std::thread::current().id());
}

impl SharedPluginPool {
    pub fn new(plugin_name: &str, size: usize) -> Self {
        let size = size.max(1);
        Self {
            plugin_name: plugin_name.to_string(),
            slots: (0..size).map(|_| Arc::new(Mutex::new(None))).collect(),
            last_observed_slot_hashes: Mutex::new(vec![None; size]),
            hashes_to_evict_when_their_in_flight_dispatch_completes: Mutex::new(
                std::collections::HashSet::new(),
            ),
            ticket_queue: Mutex::new(TicketQueue {
                cheap: ClassTicketQueue {
                    next_ticket: 0,
                    now_serving: 0,
                },
                heavy: ClassTicketQueue {
                    next_ticket: 0,
                    now_serving: 0,
                },
                heavy_inflight: 0,
                blocking_inflight: 0,
            }),
            slot_released: Condvar::new(),
        }
    }

    pub const ACQUIRE_TIMEOUT_MS: u64 = 60_000;

    fn heavy_admission_limit(&self) -> usize {
        self.slots
            .len()
            .saturating_sub(1)
            .min(MAX_CONCURRENT_HEAVY_DISPATCHES)
            .max(1)
    }

    fn blocking_admission_limit(&self) -> usize {
        let reserved_for_short_verbs = (self.slots.len() / 4).max(1);
        self.slots
            .len()
            .saturating_sub(reserved_for_short_verbs)
            .max(1)
    }

    pub fn admit_blocking(pool: &Arc<SharedPluginPool>, verb: &str) -> BlockingDispatchAdmission {
        Self::admit_blocking_within(pool, verb, Duration::from_secs(86_400)).unwrap_or(BlockingDispatchAdmission { pool: None })
    }

    pub fn admit_blocking_within(pool: &Arc<SharedPluginPool>, verb: &str, max_wait: Duration) -> Result<BlockingDispatchAdmission, AdmissionWaitReport> {
        if !is_blocking_dispatch_verb(verb) || pool.slots.len() < 2 {
            return Ok(BlockingDispatchAdmission { pool: None });
        }
        let limit = pool.blocking_admission_limit();
        let start = Instant::now();
        loop {
            {
                let mut q = pool.ticket_queue.lock().unwrap_or_else(|e| e.into_inner());
                if q.blocking_inflight < limit {
                    q.blocking_inflight += 1;
                    clear_admission_wait();
                    return Ok(BlockingDispatchAdmission {
                        pool: Some(pool.clone()),
                    });
                }
            }
            let waited_ms = start.elapsed().as_millis() as u64;
            let in_flight = pool.ticket_queue.lock().unwrap_or_else(|e| e.into_inner()).blocking_inflight;
            if start.elapsed() >= max_wait {
                clear_admission_wait();
                return Err(AdmissionWaitReport { kind: "blocking", waited_ms, limit, in_flight, slots: pool.slots.len() });
            }
            mark_admission_wait("blocking", limit, in_flight);
            let guard = pool.ticket_queue.lock().unwrap_or_else(|e| e.into_inner());
            let _ = pool
                .slot_released
                .wait_timeout(guard, std::time::Duration::from_millis(25))
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    pub fn admit(pool: &Arc<SharedPluginPool>, class: DispatchCostClass) -> HeavyDispatchAdmission {
        Self::admit_within(pool, class, Duration::from_secs(86_400)).unwrap_or(HeavyDispatchAdmission { pool: None })
    }

    pub fn admit_within(pool: &Arc<SharedPluginPool>, class: DispatchCostClass, max_wait: Duration) -> Result<HeavyDispatchAdmission, AdmissionWaitReport> {
        if class != DispatchCostClass::Heavy || pool.slots.len() < 2 {
            return Ok(HeavyDispatchAdmission { pool: None });
        }
        let limit = pool.heavy_admission_limit();
        let start = Instant::now();
        let mut logged_at_ms = 0u64;
        loop {
            {
                let mut q = pool.ticket_queue.lock().unwrap_or_else(|e| e.into_inner());
                if q.heavy_inflight < limit {
                    q.heavy_inflight += 1;
                    clear_admission_wait();
                    return Ok(HeavyDispatchAdmission {
                        pool: Some(pool.clone()),
                    });
                }
            }
            let waited_ms = start.elapsed().as_millis() as u64;
            let in_flight = pool.ticket_queue.lock().unwrap_or_else(|e| e.into_inner()).heavy_inflight;
            if start.elapsed() >= max_wait {
                clear_admission_wait();
                return Err(AdmissionWaitReport { kind: "heavy", waited_ms, limit, in_flight, slots: pool.slots.len() });
            }
            mark_admission_wait("heavy", limit, in_flight);
            if waited_ms > Self::ACQUIRE_TIMEOUT_MS && waited_ms.saturating_sub(logged_at_ms) >= 5_000 {
                logged_at_ms = waited_ms;
                eprintln!(
                    "[agentplug registry] {} heavy-dispatch admission waiting {waited_ms}ms -- {limit} of {} slots already hold heavy work, one slot stays reserved for cheap verbs",
                    pool.plugin_name,
                    pool.slots.len()
                );
            }
            let guard = pool.ticket_queue.lock().unwrap_or_else(|e| e.into_inner());
            let _ = pool
                .slot_released
                .wait_timeout(guard, std::time::Duration::from_millis(25))
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    pub fn acquire(&self) -> Option<std::sync::MutexGuard<'_, Option<SiblingHandle>>> {
        Some(self.acquire_within(Self::ACQUIRE_TIMEOUT_MS).0)
    }

    pub fn acquire_within(
        &self,
        timeout_ms: u64,
    ) -> (std::sync::MutexGuard<'_, Option<SiblingHandle>>, u64) {
        self.acquire_within_for_class(timeout_ms, DispatchCostClass::Cheap)
    }

    pub fn acquire_within_for_class(
        &self,
        timeout_ms: u64,
        class: DispatchCostClass,
    ) -> (std::sync::MutexGuard<'_, Option<SiblingHandle>>, u64) {
        let start = std::time::Instant::now();
        let my_ticket = {
            let mut q = self.ticket_queue.lock().unwrap_or_else(|e| e.into_inner());
            let queue = q.class(class);
            let t = queue.next_ticket;
            queue.next_ticket += 1;
            t
        };
        loop {
            {
                let mut q = self.ticket_queue.lock().unwrap_or_else(|e| e.into_inner());
                if q.class(class).now_serving == my_ticket {
                    for slot in &self.slots {
                        if let Some(guard) = try_lock_slot_recovering_from_poison(slot) {
                            q.class(class).now_serving += 1;
                            drop(q);
                            self.slot_released.notify_all();
                            return (guard, start.elapsed().as_millis() as u64);
                        }
                    }
                }
            }
            let guard = self.ticket_queue.lock().unwrap_or_else(|e| e.into_inner());
            let _ = self
                .slot_released
                .wait_timeout(guard, std::time::Duration::from_millis(25))
                .unwrap_or_else(|e| e.into_inner());
            let waited = start.elapsed().as_millis() as u64;
            if waited > timeout_ms && waited % 5_000 < 30 {
                let (cheap_waiting, heavy_waiting, heavy_inflight) = {
                    let q = self.ticket_queue.lock().unwrap_or_else(|e| e.into_inner());
                    (q.cheap.waiting(), q.heavy.waiting(), q.heavy_inflight)
                };
                eprintln!(
                    "[agentplug registry] pool wait exceeded diagnostic threshold ({waited}ms > {timeout_ms}ms) -- still waiting, plugin={} class={class:?} ticket #{my_ticket}, slots={} heavy_inflight={heavy_inflight} waiting(cheap={cheap_waiting} heavy={heavy_waiting}), not denying",
                    self.plugin_name,
                    self.slots.len()
                );
            }
        }
    }

    pub fn size(&self) -> usize {
        self.slots.len()
    }

    fn all_instantiated(&self) -> bool {
        self.slots
            .iter()
            .all(|s| match try_lock_slot_recovering_from_poison(s) {
                Some(g) => g.is_some(),
                None => true,
            })
    }

    fn any_instantiated_without_blocking(&self) -> bool {
        self.slots.iter().any(|slot| {
            try_lock_slot_recovering_from_poison(slot)
                .map(|guard| guard.is_some())
                .unwrap_or(true)
        })
    }

    pub(crate) fn slots_for_fill(&self) -> &[Arc<Mutex<Option<SiblingHandle>>>] {
        &self.slots
    }

    pub fn slot_snapshot_without_blocking(&self) -> Vec<SlotContentSnapshot> {
        self.slots
            .iter()
            .map(|s| match try_lock_slot_recovering_from_poison(s) {
                Some(guard) => match guard.as_ref() {
                    Some(handle) => SlotContentSnapshot::Loaded {
                        content_hash: handle.content_hash.clone(),
                    },
                    None => SlotContentSnapshot::Empty,
                },
                None => SlotContentSnapshot::BusyWithDispatchInFlight,
            })
            .collect()
    }

    pub fn slot_content_hashes(&self) -> Vec<Option<String>> {
        let mut observed = self
            .last_observed_slot_hashes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (index, slot) in self.slots.iter().enumerate() {
            if let Ok(guard) = slot.try_lock() {
                observed[index] = guard.as_ref().map(|h| h.content_hash.clone());
            }
        }
        observed.clone()
    }

    pub(crate) fn any_instantiated_within(&self, timeout_ms: u64) -> bool {
        const POLL_INTERVAL_MS: u64 = 25;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            for slot in &self.slots {
                if let Some(guard) = try_lock_slot_recovering_from_poison(slot) {
                    if guard.is_some() {
                        return true;
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                return self
                    .slots
                    .iter()
                    .any(|s| s.lock().unwrap_or_else(|e| e.into_inner()).is_some());
            }
            std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
        }
    }

    fn evict_every_currently_free_slot_without_blocking_on_busy_ones(&self) -> bool {
        let mut released = false;
        for slot in &self.slots {
            if let Some(mut guard) = try_lock_slot_recovering_from_poison(slot) {
                if guard.is_some() {
                    *guard = None;
                    released = true;
                }
            }
        }
        released
    }

    pub fn request_store_swap(&self, old_hash: &str) -> (usize, usize) {
        let mut evicted = 0usize;
        let mut deferred = 0usize;
        for slot in &self.slots {
            match try_lock_slot_recovering_from_poison(slot) {
                Some(mut guard) => {
                    if guard.as_ref().is_some_and(|h| h.content_hash == old_hash) {
                        *guard = None;
                        evicted += 1;
                    }
                }
                None => deferred += 1,
            }
        }
        if deferred > 0 {
            self.hashes_to_evict_when_their_in_flight_dispatch_completes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(old_hash.to_string());
        }
        (evicted, deferred)
    }

    pub fn evict_if_swap_pending(
        &self,
        guard: &mut std::sync::MutexGuard<'_, Option<SiblingHandle>>,
    ) {
        let Some(handle) = guard.as_ref() else { return };
        let pending = self
            .hashes_to_evict_when_their_in_flight_dispatch_completes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if pending.contains(&handle.content_hash) {
            drop(pending);
            **guard = None;
        }
    }

    pub fn note_bytes_current(&self, hash: &str) {
        self.hashes_to_evict_when_their_in_flight_dispatch_completes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(hash);
    }

    pub fn swap_pending_hashes(&self) -> Vec<String> {
        self.hashes_to_evict_when_their_in_flight_dispatch_completes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

type SharedPluginMap = Mutex<HashMap<String, Arc<SharedPluginPool>>>;
static SHARED_PLUGINS: OnceLock<SharedPluginMap> = OnceLock::new();

fn shared_plugin_pool(plugin_name: &str) -> Arc<SharedPluginPool> {
    let pool_size = if plugin_name == "gm" {
        gm_pool_size()
    } else {
        side_plugin_pool_size()
    };
    SHARED_PLUGINS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .entry(plugin_name.to_string())
        .or_insert_with(|| Arc::new(SharedPluginPool::new(plugin_name, pool_size)))
        .clone()
}

pub fn release_shared_plugin(plugin_name: &str) -> bool {
    if !is_stateless_shared_plugin(plugin_name) {
        return false;
    }
    shared_plugin_pool(plugin_name).evict_every_currently_free_slot_without_blocking_on_busy_ones()
}

pub fn request_shared_store_swap(plugin_name: &str, old_hash: &str) -> (usize, usize) {
    if !is_stateless_shared_plugin(plugin_name) {
        return (0, 0);
    }
    shared_plugin_pool(plugin_name).request_store_swap(old_hash)
}

pub fn note_shared_plugin_bytes_current(plugin_name: &str, hash: &str) {
    if !is_stateless_shared_plugin(plugin_name) {
        return;
    }
    shared_plugin_pool(plugin_name).note_bytes_current(hash);
}

pub fn shared_plugin_swap_pending_hashes(plugin_name: &str) -> Vec<String> {
    SHARED_PLUGINS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .get(plugin_name)
        .map(|pool| pool.swap_pending_hashes())
        .unwrap_or_default()
}

pub fn shared_plugin_slot_content_hashes(plugin_name: &str) -> Vec<Option<String>> {
    SHARED_PLUGINS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(plugin_name)
        .map(|pool| pool.slot_content_hashes())
        .unwrap_or_default()
}

pub fn shared_plugin_slot_snapshot_without_blocking(plugin_name: &str) -> Vec<SlotContentSnapshot> {
    SHARED_PLUGINS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(plugin_name)
        .map(|pool| pool.slot_snapshot_without_blocking())
        .unwrap_or_default()
}

pub fn get_active_provider(plugin_name: &str) -> Option<String> {
    shared_plugin_slot_content_hashes(plugin_name)
        .into_iter()
        .flatten()
        .next()
}

pub type SiblingPools = Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>;

pub type SiblingReloadSource = (Engine, HashMap<String, (Module, String)>);

static SIBLING_RELOAD_SOURCE: OnceLock<Mutex<Option<Arc<SiblingReloadSource>>>> = OnceLock::new();

pub fn set_sibling_reload_source(source: SiblingReloadSource) {
    let slot = SIBLING_RELOAD_SOURCE.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(source));
}

pub fn ensure_sibling_registered(root: &Path, plugin_name: &str, siblings: &SiblingPools) -> bool {
    if siblings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(plugin_name)
    {
        return true;
    }
    let source = {
        let slot = SIBLING_RELOAD_SOURCE.get_or_init(|| Mutex::new(None));
        slot.lock().unwrap_or_else(|e| e.into_inner()).clone()
    };
    let Some(source) = source else { return false };
    let handle = DispatchHandle {
        root: root.to_path_buf(),
        siblings: siblings.clone(),
        reload_source: Some(source.as_ref().clone()),
    };
    let _ = handle.reinstantiate_plugin_into_pool_slot_if_reload_source_available(plugin_name);
    siblings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(plugin_name)
}

fn resolve_routed_plugin_name(plugin_name: &str) -> (String, Option<crate::broker::RouteLease>) {
    match crate::broker::route(plugin_name) {
        Some(lease) => {
            let routed = lease.provider_id.clone();
            if routed != plugin_name {
                eprintln!("[agentplug registry] broker routed service_key={plugin_name} to provider_id={routed}");
            }
            (routed, Some(lease))
        }
        None => (plugin_name.to_string(), None),
    }
}

pub fn dispatch_on(
    store: &mut Store<HostState>,
    instance: wasmtime::Instance,
    verb: &str,
    body: &str,
    caller_root: &Path,
    caller_siblings: Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>,
) -> anyhow::Result<String> {
    store.data().set_cwd(caller_root.to_path_buf());
    store.data().set_siblings(caller_siblings);
    let _ = store.data().take_lost_response();
    let plugin_name = store.data().plugin_name.clone();
    if is_stateless_shared_plugin(&plugin_name) {
        crate::memory_pressure::note_shared_plugin_dispatch();
    }
    let call_deadline_secs = deadline_secs_for_call(&plugin_name, body);
    store.data().set_call_deadline_secs(call_deadline_secs);
    store.set_epoch_deadline(epoch_ticks_for_seconds(call_deadline_secs));
    let alloc = instance.get_typed_func::<u32, u32>(&mut *store, "plugkit_alloc")?;
    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} has no exported memory"))?;

    let verb_ptr = alloc.call(&mut *store, verb.len() as u32)?;
    memory.write(&mut *store, verb_ptr as usize, verb.as_bytes())?;
    let body_ptr = alloc.call(&mut *store, body.len() as u32)?;
    memory.write(&mut *store, body_ptr as usize, body.as_bytes())?;
    let free = instance
        .get_typed_func::<(u32, u32), ()>(&mut *store, "plugkit_free")
        .ok();
    let free_call_args = |store: &mut Store<HostState>| {
        if let Some(free) = &free {
            let _ = free.call(&mut *store, (verb_ptr, verb.len() as u32));
            let _ = free.call(&mut *store, (body_ptr, body.len() as u32));
        }
    };

    let dispatch_fn = instance
        .get_typed_func::<(u32, u32, u32, u32), u64>(&mut *store, "plugin_call")
        .or_else(|_| {
            instance.get_typed_func::<(u32, u32, u32, u32), u64>(&mut *store, "dispatch_verb")
        })?;
    let call_result = dispatch_fn.call(
        &mut *store,
        (verb_ptr, verb.len() as u32, body_ptr, body.len() as u32),
    );
    let packed = match call_result {
        Ok(p) => {
            free_call_args(store);
            p
        }
        Err(e) => {
            if matches!(
                e.downcast_ref::<wasmtime::Trap>(),
                Some(wasmtime::Trap::Interrupt)
            ) {
                return Err(anyhow::anyhow!("plugin_call_deadline_exceeded: {plugin_name} exceeded {call_deadline_secs}s executing verb {verb}"));
            }
            return Err(e.into());
        }
    };

    let ptr = (packed & 0xffff_ffff) as u32;
    let len = (packed >> 32) as u32;
    if ptr == 0 || len == 0 {
        if let Some(reason) = store.data().take_lost_response() {
            return Err(anyhow::anyhow!(
                "plugin_response_lost: {plugin_name} verb {verb} produced a response that never reached the guest -- {reason}"
            ));
        }
        eprintln!(
            "[agentplug registry] plugin {plugin_name} verb {verb} returned a zero packed (ptr={ptr}, len={len}) with no recorded write failure -- treating it as a genuine empty response"
        );
        return Ok(String::new());
    }
    let mut buf = vec![0u8; len as usize];
    memory.read(&mut *store, ptr as usize, &mut buf)?;
    if let Ok(free) = instance.get_typed_func::<(u32, u32), ()>(&mut *store, "plugkit_free") {
        let _ = free.call(&mut *store, (ptr, len));
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn host_fs_root() -> PathBuf {
    #[cfg(windows)]
    {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("C:\\"));
        let mut root = cwd
            .components()
            .next()
            .map(|c| PathBuf::from(c.as_os_str()))
            .unwrap_or_else(|| PathBuf::from("C:\\"));
        if !root.to_string_lossy().ends_with('\\') {
            root = PathBuf::from(format!("{}\\", root.to_string_lossy()));
        }
        root
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/")
    }
}

fn instantiate_plugin(
    engine: &Engine,
    root: PathBuf,
    plugin_name: &str,
    module: &Module,
    content_hash: &str,
) -> anyhow::Result<SiblingHandle> {
    let mut linker: Linker<HostState> = Linker::new(engine);
    register_wasi(&mut linker)?;
    register_env_imports(&mut linker)?;

    let host_state = if plugin_name == "libsql" {
        HostState::new_with_fs_root(root, plugin_name.to_string(), &host_fs_root())
    } else {
        HostState::new(root, plugin_name.to_string())
    };
    let self_instance_cell = host_state.self_instance.clone();
    let mut store = Store::new(engine, host_state);
    store.set_epoch_deadline(epoch_ticks_for_seconds(DISPATCH_CALL_DEADLINE_SECS));
    let instance = linker.instantiate(&mut store, module)?;
    *self_instance_cell.lock().unwrap() = Some(instance);
    Ok(SiblingHandle {
        store,
        instance,
        content_hash: content_hash.to_string(),
    })
}

pub struct ProjectPlugins {
    pub root: PathBuf,
    siblings: Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>,
    pub last_active: Instant,
}

impl ProjectPlugins {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            siblings: Arc::new(Mutex::new(HashMap::new())),
            last_active: Instant::now(),
        }
    }

    pub fn is_loaded(&self, plugin_name: &str) -> bool {
        self.siblings
            .lock()
            .unwrap()
            .get(plugin_name)
            .map(|pool| {
                if is_stateless_shared_plugin(plugin_name) {
                    pool.any_instantiated_without_blocking()
                } else {
                    pool.all_instantiated()
                }
            })
            .unwrap_or(false)
    }

    pub fn is_loaded_current(&self, plugin_name: &str, content_hash: &str) -> bool {
        if is_stateless_shared_plugin(plugin_name) {
            return self.is_loaded(plugin_name);
        }
        self.siblings
            .lock()
            .unwrap()
            .get(plugin_name)
            .map(|p| {
                p.slot_content_hashes()
                    .iter()
                    .any(|h| h.as_deref() == Some(content_hash))
            })
            .unwrap_or(false)
    }

    pub fn load_plugin(
        &mut self,
        engine: &Engine,
        plugin_name: &str,
        module: &Module,
        content_hash: &str,
    ) -> anyhow::Result<()> {
        if is_stateless_shared_plugin(plugin_name) {
            let pool = shared_plugin_pool(plugin_name);
            let has_current = pool.slots_for_fill().iter().any(|slot| {
                try_lock_slot_recovering_from_poison(slot)
                    .map(|guard| {
                        guard
                            .as_ref()
                            .is_some_and(|handle| handle.content_hash == content_hash)
                    })
                    .unwrap_or(true)
            });
            if !has_current {
                for slot in pool.slots_for_fill() {
                    let Some(mut guard) = try_lock_slot_recovering_from_poison(slot) else {
                        continue;
                    };
                    if guard.is_none() {
                        *guard = Some(instantiate_plugin(
                            engine,
                            self.root.clone(),
                            plugin_name,
                            module,
                            content_hash,
                        )?);
                        break;
                    }
                }
            }
            self.siblings
                .lock()
                .unwrap()
                .insert(plugin_name.to_string(), pool);
            return Ok(());
        }

        let instantiated =
            instantiate_plugin(engine, self.root.clone(), plugin_name, module, content_hash)?;
        let pool = self
            .siblings
            .lock()
            .unwrap()
            .entry(plugin_name.to_string())
            .or_insert_with(|| Arc::new(SharedPluginPool::new(plugin_name, 1)))
            .clone();
        *pool
            .acquire()
            .expect("acquire() always returns Some -- FIFO wait never denies") = Some(instantiated);
        Ok(())
    }

    pub fn dispatch(
        &mut self,
        plugin_name: &str,
        verb: &str,
        body: &str,
    ) -> anyhow::Result<String> {
        self.last_active = Instant::now();
        let (routed_name, _route_lease) = resolve_routed_plugin_name(plugin_name);
        let plugin_name = routed_name.as_str();
        const DISPATCH_LOOKUP_RETRY_ATTEMPTS: u32 = 3;
        const DISPATCH_LOOKUP_RETRY_BACKOFF_MS: u64 = 200;
        let mut pool = None;
        for attempt in 0..DISPATCH_LOOKUP_RETRY_ATTEMPTS {
            pool = self.siblings.lock().unwrap().get(plugin_name).cloned();
            if pool.is_some() || attempt + 1 == DISPATCH_LOOKUP_RETRY_ATTEMPTS {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(
                DISPATCH_LOOKUP_RETRY_BACKOFF_MS,
            ));
        }
        let pool = pool.ok_or_else(|| PluginDispatchError::NotRegistered {
            plugin_name: plugin_name.to_string(),
        })?;
        let cost_class = cost_class_for_dispatch(verb, body);
        let _heavy_admission =
            SharedPluginPool::admit_within(&pool, cost_class, admission_wait_max())
                .map_err(admission_starved_error)?;
        let _blocking_admission =
            SharedPluginPool::admit_blocking_within(&pool, verb, admission_wait_max())
                .map_err(admission_starved_error)?;
        let (mut guard, _waited_ms) =
            pool.acquire_within_for_class(SharedPluginPool::ACQUIRE_TIMEOUT_MS, cost_class);
        dispatch_and_evict_on_error(
            &mut guard,
            &pool,
            verb,
            body,
            &self.root,
            &self.siblings,
            plugin_name,
        )
    }

    pub fn dispatch_handle_with_reload(
        &self,
        reload_source: Option<(Engine, HashMap<String, (Module, String)>)>,
    ) -> DispatchHandle {
        DispatchHandle {
            root: self.root.clone(),
            siblings: self.siblings.clone(),
            reload_source,
        }
    }

    pub fn dispatch_handle(&self) -> DispatchHandle {
        DispatchHandle {
            root: self.root.clone(),
            siblings: self.siblings.clone(),
            reload_source: None,
        }
    }
}

#[derive(Clone)]
pub struct DispatchHandle {
    root: PathBuf,
    siblings: Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>,
    reload_source: Option<(Engine, HashMap<String, (Module, String)>)>,
}

impl DispatchHandle {
    fn reinstantiate_plugin_into_pool_slot_if_reload_source_available(
        &self,
        plugin_name: &str,
    ) -> anyhow::Result<()> {
        let Some((engine, modules)) = self.reload_source.as_ref() else {
            eprintln!("[agentplug registry] reinstantiate skipped for {plugin_name}: this DispatchHandle has no reload_source attached at all (dispatch_handle() no-reload constructor)");
            return Ok(());
        };
        let Some((module, content_hash)) = modules.get(plugin_name) else {
            eprintln!("[agentplug registry] reinstantiate skipped for {plugin_name}: reload_source snapshot has {} plugin(s) ({:?}) but does not include {plugin_name}", modules.len(), modules.keys().collect::<Vec<_>>());
            return Ok(());
        };
        if is_stateless_shared_plugin(plugin_name) {
            let pool = shared_plugin_pool(plugin_name);
            {
                let mut guard = pool
                    .acquire()
                    .expect("acquire() always returns Some -- FIFO wait never denies");
                let needs_fill = match guard.as_ref() {
                    None => true,
                    Some(existing) => &existing.content_hash != content_hash,
                };
                if needs_fill {
                    *guard = Some(instantiate_plugin(
                        engine,
                        self.root.clone(),
                        plugin_name,
                        module,
                        content_hash,
                    )?);
                }
            }
            self.siblings
                .lock()
                .unwrap()
                .insert(plugin_name.to_string(), pool);
            return Ok(());
        }
        let instantiated =
            instantiate_plugin(engine, self.root.clone(), plugin_name, module, content_hash)?;
        let pool = self
            .siblings
            .lock()
            .unwrap()
            .entry(plugin_name.to_string())
            .or_insert_with(|| Arc::new(SharedPluginPool::new(plugin_name, 1)))
            .clone();
        *pool
            .acquire()
            .expect("acquire() always returns Some -- FIFO wait never denies") = Some(instantiated);
        Ok(())
    }

    pub fn dispatch(&self, plugin_name: &str, verb: &str, body: &str) -> anyhow::Result<String> {
        let (routed_name, _route_lease) = resolve_routed_plugin_name(plugin_name);
        let plugin_name = routed_name.as_str();
        const REGISTRATION_LOOKUP_RETRY_ATTEMPTS: u32 = 3;
        const REGISTRATION_LOOKUP_RETRY_BACKOFF_MS: u64 = 200;
        let mut pool = None;
        for attempt in 0..REGISTRATION_LOOKUP_RETRY_ATTEMPTS {
            pool = self.siblings.lock().unwrap().get(plugin_name).cloned();
            if pool.is_some() || attempt + 1 == REGISTRATION_LOOKUP_RETRY_ATTEMPTS {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(
                REGISTRATION_LOOKUP_RETRY_BACKOFF_MS,
            ));
        }
        if pool.is_none() {
            let _ =
                self.reinstantiate_plugin_into_pool_slot_if_reload_source_available(plugin_name);
            pool = self.siblings.lock().unwrap().get(plugin_name).cloned();
        }
        let pool = pool.ok_or_else(|| PluginDispatchError::NotRegistered {
            plugin_name: plugin_name.to_string(),
        })?;
        let cost_class = cost_class_for_dispatch(verb, body);
        let _heavy_admission =
            SharedPluginPool::admit_within(&pool, cost_class, admission_wait_max())
                .map_err(admission_starved_error)?;
        let _blocking_admission =
            SharedPluginPool::admit_blocking_within(&pool, verb, admission_wait_max())
                .map_err(admission_starved_error)?;
        let (mut guard, _waited_ms) =
            pool.acquire_within_for_class(SharedPluginPool::ACQUIRE_TIMEOUT_MS, cost_class);
        if guard.is_none() {
            drop(guard);
            const REINSTANTIATION_RETRY_ATTEMPTS: u32 = 3;
            const REINSTANTIATION_RETRY_BACKOFF_MS: u64 = 250;
            let mut last_reload_error: Option<String> = None;
            let mut refilled_pool: Option<Arc<SharedPluginPool>> = None;
            for attempt in 0..REINSTANTIATION_RETRY_ATTEMPTS {
                if let Err(e) =
                    self.reinstantiate_plugin_into_pool_slot_if_reload_source_available(plugin_name)
                {
                    last_reload_error = Some(format!("{e:#}"));
                }
                let candidate_pool = self
                    .siblings
                    .lock()
                    .unwrap()
                    .get(plugin_name)
                    .cloned()
                    .ok_or_else(|| PluginDispatchError::NotRegistered {
                        plugin_name: plugin_name.to_string(),
                    })?;
                let is_refilled = {
                    let (retry_guard, _retry_waited_ms) =
                        candidate_pool.acquire_within(SharedPluginPool::ACQUIRE_TIMEOUT_MS);
                    retry_guard.is_some()
                };
                if is_refilled {
                    refilled_pool = Some(candidate_pool);
                    break;
                }
                if attempt + 1 < REINSTANTIATION_RETRY_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(
                        REINSTANTIATION_RETRY_BACKOFF_MS,
                    ));
                }
            }
            let Some(refilled_pool) = refilled_pool else {
                let detail = last_reload_error.unwrap_or_else(|| {
                    "reload produced no error but no slot was repopulated".to_string()
                });
                eprintln!("[agentplug registry] plugin {plugin_name} could not be reinstantiated after a poisoned-Store eviction (verb {verb}) -- {detail}");
                log_poisoned_store_eviction_event(&self.root, plugin_name, verb, false, &format!("reinstantiation failed after {REINSTANTIATION_RETRY_ATTEMPTS} attempts: {detail}"));
                return Err(PluginDispatchError::EvictedOrPoisoned {
                    plugin_name: plugin_name.to_string(),
                }
                .into());
            };
            let (mut final_guard, _final_waited_ms) = refilled_pool
                .acquire_within_for_class(SharedPluginPool::ACQUIRE_TIMEOUT_MS, cost_class);
            return dispatch_and_evict_on_error(
                &mut final_guard,
                &refilled_pool,
                verb,
                body,
                &self.root,
                &self.siblings,
                plugin_name,
            );
        }
        dispatch_and_evict_on_error(
            &mut guard,
            &pool,
            verb,
            body,
            &self.root,
            &self.siblings,
            plugin_name,
        )
    }
}

fn dispatch_and_evict_on_error(
    guard: &mut std::sync::MutexGuard<'_, Option<SiblingHandle>>,
    pool: &Arc<SharedPluginPool>,
    verb: &str,
    body: &str,
    root: &Path,
    siblings: &Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>,
    plugin_name: &str,
) -> anyhow::Result<String> {
    let handle = guard.as_mut().ok_or_else(|| {
        eprintln!("[agentplug registry] plugin {plugin_name} slot empty at dispatch of verb {verb} -- previously evicted for a poisoned Store, reload did not repopulate it");
        log_poisoned_store_eviction_event(root, plugin_name, verb, false, "slot already empty from a prior eviction, reload did not repopulate it");
        PluginDispatchError::EvictedOrPoisoned { plugin_name: plugin_name.to_string() }
    })?;
    let result = dispatch_on(
        &mut handle.store,
        handle.instance,
        verb,
        body,
        root,
        siblings.clone(),
    );
    if let Err(poisoning_error) = &result {
        eprintln!("[agentplug registry] evicting plugin {plugin_name} slot -- verb {verb} poisoned its Store: {poisoning_error}");
        log_poisoned_store_eviction_event(
            root,
            plugin_name,
            verb,
            true,
            &poisoning_error.to_string(),
        );
        **guard = None;
    } else {
        pool.evict_if_swap_pending(guard);
    }
    result
}

static GM_INFLIGHT_BY_PROJECT: OnceLock<Mutex<HashMap<(PathBuf, &'static str), usize>>> =
    OnceLock::new();

static GM_PROJECT_STEP_RELEASED: OnceLock<Condvar> = OnceLock::new();

struct ToolQueue {
    next_ticket: u64,
    now_serving: u64,
    active: bool,
}

static TOOL_INFLIGHT: OnceLock<Mutex<HashMap<String, ToolQueue>>> = OnceLock::new();

static TOOL_STEP_RELEASED: OnceLock<Condvar> = OnceLock::new();

fn gm_inflight_map() -> &'static Mutex<HashMap<(PathBuf, &'static str), usize>> {
    GM_INFLIGHT_BY_PROJECT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn gm_project_step_released() -> &'static Condvar {
    GM_PROJECT_STEP_RELEASED.get_or_init(Condvar::new)
}

fn tool_inflight() -> &'static Mutex<HashMap<String, ToolQueue>> {
    TOOL_INFLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tool_step_released() -> &'static Condvar {
    TOOL_STEP_RELEASED.get_or_init(Condvar::new)
}

const MAX_CONCURRENT_HEAVY_DISPATCHES: usize = 3;

const UNSERIALIZED_VERBS: &[&str] = &[
    "exec_js",
    "lang",
    "nodejs",
    "javascript",
    "node",
    "js",
    "typescript",
    "python",
    "py",
    "bash",
    "sh",
    "shell",
    "zsh",
    "powershell",
    "ps1",
    "ssh",
    "go",
    "rust",
    "c",
    "cpp",
    "java",
    "deno",
    "serp",
    "browser",
    "cdp",
    "fetch",
    "callers",
    "callees",
    "impact",
    "fs_read",
    "fs_readdir",
    "fs_stat",
    "env_get",
    "kv_get",
    "config_resolve",
    "git_status",
    "branch_status",
    "git_log",
    "git_diff",
    "git_show",
    "ci-status",
    "git_poll",
    "status",
    "wait",
    "close",
    "phase-status",
    "prd-list",
    "prd-status",
    "mutable-list",
    "filter",
];

const BLOCKING_DISPATCH_VERBS: &[&str] = &[
    "exec_js",
    "lang",
    "nodejs",
    "javascript",
    "node",
    "js",
    "typescript",
    "python",
    "py",
    "bash",
    "sh",
    "shell",
    "zsh",
    "powershell",
    "ps1",
    "ssh",
    "go",
    "rust",
    "c",
    "cpp",
    "java",
    "deno",
    "serp",
    "browser",
    "cdp",
    "fetch",
    "wait",
];

pub fn is_blocking_dispatch_verb(verb: &str) -> bool {
    BLOCKING_DISPATCH_VERBS.contains(&verb)
}

const GIT_LANE_VERBS: &[&str] = &[
    "git_add",
    "git_commit",
    "git_finalize",
    "git_push",
    "git_pull",
    "git_fetch",
    "git_checkout",
    "git_merge",
    "git_merge_abort",
    "git_branch",
    "git_branch_delete",
    "git_rm",
    "git_revert",
    "git_reset",
    "git_stash",
    "git_stash_pop",
    "git_stash_drop",
    "git_stash_list",
];

const STORE_LANE_VERBS: &[&str] = &[
    "scan_deps",
    "health",
    "memorize",
    "memorize-fire",
    "memorize-prune",
    "memorize-vacuum",
    "memorize-retention",
    "recall",
    "forget",
    "codeinsight_index",
    "code_index",
    "embed",
    "index",
    "libsql",
    "bert",
    "tencentdb-compat-probe",
    "tencentdb-memory-import",
    "config-sync-now",
    "dataflow_resolve",
    "sql_open",
    "sql_close",
    "sql_list_dbs",
    "sql_exec",
    "sql_query",
    "sql_smoke",
    "sql_serialize",
    "sql_deserialize",
    "cache_get",
    "cache_put",
    "cache_invalidate",
    "cache_stats",
    "kv_put",
    "kv_query",
];

const TREE_SCAN_VERBS: &[&str] = &["grep", "codesearch"];

fn tree_scan_without_indexing(verb: &str, body: &str) -> bool {
    TREE_SCAN_VERBS.contains(&verb) && cost_class_for_dispatch(verb, body) == DispatchCostClass::Cheap
}

pub fn is_unserialized_dispatch(verb: &str, body: &str) -> bool {
    UNSERIALIZED_VERBS.contains(&verb) || tree_scan_without_indexing(verb, body)
}

fn serial_lane_for_dispatch(verb: &str, body: &str) -> Option<&'static str> {
    if is_unserialized_dispatch(verb, body) {
        None
    } else if GIT_LANE_VERBS.contains(&verb) {
        Some("git")
    } else if STORE_LANE_VERBS.contains(&verb) || verb == "codesearch" {
        Some("store")
    } else {
        Some("state")
    }
}

pub fn dispatch_serial_lane(verb: &str, body: &str) -> Option<&'static str> {
    serial_lane_for_dispatch(verb, body)
}

fn gm_lane_waiters() -> &'static Mutex<HashMap<(PathBuf, &'static str), usize>> {
    static WAITERS: OnceLock<Mutex<HashMap<(PathBuf, &'static str), usize>>> = OnceLock::new();
    WAITERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn gm_lane_holders() -> &'static Mutex<HashMap<(PathBuf, &'static str), String>> {
    static HOLDERS: OnceLock<Mutex<HashMap<(PathBuf, &'static str), String>>> = OnceLock::new();
    HOLDERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tool_queue_holders() -> &'static Mutex<HashMap<String, String>> {
    static HOLDERS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    HOLDERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct LaneWaitReport {
    pub lane: &'static str,
    pub waited_ms: u64,
    pub waiters: usize,
    pub lane_holder_task: Option<String>,
}

pub struct ToolQueueWaitReport {
    pub queue_key: String,
    pub waited_ms: u64,
    pub position: u64,
    pub holder_task: Option<String>,
}

pub struct GmFairnessGuard {
    root: PathBuf,
    lane: Option<&'static str>,
}

impl GmFairnessGuard {
    pub fn acquire(root: &Path, verb: &str, body: &str) -> Self {
        let root = root.to_path_buf();
        let Some(lane) = serial_lane_for_dispatch(verb, body) else {
            return Self { root, lane: None };
        };
        let mut map = gm_inflight_map().lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let count = map.entry((root.clone(), lane)).or_insert(0);
            if *count == 0 {
                *count = 1;
                return Self {
                    root,
                    lane: Some(lane),
                };
            }
            map = gm_project_step_released()
                .wait(map)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    pub fn acquire_within(root: &Path, verb: &str, body: &str, task: &str, max_wait: Duration) -> Result<Self, LaneWaitReport> {
        let owned_root = root.to_path_buf();
        let Some(lane) = serial_lane_for_dispatch(verb, body) else {
            return Ok(Self { root: owned_root, lane: None });
        };
        let started = Instant::now();
        {
            let mut waiters = gm_lane_waiters().lock().unwrap_or_else(|e| e.into_inner());
            *waiters.entry((owned_root.clone(), lane)).or_insert(0) += 1;
        }
        let mut map = gm_inflight_map().lock().unwrap_or_else(|e| e.into_inner());
        let acquired = loop {
            let count = map.entry((owned_root.clone(), lane)).or_insert(0);
            if *count == 0 {
                *count = 1;
                break true;
            }
            let remaining = max_wait.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break false;
            }
            let (next, timed_out) = gm_project_step_released()
                .wait_timeout(map, remaining)
                .unwrap_or_else(|e| e.into_inner());
            map = next;
            if timed_out.timed_out() {
                break false;
            }
        };
        drop(map);
        let waiters = {
            let mut waiters = gm_lane_waiters().lock().unwrap_or_else(|e| e.into_inner());
            let key = (owned_root.clone(), lane);
            let remaining = waiters.get(&key).copied().unwrap_or(0).saturating_sub(1);
            if remaining == 0 {
                waiters.remove(&key);
            } else {
                waiters.insert(key, remaining);
            }
            remaining
        };
        if !acquired {
            let lane_holder_task = gm_lane_holders()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(owned_root.clone(), lane))
                .cloned();
            gm_project_step_released().notify_all();
            return Err(LaneWaitReport {
                lane,
                waited_ms: started.elapsed().as_millis() as u64,
                waiters,
                lane_holder_task,
            });
        }
        gm_lane_holders()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((owned_root.clone(), lane), task.to_string());
        Ok(Self { root: owned_root, lane: Some(lane) })
    }
}

impl Drop for GmFairnessGuard {
    fn drop(&mut self) {
        let Some(lane) = self.lane else {
            return;
        };
        let key = (self.root.clone(), lane);
        gm_lane_holders().lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        let mut map = gm_inflight_map().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = map.get_mut(&key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(&key);
            }
        }
        drop(map);
        gm_project_step_released().notify_all();
    }
}

pub struct ToolDispatchGuard {
    key: Option<String>,
}

impl ToolDispatchGuard {
    pub fn acquire(plugin: &str, verb: &str, body: &str) -> Self {
        if is_unserialized_dispatch(verb, body) {
            return Self { key: None };
        }
        let key = format!("{plugin}\u{0}{verb}");
        let mut active = tool_inflight().lock().unwrap_or_else(|e| e.into_inner());
        let ticket = {
            let queue = active.entry(key.clone()).or_insert(ToolQueue {
                next_ticket: 0,
                now_serving: 0,
                active: false,
            });
            let ticket = queue.next_ticket;
            queue.next_ticket += 1;
            ticket
        };
        loop {
            let queue = active
                .get_mut(&key)
                .expect("tool queue exists for its assigned ticket");
            if queue.now_serving == ticket && !queue.active {
                queue.now_serving += 1;
                queue.active = true;
                return Self { key: Some(key) };
            }
            active = tool_step_released()
                .wait(active)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    pub fn acquire_within(plugin: &str, verb: &str, body: &str, task: &str, max_wait: Duration) -> Result<Self, ToolQueueWaitReport> {
        if is_unserialized_dispatch(verb, body) {
            return Ok(Self { key: None });
        }
        let key = format!("{plugin}\u{0}{verb}");
        let started = Instant::now();
        let mut active = tool_inflight().lock().unwrap_or_else(|e| e.into_inner());
        let ticket = {
            let queue = active.entry(key.clone()).or_insert(ToolQueue {
                next_ticket: 0,
                now_serving: 0,
                active: false,
            });
            let ticket = queue.next_ticket;
            queue.next_ticket += 1;
            ticket
        };
        let acquired = loop {
            let queue = active.get_mut(&key).expect("tool queue exists for its assigned ticket");
            if queue.now_serving == ticket && !queue.active {
                queue.now_serving += 1;
                queue.active = true;
                break true;
            }
            let remaining = max_wait.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break false;
            }
            let (next, timed_out) = tool_step_released()
                .wait_timeout(active, remaining)
                .unwrap_or_else(|e| e.into_inner());
            active = next;
            if timed_out.timed_out() {
                break false;
            }
        };
        if !acquired {
            let queue = active.get_mut(&key).expect("tool queue exists for its assigned ticket");
            let position = ticket.saturating_sub(queue.now_serving);
            queue.now_serving = queue.now_serving.max(ticket + 1);
            let queue_drained = queue.next_ticket == queue.now_serving;
            if queue_drained {
                active.remove(&key);
            }
            drop(active);
            let holder_task = tool_queue_holders().lock().unwrap_or_else(|e| e.into_inner()).get(&key).cloned();
            tool_step_released().notify_all();
            return Err(ToolQueueWaitReport {
                queue_key: key,
                waited_ms: started.elapsed().as_millis() as u64,
                position,
                holder_task,
            });
        }
        tool_queue_holders()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), task.to_string());
        Ok(Self { key: Some(key) })
    }
}

impl Drop for ToolDispatchGuard {
    fn drop(&mut self) {
        let Some(key) = self.key.as_ref() else {
            return;
        };
        let mut active = tool_inflight().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(queue) = active.get_mut(key) {
            queue.active = false;
            if queue.next_ticket == queue.now_serving {
                active.remove(key);
                drop(active);
                tool_queue_holders().lock().unwrap_or_else(|e| e.into_inner()).remove(key);
                tool_step_released().notify_all();
                return;
            }
        }
        drop(active);
        tool_step_released().notify_all();
    }
}

pub fn read_project_plugin_list(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join(".agentplug").join("plugins.txt"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}
