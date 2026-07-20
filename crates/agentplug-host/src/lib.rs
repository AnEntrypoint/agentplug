mod browser;
mod exec_js;
mod host_state;
mod task;
mod imports;
mod install;
mod registry;

pub use host_state::HostState;
pub use imports::{register_env_imports, register_wasi};
pub use install::{install_dir, plugins_dir, wasmtime_cache_dir};
pub use registry::{read_project_plugin_list, release_shared_plugin, ProjectPlugins, PLUGIN_IDLE_EVICT_MS};

use wasmtime::{Cache, CacheConfig, Config, Engine};

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
    Engine::new(&config).map_err(|e| anyhow::anyhow!(e))
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
