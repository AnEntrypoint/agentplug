mod broker;
mod browser;
mod browser_engine;
mod exec_js;
mod host_state;
mod http_agent;
mod memory_pressure;
mod oxibrowser_driver;
mod precompiled;
mod subprocess;
mod task;
mod imports;
mod install;
mod registry;

pub use broker::{
    begin_rolling_update, reap_drained, register_provider, register_provider_with_weight, route, set_policy, shift_traffic, status as broker_status,
    unregister_provider, BrokerStatus, LoadBalancePolicy, ProviderStatus, RouteLease,
};
pub use browser::{close_all_sessions, reap_idle_sessions_and_os_orphans_across_every_known_project_root, run as browser_run};
pub use host_state::HostState;
pub use http_agent::{build_agent, shared_agent};
pub use imports::{git_subprocess_timeout_ms, register_env_imports, register_wasi};
pub use install::{install_dir, plugins_dir, precompiled_dir};
pub use memory_pressure::{
    process_memory_breakdown, process_private_bytes_tracking_retained_wasm_peak_unlike_working_set, reset_shared_dispatch_count, shared_dispatches_since_release,
    ProcessMemoryBreakdown,
};
pub use precompiled::{load_module_file_backed, precompiled_module_path};
pub use registry::{
    advance_plugin_fiber, ensure_sibling_loaded, epoch_ticks_for_seconds, forget_store_bytes_for_root, forget_store_bytes_for_shared_plugin, get_active_provider,
    read_plugin_lifecycle, read_project_plugin_list, release_shared_plugin, set_gm_pool_size, set_lazy_module_source, set_side_plugin_pool_size,
    set_store_linear_memory_ceilings, store_bytes_by_plugin, RELEASABLE_SHARED_PLUGINS,
    note_shared_plugin_bytes_current, request_shared_store_swap, shared_plugin_slot_content_hashes, shared_plugin_swap_pending_hashes,
    DispatchHandle, GmFairnessGuard, PluginFiberLifecycle, ProjectPlugins, StoreBytesSummary, EPOCH_TICK_INTERVAL_MS, PLUGIN_IDLE_EVICT_MS,
};

use std::sync::OnceLock;
use wasmtime::{Config, Engine};

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
