use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use wasmtime::{Engine, Linker, Module, Store};

use crate::host_state::{HostState, SiblingHandle};
use crate::imports::{register_env_imports, register_wasi};

pub const PLUGIN_IDLE_EVICT_MS: u64 = 30 * 60 * 1000;

/// Plugins with no per-project state (pure function of input -> output,
/// nothing keyed by project root) get ONE process-wide instance shared by
/// every project instead of one instantiation per project. "bert" is the
/// motivating case: its ~133MB embedding model is `include_bytes!`'d into
/// the wasm module and deserialized into live tensors on first `embed` call
/// (candle's VarBuilder::from_slice_safetensors) -- with N concurrently
/// active projects, the old per-project instantiation held N separate
/// deserialized copies resident at once (live-witnessed: 2 active projects,
/// ~2.3GB daemon RSS vs the ~500MB expected for one gm.wasm instance).
/// "gm" (per-project phase/PRD/mutable state) and "libsql" (per-project open
/// DB connections) are NOT stateless and must keep one instance per project.
fn is_stateless_shared_plugin(plugin_name: &str) -> bool {
    plugin_name == "bert"
}

type SharedPluginMap = Mutex<HashMap<String, Arc<Mutex<Option<SiblingHandle>>>>>;
static SHARED_PLUGINS: OnceLock<SharedPluginMap> = OnceLock::new();

fn shared_plugin_cell(plugin_name: &str) -> Arc<Mutex<Option<SiblingHandle>>> {
    SHARED_PLUGINS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .entry(plugin_name.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(None)))
        .clone()
}

/// Runs a verb dispatch against an already-instantiated plugin's OWN Store.
/// Shared by `ProjectPlugins::dispatch` (top-level spool dispatch) and
/// `host_plugin_call` (cross-plugin dispatch) so both go through the exact
/// same store -- never a different plugin's Caller/Store, which is what
/// produced wasmtime's "object used with the wrong store" panic
/// (store/data.rs:213) the first time host_plugin_call tried to drive a
/// sibling Instance using the calling plugin's Caller.
pub fn dispatch_on(store: &mut Store<HostState>, instance: wasmtime::Instance, verb: &str, body: &str) -> anyhow::Result<String> {
    let plugin_name = store.data().plugin_name.clone();
    let alloc = instance.get_typed_func::<u32, u32>(&mut *store, "plugkit_alloc")?;
    let memory = instance.get_memory(&mut *store, "memory").ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} has no exported memory"))?;

    let verb_ptr = alloc.call(&mut *store, verb.len() as u32)?;
    memory.write(&mut *store, verb_ptr as usize, verb.as_bytes())?;
    let body_ptr = alloc.call(&mut *store, body.len() as u32)?;
    memory.write(&mut *store, body_ptr as usize, body.as_bytes())?;

    // "plugin_call" is the export name every genuinely agentplug-native
    // plugin (bert/libsql/treesitter, all built this session) uses -- but
    // "gm" is plugkit-core's own wasm, built by rs-plugkit's own
    // long-standing cascade, predating the agentplug ABI and still
    // exporting its original name "dispatch_verb". Try the new convention
    // first, fall back to the pre-existing one, so both plugin generations
    // dispatch through this same function without gm.wasm needing to change.
    let dispatch_fn = instance
        .get_typed_func::<(u32, u32, u32, u32), u64>(&mut *store, "plugin_call")
        .or_else(|_| instance.get_typed_func::<(u32, u32, u32, u32), u64>(&mut *store, "dispatch_verb"))?;
    let packed = dispatch_fn.call(&mut *store, (verb_ptr, verb.len() as u32, body_ptr, body.len() as u32))?;

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

