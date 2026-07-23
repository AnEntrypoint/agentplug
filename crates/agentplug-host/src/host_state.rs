use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use wasmtime::{Instance, Store};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

use crate::registry::SharedPluginPool;

pub struct SiblingHandle {
    pub store: Store<HostState>,
    pub instance: Instance,
}

pub struct HostState {
    pub cwd: Mutex<PathBuf>,
    pub plugin_name: String,
    pub self_instance: Arc<Mutex<Option<wasmtime::Instance>>>,
    siblings: Mutex<Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>>,
    pub wasi: WasiP1Ctx,
}

impl HostState {
    pub fn new(cwd: PathBuf, plugin_name: String) -> Self {
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
        Self {
            cwd: Mutex::new(cwd),
            plugin_name,
            self_instance: Arc::new(Mutex::new(None)),
            siblings: Mutex::new(Arc::new(Mutex::new(HashMap::new()))),
            wasi,
        }
    }

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
