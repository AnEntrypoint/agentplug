use std::path::PathBuf;

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
