mod browser;
mod exec_js;
mod host_state;
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
    Engine::new(&config).map_err(|e| anyhow::anyhow!(e))
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
