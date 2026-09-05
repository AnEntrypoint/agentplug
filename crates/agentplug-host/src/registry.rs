use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

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
        .unwrap_or(PluginFiberState { state: PluginFiberLifecycle::Inactive, content_hash: None })
}

fn write_plugin_fiber_state(plugin_name: &str, state: PluginFiberLifecycle, content_hash: Option<String>) {
    let body = PluginFiberState { state, content_hash };
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
        }
    }
}

impl std::error::Error for PluginDispatchError {}

fn log_poisoned_store_eviction_event(root: &Path, plugin_name: &str, verb: &str, reinstantiation_succeeded: bool, prior_dispatch_error: &str) {
    let log_path = root.join(".gm").join("exec-spool").join(".watcher.log");
    let Some(parent) = log_path.parent() else { return };
    let _ = std::fs::create_dir_all(parent);
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) else { return };
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

pub struct SharedPluginPool {
    slots: Vec<Arc<Mutex<Option<SiblingHandle>>>>,
    last_observed_slot_hashes: Mutex<Vec<Option<String>>>,
    hashes_to_evict_when_their_in_flight_dispatch_completes: Mutex<std::collections::HashSet<String>>,
    ticket_queue: Mutex<TicketQueue>,
    slot_released: Condvar,
}

struct TicketQueue {
    next_ticket: u64,
    now_serving: u64,
}

impl SharedPluginPool {
    pub fn new(size: usize) -> Self {
        let size = size.max(1);
        Self {
            slots: (0..size).map(|_| Arc::new(Mutex::new(None))).collect(),
            last_observed_slot_hashes: Mutex::new(vec![None; size]),
            hashes_to_evict_when_their_in_flight_dispatch_completes: Mutex::new(std::collections::HashSet::new()),
            ticket_queue: Mutex::new(TicketQueue { next_ticket: 0, now_serving: 0 }),
            slot_released: Condvar::new(),
        }
    }

    pub const ACQUIRE_TIMEOUT_MS: u64 = 60_000;

    pub fn acquire(&self) -> Option<std::sync::MutexGuard<'_, Option<SiblingHandle>>> {
        Some(self.acquire_within(Self::ACQUIRE_TIMEOUT_MS).0)
    }

    pub fn acquire_within(&self, timeout_ms: u64) -> (std::sync::MutexGuard<'_, Option<SiblingHandle>>, u64) {
        let start = std::time::Instant::now();
        let my_ticket = {
            let mut q = self.ticket_queue.lock().unwrap();
            let t = q.next_ticket;
            q.next_ticket += 1;
            t
        };
        loop {
            {
                let q = self.ticket_queue.lock().unwrap();
                if q.now_serving == my_ticket {
                    for slot in &self.slots {
                        if let Ok(guard) = slot.try_lock() {
                            drop(q);
                            let mut q = self.ticket_queue.lock().unwrap();
                            q.now_serving += 1;
                            drop(q);
                            self.slot_released.notify_all();
                            return (guard, start.elapsed().as_millis() as u64);
                        }
                    }
                }
            }
            let guard = self.ticket_queue.lock().unwrap();
            let _ = self
                .slot_released
                .wait_timeout(guard, std::time::Duration::from_millis(25))
                .unwrap();
            let waited = start.elapsed().as_millis() as u64;
            if waited > timeout_ms && waited % 5_000 < 30 {
                eprintln!(
                    "[agentplug registry] pool wait exceeded diagnostic threshold ({waited}ms > {timeout_ms}ms) -- still waiting, ticket #{my_ticket}, not denying"
                );
            }
        }
    }

    pub fn size(&self) -> usize {
        self.slots.len()
    }

    fn all_instantiated(&self) -> bool {
        self.slots.iter().all(|s| match s.try_lock() {
            Ok(g) => g.is_some(),
            Err(_) => true,
        })
    }

    pub(crate) fn slots_for_fill(&self) -> &[Arc<Mutex<Option<SiblingHandle>>>] {
        &self.slots
    }

    pub fn slot_content_hashes(&self) -> Vec<Option<String>> {
        let mut observed = self.last_observed_slot_hashes.lock().unwrap_or_else(|e| e.into_inner());
        for (index, slot) in self.slots.iter().enumerate() {
            if let Ok(guard) = slot.try_lock() {
                observed[index] = guard.as_ref().map(|h| h.content_hash.clone());
            }
        }
        observed.clone()
    }

    fn evict_every_currently_free_slot_without_blocking_on_busy_ones(&self) -> bool {
        let mut released = false;
        for slot in &self.slots {
            if let Ok(mut guard) = slot.try_lock() {
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
            match slot.try_lock() {
                Ok(mut guard) => {
                    if guard.as_ref().is_some_and(|h| h.content_hash == old_hash) {
                        *guard = None;
                        evicted += 1;
                    }
                }
                Err(_) => deferred += 1,
            }
        }
        self.hashes_to_evict_when_their_in_flight_dispatch_completes.lock().unwrap_or_else(|e| e.into_inner()).insert(old_hash.to_string());
        (evicted, deferred)
    }

    pub fn evict_if_swap_pending(&self, guard: &mut std::sync::MutexGuard<'_, Option<SiblingHandle>>) {
        let Some(handle) = guard.as_ref() else { return };
        let pending = self.hashes_to_evict_when_their_in_flight_dispatch_completes.lock().unwrap_or_else(|e| e.into_inner());
        if pending.contains(&handle.content_hash) {
            drop(pending);
            **guard = None;
        }
    }

    pub fn note_bytes_current(&self, hash: &str) {
        self.hashes_to_evict_when_their_in_flight_dispatch_completes.lock().unwrap_or_else(|e| e.into_inner()).remove(hash);
    }

    pub fn swap_pending_hashes(&self) -> Vec<String> {
        self.hashes_to_evict_when_their_in_flight_dispatch_completes.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
    }
}

