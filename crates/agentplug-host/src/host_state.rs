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
    // Mutex, not a plain PathBuf: a SHARED plugin instance (see
    // is_stateless_shared_plugin) reuses this same HostState across every
    // project's dispatch -- registry.rs's dispatch_on updates this field to
    // the CALLING project's root immediately before every call through a
    // shared instance, so host_cwd (and every host_fs_*/kv import that reads
    // it) always reflects the current dispatch's real project, never
    // whichever project happened to instantiate this Store first. A
    // per-project (non-shared) instance's cwd is set once and never
    // changes, which is still correct since it's never reused across
    // projects.
    pub cwd: Mutex<PathBuf>,
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
        // Preopened at whichever project instantiates this Store FIRST --
        // fixed for the Store's lifetime, unlike `cwd` below. Safe only
        // because every plugin this host currently serves (gm/bert/
        // treesitter/libsql) does its real file I/O through the host_fs_*
        // imports (which consult the mutable `cwd` field fresh per call),
        // never raw WASI filesystem syscalls -- if a future plugin needs
        // real WASI fs access while also being in the shared-instance set,
        // this preopen would need the same per-call-refresh treatment.
        if let Err(e) = builder.preopened_dir(&cwd, ".", DirPerms::all(), FilePerms::all()) {
            eprintln!(
                "[agentplug] WARNING: failed to preopen {} for WASI ({}): {e}",
                cwd.display(),
                plugin_name
            );
        }
        let wasi = builder.build_p1();
        Self { cwd: Mutex::new(cwd), plugin_name, self_instance: Arc::new(Mutex::new(None)), siblings, wasi }
    }

    pub fn set_cwd(&self, cwd: PathBuf) {
        *self.cwd.lock().unwrap() = cwd;
    }

    pub fn cwd(&self) -> PathBuf {
        self.cwd.lock().unwrap().clone()
    }
}
