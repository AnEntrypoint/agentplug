use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use wasmtime::{Instance, Store};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

use crate::registry::SharedPluginPool;

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

/// One HostState per (project, plugin) instantiation -- except for shared
/// plugins (see is_stateless_shared_plugin in registry.rs), where a single
/// pool of HostStates is reused across every project. `siblings()`/
/// `set_siblings()` expose the CURRENT calling project's sibling pool map,
/// refreshed fresh on every dispatch by `registry::dispatch_on` (mirroring
/// `cwd`/`set_cwd`), so `host_plugin_call` on any plugin instance can look up
/// any other already-loaded plugin FOR THE PROJECT THAT IS CURRENTLY
/// DISPATCHING, never whichever project happened to instantiate this Store
/// first. Each sibling entry owns its OWN Store (see SiblingHandle) so
/// cross-plugin calls never reuse the calling plugin's Store.
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
    // Mutex around the Arc itself (not just the inner HashMap), mirroring
    // `cwd` above: a SHARED plugin instance (see is_stateless_shared_plugin
    // in registry.rs) reuses this same HostState across every project's
    // dispatch, so the map this points at must be swappable per-call, not
    // fixed at whichever project's `load_plugin` happened to instantiate
    // this Store first. Before this fix the Arc itself never changed after
    // construction, so `host_plugin_call` on a shared instance always
    // resolved siblings against the FIRST project to instantiate it --
    // correct by accident only because every default-config project loads
    // the same 4 plugins in the same order. `registry::dispatch_on` calls
    // `set_siblings` fresh on every dispatch, exactly like `set_cwd`.
    siblings: Mutex<Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>>,
    pub wasi: WasiP1Ctx,
}

impl HostState {
    pub fn new(cwd: PathBuf, plugin_name: String) -> Self {
        let mut builder = WasiCtxBuilder::new();
        builder.inherit_stderr();
        // Preopened at whichever project instantiates this Store FIRST --
        // fixed for the Store's lifetime, unlike `cwd` below. Safe for
        // gm/bert/treesitter because all three do their real file I/O
        // through the host_fs_* imports (which consult the mutable `cwd`
        // field fresh per call), never raw WASI filesystem syscalls.
        //
        // "libsql" is the one exception, handled by the caller via
        // `new_with_fs_roots` below instead of this constructor: it's built
        // with libsql-ffi's `wasm32-wasi-vfs` feature, so sqlite3_open_v2
        // resolves paths through REAL WASI path_open syscalls against
        // whatever got preopened here -- a single project-cwd preopen fixed
        // at first instantiation would silently misdirect every OTHER
        // project's absolute db path once libsql became a shared instance
        // (rc=14 "unable to open database file": the absolute path is
        // correct, but WASI has nothing preopened that covers it).
        if let Err(e) = builder.preopened_dir(&cwd, ".", DirPerms::all(), FilePerms::all()) {
            eprintln!(
                "[agentplug] WARNING: failed to preopen {} for WASI ({}): {e}",
                cwd.display(),
                plugin_name
            );
        }
        let wasi = builder.build_p1();
        Self {
            cwd: Mutex::new(cwd),
            plugin_name,
            self_instance: Arc::new(Mutex::new(None)),
            siblings: Mutex::new(Arc::new(Mutex::new(HashMap::new()))),
            wasi,
        }
    }

    /// Same as `new`, but preopens the whole host filesystem root as WASI
    /// guest path "/" instead of a single project's cwd as ".". Required for
    /// a shared plugin instance (currently only "libsql") whose wasm module
    /// performs real WASI filesystem syscalls (libsql-ffi's
    /// `wasm32-wasi-vfs` feature routes sqlite3_open_v2 through actual
    /// wasi-libc path_open calls) against a path supplied fresh per call --
    /// wasi-libc's path resolution (`__wasilibc_find_relpath`) works by
    /// POSIX-`/`-prefix-matching the requested path against registered
    /// preopen guest paths, then opening the remainder relative to that
    /// preopen's fd; it has no concept of a Windows drive letter and a
    /// single project-cwd preopen fixed at first instantiation would
    /// silently misdirect every OTHER project's db path once libsql became
    /// shared (rc=14 CANTOPEN: WASI has nothing preopened whose guest-path
    /// prefix matches). Preopening the real filesystem root as guest "/"
    /// once, combined with the caller passing WASI-guest-relative POSIX
    /// paths (see `posix_guest_path` in registry.rs), covers every project
    /// without per-call preopen churn (which wasmtime-wasi does not support
    /// post-instantiation anyway).
    pub fn new_with_fs_root(cwd: PathBuf, plugin_name: String, fs_root: &std::path::Path) -> Self {
        let mut builder = WasiCtxBuilder::new();
        builder.inherit_stderr();
        if let Err(e) = builder.preopened_dir(fs_root, "/", DirPerms::all(), FilePerms::all()) {
            eprintln!("[agentplug] WARNING: failed to preopen fs root {} for WASI ({}): {e}", fs_root.display(), plugin_name);
        }
        let wasi = builder.build_p1();
        Self {
            cwd: Mutex::new(cwd),
            plugin_name,
            self_instance: Arc::new(Mutex::new(None)),
            siblings: Mutex::new(Arc::new(Mutex::new(HashMap::new()))),
            wasi,
        }
    }

    pub fn set_cwd(&self, cwd: PathBuf) {
        *self.cwd.lock().unwrap() = cwd;
    }

    pub fn set_siblings(&self, new: Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>) {
        *self.siblings.lock().unwrap() = new;
    }

    pub fn siblings(&self) -> Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>> {
        self.siblings.lock().unwrap().clone()
    }

    pub fn cwd(&self) -> PathBuf {
        self.cwd.lock().unwrap().clone()
    }
}
