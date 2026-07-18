use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use wasmtime::{Engine, Instance, Linker, Module, Store};

use crate::host_state::HostState;
use crate::imports::{register_env_imports, register_wasi};

pub const PLUGIN_IDLE_EVICT_MS: u64 = 30 * 60 * 1000;

/// Every plugin instance loaded for one project. `siblings` is the shared
/// name->instance map every plugin's HostState points at, so any plugin's
/// `host_plugin_call` can reach any other already-loaded plugin for this
/// SAME project -- the mediator: agentplug-runner owns this map, plugins
/// never see each other directly.
pub struct ProjectPlugins {
    pub root: PathBuf,
    siblings: Arc<Mutex<HashMap<String, Arc<Mutex<Option<Instance>>>>>>,
    stores: HashMap<String, Store<HostState>>,
    instances: HashMap<String, Instance>,
    pub last_active: Instant,
}

impl ProjectPlugins {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            siblings: Arc::new(Mutex::new(HashMap::new())),
            stores: HashMap::new(),
            instances: HashMap::new(),
            last_active: Instant::now(),
        }
    }

    pub fn is_loaded(&self, plugin_name: &str) -> bool {
        self.instances.contains_key(plugin_name)
    }

    /// Instantiates `module` under `plugin_name` for this project. Modules
    /// are compiled ONCE per plugin (shared `Module`, keyed by plugin name,
    /// owned by the caller -- typically agentplug-runner's global registry)
    /// and instantiated fresh per project, mirroring the same
    /// expensive-compile/cheap-instantiate split rs-plugkit's gm-runner
    /// daemon.rs already established for the single-plugin case.
    pub fn load_plugin(&mut self, engine: &Engine, plugin_name: &str, module: &Module) -> anyhow::Result<()> {
        let mut linker: Linker<HostState> = Linker::new(engine);
        register_wasi(&mut linker)?;
        register_env_imports(&mut linker)?;

        let host_state = HostState::new(self.root.clone(), plugin_name.to_string(), self.siblings.clone());
        let instance_cell = host_state.instance.clone();
        let mut store = Store::new(engine, host_state);
        let instance = linker.instantiate(&mut store, module)?;
        *instance_cell.lock().unwrap() = Some(instance);

        self.stores.insert(plugin_name.to_string(), store);
        self.instances.insert(plugin_name.to_string(), instance);
        Ok(())
    }

    pub fn dispatch(&mut self, plugin_name: &str, verb: &str, body: &str) -> anyhow::Result<String> {
        self.last_active = Instant::now();
        let store = self.stores.get_mut(plugin_name).ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} not loaded"))?;
        let instance = *self.instances.get(plugin_name).ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} not loaded"))?;

        let alloc = instance.get_typed_func::<u32, u32>(&mut *store, "plugkit_alloc")?;
        let memory = instance.get_memory(&mut *store, "memory").ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} has no exported memory"))?;

        let verb_ptr = alloc.call(&mut *store, verb.len() as u32)?;
        memory.write(&mut *store, verb_ptr as usize, verb.as_bytes())?;
        let body_ptr = alloc.call(&mut *store, body.len() as u32)?;
        memory.write(&mut *store, body_ptr as usize, body.as_bytes())?;

        let dispatch_fn = instance.get_typed_func::<(u32, u32, u32, u32), u64>(&mut *store, "plugin_call")?;
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
