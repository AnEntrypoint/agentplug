use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use wasmtime::{Engine, Linker, Module, Store};

use crate::host_state::{HostState, SiblingHandle};
use crate::imports::{register_env_imports, register_wasi};

pub const PLUGIN_IDLE_EVICT_MS: u64 = 30 * 60 * 1000;

/// How often the background thread in lib.rs's `start_epoch_ticker` calls
/// `Engine::increment_epoch`. Every guest-call deadline (see
/// `epoch_ticks_for_seconds`) is expressed in units of this interval, so
/// changing it changes what one "tick" means everywhere a deadline is armed.
pub const EPOCH_TICK_INTERVAL_MS: u64 = 1_000;

/// Ticks needed to cover `secs` wall-clock seconds, given the ticker cadence
/// above -- rounds up so a caller asking for e.g. 30s never gets a deadline
/// that fires early due to integer truncation.
pub fn epoch_ticks_for_seconds(secs: u64) -> u64 {
    (secs * 1000).div_ceil(EPOCH_TICK_INTERVAL_MS)
}

/// Deadline for one `dispatch_on` guest call (see its `set_epoch_deadline`
/// call below) -- deliberately above `SharedPluginPool::ACQUIRE_TIMEOUT_MS`
/// (20s) and `host_git`'s `GIT_SUBPROCESS_TIMEOUT_MS` (15s), the two other
/// bounds in this hang class, so a call that legitimately needed the full
/// pool-acquire wait still has room to run before this bound also fires.
pub const DISPATCH_CALL_DEADLINE_SECS: u64 = 40;

