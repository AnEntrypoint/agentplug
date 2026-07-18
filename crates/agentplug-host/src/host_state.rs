use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use wasmtime::Instance;
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

/// One HostState per (project, plugin) instantiation. `siblings` is shared
/// (Arc<Mutex<..>>) across every plugin instance for the SAME project, so
/// `host_plugin_call` on any one of them can look up any other -- e.g. gm.wasm's
/// HostState and bert.wasm's HostState for the same project both point at the
/// same underlying sibling map, populated as each plugin is instantiated.
pub struct HostState {
    pub cwd: PathBuf,
    pub plugin_name: String,
    pub instance: Arc<Mutex<Option<Instance>>>,
    pub siblings: Arc<Mutex<HashMap<String, Arc<Mutex<Option<Instance>>>>>>,
    pub wasi: WasiP1Ctx,
}

impl HostState {
    pub fn new(
        cwd: PathBuf,
        plugin_name: String,
        siblings: Arc<Mutex<HashMap<String, Arc<Mutex<Option<Instance>>>>>>,
    ) -> Self {
        let mut builder = WasiCtxBuilder::new();
        builder.inherit_stderr();
        if let Err(e) = builder.preopened_dir(&cwd, ".", DirPerms::all(), FilePerms::all()) {
            eprintln!(
                "[agentplug] WARNING: failed to preopen {} for WASI ({}): {e}",
                cwd.display(),
                plugin_name
            );
        }
        let wasi = builder.build_p1();
        let instance = Arc::new(Mutex::new(None));
        siblings.lock().unwrap().insert(plugin_name.clone(), instance.clone());
        Self { cwd, plugin_name, instance, siblings, wasi }
    }
}
