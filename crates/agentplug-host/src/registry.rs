use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use wasmtime::{Engine, Linker, Module, Store};

use crate::host_state::{HostState, SiblingHandle};
use crate::imports::{register_env_imports, register_wasi};

pub const PLUGIN_IDLE_EVICT_MS: u64 = 30 * 60 * 1000;

pub const EPOCH_TICK_INTERVAL_MS: u64 = 1_000;

pub fn epoch_ticks_for_seconds(secs: u64) -> u64 {
    (secs * 1000).div_ceil(EPOCH_TICK_INTERVAL_MS)
}

pub const DISPATCH_CALL_DEADLINE_SECS: u64 = 40;

fn is_stateless_shared_plugin(plugin_name: &str) -> bool {
    matches!(plugin_name, "bert" | "treesitter" | "libsql" | "gm")
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
}

impl SharedPluginPool {
    pub fn new(size: usize) -> Self {
        Self { slots: (0..size.max(1)).map(|_| Arc::new(Mutex::new(None))).collect() }
    }

    pub fn acquire(&self) -> Option<std::sync::MutexGuard<'_, Option<SiblingHandle>>> {
        const ACQUIRE_TIMEOUT_MS: u64 = 20_000;
        const POLL_INTERVAL_MS: u64 = 25;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ACQUIRE_TIMEOUT_MS);
        loop {
            for slot in &self.slots {
                if let Ok(guard) = slot.try_lock() {
                    return Some(guard);
                }
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
        }
    }

    fn any_instantiated(&self) -> bool {
        self.slots.iter().any(|s| s.lock().unwrap().is_some())
    }

    fn release_all(&self) -> bool {
        let mut released = false;
        for slot in &self.slots {
            let mut guard = slot.lock().unwrap();
            if guard.is_some() {
                *guard = None;
                released = true;
            }
        }
        released
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
    shared_plugin_pool(plugin_name).release_all()
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
    let plugin_name = store.data().plugin_name.clone();
    store.set_epoch_deadline(epoch_ticks_for_seconds(DISPATCH_CALL_DEADLINE_SECS));
    let alloc = instance.get_typed_func::<u32, u32>(&mut *store, "plugkit_alloc")?;
    let memory = instance.get_memory(&mut *store, "memory").ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} has no exported memory"))?;

    let verb_ptr = alloc.call(&mut *store, verb.len() as u32)?;
    memory.write(&mut *store, verb_ptr as usize, verb.as_bytes())?;
    let body_ptr = alloc.call(&mut *store, body.len() as u32)?;
    memory.write(&mut *store, body_ptr as usize, body.as_bytes())?;

    let dispatch_fn = instance
        .get_typed_func::<(u32, u32, u32, u32), u64>(&mut *store, "plugin_call")
        .or_else(|_| instance.get_typed_func::<(u32, u32, u32, u32), u64>(&mut *store, "dispatch_verb"))?;
    let call_result = dispatch_fn.call(&mut *store, (verb_ptr, verb.len() as u32, body_ptr, body.len() as u32));
    let packed = match call_result {
        Ok(p) => p,
        Err(e) => {
            if matches!(e.downcast_ref::<wasmtime::Trap>(), Some(wasmtime::Trap::Interrupt)) {
                return Err(anyhow::anyhow!("plugin_call_deadline_exceeded: {plugin_name} exceeded {DISPATCH_CALL_DEADLINE_SECS}s executing verb {verb}"));
            }
            return Err(e.into());
        }
    };

    let ptr = (packed & 0xffff_ffff) as u32;
    let len = (packed >> 32) as u32;
    if ptr == 0 || len == 0 {
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

fn instantiate_plugin(engine: &Engine, root: PathBuf, plugin_name: &str, module: &Module) -> anyhow::Result<SiblingHandle> {
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
    Ok(SiblingHandle { store, instance })
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
        self.siblings.lock().unwrap().get(plugin_name).map(|p| p.any_instantiated()).unwrap_or(false)
    }

    pub fn load_plugin(&mut self, engine: &Engine, plugin_name: &str, module: &Module) -> anyhow::Result<()> {
        if is_stateless_shared_plugin(plugin_name) {
            let pool = shared_plugin_pool(plugin_name);
            {
                let mut guard = pool.acquire().ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} pool busy (timeout acquiring slot for load)"))?;
                if guard.is_none() {
                    *guard = Some(instantiate_plugin(engine, self.root.clone(), plugin_name, module)?);
                }
            }
            self.siblings.lock().unwrap().insert(plugin_name.to_string(), pool);
            return Ok(());
        }

        let instantiated = instantiate_plugin(engine, self.root.clone(), plugin_name, module)?;
        let pool = self
            .siblings
            .lock()
            .unwrap()
            .entry(plugin_name.to_string())
            .or_insert_with(|| Arc::new(SharedPluginPool::new(1)))
            .clone();
        *pool.acquire().ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} pool busy (timeout acquiring slot for load)"))? = Some(instantiated);
        Ok(())
    }

    pub fn dispatch(&mut self, plugin_name: &str, verb: &str, body: &str) -> anyhow::Result<String> {
        self.last_active = Instant::now();
        const DISPATCH_LOOKUP_RETRY_ATTEMPTS: u32 = 3;
        const DISPATCH_LOOKUP_RETRY_BACKOFF_MS: u64 = 200;
        let mut pool = None;
        for attempt in 0..DISPATCH_LOOKUP_RETRY_ATTEMPTS {
            pool = self.siblings.lock().unwrap().get(plugin_name).cloned();
            if pool.is_some() || attempt + 1 == DISPATCH_LOOKUP_RETRY_ATTEMPTS { break; }
            std::thread::sleep(std::time::Duration::from_millis(DISPATCH_LOOKUP_RETRY_BACKOFF_MS));
        }
        let pool = pool.ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} not loaded"))?;
        let mut guard = pool.acquire().ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} pool busy (timeout acquiring slot)"))?;
        dispatch_and_evict_on_error(&mut guard, verb, body, &self.root, &self.siblings, plugin_name)
    }

    pub fn dispatch_handle(&self) -> DispatchHandle {
        DispatchHandle { root: self.root.clone(), siblings: self.siblings.clone() }
    }
}

