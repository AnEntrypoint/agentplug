mod broker;
mod browser;
mod browser_engine;
mod dispatch_origin;
mod display;
mod exec_js;
mod fs_prewarm;
mod gpu;
mod host_state;
mod http_agent;
mod idle_reap;
mod imports;
mod install;
mod memory_pressure;
mod oxibrowser_driver;
mod precompiled;
mod process_tree;
mod registry;
mod task;
mod watcher_log;
mod windowless;

pub use broker::{
    begin_rolling_update, reap_drained, register_provider, register_provider_with_weight, route,
    set_policy, shift_traffic, status as broker_status, unregister_provider, BrokerStatus,
    LoadBalancePolicy, ProviderStatus, RouteLease,
};
pub use browser::{
    canonical_project_root, close_all_sessions, project_root,
    reap_idle_sessions_and_os_orphans_across_every_known_project_root, run as browser_run,
};
pub use dispatch_origin::{enter_dispatch_origin_scope, DispatchOriginScope};
pub use host_state::HostState;
pub use http_agent::{build_agent, shared_agent};
pub use imports::{
    git_subprocess_timeout_ms, github_cli_config_dir, register_env_imports, register_wasi,
    set_github_cli_config_dir,
};
pub use install::{install_dir, plugins_dir, precompiled_dir};
pub use memory_pressure::{
    process_private_bytes_tracking_retained_wasm_peak_unlike_working_set,
    reset_shared_dispatch_count, shared_dispatches_since_release,
};
pub use precompiled::{load_module_file_backed, precompiled_module_path};
pub use registry::{
    admission_wait_state_for_thread, advance_plugin_fiber, cost_class_for_dispatch,
    cost_class_for_verb, dispatch_serial_lane, epoch_ticks_for_seconds, get_active_provider,
    note_shared_plugin_bytes_current, read_plugin_lifecycle, read_project_plugin_list,
    release_shared_plugin, request_shared_store_swap, set_gm_pool_size, set_sibling_reload_source,
    set_side_plugin_pool_size, shared_plugin_slot_content_hashes,
    shared_plugin_slot_snapshot_without_blocking, shared_plugin_swap_pending_hashes,
    DispatchCostClass, DispatchHandle, GmFairnessGuard, LaneWaitReport, PluginDispatchError,
    PluginFiberLifecycle, ProjectPlugins, SharedPluginPool, SlotContentSnapshot, ToolDispatchGuard,
    ToolQueueWaitReport, EPOCH_TICK_INTERVAL_MS, PLUGIN_IDLE_EVICT_MS, RELEASABLE_SHARED_PLUGINS,
};
pub use watcher_log::{
    append_watcher_event, append_watcher_line, watcher_log_path, WATCHER_LOG_BACKUPS,
    WATCHER_LOG_MAX_BYTES,
};
pub use windowless::{apply_windowless, ensure_hidden_console};

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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