type SharedPluginMap = Mutex<HashMap<String, Arc<SharedPluginPool>>>;
static SHARED_PLUGINS: OnceLock<SharedPluginMap> = OnceLock::new();

fn shared_plugin_pool(plugin_name: &str) -> Arc<SharedPluginPool> {
    let pool_size = if plugin_name == "gm" { gm_pool_size() } else { side_plugin_pool_size() };
    SHARED_PLUGINS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .entry(plugin_name.to_string())
        .or_insert_with(|| Arc::new(SharedPluginPool::new(pool_size)))
        .clone()
}

type LazyModuleSource = (Engine, HashMap<String, (Module, String)>);
static LAZY_MODULE_SOURCE: OnceLock<Mutex<Option<LazyModuleSource>>> = OnceLock::new();

fn lazy_module_source_slot() -> &'static Mutex<Option<LazyModuleSource>> {
    LAZY_MODULE_SOURCE.get_or_init(|| Mutex::new(None))
}

pub fn set_lazy_module_source(engine: Engine, modules: HashMap<String, (Module, String)>) {
    *lazy_module_source_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some((engine, modules));
}

fn lazy_module_for(plugin_name: &str) -> Option<(Engine, Module, String)> {
    let guard = lazy_module_source_slot().lock().unwrap_or_else(|e| e.into_inner());
    let (engine, modules) = guard.as_ref()?;
    let (module, hash) = modules.get(plugin_name)?;
    Some((engine.clone(), module.clone(), hash.clone()))
}

fn append_watcher_log_event(root: &Path, event: serde_json::Value) {
    let log_path = root.join(".gm").join("exec-spool").join(".watcher.log");
    let Some(parent) = log_path.parent() else { return };
    let _ = std::fs::create_dir_all(parent);
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) else { return };
    use std::io::Write;
    let _ = writeln!(f, "evt: {event}");
}

fn fill_free_slots_needing_content(pool: &SharedPluginPool, engine: &Engine, root: &Path, plugin_name: &str, module: &Module, content_hash: &str) -> anyhow::Result<usize> {
    let mut filled = 0usize;
    for slot in pool.slots_for_fill() {
        if let Ok(mut guard) = slot.try_lock() {
            let needs_fill = match guard.as_ref() {
                None => true,
                Some(existing) => existing.content_hash != content_hash,
            };
            if needs_fill {
                *guard = Some(instantiate_plugin(engine, root.to_path_buf(), plugin_name, module, content_hash)?);
                filled += 1;
            }
        }
    }
    Ok(filled)
}