/// Plugins with no per-project state (pure function of input -> output,
/// nothing keyed by project root) get ONE process-wide instance shared by
/// every project instead of one instantiation per project.
///
/// "bert": ~133MB embedding model `include_bytes!`'d into the wasm module,
/// deserialized into live tensors on first `embed` call (candle's
/// VarBuilder::from_slice_safetensors) -- with N concurrently active
/// projects, per-project instantiation held N separate deserialized copies
/// resident at once (live-witnessed: 2 active projects, 2.3GB daemon RSS
/// before this fix vs 937MB after).
///
/// "treesitter": confirmed zero static/OnceLock/Mutex state in its source
/// (grep across agentplug-treesitter/src/*.rs) -- a pure parse function,
/// genuinely stateless already.
///
/// "libsql": DBS/PREPARED maps are keyed by the caller-supplied db `name`
/// string, not by project, and `open(name, path)` takes an explicit absolute
/// `path` argument the CALLER resolves (never reads HostState.cwd internally)
/// -- two projects opening different db files under different names/paths
/// share one instance safely with zero cross-project collision, same as any
/// ordinary connection-pool keyed by a unique identifier. Its Store is
/// instantiated via `HostState::new_with_fs_root` (not the plain `new` every
/// other shared plugin uses) -- libsql-ffi's `wasm32-wasi-vfs` feature makes
/// sqlite3_open_v2 issue REAL WASI path_open syscalls, so unlike bert/
/// treesitter/gm (which only ever touch files through the host_fs_* imports
/// that consult HostState.cwd fresh per call) a single project-cwd preopen
/// fixed at first instantiation would silently CANTOPEN every other
/// project's absolute db path. See that constructor's doc comment.
///
/// "gm": genuinely stateless the same way -- its real state lives in flat
/// files under `<project>/.gm/` (prd.yml, mutables.yml, exec-spool/), never
/// in wasm-side memory, so sharing one instance across projects is correct
/// by the same rule as bert/treesitter/libsql. Previously the sole holdout
/// -- gm.wasm's own orchestrator/mod.rs used to bake its project root into
/// a wasm-side `PROJECT_ROOT: OnceLock<PathBuf>`, resolved once via a
/// `git rev-parse` subprocess on first `gm_dir()` call and cached for that
/// instance's lifetime, which would have silently misdirected a second
/// project's work onto the first project's `.gm/` directory if shared.
/// Fixed in rs-plugkit (commit a0ddeb6): `gm_dir()` now calls
/// `resolve_project_root_with_retry()` fresh on every invocation instead
/// of caching, routed through the new `host_cwd` import this host now
/// exposes (see imports.rs) -- correct per-call regardless of which
/// project's dispatch is currently running. libsql's own db-path
/// caller-side threading (rs-plugkit commit 30562b1) was the second half
/// of this same fix: gm.wasm's internal libsql calls now forward a real
/// absolute path derived from host_cwd on every call instead of a bare
/// project-ambiguous name, since libsql itself is now ALSO a shared,
/// per-call-stateless instance (see "libsql" above and
/// agentplug-libsql's own db.rs).
///
/// Sharing is correct; it is NOT free concurrency by itself. Each acquired
/// slot's Mutex is held for the full duration of the wasm call, so a long
/// synchronous `exec_js`/`browser` dispatch against ANY project serializes
/// every OTHER project's `gm` dispatch behind it FOR THAT SLOT, for the same
/// duration -- unavoidable, since the wasm guest call itself (not some
/// separable I/O step around it) is the blocking work; there is no "release
/// the lock around blocking I/O" seam to open inside a synchronous guest
/// call. What removes the liveness gap is having enough slots that a live
/// call never needs to wait for someone else's slot: `SharedPluginPool` now
/// runs `gm_pool_size()` (default 4, matching `max_concurrent_projects`,
/// raisable via `~/.agentplug/daemon-config.json`'s `gm_concurrency`, wired
/// at daemon startup by `set_gm_pool_size`) concurrent slots for `gm`
/// specifically, instead of one -- as many worker threads as there are slots
/// can be genuinely mid-`gm`-call at once with zero queuing between them
/// (live-witnessed, this session: a `phase-status` on one project's queue
/// resolved in ~127ms while a concurrently-submitted `exec_js` was in-flight
/// on the same project). The residual bound is explicit, not silently
/// eliminated: a caller past the `gm_pool_size()`th concurrent long dispatch
/// still round-robins onto a busy slot and queues behind it for that slot's
/// full remaining wall-clock duration (bounded at 20s by
/// `SharedPluginPool::acquire`'s `ACQUIRE_TIMEOUT_MS` before it surfaces a
/// typed "pool busy" error instead of hanging) -- raise `gm_concurrency` to
/// widen that ceiling for a workload that regularly exceeds it. Reverting
/// `gm` to per-project instances is still the wrong direction regardless:
/// that would reintroduce N-times state duplication for a plugin whose
/// state is supposed to live in flat files, not wasm memory.
fn is_stateless_shared_plugin(plugin_name: &str) -> bool {
    matches!(plugin_name, "bert" | "treesitter" | "libsql" | "gm")
}

/// Process-wide ceiling on how many live `gm` Stores may exist at once.
/// Set exactly once, early in daemon startup (`set_gm_pool_size`), from
/// `DaemonConfig::gm_concurrency()` -- before that call, or on a non-daemon
/// entry point that never calls it, `gm_pool_size()` falls back to 4 (the
/// pre-existing effective concurrency ceiling, matching the old
/// `MAX_CONCURRENT_PROJECTS` default). bert/treesitter/libsql default to
/// exactly one instance each (see SIDE_PLUGIN_POOL_SIZE below for why, and
/// how to raise it) -- because bert alone costs ~133MB of resident tensors
/// per instance and no live contention was ever found for any of the three
/// under this host's own workloads; only `gm`'s exec_js/browser dispatches
/// are long enough to block unrelated projects behind a single shared Mutex
/// (see the doc comment above and the 18-21s live-witnessed stall).
static GM_POOL_SIZE: OnceLock<usize> = OnceLock::new();

/// Configure the `gm` pool size. Must be called before the first `gm`
/// dispatch to take effect (a `OnceLock` can only be set once); a call after
/// the pool already lazily-initialized at the default is a no-op returning
/// false. Intended call site: once, at daemon startup, right after
/// `DaemonConfig::load()`.
pub fn set_gm_pool_size(n: usize) -> bool {
    GM_POOL_SIZE.set(n.max(1)).is_ok()
}

fn gm_pool_size() -> usize {
    *GM_POOL_SIZE.get_or_init(|| 4)
}

