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

/// A sibling wasm plugin's lifecycle state (paper Section 4.3, Definition
/// 49), the same three-state reduction `discipline_note.rs::FiberLifecycle`
/// uses for disciplines (gm has no async load step for either -- a plugin
/// load is one synchronous `Module::from_file` + `load_plugin` call, so
/// there is no `Reloading` window to model). Generalizes the fiber
/// lifecycle abstraction beyond disciplines to gm's OTHER real component
/// family: `Inactive` (never loaded, or evicted), `Active` (a pool slot
/// currently holds this plugin's content), `Unloading` (a load attempt
/// failed or the plugin was evicted, one dispatch before the state
/// collapses back to `Inactive`).
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

/// Advances one plugin's persisted lifecycle by exactly one transition,
/// given whether `load_plugin` just succeeded. Mirrors the discipline
/// fiber's `advance_fiber`: `Inactive -> Active` on success,
/// `Active -> Unloading` on failure (a load attempt that fails leaves the
/// prior content in place structurally via `load_plugin`'s own LIFO
/// revert, but the LIFECYCLE marks the attempt as a withdrawal-in-
/// progress), `Unloading -> Inactive` on the following call regardless of
/// outcome.
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
        Self {
            slots: (0..size.max(1)).map(|_| Arc::new(Mutex::new(None))).collect(),
            hashes_to_evict_when_their_in_flight_dispatch_completes: Mutex::new(std::collections::HashSet::new()),
            ticket_queue: Mutex::new(TicketQueue { next_ticket: 0, now_serving: 0 }),
            slot_released: Condvar::new(),
        }
    }

    /// Diagnostic-only ceiling: how long a FIFO-fair wait may run before it is reported as
    /// abnormally long. Crossing it never denies the request -- the wait keeps going. The real
    /// backstop against a truly wedged pool is each dispatch's own outer call deadline
    /// (`DISPATCH_CALL_DEADLINE_SECS`), which aborts the holder and frees its slot.
    pub const ACQUIRE_TIMEOUT_MS: u64 = 60_000;

    pub fn acquire(&self) -> Option<std::sync::MutexGuard<'_, Option<SiblingHandle>>> {
        Some(self.acquire_within(Self::ACQUIRE_TIMEOUT_MS).0)
    }

    /// FIFO-fair, non-denying slot acquisition. Every caller draws a ticket and is served in
    /// arrival order -- no unordered `try_lock` race across waiters, so a project's own repeat
    /// requests drain against each other in the order they were issued instead of restarting
    /// the race each poll tick. `timeout_ms` is retained as the elapsed-time figure reported to
    /// the caller for observability; it no longer terminates the wait early. A genuinely stuck
    /// holder (wasm trap, deadlock) is bounded by its own dispatch-call deadline elsewhere, not
    /// by this wait giving up.
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
        self.slots.iter().map(|s| s.lock().unwrap().as_ref().map(|h| h.content_hash.clone())).collect()
    }

    pub(crate) fn any_instantiated_within(&self, timeout_ms: u64) -> bool {
        const POLL_INTERVAL_MS: u64 = 25;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            for slot in &self.slots {
                if let Ok(guard) = slot.try_lock() {
                    if guard.is_some() {
                        return true;
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                return self.slots.iter().any(|s| s.lock().unwrap().is_some());
            }
            std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
        }
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
        if deferred > 0 {
            self.hashes_to_evict_when_their_in_flight_dispatch_completes.lock().unwrap_or_else(|e| e.into_inner()).insert(old_hash.to_string());
        }
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
        .unwrap()
        .get(plugin_name)
        .map(|pool| pool.slot_content_hashes())
        .unwrap_or_default()
}

/// The content hash currently answering dispatches for `plugin_name`,
/// mirroring the paper's `provider_k(gamma)` (Definition 46): each shared
/// plugin is a singleton service (bert/libsql/treesitter each have exactly
/// one logical identity, unlike the paper's multi-provider services), so
/// its `SharedPluginPool` -- every dispatch resolving through the pool's
/// slots regardless of which slot happens to be free -- already IS the
/// stable entrypoint a Cordis service broker (Section 6.2) provides: a
/// caller never names a concrete slot, only the plugin name, and the pool
/// decouples that name from which of its N pooled instances actually
/// answers. Returns `None` when no slot has been filled yet (the service
/// has no active provider). A pool with mixed content hashes across slots
/// (a swap in progress) returns the hash the FIRST filled slot carries --
/// callers wanting the full in-flight picture use
/// `shared_plugin_slot_content_hashes` instead.
pub fn get_active_provider(plugin_name: &str) -> Option<String> {
    shared_plugin_slot_content_hashes(plugin_name)
        .into_iter()
        .flatten()
        .next()
}

/// The real integration point for Cordis paper Section 6.2 service
/// multiplexing: every dispatch call site (`ProjectPlugins::dispatch`,
/// `DispatchHandle::dispatch`) routes the caller-named `plugin_name` through
/// here before doing the `siblings` map lookup. `plugin_name` doubles as the
/// broker's `service_key` -- a caller wanting broker semantics for a
/// capability registers >=2 provider instances under that same key via
/// `broker::register_provider`, each provider's `provider_id` naming a
/// DISTINCT key already present in `self.siblings` (a separately-loaded
/// plugin instance). `broker::route` selects one such `provider_id` per its
/// configured policy and the returned `RouteLease` names the sibling-map key
/// to dispatch on. Zero or one registered provider (the default, unchanged
/// from before this function existed) returns `(plugin_name, None)` --
/// exclusive binding (`SharedPluginPool`/`get_active_provider`, Definition
/// 45/46) stays the default single-provider path with no behavior change.
///
/// The caller MUST hold the returned `RouteLease` alive for the full
/// duration of the actual dispatch call and drop it only once that call
/// returns. Dropping it early (e.g. right after reading `provider_id` out
/// of it) decrements `in_flight` before the real wasm call even starts,
/// which defeats `LeastLoaded` selection (every provider would read as
/// idle regardless of genuine concurrent load) and lets
/// `unregister_provider`'s in-flight safety check race a still-running
/// dispatch into believing the provider is safe to drop mid-call.
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

    /// Like `is_loaded`, but for non-shared (stateful, per-session) plugins a
    /// loaded instance whose content hash no longer matches `content_hash` is
    /// treated as not-loaded, so a rebuilt `.wasm` is picked up on the next
    /// dispatch instead of being served stale forever. Shared plugins already
    /// self-refresh their hash inside `load_plugin`'s pool-fill check, so this
    /// only changes behavior for the non-shared path.
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
            // Revertible-effect discipline: each slot fill is an effect whose inverse is
            // "put the prior occupant back". If a later slot's instantiate fails, every
            // slot already filled this call is reverted to its pre-fill state (LIFO) so a
            // partial swap never leaves the pool straddling old and new content hashes --
            // a mixed pool would silently route some dispatches to the stale plugin
            // indefinitely, since is_loaded_current only checks that ANY slot matches.
            let mut inverses: Vec<(Arc<Mutex<Option<SiblingHandle>>>, Option<SiblingHandle>)> = Vec::new();
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
                            inverses.push((slot.clone(), prior));
                        }
                    }
                }
                Ok(())
            })();
            if let Err(err) = fill_result {
                for (slot, prior) in inverses.into_iter().rev() {
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
        let (routed_name, _route_lease) = resolve_routed_plugin_name(plugin_name);
        let plugin_name = routed_name.as_str();
        const DISPATCH_LOOKUP_RETRY_ATTEMPTS: u32 = 3;
        const DISPATCH_LOOKUP_RETRY_BACKOFF_MS: u64 = 200;
        let mut pool = None;
        for attempt in 0..DISPATCH_LOOKUP_RETRY_ATTEMPTS {
            pool = self.siblings.lock().unwrap().get(plugin_name).cloned();
            if pool.is_some() || attempt + 1 == DISPATCH_LOOKUP_RETRY_ATTEMPTS { break; }
            std::thread::sleep(std::time::Duration::from_millis(DISPATCH_LOOKUP_RETRY_BACKOFF_MS));
        }
        let pool = pool.ok_or_else(|| PluginDispatchError::NotRegistered { plugin_name: plugin_name.to_string() })?;
        let (mut guard, _waited_ms) = pool.acquire_within(SharedPluginPool::ACQUIRE_TIMEOUT_MS);
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
        let (routed_name, _route_lease) = resolve_routed_plugin_name(plugin_name);
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
    } else {
        pool.evict_if_swap_pending(guard);
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