pub fn ensure_sibling_loaded(
    root: &Path,
    siblings: &Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>,
    plugin_name: &str,
) -> anyhow::Result<Option<Arc<SharedPluginPool>>> {
    let existing = siblings.lock().unwrap_or_else(|e| e.into_inner()).get(plugin_name).cloned();
    let Some((engine, module, content_hash)) = lazy_module_for(plugin_name) else { return Ok(existing) };
    let started = Instant::now();
    if is_stateless_shared_plugin(plugin_name) {
        let pool = shared_plugin_pool(plugin_name);
        let filled = fill_free_slots_needing_content(&pool, &engine, root, plugin_name, &module, &content_hash)?;
        if filled > 0 {
            log_lazy_plugin_load(root, plugin_name, "shared", filled, started);
        }
        siblings.lock().unwrap_or_else(|e| e.into_inner()).insert(plugin_name.to_string(), pool.clone());
        return Ok(Some(pool));
    }
    let pool = match existing {
        Some(pool) => pool,
        None => {
            let fresh = Arc::new(SharedPluginPool::new(1));
            siblings.lock().unwrap_or_else(|e| e.into_inner()).entry(plugin_name.to_string()).or_insert(fresh).clone()
        }
    };
    let filled = fill_free_slots_needing_content(&pool, &engine, root, plugin_name, &module, &content_hash)?;
    if filled > 0 {
        log_lazy_plugin_load(root, plugin_name, "per-project", filled, started);
    }
    Ok(Some(pool))
}

const SLOT_REFILL_ATTEMPTS: usize = 4;
const SLOT_REFILL_BACKOFF_MS: u64 = 50;

pub fn acquire_filled_slot<'a>(
    pool: &'a Arc<SharedPluginPool>,
    root: &Path,
    siblings: &Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>,
    plugin_name: &str,
) -> anyhow::Result<std::sync::MutexGuard<'a, Option<SiblingHandle>>> {
    for attempt in 0..SLOT_REFILL_ATTEMPTS {
        let (guard, _waited_ms) = pool.acquire_within(SharedPluginPool::ACQUIRE_TIMEOUT_MS);
        if guard.is_some() {
            return Ok(guard);
        }
        drop(guard);
        ensure_sibling_loaded(root, siblings, plugin_name)?;
        if attempt + 1 < SLOT_REFILL_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(SLOT_REFILL_BACKOFF_MS));
        }
    }
    Err(anyhow::anyhow!("plugin_not_loaded_yet: {plugin_name} slot stayed empty after {SLOT_REFILL_ATTEMPTS} refill attempts"))
}

fn log_lazy_plugin_load(root: &Path, plugin_name: &str, scope: &str, filled: usize, started: Instant) {
    let ms = started.elapsed().as_millis() as u64;
    eprintln!("[agentplug registry] lazily instantiated {plugin_name} ({scope}, {filled} slot(s), {ms}ms) for {} on first use", root.display());
    append_watcher_log_event(root, serde_json::json!({
        "event": "plugin_lazy_loaded",
        "plugin": plugin_name,
        "scope": scope,
        "slots": filled,
        "ms": ms,
        "ts": crate::now_ms(),
    }));
}

type CeilingTable = (u64, HashMap<String, u64>);
static STORE_LINEAR_MEMORY_CEILINGS: OnceLock<Mutex<CeilingTable>> = OnceLock::new();

fn ceiling_table() -> &'static Mutex<CeilingTable> {
    STORE_LINEAR_MEMORY_CEILINGS.get_or_init(|| Mutex::new((0, HashMap::new())))
}

pub fn set_store_linear_memory_ceilings(default_bytes: u64, by_name: HashMap<String, u64>) {
    *ceiling_table().lock().unwrap_or_else(|e| e.into_inner()) = (default_bytes, by_name);
}