/// Process-wide ceiling on how many live Stores exist for each of
/// bert/treesitter/libsql (the non-"gm" stateless-shared plugins). Unlike
/// `gm`, these have never had a live-witnessed contention incident on this
/// host's own workloads (see the doc comment above `is_stateless_shared_plugin`),
/// so the default stays 1 -- unlike bumping the default itself, this is a
/// genuine escape hatch: a deployment that DOES see real cross-project
/// `codesearch`/`recall`/`embed` contention (host_plugin_call/host_vec_embed
/// serialize behind a size-1 pool, bounded by SharedPluginPool::acquire's
/// 20s ACQUIRE_TIMEOUT_MS before surfacing `plugin_pool_busy_timeout`) can
/// raise it without a code change, at the documented cost of N times bert's
/// ~133MB resident tensors per extra slot. Configured the same way as
/// GM_POOL_SIZE: set once at daemon startup from DaemonConfig, falls back to
/// 1 (byte-identical to the pre-existing hardcoded behavior) if never set.
static SIDE_PLUGIN_POOL_SIZE: OnceLock<usize> = OnceLock::new();

/// Configure the bert/treesitter/libsql pool size. Same one-shot-before-
/// first-use contract as `set_gm_pool_size`. Intended call site: once, at
/// daemon startup, right after `DaemonConfig::load()`.
pub fn set_side_plugin_pool_size(n: usize) -> bool {
    SIDE_PLUGIN_POOL_SIZE.set(n.max(1)).is_ok()
}

fn side_plugin_pool_size() -> usize {
    *SIDE_PLUGIN_POOL_SIZE.get_or_init(|| 1)
}

/// A shared plugin's live instance slots. Most plugins (bert/treesitter/
/// libsql) run exactly one slot; `gm` runs `gm_pool_size()` slots so up to
/// that many exec_js/browser (or any other verb) dispatches can be
/// genuinely concurrent instead of collapsing to serial execution behind
/// one Mutex. Each slot lazily instantiates its own Store on first real use,
/// same as the pre-existing single-slot design.
pub struct SharedPluginPool {
    slots: Vec<Arc<Mutex<Option<SiblingHandle>>>>,
}

impl SharedPluginPool {
    pub fn new(size: usize) -> Self {
        Self { slots: (0..size.max(1)).map(|_| Arc::new(Mutex::new(None))).collect() }
    }