fn instantiate_plugin(
    engine: &Engine,
    root: PathBuf,
    plugin_name: &str,
    module: &Module,
    siblings: Arc<Mutex<HashMap<String, Arc<Mutex<Option<SiblingHandle>>>>>>,
) -> anyhow::Result<SiblingHandle> {
    let mut linker: Linker<HostState> = Linker::new(engine);
    register_wasi(&mut linker)?;
    register_env_imports(&mut linker)?;

    let host_state = HostState::new(root, plugin_name.to_string(), siblings);
    let self_instance_cell = host_state.self_instance.clone();
    let mut store = Store::new(engine, host_state);
    let instance = linker.instantiate(&mut store, module)?;
    *self_instance_cell.lock().unwrap() = Some(instance);
    Ok(SiblingHandle { store, instance })
}

/// Every plugin instance loaded for one project. `siblings` is the shared
/// name->handle map every plugin's HostState points at, so any plugin's
/// `host_plugin_call` can reach any other already-loaded plugin for this
/// SAME project -- the mediator: agentplug-runner owns this map, plugins
/// never see each other directly. Each SiblingHandle owns its OWN Store, so
/// `ProjectPlugins` itself no longer keeps a separate stores/instances map --
/// `siblings` IS the canonical storage, avoiding two owners for one Store.
pub struct ProjectPlugins {
    pub root: PathBuf,
    siblings: Arc<Mutex<HashMap<String, Arc<Mutex<Option<SiblingHandle>>>>>>,
    pub last_active: Instant,
}

impl ProjectPlugins {
    pub fn new(root: PathBuf) -> Self {
        Self { root, siblings: Arc::new(Mutex::new(HashMap::new())), last_active: Instant::now() }
    }

    pub fn is_loaded(&self, plugin_name: &str) -> bool {
        self.siblings.lock().unwrap().get(plugin_name).map(|c| c.lock().unwrap().is_some()).unwrap_or(false)
    }

    /// Instantiates `module` under `plugin_name` for this project. Modules
    /// are compiled ONCE per plugin (shared `Module`, keyed by plugin name,
    /// owned by the caller -- typically agentplug-runner's global registry).
    ///
    /// Stateless plugins (see `is_stateless_shared_plugin`) get ONE process-wide
    /// instance reused by every project instead of one instantiation per
    /// project -- this project's siblings map just points at the same shared
    /// cell everyone else uses. Stateful plugins ("gm": bakes its project
    /// root into a wasm-side OnceLock on first dispatch, cannot be shared
    /// across two different project roots without silently operating on the
    /// wrong project; "libsql": holds real open DB connections keyed by
    /// project) keep the original expensive-compile/cheap-instantiate-per-project
    /// split rs-plugkit's gm-runner daemon.rs already established.
    pub fn load_plugin(&mut self, engine: &Engine, plugin_name: &str, module: &Module) -> anyhow::Result<()> {
        if is_stateless_shared_plugin(plugin_name) {
            let cell = shared_plugin_cell(plugin_name);
            if cell.lock().unwrap().is_none() {
                let instantiated = instantiate_plugin(engine, self.root.clone(), plugin_name, module, self.siblings.clone())?;
                *cell.lock().unwrap() = Some(instantiated);
            }
            self.siblings.lock().unwrap().insert(plugin_name.to_string(), cell);
            return Ok(());
        }

        let instantiated = instantiate_plugin(engine, self.root.clone(), plugin_name, module, self.siblings.clone())?;
        let cell = self
            .siblings
            .lock()
            .unwrap()
            .entry(plugin_name.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone();
        *cell.lock().unwrap() = Some(instantiated);
        Ok(())
    }

    pub fn dispatch(&mut self, plugin_name: &str, verb: &str, body: &str) -> anyhow::Result<String> {
        self.last_active = Instant::now();
        let cell = self.siblings.lock().unwrap().get(plugin_name).cloned().ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} not loaded"))?;
        let mut guard = cell.lock().unwrap();
        let handle = guard.as_mut().ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} not loaded"))?;
        dispatch_on(&mut handle.store, handle.instance, verb, body)
    }
}

/// Discovers which plugins a project wants loaded: `<root>/.agentplug/plugins.txt`,
/// one plugin name per line (e.g. "gm", "bert"). Missing file = no plugins
/// requested yet (caller decides the default, typically just "gm").
pub fn read_project_plugin_list(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join(".agentplug").join("plugins.txt"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}
