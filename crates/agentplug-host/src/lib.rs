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
    epoch_ticks_for_seconds, read_project_plugin_list, release_shared_plugin, set_gm_pool_size, set_side_plugin_pool_size, DispatchHandle,
    GmFairnessGuard, ProjectPlugins, EPOCH_TICK_INTERVAL_MS, PLUGIN_IDLE_EVICT_MS,
};

use std::sync::OnceLock;
use wasmtime::{Cache, CacheConfig, Config, Engine};

static EPOCH_TICKER_STARTED: OnceLock<()> = OnceLock::new();

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
    config.wasm_backtrace_details(wasmtime::WasmBacktraceDetails::Enable);
    config.epoch_interruption(true);
    let engine = Engine::new(&config).map_err(|e| anyhow::anyhow!(e))?;
    start_epoch_ticker(engine.clone());
    Ok(engine)
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