    /// Picks a free slot (a `try_lock`-scan round-robin), falling back to a
    /// BOUNDED poll on slot 0 if every slot is currently busy -- under
    /// ordinary contention this serializes exactly like the old
    /// unconditional-blocking-lock design, so the common case is unchanged.
    /// Returns `None` only if slot 0 is still held after
    /// `ACQUIRE_TIMEOUT_MS`, i.e. genuinely wedged (a stuck host_git/
    /// host_plugin_call/candle forward-pass with no internal timeout of its
    /// own -- see host_git's GIT_SUBPROCESS_TIMEOUT_MS doc comment for the
    /// class of bug this backstops). Before this bound existed, a single
    /// wedged call on a size-1 shared pool (bert/libsql: exactly one slot,
    /// process-wide, serving every project) hung every OTHER project's call
    /// into that same plugin forever, with no recovery short of killing the
    /// whole daemon process -- live-reproduced against codesearch's
    /// code_index::index() -> embed_texts_batch -> host_plugin_call("bert")
    /// path. Returning `None` on timeout lets callers (host_plugin_call,
    /// host_vec_embed) surface a typed "pool busy" error instead of hanging,
    /// exactly the same shape they already use for "plugin_not_loaded_yet".
    pub fn acquire(&self) -> Option<std::sync::MutexGuard<'_, Option<SiblingHandle>>> {
        for slot in &self.slots {
            if let Ok(guard) = slot.try_lock() {
                return Some(guard);
            }
        }
        const ACQUIRE_TIMEOUT_MS: u64 = 20_000;
        const POLL_INTERVAL_MS: u64 = 25;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ACQUIRE_TIMEOUT_MS);
        loop {
            if let Ok(guard) = self.slots[0].try_lock() {
                return Some(guard);
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

    fn all_instantiated(&self) -> bool {
        // try_lock: a slot busy mid-dispatch is by definition instantiated.
        self.slots.iter().all(|s| match s.try_lock() {
            Ok(g) => g.is_some(),
            Err(_) => true,
        })
    }

    /// The raw slots, for fill-every-empty-slot loads (see `load_plugin` and
    /// `refill_shared_plugin`).
    pub(crate) fn slots_for_fill(&self) -> &[Arc<Mutex<Option<SiblingHandle>>>] {
        &self.slots
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

/// Drops a shared plugin's Store, returning its committed wasm linear memory
/// to the OS. `load_plugin` re-instantiates any shared cell holding `None` on
/// the next dispatch that needs it, so this is a release, not a teardown --
/// the only cost is one re-instantiation (cheap: the Module stays compiled and
/// cached in the Engine, only the Store/Instance are rebuilt).
///
/// This exists because a wasm linear memory only ever grows: `memory.grow`
/// commits pages and there is no shrink instruction, so a plugin that spikes
/// during one workload holds that peak for the lifetime of its Store. Measured
/// on this host: a fresh daemon sits at 350MB committed, 538MB with all four
/// plugins instantiated, and 1545MB after a single full-repo codeinsight pass
/// (114 files / ~697 chunks through bert) -- a +1006MB step that never returns
/// while the Store lives. VirtualQueryEx attributes it to one contiguous
/// ~1285MB PAGE_READWRITE private region, i.e. bert's own grown linear memory.
///
/// Note the trap this measurement corrects: WorkingSet64 is NOT the number to
/// judge this by. Windows trims a committed-but-cold working set aggressively
/// (a forced EmptyWorkingSet took WorkingSet from 1545.8MB to 1.0MB while
/// PrivateMemorySize64 held flat at 1546.6MB), which reads as an
/// accumulate-then-release sawtooth and hides the fact that nothing was ever
/// actually freed. Always judge this by PrivateMemorySize64.
pub fn release_shared_plugin(plugin_name: &str) -> bool {
    if !is_stateless_shared_plugin(plugin_name) {
        return false;
    }
    shared_plugin_pool(plugin_name).release_all()
}

/// Re-instantiates every EMPTY slot of a shared plugin's pool from `module`.
/// The plugin-auto-update swap (daemon: `modules.remove` +
/// `release_shared_plugin`) empties the whole pool, and `dispatch` hard-errors
/// on an empty slot with no re-instantiation path of its own — so without an
/// immediate refill every verb fails "plugin X not loaded" until some project
/// happens to re-register (and before the fill-all fix in `load_plugin`, even
/// THAT only refilled one slot). The instantiation `root` is non-binding for
/// shared plugins: `dispatch_on` refreshes the Store's cwd/siblings to the
/// CALLING project before every call.
pub fn refill_shared_plugin(engine: &Engine, plugin_name: &str, module: &Module, root: &Path) -> anyhow::Result<usize> {
    if !is_stateless_shared_plugin(plugin_name) {
        return Ok(0);
    }
    let pool = shared_plugin_pool(plugin_name);
    let mut filled = 0usize;
    for slot in pool.slots_for_fill() {
        if let Ok(mut guard) = slot.try_lock() {
            if guard.is_none() {
                *guard = Some(instantiate_plugin(engine, root.to_path_buf(), plugin_name, module)?);
                filled += 1;
            }
        }
    }
    Ok(filled)
}

/// Runs a verb dispatch against an already-instantiated plugin's OWN Store.
/// Shared by `ProjectPlugins::dispatch` (top-level spool dispatch) and
/// `host_plugin_call` (cross-plugin dispatch) so both go through the exact
/// same store -- never a different plugin's Caller/Store, which is what
/// produced wasmtime's "object used with the wrong store" panic
/// (store/data.rs:213) the first time host_plugin_call tried to drive a
/// sibling Instance using the calling plugin's Caller.
pub fn dispatch_on(
    store: &mut Store<HostState>,
    instance: wasmtime::Instance,
    verb: &str,
    body: &str,
    caller_root: &Path,
    caller_siblings: Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>,
) -> anyhow::Result<String> {
    // A shared instance (see is_stateless_shared_plugin) reuses the same
    // HostState across every project's dispatch -- refresh cwd to the
    // CALLING project's root immediately before every dispatch so
    // host_cwd/host_fs_*/host_kv_* all resolve against the right project,
    // never whichever project happened to instantiate this Store first.
    // Cheap and correct for a per-project (non-shared) instance too, since
    // caller_root is always that instance's own root in that case.
    store.data().set_cwd(caller_root.to_path_buf());
    // Same fix, same reason, for `siblings`: a shared instance's HostState
    // must point at the CALLING project's own siblings map for THIS call,
    // never whichever project's map got baked in at first instantiation
    // (see is_stateless_shared_plugin's / HostState's doc comments for the
    // custom-plugins.txt-subset bug this closes).
    store.data().set_siblings(caller_siblings);
    let plugin_name = store.data().plugin_name.clone();
    // A deadline set once at instantiation does NOT refill itself -- it is a
    // fixed epoch value computed from current+delta at set-time, and once
    // exceeded, every future epoch-instrumented call on this Store traps
    // immediately unless re-armed (wasmtime-46 store.rs `set_epoch_deadline`
    // doc comment). This Store is reused across many calls (SharedPluginPool
    // slots persist across dispatches for bert/libsql/gm/treesitter), so the
    // deadline must be re-armed here, before ANY epoch-instrumented call this
    // function makes -- including the plugkit_alloc calls below, which are
    // themselves guest wasm exports and therefore also epoch-checked. Arming
    // only right before dispatch_fn.call left those earlier alloc calls
    // running against whatever deadline the PREVIOUS dispatch on this same
    // Store left behind, which could already be exceeded -- live-witnessed
    // as a `plugkit_alloc call trapped` panic surfacing from write_guest_bytes
    // (a different call site reusing the same Store) after a prior call on
    // this slot had genuinely exceeded its deadline.
    store.set_epoch_deadline(epoch_ticks_for_seconds(DISPATCH_CALL_DEADLINE_SECS));
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

/// Host filesystem root covering every project this host can ever preopen
/// for a shared plugin doing real WASI filesystem syscalls (see
/// HostState::new_with_fs_root's doc comment). All projects driven by this
/// host live under a single drive in practice (Windows: the drive letter of
/// `std::env::current_dir()`, e.g. `C:\`; Unix: `/`), so preopening that one
/// root as WASI guest path "/" covers every caller's absolute db path.
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

    // `siblings` is NOT baked in here -- see Bug 2 in the module-level doc
    // comment above and HostState::set_siblings. It starts as an empty map
    // and gets pointed at the CALLING project's real siblings map fresh on
    // every dispatch_on call, exactly like `cwd`.
    let host_state = if plugin_name == "libsql" {
        HostState::new_with_fs_root(root, plugin_name.to_string(), &host_fs_root())
    } else {
        HostState::new(root, plugin_name.to_string())
    };
    let self_instance_cell = host_state.self_instance.clone();
    let mut store = Store::new(engine, host_state);
    // wasmtime's default epoch deadline is 0, i.e. "already elapsed" -- a
    // Store with epoch_interruption enabled (see build_engine) that never
    // calls set_epoch_deadline traps on its very first guest call. Arming a
    // real deadline here covers instantiate-time work (e.g. any eager guest
    // init) and any call path that reaches this Store before dispatch_on's
    // own per-call re-arm runs; dispatch_on re-arms fresh before every
    // subsequent call since this one-time arm does not refill itself.
    store.set_epoch_deadline(epoch_ticks_for_seconds(DISPATCH_CALL_DEADLINE_SECS));
    let instance = linker.instantiate(&mut store, module)?;
    *self_instance_cell.lock().unwrap() = Some(instance);
    Ok(SiblingHandle { store, instance })
}

/// Every plugin instance loaded for one project. `siblings` is the shared
/// name->pool map every plugin's HostState is pointed at (freshly, per
/// call -- see `dispatch_on`), so any plugin's `host_plugin_call` can reach
/// any other already-loaded plugin for this SAME project -- the mediator:
/// agentplug-runner owns this map, plugins never see each other directly.
/// Every entry (shared or per-project) is a `SharedPluginPool` -- shared
/// plugins (bert/treesitter/libsql/gm) use the process-wide pool from
/// `shared_plugin_pool`; a genuinely per-project plugin gets its own
/// size-1 pool, so both cases share one storage/lookup shape. Each
/// SiblingHandle owns its OWN Store, so `ProjectPlugins` itself no longer
/// keeps a separate stores/instances map -- `siblings` IS the canonical
/// storage, avoiding two owners for one Store.
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
        // ALL slots, not ANY: a partially-refilled shared pool (post
        // auto-update-swap, pre fill-all `load_plugin`) previously read as
        // "loaded" here, so the registration loop skipped the very
        // `load_plugin` call that would have healed the remaining empty
        // slots — leaving intermittent per-dispatch "plugin X not loaded"
        // (whichever concurrent verb drew an empty slot) with no recovery
        // short of killing the daemon.
        self.siblings.lock().unwrap().get(plugin_name).map(|p| p.all_instantiated()).unwrap_or(false)
    }

    /// Instantiates `module` under `plugin_name` for this project. Modules
    /// are compiled ONCE per plugin (shared `Module`, keyed by plugin name,
    /// owned by the caller -- typically agentplug-runner's global registry).
    ///
    /// Stateless plugins (see `is_stateless_shared_plugin`) get a
    /// process-wide pool reused by every project instead of one
    /// instantiation per project -- this project's siblings map just points
    /// at the same shared pool everyone else uses (`gm_pool_size()` slots
    /// for `gm`, exactly 1 for bert/treesitter/libsql). `dispatch`/
    /// `dispatch_on` refresh whichever slot's Store gets acquired to the
    /// calling project's own cwd + siblings map before every call, so a
    /// shared instance still resolves files/db paths/git ops/cross-plugin
    /// calls against the RIGHT project even though the Store itself is
    /// reused. Any future plugin that is NOT safely shareable this way
    /// keeps the original expensive-compile/cheap-instantiate-per-project
    /// split rs-plugkit's gm-runner daemon.rs already established (a size-1
    /// pool scoped to just this project).
    pub fn load_plugin(&mut self, engine: &Engine, plugin_name: &str, module: &Module) -> anyhow::Result<()> {
        if is_stateless_shared_plugin(plugin_name) {
            let pool = shared_plugin_pool(plugin_name);
            // Fill EVERY empty slot, not just the first one acquired. After a
            // plugin auto-update swap (daemon: modules.remove +
            // release_shared_plugin) the whole pool is None; `dispatch` has no
            // re-instantiation path (it hard-errors "plugin X not loaded" on
            // an empty slot), so a re-registering project that refilled only
            // ONE slot left every OTHER slot dead — live-witnessed as
            // intermittent per-dispatch "plugin gm not loaded" where exactly
            // 1 of N concurrent verbs succeeded (the one that happened to
            // acquire the single live slot), unrecoverable short of killing
            // the daemon. try_lock per slot: a slot busy mid-dispatch is by
            // definition instantiated, so skipping it is correct.
            for slot in pool.slots_for_fill() {
                if let Ok(mut guard) = slot.try_lock() {
                    if guard.is_none() {
                        *guard = Some(instantiate_plugin(engine, self.root.clone(), plugin_name, module)?);
                    }
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
        // A registered-but-momentarily-absent plugin pool (a concurrent loader thread
        // still mid-registration on the shared siblings map, or a transient eviction/
        // reload race) previously surfaced as a hard "plugin X not loaded" error with
        // zero recourse -- live-witnessed clearing on every bare caller-side retry this
        // session, the same shape as host_vec_embed's bert-pool-contention bug fixed
        // separately. Retry the pool lookup bounded before surfacing the error, same
        // pattern as that fix.
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
        let handle = guard.as_mut().ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} not loaded"))?;
        dispatch_on(&mut handle.store, handle.instance, verb, body, &self.root, self.siblings.clone())
    }

    /// A `Send + 'static` handle carrying exactly what a dispatch needs
    /// (root + the shared siblings map, both already `Arc`-backed) without
    /// borrowing `self` -- lets a caller spawn the actual `dispatch_on` call
    /// onto its own OS thread (see `background-convert` in
    /// agentplug-runner's daemon.rs) while this `ProjectPlugins` itself stays
    /// owned by the daemon's worker-pool bookkeeping and is free to be handed
    /// to a DIFFERENT thread for the project's next queued request. Calling
    /// `dispatch` through this handle is functionally identical to
    /// `ProjectPlugins::dispatch` (same pool acquire/dispatch_on path) --
    /// it just doesn't touch `last_active` itself, since the spawning caller
    /// already bumped it before detaching.
    pub fn dispatch_handle(&self) -> DispatchHandle {
        DispatchHandle { root: self.root.clone(), siblings: self.siblings.clone() }
    }
}

/// See `ProjectPlugins::dispatch_handle`. Cloning is cheap (two `Arc`
/// clones); every clone reaches the exact same underlying pool slots as the
/// `ProjectPlugins` it was taken from, so a dispatch run through a handle
/// participates in the same `GmFairnessGuard`/pool-slot accounting as one run
/// through `ProjectPlugins::dispatch` directly.
#[derive(Clone)]
pub struct DispatchHandle {
    root: PathBuf,
    siblings: Arc<Mutex<HashMap<String, Arc<SharedPluginPool>>>>,
}

impl DispatchHandle {
    pub fn dispatch(&self, plugin_name: &str, verb: &str, body: &str) -> anyhow::Result<String> {
        // Same bounded-retry rationale as ProjectPlugins::dispatch above.
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
        let handle = guard.as_mut().ok_or_else(|| anyhow::anyhow!("plugin {plugin_name} not loaded"))?;
        dispatch_on(&mut handle.store, handle.instance, verb, body, &self.root, self.siblings.clone())
    }
}

/// Per-project fairness cap on the shared `gm` pool, read fresh from
/// `<project>/.gm/daemon-project-config.json` on every dispatch -- same
/// precedent as `BrowserConfig::load(cwd)` in agentplug-host's browser.rs
/// (cheap file read, never cached, since the file can change between
/// dispatches and this is not a hot per-tick loop like the daemon's own
/// lifecycle timing).
///
/// This is deliberately NOT the same file as the machine-wide
/// `~/.agentplug/daemon-config.json` (`gm_concurrency` there sets the ACTUAL
/// total pool size, machine-scoped, one daemon process, ambiguous to
/// override per-project). This file can only ever LOWER one project's own
/// share of that shared pool -- it has no field capable of raising the
/// total, by construction (there is no "total slots" field here at all,
/// only a per-project ceiling on concurrent checkouts).
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

/// Process-wide in-flight `gm`-dispatch counter, keyed by project root.
/// Populated only when a project has actually configured its own
/// `gm_concurrency_limit` -- an unconfigured project never touches this map
/// at all (see `GmFairnessGuard::acquire`), so the common case (no
/// `.gm/daemon-project-config.json`) pays zero cost beyond the one cheap
/// file-read-that-fails in `ProjectDaemonConfig::load`.
static GM_INFLIGHT_BY_PROJECT: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

fn gm_inflight_map() -> &'static Mutex<HashMap<PathBuf, usize>> {
    GM_INFLIGHT_BY_PROJECT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// RAII guard for one project's fairness-limited `gm` dispatch slot.
/// Acquiring blocks (short poll loop) while this project is already at its
/// own configured `gm_concurrency_limit`; the slot is released on Drop so a
/// panic unwinding through `catch_unwind` in `dispatch_project` still
/// decrements the counter (Drop runs during unwind), never leaking a held
/// slot that would permanently wedge that project at its own cap.
pub struct GmFairnessGuard {
    root: PathBuf,
    limited: bool,
}

impl GmFairnessGuard {
    /// Blocks until this project's in-flight `gm` count is below its own
    /// configured `gm_concurrency_limit`, then reserves a slot and returns.
    /// A project with no limit configured (no file, or field absent) never
    /// enters the wait loop or touches the shared map at all -- byte-identical
    /// to pre-existing behavior, exactly the "default must not slow anything
    /// down" requirement this gate is built to.
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
