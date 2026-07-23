mod browser;
mod exec_js;
mod host_state;
mod task;
mod imports;
mod install;
mod registry;

pub use browser::close_all_sessions;
pub use host_state::HostState;
pub use imports::{register_env_imports, register_wasi};
pub use install::{install_dir, plugins_dir, wasmtime_cache_dir};
pub use registry::{
    epoch_ticks_for_seconds, read_project_plugin_list, refill_shared_plugin, release_shared_plugin, set_gm_pool_size, set_side_plugin_pool_size, DispatchHandle,
    GmFairnessGuard, ProjectPlugins, EPOCH_TICK_INTERVAL_MS, PLUGIN_IDLE_EVICT_MS,
};

use std::sync::OnceLock;
use wasmtime::{Cache, CacheConfig, Config, Engine};

static EPOCH_TICKER_STARTED: OnceLock<()> = OnceLock::new();

/// Starts the process-wide epoch ticker exactly once. `Engine::increment_epoch`
/// is what actually advances every Store's epoch-based deadline (set via
/// `Store::set_epoch_deadline` at instantiation, see registry.rs) -- without
/// this thread running, `epoch_interruption(true)` alone does nothing, since
/// nothing ever moves the epoch counter forward and no deadline is ever
/// reached. One ticker per process serves every Engine this process ever
/// builds, since `Engine::increment_epoch` operates on the specific Engine
/// instance it's called on; `build_engine` is called exactly once at daemon
/// startup in practice, so this ties the ticker to that one Engine.
fn start_epoch_ticker(engine: Engine) {
    if EPOCH_TICKER_STARTED.set(()).is_err() {
        return;
    }
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(EPOCH_TICK_INTERVAL_MS));
        engine.increment_epoch();
    });
}

pub fn build_engine() -> anyhow::Result<Engine> {
    let mut config = Config::new();
    let mut cache_config = CacheConfig::new();
    cache_config.with_directory(wasmtime_cache_dir());
    config.cache(Some(Cache::new(cache_config)?));
    // A guest-side Rust panic (unwrap/expect/index-out-of-bounds/etc) traps
    // as a bare wasmtime "unreachable" with ZERO diagnostic context unless
    // backtraces are explicitly enabled -- wasmtime's default is
    // environment-dependent (often off in release builds), which is exactly
    // why a real remote-user trap this session produced no actionable info
    // at all. wasm_backtrace_details(Enabled) makes every trap carry a real
    // guest-side stack frame (function name + wasm offset) instead of
    // nothing, at a small binary-size/compile-time cost that is worth it
    // for a tool whose whole job is to be debuggable when it breaks.
    config.wasm_backtrace_details(wasmtime::WasmBacktraceDetails::Enable);
    // Bounds a raw synchronous call into guest wasm (registry.rs's
    // dispatch_on, e.g. bert's forward-pass or libsql's query execution) so a
    // stuck/looping guest traps cleanly instead of hanging forever. Combined
    // with Store::set_epoch_deadline (registry.rs's instantiate_plugin) and
    // the background ticker below -- without all three pieces this is inert.
    // This closes the gap the pool-acquire timeout (SharedPluginPool::acquire)
    // does NOT cover: acquire only bounds the WAIT to get a slot; once a
    // caller has the slot and is inside dispatch_fn.call, nothing previously
    // could interrupt that call itself, and a single stuck call on a
    // process-wide pool-size-1 shared plugin (bert/libsql) wedged every
    // project's dispatch into that plugin forever.
    config.epoch_interruption(true);
    let engine = Engine::new(&config).map_err(|e| anyhow::anyhow!(e))?;
    start_epoch_ticker(engine.clone());
    Ok(engine)
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
