use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use wasmtime::{Instance, Store};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

/// A sibling plugin's own Store+Instance pair. `host_plugin_call` must drive
/// the TARGET plugin's calls through the target's own Store, never the
/// calling plugin's `Caller` -- wasmtime's `Instance::get_typed_func`/`call`
/// require a StoreContextMut matching the store the Instance was
/// instantiated in; passing a different plugin's Caller panics at runtime
/// with "object used with the wrong store" (wasmtime-46 store/data.rs:213).
/// Boxed because HostState itself lives inside a Store<HostState> -- an
/// unboxed Store<HostState> field would make HostState infinitely-sized.
pub struct SiblingHandle {
    pub store: Store<HostState>,
    pub instance: Instance,
}

/// One HostState per (project, plugin) instantiation. `siblings` is shared
/// (Arc<Mutex<..>>) across every plugin instance for the SAME project, so
/// `host_plugin_call` on any one of them can look up any other -- e.g. gm.wasm's
/// HostState and bert.wasm's HostState for the same project both point at the
/// same underlying sibling map, populated as each plugin is instantiated.
/// Each entry owns its OWN Store (see SiblingHandle) so cross-plugin calls
/// never reuse the calling plugin's Store.
pub struct HostState {
    pub cwd: PathBuf,
    pub plugin_name: String,
    // Own-plugin Instance handle -- safe to call with THIS HostState's own
    // Caller/Store (that's what `caller` already IS inside an import
    // callback), unlike `siblings`' entries which each need their OWN Store.
    // Populated by ProjectPlugins::load_plugin right after instantiation.
    pub self_instance: Arc<Mutex<Option<wasmtime::Instance>>>,
    pub siblings: Arc<Mutex<HashMap<String, Arc<Mutex<Option<SiblingHandle>>>>>>,
    pub wasi: WasiP1Ctx,
}

impl HostState {
    pub fn new(
        cwd: PathBuf,
        plugin_name: String,
        siblings: Arc<Mutex<HashMap<String, Arc<Mutex<Option<SiblingHandle>>>>>>,
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
        Self { cwd, plugin_name, self_instance: Arc::new(Mutex::new(None)), siblings, wasi }
    }
}