fn store_linear_memory_ceiling_bytes(plugin_name: &str) -> u64 {
    let table = ceiling_table().lock().unwrap_or_else(|e| e.into_inner());
    table.1.get(plugin_name).copied().unwrap_or(table.0)
}

type StoreBytesKey = (String, PathBuf);
static STORE_BYTES: OnceLock<Mutex<HashMap<StoreBytesKey, u64>>> = OnceLock::new();

fn store_bytes_table() -> &'static Mutex<HashMap<StoreBytesKey, u64>> {
    STORE_BYTES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn store_bytes_key(plugin_name: &str, root: &Path) -> StoreBytesKey {
    let scope = if is_stateless_shared_plugin(plugin_name) { PathBuf::new() } else { root.to_path_buf() };
    (plugin_name.to_string(), scope)
}

fn note_store_bytes(plugin_name: &str, root: &Path, bytes: u64) {
    store_bytes_table().lock().unwrap_or_else(|e| e.into_inner()).insert(store_bytes_key(plugin_name, root), bytes);
}

fn forget_store_bytes(plugin_name: &str, root: &Path) {
    store_bytes_table().lock().unwrap_or_else(|e| e.into_inner()).remove(&store_bytes_key(plugin_name, root));
}

pub fn forget_store_bytes_for_root(root: &Path) {
    store_bytes_table().lock().unwrap_or_else(|e| e.into_inner()).retain(|(_, scope), _| scope != root);
}

