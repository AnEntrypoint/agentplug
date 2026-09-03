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
    pub content_hash: String,
}

pub struct HostState {
    pub cwd: Mutex<PathBuf>,
    pub plugin_name: String,
    pub self_instance: Arc<Mutex<Option<wasmtime::Instance>>>,
    siblings: Mutex<Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>>,
    /// Set when a host response could not be handed to the guest -- the guest
    /// receives a zero packed value, which is indistinguishable from a genuine
    /// empty response unless the reason is recorded out of band. `dispatch_on`
    /// reads and clears it so a lost response fails loudly instead of arriving
    /// as a bodyless success.
    lost_response_reason: Mutex<Option<String>>,
    /// Additional filesystem roots the guest has explicitly named as a
    /// dispatch target this session (e.g. `codesearch {root: "<abs>"}`
    /// against a sibling repo). Narrower than a standing sandbox grant --
    /// the guest opts a specific root in by naming it in a real dispatch,
    /// and `host_fs_*` calls (see `sandboxed_guest_path`) check this list
    /// in addition to `cwd`/`~/.gm`, never widening beyond named roots.
    extra_readable_roots: Mutex<Vec<PathBuf>>,
    /// The current dispatch's own call deadline, in seconds (whatever
    /// `dispatch_on` resolved via `deadline_secs_for_call` -- the default or a
    /// caller-supplied override). `write_guest_bytes` narrows the epoch
    /// deadline to a short response-handoff grace window on every host->guest
    /// response, guest-visible or not (a call with several sequential host
    /// calls -- e.g. memorize-fire's embed, then its dedup-check read, then
    /// its md/vector writes -- crosses that narrow window's own boundary many
    /// times per dispatch); after the handoff, the deadline must be restored
    /// to this value so the rest of the guest's execution keeps its real,
    /// intended budget instead of running out the dispatch under a 5s window.
    call_deadline_secs: Mutex<u64>,
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
            lost_response_reason: Mutex::new(None),
            extra_readable_roots: Mutex::new(Vec::new()),
            call_deadline_secs: Mutex::new(crate::registry::DISPATCH_CALL_DEADLINE_SECS),
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
            lost_response_reason: Mutex::new(None),
            extra_readable_roots: Mutex::new(Vec::new()),
            call_deadline_secs: Mutex::new(crate::registry::DISPATCH_CALL_DEADLINE_SECS),
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

    pub fn set_call_deadline_secs(&self, secs: u64) {
        *self.call_deadline_secs.lock().unwrap_or_else(|e| e.into_inner()) = secs;
    }

    pub fn call_deadline_secs(&self) -> u64 {
        *self.call_deadline_secs.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn note_lost_response(&self, reason: String) {
        *self.lost_response_reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
    }

    pub fn take_lost_response(&self) -> Option<String> {
        self.lost_response_reason.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Names an additional filesystem root the guest may read from for the
    /// remainder of this session -- validated by the caller (`host_fs_allow_root`)
    /// to be a real, existing directory before it lands here. Idempotent: naming
    /// the same root twice is a no-op, not a growing list.
    pub fn allow_extra_root(&self, root: PathBuf) {
        let mut roots = self.extra_readable_roots.lock().unwrap_or_else(|e| e.into_inner());
        if !roots.contains(&root) {
            roots.push(root);
        }
    }

    pub fn extra_readable_roots(&self) -> Vec<PathBuf> {
        self.extra_readable_roots.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
