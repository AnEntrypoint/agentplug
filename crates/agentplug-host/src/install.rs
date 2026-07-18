use std::path::PathBuf;

// Sibling install root to ~/.gm-tools -- agentplug is gm-agnostic, so it
// gets its own directory rather than reusing gm's. gm-plugkit's installer
// downloads agentplug-runner INTO ~/.gm-tools alongside its own gm.wasm
// plugin manifest, but agentplug's own cache/registry/status files live
// here regardless of which consumer (gm, or a future non-gm tool) invoked it.
pub fn install_dir() -> PathBuf {
    let base = directories::BaseDirs::new().expect("no home directory resolvable on this platform");
    base.home_dir().join(".agentplug")
}

pub fn wasmtime_cache_dir() -> PathBuf {
    install_dir().join("wasmtime-cache")
}

pub fn plugins_dir() -> PathBuf {
    install_dir().join("plugins")
}