pub fn forget_store_bytes_for_shared_plugin(plugin_name: &str) {
    store_bytes_table().lock().unwrap_or_else(|e| e.into_inner()).retain(|(name, scope), _| !(name == plugin_name && scope.as_os_str().is_empty()));
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StoreBytesSummary {
    pub instances: usize,
    pub total_bytes: u64,
    pub max_bytes: u64,
    pub ceiling_bytes: u64,
    pub ceiling_evictions: u64,
    pub last_evicted_bytes: u64,
}

type EvictionTally = (u64, u64);
static CEILING_EVICTIONS: OnceLock<Mutex<HashMap<String, EvictionTally>>> = OnceLock::new();

fn ceiling_evictions_table() -> &'static Mutex<HashMap<String, EvictionTally>> {
    CEILING_EVICTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_ceiling_eviction(plugin_name: &str, bytes: u64) {
    let mut table = ceiling_evictions_table().lock().unwrap_or_else(|e| e.into_inner());
    let tally = table.entry(plugin_name.to_string()).or_insert((0, 0));
    tally.0 += 1;
    tally.1 = bytes;
}

pub fn store_bytes_by_plugin() -> HashMap<String, StoreBytesSummary> {
    let mut out: HashMap<String, StoreBytesSummary> = HashMap::new();
    for ((name, _), bytes) in store_bytes_table().lock().unwrap_or_else(|e| e.into_inner()).iter() {
        let entry = out.entry(name.clone()).or_default();
        entry.instances += 1;
        entry.total_bytes += *bytes;
        entry.max_bytes = entry.max_bytes.max(*bytes);
    }
    for (name, tally) in ceiling_evictions_table().lock().unwrap_or_else(|e| e.into_inner()).iter() {
        let entry = out.entry(name.clone()).or_default();
        entry.ceiling_evictions = tally.0;
        entry.last_evicted_bytes = tally.1;
    }
    for (name, summary) in out.iter_mut() {
        summary.ceiling_bytes = store_linear_memory_ceiling_bytes(name);
    }
    out
}

fn linear_memory_bytes(handle: &mut SiblingHandle) -> u64 {
    handle.instance.get_memory(&mut handle.store, "memory").map(|m| m.data_size(&handle.store) as u64).unwrap_or(0)
}

pub(crate) fn settle_slot_after_successful_dispatch(
    guard: &mut std::sync::MutexGuard<'_, Option<SiblingHandle>>,
    pool: &SharedPluginPool,
    root: &Path,
    plugin_name: &str,
    verb: &str,
) {
    pool.evict_if_swap_pending(guard);
    evict_if_over_linear_memory_ceiling(guard, root, plugin_name, verb);
}

fn evict_if_over_linear_memory_ceiling(guard: &mut std::sync::MutexGuard<'_, Option<SiblingHandle>>, root: &Path, plugin_name: &str, verb: &str) {
    let Some(handle) = guard.as_mut() else { return };
    let bytes = linear_memory_bytes(handle);
    note_store_bytes(plugin_name, root, bytes);
    let ceiling = store_linear_memory_ceiling_bytes(plugin_name);
    if ceiling == 0 || bytes <= ceiling {
        return;
    }
    eprintln!(
        "[agentplug registry] evicting {plugin_name} Store after verb {verb}: linear memory {}MB exceeds its {}MB ceiling (wasm memory never shrinks in place; the next dispatch re-instantiates from the file-backed module)",
        bytes / (1024 * 1024),
        ceiling / (1024 * 1024)
    );
    append_watcher_log_event(root, serde_json::json!({
        "event": "plugin_store_evicted_linear_memory_ceiling",
        "plugin": plugin_name,
        "verb": verb,
        "linear_memory_bytes": bytes,
        "ceiling_bytes": ceiling,
        "ts": crate::now_ms(),
    }));
    **guard = None;
    forget_store_bytes(plugin_name, root);
    record_ceiling_eviction(plugin_name, bytes);
}

pub fn release_shared_plugin(plugin_name: &str) -> bool {
    if !is_stateless_shared_plugin(plugin_name) {
        return false;
    }
    let released = shared_plugin_pool(plugin_name).evict_every_currently_free_slot_without_blocking_on_busy_ones();
    if released {
        forget_store_bytes_for_shared_plugin(plugin_name);
    }
    released
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

fn existing_shared_plugin_pool(plugin_name: &str) -> Option<Arc<SharedPluginPool>> {
    SHARED_PLUGINS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(plugin_name)
        .cloned()
}

pub fn shared_plugin_swap_pending_hashes(plugin_name: &str) -> Vec<String> {
    existing_shared_plugin_pool(plugin_name).map(|pool| pool.swap_pending_hashes()).unwrap_or_default()
}

pub fn shared_plugin_slot_content_hashes(plugin_name: &str) -> Vec<Option<String>> {
    existing_shared_plugin_pool(plugin_name).map(|pool| pool.slot_content_hashes()).unwrap_or_default()
}

pub fn get_active_provider(plugin_name: &str) -> Option<String> {
    shared_plugin_slot_content_hashes(plugin_name)
        .into_iter()
        .flatten()
        .next()
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
    let memory = instance.get_memory(&mut *store, "memory").ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} has no exported memory"))?;

    let verb_ptr = alloc.call(&mut *store, verb.len() as u32)?;
    memory.write(&mut *store, verb_ptr as usize, verb.as_bytes())?;
    let body_ptr = alloc.call(&mut *store, body.len() as u32)?;
    memory.write(&mut *store, body_ptr as usize, body.as_bytes())?;
    let free = instance.get_typed_func::<(u32, u32), ()>(&mut *store, "plugkit_free").ok();
    let free_call_args = |store: &mut Store<HostState>| {
        if let Some(free) = &free {
            let _ = free.call(&mut *store, (verb_ptr, verb.len() as u32));
            let _ = free.call(&mut *store, (body_ptr, body.len() as u32));
        }
    };

    let dispatch_fn = instance
        .get_typed_func::<(u32, u32, u32, u32), u64>(&mut *store, "plugin_call")
        .or_else(|_| instance.get_typed_func::<(u32, u32, u32, u32), u64>(&mut *store, "dispatch_verb"))?;
    let call_result = dispatch_fn.call(&mut *store, (verb_ptr, verb.len() as u32, body_ptr, body.len() as u32));
    let packed = match call_result {
        Ok(p) => {
            free_call_args(store);
            p
        }
        Err(e) => {
            if matches!(e.downcast_ref::<wasmtime::Trap>(), Some(wasmtime::Trap::Interrupt)) {
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
        let mut root = cwd.components().next().map(|c| PathBuf::from(c.as_os_str())).unwrap_or_else(|| PathBuf::from("C:\\"));
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

fn instantiate_plugin(engine: &Engine, root: PathBuf, plugin_name: &str, module: &Module, content_hash: &str) -> anyhow::Result<SiblingHandle> {
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
    Ok(SiblingHandle { store, instance, content_hash: content_hash.to_string() })
}

pub struct ProjectPlugins {
    pub root: PathBuf,
    siblings: Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>,
    pub last_active: Instant,
}

impl ProjectPlugins {
    pub fn new(root: PathBuf) -> Self {
        Self { root, siblings: Arc::new(Mutex::new(HashMap::new())), last_active: Instant::now() }
    }

    pub fn is_loaded(&self, plugin_name: &str) -> bool {
        self.siblings.lock().unwrap().get(plugin_name).map(|p| p.all_instantiated()).unwrap_or(false)
    }

    pub fn is_loaded_current(&self, plugin_name: &str, content_hash: &str) -> bool {
        if is_stateless_shared_plugin(plugin_name) {
            return self.is_loaded(plugin_name);
        }
        self.siblings
            .lock()
            .unwrap()
            .get(plugin_name)
            .map(|p| p.slot_content_hashes().iter().any(|h| h.as_deref() == Some(content_hash)))
            .unwrap_or(false)
    }

    pub fn load_plugin(&mut self, engine: &Engine, plugin_name: &str, module: &Module, content_hash: &str) -> anyhow::Result<()> {
        if is_stateless_shared_plugin(plugin_name) {
            let pool = shared_plugin_pool(plugin_name);
            let mut lifo_revert_stack: Vec<(Arc<Mutex<Option<SiblingHandle>>>, Option<SiblingHandle>)> = Vec::new();
            let fill_result = (|| -> anyhow::Result<()> {
                for slot in pool.slots_for_fill() {
                    if let Ok(mut guard) = slot.try_lock() {
                        let needs_fill = match guard.as_ref() {
                            None => true,
                            Some(existing) => existing.content_hash != content_hash,
                        };
                        if needs_fill {
                            let fresh = instantiate_plugin(engine, self.root.clone(), plugin_name, module, content_hash)?;
                            let prior = guard.replace(fresh);
                            lifo_revert_stack.push((slot.clone(), prior));
                        }
                    }
                }
                Ok(())
            })();
            if let Err(err) = fill_result {
                for (slot, prior) in lifo_revert_stack.into_iter().rev() {
                    if let Ok(mut guard) = slot.try_lock() {
                        *guard = prior;
                    }
                }
                return Err(err);
            }
            self.siblings.lock().unwrap().insert(plugin_name.to_string(), pool);
            return Ok(());
        }

        let instantiated = instantiate_plugin(engine, self.root.clone(), plugin_name, module, content_hash)?;
        let pool = self
            .siblings
            .lock()
            .unwrap()
            .entry(plugin_name.to_string())
            .or_insert_with(|| Arc::new(SharedPluginPool::new(1)))
            .clone();
        *pool.acquire().expect("acquire() always returns Some -- FIFO wait never denies") = Some(instantiated);
        Ok(())
    }

    pub fn dispatch(&mut self, plugin_name: &str, verb: &str, body: &str) -> anyhow::Result<String> {
        self.last_active = Instant::now();
        let (routed_name, _route_lease_held_until_dispatch_returns) = resolve_routed_plugin_name(plugin_name);
        let plugin_name = routed_name.as_str();
        const DISPATCH_LOOKUP_RETRY_ATTEMPTS: u32 = 3;
        const DISPATCH_LOOKUP_RETRY_BACKOFF_MS: u64 = 200;
        let mut pool = None;
        for attempt in 0..DISPATCH_LOOKUP_RETRY_ATTEMPTS {
            pool = self.siblings.lock().unwrap().get(plugin_name).cloned();
            if pool.is_some() || attempt + 1 == DISPATCH_LOOKUP_RETRY_ATTEMPTS { break; }
            std::thread::sleep(std::time::Duration::from_millis(DISPATCH_LOOKUP_RETRY_BACKOFF_MS));
        }
        let pool = match pool {
            Some(pool) => pool,
            None => ensure_sibling_loaded(&self.root, &self.siblings, plugin_name)?
                .ok_or_else(|| PluginDispatchError::NotRegistered { plugin_name: plugin_name.to_string() })?,
        };
        let mut guard = acquire_filled_slot(&pool, &self.root, &self.siblings, plugin_name)?;
        dispatch_and_evict_on_error(&mut guard, &pool, verb, body, &self.root, &self.siblings, plugin_name)
    }

    pub fn dispatch_handle_with_reload(&self, reload_source: Option<(Engine, HashMap<String, (Module, String)>)>) -> DispatchHandle {
        DispatchHandle { root: self.root.clone(), siblings: self.siblings.clone(), reload_source }
    }

    pub fn dispatch_handle(&self) -> DispatchHandle {
        DispatchHandle { root: self.root.clone(), siblings: self.siblings.clone(), reload_source: None }
    }
}

#[derive(Clone)]
pub struct DispatchHandle {
    root: PathBuf,
    siblings: Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>,
    reload_source: Option<(Engine, HashMap<String, (Module, String)>)>,
}

impl DispatchHandle {
    fn reinstantiate_plugin_into_pool_slot_if_reload_source_available(&self, plugin_name: &str) -> anyhow::Result<()> {
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
                let mut guard = pool.acquire().expect("acquire() always returns Some -- FIFO wait never denies");
                let needs_fill = match guard.as_ref() {
                    None => true,
                    Some(existing) => &existing.content_hash != content_hash,
                };
                if needs_fill {
                    *guard = Some(instantiate_plugin(engine, self.root.clone(), plugin_name, module, content_hash)?);
                }
            }
            self.siblings.lock().unwrap().insert(plugin_name.to_string(), pool);
            return Ok(());
        }
        let instantiated = instantiate_plugin(engine, self.root.clone(), plugin_name, module, content_hash)?;
        let pool = self
            .siblings
            .lock()
            .unwrap()
            .entry(plugin_name.to_string())
            .or_insert_with(|| Arc::new(SharedPluginPool::new(1)))
            .clone();
        *pool.acquire().expect("acquire() always returns Some -- FIFO wait never denies") = Some(instantiated);
        Ok(())
    }

    pub fn dispatch(&self, plugin_name: &str, verb: &str, body: &str) -> anyhow::Result<String> {
        let (routed_name, _route_lease_held_until_dispatch_returns) = resolve_routed_plugin_name(plugin_name);
        let plugin_name = routed_name.as_str();
        const REGISTRATION_LOOKUP_RETRY_ATTEMPTS: u32 = 3;
        const REGISTRATION_LOOKUP_RETRY_BACKOFF_MS: u64 = 200;
        let mut pool = None;
        for attempt in 0..REGISTRATION_LOOKUP_RETRY_ATTEMPTS {
            pool = self.siblings.lock().unwrap().get(plugin_name).cloned();
            if pool.is_some() || attempt + 1 == REGISTRATION_LOOKUP_RETRY_ATTEMPTS { break; }
            std::thread::sleep(std::time::Duration::from_millis(REGISTRATION_LOOKUP_RETRY_BACKOFF_MS));
        }
        if pool.is_none() {
            let _ = self.reinstantiate_plugin_into_pool_slot_if_reload_source_available(plugin_name);
            pool = self.siblings.lock().unwrap().get(plugin_name).cloned();
        }
        let pool = pool.ok_or_else(|| PluginDispatchError::NotRegistered { plugin_name: plugin_name.to_string() })?;
        let (mut guard, _waited_ms) = pool.acquire_within(SharedPluginPool::ACQUIRE_TIMEOUT_MS);
        if guard.is_none() {
            drop(guard);
            const REINSTANTIATION_RETRY_ATTEMPTS: u32 = 3;
            const REINSTANTIATION_RETRY_BACKOFF_MS: u64 = 250;
            let mut last_reload_error: Option<String> = None;
            let mut refilled_pool: Option<Arc<SharedPluginPool>> = None;
            for attempt in 0..REINSTANTIATION_RETRY_ATTEMPTS {
                if let Err(e) = self.reinstantiate_plugin_into_pool_slot_if_reload_source_available(plugin_name) {
                    last_reload_error = Some(format!("{e:#}"));
                }
                let candidate_pool = self
                    .siblings
                    .lock()
                    .unwrap()
                    .get(plugin_name)
                    .cloned()
                    .ok_or_else(|| PluginDispatchError::NotRegistered { plugin_name: plugin_name.to_string() })?;
                let is_refilled = {
                    let (retry_guard, _retry_waited_ms) = candidate_pool.acquire_within(SharedPluginPool::ACQUIRE_TIMEOUT_MS);
                    retry_guard.is_some()
                };
                if is_refilled {
                    refilled_pool = Some(candidate_pool);
                    break;
                }
                if attempt + 1 < REINSTANTIATION_RETRY_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(REINSTANTIATION_RETRY_BACKOFF_MS));
                }
            }
            let Some(refilled_pool) = refilled_pool else {
                let detail = last_reload_error.unwrap_or_else(|| "reload produced no error but no slot was repopulated".to_string());
                eprintln!("[agentplug registry] plugin {plugin_name} could not be reinstantiated after a poisoned-Store eviction (verb {verb}) -- {detail}");
                log_poisoned_store_eviction_event(&self.root, plugin_name, verb, false, &format!("reinstantiation failed after {REINSTANTIATION_RETRY_ATTEMPTS} attempts: {detail}"));
                return Err(PluginDispatchError::EvictedOrPoisoned { plugin_name: plugin_name.to_string() }.into());
            };
            let (mut final_guard, _final_waited_ms) = refilled_pool.acquire_within(SharedPluginPool::ACQUIRE_TIMEOUT_MS);
            return dispatch_and_evict_on_error(&mut final_guard, &refilled_pool, verb, body, &self.root, &self.siblings, plugin_name);
        }
        dispatch_and_evict_on_error(&mut guard, &pool, verb, body, &self.root, &self.siblings, plugin_name)
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
    let result = dispatch_on(&mut handle.store, handle.instance, verb, body, root, siblings.clone());
    if let Err(poisoning_error) = &result {
        eprintln!("[agentplug registry] evicting plugin {plugin_name} slot -- verb {verb} poisoned its Store: {poisoning_error}");
        log_poisoned_store_eviction_event(root, plugin_name, verb, true, &poisoning_error.to_string());
        **guard = None;
        forget_store_bytes(plugin_name, root);
    } else {
        pool.evict_if_swap_pending(guard);
        evict_if_over_linear_memory_ceiling(guard, root, plugin_name, verb);
    }
    result
}

#[derive(serde::Deserialize, Default)]
struct ProjectDaemonConfig {
    #[serde(default)]
    gm_concurrency_limit: Option<usize>,
}

impl ProjectDaemonConfig {
    fn load(root: &Path) -> Self {
        let path = root.join(".gm").join("daemon-project-config.json");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<ProjectDaemonConfig>(&s).ok())
            .unwrap_or_default()
    }
}

static GM_INFLIGHT_BY_PROJECT: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

fn gm_inflight_map() -> &'static Mutex<HashMap<PathBuf, usize>> {
    GM_INFLIGHT_BY_PROJECT.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct GmFairnessGuard {
    root: PathBuf,
    limited: bool,
}

impl GmFairnessGuard {
    pub fn acquire(root: &Path) -> Self {
        let limit = match ProjectDaemonConfig::load(root).gm_concurrency_limit {
            Some(n) if n > 0 => n,
            _ => return Self { root: root.to_path_buf(), limited: false },
        };
        loop {
            {
                let mut map = gm_inflight_map().lock().unwrap();
                let count = map.entry(root.to_path_buf()).or_insert(0);
                if *count < limit {
                    *count += 1;
                    return Self { root: root.to_path_buf(), limited: true };
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

impl Drop for GmFairnessGuard {
    fn drop(&mut self) {
        if !self.limited {
            return;
        }
        let mut map = gm_inflight_map().lock().unwrap();
        if let Some(count) = map.get_mut(&self.root) {
            *count = count.saturating_sub(1);
        }
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

