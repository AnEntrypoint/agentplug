use std::path::PathBuf;

pub fn install_dir() -> PathBuf {
    if let Ok(over) = std::env::var("AGENTPLUG_HOME") {
        if !over.trim().is_empty() {
            return PathBuf::from(over);
        }
    }
    let base = directories::BaseDirs::new().expect("no home directory resolvable on this platform");
    base.home_dir().join(".agentplug")
}

pub fn legacy_wasmtime_cache_dir() -> PathBuf {
    install_dir().join("wasmtime-cache")
}

pub fn precompiled_dir() -> PathBuf {
    install_dir().join("precompiled")
}

pub fn plugins_dir() -> PathBuf {
    install_dir().join("plugins")
}