#[derive(Clone)]
pub struct DispatchHandle {
    root: PathBuf,
    siblings: Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>,
}

impl DispatchHandle {
    pub fn dispatch(&self, plugin_name: &str, verb: &str, body: &str) -> anyhow::Result<String> {
        const DISPATCH_LOOKUP_RETRY_ATTEMPTS: u32 = 3;
        const DISPATCH_LOOKUP_RETRY_BACKOFF_MS: u64 = 200;
        let mut pool = None;
        for attempt in 0..DISPATCH_LOOKUP_RETRY_ATTEMPTS {
            pool = self.siblings.lock().unwrap().get(plugin_name).cloned();
            if pool.is_some() || attempt + 1 == DISPATCH_LOOKUP_RETRY_ATTEMPTS { break; }
            std::thread::sleep(std::time::Duration::from_millis(DISPATCH_LOOKUP_RETRY_BACKOFF_MS));
        }
        let pool = pool.ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} not loaded"))?;
        let mut guard = pool.acquire().ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} pool busy (timeout acquiring slot)"))?;
        dispatch_and_evict_on_error(&mut guard, verb, body, &self.root, &self.siblings, plugin_name)
    }
}

fn dispatch_and_evict_on_error(
    guard: &mut std::sync::MutexGuard<'_, Option<SiblingHandle>>,
    verb: &str,
    body: &str,
    root: &Path,
    siblings: &Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>,
    plugin_name: &str,
) -> anyhow::Result<String> {
    let handle = guard.as_mut().ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} not loaded"))?;
    let result = dispatch_on(&mut handle.store, handle.instance, verb, body, root, siblings.clone());
    if result.is_err() {
        **guard = None;
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
